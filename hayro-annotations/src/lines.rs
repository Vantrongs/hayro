//! `Line`, `Polygon` and `PolyLine` (PDF 32000-1 12.5.6.7, 12.5.6.9), with the line
//! ending styles of Table 176, leader lines and captions. Shapes follow the spec's
//! descriptions; their sizes, three times the border width for squares, circles
//! and diamonds and an arrow 8.8 widths long and 9 wide, are chosen so that the
//! endings look as common viewers draw them.

use super::entries::{Derived, Entries, Items, Numbers, Text};
use super::out::{Color, Dash, Out, Part, Refused, Rounding, corners, stroke_reach};
use super::text::{self, Align, Family, Setting};
use super::{Appearances, CAPTION, Drew, Unsupported, border_width, circle, color_of, dash};
use crate::marks::Mark;
use hayro_syntax::object::dict::keys::{C, IC};
use kurbo::{Affine, BezPath, Point, Vec2};
use std::rc::Rc;

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

    pub(super) fn of(annot: &Entries<'_>) -> [Self; 2] {
        let names = annot.items(b"LE").map(|i| i.head(3)).unwrap_or_default();
        match names.as_slice() {
            [first, last] => [first, last].map(|n| n.name().map_or(Self::None, Self::from_name)),
            _ => [Self::None, Self::None],
        }
    }

    pub(super) fn lost_at(self, tip: Point, from: Point) -> bool {
        let along = !matches!(
            self,
            Self::None | Self::Square | Self::Circle | Self::Diamond
        );
        along && tip == from && tip != Point::ORIGIN
    }

    pub(super) fn of_callout(annot: &Entries<'_>) -> Self {
        annot.get(b"LE").name().map_or(Self::None, Self::from_name)
    }
}

pub(super) struct Paint {
    pub(super) stroke: Option<Color>,
    pub(super) width: f64,
    pub(super) dash: Option<Rc<Dash>>,
    pub(super) fill: Option<Color>,
}

impl Paint {
    fn of(annot: &Entries<'_>) -> Self {
        let stroke = color_of(annot, C);
        let dash = stroke.as_ref().and_then(|_| dash(annot));
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
        out.stroke_state(self.width, self.dash.as_ref())
    }

    fn stroke_op(&self) -> Mark {
        if self.stroke.is_some() {
            Mark::STROKE
        } else {
            Mark::Discard
        }
    }

    fn close_op(&self) -> Mark {
        match (&self.stroke, &self.fill) {
            (Some(_), Some(_)) => Mark::CLOSE_FILL_STROKE,
            (Some(_), None) => Mark::CLOSE_STROKE,
            (None, Some(_)) => Mark::CLOSE_FILL,
            (None, None) => Mark::Discard,
        }
    }
}

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
        out.nums(&[r.x0, r.y0, r.width(), r.height()], Mark::Rect)?;
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

    fn ink(self, tip: Point, from: Point, width: f64) -> Option<kurbo::Rect> {
        let (path, _) = self.path(tip, from, width)?;
        Some(path.control_box())
    }
}

fn direction(v: Vec2) -> Vec2 {
    let len = v.hypot();
    if len > 0.0 { v / len } else { Vec2::ZERO }
}

fn points(n: &[f64]) -> Vec<Point> {
    n.as_chunks::<2>()
        .0
        .iter()
        .map(|[x, y]| Point::new(*x, *y))
        .collect()
}

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
    // Laid out from the values alone, and clipped first, so that an ending is told
    // under the clip it is drawn under ([`ends_drawable`]); one left out is not
    // drawn, and changes nothing else. At a negative width, where viewers clip
    // otherwise, an ending is refused or nothing drawn: clipped after.
    if w >= 0.0 {
        out.writes(|out| out.clip(clip))?;
    }
    let Some(drawn) = ends_drawable(annot, ends, || corners(bounds).to_vec(), out)? else {
        return Ok(None);
    };
    if w < 0.0 {
        out.writes(|out| out.clip(clip))?;
    }
    Ok(draw_poly(annot, vertices, closed, ends, drawn, out))
}

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

fn poly_clip(bounds: kurbo::Rect, w: f64, ends: &[(Ending, Point, Point); 2]) -> kurbo::Rect {
    ends.iter()
        .fold(bounds.inflate(w, w), |b, (ending, tip, from)| {
            ending.clip_box(*tip, *from, w, 0.0, b)
        })
}

fn point_clip(tip: Point, w: f64, cap: bool, ends: &[(Ending, Point, Point); 2]) -> kurbo::Rect {
    let grow = w.max(1.0);
    let base = grow + if cap { f64::from(CAPTION_SIZE) } else { 0.0 };
    let around = kurbo::Rect::from_points(tip, tip).inflate(base, base);
    ends.iter().fold(around, |b, (ending, tip, from)| {
        ending.clip_box(*tip, *from, w, grow, b)
    })
}

pub(super) fn ends_drawable(
    annot: &Entries<'_>,
    ends: [(Ending, Point, Point); 2],
    body: impl FnOnce() -> Vec<Point>,
    out: &mut Out,
) -> Result<Option<[bool; 2]>, Refused> {
    let painted = color_of(annot, C).is_some() || color_of(annot, IC).is_some();
    let w = border_width(annot);
    let mut drawn = [false; 2];
    for (draws, (ending, tip, from)) in drawn.iter_mut().zip(&ends) {
        let lost = ending.lost_at(*tip, *from);
        *draws = *ending != Ending::None && !lost;
        if lost && painted {
            out.refuse(Unsupported::Ending, lost_part(*tip, w))?;
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

pub(super) fn lost_part(tip: Point, w: f64) -> Part {
    let half = w.abs().max(1.0) / 2.0;
    let across = direction(tip.to_vec2());
    let across = Vec2::new(-across.y, across.x) * half;
    let band = [
        Point::ORIGIN + across,
        tip + across,
        tip - across,
        Point::ORIGIN - across,
    ];
    Part::near(band.to_vec(), 0.0)
}

fn draw_poly(
    annot: &Entries<'_>,
    vertices: &[Point],
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
    if closed {
        if let Some(stroke) = &stroke {
            out.stroke_color(stroke)?;
        }
        if let Some(fill) = &fill {
            out.fill_color(fill)?;
        }
        out.stroke_state(border_width(annot), dash(annot).as_ref())?;
        polyline(vertices, out)?;
        out.op(match (&stroke, &fill) {
            (Some(_), Some(_)) => Mark::CLOSE_FILL_STROKE,
            (Some(_), None) => Mark::CLOSE_STROKE,
            _ => Mark::CLOSE_FILL,
        });
        return Some(());
    }
    let paint = Paint::of(annot);
    paint.start(out)?;
    if paint.stroke.is_some() {
        polyline(vertices, out)?;
        out.op(Mark::STROKE);
    }
    draw_ends(ends, drawn, &paint, out)
}

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

fn ending_reach(width: f64) -> f64 {
    10.0 * width.abs().max(1.0) + stroke_reach(width)
}

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

fn layout(annot: &Entries<'_>) -> Layout {
    let ends = points(
        &annot
            .numbers(b"L", 4)
            .unwrap_or_else(|| Numbers::from_elem(0.0, 4)),
    );
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
    fn strokes(&self) -> Vec<Point> {
        let mut drawn = vec![self.a, self.b, self.la, self.lb];
        for p in [self.a, self.b] {
            drawn.push(p + self.normal * (self.sign * self.llo));
            drawn.push(p + self.normal * (self.sign * self.llo + self.ll + self.sign * self.lle));
        }
        drawn
    }

    fn caption_place(&self, annot: &Entries<'_>) -> CaptionPlace {
        let size = f64::from(CAPTION_SIZE);
        let co = annot
            .numbers(b"CO", 2)
            .unwrap_or_else(|| Numbers::from_elem(0.0, 2));
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

    fn failed_caption(&self, annot: &Entries<'_>, bytes: usize, paint: &Paint) -> Part {
        let mut points = self.caption(annot, bytes).to_vec();
        let place = self.caption_place(annot);
        let breaks = place.inline && paint.stroke.is_some() && self.len > 0.0;
        if breaks {
            let half = bytes as f64 * f64::from(CAPTION_SIZE) + 3.0;
            let to = if paint.dash.as_ref().is_none_or(|d| d.is_solid()) {
                place.mid + half
            } else {
                self.len
            };
            let at = |t: f64| self.frame() * Point::new(t.clamp(0.0, self.len), 0.0);
            points.extend([at(place.mid - half), at(to)]);
        }
        // The box holds the caption's ink; the line's strokes reach beyond their
        // points.
        Part::near(
            points,
            if breaks {
                stroke_reach(paint.width)
            } else {
                0.0
            },
        )
    }

    fn body(&self, annot: &Entries<'_>) -> Vec<Point> {
        let mut drawn = self.strokes();
        if annot.bool(b"Cap").unwrap_or(false) {
            drawn.extend(self.caption(annot, annot.text_len(Text::Contents)));
        }
        drawn
    }
}

struct CaptionPlace {
    mid: f64,
    rise: f64,
    inline: bool,
}

const CAPTION_SIZE: f32 = 12.0;

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
    out.nums(&[points[0].x, points[0].y], Mark::Move)?;
    for p in &points[1..] {
        out.nums(&[p.x, p.y], Mark::Line)?;
    }
    Some(())
}

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
    // Laid out from the values alone, and clipped first, so that an ending or a
    // caption is told under the clip it is drawn under ([`Out::refuse`]); one left
    // out changes nothing else drawn.
    if len == 0.0 && w >= 0.0 {
        out.writes(|out| out.clip(point_clip(la, w, caption, &ends)))?;
    }
    let Some(drawn) = ends_drawable(annot, ends, || layout.body(annot), out)? else {
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
                let co = annot
                    .numbers(b"CO", 2)
                    .unwrap_or_else(|| Numbers::from_elem(0.0, 2));
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
        let to = if paint.dash.as_ref().is_none_or(|d| d.is_solid()) {
            to
        } else {
            len
        };
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
        (ends, drawn),
        out,
    );
    out.wrote(drew)?;
    if let Some((text, x, rise)) = caption_text {
        out.op(Mark::Color {
            stroke: false,
            components: crate::marks::Ink::Gray([0.]),
        });
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

#[allow(clippy::too_many_arguments)]
fn draw_line(
    paint: &Paint,
    [a, b]: [Point; 2],
    [la, lb]: [Point; 2],
    normal: Vec2,
    [ll, lle, llo, sign]: [f64; 4],
    gap: Option<(f64, f64)>,
    (ends, drawn): ([(Ending, Point, Point); 2], [bool; 2]),
    out: &mut Out,
) -> Option<()> {
    let len = (lb - la).hypot();
    let dir = direction(lb - la);
    paint.start(out)?;
    if ll != 0.0 {
        for p in [a, b] {
            let from = p + normal * (sign * llo);
            let to = p + normal * (sign * llo + ll + sign * lle);
            out.nums(&[from.x, from.y], Mark::Move)?;
            out.nums(&[to.x, to.y], Mark::Line)?;
        }
    }
    let at = |t: f64| la + dir * t;
    match gap {
        Some((g0, g1)) if g0 > 0.0 && g1 < len => {
            for (s, e) in [(0.0, g0), (g1, len)] {
                out.nums(&[at(s).x, at(s).y], Mark::Move)?;
                out.nums(&[at(e).x, at(e).y], Mark::Line)?;
            }
        }
        _ => {
            out.nums(&[la.x, la.y], Mark::Move)?;
            out.nums(&[lb.x, lb.y], Mark::Line)?;
        }
    }
    out.op(paint.stroke_op());
    draw_ends(ends, drawn, paint, out)
}
