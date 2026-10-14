//! `FreeText` (PDF 32000-1 12.5.6.6): text in a box, with a callout line.
//!
//! The box is `/Rect` less `/RD`, filled with /C and bordered in the text colour at
//! the border width. The text is `/Contents` in the default appearance (`/DA`:
//! family, size, colour; 12 pt black Helvetica without one), or, with rich text
//! (`/RC`) or a default style (`/DS`), the text of `/RC` in the style `/DS` and
//! the rich text's first style set, over Times in the `/DA` size and colour. It is
//! wrapped to the box less twice the border width on each side and aligned by `/Q`,
//! its first line 0.8 em (0.9 for rich text) below that inner top, lines 1.2 em
//! apart, turned with the box by `/Rotate`, and clipped to `/Rect`. The layout and
//! the defaults match what common viewers draw for such an annotation.

use super::lines::{Ending, Paint, ending};
use super::text::{self, Family, Fonts, Style};
use super::{Out, Unsupported, border_width, color, dash, rect};
use hayro_syntax::object::dict::keys::C;
use hayro_syntax::object::{Array, Dict, Name, Object};
use kurbo::{Affine, Point};

pub(super) fn free_text(
    annot: &Dict<'_>,
    form_fonts: Option<&Dict<'_>>,
    fonts: &Fonts,
    out: &mut Out,
) -> Result<Option<()>, Unsupported> {
    let Some(r) = rect(annot) else {
        return Ok(None);
    };
    let inner = match annot.get::<[f64; 4]>(b"RD") {
        Some([l, b, rt, t])
            if [l, b, rt, t].iter().all(|v| v.is_finite() && *v >= 0.0)
                && l + rt < r.width()
                && b + t < r.height() =>
        {
            kurbo::Rect::new(r.x0 + l, r.y0 + b, r.x1 - rt, r.y1 - t)
        }
        _ => r,
    };
    let da = string(annot, b"DA").unwrap_or_default();
    let (family, size, da_color) = text::default_appearance(&da, form_fonts);
    let mut style = Style {
        family,
        size: size.unwrap_or(12.0),
        color: da_color.unwrap_or_else(|| "0 g".to_owned()),
    };
    let border_color = style
        .color
        .replace("rg", "RG")
        .replace(" g", " G")
        .replace(" k", " K");
    let rich = string(annot, b"RC");
    let ds = string(annot, b"DS");
    let (content, ascent) = if rich.is_some() || ds.is_some() {
        style.family = Family::Serif;
        if let Some(ds) = &ds {
            style = text::css_style(&text::text_string(ds), style);
        }
        let content = match &rich {
            Some(rc) => {
                let (content, css) = text::rich_text(&text::text_string(rc));
                if let Some(css) = css {
                    style = text::css_style(&css, style);
                }
                content
            }
            None => contents(annot),
        };
        (content, 0.9)
    } else {
        (contents(annot), 0.8)
    };
    let w = border_width(annot);

    // The callout line, from the point it points at to the box.
    let callout = annot
        .get::<Name<'_>>(b"IT")
        .is_some_and(|it| it.as_ref() == b"FreeTextCallout");
    let cl: Vec<Point> = annot
        .get::<Array<'_>>(b"CL")
        .map(|a| a.iter::<f64>().collect::<Vec<_>>())
        .unwrap_or_default()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[x, y]| Point::new(*x, *y))
        .filter(|p| p.x.is_finite() && p.y.is_finite())
        .collect();

    out.op("q");
    if let Some(fill) = annot.get::<Array<'_>>(C).and_then(|a| color(&a, false)) {
        out.op(&fill);
        out.nums(
            &[
                inner.x0 as f32,
                inner.y0 as f32,
                inner.width() as f32,
                inner.height() as f32,
            ],
            "re",
        )
        .ok_or(Unsupported::Kind)?;
        out.op("f");
    }
    if callout && (cl.len() == 2 || cl.len() == 3) && w > 0.0 {
        let paint = Paint {
            stroke: border_color.clone(),
            width: w,
            dash: Vec::new(),
            fill: None,
        };
        out.op(&paint.stroke);
        out.stroke_state(w, &[]).ok_or(Unsupported::Kind)?;
        out.nums(&[cl[0].x as f32, cl[0].y as f32], "m")
            .ok_or(Unsupported::Kind)?;
        for p in &cl[1..] {
            out.nums(&[p.x as f32, p.y as f32], "l")
                .ok_or(Unsupported::Kind)?;
        }
        out.op("S");
        ending(Ending::of(annot)[0], cl[0], cl[1], &paint, out).ok_or(Unsupported::Kind)?;
    }
    if w > 0.0 {
        out.op(&border_color);
        out.stroke_state(w, &dash(annot)).ok_or(Unsupported::Kind)?;
        let b = inner.inset(-f64::from(w) / 2.0);
        out.nums(
            &[
                b.x0 as f32,
                b.y0 as f32,
                b.width() as f32,
                b.height() as f32,
            ],
            "re",
        )
        .ok_or(Unsupported::Kind)?;
        out.op("S");
    }

    // The text, in a frame turned with the box: `(u, v)` from its bottom left.
    let rotate = annot.get::<i64>(b"Rotate").unwrap_or(0).rem_euclid(360);
    let (fw, fh, frame) = match rotate {
        90 => (
            inner.height(),
            inner.width(),
            Affine::new([0.0, 1.0, -1.0, 0.0, inner.x1, inner.y0]),
        ),
        180 => (
            inner.width(),
            inner.height(),
            Affine::new([-1.0, 0.0, 0.0, -1.0, inner.x1, inner.y1]),
        ),
        270 => (
            inner.height(),
            inner.width(),
            Affine::new([0.0, -1.0, 1.0, 0.0, inner.x0, inner.y1]),
        ),
        _ => (
            inner.width(),
            inner.height(),
            Affine::translate((inner.x0, inner.y0)),
        ),
    };
    let pad = 2.0 * w;
    let avail = (fw as f32 - 2.0 * pad).max(0.0);
    let lines = text::lines(fonts, style.family, style.size, &content, Some(avail))?;
    if lines.iter().any(|l| !l.is_empty()) {
        out.nums(
            &[
                r.x0 as f32,
                r.y0 as f32,
                r.width() as f32,
                r.height() as f32,
            ],
            "re",
        )
        .ok_or(Unsupported::Kind)?;
        out.op("W n");
        out.op(&style.color);
        let q = annot.get::<i64>(b"Q").unwrap_or(0);
        let mut y = fh as f32 - pad - ascent * style.size;
        for line in &lines {
            let width = text::width(line, style.size);
            let x = match q {
                1 => pad + (avail - width) / 2.0,
                2 => fw as f32 - pad - width,
                _ => pad,
            };
            text::draw(line, style.size, (x, y), frame, out).ok_or(Unsupported::Kind)?;
            y -= 1.2 * style.size;
        }
    }
    out.op("Q");
    Ok(Some(()))
}

/// The bytes of string `key`.
fn string(annot: &Dict<'_>, key: &[u8]) -> Option<Vec<u8>> {
    match annot.get::<Object<'_>>(key)? {
        Object::String(s) => Some(s.as_bytes().to_vec()),
        // Rich text may be a stream.
        Object::Stream(s) => s.decoded().ok().map(|d| d.into_owned()),
        _ => None,
    }
}

/// `/Contents` as text.
fn contents(annot: &Dict<'_>) -> String {
    string(annot, b"Contents")
        .map(|s| text::text_string(&s))
        .unwrap_or_default()
}
