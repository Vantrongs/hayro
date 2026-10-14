//! The entries of annotation dictionaries, and the boxes and matrices of the forms,
//! patterns and shadings they and pages draw, read as viewers read damaged ones.
//!
//! Generators read an annotation only through [`Entries`], which offers no typed read
//! that drops an array's entry: every entry keeps its place, and one that is not a
//! number, is null, or refers to no object reads as 0 where a number is wanted, so the
//! coordinates and colour components after it keep their meaning.

use hayro_syntax::object::{Array, Dict, Name, Number, ObjRef, Object};
use hayro_syntax::xref::XRef;

/// A dictionary an annotation is drawn from: the annotation, one it refers to (its
/// border style, its appearance characteristics, an appearance stream's), or a field
/// above a widget.
#[derive(Clone)]
pub(crate) struct Entries<'a>(Dict<'a>);

/// The value of an entry or of an array's entry.
pub(crate) enum Value<'a> {
    Number(Number),
    Bool(bool),
    Name(Name<'a>),
    String(hayro_syntax::object::String<'a>),
    Array(Items<'a>),
    Dict(Entries<'a>),
    /// A stream's decoded data.
    Data(Vec<u8>),
    /// Null, a reference to no object, a stream that cannot be decoded, or no entry.
    Null,
}

impl<'a> Value<'a> {
    fn of(object: Option<Object<'a>>) -> Self {
        match object {
            Some(Object::Number(n)) => Self::Number(n),
            Some(Object::Boolean(b)) => Self::Bool(b),
            Some(Object::Name(n)) => Self::Name(n),
            Some(Object::String(s)) => Self::String(s),
            Some(Object::Array(a)) => Self::Array(Items(a)),
            Some(Object::Dict(d)) => Self::Dict(Entries(d)),
            Some(Object::Stream(s)) => match s.decoded() {
                Ok(data) => Self::Data(data.into_owned()),
                Err(_) => Self::Null,
            },
            Some(Object::Null(_)) | None => Self::Null,
        }
    }

    pub(crate) fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub(crate) fn number(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(n.as_f64()),
            _ => None,
        }
    }

    /// A number as an integer, a real one truncated.
    pub(crate) fn int(&self) -> Option<i64> {
        match self {
            Self::Number(n) => Some(n.as_i64()),
            _ => None,
        }
    }

    pub(crate) fn name(&self) -> Option<&[u8]> {
        match self {
            Self::Name(n) => Some(n.as_ref()),
            _ => None,
        }
    }

    /// A string's bytes.
    pub(crate) fn string(&self) -> Option<&[u8]> {
        match self {
            Self::String(s) => Some(s.as_bytes()),
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
pub(crate) struct Items<'a>(Array<'a>);

impl<'a> Items<'a> {
    /// The entries in order; one that refers to no object is [`Value::Null`].
    pub(crate) fn iter(&self) -> impl Iterator<Item = Value<'a>> + '_ {
        self.0.items::<Object<'a>>().map(Value::of)
    }

    /// Entry `i`; [`Value::Null`] beyond the end.
    pub(crate) fn get(&self, i: usize) -> Value<'a> {
        self.iter().nth(i).unwrap_or(Value::Null)
    }

    /// The entries as numbers, one that is not a number read as 0.
    pub(crate) fn numbers(&self) -> Vec<f64> {
        self.iter().map(|v| v.number().unwrap_or(0.0)).collect()
    }
}

impl<'a> Entries<'a> {
    pub(crate) fn new(dict: Dict<'a>) -> Self {
        Self(dict)
    }

    /// Entry `key`; [`Value::Null`] when there is none.
    pub(crate) fn get(&self, key: &[u8]) -> Value<'a> {
        Value::of(self.0.get::<Object<'a>>(key))
    }

    /// Whether entry `key` is there and not null (PDF 32000-1 7.3.9).
    pub(crate) fn has(&self, key: &[u8]) -> bool {
        !self.get(key).is_null()
    }

    pub(crate) fn number(&self, key: &[u8]) -> Option<f64> {
        self.get(key).number()
    }

    pub(crate) fn int(&self, key: &[u8]) -> Option<i64> {
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

    /// The bytes of string `key`.
    pub(crate) fn string(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.get(key).string().map(<[u8]>::to_vec)
    }

    /// The bytes of string `key`, or the decoded data of stream `key`.
    pub(crate) fn text(&self, key: &[u8]) -> Option<Vec<u8>> {
        match self.get(key) {
            Value::String(s) => Some(s.as_bytes().to_vec()),
            Value::Data(data) => Some(data),
            _ => None,
        }
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

    /// The entries of array `key` as numbers (see [`Items::numbers`]), with 0 for
    /// those it lacks up to `len`; `None` when `key` is not an array.
    pub(crate) fn numbers(&self, key: &[u8], len: usize) -> Option<Vec<f64>> {
        let mut n = self.items(key)?.numbers();
        if n.len() < len {
            n.resize(len, 0.0);
        }
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

    /// The object this dictionary is, when it is one of its own.
    pub(crate) fn id(&self) -> Option<ObjRef> {
        self.0.obj_id().map(ObjRef::from)
    }

    /// The dictionary `/Parent` names, and the reference to it when it is an object of
    /// its own. One read through a reference is read afresh from `xref`, so that the
    /// depth it lies at costs nothing to read it.
    pub(crate) fn parent(&self, xref: &'a XRef) -> Option<(Option<ObjRef>, Self)> {
        match self.0.get_ref(b"Parent") {
            Some(r) => xref.get::<Dict<'a>>(r.into()).map(|d| (Some(r), Self(d))),
            None => self.dict(b"Parent").map(|d| (None, d)),
        }
    }
}
