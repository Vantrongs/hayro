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
mod text;
mod widget;

use text::Fonts;

use crate::BlendMode;
use hayro_syntax::object::dict::keys::{
    AP, AS, BBOX, BORDER, BS, C, CA, D, F, MATRIX, N, RECT, S, SUBTYPE, W,
};
use hayro_syntax::object::{Array, Dict, Name, Object, Rect, Stream};
use hayro_syntax::xref::XRef;
use kurbo::{Affine, Shape};
use std::fmt::Write;

/// What an annotation draws.
pub enum Look<'a> {
    /// Its normal appearance stream, which has a `/BBox` to place it with
    /// [`rect_transform`].
    Stream(Stream<'a>),
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
    /// state `/AS` names. A stream there draws when it has a `/BBox` of some area, and
    /// nothing otherwise. An `/AP` that is not a dictionary draws nothing. Without an
    /// appearance there (no `/AP`, no `/N`, no stream for the state), one is generated
    /// from the annotation's entries.
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
                    return if has_area(&stream) {
                        Look::Stream(stream)
                    } else {
                        Look::Nothing
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

/// Whether `stream` has a `/BBox` that, after its `/Matrix`, encloses some area.
fn has_area(stream: &Stream<'_>) -> bool {
    let Some(bbox) = stream.dict().get::<[f32; 4]>(BBOX) else {
        return false;
    };
    let matrix = stream
        .dict()
        .get::<[f64; 6]>(MATRIX)
        .unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
    let transform = rect_transform(
        kurbo::Rect::new(0.0, 0.0, 1.0, 1.0),
        bbox,
        Affine::new(matrix),
    );
    transform.as_coeffs().iter().all(|v| v.is_finite())
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
        let mut out = Out(String::new());
        let fonts = &self.fonts;
        let dr = self
            .form
            .as_ref()
            .and_then(|f| f.get::<Dict<'_>>(b"DR"))
            .and_then(|dr| dr.get::<Dict<'_>>(b"Font"));
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
            b"FreeText" => freetext::free_text(annot, dr.as_ref(), fonts, &mut out),
            b"Line" => lines::line(annot, fonts, &mut out),
            b"Polygon" => ok(lines::poly(annot, true, &mut out)),
            b"PolyLine" => ok(lines::poly(annot, false, &mut out)),
            b"Widget" => widget::widget(annot, self.form.as_ref(), fonts, &mut out),
            // Popup, Link, the media and 3D subtypes, PrinterMark, TrapNet, Watermark,
            // Projection, stamps of other names and unknown subtypes: nothing.
            _ => Ok(None),
        }?;
        // An appearance that paints nothing, such as a widget with no value, border
        // or background, is no appearance.
        let paints = out.0.lines().any(|l| {
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
            content: out.0,
            opacity: opacity(annot),
            blend,
        }))
    }
}

/// The field dictionary a widget takes `key` from: its own, or the nearest ancestor
/// through `/Parent` that has it (PDF 32000-1 12.7.3.1).
fn field<'a>(annot: &Dict<'a>, key: &[u8]) -> Option<Dict<'a>> {
    let mut node = annot.clone();
    // A parent chain deeper than this is a loop.
    for _ in 0..64 {
        if node.contains_key(key) {
            return Some(node);
        }
        node = node.get::<Dict<'_>>(b"Parent")?;
    }
    None
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

/// Content stream text; every number written is finite.
struct Out(String);

impl Out {
    fn nums(&mut self, nums: &[f32], op: &str) -> Option<()> {
        if !nums.iter().all(|n| n.is_finite()) {
            return None;
        }
        for n in nums {
            write!(self.0, "{n} ").ok()?;
        }
        self.0.push_str(op);
        self.0.push('\n');
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
                if !(p.x.is_finite() && p.y.is_finite()) {
                    return None;
                }
                write!(self.0, "{} {} ", round(p.x), round(p.y)).ok()?;
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
        self.0.push_str(op);
        self.0.push('\n');
    }

    /// The stroke state every generated appearance starts from.
    fn stroke_state(&mut self, width: f32, dash: &[f32]) -> Option<()> {
        self.op("0 J 0 j 10 M");
        self.nums(&[width], "w")?;
        self.0.push('[');
        for d in dash {
            write!(self.0, "{d} ").ok()?;
        }
        self.op("] 0 d");
        Some(())
    }
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

/// `/Rect`, normalized, when all its numbers are finite.
fn rect(annot: &Dict<'_>) -> Option<kurbo::Rect> {
    use crate::util::RectExt;
    let r = annot.get::<Rect>(RECT)?.to_kurbo().abs();
    [r.x0, r.y0, r.x1, r.y1]
        .iter()
        .all(|n| n.is_finite())
        .then_some(r)
}
