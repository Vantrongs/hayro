//! The colors of axial and radial shadings by their parameter, as stops of a
//! gradient whose error is known.
//!
//! PDF shadings are expensive to evaluate: a function, a color space, a tint
//! transform's program. So their colors are found once at stops and looked up
//! between them, by the shading's own sampling and, for an axial shading, as an
//! SVG-like linear gradient a renderer draws natively. Where the color is
//! straight between two inputs of the function, as a sampled function's is
//! between its samples, the stops are those inputs, and exact
//! ([`Function::shape`](crate::Function)); each step of the function is two stops
//! at one place, the values from either side. Elsewhere the color is tabulated
//! at device resolution: at least a stop for each step the parameter takes
//! from a pixel to its neighbour ([`EncodedRadialAxialShading::grid_step`]), so a
//! feature of the function a pixel can show is between no two stops.

use crate::color::AlphaColor;
use crate::encode::{
    Cost, EncodedRadialAxialShading, EncodedShadingPattern, Exhausted, Meter, RadialAxialParams,
    Unmetered, take,
};
use crate::function::Values;
use crate::function::shape::Piece;
use kurbo::{Affine, Point, Rect};
use smallvec::smallvec;

/// How far, in 8-bit levels, a stop's color may lie off the line through its
/// neighbours before the intervals beside it are split
/// ([`EncodedRadialAxialShading::refine`]): more than the colors' own rounding
/// to 8 bits puts it off, a level at most. Lines between stops so placed stray
/// from a curve by about a fourth of it.
const BEND_LEVELS: f32 = 1.0;

/// [`BEND_LEVELS`] for colors not rounded, gray's and RGB's.
const STRAIGHT_BEND_LEVELS: f32 = 0.5;

/// Into how many parts at most refining splits a step of the grid.
const BEND_SPLITS: u16 = 16;

/// An SVG-like gradient.
pub struct SvgGradient {
    /// The kind of the gradient.
    pub kind: SvgGradientKind,
    /// The transform from encoded shading coordinates into the renderer's coordinate space.
    pub transform: Affine,
    /// The stops of the gradient in the sRGB color space, from the stop at or
    /// before the start of `range` to the one at or past its end.
    pub stops: Vec<SvgGradientStop>,
    /// The part of the gradient, from its start, 0, to its end, 1, the shape it
    /// was made for reaches: past it the stops do not tell the shading's colors.
    pub range: (f32, f32),
    /// The most the gradient's parameter changes over a unit of the space it is
    /// drawn in, a device pixel.
    pub pixel_step: f32,
}

/// The kind of the SVG gradient.
pub enum SvgGradientKind {
    /// Linear gradient.
    Linear {
        /// The normalized start point of the gradient line.
        start: Point,
        /// The normalized end point of the gradient line.
        end: Point,
    },
}

/// A color stop for an SVG-like gradient.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvgGradientStop {
    /// Normalized offset.
    pub offset: f32,
    /// RGBA color components in sRGB.
    pub color: [f32; 4],
}

impl SvgGradient {
    /// Whether a renderer that paints a point at `t` the entry
    /// `trunc(t × (lut.len() - 1))` of `lut`, premultiplied colors of 8 bits in
    /// equal steps from 0 to 1, paints every point of `range` within a level of
    /// these stops, as their colors rounded to 8 bits: within 1.5 levels of them
    /// at every place an entry is read for. vello's gradients are drawn so. A
    /// step of the stops within an entry may be missed by as much as it steps
    /// where an entry is no longer than a pixel's step, so the step lies within
    /// a pixel of each point painted wrong; a longer one fails.
    pub fn paints_within_a_level(&self, lut: &[[u8; 4]]) -> bool {
        let n = lut.len();
        if n < 2 || self.stops.is_empty() {
            return false;
        }
        let cell = 1.0 / (n - 1) as f64;
        let (lo, hi) = (f64::from(self.range.0), f64::from(self.range.1));
        let first = ((lo / cell).floor() as usize).min(n - 1);
        let last = ((hi / cell).floor() as usize).min(n - 1);
        let stops = &self.stops;
        for (i, entry) in lut.iter().enumerate().take(last + 1).skip(first) {
            let (a, b) = ((i as f64 * cell).max(lo), ((i + 1) as f64 * cell).min(hi));
            let inside = stops.partition_point(|s| f64::from(s.offset) < a)
                ..stops.partition_point(|s| f64::from(s.offset) <= b);
            let steps = stops[inside.clone()]
                .windows(2)
                .any(|w| w[0].offset == w[1].offset && w[0].color != w[1].color);
            if steps && cell <= f64::from(self.pixel_step) {
                continue;
            }
            let mut colors = vec![
                lerp_at(stops, a as f32, false),
                lerp_at(stops, b as f32, true),
            ];
            colors.extend(stops[inside].iter().map(|s| s.color));
            for color in colors {
                let alpha = f64::from(color[3]);
                let premultiplied = [
                    f64::from(color[0]) * alpha,
                    f64::from(color[1]) * alpha,
                    f64::from(color[2]) * alpha,
                    alpha,
                ];
                if premultiplied
                    .iter()
                    .zip(entry)
                    .any(|(want, &got)| (f64::from(got) - want * 255.0).abs() >= 1.5)
                {
                    return false;
                }
            }
        }
        true
    }
}

impl EncodedRadialAxialShading {
    /// Convert an axial shading into an SVG-like gradient over the part of it
    /// `path_bbox` reaches.
    pub fn as_svg_gradient(
        &self,
        pattern: &EncodedShadingPattern,
        path_bbox: Rect,
    ) -> Option<SvgGradient> {
        self.as_svg_gradient_metered(pattern, path_bbox, &mut Unmetered)
            .expect("an unmetered conversion is never refused")
    }

    /// [`as_svg_gradient`](Self::as_svg_gradient), taking its work and memory of
    /// `meter` before it does it ([`stops_metered`](Self::stops_metered)).
    /// `Exhausted` when `meter` refuses; `Ok(None)` for a shading no such
    /// gradient draws: a radial one, or an axial one whose ends `path_bbox`
    /// reaches past where it does not extend.
    pub fn as_svg_gradient_metered(
        &self,
        pattern: &EncodedShadingPattern,
        path_bbox: Rect,
        meter: &mut dyn Meter,
    ) -> Result<Option<SvgGradient>, Exhausted> {
        // A two-point radial gradient of an SVG renderer tells a cone from
        // circles that touch within a tolerance and paints in `f32`, as vello's
        // does: it paints near-touching circles, and circles of a thousandth of
        // a unit, as others, and points 10^9 units out as on their neighbours'
        // circles. The shading paints its points itself.
        let RadialAxialParams::Axial = self.params else {
            return Ok(None);
        };
        let reached = pattern.base_transform.transform_rect_bbox(path_bbox);
        let (min_x, max_x) = (reached.min_x(), reached.max_x());
        if !((self.extend[0] || min_x >= 0.0) && (self.extend[1] || max_x <= 1.0)) {
            return Ok(None);
        }
        let range = (min_x.clamp(0.0, 1.0) as f32, max_x.clamp(0.0, 1.0) as f32);
        let [a, _, c, _, _, _] = pattern.base_transform.as_coeffs();
        take(meter, pattern.setup_cost())?;
        Ok(Some(SvgGradient {
            kind: SvgGradientKind::Linear {
                start: Point::ZERO,
                end: Point::new(1.0, 0.0),
            },
            transform: pattern.base_transform.inverse(),
            stops: self.stops_metered(pattern, range, 1.0, meter)?,
            range,
            pixel_step: a.hypot(c) as f32,
        }))
    }

    /// The stops of its colors over `range`, of its parameter from its start,
    /// 0, to its end, 1: from the stop at or before the range's start to the one
    /// at or past its end. Where the color is straight between points of the
    /// function's shape, those points, with one added wherever a channel crosses
    /// 0 or 1, where the colors are clamped; elsewhere, and wherever the color
    /// space or a transfer function bends the colors, the points of its shape
    /// and the color at each multiple of [`grid_step`](Self::grid_step) between
    /// them. Each piece of the shape ends in a stop of its own value there, so a
    /// step of the function is two stops at one place.
    ///
    /// The work and memory are taken of `meter` first, but for the tables its
    /// colors are looked up in, which the caller takes
    /// ([`EncodedShadingPattern::setup_cost`]): the shape, the first time
    /// ([`shape_metered`](Self::shape_metered)), then each value evaluated and
    /// each stop held.
    pub(crate) fn stops_metered(
        &self,
        pattern: &EncodedShadingPattern,
        range: (f32, f32),
        scale: f32,
        meter: &mut dyn Meter,
    ) -> Result<Vec<SvgGradientStop>, Exhausted> {
        let [t0, t1] = self.domain;
        let (lo, hi) = (
            range.0.min(range.1).clamp(0.0, 1.0),
            range.0.max(range.1).clamp(0.0, 1.0),
        );
        let value_cost = pattern.value_cost();
        if t0 == t1 || !(t0.is_finite() && t1.is_finite()) {
            take(meter, value_cost)?;
            let color = self.color(pattern, self.function.eval(&smallvec![t0]));
            return Ok(vec![
                SvgGradientStop { offset: 0.0, color },
                SvgGradientStop { offset: 1.0, color },
            ]);
        }
        // The input of the function at `t`, as the shading evaluates it.
        let x_at = |t: f32| t0 + (t1 - t0) * t;
        let t_of = |x: f32| (x - t0) / (t1 - t0);
        let (x_lo, x_hi) = {
            let (a, b) = (x_at(lo), x_at(hi));
            (a.min(b), a.max(b))
        };
        let shape = self.shape_metered(meter)?;
        let step = self.grid_step(pattern, scale);
        let straight = |values: &[f32]| {
            if pattern.transfer_function.is_some() {
                return None;
            }
            pattern.color_space.straight_rgb(values)
        };
        let lines_straight = straight(&[]).is_some();

        // What each piece the range reaches takes: the points of its lines, and
        // the multiples of the step within it and the range, a value evaluated
        // for each where the colors bend.
        let reached: Vec<&Piece> = shape
            .iter()
            .filter(|piece| {
                let (a, b) = piece.span();
                b >= x_lo && a <= x_hi
            })
            .collect();
        let grid_in = |a: f32, b: f32| -> (i64, i64) {
            let (ta, tb) = (t_of(a.max(x_lo)), t_of(b.min(x_hi)));
            let (ta, tb) = (ta.min(tb), ta.max(tb));
            (
                (f64::from(ta) / f64::from(step)).floor() as i64 - 1,
                (f64::from(tb) / f64::from(step)).ceil() as i64 + 1,
            )
        };
        let mut stops_held = 0_u64;
        let mut evaluated = 0_u64;
        for piece in &reached {
            match piece {
                Piece::Lines(points) => {
                    let (i0, i1) = within(points, x_lo, x_hi);
                    let n = (i1 - i0 + 1) as u64;
                    if lines_straight {
                        // A stop where any of three channels crosses 0 or 1.
                        stops_held = stops_held.saturating_add(n.saturating_mul(7));
                    } else {
                        let (k0, k1) = grid_in(points[i0].0, points[i1].0);
                        let grid = (k1 - k0 + 1).max(0) as u64;
                        stops_held = stops_held.saturating_add(n).saturating_add(grid);
                        evaluated = evaluated.saturating_add(n).saturating_add(grid);
                    }
                }
                Piece::Curve(a, b) => {
                    let (k0, k1) = grid_in(*a, *b);
                    let grid = (k1 - k0 + 1).max(0) as u64 + 2;
                    stops_held = stops_held.saturating_add(grid);
                    evaluated = evaluated.saturating_add(grid);
                }
            }
        }
        take(
            meter,
            value_cost.times(evaluated) + Cost::new(4).times(stops_held),
        )?;
        if !meter.hold(
            usize::try_from(stops_held)
                .unwrap_or(usize::MAX)
                .saturating_mul(size_of::<SvgGradientStop>()),
        ) {
            return Err(Exhausted);
        }

        let mut stops: Vec<SvgGradientStop> =
            Vec::with_capacity(usize::try_from(stops_held).unwrap_or(0));
        let push = |stops: &mut Vec<SvgGradientStop>, t: f32, color: [f32; 4]| {
            stops.push(SvgGradientStop { offset: t, color });
        };
        // In gray and RGB, the colors before they are rounded to 8 bits, so
        // they bend only as the function does.
        let eval_at = |x: f32| {
            let values = self.function.eval(&smallvec![x]);
            match values.as_deref().and_then(straight) {
                Some(rgb) => self.opaque(pattern, rgb),
                None => self.color(pattern, values),
            }
        };
        let bend = if lines_straight {
            STRAIGHT_BEND_LEVELS
        } else {
            BEND_LEVELS
        };
        for piece in reached {
            let mut nodes: Vec<(f32, f32, [f32; 4])> = Vec::new();
            match piece {
                Piece::Lines(points) => {
                    let (i0, i1) = within(points, x_lo, x_hi);
                    let points = &points[i0..=i1];
                    if lines_straight {
                        for (i, (x, values)) in points.iter().enumerate() {
                            let rgb = straight(values).unwrap_or([0.0; 3]);
                            if let Some((x0, v0)) = i.checked_sub(1).map(|i| &points[i])
                                && x > x0
                            {
                                let before = straight(v0).unwrap_or([0.0; 3]);
                                let mut crossings: smallvec::SmallVec<[f32; 6]> =
                                    smallvec::SmallVec::new();
                                for (p, q) in before.iter().zip(&rgb) {
                                    for side in [0.0, 1.0] {
                                        if (p - side) * (q - side) < 0.0 {
                                            crossings.push((side - p) / (q - p));
                                        }
                                    }
                                }
                                crossings.sort_by(f32::total_cmp);
                                for s in crossings {
                                    let at = x0 + (x - x0) * s;
                                    let rgb =
                                        [0, 1, 2].map(|c| before[c] + (rgb[c] - before[c]) * s);
                                    nodes.push((at, t_of(at), self.opaque(pattern, rgb)));
                                }
                            }
                            nodes.push((*x, t_of(*x), self.opaque(pattern, rgb)));
                        }
                    } else {
                        for (x, values) in points {
                            let color = self.color(pattern, Some(values.clone()));
                            nodes.push((*x, t_of(*x), color));
                        }
                        let (a, b) = (points[0].0, points[points.len() - 1].0);
                        self.grid(&mut nodes, grid_in(a, b), step, (a, b), &x_at, &eval_at);
                    }
                }
                Piece::Curve(a, b) => {
                    // Its ends of its own values: from above at its start, as a
                    // stitching evaluates at a bound, and from below at its end.
                    nodes.push((*a, t_of(*a), eval_at(*a)));
                    let end = if b > a { b.next_down() } else { *b };
                    nodes.push((*b, t_of(*b), eval_at(end)));
                    self.grid(&mut nodes, grid_in(*a, *b), step, (*a, *b), &x_at, &eval_at);
                }
            }
            // In the order of the input; the ends stay first and last.
            nodes.sort_by(|p, q| p.0.total_cmp(&q.0));
            if !(lines_straight && matches!(piece, Piece::Lines(_))) {
                let finest = step / f32::from(BEND_SPLITS) * (t1 - t0).abs();
                self.refine(
                    &mut nodes,
                    (finest, bend),
                    &t_of,
                    &eval_at,
                    value_cost,
                    meter,
                )?;
            }
            for (_, t, color) in nodes {
                push(&mut stops, t, color);
            }
        }
        if t1 < t0 {
            stops.reverse();
        }
        // From the stop at or before the range's start to the one at or past its
        // end.
        let start = stops.partition_point(|s| s.offset <= lo).saturating_sub(1);
        let end = stops
            .partition_point(|s| s.offset <= hi)
            .min(stops.len().saturating_sub(1));
        if start > 0 || end + 1 < stops.len() {
            stops = stops[start..=end.max(start)].to_vec();
        }
        Ok(stops)
    }

    /// Splits at its middle each interval between `nodes`, in the order of their
    /// inputs, next to a node off the line through its neighbours by more than
    /// `bend` levels, until none is or each such interval is no wider than
    /// `finest`: where the colors bend within a step of the grid, as a color
    /// space bends straight components or a function steps, the stops follow
    /// them, so straight lines between stops stray by no more than a fourth of
    /// that from them, but for features narrower than `finest`. Each color
    /// evaluated and stop added is taken of `meter` first.
    fn refine(
        &self,
        nodes: &mut Vec<(f32, f32, [f32; 4])>,
        (finest, bend): (f32, f32),
        t_of: &impl Fn(f32) -> f32,
        eval_at: &impl Fn(f32) -> [f32; 4],
        value_cost: Cost,
        meter: &mut dyn Meter,
    ) -> Result<(), Exhausted> {
        loop {
            let n = nodes.len();
            let mut split = vec![false; n.saturating_sub(1)];
            for k in 1..n.saturating_sub(1) {
                let (a, m, b) = (&nodes[k - 1], &nodes[k], &nodes[k + 1]);
                if !(a.0 < m.0 && m.0 < b.0) {
                    continue;
                }
                let s = (m.0 - a.0) / (b.0 - a.0);
                let bends = (0..4).any(|c| {
                    let line = a.2[c] + (b.2[c] - a.2[c]) * s;
                    (m.2[c] - line).abs() * 255.0 > bend
                });
                if bends {
                    split[k - 1] = true;
                    split[k] = true;
                }
            }
            let mut middles = 0_usize;
            for (i, split) in split.iter_mut().enumerate() {
                let (a, b) = (nodes[i].0, nodes[i + 1].0);
                let middle = a + (b - a) / 2.0;
                *split &= b - a > finest && a < middle && middle < b;
                middles += usize::from(*split);
            }
            if middles == 0 {
                return Ok(());
            }
            take(meter, (value_cost + Cost::new(4)).times(middles as u64))?;
            if !meter.hold(middles.saturating_mul(size_of::<SvgGradientStop>())) {
                return Err(Exhausted);
            }
            let mut out = Vec::with_capacity(n + middles);
            for (i, node) in nodes.iter().enumerate() {
                out.push(*node);
                if split.get(i).copied().unwrap_or(false) {
                    let (a, b) = (node.0, nodes[i + 1].0);
                    let x = a + (b - a) / 2.0;
                    out.push((x, t_of(x), eval_at(x)));
                }
            }
            *nodes = out;
        }
    }

    /// Adds to `nodes` the color at each multiple of `step` from `k0` to `k1`
    /// whose input lies strictly within `span`.
    fn grid(
        &self,
        nodes: &mut Vec<(f32, f32, [f32; 4])>,
        (k0, k1): (i64, i64),
        step: f32,
        (a, b): (f32, f32),
        x_at: &impl Fn(f32) -> f32,
        eval_at: &impl Fn(f32) -> [f32; 4],
    ) {
        for k in k0..=k1 {
            let t = (k as f64 * f64::from(step)) as f32;
            let x = x_at(t);
            if x > a && x < b {
                nodes.push((x, t, eval_at(x)));
            }
        }
    }

    /// The function's shape over the shading's domain, found the first time,
    /// its work and memory taken of `meter` first.
    fn shape_metered(&self, meter: &mut dyn Meter) -> Result<&[Piece], Exhausted> {
        if let Some(shape) = self.shape.get() {
            return Ok(shape);
        }
        let (steps, points) = self.function.shape_cost();
        take(meter, steps)?;
        let bytes = usize::try_from(points)
            .unwrap_or(usize::MAX)
            .saturating_mul(size_of::<(f32, Values)>());
        if !meter.hold(bytes) {
            return Err(Exhausted);
        }
        let [t0, t1] = self.domain;
        let shape = self.function.shape(t0.min(t1), t0.max(t1));
        Ok(self.shape.get_or_init(|| shape))
    }

    /// The step of the shading's parameter its colors are tabulated at where
    /// they bend: the largest power of two no larger than the least it changes
    /// over a device pixel anywhere, `1 / scale` units of the space
    /// `base_transform` takes points from, so each pixel's neighbours lie at
    /// least a step away and a
    /// feature of the colors wider than a pixel's step holds a stop. An axial
    /// shading's parameter changes alike everywhere, by the length of the first
    /// row of `base_transform` scaled by a pixel; a radial one's, `t` of the circle
    /// `|p - t p1| = r0 + t (r1 - r0)` through `p`, by `1 / |u·p1 + r1 - r0|`
    /// along `u`, the unit vector from that circle's centre to `p`, so by at
    /// least `1 / (|p1| + |r1 - r0|)` in the shading's space, and by at least the
    /// smaller singular value of that scaled transform times that over a pixel; the
    /// line touching circles are painted by changes by at least half of it.
    /// Within 2^-24 and a half: `t` is no finer in `f32`.
    pub(crate) fn grid_step(&self, pattern: &EncodedShadingPattern, scale: f32) -> f32 {
        let [a, b, c, d, _, _] =
            (pattern.base_transform * Affine::scale(1.0 / f64::from(scale))).as_coeffs();
        let least = match &self.params {
            RadialAxialParams::Axial => a.hypot(c),
            RadialAxialParams::Radial { p1, r, .. } => {
                let det = (a * d - b * c).abs();
                let squares = a * a + b * b + c * c + d * d;
                let most = ((squares + (squares * squares - 4.0 * det * det).max(0.0).sqrt())
                    / 2.0)
                    .sqrt();
                (det / most) / (2.0 * p1.to_vec2().hypot() + (r.y - r.x).abs())
            }
        };
        const FINEST: f32 = 1.0 / (1 << 24) as f32;
        if least.partial_cmp(&f64::from(FINEST)) != Some(std::cmp::Ordering::Greater) {
            return FINEST;
        }
        if least >= 0.5 {
            return 0.5;
        }
        // The power of two at most `least`.
        let mut step = 0.5_f64;
        while step > least {
            step /= 2.0;
        }
        step as f32
    }

    /// The color of the function's `values`, with the pattern's opacity and
    /// transfer function; none where it has no value.
    fn color(&self, pattern: &EncodedShadingPattern, values: Option<Values>) -> [f32; 4] {
        let Some(out) = values else {
            return [0.0, 0.0, 0.0, 0.0];
        };
        let mut components = pattern.color_space.to_rgba(&out, 1.0).components();
        components[3] *= pattern.opacity;

        if let Some(tf) = &pattern.transfer_function {
            return tf.apply(&AlphaColor::new(components)).components();
        }

        components
    }

    /// A straight RGB color clamped as the color space clamps it, with the
    /// pattern's opacity.
    fn opaque(&self, pattern: &EncodedShadingPattern, rgb: [f32; 3]) -> [f32; 4] {
        let clamp = |v: f32| if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
        [clamp(rgb[0]), clamp(rgb[1]), clamp(rgb[2]), pattern.opacity]
    }
}

/// The first and last of `points` that a line through them over `lo` to `hi`
/// needs: the last at or before `lo` and the first at or past `hi`.
fn within(points: &[(f32, Values)], lo: f32, hi: f32) -> (usize, usize) {
    let last = points.len() - 1;
    let i0 = points
        .partition_point(|p| p.0 <= lo)
        .saturating_sub(1)
        .min(last);
    let i1 = points.partition_point(|p| p.0 < hi).min(last);
    (i0, i1.max(i0))
}

/// The color `stops` give at `t`, from the start, 0, to the end, 1: between the
/// last stop at or before it and the next, as a gradient's stops are drawn; at a
/// bound where the function steps, two stops share it and the later one's color
/// starts there, as the function's does.
pub(crate) fn color_at(stops: &[SvgGradientStop], t: f32) -> [f32; 4] {
    lerp_at(stops, t, false)
}

/// [`color_at`], or the color from below `t` where two stops share it.
fn lerp_at(stops: &[SvgGradientStop], t: f32, from_below: bool) -> [f32; 4] {
    let next = if from_below {
        stops.partition_point(|stop| stop.offset < t)
    } else {
        stops.partition_point(|stop| stop.offset <= t)
    };
    match (
        next.checked_sub(1).map(|i| stops[i]),
        stops.get(next).copied(),
    ) {
        (Some(before), Some(after)) if after.offset > before.offset => lerp_color(
            before.color,
            after.color,
            (t - before.offset) / (after.offset - before.offset),
        ),
        (Some(stop), _) | (None, Some(stop)) => stop.color,
        (None, None) => [0.0; 4],
    }
}

fn lerp_color(c0: [f32; 4], c1: [f32; 4], t: f32) -> [f32; 4] {
    [
        c0[0] + (c1[0] - c0[0]) * t,
        c0[1] + (c1[1] - c0[1]) * t,
        c0[2] + (c1[2] - c0[2]) * t,
        c0[3] + (c1[3] - c0[3]) * t,
    ]
}
