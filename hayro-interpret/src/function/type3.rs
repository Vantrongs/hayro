use crate::encode::Cost;
use crate::function::shape::{self, Piece};
use crate::function::{
    Clamper, Costs, Function, OVER_POINTS_COUNT, TupleVec, Values, clipped_shape, interpolate,
};
use hayro_syntax::object::Array;
use hayro_syntax::object::Dict;
use hayro_syntax::object::Object;
use hayro_syntax::object::dict::keys::{BOUNDS, ENCODE, FUNCTIONS};
use smallvec::smallvec;

#[cfg(test)]
thread_local! {
    /// The functions `shape` has visited.
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
        let mut bounds = vec![domain.0 - 0.0001];
        if let Some(a) = dict.get::<Array<'_>>(BOUNDS) {
            bounds.extend(a.items::<f32>().map_while(|n| n));
        }
        // Add a small delta so that the interval is considered to be closed on the right.
        bounds.push(domain.1 + 0.0001);

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
    /// and evaluating it, and those `shape` takes and the most points it holds,
    /// of every function under this one.
    pub(super) fn cost(&self) -> Costs {
        let search = Cost::new(u64::from(usize::BITS - self.bounds.len().leading_zeros()));
        let eval = self
            .functions
            .iter()
            .map(Function::cost)
            .max()
            .unwrap_or(Cost::ZERO)
            + search
            + Cost::new(2)
            + self.clamper.cost();
        // Each function's points, and those `shape::over` adds at its sides,
        // clipped again to the range.
        let (own, steps) = self
            .functions
            .iter()
            .fold((0_u64, Cost::ZERO), |(points, steps), f| {
                (
                    points
                        .saturating_add(f.shape_points())
                        .saturating_add(OVER_POINTS_COUNT),
                    steps + f.shape_cost(),
                )
            });
        let outputs = self.clamper.range.as_ref().map_or(0, |r| r.len() as u64);
        let (points, shape) = clipped_shape(own, outputs);
        Costs {
            eval,
            shape: steps + shape,
            points,
        }
    }

    /// Evaluate the function with the given input.
    pub(crate) fn eval(&self, input: f32) -> Option<Values> {
        let mut input = [input];
        self.clamper.clamp_input(&mut input);

        let index = find_interval(&self.bounds, input[0])?;

        let bounds_i = *self.bounds.get(index + 1)?;
        let bounds_i_minus_1 = *self.bounds.get(index)?;

        // - 1 because we inserted a dummy bound in the constructor.
        let encoding = self.encode.get(index)?;
        let function = self.functions.get(index)?;
        let encoded = interpolate(input[0], bounds_i_minus_1, bounds_i, encoding.0, encoding.1);

        let mut evaluated = function.eval(smallvec![encoded])?;

        self.clamper.clamp_output(&mut evaluated);

        Some(evaluated)
    }

    /// The pieces of each function stitched, put where its interval of the
    /// domain maps to them (`shape::mapped`), one after another, each step at a
    /// bound two points at it, the lines then clipped to the range; an interval
    /// with no function or encoding a curve, as `eval` gives it no value. Bounds
    /// out of order, which `eval` searches as they are, make the whole a curve.
    pub(crate) fn shape(&self) -> Vec<Piece> {
        let Some(&(d0, d1)) = self.clamper.domain.first() else {
            return vec![Piece::Curve(0.0, 1.0)];
        };
        if d0 > d1
            || self
                .bounds
                .windows(2)
                .any(|w| w[0].partial_cmp(&w[1]).is_none_or(|o| o.is_gt()))
        {
            return vec![Piece::Curve(d0, d1)];
        }
        let mut pieces = Vec::with_capacity(self.functions.len());
        for (i, interval) in self.bounds.windows(2).enumerate() {
            let (b0, b1) = (interval[0], interval[1]);
            let (lo, hi) = (b0.max(d0), b1.min(d1));
            if lo.partial_cmp(&hi) != Some(std::cmp::Ordering::Less) {
                continue;
            }
            match (self.functions.get(i), self.encode.get(i)) {
                (Some(function), Some(&encode)) => {
                    #[cfg(test)]
                    VISITED.set(VISITED.get() + 1);
                    pieces.extend(shape::mapped(function, (lo, hi), (b0, b1), encode));
                }
                _ => pieces.push(Piece::Curve(lo, hi)),
            }
        }
        if pieces.is_empty() {
            return vec![Piece::Curve(d0, d1)];
        }
        shape::joined(pieces)
            .into_iter()
            .map(|piece| match piece {
                Piece::Lines(points) => {
                    Piece::Lines(shape::clipped(points, self.clamper.range.as_deref()))
                }
                curve => curve,
            })
            .collect()
    }
}

fn find_interval(bounds: &[f32], x: f32) -> Option<usize> {
    if x < *bounds.first()? || x >= *bounds.last()? {
        return None;
    }

    match bounds.binary_search_by(|val| {
        if *val <= x {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    }) {
        Ok(i) => Some(i - 1),
        Err(i) => Some(i - 1),
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
