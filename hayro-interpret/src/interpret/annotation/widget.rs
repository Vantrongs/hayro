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

use super::entries::{Entries, Text, Value};
use super::freetext;
use super::out::{Color, Out};
use super::text::{self, Align, Family, Fonts, Setting, Shaped, Style, TextBox};
use super::{
    Appearances, FIELD, FINEST, Field, Inherited, TEXT, TextKey, Unsupported, border_width, circle,
    color_of, rect,
};
use hayro_syntax::object::dict::keys::AS;
use kurbo::{Affine, Shape};
use std::rc::Rc;

type Drawn = Result<Option<()>, Unsupported>;

/// What a text or choice field shows, read once for the document: its value or
/// options as plain text, rich text, or, for a list without `/RV` but with `/DS`,
/// its options from `/TI` on in the style of `/DS`, one a paragraph.
#[derive(Clone)]
pub(super) enum FieldText {
    Plain,
    Rich(Rc<Shaped>),
    Options(Rc<Style>),
}

/// What a widget with the entries of `field` draws on a page that shows `visible` of
/// it; `form` is the interactive form dictionary.
///
/// As viewers make it, the appearance is drawn in a box of the field's size, its
/// sides swapped for a turn (`/MK /R`) whose remainder by 360 is 90 or 270, turned
/// by `/MK /R` and fitted into `/Rect` (PDF 32000-1 12.5.5): a field turned by
/// another angle is tilted or squeezed, as in viewers. Everything below is drawn
/// in that box, clipped to it.
pub(super) fn widget<'a>(
    annot: &Entries<'a>,
    field: &Field<'a>,
    doc: &Appearances<'a>,
    visible: kurbo::Rect,
    out: &mut Out,
) -> Drawn {
    let field_type = field.get(Inherited::Type);
    let flags = field.get(Inherited::Flags).flags();
    let mk = annot.dict(b"MK");
    let mk_color = |key: &[u8]| mk.as_ref().and_then(|mk| color_of(mk, key));
    let w = border_width(annot);
    let border = mk_color(b"BC").filter(|_| w > 0.0);
    let background = mk_color(b"BG");
    let Some(r) = rect(annot) else {
        // Without a box, viewers draw at coordinates of ±2^32: a button, or the
        // background of a text or choice field, covers the page or shows nowhere.
        let shows = match field_type.name() {
            Some(b"Btn") => true,
            Some(b"Tx" | b"Ch") => background.is_some(),
            _ => false,
        };
        return if shows {
            Err(Unsupported::Coordinates)
        } else {
            Ok(None)
        };
    };
    let fail = |o: Option<()>| o.ok_or(Unsupported::Kind);

    let kind = match field_type.name() {
        Some(b"Tx") => Kind::Text,
        Some(b"Ch") if flags & (1 << 17) != 0 => Kind::Combo,
        Some(b"Ch") => Kind::List,
        Some(b"Btn") if flags & (1 << 16) != 0 => Kind::Push,
        Some(b"Btn") if flags & (1 << 15) != 0 => Kind::Radio,
        Some(b"Btn") => Kind::Check,
        // Signed only by a signature dictionary.
        Some(b"Sig") if !matches!(field.get(Inherited::Value), Value::Dict(_)) => Kind::Sig,
        _ => return Ok(None),
    };

    // The box and how it is placed: a field of no area is drawn squeezed to
    // nothing.
    // An unsigned signature's tag is not turned (observed).
    let rotation = match kind {
        Kind::Sig => 0,
        _ => mk.as_ref().and_then(|mk| mk.int(b"R")).unwrap_or(0),
    };
    let (fw, fh) = if matches!(rotation % 360, 90 | 270) {
        (r.height(), r.width())
    } else {
        (r.width(), r.height())
    };
    if !(fw > 0.0 && fh > 0.0) {
        return Ok(None);
    }
    let turn = turned(rotation);
    let place = super::rect_transform(r, [0.0, 0.0, fw as f32, fh as f32], turn) * turn;
    let bbox = kurbo::Rect::new(0.0, 0.0, fw, fh);
    // What the page shows of the box, in the box's space.
    let shown = (place.inverse() * visible.to_path(0.1)).bounding_box();
    out.op("q");
    fail(out.nums(&place.as_coeffs(), "cm"))?;
    fail(out.clip(bbox))?;
    let drawn = draw(
        annot,
        field,
        doc,
        kind,
        flags,
        mk.as_ref(),
        w,
        border,
        background,
        bbox,
        shown,
        out,
    );
    out.op("Q");
    drawn
}

/// The turn of `degrees`, exact for the right angles.
fn turned(degrees: i32) -> Affine {
    match degrees.rem_euclid(360) {
        0 => Affine::IDENTITY,
        90 => Affine::new([0.0, 1.0, -1.0, 0.0, 0.0, 0.0]),
        180 => Affine::new([-1.0, 0.0, 0.0, -1.0, 0.0, 0.0]),
        270 => Affine::new([0.0, -1.0, 1.0, 0.0, 0.0, 0.0]),
        _ => Affine::rotate(f64::from(degrees).to_radians()),
    }
}

/// Draws the widget of [`widget`] in its box `bbox`, of which `shown` shows.
#[allow(clippy::too_many_arguments)]
fn draw<'a>(
    annot: &Entries<'a>,
    field: &Field<'a>,
    doc: &Appearances<'a>,
    kind: Kind,
    flags: u32,
    mk: Option<&Entries<'a>>,
    w: f64,
    border: Option<Color>,
    background: Option<Color>,
    bbox: kurbo::Rect,
    shown: kurbo::Rect,
    out: &mut Out,
) -> Drawn {
    let fail = |o: Option<()>| o.ok_or(Unsupported::Kind);
    let (fonts, form) = (&doc.fonts, doc.form());
    let form = form.as_ref();
    let (fw, fh) = (bbox.width(), bbox.height());
    if kind == Kind::Sig {
        return signature(bbox, fonts, out);
    }
    if matches!(kind, Kind::Check | Kind::Radio) {
        let on = annot.name(AS).is_some_and(|state| **state != *b"Off");
        let s = fw.min(fh);
        // The square at the bottom left of the box.
        let sq = kurbo::Rect::new(0.0, 0.0, s, s);
        if kind == Kind::Radio {
            fail(radio(sq, w, background, border, on, out))?;
        } else {
            fail(frame(sq, w, background, border, out))?;
            if on {
                // The mark is drawn black whatever the default appearance says.
                let check = fonts
                    .named(Family::Dingbats, "a19")
                    .ok_or(Unsupported::Kind)?;
                let size = s as f32;
                out.op("0 g");
                let origin = (w as f32 + 0.1 * size, 0.2 * size);
                fail(text::draw(&[check], size, origin, Affine::IDENTITY, out))?;
            }
        }
        return Ok(Some(()));
    }

    // The default appearance: of the widget or the nearest field above it that has
    // one, its text, or none when it is not text; else the form's. Each is read
    // once for the document.
    let (family, size, text_color) = if field.found(Inherited::Appearance) {
        doc.default_appearance(Some(field.source(Inherited::Appearance)), || {
            field
                .get(Inherited::Appearance)
                .text(Text::DefaultAppearance)
        })
    } else {
        doc.default_appearance(None, || form.and_then(|f| f.text(Text::DefaultAppearance)))
    };
    let text_color = text_color.unwrap_or(Color::BLACK);
    let (fw, fh) = (fw as f32, fh as f32);
    let face = family.into();

    if kind == Kind::Push {
        // Viewers lay the bevel and the caption out from the box inside the border,
        // which must be writable even where nothing is drawn at it.
        fail(out.within(bbox.inset(-(w + 2.0))))?;
        // Viewers draw a push button's colours, and shade its bevel, in RGB.
        let (background, border) = (background.map(Color::rgb), border.map(Color::rgb));
        fail(frame(bbox, w, background, border, out))?;
        if let Some(bg) = background {
            fail(bevel(bbox, w, bg, out))?;
        }
        // The caption, read once for the document where it has a source.
        let Some(mk) = mk else {
            return Ok(Some(()));
        };
        let source = mk.source(Text::Caption.key());
        let caption = doc.plain(source, face, TEXT, || {
            mk.text(Text::Caption)
                .map(|c| text::text_string(&c))
                .unwrap_or_default()
        })?;
        if caption.len() == 0 {
            return Ok(Some(()));
        }
        let inset = 2.0 * (w as f32 + 2.0);
        let em = caption.width(0..caption.len(), 1.0) as f32;
        let size = size.unwrap_or_else(|| {
            // Four points more of padding on each side, across as well as up
            // (observed).
            let by_height = fh - inset - 8.0;
            let by_width = if em > 0.0 {
                (fw - inset - 8.0) / em
            } else {
                by_height
            };
            by_height.min(by_width).max(1.0)
        });
        let x = (fw - em * size) / 2.0;
        fail(out.fill_color(&text_color))?;
        let origin = (f64::from(x), f64::from(fh / 2.0 - 0.3 * size));
        let set = Setting {
            base: size,
            color: text_color,
            align: Align::Left,
        };
        text::draw_line(fonts, &caption, &set, origin, Affine::IDENTITY, shown, out)?;
        return Ok(Some(()));
    }

    fail(frame(bbox, w, background, border, out))?;
    // Text is clipped to the box inside the border.
    let clip = bbox.inset(-w);
    let pad = 2.0 * w as f32;
    let multiline = kind == Kind::Text && flags & (1 << 12) != 0;
    // Rich text: the widget's own /RV or /DS, whatever its RichText flag says;
    // viewers do not take them from the fields above it. Of a text field or a
    // choice field, as the text of a text field: a list's on several lines, its
    // text without /RV its options from /TI on, one a line (observed). A size fit to
    // the field is none: such rich text is drawn 0 high.
    let rich_size = size.unwrap_or(0.0);
    let positive = rich_size > 0.0;
    let top_index = annot
        .int(b"TI")
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);
    // What the field shows, read once for the document under the entry it is read
    // from and the style `/DS` sets, whose values are not read for each widget:
    // `/RV`; else, of a list, the style alone, that of its options; else the value.
    let rich = annot.has_text(Text::RichValue);
    let shows = if matches!(kind, Kind::Text | Kind::List | Kind::Combo)
        && (rich || annot.has_text(Text::DefaultStyle))
    {
        let options = kind == Kind::List && !rich;
        let style = doc.default_style(annot, positive)?;
        let read = if rich {
            Some(annot.source(Text::RichValue.key()))
        } else if !options {
            Some(field.source(Inherited::Value))
        } else {
            None
        };
        let key = TextKey::new(
            read,
            Family::Text.into(),
            vec![FIELD, u64::from(options), u64::from(positive)],
        )
        .styled(&style);
        doc.once(
            |k| &mut k.field_text,
            Some(key),
            || {
                if options {
                    return Ok(FieldText::Options(Rc::new(style)));
                }
                // A list shows `/RV` here; its options only without it, above.
                let rv = rich.then(|| annot.text(Text::RichValue)).flatten();
                let spans =
                    freetext::rich_spans(rv.as_deref(), &style, positive, || value_text(field))?;
                Ok(FieldText::Rich(Rc::new(fonts.shape_rich(&spans)?)))
            },
        )?
    } else {
        FieldText::Plain
    };
    if !matches!(shows, FieldText::Plain) {
        let size = rich_size;
        let multiline = multiline || kind == Kind::List;
        // As viewers lay rich text out: from the top less twice the border width
        // on several lines; on one line, in a band twice the size high whose top
        // lies 1.3 sizes and four border widths above the field's middle, of which
        // the lines that fit are drawn. Either is as wide as the field less twice
        // the border width on the left and four on the right, and the text is
        // wrapped to that width.
        let w = w as f32;
        let (top, room) = if multiline {
            (fh - pad, f32::INFINITY)
        } else {
            (1.3 * size + 4.0 * w - fh / 2.0, 2.0 * size)
        };
        let bottom = if multiline { -2.0 * size } else { top - room };
        let band = kurbo::Rect::new(
            f64::from(pad),
            f64::from(bottom),
            f64::from(fw - 4.0 * w),
            f64::from(top),
        );
        let area = TextBox {
            pad,
            top,
            room,
            frame: Affine::IDENTITY,
            clip: clip.intersect(band),
        };
        let avail = (fw - 3.0 * pad).max(0.0);
        out.op("q");
        match shows {
            FieldText::Rich(text) => {
                let set = Setting {
                    base: size,
                    color: Color::BLACK,
                    align: Align::Left,
                };
                let bound = text.bound(set.base);
                text::draw_box(
                    fonts,
                    std::iter::once(Ok(text)),
                    bound,
                    &set,
                    avail,
                    freetext::RICH_ASCENT,
                    &area,
                    shown,
                    out,
                )?;
            }
            FieldText::Options(ds) => {
                // The options from /TI on, one a paragraph, in the style of /DS: each
                // label set once for the document, and only those reached.
                let set = Setting {
                    base: ds.size.at(size),
                    color: ds.color,
                    align: ds.align,
                };
                let labels = doc.labels(annot.source(b"Opt"), || annot.items(b"Opt"));
                let reach = fonts
                    .bound(ds.face)
                    .map_or(0.0, |b| b.y0.abs().max(b.y1.abs()));
                let pieces = labels
                    .get(top_index..)
                    .unwrap_or_default()
                    .iter()
                    .map(|label| doc.label(label, ds.face));
                text::draw_box(
                    fonts,
                    pieces,
                    (set.base.abs(), reach),
                    &set,
                    avail,
                    freetext::RICH_ASCENT,
                    &area,
                    shown,
                    out,
                )?;
            }
            FieldText::Plain => {}
        }
        out.op("Q");
        return Ok(Some(()));
    }
    let avail = (fw - 2.0 * pad).max(0.0);
    let q = annot.int(b"Q").unwrap_or(0);
    let align = |width: f32| match q {
        1 => pad + (avail - width) / 2.0,
        2 => fw - pad - width,
        _ => pad,
    };
    out.op("q");
    fail(out.clip(clip))?;
    fail(out.fill_color(&text_color))?;
    let comb = kind == Kind::Text && flags & (1 << 24) != 0 && !multiline;
    // What of the text the page can show: the work of drawing a value grows with
    // that, and the value, which widgets of one field share, is set once.
    let seen = clip.intersect(shown);
    let source = field.source(Inherited::Value);
    let value = || doc.plain(source, face, TEXT, || value_text(field));
    let set = |size: f32| Setting {
        base: size,
        color: text_color,
        align: Align::Left,
    };
    match kind {
        Kind::List => {
            let size = size.unwrap_or(12.0);
            // From `/TI` on, each option a line down from the top less the padding;
            // one that is not text keeps its line. Only the options whose lines can
            // show are read and set, each once for the document.
            let labels = doc.labels(annot.source(b"Opt"), || annot.items(b"Opt"));
            let labels = labels.get(top_index..).unwrap_or_default();
            let (top, step) = (f64::from(fh - pad), f64::from(1.116 * size));
            text::draw_options(
                fonts,
                |i| doc.label(&labels[i], face),
                labels.len(),
                face,
                &set(size),
                f64::from(pad),
                top,
                step,
                seen,
                out,
            )?;
        }
        _ if multiline => {
            let size = size.unwrap_or(12.0);
            let value = value()?;
            // The first line's baseline is at least a fifth of the size above the
            // padding, however low the field (observed).
            let top = (fh - pad).max(pad + 1.316 * size);
            text::draw_rows(
                fonts,
                &value,
                &set(size),
                avail,
                f64::from(top),
                f64::from(1.116 * size),
                align,
                seen,
                out,
            )?;
        }
        // A comb of cells as many as `/MaxLen`; without one, the text from the
        // field's left edge.
        _ if comb => {
            let value = value()?;
            let all = 0..value.len();
            let max_len = field.get(Inherited::MaxLen).int().filter(|n| *n >= 1);
            // Automatic: the field's height, or less for the text to fit across.
            let size = size.unwrap_or_else(|| {
                let em = value.width(all.clone(), 1.0) as f32;
                if em > 0.0 { fh.min(fw / em) } else { fh }
            });
            let Some(max_len) = max_len else {
                // Aligned by /Q in the whole width, whatever the border (observed).
                let width = value.width(all, size) as f32;
                let x = match q {
                    1 => (fw - width) / 2.0,
                    2 => fw - width,
                    _ => 0.0,
                };
                let origin = (f64::from(x), f64::from(fh / 2.0 - 0.3 * size));
                text::draw_line(
                    fonts,
                    &value,
                    &set(size),
                    origin,
                    Affine::IDENTITY,
                    seen,
                    out,
                )?;
                out.op("Q");
                return Ok(Some(()));
            };
            let cells = max_len as f32;
            let cell = fw / cells;
            // The dividers before the text, which a border wider than a cell covers.
            if let Some(border) = &border {
                fail(out.stroke_color(border))?;
                fail(out.stroke_state(w, &[]))?;
                fail(comb_dividers(
                    i64::from(max_len),
                    f64::from(fw),
                    f64::from(fh),
                    w,
                    Affine::IDENTITY,
                    shown,
                    out,
                ))?;
                out.op("S");
            }
            // Each glyph centred in its cell: those whose cells the clip can show
            // are drawn.
            let count = value.len().min(max_len as usize);
            let y = f64::from(fh / 2.0 - 0.3 * size);
            text::draw_comb(fonts, &value, &set(size), count, cell, y, seen, out)?;
        }
        // A line break is drawn as a space, as any control character is.
        _ => {
            let value = value()?;
            let em = value.width(0..value.len(), 1.0) as f32;
            let size = size.unwrap_or_else(|| {
                let by_height = fh - 2.0 * pad;
                let by_width = if em > 0.0 { avail / em } else { by_height };
                by_height.min(by_width).max(1.0)
            });
            let origin = (
                f64::from(align(em * size)),
                f64::from(fh / 2.0 - 0.3 * size),
            );
            text::draw_line(
                fonts,
                &value,
                &set(size),
                origin,
                Affine::IDENTITY,
                seen,
                out,
            )?;
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
        [p.x, p.y]
    };
    if step - w < FINEST {
        // From the outer edge of the first line to that of the last, as high as the
        // lines.
        let (a, b) = (w + step, w + step * (cells - 1.0));
        let (u0, u1) = (a.min(b) - w / 2.0, a.max(b) + w / 2.0);
        out.nums(&[(fh - 2.0 * w).abs()], "w")?;
        out.nums(&at(u0, fh / 2.0), "m")?;
        return out.nums(&at(u1, fh / 2.0), "l");
    }
    // The lines within half a width of the visible part of the frame.
    let shown = (turn.inverse() * visible.to_path(0.1)).bounding_box();
    let m = w.abs();
    let first = ((shown.x0 - 1.5 * m) / step).floor().max(1.0);
    let last = ((shown.x1 - 0.5 * m) / step).ceil().min(cells - 1.0);
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
    Sig,
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
    w: f64,
    background: Option<Color>,
    border: Option<Color>,
    out: &mut Out,
) -> Option<()> {
    if let Some(bg) = background {
        out.fill_color(&bg)?;
        out.nums(&[r.x0, r.y0, r.width(), r.height()], "re")?;
        out.op("f");
    }
    if let Some(bc) = border {
        out.stroke_color(&bc)?;
        out.stroke_state(w, &[])?;
        let b = r.inset(-w / 2.0);
        out.nums(&[b.x0, b.y0, b.width(), b.height()], "re")?;
        out.op("S");
    }
    Some(())
}

/// A radio button in the square `sq`: a disc of the background, a ring of the
/// border, and, on, a black dot half as wide as the inside of the ring.
fn radio(
    sq: kurbo::Rect,
    w: f64,
    background: Option<Color>,
    border: Option<Color>,
    on: bool,
    out: &mut Out,
) -> Option<()> {
    let c = sq.center();
    let half = sq.width() / 2.0;
    let disc = |r: f64, out: &mut Out| out.path(&circle(c, r));
    if let Some(bg) = background {
        out.fill_color(&bg)?;
        disc(half, out)?;
        out.op("f");
    }
    if let Some(bc) = border {
        out.stroke_color(&bc)?;
        out.stroke_state(w, &[])?;
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
fn bevel(r: kurbo::Rect, w: f64, bg: Color, out: &mut Out) -> Option<()> {
    let o = r.inset(-w);
    let i = r.inset(-(w + 2.0));
    if o.width() <= 0.0 || o.height() <= 0.0 || i.width() <= 0.0 || i.height() <= 0.0 {
        return Some(());
    }
    let poly = |pts: &[(f64, f64)], out: &mut Out| -> Option<()> {
        out.nums(&[pts[0].0, pts[0].1], "m")?;
        for p in &pts[1..] {
            out.nums(&[p.0, p.1], "l")?;
        }
        out.op("h f");
        Some(())
    };
    out.fill_color(&bg.map(|c| (c + 1.0) / 2.0))?;
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
    out.fill_color(&bg.map(|c| c / 2.0))?;
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
    fail(out.nums(&[pts[0].0, pts[0].1], "m"))?;
    for p in &pts[1..] {
        fail(out.nums(&[p.0, p.1], "l"))?;
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

/// The value of the field `field` as text: a text string, or a text stream, or a
/// name; nothing for an array of selected values, as viewers show it.
fn value_text(field: &Field<'_>) -> String {
    match field.get(Inherited::Value) {
        Value::Name(n) => String::from_utf8_lossy(n.as_ref()).into_owned(),
        value => value
            .text(Text::FieldValue)
            .map(|t| text::text_string(&t))
            .unwrap_or_default(),
    }
}
