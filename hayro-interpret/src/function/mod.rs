//! PDF functions.
//!
//! PDF has the concept of functions, representing objects that take a certain number of values
//! as input, do some processing on them and then return some output.

pub(crate) mod shape;
mod type0;
mod type2;
mod type3;
mod type4;

use crate::encode::Cost;
use crate::function::type0::Type0;
use crate::function::type2::Type2;
use crate::function::type3::Type3;
use crate::function::type4::Type4;
use hayro_syntax::object::Dict;
use hayro_syntax::object::dict::keys::{DOMAIN, FUNCTION_TYPE, RANGE};
use hayro_syntax::object::{Object, dict_or_stream};
use smallvec::SmallVec;
use std::sync::{Arc, OnceLock};
#[cfg(test)]
pub(crate) use type0::EVALS as TYPE0_EVALS;
#[cfg(test)]
pub(crate) use type3::VISITED as TYPE3_VISITED;
#[cfg(feature = "count-operators")]
pub use type4::operators_run;

/// The input/output type of functions.
pub(crate) type Values = SmallVec<[f32; 4]>;
type TupleVec = SmallVec<[(f32, f32); 4]>;

#[derive(Debug)]
enum FunctionType {
    Type0(Type0),
    Type2(Type2),
    Type3(Type3),
    Type4(Type4),
}

/// The steps sorting the values gathering them took `gathered` steps for.
pub(crate) fn sorting(gathered: Cost) -> Cost {
    let n = gathered.steps();
    gathered.times(u64::from(u64::BITS - n.leading_zeros()).max(1))
}

/// A PDF function.
#[derive(Debug, Clone)]
pub struct Function(Arc<(FunctionType, Costs)>);

/// What evaluating a function once may take, found once when it is read.
#[derive(Debug, Clone, Copy)]
struct Costs {
    eval: Cost,
    /// The steps [`Function::shape`] takes, and the most points it holds.
    shape: Cost,
    points: u64,
}

/// The most points `shape::over` adds to a function's own: two at each side.
pub(crate) const OVER_POINTS_COUNT: u64 = 4;

/// The most points the shape of a function with `samples` points of its own and
/// `outputs` outputs holds once clipped to its range, a line crossing each side
/// of each output's range at most once, and the steps finding them takes: a few
/// for each output of each point.
fn clipped_shape(samples: u64, outputs: u64) -> (u64, Cost) {
    let points = samples.saturating_mul(outputs.saturating_mul(2).saturating_add(1));
    (
        points,
        Cost::new(points.saturating_mul(outputs.saturating_add(4))),
    )
}

impl Function {
    /// Create a new function.
    pub fn new(obj: &Object<'_>) -> Option<Self> {
        let (dict, stream) = dict_or_stream(obj)?;

        let function_type = match dict.get::<u8>(FUNCTION_TYPE)? {
            0 => FunctionType::Type0(Type0::new(stream?)?),
            2 => FunctionType::Type2(Type2::new(dict)?),
            3 => FunctionType::Type3(Type3::new(dict)?),
            4 => FunctionType::Type4(Type4::new(stream?)?),
            _ => return None,
        };
        Some(Self::of(function_type))
    }

    /// The function of `function_type`, its cost found once.
    fn of(function_type: FunctionType) -> Self {
        let curve = (0, Cost::new(1));
        let cost = match &function_type {
            FunctionType::Type0(t0) => {
                let (points, shape) = match t0.sizes() {
                    [n] => clipped_shape(u64::from(*n).saturating_add(2), t0.outputs()),
                    _ => curve,
                };
                Costs {
                    eval: t0.cost(),
                    shape,
                    points,
                }
            }
            FunctionType::Type2(t2) => {
                let (points, shape) = clipped_shape(2, t2.outputs());
                Costs {
                    eval: t2.cost(),
                    shape,
                    points,
                }
            }
            FunctionType::Type3(t3) => t3.cost(),
            FunctionType::Type4(t4) => Costs {
                eval: t4.cost(),
                shape: curve.1,
                points: curve.0,
            },
        };
        Self(Arc::new((function_type, cost)))
    }

    /// The most steps one [`eval`](Self::eval) takes: an operator run, or a value
    /// read, computed or written, whatever the input. Known before evaluating, so a
    /// caller can bound the work of evaluating many times.
    pub fn cost(&self) -> Cost {
        self.0.1.eval
    }

    /// The steps [`shape`](Self::shape) takes.
    pub(crate) fn shape_cost(&self) -> Cost {
        self.0.1.shape
    }

    /// The most points [`shape`](Self::shape) holds.
    pub(crate) fn shape_points(&self) -> u64 {
        self.0.1.points
    }

    /// The domain of its first input, `None` where it has none.
    pub(crate) fn domain(&self) -> Option<(f32, f32)> {
        let clamper = match &self.0.0 {
            FunctionType::Type0(t0) => &t0.clamper,
            FunctionType::Type2(t2) => &t2.clamper,
            FunctionType::Type3(t3) => &t3.clamper,
            FunctionType::Type4(t4) => &t4.clamper,
        };
        clamper.domain.first().copied()
    }

    /// The function of its first input over its domain as it evaluates, piece by
    /// piece ([`shape::Piece`]): straight lines where it is made of them, as a
    /// sampled function of one input, an exponential one of exponent one, and a
    /// stitching of such are, each step of a stitching at its bound; elsewhere
    /// curves.
    pub(crate) fn shape(&self) -> Vec<shape::Piece> {
        match &self.0.0 {
            FunctionType::Type0(t0) => vec![t0.shape()],
            FunctionType::Type2(t2) => vec![t2.shape()],
            FunctionType::Type3(t3) => t3.shape(),
            FunctionType::Type4(t4) => {
                let (d0, d1) = t4.clamper.domain.first().copied().unwrap_or((0.0, 1.0));
                vec![shape::Piece::Curve(d0, d1)]
            }
        }
    }

    /// Evaluate the function with the given input.
    pub fn eval(&self, input: Values) -> Option<Values> {
        match &self.0.0 {
            FunctionType::Type0(t0) => t0.eval(input),
            FunctionType::Type2(t2) => Some(t2.eval(*input.first()?)),
            FunctionType::Type3(t3) => t3.eval(*input.first()?),
            FunctionType::Type4(t4) => Some(t4.eval(input)?),
        }
    }
}

/// A transfer function.
#[derive(Clone, Debug)]
pub struct TransferFunction {
    function: Function,
    samples: Arc<OnceLock<[u8; 256]>>,
}

impl TransferFunction {
    pub(crate) fn new(function: Function) -> Self {
        Self {
            function,
            samples: Arc::new(OnceLock::new()),
        }
    }

    /// Apply the transfer function to a buffer of values.
    pub fn apply_to(&self, values: &mut [u8]) {
        self.apply_to_stride(values, 1);
    }

    pub(crate) fn apply_to_stride(&self, values: &mut [u8], stride: usize) {
        let samples = self.samples();

        for value in values.iter_mut().step_by(stride) {
            *value = samples[*value as usize];
        }
    }

    pub(crate) fn apply_f32(&self, value: f32) -> f32 {
        self.samples()[(value * 255.0 + 0.5) as u8 as usize] as f32 / 255.0
    }

    /// The steps building the table [`apply_to`](Self::apply_to) maps by takes, until
    /// it is built.
    pub fn setup_cost(&self) -> Cost {
        if self.samples.get().is_some() {
            Cost::ZERO
        } else {
            (self.function.cost() + Cost::new(1)).times(256)
        }
    }

    pub(crate) fn samples(&self) -> &[u8; 256] {
        self.samples.get_or_init(|| {
            std::array::from_fn(|sample| {
                self.function
                    .eval(smallvec::smallvec![sample as f32 / 255.0])
                    .and_then(|output| output.first().copied())
                    .map(|output| (output * 255.0 + 0.5) as u8)
                    .unwrap_or(sample as u8)
            })
        })
    }
}

#[derive(Debug, Clone)]
struct Clamper {
    domain: TupleVec,
    range: Option<TupleVec>,
}

impl Clamper {
    /// The steps clamping an input and an output takes.
    fn cost(&self) -> Cost {
        Cost::of(self.domain.len()) + Cost::of(self.range.as_ref().map_or(0, |range| range.len()))
    }

    fn new(dict: &Dict<'_>) -> Option<Self> {
        let domain = dict.get::<TupleVec>(DOMAIN)?;
        let range = dict.get::<TupleVec>(RANGE);

        Some(Self { domain, range })
    }

    fn clamp_input(&self, input: &mut [f32]) {
        if input.len() != self.domain.len() {
            warn!("the domain of the function didn't match the input arguments");
        }

        for ((min, max), val) in self.domain.iter().zip(input.iter_mut()) {
            *val = val.min(*max).max(*min);
        }
    }

    fn clamp_output(&self, output: &mut [f32]) {
        if let Some(range) = &self.range {
            if range.len() != output.len() {
                warn!("the range of the function didn't match the output arguments");
            }

            for ((min, max), val) in range.iter().zip(output.iter_mut()) {
                *val = val.min(*max).max(*min);
            }
        }
    }
}

/// Linearly interpolate the value `x`, assuming that it lies within the range `x_min` and `x_max`,
/// to the range `y_min` and `y_max`.
pub(crate) fn interpolate(x: f32, x_min: f32, x_max: f32, y_min: f32, y_max: f32) -> f32 {
    y_min + (x - x_min) * (y_max - y_min) / (x_max - x_min)
}
