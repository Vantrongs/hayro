//! SVG-like gradient approximations of encoded shadings.
use crate::encode::{Cost, Exhausted, Meter, push_metered, take};

mod ramp;

pub(crate) use ramp::ParameterRamp;
pub use ramp::PreparedShading;

use crate::color::{color_error, lerp_color};
use crate::encode::{EncodedRadialAxialShading, EncodedShadingPattern, RadialAxialParams};
use crate::shading::ShadingFunction;
use kurbo::{Affine, Point};

const MAX_GRADIENT_SUBDIVISION_DEPTH: u8 = 10;
const RADIAL_EPSILON: f64 = 1.0e-6;
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

impl SvgGradient {
    /// Normalize geometry supported by the native raster gradient sampler.
    pub fn with_raster_geometry(self) -> Option<Self> {
        let (kind, transform) = self.kind.raster_geometry(self.transform)?;
        Some(Self {
            kind,
            transform,
            stops: self.stops,
        })
    }
}

impl SvgGradientKind {
    fn raster_geometry(self, transform: Affine) -> Option<(Self, Affine)> {
        match self {
            Self::Linear { .. } => Some((self, transform)),
            Self::Radial {
                start_center,
                start_radius,
                end_center,
                end_radius,
            } if start_center == end_center && (start_radius == 0. || end_radius == 0.) => {
                let radius = start_radius.max(end_radius);
                if !radius.is_finite() || radius <= 0. {
                    return None;
                }
                // Unit radii keep Vello's degeneracy check independent of the PDF coordinate scale.
                let kind = Self::Radial {
                    start_center: Point::ORIGIN,
                    start_radius: start_radius / radius,
                    end_center: Point::ORIGIN,
                    end_radius: end_radius / radius,
                };
                Some((
                    kind,
                    transform
                        * Affine::translate(start_center.to_vec2())
                        * Affine::scale(f64::from(radius)),
                ))
            }
            Self::Radial { .. } => None,
        }
    }
}

/// A color stop for an SVG-like gradient.
#[derive(Clone, Debug)]
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
        tolerance: f32,
    ) -> Option<SvgGradient> {
        let background = pattern.background_color.components();
        let background = (background[3] > 0.0).then(|| pattern.finish(background));
        let range = self.padded_range(background.is_some())?;

        let stops = self.native_stops(pattern, tolerance)?;

        Some(SvgGradient {
            kind: self.gradient_kind(range),
            transform: pattern.base_transform.inverse(),
            stops: pad_stops(stops, range, background)?,
        })
    }

    fn native_stops(
        &self,
        pattern: &EncodedShadingPattern,
        tolerance: f32,
    ) -> Option<Vec<SvgGradientStop>> {
        if !tolerance.is_finite() || tolerance < 0.0 {
            return None;
        }
        let direct_color =
            pattern.color_space.is_device_gray_or_rgb() && pattern.transfer_function.is_none();
        if direct_color && tolerance < 1.0 / 255.0 {
            return None;
        }

        let mut stops: Vec<SvgGradientStop> = Vec::new();
        self.function.linear_pieces(
            self.domain.map(f64::from),
            direct_color.then_some(tolerance),
            |[start, end], a, b| {
                if start >= end {
                    return None;
                }
                let (ca, cb) = (pattern.convert(a), pattern.convert(b));
                if !direct_color {
                    let expected = |t| lerp_color(ca, cb, (t - start) / (end - start));
                    let sample = |t, color, values: &[f32]| ColorSample {
                        t,
                        color,
                        codes: pattern.color_space.encode_values(values),
                    };
                    let mid = |t| {
                        let [lo, hi] = self.domain.map(f64::from);
                        let mut out = [0.0; crate::function::MAX_COMPONENTS];
                        let out = &mut out[..self.function.num_outputs()];
                        self.function
                            .eval_1d_f64(lo + (hi - lo) * f64::from(t), out)?;
                        Some(sample(t, pattern.convert(out), out))
                    };
                    check_color_interval(
                        &sample(start, ca, a),
                        &sample(end, cb, b),
                        &mid,
                        &expected,
                        tolerance,
                    )?;
                }
                if stops.last().is_none_or(|last| last.color != ca) {
                    stops.push(SvgGradientStop {
                        offset: start,
                        color: ca,
                    });
                }
                if let [.., left, middle] = &stops[..] {
                    let width = f64::from(end) - f64::from(left.offset);
                    let span = f64::from(middle.offset) - f64::from(left.offset);
                    if left.offset < middle.offset
                        && middle.offset < end
                        && (0..4).all(|i| {
                            (f64::from(middle.color[i]) - f64::from(left.color[i])) * width
                                == (f64::from(cb[i]) - f64::from(left.color[i])) * span
                        })
                    {
                        stops.pop();
                    }
                }
                stops.push(SvgGradientStop {
                    offset: end,
                    color: cb,
                });
                Some(())
            },
        )?;

        Some(stops)
    }

    // SVG gradients pad with their end stops, so a non-extended end gets a short stretch of background.
    fn padded_range(&self, has_background: bool) -> Option<[f64; 2]> {
        let mut padding = self.extend.map(|extend| if extend { 0.0 } else { PADDING });

        if let RadialAxialParams::Radial { p1, r } = self.params {
            let growth = r.y - r.x;
            let distance = p1.to_vec2().hypot();
            let nested = growth.abs() - distance > RADIAL_EPSILON;

            // Unless nested, circles past the end outrank earlier ones and some points need the background.
            if r.x < 0.0
                || r.y < 0.0
                || r.x.max(r.y) > f64::from(f32::MAX)
                || distance > growth.abs()
                || !nested && (has_background || !self.extend[1])
            {
                return None;
            }

            // The padded circles must not have a negative radius.
            padding[0] = padding[0].min(r.x / growth.max(0.0));
            padding[1] = padding[1].min(r.y / (-growth).max(0.0));
        }

        let end = 1.0 + padding[1];
        (padding[1] == 0. || end > 1.).then_some([-padding[0], end])
    }

    fn gradient_kind(&self, [t0, t1]: [f64; 2]) -> SvgGradientKind {
        match &self.params {
            RadialAxialParams::Axial { .. } => SvgGradientKind::Linear {
                start: Point::new(t0, 0.0),
                end: Point::new(t1, 0.0),
            },
            RadialAxialParams::Radial { p1, r } => SvgGradientKind::Radial {
                start_center: (p1.to_vec2() * t0).to_point(),
                start_radius: (r.x + (r.y - r.x) * t0) as f32,
                end_center: (p1.to_vec2() * t1).to_point(),
                end_radius: (r.x + (r.y - r.x) * t1) as f32,
            },
        }
    }
}

struct ColorSample {
    t: f32,
    color: [f32; 4],
    codes: smallvec::SmallVec<[u8; 4]>,
}

fn check_color_interval(
    a: &ColorSample,
    b: &ColorSample,
    sample: &impl Fn(f32) -> Option<ColorSample>,
    expected: &impl Fn(f32) -> [f32; 4],
    tolerance: f32,
) -> Option<()> {
    let (left, right) = (expected(a.t), expected(b.t));
    if color_error(a.color, left) > tolerance || color_error(b.color, right) > tolerance {
        return None;
    }
    // A monotone input crossing at most one byte boundary can only produce the two endpoint colors.
    let changes: u32 = a
        .codes
        .iter()
        .zip(&b.codes)
        .map(|(&x, &y)| u32::from(x.abs_diff(y)))
        .sum();
    if changes <= 1 && color_error(a.color, right).max(color_error(b.color, left)) <= tolerance {
        return Some(());
    }
    let t = a.t + (b.t - a.t) * 0.5;
    if t == a.t || t == b.t {
        return None;
    }
    let m = sample(t)?;
    check_color_interval(a, &m, sample, expected, tolerance)?;
    check_color_interval(&m, b, sample, expected, tolerance)
}

pub(crate) fn function_lut_metered(
    function: &ShadingFunction,
    range: [f32; 2],
    pattern: &EncodedShadingPattern,
    meter: &mut dyn Meter,
) -> Result<ColorLut, Exhausted> {
    let stops = metered_stops(function, range, pattern, LUT_TOLERANCE, meter)?;
    ColorLut::new(&stops, meter)
}

fn metered_stops(
    function: &ShadingFunction,
    range: [f32; 2],
    pattern: &EncodedShadingPattern,
    tolerance: f32,
    meter: &mut dyn Meter,
) -> Result<Vec<SvgGradientStop>, Exhausted> {
    let bounds = function.bounds_cost();
    take(meter, bounds)?;
    if !meter.hold(
        usize::try_from(
            (function.bounds_count() + Cost::new(2))
                .times(4 * size_of::<f32>() as u64)
                .steps(),
        )
        .unwrap_or(usize::MAX),
    ) {
        return Err(Exhausted);
    }
    approximate_gradient_stops(
        &mut Stops {
            meter,
            sample_cost: pattern.point_cost(),
            stops: Vec::new(),
        },
        |t| sample_function(function, range, pattern, t),
        &normalized_stitching_bounds(function, range),
        tolerance,
    )
}

fn sample_function(
    function: &ShadingFunction,
    [t0, t1]: [f32; 2],
    pattern: &EncodedShadingPattern,
    t: f32,
) -> [f32; 4] {
    let [lo, hi] = [t0, t1].map(f64::from);
    pattern.eval_1d(function, lo + (hi - lo) * f64::from(t))
}

fn normalized_stitching_bounds(function: &ShadingFunction, [t0, t1]: [f32; 2]) -> Vec<f32> {
    let dt = t1 - t0;
    if dt.abs() <= f32::EPSILON {
        return vec![];
    }

    let mut bounds = function
        .stitching_bounds()
        .into_iter()
        .map(|bound| (bound - t0) / dt)
        .filter(|bound| *bound > 0.0 && *bound < 1.0)
        .collect::<Vec<_>>();

    if dt < 0.0 {
        bounds.reverse();
    }

    bounds
}

pub(crate) struct ColorLut {
    colors: Vec<[f32; 4]>,
}

impl ColorLut {
    fn new(stops: &[SvgGradientStop], meter: &mut dyn Meter) -> Result<Self, Exhausted> {
        take(meter, Cost::of(stops.len()).times(2))?;
        let min_gap = stops
            .windows(2)
            .map(|w| w[1].offset - w[0].offset)
            .filter(|gap| *gap > 0.0)
            .fold(1.0, f32::min);
        let size = ((1.0 / min_gap).ceil() as usize)
            .clamp(MIN_LUT_SIZE, MAX_LUT_SIZE)
            .next_power_of_two();
        take(meter, Cost::of(size))?;
        if !meter.hold(size * size_of::<[f32; 4]>()) {
            return Err(Exhausted);
        }

        let mut segment = 0;
        let colors = (0..size)
            .map(|i| {
                let t = i as f32 / (size - 1) as f32;

                while segment + 2 < stops.len() && stops[segment + 1].offset < t {
                    segment += 1;
                }

                let (start, end) = (&stops[segment], &stops[(segment + 1).min(stops.len() - 1)]);
                let width = end.offset - start.offset;
                let local = if width > 0.0 {
                    ((t - start.offset) / width).clamp(0.0, 1.0)
                } else {
                    1.0
                };

                lerp_color(start.color, end.color, local)
            })
            .collect();

        Ok(Self { colors })
    }

    #[inline]
    pub(crate) fn get(&self, t: f32) -> [f32; 4] {
        let last = self.colors.len() - 1;
        self.colors[((t * last as f32 + 0.5) as usize).min(last)]
    }
}

fn pad_stops(
    mut stops: Vec<SvgGradientStop>,
    [t0, t1]: [f64; 2],
    background: Option<[f32; 4]>,
) -> Option<Vec<SvgGradientStop>> {
    let padding = |offset| SvgGradientStop {
        offset,
        color: background.unwrap_or([0.; 4]),
    };
    if t0 < 0. {
        stops.splice(0..0, [padding(0.), padding(0.)]);
    }
    if t1 > 1. {
        stops.extend([padding(1.), padding(1.)]);
    }

    let end = stops.len() - 1;
    let mut previous = None;
    for (i, stop) in stops.iter_mut().enumerate() {
        let position = match i {
            0 => t0,
            i if i == end => t1,
            _ => f64::from(stop.offset),
        };
        let offset = ((position - t0) / (t1 - t0)) as f32;
        if let Some((last, left)) = previous
            && position != last
        {
            // usvg merges within four ULPs and moves duplicates left by EPSILON (right at zero).
            let rightmost = if left == 0. { f32::EPSILON } else { left };
            if offset.to_bits().saturating_sub(left.to_bits()) <= 4
                || offset - f32::EPSILON <= rightmost
            {
                return None;
            }
        }
        stop.offset = offset;
        previous = Some((position, offset));
    }
    Some(stops)
}

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
        push_metered(
            &mut self.stops,
            SvgGradientStop { offset, color },
            self.meter,
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{AlphaColor, ColorSpace};
    use crate::encode::EncodedShadingType;
    use crate::function::Function;
    use hayro_syntax::object::{Dict, FromBytes, Object};

    #[test]
    fn adjacent_offsets_with_two_changed_ink_codes_are_unresolved() {
        let sample = |t, code| ColorSample {
            t,
            color: [0.0; 4],
            codes: smallvec::smallvec![code, code],
        };
        let a = sample(0.5, 127);
        let b = sample(a.t.next_up(), 128);
        assert!(check_color_interval(&a, &b, &|_| unreachable!(), &|_| a.color, 0.01).is_none());
    }

    fn stops(dict: &[u8]) -> Option<Vec<SvgGradientStop>> {
        let function = Function::new(&Object::Dict(Dict::from_bytes(dict).unwrap())).unwrap();
        let pattern = EncodedShadingPattern {
            base_transform: Affine::IDENTITY,
            color_space: ColorSpace::device_gray(),
            background_color: AlphaColor::TRANSPARENT,
            shading_type: EncodedShadingType::RadialAxial(EncodedRadialAxialShading {
                ramp: None,
                params: RadialAxialParams::Axial {
                    sampling_transform: Affine::IDENTITY,
                    expected_base_transform: Affine::IDENTITY,
                    start: Point::ZERO,
                    axis: kurbo::Vec2::new(1.0, 0.0),
                },
                domain: [0.0, 1.0],
                function: ShadingFunction::Single(function),
                extend: [true; 2],
            }),
            opacity: 1.0,
            transfer_function: None,
        };
        let EncodedShadingType::RadialAxial(shading) = &pattern.shading_type else {
            unreachable!()
        };
        Some(shading.as_svg_gradient(&pattern, 0.01)?.stops)
    }

    #[test]
    fn linear_and_stitched_gradients_keep_native_stops() {
        let line = "<< /FunctionType 2 /Domain [0 1] /N 1 >>";
        assert_eq!(stops(line.as_bytes()).unwrap().len(), 2);
        let constant = "<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 0 >>";
        assert_eq!(
            stops(constant.as_bytes())
                .unwrap()
                .iter()
                .map(|s| (s.offset, s.color[0]))
                .collect::<Vec<_>>(),
            [(0.0, 1.0), (1.0, 1.0)]
        );
        let stitch = |bound, encode| {
            format!(
                "<< /FunctionType 3 /Domain [0 1] /Bounds [{bound}] /Encode [{encode}] /Functions [{line} {line}] >>"
            )
        };
        let ramp = stops(stitch("0.5", "0 1 1 0").as_bytes()).unwrap();
        assert_eq!(
            ramp.iter()
                .map(|s| (s.offset, s.color[0]))
                .collect::<Vec<_>>(),
            [(0.0, 0.0), (0.5, 1.0), (1.0, 0.0)]
        );
        assert!(stops(stitch("0.00001", "0 1 1 0").as_bytes()).is_some());
        let jump = stops(stitch("0.5", "0 1 0 1").as_bytes()).unwrap();
        assert_eq!(
            jump.iter()
                .map(|s| (s.offset, s.color[0]))
                .collect::<Vec<_>>(),
            [(0.0, 0.0), (0.5, 1.0), (0.5, 0.0), (1.0, 1.0)]
        );
    }
}
