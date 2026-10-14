//! `FreeText` (PDF 32000-1 12.5.6.6): text in a box, with a callout line.
//!
//! The box is `/Rect` less `/RD`, filled with /C and bordered in the text colour at
//! the border width. The text is `/Contents` in the default appearance (`/DA`:
//! family, size, colour; 12 pt black Helvetica without one) aligned by `/Q`, or,
//! with rich text (`/RC`) or a default style (`/DS`), the text of `/RC` (or
//! `/Contents`), each part in the style its elements set over the style `/DS` sets
//! over the default face of rich text in the `/DA` size and colour, aligned by its
//! `text-align` (see [`super::rich`]). It is wrapped to the box less twice the
//! border width on each side; each line is 1.2 times its largest size high, its
//! baseline 0.8 times that size (0.9 for rich text) below its top, the first at
//! that inner top. The text is turned with the box by `/Rotate` and clipped to
//! `/Rect`. A callout line is drawn for a `/CL` of two or three points. The layout
//! and the defaults match what common viewers draw for such an annotation.

use super::lines::{Ending, Paint, ending};
use super::rich;
use super::text::{self, Align, Family, Fonts, Span, Style};
use super::{Out, Unsupported, border_width, color, dash, numbers_of, rect};
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
    let inner = match numbers_of(annot, b"RD", 4).as_deref() {
        Some(&[l, b, rt, t, ..])
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
        face: family.into(),
        size: size.unwrap_or(12.0),
        color: da_color.unwrap_or_else(|| "0 g".to_owned()),
        align: match annot.get::<i64>(b"Q").unwrap_or(0) {
            1 => Align::Center,
            2 => Align::Right,
            _ => Align::Left,
        },
    };
    let border_color = style
        .color
        .replace("rg", "RG")
        .replace(" g", " G")
        .replace(" k", " K");
    let rich = string(annot, b"RC");
    let ds = string(annot, b"DS");
    let (spans, ascent) = if rich.is_some() || ds.is_some() {
        // Rich text is aligned by its own `text-align`, not by /Q.
        style.face = Family::Text.into();
        style.align = Align::Left;
        if let Some(ds) = &ds {
            style = rich::css_style(&text::text_string(ds), style)?;
        }
        let spans = match &rich {
            Some(rc) => rich::rich_text(&text::text_string(rc), &style)?,
            None => vec![Span {
                text: contents(annot),
                style,
            }],
        };
        (spans, 0.9)
    } else {
        (
            vec![Span {
                text: contents(annot),
                style,
            }],
            0.8,
        )
    };
    let w = border_width(annot);

    // The callout line, from the point it points at to the box.
    let callout = annot
        .get::<Name<'_>>(b"IT")
        .is_some_and(|it| it.as_ref() == b"FreeTextCallout");
    let cl: Vec<Point> = numbers_of(annot, b"CL", 0)
        .filter(|n| n.len() == 4 || n.len() == 6)
        .unwrap_or_default()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[x, y]| Point::new(*x, *y))
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
    let lines = text::layout(fonts, &spans, Some(avail))?;
    if lines.iter().any(|l| !l.glyphs.is_empty()) {
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
        let mut top = fh as f32 - pad;
        let mut color = None;
        for line in &lines {
            let width = line.width();
            let (mut x, spacing) = match line.align {
                Align::Left => (pad, 0.0),
                Align::Center => (pad + (avail - width) / 2.0, 0.0),
                Align::Right => (fw as f32 - pad - width, 0.0),
                Align::Justify => match line.spaces() {
                    0 => (pad, 0.0),
                    n => (pad, ((avail - width) / n as f32).max(0.0)),
                },
            };
            let y = top - ascent * line.size;
            // Each run of glyphs of one span in its colour and size.
            for run in line.glyphs.chunk_by(|a, b| a.span == b.span) {
                let span_color = &spans[run[0].span].style.color;
                if color != Some(span_color) {
                    out.op(span_color);
                    color = Some(span_color);
                }
                let glyphs: Vec<text::Glyph<'_>> = run.iter().map(|g| g.glyph.clone()).collect();
                text::draw_spaced(&glyphs, run[0].size, (x, y), spacing, frame, out)
                    .ok_or(Unsupported::Kind)?;
                let spaces = run.iter().filter(|g| g.glyph.is_space()).count();
                x += text::width(&glyphs, run[0].size) + spacing * spaces as f32;
            }
            top -= 1.2 * line.size;
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
