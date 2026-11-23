mod image;
mod mask;

pub(crate) use image::{DecodedImage, decode_image, direct_invert, unpremultiply_samples};
pub(crate) use mask::{BILEVEL_MASK_LUT, DecodedMask, MaskPath, decode_mask, mask_lut};

use crate::InterpreterWarning;
use crate::color::{ColorComponents, ColorSpace};
use crate::function::interpolate;
use crate::x_object::image::{ImageKind, SampledImage};
use core::ops::Range;
use hayro_syntax::Filter;
use hayro_syntax::bit_reader::BitReader;
use hayro_syntax::object::array::Take;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::stream::{
    DecodeFailure, FilterResult, ImageColorSpace, ImageData, ImageDecodeParams, OwnedStream,
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
/// them, a counted piece at a time; `None` once the meter's hook asks to stop, and,
/// before anything is allocated, where they do not fit in memory ([`room_for`]).
pub(crate) fn complete(
    data: &mut Vec<u8>,
    texels: usize,
    texel: &[u8],
    meter: &Meter<'_>,
) -> Option<()> {
    room_for(data, texels, texel.len(), meter)?;
    let want = texels * texel.len();
    while data.len() < want {
        let n = (want - data.len()).min(POLL_BYTES / texel.len() * texel.len());
        for _ in 0..n / texel.len() {
            data.extend_from_slice(texel);
        }
        meter.work(n).ok()?;
    }
    Some(())
}

/// Room in `data` for `texels` texels of `channels` bytes, made on `meter`; `None`,
/// with nothing allocated, where their bytes overflow or a vector cannot hold them
/// or memory cannot give them: a plane completed up to the rows an image declares
/// is sized by the file, not by the data it holds.
pub(crate) fn room_for(
    data: &mut Vec<u8>,
    texels: usize,
    channels: usize,
    meter: &Meter<'_>,
) -> Option<()> {
    let want = texels.checked_mul(channels)?;
    let more = want.saturating_sub(data.len());
    meter.try_reserve_exact(data, more).ok()
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
pub(crate) fn sample_format(obj: &SampledImage, image_data: Option<&ImageData>) -> SampleFormat {
    sample_format_with(obj, image_data, dict_bpc(obj))
}

/// The parameters `obj`'s filters decode with, at its own size; visiting the filters
/// counts on `meter`.
pub(crate) fn image_decode_params(
    obj: &SampledImage,
    meter: &Meter<'_>,
) -> Result<ImageDecodeParams, Cancelled> {
    decode_params(obj, None, dict_bpc(obj), meter)
}

/// The bits per component the image's dictionary gives.
fn dict_bpc(obj: &SampledImage) -> Option<u8> {
    obj.bpc
}

/// How `obj`'s samples are laid out ([`SampleFormat`]), with the bits per component
/// of the dictionary already read; its decode array was read as it was drawn.
fn sample_format_with(
    obj: &SampledImage,
    image_data: Option<&ImageData>,
    dict_bpc: Option<u8>,
) -> SampleFormat {
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

    let mut decode_arr = decode_array(&obj.decode, &color_space, bits_per_component);
    if let Some(image) = image_data {
        let max = |bits: u8| 2.0_f32.powi(i32::from(bits)) - 1.;
        for ((lo, hi), &bits) in decode_arr.iter_mut().zip(&image.component_depths) {
            *hi = *lo + (*hi - *lo) * (max(bits_per_component) / max(bits));
        }
    }

    SampleFormat {
        color_space,
        bits_per_component,
        decode_arr,
    }
}

/// The components of an image's samples as drawing knows them, which its arrays are
/// read for: those of its colour space, or, where it has none, those a filter
/// reports of its data, which only decoding learns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Components {
    Of(usize),
    /// Gray, RGB or CMYK, as a filter reports them, or gray where none does
    /// (`sample_format_with`): only DCT or JPX data report more than one
    /// ([`Self::for_data`]).
    OfData,
}

impl Components {
    /// The components of samples of `color_space`, where the dictionary gives one.
    pub(crate) fn of(color_space: Option<&ColorSpace>) -> Self {
        match color_space {
            Some(cs) => Self::Of(cs.num_components() as usize),
            None => Self::OfData,
        }
    }

    /// The components of samples of the data `stream` decodes to: those of data no
    /// DCT or JPX filter decodes, which report gray or nothing, are gray. Read from the
    /// filters alone, not from the data's header, which only decoding reads.
    pub(crate) fn for_data(self, stream: &OwnedStream) -> Self {
        let applies = |filter| uncancelled(stream.applies(filter, &Meter::default()));
        match self {
            Self::OfData if !applies(Filter::DctDecode) && !applies(Filter::JpxDecode) => {
                Self::Of(1)
            }
            other => other,
        }
    }

    /// The most components the samples may have: an array is read for these, which
    /// reads every entry a rule reads for fewer, and the rule for fewer is told from it.
    fn most(self) -> usize {
        match self {
            Self::Of(n) => n,
            // At most 8 entries, whatever follows; references among them resolve inside a draw.
            Self::OfData => 4,
        }
    }

    /// Whether the samples may have `n` components.
    fn may_be(self, n: usize) -> bool {
        match self {
            Self::Of(c) => c == n,
            Self::OfData => matches!(n, 1 | 3 | 4),
        }
    }
}

/// The decode array an image's dictionary gives ([`read_decode`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum DecodeRead {
    /// None, or a null one: the colour space's default.
    #[default]
    Default,
    /// The first entries of an array, a number or none each, as many as its
    /// components may read; none for an entry that is no array.
    Entries(SmallVec<[Option<f32>; 8]>),
}

/// What decoding reads of the decode array `dict`'s `/D` gives, or where it has none
/// (or one referring to no object) its `/Decode`, for samples of `components`, as
/// `MuPDF` reads it (observed through `PyMuPDF` 1.28.2): without an entry, or a null
/// one, the colour space's default; of an array, a pair per component, an entry
/// missing or not a number being 0, entries past the components' ignored, and not
/// read; an entry that is no array maps every sample to 0, and is not read past its
/// kind. Read as the image is drawn, through its document; its work is bounded by the
/// components, however long the array, which are asked only of an array.
pub(crate) fn read_decode(dict: &Dict<'_>, components: impl FnOnce() -> Components) -> DecodeRead {
    let meter = Meter::default();
    let entry = |key| {
        dict.get::<Array<'_>>(key)
            .map(Ok)
            .or_else(|| dict.get::<Kind>(key).map(Err))
    };
    match entry(D).or_else(|| entry(DECODE)) {
        None | Some(Err(Kind::Null)) => DecodeRead::Default,
        Some(Ok(array)) => DecodeRead::Entries(
            uncancelled(array.leading::<f32>(Take::Each(2 * components().most()), &meter)).entries,
        ),
        Some(Err(_)) => DecodeRead::Entries(SmallVec::new()),
    }
}

/// The decode array of samples of `color_space` and `bits_per_component` that `read`
/// gives ([`read_decode`]): one pair per component.
fn decode_array(
    read: &DecodeRead,
    color_space: &ColorSpace,
    bits_per_component: u8,
) -> SmallVec<[(f32, f32); 4]> {
    let values = match read {
        DecodeRead::Default => {
            return color_space.default_decode_arr(f32::from(bits_per_component));
        }
        DecodeRead::Entries(values) => values,
    };
    let value = |i: usize| values.get(i).copied().flatten().unwrap_or(0.0);
    let pairs = color_space.num_components() as usize;
    (0..pairs)
        .map(|c| (value(2 * c), value(2 * c + 1)))
        .collect()
}

/// The value of an array read on a meter without a hook, which never stops.
fn uncancelled<T>(read: Result<T, Cancelled>) -> T {
    match read {
        Ok(value) => value,
        Err(Cancelled) => unreachable!("a meter without a hook never stops"),
    }
}

/// The dictionary's sample layout, preserving an unspecified JPX depth or color space.
fn decode_params(
    obj: &SampledImage,
    target_dimension: Option<(u32, u32)>,
    dict_bpc: Option<u8>,
    meter: &Meter<'_>,
) -> Result<ImageDecodeParams, Cancelled> {
    let bpc = match dict_bpc {
        Some(bpc) => Some(bpc),
        None => {
            let jpx = obj.stream.applies(Filter::JpxDecode, meter)?;
            (!jpx).then(|| fallback_bpc(obj.kind))
        }
    };
    let color_space = obj.color_space.as_ref();
    Ok(ImageDecodeParams {
        has_color_key: obj.masks.has_color_key(),
        is_indexed: color_space.is_some_and(|cs| cs.is_indexed()),
        bpc,
        num_components: color_space.map(|cs| cs.num_components()),
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
    obj: &'a SampledImage,
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
    } = sample_format_with(obj, decoded.image_data.as_ref(), dict_bpc);

    let components = color_space.num_components() as usize;
    let rows = SampleRows::new(width, components, bits_per_component, height);
    let key = colour_key(obj, components);
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
/// a stream `/Mask` decoding can use) has a `/Matte` array; `keyed` says whether it
/// has a [`colour_key`].
pub(crate) fn short_rows_transparent(obj: &SampledImage, keyed: bool) -> bool {
    if obj.kind != ImageKind::Image {
        return false;
    }
    obj.masks.mask_matte.unwrap_or(keyed)
}

/// The colour-key mask of `obj`, whose samples are of `components`: its `/Mask`
/// array, where it gives a range for each component, as `MuPDF` reads it (observed
/// through `PyMuPDF` 1.28.2): with fewer entries it is no mask, and entries past
/// those ranges are ignored, and not read: a file may add any number of them. Read as
/// the image was drawn ([`KeyRead`]).
pub(crate) fn colour_key(obj: &SampledImage, components: usize) -> Option<ColourKey> {
    obj.masks.color_key(components)
}

/// The `/Matte` of `obj`'s soft mask, where it gives a value for each of the image's
/// `components` and no more: with fewer or more numbers it gives no matte, and the
/// numbers past one more than those are not read. Read as the image was drawn
/// ([`MatteRead`]).
pub(crate) fn matte(obj: &SampledImage, components: usize) -> Option<ColorComponents> {
    obj.masks.matte(components)
}

/// A colour key's `/Mask` array as drawing reads it, for samples that may have
/// [`Components`]: the numbers it starts with, up to two per component of the most,
/// read as [`Take::AtLeast`] reads them, which tell the key of fewer components.
#[derive(Clone, Debug, Default)]
pub(crate) struct KeyRead(SmallVec<[u32; 8]>);

impl KeyRead {
    pub(crate) fn has_range(&self) -> bool {
        self.0.len() >= 2
    }

    /// `array`, where the dictionary gives one, read for `components`, which are asked
    /// only of an array.
    pub(crate) fn read(array: Option<Array<'_>>, components: impl FnOnce() -> Components) -> Self {
        let Some(array) = array else {
            return Self::default();
        };
        let take = Take::AtLeast(2 * components().most());
        let leading = uncancelled(array.leading::<u32>(take, &Meter::default()));
        Self(leading.entries.into_iter().flatten().collect())
    }

    /// The key of samples of `components`: a range for each, where the array starts
    /// with as many numbers.
    pub(crate) fn key(&self, components: usize) -> Option<ColourKey> {
        let ranges = self.0.get(..2 * components)?;
        (components > 0).then(|| ranges.iter().copied().collect())
    }
}

/// A soft mask's `/Matte` array as drawing reads it, for an image whose samples may
/// have [`Components`]: the numbers it starts with, up to as many as the most, and
/// whether a number follows those, read as [`Take::Exactly`] reads them, which tell
/// the matte of fewer components.
#[derive(Clone, Debug)]
pub(crate) struct MatteRead {
    numbers: ColorComponents,
    most: usize,
    /// Whether the array has a number past the most components'.
    more: bool,
}

impl MatteRead {
    /// `array` read for `components`.
    pub(crate) fn read(array: &Array<'_>, components: Components) -> Self {
        let most = components.most();
        let leading = uncancelled(array.leading::<f32>(Take::Exactly(most), &Meter::default()));
        let numbers: ColorComponents = leading.entries.into_iter().flatten().collect();
        let more = numbers.len() == most && !leading.met;
        Self {
            numbers,
            most,
            more,
        }
    }

    /// The matte of an image of `components`: where the array starts with exactly as
    /// many numbers.
    pub(crate) fn matte(&self, components: usize) -> Option<ColorComponents> {
        let exactly = self.numbers.len() == components
            && components <= self.most
            && (components < self.most || !self.more);
        exactly.then(|| self.numbers.clone())
    }

    /// Whether an image whose samples may have `components` may have a matte.
    pub(crate) fn may_meet(&self, components: Components) -> bool {
        (1..=self.most).any(|n| components.may_be(n) && self.matte(n).is_some())
    }
}

/// A colour-key mask: a range of stored values for each component, low then high.
pub(crate) type ColourKey = SmallVec<[u32; 4]>;

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
        9..16 | 17..=31 => {
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
        _ => None,
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
        _ => None,
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
    use crate::x_object::image::ImageXObject;
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
        let obj = SampledImage::read(&obj).unwrap();
        let params = decode_params(&obj, None, dict_bpc(&obj), &Meter::default()).unwrap();
        (params.bpc, params.num_components)
    }

    #[test]
    fn raw_jpx_infers_rgb_when_color_space_is_missing() {
        let decode = |space: &str| {
            let data = include_bytes!("../../../../hayro-jpeg2000/tests/streams/mct_ict.j2k");
            let objects = [
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
                stream(
                    &format!(
                        "/Type /XObject /Subtype /Image /Width 128 /Height 96 /Filter /JPXDecode {space}"
                    ),
                    data,
                ),
            ];
            let pdf = Pdf::new(file(&objects, "")).unwrap();
            let stream = pdf
                .xref()
                .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
                .unwrap();
            let sink: WarningSinkFn = Arc::new(|_| {});
            let obj = ImageXObject::new(&stream, |_| None, &sink, &Cache::new(), None).unwrap();
            let obj = SampledImage::read(&obj).unwrap();
            let (image, _) = decode_image(&obj, None, &Meter::default())
                .unwrap()
                .into_whole()
                .unwrap();
            let crate::ImageData::Rgb(rgb) = image else {
                panic!("expected RGB for {space}")
            };
            rgb.data
        };
        let expected = decode("/ColorSpace /DeviceRGB");
        assert_eq!(expected.len(), 128 * 96 * 3);
        assert!(expected.as_chunks::<3>().0.iter().any(|c| c[0] != c[1]));
        for space in ["/ColorSpace null", ""] {
            assert!(decode(space) == expected, "{space}");
        }
    }

    #[test]
    fn source_precision_jpx_key() {
        let data = include_bytes!(
            "../../../../hayro-jpeg2000/tests/streams/quantization_irreversible_16bit.j2k"
        );
        let (_, mask) = keyed_jpx(
            data,
            "/Width 128 /Height 96 /ColorSpace /DeviceGray /Mask [256 65535]",
        );
        let data = mask.unwrap().data;
        // OpenJPEG decodes these three source samples as 48251, 305 and 73.
        assert_eq!([data[0], data[7], data[82]], [0, 0, 255]);
    }

    #[test]
    fn mixed_precision_jpx_key() {
        let data = include_bytes!("../../../tests/fixtures/jpx-mixed-precision.j2k");
        let (image, mask) = keyed_jpx(
            data,
            "/Width 1 /Height 1 /ColorSpace /DeviceRGB /Mask [128 128 32768 32768 8 8]",
        );
        assert_eq!(mask.unwrap().data, [0]);
        let crate::ImageData::Rgb(image) = image else {
            panic!("expected RGB")
        };
        assert_eq!(image.data, [128, 128, 136]);
    }

    #[test]
    fn jpx_key_above_sixteen_bits() {
        // Empty code blocks reconstruct to the midpoint of each unsigned component.
        let mut data = *include_bytes!("../../../tests/fixtures/jpx-mixed-precision.j2k");
        data[45] = 16; // SIZ precision minus one of the green component.
        let (image, mask) = keyed_jpx(
            &data,
            "/Width 1 /Height 1 /ColorSpace /DeviceRGB /Mask [128 128 65536 65536 8 8]",
        );
        assert_eq!(mask.unwrap().data, [0]);
        let crate::ImageData::Rgb(image) = image else {
            panic!("expected RGB")
        };
        assert_eq!(image.data, [128, 128, 136]);
    }

    fn keyed_jpx(data: &[u8], params: &str) -> (crate::ImageData, Option<crate::LumaData>) {
        keyed_jpx_at(data, params, None)
    }

    #[test]
    fn keyed_jpx_thumbnail() {
        let (image, mask) = keyed_jpx_at(
            include_bytes!("../../../tests/fixtures/keyed-thumbnail.j2k"),
            "/Width 4096 /Height 4096 /ColorSpace /DeviceRGB /Mask [20 20 60 60 100 100]",
            Some((512, 512)),
        );
        assert_eq!((image.width(), image.height()), (512, 512));
        let mask = mask.unwrap();
        assert_eq!([mask.data[128], mask.data[384]], [0, 255]);
    }

    #[test]
    fn indexed_jpx_dictionary_depth() {
        let decode = |depth: &str| {
            let (image, mask) = keyed_jpx(
                include_bytes!(
                    "../../../../hayro-jpeg2000/tests/streams/codestream_precision_4.j2k"
                ),
                &format!(
                    "/Width 128 /Height 96 /ColorSpace [/Indexed /DeviceGray 15 <00112233445566778899AABBCCDDEEFF>] /Mask [1 1] {depth}"
                ),
            );
            let crate::ImageData::Luma(gray) = image else {
                panic!("expected grayscale")
            };
            (gray.data, mask.unwrap().data)
        };
        let expected = decode("");
        assert_eq!(expected.1.iter().filter(|&&a| a == 0).count(), 328);
        let actual = decode("/BitsPerComponent 8");
        assert!(actual.1 == expected.1);
        assert!(actual.0 == expected.0);
    }

    fn keyed_jpx_at(
        data: &[u8],
        params: &str,
        hint: Option<(u32, u32)>,
    ) -> (crate::ImageData, Option<crate::LumaData>) {
        let objects = [
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            stream(
                &format!("/Type /XObject /Subtype /Image /Filter /JPXDecode {params}"),
                data,
            ),
        ];
        let pdf = Pdf::new(file(&objects, "")).unwrap();
        let stream = pdf
            .xref()
            .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
            .unwrap();
        let sink: WarningSinkFn = Arc::new(|_| {});
        let obj = ImageXObject::new(&stream, |_| None, &sink, &Cache::new(), None).unwrap();
        let obj = SampledImage::read(&obj).unwrap();
        decode_image(&obj, hint, &Meter::default())
            .unwrap()
            .into_whole()
            .unwrap()
    }

    /// Flate data is told the size of the samples the image code reads, with the
    /// same defaults; a JPX image's depth is its own where its dictionary sets none.
    #[test]
    fn images_decode_with_the_layout_their_samples_are_read_with() {
        for (dict, expected) in [
            ("/ImageMask true", (Some(1), Some(1))),
            ("/ImageMask true /BitsPerComponent 1", (Some(1), Some(1))),
            ("/ColorSpace /DeviceRGB", (Some(8), Some(3))),
            ("/BitsPerComponent 4", (Some(4), None)),
        ] {
            let dict = format!("{dict} /Filter /FlateDecode");
            assert_eq!(decoded_with(&dict), expected, "{dict}");
        }
        for (dict, expected) in [
            ("/ColorSpace /DeviceRGB", (None, Some(3))),
            ("/ImageMask true", (None, Some(1))),
            ("/BitsPerComponent 16", (Some(16), None)),
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
                let by_kind = decode_array(&read_decode(dict, || Components::Of(1)), &gray, 1);
                assert_eq!(by_kind, read_as_object(dict), "{held} << {entries} >>");
            }
        }
        assert!(inlines > 100, "{inlines} inline images");
    }

    /// An absent array asks nothing of the image, not even its components.
    #[test]
    fn an_absent_array_asks_for_no_components() {
        let asked = || -> Components { panic!("the components are asked for") };
        assert_eq!(KeyRead::read(None, asked).key(1), None);
        let pdf = Pdf::new(file(&[b"<< /Type /Catalog >>".to_vec()], "")).unwrap();
        let dict = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(1, 0))
            .unwrap();
        assert_eq!(read_decode(&dict, asked), DecodeRead::Default);
    }

    /// Arrays of every kind of entry, directly and by reference, of lengths about
    /// those the rules read: every array of up to 3 entries, and runs of up to 10
    /// numbers with one entry of each kind at each place.
    fn arrays() -> Vec<String> {
        const KINDS: [&str; 14] = [
            "0", "100", "70000", "-1", "0.5", "null", "/N", "(s)", "[]", "<< >>", "true", "4 0 R",
            "5 0 R", "9 0 R",
        ];
        let mut arrays = vec![Vec::new()];
        let mut level: Vec<Vec<&str>> = vec![Vec::new()];
        for _ in 0..3 {
            level = level
                .iter()
                .flat_map(|a| {
                    KINDS.iter().map(move |kind| {
                        let mut a = a.clone();
                        a.push(*kind);
                        a
                    })
                })
                .collect();
            arrays.extend(level.iter().cloned());
        }
        for len in 1..=10 {
            for at in 0..len {
                for kind in KINDS {
                    let mut a = vec!["7"; len];
                    a[at] = kind;
                    arrays.push(a);
                }
            }
        }
        arrays
            .into_iter()
            .map(|a| format!("[{}]", a.join(" ")))
            .collect()
    }

    /// What an image without a colour space reads of its arrays, for the four
    /// components its data may have at most, tells what each array's rule reads for
    /// the components its data have, as reading for those alone tells it: the decode
    /// entries ([`Take::Each`]), the colour key ([`Take::AtLeast`]) and the matte
    /// ([`Take::Exactly`]), of 1 to 4 components, for arrays of every kind of entry.
    /// So does what an image of a colour space reads, for its own components.
    #[test]
    fn arrays_read_for_the_most_components_tell_those_read_for_each() {
        let arrays = arrays();
        let dicts: String = arrays
            .iter()
            .map(|a| format!("<< /Decode {a} /Mask {a} /Matte {a} >>"))
            .collect();
        let objects = [
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            format!("[{dicts}]").into_bytes(),
            b"7".to_vec(),
            b"[0.25 0.75]".to_vec(),
        ];
        let pdf = Pdf::new(file(&objects, "")).unwrap();
        let dicts: Vec<Dict<'_>> = pdf
            .xref()
            .get::<Array<'_>>(ObjectIdentifier::new(3, 0))
            .unwrap()
            .items::<Dict<'_>>()
            .map(Option::unwrap)
            .collect();
        assert_eq!(dicts.len(), arrays.len());
        let meter = Meter::default();
        let colour_spaces = [
            ColorSpace::device_gray(),
            ColorSpace::device_rgb(),
            ColorSpace::device_cmyk(),
        ];
        // How often each rule gives a key, a matte or a decode entry, of each count.
        let mut met = [[0_usize; 3]; 4];
        for (dict, array) in dicts.iter().zip(&arrays) {
            let entries = |key| dict.get::<Array<'_>>(key).unwrap();
            for n in 1..=4 {
                let decode = uncancelled(entries(DECODE).leading::<f32>(Take::Each(2 * n), &meter));
                let key: Option<ColourKey> =
                    uncancelled(entries(MASK).leading::<u32>(Take::AtLeast(2 * n), &meter))
                        .values();
                let matte: Option<ColorComponents> =
                    uncancelled(entries(MATTE).leading::<f32>(Take::Exactly(n), &meter)).values();
                met[n - 1][0] += usize::from(decode.entries.iter().any(Option::is_some));
                met[n - 1][1] += usize::from(key.is_some());
                met[n - 1][2] += usize::from(matte.is_some());
                for components in [Components::OfData, Components::Of(n)] {
                    let case = format!("{array} for {n} of {components:?}");
                    let DecodeRead::Entries(read) = read_decode(dict, || components) else {
                        panic!("{case}: an array is read");
                    };
                    let told: Vec<_> = read.iter().take(2 * n).copied().collect();
                    assert_eq!(told[..], decode.entries[..], "/Decode {case}");
                    let read_key = KeyRead::read(Some(entries(MASK)), || components);
                    assert_eq!(read_key.key(n), key, "/Mask {case}");
                    let read_matte = MatteRead::read(&entries(MATTE), components);
                    assert_eq!(read_matte.matte(n), matte, "/Matte {case}");
                }
                if let Some(cs) = colour_spaces
                    .iter()
                    .find(|cs| cs.num_components() as usize == n)
                {
                    let read = read_decode(dict, || Components::OfData);
                    let direct = DecodeRead::Entries(decode.entries.clone());
                    assert_eq!(
                        decode_array(&read, cs, 8),
                        decode_array(&direct, cs, 8),
                        "/Decode {array} for {n}"
                    );
                }
            }
            // A soft mask is read under `/SMaskInData 1` where the image may have a
            // matte: for the counts the data may have.
            let matte = |n| {
                let leading = entries(MATTE).leading::<f32>(Take::Exactly(n), &meter);
                uncancelled(leading).values::<ColorComponents>().is_some()
            };
            let read = MatteRead::read(&entries(MATTE), Components::OfData);
            assert_eq!(
                read.may_meet(Components::OfData),
                [1, 3, 4].into_iter().any(matte),
                "/Matte {array}"
            );
            for n in 1..=4 {
                let read = MatteRead::read(&entries(MATTE), Components::Of(n));
                assert_eq!(read.may_meet(Components::Of(n)), matte(n), "/Matte {array}");
            }
        }
        assert!(met.iter().flatten().all(|&m| m > 0), "{met:?}");
    }
}
