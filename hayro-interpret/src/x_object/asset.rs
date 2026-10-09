//! Raster images as owned assets that decode a region of a plane at a requested
//! density, so a renderer keeps a handle instead of decoded pixels and asks for what a
//! view shows.

use super::area::Area;
use super::decode::{
    BILEVEL_MASK_LUT, MaskPath, SampleFormat, decode_image, decode_mask, direct_invert,
    rows_present, sample_format, unpremultiply_samples,
};
use super::image::{ImageKind, ImageXObject};
use crate::WarningSinkFn;
use crate::cache::Cache;
use crate::color::{ColorComponents, ColorSpace, ToLuma, ToRgb};
use crate::interpret::state::ActiveTransferFunction;
use crate::{ImageData, LumaData};
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::stream::{DecodedCheckpoint, DecodedReader, OwnedStream, ReadError};
use kurbo::Affine;
use smallvec::SmallVec;
use std::sync::{Arc, Mutex};

/// A raster image's source, owned: its stream (by object identifier, or a copy of an
/// inline image's bytes), resolved colour space, transfer function, interpolation
/// and masks, without the content stream or resources it was drawn from. It holds no
/// decoded pixels; [`ImageAsset::open`] reads it. Cheap to clone.
#[derive(Clone)]
pub struct ImageAsset(Arc<AssetData>);

struct AssetData {
    stream: OwnedStream,
    checkpoints: Mutex<Checkpoints>,
    width: u32,
    height: u32,
    color_space: Option<ColorSpace>,
    interpolate: bool,
    transfer_function: Option<ActiveTransferFunction>,
    cache: Cache,
    warning_sink: WarningSinkFn,
    cs_by_name: bool,
    key: Option<u128>,
}

impl ImageXObject<'_> {
    pub(crate) fn asset(&self, key: Option<u128>) -> ImageAsset {
        ImageAsset(Arc::new(AssetData {
            stream: self.stream.to_owned_stream(),
            checkpoints: Mutex::new(Checkpoints::default()),
            width: self.width,
            height: self.height,
            color_space: self.color_space.clone(),
            interpolate: self.interpolate,
            transfer_function: self.transfer_function.clone(),
            // The colour space is resolved; decoding uses a cache only to read masks
            // (always `DeviceGray`), so the page's cache is not kept alive.
            cache: Cache::new(),
            warning_sink: self.warning_sink.clone(),
            cs_by_name: self.cs_by_name,
            key,
        }))
    }
}

impl ImageAsset {
    /// The width of the image's pixel grid, as its dictionary declares it: the image's
    /// transform maps `0..width` × `0..height` onto the unit square.
    pub fn width(&self) -> u32 {
        self.0.width
    }

    /// The height of the image's pixel grid, as its dictionary declares it.
    pub fn height(&self) -> u32 {
        self.0.height
    }

    /// Identifies what the asset decodes to, as
    /// [`RasterImage::pixels_key`](crate::RasterImage::pixels_key): two assets with the
    /// same key decode alike.
    /// `None` when the asset cannot be identified beyond itself.
    pub fn key(&self) -> Option<u128> {
        self.0.key
    }

    /// Bytes retained by this asset's shared decoder index, including Rust values
    /// and state allocations, excluding allocator bookkeeping and shared input.
    pub fn checkpoint_bytes(&self) -> Result<usize, RequestError> {
        Ok(self
            .0
            .checkpoints
            .lock()
            .map_err(|_| RequestError::Failed)?
            .allocation_size())
    }

    fn xobject(&self) -> Option<ImageXObject<'_>> {
        let a = &*self.0;
        Some(ImageXObject {
            width: a.width,
            height: a.height,
            color_space: a.color_space.clone(),
            cache: a.cache.clone(),
            interpolate: a.interpolate,
            kind: ImageKind::Image,
            stream: a.stream.get()?,
            transfer_function: a.transfer_function.clone(),
            warning_sink: a.warning_sink.clone(),
            cs_by_name: a.cs_by_name,
        })
    }

    /// Opens the image for requests. Formats read incrementally (no filter or Flate,
    /// 8-bit `DeviceGray` or `DeviceRGB` samples used as stored or inverted, and their
    /// masks) only read their dictionaries here. Unencrypted Flate images without
    /// predictors or lockstep masks retain a bounded decoder index on the asset;
    /// later requests resume from a prior checkpoint, even after reopening. Other
    /// incremental paths stream from the start. Requests keep a row and their bins.
    /// Other formats are decoded whole here (with `hint`, the size they are wanted
    /// at, for decoders that can reduce), and requests read the decoded planes, which
    /// the source holds until it is dropped. `None` when the image cannot be decoded.
    pub fn open(&self, hint: Option<(u32, u32)>) -> Option<ImageSource<'_>> {
        let obj = self.xobject()?;
        if let Some(streamed) = Streamed::new(&obj, &self.0.stream, &self.0.checkpoints) {
            return Some(ImageSource {
                layout: streamed.layout(),
                inner: Inner::Streamed(Box::new(streamed)),
            });
        }
        let decoded = decode_image(&obj, hint)?;
        let decoded = Decoded::new(decoded.image, decoded.alpha);
        Some(ImageSource {
            layout: decoded.layout(),
            inner: Inner::Decoded(Box::new(decoded)),
        })
    }
}

/// The planes an image is drawn from. Each covers the image's whole pixel grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageLayout {
    /// The image's colour, with its alpha when that has the same grid and
    /// interpolation.
    pub image: PlaneLayout,
    /// A soft mask of another size or interpolation than the colour, drawn as its own
    /// plane and composited onto the colour (destination-in).
    pub mask: Option<PlaneLayout>,
}

impl ImageLayout {
    /// The layout of `plane`, if the image has it.
    pub fn plane(&self, plane: Plane) -> Option<&PlaneLayout> {
        match plane {
            Plane::Image => Some(&self.image),
            Plane::Mask => self.mask.as_ref(),
        }
    }
}

/// One plane of an image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaneLayout {
    /// The plane's texels across.
    pub width: u32,
    /// The plane's texels down.
    pub height: u32,
    /// The size of a texel in the image's declared pixel grid, per axis (not 1 when a
    /// decoder produced another size than the dictionary declares, or for a mask of
    /// another size).
    pub to_image: (f64, f64),
    /// Whether the plane is to be interpolated when drawn enlarged.
    pub interpolate: bool,
    /// What requests return (alpha is added where the data ends early).
    pub format: PixelFormat,
}

/// A plane of an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Plane {
    /// The colour, with its alpha when on the same grid ([`ImageLayout::image`]).
    Image,
    /// A soft mask on its own grid ([`ImageLayout::mask`]).
    Mask,
}

/// Bytes per texel and what they mean. Colour is premultiplied by alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// Luma.
    Luma,
    /// Luma and alpha.
    LumaAlpha,
    /// Red, green, blue.
    Rgb,
    /// Red, green, blue and alpha.
    RgbAlpha,
    /// Alpha alone (a mask).
    Alpha,
}

impl PixelFormat {
    /// Bytes per texel.
    pub fn channels(self) -> usize {
        match self {
            Self::Luma | Self::Alpha => 1,
            Self::LumaAlpha => 2,
            Self::Rgb => 3,
            Self::RgbAlpha => 4,
        }
    }

    /// Whether the last byte is alpha.
    pub fn has_alpha(self) -> bool {
        matches!(self, Self::LumaAlpha | Self::RgbAlpha | Self::Alpha)
    }

    fn with_alpha(self) -> Self {
        match self {
            Self::Luma => Self::LumaAlpha,
            Self::Rgb => Self::RgbAlpha,
            other => other,
        }
    }
}

/// A request for part of a plane at a density.
pub struct ImageRequest<'c> {
    /// The plane.
    pub plane: Plane,
    /// The plane divided into this many bins across and down, each at most the
    /// plane's size: a bin is the area-weighted mean of the texels it covers (at the
    /// plane's size, the texels themselves).
    pub grid: (u32, u32),
    /// The bins wanted, `[x0, y0, x1, y1)`, non-empty and within `grid`.
    pub window: [u32; 4],
    /// Polled while decoding; returning true abandons the request.
    pub cancel: Option<&'c dyn Fn() -> bool>,
}

/// The bins a request asked for.
#[derive(Clone, Debug)]
pub struct ImageRegion {
    /// The texel format of `data`.
    pub format: PixelFormat,
    /// The bins, row by row, `format.channels()` bytes each.
    pub data: Vec<u8>,
    /// The bins `data` holds, `[x0, y0, x1, y1)` of `grid`.
    pub window: [u32; 4],
    /// The grid the bins belong to.
    pub grid: (u32, u32),
    /// Maps `data`'s pixel grid (`0..width` × `0..height` of the window) onto the
    /// image's declared pixel grid.
    pub to_image: Affine,
    /// The source rows the request decoded, including those it skipped through.
    pub rows_decoded: u32,
    /// Bytes produced by incremental Flate decoders during this request (zero for
    /// other codecs). Includes work discarded on restart; excludes checkpoint-skipped
    /// bytes and the whole-image permissive fallback's work on corrupt streams.
    pub bytes_inflated: u64,
}

/// Why a request returned no bins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// The plane does not exist, or the grid or window is out of bounds.
    Invalid,
    /// The `cancel` hook asked to stop.
    Cancelled,
    /// The data could not be decoded.
    Failed,
}

/// An image opened for requests (see [`ImageAsset::open`]).
pub struct ImageSource<'a> {
    layout: ImageLayout,
    inner: Inner<'a>,
}

enum Inner<'a> {
    Streamed(Box<Streamed<'a>>),
    Decoded(Box<Decoded>),
}

impl ImageSource<'_> {
    /// The image's planes.
    pub fn layout(&self) -> &ImageLayout {
        &self.layout
    }

    /// Whether requests stream the data rather than read planes decoded at `open`.
    pub fn is_streamed(&self) -> bool {
        matches!(self.inner, Inner::Streamed(_))
    }

    /// Bytes retained by this asset's decoder index, including entries and state
    /// allocations, excluding allocator bookkeeping. Shared by reopened sources.
    pub fn checkpoint_bytes(&self) -> Result<usize, RequestError> {
        match &self.inner {
            Inner::Streamed(s) => Ok(s
                .checkpoints
                .lock()
                .map_err(|_| RequestError::Failed)?
                .allocation_size()),
            Inner::Decoded(_) => Ok(0),
        }
    }

    /// The bins `request` asks for.
    pub fn request(&mut self, request: &ImageRequest<'_>) -> Result<ImageRegion, RequestError> {
        let plane = *self
            .layout
            .plane(request.plane)
            .ok_or(RequestError::Invalid)?;
        let (grid, w) = (request.grid, request.window);
        if !(1..=plane.width).contains(&grid.0)
            || !(1..=plane.height).contains(&grid.1)
            || w[0] >= w[2]
            || w[1] >= w[3]
            || w[2] > grid.0
            || w[3] > grid.1
        {
            return Err(RequestError::Invalid);
        }
        let mut area = Area::new(
            (plane.width, plane.height),
            grid,
            w,
            plane.format.channels(),
        );
        let (rows_decoded, bytes_inflated) = match &mut self.inner {
            Inner::Streamed(s) => s.run(request.plane, &mut area, request.cancel),
            Inner::Decoded(d) => d
                .run(request.plane, &mut area, request.cancel)
                .map(|rows| (rows, 0)),
        }?;
        let (data, coverage) = area.finish();
        let (format, data) = match coverage {
            // The data ended early: rows missing count as transparent.
            Some(coverage) if !plane.format.has_alpha() => {
                let c = plane.format.channels();
                let row = (w[2] - w[0]) as usize;
                let mut out = Vec::with_capacity(data.len() / c * (c + 1));
                for (texels, a) in data.chunks_exact(row * c).zip(coverage) {
                    for texel in texels.chunks_exact(c) {
                        out.extend_from_slice(texel);
                        out.push(a);
                    }
                }
                (plane.format.with_alpha(), out)
            }
            _ => (plane.format, data),
        };
        let to_image = Affine::scale_non_uniform(
            plane.width as f64 * plane.to_image.0 / grid.0 as f64,
            plane.height as f64 * plane.to_image.1 / grid.1 as f64,
        ) * Affine::translate((w[0] as f64, w[1] as f64));
        Ok(ImageRegion {
            format,
            data,
            window: w,
            grid,
            to_image,
            rows_decoded,
            bytes_inflated,
        })
    }
}

/// Polls `cancel` every this many rows.
const CANCEL_ROWS: u32 = 64;

fn cancelled(cancel: Option<&dyn Fn() -> bool>, row: u32) -> bool {
    row.is_multiple_of(CANCEL_ROWS) && cancel.is_some_and(|c| c())
}

/// One asset's index has a fixed residency ceiling. Checkpoint spacing grows with
/// the declared height so the index covers the source rather than only its tail.
const CHECKPOINT_BUDGET: usize = 2 * 1024 * 1024;
const MIN_CHECKPOINT_BYTES: usize = 256 * 1024;

struct IndexedCheckpoint {
    row: u32,
    state: DecodedCheckpoint,
}

#[derive(Default)]
struct Checkpoints {
    generation: u64,
    entries: Vec<IndexedCheckpoint>,
}

impl Checkpoints {
    fn allocation_size(&self) -> usize {
        size_of::<Mutex<Self>>()
            + self.entries.capacity() * size_of::<IndexedCheckpoint>()
            + self
                .entries
                .iter()
                .map(|e| e.state.allocation_size() - size_of::<DecodedCheckpoint>())
                .sum::<usize>()
    }

    fn interval(
        &mut self,
        reader: &DecodedReader<'_>,
        row_bytes: usize,
        height: u32,
    ) -> Option<u32> {
        let state_size = reader.checkpoint_size()?;
        let entry_size =
            state_size + size_of::<IndexedCheckpoint>() - size_of::<DecodedCheckpoint>();
        let count = (CHECKPOINT_BUDGET - size_of::<Mutex<Self>>()) / entry_size;
        if count == 0 {
            return None;
        }
        let interval = height
            .div_ceil(count as u32)
            .max(MIN_CHECKPOINT_BYTES.div_ceil(row_bytes.max(1)) as u32)
            .max(1);
        let slots = (height / interval) as usize;
        if slots == 0 {
            return None;
        }
        if self.entries.capacity() == 0 {
            self.entries.reserve_exact(slots);
        }
        Some(interval)
    }

    fn invalidate(&mut self) {
        self.entries.clear();
        self.generation += 1;
    }

    fn restore(&self, raw: &mut RawRows<'_>, before: u32) -> Result<u32, Stop> {
        let Some(entry) = self.entries.iter().rev().find(|e| e.row <= before) else {
            return Ok(0);
        };
        raw.reader.restore(&entry.state).map_err(|_| Stop::Failed)?;
        raw.total = entry.row as usize * raw.row.len();
        Ok(entry.row)
    }

    fn save(
        &mut self,
        row: u32,
        reader: &DecodedReader<'_>,
        owner: &OwnedStream,
        generation: u64,
    ) -> Result<(), Stop> {
        // A concurrent reader may already have rejected this decoder generation.
        if generation != self.generation {
            return Ok(());
        }
        let at = match self.entries.binary_search_by_key(&row, |e| e.row) {
            Ok(_) => return Ok(()),
            Err(at) => at,
        };
        // A full index may repeat work, but it must never lower image detail.
        if self.entries.len() == self.entries.capacity() {
            return Ok(());
        }
        let Some(state) = reader.checkpoint(owner).map_err(|_| Stop::Failed)? else {
            return Ok(());
        };
        self.entries.insert(at, IndexedCheckpoint { row, state });
        debug_assert!(self.allocation_size() <= CHECKPOINT_BUDGET);
        Ok(())
    }
}

/// Rows of an image's or mask's stored samples, read incrementally.
struct RawRows<'a> {
    reader: DecodedReader<'a>,
    row: Vec<u8>,
    /// Bytes read since the start.
    total: usize,
}

/// The outcome of reading a row.
enum RawRow {
    Full,
    /// The data ended within the row after this many bytes.
    Partial(usize),
    End,
}

impl<'a> RawRows<'a> {
    fn new(stream: &Stream<'a>, row_bytes: usize) -> Option<Self> {
        Some(Self {
            reader: stream.decoded_reader()?,
            row: vec![0; row_bytes],
            total: 0,
        })
    }

    /// Back to the first row, keeping the decoder a restart chose.
    fn rewind(&mut self) {
        self.reader.rewind();
        self.total = 0;
    }

    fn next(&mut self) -> Result<RawRow, ReadError> {
        let mut filled = 0;
        while filled < self.row.len() {
            match self.reader.read(&mut self.row[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => {
                    self.total = 0;
                    return Err(e);
                }
            }
        }
        self.total += filled;
        Ok(if filled == self.row.len() {
            RawRow::Full
        } else if filled == 0 {
            RawRow::End
        } else {
            RawRow::Partial(filled)
        })
    }
}

/// Whether the decoder keeps a last row the data fills in part, `samples` of the
/// samples decoded in all (`rows_present`).
fn keeps_partial_row(samples: usize, part: usize, row_len: usize) -> bool {
    rows_present(samples, row_len, u32::MAX) as usize > samples / row_len && part < row_len
}

/// A mask's rows, as alpha.
enum MaskRows<'a, 'm> {
    Streamed {
        raw: Box<RawRows<'a>>,
        path: MaskPath,
        width: usize,
        height: u32,
        row: u32,
        ended: bool,
    },
    Decoded {
        luma: &'m LumaData,
        row: u32,
    },
}

impl MaskRows<'_, '_> {
    /// Back to the first row: after its reader restarted, or with `reader`, after
    /// another reader of the pass did.
    fn rewind(&mut self, reader: bool) {
        match self {
            MaskRows::Decoded { row, .. } => *row = 0,
            MaskRows::Streamed {
                raw, row, ended, ..
            } => {
                if reader {
                    raw.rewind();
                }
                *row = 0;
                *ended = false;
            }
        }
    }

    /// Writes the next row's alpha into `out` (the mask's width); false once the
    /// mask's rows are exhausted (the row is then transparent).
    fn next(&mut self, out: &mut [u8]) -> Result<bool, ReadError> {
        match self {
            MaskRows::Decoded { luma, row } => {
                let w = luma.width as usize;
                let present = *row < luma.height;
                if present {
                    let at = *row as usize * w;
                    out.copy_from_slice(&luma.data[at..at + w]);
                } else {
                    out.fill(0);
                }
                *row += 1;
                Ok(present)
            }
            MaskRows::Streamed {
                raw,
                path,
                width,
                height,
                row,
                ended,
            } => {
                if *ended || *row >= *height {
                    out.fill(0);
                    return Ok(false);
                }
                *row += 1;
                let (bytes, last) = match raw.next()? {
                    RawRow::Full => (raw.row.len(), false),
                    RawRow::End => {
                        *ended = true;
                        out.fill(0);
                        return Ok(false);
                    }
                    RawRow::Partial(n) => (n, true),
                };
                // Samples as `decode_mask_data` counts them before padding.
                let (texels, samples) = match path {
                    MaskPath::Bilevel { .. } => {
                        let full_bytes = *width / 8;
                        let texels = if last {
                            bytes.min(full_bytes) * 8
                        } else {
                            *width
                        };
                        let before = (raw.total - bytes) / raw.row.len() * *width;
                        (texels, before + texels)
                    }
                    _ => (bytes, raw.total),
                };
                if last {
                    *ended = true;
                    if !keeps_partial_row(samples, texels, *width) {
                        out.fill(0);
                        return Ok(false);
                    }
                }
                match *path {
                    MaskPath::Bilevel { invert } => {
                        let xor = if invert { u64::MAX } else { 0 };
                        let lut = &**BILEVEL_MASK_LUT;
                        for (chunk, &byte) in out.chunks_mut(8).zip(&raw.row[..bytes]) {
                            let expanded = (lut[byte as usize] ^ xor).to_ne_bytes();
                            chunk.copy_from_slice(&expanded[..chunk.len()]);
                        }
                    }
                    MaskPath::Bytes { invert } => {
                        for (a, &v) in out.iter_mut().zip(&raw.row[..bytes]) {
                            *a = if invert { 255 - v } else { v };
                        }
                    }
                    MaskPath::Decoded => unreachable!("only fast paths stream"),
                }
                // Padding is zero, after inversion.
                out[texels..].fill(0);
                Ok(true)
            }
        }
    }
}

/// A mask, read incrementally or decoded whole.
enum MaskSource<'a> {
    Streamed {
        obj: ImageXObject<'a>,
        path: MaskPath,
    },
    Decoded(LumaData),
}

impl<'a> MaskSource<'a> {
    /// The mask in `stream`, as `decode_alpha` reads a soft mask or a mask stream.
    fn new(stream: &Stream<'a>, obj: &ImageXObject<'a>) -> Option<Self> {
        let mask = ImageXObject::new_mask(stream, &obj.warning_sink, &obj.cache)?;
        let SampleFormat {
            color_space,
            bits_per_component,
            decode_arr,
        } = sample_format(&mask, None);
        let path = MaskPath::new(
            &color_space,
            bits_per_component,
            &decode_arr,
            mask.kind == ImageKind::StencilMask,
        );
        if path != MaskPath::Decoded && mask.stream.can_read_incrementally() {
            return Some(Self::Streamed { obj: mask, path });
        }
        decode_mask(&mask, None).map(|m| Self::Decoded(m.luma))
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Self::Streamed { obj, .. } => (obj.width, obj.height),
            Self::Decoded(luma) => (luma.width, luma.height),
        }
    }

    fn interpolate(&self) -> bool {
        match self {
            Self::Streamed { obj, .. } => obj.interpolate,
            Self::Decoded(luma) => luma.interpolate,
        }
    }

    fn rows(&self) -> Result<MaskRows<'a, '_>, RequestError> {
        Ok(match self {
            Self::Streamed { obj, path } => {
                let width = obj.width as usize;
                let row_bytes = match path {
                    MaskPath::Bilevel { .. } => width.div_ceil(8),
                    _ => width,
                };
                MaskRows::Streamed {
                    raw: Box::new(
                        RawRows::new(&obj.stream, row_bytes).ok_or(RequestError::Failed)?,
                    ),
                    path: *path,
                    width,
                    height: obj.height,
                    row: 0,
                    ended: false,
                }
            }
            Self::Decoded(luma) => MaskRows::Decoded { luma, row: 0 },
        })
    }
}

/// Where an image's alpha comes from (`decode_alpha`, in its order of precedence).
enum Alpha<'a> {
    None,
    /// A soft mask or mask stream; with `/Matte`, the colour is premultiplied
    /// against it, which is undone row by row.
    Mask {
        mask: MaskSource<'a>,
        matte: Option<[u8; 3]>,
        same_grid: bool,
    },
    /// A colour-key mask: samples within these ranges are transparent.
    ColourKey(SmallVec<[u16; 4]>),
}

/// An image read incrementally.
struct Streamed<'a> {
    owner: &'a OwnedStream,
    checkpoints: &'a Mutex<Checkpoints>,
    obj: ImageXObject<'a>,
    color_space: ColorSpace,
    /// Components per stored sample.
    components: usize,
    invert: bool,
    /// Whether the colour comes out as luma, not RGB.
    luma: bool,
    alpha: Alpha<'a>,
}

impl<'a> Streamed<'a> {
    fn new(
        obj: &ImageXObject<'a>,
        owner: &'a OwnedStream,
        checkpoints: &'a Mutex<Checkpoints>,
    ) -> Option<Self> {
        if !obj.stream.can_read_incrementally() {
            return None;
        }
        let SampleFormat {
            color_space,
            bits_per_component,
            decode_arr,
        } = sample_format(obj, None);
        if bits_per_component != 8 || !color_space.is_device_gray_or_rgb() {
            return None;
        }
        let invert = direct_invert(&color_space, bits_per_component, &decode_arr)?;
        let components = color_space.num_components() as usize;
        let luma = obj
            .transfer_function
            .as_ref()
            .is_none_or(|t| matches!(t, ActiveTransferFunction::Single(_)))
            && color_space.to_luma(&mut []).is_some();

        let dict = obj.stream.dict();
        let size = (obj.width, obj.height);
        let smask = dict.get::<Stream<'_>>(SMASK);
        // `/Matte` applies when the soft mask has the image's size.
        let matte = smask.as_ref().and_then(|s| {
            let matte = s.dict().get::<ColorComponents>(MATTE)?;
            if matte.len() != components {
                return None;
            }
            let mask = MaskSource::new(s, obj)?;
            if mask.size() != size {
                return None;
            }
            let mut rgb = [0; 3];
            color_space.convert_values(&matte, &mut rgb);
            Some((mask, rgb))
        });
        let same =
            |mask: &MaskSource<'_>| mask.size() == size && mask.interpolate() == obj.interpolate;
        let alpha = if let Some((mask, rgb)) = matte {
            Alpha::Mask {
                same_grid: same(&mask),
                mask,
                matte: Some(rgb),
            }
        } else if dict.get::<u8>(SMASK_IN_DATA) == Some(1) {
            // Only JPX data holds alpha; for other data there is none.
            Alpha::None
        } else if let Some(s) = smask.or_else(|| dict.get::<Stream<'_>>(MASK)) {
            match MaskSource::new(&s, obj) {
                Some(mask) => Alpha::Mask {
                    same_grid: same(&mask),
                    mask,
                    matte: None,
                },
                None => Alpha::None,
            }
        } else if let Some(key) = dict.get::<SmallVec<[u16; 4]>>(MASK) {
            Alpha::ColourKey(key)
        } else {
            Alpha::None
        };

        Some(Self {
            obj: obj.clone(),
            owner,
            checkpoints,
            color_space,
            components,
            invert,
            luma,
            alpha,
        })
    }

    fn layout(&self) -> ImageLayout {
        let (w, h) = (self.obj.width, self.obj.height);
        let alpha = matches!(
            self.alpha,
            Alpha::ColourKey(_)
                | Alpha::Mask {
                    same_grid: true,
                    ..
                }
        );
        let format = match (self.luma, alpha) {
            (true, false) => PixelFormat::Luma,
            (true, true) => PixelFormat::LumaAlpha,
            (false, false) => PixelFormat::Rgb,
            (false, true) => PixelFormat::RgbAlpha,
        };
        let mask = match &self.alpha {
            Alpha::Mask {
                mask,
                same_grid: false,
                ..
            } => {
                let (mw, mh) = mask.size();
                Some(PlaneLayout {
                    width: mw,
                    height: mh,
                    to_image: (w as f64 / mw as f64, h as f64 / mh as f64),
                    interpolate: mask.interpolate(),
                    format: PixelFormat::Alpha,
                })
            }
            _ => None,
        };
        ImageLayout {
            image: PlaneLayout {
                width: w,
                height: h,
                to_image: (1.0, 1.0),
                interpolate: self.obj.interpolate,
                format,
            },
            mask,
        }
    }

    /// Streams `plane` into `area`; returns the rows decoded. A reader that restarts
    /// (`ReadError::Restarted`: its data failed to decode and it now reads the data
    /// again from the start, another way) restarts the pass, with the other readers
    /// opened again.
    fn run(
        &self,
        plane: Plane,
        area: &mut Area,
        cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<(u32, u64), RequestError> {
        let result = match plane {
            Plane::Image => self.run_image(area, cancel),
            Plane::Mask => self.run_mask(area, cancel),
        };
        result.map_err(|stop| match stop {
            Stop::Cancelled => RequestError::Cancelled,
            Stop::Failed => RequestError::Failed,
        })
    }

    /// The mask read in lockstep with the colour: on its grid, or with `/Matte`.
    fn lockstep_mask(&self) -> Result<Option<Lockstep<'a, '_>>, Stop> {
        Ok(match &self.alpha {
            Alpha::Mask {
                mask,
                matte,
                same_grid,
            } if *same_grid || matte.is_some() => Some(Lockstep {
                rows: mask.rows()?,
                matte: *matte,
                same_grid: *same_grid,
            }),
            _ => None,
        })
    }

    fn run_image(
        &self,
        area: &mut Area,
        cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<(u32, u64), Stop> {
        let (w, h) = (self.obj.width as usize, self.obj.height);
        let n = self.components;
        let row_len = w * n;
        let mut raw = RawRows::new(&self.obj.stream, row_len).ok_or(Stop::Failed)?;
        let mut mask = self.lockstep_mask()?;
        let indexed = mask.is_none() && raw.reader.checkpoint_size().is_some();
        let (rows, cols) = (area.rows(), area.columns());
        let (c0, c1) = (cols.start as usize, cols.end as usize);
        let mut alpha_row = vec![0; if mask.is_some() { w } else { 0 }];
        let out_channels = if self.luma { 1 } else { 3 };
        let has_alpha = matches!(
            self.alpha,
            Alpha::ColourKey(_)
                | Alpha::Mask {
                    same_grid: true,
                    ..
                }
        );
        let mut colour = Vec::with_capacity((c1 - c0) * 3);
        let mut rgb = Vec::new();
        let mut work = Vec::with_capacity((c1 - c0) * 4);
        let mut key_alpha = Vec::new();
        let mut decoded = 0;
        let (interval, mut generation, mut start) = if indexed {
            let mut index = self.checkpoints.lock().map_err(|_| Stop::Failed)?;
            let interval = index.interval(&raw.reader, row_len, h);
            let start = index.restore(&mut raw, rows.start)?;
            (interval, index.generation, start)
        } else {
            (None, 0, 0)
        };
        'pass: loop {
            let first = std::mem::take(&mut start);
            for y in first..rows.end.min(h) {
                if cancelled(cancel, y - first) {
                    return Err(Stop::Cancelled);
                }
                let last = match raw.next() {
                    Err(ReadError::Restarted) => {
                        area.reset();
                        if indexed {
                            let mut index = self.checkpoints.lock().map_err(|_| Stop::Failed)?;
                            index.invalidate();
                            generation = index.generation;
                        }
                        if let Some(m) = &mut mask {
                            m.rows.rewind(true);
                        }
                        continue 'pass;
                    }
                    Err(ReadError::Failed) => return Err(Stop::Failed),
                    Ok(RawRow::Full) => false,
                    Ok(RawRow::End) => break,
                    Ok(RawRow::Partial(part)) => {
                        if !keeps_partial_row(raw.total, part, row_len) {
                            break;
                        }
                        raw.row[part..].fill(0);
                        true
                    }
                };
                decoded += 1;
                if !last && interval.is_some_and(|step| (y + 1).is_multiple_of(step)) {
                    self.checkpoints.lock().map_err(|_| Stop::Failed)?.save(
                        y + 1,
                        &raw.reader,
                        self.owner,
                        generation,
                    )?;
                }
                let mask_present = match &mut mask {
                    Some(m) => match m.rows.next(&mut alpha_row) {
                        Err(ReadError::Restarted) => {
                            area.reset();
                            raw.rewind();
                            m.rows.rewind(false);
                            continue 'pass;
                        }
                        Err(ReadError::Failed) => return Err(Stop::Failed),
                        Ok(present) => present,
                    },
                    None => false,
                };
                if y < rows.start {
                    if last {
                        break;
                    }
                    continue;
                }
                let samples = &raw.row[c0 * n..c1 * n];
                if let Alpha::ColourKey(key) = &self.alpha {
                    key_alpha.clear();
                    key_alpha.extend(samples.chunks_exact(n).map(|pixel| {
                        let outside = pixel
                            .iter()
                            .zip(key.as_chunks::<2>().0)
                            .any(|(&c, r)| u16::from(c) > r[1] || u16::from(c) < r[0]);
                        if outside { 255 } else { 0 }
                    }));
                }
                colour.clear();
                colour.extend_from_slice(samples);
                if self.invert {
                    colour.iter_mut().for_each(|v| *v = 255 - *v);
                }
                if self.luma {
                    self.color_space.to_luma(&mut colour);
                } else if self.color_space.convert_in_place(&mut colour).is_none() {
                    rgb.clear();
                    rgb.resize((c1 - c0) * 3, 0);
                    self.color_space
                        .convert(&colour, &mut rgb)
                        .ok_or(Stop::Failed)?;
                    std::mem::swap(&mut colour, &mut rgb);
                }
                if let Some(t) = &self.obj.transfer_function {
                    t.apply_to(&mut colour);
                }
                let alpha: Option<&[u8]> = match (&mask, &self.alpha) {
                    (Some(m), _) => {
                        let a = &alpha_row[c0..c1];
                        if let Some(matte) = &m.matte
                            && mask_present
                        {
                            unpremultiply_samples(&mut colour, out_channels, a, matte);
                        }
                        m.same_grid.then_some(a)
                    }
                    (None, Alpha::ColourKey(_)) => Some(&key_alpha),
                    _ => None,
                };
                match alpha {
                    Some(alpha) if has_alpha => {
                        work.clear();
                        for (texel, &a) in colour.chunks_exact(out_channels).zip(alpha) {
                            work.extend(
                                texel
                                    .iter()
                                    .map(|&c| (u16::from(c) * u16::from(a) / 255) as u8),
                            );
                            work.push(a);
                        }
                        area.push(&work);
                    }
                    _ => area.push(&colour),
                }
                if last {
                    break;
                }
            }
            let mask_inflated = match &mask {
                Some(Lockstep {
                    rows: MaskRows::Streamed { raw, .. },
                    ..
                }) => raw.reader.inflated_bytes(),
                _ => 0,
            };
            return Ok((decoded, raw.reader.inflated_bytes() + mask_inflated));
        }
    }

    fn run_mask(
        &self,
        area: &mut Area,
        cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<(u32, u64), Stop> {
        let Alpha::Mask { mask, .. } = &self.alpha else {
            return Err(Stop::Failed);
        };
        let (mw, mh) = mask.size();
        let mut rows = mask.rows()?;
        let mut alpha = vec![0; mw as usize];
        let (needed, cols) = (area.rows(), area.columns());
        let mut decoded = 0;
        'pass: loop {
            for y in 0..needed.end.min(mh) {
                if cancelled(cancel, y) {
                    return Err(Stop::Cancelled);
                }
                match rows.next(&mut alpha) {
                    Err(ReadError::Restarted) => {
                        area.reset();
                        rows.rewind(false);
                        continue 'pass;
                    }
                    Err(ReadError::Failed) => return Err(Stop::Failed),
                    Ok(false) => break,
                    Ok(true) => {}
                }
                decoded += 1;
                if y >= needed.start {
                    area.push(&alpha[cols.start as usize..cols.end as usize]);
                }
            }
            let inflated = match &rows {
                MaskRows::Streamed { raw, .. } => raw.reader.inflated_bytes(),
                _ => 0,
            };
            return Ok((decoded, inflated));
        }
    }
}

/// A mask read in lockstep with the colour.
struct Lockstep<'a, 'm> {
    rows: MaskRows<'a, 'm>,
    matte: Option<[u8; 3]>,
    same_grid: bool,
}

/// Why a pass stopped.
enum Stop {
    Cancelled,
    Failed,
}

impl From<RequestError> for Stop {
    fn from(e: RequestError) -> Self {
        match e {
            RequestError::Cancelled => Self::Cancelled,
            _ => Self::Failed,
        }
    }
}

/// An image decoded whole at `open`.
struct Decoded {
    image: ImageData,
    alpha: Option<LumaData>,
    /// Whether `alpha` shares the colour's grid (the colour is then premultiplied).
    same_grid: bool,
}

impl Decoded {
    fn new(mut image: ImageData, alpha: Option<LumaData>) -> Self {
        let (w, h) = (image.width(), image.height());
        let same_grid = alpha
            .as_ref()
            .is_some_and(|a| (a.width, a.height) == (w, h) && a.interpolate == image.interpolate());
        if same_grid && let Some(a) = &alpha {
            let (data, c) = match &mut image {
                ImageData::Rgb(d) => (&mut d.data, 3),
                ImageData::Luma(d) => (&mut d.data, 1),
            };
            for (texel, &a) in data.chunks_exact_mut(c).zip(&a.data) {
                for v in texel {
                    *v = (*v as u16 * a as u16 / 255) as u8;
                }
            }
        }
        Self {
            image,
            alpha,
            same_grid,
        }
    }

    fn layout(&self) -> ImageLayout {
        let (w, h) = (self.image.width(), self.image.height());
        let (sx, sy) = self.image.scale_factors();
        let format = match (&self.image, self.same_grid) {
            (ImageData::Luma(_), false) => PixelFormat::Luma,
            (ImageData::Luma(_), true) => PixelFormat::LumaAlpha,
            (ImageData::Rgb(_), false) => PixelFormat::Rgb,
            (ImageData::Rgb(_), true) => PixelFormat::RgbAlpha,
        };
        ImageLayout {
            image: PlaneLayout {
                width: w,
                height: h,
                to_image: (sx as f64, sy as f64),
                interpolate: self.image.interpolate(),
                format,
            },
            mask: self
                .alpha
                .as_ref()
                .filter(|_| !self.same_grid)
                .map(|a| PlaneLayout {
                    width: a.width,
                    height: a.height,
                    to_image: (
                        sx as f64 * w as f64 / a.width as f64,
                        sy as f64 * h as f64 / a.height as f64,
                    ),
                    interpolate: a.interpolate,
                    format: PixelFormat::Alpha,
                }),
        }
    }

    fn run(
        &self,
        plane: Plane,
        area: &mut Area,
        cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<u32, RequestError> {
        let (rows, cols) = (area.rows(), area.columns());
        let (c0, c1) = (cols.start as usize, cols.end as usize);
        let (data, c, w) = match (plane, &self.image) {
            (Plane::Mask, _) => {
                let a = self.alpha.as_ref().ok_or(RequestError::Invalid)?;
                (&a.data, 1, a.width as usize)
            }
            (Plane::Image, ImageData::Rgb(d)) => (&d.data, 3, d.width as usize),
            (Plane::Image, ImageData::Luma(d)) => (&d.data, 1, d.width as usize),
        };
        let alpha = (plane == Plane::Image && self.same_grid)
            .then_some(self.alpha.as_ref())
            .flatten();
        let mut work = Vec::with_capacity((c1 - c0) * (c + 1));
        for y in rows.clone() {
            if cancelled(cancel, y) {
                return Err(RequestError::Cancelled);
            }
            let at = y as usize * w;
            let texels = &data[(at + c0) * c..(at + c1) * c];
            match alpha {
                Some(a) => {
                    work.clear();
                    for (texel, &a) in texels.chunks_exact(c).zip(&a.data[at + c0..at + c1]) {
                        work.extend_from_slice(texel);
                        work.push(a);
                    }
                    area.push(&work);
                }
                None => area.push(texels),
            }
        }
        Ok(rows.end - rows.start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use hayro_syntax::Pdf;
    use hayro_syntax::object::ObjectIdentifier;

    /// A PDF whose objects 3 onwards are `objects`, each a dictionary and its stream
    /// data.
    fn pdf(objects: &[(String, Vec<u8>)]) -> Pdf {
        let mut objs = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
        ];
        objs.extend(objects.iter().map(|(dict, data)| {
            let mut s = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            s.extend_from_slice(data);
            s.extend_from_slice(b"\nendstream");
            s
        }));
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n", i + 1).bytes());
            out.extend_from_slice(o);
            out.extend(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
        for o in offsets {
            out.extend(format!("{o:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .bytes(),
        );
        Pdf::new(out).unwrap()
    }

    fn image(dict: &str, w: u32, h: u32) -> String {
        format!("/Type /XObject /Subtype /Image /Width {w} /Height {h} {dict}")
    }

    /// `data` as a zlib stream of stored blocks.
    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        out.extend(deflate_stored(data));
        let (mut a, mut b) = (1_u32, 0_u32);
        for &x in data {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        out.extend((b << 16 | a).to_be_bytes());
        out
    }

    fn deflate_stored(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let chunks: Vec<&[u8]> = data.chunks(1000).collect();
        for (i, c) in chunks.iter().enumerate() {
            out.push(u8::from(i + 1 == chunks.len()));
            let len = c.len() as u16;
            out.extend(len.to_le_bytes());
            out.extend((!len).to_le_bytes());
            out.extend_from_slice(c);
        }
        if chunks.is_empty() {
            out.extend([1, 0, 0, 0xff, 0xff]);
        }
        out
    }

    /// `rows` of `row_len` bytes PNG-filtered (cycling None, Sub, Up, Average, Paeth)
    /// with `bpp` bytes per pixel.
    fn png_filter(data: &[u8], row_len: usize, bpp: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let zero = vec![0; row_len];
        for (r, row) in data.chunks(row_len).enumerate() {
            let prev = if r == 0 {
                &zero[..]
            } else {
                &data[(r - 1) * row_len..r * row_len]
            };
            let filter = (r % 5) as u8;
            out.push(filter);
            for i in 0..row.len() {
                let a = if i >= bpp { row[i - bpp] as i16 } else { 0 };
                let b = prev[i] as i16;
                let c = if i >= bpp { prev[i - bpp] as i16 } else { 0 };
                let p = match filter {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => (a + b) / 2,
                    _ => {
                        let p = a + b - c;
                        let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                        if pa <= pb && pa <= pc {
                            a
                        } else if pb <= pc {
                            b
                        } else {
                            c
                        }
                    }
                };
                out.push(row[i].wrapping_sub(p as u8));
            }
        }
        out
    }

    fn noise(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2654435761).max(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 24) as u8
            })
            .collect()
    }

    fn sink() -> WarningSinkFn {
        Arc::new(|_| {})
    }

    fn asset(pdf: &Pdf, cache: &Cache) -> ImageAsset {
        let stream = pdf
            .xref()
            .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
            .unwrap();
        ImageXObject::new(&stream, |_| None, &sink(), cache, None)
            .unwrap()
            .asset(None)
    }

    /// The image decoded whole, as the fallback path reads it.
    fn whole(asset: &ImageAsset) -> ImageSource<'_> {
        let obj = asset.xobject().unwrap();
        let decoded = decode_image(&obj, None).unwrap();
        let decoded = Decoded::new(decoded.image, decoded.alpha);
        ImageSource {
            layout: decoded.layout(),
            inner: Inner::Decoded(Box::new(decoded)),
        }
    }

    fn request(
        source: &mut ImageSource<'_>,
        plane: Plane,
        grid: (u32, u32),
        window: [u32; 4],
    ) -> ImageRegion {
        source
            .request(&ImageRequest {
                plane,
                grid,
                window,
                cancel: None,
            })
            .unwrap()
    }

    /// Streamed requests of every plane, at several grids and windows, equal the same
    /// requests of the image decoded whole.
    fn assert_streams_as_decoded(objects: &[(String, Vec<u8>)]) {
        let pdf = pdf(objects);
        let cache = Cache::new();
        let asset = asset(&pdf, &cache);
        let mut streamed = asset.open(None).unwrap();
        assert!(streamed.is_streamed());
        let mut decoded = whole(&asset);
        assert_eq!(streamed.layout(), decoded.layout());
        let layout = *streamed.layout();
        for plane in [Plane::Image, Plane::Mask] {
            let Some(p) = layout.plane(plane) else {
                continue;
            };
            let (w, h) = (p.width, p.height);
            for grid in [
                (w, h),
                (w.div_ceil(2), h.div_ceil(3)),
                (1, 1),
                (w, 2.min(h)),
            ] {
                for window in [
                    [0, 0, grid.0, grid.1],
                    [grid.0 / 2, grid.1 / 2, grid.0, grid.1],
                    [0, grid.1 - 1, 1.max(grid.0 / 3), grid.1],
                ] {
                    let a = request(&mut streamed, plane, grid, window);
                    let b = request(&mut decoded, plane, grid, window);
                    assert_eq!(
                        (a.format, &a.data, a.to_image),
                        (b.format, &b.data, b.to_image),
                        "{plane:?} grid {grid:?} window {window:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn reopened_flate_image_seeks_to_retained_decoder_state() {
        let (w, h) = (257_u32, 4096_u32);
        let data = noise(w as usize * h as usize, 71);
        let pdf = pdf(&[(
            image(
                "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
                w,
                h,
            ),
            zlib(&data),
        )]);
        let asset = asset(&pdf, &Cache::new());
        let start = std::time::Instant::now();
        let mut cold = asset.open(None).unwrap();
        let a = request(&mut cold, Plane::Image, (w, h), [0, 3000, w, 3016]);
        let cold_time = start.elapsed();
        assert_eq!(a.data, data[3000 * w as usize..3016 * w as usize]);
        assert_eq!(a.bytes_inflated, u64::from(3016 * w));
        drop(cold);
        let start = std::time::Instant::now();
        let mut warm = asset.open(None).unwrap();
        let b = request(&mut warm, Plane::Image, (w, h), [0, 2800, w, 2816]);
        assert_eq!(b.data, data[2800 * w as usize..2816 * w as usize]);
        let warm_time = start.elapsed();
        assert!(
            b.rows_decoded < 1024,
            "warm request repeated {} rows",
            b.rows_decoded
        );
        assert_eq!(b.bytes_inflated, u64::from(b.rows_decoded * w));
        assert!(b.bytes_inflated < (MIN_CHECKPOINT_BYTES + 16 * w as usize) as u64);
        assert!(warm.checkpoint_bytes().unwrap() <= CHECKPOINT_BUDGET);
        println!(
            "reopened cold={cold_time:?} rows={} inflated={} warm={warm_time:?} rows={} inflated={} checkpoint_bytes={}",
            a.rows_decoded,
            a.bytes_inflated,
            b.rows_decoded,
            b.bytes_inflated,
            warm.checkpoint_bytes().unwrap()
        );
        let mut decoded = whole(&asset);
        for (grid, window) in [
            ((w, h), [3, 3077, 200, 3091]),
            ((31, 511), [1, 222, 27, 233]),
            ((w, h), [0, 21, w, 41]),
        ] {
            let actual = request(&mut warm, Plane::Image, grid, window);
            let expected = request(&mut decoded, Plane::Image, grid, window);
            assert_eq!(
                (actual.format, actual.data),
                (expected.format, expected.data)
            );
        }
        drop(warm);
        drop(decoded);
        let weak = Arc::downgrade(&asset.0);
        drop(asset);
        assert!(
            weak.upgrade().is_none(),
            "checkpoint ownership must not retain the image asset"
        );
    }

    #[test]
    fn checkpoint_budget_and_eviction_preserve_pixels() {
        let (w, h) = (1024_u32, 20_000_u32);
        let data = vec![71; w as usize * h as usize];
        let pdf = pdf(&[(
            image(
                "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
                w,
                h,
            ),
            zlib(&data),
        )]);
        let asset = asset(&pdf, &Cache::new());
        let mut source = asset.open(None).unwrap();
        let first = request(&mut source, Plane::Image, (w, h), [0, h - 2, w, h]);
        assert_eq!(first.data, vec![71; w as usize * 2]);
        assert!(source.checkpoint_bytes().unwrap() <= CHECKPOINT_BUDGET);
        assert!(asset.0.checkpoints.lock().unwrap().entries.len() > 2);
        let warm = request(&mut source, Plane::Image, (w, h), [0, h - 9, w, h - 7]);
        asset.0.checkpoints.lock().unwrap().entries.clear();
        let evicted = request(&mut source, Plane::Image, (w, h), [0, h - 9, w, h - 7]);
        assert_eq!(warm.data, evicted.data);
        assert!(warm.bytes_inflated < evicted.bytes_inflated);
        assert!(source.checkpoint_bytes().unwrap() <= CHECKPOINT_BUDGET);
    }

    #[test]
    fn concurrent_views_and_cancel_callbacks_do_not_hold_the_index_lock() {
        let (w, h) = (257_u32, 4096_u32);
        let data = noise(w as usize * h as usize, 71);
        let pdf = pdf(&[(
            image(
                "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
                w,
                h,
            ),
            zlib(&data),
        )]);
        let asset = asset(&pdf, &Cache::new());
        let mut source = asset.open(None).unwrap();
        request(&mut source, Plane::Image, (w, h), [0, 3000, w, 3016]);
        // Checkpoint rows are not aligned to the cancellation polling cadence.
        let row = asset.0.checkpoints.lock().unwrap().entries[0].row;
        let cancel = || {
            assert!(asset.checkpoint_bytes().unwrap() <= CHECKPOINT_BUDGET);
            true
        };
        assert_eq!(
            source
                .request(&ImageRequest {
                    plane: Plane::Image,
                    grid: (w, h),
                    window: [0, row, w, row + 1],
                    cancel: Some(&cancel)
                })
                .unwrap_err(),
            RequestError::Cancelled
        );
        std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                let mut source = asset.open(None).unwrap();
                request(&mut source, Plane::Image, (w, h), [0, 2800, w, 2816]).data
            });
            let b = scope.spawn(|| {
                let mut source = asset.open(None).unwrap();
                request(&mut source, Plane::Image, (w, h), [0, 3100, w, 3116]).data
            });
            assert_eq!(
                a.join().unwrap(),
                data[2800 * w as usize..2816 * w as usize]
            );
            assert_eq!(
                b.join().unwrap(),
                data[3100 * w as usize..3116 * w as usize]
            );
        });
    }

    #[test]
    fn a_reader_from_a_rejected_generation_cannot_repopulate_the_index() {
        let (w, h) = (257_u32, 4096_u32);
        let data = noise(w as usize * h as usize, 71);
        let pdf = pdf(&[(
            image(
                "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
                w,
                h,
            ),
            zlib(&data),
        )]);
        let asset = asset(&pdf, &Cache::new());
        let mut raw = RawRows::new(&asset.xobject().unwrap().stream, w as usize).unwrap();
        let mut index = asset.0.checkpoints.lock().unwrap();
        let interval = index.interval(&raw.reader, w as usize, h).unwrap();
        let generation = index.generation;
        for _ in 0..interval {
            assert!(matches!(raw.next().unwrap(), RawRow::Full));
        }
        assert!(
            index
                .save(interval, &raw.reader, &asset.0.stream, generation)
                .is_ok()
        );
        index.invalidate();
        assert!(
            index
                .save(interval, &raw.reader, &asset.0.stream, generation)
                .is_ok()
        );
        assert!(index.entries.is_empty());
    }

    #[test]
    fn raw_flate_checkpoints_reopen_in_the_selected_mode() {
        let (w, h) = (257_u32, 4096_u32);
        let data = noise(w as usize * h as usize, 71);
        let pdf = pdf(&[(
            image(
                "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
                w,
                h,
            ),
            deflate_stored(&data),
        )]);
        let asset = asset(&pdf, &Cache::new());
        let mut source = asset.open(None).unwrap();
        request(&mut source, Plane::Image, (w, h), [0, 3000, w, 3016]);
        drop(source);
        let mut source = asset.open(None).unwrap();
        let warm = request(&mut source, Plane::Image, (w, h), [0, 2800, w, 2816]);
        assert_eq!(warm.data, data[2800 * w as usize..2816 * w as usize]);
        assert!(warm.bytes_inflated < (MIN_CHECKPOINT_BYTES + 16 * w as usize) as u64);
    }

    #[test]
    fn a_late_restart_invalidates_the_retained_decoder_generation() {
        let (w, h) = (257_u32, 4096_u32);
        let data = noise(w as usize * h as usize, 31);
        let mut encoded = zlib(&data);
        *encoded.last_mut().unwrap() ^= 1; // Valid prefix, invalid final checksum.
        let pdf = pdf(&[(
            image(
                "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
                w,
                h,
            ),
            encoded,
        )]);
        let asset = asset(&pdf, &Cache::new());
        let mut source = asset.open(None).unwrap();
        request(&mut source, Plane::Image, (w, h), [0, 2800, w, 2816]);
        assert!(!asset.0.checkpoints.lock().unwrap().entries.is_empty());
        let mut decoded = whole(&asset);
        let expected = request(&mut decoded, Plane::Image, (w, h), [0, 4000, w, h]);
        let actual = request(&mut source, Plane::Image, (w, h), [0, 4000, w, h]);
        assert_eq!(
            (actual.format, actual.data),
            (expected.format, expected.data)
        );
        assert!(asset.0.checkpoints.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn streamed_gray_and_rgb_equal_the_whole_decode() {
        let (w, h) = (13, 9);
        let gray = noise(w * h, 1);
        let rgb = noise(w * h * 3, 2);
        for (dict, data) in [
            ("/ColorSpace /DeviceGray /BitsPerComponent 8", gray.clone()),
            (
                "/ColorSpace /DeviceGray /BitsPerComponent 8 /Decode [1 0]",
                gray.clone(),
            ),
            ("/ColorSpace /DeviceRGB /BitsPerComponent 8", rgb.clone()),
            (
                "/ColorSpace /DeviceRGB /BitsPerComponent 8 /Decode [1 0 1 0 1 0]",
                rgb.clone(),
            ),
        ] {
            assert_streams_as_decoded(&[(image(dict, w as u32, h as u32), data.clone())]);
            let flate = format!("{dict} /Filter /FlateDecode");
            assert_streams_as_decoded(&[(image(&flate, w as u32, h as u32), zlib(&data))]);
            let n = data.len() / (w * h);
            let predicted = format!(
                "{dict} /Filter /FlateDecode /DecodeParms << /Predictor 15 /Colors {n} /Columns {w} >>"
            );
            assert_streams_as_decoded(&[(
                image(&predicted, w as u32, h as u32),
                zlib(&png_filter(&data, w * n, n)),
            )]);
        }
    }

    #[test]
    fn streamed_masks_equal_the_whole_decode() {
        let (w, h) = (11_u32, 7_u32);
        let rgb = noise((w * h * 3) as usize, 3);
        let colour = "/ColorSpace /DeviceRGB /BitsPerComponent 8";
        let smask = |mw: u32, mh: u32, extra: &str| {
            (
                image(
                    &format!("/ColorSpace /DeviceGray /BitsPerComponent 8 {extra}"),
                    mw,
                    mh,
                ),
                noise((mw * mh) as usize, mw + mh),
            )
        };
        // A soft mask on the colour's grid, on another grid, and one with /Matte.
        for mask in [
            smask(w, h, ""),
            smask(5, 3, ""),
            smask(w, h, "/Matte [0.2 0.5 1]"),
        ] {
            assert_streams_as_decoded(&[
                (image(&format!("{colour} /SMask 4 0 R"), w, h), rgb.clone()),
                mask,
            ]);
        }
        // Interpolation differs: another plane, though the size is the same.
        assert_streams_as_decoded(&[
            (image(&format!("{colour} /SMask 4 0 R"), w, h), rgb.clone()),
            smask(w, h, "/Interpolate true"),
        ]);
        // A bilevel mask stream of an odd width, inverted or not.
        for decode in ["", "/Decode [1 0]"] {
            assert_streams_as_decoded(&[
                (image(&format!("{colour} /Mask 4 0 R"), w, h), rgb.clone()),
                (
                    image(&format!("/ImageMask true {decode}"), w, h),
                    noise((w.div_ceil(8) * h) as usize, 9),
                ),
            ]);
        }
        // A colour key.
        assert_streams_as_decoded(&[(
            image(&format!("{colour} /Mask [0 120 60 255 0 200]"), w, h),
            rgb.clone(),
        )]);
    }

    #[test]
    fn data_that_ends_early_leaves_the_rest_transparent() {
        let (w, h) = (10_u32, 8_u32);
        let gray = noise((w * h) as usize, 4);
        // A short last row is padded; the rows after it are missing.
        let cut = &gray[..(w * 3 + 4) as usize];
        let dict = image("/ColorSpace /DeviceGray /BitsPerComponent 8", w, h);
        for data in [cut.to_vec(), zlib(cut)] {
            let dict = if data.len() == cut.len() {
                dict.clone()
            } else {
                format!("{dict} /Filter /FlateDecode")
            };
            let pdf = pdf(&[(dict, data)]);
            let cache = Cache::new();
            let asset = asset(&pdf, &cache);
            let mut streamed = asset.open(None).unwrap();
            let mut decoded = whole(&asset);
            assert_eq!(decoded.layout().image.height, 4);
            let a = request(&mut streamed, Plane::Image, (w, h), [0, 0, w, h]);
            let b = request(&mut decoded, Plane::Image, (w, 4), [0, 0, w, 4]);
            assert_eq!(a.format, PixelFormat::LumaAlpha);
            let (present, missing) = a.data.split_at((w * 4 * 2) as usize);
            let expected: Vec<u8> = b.data.iter().flat_map(|&v| [v, 255]).collect();
            assert_eq!(present, expected);
            assert!(missing.iter().all(|&v| v == 0));
            assert_eq!(a.rows_decoded, 4);
            // Bins over present and missing rows are covered in part.
            let half = request(&mut streamed, Plane::Image, (1, 2), [0, 0, 1, 2]);
            assert_eq!(half.data[1], 255);
            assert_eq!(half.data[3], 0);
        }
    }

    #[test]
    fn a_huge_declared_image_with_little_data_costs_the_data() {
        // The short row is kept (padding it takes under `MIN_ROW_PAD`); a
        // whole decode would hold `w` bytes, a request holds its bins.
        let (w, h) = (60_000, 1_000_000);
        let pdf = pdf(&[(
            image("/ColorSpace /DeviceGray /BitsPerComponent 8", w, h),
            vec![7; 100],
        )]);
        let cache = Cache::new();
        let asset = asset(&pdf, &cache);
        let mut source = asset.open(None).unwrap();
        let region = request(&mut source, Plane::Image, (100, 100), [0, 0, 100, 100]);
        assert_eq!(region.format, PixelFormat::LumaAlpha);
        assert_eq!(region.rows_decoded, 1);
        assert!(region.data.chunks(2).all(|t| t == [0, 0]));
        let region = request(&mut source, Plane::Image, (w, h), [0, 0, 4, 2]);
        assert_eq!(
            region.data,
            [7, 255, 7, 255, 7, 255, 7, 255, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn data_that_is_not_zlib_restarts_as_raw_deflate() {
        let (w, h) = (9_u32, 5_u32);
        let gray = noise((w * h) as usize, 5);
        assert_streams_as_decoded(&[(
            image(
                "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
                w,
                h,
            ),
            deflate_stored(&gray),
        )]);
    }

    /// Colour and soft mask both raw deflate: each restarts once, and a restart of one
    /// rewinds the other without making it restart again.
    #[test]
    fn colour_and_mask_that_both_restart_finish() {
        let (w, h) = (9_u32, 5_u32);
        let flate = "/BitsPerComponent 8 /Filter /FlateDecode";
        assert_streams_as_decoded(&[
            (
                image(
                    &format!("/ColorSpace /DeviceRGB {flate} /SMask 4 0 R"),
                    w,
                    h,
                ),
                deflate_stored(&noise((w * h * 3) as usize, 7)),
            ),
            (
                image(&format!("/ColorSpace /DeviceGray {flate}"), w, h),
                deflate_stored(&noise((w * h) as usize, 8)),
            ),
        ]);
    }

    #[test]
    fn requests_can_be_cancelled_and_are_checked() {
        let (w, h) = (4_u32, 200_u32);
        let pdf = pdf(&[(
            image("/ColorSpace /DeviceGray /BitsPerComponent 8", w, h),
            noise((w * h) as usize, 6),
        )]);
        let cache = Cache::new();
        let asset = asset(&pdf, &cache);
        let mut source = asset.open(None).unwrap();
        let polled = std::cell::Cell::new(0);
        let cancel = || {
            polled.set(polled.get() + 1);
            polled.get() > 1
        };
        let result = source.request(&ImageRequest {
            plane: Plane::Image,
            grid: (w, h),
            window: [0, 0, w, h],
            cancel: Some(&cancel),
        });
        assert_eq!(result.unwrap_err(), RequestError::Cancelled);
        for (plane, grid, window) in [
            (Plane::Mask, (w, h), [0, 0, 1, 1]),
            (Plane::Image, (w + 1, h), [0, 0, 1, 1]),
            (Plane::Image, (w, h), [0, 0, 0, 1]),
            (Plane::Image, (w, h), [0, 0, w + 1, 1]),
            (Plane::Image, (0, h), [0, 0, 0, 1]),
        ] {
            let result = source.request(&ImageRequest {
                plane,
                grid,
                window,
                cancel: None,
            });
            assert_eq!(result.unwrap_err(), RequestError::Invalid);
        }
    }
}
