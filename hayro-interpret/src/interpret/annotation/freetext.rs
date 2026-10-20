//! `FreeText` (PDF 32000-1 12.5.6.6): text in a box, with a callout line.
//!
//! The box is `/Rect` less `/RD`, filled with /C and bordered in the text colour at
//! the border width. The text is `/Contents` in the default appearance (`/DA`:
//! family, size, colour; 12 pt black Helvetica without one) aligned by `/Q`, or,
//! with rich text (`/RC`) or a default style (`/DS`), the text of `/RC` (or
//! `/Contents`), each part in the style its elements set over the style `/DS` sets
//! over the default face of rich text in the `/DA` size and black, aligned by its
//! `text-align` (see [`super::rich`]). It is wrapped to the box less twice the
//! border width on each side; each line is 1.2 times its largest size high, its
//! baseline 0.8 times that size (0.9 for rich text) below its top, the first at
//! that inner top. The text is turned with the box by `/Rotate` and clipped to
//! `/Rect`. A callout line is drawn for a `/CL` of two or three points. The layout
//! and the defaults match what common viewers draw for such an annotation.

use super::entries::{Entries, Text};
use super::lines::{Ending, Paint, ending, lost_part};
use super::out::{Color, Out, Part};
use super::rich;
use super::text::{self, Align, Family, Setting, Size, Span, Style, TextBox};
use super::{
    Appearances, Drew, FREE_TEXT, TEXT, TextKey, Unsupported, border_width, color_of, dash, rect,
};
use hayro_syntax::object::dict::keys::C;
use kurbo::{Affine, Point, Shape};

/// The appearance of `FreeText` `annot`, of which the page shows `visible`; text the
/// page does not show is not drawn, and text many annotations share is laid out
/// once for `doc`'s document.
pub(super) fn free_text<'a>(
    annot: &Entries<'a>,
    doc: &Appearances<'a>,
    visible: kurbo::Rect,
    out: &mut Out,
) -> Drew {
    // Without a box, viewers draw over coordinates of ±2^32.
    let Some(Boxes {
        r,
        rd,
        inner,
        w,
        callout,
        cl,
        clip,
    }) = boxes(annot)
    else {
        out.refuse(Unsupported::Coordinates, Part::Anywhere)?;
        return Ok(None);
    };
    let (family, size, da_color) = doc.default_appearance(Some(annot.source(b"DA")), || {
        annot.text(Text::DefaultAppearance)
    });
    let color = da_color.unwrap_or(Color::BLACK);
    let align = match annot.int(b"Q").unwrap_or(0) {
        1 => Align::Center,
        2 => Align::Right,
        _ => Align::Left,
    };
    let border_color = color;
    // The text, set once for the document whatever size, colour and alignment each
    // annotation draws it in, under the entry it is read from and the style it is
    // set in: rich text over its default style; else the contents in the default
    // style; else the contents in the default appearance's family. Rich text's
    // relative sizes are read as the default appearance's sign allows.
    let positive = size.is_some_and(|s| s > 0.0);
    let rich = annot.has_text(Text::Rich);
    let text = if rich || annot.has_text(Text::DefaultStyle) {
        doc.default_style(annot, positive).and_then(|style| {
            let read = if rich { Text::Rich } else { Text::Contents };
            let key = TextKey::new(
                [annot.source(read.key())],
                Family::Text.into(),
                vec![FREE_TEXT, u64::from(positive)],
            )
            .styled(&style);
            doc.shaped(key, || {
                let rc = rich.then(|| annot.text(Text::Rich)).flatten();
                let spans = rich_spans(rc.as_deref(), &style, positive, || Ok(contents(annot)))?;
                doc.fonts.shape_rich(&spans)
            })
        })
    } else {
        let source = annot.source(Text::Contents.key());
        doc.plain(source, family.into(), TEXT, || Ok(contents(annot)))
    };
    // Of a callout whose ending has no direction, viewers draw the ending
    // otherwise ([`lost_part`], in the box all it draws is clipped to); the rest as given (observed).
    let lost = callout
        && (cl.len() == 2 || cl.len() == 3)
        && Ending::of_callout(annot).lost_at(cl[0], cl[1]);
    if lost {
        out.refuse(Unsupported::Ending, lost_part(cl[0], w, Some(clip)))?;
    }
    out.op("q");
    out.writes(|out| {
        out.clip(clip)?;
        if let Some(fill) = color_of(annot, C) {
            out.fill_color(&fill)?;
            out.nums(&[inner.x0, inner.y0, inner.width(), inner.height()], "re")?;
            out.op("f");
        }
        // The callout is drawn at the border width as given, however thin or
        // negative.
        if callout && (cl.len() == 2 || cl.len() == 3) {
            let paint = Paint {
                stroke: Some(border_color),
                width: w,
                dash: Vec::new(),
                fill: None,
            };
            out.stroke_color(&border_color)?;
            out.stroke_state(w, &[])?;
            out.nums(&[cl[0].x, cl[0].y], "m")?;
            for p in &cl[1..] {
                out.nums(&[p.x, p.y], "l")?;
            }
            out.op("S");
            if !lost {
                ending(Ending::of_callout(annot), cl[0], cl[1], &paint, out)?;
            }
        }
        if w > 0.0 {
            out.stroke_color(&border_color)?;
            out.stroke_state(w, &dash(annot))?;
            let b = within(inner, w / 2.0);
            out.nums(&[b.x0, b.y0, b.width(), b.height()], "re")?;
            out.op("S");
        }
        Some(())
    })?;

    // The text, in a frame turned with the box: `(u, v)` from its bottom left. As
    // viewers lay it out, a turn of 90, 180 or 270 degrees, as given, turns the
    // frame into the box; any other turns the unturned frame about the box's
    // bottom left corner.
    let rotate = annot.int(b"Rotate").unwrap_or(0);
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
            Affine::translate((inner.x0, inner.y0))
                * Affine::rotate(f64::from(rotate).to_radians()),
        ),
    };
    // The text is clipped to the box less the border width.
    let mut clip = within(inner, w).abs();
    // Text that cannot be set as viewers set it would be drawn within that box
    // (and the box all is clipped to, set above); viewers draw the rest as given
    // (observed).
    let Some(text) = out.part(text, || Part::rect(clip))? else {
        out.op("Q");
        return Ok(Some(()));
    };
    // Plain text is drawn at the default appearance's size, 12 pt for a size fit to
    // the box; rich text's base size is the default appearance's, and a size fit to
    // the box is none: it is drawn 0 high, as viewers draw it.
    let set = Setting {
        base: if text.rich {
            size.unwrap_or(0.0)
        } else {
            size.unwrap_or(12.0)
        },
        color,
        align,
    };
    // Rich text is laid out in a box of its own, sized by the default appearance's.
    let rich_size = text.rich.then(|| size.unwrap_or(0.0));
    let ascent = if text.rich { RICH_ASCENT } else { 0.8 };

    let pad = 2.0 * w as f32;
    if let Some(size) = rich_size {
        // Rich text, also to its box in the frame: from the padding on the left to
        // twice the padding on the right, and from the padding below the top to
        // twice the size below the bottom (observed).
        let p = f64::from(pad);
        let layout = kurbo::Rect::new(p, -2.0 * f64::from(size), fw - 2.0 * p, fh - p).abs();
        let layout = frame * layout.to_path(0.1);
        out.writes(|out| out.path(&layout))?;
        out.op("W n");
        clip = clip.intersect(layout.bounding_box());
    }
    // Rich text is laid out to twice the padding from the right (observed).
    // Plain text's room is the box's side, each end moved in by its `/RD` entry,
    // less four border widths, in 32-bit numbers, not held at 0 (observed: to the
    // bit at 120 boundaries with `/RD` and border widths from 0 to 3).
    let avail = if rich_size.is_some() {
        (fw as f32 - pad - 2.0 * pad).max(0.0)
    } else {
        let side = |hi: f64, rd_hi: f64, lo: f64, rd_lo: f64| {
            (hi as f32 - rd_hi as f32) - (lo as f32 + rd_lo as f32)
        };
        let side = if matches!(rotate, 90 | 270) {
            side(r.y1, rd[3], r.y0, rd[1])
        } else {
            side(r.x1, rd[2], r.x0, rd[0])
        };
        side - 2.0 * pad
    };
    let area = TextBox {
        rich: text.rich,
        pad,
        top: fh as f32 - pad,
        room: f32::INFINITY,
        frame,
        clip,
    };
    let bound = text.bound(set.base);
    text::draw_box(
        &doc.fonts,
        std::iter::once(Ok(text)),
        bound,
        &set,
        avail,
        ascent,
        &area,
        visible,
        out,
    )?;
    out.op("Q");
    Ok(Some(()))
}

/// Where a `FreeText` is drawn ([`free_text`]).
pub(super) struct Boxes {
    /// `/Rect`.
    r: kurbo::Rect,
    /// `/RD`, four numbers.
    rd: Vec<f64>,
    /// The box the text is laid out in.
    inner: kurbo::Rect,
    /// The border width.
    w: f64,
    /// Whether it has a callout line.
    callout: bool,
    /// The callout line's points.
    cl: Vec<Point>,
    /// The box all it draws is clipped to.
    pub(super) clip: kurbo::Rect,
}

/// Where `FreeText` `annot` is drawn; `None` without a `/Rect`.
pub(super) fn boxes(annot: &Entries<'_>) -> Option<Boxes> {
    let r = rect(annot)?;
    // The box: `/Rect` with each side moved in by its `/RD` entry (left, bottom,
    // right, top; 0 for one it lacks or that is not a number), as given, negative
    // or not; one this turns inside out is laid out as it is, its width or height
    // negative, as viewers lay it out.
    let rd = annot.numbers(b"RD", 4).unwrap_or_else(|| vec![0.0; 4]);
    let inner = kurbo::Rect::new(r.x0 + rd[0], r.y0 + rd[1], r.x1 - rd[2], r.y1 - rd[3]);
    let w = border_width(annot);

    // The callout line, from the point it points at to the box.
    let callout = annot.is(b"IT", b"FreeTextCallout");
    // Of more than six, none are read beyond the seventh.
    let cl: Vec<Point> = annot
        .items(b"CL")
        .map(|items| items.first(7))
        .filter(|n| n.len() == 4 || n.len() == 6)
        .unwrap_or_default()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[x, y]| Point::new(*x, *y))
        .collect();

    // Viewers clip the appearance to the box, or, with a callout, to the box, the
    // callout's points and its ending, as [`Ending::clip_box`] says.
    let drawn = cl.iter().fold(inner.abs(), |b, p| b.union_pt(*p));
    let clip = match cl.as_slice() {
        [tip, from, ..] if callout => {
            Ending::of_callout(annot).clip_box(*tip, *from, w, 0.0, drawn)
        }
        _ => inner.abs(),
    };
    Some(Boxes {
        r,
        rd,
        inner,
        w,
        callout,
        cl,
        clip,
    })
}

/// The baseline of the first line of rich text below the top of its box, in
/// sizes of the line.
pub(super) const RICH_ASCENT: f32 = 0.9;

/// The style default style `ds` sets over the face of rich text in the base size
/// and black, as viewers set it: the colour of the default appearance does not
/// apply, and a default style with a declaration that is not one (no colon) is a
/// syntax error that drops all of it. Relative sizes are of a base size that is
/// `positive` or not.
pub(super) fn default_style(ds: Option<&[u8]>, positive: bool) -> Result<Style, Unsupported> {
    let style = Style {
        face: Family::Text.into(),
        size: Size::Base(1.0),
        color: Color::BLACK,
        align: Align::Left,
    };
    let ds = ds.map(text::text_string).filter(|css| {
        css.split(';')
            .all(|d| d.trim().is_empty() || d.contains(':'))
    });
    match ds {
        Some(ds) => rich::css_style(&ds, style, positive),
        None => Ok(style),
    }
}

/// The spans of rich text `rc` (or, without it, the plain text `plain` gives) over
/// `style`, a [`default_style`], as viewers set them: lines are aligned by their own
/// `text-align`, not by /Q. Relative sizes are of a base size that is `positive` or
/// not.
pub(super) fn rich_spans(
    rc: Option<&[u8]>,
    style: &Style,
    positive: bool,
    plain: impl FnOnce() -> Result<String, Unsupported>,
) -> Result<Vec<Span>, Unsupported> {
    match rc {
        // A zero character is drawn as a fallback glyph (observed), which the rich
        // text parser drops.
        Some(rc) if text::text_string(rc).contains('\0') => Err(Unsupported::Character('\0')),
        Some(rc) => rich::rich_text(&text::text_string(rc), style, positive),
        None => Ok(vec![Span {
            text: plain()?,
            style: style.clone(),
        }]),
    }
}

/// `r` with each side moved in by `d`, as given: a box of negative width or height
/// grows (where [`kurbo::Rect::inset`] takes it as the box it covers).
fn within(r: kurbo::Rect, d: f64) -> kurbo::Rect {
    kurbo::Rect::new(r.x0 + d, r.y0 + d, r.x1 - d, r.y1 - d)
}

/// `/Contents` as text.
fn contents(annot: &Entries<'_>) -> String {
    annot
        .text(Text::Contents)
        .map(|s| text::text_string(&s))
        .unwrap_or_default()
}
