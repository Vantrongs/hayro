mod image;
mod mask;

pub(crate) use image::{DecodedImage, decode_image, direct_invert, unpremultiply_samples};
pub(crate) use mask::{BILEVEL_MASK_LUT, DecodedMask, MaskPath, decode_mask, mask_lut};

use crate::InterpreterWarning;
use crate::color::{ColorComponents, ColorSpace};
use crate::function::interpolate;
use crate::x_object::image::{ImageKind, ImageXObject};
use core::ops::Range;
use hayro_syntax::Filter;
use hayro_syntax::bit_reader::BitReader;
use hayro_syntax::object::Stream;
use hayro_syntax::object::array::Take;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::stream::{
    DecodeFailure, FilterResult, ImageColorSpace, ImageData, ImageDecodeParams,
};
use hayro_syntax::object::{Array, Dict, Kind};
use hayro_syntax::work::{Cancelled, Meter, POLL_BYTES};
use smallvec::SmallVec;
use std::borrow::Cow;

struct DecodeContext<'a> {
    /// The stored samples held ([`fit_rows`]).
    decoded: FilterResult<'a>,
    width: u32,
    /// The rows the samples held lie in, the last perhaps in part ([`fit_rows`]).
    height: u32,
    /// The rows the image declares.
    declared: u32,
    /// Whether the samples the data does not hold are transparent, not stored zero
    /// ([`short_rows_transparent`]).
    transparent: bool,
    scale_factors: (f32, f32),
    color_space: ColorSpace,
    bits_per_component: u8,
    decode_arr: SmallVec<[(f32, f32); 4]>,
}

/// The rows a decoded plane covers, those its image declares (or its filter
/// reports), and what lies past the texels it holds, which are the first of them in
/// row order: the texel of a sample the data does not hold, or nothing where those
/// are transparent (the plane then holds whole rows). So a plane costs the data it
/// holds, not the size the image declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rows {
    pub(crate) declared: u32,
    /// The texel past those held, its first bytes as many as the plane's channels.
    pub(crate) fill: Option<Texel>,
}

/// A texel of up to three bytes: luma or alpha in its first, RGB in all three.
pub(crate) type Texel = [u8; 3];

impl Rows {
    /// Whether a plane of `len` bytes, of rows `width` texels of `channels` bytes wide,
    /// holds the texels of `height` rows, at most those it covers, all of the last one
    /// too where the texels past those held are transparent: as every reader of a
    /// decoded plane takes it to.
    fn hold(self, len: usize, width: u32, height: u32, channels: usize) -> bool {
        let Some(row) = (width as usize).checked_mul(channels) else {
            return false;
        };
        let fits = width > 0 && channels > 0 && height <= self.declared;
        fits && len.is_multiple_of(channels)
            && match self.fill {
                Some(_) => len.div_ceil(row) == height as usize,
                None => height > 0 && Some(len) == row.checked_mul(height as usize),
            }
    }
}

/// A decoded plane as its readers take it: the texels its data holds, the first of
/// rows `width` texels of `channels` bytes in row order, and past them its fill. The
/// one way to read a plane's rows, so that no reader reaches past the texels held.
#[derive(Clone, Copy)]
pub(crate) struct Held<'p> {
    data: &'p [u8],
    width: usize,
    channels: usize,
    /// The texel past those held; `None` where those are transparent.
    pub(crate) fill: Option<&'p [u8]>,
}

impl<'p> Held<'p> {
    pub(crate) fn new(data: &'p [u8], width: u32, channels: usize, fill: Option<&'p [u8]>) -> Self {
        Self {
            data,
            width: width as usize,
            channels: channels.max(1),
            fill,
        }
    }

    /// The texels held of columns `cols` of row `y`: those from `cols.start` up to
    /// `cols.end` or the last one held, perhaps none; the rest of `cols` are the fill.
    pub(crate) fn row(&self, y: u32, cols: Range<usize>) -> &'p [u8] {
        let texels = self.data.len() / self.channels;
        let at = (y as usize).saturating_mul(self.width);
        let end = at.saturating_add(cols.end.min(self.width)).min(texels);
        let start = at.saturating_add(cols.start).min(end);
        &self.data[start * self.channels..end * self.channels]
    }

    /// Whether row `y` and every row after it hold no texel: they are all the fill.
    pub(crate) fn past(&self, y: u32) -> bool {
        (y as usize).saturating_mul(self.width) >= self.data.len() / self.channels
    }
}

/// `data`, texels of `texel.len()` bytes, completed with `texel` up to `texels` of
/// them, a counted piece at a time; `None` once the meter's hook asks to stop.
pub(crate) fn complete(
    data: &mut Vec<u8>,
    texels: usize,
    texel: &[u8],
    meter: &Meter<'_>,
) -> Option<()> {
    let want = texels * texel.len();
    if data.len() >= want {
        return Some(());
    }
    meter.reserve_exact(data, want - data.len());
    while data.len() < want {
        let n = (want - data.len()).min(POLL_BYTES / texel.len() * texel.len());
        for _ in 0..n / texel.len() {
            data.extend_from_slice(texel);
        }
        meter.work(n).ok()?;
    }
    Some(())
}

/// How an image's samples are laid out and what they mean: from its dictionary, and
/// the image data a filter reported (`decode_context`).
pub(crate) struct SampleFormat {
    pub(crate) color_space: ColorSpace,
    pub(crate) bits_per_component: u8,
    /// A pair per component ([`decode_array`]).
    pub(crate) decode_arr: SmallVec<[(f32, f32); 4]>,
}

/// [`sample_format_with`], with the bits per component of the dictionary.
pub(crate) fn sample_format(
    obj: &ImageXObject<'_>,
    image_data: Option<&ImageData>,
    meter: &Meter<'_>,
) -> Result<SampleFormat, Cancelled> {
    sample_format_with(obj, image_data, dict_bpc(obj), meter)
}

/// The parameters `obj`'s filters decode with, at its own size; reading the filters
/// counts on `meter`.
pub(crate) fn image_decode_params(
    obj: &ImageXObject<'_>,
    meter: &Meter<'_>,
) -> Result<ImageDecodeParams, Cancelled> {
    decode_params(obj, None, dict_bpc(obj), meter)
}

/// The bits per component the image's dictionary gives.
fn dict_bpc(obj: &ImageXObject<'_>) -> Option<u8> {
    let dict = obj.stream.dict();
    dict.get::<u8>(BPC)
        .or_else(|| dict.get::<u8>(BITS_PER_COMPONENT))
}

/// How `obj`'s samples are laid out ([`SampleFormat`]), with the bits per component
/// of the dictionary already read; reading its decode array counts on `meter`.
fn sample_format_with(
    obj: &ImageXObject<'_>,
    image_data: Option<&ImageData>,
    dict_bpc: Option<u8>,
    meter: &Meter<'_>,
) -> Result<SampleFormat, Cancelled> {
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

    let decode_arr = decode_array(dict, &color_space, bits_per_component, meter)?;

    Ok(SampleFormat {
        color_space,
        bits_per_component,
        decode_arr,
    })
}

/// The decode array of samples of `color_space` and `bits_per_component` that
/// `dict`'s `/D` gives, or where it has none (or one referring to no object) its
/// `/Decode`, one pair per component, as `MuPDF` reads it (observed through `PyMuPDF`
/// 1.28.2): without an entry, or a null one, the colour space's default; an entry
/// missing from the array or not a number is 0, and entries past the components'
/// are ignored, and not read; an entry that is no array maps every sample to 0, and
/// is not read past its kind. Reading the array counts on `meter`.
fn decode_array(
    dict: &Dict<'_>,
    color_space: &ColorSpace,
    bits_per_component: u8,
    meter: &Meter<'_>,
) -> Result<SmallVec<[(f32, f32); 4]>, Cancelled> {
    let pairs = color_space.num_components() as usize;
    let entry = |key| {
        dict.get::<Array<'_>>(key)
            .map(Ok)
            .or_else(|| dict.get::<Kind>(key).map(Err))
    };
    let values = match entry(D).or_else(|| entry(DECODE)) {
        None | Some(Err(Kind::Null)) => {
            return Ok(color_space.default_decode_arr(f32::from(bits_per_component)));
        }
        Some(Ok(array)) => array.leading::<f32>(Take::Each(2 * pairs), meter)?.entries,
        Some(Err(_)) => SmallVec::new(),
    };
    let value = |i: usize| values.get(i).copied().flatten().unwrap_or(0.0);
    Ok((0..pairs)
        .map(|c| (value(2 * c), value(2 * c + 1)))
        .collect())
}

/// The parameters `obj`'s filters decode with: where its dictionary gives no bits
/// per component or colour space, those its samples are read with when no filter
/// reports them ([`sample_format_with`]), so that the last filter knows their size.
/// A JPX image's bits per component stay the dictionary's: its decoder scales its
/// samples to the depth it is given and keeps their own without one. Reading the
/// filters counts on `meter`.
fn decode_params(
    obj: &ImageXObject<'_>,
    target_dimension: Option<(u32, u32)>,
    dict_bpc: Option<u8>,
    meter: &Meter<'_>,
) -> Result<ImageDecodeParams, Cancelled> {
    let bpc = match dict_bpc {
        Some(bpc) => Some(bpc),
        None => {
            let filters = obj.stream.filter_entries();
            let mut jpx = false;
            for applied in filters.applied_counted(meter) {
                if applied?.0 == Filter::JpxDecode {
                    jpx = true;
                    break;
                }
            }
            (!jpx).then(|| fallback_bpc(obj.kind))
        }
    };
    let color_space = obj.color_space.as_ref();
    Ok(ImageDecodeParams {
        is_indexed: color_space.is_some_and(|cs| cs.is_indexed()),
        bpc,
        // Device gray's, as `sample_format_with` reads samples without a colour space.
        num_components: Some(color_space.map_or(1, |cs| cs.num_components())),
        target_dimension,
        width: obj.width(),
        height: obj.height(),
    })
}

/// The bits per component samples of an image of `kind` are read with where neither
/// its dictionary nor a filter gives them.
fn fallback_bpc(kind: ImageKind) -> u8 {
    if kind == ImageKind::StencilMask { 1 } else { 8 }
}

/// The context `obj` decodes in, the rows its data does not hold transparent where
/// [`short_rows_transparent`] says, and its [`colour_key`].
fn decode_context<'a>(
    obj: &ImageXObject<'a>,
    target_dimension: Option<(u32, u32)>,
    meter: &Meter<'_>,
) -> Option<(DecodeContext<'a>, Option<ColourKey>)> {
    let dict_bpc = dict_bpc(obj);
    let decode_params = decode_params(obj, target_dimension, dict_bpc, meter).ok()?;

    let mut decoded = obj
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
    } = sample_format_with(obj, decoded.image_data.as_ref(), dict_bpc, meter).ok()?;

    let components = color_space.num_components() as usize;
    let rows = SampleRows::new(width, components, bits_per_component, height);
    let key = colour_key(obj, components, meter).ok()?;
    let transparent = short_rows_transparent(obj, key.is_some());
    let (data, held) = fit_rows(
        core::mem::take(&mut decoded.data),
        rows,
        components,
        transparent,
        meter,
    )?;
    decoded.data = data;
    // Data that holds no whole row of an image whose short rows are transparent fails.
    if width == 0 || components == 0 || transparent && held == 0 {
        return None;
    }

    let ctx = DecodeContext {
        decoded,
        width,
        height: held,
        declared: height,
        transparent,
        scale_factors: (scale_x, scale_y),
        color_space,
        bits_per_component,
        decode_arr,
    };
    Some((ctx, key))
}

/// `data`, the stored samples of an image of `components` lying in `rows`, as
/// decoding reads them, and the rows they lie in, as `MuPDF` reads data that ends
/// early (observed through `PyMuPDF` 1.28.2): the image keeps its declared rows, and a
/// sample the data does not hold reads as stored zero, which then maps as a stored
/// one does (black gray or RGB, white CMYK, a palette's first entry, white under an
/// inverted decode array, a stencil mask painted). Of 8-bit samples, every byte held
/// counts, a texel the data ends within holding stored zero past it; of samples of
/// other depths, only whole rows do, a row the data ends within reading as zero.
/// Only the samples held are kept: the texel stored zero makes stands for the rest
/// ([`Rows::fill`]), so data of a few bytes declaring a huge image costs little.
/// Where short rows are `transparent`, only whole rows count at every depth and the
/// rows past them are transparent. Data past the declared rows is dropped. Where
/// the samples of a texel are completed, the data is copied or grown on `meter`;
/// `None` once its hook asks to stop.
fn fit_rows<'d>(
    data: Cow<'d, [u8]>,
    rows: SampleRows,
    components: usize,
    transparent: bool,
    meter: &Meter<'_>,
) -> Option<(Cow<'d, [u8]>, u32)> {
    let held = rows.bytes_held(data.len(), transparent);
    let data = match data {
        Cow::Borrowed(d) => Cow::Borrowed(&d[..held]),
        Cow::Owned(mut d) => {
            d.truncate(held);
            Cow::Owned(d)
        }
    };
    // 8-bit samples of a texel the data ends within; rows of them hold whole texels.
    let len = match rows.byte_granular() {
        true => held.div_ceil(components.max(1)) * components,
        false => held,
    };
    let data = match len > held {
        true => {
            let mut owned = into_owned(data, len, meter)?;
            owned.resize(len, 0);
            Cow::Owned(owned)
        }
        false => data,
    };
    Some((data, len.div_ceil(rows.row_bytes()) as u32))
}

/// Whether the rows of `obj`'s colour that its data does not hold are transparent,
/// not stored zero ([`fit_rows`]): as `MuPDF` draws them (observed through `PyMuPDF`
/// 1.28.2) where the image has a colour-key mask, or its mask stream (`/SMask`, else
/// a stream `/Mask`) has a `/Matte` array; `keyed` says whether it has a
/// [`colour_key`].
pub(crate) fn short_rows_transparent(obj: &ImageXObject<'_>, keyed: bool) -> bool {
    if obj.kind != ImageKind::Image {
        return false;
    }
    let dict = obj.stream.dict();
    match dict
        .get::<Stream<'_>>(SMASK)
        .or_else(|| dict.get::<Stream<'_>>(MASK))
    {
        Some(mask) => mask.dict().get::<Array<'_>>(MATTE).is_some(),
        None => keyed,
    }
}

/// The colour-key mask of `obj`, whose samples are of `components`: its `/Mask`
/// array, where it gives a range for each component, as `MuPDF` reads it (observed
/// through `PyMuPDF` 1.28.2): with fewer entries it is no mask, and entries past
/// those ranges are ignored, and not read: a file may add any number of them.
/// Reading the array counts on `meter`.
pub(crate) fn colour_key(
    obj: &ImageXObject<'_>,
    components: usize,
    meter: &Meter<'_>,
) -> Result<Option<ColourKey>, Cancelled> {
    let Some(array) = obj.stream.dict().get::<Array<'_>>(MASK) else {
        return Ok(None);
    };
    Ok(array
        .leading::<u16>(Take::AtLeast(2 * components), meter)?
        .values())
}

/// The `/Matte` of `obj`'s soft mask, where it gives a value for each of the image's
/// `components` and no more: with fewer or more numbers it gives no matte, and the
/// numbers past one more than those are not read. Reading it counts on `meter`.
pub(crate) fn matte(
    obj: &ImageXObject<'_>,
    components: usize,
    meter: &Meter<'_>,
) -> Result<Option<ColorComponents>, Cancelled> {
    let Some(array) = obj
        .stream
        .dict()
        .get::<Stream<'_>>(SMASK)
        .and_then(|mask| mask.dict().get::<Array<'_>>(MATTE))
    else {
        return Ok(None);
    };
    Ok(array
        .leading::<f32>(Take::Exactly(components), meter)?
        .values())
}

/// A colour-key mask: a range of stored values for each component, low then high.
pub(crate) type ColourKey = SmallVec<[u16; 4]>;

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

/// The bytes of `data`, owned, with room for `room` bytes at least: copied a
/// counted piece at a time if borrowed, moved so if owned without that room.
fn into_owned(data: Cow<'_, [u8]>, room: usize, meter: &Meter<'_>) -> Option<Vec<u8>> {
    let mut owned = match data {
        Cow::Owned(data) => data,
        Cow::Borrowed(data) => {
            let mut owned = meter.room(room.max(data.len()));
            for piece in data.chunks(POLL_BYTES) {
                meter.work(piece.len()).ok()?;
                owned.extend_from_slice(piece);
            }
            owned
        }
    };
    let more = room.saturating_sub(owned.len());
    meter.reserve_exact(&mut owned, more);
    Some(owned)
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
            let mut buf = meter.room(rows.row_len * height as usize);
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
            let mut buf = meter.room(width as usize * num_components * height as usize);
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
            let mut buf = meter.room(data.len() / (bits_per_component as usize / 8));
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
            let mut buf = meter.room(rows.row_len * height as usize);
            for_each_packed_row(data, rows, meter, |_, samples| {
                buf.extend(samples.iter().map(|&v| u16::from(v)));
            })?;
            Some(buf)
        }
        9..16 => {
            let rows = SampleRows::new(width, num_components, bits_per_component, height);
            let mut buf = meter.room(rows.row_len * height as usize);
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
            let mut buf = meter.room(data.len() / (bits_per_component as usize / 8));
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
/// source of the samples that both decoding an image whole and streaming it keep of
/// data that ends early.
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

    /// Of data of `len` stored bytes, those [`fit_rows`] keeps: at most the rows
    /// declared, and of samples of other depths than eight, or `whole_rows`, only
    /// whole rows.
    pub(crate) fn bytes_held(&self, len: usize, whole_rows: bool) -> usize {
        let row_bytes = self.row_bytes();
        let len = len.min(row_bytes.saturating_mul(self.height as usize));
        if whole_rows || !self.byte_granular() {
            len / row_bytes * row_bytes
        } else {
            len
        }
    }

    /// Whether every stored byte of a row the data ends within counts: for 8-bit
    /// samples.
    pub(crate) fn byte_granular(&self) -> bool {
        self.bits == 8
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

/// Visits the stored values of the rows of `data` (`rows.height` whole rows),
/// unpacked ([`unpack_packed`]), a counted piece at a time, each with the index in its
/// row of its first sample.
fn for_each_packed_row(
    data: &[u8],
    rows: SampleRows,
    meter: &Meter<'_>,
    mut visit: impl FnMut(usize, &[u8]),
) -> Option<()> {
    let kept = rows.height as usize;
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
    use hayro_syntax::object::{Object, ObjectIdentifier, Stream};
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
        let params = decode_params(&obj, None, dict_bpc(&obj), &Meter::default()).unwrap();
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

    /// The decode array a dictionary gives, read as its entry was read before it was
    /// read by kind: `/D`, or `/Decode` where it reads as no object, as an object.
    fn read_as_object(dict: &Dict<'_>) -> SmallVec<[(f32, f32); 4]> {
        let gray = ColorSpace::device_gray();
        let values: SmallVec<[f32; 2]> = match dict
            .get::<Object<'_>>(D)
            .or_else(|| dict.get::<Object<'_>>(DECODE))
        {
            None | Some(Object::Null(_)) => return gray.default_decode_arr(1.0),
            Some(Object::Array(array)) => array
                .items::<f32>()
                .take(2)
                .map(|v| v.unwrap_or(0.0))
                .collect(),
            Some(_) => SmallVec::new(),
        };
        let value = |i: usize| values.get(i).copied().unwrap_or(0.0);
        [(value(0), value(1))].into_iter().collect()
    }

    /// Whatever kind of object `/D` and `/Decode` are, directly or by reference, in
    /// a dictionary read from bytes (an inline image's) or parsed with its object's
    /// body, the decode array is the one reading the entry as an object gives.
    #[test]
    fn decode_entries_are_read_by_kind_as_they_were_read_as_objects() {
        use hayro_syntax::content::TypedIter;
        use hayro_syntax::content::ops::TypedInstruction;
        let values = [
            "[1 0]",
            "[0.5]",
            "[1 /N 0]",
            "null",
            "5",
            "1e3",
            "true",
            "/N",
            "(x)",
            "<< /A 1 >>",
            "[]",
            "4 0 R",
            "5 0 R",
            "6 0 R",
            "9 0 R",
        ];
        let mut dicts = Vec::new();
        for d in values
            .iter()
            .map(|v| format!("/D {v}"))
            .chain([String::new()])
        {
            for decode in values
                .iter()
                .map(|v| format!("/Decode {v}"))
                .chain([String::new()])
            {
                dicts.push(format!("{d} {decode}"));
            }
        }
        let gray = ColorSpace::device_gray();
        let meter = Meter::default();
        let mut inlines = 0;
        for entries in dicts {
            let objects = [
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
                format!("<< {entries} >>").into_bytes(),
                b"[0.25 0.75]".to_vec(),
                b"null".to_vec(),
                b"<< /B 2 >>".to_vec(),
            ];
            let pdf = Pdf::new(file(&objects, "")).unwrap();
            let parsed = pdf
                .xref()
                .get::<Dict<'_>>(ObjectIdentifier::new(3, 0))
                .unwrap();
            // An inline image holds no reference, nor a token a content stream
            // cannot read (`1e3`): with one, it is no image.
            let content = format!("BI {entries} ID x EI");
            let mut ops = TypedIter::new(content.as_bytes());
            let inline = loop {
                match ops.next() {
                    Some(TypedInstruction::InlineImage(image)) => {
                        break Some(image.0.dict().clone());
                    }
                    Some(_) => {}
                    None => break None,
                }
            };
            inlines += usize::from(inline.is_some());
            for (held, dict) in [("parsed", Some(&parsed)), ("inline", inline.as_ref())] {
                let Some(dict) = dict else {
                    continue;
                };
                let by_kind = decode_array(dict, &gray, 1, &meter).unwrap();
                assert_eq!(by_kind, read_as_object(dict), "{held} << {entries} >>");
            }
        }
        assert!(inlines > 100, "{inlines} inline images");
    }
}
