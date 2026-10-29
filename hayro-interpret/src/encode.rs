//! Encoding shading patterns for easy sampling.

use crate::color::{AlphaColor, ColorComponents, ColorSpace};
use crate::interpret::state::ActiveTransferFunction;
use crate::pattern::ShadingPattern;
use crate::shading::{PATCH_TRIANGLES, ShadingFunction, ShadingType, Triangle};
use crate::util::x_y_advances;
use hayro_syntax::hash;
use kurbo::{Affine, Point, Rect, Shape, Vec2};
use smallvec::{SmallVec, ToSmallVec, smallvec};
use std::ops::Range;
use std::sync::Arc;

/// A shading pattern that was encoded so it can be sampled.
#[derive(Clone, Debug)]
pub struct EncodedShadingPattern {
    /// The base transform of the shading pattern.
    pub base_transform: Affine,
    pub(crate) color_space: ColorSpace,
    pub(crate) background_color: AlphaColor,
    /// The encoded shading type.
    pub shading_type: EncodedShadingType,
    pub(crate) opacity: f32,
    pub(crate) transfer_function: Option<ActiveTransferFunction>,
}

/// The encoded shading type.
#[derive(Clone, Debug)]
pub enum EncodedShadingType {
    /// Function-based shading.
    FunctionBased(EncodedFunctionBasedShading),
    /// Axial or radial shading.
    RadialAxial(EncodedRadialAxialShading),
    /// Mesh-based shading sampled into device pixels.
    Sampled(EncodedSampledShading),
    /// Empty dummy shading.
    Dummy(EncodedDummyShading),
}

/// Encoded function-based shading.
#[derive(Clone, Debug)]
pub struct EncodedFunctionBasedShading {
    pub(crate) domain: Rect,
    pub(crate) function: ShadingFunction,
}

/// Encoded axial or radial shading.
#[derive(Clone, Debug)]
pub struct EncodedRadialAxialShading {
    pub(crate) function: ShadingFunction,
    pub(crate) params: RadialAxialParams,
    pub(crate) domain: [f32; 2],
    pub(crate) extend: [bool; 2],
}

/// Encoded mesh shading, sampled at the device pixels a texture reads.
#[derive(Clone, Debug)]
pub struct EncodedSampledShading {
    /// A triangle or patch mesh.
    pub(crate) mesh: Arc<ShadingType>,
    /// Places the mesh in device space.
    pub(crate) transform: Affine,
    pub(crate) function: Option<ShadingFunction>,
}

impl EncodedSampledShading {
    /// The colors of the pixels in `region` the mesh covers.
    fn sample(&self, region: &Region) -> Samples {
        let mut patched = vec![];
        let triangles = match self.mesh.as_ref() {
            ShadingType::TriangleMesh { triangles, .. } => triangles,
            ShadingType::CoonsPatchMesh { patches, .. } => {
                patched.reserve_exact(patches.len() * PATCH_TRIANGLES);
                for patch in patches {
                    patch.to_triangles(&mut patched);
                }
                &patched
            }
            ShadingType::TensorProductPatchMesh { patches, .. } => {
                patched.reserve_exact(patches.len() * PATCH_TRIANGLES);
                for patch in patches {
                    patch.to_triangles(&mut patched);
                }
                &patched
            }
            _ => &patched,
        };
        sample_triangles(triangles, self.transform, region)
    }
}

/// Device pixels, `xs` by `ys`.
type Region = (Range<u16>, Range<u16>);

/// The box of the device pixels points lie in, `x0`, `x1`, `y0`, `y1`, each
/// inclusive.
struct Reach(Option<(u16, u16, u16, u16)>);

impl Reach {
    #[inline]
    fn add(&mut self, pos: Point) {
        let (x, y) = pixel(pos);
        let (x0, x1, y0, y1) = self.0.unwrap_or((x, x, y, y));
        self.0 = Some((x0.min(x), x1.max(x), y0.min(y), y1.max(y)));
    }
}

/// The device pixel a sampled mesh is looked up at for `pos`.
fn pixel(pos: Point) -> (u16, u16) {
    (pos.x as u16, pos.y as u16)
}

/// The colors of a mesh shading at the device pixels of a region its triangles
/// cover, a later triangle's where two cover one.
#[derive(Debug)]
pub(crate) enum Samples {
    /// Every pixel of the box from `x0`, `y0` that holds the covered ones, `width` to
    /// a row: looked up without hashing, in the order it is drawn in.
    Grid {
        x0: u16,
        y0: u16,
        width: usize,
        cells: Vec<Option<ColorComponents>>,
    },
    /// The covered pixels, where they fill too little of the box that holds them.
    Map(hash::Map<(u16, u16), ColorComponents>),
}

impl Samples {
    fn get(&self, (x, y): (u16, u16)) -> Option<&ColorComponents> {
        match self {
            Self::Grid {
                x0,
                y0,
                width,
                cells,
            } => {
                let (x, y) = (x.checked_sub(*x0)?, y.checked_sub(*y0)?);
                if usize::from(x) >= *width {
                    return None;
                }
                cells.get(usize::from(y) * width + usize::from(x))?.as_ref()
            }
            Self::Map(map) => map.get(&(x, y)),
        }
    }
}

/// Encoded dummy shading.
#[derive(Clone, Debug)]
pub struct EncodedDummyShading;

/// A shading that was rasterized into a texture.
pub struct ShadingTexture {
    /// The straight (non-premultiplied) RGBA8 pixel data, row-major.
    pub data: Vec<u8>,
    /// The width of the texture in pixels.
    pub width: u32,
    /// The height of the texture in pixels.
    pub height: u32,
    /// The transform from texture space to the space of the rasterized
    /// bounding box.
    pub transform: Affine,
    /// Whether any pixel of the texture is not fully opaque.
    pub has_transparency: bool,
}

/// Return the texture dimensions [`EncodedShadingPattern::sample_texture`]
/// uses for the given bounding box and scale factor.
///
/// Exposed so that callers can bound the size of the resulting texture
/// before sampling.
pub fn texture_dimensions(bbox: Rect, scale: f32) -> (u32, u32) {
    (
        (bbox.width() as f32 * scale).max(1.0).ceil() as u32,
        (bbox.height() as f32 * scale).max(1.0).ceil() as u32,
    )
}

impl EncodedShadingPattern {
    /// Sample the shading at the given position.
    ///
    /// A mesh shading places all its triangles for each call;
    /// [`sample_texture`](Self::sample_texture) places them once for all its points.
    pub fn sample(&self, pos: Point) -> [f32; 4] {
        let samples = self.samples_at(|pixels| pixels.add(pos));
        self.sample_in(pos, samples.as_ref())
    }

    /// For a mesh shading, its samples at every pixel `walk` adds a point in.
    fn samples_at(&self, walk: impl FnOnce(&mut Reach)) -> Option<Samples> {
        let EncodedShadingType::Sampled(mesh) = &self.shading_type else {
            return None;
        };
        let mut reach = Reach(None);
        walk(&mut reach);
        // The last pixel of a row or column, 65535, is never a triangle's.
        let region = reach.0.map_or((0..0, 0..0), |(x0, x1, y0, y1)| {
            (x0..x1.saturating_add(1), y0..y1.saturating_add(1))
        });
        Some(mesh.sample(&region))
    }

    /// The shading at `pos`, a mesh's looked up in `samples`.
    #[inline]
    fn sample_in(&self, pos: Point, samples: Option<&Samples>) -> [f32; 4] {
        self.shading_type
            .eval(pos, self.background_color, &self.color_space, samples)
            .map(|v| {
                let mut components = v.components();
                components[3] *= self.opacity;

                if let Some(tf) = &self.transfer_function {
                    return tf.apply(&AlphaColor::new(components)).components();
                }

                components
            })
            .unwrap_or([0.0, 0.0, 0.0, 0.0])
    }

    /// Sample the shading on a pixel grid covering `bbox`, with `scale`
    /// pixels per unit of `bbox`.
    ///
    /// The callback is invoked once per pixel in row-major order with the
    /// straight (non-premultiplied) RGBA sample, letting callers apply their
    /// own conversion. Returns the dimensions of the grid (see
    /// [`texture_dimensions`]) and the transform from grid space to the space
    /// of `bbox`. No size cap is applied — callers that need to bound the
    /// texture size should check [`texture_dimensions`] first.
    pub fn sample_texture(
        &self,
        bbox: Rect,
        scale: f32,
        mut f: impl FnMut([f32; 4]),
    ) -> (u32, u32, Affine) {
        // A mesh is sampled only at the pixels the texture reads.
        let samples = self.samples_at(|reach| {
            let (start, x_advance, y_advance, width, height) = self.texture_grid(bbox, scale);
            if x_advance.y == 0.0 && y_advance.x == 0.0 {
                // A row's points differ only across, a column's only up.
                let mut point = start;
                for _ in 0..width {
                    reach.add(point);
                    point += x_advance;
                }
                let mut point = start;
                for _ in 0..height {
                    reach.add(point);
                    point += y_advance;
                }
            } else {
                self.texture_points(bbox, scale, |point| reach.add(point));
            }
        });
        self.texture_points(bbox, scale, |point| {
            f(self.sample_in(point, samples.as_ref()));
        });

        let (width, height) = texture_dimensions(bbox, scale);
        let inv_scale = 1.0 / scale as f64;
        (
            width,
            height,
            Affine::translate((bbox.x0, bbox.y0)) * Affine::scale(inv_scale),
        )
    }

    /// The first point [`sample_texture`](Self::sample_texture) samples, the steps to
    /// the next across and the next row, and how many across and rows.
    fn texture_grid(&self, bbox: Rect, scale: f32) -> (Point, Vec2, Vec2, u32, u32) {
        let (width, height) = texture_dimensions(bbox, scale);
        let inv_scale = 1.0 / scale as f64;

        let (x_advance, y_advance) =
            x_y_advances(&(Affine::scale(inv_scale) * self.base_transform));

        let start_point = self.base_transform
            * Affine::translate((0.5 * inv_scale, 0.5 * inv_scale))
            * Point::new(bbox.x0, bbox.y0);
        (start_point, x_advance, y_advance, width, height)
    }

    /// The points [`sample_texture`](Self::sample_texture) samples, in its order.
    fn texture_points(&self, bbox: Rect, scale: f32, mut f: impl FnMut(Point)) {
        let (mut start_point, x_advance, y_advance, width, height) = self.texture_grid(bbox, scale);

        for _ in 0..height {
            let mut point = start_point;

            for _ in 0..width {
                f(point);

                point += x_advance;
            }

            start_point += y_advance;
        }
    }

    /// Rasterize the shading into a straight (non-premultiplied) RGBA8
    /// texture covering `bbox`, with `scale` pixels per unit of `bbox`.
    ///
    /// See [`sample_texture`](Self::sample_texture) for the grid semantics.
    pub fn render_texture(&self, bbox: Rect, scale: f32) -> ShadingTexture {
        let (width, height) = texture_dimensions(bbox, scale);
        let mut data = Vec::with_capacity(width as usize * height as usize * 4);
        let mut has_transparency = false;

        let (_, _, transform) = self.sample_texture(bbox, scale, |sample| {
            let converted = [
                (sample[0] * 255.0 + 0.5) as u8,
                (sample[1] * 255.0 + 0.5) as u8,
                (sample[2] * 255.0 + 0.5) as u8,
                (sample[3] * 255.0 + 0.5) as u8,
            ];

            has_transparency |= converted[3] != 255;
            data.extend_from_slice(&converted);
        });

        ShadingTexture {
            data,
            width,
            height,
            transform,
            has_transparency,
        }
    }
}

impl ShadingPattern {
    /// Encode the shading pattern.
    pub fn encode(&self) -> EncodedShadingPattern {
        let base_transform;

        let shading_type = match self.shading.shading_type.as_ref() {
            ShadingType::FunctionBased {
                domain,
                matrix,
                function,
            } => {
                base_transform = (self.matrix * *matrix).inverse();
                encode_function_shading(domain, function)
            }
            ShadingType::RadialAxial {
                coords,
                domain,
                function,
                extend,
                axial,
            } => {
                let (encoded, initial_transform) =
                    encode_axial_shading(*coords, *domain, function, *extend, *axial);

                base_transform = initial_transform * self.matrix.inverse();

                encoded
            }
            ShadingType::TriangleMesh { function, .. }
            | ShadingType::CoonsPatchMesh { function, .. }
            | ShadingType::TensorProductPatchMesh { function, .. } => {
                base_transform = Affine::IDENTITY;

                EncodedShadingType::Sampled(EncodedSampledShading {
                    mesh: self.shading.shading_type.clone(),
                    transform: self.matrix,
                    function: function.clone(),
                })
            }
            ShadingType::Dummy => {
                base_transform = Affine::IDENTITY;

                EncodedShadingType::Dummy(EncodedDummyShading)
            }
        };

        let color_space = self.shading.color_space.clone();

        let background_color = self
            .shading
            .background
            .as_ref()
            .map(|b| color_space.to_rgba(b, 1.0))
            .unwrap_or(AlphaColor::TRANSPARENT);

        EncodedShadingPattern {
            color_space,
            background_color,
            shading_type,
            base_transform,
            opacity: self.opacity,
            transfer_function: self.transfer_function.clone(),
        }
    }
}

fn encode_axial_shading(
    coords: [f32; 6],
    domain: [f32; 2],
    function: &ShadingFunction,
    extend: [bool; 2],
    is_axial: bool,
) -> (EncodedShadingType, Affine) {
    let initial_transform;

    let params = if is_axial {
        let [x_0, y_0, x_1, y_1, _, _] = coords;

        initial_transform = ts_from_line_to_line(
            Point::new(x_0 as f64, y_0 as f64),
            Point::new(x_1 as f64, y_1 as f64),
            Point::ZERO,
            Point::new(1.0, 0.0),
        );

        RadialAxialParams::Axial
    } else {
        let [x_0, y_0, r0, x_1, y_1, r_1] = coords;

        initial_transform = Affine::translate((-x_0 as f64, -y_0 as f64));
        let new_x1 = x_1 - x_0;
        let new_y1 = y_1 - y_0;

        let p1 = Point::new(new_x1 as f64, new_y1 as f64);
        let r = Point::new(r0 as f64, r_1 as f64);

        RadialAxialParams::Radial { p1, r }
    };

    (
        EncodedShadingType::RadialAxial(EncodedRadialAxialShading {
            function: function.clone(),
            params,
            domain,
            extend,
        }),
        initial_transform,
    )
}

fn sample_triangles(triangles: &[Triangle], transform: Affine, region: &Region) -> Samples {
    // A grid of the box that holds the pixels sampling may put takes at most twice
    // the cells of the samples it may put, and no more than the region.
    let mut most = 0;
    let boxes: Vec<_> = triangles
        .iter()
        .map(|t| {
            let (xs, ys, covered) = pixels(t, transform, region)?;
            most += covered;
            Some((xs, ys))
        })
        .collect();
    let mut bounds: Option<(u16, u16, u16, u16)> = None;
    for (xs, ys) in boxes.iter().flatten() {
        let (x0, x1, y0, y1) = bounds.unwrap_or((xs.start, xs.end, ys.start, ys.end));
        bounds = Some((
            x0.min(xs.start),
            x1.max(xs.end),
            y0.min(ys.start),
            y1.max(ys.end),
        ));
    }
    let grid =
        bounds.filter(|(x0, x1, y0, y1)| usize::from(x1 - x0) * usize::from(y1 - y0) <= 2 * most);

    match grid {
        Some((x0, x1, y0, y1)) => {
            let width = usize::from(x1 - x0);
            let mut cells = vec![None; width * usize::from(y1 - y0)];
            sample(triangles, &boxes, transform, |x, y, color| {
                cells[usize::from(y - y0) * width + usize::from(x - x0)] = Some(color);
            });
            Samples::Grid {
                x0,
                y0,
                width,
                cells,
            }
        }
        None => {
            let mut map = hash::Map::default();
            sample(triangles, &boxes, transform, |x, y, color| {
                map.insert((x, y), color);
            });
            Samples::Map(map)
        }
    }
}

/// The most pixels of a triangle's box counted as covered without a bound on those
/// it covers: no more than a grid takes for each triangle's own data.
const SMALL: usize = 64;

/// The device pixels of the box of `t` placed by `transform` in `region`, where it
/// has any, and how many of them it may cover.
fn pixels(
    t: &Triangle,
    transform: Affine,
    (xs, ys): &Region,
) -> Option<(Range<u16>, Range<u16>, usize)> {
    let placed = kurbo::Triangle::new(
        transform * t.p0.point,
        transform * t.p1.point,
        transform * t.p2.point,
    );
    let bbox = placed.bounding_box();
    let (x, y) = (span(bbox.x0, bbox.x1), span(bbox.y0, bbox.y1));
    let xs = x.start.max(xs.start)..x.end.min(xs.end);
    let ys = y.start.max(ys.start)..y.end.min(ys.end);
    if xs.is_empty() || ys.is_empty() {
        return None;
    }
    let cells = xs.len() * ys.len();
    // A small box is counted whole: it is read whole anyway.
    if cells <= SMALL {
        return Some((xs, ys, cells));
    }
    // A convex shape of area A and perimeter P holds at most A + P / 2 + 1 points
    // of the integer grid (Nosarzewska), and P is at most that of its box: here of
    // the triangle, cut by the region's pixels where it reaches past them.
    let (area, extent) = if (&xs, &ys) == (&x, &y) {
        (placed.area().abs(), bbox)
    } else {
        let cut = Rect::new(
            f64::from(xs.start),
            f64::from(ys.start),
            f64::from(xs.end - 1),
            f64::from(ys.end - 1),
        );
        cut_by(&[placed.a, placed.b, placed.c], cut)
    };
    let lattice = area + extent.width() + extent.height() + 1.0;
    let covered = if lattice < cells as f64 {
        lattice as usize + 1
    } else {
        cells
    };
    Some((xs, ys, covered))
}

/// The area and the box of the convex polygon `points` cut by `rect`.
fn cut_by(points: &[Point], rect: Rect) -> (f64, Rect) {
    let mut polygon: SmallVec<[Point; 8]> = points.into();
    // Each side of `rect`: a point `p` lies inside it where `n · p >= at`.
    let sides = [
        (Vec2::new(1.0, 0.0), rect.x0),
        (Vec2::new(-1.0, 0.0), -rect.x1),
        (Vec2::new(0.0, 1.0), rect.y0),
        (Vec2::new(0.0, -1.0), -rect.y1),
    ];
    for (n, at) in sides {
        let mut cut = SmallVec::<[Point; 8]>::new();
        for (i, &p) in polygon.iter().enumerate() {
            let q = polygon[(i + 1) % polygon.len()];
            let (a, b) = (n.dot(p.to_vec2()) - at, n.dot(q.to_vec2()) - at);
            if a >= 0.0 {
                cut.push(p);
            }
            if (a >= 0.0) != (b >= 0.0) {
                cut.push(p.lerp(q, a / (a - b)));
            }
        }
        polygon = cut;
        if polygon.is_empty() {
            return (0.0, Rect::ZERO);
        }
    }
    let mut area = 0.0;
    let mut extent = Rect::from_points(polygon[0], polygon[0]);
    for (i, &p) in polygon.iter().enumerate() {
        area += p
            .to_vec2()
            .cross(polygon[(i + 1) % polygon.len()].to_vec2());
        extent = extent.union_pt(p);
    }
    (area.abs() / 2.0, extent)
}

/// `lo.floor() as u16..hi.ceil() as u16`, without the calls `floor` and `ceil` are
/// on targets without SSE4.1: a float converts to `u16` rounded towards zero and
/// saturated, which is its floor for any `lo` that converts to more than 0.
fn span(lo: f64, hi: f64) -> Range<u16> {
    let end = hi as u16;
    let end = if f64::from(end) < hi {
        end.saturating_add(1)
    } else {
        end
    };
    (lo as u16)..end
}

/// Puts the color of each pixel of each triangle's box, `boxes` in the order of the
/// triangles, that the triangle, placed by `transform`, covers, in that order.
fn sample(
    triangles: &[Triangle],
    boxes: &[Option<(Range<u16>, Range<u16>)>],
    transform: Affine,
    mut put: impl FnMut(u16, u16, ColorComponents),
) {
    for (t, pixels) in triangles.iter().zip(boxes) {
        let Some((xs, ys)) = pixels else {
            continue;
        };
        let t = {
            let mut v0 = t.p0.clone();
            v0.point = transform * t.p0.point;
            let mut v1 = t.p1.clone();
            v1.point = transform * t.p1.point;
            let mut v2 = t.p2.clone();
            v2.point = transform * t.p2.point;
            Triangle::new(v0, v1, v2)
        };
        for y in ys.clone() {
            for x in xs.clone() {
                let point = Point::new(x as f64, y as f64);
                if t.contains_point(point) {
                    put(x, y, t.interpolate(point));
                }
            }
        }
    }
}

fn encode_function_shading(domain: &[f32; 4], function: &ShadingFunction) -> EncodedShadingType {
    let domain = Rect::new(
        domain[0] as f64,
        domain[2] as f64,
        domain[1] as f64,
        domain[3] as f64,
    );

    EncodedShadingType::FunctionBased(EncodedFunctionBasedShading {
        domain,
        function: function.clone(),
    })
}

#[derive(Clone, Debug)]
pub(crate) enum RadialAxialParams {
    Axial,
    Radial { p1: Point, r: Point },
}

impl EncodedShadingType {
    pub(crate) fn eval(
        &self,
        pos: Point,
        bg_color: AlphaColor,
        color_space: &ColorSpace,
        samples: Option<&Samples>,
    ) -> Option<AlphaColor> {
        match self {
            Self::FunctionBased(EncodedFunctionBasedShading { domain, function }) => {
                if !domain.contains(pos) {
                    Some(bg_color)
                } else {
                    let out = function.eval(&smallvec![pos.x as f32, pos.y as f32])?;
                    // TODO: Clamp out-of-range values.
                    Some(color_space.to_rgba(&out, 1.0))
                }
            }
            Self::RadialAxial(EncodedRadialAxialShading {
                function,
                params,
                domain,
                extend,
            }) => {
                let (t0, t1) = (domain[0], domain[1]);

                let mut t = match params {
                    RadialAxialParams::Axial => pos.x as f32,
                    RadialAxialParams::Radial { p1, r } => {
                        radial_pos(&pos, p1, *r, extend[0], extend[1]).unwrap_or(f32::MIN)
                    }
                };

                if t == f32::MIN {
                    return Some(bg_color);
                }

                if t < 0.0 {
                    if extend[0] {
                        t = 0.0;
                    } else {
                        return Some(bg_color);
                    }
                } else if t > 1.0 {
                    if extend[1] {
                        t = 1.0;
                    } else {
                        return Some(bg_color);
                    }
                }

                let t = t0 + (t1 - t0) * t;

                let val = function.eval(&smallvec![t])?;

                Some(color_space.to_rgba(&val, 1.0))
            }
            Self::Sampled(EncodedSampledShading { function, .. }) => {
                if let Some(color) = samples.and_then(|samples| samples.get(pixel(pos))) {
                    if let Some(function) = function {
                        let val = function.eval(&color.to_smallvec())?;
                        Some(color_space.to_rgba(&val, 1.0))
                    } else {
                        Some(color_space.to_rgba(color, 1.0))
                    }
                } else {
                    Some(bg_color)
                }
            }
            Self::Dummy(_) => Some(AlphaColor::TRANSPARENT),
        }
    }
}

fn ts_from_line_to_line(src1: Point, src2: Point, dst1: Point, dst2: Point) -> Affine {
    let unit_to_line1 = unit_to_line(src1, src2);
    let line1_to_unit = unit_to_line1.inverse();
    let unit_to_line2 = unit_to_line(dst1, dst2);

    unit_to_line2 * line1_to_unit
}

fn unit_to_line(p0: Point, p1: Point) -> Affine {
    Affine::new([
        p1.y - p0.y,
        p0.x - p1.x,
        p1.x - p0.x,
        p1.y - p0.y,
        p0.x,
        p0.y,
    ])
}

fn radial_pos(
    pos: &Point,
    p1: &Point,
    r: Point,
    min_extend: bool,
    max_extend: bool,
) -> Option<f32> {
    let r0 = r.x as f32;
    let dx = p1.x as f32;
    let dy = p1.y as f32;
    let dr = r.y as f32 - r0;

    let px = pos.x as f32;
    let py = pos.y as f32;

    let a = dx * dx + dy * dy - dr * dr;
    let b = -2.0 * (px * dx + py * dy + r0 * dr);
    let c = px * px + py * py - r0 * r0;

    let discriminant = b * b - 4.0 * a * c;

    // No solution available.
    if discriminant < 0.0 {
        return None;
    }

    if a.abs() < 1e-6 {
        if b.abs() < 1e-6 {
            return None;
        }

        let t = -c / b;

        if (!min_extend && t < 0.0) || (!max_extend && t > 1.0) {
            return None;
        }

        let r_t = r0 + dr * t;
        if r_t < 0.0 {
            return None;
        }

        return Some(t);
    }

    let sqrt_d = discriminant.sqrt();
    let t1 = (-b - sqrt_d) / (2.0 * a);
    let t2 = (-b + sqrt_d) / (2.0 * a);

    let max = t1.max(t2);
    let mut take_max = Some(max);
    let min = t1.min(t2);
    let mut take_min = Some(min);

    if (!min_extend && min < 0.0) || r0 + dr * min < 0.0 {
        take_min = None;
    }

    if (!max_extend && max > 1.0) || r0 + dr * max < 0.0 {
        take_max = None;
    }

    match (take_min, take_max) {
        (Some(_), Some(max)) => Some(max),
        (Some(min), None) => Some(min),
        (None, Some(max)) => Some(max),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::span;
    use super::{Region, Samples, cut_by, sample_triangles};
    use crate::shading::{Triangle, TriangleVertex};
    use hayro_syntax::hash;
    use kurbo::{Affine, Point, Rect};
    use smallvec::smallvec;

    struct Random(u64);

    impl Random {
        fn below(&mut self, n: f64) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % 1_000_000) as f64 / 1_000_000.0 * n
        }
    }

    fn triangle(points: [(f64, f64); 3], color: f32) -> Triangle {
        let [a, b, c] = points.map(|(x, y)| {
            TriangleVertex::at(Point::new(x, y), smallvec![color, color / 2.0, 1.0 - color])
        });
        Triangle::new(a, b, c)
    }

    /// Whether the samples in a region are what putting each covered pixel of each
    /// triangle into a map, the later over the earlier, finds, at every pixel of a
    /// window around the region.
    fn check(triangles: &[Triangle], transform: Affine) -> Samples {
        check_in(triangles, transform, &(0..400, 0..400), (0..400, 0..400))
    }

    fn check_in(
        triangles: &[Triangle],
        transform: Affine,
        region: &Region,
        (columns, rows): Region,
    ) -> Samples {
        let samples = sample_triangles(triangles, transform, region);
        let mut expected = hash::Map::default();
        for t in triangles {
            let [p0, p1, p2] = [&t.p0, &t.p1, &t.p2].map(|v| {
                let mut v = (*v).clone();
                v.point = transform * v.point;
                v
            });
            let t = Triangle::new(p0, p1, p2);
            let bbox = t.bounding_box();
            // The pixels of the box, as many as lie in the region.
            let (x0, x1) = (bbox.x0.floor() as u16, bbox.x1.ceil() as u16);
            let (y0, y1) = (bbox.y0.floor() as u16, bbox.y1.ceil() as u16);
            let xs = x0.max(region.0.start)..x1.min(region.0.end);
            for y in y0.max(region.1.start)..y1.min(region.1.end) {
                for x in xs.clone() {
                    let point = Point::new(x as f64, y as f64);
                    if t.contains_point(point) {
                        expected.insert((x, y), t.interpolate(point));
                    }
                }
            }
        }
        for y in rows {
            for x in columns.clone() {
                assert_eq!(samples.get((x, y)), expected.get(&(x, y)), "({x}, {y})");
            }
        }
        samples
    }

    /// The cells the samples hold.
    fn cells(samples: &Samples) -> usize {
        match samples {
            Samples::Grid { cells, .. } => cells.len(),
            Samples::Map(map) => map.len(),
        }
    }

    #[test]
    fn a_thin_triangle_is_sampled_in_cells_as_few_as_its_pixels() {
        // A diagonal sliver whose box grows with it, sampled in a region of 400 by
        // 300 pixels that cuts it, and in one that holds it.
        for size in [100.0, 1000.0, 10000.0, 65534.0] {
            let sliver = [triangle(
                [(0.0, 0.0), (size, size), (size, size - 0.01)],
                0.5,
            )];
            let samples = check_in(
                &sliver,
                Affine::IDENTITY,
                &(50..450, 20..320),
                (0..500, 0..400),
            );
            assert!(cells(&samples) <= 300, "{size}: {}", cells(&samples));
            if size <= 1000.0 {
                let end = size as u16 + 1;
                let samples = check_in(
                    &sliver,
                    Affine::IDENTITY,
                    &(0..end, 0..end),
                    (0..end, 0..end),
                );
                assert!(matches!(samples, Samples::Map(_)), "{size}");
                assert!(
                    cells(&samples) <= 2 * end as usize,
                    "{size}: {}",
                    cells(&samples)
                );
            }
        }
        // Many in one region, where they tile it, are a grid of it.
        let mut fan = vec![];
        for i in 0..100 {
            let x = i as f64 * 4.0;
            fan.push(triangle([(x, 0.0), (x + 4.0, 0.0), (x, 300.0)], 0.25));
            fan.push(triangle(
                [(x + 4.0, 0.0), (x + 4.0, 300.0), (x, 300.0)],
                0.75,
            ));
        }
        let samples = check_in(&fan, Affine::IDENTITY, &(0..400, 0..300), (0..450, 0..350));
        assert!(matches!(samples, Samples::Grid { .. }));
        assert!(cells(&samples) <= 401 * 301);
    }

    #[test]
    fn a_mesh_is_sampled_as_a_map_of_its_covered_pixels_would_hold_it() {
        let mut random = Random(0x2545_f491_4f6c_dd1d);
        // A tiling of a box, with triangles over it that cover others.
        let mut tiles = vec![];
        for row in 0..6 {
            for column in 0..6 {
                let (x, y) = (20.0 + column as f64 * 30.0, 40.0 + row as f64 * 25.0);
                let color = random.below(1.0) as f32;
                tiles.push(triangle([(x, y), (x + 30.0, y), (x, y + 25.0)], color));
                tiles.push(triangle(
                    [(x + 30.0, y), (x + 30.0, y + 25.0), (x, y + 25.0)],
                    color / 3.0,
                ));
            }
        }
        for _ in 0..10 {
            let corner =
                |random: &mut Random| (20.0 + random.below(180.0), 40.0 + random.below(150.0));
            let points = [
                corner(&mut random),
                corner(&mut random),
                corner(&mut random),
            ];
            tiles.push(triangle(points, random.below(1.0) as f32));
        }
        for transform in [
            Affine::IDENTITY,
            Affine::new([1.5, 0.3, -0.2, 1.1, 10.0, -12.5]),
        ] {
            assert!(matches!(check(&tiles, transform), Samples::Grid { .. }));
        }

        // Slivers far apart, whose box the grid would mostly fill with nothing.
        let slivers = [
            triangle([(1.0, 1.0), (3.0, 1.0), (1.0, 390.0)], 0.25),
            triangle([(380.0, 1.0), (383.0, 1.0), (380.0, 390.0)], 0.75),
        ];
        assert!(matches!(check(&slivers, Affine::IDENTITY), Samples::Map(_)));
        // Nothing, and a mesh off the device.
        assert!(matches!(check(&[], Affine::IDENTITY), Samples::Map(_)));
        check(&tiles, Affine::translate((-500.0, -500.0)));
    }

    #[test]
    fn a_triangle_cut_by_a_box_keeps_the_part_inside() {
        let triangle = [
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(0.0, 10.0),
        ];
        let (area, extent) = cut_by(&triangle, Rect::new(2.0, 2.0, 20.0, 20.0));
        // The triangle (2, 2), (8, 2), (2, 8).
        assert!((area - 18.0).abs() < 1e-9, "{area}");
        assert_eq!(extent, Rect::new(2.0, 2.0, 8.0, 8.0));
        let (area, _) = cut_by(&triangle, Rect::new(11.0, 11.0, 20.0, 20.0));
        assert_eq!(area, 0.0);
        let (area, extent) = cut_by(&triangle, Rect::new(-5.0, -5.0, 20.0, 20.0));
        assert!((area - 50.0).abs() < 1e-9, "{area}");
        assert_eq!(extent, Rect::new(0.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn a_span_is_the_floor_to_the_ceiling() {
        let values = [
            -70000.0,
            -1.5,
            -0.5,
            -0.0,
            0.0,
            0.25,
            1.0,
            1.5,
            2.0,
            65534.5,
            65535.0,
            65535.5,
            70000.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ];
        for lo in values {
            for hi in values {
                assert_eq!(
                    span(lo, hi),
                    (lo.floor() as u16)..(hi.ceil() as u16),
                    "{lo} {hi}"
                );
            }
        }
    }
}
