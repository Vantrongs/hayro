//! Encoding shading patterns for easy sampling.

use crate::color::{AlphaColor, ColorComponents, ColorSpace};
use crate::interpret::state::ActiveTransferFunction;
use crate::pattern::ShadingPattern;
use crate::shading::{PATCH_TRIANGLES, ShadingFunction, ShadingType, Triangle};
use crate::util::x_y_advances;
use hayro_syntax::hash;
use kurbo::{Affine, BezPath, PathEl, Point, Rect, Shape, Vec2};
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
    /// The sides of `domain` it is extended past, `x0`, `x1`, `y0`, `y1`: a point
    /// past one is taken at the side ([`EncodedShadingPattern::extend_within_metered`]).
    pub(crate) extended: [bool; 4],
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
    /// The color of the mesh where it interpolated `color`, or the background
    /// where no triangle lies.
    #[inline]
    fn color(
        &self,
        color: Option<&ColorComponents>,
        bg_color: AlphaColor,
        color_space: &ColorSpace,
    ) -> Option<AlphaColor> {
        let Some(color) = color else {
            return Some(bg_color);
        };
        if let Some(function) = &self.function {
            let val = function.eval(&color.to_smallvec())?;
            Some(color_space.to_rgba(&val, 1.0))
        } else {
            Some(color_space.to_rgba(color, 1.0))
        }
    }

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

    /// The colors of the pixels in `region` the mesh covers, placed `shift`
    /// further, the work and memory taken of `meter` before each pass over the
    /// triangles.
    fn sample(
        &self,
        region: &Region,
        shift: Vec2,
        meter: &mut dyn Meter,
    ) -> Result<Samples, Exhausted> {
        let placing = TRIANGLE_COST.times(self.triangles());
        take(meter, placing)?;
        let transform = Affine::translate(shift) * self.transform;
        let plan = plan(|f| self.each_triangle(f), transform, region);
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
            transform,
            region,
        ))
    }
}

impl EncodedSampledShading {
    /// The colors of the mesh at `points`, in device space, each found where it
    /// lies rather than at its pixel's centre: a later triangle's where two hold
    /// it, `None` where none does. The work and memory are taken of `meter` before
    /// they are spent: the triangles placed and what the points' order holds at
    /// once, then for each triangle a search a row of points its box reaches,
    /// and each point of its box tested.
    fn sample_points(
        &self,
        points: &[Point],
        meter: &mut dyn Meter,
    ) -> Result<Vec<Option<ColorComponents>>, Exhausted> {
        take(meter, TRIANGLE_COST.times(self.triangles()))?;
        let held = size_of::<((i32, i32), u32)>() + size_of::<Option<ColorComponents>>();
        if !meter.hold(points.len().saturating_mul(held)) {
            return Err(Exhausted);
        }
        // The points by the pixel they lie in, row by row.
        let mut order: Vec<((i32, i32), u32)> = points
            .iter()
            .zip(0..)
            .map(|(p, i)| ((p.y.floor() as i32, p.x.floor() as i32), i))
            .collect();
        order.sort_unstable();
        let mut colors = vec![None; points.len()];
        let mut refused = false;
        self.each_triangle(|t| {
            if refused {
                return;
            }
            let t = placed(t, self.transform);
            let bbox = t.bounding_box();
            let (x0, x1) = (bbox.x0.floor() as i32, bbox.x1.floor() as i32);
            let row_end = bbox.y1.floor() as i32;
            let first = |row: i32| order.partition_point(|&(pixel, _)| pixel < (row, x0));
            let mut at = first(bbox.y0.floor() as i32);
            // Two searches for each row of points the box reaches: its end, and
            // the next row's start.
            while let Some(&((row, _), _)) = order.get(at).filter(|(p, _)| p.0 <= row_end) {
                if !meter.work(SEARCH_COST.times(2)) {
                    refused = true;
                    return;
                }
                let end = at + order[at..].partition_point(|&(pixel, _)| pixel <= (row, x1));
                let tests = (end - at) as u64;
                if !meter.work((VISIT_COST + Cost::of(t.p0.colors.len())).times(tests)) {
                    refused = true;
                    return;
                }
                for &(_, i) in &order[at..end] {
                    let point = points[i as usize];
                    if t.contains_point(point) {
                        colors[i as usize] = Some(t.interpolate(point));
                    }
                }
                at = first(row.saturating_add(1));
            }
        });
        if refused {
            return Err(Exhausted);
        }
        Ok(colors)
    }
}

/// The steps a binary search among the points a mesh is sampled at takes.
const SEARCH_COST: Cost = Cost::new(32);

/// Device pixels, `xs` by `ys`.
type Region = (Range<u16>, Range<u16>);

/// The box points lie in, `x0`, `x1`, `y0`, `y1`, each inclusive.
struct Reach(Option<(f64, f64, f64, f64)>);

impl Reach {
    #[inline]
    fn add(&mut self, pos: Point) {
        let (x, y) = (pos.x, pos.y);
        let (x0, x1, y0, y1) = self.0.unwrap_or((x, x, y, y));
        self.0 = Some((x0.min(x), x1.max(x), y0.min(y), y1.max(y)));
    }
}

/// A mesh's samples at the device pixels of a region, placed `shift` further, so
/// that the region's pixels left of or above the origin, which a pixel's place in
/// `Samples` cannot name, are sampled where they lie.
pub(crate) struct Located {
    samples: Samples,
    shift: Vec2,
}

impl Located {
    /// The samples of the pixel `pos` lies in.
    #[inline]
    fn get(&self, pos: Point) -> Option<&ColorComponents> {
        self.samples.get(pixel(pos + self.shift))
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

/// The share of a circle's radius a path drawing it may reach past it and still
/// be taken to end at it (`extend_within_metered`): four cubic arcs, as producers
/// draw a circle, reach 0.027 % past it, and the chords a path is tested along
/// stray 0.02 % more.
const CIRCLE_SLACK: f64 = 5e-4;

/// The share of `CIRCLE_SLACK` the chords a path is tested along may stray from it.
const CHORD_SLACK: f64 = 2e-4;

/// The steps testing a point of a path against an edge of a shading takes:
/// placing it and comparing.
const EDGE_POINT_COST: Cost = Cost::new(8);

impl EncodedShadingPattern {
    /// Extends the shading past each edge of the part it paints itself in that
    /// `shape`, placed in device space by `to_device`, lies within, reaching at
    /// most `tolerance` device units past it: an end of an axial or radial
    /// shading that does not extend, a side of a function-based one's domain.
    /// Within the shape it paints as before, but no edge of its own cuts a pixel
    /// where the shape ends at it, as where a gradient fills its own rectangle or
    /// disc. A renderer that multiplies a shape's coverage of a pixel by the
    /// shading's there would cut such a pixel twice: a shape covering half of it
    /// would paint a quarter. A radial shading is extended only where one of its
    /// circles holds the other, as elsewhere extending an end changes which circle
    /// a point it painted lies on; a path may reach `CIRCLE_SLACK` of a circle's
    /// radius past it besides, as cubic arcs do. A mesh has no such edges here.
    ///
    /// Only the part of `shape` within `window`, device pixels, is tested for a
    /// radial shading: past them it paints nothing, so an end may be extended
    /// there.
    ///
    /// The work is taken of `meter` before it is done: a step for each point of
    /// the shape's elements, and for a radial shading the lines of its outline
    /// near the window ([`flatten_metered`](crate::flatten::flatten_metered)).
    /// `Exhausted`, the shading unchanged, when `meter` refuses.
    pub fn extend_within_metered(
        &mut self,
        shape: &BezPath,
        to_device: Affine,
        window: Rect,
        tolerance: f64,
        meter: &mut dyn Meter,
    ) -> Result<(), Exhausted> {
        let base = self.base_transform;
        let to_shading = base * to_device;
        let [a, b, c, d, _, _] = base.as_coeffs();
        // The shading's units across a device unit along its axes, and the fewest
        // along any direction.
        let (per_x, per_y) = (a.hypot(c), b.hypot(d));
        let det = (a * d - b * c).abs();
        let squares = a * a + b * b + c * c + d * d;
        let most = ((squares + (squares * squares - 4.0 * det * det).max(0.0).sqrt()) / 2.0).sqrt();
        let fewest = if most > 0.0 { det / most } else { 0.0 };
        let edges = match &self.shading_type {
            EncodedShadingType::FunctionBased(shading) => !shading.extended.iter().all(|&e| e),
            EncodedShadingType::RadialAxial(shading) => shading.extend != [true, true],
            EncodedShadingType::Sampled(_) | EncodedShadingType::Dummy(_) => false,
        };
        if !edges {
            return Ok(());
        }
        take(meter, EDGE_POINT_COST.times(control_points(shape) as u64))?;
        // The box of the shape's control points in the shading's space, which
        // holds the shape: a side of the box within a half-plane holds it there.
        let bounds = || {
            let mut bounds: Option<Rect> = None;
            each_control_point(shape, |p| {
                let p = to_shading * p;
                bounds = Some(bounds.map_or(Rect::from_points(p, p), |r| r.union_pt(p)));
            });
            bounds
        };
        match &mut self.shading_type {
            EncodedShadingType::FunctionBased(shading) => {
                let Some(bounds) = bounds() else {
                    return Ok(());
                };
                let domain = shading.domain;
                let (x, y) = (tolerance * per_x, tolerance * per_y);
                shading.extended[0] |= bounds.x0 >= domain.x0 - x;
                shading.extended[1] |= bounds.x1 <= domain.x1 + x;
                shading.extended[2] |= bounds.y0 >= domain.y0 - y;
                shading.extended[3] |= bounds.y1 <= domain.y1 + y;
            }
            EncodedShadingType::RadialAxial(shading) => match shading.params {
                RadialAxialParams::Axial => {
                    let Some(bounds) = bounds() else {
                        return Ok(());
                    };
                    let t = tolerance * per_x;
                    shading.extend[0] |= bounds.x0 >= -t;
                    shading.extend[1] |= bounds.x1 <= 1.0 + t;
                }
                RadialAxialParams::Radial { p1, r } => {
                    let (r0, r1) = (r.x, r.y);
                    // Where one circle holds the other, each holds the ones before
                    // it or after: one end is the larger circle's inside, the other
                    // the smaller one's outside.
                    let grows = r1 >= r0;
                    let nested = p1.to_vec2().hypot() <= (r1 - r0).abs() * (1.0 + 1e-9);
                    if !nested {
                        return Ok(());
                    }
                    let (large, small) = if grows {
                        ((p1, r1), (Point::ZERO, r0))
                    } else {
                        ((Point::ZERO, r0), (p1, r1))
                    };
                    // The tolerance in the shading's units, which reach it along
                    // every direction.
                    let near = tolerance * fewest;
                    let chord = large.1 * CHORD_SLACK + near / 2.0;
                    let mut test = Nesting::new(
                        base.transform_rect_bbox(window),
                        (
                            large.0,
                            large.1 * (1.0 + CIRCLE_SLACK - CHORD_SLACK) + near / 2.0,
                        ),
                        (small.0, small.1 * (1.0 - CIRCLE_SLACK) - near),
                    );
                    // Lines past the window only wind around points in it.
                    let cull = test.window.inflate(chord, chord);
                    crate::flatten::flatten_metered(
                        shape.iter(),
                        to_shading,
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
            EncodedShadingType::Sampled(_) | EncodedShadingType::Dummy(_) => {}
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
            EncodedShadingType::Sampled(_) | EncodedShadingType::Dummy(_) => 0,
        }
    }

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
    ) -> Result<Option<Located>, Exhausted> {
        let EncodedShadingType::Sampled(mesh) = &self.shading_type else {
            return Ok(None);
        };
        let mut reach = Reach(None);
        walk(&mut reach);
        let Some((x0, x1, y0, y1)) = reach.0 else {
            return mesh
                .sample(&(0..0, 0..0), Vec2::ZERO, meter)
                .map(|samples| {
                    Some(Located {
                        samples,
                        shift: Vec2::ZERO,
                    })
                });
        };
        // Whole pixels, so each is sampled at its centre as before.
        let shift = Vec2::new((-x0.floor()).max(0.0), (-y0.floor()).max(0.0));
        let ((x0, y0), (x1, y1)) = (
            pixel(Point::new(x0, y0) + shift),
            pixel(Point::new(x1, y1) + shift),
        );
        // The last pixel of a row or column, 65535, is never a triangle's.
        let region = (x0..x1.saturating_add(1), y0..y1.saturating_add(1));
        mesh.sample(&region, shift, meter)
            .map(|samples| Some(Located { samples, shift }))
    }

    /// The shading at `pos`, a mesh's looked up in `samples`.
    #[inline]
    fn sample_in(&self, pos: Point, samples: Option<&Located>) -> [f32; 4] {
        self.finish(
            self.shading_type
                .eval(pos, self.background_color, &self.color_space, samples),
        )
    }

    /// A color the shading evaluated to, with its opacity and transfer function.
    #[inline]
    fn finish(&self, color: Option<AlphaColor>) -> [f32; 4] {
        color
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
    /// sampled where each point lies, not at its pixel's centre as a texture
    /// samples it, so points spread over a pixel find the edges of the mesh that
    /// cross it.
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
        if let EncodedShadingType::Sampled(mesh) = &self.shading_type {
            let placed: Vec<Point> = points.iter().map(|p| self.base_transform * *p).collect();
            let colors = mesh.sample_points(&placed, meter)?;
            for color in &colors {
                f(self.finish(mesh.color(
                    color.as_ref(),
                    self.background_color,
                    &self.color_space,
                )));
            }
            return Ok(());
        }
        for point in points {
            f(self.sample_in(self.base_transform * *point, None));
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
        let t = placed(t, transform);
        for y in ys {
            for x in xs.clone() {
                let point = pixel_centre(x, y);
                if t.contains_point(point) {
                    put(x, y, t.interpolate(point));
                }
            }
        }
    });
}

/// The point a mesh's pixel is sampled at: its centre, where the texture that
/// looks it up samples it.
fn pixel_centre(x: u16, y: u16) -> Point {
    Point::new(f64::from(x) + 0.5, f64::from(y) + 0.5)
}

/// `t`, as `transform` places it.
fn placed(t: &Triangle, transform: Affine) -> Triangle {
    let [v0, v1, v2] = [&t.p0, &t.p1, &t.p2].map(|v| {
        let mut v = v.clone();
        v.point = transform * v.point;
        v
    });
    Triangle::new(v0, v1, v2)
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
        extended: [false; 4],
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
        samples: Option<&Located>,
    ) -> Option<AlphaColor> {
        match self {
            Self::FunctionBased(EncodedFunctionBasedShading {
                domain,
                function,
                extended,
            }) => {
                // A side the shading is extended past holds the points past it.
                let x = match (extended[0], extended[1]) {
                    (true, _) if pos.x < domain.x0 => domain.x0,
                    (_, true) if pos.x >= domain.x1 => domain.x1,
                    _ => pos.x,
                };
                let y = match (extended[2], extended[3]) {
                    (true, _) if pos.y < domain.y0 => domain.y0,
                    (_, true) if pos.y >= domain.y1 => domain.y1,
                    _ => pos.y,
                };
                // The domain holds its far sides only where it is extended past them.
                let within =
                    |v: f64, lo: f64, hi: f64, far: bool| v >= lo && (v < hi || far && v <= hi);
                if !(within(x, domain.x0, domain.x1, extended[1])
                    && within(y, domain.y0, domain.y1, extended[3]))
                {
                    Some(bg_color)
                } else {
                    let out = function.eval(&smallvec![x as f32, y as f32])?;
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
            Self::Sampled(mesh) => mesh.color(
                samples.and_then(|samples| samples.get(pos)),
                bg_color,
                color_space,
            ),
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

/// The points of `shape`'s elements, each element's after its start.
fn each_control_point(shape: &BezPath, mut f: impl FnMut(Point)) {
    for el in shape.elements() {
        match *el {
            PathEl::MoveTo(p) | PathEl::LineTo(p) => f(p),
            PathEl::QuadTo(p1, p2) => [p1, p2].into_iter().for_each(&mut f),
            PathEl::CurveTo(p1, p2, p3) => [p1, p2, p3].into_iter().for_each(&mut f),
            PathEl::ClosePath => {}
        }
    }
}

/// How many points `each_control_point` passes on.
fn control_points(shape: &BezPath) -> usize {
    shape
        .elements()
        .iter()
        .map(|el| match el {
            PathEl::MoveTo(_) | PathEl::LineTo(_) => 1,
            PathEl::QuadTo(..) => 2,
            PathEl::CurveTo(..) => 3,
            PathEl::ClosePath => 0,
        })
        .sum()
}

/// What the outline of a shape, its lines passed on by
/// [`flatten_metered`](crate::flatten::flatten_metered), shows of its part within
/// `window`: whether a disc, `large`, holds that part, and whether it misses
/// another, `hole`. A part bounded by lines within a convex disc lies within it; so
/// does the part of the window's sides the shape holds, which ends at its corners
/// or where its lines cross them. Where no line reaches into the hole within the
/// window, the shape holds all of the hole's part there or none of it, as it holds
/// the point of it nearest the hole's centre. Holding is winding around a point:
/// what either fill rule fills, it winds around.
struct Nesting {
    window: Rect,
    large: (Point, f64),
    hole: (Point, f64),
    /// Whether a line's part within the window reaches past `large`.
    leaves: bool,
    /// Whether a line's part within the window reaches into `hole`.
    enters: bool,
    /// Points just inside the window's corners, and the window's point nearest the
    /// hole's centre, and the lines' winding around each.
    probes: [Point; 5],
    winding: [i32; 5],
    start: Point,
    last: Point,
}

impl Nesting {
    fn new(window: Rect, large: (Point, f64), hole: (Point, f64)) -> Self {
        // A corner's probe lies inside it, off the window's sides, which a shape
        // ending at them passes along.
        let inset = 1e-9 * window.width().max(window.height()).max(f64::MIN_POSITIVE);
        let inner = window.inset(-inset);
        let nearest = Point::new(
            hole.0.x.clamp(window.x0, window.x1),
            hole.0.y.clamp(window.y0, window.y1),
        );
        Self {
            window,
            large,
            hole,
            leaves: false,
            enters: false,
            probes: [
                Point::new(inner.x0, inner.y0),
                Point::new(inner.x1, inner.y0),
                Point::new(inner.x0, inner.y1),
                Point::new(inner.x1, inner.y1),
                nearest,
            ],
            winding: [0; 5],
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
        for (probe, winding) in self.probes.iter().zip(&mut self.winding) {
            if (a.y <= probe.y) != (b.y <= probe.y)
                && a.x + (probe.y - a.y) * (b.x - a.x) / (b.y - a.y) > probe.x
            {
                *winding += if b.y > a.y { 1 } else { -1 };
            }
        }
        let Some((p, q)) = clip_to(a, b, self.window) else {
            return;
        };
        let (center, radius) = self.large;
        self.leaves |= p.distance(center) > radius || q.distance(center) > radius;
        let (center, radius) = self.hole;
        let side = q - p;
        let along = if side.hypot2() > 0.0 {
            ((center - p).dot(side) / side.hypot2()).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.enters |= center.distance(p + side * along) < radius;
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
        let (center, radius) = self.large;
        let inside = !self.leaves
            && corners
                .iter()
                .zip(self.winding)
                .all(|(corner, winding)| winding == 0 || corner.distance(center) <= radius);
        let (center, radius) = self.hole;
        let nearest = self.probes[4];
        let outside = radius <= 0.0
            || !self.enters && (nearest.distance(center) >= radius || self.winding[4] == 0);
        (inside, outside)
    }
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
                    let point = Point::new(x as f64 + 0.5, y as f64 + 0.5);
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
    /// refused before any color is evaluated, in a debug build and a release one;
    /// and so does that space wrapped in a `Pattern` space, whose conversion hands
    /// each color to it.
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
        let device_n = format!("[/DeviceN [{}] /DeviceGray 6 0 R]", names.join(" "));
        for space in [device_n.clone(), format!("[/Pattern {device_n}]")] {
            let pattern = shading_pattern(&[
                "<< /PatternType 2 /Shading 5 0 R >>".to_string(),
                format!("<< /ShadingType 1 /ColorSpace {space} /Function 7 0 R >>"),
                tint.clone(),
                components.clone(),
            ]);
            let cost = pattern.shading.color_space.cost();
            assert_eq!(cost, Cost::new(u64::MAX), "{space}");
            let encoded = pattern.encode();
            assert_eq!(encoded.point_cost(), Cost::new(u64::MAX), "{space}");
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
            assert_eq!(refused, Err(Exhausted), "{space}");
            assert_eq!((sampled, TYPE0_EVALS.get()), (0, 0), "{space}");
            // The tint does run when it is given the work, so the count above
            // shows it was refused, not that it never would have run.
            let _ = encoded.sample(Point::new(0.5, 0.5));
            assert!(TYPE0_EVALS.get() > 0, "{space}");
        }
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

    /// A shading is extended past the edges of the part it paints that a shape
    /// ends at, within a thirty-second of a device unit, and nowhere else: an
    /// axial one's ends at its own rectangle's, turned or not, but not an end a
    /// rectangle reaches a unit past; a function-based one's domain's sides; a
    /// radial one's larger circle drawn with four cubic arcs, the smaller one's
    /// outside where a ring's hole is it, but not where a disc covers the hole,
    /// and no end of one whose circles do not nest. Inside the shape it paints as
    /// before, and past an extended edge the colour at the edge.
    #[test]
    fn a_shading_is_extended_past_the_edges_a_shape_ends_at() {
        use kurbo::{BezPath, Shape};
        const TOLERANCE: f64 = 1.0 / 32.0;
        let encoded = |shading: &str, matrix: Affine| {
            let mut pattern = shading_pattern(&[
                "<< /PatternType 2 /Shading 5 0 R >>".to_string(),
                shading.to_string(),
                "<< /FunctionType 4 /Domain [0 1 0 1] /Range [0 1 0 1 0 1] /Length 22 >>\n\
                 stream\n{ pop dup 1 exch sub 0 }\nendstream"
                    .to_string(),
            ]);
            pattern.matrix = matrix;
            pattern.encode()
        };
        // The device pixels a shape covers, all of which are seen.
        let seen =
            |shape: &BezPath, matrix: Affine| matrix.transform_rect_bbox(shape.bounding_box());
        // The colour at a device point.
        let at = |encoded: &super::EncodedShadingPattern, point: Point| {
            let mut color = [0.0; 4];
            encoded
                .sample_points_metered(&[point], &mut super::Unmetered, |c| color = c)
                .expect("unmetered");
            color
        };
        let extended = |shading: &str, matrix: Affine, shape: &BezPath| {
            let mut encoded = encoded(shading, matrix);
            encoded
                .extend_within_metered(
                    shape,
                    matrix,
                    seen(shape, matrix),
                    TOLERANCE,
                    &mut super::Unmetered,
                )
                .expect("unmetered");
            encoded.extended_edges()
        };
        let red_to_blue =
            "/Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >>";
        let axial = format!(
            "<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [10.3 20 90.6 20] {red_to_blue} >>"
        );
        let turned = Affine::rotate_about(0.5, Point::new(50.0, 40.0));
        let rect = |x1: f64| Rect::new(10.3, 15.7, x1, 75.2).to_path(0.1);
        for matrix in [Affine::IDENTITY, turned, Affine::scale(4.0) * turned] {
            assert_eq!(extended(&axial, matrix, &rect(90.6)), 0b11, "{matrix:?}");
            assert_eq!(extended(&axial, matrix, &rect(91.6)), 0b01, "{matrix:?}");
        }
        // Inside it paints as before; past the start, the start's colour, not nothing.
        let mut start = encoded(&axial, Affine::IDENTITY);
        let (inside, past) = (Point::new(40.0, 30.0), Point::new(10.0, 30.0));
        let (before, nothing) = (at(&start, inside), at(&start, past));
        start
            .extend_within_metered(
                &rect(90.6),
                Affine::IDENTITY,
                seen(&rect(90.6), Affine::IDENTITY),
                TOLERANCE,
                &mut super::Unmetered,
            )
            .expect("unmetered");
        assert_eq!(nothing, [0.0; 4]);
        assert_eq!(at(&start, inside), before);
        assert_eq!(at(&start, past), [1.0, 0.0, 0.0, 1.0]);

        let function = "<< /ShadingType 1 /ColorSpace /DeviceRGB /Domain [0 1 0 1] \
                        /Matrix [80.3 0 0 59.5 10.3 15.7] /Function 6 0 R >>";
        assert_eq!(extended(function, Affine::IDENTITY, &rect(90.6)), 0b1111);
        assert_eq!(extended(function, turned, &rect(90.6)), 0b1111);
        assert_eq!(extended(function, Affine::IDENTITY, &rect(91.6)), 0b1101);
        let mut domain = encoded(function, Affine::IDENTITY);
        let (edge, past) = (Point::new(90.6, 30.0), Point::new(91.0, 30.0));
        assert_eq!(at(&domain, past), [0.0; 4]);
        domain
            .extend_within_metered(
                &rect(90.6),
                Affine::IDENTITY,
                seen(&rect(90.6), Affine::IDENTITY),
                TOLERANCE,
                &mut super::Unmetered,
            )
            .expect("unmetered");
        assert_eq!(at(&domain, past), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(at(&domain, edge), [1.0, 0.0, 0.0, 1.0]);

        // A circle as producers draw it: four cubic arcs reaching 0.027 % past it.
        let arcs = |center: Point, r: f64, reversed: bool| {
            const KAPPA: f64 = 0.552_284_749_8;
            let (c, k) = (center, r * KAPPA);
            let mut path = BezPath::new();
            path.move_to((c.x + r, c.y));
            let quarter = |path: &mut BezPath, s: f64| {
                path.curve_to(
                    (c.x + r, c.y + s * k),
                    (c.x + k, c.y + s * r),
                    (c.x, c.y + s * r),
                );
                path.curve_to(
                    (c.x - k, c.y + s * r),
                    (c.x - r, c.y + s * k),
                    (c.x - r, c.y),
                );
                path.curve_to(
                    (c.x - r, c.y - s * k),
                    (c.x - k, c.y - s * r),
                    (c.x, c.y - s * r),
                );
                path.curve_to(
                    (c.x + k, c.y - s * r),
                    (c.x + r, c.y - s * k),
                    (c.x + r, c.y),
                );
            };
            quarter(&mut path, if reversed { -1.0 } else { 1.0 });
            path.close_path();
            path
        };
        let center = Point::new(50.0, 40.0);
        let radial = |coords: &str| {
            format!("<< /ShadingType 3 /ColorSpace /DeviceRGB /Coords [{coords}] {red_to_blue} >>")
        };
        let disc = radial("50 40 0 50 40 300");
        for matrix in [Affine::IDENTITY, Affine::scale(4.0) * turned] {
            assert_eq!(
                extended(&disc, matrix, &arcs(center, 300.0, false)) & 0b10,
                0b10
            );
            assert_eq!(
                extended(&disc, matrix, &arcs(center, 300.5, false)) & 0b10,
                0
            );
        }
        let ring = radial("50 40 10 50 40 30");
        let mut hole = arcs(center, 30.0, false);
        hole.extend(arcs(center, 10.0, true));
        assert_eq!(extended(&ring, Affine::IDENTITY, &hole), 0b11);
        assert_eq!(
            extended(&ring, Affine::IDENTITY, &arcs(center, 30.0, false)),
            0b10
        );
        let cone = radial("20 40 5 80 40 10");
        let around = Rect::new(0.0, 0.0, 100.0, 80.0).to_path(0.1);
        assert_eq!(extended(&cone, Affine::IDENTITY, &around), 0);

        // The work is asked first: refused, the shading is left as it was; a
        // shading with no edge to extend asks none.
        for shading in [&axial, &disc] {
            let mut refused = encoded(shading, Affine::IDENTITY);
            let mut meter = Recording {
                left: 0,
                asked: vec![],
                held: vec![],
            };
            let shape = arcs(center, 300.0, false);
            let result = refused.extend_within_metered(
                &shape,
                Affine::IDENTITY,
                seen(&shape, Affine::IDENTITY),
                TOLERANCE,
                &mut meter,
            );
            assert_eq!((result, refused.extended_edges()), (Err(Exhausted), 0));
            assert!(meter.asked[0] > 0);
        }
        let both = format!(
            "<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [10.3 20 90.6 20] \
             /Extend [true true] {red_to_blue} >>"
        );
        let mut meter = Recording {
            left: 0,
            asked: vec![],
            held: vec![],
        };
        encoded(&both, Affine::IDENTITY)
            .extend_within_metered(
                &rect(90.6),
                Affine::IDENTITY,
                seen(&rect(90.6), Affine::IDENTITY),
                TOLERANCE,
                &mut meter,
            )
            .expect("nothing to do");
        assert!(meter.asked.is_empty());
    }

    /// A radial shading is extended where the shape ends at its circle within
    /// the window it is seen in, whatever the shape does past it, and the shape
    /// is tested near that window alone: a curve reaching 10¹⁵ units out, which
    /// tested whole asked for gigabytes of lines, asks fewer than 10⁵ steps
    /// and bytes.
    #[test]
    fn a_radial_shading_is_tested_within_the_window_it_is_seen_in() {
        use kurbo::{BezPath, Shape};
        let red_to_blue =
            "/Function << /FunctionType 2 /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >>";
        let encoded = |coords: &str| {
            shading_pattern(&[
                "<< /PatternType 2 /Shading 5 0 R >>".to_string(),
                format!(
                    "<< /ShadingType 3 /ColorSpace /DeviceRGB /Coords [{coords}] {red_to_blue} >>"
                ),
            ])
            .encode()
        };
        let extended = |coords: &str, shape: &BezPath, window: Rect, meter: &mut dyn Meter| {
            let mut encoded = encoded(coords);
            encoded
                .extend_within_metered(shape, Affine::IDENTITY, window, 1.0 / 32.0, meter)
                .map(|()| encoded.extended_edges())
        };
        // It leaves the disc of radius 30 about (50, 40) at x 80 alone; the
        // disc's centre, its other end, it misses everywhere.
        let shape = Rect::new(35.0, 35.0, 120.0, 45.0).to_path(0.1);
        let within = Rect::new(30.0, 30.0, 70.0, 50.0);
        let disc = "50 40 0 50 40 30";
        assert_eq!(
            extended(disc, &shape, within, &mut super::Unmetered),
            Ok(0b11)
        );
        assert_eq!(
            extended(disc, &shape, shape.bounding_box(), &mut super::Unmetered),
            Ok(0b01)
        );

        let mut huge = BezPath::new();
        huge.move_to((0.0, 0.0));
        huge.curve_to((1e15, 0.0), (-1e15, 1.0), (1.0, 1.0));
        huge.close_path();
        let mut meter = Recording {
            left: u64::MAX,
            asked: vec![],
            held: vec![],
        };
        let page = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert_eq!(extended("0 0 0 0 0 1", &huge, page, &mut meter), Ok(0b01));
        let (asked, held) = (
            meter.asked.iter().sum::<u64>(),
            meter.held.iter().sum::<usize>(),
        );
        assert!(asked < 100_000 && held < 100_000, "{asked} {held}");
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

    /// A mesh's points are sampled where they lie: at pixel centres, as its
    /// texture samples them; within one pixel, on either side of an edge that
    /// crosses it. They are paid for as the texture's points, then the triangles
    /// placed and the points' order held, then for each triangle two searches a
    /// row of points its box reaches and a test each point there, each taken
    /// before it is spent: a triangle 80 rows tall over points in two rows takes
    /// four searches, and a meter that refuses passes no colour on.
    #[test]
    fn mesh_points_are_sampled_where_they_lie() {
        let vertex = |x: f64, y: f64, rgb: [u8; 3]| {
            let at = |v: f64| format!("{:04X}", (v / 100.0 * 65535.0).round() as u32);
            format!(
                "00{}{}{:02X}{:02X}{:02X}",
                at(x),
                at(y),
                rgb[0],
                rgb[1],
                rgb[2]
            )
        };
        // Its right edge, at x 50.25 by the 16-bit coordinates, crosses the
        // pixels of column 50.
        let data = [
            vertex(10.0, 10.0, [255, 0, 0]),
            vertex(50.25, 10.0, [0, 255, 0]),
            vertex(50.25, 90.0, [0, 0, 255]),
        ]
        .concat();
        let pattern = shading_pattern(&[
            "<< /PatternType 2 /Shading 5 0 R >>".to_string(),
            format!(
                "<< /ShadingType 4 /ColorSpace /DeviceRGB /BitsPerCoordinate 16 \
                 /BitsPerComponent 8 /BitsPerFlag 8 /Decode [0 100 0 100 0 1 0 1 0 1] \
                 /Filter /ASCIIHexDecode /Length {} >>\nstream\n{data}>\nendstream",
                data.len() + 1
            ),
        ]);
        let encoded = pattern.encode();
        let bbox = Rect::new(0.0, 0.0, 100.0, 100.0);
        let mut texture = vec![];
        let (width, _, to_bbox) = encoded.sample_texture(bbox, 1.0, |c| texture.push(c));
        let centres: Vec<Point> = (0..texture.len())
            .map(|i| {
                let (x, y) = (i as u32 % width, i as u32 / width);
                to_bbox * Point::new(f64::from(x) + 0.5, f64::from(y) + 0.5)
            })
            .collect();
        let mut sampled = vec![];
        encoded
            .sample_points_metered(&centres, &mut super::Unmetered, |c| sampled.push(c))
            .expect("unmetered");
        assert_eq!(sampled, texture);
        assert!(texture.iter().any(|c| c[3] == 1.0));
        // Two points of pixel (50, 30), inside the edge and past it, and two
        // of pixel (20, 70), above the triangle's diagonal.
        let points = [
            Point::new(50.1, 30.5),
            Point::new(50.4, 30.5),
            Point::new(20.5, 70.5),
            Point::new(20.6, 70.5),
        ];
        let mut sampled = vec![];
        let mut meter = Recording {
            left: u64::MAX,
            asked: vec![],
            held: vec![],
        };
        encoded
            .sample_points_metered(&points, &mut meter, |c| sampled.push(c))
            .expect("given all it asks");
        assert_eq!(sampled[0][3], 1.0, "{sampled:?}");
        assert_eq!(sampled[1], [0.0; 4], "{sampled:?}");
        let whole = encoded.setup_cost() + encoded.point_cost().times(points.len() as u64);
        let search = super::SEARCH_COST.times(2).steps();
        let test = (super::VISIT_COST + Cost::of(3)).steps();
        assert_eq!(
            meter.asked,
            [
                whole.steps(),
                TRIANGLE_COST.steps(),
                search,
                2 * test,
                search,
                2 * test
            ]
        );
        assert_eq!(meter.held.len(), 1);
        assert!(meter.held[0] >= points.len() * size_of::<Option<crate::color::ColorComponents>>());
        let all: u64 = meter.asked.iter().sum();
        for left in [0, whole.steps(), all - 1] {
            let mut meter = Recording {
                left,
                asked: vec![],
                held: vec![],
            };
            let mut passed = 0;
            let refused = encoded.sample_points_metered(&points, &mut meter, |_| passed += 1);
            assert_eq!((refused, passed), (Err(Exhausted), 0), "{left}");
        }
    }

    /// A texture reaching left of and above the origin samples a mesh there
    /// where its pixels lie, as one that starts at the origin does: past the
    /// mesh, nothing, not the colour of the pixel at the origin.
    #[test]
    fn a_mesh_is_sampled_where_it_lies_left_of_the_origin() {
        let vertex = |x: f64, y: f64| {
            let at = |v: f64| format!("{:04X}", (v / 100.0 * 65535.0).round() as u32);
            format!("00{}{}000000", at(x), at(y))
        };
        let data = [vertex(0.0, 0.0), vertex(50.0, 0.0), vertex(0.0, 50.0)].concat();
        let pattern = shading_pattern(&[
            "<< /PatternType 2 /Shading 5 0 R >>".to_string(),
            format!(
                "<< /ShadingType 4 /ColorSpace /DeviceRGB /BitsPerCoordinate 16 \
                 /BitsPerComponent 8 /BitsPerFlag 8 /Decode [0 100 0 100 0 1 0 1 0 1] \
                 /Filter /ASCIIHexDecode /Length {} >>\nstream\n{data}>\nendstream",
                data.len() + 1
            ),
        ]);
        let encoded = pattern.encode();
        let texture = |bbox: Rect| {
            let mut texture = vec![];
            let (width, _, _) = encoded.sample_texture(bbox, 1.0, |c| texture.push(c));
            (width as usize, texture)
        };
        let (whole_width, whole) = texture(Rect::new(0.0, 0.0, 60.0, 60.0));
        let (width, shifted) = texture(Rect::new(-2.0, -3.0, 60.0, 60.0));
        for (i, color) in shifted.iter().enumerate() {
            let (x, y) = ((i % width) as i64 - 2, (i / width) as i64 - 3);
            let expected = if x < 0 || y < 0 {
                [0.0; 4]
            } else {
                whole[y as usize * whole_width + x as usize]
            };
            assert_eq!(*color, expected, "({x}, {y})");
        }
        assert_eq!(whole[0], [0.0, 0.0, 0.0, 1.0]);
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
        let samples = mesh.sample(&region, kurbo::Vec2::ZERO, &mut meter).unwrap();
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
            assert_eq!(
                mesh.sample(&region, kurbo::Vec2::ZERO, &mut meter).err(),
                Some(Exhausted)
            );
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
