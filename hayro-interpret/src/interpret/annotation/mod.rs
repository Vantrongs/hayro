//! How annotations look: the appearance stream an annotation names and where it lands
//! on the page, or, for an annotation without a usable one, an appearance generated
//! from its entries (PDF 32000-1 12.5).
//!
//! The renderer draws a page's annotations as [`Appearances::page`] lists them, and a
//! writer that flattens annotations into page content can use the same to draw what
//! the renderer shows.
//!
//! What a generated appearance shows follows the PDF specification. The markup
//! shapes (`markup.rs`) are adapted from `PDFium` (`core/fpdfdoc/cpdf_generateap.cpp`,
//! BSD-3-Clause; its license is in the repository's `NOTICE.md`). The layout of the
//! other generators, which the spec leaves to the viewer, is written here and sized
//! to match the rendered output of common viewers; each module says what it draws.
//! Text is drawn as glyph outlines from the standard fonts the font resolver gives,
//! each glyph a form written once and placed where it is drawn.
//!
//! Annotation dictionaries are read as viewers read damaged ones (`entries.rs`): an
//! array's entry that is not a number reads as 0 and keeps its place.
//!
//! A generated appearance is a content stream in default user space that needs no
//! resources but the forms of the glyphs it draws ([`Glyph`]), each written once. /CA
//! and the highlight's Multiply blend apply to each thing it paints, as the graphics
//! state it is drawn in, as viewers draw them: where two of its fills overlap, they
//! blend twice.

pub(crate) mod entries;
mod freetext;
mod icons;
mod lines;
mod markup;
mod out;
mod rich;
mod text;
mod widget;

use entries::{Annots, Entries, Id, Items, Label, Listed, Objects, Source, StreamEntries, Value};
use out::{Color, Out, written_all};
use text::{Family, Fonts, Shaped, Style, StyleKey, Typeface};

use super::AnnotationSelection;
use crate::BlendMode;
use crate::x_object::box_and_matrix;
use hayro_syntax::hash;
use hayro_syntax::lost::Lost;
use hayro_syntax::object::dict::keys::{AP, AS, BORDER, BS, C, CA, D, F, N, RECT, SUBTYPE, W};
use hayro_syntax::object::{Name, Stream};
use hayro_syntax::page::Page;
use hayro_syntax::xref::{ContentKey, XRef};
use kurbo::{Affine, Shape};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

thread_local! {
    static WORK: Cell<u64> = const { Cell::new(0) };
    static MOST_PIECES: Cell<u32> = const { Cell::new(262_144) };
}

/// Counts `n` units of the work of generating appearances ([`work_done`]).
fn work(n: usize) {
    WORK.with(|w| w.set(w.get().saturating_add(n as u64)));
}

/// The units of work generating appearances has done on this thread so far: each
/// glyph set or drawn, line laid out, step of a search over text, option and number
/// read, and part of a default appearance string read. The work an appearance takes
/// grows with what it draws and with what the file holds, not with the times
/// annotations share an entry; tests bound it.
pub fn work_done() -> u64 {
    WORK.with(Cell::get)
}

/// Sets the most pieces one shape of repeated pieces draws where the page shows it
/// (the steps of a squiggle, the dividers of a comb field), for appearances
/// generated on this thread from then on: 262 144 unless set, 5 to 10 MB of
/// content. A shape that would draw more is [`Unsupported::Coordinates`]; tests
/// set a few to reach that with few pieces.
pub fn set_most_pieces(n: u32) {
    MOST_PIECES.with(|m| m.set(n));
}

/// The most pieces one shape draws ([`set_most_pieces`]).
fn most_pieces() -> f64 {
    f64::from(MOST_PIECES.with(Cell::get))
}

/// What an annotation draws.
#[derive(Clone)]
pub enum Look<'a> {
    /// Its normal appearance stream, drawn as [`Placement`] says.
    Stream {
        /// The appearance stream.
        stream: Stream<'a>,
        /// Its box and matrix, and where they land on the page.
        placement: Placement,
    },
    /// An appearance generated from its entries.
    Generated(Rc<Appearance>),
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
    /// A coordinate it draws at is too large to write: 2^23 or more, where the
    /// 32-bit numbers content streams are read in are a unit or more apart.
    Coordinates,
    /// Its rich text (`/RC`) or default style (`/DS`) uses an element, attribute or
    /// style declaration the generator cannot draw as viewers do, nests or repeats
    /// elements beyond what written rich text does, or sets text both in sizes in
    /// points and in a negative default appearance size, which run opposite ways.
    RichText,
    /// A number it is drawn by has digits whose reading by viewers is not
    /// established (see `entries.rs`).
    Number,
    /// It draws a line ending at a negative border width, or along a line of no
    /// length, which viewers clip by rules not reproduced.
    Ending,
    /// Its field value, `/V`, is a stream whose data do not decode, or are encoded
    /// otherwise than by Flate alone: viewers show part of damaged data by rules of
    /// their decoders' buffering, and read data cut short in other filters otherwise,
    /// which the generator does not reproduce.
    ValueStream,
}

/// An appearance generated for an annotation without a usable appearance stream.
/// Its opacity and blend mode are the graphics state its content is drawn in (an
/// `ExtGState` set before it, not a transparency group around it): they apply to
/// each fill and stroke of the content on its own.
pub struct Appearance {
    /// The content stream, in default user space; it uses no resources but the
    /// glyphs of [`Appearance::glyphs`], each drawn as a form `XObject` with `/name Do`.
    pub content: String,
    /// The glyphs the content draws, each once.
    pub glyphs: Vec<Rc<Glyph>>,
    /// The annotation's `/CA`, clamped to 0–1, for both fills and strokes; 1 for a
    /// widget. `/ca` is not read: viewers draw these appearances without it.
    pub opacity: f32,
    /// Multiply for highlights, Normal otherwise.
    pub blend: BlendMode,
}

/// A glyph of generated text, which an [`Appearance`]'s content draws as a form
/// `XObject` with `/name Do` in a space of 1000 units to the em: its outline, filled,
/// in that space, and the box of its points. Its name is the same wherever the
/// [`Appearances`] that made it draw it, so that a writer writes it once.
#[derive(Debug)]
pub struct Glyph {
    name: Rc<str>,
    content: String,
    bbox: [f32; 4],
}

impl Glyph {
    /// The glyph named `name` with outline `path` and the box `ink` of its points;
    /// `None` when a number of them cannot be written.
    fn new(name: String, path: &kurbo::BezPath, ink: kurbo::Rect) -> Option<Self> {
        let mut out = Out::default();
        out.path(path)?;
        out.op("f");
        let (content, _) = out.finish().ok()?;
        let bbox =
            written_all([ink.x0.floor(), ink.y0.floor(), ink.x1.ceil(), ink.y1.ceil()]).ok()?;
        Some(Self {
            name: name.into(),
            content,
            bbox,
        })
    }

    /// The name the content draws it by.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn name_rc(&self) -> Rc<str> {
        self.name.clone()
    }

    /// Its outline as a content stream that fills it, in a space of 1000 units to the
    /// em.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// The box its points lie in, in that space.
    pub fn bbox(&self) -> [f32; 4] {
        self.bbox
    }
}

/// An annotation a page draws, or one it cannot draw ([`Appearances::page`]).
#[derive(Clone)]
pub struct Drawn<'a> {
    index: usize,
    annotation: Rc<Annotation<'a>>,
}

impl<'a> Drawn<'a> {
    /// Its place among the entries of the page's `/Annots`.
    pub fn index(&self) -> usize {
        self.index
    }

    /// What it draws: never [`Look::Nothing`].
    pub fn look(&self) -> &Look<'a> {
        &self.annotation.look
    }

    /// Its `/Subtype`, to tell which annotation it is.
    pub fn subtype(&self) -> Option<&[u8]> {
        self.annotation.subtype.as_deref().map(|n| &**n)
    }
}

/// What an annotation looks like on a page, and its subtype.
struct Annotation<'a> {
    look: Look<'a>,
    subtype: Option<Rc<Name<'a>>>,
}

/// What an annotation a page lists comes to before the area the page shows is known.
enum Plan<'a> {
    /// The same on every page: an appearance stream, or why it cannot be drawn.
    Fixed(Rc<Annotation<'a>>),
    /// An appearance generated for each area shown, from `annot`, read as its own
    /// the first time and as a shared object's after.
    Generate {
        id: Id<'a>,
        annot: Entries<'a>,
        subtype: Option<Rc<Name<'a>>>,
        again: Cell<bool>,
    },
}

/// What the state an annotation's `/AS` names among its normal appearances comes to
/// ([`Appearances::state`]).
#[derive(Clone)]
enum State<'a> {
    /// A stream, its appearance.
    Stream(StreamEntries<'a>),
    /// There but no stream: the annotation draws nothing.
    Nothing,
    /// No such state, or no `/AS`: as if there were no appearance.
    Absent,
}

/// What an annotation comes to before generating: an appearance stream, nothing,
/// why it cannot be drawn, or an appearance to generate.
enum Prepared<'a> {
    Look(Look<'a>),
    Generate,
}

/// The annotations a page lists that may draw, each with its place in `/Annots`.
type Plans<'a> = Rc<[(usize, Rc<Plan<'a>>)]>;

/// The area a page shows and the points of its unit, as a key: their bits.
type View = [u64; 5];

fn view(visible: kurbo::Rect, unit: f64) -> View {
    [visible.x0, visible.y0, visible.x1, visible.y1, unit].map(f64::to_bits)
}

/// Whether `annot` is drawn as `selection` says (PDF 32000-1 12.5.3), by its flags
/// (`/F`), read as a number.
fn selected(annot: &Entries<'_>, selection: AnnotationSelection) -> bool {
    let flags = annot.get(F).flags();
    match selection {
        AnnotationSelection::View => flags & (2 | 32) == 0,
        AnnotationSelection::Print => flags & 2 == 0 && flags & 4 != 0,
        AnnotationSelection::Flatten => flags & 3 == 0 && !annot.is(SUBTYPE, b"Link"),
    }
}

/// What the annotations of one document look like: [`Appearances::page`], with
/// generated text in the standard fonts a font resolver gives (Helvetica for
/// characters a family's font lacks) and widget defaults from the document's
/// interactive form. Make one per document: fonts are loaded once, and each field
/// above widgets is read once, however many pages its widgets are on.
pub struct Appearances<'a> {
    xref: &'a XRef,
    objects: Objects<'a>,
    fonts: Fonts,
    fields: Fields<'a>,
}

/// What has been read of a document for its annotations so far: its objects, each
/// read once ([`Objects`]); the fields, by what each is read from, with what the widgets below
/// each inherit, or `None` when its `/Parent` chain loops; default appearances and
/// list options, by the entry that holds them; and text set in its fonts, so that
/// what many annotations share is read and set once. Shared by every
/// [`Appearances`] of the document a renderer makes, one per page, through its
/// document's cache.
///
/// Each value is kept with what reading it lost ([`Lost`]), which every later use
/// is told as the first was ([`XRef::note`]), as the reader's own caches do
/// ([`hayro_syntax::lost::Memo`]): a page drawn with what another page's
/// annotations read without an object is told so.
#[derive(Clone, Default)]
pub(crate) struct Fields<'a>(Rc<RefCell<Known<'a>>>);

#[derive(Default)]
struct Known<'a> {
    /// The document's objects, each read once.
    objects: Option<Objects<'a>>,
    /// The interactive form dictionary, once read.
    form: Option<(Option<Entries<'a>>, Lost)>,
    /// What each annotation pages list comes to, by which it is and the selection;
    /// `None` for one that draws nothing on any page.
    plans: Kept<(Id<'a>, AnnotationSelection), Option<Rc<Plan<'a>>>>,
    /// The appearances generated, by annotation and area shown.
    generated: Kept<(Id<'a>, View), Option<Rc<Annotation<'a>>>>,
    /// Of each `/Annots` array, an object or written in a page, by selection, the
    /// annotations it lists that may draw, each with its place.
    lists: Kept<(Id<'a>, AnnotationSelection), Plans<'a>>,
    /// What a page draws, by its `/Annots` array, the selection and the area shown.
    pages: Kept<(Id<'a>, AnnotationSelection, View), Rc<[Drawn<'a>]>>,
    /// The states `/AS` entries held in objects of their own name, by the
    /// dictionary of states and the entry ([`Appearances::state`]).
    states: Kept<(ContentKey<'a>, Source<'a>), State<'a>>,
    /// Each field and widget above a widget, by the dictionary it is read from.
    fields: Kept<ContentKey<'a>, Option<Field<'a>>>,
    /// Text set in its fonts, by what it is made of.
    shaped: Kept<TextKey<'a>, Result<Rc<Shaped>, Unsupported>>,
    /// The text of stamps, by the entry that holds the name.
    stamps: Kept<Source<'a>, Result<Rc<icons::StampText>, Unsupported>>,
    /// The styles default styles (`/DS`) set, by the entry that holds each and
    /// whether relative sizes are of a positive base size.
    styles: Kept<(Source<'a>, bool), Result<Style, Unsupported>>,
    /// The options of lists, by the entry that holds them.
    labels: Kept<Source<'a>, Rc<Vec<Label<'a>>>>,
    /// Default appearances read, by the entry that holds each, and the form's; with
    /// whether reading each met a number whose reading is not established.
    appearances: Kept<Source<'a>, (DefaultAppearance, bool)>,
    form_appearance: Option<((DefaultAppearance, bool), Lost)>,
    /// What text and choice fields show, by what it is made of.
    field_text: Kept<TextKey<'a>, Result<widget::FieldText, Unsupported>>,
}

/// Values found once for the document, each with what finding it lost.
type Kept<K, V> = hash::Map<K, (V, Lost)>;

/// A default appearance as [`text::default_appearance`] reads it.
type DefaultAppearance = (Family, Option<f32>, Option<Color>);

/// What text is made of, as a key for setting it once per document: the entries it
/// is read from, the typeface it is set in, the style it is set in, when one is
/// read from an entry of its own (a default style, `/DS`), and numbers that tell
/// apart the ways it is read ([`TEXT`] and those after it first). A style is part
/// of the key by its value, so that annotations that each hold an equal style set
/// a text they share once.
#[derive(Clone, PartialEq, Eq, Hash)]
struct TextKey<'a> {
    sources: Vec<Source<'a>>,
    face: Typeface,
    style: Option<StyleKey>,
    numbers: Vec<u64>,
}

/// The ways text is read: plain text, a line's caption on one line, rich text of a
/// `FreeText`, rich text of a field, a list's option.
const TEXT: u64 = 0;
const CAPTION: u64 = 1;
const FREE_TEXT: u64 = 2;
const FIELD: u64 = 3;
const OPTION: u64 = 4;

impl<'a> TextKey<'a> {
    /// The key of text read from entries `sources`, in `face` and the way `numbers`
    /// tell.
    fn new(
        sources: impl IntoIterator<Item = Source<'a>>,
        face: Typeface,
        numbers: Vec<u64>,
    ) -> Self {
        Self {
            sources: sources.into_iter().collect(),
            face,
            style: None,
            numbers,
        }
    }

    /// This key, of text set in `style`.
    fn styled(self, style: &Style) -> Self {
        Self {
            style: Some(style.key()),
            ..self
        }
    }
}

impl<'a> Appearances<'a> {
    /// The appearances of annotations in the document of `xref`, with text in the
    /// fonts `font_resolver` gives for the standard fonts.
    pub fn new(xref: &'a XRef, font_resolver: crate::FontResolverFn) -> Self {
        Self::with_fields(xref, font_resolver, Fields::default())
    }

    /// As [`Appearances::new`], reading fields into `fields`, the fields of the
    /// document of `xref` read before.
    pub(crate) fn with_fields(
        xref: &'a XRef,
        font_resolver: crate::FontResolverFn,
        fields: Fields<'a>,
    ) -> Self {
        let objects = fields
            .0
            .borrow_mut()
            .objects
            .get_or_insert_with(|| Objects::new(xref))
            .clone();
        Self {
            xref,
            objects,
            fonts: Fonts::new(font_resolver),
            fields,
        }
    }

    /// The interactive form dictionary (`/AcroForm`), read once for the document,
    /// where a widget needs it: what reading it lost is told to each computation
    /// that uses it.
    fn form(&self) -> Option<Entries<'a>> {
        let known = self.fields.0.borrow().form.clone();
        let (form, lost) = known.unwrap_or_else(|| {
            let made = self.xref.watch(|| {
                self.objects
                    .catalog()
                    .and_then(|root| root.dict(b"AcroForm"))
            });
            self.fields.0.borrow_mut().form = Some(made.clone());
            made
        });
        self.xref.note(lost);
        form
    }

    /// The annotations of `page` that `selection` draws and that draw something or
    /// cannot be drawn, in the order of its `/Annots`, as they look on a page that
    /// shows `visible` (in default user space, whose unit is `unit` points: the
    /// page's `/UserUnit`, 1 for a page drawn in its own units) of them, with text
    /// in the fonts of these appearances. An entry of `/Annots` that names no dictionary names no
    /// annotation; one whose flags cannot be read as viewers read them cannot be
    /// drawn ([`Unsupported::Number`]).
    ///
    /// The normal appearance is `/AP /N`, or, when that is a dictionary of states, the
    /// state `/AS` names. A stream there draws when it has a `/BBox` that, after its
    /// `/Matrix`, has some area, and nothing otherwise; without a `/Rect` it cannot
    /// be drawn ([`placement`]).
    /// An `/AP` that is not a dictionary, or refers to no object, draws nothing, but on a
    /// button widget, which viewers draw anew. Without an appearance there
    /// (no `/AP`, no `/N`, no stream for the state), one is generated from the
    /// annotation's entries.
    ///
    /// What an annotation comes to is found once for the document, by the object
    /// it is or where it is written, the selection and the area shown, however many
    /// times pages list it; and what a page draws, by its `/Annots` array, an object
    /// or written in the page, the selection and the area shown. A page listing
    /// again what was found costs a lookup, whatever it holds and whatever it draws.
    pub fn page(
        &self,
        page: &Page<'a>,
        selection: AnnotationSelection,
        visible: kurbo::Rect,
        unit: f64,
    ) -> Rc<[Drawn<'a>]> {
        let Some(annots) = self.objects.annots(page.raw()) else {
            return Rc::from([]);
        };
        let view = view(visible, unit);
        // A page of no size, or of none that is a number, shows no piece more than
        // what lies in it.
        let beyond = if unit.abs() > 0.0 && unit.is_finite() {
            PAINTS_BEYOND / unit.abs()
        } else {
            0.0
        };
        let array = annots.id();
        self.once(
            |k| &mut k.pages,
            Some((array, selection, view)),
            || {
                let plans = self.once(
                    |k| &mut k.lists,
                    Some((array, selection)),
                    || self.plans(&annots, selection).into(),
                );
                self.drawn(&plans, visible, beyond, view).into()
            },
        )
    }

    /// The annotations `annots` lists that `selection` draws and that may draw
    /// something, each with its place, each found once for the document.
    fn plans(
        &self,
        annots: &Annots<'a>,
        selection: AnnotationSelection,
    ) -> Vec<(usize, Rc<Plan<'a>>)> {
        annots
            .entries()
            .filter_map(|listed| {
                let plan = self.once(
                    |k| &mut k.plans,
                    Some((listed.id, selection)),
                    || self.plan(&listed, selection),
                )?;
                Some((listed.index, plan))
            })
            .collect()
    }

    /// What the annotation `listed` names comes to as `selection` draws it, before
    /// the area shown is known; `None` when it draws nothing on any page.
    fn plan(&self, listed: &Listed<'a>, selection: AnnotationSelection) -> Option<Rc<Plan<'a>>> {
        let annot = self.objects.annotation(listed)?;
        let subtype = annot.name(SUBTYPE);
        let fixed = |look| {
            let annotation = Annotation {
                look,
                subtype: subtype.clone(),
            };
            Some(Rc::new(Plan::Fixed(Rc::new(annotation))))
        };
        match entries::read(|| selected(&annot, selection)) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(why) => return fixed(Look::Unsupported(why)),
        }
        match entries::read(|| self.prepare(&annot)) {
            Ok(Prepared::Look(Look::Nothing)) => None,
            Ok(Prepared::Look(look)) => fixed(look),
            Ok(Prepared::Generate) => Some(Rc::new(Plan::Generate {
                id: listed.id,
                annot,
                subtype,
                again: Cell::new(false),
            })),
            Err(why) => fixed(Look::Unsupported(why)),
        }
    }

    /// The annotations of `plans` as drawn on a page that shows `visible`, where a
    /// stroke paints `beyond` its outline, whose key is `view`: those generated,
    /// once for the document for each area shown.
    fn drawn(
        &self,
        plans: &[(usize, Rc<Plan<'a>>)],
        visible: kurbo::Rect,
        beyond: f64,
        view: View,
    ) -> Vec<Drawn<'a>> {
        plans
            .iter()
            .filter_map(|(index, plan)| {
                let annotation = match &**plan {
                    Plan::Fixed(annotation) => annotation.clone(),
                    Plan::Generate { id, .. } => self.once(
                        |k| &mut k.generated,
                        Some((*id, view)),
                        || self.generated(plan, visible, beyond),
                    )?,
                };
                Some(Drawn {
                    index: *index,
                    annotation,
                })
            })
            .collect()
    }

    /// The appearance `plan`, [`Plan::Generate`], generates for a page that shows
    /// `visible`, where a stroke paints `beyond` its outline; `None` when it paints
    /// nothing.
    fn generated(
        &self,
        plan: &Plan<'a>,
        visible: kurbo::Rect,
        beyond: f64,
    ) -> Option<Rc<Annotation<'a>>> {
        let Plan::Generate {
            annot,
            subtype,
            again,
            ..
        } = plan
        else {
            return None;
        };
        let annot = if again.replace(true) {
            annot.shared()
        } else {
            annot.clone()
        };
        let look = match entries::read(|| self.generate(&annot, visible, beyond)) {
            Ok(Ok(Some(appearance))) => Look::Generated(Rc::new(appearance)),
            Ok(Ok(None)) => return None,
            Ok(Err(why)) | Err(why) => Look::Unsupported(why),
        };
        Some(Rc::new(Annotation {
            look,
            subtype: subtype.clone(),
        }))
    }

    /// What `annot` comes to before generating (see [`Appearances::page`]).
    fn prepare(&self, annot: &Entries<'a>) -> Prepared<'a> {
        match annot.get(AP) {
            // A reference to null or to no object.
            Value::Null if annot.present(AP) => return self.not_a_dictionary(annot),
            Value::Null => {}
            Value::Dict(ap) => {
                // Each read as any object: a dictionary read as a stream may pick up
                // the data of a stream after it in the file.
                let stream = match ap.get(N) {
                    Value::Stream(stream) => Some(stream),
                    Value::Dict(states) => match self.state(annot, &states) {
                        State::Stream(stream) => Some(stream),
                        State::Nothing => return Prepared::Look(Look::Nothing),
                        State::Absent => None,
                    },
                    _ => None,
                };
                if let Some(stream) = stream {
                    return Prepared::Look(match placement(&stream, annot) {
                        Ok(Some(placement)) => Look::Stream {
                            stream: stream.into_stream(),
                            placement,
                        },
                        Ok(None) => Look::Nothing,
                        Err(why) => Look::Unsupported(why),
                    });
                }
            }
            _ => return self.not_a_dictionary(annot),
        }
        self.to_generate(annot)
    }

    /// The state `annot`'s `/AS` names among `states`, its normal appearances, found
    /// once for the document for each `/AS` held in an object of its own and each
    /// dictionary of states: annotations that share both find it by a lookup, which
    /// neither hashes nor compares the name, however long.
    fn state(&self, annot: &Entries<'a>, states: &Entries<'a>) -> State<'a> {
        let find = || {
            let Some(state) = annot.name(AS) else {
                return State::Absent;
            };
            match states.get(&state) {
                Value::Stream(stream) => State::Stream(stream),
                // A state that is there but is no stream, a reference to no object
                // among them, draws nothing (observed).
                _ if states.present(&state) => State::Nothing,
                _ => State::Absent,
            }
        };
        let key = Some(annot.source(AS))
            .filter(Source::is_object)
            .map(|s| (states.content_key(), s));
        self.once(|k| &mut k.states, key, find)
    }

    /// What `annot`, whose `/AP` is neither a dictionary nor absent, comes to:
    /// nothing, but a button, whose appearance viewers make anew.
    fn not_a_dictionary(&self, annot: &Entries<'a>) -> Prepared<'a> {
        let button = annot.is(SUBTYPE, b"Widget")
            && self
                .field(annot)
                .is_some_and(|f| f.get(Inherited::Type).name() == Some(b"Btn"));
        if button {
            self.to_generate(annot)
        } else {
            Prepared::Look(Look::Nothing)
        }
    }

    /// An appearance to generate for `annot`: none for one held directly in a
    /// page's `/Annots` rather than an object of its own, which viewers draw only by
    /// its appearance stream (observed in `PyMuPDF` bake).
    fn to_generate(&self, annot: &Entries<'a>) -> Prepared<'a> {
        match annot.own() {
            Some(true) => Prepared::Generate,
            Some(false) => Prepared::Look(Look::Nothing),
            None => Prepared::Look(Look::Unsupported(Unsupported::Kind)),
        }
    }
}

/// Where appearance `stream` is drawn on `annot`: `None` when the stream has no
/// `/BBox` that, after its `/Matrix`, encloses some area; [`Unsupported::Coordinates`]
/// when one of its numbers cannot be written, and when the annotation has no
/// `/Rect` that is an array, where viewers place the box on one from about 2^31 to
/// -2^31 (observed in `PyMuPDF` 1.28.2 bake), too large to write.
fn placement(
    stream: &StreamEntries<'_>,
    annot: &Entries<'_>,
) -> Result<Option<Placement>, Unsupported> {
    let Some((bbox, matrix)) = box_and_matrix(&stream.dict()) else {
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
    let rect = rect(annot).ok_or(Unsupported::Coordinates)?;
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
    /// The appearance generated for `annot` from its entries, for a page that shows
    /// `visible`, where a stroke paints `beyond` its outline (in the page's units).
    fn generate(&self, annot: &Entries<'a>, visible: kurbo::Rect, beyond: f64) -> Generated {
        let Some(subtype) = annot.name(SUBTYPE) else {
            return Ok(None);
        };
        let mut out = Out::default();
        let fonts = &self.fonts;
        let ok = |drew: Option<()>| Ok(drew);
        let drew = match &**subtype {
            b"Square" => ok(markup::shape(annot, false, &mut out)),
            b"Circle" => ok(markup::shape(annot, true, &mut out)),
            b"Highlight" => ok(markup::highlight(annot, &mut out)),
            b"Underline" => ok(markup::text_lines(annot, markup::Line::Under, &mut out)),
            b"StrikeOut" => ok(markup::text_lines(annot, markup::Line::Through, &mut out)),
            b"Squiggly" => markup::squiggly(annot, visible, beyond, &mut out),
            b"Ink" => ok(markup::ink(annot, &mut out)),
            b"Text" => icons::icon(annot, icons::Icon::Note, fonts, &mut out),
            b"FileAttachment" => icons::icon(annot, icons::Icon::FileAttachment, fonts, &mut out),
            b"Sound" => icons::icon(annot, icons::Icon::Sound, fonts, &mut out),
            b"Caret" => ok(icons::caret(annot, &mut out)),
            b"Redact" => ok(icons::redact(annot, &mut out)),
            b"Stamp" => icons::stamp(annot, self, visible, &mut out),
            b"FreeText" => freetext::free_text(annot, self, visible, &mut out),
            b"Line" => lines::line(annot, self, visible, &mut out),
            b"Polygon" => lines::poly(annot, true, &mut out),
            b"PolyLine" => lines::poly(annot, false, &mut out),
            b"Widget" => match self.field(annot) {
                Some(field) => widget::widget(annot, &field, self, visible, beyond, &mut out),
                // A widget whose /Parent chain loops is a broken field: nothing.
                None => Ok(None),
            },
            // Popup, Link, the media and 3D subtypes, PrinterMark, TrapNet, Watermark,
            // Projection, stamps of other names and unknown subtypes: nothing.
            _ => Ok(None),
        };
        // A number that could not be written fails the appearance, whatever the
        // generator made of it: `None` from a generator means nothing to draw.
        let (content, glyphs) = out.finish()?;
        let drew = drew?;
        // An appearance that paints nothing, such as a widget with no value, border
        // or background, is no appearance.
        let paints = !glyphs.is_empty()
            || content.lines().any(|l| {
                matches!(
                    l.rsplit(' ').next(),
                    Some("f" | "f*" | "S" | "s" | "b" | "b*" | "B" | "B*")
                )
            });
        if drew.is_none() || !paints {
            return Ok(None);
        }
        let blend = if **subtype == *b"Highlight" {
            BlendMode::Multiply
        } else {
            BlendMode::Normal
        };
        // A widget is drawn opaque whatever its /CA says (observed in `PyMuPDF`
        // 1.28.2 bake: text, choice and button fields alike, /CA 0 or 0.5).
        let opacity = if **subtype == *b"Widget" {
            1.0
        } else {
            opacity(annot)
        };
        Ok(Some(Appearance {
            content,
            glyphs,
            opacity,
            blend,
        }))
    }

    /// `make()`, made once for the document under `key` in the map `map` gives;
    /// made each time without a key. What making it lost is told to the
    /// computation asking, now and each time it is asked for again.
    fn once<K: Eq + std::hash::Hash, V: Clone>(
        &self,
        map: for<'k> fn(&'k mut Known<'a>) -> &'k mut Kept<K, V>,
        key: Option<K>,
        make: impl FnOnce() -> V,
    ) -> V {
        let Some(key) = key else {
            return make();
        };
        let known = map(&mut self.fields.0.borrow_mut()).get(&key).cloned();
        if let Some((made, lost)) = known {
            self.xref.note(lost);
            return made;
        }
        // Made without the map borrowed: making may look other things up.
        let (made, lost) = self.xref.watch(make);
        map(&mut self.fields.0.borrow_mut()).insert(key, (made.clone(), lost));
        made
    }

    /// The text `make` sets, made once for the document under `key`.
    fn shaped(
        &self,
        key: TextKey<'a>,
        make: impl FnOnce() -> Result<Shaped, Unsupported>,
    ) -> Result<Rc<Shaped>, Unsupported> {
        self.once(|k| &mut k.shaped, Some(key), || make().map(Rc::new))
    }

    /// `text()`, of entry `source`, set in `face` as plain text, read as `way`
    /// ([`TEXT`] or [`CAPTION`]), made once for the document.
    fn plain(
        &self,
        source: Source<'a>,
        face: Typeface,
        way: u64,
        text: impl FnOnce() -> Result<String, Unsupported>,
    ) -> Result<Rc<Shaped>, Unsupported> {
        let key = TextKey::new([source], face, vec![way]);
        self.shaped(key, || self.fonts.shape_plain(face, &text()?))
    }

    /// The labels of the options of `/Opt`, of entry `source`, read once for the
    /// document ([`Items::labels`]); none is set.
    fn labels(
        &self,
        source: Source<'a>,
        items: impl FnOnce() -> Option<Items<'a>>,
    ) -> Rc<Vec<Label<'a>>> {
        self.once(
            |k| &mut k.labels,
            Some(source),
            || Rc::new(items().map(|i| i.labels()).unwrap_or_default()),
        )
    }

    /// Option `label` set in `face` as plain text, once for the document.
    fn label(&self, label: &Label<'a>, face: Typeface) -> Result<Rc<Shaped>, Unsupported> {
        let key = TextKey::new([label.source], face, vec![OPTION]);
        self.shaped(key, || {
            let text = label.value.text();
            self.fonts.shape_plain(
                face,
                &text.map(|t| text::text_string(&t)).unwrap_or_default(),
            )
        })
    }

    /// The text of stamp `annot` ([`icons::stamp_text`]), set once for the document
    /// by the entry that holds its name.
    fn stamp_text(&self, annot: &Entries<'a>) -> Result<Rc<icons::StampText>, Unsupported> {
        self.once(
            |k| &mut k.stamps,
            Some(annot.source(b"Name")),
            || icons::stamp_text(annot, &self.fonts).map(Rc::new),
        )
    }

    /// The style the default style (`/DS`) of `annot` sets, relative sizes of a base
    /// size that is `positive` or not ([`freetext::default_style`]), read once for
    /// the document by the entry that holds it.
    fn default_style(&self, annot: &Entries<'a>, positive: bool) -> Result<Style, Unsupported> {
        self.once(
            |k| &mut k.styles,
            Some((annot.source(entries::Text::DefaultStyle.key()), positive)),
            || {
                freetext::default_style(
                    annot.text(entries::Text::DefaultStyle).as_deref(),
                    positive,
                )
            },
        )
    }

    /// The default appearance `read()` gives, of entry `source`, or the form's
    /// without one, read once for the document.
    fn default_appearance(
        &self,
        source: Option<Source<'a>>,
        read: impl FnOnce() -> Option<Vec<u8>>,
    ) -> DefaultAppearance {
        let make = || entries::noting(|| text::default_appearance(&read().unwrap_or_default()));
        let (da, unreadable) = if source.is_none() {
            let known = self.fields.0.borrow().form_appearance;
            let (made, lost) = known.unwrap_or_else(|| {
                let made = self.xref.watch(make);
                self.fields.0.borrow_mut().form_appearance = Some(made);
                made
            });
            self.xref.note(lost);
            made
        } else {
            self.once(|k| &mut k.appearances, source, make)
        };
        if unreadable {
            entries::unreadable();
        }
        da
    }

    /// The field entries widget `annot` is drawn with, however deep its `/Parent`
    /// chain; `None` when the chain loops, which viewers read as a broken field and
    /// draw nothing for. Each field above widgets is read once, by the dictionary it
    /// is read from however it is reached, and kept with what reading it and the
    /// fields above it lost.
    fn field(&self, annot: &Entries<'a>) -> Option<Field<'a>> {
        let mut on_walk = hash::Set::default();
        on_walk.insert(annot.content_key());
        // From the widget up to the first field read before, or to the top; what
        // reading the walk lost is lost to each field on it.
        let mut walk: Vec<(Option<ContentKey<'a>>, Entries<'a>)> = vec![(None, annot.clone())];
        let (mut field, lost) = self.xref.watch(|| {
            loop {
                let node = &walk[walk.len() - 1].1;
                let Some(parent) = node.parent() else {
                    break Some(Field::default());
                };
                let id = parent.content_key();
                let known = self.fields.0.borrow().fields.get(&id).cloned();
                if let Some((field, lost)) = known {
                    self.xref.note(lost);
                    break field;
                }
                if !on_walk.insert(id) {
                    break None;
                }
                walk.push((Some(id), parent));
            }
        });
        let (fields, lost) = self.xref.watch(|| {
            self.xref.note(lost);
            let mut fields = Vec::new();
            for (id, node) in walk.into_iter().rev() {
                field = field.clone().map(|above| above.below(&node));
                fields.extend(id.map(|id| (id, field.clone())));
            }
            fields
        });
        let known = &mut self.fields.0.borrow_mut().fields;
        for (id, field) in fields {
            known.insert(id, (field, lost));
        }
        field
    }
}

/// The field entries a widget inherits (PDF 32000-1 12.7.3.1) and is drawn with,
/// as viewers inherit them: `/Q`, `/Opt` and `/TI` they read from the widget alone.
#[derive(Clone, Copy)]
enum Inherited {
    Type,
    Flags,
    Value,
    Appearance,
    MaxLen,
}

impl Inherited {
    const ALL: [Self; 5] = [
        Self::Type,
        Self::Flags,
        Self::Value,
        Self::Appearance,
        Self::MaxLen,
    ];

    fn key(self) -> &'static [u8] {
        match self {
            Self::Type => b"FT",
            Self::Flags => b"Ff",
            Self::Value => b"V",
            Self::Appearance => b"DA",
            Self::MaxLen => b"MaxLen",
        }
    }
}

/// For each [`Inherited`] entry, the nearest dictionary that has it in a widget's
/// chain of fields through `/Parent`, as viewers look it up ([`Entries::present`]):
/// a null entry is absent, one that refers to no object stops the search and reads
/// as null.
#[derive(Clone, Default)]
struct Field<'a>([Option<Entries<'a>>; Inherited::ALL.len()]);

impl<'a> Field<'a> {
    /// The entries of `node`, a field or widget below the fields of `self`.
    fn below(&self, node: &Entries<'a>) -> Self {
        Self(std::array::from_fn(|i| {
            if node.present(Inherited::ALL[i].key()) {
                Some(node.clone())
            } else {
                self.0[i].clone()
            }
        }))
    }

    /// Whether some dictionary in the chain has `key`.
    fn found(&self, key: Inherited) -> bool {
        self.0[key as usize].is_some()
    }

    fn get(&self, key: Inherited) -> Value<'a> {
        self.0[key as usize]
            .as_ref()
            .map_or(Value::Null, |d| d.get(key.key()))
    }

    /// Where entry `key` is held ([`Entries::source`]): in the nearest dictionary
    /// that has it, or absent from all.
    fn source(&self, key: Inherited) -> Source<'a> {
        match &self.0[key as usize] {
            Some(d) => d.source(key.key()),
            None => Source::absent(key.key()),
        }
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

/// How far beyond its outline a stroke paints, in points on the page, across and
/// up: `MuPDF` paints a stroke up to about a tenth of a pixel beyond its edge, and
/// draws the thinnest strokes that wide on each side of their line (observed in
/// `PyMuPDF` 1.28.2 from 0.5 to 8 px/pt: a line 0.00004 pt wide 0.08 px below the
/// page greys its bottom row, 0.1 px below it does not), which at 0.4 px/pt or
/// finer is within a quarter of a point. A clip cuts it off where it lies. A
/// generator choosing the pieces that show counts those that come that close to
/// what the clips let show: whose outline meets it grown by this on every side.
const PAINTS_BEYOND: f64 = 0.25;

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
/// number read as 0; components after the fourth are not read.
fn color(items: &Items<'_>) -> Option<Color> {
    Color::of(&items.first(4))
}

/// The colour of array `key` of `dict`, `None` when it is not an array or transparent.
fn color_of(dict: &Entries<'_>, key: &[u8]) -> Option<Color> {
    color(&dict.items(key)?)
}

/// /C as a colour, `default` without one: when it is absent, empty, or not an
/// array, as viewers draw these annotations then.
fn c_color(annot: &Entries<'_>, default: Color) -> Color {
    color_of(annot, C).unwrap_or(default)
}

/// /BS /W, else the third /Border element, else 1, as given: viewers write a
/// negative width into the appearance as it is, and their renderer draws it.
fn border_width(annot: &Entries<'_>) -> f64 {
    annot
        .dict(BS)
        .and_then(|bs| bs.number(W))
        .or_else(|| annot.items(BORDER).and_then(|b| b.get(2).number()))
        .unwrap_or(1.0)
}

/// The dash array `/BS /D` when it is an array, whatever the border style `/S`,
/// else the fourth `/Border` element, each entry in its place: viewers write it
/// into the appearance as it is, however long, with 0 for an entry that is not a
/// number, and draw a pattern that is all zeros or has negative entries as their
/// renderer draws it.
fn dash(annot: &Entries<'_>) -> Vec<f64> {
    let items = annot
        .dict(BS)
        .and_then(|bs| bs.items(D))
        .or_else(|| annot.items(BORDER).and_then(|b| b.get(3).items()));
    items.map(|a| a.numbers()).unwrap_or_default()
}

/// `/Rect`, normalized, read as viewers read a damaged one: a coordinate it lacks
/// or that is not a number is 0. `None` when it is not an array.
fn rect(annot: &Entries<'_>) -> Option<kurbo::Rect> {
    let n = annot.numbers(RECT, 4)?;
    Some(kurbo::Rect::new(n[0], n[1], n[2], n[3]).abs())
}
