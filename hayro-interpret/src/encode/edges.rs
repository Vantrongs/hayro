use super::{EncodedShadingPattern, EncodedShadingType, Exhausted, Meter, RadialAxialParams, take};
use crate::flatten::ControlHull;
use kurbo::{Affine, BezPath, PathEl, Point, Rect};

// Leave the rest of the tolerance for the shape itself.
const CHORD_SHARE: f64 = 1.0 / 8.0;

impl EncodedShadingPattern {
    /// Extend edges coincident with the clipped shape within the device-space tolerance.
    pub fn extend_within_metered(
        &mut self,
        shape: &BezPath,
        hull: Option<&ControlHull>,
        to_device: Affine,
        window: Rect,
        tolerance: f64,
        meter: &mut dyn Meter,
    ) -> Result<(), Exhausted> {
        let Some(placed) = Placement::new(self.base_transform, to_device, window) else {
            return Ok(());
        };
        let allowed = placed.span(tolerance);
        let edges = match &self.shading_type {
            EncodedShadingType::FunctionBased(shading) => !shading.extended.iter().all(|&e| e),
            EncodedShadingType::RadialAxial(shading) => shading.extend != [true, true],
            EncodedShadingType::Mesh(_) | EncodedShadingType::Dummy(_) => false,
        };
        if !edges {
            return Ok(());
        }
        // The box of the shape's control points in the shading's space, which
        // holds the shape: a side of the box within a half-plane holds it there.
        let mut bounds = || {
            let built;
            let hull = match hull {
                Some(hull) => hull,
                None => {
                    built = ControlHull::new_metered(shape, meter)?;
                    &built
                }
            };
            take(meter, hull.bounds_cost())?;
            Ok(hull
                .bounds(placed.to_shading)
                .map(|bounds| (bounds, placed.rounding(reach(bounds)))))
        };
        match &mut self.shading_type {
            EncodedShadingType::FunctionBased(shading) => {
                let Some((bounds, error)) = bounds()? else {
                    return Ok(());
                };
                let domain = shading.domain;
                let (x, y) = (allowed.x - error, allowed.y - error);
                shading.extended[0] |= bounds.x0 >= domain.x0 - x;
                shading.extended[1] |= bounds.x1 <= domain.x1 + x;
                shading.extended[2] |= bounds.y0 >= domain.y0 - y;
                shading.extended[3] |= bounds.y1 <= domain.y1 + y;
            }
            EncodedShadingType::RadialAxial(shading) => match shading.params {
                RadialAxialParams::Axial { .. } => {
                    let Some((bounds, error)) = bounds()? else {
                        return Ok(());
                    };
                    let t = allowed.x - error;
                    shading.extend[0] |= bounds.x0 >= -t;
                    shading.extend[1] |= bounds.x1 <= 1.0 + t;
                }
                RadialAxialParams::Radial(crate::radial::Circles { p1, r, a, .. }) => {
                    let (r0, r1) = (r.x, r.y);
                    // Where one circle holds the other, each holds the ones before
                    // it or after: one end is the larger circle's inside, the other
                    // the smaller one's outside.
                    let grows = r1 >= r0;
                    if a >= 0.0 {
                        return Ok(());
                    }
                    let (large, small) = if grows {
                        ((p1, r1), (Point::ZERO, r0))
                    } else {
                        ((Point::ZERO, r0), (p1, r1))
                    };
                    // The rounding of the shape's lines grows with its control
                    // points, which a curve's lines are evaluated from.
                    let Some((bounds, _)) = bounds()? else {
                        return Ok(());
                    };
                    let error = placed.rounding(reach(bounds).max(reach(placed.window)));
                    // The shape lies within `chord` of its lines: those within
                    // `near - chord` past the larger circle and short of the
                    // smaller one bound it within `near`.
                    let near = allowed.any;
                    let chord = near * CHORD_SHARE;
                    let mut test = Nesting::new(
                        &placed,
                        (large.0, large.1 + (near - chord)),
                        (small.0, small.1 - (near - chord)),
                        error,
                    );
                    // Lines past the window only wind around points in it; those
                    // that may reach it as painted are tested.
                    let past = chord + 3.0 * error;
                    let cull = test.window.inflate(past, past);
                    crate::flatten::flatten_metered(
                        shape.iter(),
                        placed.to_shading,
                        chord,
                        Some(cull),
                        meter,
                        |el, _| {
                            test.add(el);
                            Ok(())
                        },
                    )?;
                    let (inside, outside) = test.finish();
                    let (outer_end, inner_end) = if grows { (1, 0) } else { (0, 1) };
                    shading.extend[outer_end] |= inside;
                    shading.extend[inner_end] |= outside;
                }
            },
            EncodedShadingType::Mesh(_) | EncodedShadingType::Dummy(_) => {}
        }
        Ok(())
    }

    /// Which edges of the part the shading paints itself in it extends past, a
    /// bit each: encodings of one shading under one transform paint alike where
    /// these agree ([`extend_within_metered`](Self::extend_within_metered)).
    pub fn extended_edges(&self) -> u8 {
        let bits = |flags: &[bool]| {
            flags
                .iter()
                .enumerate()
                .fold(0, |bits, (i, &on)| bits | (u8::from(on) << i))
        };
        match &self.shading_type {
            EncodedShadingType::FunctionBased(shading) => bits(&shading.extended),
            EncodedShadingType::RadialAxial(shading) => bits(&shading.extend),
            EncodedShadingType::Mesh(_) | EncodedShadingType::Dummy(_) => 0,
        }
    }
}

struct Nesting {
    window: Rect,
    large: (Point, f64),
    hole: (Point, f64),
    error: f64,
    /// Whether a line's part within the window may reach past `large`.
    leaves: bool,
    /// Whether a line's part within the window may reach into `hole`.
    enters: bool,
    /// The window's corners, and its point nearest the hole's centre, the lines'
    /// winding around each, and whether a line passes too near it for that
    /// winding to be known: a corner a line passes through, as one of a shape
    /// ending at the window's side, is held to `large` as one the shape holds.
    probes: [Point; 5],
    winding: [i32; 5],
    unsure: [bool; 5],
    start: Point,
    last: Point,
}

impl Nesting {
    fn new(placed: &Placement, large: (Point, f64), hole: (Point, f64), error: f64) -> Self {
        let window = placed.window;
        let nearest = Point::new(
            hole.0.x.clamp(window.x0, window.x1),
            hole.0.y.clamp(window.y0, window.y1),
        );
        Self {
            window,
            large,
            hole,
            error,
            leaves: false,
            enters: false,
            probes: [
                Point::new(window.x0, window.y0),
                Point::new(window.x1, window.y0),
                Point::new(window.x0, window.y1),
                Point::new(window.x1, window.y1),
                nearest,
            ],
            winding: [0; 5],
            unsure: [false; 5],
            start: Point::ZERO,
            last: Point::ZERO,
        }
    }

    fn add(&mut self, el: PathEl) {
        match el {
            PathEl::MoveTo(p) => {
                self.line(self.last, self.start);
                (self.start, self.last) = (p, p);
            }
            PathEl::LineTo(p) => {
                self.line(self.last, p);
                self.last = p;
            }
            PathEl::ClosePath => {
                self.line(self.last, self.start);
                self.last = self.start;
            }
            PathEl::QuadTo(..) | PathEl::CurveTo(..) => {}
        }
    }

    fn line(&mut self, a: Point, b: Point) {
        if a == b {
            return;
        }
        // Each test holds only where it is proven: not where a distance is not a
        // number.
        let (margin, near) = (2.0 * self.error, 3.0 * self.error);
        for ((probe, winding), unsure) in self
            .probes
            .iter()
            .zip(&mut self.winding)
            .zip(&mut self.unsure)
        {
            let clear = nearest_on(a, b, *probe).distance(*probe) > near;
            if !clear {
                *unsure = true;
            } else if (a.y <= probe.y) != (b.y <= probe.y)
                && a.x + (probe.y - a.y) * (b.x - a.x) / (b.y - a.y) > probe.x
            {
                *winding += if b.y > a.y { 1 } else { -1 };
            }
        }
        let Some((p, q)) = clip_to(a, b, self.window.inflate(near, near)) else {
            return;
        };
        let (center, radius) = self.large;
        let within = radius - margin;
        let held = p.distance(center) <= within && q.distance(center) <= within;
        self.leaves |= !held;
        let (center, radius) = self.hole;
        let misses = nearest_on(p, q, center).distance(center) >= radius + margin;
        self.enters |= !misses;
    }

    /// Whether `large` holds the shape's part within the window, and whether that
    /// part misses `hole`.
    fn finish(mut self) -> (bool, bool) {
        self.line(self.last, self.start);
        let window = self.window;
        let corners = [
            Point::new(window.x0, window.y0),
            Point::new(window.x1, window.y0),
            Point::new(window.x0, window.y1),
            Point::new(window.x1, window.y1),
        ];
        let margin = 2.0 * self.error;
        let known_zero = |i: usize| self.winding[i] == 0 && !self.unsure[i];
        let (center, radius) = self.large;
        let inside = !self.leaves
            && corners
                .iter()
                .enumerate()
                .all(|(i, corner)| known_zero(i) || corner.distance(center) <= radius - margin);
        let (center, radius) = self.hole;
        let nearest = self.probes[4];
        let outside = radius <= 0.0
            || !self.enters && (nearest.distance(center) >= radius + margin || known_zero(4));
        (inside, outside)
    }
}

/// The point of the line from `a` to `b` nearest `p`.
fn nearest_on(a: Point, b: Point, p: Point) -> Point {
    let side = b - a;
    let along = if side.hypot2() > 0.0 {
        ((p - a).dot(side) / side.hypot2()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    a + side * along
}

/// The largest magnitude of a coordinate of `rect`.
fn reach(rect: Rect) -> f64 {
    rect.x0
        .abs()
        .max(rect.x1.abs())
        .max(rect.y0.abs())
        .max(rect.y1.abs())
}

/// Where a shape is tested for the edges a shading extends past
/// ([`EncodedShadingPattern::extend_within_metered`]): the shading's transform
/// from device space, the shape's transform to device space, the two together,
/// and the window the shape is seen in, in the shading's space. Each is finite
/// and each transform inverts, so every bound a test derives from them is a
/// number, if an infinite one, and the window's sides are ordered.
struct Placement {
    base: Affine,
    to_device: Affine,
    to_shading: Affine,
    window: Rect,
}

impl Placement {
    /// `None` where a transform or the window, in device space or the shading's,
    /// is not finite, or a transform does not invert: a shape squeezed onto a
    /// line covers no pixel, and from a place that is not a number no test
    /// proves anything, so nothing is extended.
    fn new(base: Affine, to_device: Affine, window: Rect) -> Option<Self> {
        let inverts = |t: Affine| {
            let det = t.determinant();
            t.is_finite() && det != 0.0 && det.is_finite() && t.inverse().is_finite()
        };
        let finite = |r: Rect| {
            [r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite()) && r.x0 <= r.x1 && r.y0 <= r.y1
        };
        let to_shading = base * to_device;
        if !(inverts(base) && inverts(to_device) && inverts(to_shading) && finite(window)) {
            return None;
        }
        let window = base.transform_rect_bbox(window);
        finite(window).then_some(Self {
            base,
            to_device,
            to_shading,
            window,
        })
    }

    /// How far in the shading's space a point may lie past an edge of it and lie
    /// at most `device` units past it in device space: past a side along which
    /// its x is constant, one along which its y is, and past any edge, which a
    /// point within `any` of it lies within `device` units of whichever way the
    /// shading is turned, sheared or stretched. Each allowance a test gives a
    /// shape past a shading's edge is one of these, so it holds in device units
    /// at any of the shading's sizes.
    fn span(&self, device: f64) -> Span {
        let [a, b, c, d, _, _] = self.base.as_coeffs();
        // The shading's x and y change by `(a, c)` and `(b, d)` along a device
        // unit; along any direction by at least the smaller singular value.
        let det = (a * d - b * c).abs();
        let squares = a * a + b * b + c * c + d * d;
        let most = ((squares + (squares * squares - 4.0 * det * det).max(0.0).sqrt()) / 2.0).sqrt();
        Span {
            x: device * a.hypot(c),
            y: device * b.hypot(d),
            any: device * (det / most),
        }
    }

    /// How far, along either axis in a shading's space, a point of a shape placed
    /// there through `to_device` and then `base`, as a test computes it, may lie from
    /// the same point as painted, where vello places it in device space through
    /// `to_device` alone, for a shape and window within `reach` of the origin there;
    /// with the error of a test's own arithmetic on lines so placed besides. A margin
    /// smaller than this proves nothing: infinite where it is not known.
    ///
    /// Each step, a product or sum of terms reaching `m`, rounds by at most
    /// `f64::EPSILON * m`. A point `s = B T p` lies within `‖B‖‖T‖‖p‖` of the
    /// origin's image, and `‖p‖ ≤ ‖(B T)⁻¹‖ (‖s‖ + ‖t‖)` for `t` the translation of
    /// `B T`: placing it in each way, making `B T`, flattening a curve and clipping a
    /// line to the window each takes a few steps over terms within
    /// `cond(B) cond(T) (reach + ‖t‖)` of the origin, besides the translations.
    fn rounding(&self, reach: f64) -> f64 {
        /// More than the steps of rounding all of these take together.
        const STEPS: f64 = 64.0;
        // The largest sum of a row's magnitudes, of the matrix and of its inverse.
        let norm = |[a, b, c, d, _, _]: [f64; 6]| (a.abs() + c.abs()).max(b.abs() + d.abs());
        let inverse_norm = |[a, b, c, d, _, _]: [f64; 6]| {
            (d.abs() + c.abs()).max(b.abs() + a.abs()) / (a * d - b * c).abs()
        };
        let shift = |[_, _, _, _, e, f]: [f64; 6]| e.abs().max(f.abs());
        let (base, to_device) = (self.base.as_coeffs(), self.to_device.as_coeffs());
        let spread = norm(base) * inverse_norm(base) * norm(to_device) * inverse_norm(to_device);
        let offset = norm(base) * shift(to_device) + shift(base);
        let error = STEPS * f64::EPSILON * (spread * (reach + offset) + offset);
        if error.is_nan() { f64::INFINITY } else { error }
    }
}

/// Distances in a shading's space that span a distance in device space
/// ([`Placement::span`]).
struct Span {
    x: f64,
    y: f64,
    any: f64,
}

/// The part of the line from `a` to `b` within `rect`, `None` where it misses it.
fn clip_to(a: Point, b: Point, rect: Rect) -> Option<(Point, Point)> {
    let d = b - a;
    let (mut t0, mut t1) = (0.0_f64, 1.0_f64);
    for (q, r) in [
        (-d.x, a.x - rect.x0),
        (d.x, rect.x1 - a.x),
        (-d.y, a.y - rect.y0),
        (d.y, rect.y1 - a.y),
    ] {
        if q == 0.0 {
            if r < 0.0 {
                return None;
            }
        } else {
            let t = r / q;
            if q < 0.0 {
                t0 = t0.max(t);
            } else {
                t1 = t1.min(t);
            }
        }
    }
    (t0 <= t1).then(|| (a + d * t0, a + d * t1))
}
