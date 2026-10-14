//! The appearances `PDFium` generates for markup annotations drawn from geometry
//! alone (`core/fpdfdoc/cpdf_generateap.cpp`, BSD-3-Clause), with their geometry and
//! defaults.

use super::{Out, border_width, c_color, color, dash, rect};
use hayro_syntax::object::dict::keys::{C, IC, INKLIST, QUADPOINTS};
use hayro_syntax::object::{Array, Dict};

/// Square and Circle: /IC fills the shape, /C strokes it at the border width, inside
/// `/Rect`. Unlike `PDFium`, an absent `/C` means no border: the spec gives it no default.
/// A border of width 0 is the thinnest line, not none.
pub(super) fn shape(annot: &Dict<'_>, circle: bool, out: &mut Out) -> Option<()> {
    let fill = annot.get::<Array<'_>>(IC).and_then(|a| color(&a, false));
    let width = border_width(annot);
    let stroke = annot.get::<Array<'_>>(C).and_then(|a| color(&a, true));
    if fill.is_none() && stroke.is_none() {
        return None;
    }
    let mut r = rect(annot)?;
    if let Some(stroke) = &stroke {
        out.op(stroke);
        out.stroke_state(width, &dash(annot))?;
        let half = f64::from(width) / 2.0;
        r = r.inset(-half);
        if r.width() < 0.0 || r.height() < 0.0 {
            return None;
        }
    }
    if let Some(fill) = &fill {
        out.op(fill);
    }
    let [x0, y0, x1, y1] = [r.x0 as f32, r.y0 as f32, r.x1 as f32, r.y1 as f32];
    if circle {
        // Four cubic quarter arcs; 0.5523 ≈ 4/3·tan(π/8).
        const K: f32 = 0.5523;
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

/// The upright bounding box of each quadrilateral in `/QuadPoints`.
pub(super) fn quads(annot: &Dict<'_>) -> Vec<[f32; 4]> {
    let Some(arr) = annot.get::<Array<'_>>(QUADPOINTS) else {
        return Vec::new();
    };
    let n: Vec<f32> = arr.iter::<f32>().collect();
    n.as_chunks::<8>()
        .0
        .iter()
        .filter(|q| q.iter().all(|v| v.is_finite()))
        .map(|q| {
            let xs = [q[0], q[2], q[4], q[6]];
            let ys = [q[1], q[3], q[5], q[7]];
            let min = |v: [f32; 4]| v.into_iter().fold(f32::INFINITY, f32::min);
            let max = |v: [f32; 4]| v.into_iter().fold(f32::NEG_INFINITY, f32::max);
            [min(xs), min(ys), max(xs), max(ys)]
        })
        .collect()
}

/// Highlight: each quad filled with /C (default yellow), blended with Multiply.
pub(super) fn highlight(annot: &Dict<'_>, out: &mut Out) -> Option<()> {
    let fill = c_color(annot, false, "1 1 0 rg")?;
    let quads = quads(annot);
    if quads.is_empty() {
        return None;
    }
    out.op(&fill);
    for [x0, y0, x1, y1] in quads {
        out.nums(&[x0, y0, x1 - x0, y1 - y0], "re")?;
    }
    out.op("f");
    Some(())
}

pub(super) enum Line {
    Under,
    Through,
    Squiggle,
}

/// `Underline`, `StrikeOut` and `Squiggly`: a 1 pt line per quad in /C (default black): one
/// unit above the bottom, through the middle, or a zigzag 2 pt high and 2 pt per step.
pub(super) fn text_lines(
    annot: &Dict<'_>,
    line: Line,
    visible: kurbo::Rect,
    out: &mut Out,
) -> Option<()> {
    let stroke = c_color(annot, true, "0 G")?;
    let quads = quads(annot);
    if quads.is_empty() {
        return None;
    }
    out.op(&stroke);
    out.stroke_state(1.0, &[])?;
    for [x0, y0, x1, y1] in quads {
        match line {
            Line::Under => {
                out.nums(&[x0, y0 + 1.0], "m")?;
                out.nums(&[x1, y0 + 1.0], "l")?;
            }
            Line::Through => {
                let y = (y0 + y1) / 2.0;
                out.nums(&[x0, y], "m")?;
                out.nums(&[x1, y], "l")?;
            }
            Line::Squiggle => squiggle(x0, x1, y0, visible, out)?,
        }
        out.op("S");
    }
    Some(())
}

/// The zigzag's height and the length of one step.
const DELTA: f64 = 2.0;

/// The zigzag of a `Squiggly` quad from `x0` to `x1` above `y`: 2 pt high, one step per
/// 2 pt from `x0` as in `PDFium`. Only its steps across `visible` and one beyond each
/// side are drawn: the page clips the rest, and the ends cut there, with their caps
/// and joins, lie outside it.
fn squiggle(x0: f32, x1: f32, y: f32, visible: kurbo::Rect, out: &mut Out) -> Option<()> {
    let (bottom, top) = (y, y + DELTA as f32);
    if f64::from(top) + DELTA < visible.y0 || f64::from(bottom) - DELTA > visible.y1 {
        return Some(());
    }
    let (x0, x1) = (f64::from(x0), f64::from(x1));
    // The corners lie at `x0 + 2 i`, a peak for even `i`: at `r + 2 k` for
    // `r = x0 % 4`, a peak for even `k`. Corners near the visible range are so
    // computed from small numbers however far `x0` lies, and in f64: in f32,
    // `x += 2` stops advancing at 2^25.
    let r = x0 % (2.0 * DELTA);
    let corner = |k: f64| (r + k * DELTA) as f32;
    let level = |k: f64| if k % 2.0 == 0.0 { top } else { bottom };
    let first = (x0 - r) / DELTA;
    // The last corner before `x1`, where the last, possibly partial, step starts.
    let last = (((x1 - r) / DELTA).ceil() - 1.0).max(first);
    let from = first.max(((visible.x0 - r) / DELTA).floor() - 1.0);
    let beyond = ((visible.x1 - r) / DELTA).ceil() + 1.0;
    let to = beyond.min(last);
    let ends = beyond > last;
    if from > to || (from == to && !ends) {
        return Some(());
    }
    out.nums(&[corner(from), level(from)], "m")?;
    for k in 1..=(to - from) as u64 {
        let k = from + k as f64;
        out.nums(&[corner(k), level(k)], "l")?;
    }
    if !ends {
        return Some(());
    }
    // The last step keeps the slope.
    let rise = (x1 - (r + last * DELTA)) as f32;
    let y1 = if last % 2.0 == 0.0 {
        top - rise
    } else {
        bottom + rise
    };
    out.nums(&[x1 as f32, y1], "l")
}

/// `Ink`: each `/InkList` path stroked as a polyline in /C at the border width, with
/// the border dash. Unlike `PDFium`, an absent `/C` draws nothing: the spec gives it no
/// default.
pub(super) fn ink(annot: &Dict<'_>, out: &mut Out) -> Option<()> {
    let width = border_width(annot);
    let stroke = annot.get::<Array<'_>>(C).and_then(|a| color(&a, true))?;
    let list = annot.get::<Array<'_>>(INKLIST)?;
    out.op(&stroke);
    out.stroke_state(width, &dash(annot))?;
    let mut drew = false;
    for path in list.iter::<Array<'_>>() {
        let p: Vec<f32> = path.iter::<f32>().collect();
        if p.len() < 2 || !p.iter().all(|v| v.is_finite()) {
            continue;
        }
        out.nums(&p[..2], "m")?;
        for pt in p[2..].as_chunks::<2>().0 {
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
    use super::{Line, text_lines};
    use hayro_syntax::object::{Dict, FromBytes};
    use kurbo::Rect;

    /// A page large enough for every quad below but the huge ones.
    const PAGE: Rect = Rect::new(0.0, 0.0, 1e8, 1e8);

    fn content(annot: &str, visible: Rect) -> String {
        let dict = Dict::from_bytes(annot.as_bytes()).expect("valid dict");
        let mut out = Out(String::new());
        text_lines(&dict, Line::Squiggle, visible, &mut out).expect("an appearance");
        out.0
    }

    fn squiggly(quads: &str, visible: Rect) -> String {
        content(
            &format!("<< /Subtype /Squiggly /Rect [0 0 1 1] /QuadPoints [{quads}] >>"),
            visible,
        )
    }

    #[test]
    fn squiggly_follows_pdfium_steps() {
        let c = squiggly("10 20 15 20 10 10 15 10", PAGE);
        assert!(
            c.ends_with("10 12 m\n12 10 l\n14 12 l\n15 11 l\nS\n"),
            "{c}"
        );
    }

    #[test]
    fn squiggly_ends_at_large_coordinates() {
        // In f32, 2^25 + 2 == 2^25: positions accumulated in f32 never reach x1.
        let c = squiggly("33554432 20 33554440 20 33554432 10 33554440 10", PAGE);
        assert_eq!(c.matches(" l\n").count(), 4, "{c}");
        assert!(c.ends_with(" l\nS\n"), "{c}");
    }

    /// The zigzag of each quad drawn in `c`, from its first point.
    fn zigzags(c: &str) -> Vec<&str> {
        c.split("S\n")
            .filter_map(|q| {
                let m = q.find(" m\n")?;
                Some(&q[q[..m].rfind('\n').map_or(0, |i| i + 1)..])
            })
            .collect()
    }

    /// A quad's zigzag is the same alone and among quads that take more than
    /// 100 000 steps in all.
    #[test]
    fn a_zigzag_does_not_depend_on_the_other_quads() {
        let page = Rect::new(0.0, 0.0, 2100.0, 2100.0);
        let quad = "1 20 2049 20 1 10 2049 10 ";
        let alone = squiggly(quad, page);
        let alone = zigzags(&alone)[0];
        assert_eq!(alone.matches(" l\n").count(), 1024);
        let among = squiggly(&quad.repeat(101), page);
        let among = zigzags(&among);
        assert_eq!(among.len(), 101);
        assert!(among.iter().all(|&z| z == alone));
    }

    /// A quad 10^9 pt wide, starting at its left end or far left of the page, draws
    /// the steps across the page and one beyond each side, with the corners its own
    /// start puts them at.
    #[test]
    fn a_huge_quad_draws_the_steps_the_page_shows() {
        let page = Rect::new(100.0, 0.0, 712.0, 800.0);
        for x0 in ["0", "-1000000000", "-999999999"] {
            let c = squiggly(
                &format!("{x0} 20 1000000000 20 {x0} 10 1000000000 10"),
                page,
            );
            let corners: Vec<(f32, f32)> = c
                .lines()
                .filter(|l| l.ends_with(" l") || l.ends_with(" m"))
                .map(|l| {
                    let n: Vec<f32> = l.split(' ').take(2).map(|v| v.parse().unwrap()).collect();
                    (n[0], n[1])
                })
                .collect();
            let start: f32 = x0.parse().unwrap();
            // The corners at or beyond a step outside the page.
            let (first, last) = if start % 2.0 == 0.0 {
                (98.0, 714.0)
            } else {
                (97.0, 715.0)
            };
            assert_eq!(corners.first().unwrap().0, first, "{x0}");
            assert_eq!(corners.last().unwrap().0, last, "{x0}");
            assert_eq!(corners.len(), ((last - first) / 2.0) as usize + 1, "{x0}");
            for &(x, y) in &corners {
                let i = ((f64::from(x) - f64::from(start)) / 2.0) as i64;
                assert_eq!(y, if i % 2 == 0 { 12.0 } else { 10.0 }, "{x0} at {x}");
            }
        }
        // Off the page, nothing.
        let c = squiggly("0 2000 1000000000 2000 0 1990 1000000000 1990", page);
        assert!(zigzags(&c).is_empty(), "{c}");
    }
}
