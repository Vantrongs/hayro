//! Annotations drawn as a symbol: `Text` (a note), `FileAttachment` and `Sound` as
//! an icon of their `/Name` (PDF 32000-1 12.5.6.4, 12.5.6.15, 12.5.6.16), `Caret`
//! (12.5.6.11), `Stamp` as the text of its name (12.5.6.12, Table 181) and `Redact`
//! (12.5.6.23) as the outline of the area it marks.
//!
//! The spec leaves their look to the viewer. These follow what common viewers draw
//! when such an annotation has no appearance stream: a 20 pt icon at the top left
//! of `/Rect`, a black symbol in a box of /C (white without it); a 20 × 14 pt caret
//! centred on `/Rect`; the stamp's name in Times Bold in a box tilted 0.6°, set for
//! a 190 × 50 stamp and scaled to fit `/Rect`; a red outline of each quadrilateral.
//! The symbols are drawn here, each in a 10 pt square.

use super::entries::Entries;
use super::markup::{Quad, quads};
use super::out::{Color, Out};
use super::text::{self, Align, Family, Fonts, Setting, Shaped, Typeface};
use super::{Appearances, Unsupported, c_color, color_of, rect};
use hayro_syntax::object::dict::keys::C;
use kurbo::{Affine, BezPath, Circle, Point, Shape};

type Drawn = Result<Option<()>, Unsupported>;

fn fail(o: Option<()>) -> Result<(), Unsupported> {
    o.ok_or(Unsupported::Kind)
}

/// Which symbol an icon shows.
#[derive(Clone, Copy)]
pub(super) enum Icon {
    Note,
    FileAttachment,
    Sound,
}

/// The icon of a `Text`, `FileAttachment` or `Sound` annotation: as viewers draw
/// it, in a square at the top left of `/Rect` as wide as `/Rect`'s shorter side,
/// but no less than 10 and no more than 20, set here for 20; the symbol black, or
/// white on a box of /C whose luminance is below a quarter. The symbol is the one
/// `/Name` names, whichever subtype names it, the subtype's own without a name,
/// and a star for another name or a `/Name` that is not one.
pub(super) fn icon(annot: &Entries<'_>, kind: Icon, fonts: &Fonts, out: &mut Out) -> Drawn {
    let Some(r) = rect(annot) else {
        return Ok(None);
    };
    let s = r.width().min(r.height()).clamp(10.0, 20.0);
    let place = Affine::translate((r.x0, r.y1 - s)) * Affine::scale(s / 20.0);
    let symbol = match annot.name(b"Name") {
        Some(name) => Symbol::named(name.as_ref()),
        None if annot.present(b"Name") => Symbol::Star,
        None => match kind {
            Icon::Note => Symbol::Note,
            Icon::FileAttachment => Symbol::PushPin,
            Icon::Sound => Symbol::Speaker,
        },
    };
    let fill = color_of(annot, C).unwrap_or(Color::Gray(1.0));
    out.op("q");
    fail(out.nums(&place.as_coeffs(), "cm"))?;
    fail(out.fill_color(&fill))?;
    out.op("0 G");
    fail(out.stroke_state(1.25, &[]))?;
    fail(out.nums(&[0.625, 0.625, 18.75, 18.75], "re"))?;
    out.op("b");
    out.op(if fill.luminance() < 0.25 {
        "1 g"
    } else {
        "0 g"
    });
    // The symbol's 10 pt square.
    symbol.draw(Affine::translate((5.0, 5.0)), fonts, out)?;
    out.op("Q");
    Ok(Some(()))
}

enum Symbol {
    Note,
    Comment,
    Key,
    Glyph(char),
    /// A pilcrow, its bowl on the left.
    Paragraph,
    /// A triangle pointing up, the square wide, between these heights.
    Triangle(f64, f64),
    Star,
    PushPin,
    Graph,
    Paperclip,
    Tag,
    Speaker,
    Mic,
}

impl Symbol {
    /// The symbol `/Name` names.
    fn named(name: &[u8]) -> Self {
        match name {
            b"Comment" => Self::Comment,
            b"Key" => Self::Key,
            b"Note" => Self::Note,
            b"Help" => Self::Glyph('?'),
            b"Paragraph" => Self::Paragraph,
            b"NewParagraph" => Self::Triangle(0.0, 10.0),
            b"Insert" => Self::Triangle(3.75, 10.0),
            b"PushPin" => Self::PushPin,
            b"Graph" => Self::Graph,
            b"Paperclip" => Self::Paperclip,
            b"Tag" => Self::Tag,
            b"Speaker" => Self::Speaker,
            b"Mic" => Self::Mic,
            _ => Self::Star,
        }
    }

    /// Fills the symbol in the 10 pt square `at` maps from (0, 0)–(10, 10).
    fn draw(&self, at: Affine, fonts: &Fonts, out: &mut Out) -> Result<(), Unsupported> {
        let mut p = BezPath::new();
        let poly = |p: &mut BezPath, pts: &[(f64, f64)]| {
            p.move_to(pts[0]);
            for &q in &pts[1..] {
                p.line_to(q);
            }
            p.close_path();
        };
        let rect = |p: &mut BezPath, x0: f64, y0: f64, x1: f64, y1: f64| {
            poly(p, &[(x0, y0), (x1, y0), (x1, y1), (x0, y1)]);
        };
        // A stroke of `width` along the points, as an outline to fill.
        let stroke = |p: &mut BezPath, pts: &[(f64, f64)], width: f64| {
            let mut line = BezPath::new();
            line.move_to(pts[0]);
            for &q in &pts[1..] {
                line.line_to(q);
            }
            let style = kurbo::Stroke::new(width)
                .with_caps(kurbo::Cap::Round)
                .with_join(kurbo::Join::Round);
            p.extend(kurbo::stroke(
                line,
                &style,
                &kurbo::StrokeOpts::default(),
                0.01,
            ));
        };
        let ring = |p: &mut BezPath, c: (f64, f64), r: f64, width: f64| {
            let style = kurbo::Stroke::new(width);
            p.extend(kurbo::stroke(
                Circle::new(c, r).to_path(0.01),
                &style,
                &kurbo::StrokeOpts::default(),
                0.01,
            ));
        };
        match self {
            Self::Note => {
                for y in [8.75, 6.25, 3.75, 1.25] {
                    rect(&mut p, 0.0, y, 10.0, y + 1.25);
                }
            }
            Self::Comment => poly(
                &mut p,
                &[
                    (0.0, 3.0),
                    (6.5, 3.0),
                    (9.0, 0.5),
                    (9.0, 3.0),
                    (10.0, 3.0),
                    (10.0, 10.0),
                    (0.0, 10.0),
                ],
            ),
            Self::Key => {
                p.extend(Circle::new((6.8, 6.8), 3.2).to_path(0.01));
                stroke(&mut p, &[(5.5, 5.5), (0.8, 0.8)], 2.2);
                stroke(&mut p, &[(2.2, 2.2), (3.6, 0.8)], 2.0);
            }
            Self::Glyph(c) => {
                let glyphs = fonts.shape(Family::Sans, &c.to_string())?;
                // About 10 pt from baseline to top.
                let size = 13.0;
                let x = (10.0 - text::width(&glyphs, size)) / 2.0;
                fail(text::draw(&glyphs, size, (x, 0.3), at, out))?;
                return Ok(());
            }
            Self::Triangle(y0, y1) => poly(&mut p, &[(0.0, *y0), (10.0, *y0), (5.0, *y1)]),
            Self::Paragraph => {
                // A bar across the top with a half disc hanging from its left end,
                // and two stems down from it.
                let k = 0.552 * 2.5;
                p.move_to((8.75, 10.0));
                p.line_to((2.5, 10.0));
                p.curve_to((2.5 - k, 10.0), (0.0, 7.5 + k), (0.0, 7.5));
                p.curve_to((0.0, 7.5 - k), (2.5 - k, 5.0), (2.5, 5.0));
                for q in [
                    (3.75, 5.0),
                    (3.75, 0.0),
                    (5.0, 0.0),
                    (5.0, 8.75),
                    (6.25, 8.75),
                    (6.25, 0.0),
                    (7.5, 0.0),
                    (7.5, 8.75),
                    (8.75, 8.75),
                ] {
                    p.line_to(q);
                }
                p.close_path();
            }
            Self::Star => {
                let pts: Vec<(f64, f64)> = (0..10)
                    .map(|i| {
                        let r = if i % 2 == 0 { 5.0 } else { 2.0 };
                        let a =
                            std::f64::consts::FRAC_PI_2 + f64::from(i) * std::f64::consts::PI / 5.0;
                        (5.0 + r * a.cos(), 4.5 + r * a.sin())
                    })
                    .collect();
                poly(&mut p, &pts);
            }
            Self::PushPin => {
                rect(&mut p, 2.0, 8.75, 8.0, 10.0);
                rect(&mut p, 3.25, 5.0, 6.75, 8.75);
                rect(&mut p, 0.0, 3.75, 10.0, 5.0);
                poly(&mut p, &[(4.4, 3.75), (5.6, 3.75), (5.0, 0.0)]);
            }
            Self::Graph => {
                stroke(
                    &mut p,
                    &[(1.0, 2.5), (3.75, 5.5), (5.75, 4.0), (9.25, 7.75)],
                    1.3,
                );
                rect(&mut p, 0.0, 0.0, 10.0, 1.25);
            }
            Self::Paperclip => stroke(
                &mut p,
                &[
                    (7.5, 6.5),
                    (3.5, 2.5),
                    (2.5, 3.5),
                    (6.5, 7.5),
                    (8.0, 8.5),
                    (9.0, 7.0),
                    (4.5, 1.5),
                    (1.5, 1.0),
                    (1.0, 4.5),
                    (5.0, 9.0),
                ],
                1.1,
            ),
            Self::Tag => {
                poly(
                    &mut p,
                    &[(0.5, 9.5), (5.0, 9.5), (9.5, 4.8), (5.2, 0.5), (0.5, 5.2)],
                );
                p.extend(
                    Circle::new((2.8, 7.2), 1.0)
                        .to_path(0.01)
                        .reverse_subpaths(),
                );
            }
            Self::Speaker => {
                poly(
                    &mut p,
                    &[
                        (0.0, 2.5),
                        (2.5, 2.5),
                        (4.2, 0.0),
                        (5.0, 0.0),
                        (5.0, 10.0),
                        (4.2, 10.0),
                        (2.5, 7.5),
                        (0.0, 7.5),
                    ],
                );
                ring(&mut p, (6.2, 5.0), 2.4, 1.2);
            }
            Self::Mic => {
                p.extend(kurbo::RoundedRect::new(3.25, 3.5, 6.75, 10.0, 1.75).to_path(0.01));
                stroke(
                    &mut p,
                    &[
                        (1.75, 6.0),
                        (1.75, 5.0),
                        (3.0, 2.75),
                        (5.0, 2.25),
                        (7.0, 2.75),
                        (8.25, 5.0),
                        (8.25, 6.0),
                    ],
                    1.1,
                );
                rect(&mut p, 4.4, 0.0, 5.6, 2.5);
                rect(&mut p, 2.5, 0.0, 7.5, 1.0);
            }
        }
        p.apply_affine(at);
        fail(out.path(&p))?;
        out.op("f");
        Ok(())
    }
}

/// `Caret`: a 20 × 14 pt caret with concave sides, centred on `/Rect`, filled with /C
/// (black without it).
pub(super) fn caret(annot: &Entries<'_>, out: &mut Out) -> Option<()> {
    let r = rect(annot)?;
    let fill = c_color(annot, Color::BLACK);
    let c = r.center();
    let (x, y) = (c.x, c.y);
    out.fill_color(&fill)?;
    out.nums(&[x - 10.0, y - 7.0], "m")?;
    out.nums(&[x, y - 7.0, x, y, x, y + 7.0], "c")?;
    out.nums(&[x, y, x, y - 7.0, x + 10.0, y - 7.0], "c")?;
    out.op("f");
    Some(())
}

/// `Redact`: each quadrilateral outlined in red, 1 pt wide, clipped as viewers clip it
/// to the box of the quadrilaterals a point wider on each side; or `/Rect` less a
/// point without `/QuadPoints`, clipped to `/Rect`.
pub(super) fn redact(annot: &Entries<'_>, out: &mut Out) -> Option<()> {
    // Without quadrilaterals, as with an empty `/QuadPoints`, the box.
    let Some(quads) = quads(annot).filter(|q| !q.all.is_empty()) else {
        // Clipped to the box, as viewers clip it, so that a box under two points
        // wide shows only the part of the outline within it.
        let r = rect(annot)?;
        let inner = r.inset(-1.0);
        out.op("q");
        out.clip(r)?;
        out.op("1 0 0 RG");
        out.stroke_state(1.0, &[])?;
        out.nums(&[inner.x0, inner.y0, inner.width(), inner.height()], "re")?;
        out.op("S");
        out.op("Q");
        return Some(());
    };
    let clip = quads
        .all
        .iter()
        .map(|q| q.bounds(1.0))
        .reduce(|a, b| a.union(b))?;
    out.op("q");
    out.clip(clip)?;
    out.op("1 0 0 RG");
    out.stroke_state(1.0, &[])?;
    for &Quad([tl, tr, bl, br]) in &quads.all {
        out.nums(&[bl.x, bl.y], "m")?;
        for p in [br, tr, tl] {
            out.nums(&[p.x, p.y], "l")?;
        }
        out.op("s");
    }
    out.op("Q");
    Some(())
}

/// The text of each standard stamp name, its lines and size.
fn standard(name: &[u8]) -> Option<(&'static [&'static str], f32)> {
    Some(match name {
        b"Approved" => (&["APPROVED"], 30.0),
        b"Experimental" => (&["EXPERIMENTAL"], 20.0),
        b"NotApproved" => (&["NOT APPROVED"], 20.0),
        b"AsIs" => (&["AS IS"], 30.0),
        b"Expired" => (&["EXPIRED"], 30.0),
        b"NotForPublicRelease" => (&["NOT FOR", "PUBLIC RELEASE"], 18.0),
        b"Confidential" => (&["CONFIDENTIAL"], 20.0),
        b"Final" => (&["FINAL"], 30.0),
        b"Sold" => (&["SOLD"], 30.0),
        b"Departmental" => (&["DEPARTMENTAL"], 20.0),
        b"ForComment" => (&["FOR COMMENT"], 20.0),
        b"TopSecret" => (&["TOP SECRET"], 26.0),
        b"Draft" => (&["DRAFT"], 30.0),
        b"ForPublicRelease" => (&["FOR PUBLIC", "RELEASE"], 18.0),
        _ => return None,
    })
}

/// The text a stamp draws: its lines, set in Times Bold, and the size they are
/// drawn at.
pub(super) struct StampText {
    lines: Vec<Shaped>,
    size: f32,
}

/// The text of stamp `annot`'s `/Name` (`Draft` without one, Table 181): a standard
/// name in its own words and size, another name as it is, in 20 pt.
pub(super) fn stamp_text(annot: &Entries<'_>, fonts: &Fonts) -> Result<StampText, Unsupported> {
    let name = annot.name(b"Name");
    let name = name.as_deref().map_or(&b"Draft"[..], |n| n.as_ref());
    let custom;
    let (lines, size): (&[&str], f32) = match standard(name) {
        Some((lines, size)) => (lines, size),
        None => {
            custom = [String::from_utf8_lossy(name)];
            (&[&*custom[0]], 20.0)
        }
    };
    let lines = lines
        .iter()
        .map(|line| fonts.shape_plain(Typeface::SERIF_BOLD, line))
        .collect::<Result<_, _>>()?;
    Ok(StampText { lines, size })
}

/// `Stamp`: the text of its `/Name` ([`stamp_text`], set once for the document by
/// the entry that holds it), in /C (black without it) in a box of the same colour,
/// clipped to `/Rect`: what of it the page shows, in `visible`.
pub(super) fn stamp<'a>(
    annot: &Entries<'a>,
    doc: &Appearances<'a>,
    visible: kurbo::Rect,
    out: &mut Out,
) -> Drawn {
    // Without a box, viewers draw over coordinates of ±2^32.
    let r = rect(annot).ok_or(Unsupported::Coordinates)?;
    let scale = (r.width() / 190.0).min(r.height() / 50.0);
    if scale <= 0.0 {
        return Ok(None);
    }
    let color = c_color(annot, Color::BLACK);
    let text = doc.stamp_text(annot)?;
    let at = Affine::translate(r.center().to_vec2())
        * Affine::rotate(0.6_f64.to_radians())
        * Affine::scale(scale);
    out.op("q");
    fail(out.clip(r))?;
    fail(out.stroke_color(&color))?;
    fail(out.stroke_state(2.0 * scale, &[]))?;
    let corners = [(-93.0, -22.0), (93.0, -22.0), (93.0, 22.0), (-93.0, 22.0)]
        .map(|(x, y)| at * Point::new(x, y));
    fail(out.nums(&[corners[0].x, corners[0].y], "m"))?;
    for c in &corners[1..] {
        fail(out.nums(&[c.x, c.y], "l"))?;
    }
    out.op("s");
    fail(out.fill_color(&color))?;
    let size = text.size;
    // Baselines from the box's middle.
    let baselines: &[f64] = match (text.lines.len(), size as u32) {
        (2, _) => &[2.0, -15.5],
        (_, 30) => &[-11.0],
        (_, 26) => &[-10.0],
        _ => &[-7.0],
    };
    let set = Setting {
        base: size,
        color,
        align: Align::Left,
    };
    for (line, y) in text.lines.iter().zip(baselines) {
        let x = -line.width(0..line.len(), size) / 2.0;
        text::draw_line(
            &doc.fonts,
            line,
            &set,
            (x, *y),
            at,
            r.intersect(visible),
            out,
        )?;
    }
    out.op("Q");
    Ok(Some(()))
}
