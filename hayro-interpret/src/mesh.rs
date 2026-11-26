use crate::canvas::{Canvas, bounds};
use crate::color::ColorComponents;
use crate::encode::{Cost, EncodedShadingPattern, Exhausted, Meter, take};
use crate::function::MAX_COMPONENTS;
use crate::gradient::{ColorLut, function_lut_metered};
use crate::shading::{Patch, ShadingFunction, ShadingType, Triangle};
use kurbo::{Affine, Point, Rect};

const MESH_SEGMENT: f64 = 8.0;
const MAX_DEPTH: u32 = 10;
const PATCH_FLATNESS: f64 = 0.125;
const MAX_PIECES: f64 = (1 << 20) as f64;

struct Target<'a> {
    canvas: &'a mut Canvas,
    meter: &'a mut dyn Meter,
    vertex_cost: Cost,
    pixel_centers: bool,
}

pub(crate) fn draw_mesh(
    canvas: &mut Canvas,
    shading_type: &ShadingType,
    transform: Affine,
    pattern: &EncodedShadingPattern,
    pixel_centers: bool,
    meter: &mut dyn Meter,
) -> Result<(), Exhausted> {
    let mut target = Target {
        canvas,
        meter,
        pixel_centers,
        vertex_cost: Cost::of(usize::from(pattern.color_space.num_components()) + 4)
            + pattern.color_space.prepared().sample,
    };
    match shading_type {
        ShadingType::TriangleMesh {
            triangles,
            function,
        } => {
            take(target.meter, Cost::of(triangles.len()).times(6))?;
            if !target.meter.hold(
                triangles
                    .len()
                    .saturating_mul(size_of::<CanvasTriangle<'_>>()),
            ) {
                return Err(Exhausted);
            }
            let triangles = triangles
                .iter()
                .map(|triangle| CanvasTriangle::new(triangle, transform))
                .collect::<Vec<_>>();

            if let Some(function) = function {
                let corner_colors = triangles.iter().flat_map(|t| t.colors);
                let colors = Parameter::new(function, corner_colors, pattern, target.meter)?;
                draw_triangles(&mut target, &triangles, 0, &colors)?;
                target.canvas.finish_parameter_mesh(colors.lut);
            } else {
                let colors = Direct { pattern };
                let depth = if colors.is_linear() {
                    0
                } else {
                    let pieces = triangles.iter().map(|t| (t.extent(), t.bounds()));
                    mesh_depth(target.canvas.bounds(), pieces, target.meter)?
                };

                draw_triangles(&mut target, &triangles, depth, &colors)?;
            }
        }
        ShadingType::PatchMesh { patches, function } => {
            take(target.meter, Cost::of(patches.len()).times(32))?;
            if !target
                .meter
                .hold(patches.len().saturating_mul(size_of::<CanvasPatch<'_>>()))
            {
                return Err(Exhausted);
            }
            let patches = patches
                .iter()
                .map(|patch| CanvasPatch::new(patch, transform))
                .collect::<Vec<_>>();
            let pieces = patches.iter().map(|p| (p.extent(), p.bounds()));
            let depth = mesh_depth(target.canvas.bounds(), pieces, target.meter)?;

            let direct = Direct { pattern };
            let depths = group_depths(
                &patches,
                target.canvas.bounds(),
                depth,
                direct.is_linear() && function.is_none(),
                target.meter,
            )?;
            if let Some(function) = function {
                let corner_colors = patches.iter().flat_map(|p| p.colors.as_flattened());
                let colors = Parameter::new(function, corner_colors, pattern, target.meter)?;
                draw_patches(&mut target, &patches, &depths, &colors)?;
                target.canvas.finish_parameter_mesh(colors.lut);
            } else {
                draw_patches(&mut target, &patches, &depths, &Direct { pattern })?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn mesh_depth(
    canvas: Rect,
    pieces: impl ExactSizeIterator<Item = (f64, Rect)>,
    meter: &mut dyn Meter,
) -> Result<u32, Exhausted> {
    take(meter, Cost::of(pieces.len()))?;
    let mut max_extent: f64 = 0.0;
    let mut visible_area = 0.0;

    for (extent, bounds) in pieces {
        if !bounds.overlaps(canvas) {
            continue;
        }

        let area = bounds.area();
        max_extent = max_extent.max(extent);
        visible_area += if area > 0.0 {
            bounds.intersect(canvas).area() / area
        } else {
            1.0
        };
    }

    let mut depth = (max_extent / MESH_SEGMENT)
        .log2()
        .ceil()
        .clamp(0.0, MAX_DEPTH as f64) as u32;

    while depth > 0 && visible_area * 4.0_f64.powi(depth as i32) > MAX_PIECES {
        depth -= 1;
    }

    Ok(depth)
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

fn group_depths(
    patches: &[CanvasPatch<'_>],
    canvas: Rect,
    fine: u32,
    linear: bool,
    meter: &mut dyn Meter,
) -> Result<Vec<u32>, Exhausted> {
    let n = patches.len();
    take(meter, Cost::of(n).times(128))?;
    if !meter.hold(n.saturating_mul(size_of::<u32>())) {
        return Err(Exhausted);
    }
    let mut depths = patches
        .iter()
        .map(|p| {
            if !p.bounds().overlaps(canvas) {
                0
            } else if linear {
                p.depth(fine)
            } else {
                fine
            }
        })
        .collect::<Vec<_>>();
    if n < 2 || depths.iter().all(|d| *d == depths[0]) {
        return Ok(depths);
    }
    crate::encode::scratch(meter, |meter| {
        let count = n.checked_mul(4).ok_or(Exhausted)?;
        let bytes = count
            .checked_mul(size_of::<(EdgeKey, usize)>())
            .and_then(|v| v.checked_add(n.checked_mul(size_of::<usize>())?))
            .ok_or(Exhausted)?;
        if !meter.hold(bytes) {
            return Err(Exhausted);
        }
        take(meter, crate::function::sorting(Cost::of(count)).times(8))?;
        let mut edges = Vec::with_capacity(count);
        let mut parent = (0..n).collect::<Vec<_>>();
        for (i, p) in patches.iter().enumerate() {
            let net = &p.points;
            for key in [net[0], net[3], net.map(|c| c[0]), net.map(|c| c[3])]
                .into_iter()
                .filter_map(edge_key)
            {
                edges.push((key, i));
            }
        }
        edges.sort_unstable();
        for pair in edges.windows(2) {
            if pair[0].0 == pair[1].0 {
                let a = root(&mut parent, pair[0].1, meter)?;
                let b = root(&mut parent, pair[1].1, meter)?;
                parent[a] = b;
                depths[b] = depths[b].max(depths[a]);
            }
        }
        for i in 0..n {
            let group = root(&mut parent, i, meter)?;
            depths[i] = depths[group];
        }
        Ok(())
    })?;
    Ok(depths)
}

trait MeshColors<const N: usize> {
    fn values(&self, components: &[f32]) -> [f32; N];
    fn invariant_column(&self, _: &[f32], _: &[f32], _: &[f32]) -> Option<[f32; N]> {
        None
    }
    fn shade(&self, values: &[f32; N]) -> [f32; 4];
}

struct Parameter {
    lut: ColorLut,
    t0: f32,
    scale: f32,
}

impl Parameter {
    fn new<'a>(
        function: &ShadingFunction,
        colors: impl Iterator<Item = &'a ColorComponents>,
        pattern: &EncodedShadingPattern,
        meter: &mut dyn Meter,
    ) -> Result<Self, Exhausted> {
        let (t0, t1) = colors
            .filter_map(|c| c.first().copied())
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), t| {
                (lo.min(t), hi.max(t))
            });
        let (t0, t1) = if t0 <= t1 { (t0, t1) } else { (0.0, 0.0) };

        Ok(Self {
            lut: function_lut_metered(function, [t0, t1], pattern, meter)?,
            t0,
            scale: if t1 > t0 { 1.0 / (t1 - t0) } else { 0.0 },
        })
    }
}

impl MeshColors<1> for Parameter {
    fn values(&self, components: &[f32]) -> [f32; 1] {
        [(components.first().copied().unwrap_or(0.0) - self.t0) * self.scale]
    }

    fn shade(&self, values: &[f32; 1]) -> [f32; 4] {
        self.lut.get(values[0])
    }
}

struct Direct<'a> {
    pattern: &'a EncodedShadingPattern,
}

impl Direct<'_> {
    fn is_linear(&self) -> bool {
        self.pattern.color_space.is_device_gray_or_rgb() && self.pattern.transfer_function.is_none()
    }
}

impl MeshColors<4> for Direct<'_> {
    fn values(&self, components: &[f32]) -> [f32; 4] {
        self.pattern.convert(components)
    }

    fn invariant_column(&self, values: &[f32], a: &[f32], b: &[f32]) -> Option<[f32; 4]> {
        let mut low = [0.; MAX_COMPONENTS];
        let mut high = low;
        for (i, &v) in values.iter().enumerate() {
            // Enclose both rounded four-term bilinear sums around the same exact column value.
            let error = 16. * f32::EPSILON * a[i].abs().max(b[i].abs()) + f32::MIN_POSITIVE;
            low[i] = v - error;
            high[i] = v + error;
        }
        let space = &self.pattern.color_space;
        (space.encode_values(&low[..values.len()]) == space.encode_values(&high[..values.len()]))
            .then(|| self.values(values))
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
        *out = colors
            .iter()
            .zip(weights)
            .map(|(color, weight)| color[i] * weight)
            .sum();
    }

    &out[..len]
}

struct CanvasTriangle<'a> {
    points: [Point; 3],
    colors: [&'a ColorComponents; 3],
}

impl<'a> CanvasTriangle<'a> {
    fn new(triangle: &'a Triangle, transform: Affine) -> Self {
        Self {
            points: triangle.vertices.each_ref().map(|v| transform * v.point),
            colors: triangle.vertices.each_ref().map(|v| &v.colors),
        }
    }

    fn extent(&self) -> f64 {
        let [a, b, c] = self.points;
        (a - b).hypot().max((b - c).hypot()).max((c - a).hypot())
    }

    fn bounds(&self) -> Rect {
        bounds(&self.points)
    }
}

fn draw_triangles<const N: usize>(
    target: &mut Target<'_>,
    triangles: &[CanvasTriangle<'_>],
    depth: u32,
    colors: &impl MeshColors<N>,
) -> Result<(), Exhausted> {
    const CORNERS: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

    for triangle in triangles {
        draw_triangle_piece(target, triangle, triangle.points, CORNERS, depth, colors)?;
    }
    Ok(())
}

fn draw_triangle_piece<const N: usize>(
    target: &mut Target<'_>,
    triangle: &CanvasTriangle<'_>,
    points: [Point; 3],
    weights: [[f32; 3]; 3],
    depth: u32,
    colors: &impl MeshColors<N>,
) -> Result<(), Exhausted> {
    take(target.meter, Cost::new(1))?;
    if !target.canvas.is_visible(&points) {
        return Ok(());
    }

    if depth == 0 {
        let mut buffer = [0.0; MAX_COMPONENTS];
        take(target.meter, target.vertex_cost.times(3))?;
        let values = weights.map(|w| colors.values(mix(triangle.colors, w, &mut buffer)));
        target
            .canvas
            .fill_triangle(points, values, &|v| colors.shade(v), target.meter)?;

        return Ok(());
    }

    let [p0, p1, p2] = points;
    let [w0, w1, w2] = weights;
    let (m01, m12, m20) = (p0.midpoint(p1), p1.midpoint(p2), p2.midpoint(p0));
    let mid = |a: [f32; 3], b: [f32; 3]| [0, 1, 2].map(|i| 0.5 * (a[i] + b[i]));
    let (n01, n12, n20) = (mid(w0, w1), mid(w1, w2), mid(w2, w0));

    for (points, weights) in [
        ([p0, m01, m20], [w0, n01, n20]),
        ([m01, p1, m12], [n01, w1, n12]),
        ([m20, m12, p2], [n20, n12, w2]),
        ([m01, m12, m20], [n01, n12, n20]),
    ] {
        draw_triangle_piece(target, triangle, points, weights, depth - 1, colors)?;
    }
    Ok(())
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

    fn depth(&self, fine: u32) -> u32 {
        let curved = |[a, b, c, d]: [Point; 4]| {
            let axis = d - a;
            let tolerance = f64::EPSILON * 16. * axis.hypot2();
            if axis.hypot2() == 0. {
                return a != b || a != c;
            }
            (b - a).cross(axis).abs() > tolerance
                || (c - a).cross(axis).abs() > tolerance
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
        // Retain the fine grid when intermediate vertex quantization introduces extra color steps.
        let divisions = f64::from(1_u32 << fine);
        for k in 0..c00.len() {
            let [a, b, c, d] = [c00, c01, c10, c11].map(|v| f64::from(v[k]) * 255.);
            if [b - a, c - a, d - c, d - b]
                .iter()
                .any(|v| (v / divisions).fract() != 0.)
            {
                return fine;
            }
        }
        let twist = c00
            .iter()
            .zip(c01)
            .zip(c10)
            .zip(c11)
            .map(|(((&a, &b), &c), &d)| {
                (f64::from(a) - f64::from(b) - f64::from(c) + f64::from(d)).abs()
            })
            .fold(0., f64::max);
        let color_ratio = twist * 512.;
        if geometry <= PATCH_FLATNESS && color_ratio <= 1. {
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
        let ratio = ((pure + 2.25 * mixed) / PATCH_FLATNESS).max(color_ratio);
        (0.5 * ratio.max(1.).log2()).ceil().min(f64::from(fine)) as u32
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

fn draw_patches<const N: usize>(
    target: &mut Target<'_>,
    patches: &[CanvasPatch<'_>],
    depths: &[u32],
    colors: &impl MeshColors<N>,
) -> Result<(), Exhausted> {
    for (patch, &depth) in patches.iter().zip(depths) {
        let samples = 1_usize << depth;
        let mut line = Vec::new();
        if depth > 0
            && patch.colors.iter().all(|pair| pair[0] == pair[1])
            && patch.bounds().overlaps(target.canvas.bounds())
        {
            let bytes = (samples + 1) * size_of::<Option<[f32; N]>>();
            if !target.meter.hold(bytes) {
                return Err(Exhausted);
            }
            take(
                target.meter,
                target.vertex_cost.times(4 * (samples + 1) as u64),
            )?;
            line.reserve_exact(samples + 1);
            let mut buffer = [0.; MAX_COMPONENTS];
            for i in 0..=samples {
                let values = patch.components(i as f32 / samples as f32, 0., &mut buffer);
                line.push(colors.invariant_column(
                    values,
                    &patch.colors[0][0],
                    &patch.colors[1][0],
                ));
            }
        }
        draw_patch_rows(
            target,
            patch,
            patch.points,
            [0., 1.],
            depth,
            depth,
            colors,
            &line,
        )?;
        target
            .meter
            .release_bytes(line.capacity() * size_of::<Option<[f32; N]>>());
    }
    Ok(())
}

fn draw_patch_rows<const N: usize>(
    target: &mut Target<'_>,
    patch: &CanvasPatch<'_>,
    points: [[Point; 4]; 4],
    [v0, v1]: [f32; 2],
    remaining: u32,
    depth: u32,
    colors: &impl MeshColors<N>,
    line: &[Option<[f32; N]>],
) -> Result<(), Exhausted> {
    take(target.meter, Cost::new(1))?;
    if !target.canvas.is_visible(points.as_flattened()) {
        return Ok(());
    }
    if remaining > 0 {
        let (lo, hi) = split_rows(points);
        let mid = (v0 + v1) * 0.5;
        draw_patch_rows(
            target,
            patch,
            lo,
            [v0, mid],
            remaining - 1,
            depth,
            colors,
            line,
        )?;
        return draw_patch_rows(
            target,
            patch,
            hi,
            [mid, v1],
            remaining - 1,
            depth,
            colors,
            line,
        );
    }
    let segments = 1_usize << depth;
    let mut previous = None;
    let mut buffer = [0.; MAX_COMPONENTS];
    let vertex_cost = target.vertex_cost;
    let mut column = |i: usize, meter: &mut dyn Meter| -> Result<[[f32; N]; 2], Exhausted> {
        if let Some(Some(value)) = line.get(i) {
            return Ok([*value; 2]);
        }
        take(meter, vertex_cost.times(2))?;
        let u = i as f32 / segments as f32;
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
            let shade = |v: &[f32; N]| colors.shade(v);
            target.canvas.fill_triangle(
                [p[0], p[1], q[0]],
                [c00, c10, c01],
                &shade,
                target.meter,
            )?;
            target
                .canvas
                .fill_triangle([p[1], q[1], q[0]], [c10, c11, c01], &shade, target.meter)
        },
    )
}

fn draw_patch_strip(
    target: &mut Target<'_>,
    top: [Point; 4],
    bottom: [Point; 4],
    start: usize,
    width: usize,
    leaf: &mut impl FnMut(&mut Target<'_>, usize, [Point; 2], [Point; 2]) -> Result<(), Exhausted>,
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
            CanvasPatch {
                points: grid(0.5, false),
                colors: &flat,
            },
            CanvasPatch {
                points: grid(12.5, true),
                colors: &varied,
            },
            CanvasPatch {
                points: grid(28., false),
                colors: &flat,
            },
        ];
        let mut canvas = Canvas::new(40, 12, [0.; 4]);
        let depths = group_depths(&patches, canvas.bounds(), 3, true, &mut Unmetered).unwrap();
        assert_eq!(depths, [3, 3, 0]);
        let mut target = Target {
            canvas: &mut canvas,
            meter: &mut Unmetered,
            vertex_cost: Cost::new(1),
            pixel_centers: true,
        };
        draw_patches(&mut target, &patches, &depths, &Gray).unwrap();
        for row in canvas.pixels.as_chunks::<40>().0 {
            assert!(row[..24].iter().all(|p| p[3] == 1.));
            assert!(row[24..28].iter().all(|p| p[3] == 0.));
        }
    }
}
