//! SVG-like gradient conversion for encoded shadings.
//!
//! PDF shadings are very expensive to evaluate. Because of this, it's useful to
//! approximate axial shadings by SVG-like sRGB linear gradients. Radial ones are
//! painted by the shading itself (`as_svg_gradient_metered`), their colors
//! looked up in the same stops.

use crate::color::AlphaColor;
use crate::encode::{
    Cost, EncodedRadialAxialShading, EncodedShadingPattern, Exhausted, Meter, RadialAxialParams,
    Unmetered, take,
};
use crate::function::StitchingBounds;
use kurbo::{Affine, Point, Rect};
use smallvec::smallvec;

const MAX_GRADIENT_SUBDIVISION_DEPTH: u8 = 10;

/// The most, in any component, the colors between two stops may stray from the
/// shading's own where the stops are found ([`approximate_gradient_stops`]): a
/// native linear gradient's stops and the colors a radial shading is sampled at
/// alike.
pub const GRADIENT_TOLERANCE: f32 = 0.01;

/// An SVG-like gradient.
pub struct SvgGradient {
    /// The kind of the gradient.
    pub kind: SvgGradientKind,
    /// The transform from encoded shading coordinates into the renderer's coordinate space.
    pub transform: Affine,
    /// The stops of the gradient in the sRGB color space.
    pub stops: Vec<SvgGradientStop>,
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

impl EncodedRadialAxialShading {
    /// Convert an axial/radial PDF shading into an SVG-like gradient approximation.
    pub fn as_svg_gradient(
        &self,
        pattern: &EncodedShadingPattern,
        path_bbox: Rect,
    ) -> Option<SvgGradient> {
        self.as_svg_gradient_metered(pattern, path_bbox, &mut Unmetered)
            .expect("an unmetered conversion is never refused")
    }

    /// [`as_svg_gradient`](Self::as_svg_gradient), taking its work of `meter` before
    /// it does it: the tables colors are looked up in, the function's stitching
    /// bounds gathered, and each value sampled
    /// ([`point_cost`](EncodedShadingPattern::point_cost)), up to
    /// `3 * 2^MAX_GRADIENT_SUBDIVISION_DEPTH` between two bounds. `Exhausted` when
    /// `meter` refuses; `Ok(None)` for a shading no such gradient draws.
    pub fn as_svg_gradient_metered(
        &self,
        pattern: &EncodedShadingPattern,
        path_bbox: Rect,
        meter: &mut dyn Meter,
    ) -> Result<Option<SvgGradient>, Exhausted> {
        // A couple of cases cannot be losslessly represented by an SVG gradient.
        let kind = match self.params {
            RadialAxialParams::Axial => {
                if !self.axial_extend_covers_bbox(pattern, path_bbox) {
                    return Ok(None);
                }
                SvgGradientKind::Linear {
                    start: Point::ZERO,
                    end: Point::new(1.0, 0.0),
                }
            }
            // A two-point radial gradient of an SVG renderer tells a cone from
            // circles that touch within a tolerance and paints in `f32`, as
            // vello's does: it paints near-touching circles, and circles of a
            // thousandth of a unit, as others, and points 10^9 units out as on
            // their neighbours' circles. The shading paints its points itself.
            RadialAxialParams::Radial { .. } => return Ok(None),
        };

        Ok(Some(SvgGradient {
            kind,
            transform: pattern.base_transform.inverse(),
            stops: self.stops_metered(pattern, meter)?,
        }))
    }

    /// The stops of its colors from its start to its end, within
    /// [`GRADIENT_TOLERANCE`], taking first of `meter` the tables its colors are
    /// looked up in and its function's stitching bounds, then each sample and the
    /// stops held.
    pub(crate) fn stops_metered(
        &self,
        pattern: &EncodedShadingPattern,
        meter: &mut dyn Meter,
    ) -> Result<Vec<SvgGradientStop>, Exhausted> {
        take(meter, pattern.setup_cost() + self.function.bounds_cost())?;
        approximate_gradient_stops(
            &mut Stops {
                meter,
                sample_cost: pattern.value_cost(),
                stops: Vec::new(),
            },
            |t| self.sample_t(pattern, t),
            &self.normalized_stitching_bounds(),
            GRADIENT_TOLERANCE,
        )
    }

    fn sample_t(&self, pattern: &EncodedShadingPattern, t: f32) -> [f32; 4] {
        let (t0, t1) = (self.domain[0], self.domain[1]);
        let t = t0 + (t1 - t0) * t;
        let Some(out) = self.function.eval(&smallvec![t]) else {
            return [0.0, 0.0, 0.0, 0.0];
        };
        let mut components = pattern.color_space.to_rgba(&out, 1.0).components();
        components[3] *= pattern.opacity;

        if let Some(tf) = &pattern.transfer_function {
            return tf.apply(&AlphaColor::new(components)).components();
        }

        components
    }

    fn axial_extend_covers_bbox(&self, pattern: &EncodedShadingPattern, path_bbox: Rect) -> bool {
        let path_bbox = pattern.base_transform.transform_rect_bbox(path_bbox);

        let min_x = path_bbox.min_x();
        let max_x = path_bbox.max_x();

        (self.extend[0] || min_x >= 0.0) && (self.extend[1] || max_x <= 1.0)
    }

    fn normalized_stitching_bounds(&self) -> StitchingBounds {
        let [t0, t1] = self.domain;
        let dt = t1 - t0;
        if dt.abs() <= f32::EPSILON {
            return StitchingBounds::new();
        }

        let mut bounds = self
            .function
            .stitching_bounds()
            .into_iter()
            .map(|bound| (bound - t0) / dt)
            .filter(|bound| *bound > 0.0 && *bound < 1.0)
            .collect::<StitchingBounds>();

        if dt < 0.0 {
            bounds.reverse();
        }

        bounds
    }
}

/// Stops as they are found, each sample taken for them and each growth of the
/// list paid for with `meter` first.
struct Stops<'m> {
    meter: &'m mut dyn Meter,
    /// The steps a sample takes.
    sample_cost: Cost,
    stops: Vec<SvgGradientStop>,
}

impl Stops<'_> {
    /// Pays for `samples` samples.
    fn pay(&mut self, samples: u64) -> Result<(), Exhausted> {
        take(self.meter, self.sample_cost.times(samples))
    }

    fn push(&mut self, offset: f32, color: [f32; 4]) -> Result<(), Exhausted> {
        if self.stops.len() == self.stops.capacity() {
            // Twice as many, held while the list grows next to the one before.
            let more = self.stops.len().max(4);
            let bytes = (2 * self.stops.len() + more) * size_of::<SvgGradientStop>();
            if !self.meter.hold(bytes) {
                return Err(Exhausted);
            }
            self.stops.reserve_exact(more);
        }
        self.stops.push(SvgGradientStop { offset, color });
        Ok(())
    }
}

/// The stops approximating, within `tolerance`, the colors `sample` finds between
/// each two `breakpoints`, each sample and the stops held paid for first.
fn approximate_gradient_stops(
    stops: &mut Stops<'_>,
    mut sample: impl FnMut(f32) -> [f32; 4],
    breakpoints: &[f32],
    tolerance: f32,
) -> Result<Vec<SvgGradientStop>, Exhausted> {
    let mut interval_bounds = Vec::with_capacity(breakpoints.len() + 2);
    interval_bounds.push(0.0);
    interval_bounds.extend(breakpoints.iter().copied());
    interval_bounds.push(1.0);

    for interval in interval_bounds.windows(2) {
        let start_offset = interval[0];
        let end_offset = interval[1];

        stops.pay(2)?;
        let start_color = sample(start_offset);
        stops.push(start_offset, start_color)?;

        let end_color = sample(end_offset);

        approximate_gradient_interval(
            stops,
            &mut sample,
            GradientSample {
                offset: start_offset,
                color: start_color,
            },
            GradientSample {
                offset: end_offset,
                color: end_color,
            },
            tolerance.max(0.0),
            0,
        )?;
    }

    Ok(std::mem::take(&mut stops.stops))
}

#[derive(Clone, Copy)]
struct GradientSample {
    offset: f32,
    color: [f32; 4],
}

fn approximate_gradient_interval(
    stops: &mut Stops<'_>,
    sample: &mut impl FnMut(f32) -> [f32; 4],
    start: GradientSample,
    end: GradientSample,
    tolerance: f32,
    depth: u8,
) -> Result<(), Exhausted> {
    let offset_delta = end.offset - start.offset;
    let quarter_offset = start.offset + offset_delta * 0.25;
    let mid_offset = start.offset + offset_delta * 0.5;
    let three_quarter_offset = start.offset + offset_delta * 0.75;

    stops.pay(3)?;
    let quarter_color = sample(quarter_offset);
    let mid_color = sample(mid_offset);
    let three_quarter_color = sample(three_quarter_offset);

    if color_error(quarter_color, lerp_color(start.color, end.color, 0.25)) <= tolerance
        && color_error(mid_color, lerp_color(start.color, end.color, 0.5)) <= tolerance
        && color_error(
            three_quarter_color,
            lerp_color(start.color, end.color, 0.75),
        ) <= tolerance
        || depth == MAX_GRADIENT_SUBDIVISION_DEPTH
    {
        return stops.push(end.offset, end.color);
    }

    let mid = GradientSample {
        offset: mid_offset,
        color: mid_color,
    };

    approximate_gradient_interval(stops, sample, start, mid, tolerance, depth + 1)?;
    approximate_gradient_interval(stops, sample, mid, end, tolerance, depth + 1)
}

/// The color `stops` give at `t`, from the start, 0, to the end, 1: between the
/// last stop at or before it and the next, as a gradient's stops are drawn; at a
/// bound where the function steps, two stops share it and the later one's color
/// starts there, as the function's does.
pub(crate) fn color_at(stops: &[SvgGradientStop], t: f32) -> [f32; 4] {
    let next = stops.partition_point(|stop| stop.offset <= t);
    match (
        next.checked_sub(1).map(|i| stops[i]),
        stops.get(next).copied(),
    ) {
        (Some(before), Some(after)) => lerp_color(
            before.color,
            after.color,
            (t - before.offset) / (after.offset - before.offset),
        ),
        (Some(stop), None) | (None, Some(stop)) => stop.color,
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

fn color_error(c0: [f32; 4], c1: [f32; 4]) -> f32 {
    c0.iter()
        .zip(c1)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f32::max)
}

#[cfg(test)]
mod tests {
    use super::{Cost, Exhausted, Meter, Stops, SvgGradientStop, approximate_gradient_stops};

    /// Gives `left` steps, and records the most bytes asked to be held.
    struct Giving {
        left: u64,
        held: usize,
    }

    impl Meter for Giving {
        fn work(&mut self, steps: Cost) -> bool {
            let Some(left) = self.left.checked_sub(steps.steps()) else {
                return false;
            };
            self.left = left;
            true
        }

        fn hold(&mut self, bytes: usize) -> bool {
            self.held += bytes;
            true
        }
    }

    /// Stops are found by sampling only what was paid for first, and held only
    /// as asked: the samples a gradient of twenty turns takes at the deepest
    /// subdivision, each paid for before it is taken, none past what the meter
    /// gives, and the list's bytes asked before it grows.
    #[test]
    fn stops_are_sampled_only_as_paid() {
        let wave = |t: f32| [(t * 125.0).sin() * 0.5 + 0.5, 0.0, 0.0, 1.0];
        let find = |left: u64| {
            let mut meter = Giving { left, held: 0 };
            let mut sampled = 0;
            let found = approximate_gradient_stops(
                &mut Stops {
                    meter: &mut meter,
                    sample_cost: Cost::new(1),
                    stops: Vec::new(),
                },
                |t| {
                    sampled += 1;
                    wave(t)
                },
                &[0.5],
                0.01,
            );
            (found, sampled, meter)
        };
        let (found, sampled, meter) = find(u64::MAX);
        let stops = found.unwrap();
        assert_eq!(u64::MAX - meter.left, sampled);
        assert!(stops.len() > 100, "{}", stops.len());
        assert!(meter.held >= stops.capacity() * size_of::<SvgGradientStop>());
        for give in [0, 1, sampled / 2, sampled - 1] {
            let (found, taken, _) = find(give);
            assert_eq!(found.err(), Some(Exhausted), "{give}");
            assert!(taken <= give, "{give}: sampled {taken}");
        }
    }
}
