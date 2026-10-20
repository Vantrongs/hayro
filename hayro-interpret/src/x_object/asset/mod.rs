//! Raster images as owned assets that decode a region of a plane at a requested
//! density, so a renderer keeps a handle instead of decoded pixels and asks for what a
//! view shows.

mod decoded;
mod index;
mod jpeg;
mod rows;
mod streamed;

use super::area::Area;
use super::decode::decode_image;
use super::image::{ImageKind, ImageXObject};
use crate::WarningSinkFn;
use crate::cache::Cache;
use crate::color::ColorSpace;
use crate::interpret::state::ActiveTransferFunction;
use decoded::Decoded;
use hayro_syntax::object::stream::OwnedStream;
use hayro_syntax::work::{Cancelled, Meter};
use index::Checkpoints;
use jpeg::JpegStreamed;
use kurbo::Affine;
use rows::{MaskHeight, PIECE_BYTES};
use std::sync::Arc;
use streamed::Streamed;

/// A raster image's source, owned: its stream (by object identifier, or a copy of an
/// inline image's bytes), resolved colour space, transfer function, interpolation
/// and masks, without the content stream or resources it was drawn from. It holds no
/// decoded pixels; [`ImageAsset::open`] reads it. Cheap to clone.
#[derive(Clone)]
pub struct ImageAsset(Arc<AssetData>);

struct AssetData {
    stream: OwnedStream,
    checkpoints: Checkpoints,
    mask_height: MaskHeight,
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
            checkpoints: Checkpoints::default(),
            mask_height: MaskHeight::new(),
            width: self.width(),
            height: self.height(),
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
    /// same key decode alike. `None` when the asset cannot be identified beyond itself.
    pub fn key(&self) -> Option<u128> {
        self.0.key
    }

    /// The bytes the decoder states saved for this asset's requests take: at most
    /// 2 MiB, whatever the image's size.
    pub fn checkpoint_bytes(&self) -> Result<usize, RequestError> {
        Ok(self.0.checkpoints.bytes()?)
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

    /// Opens the image for requests. Images with no filter or Flate, 8-bit
    /// `DeviceGray` or `DeviceRGB` samples used as stored or inverted, and masks of the
    /// same kinds or decoded whole, only read their dictionaries here (and, the first
    /// time, read such a mask's data through to count its rows); requests read their
    /// data and keep a piece of a row and a few sums per bin. A request of such a Flate
    /// image without a mask read alongside resumes from the nearest decoder state an
    /// earlier request of the asset saved, instead of from the start. So does a request
    /// of a single-scan baseline grayscale JPEG of its declared size without masks,
    /// read in bands of eight rows; once its data fails to decode, it is decoded whole
    /// like other images. Other images are decoded whole here (with `hint`, the size
    /// they are wanted at, for decoders that can reduce), and requests read the decoded
    /// planes, which the source holds until it is dropped. `cancel` is polled as
    /// [`ImageRequest::cancel`] is; a cancelled open leaves the asset as it was, to be
    /// opened again.
    pub fn open(
        &self,
        hint: Option<(u32, u32)>,
        cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<ImageSource<'_>, RequestError> {
        let meter = Meter::new(cancel);
        meter.poll()?;
        let obj = self.xobject().ok_or(RequestError::Failed)?;
        let a = &*self.0;
        let streamed = Streamed::new(&obj, &a.stream, &a.checkpoints, &a.mask_height, &meter)?;
        if let Some(streamed) = streamed {
            let (layout, opened) = (streamed.layout(), streamed.opened);
            return Ok(ImageSource::new(
                layout,
                Inner::Streamed(Box::new(streamed)),
                opened,
            ));
        }
        if let Some(jpeg) = JpegStreamed::new(&obj, &a.stream, &a.checkpoints.jpeg, &meter)? {
            let layout = jpeg.layout();
            return Ok(ImageSource::new(
                layout,
                Inner::Jpeg(Box::new(jpeg)),
                DecodeWork::default(),
            ));
        }
        let decoded = decode_image(&obj, hint, &meter);
        meter.poll()?;
        let decoded = decoded.ok_or(RequestError::Failed)?;
        let decoded = Decoded::new(decoded.image, decoded.alpha, &meter)?;
        Ok(ImageSource::new(
            decoded.layout(),
            Inner::Decoded(decoded),
            DecodeWork::default(),
        ))
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

    /// Colour in `channels`, with alpha or not.
    fn colour(channels: usize, alpha: bool) -> Self {
        match (channels, alpha) {
            (1, false) => Self::Luma,
            (1, true) => Self::LumaAlpha,
            (_, false) => Self::Rgb,
            (_, true) => Self::RgbAlpha,
        }
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
    /// Polled as the request starts and again after each piece of work (about 64 KiB
    /// of data read, decoded or resampled), whatever the image's size; returning true
    /// abandons the request, and the source stays usable for others.
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
}

/// The decoding an [`ImageSource`]'s requests have done since it was opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodeWork {
    /// Source rows decoded, including those read through to reach a window and those
    /// a restart of the decoder voided.
    pub rows_decoded: u64,
    /// Bytes the incremental Flate decoders produced (see
    /// [`DecodedReader::inflated_bytes`](hayro_syntax::object::stream::DecodedReader::inflated_bytes)).
    pub bytes_inflated: u64,
}

impl DecodeWork {
    fn add(&mut self, other: Self) {
        self.rows_decoded += other.rows_decoded;
        self.bytes_inflated += other.bytes_inflated;
    }
}

/// Why opening an image or a request of it returned nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// The plane does not exist, or the grid or window is out of bounds.
    Invalid,
    /// The `cancel` hook asked to stop.
    Cancelled,
    /// The data could not be decoded.
    Failed,
}

impl From<Cancelled> for RequestError {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}

/// An image opened for requests (see [`ImageAsset::open`]).
pub struct ImageSource<'a> {
    layout: ImageLayout,
    inner: Inner<'a>,
    work: DecodeWork,
}

enum Inner<'a> {
    Streamed(Box<Streamed<'a>>),
    Jpeg(Box<JpegStreamed<'a>>),
    Decoded(Decoded),
}

impl<'a> ImageSource<'a> {
    fn new(layout: ImageLayout, inner: Inner<'a>, work: DecodeWork) -> Self {
        Self {
            layout,
            inner,
            work,
        }
    }

    /// The image's planes.
    pub fn layout(&self) -> &ImageLayout {
        &self.layout
    }

    /// The decoding opening the image and the requests so far have done.
    pub fn work(&self) -> DecodeWork {
        self.work
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
        let meter = Meter::new(request.cancel);
        meter.poll()?;
        let mut area = Area::new(
            (plane.width, plane.height),
            grid,
            w,
            plane.format.channels(),
            &meter,
        )?;
        let work = match &mut self.inner {
            Inner::Streamed(s) => s.run(request.plane, &mut area, &meter),
            // Its layout has no mask plane.
            Inner::Jpeg(j) => j.run(&mut area, &meter),
            Inner::Decoded(d) => d.run(request.plane, &mut area).map(|rows| DecodeWork {
                rows_decoded: u64::from(rows),
                bytes_inflated: 0,
            }),
        }?;
        self.work.add(work);
        let (format, data) = with_coverage(area, plane.format, (w[2] - w[0]) as usize, &meter)?;
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
        })
    }
}

/// The bins `area` finished with, in `format`; where the data ended early, the rows
/// missing count as transparent, which adds alpha to a format without it.
fn with_coverage(
    area: Area<'_>,
    format: PixelFormat,
    row: usize,
    meter: &Meter<'_>,
) -> Result<(PixelFormat, Vec<u8>), Cancelled> {
    let (data, coverage) = area.finish()?;
    Ok(match coverage {
        Some(coverage) if !format.has_alpha() => {
            let c = format.channels();
            let mut out = Vec::with_capacity(data.len() / c * (c + 1));
            let texels = data.len() / c;
            let mut start = 0;
            while start < texels {
                let end = texels.min(start + PIECE_BYTES);
                for (t, texel) in (start..).zip(data[start * c..end * c].chunks_exact(c)) {
                    out.extend_from_slice(texel);
                    out.push(coverage[t / row]);
                }
                meter.work(end - start)?;
                start = end;
            }
            (format.with_alpha(), out)
        }
        _ => (format, data),
    })
}

/// Why a request stopped before its last row.
#[derive(Debug)]
enum Stop {
    Cancelled,
    Failed,
}

impl From<Stop> for RequestError {
    fn from(stop: Stop) -> Self {
        match stop {
            Stop::Cancelled => Self::Cancelled,
            Stop::Failed => Self::Failed,
        }
    }
}

impl From<Cancelled> for Stop {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}

/// Premultiplies texels of `channels` bytes by their `alpha`.
fn premultiply(texels: &mut [u8], channels: usize, alpha: &[u8]) {
    for (texel, &a) in texels.chunks_exact_mut(channels).zip(alpha) {
        for c in texel {
            *c = (u16::from(*c) * u16::from(a) / 255) as u8;
        }
    }
}

/// `texels` of `channels` bytes, each followed by its alpha, into `out`.
fn interleave(texels: &[u8], channels: usize, alpha: &[u8], out: &mut Vec<u8>) {
    out.clear();
    for (texel, &a) in texels.chunks_exact(channels).zip(alpha) {
        out.extend_from_slice(texel);
        out.push(a);
    }
}

#[cfg(test)]
mod tests;
