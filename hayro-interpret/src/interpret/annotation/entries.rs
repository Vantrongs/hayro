//! The entries of annotation dictionaries, and the boxes and matrices of the forms,
//! patterns and shadings they and pages draw, read as viewers read damaged ones.
//!
//! Generators read an annotation only through [`Entries`], which offers no typed read
//! that drops an array's entry: every entry keeps its place, and one that is not a
//! number, is null, or refers to no object reads as 0 where a number is wanted, so the
//! coordinates and colour components after it keep their meaning.
//!
//! Numbers are read as `MuPDF` reads them, which is what the files are checked against:
//! from the digits as written ([`Number::viewer_f32`]), an integer entry of a real
//! rounded half up in 32-bit floats, and `i32::MIN` beyond 32 bits. A number whose
//! reading is not established makes [`read`] fail with [`Unsupported::Number`].

use super::Unsupported;
use hayro_syntax::Filter;
use hayro_syntax::hash;
use hayro_syntax::lost::Lost;
use hayro_syntax::object::{Array, Dict, MaybeRef, Name, Number, ObjRef, Object, Stream};
use hayro_syntax::xref::{ContentKey, XRef};
use smallvec::SmallVec;
use std::any::{Any, TypeId};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// The first entries of an array read as numbers ([`Items::first`]): those of a
/// box, a matrix or a colour, held without an allocation.
pub(crate) type Numbers = SmallVec<[f64; 8]>;

thread_local! {
    /// Whether a number that cannot be read as `MuPDF` reads it was read since
    /// [`read`] began.
    static UNREADABLE: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f`, which reads entries; [`Unsupported::Number`] when it read a number
/// that cannot be read as `MuPDF` reads it, whatever `f` made of it.
pub(crate) fn read<T>(f: impl FnOnce() -> T) -> Result<T, Unsupported> {
    match reading(f) {
        (_, true) => Err(Unsupported::Number),
        (value, false) => Ok(value),
    }
}

/// Runs `f`, which reads entries, and tells whether it read a number that cannot be
/// read as `MuPDF` reads it, for the caller to tell where that may matter.
pub(crate) fn reading<T>(f: impl FnOnce() -> T) -> (T, bool) {
    let before = UNREADABLE.with(|u| u.replace(false));
    let value = f();
    let unreadable = UNREADABLE.with(|u| u.replace(before));
    (value, unreadable)
}

/// Runs `f`, which reads entries, and tells whether it read a number that cannot be
/// read as `MuPDF` reads it, for what is made of entries once and kept: each later
/// use of it calls [`unreadable`] when it did.
pub(crate) fn noting<T>(f: impl FnOnce() -> T) -> (T, bool) {
    let before = UNREADABLE.with(|u| u.replace(false));
    let value = f();
    let unreadable = UNREADABLE.with(|u| u.replace(before));
    if unreadable {
        UNREADABLE.with(|u| u.set(true));
    }
    (value, unreadable)
}

/// Notes that a number that cannot be read as `MuPDF` reads it was read, for
/// [`read`] to fail.
pub(crate) fn unreadable() {
    UNREADABLE.with(|u| u.set(true));
}

/// `n` as `MuPDF` reads a number where a real is wanted (see the module).
pub(crate) fn real(n: Number) -> f64 {
    super::work(1);
    match n.viewer_f32() {
        Some(v) => f64::from(v),
        None => {
            UNREADABLE.with(|u| u.set(true));
            0.0
        }
    }
}

/// `n` as `MuPDF` reads a number in a string of operators, such as a default
/// appearance: there a real is read as its value, so `4294967296.5` is past
/// 4 billion rather than 0.5, and one beyond 32-bit floats is not read as either
/// (observed in `PyMuPDF` 1.28.2 bake). Where that differs from [`real`], which
/// alone is established digit for digit, the number is not read.
pub(crate) fn operand_real(n: Number) -> f64 {
    let v = real(n);
    if !n.is_integer() && n.as_f64() as f32 != v as f32 {
        unreadable();
    }
    v
}

/// `n` as `MuPDF` reads a number where an integer is wanted (see the module).
fn int(n: Number) -> i32 {
    if n.is_integer() {
        // The low 32 bits.
        return n.as_i64() as i32;
    }
    let rounded = (real(n) as f32 + 0.5).floor();
    if (-2_147_483_648.0..2_147_483_648.0).contains(&rounded) {
        rounded as i32
    } else {
        i32::MIN
    }
}

/// A dictionary an annotation is drawn from: the annotation, one it refers to (its
/// border style, its appearance characteristics, an appearance stream's), or a field
/// above a widget.
#[derive(Clone)]
pub(crate) struct Entries<'a>(Dict<'a>, Objects<'a>);

/// An entry that holds text: a string. A stream there is no text, as if the entry
/// were absent (observed in `PyMuPDF` bake); only a field's value is read from a
/// stream too ([`Value::field_text`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Text {
    /// `/Contents`.
    Contents,
    /// `/DA`, of an annotation, a field or the form.
    DefaultAppearance,
    /// `/RC`.
    Rich,
    /// `/DS`.
    DefaultStyle,
    /// `/CA` of `/MK`, a push button's caption.
    Caption,
    /// `/RV` of a rich text field.
    RichValue,
}

impl Text {
    pub(crate) const fn key(self) -> &'static [u8] {
        match self {
            Self::Contents => b"Contents",
            Self::DefaultAppearance => b"DA",
            Self::Rich => b"RC",
            Self::DefaultStyle => b"DS",
            Self::Caption => b"CA",
            Self::RichValue => b"RV",
        }
    }
}

/// Entry `key` as a key for what a document makes of its value once: the bytes the
/// value is read from ([`Held`]), never the value itself, and the entry's key, so
/// that two entries with the same source have the same value and are read alike,
/// and telling them apart costs nothing however long the value. Two entries that
/// lead to one value by different ways, through entries of the table that lead to
/// one body, or through the same dictionary read again, have one source; two values
/// have two, wherever they lie.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Source<'a> {
    held: Held<'a>,
    key: &'static [u8],
}

impl Source<'_> {
    /// Whether the value is an object of its own, which entries that refer to it
    /// share.
    pub(crate) fn is_object(&self) -> bool {
        matches!(self.held, Held::Object(_))
    }

    /// Entry `key` absent, as [`Entries::source`] gives it.
    pub(crate) fn absent(key: &'static [u8]) -> Self {
        Self {
            held: Held::Absent,
            key,
        }
    }
}

/// What a value is read from, by [`ContentKey`]: one key for the same bytes read
/// in the same document under the same decryption, however they were reached, and
/// another for any other bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Held<'a> {
    /// The body of the object an entry refers to ([`XRef::content_key`]), which
    /// every entry of the table leading to it reads.
    Object(ContentKey<'a>),
    /// A reference to an object whose body the table does not lead to: read as
    /// null, and, where its place is lost, told so ([`XRef::watch`]); known by
    /// the reference, as no bytes hold it.
    Unread(ObjRef),
    /// Written in the dictionary read from these bytes, under the source's key.
    In(ContentKey<'a>),
    /// Entry `n` of the array read from these bytes.
    Item(ContentKey<'a>, usize),
    Absent,
}

/// A document's objects that its annotations refer to, read from the file once
/// ([`Objects::new`]): reading an object costs as much as it is long, which the
/// annotations that share it must not each pay. So each object a reference names is
/// read once, and so is each entry read of a dictionary that lies in one, and the
/// first entries read of an array there; an annotation's own entries, which no
/// other annotation reads, are read as they are wanted the first time it is looked
/// at, and as a shared object's after ([`Entries::shared`]). Generators read only
/// through these: they are given [`Entries`], [`Items`] and [`StreamEntries`], which
/// carry the objects of their document, and hold no dictionary, array or stream of
/// it otherwise. Read without them ([`Entries::detached`]), as boxes and matrices
/// of content outside annotations are, each reference is followed afresh.
#[derive(Clone, Default)]
pub(crate) struct Objects<'a> {
    read: Option<Rc<Read<'a>>>,
    /// Whether what is read lies in an object a reference names.
    shared: bool,
}

/// Where a dictionary or array lies: its data, which the document holds while it is
/// read, and its length, to tell whether one lies within another ([`Objects::own`]).
/// What is read from one is keyed by its [`ContentKey`] instead, which also tells
/// apart the same bytes decrypted for two objects.
type Place = (usize, usize);

/// Entries read of a dictionary, by key, each with what reading it lost.
type Keyed<'a> = hash::Map<Box<[u8]>, (Entry<'a>, Lost)>;

/// An entry read of an array, and the object it refers to.
type Item<'a> = (Found<'a>, Option<ObjRef>);

/// The first entries read of an array, whether they are all, and what reading them
/// lost.
type Prefix<'a> = (Vec<Item<'a>>, bool, Lost);

/// What is kept of the objects read, each with what reading it lost ([`Lost`]),
/// which every later use is told as the first was ([`XRef::note`]), as the reader's
/// own caches do. Objects are kept by their bodies ([`XRef::content_key`]): entries
/// of a damaged table that lead to one body read it once.
struct Read<'a> {
    xref: &'a XRef,
    /// The body each reference leads to, found once.
    bodies: RefCell<hash::Map<ObjRef, (Option<ContentKey<'a>>, Lost)>>,
    objects: RefCell<hash::Map<ContentKey<'a>, (Found<'a>, Lost)>>,
    /// The entries read of dictionaries in shared objects.
    entries: RefCell<hash::Map<ContentKey<'a>, Keyed<'a>>>,
    /// The first entries read of arrays in shared objects, each with the object it
    /// refers to, and whether they are all.
    prefixes: RefCell<hash::Map<ContentKey<'a>, Prefix<'a>>>,
    /// Where the data of each object an annotation's own-ness was told by lies, or
    /// `None` when it is no dictionary, array or stream ([`Objects::own`]).
    places: RefCell<hash::Map<ContentKey<'a>, (Option<Place>, Lost)>>,
    /// What is made of arrays ([`Items::derived`]), by the array and what is made,
    /// with whether making it read a number not established ([`noting`]).
    derived: RefCell<hash::Map<(ContentKey<'a>, TypeId), Made>>,
}

/// A value made of an array's entries ([`Derived`]), what making it lost, and whether
/// it read a number that cannot be read as `MuPDF` reads it.
type Made = (Rc<dyn Any>, Lost, bool);

/// What is made of an array's entries alone, such as the quadrilaterals of
/// `/QuadPoints`, kept once for the document by its type ([`Items::derived`]): so
/// that annotations sharing a long array do not each walk it where what they draw
/// does not grow with it. Each way of making one is a type of its own, which no
/// other way makes.
pub(crate) trait Derived: 'static {
    fn derive(items: &Items<'_>) -> Self;
}

impl<'a> Read<'a> {
    /// The body object `r` leads to ([`XRef::content_key`]); `None` where it leads to
    /// none, so that it reads as null, and where its place is lost, which the
    /// computation asking is told.
    fn body(&self, r: ObjRef) -> Option<ContentKey<'a>> {
        let known = self.bodies.borrow().get(&r).copied();
        let (body, lost) = known.unwrap_or_else(|| {
            let found = self.xref.watch(|| self.xref.content_key(r.into()));
            self.bodies.borrow_mut().insert(r, found);
            found
        });
        self.xref.note(lost);
        body
    }

    /// Where the value of a reference to `r` is held ([`Held`]).
    fn held(&self, r: ObjRef) -> Held<'a> {
        self.body(r).map_or(Held::Unread(r), Held::Object)
    }

    /// What was kept of `body`, its loss told to the computation asking, if anything.
    fn object(&self, body: ContentKey<'a>) -> Option<Found<'a>> {
        let (found, lost) = self.objects.borrow().get(&body).cloned()?;
        self.xref.note(lost);
        Some(found)
    }

    /// Object `r` read, and what reading it lost, which the computation asking is
    /// told too.
    fn read(&self, r: ObjRef) -> (Found<'a>, Lost) {
        self.xref
            .watch(|| Found::read(self.xref.get::<Object<'a>>(r.into())))
    }

    /// Object `r`, which leads to `body`, read once for every entry leading there.
    fn kept(&self, r: ObjRef, body: ContentKey<'a>) -> Found<'a> {
        self.object(body).unwrap_or_else(|| {
            let read_now = self.read(r);
            self.objects.borrow_mut().insert(body, read_now.clone());
            read_now.0
        })
    }
}

/// An object as it was read: a string's bytes and a name's shared, as they may
/// have been decoded, so that a copy costs the same however long they are; `None`
/// for a reference to no object or no entry.
#[derive(Clone)]
enum Found<'a> {
    /// Any object but a string or a name.
    Object(Object<'a>),
    String(hayro_syntax::object::String<'a>),
    Name(Name<'a>),
    None,
}

impl<'a> Found<'a> {
    /// `object`, read.
    fn of(object: Option<Object<'a>>) -> Self {
        match object {
            Some(Object::String(s)) => Self::String(s),
            Some(Object::Name(n)) => Self::Name(n),
            Some(object) => Self::Object(object),
            None => Self::None,
        }
    }

    /// `object`, with the work of reading it for an annotation: as much as it is
    /// long.
    fn read(object: Option<Object<'a>>) -> Self {
        let found = Self::of(object);
        super::work(match &found {
            Self::Object(Object::Dict(d)) => d.data().len(),
            Self::Object(Object::Array(a)) => a.data().len(),
            Self::Object(Object::Stream(s)) => s.dict().data().len(),
            Self::String(s) => s.as_bytes().len(),
            Self::Name(n) => n.len(),
            _ => 1,
        });
        found
    }
}

/// A dictionary's entry as it was read: what it holds, the object it refers to, and
/// whether the dictionary has the key.
#[derive(Clone)]
struct Entry<'a> {
    found: Found<'a>,
    r: Option<ObjRef>,
    written: bool,
}

impl Entry<'_> {
    /// Whether the entry is there as viewers look an entry up: a null value is no
    /// entry, nor is a word read as null (`foo`, `1e3`), but a reference is one,
    /// even to null or to no object (observed in `PyMuPDF` 1.28.2 bake: `/AP` and an
    /// inherited `/V` so written are absent, and so referred to, present).
    fn present(&self) -> bool {
        self.written && (self.r.is_some() || !matches!(self.found, Found::Object(Object::Null(_))))
    }
}

fn place(data: &[u8]) -> Place {
    (data.as_ptr() as usize, data.len())
}

impl<'a> Objects<'a> {
    /// The objects of the document of `xref`, none read yet.
    pub(crate) fn new(xref: &'a XRef) -> Self {
        Self {
            read: Some(Rc::new(Read {
                xref,
                bodies: RefCell::default(),
                objects: RefCell::default(),
                entries: RefCell::default(),
                prefixes: RefCell::default(),
                places: RefCell::default(),
                derived: RefCell::default(),
            })),
            shared: false,
        }
    }

    /// Entry `entry` of a dictionary or array read with these objects, its
    /// reference followed in `read`, theirs: what it holds, and the objects to read
    /// in it with.
    fn found(&self, read: &Read<'a>, entry: Option<MaybeRef<Object<'a>>>) -> (Found<'a>, Self) {
        match entry {
            Some(MaybeRef::Ref(r)) => {
                let found = match read.body(r) {
                    Some(body) => read.kept(r, body),
                    None => Found::None,
                };
                let shared = Self {
                    read: self.read.clone(),
                    shared: true,
                };
                (found, shared)
            }
            Some(MaybeRef::NotRef(o)) => (Found::read(Some(o)), self.clone()),
            None => (Found::None, self.clone()),
        }
    }
}

/// Which annotation an entry of a page's `/Annots` names, or which `/Annots` array a
/// page has, as a key for what a document makes of it once: what it is read from
/// ([`ContentKey`]), the body of the object it refers to ([`XRef::content_key`]),
/// one for every entry of the table leading there, or the dictionary or array
/// written in place: one key however many times it is read, as the body that holds
/// it is parsed once, and another for the same bytes decrypted for another object.
/// Reading the entry costs as much as finding this.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Id<'a> {
    Object(ContentKey<'a>),
    Written(ContentKey<'a>),
}

/// A page's `/Annots` array: the object it is, read only when its entries are
/// ([`Annots::entries`]), and the body it leads to, or the array written in the
/// page; with the objects of the document.
pub(crate) enum Annots<'a> {
    Object(ObjRef, ContentKey<'a>, Objects<'a>),
    Written(Array<'a>, Objects<'a>),
}

/// An entry of a page's `/Annots` that may name an annotation: its place in the
/// array, which annotation, and the object it refers to or the dictionary written
/// there.
pub(crate) struct Listed<'a> {
    pub(crate) index: usize,
    pub(crate) id: Id<'a>,
    named: Named<'a>,
}

enum Named<'a> {
    Object(ObjRef),
    Written(Dict<'a>),
}

impl<'a> Objects<'a> {
    /// The `/Annots` of page dictionary `page`; `None` when it has none, it refers
    /// to no body, or these are no document's objects. An array object it refers to
    /// is read only when its entries are.
    pub(crate) fn annots(&self, page: &Dict<'a>) -> Option<Annots<'a>> {
        let read = self.read.as_ref()?;
        match page.get_raw::<Object<'a>>(b"Annots")? {
            MaybeRef::Ref(r) => Some(Annots::Object(r, read.body(r)?, self.clone())),
            MaybeRef::NotRef(Object::Array(a)) => Some(Annots::Written(a, self.clone())),
            MaybeRef::NotRef(_) => None,
        }
    }

    /// The entries of the document's catalog, read afresh.
    pub(crate) fn catalog(&self) -> Option<Entries<'a>> {
        let read = self.read.as_ref()?;
        let root = read.xref.get::<Dict<'a>>(read.xref.root_id())?;
        let objects = Self {
            read: self.read.clone(),
            shared: true,
        };
        Some(Entries(root, objects))
    }

    /// The entries of the annotation `listed` names, read as its own are the first
    /// time it is looked at ([`Entries::shared`] for later); `None` when it names
    /// no dictionary. An annotation object is read as a dictionary, as one that is
    /// a stream's is, and afresh: from the page's `/Annots` too, whatever objects
    /// the page was read through (observed in `PyMuPDF` 1.28.2 bake, which draws an
    /// annotation that is the page itself).
    pub(crate) fn annotation(&self, listed: &Listed<'a>) -> Option<Entries<'a>> {
        let dict = match &listed.named {
            Named::Written(dict) => dict.clone(),
            Named::Object(r) => {
                let read = self.read.as_ref()?;
                let dict = read.xref.get::<Dict<'a>>((*r).into())?;
                super::work(dict.data().len());
                dict
            }
        };
        Some(Entries(dict, self.clone()))
    }

    /// Whether `dict` is an object of its own, rather than held directly in another,
    /// as an annotation written into a page's `/Annots` is; `None` where the file
    /// tells neither: the object `dict` names as its own is neither it nor one that
    /// holds it. A dictionary read without objects, or of no object, is taken as
    /// one of its own. Each body is read for this once for the document, and of it
    /// only where its data lies is kept.
    fn own(&self, dict: &Dict<'a>) -> Option<bool> {
        let (Some(read), Some(id)) = (&self.read, dict.obj_id()) else {
            return Some(true);
        };
        let r = ObjRef::from(id);
        let body = read.body(r)?;
        let at = place(dict.data());
        let known = read.places.borrow().get(&body).copied();
        let outer = match known {
            Some((outer, lost)) => {
                read.xref.note(lost);
                outer
            }
            None => {
                let kept = read.objects.borrow().get(&body).cloned();
                let (found, lost) = match kept {
                    Some((found, lost)) => {
                        read.xref.note(lost);
                        (found, lost)
                    }
                    None => {
                        let read_now = read.read(r);
                        // An object that holds annotations is kept for the others it
                        // holds; an annotation's own object, which no other reads, is
                        // not.
                        if !matches!(&read_now.0, Found::Object(Object::Dict(d)) if place(d.data()) == at)
                        {
                            read.objects.borrow_mut().insert(body, read_now.clone());
                        }
                        read_now
                    }
                };
                let outer = match &found {
                    Found::Object(Object::Dict(d)) => Some(place(d.data())),
                    Found::Object(Object::Array(a)) => Some(place(a.data())),
                    Found::Object(Object::Stream(s)) => Some(place(s.dict().data())),
                    _ => None,
                };
                read.places.borrow_mut().insert(body, (outer, lost));
                outer
            }
        };
        let (start, len) = outer?;
        if (start, len) == at {
            Some(true)
        } else {
            // Held within the object's data.
            (start <= at.0 && at.0 + at.1 <= start + len).then_some(false)
        }
    }
}

impl<'a> Annots<'a> {
    /// Which array this is: the body of the object it is, or what it is read from
    /// where it is written in the page, which reading it again does not change.
    pub(crate) fn id(&self) -> Id<'a> {
        match self {
            Self::Object(_, body, _) => Id::Object(*body),
            Self::Written(a, _) => Id::Written(a.content_key()),
        }
    }

    /// The entries that may name an annotation, in order: references to a body,
    /// and dictionaries written in the array. The array is read here, and of a
    /// reference no more than it and the body it leads to.
    pub(crate) fn entries(&self) -> impl Iterator<Item = Listed<'a>> + use<'a> {
        let (array, objects) = match self {
            Self::Written(a, objects) => (Some(a.clone()), objects.clone()),
            Self::Object(r, _, objects) => {
                let array = objects.read.as_ref().and_then(|read| {
                    match objects.found(read, Some(MaybeRef::Ref(*r))).0 {
                        Found::Object(Object::Array(a)) => Some(a),
                        _ => None,
                    }
                });
                (array, objects.clone())
            }
        };
        array
            .into_iter()
            .flat_map(|a| a.raw_iter())
            .enumerate()
            .filter_map(move |(index, entry)| {
                super::work(1);
                let (id, named) = match entry {
                    MaybeRef::Ref(r) => {
                        let body = objects.read.as_ref()?.body(r)?;
                        (Id::Object(body), Named::Object(r))
                    }
                    MaybeRef::NotRef(Object::Dict(d)) => {
                        super::work(d.data().len());
                        (Id::Written(d.content_key()), Named::Written(d))
                    }
                    MaybeRef::NotRef(Object::Stream(s)) => {
                        let d = s.dict().clone();
                        super::work(d.data().len());
                        (Id::Written(d.content_key()), Named::Written(d))
                    }
                    MaybeRef::NotRef(_) => return None,
                };
                Some(Listed { index, id, named })
            })
    }
}

/// The value of an entry or of an array's entry.
#[derive(Clone)]
pub(crate) enum Value<'a> {
    Number(Number),
    Bool(bool),
    Name(Name<'a>),
    String(hayro_syntax::object::String<'a>),
    Array(Items<'a>),
    Dict(Entries<'a>),
    /// A stream, decoded only where its text is read ([`Value::field_text`]).
    Stream(StreamEntries<'a>),
    /// Null, a reference to no object, or no entry.
    Null,
}

impl<'a> Value<'a> {
    fn of(found: Found<'a>, objects: &Objects<'a>) -> Self {
        let object = match found {
            Found::Object(o) => o,
            Found::String(s) => return Self::String(s),
            Found::Name(n) => return Self::Name(n),
            Found::None => return Self::Null,
        };
        match object {
            Object::Number(n) => Self::Number(n),
            Object::Boolean(b) => Self::Bool(b),
            // Read into `Found::Name` and `Found::String`.
            Object::Name(n) => Self::Name(n),
            Object::String(s) => Self::String(s),
            Object::Array(a) => Self::Array(Items(a, objects.clone())),
            Object::Dict(d) => Self::Dict(Entries(d, objects.clone())),
            Object::Stream(s) => Self::Stream(StreamEntries(s, objects.clone())),
            Object::Null(_) => Self::Null,
        }
    }

    /// `object`, read without [`Objects`].
    fn read(object: Option<Object<'a>>) -> Self {
        Self::of(Found::of(object), &Objects::default())
    }

    /// A number, read as a real (see the module).
    pub(crate) fn number(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(real(*n)),
            _ => None,
        }
    }

    /// A number, read as an integer (see the module).
    pub(crate) fn int(&self) -> Option<i32> {
        match self {
            Self::Number(n) => Some(int(*n)),
            _ => None,
        }
    }

    /// A flags entry (`/F`, `/Ff`): the bits of its integer; no flags when it is not
    /// a number.
    pub(crate) fn flags(&self) -> u32 {
        self.int().map_or(0, |i| i as u32)
    }

    pub(crate) fn name(&self) -> Option<&[u8]> {
        match self {
            Self::Name(n) => Some(n),
            _ => None,
        }
    }

    /// Whether [`Value::text`] gives text, told without copying it.
    pub(crate) fn is_text(&self) -> bool {
        matches!(self, Self::String(_))
    }

    /// The length of [`Value::text`], told without copying it: 0 for no text.
    pub(crate) fn text_len(&self) -> usize {
        match self {
            Self::String(s) => s.as_bytes().len(),
            _ => 0,
        }
    }

    /// The bytes of a text entry ([`Text`]) whose value this is: a string's.
    pub(crate) fn text(&self) -> Option<Vec<u8>> {
        match self {
            Self::String(s) => Some(s.as_bytes().to_vec()),
            _ => None,
        }
    }

    /// The bytes of a text or choice field's value, `/V`, whose value this is: a
    /// string's, or a stream's data. Viewers (`PyMuPDF` bake, observed) show the data
    /// as decoding them whole makes them ([`Stream::decoded_strict`]) where they are
    /// stored as they are or by Flate alone, damaged or cut short, with or without a
    /// predictor, whose reading matches theirs byte for byte, a last row cut short
    /// included, its parameters written in the stream's dictionary. Elsewhere,
    /// [`Unsupported::ValueStream`]: where the data do not decode, viewers show what
    /// their decoder made before it failed, in pieces of their own buffering; data cut
    /// short in another filter are read otherwise by them (a half byte of hexadecimal,
    /// a group of base-85 without its end, a cut run, a byte for an LZW code cut
    /// short); and parameters given by reference may be ones the cycle check rejects.
    /// Writers make no such value, and it is not told apart from a valid one.
    pub(crate) fn field_text(&self) -> Result<Option<Vec<u8>>, Unsupported> {
        let stream = match self {
            Self::String(s) => return Ok(Some(s.as_bytes().to_vec())),
            Self::Stream(s) => &s.0,
            _ => return Ok(None),
        };
        // Parameters given by reference may lead back to an object being read, which
        // the cycle check rejects where viewers read it (issue 59): refused too.
        let filters = stream.filter_entries();
        let mut applied = filters.applied();
        let shown = match (applied.next(), applied.next()) {
            (None, _) => true,
            (Some((Filter::FlateDecode, flate)), None) => !flate.params_by_reference,
            _ => false,
        };
        if !shown {
            return Err(Unsupported::ValueStream);
        }
        stream
            .decoded_strict()
            .map(|data| Some(data.into_owned()))
            .map_err(|_| Unsupported::ValueStream)
    }

    pub(crate) fn items(self) -> Option<Items<'a>> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// The entries of an array, each in its place.
#[derive(Clone)]
pub(crate) struct Items<'a>(Array<'a>, Objects<'a>);

impl<'a> Items<'a> {
    /// The entries in order; one that refers to no object is [`Value::Null`].
    pub(crate) fn iter(&self) -> impl Iterator<Item = Value<'a>> + '_ {
        // One of the two runs; neither is boxed, as arrays of a few numbers are
        // read for every annotation.
        let (with_objects, without) = match &self.1.read {
            Some(read) => (
                Some(self.0.raw_iter().map(|e| {
                    let (found, objects) = self.1.found(read, Some(e));
                    Value::of(found, &objects)
                })),
                None,
            ),
            None => (None, Some(self.0.items::<Object<'a>>().map(Value::read))),
        };
        with_objects
            .into_iter()
            .flatten()
            .chain(without.into_iter().flatten())
    }

    /// `T` made of these entries ([`Derived`]), once for the document: a later use,
    /// by any annotation, costs a lookup, and is told what making it lost and whether
    /// it read a number not established, as the first was.
    pub(crate) fn derived<T: Derived>(&self) -> Rc<T> {
        let Some(read) = &self.1.read else {
            return Rc::new(T::derive(self));
        };
        let key = (self.0.content_key(), TypeId::of::<T>());
        let known = read.derived.borrow().get(&key).cloned();
        let made = match known {
            Some((made, lost, unreadable_)) => {
                read.xref.note(lost);
                if unreadable_ {
                    unreadable();
                }
                made
            }
            None => {
                let ((made, unreadable_), lost) = read
                    .xref
                    .watch(|| noting(|| Rc::new(T::derive(self)) as Rc<dyn Any>));
                read.derived
                    .borrow_mut()
                    .insert(key, (made.clone(), lost, unreadable_));
                made
            }
        };
        // Kept under the type it was made as.
        made.downcast::<T>().expect("made as T")
    }

    /// `T` made of each entry that is an array, in order: once for the document
    /// for an array the entry refers to, which other arrays may refer to as well
    /// ([`Self::derived`]), and for one written in this array, as part of what is
    /// made of this one. So arrays of their own that share an array they refer
    /// to, such as `/InkList` arrays sharing a path, walk it once between them.
    pub(crate) fn each<T: Derived>(&self) -> Vec<Rc<T>> {
        let Some(read) = &self.1.read else {
            let arrays = self.iter().filter_map(Value::items);
            return arrays.map(|items| Rc::new(T::derive(&items))).collect();
        };
        self.0
            .raw_iter()
            .filter_map(|e| {
                let referred = matches!(e, MaybeRef::Ref(_));
                let (found, objects) = self.1.found(read, Some(e));
                let items = Value::of(found, &objects).items()?;
                Some(if referred {
                    items.derived::<T>()
                } else {
                    Rc::new(T::derive(&items))
                })
            })
            .collect()
    }

    /// The first `n` entries, or all when there are fewer; those after them are not
    /// read, and in a shared object these are read once.
    pub(crate) fn head(&self, n: usize) -> Vec<Value<'a>> {
        self.prefix(n).into_iter().map(|(v, _)| v).collect()
    }

    /// The first `n` entries, or all when there are fewer, each with the object it
    /// refers to; those after them are not read, and in a shared object these are
    /// read once.
    fn prefix(&self, n: usize) -> Vec<(Value<'a>, Option<ObjRef>)> {
        let Some(read) = self.shared_read() else {
            let refs = self.0.raw_iter().map(|e| e.as_obj_ref());
            return self.iter().zip(refs).take(n).collect();
        };
        let at = self.0.content_key();
        let known = read.prefixes.borrow().get(&at).cloned();
        let found = match known {
            Some((found, all, lost)) if all || found.len() >= n => {
                read.xref.note(lost);
                found
            }
            _ => {
                let ((found, all), lost) = read.xref.watch(|| {
                    let mut entries = self.0.raw_iter();
                    let found: Vec<Item<'a>> = entries
                        .by_ref()
                        .take(n)
                        .map(|e| (e.as_obj_ref(), e))
                        .map(|(r, e)| (self.1.found(read, Some(e)).0, r))
                        .collect();
                    let all = found.len() < n || entries.next().is_none();
                    (found, all)
                });
                read.prefixes
                    .borrow_mut()
                    .insert(at, (found.clone(), all, lost));
                found
            }
        };
        // Every entry of a shared array is read with the objects of shared ones.
        found
            .into_iter()
            .take(n)
            .map(|(f, r)| (Value::of(f, &self.1), r))
            .collect()
    }

    /// What reads the entries where the array is shared, whose prefixes are kept.
    fn shared_read(&self) -> Option<&Rc<Read<'a>>> {
        self.1.read.as_ref().filter(|_| self.1.shared)
    }

    /// Entry `i`, and the object it refers to; [`Value::Null`] beyond the end.
    fn entry(&self, i: usize) -> (Value<'a>, Option<ObjRef>) {
        match &self.1.read {
            // Of an annotation's own, the entries before it are not kept.
            Some(read) if !self.1.shared => match self.0.raw_iter().nth(i) {
                Some(e) => {
                    let r = e.as_obj_ref();
                    let (found, objects) = self.1.found(read, Some(e));
                    (Value::of(found, &objects), r)
                }
                None => (Value::Null, None),
            },
            _ => self
                .prefix(i + 1)
                .into_iter()
                .nth(i)
                .unwrap_or((Value::Null, None)),
        }
    }

    /// Entry `i`; [`Value::Null`] beyond the end.
    pub(crate) fn get(&self, i: usize) -> Value<'a> {
        self.entry(i).0
    }

    /// The entries as numbers, one that is not a number read as 0.
    pub(crate) fn numbers(&self) -> Vec<f64> {
        self.iter().map(|v| v.number().unwrap_or(0.0)).collect()
    }

    /// The first `n` entries, or all when there are fewer, as numbers, one that is
    /// not a number read as 0; those after them are not read.
    pub(crate) fn first(&self, n: usize) -> Numbers {
        let number = |v: &Value<'a>| v.number().unwrap_or(0.0);
        if self.shared_read().is_none() {
            // As `prefix` reads them, without its list.
            return self.iter().take(n).map(|v| number(&v)).collect();
        }
        self.prefix(n).iter().map(|(v, _)| number(v)).collect()
    }

    /// Where entry `i` of this array, which refers to `r`, is held ([`Held`]).
    fn held(&self, r: Option<ObjRef>, i: usize) -> Held<'a> {
        match (r, &self.1.read) {
            (Some(r), Some(read)) => read.held(r),
            // Read without objects: no document to find a body in.
            (Some(r), None) => Held::Unread(r),
            (None, _) => Held::Item(self.0.content_key(), i),
        }
    }

    /// The labels of the options of this `/Opt` array: of each entry, the text it
    /// shows, after its export value in a pair or alone, and where it is held
    /// ([`Held`]), so that a document sets an option many lists show once.
    pub(crate) fn labels(&self) -> Vec<Label<'a>> {
        self.0
            .raw_iter()
            .zip(self.iter())
            .enumerate()
            .map(|(i, (raw, value))| {
                super::work(1);
                let (held, value) = match value {
                    Value::Array(pair) => {
                        let (value, r) = pair.entry(1);
                        (pair.held(r, 1), value)
                    }
                    value => (self.held(raw.as_obj_ref(), i), value),
                };
                Label {
                    source: Source { held, key: b"Opt" },
                    value,
                }
            })
            .collect()
    }
}

/// An option of a list ([`Items::labels`]).
pub(crate) struct Label<'a> {
    pub(crate) source: Source<'a>,
    pub(crate) value: Value<'a>,
}

impl<'a> Entries<'a> {
    /// The entries of `dict`, of content outside annotations, its references
    /// followed afresh ([`Objects`]). Annotations are read with their document's
    /// objects ([`Objects::annotation`]), and hold no dictionary to read so.
    pub(crate) fn detached(dict: Dict<'a>) -> Self {
        Self(dict, Objects::default())
    }

    /// These entries, read from now on as a shared object's are: each once, and
    /// kept. An annotation looked at again is read so.
    pub(crate) fn shared(&self) -> Self {
        let objects = Objects {
            read: self.1.read.clone(),
            shared: true,
        };
        Self(self.0.clone(), objects)
    }

    /// Entry `key` as read, and the objects to read in it with. In a shared object,
    /// it is read once.
    fn entry(&self, key: &[u8]) -> (Entry<'a>, Objects<'a>) {
        let Some(read) = &self.1.read else {
            let entry = Entry {
                found: Found::of(self.0.get::<Object<'a>>(key)),
                r: self.0.get_ref(key),
                written: self.0.contains_key(key),
            };
            return (entry, Objects::default());
        };
        let fresh = || {
            let raw = self.0.get_raw::<Object<'a>>(key);
            let r = raw.as_ref().and_then(MaybeRef::as_obj_ref);
            let (found, objects) = self.1.found(read, raw);
            let written = self.0.contains_key(key);
            (Entry { found, r, written }, objects)
        };
        if !self.1.shared {
            return fresh();
        }
        let at = self.0.content_key();
        let known = read
            .entries
            .borrow()
            .get(&at)
            .and_then(|entries| entries.get(key).cloned());
        let entry = match known {
            Some((entry, lost)) => {
                read.xref.note(lost);
                entry
            }
            None => {
                let ((entry, _), lost) = read.xref.watch(fresh);
                read.entries
                    .borrow_mut()
                    .entry(at)
                    .or_default()
                    .insert(key.into(), (entry.clone(), lost));
                entry
            }
        };
        // Every entry of a shared object is read with the objects of shared ones.
        (entry, self.1.clone())
    }

    /// Entry `key`; [`Value::Null`] when there is none.
    pub(crate) fn get(&self, key: &[u8]) -> Value<'a> {
        let (entry, objects) = self.entry(key);
        Value::of(entry.found, &objects)
    }

    /// Whether entry `key` is there as viewers look an entry up ([`Entry::present`]).
    pub(crate) fn present(&self, key: &[u8]) -> bool {
        self.entry(key).0.present()
    }

    pub(crate) fn number(&self, key: &[u8]) -> Option<f64> {
        self.get(key).number()
    }

    pub(crate) fn int(&self, key: &[u8]) -> Option<i32> {
        self.get(key).int()
    }

    pub(crate) fn bool(&self, key: &[u8]) -> Option<bool> {
        match self.get(key) {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }

    pub(crate) fn name(&self, key: &[u8]) -> Option<Name<'a>> {
        match self.get(key) {
            Value::Name(n) => Some(n),
            _ => None,
        }
    }

    /// Whether entry `key` is the name `name`.
    pub(crate) fn is(&self, key: &[u8], name: &[u8]) -> bool {
        self.name(key).is_some_and(|n| *n == *name)
    }

    /// The bytes of text entry `entry` ([`Value::text`]).
    pub(crate) fn text(&self, entry: Text) -> Option<Vec<u8>> {
        self.get(entry.key()).text()
    }

    /// The length of the bytes of text entry `entry` ([`Value::text_len`]), told
    /// without copying them.
    pub(crate) fn text_len(&self, entry: Text) -> usize {
        self.get(entry.key()).text_len()
    }

    /// Whether text entry `entry` gives text ([`Value::is_text`]).
    pub(crate) fn has_text(&self, entry: Text) -> bool {
        self.get(entry.key()).is_text()
    }

    pub(crate) fn dict(&self, key: &[u8]) -> Option<Self> {
        match self.get(key) {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    pub(crate) fn items(&self, key: &[u8]) -> Option<Items<'a>> {
        self.get(key).items()
    }

    /// The first `len` entries of array `key` as numbers (see [`Items::first`]),
    /// with 0 for those it lacks; `None` when `key` is not an array. The entries
    /// after them are not read.
    pub(crate) fn numbers(&self, key: &[u8], len: usize) -> Option<Numbers> {
        let mut n = self.items(key)?.first(len);
        n.resize(len, 0.0);
        Some(n)
    }

    /// Matrix `key` (`/Matrix`), six numbers read as [`Entries::numbers`] reads them;
    /// the identity when it is not an array.
    pub(crate) fn matrix(&self, key: &[u8]) -> [f64; 6] {
        match self.numbers(key, 6) {
            Some(m) => [m[0], m[1], m[2], m[3], m[4], m[5]],
            None => [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        }
    }

    /// Rectangle `key` (`/BBox`), four numbers read as [`Entries::numbers`] reads
    /// them, normalized; `None` when it is not an array.
    pub(crate) fn rect(&self, key: &[u8]) -> Option<kurbo::Rect> {
        let n = self.numbers(key, 4)?;
        Some(kurbo::Rect::new(n[0], n[1], n[2], n[3]).abs())
    }

    /// Where entry `key`'s value is held ([`Source`]). Told from the entry as read: a
    /// text many annotations share is so known by the body that holds it, and one
    /// written in a dictionary by that dictionary, however long.
    pub(crate) fn source(&self, key: &'static [u8]) -> Source<'a> {
        let entry = self.entry(key).0;
        let held = match (entry.r, &self.1.read) {
            (Some(r), Some(read)) => read.held(r),
            // Read without objects: no document to find a body in.
            (Some(r), None) => Held::Unread(r),
            (None, _) if !entry.present() => Held::Absent,
            (None, _) => Held::In(self.content_key()),
        };
        Source { held, key }
    }

    /// What this dictionary is read from ([`ContentKey`]), the same however often it
    /// is read.
    pub(crate) fn content_key(&self) -> ContentKey<'a> {
        self.0.content_key()
    }

    /// Whether this dictionary is an object of its own ([`Objects`]), rather than
    /// held directly in another.
    pub(crate) fn own(&self) -> Option<bool> {
        self.1.own(&self.0)
    }

    /// The dictionary `/Parent` names.
    pub(crate) fn parent(&self) -> Option<Self> {
        self.dict(b"Parent")
    }
}

/// A stream an entry holds, with the objects of its document: its dictionary is read
/// as [`Entries`], and the stream itself is given out only as what an annotation
/// draws ([`StreamEntries::into_stream`]).
#[derive(Clone)]
pub(crate) struct StreamEntries<'a>(Stream<'a>, Objects<'a>);

impl<'a> StreamEntries<'a> {
    /// The entries of the stream's dictionary.
    pub(crate) fn dict(&self) -> Entries<'a> {
        Entries(self.0.dict().clone(), self.1.clone())
    }

    /// The stream, for a renderer or writer to draw.
    pub(crate) fn into_stream(self) -> Stream<'a> {
        self.0
    }
}
