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
use super::out::{Color, Out};
use super::{Unsupported, work};
use crate::FontResolverFn;
use crate::font::blob::{CffFontBlob, OpenTypeFontBlob};
use crate::font::{FontQuery, StandardFont, glyph_name_to_unicode};
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
                Face::new(data, index, query.standard_substitute(), typeface.index())
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

    /// The glyph of `c` in `typeface`, with its advance in em.
    fn glyph(&self, typeface: Typeface, c: char) -> Result<Glyph<'_>, Unsupported> {
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
    let (mut x, y) = (f64::from(origin.0), f64::from(origin.1));
    let mut pen = Pen::new(transform, size, y, EVERYWHERE);
    let mut brush = Brush::new(None, None);
    for g in glyphs {
        pen.put(out, x, &g.face.outline(g.id), EVERYWHERE, &mut brush)?;
        x += f64::from(g.advance) * f64::from(size);
    }
    pen.end(out);
    Some(())
}

/// A clip that cuts nothing away.
const EVERYWHERE: Rect = Rect::new(
    f64::NEG_INFINITY,
    f64::NEG_INFINITY,
    f64::INFINITY,
    f64::INFINITY,
);

/// Writes glyphs of one size along a baseline in a frame: the frame, scaled to the
/// size and moved to the first glyph written, once, and each glyph that can reach a
/// clip as its form, moved along from the glyph before it ([`Out::form`]), so that
/// a glyph costs a few bytes of content however often it is drawn. The numbers a
/// viewer computes a glyph's place from are those written, so each must be
/// writable; a glyph is tested by the box of its outline's points, first in the
/// frame against a box there that holds what the clip shows ([`reaches`]), then
/// where it lies, whose corners must be writable, as its points then are.
pub(super) struct Pen {
    frame: Affine,
    size: f64,
    scale: f64,
    /// The baseline, up the frame.
    y: f64,
    view: Rect,
    /// Where across the last glyph written lies; `None` before the first.
    at: Option<f64>,
}

impl Pen {
    /// A pen for glyphs at `size` (not 0) along the baseline `y` up `frame`, of
    /// which those that can reach `view`, in the frame, are tested further.
    pub(super) fn new(frame: Affine, size: f32, y: f64, view: Rect) -> Self {
        Self {
            frame,
            size: f64::from(size),
            scale: f64::from(size) / 1000.0,
            y,
            view,
            at: None,
        }
    }

    /// Writes the glyph of `outline` with its origin `x` across, when it can reach
    /// `clip`, in the content's space; what `brush` holds back is set before the
    /// first glyph written. At a size that is no number, the box's corners cannot
    /// be written, which fails the content, wherever the glyph lies.
    fn put(
        &mut self,
        out: &mut Out,
        x: f64,
        outline: &Outline,
        clip: Rect,
        brush: &mut Brush,
    ) -> Option<()> {
        let Some(form) = &outline.form else {
            return Some(());
        };
        if self.size.is_finite() && !reaches(outline.em, self.size, (x, self.y), self.view) {
            return Some(());
        }
        let (s, y) = (self.scale, self.y);
        let at = self.frame * Affine::new([s, 0.0, 0.0, s, x, y]);
        if !out.box_reaches(at, outline.ink, clip)? {
            return Some(());
        }
        // The first glyph written is placed where it lies, not from the baseline's
        // origin, which may lie as far before it as the text passed over is long.
        match self.at {
            Some(from) => {
                let dx = (x - from) / s;
                if dx != 0.0 {
                    out.nums(&[1.0, 0.0, 0.0, 1.0, dx, 0.0], "cm")?;
                }
            }
            None => {
                brush.ready(out)?;
                out.op("q");
                out.nums(&at.as_coeffs(), "cm")?;
            }
        }
        out.form(form);
        self.at = Some(x);
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
/// first: a clip, set once, and the fill colour; so text that writes no glyph
/// writes nothing else either, whatever was passed over to find that.
pub(super) struct Brush {
    clip: Option<Rect>,
    color: Option<Color>,
    want: Option<Color>,
}

impl Brush {
    /// A brush that sets `clip` before the first glyph, with fill colour `color` set.
    pub(super) fn new(clip: Option<Rect>, color: Option<Color>) -> Self {
        Self {
            clip,
            color,
            want: color,
        }
    }

    /// Sets what the next glyph needs.
    fn ready(&mut self, out: &mut Out) -> Option<()> {
        if let Some(clip) = self.clip.take() {
            out.clip(clip)?;
        }
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
    /// Whether it is rich text, laid out as such.
    pub(super) rich: bool,
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
    /// Its advance, at the base size.
    pub(super) width: f64,
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
        self.shape_spans(text.chars().map(|c| (c, 0)), vec![style], false)
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
        self.shape_spans(chars, styles, true)
    }

    /// The characters `chars`, each with its span among `styles`, set.
    fn shape_spans(
        &self,
        chars: impl Iterator<Item = (char, usize)>,
        styles: Vec<SpanStyle>,
        rich: bool,
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
            ink: None,
            widest: 0.0,
            rest: Vec::new(),
            sizes: Vec::new(),
            leaves: 0,
            in_points_any: false,
            in_multiples_any: false,
            rich,
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
    /// lines each hold a word, but for a last one of the spaces after them.
    fn most_lines(&self) -> usize {
        self.words.len() + self.paragraphs.len()
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
    /// largest size among them are passed over together, so the work grows with
    /// the spans that can reach it, not with those of the range.
    fn reaching(
        &self,
        node: usize,
        under: Range<usize>,
        seek: &Seek<'_>,
        visit: &mut dyn FnMut(usize) -> Result<(), Unsupported>,
    ) -> Result<(), Unsupported> {
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
            let far = size * seek.reach;
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

    /// The lines of the text at base size `base`: a paragraph's words wrapped to
    /// fit `width` when there is one, a word wider than a line alone on its line,
    /// overflowing it, as viewers set them. Lines are laid out as they are asked
    /// for: each costs a search over the text's words, not a pass over them.
    pub(super) fn lines(&self, base: f32, width: Option<f64>, align: Align) -> Lines<'_> {
        Lines {
            text: self,
            base,
            width,
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

    /// Where the line from glyph `start` of paragraph `par` ends, and where the next
    /// starts, broken to fit `width`.
    fn line_end(&self, start: usize, par: Range<usize>, width: f64, base: f32) -> (usize, usize) {
        let fits = |end: usize| self.width(start..end, base) <= width;
        // The words of the paragraph from the one that ends after `start`.
        let first = self.words.partition_point(|w| w.end <= start);
        let after = self.words.partition_point(|w| w.start < par.end);
        if first >= after {
            // Spaces alone.
            return (par.end, par.end);
        }
        if fits(par.end) {
            return (par.end, par.end);
        }
        // The first word always; then as many more as fit.
        let more = partition(after - first - 1, |j| fits(self.words[first + 1 + j].end));
        let last = first + more;
        let next = self.words.get(last + 1).filter(|w| w.start < par.end);
        (self.words[last].end, next.map_or(par.end, |w| w.start))
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
    view: Rect,
}

/// The lines of [`Shaped`] text ([`Shaped::lines`]).
pub(super) struct Lines<'s> {
    text: &'s Shaped,
    base: f32,
    width: Option<f64>,
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
        let (end, next) = match self.width {
            Some(width) if self.forward && start < par.glyphs.end => {
                text.line_end(start, par.glyphs.clone(), width, self.base)
            }
            _ => (par.glyphs.end, par.glyphs.end),
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
            width: text.width(start..end, self.base),
        })
    }
}

/// Fails with [`Unsupported::Kind`] where `Out` failed, as the content then fails
/// whatever the generator does next.
fn fail(o: Option<()>) -> Result<(), Unsupported> {
    o.ok_or(Unsupported::Kind)
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

/// Draws glyphs `range` of `text` laid out at `set`, along the baseline from
/// `(x, y)` in `frame`, each space a justified line stretches `spacing` wider: of
/// those, only the spans and glyphs whose boxes can reach `view` across, a box in
/// the frame, are looked at, by searches, and those drawn that reach `clip`, in the
/// content's space. The work grows with the glyphs drawn, not with the text.
/// `color` is the fill colour set so far.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_glyphs(
    fonts: &Fonts,
    text: &Shaped,
    set: &Setting,
    range: Range<usize>,
    (x, y): (f64, f64),
    spacing: f64,
    frame: Affine,
    view: Rect,
    clip: Rect,
    out: &mut Out,
    brush: &mut Brush,
) -> Result<(), Unsupported> {
    let Some(ink) = text.ink else {
        return Ok(());
    };
    if range.is_empty() {
        return Ok(());
    }
    let x0 = text.x(range.start, set.base);
    let pos =
        |k: usize| x + (text.x(k, set.base) - x0) + spacing * text.stretched(range.start..k) as f64;
    let mut draw_span = |span: usize| -> Result<(), Unsupported> {
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
        // corners of each glyph's box lie between `lo` and `hi` of its place.
        let (first, last) = if !skips() || !s.is_finite() {
            (0, n)
        } else if size > 0.0 {
            (
                partition(n, |j| pos(i + j) + hi < view.x0),
                partition(n, |j| pos(i + j) + lo <= view.x1),
            )
        } else {
            (
                partition(n, |j| pos(i + j) + lo > view.x1),
                partition(n, |j| pos(i + j) + hi >= view.x0),
            )
        };
        if first < last {
            brush.want = Some(style.color.unwrap_or(set.color));
            let mut pen = Pen::new(frame, size, y, view);
            for k in i + first..i + last {
                work(1);
                let g = fonts.glyph(style.face, text.chars[k])?;
                fail(pen.put(out, pos(k), &g.face.outline(g.id), clip, brush))?;
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
        view,
    };
    text.reaching(1, 0..text.leaves, &seek, &mut draw_span)
}

/// Draws `text` on one line, line breaks as spaces, along the baseline from
/// `origin` in `frame`: the glyphs that can reach `seen`, in the content's space.
pub(super) fn draw_line(
    fonts: &Fonts,
    text: &Shaped,
    set: &Setting,
    origin: (f64, f64),
    frame: Affine,
    seen: Rect,
    out: &mut Out,
) -> Result<(), Unsupported> {
    if !(seen.width() >= 0.0 && seen.height() >= 0.0) {
        return Ok(());
    }
    text.direction(set.base)?;
    let view = frame.inverse().transform_rect_bbox(seen);
    draw_glyphs(
        fonts,
        text,
        set,
        0..text.len(),
        origin,
        0.0,
        frame,
        view,
        seen,
        out,
        &mut Brush::new(None, Some(set.color)),
    )
}

/// Draws the lines of `text` at `set`'s size broken to fit `avail`, line `j`'s
/// baseline at `top - (j + 1) step` and its start across at `align` of its width:
/// those that can reach `seen`, laid out up to the last that can.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_rows(
    fonts: &Fonts,
    text: &Shaped,
    set: &Setting,
    avail: f32,
    top: f64,
    step: f64,
    align: impl Fn(f32) -> f32,
    seen: Rect,
    out: &mut Out,
) -> Result<(), Unsupported> {
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
    // The text makes no more lines than it can at any width, and its baselines, as
    // computed, run one way: none is laid out when the last it can make lies
    // wholly before the view.
    let last = baseline(text.most_lines() - 1);
    let before = (step > 0.0 && last - reach > seen.y1) || (step < 0.0 && last + reach < seen.y0);
    if skips() && before {
        return Ok(());
    }
    let mut brush = Brush::new(None, Some(set.color));
    for (j, line) in text
        .lines(size, Some(f64::from(avail)), Align::Left)
        .enumerate()
    {
        let y = baseline(j);
        let near = !(y + reach < seen.y0 || y - reach > seen.y1);
        if !skips() || (!line.glyphs.is_empty() && near) {
            let x = f64::from(align(line.width as f32));
            draw_glyphs(
                fonts,
                text,
                set,
                line.glyphs,
                (x, y),
                0.0,
                Affine::IDENTITY,
                seen,
                seen,
                out,
                &mut brush,
            )?;
        }
        // The lines after it lie from the next on, down at a positive size, up at a
        // negative one.
        let next = baseline(j + 1);
        let past = (step > 0.0 && next + reach < seen.y0) || (step < 0.0 && next - reach > seen.y1);
        if skips() && past {
            break;
        }
    }
    Ok(())
}

/// Draws the first `count` glyphs of `text` at `set`'s size, glyph `i` centred in
/// cell `i` of cells `cell` wide from 0, with its baseline at `y`: those whose
/// cells can reach `seen`.
pub(super) fn draw_comb(
    fonts: &Fonts,
    text: &Shaped,
    set: &Setting,
    count: usize,
    cell: f32,
    y: f64,
    seen: Rect,
    out: &mut Out,
) -> Result<(), Unsupported> {
    let size = set.base;
    if size == 0.0 || count == 0 || !(seen.width() >= 0.0 && seen.height() >= 0.0) {
        return Ok(());
    }
    let start = |i: usize| cell * i as f32;
    let middle = |advance: f32| (cell - advance * size) / 2.0;
    let at = |i: usize| f64::from(start(i) + middle(text.advance(i)));
    let face = text.styles[0].face;
    let mut pen = Pen::new(Affine::IDENTITY, size, y, seen);
    let mut brush = Brush::new(None, None);
    let cells = if skips() {
        cells(text, size, cell, &start, &middle, count, seen)
    } else {
        0..count
    };
    for i in cells {
        work(1);
        let g = fonts.glyph(face, text.chars[i])?;
        fail(pen.put(out, at(i), &g.face.outline(g.id), seen, &mut brush))?;
    }
    pen.end(out);
    Ok(())
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
/// the others not made.
pub(super) fn draw_options(
    fonts: &Fonts,
    label: impl Fn(usize) -> Result<Rc<Shaped>, Unsupported>,
    count: usize,
    face: Typeface,
    set: &Setting,
    pad: f64,
    top: f64,
    step: f64,
    seen: Rect,
    out: &mut Out,
) -> Result<(), Unsupported> {
    if !(seen.width() >= 0.0 && seen.height() >= 0.0) {
        return Ok(());
    }
    let baseline = |i: usize| top - (i as f64 + 1.0) * step;
    // The points of every glyph of an option lie within the font's box ([`Fonts::bound`]),
    // and the baselines, as computed, run down at a positive step and up at a
    // negative one: an option whose box lies wholly above or below the view draws
    // nothing ([`reaches`]).
    let rows = match scaled(fonts.bound(face), set.base) {
        _ if !skips() => 0..count,
        None => 0..0,
        Some((_, _, y0, y1)) if y0.is_finite() && y1.is_finite() => {
            let above = |i: usize| baseline(i) + y0 > seen.y1;
            let below = |i: usize| baseline(i) + y1 < seen.y0;
            let (first, last) = if step >= 0.0 {
                (partition(count, above), partition(count, |i| !below(i)))
            } else {
                (partition(count, below), partition(count, |i| !above(i)))
            };
            first..last.max(first)
        }
        Some(_) => 0..count,
    };
    for i in rows {
        let label = label(i)?;
        draw_line(
            fonts,
            &label,
            set,
            (pad, baseline(i)),
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
/// that `frame` maps to the content's space, under a clip to `clip`, in that space.
pub(super) struct TextBox {
    pub(super) pad: f32,
    pub(super) top: f32,
    pub(super) room: f32,
    pub(super) frame: Affine,
    pub(super) clip: Rect,
}

/// Draws texts `pieces`, each made when its lines are reached, as one text whose
/// paragraphs are theirs (as list options joined with line breaks are), laid out
/// at `set` in `avail`, each line's first baseline `ascent` (not negative) times its
/// size below its top, in `area`: the lines that can reach both its clip and
/// `seen`, laid out up to the last that can, and of the last piece none when none
/// can. The sizes and glyph boxes of every piece are within `bound`, in points and
/// in em around a glyph's origin, either way ([`Shaped::bound`]): pieces drawn 0
/// high are not made.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_box(
    fonts: &Fonts,
    pieces: impl Iterator<Item = Result<Rc<Shaped>, Unsupported>>,
    bound: (f32, f64),
    set: &Setting,
    avail: f32,
    ascent: f32,
    area: &TextBox,
    seen: Rect,
    out: &mut Out,
) -> Result<(), Unsupported> {
    let shown = area.clip.intersect(seen);
    if !(shown.width() >= 0.0 && shown.height() >= 0.0) {
        return Ok(());
    }
    // A size of 0 draws nothing (and needs nothing checked: such text is never
    // refused for the way it runs, [`Shaped::direction`]).
    if skips() && bound.0 == 0.0 {
        return Ok(());
    }
    let view = area.frame.inverse().transform_rect_bbox(shown);
    let (pad, top) = (f64::from(area.pad), area.top);
    let mut brush = Brush::new(Some(area.clip), None);
    let (mut height, mut filled) = (0.0_f32, f32::NEG_INFINITY);
    let mut way = None;
    let mut pieces = pieces.peekable();
    while let Some(text) = pieces.next() {
        let text = text?;
        let direction = text.direction(set.base)?;
        if direction != Direction::Nowhere {
            if way.is_some_and(|w| w != direction) {
                return Err(Unsupported::RichText);
            }
            way = Some(direction);
        }
        let mut lines = text.lines(set.base, Some(f64::from(avail)), set.align);
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
        if skips() && last {
            let depth = Depth {
                top,
                height,
                lines: text.most_lines(),
                size: text.rest(0, set.base),
                ascent,
            };
            if depth.cannot_reach(direction, ink, view) {
                return Ok(());
            }
        }
        for line in lines {
            let below = height + ascent * line.size;
            height += 1.2 * line.size;
            filled = filled.max(height);
            if filled > area.room {
                // This line and those after it do not fit.
                return Ok(());
            }
            let y = f64::from(top - below);
            let reach = ink * f64::from(line.size.abs());
            let near = line.size != 0.0
                && !line.glyphs.is_empty()
                && !(y + reach < view.y0 || y - reach > view.y1);
            if near || !skips() {
                let width = line.width as f32;
                let spaces = text.stretched(line.glyphs.clone());
                let (x0, spacing) = match line.align {
                    Align::Left => (0.0, 0.0),
                    Align::Center => ((avail - width) / 2.0, 0.0),
                    Align::Right => (avail - width, 0.0),
                    Align::Justify if spaces == 0 => (0.0, 0.0),
                    Align::Justify => (0.0, ((avail - width) / spaces as f32).max(0.0)),
                };
                draw_glyphs(
                    fonts,
                    &text,
                    set,
                    line.glyphs.clone(),
                    (pad + f64::from(x0), y),
                    f64::from(spacing),
                    area.frame,
                    view,
                    shown,
                    out,
                    &mut brush,
                )?;
            }
            // The lines after this one lie below the top of the next, at a positive
            // size, or above it, at a negative one (the next adds a height and an
            // ascent of its sign, and a rounded sum lies no nearer than its terms),
            // by no more than their glyphs reach.
            let rest = f64::from(text.rest(line.glyphs.end, set.base).max(later.0));
            let reach = rest * ink.max(later.1);
            let next = f64::from(top - height);
            let past = match direction {
                Direction::Forward => next + reach < view.y0,
                Direction::Backward => next - reach > view.y1,
                Direction::Nowhere => false,
            };
            if skips() && past {
                return Ok(());
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
}

impl Depth {
    /// Whether no line can reach `view` with glyphs whose points lie within `ink`
    /// times its size of its baseline, the lines leaving the view behind as they
    /// go `direction`, as [`draw_box`] computes their baselines and the pen tests
    /// their glyphs ([`reaches`]).
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
        let reach = ink * f64::from(self.size) * (1.0 + U);
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
    OpenType(OpenTypeFontBlob),
}

impl Face {
    fn new(
        data: crate::font::FontData,
        index: u32,
        standard: StandardFont,
        typeface: usize,
    ) -> Option<Self> {
        Some(Self {
            font: Font::new(data, index, standard)?,
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
                Font::OpenType(blob) => {
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
    fn new(data: crate::font::FontData, index: u32, standard: StandardFont) -> Option<Self> {
        if let Some(blob) = CffFontBlob::new(data.clone()) {
            let mut glyphs = hash::Map::default();
            for (id, name) in blob.glyph_names() {
                let (Some(c), Some(width)) =
                    (glyph_name_to_unicode(&name), standard.get_width(&name))
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
        OpenTypeFontBlob::new(data, index).map(Self::OpenType)
    }

    /// The glyph of `c` and its advance in em.
    fn glyph(&self, c: char) -> Option<(GlyphId, f32)> {
        match self {
            Self::Cff { glyphs, .. } => glyphs.get(&c).copied(),
            Self::OpenType(blob) => {
                use skrifa::raw::TableProvider;
                let id = blob.font_ref().cmap().ok()?.map_codepoint(c)?;
                if id == GlyphId::NOTDEF {
                    return None;
                }
                let advance = blob.glyph_metrics().advance_width(id)?;
                Some((id, advance / 1000.0))
            }
        }
    }

    /// The outline of glyph `id`, 1000 units to the em.
    fn outline(&self, id: GlyphId) -> BezPath {
        match self {
            Self::Cff { blob, .. } => blob.outline_glyph(id),
            Self::OpenType(blob) => blob.outline_glyph(id),
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

    /// The lines `n` glyphs are broken into to fit `width`, as viewers break them:
    /// each line as many words as fit, with the spaces before each but the first; a
    /// word wider than a line alone on its line, overflowing it. Each word is
    /// measured as it is reached: the reference [`super::Shaped::lines`] searches
    /// for the same lines.
    fn breaks(
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

    /// Lines are those the reference breaks each paragraph into, glyph for glyph:
    /// paragraphs at LF, CR and CR LF, words wrapped, a word wider than the line
    /// alone on its line, trailing spaces that do not fit on a line of none.
    #[test]
    fn lines_break_as_viewers_break_them() {
        let fonts = Fonts::new(crate::InterpreterSettings::default().font_resolver);
        let texts = [
            "",
            "one",
            "a b  c d",
            "word\r\nnext\rthird\n\nlast",
            "averyveryverylongword short words after it",
            " lead",
            "trail  ",
            "trail      ",
            "\r\n",
            "x\r",
            "\n\r",
            "ab cd ef gh ij kl mn op qr st uv wx yz",
        ];
        for text in texts {
            let shaped = fonts.shape_plain(Family::Sans, text).expect("drawable");
            for width in [0.0, 10.0, 20.0, 40.0, 1000.0] {
                let got: Vec<Range<usize>> = shaped
                    .lines(12.0, Some(width), Align::Left)
                    .map(|l| l.glyphs)
                    .collect();
                let mut expected = Vec::new();
                for p in &shaped.paragraphs {
                    let r = p.glyphs.clone();
                    let lines = breaks(
                        r.len(),
                        |k| shaped.chars[r.start + k] == ' ',
                        |k| shaped.width(r.start + k..r.start + k + 1, 12.0),
                        width,
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
            let first = shaped.lines(12.0, Some(100.0), Align::Left).next().unwrap();
            costs.push(work_done() - before);
            assert!(first.glyphs.len() < 30, "{n}");
        }
        assert!(costs[1] < costs[0] + 20, "{costs:?}");
    }

    use super::super::work_done;
    use super::{
        Color, Direction, Out, Setting, Shaped, Size, Span, Style, TextBox, Typeface, draw_box,
        draw_comb, draw_line, draw_options, draw_rows,
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
    fn written(draw: impl FnOnce(&mut Out) -> Result<(), super::Unsupported>) -> String {
        let mut out = Out::default();
        let drew = draw(&mut out);
        match (drew, out.finish()) {
            (Ok(()), Ok((content, glyphs))) => {
                let names: Vec<&str> = glyphs.iter().map(|g| g.name()).collect();
                format!("{content}{names:?}")
            }
            (Err(why), _) | (_, Err(why)) => format!("{why:?}"),
        }
    }

    /// What `draw` writes with the searches and bounds that pass over what cannot
    /// show, and without them, and the work each took: the same, or the case
    /// fails. Returns whether passing over took less work.
    fn same(
        case: &str,
        draw: impl Fn(&mut Out) -> Result<(), super::Unsupported>,
    ) -> (bool, String) {
        let before = work_done();
        let fast = written(&draw);
        let quick = work_done() - before;
        super::EXHAUSTIVE.with(|e| e.set(true));
        let before = work_done();
        let all = written(&draw);
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
        same(case, |out| {
            draw_box(
                fonts,
                std::iter::once(Ok(text.clone())),
                text.bound(set.base),
                set,
                avail,
                0.8,
                area,
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
            let (quick, drawn) = same(&format!("case {case}"), |out| {
                draw_box(
                    &fonts,
                    pieces.iter().cloned().map(Ok),
                    bound,
                    &set,
                    avail,
                    ascent,
                    &area,
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
            assert!(drawn[0].contains(" Do"), "{rect:?}: {}", drawn[0]);
            assert_eq!(drawn[0], drawn[1], "{rect:?}");
        }

        // Many thousand lines whose last alone reach the page, at sizes where their
        // heights round as they add up: plain text going down, to the page's top,
        // plain text at a negative size going up, to its bottom, and rich text of
        // two sizes going down.
        let cases = [
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
            for line in text.lines(base, Some(f64::from(avail)), Align::Left) {
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
                    pad: 0.0,
                    top: end + below,
                    room: f32::INFINITY,
                    frame: Affine::translate((10.0, 0.0)),
                    clip: Rect::new(-1e6, -1e6, 1e6, 1e6),
                };
                let case = format!("{words} at {base}, last at {end}");
                let (_, drawn) = one_box(&fonts, &case, &text, &set, avail, &area, PAGE);
                assert!(drawn.ends_with(']'), "{case}: {drawn}");
                drew += usize::from(drawn.contains(" Do"));
            }
            assert!((1..ends.len()).contains(&drew), "{words} at {base}: {drew}");
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
            let rows = same(&format!("rows {case}"), |out| {
                draw_rows(
                    &fonts,
                    &text,
                    &set,
                    avail,
                    top,
                    step,
                    |w| pad + (avail - w) / 2.0,
                    seen,
                    out,
                )
            });
            let origin = (
                rng.pick(&[-500.0, 0.0, 20.0, 150.0]),
                rng.pick(&[-10.0, 20.0, 190.0]),
            );
            let frame = frame(&mut rng);
            let line = same(&format!("line {case}"), |out| {
                draw_line(&fonts, &text, &set, origin, frame, seen, out)
            });
            let cell = rng.pick(&[0.0, 0.01, 7.5, 40.0, 1e5]);
            let count = text.len().min(rng.pick(&[1, 5, 30, 1000]));
            let y = rng.pick(&[-30.0, 10.0, 100.0, 195.0]);
            let comb = same(&format!("comb {case}"), |out| {
                draw_comb(&fonts, &text, &set, count, cell, y, seen, out)
            });
            let labels: Vec<Rc<Shaped>> = (0..rng.pick(&[1, 3, 40, 300]))
                .map(|_| {
                    let words = 1 + rng.below(3);
                    let label = plain_text(&mut rng, words);
                    Rc::new(fonts.shape_plain(Family::Sans, &label).unwrap())
                })
                .collect();
            let options = same(&format!("options {case}"), |out| {
                draw_options(
                    &fonts,
                    |i| Ok(labels[i].clone()),
                    labels.len(),
                    Family::Sans.into(),
                    &set,
                    f64::from(pad),
                    top,
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
        // each lies is rounded as it is far from the first.
        let set = Setting {
            base: 12.0,
            color: Color::Gray(0.0),
            align: Align::Left,
        };
        let step = f64::from(1.116_f32 * 12.0);
        let n = 200_000;
        let text = fonts.shape_plain(Family::Sans, &column(n, 0)).unwrap();
        let label = Rc::new(fonts.shape_plain(Family::Sans, "Wm").unwrap());
        let mut drew = 0;
        for shift in [-20.0, -0.5, 0.0, 7.0, 100.0, 199.0, 200.5, 230.0] {
            let top = n as f64 * step + shift;
            let rows = same(&format!("rows {shift}"), |out| {
                draw_rows(&fonts, &text, &set, 9.0, top, step, |_| 0.0, PAGE, out)
            });
            let options = same(&format!("options {shift}"), |out| {
                draw_options(
                    &fonts,
                    |_| Ok(label.clone()),
                    n,
                    Family::Sans.into(),
                    &set,
                    2.0,
                    top,
                    step,
                    PAGE,
                    out,
                )
            });
            drew += usize::from(rows.1.contains(" Do")) + usize::from(options.1.contains(" Do"));
        }
        let word = fonts
            .shape_plain(Family::Sans, &"AWi".repeat(100_000))
            .unwrap();
        for cell in [0.001_f32, 0.0137, 7.0] {
            let last = f64::from(cell) * word.len() as f64;
            for at in [last - 150.0, last - 0.5 * f64::from(cell), last + 3.0] {
                let seen = Rect::new(at, 0.0, at + 200.0, 200.0);
                let comb = same(&format!("comb {cell} at {at}"), |out| {
                    draw_comb(&fonts, &word, &set, word.len(), cell, 100.0, seen, out)
                });
                drew += usize::from(comb.1.contains(" Do"));
            }
        }
        assert!(drew > 10, "{drew}");
    }
}
