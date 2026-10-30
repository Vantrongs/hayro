use crate::encode::Cost;
use crate::function::shape::{Line, Side, Span};
use crate::function::{Clamper, Values};
use hayro_syntax::object::Dict;
use hayro_syntax::object::Number;
use hayro_syntax::object::dict::keys::{C0, C1, N};
use smallvec::{SmallVec, smallvec};

/// A type 2 function (exponential function).
#[derive(Debug)]
pub(crate) struct Type2 {
    c0: Values,
    delta: Values,
    pub(super) clamper: Clamper,
    n: f32,
}

impl Type2 {
    /// Create a new type 2 function.
    pub(crate) fn new(dict: &Dict<'_>) -> Option<Self> {
        let c0 = dict.get::<Values>(C0).unwrap_or(smallvec![0.0]);
        let c1 = dict.get::<Values>(C1).unwrap_or(smallvec![1.0]);
        let delta = c0.iter().zip(&c1).map(|(c0, c1)| c1 - c0).collect();
        let clamper = Clamper::new(dict)?;
        let n = dict.get::<Number>(N)?.as_f64() as f32;

        Some(Self {
            c0,
            delta,
            clamper,
            n,
        })
    }

    /// The number of outputs.
    pub(crate) fn outputs(&self) -> u64 {
        self.c0.len() as u64
    }

    /// The most steps `eval` takes: the power, and a step an output.
    pub(crate) fn cost(&self) -> Cost {
        Cost::new(2) + Cost::of(self.c0.len()).times(2) + self.clamper.cost()
    }

    /// Whether it is straight, of exponent one.
    pub(crate) fn straight(&self) -> bool {
        self.n == 1.0
    }

    /// The function on `side` of `x`, within its domain
    /// ([`Function::span`](crate::Function)): of exponent one, the straight line
    /// from one side of its domain to the other, clipped to its range
    /// ([`Line::clipped`]); otherwise a curve.
    pub(crate) fn span(&self, x: f32, side: Side) -> Span {
        let (d0, d1) = self.clamper.domain.first().copied().unwrap_or((0.0, 1.0));
        if !self.straight() || d0 > d1 {
            return Span::Curve(d0, d1);
        }
        Span::Line(self.line(x, side))
    }

    /// Of exponent one, the line it is on at `side` of `x`, within its domain.
    fn line(&self, x: f32, side: Side) -> Line {
        let (d0, d1) = self.clamper.domain.first().copied().unwrap_or((0.0, 1.0));
        let at = |x: f32| -> Values {
            self.c0
                .iter()
                .zip(&self.delta)
                .map(|(c0, delta)| *c0 + x * *delta)
                .collect()
        };
        Line {
            x0: d0,
            x1: d1,
            v0: at(d0),
            v1: at(d1),
        }
        .clipped(self.clamper.range.as_deref(), x, side)
    }

    /// Evaluate the function with the given input.
    pub(crate) fn eval(&self, input: f32) -> Values {
        let mut input = [input];
        self.clamper.clamp_input(&mut input);
        if self.straight() {
            // The value at it of its line there, as its shape is.
            let x = input[0];
            return self.line(x, Side::At).at(x);
        }
        let factor = input[0].powf(self.n);

        let mut out = self
            .c0
            .iter()
            .zip(&self.delta)
            .map(|(c0, delta)| *c0 + factor * *delta)
            .collect::<SmallVec<_>>();

        self.clamper.clamp_output(&mut out);

        out
    }
}

#[cfg(test)]
mod tests {
    use crate::function::Function;

    use hayro_syntax::object::Object;
    use hayro_syntax::object::{Dict, FromBytes};
    use smallvec::smallvec;

    #[test]
    fn simple() {
        let func = Function::new(&Object::Dict(
            Dict::from_bytes(
                b"<<
              /FunctionType 2
              /Domain [ 0  1 ]
              /C0 [ 0 20  ]
              /C1 [ 30 -50 ]
              /N 1
            >>",
            )
            .unwrap(),
        ))
        .unwrap();

        assert_eq!(func.eval(smallvec![0.0]).unwrap().as_ref(), &[0.0, 20.0]);
        assert_eq!(func.eval(smallvec![0.5]).unwrap().as_ref(), &[15.0, -15.0]);
        assert_eq!(func.eval(smallvec![1.0]).unwrap().as_ref(), &[30.0, -50.0]);
    }

    #[test]
    fn with_exponent() {
        let func = Function::new(&Object::Dict(
            Dict::from_bytes(
                b"<<
              /FunctionType 2
              /Domain [ 0  1 ]
              /C0 [ 0  ]
              /C1 [ 30 ]
              /N 2
            >>",
            )
            .unwrap(),
        ))
        .unwrap();

        assert_eq!(func.eval(smallvec![0.5]), Some(smallvec![7.5]));
    }

    #[test]
    fn clamp_domain() {
        let func = Function::new(&Object::Dict(
            Dict::from_bytes(
                b"<<
              /FunctionType 2
              /Domain [ 0.2  0.8 ]
              /C0 [ 0  ]
              /C1 [ 30 ]
              /N 2
            >>",
            )
            .unwrap(),
        ))
        .unwrap();

        assert_eq!(
            func.eval(smallvec![0.0]).as_ref(),
            func.eval(smallvec![0.2]).as_ref(),
        );
        assert_eq!(
            func.eval(smallvec![-10.]).as_ref(),
            func.eval(smallvec![0.2]).as_ref(),
        );
        assert_eq!(
            func.eval(smallvec![0.8]).as_ref(),
            func.eval(smallvec![0.8]).as_ref(),
        );
        assert_eq!(
            func.eval(smallvec![1.2]).as_ref(),
            func.eval(smallvec![1.0]).as_ref(),
        );
    }

    #[test]
    fn clamp_range() {
        let func = Function::new(&Object::Dict(
            Dict::from_bytes(
                b"<<
              /FunctionType 2
              /Domain [ 0.0  1.0 ]
              /Range [10.0 20.0]
              /C0 [ 0  ]
              /C1 [ 30 ]
              /N 1
            >>",
            )
            .unwrap(),
        ))
        .unwrap();

        assert_eq!(func.eval(smallvec![0.0]), Some(smallvec![10.0]));
        assert_eq!(func.eval(smallvec![0.5]), Some(smallvec![15.0]));
        assert_eq!(func.eval(smallvec![1.0]), Some(smallvec![20.0]));
    }
}
