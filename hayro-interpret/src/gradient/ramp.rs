use super::{SvgGradient, SvgGradientStop};
use crate::CacheKey;
use crate::color::color_error;
use crate::encode::{
    EncodedRadialAxialShading, EncodedShadingPattern, EncodedShadingType, RadialAxialParams,
};
use crate::function::{Function, MAX_COMPONENTS};
use crate::pattern::ShadingPattern;
use crate::shading::ShadingFunction;
use kurbo::Affine;
use smallvec::SmallVec;
use std::sync::{Arc, OnceLock};

/// Color samples shared between placements of an axial or radial shading.
pub struct PreparedShading {
    pattern: ShadingPattern,
    ramp: Option<Arc<ParameterRamp>>,
}

impl PreparedShading {
    /// Number of parameter cells prepared for repeated texture samples.
    pub const MIN_SAMPLES: usize = PARAMETER_CELLS;

    /// Number of interior texels in the high-resolution axial parameter texture.
    pub const STRIP_SAMPLES: usize = PARAMETER_CELLS * 16;

    /// Identify a shading and its color inputs within the document, independently of placement.
    pub fn cache_key(pattern: &ShadingPattern) -> u128 {
        crate::util::hash128(&(
            pattern.shading.cache_key(),
            Arc::as_ptr(&pattern.shading.shading_type),
            pattern.shading.color_space.identity(),
            pattern.shading.background.as_ref().map(|v| {
                v.iter()
                    .map(|x| x.to_bits())
                    .collect::<SmallVec<[u32; 4]>>()
            }),
            pattern.opacity.to_bits(),
            &pattern.transfer_function,
        ))
    }

    /// Prepare the parameter colors independently of the placement.
    pub fn new(pattern: ShadingPattern) -> Self {
        let encoded = pattern.encode();
        let ramp = match &encoded.shading_type {
            EncodedShadingType::RadialAxial(g) => ParameterRamp::new(g, &encoded).map(Arc::new),
            _ => None,
        };
        Self { pattern, ramp }
    }

    /// Place the shading with its prepared colors.
    pub fn encode(&self, matrix: Affine) -> EncodedShadingPattern {
        let mut pattern = self.pattern.clone();
        pattern.matrix = matrix;
        let mut encoded = pattern.encode();
        if let EncodedShadingType::RadialAxial(g) = &mut encoded.shading_type {
            g.ramp.clone_from(&self.ramp);
        }
        encoded
    }
}

const PARAMETER_CELLS: usize = 32768;
const STRIP_ADDRESS_ERROR: usize = 2;
const STRIP_COLOR_ERROR: f32 = 2.0 / 255.0 + f32::EPSILON;

#[derive(Debug)]
pub(crate) struct ParameterRamp {
    cells: Vec<u16>,
    ranges: Vec<Option<ColorBounds>>,
    can_promote: bool,
    native: OnceLock<Option<Vec<SvgGradientStop>>>,
}

impl ParameterRamp {
    fn new(g: &EncodedRadialAxialShading, pattern: &EncodedShadingPattern) -> Option<Self> {
        let [lo, hi] = g.domain.map(f64::from);
        let parameter = |i: usize| {
            if i == PARAMETER_CELLS {
                hi
            } else {
                lo + (hi - lo) * (i as f64 / PARAMETER_CELLS as f64)
            }
        };
        let mut cells = Vec::with_capacity(PARAMETER_CELLS);
        let mut ranges = Vec::new();
        let mut can_promote =
            !pattern.color_space.is_device_gray_or_rgb() || pattern.transfer_function.is_some();
        let calculator = |f: &Function| f.calculator_interval([lo, hi]).is_some();
        can_promote &= !match &g.function {
            ShadingFunction::Single(f) => calculator(f),
            ShadingFunction::Multiple(fs) => fs.iter().any(calculator),
        };
        let mut budget = PARAMETER_CELLS * 8;
        let mut pending: SmallVec<[(usize, usize); 16]> = smallvec::smallvec![(0, PARAMETER_CELLS)];
        while let Some((start, end)) = pending.pop() {
            let domain = [parameter(start), parameter(end)];
            let interval = color_bounds(&g.function, domain, pattern, &mut budget)?;
            budget = budget.checked_sub(1)?;
            if end - start > 1 && !interval.is_some_and(|(lo, hi)| lo == hi) {
                let mid = (start + end) / 2;
                pending.push((mid, end));
                pending.push((start, mid));
                continue;
            }
            can_promote &= interval.is_some();
            if ranges.last() != Some(&interval) {
                ranges.push(interval);
            }
            cells.resize(end, (ranges.len() - 1) as u16);
        }
        ranges
            .iter()
            .flatten()
            .any(|(lo, hi)| lo == hi)
            .then_some(Self {
                cells,
                ranges,
                can_promote,
                native: OnceLock::new(),
            })
    }
    pub(crate) fn get(&self, t: f64) -> Option<[f32; 4]> {
        let index = ((t * PARAMETER_CELLS as f64) as usize).min(PARAMETER_CELLS - 1);
        let (lo, hi) = self.ranges[usize::from(self.cells[index])]?;
        (lo == hi).then_some(lo)
    }
}

type ColorBounds = ([f32; 4], [f32; 4]);

fn color_bounds(
    function: &ShadingFunction,
    domain: [f64; 2],
    pattern: &EncodedShadingPattern,
    budget: &mut usize,
) -> Option<Option<ColorBounds>> {
    let mut min = [1.; MAX_COMPONENTS];
    let mut max = [0.; MAX_COMPONENTS];
    let n = function.num_outputs();
    let (min, max) = (min.get_mut(..n)?, max.get_mut(..n)?);
    let mut collect = |f: &Function, first: usize| {
        if let Some(monotone) = f.calculator_interval(domain) {
            if !monotone {
                return Some(false);
            }
            let end = first + f.num_outputs();
            let a = min.get_mut(first..end)?;
            let b = max.get_mut(first..end)?;
            f.eval_1d_f64(domain[0], a)?;
            f.eval_1d_f64(domain[1], b)?;
            for (lo, hi) in a.iter_mut().zip(b) {
                if !lo.is_finite() || !hi.is_finite() {
                    return None;
                }
                if *lo > *hi {
                    std::mem::swap(lo, hi);
                }
            }
            return Some(true);
        }
        f.linear_pieces(
            domain,
            [0., 1.],
            budget,
            Some(f32::INFINITY),
            &mut |_, a, b| {
                for (k, (&x, &y)) in a.iter().zip(b).enumerate() {
                    min[first + k] = f32::min(min[first + k], x.min(y));
                    max[first + k] = f32::max(max[first + k], x.max(y));
                }
                Some(())
            },
        )
        .map(|()| true)
    };
    match function {
        ShadingFunction::Single(f) => {
            if !collect(f, 0)? {
                return Some(None);
            }
        }
        ShadingFunction::Multiple(fs) => {
            for (k, f) in fs.iter().enumerate() {
                if f.num_outputs() != 1 {
                    return None;
                }
                if !collect(f, k)? {
                    return Some(None);
                }
            }
        }
    }
    let a = pattern.color_space.encode_values(min);
    let b = pattern.color_space.encode_values(max);
    let mut indices = SmallVec::<[usize; 3]>::new();
    for (k, (&x, &y)) in a.iter().zip(&b).enumerate() {
        if x == y {
            continue;
        }
        if x.abs_diff(y) > 1 || indices.len() == 3 {
            return Some(None);
        }
        indices.push(k);
    }
    // Enumerate every possible byte tuple before the nonlinear tint, ICC and transfer conversions.
    let mut low = pattern.convert(min);
    let mut high = low;
    let mut values = SmallVec::<[f32; 4]>::from_slice(min);
    for mask in 1..(1 << indices.len()) {
        for (bit, &k) in indices.iter().enumerate() {
            values[k] = if mask & (1 << bit) == 0 {
                min[k]
            } else {
                max[k]
            };
        }
        let c = pattern.convert(&values);
        for k in 0..4 {
            low[k] = low[k].min(c[k]);
            high[k] = high[k].max(c[k]);
        }
    }
    Some(Some((low, high)))
}

impl EncodedRadialAxialShading {
    /// Sample an extended axial ramp whose converted colors vary by at most two levels locally.
    pub fn axial_strip(&self, pattern: &EncodedShadingPattern) -> Option<Vec<[f32; 4]>> {
        let len = PreparedShading::STRIP_SAMPLES;
        let ramp = self.ramp.as_ref()?;
        if !matches!(self.params, RadialAxialParams::Axial { .. })
            || self.extend != [true; 2]
            || !ramp.can_promote
        {
            return None;
        }
        let [a, b] = self.domain.map(f64::from);
        let start_color = pattern.eval_1d(&self.function, a);
        let end_color = pattern.eval_1d(&self.function, b);
        let mut colors = Vec::with_capacity(len + 2);
        colors.push(start_color);
        let mut budget = len;
        let texels_per_cell = len / PARAMETER_CELLS;
        let param = |k| {
            if k == len {
                b
            } else {
                a + (b - a) * (k as f64 / len as f64)
            }
        };
        for &(pad_color, lo_idx, hi_idx) in &[
            (start_color, 0, STRIP_ADDRESS_ERROR + 1),
            (end_color, len - STRIP_ADDRESS_ERROR, len),
        ] {
            let (lo, hi) = color_bounds(
                &self.function,
                [param(lo_idx), param(hi_idx)],
                pattern,
                &mut budget,
            )??;
            if lo[3] != hi[3]
                || lo[3] != pad_color[3]
                || color_error(lo, pad_color).max(color_error(hi, pad_color)) > STRIP_COLOR_ERROR
            {
                return None;
            }
        }
        for i in 0..len {
            let first = i.saturating_sub(STRIP_ADDRESS_ERROR) / texels_per_cell;
            let last = ((i + STRIP_ADDRESS_ERROR + 1) / texels_per_cell).min(PARAMETER_CELLS - 1);
            let t = (i as f64 + 0.5) / len as f64;
            let color = ramp
                .get(t)
                .unwrap_or_else(|| pattern.eval_1d(&self.function, a + (b - a) * t));
            let fits = |lo: [f32; 4], hi: [f32; 4]| {
                lo[3] == hi[3]
                    && color_error(lo, color).max(color_error(hi, color)) <= STRIP_COLOR_ERROR
            };
            if ramp.cells[first..=last]
                .iter()
                .all(|&id| ramp.ranges[usize::from(id)].is_some_and(|(lo, hi)| fits(lo, hi)))
            {
                colors.push(color);
                continue;
            }
            let (lo, hi) = color_bounds(
                &self.function,
                [
                    param(i.saturating_sub(STRIP_ADDRESS_ERROR)),
                    param((i + STRIP_ADDRESS_ERROR + 1).min(len)),
                ],
                pattern,
                &mut budget,
            )??;
            if fits(lo, hi) {
                colors.push(color);
            } else {
                let mut mid = std::array::from_fn(|k| ((lo[k] + hi[k]) * 127.5).round() / 255.);
                mid[3] = lo[3];
                if lo[3] != hi[3]
                    || color_error(mid, lo).max(color_error(mid, hi)) > STRIP_COLOR_ERROR
                {
                    return None;
                }
                colors.push(mid);
            }
        }
        colors.push(end_color);
        Some(colors)
    }

    /// Return a gradient whose native table entries reproduce the converted colors within one level.
    pub fn as_raster_gradient(&self, pattern: &EncodedShadingPattern) -> Option<SvgGradient> {
        let ramp = self.ramp.as_ref()?;
        if self.extend != [true; 2] || !ramp.can_promote {
            return None;
        }
        let (kind, transform) = self
            .gradient_kind([0., 1.])
            .raster_geometry(pattern.base_transform.inverse())?;
        let stops = ramp
            .native
            .get_or_init(|| {
                [1024, 2048, 4096]
                    .into_iter()
                    .find_map(|size| Self::raster_stops(ramp, size))
            })
            .as_ref()?;
        Some(SvgGradient {
            kind,
            transform,
            stops: stops.clone(),
        })
    }

    fn raster_stops(ramp: &ParameterRamp, size: usize) -> Option<Vec<SvgGradientStop>> {
        // Preserve adjacent stops so Vello selects this power-of-two table size.
        let mut stops: Vec<SvgGradientStop> = Vec::new();
        for i in 0..size {
            let offset = i as f32 / (size - 1) as f32;
            let first = (i * PARAMETER_CELLS / (size - 1))
                .saturating_sub(1)
                .min(PARAMETER_CELLS - 1);
            let last = ((i + 1) * PARAMETER_CELLS / (size - 1)).min(PARAMETER_CELLS - 1);
            let mut low = [1.0_f32; 4];
            let mut high = [0.0_f32; 4];
            for &id in &ramp.cells[first..=last] {
                let (lo, hi) = ramp.ranges[usize::from(id)]?;
                for c in 0..4 {
                    low[c] = low[c].min(lo[c]);
                    high[c] = high[c].max(hi[c]);
                }
            }
            let color = std::array::from_fn(|c| ((low[c] + high[c]) * 127.5).round() / 255.);
            if low[3] != 1.
                || color_error(color, low).max(color_error(color, high)) > 1. / 255. + f32::EPSILON
            {
                return None;
            }
            if let [.., a, b] = &stops[..]
                && i > 2
                && a.color == color
                && b.color == color
            {
                stops.pop();
            }
            stops.push(SvgGradientStop { offset, color });
        }
        Some(stops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::shading::Shading;
    use hayro_syntax::object::{Dict, FromBytes};

    #[test]
    fn native_bins_choose_the_smallest_table_covering_a_three_level_step() {
        let mut cells = vec![0; PARAMETER_CELLS];
        cells[12..].fill(1);
        cells[24..].fill(2);
        let gray = |b| [b, b, b, 1.];
        let ranges = [0., 2. / 255., 3. / 255.]
            .map(gray)
            .map(|v| Some((v, v)))
            .to_vec();
        let ramp = ParameterRamp {
            cells,
            ranges,
            can_promote: true,
            native: OnceLock::new(),
        };
        assert!(EncodedRadialAxialShading::raster_stops(&ramp, 1024).is_none());
        let stops = EncodedRadialAxialShading::raster_stops(&ramp, 2048).unwrap();
        assert_eq!(stops[0].color, gray(1. / 255.));
        assert_eq!(stops[1].offset, 1. / 2047.);
    }

    #[test]
    fn direct_color_ramps_reuse_only_constant_cells() {
        for (space, start, end) in [("DeviceGray", "0", "1"), ("DeviceRGB", "0 1 0", "1 0 0")] {
            let source = format!(
                "<< /ShadingType 2 /Coords [0 0 1 1] /Extend [true true]
                /ColorSpace /{space} /Function << /FunctionType 2 /Domain [0 1]
                /N 1 /C0 [{start}] /C1 [{end}] >> >>"
            );
            let dict = Dict::from_bytes(source.as_bytes()).unwrap();
            let pattern = ShadingPattern {
                shading: Arc::new(Shading::new(&dict, None, &Cache::default()).unwrap()),
                matrix: Affine::IDENTITY,
                opacity: 1.,
                transfer_function: None,
            };
            let encoded = PreparedShading::new(pattern).encode(Affine::IDENTITY);
            let EncodedShadingType::RadialAxial(g) = &encoded.shading_type else {
                unreachable!()
            };
            let ramp = g.ramp.as_ref().unwrap();
            assert_eq!(ramp.get(0.25), Some(encoded.eval_1d(&g.function, 0.25)));
            assert!(ramp.get(0.5 - f64::EPSILON).is_none());
            assert!(g.axial_strip(&encoded).is_none());
            assert!(g.as_raster_gradient(&encoded).is_none());
        }
    }

    #[test]
    fn unresolved_subtexel_spot_color_uses_exact_parameter_evaluation() {
        let dict = Dict::from_bytes(
            b"<< /ShadingType 2 /Coords [0 0 1 0] /Extend [true true]
            /ColorSpace [/Separation /Ink /DeviceRGB << /FunctionType 2 /Domain [0 1] /N 1 /C0 [0 0 0] /C1 [1 0 0] >>]
            /Function << /FunctionType 3 /Domain [0 1] /Bounds [.5000002 .5000004] /Encode [0 0 1 1 0 0]
            /Functions [<< /FunctionType 2 /Domain [0 1] /N 1 >> << /FunctionType 2 /Domain [0 1] /N 1 >> << /FunctionType 2 /Domain [0 1] /N 1 >>] >> >>",
        ).unwrap();
        let pattern = ShadingPattern {
            shading: Arc::new(Shading::new(&dict, None, &Cache::default()).unwrap()),
            matrix: Affine::IDENTITY,
            opacity: 1.,
            transfer_function: None,
        };
        let encoded = PreparedShading::new(pattern).encode(Affine::IDENTITY);
        let EncodedShadingType::RadialAxial(g) = &encoded.shading_type else {
            unreachable!()
        };
        assert!(g.as_raster_gradient(&encoded).is_none());
        assert!(g.axial_strip(&encoded).is_none());
        let ramp = g.ramp.as_ref().unwrap();
        assert_eq!(ramp.get(0.25), Some([0., 0., 0., 1.]));
        assert!(ramp.get(0.5000003).is_none());
        assert_eq!(encoded.eval_1d(&g.function, 0.5000003), [1., 0., 0., 1.]);
    }

    #[test]
    fn radial_native_promotion_uses_normalized_circles() {
        for (coords, native) in [
            ("0 0 100 0 0 500", true),
            ("0 0 0 1 0 2", true),
            ("0 0 2 1 0 0", true),
            ("0 0 0.0001 1 0 2", false),
            ("0 0 2 1 0 0.0001", false),
            ("0 0 0 1 0 100000000000000000000", false),
            ("0 .4 1 1 .4 1.0002", false),
        ] {
            let source = format!(
                "<< /ShadingType 3 /Coords [{coords}] /Extend [true true]
                /ColorSpace [/DeviceN [/Ink] /DeviceGray << /FunctionType 2 /Domain [0 1] /N 1 >>]
                /Function << /FunctionType 2 /Domain [0 1] /C0 [.25] /C1 [.25] /N 1 >> >>"
            );
            let dict = Dict::from_bytes(source.as_bytes()).unwrap();
            let pattern = ShadingPattern {
                shading: Arc::new(Shading::new(&dict, None, &Cache::default()).unwrap()),
                matrix: Affine::IDENTITY,
                opacity: 1.,
                transfer_function: None,
            };
            let encoded = PreparedShading::new(pattern).encode(Affine::IDENTITY);
            let EncodedShadingType::RadialAxial(g) = &encoded.shading_type else {
                unreachable!()
            };
            assert_eq!(g.as_raster_gradient(&encoded).is_some(), native, "{coords}");
            assert_eq!(
                g.as_svg_gradient(&encoded, 0.01)
                    .and_then(SvgGradient::with_raster_geometry)
                    .is_some(),
                native,
                "{coords}"
            );
        }
    }

    #[test]
    fn axial_strip_rejects_unvalidated_endpoint_step() {
        let dict = Dict::from_bytes(
            b"<< /ShadingType 2 /Coords [0 0 512 512] /Extend [true true] /Domain [-1 1]
            /ColorSpace [/Separation /Ink /DeviceGray << /FunctionType 2 /Domain [0 1] /N 0.75 /C0 [0] /C1 [1] >>]
            /Function << /FunctionType 3 /Domain [-1 1] /Bounds [0.99999994] /Encode [0 0 1 1]
            /Functions [<< /FunctionType 2 /Domain [0 1] /N 1 /C0 [0] /C1 [0] >> << /FunctionType 2 /Domain [0 1] /N 1 /C0 [0.004] /C1 [0.004] >>] >> >>",
        )
        .unwrap();
        let pattern = ShadingPattern {
            shading: Arc::new(Shading::new(&dict, None, &Cache::default()).unwrap()),
            matrix: Affine::IDENTITY,
            opacity: 1.,
            transfer_function: None,
        };
        let encoded = PreparedShading::new(pattern).encode(Affine::IDENTITY);
        let EncodedShadingType::RadialAxial(g) = &encoded.shading_type else {
            unreachable!()
        };
        assert!(g.axial_strip(&encoded).is_none());
    }
}
