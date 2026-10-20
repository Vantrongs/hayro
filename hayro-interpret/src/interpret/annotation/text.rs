//! Text in generated appearances: glyphs of the standard fonts the font resolver
//! gives, each drawn as a form of its outline ([`super::Glyph`]), so that the
//! appearance needs no font resources and a glyph is written once.
//!
//! Text an entry holds is set once for the document ([`Shaped`]), whatever size,
//! width and box each annotation that shows it lays it out in; each lays out lines
//! only up to the last its box can show, and draws only the glyphs that can show.
//!
//! What a glyph shows is decided once, by [`Pen`], as every glyph drawn passes its
//! test ([`reaches`]). Each search or bound that passes over lines, spans or glyphs
//! without drawing them ([`skips`]) is a test that, in the same rounded arithmetic
//! as the layout's and the pen's, implies the pen's for each of them: what it
//! passes over, the pen would not have drawn. Tests draw with and without them and
//! compare.

use super::entries::operand_real;
use super::out::{Color, Out, Part, Refused, Rounding, corners};
use super::{Unsupported, work};
use crate::FontResolverFn;
use crate::font::blob::{CffFontBlob, OpenTypeFontBlob};
use crate::font::{FontQuery, StandardFont, glyph_name_to_char};
use hayro_syntax::hash;
use kurbo::{Affine, BezPath, Rect};
use skrifa::GlyphId;
use std::cell::{OnceCell, RefCell};
use std::ops::Range;
use std::rc::Rc;

/// The font families generated text is set in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Family {
    /// Helvetica.
    Sans,
    /// Times.
    Serif,
    /// Courier.
    Mono,
    /// The face of rich text that names no family a viewer maps to a standard
    /// font ([`FontQuery::TextDefault`]).
    Text,
    /// `ZapfDingbats`: the check box mark, and text in its built-in encoding.
    Dingbats,
}

impl Family {
    /// The family a default appearance font name stands for, as viewers draw it:
    /// three of the names Acrobat puts in `/DR` by their own family, whatever font
    /// `/DR` gives them, and Helvetica for any other.
    pub(super) fn of_font_name(name: &[u8]) -> Self {
        match name {
            b"TiRo" => Self::Serif,
            b"Cour" => Self::Mono,
            b"ZaDb" => Self::Dingbats,
            _ => Self::Sans,
        }
    }
}

/// A family in a weight and slant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Typeface {
    pub(super) family: Family,
    pub(super) bold: bool,
    pub(super) italic: bool,
}

impl From<Family> for Typeface {
    fn from(family: Family) -> Self {
        Self {
            family,
            bold: false,
            italic: false,
        }
    }
}

impl Typeface {
    /// Times Bold, the stamps' face.
    pub(super) const SERIF_BOLD: Self = Self {
        family: Family::Serif,
        bold: true,
        italic: false,
    };

    /// The number of typefaces, [`Self::index`]'s bound.
    const COUNT: usize = 20;

    fn index(self) -> usize {
        let family = match self.family {
            Family::Sans => 0,
            Family::Serif => 1,
            Family::Mono => 2,
            Family::Text => 3,
            Family::Dingbats => 4,
        };
        family * 4 + usize::from(self.bold) * 2 + usize::from(self.italic)
    }

    fn query(self) -> FontQuery {
        use StandardFont as S;
        let (bold, italic) = (self.bold, self.italic);
        let pick = |faces: [S; 4]| faces[usize::from(bold) * 2 + usize::from(italic)];
        FontQuery::Standard(match self.family {
            Family::Sans => pick([
                S::Helvetica,
                S::HelveticaOblique,
                S::HelveticaBold,
                S::HelveticaBoldOblique,
            ]),
            Family::Serif => pick([
                S::TimesRoman,
                S::TimesItalic,
                S::TimesBold,
                S::TimesBoldItalic,
            ]),
            Family::Mono => pick([
                S::Courier,
                S::CourierOblique,
                S::CourierBold,
                S::CourierBoldOblique,
            ]),
            Family::Text => return FontQuery::TextDefault { bold, italic },
            Family::Dingbats => S::ZapfDingBats,
        })
    }
}

/// The fonts generated text is drawn with: for each typeface, the font the font
/// resolver gives; a character that font lacks is drawn in the Helvetica one.
/// A character neither has makes the appearance unsupported.
///
/// Fonts are loaded on first use; make one per page, or per document, and reuse it.
pub(super) struct Fonts {
    resolver: FontResolverFn,
    faces: [OnceCell<Option<Face>>; Typeface::COUNT],
}

impl Fonts {
    /// The fonts `resolver` gives for the typefaces.
    pub(super) fn new(resolver: FontResolverFn) -> Self {
        Self {
            resolver,
            faces: Default::default(),
        }
    }

    fn face(&self, typeface: Typeface) -> Option<&Face> {
        self.faces[typeface.index()]
            .get_or_init(|| {
                let query = typeface.query();
                let (data, index) = (self.resolver)(&query)?;
                let stands_in = matches!(query, FontQuery::Standard(_));
                Face::new(
                    data,
                    index,
                    query.standard_substitute(),
                    stands_in,
                    typeface.index(),
                )
            })
            .as_ref()
    }

    /// The glyph named `name` in `family`'s font, when that is a bare CFF font as the
    /// embedded standard fonts are.
    pub(super) fn named(&self, family: Family, name: &str) -> Option<Glyph<'_>> {
        let face = self.face(family.into())?;
        let Font::Cff { blob, standard, .. } = &face.font else {
            return None;
        };
        let id = blob.glyph_index_by_name(name)?;
        let advance = standard.get_width(name)? / 1000.0;
        Some(Glyph {
            face,
            id,
            advance,
            space: false,
        })
    }

    /// The box around a glyph's origin the points of every glyph text in `typeface`
    /// can be drawn with lie in, in em: its font's and Helvetica's, for the
    /// characters its font lacks.
    pub(super) fn bound(&self, typeface: Typeface) -> Option<Rect> {
        [typeface, Family::Sans.into()]
            .into_iter()
            .filter_map(|t| self.face(t)?.bound())
            .reduce(|a, b| a.union(b))
    }

    /// The glyphs of `text` in `typeface`, each with its advance in em.
    pub(super) fn shape(
        &self,
        typeface: impl Into<Typeface>,
        text: &str,
    ) -> Result<Vec<Glyph<'_>>, Unsupported> {
        let typeface = typeface.into();
        text.chars().map(|c| self.glyph(typeface, c)).collect()
    }

    /// The glyph of `c` in `typeface`, with its advance in em. Every text drawn is
    /// set through this, so a character it refuses is refused in all of them.
    fn glyph(&self, typeface: Typeface, c: char) -> Result<Glyph<'_>, Unsupported> {
        if reordered(c) {
            return Err(Unsupported::Bidi(c));
        }
        if typeface.family == Family::Dingbats {
            // Viewers write the text's bytes in the font's own encoding; where
            // PDFDocEncoding and Latin-1 agree, a character is its byte.
            // A control character draws nothing and advances as a space (observed).
            let c = if c.is_control() && c != '\0' { ' ' } else { c };
            let code = u8::try_from(c)
                .ok()
                .filter(|b| matches!(b, 0x20..=0x7E | 0xA1..=0xFF));
            return code
                .and_then(|b| StandardFont::ZapfDingBats.code_to_name(b))
                .and_then(|name| self.named(Family::Dingbats, name))
                .map(|g| Glyph {
                    space: c == ' ',
                    ..g
                })
                .ok_or(Unsupported::Character(c));
        }
        // Tabs and other controls take a space; a zero character is drawn otherwise
        // (as a rich text fallback, observed) and is not supported.
        if c == '\0' {
            return Err(Unsupported::Character(c));
        }
        let c = if c.is_control() { ' ' } else { c };
        let sans = Typeface::from(Family::Sans);
        [typeface, sans]
            .into_iter()
            .find_map(|f| {
                let face = self.face(f)?;
                let (id, advance) = face.glyph(c)?;
                Some(Glyph {
                    face,
                    id,
                    advance,
                    space: c == ' ',
                })
            })
            .ok_or(if self.face(sans).is_some() {
                Unsupported::Character(c)
            } else {
                // No fonts at all: nothing is drawable.
                Unsupported::Kind
            })
    }
}

/// Whether text with `c` may be drawn by viewers in an order other than the order
/// it is written in: `c` is of bidirectional class R, AL or AN, or an explicit
/// direction formatting character (embeddings, overrides, isolates and their ends,
/// and the left-to-right mark; the right-to-left and Arabic letter marks are R and
/// AL). Viewers draw such text in visual order (observed in `PyMuPDF` 1.28.2:
/// Hebrew reversed, runs of it among Latin text reversed in place, and Arabic in
/// its joining forms), which is not reproduced.
fn reordered(c: char) -> bool {
    use unicode_bidi::BidiClass::{AL, AN, FSI, LRE, LRI, LRO, PDF, PDI, R, RLE, RLI, RLO};
    c == '\u{200E}'
        || matches!(
            unicode_bidi::bidi_class(c),
            R | AL | AN | LRE | RLE | LRO | RLO | PDF | LRI | RLI | FSI | PDI
        )
}

/// One glyph of a font.
#[derive(Clone)]
pub(super) struct Glyph<'a> {
    face: &'a Face,
    id: GlyphId,
    /// The advance, in em.
    pub(super) advance: f32,
    /// Whether it is a space, where justified lines stretch.
    space: bool,
}

/// The width of `glyphs` at `size`.
pub(super) fn width(glyphs: &[Glyph<'_>], size: f32) -> f32 {
    glyphs.iter().map(|g| g.advance).sum::<f32>() * size
}

/// Fills `glyphs` at `size` with their baseline from `(x, y)`, in `transform`.
pub(super) fn draw(
    glyphs: &[Glyph<'_>],
    size: f32,
    origin: (f32, f32),
    transform: Affine,
    out: &mut Out,
) -> Option<()> {
    if size == 0.0 {
        return Some(());
    }
    let (x0, y) = (f64::from(origin.0), f64::from(origin.1));
    let mut pen = Pen::new(transform, size, y, Rounding::of(1.0, y), EVERYWHERE);
    let mut brush = Brush::new(None);
    let most = glyphs
        .iter()
        .fold(0.0_f64, |m, g| m.max(f64::from(g.advance).abs()))
        * f64::from(size).abs();
    let mut x = x0;
    for (i, g) in glyphs.iter().enumerate() {
        let rounding = Rounding::sum(x0, i as f64, most, (x - x0).abs(), PER_GLYPH);
        pen.put(
            out,
            x,
            rounding,
            (0.0, 0.0),
            &g.face.outline(g.id),
            &mut brush,
        )?;
        x += f64::from(g.advance) * f64::from(size);
    }
    pen.end(out);
    Some(())
}

/// The roundings a viewer takes computing a glyph's advance from its font's
/// width, 1000 to the em, and the size: the width in em, the size (of rich text,
/// a multiple of the base size), the product, and the 32-bit width in em ours is
/// read in, each a rounding of at most the advance.
const PER_GLYPH: f64 = 5.0;

/// How far, in units of a glyph's form (1000 to the em), a glyph is written from
/// the first of its run ([`Pen`]): 2^16, so that the number written is exact to
/// 2^-8 of a unit, well within the 2^23 a number may reach.
const RUN: f64 = 65_536.0;

/// A clip that cuts nothing away.
const EVERYWHERE: Rect = Rect::new(
    f64::NEG_INFINITY,
    f64::NEG_INFINITY,
    f64::INFINITY,
    f64::INFINITY,
);

/// Writes glyphs of one size along a baseline in a frame: the frame, scaled to the
/// size and moved to the first glyph written, once, and each glyph after it that
/// can paint where the page shows it as its form, moved from the first
/// ([`Out::form`]), so that a glyph costs a few bytes of content however often it
/// is drawn, and a viewer composes each glyph's place from two transforms, not
/// from those of every glyph before it. The numbers a viewer computes a glyph's
/// place from are those written, so each must be writable; a glyph is tested by
/// the box of its outline's points, first in the frame against a box there that
/// holds what the clip shows ([`reaches`]), then where it lies, against what the
/// page shows of it under the clips written, wherever a viewer laying the text out
/// in 32-bit numbers may put it ([`Out::form`]): its place across computed as the
/// caller says, its baseline as `y_rounding` says.
pub(super) struct Pen {
    frame: Affine,
    size: f64,
    scale: f64,
    /// The baseline, up the frame, and how it was computed ([`Rounding`], across).
    y: f64,
    y_rounding: Rounding,
    view: Rect,
    /// Where across the first glyph written lies; `None` before it.
    at: Option<f64>,
}

impl Pen {
    /// A pen for glyphs at `size` (not 0) along the baseline `y`, computed as
    /// `y_rounding` says, up `frame`, of which those that can reach `view`, in the
    /// frame, are tested further.
    pub(super) fn new(frame: Affine, size: f32, y: f64, y_rounding: Rounding, view: Rect) -> Self {
        Self {
            frame,
            size: f64::from(size),
            scale: f64::from(size) / 1000.0,
            y,
            y_rounding,
            view,
            at: None,
        }
    }

    /// Writes the glyph of `outline` with its origin `x` across, computed as
    /// `x_rounding` says, when its box can paint where the page shows it, first
    /// tested against the view grown by `grow`, how far across and up a viewer
    /// may put it in the frame; what
    /// `brush` holds back is set before. At a size that is no number, the box's
    /// place cannot be told, and the glyph's numbers cannot be written, which fails
    /// the content, wherever it lies. `None` where the content fails.
    fn put(
        &mut self,
        out: &mut Out,
        x: f64,
        x_rounding: Rounding,
        (gx, gy): (f64, f64),
        outline: &Outline,
        brush: &mut Brush,
    ) -> Option<()> {
        let Some(form) = &outline.form else {
            return Some(());
        };
        let v = self.view;
        let view = Rect::new(v.x0 - gx, v.y0 - gy, v.x1 + gx, v.y1 + gy);
        if self.size.is_finite() && !reaches(outline.em, self.size, (x, self.y), view) {
            return Some(());
        }
        let (s, y) = (self.scale, self.y);
        // What the brush sets is held back until a glyph is drawn ([`Out`]).
        if self.at.is_none() {
            brush.ready(out)?;
        }
        // The glyph's place as computed, in its own space, `s` to a unit of the
        // frame's, and the rounding of the numbers written for it.
        let local = Rounding::point(
            x_rounding.plus(Rounding::of(2.0, x)),
            self.y_rounding.plus(Rounding::of(2.0, y)),
        )
        .scaled(1.0 / s);
        // The first glyph written is placed where it lies, not from the baseline's
        // origin, which may lie as far before it as the text passed over is long;
        // each after it, in a `q`, from the first, as long as it lies within
        // [`RUN`] of it, so that what is written for a glyph is bounded however
        // long the line; one farther starts a run of its own, placed where it lies.
        let far = |from: f64| {
            let units = ((x - from) / s).abs();
            units.is_nan() || units >= RUN
        };
        if self.at.is_some_and(far) {
            out.op("Q");
            self.at = None;
        }
        out.op("q");
        match self.at {
            Some(from) => {
                out.nums(&[1.0, 0.0, 0.0, 1.0, (x - from) / s, 0.0], "cm")?;
                out.form(form, local)?;
                out.op("Q");
            }
            None => {
                let at = self.frame * Affine::new([s, 0.0, 0.0, s, x, y]);
                out.nums(&at.as_coeffs(), "cm")?;
                if out.form(form, local)? {
                    self.at = Some(x);
                } else {
                    out.op("Q");
                }
            }
        }
        Some(())
    }

    /// Ends what the glyphs set.
    pub(super) fn end(self, out: &mut Out) {
        if self.at.is_some() {
            out.op("Q");
        }
    }
}

/// Whether a glyph whose points lie in `em`, in em around its origin, at `size`,
/// its origin at `(x, y)`, can reach `view`, all in one frame: not when its box
/// lies wholly to one side of the view. A bound that passes over glyphs implies
/// this for each by computing the same sums from numbers no nearer the view: a
/// place no nearer, a size no smaller, a box no smaller ([`skips`]). A sum that is
/// no number can reach.
fn reaches(em: Rect, size: f64, (x, y): (f64, f64), view: Rect) -> bool {
    let (x0, x1) = (em.x0 * size, em.x1 * size);
    let (y0, y1) = (em.y0 * size, em.y1 * size);
    let left = x + x0 < view.x0 && x + x1 < view.x0;
    let right = x + x0 > view.x1 && x + x1 > view.x1;
    let below = y + y0 < view.y0 && y + y1 < view.y0;
    let above = y + y0 > view.y1 && y + y1 > view.y1;
    !(left || right || below || above)
}

/// What glyphs drawn so far have set, and what the next glyph written needs set
/// first, the fill colour; so text that writes no glyph writes nothing else
/// either, whatever was passed over to find that.
pub(super) struct Brush {
    color: Option<Color>,
    want: Option<Color>,
}

impl Brush {
    /// A brush with fill colour `color` set.
    pub(super) fn new(color: Option<Color>) -> Self {
        Self { color, want: color }
    }

    /// Sets what the next glyph needs.
    fn ready(&mut self, out: &mut Out) -> Option<()> {
        if let Some(want) = self.want
            && self.color != Some(want)
        {
            out.fill_color(&want)?;
            self.color = Some(want);
        }
        Some(())
    }
}

#[cfg(test)]
thread_local! {
    /// Whether text is drawn without the searches and bounds that pass over what
    /// cannot show, for tests to compare.
    static EXHAUSTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether lines, spans and glyphs that cannot show are passed over by searches
/// and bounds; each such skip is proven by [`reaches`] (see the module), and tests
/// compare drawing with and without them.
fn skips() -> bool {
    #[cfg(test)]
    return !EXHAUSTIVE.with(std::cell::Cell::get);
    #[cfg(not(test))]
    true
}

/// How large text is set: in points, or as a multiple of the size it is laid out at
/// (a default appearance's), as rich text's relative sizes are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Size {
    Points(f32),
    Base(f32),
}

impl Size {
    /// The size at base size `base`.
    pub(super) fn at(self, base: f32) -> f32 {
        match self {
            Self::Points(p) => p,
            Self::Base(k) => base * k,
        }
    }

    /// `n` times this size, as a size relative to it is.
    pub(super) fn times(self, n: f32) -> Self {
        match self {
            Self::Points(p) => Self::Points(n * p),
            Self::Base(k) => Self::Base(n * k),
        }
    }

    /// `n` hundredths of this size.
    pub(super) fn percent(self, n: f32) -> Self {
        match self {
            Self::Points(p) => Self::Points(n * p / 100.0),
            Self::Base(k) => Self::Base(n * k / 100.0),
        }
    }
}

/// The style of a span of [`Shaped`] text: its typeface and size, and its colour
/// and alignment, or, for plain text, those the text is drawn with ([`Setting`]).
#[derive(Clone, Debug, PartialEq)]
struct SpanStyle {
    face: Typeface,
    size: Size,
    color: Option<Color>,
    align: Option<Align>,
}

/// What annotations that share [`Shaped`] text draw it with: the size its sizes are
/// relative to, and the colour and alignment of plain text.
#[derive(Clone, Copy)]
pub(super) struct Setting {
    pub(super) base: f32,
    pub(super) color: Color,
    pub(super) align: Align,
}

/// A paragraph of [`Shaped`] text: its glyphs, and the glyph whose span sizes it
/// when it has none, the line break that ends it or the text's last glyph.
#[derive(Clone)]
struct Paragraph {
    glyphs: Range<usize>,
    sizer: Option<usize>,
}

/// Text in its styles set in its fonts, once for the document however many
/// annotations draw it, at whatever size and in whatever width each lays it out:
/// each glyph's character and advance, and, before each glyph, the sums of the
/// advances of those set in points and of those set at a multiple of the base size,
/// so that any part is measured at once; its paragraphs and words, where lines
/// break ([`Shaped::lines`]). An annotation draws the lines and glyphs its box
/// shows alone, so its work grows with what it shows, not with the text.
pub(super) struct Shaped {
    chars: Vec<char>,
    /// In em.
    advances: Vec<f32>,
    /// The styles of the spans that have glyphs, and the first glyph of each.
    styles: Vec<SpanStyle>,
    starts: Vec<usize>,
    /// The style of a text without glyphs: its first span's.
    empty: Option<SpanStyle>,
    /// Up to the count of glyphs; empty when no span is set so.
    points: Vec<f64>,
    multiples: Vec<f64>,
    /// The glyphs that are spaces justified lines stretch, in order.
    stretch: Vec<usize>,
    paragraphs: Vec<Paragraph>,
    /// Of each paragraph, the first from it on whose size is set in points.
    in_points: Vec<usize>,
    /// The runs of glyphs other than `' '` of each paragraph, in order.
    words: Vec<Range<usize>>,
    /// The spaces that do not follow a glyph other than `' '` of their paragraph:
    /// each is a token of its own where plain text breaks lines ([`Shaped::lines`]).
    lone_spaces: usize,
    /// The advance at size 1 as viewers measure it ([`Shaped::em`]).
    em: f32,
    /// The box around a glyph's origin the points of every glyph lie in, in em.
    ink: Option<Rect>,
    /// The largest advance, in em, either way.
    widest: f64,
    /// Of each span, the largest size in points and the largest multiple of the
    /// base size from it on.
    rest: Vec<(f32, f32)>,
    /// The same of the spans under each node of a tree over them, so that the size
    /// of any run of spans is found by a search ([`Shaped::sizes_in`]) and the spans
    /// a box can show among them by a descent ([`Shaped::reaching`]): node 1 is all
    /// of them, node `n` has children `2n` and `2n + 1`, and span `s` is node
    /// `leaves + s`.
    sizes: Vec<(f32, f32)>,
    leaves: usize,
    /// Whether some glyph is set in points, and some at a multiple of the base size;
    /// each such size is positive (the rich text parser refuses others).
    in_points_any: bool,
    in_multiples_any: bool,
    /// Whether every character is white space.
    blank: bool,
    /// Whether it ends with a carriage return.
    ends_with_cr: bool,
}

/// One line of [`Shaped`] text laid out ([`Shaped::lines`]).
pub(super) struct LaidLine {
    pub(super) glyphs: Range<usize>,
    /// The largest size on it, or, of negative ones, the one furthest from 0; on a
    /// line without glyphs, its paragraph's.
    pub(super) size: f32,
    /// The alignment of its paragraph, but left for the last line of a justified one.
    pub(super) align: Align,
    /// The glyphs it is aligned by: of plain text, all but the spaces after its
    /// last word.
    pub(super) measured: Range<usize>,
    /// The advance of `measured` at the base size.
    pub(super) width: f64,
    /// That advance as a viewer measures it ([`Shaped::walk`]), where the line was
    /// laid out so; else a viewer's is within [`aligned`]'s bound of `width`.
    pub(super) exact: Option<f32>,
    /// Whether where viewers break it cannot be told ([`Shaped::rich_line_end`]):
    /// they may lay it and the lines after it out otherwise ([`draw_box`]).
    pub(super) uncertain: bool,
    /// The least size of a line from its first glyph as any viewer may break it:
    /// `size`, or of an uncertain line, the size of its first word, which every
    /// break keeps on it, and no more glyphs make a line smaller, as where lines
    /// break no size is negative ([`Shaped::direction`]).
    pub(super) least: f32,
}

/// Where a plain line ends ([`Shaped::walk`]).
struct Walked {
    end: usize,
    /// The end of the glyphs the line is aligned by, and their advance as viewers
    /// measure it, where it was measured to the end.
    measured: usize,
    exact: Option<f32>,
}

/// Which way the advances of text laid out at a base size run.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    /// None is negative: lines are broken, and go down.
    Forward,
    /// None is positive, some negative: a paragraph is one line, and lines go up.
    Backward,
    /// All are 0: nothing is drawn.
    Nowhere,
}

impl Fonts {
    /// `text` set in `face`, as plain text: its colour and alignment are those it is
    /// drawn with, its size the base size.
    pub(super) fn shape_plain(
        &self,
        face: impl Into<Typeface>,
        text: &str,
    ) -> Result<Shaped, Unsupported> {
        let style = SpanStyle {
            face: face.into(),
            size: Size::Base(1.0),
            color: None,
            align: None,
        };
        self.shape_spans(text.chars().map(|c| (c, 0)), vec![style])
    }

    /// Rich text `spans`, set as viewers set it.
    pub(super) fn shape_rich(&self, spans: &[Span]) -> Result<Shaped, Unsupported> {
        let styles = spans
            .iter()
            .map(|s| SpanStyle {
                face: s.style.face,
                size: s.style.size,
                color: Some(s.style.color),
                align: Some(s.style.align),
            })
            .collect();
        let chars = spans
            .iter()
            .enumerate()
            .flat_map(|(i, s)| s.text.chars().map(move |c| (c, i)));
        self.shape_spans(chars, styles)
    }

    /// The characters `chars`, each with its span among `styles`, set.
    fn shape_spans(
        &self,
        chars: impl Iterator<Item = (char, usize)>,
        styles: Vec<SpanStyle>,
    ) -> Result<Shaped, Unsupported> {
        let mut s = Shaped {
            chars: Vec::new(),
            advances: Vec::new(),
            styles: Vec::new(),
            starts: Vec::new(),
            empty: styles.first().cloned(),
            points: vec![0.0],
            multiples: vec![0.0],
            stretch: Vec::new(),
            paragraphs: Vec::new(),
            in_points: Vec::new(),
            words: Vec::new(),
            lone_spaces: 0,
            em: 0.0,
            ink: None,
            widest: 0.0,
            rest: Vec::new(),
            sizes: Vec::new(),
            leaves: 0,
            in_points_any: false,
            in_multiples_any: false,
            blank: true,
            ends_with_cr: false,
        };
        let mut last_span = None;
        let (mut points, mut multiples) = (0.0_f64, 0.0_f64);
        let (mut any_points, mut any_multiples) = (false, false);
        for (c, span) in chars {
            let style = &styles[span];
            if last_span != Some(span) {
                s.styles.push(style.clone());
                s.starts.push(s.chars.len());
                last_span = Some(span);
            }
            let g = self.glyph(style.face, c)?;
            work(1);
            let outline = g.face.outline(g.id);
            if !outline.path.elements().is_empty() {
                s.ink = Some(s.ink.map_or(outline.ink, |r| r.union(outline.ink)));
            }
            let advance = f64::from(g.advance);
            match style.size {
                Size::Points(p) => {
                    points += advance * f64::from(p);
                    any_points = true;
                }
                Size::Base(k) => {
                    multiples += advance * f64::from(k);
                    any_multiples = true;
                }
            }
            if g.space {
                s.stretch.push(s.chars.len());
            }
            s.widest = s.widest.max(advance.abs());
            s.em += g.advance;
            s.blank &= c.is_whitespace();
            s.chars.push(c);
            s.advances.push(g.advance);
            s.points.push(points);
            s.multiples.push(multiples);
        }
        (s.in_points_any, s.in_multiples_any) = (any_points, any_multiples);
        if !any_points {
            s.points = Vec::new();
        }
        if !any_multiples {
            s.multiples = Vec::new();
        }
        s.ink = s.ink.map(|r| r.scale_from_origin(0.001));
        s.ends_with_cr = s.chars.last() == Some(&'\r');
        s.split();
        let mut rest = (0.0_f32, 0.0_f32);
        s.rest = vec![(0.0, 0.0); s.styles.len()];
        s.leaves = s.styles.len().next_power_of_two();
        s.sizes = vec![(0.0, 0.0); 2 * s.leaves];
        for (i, style) in s.styles.iter().enumerate().rev() {
            let size = match style.size {
                Size::Points(p) => (p.abs(), 0.0),
                Size::Base(k) => (0.0, k.abs()),
            };
            rest = larger(rest, size);
            s.rest[i] = rest;
            s.sizes[s.leaves + i] = size;
        }
        for n in (1..s.leaves).rev() {
            s.sizes[n] = larger(s.sizes[2 * n], s.sizes[2 * n + 1]);
        }
        Ok(s)
    }
}

/// The larger of two sizes in points and of two multiples of the base size.
fn larger(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    (a.0.max(b.0), a.1.max(b.1))
}

impl Shaped {
    /// Splits the text into paragraphs at line breaks, a CR LF pair one, and those
    /// into words.
    fn split(&mut self) {
        let n = self.chars.len();
        let mut start = 0;
        let mut k = 0;
        while k <= n {
            let end = k == n || matches!(self.chars[k], '\n' | '\r');
            if !end {
                k += 1;
                continue;
            }
            let pair = k + 1 < n && self.chars[k] == '\r' && self.chars[k + 1] == '\n';
            // An empty paragraph is as high as the break that ends it (the line
            // feed of a pair), or, the last, as the text's last glyph.
            let sizer = if k < n {
                Some(if pair { k + 1 } else { k })
            } else {
                n.checked_sub(1)
            };
            let mut w = start;
            while w < k {
                if self.chars[w] == ' ' {
                    if w == start || self.chars[w - 1] == ' ' {
                        self.lone_spaces += 1;
                    }
                    w += 1;
                    continue;
                }
                let from = w;
                while w < k && self.chars[w] != ' ' {
                    w += 1;
                }
                self.words.push(from..w);
            }
            self.paragraphs.push(Paragraph {
                glyphs: start..k,
                sizer,
            });
            k += if pair { 2 } else { 1 };
            start = k;
        }
        let mut next = self.paragraphs.len();
        self.in_points = vec![next; self.paragraphs.len()];
        for p in (0..self.paragraphs.len()).rev() {
            if self.sized_in_points(p) {
                next = p;
            }
            self.in_points[p] = next;
        }
    }

    /// Whether paragraph `p` has a glyph set in points, or, without glyphs, is sized
    /// by one.
    fn sized_in_points(&self, p: usize) -> bool {
        let par = &self.paragraphs[p];
        let points = |g: usize| matches!(self.styles[self.span(g)].size, Size::Points(_));
        if par.glyphs.is_empty() {
            return par.sizer.is_some_and(points);
        }
        let (first, last) = (self.span(par.glyphs.start), self.span(par.glyphs.end - 1));
        (first..=last).any(|s| matches!(self.styles[s].size, Size::Points(_)))
    }

    pub(super) fn len(&self) -> usize {
        self.chars.len()
    }

    /// Whether every character is white space.
    pub(super) fn is_blank(&self) -> bool {
        self.blank
    }

    /// The largest advance, in em, either way.
    pub(super) fn widest(&self) -> f64 {
        self.widest
    }

    /// The advance of glyph `i`, in em.
    pub(super) fn advance(&self, i: usize) -> f32 {
        self.advances[i]
    }

    /// The span glyph `i` is in.
    fn span(&self, i: usize) -> usize {
        self.starts.partition_point(|&s| s <= i) - 1
    }

    /// Where glyph `i` starts, at base size `base`, from the text's first.
    fn x(&self, i: usize, base: f32) -> f64 {
        let at = |v: &Vec<f64>| v.get(i).copied().unwrap_or(0.0);
        at(&self.points) + f64::from(base) * at(&self.multiples)
    }

    /// The advance of glyphs `range` at base size `base`.
    pub(super) fn width(&self, range: Range<usize>, base: f32) -> f64 {
        self.x(range.end, base) - self.x(range.start, base)
    }

    /// The spaces justified lines stretch among glyphs `range`.
    fn stretched(&self, range: Range<usize>) -> usize {
        let count = |i: usize| self.stretch.partition_point(|&s| s < i);
        count(range.end) - count(range.start)
    }

    /// Which way the advances run at base size `base`; [`Unsupported::RichText`]
    /// when some glyphs advance one way and some the other, as with a negative
    /// base size and sizes in points, which no producer writes and whose lines no
    /// search can bound. Sizes in points are positive, multiples of the base size
    /// positive multiples.
    pub(super) fn direction(&self, base: f32) -> Result<Direction, Unsupported> {
        let forward = self.in_points_any || self.in_multiples_any && base > 0.0;
        let backward = self.in_multiples_any && base < 0.0;
        match (forward, backward) {
            (true, true) => Err(Unsupported::RichText),
            (true, false) => Ok(Direction::Forward),
            (false, true) => Ok(Direction::Backward),
            (false, false) => Ok(Direction::Nowhere),
        }
    }

    /// The largest size, either way, of the text from glyph `i` on, at base size
    /// `base`.
    fn rest(&self, i: usize, base: f32) -> f32 {
        if i >= self.chars.len() {
            return 0.0;
        }
        let (points, multiples) = self.rest[self.span(i)];
        points.max(base.abs() * multiples)
    }

    /// The largest size, either way, at base size `base`, and the box around a
    /// glyph's origin the points of every glyph lie in, as far as it reaches up or
    /// down, in em: the bounds of what the text can draw ([`draw_box`]).
    pub(super) fn bound(&self, base: f32) -> (f32, f64) {
        let ink = self.ink.map_or(0.0, |r| r.y0.abs().max(r.y1.abs()));
        (self.rest(0, base), ink)
    }

    /// The most lines the text can be laid out in at any width: a paragraph's
    /// lines each hold a word, or, of plain text, a space that follows none, but
    /// for a last one of the spaces after them.
    fn most_lines(&self) -> usize {
        self.words.len() + self.lone_spaces + self.paragraphs.len()
    }

    /// The advance of the text at size 1 as viewers measure it to fit plain text
    /// to a box: each glyph's advance in em, the font's width over 1000, added in
    /// order in 32-bit numbers (observed: the automatic size of 200 single-line and
    /// comb fields in `PyMuPDF` 1.28.2 is the room over this sum, to the bit).
    pub(super) fn em(&self) -> f32 {
        self.em
    }

    /// The largest size in points and the largest multiple of the base size of
    /// spans `spans`, found by a search of the tree of their sizes.
    fn sizes_in(&self, spans: Range<usize>) -> (f32, f32) {
        let (mut lo, mut hi) = (self.leaves + spans.start, self.leaves + spans.end);
        let mut sizes = (0.0, 0.0);
        while lo < hi {
            work(1);
            if lo % 2 == 1 {
                sizes = larger(sizes, self.sizes[lo]);
                lo += 1;
            }
            if hi % 2 == 1 {
                hi -= 1;
                sizes = larger(sizes, self.sizes[hi]);
            }
            (lo, hi) = (lo / 2, hi / 2);
        }
        sizes
    }

    /// Calls `visit`, in order, with each span of `seek.range`'s glyphs some glyph
    /// of which can reach `seek.view` across, and with others only where the
    /// glyphs' places skip over it, among the spans under `node`, spans `under`.
    /// The spans under a node none of whose glyphs can reach the view at the
    /// largest size among them, as far as a viewer may put them, are passed over
    /// together, so the work grows with the spans that can reach it, not with
    /// those of the range.
    fn reaching(
        &self,
        node: usize,
        under: Range<usize>,
        seek: &Seek<'_>,
        visit: &mut dyn FnMut(usize) -> Result<(), Refused>,
    ) -> Result<(), Refused> {
        let (first, last) = (
            self.starts
                .get(under.start)
                .copied()
                .unwrap_or(self.chars.len()),
            self.starts
                .get(under.end)
                .copied()
                .unwrap_or(self.chars.len()),
        );
        let (first, last) = (first.max(seek.range.start), last.min(seek.range.end));
        if first >= last {
            return Ok(());
        }
        work(1);
        let (points, multiples) = self.sizes[node];
        // No glyph under the node is larger: |base k| <= |base| max k, as rounded.
        let size = f64::from(points.max(seek.base.abs() * multiples));
        if skips() && size.is_finite() {
            if size == 0.0 {
                return Ok(());
            }
            // The glyphs run one way, so their places lie between the first's and
            // the last's; a glyph's points lie within `far` of its place, as its
            // box's corners are found ([`reaches`]): |e s| <= reach size.
            let (a, b) = ((seek.pos)(first), (seek.pos)(last - 1));
            // No glyph under the node is put farther than the last.
            let far = size * seek.reach + (seek.off)(last - 1);
            if a.min(b) - far > seek.view.x1 || a.max(b) + far < seek.view.x0 {
                return Ok(());
            }
        }
        if under.len() == 1 {
            return visit(under.start);
        }
        let middle = under.start + under.len() / 2;
        self.reaching(2 * node, under.start..middle, seek, visit)?;
        self.reaching(2 * node + 1, middle..under.end, seek, visit)
    }

    /// The lines of the text at base size `base`, broken to fit `room` when there
    /// is one, as viewers set them: as plain text ([`Shaped::walk`]), or, `rich`,
    /// as viewers lay out rich text ([`Shaped::rich_line_end`]), whatever the text
    /// was read from. Lines are laid out as they are asked for.
    pub(super) fn lines(
        &self,
        base: f32,
        room: Option<f32>,
        align: Align,
        rich: bool,
    ) -> Lines<'_> {
        Lines {
            text: self,
            base,
            room,
            rich,
            forward: !(self.in_multiples_any && base < 0.0),
            align,
            paragraph: 0,
            at: None,
            last_paragraph: self.paragraphs.len(),
        }
    }

    /// The style of glyph `i`'s span, or of the text without glyphs.
    fn style_at(&self, i: Option<usize>) -> Option<&SpanStyle> {
        match i {
            Some(i) => Some(&self.styles[self.span(i)]),
            None => self.empty.as_ref(),
        }
    }

    /// The size of glyphs `range`: the largest, or, of negative ones, the one
    /// furthest from 0. (Sizes in points are positive, and are never set with a
    /// negative base size: [`Shaped::direction`].)
    fn size_of(&self, range: Range<usize>, base: f32) -> f32 {
        let (points, multiples) =
            self.sizes_in(self.span(range.start)..self.span(range.end - 1) + 1);
        let multiple = base * multiples;
        if multiple.abs() > points {
            multiple
        } else {
            points
        }
    }

    /// Where the line of plain text from glyph `start` of the paragraph ending at
    /// `end` ends, broken to fit `room` as viewers break it (observed in
    /// `PyMuPDF` 1.28.2, for text fields and `FreeText`, glyph for glyph in 3,900
    /// random texts of runs of spaces and paragraph breaks, and to the bit at 800
    /// boundaries): the paragraph is a run of tokens, each the glyphs up to a
    /// space and that space, so that a space after another is a token of its own;
    /// a line takes its first token, then each next one while the line up to that
    /// token's space fits; the next line starts after it, its spaces included.
    ///
    /// A line fits when its advance as viewers measure it is at most `room`:
    /// each glyph's advance in em times the size, both 32-bit numbers, added from
    /// 0 at the line's first glyph in order in 32-bit numbers (to the bit in 640
    /// boundary cases of Helvetica, Times and Courier at sizes from 3 to 40; a
    /// product by 0.001 for the width, a sum of advances in em then scaled, or a
    /// sum in 64 bits each fail 60–75% of them). Advances are not negative where
    /// lines are broken ([`Shaped::direction`]), so the sum does not fall: the
    /// walk stops at the first glyph past `room`, and costs the glyphs of the line
    /// and of the next token up to that glyph, and a token whose space ends it at
    /// once. A token wider than `room` that starts a line is passed whole by a
    /// search of the words.
    fn walk(&self, start: usize, end: usize, room: f32, base: f32) -> Walked {
        let advance = |i: usize| self.advances[i] * base;
        // The end of the glyphs other than `' '` of the token from `i`.
        let word_end = |i: usize| {
            if i >= end || self.chars[i] == ' ' {
                return i;
            }
            let w = self.words.partition_point(|w| w.end <= i);
            self.words.get(w).map_or(end, |w| w.end.min(end))
        };
        // Past its space.
        let token_end = |i: usize| if i < end { i + 1 } else { i };
        // Past the room, or no number.
        let past = |y: f32| y.is_nan() || y > room;
        let mut x = 0.0_f32;
        let mut i = start;
        let (mut measured, mut exact) = (start, None);
        let mut first = true;
        while i < end {
            let we = word_end(i);
            let mut y = x;
            let mut k = i;
            while k < we && !past(y) {
                work(1);
                y += advance(k);
                k += 1;
            }
            if past(y) {
                if first {
                    // The first token is the line's however wide; its width is
                    // not measured to the end.
                    return Walked {
                        end: token_end(we),
                        measured: we,
                        exact: None,
                    };
                }
                break;
            }
            first = false;
            (measured, exact) = (we, Some(y));
            x = y;
            i = we;
            if i < end {
                x += advance(i);
                i += 1;
            }
        }
        Walked {
            end: i,
            measured,
            exact,
        }
    }

    /// Where the first word that ends after glyph `start` ends, which a line of rich
    /// text from `start` holds however it is broken ([`Self::rich_line_end`]).
    fn first_word_end(&self, start: usize) -> Option<usize> {
        let first = self.words.partition_point(|w| w.end <= start);
        self.words.get(first).map(|w| w.end)
    }

    /// Where the line of rich text from glyph `start` of paragraph `par` ends, and
    /// where the next starts, broken to fit `room`: as many words as fit, a word
    /// wider than a line alone on it, the spaces where it breaks on neither.
    /// Viewers lay rich text out by their own measure, which no arithmetic tried
    /// matches to the bit, so a line fits where its advance is within `room` by
    /// more than any sum of the glyphs' advances in 32-bit numbers may round
    /// ([`Rounding::sum`]) and the room more than a few roundings; where it is
    /// nearer, whether it fits cannot be told: the line is broken as its advance
    /// says, and is uncertain ([`LaidLine::uncertain`]) where the other way ends it,
    /// or starts the next, elsewhere.
    fn rich_line_end(
        &self,
        start: usize,
        par: Range<usize>,
        room: f32,
        base: f32,
    ) -> (usize, usize, bool) {
        let room = f64::from(room);
        let most = self.widest * f64::from(self.rest(start, base));
        let fits = |end: usize| {
            let width = self.width(start..end, base);
            let n = (end - start) as f64;
            let margin = Rounding::sum(0.0, n, most, width.abs(), PER_GLYPH)
                .plus(Rounding::of(4.0, room.abs()))
                .bound()
                .x;
            // As its advance says, and whether that may not be what viewers
            // find: not where nothing rounds.
            let fits = width <= room;
            (fits, margin > 0.0 && (width - room).abs() <= margin)
        };
        // The words of the paragraph from the one that ends after `start`.
        let first = self.words.partition_point(|w| w.end <= start);
        let after = self.words.partition_point(|w| w.start < par.end);
        if first >= after {
            // Spaces alone.
            return (par.end, par.end, false);
        }
        let (all, near) = fits(par.end);
        if all && !near {
            return (par.end, par.end, false);
        }
        // The first word always; then as many more as fit.
        let more = partition(after - first - 1, |j| fits(self.words[first + 1 + j].end).0);
        let last = first + more;
        let next = self.words.get(last + 1).filter(|w| w.start < par.end);
        let broken = (self.words[last].end, next.map_or(par.end, |w| w.start));
        // Untold where the last word taken or the next, as their advance says, may
        // fit otherwise: the line ends elsewhere. The words before the last fit
        // more surely, and those after the next fit less.
        let untold =
            (more > 0 && fits(self.words[last].end).1) || next.is_some_and(|w| fits(w.end).1);
        // And where the paragraph may fit otherwise and that ends the line
        // elsewhere: not where it is one word, or words that fit, to its end.
        let untold = untold || (near && broken != (par.end, par.end));
        if all {
            (par.end, par.end, untold)
        } else {
            (broken.0, broken.1, untold)
        }
    }
}

/// The glyphs [`Shaped::reaching`] looks among, `range`, at base size `base`, glyph
/// `k` at `pos(k)` across, each glyph's points within `reach` em of its origin
/// across, and the part of their frame it looks for, `view`.
struct Seek<'p> {
    range: Range<usize>,
    base: f32,
    reach: f64,
    pos: &'p dyn Fn(usize) -> f64,
    /// How far a viewer may put glyph `k` from `pos(k)`: not decreasing with `k`.
    off: &'p dyn Fn(usize) -> f64,
    view: Rect,
}

/// The lines of [`Shaped`] text ([`Shaped::lines`]).
pub(super) struct Lines<'s> {
    text: &'s Shaped,
    base: f32,
    room: Option<f32>,
    /// Whether lines break as rich text's do.
    rich: bool,
    /// Whether no advance is negative, so that lines are broken.
    forward: bool,
    align: Align,
    paragraph: usize,
    /// Where the next line of the paragraph starts; `None` at its start.
    at: Option<usize>,
    /// The paragraphs laid out, up to this one.
    last_paragraph: usize,
}

impl Lines<'_> {
    /// Lays out no paragraph after the one that ends the text, as list options
    /// joined with line breaks lay out a text ending with a carriage return.
    pub(super) fn without_last_paragraph(mut self) -> Self {
        if self.text.ends_with_cr {
            self.last_paragraph -= 1;
        }
        self
    }
}

impl Iterator for Lines<'_> {
    type Item = LaidLine;

    fn next(&mut self) -> Option<LaidLine> {
        let text = self.text;
        if skips() && self.at.is_none() && self.base == 0.0 && self.paragraph < self.last_paragraph
        {
            // Paragraphs without glyphs sized in points are lines 0 high that draw
            // nothing: passed over at once.
            self.paragraph = text.in_points[self.paragraph];
        }
        if self.paragraph >= self.last_paragraph {
            return None;
        }
        work(1);
        let par = &text.paragraphs[self.paragraph];
        let start = self.at.unwrap_or(par.glyphs.start);
        let whole = (par.glyphs.end, par.glyphs.end, par.glyphs.end, None, false);
        let (end, next, measured, exact, uncertain) = match self.room {
            Some(room) if self.forward && start < par.glyphs.end && !self.rich => {
                let w = text.walk(start, par.glyphs.end, room, self.base);
                (w.end, w.end, w.measured, w.exact, false)
            }
            Some(room) if self.forward && start < par.glyphs.end => {
                let (end, next, uncertain) =
                    text.rich_line_end(start, par.glyphs.clone(), room, self.base);
                (end, next, end, None, uncertain)
            }
            _ => whole,
        };
        let last = end == par.glyphs.end && next == par.glyphs.end;
        // The span that sizes and aligns a line without glyphs: its paragraph's first
        // glyph's, or its break's.
        let sizer = if par.glyphs.is_empty() {
            par.sizer
        } else {
            Some(par.glyphs.start)
        };
        let size = if end > start {
            text.size_of(start..end, self.base)
        } else {
            text.style_at(sizer).map_or(0.0, |s| s.size.at(self.base))
        };
        let least = match uncertain.then(|| text.first_word_end(start)).flatten() {
            Some(first) => text.size_of(start..first, self.base),
            None => size,
        };
        let first = if end > start { Some(start) } else { sizer };
        let align = text
            .style_at(first)
            .and_then(|s| s.align)
            .unwrap_or(self.align);
        let align = match align {
            Align::Justify if last => Align::Left,
            align => align,
        };
        if last {
            self.paragraph += 1;
            self.at = None;
        } else {
            self.at = Some(next);
        }
        Some(LaidLine {
            glyphs: start..end,
            size,
            align,
            measured: start..measured,
            width: text.width(start..measured, self.base),
            exact,
            uncertain,
            least,
        })
    }
}

/// The number of indices from 0 below `n` for which `pred`, true for those before
/// some index and false from it on, holds.
fn partition(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        work(1);
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Of items `from..n` along a line, item `k` reaching back to `at(k)`, which does
/// not decrease with `k`, wherever a viewer puts it within `off(k)` of where it
/// lies, `off` not decreasing either: whether each lies wholly past `limit`,
/// `at(k) - off(k) > limit`. Checked by blocks doubling in length from `from`: of
/// a block, `at` is least at its first item and `off` most at its last.
fn past_all(
    from: usize,
    n: usize,
    at: &dyn Fn(usize) -> f64,
    off: &dyn Fn(usize) -> f64,
    limit: f64,
) -> bool {
    let (mut a, mut len) = (from, 1_usize);
    while a < n {
        work(1);
        let b = a.saturating_add(len).min(n);
        let past = at(a) - off(b - 1);
        if past.is_nan() || past <= limit {
            return false;
        }
        a = b;
        len = len.saturating_mul(2);
    }
    true
}

/// Of items `from..n` as [`past_all`] has them, the first from which every one
/// lies wholly past `limit`: those before it are the ones that may reach it.
/// Found by widening `limit` by how far a viewer may put the last item within the
/// widened limit, until that no longer grows, and checked by [`past_all`]; where
/// that does not settle, by the farthest any item may be put, `off(n - 1)`.
fn last_reaching(
    from: usize,
    n: usize,
    at: &dyn Fn(usize) -> f64,
    off: &dyn Fn(usize) -> f64,
    limit: f64,
) -> usize {
    if from >= n {
        return n;
    }
    let within = |g: f64| {
        from + partition(n - from, |j| {
            let past = at(from + j) - g;
            past.is_nan() || past <= limit
        })
    };
    let mut g = off(from);
    for _ in 0..64 {
        let k = within(g);
        let wider = if k < n { off(k) } else { g };
        if wider <= g {
            if past_all(k, n, at, off, limit) {
                return k;
            }
            break;
        }
        g = wider;
    }
    within(off(n - 1))
}

/// Draws glyphs `range` of `text` laid out at `set`, along the baseline from
/// `(x, y)`, computed as `computed` says across and up, in `frame`, each space a
/// justified line stretches `spacing` wider: of those, only the spans and glyphs
/// whose boxes can reach `view` across, a box in the frame, are looked at, by
/// searches, and those drawn that can paint where the page shows them ([`Pen`]).
/// A viewer lays the glyphs out by adding their advances to `x`, in whatever order
/// ([`Rounding::sum`]), so a glyph lies where it is computed within the bound of
/// that sum, and the searches look for glyphs that reach `view` grown by the bound
/// of the line's last glyph and of where a viewer puts a point of the frame
/// ([`frame_slack`]). The work grows with the glyphs drawn, not with the text.
/// `color` is the fill colour set so far.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_glyphs(
    fonts: &Fonts,
    text: &Shaped,
    set: &Setting,
    range: Range<usize>,
    (x, y): (f64, f64),
    computed: (Rounding, Rounding),
    spacing: f64,
    frame: Affine,
    view: Rect,
    out: &mut Out,
    brush: &mut Brush,
) -> Result<(), Refused> {
    let Some(ink) = text.ink else {
        return Ok(());
    };
    if range.is_empty() {
        return Ok(());
    }
    let x0 = text.x(range.start, set.base);
    let pos =
        |k: usize| x + (text.x(k, set.base) - x0) + spacing * text.stretched(range.start..k) as f64;
    // A glyph's advance at the largest size from the line's start, or a space's
    // stretch, is a term of the sum at most this.
    let most = text.widest() * f64::from(text.rest(range.start, set.base)) + spacing.abs();
    let place = |k: usize| {
        let spaces = text.stretched(range.start..k) as f64;
        let total = (text.x(k, set.base) - x0).abs() + spacing.abs() * spaces;
        let n = (k - range.start) as f64 + spaces;
        computed.0.plus(Rounding::sum(x, n, most, total, PER_GLYPH))
    };
    let reach = ink
        .x0
        .abs()
        .max(ink.x1.abs())
        .max(ink.y0.abs())
        .max(ink.y1.abs());
    let farthest = (pos(range.end) - x).abs()
        + x.abs()
        + y.abs()
        + reach * f64::from(text.rest(range.start, set.base));
    let slack = frame_slack(out, frame, farthest);
    // How far a viewer may put glyph `k`, across, and the line, up, from where
    // they lie: not decreasing with `k`.
    let off = |k: usize| place(k).bound().x + slack;
    let up = computed.1.bound().x + slack;
    let mut draw_span = |span: usize| -> Result<(), Refused> {
        let i = text.starts[span].max(range.start);
        let end = text
            .starts
            .get(span + 1)
            .copied()
            .unwrap_or(text.len())
            .min(range.end);
        let style = &text.styles[span];
        let size = style.size.at(set.base);
        if size == 0.0 {
            return Ok(());
        }
        let s = f64::from(size);
        let (a, b) = (ink.x0 * s, ink.x1 * s);
        let (lo, hi) = (a.min(b), a.max(b));
        let n = end - i;
        // The glyphs run right at a positive size, left at a negative one, and the
        // corners of each glyph's box lie between `lo` and `hi` of its place: along
        // the way they run, from where they start, they lie from `near(k)` to
        // `far(k)`, and the view from `v0` to `v1`.
        let way = if size > 0.0 { 1.0 } else { -1.0 };
        let (near, far) = if size > 0.0 { (lo, hi) } else { (-hi, -lo) };
        let (v0, v1) = if size > 0.0 {
            (view.x0, view.x1)
        } else {
            (-view.x1, -view.x0)
        };
        let along = |k: usize| way * pos(k);
        let (first, last) = if !skips() || !s.is_finite() {
            (0, n)
        } else {
            let first = partition(n, |j| along(i + j) + far + off(i + j) < v0);
            let start = |k: usize| along(k) + near;
            let last = last_reaching(i + first, end, &start, &off, v1) - i;
            (first, last)
        };
        if first < last {
            brush.want = Some(style.color.unwrap_or(set.color));
            let mut pen = Pen::new(frame, size, y, computed.1, view);
            for k in i + first..i + last {
                work(1);
                // Set once, the glyph is found again: a font lacking it, which the
                // text was set without, would leave it out where what this draws
                // cannot show.
                let glyph = fonts.glyph(style.face, text.chars[k]);
                let in_view = || Part::near(corners(view).map(|p| frame * p).to_vec(), 0.0);
                let Some(g) = out.part(glyph, in_view)? else {
                    continue;
                };
                let (at, grow) = (pos(k), (off(k), up));
                let put = pen.put(out, at, place(k), grow, &g.face.outline(g.id), brush);
                out.wrote(put)?;
            }
            pen.end(out);
        }
        Ok(())
    };
    let seek = Seek {
        range: range.clone(),
        base: set.base,
        reach: ink.x0.abs().max(ink.x1.abs()),
        pos: &pos,
        off: &off,
        view: Rect::new(view.x0, view.y0 - up, view.x1, view.y1 + up),
    };
    text.reaching(1, 0..text.leaves, &seek, &mut draw_span)
}

/// How far, in `frame`'s units, a viewer computing in 32-bit numbers may put a
/// point of the frame at most `most` from its origin across and up from where it
/// lies ([`Out::slack`]): for a search in the frame to widen what it looks for by.
fn frame_slack(out: &Out, frame: Affine, most: f64) -> f64 {
    let [a, b, c, d, e, f] = frame.as_coeffs();
    let magnitude = (a.abs() + b.abs() + c.abs() + d.abs()) * most + e.abs().max(f.abs());
    // The least the frame stretches a length: |det| over the most.
    let least = (a * d - b * c).abs() / (a * a + b * b + c * c + d * d).sqrt();
    let slack = out.slack(magnitude, Rounding::NONE) / least;
    if slack.is_nan() { f64::INFINITY } else { slack }
}

/// How a viewer computes where a line of `text`, glyphs `range` at base size
/// `base`, starts at `x` when that is aligned by its advance: the advance, a sum
/// of the glyphs' ([`Rounding::sum`]), and a few roundings of the numbers it is
/// taken from, at most `x`, the advance and `room`.
pub(super) fn aligned(
    text: &Shaped,
    range: Range<usize>,
    base: f32,
    x: f64,
    room: f64,
) -> Rounding {
    let width = text.width(range.clone(), base).abs();
    let most = text.widest() * f64::from(text.rest(range.start, base));
    Rounding::sum(0.0, range.len() as f64, most, width, PER_GLYPH)
        .plus(Rounding::of(4.0, x.abs() + width + room.abs()))
}

/// How a viewer computes where `line` of `text` at base size `base` starts at `x`
/// when that is aligned in `room` by its advance: from the advance it measured
/// in laying the line out, in a few roundings of `x`, it and `room`; else as
/// [`aligned`] says.
fn line_start(text: &Shaped, line: &LaidLine, base: f32, x: f64, room: f32) -> Rounding {
    match line.exact {
        Some(width) => Rounding::of(
            4.0,
            x.abs() + f64::from(width).abs() + f64::from(room).abs(),
        ),
        None => aligned(text, line.measured.clone(), base, x, f64::from(room)),
    }
}

/// The starts of the lines of plain text as viewers write them: each line's
/// start from the last's (observed: `PyMuPDF` 1.28.2 writes `dx -13.392 Td` for
/// each line after the first), so that a viewer adds the moves up, each sum a
/// rounding of at most the start it reaches, each move one of at most the move.
#[derive(Default)]
struct Moves {
    at: f64,
    rounding: Rounding,
}

impl Moves {
    /// How a viewer computes the start `x` of the next line, beyond the
    /// roundings of `x` itself.
    fn to(&mut self, x: f64) -> Rounding {
        let step = x - self.at;
        if step != 0.0 {
            self.rounding = self
                .rounding
                .plus(Rounding::of(2.0, x.abs().max(step.abs())));
        }
        self.at = x;
        self.rounding
    }
}

/// Draws `text` on one line, line breaks as spaces, along the baseline from
/// `origin`, computed as `computed` says across and up, in `frame`: the glyphs that
/// can reach `seen`, in the content's space.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_line(
    fonts: &Fonts,
    text: &Shaped,
    set: &Setting,
    origin: (f64, f64),
    computed: (Rounding, Rounding),
    frame: Affine,
    seen: Rect,
    out: &mut Out,
) -> Result<(), Refused> {
    if !(seen.width() >= 0.0 && seen.height() >= 0.0) {
        return Ok(());
    }
    if out
        .part(text.direction(set.base), || Part::rect(seen))?
        .is_none()
    {
        return Ok(());
    }
    let view = frame.inverse().transform_rect_bbox(seen);
    draw_glyphs(
        fonts,
        text,
        set,
        0..text.len(),
        origin,
        computed,
        0.0,
        frame,
        view,
        out,
        &mut Brush::new(Some(set.color)),
    )
}

/// Draws the lines of `text` at `set`'s size broken to fit `avail`, line `j`'s
/// baseline at `top - (j + 1) step` and its start across at `align` of its width
/// (by its width when `by_width`, else at a place of its own): those that can
/// reach `seen`, laid out up to the last that can. `top` is computed as its
/// [`Rounding`] says, and a viewer moves down a line at a time, adding `step`
/// ([`Rounding::sum`]): the searches widen `seen` by where it may put a line.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_rows(
    fonts: &Fonts,
    text: &Shaped,
    set: &Setting,
    avail: f32,
    (top, top_rounding): (f64, Rounding),
    step: f64,
    align: impl Fn(f32) -> f32,
    by_width: bool,
    seen: Rect,
    out: &mut Out,
) -> Result<(), Refused> {
    let size = set.base;
    let Some(ink) = text.ink else {
        return Ok(());
    };
    if size == 0.0 || !(seen.width() >= 0.0 && seen.height() >= 0.0) {
        return Ok(());
    }
    // How far a glyph's points lie from its baseline, up or down, as the pen finds
    // them ([`reaches`]): |e s| <= reach.
    let reach = ink.y0.abs().max(ink.y1.abs()) * f64::from(size.abs());
    let baseline = |j: usize| top - (j as f64 + 1.0) * step;
    let lines = text.most_lines();
    let level = |j: usize| {
        let n = j as f64 + 1.0;
        top_rounding.plus(Rounding::sum(top, n, step, n * step.abs(), 2.0))
    };
    let far = top.abs() + lines as f64 * step.abs() + reach + f64::from(avail).abs();
    let slack = out.slack(far, Rounding::NONE);
    // How far a viewer may put line `j`, and the glyphs on it, from where it lies.
    let off = |j: usize| level(j).bound().x + slack;
    // The text makes no more lines than it can at any width, and its baselines, as
    // computed, run one way: none is laid out when the last it can make lies
    // wholly before the view.
    let last = baseline(lines - 1);
    let r = reach + off(lines);
    let before = (step > 0.0 && last - r > seen.y1) || (step < 0.0 && last + r < seen.y0);
    if skips() && before {
        return Ok(());
    }
    let mut brush = Brush::new(Some(set.color));
    let mut moves = Moves::default();
    for (j, line) in text
        .lines(size, Some(avail), Align::Left, false)
        .enumerate()
    {
        let y = baseline(j);
        let r = reach + off(j);
        let near = !(y + r < seen.y0 || y - r > seen.y1);
        // Viewers move to each line's start from the last's ([`Moves`]).
        let x = f64::from(align(line.exact.unwrap_or(line.width as f32)));
        let moved = moves.to(x);
        if !skips() || (!line.glyphs.is_empty() && near) {
            let across = if by_width {
                line_start(text, &line, size, x, avail).plus(moved)
            } else {
                Rounding::of(2.0, x)
            };
            draw_glyphs(
                fonts,
                text,
                set,
                line.glyphs,
                (x, y),
                (across, level(j)),
                0.0,
                Affine::IDENTITY,
                seen,
                out,
                &mut brush,
            )?;
        }
        // The lines after it lie from the next on, down at a positive size, up at a
        // negative one, each as far as a viewer may put it.
        let past = if step > 0.0 {
            let top = |k: usize| -(baseline(k) + reach);
            past_all(j + 1, lines, &top, &off, -seen.y0)
        } else if step < 0.0 {
            let bottom = |k: usize| baseline(k) - reach;
            past_all(j + 1, lines, &bottom, &off, seen.y1)
        } else {
            false
        };
        if skips() && past {
            break;
        }
    }
    Ok(())
}

/// Draws the first `count` glyphs of `text` at `set`'s size, glyph `i` centred in
/// cell `i` of cells `cell` wide from 0, with its baseline at `y`, computed as its
/// [`Rounding`] says: those whose cells can reach `seen`, wherever a viewer
/// computing a cell's place in 32-bit numbers puts it ([`cell_place`]).
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_comb(
    fonts: &Fonts,
    text: &Shaped,
    set: &Setting,
    count: usize,
    cell: f32,
    (y, y_rounding): (f64, Rounding),
    seen: Rect,
    out: &mut Out,
) -> Result<(), Refused> {
    let size = set.base;
    if size == 0.0 || count == 0 || !(seen.width() >= 0.0 && seen.height() >= 0.0) {
        return Ok(());
    }
    let start = |i: usize| cell * i as f32;
    let middle = |advance: f32| (cell - advance * size) / 2.0;
    let at = |i: usize| f64::from(start(i) + middle(text.advance(i)));
    let glyph = f64::from(size).abs() * text.widest();
    let place = |i: usize| cell_place(i as f64, f64::from(cell), glyph);
    let far = f64::from(cell).abs() * count as f64 + glyph + y.abs();
    let (gx, gy) = (
        place(count).bound().x + out.slack(far, Rounding::NONE),
        y_rounding.bound().x + out.slack(far, Rounding::NONE),
    );
    let seen = Rect::new(seen.x0 - gx, seen.y0 - gy, seen.x1 + gx, seen.y1 + gy);
    let face = text.styles[0].face;
    let mut pen = Pen::new(Affine::IDENTITY, size, y, y_rounding, seen);
    let mut brush = Brush::new(None);
    let cells = if skips() {
        cells(text, size, cell, &start, &middle, count, seen)
    } else {
        0..count
    };
    for i in cells {
        work(1);
        // Found again as it was set ([`draw_glyphs`]).
        let Some(g) = out.part(fonts.glyph(face, text.chars[i]), || Part::rect(seen))? else {
            continue;
        };
        let put = pen.put(
            out,
            at(i),
            place(i),
            (0.0, 0.0),
            &g.face.outline(g.id),
            &mut brush,
        );
        out.wrote(put)?;
    }
    pen.end(out);
    Ok(())
}

/// How a viewer computes where cell `i` of cells `cell` wide, or what lies in it
/// at most `extra` from its start, lies across: the cell's width times `i`, and a
/// few sums and products, each of at most the place's magnitude.
pub(super) fn cell_place(i: f64, cell: f64, extra: f64) -> Rounding {
    Rounding::of(5.0, (cell * i).abs() + cell.abs() + extra.abs())
}

/// Of the first `count` glyphs of `text` at `size`, glyph `i` at `start(i)` and
/// `middle` of its advance across, those that can reach `clip` across. Each sum
/// and product rounds no nearer than its operands lie, so glyph `i` lies between
/// where glyphs of the advances either side of every one would, places that run
/// one way as `i` grows, as cells do (`cell` is not negative): a glyph found to lie
/// wholly to one side so lies there as the pen finds it ([`reaches`]).
fn cells(
    text: &Shaped,
    size: f32,
    cell: f32,
    start: &dyn Fn(usize) -> f32,
    middle: &dyn Fn(f32) -> f32,
    count: usize,
    clip: Rect,
) -> Range<usize> {
    let Some((x0, x1, _, _)) = scaled(text.ink, size) else {
        return 0..0;
    };
    let widest = text.widest() as f32;
    let (a, b) = (middle(-widest), middle(widest));
    let (lo, hi) = (a.min(b), a.max(b));
    if !(cell >= 0.0 && [lo, hi].iter().all(|v| v.is_finite()) && x0.is_finite() && x1.is_finite())
    {
        return 0..count;
    }
    let first = partition(count, |i| f64::from(start(i) + hi) + x1 < clip.x0);
    let last = partition(count, |i| f64::from(start(i) + lo) + x0 <= clip.x1);
    first..last.max(first)
}

/// Draws options, `count` of them, each the text `label(i)` gives, set in `face`
/// and drawn at `set`, option `i` on one line from `pad` across with its baseline
/// at `top - (i + 1) step`: those whose lines can reach `seen`, found by searches,
/// the others not made. `pad` and `top` are computed as their [`Rounding`]s say,
/// and a viewer moves down an option at a time, adding `step` ([`Rounding::sum`]):
/// the searches widen `seen` by where it may put the last.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_options(
    fonts: &Fonts,
    label: impl Fn(usize) -> Result<Rc<Shaped>, Unsupported>,
    count: usize,
    face: Typeface,
    set: &Setting,
    (pad, pad_rounding): (f64, Rounding),
    (top, top_rounding): (f64, Rounding),
    step: f64,
    seen: Rect,
    out: &mut Out,
) -> Result<(), Refused> {
    if !(seen.width() >= 0.0 && seen.height() >= 0.0) {
        return Ok(());
    }
    let baseline = |i: usize| top - (i as f64 + 1.0) * step;
    let level = |i: usize| {
        let n = i as f64 + 1.0;
        top_rounding.plus(Rounding::sum(top, n, step, n * step.abs(), 2.0))
    };
    let far = top.abs() + count as f64 * step.abs() + f64::from(set.base).abs();
    let slack = out.slack(far, Rounding::NONE);
    // How far a viewer may put option `i` from where it lies: not decreasing.
    let off = |i: usize| level(i).bound().x + slack;
    // The points of every glyph of an option lie within the font's box ([`Fonts::bound`]),
    // and the baselines, as computed, run down at a positive step and up at a
    // negative one: an option whose box lies wholly above or below the view, as
    // far as a viewer may put it, draws nothing ([`reaches`]).
    let ink = scaled(fonts.bound(face), set.base);
    let rows = match ink {
        _ if !skips() => 0..count,
        None => 0..0,
        Some((_, _, y0, y1)) if y0.is_finite() && y1.is_finite() => {
            if step >= 0.0 {
                let first = partition(count, |i| baseline(i) + y0 - off(i) > seen.y1);
                let top = |i: usize| -(baseline(i) + y1);
                first..last_reaching(first, count, &top, &off, -seen.y0)
            } else {
                let first = partition(count, |i| baseline(i) + y1 + off(i) < seen.y0);
                let bottom = |i: usize| baseline(i) + y0;
                first..last_reaching(first, count, &bottom, &off, seen.y1)
            }
        }
        Some(_) => 0..count,
    };
    for i in rows {
        // An option that cannot be set draws within its line: across what shows,
        // up and down as far as the font's glyphs reach from its baseline, wherever
        // a viewer puts it, or anywhere a size of no number puts it.
        let line = || match ink {
            Some((_, _, y0, y1)) if y0.is_finite() && y1.is_finite() => {
                let (lo, hi) = (y0.min(y1), y0.max(y1));
                let y = baseline(i);
                Part::rect(Rect::new(
                    seen.x0,
                    y + lo - off(i),
                    seen.x1,
                    y + hi + off(i),
                ))
            }
            _ => Part::rect(seen),
        };
        let Some(label) = out.part(label(i), line)? else {
            continue;
        };
        draw_line(
            fonts,
            &label,
            set,
            (pad, baseline(i)),
            (pad_rounding, level(i)),
            Affine::IDENTITY,
            seen,
            out,
        )?;
    }
    Ok(())
}

/// Where laid out text is drawn ([`draw_box`]): its lines from `pad` across, the
/// top of the first at `top` and each line 1.2 times its largest size below the one
/// before, as many as fit together in `room` (lines beyond are not drawn), in a box
/// that `frame` maps to the content's space, under a clip to `clip`, in that space,
/// written before anything of the text is told ([`TextBox::enter`]).
#[derive(Clone, Copy)]
pub(super) struct TextBox {
    /// Whether lines break as rich text's do ([`Shaped::lines`]).
    pub(super) rich: bool,
    pub(super) pad: f32,
    pub(super) top: f32,
    pub(super) room: f32,
    pub(super) frame: Affine,
    pub(super) clip: Rect,
}

/// A [`TextBox`] whose clip is written: a part of its text told from here on is
/// told under it, as it is drawn ([`Out::refuse`]).
pub(super) struct Entered(TextBox);

impl TextBox {
    /// Writes the clip of this box, which all its text is drawn under.
    pub(super) fn enter(self, out: &mut Out) -> Result<Entered, Refused> {
        out.writes(|out| out.clip(self.clip))?;
        Ok(Entered(self))
    }
}

/// Draws texts `pieces`, each made when its lines are reached, as one text whose
/// paragraphs are theirs (as list options joined with line breaks are), laid out
/// at `set` in `avail`, each line's first baseline `ascent` (not negative) times its
/// size below its top, in `area`: the lines that can reach both its clip and
/// `seen`, laid out up to the last that can, and of the last piece none when none
/// can. The sizes and glyph boxes of every piece are within `bound`, in points and
/// in em around a glyph's origin, either way ([`Shaped::bound`]): pieces drawn 0
/// high are not made. A viewer moves down a line at a time, adding its height
/// ([`Rounding::sum`]), so the lines are looked for within how far it may put
/// them, and a line's glyphs are drawn wherever it may put them ([`Pen`]).
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_box(
    fonts: &Fonts,
    pieces: impl Iterator<Item = Result<Rc<Shaped>, Unsupported>>,
    bound: (f32, f64),
    set: &Setting,
    avail: f32,
    ascent: f32,
    area: &Entered,
    seen: Rect,
    out: &mut Out,
) -> Result<(), Refused> {
    let area = &area.0;
    let shown = area.clip.intersect(seen);
    if !(shown.width() >= 0.0 && shown.height() >= 0.0) {
        return Ok(());
    }
    // A size of 0 draws nothing (and needs nothing checked: such text is never
    // refused for the way it runs, [`Shaped::direction`]).
    if skips() && bound.0 == 0.0 {
        return Ok(());
    }
    // Whether what is laid out from here on cannot show, whichever way viewers
    // break its lines: the searches pass over it, and a line that may break
    // otherwise there refuses nothing.
    let mut gone = bound.0 == 0.0;
    let view = area.frame.inverse().transform_rect_bbox(shown);
    let (pad, top) = (f64::from(area.pad), area.top);
    // How far a viewer may put the baseline of a line `below` the top, the
    // `j`-th, in the frame: the sum of the heights above it, each at most
    // `tallest`, and a point of the frame ([`frame_slack`]).
    let tallest = f64::from(bound.0).abs() * (1.2 + f64::from(ascent).abs());
    let far =
        f64::from(top).abs() + pad.abs() + f64::from(avail).abs() + f64::from(area.room).abs();
    let slack = frame_slack(out, area.frame, far + tallest);
    let level = |j: usize, below: f64| {
        let n = j as f64 + 1.0;
        Rounding::of(4.0, f64::from(top)).plus(Rounding::sum(
            f64::from(top),
            n,
            tallest,
            below.abs() + tallest,
            2.0,
        ))
    };
    let off = |j: usize, below: f64| level(j, below).bound().x + slack;
    let mut brush = Brush::new(None);
    let mut moves = Moves::default();
    let (mut height, mut filled) = (0.0_f32, f32::NEG_INFINITY);
    let mut way = None;
    let mut laid = 0_usize;
    let mut pieces = pieces.peekable();
    // A piece that cannot be set, or laid out the way the text before it runs,
    // viewers lay out by rules not reproduced anywhere in the box: refused where
    // the box may show, and left out (with all the box draws) where it cannot.
    let clipped = || Part::rect(area.clip);
    while let Some(text) = pieces.next() {
        let Some(text) = out.part(text, clipped)? else {
            continue;
        };
        let Some(direction) = out.part(text.direction(set.base), clipped)? else {
            continue;
        };
        if direction != Direction::Nowhere {
            if way.is_some_and(|w| w != direction) {
                out.refuse(Unsupported::RichText, clipped())?;
                continue;
            }
            way = Some(direction);
        }
        let mut lines = text.lines(set.base, Some(avail), set.align, area.rich);
        let last = pieces.peek().is_none();
        // The pieces after this one, whose sizes and glyphs are within the bound.
        let later = if last {
            (0.0, 0.0)
        } else {
            lines = lines.without_last_paragraph();
            bound
        };
        let ink = text.ink.map_or(0.0, |r| r.y0.abs().max(r.y1.abs()));
        // The last piece: none of its lines is laid out when none can reach the view.
        if last {
            let most = text.most_lines();
            let depth = Depth {
                top,
                height,
                lines: most,
                size: text.rest(0, set.base),
                ascent,
                off: off(
                    laid + most,
                    (laid + most) as f64 * tallest + f64::from(height),
                ),
            };
            if depth.cannot_reach(direction, ink, view) {
                if skips() {
                    return Ok(());
                }
                gone = true;
            }
        }
        for line in lines {
            // A line that does not fit in the box however a viewer breaks it draws
            // nothing, nor do those after it: where it breaks changes nothing.
            if filled.max(height + 1.2 * line.least) > area.room {
                return Ok(());
            }
            if line.uncertain && !gone {
                // Viewers may break this line otherwise, and lay it and every line
                // after it out otherwise: anywhere in the box from its top down,
                // and above that as far as the glyphs from its first on reach
                // above the top of their line, wherever a viewer puts it (as the
                // lines past the view are passed over, below).
                let rest = f64::from(text.rest(line.glyphs.start, set.base).max(later.0));
                let up = rest * ink.max(later.1)
                    + off(
                        laid + text.most_lines(),
                        f64::from(height).abs() + text.most_lines() as f64 * tallest,
                    );
                let clip = area.frame.inverse().transform_rect_bbox(area.clip);
                let y1 = match direction {
                    Direction::Forward => clip.y1.min(f64::from(top - height) + up),
                    Direction::Backward | Direction::Nowhere => clip.y1,
                };
                if y1.partial_cmp(&clip.y0).is_none_or(|o| o.is_ge()) {
                    let differ = [
                        (clip.x0, clip.y0),
                        (clip.x1, clip.y0),
                        (clip.x1, y1),
                        (clip.x0, y1),
                    ]
                    .map(|(x, y)| area.frame * kurbo::Point::new(x, y));
                    out.refuse(Unsupported::Measure, Part::near(differ.to_vec(), 0.0))?;
                }
            }
            let below = height + ascent * line.size;
            height += 1.2 * line.size;
            filled = filled.max(height);
            if filled > area.room {
                // This line and those after it do not fit.
                return Ok(());
            }
            let y = f64::from(top - below);
            let reach = ink * f64::from(line.size.abs()) + off(laid, f64::from(below));
            let near = line.size != 0.0
                && !line.glyphs.is_empty()
                && !(y + reach < view.y0 || y - reach > view.y1);
            let width = line.exact.unwrap_or(line.width as f32);
            let x0 = match line.align {
                Align::Left => 0.0,
                Align::Center => (avail - width) / 2.0,
                Align::Right => avail - width,
                Align::Justify => 0.0,
            };
            let x = pad + f64::from(x0);
            // Plain text: viewers move to each line's start from the last's.
            let moved = if area.rich {
                Rounding::NONE
            } else {
                moves.to(x)
            };
            if near || !skips() {
                let spaces = text.stretched(line.glyphs.clone());
                let spacing = match line.align {
                    Align::Justify if spaces > 0 => ((avail - width) / spaces as f32).max(0.0),
                    _ => 0.0,
                };
                // A line aligned or stretched by its advance starts, or stretches,
                // where a viewer measures it.
                let across = match line.align {
                    Align::Left => Rounding::of(2.0, x),
                    _ => line_start(&text, &line, set.base, x, avail),
                }
                .plus(moved);
                draw_glyphs(
                    fonts,
                    &text,
                    set,
                    line.glyphs.clone(),
                    (x, y),
                    (across, level(laid, f64::from(below))),
                    f64::from(spacing),
                    area.frame,
                    view,
                    out,
                    &mut brush,
                )?;
            }
            laid += 1;
            // The lines after this one lie below the top of the next, at a positive
            // size, or above it, at a negative one (the next adds a height and an
            // ascent of its sign, and a rounded sum lies no nearer than its terms),
            // by no more than their glyphs reach, as far as a viewer may put them.
            let rest = f64::from(text.rest(line.glyphs.end, set.base).max(later.0));
            let next = f64::from(top - height);
            let reach = rest * ink.max(later.1)
                + off(
                    laid + text.most_lines(),
                    f64::from(height).abs() + text.most_lines() as f64 * tallest,
                );
            let past = match direction {
                Direction::Forward => next + reach < view.y0,
                Direction::Backward => next - reach > view.y1,
                Direction::Nowhere => false,
            };
            if past {
                if skips() {
                    return Ok(());
                }
                gone = true;
            }
        }
    }
    Ok(())
}

/// Where the lines of the last text [`draw_box`] lays out can lie: from `height`
/// below `top`, at most `lines` of them, each sized at most `size` either way, its
/// height 1.2 times its size and its baseline `ascent` times its size below its top.
struct Depth {
    top: f32,
    height: f32,
    lines: usize,
    size: f32,
    ascent: f32,
    /// How far a viewer may put a line from where it is computed.
    off: f64,
}

impl Depth {
    /// Whether no line can reach `view` with glyphs whose points lie within `ink`
    /// times its size of its baseline, the lines leaving the view behind as they
    /// go `direction`, as [`draw_box`] computes their baselines and the pen tests
    /// their glyphs ([`reaches`]), and `off` farther, where a viewer may put them.
    ///
    /// Heights add up in 32-bit floats, each sum and product rounded to within
    /// u = 2^-24 of itself, or e = 2^-149 among subnormal numbers:
    /// |r(x)| <= |x| (1 + u) + e. After k lines, each r(1.2 s) <= t = r(1.2 size)
    /// high, |height| <= (|h| + k (t + e)) (1 + u)^k; a baseline lies that and
    /// r(ascent size) more below the top, rounded, and is rounded once more as it
    /// is taken from the top. The bound is computed in 64-bit floats with u doubled
    /// and (1 + u)^k as exp(k u), which leaves more room than their own rounding
    /// takes, so it never falls short of where the lines can lie.
    fn cannot_reach(&self, direction: Direction, ink: f64, view: Rect) -> bool {
        // Twice u, and e.
        const U: f64 = 1.0 / 8_388_608.0;
        const E: f64 = 1.401_298_464_324_817e-45;
        let n = self.lines as f64;
        let t = f64::from(1.2 * self.size);
        let ascent = f64::from(self.ascent.abs() * self.size);
        let height = (f64::from(self.height).abs() + n * (t + E)) * (n * U).exp();
        let depth = (height + ascent) * (1.0 + U) + E;
        let top = f64::from(self.top);
        let far = depth + U * (top.abs() + depth) + E;
        let reach = ink * f64::from(self.size) * (1.0 + U) + self.off;
        // Beyond, a height may round to no number, and the lines to anywhere.
        if !(far < f64::from(f32::MAX) && reach.is_finite()) {
            return false;
        }
        match direction {
            Direction::Forward => top - far - reach > view.y1,
            Direction::Backward => top + far + reach < view.y0,
            Direction::Nowhere => true,
        }
    }
}

/// `ink`, a box in em around a glyph's origin, at `size`, as `(x0, x1, y0, y1)`.
fn scaled(ink: Option<Rect>, size: f32) -> Option<(f64, f64, f64, f64)> {
    let ink = ink?;
    let s = f64::from(size);
    let (x0, x1) = (ink.x0 * s, ink.x1 * s);
    let (y0, y1) = (ink.y0 * s, ink.y1 * s);
    Some((x0.min(x1), x0.max(x1), y0.min(y1), y0.max(y1)))
}

/// A standard font's glyphs, each outline read once, and the typeface it was loaded
/// for, which names its glyphs' forms.
struct Face {
    font: Font,
    index: usize,
    outlines: RefCell<hash::Map<GlyphId, Rc<Outline>>>,
    /// The box the points of every glyph lie in, 1000 units to the em.
    bound: OnceCell<Option<Rect>>,
}

/// A glyph's outline, 1000 units to the em, the box of its points, that box in em
/// (scaled as [`Shaped`] and [`Face::bound`] scale theirs, so that theirs hold it in
/// the same numbers), and the form it is drawn as (`None` when it has no points).
struct Outline {
    path: BezPath,
    ink: Rect,
    em: Rect,
    form: Option<Rc<super::Glyph>>,
}

/// A bare CFF font, as the embedded standard fonts are, read by glyph name with the
/// standard font's metrics, or an OpenType font, read by its character map.
enum Font {
    Cff {
        blob: CffFontBlob,
        standard: StandardFont,
        glyphs: hash::Map<char, (GlyphId, f32)>,
    },
    OpenType {
        blob: OpenTypeFontBlob,
        /// For a font standing in for a standard font, the standard font's widths
        /// of the glyphs it names, in em.
        standard: Option<hash::Map<GlyphId, f32>>,
    },
}

impl Face {
    fn new(
        data: crate::font::FontData,
        index: u32,
        standard: StandardFont,
        stands_in: bool,
        typeface: usize,
    ) -> Option<Self> {
        Some(Self {
            font: Font::new(data, index, standard, stands_in)?,
            index: typeface,
            outlines: RefCell::default(),
            bound: OnceCell::new(),
        })
    }

    /// The box the points of every glyph of the font lie in, in em; `None` when no
    /// glyph has any.
    fn bound(&self) -> Option<Rect> {
        *self.bound.get_or_init(|| {
            let count = match &self.font {
                Font::Cff { blob, .. } => blob.num_glyphs(),
                Font::OpenType { blob, .. } => {
                    use skrifa::raw::TableProvider;
                    blob.font_ref()
                        .maxp()
                        .map_or(0, |m| u32::from(m.num_glyphs()))
                }
            };
            (0..count)
                .map(|id| self.font.outline(GlyphId::new(id)))
                .filter(|p| !p.elements().is_empty())
                .map(|p| p.control_box())
                .reduce(|a, b| a.union(b))
                .map(|r| r.scale_from_origin(0.001))
        })
    }

    fn glyph(&self, c: char) -> Option<(GlyphId, f32)> {
        self.font.glyph(c)
    }

    /// The outline of glyph `id`.
    fn outline(&self, id: GlyphId) -> Rc<Outline> {
        let mut outlines = self.outlines.borrow_mut();
        outlines
            .entry(id)
            .or_insert_with(|| {
                let path = self.font.outline(id);
                let ink = path.control_box();
                let form = (!path.elements().is_empty())
                    .then(|| {
                        super::Glyph::new(format!("G{}_{}", self.index, id.to_u32()), &path, ink)
                    })
                    .flatten()
                    .map(Rc::new);
                let em = ink.scale_from_origin(0.001);
                Rc::new(Outline {
                    path,
                    ink,
                    em,
                    form,
                })
            })
            .clone()
    }
}

impl Font {
    /// The font in `data`; `stands_in` when it stands in for `standard`, whose
    /// widths viewers measure text by.
    fn new(
        data: crate::font::FontData,
        index: u32,
        standard: StandardFont,
        stands_in: bool,
    ) -> Option<Self> {
        if let Some(blob) = CffFontBlob::new(data.clone()) {
            let mut glyphs = hash::Map::default();
            for (id, name) in blob.glyph_names() {
                let (Some(c), Some(width)) = (glyph_name_to_char(&name), standard.get_width(&name))
                else {
                    continue;
                };
                glyphs.entry(c).or_insert((id, width / 1000.0));
            }
            return Some(Self::Cff {
                blob,
                standard,
                glyphs,
            });
        }
        let blob = OpenTypeFontBlob::new(data, index)?;
        let standard = stands_in.then(|| {
            blob.glyph_names()
                .into_iter()
                .filter_map(|(name, id)| Some((id, standard.get_width(&name)? / 1000.0)))
                .collect()
        });
        Some(Self::OpenType { blob, standard })
    }

    /// The glyph of `c` and its advance in em.
    fn glyph(&self, c: char) -> Option<(GlyphId, f32)> {
        match self {
            Self::Cff { glyphs, .. } => glyphs.get(&c).copied(),
            Self::OpenType { blob, standard } => {
                use skrifa::raw::TableProvider;
                let id = blob.font_ref().cmap().ok()?.map_codepoint(c)?;
                if id == GlyphId::NOTDEF {
                    return None;
                }
                let advance = blob.glyph_metrics().advance_width(id)?;
                Some((
                    id,
                    match standard {
                        // Viewers' standard fonts have the standard widths, and
                        // whole thousandths of an em for the glyphs those lack
                        // (`a_standard_font_stand_in_measures_as_viewers_do`).
                        Some(widths) => widths
                            .get(&id)
                            .copied()
                            .unwrap_or_else(|| advance.round() / 1000.0),
                        None => advance / 1000.0,
                    },
                ))
            }
        }
    }

    /// The outline of glyph `id`, 1000 units to the em.
    fn outline(&self, id: GlyphId) -> BezPath {
        match self {
            Self::Cff { blob, .. } => blob.outline_glyph(id),
            Self::OpenType { blob, .. } => blob.outline_glyph(id),
        }
    }
}

/// How rich text is set: typeface, size, the fill colour, and how the lines of its
/// paragraph are aligned.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Style {
    pub(super) face: Typeface,
    pub(super) size: Size,
    pub(super) color: Color,
    pub(super) align: Align,
}

impl Style {
    /// This style as a key: its numbers by their bits, so that styles of the same
    /// numbers are one key and others are not.
    pub(super) fn key(&self) -> StyleKey {
        let size = match self.size {
            Size::Points(p) => (false, p.to_bits()),
            Size::Base(k) => (true, k.to_bits()),
        };
        let color = match self.color {
            Color::Gray(g) => [0, g.to_bits(), 0, 0, 0],
            Color::Rgb([r, g, b]) => [1, r.to_bits(), g.to_bits(), b.to_bits(), 0],
            Color::Cmyk([c, m, y, k]) => [2, c.to_bits(), m.to_bits(), y.to_bits(), k.to_bits()],
        };
        StyleKey {
            face: self.face,
            size,
            color,
            align: self.align,
        }
    }
}

/// A [`Style`] as a key ([`Style::key`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct StyleKey {
    face: Typeface,
    size: (bool, u32),
    color: [u64; 5],
    align: Align,
}

/// How a line sits between the sides of its box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Align {
    Left,
    Center,
    Right,
    /// Stretched to both sides at its spaces, but for the last line of a paragraph,
    /// which is left aligned.
    Justify,
}

/// The style of a default appearance string (PDF 32000-1 12.7.3.3): the font
/// operand of `Tf` and its size, and the last colour operator. A size of 0
/// (fit to the field) is `None`; a negative one is kept, as viewers draw it; without
/// `Tf` the size is 12 pt, as common viewers set it.
///
/// Read as viewers read it: an operator takes the last name before it and the
/// numbers since the operator before it, the first of them first, 0 for each it
/// lacks; whatever is neither, such as an array or a stray `]`, is passed over.
pub(super) fn default_appearance(da: &[u8]) -> (Family, Option<f32>, Option<Color>) {
    use hayro_syntax::object::Object;
    use hayro_syntax::reader::{Reader, ReaderExt};
    let mut family = Family::Sans;
    let mut size = Some(12.0);
    let mut color = None;
    let mut name = None;
    let mut numbers: Vec<f64> = Vec::new();
    let mut r = Reader::new(da);
    let delimiter = |b: u8| b"()<>[]{}/%".contains(&b);
    let white = |b: u8| b"\0\t\n\x0c\r ".contains(&b);
    loop {
        r.skip_white_spaces_and_comments();
        if r.at_end() {
            break;
        }
        let starts_operand = r.peek_byte().is_some_and(|b| {
            matches!(
                b,
                b'/' | b'.' | b'+' | b'-' | b'0'..=b'9' | b'[' | b'<' | b'('
            )
        });
        if let Some(operand) = starts_operand
            .then(|| r.read_without_context::<Object<'_>>())
            .flatten()
        {
            match operand {
                // Only the first four are ever taken.
                Object::Number(n) if numbers.len() < 4 => numbers.push(operand_real(n)),
                Object::Name(n) => name = Some(n),
                _ => {}
            }
            continue;
        }
        let start = r.offset();
        r.forward_while(|b| !white(b) && !delimiter(b));
        if r.offset() == start {
            // A delimiter that starts no operand.
            r.forward();
            continue;
        }
        let n = |i: usize| numbers.get(i).copied().unwrap_or(0.0);
        match &da[start..r.offset()] {
            b"Tf" => {
                family = name
                    .as_ref()
                    .map_or(Family::Sans, |n| Family::of_font_name(n.as_ref()));
                // A negative size is drawn as one, turned half round.
                size = Some(n(0) as f32).filter(|s| *s != 0.0);
            }
            b"g" => color = Color::of(&[n(0)]),
            b"rg" => color = Color::of(&[n(0), n(1), n(2)]),
            b"k" => color = Color::of(&[n(0), n(1), n(2), n(3)]),
            _ => {}
        }
        numbers.clear();
    }
    (family, size, color)
}

/// The text of a text string (PDF 32000-1 7.9.2.2): UTF-16BE or UTF-8 with a byte
/// order mark, else `PDFDocEncoding` (read as Latin-1 for the printable range).
/// Viewers end a string that is not UTF-16 at its first zero byte (observed); in
/// UTF-16, a zero character is kept, and no font draws it ([`Fonts::shape`]).
pub(super) fn text_string(bytes: &[u8]) -> String {
    if let Some(utf16) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = utf16
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_be_bytes(*b))
            .collect();
        return char::decode_utf16(units)
            .map(|c| c.unwrap_or('\u{FFFD}'))
            .collect();
    }
    let bytes = bytes.split(|&b| b == 0).next().unwrap_or_default();
    if let Some(utf8) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(utf8).into_owned();
    }
    bytes.iter().map(|&b| pdf_doc_char(b)).collect()
}

/// A `PDFDocEncoding` byte (PDF 32000-1 Annex D.2).
fn pdf_doc_char(b: u8) -> char {
    const HIGH: [char; 32] = [
        '•', '†', '‡', '…', '—', '–', 'ƒ', '⁄', '‹', '›', '−', '‰', '„', '“', '”', '‘', '’', '‚',
        '™', 'ﬁ', 'ﬂ', 'Ł', 'Œ', 'Š', 'Ÿ', 'Ž', 'ı', 'ł', 'œ', 'š', 'ž', '\u{FFFD}',
    ];
    const LOW: [char; 8] = ['˘', 'ˇ', 'ˆ', '˙', '˝', '˛', '˚', '˜'];
    match b {
        0x18..=0x1F => LOW[usize::from(b - 0x18)],
        0x80..=0x9F => HIGH[usize::from(b - 0x80)],
        0xA0 => '€',
        _ => char::from(b),
    }
}

/// Rich text in one style.
#[derive(Debug)]
pub(super) struct Span {
    pub(super) text: String,
    pub(super) style: Style,
}

#[cfg(test)]
mod tests {
    use super::{Align, Family, Fonts};
    use std::ops::Range;

    /// The lines `n` glyphs of rich text are broken into to fit `width`: each line
    /// as many words as fit, with the spaces before each but the first; a word
    /// wider than a line alone on its line, overflowing it. Each word is measured
    /// as it is reached: the reference [`super::Shaped::lines`] searches for the
    /// same lines.
    fn rich_breaks(
        n: usize,
        space: impl Fn(usize) -> bool,
        advance: impl Fn(usize) -> f64,
        width: f64,
    ) -> Vec<Range<usize>> {
        let mut lines = Vec::new();
        let (mut start, mut line_width) = (0, 0.0);
        let mut i = 0;
        while i < n {
            let mut j = i;
            while j < n && space(j) {
                j += 1;
            }
            while j < n && !space(j) {
                j += 1;
            }
            let word_width: f64 = (i..j).map(&advance).sum();
            if start == i || line_width + word_width <= width {
                line_width += word_width;
                i = j;
            } else {
                lines.push(start..i);
                line_width = 0.0;
                while i < j && space(i) {
                    i += 1;
                }
                start = i;
            }
        }
        lines.push(start..i);
        lines
    }

    /// The lines `n` glyphs of plain text are broken into to fit `room`, as bake
    /// breaks them: tokens of the glyphs up to a space and that space; a line
    /// takes its first token, then each next while the line up to that token's
    /// space, its advances `advance(k)` added from 0 in order in 32-bit numbers,
    /// is at most `room`. Each sum is taken anew from the line's start.
    fn plain_breaks(
        n: usize,
        space: impl Fn(usize) -> bool,
        advance: impl Fn(usize) -> f32,
        room: f32,
    ) -> Vec<Range<usize>> {
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < n {
            let mut j = i;
            while j < n && !space(j) {
                j += 1;
            }
            tokens.push((i, j, (j + 1).min(n)));
            i = (j + 1).min(n);
        }
        let sum = |a: usize, b: usize| (a..b).fold(0.0_f32, |x, k| x + advance(k));
        let mut lines = Vec::new();
        let Some(&(first, _, mut end)) = tokens.first() else {
            return std::iter::once(0..0).collect();
        };
        let mut start = first;
        for &(from, word, to) in &tokens[1..] {
            if sum(start, word) <= room {
                end = to;
            } else {
                lines.push(start..end);
                (start, end) = (from, to);
            }
        }
        lines.push(start..end);
        lines
    }

    const TEXTS: [&str; 15] = [
        "",
        "one",
        "a b  c d",
        "word\r\nnext\rthird\n\nlast",
        "averyveryverylongword short words after it",
        " lead",
        "   three lead",
        "trail  ",
        "trail      ",
        "\r\n",
        "x\r",
        "\n\r",
        "ab cd ef gh ij kl mn op qr st uv wx yz",
        "AAAAA   BBBBB CC",
        "AAAAA BBBBB   CC DD",
    ];

    /// Plain lines are those the reference breaks each paragraph into, glyph for
    /// glyph: paragraphs at LF, CR and CR LF, tokens wrapped, a token wider than
    /// the line alone on its line, spaces after a space tokens of their own; at
    /// rooms that are the measure of a token's end, and the number just below.
    #[test]
    fn plain_lines_break_as_viewers_break_them() {
        let fonts = Fonts::new(crate::InterpreterSettings::default().font_resolver);
        for text in TEXTS {
            for (family, size) in [
                (Family::Sans, 12.0),
                (Family::Mono, 9.7),
                (Family::Serif, 17.25),
            ] {
                let shaped = fonts.shape_plain(family, text).expect("drawable");
                let advance = |k: usize| shaped.advances[k] * size;
                let mut rooms = vec![-1.0_f32, 0.0, 10.0, 20.0, 40.0, 1000.0];
                for p in &shaped.paragraphs {
                    for w in shaped.words.iter().filter(|w| p.glyphs.contains(&w.start)) {
                        let at = (p.glyphs.start..w.end).fold(0.0_f32, |x, k| x + advance(k));
                        rooms.extend([at, f32::from_bits(at.to_bits() - 1)]);
                    }
                }
                for room in rooms {
                    let got: Vec<Range<usize>> = shaped
                        .lines(size, Some(room), Align::Left, false)
                        .map(|l| l.glyphs)
                        .collect();
                    let mut expected = Vec::new();
                    for p in &shaped.paragraphs {
                        let r = p.glyphs.clone();
                        let lines = plain_breaks(
                            r.len(),
                            |k| shaped.chars[r.start + k] == ' ',
                            |k| advance(r.start + k),
                            room,
                        );
                        expected.extend(
                            lines
                                .into_iter()
                                .map(|l| r.start + l.start..r.start + l.end),
                        );
                    }
                    assert_eq!(got, expected, "{text:?} at {room}");
                }
            }
        }
    }

    /// Rich lines are those the reference breaks each paragraph into, glyph for
    /// glyph, at rooms no line's measure lies near.
    #[test]
    fn rich_lines_break_by_words() {
        let fonts = Fonts::new(crate::InterpreterSettings::default().font_resolver);
        for text in TEXTS {
            let shaped = fonts.shape_plain(Family::Sans, text).expect("drawable");
            for width in [0.0, 10.0, 20.0, 40.0, 1000.0] {
                let got: Vec<Range<usize>> = shaped
                    .lines(12.0, Some(width), Align::Left, true)
                    .inspect(|l| assert!(!l.uncertain, "{text:?} at {width}"))
                    .map(|l| l.glyphs)
                    .collect();
                let mut expected = Vec::new();
                for p in &shaped.paragraphs {
                    let r = p.glyphs.clone();
                    let lines = rich_breaks(
                        r.len(),
                        |k| shaped.chars[r.start + k] == ' ',
                        |k| shaped.width(r.start + k..r.start + k + 1, 12.0),
                        f64::from(width),
                    );
                    expected.extend(
                        lines
                            .into_iter()
                            .map(|l| r.start + l.start..r.start + l.end),
                    );
                }
                assert_eq!(got, expected, "{text:?} at {width}");
            }
        }
    }

    /// A rich line whose measure lies within the rounding of its room cannot be
    /// told to fit or not, and is refused; one clear of it is broken.
    #[test]
    fn a_rich_line_at_its_room_is_refused() {
        let fonts = Fonts::new(crate::InterpreterSettings::default().font_resolver);
        let shaped = fonts
            .shape_plain(Family::Mono, "AAAAA AAAAA")
            .expect("drawable");
        let lines = |room: f32| -> Vec<(usize, bool)> {
            shaped
                .lines(12.0, Some(room), Align::Left, true)
                .map(|l| (l.glyphs.len(), l.uncertain))
                .collect()
        };
        assert!(lines(79.2)[0].1);
        assert_eq!(lines(79.3), [(11, false)]);
        assert_eq!(lines(79.1), [(5, false), (5, false)]);
    }

    /// A paragraph of one word is laid out on one line whether or not it fits: at
    /// its room it is not refused. A space after it, which a line that does not
    /// fit leaves to a line of its own, is.
    #[test]
    fn a_word_alone_at_its_room_is_laid_out() {
        let fonts = Fonts::new(crate::InterpreterSettings::default().font_resolver);
        let lines = |text: &str, room: f32| -> Vec<(usize, bool)> {
            let shaped = fonts.shape_plain(Family::Mono, text).expect("drawable");
            shaped
                .lines(12.0, Some(room), Align::Left, true)
                .map(|l| (l.glyphs.len(), l.uncertain))
                .collect()
        };
        for room in [35.99999, 36.0, 36.00001] {
            assert_eq!(lines("AAAAA", room), [(5, false)], "{room}");
        }
        assert!(lines("AAAA ", 36.0)[0].1);
        // Two words, the second's fit untold, as the paragraph's.
        assert!(lines("AA AA", 36.0)[0].1);
    }

    /// Lines are laid out as they are asked for, each by a search: the first line of
    /// a text of many words costs a few steps, however long the text.
    #[test]
    fn a_first_line_costs_a_search() {
        let fonts = Fonts::new(crate::InterpreterSettings::default().font_resolver);
        let mut costs = Vec::new();
        for n in [1_000, 100_000] {
            let shaped = fonts
                .shape_plain(Family::Sans, &"ab ".repeat(n))
                .expect("drawable");
            let before = work_done();
            let first = shaped
                .lines(12.0, Some(100.0), Align::Left, false)
                .next()
                .unwrap();
            costs.push(work_done() - before);
            assert!(first.glyphs.len() < 30, "{n}");
        }
        assert!(costs[1] < costs[0] + 20, "{costs:?}");
    }

    use super::super::work_done;
    use super::{
        Color, Direction, Out, Rounding, Setting, Shaped, Size, Span, Style, TextBox, Typeface,
        draw_box, draw_comb, draw_line, draw_options, draw_rows,
    };
    use kurbo::{Affine, Rect};
    use std::rc::Rc;

    /// Numbers for the cases below, the same on every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn pick<T: Clone>(&mut self, of: &[T]) -> T {
            of[self.below(of.len())].clone()
        }
    }

    /// What `draw` writes: its content and the glyphs it names, or why it fails.
    fn written(seen: Rect, draw: impl FnOnce(&mut Out) -> Result<(), super::Refused>) -> String {
        let mut out = Out::new(seen, 0.0);
        let drew = draw(&mut out);
        match out.finish(drew) {
            Ok(((), content, glyphs)) => {
                let names: Vec<&str> = glyphs.iter().map(|g| g.name()).collect();
                format!("{content}{names:?}")
            }
            Err(why) => format!("{why:?}"),
        }
    }

    /// What `draw` writes with the searches and bounds that pass over what cannot
    /// show, and without them, and the work each took: the same, or the case
    /// fails. Returns whether passing over took less work.
    fn same(
        case: &str,
        seen: Rect,
        draw: impl Fn(&mut Out) -> Result<(), super::Refused>,
    ) -> (bool, String) {
        let before = work_done();
        let fast = written(seen, &draw);
        let quick = work_done() - before;
        super::EXHAUSTIVE.with(|e| e.set(true));
        let before = work_done();
        let all = written(seen, &draw);
        let slow = work_done() - before;
        super::EXHAUSTIVE.with(|e| e.set(false));
        assert!(
            fast == all,
            "{case}:\nwith skips:    {fast}\nwithout skips: {all}"
        );
        (quick < slow, fast)
    }

    fn fonts() -> Fonts {
        Fonts::new(crate::InterpreterSettings::default().font_resolver)
    }

    /// Words of a few letters wide and narrow, spaces, and line breaks of each kind.
    fn plain_text(rng: &mut Rng, words: usize) -> String {
        let mut text = String::new();
        for _ in 0..words {
            for _ in 0..1 + rng.below(6) {
                text.push(rng.pick(&['A', 'i', 'W', 'm', '.', '-', 'g']));
            }
            text.push_str(rng.pick(&[" ", " ", " ", "  ", "\n", "\r\n", "\r", "\n\n"]));
        }
        text
    }

    /// Rich text spans of sizes in points and multiples of the base size, faces,
    /// colours and alignments; multiples alone when `multiples`.
    fn rich_spans(rng: &mut Rng, multiples: bool) -> Vec<Span> {
        (0..1 + rng.below(12))
            .map(|_| {
                let size = if multiples || rng.below(2) == 0 {
                    Size::Base(rng.pick(&[0.5, 1.0, 2.5, 0.0001]))
                } else {
                    Size::Points(rng.pick(&[1.0, 6.0, 12.0, 40.0, 1e-30]))
                };
                let words = 1 + rng.below(5);
                Span {
                    text: plain_text(rng, words),
                    style: Style {
                        face: Typeface {
                            family: rng.pick(&[Family::Sans, Family::Serif, Family::Mono]),
                            bold: rng.below(2) == 0,
                            italic: rng.below(2) == 0,
                        },
                        size,
                        color: rng.pick(&[Color::Rgb([1.0, 0.0, 0.0]), Color::Gray(0.5)]),
                        align: rng.pick(&[
                            Align::Left,
                            Align::Center,
                            Align::Right,
                            Align::Justify,
                        ]),
                    },
                }
            })
            .collect()
    }

    /// Frames a box is laid out in: as is, moved, turned by right angles as
    /// `FreeText` turns them, and by others.
    fn frame(rng: &mut Rng) -> Affine {
        rng.pick(&[
            Affine::IDENTITY,
            Affine::translate((10.0, -100.0)),
            Affine::new([0.0, 1.0, -1.0, 0.0, 150.0, 20.0]),
            Affine::new([-1.0, 0.0, 0.0, -1.0, 190.0, 180.0]),
            Affine::new([0.0, -1.0, 1.0, 0.0, 10.0, 170.0]),
            Affine::translate((40.0, 30.0)) * Affine::rotate(0.5),
            Affine::translate((100.0, 300.0)) * Affine::rotate(-2.1),
        ])
    }

    fn rect(rng: &mut Rng) -> Rect {
        let (x, y) = (
            rng.pick(&[-60.0, 0.0, 10.0, 150.0]),
            rng.pick(&[-400.0, 0.0, 30.0, 170.0]),
        );
        let (w, h) = (
            rng.pick(&[0.0, 9.0, 120.0, 500.0]),
            rng.pick(&[0.0, 15.0, 160.0, 900.0]),
        );
        Rect::new(x, y, x + w, y + h)
    }

    const PAGE: Rect = Rect::new(0.0, 0.0, 200.0, 200.0);

    fn seen(rng: &mut Rng) -> Rect {
        rng.pick(&[
            PAGE,
            Rect::new(50.0, 50.0, 60.0, 60.0),
            Rect::new(0.0, 150.0, 200.0, 200.0),
            Rect::new(-1e4, -1e4, 1e4, 1e4),
        ])
    }

    /// The text of words `A` on lines of their own: `n` of them, and `breaks` more
    /// line breaks after them.
    fn column(n: usize, breaks: usize) -> String {
        vec!["A"; n].join(" ") + &"\n".repeat(breaks)
    }

    /// One text in a box, as a `FreeText` lays it out, with and without the bounds.
    fn one_box(
        fonts: &Fonts,
        case: &str,
        text: &Rc<Shaped>,
        set: &Setting,
        avail: f32,
        area: &TextBox,
        seen: Rect,
    ) -> (bool, String) {
        same(case, seen, |out| {
            draw_box(
                fonts,
                std::iter::once(Ok(text.clone())),
                text.bound(set.base),
                set,
                avail,
                0.8,
                &area.enter(out)?,
                seen,
                out,
            )
        })
    }

    /// Text laid out in a box draws the same with and without the searches and
    /// bounds that pass over lines, spans and glyphs that cannot show: plain and
    /// rich text, sizes in points and multiples of the base size, positive,
    /// negative, 0 and tiny base sizes, boxes turned any way, one or several texts
    /// one after another, and texts of many thousand lines whose heights round as
    /// they add up, such as those whose last lines alone reach the page.
    #[test]
    fn boxes_draw_the_same_with_and_without_skips() {
        let fonts = fonts();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let (mut skipped, mut drew, mut failed) = (0, 0, 0);
        for case in 0..20_000 {
            let base = rng.pick(&[12.0, 1.0, 0.5, 40.0, 0.0, -12.0, -0.7, 1e-30]);
            let pieces: Vec<Rc<Shaped>> = (0..1 + rng.below(3))
                .map(|_| {
                    let (plain, words) = (rng.below(2) == 0, 1 + rng.below(40));
                    let multiples = base < 0.0 || rng.below(3) == 0;
                    let shaped = if plain {
                        fonts.shape_plain(Family::Sans, &plain_text(&mut rng, words))
                    } else {
                        fonts.shape_rich(&rich_spans(&mut rng, multiples))
                    };
                    Rc::new(shaped.expect("drawable"))
                })
                .collect();
            let bound = pieces.iter().fold((0.0_f32, 0.0_f64), |(s, i), p| {
                let (ps, pi) = p.bound(base);
                (s.max(ps), i.max(pi))
            });
            let set = Setting {
                base,
                color: Color::Gray(0.0),
                align: rng.pick(&[Align::Left, Align::Center, Align::Right, Align::Justify]),
            };
            let avail = rng.pick(&[0.0, 8.0, 60.0, 200.0, 1000.0]);
            let area = TextBox {
                rich: rng.below(2) == 0,
                pad: rng.pick(&[0.0, 2.0]),
                top: rng.pick(&[40.0, 120.0, 200.0, 200.0, 400.0, 5000.0, -20.0, -300.0]),
                room: rng.pick(&[f32::INFINITY, f32::INFINITY, 30.0, 300.0]),
                frame: frame(&mut rng),
                clip: if rng.below(3) == 0 {
                    rect(&mut rng)
                } else {
                    Rect::new(-20.0, -20.0, 220.0, 220.0)
                },
            };
            let seen = seen(&mut rng);
            let ascent = rng.pick(&[0.8, 0.9]);
            let (quick, drawn) = same(&format!("case {case}"), seen, |out| {
                draw_box(
                    &fonts,
                    pieces.iter().cloned().map(Ok),
                    bound,
                    &set,
                    avail,
                    ascent,
                    &area.enter(out)?,
                    seen,
                    out,
                )
            });
            skipped += usize::from(quick);
            drew += usize::from(drawn.contains(" Do"));
            failed += usize::from(!drawn.contains('['));
        }
        assert!(
            skipped > 5000 && drew > 3000 && failed < 2000,
            "of 20,000, {skipped} passed over something, {drew} drew, {failed} failed"
        );

        // Words `A`, one a line, in boxes so tall that their last lines alone show
        // at the top of the page; ten line breaks after them change nothing drawn.
        // A viewer adding the lines' heights may put the last hundreds of points
        // from where they are computed: refused, with or without skips.
        let set = Setting {
            base: 12.0,
            color: Color::Gray(0.0),
            align: Align::Left,
        };
        for (rect, words) in [
            ([10.0, -100.0, 19.0, 240157.0], 16_662),
            ([10.0, 0.0, 20.0, 432300.0], 30_000),
        ] {
            let inner = Rect::new(rect[0], rect[1], rect[2], rect[3]);
            let area = TextBox {
                rich: false,
                pad: 0.0,
                top: inner.height() as f32,
                room: f32::INFINITY,
                frame: Affine::translate((inner.x0, inner.y0)),
                clip: inner,
            };
            let avail = inner.width() as f32;
            let drawn: Vec<String> = [0, 10]
                .into_iter()
                .map(|breaks| {
                    let text = Rc::new(
                        fonts
                            .shape_plain(Family::Sans, &column(words, breaks))
                            .unwrap(),
                    );
                    one_box(
                        &fonts,
                        &format!("{rect:?}, {breaks} breaks"),
                        &text,
                        &set,
                        avail,
                        &area,
                        PAGE,
                    )
                    .1
                })
                .collect();
            assert_eq!(drawn[0], "Coordinates", "{rect:?}");
            assert_eq!(drawn[0], drawn[1], "{rect:?}");
        }

        // Lines whose last alone reach the page: a few hundred, drawn, and many
        // thousand, at sizes where their heights round as they add up, refused
        // where they may reach it (a viewer adding them up may put them hundreds of
        // points off): plain text going down, to the page's top, plain text at a
        // negative size going up, to its bottom, and rich text of two sizes going
        // down.
        let cases = [
            (300, 12.0, false),
            (300, -12.0, false),
            (300, 12.0, true),
            (16_662, 12.0, false),
            (30_000, 12.0, false),
            (100_000, 12.0, false),
            (60_000, 1.0, false),
            (50_000, 0.3, false),
            (200_000, 5.5, false),
            (20_000, -12.0, false),
            (20_000, 12.0, true),
        ];
        for (words, base, rich) in cases {
            let text = Rc::new(if rich {
                let spans: Vec<Span> = (0..words)
                    .map(|i| Span {
                        text: "A\n".into(),
                        style: Style {
                            face: Family::Sans.into(),
                            size: Size::Points(if i % 2 == 0 { 12.0 } else { 7.0 }),
                            color: Color::Gray(0.0),
                            align: Align::Left,
                        },
                    })
                    .collect();
                fonts.shape_rich(&spans).unwrap()
            } else if base < 0.0 {
                fonts
                    .shape_plain(Family::Sans, &vec!["A"; words].join("\n"))
                    .unwrap()
            } else {
                fonts.shape_plain(Family::Sans, &column(words, 0)).unwrap()
            });
            let set = Setting {
                base,
                color: Color::Gray(0.0),
                align: Align::Left,
            };
            // One word a line.
            let avail = 9.0 * base.abs() / 12.0;
            // How far below the top the last line's baseline lies, laid out as
            // `draw_box` lays it out.
            let (mut height, mut below) = (0.0_f32, 0.0_f32);
            for line in text.lines(base, Some(avail), Align::Left, rich) {
                below = height + 0.8 * line.size;
                height += 1.2 * line.size;
            }
            let forward = text.direction(base).unwrap() == Direction::Forward;
            // The last baseline about where the last glyphs' boxes, above the
            // baseline going down and below it going up, start to reach the page.
            let ends: &[f32] = if forward {
                &[180.0, 199.0, 200.0, 200.5, 203.0, 209.0, 220.0]
            } else {
                &[-20.0, -12.0, -1.0, -0.1, 0.0, 3.0, 10.0]
            };
            let mut drew = 0;
            for &end in ends {
                let area = TextBox {
                    rich,
                    pad: 0.0,
                    top: end + below,
                    room: f32::INFINITY,
                    frame: Affine::translate((10.0, 0.0)),
                    clip: Rect::new(-1e6, -1e6, 1e6, 1e6),
                };
                let case = format!("{words} at {base}, last at {end}");
                let (_, drawn) = one_box(&fonts, &case, &text, &set, avail, &area, PAGE);
                if words > 1000 {
                    assert!(drawn == "Coordinates" || drawn == "[]", "{case}: {drawn}");
                } else {
                    assert!(drawn.ends_with(']'), "{case}: {drawn}");
                }
                drew += usize::from(drawn.contains(" Do"));
            }
            if words < 1000 {
                assert!((1..ends.len()).contains(&drew), "{words} at {base}: {drew}");
            }
        }
    }

    /// The other ways text is drawn draw the same with and without the searches
    /// that pass over what cannot show: lines of a multiline field, a line, a
    /// comb's cells, and a list's options, at positive, negative and small sizes,
    /// near and far, and in many thousand rows or cells.
    #[test]
    fn rows_lines_combs_and_options_draw_the_same_with_and_without_skips() {
        let fonts = fonts();
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let (mut skipped, mut drew) = ([0; 4], [0; 4]);
        for case in 0..10_000 {
            let size: f32 = rng.pick(&[12.0, 1.0, 0.5, 40.0, -12.0, 1e-30, 300.0]);
            let words = 1 + rng.below(60);
            let text = fonts
                .shape_plain(Family::Sans, &plain_text(&mut rng, words))
                .unwrap();
            let set = Setting {
                base: size,
                color: Color::Gray(0.0),
                align: Align::Left,
            };
            let seen = seen(&mut rng);
            let top = rng.pick(&[100.0, 200.0, 1e4, -50.0, 1e6]);
            let step = f64::from(1.116 * size);
            let avail = rng.pick(&[0.0, 8.0, 60.0, 190.0]);
            let pad = rng.pick(&[0.0, 2.0]);
            let rows = same(&format!("rows {case}"), seen, |out| {
                draw_rows(
                    &fonts,
                    &text,
                    &set,
                    avail,
                    (top, Rounding::NONE),
                    step,
                    |w| pad + (avail - w) / 2.0,
                    true,
                    seen,
                    out,
                )
            });
            let origin = (
                rng.pick(&[-500.0, 0.0, 20.0, 150.0]),
                rng.pick(&[-10.0, 20.0, 190.0]),
            );
            let frame = frame(&mut rng);
            let line = same(&format!("line {case}"), seen, |out| {
                draw_line(
                    &fonts,
                    &text,
                    &set,
                    origin,
                    (Rounding::NONE, Rounding::NONE),
                    frame,
                    seen,
                    out,
                )
            });
            let cell = rng.pick(&[0.0, 0.01, 7.5, 40.0, 1e5]);
            let count = text.len().min(rng.pick(&[1, 5, 30, 1000]));
            let y = rng.pick(&[-30.0, 10.0, 100.0, 195.0]);
            let comb = same(&format!("comb {case}"), seen, |out| {
                draw_comb(
                    &fonts,
                    &text,
                    &set,
                    count,
                    cell,
                    (y, Rounding::NONE),
                    seen,
                    out,
                )
            });
            let labels: Vec<Rc<Shaped>> = (0..rng.pick(&[1, 3, 40, 300]))
                .map(|_| {
                    let words = 1 + rng.below(3);
                    let label = plain_text(&mut rng, words);
                    Rc::new(fonts.shape_plain(Family::Sans, &label).unwrap())
                })
                .collect();
            let options = same(&format!("options {case}"), seen, |out| {
                draw_options(
                    &fonts,
                    |i| Ok(labels[i].clone()),
                    labels.len(),
                    Family::Sans.into(),
                    &set,
                    (f64::from(pad), Rounding::NONE),
                    (top, Rounding::NONE),
                    step,
                    seen,
                    out,
                )
            });
            for (k, (quick, drawn)) in [rows, line, comb, options].into_iter().enumerate() {
                skipped[k] += usize::from(quick);
                drew[k] += usize::from(drawn.contains(" Do"));
            }
        }
        assert!(
            skipped.iter().chain(&drew).all(|&n| n > 1000),
            "of 10,000 rows, lines, combs and options, {skipped:?} passed over something, \
             {drew:?} drew"
        );

        // Many rows, cells and options, of which the page shows the last: where
        // each lies is rounded as it is far from the first. (Of 200,000 rows a
        // viewer may put the last thousands of points off, which is refused:
        // `annotation_appearance` holds that.)
        let set = Setting {
            base: 12.0,
            color: Color::Gray(0.0),
            align: Align::Left,
        };
        let step = f64::from(1.116_f32 * 12.0);
        let n = 500;
        let text = fonts.shape_plain(Family::Sans, &column(n, 0)).unwrap();
        let label = Rc::new(fonts.shape_plain(Family::Sans, "Wm").unwrap());
        let mut drew = 0;
        for shift in [-20.0, -0.5, 0.0, 7.0, 100.0, 199.0, 200.5, 230.0] {
            let top = n as f64 * step + shift;
            let rows = same(&format!("rows {shift}"), PAGE, |out| {
                draw_rows(
                    &fonts,
                    &text,
                    &set,
                    9.0,
                    (top, Rounding::NONE),
                    step,
                    |_| 0.0,
                    false,
                    PAGE,
                    out,
                )
            });
            let options = same(&format!("options {shift}"), PAGE, |out| {
                draw_options(
                    &fonts,
                    |_| Ok(label.clone()),
                    n,
                    Family::Sans.into(),
                    &set,
                    (2.0, Rounding::NONE),
                    (top, Rounding::NONE),
                    step,
                    PAGE,
                    out,
                )
            });
            drew += usize::from(rows.1.contains(" Do")) + usize::from(options.1.contains(" Do"));
        }
        let word = fonts
            .shape_plain(Family::Sans, &"AWi".repeat(1_000))
            .unwrap();
        for cell in [0.001_f32, 0.0137, 7.0] {
            let last = f64::from(cell) * word.len() as f64;
            for at in [last - 150.0, last - 0.5 * f64::from(cell), last + 3.0] {
                let seen = Rect::new(at, 0.0, at + 200.0, 200.0);
                let comb = same(&format!("comb {cell} at {at}"), seen, |out| {
                    draw_comb(
                        &fonts,
                        &word,
                        &set,
                        word.len(),
                        cell,
                        (100.0, Rounding::NONE),
                        seen,
                        out,
                    )
                });
                drew += usize::from(comb.1.contains(" Do"));
            }
        }
        assert!(drew > 10, "{drew}");
    }

    /// Characters viewers may draw out of their order are those of right-to-left
    /// scripts, Arabic digits and explicit direction marks; Latin, Cyrillic, Greek,
    /// Han, European digits, punctuation and spaces are drawn in order.
    #[test]
    fn only_characters_viewers_may_reorder_are_refused() {
        for c in [
            '\u{5D0}',
            '\u{5EA}',
            '\u{628}',
            '\u{661}',
            '\u{700}',
            '\u{FB1D}',
            '\u{FE8F}',
            '\u{10800}',
            '\u{200E}',
            '\u{200F}',
            '\u{61C}',
            '\u{202A}',
            '\u{202B}',
            '\u{202C}',
            '\u{202D}',
            '\u{202E}',
            '\u{2066}',
            '\u{2067}',
            '\u{2068}',
            '\u{2069}',
        ] {
            assert!(super::reordered(c), "{c:?}");
        }
        for c in "Az09 .,;!?()-+%$\u{E9}\u{416}\u{44F}\u{3B1}\u{6F22}\u{2014}\u{A0}\t\u{0}".chars()
        {
            assert!(!super::reordered(c), "{c:?}");
        }
    }

    /// A font standing in for Helvetica (Liberation Sans here, whose advances are
    /// 2048ths of an em) measures text as `PyMuPDF` 1.28.2's Helvetica does
    /// (`Font("helv").glyph_advance`): the AFM width of each glyph the AFM lists
    /// (Liberation's ±, ď and ľ are 549, 615 and 292), and whole thousandths of
    /// its own advance for the others (Ж 923.34, њ 812.5).
    #[test]
    fn a_standard_font_stand_in_measures_as_viewers_do() {
        use crate::font::{FontQuery, StandardFont};
        let liberation: &'static [u8] =
            include_bytes!("../../../../hayro-tests/assets/LiberationSans-Regular.ttf");
        let fonts = Fonts::new(std::sync::Arc::new(move |query: &FontQuery| match query {
            FontQuery::Standard(StandardFont::Helvetica) => {
                Some((std::sync::Arc::new(liberation), 0))
            }
            _ => None,
        }));
        let glyphs = fonts.shape(Family::Sans, "AbcW ±ďľЖжњЁ").expect("drawable");
        let advances: Vec<f32> = glyphs.iter().map(|g| g.advance).collect();
        let viewers = [
            667.0, 556.0, 500.0, 944.0, 278.0, 584.0, 643.0, 299.0, 923.0, 669.0, 813.0, 667.0,
        ]
        .map(|w: f32| w / 1000.0);
        assert_eq!(advances, viewers);
    }
}
