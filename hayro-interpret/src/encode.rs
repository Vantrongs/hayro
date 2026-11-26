//! Encoding shading patterns for easy sampling.

#[cfg(test)]
#[path = "encode/tests.rs"]
mod metering_tests;
use crate::canvas::Canvas;
use crate::color::{AlphaColor, ColorSpace};
use crate::function::MAX_COMPONENTS;
use crate::function_shading::draw_function;
use crate::interpret::state::ActiveTransferFunction;
use crate::mesh::draw_mesh;
use crate::pattern::ShadingPattern;
use crate::shading::{ShadingFunction, ShadingType};
use kurbo::{Affine, Point, Rect, Vec2};
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
) -> Result<T, Exhausted> {
    let mark = meter.scratch_mark();
    let result = run(meter);
    meter.release_scratch(mark);
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
    pub(crate) function: ShadingFunction,
}

/// Encoded axial or radial shading.
#[derive(Clone, Debug)]
pub struct EncodedRadialAxialShading {
    pub(crate) function: ShadingFunction,
    pub(crate) ramp: Option<Arc<crate::gradient::ParameterRamp>>,
    pub(crate) params: RadialAxialParams,
    pub(crate) domain: [f32; 2],
    pub(crate) extend: [bool; 2],
}

/// Encoded triangle-mesh or patch-mesh shading.
#[derive(Clone, Debug)]
pub struct EncodedMeshShading {
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
    /// Work to evaluate and convert one sample.
    pub fn point_cost(&self) -> Cost {
        self.sample_cost(self.color_space.prepared().sample)
    }
    fn sample_cost(&self, conversion: Cost) -> Cost {
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
            + conversion
            + Cost::new(3 * u64::from(self.transfer_function.is_some()))
    }
    /// The steps building the tables sampling looks colors up in takes, until they
    /// are built: the color space's and the transfer function's.
    pub fn setup_cost(&self) -> Cost {
        self.color_space.prepared().setup + self.transfer_cost()
    }
    fn transfer_cost(&self) -> Cost {
        self.transfer_function
            .as_ref()
            .map_or(Cost::ZERO, ActiveTransferFunction::setup_cost)
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

    pub(crate) fn eval_1d(&self, function: &ShadingFunction, x: f64) -> [f32; 4] {
        self.eval_color(function, |out| function.eval_1d_f64(x, out))
    }

    pub(crate) fn eval_color(
        &self,
        function: &ShadingFunction,
        eval: impl FnOnce(&mut [f32]) -> Option<()>,
    ) -> [f32; 4] {
        let mut out = [0.0; MAX_COMPONENTS];
        let out = &mut out[..function.num_outputs()];

        match eval(out) {
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
        let (width, height) = texture_dimensions(bbox, scale);
        let grid_transform =
            Affine::translate((bbox.x0, bbox.y0)) * Affine::scale(1.0 / scale as f64);
        if let EncodedShadingType::RadialAxial(shading) = &self.shading_type {
            let sampling_transform = self.sampling_transform();
            shading.sample(width, height, sampling_transform * grid_transform, self, f);
            return (width, height, grid_transform);
        }
        let mut canvas = Canvas::new(
            width,
            height,
            self.finish(self.background_color.components()),
        );
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
            EncodedShadingType::Mesh(shading) => {
                draw_mesh(
                    &mut canvas,
                    &shading.shading_type,
                    transform,
                    self,
                    &mut Unmetered,
                )
                .expect("unmetered");
            }
            EncodedShadingType::Dummy(_) => {}
            EncodedShadingType::RadialAxial(_) => unreachable!(),
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
        scratch(meter, |meter| {
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
            let bg = self.admit_batch(count, meter)?;
            if let EncodedShadingType::Mesh(s) = &self.shading_type {
                take(meter, Cost::new(count))?;
                let mut canvas = Canvas::for_rows(width, height, bg);
                draw_mesh(
                    &mut canvas,
                    &s.shading_type,
                    grid.inverse() * self.base_transform.inverse(),
                    self,
                    meter,
                )?;
                canvas.emit_rows(bg, meter, f)?;
            } else {
                let sample = self.point_sampler(bg);
                let transform = self.sampling_transform() * grid;
                let mut row = vec![bg; width as usize];
                for y in 0..height {
                    for (x, pixel) in row.iter_mut().enumerate() {
                        *pixel = sample(transform * Point::new(x as f64 + 0.5, f64::from(y) + 0.5));
                    }
                    f(&row);
                }
            }
            Ok((width, height, grid))
        })
    }

    /// Sample the requested positions in order. A refused request publishes no samples.
    pub fn sample_points_metered(
        &self,
        points: &[Point],
        meter: &mut dyn Meter,
        mut f: impl FnMut([f32; 4]),
    ) -> Result<(), Exhausted> {
        scratch(meter, |meter| {
            if points.is_empty() {
                return Ok(());
            }
            let bg = self.admit_batch(points.len() as u64, meter)?;
            if let EncodedShadingType::Mesh(s) = &self.shading_type {
                let mut canvas = Canvas::for_points(points, bg, meter)?;
                draw_mesh(
                    &mut canvas,
                    &s.shading_type,
                    self.base_transform.inverse(),
                    self,
                    meter,
                )?;
                canvas.pixels.into_iter().for_each(f);
            } else {
                let sample = self.point_sampler(bg);
                let transform = self.sampling_transform();
                for point in points {
                    f(sample(transform * *point));
                }
            }
            Ok(())
        })
    }

    fn sampling_transform(&self) -> Affine {
        let EncodedShadingType::RadialAxial(shading) = &self.shading_type else {
            return self.base_transform;
        };
        match shading.params {
            RadialAxialParams::Axial {
                start,
                axis,
                sampling_transform,
                expected_base_transform,
            } => {
                if self.base_transform == expected_base_transform {
                    sampling_transform
                } else {
                    Affine::new([axis.x, axis.y, -axis.y, axis.x, start.x, start.y])
                        * self.base_transform
                }
            }
            RadialAxialParams::Radial { .. } => self.base_transform,
        }
    }
    fn admit_batch(&self, count: u64, meter: &mut dyn Meter) -> Result<[f32; 4], Exhausted> {
        let conversion = self.color_space.prepared();
        let work = if matches!(self.shading_type, EncodedShadingType::Mesh(_)) {
            Cost::ZERO
        } else {
            self.sample_cost(conversion.sample).times(count)
        };
        conversion.hold(meter)?;
        take(meter, conversion.setup + self.transfer_cost() + work)?;
        Ok(self.finish(self.background_color.components()))
    }

    /// Prepare reusable color tables for a nonempty placement.
    pub fn prepare_for(self, bbox: Rect) -> Self {
        if bbox.area() > 0.0 {
            self.color_space.prepare_tables();
            if let Some(tf) = &self.transfer_function {
                tf.apply(&AlphaColor::TRANSPARENT);
            }
        }
        self
    }

    fn point_sampler(&self, bg: [f32; 4]) -> impl Fn(Point) -> [f32; 4] + '_ {
        move |p| match &self.shading_type {
            EncodedShadingType::RadialAxial(s) => s.position(p).map_or(bg, |t| {
                let [lo, hi] = s.domain.map(f64::from);
                self.eval_1d(&s.function, lo + (hi - lo) * t)
            }),
            EncodedShadingType::FunctionBased(s) if s.domain.contains(p) => self
                .eval_color(&s.function, |out| {
                    s.function.eval(&[p.x as f32, p.y as f32], out)
                }),
            _ => bg,
        }
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
            let p = color_space.prepared();
            p.hold(meter)?;
            take(meter, p.setup + p.sample)?;
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
                let sampling_transform = self.matrix.inverse();
                let (encoded, initial_transform) = encode_axial_shading(
                    *coords,
                    *domain,
                    function,
                    *extend,
                    *axial,
                    sampling_transform,
                );

                base_transform = initial_transform * sampling_transform;

                encoded
            }
            ShadingType::TriangleMesh { .. } | ShadingType::PatchMesh { .. } => {
                base_transform = self.matrix.inverse();

                EncodedShadingType::Mesh(EncodedMeshShading {
                    shading_type: self.shading.shading_type.clone(),
                })
            }
            ShadingType::Dummy => {
                base_transform = Affine::IDENTITY;

                EncodedShadingType::Dummy(EncodedDummyShading)
            }
        };

        let color_space = self.shading.color_space.clone();

        let background_color = match (&shading_type, &self.shading.background) {
            (EncodedShadingType::Dummy(_), _) | (_, None) => AlphaColor::TRANSPARENT,
            (_, Some(background)) => color_space.to_rgba(background, 1.0),
        };

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
    coords: [f64; 6],
    domain: [f32; 2],
    function: &ShadingFunction,
    extend: [bool; 2],
    is_axial: bool,
    sampling_transform: Affine,
) -> (EncodedShadingType, Affine) {
    let initial_transform;

    let params = if is_axial {
        let [x_0, y_0, x_1, y_1, _, _] = coords;

        let start = Point::new(x_0, y_0);
        let axis = Point::new(x_1, y_1) - start;
        initial_transform =
            Affine::new([axis.x, axis.y, -axis.y, axis.x, start.x, start.y]).inverse();
        RadialAxialParams::Axial {
            start,
            axis,
            sampling_transform,
            expected_base_transform: initial_transform * sampling_transform,
        }
    } else {
        let [x_0, y_0, r0, x_1, y_1, r_1] = coords;

        initial_transform = Affine::translate((-x_0, -y_0));
        let new_x1 = x_1 - x_0;
        let new_y1 = y_1 - y_0;

        let p1 = Point::new(new_x1, new_y1);
        let r = Point::new(r0, r_1);

        RadialAxialParams::Radial { p1, r }
    };

    (
        EncodedShadingType::RadialAxial(EncodedRadialAxialShading {
            function: function.clone(),
            ramp: None,
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
        function: function.clone(),
    })
}

#[derive(Clone, Debug)]
pub(crate) enum RadialAxialParams {
    Axial {
        start: Point,
        axis: Vec2,
        sampling_transform: Affine,
        expected_base_transform: Affine,
    },
    Radial {
        p1: Point,
        r: Point,
    },
}

impl EncodedRadialAxialShading {
    /// Return a single device row or column when the axial parameter is invariant on that axis.
    pub fn texture_bounds(&self, bbox: Rect, pattern: &EncodedShadingPattern) -> Rect {
        let RadialAxialParams::Axial {
            sampling_transform,
            expected_base_transform,
            ..
        } = self.params
        else {
            return bbox;
        };
        if pattern.base_transform != expected_base_transform {
            return bbox;
        }
        if self.invariant_y(sampling_transform) {
            Rect::new(bbox.x0, bbox.y0, bbox.x1, bbox.y0 + 1.)
        } else if self.invariant_x(sampling_transform) {
            Rect::new(bbox.x0, bbox.y0, bbox.x0 + 1., bbox.y1)
        } else {
            bbox
        }
    }

    fn sample(
        &self,
        width: u32,
        height: u32,
        transform: Affine,
        pattern: &EncodedShadingPattern,
        mut emit: impl FnMut([f32; 4]),
    ) {
        let background = pattern.finish(pattern.background_color.components());
        let [start, end] = self.domain.map(f64::from);
        let (mut start_color, mut end_color) = (None, None);
        let mut sample = |x: u32, y: u32| {
            let point = transform * Point::new(x as f64 + 0.5, y as f64 + 0.5);
            self.position(point)
                .map(|t| match t {
                    0.0 => {
                        *start_color.get_or_insert_with(|| pattern.eval_1d(&self.function, start))
                    }
                    1.0 => *end_color.get_or_insert_with(|| pattern.eval_1d(&self.function, end)),
                    _ => self
                        .ramp
                        .as_ref()
                        .and_then(|r| r.get(t))
                        .unwrap_or_else(|| {
                            pattern.eval_1d(&self.function, start + (end - start) * t)
                        }),
                })
                .unwrap_or(background)
        };
        if height > 1 && self.invariant_y(transform) {
            let row = (0..width).map(|x| sample(x, 0)).collect::<Vec<_>>();
            for _ in 0..height {
                row.iter().copied().for_each(&mut emit);
            }
        } else if self.invariant_x(transform) {
            for y in 0..height {
                std::iter::repeat_n(sample(0, y), width as usize).for_each(&mut emit);
            }
        } else {
            for y in 0..height {
                for x in 0..width {
                    emit(sample(x, y));
                }
            }
        }
    }

    pub(crate) fn invariant_y(&self, transform: Affine) -> bool {
        let [_, _, c, d, ..] = transform.as_coeffs();
        matches!(self.params, RadialAxialParams::Axial { axis, .. } if (axis.x == 0. || c == 0.) && (axis.y == 0. || d == 0.))
    }

    pub(crate) fn invariant_x(&self, transform: Affine) -> bool {
        let [a, b, ..] = transform.as_coeffs();
        matches!(self.params, RadialAxialParams::Axial { axis, .. } if (axis.x == 0. || a == 0.) && (axis.y == 0. || b == 0.))
    }

    fn position(&self, pos: Point) -> Option<f64> {
        let t = match self.params {
            RadialAxialParams::Axial { start, axis, .. } => (pos - start).dot(axis) / axis.hypot2(),
            RadialAxialParams::Radial { p1, r } => {
                return radial_pos(pos, p1, r, self.extend).map(|t| t.clamp(0.0, 1.0));
            }
        };

        if !t.is_finite() {
            return None;
        }
        if t < 0.0 {
            self.extend[0].then_some(0.0)
        } else if t > 1.0 {
            self.extend[1].then_some(1.0)
        } else {
            Some(t)
        }
    }
}

fn radial_pos(pos: Point, p1: Point, r: Point, extend: [bool; 2]) -> Option<f64> {
    if r.x == 0.0 && r.y == 0.0 {
        return None;
    }
    let dr = r.y - r.x;
    let r0 = r.x;
    let a = norm_squared_minus_square(p1, dr);
    let bx = pos.x * p1.x;
    let by = pos.y * p1.y;
    let br = r0 * dr;
    // Add the smallest product last so cancellation can expose it.
    let b = if bx.abs() <= by.abs() && bx.abs() <= br.abs() {
        (by + br) + bx
    } else if by.abs() <= br.abs() {
        (bx + br) + by
    } else {
        (bx + by) + br
    };
    let c = || norm_squared_minus_square(pos, r.x);
    let valid = |t: f64| (extend[0] || t >= 0.0) && (extend[1] || t <= 1.0) && r.x + dr * t >= 0.0;

    if a == 0.0 {
        if b == 0.0 {
            return [1.0, 0.0].into_iter().find(|&t| c() == 0.0 && valid(t));
        }
        let t = c() / (2.0 * b);
        return valid(t).then_some(t);
    }

    let (dx, dy) = (p1.x, p1.y);
    let (x, y) = (pos.x, pos.y);
    let u = r0 * dx + dr * x;
    let v = r0 * dy + dr * y;
    // Factor u² + v² − (x*dy − y*dx)² before the cross product loses its smaller term.
    let d = if u.abs() >= v.abs() {
        (dx * (r0 - y) + x * (dr + dy)) * (dx * (r0 + y) + x * (dr - dy)) + v * v
    } else {
        (dy * (r0 + x) + y * (dr - dx)) * (dy * (r0 - x) + y * (dr + dx)) + u * u
    };
    if d < 0.0 {
        return None;
    }
    let sqrt_d = d.sqrt();

    // Add terms of the same sign, then recover the other root from their product c/a.
    let q = b + sqrt_d.copysign(b);

    if q == 0.0 {
        return (c() == 0.0 && valid(0.0)).then_some(0.0);
    }

    let (max, min) = if q.is_sign_negative() == a.is_sign_negative() {
        (q / a, c() / q)
    } else {
        (c() / q, q / a)
    };
    if valid(max) {
        Some(max)
    } else {
        valid(min).then_some(min)
    }
}

fn norm_squared_minus_square(p: Point, r: f64) -> f64 {
    let (major, minor) = if p.x.abs() >= p.y.abs() {
        (p.x.abs(), p.y)
    } else {
        (p.y.abs(), p.x)
    };
    (major - r.abs()) * (major + r.abs()) + minor * minor
}

#[cfg(test)]
mod radial_tests {
    use super::*;
    use crate::cache::Cache;
    use hayro_syntax::object::{Dict, FromBytes};

    #[test]
    fn zero_radius_pattern_and_shading_operator() {
        use crate::font::GlyphRun;
        use crate::pattern::Pattern;
        use crate::{
            BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps,
            InterpreterCache, Paint, SoftMask, interpret_page,
        };
        use hayro_syntax::Pdf;
        use kurbo::BezPath;

        struct Samples(Vec<Vec<u8>>);
        impl Device<'_> for Samples {
            fn draw_path(&mut self, _: &BezPath, props: DrawProps<'_>, _: &DrawMode) {
                let Paint::Pattern(pattern) = props.paint else {
                    panic!("expected pattern paint")
                };
                let Pattern::Shading(pattern) = *pattern else {
                    panic!("expected shading pattern")
                };
                self.0.push(
                    pattern
                        .encode()
                        .render_texture(Rect::new(0.0, 0.0, 1.0, 1.0), 1.0)
                        .data,
                );
            }
            fn push_clip_path(&mut self, _: &ClipPath) {}
            fn push_transparency_group(
                &mut self,
                _: f32,
                _: Option<SoftMask<'_>>,
                _: BlendMode,
                _: bool,
                _: bool,
            ) {
            }
            fn draw_glyph_run(&mut self, _: &GlyphRun<'_, '_>, _: DrawProps<'_>, _: &DrawMode) {}
            fn draw_image(&mut self, _: Image<'_, '_>, _: ImageDrawProps<'_>) {}
            fn pop_clip(&mut self) {}
            fn pop_transparency_group(&mut self) {}
        }

        let ops = "/Pattern cs /P scn 0 0 1 1 re f /S sh";
        let pdf = format!("%PDF-1.7
1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj
2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj
3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] /Contents 4 0 R
/Resources << /Pattern << /P << /PatternType 2 /Shading 5 0 R >> >> /Shading << /S 5 0 R >> >> >> endobj
4 0 obj << /Length {} >> stream
{ops}
endstream endobj
5 0 obj << /ShadingType 3 /ColorSpace /DeviceRGB /Background [1 0 0]
/Coords [0 0.5 0 1 0.5 0] /Function << /FunctionType 2 /Domain [0 1] /C0 [0 0 0] /C1 [1 1 1] /N 1 >> >> endobj
trailer << /Root 1 0 R >>
%%EOF", ops.len());
        let pdf = Pdf::new(pdf.into_bytes()).unwrap();
        let mut context = Context::new(
            Affine::IDENTITY,
            Rect::new(0.0, 0.0, 1.0, 1.0),
            &InterpreterCache::new(),
            pdf.xref(),
            crate::InterpreterSettings::default(),
        );
        let mut samples = Samples(Vec::new());
        interpret_page(&pdf.pages()[0], &mut context, &mut samples);
        assert_eq!(samples.0, [[255, 0, 0, 255], [0, 0, 0, 0]]);
        assert_eq!(
            radial_pos(
                Point::new(0.5, 0.0),
                Point::new(1.0, 0.0),
                Point::ZERO,
                [false; 2]
            ),
            None
        );
    }

    #[test]
    fn radial_precision() {
        let sample = |coords: [f64; 6], pos: Point, extend: [bool; 2]| {
            let [x0, y0, r0, x1, y1, r1] = coords;
            let [start, end] = extend;
            let data = format!(
                "<< /Shading << /ShadingType 3 /ColorSpace /DeviceGray /Coords [{x0} {y0} {r0} {x1} {y1} {r1}] /Extend [{start} {end}] /Function << /FunctionType 2 /Domain [0 1] /N 1 >> >> >>"
            );
            let dict = Dict::from_bytes(data.as_bytes()).unwrap();
            let pattern = ShadingPattern::new(&dict, &Cache::new()).unwrap().encode();
            match pattern.shading_type {
                EncodedShadingType::Dummy(_) => None,
                EncodedShadingType::RadialAxial(shading) => shading
                    .position(pattern.base_transform * pos)
                    .map(|t| t as f32),
                _ => unreachable!(),
            }
        };
        for (coords, x, y, expected) in [
            ([0.0, 0.5, 0.0, 8193.0, 0.5, 0.0], 0.5, 0.5, None),
            (
                [-1073741824.0, 0.0, 1.0, 1073741824.0, 0.0, 1.0],
                0.5,
                2.5,
                None,
            ),
            ([0.0, 0.0, 4.0, 3.0, 4.0, 7.0], 4.0, -3.0, Some(0.75)),
            (
                [0.0, 0.0, 0.0, 134217728.0, 1.0, 134217728.0],
                0.0,
                0.75,
                Some(0.75),
            ),
            (
                [0.0, 0.0, 0.0, 134217728.0, 1.0, 134217728.0],
                2_f64.powi(-31),
                0.5,
                Some(0.8201941),
            ),
            ([0.0, 0.0, 3.0, 1.0, 1.0, 3.0], -2.0, 1.0, Some(1.0)),
            (
                [0.0, 0.0, 3.0, 1.0, 1.0, 3.0],
                -2.001,
                1.0,
                Some(0.99899983),
            ),
            ([0.0, 0.0, 3.0, 1.0, 1.0, 3.0], -1.999, 1.0, None),
            (
                [0.0, 0.0, 2.5, 64.1, 0.0, 2.5],
                0.5,
                2.5,
                Some((0.5 / 64.1) as f32),
            ),
            ([0.0, 0.0, 2.5, 64.1, 0.0, 2.5], 0.5, 2.501, None),
            ([0.0, 0.0, 1.0, 1.0, 0.0, 2.0], -1.0, 0.0, Some(1.0)),
        ] {
            let [x0, y0, r0, x1, y1, r1] = coords;
            assert_eq!(sample(coords, Point::new(x, y), [false; 2]), expected);
            assert_eq!(
                sample([y0, x0, r0, y1, x1, r1], Point::new(y, x), [false; 2]),
                expected
            );
        }
        for extend in [[true, false], [false, true], [true; 2]] {
            assert_eq!(
                sample(
                    [0.0, 0.0, 1.0, 1.0, 0.0, 2.0],
                    Point::new(-1.0, 0.0),
                    extend
                ),
                Some(1.0)
            );
        }
        for extend in [[false; 2], [true, false], [false, true], [true; 2]] {
            let l = 134217728.0;
            for (y, expected) in [(0.5, Some(0.5)), (-0.5, extend[0].then_some(0.0))] {
                assert_eq!(
                    sample([0.0, 0.0, l, l, 1.0, 0.0], Point::new(l, y), extend),
                    expected
                );
                assert_eq!(
                    sample([0.0, 0.0, l, 1.0, l, 0.0], Point::new(y, l), extend),
                    expected
                );
            }
            let coords = [0.0, 0.0, 3.0, 0.0, 0.0, 1.0];
            for (x, expected) in [
                (4.0, extend[0].then_some(0.0)),
                (0.5, extend[1].then_some(1.0)),
                (2.0, Some(0.5)),
            ] {
                assert_eq!(sample(coords, Point::new(x, 0.0), extend), expected);
            }
        }
        for scale in [1e-30, 1e30] {
            let t = radial_pos(
                Point::new(scale, 0.0),
                Point::new(2.0 * scale, 0.0),
                Point::new(scale / 2.0, scale / 2.0),
                [false; 2],
            )
            .unwrap();
            assert!((t - 0.75).abs() < 1e-14);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::shading::Shading;
    use hayro_syntax::object::{Dict, FromBytes};

    #[test]
    fn quarter_turn_axial_texture() {
        let bbox = Rect::new(0., 0., 4., 5.);
        for (coords, expected, horizontal) in [
            ("0 0 8 0", Rect::new(0., 0., 1., 5.), true),
            ("0 0 0 -8", Rect::new(0., 0., 4., 1.), false),
        ] {
            let source = format!(
                "<< /ShadingType 2 /Coords [{coords}] /ColorSpace /DeviceGray
                /Function << /FunctionType 2 /Domain [0 1] /N 1 >> >>"
            );
            let dict = Dict::from_bytes(source.as_bytes()).unwrap();
            let pattern = ShadingPattern {
                shading: Arc::new(Shading::new(&dict, None, &Cache::default()).unwrap()),
                matrix: Affine::new([0., 1., -1., 0., 0., 0.]),
                opacity: 1.,
                transfer_function: None,
            }
            .encode();
            let EncodedShadingType::RadialAxial(g) = &pattern.shading_type else {
                unreachable!()
            };
            assert_eq!(g.texture_bounds(bbox, &pattern), expected);
            let mut pixels = Vec::new();
            pattern.sample_texture(bbox, 1., |c| pixels.push(c));
            for y in 0..5 {
                for x in 0..4 {
                    let t = (if horizontal { y } else { x } as f64 + 0.5) / 8.;
                    assert_eq!(pixels[y * 4 + x], pattern.eval_1d(&g.function, t));
                }
            }
        }
    }

    #[test]
    fn axial_base_transform_normalizes_endpoints() {
        let dict = Dict::from_bytes(b"<< /ShadingType 2 /Coords [4 2 8 2] /ColorSpace /DeviceGray /Function << /FunctionType 2 /Domain [0 1] /N 1 >> >>").unwrap();
        let pattern = ShadingPattern {
            shading: Arc::new(Shading::new(&dict, None, &Cache::default()).unwrap()),
            matrix: Affine::IDENTITY,
            opacity: 1.0,
            transfer_function: None,
        }
        .encode();
        assert_eq!(pattern.base_transform * Point::new(4.0, 2.0), Point::ZERO);
        assert_eq!(
            pattern.base_transform * Point::new(8.0, 2.0),
            Point::new(1.0, 0.0)
        );
    }
}
