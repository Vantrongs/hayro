//! Text in generated appearances: glyphs of the standard fonts the font resolver
//! gives, drawn as outlines so that the appearance needs no font resources.

use super::{Out, Unsupported};
use crate::FontResolverFn;
use crate::font::blob::{CffFontBlob, OpenTypeFontBlob};
use crate::font::{FontQuery, StandardFont, glyph_name_to_unicode};
use kurbo::{Affine, BezPath};
use rustc_hash::FxHashMap;
use skrifa::GlyphId;
use std::cell::OnceCell;
use std::fmt::Write;

/// The font families generated text is set in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    /// Helvetica.
    Sans,
    /// Times Roman.
    Serif,
    /// Courier.
    Mono,
    /// Times Bold, the stamps' face.
    SerifBold,
    /// `ZapfDingbats`, the check box mark.
    Dingbats,
}

impl Family {
    const ALL: [Self; 5] = [
        Self::Sans,
        Self::Serif,
        Self::Mono,
        Self::SerifBold,
        Self::Dingbats,
    ];

    fn standard(self) -> StandardFont {
        match self {
            Self::Sans => StandardFont::Helvetica,
            Self::Serif => StandardFont::TimesRoman,
            Self::Mono => StandardFont::Courier,
            Self::SerifBold => StandardFont::TimesBold,
            Self::Dingbats => StandardFont::ZapfDingBats,
        }
    }

    /// The family a default appearance font name stands for: the base fonts of the
    /// names Acrobat puts in `/DR` (`Helv`, `TiRo`, `Cour`, …) or a base font name,
    /// by its family; Helvetica for any other.
    pub(super) fn of_font_name(name: &[u8]) -> Self {
        let name = String::from_utf8_lossy(name).to_ascii_lowercase();
        if name.starts_with("tiro")
            || name.starts_with("times")
            || name.starts_with("tibo")
            || name.starts_with("tiit")
            || name.starts_with("tibi")
        {
            Self::Serif
        } else if name.starts_with("cour")
            || name.starts_with("cobo")
            || name.starts_with("coit")
            || name.starts_with("cobi")
        {
            Self::Mono
        } else {
            Self::Sans
        }
    }
}

/// The fonts generated text is drawn with: for each family, the standard font the
/// font resolver gives; a character that font lacks is drawn in the Helvetica one.
/// A character neither has makes the appearance unsupported.
///
/// Fonts are loaded on first use; make one per page, or per document, and reuse it.
pub(super) struct Fonts {
    resolver: FontResolverFn,
    faces: [OnceCell<Option<Face>>; 5],
}

impl Fonts {
    /// The fonts `resolver` gives for the standard font queries.
    pub(super) fn new(resolver: FontResolverFn) -> Self {
        Self {
            resolver,
            faces: Default::default(),
        }
    }

    fn face(&self, family: Family) -> Option<&Face> {
        let i = Family::ALL.iter().position(|f| *f == family)?;
        self.faces[i]
            .get_or_init(|| {
                let (data, index) = (self.resolver)(&FontQuery::Standard(family.standard()))?;
                Face::new(data, index, family.standard())
            })
            .as_ref()
    }

    /// The glyph named `name` in `family`'s font, when that is a bare CFF font as the
    /// embedded standard fonts are.
    pub(super) fn named(&self, family: Family, name: &str) -> Option<Glyph<'_>> {
        let face = self.face(family)?;
        let Face::Cff { blob, standard, .. } = face else {
            return None;
        };
        let id = blob.glyph_index_by_name(name)?;
        let advance = standard.get_width(name)? / 1000.0;
        Some(Glyph { face, id, advance })
    }

    /// The glyphs of `text` in `family`, each with its advance in em.
    pub(super) fn shape(&self, family: Family, text: &str) -> Result<Vec<Glyph<'_>>, Unsupported> {
        text.chars().map(|c| self.glyph(family, c)).collect()
    }

    /// The glyph of `c` in `family`, with its advance in em.
    fn glyph(&self, family: Family, c: char) -> Result<Glyph<'_>, Unsupported> {
        // Tabs and other controls take a space.
        let c = if c.is_control() { ' ' } else { c };
        [family, Family::Sans]
            .into_iter()
            .find_map(|f| {
                let face = self.face(f)?;
                let (id, advance) = face.glyph(c)?;
                Some(Glyph { face, id, advance })
            })
            .ok_or(if self.face(Family::Sans).is_some() {
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
}

/// The width of `glyphs` at `size`.
pub(super) fn width(glyphs: &[Glyph<'_>], size: f32) -> f32 {
    glyphs.iter().map(|g| g.advance).sum::<f32>() * size
}

/// Fills `glyphs` at `size` with their baseline from `(x, y)`, in `transform`.
pub(super) fn draw(
    glyphs: &[Glyph<'_>],
    size: f32,
    (x, y): (f32, f32),
    transform: Affine,
    out: &mut Out,
) -> Option<()> {
    let mut x = f64::from(x);
    let scale = f64::from(size) / 1000.0;
    let mut any = false;
    for g in glyphs {
        let mut path = g.face.outline(g.id);
        if !path.elements().is_empty() {
            path.apply_affine(transform * Affine::new([scale, 0.0, 0.0, scale, x, f64::from(y)]));
            out.path(&path)?;
            any = true;
        }
        x += f64::from(g.advance) * f64::from(size);
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

/// How text is set: family, size in points, and the fill colour operator.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Style {
    pub(super) family: Family,
    pub(super) size: f32,
    pub(super) color: String,
}

/// The style of a default appearance string (PDF 32000-1 12.7.3.3): the font
/// operand of `Tf` and its size, and the last colour operator. The font name is
/// looked up in `fonts` (an `/DR /Font` dictionary) for its `/BaseFont`. A size of 0
/// (fit to the field) is `None`; without `Tf` the size is 12 pt, as common viewers
/// set it.
pub(super) fn default_appearance(
    da: &[u8],
    fonts: Option<&hayro_syntax::object::Dict<'_>>,
) -> (Family, Option<f32>, Option<String>) {
    let tokens: Vec<&[u8]> = da
        .split(|b| b.is_ascii_whitespace())
        .filter(|t| !t.is_empty())
        .collect();
    let mut family = Family::Sans;
    let mut size = Some(12.0);
    let mut color = None;
    for (i, op) in tokens.iter().enumerate() {
        let operands = |n: usize| -> Option<Vec<f32>> {
            let from = i.checked_sub(n)?;
            tokens[from..i]
                .iter()
                .map(|t| std::str::from_utf8(t).ok()?.parse::<f32>().ok())
                .collect::<Option<Vec<f32>>>()
                .filter(|v| v.iter().all(|n| n.is_finite()))
        };
        match *op {
            b"Tf" if i >= 2 => {
                let name = tokens[i - 2].strip_prefix(b"/").unwrap_or(tokens[i - 2]);
                let base = fonts
                    .and_then(|f| f.get::<hayro_syntax::object::Dict<'_>>(name))
                    .and_then(|f| f.get::<hayro_syntax::object::Name<'_>>(b"BaseFont"));
                family = Family::of_font_name(base.as_ref().map_or(name, |b| b.as_ref()));
                size = operands(1).map(|s| s[0]).filter(|s| *s > 0.0);
            }
            b"g" | b"rg" | b"k" => {
                let n = match *op {
                    b"g" => 1,
                    b"rg" => 3,
                    _ => 4,
                };
                if let Some(c) = operands(n) {
                    let mut s = String::new();
                    for v in c {
                        let _ = write!(s, "{} ", v.clamp(0.0, 1.0));
                    }
                    s.push_str(std::str::from_utf8(op).unwrap_or("g"));
                    color = Some(s);
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
            family,
            size,
            color: String::new(),
        },
    };
    Ok(layout(fonts, &[span], width)?
        .into_iter()
        .map(|line| line.glyphs.into_iter().map(|g| g.glyph).collect())
        .collect())
}

/// Text in one style.
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
}

impl Line<'_> {
    pub(super) fn width(&self) -> f32 {
        self.glyphs.iter().map(Set::width).sum()
    }
}

/// The lines of `spans`, each glyph in its span's family and size: paragraphs split
/// at line breaks, and, with a `width`, wrapped between words, a word wider than
/// the line between its characters.
pub(super) fn layout<'f>(
    fonts: &'f Fonts,
    spans: &[Span],
    width: Option<f32>,
) -> Result<Vec<Line<'f>>, Unsupported> {
    // Each character with its span; a line break is a paragraph's end.
    let mut chars: Vec<(char, usize)> = Vec::new();
    for (i, span) in spans.iter().enumerate() {
        let text = span.text.replace("\r\n", "\n").replace('\r', "\n");
        chars.extend(text.chars().map(|c| (c, i)));
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
        start += paragraph.len() + 1;
        let glyphs = paragraph
            .iter()
            .map(|&(c, span)| {
                let style = &spans[span].style;
                Ok(Set {
                    glyph: fonts.glyph(style.family, c)?,
                    size: style.size,
                    span,
                })
            })
            .collect::<Result<Vec<_>, Unsupported>>()?;
        let mut push = |glyphs: Vec<Set<'f>>| {
            let size = glyphs.iter().map(|g| g.size).fold(0.0, f32::max);
            out.push(Line {
                size: if glyphs.is_empty() { para_size } else { size },
                glyphs,
            });
        };
        let Some(width) = width else {
            push(glyphs);
            continue;
        };
        let space = |k: usize| paragraph[k].0 == ' ';
        let mut line: Vec<Set<'f>> = Vec::new();
        let mut line_width = 0.0;
        let mut i = 0;
        while i < glyphs.len() {
            // The next word with the spaces before it.
            let mut j = i;
            while j < glyphs.len() && space(j) {
                j += 1;
            }
            while j < glyphs.len() && !space(j) {
                j += 1;
            }
            let word_width: f32 = glyphs[i..j].iter().map(Set::width).sum();
            if line_width + word_width <= width || line.is_empty() && word_width <= width {
                line.extend_from_slice(&glyphs[i..j]);
                line_width += word_width;
                i = j;
            } else if line.is_empty() {
                // A word wider than the line: as much of it as fits, at least one
                // character.
                let mut k = i;
                while k < j && (k == i || line_width + glyphs[k].width() <= width) {
                    line_width += glyphs[k].width();
                    line.push(glyphs[k].clone());
                    k += 1;
                }
                push(std::mem::take(&mut line));
                line_width = 0.0;
                i = k;
            } else {
                push(std::mem::take(&mut line));
                line_width = 0.0;
                // The next line starts with the word, not the spaces before it.
                while i < j && space(i) {
                    i += 1;
                }
            }
        }
        push(line);
    }
    Ok(out)
}

/// The style rich text (PDF 32000-1 12.7.3.4) sets: `font-size`, `font-family`
/// (also in the `font` shorthand) and `color` declarations of `css`, over `style`.
pub(super) fn css_style(css: &str, mut style: Style) -> Style {
    for declaration in css.split(';') {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match property.trim().to_ascii_lowercase().as_str() {
            "font-size" => {
                if let Some(size) = css_size(value) {
                    style.size = size;
                }
            }
            "font-family" => style.family = css_family(value),
            "font" => {
                // [style] [weight] size[/line-height] family…
                let mut words = value.split_whitespace();
                for word in words.by_ref() {
                    if let Some(size) = css_size(word.split('/').next().unwrap_or(word)) {
                        style.size = size;
                        break;
                    }
                }
                let family: Vec<&str> = words.collect();
                if !family.is_empty() {
                    style.family = css_family(&family.join(" "));
                }
            }
            "color" => {
                if let Some(color) = css_color(value) {
                    style.color = color;
                }
            }
            _ => {}
        }
    }
    style
}

fn css_size(value: &str) -> Option<f32> {
    let number = value.trim_end_matches("pt").trim_end_matches("px");
    number
        .parse::<f32>()
        .ok()
        .filter(|s| s.is_finite() && *s > 0.0)
}

fn css_family(value: &str) -> Family {
    let first = value
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches(['\'', '"']);
    let lower = first.to_ascii_lowercase();
    if lower.contains("courier") || lower == "monospace" {
        Family::Mono
    } else if lower.contains("helvetica") || lower.contains("arial") || lower == "sans-serif" {
        Family::Sans
    } else {
        Family::Serif
    }
}

/// A CSS colour as an RGB fill operator: `#rrggbb`, `#rgb` or `rgb(r, g, b)`.
fn css_color(value: &str) -> Option<String> {
    let rgb: [f32; 3] = if let Some(hex) = value.strip_prefix('#') {
        let digits: Vec<u32> = hex.chars().map(|c| c.to_digit(16)).collect::<Option<_>>()?;
        match digits.len() {
            6 => [0, 2, 4].map(|i| (digits[i] * 16 + digits[i + 1]) as f32 / 255.0),
            3 => [0, 1, 2].map(|i| (digits[i] * 17) as f32 / 255.0),
            _ => return None,
        }
    } else {
        let inner = value.strip_prefix("rgb(")?.strip_suffix(')')?;
        let parts: Vec<f32> = inner
            .split(',')
            .map(|p| p.trim().parse::<f32>().ok())
            .collect::<Option<_>>()?;
        let [r, g, b] = parts.as_slice() else {
            return None;
        };
        [*r, *g, *b].map(|v| (v / 255.0).clamp(0.0, 1.0))
    };
    let [r, g, b] = rgb.map(|v| (v * 10000.0).round() / 10000.0);
    Some(format!("{r} {g} {b} rg"))
}

/// The text of rich text (PDF 32000-1 12.7.3.4, XHTML) in the styles its elements
/// set: each element's style attribute over the style of the element it is in, the
/// outermost over `base`. Paragraphs and breaks are line breaks, other whitespace is
/// kept, entities are decoded.
pub(super) fn rich_text(xhtml: &str, base: &Style) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let add = |spans: &mut Vec<Span>, text: &str, style: &Style| {
        if text.is_empty() {
            return;
        }
        match spans.last_mut() {
            Some(last) if last.style == *style => last.text.push_str(text),
            _ => spans.push(Span {
                text: text.to_owned(),
                style: style.clone(),
            }),
        }
    };
    // The style of each open element, outermost first.
    let mut open = vec![base.clone()];
    let mut rest = xhtml;
    while let Some(lt) = rest.find('<') {
        add(
            &mut spans,
            &decode_entities(&rest[..lt]),
            open.last().unwrap_or(base),
        );
        let Some(gt) = rest[lt..].find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[lt + 1..lt + gt];
        rest = &rest[lt + gt + 1..];
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        let current = open.last().unwrap_or(base).clone();
        let at_line_start = spans.last().is_none_or(|s| s.text.ends_with('\n'));
        match name.as_str() {
            // A paragraph starts and ends a line; whitespace is kept as written.
            "p" | "div" if !at_line_start => add(&mut spans, "\n", &current),
            "br" => add(&mut spans, "\n", &current),
            _ => {}
        }
        if closing {
            if open.len() > 1 {
                open.pop();
            }
        } else if !tag.ends_with('/') && name != "br" {
            let style = match tag.find("style=") {
                // The value between its quotes.
                Some(at) => {
                    let mut value = tag[at + 6..].chars();
                    match value
                        .next()
                        .and_then(|quote| value.as_str().split(quote).next())
                    {
                        Some(css) => css_style(css, current),
                        None => current,
                    }
                }
                None => current,
            };
            open.push(style);
        }
    }
    add(
        &mut spans,
        &decode_entities(rest),
        open.last().unwrap_or(base),
    );
    spans
}

fn decode_entities(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let Some(semi) = after.find(';').filter(|&i| i <= 10) else {
            out.push('&');
            rest = after;
            continue;
        };
        let entity = &after[..semi];
        let c = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{A0}'),
            _ => entity.strip_prefix('#').and_then(|n| {
                let code = match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse().ok(),
                };
                code.and_then(char::from_u32)
            }),
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::{Family, Style, rich_text};

    fn base() -> Style {
        Style {
            family: Family::Serif,
            size: 12.0,
            color: "0 g".to_owned(),
        }
    }

    /// A paragraph starts and ends a line; spaces and line breaks in the text stay.
    #[test]
    fn rich_text_keeps_whitespace_and_breaks_at_paragraphs() {
        let spans = rich_text(
            "<?xml version=\"1.0\"?><body style=\"color:#f00\"><p>A</p>  <p>  B  </p>\
             <span>C</span> D<br/>E &amp;\n F</body>",
            &base(),
        );
        let text: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(text, "A\n  \n  B  \nC D\nE &\n F");
        assert!(spans.iter().all(|s| s.style.color == "1 0 0 rg"));
        // An attribute without a value.
        let spans = rich_text("<p style=>x</p>", &base());
        assert_eq!((spans.len(), spans[0].text.as_str()), (1, "x\n"));
        assert_eq!(spans[0].style, base());
    }

    /// An element's style applies to what it holds, over the style of the element it
    /// is in; the next element starts from that again.
    #[test]
    fn rich_text_styles_nest() {
        let spans = rich_text(
            "<body style=\"font-size:10pt\"><p style=\"font-family:Helvetica\">A \
             <span style=\"color:#ff0000\">B</span> C</p><p>D</p></body>",
            &base(),
        );
        let got: Vec<(&str, Family, f32, &str)> = spans
            .iter()
            .map(|s| {
                (
                    s.text.as_str(),
                    s.style.family,
                    s.style.size,
                    s.style.color.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("A ", Family::Sans, 10.0, "0 g"),
                ("B", Family::Sans, 10.0, "1 0 0 rg"),
                (" C\n", Family::Sans, 10.0, "0 g"),
                ("D\n", Family::Serif, 10.0, "0 g"),
            ]
        );
    }
}
