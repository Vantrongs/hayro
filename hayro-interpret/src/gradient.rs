//! SVG-like gradient approximations of encoded shadings.

use crate::color::{color_error, lerp_color};
use crate::encode::{Cost, Exhausted, Meter, Unmetered, push_metered, scratch, take};
use crate::encode::{EncodedRadialAxialShading, EncodedShadingPattern, RadialAxialParams};
use crate::shading::ShadingFunction;
use kurbo::{Affine, Point};

const PADDING: f64 = 0.01;
const LUT_TOLERANCE: f32 = 0.01;
const MIN_LUT_SIZE: usize = 256;
const MAX_LUT_SIZE: usize = 4096;

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
    /// Radial gradient.
    Radial {
        /// The center point of the start circle.
        start_center: Point,
        /// The radius of the start circle.
        start_radius: f32,
        /// The center point of the end circle.
        end_center: Point,
        /// The radius of the end circle.
        end_radius: f32,
    },
}

/// A color stop for an SVG-like gradient.
#[derive(Clone, Debug)]
pub struct SvgGradientStop {
    /// Normalized offset.
    pub offset: f32,
    /// RGBA color components in sRGB.
    pub color: [f32; 4],
}

/// The color stops of a one-dimensional sampled shading in parameter order.
#[derive(Debug)]
pub struct SampleRamp {
    stops: Vec<SvgGradientStop>,
    native_span: f64,
}

struct Sample {
    t: f32,
    codes: smallvec::SmallVec<[u8; 4]>,
    color: [f32; 4],
}

struct RampBuilder<'a> {
    f: &'a ShadingFunction,
    linear_rgb: bool,
    pattern: &'a EncodedShadingPattern,
    space: &'a crate::color::ColorSpace,
    domain: [f32; 2],
    meter: &'a mut dyn Meter,
    stops: Vec<SvgGradientStop>,
    jumps: Vec<f32>,
    start: usize,
    lo: [f32; 4],
    hi: [f32; 4],
}

impl RampBuilder<'_> {
    fn sample(&mut self, t: f32, last: Option<&Sample>) -> Result<Sample, Exhausted> {
        take(self.meter, self.f.cost() + Cost::new(20))?;
        let mut out = [0.; crate::function::MAX_COMPONENTS];
        let out = &mut out[..self.f.num_outputs()];
        self.f
            .eval(&[parameter_at(self.domain, t)], out)
            .expect("one input with the function's output length");
        let codes = self.space.encode_values(out);
        let color = if let Some(s) = last.filter(|s| s.codes == codes) {
            s.color
        } else {
            take(self.meter, self.space.cost() + Cost::new(4))?;
            self.pattern.convert(out)
        };
        Ok(Sample { t, codes, color })
    }

    fn interval(&mut self, a: &Sample, b: &Sample) -> Result<(), Exhausted> {
        let mid = a.t + (b.t - a.t) * 0.5;
        // Within a source interval each component is monotone. A single code change
        // can produce only the two endpoint colors, even with a nonlinear tint transform.
        let simple = self.linear_rgb
            || a.codes == b.codes
            || (color_error(a.color, b.color) <= LUT_TOLERANCE
                && a.codes
                    .iter()
                    .zip(&b.codes)
                    .map(|(&a, &b)| u32::from(a.abs_diff(b)))
                    .sum::<u32>()
                    <= 1);
        if simple || mid <= a.t || mid >= b.t {
            let jump = color_error(a.color, b.color);
            if !self.linear_rgb && jump > 0.0 && a.codes != b.codes {
                push_metered(&mut self.jumps, b.t, self.meter)?;
            }
            return self.push(b);
        }
        let m = self.sample(mid, Some(a))?;
        self.interval(a, &m)?;
        self.interval(&m, b)
    }

    fn push(&mut self, s: &Sample) -> Result<(), Exhausted> {
        if self.stops.len() >= 2 {
            let a = &self.stops[self.start];
            let dt = s.t - a.offset;
            let fits = (0..4).all(|k| {
                let slope = (s.color[k] - a.color[k]) / dt;
                self.lo[k] <= slope && slope <= self.hi[k]
            });
            if fits {
                self.stops.pop();
            } else {
                self.start = self.stops.len() - 1;
                self.lo.fill(f32::NEG_INFINITY);
                self.hi.fill(f32::INFINITY);
            }
        }
        if let Some(a) = self.stops.get(self.start) {
            let dt = s.t - a.offset;
            for k in 0..4 {
                let delta = s.color[k] - a.color[k];
                self.lo[k] = self.lo[k].max((delta - 0.5 / 255.) / dt);
                self.hi[k] = self.hi[k].min((delta + 0.5 / 255.) / dt);
            }
        }
        push_metered(
            &mut self.stops,
            SvgGradientStop {
                offset: s.t,
                color: s.color,
            },
            self.meter,
        )
    }
}

impl SampleRamp {
    pub(crate) fn new(
        function: &ShadingFunction,
        domain: [f32; 2],
        pattern: &EncodedShadingPattern,
        meter: &mut dyn Meter,
    ) -> Result<Option<Self>, Exhausted> {
        scratch(
            meter,
            |meter| Self::build(function, domain, pattern, meter),
            Option::is_some,
        )
    }

    fn build(
        function: &ShadingFunction,
        domain: [f32; 2],
        pattern: &EncodedShadingPattern,
        meter: &mut dyn Meter,
    ) -> Result<Option<Self>, Exhausted> {
        let space = &pattern.color_space;
        if function.requires_direct_sampling()
            || function.functions().iter().any(|f| f.num_inputs() != 1)
            || !space.ramp_compatible()
            || domain[0] == domain[1]
        {
            return Ok(None);
        }
        take(meter, pattern.setup_cost())?;
        let grid = match function {
            ShadingFunction::Single(f) => f.unit_grid(domain),
            _ => None,
        };
        let linear_rgb = (grid.is_some() || function.unit_linear(domain))
            && space.is_device_gray_or_rgb()
            && pattern.transfer_function.is_none();
        let mut builder = RampBuilder {
            pattern,
            f: function,
            linear_rgb,
            space,
            domain,
            meter,
            stops: Vec::new(),
            jumps: Vec::new(),
            start: 0,
            lo: [f32::NEG_INFINITY; 4],
            hi: [f32::INFINITY; 4],
        };
        let mut previous = builder.sample(0., None)?;
        builder.push(&previous)?;
        let mut ends = Vec::new();
        if let Some(samples) = grid {
            let width = function.num_outputs();
            let n = samples.len() / width;
            take(builder.meter, Cost::of(samples.len()).times(2))?;
            if !builder.meter.hold(n * size_of::<f32>()) {
                return Err(Exhausted);
            }
            let mut previous_slope = [0.; crate::function::MAX_COMPONENTS];
            ends.reserve_exact(n);
            for (i, pair) in samples.windows(2 * width).step_by(width).enumerate() {
                let mut split = false;
                for k in 0..width {
                    let slope = f64::from(pair[k + width]) - f64::from(pair[k]);
                    if linear_rgb {
                        split |= slope != previous_slope[k];
                    } else {
                        split |= slope * previous_slope[k] < 0.;
                    }
                    if linear_rgb || slope != 0. {
                        previous_slope[k] = slope;
                    }
                }
                if i > 0 && split {
                    ends.push(i as f32 / (n - 1) as f32);
                }
            }
            ends.push(1.);
        } else if linear_rgb {
            push_metered(&mut ends, 1., builder.meter)?;
        } else {
            let input = |t: f32| f64::from(parameter_at(domain, t));
            for f in function.functions() {
                f.piece_ends(
                    [0, 1_f32.to_bits()],
                    &input,
                    &mut ends,
                    builder.meter,
                    f.cost() + Cost::new(4),
                )?;
            }
            take(
                builder.meter,
                crate::function::sorting(Cost::of(ends.len())),
            )?;
            ends.sort_unstable_by(f32::total_cmp);
            ends.dedup();
        }
        let end_bytes = ends.capacity() * size_of::<f32>();
        for t in ends.into_iter().filter(|&t| t > 0.) {
            let current = builder.sample(t, Some(&previous))?;
            builder.interval(&previous, &current)?;
            previous = current;
        }
        builder.meter.release_bytes(end_bytes);
        let RampBuilder { stops, jumps, .. } = builder;
        let size = native_table_size(&stops);
        take(meter, Cost::of(stops.len() + jumps.len()).times(8))?;
        let shallow = stops.windows(2).all(|w| {
            let width = w[1].offset - w[0].offset;
            width >= 1e-7 && color_error(w[0].color, w[1].color) <= width * (size - 1) as f32 / 255.
        });
        let native_span = if shallow {
            f64::INFINITY
        } else {
            take(meter, Cost::of(size).times(8))?;
            table_span(&stops, &jumps, size)
        };
        meter.release_bytes(jumps.capacity() * size_of::<f32>());
        Ok(Some(Self { stops, native_span }))
    }

    pub(crate) fn lookup_cost(&self) -> Cost {
        Cost::new(19 + u64::from(self.stops.len().ilog2()) + 1)
    }

    fn get(&self, t: f32, cursor: &std::cell::Cell<usize>) -> [f32; 4] {
        if t <= 0. {
            return self.stops[0].color;
        }
        if t >= 1. {
            return self.stops[self.stops.len() - 1].color;
        }
        // Adjacent pixel centers commonly fall in the same piece or its neighbour.
        let mut hi = cursor.get();
        if !(self.stops[hi - 1].offset < t && t <= self.stops[hi].offset) {
            let next = if self.stops[hi].offset < t {
                hi + 1
            } else {
                hi.saturating_sub(1).max(1)
            };
            hi = if next < self.stops.len()
                && self.stops[next - 1].offset < t
                && t <= self.stops[next].offset
            {
                next
            } else {
                self.stops
                    .partition_point(|s| s.offset < t)
                    .clamp(1, self.stops.len() - 1)
            };
            cursor.set(hi);
        }
        let a = &self.stops[hi - 1];
        let b = &self.stops[hi];
        lerp_color(
            a.color,
            b.color,
            ((t - a.offset) / (b.offset - a.offset)).clamp(0., 1.),
        )
    }
}

fn native_table_size(stops: &[SvgGradientStop]) -> usize {
    let minimum = match stops.len() {
        2 => 256,
        3 => 512,
        _ => 1024,
    };
    stops
        .windows(2)
        .map(|w| (1.0 / (w[1].offset - w[0].offset)).ceil() as usize)
        .max()
        .unwrap_or(minimum)
        .clamp(minimum, MAX_LUT_SIZE)
        .next_power_of_two()
}

fn table_span(stops: &[SvgGradientStop], jumps: &[f32], size: usize) -> f64 {
    let mut limit = f64::INFINITY;
    let mut jump = 0;
    let mut next = 1;
    let mut start = stops[0].color;
    for bin in 1..size {
        let t = bin as f32 / (size - 1) as f32;
        let (mut lo, mut hi) = (start, start);
        let (mut rise, mut fall) = ([0_f32; 4], [0_f32; 4]);
        while next + 1 < stops.len() && stops[next].offset <= t {
            for k in 0..4 {
                rise[k] = rise[k].max(stops[next].color[k] - lo[k]);
                fall[k] = fall[k].max(hi[k] - stops[next].color[k]);
                lo[k] = lo[k].min(stops[next].color[k]);
                hi[k] = hi[k].max(stops[next].color[k]);
            }
            next += 1;
        }
        let (a, b) = (&stops[next - 1], &stops[next]);
        let width = b.offset - a.offset;
        if width <= 0. {
            return 0.;
        }
        start = lerp_color(a.color, b.color, (t - a.offset) / width);
        // Vello stores a slope and intercept and nudges zero-width ranges.
        for (k, &expected) in start.iter().enumerate() {
            let slope = (b.color[k] - a.color[k]) / width.max(1e-7);
            if (slope.mul_add(t, a.color[k] - a.offset * slope) - expected).abs() > 1.5 / 255. {
                return 0.;
            }
        }
        let previous_jump = jump;
        while jump < jumps.len() && jumps[jump] <= t {
            jump += 1;
        }
        // A monotone transition can move by one bin. A reversal inside that bin
        // can lose a peak even when both extrema fall between the endpoint colors.
        for k in 0..4 {
            let up = rise[k].max(start[k] - lo[k]);
            let down = fall[k].max(hi[k] - start[k]);
            if up.min(down) > 1.5 / 255. {
                return 0.;
            }
            if up.max(down) > 1.5 / 255. {
                if jump == previous_jump {
                    return 0.;
                }
                limit = (size - 1) as f64;
            }
        }
    }
    limit
}

impl EncodedRadialAxialShading {
    fn device_span(&self, pattern: &EncodedShadingPattern) -> f64 {
        match self.params {
            RadialAxialParams::Axial => {
                let [a, _, c, ..] = pattern.base_transform.as_coeffs();
                1.0 / a.hypot(c)
            }
            RadialAxialParams::Radial(crate::radial::Circles { p1, r, .. }) => {
                let [a, b, c, d, ..] = pattern.base_transform.inverse().as_coeffs();
                let scale = ((a + d).hypot(b - c) + (a - d).hypot(b + c)) * 0.5;
                scale * (p1.to_vec2().hypot() + (r.y - r.x).abs())
            }
        }
    }

    /// Whether native end padding agrees with the extended axial shading.
    pub fn native_extended(&self) -> bool {
        self.extend == [true; 2]
            && match self.params {
                RadialAxialParams::Axial => true,
                RadialAxialParams::Radial(crate::radial::Circles { p1, r, .. }) => {
                    p1 == Point::ZERO && r == Point::new(0., 1.)
                }
            }
    }

    /// Convert an axial/radial PDF shading into an SVG-like gradient approximation.
    pub fn as_svg_gradient(
        &self,
        pattern: &EncodedShadingPattern,
        tolerance: f32,
    ) -> Option<SvgGradient> {
        self.as_svg_gradient_metered(pattern, tolerance, &mut Unmetered)
            .expect("unmetered")
    }

    /// Convert a gradient, charging its function samples and stop storage before use.
    pub fn as_svg_gradient_metered(
        &self,
        pattern: &EncodedShadingPattern,
        tolerance: f32,
        meter: &mut dyn Meter,
    ) -> Result<Option<SvgGradient>, Exhausted> {
        if tolerance < LUT_TOLERANCE {
            return Ok(None);
        }
        if let RadialAxialParams::Radial(crate::radial::Circles { p1, r, .. }) = self.params {
            let placement = pattern.base_transform.inverse();
            let [a, b, c, d, x, y] = placement.as_coeffs();
            let reach = p1.to_vec2().hypot() + r.x.max(r.y);
            let device_extent =
                x.abs().max(y.abs()) + reach * (a.abs() + c.abs()).max(b.abs() + d.abs());
            // At this magnitude adjacent native f32 positions span a pixel center.
            if device_extent * f64::from(f32::EPSILON) >= 0.5 {
                return Ok(None);
            }
        }
        let background = pattern.background_color.components();
        let background = (background[3] > 0.0).then(|| pattern.finish(background));
        let Some(range) = self.padded_range() else {
            return Ok(None);
        };
        let radius_scale = match self.params {
            RadialAxialParams::Radial(circles) if circles.r.x == 0. => circles.r.y,
            _ => 1.,
        };
        let kind = self.gradient_kind(range, radius_scale);
        if let SvgGradientKind::Radial {
            start_radius,
            end_radius,
            ..
        } = kind
            && (end_radius - start_radius).abs() <= 1e-6
        {
            return Ok(None);
        }
        let Some(ramp) = &pattern.ramp else {
            return Ok(None);
        };
        let span = self.device_span(pattern) * (range[1] - range[0]);
        if ramp.native_span == 0.
            || span > ramp.native_span
            || (range != [0., 1.] && span > (MIN_LUT_SIZE - 1) as f64)
        {
            return Ok(None);
        }
        take(meter, Cost::of(ramp.stops.len()))?;
        scratch(
            meter,
            |meter| {
                let capacity = ramp.stops.len()
                    + 2 * (usize::from(range[0] < 0.) + usize::from(range[1] > 1.));
                if !meter.hold(capacity * size_of::<SvgGradientStop>()) {
                    return Err(Exhausted);
                }
                let mut stops = Vec::with_capacity(capacity);
                stops.extend_from_slice(&ramp.stops);
                let stops = pad_stops(stops, range, background);
                if range != [0., 1.] {
                    let size = native_table_size(&stops);
                    take(meter, Cost::of(size + stops.len()).times(8))?;
                    let limit = table_span(&stops, &[], size);
                    if limit == 0. || span > limit {
                        return Ok(None);
                    }
                }
                Ok(Some(SvgGradient {
                    kind,
                    transform: pattern.base_transform.inverse() * Affine::scale(radius_scale),
                    stops,
                }))
            },
            Option::is_some,
        )
    }

    // SVG gradients pad with their end stops, so a non-extended end gets a short stretch of background.
    fn padded_range(&self) -> Option<[f64; 2]> {
        let mut padding = self.extend.map(|extend| if extend { 0.0 } else { PADDING });

        if let RadialAxialParams::Radial(circles) = self.params {
            let crate::radial::Circles {
                p1,
                r,
                dr: growth,
                a,
            } = circles;
            // Only concentric circles preserve their degree under native focal/strip snapping.
            if a == 0.0 || p1 != Point::ZERO || r.x as f32 == r.y as f32 {
                return None;
            }

            if r.x < 0.0 || r.y < 0.0 {
                return None;
            }

            // The padded circles must not have a negative radius.
            padding[0] = padding[0].min(r.x / growth.max(0.0));
            padding[1] = padding[1].min(r.y / (-growth).max(0.0));
        }

        Some([-padding[0], 1.0 + padding[1]])
    }

    fn gradient_kind(&self, [t0, t1]: [f64; 2], scale: f64) -> SvgGradientKind {
        match &self.params {
            RadialAxialParams::Axial => SvgGradientKind::Linear {
                start: Point::new(t0, 0.0),
                end: Point::new(t1, 0.0),
            },
            RadialAxialParams::Radial(crate::radial::Circles { r, .. }) => {
                SvgGradientKind::Radial {
                    start_center: Point::ZERO,
                    start_radius: ((r.x + (r.y - r.x) * t0) / scale) as f32,
                    end_center: Point::ZERO,
                    end_radius: ((r.x + (r.y - r.x) * t1) / scale) as f32,
                }
            }
        }
    }
}

fn parameter_at([lo, hi]: [f32; 2], t: f32) -> f32 {
    if t == 1. {
        hi
    } else {
        (f64::from(hi) - f64::from(lo)).mul_add(f64::from(t), f64::from(lo)) as f32
    }
}

pub(crate) struct ParameterColors<'a> {
    cursor: std::cell::Cell<usize>,
    pattern: &'a EncodedShadingPattern,
    function: &'a ShadingFunction,
    range: [f32; 2],
}

impl<'a> ParameterColors<'a> {
    pub(crate) fn new(
        pattern: &'a EncodedShadingPattern,
        function: &'a ShadingFunction,
        range: [f32; 2],
    ) -> Self {
        Self {
            cursor: std::cell::Cell::new(1),
            pattern,
            function,
            range,
        }
    }
    pub(crate) fn cost(&self) -> Cost {
        self.pattern
            .ramp
            .as_ref()
            .map_or_else(|| self.pattern.point_cost(), |r| r.lookup_cost())
    }
    pub(crate) fn get(&self, t: f32) -> [f32; 4] {
        self.color(t).unwrap_or([0.; 4])
    }

    pub(crate) fn color(&self, t: f32) -> Option<[f32; 4]> {
        self.pattern.ramp.as_ref().map_or_else(
            || {
                self.pattern
                    .eval_color(self.function, &[parameter_at(self.range, t)])
            },
            |r| Some(r.get(t, &self.cursor)),
        )
    }
}

fn pad_stops(
    mut stops: Vec<SvgGradientStop>,
    [t0, t1]: [f64; 2],
    background: Option<[f32; 4]>,
) -> Vec<SvgGradientStop> {
    let (t0, t1) = (t0 as f32, t1 as f32);
    let padding = |offset| SvgGradientStop {
        offset,
        color: background.unwrap_or([0.0; 4]),
    };

    for stop in &mut stops {
        stop.offset = (stop.offset - t0) / (t1 - t0);
    }

    if t0 < 0.0 {
        let edge = stops[0].offset;
        stops.splice(0..0, [padding(0.0), padding(edge)]);
    }

    if t1 > 1.0 {
        let edge = stops[stops.len() - 1].offset;
        stops.extend([padding(edge), padding(1.0)]);
    }

    stops
}

#[cfg(test)]
mod tests {
    use super::SvgGradientStop;
    #[test]
    fn a_rising_bin_preserves_a_peak_between_its_end_colors() {
        let values = [
            (0., 0.),
            (0.5, 0.),
            (0.50001, 1.),
            (0.50002, 0.),
            (0.50003, 1.),
            (1., 1.),
        ];
        let stops = values.map(|(offset, v)| SvgGradientStop {
            offset,
            color: [v, v, v, 1.],
        });
        assert_eq!(
            super::table_span(&stops, &[0.50001, 0.50002, 0.50003], 4096),
            0.
        );
    }

    #[test]
    fn a_tint_step_cannot_hide_a_peak_within_a_table_entry() {
        let values = [
            (0., 0.),
            (0.50000, 0.),
            (0.50001, 1.),
            (0.50002, 0.),
            (1., 0.),
        ];
        let stops = values.map(|(offset, v)| SvgGradientStop {
            offset,
            color: [v, v, v, 1.],
        });
        assert_eq!(0., super::table_span(&stops, &[0.50001, 0.50002], 4096));
    }
}
