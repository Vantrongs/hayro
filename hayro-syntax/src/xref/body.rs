//! What an entry of the table leads to: the body of an object, where it is
//! ([`Place`]) and what it holds ([`Shape`]), read once for every entry and every
//! read that leads to it, and why an entry's object may not be read ([`Unread`]).
//!
//! Every read of an object the table lists goes from its entry to its place, which
//! takes no reading of the file, then to its shape, which is read the first time any
//! read reaches the place and kept: a header is read once however many entries lead
//! to it, and a read of any type is the object the shape holds, cast to that type,
//! so that a read of another type than the object's costs nothing to repeat.

use crate::lost::Lost;
use crate::object::dict::Kept;
use crate::object::{
    Array, Bytes, Dict, Name, Null, Number, Object, ObjectIdentifier, Stream, String,
};
use crate::reader::{Reader, ReaderContext, ReaderExt};
#[cfg(test)]
use crate::sync::FxHashMap;

/// Where the body of an object is: the bytes it is in and where it starts in them,
/// its header and all for an object of the file, and the object whose key decrypts
/// it ([`ReaderContext::decryption`]). Every entry that leads to one place reads
/// one object there, which is read once ([`Shape`]).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct Place {
    pub(super) source: Source,
    pub(super) at: usize,
    pub(super) decryption: Option<ObjectIdentifier>,
}

/// The bytes an object is in.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Source {
    /// The file, where an object starts with its header.
    File,
    /// The decoded data of an object stream, by where it is kept
    /// ([`crate::data::Data::get_with`]), where an object has no header.
    Stream(usize),
}

/// Why a read of an object the table lists finds none.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) enum Unread {
    /// The object is lost: the table lists it, but what it leads to cannot be read
    /// as the object. The object named is the first that reading it lost, which
    /// the read tells the computation being watched ([`crate::lost`]): this is the
    /// only way a read that fails says why, and the only one from such a failure to
    /// a read that finds nothing.
    Lost(ObjectIdentifier),
    /// The read's own chain of references rejects the object or one that reading
    /// it needs as a cycle: from another chain it may be read, so it is neither
    /// lost nor kept.
    Rejected,
}

/// How a read takes an object that is a stream.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) enum Taken {
    /// As its dictionary, as a read of any type but a stream or any object reads it.
    Dict,
    /// With its data: a stream whose data cannot be read is lost.
    Whole,
    /// With its data, or as its dictionary where they cannot be read, as loading
    /// the pages reads it, whose reads look at dictionaries, arrays and numbers.
    Either,
}

/// What the body at a place holds, without the bytes it is read from, which every
/// read of the place makes an object of.
#[derive(Clone)]
pub(super) enum Shape {
    /// Nothing that can be read as an object: a header or a body that does not
    /// parse.
    Unreadable,
    /// An object that is no stream.
    Value(Value),
    /// A stream: its dictionary, where the dictionary ends, and where its data are,
    /// with what finding them lost, once found the same way from every chain of
    /// references (the data are found by /Length, which may be an object).
    Stream {
        dict: DictShape,
        end: usize,
        data: Option<(Option<Span>, Lost)>,
    },
}

impl core::fmt::Debug for Shape {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Unreadable => "Unreadable",
            Self::Value(_) => "Value",
            Self::Stream { .. } => "Stream",
        })
    }
}

/// Where bytes are, from and up to.
pub(super) type Span = (usize, usize);

/// An object that is no stream, without the bytes it is read from.
#[derive(Clone)]
pub(super) enum Value {
    Null,
    Boolean(bool),
    Number(Number),
    String(Text),
    Name(Text),
    /// An array's items.
    Array(Span),
    Dict(DictShape),
}

/// The bytes of a string or name: where they are, or decoded.
#[derive(Clone)]
pub(super) enum Text {
    At(Span),
    Held(Bytes<'static>),
}

/// A dictionary: its bytes, and where each entry's value starts in them.
#[derive(Clone)]
pub(super) struct DictShape {
    span: Span,
    offsets: Kept<'static>,
}

#[cfg(test)]
std::thread_local! {
    /// The places whose bodies were read on this thread, how often each, which the
    /// tests bound to show that each is read once however it is asked for.
    pub(crate) static BODIES: core::cell::RefCell<FxHashMap<Place, usize>> =
        core::cell::RefCell::new(FxHashMap::default());
    /// The places whose streams' data were found on this thread, how often each.
    pub(crate) static DATA_FOUND: core::cell::RefCell<FxHashMap<Place, usize>> =
        core::cell::RefCell::new(FxHashMap::default());
}

/// Where `part`, which `data` holds, is in it.
fn span(data: &[u8], part: &[u8]) -> Span {
    let start = part.as_ptr() as usize - data.as_ptr() as usize;
    (start, start + part.len())
}

impl Shape {
    /// What the body at `place` holds: the object `data` holds there, after a
    /// header in the file, read in `ctx` as an entry leading to it reads it, which
    /// resolves nothing.
    pub(super) fn read<'a>(data: &'a [u8], place: Place, ctx: &ReaderContext<'a>) -> Self {
        #[cfg(test)]
        BODIES.with_borrow_mut(|bodies| *bodies.entry(place).or_default() += 1);
        let Some(mut r) = data.get(place.at..).map(|_| Reader::new(data)) else {
            return Self::Unreadable;
        };
        r.jump(place.at);
        let header = place.source == Source::File;
        let read = Read::from(&mut r, header, ctx, |r| Dict::read_static(r, ctx));
        crate::work::read_bodies(r.offset() - place.at);
        match read {
            None => Self::Unreadable,
            Some(Read::Object(object)) => Self::Value(Value::of(data, object)),
            Some(Read::Dict((bytes, offsets))) => Self::Value(Value::Dict(DictShape {
                span: span(data, bytes),
                offsets,
            })),
            Some(Read::Stream {
                dict: (bytes, offsets),
                end,
            }) => Self::Stream {
                dict: DictShape {
                    span: span(data, bytes),
                    offsets,
                },
                end,
                data: None,
            },
        }
    }
}

/// What reading the body of an object finds, whatever entry it is read for, a
/// dictionary as `D`.
pub(super) enum Read<'a, D> {
    /// An object that is no stream nor dictionary.
    Object(Object<'a>),
    /// A dictionary.
    Dict(D),
    /// A stream's dictionary, and where the dictionary ends: the stream's data are
    /// found by its /Length, which may be an object, read from each chain of
    /// references.
    Stream { dict: D, end: usize },
}

impl<'a, D> Read<'a, D> {
    /// The body `r` is at, after its header where `header` ([`super::read_object`]),
    /// a dictionary read by `dict`; `r` is left where it ends, a stream at its
    /// dictionary's end.
    pub(super) fn from(
        r: &mut Reader<'a>,
        header: bool,
        ctx: &ReaderContext<'a>,
        dict: impl FnOnce(&mut Reader<'a>) -> Option<D>,
    ) -> Option<Self> {
        let (_, object) = super::read_object(r, header, ctx, dict)?;
        match object {
            // As `Object::read_indirect` reads an indirect object; an object stream
            // holds none, but reads one so too.
            Ok(dict) => {
                let end = r.offset();
                r.skip_white_spaces_and_comments();
                let stream = r.forward_tag(b"stream").is_some();
                r.jump(end);
                Some(if stream {
                    Self::Stream { dict, end }
                } else {
                    Self::Dict(dict)
                })
            }
            Err(object) => Some(Self::Object(object)),
        }
    }
}

impl Value {
    fn of<'a>(data: &'a [u8], object: Object<'a>) -> Self {
        let text = |bytes: Bytes<'a>| match bytes.held() {
            Ok(bytes) => Text::At(span(data, bytes)),
            Err(held) => Text::Held(held),
        };
        match object {
            Object::Null(_) => Self::Null,
            Object::Boolean(value) => Self::Boolean(value),
            Object::Number(number) => Self::Number(number),
            Object::String(string) => Self::String(text(string.into_bytes())),
            Object::Name(name) => Self::Name(text(name.into_bytes())),
            Object::Array(array) => Self::Array(span(data, array.data())),
            Object::Dict(dict) => Self::Dict(DictShape::of(data, &dict)),
            // Read only after a dictionary, which `Shape::read` reads itself.
            Object::Stream(stream) => Self::Dict(DictShape::of(data, stream.dict())),
        }
    }

    /// The object, its bytes in `data`, read in `ctx`.
    pub(super) fn object<'a>(&self, data: &'a [u8], ctx: &ReaderContext<'a>) -> Object<'a> {
        let bytes = |text: &Text| match text {
            Text::At((from, to)) => Bytes::Borrowed(&data[*from..*to]),
            Text::Held(bytes) => bytes.clone(),
        };
        match self {
            Self::Null => Object::Null(Null),
            Self::Boolean(value) => Object::Boolean(*value),
            Self::Number(number) => Object::Number(*number),
            Self::String(text) => Object::String(String::from_bytes(bytes(text))),
            Self::Name(text) => Object::Name(Name::from_bytes(bytes(text))),
            Self::Array((from, to)) => Object::Array(Array::new(&data[*from..*to], ctx)),
            Self::Dict(dict) => Object::Dict(dict.object(data, ctx)),
        }
    }
}

impl DictShape {
    fn of<'a>(data: &'a [u8], dict: &Dict<'a>) -> Self {
        Self {
            span: span(data, dict.data()),
            offsets: dict.static_offsets(),
        }
    }

    /// The dictionary, its bytes in `data`, read in `ctx`.
    pub(super) fn object<'a>(&self, data: &'a [u8], ctx: &ReaderContext<'a>) -> Dict<'a> {
        let (from, to) = self.span;
        Dict::with_offsets(&data[from..to], self.offsets.clone(), ctx)
    }
}

/// The data of the stream at `place` whose dictionary, `dict`, ends at `end` of
/// `data`, as [`Stream::after_dict`] finds them; where they are in `data`.
pub(super) fn stream_data<'a>(
    data: &'a [u8],
    place: Place,
    end: usize,
    dict: Dict<'a>,
) -> Option<Span> {
    #[cfg(test)]
    DATA_FOUND.with_borrow_mut(|found| *found.entry(place).or_default() += 1);
    #[cfg(not(test))]
    let _ = place;
    let mut r = Reader::new(data);
    r.jump(end);
    let stream = Stream::after_dict(&mut r, dict);
    crate::work::read_bodies(r.offset().saturating_sub(end));
    let stream = stream?;
    Some(span(data, stream.stored()))
}
