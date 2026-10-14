//! Text in generated appearances: glyphs of the standard fonts the font resolver
//! gives, drawn as outlines so that the appearance needs no font resources.

use super::Unsupported;
use super::out::{Color, Out};
use crate::FontResolverFn;
use crate::font::blob::{CffFontBlob, OpenTypeFontBlob};
use crate::font::{FontQuery, StandardFont, glyph_name_to_unicode};
use kurbo::{Affine, BezPath, Rect};
use rustc_hash::FxHashMap;
use skrifa::GlyphId;
use std::cell::OnceCell;

/// The font families generated text is set in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
                Face::new(data, index, query.standard_substitute())
            })
            .as_ref()
    }

    /// The glyph named `name` in `family`'s font, when that is a bare CFF font as the
    /// embedded standard fonts are.
    pub(super) fn named(&self, family: Family, name: &str) -> Option<Glyph<'_>> {
        let face = self.face(family.into())?;
        let Face::Cff { blob, standard, .. } = face else {
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
        // Tabs and other controls take a space.
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

impl Glyph<'_> {
    pub(super) fn is_space(&self) -> bool {
        self.space
    }
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
    fill(glyphs, size, origin, 0.0, transform, None, out)
}

/// Fills `glyphs` as [`draw`] does, each space `spacing` wider than its advance,
/// under a clip to `clip`: a glyph wholly outside it is not written, so text
/// running far beyond its box costs no more output than the box shows.
pub(super) fn draw_clipped(
    glyphs: &[Glyph<'_>],
    size: f32,
    origin: (f32, f32),
    spacing: f32,
    transform: Affine,
    clip: Rect,
    out: &mut Out,
) -> Option<()> {
    fill(glyphs, size, origin, spacing, transform, Some(clip), out)
}

fn fill(
    glyphs: &[Glyph<'_>],
    size: f32,
    (x, y): (f32, f32),
    spacing: f32,
    transform: Affine,
    clip: Option<Rect>,
    out: &mut Out,
) -> Option<()> {
    let mut x = f64::from(x);
    let scale = f64::from(size) / 1000.0;
    let mut any = false;
    for g in glyphs {
        let mut path = g.face.outline(g.id);
        if !path.elements().is_empty() {
            path.apply_affine(transform * Affine::new([scale, 0.0, 0.0, scale, x, f64::from(y)]));
            any |= match clip {
                Some(clip) => out.path_within(&path, clip)?,
                None => out.path(&path).is_some(),
            };
        }
        x += f64::from(g.advance) * f64::from(size);
        if g.space {
            x += f64::from(spacing);
        }
    }
    if any {
        out.op("f");
    }
    Some(())
}

/// A standard font's glyphs: a bare CFF font, as the embedded standard fonts are,
/// read by glyph name with the standard font's metrics, or an OpenType font, read by
/// its character map.
enum Face {
    Cff {
        blob: CffFontBlob,
        standard: StandardFont,
        glyphs: FxHashMap<char, (GlyphId, f32)>,
    },
    OpenType(OpenTypeFontBlob),
}

impl Face {
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

/// How text is set: typeface, size in points, the fill colour operator, and how
/// the lines of its paragraph are aligned.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Style {
    pub(super) face: Typeface,
    pub(super) size: f32,
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
/// (fit to the field) is `None`; without `Tf` the size is 12 pt, as common viewers
/// set it.
pub(super) fn default_appearance(da: &[u8]) -> (Family, Option<f32>, Option<Color>) {
    let tokens: Vec<&[u8]> = da
        .split(|b| b.is_ascii_whitespace())
        .filter(|t| !t.is_empty())
        .collect();
    let mut family = Family::Sans;
    let mut size = Some(12.0);
    let mut color = None;
    for (i, op) in tokens.iter().enumerate() {
        // Numbers as PDF writes them (7.3.3), however large: one too large to draw
        // fails the appearance where it is drawn.
        let operands = |n: usize| -> Option<Vec<f64>> {
            let from = i.checked_sub(n)?;
            tokens[from..i]
                .iter()
                .map(|t| {
                    let number = t.iter().all(|b| b.is_ascii_digit() || b"+-.".contains(b));
                    number.then(|| std::str::from_utf8(t).ok()?.parse::<f64>().ok())?
                })
                .collect::<Option<Vec<f64>>>()
        };
        match *op {
            b"Tf" if i >= 2 => {
                family =
                    Family::of_font_name(tokens[i - 2].strip_prefix(b"/").unwrap_or(tokens[i - 2]));
                size = operands(1).map(|s| s[0] as f32).filter(|s| *s > 0.0);
            }
            b"g" | b"rg" | b"k" => {
                let n = match *op {
                    b"g" => 1,
                    b"rg" => 3,
                    _ => 4,
                };
                if let Some(c) = operands(n) {
                    color = Color::of(&c);
                }
            }
            _ => {}
        }
    }
    (family, size, color)
}

/// The text of a text string (PDF 32000-1 7.9.2.2): UTF-16BE or UTF-8 with a byte
/// order mark, else `PDFDocEncoding` (read as Latin-1 for the printable range).
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

/// The lines of `text` set in `family` at `size`: paragraphs split at line breaks,
/// and, with a `width`, wrapped between words, a word wider than the line between
/// its characters.
pub(super) fn lines<'f>(
    fonts: &'f Fonts,
    family: Family,
    size: f32,
    text: &str,
    width: Option<f32>,
) -> Result<Vec<Vec<Glyph<'f>>>, Unsupported> {
    let span = Span {
        text: text.to_owned(),
        style: Style {
            face: family.into(),
            size,
            color: Color::BLACK,
            align: Align::Left,
        },
    };
    Ok(layout(fonts, &[span], width)?
        .into_iter()
        .map(|line| line.glyphs.into_iter().map(|g| g.glyph).collect())
        .collect())
}

/// Text in one style.
#[derive(Debug)]
pub(super) struct Span {
    pub(super) text: String,
    pub(super) style: Style,
}

/// A glyph at the size of the style of span `span` it was set from.
#[derive(Clone)]
pub(super) struct Set<'f> {
    pub(super) glyph: Glyph<'f>,
    pub(super) size: f32,
    pub(super) span: usize,
}

impl Set<'_> {
    fn width(&self) -> f32 {
        self.glyph.advance * self.size
    }
}

/// A line of set text.
pub(super) struct Line<'f> {
    pub(super) glyphs: Vec<Set<'f>>,
    /// The largest size on the line, or, on an empty one, its paragraph's.
    pub(super) size: f32,
    /// The alignment of its paragraph, but left for the last line of a justified
    /// one.
    pub(super) align: Align,
}

impl Line<'_> {
    pub(super) fn width(&self) -> f32 {
        self.glyphs.iter().map(Set::width).sum()
    }

    /// The number of spaces a justified line stretches at.
    pub(super) fn spaces(&self) -> usize {
        self.glyphs.iter().filter(|g| g.glyph.is_space()).count()
    }
}

/// The lines of `spans`, each glyph in its span's family and size: paragraphs split
/// at line breaks, and, with a `width`, wrapped between words, a word wider than
/// the line between its characters. A line is aligned as the span of its first
/// glyph, or of its paragraph's break when it has none.
pub(super) fn layout<'f>(
    fonts: &'f Fonts,
    spans: &[Span],
    width: Option<f32>,
) -> Result<Vec<Line<'f>>, Unsupported> {
    // Each character with its span; a line break is a paragraph's end. A carriage
    // return is a line break, one with the line feed after it, in whichever span.
    let all: Vec<(char, usize)> = spans
        .iter()
        .enumerate()
        .flat_map(|(i, span)| span.text.chars().map(move |c| (c, i)))
        .collect();
    let mut chars: Vec<(char, usize)> = Vec::with_capacity(all.len());
    for (k, &(c, span)) in all.iter().enumerate() {
        match c {
            '\r' if all.get(k + 1).is_some_and(|(next, _)| *next == '\n') => {}
            '\r' => chars.push(('\n', span)),
            _ => chars.push((c, span)),
        }
    }
    let mut out = Vec::new();
    let mut start = 0;
    for paragraph in chars.split(|(c, _)| *c == '\n') {
        // An empty paragraph is as high as the break that ends it, or the text
        // before it.
        let span = paragraph
            .first()
            .or(chars.get(start + paragraph.len()))
            .or(chars.last())
            .map_or(0, |(_, span)| *span);
        let para_size = spans.get(span).map_or(0.0, |s| s.style.size);
        let para_align = spans.get(span).map_or(Align::Left, |s| s.style.align);
        start += paragraph.len() + 1;
        let glyphs = paragraph
            .iter()
            .map(|&(c, span)| {
                let style = &spans[span].style;
                Ok(Set {
                    glyph: fonts.glyph(style.face, c)?,
                    size: style.size,
                    span,
                })
            })
            .collect::<Result<Vec<_>, Unsupported>>()?;
        let ranges = match width {
            Some(width) => breaks(
                glyphs.len(),
                |k| paragraph[k].0 == ' ',
                |k| glyphs[k].width(),
                width,
            ),
            None => std::iter::once(0..glyphs.len()).collect(),
        };
        let count = ranges.len();
        for (n, range) in ranges.into_iter().enumerate() {
            let line = glyphs[range].to_vec();
            let size = line.iter().map(|g| g.size).fold(0.0, f32::max);
            let align = match line
                .first()
                .map_or(para_align, |g| spans[g.span].style.align)
            {
                Align::Justify if n + 1 == count => Align::Left,
                align => align,
            };
            out.push(Line {
                size: if line.is_empty() { para_size } else { size },
                glyphs: line,
                align,
            });
        }
    }
    Ok(out)
}

/// The lines `n` glyphs are broken into to fit `width`: each line as many words
/// as fit, with the spaces before each but the first; a word wider than a line
/// alone on its line, overflowing it, as viewers set it. `space(k)` tells whether
/// glyph `k` is a space and `advance(k)` its width; each is asked at most twice
/// per glyph.
fn breaks(
    n: usize,
    space: impl Fn(usize) -> bool,
    advance: impl Fn(usize) -> f32,
    width: f32,
) -> Vec<std::ops::Range<usize>> {
    let mut lines = Vec::new();
    // The current line, from `start` to `i`, and its width.
    let (mut start, mut line_width) = (0, 0.0_f32);
    let mut i = 0;
    while i < n {
        // The next word with the spaces before it.
        let mut j = i;
        while j < n && space(j) {
            j += 1;
        }
        while j < n && !space(j) {
            j += 1;
        }
        let word_width: f32 = (i..j).map(&advance).sum();
        if start == i || line_width + word_width <= width {
            line_width += word_width;
            i = j;
        } else {
            lines.push(start..i);
            line_width = 0.0;
            // The next line starts with the word, not the spaces before it.
            while i < j && space(i) {
                i += 1;
            }
            start = i;
        }
    }
    lines.push(start..i);
    lines
}

#[cfg(test)]
mod tests {
    use super::breaks;
    use std::cell::Cell;

    /// Lines as the characters of `text` with 1-wide glyphs.
    fn wrap(text: &str, width: f32) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        breaks(chars.len(), |k| chars[k] == ' ', |_| 1.0, width)
            .into_iter()
            .map(|r| chars[r].iter().collect())
            .collect()
    }

    #[test]
    fn words_wrap_and_a_long_word_overflows_its_line() {
        assert_eq!(wrap("ab cd ef", 5.0), ["ab cd", "ef"]);
        assert_eq!(wrap("abcdefgh ij", 3.0), ["abcdefgh", "ij"]);
        assert_eq!(wrap("ab  abcdefg ij", 3.0), ["ab", "abcdefg", "ij"]);
        assert_eq!(wrap("  ab", 3.0), ["  ab"]);
        assert_eq!(wrap("", 3.0), [""]);
        assert_eq!(wrap("abc", 0.0), ["abc"]);
    }

    /// Each glyph is looked at a bounded number of times, however long the word
    /// wider than the line or the words after it.
    #[test]
    fn wrapping_a_long_word_is_linear() {
        for n in [1_000, 10_000, 100_000] {
            let (spaces, advances) = (Cell::new(0), Cell::new(0));
            // A word as long as the text, then words of one glyph.
            let lines = breaks(
                n,
                |k| {
                    spaces.set(spaces.get() + 1);
                    k > n / 2 && k % 2 == 1
                },
                |_| {
                    advances.set(advances.get() + 1);
                    1.0
                },
                7.0,
            );
            assert!(spaces.get() <= 2 * n && advances.get() <= 2 * n, "{n}");
            assert_eq!(lines[0], 0..n / 2 + 1, "{n}");
        }
    }
}
