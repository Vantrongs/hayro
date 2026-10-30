use crate::encode::Cost;
use crate::function::shape::{Side, Span};
use crate::function::{
    Clamper, Costs, Function, TupleVec, Values, clipped_spans, interpolate, span_steps,
};
use hayro_syntax::object::Array;
use hayro_syntax::object::Dict;
use hayro_syntax::object::Object;
use hayro_syntax::object::dict::keys::{BOUNDS, ENCODE, FUNCTIONS};
use smallvec::smallvec;

#[cfg(test)]
thread_local! {
    /// The spans of a function stitched `span` has asked for.
    pub(crate) static VISITED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A type 3 function (stitching function).
#[derive(Debug)]
pub(crate) struct Type3 {
    functions: Vec<Function>,
    bounds: Vec<f32>,
    encode: TupleVec,
    pub(super) clamper: Clamper,
}

impl Type3 {
    /// Create a new type 3 function.
    pub(crate) fn new(dict: &Dict<'_>) -> Option<Self> {
        let clamper = Clamper::new(dict)?;

        let mut functions: Vec<Function> = dict.get::<Array<'_>>(FUNCTIONS).and_then(|d| {
            d.items::<Object<'_>>()
                .map_while(|o| o)
                .map(|o| Function::new(&o))
                .collect()
        })?;
        let domain = *clamper.domain.first()?;
        let mut bounds = vec![domain.0];
        if let Some(a) = dict.get::<Array<'_>>(BOUNDS) {
            bounds.extend(a.items::<f32>().map_while(|n| n));
        }
        bounds.push(domain.1);

        let encode = dict.get::<TupleVec>(ENCODE)?;
        // A function past the last interval the bounds make, or past the last
        // encoding, is never evaluated: kept, it would only be walked over.
        functions.truncate((bounds.len() - 1).min(encode.len()));

        Some(Self {
            functions,
            clamper,
            bounds,
            encode,
        })
    }

    /// The most steps `eval` takes, finding the one function the input falls to
    /// and evaluating it; those `span` takes, finding that function's span and
    /// putting it in place; and the most spans [`shape::over`] finds of it: of
    /// each function stitched, its own, a constant one where an interval
    /// encodes to one place, clipped again to the range.
    ///
    /// [`shape::over`]: crate::function::shape::over
    pub(super) fn cost(&self) -> Costs {
        let search = Cost::new(u64::from(usize::BITS - self.bounds.len().leading_zeros()));
        let most =
            |of: fn(&Function) -> Cost| self.functions.iter().map(of).max().unwrap_or(Cost::ZERO);
        let eval = most(Function::cost) + search + Cost::new(2) + self.clamper.cost();
        let outputs = self.clamper.range.as_ref().map_or(0, |r| r.len() as u64);
        let span = eval + most(Function::span_cost) + span_steps(outputs);
        let own = self.functions.iter().fold(0_u64, |spans, f| {
            spans.saturating_add(f.spans()).saturating_add(1)
        });
        Costs {
            eval,
            span,
            spans: clipped_spans(own, outputs),
        }
    }

    /// Whether its bounds are in order, as finding a subdomain needs.
    fn ordered(&self) -> bool {
        self.bounds
            .windows(2)
            .all(|w| w[0].partial_cmp(&w[1]).is_some_and(|o| o.is_le()))
    }

    /// The subdomain the input `x`, within the domain, falls in on `side` of
    /// it: each closed on the left and open on the right, but the last,
    /// closed on the right as well (ISO 32000-1, 7.10.4), so at `x` the one
    /// whose interval holds it, just above it the one starting at or below
    /// it and ending past it, and just below it the one starting below it and
    /// ending at or past it.
    fn locate(&self, x: f32, side: Side) -> Option<usize> {
        let k = self.bounds.len().checked_sub(1).filter(|k| *k > 0)?;
        let i = match side {
            Side::Below => self.bounds.partition_point(|b| *b < x).checked_sub(1)?,
            Side::At if x == *self.bounds.last()? => k - 1,
            Side::At | Side::Above => self.bounds.partition_point(|b| *b <= x).checked_sub(1)?,
        };
        Some(i.min(k - 1))
    }

    /// Evaluate the function with the given input.
    pub(crate) fn eval(&self, input: f32) -> Option<Values> {
        let mut input = [input];
        self.clamper.clamp_input(&mut input);
        let x = input[0];
        if !(x >= *self.bounds.first()? && x <= *self.bounds.last()?) {
            return None;
        }
        let index = self.locate(x, Side::At)?;
        let (b0, b1) = (*self.bounds.get(index)?, *self.bounds.get(index + 1)?);
        let encoding = self.encode.get(index)?;
        let function = self.functions.get(index)?;
        // A last bound at `Domain1` leaves `Domain1` alone in the last subdomain,
        // which the specification encodes as its `Encode` start.
        let encoded = if b0 == b1 {
            encoding.0
        } else {
            interpolate(x, b0, b1, encoding.0, encoding.1)
        };

        let mut evaluated = function.eval(smallvec![encoded])?;

        self.clamper.clamp_output(&mut evaluated);

        Some(evaluated)
    }

    /// The function on `side` of `x`, within its domain
    /// ([`Function::span`]): the span of the function of the subdomain `x`
    /// falls in on that side ([`locate`](Self::locate)) at the input `x`
    /// encodes to, on the side it maps that side to, put where it maps from
    /// and cut to the subdomain; a subdomain that encodes to one place, as a
    /// last one at `Domain1` does, the line of its value there; its values
    /// clipped to the range. A subdomain with no function or encoding, which
    /// `eval` gives no value, and bounds out of order, a curve.
    pub(crate) fn span(&self, x: f32, side: Side) -> Span {
        let (d0, d1) = self.clamper.domain.first().copied().unwrap_or((0.0, 1.0));
        let Some(index) = self.locate(x, side).filter(|_| self.ordered()) else {
            return Span::Curve(d0, d1);
        };
        let (b0, b1) = (self.bounds[index], self.bounds[index + 1]);
        let (Some(function), Some(&(e0, e1))) = (self.functions.get(index), self.encode.get(index))
        else {
            return Span::Curve(b0, b1);
        };
        #[cfg(test)]
        VISITED.set(VISITED.get() + 1);
        let span = if b0 == b1 || e0 == e1 {
            match function.eval(smallvec![e0]) {
                Some(value) => Span::Line(crate::function::Line::constant(b0, b1, value)),
                None => Span::Curve(b0, b1),
            }
        } else {
            let u = interpolate(x, b0, b1, e0, e1);
            match function.span(u, side.through(e1 > e0)) {
                Span::Line(line) => Span::Line(line.mapped((b0, b1), (e0, e1))),
                Span::Curve(u0, u1) => {
                    let to_x = |u: f32| {
                        if u.is_infinite() {
                            if (u > 0.0) == (e1 > e0) { b1 } else { b0 }
                        } else {
                            interpolate(u, e0, e1, b0, b1).max(b0).min(b1)
                        }
                    };
                    let (a, b) = (to_x(u0), to_x(u1));
                    Span::Curve(a.min(b), a.max(b))
                }
            }
        };
        span.clipped(self.clamper.range.as_deref(), x, side)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hayro_syntax::object::FromBytes;

    /// One evaluation runs one of the functions stitched: it costs the costliest.
    #[test]
    fn a_stitching_costs_its_costliest_function() {
        let exponential = |outputs: usize| {
            let c = vec!["0"; outputs].join(" ");
            format!("<< /FunctionType 2 /Domain [0 1] /C0 [{c}] /C1 [{c}] /N 1 >>")
        };
        let function = |functions: &[String]| {
            let data = format!(
                "<< /FunctionType 3 /Domain [0 1] /Functions [{}] /Bounds [{}] /Encode [{}] >>",
                functions.join(" "),
                vec!["0.5"; functions.len() - 1].join(" "),
                vec!["0 1"; functions.len()].join(" "),
            );
            Function::new(&Object::from_bytes(data.as_bytes()).unwrap()).unwrap()
        };
        let small = function(&[exponential(1)]).cost().steps();
        let large = function(&[exponential(1000)]).cost().steps();
        let both = function(&[exponential(1), exponential(1000)])
            .cost()
            .steps();
        assert!(large >= 2000, "{large}");
        assert!(
            both >= large && both < large + small,
            "{small} {large} {both}"
        );
    }

    /// Each subdomain maps to its encoding from the bounds the dictionary gives,
    /// `Domain` at the ends (ISO 32000-1, 7.10.4): the first function sees its
    /// `Encode` start at `Domain0` and the last its end at `Domain1`, and a last
    /// bound at `Domain1` makes `Domain1` the last function's `Encode` start.
    #[test]
    fn a_stitching_encodes_each_subdomain_from_its_exact_bounds() {
        let function = |domain: &str, bounds: &str, encode: &str| {
            let ramp = "<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>";
            let data = format!(
                "<< /FunctionType 3 /Domain [{domain}] /Functions [{ramp} {ramp}] \
                 /Bounds [{bounds}] /Encode [{encode}] >>"
            );
            Function::new(&Object::from_bytes(data.as_bytes()).unwrap()).unwrap()
        };
        let at = |f: &Function, x: f32| f.eval(smallvec![x]).unwrap()[0];
        let halves = function("0 1", "0.5", "0 1 0 1");
        assert_eq!(at(&halves, 0.0), 0.0);
        assert_eq!(at(&halves, 0.25), 0.5);
        assert_eq!(at(&halves, 0.5), 0.0);
        assert_eq!(at(&halves, 1.0), 1.0);
        let reversed = function("2 4", "3", "1 0 1 0");
        assert_eq!(at(&reversed, 2.0), 1.0);
        assert_eq!(at(&reversed, 4.0), 0.0);
        let last_at_end = function("0 1", "1", "0 1 0.25 1");
        assert_eq!(at(&last_at_end, 1.0), 0.25);
        assert_eq!(at(&last_at_end, 0.5), 0.5);
    }

    #[test]
    fn simple() {
        let data = b"<<
  /FunctionType 3
  /Domain [-7 7]
  /Functions [
    << /FunctionType 2
       /Domain [0 1]
       /C0 [0.5 0.5 0.5]
       /C1 [0.5 0.5 0.5]
       /N 1
    >>
    << /FunctionType 2
       /Domain [0 1]
       /C0 [0.7 0.7 0.7]
       /C1 [0.7 0.7 0.7]
       /N 1
    >>
  ]
  /Bounds [0]
  /Encode [0 1 0 1]
>>";

        let dict = Object::from_bytes(data).unwrap();
        let function = Function::new(&dict).unwrap();

        assert_eq!(
            function.eval(smallvec![-7.0]).unwrap().as_slice(),
            &[0.5, 0.5, 0.5]
        );
        assert_eq!(
            function.eval(smallvec![-3.0]).unwrap().as_slice(),
            &[0.5, 0.5, 0.5]
        );
        assert_eq!(
            function.eval(smallvec![-0.5]).unwrap().as_slice(),
            &[0.5, 0.5, 0.5]
        );
        assert_eq!(
            function.eval(smallvec![0.0]).unwrap().as_slice(),
            &[0.7, 0.7, 0.7]
        );
        assert_eq!(
            function.eval(smallvec![7.0]).unwrap().as_slice(),
            &[0.7, 0.7, 0.7]
        );
    }
}
