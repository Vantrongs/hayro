//! The shape of a function of one input as it evaluates: where it is straight
//! between points, and where it steps.

use crate::function::{Function, Values, interpolate};
use smallvec::SmallVec;

/// A part of a function of one input, over an interval of it, as it evaluates
/// ([`Function::shape`]). The pieces of a shape follow one another from the start
/// of its interval to the end, each starting where the one before ends; where
/// they meet the function may step.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Piece {
    /// Straight between each two points, from the first's input to the last's:
    /// two points at one input are the values from below it and from above it,
    /// where the function steps.
    Lines(Vec<(f32, Values)>),
    /// From one input to another, of no shape known here: its values are those
    /// the function evaluates to.
    Curve(f32, f32),
}

impl Piece {
    /// Where the piece starts and ends.
    pub(crate) fn span(&self) -> (f32, f32) {
        match self {
            Self::Lines(points) => (
                points.first().map_or(f32::NAN, |p| p.0),
                points.last().map_or(f32::NAN, |p| p.0),
            ),
            Self::Curve(from, to) => (*from, *to),
        }
    }
}

/// The value of straight lines through `points` at `x`, within them: from above
/// where two points share `x`, as a function evaluates at a step.
pub(crate) fn along(points: &[(f32, Values)], x: f32) -> Values {
    let next = points.partition_point(|p| p.0 <= x);
    match (next.checked_sub(1).map(|i| &points[i]), points.get(next)) {
        (Some((x0, v0)), Some((x1, v1))) if x1 > x0 => {
            let s = (x - x0) / (x1 - x0);
            v0.iter().zip(v1).map(|(a, b)| a + (b - a) * s).collect()
        }
        (Some((_, v)), _) | (None, Some((_, v))) => v.clone(),
        (None, None) => Values::new(),
    }
}

/// The lines through `points`, each value then clipped to its `range`, with a
/// point added wherever a line crosses a side of the range, so the clipped
/// values are straight between each two points too.
pub(crate) fn clipped(
    points: Vec<(f32, Values)>,
    range: Option<&[(f32, f32)]>,
) -> Vec<(f32, Values)> {
    let Some(range) = range else {
        return points;
    };
    let mut out: Vec<(f32, Values)> = Vec::with_capacity(points.len());
    for (i, point) in points.iter().enumerate() {
        if let Some((x0, v0)) = i.checked_sub(1).map(|i| &points[i])
            && point.0 > *x0
        {
            let (x1, v1) = point;
            let mut crossings: SmallVec<[f32; 8]> = SmallVec::new();
            for ((a, b), (lo, hi)) in v0.iter().zip(v1).zip(range) {
                for side in [lo, hi] {
                    if (a - side) * (b - side) < 0.0 {
                        crossings.push((side - a) / (b - a));
                    }
                }
            }
            crossings.sort_by(f32::total_cmp);
            for s in crossings {
                let value = v0.iter().zip(v1).map(|(a, b)| a + (b - a) * s).collect();
                out.push((x0 + (x1 - x0) * s, value));
            }
        }
        out.push(point.clone());
    }
    for (_, values) in &mut out {
        for (value, (lo, hi)) in values.iter_mut().zip(range) {
            *value = value.min(*hi).max(*lo);
        }
    }
    out
}

/// The pieces of `function` over `from` to `to`, `from` below `to`: its own
/// within its domain, and past either side of it the value it evaluates to at
/// that side, as it clamps its input there.
pub(crate) fn over(function: &Function, from: f32, to: f32) -> Vec<Piece> {
    let Some((d0, d1)) = function.domain().filter(|(d0, d1)| d0 <= d1) else {
        return vec![Piece::Curve(from, to)];
    };
    let constant = |a: f32, b: f32, at: f32| match function.eval(SmallVec::from_slice(&[at])) {
        Some(value) => Piece::Lines(vec![(a, value.clone()), (b, value)]),
        None => Piece::Curve(a, b),
    };
    let mut pieces = Vec::new();
    if from < d0 {
        pieces.push(constant(from, d0.min(to), d0));
    }
    for piece in function.shape() {
        if let Some(piece) = within(piece, from, to) {
            pieces.push(piece);
        }
    }
    if to > d1 {
        pieces.push(constant(d1.max(from), to, d1));
    }
    joined(pieces)
}

/// The part of `piece` from `from` to `to`, `None` where it has none of more than a
/// point.
fn within(piece: Piece, from: f32, to: f32) -> Option<Piece> {
    let (a, b) = piece.span();
    let (lo, hi) = (a.max(from), b.min(to));
    if lo.partial_cmp(&hi) != Some(std::cmp::Ordering::Less) {
        return None;
    }
    Some(match piece {
        Piece::Curve(..) => Piece::Curve(lo, hi),
        Piece::Lines(points) => {
            let mut kept = Vec::with_capacity(points.len() + 2);
            if lo > a {
                kept.push((lo, along(&points, lo)));
            }
            kept.extend(
                points
                    .iter()
                    .filter(|p| {
                        (p.0 > lo || lo == a && p.0 == lo) && (p.0 < hi || hi == b && p.0 == hi)
                    })
                    .cloned(),
            );
            if hi < b {
                kept.push((hi, below(&points, hi)));
            }
            Piece::Lines(kept)
        }
    })
}

/// `pieces`, each starting where the one before ends, with neighbouring lines
/// joined into one, a step between them two points at one input.
pub(crate) fn joined(pieces: Vec<Piece>) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        match (out.last_mut(), piece) {
            (Some(Piece::Lines(before)), Piece::Lines(points)) => {
                let mut points = points.into_iter();
                if let (Some(last), Some(first)) = (before.last(), points.next()) {
                    // The same value from either side is no step.
                    if !(last.0 == first.0 && last.1 == first.1) {
                        before.push(first);
                    }
                }
                before.extend(points);
            }
            (_, piece) => out.push(piece),
        }
    }
    out
}

/// The pieces of a function of the input `x` that evaluates `inner`'s pieces at
/// `interpolate(x, b0, b1, e0, e1)`, over `lo` to `hi`, `lo` below `hi`: those of
/// `inner` over the inputs `lo` and `hi` map to, put where they map from, in the
/// order of `x`.
pub(crate) fn mapped(
    function: &Function,
    (lo, hi): (f32, f32),
    (b0, b1): (f32, f32),
    (e0, e1): (f32, f32),
) -> Vec<Piece> {
    let u = |x: f32| interpolate(x, b0, b1, e0, e1);
    let (ua, ub) = (u(lo), u(hi));
    if ua == ub || e0 == e1 {
        return match function.eval(SmallVec::from_slice(&[ua])) {
            Some(value) => vec![Piece::Lines(vec![(lo, value.clone()), (hi, value)])],
            None => vec![Piece::Curve(lo, hi)],
        };
    }
    let x = |u: f32| interpolate(u, e0, e1, b0, b1).clamp(lo, hi);
    let reversed = ua > ub;
    let mut pieces: Vec<Piece> = over(function, ua.min(ub), ua.max(ub))
        .into_iter()
        .map(|piece| match piece {
            Piece::Curve(a, b) => {
                let (a, b) = (x(a), x(b));
                Piece::Curve(a.min(b), a.max(b))
            }
            Piece::Lines(points) => {
                let mut points: Vec<_> = points.into_iter().map(|(u, v)| (x(u), v)).collect();
                if reversed {
                    points.reverse();
                }
                Piece::Lines(points)
            }
        })
        .collect();
    if reversed {
        pieces.reverse();
    }
    // The ends, which rounding may have moved, are the interval's.
    match pieces.first_mut() {
        Some(Piece::Lines(points)) => points[0].0 = lo,
        Some(Piece::Curve(a, _)) => *a = lo,
        None => {}
    }
    match pieces.last_mut() {
        Some(Piece::Lines(points)) => {
            let last = points.len() - 1;
            points[last].0 = hi;
        }
        Some(Piece::Curve(_, b)) => *b = hi,
        None => {}
    }
    pieces
}

/// The pieces of functions of one output each evaluated alike, as one function
/// of their outputs side by side, over `from` to `to`: lines where every one is
/// lines, through the points of them all; elsewhere curves, each ending where a
/// piece of one of them ends, so each step of one is a step of them all.
pub(crate) fn side_by_side(functions: &[Function], from: f32, to: f32) -> Vec<Piece> {
    let shapes: Vec<Vec<Piece>> = functions.iter().map(|f| over(f, from, to)).collect();
    let lines: Option<Vec<&[(f32, Values)]>> = shapes
        .iter()
        .map(|shape| match shape.as_slice() {
            [Piece::Lines(points)] => Some(points.as_slice()),
            _ => None,
        })
        .collect();
    if let Some(lines) = lines {
        // Every input a point of one is at, from below and from above.
        let mut xs: Vec<f32> = lines.iter().flat_map(|l| l.iter().map(|p| p.0)).collect();
        xs.sort_by(f32::total_cmp);
        xs.dedup();
        let mut points = Vec::with_capacity(2 * xs.len());
        for x in xs {
            let first = |v: Values| v.first().copied().unwrap_or(0.0);
            let from_below: Values = lines.iter().map(|l| first(below(l, x))).collect();
            let above: Values = lines.iter().map(|l| first(along(l, x))).collect();
            if from_below != above {
                points.push((x, from_below));
            }
            points.push((x, above));
        }
        return vec![Piece::Lines(points)];
    }
    let mut ends: Vec<f32> = shapes
        .iter()
        .flat_map(|shape| {
            shape.iter().flat_map(|p| {
                let (a, b) = p.span();
                [a, b]
            })
        })
        .filter(|x| x.is_finite())
        .collect();
    ends.push(from);
    ends.push(to);
    ends.sort_by(f32::total_cmp);
    ends.dedup();
    ends.windows(2).map(|w| Piece::Curve(w[0], w[1])).collect()
}

/// The value of straight lines through `points` at `x`, within them, from below
/// where two points share `x`.
fn below(points: &[(f32, Values)], x: f32) -> Values {
    let next = points.partition_point(|p| p.0 < x);
    match (next.checked_sub(1).map(|i| &points[i]), points.get(next)) {
        (Some((x0, v0)), Some((x1, v1))) if x1 > x0 => {
            let s = (x - x0) / (x1 - x0);
            v0.iter().zip(v1).map(|(a, b)| a + (b - a) * s).collect()
        }
        (_, Some((_, v))) | (Some((_, v)), None) => v.clone(),
        (None, None) => Values::new(),
    }
}
