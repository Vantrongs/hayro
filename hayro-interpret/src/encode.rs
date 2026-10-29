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

/// Steps of work: an operator of a function run, a value read, computed or
/// written, a pixel visited. Its arithmetic saturates, so a cost made of others
/// is never less than any of them: work too large to count is never given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cost(u64);

impl Cost {
    /// No work.
    pub const ZERO: Self = Self(0);

    /// `steps` steps.
    pub const fn new(steps: u64) -> Self {
        Self(steps)
    }

    /// A step for each of `n` things.
    pub fn of(n: usize) -> Self {
        Self(u64::try_from(n).unwrap_or(u64::MAX))
    }

    /// The steps, `u64::MAX` for any number of them that does not fit.
    pub const fn steps(self) -> u64 {
        self.0
    }

    /// The cost of doing this `n` times.
    #[must_use]
    pub const fn times(self, n: u64) -> Self {
        Self(self.0.saturating_mul(n))
    }
}

impl std::ops::Add for Cost {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }
}

impl std::iter::Sum for Cost {
    fn sum<I: Iterator<Item = Self>>(costs: I) -> Self {
        costs.fold(Self::ZERO, |sum, cost| sum + cost)
    }
}

/// What a caller lets sampling a shading take, asked before the work is done: the
/// steps of work ([`Cost`]) and the bytes held while it runs. A sampling it
/// refuses stops unfinished, with [`Exhausted`], having done no more than it was
/// given.
pub trait Meter {
    /// Takes `steps`: `false`, and the work stops, when fewer are left.
    fn work(&mut self, steps: Cost) -> bool;

    /// Takes `bytes` held until the call that takes them returns: `false`, and the
    /// work stops, when they do not fit.
    fn hold(&mut self, bytes: usize) -> bool;
}

/// A meter that gives any work and memory asked.
pub struct Unmetered;

impl Meter for Unmetered {
    fn work(&mut self, _: Cost) -> bool {
        true
    }

    fn hold(&mut self, _: usize) -> bool {
        true
    }
}

/// Work its [`Meter`] refused to give more to, stopped unfinished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exhausted;

/// Takes `steps` of `meter`, or stops with [`Exhausted`].
pub(crate) fn take(meter: &mut dyn Meter, steps: Cost) -> Result<(), Exhausted> {
    meter.work(steps).then_some(()).ok_or(Exhausted)
}

/// The steps sampling a point takes besides its function and its color: finding the
/// value it is evaluated at, adding it to the reach a mesh is sampled over, and
/// putting the result out.
const POINT_COST: Cost = Cost::new(8);

/// The steps placing a triangle of a mesh takes, its box found and cut by the
/// region sampled, or a patch's cut out of it (`PATCH_TRIANGLES` of them a patch).
const TRIANGLE_COST: Cost = Cost::new(64);

/// The steps visiting a pixel of a triangle's box takes, besides one for each
/// color component it interpolates there.
const VISIT_COST: Cost = Cost::new(4);

/// Why an [`Unmetered`] sampling cannot stop.
const UNMETERED: &str = "an unmetered sampling is never refused";

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
    /// The triangles the mesh is cut into.
    fn triangles(&self) -> u64 {
        match self.mesh.as_ref() {
            ShadingType::TriangleMesh { triangles, .. } => triangles.len() as u64,
            ShadingType::CoonsPatchMesh { patches, .. } => {
                (patches.len() as u64).saturating_mul(PATCH_TRIANGLES as u64)
            }
            ShadingType::TensorProductPatchMesh { patches, .. } => {
                (patches.len() as u64).saturating_mul(PATCH_TRIANGLES as u64)
            }
            _ => 0,
        }
    }

    /// Calls `f` with each triangle of the mesh in order, a patch's cut only when
    /// it is reached, so no more than one patch's are held at once.
    fn each_triangle(&self, mut f: impl FnMut(&Triangle)) {
        let mut patched = vec![];
        let mut cut = |to_triangles: &dyn Fn(&mut Vec<Triangle>)| {
            patched.clear();
            to_triangles(&mut patched);
            patched.iter().for_each(&mut f);
        };
        match self.mesh.as_ref() {
            ShadingType::TriangleMesh { triangles, .. } => triangles.iter().for_each(f),
            ShadingType::CoonsPatchMesh { patches, .. } => {
                for patch in patches {
                    cut(&|buffer| patch.to_triangles(buffer));
                }
            }
            ShadingType::TensorProductPatchMesh { patches, .. } => {
                for patch in patches {
                    cut(&|buffer| patch.to_triangles(buffer));
                }
            }
            _ => {}
        }
    }

    /// The colors of the pixels in `region` the mesh covers, the work and memory
    /// taken of `meter` before each pass over the triangles.
    fn sample(&self, region: &Region, meter: &mut dyn Meter) -> Result<Samples, Exhausted> {
        let placing = TRIANGLE_COST.times(self.triangles());
        take(meter, placing)?;
        let plan = plan(|f| self.each_triangle(f), self.transform, region);
        if !meter.hold(plan.bytes()) {
            return Err(Exhausted);
        }
        take(
            meter,
            placing + (VISIT_COST + Cost::of(plan.components)).times(plan.visits),
        )?;
        Ok(sample_planned(
            &plan,
            |f| self.each_triangle(f),
            self.transform,
            region,
        ))
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
        let samples = self
            .samples_at(&mut Unmetered, |pixels| pixels.add(pos))
            .expect(UNMETERED);
        self.sample_in(pos, samples.as_ref())
    }

    /// The most steps sampling a point takes once the tables its color is looked up
    /// in are built ([`setup_cost`](Self::setup_cost)): its function's
    /// ([`Function::cost`](crate::Function::cost)), its color's
    /// ([`ColorSpace::cost`](crate::color::ColorSpace::cost)) and the transfer
    /// function's. A mesh's triangles are placed besides, once for all points.
    pub fn point_cost(&self) -> Cost {
        let function = match &self.shading_type {
            EncodedShadingType::FunctionBased(shading) => shading.function.cost(),
            EncodedShadingType::RadialAxial(shading) => shading.function.cost(),
            EncodedShadingType::Sampled(mesh) => {
                mesh.function.as_ref().map_or(Cost::ZERO, |f| f.cost())
            }
            EncodedShadingType::Dummy(_) => Cost::ZERO,
        };
        // A step a channel mapped.
        let transfer = if self.transfer_function.is_some() {
            Cost::new(3)
        } else {
            Cost::ZERO
        };
        POINT_COST + transfer + function + self.color_space.cost()
    }

    /// The steps building the tables sampling looks colors up in takes, until they
    /// are built: the color space's and the transfer function's.
    pub fn setup_cost(&self) -> Cost {
        let transfer = self
            .transfer_function
            .as_ref()
            .map_or(Cost::ZERO, ActiveTransferFunction::setup_cost);
        self.color_space.setup_cost() + transfer
    }

    /// For a mesh shading, its samples at every pixel `walk` adds a point in, the
    /// work and memory taken of `meter` first.
    fn samples_at(
        &self,
        meter: &mut dyn Meter,
        walk: impl FnOnce(&mut Reach),
    ) -> Result<Option<Samples>, Exhausted> {
        let EncodedShadingType::Sampled(mesh) = &self.shading_type else {
            return Ok(None);
        };
        let mut reach = Reach(None);
        walk(&mut reach);
        // The last pixel of a row or column, 65535, is never a triangle's.
        let region = reach.0.map_or((0..0, 0..0), |(x0, x1, y0, y1)| {
            (x0..x1.saturating_add(1), y0..y1.saturating_add(1))
        });
        mesh.sample(&region, meter).map(Some)
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
        f: impl FnMut([f32; 4]),
    ) -> (u32, u32, Affine) {
        self.sample_texture_metered(bbox, scale, &mut Unmetered, f)
            .expect(UNMETERED)
    }

    /// [`sample_texture`](Self::sample_texture), taking its work and memory of
    /// `meter` before it does it: the tables colors are looked up in
    /// ([`setup_cost`](Self::setup_cost)), each point
    /// ([`point_cost`](Self::point_cost)), and for a mesh, each triangle placed and
    /// each pixel of its box visited, twice, and the samples held. `Exhausted`, with
    /// no point passed to `f`, when `meter` refuses.
    pub fn sample_texture_metered(
        &self,
        bbox: Rect,
        scale: f32,
        meter: &mut dyn Meter,
        mut f: impl FnMut([f32; 4]),
    ) -> Result<(u32, u32, Affine), Exhausted> {
        let (width, height) = texture_dimensions(bbox, scale);
        let points = u64::from(width) * u64::from(height);
        take(meter, self.setup_cost() + self.point_cost().times(points))?;
        // A mesh is sampled only at the pixels the texture reads.
        let samples = self.samples_at(meter, |reach| {
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
        })?;
        self.texture_points(bbox, scale, |point| {
            f(self.sample_in(point, samples.as_ref()));
        });

        let inv_scale = 1.0 / scale as f64;
        Ok((
            width,
            height,
            Affine::translate((bbox.x0, bbox.y0)) * Affine::scale(inv_scale),
        ))
    }

    /// The shading at each of `points`, in the space of
    /// [`sample_texture`](Self::sample_texture)'s `bbox`, passed to `f` in their
    /// order as straight RGBA, its work and memory taken of `meter` first as
    /// [`sample_texture_metered`](Self::sample_texture_metered) takes them.
    /// `Exhausted`, with no point passed to `f`, when `meter` refuses. A mesh is
    /// sampled at the pixels the points fall in, as its texture is.
    pub fn sample_points_metered(
        &self,
        points: &[Point],
        meter: &mut dyn Meter,
        mut f: impl FnMut([f32; 4]),
    ) -> Result<(), Exhausted> {
        take(
            meter,
            self.setup_cost() + self.point_cost().times(points.len() as u64),
        )?;
        let samples = self.samples_at(meter, |reach| {
            for point in points {
                reach.add(self.base_transform * *point);
            }
        })?;
        for point in points {
            f(self.sample_in(self.base_transform * *point, samples.as_ref()));
        }
        Ok(())
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
    /// [`encode`](Self::encode), taking first of `meter` the steps building the
    /// tables its background color is looked up in, until they are built, and
    /// converting it.
    pub fn encode_metered(
        &self,
        meter: &mut dyn Meter,
    ) -> Result<EncodedShadingPattern, Exhausted> {
        let color_space = &self.shading.color_space;
        if self.shading.background.is_some() {
            take(meter, color_space.setup_cost() + color_space.cost())?;
        }
        Ok(self.encode())
    }

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

/// What sampling a mesh's triangles in a region takes, found in a pass over them
/// before any is sampled.
struct Plan {
    /// The box of the pixels sampled, `x0`, `x1`, `y0`, `y1`, where a grid of it
    /// takes at most twice the cells of the samples it may put; `None` for a map.
    grid: Option<(u16, u16, u16, u16)>,
    /// The most pixels the triangles cover, counting each time one is covered.
    most: usize,
    /// The pixels of the region the triangles' boxes hold, each as often as a box
    /// holds it: those sampling visits.
    visits: u64,
    /// The most color components a triangle interpolates.
    components: usize,
    /// The pixels of the region.
    region: usize,
}

impl Plan {
    /// The most bytes the samples take while they are put.
    fn bytes(&self) -> usize {
        // Components past those a sample holds inline are on the heap.
        let spill = if self.components > 4 {
            self.components * size_of::<f32>()
        } else {
            0
        };
        let samples = self.most.min(self.region);
        match self.grid {
            Some((x0, x1, y0, y1)) => {
                let cells = usize::from(x1 - x0) * usize::from(y1 - y0);
                cells * size_of::<Option<ColorComponents>>() + samples * spill
            }
            None => {
                // A table of buckets a power of two, at most seven eighths full,
                // while it grows next to the one before.
                let buckets = (samples * 8 / 7 + 1).next_power_of_two();
                let bucket = size_of::<((u16, u16), ColorComponents)>() + 1;
                buckets * bucket * 3 / 2 + samples * spill
            }
        }
    }
}

/// Plans sampling the triangles `triangles` passes on, placed by `transform`, in
/// `region`.
fn plan(
    triangles: impl FnOnce(&mut dyn FnMut(&Triangle)),
    transform: Affine,
    region: &Region,
) -> Plan {
    let mut most = 0_usize;
    let mut visits = 0_u64;
    let mut components = 0;
    let mut bounds: Option<(u16, u16, u16, u16)> = None;
    triangles(&mut |t| {
        let Some((xs, ys, covered)) = pixels(t, transform, region) else {
            return;
        };
        most = most.saturating_add(covered);
        visits = visits.saturating_add((xs.len() * ys.len()) as u64);
        components = components.max(t.p0.colors.len());
        let (x0, x1, y0, y1) = bounds.unwrap_or((xs.start, xs.end, ys.start, ys.end));
        bounds = Some((
            x0.min(xs.start),
            x1.max(xs.end),
            y0.min(ys.start),
            y1.max(ys.end),
        ));
    });
    // A grid of the box that holds the pixels sampling may put takes at most twice
    // the cells of the samples it may put, and no more than the region.
    let grid = bounds.filter(|(x0, x1, y0, y1)| {
        usize::from(x1 - x0) * usize::from(y1 - y0) <= most.saturating_mul(2)
    });
    Plan {
        grid,
        most,
        visits,
        components,
        region: region.0.len() * region.1.len(),
    }
}

/// The samples of the triangles `triangles` passes on, as `plan` planned them.
fn sample_planned(
    plan: &Plan,
    triangles: impl FnOnce(&mut dyn FnMut(&Triangle)),
    transform: Affine,
    region: &Region,
) -> Samples {
    match plan.grid {
        Some((x0, x1, y0, y1)) => {
            let width = usize::from(x1 - x0);
            let mut cells = vec![None; width * usize::from(y1 - y0)];
            sample(triangles, transform, region, |x, y, color| {
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
            sample(triangles, transform, region, |x, y, color| {
                map.insert((x, y), color);
            });
            Samples::Map(map)
        }
    }
}

#[cfg(test)]
fn sample_triangles(triangles: &[Triangle], transform: Affine, region: &Region) -> Samples {
    let each = |f: &mut dyn FnMut(&Triangle)| triangles.iter().for_each(f);
    sample_planned(&plan(each, transform, region), each, transform, region)
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

/// Puts the color of each pixel of each triangle's box in `region` that the
/// triangle, placed by `transform`, covers, the triangles in the order `triangles`
/// passes them on.
fn sample(
    triangles: impl FnOnce(&mut dyn FnMut(&Triangle)),
    transform: Affine,
    region: &Region,
    mut put: impl FnMut(u16, u16, ColorComponents),
) {
    triangles(&mut |t| {
        let Some((xs, ys, _)) = pixels(t, transform, region) else {
            return;
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
        for y in ys {
            for x in xs.clone() {
                let point = Point::new(x as f64, y as f64);
                if t.contains_point(point) {
                    put(x, y, t.interpolate(point));
                }
            }
        }
    });
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
    use super::{
        Cost, EncodedSampledShading, EncodedShadingType, Exhausted, Meter, PATCH_TRIANGLES, Region,
        Samples, TRIANGLE_COST, cut_by, sample_triangles,
    };
    use crate::shading::{CoonsPatch, ShadingType, Triangle, TriangleVertex};
    use hayro_syntax::hash;
    use kurbo::{Affine, Point, Rect};
    use smallvec::smallvec;
    use std::sync::Arc;

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

    /// Gives `left` steps, and records what it is asked.
    struct Recording {
        left: u64,
        asked: Vec<u64>,
        held: Vec<usize>,
    }

    impl Meter for Recording {
        fn work(&mut self, steps: Cost) -> bool {
            let steps = steps.steps();
            self.asked.push(steps);
            let Some(left) = self.left.checked_sub(steps) else {
                return false;
            };
            self.left = left;
            true
        }

        fn hold(&mut self, bytes: usize) -> bool {
            self.held.push(bytes);
            true
        }
    }

    /// The pattern object 4 of a one-page file of `objects`, numbered from 4.
    fn shading_pattern(objects: &[String]) -> crate::pattern::ShadingPattern {
        use hayro_syntax::object::{Dict, ObjectIdentifier};
        let mut all = crate::fixtures::one_page_objects(10, "");
        all.extend(objects.iter().map(|o| o.clone().into_bytes()));
        let pdf = hayro_syntax::Pdf::new(crate::fixtures::file_with_aliases(all, &[]))
            .expect("valid pdf");
        let dict = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(4, 0))
            .expect("a pattern");
        crate::pattern::ShadingPattern::new(&dict, &crate::cache::Cache::new()).expect("a shading")
    }

    /// A tint transform whose evaluation takes more steps than a `u64` counts
    /// costs as much as one can: a `DeviceN` space of 64 colorants whose Type 0
    /// tint reads 2^16 samples around each point (its sizes sixteen 2s and
    /// forty-eight 1s, its table 2^16 bytes) costs `u64::MAX`, not that plus the
    /// colorants wrapped round to 65, so sampling it with a small allowance is
    /// refused before any color is evaluated, in a debug build and a release one.
    #[test]
    fn a_cost_past_counting_is_refused_before_any_evaluation() {
        use crate::function::TYPE0_EVALS;
        let n = 64;
        let names: Vec<String> = (0..n).map(|i| format!("/C{i}")).collect();
        let sizes: Vec<&str> = (0..n).map(|i| if i < 16 { "2" } else { "1" }).collect();
        let pairs = vec!["0 1"; n].join(" ");
        let samples = "A".repeat(1 << 16);
        let tint = format!(
            "<< /FunctionType 0 /Domain [{pairs}] /Range [0 1] /Size [{}] /BitsPerSample 8 \
             /Length {} >>\nstream\n{samples}\nendstream",
            sizes.join(" "),
            samples.len(),
        );
        let program = format!("{{ pop pop {}}}", "0.3 ".repeat(n));
        let components = format!(
            "<< /FunctionType 4 /Domain [0 1 0 1] /Range [{pairs}] /Length {} >>\nstream\n{program}\nendstream",
            program.len()
        );
        let pattern = shading_pattern(&[
            "<< /PatternType 2 /Shading 5 0 R >>".to_string(),
            format!(
                "<< /ShadingType 1 /ColorSpace [/DeviceN [{}] /DeviceGray 6 0 R] \
                 /Function 7 0 R >>",
                names.join(" ")
            ),
            tint,
            components,
        ]);
        assert_eq!(pattern.shading.color_space.cost(), Cost::new(u64::MAX));
        let encoded = pattern.encode();
        assert_eq!(encoded.point_cost(), Cost::new(u64::MAX));
        TYPE0_EVALS.set(0);
        let mut meter = Recording {
            left: 1 << 20,
            asked: vec![],
            held: vec![],
        };
        let mut sampled = 0;
        let refused = encoded.sample_texture_metered(
            Rect::new(0.0, 0.0, 100.0, 100.0),
            1.0,
            &mut meter,
            |_| sampled += 1,
        );
        assert_eq!(refused, Err(Exhausted));
        assert_eq!((sampled, TYPE0_EVALS.get()), (0, 0));
        // The tint does run when it is given the work, so the count above
        // shows it was refused, not that it never would have run.
        let _ = encoded.sample(Point::new(0.5, 0.5));
        assert!(TYPE0_EVALS.get() > 0);
    }

    /// Gathering a stitching function's bounds, as each fill drawn as a native
    /// gradient does, walks no function past the last of its bounds and
    /// encodings, which evaluating never reaches: with 1 to 10⁴ functions
    /// stitched over an empty `/Bounds`, 100 fills visit one function each and
    /// take the same steps, whatever the length of `/Functions`.
    #[test]
    fn a_stitching_walks_only_the_functions_it_evaluates() {
        use crate::function::TYPE3_VISITED;
        let mut taken = None;
        for n in [1, 100, 10_000] {
            let child = "<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >> ";
            let pattern = shading_pattern(&[
                "<< /PatternType 2 /Shading 5 0 R >>".to_string(),
                format!(
                    "<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 10 0] \
                     /Extend [true true] /Function << /FunctionType 3 /Domain [0 1] \
                     /Functions [{}] /Bounds [] /Encode [0 1] >> >>",
                    child.repeat(n)
                ),
            ]);
            let encoded = pattern.encode();
            let EncodedShadingType::RadialAxial(gradient) = &encoded.shading_type else {
                panic!("an axial shading");
            };
            TYPE3_VISITED.set(0);
            let mut meter = Recording {
                left: u64::MAX,
                asked: vec![],
                held: vec![],
            };
            for _ in 0..100 {
                gradient
                    .as_svg_gradient_metered(
                        &encoded,
                        Rect::new(0.0, 0.0, 10.0, 10.0),
                        0.01,
                        &mut meter,
                    )
                    .expect("given all it asks")
                    .expect("a native gradient");
            }
            assert_eq!(TYPE3_VISITED.get(), 100, "{n} functions");
            let asked: u64 = meter.asked.iter().sum();
            assert_eq!(*taken.get_or_insert(asked), asked, "{n} functions");
        }
    }

    /// Points sampled one by one are the colours a texture samples at the same
    /// points, paid for as the texture's are, and a meter that refuses them
    /// stops the sampling before any colour is passed on.
    #[test]
    fn points_are_sampled_as_a_texture_samples_them() {
        let pattern = shading_pattern(&[
            "<< /PatternType 2 /Shading 5 0 R >>".to_string(),
            "<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 10 3] \
             /Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0.5 1] /N 1 >> >>"
                .to_string(),
        ]);
        let encoded = pattern.encode();
        let bbox = Rect::new(0.0, 0.0, 10.0, 4.0);
        let mut texture = vec![];
        let (width, _, to_bbox) = encoded.sample_texture(bbox, 1.0, |c| texture.push(c));
        let points: Vec<Point> = (0..texture.len())
            .map(|i| {
                let (x, y) = (i as u32 % width, i as u32 / width);
                to_bbox * Point::new(f64::from(x) + 0.5, f64::from(y) + 0.5)
            })
            .collect();
        let mut meter = Recording {
            left: u64::MAX,
            asked: vec![],
            held: vec![],
        };
        let mut sampled = vec![];
        encoded
            .sample_points_metered(&points, &mut meter, |c| sampled.push(c))
            .expect("given all it asks");
        assert_eq!(sampled, texture);
        let whole = encoded.setup_cost() + encoded.point_cost().times(points.len() as u64);
        assert_eq!(meter.asked, vec![whole.steps()]);
        let mut meter = Recording {
            left: whole.steps() - 1,
            asked: vec![],
            held: vec![],
        };
        let mut passed = 0;
        let refused = encoded.sample_points_metered(&points, &mut meter, |_| passed += 1);
        assert_eq!((refused, passed), (Err(Exhausted), 0));
    }

    /// A patch mesh is sampled as the triangles it is cut into are, one patch
    /// cut at a time, its triangles paid for before each pass over them and the
    /// pixels of their boxes before they are visited, with what its samples hold
    /// asked first; a meter that refuses stops it before it places any.
    #[test]
    fn a_mesh_takes_its_work_before_it_samples() {
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        let patches: Vec<_> = (0..30)
            .map(|_| {
                let (x, y) = (random.below(300.0), random.below(300.0));
                let corner = |dx: f64, dy: f64| Point::new(x + dx, y + dy);
                let mut control_points = [Point::ZERO; 12];
                for (i, point) in control_points.iter_mut().enumerate() {
                    let t = i as f64 / 12.0 * std::f64::consts::TAU;
                    *point = corner(60.0 * t.cos(), 40.0 * t.sin());
                }
                let colors = std::array::from_fn(|i| smallvec![i as f32 / 4.0, 0.5, 1.0]);
                CoonsPatch {
                    control_points,
                    colors,
                }
            })
            .collect();
        let mut triangles = vec![];
        for patch in &patches {
            patch.to_triangles(&mut triangles);
        }
        let mesh = EncodedSampledShading {
            mesh: Arc::new(ShadingType::CoonsPatchMesh {
                patches,
                function: None,
            }),
            transform: Affine::IDENTITY,
            function: None,
        };
        let region: Region = (20..380, 10..360);
        let expected = sample_triangles(&triangles, Affine::IDENTITY, &region);
        let placing = TRIANGLE_COST.times(30 * PATCH_TRIANGLES as u64).steps();
        let mut meter = Recording {
            left: u64::MAX,
            asked: vec![],
            held: vec![],
        };
        let samples = mesh.sample(&region, &mut meter).unwrap();
        for y in region.1.clone() {
            for x in region.0.clone() {
                assert_eq!(samples.get((x, y)), expected.get((x, y)), "({x}, {y})");
            }
        }
        assert_eq!(meter.asked.len(), 2);
        assert_eq!(meter.asked[0], placing);
        // Placing them again, and a step a pixel visited besides its components.
        assert!(meter.asked[1] > placing + 4 * 10_000, "{:?}", meter.asked);
        if let Samples::Grid { cells, .. } = &samples {
            assert!(
                meter.held[0] >= size_of_val(cells.as_slice()),
                "{:?}",
                meter.held
            );
        }
        for left in [0, placing - 1, placing, meter.asked[1] + placing - 1] {
            let mut meter = Recording {
                left,
                asked: vec![],
                held: vec![],
            };
            assert_eq!(mesh.sample(&region, &mut meter).err(), Some(Exhausted));
        }
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
