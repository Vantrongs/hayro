//! The entries of annotation dictionaries, and the boxes and matrices of the forms,
//! patterns and shadings they and pages draw, read as viewers read damaged ones.
//!
//! Generators read an annotation only through [`Entries`], which offers no typed read
//! that drops an array's entry: every entry keeps its place, and one that is not a
//! number, is null, or refers to no object reads as 0 where a number is wanted, so the
//! coordinates and colour components after it keep their meaning.
//!
//! Numbers are read as `MuPDF` reads them, which is what the files are checked against
//! (observed in `PyMuPDF` 1.28.2, which writes back the numbers it read): an integer as
//! a 64-bit integer; a real as a 32-bit float of its integer part's low 32 bits and
//! its fraction, so that `4294967297.5` is 1.5 and a digit string of any length
//! fits; an integer entry of a real rounded half up, and `i32::MIN` beyond 32 bits.
//! A real whose integer part a 64-bit float no longer holds exactly (2^53 or more)
//! cannot be read so; reading one makes [`read`] fail with [`Unsupported::Number`].

use super::Unsupported;
use hayro_syntax::object::{Array, Dict, Name, Number, ObjRef, Object, Stream};
use hayro_syntax::xref::XRef;
use std::cell::Cell;

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

/// 2^53: from here on a 64-bit float does not hold every integer.
const EXACT: f64 = 9_007_199_254_740_992.0;

/// `n` as `MuPDF` reads a number where a real is wanted (see the module).
pub(crate) fn real(n: Number) -> f64 {
    if n.is_integer() {
        // As a float, as `MuPDF` converts one.
        return f64::from(n.as_i64() as f32);
    }
    let r = n.as_f64();
    if r.is_nan() || r.abs() >= EXACT {
        UNREADABLE.with(|u| u.set(true));
        return 0.0;
    }
    let whole = r.abs().trunc();
    // Exact: both are below 2^53.
    let low = whole % 4_294_967_296.0;
    let v = (low + (r.abs() - whole)) as f32;
    f64::from(if r < 0.0 { -v } else { v })
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
pub(crate) struct Entries<'a>(Dict<'a>);

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
}

/// The value of an entry or of an array's entry.
pub(crate) enum Value<'a> {
    Number(Number),
    Bool(bool),
    Name(Name<'a>),
    String(hayro_syntax::object::String<'a>),
    Array(Items<'a>),
    Dict(Entries<'a>),
    /// A stream, decoded only where its text is read ([`Value::text`]).
    Stream(Stream<'a>),
    /// Null, a reference to no object, or no entry.
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
            Some(Object::Stream(s)) => Self::Stream(s),
            Some(Object::Null(_)) | None => Self::Null,
        }
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

    /// Whether entry `key` is there as viewers look an entry up: a null value is no
    /// entry, but a reference is one, even to no object (it then reads as null).
    pub(crate) fn present(&self, key: &[u8]) -> bool {
        match self.0.get::<Object<'a>>(key) {
            Some(Object::Null(_)) => false,
            Some(_) => true,
            None => self.0.contains_key(key),
        }
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
