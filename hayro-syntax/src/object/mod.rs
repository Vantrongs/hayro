//! Parsing and reading from PDF objects.
//!
//! An object is read from its tokens by these rules, alike where it is read from
//! bytes, parsed as the body of an object a table lists, skipped, or met by the scan
//! that rebuilds a table. Numbers are tokens by the rules of the `number` module.
//!
//! 1. `<<` starts a dictionary and `[` an array. Any other token is one object: a
//!    name (`/` and the regular characters after it), a string (`(` or `<`), a
//!    number, or a word of regular characters, which is `true`, `false` or `null`
//!    only as a whole word.
//! 2. Outside a content stream, a word that is none of those three (`truefoo`, `R`,
//!    `obj`) and a token that starts like a number but is none (`1e3`, `0R`) are
//!    each `null`, up to the next white space or delimiter: as an item of an array,
//!    a value of a dictionary, or an object of its own.
//! 3. In a content stream, a word in an array or dictionary is `null` too, but a
//!    token that starts like a number and is none cannot be read (outside an array
//!    or dictionary, the content reader then reads it as an operator).
//! 4. Nor can a delimiter that starts no object where an object is to be read
//!    (`)`, `{`, `}`, a lone `>`, a `>>` or a `]` that ends nothing), nor a string
//!    that cannot be read. An array or dictionary holding a token that cannot be
//!    read fails whole, and so does each one around it.
//! 5. An array is `[`, then items, until the first `]`. An item is a reference
//!    where two numbers that read as 32-bit integers (each toward zero) and `R`
//!    follow one another, separated by white space only, not by a comment; else
//!    an object. `R` ends a reference whatever follows it: `1 0 Rx` is a reference
//!    and the word `x`; `1 0 obj` and `4294967297 0 R` are three items each.
//! 6. A dictionary is `<<`, then entries until `>>`, each a key, which is a name,
//!    and a value, read as an item of an array is. A token in a key's place that
//!    is no name is read as one object and passed over, and the next token is read
//!    as a key. A key without a value before `>>` fails the dictionary. A later
//!    entry of a key replaces an earlier one; a `null` value is kept as an entry.
//! 7. Arrays and dictionaries nest up to 64 deep: an object deeper than that fails,
//!    with each one around it.
//! 8. What follows the end of an object is not read.
//!
//! `PyMuPDF` 1.28.2 reads the same items and entries but where rule 4 meets a
//! delimiter in an array or a value's place (null there), where rule 5 meets a
//! comment between a reference's numbers or a number outside 32 bits (a reference
//! there), a real as its first number or `R` after a reference (the object fails),
//! or `Rx` (a word, `null`), and where rule 6 passes over a token in a key's place
//! (the dictionary fails), or meets `0R` after a value (which it reads as `null`).
//! It fails to read an object of its own that rule 2 makes `null`. The tests in
//! `tests/numbers_and_objects.rs` pin these rules and name which differences are
//! deliberate.

pub use crate::object::array::Array;
pub use crate::object::date::DateTime;
pub use crate::object::dict::Dict;
pub use crate::object::name::Name;
pub use crate::object::null::Null;
pub use crate::object::number::Number;
pub(crate) use crate::object::number::starts_number;
pub use crate::object::rect::Rect;
pub use crate::object::r#ref::{MaybeRef, ObjRef};
pub use crate::object::stream::Stream;
pub use crate::object::string::String;
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};
use crate::sync::Arc;
use crate::trivia::is_regular_character;
use alloc::vec::Vec;
use core::fmt::{Debug, Display, Formatter};
use smallvec::SmallVec;

pub(crate) const MAX_OBJECT_NESTING_DEPTH: usize = 64;

mod bool;
mod date;
mod null;
mod number;
mod rect;
mod r#ref;
mod string;
mod tuple;

pub(crate) mod indirect;
pub(crate) mod name;
pub(crate) mod parsed;

pub mod array;
pub mod dict;
pub mod stream;

/// The bytes of a string or name: borrowed from the file, or decoded from it, those
/// few enough held in place and others shared by every clone, so that cloning an
/// object costs the same however large it is, and a value many pages inherit is held
/// once.
#[derive(Clone)]
pub(crate) enum Bytes<'a> {
    Borrowed(&'a [u8]),
    Inline(SmallVec<[u8; 23]>),
    Shared(Arc<[u8]>),
}

impl Bytes<'_> {
    /// Bytes decoded into `bytes`.
    pub(crate) fn decoded(bytes: SmallVec<[u8; 23]>) -> Self {
        if bytes.spilled() {
            Self::Shared(Arc::from(bytes.as_slice()))
        } else {
            Self::Inline(bytes)
        }
    }
}

impl<'a> Bytes<'a> {
    /// The bytes borrowed from what they were read from, or else as they are held,
    /// which no data is needed for.
    pub(crate) fn held(self) -> Result<&'a [u8], Bytes<'static>> {
        match self {
            Self::Borrowed(bytes) => Ok(bytes),
            Self::Inline(bytes) => Err(Bytes::Inline(bytes)),
            Self::Shared(bytes) => Err(Bytes::Shared(bytes)),
        }
    }
}

impl From<Vec<u8>> for Bytes<'_> {
    fn from(bytes: Vec<u8>) -> Self {
        Self::decoded(SmallVec::from_vec(bytes))
    }
}

impl AsRef<[u8]> for Bytes<'_> {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Borrowed(bytes) => bytes,
            Self::Inline(bytes) => bytes,
            Self::Shared(bytes) => bytes,
        }
    }
}

/// A trait for PDF objects.
pub(crate) trait ObjectLike<'a>: TryFrom<Object<'a>> + Readable<'a> + Debug + Clone {
    /// Whether it is one entry of an array; a tuple is several.
    const ONE_ENTRY: bool = true;

    /// Whether a stream is read as one, its data and all: a stream whose data cannot
    /// be read is lost to such a read, where any other reads its dictionary or is
    /// of another kind.
    const STREAM: bool = false;

    /// The one kind of object that is read as one, where there is one: an object
    /// whose first token tells it is of another kind ([`Kind::of_token`]) is no such
    /// object, and is not read as one.
    const KIND: Option<Kind> = None;

    /// Reads the object an indirect object holds, after its header.
    fn read_indirect(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        r.read_with_context::<Self>(ctx)
    }

    /// Reads the object the items of a parsed array start with, as [`Readable::read`]
    /// reads it from the array's bytes: one item, which is no reference.
    fn from_items(items: &mut parsed::Items<'_, 'a>, _: &ReaderContext<'a>) -> Option<Self> {
        items.next_as::<Self>()
    }
}

/// A trait for PDF objects that can be borrowed directly from an [`Object`].
pub(crate) trait ObjectRefLike<'a>: Sized {
    fn cast_ref<'b>(obj: &'b Object<'a>) -> Option<&'b Self>;
}

/// An object as the file writes it, for a copy that is to be read as its source is
/// ([`Dict::written`], [`Array::written`], [`crate::xref::XRef::written`]): a number
/// is the bytes it is written as, which its value does not tell apart where readers
/// read them otherwise (`PyMuPDF` 1.28.2 reads `4294967297` as 1 where it reads an
/// integer, and `--5` as -5, hayro the first as 4294967297 and the second as 0, and
/// `5.0` is a real to both), and an
/// item of an array or a value of a dictionary that is a reference is the reference.
#[derive(Debug, Clone, PartialEq)]
pub enum Written<'a> {
    /// A reference, which only an item or a value is.
    Ref(ObjRef),
    /// The null object.
    Null,
    /// A boolean.
    Boolean(bool),
    /// A number, as the bytes it is written as: a sign, digits, points and minuses.
    Number(&'a [u8]),
    /// A string, decoded and decrypted.
    String(String<'a>),
    /// A name, decoded.
    Name(Name<'a>),
    /// A dictionary.
    Dict(Dict<'a>),
    /// An array.
    Array(Array<'a>),
    /// A stream, which only an object of its own is.
    Stream(Stream<'a>),
}

impl<'a> Written<'a> {
    /// The item `r` is at, read in `ctx` as a `MaybeRef<Object>` is read, after
    /// white space and comments.
    pub(crate) fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        r.skip_white_spaces_and_comments();
        let at = r.offset();
        Some(match r.read_with_context::<MaybeRef<Object<'a>>>(ctx)? {
            MaybeRef::Ref(reference) => Self::Ref(reference),
            MaybeRef::NotRef(object) => Self::of(object, r.range(at..r.offset())?),
        })
    }

    /// `object`, read from `bytes`.
    pub(crate) fn of(object: Object<'a>, bytes: &'a [u8]) -> Self {
        match object {
            Object::Null(_) => Self::Null,
            Object::Boolean(value) => Self::Boolean(value),
            Object::Number(_) => Self::Number(bytes),
            Object::String(string) => Self::String(string),
            Object::Name(name) => Self::Name(name),
            Object::Dict(dict) => Self::Dict(dict),
            Object::Array(array) => Self::Array(array),
            Object::Stream(stream) => Self::Stream(stream),
        }
    }
}

/// The kind of an object: the variant of [`Object`] it reads as. Read as an object,
/// it is the kind of the object there, which is passed over, not read: a dictionary
/// or an array of any size costs its bytes, looked at, not what reading it makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The null object.
    Null,
    /// A boolean.
    Boolean,
    /// A number.
    Number,
    /// A string.
    String,
    /// A name.
    Name,
    /// A dictionary.
    Dict,
    /// An array.
    Array,
    /// A stream, which only an object of its own is.
    Stream,
}

impl Kind {
    /// The kind of the object `r` is at, where its first token alone tells it, as
    /// [`Object::read`] reads it (rule 1 of the module doc): a dictionary, an array,
    /// a name or a string. `None` for another token: a number, which may start a
    /// reference or be `null` (rule 2), or a word.
    #[inline]
    pub(crate) fn of_token(r: &Reader<'_>) -> Option<Self> {
        match r.peek_byte()? {
            b'<' if r.peek_bytes(2) == Some(b"<<") => Some(Self::Dict),
            b'[' => Some(Self::Array),
            b'/' => Some(Self::Name),
            b'(' | b'<' => Some(Self::String),
            _ => None,
        }
    }
}

impl<'a> Readable<'a> for Kind {
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let in_content_stream = ctx.in_content_stream();
        match Self::of_token(r) {
            Some(kind @ (Self::Dict | Self::Array)) => {
                r.skip::<Object<'_>>(in_content_stream)?;
                Some(kind)
            }
            _ => leaf(r, in_content_stream, KindOf),
        }
    }
}

impl TryFrom<Object<'_>> for Kind {
    type Error = ();

    fn try_from(value: Object<'_>) -> Result<Self, Self::Error> {
        Ok(match value {
            Object::Null(_) => Self::Null,
            Object::Boolean(_) => Self::Boolean,
            Object::Number(_) => Self::Number,
            Object::String(_) => Self::String,
            Object::Name(_) => Self::Name,
            Object::Dict(_) => Self::Dict,
            Object::Array(_) => Self::Array,
            Object::Stream(_) => Self::Stream,
        })
    }
}

// Read where an object is: a stream is one, as it is to an `Object`.
impl ObjectLike<'_> for Kind {
    const STREAM: bool = true;
}

/// Makes the kind of a token [`leaf`] reads, which it passes over.
struct KindOf;

impl<'a> Leaf<'a> for KindOf {
    type Out = Kind;
    fn null(&self) -> Kind {
        Kind::Null
    }
    fn boolean(&self, _: bool) -> Kind {
        Kind::Boolean
    }
    fn name(&self, _: &'a [u8]) -> Option<Kind> {
        Some(Kind::Name)
    }
    fn string(&self, r: &mut Reader<'a>, is_content_stream: bool) -> Option<Kind> {
        String::skip(r, is_content_stream).map(|()| Kind::String)
    }
    fn number(&self, r: &mut Reader<'a>) -> Option<Kind> {
        Skip.number(r).map(|()| Kind::Number)
    }
}

/// A primitive PDF object.
#[derive(Debug, Clone, PartialEq)]
pub enum Object<'a> {
    /// A null object.
    Null(Null),
    /// A boolean object.
    Boolean(bool),
    /// A number object.
    Number(Number),
    /// A string object.
    String(String<'a>),
    /// A name object.
    Name(Name<'a>),
    /// A dict object.
    Dict(Dict<'a>),
    /// An array object.
    Array(Array<'a>),
    /// A stream object.
    // Can only be an indirect object in theory and thus comes with some caveats,
    // but we just treat it the same.
    Stream(Stream<'a>),
}

impl Display for Object<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Null(value) => Display::fmt(value, f),
            Self::Boolean(value) => Display::fmt(value, f),
            Self::Number(value) => Display::fmt(value, f),
            Self::String(value) => Display::fmt(value, f),
            Self::Name(value) => Display::fmt(value, f),
            Self::Dict(value) => Display::fmt(value, f),
            Self::Array(value) => Display::fmt(value, f),
            Self::Stream(value) => Display::fmt(value, f),
        }
    }
}

impl<'a> Object<'a> {
    /// Try casting the object to a specific subtype.
    pub(crate) fn cast<T>(self) -> Option<T>
    where
        T: ObjectLike<'a>,
    {
        self.try_into().ok()
    }

    /// Try casting the object to a reference of the specific type.
    pub(crate) fn cast_ref<T>(&self) -> Option<&T>
    where
        T: ObjectRefLike<'a>,
    {
        T::cast_ref(self)
    }

    /// Try casting the object to a dict.
    #[inline(always)]
    pub fn into_dict(self) -> Option<Dict<'a>> {
        self.cast()
    }

    /// Try casting the object to a name.
    #[inline(always)]
    pub fn into_name(self) -> Option<Name<'a>> {
        self.cast()
    }

    /// Try casting the object to the null object.
    #[inline(always)]
    pub fn into_null(self) -> Option<Null> {
        self.cast()
    }

    /// Try casting the object to a bool.
    #[inline(always)]
    pub fn into_bool(self) -> Option<bool> {
        self.cast()
    }

    /// Try casting the object to a string.
    #[inline(always)]
    pub fn into_string(self) -> Option<String<'a>> {
        self.cast()
    }

    /// Try casting the object to a stream.
    #[inline(always)]
    pub fn into_stream(self) -> Option<Stream<'a>> {
        self.cast()
    }

    /// Try casting the object to an array.
    #[inline(always)]
    pub fn into_array(self) -> Option<Array<'a>> {
        self.cast()
    }

    /// Try casting the object to a u8.
    #[inline(always)]
    pub fn into_u8(self) -> Option<u8> {
        self.cast()
    }

    /// Try casting the object to a u16.
    #[inline(always)]
    pub fn into_u16(self) -> Option<u16> {
        self.cast()
    }

    /// Try casting the object to a f32.
    #[inline(always)]
    pub fn into_f32(self) -> Option<f32> {
        self.cast()
    }

    /// Try casting the object to a i32.
    #[inline(always)]
    pub fn into_i32(self) -> Option<i32> {
        self.cast()
    }

    /// Try casting the object to a number.
    #[inline(always)]
    pub fn into_number(self) -> Option<Number> {
        self.cast()
    }
}

impl<'a> ObjectLike<'a> for Object<'a> {
    const STREAM: bool = true;

    // Only an indirect object is a stream: a dictionary followed by `stream`, as
    // `body::Read::in_slot` in the `xref` module reads one. In an array or dictionary,
    // which reading and skipping read alike, it is a dictionary.
    fn read_indirect(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        match r.read_with_context::<Self>(ctx)? {
            Self::Dict(dict) => {
                let end = r.offset();
                r.skip_white_spaces_and_comments();
                let stream = r.forward_tag(b"stream").is_some();
                r.jump(end);
                if stream {
                    Stream::after_dict(r, dict).map(Self::Stream)
                } else {
                    Some(Self::Dict(dict))
                }
            }
            object => Some(object),
        }
    }
}
impl<'a> ObjectRefLike<'a> for Object<'a> {
    fn cast_ref(obj: &Self) -> Option<&Self> {
        Some(obj)
    }
}

// Skipping an object reads it as reading does: an array or a dictionary by the one
// reading both share, any other token by `leaf`.
impl Skippable for Object<'_> {
    fn skip(r: &mut Reader<'_>, is_content_stream: bool) -> Option<()> {
        Self::skip_with_depth(r, is_content_stream, 0)
    }

    fn skip_with_depth(r: &mut Reader<'_>, is_content_stream: bool, depth: usize) -> Option<()> {
        match r.peek_byte()? {
            b'<' if r.peek_bytes(2) == Some(b"<<") => {
                Dict::skip_with_depth(r, is_content_stream, depth)
            }
            b'[' => Array::skip_with_depth(r, is_content_stream, depth),
            _ => leaf(r, is_content_stream, Skip),
        }
    }
}

impl<'a> Readable<'a> for Object<'a> {
    #[inline]
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        match r.peek_byte()? {
            b'<' if r.peek_bytes(2) == Some(b"<<") => Some(Self::Dict(Dict::read(r, ctx)?)),
            b'[' => Some(Self::Array(Array::read(r, ctx)?)),
            _ => leaf(r, ctx.in_content_stream(), ctx),
        }
    }
}

/// What reading a token that is no array or dictionary makes: an object, read in
/// the context the reader holds, or nothing, when the token is only skipped.
pub(crate) trait Leaf<'a> {
    type Out;
    fn null(&self) -> Self::Out;
    fn boolean(&self, value: bool) -> Self::Out;
    /// A name, `/` and all, whose escapes are known to be sound.
    fn name(&self, name: &'a [u8]) -> Option<Self::Out>;
    fn string(&self, r: &mut Reader<'a>, is_content_stream: bool) -> Option<Self::Out>;
    fn number(&self, r: &mut Reader<'a>) -> Option<Self::Out>;
}

/// Skips a token, accepting what reading it does.
pub(crate) struct Skip;

impl<'a> Leaf<'a> for Skip {
    type Out = ();
    fn null(&self) {}
    fn boolean(&self, _: bool) {}
    fn name(&self, _: &'a [u8]) -> Option<()> {
        Some(())
    }
    fn string(&self, r: &mut Reader<'a>, is_content_stream: bool) -> Option<()> {
        String::skip(r, is_content_stream)
    }
    fn number(&self, r: &mut Reader<'a>) -> Option<()> {
        r.skip::<Number>(false).map(|_| ())
    }
}

impl<'a> Leaf<'a> for &ReaderContext<'a> {
    type Out = Object<'a>;
    fn null(&self) -> Object<'a> {
        Object::Null(Null)
    }
    fn boolean(&self, value: bool) -> Object<'a> {
        Object::Boolean(value)
    }
    fn name(&self, name: &'a [u8]) -> Option<Object<'a>> {
        Name::new(&name[1..]).map(Object::Name)
    }
    fn string(&self, r: &mut Reader<'a>, _: bool) -> Option<Object<'a>> {
        String::read(r, self).map(Object::String)
    }
    // Inlined into `leaf`, so that the number is made where the object is: called,
    // its object comes back through memory, which a load of it then waits on.
    #[inline(always)]
    fn number(&self, r: &mut Reader<'a>) -> Option<Object<'a>> {
        r.read_without_context::<Number>().map(Object::Number)
    }
}

/// Reads a token that is no array or dictionary, the one way reading, skipping, and
/// the scan that rebuilds a table read it; `make` says whether into an object or
/// into nothing.
///
/// A token that starts like a number but is none, such as `1e3`, is `null` up to the
/// next white space or delimiter, so that one bad token does not lose the object
/// around it; in a content stream it is none (rules 2 to 4 of the module doc).
#[inline(always)]
pub(crate) fn leaf<'a, L: Leaf<'a>>(
    r: &mut Reader<'a>,
    is_content_stream: bool,
    make: L,
) -> Option<L::Out> {
    let start = r.offset();
    match r.peek_byte()? {
        byte if starts_number(byte) => match make.number(r) {
            Some(number) => Some(number),
            None if is_content_stream => None,
            None => {
                r.jump(start);
                r.forward_while(is_regular_character);
                Some(make.null())
            }
        },
        b'/' => {
            name::skip_name_like(r, true)?;
            make.name(r.range(start..r.offset())?)
        }
        b'(' | b'<' => make.string(r, is_content_stream),
        byte if is_regular_character(byte) => {
            r.forward_while(is_regular_character);
            Some(match r.range(start..r.offset())? {
                b"true" => make.boolean(true),
                b"false" => make.boolean(false),
                _ => make.null(),
            })
        }
        // White space, or a delimiter that starts no object.
        _ => None,
    }
}

/// A trait for objects that can be parsed from a simple byte stream.
pub trait FromBytes<'a>: Sized {
    /// Try to read the object from the given bytes.
    fn from_bytes(b: &'a [u8]) -> Option<Self>;
}

impl<'a, T: Readable<'a>> FromBytes<'a> for T {
    fn from_bytes(b: &'a [u8]) -> Option<Self> {
        Self::from_bytes_impl(b)
    }
}

/// An identifier for a PDF object.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct ObjectIdentifier {
    /// The object number.
    pub obj_number: i32,
    /// The generation number.
    pub gen_number: i32,
}

impl ObjectIdentifier {
    /// Create a new `ObjectIdentifier`.
    pub fn new(obj_number: i32, gen_number: i32) -> Self {
        Self {
            obj_number,
            gen_number,
        }
    }
}

impl Readable<'_> for ObjectIdentifier {
    fn read(r: &mut Reader<'_>, _: &ReaderContext<'_>) -> Option<Self> {
        let obj_number = r.read_without_context::<i32>()?;
        r.skip_white_spaces_and_comments();
        let gen_number = r.read_without_context::<i32>()?;
        r.skip_white_spaces_and_comments();
        r.forward_tag(b"obj")?;
        Some(Self {
            obj_number,
            gen_number,
        })
    }
}

// As reading one reads it: checking a table, rebuilding it, and reading an object
// find the same headers.
impl Skippable for ObjectIdentifier {
    fn skip(r: &mut Reader<'_>, _: bool) -> Option<()> {
        r.read_without_context::<Self>().map(|_| ())
    }
}

/// A convenience function that extracts a dict and a stream from an object.
/// If the object is just a dictionary, it will return `None` for the stream.
/// If the object is a stream, it will return its dictionary as well as the stream
/// itself.
pub fn dict_or_stream<'a, 'b>(
    obj: &'b Object<'a>,
) -> Option<(&'b Dict<'a>, Option<&'b Stream<'a>>)> {
    if let Some(stream) = obj.cast_ref::<Stream<'a>>() {
        Some((stream.dict(), Some(stream)))
    } else {
        obj.cast_ref::<Dict<'a>>().map(|dict| (dict, None))
    }
}

mod macros {
    macro_rules! object {
        ($t:ident $(<$l:lifetime>),*, $s:ident) => {
            impl<'a> TryFrom<Object<'a>> for $t$(<$l>),* {
                type Error = ();

                fn try_from(value: Object<'a>) -> core::result::Result<Self, Self::Error> {
                    match value {
                        Object::$s(b) => Ok(b),
                        _ => Err(()),
                    }
                }
            }

            impl<'a> crate::object::ObjectLike<'a> for $t$(<$l>),* {
                const KIND: Option<crate::object::Kind> = Some(crate::object::Kind::$s);
            }

            impl<'a> crate::object::ObjectRefLike<'a> for $t$(<$l>),* {
                fn cast_ref<'b>(obj: &'b crate::object::Object<'a>) -> Option<&'b Self> {
                    match obj {
                        crate::object::Object::$s(b) => Some(b),
                        _ => None,
                    }
                }
            }
        };
    }

    pub(crate) use object;
}

#[cfg(test)]
mod tests {
    use crate::object::indirect::IndirectObject;
    use crate::object::{Array, Dict, Name, Null, Number, Object, Stream, String};
    use crate::reader::Reader;
    use crate::reader::{ReaderContext, ReaderExt};

    fn object_impl(data: &[u8]) -> Option<Object<'_>> {
        let mut r = Reader::new(data);
        r.read_with_context::<Object<'_>>(&ReaderContext::dummy())
    }

    #[test]
    fn null() {
        assert!(matches!(object_impl(b"null").unwrap(), Object::Null(_)));
    }

    #[test]
    fn bool() {
        assert!(matches!(object_impl(b"true").unwrap(), Object::Boolean(_)));
    }

    #[test]
    fn string_1() {
        assert!(matches!(object_impl(b"(Hi)").unwrap(), Object::String(_)));
    }

    #[test]
    fn string_2() {
        assert!(matches!(object_impl(b"<34>").unwrap(), Object::String(_)));
    }

    #[test]
    fn name() {
        assert!(matches!(object_impl(b"/Name").unwrap(), Object::Name(_)));
    }

    #[test]
    fn dict() {
        assert!(matches!(
            object_impl(b"<</Entry 45>>").unwrap(),
            Object::Dict(_)
        ));
    }

    #[test]
    fn array() {
        assert!(matches!(object_impl(b"[45]").unwrap(), Object::Array(_)));
    }

    fn indirect_impl(data: &[u8]) -> Option<Object<'_>> {
        let mut r = Reader::new(data);
        r.read_with_context::<IndirectObject<Object<'_>>>(&ReaderContext::dummy())
            .map(IndirectObject::get)
    }

    // Only an indirect object is a stream: in an array, as skipping it reads it, a
    // dictionary followed by `stream` is a dictionary and keywords.
    #[test]
    fn stream() {
        assert!(matches!(
            indirect_impl(b"1 0 obj << /Length 3 >> stream\nabc\nendstream endobj").unwrap(),
            Object::Stream(_)
        ));
        assert!(matches!(
            object_impl(b"<< /Length 3 >> stream\nabc\nendstream").unwrap(),
            Object::Dict(_)
        ));
        let array = object_impl(b"[<<>> stream ( endstream )]").unwrap();
        let items: Vec<_> = array.into_array().unwrap().raw_iter().collect();
        assert_eq!(items.len(), 3);
    }

    // A name whose `#` is no escape is a name wherever it is an object, the `#`
    // itself, as `MuPDF` reads it (`PyMuPDF` 1.28.2).
    #[test]
    fn name_with_escapes_that_are_not_hex() {
        let name = |object: Object<'_>| object.into_name().map(|n| n.to_vec());
        assert_eq!(name(object_impl(b"/a#zz").unwrap()), Some(b"a#zz".to_vec()));
        let array = object_impl(b"[/a#zz 1]").unwrap();
        let items: Vec<_> = array.into_array().unwrap().iter::<Object<'_>>().collect();
        assert_eq!(items.len(), 2);
        assert_eq!(name(items[0].clone()), Some(b"a#zz".to_vec()));
        let dict = object_impl(b"<< /K /a#zz /L 1 >>")
            .unwrap()
            .into_dict()
            .unwrap();
        assert_eq!(dict.entries().count(), 2);
        assert_eq!(
            dict.get::<Name<'_>>(b"K").map(|n| n.to_vec()),
            Some(b"a#zz".to_vec())
        );
    }

    #[test]
    fn display() {
        let cases: &[(&[u8], &str)] = &[
            (b"null", "null"),
            (b"true", "true"),
            (b"34.5", "34.5"),
            (b"(Hi)", "\"Hi\""),
            (b"/Name", "/Name"),
            (b"[45]", "[45]"),
            (b"<</Entry 45>>", "<< /Entry 45 >>"),
        ];

        for (input, expected) in cases {
            assert_eq!(format!("{}", object_impl(input).unwrap()), *expected);
        }
        let stream = indirect_impl(b"1 0 obj << /Length 3 >> stream\nabc\nendstream endobj");
        assert_eq!(format!("{}", stream.unwrap()), "Stream (len: 3)");
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn object_sizes() {
        assert_eq!(size_of::<Object<'_>>(), 40);
        assert_eq!(size_of::<Array<'_>>(), 32);
        assert_eq!(size_of::<Dict<'_>>(), 16);
        assert_eq!(size_of::<Name<'_>>(), 32);
        assert_eq!(size_of::<Null>(), 0);
        assert_eq!(size_of::<Number>(), 16);
        assert_eq!(size_of::<Stream<'_>>(), 32);
        assert_eq!(size_of::<String<'_>>(), 32);
    }
}
