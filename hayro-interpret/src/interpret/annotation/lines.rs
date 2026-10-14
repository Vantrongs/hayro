//! `Line`, `Polygon` and `PolyLine` (PDF 32000-1 12.5.6.7, 12.5.6.9), with the line
//! ending styles of Table 176, leader lines and captions. Shapes follow the spec's
//! descriptions; their sizes, three times the border width for squares, circles
//! and diamonds and an arrow 8.8 widths long and 9 wide, are chosen so that the
//! endings look as common viewers draw them.

use super::text::{self, Family, Fonts};
use super::{Out, Unsupported, border_width, color, dash};
use hayro_syntax::object::dict::keys::{C, IC};
use hayro_syntax::object::{Array, Dict, Name, Object};
use kurbo::{Affine, Point, Vec2};

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

    /// The endings of `/LE`: two names, or, for a free text callout, one.
    pub(super) fn of(annot: &Dict<'_>) -> [Self; 2] {
        match annot.get::<Object<'_>>(b"LE") {
            Some(Object::Name(n)) => [Self::from_name(n.as_ref()), Self::None],
            Some(Object::Array(a)) => {
                let mut names = a.iter::<Name<'_>>().map(|n| Self::from_name(n.as_ref()));
                [
                    names.next().unwrap_or(Self::None),
                    names.next().unwrap_or(Self::None),
                ]
            }
            _ => [Self::None, Self::None],
        }
    }
}

/// How a shape is painted: its stroke colour and width, dash, and interior fill.
pub(super) struct Paint {
    pub(super) stroke: String,
    pub(super) width: f32,
    pub(super) dash: Vec<f32>,
    pub(super) fill: Option<String>,
}

impl Paint {
    /// /C strokes at the border width with the border dash, /IC fills; `None` without
    /// a stroke colour.
    fn of(annot: &Dict<'_>) -> Option<Self> {
        Some(Self {
            stroke: annot.get::<Array<'_>>(C).and_then(|a| color(&a, true))?,
            width: border_width(annot),
            dash: dash(annot),
            fill: annot.get::<Array<'_>>(IC).and_then(|a| color(&a, false)),
        })
    }

    fn start(&self, out: &mut Out) -> Option<()> {
        out.op(&self.stroke);
        if let Some(fill) = &self.fill {
            out.op(fill);
        }
        out.stroke_state(self.width, &self.dash)
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
    let w = f64::from(paint.width);
    let along = tip - from;
    let len = along.hypot();
    let dir = if len > 0.0 {
        along / len
    } else {
        Vec2::new(1.0, 0.0)
    };
    let normal = Vec2::new(-dir.y, dir.x);
    let fill_op = if paint.fill.is_some() { "b" } else { "s" };
    // Dashes do not apply to endings.
    out.op("[] 0 d");
    let pt = |p: Point| [p.x as f32, p.y as f32];
    match ending {
        Ending::None => return Some(()),
        Ending::Square | Ending::Diamond | Ending::Circle => {
            let r = 3.0 * w;
            if ending == Ending::Circle {
                let circle = kurbo::Circle::new(tip, r);
                out.path(&kurbo::Shape::to_path(&circle, 0.01))?;
            } else if ending == Ending::Square {
                out.nums(
                    &[
                        (tip.x - r) as f32,
                        (tip.y - r) as f32,
                        (2.0 * r) as f32,
                        (2.0 * r) as f32,
                    ],
                    "re",
                )?;
            } else {
                out.nums(&pt(tip + Vec2::new(0.0, r)), "m")?;
                out.nums(&pt(tip + Vec2::new(r, 0.0)), "l")?;
                out.nums(&pt(tip + Vec2::new(0.0, -r)), "l")?;
                out.nums(&pt(tip + Vec2::new(-r, 0.0)), "l")?;
                out.op("h");
            }
            out.op(fill_op);
        }
        Ending::OpenArrow | Ending::ClosedArrow | Ending::ROpenArrow | Ending::RClosedArrow => {
            // The arms run back along the line, or, reversed, on beyond its end.
            let back = if matches!(ending, Ending::OpenArrow | Ending::ClosedArrow) {
                -dir
            } else {
                dir
            };
            let base = tip + back * (8.8 * w);
            let (a, b) = (base + normal * (4.5 * w), base - normal * (4.5 * w));
            out.nums(&pt(a), "m")?;
            out.nums(&pt(tip), "l")?;
            out.nums(&pt(b), "l")?;
            if matches!(ending, Ending::ClosedArrow | Ending::RClosedArrow) {
                out.op("h");
                out.op(fill_op);
            } else {
                out.op("S");
            }
        }
        Ending::Butt => {
            out.nums(&pt(tip + normal * (3.0 * w)), "m")?;
            out.nums(&pt(tip - normal * (3.0 * w)), "l")?;
            out.op("S");
        }
        Ending::Slash => {
            // Across the line at 60 degrees.
            let (sin, cos) = std::f64::consts::FRAC_PI_3.sin_cos();
            let slant = Vec2::new(dir.x * cos - dir.y * sin, dir.x * sin + dir.y * cos);
            out.nums(&pt(tip + slant * (5.0 * w)), "m")?;
            out.nums(&pt(tip - slant * (5.0 * w)), "l")?;
            out.op("S");
        }
    }
    Some(())
}

/// The numbers of `key` as points.
fn points(annot: &Dict<'_>, key: &[u8]) -> Vec<Point> {
    let Some(arr) = annot.get::<Array<'_>>(key) else {
        return Vec::new();
    };
    let n: Vec<f64> = arr.iter::<f64>().collect();
    n.as_chunks::<2>()
        .0
        .iter()
        .map(|[x, y]| Point::new(*x, *y))
        .filter(|p| p.x.is_finite() && p.y.is_finite())
        .collect()
}

/// `Polygon` and `PolyLine`: `/Vertices` joined, a polygon closed and filled with /IC,
/// a polyline with its `/LE` endings at the first and last vertex.
pub(super) fn poly(annot: &Dict<'_>, closed: bool, out: &mut Out) -> Option<()> {
    let vertices = points(annot, b"Vertices");
    if vertices.len() < 2 {
        return None;
    }
    let stroke = annot.get::<Array<'_>>(C).and_then(|a| color(&a, true));
    let fill = annot.get::<Array<'_>>(IC).and_then(|a| color(&a, false));
    if closed {
        if stroke.is_none() && fill.is_none() {
            return None;
        }
        if let Some(stroke) = &stroke {
            out.op(stroke);
        }
        if let Some(fill) = &fill {
            out.op(fill);
        }
        out.stroke_state(border_width(annot), &dash(annot))?;
        polyline(&vertices, out)?;
        out.op(match (&stroke, &fill) {
            (Some(_), Some(_)) => "b",
            (Some(_), None) => "s",
            _ => "h f",
        });
        return Some(());
    }
    let paint = Paint::of(annot)?;
    paint.start(out)?;
    polyline(&vertices, out)?;
    out.op("S");
    let [first, last] = Ending::of(annot);
    let n = vertices.len();
    ending(first, vertices[0], vertices[1], &paint, out)?;
    ending(last, vertices[n - 1], vertices[n - 2], &paint, out)
}

fn polyline(points: &[Point], out: &mut Out) -> Option<()> {
    out.nums(&[points[0].x as f32, points[0].y as f32], "m")?;
    for p in &points[1..] {
        out.nums(&[p.x as f32, p.y as f32], "l")?;
    }
    Some(())
}

/// `Line`: `/L` stroked in /C, moved off its points by a leader line (`/LL`, `/LLE`,
/// `/LLO`), with its `/LE` endings and, with `/Cap`, `/Contents` as a caption in
/// 12 pt Helvetica: inline, the line broken around it, or on top (`/CP`), moved by
/// `/CO`.
pub(super) fn line(
    annot: &Dict<'_>,
    fonts: &Fonts,
    out: &mut Out,
) -> Result<Option<()>, Unsupported> {
    let ends = points(annot, b"L");
    let (Some(paint), [a, b, ..]) = (Paint::of(annot), ends.as_slice()) else {
        return Ok(None);
    };
    let (a, b) = (*a, *b);
    let along = b - a;
    let len = along.hypot();
    let dir = if len > 0.0 {
        along / len
    } else {
        Vec2::new(1.0, 0.0)
    };
    let normal = Vec2::new(-dir.y, dir.x);
    let num = |key: &[u8]| {
        annot
            .get::<f64>(key)
            .filter(|v| v.is_finite())
            .unwrap_or(0.0)
    };
    let (ll, lle, llo) = (num(b"LL"), num(b"LLE").abs(), num(b"LLO").abs());
    let sign = if ll < 0.0 { -1.0 } else { 1.0 };
    let shift = normal * (sign * llo + ll);
    let (la, lb) = (a + shift, b + shift);

    // The caption, laid out first: an inline one breaks the line.
    let caption = annot.get::<bool>(b"Cap").unwrap_or(false);
    let contents = annot
        .get::<hayro_syntax::object::String<'_>>(b"Contents")
        .map(|s| text::text_string(s.as_bytes()))
        .filter(|s| !s.trim().is_empty());
    let mut gap = None;
    let mut caption_glyphs = None;
    if let (true, Some(contents)) = (caption, contents) {
        const SIZE: f32 = 12.0;
        let line_text = contents.lines().next().unwrap_or("").to_owned();
        let glyphs = fonts.shape(Family::Sans, &line_text)?;
        let width = f64::from(text::width(&glyphs, SIZE));
        let top = annot
            .get::<Name<'_>>(b"CP")
            .is_some_and(|p| p.as_ref() == b"Top");
        let offset = points(annot, b"CO").first().copied().unwrap_or(Point::ZERO);
        let size = f64::from(SIZE);
        let mid = len / 2.0 + offset.x;
        let rise = if top {
            offset.y + 0.2 * size
        } else if offset.y == 0.0 {
            gap = Some((mid - width / 2.0 - 3.0, mid + width / 2.0 + 3.0));
            -0.3 * size
        } else {
            offset.y
        };
        caption_glyphs = Some((glyphs, mid - width / 2.0, rise));
    }

    draw_line(
        &paint,
        [a, b],
        [la, lb],
        normal,
        [ll, lle, llo, sign],
        gap,
        caption_glyphs,
        annot,
        out,
    )
    .ok_or(Unsupported::Kind)?;
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
    caption: Option<(Vec<text::Glyph<'_>>, f64, f64)>,
    annot: &Dict<'_>,
    out: &mut Out,
) -> Option<()> {
    let along = lb - la;
    let len = along.hypot();
    let dir = if len > 0.0 {
        along / len
    } else {
        Vec2::new(1.0, 0.0)
    };
    paint.start(out)?;
    if ll != 0.0 {
        for p in [a, b] {
            let from = p + normal * (sign * llo);
            let to = p + normal * (sign * llo + ll + sign * lle);
            out.nums(&[from.x as f32, from.y as f32], "m")?;
            out.nums(&[to.x as f32, to.y as f32], "l")?;
        }
    }
    let at = |t: f64| la + dir * t;
    match gap {
        Some((g0, g1)) if g0 > 0.0 && g1 < len => {
            for (s, e) in [(0.0, g0), (g1, len)] {
                out.nums(&[at(s).x as f32, at(s).y as f32], "m")?;
                out.nums(&[at(e).x as f32, at(e).y as f32], "l")?;
            }
        }
        _ => {
            out.nums(&[la.x as f32, la.y as f32], "m")?;
            out.nums(&[lb.x as f32, lb.y as f32], "l")?;
        }
    }
    out.op("S");
    let [first, last] = Ending::of(annot);
    ending(first, la, lb, paint, out)?;
    ending(last, lb, la, paint, out)?;

    if let Some((glyphs, x, rise)) = caption {
        out.op("0 g");
        let frame = Affine::new([dir.x, dir.y, normal.x, normal.y, la.x, la.y]);
        text::draw(&glyphs, 12.0, (x as f32, rise as f32), frame, out)?;
    }
    Some(())
}
