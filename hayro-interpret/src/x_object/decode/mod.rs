mod image;
mod mask;

pub(crate) use image::{DecodedImage, decode_image, direct_invert, unpremultiply_samples};
pub(crate) use mask::{BILEVEL_MASK_LUT, DecodedMask, MaskPath, decode_mask, mask_lut};

use crate::InterpreterWarning;
use crate::color::ColorSpace;
use crate::function::interpolate;
use crate::x_object::image::{ImageKind, ImageXObject};
use hayro_syntax::Filter;
use hayro_syntax::bit_reader::BitReader;
use hayro_syntax::object::Array;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::stream::{
    DecodeFailure, FilterResult, ImageColorSpace, ImageData, ImageDecodeParams,
};
use hayro_syntax::work::{Meter, POLL_BYTES};
use smallvec::SmallVec;
use std::borrow::Cow;

struct DecodeContext<'a> {
    decoded: FilterResult<'a>,
    width: u32,
    height: u32,
    scale_factors: (f32, f32),
    color_space: ColorSpace,
    bits_per_component: u8,
    decode_arr: SmallVec<[(f32, f32); 4]>,
}

/// How an image's samples are laid out and what they mean: from its dictionary, and
/// the image data a filter reported (`decode_context`).
pub(crate) struct SampleFormat {
    pub(crate) color_space: ColorSpace,
    pub(crate) bits_per_component: u8,
    pub(crate) decode_arr: SmallVec<[(f32, f32); 4]>,
}

pub(crate) fn sample_format(
    obj: &ImageXObject<'_>,
    image_data: Option<&ImageData>,
) -> SampleFormat {
    sample_format_with(obj, image_data, dict_bpc(obj))
}

/// The parameters `obj`'s filters decode with, at its own size.
pub(crate) fn image_decode_params(obj: &ImageXObject<'_>) -> ImageDecodeParams {
    decode_params(obj, None, dict_bpc(obj))
}

/// The bits per component the image's dictionary gives.
fn dict_bpc(obj: &ImageXObject<'_>) -> Option<u8> {
    let dict = obj.stream.dict();
    dict.get::<u8>(BPC)
        .or_else(|| dict.get::<u8>(BITS_PER_COMPONENT))
}

/// [`sample_format`], with the bits per component of the dictionary already read.
fn sample_format_with(
    obj: &ImageXObject<'_>,
    image_data: Option<&ImageData>,
    dict_bpc: Option<u8>,
) -> SampleFormat {
    let dict = obj.stream.dict();
    let color_space = obj
        .color_space
        .clone()
        .or_else(|| {
            image_data.map(|i| i.color_space).and_then(|c| {
                c.and_then(|c| match c {
                    ImageColorSpace::Gray => Some(ColorSpace::device_gray()),
                    ImageColorSpace::Rgb => Some(ColorSpace::device_rgb()),
                    ImageColorSpace::Cmyk => Some(ColorSpace::device_cmyk()),
                    ImageColorSpace::Unknown(_) => None,
                })
            })
        })
        .unwrap_or(ColorSpace::device_gray());

    let bits_per_component = image_data
        .map(|i| i.bits_per_component)
        .or(dict_bpc)
        .unwrap_or_else(|| fallback_bpc(obj.kind));

    let decode_arr = dict
        .get::<Array<'_>>(D)
        .or_else(|| dict.get::<Array<'_>>(DECODE))
        .map(|a| {
            let mut pairs = a.flex_iter();
            core::iter::from_fn(|| pairs.next::<(f32, f32)>()).collect::<SmallVec<_>>()
        })
        .unwrap_or(color_space.default_decode_arr(bits_per_component as f32));

    SampleFormat {
        color_space,
        bits_per_component,
        decode_arr,
    }
}

/// The parameters `obj`'s filters decode with: where its dictionary gives no bits
/// per component or colour space, those its samples are read with when no filter
/// reports them ([`sample_format_with`]), so that the last filter knows their size.
/// A JPX image's bits per component stay the dictionary's: its decoder scales its
/// samples to the depth it is given and keeps their own without one.
fn decode_params(
    obj: &ImageXObject<'_>,
    target_dimension: Option<(u32, u32)>,
    dict_bpc: Option<u8>,
) -> ImageDecodeParams {
    let bpc = dict_bpc.or_else(|| {
        let filters = obj.stream.filter_entries();
        let jpx = filters
            .applied()
            .any(|(filter, _)| filter == Filter::JpxDecode);
        (!jpx).then(|| fallback_bpc(obj.kind))
    });
    let color_space = obj.color_space.as_ref();
    ImageDecodeParams {
        is_indexed: color_space.is_some_and(|cs| cs.is_indexed()),
        bpc,
        // Device gray's, as `sample_format_with` reads samples without a colour space.
        num_components: Some(color_space.map_or(1, |cs| cs.num_components())),
        target_dimension,
        width: obj.width(),
        height: obj.height(),
    }
}

/// The bits per component samples of an image of `kind` are read with where neither
/// its dictionary nor a filter gives them.
fn fallback_bpc(kind: ImageKind) -> u8 {
    if kind == ImageKind::StencilMask { 1 } else { 8 }
}

fn decode_context<'a>(
    obj: &ImageXObject<'a>,
    target_dimension: Option<(u32, u32)>,
    meter: &Meter<'_>,
) -> Option<DecodeContext<'a>> {
    let dict_bpc = dict_bpc(obj);
    let decode_params = decode_params(obj, target_dimension, dict_bpc);

    let decoded = obj
        .stream
        .decoded_image_metered(&decode_params, meter)
        .map_err(|e| {
            if !matches!(e, DecodeFailure::Cancelled) {
                (obj.warning_sink)(InterpreterWarning::ImageDecodeFailure);
            }
        })
        .ok()?;

    let (mut scale_x, mut scale_y) = (1.0, 1.0);

    let (width, height) = decoded
        .image_data
        .as_ref()
        .map(|d| {
            scale_x = obj.width() as f32 / d.width as f32;
            scale_y = obj.height() as f32 / d.height as f32;

            (d.width, d.height)
        })
        .unwrap_or((obj.width(), obj.height()));

    let SampleFormat {
        color_space,
        bits_per_component,
        decode_arr,
    } = sample_format_with(obj, decoded.image_data.as_ref(), dict_bpc);

    Some(DecodeContext {
        decoded,
        width,
        height,
        scale_factors: (scale_x, scale_y),
        color_space,
        bits_per_component,
        decode_arr,
    })
}

#[must_use]
fn fix_image_length<T: Copy>(
    image: &mut Vec<T>,
    width: u32,
    height: &mut u32,
    filler: T,
    num_components: usize,
    meter: &Meter<'_>,
) -> Option<()> {
    let row_len = (width as usize).saturating_mul(num_components);
    // Too much data (or just the right amount) is truncated; too little adapts the
    // height and pads the last row.
    *height = rows_present(image.len(), row_len, *height);
    let len = row_len * *height as usize;
    image.truncate(len);
    while image.len() < len {
        let n = (len - image.len()).min(POLL_BYTES);
        image.resize(image.len() + n, filler);
        meter.work(n).ok()?;
    }

    if width == 0 || *height == 0 {
        None
    } else {
        Some(())
    }
}

/// The pixels of `channels` bytes a piece of work takes: whole groups of 64, so that
/// a conversion that works on groups of pixels splits pieces as it splits the whole.
fn pixel_piece(channels: usize) -> usize {
    (POLL_BYTES / channels.max(1)).max(64) / 64 * 64
}

/// `f` applied to `data`, `channels`-byte pixels a piece at a time, each piece counted
/// on `meter`; `None` if `f` fails or the meter's hook asks to stop.
fn in_pieces(
    data: &mut [u8],
    channels: usize,
    meter: &Meter<'_>,
    mut f: impl FnMut(&mut [u8]) -> Option<()>,
) -> Option<()> {
    for piece in data.chunks_mut(pixel_piece(channels) * channels.max(1)) {
        f(piece)?;
        meter.work(piece.len()).ok()?;
    }
    Some(())
}

/// The bytes of `data`, owned, copied a counted piece at a time if borrowed.
fn into_owned(data: Cow<'_, [u8]>, meter: &Meter<'_>) -> Option<Vec<u8>> {
    match data {
        Cow::Owned(data) => Some(data),
        Cow::Borrowed(data) => {
            let mut owned = Vec::with_capacity(data.len());
            for piece in data.chunks(POLL_BYTES) {
                owned.extend_from_slice(piece);
                meter.work(piece.len()).ok()?;
            }
            Some(owned)
        }
    }
}

/// Every byte of `data` inverted, a counted piece at a time.
fn invert(data: &mut [u8], meter: &Meter<'_>) -> Option<()> {
    in_pieces(data, 1, meter, |piece| {
        for value in piece {
            *value = 255 - *value;
        }
        Some(())
    })
}

/// The rows of `row_len` samples, at most `height`, that `samples` decoded samples
/// fill. A last row they fill in part counts, to be padded, unless padding it would
/// take more than the samples present (and more than `MIN_ROW_PAD`): data cut short
/// costs memory in proportion to what it holds, never to what the image declares.
pub(crate) fn rows_present(samples: usize, row_len: usize, height: u32) -> u32 {
    if row_len == 0 {
        return 0;
    }
    let (full, part) = (samples / row_len, samples % row_len);
    let pad = row_len - part;
    let rows = if part > 0 && (pad <= samples || pad <= MIN_ROW_PAD) {
        full + 1
    } else {
        full
    };
    rows.min(height as usize) as u32
}

/// A short last row is padded up to this many samples even when the data before it
/// holds fewer.
const MIN_ROW_PAD: usize = 1 << 16;

/// What each stored sample value becomes: the decode array maps it onto the colour
/// space's range, scaled to a byte, or onto a palette index.
struct SampleMap<'a> {
    source_max: f32,
    decode: &'a [(f32, f32)],
    ranges: SmallVec<[(f32, f32); 4]>,
    indexed_hival: Option<u8>,
    num_components: usize,
}

impl<'a> SampleMap<'a> {
    fn new(color_space: &ColorSpace, bits_per_component: u8, decode: &'a [(f32, f32)]) -> Self {
        Self {
            source_max: 2.0_f32.powi(bits_per_component as i32) - 1.0,
            decode,
            ranges: color_space.component_ranges(),
            indexed_hival: color_space.indexed_hival(),
            num_components: color_space.num_components() as usize,
        }
    }

    /// The byte stored `value` of the sample at `index` (counted over components)
    /// becomes; `None` when the decode array has no entry for its component.
    fn map(&self, value: u32, index: usize) -> Option<u8> {
        let component_index = index % self.num_components;
        let (decode_min, decode_max) = *self.decode.get(component_index)?;
        let decoded = interpolate(value as f32, 0.0, self.source_max, decode_min, decode_max);

        if let Some(hival) = self.indexed_hival {
            Some((decoded + 0.5).clamp(0.0, hival as f32) as u8)
        } else {
            let (range_min, range_max) = *self.ranges.get(component_index)?;
            let normalized = if range_min == range_max {
                0.0
            } else {
                (decoded - range_min) / (range_max - range_min)
            };
            Some((normalized * 255.0 + 0.5) as u8)
        }
    }
}

/// For samples of at most eight bits, the byte each stored value becomes, per
/// component: `lut[c][v]` for value `v` of component `c`. `None` when the decode
/// array lacks a component, as decoding the samples whole then fails.
pub(crate) fn sample_lut(
    color_space: &ColorSpace,
    bits_per_component: u8,
    decode: &[(f32, f32)],
) -> Option<Vec<[u8; 256]>> {
    if !(1..=8).contains(&bits_per_component) {
        return None;
    }
    let map = SampleMap::new(color_space, bits_per_component, decode);
    (0..map.num_components)
        .map(|c| {
            let mut lut = [0; 256];
            for (v, out) in lut.iter_mut().enumerate().take(1 << bits_per_component) {
                *out = map.map(v as u32, c)?;
            }
            Some(lut)
        })
        .collect()
}

fn decode_u8_samples(
    data: &[u8],
    width: u32,
    height: u32,
    color_space: &ColorSpace,
    bits_per_component: u8,
    decode: &[(f32, f32)],
    meter: &Meter<'_>,
) -> Option<Vec<u8>> {
    let num_components = color_space.num_components() as usize;

    match bits_per_component {
        1..8 => {
            let lut = sample_lut(color_space, bits_per_component, decode)?;
            let rows = SampleRows::new(width, num_components, bits_per_component, height);
            let mut buf = Vec::with_capacity(rows.row_len * rows.rows_kept(data.len()) as usize);
            for_each_packed_row(data, rows, meter, |first, samples| {
                if let [lut] = &lut[..] {
                    buf.extend(samples.iter().map(|&v| lut[usize::from(v)]));
                } else {
                    let components = (first..).map(|i| &lut[i % num_components]);
                    buf.extend(
                        samples
                            .iter()
                            .zip(components)
                            .map(|(&v, lut)| lut[usize::from(v)]),
                    );
                }
            })?;
            Some(buf)
        }
        9..16 => {
            let map = SampleMap::new(color_space, bits_per_component, decode);
            let height = SampleRows::new(width, num_components, bits_per_component, height)
                .rows_kept(data.len());
            let mut buf = Vec::with_capacity(width as usize * num_components * height as usize);
            for_each_sample(
                data,
                width,
                height,
                num_components,
                bits_per_component,
                meter,
                |value, index| {
                    buf.push(map.map(value, index)?);
                    Some(())
                },
            )?;

            Some(buf)
        }
        8 | 16 => {
            let map = SampleMap::new(color_space, bits_per_component, decode);
            let mut buf = Vec::with_capacity(data.len() / (bits_per_component as usize / 8));
            for_each_whole_sample(data, bits_per_component, meter, |value, index| {
                buf.push(map.map(value, index)?);
                Some(())
            })?;
            Some(buf)
        }
        _ => {
            warn!("unsupported bits per component: {bits_per_component}");
            None
        }
    }
}

fn unpack_samples(
    data: &[u8],
    width: u32,
    height: u32,
    num_components: usize,
    bits_per_component: u8,
    meter: &Meter<'_>,
) -> Option<Vec<u16>> {
    match bits_per_component {
        1..8 => {
            let rows = SampleRows::new(width, num_components, bits_per_component, height);
            let mut buf = Vec::with_capacity(rows.row_len * rows.rows_kept(data.len()) as usize);
            for_each_packed_row(data, rows, meter, |_, samples| {
                buf.extend(samples.iter().map(|&v| u16::from(v)));
            })?;
            Some(buf)
        }
        9..16 => {
            let rows = SampleRows::new(width, num_components, bits_per_component, height);
            let height = rows.rows_kept(data.len());
            let mut buf = Vec::with_capacity(rows.row_len * height as usize);
            for_each_sample(
                data,
                width,
                height,
                num_components,
                bits_per_component,
                meter,
                |value, _| {
                    buf.push(value as u16);
                    Some(())
                },
            )?;

            Some(buf)
        }
        8 | 16 => {
            let mut buf = Vec::with_capacity(data.len() / (bits_per_component as usize / 8));
            for_each_whole_sample(data, bits_per_component, meter, |value, _| {
                buf.push(value as u16);
                Some(())
            })?;
            Some(buf)
        }
        _ => {
            warn!("unsupported bits per component: {bits_per_component}");
            None
        }
    }
}

/// How stored samples lie in an image's rows, each row starting on a byte: the one
/// source of the samples and rows that both decoding an image whole and streaming
/// it keep of data that ends early.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SampleRows {
    /// Samples per row, over components.
    pub(crate) row_len: usize,
    pub(crate) bits: u8,
    /// The rows the image declares.
    pub(crate) height: u32,
}

impl SampleRows {
    pub(crate) fn new(width: u32, components: usize, bits: u8, height: u32) -> Self {
        Self {
            row_len: (width as usize).saturating_mul(components),
            bits: bits.max(1),
            height,
        }
    }

    /// Stored bytes per row, at least one.
    pub(crate) fn row_bytes(&self) -> usize {
        self.row_len
            .saturating_mul(usize::from(self.bits))
            .div_ceil(8)
            .max(1)
    }

    /// The samples whose bits the first `bytes` stored bytes hold whole.
    pub(crate) fn samples_held(&self, bytes: usize) -> usize {
        let row_bytes = self.row_bytes();
        (bytes / row_bytes).saturating_mul(self.row_len)
            + (bytes % row_bytes * 8 / usize::from(self.bits)).min(self.row_len)
    }

    /// The rows kept of data of `bytes` stored bytes (see `rows_present`).
    pub(crate) fn rows_kept(&self, bytes: usize) -> u32 {
        rows_present(self.samples_held(bytes), self.row_len, self.height)
    }

    /// Whether the row data of `bytes` stored bytes ends within is kept, padded.
    pub(crate) fn keeps_row_cut_at(&self, bytes: usize) -> bool {
        self.rows_kept(bytes) as usize > bytes / self.row_bytes()
    }
}

/// The stored values of the samples of `bits` each (at most eight) that `bytes`
/// starts with, a byte each, into `out`: a sample whose bits `bytes` does not hold
/// whole, and every one after it, is `fill`.
pub(crate) fn unpack_packed(bits: u8, bytes: &[u8], out: &mut [u8], fill: u8) {
    let bits = usize::from(bits);
    let held = (bytes.len() * 8 / bits).min(out.len());
    let (out, rest) = out.split_at_mut(held);
    rest.fill(fill);
    match bits {
        8 => out.copy_from_slice(&bytes[..held]),
        1 => {
            let lut = &**BILEVEL_MASK_LUT;
            for (texels, &byte) in out.chunks_mut(8).zip(bytes) {
                let expanded = (lut[byte as usize] & 0x0101_0101_0101_0101).to_ne_bytes();
                texels.copy_from_slice(&expanded[..texels.len()]);
            }
        }
        _ => {
            let mask = (1_u16 << bits) - 1;
            for (i, sample) in out.iter_mut().enumerate() {
                let at = i * bits;
                let window = u16::from_be_bytes([
                    bytes[at / 8],
                    bytes.get(at / 8 + 1).copied().unwrap_or(0),
                ]);
                *sample = ((window >> (16 - bits - at % 8)) & mask) as u8;
            }
        }
    }
}

/// Visits the stored values of the rows `rows` keeps of `data`, unpacked
/// ([`unpack_packed`], those the data does not hold whole zero), a counted piece at
/// a time, each with the index in its row of its first sample.
fn for_each_packed_row(
    data: &[u8],
    rows: SampleRows,
    meter: &Meter<'_>,
    mut visit: impl FnMut(usize, &[u8]),
) -> Option<()> {
    let kept = rows.rows_kept(data.len()) as usize;
    if kept == 0 {
        return Some(());
    }
    let row_bytes = rows.row_bytes();
    let bits = usize::from(rows.bits);
    // Whole bytes of samples.
    let piece = rows.row_len.clamp(1, POLL_BYTES / 8 * 8);
    let mut samples = vec![0; piece];
    for r in 0..kept {
        let row = data.get(r * row_bytes..).unwrap_or_default();
        let row = &row[..row.len().min(row_bytes)];
        for first in (0..rows.row_len).step_by(piece) {
            let len = piece.min(rows.row_len - first);
            let start = (first * bits / 8).min(row.len());
            let end = ((first + len) * bits).div_ceil(8).min(row.len());
            let samples = &mut samples[..len];
            unpack_packed(rows.bits, &row[start..end], samples, 0);
            visit(first, samples);
            meter.work(len).ok()?;
        }
    }
    Some(())
}

fn for_each_sample(
    data: &[u8],
    width: u32,
    height: u32,
    num_components: usize,
    bits_per_component: u8,
    meter: &Meter<'_>,
    mut visit: impl FnMut(u32, usize) -> Option<()>,
) -> Option<()> {
    let mut reader = BitReader::new(data);
    let mut index = 0;
    // Samples since the meter last counted them.
    let mut uncounted = 0;

    for _ in 0..height {
        for _ in 0..width {
            for _ in 0..num_components {
                // Some images seemingly don't have enough data for their last row,
                // so we just pad it with zeroes.
                visit(reader.read(bits_per_component).unwrap_or(0), index)?;
                index += 1;
            }
            uncounted += num_components;
            if uncounted >= POLL_BYTES {
                meter.work(uncounted).ok()?;
                uncounted = 0;
            }
        }

        reader.align();
    }

    meter.work(uncounted).ok()
}

/// Visits the 8- or 16-bit samples of `data` with their index, a counted piece at a
/// time; a last odd byte of 16-bit samples is left out.
fn for_each_whole_sample(
    data: &[u8],
    bits_per_component: u8,
    meter: &Meter<'_>,
    mut visit: impl FnMut(u32, usize) -> Option<()>,
) -> Option<()> {
    let mut index = 0;
    // A piece holds whole 16-bit samples.
    for piece in data.chunks(POLL_BYTES) {
        if bits_per_component == 8 {
            for value in piece {
                visit(*value as u32, index)?;
                index += 1;
            }
        } else {
            for value in piece.as_chunks::<2>().0 {
                visit(u16::from_be_bytes([value[0], value[1]]) as u32, index)?;
                index += 1;
            }
        }
        meter.work(piece.len()).ok()?;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WarningSinkFn;
    use crate::cache::Cache;
    use crate::fixtures::{file, stream};
    use hayro_syntax::Pdf;
    use hayro_syntax::object::{ObjectIdentifier, Stream};
    use std::sync::Arc;

    /// The bits per component and components an image of `dict` decodes with.
    fn decoded_with(dict: &str) -> (Option<u8>, Option<u8>) {
        let objects = [
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            stream(
                &format!("/Type /XObject /Subtype /Image /Width 8 /Height 2 {dict}"),
                &[0; 2],
            ),
        ];
        let pdf = Pdf::new(file(&objects, "")).unwrap();
        let stream = pdf
            .xref()
            .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
            .unwrap();
        let sink: WarningSinkFn = Arc::new(|_| {});
        let obj = ImageXObject::new(&stream, |_| None, &sink, &Cache::new(), None).unwrap();
        let params = decode_params(&obj, None, dict_bpc(&obj));
        (params.bpc, params.num_components)
    }

    /// Flate data is told the size of the samples the image code reads, with the
    /// same defaults; a JPX image's depth is its own where its dictionary sets none.
    #[test]
    fn images_decode_with_the_layout_their_samples_are_read_with() {
        for (dict, expected) in [
            ("/ImageMask true", (Some(1), Some(1))),
            ("/ImageMask true /BitsPerComponent 1", (Some(1), Some(1))),
            ("/ColorSpace /DeviceRGB", (Some(8), Some(3))),
            ("/BitsPerComponent 4", (Some(4), Some(1))),
        ] {
            let dict = format!("{dict} /Filter /FlateDecode");
            assert_eq!(decoded_with(&dict), expected, "{dict}");
        }
        for (dict, expected) in [
            ("/ColorSpace /DeviceRGB", (None, Some(3))),
            ("/ImageMask true", (None, Some(1))),
            ("/BitsPerComponent 16", (Some(16), Some(1))),
        ] {
            let dict = format!("{dict} /Filter /JPXDecode");
            assert_eq!(decoded_with(&dict), expected, "{dict}");
        }
    }
}
