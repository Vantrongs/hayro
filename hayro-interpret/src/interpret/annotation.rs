//! Appearances for annotations without an `/AP` stream, for the subtypes `PDFium`
//! generates one for, with its geometry and defaults (`core/fpdfdoc/cpdf_generateap.cpp`,
//! BSD-3-Clause). `FreeText` and `Popup` are left out: their appearance is laid-out text.
//!
//! The appearance is a content stream in default user space; /CA and the highlight's
//! Multiply blend apply to it as one transparency group.

use crate::BlendMode;
use crate::util::RectExt;
use hayro_syntax::object::dict::keys::{
    BORDER, BS, C, CA, D, IC, INKLIST, QUADPOINTS, RECT, S, SUBTYPE, W,
};
use hayro_syntax::object::{Array, Dict, Name, Object, Rect};
use std::fmt::Write;

pub(crate) struct Appearance {
    pub(crate) content: String,
    pub(crate) opacity: f32,
    pub(crate) blend: BlendMode,
}

/// The appearance `PDFium` would generate for `annot`, if its subtype gets one and it
/// paints anything.
pub(crate) fn generate(annot: &Dict<'_>) -> Option<Appearance> {
    let subtype = annot.get::<Name<'_>>(SUBTYPE)?;
    let mut out = Out(String::new());
    let blend = match subtype.as_ref() {
        b"Square" => {
            shape(annot, false, &mut out)?;
            BlendMode::Normal
        }
        b"Circle" => {
            shape(annot, true, &mut out)?;
            BlendMode::Normal
        }
        b"Highlight" => {
            highlight(annot, &mut out)?;
            BlendMode::Multiply
        }
        b"Underline" => {
            text_lines(annot, Line::Under, &mut out)?;
            BlendMode::Normal
        }
        b"StrikeOut" => {
            text_lines(annot, Line::Through, &mut out)?;
            BlendMode::Normal
        }
        b"Squiggly" => {
            text_lines(annot, Line::Squiggle, &mut out)?;
            BlendMode::Normal
        }
        b"Ink" => {
            ink(annot, &mut out)?;
            BlendMode::Normal
        }
        b"Text" => {
            note_icon(annot, &mut out)?;
            BlendMode::Normal
        }
        _ => return None,
    };
    let opacity = annot.get::<f32>(CA).unwrap_or(1.0);
    let opacity = if opacity.is_finite() {
        opacity.clamp(0.0, 1.0)
    } else {
        1.0
    };
    Some(Appearance {
        content: out.0,
        opacity,
        blend,
    })
}

/// Content stream text; every number written is finite.
struct Out(String);

impl Out {
    fn nums(&mut self, nums: &[f32], op: &str) -> Option<()> {
        if !nums.iter().all(|n| n.is_finite()) {
            return None;
        }
        for n in nums {
            write!(self.0, "{n} ").ok()?;
        }
        self.0.push_str(op);
        self.0.push('\n');
        Some(())
    }

    fn op(&mut self, op: &str) {
        self.0.push_str(op);
        self.0.push('\n');
    }

    /// The stroke state every generated appearance starts from.
    fn stroke_state(&mut self, width: f32, dash: &[f32]) -> Option<()> {
        self.op("0 J 0 j 10 M");
        self.nums(&[width], "w")?;
        self.0.push('[');
        for d in dash {
            write!(self.0, "{d} ").ok()?;
        }
        self.op("] 0 d");
        Some(())
    }
}

/// The colour operator for a colour array (gray, RGB or CMYK); `None` when the array
/// has no components (transparent) or an invalid count.
fn color(arr: &Array<'_>, stroke: bool) -> Option<String> {
    let c: Vec<f32> = arr.iter::<f32>().filter(|c| c.is_finite()).collect();
    let op = match (c.len(), stroke) {
        (1, false) => "g",
        (1, true) => "G",
        (3, false) => "rg",
        (3, true) => "RG",
        (4, false) => "k",
        (4, true) => "K",
        _ => return None,
    };
    let mut s = String::new();
    for n in &c {
        write!(s, "{n} ").ok()?;
    }
    s.push_str(op);
    Some(s)
}

/// /C as a colour operator: `default` when absent, `None` when it is transparent.
fn c_color(annot: &Dict<'_>, stroke: bool, default: &str) -> Option<String> {
    match annot.get::<Array<'_>>(C) {
        Some(arr) => color(&arr, stroke),
        None => Some(default.to_string()),
    }
}

/// /BS /W, else the third /Border element, else 1.
fn border_width(annot: &Dict<'_>) -> f32 {
    annot
        .get::<Dict<'_>>(BS)
        .and_then(|bs| bs.get::<f32>(W))
        .or_else(|| {
            annot
                .get::<Array<'_>>(BORDER)
                .and_then(|b| b.iter::<f32>().nth(2))
        })
        .filter(|w| w.is_finite())
        .unwrap_or(1.0)
}

/// The dash array of a dashed border style (/BS /S /D), else the fourth /Border
/// element; at most ten entries, as in `PDFium`.
fn dash(annot: &Dict<'_>) -> Vec<f32> {
    let bs = annot.get::<Dict<'_>>(BS);
    let arr = match &bs {
        Some(bs) if bs.get::<Name<'_>>(S).is_some_and(|s| s.as_ref() == b"D") => {
            bs.get::<Array<'_>>(D)
        }
        _ => annot
            .get::<Array<'_>>(BORDER)
            .and_then(|b| match b.iter::<Object<'_>>().nth(3)? {
                Object::Array(a) => Some(a),
                _ => None,
            }),
    };
    let dash: Vec<f32> = arr
        .map(|a| a.iter::<f32>().take(10).collect())
        .unwrap_or_default();
    // An all-zero or negative pattern draws nothing; draw solid instead.
    if dash.iter().all(|d| d.is_finite() && *d >= 0.0) && dash.iter().any(|d| *d > 0.0) {
        dash
    } else {
        Vec::new()
    }
}

fn rect(annot: &Dict<'_>) -> Option<kurbo::Rect> {
    let r = annot.get::<Rect>(RECT)?.to_kurbo().abs();
    [r.x0, r.y0, r.x1, r.y1]
        .iter()
        .all(|n| n.is_finite())
        .then_some(r)
}

/// Square and Circle: /IC fills the shape, /C strokes it at the border width, inside
/// `/Rect`. Unlike `PDFium`, an absent `/C` means no border: the spec gives it no default.
fn shape(annot: &Dict<'_>, circle: bool, out: &mut Out) -> Option<()> {
    let fill = annot.get::<Array<'_>>(IC).and_then(|a| color(&a, false));
    let width = border_width(annot);
    let stroke = if width > 0.0 {
        annot.get::<Array<'_>>(C).and_then(|a| color(&a, true))
    } else {
        None
    };
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
fn quads(annot: &Dict<'_>) -> Vec<[f32; 4]> {
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
fn highlight(annot: &Dict<'_>, out: &mut Out) -> Option<()> {
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

enum Line {
    Under,
    Through,
    Squiggle,
}

/// `Underline`, `StrikeOut` and `Squiggly`: a 1 pt line per quad in /C (default black): one
/// unit above the bottom, through the middle, or a zigzag 2 pt high and 2 pt per step.
fn text_lines(annot: &Dict<'_>, line: Line, out: &mut Out) -> Option<()> {
    let stroke = c_color(annot, true, "0 G")?;
    let quads = quads(annot);
    if quads.is_empty() {
        return None;
    }
    out.op(&stroke);
    out.stroke_state(1.0, &[])?;
    let steps = match line {
        Line::Squiggle => squiggle_steps(&quads),
        _ => Vec::new(),
    };
    for (i, [x0, y0, x1, y1]) in quads.into_iter().enumerate() {
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
            Line::Squiggle => squiggle(x0, x1, y0, steps[i], out)?,
        }
        out.op("S");
    }
    Some(())
}

/// Zigzag steps one `Squiggly` appearance has in all (a page of 50 squiggled lines of
/// 500 pt has 12 500), so its content stays within about a megabyte plus a few lines
/// per quad; `squiggle_steps` shares them out.
const SQUIGGLE_STEPS: u32 = 1 << 16;

/// The zigzag's height, and the length of one step when nothing is shared out.
const DELTA: f64 = 2.0;

/// The steps a quad `width` wide has at 2 pt each, at least one and at most
/// `SQUIGGLE_STEPS`.
fn natural_steps(width: f64) -> u32 {
    (width / DELTA).ceil().clamp(1.0, f64::from(SQUIGGLE_STEPS)) as u32
}

/// The steps each quad's zigzag gets: its 2 pt steps when they fit `SQUIGGLE_STEPS`
/// together; otherwise every quad keeps its own up to one share, the largest that
/// fits, so narrow quads stay as they are and only the widest get longer steps. Each
/// quad gets at least one step, across its whole width.
fn squiggle_steps(quads: &[[f32; 4]]) -> Vec<u32> {
    let natural: Vec<u32> = quads
        .iter()
        .map(|&[x0, _, x1, _]| natural_steps(f64::from(x1) - f64::from(x0)))
        .collect();
    let mut sorted = natural.clone();
    sorted.sort_unstable();
    let (mut left, mut rest) = (u64::from(SQUIGGLE_STEPS), sorted.len() as u64);
    for n in sorted {
        let n = u64::from(n);
        if n * rest > left {
            let share = (left / rest).max(1) as u32;
            return natural.into_iter().map(|n| n.min(share)).collect();
        }
        left -= n;
        rest -= 1;
    }
    natural
}

/// The zigzag of a `Squiggly` quad from `x0` to `x1` above `y`: 2 pt high, one step per
/// 2 pt as in `PDFium`, but at most `max_steps` steps (and at least one), so a wider
/// quad gets longer steps. Positions are computed from the step index in f64: in f32,
/// `x += 2` stops advancing at 2^25.
fn squiggle(x0: f32, x1: f32, y: f32, max_steps: u32, out: &mut Out) -> Option<()> {
    let max_steps = f64::from(max_steps.max(1));
    let (bottom, top) = (y, y + DELTA as f32);
    let (x0, x1) = (f64::from(x0), f64::from(x1));
    let width = x1 - x0;
    let steps = (width / DELTA).ceil().max(1.0);
    let (steps, step) = if steps > max_steps {
        (max_steps, width / max_steps)
    } else {
        (steps, DELTA)
    };
    // Every step but the last ends on a full peak or trough.
    let full = steps as u32 - 1;
    out.nums(&[x0 as f32, top], "m")?;
    for i in 1..=full {
        let x = x0 + f64::from(i) * step;
        out.nums(&[x as f32, if i % 2 == 0 { top } else { bottom }], "l")?;
    }
    // The last, possibly partial, step keeps the slope.
    let rise = ((x1 - (x0 + f64::from(full) * step)) / step * DELTA) as f32;
    let up = full % 2 == 1;
    out.nums(
        &[x1 as f32, if up { bottom + rise } else { top - rise }],
        "l",
    )
}

/// `Ink`: each `/InkList` path stroked as a polyline in /C (default black) at the border
/// width, with the border dash.
fn ink(annot: &Dict<'_>, out: &mut Out) -> Option<()> {
    let width = border_width(annot);
    if width <= 0.0 {
        return None;
    }
    let stroke = c_color(annot, true, "0 G")?;
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

/// `Text` (a note): `PDFium`'s 20 pt note icon at the bottom left of /Rect, a yellow
/// speech box with three black lines.
fn note_icon(annot: &Dict<'_>, out: &mut Out) -> Option<()> {
    const SIZE: f32 = 20.0;
    const HALF: f32 = 0.5;
    const TIP: f32 = 4.0;
    let r = rect(annot)?;
    let (left, bottom) = (r.x0 as f32 + HALF, r.y0 as f32 + HALF);
    let (right, top) = (r.x0 as f32 + SIZE - HALF, r.y0 as f32 + SIZE - HALF);
    let box_bottom = bottom + TIP;
    let (tip_left, tip_right) = (left + TIP, left + 2.0 * TIP);
    out.op("1 1 0 rg 0 G");
    out.stroke_state(1.0, &[])?;
    out.nums(&[left, box_bottom], "m")?;
    out.nums(&[left, top], "l")?;
    out.nums(&[right, top], "l")?;
    out.nums(&[right, box_bottom], "l")?;
    out.nums(&[tip_right, box_bottom], "l")?;
    out.nums(&[(tip_left + tip_right) / 2.0, box_bottom - TIP], "l")?;
    out.nums(&[tip_left, box_bottom], "l")?;
    out.nums(&[left, box_bottom], "l")?;
    let step = (top - box_bottom) / 4.0;
    for i in 1..=3 {
        let y = top - step * i as f32;
        out.nums(&[left + 2.0, y], "m")?;
        out.nums(&[right - 2.0, y], "l")?;
    }
    out.op("B*");
    Some(())
}

#[cfg(test)]
mod tests {
    use super::generate;
    use hayro_syntax::object::{Dict, FromBytes};

    fn content(annot: &str) -> String {
        let dict = Dict::from_bytes(annot.as_bytes()).expect("valid dict");
        generate(&dict).expect("an appearance").content
    }

    fn squiggly(quad: &str) -> String {
        content(&format!(
            "<< /Subtype /Squiggly /Rect [0 0 1 1] /QuadPoints [{quad}] >>"
        ))
    }

    #[test]
    fn squiggly_follows_pdfium_steps() {
        let c = squiggly("10 20 15 20 10 10 15 10");
        assert!(
            c.ends_with("10 12 m\n12 10 l\n14 12 l\n15 11 l\nS\n"),
            "{c}"
        );
    }

    #[test]
    fn squiggly_ends_at_large_coordinates() {
        // In f32, 2^25 + 2 == 2^25: a loop adding the step never reached x1.
        let c = squiggly("33554432 20 33554440 20 33554432 10 33554440 10");
        assert_eq!(c.matches(" l\n").count(), 4, "{c}");
        assert!(c.ends_with(" l\nS\n"), "{c}");
    }

    /// The steps of each quad drawn in `c`.
    fn steps(c: &str) -> Vec<usize> {
        c.split("S\n")
            .filter(|q| q.contains(" m\n"))
            .map(|q| q.matches(" l\n").count())
            .collect()
    }

    /// A quad 10^30 pt wide takes the steps the appearance has left; the 40 pt quad
    /// beside it keeps its 2 pt steps.
    #[test]
    fn a_huge_quad_gets_longer_steps_and_leaves_the_others() {
        let big = "1000000000000000000000000000000";
        let c = squiggly(&format!(
            "0 20 {big} 20 0 10 {big} 10 0 50 40 50 0 40 40 40"
        ));
        assert_eq!(steps(&c), [65516, 20]);
        let quads: Vec<&str> = c.split("S\n").collect();
        assert!(
            quads[0].ends_with(&format!("{big} 12 l\n")),
            "{}",
            &quads[0][..80]
        );
    }

    /// The review's case at a fiftieth of its size: 20 000 quads 2048 pt wide asked
    /// for 1024 steps each, 20 million lines. They share the appearance's steps
    /// evenly, each still across its whole width.
    #[test]
    fn squiggly_steps_are_shared_per_appearance() {
        let c = squiggly(&"0 20 2048 20 0 10 2048 10 ".repeat(20_000));
        let drawn = steps(&c);
        assert_eq!(drawn.len(), 20_000);
        assert!(drawn.iter().all(|&n| n == 3), "{:?}", &drawn[..4]);
        assert!(
            c.ends_with(" 12 l\n2048 10 l\nS\n"),
            "{}",
            &c[c.len() - 60..]
        );
        assert!(c.len() < 2 << 20, "{} bytes", c.len());
        // A quad narrower than the share keeps its 2 pt steps beside wide ones.
        let mut quads = vec![[0.0, 0.0, 2048.0, 1.0]; 1000];
        quads.push([0.0, 0.0, 40.0, 1.0]);
        let shared = super::squiggle_steps(&quads);
        assert_eq!(shared[1000], 20);
        assert!(
            shared[..1000].iter().all(|&n| n == 65),
            "{:?}",
            &shared[..2]
        );
    }

    /// Steps that fit the appearance are each quad's 2 pt steps, so ordinary
    /// appearances are drawn as before.
    #[test]
    fn squiggly_steps_that_fit_are_kept() {
        assert_eq!(super::squiggle_steps(&[[0.0, 0.0, 5.0, 1.0]; 3]), [3; 3]);
        let quads = [[0.0, 0.0, 4096.0, 1.0], [0.0, 0.0, 2.0, 1.0]];
        assert_eq!(super::squiggle_steps(&quads), [2048, 1]);
    }
}
