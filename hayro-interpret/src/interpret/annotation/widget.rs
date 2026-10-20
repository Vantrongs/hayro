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
use super::out::{Color, Out, Part, Refused, Rounding, as_written};
use super::text::{self, Align, Family, Fonts, Setting, Shaped, Style, TextBox};
use super::{
    Appearances, Drew, FIELD, Field, Inherited, TEXT, TextKey, Unsupported, border_width, circle,
    color_of, most_pieces, rect,
};
use hayro_syntax::object::dict::keys::AS;
use kurbo::{Affine, Shape};
use std::rc::Rc;

type Drawn = Drew;

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
        if shows {
            out.refuse(Unsupported::Coordinates, Part::Anywhere)?;
        }
        return Ok(None);
    };

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
    // The sides as viewers compute them, in 32-bit numbers: the field's room for
    // text is measured against them to the bit ([`text::Shaped::lines`]).
    let (width, height) = (
        f64::from(r.x1 as f32 - r.x0 as f32),
        f64::from(r.y1 as f32 - r.y0 as f32),
    );
    let (fw, fh) = if matches!(rotation % 360, 90 | 270) {
        (height, width)
    } else {
        (width, height)
    };
    if !(fw > 0.0 && fh > 0.0) {
        return Ok(None);
    }
    let turn = turned(rotation);
    let place = super::rect_transform(r, [0.0, 0.0, fw as f32, fh as f32], turn) * turn;
    let bbox = kurbo::Rect::new(0.0, 0.0, fw, fh);
    // What the page shows of the box: the rectangle holding it, in the box's space,
    // for finding the text that can show.
    let shown = (place.inverse() * visible.to_path(0.1)).bounding_box();
    out.op("q");
    out.writes(|out| {
        out.nums(&place.as_coeffs(), "cm")?;
        out.clip(bbox)
    })?;
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

/// Draws the widget of [`widget`] in its box `bbox`, of which `visible` shows.
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
            out.writes(|out| radio(sq, w, background, border, on, out))?;
        } else {
            out.writes(|out| frame(sq, w, background, border, out))?;
            if on {
                // The mark is drawn black whatever the default appearance says,
                // within the box.
                let check = fonts.named(Family::Dingbats, "a19");
                let check = out.part(check.ok_or(Unsupported::Kind), || Part::rect(bbox))?;
                if let Some(check) = check {
                    let size = s as f32;
                    out.op("0 g");
                    let origin = (w as f32 + 0.1 * size, 0.2 * size);
                    out.writes(|out| text::draw(&[check], size, origin, Affine::IDENTITY, out))?;
                }
            }
        }
        return Ok(Some(()));
    }

    // The default appearance: of the widget or the nearest field above it that has
    // one, its text, or none when it is not text; else the form's. Each is read
    // once for the document.
    let (family, size, text_color) = if field.found(Inherited::Appearance) {
        doc.default_appearance(Some(field.source(Inherited::Appearance)), || {
            field.get(Inherited::Appearance).text()
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
        // Viewers draw a push button's colours, and shade its bevel, in RGB.
        let (background, border) = (background.map(Color::rgb), border.map(Color::rgb));
        out.writes(|out| {
            out.within(bbox.inset(-(w + 2.0)))?;
            frame(bbox, w, background, border, out)?;
            match background {
                Some(bg) => bevel(bbox, w, bg, out),
                None => Some(()),
            }
        })?;
        // The caption, read once for the document where it has a source.
        let Some(mk) = mk else {
            return Ok(Some(()));
        };
        let source = mk.source(Text::Caption.key());
        let caption = doc.plain(source, face, TEXT, || {
            Ok(mk
                .text(Text::Caption)
                .map(|c| text::text_string(&c))
                .unwrap_or_default())
        });
        // A caption that cannot be set would be drawn within the box (observed:
        // viewers draw the rest of the button as given).
        let Some(caption) = out.part(caption, || Part::rect(bbox))? else {
            return Ok(Some(()));
        };
        if caption.len() == 0 {
            return Ok(Some(()));
        }
        let inset = 2.0 * (w as f32 + 2.0);
        let em = caption.width(0..caption.len(), 1.0) as f32;
        let size = size.unwrap_or_else(|| {
            // Four points more of padding on each side, across as well as up
            // (observed).
            let by_height = fh - inset - 8.0;
            let measured = caption.em();
            let by_width = if measured > 0.0 {
                (fw - inset - 8.0) / measured
            } else {
                by_height
            };
            by_height.min(by_width).max(1.0)
        });
        let x = (fw - em * size) / 2.0;
        out.writes(|out| out.fill_color(&text_color))?;
        let origin = (f64::from(x), f64::from(fh / 2.0 - 0.3 * size));
        let set = Setting {
            base: size,
            color: text_color,
            align: Align::Left,
        };
        // Centred by its advance; its baseline from the height and size.
        let computed = (
            text::aligned(&caption, 0..caption.len(), size, origin.0, f64::from(fw)),
            Rounding::of(3.0, f64::from(fh).abs() + f64::from(size).abs()),
        );
        text::draw_line(
            fonts,
            &caption,
            &set,
            origin,
            computed,
            Affine::IDENTITY,
            shown,
            out,
        )?;
        return Ok(Some(()));
    }

    out.writes(|out| frame(bbox, w, background, border, out))?;
    // Text is clipped to the box inside the border: text that cannot be set as
    // viewers set it would be drawn there (observed: viewers draw the rest of the
    // field as given).
    let clip = bbox.inset(-w);
    let in_clip = || Part::rect(clip);
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
        let shows = doc.default_style(annot, positive).and_then(|style| {
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
                    let spans = freetext::rich_spans(rv.as_deref(), &style, positive, || {
                        value_text(field)
                    })?;
                    Ok(FieldText::Rich(Rc::new(fonts.shape_rich(&spans)?)))
                },
            )
        });
        let Some(shows) = out.part(shows, in_clip)? else {
            return Ok(Some(()));
        };
        shows
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
            rich: true,
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
    // The room for the text, as viewers compute it: not held at 0 where the
    // padding is wider than the field (observed: a field 10 wide at border width
    // 3 centres its text at 6 + (-2 - width) / 2).
    let avail = fw - 2.0 * pad;
    let q = annot.int(b"Q").unwrap_or(0);
    let align = |width: f32| match q {
        1 => pad + (avail - width) / 2.0,
        2 => fw - pad - width,
        _ => pad,
    };
    out.op("q");
    out.writes(|out| {
        out.clip(clip)?;
        out.fill_color(&text_color)
    })?;
    let comb = kind == Kind::Text && flags & (1 << 24) != 0 && !multiline;
    // What of the text the page can show: the work of drawing a value grows with
    // that, and the value, which widgets of one field share, is set once.
    let seen = clip.intersect(shown);
    let source = field.source(Inherited::Value);
    let value = |out: &mut Out| {
        let value = doc.plain(source, face, TEXT, || value_text(field));
        out.part(value, in_clip)
    };
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
                (f64::from(pad), Rounding::of(2.0, f64::from(pad))),
                (
                    top,
                    Rounding::of(2.0, f64::from(fh).abs() + f64::from(pad).abs()),
                ),
                step,
                seen,
                out,
            )?;
        }
        _ if multiline => {
            let size = size.unwrap_or(12.0);
            let Some(value) = value(out)? else {
                out.op("Q");
                return Ok(Some(()));
            };
            // The first line's baseline is at least a fifth of the size above the
            // padding, however low the field (observed).
            let top = (fh - pad).max(pad + 1.316 * size);
            text::draw_rows(
                fonts,
                &value,
                &set(size),
                avail,
                (
                    f64::from(top),
                    Rounding::of(4.0, f64::from(fh).abs() + 2.0 * f64::from(size).abs()),
                ),
                f64::from(1.116 * size),
                align,
                q == 1 || q == 2,
                seen,
                out,
            )?;
        }
        // A comb of cells as many as `/MaxLen`; without one, the text from the
        // field's left edge.
        _ if comb => {
            let max_len = field.get(Inherited::MaxLen).int().filter(|n| *n >= 1);
            // The dividers before the text, which a border wider than a cell covers;
            // drawn whether the text is or not.
            if let Some(max_len) = max_len
                && let Some(border) = &border
            {
                out.writes(|out| out.stroke_color(border))?;
                comb_dividers(i64::from(max_len), f64::from(fw), f64::from(fh), w, out)?;
            }
            let Some(value) = value(out)? else {
                out.op("Q");
                return Ok(Some(()));
            };
            let all = 0..value.len();
            // Automatic: the field's height, or less for the text to fit across.
            let size = size.unwrap_or_else(|| {
                let em = value.em();
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
                let across = if q == 1 || q == 2 {
                    text::aligned(&value, 0..value.len(), size, origin.0, f64::from(fw))
                } else {
                    Rounding::NONE
                };
                text::draw_line(
                    fonts,
                    &value,
                    &set(size),
                    origin,
                    (across, middle(fh, size)),
                    Affine::IDENTITY,
                    seen,
                    out,
                )?;
                out.op("Q");
                return Ok(Some(()));
            };
            let cells = max_len as f32;
            let cell = fw / cells;
            // Each glyph centred in its cell: those whose cells the clip can show
            // are drawn.
            let count = value.len().min(max_len as usize);
            let y = f64::from(fh / 2.0 - 0.3 * size);
            let at = (y, middle(fh, size));
            text::draw_comb(fonts, &value, &set(size), count, cell, at, seen, out)?;
        }
        // A line break is drawn as a space, as any control character is.
        _ => {
            let Some(value) = value(out)? else {
                out.op("Q");
                return Ok(Some(()));
            };
            let em = value.width(0..value.len(), 1.0) as f32;
            let size = size.unwrap_or_else(|| {
                let by_height = fh - 2.0 * pad;
                let measured = value.em();
                let by_width = if measured > 0.0 {
                    avail / measured
                } else {
                    by_height
                };
                by_height.min(by_width).max(1.0)
            });
            let origin = (
                f64::from(align(em * size)),
                f64::from(fh / 2.0 - 0.3 * size),
            );
            let across = if q == 1 || q == 2 {
                text::aligned(&value, 0..value.len(), size, origin.0, f64::from(fw))
            } else {
                Rounding::of(2.0, origin.0)
            };
            text::draw_line(
                fonts,
                &value,
                &set(size),
                origin,
                (across, middle(fh, size)),
                Affine::IDENTITY,
                seen,
                out,
            )?;
        }
    }
    out.op("Q");
    Ok(Some(()))
}

/// How a viewer computes the baseline of one line of text at `size` in a field
/// `fh` high, half the height less three tenths of the size: a few roundings of
/// at most those.
fn middle(fh: f32, size: f32) -> Rounding {
    Rounding::of(3.0, f64::from(fh).abs() + f64::from(size).abs())
}

/// The lines between the `cells` cells of a comb field `fw` × `fh`, inside its
/// border of width `w` (> 0), stroked `w` wide with butt caps (line `i` at
/// `u = w + step i` across, `step` = `(fw - 2w) / cells`, from `w` to `fh - w` up),
/// those that paint where the page shows them under the clips they are drawn in
/// (the box, and the box inside the border), as [`Out::region`] says: each line as
/// it is written, its numbers as a viewer reads them, whose outline meets that.
///
/// Each line is its own stroke, however close or overlapping the lines: the
/// content can be scaled and zoomed any amount, and viewers draw lines within a
/// pixel solid when they are one path or one band, where lines stroked each are
/// grey, as in bake (observed). More lines that paint than
/// [`super::set_most_pieces`] allows are [`Unsupported::Coordinates`]. Lines of no
/// length as written draw nothing; nor do lines under a clip of no area.
///
/// The lines that paint are found without looking at the others: those whose
/// outline meets what shows are those whose `u`, as written, lies in one span
/// ([`super::out::Region::span`]), and `u` as written runs one way with `i`, so
/// they are a run of lines, whose ends are found by halving.
fn comb_dividers(cells: i64, fw: f64, fh: f64, w: f64, out: &mut Out) -> Result<(), Refused> {
    let cells = cells as f64;
    if cells < 2.0 {
        return Ok(());
    }
    let region = out.region();
    if region.is_empty() {
        return Ok(());
    }
    // A line as written: from `w` to `fh - w` up, half the width to each side.
    let (y0, y1, half) = (as_written(w), as_written(fh - w), as_written(w) / 2.0);
    if y0 == y1 {
        return Ok(());
    }
    let base =
        [(-half, y0), (half, y0), (half, y1), (-half, y1)].map(|(x, y)| kurbo::Point::new(x, y));
    let step = (fw - 2.0 * w) / cells;
    let u = |i: f64| w + step * i;
    // A viewer computes line `i` at `w` and `i` steps across, the step from the
    // width and the count ([`text::cell_place`]): the lines are looked for as far
    // as it may put the last.
    let place = |i: f64| text::cell_place(i, step, w);
    let far = fw.abs() + w.abs();
    let Some((u0, u1)) = region.span(&base, kurbo::Vec2::new(1.0, 0.0), far) else {
        return Ok(());
    };
    let g = place(cells).bound().x;
    let (u0, u1) = (u0 - g, u1 + g);
    let at = |i: f64| as_written(u(i));
    // The lines 1 to `cells - 1` whose `u` as written lies in `u0..=u1`: before
    // the first, it lies on one side of the span, past the last, on the other.
    let (lo, hi) = (1.0, cells - 1.0);
    let (first, end) = if step >= 0.0 {
        (
            first_where(lo, hi, |i| at(i) >= u0),
            first_where(lo, hi, |i| at(i) > u1),
        )
    } else {
        (
            first_where(lo, hi, |i| at(i) <= u1),
            first_where(lo, hi, |i| at(i) < u0),
        )
    };
    if first >= end {
        return Ok(());
    }
    if end - first > most_pieces() {
        // Lines that paint where the page shows them, under the clips set.
        return out.refuse(Unsupported::Coordinates, Part::Anywhere);
    }
    out.writes(|out| {
        out.stroke_state(w, &[])?;
        for k in 0..(end - first) as u64 {
            let i = first + k as f64;
            let (u, place) = (u(i), place(i));
            out.computed(&[u, w], "m", place)?;
            out.computed(&[u, fh - w], "l", place)?;
            out.op("S");
        }
        Some(())
    })
}

/// The first whole number from `lo` to `hi` for which `holds`, which holds from
/// some number on; `hi + 1` for none.
fn first_where(lo: f64, hi: f64, holds: impl Fn(f64) -> bool) -> f64 {
    let (mut lo, mut hi) = (lo, hi + 1.0);
    while lo < hi {
        let mid = ((lo + hi) / 2.0).floor();
        if holds(mid) {
            hi = mid;
        } else {
            lo = mid + 1.0;
        }
    }
    lo
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
    out.op("0.95 0.33 0.18 rg");
    out.writes(|out| {
        out.nums(&[pts[0].0, pts[0].1], "m")?;
        for p in &pts[1..] {
            out.nums(&[p.0, p.1], "l")?;
        }
        Some(())
    })?;
    out.op("h f");
    // The word is drawn on the tag.
    let tag = kurbo::Rect::new(x0, y1 - h / 6.0, x0 + h * 3.0 / 4.0, y1);
    let Some(glyphs) = out.part(fonts.shape(Family::Sans, "SIGN"), || Part::rect(tag))? else {
        return Ok(Some(()));
    };
    out.op("1 g");
    let origin = ((x0 + 0.1655 * h) as f32, (y1 - 0.1332 * h) as f32);
    out.writes(|out| text::draw(&glyphs, (h / 7.2) as f32, origin, Affine::IDENTITY, out))?;
    Ok(Some(()))
}

/// The value of the field `field` as text: a text string, or a text stream
/// ([`Value::field_text`]), or a name; nothing for an array of selected values, as
/// viewers show it.
fn value_text(field: &Field<'_>) -> Result<String, Unsupported> {
    Ok(match field.get(Inherited::Value) {
        Value::Name(n) => String::from_utf8_lossy(n.as_ref()).into_owned(),
        value => value
            .field_text()?
            .map(|t| text::text_string(&t))
            .unwrap_or_default(),
    })
}
