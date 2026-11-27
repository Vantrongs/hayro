mod bands;
use super::area::Area;
use super::image::samples::Samples;
use super::image::{ImageKind, ImageXObject};
use crate::ImageData;
use crate::color::ToLuma;
use crate::interpret::state::ActiveTransferFunction;
#[cfg(test)]
use crate::{WarningSinkFn, cache::Cache};
use bands::Bands;
use hayro_syntax::object::stream::OwnedStream;
use kurbo::Affine;
use std::sync::Arc;

/// Owned raster or stencil image data.
#[derive(Clone)]
pub struct ImageAsset(Arc<AssetData>);

struct AssetData {
    samples: Samples<OwnedStream>,
    key: Option<u128>,
}

impl ImageXObject<'_> {
    pub(crate) fn asset(&self, key: Option<u128>) -> ImageAsset {
        ImageAsset(Arc::new(AssetData {
            samples: Samples::read(self, &hayro_syntax::object::Stream::to_owned_stream),
            key,
        }))
    }
}

impl ImageAsset {
    /// The width of the image's pixel grid, as its dictionary declares it: the image's
    /// transform maps `0..width` × `0..height` onto the unit square.
    pub fn width(&self) -> u32 {
        self.0.samples.width
    }

    /// The height of the image's pixel grid, as its dictionary declares it.
    pub fn height(&self) -> u32 {
        self.0.samples.height
    }

    /// Identifies what the asset decodes to, as
    /// [`RasterImage::pixels_key`](crate::RasterImage::pixels_key): two assets with the
    /// same key decode alike.
    /// `None` when the asset cannot be identified beyond itself.
    pub fn key(&self) -> Option<u128> {
        self.0.key
    }

    /// Bytes copied when capturing the stream.
    pub fn copied_bytes(&self) -> usize {
        self.0.samples.stream.copied_bytes()
            + self
                .0
                .samples
                .mask
                .as_ref()
                .map_or(0, |m| m.stream.copied_bytes())
    }

    /// Opens an independent decoder for the image.
    pub fn open(&self, hint: Option<(u32, u32)>) -> Option<ImageSource> {
        self.open_with_budget(hint, |_| true).ok()
    }

    /// Checks space for whole-image samples before starting a non-streaming decode.
    pub fn open_with_budget(
        &self,
        hint: Option<(u32, u32)>,
        fits: impl Fn(usize) -> bool,
    ) -> Result<ImageSource, RequestError> {
        let obj = &self.0.samples;
        let token = Arc::default();
        if let Some(bands) = Bands::new(self, &token) {
            return Ok(ImageSource {
                asset: self.clone(),
                layout: bands.layout,
                bands: Some(bands),
                token: Some(token),
                decoded: None,
            });
        }
        let storage = |s: &Samples<OwnedStream>| {
            let channels = s
                .color_space
                .as_ref()
                .map_or(3, |cs| usize::from(cs.num_components()));
            let pixels = (s.width as usize).saturating_mul(s.height as usize);
            let native_row = (s.width as usize)
                .saturating_mul(channels)
                .saturating_mul(usize::from(s.bpc.unwrap_or(8)))
                .div_ceil(8);
            let gray = s.kind != ImageKind::Image
                || s.color_space.as_ref().is_some_and(|cs| {
                    cs.to_luma(&mut []).is_some()
                        && s.transfer_function
                            .as_ref()
                            .is_none_or(|tf| matches!(tf, ActiveTransferFunction::Single(_)))
                });
            let output = pixels.saturating_mul(if gray { 1 } else { 3 });
            native_row
                .saturating_mul(s.height as usize)
                .max(output)
                .saturating_add(if s.key.is_some() || s.in_data != 0 {
                    pixels
                } else {
                    0
                })
        };
        let bytes = storage(obj).saturating_add(obj.mask.as_deref().map_or(0, storage));
        if !fits(bytes) {
            return Err(RequestError::Budget);
        }
        let stencil = obj.kind == ImageKind::StencilMask;
        let decoded = if stencil {
            let d = super::decode::decode_mask(obj, hint).ok_or(RequestError::Failed)?;
            Decoded::new(
                ImageData::Luma(d.luma),
                None,
                d.tail.width,
                d.tail.declared,
                d.tail.texel,
            )
        } else {
            let d = super::decode::decode_image(obj, hint).ok_or(RequestError::Failed)?;
            Decoded::new(
                d.image,
                d.alpha,
                d.tail.width,
                d.tail.declared,
                d.tail.texel,
            )
        };
        let mut layout = decoded.layout();
        if stencil {
            layout.image.format = PixelFormat::Alpha;
        }
        Ok(ImageSource {
            asset: self.clone(),
            bands: None,
            token: None,
            layout,
            decoded: Some(decoded),
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
    /// other codecs).
    pub bytes_inflated: u64,
}

/// Why a request returned no bins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// The plane does not exist, or the grid or window is out of bounds.
    Invalid,
    /// The `cancel` hook asked to stop.
    Cancelled,
    /// The decoded samples exceed the available space.
    Budget,
    /// The data could not be decoded.
    Failed,
}

/// An image opened for requests (see [`ImageAsset::open`]).
pub struct ImageSource {
    layout: ImageLayout,
    asset: ImageAsset,
    bands: Option<Bands>,
    token: Option<Arc<std::sync::atomic::AtomicBool>>,
    decoded: Option<Decoded>,
}

impl ImageSource {
    /// Cancellation flag for this source's forward decoder.
    pub fn cancel_token(&self) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        self.token.clone()
    }

    /// The planes of the opened image.
    pub fn layout(&self) -> &ImageLayout {
        &self.layout
    }

    /// Whether samples are decoded on demand.
    pub fn is_streamed(&self) -> bool {
        self.decoded.is_none()
    }

    /// Bytes retained in decoder checkpoints.
    pub fn checkpoint_bytes(&self) -> usize {
        self.bands.as_ref().map_or(0, Bands::retained_bytes)
    }

    /// Sizes a request window from its decoder layout and resampling grid.
    pub fn request_scratch(
        &self,
        plane: Plane,
        grid: (u32, u32),
    ) -> Result<impl Fn([u32; 4]) -> usize + use<>, RequestError> {
        let p = *self.layout.plane(plane).ok_or(RequestError::Invalid)?;
        if grid.0 == 0 || grid.1 == 0 || grid.0 > p.width || grid.1 > p.height {
            return Err(RequestError::Invalid);
        }
        let native = self
            .bands
            .as_ref()
            .map_or([None; 2], |b| b.scratch_layout(plane));
        Ok(move |window: [u32; 4]| {
            if window[0] >= window[2]
                || window[1] >= window[3]
                || window[2] > grid.0
                || window[3] > grid.1
            {
                return usize::MAX;
            }
            let mapped = crate::area_texels((p.width, p.height), grid, window);
            let columns = (mapped[2] - mapped[0]) as usize;
            let width = (window[2] - window[0]) as usize;
            let height = (window[3] - window[1]) as usize;
            let c = p.format.channels();
            let area = Area::scratch_bytes((p.width, p.height), grid, columns, width, height, c);
            let output = width.saturating_mul(height).saturating_mul(
                p.format.with_alpha().channels() + if p.format.has_alpha() { 0 } else { c },
            );
            native
                .iter()
                .flatten()
                .fold(output.saturating_add(area), |sum, n| {
                    sum.saturating_add(n.bytes(mapped))
                })
        })
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
        if request.cancel.is_some_and(|c| c()) {
            return Err(RequestError::Cancelled);
        }
        let mut area = Area::new(
            (plane.width, plane.height),
            grid,
            w,
            plane.format.channels(),
        );
        let mut rows_decoded = 0;
        let mut bytes_inflated = 0;
        if let Some(decoded) = &self.decoded {
            rows_decoded = decoded.run(request.plane, &mut area, request.cancel)?;
        } else {
            let obj = &self.asset.0.samples;
            let rows = area.rows();
            let columns = area.columns();
            let c = plane.format.channels();
            let mut y = rows.start;
            while y < rows.end {
                if request.cancel.is_some_and(|f| f()) {
                    return Err(RequestError::Cancelled);
                }
                if self.bands.is_none() {
                    self.bands = Bands::new(
                        &self.asset,
                        self.token.as_ref().ok_or(RequestError::Failed)?,
                    );
                }
                let bands = self.bands.as_mut().ok_or(RequestError::Failed)?;
                let end = y
                    .saturating_add(bands.request_rows(request.plane) as u32)
                    .min(rows.end);
                let band = match bands.read_band(request.plane, &columns, y, end, obj) {
                    Ok(result) => result,
                    Err(error) => {
                        self.bands = None;
                        return Err(error);
                    }
                };
                let solid_tail =
                    band.data.is_some() && end < rows.end && bands.past_samples(request.plane, y);
                rows_decoded += band.rows;
                bytes_inflated += band.bytes;
                let Some(image) = band.data else {
                    continue;
                };
                if solid_tail {
                    area.fill_tail(&image[..c]);
                    break;
                }
                for row in image.chunks_exact(band.width * c) {
                    area.push_prefix(row, &row[row.len() - c..]);
                }
                y = end;
            }
        }

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

/// An image decoded whole at `open`.
struct Decoded {
    image: ImageData,
    alpha: Option<super::decode::DecodedMask>,
    /// Whether `alpha` shares the colour's grid (the colour is then premultiplied).
    same_grid: bool,
    width: u32,
    height: u32,
    fill: Option<Vec<u8>>,
}

impl Decoded {
    fn new(
        image: ImageData,
        alpha: Option<super::decode::DecodedMask>,
        width: u32,
        height: u32,
        fill: Option<Vec<u8>>,
    ) -> Self {
        let effective_height = if fill.is_some() {
            height
        } else {
            image.height()
        };
        let same_grid = alpha.as_ref().is_some_and(|a| {
            a.tail.width == width
                && a.tail.declared == effective_height
                && a.luma.interpolate == image.interpolate()
        });
        Self {
            image,
            alpha,
            same_grid,
            width,
            height,
            fill,
        }
    }

    fn layout(&self) -> ImageLayout {
        let (w, h) = (self.width, self.height);
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
            mask: self.alpha.as_ref().filter(|_| !self.same_grid).map(|a| {
                let color_h = if self.fill.is_some() {
                    h
                } else {
                    self.image.height()
                };
                PlaneLayout {
                    width: a.tail.width,
                    height: a.tail.declared,
                    to_image: (
                        sx as f64 * w as f64 / a.tail.width as f64,
                        sy as f64 * color_h as f64 / a.tail.declared as f64,
                    ),
                    interpolate: a.luma.interpolate,
                    format: PixelFormat::Alpha,
                }
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
                (&a.luma.data, 1, a.luma.width as usize)
            }
            (Plane::Image, ImageData::Rgb(d)) => (&d.data, 3, d.width as usize),
            (Plane::Image, ImageData::Luma(d)) => (&d.data, 1, d.width as usize),
        };
        let alpha = (plane == Plane::Image && self.same_grid)
            .then_some(self.alpha.as_ref())
            .flatten();
        let tail = if plane == Plane::Mask {
            self.alpha.as_ref().and_then(|a| a.tail.texel.as_deref())
        } else {
            self.fill.as_deref()
        };
        let mut work = Vec::new();
        for y in rows.clone() {
            if cancelled(cancel, y) {
                return Err(RequestError::Cancelled);
            }
            let row = |stride: usize, channels: usize, len: usize| {
                let start = y as usize * stride;
                let end = (start + c1.min(stride)) * channels;
                let first = (start + c0.min(stride)) * channels;
                if first < end && end <= len {
                    first..end
                } else {
                    0..0
                }
            };
            let colors = &data[row(w, c, data.len())];
            let mask =
                alpha.map(|a| &a.luma.data[row(a.luma.width as usize, 1, a.luma.data.len())]);
            if let Some(a) = alpha {
                let mask = mask.unwrap();
                let count = (colors.len() / c).max(mask.len());
                if (colors.is_empty() && tail.is_none())
                    || (mask.is_empty() && a.tail.texel.is_none())
                {
                    break;
                }
                let missing_alpha = a.tail.texel.as_ref().map_or(0, |v| v[0]);
                let mut missing = vec![0; c + 1];
                if let Some(t) = tail {
                    missing.clear();
                    super::decode::premultiply(&mut missing, t, c, &[missing_alpha]);
                }
                if count == 0 {
                    area.fill_tail(&missing);
                    break;
                }
                work.clear();
                if colors.len() / c == mask.len() {
                    super::decode::premultiply(&mut work, colors, c, mask);
                } else {
                    for i in 0..count {
                        let t = colors
                            .get(i * c..(i + 1) * c)
                            .or(tail)
                            .ok_or(RequestError::Failed)?;
                        let opacity = mask.get(i).copied().unwrap_or(missing_alpha);
                        super::decode::premultiply(&mut work, t, c, &[opacity]);
                    }
                }
                area.push_prefix(&work, &missing);
            } else if colors.is_empty() && y >= (data.len() / c / w) as u32 {
                if let Some(fill) = tail {
                    area.fill_tail(fill);
                }
                break;
            } else {
                area.push_prefix(colors, tail.unwrap_or_default());
            }
        }
        Ok(rows.end - rows.start)
    }
}

#[cfg(test)]
mod short_rows;

#[cfg(all(test, debug_assertions))]
#[test]
fn recorded_mask_and_filter_operands_need_no_object_reads_on_the_drawing_thread() {
    use crate::fixtures::{file, stream};
    use hayro_syntax::Pdf;
    use hayro_syntax::object::{ObjectIdentifier, Stream};
    use hayro_syntax::work::BODIES_READ;

    for (mask, expected) in [("/Mask 4 0 R", 0), ("/SMask 5 0 R", 127)] {
        let objects = [
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            stream(
                &format!("/Width 1 /Height 1 /CS /G /BPC 8 /Decode 6 0 R {mask}"),
                &[128],
            ),
            b"[7 0 R 7 0 R]".to_vec(),
            stream(
                "/Width 1 /Height 1 /BPC 8 /Decode 6 0 R /Filter /LZWDecode /DP << /EarlyChange 8 0 R >>",
                &[0x80, 0x1f, 0xe0, 0x20],
            ),
            b"[0 1]".to_vec(),
            b"128".to_vec(),
            b"1".to_vec(),
        ];
        let pdf = Pdf::new(file(&objects, "")).unwrap();
        let stream: Stream<'_> = pdf.xref().get(ObjectIdentifier::new(3, 0)).unwrap();
        let obj = ImageXObject::new(
            &stream,
            |_| None,
            &(Arc::new(|_| {}) as WarningSinkFn),
            &Cache::new(),
            None,
        )
        .unwrap();
        let asset = obj.asset(None);
        drop(pdf);
        std::thread::spawn(move || {
            BODIES_READ.set(0);
            let mut source = asset.open(None).unwrap();
            let out = source
                .request(&ImageRequest {
                    plane: Plane::Image,
                    grid: (1, 1),
                    window: [0, 0, 1, 1],
                    cancel: None,
                })
                .unwrap();
            assert_eq!(out.data.last(), Some(&expected));
            assert_eq!(BODIES_READ.get(), 0);
        })
        .join()
        .unwrap();
    }
}
