use crate::canvas::{Canvas, RowShade, bounds};
use crate::color::ColorComponents;
use crate::encode::{
    Cost, EncodedMeshShading, EncodedShadingPattern, Exhausted, Meter, SamplePixel, take,
};
use crate::function::MAX_COMPONENTS;
use crate::gradient::ParameterColors;
use crate::shading::{Patch, ShadingFunction, ShadingType, Triangle};
use kurbo::{Affine, Point, Rect};

const FLATNESS: f64 = 0.25;
const MAX_DEPTH: u32 = 20;

struct Target<'a, 'b, P> {
    canvas: &'a mut Canvas<'b, P>,
    meter: &'a mut dyn Meter,
    vertex_cost: Cost,
}

pub(crate) fn draw_mesh<'b, P: SamplePixel>(
    canvas: &mut Canvas<'b, P>,
    mesh: &'b EncodedMeshShading,
    transform: Affine,
    pattern: &'b EncodedShadingPattern,
    meter: &mut dyn Meter,
) -> Result<(), Exhausted> {
    let (pieces, function, domain) = match mesh.shading_type.as_ref() {
        ShadingType::TriangleMesh {
            triangles,
            function,
            domain,
        } => {
            take(meter, Cost::of(triangles.len()).times(6))?;
            (Pieces::Triangles(triangles), function, domain)
        }
        ShadingType::PatchMesh {
            patches,
            function,
            domain,
        } => {
            let n = if function.is_some() {
                1
            } else {
                pattern.color_space.num_components()
            };
            take(meter, Cost::of(patches.len()).times(64 + 8 * u64::from(n)))?;
            let mut ranges = pattern.color_space.default_decode_arr(8.);
            if function.is_some() {
                ranges.clear();
                ranges.push((domain[0], domain[1]));
            } else if let Some(hival) = pattern.color_space.indexed_hival() {
                ranges[0] = (0., f32::from(hival));
            }
            let linear_direct = function.is_none()
                && mesh.unit_components
                && pattern.color_space.is_device_gray_or_rgb()
                && pattern.transfer_function.is_none();
            let tolerance = if linear_direct {
                1. / 2048.
            } else {
                1. / 32768.
            };
            let depth = mesh_depths(
                patches,
                transform,
                canvas.bounds(),
                &ranges,
                tolerance,
                !linear_direct,
                meter,
            )?;
            (Pieces::Patches(patches, depth), function, domain)
        }
        _ => return Ok(()),
    };
    let n = usize::from(pattern.color_space.num_components());
    let mut target = Target {
        canvas,
        meter,
        vertex_cost: Cost::of(n + 4),
    };
    if let Some(function) = function {
        let colors = Parameter::new(function, *domain, pattern);
        pieces.draw(&mut target, transform, &colors)?;
        target.canvas.set_row_shade(RowShade::Parameter(colors.lut));
    } else if mesh.unit_components
        && pattern.color_space.is_device_gray_or_rgb()
        && pattern.transfer_function.is_none()
    {
        target.vertex_cost = target.vertex_cost + pattern.color_space.cost();
        pieces.draw(&mut target, transform, &DirectLinear { pattern })?;
    } else {
        let colors = DirectComponents {
            pattern,
            num_components: n,
        };
        target.canvas.set_row_shade(RowShade::Direct {
            pattern,
            num_components: n,
        });
        match n {
            1 => pieces.draw::<1, _>(&mut target, transform, &colors)?,
            2 => pieces.draw::<2, _>(&mut target, transform, &colors)?,
            3 => pieces.draw::<3, _>(&mut target, transform, &colors)?,
            4 => pieces.draw::<4, _>(&mut target, transform, &colors)?,
            _ => {
                pieces.draw::<MAX_COMPONENTS, _>(&mut target, transform, &colors)?;
            }
        }
    }
    if let Pieces::Patches(_, PatchDepths::Many(depths)) = pieces {
        let bytes = depths.capacity();
        drop(depths);
        target.meter.release_bytes(bytes);
    }
    Ok(())
}

enum Pieces<'a> {
    Triangles(&'a [Triangle]),
    Patches(&'a [Patch], PatchDepths),
}

impl Pieces<'_> {
    fn draw<const N: usize, P: SamplePixel>(
        &self,
        target: &mut Target<'_, '_, P>,
        transform: Affine,
        colors: &impl MeshColors<N>,
    ) -> Result<(), Exhausted> {
        match self {
            Self::Triangles(triangles) => {
                for triangle in *triangles {
                    take(target.meter, Cost::new(1))?;
                    let points = triangle.vertices.each_ref().map(|v| transform * v.point);
                    if target.canvas.is_visible(&points) {
                        take(target.meter, target.vertex_cost.times(3))?;
                        let values = triangle
                            .vertices
                            .each_ref()
                            .map(|v| colors.values(&v.colors));
                        target.canvas.fill_triangle(
                            points,
                            values,
                            &|v| P::evaluated(colors.shade(v)),
                            colors.cost(),
                            target.meter,
                        )?;
                    }
                }
            }
            Self::Patches(patches, depth) => {
                for (i, p) in distinct_patches(patches).enumerate() {
                    let patch = CanvasPatch::new(p, transform);
                    if !patch.bounds().overlaps(target.canvas.bounds()) {
                        continue;
                    }
                    draw_patch_piece(
                        target,
                        &patch,
                        patch.points,
                        [[0., 1.]; 2],
                        [depth.at(i); 2],
                        colors,
                    )?;
                }
            }
        }
        Ok(())
    }
}

// Consecutive identical patches replace exactly the same samples on every canvas.
fn distinct_patches(patches: &[Patch]) -> impl Iterator<Item = &Patch> {
    patches
        .chunk_by(|a, b| a.control_points == b.control_points && a.colors == b.colors)
        .map(|run| &run[0])
}

enum PatchDepths {
    Single(u8),
    Many(Vec<u8>),
}

impl PatchDepths {
    fn at(&self, i: usize) -> u32 {
        u32::from(match self {
            Self::Single(d) => *d,
            Self::Many(depths) => depths[i],
        })
    }
}

type EdgeKey = [[u64; 2]; 4];

fn edge_key(points: [Point; 4]) -> Option<EdgeKey> {
    if !points.iter().all(|p| p.is_finite()) {
        return None;
    }
    let forward = points.map(|p| [p.x, p.y].map(|v| if v == 0. { 0 } else { v.to_bits() }));
    let [a, b, c, d] = forward;
    Some(forward.min([d, c, b, a]))
}

fn root(parent: &mut [usize], mut i: usize, meter: &mut dyn Meter) -> Result<usize, Exhausted> {
    while parent[i] != i {
        take(meter, Cost::new(1))?;
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    Ok(i)
}

fn mesh_depths(
    patches: &[Patch],
    transform: Affine,
    canvas: Rect,
    ranges: &[(f32, f32)],
    tolerance: f64,
    nonlinear: bool,
    meter: &mut dyn Meter,
) -> Result<PatchDepths, Exhausted> {
    let linear = transform.with_translation(kurbo::Vec2::ZERO);
    let visible = |p| CanvasPatch::new(p, transform).bounds().overlaps(canvas);
    let depth = |p| CanvasPatch::new(p, linear).depth(ranges, tolerance, nonlinear) as u8;
    let n = distinct_patches(patches).count();
    if n <= 1 {
        return Ok(PatchDepths::Single(
            patches.first().filter(|p| visible(p)).map_or(0, depth),
        ));
    }
    take(meter, Cost::of(n).times(4 * 8 + 16))?;
    if !meter.hold(n) {
        return Err(Exhausted);
    }
    let mut depths = vec![u8::MAX; n];
    let bytes = n.checked_mul(size_of::<usize>()).ok_or(Exhausted)?;
    if !meter.hold(bytes) {
        return Err(Exhausted);
    }
    let mut parent: Vec<_> = (0..n).collect();
    crate::encode::scratch(
        meter,
        |meter| join_edges(patches, &mut parent, meter),
        |_| false,
    )?;
    for (i, patch) in distinct_patches(patches).enumerate() {
        parent[i] = root(&mut parent, i, meter)?;
        if visible(patch) {
            depths[parent[i]] = 0;
        }
    }
    for (i, patch) in distinct_patches(patches).enumerate() {
        let component = &mut depths[parent[i]];
        if *component != u8::MAX {
            *component = (*component).max(depth(patch));
        }
    }
    for (i, &component) in parent.iter().enumerate() {
        depths[i] = depths[component];
    }
    drop(parent);
    meter.release_bytes(bytes);
    Ok(PatchDepths::Many(depths))
}

fn join_edges(
    patches: &[Patch],
    parent: &mut [usize],
    meter: &mut dyn Meter,
) -> Result<(), Exhausted> {
    let mut edges = rustc_hash::FxHashMap::default();
    let mut held = 0;
    for (i, patch) in distinct_patches(patches).enumerate() {
        let p = &patch.control_points;
        for key in [p[0], p[3], p.map(|col| col[0]), p.map(|col| col[3])]
            .into_iter()
            .filter_map(edge_key)
        {
            if let Some(&other) = edges.get(&key) {
                let a = root(parent, i, meter)?;
                let b = root(parent, other, meter)?;
                parent[a] = b;
            } else {
                if edges.len() == edges.capacity() {
                    take(meter, Cost::of(edges.len()).times(8))?;
                    // Rehashing holds both tables, including control bytes and a trailing SIMD group.
                    let buckets = edges
                        .capacity()
                        .checked_next_power_of_two()
                        .and_then(|v| v.checked_mul(2))
                        .ok_or(Exhausted)?
                        .max(4);
                    let bytes = buckets
                        .checked_mul(size_of::<(EdgeKey, usize)>() + 1)
                        .and_then(|v| v.checked_add(32))
                        .ok_or(Exhausted)?;
                    if !meter.hold(bytes) {
                        return Err(Exhausted);
                    }
                    edges.try_reserve(1).map_err(|_| Exhausted)?;
                    meter.release_bytes(held);
                    held = bytes;
                }
                edges.insert(key, i);
            }
        }
    }
    Ok(())
}

trait MeshColors<const N: usize> {
    fn cost(&self) -> Cost {
        Cost::ZERO
    }
    fn values(&self, components: &[f32]) -> [f32; N];
    fn shade(&self, values: &[f32; N]) -> Option<[f32; 4]>;
}

struct Parameter<'a> {
    lut: ParameterColors<'a>,
    t0: f32,
    span: f64,
}

impl<'a> Parameter<'a> {
    fn new(
        function: &'a ShadingFunction,
        [t0, t1]: [f32; 2],
        pattern: &'a EncodedShadingPattern,
    ) -> Self {
        Self {
            lut: ParameterColors::new(pattern, function, [t0, t1]),
            t0,
            span: f64::from(t1) - f64::from(t0),
        }
    }
}

impl MeshColors<1> for Parameter<'_> {
    fn cost(&self) -> Cost {
        self.lut.cost()
    }
    fn values(&self, components: &[f32]) -> [f32; 1] {
        let value = f64::from(components.first().copied().unwrap_or(0.0));
        [if self.span == 0. {
            0.
        } else {
            ((value - f64::from(self.t0)) / self.span) as f32
        }]
    }

    fn shade(&self, values: &[f32; 1]) -> Option<[f32; 4]> {
        self.lut.color(values[0])
    }
}

struct DirectComponents<'a> {
    pattern: &'a EncodedShadingPattern,
    num_components: usize,
}

impl<const N: usize> MeshColors<N> for DirectComponents<'_> {
    fn cost(&self) -> Cost {
        self.pattern.color_space.cost()
            + Cost::new(3 * u64::from(self.pattern.transfer_function.is_some()))
    }
    fn values(&self, components: &[f32]) -> [f32; N] {
        let mut out = [0.; N];
        let used = components.len().min(self.num_components);
        out[..used].copy_from_slice(&components[..used]);
        if used < self.num_components {
            out[used..self.num_components]
                .copy_from_slice(&self.pattern.color_space.initial_color()[used..]);
        }
        out
    }

    fn shade(&self, values: &[f32; N]) -> Option<[f32; 4]> {
        Some(self.pattern.convert(&values[..self.num_components]))
    }
}

struct DirectLinear<'a> {
    pattern: &'a EncodedShadingPattern,
}

impl MeshColors<4> for DirectLinear<'_> {
    fn values(&self, components: &[f32]) -> [f32; 4] {
        self.pattern.convert(components)
    }

    fn shade(&self, values: &[f32; 4]) -> Option<[f32; 4]> {
        Some(*values)
    }
}

fn mix<'a, const K: usize>(
    colors: [&ColorComponents; K],
    weights: [f32; K],
    out: &'a mut [f32; MAX_COMPONENTS],
) -> &'a [f32] {
    let len = colors
        .iter()
        .map(|c| c.len())
        .min()
        .unwrap_or(0)
        .min(MAX_COMPONENTS);

    for (i, out) in out[..len].iter_mut().enumerate() {
        let values = colors.map(|color| color[i]);
        let first = values[0];
        *out = if values.iter().all(|&v| v == first) {
            first
        } else {
            let min = values.into_iter().fold(first, f32::min);
            let max = values.into_iter().fold(first, f32::max);
            values
                .into_iter()
                .zip(weights)
                .map(|(v, w)| v * w)
                .sum::<f32>()
                .max(min)
                .min(max)
        };
    }

    &out[..len]
}

struct CanvasPatch<'a> {
    points: [[Point; 4]; 4],
    colors: &'a [[ColorComponents; 2]; 2],
}

impl<'a> CanvasPatch<'a> {
    fn new(patch: &'a Patch, transform: Affine) -> Self {
        Self {
            points: patch
                .control_points
                .map(|column| column.map(|p| transform * p)),
            colors: &patch.colors,
        }
    }

    fn depth(&self, ranges: &[(f32, f32)], tolerance: f64, nonlinear: bool) -> u32 {
        let second_difference = |[a, b, c, d]: [Point; 4]| {
            let middle = c - b;
            (middle - (b - a)).hypot().max(((d - c) - middle).hypot())
        };
        let rows = (0..4).map(|j| self.points.map(|column| column[j]));
        let u = rows.map(second_difference).fold(0., f64::max);
        let v = self
            .points
            .map(second_difference)
            .into_iter()
            .fold(0., f64::max);
        let [left, _, _, right] = self.points;
        let corner = ((right[3] - left[3]) - (right[0] - left[0])).hypot();
        let pure = 0.75 * (u + v);
        let geometry = pure + 0.25 * corner;

        let [[c00, c01], [c10, c11]] = self.colors;
        let mut twist: f64 = 0.;
        let mut relative_twist: f64 = 0.;
        for (i, &(lo, hi)) in ranges.iter().enumerate() {
            let (Some(&v00), Some(&v10), Some(&v01), Some(&v11)) =
                (c00.get(i), c10.get(i), c01.get(i), c11.get(i))
            else {
                continue;
            };
            let delta = (f64::from(v00) - f64::from(v10) - f64::from(v01) + f64::from(v11)).abs();
            let span = (f64::from(hi) - f64::from(lo)).abs();
            if span > 0. {
                twist = twist.max(delta / span);
            }
            if nonlinear {
                let min = v00.min(v10).min(v01).min(v11);
                let max = v00.max(v10).max(v01).max(v11);
                let patch_span = f64::from(max) - f64::from(min);
                if patch_span > 0. {
                    relative_twist = relative_twist.max(delta / patch_span);
                }
            }
        }
        let contour_ratio = if relative_twist > 0. {
            let b = self.bounds();
            let extent = b.width().hypot(b.height());
            (0.25 * extent * relative_twist) / FLATNESS
        } else {
            0.
        };
        let color_ratio = (0.25 * twist / tolerance).max(contour_ratio);
        if geometry <= FLATNESS && color_ratio <= 1. {
            return 0;
        }
        // A child rectangle's corner twist integrates the mixed derivative over its area.
        let mut mixed: f64 = 0.;
        for pair in self.points.windows(2) {
            for j in 0..3 {
                mixed = mixed
                    .max(((pair[1][j + 1] - pair[1][j]) - (pair[0][j + 1] - pair[0][j])).hypot());
            }
        }
        let ratio = ((pure + 2.25 * mixed) / FLATNESS).max(color_ratio);
        (0.5 * ratio.max(1.).log2())
            .ceil()
            .min(f64::from(MAX_DEPTH)) as u32
    }

    fn bounds(&self) -> Rect {
        bounds(self.points.as_flattened())
    }

    fn components<'b>(&self, u: f32, v: f32, out: &'b mut [f32; MAX_COMPONENTS]) -> &'b [f32] {
        let [[c00, c01], [c10, c11]] = self.colors;
        let weights = [(1.0 - u) * (1.0 - v), (1.0 - u) * v, u * (1.0 - v), u * v];

        mix([c00, c01, c10, c11], weights, out)
    }
}

fn draw_patch_piece<const N: usize, P: SamplePixel>(
    target: &mut Target<'_, '_, P>,
    patch: &CanvasPatch<'_>,
    points: [[Point; 4]; 4],
    [u_range, v_range]: [[f32; 2]; 2],
    [u_depth, v_depth]: [u32; 2],
    colors: &impl MeshColors<N>,
) -> Result<(), Exhausted> {
    take(target.meter, Cost::new(1))?;
    if !target.canvas.is_visible(points.as_flattened()) {
        return Ok(());
    }

    let halves = |[a, b]: [f32; 2]| [[a, 0.5 * (a + b)], [0.5 * (a + b), b]];

    if v_depth > 0 {
        let (low, high) = split_rows(points);

        for (points, v_range) in [low, high].into_iter().zip(halves(v_range)) {
            let ranges = [u_range, v_range];
            draw_patch_piece(
                target,
                patch,
                points,
                ranges,
                [u_depth, v_depth - 1],
                colors,
            )?;
        }
    } else if u_depth > 0 {
        let (low, high) = split_columns(points);

        for (points, u_range) in [low, high].into_iter().zip(halves(u_range)) {
            let ranges = [u_range, v_range];
            draw_patch_piece(
                target,
                patch,
                points,
                ranges,
                [u_depth - 1, v_depth],
                colors,
            )?;
        }
    } else {
        let ([u0, u1], [v0, v1]) = (u_range, v_range);
        let mut buffer = [0.0; MAX_COMPONENTS];
        take(target.meter, target.vertex_cost.times(4))?;
        let mut values = |u, v| colors.values(patch.components(u, v, &mut buffer));
        let (c00, c10, c01, c11) = (
            values(u0, v0),
            values(u1, v0),
            values(u0, v1),
            values(u1, v1),
        );
        let (p00, p10, p01, p11) = (points[0][0], points[3][0], points[0][3], points[3][3]);
        let shade = |v: &[f32; N]| P::evaluated(colors.shade(v));

        target.canvas.fill_triangle(
            [p00, p10, p01],
            [c00, c10, c01],
            &shade,
            colors.cost(),
            target.meter,
        )?;
        target.canvas.fill_triangle(
            [p10, p11, p01],
            [c10, c11, c01],
            &shade,
            colors.cost(),
            target.meter,
        )?;
    }
    Ok(())
}

fn split_rows(points: [[Point; 4]; 4]) -> ([[Point; 4]; 4], [[Point; 4]; 4]) {
    let split = points.map(split_cubic);

    (split.map(|(low, _)| low), split.map(|(_, high)| high))
}

fn split_columns(points: [[Point; 4]; 4]) -> ([[Point; 4]; 4], [[Point; 4]; 4]) {
    let mut low = points;
    let mut high = points;

    for j in 0..4 {
        let (l, h) = split_cubic(points.map(|column| column[j]));

        for i in 0..4 {
            low[i][j] = l[i];
            high[i][j] = h[i];
        }
    }

    (low, high)
}

fn split_cubic([p0, p1, p2, p3]: [Point; 4]) -> ([Point; 4], [Point; 4]) {
    let (p01, p12, p23) = (p0.midpoint(p1), p1.midpoint(p2), p2.midpoint(p3));
    let (p012, p123) = (p01.midpoint(p12), p12.midpoint(p23));
    let p0123 = p012.midpoint(p123);

    ([p0, p01, p012, p0123], [p0123, p123, p23, p3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inflected_edge_meets_flatness() {
        let points = core::array::from_fn(|i| {
            [0., 1., 2., 3.].map(|v| Point::new(i as f64, v + [0., 1., -1., 0.][i]))
        });
        let patch = CanvasPatch {
            points,
            colors: &Default::default(),
        };
        assert!(patch.depth(&[], 1., false) >= 2);
    }

    #[test]
    fn combined_curvature_depth() {
        let points = core::array::from_fn(|i| {
            core::array::from_fn(|j| {
                let u = i as f64 / 3.;
                let v = j as f64 / 3.;
                let bend = |k| if k == 1 || k == 2 { 0.33 } else { 0. };
                Point::new(40. * u + bend(i) + bend(j) + 0.96 * u * v, 100. * v)
            })
        });
        let patch = CanvasPatch {
            points,
            colors: &Default::default(),
        };
        assert_eq!(patch.depth(&[], 1., false), 1);
    }
}
