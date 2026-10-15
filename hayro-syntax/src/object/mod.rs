//! Parsing and reading from PDF objects.

pub use crate::object::array::Array;
pub use crate::object::date::DateTime;
pub use crate::object::dict::Dict;
pub use crate::object::name::Name;
use crate::object::name::skip_name_like;
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
    /// Reads the object an indirect object holds, after its header.
    fn read_indirect(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        r.read_with_context::<Self>(ctx)
    }
}

/// A trait for PDF objects that can be borrowed directly from an [`Object`].
pub(crate) trait ObjectRefLike<'a>: Sized {
    fn cast_ref<'b>(obj: &'b Object<'a>) -> Option<&'b Self>;
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

impl<'a> Object<'a> {
    /// The object read in another context from the same bytes: the same object, its
    /// dictionaries and arrays resolving references in `ctx`. A string is the same
    /// string: the bytes say how they decrypt ([`ReaderContext::decryption`]), not
    /// the context.
    pub(crate) fn in_context(&self, ctx: &ReaderContext<'a>) -> Self {
        match self {
            Self::Dict(dict) => Self::Dict(dict.in_context(ctx)),
            Self::Array(array) => Self::Array(array.in_context(ctx)),
            Self::Stream(stream) => Self::Stream(stream.in_context(ctx)),
            Self::Null(_)
            | Self::Boolean(_)
            | Self::Number(_)
            | Self::String(_)
            | Self::Name(_) => self.clone(),
        }
    }
}

impl<'a> ObjectLike<'a> for Object<'a> {
    // Only an indirect object is a stream: a dictionary followed by `stream`, as
    // `Syntax::read_from` in the `xref` module reads one. In an array or dictionary,
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
    fn number(&self, r: &mut Reader<'a>) -> Option<Object<'a>> {
        r.read_without_context::<Number>().map(Object::Number)
    }
}

/// Reads a token that is no array or dictionary, the one way reading, skipping, and
/// the scan that rebuilds a table read it; `make` says whether into an object or
/// into nothing.
///
/// A token that starts like a number or a name but is none, such as `1e3` or a name
/// with an escape that is not hex, is `null` up to the next white space or delimiter.
/// `MuPDF` reads such a number as a keyword, which is `null` as a value in an array or
/// dictionary (`lex_number` in `MuPDF`'s `pdf-lex.c`, `pdf_parse_array` and
/// `pdf_parse_dict` in `pdf-parse.c`), so one bad token does not lose the object
/// around it. In a content stream, a bad number is none, as its operators expect.
#[inline(always)]
pub(crate) fn leaf<'a, L: Leaf<'a>>(
    r: &mut Reader<'a>,
    is_content_stream: bool,
    make: L,
) -> Option<L::Out> {
    Some(match r.peek_byte()? {
        b'n' => {
            Null::read(r, ReaderContext::dummy_ref())?;
            make.null()
        }
        b't' | b'f' => make.boolean(bool::read(r, ReaderContext::dummy_ref())?),
        b'/' => match r.skip::<Name<'_>>(is_content_stream) {
            Some(name) => make.name(name)?,
            None => {
                skip_name_like(r, true)?;
                make.null()
            }
        },
        b'<' | b'(' => make.string(r, is_content_stream)?,
        b'.' | b'+' | b'-' | b'0'..=b'9' => match make.number(r) {
            Some(number) => number,
            None if !is_content_stream => {
                skip_name_like(r, false)?;
                make.null()
            }
            None => return None,
        },
        // See test case operator-in-TJ-array-0: Be lenient and skip content operators in
        // array
        _ => {
            skip_name_like(r, false)?;
            make.null()
        }
    })
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

            impl<'a> crate::object::ObjectLike<'a> for $t$(<$l>),* {}

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
    fn deeply_nested_arrays() {
        let data = format!("{}0{}", "[".repeat(5000), "]".repeat(5000));
        assert!(object_impl(data.as_bytes()).is_none());
    }

    #[test]
    fn deeply_nested_dictionaries() {
        let data = format!("{}0{}", "<< /X ".repeat(5000), " >>".repeat(5000));
        assert!(object_impl(data.as_bytes()).is_none());
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
    fn number() {
        assert!(matches!(object_impl(b"34.5").unwrap(), Object::Number(_)));
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

    // A name whose escapes are not hex is `null` wherever it is an object, as skipping
    // it reads it, and no name.
    #[test]
    fn name_with_escapes_that_are_not_hex() {
        assert!(matches!(object_impl(b"/a#zz").unwrap(), Object::Null(_)));
        let array = object_impl(b"[/a#zz 1]").unwrap();
        let items: Vec<_> = array.into_array().unwrap().raw_iter().collect();
        assert_eq!(items.len(), 2);
        let dict = object_impl(b"<< /K /a#zz /L 1 >>").unwrap();
        assert_eq!(dict.into_dict().unwrap().entries().count(), 2);
        assert!(
            Reader::new(b"/a#zz")
                .read_without_context::<Name<'_>>()
                .is_none()
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
        assert_eq!(size_of::<Dict<'_>>(), 8);
        assert_eq!(size_of::<Name<'_>>(), 32);
        assert_eq!(size_of::<Null>(), 0);
        assert_eq!(size_of::<Number>(), 16);
        assert_eq!(size_of::<Stream<'_>>(), 24);
        assert_eq!(size_of::<String<'_>>(), 32);
    }
}
