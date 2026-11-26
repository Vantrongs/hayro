use crate::canvas::{Canvas, RowShade, bounds};
use crate::color::ColorComponents;
use crate::encode::{
    Cost, EncodedMeshShading, EncodedShadingPattern, Exhausted, Meter, SamplePixel, take,
};
use crate::function::MAX_COMPONENTS;
use crate::gradient::{ColorLut, function_lut_metered};
use crate::shading::{Patch, ShadingFunction, ShadingType, Triangle};
use kurbo::{Affine, Point, Rect};

const MESH_SEGMENT: f64 = 8.;
const MAX_DEPTH: u32 = 10;
const PATCH_FLATNESS: f64 = 0.125;
const COMPONENT_FLATNESS: f64 = 0.25;
const MAX_COMPONENT_DEPTH: u32 = 20;
const COMPONENT_TOLERANCE: f64 = 1. / 32768.;

struct Target<'a, 'b, P> {
    canvas: &'a mut Canvas<'b, P>,
    meter: &'a mut dyn Meter,
    pixel_centers: bool,
    vertex_cost: Cost,
}

pub(crate) fn draw_mesh<'a, P: SamplePixel>(
    canvas: &mut Canvas<'a, P>,
    mesh: &'a EncodedMeshShading,
    transform: Affine,
    pattern: &'a EncodedShadingPattern,
    meter: &mut dyn Meter,
) -> Result<(), Exhausted> {
    let n = usize::from(pattern.color_space.num_components());
    let linear = mesh.unit_components
        && pattern.color_space.is_device_gray_or_rgb()
        && pattern.transfer_function.is_none();
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
            take(meter, Cost::of(patches.len()).times(64 + 8 * n as u64))?;
            let ranges = if function.is_some() {
                smallvec::smallvec![(domain[0], domain[1])]
            } else {
                pattern.color_space.component_ranges()
            };
            let depths = mesh_depths(
                patches,
                transform,
                canvas.bounds(),
                &ranges,
                linear && function.is_none(),
                meter,
            )?;
            (Pieces::Patches(patches, depths), function, domain)
        }
        _ => return Ok(()),
    };
    let mut target = Target {
        pixel_centers: canvas.width > 0,
        canvas,
        meter,
        vertex_cost: Cost::of(n + 4),
    };
    if let Some(function) = function {
        let parameter = Parameter::new::<P>(function, *domain, pattern, target.meter)?;
        pieces.draw(&mut target, transform, &parameter)?;
        target
            .canvas
            .set_row_shade(RowShade::Parameter(parameter.lut));
    } else if linear {
        target.vertex_cost = target.vertex_cost + pattern.color_space.prepared().sample;
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
            _ => pieces.draw::<MAX_COMPONENTS, _>(&mut target, transform, &colors)?,
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
                            &|v| P::evaluated(Some(colors.shade(v))),
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
                    draw_patch(target, &patch, depth.at(i), colors)?;
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
    linear_color: bool,
    meter: &mut dyn Meter,
) -> Result<PatchDepths, Exhausted> {
    let linear = transform.with_translation(kurbo::Vec2::ZERO);
    let visible = |p| CanvasPatch::new(p, transform).bounds().overlaps(canvas);
    let depth = |p| {
        let patch = CanvasPatch::new(p, linear);
        if linear_color {
            let fine = (patch.extent() / MESH_SEGMENT)
                .max(1.)
                .log2()
                .ceil()
                .min(f64::from(MAX_DEPTH)) as u32;
            patch.linear_depth(fine) as u8
        } else {
            patch.component_depth(ranges) as u8
        }
    };
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
    crate::encode::scratch(meter, |meter| join_edges(patches, &mut parent, meter))?;
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
    fn shade(&self, values: &[f32; N]) -> [f32; 4];
}

struct Parameter {
    lut: Option<ColorLut>,
    t0: f32,
    scale: f64,
}

impl Parameter {
    fn new<P: SamplePixel>(
        function: &ShadingFunction,
        [t0, t1]: [f32; 2],
        pattern: &EncodedShadingPattern,
        meter: &mut dyn Meter,
    ) -> Result<Self, Exhausted> {
        if P::CLASSIFIED {
            return Ok(Self {
                lut: None,
                t0: 0.,
                scale: 1.,
            });
        }
        Ok(Self {
            lut: Some(function_lut_metered(function, [t0, t1], pattern, meter)?),
            t0,
            scale: if t1 != t0 {
                1.0 / (f64::from(t1) - f64::from(t0))
            } else {
                0.0
            },
        })
    }
}

impl MeshColors<1> for Parameter {
    fn values(&self, components: &[f32]) -> [f32; 1] {
        [
            ((f64::from(components.first().copied().unwrap_or(0.0)) - f64::from(self.t0))
                * self.scale) as f32,
        ]
    }

    fn shade(&self, values: &[f32; 1]) -> [f32; 4] {
        self.lut
            .as_ref()
            .map_or([values[0], 0., 0., 0.], |lut| lut.get(values[0]))
    }
}

struct DirectComponents<'a> {
    pattern: &'a EncodedShadingPattern,
    num_components: usize,
}

impl<const N: usize> MeshColors<N> for DirectComponents<'_> {
    fn cost(&self) -> Cost {
        self.pattern.color_space.prepared().sample
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

    fn shade(&self, values: &[f32; N]) -> [f32; 4] {
        self.pattern.convert(&values[..self.num_components])
    }
}

struct DirectLinear<'a> {
    pattern: &'a EncodedShadingPattern,
}

impl MeshColors<4> for DirectLinear<'_> {
    fn values(&self, components: &[f32]) -> [f32; 4] {
        self.pattern.convert(components)
    }

    fn shade(&self, values: &[f32; 4]) -> [f32; 4] {
        *values
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

    fn extent(&self) -> f64 {
        let length = |points: [Point; 4]| points.windows(2).map(|w| (w[1] - w[0]).hypot()).sum();
        let columns = self.points.map(length);
        let rows = [0, 1, 2, 3].map(|j| length(self.points.map(|column| column[j])));

        columns.into_iter().chain(rows).fold(0.0, f64::max)
    }

    fn linear_depth(&self, fine: u32) -> u32 {
        let curved = |[a, b, c, d]: [Point; 4]| {
            let axis = d - a;

            if axis.hypot2() == 0. {
                return a != b || a != c;
            }
            (b - a).cross(axis) != 0.
                || (c - a).cross(axis) != 0.
                || (b - a).dot(axis) < 0.
                || (c - b).dot(axis) < 0.
                || (d - c).dot(axis) < 0.
        };
        let net = &self.points;
        if [net[0], net[3], net.map(|c| c[0]), net.map(|c| c[3])]
            .into_iter()
            .any(curved)
        {
            return fine;
        }
        let [left, _, _, right] = self.points;
        // Each parametric tangent is a convex combination of control-net differences.
        let orientation = (right[0] - left[0]).cross(left[3] - left[0]).signum();
        for pair in net.windows(2) {
            for (a, b) in pair[0].iter().zip(&pair[1]) {
                let du = *b - *a;
                for column in net {
                    if !column
                        .windows(2)
                        .all(|v| du.cross(v[1] - v[0]) * orientation > 0.)
                    {
                        return fine;
                    }
                }
            }
        }

        let [[c00, c01], [c10, c11]] = self.colors;
        let components = c00
            .len()
            .min(c01.len())
            .min(c10.len())
            .min(c11.len())
            .min(MAX_COMPONENTS);
        // Retain the fine grid when clamping or intermediate vertex quantization changes steps.
        let divisions = f64::from(1_u32 << fine);
        for k in 0..components {
            let corners = [c00[k], c01[k], c10[k], c11[k]];
            if corners.iter().any(|v| !(0.0..=1.0).contains(v)) {
                return fine;
            }
            let [a, b, c, d] = corners.map(|v| f64::from(v) * 255.);
            if [b - a, c - a, d - c, d - b]
                .iter()
                .any(|v| (v / divisions).fract() != 0.)
            {
                return fine;
            }
        }
        let twist = (0..components)
            .map(|k| {
                (f64::from(c00[k]) - f64::from(c01[k]) - f64::from(c10[k]) + f64::from(c11[k]))
                    .abs()
            })
            .fold(0., f64::max);
        self.subdivision_depth(twist * 512., PATCH_FLATNESS, fine)
    }

    fn component_depth(&self, ranges: &[(f32, f32)]) -> u32 {
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
            let min = v00.min(v10).min(v01).min(v11);
            let max = v00.max(v10).max(v01).max(v11);
            let patch_span = f64::from(max) - f64::from(min);
            if patch_span > 0. {
                relative_twist = relative_twist.max(delta / patch_span);
            }
        }
        let contour_ratio = if relative_twist > 0. {
            let b = self.bounds();
            let extent = b.width().hypot(b.height());
            (0.25 * extent * relative_twist) / COMPONENT_FLATNESS
        } else {
            0.
        };
        let color_ratio = (0.25 * twist / COMPONENT_TOLERANCE).max(contour_ratio);
        self.subdivision_depth(color_ratio, COMPONENT_FLATNESS, MAX_COMPONENT_DEPTH)
    }

    fn subdivision_depth(&self, color_ratio: f64, flatness: f64, max_depth: u32) -> u32 {
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
        if geometry <= flatness && color_ratio <= 1. {
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
        let ratio = ((pure + 2.25 * mixed) / flatness).max(color_ratio);
        (0.5 * ratio.max(1.).log2())
            .ceil()
            .min(f64::from(max_depth)) as u32
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

fn draw_patch<P: SamplePixel, const N: usize>(
    target: &mut Target<'_, '_, P>,
    patch: &CanvasPatch<'_>,
    depth: u32,
    colors: &impl MeshColors<N>,
) -> Result<(), Exhausted> {
    draw_patch_rows(target, patch, patch.points, [0., 1.], depth, depth, colors)
}

fn draw_patch_rows<P: SamplePixel, const N: usize>(
    target: &mut Target<'_, '_, P>,
    patch: &CanvasPatch<'_>,
    points: [[Point; 4]; 4],
    [v0, v1]: [f32; 2],
    remaining: u32,
    depth: u32,
    colors: &impl MeshColors<N>,
) -> Result<(), Exhausted> {
    take(target.meter, Cost::new(1))?;
    if !target.canvas.is_visible(points.as_flattened()) {
        return Ok(());
    }
    if remaining > 0 {
        let (lo, hi) = split_rows(points);
        let mid = (v0 + v1) * 0.5;
        draw_patch_rows(target, patch, lo, [v0, mid], remaining - 1, depth, colors)?;
        return draw_patch_rows(target, patch, hi, [mid, v1], remaining - 1, depth, colors);
    }
    let segments = 1_usize << depth;
    let mut previous = None;
    let mut buffer = [0.; MAX_COMPONENTS];
    let vertex_cost = target.vertex_cost;
    let mut column = |i: usize, meter: &mut dyn Meter| -> Result<[[f32; N]; 2], Exhausted> {
        let u = i as f32 / segments as f32;
        take(meter, vertex_cost.times(2))?;
        Ok([v0, v1].map(|v| colors.values(patch.components(u, v, &mut buffer))))
    };
    draw_patch_strip(
        target,
        points.map(|c| c[0]),
        points.map(|c| c[3]),
        0,
        segments,
        &mut |target, i, p, q| {
            let [c00, c01] = match previous {
                Some((last, values)) if last == i => values,
                _ => column(i, target.meter)?,
            };
            let right = column(i + 1, target.meter)?;
            let [c10, c11] = right;
            previous = Some((i + 1, right));
            let shade = |v: &[f32; N]| P::evaluated(Some(colors.shade(v)));
            target.canvas.fill_triangle(
                [p[0], p[1], q[0]],
                [c00, c10, c01],
                &shade,
                colors.cost(),
                target.meter,
            )?;
            target.canvas.fill_triangle(
                [p[1], q[1], q[0]],
                [c10, c11, c01],
                &shade,
                colors.cost(),
                target.meter,
            )
        },
    )
}

fn draw_patch_strip<P: SamplePixel>(
    target: &mut Target<'_, '_, P>,
    top: [Point; 4],
    bottom: [Point; 4],
    start: usize,
    width: usize,
    leaf: &mut impl FnMut(
        &mut Target<'_, '_, P>,
        usize,
        [Point; 2],
        [Point; 2],
    ) -> Result<(), Exhausted>,
) -> Result<(), Exhausted> {
    take(target.meter, Cost::new(1))?;
    let area = if width == 1 {
        bounds(&[top[0], top[3], bottom[0], bottom[3]])
    } else {
        bounds(&top).union(bounds(&bottom))
    };
    if !area.overlaps(target.canvas.bounds())
        || (target.pixel_centers
            && ((area.y0 - 0.5).ceil() >= (area.y1 - 0.5).ceil()
                || (area.x0 - 0.5).ceil() >= (area.x1 - 0.5).ceil()))
    {
        return Ok(());
    }
    if width == 1 {
        return leaf(target, start, [top[0], top[3]], [bottom[0], bottom[3]]);
    }
    let (a, b) = split_cubic(top);
    let (c, d) = split_cubic(bottom);
    let half = width / 2;
    draw_patch_strip(target, a, c, start, half, leaf)?;
    draw_patch_strip(target, b, d, start + half, half, leaf)
}

fn split_rows(points: [[Point; 4]; 4]) -> ([[Point; 4]; 4], [[Point; 4]; 4]) {
    let split = points.map(split_cubic);

    (split.map(|(low, _)| low), split.map(|(_, high)| high))
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
    use crate::encode::Unmetered;

    struct Gray;

    impl MeshColors<1> for Gray {
        fn values(&self, c: &[f32]) -> [f32; 1] {
            [c[0]]
        }
        fn shade(&self, v: &[f32; 1]) -> [f32; 4] {
            [v[0], v[0], v[0], 1.]
        }
    }

    #[test]
    fn folded_patch_coverage_matches_fine_subdivision() {
        let colors = [[0., 0.], [0., 0.]].map(|row| row.map(|v| smallvec::smallvec![v]));
        for degenerate in [false, true] {
            let points = std::array::from_fn(|i| {
                std::array::from_fn(|j| {
                    let interior = (1..3).contains(&i) && (1..3).contains(&j);
                    let y = if degenerate {
                        if interior { 0.52 } else { 0.5 }
                    } else {
                        (60 + j) as f64 / 128. + if interior { 1. / 16. } else { 0. }
                    };
                    Point::new(8. * i as f64, y)
                })
            });
            let patch = CanvasPatch {
                points,
                colors: &colors,
            };
            let draw = |depth| {
                let mut canvas = Canvas::new(24, 1, [0.; 4]);
                let mut target = Target {
                    canvas: &mut canvas,
                    meter: &mut Unmetered,
                    vertex_cost: Cost::new(1),
                    pixel_centers: true,
                };
                draw_patch(&mut target, &patch, depth, &Gray).unwrap();
                canvas.pixels
            };
            let fine = draw(4);
            assert!(fine.iter().any(|p| p[3] == 1.));
            assert_eq!(draw(patch.linear_depth(4)), fine);
        }
    }

    #[test]
    fn reversed_shared_edges_cover_every_pixel_center() {
        let grid = |x: f64, reverse: bool| {
            std::array::from_fn(|i| {
                std::array::from_fn(|j| {
                    Point::new(
                        x + i as f64 * 4.,
                        (if reverse { 3 - j } else { j }) as f64 * 4.,
                    )
                })
            })
        };
        let flat = [[0., 0.], [0., 0.]].map(|row| row.map(|v| smallvec::smallvec![v]));
        let varied = [[0., 1.], [1., 0.]].map(|row| row.map(|v| smallvec::smallvec![v]));
        let patches = [
            Patch {
                control_points: grid(0.5, false),
                colors: flat.clone(),
            },
            Patch {
                control_points: grid(12.5, true),
                colors: varied,
            },
            Patch {
                control_points: grid(28., false),
                colors: flat.clone(),
            },
        ];
        let mut canvas = Canvas::new(40, 12, [0.; 4]);
        let zoom = Affine::scale(4.);
        let depths = mesh_depths(
            &patches,
            zoom,
            zoom.transform_rect_bbox(canvas.bounds()),
            &[(0., 1.)],
            true,
            &mut Unmetered,
        )
        .unwrap();
        assert_eq!([depths.at(0), depths.at(1), depths.at(2)], [3, 3, 0]);
        let mut target = Target {
            canvas: &mut canvas,
            meter: &mut Unmetered,
            vertex_cost: Cost::new(1),
            pixel_centers: true,
        };
        Pieces::Patches(&patches, depths)
            .draw(&mut target, Affine::IDENTITY, &Gray)
            .unwrap();
        for row in canvas.pixels.as_chunks::<40>().0 {
            assert!(row[..24].iter().all(|p| p[3] == 1.));
            assert!(row[24..28].iter().all(|p| p[3] == 0.));
        }
    }

    #[test]
    fn out_of_range_and_unequal_patch_colors_retain_fine_depth() {
        let points = std::array::from_fn(|i| {
            std::array::from_fn(|j| Point::new(i as f64 * 8., j as f64 * 8.))
        });
        let clipped = [
            [smallvec::smallvec![0.], smallvec::smallvec![0.]],
            [smallvec::smallvec![4.], smallvec::smallvec![4.]],
        ];
        assert_eq!(
            CanvasPatch {
                points,
                colors: &clipped,
            }
            .linear_depth(2),
            2
        );
        let unequal = [
            [smallvec::smallvec![0., 0.], smallvec::smallvec![0.]],
            [smallvec::smallvec![0.], smallvec::smallvec![0.]],
        ];
        assert_eq!(
            CanvasPatch {
                points,
                colors: &unequal,
            }
            .linear_depth(2),
            0
        );
    }
}
