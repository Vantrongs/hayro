use crate::CacheKey;
use crate::color::Color;
use crate::interpret::state::ActiveTransferFunction;
use crate::pattern::Pattern;
use crate::util::hash128;
use crate::x_object::ImageXObject;
use crate::x_object::soft_mask::SoftMask;
use hayro_syntax::object::Stream;
use kurbo::{Affine, BezPath, Cap, Join};
use smallvec::{SmallVec, smallvec};
use std::sync::Arc;

/// A clip path.
#[derive(Debug, Clone)]
pub struct ClipPath {
    /// The clipping path.
    pub path: BezPath,
    /// The fill rule.
    pub fill: FillRule,
}

impl CacheKey for ClipPath {
    fn cache_key(&self) -> u128 {
        hash128(&(&self.path.to_svg(), &self.fill))
    }
}

/// A stencil image.
pub struct StencilImage<'a, 'b> {
    pub(crate) paint: Paint<'a>,
    pub(crate) image_xobject: ImageXObject<'b>,
}

impl<'a, 'b> StencilImage<'a, 'b> {
    /// Perform some operation with the stencil data of the image.
    ///
    /// The second argument allows you to give the image decoder a hint for
    /// what resolution of the image you want to have. Note that this does not
    /// mean that the resulting image will have that dimension. Instead, it allows
    /// the image decoder to extract a lower-resolution version of the image in
    /// certain cases.
    pub fn with_stencil(
        &self,
        func: impl FnOnce(LumaData, &Paint<'a>),
        target_dimension: Option<(u32, u32)>,
    ) {
        if let Some(decoded) = self.image_xobject.decoded_mask(target_dimension) {
            func(decoded.luma, &self.paint);
        }
    }

    /// The paint the mask lets through.
    pub fn paint(&self) -> &Paint<'a> {
        &self.paint
    }

    /// The mask as an owned asset, as [`RasterImage::asset`]: its one plane
    /// ([`Plane::Image`](crate::Plane::Image)) is the alpha the paint is drawn
    /// through ([`PixelFormat::Alpha`](crate::PixelFormat::Alpha)), decoded whole when
    /// opened.
    pub fn asset(&self) -> crate::ImageAsset {
        self.image_xobject.asset(self.pixels_key())
    }

    /// Identifies the alpha the mask decodes to, as [`RasterImage::pixels_key`]: its
    /// body, which no raster image's key names alone. `None` for an inline mask.
    pub fn pixels_key(&self) -> Option<u128> {
        let id = self.image_xobject.stream.identity()?;
        Some(hash128(&(id, "stencil")))
    }

    // These are hidden since clients are supposed to call get the
    // width/height from `LumaData` instead.
    #[doc(hidden)]
    pub fn width(&self) -> u32 {
        self.image_xobject.width()
    }

    #[doc(hidden)]
    pub fn height(&self) -> u32 {
        self.image_xobject.height()
    }
}

impl CacheKey for StencilImage<'_, '_> {
    fn cache_key(&self) -> u128 {
        self.image_xobject.cache_key()
    }
}

/// A raster image.
pub struct RasterImage<'a>(pub(crate) ImageXObject<'a>);

impl RasterImage<'_> {
    /// Perform some operation with the RGB and alpha channel of the image.
    ///
    /// The second argument allows you to give the image decoder a hint for
    /// what resolution of the image you want to have. Note that this does not
    /// mean that the resulting image will have that dimension. Instead, it allows
    /// the image decoder to extract a lower-resolution version of the image in
    /// certain cases.
    pub fn with_rgba(
        &self,
        func: impl FnOnce(ImageData, Option<LumaData>),
        target_dimension: Option<(u32, u32)>,
    ) {
        if let Some(decoded) = self.0.decoded_image(target_dimension) {
            func(decoded.image, decoded.alpha);
        }
    }

    /// Return the underlying stream object.
    ///
    /// This allows you to get access to the raw encoded image data, without doing any decoding.
    pub fn stream(&self) -> &Stream<'_> {
        self.0.stream()
    }

    // These are hidden since clients are supposed to call get the
    // width/height from `LumaData` instead.
    #[doc(hidden)]
    pub fn width(&self) -> u32 {
        self.0.width()
    }

    #[doc(hidden)]
    pub fn height(&self) -> u32 {
        self.0.height()
    }

    /// The image as an owned asset: a handle that outlives the content stream and
    /// decodes regions of the image on request, keyed by [`Self::pixels_key`].
    pub fn asset(&self) -> crate::ImageAsset {
        self.0.asset(self.pixels_key())
    }

    /// Identifies the pixels `with_rgba` decodes for a given `target_dimension`: the
    /// image's body ([`Stream::identity`](hayro_syntax::object::Stream::identity),
    /// unique among the documents loaded, one for every entry leading to it) and the
    /// transfer function in effect. `None`
    /// when there is no object to identify (an inline image) or the colour space is a
    /// name looked up in the resources where the image is drawn.
    pub fn pixels_key(&self) -> Option<u128> {
        let image = &self.0;
        let id = image.stream.identity()?;
        if image.cs_by_name {
            return None;
        }
        // Decoding applies the transfer function through these tables
        // (`decode_image`): a luma image the single one, an RGB one the first three.
        let tables: SmallVec<[&[u8; 256]; 3]> = match &image.transfer_function {
            None => SmallVec::new(),
            Some(ActiveTransferFunction::Single(f)) => smallvec![f.samples()],
            Some(ActiveTransferFunction::Four(f)) => f[..3].iter().map(|f| f.samples()).collect(),
        };
        Some(hash128(&(id, tables)))
    }
}

impl CacheKey for RasterImage<'_> {
    fn cache_key(&self) -> u128 {
        self.0.cache_key()
    }
}

/// A type of image.
pub enum Image<'a, 'b> {
    /// A stencil image.
    Stencil(StencilImage<'a, 'b>),
    /// A normal raster image.
    Raster(RasterImage<'b>),
}

impl Image<'_, '_> {
    // These are hidden since clients are supposed to call get the
    // width/height from `LumaData/RgbData` instead.
    #[doc(hidden)]
    pub fn width(&self) -> u32 {
        match self {
            Image::Stencil(s) => s.width(),
            Image::Raster(r) => r.width(),
        }
    }

    // These are hidden since clients are supposed to call get the
    // width/height from `LumaData/RgbData` instead.
    #[doc(hidden)]
    pub fn height(&self) -> u32 {
        match self {
            Image::Stencil(s) => s.height(),
            Image::Raster(r) => r.height(),
        }
    }
}

impl CacheKey for Image<'_, '_> {
    fn cache_key(&self) -> u128 {
        match self {
            Image::Stencil(i) => i.cache_key(),
            Image::Raster(i) => i.cache_key(),
        }
    }
}

/// A structure holding 3-channel RGB data.
#[derive(Clone)]
pub struct RgbData {
    /// The actual data. It is guaranteed to have the length width * height * 3.
    pub data: Vec<u8>,
    /// The width.
    pub width: u32,
    /// The height.
    pub height: u32,
    /// Whether the image should be interpolated.
    pub interpolate: bool,
    /// Additional scaling factors to apply to the image.
    ///
    /// In most cases, those factors will just be 1.0, and you can
    /// ignore them. There are two situations in which they will not be equal
    /// to 1:
    /// 1) The PDF provided wrong metadata about the width/height of the image,
    ///    which needs to be corrected
    /// 2) A lower resolution of the image was requested, in which case it needs
    ///    to be scaled up so that it still covers the same area.
    ///
    /// The first number indicates the x scaling factor, the second number the
    /// y scaling factor.
    pub scale_factors: (f32, f32),
}

/// A structure holding 1-channel luma data.
#[derive(Clone)]
pub struct LumaData {
    /// The actual data. It is guaranteed to have the length width * height.
    pub data: Vec<u8>,
    /// The width.
    pub width: u32,
    /// The height.
    pub height: u32,
    /// Whether the image should be interpolated.
    pub interpolate: bool,
    /// Additional scaling factors to apply to the image.
    ///
    /// In most cases, those factors will just be 1.0, and you can
    /// ignore them. There are two situations in which they will not be equal
    /// to 1:
    /// 1) The PDF provided wrong metadata about the width/height of the image,
    ///    which needs to be corrected
    /// 2) A lower resolution of the image was requested, in which case it needs
    ///    to be scaled up so that it still covers the same area.
    ///
    /// The first number indicates the x scaling factor, the second number the
    /// y scaling factor.
    pub scale_factors: (f32, f32),
}

/// The color data of a raster image, either 3-channel RGB or 1-channel luma.
#[derive(Clone)]
pub enum ImageData {
    /// 3-channel RGB data.
    Rgb(RgbData),
    /// 1-channel grayscale data.
    Luma(LumaData),
}

impl ImageData {
    /// The width of the image.
    pub fn width(&self) -> u32 {
        match self {
            Self::Rgb(d) => d.width,
            Self::Luma(d) => d.width,
        }
    }

    /// The height of the image.
    pub fn height(&self) -> u32 {
        match self {
            Self::Rgb(d) => d.height,
            Self::Luma(d) => d.height,
        }
    }

    /// Whether the image should be interpolated.
    pub fn interpolate(&self) -> bool {
        match self {
            Self::Rgb(d) => d.interpolate,
            Self::Luma(d) => d.interpolate,
        }
    }

    /// The scaling factors of the image.
    pub fn scale_factors(&self) -> (f32, f32) {
        match self {
            Self::Rgb(d) => d.scale_factors,
            Self::Luma(d) => d.scale_factors,
        }
    }
}

/// A type of paint.
#[derive(Clone, Debug)]
pub enum Paint<'a> {
    /// A solid RGBA color.
    Color(Color),
    /// A PDF pattern.
    Pattern(Box<Pattern<'a>>),
}

impl CacheKey for Paint<'_> {
    fn cache_key(&self) -> u128 {
        match self {
            Paint::Color(c) => {
                // TODO: We should actually cache the color with color space etc., not just the
                // RGBA8 version.
                hash128(&c.to_rgba().to_rgba8())
            }
            Paint::Pattern(p) => p.cache_key(),
        }
    }
}

/// Properties for a painted drawing operation.
#[derive(Clone)]
pub struct DrawProps<'a> {
    /// The transform.
    pub transform: Affine,
    /// The paint.
    pub paint: Paint<'a>,
    /// The soft mask.
    pub soft_mask: Option<SoftMask<'a>>,
    /// The blend mode.
    pub blend_mode: BlendMode,
    /// How the drawing combines with what the current transparency group drew
    /// beneath it.
    pub knockout: Knockout,
}

/// How a drawing combines with what the current transparency group drew beneath it:
/// the stroke of a path filled and stroked by one operator, which viewers paint as a
/// knockout group (PDF 32000-1 11.4.6.2), hides the fill where it covers it. A device
/// that composites every drawing over shows what the stroke should hide.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Knockout {
    /// Composited over it.
    #[default]
    None,
    /// Replaces it in proportion to how much of each pixel the drawing covers, so a
    /// pixel the drawing covers in part is as opaque as the two together.
    Replace,
    /// Clears it in proportion to coverage, then is composited over it, so a pixel
    /// the drawing covers in part shows less of either.
    ClearThenOver,
}

/// Properties for an image drawing operation.
#[derive(Clone)]
pub struct ImageDrawProps<'a> {
    /// The transform.
    pub transform: Affine,
    /// The soft mask.
    pub soft_mask: Option<SoftMask<'a>>,
    /// The blend mode.
    pub blend_mode: BlendMode,
}

/// The draw mode.
#[derive(Clone, Debug)]
pub enum DrawMode {
    /// Draw using a fill.
    Fill(FillRule),
    /// Draw using a stroke.
    Stroke(StrokeProps),
    /// Draw using both fill and stroke.
    FillAndStroke(FillRule, StrokeProps),
    /// Invisible text (for text extraction but not visual rendering).
    Invisible,
}

/// Stroke properties.
#[derive(Clone, Debug)]
pub struct StrokeProps {
    /// The line width.
    pub line_width: f32,
    /// The line cap.
    pub line_cap: Cap,
    /// The line join.
    pub line_join: Join,
    /// The miter limit.
    pub miter_limit: f32,
    /// The dash array, shared by the copies of the graphics state that hold it, so
    /// that a copy costs the same however long the array is.
    pub dash_array: Arc<[f32]>,
    /// The dash offset.
    pub dash_offset: f32,
    /// Whether the dash pattern hides the stroke (`set_dash`): the interpreter
    /// draws no stroke while it does.
    pub(crate) dash_hides: bool,
}

impl Default for StrokeProps {
    fn default() -> Self {
        Self {
            line_width: 1.0,
            line_cap: Cap::Butt,
            line_join: Join::Miter,
            miter_limit: 10.0,
            dash_array: Arc::default(),
            dash_offset: 0.0,
            dash_hides: false,
        }
    }
}

impl StrokeProps {
    /// Whether a stroke in these properties is drawn: not when its dash pattern
    /// hides it.
    pub(crate) fn drawn(&self) -> bool {
        !self.dash_hides
    }
}

/// A fill rule.
#[derive(Clone, Debug, Copy, Hash, PartialEq, Eq)]
pub enum FillRule {
    /// Non-zero filling.
    NonZero,
    /// Even-odd filling.
    EvenOdd,
}

/// A blend mode.
#[derive(Clone, Debug, Copy, Hash, PartialEq, Eq, Default)]
pub enum BlendMode {
    /// Normal blend mode (default).
    #[default]
    Normal,
    /// Multiply blend mode.
    Multiply,
    /// Screen blend mode.
    Screen,
    /// Overlay blend mode.
    Overlay,
    /// Darken blend mode.
    Darken,
    /// Lighten blend mode.
    Lighten,
    /// `ColorDodge` blend mode.
    ColorDodge,
    /// `ColorBurn` blend mode.
    ColorBurn,
    /// `HardLight` blend mode.
    HardLight,
    /// `SoftLight` blend mode.
    SoftLight,
    /// Difference blend mode.
    Difference,
    /// Exclusion blend mode.
    Exclusion,
    /// Hue blend mode.
    Hue,
    /// Saturation blend mode.
    Saturation,
    /// Color blend mode.
    Color,
    /// Luminosity blend mode.
    Luminosity,
}
