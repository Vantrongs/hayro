use crate::context::{Context, path_as_rect};
use crate::device::Device;
use crate::util::{BezPathExt, Float32Ext, x_y_advances};
use crate::{BlendMode, DrawMode, DrawProps, FillRule, Knockout, StrokeProps};
use kurbo::{BezPath, Cap, Join, PathEl};

pub(crate) fn fill_path<'a>(
    context: &mut Context<'a>,
    device: &mut impl Device<'a>,
    fill_rule: FillRule,
) {
    fill_path_impl(context, device, fill_rule, None);
    end_path(context, device);
}

pub(crate) fn stroke_path<'a>(context: &mut Context<'a>, device: &mut impl Device<'a>) {
    stroke_path_impl(context, device, None, Knockout::None);
    end_path(context, device);
}

/// Fills and then strokes the current path, as one operator does. With one opacity
/// for both, translucent, blended or in a soft mask, viewers draw the pair opaque in a
/// group drawn at that opacity, blend mode and soft mask (observed in `PyMuPDF`
/// 1.28.2). Translucent or blended, the group is a knockout group: where the stroke
/// covers the fill, only the stroke shows, its paint included (a tiling cell or a
/// shading that paints translucently). Translucent, a pixel the stroke covers in part
/// shows less of either ([`Knockout::ClearThenOver`]); opaque and blended, it is as
/// opaque as the pair ([`Knockout::Replace`]). Opaque and unblended, in a soft mask
/// or not, the stroke is drawn over the fill.
pub(crate) fn fill_stroke_path<'a>(
    context: &mut Context<'a>,
    device: &mut impl Device<'a>,
    fill_rule: FillRule,
) {
    let alpha = context.fill_paint().alpha;
    let compositing = context.compositing();
    let group = alpha == context.stroke_paint().alpha
        && (alpha < 1.0
            || compositing.blend_mode != BlendMode::Normal
            || compositing.soft_mask.is_some())
        && context.ocg_state.is_visible();
    if group {
        let (mask, blend) = (compositing.soft_mask.clone(), compositing.blend_mode);
        device.push_transparency_group(alpha, mask, blend);
        let compositing = context.compositing_mut();
        let saved = (compositing.soft_mask.take(), compositing.blend_mode);
        compositing.blend_mode = BlendMode::Normal;
        context.fill_paint_mut().alpha = 1.0;
        context.stroke_paint_mut().alpha = 1.0;
        fill_path_impl(context, device, fill_rule, None);
        let knockout = if alpha < 1.0 {
            Knockout::ClearThenOver
        } else if blend != BlendMode::Normal {
            Knockout::Replace
        } else {
            Knockout::None
        };
        stroke_path_impl(context, device, None, knockout);
        let compositing = context.compositing_mut();
        (compositing.soft_mask, compositing.blend_mode) = saved;
        context.fill_paint_mut().alpha = alpha;
        context.stroke_paint_mut().alpha = alpha;
        device.pop_transparency_group();
    } else {
        fill_path_impl(context, device, fill_rule, None);
        stroke_path_impl(context, device, None, Knockout::None);
    }

    end_path(context, device);
}

pub(crate) fn end_path<'a>(context: &mut Context<'a>, device: &mut impl Device<'a>) {
    if let Some(clip) = context.clip_mut().take()
        && !context.path().elements().is_empty()
    {
        let clip_path = context.ctm() * context.path().clone();
        context.push_clip_path(clip_path, clip, device);
    }

    context.path_mut().truncate(0);
}

pub(crate) fn close_path(context: &mut Context<'_>) {
    // This is necessary to prevent artifacts (see for example issue 157),
    // but it does cause some weird lines (maybe conflation artifacts) in
    // pdftc_900k_0907.
    if context.path().elements().last() != Some(&PathEl::ClosePath) && !context.path().is_empty() {
        context.path_mut().close_path();

        *(context.last_point_mut()) = *context.sub_path_start();
    }
}

pub(crate) fn fill_path_impl<'a>(
    context: &mut Context<'a>,
    device: &mut impl Device<'a>,
    fill_rule: FillRule,
    path: Option<&BezPath>,
) {
    if !context.ocg_state.is_visible() {
        return;
    }

    let props = context.draw_props(false);

    let mut draw = |path: &BezPath| {
        // pdf.js issue 4260: Replace zero-sized paths with a small stroke instead.
        let bbox = path.fast_bounding_box();
        let (x_advance, y_advance) = x_y_advances(&props.transform);
        match (
            ((bbox.width() * x_advance.length()) as f32).is_nearly_zero(),
            ((bbox.height() * y_advance.length()) as f32).is_nearly_zero(),
        ) {
            (false, false) => {
                let draw_mode = DrawMode::Fill(fill_rule);
                if let Some(rect) = path_as_rect(path) {
                    device.draw_rect(&rect, props.clone(), &draw_mode);
                } else {
                    device.draw_path(path, props.clone(), &draw_mode);
                }
            }
            _ => {
                let mut path = BezPath::new();
                path.move_to((bbox.x0, bbox.y0));
                path.line_to((bbox.x1, bbox.y1));

                let scale = x_advance.length().max(y_advance.length()) as f32;
                let stroke_props = StrokeProps {
                    line_width: if scale > 0.0 { 1.0 / scale } else { 0.001 },
                    line_join: Join::Bevel,
                    line_cap: Cap::Butt,
                    ..Default::default()
                };

                device.draw_path(&path, props.clone(), &DrawMode::Stroke(stroke_props));
            }
        };
    };

    match path {
        None => draw(context.path()),
        Some(path) => draw(path),
    };
}

pub(crate) fn stroke_path_impl<'a>(
    context: &mut Context<'a>,
    device: &mut impl Device<'a>,
    path: Option<&BezPath>,
    knockout: Knockout,
) {
    if !context.ocg_state.is_visible() {
        return;
    }

    let stroke_props = context.stroke_props().clone();
    let props = DrawProps {
        knockout,
        ..context.draw_props(true)
    };

    let path = path.unwrap_or(context.path());
    let draw_mode = DrawMode::Stroke(stroke_props);

    if let Some(rect) = path_as_rect(path) {
        device.draw_rect(&rect, props, &draw_mode);
    } else {
        device.draw_path(path, props, &draw_mode);
    }
}
