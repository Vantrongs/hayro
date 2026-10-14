//! Markup annotations drawn from geometry alone. Square, Circle and Ink follow the
//! appearances `PDFium` generates (`core/fpdfdoc/cpdf_generateap.cpp`,
//! BSD-3-Clause), with their geometry and defaults. The text markup shapes
//! (`Highlight`, `Underline`, `StrikeOut`, `Squiggly`) are drawn in each
//! quadrilateral's own orientation, sized from its height so that they cover what
//! common viewers draw, and clipped as they clip them: to the box of the
//! quadrilaterals, a line width wider on each side.

use super::entries::{Entries, Value};
use super::{FINEST, Out, Unsupported, border_width, c_color, color_of, dash, rect};
use hayro_syntax::object::dict::keys::{C, IC, INKLIST, QUADPOINTS};
use kurbo::{Point, Rect, Vec2};

/// Square and Circle: /IC fills the shape, /C strokes it at the border width, inside
/// `/Rect`. Unlike `PDFium`, an absent `/C` means no border: the spec gives it no default.
/// A border of width 0 is the thinnest line, not none.
pub(super) fn shape(annot: &Entries<'_>, circle: bool, out: &mut Out) -> Option<()> {
    let fill = color_of(annot, IC, false);
    let width = border_width(annot);
    let stroke = color_of(annot, C, true);
    if fill.is_none() && stroke.is_none() {
        return None;
    }
    let mut r = rect(annot)?;
    if let Some(stroke) = &stroke {
        out.op(stroke);
        out.stroke_state(width, &dash(annot))?;
        r = r.inset(-width / 2.0);
        if r.width() < 0.0 || r.height() < 0.0 {
            return None;
        }
    }
    if let Some(fill) = &fill {
        out.op(fill);
    }
    let [x0, y0, x1, y1] = [r.x0, r.y0, r.x1, r.y1];
    if circle {
        // Four cubic quarter arcs; 0.5523 ≈ 4/3·tan(π/8).
        const K: f64 = 0.5523;
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

/// The quadrilaterals of `/QuadPoints`, every number in its place; a last one cut
/// short has 0 for the coordinates it lacks. `None` when there is no such array.
pub(super) fn quads(annot: &Entries<'_>) -> Option<Vec<Quad>> {
    let mut n = annot.numbers(QUADPOINTS, 0)?;
    n.resize(n.len().next_multiple_of(8), 0.0);
    Some(
        n.as_chunks::<8>()
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
            .collect(),
    )
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
fn marked(annot: &Entries<'_>) -> Option<(Vec<Quad>, Rect)> {
    let quads = quads(annot)?;
    let clip = union(quads.iter().map(|q| q.bounds(line_width(q))))?;
    (clip.width() > 0.0 && clip.height() > 0.0).then_some((quads, clip))
}

/// Highlight: each quad filled with /C (default yellow), blended with Multiply, its
/// left and right sides bulging out to a third of its height, turned with its bottom
/// edge. A quad whose bottom edge has no length has no direction: viewers put the
/// curves' control points at the origin, and so does this.
pub(super) fn highlight(annot: &Entries<'_>, out: &mut Out) -> Option<()> {
    let fill = c_color(annot, false, "1 1 0 rg")?;
    let (quads, around) = marked(annot)?;
    let shapes: Vec<(Quad, Option<[Point; 4]>)> = quads
        .into_iter()
        .map(|q| {
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
    out.op(&fill);
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
    let stroke = c_color(annot, true, "0 G")?;
    let (quads, clip) = marked(annot)?;
    out.op("q");
    out.clip(clip)?;
    out.op(&stroke);
    for q in &quads {
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
    let Some(stroke) = c_color(annot, true, "0 G") else {
        return Ok(None);
    };
    let Some((quads, clip)) = marked(annot) else {
        return Ok(None);
    };
    // Every way to fail to write is the coordinates'.
    let wrote = |o: Option<()>| o.ok_or(Unsupported::Coordinates);
    out.op("q");
    wrote(out.clip(clip))?;
    out.op(&stroke);
    if visible.overlaps(clip) {
        let shown = visible.intersect(clip);
        for q in &quads {
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
/// Only its steps that can reach `shown`, the part of the page the clip lets show,
/// are drawn, with one more on each side: the rest is clipped, and the ends cut there,
/// with their caps and joins, lie outside it. Steps finer than [`FINEST`] are drawn
/// as the band they cover: a straight line as high as the zigzag with its 45°
/// strokes, a seventh of `h` + √2 times the line width.
///
/// The steps that can reach `shown` are those whose corners on the bottom edge lie
/// within the zigzag's largest height of it. For quads whose sides are about as long
/// as each other, as those of text are, they are at most the steps across `shown`
/// and a few more; a quad whose sides differ a thousandfold or more can make them
/// many more than `shown` could tell apart, and is [`Unsupported::Coordinates`].
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
    // The corners are `delta` apart along the bottom edge, at `t = k delta / len`.
    let steps = (len / delta).ceil();
    let end = steps * delta / len;
    if delta < FINEST {
        let band = delta + std::f64::consts::SQRT_2 * width;
        let (a, b) = (
            bottom(0.0).midpoint(top(0.0)),
            bottom(end).midpoint(top(end)),
        );
        wrote(out.nums(&[band], "w"))?;
        wrote(out.nums(&[a.x, a.y], "m"))?;
        wrote(out.nums(&[b.x, b.y], "l"))?;
        out.op("S");
        return Ok(());
    }
    let reach = (top(0.0) - bottom(0.0))
        .hypot()
        .max((top(end) - bottom(end)).hypot())
        + width;
    let Some((t0, t1)) = clip_line(bl, along, end, shown.inset(reach)) else {
        return Ok(());
    };
    let first = ((t0 * len / delta).floor() - 1.0).max(0.0);
    let last = ((t1 * len / delta).ceil() + 1.0).min(steps);
    let most = 2.0 * (shown.width().hypot(shown.height()) / FINEST) + 16.0;
    if last - first > most {
        return Err(Unsupported::Coordinates);
    }
    let corner = |k: f64| {
        let t = k * delta / len;
        if k % 2.0 == 0.0 { bottom(t) } else { top(t) }
    };
    let p = corner(first);
    wrote(out.nums(&[p.x, p.y], "m"))?;
    for i in 1..=(last - first) as u64 {
        let p = corner(first + i as f64);
        wrote(out.nums(&[p.x, p.y], "l"))?;
    }
    out.op("S");
    Ok(())
}

/// The range of `t` in `0..=end` for which `from + t along` lies in `r`, when there
/// is one.
fn clip_line(from: Point, along: Vec2, end: f64, r: Rect) -> Option<(f64, f64)> {
    let (mut lo, mut hi) = (0.0_f64, end);
    for (p, q) in [
        (-along.x, from.x - r.x0),
        (along.x, r.x1 - from.x),
        (-along.y, from.y - r.y0),
        (along.y, r.y1 - from.y),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else if p < 0.0 {
            lo = lo.max(q / p);
        } else {
            hi = hi.min(q / p);
        }
    }
    (lo <= hi).then_some((lo, hi))
}

/// `Ink`: each `/InkList` path stroked as a polyline in /C at the border width, with
/// the border dash, round ends and joins; a path of one point is a dot. Unlike `PDFium`, an absent `/C` draws nothing: the spec gives it no
/// default.
pub(super) fn ink(annot: &Entries<'_>, out: &mut Out) -> Option<()> {
    let width = border_width(annot);
    let stroke = color_of(annot, C, true)?;
    let list = annot.items(INKLIST)?;
    out.op(&stroke);
    out.stroke_state(width, &dash(annot))?;
    // Round ends and corners, as a pen draws.
    out.op("1 J 1 j");
    let mut drew = false;
    for path in list.iter().filter_map(Value::items) {
        let p = path.numbers();
        if p.len() < 2 {
            continue;
        }
        out.nums(&p[..2], "m")?;
        // A path of one point is a dot.
        let rest = if p.len() < 4 { &p[..2] } else { &p[2..] };
        for pt in rest.as_chunks::<2>().0 {
            out.nums(pt, "l")?;
        }
        out.op("S");
        drew = true;
    }
    drew.then_some(())
}

#[cfg(test)]
mod tests {
    use super::super::Out;
    use super::super::entries::Entries;
    use super::squiggly;
    use hayro_syntax::object::{Dict, FromBytes};
    use kurbo::Rect;

    /// A page large enough for every quad below but the huge ones.
    const PAGE: Rect = Rect::new(0.0, 0.0, 1e8, 1e8);

    fn squiggly_content(quads: &str, visible: Rect) -> String {
        let annot = format!("<< /Subtype /Squiggly /Rect [0 0 1 1] /QuadPoints [{quads}] >>");
        let dict = Dict::from_bytes(annot.as_bytes()).expect("valid dict");
        let mut out = Out::default();
        squiggly(&Entries::new(dict), visible, &mut out)
            .expect("drawable")
            .expect("an appearance");
        out.content
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

    #[test]
    fn squiggly_ends_at_large_coordinates() {
        // In f32, 2^25 + 2 == 2^25: positions accumulated in f32 never reach x1.
        let c = squiggly_content("33554432 24 33554440 24 33554432 10 33554440 10", PAGE);
        assert_eq!(c.matches(" l\n").count(), 4, "{c}");
        assert!(c.ends_with(" l\nS\nQ\n"), "{c}");
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

    /// A quad 10^9 pt wide, starting at its left end or far left of the page, draws
    /// the steps across the page and a few beyond each side, with the corners its own
    /// start puts them at.
    #[test]
    fn a_huge_quad_draws_the_steps_the_page_shows() {
        let page = Rect::new(100.0, 0.0, 712.0, 800.0);
        for x0 in ["0", "-1000000000", "-999999999"] {
            let c = squiggly_content(
                &format!("{x0} 24 1000000000 24 {x0} 10 1000000000 10"),
                page,
            );
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
        let c = squiggly_content("0 2000 1000000000 2000 0 1986 1000000000 1986", page);
        assert!(corners(&c).is_empty(), "{c}");
    }
}
