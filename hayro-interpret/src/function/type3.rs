use crate::function::{Function, Interval, read_intervals};
use hayro_syntax::object::Array;
use hayro_syntax::object::Dict;
use hayro_syntax::object::Object;
use hayro_syntax::object::dict::keys::{BOUNDS, ENCODE, FUNCTIONS};

#[derive(Debug)]
pub(crate) struct Type3 {
    strictly_increasing: bool,
    functions: Vec<Function>,
    bounds: Vec<f32>,
    encode: Vec<Interval>,
}

impl Type3 {
    pub(crate) fn new(dict: &Dict<'_>, domain: Interval, depth: u8) -> Option<Self> {
        let functions = dict
            .get::<Array<'_>>(FUNCTIONS)?
            .iter::<Object<'_>>()
            .map(|o| Function::new_nested(&o, depth + 1))
            .collect::<Option<Vec<_>>>()?;

        let num_outputs = functions.first().map(Function::num_outputs)?;

        if functions
            .iter()
            .any(|f| f.num_inputs() != 1 || f.num_outputs() != num_outputs)
        {
            return None;
        }

        let inner_bounds = dict
            .get::<Array<'_>>(BOUNDS)
            .map(|a| a.iter::<f32>().collect::<Vec<_>>())
            .unwrap_or_default();
        let encode = read_intervals(dict, ENCODE)?;

        if inner_bounds.len() != functions.len() - 1 || encode.len() != functions.len() {
            return None;
        }

        let mut bounds = vec![domain.min];
        bounds.extend(inner_bounds);
        bounds.push(domain.max);

        Some(Self {
            strictly_increasing: bounds.windows(2).all(|p| p[0] < p[1]),
            functions,
            bounds,
            encode,
        })
    }

    pub(crate) fn num_outputs(&self) -> usize {
        self.functions[0].num_outputs()
    }

    pub(crate) fn eval(&self, input: f64, output: &mut [f32]) -> Option<()> {
        let index = self.find_interval(input);
        let interval = Interval {
            min: self.bounds[index],
            max: self.bounds[index + 1],
        };

        self.functions[index].eval_1d_f64(interval.map_f64(input, self.encode[index]), output)
    }

    fn find_interval(&self, x: f64) -> usize {
        let inner = &self.bounds[1..self.bounds.len() - 1];

        if inner.first() == Some(&self.bounds[0]) && x == f64::from(self.bounds[0]) {
            return 0;
        }

        inner.partition_point(|bound| f64::from(*bound) <= x)
    }

    pub(super) fn linear_pieces(
        &self,
        domain: [f64; 2],
        offsets: [f64; 2],
        budget: &mut usize,
        tolerance: f32,
        emit: &mut super::PieceEmitter<'_>,
    ) -> Option<()> {
        let [first, last] = domain;
        if !self.strictly_increasing {
            return None;
        }
        if first == last {
            let mut color = [0.0; super::MAX_COMPONENTS];
            let color = &mut color[..self.num_outputs()];
            self.eval(first, color)?;
            *budget -= 1;
            return emit(offsets.map(|v| v as f32), color, color, budget);
        }
        let low = first.min(last);
        let high = first.max(last);
        let begin = self.find_interval(low);
        let end = self
            .bounds
            .partition_point(|&bound| f64::from(bound) < high);
        if end - begin > *budget {
            return None;
        }
        // A clipped upper endpoint at a stitch belongs to the next child.
        if end < self.functions.len() && f64::from(self.bounds[end]) == high {
            let mut left = [0.0; super::MAX_COMPONENTS];
            let mut right = left;
            let n = self.num_outputs();
            self.functions[end - 1].eval(&[self.encode[end - 1].max], &mut left[..n])?;
            self.eval(high, &mut right[..n])?;
            if left != right {
                return None;
            }
        }
        let offset = |x: f64| offsets[0] + (x - first) / (last - first) * (offsets[1] - offsets[0]);
        for k in begin..end {
            let i = if first < last {
                k
            } else {
                end - 1 - (k - begin)
            };
            let interval = Interval {
                min: self.bounds[i],
                max: self.bounds[i + 1],
            };
            let clipped = [
                f64::from(interval.min).max(low),
                f64::from(interval.max).min(high),
            ];
            let ordered = if first < last {
                clipped
            } else {
                [clipped[1], clipped[0]]
            };
            let child_domain = ordered.map(|x| interval.map_f64(x, self.encode[i]));
            self.functions[i].ranged_pieces(
                child_domain,
                ordered.map(offset),
                budget,
                Some(tolerance),
                emit,
            )?;
        }
        Some(())
    }

    pub(crate) fn stitching_bounds(&self) -> Vec<f32> {
        let mut stitching_bounds = self.bounds[1..self.bounds.len() - 1].to_vec();

        for (index, function) in self.functions.iter().enumerate() {
            let (bounds_i_minus_1, bounds_i) = (self.bounds[index], self.bounds[index + 1]);
            let Interval {
                min: encode_min,
                max: encode_max,
            } = self.encode[index];

            if (encode_max - encode_min).abs() <= f32::EPSILON {
                continue;
            }

            for child_bound in function.stitching_bounds() {
                let bound = self.encode[index].map(
                    child_bound,
                    Interval {
                        min: bounds_i_minus_1,
                        max: bounds_i,
                    },
                );
                let min_bound = bounds_i_minus_1.min(bounds_i);
                let max_bound = bounds_i_minus_1.max(bounds_i);
                if bound > min_bound && bound < max_bound {
                    stitching_bounds.push(bound);
                }
            }
        }

        stitching_bounds.sort_by(f32::total_cmp);
        stitching_bounds.dedup_by(|a, b| (*a - *b).abs() <= f32::EPSILON);

        stitching_bounds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hayro_syntax::object::FromBytes;

    #[test]
    fn reversed_stitch_clamps_channels_with_a_shared_budget() {
        let dict = b"<< /FunctionType 3 /Domain [0 1] /Range [.25 .75 .5 1] /Functions [
            << /FunctionType 2 /Domain [0 1] /N 1 /C0 [0 1] /C1 [1 0] >>] /Bounds [] /Encode [1 0] >>";
        let function = Function::new(&Object::from_bytes(dict).unwrap()).unwrap();
        for mut budget in [3, 4] {
            let fits = budget == 4;
            let mut edges = Vec::new();
            let result = function.linear_pieces(
                [0.0, 1.0],
                [0.0, 1.0],
                &mut budget,
                Some(0.01),
                &mut |ts, a, b| {
                    for (t, color) in ts.into_iter().zip([a, b]) {
                        assert_eq!(color, function.eval_one(&[t]).unwrap());
                    }
                    edges.push(ts[1]);
                    Some(())
                },
            );
            assert_eq!(result.is_some(), fits);
            if fits {
                assert_eq!(budget, 0);
                assert_eq!(edges, [0.25, 0.5, 0.75, 1.0]);
            }
        }
    }

    #[test]
    fn nested_stitch_subsets_share_the_piece_budget() {
        let line = "<< /FunctionType 2 /Domain [0 1] /N 1 >>";
        let child = format!(
            "<< /FunctionType 3 /Domain [0 2] /Functions [{line} {line}] /Bounds [1] /Encode [0 1 1 0] >>"
        );
        let outer = format!(
            "<< /FunctionType 3 /Domain [-2 2] /Functions [{child} {child}] /Bounds [0] /Encode [0 2 0 2] >>"
        );
        let function = Function::new(&Object::from_bytes(outer.as_bytes()).unwrap()).unwrap();
        for domain in [[-1.5, 0.5], [0.5, -1.5]] {
            let mut pieces = Vec::new();
            let mut budget = 3;
            function
                .linear_pieces(
                    domain,
                    [0.0, 1.0],
                    &mut budget,
                    Some(0.01),
                    &mut |offsets, a, b| {
                        pieces.push((offsets, a[0], b[0]));
                        Some(())
                    },
                )
                .unwrap();
            assert_eq!(pieces.len(), 3);
            assert_eq!(budget, 0);
            for ([start, end], a, b) in pieces {
                for (offset, expected) in [(start, a), (end, b)] {
                    assert_eq!(
                        function
                            .eval_one(&[
                                (domain[0] + (domain[1] - domain[0]) * f64::from(offset)) as f32
                            ])
                            .unwrap()[0],
                        expected
                    );
                }
            }
            assert!(
                function
                    .linear_pieces(domain, [0.0, 1.0], &mut 2, Some(0.01), &mut |_, _, _| Some(
                        ()
                    ))
                    .is_none()
            );
        }
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
            function.eval_one(&[-7.0]).unwrap().as_slice(),
            &[0.5, 0.5, 0.5]
        );
        assert_eq!(
            function.eval_one(&[-3.0]).unwrap().as_slice(),
            &[0.5, 0.5, 0.5]
        );
        assert_eq!(
            function.eval_one(&[-0.5]).unwrap().as_slice(),
            &[0.5, 0.5, 0.5]
        );
        assert_eq!(
            function.eval_one(&[0.0]).unwrap().as_slice(),
            &[0.7, 0.7, 0.7]
        );
        assert_eq!(
            function.eval_one(&[7.0]).unwrap().as_slice(),
            &[0.7, 0.7, 0.7]
        );
    }
}
