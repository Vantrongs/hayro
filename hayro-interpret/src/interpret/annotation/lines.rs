//! `Line`, `Polygon` and `PolyLine` (PDF 32000-1 12.5.6.7, 12.5.6.9), with the line
//! ending styles of Table 176, leader lines and captions. Shapes follow the spec's
//! descriptions; their sizes, three times the border width for squares, circles
//! and diamonds and an arrow 8.8 widths long and 9 wide, are chosen so that the
//! endings look as common viewers draw them.

use super::entries::{Derived, Entries, Items, Text};
use super::out::{Color, Out, Part, Refused, Rounding, corners, stroke_reach};
use super::text::{self, Align, Family, Setting};
use super::{Appearances, CAPTION, Drew, Unsupported, border_width, circle, color_of, dash};
use hayro_syntax::object::dict::keys::{C, IC};
use kurbo::{Affine, BezPath, Point, Vec2};

/// A line ending style.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Ending {
    None,
    Square,
    Circle,
    Diamond,
    OpenArrow,
    ClosedArrow,
    Butt,
    ROpenArrow,
    RClosedArrow,
    Slash,
}

impl Ending {
    fn from_name(name: &[u8]) -> Self {
        match name {
            b"Square" => Self::Square,
            b"Circle" => Self::Circle,
            b"Diamond" => Self::Diamond,
            b"OpenArrow" => Self::OpenArrow,
            b"ClosedArrow" => Self::ClosedArrow,
            b"Butt" => Self::Butt,
            b"ROpenArrow" => Self::ROpenArrow,
            b"RClosedArrow" => Self::RClosedArrow,
            b"Slash" => Self::Slash,
            _ => Self::None,
        }
    }

    /// The endings of `/LE` of a line or polyline, as viewers read it: an array of
    /// exactly two entries, each a name or none. Of a longer one, no entry after the
    /// third is read.
    pub(super) fn of(annot: &Entries<'_>) -> [Self; 2] {
        let names = annot.items(b"LE").map(|i| i.head(3)).unwrap_or_default();
        match names.as_slice() {
            [first, last] => [first, last].map(|n| n.name().map_or(Self::None, Self::from_name)),
            _ => [Self::None, Self::None],
        }
    }

    /// Whether this ending at `tip`, arriving from `from`, is laid along or across
    /// a line of no length away from the page's origin, from which viewers draw
    /// it (see [`ends_drawable`]); at the origin, it is drawn in one point.
    pub(super) fn lost_at(self, tip: Point, from: Point) -> bool {
        let along = !matches!(
            self,
            Self::None | Self::Square | Self::Circle | Self::Diamond
        );
        along && tip == from && tip != Point::ORIGIN
    }

    /// The ending of `/LE` of a free text callout, as viewers read it: a name, not
    /// an array.
    pub(super) fn of_callout(annot: &Entries<'_>) -> Self {
        annot.get(b"LE").name().map_or(Self::None, Self::from_name)
    }
}

/// How a shape is painted: its stroke colour, width and dash, and interior fill;
/// without a stroke colour, its outlines are not drawn.
pub(super) struct Paint {
    pub(super) stroke: Option<Color>,
    pub(super) width: f64,
    pub(super) dash: Vec<f64>,
    pub(super) fill: Option<Color>,
}

impl Paint {
    /// /C strokes at the border width with the border dash, /IC fills. The dash
    /// patterns only a stroke: without /C, it is not read.
    fn of(annot: &Entries<'_>) -> Self {
        let stroke = color_of(annot, C);
        let dash = if stroke.is_some() {
            dash(annot)
        } else {
            Vec::new()
        };
        Self {
            stroke,
            width: border_width(annot),
            dash,
            fill: color_of(annot, IC),
        }
    }

    fn start(&self, out: &mut Out) -> Option<()> {
        if let Some(stroke) = &self.stroke {
            out.stroke_color(stroke)?;
        }
        if let Some(fill) = &self.fill {
            out.fill_color(fill)?;
        }
        out.stroke_state(self.width, &self.dash)
    }

    /// The operator that strokes an open path, or ends it unpainted.
    fn stroke_op(&self) -> &'static str {
        if self.stroke.is_some() { "S" } else { "n" }
    }

    /// The operator that closes, strokes and fills a closed path, as far as there
    /// are colours to.
    fn close_op(&self) -> &'static str {
        match (&self.stroke, &self.fill) {
            (Some(_), Some(_)) => "b",
            (Some(_), None) => "s",
            (None, Some(_)) => "h f",
            (None, None) => "n",
        }
    }
}

/// The ending `ending` at `tip`, of a line that arrives there from `from`.
pub(super) fn ending(
    ending: Ending,
    tip: Point,
    from: Point,
    paint: &Paint,
    out: &mut Out,
) -> Option<()> {
    let Some((path, closed)) = ending.path(tip, from, paint.width) else {
        return Some(());
    };
    if ending == Ending::Square {
        // A rectangle, as viewers write it: renderers stroke one of a negative
        // width otherwise than the same path.
        let r = path.control_box();
        out.nums(&[r.x0, r.y0, r.width(), r.height()], "re")?;
    } else {
        out.path(&path)?;
    }
    out.op(if closed {
        paint.close_op()
    } else {
        paint.stroke_op()
    });
    Some(())
}

impl Ending {
    /// The path of the ending at `tip` of a line that arrives there from `from`,
    /// drawn `width` wide, and whether it is closed; `None` for no ending.
    fn path(self, tip: Point, from: Point, width: f64) -> Option<(BezPath, bool)> {
        // Endings are sized for a border at least 1 wide.
        let w = width.max(1.0);
        // A line of no length has no direction (see [`ends_drawable`]).
        let dir = direction(tip - from);
        let normal = Vec2::new(-dir.y, dir.x);
        let poly = |pts: &[Point], closed: bool| {
            let mut p = BezPath::new();
            p.move_to(pts[0]);
            for q in &pts[1..] {
                p.line_to(*q);
            }
            if closed {
                p.close_path();
            }
            Some((p, closed))
        };
        let r = 3.0 * w;
        match self {
            Self::None => None,
            Self::Circle => Some((circle(tip, r), true)),
            Self::Square => poly(
                &[(-r, -r), (r, -r), (r, r), (-r, r)].map(|(x, y)| tip + Vec2::new(x, y)),
                true,
            ),
            Self::Diamond => poly(
                &[(0.0, r), (r, 0.0), (0.0, -r), (-r, 0.0)].map(|(x, y)| tip + Vec2::new(x, y)),
                true,
            ),
            Self::OpenArrow | Self::ClosedArrow | Self::ROpenArrow | Self::RClosedArrow => {
                // The arms run back along the line, or, reversed, on beyond its end.
                let back = if matches!(self, Self::OpenArrow | Self::ClosedArrow) {
                    -dir
                } else {
                    dir
                };
                let base = tip + back * (8.8 * w);
                // Drawn from the arm on the left of the arms' direction, which a dash
                // pattern starts on, as viewers draw it.
                let side = Vec2::new(-back.y, back.x) * (4.5 * w);
                let closed = matches!(self, Self::ClosedArrow | Self::RClosedArrow);
                poly(&[base + side, tip, base - side], closed)
            }
            Self::Butt => poly(&[tip - normal * r, tip + normal * r], false),
            Self::Slash => {
                // Across the line at 60 degrees.
                let (sin, cos) = std::f64::consts::FRAC_PI_3.sin_cos();
                let slant = Vec2::new(dir.x * cos - dir.y * sin, dir.x * sin + dir.y * cos);
                poly(&[tip - slant * (5.0 * w), tip + slant * (5.0 * w)], false)
            }
        }
    }

    /// The box viewers clip an annotation to that ends in this ending at `tip`,
    /// arriving from `from`, drawn `width` wide, and holds `around` (what the
    /// annotation draws before this ending): with an arrow, butt or slash, `around`
    /// and the ending's shape out by `grow`, all of it then out by the width as
    /// given; with a square, circle or diamond, `around` and the shape out by `grow`
    /// and its stroke (half the width, along a diagonal at the diamond's corners);
    /// without an ending, `around`. Of a free text callout and a polyline, `grow` is
    /// 0; of a line, the width, at least 1 ([`point_clip`]).
    pub(super) fn clip_box(
        self,
        tip: Point,
        from: Point,
        width: f64,
        grow: f64,
        around: kurbo::Rect,
    ) -> kurbo::Rect {
        let Some((path, _)) = self.path(tip, from, width) else {
            return around;
        };
        let shape = path.control_box().inflate(grow, grow);
        match self {
            Self::Square | Self::Circle => around.union(shape.inflate(width / 2.0, width / 2.0)),
            Self::Diamond => {
                let d = width / std::f64::consts::SQRT_2;
                around.union(shape.inflate(d, d))
            }
            _ => around.union(shape).inflate(width, width),
        }
    }

    /// The box of this ending's shape at `tip`, arriving from `from`, drawn
    /// `width` wide ([`Ending::path`]: sized as for a width of 1 where the width is
    /// less); `None` for no ending. Viewers drawing it at a negative width paint
    /// within it (observed in `PyMuPDF` 1.28.2 at widths -0.5 to -10, of lines in
    /// three directions and of polylines: a square, circle or diamond within 3 of
    /// the tip, less half the width; an arrow within its arms, less the width).
    fn ink(self, tip: Point, from: Point, width: f64) -> Option<kurbo::Rect> {
        let (path, _) = self.path(tip, from, width)?;
        Some(path.control_box())
    }
}

/// The unit vector along `v`; none, of no length, when `v` has no length.
fn direction(v: Vec2) -> Vec2 {
    let len = v.hypot();
    if len > 0.0 { v / len } else { Vec2::ZERO }
}

/// Numbers as points, each pair one; a last number alone is none.
fn points(n: &[f64]) -> Vec<Point> {
    n.as_chunks::<2>()
        .0
        .iter()
        .map(|[x, y]| Point::new(*x, *y))
        .collect()
}

/// The points of `/Vertices` and their bounds, made once for the document
/// ([`Derived`]).
struct Vertices(Vec<Point>, Option<kurbo::Rect>);

impl Derived for Vertices {
    fn derive(items: &Items<'_>) -> Self {
        let vertices = points(&items.numbers());
        let bounds = vertices.first().map(|first| {
            vertices
                .iter()
                .fold(kurbo::Rect::from_points(*first, *first), |b, p| {
                    b.union_pt(*p)
                })
        });
        Self(vertices, bounds)
    }
}

/// `Polygon` and `PolyLine`: `/Vertices` joined, a polygon closed and filled with /IC,
/// a polyline with its `/LE` endings at the first and last vertex, clipped to
/// [`poly_clip`].
pub(super) fn poly(annot: &Entries<'_>, closed: bool, out: &mut Out) -> Drew {
    let derived = annot.items(b"Vertices").map(|i| i.derived::<Vertices>());
    let (vertices, bounds) = derived
        .as_ref()
        .map_or((&[][..], None), |v| (&v.0[..], v.1));
    let Some(bounds) = bounds.filter(|_| vertices.len() >= 2) else {
        return Ok(None);
    };
    let ends = poly_ends(annot, vertices, closed);
    let w = border_width(annot);
    let clip = poly_clip(bounds, w, &ends);
    // Laid out from the values alone; an ending left out ([`ends_drawable`]) is not
    // drawn, and changes nothing else.
    let Some(drawn) = ends_drawable(
        annot,
        ends,
        || corners(bounds).to_vec(),
        (w >= 0.0).then_some(clip),
        out,
    )?
    else {
        return Ok(None);
    };
    Ok(draw_poly(annot, vertices, clip, closed, ends, drawn, out))
}

/// The endings of a polyline with vertices `vertices` (two or more), each at its
/// tip arriving from a vertex; none of a polygon.
fn poly_ends(annot: &Entries<'_>, vertices: &[Point], closed: bool) -> [(Ending, Point, Point); 2] {
    let [first, last] = if closed {
        [Ending::None; 2]
    } else {
        Ending::of(annot)
    };
    let n = vertices.len();
    [
        (first, vertices[0], vertices[1]),
        (last, vertices[n - 1], vertices[n - 2]),
    ]
}

/// The box viewers clip a polygon or polyline `w` wide to, of the box of its
/// vertices `bounds`, with endings `ends`: that box out by the width as given, then
/// each ending's [`Ending::clip_box`], the first first. This cuts off the tip of a
/// sharp mitred corner, or, for a negative width, the shape's edges (observed in
/// `PyMuPDF` 1.28.2: bake's `/BBox` to the bit for 600 polylines of 2 to 4 random
/// vertices, a third with a first segment of no length, at widths 0 to 7 with each
/// pair of endings).
fn poly_clip(bounds: kurbo::Rect, w: f64, ends: &[(Ending, Point, Point); 2]) -> kurbo::Rect {
    ends.iter()
        .fold(bounds.inflate(w, w), |b, (ending, tip, from)| {
            ending.clip_box(*tip, *from, w, 0.0, b)
        })
}

/// The box viewers clip a `Line` of no length at `tip`, `w` wide (not negative),
/// with endings `ends`, to: the point out by the width, at least 1, and by 12 more
/// with `/Cap` (`cap`), whatever the caption; then each ending's
/// [`Ending::clip_box`], its shape out by the width, at least 1 (observed in
/// `PyMuPDF` 1.28.2: bake's `/BBox` to the bit for 500 lines of no length at widths
/// 0 to 7, with each pair of endings, with and without `/Cap`, `/CO` and `/CP`).
fn point_clip(tip: Point, w: f64, cap: bool, ends: &[(Ending, Point, Point); 2]) -> kurbo::Rect {
    let grow = w.max(1.0);
    let base = grow + if cap { f64::from(CAPTION_SIZE) } else { 0.0 };
    let around = kurbo::Rect::from_points(tip, tip).inflate(base, base);
    ends.iter().fold(around, |b, (ending, tip, from)| {
        ending.clip_box(*tip, *from, w, grow, b)
    })
}

/// Which endings of a line or polyline, each at its tip arriving from a point, are
/// drawn as given ([`Unsupported::Ending`] refused where it may show, [`Out::refuse`],
/// for the others), when its content is clipped to `clip` where that is known:
///
/// - an ending laid along or across a line of no length, whose points viewers put at
///   the page's origin, is never drawn: what may differ is where [`lost_part`] says
///   under `clip`, or, without one, along all its line from the origin; the rest is
///   drawn as given;
/// - at a negative border width with an ending, where the box viewers clip to
///   shrinks and a closed ending widens it, showing more of the line anywhere along
///   it: viewers draw all of it within the box of the points `body` gives and the
///   endings' shapes ([`Ending::ink`]), and nothing beyond (observed in `PyMuPDF`
///   1.28.2: at widths -0.5 to -10, a polyline's ink lies 10 widths inside the box of
///   its vertices without a closed ending, and beyond it only as far as such an
///   ending's shape). Refused where that box may show; nothing is drawn where it
///   cannot: `None`.
///
/// Whether an ending is left out changes nothing else drawn: the clip and the rest
/// are laid out from the annotation's values alone.
pub(super) fn ends_drawable(
    annot: &Entries<'_>,
    ends: [(Ending, Point, Point); 2],
    body: impl FnOnce() -> Vec<Point>,
    clip: Option<kurbo::Rect>,
    out: &mut Out,
) -> Result<Option<[bool; 2]>, Refused> {
    let painted = color_of(annot, C).is_some() || color_of(annot, IC).is_some();
    let w = border_width(annot);
    let mut drawn = [false; 2];
    for (draws, (ending, tip, from)) in drawn.iter_mut().zip(&ends) {
        let lost = ending.lost_at(*tip, *from);
        *draws = *ending != Ending::None && !lost;
        if lost && painted {
            out.refuse(Unsupported::Ending, lost_part(*tip, w, clip))?;
        }
    }
    let any = ends.iter().filter(|(e, _, _)| *e != Ending::None);
    if painted && w < 0.0 && any.clone().next().is_some() {
        let body = body();
        let all = any
            .filter_map(|(ending, tip, from)| ending.ink(*tip, *from, w))
            .chain(body.iter().map(|p| kurbo::Rect::from_points(*p, *p)))
            .reduce(|a, b| a.union(b));
        if let Some(all) = all {
            out.refuse(Unsupported::Ending, Part::rect(all))?;
        }
        return Ok(None);
    }
    Ok(Some(drawn))
}

/// Where an ending of no direction at `tip`, of a line `w` wide, may change what
/// viewers draw, who clip the annotation to `within` where that is known: they draw
/// it from the page's origin to its tip and back (observed: an open arrow at 50 60
/// is `0 0 m 50 60 l 0 0 l`), with butt ends, within half its width, at least a
/// half, of that line (observed in `PyMuPDF` 1.28.2 at widths 0.25 to 10: past the
/// tip by 0.375 widths along each axis of a line at 45 degrees).
pub(super) fn lost_part(tip: Point, w: f64, within: Option<kurbo::Rect>) -> Part {
    let half = w.abs().max(1.0) / 2.0;
    let across = direction(tip.to_vec2());
    let across = Vec2::new(-across.y, across.x) * half;
    let band = [
        Point::ORIGIN + across,
        tip + across,
        tip - across,
        Point::ORIGIN - across,
    ];
    let part = Part::near(band.to_vec(), 0.0);
    match within {
        Some(within) => part.under(within),
        None => part,
    }
}

fn draw_poly(
    annot: &Entries<'_>,
    vertices: &[Point],
    clip: kurbo::Rect,
    closed: bool,
    ends: [(Ending, Point, Point); 2],
    drawn: [bool; 2],
    out: &mut Out,
) -> Option<()> {
    let stroke = color_of(annot, C);
    let fill = color_of(annot, IC);
    let any = ends.iter().any(|(e, _, _)| *e != Ending::None);
    // A polygon draws with either colour; of a polyline, only a stroke draws the
    // line, and a fill only its endings: without them, nothing is drawn.
    if stroke.is_none() && (if closed { fill.is_none() } else { !any }) {
        return None;
    }
    out.clip(clip)?;
    if closed {
        if let Some(stroke) = &stroke {
            out.stroke_color(stroke)?;
        }
        if let Some(fill) = &fill {
            out.fill_color(fill)?;
        }
        out.stroke_state(border_width(annot), &dash(annot))?;
        polyline(vertices, out)?;
        out.op(match (&stroke, &fill) {
            (Some(_), Some(_)) => "b",
            (Some(_), None) => "s",
            _ => "h f",
        });
        return Some(());
    }
    let paint = Paint::of(annot);
    paint.start(out)?;
    if paint.stroke.is_some() {
        polyline(vertices, out)?;
        out.op("S");
    }
    draw_ends(ends, drawn, &paint, out)
}

/// The endings `ends` of which `drawn` says they are drawn.
fn draw_ends(
    ends: [(Ending, Point, Point); 2],
    drawn: [bool; 2],
    paint: &Paint,
    out: &mut Out,
) -> Option<()> {
    for ((e, tip, from), draws) in ends.into_iter().zip(drawn) {
        if draws {
            ending(e, tip, from, paint, out)?;
        }
    }
    Some(())
}

/// Where a `Polygon` or `PolyLine` ([`poly`]) may paint: within the reach returned
/// of the box returned; `None` for fewer than two vertices, which draw nothing. All
/// it draws is clipped to [`poly_clip`]; at a negative width with an ending, it may
/// draw as far as an ending reaches ([`ends_drawable`]).
pub(super) fn poly_extent(annot: &Entries<'_>, closed: bool) -> Option<(kurbo::Rect, f64)> {
    let vertices = annot.items(b"Vertices")?.derived::<Vertices>();
    let bounds = vertices.1.filter(|_| vertices.0.len() >= 2)?;
    let w = border_width(annot);
    let ends = poly_ends(annot, &vertices.0, closed);
    Some(
        if w < 0.0 && ends.iter().any(|(e, _, _)| *e != Ending::None) {
            (bounds, stroke_reach(w).max(ending_reach(w)))
        } else {
            (poly_clip(bounds, w, &ends), 0.0)
        },
    )
}

/// How far from its tip an ending of a line `width` wide may paint, its stroke
/// included: an arrow's arms reach 8.8 widths of at least 1 back and 4.5 across,
/// the farthest of the endings ([`Ending::path`]), here of the width's magnitude,
/// as a viewer may size it.
fn ending_reach(width: f64) -> f64 {
    10.0 * width.abs().max(1.0) + stroke_reach(width)
}

/// How a `Line` is laid out ([`line`]): its ends `a` and `b` from `/L`, of length
/// `len` along `normal` turned back, those moved by its leader line, `la` and `lb`,
/// and the leader line's length, extension, offset and side.
struct Layout {
    a: Point,
    b: Point,
    la: Point,
    lb: Point,
    len: f64,
    normal: Vec2,
    ll: f64,
    lle: f64,
    llo: f64,
    sign: f64,
}

/// The layout of `Line` `annot`. A line has two points, as viewers read `/L`:
/// coordinates it lacks, or all when it is not an array, are 0. Without a leader
/// line, its extension and offset do not apply; with one, each is laid out on the
/// side the leader line goes to, as given, negative or not.
fn layout(annot: &Entries<'_>) -> Layout {
    let ends = points(&annot.numbers(b"L", 4).unwrap_or_else(|| vec![0.0; 4]));
    let (a, b) = (ends[0], ends[1]);
    let len = (b - a).hypot();
    let dir = direction(b - a);
    let normal = Vec2::new(-dir.y, dir.x);
    let num = |key: &[u8]| annot.number(key).unwrap_or(0.0);
    let ll = num(b"LL");
    let (lle, llo) = if ll == 0.0 {
        (0.0, 0.0)
    } else {
        (num(b"LLE"), num(b"LLO"))
    };
    let sign = if ll < 0.0 { -1.0 } else { 1.0 };
    let shift = normal * (sign * llo + ll);
    Layout {
        a,
        b,
        la: a + shift,
        lb: b + shift,
        len,
        normal,
        ll,
        lle,
        llo,
        sign,
    }
}

impl Layout {
    /// The points its strokes run between: the line moved by its leader line, and
    /// the leader lines, from the offset along `/LL` and on by its extension.
    fn strokes(&self) -> Vec<Point> {
        let mut drawn = vec![self.a, self.b, self.la, self.lb];
        for p in [self.a, self.b] {
            drawn.push(p + self.normal * (self.sign * self.llo));
            drawn.push(p + self.normal * (self.sign * self.llo + self.ll + self.sign * self.lle));
        }
        drawn
    }

    /// Where viewers set the caption of `annot` in the line's frame, from its values
    /// alone: without an offset (`/CO`, 0 for a number it lacks), inline or on top
    /// as `/CP` says; with one, moved by it from the line's middle, below the line
    /// when it moves down, whatever `/CP` says.
    fn caption_place(&self, annot: &Entries<'_>) -> CaptionPlace {
        let size = f64::from(CAPTION_SIZE);
        let co = annot.numbers(b"CO", 2).unwrap_or_else(|| vec![0.0; 2]);
        let mid = self.len / 2.0 + co[0];
        let (rise, inline) = if co[0] != 0.0 || co[1] != 0.0 {
            (if co[1] < 0.0 { co[1] - size } else { co[1] }, false)
        } else if annot.is(b"CP", b"Top") {
            (0.2 * size, false)
        } else {
            (-0.3 * size, true)
        };
        CaptionPlace { mid, rise, inline }
    }

    /// Where the caption of `annot` may lie, `bytes` bytes of `/Contents`: in the
    /// line's frame, as each byte a glyph at most 2 em wide, centred on where it is
    /// set ([`Layout::caption_place`]), from 1 em below its baseline to 2 em above;
    /// turned any way about the line's start where the line has no direction.
    fn caption(&self, annot: &Entries<'_>, bytes: usize) -> [Point; 4] {
        let size = f64::from(CAPTION_SIZE);
        let width = bytes as f64 * 2.0 * size;
        let CaptionPlace { mid, rise, .. } = self.caption_place(annot);
        let (x0, x1) = (mid - width / 2.0 - size, mid + width / 2.0 + size);
        let (y0, y1) = (rise - size, rise + 2.0 * size);
        let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(|(x, y)| Point::new(x, y));
        if self.len > 0.0 {
            corners.map(|p| self.frame() * p)
        } else {
            let r = corners
                .iter()
                .fold(0.0_f64, |r, p| r.max(p.to_vec2().hypot()));
            [(-r, -r), (r, -r), (r, r), (-r, r)].map(|(x, y)| self.la + Vec2::new(x, y))
        }
    }

    /// The line's frame: along it from `la`, and across it along `normal`.
    fn frame(&self) -> Affine {
        let dir = Vec2::new(self.normal.y, -self.normal.x);
        Affine::new([
            dir.x,
            dir.y,
            self.normal.x,
            self.normal.y,
            self.la.x,
            self.la.y,
        ])
    }

    /// Where a caption of `annot`, `bytes` bytes of `/Contents`, that cannot be set
    /// may make viewers draw otherwise, as far as `paint` strokes the line: in the
    /// caption's box ([`Layout::caption`]), and, inline, on the line where its gap
    /// may lie, and beyond it to the line's end where the line is dashed, as the
    /// dashes start anew after the gap ([`line`]). With an offset or on top, the
    /// caption breaks no line.
    fn failed_caption(&self, annot: &Entries<'_>, bytes: usize, paint: &Paint) -> Part {
        let mut points = self.caption(annot, bytes).to_vec();
        let place = self.caption_place(annot);
        if place.inline && paint.stroke.is_some() && self.len > 0.0 {
            let half = bytes as f64 * f64::from(CAPTION_SIZE) + 3.0;
            let to = if paint.dash.is_empty() {
                place.mid + half
            } else {
                self.len
            };
            let at = |t: f64| self.frame() * Point::new(t.clamp(0.0, self.len), 0.0);
            points.extend([at(place.mid - half), at(to)]);
        }
        Part::near(points, stroke_reach(paint.width))
    }

    /// Of `Line` `annot`, the points of what it strokes and of its caption with
    /// `/Cap`.
    fn body(&self, annot: &Entries<'_>) -> Vec<Point> {
        let mut drawn = self.strokes();
        if annot.bool(b"Cap").unwrap_or(false) {
            drawn.extend(self.caption(annot, annot.text_len(Text::Contents)));
        }
        drawn
    }
}

/// Where viewers set a line's caption ([`Layout::caption_place`]): centred on `mid`
/// along the line, its baseline `rise` above it, and whether it is inline, breaking
/// the line.
struct CaptionPlace {
    mid: f64,
    rise: f64,
    inline: bool,
}

/// The size viewers set a line's caption in.
const CAPTION_SIZE: f32 = 12.0;

/// Where a `Line` ([`line`]) may paint: within the reach returned of the points
/// returned. Those of the line moved by its leader line, at its ends, and of the
/// leader lines ([`Layout::strokes`]); and with a caption, the box its text may
/// lie in ([`Layout::caption`]), read only with `/Cap` and from the length of
/// `/Contents` alone.
pub(super) fn line_extent(annot: &Entries<'_>) -> (Vec<Point>, f64) {
    let layout = layout(annot);
    let mut drawn = layout.strokes();
    let w = border_width(annot);
    let reach = stroke_reach(w).max(ending_reach(w));
    if annot.bool(b"Cap").unwrap_or(false) {
        let bytes = annot.text_len(Text::Contents);
        if bytes > 0 {
            drawn.extend(layout.caption(annot, bytes));
        }
    }
    (drawn, reach)
}

fn polyline(points: &[Point], out: &mut Out) -> Option<()> {
    out.nums(&[points[0].x, points[0].y], "m")?;
    for p in &points[1..] {
        out.nums(&[p.x, p.y], "l")?;
    }
    Some(())
}

/// `Line`: `/L` stroked in /C, moved off its points by a leader line (`/LL`, `/LLE`,
/// `/LLO`), with its `/LE` endings and, with `/Cap`, `/Contents` as a caption in
/// 12 pt Helvetica on one line, its line breaks as spaces as common viewers set it:
/// inline, the line broken around it, or on top (`/CP`), moved by `/CO`. A line of
/// no length is clipped to [`point_clip`]; the box viewers clip others to is not
/// reproduced.
pub(super) fn line<'a>(
    annot: &Entries<'a>,
    doc: &Appearances<'a>,
    visible: kurbo::Rect,
    out: &mut Out,
) -> Drew {
    let layout = layout(annot);
    let Layout {
        a,
        b,
        la,
        lb,
        len,
        normal,
        ll,
        lle,
        llo,
        sign,
    } = layout;
    // With a leader line, a line of no length is drawn at the origin of the page,
    // all of it in one point (observed).
    if ll != 0.0 && len == 0.0 {
        return Ok(None);
    }
    let [first, last] = Ending::of(annot);
    let ends = [(first, la, lb), (last, lb, la)];
    let w = border_width(annot);
    let caption = annot.bool(b"Cap").unwrap_or(false);
    let clip = (len == 0.0 && w >= 0.0).then(|| point_clip(la, w, caption, &ends));
    // Laid out from the values alone; an ending or a caption left out changes
    // nothing else drawn.
    let Some(drawn) = ends_drawable(annot, ends, || layout.body(annot), clip, out)? else {
        return Ok(None);
    };
    // Read only where the line is drawn.
    let paint = Paint::of(annot);

    // The caption, laid out first: an inline one breaks the line. Its text, on one
    // line, is set once for the document, and only what the page shows of it drawn.
    let mut gap = None;
    // Where viewers may break the line for the gap or not otherwise than the gap
    // here does: the part of the line between where they may start and end it.
    let mut undecided = None;
    let mut caption_text = None;
    if caption {
        let source = annot.source(Text::Contents.key());
        let text = doc.plain(source, Family::Sans.into(), CAPTION, || {
            Ok(annot
                .text(Text::Contents)
                .map(|s| text::text_string(&s))
                .map(|c| c.replace("\r\n", " ").replace(['\r', '\n'], " "))
                .unwrap_or_default())
        });
        // Text that cannot be set may lie anywhere in the caption's box and, inline,
        // break the line anywhere it may: left out where neither may show.
        let text = out.part(text, || {
            layout.failed_caption(annot, annot.text_len(Text::Contents), &paint)
        })?;
        if let Some(text) = text.filter(|t| !t.is_blank()) {
            let width = f64::from(text.width(0..text.len(), CAPTION_SIZE) as f32);
            let CaptionPlace { mid, rise, inline } = layout.caption_place(annot);
            if inline {
                let (g0, g1) = (mid - width / 2.0 - 3.0, mid + width / 2.0 + 3.0);
                // Whether the gap breaks the line, or the line is drawn whole, is
                // told from the caption's advance, which viewers add up in their
                // own order: where an end of the gap lies within how far that and
                // the line's length and middle may round of an end of the line,
                // which they draw cannot be told.
                let co = annot.numbers(b"CO", 2).unwrap_or_else(|| vec![0.0; 2]);
                let margin = text::aligned(&text, 0..text.len(), CAPTION_SIZE, mid, len)
                    .plus(Rounding::of(8.0, len.abs() + co[0].abs() + 3.0))
                    .bound()
                    .x;
                if g0.abs() <= margin || (len - g1).abs() <= margin {
                    let at = |t: f64| t.clamp(0.0, len.max(0.0));
                    undecided = Some((at(g0 - margin), at(g1 + margin)));
                }
                gap = Some((g0, g1));
            }
            caption_text = Some((text, mid - width / 2.0, rise));
        }
    }

    if let Some((from, to)) = undecided
        && paint.stroke.is_some()
    {
        // The ways differ where one strokes the gap and the other does not, and,
        // where the line is dashed, after it: the dashes start anew where the line
        // starts again.
        let to = if paint.dash.is_empty() { to } else { len };
        let dir = direction(lb - la);
        let differ = vec![la + dir * from, la + dir * to];
        out.refuse(
            Unsupported::Measure,
            Part::near(differ, stroke_reach(paint.width)),
        )?;
    }
    let drew = draw_line(
        &paint,
        [a, b],
        [la, lb],
        normal,
        [ll, lle, llo, sign],
        gap,
        clip,
        (ends, drawn),
        out,
    );
    out.wrote(drew)?;
    if let Some((text, x, rise)) = caption_text {
        out.op("0 g");
        let dir = direction(lb - la);
        let frame = Affine::new([dir.x, dir.y, normal.x, normal.y, la.x, la.y]);
        let set = Setting {
            base: CAPTION_SIZE,
            color: Color::Gray(0.0),
            align: Align::Left,
        };
        let origin = (f64::from(x as f32), f64::from(rise as f32));
        // Centred on the line by its advance; up by the size and `/CO`.
        let computed = (
            text::aligned(&text, 0..text.len(), set.base, origin.0, (lb - la).hypot()),
            Rounding::of(3.0, origin.1.abs() + f64::from(set.base)),
        );
        text::draw_line(
            &doc.fonts, &text, &set, origin, computed, frame, visible, out,
        )?;
    }
    Ok(Some(()))
}

/// Draws a line from its layout (see [`line`]), clipped to `clip` where known, with
/// the endings `ends` of which `drawn` says they are drawn.
#[allow(clippy::too_many_arguments)]
fn draw_line(
    paint: &Paint,
    [a, b]: [Point; 2],
    [la, lb]: [Point; 2],
    normal: Vec2,
    [ll, lle, llo, sign]: [f64; 4],
    gap: Option<(f64, f64)>,
    clip: Option<kurbo::Rect>,
    (ends, drawn): ([(Ending, Point, Point); 2], [bool; 2]),
    out: &mut Out,
) -> Option<()> {
    let len = (lb - la).hypot();
    let dir = direction(lb - la);
    if let Some(clip) = clip {
        out.clip(clip)?;
    }
    paint.start(out)?;
    if ll != 0.0 {
        for p in [a, b] {
            let from = p + normal * (sign * llo);
            let to = p + normal * (sign * llo + ll + sign * lle);
            out.nums(&[from.x, from.y], "m")?;
            out.nums(&[to.x, to.y], "l")?;
        }
    }
    let at = |t: f64| la + dir * t;
    match gap {
        Some((g0, g1)) if g0 > 0.0 && g1 < len => {
            for (s, e) in [(0.0, g0), (g1, len)] {
                out.nums(&[at(s).x, at(s).y], "m")?;
                out.nums(&[at(e).x, at(e).y], "l")?;
            }
        }
        _ => {
            out.nums(&[la.x, la.y], "m")?;
            out.nums(&[lb.x, lb.y], "l")?;
        }
    }
    out.op(paint.stroke_op());
    draw_ends(ends, drawn, paint, out)
}
