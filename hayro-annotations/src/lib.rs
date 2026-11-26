//! Annotation appearance selection, synthesis, device replay, and PDF serialization.
//!
//! Generated marks use default page coordinates. Apply the appearance opacity and blend
//! mode per paint when writing PDF and install each referenced glyph as a form `XObject`.

mod entries;
mod freetext;
mod icons;
mod index;
mod lines;
mod marks;
mod markup;
mod out;
mod rich;
mod text;
mod widget;

use crate::marks::Mark;
use entries::{Annots, Entries, Id, Items, Label, Listed, Objects, Source, StreamEntries, Value};
use out::{Color, Out, Part, Refused, written_all};
use text::{Align, Family, Fonts, Shaped, Size, Span, Style, StyleKey, Typeface};

use entries::Location;
use hayro_interpret::{AnnotationSelection, BlendMode, OcgState};
mod hash {
    pub(crate) use std::collections::{HashMap as Map, HashSet as Set};
}
use hayro_syntax::object::dict::keys::{AP, AS, BORDER, BS, C, CA, D, F, N, RECT, SUBTYPE, W};
use hayro_syntax::object::{Name, Stream};
use hayro_syntax::page::Page;
use hayro_syntax::xref::XRef;
use kurbo::Affine;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const MAX_VISIBLE_PIECES: f64 = 262_144.0;

/// What an annotation draws.
#[derive(Clone)]
pub enum Look<'a> {
    /// An existing form and its placement.
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
    /// The visible appearance could not be drawn.
    Unsupported(Unsupported),
}

/// An appearance form’s bounds, matrix, and placement on the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    bbox: [f32; 4],
    matrix: [f32; 6],
    transform: [f32; 6],
}

impl Placement {
    /// The bounding box in form coordinates.
    pub fn bbox(&self) -> [f32; 4] {
        self.bbox
    }

    /// The form matrix.
    pub fn matrix(&self) -> [f32; 6] {
        self.matrix
    }

    /// The transform from the matrix-adjusted form to default page coordinates.
    pub fn transform(&self) -> [f32; 6] {
        self.transform
    }
}

/// The reason a visible part of an appearance could not be generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// Its subtype, or its field type for a widget, has no generator.
    Kind,
    /// Its text has a character none of the fonts has.
    Character(char),
    /// The text requires bidirectional layout.
    Bidi(char),
    /// The geometry exceeds the supported range or precision.
    Coordinates,
    /// The appearance depends on a number without an established viewer reading.
    Number,
    /// The rich text uses unsupported structure or styling.
    RichText,
    /// The line ending has ambiguous geometry.
    Ending,
    /// The field value stream uses unsupported filters or contains corrupt data.
    ValueStream,
    /// Text measurement is too close to a layout boundary to place reliably.
    Measure,
}

/// The typed marks and graphics state of a generated appearance.
pub struct Appearance {
    marks: marks::Marks,
    /// The glyphs the content draws, each once.
    pub glyphs: Vec<Rc<Glyph>>,
    /// The opacity applied separately to each fill and stroke.
    pub opacity: f32,
    /// Multiply for highlights, Normal otherwise.
    pub blend: BlendMode,
}

/// A reusable filled glyph outline in a 1000-unit em.
#[derive(Debug)]
pub struct Glyph {
    name: Rc<str>,
    marks: marks::Marks,
    path: kurbo::BezPath,
    bbox: [f32; 4],
}

impl Glyph {
    fn new(name: String, path: &kurbo::BezPath, ink: kurbo::Rect) -> Option<Self> {
        let mut out = Out::default();
        out.path(path)?;
        out.op(Mark::FILL);
        let ((), content, _) = out.finish(Ok(())).ok()?;
        let bbox =
            written_all([ink.x0.floor(), ink.y0.floor(), ink.x1.ceil(), ink.y1.ceil()]).ok()?;
        Some(Self {
            name: name.into(),
            path: content.outline(),
            marks: content,
            bbox,
        })
    }

    /// The name the content draws it by.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Serialize the glyph outline as a filled PDF path.
    pub fn content(&self) -> String {
        self.marks.to_string()
    }

    /// The bounding box in form coordinates.
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
        self.annotation.subtype.as_deref()
    }
}

struct Annotation<'a> {
    look: Look<'a>,
    subtype: Option<Name<'a>>,
}

enum Plan<'a> {
    Fixed {
        annotation: Rc<Annotation<'a>>,
        placed: Option<Placement>,
    },
    Generate {
        id: Id<'a>,
        annot: Entries<'a>,
        subtype: Option<Name<'a>>,
        refused: Option<Unsupported>,
        again: Cell<bool>,
    },
}

#[derive(Clone)]
enum State<'a> {
    Stream(StreamEntries<'a>),
    Nothing,
    Absent,
}

enum Prepared<'a> {
    Nothing,
    Placed(Look<'a>, Option<Placement>),
    Generate(Option<Unsupported>),
}

type Plans<'a> = Rc<[(usize, Rc<Plan<'a>>)]>;

/// The visible page rectangle and the scale of a default user-space unit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    visible: kurbo::Rect,
    unit: f64,
}

impl View {
    /// Create a view with the given page rectangle and points per unit.
    pub fn new(visible: kurbo::Rect, unit: f64) -> Self {
        Self { visible, unit }
    }

    /// Use the rectangle and unit used by the Hayro renderer.
    pub fn of(page: &Page<'_>) -> Self {
        use hayro_interpret::RectExt;
        Self::new(page.intersected_crop_box().to_kurbo(), 1.0)
    }

    /// The area shown, in default user space.
    pub fn visible(&self) -> kurbo::Rect {
        self.visible
    }

    /// The points of one unit of default user space.
    pub fn unit(&self) -> f64 {
        self.unit
    }

    fn key(&self) -> ViewKey {
        let v = self.visible;
        [v.x0, v.y0, v.x1, v.y1, self.unit].map(f64::to_bits)
    }
}

type ViewKey = [u64; 5];

fn selected(annot: &Entries<'_>, selection: AnnotationSelection) -> bool {
    let flags = annot.get(F).flags();
    match selection {
        AnnotationSelection::View => flags & (2 | 32) == 0,
        AnnotationSelection::Print => flags & 2 == 0 && flags & 4 != 0,
        AnnotationSelection::Flatten => flags & 3 == 0 && !annot.is(SUBTYPE, b"Link"),
    }
}

/// Annotation preparation and glyph caches for one document and font resolver.
pub struct Appearances<'a> {
    xref: &'a XRef,
    visibility_loss: hayro_syntax::lost::Lost,
    visibility: RefCell<OcgState>,
    objects: Objects<'a>,
    fonts: Fonts,
    fields: RefCell<Known<'a>>,
}

#[derive(Default)]
struct Known<'a> {
    form: Option<Observed<Option<Entries<'a>>>>,
    plans: Kept<(Id<'a>, AnnotationSelection), Option<Rc<Plan<'a>>>>,
    generated: Kept<(Id<'a>, ViewKey), Option<Rc<Annotation<'a>>>>,
    lists: Kept<(Id<'a>, AnnotationSelection), Plans<'a>>,
    pages: Kept<(Id<'a>, AnnotationSelection, ViewKey), Rc<[Drawn<'a>]>>,
    states: Kept<(Location<'a>, Source<'a>), State<'a>>,
    fields: hash::Map<Location<'a>, Option<Field<'a>>>,
    shaped: Kept<TextKey<'a>, Result<Rc<Shaped>, Unsupported>>,
    stamps: Kept<Source<'a>, Result<Rc<icons::StampText>, Unsupported>>,
    styles: Kept<(Source<'a>, bool), Result<Style, Unsupported>>,
    labels: Kept<Source<'a>, Rc<Vec<Label<'a>>>>,
    appearances: Kept<Source<'a>, DefaultAppearance>,
    form_appearance: Option<Observed<DefaultAppearance>>,
    field_text: Kept<TextKey<'a>, Result<widget::FieldText, Unsupported>>,
}

type Kept<K, V> = hash::Map<K, Observed<V>>;

#[derive(Clone, Copy)]
struct Observed<T>(T, hayro_syntax::lost::Lost, bool);

impl<T> Observed<T> {
    fn read(xref: &XRef, make: impl FnOnce() -> T) -> Self {
        let ((value, invalid_number), lost) = xref.apart(|| entries::reading(make));
        Self(value, lost, invalid_number)
    }

    fn replay(self, xref: &XRef) -> T {
        xref.note(self.1);
        if self.2 {
            entries::unreadable_number();
        }
        self.0
    }
}

type DefaultAppearance = (Family, Option<f32>, Option<Color>);

#[derive(Clone, PartialEq, Eq, Hash)]
struct TextKey<'a> {
    sources: Vec<Source<'a>>,
    face: Typeface,
    style: Option<StyleKey>,
    numbers: Vec<u64>,
}

const TEXT: u64 = 0;
const CAPTION: u64 = 1;
const FREE_TEXT: u64 = 2;
const FIELD: u64 = 3;
const OPTION: u64 = 4;

impl<'a> TextKey<'a> {
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

    fn styled(self, style: &Style) -> Self {
        Self {
            style: Some(style.key()),
            ..self
        }
    }
}

impl<'a> Appearances<'a> {
    /// Create the annotation cache for this document.
    pub fn new(xref: &'a XRef, font_resolver: hayro_font::FontResolverFn) -> Self {
        let (visibility, visibility_loss) = xref.watch(|| {
            let catalog = xref.get(xref.root_id()).unwrap_or_default();
            OcgState::from_catalog(xref, &catalog)
        });
        Self {
            xref,
            visibility_loss,
            visibility: RefCell::new(visibility),
            objects: Objects::new(xref),
            fonts: Fonts::new(font_resolver),
            fields: RefCell::default(),
        }
    }

    fn form(&self) -> Option<Entries<'a>> {
        let known = self.fields.borrow().form.clone();
        known
            .unwrap_or_else(|| {
                let made = Observed::read(self.xref, || {
                    self.objects
                        .catalog()
                        .and_then(|root| root.dict(b"AcroForm"))
                });
                self.fields.borrow_mut().form = Some(made.clone());
                made
            })
            .replay(self.xref)
    }

    /// Return the selected appearances on a page of this document, in annotation order.
    pub fn page(
        &self,
        page: &Page<'a>,
        selection: AnnotationSelection,
        view: &View,
    ) -> Rc<[Drawn<'a>]> {
        self.xref.note(self.visibility_loss);
        let Some(annots) = self.objects.annots(page.raw()) else {
            return Rc::from([]);
        };
        let View { visible, unit } = *view;
        let view = view.key();
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
                self.drawn(&plans, visible, unit, view).into()
            },
        )
    }

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

    fn plan(&self, listed: &Listed<'a>, selection: AnnotationSelection) -> Option<Rc<Plan<'a>>> {
        let annot = self.objects.annotation(listed)?;
        let subtype = annot.name(SUBTYPE);
        let fixed = |look, placed| {
            let annotation = Rc::new(Annotation {
                look,
                subtype: subtype.clone(),
            });
            Some(Rc::new(Plan::Fixed { annotation, placed }))
        };
        let unselected = match entries::read(|| selected(&annot, selection)) {
            Ok(true) => None,
            Ok(false) => return None,
            Err(why) => Some(why),
        };
        if !annot.shown_in(&mut self.visibility.borrow_mut()) {
            return None;
        }
        match entries::read(|| self.prepare(&annot)) {
            Err(why) => fixed(Look::Unsupported(why), None),
            Ok(Prepared::Nothing | Prepared::Placed(Look::Nothing, _)) => None,
            Ok(Prepared::Placed(look, placed)) => {
                fixed(unselected.map_or(look, Look::Unsupported), placed)
            }
            Ok(Prepared::Generate(refused)) => Some(Rc::new(Plan::Generate {
                id: listed.id,
                annot,
                subtype,
                refused: unselected.or(refused),
                again: Cell::new(false),
            })),
        }
    }

    fn drawn(
        &self,
        plans: &[(usize, Rc<Plan<'a>>)],
        visible: kurbo::Rect,
        unit: f64,
        view: ViewKey,
    ) -> Vec<Drawn<'a>> {
        // What the page shows, grown by how far a stroke paints beyond it.
        let region = Out::new(visible, unit).region();
        plans
            .iter()
            .filter_map(|(index, plan)| {
                let annotation = match &**plan {
                    Plan::Fixed { annotation, placed } => {
                        if placed.is_some_and(|p| !region.may_show_form(&p)) {
                            return None;
                        }
                        annotation.clone()
                    }
                    Plan::Generate { id, .. } => self.once(
                        |k| &mut k.generated,
                        Some((*id, view)),
                        || self.generated(plan, visible, unit),
                    )?,
                };
                Some(Drawn {
                    index: *index,
                    annotation,
                })
            })
            .collect()
    }

    fn generated(
        &self,
        plan: &Plan<'a>,
        visible: kurbo::Rect,
        unit: f64,
    ) -> Option<Rc<Annotation<'a>>> {
        let Plan::Generate {
            annot,
            subtype,
            refused,
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
        let look = match self.generate(*refused, &annot, visible, unit) {
            Ok(Some(appearance)) => Look::Generated(Rc::new(appearance)),
            Ok(None) => return None,
            Err(why) => Look::Unsupported(why),
        };
        Some(Rc::new(Annotation {
            look,
            subtype: subtype.clone(),
        }))
    }

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
                        State::Nothing => return Prepared::Nothing,
                        State::Absent => None,
                    },
                    _ => None,
                };
                if let Some(stream) = stream {
                    let (placed, read) = placement(&stream, annot);
                    let look = match placed {
                        Ok(Some(placement)) => Look::Stream {
                            stream: stream.into_stream(),
                            placement,
                        },
                        Ok(None) => Look::Nothing,
                        Err(why) => Look::Unsupported(why),
                    };
                    return Prepared::Placed(look, read);
                }
            }
            _ => return self.not_a_dictionary(annot),
        }
        self.to_generate(annot)
    }

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
            .map(|s| (states.location(), s));
        self.once(|k| &mut k.states, key, find)
    }

    fn not_a_dictionary(&self, annot: &Entries<'a>) -> Prepared<'a> {
        let button = annot.is(SUBTYPE, b"Widget")
            && self
                .field(annot)
                .is_some_and(|f| f.get(Inherited::Type).name() == Some(b"Btn"));
        if button {
            self.to_generate(annot)
        } else {
            Prepared::Nothing
        }
    }

    fn to_generate(&self, annot: &Entries<'a>) -> Prepared<'a> {
        match annot.own() {
            Some(true) => Prepared::Generate(None),
            Some(false) => Prepared::Nothing,
            None => Prepared::Generate(Some(Unsupported::Kind)),
        }
    }
}

fn placement(
    stream: &StreamEntries<'_>,
    annot: &Entries<'_>,
) -> (Result<Option<Placement>, Unsupported>, Option<Placement>) {
    let Some((bbox, matrix)) = stream.dict().box_and_matrix() else {
        return (Ok(None), None);
    };
    let read = rect(annot).map(|rect| {
        let (bbox, matrix) = (bbox.map(|v| v as f32), matrix.map(|v| v as f32));
        let transform = rect_transform(rect, bbox, Affine::new(matrix.map(f64::from)));
        Placement {
            bbox,
            matrix,
            transform: transform.as_coeffs().map(|v| v as f32),
        }
    });
    (written_placement(bbox, matrix, annot), read)
}

fn written_placement(
    bbox: [f64; 4],
    matrix: [f64; 6],
    annot: &Entries<'_>,
) -> Result<Option<Placement>, Unsupported> {
    let bbox = written_all(bbox)?;
    let matrix = written_all(matrix)?;
    let affine = Affine::new(matrix.map(f64::from));
    let [x0, y0, x1, y1] = bbox.map(f64::from);
    let transformed = affine.transform_rect_bbox(kurbo::Rect::new(x0, y0, x1, y1));
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

/// Compute the PDF transform that places an appearance in an annotation rectangle.
pub fn rect_transform(rect: kurbo::Rect, bbox: [f32; 4], matrix: Affine) -> Affine {
    let [x0, y0, x1, y1] = bbox.map(f64::from);
    let transformed = matrix.transform_rect_bbox(kurbo::Rect::new(x0, y0, x1, y1));
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

type Generated = Result<Option<Appearance>, Unsupported>;

type Drew = Result<Option<()>, Refused>;

impl<'a> Appearances<'a> {
    fn generate(
        &self,
        refused: Option<Unsupported>,
        annot: &Entries<'a>,
        visible: kurbo::Rect,
        unit: f64,
    ) -> Generated {
        let Some(subtype) = annot.name(SUBTYPE) else {
            return Ok(None);
        };
        let mut out = Out::new(visible, unit);
        let (whole, refused) = match entries::read(|| whole(&subtype, annot)) {
            Err(why) => (Part::Anywhere, Some(why)),
            Ok(None) => return Ok(None),
            Ok(Some(whole)) => (whole, refused),
        };
        if !out.shows(&whole) {
            return Ok(None);
        }
        let drew = match refused {
            Some(why) => out.refuse(why, whole).map(|()| None),
            None => {
                let (drew, invalid) =
                    entries::reading(|| self.generated_anyway(annot, &subtype, visible, &mut out));
                if invalid {
                    drew.and_then(|_| out.refuse(Unsupported::Number, whole).map(|()| None))
                } else {
                    drew
                }
            }
        };
        // A number that could not be written fails the appearance, whatever the
        // generator made of it: `None` from a generator means nothing to draw.
        let (drew, content, glyphs) = out.finish(drew)?;
        if drew.is_none() || !content.paints() {
            return Ok(None);
        }
        let blend = if *subtype == *b"Highlight" {
            BlendMode::Multiply
        } else {
            BlendMode::Normal
        };
        // A widget is drawn opaque whatever its /CA says (observed in `PyMuPDF`
        // 1.28.2 bake: text, choice and button fields alike, /CA 0 or 0.5).
        let opacity = if *subtype == *b"Widget" {
            1.0
        } else {
            entries::read(|| opacity(annot))?
        };
        Ok(Some(Appearance {
            marks: content,
            glyphs,
            opacity,
            blend,
        }))
    }

    fn generated_anyway(
        &self,
        annot: &Entries<'a>,
        subtype: &Name<'_>,
        visible: kurbo::Rect,
        out: &mut Out,
    ) -> Drew {
        let fonts = &self.fonts;
        let ok = |drew: Option<()>| Ok(drew);
        match &**subtype {
            b"Square" => ok(markup::shape(annot, false, out)),
            b"Circle" => ok(markup::shape(annot, true, out)),
            b"Highlight" => ok(markup::highlight(annot, out)),
            b"Underline" => ok(markup::text_lines(annot, markup::Line::Under, out)),
            b"StrikeOut" => ok(markup::text_lines(annot, markup::Line::Through, out)),
            b"Squiggly" => markup::squiggly(annot, out),
            b"Ink" => ok(markup::ink(annot, out)),
            b"Text" => icons::icon(annot, icons::Icon::Note, fonts, out),
            b"FileAttachment" => icons::icon(annot, icons::Icon::FileAttachment, fonts, out),
            b"Sound" => icons::icon(annot, icons::Icon::Sound, fonts, out),
            b"Caret" => ok(icons::caret(annot, out)),
            b"Redact" => ok(icons::redact(annot, out)),
            b"Stamp" => icons::stamp(annot, self, visible, out),
            b"FreeText" => freetext::free_text(annot, self, visible, out),
            b"Line" => lines::line(annot, self, visible, out),
            b"Polygon" => lines::poly(annot, true, out),
            b"PolyLine" => lines::poly(annot, false, out),
            b"Widget" => match self.field(annot) {
                Some(field) => widget::widget(annot, &field, self, visible, out),
                // A widget whose /Parent chain loops is a broken field: nothing.
                None => Ok(None),
            },
            // Popup, Link, the media and 3D subtypes, PrinterMark, TrapNet, Watermark,
            // Projection, stamps of other names and unknown subtypes: nothing.
            _ => Ok(None),
        }
    }

    fn once<K: Eq + std::hash::Hash, V: Clone>(
        &self,
        map: for<'k> fn(&'k mut Known<'a>) -> &'k mut Kept<K, V>,
        key: Option<K>,
        make: impl FnOnce() -> V,
    ) -> V {
        let Some(key) = key else {
            return make();
        };
        let known = map(&mut self.fields.borrow_mut()).get(&key).cloned();
        if let Some(made) = known {
            return made.replay(self.xref);
        }
        // Made without the map borrowed: making may look other things up.
        let made = Observed::read(self.xref, make);
        map(&mut self.fields.borrow_mut()).insert(key, made.clone());
        made.replay(self.xref)
    }

    fn shaped(
        &self,
        key: TextKey<'a>,
        make: impl FnOnce() -> Result<Shaped, Unsupported>,
    ) -> Result<Rc<Shaped>, Unsupported> {
        self.once(|k| &mut k.shaped, Some(key), || make().map(Rc::new))
    }

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

    fn stamp_text(&self, annot: &Entries<'a>) -> Result<Rc<icons::StampText>, Unsupported> {
        self.once(
            |k| &mut k.stamps,
            Some(annot.source(b"Name")),
            || icons::stamp_text(annot, &self.fonts).map(Rc::new),
        )
    }

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

    fn default_appearance(
        &self,
        source: Option<Source<'a>>,
        read: impl FnOnce() -> Option<Vec<u8>>,
    ) -> DefaultAppearance {
        let make = || text::default_appearance(&read().unwrap_or_default());
        if source.is_none() {
            let known = self.fields.borrow().form_appearance;
            known
                .unwrap_or_else(|| {
                    let made = Observed::read(self.xref, make);
                    self.fields.borrow_mut().form_appearance = Some(made);
                    made
                })
                .replay(self.xref)
        } else {
            self.once(|k| &mut k.appearances, source, make)
        }
    }

    fn field(&self, annot: &Entries<'a>) -> Option<Field<'a>> {
        let mut on_walk = hash::Set::new();
        on_walk.insert(annot.location());
        let mut walk: Vec<(Option<Location<'a>>, Entries<'a>)> = vec![(None, annot.clone())];
        let (mut field, walk_loss) = self.xref.watch(|| {
            loop {
                let node = &walk[walk.len() - 1].1;
                let Some(parent) = node.parent() else {
                    break Some(Field::default());
                };
                let id = parent.location();
                let known = self.fields.borrow().fields.get(&id).cloned();
                if let Some(field) = known {
                    break field;
                }
                if !on_walk.insert(id) {
                    break None;
                }
                walk.push((Some(id), parent));
            }
        });
        let (fields, inherit_loss) = self.xref.watch(|| {
            let mut fields = Vec::new();
            for (id, node) in walk.into_iter().rev() {
                field = field.clone().map(|above| above.below(&node));
                fields.extend(id.map(|id| (id, field.clone())));
            }
            fields
        });
        if walk_loss.is_some() || inherit_loss.is_some() {
            return field;
        }
        let known = &mut self.fields.borrow_mut().fields;
        for (id, field) in fields {
            known.insert(id, field);
        }
        field
    }
}

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

#[derive(Clone, Default)]
struct Field<'a>([Option<Entries<'a>>; Inherited::ALL.len()]);

impl<'a> Field<'a> {
    fn below(&self, node: &Entries<'a>) -> Self {
        Self(std::array::from_fn(|i| {
            if node.present(Inherited::ALL[i].key()) {
                Some(node.clone())
            } else {
                self.0[i].clone()
            }
        }))
    }

    fn found(&self, key: Inherited) -> bool {
        self.0[key as usize].is_some()
    }

    fn get(&self, key: Inherited) -> Value<'a> {
        self.0[key as usize]
            .as_ref()
            .map_or(Value::Null, |d| d.get(key.key()))
    }

    fn source(&self, key: Inherited) -> Source<'a> {
        match &self.0[key as usize] {
            Some(d) => d.source(key.key()),
            None => Source::absent(key.key()),
        }
    }
}

fn whole(subtype: &[u8], annot: &Entries<'_>) -> Option<Part> {
    // A widget is drawn opaque whatever its /CA says ([`Appearances::generate`]).
    if subtype != b"Widget" && opacity(annot) == 0.0 {
        return None;
    }
    let Some((points, reach)) = extent(subtype, annot) else {
        return Some(Part::Anywhere);
    };
    // Each point computed from a few numbers read, in 32-bit numbers as viewers
    // compute it ([`out::Rounding`]).
    let most = points
        .iter()
        .fold(reach.abs(), |m, p| m.max(p.x.abs()).max(p.y.abs()));
    let local = out::Rounding::point(
        out::Rounding::of(EXTENT_ROUNDINGS, most),
        out::Rounding::of(EXTENT_ROUNDINGS, most),
    );
    Some(Part::near(points, reach).computed(local))
}

const EXTENT_ROUNDINGS: f64 = 16.0;

fn extent(subtype: &[u8], annot: &Entries<'_>) -> Option<(Vec<kurbo::Point>, f64)> {
    let around = |(r, reach): (kurbo::Rect, f64)| {
        let points = [(r.x0, r.y0), (r.x1, r.y0), (r.x1, r.y1), (r.x0, r.y1)];
        (points.map(|(x, y)| kurbo::Point::new(x, y)).to_vec(), reach)
    };
    let corners = |r: kurbo::Rect| around((r, 0.0));
    Some(match subtype {
        b"Square" | b"Circle" => corners(markup::shape_box(annot)?.1),
        b"Highlight" => corners(markup::highlights(annot)?.2),
        b"Underline" | b"StrikeOut" | b"Squiggly" => corners(markup::marked(annot)?.2),
        b"Ink" => around(markup::ink_extent(annot)?),
        b"Text" | b"FileAttachment" | b"Sound" => {
            let (corner, s) = icons::icon_square(annot)?;
            // Framed by a stroke 1.25 wide at 20 pt.
            let square = kurbo::Rect::from_origin_size(corner, (s, s));
            around((square, out::stroke_reach(1.25 * s / 20.0)))
        }
        b"Caret" => corners(icons::caret_box(annot)?),
        b"Redact" => corners(icons::redact_clip(annot)?),
        b"Stamp" | b"Widget" => corners(rect(annot)?),
        b"FreeText" => corners(freetext::boxes(annot)?.clip),
        b"Line" => lines::line_extent(annot),
        b"Polygon" => around(lines::poly_extent(annot, true)?),
        b"PolyLine" => around(lines::poly_extent(annot, false)?),
        _ => return None,
    })
}

fn opacity(annot: &Entries<'_>) -> f32 {
    let opacity = annot.number(CA).unwrap_or(1.0);
    if opacity.is_finite() {
        opacity.clamp(0.0, 1.0) as f32
    } else {
        1.0
    }
}

const PAINTS_BEYOND: f64 = 0.25;

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

fn color(items: &Items<'_>) -> Option<Color> {
    Color::of(&items.first(4))
}

fn color_of(dict: &Entries<'_>, key: &[u8]) -> Option<Color> {
    color(&dict.items(key)?)
}

fn c_color(annot: &Entries<'_>, default: Color) -> Color {
    color_of(annot, C).unwrap_or(default)
}

fn border_width(annot: &Entries<'_>) -> f64 {
    annot
        .dict(BS)
        .and_then(|bs| bs.number(W))
        .or_else(|| annot.items(BORDER).and_then(|b| b.get(2).number()))
        .unwrap_or(1.0)
}

fn dash(annot: &Entries<'_>) -> Option<Rc<out::Dash>> {
    let items = annot
        .dict(BS)
        .and_then(|bs| bs.items(D))
        .or_else(|| annot.items(BORDER).and_then(|b| b.get(3).items()));
    items.map(|a| a.derived::<out::Dash>())
}

impl entries::Derived for out::Dash {
    fn derive(items: &Items<'_>) -> Self {
        Self::of(&items.numbers())
    }
}

fn rect(annot: &Entries<'_>) -> Option<kurbo::Rect> {
    annot.rect(RECT)
}

#[cfg(test)]
mod tests {
    use super::entries::Entries;
    use super::{Appearances, Out, SUBTYPE, extent};
    use hayro_syntax::Pdf;
    use hayro_syntax::object::{Dict, FromBytes};
    use kurbo::Rect;

    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn num(&mut self, lo: f64, hi: f64) -> f64 {
            let t = (self.next() % 1_000_001) as f64 / 1_000_000.0;
            ((lo + t * (hi - lo)) * 100.0).round() / 100.0
        }

        fn pick<'s>(&mut self, of: &[&'s str]) -> &'s str {
            of[(self.next() % of.len() as u64) as usize]
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.next() % 100 < percent
        }

        fn nums(&mut self, n: usize, lo: f64, hi: f64) -> String {
            (0..n)
                .map(|_| self.num(lo, hi).to_string())
                .collect::<Vec<_>>()
                .join(" ")
        }

        fn rect(&mut self) -> String {
            let (x, y) = (self.num(-40.0, 160.0), self.num(-40.0, 160.0));
            let (w, h) = (self.num(0.5, 140.0), self.num(0.5, 140.0));
            format!("[{x} {y} {} {}]", x + w, y + h)
        }

        fn width(&mut self) -> String {
            let w = self.pick(&["0", "0.5", "1", "3", "12", "-4", "40"]);
            format!("/BS << /W {w} >>")
        }

        fn ending(&mut self) -> &'static str {
            self.pick(&[
                "/None",
                "/Square",
                "/Circle",
                "/Diamond",
                "/OpenArrow",
                "/ClosedArrow",
                "/Butt",
                "/ROpenArrow",
                "/RClosedArrow",
                "/Slash",
            ])
        }

        fn quads(&mut self) -> String {
            let mut q = vec![];
            for _ in 0..1 + self.next() % 3 {
                let (x, y) = (self.num(-40.0, 160.0), self.num(-40.0, 160.0));
                let (len, h, turn) = (
                    self.num(1.0, 150.0),
                    self.num(0.0, 40.0),
                    self.num(-3.2, 3.2),
                );
                let (c, s) = (turn.cos(), turn.sin());
                let at = |u: f64, v: f64| format!("{} {}", x + u * c - v * s, y + u * s + v * c);
                q.push(format!(
                    "{} {} {} {}",
                    at(0.0, h),
                    at(len, h),
                    at(0.0, 0.0),
                    at(len, 0.0)
                ));
            }
            format!("[{}]", q.join(" "))
        }

        fn words(&mut self) -> String {
            let n = 1 + self.next() % 12;
            (0..n)
                .map(|_| self.pick(&["A", "Wide", "text", "MMMMMM", "a b", "\\(x\\)"]))
                .collect::<Vec<_>>()
                .join(" ")
        }
    }

    fn annotation(r: &mut Random, subtype: &str) -> String {
        let color = |r: &mut Random, key: &str| {
            if r.chance(80) {
                format!(
                    "/{key} [{}]",
                    r.pick(&["1 0 0", "0", "0 0 1 0", "0.2 0.5 0.9"])
                )
            } else {
                String::new()
            }
        };
        let rest = match subtype {
            "Square" | "Circle" => format!(
                "/Rect {} /RD [{}] {} {} {}",
                r.rect(),
                r.nums(4, -10.0, 30.0),
                r.width(),
                color(r, "C"),
                color(r, "IC")
            ),
            "Highlight" | "Underline" | "StrikeOut" | "Squiggly" | "Redact" => format!(
                "/Rect {} /QuadPoints {} {}",
                r.rect(),
                r.quads(),
                color(r, "C")
            ),
            "Ink" => {
                let paths: Vec<String> = (0..1 + r.next() % 3)
                    .map(|_| {
                        let n = 2 * (1 + r.next() % 6) as usize;
                        format!("[{}]", r.nums(n, -40.0, 240.0))
                    })
                    .collect();
                format!(
                    "/Rect [0 0 1 1] /InkList [{}] {} /C [0 0 1]",
                    paths.join(" "),
                    r.width()
                )
            }
            "Text" | "FileAttachment" | "Sound" => {
                let (x, y) = (r.num(-40.0, 160.0), r.num(-40.0, 160.0));
                let s = r.num(1.0, 40.0);
                let name = r.pick(&[
                    "/Help", "/Comment", "/Key", "/PushPin", "/Mic", "/Other", "",
                ]);
                format!(
                    "/Rect [{x} {y} {} {}] /Name {name} {}",
                    x + s,
                    y + r.num(1.0, 40.0),
                    color(r, "C")
                )
            }
            "Caret" | "Stamp" => {
                let name = r.pick(&["/Draft", "/NotForPublicRelease", "/Custom", "(x)"]);
                format!("/Rect {} /Name {name} {}", r.rect(), color(r, "C"))
            }
            "FreeText" => {
                let text = r.words();
                let body = if r.chance(40) {
                    format!(
                        "/RC (<body style=\\\"font-size:{}pt\\\"><p>{text}</p></body>)",
                        r.pick(&["9", "14", "30"])
                    )
                } else {
                    format!("/Contents ({text})")
                };
                let callout = if r.chance(50) {
                    let n = if r.chance(50) { 4 } else { 6 };
                    format!(
                        "/IT /FreeTextCallout /CL [{}] /LE {}",
                        r.nums(n, -60.0, 260.0),
                        r.ending()
                    )
                } else {
                    String::new()
                };
                format!(
                    "/Rect {} /RD [{}] /DA (/Helv {} Tf 0 g) {body} {callout} {} {} /Rotate {}",
                    r.rect(),
                    r.nums(4, -5.0, 20.0),
                    r.pick(&["0", "8", "12", "40"]),
                    r.width(),
                    color(r, "C"),
                    r.pick(&["0", "90", "180", "33"])
                )
            }
            "Line" => {
                let leader = if r.chance(50) {
                    format!(
                        "/LL {} /LLE {} /LLO {}",
                        r.num(-40.0, 40.0),
                        r.num(-10.0, 20.0),
                        r.num(-10.0, 20.0)
                    )
                } else {
                    String::new()
                };
                let caption = if r.chance(60) {
                    format!(
                        "/Cap true /Contents ({}) {} {}",
                        r.words(),
                        if r.chance(30) { "/CP /Top" } else { "" },
                        if r.chance(40) {
                            format!("/CO [{}]", r.nums(2, -30.0, 30.0))
                        } else {
                            String::new()
                        }
                    )
                } else {
                    String::new()
                };
                format!(
                    "/Rect [0 0 1 1] /L [{}] {leader} /LE [{} {}] {} {} {} {caption}",
                    r.nums(4, -40.0, 240.0),
                    r.ending(),
                    r.ending(),
                    r.width(),
                    color(r, "C"),
                    color(r, "IC")
                )
            }
            "Polygon" | "PolyLine" => {
                let n = 2 * (2 + r.next() % 5) as usize;
                format!(
                    "/Rect [0 0 1 1] /Vertices [{}] /LE [{} {}] {} {} {}",
                    r.nums(n, -40.0, 240.0),
                    r.ending(),
                    r.ending(),
                    r.width(),
                    color(r, "C"),
                    color(r, "IC")
                )
            }
            _ => {
                let field = r.pick(&[
                    "/FT /Tx /V (Some text here)",
                    "/FT /Tx /Ff 4096 /V (Many words of a long value to wrap)",
                    "/FT /Btn /AS /Yes",
                    "/FT /Btn /Ff 65536 /MK << /CA (Push) >>",
                    "/FT /Ch /Ff 131072 /V (Pick) /Opt [(Pick) (Other)]",
                ]);
                format!(
                    "/Rect {} {field} /DA (/Helv {} Tf 0 g) /MK << /BG [0.9] /BC [0 0 1] /R {} >> {}",
                    r.rect(),
                    r.pick(&["0", "9", "24"]),
                    r.pick(&["0", "90", "270", "45"]),
                    r.width()
                )
            }
        };
        format!("<< /Type /Annot /Subtype /{subtype} {rest} /F 4 >>")
    }

    fn empty() -> Pdf {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>",
        ];
        let mut data = b"%PDF-1.7\n".to_vec();
        let mut offsets = vec![];
        for (i, o) in objects.iter().enumerate() {
            offsets.push(data.len());
            data.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
        }
        let xref = data.len();
        data.extend(b"xref\n0 4\n0000000000 65535 f \n");
        for o in offsets {
            data.extend(format!("{o:010} 00000 n \n").bytes());
        }
        data.extend(
            format!("trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").bytes(),
        );
        Pdf::new(data).expect("valid pdf")
    }

    #[test]
    fn no_annotation_paints_beyond_its_extent() {
        let pdf = empty();
        let doc = Appearances::new(
            pdf.xref(),
            hayro_interpret::InterpreterSettings::default().font_resolver,
        );
        let mut r = Random(0x9e37_79b9_7f4a_7c15);
        let subtypes = [
            "Square",
            "Circle",
            "Highlight",
            "Underline",
            "StrikeOut",
            "Squiggly",
            "Ink",
            "Text",
            "FileAttachment",
            "Sound",
            "Caret",
            "Redact",
            "Stamp",
            "FreeText",
            "Line",
            "Polygon",
            "PolyLine",
            "Widget",
        ];
        for subtype in subtypes {
            let mut painted = 0;
            const CASES: usize = 300;
            for _ in 0..CASES {
                // As long as the document's appearances, which hold what they read.
                let entries: &str = Box::leak(annotation(&mut r, subtype).into_boxed_str());
                let dict = Dict::from_bytes(entries.as_bytes()).expect("valid dict");
                let annot = Entries::detached(dict);
                let name = annot.name(SUBTYPE).expect("a subtype");
                let (points, reach) = extent(&name, &annot).expect("an extent");
                let b = points
                    .iter()
                    .fold(Rect::from_points(points[0], points[0]), |b, p| {
                        b.union_pt(*p)
                    })
                    .inflate(reach, reach);
                let size = [b.x0, b.y0, b.x1, b.y1]
                    .iter()
                    .fold(1.0_f64, |m, v| m.max(v.abs()));
                // Beyond what [`Out`] grows a piece by, and what a viewer moves it by.
                let m = 1.0 + size * 1e-5;
                let far = 1000.0 + size;
                let beyond = [
                    Rect::new(b.x0 - far, b.y0 - far, b.x0 - m, b.y1 + far),
                    Rect::new(b.x1 + m, b.y0 - far, b.x1 + far, b.y1 + far),
                    Rect::new(b.x0 - far, b.y0 - far, b.x1 + far, b.y0 - m),
                    Rect::new(b.x0 - far, b.y1 + m, b.x1 + far, b.y1 + far),
                ];
                // What the generator paints in `view`, gate or no gate: nothing,
                // and refuses nothing, beyond the extent.
                let anyway = |view: Rect| {
                    let mut out = Out::new(view, 1.0);
                    let drew = doc.generated_anyway(&annot, &name, view, &mut out);
                    out.finish(drew)
                        .map(|(drew, content, _)| drew.is_some() && content.paints())
                };
                for view in beyond {
                    let drawn = anyway(view);
                    assert!(
                        matches!(drawn, Ok(false)),
                        "{entries} seen in {view:?}: {drawn:?}"
                    );
                }
                let whole = b.inflate(far, far);
                let part = super::whole(&name, &annot);
                assert!(
                    part.is_some_and(|part| Out::new(whole, 1.0).shows(&part))
                        || annot.number(super::CA) == Some(0.0)
                );
                if let Ok(true) = anyway(whole) {
                    painted += 1;
                }
            }
            assert!(painted > CASES / 3, "{subtype}: {painted} of {CASES} paint");
        }
    }
}
