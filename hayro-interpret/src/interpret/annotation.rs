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
            Line::Squiggle => {
                const DELTA: f32 = 2.0;
                let (bottom, top) = (y0, y0 + DELTA);
                out.nums(&[x0, top], "m")?;
                let mut x = x0 + DELTA;
                let mut up = false;
                while x < x1 {
                    out.nums(&[x, if up { top } else { bottom }], "l")?;
                    x += DELTA;
                    up = !up;
                }
                // The last partial step keeps the slope.
                let rest = x1 - (x - DELTA);
                out.nums(&[x1, if up { bottom + rest } else { top - rest }], "l")?;
            }
        }
        out.op("S");
    }
    Some(())
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
