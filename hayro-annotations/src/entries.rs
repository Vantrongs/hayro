//! Position-preserving annotation dictionary reads and document-local object identity.

use super::Unsupported;
use crate::hash;
use hayro_interpret::OcgState;
use hayro_syntax::object::{Array, Dict, MaybeRef, Name, Number, ObjRef, Object, Stream};
use hayro_syntax::xref::XRef;
use smallvec::SmallVec;
use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::rc::Rc;

pub(crate) type Numbers = SmallVec<[f64; 8]>;

pub(crate) fn real(n: Number) -> f64 {
    f64::from(n.as_f32())
}

fn int(n: Number) -> i32 {
    if n.is_integer() {
        n.as_i64() as i32
    } else {
        let rounded = (n.as_f32() + 0.5).floor();
        if (-2_147_483_648.0..2_147_483_648.0).contains(&rounded) {
            rounded as i32
        } else {
            i32::MIN
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Location(Option<hayro_syntax::object::ObjectIdentifier>, usize, usize);

fn location(id: Option<hayro_syntax::object::ObjectIdentifier>, data: &[u8]) -> Location {
    Location(id, data.as_ptr() as usize, data.len())
}

#[derive(Clone)]
pub(crate) struct Entries<'a>(Dict<'a>, Objects<'a>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Text {
    Contents,
    DefaultAppearance,
    Rich,
    DefaultStyle,
    Caption,
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

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Source {
    held: Held,
    key: &'static [u8],
}

impl Source {
    pub(crate) fn is_object(&self) -> bool {
        matches!(self.held, Held::Object(_))
    }

    pub(crate) fn absent(key: &'static [u8]) -> Self {
        Self {
            held: Held::Absent,
            key,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Held {
    Object(ObjRef),
    Unread(ObjRef),
    In(Location),
    Item(Location, usize),
    Absent,
}

#[derive(Clone, Default)]
pub(crate) struct Objects<'a> {
    read: Option<Rc<Read<'a>>>,
    shared: bool,
    owner: Option<hayro_syntax::object::ObjectIdentifier>,
}

type Place = (usize, usize);

type Keyed<'a> = hash::Map<Box<[u8]>, Entry<'a>>;

type Item<'a> = (Found<'a>, Option<ObjRef>);

type Prefix<'a> = (Vec<Item<'a>>, bool);

struct Read<'a> {
    pub(crate) xref: &'a XRef,
    objects: RefCell<hash::Map<ObjRef, Found<'a>>>,
    entries: RefCell<hash::Map<Location, Keyed<'a>>>,
    prefixes: RefCell<hash::Map<Location, Prefix<'a>>>,
    places: RefCell<hash::Map<ObjRef, Option<Place>>>,
    derived: RefCell<hash::Map<(Location, TypeId), Made>>,
}

type Made = Rc<dyn Any>;

pub(crate) trait Derived: 'static {
    fn derive(items: &Items<'_>) -> Self;
}

impl<'a> Read<'a> {
    fn kept(&self, r: ObjRef) -> Found<'a> {
        let known = self.objects.borrow().get(&r).cloned();
        known.unwrap_or_else(|| {
            let found = Found::of(self.xref.get(r.into()));
            self.objects.borrow_mut().insert(r, found.clone());
            found
        })
    }
}
#[derive(Clone)]
enum Found<'a> {
    Object(Object<'a>),
    String(Rc<hayro_syntax::object::String<'a>>),
    Name(Rc<Name<'a>>),
    None,
}

impl<'a> Found<'a> {
    fn of(object: Option<Object<'a>>) -> Self {
        match object {
            Some(Object::String(s)) => Self::String(Rc::new(s)),
            Some(Object::Name(n)) => Self::Name(Rc::new(n)),
            Some(object) => Self::Object(object),
            None => Self::None,
        }
    }
}

#[derive(Clone)]
struct Entry<'a> {
    found: Found<'a>,
    r: Option<ObjRef>,
    written: bool,
}

impl Entry<'_> {
    fn present(&self) -> bool {
        self.written && (self.r.is_some() || !matches!(self.found, Found::Object(Object::Null(_))))
    }
}

fn place(data: &[u8]) -> Place {
    (data.as_ptr() as usize, data.len())
}

impl<'a> Objects<'a> {
    pub(crate) fn new(xref: &'a XRef) -> Self {
        Self {
            read: Some(Rc::new(Read {
                xref,
                objects: RefCell::default(),
                entries: RefCell::default(),
                prefixes: RefCell::default(),
                places: RefCell::default(),
                derived: RefCell::default(),
            })),
            shared: false,
            owner: None,
        }
    }

    fn found(&self, read: &Read<'a>, entry: Option<MaybeRef<Object<'a>>>) -> (Found<'a>, Self) {
        match entry {
            Some(MaybeRef::Ref(r)) => {
                let found = read.kept(r);
                let shared = Self {
                    read: self.read.clone(),
                    shared: true,
                    owner: Some(r.into()),
                };
                (found, shared)
            }
            Some(MaybeRef::NotRef(o)) => (Found::of(Some(o)), self.clone()),
            None => (Found::None, self.clone()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Id {
    Object(ObjRef),
    Written(Location),
}

pub(crate) enum Annots<'a> {
    Object(ObjRef, Objects<'a>),
    Written(Array<'a>, Objects<'a>),
}

pub(crate) struct Listed<'a> {
    pub(crate) index: usize,
    pub(crate) id: Id,
    named: Named<'a>,
}

enum Named<'a> {
    Object(ObjRef),
    Written(Dict<'a>),
}

impl<'a> Objects<'a> {
    pub(crate) fn annots(&self, page: &Dict<'a>) -> Option<Annots<'a>> {
        match page.get_raw::<Object<'a>>(b"Annots")? {
            MaybeRef::Ref(r) => Some(Annots::Object(r, self.clone())),
            MaybeRef::NotRef(Object::Array(a)) => Some(Annots::Written(
                a,
                Self {
                    owner: page.obj_id(),
                    ..self.clone()
                },
            )),
            MaybeRef::NotRef(_) => None,
        }
    }

    pub(crate) fn catalog(&self) -> Option<Entries<'a>> {
        let read = self.read.as_ref()?;
        let root = read.xref.get::<Dict<'a>>(read.xref.root_id())?;
        let objects = Self {
            read: self.read.clone(),
            shared: true,
            owner: root.obj_id(),
        };
        Some(Entries(root, objects))
    }

    pub(crate) fn annotation(&self, listed: &Listed<'a>) -> Option<Entries<'a>> {
        let dict = match &listed.named {
            Named::Written(dict) => dict.clone(),
            Named::Object(r) => {
                let read = self.read.as_ref()?;
                read.xref.get::<Dict<'a>>((*r).into())?
            }
        };
        // An indirect /Annots array can refer back into the page tree.
        if matches!(
            dict.get::<Name<'_>>(b"Type").as_deref(),
            Some(b"Page" | b"Pages" | b"Catalog")
        ) {
            return None;
        }
        Some(Entries(
            dict.clone(),
            Self {
                owner: dict.obj_id(),
                ..self.clone()
            },
        ))
    }

    fn own(&self, dict: &Dict<'a>) -> Option<bool> {
        let (Some(read), Some(id)) = (&self.read, dict.obj_id()) else {
            return Some(true);
        };
        let r = ObjRef::from(id);
        let at = place(dict.data());
        let known = read.places.borrow().get(&r).copied();
        let outer = known.unwrap_or_else(|| {
            let outer = match read.kept(r) {
                Found::Object(Object::Dict(d)) => Some(place(d.data())),
                Found::Object(Object::Array(a)) => Some(place(a.data())),
                Found::Object(Object::Stream(s)) => Some(place(s.dict().data())),
                _ => None,
            };
            read.places.borrow_mut().insert(r, outer);
            outer
        });
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
    pub(crate) fn id(&self) -> Id {
        match self {
            Self::Object(r, _) => Id::Object(*r),
            Self::Written(a, o) => Id::Written(location(o.owner, a.data())),
        }
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = Listed<'a>> + use<'a> {
        let array = match self {
            Self::Written(a, _) => Some(a.clone()),
            Self::Object(r, objects) => objects.read.as_ref().and_then(|read| {
                match objects.found(read, Some(MaybeRef::Ref(*r))).0 {
                    Found::Object(Object::Array(a)) => Some(a),
                    _ => None,
                }
            }),
        };
        array
            .into_iter()
            .flat_map(|a| a.dictionary_entries())
            .map(|(index, entry)| {
                let (id, named) = match entry {
                    MaybeRef::Ref(r) => (Id::Object(r), Named::Object(r)),
                    MaybeRef::NotRef(d) => (
                        Id::Written(location(d.obj_id(), d.data())),
                        Named::Written(d),
                    ),
                };
                Listed { index, id, named }
            })
    }
}

#[derive(Clone)]
pub(crate) enum Value<'a> {
    Number(Number),
    Bool(bool),
    Name(Rc<Name<'a>>),
    String(Rc<hayro_syntax::object::String<'a>>),
    Array(Items<'a>),
    Dict(Entries<'a>),
    Stream(StreamEntries<'a>),
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
            Object::Name(n) => Self::Name(Rc::new(n)),
            Object::String(s) => Self::String(Rc::new(s)),
            Object::Array(a) => Self::Array(Items(a, objects.clone())),
            Object::Dict(d) => Self::Dict(Entries(d, objects.clone())),
            Object::Stream(s) => Self::Stream(StreamEntries(s, objects.clone())),
            Object::Null(_) => Self::Null,
        }
    }

    fn read(object: Option<Object<'a>>) -> Self {
        Self::of(Found::of(object), &Objects::default())
    }

    pub(crate) fn number(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(real(*n)),
            _ => None,
        }
    }

    pub(crate) fn int(&self) -> Option<i32> {
        match self {
            Self::Number(n) => Some(int(*n)),
            _ => None,
        }
    }

    pub(crate) fn flags(&self) -> u32 {
        self.int().map_or(0, |i| i as u32)
    }

    pub(crate) fn name(&self) -> Option<&[u8]> {
        match self {
            Self::Name(n) => Some(n),
            _ => None,
        }
    }

    pub(crate) fn is_text(&self) -> bool {
        matches!(self, Self::String(_))
    }

    pub(crate) fn text_len(&self) -> usize {
        match self {
            Self::String(s) => s.as_bytes().len(),
            _ => 0,
        }
    }

    pub(crate) fn text(&self) -> Option<Vec<u8>> {
        match self {
            Self::String(s) => Some(s.as_bytes().to_vec()),
            _ => None,
        }
    }

    pub(crate) fn field_text(&self) -> Result<Option<Vec<u8>>, Unsupported> {
        let stream = match self {
            Self::String(s) => return Ok(Some(s.as_bytes().to_vec())),
            Self::Stream(s) => &s.0,
            _ => return Ok(None),
        };
        stream
            .decoded_field_value()
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

#[derive(Clone)]
pub(crate) struct Items<'a>(Array<'a>, Objects<'a>);

impl<'a> Items<'a> {
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
            None => (
                None,
                Some(self.0.iter::<Object<'a>>().map(|o| Value::read(Some(o)))),
            ),
        };
        with_objects
            .into_iter()
            .flatten()
            .chain(without.into_iter().flatten())
    }

    pub(crate) fn derived<T: Derived>(&self) -> Rc<T> {
        let Some(read) = &self.1.read else {
            return Rc::new(T::derive(self));
        };
        let key = (location(self.1.owner, self.0.data()), TypeId::of::<T>());
        let known = read.derived.borrow().get(&key).cloned();
        let made = known.unwrap_or_else(|| {
            let made = Rc::new(T::derive(self)) as Rc<dyn Any>;
            read.derived.borrow_mut().insert(key, made.clone());
            made
        });
        // Kept under the type it was made as.
        made.downcast::<T>().expect("made as T")
    }

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

    pub(crate) fn head(&self, n: usize) -> Vec<Value<'a>> {
        self.prefix(n).into_iter().map(|(v, _)| v).collect()
    }

    fn prefix(&self, n: usize) -> Vec<(Value<'a>, Option<ObjRef>)> {
        let Some(read) = self.shared_read() else {
            let refs = self.0.raw_iter().map(|e| e.as_obj_ref());
            return self.iter().zip(refs).take(n).collect();
        };
        let at = location(self.1.owner, self.0.data());
        let known = read.prefixes.borrow().get(&at).cloned();
        let found = match known {
            Some((found, all)) if all || found.len() >= n => found,
            _ => {
                let (found, all) = {
                    let mut entries = self.0.raw_iter();
                    let found: Vec<Item<'a>> = entries
                        .by_ref()
                        .take(n)
                        .map(|e| (e.as_obj_ref(), e))
                        .map(|(r, e)| (self.1.found(read, Some(e)).0, r))
                        .collect();
                    let all = found.len() < n || entries.next().is_none();
                    (found, all)
                };
                read.prefixes.borrow_mut().insert(at, (found.clone(), all));
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

    fn shared_read(&self) -> Option<&Rc<Read<'a>>> {
        self.1.read.as_ref().filter(|_| self.1.shared)
    }

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

    pub(crate) fn get(&self, i: usize) -> Value<'a> {
        self.entry(i).0
    }

    pub(crate) fn numbers(&self) -> Vec<f64> {
        self.iter().map(|v| v.number().unwrap_or(0.0)).collect()
    }

    pub(crate) fn first(&self, n: usize) -> Numbers {
        let number = |v: &Value<'a>| v.number().unwrap_or(0.0);
        if self.shared_read().is_none() {
            // As `prefix` reads them, without its list.
            return self.iter().take(n).map(|v| number(&v)).collect();
        }
        self.prefix(n).iter().map(|(v, _)| number(v)).collect()
    }

    fn held(&self, r: Option<ObjRef>, i: usize) -> Held {
        match (r, &self.1.read) {
            (Some(r), Some(_)) => Held::Object(r),
            // Read without objects: no document to find a body in.
            (Some(r), None) => Held::Unread(r),
            (None, _) => Held::Item(location(self.1.owner, self.0.data()), i),
        }
    }

    pub(crate) fn labels(&self) -> Vec<Label<'a>> {
        self.0
            .raw_iter()
            .zip(self.iter())
            .enumerate()
            .map(|(i, (raw, value))| {
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

pub(crate) struct Label<'a> {
    pub(crate) source: Source,
    pub(crate) value: Value<'a>,
}

impl<'a> Entries<'a> {
    #[cfg(test)]
    pub(crate) fn detached(dict: Dict<'a>) -> Self {
        Self(dict, Objects::default())
    }

    pub(crate) fn shared(&self) -> Self {
        let objects = Objects {
            read: self.1.read.clone(),
            shared: true,
            owner: self.1.owner,
        };
        Self(self.0.clone(), objects)
    }

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
        let at = location(self.1.owner, self.0.data());
        let known = read
            .entries
            .borrow()
            .get(&at)
            .and_then(|entries| entries.get(key).cloned());
        let entry = match known {
            Some(entry) => entry,
            None => {
                let (entry, _) = fresh();
                read.entries
                    .borrow_mut()
                    .entry(at)
                    .or_default()
                    .insert(key.into(), entry.clone());
                entry
            }
        };
        // Every entry of a shared object is read with the objects of shared ones.
        (entry, self.1.clone())
    }

    pub(crate) fn shown_in(&self, xref: &XRef, content: &mut OcgState) -> bool {
        content.shows(xref, &self.0)
    }

    pub(crate) fn get(&self, key: &[u8]) -> Value<'a> {
        let (entry, objects) = self.entry(key);
        Value::of(entry.found, &objects)
    }

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
            Value::Name(n) => Some((*n).clone()),
            _ => None,
        }
    }

    pub(crate) fn is(&self, key: &[u8], name: &[u8]) -> bool {
        self.name(key).is_some_and(|n| *n == *name)
    }

    pub(crate) fn text(&self, entry: Text) -> Option<Vec<u8>> {
        self.get(entry.key()).text()
    }

    pub(crate) fn text_len(&self, entry: Text) -> usize {
        self.get(entry.key()).text_len()
    }

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

    pub(crate) fn numbers(&self, key: &[u8], len: usize) -> Option<Numbers> {
        let mut n = self.items(key)?.first(len);
        n.resize(len, 0.0);
        Some(n)
    }

    pub(crate) fn matrix(&self, key: &[u8]) -> [f64; 6] {
        match self.numbers(key, 6) {
            Some(m) => [m[0], m[1], m[2], m[3], m[4], m[5]],
            None => [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        }
    }

    pub(crate) fn rect(&self, key: &[u8]) -> Option<kurbo::Rect> {
        let n = self.numbers(key, 4)?;
        Some(kurbo::Rect::new(n[0], n[1], n[2], n[3]).abs())
    }

    pub(crate) fn source(&self, key: &'static [u8]) -> Source {
        let entry = self.entry(key).0;
        let held = match (entry.r, &self.1.read) {
            (Some(r), Some(_)) => Held::Object(r),
            // Read without objects: no document to find a body in.
            (Some(r), None) => Held::Unread(r),
            (None, _) if !entry.present() => Held::Absent,
            (None, _) => Held::In(self.location()),
        };
        Source { held, key }
    }

    pub(crate) fn location(&self) -> Location {
        location(self.1.owner, self.0.data())
    }

    pub(crate) fn box_and_matrix(&self) -> Option<([f64; 4], [f64; 6])> {
        let b = self.numbers(b"BBox", 4)?;
        Some(([b[0], b[1], b[2], b[3]], self.matrix(b"Matrix")))
    }

    pub(crate) fn own(&self) -> Option<bool> {
        self.1.own(&self.0)
    }

    pub(crate) fn parent(&self) -> Option<Self> {
        self.dict(b"Parent")
    }
}

#[derive(Clone)]
pub(crate) struct StreamEntries<'a>(Stream<'a>, Objects<'a>);

impl<'a> StreamEntries<'a> {
    pub(crate) fn dict(&self) -> Entries<'a> {
        Entries(self.0.dict().clone(), self.1.clone())
    }

    pub(crate) fn into_stream(self) -> Stream<'a> {
        self.0
    }
}
