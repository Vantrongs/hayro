//! PDF patterns.

use crate::cache::Cache;
use crate::color::{Color, ColorSpace};
use crate::context::{Context, InterpreterCache};
use crate::device::Device;
use crate::font::GlyphRun;
use crate::interpret::annotation::entries::Entries;
use crate::interpret::state::{ActiveTransferFunction, State};
use crate::shading::Shading;
use crate::util::{Float32Ext, hash128};
use crate::x_object::soft_mask::SoftMask;
use crate::{BlendMode, CacheKey, ClipPath, DrawMode, DrawProps, Image, ImageDrawProps, Knockout};
use crate::{FillRule, InterpreterSettings, Paint, interpret};
use hayro_syntax::content::TypedIter;
use hayro_syntax::object::Dict;
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::{
    BBOX, EXT_G_STATE, MATRIX, PAINT_TYPE, RESOURCES, SHADING, X_STEP, Y_STEP,
};
use hayro_syntax::object::{Object, dict_or_stream};
use hayro_syntax::page::Resources;
use hayro_syntax::xref::XRef;
use kurbo::{Affine, BezPath, Rect, Shape};
use std::fmt::{Debug, Formatter};
use std::sync::Arc;

/// A PDF pattern.
#[derive(Debug, Clone)]
pub enum Pattern<'a> {
    /// A shading pattern.
    Shading(ShadingPattern),
    /// A tiling pattern.
    Tiling(Box<TilingPattern<'a>>),
}

impl<'a> Pattern<'a> {
    pub(crate) fn new(object: Object<'a>, ctx: &Context<'a>) -> Option<Self> {
        match object {
            Object::Dict(dict) => Some(Self::Shading(ShadingPattern::new(
                &dict,
                &ctx.interpreter_cache.object_cache,
            )?)),
            Object::Stream(stream) => {
                Some(Self::Tiling(Box::new(TilingPattern::new(stream, ctx)?)))
            }
            _ => None,
        }
    }

    pub(crate) fn pre_concat_transform(&mut self, transform: Affine) {
        match self {
            Self::Shading(p) => {
                p.matrix = transform * p.matrix;
            }
            Self::Tiling(p) => p.matrix = transform * p.matrix,
        }
    }

    /// Paints at `opacity`, the alpha of the graphics state it paints in: viewers
    /// take it when the pattern paints, not when it is selected.
    pub(crate) fn set_opacity(&mut self, opacity: f32) {
        match self {
            Self::Shading(p) => p.opacity = opacity,
            Self::Tiling(p) => p.opacity = opacity,
        }
    }

    pub(crate) fn set_transfer_function(&mut self, tf: ActiveTransferFunction) {
        if let Self::Shading(p) = self {
            p.transfer_function = Some(tf);
        }
    }
}

impl CacheKey for Pattern<'_> {
    fn cache_key(&self) -> u128 {
        match self {
            Self::Shading(p) => p.cache_key(),
            Self::Tiling(p) => p.cache_key(),
        }
    }
}

/// A shading pattern.
#[derive(Clone, Debug)]
pub struct ShadingPattern {
    /// The underlying shading of the pattern.
    pub shading: Arc<Shading>,
    /// A transformation matrix to apply prior to rendering.
    pub matrix: Affine,
    /// The opacity the shading paints at (see [`Pattern::set_opacity`]).
    pub opacity: f32,
    /// An optional transfer function to apply to the shading's output colors.
    pub transfer_function: Option<ActiveTransferFunction>,
}

impl ShadingPattern {
    pub(crate) fn new(dict: &Dict<'_>, cache: &Cache) -> Option<Self> {
        let shading = dict.get::<Object<'_>>(SHADING).and_then(|o| {
            let (dict, stream) = dict_or_stream(&o)?;

            Shading::new(dict, stream, cache)
        })?;
        let matrix = Affine::new(Entries::detached(dict.clone()).matrix(MATRIX));

        if dict.contains_key(EXT_G_STATE) {
            warn!("shading patterns with ext_g_state are not supported yet");
        }

        Some(Self {
            shading: Arc::new(shading),
            opacity: 1.0,
            matrix,
            transfer_function: None,
        })
    }
}

impl ShadingPattern {
    /// The shading's `/BBox` placed by `matrix`, which it is clipped to.
    pub fn clip_path(&self) -> Option<BezPath> {
        self.shading.clip_path().map(|path| self.matrix * path)
    }
}

impl CacheKey for ShadingPattern {
    fn cache_key(&self) -> u128 {
        hash128(&(
            self.shading.cache_key(),
            self.matrix.cache_key(),
            self.opacity.to_bits(),
            self.transfer_function.as_ref().map(CacheKey::cache_key),
        ))
    }
}

/// A tiling pattern.
#[derive(Clone)]
pub struct TilingPattern<'a> {
    cache_key: u128,
    ctx_bbox: Rect,
    /// The bbox of the tiling pattern.
    pub bbox: Rect,
    /// The step in the x direction.
    pub x_step: f32,
    /// The step in the y direction.
    pub y_step: f32,
    /// A transformation to apply prior to rendering.
    pub matrix: Affine,
    /// The opacity the pattern paints at, its cell as a whole (see
    /// [`Pattern::set_opacity`]).
    pub opacity: f32,
    stream: Stream<'a>,
    is_color: bool,
    pub(crate) stroke_paint: Color,
    pub(crate) non_stroking_paint: Color,
    resources: Resources<'a>,
    pub(crate) cache: InterpreterCache<'a>,
    pub(crate) settings: InterpreterSettings,
    pub(crate) xref: &'a XRef,
    nesting_depth: u32,
}

impl Debug for TilingPattern<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("TilingPattern")
    }
}

impl<'a> TilingPattern<'a> {
    pub(crate) fn new(stream: Stream<'a>, ctx: &Context<'a>) -> Option<Self> {
        let cache_key = stream.cache_key();
        let dict = stream.dict();

        // Read as viewers read damaged ones, as forms are.
        let entries = Entries::detached(dict.clone());
        let bbox = entries.rect(BBOX)?;
        let x_step = entries.number(X_STEP).unwrap_or(0.0) as f32;
        let y_step = entries.number(Y_STEP).unwrap_or(0.0) as f32;

        if x_step.is_nearly_zero() || y_step.is_nearly_zero() || bbox.is_zero_area() {
            return None;
        }

        let is_color = dict.get::<u8>(PAINT_TYPE)? == 1;
        let matrix = Affine::new(entries.matrix(MATRIX));

        let ctx_bbox = ctx.bbox();

        let fill = ctx.fill_paint();
        let fill_cs = fill
            .color_space
            .pattern_cs()
            .unwrap_or(ColorSpace::device_gray());
        let non_stroking_paint = Color::new(fill_cs, fill.color.clone(), 1.0);

        let stroke = ctx.stroke_paint();
        let stroke_cs = stroke
            .color_space
            .pattern_cs()
            .unwrap_or(ColorSpace::device_gray());
        // Opaque: the cell is painted at the pattern's opacity.
        let stroke_paint = Color::new(stroke_cs, stroke.color.clone(), 1.0);
        let nesting_depth = ctx.nesting_depth() + 1;
        let resources = Resources::new(dict.get(RESOURCES).unwrap_or_default());

        Some(Self {
            cache_key,
            bbox,
            x_step,
            y_step,
            matrix,
            opacity: 1.0,
            ctx_bbox,
            is_color,
            stream,
            stroke_paint,
            non_stroking_paint,
            settings: ctx.settings.clone(),
            resources,
            cache: ctx.interpreter_cache.clone(),
            xref: ctx.xref,
            nesting_depth,
        })
    }

    /// A key for the cell this pattern paints a fill (or, with `is_stroke`, a
    /// stroke) with: uses with equal keys draw the same cell, so a device can draw
    /// it once. Unlike [`CacheKey::cache_key`], which names the pattern's stream, it
    /// also covers what changes the cell between uses: the matrix, the bounds the
    /// cell is interpreted in, how deep in forms and patterns it is (which limits
    /// the forms it may draw) and, for an uncoloured pattern, its colour. The
    /// opacity is not in it: it applies to the cell as a whole.
    pub fn paint_key(&self, is_stroke: bool) -> u128 {
        let colour = (!self.is_color).then(|| {
            let paint = if is_stroke {
                &self.stroke_paint
            } else {
                &self.non_stroking_paint
            };
            paint.to_rgba().components().map(f32::to_bits)
        });
        let bounds = [
            self.ctx_bbox.x0,
            self.ctx_bbox.y0,
            self.ctx_bbox.x1,
            self.ctx_bbox.y1,
        ]
        .map(f64::to_bits);
        hash128(&(
            self.cache_key,
            self.matrix.cache_key(),
            bounds,
            self.nesting_depth,
            colour,
        ))
    }

    /// Interpret the contents of the pattern into the given device.
    pub fn interpret(
        &self,
        device: &mut impl Device<'a>,
        initial_transform: Affine,
        is_stroke: bool,
    ) -> Option<()> {
        let state = State::new(initial_transform);

        let mut context = Context::new_with(
            state.ctm,
            // TODO: bbox?
            (initial_transform * self.ctx_bbox.to_path(0.1)).bounding_box(),
            &self.cache,
            self.xref,
            self.settings.clone(),
            state,
            self.nesting_depth,
            // `PDFium` draws the cell of a tiling pattern without optional content
            // where it is smaller than the area filled (`CPDF_RenderTiling`).
            None,
        );

        let decoded = self.stream.decoded().ok()?;
        let iter = TypedIter::new(decoded.as_ref());

        let clip_path = ClipPath {
            path: initial_transform * self.bbox.to_path(0.1),
            fill: FillRule::NonZero,
        };
        device.push_clip_path(&clip_path);

        if self.is_color {
            interpret(iter, &self.resources, &mut context, device);
        } else {
            let paint = if !is_stroke {
                Paint::Color(self.non_stroking_paint.clone())
            } else {
                Paint::Color(self.stroke_paint.clone())
            };

            let mut device = StencilPatternDevice::new(device, paint.clone());
            interpret(iter, &self.resources, &mut context, &mut device);
        }

        device.pop_clip();

        Some(())
    }
}

impl CacheKey for TilingPattern<'_> {
    fn cache_key(&self) -> u128 {
        self.cache_key
    }
}

struct StencilPatternDevice<'a, 'b, T: Device<'a>> {
    inner: &'b mut T,
    paint: Paint<'a>,
}

impl<'a, 'b, T: Device<'a>> StencilPatternDevice<'a, 'b, T> {
    pub(crate) fn new(device: &'b mut T, paint: Paint<'a>) -> Self {
        Self {
            inner: device,
            paint,
        }
    }
}

// Only filling, stroking of paths and stencil masks are allowed.
impl<'a, T: Device<'a>> Device<'a> for StencilPatternDevice<'a, '_, T> {
    fn draw_path(&mut self, path: &BezPath, props: DrawProps<'a>, draw_mode: &DrawMode) {
        // The pair a knockout is for is not grouped here, so the stroke is drawn over.
        let props = DrawProps {
            paint: self.paint.clone(),
            knockout: Knockout::None,
            ..props
        };
        self.inner.draw_path(path, props, draw_mode);
    }

    fn push_clip_path(&mut self, clip_path: &ClipPath) {
        self.inner.push_clip_path(clip_path);
    }

    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'_>>, _: BlendMode) {}

    fn draw_glyph_run(
        &mut self,
        glyph_run: &GlyphRun<'_, 'a>,
        props: DrawProps<'a>,
        draw_mode: &DrawMode,
    ) {
        self.inner.draw_glyph_run(glyph_run, props, draw_mode);
    }

    fn draw_image(&mut self, image: Image<'a, '_>, props: ImageDrawProps<'a>) {
        if let Image::Stencil(mut s) = image {
            s.paint = self.paint.clone();
            self.inner.draw_image(Image::Stencil(s), props);
        }
    }

    fn pop_clip(&mut self) {
        self.inner.pop_clip();
    }

    fn pop_transparency_group(&mut self) {}

    // Groups are not drawn here, so the pair's fill lies in no group of its own:
    // clearing in the device drawn into would clear what was drawn before the pair
    // as well.
}
