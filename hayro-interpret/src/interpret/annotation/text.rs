//! Text in generated appearances: glyphs of the standard fonts the font resolver
//! gives, drawn as outlines so that the appearance needs no font resources.

use super::Unsupported;
use super::entries::real;
use super::out::{Color, Out};
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
                Face::new(data, index, query.standard_substitute())
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
    let origin = (f64::from(origin.0), f64::from(origin.1));
    fill(glyphs, size, origin, 0.0, transform, None, out)
}

/// Fills `glyphs` at `size` from `(x, y)` in `transform`, each space `spacing`
/// wider than its advance, those that reach `clip` alone when there is one: a glyph
/// wholly outside it is not written, so text running far beyond its box costs no
/// more output than the box shows. A glyph is tested by the box of its outline's
/// points before its outline is taken: one outside the clip costs no outline, and
/// the box's corners being written stands for its points being written.
fn fill(
    glyphs: &[Glyph<'_>],
    size: f32,
    (mut x, y): (f64, f64),
    spacing: f32,
    transform: Affine,
    clip: Option<Rect>,
    out: &mut Out,
) -> Option<()> {
    let scale = f64::from(size) / 1000.0;
    let mut any = false;
    for g in glyphs {
        let outline = g.face.outline(g.id);
        if !outline.path.elements().is_empty() {
            let at = transform * Affine::new([scale, 0.0, 0.0, scale, x, y]);
            let reaches = match clip {
                Some(clip) => out.box_reaches(at, outline.ink, clip)?,
                None => true,
            };
            if reaches {
                let mut path = outline.path.clone();
                path.apply_affine(at);
                any |= match clip {
                    Some(clip) => out.path_within(&path, clip)?,
                    None => out.path(&path).is_some(),
                };
            }
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

/// Text set in one typeface, of which the part a clip shows is drawn alone
/// ([`draw_run`], [`wrap`]): its characters, where each glyph starts, and the box
/// around a glyph's origin the points of every one of its glyphs lie in. A value many
/// widgets show is set once for the document.
pub(super) struct Run {
    typeface: Typeface,
    chars: Vec<char>,
    /// The advances of the glyphs before glyph `k`, in em, for `k` up to the count.
    starts: Vec<f64>,
    /// The box of the points of all glyphs, each at the origin, in em; `None` when
    /// no glyph has any.
    ink: Option<Rect>,
    /// The largest advance, in em.
    widest: f64,
}

impl Run {
    pub(super) fn typeface(&self) -> Typeface {
        self.typeface
    }

    pub(super) fn len(&self) -> usize {
        self.chars.len()
    }

    /// The box around a glyph's origin the points of each of its glyphs lie in, in
    /// em; `None` when no glyph has any.
    pub(super) fn ink(&self) -> Option<Rect> {
        self.ink
    }

    /// The largest advance, in em.
    pub(super) fn widest(&self) -> f64 {
        self.widest
    }

    /// The advance of glyphs `range`, in em.
    pub(super) fn width(&self, range: Range<usize>) -> f64 {
        self.starts[range.end] - self.starts[range.start]
    }

    /// The box glyphs `range` at `size` from `(x, y)` lie in, given that each glyph
    /// lies `extra` further along either way; `None` when none has points.
    fn bounds(
        &self,
        range: Range<usize>,
        size: f64,
        (x, y): (f64, f64),
        extra: f64,
    ) -> Option<Rect> {
        let ink = self.ink?;
        let end = x + self.width(range) * size;
        let xs = [
            x + ink.x0 * size,
            x + ink.x1 * size,
            end + ink.x0 * size,
            end + ink.x1 * size,
        ];
        let (x0, x1) = xs
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &v| {
                (a.min(v), b.max(v))
            });
        let (y0, y1) = (y + ink.y0 * size, y + ink.y1 * size);
        Some(Rect::new(x0 - extra, y0.min(y1), x1 + extra, y0.max(y1)))
    }
}

impl Fonts {
    /// `text` set in `typeface` as [`Fonts::shape`] sets it, as a [`Run`].
    pub(super) fn run(
        &self,
        typeface: impl Into<Typeface>,
        text: &str,
    ) -> Result<Run, Unsupported> {
        let typeface = typeface.into();
        let chars: Vec<char> = text.chars().collect();
        let mut starts = Vec::with_capacity(chars.len() + 1);
        let (mut at, mut widest, mut ink) = (0.0, 0.0_f64, None::<Rect>);
        starts.push(at);
        for &c in &chars {
            let g = self.glyph(typeface, c)?;
            let outline = g.face.outline(g.id);
            if !outline.path.elements().is_empty() {
                ink = Some(ink.map_or(outline.ink, |r| r.union(outline.ink)));
            }
            at += f64::from(g.advance);
            widest = widest.max(f64::from(g.advance));
            starts.push(at);
        }
        Ok(Run {
            typeface,
            chars,
            starts,
            ink: ink.map(|r| r.scale_from_origin(0.001)),
            widest,
        })
    }
}

/// Fills glyphs `range` of `run` at `size` with their baseline from `(x, y)`, as
/// [`fill`] does under a clip, but only those whose box can reach `clip` are looked at:
/// the work grows with the glyphs the clip shows, not with the run. Those not looked
/// at are written nowhere, but where they lie must be writable, as it must be for a
/// glyph the clip cuts away.
pub(super) fn draw_run(
    fonts: &Fonts,
    run: &Run,
    range: Range<usize>,
    size: f32,
    (x, y): (f64, f64),
    clip: Rect,
    out: &mut Out,
) -> Option<()> {
    let s = f64::from(size);
    let Some(all) = run.bounds(range.clone(), s, (x, y), 0.0) else {
        return Some(());
    };
    out.within(all)?;
    let ink = run.ink?;
    let (i0, i1) = (ink.x0 * s, ink.x1 * s);
    let (lo, hi) = (i0.min(i1), i0.max(i1));
    let seen = all.y1 >= clip.y0 && all.y0 <= clip.y1 && clip.x0 <= clip.x1 && clip.y0 <= clip.y1;
    if !seen || s == 0.0 {
        return Some(());
    }
    // Glyph k starts at x + (starts[k] - starts[range.start]) s; it can reach the clip
    // when that lies in [clip.x0 - hi, clip.x1 - lo].
    let base = run.starts[range.start];
    let (a, b) = ((clip.x0 - hi - x) / s + base, (clip.x1 - lo - x) / s + base);
    let (from, to) = (a.min(b), a.max(b));
    let starts = &run.starts[range.clone()];
    let first = range.start + starts.partition_point(|&v| v < from);
    let last = range.start + starts.partition_point(|&v| v <= to);
    if first >= last {
        return Some(());
    }
    let glyphs = run.chars[first..last]
        .iter()
        .map(|&c| fonts.glyph(run.typeface, c).ok())
        .collect::<Option<Vec<_>>>()?;
    let origin = (x + (run.starts[first] - base) * s, y);
    fill(
        &glyphs,
        size,
        origin,
        0.0,
        Affine::IDENTITY,
        Some(clip),
        out,
    )
}

/// The lines of a [`Run`] ([`wrap`]): ranges of its glyphs, and the advance of the
/// widest, in em.
pub(super) struct Lines {
    pub(super) ranges: Vec<Range<usize>>,
    pub(super) widest: f64,
}

/// The ink of `run`'s glyphs, at `size`, across and up from a glyph's origin.
fn ink(run: &Run, size: f32) -> Option<(f64, f64, f64, f64)> {
    scaled(run.ink, size)
}

/// `ink`, a box in em around a glyph's origin, at `size`, as `(x0, x1, y0, y1)`.
fn scaled(ink: Option<Rect>, size: f32) -> Option<(f64, f64, f64, f64)> {
    let ink = ink?;
    let s = f64::from(size);
    let (x0, x1) = (ink.x0 * s, ink.x1 * s);
    let (y0, y1) = (ink.y0 * s, ink.y1 * s);
    Some((x0.min(x1), x0.max(x1), y0.min(y1), y0.max(y1)))
}

/// `None`, failing the content, when a glyph of `run` at `size` whose origin lies
/// within `slack` of `(x, y)` across could have a point that cannot be written.
pub(super) fn reach(
    run: &Run,
    size: f32,
    (x, y): (f64, f64),
    slack: f64,
    out: &mut Out,
) -> Option<()> {
    let Some((x0, x1, y0, y1)) = ink(run, size) else {
        return Some(());
    };
    out.within(Rect::new(x + x0 - slack, y + y0, x + x1 + slack, y + y1))
}

/// Of lines `0..=last` of `run` at `size`, line `j` with its baseline at
/// `top - (j + 1) step`, those whose glyphs can reach `clip`.
pub(super) fn rows(
    run: &Run,
    size: f32,
    top: f64,
    step: f64,
    last: usize,
    clip: Rect,
) -> Range<usize> {
    rows_of(run.ink, size, top, step, last, clip)
}

/// [`rows`] of lines whose glyphs' points lie in `ink`, in em around each glyph's
/// origin.
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

/// `None`, failing the content, when a glyph of lines set at `size` could have a
/// point that cannot be written: lines whose glyphs' points lie in `ink`, in em
/// around each glyph's origin, none advancing more than `widest` em, each from an
/// origin on the segment from `first` to `last`.
pub(super) fn reach_all(
    ink: Option<Rect>,
    widest: f64,
    size: f32,
    first: (f64, f64),
    last: (f64, f64),
    out: &mut Out,
) -> Option<()> {
    let Some((x0, x1, y0, y1)) = scaled(ink, size) else {
        return Some(());
    };
    let w = widest * f64::from(size);
    out.within(Rect::new(
        first.0.min(last.0) + x0 + w.min(0.0),
        first.1.min(last.1) + y0,
        first.0.max(last.0) + x1 + w.max(0.0),
        first.1.max(last.1) + y1,
    ))
}

/// Of `count` glyphs of `run` at `size` centred in cells `cell` wide from 0, each
/// within `slack` of its cell's middle across, those that can reach `clip` across.
pub(super) fn cells(
    run: &Run,
    size: f32,
    cell: f64,
    count: usize,
    slack: f64,
    clip: Rect,
) -> Range<usize> {
    let Some((x0, x1, _, _)) = ink(run, size) else {
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

/// The lines of `run` at `size`, as [`layout`] breaks the text in one style to fit
/// `width`, as ranges of its glyphs.
pub(super) fn wrap(run: &Run, size: f32, width: f32) -> Lines {
    let chars = &run.chars;
    let mut out = Vec::new();
    let mut start = 0;
    let mut k = 0;
    while k <= chars.len() {
        // A paragraph ends at a line feed, a carriage return, or a CR LF pair.
        let end = k == chars.len() || matches!(chars[k], '\n' | '\r');
        if !end {
            k += 1;
            continue;
        }
        let ranges = breaks(
            k - start,
            |i| chars[start + i] == ' ',
            |i| (run.width(start + i..start + i + 1) * f64::from(size)) as f32,
            width,
        );
        out.extend(ranges.into_iter().map(|r| start + r.start..start + r.end));
        let pair = k + 1 < chars.len() && chars[k] == '\r' && chars[k + 1] == '\n';
        k += if pair { 2 } else { 1 };
        start = k;
    }
    let widest = out.iter().map(|r| run.width(r.clone())).fold(0.0, f64::max);
    Lines {
        ranges: out,
        widest,
    }
}

/// Where laid out text is drawn ([`Laid::draw`]): its lines from `pad` across, the
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

/// Text laid out in lines as [`layout`] lays it out to fit a width, kept without
/// its fonts, so that a document lays out a text many annotations share once, and
/// each draws only the part its clip shows ([`Laid::draw`]): the work of drawing
/// grows with what is shown, not with the text.
pub(super) struct Laid {
    styles: Vec<Style>,
    /// The width the lines are aligned in, and their first baseline below their top,
    /// in sizes.
    avail: f32,
    ascent: f32,
    /// Each glyph's character and span, line after line.
    chars: Vec<(char, usize)>,
    /// Each glyph's origin from its line's start without stretching, and the spaces
    /// before it on its line.
    xs: Vec<f64>,
    spaces: Vec<usize>,
    lines: Vec<LaidLine>,
    /// Over the lines, in order: the least bottom of a line's glyphs so far and from
    /// each line on, and the greatest top, from the top of the text; and the height
    /// of the lines so far, at its greatest.
    lowest: Vec<f64>,
    lowest_after: Vec<f64>,
    highest: Vec<f64>,
    highest_after: Vec<f64>,
    filled: Vec<f32>,
    /// The box the points of every glyph lie in, from `(pad, top)`; `None` without
    /// glyphs that have points.
    extent: Option<Rect>,
    /// Whether any line has a glyph.
    any: bool,
}

struct LaidLine {
    /// Its glyphs, in [`Laid::chars`].
    glyphs: Range<usize>,
    align: Align,
    width: f32,
    spaces: usize,
    /// Its baseline below the top of the text.
    below: f32,
    /// The box around a glyph's origin the points of each of its glyphs lie in,
    /// `(x0, x1, y0, y1)`.
    ink: Option<(f64, f64, f64, f64)>,
    /// Whether no advance on it is negative, or none positive.
    forward: bool,
    backward: bool,
}

impl LaidLine {
    /// Where it starts across from `pad`, and how much wider each space is, as
    /// lines are aligned in `avail`.
    fn start(&self, avail: f32) -> (f32, f32) {
        let width = self.width;
        match self.align {
            Align::Left => (0.0, 0.0),
            Align::Center => ((avail - width) / 2.0, 0.0),
            Align::Right => (avail - width, 0.0),
            Align::Justify => match self.spaces {
                0 => (0.0, 0.0),
                n => (0.0, ((avail - width) / n as f32).max(0.0)),
            },
        }
    }

    /// How low its glyphs reach, from the top of the text; above everything
    /// without glyphs that have points.
    fn lo(&self) -> f64 {
        self.ink
            .map_or(f64::INFINITY, |(_, _, y0, _)| y0 - f64::from(self.below))
    }

    /// How high its glyphs reach, from the top of the text.
    fn hi(&self) -> f64 {
        self.ink.map_or(f64::NEG_INFINITY, |(_, _, _, y1)| {
            y1 - f64::from(self.below)
        })
    }
}

/// The number of indices from 0 below `n` for which `pred`, true for those before
/// some index and false from it on, holds.
fn partition(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

impl Laid {
    /// `spans` laid out as [`layout`] lays them out in `avail`, each line's first
    /// baseline `ascent` times its size below its top.
    pub(super) fn new(
        fonts: &Fonts,
        spans: &[Span],
        avail: f32,
        ascent: f32,
    ) -> Result<Self, Unsupported> {
        let mut laid = Self {
            styles: spans.iter().map(|s| s.style.clone()).collect(),
            avail,
            ascent,
            chars: Vec::new(),
            xs: Vec::new(),
            spaces: Vec::new(),
            lines: Vec::new(),
            lowest: Vec::new(),
            lowest_after: Vec::new(),
            highest: Vec::new(),
            highest_after: Vec::new(),
            filled: Vec::new(),
            extent: None,
            any: false,
        };
        let (mut height, mut filled) = (0.0_f32, f32::NEG_INFINITY);
        let (mut lowest, mut highest) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut tops = Vec::new();
        for line in layout(fonts, spans, Some(avail))? {
            let start = laid.chars.len();
            let (mut x, mut spaces, mut ink) = (0.0_f64, 0, None::<(f64, f64, f64, f64)>);
            let (mut forward, mut backward) = (true, true);
            for set in &line.glyphs {
                laid.chars.push((set.ch, set.span));
                laid.xs.push(x);
                laid.spaces.push(spaces);
                let advance = f64::from(set.glyph.advance) * f64::from(set.size);
                forward &= advance >= 0.0;
                backward &= advance <= 0.0;
                x += advance;
                spaces += usize::from(set.glyph.is_space());
                let outline = set.glyph.face.outline(set.glyph.id);
                if !outline.path.elements().is_empty() {
                    let s = f64::from(set.size) / 1000.0;
                    let r = outline.ink;
                    let (x0, x1) = ((r.x0 * s).min(r.x1 * s), (r.x0 * s).max(r.x1 * s));
                    let (y0, y1) = ((r.y0 * s).min(r.y1 * s), (r.y0 * s).max(r.y1 * s));
                    ink = Some(ink.map_or((x0, x1, y0, y1), |(a, b, c, d)| {
                        (a.min(x0), b.max(x1), c.min(y0), d.max(y1))
                    }));
                }
            }
            let below = height + laid.ascent * line.size;
            height += 1.2 * line.size;
            filled = filled.max(height);
            let laid_line = LaidLine {
                glyphs: start..laid.chars.len(),
                align: line.align,
                width: line.width(),
                spaces,
                below,
                ink,
                forward,
                backward,
            };
            // Its glyphs' points, from `(pad, top)`.
            let (x0, spacing) = laid_line.start(avail);
            let stretch = f64::from(spacing) * spaces as f64;
            let xs = &laid.xs[laid_line.glyphs.clone()];
            let (lo, hi) = (laid_line.lo(), laid_line.hi());
            if let (Some((ix0, ix1, _, _)), Some(&first)) = (ink, xs.first()) {
                let (min, max) = xs
                    .iter()
                    .fold((first, first), |(a, b), &v| (a.min(v), b.max(v)));
                let x0 = f64::from(x0);
                let r = Rect::new(x0 + min + ix0, lo, x0 + max + stretch + ix1, hi);
                laid.extent = Some(laid.extent.map_or(r, |e| e.union(r)));
            }
            lowest = lowest.min(lo);
            highest = highest.max(hi);
            laid.lowest.push(lowest);
            laid.highest.push(highest);
            tops.push((lo, hi));
            laid.filled.push(filled);
            laid.lines.push(laid_line);
        }
        laid.any = !laid.chars.is_empty();
        let (mut lowest, mut highest) = (f64::INFINITY, f64::NEG_INFINITY);
        laid.lowest_after = vec![0.0; tops.len()];
        laid.highest_after = vec![0.0; tops.len()];
        for (j, (lo, hi)) in tops.into_iter().enumerate().rev() {
            lowest = lowest.min(lo);
            highest = highest.max(hi);
            laid.lowest_after[j] = lowest;
            laid.highest_after[j] = highest;
        }
        Ok(laid)
    }

    /// Draws the lines in `area`, those glyphs alone that can reach both its clip
    /// and `seen`, in the content's space, but fails, as [`fill`] fails for a glyph
    /// it does not write, when a point of any glyph cannot be written.
    pub(super) fn draw(
        &self,
        area: &TextBox,
        seen: Rect,
        fonts: &Fonts,
        out: &mut Out,
    ) -> Result<(), Unsupported> {
        let fail = |o: Option<()>| o.ok_or(Unsupported::Kind);
        if !self.any {
            return Ok(());
        }
        fail(out.clip(area.clip))?;
        let (pad, top) = (f64::from(area.pad), f64::from(area.top));
        if let Some(e) = self.extent {
            let all = e + kurbo::Vec2::new(pad, top);
            fail(out.within(area.frame.transform_rect_bbox(all)))?;
        }
        // What can show, in the box: what the clip and `seen` share.
        let view = area.clip.intersect(seen);
        if !(view.width() >= 0.0 && view.height() >= 0.0) {
            return Ok(());
        }
        let view = area.frame.inverse().transform_rect_bbox(view);
        let (qa, qb) = (view.y0 - top, view.y1 - top);
        // Lines beyond those that fit in the room are not drawn; of the others, a
        // line before the first whose glyphs can reach as low as the view's top, or
        // as high as its bottom, cannot reach the view, nor one after the last.
        let fit = self.filled.partition_point(|&f| f <= area.room);
        let first = self
            .lowest
            .partition_point(|&v| v > qb)
            .max(self.highest.partition_point(|&v| v < qa));
        let last = self
            .highest_after
            .partition_point(|&v| v >= qa)
            .min(self.lowest_after.partition_point(|&v| v <= qb))
            .min(fit);
        let mut color = None;
        for line in self.lines.get(first..last).unwrap_or_default() {
            if !(line.hi() >= qa && line.lo() <= qb) {
                continue;
            }
            let Some((ix0, ix1, _, _)) = line.ink else {
                continue;
            };
            let xs = &self.xs[line.glyphs.clone()];
            let spaces = &self.spaces[line.glyphs.clone()];
            let (x0, spacing) = line.start(self.avail);
            let x0 = pad + f64::from(x0);
            let s = f64::from(spacing);
            let at = |i: usize| x0 + xs[i] + s * spaces[i] as f64;
            // The glyphs that can reach the view across, when their origins run one
            // way; else all of them.
            let (vx0, vx1) = (view.x0, view.x1);
            let range = if line.forward {
                partition(xs.len(), |i| at(i) + ix1 < vx0)..partition(xs.len(), |i| {
                    at(i) + ix0 <= vx1
                })
            } else if line.backward && spacing == 0.0 {
                partition(xs.len(), |i| at(i) + ix0 > vx1)..partition(xs.len(), |i| {
                    at(i) + ix1 >= vx0
                })
            } else {
                0..xs.len()
            };
            let y = f64::from(area.top - line.below);
            let chars = &self.chars[line.glyphs.clone()];
            let mut i = range.start;
            while i < range.end {
                let span = chars[i].1;
                let mut j = i + 1;
                while j < range.end && chars[j].1 == span {
                    j += 1;
                }
                let style = &self.styles[span];
                if color != Some(&style.color) {
                    fail(out.fill_color(&style.color))?;
                    color = Some(&style.color);
                }
                let glyphs = chars[i..j]
                    .iter()
                    .map(|&(c, _)| fonts.glyph(style.face, c))
                    .collect::<Result<Vec<_>, _>>()?;
                let origin = (at(i), y);
                fail(fill(
                    &glyphs,
                    style.size,
                    origin,
                    spacing,
                    area.frame,
                    Some(area.clip),
                    out,
                ))?;
                i = j;
            }
        }
        Ok(())
    }
}

/// A standard font's glyphs, each outline read once.
struct Face {
    font: Font,
    outlines: RefCell<FxHashMap<GlyphId, Rc<Outline>>>,
}

/// A glyph's outline, 1000 units to the em, and the box of its points.
struct Outline {
    path: BezPath,
    ink: Rect,
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
    fn new(data: crate::font::FontData, index: u32, standard: StandardFont) -> Option<Self> {
        Some(Self {
            font: Font::new(data, index, standard)?,
            outlines: RefCell::default(),
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
                Rc::new(Outline { path, ink })
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
                Object::Number(n) if numbers.len() < 4 => numbers.push(real(n)),
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

/// Text in one style.
#[derive(Debug)]
pub(super) struct Span {
    pub(super) text: String,
    pub(super) style: Style,
}

/// A glyph, and the character it was set for, at the size of the style of span
/// `span` it was set from.
#[derive(Clone)]
pub(super) struct Set<'f> {
    pub(super) glyph: Glyph<'f>,
    pub(super) ch: char,
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
                    ch: c,
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
            // The largest size, or, of negative ones, the one furthest from 0.
            let size = line
                .iter()
                .map(|g| g.size)
                .fold(0.0, |a: f32, b| if b.abs() > a.abs() { b } else { a });
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
) -> Vec<Range<usize>> {
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
    use super::{Align, Color, Family, Fonts, Span, Style, breaks, layout};
    use std::cell::Cell;

    /// A value set as a run breaks into the lines its text in one style lays out
    /// into, glyph for glyph: paragraphs at LF, CR and CR LF, words wrapped, a word
    /// wider than the line alone on its line.
    #[test]
    fn a_run_wraps_as_its_text_lays_out() {
        let fonts = Fonts::new(crate::InterpreterSettings::default().font_resolver);
        let texts = [
            "",
            "one",
            "a b  c d",
            "word\r\nnext\rthird\n\nlast",
            "averyveryverylongword short words after it",
            " lead",
            "trail  ",
            "\r\n",
            "x\r",
            "\n\r",
        ];
        for text in texts {
            for width in [0.0, 10.0, 40.0, 1000.0] {
                let run = fonts.run(Family::Sans, text).expect("drawable");
                let got: Vec<(usize, f32)> = super::wrap(&run, 12.0, width)
                    .ranges
                    .iter()
                    .map(|r| (r.len(), (run.width(r.clone()) * 12.0) as f32))
                    .collect();
                let span = Span {
                    text: text.to_owned(),
                    style: Style {
                        face: Family::Sans.into(),
                        size: 12.0,
                        color: Color::BLACK,
                        align: Align::Left,
                    },
                };
                let expected: Vec<(usize, f32)> = layout(&fonts, &[span], Some(width))
                    .expect("drawable")
                    .iter()
                    .map(|l| (l.glyphs.len(), l.width()))
                    .collect();
                assert_eq!(got.len(), expected.len(), "{text:?} at {width}");
                for (g, e) in got.iter().zip(&expected) {
                    assert!(
                        g.0 == e.0 && (g.1 - e.1).abs() < 1e-3,
                        "{text:?} at {width}: {got:?} {expected:?}"
                    );
                }
            }
        }
    }

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
