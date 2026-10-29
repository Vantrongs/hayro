//! Paths flattened into lines, the work of each line taken of a [`Meter`] before
//! the line is made, and only near a window where the lines are needed.
//!
//! A curve's lines follow from its coordinates, which a document sets: a cubic
//! with control points at 10²⁴ flattens into hundreds of millions of lines at a
//! pixel's tolerance. Here the lines of a curve are counted before any is made,
//! and paid for; a curve larger than the window it is needed in is first split
//! in halves, and a piece whose control points lie wholly outside the window
//! becomes its chord, so the lines made follow the part of the path in the window.

use crate::encode::{Cost, Exhausted, Meter, take};
use kurbo::{Affine, CubicBez, Line, ParamCurve, PathEl, PathSeg, Point, QuadBez, Rect};

/// The steps making a line takes: placing its end and passing it on. Each element
/// passed on takes as much.
pub const LINE_COST: Cost = Cost::new(4);

/// The steps splitting a curve in halves takes.
const SPLIT_COST: Cost = Cost::new(16);

/// The lines a curve that reaches into the window is flattened into without being
/// split first, however large it is.
const UNSPLIT_LINES: u64 = 64;

/// Halvings after which a piece still larger than the window is refused: halving a
/// box that fits in `f64` brings it to a pixel in fewer.
const MAX_DEPTH: usize = 1100;

#[cfg(any(test, feature = "count-operators"))]
thread_local! {
    /// The elements this thread's flattening passed on.
    static PASSED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The elements [`flatten_metered`] has passed on on this thread, each paid for
/// with [`LINE_COST`]: a count for a caller's tests to bound the work it did.
#[cfg(feature = "count-operators")]
pub fn elements_flattened() -> u64 {
    PASSED.get()
}

/// `path`, placed by `transform`, as its moves, lines and closes: a curve as lines
/// through points of it, its chords straying at most `tolerance` from it. With a
/// `window`, a piece of a curve whose control points lie wholly outside it is passed
/// on as its chord, which lies within those points' hull, so within the window the
/// lines are the curve's flattened, and a point there is wound around as often as
/// by the curve; a curve larger than the window is split in halves until each piece
/// is no larger or misses it. Without one, every curve is flattened whole.
///
/// The work is taken of `meter` before it is done: [`LINE_COST`] for each element
/// passed on, all of a curve's lines before the first, and `SPLIT_COST` for each
/// split;
/// the halves waiting to be flattened are held of it. `out` is given the meter
/// with each element, for what it does with it, and stops the flattening with
/// `Exhausted`. `Exhausted` when either refuses, or for a coordinate that is not
/// finite, with the elements before passed on.
pub fn flatten_metered(
    path: impl IntoIterator<Item = PathEl>,
    transform: Affine,
    tolerance: f64,
    window: Option<Rect>,
    meter: &mut dyn Meter,
    mut out: impl FnMut(PathEl, &mut dyn Meter) -> Result<(), Exhausted>,
) -> Result<(), Exhausted> {
    let mut out = |el, meter: &mut dyn Meter| {
        #[cfg(any(test, feature = "count-operators"))]
        PASSED.set(PASSED.get() + 1);
        out(el, meter)
    };
    let mut last = Point::ZERO;
    let mut start = Point::ZERO;
    let mut pieces: Vec<(PathSeg, usize)> = Vec::new();
    for el in path {
        let seg = match transform * el {
            PathEl::MoveTo(p) => {
                take(meter, LINE_COST)?;
                finite(&[p])?;
                (start, last) = (p, p);
                out(PathEl::MoveTo(p), meter)?;
                continue;
            }
            PathEl::LineTo(p) => {
                take(meter, LINE_COST)?;
                finite(&[p])?;
                last = p;
                out(PathEl::LineTo(p), meter)?;
                continue;
            }
            PathEl::ClosePath => {
                take(meter, LINE_COST)?;
                last = start;
                out(PathEl::ClosePath, meter)?;
                continue;
            }
            PathEl::QuadTo(p1, p2) => PathSeg::Quad(QuadBez::new(last, p1, p2)),
            PathEl::CurveTo(p1, p2, p3) => PathSeg::Cubic(CubicBez::new(last, p1, p2, p3)),
        };
        last = seg.end();
        pieces.push((seg, 0));
        while let Some((seg, depth)) = pieces.pop() {
            let bounds = control_bounds(&seg)?;
            if window.is_some_and(|window| misses(window, bounds)) {
                take(meter, LINE_COST)?;
                out(PathEl::LineTo(seg.end()), meter)?;
                continue;
            }
            let lines = lines_for(&seg, tolerance);
            let larger = window.is_some_and(|window| {
                bounds.width().max(bounds.height()) > window.width().max(window.height())
            });
            if lines > UNSPLIT_LINES && larger {
                if depth >= MAX_DEPTH {
                    return Err(Exhausted);
                }
                take(meter, SPLIT_COST)?;
                if pieces.len() + 2 > pieces.capacity() {
                    // The most the pieces waiting can number: one a depth.
                    if !meter.hold((MAX_DEPTH + 2) * size_of::<(PathSeg, usize)>()) {
                        return Err(Exhausted);
                    }
                    pieces.reserve_exact(MAX_DEPTH + 2 - pieces.len());
                }
                let (a, b) = halves(&seg);
                pieces.push((b, depth + 1));
                pieces.push((a, depth + 1));
                continue;
            }
            take(meter, LINE_COST.times(lines))?;
            for i in 1..lines {
                out(PathEl::LineTo(seg.eval(i as f64 / lines as f64)), meter)?;
            }
            out(PathEl::LineTo(seg.end()), meter)?;
        }
    }
    Ok(())
}

/// Whether `bounds` lies wholly outside `window`.
fn misses(window: Rect, bounds: Rect) -> bool {
    bounds.x1 < window.x0 || bounds.x0 > window.x1 || bounds.y1 < window.y0 || bounds.y0 > window.y1
}

/// `Exhausted` for a point that is not finite: no line through it can be placed.
fn finite(points: &[Point]) -> Result<(), Exhausted> {
    if points.iter().all(|p| p.is_finite()) {
        Ok(())
    } else {
        Err(Exhausted)
    }
}

/// The box of a curve's control points, which holds it; `Exhausted` for one that is
/// not finite.
fn control_bounds(seg: &PathSeg) -> Result<Rect, Exhausted> {
    let (first, rest): (Point, &[Point]) = match seg {
        PathSeg::Line(l) => (l.p0, std::slice::from_ref(&l.p1)),
        PathSeg::Quad(q) => (q.p0, &[q.p1, q.p2][..]),
        PathSeg::Cubic(c) => (c.p0, &[c.p1, c.p2, c.p3][..]),
    };
    finite(&[first])?;
    finite(rest)?;
    Ok(rest
        .iter()
        .fold(Rect::from_points(first, first), |r, &p| r.union_pt(p)))
}

/// The lines through equal steps of a curve's parameter whose chords stray at most
/// `tolerance` from it: a chord over a step `h` strays at most `max |c''| h² / 8`,
/// and `|c''|` is at most twice a quadratic's second difference, six times a
/// cubic's larger one. Saturates for a curve too large to count.
pub fn lines_for(seg: &PathSeg, tolerance: f64) -> u64 {
    let second = |p0: Point, p1: Point, p2: Point| {
        (p0.to_vec2() - 2.0 * p1.to_vec2() + p2.to_vec2()).hypot()
    };
    let bend = match *seg {
        PathSeg::Line(_) => 0.0,
        PathSeg::Quad(q) => 2.0 * second(q.p0, q.p1, q.p2),
        PathSeg::Cubic(c) => 6.0 * second(c.p0, c.p1, c.p2).max(second(c.p1, c.p2, c.p3)),
    };
    let lines = (bend / (8.0 * tolerance)).sqrt().ceil();
    if lines.is_finite() {
        // `as` saturates.
        (lines as u64).max(1)
    } else {
        u64::MAX
    }
}

/// A curve's halves, split at the middle of its parameter.
fn halves(seg: &PathSeg) -> (PathSeg, PathSeg) {
    match *seg {
        PathSeg::Line(l) => {
            let m = l.eval(0.5);
            (
                PathSeg::Line(Line::new(l.p0, m)),
                PathSeg::Line(Line::new(m, l.p1)),
            )
        }
        PathSeg::Quad(q) => {
            let (a, b) = q.subdivide();
            (PathSeg::Quad(a), PathSeg::Quad(b))
        }
        PathSeg::Cubic(c) => {
            let (a, b) = c.subdivide();
            (PathSeg::Cubic(a), PathSeg::Cubic(b))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LINE_COST, flatten_metered};
    use crate::encode::{Cost, Exhausted, Meter, Unmetered};
    use kurbo::{Affine, BezPath, Line, ParamCurveNearest, PathEl, Point, Rect, Shape};

    /// A meter that gives `left` steps and records each request.
    struct Recording {
        left: u64,
        asked: Vec<u64>,
        held: Vec<usize>,
    }

    impl Meter for Recording {
        fn work(&mut self, steps: Cost) -> bool {
            self.asked.push(steps.steps());
            match self.left.checked_sub(steps.steps()) {
                Some(left) => {
                    self.left = left;
                    true
                }
                None => false,
            }
        }

        fn hold(&mut self, bytes: usize) -> bool {
            self.held.push(bytes);
            true
        }
    }

    /// The lines of `path` flattened within `window`, each subpath closed.
    fn lines(
        path: &BezPath,
        window: Option<Rect>,
        meter: &mut dyn Meter,
    ) -> Result<Vec<Line>, Exhausted> {
        let mut lines = vec![];
        let (mut start, mut last) = (Point::ZERO, Point::ZERO);
        flatten_metered(
            path.iter(),
            Affine::IDENTITY,
            0.25,
            window,
            meter,
            |el, _| {
                match el {
                    PathEl::MoveTo(p) => (start, last) = (p, p),
                    PathEl::LineTo(p) => {
                        lines.push(Line::new(last, p));
                        last = p;
                    }
                    PathEl::ClosePath => {
                        lines.push(Line::new(last, start));
                        last = start;
                    }
                    PathEl::QuadTo(..) | PathEl::CurveTo(..) => {}
                }
                Ok(())
            },
        )?;
        Ok(lines)
    }

    /// How often `lines` wind around `p`.
    fn winding(lines: &[Line], p: Point) -> i32 {
        lines
            .iter()
            .filter(|l| (l.p0.y <= p.y) != (l.p1.y <= p.y))
            .filter(|l| l.p0.x + (p.y - l.p0.y) * (l.p1.x - l.p0.x) / (l.p1.y - l.p0.y) > p.x)
            .map(|l| if l.p1.y > l.p0.y { 1 } else { -1 })
            .sum()
    }

    /// A cubic far larger than the window it is needed in is flattened near the
    /// window alone, in lines that follow it there, and the chords away from it
    /// wind around each point of the window as the curve does: the review's cubic
    /// with control points at ±10²⁴ in a 100² window, which flattened whole asks
    /// for 570 million lines, passes on a few hundred; one at ±10⁶ fewer than
    /// whole. Each point at least a pixel from the curve is wound around as
    /// kurbo's exact winding of the curve says.
    #[test]
    fn a_curve_is_flattened_near_its_window() {
        let window = Rect::new(0.0, 0.0, 100.0, 100.0);
        for (far, most) in [(1e6, 600), (1e24, 600)] {
            let mut path = BezPath::new();
            path.move_to((10.0, 10.0));
            path.curve_to((far, 10.0), (-far, 50.0), (20.0, 90.0));
            path.close_path();
            let near = lines(&path, Some(window), &mut Unmetered).expect("unmetered");
            assert!(near.len() <= most, "{far}: {}", near.len());
            let curve = match path.segments().next() {
                Some(kurbo::PathSeg::Cubic(c)) => c,
                _ => unreachable!(),
            };
            let (closing, flattened) = near.split_last().expect("lines");
            assert_eq!(*closing, Line::new((20.0, 90.0), (10.0, 10.0)));
            for line in flattened {
                if window.contains(line.p0) && window.contains(line.p1) {
                    let middle = line.p0.midpoint(line.p1);
                    let off = curve.nearest(middle, 1e-9).distance_sq.sqrt();
                    assert!(off <= 0.25 + 1e-6, "{far}: {line:?} {off}");
                }
            }
            for i in 0..40 {
                for j in 0..40 {
                    let p = Point::new(1.3 + 2.5 * f64::from(i), 0.7 + 2.5 * f64::from(j));
                    if curve.nearest(p, 1e-9).distance_sq < 1.0 {
                        continue;
                    }
                    assert_eq!(winding(&near, p), path.winding(p), "{far}: {p:?}");
                }
            }
        }
    }

    /// Each element's work is asked before it is passed on, all of a curve's
    /// lines at once, and the halves waiting are held first: a meter that gives
    /// fewer steps gets no more elements than it paid for. A coordinate that is
    /// not finite stops the flattening.
    #[test]
    fn flattening_takes_its_work_before_it_passes_lines_on() {
        let window = Rect::new(0.0, 0.0, 100.0, 100.0);
        let mut path = BezPath::new();
        path.move_to((10.0, 10.0));
        path.curve_to((1e24, 10.0), (-1e24, 50.0), (20.0, 90.0));
        let mut meter = Recording {
            left: u64::MAX,
            asked: vec![],
            held: vec![],
        };
        let all = lines(&path, Some(window), &mut meter)
            .expect("given all")
            .len();
        assert!(!meter.held.is_empty());
        let paid: u64 = meter.asked.iter().sum();
        for left in [0, paid / 3, paid - 1] {
            let mut meter = Recording {
                left,
                asked: vec![],
                held: vec![],
            };
            let mut passed = 0_u64;
            let result = flatten_metered(
                path.iter(),
                Affine::IDENTITY,
                0.25,
                Some(window),
                &mut meter,
                |_, _| {
                    passed += 1;
                    Ok(())
                },
            );
            assert_eq!(result, Err(Exhausted), "{left}");
            assert!(
                passed * LINE_COST.steps() <= left,
                "{left}: {passed} of {all}"
            );
        }
        let mut infinite = BezPath::new();
        infinite.move_to((0.0, 0.0));
        infinite.curve_to((f64::INFINITY, 0.0), (0.0, 1.0), (1.0, 1.0));
        assert_eq!(
            lines(&infinite, Some(window), &mut Unmetered),
            Err(Exhausted)
        );
    }
}
