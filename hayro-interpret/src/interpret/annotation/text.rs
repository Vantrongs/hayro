//! Text in generated appearances: glyphs of the standard fonts the font resolver
//! gives, each drawn as a form of its outline ([`super::Glyph`]), so that the
//! appearance needs no font resources and a glyph is written once.
//!
//! Text an entry holds is set once for the document ([`Shaped`]), whatever size,
//! width and box each annotation that shows it lays it out in; each lays out lines
//! only up to the last its box can show, and draws only the glyphs that can show.

use super::entries::operand_real;
use super::out::{Color, Out};
use super::{Unsupported, work};
use crate::FontResolverFn;
use crate::font::blob::{CffFontBlob, OpenTypeFontBlob};
use crate::font::{FontQuery, StandardFont, glyph_name_to_unicode};
use kurbo::{Affine, BezPath, Rect};
use rustc_hash::FxHashMap;
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
    let mut pen = Pen::new(transform, size, (x, y));
    for g in glyphs {
        pen.put(out, x, &g.face.outline(g.id), EVERYWHERE)?;
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
/// size and moved to the baseline's origin, once, and each glyph that can reach a
/// clip as its form, moved along from the glyph before it ([`Out::form`]), so that
/// a glyph costs a few bytes of content however often it is drawn. The numbers a
/// viewer computes a glyph's place from are those written, so each must be
/// writable; a glyph is tested by the box of its outline's points, whose corners
/// must be writable where it lies, as its points then are.
pub(super) struct Pen {
    frame: Affine,
    scale: f64,
    origin: (f64, f64),
    /// Where across the last glyph written lies; `None` before the first.
    at: Option<f64>,
}

impl Pen {
    /// A pen for glyphs at `size` (not 0) along the baseline from `origin` in `frame`.
    pub(super) fn new(frame: Affine, size: f32, origin: (f64, f64)) -> Self {
        Self {
            frame,
            scale: f64::from(size) / 1000.0,
            origin,
            at: None,
        }
    }

    /// Writes the glyph of `outline` with its origin `x` across, when it can reach
    /// `clip`, in the content's space.
    fn put(&mut self, out: &mut Out, x: f64, outline: &Outline, clip: Rect) -> Option<()> {
        let Some(form) = &outline.form else {
            return Some(());
        };
        let (s, y) = (self.scale, self.origin.1);
        let at = self.frame * Affine::new([s, 0.0, 0.0, s, x, y]);
        if !out.box_reaches(at, outline.ink, clip)? {
            return Some(());
        }
        let from = match self.at {
            Some(from) => from,
            None => {
                out.op("q");
                let start = self.frame * Affine::new([s, 0.0, 0.0, s, self.origin.0, y]);
                out.nums(&start.as_coeffs(), "cm")?;
                self.origin.0
            }
        };
        let dx = (x - from) / s;
        if dx != 0.0 {
            out.nums(&[1.0, 0.0, 0.0, 1.0, dx, 0.0], "cm")?;
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
        for (i, style) in s.styles.iter().enumerate().rev() {
            match style.size {
                Size::Points(p) => rest.0 = rest.0.max(p.abs()),
                Size::Base(k) => rest.1 = rest.1.max(k.abs()),
            }
            s.rest[i] = rest;
        }
        Ok(s)
    }
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
    /// furthest from 0.
    fn size_of(&self, range: Range<usize>, base: f32) -> f32 {
        let (first, last) = (self.span(range.start), self.span(range.end - 1));
        work(last - first + 1);
        self.styles[first..=last]
            .iter()
            .map(|s| s.size.at(base))
            .fold(0.0, |a: f32, b| if b.abs() > a.abs() { b } else { a })
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
        if self.at.is_none() && self.base == 0.0 && self.paragraph < self.last_paragraph {
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
/// those, only the glyphs whose boxes can reach `view` across, a box in the frame,
/// are looked at, by a search, and those drawn that reach `clip`, in the content's
/// space. The work grows with the glyphs drawn, not with the text. `color` is the
/// fill colour set so far.
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
    color: &mut Option<Color>,
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
    let mut span = text.span(range.start);
    let mut i = range.start;
    while i < range.end {
        work(1);
        let end = text
            .starts
            .get(span + 1)
            .copied()
            .unwrap_or(text.len())
            .min(range.end);
        let style = &text.styles[span];
        let size = style.size.at(set.base);
        if size != 0.0 {
            let s = f64::from(size);
            let (a, b) = (ink.x0 * s, ink.x1 * s);
            let (lo, hi) = (a.min(b), a.max(b));
            let n = end - i;
            // The glyphs run right at a positive size, left at a negative one.
            let (first, last) = if size > 0.0 {
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
                let c = style.color.unwrap_or(set.color);
                if *color != Some(c) {
                    fail(out.fill_color(&c))?;
                    *color = Some(c);
                }
                let mut pen = Pen::new(frame, size, (pos(i), y));
                for k in i + first..i + last {
                    work(1);
                    let g = fonts.glyph(style.face, text.chars[k])?;
                    fail(pen.put(out, pos(k), &g.face.outline(g.id), clip))?;
                }
                pen.end(out);
            }
        }
        i = end;
        span += 1;
    }
    Ok(())
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
    let mut color = Some(set.color);
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
        &mut color,
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
    // How far a glyph's points lie from its baseline, up or down.
    let reach = ink.y0.abs().max(ink.y1.abs()) * f64::from(size.abs());
    let mut color = Some(set.color);
    for (j, line) in text
        .lines(size, Some(f64::from(avail)), Align::Left)
        .enumerate()
    {
        let y = top - (j as f64 + 1.0) * step;
        if !line.glyphs.is_empty() && y + reach >= seen.y0 && y - reach <= seen.y1 {
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
                &mut color,
            )?;
        }
        // The lines after it lie further on, down at a positive size, up at a
        // negative one.
        let next = y - step;
        if (step > 0.0 && next + reach < seen.y0) || (step < 0.0 && next - reach > seen.y1) {
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
    if size == 0.0 || count == 0 {
        return Ok(());
    }
    let at = |i: usize| f64::from(cell * i as f32 + (cell - text.advance(i) * size) / 2.0);
    let slack = (text.widest() * f64::from(size)).abs() / 2.0;
    let face = text.styles[0].face;
    let mut pen = Pen::new(Affine::IDENTITY, size, (at(0), y));
    for i in cells(text.ink, size, f64::from(cell), count, slack, seen) {
        work(1);
        let g = fonts.glyph(face, text.chars[i])?;
        fail(pen.put(out, at(i), &g.face.outline(g.id), seen))?;
    }
    pen.end(out);
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
/// at `set` in `avail`, each line's first baseline `ascent` times its size below
/// its top, in `area`: the lines that can reach both its clip and `seen`, laid out
/// up to the last that can; the sizes and glyph boxes of pieces not reached are
/// within `later`, in points and in em around a glyph's origin, either way.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_box(
    fonts: &Fonts,
    pieces: impl Iterator<Item = Result<Rc<Shaped>, Unsupported>>,
    later: (f32, f64),
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
    let view = area.frame.inverse().transform_rect_bbox(shown);
    let (pad, top) = (f64::from(area.pad), area.top);
    let (mut clipped, mut color) = (false, None);
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
        if pieces.peek().is_some() {
            lines = lines.without_last_paragraph();
        }
        let ink = text.ink.map_or(0.0, |r| r.y0.abs().max(r.y1.abs()));
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
            if line.size != 0.0
                && !line.glyphs.is_empty()
                && y + reach >= view.y0
                && y - reach <= view.y1
            {
                let width = line.width as f32;
                let spaces = text.stretched(line.glyphs.clone());
                let (x0, spacing) = match line.align {
                    Align::Left => (0.0, 0.0),
                    Align::Center => ((avail - width) / 2.0, 0.0),
                    Align::Right => (avail - width, 0.0),
                    Align::Justify if spaces == 0 => (0.0, 0.0),
                    Align::Justify => (0.0, ((avail - width) / spaces as f32).max(0.0)),
                };
                if !clipped {
                    fail(out.clip(area.clip))?;
                    clipped = true;
                }
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
                    &mut color,
                )?;
            }
            // The lines after this one lie below the top of the next, at a positive
            // size, or above it, at a negative one, by no more than their glyphs
            // reach.
            let rest = f64::from(text.rest(line.glyphs.end, set.base).max(later.0));
            let reach = rest * ink.max(later.1);
            let next = f64::from(top - height);
            match direction {
                Direction::Forward if next + reach < view.y0 => return Ok(()),
                Direction::Backward if next - reach > view.y1 => return Ok(()),
                _ => {}
            }
        }
    }
    Ok(())
}

/// `ink`, a box in em around a glyph's origin, at `size`, as `(x0, x1, y0, y1)`.
fn scaled(ink: Option<Rect>, size: f32) -> Option<(f64, f64, f64, f64)> {
    let ink = ink?;
    let s = f64::from(size);
    let (x0, x1) = (ink.x0 * s, ink.x1 * s);
    let (y0, y1) = (ink.y0 * s, ink.y1 * s);
    Some((x0.min(x1), x0.max(x1), y0.min(y1), y0.max(y1)))
}

/// Of lines `0..=last` at `size`, line `j` with its baseline at
/// `top - (j + 1) step`, whose glyphs' points lie in `ink`, in em around each
/// glyph's origin, those whose glyphs can reach `clip`.
pub(super) fn rows_of(
    ink: Option<Rect>,
    size: f32,
    top: f64,
    step: f64,
    last: usize,
    clip: Rect,
) -> Range<usize> {
    let Some((_, _, y0, y1)) = scaled(ink, size) else {
        return 0..0;
    };
    // Line j can reach the clip when j + 1 lies between these.
    let (a, b) = ((top + y1 - clip.y0) / step, (top + y0 - clip.y1) / step);
    within_count(a.min(b) - 1.0, a.max(b) - 1.0, last + 1, clip)
}

/// Of `count` glyphs whose points lie in `ink`, in em around each glyph's origin, at
/// `size` centred in cells `cell` wide from 0, each within `slack` of its cell's
/// middle across, those that can reach `clip` across.
fn cells(
    ink: Option<Rect>,
    size: f32,
    cell: f64,
    count: usize,
    slack: f64,
    clip: Rect,
) -> Range<usize> {
    let Some((x0, x1, _, _)) = scaled(ink, size) else {
        return 0..0;
    };
    // Glyph i, its cell's middle at (i + 0.5) cell, can reach the clip when i lies
    // between these.
    let (a, b) = (
        (clip.x0 - x1 - slack) / cell - 0.5,
        (clip.x1 - x0 + slack) / cell - 0.5,
    );
    within_count(a.min(b), a.max(b), count, clip)
}

/// The indices from `from` up to `to` among `0..count`; none for a clip of no area
/// or bounds that are not numbers.
fn within_count(from: f64, to: f64, count: usize, clip: Rect) -> Range<usize> {
    if clip.x0 > clip.x1 || clip.y0 > clip.y1 || from.is_nan() || to.is_nan() {
        return 0..0;
    }
    let first = from.ceil().max(0.0);
    let last = to.floor() + 1.0;
    // Saturating: beyond the count is the count.
    let (first, last) = (
        first.min(count as f64) as usize,
        last.clamp(0.0, count as f64) as usize,
    );
    first..last.max(first)
}

/// A standard font's glyphs, each outline read once, and the typeface it was loaded
/// for, which names its glyphs' forms.
struct Face {
    font: Font,
    index: usize,
    outlines: RefCell<FxHashMap<GlyphId, Rc<Outline>>>,
    /// The box the points of every glyph lie in, 1000 units to the em.
    bound: OnceCell<Option<Rect>>,
}

/// A glyph's outline, 1000 units to the em, the box of its points, and the form it is
/// drawn as (`None` when it has no points).
struct Outline {
    path: BezPath,
    ink: Rect,
    form: Option<Rc<super::Glyph>>,
}

/// A bare CFF font, as the embedded standard fonts are, read by glyph name with the
/// standard font's metrics, or an OpenType font, read by its character map.
enum Font {
    Cff {
        blob: CffFontBlob,
        standard: StandardFont,
        glyphs: FxHashMap<char, (GlyphId, f32)>,
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
                Rc::new(Outline { path, ink, form })
            })
            .clone()
    }
}

impl Font {
    fn new(data: crate::font::FontData, index: u32, standard: StandardFont) -> Option<Self> {
        if let Some(blob) = CffFontBlob::new(data.clone()) {
            let mut glyphs = FxHashMap::default();
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

/// How a line sits between the sides of its box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
            let before = super::super::work_done();
            let first = shaped.lines(12.0, Some(100.0), Align::Left).next().unwrap();
            costs.push(super::super::work_done() - before);
            assert!(first.glyphs.len() < 30, "{n}");
        }
        assert!(costs[1] < costs[0] + 20, "{costs:?}");
    }
}
