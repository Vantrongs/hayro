//! `FreeText` (PDF 32000-1 12.5.6.6): text in a box, with a callout line.
//!
//! The box is `/Rect` less `/RD`, filled with /C and bordered in the text colour at
//! the border width. The text is `/Contents` in the default appearance (`/DA`:
//! family, size, colour; 12 pt black Helvetica without one), or, with rich text
//! (`/RC`) or a default style (`/DS`), the text of `/RC`, each part in the style its
//! elements set over the style `/DS` sets over Times in the `/DA` size and colour.
//! It is wrapped to the box less twice the border width on each side and aligned
//! by `/Q`; each line is 1.2 times its largest size high, its baseline 0.8 times
//! that size (0.9 for rich text) below its top, the first at that inner top. The
//! text is turned with the box by `/Rotate` and clipped to `/Rect`. A callout line
//! is drawn for a `/CL` of two or three points. The layout and the defaults match
//! what common viewers draw for such an annotation.

use super::entries::Entries;
use super::lines::{Ending, Paint, ending};
use super::text::{self, Family, Fonts, Span, Style};
use super::{Out, Unsupported, border_width, color_of, dash, rect};
use hayro_syntax::object::dict::keys::C;
use kurbo::{Affine, Point};

pub(super) fn free_text(
    annot: &Entries<'_>,
    form_fonts: Option<&Entries<'_>>,
    fonts: &Fonts,
    out: &mut Out,
) -> Result<Option<()>, Unsupported> {
    let Some(r) = rect(annot) else {
        return Ok(None);
    };
    let inner = match annot.numbers(b"RD", 4).as_deref() {
        Some(&[l, b, rt, t, ..])
            if [l, b, rt, t].iter().all(|v| *v >= 0.0)
                && l + rt < r.width()
                && b + t < r.height() =>
        {
            kurbo::Rect::new(r.x0 + l, r.y0 + b, r.x1 - rt, r.y1 - t)
        }
        _ => r,
    };
    let da = annot.text(b"DA").unwrap_or_default();
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
    // Rich text may be a stream.
    let rich = annot.text(b"RC");
    let ds = annot.text(b"DS");
    let (spans, ascent) = if rich.is_some() || ds.is_some() {
        style.family = Family::Serif;
        if let Some(ds) = &ds {
            style = text::css_style(&text::text_string(ds), style);
        }
        let spans = match &rich {
            Some(rc) => text::rich_text(&text::text_string(rc), &style),
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
    let callout = annot.is(b"IT", b"FreeTextCallout");
    let cl: Vec<Point> = annot
        .numbers(b"CL", 0)
        .filter(|n| n.len() == 4 || n.len() == 6)
        .unwrap_or_default()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[x, y]| Point::new(*x, *y))
        .collect();

    out.op("q");
    if let Some(fill) = color_of(annot, C, false) {
        out.op(&fill);
        out.nums(&[inner.x0, inner.y0, inner.width(), inner.height()], "re")
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
        out.nums(&[cl[0].x, cl[0].y], "m")
            .ok_or(Unsupported::Kind)?;
        for p in &cl[1..] {
            out.nums(&[p.x, p.y], "l").ok_or(Unsupported::Kind)?;
        }
        out.op("S");
        ending(Ending::of(annot)[0], cl[0], cl[1], &paint, out).ok_or(Unsupported::Kind)?;
    }
    if w > 0.0 {
        out.op(&border_color);
        out.stroke_state(w, &dash(annot)).ok_or(Unsupported::Kind)?;
        let b = inner.inset(-w / 2.0);
        out.nums(&[b.x0, b.y0, b.width(), b.height()], "re")
            .ok_or(Unsupported::Kind)?;
        out.op("S");
    }

    // The text, in a frame turned with the box: `(u, v)` from its bottom left.
    let rotate = annot.int(b"Rotate").unwrap_or(0).rem_euclid(360);
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
    let pad = 2.0 * w as f32;
    let avail = (fw as f32 - 2.0 * pad).max(0.0);
    let lines = text::layout(fonts, &spans, Some(avail))?;
    if lines.iter().any(|l| !l.glyphs.is_empty()) {
        out.clip(r).ok_or(Unsupported::Kind)?;
        let q = annot.int(b"Q").unwrap_or(0);
        let mut top = fh as f32 - pad;
        let mut color = None;
        for line in &lines {
            let width = line.width();
            let mut x = match q {
                1 => pad + (avail - width) / 2.0,
                2 => fw as f32 - pad - width,
                _ => pad,
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
                text::draw(&glyphs, run[0].size, (x, y), frame, out).ok_or(Unsupported::Kind)?;
                x += text::width(&glyphs, run[0].size);
            }
            top -= 1.2 * line.size;
        }
    }
    out.op("Q");
    Ok(Some(()))
}

/// `/Contents` as text.
fn contents(annot: &Entries<'_>) -> String {
    annot
        .text(b"Contents")
        .map(|s| text::text_string(&s))
        .unwrap_or_default()
}
