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
use hayro_syntax::object::{Array, Dict, MaybeRef, Name, Null, Number, ObjRef, Object, Stream};
use hayro_syntax::xref::XRef;
use rustc_hash::FxHashMap;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

thread_local! {
    /// Whether a number that cannot be read as `MuPDF` reads it was read since
    /// [`read`] began.
    static UNREADABLE: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f`, which reads entries; [`Unsupported::Number`] when it read a number
/// that cannot be read as `MuPDF` reads it, whatever `f` made of it.
pub(crate) fn read<T>(f: impl FnOnce() -> T) -> Result<T, Unsupported> {
    let before = UNREADABLE.with(|u| u.replace(false));
    let value = f();
    let unreadable = UNREADABLE.with(|u| u.replace(before));
    if unreadable {
        Err(Unsupported::Number)
    } else {
        Ok(value)
    }
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

/// A flags entry (`/F`, `/Ff`) read as a number: the bits of its integer, none
/// when it is not a number.
pub(crate) fn flags(n: Option<Number>) -> u32 {
    n.map_or(0, |n| int(n) as u32)
}

/// A dictionary an annotation is drawn from: the annotation, one it refers to (its
/// border style, its appearance characteristics, an appearance stream's), or a field
/// above a widget.
#[derive(Clone)]
pub(crate) struct Entries<'a>(Dict<'a>, Objects<'a>);

/// An entry that holds text, and whether viewers read a stream there as its text:
/// only a field's value is (observed in `PyMuPDF` bake); elsewhere a stream is no
/// text, as if the entry were absent.
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
    /// `/V` of a text field or choice field.
    FieldValue,
    /// `/RV` of a rich text field.
    RichValue,
    /// An entry of `/Opt`, or of a choice field's `/V` array.
    Option,
}

impl Text {
    pub(crate) const fn key(self) -> &'static [u8] {
        match self {
            Self::Contents => b"Contents",
            Self::DefaultAppearance => b"DA",
            Self::Rich => b"RC",
            Self::DefaultStyle => b"DS",
            Self::Caption => b"CA",
            Self::FieldValue => b"V",
            Self::RichValue => b"RV",
            Self::Option => b"Opt",
        }
    }

    const fn stream(self) -> bool {
        matches!(self, Self::FieldValue)
    }
}

/// Entry `key` as a key for what a document makes of its value once: where the
/// value is held (the object it refers to, the object of the dictionary it lies
/// in, or nowhere, when the entry is absent), or its text, and the entry's key, so
/// that two entries with the same source have the same value and are read alike.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Source {
    held: Held,
    key: &'static [u8],
}

impl Source {
    /// Entry `key` absent, as [`Entries::source`] gives it.
    pub(crate) fn absent(key: &'static [u8]) -> Self {
        Self {
            held: Held::Absent,
            key,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Held {
    Object(ObjRef),
    In(ObjRef),
    Absent,
    Text(Option<Vec<u8>>),
    Name(Vec<u8>),
    /// Entry `n` of the array held there.
    Item(Box<Self>, usize),
}

/// A document's objects that its annotations refer to, read from the file once
/// ([`Objects::new`]): reading an object costs as much as it is long, which the
/// annotations that share it must not each pay. So each object a reference names is
/// read once, and so is each entry read of a dictionary that lies in one, and the
/// first entries read of an array there; an annotation's own entries, which no
/// other annotation reads, are read as they are wanted. Read without them
/// ([`Objects::default`]), as boxes and matrices outside annotations are, each
/// reference is followed afresh.
#[derive(Clone, Default)]
pub(crate) struct Objects<'a> {
    read: Option<Rc<Read<'a>>>,
    /// Whether what is read lies in an object a reference names.
    shared: bool,
}

/// Where a dictionary or array lies: its data, which the document holds while it is
/// read, and its length.
type Place = (usize, usize);

/// Entries read of a dictionary, by key.
type Keyed<'a> = FxHashMap<Box<[u8]>, Found<'a>>;

struct Read<'a> {
    xref: &'a XRef,
    objects: RefCell<FxHashMap<ObjRef, Found<'a>>>,
    /// The entries read of dictionaries in shared objects.
    entries: RefCell<FxHashMap<Place, Keyed<'a>>>,
    /// The first entries read of arrays in shared objects, and whether they are all.
    prefixes: RefCell<FxHashMap<Place, (Vec<Found<'a>>, bool)>>,
}

/// An object as it was read: a string's bytes shared, as they may have been
/// decoded; `None` for a reference to no object or no entry.
#[derive(Clone)]
enum Found<'a> {
    Object(Object<'a>),
    String(Rc<hayro_syntax::object::String<'a>>),
    None,
}

impl<'a> Found<'a> {
    /// `object`, with the work of reading it: as much as it is long.
    fn read(object: Option<Object<'a>>) -> Self {
        let found = match object {
            Some(Object::String(s)) => Self::String(Rc::new(s)),
            Some(object) => Self::Object(object),
            None => Self::None,
        };
        super::work(match &found {
            Self::Object(Object::Dict(d)) => d.data().len(),
            Self::Object(Object::Array(a)) => a.data().len(),
            Self::Object(Object::Stream(s)) => s.dict().data().len(),
            Self::String(s) => s.as_bytes().len(),
            _ => 1,
        });
        found
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
                objects: RefCell::default(),
                entries: RefCell::default(),
                prefixes: RefCell::default(),
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
                let known = read.objects.borrow().get(&r).cloned();
                let found = known.unwrap_or_else(|| {
                    let found = Found::read(read.xref.get::<Object<'a>>(r.into()));
                    read.objects.borrow_mut().insert(r, found.clone());
                    found
                });
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

/// Whether `inner` lies within `outer`, both data the document holds.
fn within(inner: &[u8], outer: &[u8]) -> bool {
    let (i, o) = (inner.as_ptr_range(), outer.as_ptr_range());
    o.start <= i.start && i.end <= o.end
}

impl<'a> Objects<'a> {
    /// Whether `dict` is an object of its own, rather than held directly in another,
    /// as an annotation written into a page's `/Annots` is; `None` where the file
    /// tells neither: the object `dict` names as its own is neither it nor one that
    /// holds it. A dictionary read without objects, or of no object, is taken as
    /// one of its own.
    fn own(&self, dict: &Dict<'a>) -> Option<bool> {
        let (Some(read), Some(id)) = (&self.read, dict.obj_id()) else {
            return Some(true);
        };
        let r = ObjRef::from(id);
        let at = dict.data();
        let known = read.objects.borrow().get(&r).cloned();
        let found = known.unwrap_or_else(|| {
            let found = Found::read(read.xref.get::<Object<'a>>(id));
            // An object that holds annotations is kept for the others it holds;
            // an annotation's own object, which no other reads, is not.
            if !matches!(&found, Found::Object(Object::Dict(d)) if place(d.data()) == place(at)) {
                read.objects.borrow_mut().insert(r, found.clone());
            }
            found
        });
        let data = match &found {
            Found::Object(Object::Dict(d)) => d.data(),
            Found::Object(Object::Array(a)) => a.data(),
            Found::Object(Object::Stream(s)) => s.dict().data(),
            _ => return None,
        };
        if place(data) == place(at) {
            Some(true)
        } else {
            within(at, data).then_some(false)
        }
    }
}

/// The value of an entry or of an array's entry.
#[derive(Clone)]
pub(crate) enum Value<'a> {
    Number(Number),
    Bool(bool),
    Name(Name<'a>),
    String(Rc<hayro_syntax::object::String<'a>>),
    Array(Items<'a>),
    Dict(Entries<'a>),
    /// A stream, decoded only where its text is read ([`Value::text`]).
    Stream(Stream<'a>),
    /// Null, a reference to no object, or no entry.
    Null,
}

impl<'a> Value<'a> {
    fn of(found: Found<'a>, objects: &Objects<'a>) -> Self {
        let object = match found {
            Found::Object(o) => o,
            Found::String(s) => return Self::String(s),
            Found::None => return Self::Null,
        };
        match object {
            Object::Number(n) => Self::Number(n),
            Object::Boolean(b) => Self::Bool(b),
            Object::Name(n) => Self::Name(n),
            Object::String(s) => Self::String(Rc::new(s)),
            Object::Array(a) => Self::Array(Items(a, objects.clone())),
            Object::Dict(d) => Self::Dict(Entries(d, objects.clone())),
            Object::Stream(s) => Self::Stream(s),
            Object::Null(_) => Self::Null,
        }
    }

    /// `object`, read without [`Objects`].
    fn read(object: Option<Object<'a>>) -> Self {
        Self::of(
            object.map_or(Found::None, Found::Object),
            &Objects::default(),
        )
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
            Self::Name(n) => Some(n.as_ref()),
            _ => None,
        }
    }

    /// The bytes of text entry `entry` whose value this is: a string's, or, where
    /// viewers read one, a stream's decoded data.
    pub(crate) fn text(&self, entry: Text) -> Option<Vec<u8>> {
        match self {
            Self::String(s) => Some(s.as_bytes().to_vec()),
            Self::Stream(s) if entry.stream() => s.decoded().ok().map(std::borrow::Cow::into_owned),
            _ => None,
        }
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
    pub(crate) fn iter(&self) -> Box<dyn Iterator<Item = Value<'a>> + '_> {
        match &self.1.read {
            Some(read) => Box::new(self.0.raw_iter().map(|e| {
                let (found, objects) = self.1.found(read, Some(e));
                Value::of(found, &objects)
            })),
            None => Box::new(self.0.items::<Object<'a>>().map(Value::read)),
        }
    }

    /// The first `n` entries, or all when there are fewer; those after them are not
    /// read, and in a shared object these are read once.
    fn prefix(&self, n: usize) -> Vec<Value<'a>> {
        let Some(read) = self.1.read.as_ref().filter(|_| self.1.shared) else {
            return self.iter().take(n).collect();
        };
        let at = place(self.0.data());
        let known = read.prefixes.borrow().get(&at).cloned();
        let found = match known {
            Some((found, all)) if all || found.len() >= n => found,
            _ => {
                let mut entries = self.0.raw_iter();
                let found: Vec<Found<'a>> = entries
                    .by_ref()
                    .take(n)
                    .map(|e| self.1.found(read, Some(e)).0)
                    .collect();
                let all = found.len() < n || entries.next().is_none();
                read.prefixes.borrow_mut().insert(at, (found.clone(), all));
                found
            }
        };
        // Every entry of a shared array is read with the objects of shared ones.
        found
            .into_iter()
            .take(n)
            .map(|f| Value::of(f, &self.1))
            .collect()
    }

    /// Entry `i`; [`Value::Null`] beyond the end.
    pub(crate) fn get(&self, i: usize) -> Value<'a> {
        self.prefix(i + 1).into_iter().nth(i).unwrap_or(Value::Null)
    }

    /// The entries as numbers, one that is not a number read as 0.
    pub(crate) fn numbers(&self) -> Vec<f64> {
        self.iter().map(|v| v.number().unwrap_or(0.0)).collect()
    }

    /// The first `n` entries, or all when there are fewer, as numbers, one that is
    /// not a number read as 0; those after them are not read.
    pub(crate) fn first(&self, n: usize) -> Vec<f64> {
        self.prefix(n)
            .iter()
            .map(|v| v.number().unwrap_or(0.0))
            .collect()
    }

    /// The labels of the options of `/Opt` array, held in `source`: of each entry,
    /// the text it shows, after its export value in a pair or alone, and where it is
    /// held, so that a document sets an option many lists show once.
    pub(crate) fn labels(&self, source: Option<&Source>) -> Vec<Label<'a>> {
        self.0
            .raw_iter()
            .zip(self.iter())
            .enumerate()
            .map(|(i, (raw, value))| {
                super::work(1);
                let (r, value) = match value {
                    Value::Array(pair) => {
                        let second = pair.0.raw_iter().nth(1);
                        (second.and_then(|e| e.as_obj_ref()), pair.get(1))
                    }
                    value => (raw.as_obj_ref(), value),
                };
                let held = match r {
                    Some(r) => Some(Held::Object(r)),
                    None => source.map(|s| Held::Item(Box::new(s.held.clone()), i)),
                };
                Label {
                    source: held.map(|held| Source { held, key: b"Opt" }),
                    value,
                }
            })
            .collect()
    }
}

/// An option of a list ([`Items::labels`]).
pub(crate) struct Label<'a> {
    pub(crate) source: Option<Source>,
    pub(crate) value: Value<'a>,
}

impl<'a> Entries<'a> {
    /// The entries of `dict`, its references followed afresh ([`Objects`]).
    pub(crate) fn new(dict: Dict<'a>) -> Self {
        Self(dict, Objects::default())
    }

    /// The entries of `dict`, of a document whose objects are `objects`.
    pub(crate) fn of(dict: Dict<'a>, objects: &Objects<'a>) -> Self {
        Self(dict, objects.clone())
    }

    /// Entry `key`; [`Value::Null`] when there is none. In a shared object, it is
    /// read once.
    pub(crate) fn get(&self, key: &[u8]) -> Value<'a> {
        let Some(read) = &self.1.read else {
            return Value::read(self.0.get::<Object<'a>>(key));
        };
        if !self.1.shared {
            let (found, objects) = self.1.found(read, self.0.get_raw::<Object<'a>>(key));
            return Value::of(found, &objects);
        }
        let at = place(self.0.data());
        let known = read
            .entries
            .borrow()
            .get(&at)
            .and_then(|entries| entries.get(key).cloned());
        let found = known.unwrap_or_else(|| {
            let (found, _) = self.1.found(read, self.0.get_raw::<Object<'a>>(key));
            read.entries
                .borrow_mut()
                .entry(at)
                .or_default()
                .insert(key.into(), found.clone());
            found
        });
        Value::of(found, &self.1)
    }

    /// Whether entry `key` is there as viewers look an entry up: a null value is no
    /// entry, but a reference is one, even to no object (it then reads as null).
    /// The value is not read but to tell whether it is null.
    pub(crate) fn present(&self, key: &[u8]) -> bool {
        self.0.contains_key(key) && self.0.get::<Null>(key).is_none()
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
        self.name(key).is_some_and(|n| n.as_ref() == name)
    }

    /// The bytes of text entry `entry` ([`Value::text`]).
    pub(crate) fn text(&self, entry: Text) -> Option<Vec<u8>> {
        self.get(entry.key()).text(entry)
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
    pub(crate) fn numbers(&self, key: &[u8], len: usize) -> Option<Vec<f64>> {
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

    /// Where entry `key`'s value is held ([`Source`]); `None` for a value in a direct
    /// dictionary, which no key names. The value is not read.
    pub(crate) fn source(&self, key: &'static [u8]) -> Option<Source> {
        let held = if let Some(r) = self.0.get_ref(key) {
            Held::Object(r)
        } else if !self.present(key) {
            Held::Absent
        } else {
            Held::In(self.id()?)
        };
        Some(Source { held, key })
    }

    /// Text entry `entry` as a source: the object it refers to, read once for the
    /// document, or, in the dictionary itself, its text or name, which annotations
    /// sharing it then share however they are held.
    pub(crate) fn text_source(&self, entry: Text) -> Source {
        let key = entry.key();
        let held = match self.0.get_ref(key) {
            Some(r) => Held::Object(r),
            None => match self.get(key) {
                Value::Name(n) => Held::Name(n.as_ref().to_vec()),
                value => Held::Text(value.text(entry)),
            },
        };
        Source { held, key }
    }

    /// Whether this dictionary is an object of its own ([`Objects`]), rather than
    /// held directly in another.
    pub(crate) fn own(&self) -> Option<bool> {
        self.1.own(&self.0)
    }

    /// The object this dictionary is, when it is one of its own.
    pub(crate) fn id(&self) -> Option<ObjRef> {
        self.0.obj_id().map(ObjRef::from)
    }

    /// The dictionary `/Parent` names, and the reference to it when it is an object of
    /// its own.
    pub(crate) fn parent(&self) -> Option<(Option<ObjRef>, Self)> {
        let parent = self.dict(b"Parent")?;
        Some((self.0.get_ref(b"Parent"), parent))
    }
}
