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
//! Annotation dictionaries are read as viewers read damaged ones (`entries.rs`): an
//! array's entry that is not a number reads as 0 and keeps its place.
//!
//! A generated appearance is a content stream in default user space that needs no
//! resources. /CA and the highlight's Multiply
//! blend apply to it as one transparency group.

pub(crate) mod entries;
mod freetext;
mod icons;
mod lines;
mod markup;
mod out;
mod rich;
mod text;
mod widget;

use entries::{Entries, Items, Value};
use out::{Color, Out, written_all};
use text::Fonts;

use crate::BlendMode;
use crate::x_object::box_and_matrix;
use hayro_syntax::object::dict::keys::{AP, AS, BORDER, BS, C, CA, D, F, N, RECT, S, SUBTYPE, W};
use hayro_syntax::object::{Dict, Name, ObjRef, Object, Stream};
use hayro_syntax::xref::XRef;
use kurbo::{Affine, Shape};
use rustc_hash::FxHashMap;
use std::cell::RefCell;

/// What an annotation draws.
pub enum Look<'a> {
    /// Its normal appearance stream, drawn as [`Placement`] says.
    Stream {
        /// The appearance stream.
        stream: Stream<'a>,
        /// Its box and matrix, and where they land on the page.
        placement: Placement,
    },
    /// An appearance generated from its entries.
    Generated(Appearance),
    /// Nothing.
    Nothing,
    /// Something this crate cannot draw: the annotation has no usable appearance
    /// stream, and one cannot be generated.
    Unsupported(Unsupported),
}

/// How an appearance stream is drawn (PDF 32000-1 12.5.5): its `/BBox` and `/Matrix`
/// as viewers read damaged ones, and the transform that places the box, after the
/// matrix, on the annotation's `/Rect`; each number as it is written in content.
/// A renderer and a writer that draw the stream with these, rather than the entries
/// in its dictionary, put it in the same place.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    bbox: [f32; 4],
    matrix: [f32; 6],
    transform: [f32; 6],
}

impl Placement {
    /// The box the stream is clipped to, in its own space: `/BBox`, with 0 for an
    /// entry it lacks or that is not a number, and without entries beyond four.
    pub fn bbox(&self) -> [f32; 4] {
        self.bbox
    }

    /// From the stream's space to the space `transform` maps from: `/Matrix`, read as
    /// `/BBox` is, of six entries; the identity when it is not an array.
    pub fn matrix(&self) -> [f32; 6] {
        self.matrix
    }

    /// From the space `matrix` maps to, to default user space: the matrix A of
    /// [`rect_transform`].
    pub fn transform(&self) -> [f32; 6] {
        self.transform
    }
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
    let annot = Entries::new(annot.clone());
    !annot.is(SUBTYPE, b"Link") && flags_of(&annot) & 3 == 0
}

/// The annotation flags of `annot` (`/F`, PDF 32000-1 12.5.3).
pub fn flags(annot: &Dict<'_>) -> u32 {
    flags_of(&Entries::new(annot.clone()))
}

/// `/F` as viewers read a damaged one: a real number truncated, an integer to its
/// low 32 bits, and no flags for anything else.
fn flags_of(annot: &Entries<'_>) -> u32 {
    // Truncating to 32 bits is the reading meant.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    annot.int(F).map_or(0, |f| f as u32)
}

/// What the annotations of one document look like: [`Appearances::look`], with
/// generated text in the standard fonts a font resolver gives (Helvetica for
/// characters a family's font lacks) and widget defaults from the document's
/// interactive form. Make one per page or document: fonts are loaded once, and each
/// field above widgets is read once.
pub struct Appearances<'a> {
    xref: &'a XRef,
    fonts: Fonts,
    /// The interactive form dictionary (`/AcroForm`).
    form: Option<Entries<'a>>,
    /// The fields read so far, by reference: what the widgets below each inherit, or
    /// `None` when its `/Parent` chain loops.
    fields: RefCell<FxHashMap<ObjRef, Option<Field<'a>>>>,
}

impl<'a> Appearances<'a> {
    /// The appearances of annotations in the document of `xref`, with text in the
    /// fonts `font_resolver` gives for the standard fonts.
    pub fn new(xref: &'a XRef, font_resolver: crate::FontResolverFn) -> Self {
        let form = xref
            .get::<Dict<'a>>(xref.root_id())
            .and_then(|root| root.get::<Dict<'a>>(b"AcroForm"))
            .map(Entries::new);
        Self {
            xref,
            fonts: Fonts::new(font_resolver),
            form,
            fields: RefCell::default(),
        }
    }

    /// What `annot` draws on a page that shows `visible` (in default user space) of it,
    /// with text in `fonts`.
    ///
    /// The normal appearance is `/AP /N`, or, when that is a dictionary of states, the
    /// state `/AS` names. A stream there draws when it has a `/BBox` that, after its
    /// `/Matrix`, has some area, and the annotation a `/Rect`, and nothing otherwise.
    /// An `/AP` that is not a dictionary draws nothing. Without an appearance there
    /// (no `/AP`, no `/N`, no stream for the state), one is generated from the
    /// annotation's entries.
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
                    return match placement(&stream, &Entries::new(annot.clone())) {
                        Ok(Some(placement)) => Look::Stream { stream, placement },
                        Ok(None) => Look::Nothing,
                        Err(why) => Look::Unsupported(why),
                    };
                }
            }
            Some(_) => return Look::Nothing,
        }
        if annot.contains_key(AP) && annot.get::<Object<'_>>(AP).is_none() {
            // A reference to no object.
            return Look::Nothing;
        }
        match self.generate(&Entries::new(annot.clone()), visible) {
            Ok(Some(appearance)) => Look::Generated(appearance),
            Ok(None) => Look::Nothing,
            Err(why) => Look::Unsupported(why),
        }
    }
}

/// Where appearance `stream` is drawn on `annot`: `None` when the annotation has no
/// `/Rect`, or the stream has no `/BBox` that, after its `/Matrix`, encloses some
/// area; [`Unsupported::Coordinates`] when one of its numbers cannot be written.
fn placement(stream: &Stream<'_>, annot: &Entries<'_>) -> Result<Option<Placement>, Unsupported> {
    let Some((bbox, matrix)) = box_and_matrix(stream.dict()) else {
        return Ok(None);
    };
    let Some(rect) = rect(annot) else {
        return Ok(None);
    };
    let bbox = written_all(bbox)?;
    let matrix = written_all(matrix)?;
    let affine = Affine::new(matrix.map(f64::from));
    let [x0, y0, x1, y1] = bbox.map(f64::from);
    let transformed = (affine * kurbo::Rect::new(x0, y0, x1, y1).to_path(0.1)).bounding_box();
    // A box of no area maps onto no rect; a /Rect of no area maps the box onto
    // nothing, which is drawn as such.
    if !(transformed.width() > 0.0 && transformed.height() > 0.0) {
        return Ok(None);
    }
    let transform = written_all(rect_transform(rect, bbox, affine).as_coeffs())?;
    Ok(Some(Placement {
        bbox,
        matrix,
        transform,
    }))
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

impl<'a> Appearances<'a> {
    /// The appearance generated for `annot` from its entries.
    fn generate(&self, annot: &Entries<'a>, visible: kurbo::Rect) -> Generated {
        let Some(subtype) = annot.name(SUBTYPE) else {
            return Ok(None);
        };
        let mut out = Out::default();
        let fonts = &self.fonts;
        let ok = |drew: Option<()>| Ok(drew);
        let drew = match subtype.as_ref() {
            b"Square" => ok(markup::shape(annot, false, &mut out)),
            b"Circle" => ok(markup::shape(annot, true, &mut out)),
            b"Highlight" => ok(markup::highlight(annot, &mut out)),
            b"Underline" => ok(markup::text_lines(annot, markup::Line::Under, &mut out)),
            b"StrikeOut" => ok(markup::text_lines(annot, markup::Line::Through, &mut out)),
            b"Squiggly" => markup::squiggly(annot, visible, &mut out),
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
            b"Widget" => match self.field(annot) {
                Some(field) => {
                    widget::widget(annot, &field, self.form.as_ref(), fonts, visible, &mut out)
                }
                // A widget whose /Parent chain loops is a broken field: nothing.
                None => Ok(None),
            },
            // Popup, Link, the media and 3D subtypes, PrinterMark, TrapNet, Watermark,
            // Projection, stamps of other names and unknown subtypes: nothing.
            _ => Ok(None),
        };
        // A number that could not be written fails the appearance, whatever the
        // generator made of it: `None` from a generator means nothing to draw.
        let content = out.finish()?;
        let drew = drew?;
        // An appearance that paints nothing, such as a widget with no value, border
        // or background, is no appearance.
        let paints = content.lines().any(|l| {
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
            content,
            opacity: opacity(annot),
            blend,
        }))
    }

    /// The field entries widget `annot` is drawn with, however deep its `/Parent`
    /// chain; `None` when the chain loops, which viewers read as a broken field and
    /// draw nothing for. Each field above widgets is read once.
    fn field(&self, annot: &Entries<'a>) -> Option<Field<'a>> {
        let mut known = self.fields.borrow_mut();
        let mut on_walk = rustc_hash::FxHashSet::default();
        on_walk.extend(annot.id());
        // From the widget up to the first field read before, or to the top.
        let mut walk: Vec<(Option<ObjRef>, Entries<'a>)> = vec![(None, annot.clone())];
        let mut field = loop {
            let node = &walk[walk.len() - 1].1;
            let Some((id, parent)) = node.parent(self.xref) else {
                break Some(Field::default());
            };
            // A direct parent dictionary cannot lead back to itself.
            if let Some(id) = id {
                if let Some(field) = known.get(&id) {
                    break field.clone();
                }
                if !on_walk.insert(id) {
                    break None;
                }
            }
            walk.push((id, parent));
        };
        for (id, node) in walk.into_iter().rev() {
            field = field.map(|above| above.below(&node));
            if let Some(id) = id {
                known.insert(id, field.clone());
            }
        }
        field
    }
}

/// The field entries a widget inherits (PDF 32000-1 12.7.3.1) and is drawn with.
#[derive(Clone, Copy)]
enum Inherited {
    Type,
    Flags,
    Value,
    Appearance,
    Quadding,
    MaxLen,
    Options,
    TopIndex,
}

impl Inherited {
    const ALL: [Self; 8] = [
        Self::Type,
        Self::Flags,
        Self::Value,
        Self::Appearance,
        Self::Quadding,
        Self::MaxLen,
        Self::Options,
        Self::TopIndex,
    ];

    fn key(self) -> &'static [u8] {
        match self {
            Self::Type => b"FT",
            Self::Flags => b"Ff",
            Self::Value => b"V",
            Self::Appearance => b"DA",
            Self::Quadding => b"Q",
            Self::MaxLen => b"MaxLen",
            Self::Options => b"Opt",
            Self::TopIndex => b"TI",
        }
    }
}

/// For each [`Inherited`] entry, the nearest dictionary that has it in a widget's
/// chain of fields through `/Parent`; an entry that is null or names no object is
/// absent (PDF 32000-1 7.3.9).
#[derive(Clone, Default)]
struct Field<'a>([Option<Entries<'a>>; Inherited::ALL.len()]);

impl<'a> Field<'a> {
    /// The entries of `node`, a field or widget below the fields of `self`.
    fn below(&self, node: &Entries<'a>) -> Self {
        Self(std::array::from_fn(|i| {
            if node.has(Inherited::ALL[i].key()) {
                Some(node.clone())
            } else {
                self.0[i].clone()
            }
        }))
    }

    /// Whether some dictionary in the chain has `key`.
    fn has(&self, key: Inherited) -> bool {
        self.0[key as usize].is_some()
    }

    fn get(&self, key: Inherited) -> Value<'a> {
        self.0[key as usize]
            .as_ref()
            .map_or(Value::Null, |d| d.get(key.key()))
    }
}

/// /CA clamped to 0–1, 1 when absent or not a number.
fn opacity(annot: &Entries<'_>) -> f32 {
    let opacity = annot.number(CA).unwrap_or(1.0);
    if opacity.is_finite() {
        opacity.clamp(0.0, 1.0) as f32
    } else {
        1.0
    }
}

/// The smallest distance generated geometry keeps apart: a hundredth of a point,
/// finer than any printer or screen resolves (7200 dpi). Steps closer than this, of
/// a squiggle or of comb cell dividers, are drawn as the band they cover, so the
/// work to draw them is bounded by the size of what is visible.
const FINEST: f64 = 0.01;

/// A circle as viewers draw one: four cubic quarter arcs from its top, clockwise, so
/// that it costs the same at any size.
fn circle(c: kurbo::Point, r: f64) -> kurbo::BezPath {
    // The control point distance, in radii, that viewers use.
    const K: f64 = 0.551_915;
    let (x, y, k) = (c.x, c.y, K * r);
    let mut p = kurbo::BezPath::new();
    p.move_to((x, y + r));
    p.curve_to((x + k, y + r), (x + r, y + k), (x + r, y));
    p.curve_to((x + r, y - k), (x + k, y - r), (x, y - r));
    p.curve_to((x - k, y - r), (x - r, y - k), (x - r, y));
    p.curve_to((x - r, y + k), (x - k, y + r), (x, y + r));
    p.close_path();
    p
}

/// The colour of colour array `items` ([`Color::of`]), each component that is not a
/// number read as 0.
fn color(items: &Items<'_>) -> Option<Color> {
    Color::of(&items.numbers())
}

/// The colour of array `key` of `dict`, `None` when it is not an array or transparent.
fn color_of(dict: &Entries<'_>, key: &[u8]) -> Option<Color> {
    color(&dict.items(key)?)
}

/// /C as a colour: `default` when absent, `None` when it is transparent.
fn c_color(annot: &Entries<'_>, default: Color) -> Option<Color> {
    match annot.items(C) {
        Some(items) => color(&items),
        None => Some(default),
    }
}

/// /BS /W, else the third /Border element, else 1; 0 strokes the thinnest line
/// (PDF 32000-1 8.4.3.2), as does a width that is negative or not a number.
fn border_width(annot: &Entries<'_>) -> f64 {
    let width = annot
        .dict(BS)
        .and_then(|bs| bs.number(W))
        .or_else(|| annot.items(BORDER).and_then(|b| b.get(2).number()))
        .unwrap_or(1.0);
    // `max` reads no number as 0 too.
    width.max(0.0)
}

/// The dash array of a dashed border style (/BS /S /D), else the fourth /Border
/// element; at most ten entries, as in `PDFium`.
fn dash(annot: &Entries<'_>) -> Vec<f64> {
    let items = match annot.dict(BS) {
        Some(bs) if bs.is(S, b"D") => bs.items(D),
        _ => annot.items(BORDER).and_then(|b| b.get(3).items()),
    };
    let dash: Vec<f64> = items
        .map(|a| a.numbers().into_iter().take(10).collect())
        .unwrap_or_default();
    // An all-zero or negative pattern draws nothing; draw solid instead.
    if dash.iter().all(|d| *d >= 0.0) && dash.iter().any(|d| *d > 0.0) {
        dash
    } else {
        Vec::new()
    }
}

/// `/Rect`, normalized, read as viewers read a damaged one: a coordinate it lacks
/// or that is not a number is 0. `None` when it is not an array.
fn rect(annot: &Entries<'_>) -> Option<kurbo::Rect> {
    let n = annot.numbers(RECT, 4)?;
    Some(kurbo::Rect::new(n[0], n[1], n[2], n[3]).abs())
}
