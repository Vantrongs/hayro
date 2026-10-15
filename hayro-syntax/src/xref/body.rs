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
use crate::object::ObjectIdentifier;
use crate::object::parsed::{DictNode, Node};
use crate::reader::{Reader, ReaderContext, ReaderExt};
use crate::sync::Arc;
#[cfg(test)]
use crate::sync::FxHashMap;

/// Where the body of an object is: the bytes it is in and where it starts in them,
/// its header and all for an object of the file, and the object whose key decrypts
/// it ([`ReaderContext::decryption`]). Every entry that leads to one place reads
/// one object there, which is read once ([`Shape`]).
///
/// A body is read only within its slot: from its place up to the next place an entry
/// of the table, or the header of an object stream, names in the same bytes, or their
/// end ([`super::XRef::place`]). A body that does not end in its slot cannot be
/// read. So the places of one buffer lex disjoint bytes, and reading every body of a
/// buffer, keeping what it holds, takes time and memory linear in the buffer, however
/// the objects of a damaged file overlap; the table's check reads a file's objects by
/// the same rule ([`super::check`]). A stream's data, which are not lexed, nor copied,
/// may reach past the slot ([`super::Layout::stream_data`]); those of an object
/// stream, which are decoded and kept, end at another stream's place
/// ([`Taken::Contained`]).
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
    /// As an object stream: with its data, cut at the next place that holds a
    /// stream, if they reach it. Its data are decoded and kept, so the data of the
    /// object streams of a file must be disjoint for what is kept to be linear in
    /// the file, and those of streams at different places are only where none
    /// reaches another's place.
    Contained,
}

/// Where the data of a stream are, from and up to, and where they are cut as an
/// object stream's ([`Taken::Contained`]).
pub(super) type DataAt = (usize, usize, usize);

/// What the body at a place holds, parsed once with every array and dictionary in
/// it ([`crate::object::parsed`]), which every read of the place, and of anything in
/// it, makes an object of without lexing the body again.
#[derive(Clone)]
pub(super) enum Shape {
    /// Nothing that can be read as an object: a header or a body that does not
    /// parse in its slot.
    Unreadable,
    /// An object that is no stream.
    Value(Node),
    /// A stream: its dictionary, where the dictionary ends, and where its data are,
    /// with what finding them lost, once found the same way from every chain of
    /// references (the data are found by /Length, which may be an object).
    Stream {
        dict: Arc<DictNode>,
        end: usize,
        data: Option<(Option<DataAt>, Lost)>,
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

impl Shape {
    /// What the body at `place` holds: the object `data`, which ends where the
    /// place's slot does, holds there, after a header in the file, read in `ctx` as an
    /// entry leading to it reads it, which resolves nothing.
    pub(super) fn read<'a>(data: &'a [u8], place: Place, ctx: &ReaderContext<'a>) -> Self {
        #[cfg(test)]
        BODIES.with_borrow_mut(|bodies| *bodies.entry(place).or_default() += 1);
        let Some(mut r) = data.get(place.at..).map(|_| Reader::new(data)) else {
            return Self::Unreadable;
        };
        r.jump(place.at);
        let header = place.source == Source::File;
        let read = Read::from(
            &mut r,
            header,
            |r| DictNode::read(r, data, ctx),
            |r| Node::read(r, data, ctx),
        );
        crate::work::read_bodies(r.offset() - place.at);
        match read {
            None => Self::Unreadable,
            Some(Read::Object(node)) => Self::Value(node),
            Some(Read::Dict(dict)) => Self::Value(Node::Dict(Arc::new(dict))),
            Some(Read::Stream { dict, end }) => Self::Stream {
                dict: Arc::new(dict),
                end,
                data: None,
            },
        }
    }
}

/// What reading the body of an object finds, whatever entry it is read for, a
/// dictionary as `D`, another object as `O`.
pub(super) enum Read<O, D> {
    /// An object that is no stream nor dictionary.
    Object(O),
    /// A dictionary.
    Dict(D),
    /// A stream's dictionary, and where the dictionary ends: the stream's data are
    /// found by its /Length, which may be an object, read from each chain of
    /// references.
    Stream { dict: D, end: usize },
}

impl<O, D> Read<O, D> {
    /// The body `r` is at, after its header where `header` ([`super::read_object`]),
    /// a dictionary read by `dict`, another object by `object`; `r` is left where it
    /// ends, a stream at its dictionary's end.
    pub(super) fn from<'a>(
        r: &mut Reader<'a>,
        header: bool,
        dict: impl FnOnce(&mut Reader<'a>) -> Option<D>,
        object: impl FnOnce(&mut Reader<'a>) -> Option<O>,
    ) -> Option<Self> {
        let (_, read) = super::read_object(r, header, dict, object)?;
        match read {
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
