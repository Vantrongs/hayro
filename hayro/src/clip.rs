use crate::{Renderer, convert_fill_rule};
use hayro_interpret::{ClipPath, FillRule};
use kurbo::{Affine, BezPath, Rect, Shape};

impl Renderer<'_> {
    pub(super) fn push_clip_path_inner(&mut self, clip_path: &BezPath, fill: FillRule) {
        let old_transform = *self.ctx.transform();

        self.ctx.set_fill_rule(convert_fill_rule(fill));
        self.ctx.set_transform(Affine::IDENTITY);
        self.ctx.push_clip_path(clip_path);

        self.ctx.set_transform(old_transform);
    }

    pub(super) fn push_clip_path(&mut self, clip_path: &ClipPath) {
        self.push_clip_path_of(&clip_path.path, clip_path.fill, None);
    }

    /// Pushes `path` as a clip, where it is the rectangle `rect` if one is given.
    pub(super) fn push_clip_path_of(&mut self, path: &BezPath, fill: FillRule, rect: Option<Rect>) {
        self.push_clip_path_inner(path, fill);
        self.push_clips(ClipBounds {
            bounds: rect.unwrap_or_else(|| path.bounding_box()),
            rects: rect.is_some(),
        });
    }

    pub(super) fn push_clip_rect(&mut self, rect: &Rect) {
        self.ctx.set_fill_rule(convert_fill_rule(FillRule::NonZero));
        self.ctx.set_transform(Affine::IDENTITY);
        self.ctx.push_clip_rect(rect);
        self.push_clips(ClipBounds {
            bounds: *rect,
            rects: true,
        });
    }

    pub(super) fn pop_clip(&mut self) {
        self.ctx.pop_clip();
        self.clips.pop();
    }

    fn push_clips(&mut self, clip: ClipBounds) {
        let clips = match self.clips.last() {
            Some(outer) => ClipBounds {
                bounds: outer.bounds.intersect(clip.bounds),
                rects: outer.rects && clip.rects,
            },
            None => clip,
        };
        self.clips.push(clips);
    }

    /// A clip for the layer of a group that draws only within `bounds`, whole device
    /// pixels, which limits the layer to them where that changes nothing the layer
    /// composites. A layer without a clip is composited across the width of the page
    /// for every row it draws in, which makes a group per small shape, as a knockout
    /// group per `B` is, cost as much as the page's width. `vello_cpu` clips a layer's
    /// clip by the clips pushed, and composites the layer through the result after
    /// drawing its contents through them, so a pixel they cover in part would be
    /// covered twice: the clip is only given where they leave a rectangle of whole
    /// pixels there, which they cover whole. Composited through a clip, the pixels
    /// along its edges may differ by a unit from those composited without one, so a
    /// group that may draw anywhere gets none.
    pub(super) fn layer_clip(&self, bounds: Rect) -> Option<BezPath> {
        let (rects, b) = match self.clips.last() {
            Some(clips) => (clips.rects, clips.bounds.intersect(bounds)),
            None => (true, bounds),
        };
        let whole = [b.x0, b.y0, b.x1, b.y1]
            .iter()
            .all(|v| v.is_finite() && v.floor() == *v);
        (rects && whole).then(|| b.to_path(0.1))
    }
}

/// The clips pushed on a device, as far as a layer's clip needs them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClipBounds {
    /// The intersection of their bounding boxes, in device space.
    bounds: Rect,
    /// Whether each is a rectangle, so that `bounds` is the region they leave.
    rects: bool,
}
