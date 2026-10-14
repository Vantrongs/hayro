//! How annotations look: the appearance stream an annotation names and where it lands
//! on the page, or, for an annotation without a usable one, an appearance generated
//! from its entries (PDF 32000-1 12.5).
//!
//! The renderer draws annotations with [`Appearances::look`], and a writer that flattens
//! annotations into page content can use the same to draw what the renderer shows.
//!
//! What a generated appearance shows follows the PDF specification. The markup
//! shapes (`markup.rs`) are adapted from `PDFium` (`core/fpdfdoc/cpdf_generateap.cpp`,
//! BSD-3-Clause; its license is in the repository's `NOTICE.md`). The layout of the
//! other generators, which the spec leaves to the viewer, is written here and sized
//! to match the rendered output of common viewers; each module says what it draws.
//! Text is drawn as glyph outlines from the standard fonts the font resolver gives.
//!
//! A generated appearance is a content stream in default user space that needs no
//! resources. /CA and the highlight's Multiply
//! blend apply to it as one transparency group.

mod freetext;
mod icons;
mod lines;
mod markup;
mod rich;
mod text;
mod widget;

use text::Fonts;

use crate::BlendMode;
use hayro_syntax::object::dict::keys::{
    AP, AS, BBOX, BORDER, BS, C, CA, D, F, MATRIX, N, RECT, S, SUBTYPE, W,
};
use hayro_syntax::object::{Array, Dict, Name, ObjRef, Object, Stream};
use hayro_syntax::xref::XRef;
use kurbo::{Affine, Shape};
use std::fmt::Write;

/// What an annotation draws.
pub enum Look<'a> {
    /// Its normal appearance stream, drawn after its own `/Matrix` with `transform`,
    /// the [`rect_transform`] that places its `/BBox` on the annotation's `/Rect`.
    Stream {
        /// The appearance stream.
        stream: Stream<'a>,
        /// From the appearance's space, after its `/Matrix`, to default user space.
        transform: Affine,
    },
    /// An appearance generated from its entries.
    Generated(Appearance),
    /// Nothing.
    Nothing,
    /// Something this crate cannot draw: the annotation has no usable appearance
    /// stream, and one cannot be generated.
    Unsupported(Unsupported),
}

/// Why no appearance can be generated for an annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// Its subtype, or its field type for a widget, has no generator.
    Kind,
    /// Its text has a character none of the fonts has.
    Character(char),
    /// A coordinate it draws at is too large to write: beyond the range of the
    /// 32-bit numbers content streams are read in.
    Coordinates,
    /// Its rich text (`/RC`) or default style (`/DS`) uses an element, attribute or
    /// style declaration the generator cannot draw as viewers do, or nests or
    /// repeats elements beyond what written rich text does.
    RichText,
}

/// An appearance generated for an annotation without a usable appearance stream.
pub struct Appearance {
    /// The content stream, in default user space; it uses no resources.
    pub content: String,
    /// The annotation's `/CA`, clamped to 0–1.
    pub opacity: f32,
    /// Multiply for highlights, Normal otherwise.
    pub blend: BlendMode,
}

/// Whether `annot` is kept when annotations are flattened into page content for
/// printing what the reader sees: every annotation but links and those flagged
/// Invisible or Hidden (PDF 32000-1 12.5.3), whatever their Print and `NoView` flags.
pub fn flattened(annot: &Dict<'_>) -> bool {
    let link = annot
        .get::<Name<'_>>(SUBTYPE)
        .is_some_and(|s| s.as_ref() == b"Link");
    !link && annot.get::<u32>(F).unwrap_or(0) & 3 == 0
}

/// What the annotations of one document look like: [`Appearances::look`], with
/// generated text in the standard fonts a font resolver gives (Helvetica for
/// characters a family's font lacks) and widget defaults from the document's
/// interactive form. Make one per page or document: fonts are loaded once.
pub struct Appearances<'a> {
    fonts: Fonts,
    /// The interactive form dictionary (`/AcroForm`).
    form: Option<Dict<'a>>,
}

impl<'a> Appearances<'a> {
    /// The appearances of annotations in the document of `xref`, with text in the
    /// fonts `font_resolver` gives for the standard fonts.
    pub fn new(xref: &'a XRef, font_resolver: crate::FontResolverFn) -> Self {
        let form = xref
            .get::<Dict<'a>>(xref.root_id())
            .and_then(|root| root.get::<Dict<'a>>(b"AcroForm"));
        Self {
            fonts: Fonts::new(font_resolver),
            form,
        }
    }

    /// What `annot` draws on a page that shows `visible` (in default user space) of it,
    /// with text in `fonts`.
    ///
    /// The normal appearance is `/AP /N`, or, when that is a dictionary of states, the
    /// state `/AS` names. A stream there draws when it has a `/BBox` of some area and
    /// the annotation a `/Rect`, and nothing otherwise. An `/AP` that is not a
    /// dictionary draws nothing. Without an appearance there (no `/AP`, no `/N`, no
    /// stream for the state), one is generated from the annotation's entries.
    pub fn look(&self, annot: &Dict<'a>, visible: kurbo::Rect) -> Look<'a> {
        match annot.get::<Object<'_>>(AP) {
            None | Some(Object::Null(_)) => {}
            Some(Object::Dict(ap)) => {
                // Read as `Object` first: a dictionary read as a stream may pick up the
                // data of a stream after it in the file.
                let stream = match ap.get::<Object<'_>>(N) {
                    Some(Object::Stream(stream)) => Some(stream),
                    Some(Object::Dict(states)) => annot
                        .get::<Name<'_>>(AS)
                        .and_then(|state| states.get::<Stream<'_>>(state)),
                    _ => None,
                };
                if let Some(stream) = stream {
                    return match placement(&stream, annot) {
                        Some(transform) => Look::Stream { stream, transform },
                        None => Look::Nothing,
                    };
                }
            }
            Some(_) => return Look::Nothing,
        }
        if annot.contains_key(AP) && annot.get::<Object<'_>>(AP).is_none() {
            // A reference to no object.
            return Look::Nothing;
        }
        match self.generate(annot, visible) {
            Ok(Some(appearance)) => Look::Generated(appearance),
            Ok(None) => Look::Nothing,
            Err(why) => Look::Unsupported(why),
        }
    }
}

/// The transform that places appearance `stream` on the `/Rect` of `annot`, when
/// the stream has a `/BBox` that, after its `/Matrix`, encloses some area, and the
/// annotation has a `/Rect`.
fn placement(stream: &Stream<'_>, annot: &Dict<'_>) -> Option<Affine> {
    let bbox = stream.dict().get::<[f32; 4]>(BBOX)?;
    let matrix = stream
        .dict()
        .get::<[f64; 6]>(MATRIX)
        .unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
    let matrix = Affine::new(matrix);
    // A box of no area maps onto no rect; a /Rect of no area maps the box onto
    // nothing, which is drawn as such.
    let unit = rect_transform(kurbo::Rect::new(0.0, 0.0, 1.0, 1.0), bbox, matrix);
    let transform = rect_transform(rect(annot)?, bbox, matrix);
    [unit, transform]
        .iter()
        .all(|t| t.as_coeffs().iter().all(|v| v.is_finite()))
        .then_some(transform)
}

/// The matrix A of PDF 32000-1 12.5.5 for an appearance with `bbox` and `matrix` on
/// an annotation with `rect`: it maps the appearance box, transformed by `matrix`,
/// onto `rect`. The appearance is drawn with `matrix` and then A.
pub fn rect_transform(rect: kurbo::Rect, bbox: [f32; 4], matrix: Affine) -> Affine {
    let [x0, y0, x1, y1] = bbox.map(f64::from);
    let transformed = (matrix * kurbo::Rect::new(x0, y0, x1, y1).to_path(0.1)).bounding_box();
    let (sx, sy) = (
        rect.width() / transformed.width(),
        rect.height() / transformed.height(),
    );
    Affine::new([
        sx,
        0.0,
        0.0,
        sy,
        rect.x0 - transformed.x0 * sx,
        rect.y0 - transformed.y0 * sy,
    ])
}

/// What a generator makes: an appearance, nothing to draw, or why it cannot.
type Generated = Result<Option<Appearance>, Unsupported>;

impl Appearances<'_> {
    /// The appearance generated for `annot` from its entries.
    fn generate(&self, annot: &Dict<'_>, visible: kurbo::Rect) -> Generated {
        let Some(subtype) = annot.get::<Name<'_>>(SUBTYPE) else {
            return Ok(None);
        };
        let mut out = Out::default();
        let fonts = &self.fonts;
        let ok = |drew: Option<()>| Ok(drew);
        let drew = match subtype.as_ref() {
            b"Square" => ok(markup::shape(annot, false, &mut out)),
            b"Circle" => ok(markup::shape(annot, true, &mut out)),
            b"Highlight" => ok(markup::highlight(annot, &mut out)),
            b"Underline" => ok(markup::text_lines(
                annot,
                markup::Line::Under,
                visible,
                &mut out,
            )),
            b"StrikeOut" => ok(markup::text_lines(
                annot,
                markup::Line::Through,
                visible,
                &mut out,
            )),
            b"Squiggly" => ok(markup::text_lines(
                annot,
                markup::Line::Squiggle,
                visible,
                &mut out,
            )),
            b"Ink" => ok(markup::ink(annot, &mut out)),
            b"Text" => icons::icon(annot, icons::Icon::Note, fonts, &mut out),
            b"FileAttachment" => icons::icon(annot, icons::Icon::FileAttachment, fonts, &mut out),
            b"Sound" => icons::icon(annot, icons::Icon::Sound, fonts, &mut out),
            b"Caret" => ok(icons::caret(annot, &mut out)),
            b"Redact" => ok(icons::redact(annot, &mut out)),
            b"Stamp" => icons::stamp(annot, fonts, &mut out),
            b"FreeText" => freetext::free_text(annot, fonts, &mut out),
            b"Line" => lines::line(annot, fonts, &mut out),
            b"Polygon" => ok(lines::poly(annot, true, &mut out)),
            b"PolyLine" => ok(lines::poly(annot, false, &mut out)),
            b"Widget" => widget::widget(annot, self.form.as_ref(), fonts, visible, &mut out),
            // Popup, Link, the media and 3D subtypes, PrinterMark, TrapNet, Watermark,
            // Projection, stamps of other names and unknown subtypes: nothing.
            _ => Ok(None),
        };
        // A number that could not be written fails the appearance, whatever the
        // generator made of it: `None` from a generator means nothing to draw.
        if out.failed {
            return Err(Unsupported::Coordinates);
        }
        let drew = drew?;
        // An appearance that paints nothing, such as a widget with no value, border
        // or background, is no appearance.
        let paints = out.content.lines().any(|l| {
            matches!(
                l.rsplit(' ').next(),
                Some("f" | "f*" | "S" | "s" | "b" | "b*" | "B" | "B*")
            )
        });
        if drew.is_none() || !paints {
            return Ok(None);
        }
        let blend = if subtype.as_ref() == b"Highlight" {
            BlendMode::Multiply
        } else {
            BlendMode::Normal
        };
        Ok(Some(Appearance {
            content: out.content,
            opacity: opacity(annot),
            blend,
        }))
    }
}

/// A widget and the fields above it through `/Parent`, from which it inherits field
/// entries (PDF 32000-1 12.7.3.1).
struct Field<'a>(Vec<Dict<'a>>);

impl<'a> Field<'a> {
    /// The chain of `annot`, however deep; `None` when it loops, which viewers read as
    /// a broken field and draw nothing for.
    fn of(annot: &Dict<'a>) -> Option<Self> {
        let mut seen = rustc_hash::FxHashSet::default();
        if let Some(id) = annot.obj_id() {
            seen.insert(ObjRef::from(id));
        }
        let mut chain = vec![annot.clone()];
        loop {
            let node = chain.last()?;
            // A direct parent dictionary cannot lead back to itself.
            if let Some(parent) = node.get_ref(b"Parent")
                && !seen.insert(parent)
            {
                return None;
            }
            let Some(parent) = node.get::<Dict<'a>>(b"Parent") else {
                return Some(Self(chain));
            };
            chain.push(parent);
        }
    }

    /// The nearest dictionary in the chain with `key`; an entry that is null or names
    /// no object is absent (PDF 32000-1 7.3.9).
    fn with(&self, key: &[u8]) -> Option<&Dict<'a>> {
        self.0.iter().find(|d| {
            d.get::<Object<'_>>(key)
                .is_some_and(|o| !matches!(o, Object::Null(_)))
        })
    }

    /// The inherited entry `key` as a `T`.
    fn get<T: TryFrom<Object<'a>>>(&self, key: &[u8]) -> Option<T> {
        T::try_from(self.with(key)?.get::<Object<'a>>(key)?).ok()
    }
}

/// /CA clamped to 0–1, 1 when absent or not a number.
fn opacity(annot: &Dict<'_>) -> f32 {
    let opacity = annot.get::<f32>(CA).unwrap_or(1.0);
    if opacity.is_finite() {
        opacity.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// The smallest distance generated geometry keeps apart: a hundredth of a point,
/// finer than any printer or screen resolves (7200 dpi). Steps closer than this, of
/// a squiggle or of comb cell dividers, are drawn as the band they cover, so the
/// work to draw them is bounded by the size of what is visible.
const FINEST: f64 = 0.01;

/// Content stream text; every number written is finite. Writing one that is not
/// fails the whole appearance (`failed`), whatever the generator does next.
#[derive(Default)]
struct Out {
    content: String,
    failed: bool,
}

impl Out {
    fn nums(&mut self, nums: &[f32], op: &str) -> Option<()> {
        if !nums.iter().all(|n| n.is_finite()) {
            self.failed = true;
            return None;
        }
        for n in nums {
            write!(self.content, "{n} ").ok()?;
        }
        self.content.push_str(op);
        self.content.push('\n');
        Some(())
    }

    /// `path` as path construction operators, to a thousandth of a point.
    fn path(&mut self, path: &kurbo::BezPath) -> Option<()> {
        use kurbo::PathEl;
        let mut last = kurbo::Point::ZERO;
        for el in path.elements() {
            let pts: &[kurbo::Point] = match el {
                PathEl::MoveTo(p) | PathEl::LineTo(p) => &[*p],
                PathEl::QuadTo(c, p) => {
                    // The cubic of the same curve.
                    let c1 = last + (*c - last) * (2.0 / 3.0);
                    let c2 = *p + (*c - *p) * (2.0 / 3.0);
                    &[c1, c2, *p]
                }
                PathEl::CurveTo(a, b, p) => &[*a, *b, *p],
                PathEl::ClosePath => {
                    self.op("h");
                    continue;
                }
            };
            for p in pts {
                // In the f32 content streams are read in.
                if !(p.x.is_finite()
                    && p.y.is_finite()
                    && (p.x as f32).is_finite()
                    && (p.y as f32).is_finite())
                {
                    self.failed = true;
                    return None;
                }
                write!(self.content, "{} {} ", round(p.x), round(p.y)).ok()?;
            }
            self.op(match el {
                PathEl::MoveTo(_) => "m",
                PathEl::LineTo(_) => "l",
                _ => "c",
            });
            last = *pts.last()?;
        }
        Some(())
    }

    fn op(&mut self, op: &str) {
        self.content.push_str(op);
        self.content.push('\n');
    }

    /// The stroke state every generated appearance starts from.
    fn stroke_state(&mut self, width: f32, dash: &[f32]) -> Option<()> {
        self.op("0 J 0 j 10 M");
        self.nums(&[width], "w")?;
        self.content.push('[');
        for d in dash {
            write!(self.content, "{d} ").ok()?;
        }
        self.op("] 0 d");
        Some(())
    }
}

/// The entries of a coordinate array, an entry that is not a number read as 0, as
/// viewers read a damaged array, so a later coordinate keeps its place.
fn numbers(arr: &Array<'_>) -> Vec<f64> {
    arr.iter::<Object<'_>>()
        .map(|o| match o {
            Object::Number(n) => n.as_f64(),
            _ => 0.0,
        })
        .collect()
}

/// The [`numbers`] of array `key`, padded with 0 to at least `len`; `None` when
/// `key` is not an array.
fn numbers_of(dict: &Dict<'_>, key: &[u8], len: usize) -> Option<Vec<f64>> {
    let mut n = numbers(&dict.get::<Array<'_>>(key)?);
    if n.len() < len {
        n.resize(len, 0.0);
    }
    Some(n)
}

/// `v` to a thousandth, without the trailing zeros.
fn round(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0 + 0.0
}

/// The colour operator for a colour array (gray, RGB or CMYK); `None` when the array
/// has no components (transparent) or an invalid count.
fn color(arr: &Array<'_>, stroke: bool) -> Option<String> {
    let c: Vec<f32> = arr.iter::<f32>().filter(|c| c.is_finite()).collect();
    let op = match (c.len(), stroke) {
        (1, false) => "g",
        (1, true) => "G",
        (3, false) => "rg",
        (3, true) => "RG",
        (4, false) => "k",
        (4, true) => "K",
        _ => return None,
    };
    let mut s = String::new();
    for n in &c {
        write!(s, "{n} ").ok()?;
    }
    s.push_str(op);
    Some(s)
}

/// /C as a colour operator: `default` when absent, `None` when it is transparent.
fn c_color(annot: &Dict<'_>, stroke: bool, default: &str) -> Option<String> {
    match annot.get::<Array<'_>>(C) {
        Some(arr) => color(&arr, stroke),
        None => Some(default.to_string()),
    }
}

/// /BS /W, else the third /Border element, else 1; 0 strokes the thinnest line
/// (PDF 32000-1 8.4.3.2), as does a width that is negative or not a number.
fn border_width(annot: &Dict<'_>) -> f32 {
    let width = annot
        .get::<Dict<'_>>(BS)
        .and_then(|bs| bs.get::<f32>(W))
        .or_else(|| {
            annot
                .get::<Array<'_>>(BORDER)
                .and_then(|b| b.iter::<f32>().nth(2))
        })
        .unwrap_or(1.0);
    if width.is_finite() {
        width.max(0.0)
    } else {
        0.0
    }
}

/// The dash array of a dashed border style (/BS /S /D), else the fourth /Border
/// element; at most ten entries, as in `PDFium`.
fn dash(annot: &Dict<'_>) -> Vec<f32> {
    let bs = annot.get::<Dict<'_>>(BS);
    let arr = match &bs {
        Some(bs) if bs.get::<Name<'_>>(S).is_some_and(|s| s.as_ref() == b"D") => {
            bs.get::<Array<'_>>(D)
        }
        _ => annot
            .get::<Array<'_>>(BORDER)
            .and_then(|b| match b.iter::<Object<'_>>().nth(3)? {
                Object::Array(a) => Some(a),
                _ => None,
            }),
    };
    let dash: Vec<f32> = arr
        .map(|a| a.iter::<f32>().take(10).collect())
        .unwrap_or_default();
    // An all-zero or negative pattern draws nothing; draw solid instead.
    if dash.iter().all(|d| d.is_finite() && *d >= 0.0) && dash.iter().any(|d| *d > 0.0) {
        dash
    } else {
        Vec::new()
    }
}

/// `/Rect`, normalized, read as viewers read a damaged one: a coordinate it lacks
/// or that is not a number is 0. `None` when it is not an array.
fn rect(annot: &Dict<'_>) -> Option<kurbo::Rect> {
    let n = numbers_of(annot, RECT, 4)?;
    Some(kurbo::Rect::new(n[0], n[1], n[2], n[3]).abs())
}
