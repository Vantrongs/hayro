use crate::color::ColorSpace;
use crate::context::Context;
use crate::convert::{convert_line_cap, convert_line_join};
use crate::device::{Device, MarkedContentProperties};
use crate::font::{Font, FontData, FontQuery, StandardFont};
use crate::interpret::path::{
    close_path, end_path, fill_path, fill_path_impl, fill_stroke_path, stroke_path,
};
use crate::interpret::state::{TextStateFont, convert_dash_array, handle_gs};
use crate::interpret::text::TextRenderingMode;
use crate::ocg::Kept;
use crate::pattern::{Pattern, ShadingPattern};
use crate::shading::Shading;
use crate::util::OptionLog;
use crate::x_object::{FormXObject, ImageXObject, XObject};
use crate::{FillRule, StrokeProps};
use hayro_syntax::content::TypedIter;
use hayro_syntax::content::ops::TypedInstruction;
use hayro_syntax::hash;
use hayro_syntax::object::dict::keys::{ANNOTS, OC};
use hayro_syntax::object::{Dict, Name, Object, dict_or_stream};
use hayro_syntax::page::{Page, Resources};
use kurbo::{Affine, Point, Shape};
use smallvec::smallvec;
use std::sync::Arc;

pub mod annotation;
pub(crate) mod path;
pub(crate) mod stack;
pub(crate) mod state;
pub(crate) mod text;

pub use state::ActiveTransferFunction;

/// A callback function for resolving font queries.
///
/// The first argument is the raw data, the second argument is the index in case the font
/// is a TTC, otherwise it should be 0.
pub type FontResolverFn = Arc<dyn Fn(&FontQuery) -> Option<(FontData, u32)> + Send + Sync>;
/// A callback function for resolving cmap names to their files.
pub type CMapResolverFn =
    Arc<dyn Fn(hayro_cmap::CMapName<'_>) -> Option<&'static [u8]> + Send + Sync>;
/// A callback function for resolving warnings during interpretation.
pub type WarningSinkFn = Arc<dyn Fn(InterpreterWarning) + Send + Sync>;

#[derive(Clone)]
/// Settings that should be applied during the interpretation process.
pub struct InterpreterSettings {
    /// Nearly every PDF contains text. In most cases, PDF files embed the fonts they use, and
    /// hayro can therefore read the font files and do all the processing needed. However, there
    /// are two problems:
    /// - Fonts don't _have_ to be embedded, it's possible that the PDF file only defines the basic
    ///   metadata of the font, like its name, but relies on the PDF processor to find that font
    ///   in its environment.
    /// - The PDF specification requires a list of 14 fonts that should always be available to a
    ///   PDF processor. These include:
    ///   - Times New Roman (Normal, Bold, Italic, `BoldItalic`)
    ///   - Courier (Normal, Bold, Italic, `BoldItalic`)
    ///   - Helvetica (Normal, Bold, Italic, `BoldItalic`)
    ///   - `ZapfDingBats`
    ///   - Symbol
    ///
    /// Because of this, if any of the above situations occurs, this callback will be called, which
    /// expects the data of an appropriate font to be returned, if available. If no such font is
    /// provided, the text will most likely fail to render.
    ///
    /// For the font data, there are two different formats that are accepted:
    /// - Any valid TTF/OTF font.
    /// - A valid CFF font program.
    ///
    /// The following recommendations are given for the implementation of this callback function.
    ///
    /// For the standard fonts, in case the original fonts are available on the system, you should
    /// just return those. Otherwise, for Helvetica, Courier and Times New Roman, the best alternative
    /// are the corresponding fonts of the [Liberation font family](https://github.com/liberationfonts/liberation-fonts).
    /// If you prefer smaller fonts, you can use the [Foxit CFF fonts](https://github.com/LaurenzV/hayro/tree/master/assets/standard_fonts),
    /// which are much smaller but are missing glyphs for certain scripts.
    ///
    /// For the `Symbol` and `ZapfDingBats` fonts, you should also prefer the system fonts, and if
    /// not available to you, you can, similarly to above, use the corresponding fonts from Foxit.
    ///
    /// If you don't want having to deal with this, you can just enable the `embed-fonts` feature
    /// and use the default implementation of the callback.
    pub font_resolver: FontResolverFn,
    /// A callback for resolving cmaps that aren't embedded.
    ///
    /// When the PDF requires using a cmap that is not directly embedded in the PDF,
    /// this callback will be called to attempt fetching the data of the file.
    ///
    /// When the `embed-cmaps` feature is enabled, this uses `load_embedded`
    /// method from `hayro-cmap` by default, which embeds the cmap files for
    /// all 61 predefined cmaps
    /// that the PDF specification requires to be readily available on a system.
    /// Otherwise, you can implement your custom logic for lazily fetching the
    /// data. If you are fine not supporting such PDFs, you can simply pass a closure
    /// that always returns `None`.
    pub cmap_resolver: CMapResolverFn,
    /// In certain cases, `hayro` will emit a warning in case an issue was encountered while interpreting
    /// the PDF file. Providing a callback allows you to catch those warnings and handle them, if desired.
    pub warning_sink: WarningSinkFn,
    /// Whether annotations should be rendered as well.
    ///
    /// Note that this feature is currently not fully implemented yet, so some
    /// annotations might be missing.
    pub render_annotations: bool,
    /// Which annotations are drawn.
    pub annotations: AnnotationSelection,
}

/// Which annotations a page is drawn with (PDF 32000-1 12.5.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AnnotationSelection {
    /// As for viewing: all but those flagged Hidden or `NoView`.
    #[default]
    View,
    /// As for printing: those flagged Print, but not Hidden.
    Print,
    /// As when annotations are flattened into page content for printing what the
    /// reader sees: all but links and those flagged Hidden or Invisible, whatever
    /// their Print and `NoView` flags.
    Flatten,
}

impl Default for InterpreterSettings {
    fn default() -> Self {
        Self {
            #[cfg(not(feature = "embed-fonts"))]
            font_resolver: Arc::new(|_| None),
            #[cfg(feature = "embed-fonts")]
            font_resolver: Arc::new(|query| Some(query.standard_substitute().get_font_data())),
            #[cfg(feature = "embed-cmaps")]
            cmap_resolver: Arc::new(hayro_cmap::load_embedded),
            #[cfg(not(feature = "embed-cmaps"))]
            cmap_resolver: Arc::new(|_| None),
            warning_sink: Arc::new(|_| {}),
            render_annotations: true,
            annotations: AnnotationSelection::View,
        }
    }
}

#[derive(Copy, Clone, Debug)]
/// Warnings that can occur while interpreting a PDF file.
#[non_exhaustive]
pub enum InterpreterWarning {
    /// A font could not be loaded.
    UnsupportedFont,
    /// An image failed to decode.
    ImageDecodeFailure,
    /// An annotation has no usable appearance stream, and none could be generated
    /// for it ([`annotation::Unsupported`]), so it was not drawn.
    UnsupportedAnnotation,
}

/// interpret the contents of the page and render them into the device, with its
/// annotations as they look on the page shown as `view` says: the view the caller
/// clips the page to ([`annotation::View`]).
pub fn interpret_page<'a>(
    page: &Page<'a>,
    view: &annotation::View,
    context: &mut Context<'a>,
    device: &mut impl Device<'a>,
) {
    let resources = page.resources();
    // Decoded for this call alone, so that a document kept open while page after
    // page is drawn does not hold every page's content.
    let content = page.read_page_stream().unwrap_or_default();
    interpret(TypedIter::new(&content), resources, context, device);

    if context.settings.render_annotations && page.raw().contains_key(ANNOTS) {
        // The fields of the document's forms are read once for all its pages.
        let appearances = annotation::Appearances::with_fields(
            page.xref(),
            context.settings.font_resolver.clone(),
            context.interpreter_cache.annotation_fields.clone(),
        );
        let drawn = appearances.page(page, context.settings.annotations, view);
        for annot in drawn.iter() {
            match annot.look() {
                annotation::Look::Stream { stream, placement } => {
                    // The box and matrix as the placement reads them, as a writer
                    // that copies the stream draws it.
                    let Some(apx) = FormXObject::appearance(
                        stream,
                        Affine::new(placement.matrix().map(f64::from)),
                        placement.bbox(),
                    ) else {
                        continue;
                    };

                    context.save_state();
                    context.pre_concat_affine(Affine::new(placement.transform().map(f64::from)));
                    context.push_root_transform();

                    apx.draw(resources, context, device);
                    context.pop_root_transform();
                    context.restore_state(device);
                }
                annotation::Look::Generated(appearance) => {
                    draw_generated(appearance, resources, context, device);
                }
                annotation::Look::Nothing => {}
                annotation::Look::Unsupported(_) => {
                    (context.settings.warning_sink)(InterpreterWarning::UnsupportedAnnotation);
                }
            }
        }
    }
}

/// Draws an appearance generated for an annotation without one, from a fresh
/// graphics state in default user space with its opacity and blend mode, which apply
/// to each thing it paints.
fn draw_generated<'a>(
    appearance: &annotation::Appearance,
    resources: &Resources<'a>,
    context: &mut Context<'a>,
    device: &mut impl Device<'a>,
) {
    context.path_mut().truncate(0);
    context.save_state();
    let paint = state::PaintState {
        alpha: appearance.opacity,
        ..Default::default()
    };
    *context.fill_paint_mut() = paint.clone();
    *context.stroke_paint_mut() = paint;
    *context.stroke_props_mut() = StrokeProps::default();
    *context.compositing_mut() = state::CompositingState {
        blend_mode: appearance.blend,
        ..Default::default()
    };
    let glyphs = appearance
        .glyphs
        .iter()
        .map(|g| (g.name_rc(), g.clone()))
        .collect();
    let outer = std::mem::replace(&mut context.generated_glyphs, glyphs);
    interpret(
        TypedIter::new(appearance.content.as_bytes()),
        resources,
        context,
        device,
    );
    context.generated_glyphs = outer;
    context.restore_state(device);
}

/// Interpret the instructions from `ops` and render them into the device.
pub fn interpret<'a>(
    mut ops: TypedIter<'_>,
    resources: &Resources<'a>,
    context: &mut Context<'a>,
    device: &mut impl Device<'a>,
) {
    let num_states = context.num_states();
    let floor = context.ocg_state.open();
    let outer_text_clip = core::mem::take(&mut context.text_clip);
    let mut font_dict_cache = hash::Map::<Name<'a>, Dict<'a>>::default();

    context.save_state();

    while let Some(op) = ops.next() {
        match op {
            TypedInstruction::SaveState(_) => context.save_state(),
            TypedInstruction::StrokeColorDeviceRgb(s) => {
                context.stroke_paint_mut().color_space = ColorSpace::device_rgb();
                context.stroke_paint_mut().color =
                    smallvec![s.0.as_f32(), s.1.as_f32(), s.2.as_f32()];
                context.stroke_paint_mut().pattern = None;
            }
            TypedInstruction::StrokeColorDeviceGray(s) => {
                context.stroke_paint_mut().color_space = ColorSpace::device_gray();
                context.stroke_paint_mut().color = smallvec![s.0.as_f32()];
                context.stroke_paint_mut().pattern = None;
            }
            TypedInstruction::StrokeColorCmyk(s) => {
                context.stroke_paint_mut().color_space = ColorSpace::device_cmyk();
                context.stroke_paint_mut().color =
                    smallvec![s.0.as_f32(), s.1.as_f32(), s.2.as_f32(), s.3.as_f32()];
                context.stroke_paint_mut().pattern = None;
            }
            TypedInstruction::LineWidth(w) => {
                context.stroke_props_mut().line_width = w.0.as_f32();
            }
            TypedInstruction::LineCap(c) => {
                context.stroke_props_mut().line_cap = convert_line_cap(c);
            }
            TypedInstruction::LineJoin(j) => {
                context.stroke_props_mut().line_join = convert_line_join(j);
            }
            TypedInstruction::MiterLimit(l) => {
                context.stroke_props_mut().miter_limit = l.0.as_f32();
            }
            TypedInstruction::Transform(t) => {
                context.pre_concat_transform(t);
            }
            TypedInstruction::RectPath(r) => {
                let rect = kurbo::Rect::new(
                    r.0.as_f64(),
                    r.1.as_f64(),
                    r.0.as_f64() + r.2.as_f64(),
                    r.1.as_f64() + r.3.as_f64(),
                )
                .to_path(0.1);
                context.path_mut().extend(rect);
                // `re` is `x y m ... h`: the current point is the rectangle's origin.
                let origin = Point::new(r.0.as_f64(), r.1.as_f64());
                *(context.last_point_mut()) = origin;
                *(context.sub_path_start_mut()) = origin;
            }
            TypedInstruction::MoveTo(m) => {
                let p = Point::new(m.0.as_f64(), m.1.as_f64());
                *(context.last_point_mut()) = p;
                *(context.sub_path_start_mut()) = p;
                context.path_mut().move_to(p);
            }
            TypedInstruction::FillPathEvenOdd(_) => {
                fill_path(context, device, FillRule::EvenOdd);
            }
            TypedInstruction::FillPathNonZero(_) => {
                fill_path(context, device, FillRule::NonZero);
            }
            TypedInstruction::FillPathNonZeroCompatibility(_) => {
                fill_path(context, device, FillRule::NonZero);
            }
            TypedInstruction::FillAndStrokeEvenOdd(_) => {
                fill_stroke_path(context, device, FillRule::EvenOdd);
            }
            TypedInstruction::FillAndStrokeNonZero(_) => {
                fill_stroke_path(context, device, FillRule::NonZero);
            }
            TypedInstruction::CloseAndStrokePath(_) => {
                close_path(context);
                stroke_path(context, device);
            }
            TypedInstruction::CloseFillAndStrokeEvenOdd(_) => {
                close_path(context);
                fill_stroke_path(context, device, FillRule::EvenOdd);
            }
            TypedInstruction::CloseFillAndStrokeNonZero(_) => {
                close_path(context);
                fill_stroke_path(context, device, FillRule::NonZero);
            }
            TypedInstruction::NonStrokeColorDeviceGray(s) => {
                context.fill_paint_mut().color_space = ColorSpace::device_gray();
                context.fill_paint_mut().color = smallvec![s.0.as_f32()];
                context.fill_paint_mut().pattern = None;
            }
            TypedInstruction::NonStrokeColorDeviceRgb(s) => {
                context.fill_paint_mut().color_space = ColorSpace::device_rgb();
                context.fill_paint_mut().color =
                    smallvec![s.0.as_f32(), s.1.as_f32(), s.2.as_f32()];
                context.fill_paint_mut().pattern = None;
            }
            TypedInstruction::NonStrokeColorCmyk(s) => {
                context.fill_paint_mut().color_space = ColorSpace::device_cmyk();
                context.fill_paint_mut().color =
                    smallvec![s.0.as_f32(), s.1.as_f32(), s.2.as_f32(), s.3.as_f32()];
                context.fill_paint_mut().pattern = None;
            }
            TypedInstruction::LineTo(m) => {
                if !context.path().elements().is_empty() {
                    let last_point = *context.last_point();
                    let mut p = Point::new(m.0.as_f64(), m.1.as_f64());
                    *(context.last_point_mut()) = p;
                    if last_point == p {
                        // Add a small delta so that zero width lines can still have a round stroke.
                        p.x += 0.0001;
                    }

                    context.path_for_segment().line_to(p);
                }
            }
            TypedInstruction::CubicTo(c) => {
                if !context.path().elements().is_empty() {
                    let p1 = Point::new(c.0.as_f64(), c.1.as_f64());
                    let p2 = Point::new(c.2.as_f64(), c.3.as_f64());
                    let p3 = Point::new(c.4.as_f64(), c.5.as_f64());

                    *(context.last_point_mut()) = p3;

                    context.path_for_segment().curve_to(p1, p2, p3);
                }
            }
            TypedInstruction::CubicStartTo(c) => {
                if !context.path().elements().is_empty() {
                    let p1 = *context.last_point();
                    let p2 = Point::new(c.0.as_f64(), c.1.as_f64());
                    let p3 = Point::new(c.2.as_f64(), c.3.as_f64());

                    *(context.last_point_mut()) = p3;

                    context.path_for_segment().curve_to(p1, p2, p3);
                }
            }
            TypedInstruction::CubicEndTo(c) => {
                if !context.path().elements().is_empty() {
                    let p2 = Point::new(c.0.as_f64(), c.1.as_f64());
                    let p3 = Point::new(c.2.as_f64(), c.3.as_f64());

                    *(context.last_point_mut()) = p3;

                    context.path_for_segment().curve_to(p2, p3, p3);
                }
            }
            TypedInstruction::ClosePath(_) => {
                close_path(context);
            }
            TypedInstruction::SetGraphicsState(gs) => {
                if let Some(gs) = resources
                    .get_ext_g_state(gs.0)
                    .warn_none(&format!("failed to get extgstate {}", gs.0.as_str()))
                {
                    handle_gs(&gs, context, resources);
                }
            }
            TypedInstruction::StrokePath(_) => {
                stroke_path(context, device);
            }
            TypedInstruction::EndPath(_) => end_path(context, device),
            TypedInstruction::NonStrokeColor(c) => {
                let paint = context.fill_paint_mut();
                paint.color = c.0.into_iter().map(|n| n.as_f32()).collect();
                paint.pattern = None;
            }
            TypedInstruction::StrokeColor(c) => {
                let paint = context.stroke_paint_mut();
                paint.color = c.0.into_iter().map(|n| n.as_f32()).collect();
                paint.pattern = None;
            }
            TypedInstruction::ClipNonZero(_) => {
                *(context.clip_mut()) = Some(FillRule::NonZero);
            }
            TypedInstruction::ClipEvenOdd(_) => {
                *(context.clip_mut()) = Some(FillRule::EvenOdd);
            }
            TypedInstruction::RestoreState(_) => context.restore_state(device),
            TypedInstruction::FlatnessTolerance(_) => {
                // Ignore for now.
            }
            TypedInstruction::ColorSpaceStroke(c) => {
                let cs = if let Some(named) = ColorSpace::new_from_name(c.0) {
                    named
                } else {
                    context
                        .get_color_space(resources, c.0)
                        .unwrap_or(ColorSpace::device_gray())
                };

                if !cs.is_pattern() {
                    context.stroke_paint_mut().pattern = None;
                }
                context.stroke_paint_mut().color = cs.initial_color();
                context.stroke_paint_mut().color_space = cs;
            }
            TypedInstruction::ColorSpaceNonStroke(c) => {
                let cs = if let Some(named) = ColorSpace::new_from_name(c.0) {
                    named
                } else {
                    context
                        .get_color_space(resources, c.0)
                        .unwrap_or(ColorSpace::device_gray())
                };

                if !cs.is_pattern() {
                    context.fill_paint_mut().pattern = None;
                }
                context.fill_paint_mut().color = cs.initial_color();
                context.fill_paint_mut().color_space = cs;
            }
            TypedInstruction::DashPattern(p) => {
                context.stroke_props_mut().dash_offset = p.1.as_f32();
                context.stroke_props_mut().dash_array = convert_dash_array(p.0);
            }
            TypedInstruction::RenderingIntent(_) => {
                // Ignore for now.
            }
            TypedInstruction::NonStrokeColorNamed(n) => {
                context.fill_paint_mut().color = n.0.into_iter().map(|n| n.as_f32()).collect();
                context.fill_paint_mut().pattern = n.1.and_then(|name| {
                    resources
                        .get_pattern(name)
                        .and_then(|d| Pattern::new(d, context))
                });
            }
            TypedInstruction::StrokeColorNamed(n) => {
                context.stroke_paint_mut().color = n.0.into_iter().map(|n| n.as_f32()).collect();
                context.stroke_paint_mut().pattern = n.1.and_then(|name| {
                    resources
                        .get_pattern(name)
                        .and_then(|d| Pattern::new(d, context))
                });
            }
            TypedInstruction::BeginMarkedContentWithProperties(bdc) => {
                // Properties can be either:
                // 1. A Name that references an entry in the Resources/Properties dictionary
                // 2. An inline property-list dictionary

                let properties = bdc
                    .1
                    .clone()
                    .into_name()
                    .and_then(|name| resources.properties.get::<Dict<'_>>(name))
                    .or_else(|| dict_or_stream(bdc.1).map(|(props, _)| props.clone()));

                // Only the `OC` tag of a property list the resources name marks
                // optional content, as `PDFium` reads it
                // (`CPDF_OCContext::CheckPageObjectVisible`), and a level begins only
                // for a property list there is
                // (`CPDF_StreamContentParser::Handle_BeginMarkedContent_Dictionary`):
                // the `EMC` of one there is not ends the level before it.
                let begun = bdc.0.as_ref() == OC
                    && bdc.1.clone().into_name().is_some_and(|name| {
                        context.ocg_state.begin_named(
                            &Kept::Document(resources.properties.clone()),
                            name.as_ref(),
                        )
                    });
                if !begun && properties.is_some() {
                    context.ocg_state.begin_marked_content();
                }

                device.begin_marked_content(bdc.0, &MarkedContentProperties::new(properties));
            }
            TypedInstruction::MarkedContentPointWithProperties(_) => {}
            TypedInstruction::EndMarkedContent(_) => {
                context.ocg_state.end_marked_content();
                device.end_marked_content();
            }
            TypedInstruction::MarkedContentPoint(_) => {}
            TypedInstruction::BeginMarkedContent(bmc) => {
                context.ocg_state.begin_marked_content();
                device.begin_marked_content(bmc.0, &MarkedContentProperties::default());
            }
            TypedInstruction::BeginText(_) => {
                context.text_state_mut().text_matrix = Affine::IDENTITY;
                context.text_state_mut().text_line_matrix = Affine::IDENTITY;
            }
            TypedInstruction::SetTextMatrix(m) => {
                let m = Affine::new([
                    m.0.as_f64(),
                    m.1.as_f64(),
                    m.2.as_f64(),
                    m.3.as_f64(),
                    m.4.as_f64(),
                    m.5.as_f64(),
                ]);
                context.text_state_mut().text_line_matrix = m;
                context.text_state_mut().text_matrix = m;
            }
            TypedInstruction::EndText(_) => {
                if let Some(clip_path) = context.text_clip.take()
                    && context.text_state().render_mode.clips()
                {
                    context.push_clip_path(clip_path, FillRule::NonZero, device);
                }
            }
            TypedInstruction::TextFont(t) => {
                let name = t.0;

                // In case we are unable to resolve the font, two scenarios:
                // 1) If the font doesn't exist in the first place in the resource dictionary,
                // assume Helvetica (this seems to be what other PDF viewers do).
                // 2) In case it's `None` because we were unable to resolve the font
                // (for whatever reason), leave it as `None`. Better showing no
                // text at all than garbage text.
                let font = if let Some(font_dict) = font_dict_cache.get(name).cloned() {
                    context.resolve_font(&font_dict)
                } else if let Some(font_dict) = resources.get_font(name) {
                    font_dict_cache.insert(name.clone(), font_dict.clone());
                    context.resolve_font(&font_dict)
                } else {
                    Font::new_standard(StandardFont::Helvetica, &context.settings.font_resolver)
                        .map(TextStateFont::Fallback)
                };

                context.text_state_mut().font_size = t.1.as_f32();
                context.text_state_mut().font = font;
            }
            TypedInstruction::ShowText(s) => {
                if context.text_state().font.is_none() {
                    // Even if no explicit font was set, we try to assume Helvetica. Acrobat
                    // seems to do the same.
                    context.text_state_mut().font = Font::new_standard(
                        StandardFont::Helvetica,
                        &context.settings.font_resolver,
                    )
                    .map(TextStateFont::Fallback);
                }

                text::show_text_string(context, device, resources, s.0);
            }
            TypedInstruction::ShowTexts(s) => {
                let had_font = context.text_state().font.is_some();
                if !had_font {
                    // Even if no explicit font was set, we try to assume Helvetica. Acrobat
                    // seems to do the same.
                    context.text_state_mut().font = Font::new_standard(
                        StandardFont::Helvetica,
                        &context.settings.font_resolver,
                    )
                    .map(TextStateFont::Fallback);
                }

                let text_matrix = context.text_state().text_matrix;
                text::begin_glyph_run(context);
                let mut drawn = true;
                for obj in s.0.iter::<Object<'_>>() {
                    match obj {
                        Object::Number(num) => {
                            text::apply_glyph_run_adjustment(context, num.as_f32());
                        }
                        Object::String(text) => {
                            text::append_text_string(context, resources, &text);
                        }
                        _ => {
                            drawn = false;
                            break;
                        }
                    }
                }
                if drawn {
                    text::show_glyph_run(context, device);
                } else {
                    // An array with an entry that is neither a number nor a string
                    // draws nothing and moves the text on by nothing, as `MuPDF`
                    // (`PyMuPDF` 1.28.2) draws it: the glyphs gathered before the
                    // entry are dropped and the text state is as before the array.
                    text::begin_glyph_run(context);
                    let state = context.text_state_mut();
                    state.text_matrix = text_matrix;
                    if !had_font {
                        state.font = None;
                    }
                }
            }
            TypedInstruction::HorizontalScaling(h) => {
                context.text_state_mut().horizontal_scaling = h.0.as_f32();
            }
            TypedInstruction::TextLeading(tl) => {
                context.text_state_mut().leading = tl.0.as_f32();
            }
            TypedInstruction::CharacterSpacing(c) => {
                context.text_state_mut().char_space = c.0.as_f32();
            }
            TypedInstruction::WordSpacing(w) => {
                context.text_state_mut().word_space = w.0.as_f32();
            }
            TypedInstruction::NextLine(n) => {
                let (tx, ty) = (n.0.as_f64(), n.1.as_f64());
                text::next_line(context, tx, ty);
            }
            TypedInstruction::NextLineUsingLeading(_) => {
                text::next_line(context, 0.0, -context.text_state().leading as f64);
            }
            TypedInstruction::NextLineAndShowText(n) => {
                text::next_line(context, 0.0, -context.text_state().leading as f64);
                text::show_text_string(context, device, resources, n.0);
            }
            TypedInstruction::TextRenderingMode(r) => {
                let mode = match r.0.as_i64() {
                    0 => TextRenderingMode::Fill,
                    1 => TextRenderingMode::Stroke,
                    2 => TextRenderingMode::FillStroke,
                    3 => TextRenderingMode::Invisible,
                    4 => TextRenderingMode::FillAndClip,
                    5 => TextRenderingMode::StrokeAndClip,
                    6 => TextRenderingMode::FillAndStrokeAndClip,
                    7 => TextRenderingMode::Clip,
                    _ => {
                        warn!("unknown text rendering mode {}", r.0.as_i64());

                        TextRenderingMode::Fill
                    }
                };

                context.text_state_mut().render_mode = mode;
            }
            TypedInstruction::NextLineAndSetLeading(n) => {
                let (tx, ty) = (n.0.as_f64(), n.1.as_f64());
                context.text_state_mut().leading = -ty as f32;
                text::next_line(context, tx, ty);
            }
            TypedInstruction::ShapeGlyph(_) => {}
            TypedInstruction::XObject(x) => {
                // A glyph of the generated appearance being drawn.
                let glyph = std::str::from_utf8(x.0.as_ref())
                    .ok()
                    .and_then(|name| context.generated_glyphs.get(name).cloned());
                if let Some(glyph) = glyph {
                    interpret(
                        TypedIter::new(glyph.content().as_bytes()),
                        resources,
                        context,
                        device,
                    );
                    continue;
                }
                let cache = context.interpreter_cache.object_cache.clone();
                let warning_sink = context.settings.warning_sink.clone();
                let transfer_function = context.compositing().transfer_function.clone();
                if let Some(x_object) = resources.get_x_object(x.0).and_then(|s| {
                    XObject::new(
                        &s,
                        |name| context.get_color_space(resources, name),
                        &warning_sink,
                        &cache,
                        transfer_function.clone(),
                    )
                }) {
                    x_object.draw(resources, context, device);
                }
            }
            TypedInstruction::InlineImage(i) => {
                let warning_sink = context.settings.warning_sink.clone();
                let transfer_function = context.compositing().transfer_function.clone();
                let cache = context.interpreter_cache.object_cache.clone();
                if let Some(x_object) = ImageXObject::new(
                    i.0,
                    |name| context.get_color_space(resources, name),
                    &warning_sink,
                    &cache,
                    transfer_function,
                ) {
                    x_object.draw(context, device, &Kept::Content(i.0.dict().clone()));
                }
            }
            TypedInstruction::TextRise(t) => {
                context.text_state_mut().rise = t.0.as_f32();
            }
            TypedInstruction::Shading(s) => {
                if !context.ocg_state.is_visible() {
                    continue;
                }

                let transfer_function = context.compositing().transfer_function.clone();

                if let Some(sp) = resources
                    .get_shading(s.0)
                    .and_then(|o| {
                        let (dict, stream) = dict_or_stream(&o)?;
                        Shading::new(dict, stream, &context.interpreter_cache.object_cache)
                    })
                    .map(|s| {
                        Pattern::Shading(ShadingPattern {
                            shading: Arc::new(s),
                            matrix: Affine::IDENTITY,
                            // Set where it paints.
                            opacity: 1.0,
                            transfer_function: transfer_function.clone(),
                        })
                    })
                {
                    context.save_state();
                    context.push_root_transform();
                    let paint = context.fill_paint_mut();
                    paint.pattern = Some(sp);
                    paint.color_space = ColorSpace::pattern();

                    let bbox = context.bbox().to_path(0.1);
                    let inverted_bbox = context.ctm().inverse() * bbox;
                    fill_path_impl(context, device, FillRule::NonZero, Some(&inverted_bbox));

                    context.pop_root_transform();
                    context.restore_state(device);
                } else {
                    warn!("failed to process shading");
                }
            }
            TypedInstruction::BeginCompatibility(_) => {}
            TypedInstruction::EndCompatibility(_) => {}
            TypedInstruction::ColorGlyph(_) => {}
            TypedInstruction::ShowTextWithParameters(t) => {
                context.text_state_mut().word_space = t.0.as_f32();
                context.text_state_mut().char_space = t.1.as_f32();
                text::next_line(context, 0.0, -context.text_state().leading as f64);
                text::show_text_string(context, device, resources, t.2);
            }
            _ => {
                warn!("failed to read an operator");
            }
        }
    }

    // A path object, including a pending `W` or `W*`, can't continue beyond the content
    // stream it starts in.
    context.path_mut().truncate(0);
    *(context.clip_mut()) = None;

    while context.num_states() > num_states {
        context.restore_state(device);
    }
    context.ocg_state.close(floor);
    context.text_clip = outer_text_clip;
}
