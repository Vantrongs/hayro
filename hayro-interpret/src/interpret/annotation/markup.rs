//! Markup annotations drawn from geometry alone. Square, Circle and Ink follow the
//! appearances `PDFium` generates (`core/fpdfdoc/cpdf_generateap.cpp`,
//! BSD-3-Clause), with their geometry and defaults. The text markup shapes
//! (`Highlight`, `Underline`, `StrikeOut`, `Squiggly`) are drawn in each
//! quadrilateral's own orientation, sized from its height so that they cover what
//! common viewers draw, and clipped as they clip them: to the box of the
//! quadrilaterals, a line width wider on each side.

use super::entries::{Derived, Entries, Items, Value};
use super::out::{Color, MITER_LIMIT, Out};
use super::{MOST_PIECES, Unsupported, border_width, c_color, color_of, dash, rect};
use hayro_syntax::object::dict::keys::{C, IC, INKLIST, QUADPOINTS};
use kurbo::{Point, Rect, Vec2};
use std::rc::Rc;

/// Square and Circle: /IC fills the shape, /C strokes it at the border width, inside
/// `/Rect` less `/RD`. Unlike `PDFium`, an absent `/C` means no border: the spec gives
/// it no default. A border of width 0 is the thinnest line, not none. As viewers draw
/// it, each side lies inside its rectangle difference or half the border width,
/// whichever is more, whether a border is drawn or not, and the shape is at least
/// 1 wide and high, starting where its left and bottom sides are, however narrow
/// `/Rect` is.
pub(super) fn shape(annot: &Entries<'_>, circle: bool, out: &mut Out) -> Option<()> {
    let fill = color_of(annot, IC);
    let width = border_width(annot);
    let stroke = color_of(annot, C);
    if fill.is_none() && stroke.is_none() {
        return None;
    }
    let r = rect(annot)?;
    // Each side moved in by its rectangle difference (`/RD`: left, bottom, right,
    // top; 0 for one it lacks), or by half the border width where that is more.
    let rd = annot.numbers(b"RD", 4).unwrap_or_else(|| vec![0.0; 4]);
    let inset = |i: usize| rd[i].max(width / 2.0);
    let (x0, y0) = (r.x0 + inset(0), r.y0 + inset(1));
    let (x1, y1) = (
        (r.x1 - inset(2)).max(x0 + 1.0),
        (r.y1 - inset(3)).max(y0 + 1.0),
    );
    // Viewers clip the shape to its box: `/Rect`, grown to the right and up to
    // hold the shape and its insets where it is too small for them.
    out.op("q");
    out.clip(Rect::new(r.x0, r.y0, x1 + inset(2), y1 + inset(3)))?;
    if let Some(stroke) = &stroke {
        out.stroke_color(stroke)?;
        out.stroke_state(width, &dash(annot))?;
    }
    if let Some(fill) = &fill {
        out.fill_color(fill)?;
    }
    if circle {
        // Four cubic quarter arcs, with the control point distance of [`super::circle`].
        const K: f64 = 0.551_915;
        let (mx, my) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let (dx, dy) = (K * (x1 - x0) / 2.0, K * (y1 - y0) / 2.0);
        out.nums(&[mx, y1], "m")?;
        out.nums(&[mx + dx, y1, x1, my + dy, x1, my], "c")?;
        out.nums(&[x1, my - dy, mx + dx, y0, mx, y0], "c")?;
        out.nums(&[mx - dx, y0, x0, my - dy, x0, my], "c")?;
        out.nums(&[x0, my + dy, mx - dx, y1, mx, y1], "c")?;
    } else {
        out.nums(&[x0, y0, x1 - x0, y1 - y0], "re")?;
    }
    out.op(match (stroke.is_some(), fill.is_some()) {
        (true, true) => "b",
        (true, false) => "s",
        _ => "f",
    });
    out.op("Q");
    Some(())
}

/// A quadrilateral of `/QuadPoints`, its points in the order of the array: as text
/// markup gives them, the top left and right, then the bottom left and right of the
/// text it marks, whichever way that text runs.
#[derive(Clone, Copy)]
pub(super) struct Quad(pub(super) [Point; 4]);

impl Quad {
    /// The height of the text: from its bottom left to its top left.
    fn height(&self) -> f64 {
        (self.0[0] - self.0[2]).hypot()
    }

    /// The line a fraction `f` of the way up from the bottom edge to the top edge
    /// along each side, from the left side to the right.
    fn across(&self, f: f64) -> (Point, Point) {
        let [tl, tr, bl, br] = self.0;
        (bl.lerp(tl, f), br.lerp(tr, f))
    }

    /// The box of its points, `grow` wider on each side.
    pub(super) fn bounds(&self, grow: f64) -> Rect {
        let [a, b, c, d] = self.0;
        Rect::from_points(a, b).union_pt(c).union_pt(d).inset(grow)
    }
}

/// The quadrilaterals of a `/QuadPoints` array, every number in its place; a last
/// one cut short has 0 for the coordinates it lacks. Made once for the document
/// ([`Derived`]), with what text markup draws of them, which annotations sharing
/// the array each look up rather than walk it.
pub(super) struct Quads {
    pub(super) all: Vec<Quad>,
    /// The box of each, a line width wider on each side; `None` when it has no
    /// area ([`marked`]).
    clip: Option<Rect>,
    /// Those of some height, which a squiggly draws.
    high: Vec<Quad>,
}

impl Derived for Quads {
    fn derive(items: &Items<'_>) -> Self {
        let mut n = items.numbers();
        n.resize(n.len().next_multiple_of(8), 0.0);
        let all: Vec<Quad> = n
            .as_chunks::<8>()
            .0
            .iter()
            .map(|q| {
                Quad([
                    Point::new(q[0], q[1]),
                    Point::new(q[2], q[3]),
                    Point::new(q[4], q[5]),
                    Point::new(q[6], q[7]),
                ])
            })
            .collect();
        let clip = union(all.iter().map(|q| q.bounds(line_width(q))))
            .filter(|clip| clip.width() > 0.0 && clip.height() > 0.0);
        let high = all.iter().filter(|q| q.height() > 0.0).copied().collect();
        Self { all, clip, high }
    }
}

/// The quadrilaterals of `/QuadPoints` ([`Quads`]); `None` when there is no such
/// array.
pub(super) fn quads(annot: &Entries<'_>) -> Option<Rc<Quads>> {
    Some(annot.items(QUADPOINTS)?.derived::<Quads>())
}

/// The width of a text markup line on `quad`: a sixteenth of its height.
fn line_width(quad: &Quad) -> f64 {
    quad.height() / 16.0
}

/// The union of `boxes`; `None` for none.
fn union(boxes: impl IntoIterator<Item = Rect>) -> Option<Rect> {
    boxes.into_iter().reduce(|a, b| a.union(b))
}

/// The quadrilaterals of a text markup annotation and the box viewers clip it to: the
/// box of each, a line width wider on each side. `None` without quadrilaterals, or
/// when that box has no area: quadrilaterals of no height mark no text.
fn marked(annot: &Entries<'_>) -> Option<(Rc<Quads>, Rect)> {
    let quads = quads(annot)?;
    let clip = quads.clip?;
    Some((quads, clip))
}

/// Highlight: each quad filled with /C (default yellow), blended with Multiply, its
/// left and right sides bulging out to a third of its height, turned with its bottom
/// edge. A quad whose bottom edge has no length has no direction: viewers put the
/// curves' control points at the origin, and so does this.
pub(super) fn highlight(annot: &Entries<'_>, out: &mut Out) -> Option<()> {
    let fill = c_color(annot, Color::Rgb([1.0, 1.0, 0.0]));
    let (quads, around) = marked(annot)?;
    let shapes: Vec<(Quad, Option<[Point; 4]>)> = quads
        .all
        .iter()
        .map(|&q| {
            let [tl, tr, bl, br] = q.0;
            // √2/6 of the height, on each axis of the turned quad.
            let k = q.height() * std::f64::consts::SQRT_2 / 6.0;
            let along = br - bl;
            let len = along.hypot();
            let controls = (len > 0.0).then(|| {
                let (cos, sin) = (along.x / len, along.y / len);
                let turn = |x: f64, y: f64| Vec2::new(x * cos - y * sin, x * sin + y * cos);
                [
                    bl + turn(-k, k),
                    tl + turn(-k, -k),
                    tr + turn(k, -k),
                    br + turn(k, k),
                ]
            });
            (q, controls)
        })
        .collect();
    let clip = shapes
        .iter()
        .filter_map(|(_, c)| *c)
        .flatten()
        .fold(around, |r, p| r.union_pt(p));
    out.op("q");
    out.clip(clip)?;
    out.fill_color(&fill)?;
    for (q, controls) in shapes {
        let [tl, tr, bl, br] = q.0;
        let [c1, c2, c3, c4] = controls.unwrap_or([Point::ZERO; 4]);
        out.nums(&[bl.x, bl.y], "m")?;
        out.nums(&[c1.x, c1.y, c2.x, c2.y, tl.x, tl.y], "c")?;
        out.nums(&[tr.x, tr.y], "l")?;
        out.nums(&[c3.x, c3.y, c4.x, c4.y, br.x, br.y], "c")?;
        out.op("f");
    }
    out.op("Q");
    Some(())
}

pub(super) enum Line {
    Under,
    Through,
}

/// `Underline` and `StrikeOut`: per quad, a line in /C (default black), a sixteenth
/// of the quad's height wide, across it from side to side, a seventh or three
/// sevenths of the way up each side.
pub(super) fn text_lines(annot: &Entries<'_>, line: Line, out: &mut Out) -> Option<()> {
    let stroke = c_color(annot, Color::BLACK);
    let (quads, clip) = marked(annot)?;
    out.op("q");
    out.clip(clip)?;
    out.stroke_color(&stroke)?;
    for q in &quads.all {
        out.stroke_state(line_width(q), &[])?;
        let (a, b) = q.across(match line {
            Line::Under => 1.0 / 7.0,
            Line::Through => 3.0 / 7.0,
        });
        out.nums(&[a.x, a.y], "m")?;
        out.nums(&[b.x, b.y], "l")?;
        out.op("S");
    }
    out.op("Q");
    Some(())
}

/// `Squiggly`: per quad, a zigzag in /C (default black), a sixteenth of the quad's
/// height `h` wide, between its bottom edge and the line a seventh of the way up
/// (see [`zigzag`]).
pub(super) fn squiggly(
    annot: &Entries<'_>,
    visible: Rect,
    out: &mut Out,
) -> Result<Option<()>, Unsupported> {
    let stroke = c_color(annot, Color::BLACK);
    let Some((quads, clip)) = marked(annot) else {
        return Ok(None);
    };
    // Every way to fail to write is the coordinates'.
    let wrote = |o: Option<()>| o.ok_or(Unsupported::Coordinates);
    out.op("q");
    wrote(out.clip(clip))?;
    wrote(out.stroke_color(&stroke))?;
    if visible.overlaps(clip) {
        let shown = visible.intersect(clip);
        // A quad of no height draws nothing ([`zigzag`]).
        for q in &quads.high {
            zigzag(q, shown, out)?;
        }
    }
    out.op("Q");
    Ok(Some(()))
}

/// The zigzag of a `Squiggly` quad as viewers draw it: steps of a seventh of its
/// height `h` along its bottom edge, from its bottom left, to the first step at or
/// past its bottom right; the corners alternately on the bottom edge and on the line
/// a seventh of the way up each side, stroked `h`/16 wide. A quad of no height
/// draws nothing.
///
/// Only the steps whose stroke can reach `shown`, the part of the page the clip lets
/// show, are drawn: those that come within half the miter limit times the width of
/// it, which holds the stroke and every join. Where a neighbouring step is left out,
/// the run drawn ends there with a butt cap; the join it lacks lies outside `shown`,
/// as the corner they share is further than that from it. Every step is drawn as it
/// is, however fine: the content can be scaled and zoomed any amount. A quad that
/// would draw more than [`MOST_PIECES`] is [`Unsupported::Coordinates`]: one whose
/// part on the page is more than about 37 000 times as long as high, which no text
/// is.
///
/// The steps drawn are found by halving the range of steps, from all of them down
/// to single steps, and leaving out each range whose corners' convex hull, that of
/// its first and last corner on each line, is further than that from `shown`
/// ([`near`]). The steps run from one line to the other, so a range near `shown`
/// holds a step near it, but for those at the ends of the part of the zigzag near
/// `shown`: at most a few at each depth. The work is within about twice the depth,
/// the bits of the number of steps, times the steps drawn and those few.
fn zigzag(q: &Quad, shown: Rect, out: &mut Out) -> Result<(), Unsupported> {
    let wrote = |o: Option<()>| o.ok_or(Unsupported::Coordinates);
    let [_, _, bl, br] = q.0;
    let width = line_width(q);
    let delta = q.height() / 7.0;
    // Viewers never end a zigzag of no step.
    if delta.is_nan() || delta <= 0.0 {
        return Ok(());
    }
    wrote(out.stroke_state(width, &[]))?;
    let along = br - bl;
    let len = along.hypot();
    if len == 0.0 {
        return Ok(());
    }
    let (top_left, top_right) = q.across(1.0 / 7.0);
    let bottom = |t: f64| bl.lerp(br, t);
    let top = |t: f64| top_left.lerp(top_right, t);
    let hull = |ta: f64, tb: f64| [bottom(ta), bottom(tb), top(tb), top(ta)];
    let reach = MITER_LIMIT * width / 2.0;
    // The corners are `delta` apart along the bottom edge, at `t = k delta / len`.
    let steps = (len / delta).ceil();
    if !steps.is_finite() {
        // Steps too many to number, which no viewer tells apart: refused where the
        // quad can show.
        return if near(&hull(0.0, 1.0), shown, reach) {
            Err(Unsupported::Coordinates)
        } else {
            Ok(())
        };
    }
    let t = |k: f64| k * delta / len;
    let corner = |k: f64| {
        if k % 2.0 == 0.0 {
            bottom(t(k))
        } else {
            top(t(k))
        }
    };
    // The ranges of steps left to look at, the first last; step `k` runs from
    // corner `k` to corner `k + 1`.
    let mut ranges = vec![(0.0, steps)];
    let mut drawn = 0.0;
    // The corner the last step drawn ends at.
    let mut until = None;
    while let Some((a, b)) = ranges.pop() {
        super::work(1);
        if b - a > 1.0 {
            if near(&hull(t(a), t(b)), shown, reach) {
                let mid = ((a + b) / 2.0).floor();
                // Past 2^53, neighbouring steps' numbers are no longer apart.
                if !(a < mid && mid < b) {
                    return Err(Unsupported::Coordinates);
                }
                ranges.push((mid, b));
                ranges.push((a, mid));
            }
            continue;
        }
        let (from, to) = (corner(a), corner(b));
        if !near(&[from, to], shown, reach) {
            continue;
        }
        drawn += 1.0;
        if drawn > MOST_PIECES {
            return Err(Unsupported::Coordinates);
        }
        if until != Some(a) {
            wrote(out.nums(&[from.x, from.y], "m"))?;
        }
        wrote(out.nums(&[to.x, to.y], "l"))?;
        until = Some(b);
    }
    if until.is_some() {
        out.op("S");
    }
    Ok(())
}

/// Whether the convex hull of `points` comes within `reach` of `r`; also where a
/// point is no finite number, which cannot then be written.
fn near(points: &[Point], r: Rect, reach: f64) -> bool {
    if !points.iter().all(|p| p.is_finite()) {
        return true;
    }
    let r = r.abs();
    let (c, half) = (r.center(), Vec2::new(r.width() / 2.0, r.height() / 2.0));
    // Apart along `n`: the projections of the hull and of `r` do not meet.
    let apart = |n: Vec2| {
        let extent = n.x.abs() * half.x + n.y.abs() * half.y;
        let along = points.iter().map(|p| n.dot(*p - c));
        let (lo, hi) = along.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
        lo > extent || hi < -extent
    };
    let pairs = || {
        (0..points.len())
            .flat_map(move |i| (i + 1..points.len()).map(move |j| (points[i], points[j])))
    };
    // Two convex shapes that meet are apart along no side's normal.
    let axes = [Vec2::new(1.0, 0.0), Vec2::new(0.0, 1.0)];
    let sides = pairs().map(|(p, q)| (q - p).turn_90());
    if !axes.into_iter().chain(sides).any(apart) {
        return true;
    }
    // Apart, they are nearest at a corner of one and a side or corner of the other.
    let to_rect = |p: Point| {
        let dx = (r.x0 - p.x).max(p.x - r.x1).max(0.0);
        let dy = (r.y0 - p.y).max(p.y - r.y1).max(0.0);
        Vec2::new(dx, dy).hypot()
    };
    let to_side = |p: Point, a: Point, b: Point| {
        let ab = b - a;
        let f = (p - a).dot(ab) / ab.hypot2();
        let f = if f.is_nan() { 0.0 } else { f.clamp(0.0, 1.0) };
        (p - a.lerp(b, f)).hypot()
    };
    let corners = [
        Point::new(r.x0, r.y0),
        Point::new(r.x1, r.y0),
        Point::new(r.x1, r.y1),
        Point::new(r.x0, r.y1),
    ];
    points.iter().any(|&p| to_rect(p) <= reach)
        || corners
            .iter()
            .any(|&k| pairs().any(|(a, b)| to_side(k, a, b) <= reach))
}

/// `Ink`: the `/InkList` paths as polylines of one path, stroked once in /C at the
/// border width, with the border dash, round ends and joins; a path of one point is
/// a dot. Unlike `PDFium`, an absent `/C` draws nothing: the spec gives it no
/// default.
pub(super) fn ink(annot: &Entries<'_>, out: &mut Out) -> Option<()> {
    let width = border_width(annot);
    let stroke = color_of(annot, C)?;
    let paths = annot.items(INKLIST)?.derived::<Paths>();
    if paths.0.is_empty() {
        return None;
    }
    out.stroke_color(&stroke)?;
    out.stroke_state(width, &dash(annot))?;
    // Round ends and corners, as a pen draws.
    out.op("1 J 1 j");
    for p in &paths.0 {
        out.nums(&p[..2], "m")?;
        // A path of one point is a dot.
        let rest = if p.len() < 4 { &p[..2] } else { &p[2..] };
        for pt in rest.as_chunks::<2>().0 {
            out.nums(pt, "l")?;
        }
    }
    // One stroke of all paths, as viewers draw it: where paths cross, a translucent
    // ink is laid once.
    out.op("S");
    Some(())
}

/// The paths of an `/InkList` that draw, the numbers of each, at least two: made
/// once for the document ([`Derived`]), so that annotations sharing a list walk
/// what it holds that draws nothing once.
struct Paths(Vec<Vec<f64>>);

impl Derived for Paths {
    fn derive(items: &Items<'_>) -> Self {
        let paths = items.iter().filter_map(Value::items).map(|p| p.numbers());
        Self(paths.filter(|p| p.len() >= 2).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::super::entries::Entries;
    use super::super::out::Out;
    use super::squiggly;
    use hayro_syntax::object::{Dict, FromBytes};
    use kurbo::Rect;

    /// A page large enough for every quad below but the huge ones.
    const PAGE: Rect = Rect::new(0.0, 0.0, 1e8, 1e8);

    fn squiggly_content(quads: &str, visible: Rect) -> String {
        let annot = format!("<< /Subtype /Squiggly /Rect [0 0 1 1] /QuadPoints [{quads}] >>");
        let dict = Dict::from_bytes(annot.as_bytes()).expect("valid dict");
        let mut out = Out::default();
        squiggly(&Entries::detached(dict), visible, &mut out)
            .expect("drawable")
            .expect("an appearance");
        out.finish().expect("written").0
    }

    /// The corners of the zigzags drawn in `c`.
    fn corners(c: &str) -> Vec<(f64, f64)> {
        c.lines()
            .filter(|l| l.ends_with(" l") || l.ends_with(" m"))
            .filter(|l| l.split(' ').count() == 3)
            .map(|l| {
                let n: Vec<f64> = l.split(' ').take(2).map(|v| v.parse().unwrap()).collect();
                (n[0], n[1])
            })
            .collect()
    }

    #[test]
    fn squiggly_steps_a_seventh_of_the_height_to_the_first_past_its_end() {
        // 14 pt high: steps of 2 pt; the last ends past the quad, where the clip
        // cuts it.
        let c = squiggly_content("10 24 15 24 10 10 15 10", PAGE);
        assert!(c.starts_with("q\n9.125 9.125 6.75 15.75 re\nW n\n"), "{c}");
        assert!(
            c.ends_with("10 10 m\n12 12 l\n14 10 l\n16 12 l\nS\nQ\n"),
            "{c}"
        );
        // A quad of a whole number of steps ends at its corner.
        let c = squiggly_content("10 24 14 24 10 10 14 10", PAGE);
        assert!(c.ends_with("10 10 m\n12 12 l\n14 10 l\nS\nQ\n"), "{c}");
    }

    /// A slanted quad's zigzag runs along its bottom edge, its corners on the bottom
    /// edge and on the line a seventh of the way up each side, as viewers draw it.
    #[test]
    fn a_slanted_squiggly_follows_its_quad() {
        let c = squiggly_content("30 50 100 120 45 25 115 105", PAGE);
        let got = corners(&c);
        let expected = [
            (45.0, 25.0),
            (45.599794, 31.649914),
            (50.4853, 31.268912),
            (51.08509, 37.806879),
        ];
        for (g, e) in got.iter().zip(expected) {
            assert!((g.0 - e.0).abs() < 1e-3 && (g.1 - e.1).abs() < 1e-3, "{c}");
        }
    }

    /// Where 32-bit numbers are a unit or more apart, viewers' zigzags round
    /// otherwise than ours (in f32, 2^25 + 2 == 2^25: positions accumulated in f32
    /// never reach x1): such a quad is not drawn but refused.
    #[test]
    fn squiggly_at_large_coordinates_is_unsupported() {
        let annot = "<< /Subtype /Squiggly /Rect [0 0 1 1] \
            /QuadPoints [33554432 24 33554440 24 33554432 10 33554440 10] >>";
        let dict = Dict::from_bytes(annot.as_bytes()).expect("valid dict");
        let mut out = Out::default();
        let got = squiggly(&Entries::detached(dict), PAGE, &mut out);
        assert!(matches!(got, Err(super::Unsupported::Coordinates)));
    }

    /// A quad's zigzag is the same alone and among quads that take more than
    /// 100 000 steps in all.
    #[test]
    fn a_zigzag_does_not_depend_on_the_other_quads() {
        let page = Rect::new(0.0, 0.0, 2100.0, 2100.0);
        let quad = "1 24 2049 24 1 10 2049 10 ";
        let zigzags = |c: &str| -> Vec<String> {
            c.split("] 0 d\n")
                .skip(1)
                .map(|z| z.split("S\n").next().unwrap().to_owned())
                .collect()
        };
        let alone = zigzags(&squiggly_content(quad, page));
        assert_eq!(alone[0].matches(" l\n").count(), 1024);
        let among = zigzags(&squiggly_content(&quad.repeat(101), page));
        assert_eq!(among.len(), 101);
        assert!(among.iter().all(|z| *z == alone[0]));
    }

    /// The distance from the segment `a`-`b` to `r`.
    fn distance(a: (f64, f64), b: (f64, f64), r: Rect) -> f64 {
        let point = |(x, y): (f64, f64)| {
            let dx = (r.x0 - x).max(x - r.x1).max(0.0);
            let dy = (r.y0 - y).max(y - r.y1).max(0.0);
            dx.hypot(dy)
        };
        // Where the segment crosses `r`, if it does: the part of it within each of
        // the rectangle's bands.
        let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
        for (from, d, min, max) in [(a.0, b.0 - a.0, r.x0, r.x1), (a.1, b.1 - a.1, r.y0, r.y1)] {
            if d == 0.0 {
                if from < min || from > max {
                    hi = -1.0;
                }
            } else {
                let (s, t) = ((min - from) / d, (max - from) / d);
                lo = lo.max(s.min(t));
                hi = hi.min(s.max(t));
            }
        }
        if lo <= hi {
            return 0.0;
        }
        let to_segment = |(x, y): (f64, f64)| {
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let f = (((x - a.0) * dx + (y - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
            (x - a.0 - f * dx).hypot(y - a.1 - f * dy)
        };
        [(r.x0, r.y0), (r.x1, r.y0), (r.x0, r.y1), (r.x1, r.y1)]
            .map(to_segment)
            .into_iter()
            .chain([point(a), point(b)])
            .fold(f64::INFINITY, f64::min)
    }

    /// A quad whose line a seventh of the way up leans far back over the page, while
    /// its bottom edge lies beside it: of its thousand steps, those whose stroke can
    /// reach the page, within half the miter limit times the line width of it, are
    /// drawn, and no other (the 30 steps from 86 on; before, every step was).
    #[test]
    fn a_steep_zigzag_draws_the_steps_that_reach_the_page() {
        let page = Rect::new(0.0, 0.0, 200.0, 200.0);
        let c = squiggly_content("300 7 -13700 14000 300 0 1300 0", page);
        // Height 7: steps of 1 along the bottom edge from x = 300, a line 7/16 wide;
        // the line a seventh of the way up runs from (300, 1) to (1300 - 15000 / 7,
        // 2000).
        let (top_left, top_right) = ((300.0, 1.0), (1300.0 - 15000.0 / 7.0, 2000.0));
        let corner = |k: usize| {
            let t = k as f64 / 1000.0;
            if k.is_multiple_of(2) {
                (300.0 + 1000.0 * t, 0.0)
            } else {
                (
                    top_left.0 + (top_right.0 - top_left.0) * t,
                    top_left.1 + (top_right.1 - top_left.1) * t,
                )
            }
        };
        let reach = 10.0 * (7.0 / 16.0) / 2.0;
        let near: Vec<usize> = (0..1000)
            .filter(|&k| distance(corner(k), corner(k + 1), page) <= reach)
            .collect();
        // The runs of steps drawn, as the steps of each.
        let mut drawn = vec![];
        let mut at = None;
        for line in c.lines() {
            let n: Vec<&str> = line.split(' ').collect();
            let (x, y) = match n[..] {
                [x, y, "m" | "l"] => (x.parse::<f64>().unwrap(), y.parse::<f64>().unwrap()),
                _ => continue,
            };
            // The corner of the bottom edge a step starts or ends at names it.
            let k = if line.ends_with(" m") {
                None
            } else {
                let (from_x, from_y) = at.unwrap();
                Some(if from_y == 0.0 { from_x } else { x } - 300.0)
            };
            if let Some(k) = k {
                let k = if at.unwrap().1 == 0.0 { k } else { k - 1.0 };
                drawn.push(k.round() as usize);
            }
            at = Some((x, y));
        }
        assert!(near.len() > 20 && near.len() < 100, "{near:?}");
        assert_eq!(drawn, near, "{c}");
    }

    /// A quad 4·10^6 or 8·10^6 pt wide, starting at its left end or far left of the page,
    /// draws the steps across the page and a few beyond each side, with the corners
    /// its own start puts them at.
    #[test]
    fn a_huge_quad_draws_the_steps_the_page_shows() {
        let page = Rect::new(100.0, 0.0, 712.0, 800.0);
        for x0 in ["0", "-4000000", "-3999999"] {
            let c = squiggly_content(&format!("{x0} 24 4000000 24 {x0} 10 4000000 10"), page);
            let corners = corners(&c);
            let start: f64 = x0.parse().unwrap();
            let (first, last) = (corners[0].0, corners[corners.len() - 1].0);
            assert!((90.0..=98.0).contains(&first), "{x0}: {first}");
            assert!((714.0..=722.0).contains(&last), "{x0}: {last}");
            assert_eq!(corners.len(), ((last - first) / 2.0) as usize + 1, "{x0}");
            for &(x, y) in &corners {
                let i = ((x - start) / 2.0).round() as i64;
                assert_eq!(y, if i % 2 == 0 { 10.0 } else { 12.0 }, "{x0} at {x}");
            }
        }
        // Off the page, nothing.
        let c = squiggly_content("0 2000 4000000 2000 0 1986 4000000 1986", page);
        assert!(corners(&c).is_empty(), "{c}");
    }
}
