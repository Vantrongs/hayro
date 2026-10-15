//! `Line`, `Polygon` and `PolyLine` (PDF 32000-1 12.5.6.7, 12.5.6.9), with the line
//! ending styles of Table 176, leader lines and captions. Shapes follow the spec's
//! descriptions; their sizes, three times the border width for squares, circles
//! and diamonds and an arrow 8.8 widths long and 9 wide, are chosen so that the
//! endings look as common viewers draw them.

use super::entries::{Entries, Text};
use super::out::{Color, Out};
use super::text::{self, Align, Family, Setting};
use super::{Appearances, CAPTION, Unsupported, border_width, circle, color_of, dash};
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
    /// exactly two entries, each a name or none.
    pub(super) fn of(annot: &Entries<'_>) -> [Self; 2] {
        match annot.items(b"LE") {
            Some(names) if names.iter().count() == 2 => {
                [0, 1].map(|i| names.get(i).name().map_or(Self::None, Self::from_name))
            }
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
    /// /C strokes at the border width with the border dash, /IC fills.
    fn of(annot: &Entries<'_>) -> Self {
        Self {
            stroke: color_of(annot, C),
            width: border_width(annot),
            dash: dash(annot),
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

    /// The box viewers clip a free text callout to, which ends in this ending at
    /// `tip`, arriving from `from`, drawn `width` wide, and holds `around` (the
    /// box and the callout's points): with an arrow, butt or slash, all of it out
    /// by the width as given; with a square, circle or diamond, `around` and the
    /// shape out by its stroke (half the width, along a diagonal at the diamond's
    /// corners); without an ending, `around`.
    pub(super) fn callout_box(
        self,
        tip: Point,
        from: Point,
        width: f64,
        around: kurbo::Rect,
    ) -> kurbo::Rect {
        let Some((path, _)) = self.path(tip, from, width) else {
            return around;
        };
        let shape = path.control_box();
        match self {
            Self::Square | Self::Circle => around.union(shape.inflate(width / 2.0, width / 2.0)),
            Self::Diamond => {
                let d = width / std::f64::consts::SQRT_2;
                around.union(shape.inflate(d, d))
            }
            _ => around.union(shape).inflate(width, width),
        }
    }
}

/// The unit vector along `v`; none, of no length, when `v` has no length.
fn direction(v: Vec2) -> Vec2 {
    let len = v.hypot();
    if len > 0.0 { v / len } else { Vec2::ZERO }
}

/// The numbers of `key` as points.
fn points(annot: &Entries<'_>, key: &[u8]) -> Vec<Point> {
    // A line has two points, as viewers read `/L`: coordinates it lacks, or all
    // when it is not an array, are 0.
    let n = if key == b"L" {
        annot.numbers(key, 4).unwrap_or_else(|| vec![0.0; 4])
    } else {
        annot.items(key).map(|i| i.numbers()).unwrap_or_default()
    };
    n.as_chunks::<2>()
        .0
        .iter()
        .map(|[x, y]| Point::new(*x, *y))
        .collect()
}

/// `Polygon` and `PolyLine`: `/Vertices` joined, a polygon closed and filled with /IC,
/// a polyline with its `/LE` endings at the first and last vertex.
///
/// Viewers clip a polygon, and a polyline without endings, to the bounds of its
/// vertices out by the border width as given, cutting off the tip of a sharp
/// mitred corner there, or, for a negative width, the shape's edges.
pub(super) fn poly(
    annot: &Entries<'_>,
    closed: bool,
    out: &mut Out,
) -> Result<Option<()>, Unsupported> {
    let vertices = points(annot, b"Vertices");
    if vertices.len() < 2 {
        return Ok(None);
    }
    let endings = if closed {
        [Ending::None; 2]
    } else {
        Ending::of(annot)
    };
    let n = vertices.len();
    ends_drawable(
        annot,
        [
            (endings[0], vertices[0], vertices[1]),
            (endings[1], vertices[n - 1], vertices[n - 2]),
        ],
    )?;
    Ok(draw_poly(annot, &vertices, closed, endings, out))
}

/// [`Unsupported::Ending`] when a line, polyline or callout draws an ending, each
/// at its tip arriving from a point, whose content viewers clip to a box they
/// compute by rules not reproduced: at a negative border width, where the box
/// shrinks (observed: at width -6, most of a square or arrow ending cut off), and
/// for an ending laid along or across a line of no length, whose points they put
/// at the page's origin, outside the box (observed: an open arrow at 50 60 is
/// `0 0 m 50 60 l 0 0 l`, clipped to 44 54 56 66).
pub(super) fn ends_drawable(
    annot: &Entries<'_>,
    ends: [(Ending, Point, Point); 2],
) -> Result<(), Unsupported> {
    if color_of(annot, C).is_none() && color_of(annot, IC).is_none() {
        return Ok(());
    }
    let negative = border_width(annot) < 0.0;
    for (ending, tip, from) in ends {
        if ending != Ending::None && (negative || ending.lost_at(tip, from)) {
            return Err(Unsupported::Ending);
        }
    }
    Ok(())
}

fn draw_poly(
    annot: &Entries<'_>,
    vertices: &[Point],
    closed: bool,
    endings: [Ending; 2],
    out: &mut Out,
) -> Option<()> {
    let stroke = color_of(annot, C);
    let fill = color_of(annot, IC);
    if closed || endings.iter().all(|e| matches!(e, Ending::None)) {
        let w = border_width(annot);
        let bounds = vertices.iter().fold(
            kurbo::Rect::from_points(vertices[0], vertices[0]),
            |b, p| b.union_pt(*p),
        );
        out.clip(bounds.inflate(w, w))?;
    }
    if closed {
        if stroke.is_none() && fill.is_none() {
            return None;
        }
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
    polyline(vertices, out)?;
    out.op(paint.stroke_op());
    let [first, last] = endings;
    let n = vertices.len();
    ending(first, vertices[0], vertices[1], &paint, out)?;
    ending(last, vertices[n - 1], vertices[n - 2], &paint, out)
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
/// inline, the line broken around it, or on top (`/CP`), moved by `/CO`.
pub(super) fn line<'a>(
    annot: &Entries<'a>,
    doc: &Appearances<'a>,
    visible: kurbo::Rect,
    out: &mut Out,
) -> Result<Option<()>, Unsupported> {
    let paint = Paint::of(annot);
    let ends = points(annot, b"L");
    let (a, b) = (ends[0], ends[1]);
    let len = (b - a).hypot();
    let dir = direction(b - a);
    let normal = Vec2::new(-dir.y, dir.x);
    // Without a leader line, its extension and offset do not apply; with one, each
    // is laid out on the side the leader line goes to, as given, negative or not.
    let num = |key: &[u8]| annot.number(key).unwrap_or(0.0);
    let ll = num(b"LL");
    let (lle, llo) = if ll == 0.0 {
        (0.0, 0.0)
    } else {
        (num(b"LLE"), num(b"LLO"))
    };
    // With a leader line, a line of no length is drawn at the origin of the page,
    // all of it in one point (observed).
    if ll != 0.0 && len == 0.0 {
        return Ok(None);
    }
    let [first, last] = Ending::of(annot);
    ends_drawable(annot, [(first, a, b), (last, b, a)])?;
    let sign = if ll < 0.0 { -1.0 } else { 1.0 };
    let shift = normal * (sign * llo + ll);
    let (la, lb) = (a + shift, b + shift);

    // The caption, laid out first: an inline one breaks the line. Its text, on one
    // line, is set once for the document, and only what the page shows of it drawn.
    let caption = annot.bool(b"Cap").unwrap_or(false);
    let mut gap = None;
    let mut caption_text = None;
    if caption {
        const SIZE: f32 = 12.0;
        let source = annot.text_source(Text::Contents);
        let text = doc.plain(Some(&source), Family::Sans.into(), CAPTION, || {
            annot
                .text(Text::Contents)
                .map(|s| text::text_string(&s))
                .map(|c| c.replace("\r\n", " ").replace(['\r', '\n'], " "))
                .unwrap_or_default()
        })?;
        if !text.is_blank() {
            let width = f64::from(text.width(0..text.len(), SIZE) as f32);
            let top = annot.is(b"CP", b"Top");
            // As viewers place it: without an offset (`/CO`, 0 for a number it lacks),
            // inline or on top as `/CP` says; with one, moved by it from the line's
            // middle, below the line when it moves down, whatever `/CP` says.
            let co = annot.numbers(b"CO", 2).unwrap_or_else(|| vec![0.0; 2]);
            let size = f64::from(SIZE);
            let mid = len / 2.0 + co[0];
            let rise = if co[0] != 0.0 || co[1] != 0.0 {
                if co[1] < 0.0 { co[1] - size } else { co[1] }
            } else if top {
                0.2 * size
            } else {
                gap = Some((mid - width / 2.0 - 3.0, mid + width / 2.0 + 3.0));
                -0.3 * size
            };
            caption_text = Some((text, mid - width / 2.0, rise));
        }
    }

    draw_line(
        &paint,
        [a, b],
        [la, lb],
        normal,
        [ll, lle, llo, sign],
        gap,
        annot,
        out,
    )
    .ok_or(Unsupported::Kind)?;
    if let Some((text, x, rise)) = caption_text {
        out.op("0 g");
        let dir = direction(lb - la);
        let frame = Affine::new([dir.x, dir.y, normal.x, normal.y, la.x, la.y]);
        let set = Setting {
            base: 12.0,
            color: Color::Gray(0.0),
            align: Align::Left,
        };
        let origin = (f64::from(x as f32), f64::from(rise as f32));
        text::draw_line(&doc.fonts, &text, &set, origin, frame, visible, out)?;
    }
    Ok(Some(()))
}

/// Draws a line from its layout (see [`line`]).
#[allow(clippy::too_many_arguments)]
fn draw_line(
    paint: &Paint,
    [a, b]: [Point; 2],
    [la, lb]: [Point; 2],
    normal: Vec2,
    [ll, lle, llo, sign]: [f64; 4],
    gap: Option<(f64, f64)>,
    annot: &Entries<'_>,
    out: &mut Out,
) -> Option<()> {
    let len = (lb - la).hypot();
    let dir = direction(lb - la);
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
    let [first, last] = Ending::of(annot);
    ending(first, la, lb, paint, out)?;
    ending(last, lb, la, paint, out)?;
    Some(())
}
