//! The shape of a function of one input as it evaluates: where it is straight
//! between points, and where it steps.
//!
//! Each function that is straight between points says so at an input itself
//! ([`Function::span`]): the line it evaluates on just below the input, at it,
//! or just above it, its ends where the function bends or steps, as at a
//! sample, where an encoded input is clipped to the samples, at a stitching's
//! bound, or where an output is clipped to its range. Its evaluation is that
//! line's value at the input, so a shape walked from those lines
//! ([`over`]) is the function as it evaluates, bends, steps and the value at a
//! step alike.

use crate::function::{Function, Values, interpolate};
use smallvec::SmallVec;

/// A part of a function of one input, over an interval of it, as it evaluates
/// ([`over`]). The pieces of a shape follow one another from the start
/// of its interval to the end, each starting where the one before ends.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Piece {
    /// Straight between each two points at different inputs, from the first's
    /// input to the last's. Where the function steps, or its value at an input
    /// is neither of those around it, three points share the input: the value
    /// from below it, at it, and from above it ([`values_at`]); at the first
    /// input of the piece two may, the value at it and from above, and at the
    /// last two, from below and at it.
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

/// Which side of an input a function's [`Span`] there is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    /// The line just below the input, ending at it.
    Below,
    /// A line the function's value at the input is on.
    At,
    /// The line just above the input, starting at it.
    Above,
}

impl Side {
    /// The side of the input `x` an increasing map takes to this side of its
    /// image, and a decreasing one to the other.
    pub(crate) fn through(self, increasing: bool) -> Self {
        match (self, increasing) {
            (side, true) | (side @ Self::At, false) => side,
            (Self::Below, false) => Self::Above,
            (Self::Above, false) => Self::Below,
        }
    }
}

/// A straight line of a function from `x0` to `x1`, `x0` at most `x1`, of the
/// values `v0` and `v1` there; either end may be infinite, the values then one.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Line {
    pub(crate) x0: f32,
    pub(crate) x1: f32,
    pub(crate) v0: Values,
    pub(crate) v1: Values,
}

impl Line {
    /// The line of one value `v` from `x0` to `x1`.
    pub(crate) fn constant(x0: f32, x1: f32, v: Values) -> Self {
        Self {
            x0,
            x1,
            v0: v.clone(),
            v1: v,
        }
    }

    /// Its value at `x`: at either end that end's value.
    pub(crate) fn at(&self, x: f32) -> Values {
        if x == self.x0 || self.v0 == self.v1 {
            return self.v0.clone();
        }
        if x == self.x1 {
            return self.v1.clone();
        }
        let s = (x - self.x0) / (self.x1 - self.x0);
        self.v0
            .iter()
            .zip(&self.v1)
            .map(|(a, b)| a + (b - a) * s)
            .collect()
    }

    /// The part of the line from `lo` to `hi`, `lo` at most `hi`, within it.
    fn between(&self, lo: f32, hi: f32) -> Self {
        let (x0, x1) = (self.x0.max(lo), self.x1.min(hi));
        Self {
            x0,
            x1,
            v0: self.at(x0),
            v1: self.at(x1),
        }
    }

    /// The line, each value clipped to its `range`, as a function clips its
    /// outputs: where a value crosses a side of its range the clipped values
    /// bend, so the part from the crossing before `x` to the one after it on
    /// `side`, straight there, its values clipped.
    pub(crate) fn clipped(self, range: Option<&[(f32, f32)]>, x: f32, side: Side) -> Self {
        let Some(range) = range else {
            return self;
        };
        let clip = |values: &Values| -> Values {
            values
                .iter()
                .zip(range)
                .map(|(v, (lo, hi))| v.min(*hi).max(*lo))
                .chain(values.iter().skip(range.len()).copied())
                .collect()
        };
        let mut line = self;
        if line.v0 != line.v1 && line.x0 < line.x1 {
            let mut crossings: SmallVec<[f32; 8]> = SmallVec::new();
            for ((a, b), (lo, hi)) in line.v0.iter().zip(&line.v1).zip(range) {
                for edge in [lo, hi] {
                    if (a - edge) * (b - edge) < 0.0 {
                        let s = (edge - a) / (b - a);
                        crossings.push(line.x0 + (line.x1 - line.x0) * s);
                    }
                }
            }
            let (mut lo, mut hi) = (line.x0, line.x1);
            for c in crossings {
                let before = match side {
                    Side::Below => c < x,
                    Side::At | Side::Above => c <= x,
                };
                if before {
                    lo = lo.max(c);
                } else {
                    hi = hi.min(c);
                }
            }
            if lo > line.x0 || hi < line.x1 {
                line = line.between(lo, hi);
            }
        }
        Self {
            x0: line.x0,
            x1: line.x1,
            v0: clip(&line.v0),
            v1: clip(&line.v1),
        }
    }

    /// The line of a function that evaluates this one at
    /// `interpolate(x, b0, b1, e0, e1)`, over `b0` to `b1`, `b0` below `b1`, and
    /// `e0` not `e1`: its ends put where they map from, cut to that interval.
    pub(crate) fn mapped(&self, (b0, b1): (f32, f32), (e0, e1): (f32, f32)) -> Self {
        let x = |u: f32| {
            if u.is_infinite() {
                if (u > 0.0) == (e1 > e0) {
                    f32::INFINITY
                } else {
                    f32::NEG_INFINITY
                }
            } else {
                interpolate(u, e0, e1, b0, b1)
            }
        };
        // The inputs `b0` and `b1` map to, exactly.
        let u = |x: f32| if x == b0 { e0 } else { e1 };
        let (xa, xb) = (x(self.x0), x(self.x1));
        let ((xa, ua), (xb, ub)) = if xa <= xb {
            ((xa, self.x0), (xb, self.x1))
        } else {
            ((xb, self.x1), (xa, self.x0))
        };
        let (x0, v0) = if xa < b0 {
            (b0, self.at(u(b0)))
        } else {
            (xa, self.at(ua))
        };
        let (x1, v1) = if xb > b1 {
            (b1, self.at(u(b1)))
        } else {
            (xb, self.at(ub))
        };
        Self { x0, x1, v0, v1 }
    }
}

/// What a function is at an input, on a side of it ([`Function::span`]).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Span {
    /// Straight: its value on that side is the line's, and at the input, where
    /// asked [`Side::At`], the line's value there.
    Line(Line),
    /// Of no shape known here from one input to another, `from` at most `to`.
    Curve(f32, f32),
}

impl Span {
    /// The span's line clipped to `range` ([`Line::clipped`]).
    pub(crate) fn clipped(self, range: Option<&[(f32, f32)]>, x: f32, side: Side) -> Self {
        match self {
            Self::Line(line) => Self::Line(line.clipped(range, x, side)),
            curve => curve,
        }
    }

    /// Where the span starts and ends.
    fn ends(&self) -> (f32, f32) {
        match self {
            Self::Line(line) => (line.x0, line.x1),
            Self::Curve(from, to) => (*from, *to),
        }
    }
}

/// The values of lines through `points`, a [`Piece::Lines`], at `x` within them:
/// from below it, at it, and from above it. At the first input of the points
/// the value from below is the value at it, and at the last the value from
/// above is.
pub(crate) fn values_at(points: &[(f32, Values)], x: f32) -> (Values, Values, Values) {
    let lo = points.partition_point(|p| p.0 < x);
    let hi = points.partition_point(|p| p.0 <= x);
    let group = &points[lo..hi];
    if group.is_empty() {
        let value = match (lo.checked_sub(1).map(|i| &points[i]), points.get(lo)) {
            (Some((x0, v0)), Some((x1, v1))) => Line {
                x0: *x0,
                x1: *x1,
                v0: v0.clone(),
                v1: v1.clone(),
            }
            .at(x),
            (Some((_, v)), None) | (None, Some((_, v))) => v.clone(),
            (None, None) => Values::new(),
        };
        return (value.clone(), value.clone(), value);
    }
    let (first, last) = (lo == 0, hi == points.len());
    let (below, at, above) = match group.len() {
        1 => (0, 0, 0),
        2 if first && !last => (0, 0, 1),
        2 if last && !first => (0, 1, 1),
        2 => (0, 1, 1),
        _ => (0, 1, group.len() - 1),
    };
    (
        group[below].1.clone(),
        group[at].1.clone(),
        group[above].1.clone(),
    )
}

/// Which of the points at its input the `i`th of `points`, a [`Piece::Lines`],
/// is ([`Piece::Lines`]): of several there, the value from below the input, at
/// it, or from above it; `None` where it is the only one.
pub(crate) fn side_of(points: &[(f32, Values)], i: usize) -> Option<Side> {
    let x = points[i].0;
    let mut lo = i;
    while lo > 0 && points[lo - 1].0 == x {
        lo -= 1;
    }
    let mut hi = i + 1;
    while hi < points.len() && points[hi].0 == x {
        hi += 1;
    }
    let (first, last) = (lo == 0, hi == points.len());
    match (hi - lo, i - lo) {
        (1, _) => None,
        (2, 0) if first && !last => Some(Side::At),
        (2, _) if first && !last => Some(Side::Above),
        (2, 0) | (_, 0) => Some(Side::Below),
        (2, _) => Some(Side::At),
        (n, k) if k == n - 1 => Some(Side::Above),
        _ => Some(Side::At),
    }
}

/// The pieces of `function` over `from` to `to`, `from` below `to`, as it
/// evaluates, past either side of its domain the value it evaluates to at that
/// side, as it clamps its input there: the lines and curves it says it is on
/// above each input from `from` on ([`Function::span`]), each starting where
/// the one before ends; at each input where two meet, its value from below,
/// at it and from above; neighbouring lines one piece.
pub(crate) fn over(function: &Function, from: f32, to: f32) -> Vec<Piece> {
    if from.partial_cmp(&to) != Some(core::cmp::Ordering::Less) {
        return vec![Piece::Curve(from, to)];
    }
    // The spans above each input from `from` on, cut to `to`.
    let mut spans: Vec<Span> = Vec::new();
    let mut x = from;
    while x < to {
        let span = function.span(x, Side::Above);
        let (_, end) = span.ends();
        // Rounding may put a span's end at its start: the span then holds the
        // least step past it.
        let end = if end > x {
            end.min(to)
        } else {
            x.next_up().min(to)
        };
        spans.push(match span {
            Span::Line(line) => Span::Line(Line {
                x0: x,
                x1: end,
                v0: line.at(x),
                v1: line.at(end),
            }),
            Span::Curve(..) => Span::Curve(x, end),
        });
        x = end;
    }
    // The value at `x`: on the line the function says it is on there, or as
    // it evaluates.
    let value_at = |x: f32| match function.span(x, Side::At) {
        Span::Line(line) => Some(line.at(x)),
        Span::Curve(..) => function.eval(SmallVec::from_slice(&[x])),
    };
    let mut pieces = Vec::new();
    let mut points: Vec<(f32, Values)> = Vec::new();
    for (i, span) in spans.iter().enumerate() {
        let Span::Line(line) = span else {
            if !points.is_empty() {
                pieces.push(Piece::Lines(std::mem::take(&mut points)));
            }
            let (a, b) = span.ends();
            pieces.push(Piece::Curve(a, b));
            continue;
        };
        let after_line = i > 0 && matches!(spans[i - 1], Span::Line(_));
        let at = value_at(line.x0);
        if after_line {
            // A point the line before ended at, from below.
            let below = points.last().map(|p| p.1.clone());
            match (at, below) {
                (Some(at), Some(below)) if !(below == at && at == line.v0) => {
                    points.push((line.x0, at));
                    points.push((line.x0, line.v0.clone()));
                }
                (None, Some(_)) => {
                    // No value at it: none to tell from those around it.
                    points.push((line.x0, line.v0.clone()));
                    points.push((line.x0, line.v0.clone()));
                }
                _ => {}
            }
        } else {
            if let Some(at) = at
                && at != line.v0
            {
                points.push((line.x0, at));
            }
            points.push((line.x0, line.v0.clone()));
        }
        points.push((line.x1, line.v1.clone()));
        let next_is_line = matches!(spans.get(i + 1), Some(Span::Line(_)));
        if !next_is_line
            && let Some(at) = value_at(line.x1)
            && at != line.v1
        {
            points.push((line.x1, at));
        }
    }
    if !points.is_empty() {
        pieces.push(Piece::Lines(points));
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
        // Every input a point of one is at, from below, at, and from above.
        let mut xs: Vec<f32> = lines.iter().flat_map(|l| l.iter().map(|p| p.0)).collect();
        xs.sort_by(f32::total_cmp);
        xs.dedup();
        let last = xs.len().saturating_sub(1);
        let mut points = Vec::with_capacity(3 * xs.len());
        for (k, x) in xs.into_iter().enumerate() {
            let first = |v: Values| v.first().copied().unwrap_or(0.0);
            let mut below = Values::new();
            let mut at = Values::new();
            let mut above = Values::new();
            for l in &lines {
                let (b, a, c) = values_at(l, x);
                below.push(first(b));
                at.push(first(a));
                above.push(first(c));
            }
            if k == 0 {
                if above != at {
                    points.push((x, at));
                }
                points.push((x, above));
            } else if k == last {
                points.push((x, below.clone()));
                if at != below {
                    points.push((x, at));
                }
            } else if below == at && at == above {
                points.push((x, at));
            } else {
                points.extend([(x, below), (x, at), (x, above)]);
            }
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
