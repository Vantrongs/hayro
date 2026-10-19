//! Markup annotations drawn from geometry alone. Square, Circle and Ink follow the
//! appearances `PDFium` generates (`core/fpdfdoc/cpdf_generateap.cpp`,
//! BSD-3-Clause), with their geometry and defaults. The text markup shapes
//! (`Highlight`, `Underline`, `StrikeOut`, `Squiggly`) are drawn in each
//! quadrilateral's own orientation, sized from its height so that they cover what
//! common viewers draw, and clipped as they clip them: to the box of the
//! quadrilaterals, a line width wider on each side.

use super::entries::{Derived, Entries, Items, Value};
use super::out::{Color, MITER_LIMIT, Out, Region, Rounding, as_written, stroke_reach};
use super::{Unsupported, border_width, c_color, color_of, dash, most_pieces, rect};
use hayro_syntax::object::dict::keys::{C, IC, INKLIST, QUADPOINTS};
use kurbo::{Point, Rect, Vec2};
use std::f64::consts::SQRT_2;
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
    let (Rect { x0, y0, x1, y1 }, clip) = shape_box(annot)?;
    out.op("q");
    out.clip(clip)?;
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

/// The shape of a Square or Circle and the box viewers clip it to ([`shape`]):
/// `/Rect` with each side moved in by its rectangle difference (`/RD`: left,
/// bottom, right, top; 0 for one it lacks), or by half the border width where
/// that is more, at least 1 wide and high; and `/Rect`, grown to the right and up
/// to hold the shape and its insets where it is too small for them.
pub(super) fn shape_box(annot: &Entries<'_>) -> Option<(Rect, Rect)> {
    let width = border_width(annot);
    let r = rect(annot)?;
    let rd = annot.numbers(b"RD", 4).unwrap_or_else(|| vec![0.0; 4]);
    let inset = |i: usize| rd[i].max(width / 2.0);
    let (x0, y0) = (r.x0 + inset(0), r.y0 + inset(1));
    let (x1, y1) = (
        (r.x1 - inset(2)).max(x0 + 1.0),
        (r.y1 - inset(3)).max(y0 + 1.0),
    );
    let clip = Rect::new(r.x0, r.y0, x1 + inset(2), y1 + inset(3));
    Some((Rect::new(x0, y0, x1, y1), clip))
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
pub(super) fn marked(annot: &Entries<'_>) -> Option<(Rc<Quads>, Rect)> {
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
    let (shapes, clip) = highlights(annot)?;
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

/// A quad a highlight fills, with the control points of its sides where its bottom
/// edge has a direction ([`highlight`]).
type Rounded = (Quad, Option<[Point; 4]>);

/// Each quad a highlight fills ([`Rounded`]), and the box viewers clip them to: the
/// box of the quads, a line width wider on each side, and of the control points.
pub(super) fn highlights(annot: &Entries<'_>) -> Option<(Vec<Rounded>, Rect)> {
    let (quads, around) = marked(annot)?;
    let shapes: Vec<Rounded> = quads
        .all
        .iter()
        .map(|&q| {
            let [tl, tr, bl, br] = q.0;
            // √2/6 of the height, on each axis of the turned quad.
            let k = q.height() * SQRT_2 / 6.0;
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
    Some((shapes, clip))
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
/// (see [`zigzag`]): the steps that paint where the page shows them under the
/// clip.
pub(super) fn squiggly(annot: &Entries<'_>, out: &mut Out) -> Result<Option<()>, Unsupported> {
    let stroke = c_color(annot, Color::BLACK);
    let Some((quads, clip)) = marked(annot) else {
        return Ok(None);
    };
    out.op("q");
    out.clip(clip);
    out.stroke_color(&stroke);
    // What the page shows under the clip, as written.
    let region = out.region();
    if !region.is_empty() {
        // One budget for the steps the annotation's zigzags look at one by one.
        let mut pen = Run {
            spent: 0.0,
            most: most_pieces(),
            until: None,
        };
        // A quad of no height draws nothing ([`zigzag`]).
        for q in &quads.high {
            zigzag(q, &region, &mut pen, out)?;
        }
    }
    out.op("Q");
    Ok(Some(()))
}

/// The zigzag of a `Squiggly` quad as viewers draw it: steps of a seventh of its
/// height `h` along its bottom edge, from its bottom left, to the first step at or
/// past its bottom right; the corners alternately on the bottom edge and on the line
/// a seventh of the way up each side, stroked `h`/16 wide with butt caps and joins
/// mitred up to [`MITER_LIMIT`]. A quad of no height draws nothing.
///
/// Only the steps that paint in `region`, what the page shows under the clip, are
/// drawn and counted ([`Zigzag::paints`]): each as written, its corners and width
/// as a viewer reads them, by its stroke and the joins it shares with the steps of
/// some length either side of it. A step of no length as written paints nothing of
/// its own and is not written; the steps either side of it then meet at its corner.
/// Where a neighbouring step is left out, the run drawn ends there with a butt cap;
/// the join it lacks paints nothing in `region`, as the neighbour, whose join it is
/// too, would be drawn otherwise. Every step is drawn as it is, however fine: the
/// content can be scaled and zoomed any amount. An annotation whose quads would
/// draw more steps than [`super::set_most_pieces`] allows is
/// [`Unsupported::Coordinates`]: at the default, one whose part on the page is
/// more than about 37 000 times as long as high, which no text is. The steps looked
/// at one by one that paint nothing count too, so that the work stays within a few
/// times the most: they are a few at each end of what paints, but where the zigzag
/// runs along the edge of `region` with its joins' tips within what writing moves
/// them by of it, or its steps are about as fine as that, where no bound on a range
/// tells them from steps that paint. Only a quad with more steps than the most
/// within its joins' reach of `region`, as long as that, refuses for them.
///
/// The steps drawn are found by halving the range of steps, from all of them down
/// to single steps: a range is left out when what its steps can paint is apart from
/// `region` ([`Zigzag::may_paint`]), and drawn whole when its corners, as written,
/// lie in it. What the steps of a range paint lies in the convex hull of the first
/// and last step's strokes and joins of each kind (on the bottom edge, on the line
/// above): exactly, where the quad is a parallelogram (the steps of each kind are
/// then the same shape moved along), and within a bound that shrinks with the range
/// otherwise; writing moves each corner by at most its size times 2^-24, which the
/// bound adds for the corners of that range, those that can come near `region`, not
/// for the zigzag's farthest. The steps run from one line to the other and the
/// joins stand out from the lines between them, so a range that hull meets holds a
/// step that paints, but for those at the ends of what the zigzag paints in
/// `region`: at most a few at each depth. The work is within about twice the depth,
/// the bits of the number of steps, times those few and the ranges drawn; for a
/// quad that is no parallelogram, also the ranges whose bound reaches `region`
/// where their steps do not, fewer the closer the quad is to a parallelogram, and
/// for steps shorter than a few times what writing moves their corners, those whose
/// joins' reach comes that close.
fn zigzag(q: &Quad, region: &Region, pen: &mut Run, out: &mut Out) -> Result<(), Unsupported> {
    let [_, _, bl, br] = q.0;
    let width = line_width(q);
    let delta = q.height() / 7.0;
    // Viewers never end a zigzag of no step.
    if delta.is_nan() || delta <= 0.0 {
        return Ok(());
    }
    out.stroke_state(width, &[]);
    let along = br - bl;
    let len = along.hypot();
    if len == 0.0 {
        return Ok(());
    }
    let (top_left, top_right) = q.across(1.0 / 7.0);
    let top_along = top_right - top_left;
    // What computing a corner in 64-bit numbers can move it by.
    let size = |v: Vec2| v.x.abs().max(v.y.abs());
    let error = (size(bl.to_vec2()) + size(along) + size(top_left.to_vec2()) + size(top_along))
        * f64::EPSILON
        * 4.0;
    let z = Zigzag {
        bl,
        along,
        top_left,
        top_along,
        // The corners are `delta` apart along the bottom edge, at `t = k dt`.
        dt: delta / len,
        // Half the width as written.
        half: as_written(width) / 2.0,
        error,
        lead: Vec2::new(
            bl.x.abs().max(top_left.x.abs()),
            bl.y.abs().max(top_left.y.abs()),
        ),
        step: Vec2::new(
            along.x.abs().max(top_along.x.abs()),
            along.y.abs().max(top_along.y.abs()),
        ) * (delta / len),
        region: region.clone(),
    };
    let steps = (len / delta).ceil();
    if !steps.is_finite() {
        // Steps too many to number, which no viewer tells apart: refused where the
        // quad can show.
        let hull = z.hull(0.0, 1.0);
        return if region.near(&hull, z.reach(z.slack(&hull))) {
            Err(Unsupported::Coordinates)
        } else {
            Ok(())
        };
    }
    pen.until = None;
    // The ranges of steps left to look at, the first last; step `k` runs from
    // corner `k` to corner `k + 1`.
    let mut ranges = vec![(0.0, steps)];
    while let Some((a, b)) = ranges.pop() {
        super::work(1);
        let hull = z.hull(z.t(a), z.t(b));
        let (slack, place) = (z.slack(&hull), z.place(b));
        let near = |reach: f64| region.may_show(&hull, reach, place).is_some();
        if !near(z.reach(slack)) || z.still(a, b) || !z.written_near(a, b) {
            continue;
        }
        let moved = place.bound();
        if region.holds(&hull, slack + moved.x.max(moved.y)) {
            // Every corner as written lies in `region`: every step of some length
            // paints.
            pen.draw(&z, a, b, out)?;
            continue;
        }
        if b - a > 1.0 {
            // Where the corners' hull meets `region`, a step crosses it but where
            // `region` lies between two steps; else what the strokes and joins
            // paint decides.
            if near(slack * SQRT_2) || z.may_paint(a, b, slack) {
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
        if z.paints(a, steps) {
            pen.draw(&z, a, b, out)?;
        } else {
            pen.spend()?;
        }
    }
    if pen.until.is_some() {
        out.op("S");
    }
    Ok(())
}

/// The steps of an annotation's zigzags looked at one by one so far, those drawn
/// and those that turned out to paint nothing, the most there may be, and the
/// corner the last step drawn of the current zigzag ends at.
struct Run {
    spent: f64,
    most: f64,
    until: Option<f64>,
}

impl Run {
    /// Counts a step looked at that paints nothing.
    fn spend(&mut self) -> Result<(), Unsupported> {
        self.spent += 1.0;
        if self.spent > self.most {
            return Err(Unsupported::Coordinates);
        }
        Ok(())
    }

    /// Draws the steps `a..b` of `z` that have some length as written, each of
    /// which paints; a run goes on from the last step drawn where the corners
    /// between are one point as written.
    fn draw(&mut self, z: &Zigzag, a: f64, b: f64, out: &mut Out) -> Result<(), Unsupported> {
        let mut k = a;
        while k < b {
            let (from, to) = (z.written(k), z.written(k + 1.0));
            if from == to {
                // A run of corners at one point: on from its last.
                k = z.run_end(k, b);
                continue;
            }
            self.spend()?;
            let on = self.until.is_some_and(|u| u == k || z.still(u, k));
            if !on {
                out.computed(&[from.x, from.y], "m", z.place(k));
            }
            out.computed(&[to.x, to.y], "l", z.place(k + 1.0));
            self.until = Some(k + 1.0);
            k += 1.0;
        }
        Ok(())
    }
}

/// A zigzag ([`zigzag`]) and what the page shows of it.
struct Zigzag {
    bl: Point,
    along: Vec2,
    top_left: Point,
    top_along: Vec2,
    dt: f64,
    half: f64,
    /// What computing a corner in 64-bit numbers can move it by.
    error: f64,
    /// The larger of the magnitudes the two lines start from, across and up, and
    /// of the steps along them ([`Zigzag::place`]).
    lead: Vec2,
    step: Vec2,
    region: Region,
}

impl Zigzag {
    fn t(&self, k: f64) -> f64 {
        k * self.dt
    }

    fn bottom(&self, t: f64) -> Point {
        self.bl + self.along * t
    }

    fn top(&self, t: f64) -> Point {
        self.top_left + self.top_along * t
    }

    /// Corner `k`: on the bottom edge for an even `k`, on the line above for an odd.
    fn corner(&self, k: f64) -> Point {
        if k % 2.0 == 0.0 {
            self.bottom(self.t(k))
        } else {
            self.top(self.t(k))
        }
    }

    /// Corner `k` as written, as a viewer reads it. Along each line, each of its
    /// coordinates runs one way with `k`, as computed and as written.
    fn written(&self, k: f64) -> Point {
        let c = self.corner(k);
        Point::new(as_written(c.x), as_written(c.y))
    }

    /// Whether the corners `a` to `b` are one point as written: the first and last
    /// of each kind are, as each coordinate of each kind runs one way.
    fn still(&self, a: f64, b: f64) -> bool {
        let p = self.written(a);
        [a + 1.0, b - 1.0, b]
            .into_iter()
            .filter(|&k| a <= k && k <= b)
            .all(|k| self.written(k) == p)
    }

    /// The last corner from `k` to `limit` up to which the corners from `k` on
    /// are one point as written.
    fn run_end(&self, k: f64, limit: f64) -> f64 {
        if k >= limit || self.written(k + 1.0) != self.written(k) {
            return k;
        }
        let (mut lo, mut hi) = (k + 1.0, limit);
        while lo < hi {
            let mid = ((lo + hi + 1.0) / 2.0).floor();
            if self.still(k, mid) {
                lo = mid;
            } else {
                hi = mid - 1.0;
            }
        }
        lo
    }

    /// The first corner from which the corners up to `k` are one point as written.
    fn run_start(&self, k: f64) -> f64 {
        if k <= 0.0 || self.written(k - 1.0) != self.written(k) {
            return k;
        }
        let (mut lo, mut hi) = (0.0, k - 1.0);
        while lo < hi {
            let mid = ((lo + hi) / 2.0).floor();
            if self.still(mid, k) {
                hi = mid;
            } else {
                lo = mid + 1.0;
            }
        }
        lo
    }

    /// Whether the steps `a..b`, as written, come within reach of `region`: the
    /// corners of each kind lie in the box of the first and last of them, as each
    /// coordinate of each kind runs one way ([`Zigzag::written`]), and no stroke
    /// or join reaches farther from its corner than the miter limit times half the
    /// width. Free of the rounding `slack` bounds, it leaves out the steps near
    /// `region` as computed that are not as written.
    fn written_near(&self, a: f64, b: f64) -> bool {
        let mut points = Vec::with_capacity(8);
        for odd in [false, true] {
            let parity = |k: f64| (k % 2.0 == 1.0) == odd;
            let first = if parity(a) { a } else { a + 1.0 };
            let last = if parity(b) { b } else { b - 1.0 };
            if first > last {
                continue;
            }
            let (p, q) = (self.written(first), self.written(last));
            points.extend([p, Point::new(p.x, q.y), q, Point::new(q.x, p.y)]);
        }
        // What computing a stroke's or join's points from the corners moves them by.
        let error = points
            .iter()
            .fold(0.0_f64, |m, p| m.max(p.x.abs()).max(p.y.abs()))
            * f64::EPSILON
            * 16.0;
        self.region
            .may_show(
                &points,
                MITER_LIMIT * self.half * (1.0 + 1e-9) + error,
                self.place(b),
            )
            .is_some()
    }

    /// The corners of the steps from `t = ta` to `tb`, on the two lines, whose
    /// convex hull holds them.
    fn hull(&self, ta: f64, tb: f64) -> [Point; 4] {
        [self.bottom(ta), self.bottom(tb), self.top(tb), self.top(ta)]
    }

    /// The most writing, or computing in 64-bit numbers, moves a corner in the hull
    /// of `points` by, across or up: a 32-bit number is within 2^-24 of its size of
    /// the number it is written for. What a viewer computing it moves it by is
    /// [`Zigzag::place`]'s.
    fn slack(&self, points: &[Point]) -> f64 {
        let size = points
            .iter()
            .fold(0.0_f64, |m, p| m.max(p.x.abs()).max(p.y.abs()));
        (size + self.error) * f64::from(f32::EPSILON) / 2.0
            + self.error
            + f64::from(f32::MIN_POSITIVE)
    }

    /// How a viewer computes corner `k`, or any before it: as `bake` does
    /// (observed: a squiggle 10,000 steps from -100,000 drifts 9 pt), by adding
    /// the step to the corner before, from where its line starts, so that each
    /// coordinate is a running sum of `k` steps ([`Rounding::sum`]), each step and
    /// which line the corner is on a few roundings more.
    fn place(&self, k: f64) -> Rounding {
        let axis = |lead: f64, step: f64| {
            let total = k * step;
            Rounding::sum(lead, k, step, total, 2.0).plus(Rounding::of(4.0, lead + total))
        };
        Rounding::point(
            axis(self.lead.x, self.step.x),
            axis(self.lead.y, self.step.y),
        )
    }

    /// The most any stroke or join reaches from the steps' segments, as computed:
    /// half the miter limit times the width, and what writing moves a corner by
    /// (`slack` across or up).
    fn reach(&self, slack: f64) -> f64 {
        MITER_LIMIT * self.half + slack * SQRT_2
    }

    /// The steps into and out of a corner at `t`, on the bottom edge (`odd`
    /// false) or the line above, as functions of `t` that hold at every corner of
    /// that kind: each affine in `t`.
    fn ways(&self, odd: bool, t: f64) -> (Vec2, Vec2) {
        let dt = self.dt;
        if odd {
            (
                self.top(t) - self.bottom(t - dt),
                self.bottom(t + dt) - self.top(t),
            )
        } else {
            (
                self.bottom(t) - self.top(t - dt),
                self.top(t + dt) - self.bottom(t),
            )
        }
    }

    /// Whether step `k` of `steps`, as written, may paint in `region` wherever a
    /// viewer puts it ([`Zigzag::place`]): its rectangle,
    /// half the width to each side of it from corner to corner, or the join at
    /// either end it shares with the nearest step of some length on that side. A
    /// step of no length paints nothing of its own.
    fn paints(&self, k: f64, steps: f64) -> bool {
        let (region, half) = (&self.region, self.half);
        let (from, to) = (self.written(k), self.written(k + 1.0));
        if !(from.is_finite() && to.is_finite()) {
            return true;
        }
        let Some(d) = unit(to - from) else {
            return false;
        };
        let n = d.turn_90() * half;
        let after = self.run_end(k + 1.0, steps);
        // Wherever a viewer puts the corners up to the last this looks at.
        let place = self.place(after + 1.0);
        let meets = |points: &[Point]| region.may_show(points, 0.0, place).is_some();
        if meets(&[from + n, to + n, to - n, from - n]) {
            return true;
        }
        let join = |a: Point, c: Point, b: Point| {
            Join::of(c - a, b - c, half)
                .and_then(|join| join.region(c))
                .is_some_and(|r| meets(&r))
        };
        let before = self.run_start(k);
        (before > 0.0 && join(self.written(before - 1.0), from, to))
            || (after < steps && join(from, to, self.written(after + 1.0)))
    }

    /// Whether the steps `a..b` may paint in `region`, their corners within
    /// `slack` across and up of where they are computed: whether the convex hull
    /// of what the first and last corners of each kind and their strokes and joins
    /// can paint comes within the bound below of it. The steps into and out of the
    /// corners of a kind change along the range by at most `dv` each, being affine
    /// in `t`, and writing moves their ends by `slack` √2 at most, so that their
    /// directions change by at most `(2 dv + 4 √2 slack) / m`, `m` the shortest of
    /// them; the strokes' corners and the joins' tips move by at most as much times
    /// half the width and what the join's shape makes of it.
    fn may_paint(&self, a: f64, b: f64, slack: f64) -> bool {
        let place = self.place(b);
        let moved = place.bound();
        // Each corner may move by what writing and what a viewer computing it
        // moves it by, and the directions change with them.
        let moved = slack + moved.x.max(moved.y);
        let mut points = Vec::with_capacity(24);
        let mut bound: f64 = 0.0;
        for odd in [false, true] {
            let parity = |k: f64| (k % 2.0 == 1.0) == odd;
            let first = if parity(a) { a } else { a + 1.0 };
            let last = if parity(b) { b } else { b - 1.0 };
            if first > last {
                continue;
            }
            let (c0, c1) = (self.corner(first), self.corner(last));
            let (tf, tl) = (self.t(first), self.t(last));
            let ((i0, o0), (i1, o1)) = (self.ways(odd, tf), self.ways(odd, tl));
            let shortest = segment_from_origin(i0, i1).min(segment_from_origin(o0, o1));
            let dv = (i1 - i0).hypot().max((o1 - o0).hypot());
            let turned = ((2.0 * dv + 4.0 * SQRT_2 * moved) / shortest).min(2.0);
            let join = Join::of(i0, o0, self.half);
            let Some(join) = join.filter(|_| shortest > 0.0 && turned.is_finite()) else {
                // No direction to bound by: as far as a join can reach.
                points.extend([c0, c1]);
                bound = bound.max(MITER_LIMIT * self.half);
                continue;
            };
            // Whether the outer side, and whether the join is mitred, can change
            // along the range: then both sides, and the tip as far as the limit
            // lets it reach.
            let (either_side, miter) = join.along(turned);
            let mut reach = self.half * turned;
            match miter {
                Some(true) => reach += join.tip_moves(turned),
                Some(false) => {}
                None => reach = reach.max(MITER_LIMIT * self.half),
            }
            bound = bound.max(reach);
            for o in join.offsets(either_side, miter == Some(true)) {
                points.extend([c0 + o, c1 + o]);
            }
        }
        // The ends of the zigzag are butt caps, not joins: what is added for them
        // is more than they paint.
        self.region
            .may_show(&points, bound + slack * SQRT_2, place)
            .is_some()
    }
}

/// `v` of length 1; `None` for no length or none that is finite.
fn unit(v: Vec2) -> Option<Vec2> {
    let n = v.hypot();
    (n > 0.0 && n.is_finite()).then(|| v / n)
}

/// The distance from the origin to the segment from `a` to `b`.
fn segment_from_origin(a: Vec2, b: Vec2) -> f64 {
    let ab = b - a;
    let f = (-a).dot(ab) / ab.hypot2();
    let f = if f.is_finite() {
        f.clamp(0.0, 1.0)
    } else {
        0.0
    };
    (a + ab * f).hypot()
}

/// The join of two steps of a stroke `2 half` wide, along `d1` into a corner and
/// `d2` out of it, mitred up to [`MITER_LIMIT`] (PDF 32000-1 8.4.3.5): on the outer
/// side of the turn, the bevel between the two strokes' corners and, where the
/// miter is within the limit, the miter's tip.
struct Join {
    /// Of length `half`, to the left of each step.
    n1: Vec2,
    n2: Vec2,
    /// The sine of the turn, positive to the left.
    turn: f64,
    /// 1 + the cosine of the turn: the miter is within the limit where this is at
    /// least `2 / MITER_LIMIT²`, its tip `half (n1 + n2) / this` from the corner on
    /// the outer side.
    bend: f64,
    half: f64,
}

/// The least [`Join::bend`] of a mitred join.
const LEAST_MITER_BEND: f64 = 2.0 / (MITER_LIMIT * MITER_LIMIT);

impl Join {
    fn of(d1: Vec2, d2: Vec2, half: f64) -> Option<Self> {
        let (u1, u2) = (unit(d1)?, unit(d2)?);
        Some(Self {
            n1: u1.turn_90() * half,
            n2: u2.turn_90() * half,
            turn: u1.cross(u2),
            bend: 1.0 + u1.dot(u2),
            half,
        })
    }

    /// Of the outer side.
    fn outer(&self) -> f64 {
        if self.turn > 0.0 { -1.0 } else { 1.0 }
    }

    /// The tip of the miter from the corner, on the outer side.
    fn tip(&self) -> Vec2 {
        (self.n1 + self.n2) * (self.outer() / self.bend)
    }

    /// The join at corner `c`, a convex polygon: the corner, the two strokes'
    /// corners on the outer side, and the miter's tip between them where it is
    /// within the limit. `None` for a turn of none, straight on or back, where the
    /// strokes' ends cover the corner.
    fn region(&self, c: Point) -> Option<Vec<Point>> {
        if self.turn == 0.0 {
            return None;
        }
        let s = self.outer();
        let mut p = vec![c, c + self.n1 * s];
        if self.bend >= LEAST_MITER_BEND {
            p.push(c + self.tip());
        }
        p.push(c + self.n2 * s);
        Some(p)
    }

    /// For a range along which each step's direction changes by at most
    /// `turned`: whether the outer side can change, and whether the join is
    /// mitred all along (`Some(true)`), bevelled all along (`Some(false)`) or
    /// either.
    fn along(&self, turned: f64) -> (bool, Option<bool>) {
        // The sine and the cosine of the turn change by at most twice as much.
        let change = 2.0 * turned;
        let either_side = self.turn.abs() <= change;
        let miter = if self.bend - change >= LEAST_MITER_BEND {
            Some(true)
        } else if self.bend + change < LEAST_MITER_BEND {
            Some(false)
        } else {
            None
        };
        (either_side, miter)
    }

    /// The most the tip moves where each step's direction changes by at most
    /// `turned` and the join stays mitred: `half (n1 + n2) / bend`, its numerator
    /// moving by `2 half turned` at most, its denominator by `2 turned`.
    fn tip_moves(&self, turned: f64) -> f64 {
        let least = (self.bend - 2.0 * turned).max(LEAST_MITER_BEND);
        self.half * (2.0 * turned / least + 2.0 * 2.0 * turned / (least * least))
    }

    /// What the strokes and the join can paint around a corner, from it: the
    /// strokes' corners on both sides, and the miter's tip, on the outer side or
    /// on either.
    fn offsets(&self, either_side: bool, miter: bool) -> Vec<Vec2> {
        let mut o = vec![self.n1, -self.n1, self.n2, -self.n2];
        if miter {
            let tip = self.tip();
            o.push(tip);
            if either_side {
                o.push(-tip);
            }
        }
        o
    }
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

/// The box of the `/InkList` paths that [`ink`] strokes, and how far beyond it the
/// stroke paints; `None` without a path, which draws nothing.
pub(super) fn ink_extent(annot: &Entries<'_>) -> Option<(Rect, f64)> {
    let paths = annot.items(INKLIST)?.derived::<Paths>();
    Some((paths.1?, stroke_reach(border_width(annot))))
}

/// The paths of an `/InkList` that draw, the numbers of each, at least two, and
/// the box of their points: made once for the document ([`Derived`]), so that
/// annotations sharing a list walk what it holds that draws nothing once.
struct Paths(Vec<Vec<f64>>, Option<Rect>);

impl Derived for Paths {
    fn derive(items: &Items<'_>) -> Self {
        let paths = items.iter().filter_map(Value::items).map(|p| p.numbers());
        let paths: Vec<Vec<f64>> = paths.filter(|p| p.len() >= 2).collect();
        let bounds = paths
            .iter()
            .flat_map(|p| p.as_chunks::<2>().0)
            .map(|[x, y]| Point::new(*x, *y))
            .fold(None, |b: Option<Rect>, p| {
                Some(b.map_or(Rect::from_points(p, p), |b| b.union_pt(p)))
            });
        Self(paths, bounds)
    }
}

#[cfg(test)]
mod tests {
    use super::super::PAINTS_BEYOND;
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
        let mut out = Out::new(visible, 1.0);
        squiggly(&Entries::detached(dict), &mut out)
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
        let mut out = Out::new(PAGE, 1.0);
        squiggly(&Entries::detached(dict), &mut out).expect("drawable");
        assert!(matches!(out.finish(), Err(super::Unsupported::Coordinates)));
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

    /// Whether step `k` of the polyline through `corners`, stroked `width` wide with
    /// butt caps and mitred joins up to 10 by kurbo's stroker, paints in `r`: the
    /// step with a stub of 10^-9 of each neighbour, which carries the join it shares
    /// with it.
    fn step_paints(corners: &[(f64, f64)], k: usize, width: f64, r: Rect) -> bool {
        use kurbo::{BezPath, Cap, Join, PathEl, Point, Shape, Stroke, StrokeOpts};
        let p = |i: usize| Point::new(corners[i].0, corners[i].1);
        let stub = |from: Point, to: Point| from + (to - from).normalize() * 1e-9;
        let mut path = BezPath::new();
        path.move_to(if k > 0 { stub(p(k), p(k - 1)) } else { p(k) });
        path.line_to(p(k));
        path.line_to(p(k + 1));
        if k + 2 < corners.len() {
            path.line_to(stub(p(k + 1), p(k + 2)));
        }
        let style = Stroke::new(width)
            .with_join(Join::Miter)
            .with_miter_limit(10.0)
            .with_caps(Cap::Butt);
        let outline = kurbo::stroke(path, &style, &StrokeOpts::default(), 1e-9);
        let corners_of_r = [(r.x0, r.y0), (r.x1, r.y0), (r.x1, r.y1), (r.x0, r.y1)];
        if corners_of_r
            .iter()
            .any(|&(x, y)| outline.winding(Point::new(x, y)) != 0)
        {
            return true;
        }
        let mut last = Point::ZERO;
        let mut start = Point::ZERO;
        let mut meets = false;
        kurbo::flatten(&outline, 1e-9, |el| {
            let (a, b) = match el {
                PathEl::MoveTo(q) => {
                    (last, start) = (q, q);
                    return;
                }
                PathEl::LineTo(q) => (last, q),
                PathEl::ClosePath => (last, start),
                _ => unreachable!("flattened"),
            };
            meets |= distance((a.x, a.y), (b.x, b.y), r) == 0.0;
            last = b;
        });
        meets
    }

    /// A quad whose line a seventh of the way up leans far back over the page, while
    /// its bottom edge lies beside it: of its thousand steps, those whose stroke or
    /// joins paint on the page are drawn, and no other (the 26 steps from 88 on;
    /// within half the miter limit times the width of it, before, the 30 from 86;
    /// before that, every step).
    #[test]
    fn a_steep_zigzag_draws_the_steps_that_reach_the_page() {
        let page = Rect::new(0.0, 0.0, 200.0, 200.0);
        let c = squiggly_content("300 7 -13700 14000 300 0 1300 0", page);
        // Height 7: steps of 1 along the bottom edge from x = 300, a line 7/16 wide;
        // the line a seventh of the way up runs from (300, 1) to (1300 - 15000 / 7,
        // 2000). The corners as written, in 32-bit numbers.
        let (top_left, top_right) = ((300.0, 1.0), (1300.0 - 15000.0 / 7.0, 2000.0));
        let corner = |k: usize| {
            let t = k as f64 / 1000.0;
            let (x, y) = if k.is_multiple_of(2) {
                (300.0 + 1000.0 * t, 0.0)
            } else {
                (
                    top_left.0 + (top_right.0 - top_left.0) * t,
                    top_left.1 + (top_right.1 - top_left.1) * t,
                )
            };
            (f64::from(x as f32), f64::from(y as f32))
        };
        let corners: Vec<(f64, f64)> = (0..=1000).map(corner).collect();
        let near: Vec<usize> = (0..1000)
            .filter(|&k| step_paints(&corners, k, 7.0 / 16.0, page))
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

    /// The steps drawn in `c`, by the corner nearest where each `l` ends.
    fn steps_drawn(c: &str, corners: &[(f64, f64)]) -> Vec<usize> {
        corners_drawn(c)
            .into_iter()
            .map(|(x, y)| {
                let d = |k: usize| (corners[k].0 - x).hypot(corners[k].1 - y);
                (0..corners.len())
                    .min_by(|&a, &b| d(a).total_cmp(&d(b)))
                    .unwrap()
                    - 1
            })
            .collect()
    }

    /// Where each `l` of `c` ends.
    fn corners_drawn(c: &str) -> Vec<(f64, f64)> {
        c.lines()
            .filter(|l| l.ends_with(" l"))
            .map(|l| {
                let n: Vec<f64> = l.split(' ').take(2).map(|v| v.parse().unwrap()).collect();
                (n[0], n[1])
            })
            .collect()
    }

    /// Zigzags along each side of the page, just off it, upside down so that the
    /// joins' tips of the corners nearest the page reach towards it, by less or more
    /// than the gap: rectangles, slanted ones, and quads whose sides are not
    /// parallel, whose steps change shape along them. The steps drawn are exactly
    /// those whose stroke or joins, as kurbo's stroker outlines them, come within
    /// [`PAINTS_BEYOND`] of the page inside the clip.
    #[test]
    fn zigzags_along_the_page_draw_the_steps_that_paint_on_it() {
        let page = Rect::new(0.0, 0.0, 200.0, 200.0);
        // A small linear congruential generator: the same quads each run.
        let mut seed = 0x2545_f491_u64;
        let mut next = |lo: f64, hi: f64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            lo + (hi - lo) * ((seed >> 11) as f64 / (1_u64 << 53) as f64)
        };
        let mut tried = 0;
        let mut painted = 0;
        for i in 0..48 {
            let h = next(2.0, 9.0);
            let width = h / 16.0;
            // From the page's edge to the bottom edge, outwards: from a little
            // inside to past the miter tips of the steepest joins.
            let gap = next(-0.5 * width, 3.0 * width);
            let slant = next(-1.5 * h, 1.5 * h);
            let spread = next(-0.3, 0.3) * h * f64::from(i % 3 != 0);
            let (x0, x1) = (next(-40.0, 20.0), next(120.0, 240.0));
            // Along the bottom side of the page, then turned onto each other side.
            let q = [
                (x0 + slant, -gap - h),
                (x1 + slant, -gap - h - spread),
                (x0, -gap),
                (x1, -gap),
            ];
            let turn = |(x, y): (f64, f64)| match i % 4 {
                0 => (x, y),
                1 => (200.0 - y, x),
                2 => (200.0 - x, 200.0 - y),
                _ => (y, 200.0 - x),
            };
            let q = q.map(turn);
            let quads = q
                .iter()
                .map(|(x, y)| format!("{x} {y}"))
                .collect::<Vec<_>>()
                .join(" ");
            let c = squiggly_content(&quads, page);
            // The zigzag as `zigzag` draws it: corners a seventh of the height
            // apart along the bottom edge, alternately on it and on the line a
            // seventh of the way up.
            let pt = |(x, y): (f64, f64)| kurbo::Point::new(x, y);
            let [tl, tr, bl, br] = q.map(pt);
            let height = (tl - bl).hypot();
            let delta = height / 7.0;
            let len = (br - bl).hypot();
            let steps = (len / delta).ceil() as usize;
            let (top_left, top_right) = (bl.lerp(tl, 1.0 / 7.0), br.lerp(tr, 1.0 / 7.0));
            let corners: Vec<(f64, f64)> = (0..=steps)
                .map(|k| {
                    let t = k as f64 * delta / len;
                    let p = if k % 2 == 0 {
                        bl.lerp(br, t)
                    } else {
                        top_left.lerp(top_right, t)
                    };
                    (f64::from(p.x as f32), f64::from(p.y as f32))
                })
                .collect();
            let width = height / 16.0;
            // Clipped to the box of the quad, a width wider on each side, as written.
            let clip = Rect::from_points(tl, tr)
                .union_pt(bl)
                .union_pt(br)
                .inset(width);
            let [x, y, w, h] =
                [clip.x0, clip.y0, clip.width(), clip.height()].map(|v| f64::from(v as f32));
            // A clip that leaves nothing of the page shows nothing, however close.
            let clip = Rect::new(x, y, x + w, y + h);
            let shown = page.intersect(clip).inset(PAINTS_BEYOND);
            let width = f64::from(width as f32);
            let expected: Vec<usize> = (0..steps)
                .filter(|&k| page.overlaps(clip) && step_paints(&corners, k, width, shown))
                .collect();
            assert_eq!(steps_drawn(&c, &corners), expected, "{quads}");
            tried += 1;
            painted += usize::from(!expected.is_empty());
        }
        // Some paint and some do not.
        assert!(painted > 8 && painted < tried - 8, "{painted} of {tried}");
    }

    /// A range of steps is left out only when none of its steps paints: for quads
    /// of every shape (their sides far from parallel, so that the steps' joins
    /// change from bevels to long miters and back along them), ranges of every
    /// length, and small parts of the page around the zigzag.
    #[test]
    fn a_range_left_out_holds_no_step_that_paints() {
        use super::{Quad, Zigzag};
        use kurbo::Point;
        let mut seed = 0x9e37_79b9_u64;
        let mut next = |lo: f64, hi: f64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            lo + (hi - lo) * ((seed >> 11) as f64 / (1_u64 << 53) as f64)
        };
        let (mut left_out, mut specks) = (0, 0);
        for _ in 0..300 {
            let mut p = || Point::new(next(0.0, 100.0), next(0.0, 100.0));
            let (bl, br) = (p(), p());
            let tl = bl + kurbo::Vec2::new(next(-30.0, 30.0), next(-30.0, 30.0));
            let tr = br + kurbo::Vec2::new(next(-60.0, 60.0), next(-60.0, 60.0));
            let q = Quad([tl, tr, bl, br]);
            let delta = q.height() / 7.0;
            let len = (br - bl).hypot();
            let steps = (len / delta).ceil();
            if !(steps > 4.0 && steps < 400.0) {
                continue;
            }
            let (top_left, top_right) = q.across(1.0 / 7.0);
            let c = Point::new(next(0.0, 100.0), next(0.0, 100.0));
            let (w, h) = (next(0.0, 3.0), next(0.0, 3.0));
            let region = |r: Rect| Out::new(r, 0.0).region();
            let mut z = Zigzag {
                bl,
                along: br - bl,
                top_left,
                top_along: top_right - top_left,
                dt: delta / len,
                half: f64::from((q.height() / 16.0) as f32) / 2.0,
                error: 1e-12,
                lead: kurbo::Vec2::ZERO,
                step: kurbo::Vec2::ZERO,
                region: region(Rect::new(c.x, c.y, c.x + w, c.y + h)),
            };
            let may_paint = |z: &Zigzag, a: f64, b: f64| {
                let slack = z.slack(&z.hull(z.t(a), z.t(b)));
                z.may_paint(a, b, slack)
            };
            for _ in 0..20 {
                let a = next(0.0, steps - 1.0).floor();
                let b = (a + 1.0 + next(0.0, steps - a).floor()).min(steps);
                if b - a < 2.0 || may_paint(&z, a, b) {
                    continue;
                }
                left_out += 1;
                for k in a as usize..b as usize {
                    assert!(
                        !z.paints(k as f64, steps),
                        "step {k} of {a}..{b} paints: {:?} in {:?}",
                        q.0,
                        z.region
                    );
                }
            }
            // A speck of the page at the outermost points of a step's joins and
            // stroke, which that step paints: no range holding it is left out.
            for _ in 0..20 {
                let a = next(0.0, steps - 1.0).floor();
                let b = (a + 1.0 + next(0.0, steps - a).floor()).min(steps);
                let k = (a + next(0.0, b - a).floor()).min(b - 1.0);
                let from = z.written(k);
                let joins = [
                    (k > 0.0).then(|| (z.written(k - 1.0), from, z.written(k + 1.0))),
                    (k + 1.0 < steps).then(|| (from, z.written(k + 1.0), z.written(k + 2.0))),
                ];
                let to = z.written(k + 1.0);
                let n = (to - from).normalize().turn_90() * z.half;
                let stroke = [from + n, from - n, to + n, to - n];
                let regions = joins.into_iter().flatten().filter_map(|(p0, c, p1)| {
                    super::Join::of(c - p0, p1 - c, z.half).and_then(|j| j.region(c))
                });
                for points in regions.chain([stroke.to_vec()]) {
                    for point in points {
                        let e = 1e-9 * (1.0 + point.x.abs().max(point.y.abs()));
                        z.region = region(Rect::new(
                            point.x - e,
                            point.y - e,
                            point.x + e,
                            point.y + e,
                        ));
                        if z.paints(k, steps) {
                            assert!(
                                may_paint(&z, a, b),
                                "step {k} of {a}..{b} paints at {point:?}: {:?}",
                                q.0
                            );
                            specks += 1;
                        }
                    }
                }
            }
        }
        assert!(left_out > 100, "{left_out}");
        assert!(specks > 1000, "{specks}");
    }

    /// Of a zigzag below the page, whose bottom corners are nearer the page than
    /// half the miter limit times the width but whose stroke and joins stay below
    /// it, no step is drawn (2 000 001 steps; before, more than 262 144 were
    /// counted and the page refused).
    #[test]
    fn a_zigzag_whose_joins_stay_off_the_page_draws_nothing() {
        let page = Rect::new(0.0, 0.0, 200.0, 200.0);
        let c = squiggly_content("0 -.0008 200 -.0008 0 -.0001 200 -.0001", page);
        assert!(corners(&c).is_empty(), "{c}");
    }

    /// A quad 4·10^6 pt wide starting at its left end draws the steps across the
    /// page, with the corners its own start puts them at; one starting 4·10^6 pt
    /// left of the page is refused.
    #[test]
    fn a_huge_quad_draws_the_steps_the_page_shows() {
        let page = Rect::new(100.0, 0.0, 712.0, 800.0);
        // From 4·10^6 off the page, a viewer computing a corner from the quad's
        // start may put it more than a point off: refused.
        for x0 in ["-4000000", "-3999999"] {
            let annot = format!(
                "<< /Subtype /Squiggly /Rect [0 0 1 1] /QuadPoints \
                 [{x0} 24 4000000 24 {x0} 10 4000000 10] >>"
            );
            let dict = Dict::from_bytes(annot.as_bytes()).expect("valid dict");
            let mut out = Out::new(page, 1.0);
            let drew = squiggly(&Entries::detached(dict), &mut out);
            assert!(drew.is_err() || out.finish().is_err(), "{x0}: drawn");
        }
        for x0 in ["0"] {
            let c = squiggly_content(&format!("{x0} 24 4000000 24 {x0} 10 4000000 10"), page);
            let corners = corners(&c);
            let start: f64 = x0.parse().unwrap();
            let (first, last) = (corners[0].0, corners[corners.len() - 1].0);
            // The steps across each side of the page, or as close to it as a stroke
            // paints beyond its outline, are drawn, and none further.
            assert!((96.0..100.0).contains(&first), "{x0}: {first}");
            assert!((712.0..716.0).contains(&last), "{x0}: {last}");
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
