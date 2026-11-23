//! Encoding shading patterns for easy sampling.

mod edges;

use crate::canvas::Canvas;
use crate::color::{AlphaColor, ColorSpace};
use crate::function::MAX_COMPONENTS;
use crate::function_shading::draw_function;
use crate::gradient::ParameterColors;
use crate::interpret::state::ActiveTransferFunction;
use crate::mesh::draw_mesh;
use crate::pattern::ShadingPattern;
use crate::shading::{ShadingFunction, ShadingType};
use crate::util::x_y_advances;
use kurbo::{Affine, Point, Rect};
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
    /// Marks the start of temporary holds within a sampling call.
    fn scratch_mark(&self) -> usize {
        0
    }
    /// Releases temporary holds after the marked scope.
    fn release_scratch(&mut self, _mark: usize) {}

    /// Releases bytes for storage dropped within the current scope.
    fn release_bytes(&mut self, _bytes: usize) {}

    /// Takes `steps`: `false`, and the work stops, when fewer are left.
    fn work(&mut self, steps: Cost) -> bool;

    /// Takes `bytes` held until the call that takes them returns: `false`, and the
    /// work stops, when they do not fit.
    fn hold(&mut self, bytes: usize) -> bool;
}

pub(crate) fn push_metered<T>(
    vec: &mut Vec<T>,
    value: T,
    meter: &mut dyn Meter,
) -> Result<(), Exhausted> {
    reserve_metered(vec, 1, meter)?;
    vec.push(value);
    Ok(())
}

pub(crate) fn capacity_after<T>(vec: &Vec<T>, additional: usize) -> usize {
    let required = vec.len().saturating_add(additional);
    if required <= vec.capacity() {
        vec.capacity()
    } else {
        required.max(vec.capacity().saturating_mul(2)).max(4)
    }
}

pub(crate) fn reserve_metered<T>(
    vec: &mut Vec<T>,
    additional: usize,
    meter: &mut dyn Meter,
) -> Result<(), Exhausted> {
    let capacity = capacity_after(vec, additional);
    if capacity > vec.capacity() {
        let old = vec.capacity();
        let bytes = capacity.checked_mul(size_of::<T>()).ok_or(Exhausted)?;
        if !meter.hold(bytes) {
            return Err(Exhausted);
        }
        vec.try_reserve_exact(capacity - vec.len())
            .map_err(|_| Exhausted)?;
        meter.release_bytes(old * size_of::<T>());
    }
    Ok(())
}

pub(crate) fn scratch<T>(
    meter: &mut dyn Meter,
    run: impl FnOnce(&mut dyn Meter) -> Result<T, Exhausted>,
    keeps_storage: impl FnOnce(&T) -> bool,
) -> Result<T, Exhausted> {
    let mark = meter.scratch_mark();
    let result = run(meter);
    if !result.as_ref().is_ok_and(keeps_storage) {
        meter.release_scratch(mark);
    }
    result
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

/// A shading pattern that was encoded so it can be sampled.
#[derive(Clone, Debug)]
pub struct EncodedShadingPattern {
    pub(crate) ramp: Option<Arc<crate::gradient::SampleRamp>>,
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
    /// Triangle-mesh or patch-mesh shading.
    Mesh(EncodedMeshShading),
    /// Empty dummy shading.
    Dummy(EncodedDummyShading),
}

/// Encoded function-based shading.
#[derive(Clone, Debug)]
pub struct EncodedFunctionBasedShading {
    pub(crate) domain: Rect,
    extended: [bool; 4],
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

/// Encoded triangle-mesh or patch-mesh shading.
#[derive(Clone, Debug)]
pub struct EncodedMeshShading {
    pub(crate) unit_components: bool,
    pub(crate) shading_type: Arc<ShadingType>,
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
    /// The transform from texture space to the space of the bounding box.
    pub transform: Affine,
    /// Whether any pixel of the texture is not fully opaque.
    pub has_transparency: bool,
}

/// The texture dimensions for `bbox` at `scale` pixels per unit.
pub fn texture_dimensions(bbox: Rect, scale: f32) -> (u32, u32) {
    (
        (bbox.width() as f32 * scale).max(1.0).ceil() as u32,
        (bbox.height() as f32 * scale).max(1.0).ceil() as u32,
    )
}

impl EncodedShadingPattern {
    /// Prepares colors within the work of directly evaluating the visible samples.
    pub fn sample_ramp_for(
        &self,
        samples: u64,
        meter: &mut dyn Meter,
    ) -> Result<Option<Arc<crate::gradient::SampleRamp>>, Exhausted> {
        struct Preparation<'a> {
            meter: &'a mut dyn Meter,
            remaining: u64,
            exceeded: bool,
        }
        impl Meter for Preparation<'_> {
            fn work(&mut self, work: Cost) -> bool {
                let Some(left) = self.remaining.checked_sub(work.steps()) else {
                    self.exceeded = true;
                    return false;
                };
                self.remaining = left;
                self.meter.work(work)
            }
            fn scratch_mark(&self) -> usize {
                self.meter.scratch_mark()
            }
            fn release_bytes(&mut self, bytes: usize) {
                self.meter.release_bytes(bytes);
            }
            fn release_scratch(&mut self, mark: usize) {
                self.meter.release_scratch(mark);
            }
            fn hold(&mut self, bytes: usize) -> bool {
                self.meter.hold(bytes)
            }
        }
        let mut limit = Preparation {
            meter,
            remaining: self.point_cost().times(samples).steps(),
            exceeded: false,
        };
        let ramp = self.sample_ramp(&mut limit);
        if limit.exceeded { Ok(None) } else { ramp }
    }

    /// Prepares this placement for raster or native rendering.
    pub fn prepare_for(mut self, bbox: Rect) -> Self {
        self.ramp = self
            .sample_ramp_for(bbox.area().ceil() as u64, &mut Unmetered)
            .expect("unmetered");
        self
    }

    /// Prepare a color ramp for the shading domain, function, color space, transfer and opacity.
    pub fn sample_ramp(
        &self,
        meter: &mut dyn Meter,
    ) -> Result<Option<Arc<crate::gradient::SampleRamp>>, Exhausted> {
        let (function, domain) = match &self.shading_type {
            EncodedShadingType::RadialAxial(s) => (&s.function, s.domain),
            EncodedShadingType::Mesh(s) => match s.shading_type.as_ref() {
                ShadingType::TriangleMesh {
                    function: Some(f),
                    domain,
                    ..
                }
                | ShadingType::PatchMesh {
                    function: Some(f),
                    domain,
                    ..
                } => (f, *domain),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        crate::gradient::SampleRamp::new(function, domain, self, meter).map(|r| r.map(Arc::new))
    }
    /// Use a ramp prepared from the same source shading with the same transfer and opacity.
    pub fn set_sample_ramp(&mut self, ramp: Option<Arc<crate::gradient::SampleRamp>>) {
        self.ramp = ramp;
    }

    /// Work to evaluate and convert one sample.
    pub fn point_cost(&self) -> Cost {
        let function = match &self.shading_type {
            EncodedShadingType::FunctionBased(s) => Some(&s.function),
            EncodedShadingType::RadialAxial(s) => Some(&s.function),
            EncodedShadingType::Mesh(s) => match &*s.shading_type {
                ShadingType::TriangleMesh { function, .. }
                | ShadingType::PatchMesh { function, .. } => function.as_ref(),
                _ => None,
            },
            EncodedShadingType::Dummy(_) => None,
        };
        Cost::new(8)
            + function.map_or(Cost::ZERO, ShadingFunction::cost)
            + self.color_space.cost()
            + Cost::new(3 * u64::from(self.transfer_function.is_some()))
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

    pub(crate) fn finish(&self, mut color: [f32; 4]) -> [f32; 4] {
        color[3] *= self.opacity;

        match &self.transfer_function {
            Some(tf) => tf.apply(&AlphaColor::new(color)).components(),
            None => color,
        }
    }

    pub(crate) fn convert(&self, components: &[f32]) -> [f32; 4] {
        self.finish(self.color_space.to_rgba(components, 1.0).components())
    }

    pub(crate) fn eval(&self, function: &ShadingFunction, input: &[f32]) -> [f32; 4] {
        let mut out = [0.0; MAX_COMPONENTS];
        let out = &mut out[..function.num_outputs()];

        match function.eval(input, out) {
            Some(()) => self.convert(out),
            None => [0.0; 4],
        }
    }

    /// Call `f` with the straight RGBA color of each pixel of a grid covering `bbox`, in row-major order.
    pub fn sample_texture(
        &self,
        bbox: Rect,
        scale: f32,
        f: impl FnMut([f32; 4]),
    ) -> (u32, u32, Affine) {
        if let EncodedShadingType::FunctionBased(shading) = &self.shading_type
            && shading.extended.contains(&true)
        {
            let mut pixel = f;
            return self
                .sample_texture_metered(bbox, scale, &mut Unmetered, |row| {
                    row.iter().copied().for_each(&mut pixel);
                })
                .expect("unmetered");
        }
        let (width, height) = texture_dimensions(bbox, scale);
        let grid_transform =
            Affine::translate((bbox.x0, bbox.y0)) * Affine::scale(1.0 / scale as f64);
        let background = self.finish(self.background_color.components());
        let mut canvas = Canvas::new(width, height, background);
        let transform = grid_transform.inverse() * self.base_transform.inverse();

        match &self.shading_type {
            EncodedShadingType::FunctionBased(shading) => {
                draw_function(
                    &mut canvas,
                    shading.domain,
                    &shading.function,
                    transform,
                    self,
                );
            }
            EncodedShadingType::RadialAxial(shading) => shading.draw(&mut canvas, transform, self),
            EncodedShadingType::Mesh(shading) => {
                draw_mesh(&mut canvas, shading, transform, self, &mut Unmetered)
                    .expect("unmetered");
            }
            EncodedShadingType::Dummy(_) => {}
        }

        canvas.pixels.into_iter().for_each(f);

        (width, height, grid_transform)
    }

    /// Sample each row of the pixel grid after reserving its work and scratch space.
    pub fn sample_texture_metered(
        &self,
        bbox: Rect,
        scale: f32,
        meter: &mut dyn Meter,
        mut f: impl FnMut(&[[f32; 4]]),
    ) -> Result<(u32, u32, Affine), Exhausted> {
        scratch(
            meter,
            |meter| {
                let (width, height) = texture_dimensions(bbox, scale);
                let grid =
                    Affine::translate((bbox.x0, bbox.y0)) * Affine::scale(1.0 / f64::from(scale));
                let count = u64::from(width) * u64::from(height);
                let buffer = u64::from(width);
                if !meter.hold(
                    usize::try_from(buffer)
                        .ok()
                        .and_then(|n| n.checked_mul(size_of::<[f32; 4]>()))
                        .filter(|&n| n <= isize::MAX as usize)
                        .ok_or(Exhausted)?,
                ) {
                    return Err(Exhausted);
                }
                take(meter, self.setup_cost())?;
                let bg = self.finish(self.background_color.components());
                if let EncodedShadingType::Mesh(s) = &self.shading_type {
                    take(meter, Cost::new(count))?;
                    let mut canvas = Canvas::for_rows(width, height, bg);
                    draw_mesh(
                        &mut canvas,
                        s,
                        grid.inverse() * self.base_transform.inverse(),
                        self,
                        meter,
                    )?;
                    canvas.emit_rows(bg, meter, f)?;
                } else {
                    let sample = self.point_sampler(meter, count, bg)?;
                    let transform = self.base_transform * grid;
                    let mut row = vec![bg; width as usize];
                    for y in 0..height {
                        for (x, pixel) in row.iter_mut().enumerate() {
                            *pixel =
                                sample(transform * Point::new(x as f64 + 0.5, f64::from(y) + 0.5));
                        }
                        f(&row);
                    }
                }
                Ok((width, height, grid))
            },
            |_| false,
        )
    }

    /// Sample the requested positions in order. A refused request publishes no samples.
    pub fn sample_points_metered(
        &self,
        points: &[Point],
        meter: &mut dyn Meter,
        mut f: impl FnMut([f32; 4]),
    ) -> Result<(), Exhausted> {
        scratch(
            meter,
            |meter| {
                if points.is_empty() {
                    return Ok(());
                }
                take(meter, self.setup_cost())?;
                let bg = self.finish(self.background_color.components());
                if let EncodedShadingType::Mesh(s) = &self.shading_type {
                    let mut canvas = Canvas::for_points(points, bg, meter)?;
                    draw_mesh(&mut canvas, s, self.base_transform.inverse(), self, meter)?;
                    canvas.pixels.into_iter().for_each(f);
                } else {
                    let sample = self.point_sampler(meter, points.len() as u64, bg)?;
                    for point in points {
                        f(sample(self.base_transform * *point));
                    }
                }
                Ok(())
            },
            |_| false,
        )
    }

    fn point_sampler(
        &self,
        meter: &mut dyn Meter,
        count: u64,
        bg: [f32; 4],
    ) -> Result<impl Fn(Point) -> [f32; 4] + '_, Exhausted> {
        let colors = match &self.shading_type {
            EncodedShadingType::RadialAxial(s) => {
                Some(ParameterColors::new(self, &s.function, s.domain))
            }
            _ => None,
        };
        take(
            meter,
            colors
                .as_ref()
                .map_or_else(|| self.point_cost(), ParameterColors::cost)
                .times(count),
        )?;
        Ok(move |p: Point| match &self.shading_type {
            EncodedShadingType::FunctionBased(s) => {
                let coordinate = |x: f64, lo: f64, hi: f64, left: bool, right: bool| {
                    if x < lo {
                        left.then_some(lo)
                    } else if x >= hi {
                        right.then_some(hi)
                    } else {
                        Some(x)
                    }
                };
                match (
                    coordinate(p.x, s.domain.x0, s.domain.x1, s.extended[0], s.extended[1]),
                    coordinate(p.y, s.domain.y0, s.domain.y1, s.extended[2], s.extended[3]),
                ) {
                    (Some(x), Some(y)) => self.eval(&s.function, &[x as f32, y as f32]),
                    _ => bg,
                }
            }
            EncodedShadingType::RadialAxial(s) => s
                .position(p)
                .map_or(bg, |t| colors.as_ref().unwrap().get(t)),
            _ => bg,
        })
    }

    /// Rasterize the shading into a straight RGBA8 texture covering `bbox`.
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
                circles,
            } => {
                let (encoded, initial_transform) =
                    encode_axial_shading(*coords, *domain, function, *extend, *axial, *circles);

                base_transform = initial_transform * self.matrix.inverse();

                encoded
            }
            ShadingType::TriangleMesh { .. } | ShadingType::PatchMesh { .. } => {
                base_transform = self.matrix.inverse();

                EncodedShadingType::Mesh(EncodedMeshShading {
                    unit_components: self.shading.unit_components,
                    shading_type: self.shading.shading_type.clone(),
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
            .map_or(AlphaColor::TRANSPARENT, |background| {
                color_space.to_rgba(background, 1.0)
            });

        let (base_transform, shading_type) =
            if base_transform.is_finite() && base_transform.determinant() != 0.0 {
                (base_transform, shading_type)
            } else {
                (
                    Affine::IDENTITY,
                    EncodedShadingType::Dummy(EncodedDummyShading),
                )
            };
        EncodedShadingPattern {
            ramp: None,
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
    coords: [f64; 6],
    domain: [f32; 2],
    function: &ShadingFunction,
    extend: [bool; 2],
    is_axial: bool,
    circles: crate::radial::Circles,
) -> (EncodedShadingType, Affine) {
    let initial_transform;

    let params = if is_axial {
        let [x_0, y_0, x_1, y_1, _, _] = coords;

        initial_transform = ts_from_line_to_line(
            Point::new(x_0, y_0),
            Point::new(x_1, y_1),
            Point::ZERO,
            Point::new(1.0, 0.0),
        );

        RadialAxialParams::Axial
    } else {
        initial_transform = Affine::translate((-coords[0], -coords[1]));
        RadialAxialParams::Radial(circles)
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

fn encode_function_shading(domain: &[f32; 4], function: &ShadingFunction) -> EncodedShadingType {
    let domain = Rect::new(
        domain[0] as f64,
        domain[2] as f64,
        domain[1] as f64,
        domain[3] as f64,
    );

    EncodedShadingType::FunctionBased(EncodedFunctionBasedShading {
        domain,
        extended: [false; 4],
        function: function.clone(),
    })
}

#[derive(Clone, Debug)]
pub(crate) enum RadialAxialParams {
    Axial,
    Radial(crate::radial::Circles),
}

impl EncodedRadialAxialShading {
    fn draw(&self, canvas: &mut Canvas<'_>, transform: Affine, pattern: &EncodedShadingPattern) {
        let colors = ParameterColors::new(pattern, &self.function, self.domain);
        let to_shading = transform.inverse();
        let (x_advance, y_advance) = x_y_advances(&to_shading);
        let mut row_start = to_shading * Point::new(0.5, 0.5);

        for row in canvas.pixels.chunks_exact_mut(canvas.width) {
            let mut point = row_start;

            for pixel in row {
                if let Some(t) = self.position(point) {
                    *pixel = colors.get(t);
                }

                point += x_advance;
            }

            row_start += y_advance;
        }
    }

    fn position(&self, pos: Point) -> Option<f32> {
        let t = match self.params {
            RadialAxialParams::Axial => pos.x as f32,
            RadialAxialParams::Radial(circles) => {
                radial_pos(&pos, circles, self.extend[0], self.extend[1])?
            }
        };

        if t < 0.0 {
            self.extend[0].then_some(0.0)
        } else if t > 1.0 {
            self.extend[1].then_some(1.0)
        } else {
            Some(t)
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
    circles: crate::radial::Circles,
    min_extend: bool,
    max_extend: bool,
) -> Option<f32> {
    let crate::radial::Circles { p1, r, dr, a } = circles;
    if p1 == Point::ZERO {
        let t = (pos.to_vec2().hypot() - r.x) / dr;
        return (t.is_finite() && (min_extend || t >= 0.0) && (max_extend || t <= 1.0))
            .then_some(t as f32);
    }
    let b = -2.0 * (pos.to_vec2().dot(p1.to_vec2()) + r.x * dr);
    let c = pos.to_vec2().hypot2() - r.x * r.x;
    let valid = |t: f64| {
        t.is_finite() && (min_extend || t >= 0.0) && (max_extend || t <= 1.0) && r.x + dr * t >= 0.0
    };
    if dr == 0.0 {
        let length = p1.to_vec2().hypot();
        let axis = p1.to_vec2() / length;
        let across = pos.to_vec2().cross(axis).abs();
        if across > r.x {
            return None;
        }
        let along = pos.to_vec2().dot(axis);
        let half = ((r.x - across) * (r.x + across)).sqrt();
        return [(along + half) / length, (along - half) / length]
            .into_iter()
            .find(|&t| valid(t))
            .map(|t| t as f32);
    }
    // An extended near-tangent cone can paint the second root far past its end.
    if a == 0.0 {
        let t = -c / b;
        return valid(t).then_some(t as f32);
    }
    let discriminant = b.mul_add(b, -4.0 * a * c);
    if discriminant < 0.0 {
        return None;
    }
    let q = -0.5 * (b + discriminant.sqrt().copysign(b));
    let roots = [q / a, if q == 0.0 { 0.0 } else { c / q }];
    roots
        .into_iter()
        .filter(|&t| valid(t))
        .max_by(f64::total_cmp)
        .map(|t| t as f32)
}

#[cfg(test)]
mod tests;
