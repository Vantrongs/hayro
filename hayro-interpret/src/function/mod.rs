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
pub(crate) use shape::{Line, Side, Span};
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

/// What evaluating a function once may take, and walking its shape, found once
/// when it is read.
#[derive(Debug, Clone, Copy)]
struct Costs {
    eval: Cost,
    /// The steps one [`Function::span`] takes.
    span: Cost,
    /// The most spans above an input [`shape::over`] finds, over any interval:
    /// lines and curves, those past either side of the domain among them.
    spans: u64,
}

/// The most spans of a function whose own lines, between the sides of its
/// domain, are `lines` and whose outputs are `outputs`, once clipped to its
/// range, a line crossing each side of each output's range at most once,
/// with the two past the sides of its domain.
fn clipped_spans(lines: u64, outputs: u64) -> u64 {
    lines
        .saturating_mul(outputs.saturating_mul(2).saturating_add(1))
        .saturating_add(2)
}

/// The steps a function's own part of [`Function::span`] takes for `outputs`
/// outputs: finding its line and clipping it to the range.
fn span_steps(outputs: u64) -> Cost {
    Cost::new(outputs.saturating_mul(8).saturating_add(16))
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
        let curve = |eval: Cost| Costs {
            eval,
            span: eval + Cost::new(4),
            spans: 3,
        };
        let cost = match &function_type {
            FunctionType::Type0(t0) => match t0.sizes() {
                [n] => {
                    let eval = t0.cost() + span_steps(t0.outputs());
                    Costs {
                        eval,
                        span: eval + span_steps(t0.outputs()),
                        spans: clipped_spans(u64::from(*n).saturating_add(2), t0.outputs()),
                    }
                }
                _ => curve(t0.cost()),
            },
            FunctionType::Type2(t2) if t2.straight() => {
                let eval = t2.cost() + span_steps(t2.outputs());
                Costs {
                    eval,
                    span: eval + span_steps(t2.outputs()),
                    spans: clipped_spans(1, t2.outputs()),
                }
            }
            FunctionType::Type2(t2) => curve(t2.cost()),
            FunctionType::Type3(t3) => t3.cost(),
            FunctionType::Type4(t4) => curve(t4.cost()),
        };
        Self(Arc::new((function_type, cost)))
    }

    /// The most steps one [`eval`](Self::eval) takes: an operator run, or a value
    /// read, computed or written, whatever the input. Known before evaluating, so a
    /// caller can bound the work of evaluating many times.
    pub fn cost(&self) -> Cost {
        self.0.1.eval
    }

    /// The steps [`shape::over`] takes, over any interval.
    pub(crate) fn shape_cost(&self) -> Cost {
        // A span above each input it walks from, and one at each end of each.
        self.0.1.span.times(self.0.1.spans.saturating_mul(3))
    }

    /// The most points [`shape::over`] holds, over any interval: three where
    /// two spans meet.
    pub(crate) fn shape_points(&self) -> u64 {
        self.0.1.spans.saturating_add(1).saturating_mul(3)
    }

    /// The most steps one [`span`](Self::span) takes.
    fn span_cost(&self) -> Cost {
        self.0.1.span
    }

    /// The most spans [`shape::over`] finds of it.
    fn spans(&self) -> u64 {
        self.0.1.spans
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

    /// What the function of its first input is on `side` of `x` as it
    /// evaluates ([`shape::Span`]): a line where it is straight there, as a
    /// sampled function of one input is between its samples, an exponential
    /// one of exponent one is, and a stitching of such is between its bounds,
    /// each ending where the function bends or steps; elsewhere a curve. Past
    /// either side of its domain, and at a side on the side past it, the line
    /// of its value at that side, to infinity, as it clamps its input there.
    /// Its evaluation at `x` is the value at `x` of its span there on
    /// [`Side::At`] where that is a line, so the two agree.
    pub(crate) fn span(&self, x: f32, side: Side) -> Span {
        let Some((d0, d1)) = self.domain().filter(|(d0, d1)| d0 <= d1) else {
            return Span::Curve(f32::NEG_INFINITY, f32::INFINITY);
        };
        let constant = |a: f32, b: f32, at: f32| match self.eval(smallvec::smallvec![at]) {
            Some(value) => Span::Line(Line::constant(a, b, value)),
            None => Span::Curve(a, b),
        };
        if x < d0 || x == d0 && side == Side::Below {
            return constant(f32::NEG_INFINITY, d0, d0);
        }
        if x > d1 || x == d1 && side == Side::Above {
            return constant(d1, f32::INFINITY, d1);
        }
        if d0 == d1 {
            return constant(d0, d1, d0);
        }
        match &self.0.0 {
            FunctionType::Type0(t0) => t0.span(x, side),
            FunctionType::Type2(t2) => t2.span(x, side),
            FunctionType::Type3(t3) => t3.span(x, side),
            FunctionType::Type4(_) => Span::Curve(d0, d1),
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
