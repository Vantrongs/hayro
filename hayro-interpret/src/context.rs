use crate::cache::{Cache, CacheKey};
use crate::color::{Color, ColorSpace};
use crate::convert::convert_transform;
use crate::font::{Font, PositionedGlyph, StandardFont};
use crate::interpret::annotation::Glyph;
use crate::interpret::stack::StateStack;
use crate::interpret::state::{
    ClipType, CompositingState, PaintData, PaintState, State, TextState, TextStateFont,
};
use crate::ocg::{OcgState, OptionalContent};
use crate::util::{BezPathExt, Float64Ext};
use crate::{
    ClipPath, Device, DrawProps, FillRule, InterpreterSettings, InterpreterWarning, Knockout,
    Paint, StrokeProps,
};
use hayro_syntax::content::ops::Transform;
use hayro_syntax::hash;
use hayro_syntax::object::Dict;
use hayro_syntax::object::Name;
use hayro_syntax::page::Resources;
use hayro_syntax::xref::{Detached, XRef};
use kurbo::{Affine, BezPath, PathEl, Point, Rect, Shape};
use smallvec::smallvec;
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

/// Maximum nesting depth for interpreting `XObject`'s/patterns/streams.
pub(crate) const MAX_NESTED_INTERPRETATION_DEPTH: u32 = 50;

/// A cache used by the interpreter.
///
/// Ideally, such a cache should be constructed once per PDF and then reused across
/// multiple interpreter invocations on the same document.
#[derive(Clone)]
pub struct InterpreterCache<'a> {
    pub(crate) font_cache: Rc<RefCell<hash::Map<u128, Option<Font<'a>>>>>,
    pub(crate) object_cache: Cache,
    /// The form fields read for the annotations of the document's pages.
    pub(crate) annotation_fields: crate::interpret::annotation::Fields<'a>,
}

impl<'a> Default for InterpreterCache<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> InterpreterCache<'a> {
    /// Create a new interpreter cache.
    pub fn new() -> Self {
        Self {
            font_cache: Rc::new(RefCell::new(hash::Map::default())),
            object_cache: Cache::new(),
            annotation_fields: crate::interpret::annotation::Fields::default(),
        }
    }
}

/// A per-page interpretation context that borrows shared data from an [`InterpreterCache`].
pub struct Context<'a> {
    stack: StateStack<'a>,
    path: BezPath,
    sub_path_start: Point,
    last_point: Point,
    clip: Option<FillRule>,
    root_transforms: Vec<Affine>,
    bbox: Vec<Rect>,
    /// Whether anything drawn read `bbox`, the box the context was made with as the
    /// clips leave it: what is drawn from it then depends on that box.
    bbox_read: Cell<bool>,
    pub(crate) settings: InterpreterSettings,
    pub(crate) interpreter_cache: InterpreterCache<'a>,
    pub(crate) xref: &'a XRef,
    pub(crate) ocg_state: OcgState<'a>,
    /// The outlines of the glyphs shown in a clipping mode since the last `ET` of the
    /// content stream being drawn, in device space, if any were shown: `PDFium` keeps
    /// them with the parser of each content stream, not in the graphics state, so `Q`
    /// brings back none an `ET` has used, and glyphs without outlines clip all away.
    pub(crate) text_clip: Option<BezPath>,
    nesting_depth: u32,
    pub(crate) glyph_scratch: Vec<PositionedGlyph<'a>>,
    /// While a generated annotation appearance is drawn, the glyphs it draws as forms
    /// by name.
    pub(crate) generated_glyphs: hash::Map<Rc<str>, Rc<Glyph>>,
    /// What the readers of the images drawn in the context share, once one is drawn
    /// ([`Context::reader`]).
    reader: OnceCell<Detached>,
}

impl<'a> Context<'a> {
    /// Create a new context.
    pub fn new(
        initial_transform: Affine,
        bbox: Rect,
        cache: &InterpreterCache<'a>,
        xref: &'a XRef,
        settings: InterpreterSettings,
    ) -> Self {
        let state = State::new(initial_transform);
        let optional_content = cache.annotation_fields.optional_content(xref);

        Self::new_with(
            initial_transform,
            bbox,
            cache,
            xref,
            settings,
            state,
            0,
            Some(optional_content),
        )
    }

    pub(crate) fn new_with(
        initial_transform: Affine,
        bbox: Rect,
        cache: &InterpreterCache<'a>,
        xref: &'a XRef,
        settings: InterpreterSettings,
        state: State<'a>,
        nesting_depth: u32,
        optional_content: Option<Rc<OptionalContent<'a>>>,
    ) -> Self {
        let ocg_state = OcgState::new(optional_content);

        Self {
            stack: StateStack::new(state),
            settings,
            xref,
            root_transforms: vec![initial_transform],
            last_point: Point::default(),
            sub_path_start: Point::default(),
            clip: None,
            bbox: vec![bbox],
            bbox_read: Cell::new(false),
            path: BezPath::new(),
            interpreter_cache: cache.clone(),
            ocg_state,
            text_clip: None,
            nesting_depth,
            glyph_scratch: Vec::new(),
            generated_glyphs: hash::Map::default(),
            reader: OnceCell::new(),
        }
    }

    /// What readers of the document apart from `xref` share with it
    /// ([`XRef::detached`]), for every image drawn in the context: decoding an image
    /// reads what its dictionary names (its filters' parameters, its masks) through
    /// a reader of its own made from it, which keeps what it reads while the image
    /// is decoded, not `xref` for the document.
    pub(crate) fn reader(&self) -> &Detached {
        self.reader.get_or_init(|| self.xref.detached())
    }

    pub(crate) fn save_state(&mut self) {
        self.stack.save();
    }

    /// The box the context was made with, as the clips in effect leave it. What reads
    /// it depends on the box: [`Context::bbox_read`].
    pub(crate) fn bbox(&self) -> Rect {
        self.bbox_read.set(true);
        self.current_bbox()
    }

    /// Whether anything drawn in the context read [`Context::bbox`].
    pub(crate) fn bbox_read(&self) -> bool {
        self.bbox_read.get()
    }

    fn current_bbox(&self) -> Rect {
        self.bbox.last().copied().unwrap_or_else(|| {
            warn!("failed to get a bbox");

            Rect::new(0.0, 0.0, 1.0, 1.0)
        })
    }

    /// Narrows the box by a clip's; only what reads the box narrowed reads it.
    fn push_bbox(&mut self, bbox: Rect) {
        let new = self.current_bbox().intersect(bbox);
        self.bbox.push(new);
    }

    pub(crate) fn push_clip_path(
        &mut self,
        clip_path: BezPath,
        fill: FillRule,
        device: &mut impl Device<'a>,
    ) {
        if let Some(clip_rect) = path_as_rect(&clip_path) {
            let cur_bbox = self.bbox();

            // If the clip path is a rect and completely covers the current bbox, don't emit it.
            if cur_bbox
                .min_x()
                .is_nearly_greater_or_equal(clip_rect.min_x())
                && cur_bbox
                    .min_y()
                    .is_nearly_greater_or_equal(clip_rect.min_y())
                && cur_bbox.max_x().is_nearly_less_or_equal(clip_rect.max_x())
                && cur_bbox.max_y().is_nearly_less_or_equal(clip_rect.max_y())
            {
                self.stack.push_clip(ClipType::Dummy);
                return;
            }

            device.push_clip_rect(&clip_rect);
            self.push_bbox(clip_rect);
            self.stack.push_clip(ClipType::Real);
            return;
        }

        let bbox = clip_path.bounding_box();
        device.push_clip_path(&ClipPath {
            path: clip_path,
            fill,
        });
        self.push_bbox(bbox);
        self.stack.push_clip(ClipType::Real);
    }

    pub(crate) fn pop_clip(&mut self, device: &mut impl Device<'a>) {
        if let Some(ClipType::Real) = self.stack.pop_clip() {
            device.pop_clip();
            self.pop_bbox();
        }
    }

    fn pop_bbox(&mut self) {
        self.bbox.pop();
    }

    pub(crate) fn push_root_transform(&mut self) {
        self.root_transforms.push(self.ctm());
    }

    pub(crate) fn pop_root_transform(&mut self) {
        self.root_transforms.pop();
    }

    pub(crate) fn root_transform(&self) -> Affine {
        self.root_transforms
            .last()
            .copied()
            .unwrap_or(Affine::IDENTITY)
    }

    pub(crate) fn restore_state(&mut self, device: &mut impl Device<'a>) {
        let Some(target_clips) = self.stack.restore_target_clips() else {
            return;
        };

        while self.stack.num_clips() > target_clips {
            self.pop_clip(device);
        }

        self.stack.restore();
    }

    pub(crate) fn draw_props(&self, is_stroke: bool) -> DrawProps<'a> {
        DrawProps {
            transform: self.ctm(),
            paint: self.get_paint(is_stroke),
            soft_mask: self.compositing().soft_mask.clone(),
            blend_mode: self.compositing().blend_mode,
            knockout: Knockout::None,
        }
    }

    pub(crate) fn get_paint(&self, is_stroke: bool) -> Paint<'a> {
        let paint = if is_stroke {
            self.stroke_paint()
        } else {
            self.fill_paint()
        };
        let data = PaintData {
            alpha: paint.alpha,
            color: paint.color.clone(),
            color_space: paint.color_space.clone(),
            pattern: paint.pattern.clone(),
            transfer_function: self.compositing().transfer_function.clone(),
        };

        if data.color_space.is_pattern() || data.pattern.is_some() {
            if let Some(mut pattern) = data.pattern {
                pattern.set_opacity(data.alpha);
                if let Some(tf) = &data.transfer_function {
                    pattern.set_transfer_function(tf.clone());
                }

                pattern.pre_concat_transform(self.root_transform());

                Paint::Pattern(Box::new(pattern))
            } else {
                // Pattern was likely invalid, use transparent paint.
                Paint::Color(Color::new(ColorSpace::device_gray(), smallvec![0.0], 0.0))
            }
        } else {
            let color = Color::new(data.color_space, data.color, data.alpha);

            if let Some(tf) = &data.transfer_function {
                Paint::Color(Color::from_rgba(tf.apply(&color.to_rgba())))
            } else {
                Paint::Color(color)
            }
        }
    }

    pub(crate) fn path(&self) -> &BezPath {
        &self.path
    }

    pub(crate) fn path_mut(&mut self) -> &mut BezPath {
        &mut self.path
    }

    /// The path, ready for a segment from the current point. After `h` (or `re`) PDF
    /// continues from the start of the closed subpath; a `BezPath` subpath always
    /// begins with a `MoveTo`, so one is added there.
    pub(crate) fn path_for_segment(&mut self) -> &mut BezPath {
        if self.path.elements().last() == Some(&PathEl::ClosePath) {
            self.path.move_to(self.sub_path_start);
        }
        &mut self.path
    }

    pub(crate) fn sub_path_start(&self) -> &Point {
        &self.sub_path_start
    }

    pub(crate) fn sub_path_start_mut(&mut self) -> &mut Point {
        &mut self.sub_path_start
    }

    pub(crate) fn last_point(&self) -> &Point {
        &self.last_point
    }

    pub(crate) fn last_point_mut(&mut self) -> &mut Point {
        &mut self.last_point
    }

    pub(crate) fn clip_mut(&mut self) -> &mut Option<FillRule> {
        &mut self.clip
    }

    pub(crate) fn ctm(&self) -> Affine {
        *self.stack.ctm()
    }

    pub(crate) fn ctm_mut(&mut self) -> &mut Affine {
        self.stack.ctm_mut()
    }

    pub(crate) fn stroke_paint(&self) -> &PaintState<'a> {
        self.stack.stroke()
    }

    pub(crate) fn stroke_paint_mut(&mut self) -> &mut PaintState<'a> {
        self.stack.stroke_mut()
    }

    pub(crate) fn fill_paint(&self) -> &PaintState<'a> {
        self.stack.fill()
    }

    pub(crate) fn fill_paint_mut(&mut self) -> &mut PaintState<'a> {
        self.stack.fill_mut()
    }

    pub(crate) fn stroke_props(&self) -> &StrokeProps {
        self.stack.stroke_props()
    }

    pub(crate) fn stroke_props_mut(&mut self) -> &mut StrokeProps {
        self.stack.stroke_props_mut()
    }

    pub(crate) fn text_state(&self) -> &TextState<'a> {
        self.stack.text()
    }

    pub(crate) fn text_state_mut(&mut self) -> &mut TextState<'a> {
        self.stack.text_mut()
    }

    pub(crate) fn compositing(&self) -> &CompositingState<'a> {
        self.stack.compositing()
    }

    pub(crate) fn compositing_mut(&mut self) -> &mut CompositingState<'a> {
        self.stack.compositing_mut()
    }

    /// A copy of the current graphics state.
    pub(crate) fn snapshot(&self) -> State<'a> {
        self.stack.snapshot()
    }

    pub(crate) fn pre_concat_transform(&mut self, transform: Transform) {
        self.pre_concat_affine(convert_transform(transform));
    }

    pub(crate) fn pre_concat_affine(&mut self, transform: Affine) {
        *self.ctm_mut() *= transform;
    }

    pub(crate) fn get_color_space(
        &mut self,
        resources: &Resources<'_>,
        name: &Name<'_>,
    ) -> Option<ColorSpace> {
        let cs_object = resources.get_color_space(name)?;
        self.interpreter_cache
            .object_cache
            .get_or_insert_with(cs_object.cache_key(), || {
                ColorSpace::new(cs_object.clone(), &self.interpreter_cache.object_cache)
            })
    }

    pub(crate) fn num_states(&self) -> usize {
        self.stack.len()
    }

    pub(crate) fn nesting_depth(&self) -> u32 {
        self.nesting_depth
    }

    pub(crate) fn begin_nested_interpretation(&mut self) -> bool {
        if self.nesting_depth >= MAX_NESTED_INTERPRETATION_DEPTH {
            warn!("interpreter nesting depth exceeded");

            return false;
        }

        self.nesting_depth += 1;

        true
    }

    pub(crate) fn end_nested_interpretation(&mut self) {
        self.nesting_depth = self.nesting_depth.saturating_sub(1);
    }
    pub(crate) fn resolve_font(&mut self, font_dict: &Dict<'a>) -> Option<TextStateFont<'a>> {
        let cache_key = font_dict.cache_key();

        let resolved = {
            let mut font_cache = self.interpreter_cache.font_cache.borrow_mut();
            font_cache
                .entry(cache_key)
                .or_insert_with(|| {
                    // Kept for every page, the font is made from its dictionary read as
                    // the object it is, not through the references this page reached
                    // it by: what the font looks up later (a Type 3 font's resources)
                    // would be refused where it leads back to one of them, for every
                    // page drawn after this one, which drawn alone reads it.
                    let rooted = font_dict
                        .obj_id()
                        .and_then(|id| self.xref.get::<Dict<'a>>(id))
                        .filter(|read| core::ptr::eq(read.data(), font_dict.data()));
                    Font::new(
                        rooted.as_ref().unwrap_or(font_dict),
                        &self.settings.font_resolver,
                        &self.settings.cmap_resolver,
                    )
                })
                .clone()
        };

        if let Some(resolved) = resolved {
            Some(TextStateFont::Font(resolved))
        } else {
            (self.settings.warning_sink)(InterpreterWarning::UnsupportedFont);

            Font::new_standard(StandardFont::Helvetica, &self.settings.font_resolver)
                .map(TextStateFont::Fallback)
        }
    }
}

pub(crate) fn path_as_rect(path: &BezPath) -> Option<Rect> {
    let points = match path.elements() {
        [
            PathEl::MoveTo(first),
            PathEl::LineTo(second),
            PathEl::LineTo(third),
            PathEl::LineTo(fourth),
            PathEl::ClosePath,
        ] => [*first, *second, *third, *fourth, *first],
        [
            PathEl::MoveTo(first),
            PathEl::LineTo(second),
            PathEl::LineTo(third),
            PathEl::LineTo(fourth),
            PathEl::LineTo(last),
        ] if first.x.is_nearly_equal(last.x) && first.y.is_nearly_equal(last.y) => {
            [*first, *second, *third, *fourth, *last]
        }
        _ => return None,
    };

    let mut previous_axis = None;
    for edge in points.windows(2) {
        let same_x = edge[0].x.is_nearly_equal(edge[1].x);
        let same_y = edge[0].y.is_nearly_equal(edge[1].y);
        if same_x == same_y || previous_axis == Some(same_x) {
            return None;
        }
        previous_axis = Some(same_x);
    }

    Some(path.fast_bounding_box())
}
