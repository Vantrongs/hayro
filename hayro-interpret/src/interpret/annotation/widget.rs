//! Form field widgets (PDF 32000-1 12.7.4, 12.5.6.19): the border and background of
//! `/MK`, and the field's value: the text of a text field (single line, multiline,
//! comb), the value of a combo box, the options of a list box, the caption of a
//! push button, the mark of a check box or radio button in an on state, and the
//! look of an unsigned signature field.
//!
//! What is drawn follows the spec; the layout (padding of twice the border width,
//! text centred 0.3 em below the middle, an automatic size that fits the field,
//! the check mark `ZapfDingbats` a19, the radio dot, the push button bevel, the
//! signature tag) matches what common viewers draw for widgets without an
//! appearance stream. The check box and radio button shapes take the square at the
//! bottom left of the field, turned with it by `/MK /R`. A widget whose `/Parent`
//! chain loops is a broken field and draws nothing, as in common viewers.

use super::text::{self, Family, Fonts, Glyph};
use super::{FINEST, Field, Out, Unsupported, border_width, color, rect};
use hayro_syntax::object::dict::keys::AS;
use hayro_syntax::object::{Array, Dict, Name, Object};
use kurbo::{Affine, Shape};

type Drawn = Result<Option<()>, Unsupported>;

/// What a widget draws on a page that shows `visible` of it; `form` is the
/// interactive form dictionary.
pub(super) fn widget(
    annot: &Dict<'_>,
    form: Option<&Dict<'_>>,
    fonts: &Fonts,
    visible: kurbo::Rect,
    out: &mut Out,
) -> Drawn {
    let (Some(r), Some(field)) = (rect(annot), Field::of(annot)) else {
        return Ok(None);
    };
    let field_type = field.get::<Name<'_>>(b"FT");
    let flags = field.get::<u32>(b"Ff").unwrap_or(0);
    let mk = annot.get::<Dict<'_>>(b"MK");
    let mk_color = |key: &[u8], stroke: bool| {
        mk.as_ref()
            .and_then(|mk| mk.get::<Array<'_>>(key))
            .and_then(|a| color(&a, stroke))
    };
    let w = border_width(annot);
    let border = mk_color(b"BC", true).filter(|_| w > 0.0);
    let background = mk_color(b"BG", false);
    let fail = |o: Option<()>| o.ok_or(Unsupported::Kind);

    let kind = match field_type.as_ref().map(|n| n.as_ref()) {
        Some(b"Tx") => Kind::Text,
        Some(b"Ch") if flags & (1 << 17) != 0 => Kind::Combo,
        Some(b"Ch") => Kind::List,
        Some(b"Btn") if flags & (1 << 16) != 0 => Kind::Push,
        Some(b"Btn") if flags & (1 << 15) != 0 => Kind::Radio,
        Some(b"Btn") => Kind::Check,
        Some(b"Sig") if field.with(b"V").is_none() => return signature(r, fonts, out),
        _ => return Ok(None),
    };

    // A field turned by /MK /R lays out in a turned frame, `(u, v)` from its bottom
    // left.
    let rotation = mk
        .as_ref()
        .and_then(|mk| mk.get::<i64>(b"R"))
        .unwrap_or(0)
        .rem_euclid(360);
    let (fw, fh, turn) = match rotation {
        90 => (
            r.height(),
            r.width(),
            Affine::new([0.0, 1.0, -1.0, 0.0, r.x1, r.y0]),
        ),
        180 => (
            r.width(),
            r.height(),
            Affine::new([-1.0, 0.0, 0.0, -1.0, r.x1, r.y1]),
        ),
        270 => (
            r.height(),
            r.width(),
            Affine::new([0.0, -1.0, 1.0, 0.0, r.x0, r.y1]),
        ),
        _ => (r.width(), r.height(), Affine::translate((r.x0, r.y0))),
    };

    if matches!(kind, Kind::Check | Kind::Radio) {
        let on = annot
            .get::<Name<'_>>(AS)
            .is_some_and(|state| state.as_ref() != b"Off");
        let s = fw.min(fh);
        // The square at the bottom left of the turned frame; its frame and disc look
        // the same turned, the check mark is drawn in the frame.
        let sq = (turn * kurbo::Rect::new(0.0, 0.0, s, s).to_path(0.1)).bounding_box();
        if kind == Kind::Radio {
            fail(radio(
                sq,
                w,
                background.as_deref(),
                border.as_deref(),
                on,
                out,
            ))?;
        } else {
            fail(frame(sq, w, background.as_deref(), border.as_deref(), out))?;
            if on {
                // The mark is drawn black whatever the default appearance says.
                let check = fonts
                    .named(Family::Dingbats, "a19")
                    .ok_or(Unsupported::Kind)?;
                let size = s as f32;
                out.op("0 g");
                let origin = (w + 0.1 * size, 0.2 * size);
                fail(text::draw(&[check], size, origin, turn, out))?;
            }
        }
        return Ok(Some(()));
    }

    // The default appearance: the widget's, its field's, or the form's.
    let da = field
        .get::<hayro_syntax::object::String<'_>>(b"DA")
        .or_else(|| form.and_then(|f| f.get::<hayro_syntax::object::String<'_>>(b"DA")))
        .map(|s| s.as_bytes().to_vec())
        .unwrap_or_default();
    let (family, size, text_color) = text::default_appearance(&da);
    let text_color = text_color.unwrap_or_else(|| "0 g".to_owned());
    let (fw, fh) = (fw as f32, fh as f32);

    if kind == Kind::Push {
        fail(frame(r, w, background.as_deref(), border.as_deref(), out))?;
        if let Some(bg) = mk
            .as_ref()
            .and_then(|mk| mk.get::<Array<'_>>(b"BG"))
            .map(|a| a.iter::<f32>().collect::<Vec<_>>())
        {
            fail(bevel(r, w, &bg, out))?;
        }
        let caption = mk
            .as_ref()
            .and_then(|mk| mk.get::<hayro_syntax::object::String<'_>>(b"CA"))
            .map(|s| text::text_string(s.as_bytes()))
            .unwrap_or_default();
        if caption.is_empty() {
            return Ok(Some(()));
        }
        let glyphs = fonts.shape(family, &caption)?;
        let inset = 2.0 * (w + 2.0);
        let size = size.unwrap_or_else(|| {
            let by_height = fh - inset - 8.0;
            let em = text::width(&glyphs, 1.0);
            let by_width = if em > 0.0 {
                (fw - inset) / em
            } else {
                by_height
            };
            by_height.min(by_width).max(1.0)
        });
        let x = (fw - text::width(&glyphs, size)) / 2.0;
        out.op(&text_color);
        fail(text::draw(
            &glyphs,
            size,
            (x, fh / 2.0 - 0.3 * size),
            turn,
            out,
        ))?;
        return Ok(Some(()));
    }

    fail(frame(r, w, background.as_deref(), border.as_deref(), out))?;
    let value = field.get::<Object<'_>>(b"V");
    let value = match value {
        Some(Object::String(s)) => text::text_string(s.as_bytes()),
        Some(Object::Name(n)) => String::from_utf8_lossy(n.as_ref()).into_owned(),
        // The first of several selected values.
        Some(Object::Array(a)) => a
            .iter::<hayro_syntax::object::String<'_>>()
            .next()
            .map(|s| text::text_string(s.as_bytes()))
            .unwrap_or_default(),
        _ => String::new(),
    };
    let pad = 2.0 * w;
    let avail = (fw - 2.0 * pad).max(0.0);
    let q = field.get::<i64>(b"Q").unwrap_or(0);
    let align = |width: f32| match q {
        1 => pad + (avail - width) / 2.0,
        2 => fw - pad - width,
        _ => pad,
    };
    out.op("q");
    fail(out.nums(
        &[
            r.x0 as f32,
            r.y0 as f32,
            r.width() as f32,
            r.height() as f32,
        ],
        "re",
    ))?;
    out.op("W n");
    out.op(&text_color);
    let multiline = kind == Kind::Text && flags & (1 << 12) != 0;
    // A length beyond a 32-bit integer is no comb, as in common viewers.
    let max_len = field
        .get::<i64>(b"MaxLen")
        .filter(|n| (1..=i64::from(i32::MAX)).contains(n));
    let comb = kind == Kind::Text && flags & (1 << 24) != 0 && !multiline;
    match kind {
        Kind::List => {
            let size = size.unwrap_or(12.0);
            let opts = field.get::<Array<'_>>(b"Opt");
            let top_index = field.get::<u32>(b"TI").unwrap_or(0);
            let mut y = fh - pad - 1.116 * size;
            for opt in opts
                .iter()
                .flat_map(|o| o.iter::<Object<'_>>())
                .skip(top_index as usize)
            {
                let label = match opt {
                    Object::String(s) => text::text_string(s.as_bytes()),
                    // An export value and the text shown.
                    Object::Array(pair) => pair
                        .iter::<hayro_syntax::object::String<'_>>()
                        .nth(1)
                        .map(|s| text::text_string(s.as_bytes()))
                        .unwrap_or_default(),
                    _ => continue,
                };
                let glyphs = fonts.shape(family, &label)?;
                fail(text::draw_clipped(
                    &glyphs,
                    size,
                    (pad, y),
                    0.0,
                    turn,
                    r,
                    out,
                ))?;
                y -= 1.116 * size;
            }
        }
        _ if multiline => {
            let size = size.unwrap_or(12.0);
            let lines = text::lines(fonts, family, size, &value, Some(avail))?;
            let mut y = fh - pad - 1.116 * size;
            for line in &lines {
                fail(text::draw_clipped(
                    line,
                    size,
                    (align(text::width(line, size)), y),
                    0.0,
                    turn,
                    r,
                    out,
                ))?;
                y -= 1.116 * size;
            }
        }
        _ if comb && let Some(max_len) = max_len => {
            let cells = max_len as f32;
            let glyphs = fonts.shape(family, &value)?;
            // Automatic: the field's height, or less for the text to fit across.
            let size = size.unwrap_or_else(|| {
                let em = text::width(&glyphs, 1.0);
                if em > 0.0 { fh.min(fw / em) } else { fh }
            });
            let cell = fw / cells;
            for (i, g) in glyphs.iter().take(cells as usize).enumerate() {
                let x = cell * i as f32 + (cell - g.advance * size) / 2.0;
                fail(text::draw_clipped(
                    std::slice::from_ref(g),
                    size,
                    (x, fh / 2.0 - 0.3 * size),
                    0.0,
                    turn,
                    r,
                    out,
                ))?;
            }
            if let Some(border) = &border {
                out.op(border);
                fail(out.stroke_state(w, &[]))?;
                fail(comb_dividers(
                    max_len,
                    f64::from(fw),
                    f64::from(fh),
                    f64::from(w),
                    turn,
                    visible,
                    out,
                ))?;
                out.op("S");
            }
        }
        _ => {
            let glyphs: Vec<Glyph<'_>> = fonts.shape(family, &value.replace(['\r', '\n'], " "))?;
            let size = size.unwrap_or_else(|| {
                let by_height = fh - 2.0 * pad;
                let em = text::width(&glyphs, 1.0);
                let by_width = if em > 0.0 { avail / em } else { by_height };
                by_height.min(by_width).max(1.0)
            });
            let width = text::width(&glyphs, size);
            fail(text::draw_clipped(
                &glyphs,
                size,
                (align(width), fh / 2.0 - 0.3 * size),
                0.0,
                turn,
                r,
                out,
            ))?;
        }
    }
    out.op("Q");
    Ok(Some(()))
}

/// The lines between the `cells` cells of a comb field `fw` × `fh` in frame `turn`,
/// inside its border of width `w`, stroked `w` wide, those `visible` shows. Lines
/// whose gaps are finer than [`FINEST`] are drawn as the band they cover, one
/// stroke as wide, so the work does not grow with `cells` beyond what is visible.
fn comb_dividers(
    cells: i64,
    fw: f64,
    fh: f64,
    w: f64,
    turn: Affine,
    visible: kurbo::Rect,
    out: &mut Out,
) -> Option<()> {
    let cells = cells as f64;
    if cells < 2.0 {
        return Some(());
    }
    let step = (fw - 2.0 * w) / cells;
    let at = |u: f64, v: f64| {
        let p = turn * kurbo::Point::new(u, v);
        [p.x as f32, p.y as f32]
    };
    if step - w < FINEST {
        // From the outer edge of the first line to that of the last, as high as the
        // lines.
        let (a, b) = (w + step, w + step * (cells - 1.0));
        let (u0, u1) = (a.min(b) - w / 2.0, a.max(b) + w / 2.0);
        out.nums(&[(fh - 2.0 * w).abs() as f32], "w")?;
        out.nums(&at(u0, fh / 2.0), "m")?;
        return out.nums(&at(u1, fh / 2.0), "l");
    }
    // The lines within half a width of the visible part of the frame.
    let shown = (turn.inverse() * visible.to_path(0.1)).bounding_box();
    let first = ((shown.x0 - 1.5 * w) / step).floor().max(1.0);
    let last = ((shown.x1 - 0.5 * w) / step).ceil().min(cells - 1.0);
    if first > last {
        return Some(());
    }
    for i in 0..=(last - first) as u64 {
        let u = w + step * (first + i as f64);
        out.nums(&at(u, w), "m")?;
        out.nums(&at(u, fh - w), "l")?;
    }
    Some(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Combo,
    List,
    Push,
    Check,
    Radio,
}

/// The background filling `r` and the border stroked inside it.
fn frame(
    r: kurbo::Rect,
    w: f32,
    background: Option<&str>,
    border: Option<&str>,
    out: &mut Out,
) -> Option<()> {
    if let Some(bg) = background {
        out.op(bg);
        out.nums(
            &[
                r.x0 as f32,
                r.y0 as f32,
                r.width() as f32,
                r.height() as f32,
            ],
            "re",
        )?;
        out.op("f");
    }
    if let Some(bc) = border {
        out.op(bc);
        out.stroke_state(w, &[])?;
        let b = r.inset(-f64::from(w) / 2.0);
        out.nums(
            &[
                b.x0 as f32,
                b.y0 as f32,
                b.width() as f32,
                b.height() as f32,
            ],
            "re",
        )?;
        out.op("S");
    }
    Some(())
}

/// A radio button in the square `sq`: a disc of the background, a ring of the
/// border, and, on, a black dot half as wide as the inside of the ring.
fn radio(
    sq: kurbo::Rect,
    w: f32,
    background: Option<&str>,
    border: Option<&str>,
    on: bool,
    out: &mut Out,
) -> Option<()> {
    let c = sq.center();
    let half = sq.width() / 2.0;
    let w = f64::from(w);
    let disc = |r: f64, out: &mut Out| out.path(&kurbo::Circle::new(c, r).to_path(0.01));
    if let Some(bg) = background {
        out.op(bg);
        disc(half, out)?;
        out.op("f");
    }
    if let Some(bc) = border {
        out.op(bc);
        out.stroke_state(w as f32, &[])?;
        disc(half - w / 2.0, out)?;
        out.op("S");
    }
    if on {
        out.op("0 g");
        disc((half - w) / 2.0, out)?;
        out.op("f");
    }
    Some(())
}

/// The bevel of a push button with background `bg`: a lighter band along the top
/// and left, a darker one along the bottom and right, 2 pt wide inside the border.
fn bevel(r: kurbo::Rect, w: f32, bg: &[f32], out: &mut Out) -> Option<()> {
    let op = match bg.len() {
        1 => "g",
        3 => "rg",
        _ => return Some(()),
    };
    let tone = |f: &dyn Fn(f32) -> f32| {
        let mut s = String::new();
        for c in bg {
            s.push_str(&format!("{} ", f(c.clamp(0.0, 1.0))));
        }
        s + op
    };
    let w = f64::from(w);
    let o = r.inset(-w);
    let i = r.inset(-(w + 2.0));
    if o.width() <= 0.0 || o.height() <= 0.0 || i.width() <= 0.0 || i.height() <= 0.0 {
        return Some(());
    }
    let poly = |pts: &[(f64, f64)], out: &mut Out| -> Option<()> {
        out.nums(&[pts[0].0 as f32, pts[0].1 as f32], "m")?;
        for p in &pts[1..] {
            out.nums(&[p.0 as f32, p.1 as f32], "l")?;
        }
        out.op("h f");
        Some(())
    };
    out.op(&tone(&|c| c + (1.0 - c) / 2.0));
    poly(
        &[
            (o.x0, o.y0),
            (o.x0, o.y1),
            (o.x1, o.y1),
            (i.x1, i.y1),
            (i.x0, i.y1),
            (i.x0, i.y0),
        ],
        out,
    )?;
    out.op(&tone(&|c| c / 2.0));
    poly(
        &[
            (o.x0, o.y0),
            (i.x0, i.y0),
            (i.x1, i.y0),
            (i.x1, i.y1),
            (o.x1, o.y1),
            (o.x1, o.y0),
        ],
        out,
    )
}

/// An unsigned signature field: an orange tag reading SIGN at its top left, a sixth
/// of the field high.
fn signature(r: kurbo::Rect, fonts: &Fonts, out: &mut Out) -> Drawn {
    let h = r.height();
    let (x0, y1) = (r.x0, r.y1);
    let pts = [
        (x0, y1),
        (x0 + h * 2.0 / 3.0, y1),
        (x0 + h * 3.0 / 4.0, y1 - h / 12.0),
        (x0 + h * 2.0 / 3.0, y1 - h / 6.0),
        (x0, y1 - h / 6.0),
    ];
    let fail = |o: Option<()>| o.ok_or(Unsupported::Kind);
    out.op("0.95 0.33 0.18 rg");
    fail(out.nums(&[pts[0].0 as f32, pts[0].1 as f32], "m"))?;
    for p in &pts[1..] {
        fail(out.nums(&[p.0 as f32, p.1 as f32], "l"))?;
    }
    out.op("h f");
    let glyphs = fonts.shape(Family::Sans, "SIGN")?;
    out.op("1 g");
    let origin = ((x0 + 0.1655 * h) as f32, (y1 - 0.1332 * h) as f32);
    fail(text::draw(
        &glyphs,
        (h / 7.2) as f32,
        origin,
        Affine::IDENTITY,
        out,
    ))?;
    Ok(Some(()))
}
