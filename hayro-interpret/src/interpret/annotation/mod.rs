//! How annotations look: the appearance stream an annotation names and where it lands
//! on the page, or, for an annotation without a usable one, an appearance generated
//! from its entries (PDF 32000-1 12.5).
//!
//! The renderer draws annotations with [`look`], and a writer that flattens
//! annotations into page content can use the same to draw what the renderer shows.
//!
//! The generated appearances follow the PDF specification and `PDFium`
//! (`core/fpdfdoc/cpdf_generateap.cpp`, BSD-3-Clause); each generator names its
//! source. Their licenses are in the repository's `NOTICE.md`.
//!
//! A generated appearance is a content stream in default user space that needs no
//! resources. /CA and the highlight's Multiply
//! blend apply to it as one transparency group.

mod markup;

use crate::BlendMode;
use hayro_syntax::object::dict::keys::{
    AP, AS, BBOX, BORDER, BS, C, CA, D, F, MATRIX, N, RECT, S, SUBTYPE, W,
};
use hayro_syntax::object::{Array, Dict, Name, Object, Rect, Stream};
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

/// What `annot` draws on a page that shows `visible` (in default user space) of it.
///
/// The normal appearance is `/AP /N`, or, when that is a dictionary of states, the
/// state `/AS` names. A stream there draws when it has a `/BBox` of some area, and
/// nothing otherwise. An `/AP` that is not a dictionary draws nothing. Without an
/// appearance there (no `/AP`, no `/N`, no stream for the state), one is generated
/// from the annotation's entries.
pub fn look<'a>(annot: &Dict<'a>, visible: kurbo::Rect) -> Look<'a> {
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
    match generate(annot, visible) {
        Ok(Some(appearance)) => Look::Generated(appearance),
        Ok(None) => Look::Nothing,
        Err(why) => Look::Unsupported(why),
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

/// The appearance generated for `annot` from its entries.
fn generate(annot: &Dict<'_>, visible: kurbo::Rect) -> Generated {
    let Some(subtype) = annot.get::<Name<'_>>(SUBTYPE) else {
        return Ok(None);
    };
    let mut out = Out(String::new());
    let drew = match subtype.as_ref() {
        b"Square" => markup::shape(annot, false, &mut out),
        b"Circle" => markup::shape(annot, true, &mut out),
        b"Highlight" => markup::highlight(annot, &mut out),
        b"Underline" => markup::text_lines(annot, markup::Line::Under, visible, &mut out),
        b"StrikeOut" => markup::text_lines(annot, markup::Line::Through, visible, &mut out),
        b"Squiggly" => markup::text_lines(annot, markup::Line::Squiggle, visible, &mut out),
        b"Ink" => markup::ink(annot, &mut out),
        b"Text" => markup::note_icon(annot, &mut out),
        b"FreeText" | b"Line" | b"Polygon" | b"PolyLine" | b"Caret" | b"FileAttachment"
        | b"Sound" | b"Redact" => return Err(Unsupported::Kind),
        b"Stamp"
            if annot
                .get::<Name<'_>>(b"Name")
                .is_some_and(|n| STAMPS.contains(&n.as_ref())) =>
        {
            return Err(Unsupported::Kind);
        }
        b"Widget" => return widget(annot),
        // Popup, Link, the media and 3D subtypes, PrinterMark, TrapNet, Watermark,
        // Projection, stamps of other names and unknown subtypes: nothing.
        _ => return Ok(None),
    };
    if drew.is_none() {
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

/// The stamp names of PDF 32000-1 Table 181, which a stamp without an appearance
/// stream is drawn for.
const STAMPS: [&[u8]; 14] = [
    b"Approved",
    b"Experimental",
    b"NotApproved",
    b"AsIs",
    b"Expired",
    b"NotForPublicRelease",
    b"Confidential",
    b"Final",
    b"Sold",
    b"Departmental",
    b"ForComment",
    b"TopSecret",
    b"Draft",
    b"ForPublicRelease",
];

/// A form field's widget: one shows its border and background (`/MK /BC`, `/BG`), and
/// the value of its field: the text of a text field or a combo box, the options of
/// a list box, the caption of a push button (`/MK /CA`), the mark of a check box or
/// radio button in an on state (`/AS`), the look of an unsigned signature field. A
/// field without a type, a signed signature and a widget with none of these draw
/// nothing.
fn widget(annot: &Dict<'_>) -> Generated {
    let field_type = field(annot, b"FT").and_then(|d| d.get::<Name<'_>>(b"FT"));
    let flags = field(annot, b"Ff")
        .and_then(|d| d.get::<u32>(b"Ff"))
        .unwrap_or(0);
    let mk = annot.get::<Dict<'_>>(b"MK");
    let mk_color = |key: &[u8]| {
        mk.as_ref()
            .and_then(|mk| mk.get::<Array<'_>>(key))
            .is_some_and(|a| color(&a, false).is_some())
    };
    let border = mk_color(b"BC") && border_width(annot) > 0.0;
    let text = |key: &[u8]| {
        field(annot, key)
            .and_then(|d| d.get::<hayro_syntax::object::String<'_>>(key))
            .is_some_and(|s| !s.as_bytes().is_empty())
    };
    let shows = match field_type.as_ref().map(|n| n.as_ref()) {
        Some(b"Sig") => {
            return if field(annot, b"V").is_none() {
                Err(Unsupported::Kind)
            } else {
                Ok(None)
            };
        }
        Some(b"Tx") => text(b"V"),
        // Bit 18: a combo box.
        Some(b"Ch") if flags & (1 << 17) != 0 => text(b"V"),
        Some(b"Ch") => field(annot, b"Opt")
            .and_then(|d| d.get::<Array<'_>>(b"Opt"))
            .is_some_and(|o| o.iter::<Object<'_>>().next().is_some()),
        // Bit 17: a push button.
        Some(b"Btn") if flags & (1 << 16) != 0 => mk
            .as_ref()
            .and_then(|mk| mk.get::<hayro_syntax::object::String<'_>>(b"CA"))
            .is_some_and(|s| !s.as_bytes().is_empty()),
        Some(b"Btn") => annot
            .get::<Name<'_>>(AS)
            .is_some_and(|state| state.as_ref() != b"Off"),
        _ => return Ok(None),
    };
    if shows || border || mk_color(b"BG") {
        Err(Unsupported::Kind)
    } else {
        Ok(None)
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
