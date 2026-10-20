use crate::context::{Context, path_as_rect};
use crate::device::Device;
use crate::util::{BezPathExt, Float32Ext, x_y_advances};
use crate::{BlendMode, DrawMode, DrawProps, FillRule, Knockout, StrokeProps};
use kurbo::{Affine, BezPath, Cap, Join, PathEl, Rect, Shape};

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
        match painted_bounds(context.path(), context.ctm(), context.stroke_props()) {
            Some(bounds) => device.push_knockout_group(alpha, mask, blend, bounds),
            None => device.push_transparency_group(alpha, mask, blend),
        }
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

/// A rectangle of whole device pixels that holds every pixel the pair of `path`,
/// filled and stroked with `stroke` in `ctm`, paints, its antialiasing included:
/// what a group holding only the pair changes. The group composited outside it is
/// transparent there, which no opacity, blend mode or soft mask changes, so a
/// device may limit the group's layer, which a renderer otherwise composites over
/// the rows of the whole page, to these pixels. A stroke paints within half its width, times the miter limit at a
/// mitred join and √2 at a square cap, of the path, and renderers widen a stroke
/// thinner than a pixel to one; `None` where the bound is no finite rectangle.
fn painted_bounds(path: &BezPath, ctm: Affine, stroke: &StrokeProps) -> Option<Rect> {
    // Antialiasing, and a pixel to spare.
    const BEYOND: f64 = 2.0;
    let [a, b, c, d, _, _] = ctm.as_coeffs();
    let largest_scale = {
        let sum = a * a + b * b + c * c + d * d;
        let det = a * d - b * c;
        ((sum + (sum * sum - 4.0 * det * det).max(0.0).sqrt()) / 2.0).sqrt()
    };
    let corner = match stroke.line_join {
        Join::Miter => f64::from(stroke.miter_limit).max(core::f64::consts::SQRT_2),
        _ => core::f64::consts::SQRT_2,
    };
    let width = (largest_scale * f64::from(stroke.line_width).abs()).max(1.0);
    let reach = corner * width / 2.0 + BEYOND;
    let bounds = (ctm * path.clone()).bounding_box().inflate(reach, reach);
    let bounds = Rect::new(
        bounds.x0.floor(),
        bounds.y0.floor(),
        bounds.x1.ceil(),
        bounds.y1.ceil(),
    );
    (bounds.is_finite() && bounds.area() > 0.0).then_some(bounds)
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

#[cfg(test)]
mod tests {
    use super::painted_bounds;
    use crate::StrokeProps;
    use kurbo::{Affine, BezPath, Cap, Join, Shape, Stroke, StrokeOpts};

    /// The outline kurbo strokes each path into, transformed to the device, and its
    /// fill, lie in the bound, a pixel and more inside it, whatever the join, cap,
    /// dashes and transform.
    #[test]
    fn the_painted_bounds_hold_the_stroke_and_the_fill() {
        let paths = [
            "M10 10 L90 12 L11 14",
            "M10 10 L90 30 L10 50",
            "M10 10 L90 10 L90 90 L10 90 Z",
            "M50 50 C10 90 90 90 50 10",
            "M20 20 L20 20",
            "M5 5 L95 6 M40 60 L41 90",
        ];
        let ctms = [
            Affine::IDENTITY,
            Affine::scale(3.0),
            Affine::new([2.0, 0.5, -1.5, 0.25, 7.0, -3.0]),
            Affine::rotate(0.7).then_scale_non_uniform(0.05, 4.0),
            Affine::scale(0.01),
        ];
        for path in paths {
            let path = BezPath::from_svg(path).unwrap();
            for ctm in ctms {
                for line_join in [Join::Miter, Join::Round, Join::Bevel] {
                    for line_cap in [Cap::Butt, Cap::Round, Cap::Square] {
                        for (line_width, miter_limit, dashes) in [
                            (0.0, 10.0, &[][..]),
                            (6.0, 10.0, &[]),
                            (6.0, 1.5, &[3.0, 2.0]),
                        ] {
                            let props = StrokeProps {
                                line_width,
                                line_cap,
                                line_join,
                                miter_limit,
                                dash_array: dashes.iter().copied().collect(),
                                dash_offset: 0.0,
                            };
                            let bounds = painted_bounds(&path, ctm, &props).unwrap().inset(-1.0);
                            let mut stroke = Stroke::new(f64::from(line_width))
                                .with_join(line_join)
                                .with_caps(line_cap)
                                .with_miter_limit(f64::from(miter_limit));
                            if !dashes.is_empty() {
                                stroke = stroke.with_dashes(0.0, dashes.iter().map(|d| *d as f64));
                            }
                            let outline =
                                kurbo::stroke(path.iter(), &stroke, &StrokeOpts::default(), 0.01);
                            for drawn in [ctm * outline, ctm * path.clone()] {
                                if drawn.elements().is_empty() {
                                    continue;
                                }
                                let drawn = drawn.bounding_box();
                                assert!(
                                    bounds.contains_rect(drawn),
                                    "{path:?} {ctm:?} {props:?}: {drawn:?} not in {bounds:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
