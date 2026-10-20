//! Objects parsed once, with every array and dictionary in them, which reads of the
//! body of an object the table lists are made from ([`crate::xref`]): a read of any
//! type, of the object or of anything in it, walks what was parsed and never lexes
//! the body's bytes again, however many reads share the body.
//!
//! A node holds no bytes: where its strings, names, arrays and dictionaries are is
//! kept relative to the bytes of what holds it, the body for an object, an array's
//! items for an item, a dictionary for a value, so that a node is read from
//! wherever those bytes are. Parsing reads an object as skipping it does (the one
//! reading of arrays and dictionaries that `Object::skip_with_depth` and
//! `parse_dict_with` share), so a body parses where it skips, to the same end.

use crate::hash;
use crate::object::array::parse_array_with;
use crate::object::dict::{new_offsets, parse_dict_with};
use crate::object::{
    Array, Bytes, Dict, MAX_OBJECT_NESTING_DEPTH, MaybeRef, Name, Null, Number, ObjRef, Object,
    ObjectLike, String, Written, leaf,
};
use crate::reader::{Reader, ReaderContext, ReaderExt};
use crate::sync::Arc;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// Where bytes are, from and up to, in the bytes of what holds them.
pub(crate) type Span = (usize, usize);

/// Where a number's bytes are, from and up to, in the bytes of what holds it, in 32
/// bits each, so that a number's node is no larger than a string's: a dictionary,
/// an array or an object of 4 GiB or more that holds a number does not parse.
type Token = (u32, u32);

/// The bytes of a string or name: where they are, or decoded.
#[derive(Clone)]
pub(crate) enum Text {
    At(Span),
    Held(Arc<[u8]>),
}

impl Text {
    /// The text of `bytes`, which `data` holds from `start` on, if borrowed from it.
    fn of(bytes: Bytes<'_>, data: &[u8], start: usize) -> Self {
        match bytes.held() {
            Ok(part) => {
                let from = part.as_ptr() as usize - data.as_ptr() as usize - start;
                Self::At((from, from + part.len()))
            }
            Err(held) => Self::Held(match held {
                Bytes::Shared(bytes) => bytes,
                held => Arc::from(held.as_ref()),
            }),
        }
    }

    fn bytes<'a>(&self, data: &'a [u8]) -> Bytes<'a> {
        match self {
            Self::At((from, to)) => Bytes::Borrowed(&data[*from..*to]),
            Self::Held(bytes) => Bytes::Shared(bytes.clone()),
        }
    }

    fn as_slice<'s>(&'s self, data: &'s [u8]) -> &'s [u8] {
        match self {
            Self::At((from, to)) => &data[*from..*to],
            Self::Held(bytes) => bytes,
        }
    }
}

/// An object, or an item of an array or value of a dictionary, parsed.
#[derive(Clone)]
pub(crate) enum Node {
    Null,
    Boolean(bool),
    /// A number, and where its bytes are, which a copy writes
    /// ([`crate::object::Written`]).
    Number(Number, Token),
    /// A reference, which only an item or a value is.
    Ref(ObjRef),
    String(Text),
    Name(Text),
    Array(Arc<ArrayNode>),
    Dict(Arc<DictNode>),
}

/// An array: where its items are, and each item.
pub(crate) struct ArrayNode {
    span: Span,
    items: Vec<Node>,
}

/// A dictionary: where it is, and each entry's value, those of a small dictionary in
/// a list with where each key is, which takes a few dozen bytes an entry where its
/// map takes hundreds a dictionary, in no particular order ([`Dict::keys`] sorts
/// them).
pub(crate) struct DictNode {
    span: Span,
    entries: Entries,
}

enum Entries {
    List(Box<[(Text, Node)]>),
    Map(Box<hash::Map<Name<'static>, Node>>),
}

/// At most this many entries are kept in a list, which a key is searched in entry by
/// entry.
const LISTED: usize = 8;

/// What parsing is reading: the bytes `r` reads, in `ctx`, which decrypts strings.
struct Parse<'r, 'a> {
    data: &'a [u8],
    ctx: &'r ReaderContext<'a>,
}

impl<'a> Parse<'_, 'a> {
    /// An item of an array or a value of a dictionary `depth` deep, as
    /// `MaybeRef<Object>` skips it: a reference, or else an object.
    fn item(&self, r: &mut Reader<'a>, start: usize, depth: usize) -> Option<Node> {
        if let Some(reference) = r.read_without_context::<ObjRef>() {
            return Some(Node::Ref(reference));
        }
        let at = r.offset();
        let node = self.object(r, start, depth);
        if node.is_none() {
            r.jump(at);
        }
        node
    }

    /// An object `depth` deep, as `Object::skip_with_depth` skips it.
    fn object(&self, r: &mut Reader<'a>, start: usize, depth: usize) -> Option<Node> {
        match r.peek_byte()? {
            b'<' if r.peek_bytes(2) == Some(b"<<") => {
                Some(Node::Dict(Arc::new(self.dict(r, start, depth)?)))
            }
            b'[' => Some(Node::Array(Arc::new(self.array(r, start, depth)?))),
            _ => {
                let at = r.offset();
                Some(match leaf(r, false, self.ctx)? {
                    Object::Null(_) => Node::Null,
                    Object::Boolean(value) => Node::Boolean(value),
                    Object::Number(number) => {
                        let place = |offset: usize| u32::try_from(offset - start).ok();
                        Node::Number(number, (place(at)?, place(r.offset())?))
                    }
                    Object::String(string) => {
                        Node::String(Text::of(string.into_bytes(), self.data, start))
                    }
                    Object::Name(name) => Node::Name(Text::of(name.into_bytes(), self.data, start)),
                    // A token is none of these.
                    Object::Array(_) | Object::Dict(_) | Object::Stream(_) => return None,
                })
            }
        }
    }

    /// A dictionary `depth` deep, as `Dict::skip_with_depth` skips it, where it is
    /// in what holds it from `start`.
    fn dict(&self, r: &mut Reader<'a>, start: usize, depth: usize) -> Option<DictNode> {
        if depth >= MAX_OBJECT_NESTING_DEPTH {
            return None;
        }
        let from = r.offset();
        // The entries, each key borrowed while they are few, in a list where a key's
        // later entry replaces its value, as a map built by the same insertions as
        // the map of a dictionary read from its bytes keeps the last; past `LISTED`
        // keys, in such a map, the keys held.
        let mut few: Vec<(Name<'a>, Node)> = Vec::new();
        let mut many: Option<hash::Map<Name<'static>, Node>> = None;
        let bytes = parse_dict_with(r, false, depth + 1, Some(b"<<"), b">>", |r, key, _| {
            let key = Name::new(key)?;
            let value = self.item(r, from, depth + 1)?;
            match &mut many {
                Some(many) => {
                    many.insert(key.to_static(), value);
                }
                None => match few.iter().position(|(listed, _)| *listed == key) {
                    Some(at) => few[at].1 = value,
                    None if few.len() < LISTED => few.push((key, value)),
                    None => {
                        let mut held = new_offsets();
                        for (key, value) in few.drain(..) {
                            held.insert(key.to_static(), value);
                        }
                        held.insert(key.to_static(), value);
                        many = Some(held);
                    }
                },
            }
            Some(())
        })?;
        let entries = match many {
            Some(many) => Entries::Map(Box::new(many)),
            None => {
                let text = |key: Name<'a>| Text::of(key.into_bytes(), self.data, from);
                Entries::List(
                    few.into_iter()
                        .map(|(key, value)| (text(key), value))
                        .collect(),
                )
            }
        };
        Some(DictNode {
            span: (from - start, from - start + bytes.len()),
            entries,
        })
    }

    /// An array `depth` deep, as `Array::skip_with_depth` skips it, where its items
    /// are in what holds it from `start`.
    fn array(&self, r: &mut Reader<'a>, start: usize, depth: usize) -> Option<ArrayNode> {
        if depth >= MAX_OBJECT_NESTING_DEPTH {
            return None;
        }
        let from = r.offset() + 1;
        let mut items = Vec::new();
        parse_array_with(r, |r| {
            items.push(self.item(r, from, depth + 1)?);
            Some(())
        })?;
        items.shrink_to_fit();
        Some(ArrayNode {
            span: (from - start, r.offset() - 1 - start),
            items,
        })
    }
}

/// An object parsed where a body holds it: its node, and where it starts in the
/// bytes it is read from, which where its parts are is kept relative to.
#[derive(Clone)]
pub(crate) struct Top {
    at: usize,
    node: Node,
}

impl Top {
    /// The object `r` is at in `data`, read in `ctx` as reading an object reads it
    /// (`Object::read`), a reference being a number.
    pub(crate) fn read<'a>(
        r: &mut Reader<'a>,
        data: &'a [u8],
        ctx: &ReaderContext<'a>,
    ) -> Option<Self> {
        let at = r.offset();
        let node = Parse { data, ctx }.object(r, at, 0)?;
        Some(Self { at, node })
    }

    /// The dictionary `dict`, parsed where it is kept relative to the start of the
    /// bytes it is read from.
    pub(crate) fn dict(dict: Arc<DictNode>) -> Self {
        Self {
            at: 0,
            node: Node::Dict(dict),
        }
    }

    /// The object, read from `data`, which it was parsed in, in `ctx`.
    pub(crate) fn object<'a>(&self, data: &'a [u8], ctx: &ReaderContext<'a>) -> Object<'a> {
        self.node.object(&data[self.at..], ctx)
    }

    /// The object as written ([`Written`]), read from `data`, which it was parsed
    /// in, in `ctx`.
    pub(crate) fn written<'a>(&self, data: &'a [u8], ctx: &ReaderContext<'a>) -> Written<'a> {
        self.node.written(&data[self.at..], ctx)
    }
}

impl Node {
    /// The object, held in `data`, read in `ctx`; `null` for a reference, which only
    /// [`Self::item`] reads.
    pub(crate) fn object<'a>(&self, data: &'a [u8], ctx: &ReaderContext<'a>) -> Object<'a> {
        match self {
            Self::Null | Self::Ref(_) => Object::Null(Null),
            Self::Boolean(value) => Object::Boolean(*value),
            Self::Number(number, _) => Object::Number(*number),
            Self::String(text) => Object::String(String::from_bytes(text.bytes(data))),
            Self::Name(text) => Object::Name(Name::from_bytes(text.bytes(data))),
            Self::Array(array) => Object::Array(Array::parsed(data, array.clone(), ctx)),
            Self::Dict(dict) => Object::Dict(Dict::parsed(data, dict.clone(), ctx)),
        }
    }

    /// The item, held in `data`, read in `ctx`, as written ([`Written`]).
    pub(crate) fn written<'a>(&self, data: &'a [u8], ctx: &ReaderContext<'a>) -> Written<'a> {
        match self {
            Self::Ref(reference) => Written::Ref(*reference),
            Self::Number(_, (from, to)) => Written::Number(&data[*from as usize..*to as usize]),
            Self::Null => Written::Null,
            Self::Boolean(value) => Written::Boolean(*value),
            Self::String(text) => Written::String(String::from_bytes(text.bytes(data))),
            Self::Name(text) => Written::Name(Name::from_bytes(text.bytes(data))),
            Self::Array(array) => Written::Array(Array::parsed(data, array.clone(), ctx)),
            Self::Dict(dict) => Written::Dict(Dict::parsed(data, dict.clone(), ctx)),
        }
    }

    /// The item, held in `data`, read in `ctx`.
    pub(crate) fn item<'a>(&self, data: &'a [u8], ctx: &ReaderContext<'a>) -> MaybeRef<Object<'a>> {
        match self {
            Self::Ref(reference) => MaybeRef::Ref(*reference),
            node => MaybeRef::NotRef(node.object(data, ctx)),
        }
    }

    /// The item, held in `data`, read in `ctx`, as a `T`, as reading a `T` from its
    /// bytes reads it, but for the reference it may be.
    pub(crate) fn item_as<'a, T: ObjectLike<'a>>(
        &self,
        data: &'a [u8],
        ctx: &ReaderContext<'a>,
    ) -> Option<MaybeRef<T>> {
        match self.item(data, ctx) {
            MaybeRef::Ref(reference) => Some(MaybeRef::Ref(reference)),
            MaybeRef::NotRef(object) => object.cast::<T>().map(MaybeRef::NotRef),
        }
    }
}

impl ArrayNode {
    /// Where the items are in what holds the array.
    pub(crate) fn span(&self) -> Span {
        self.span
    }

    /// The items.
    pub(crate) fn items(&self) -> &[Node] {
        &self.items
    }
}

impl DictNode {
    /// The dictionary `r` is at in `data`, read in `ctx` as reading an indirect
    /// object's dictionary reads it: where it is is kept relative to the start of
    /// `data`.
    pub(crate) fn read<'a>(
        r: &mut Reader<'a>,
        data: &'a [u8],
        ctx: &ReaderContext<'a>,
    ) -> Option<Self> {
        Parse { data, ctx }.dict(r, 0, 0)
    }

    /// Where the dictionary is in what holds it.
    pub(crate) fn span(&self) -> Span {
        self.span
    }

    pub(crate) fn len(&self) -> usize {
        match &self.entries {
            Entries::List(entries) => entries.len(),
            Entries::Map(entries) => entries.len(),
        }
    }

    /// The value of `key`, of the dictionary whose bytes are `data`.
    pub(crate) fn get(&self, key: &[u8], data: &[u8]) -> Option<&Node> {
        match &self.entries {
            Entries::List(entries) => entries
                .iter()
                .find(|(name, _)| name.as_slice(data) == key)
                .map(|(_, node)| node),
            Entries::Map(entries) => entries.get(key),
        }
    }

    /// The entries of the dictionary whose bytes are `data`, in no particular order.
    pub(crate) fn entries<'s, 'a: 's>(
        &'s self,
        data: &'a [u8],
    ) -> impl Iterator<Item = (Name<'a>, &'s Node)> + 's {
        let (list, map) = match &self.entries {
            Entries::List(entries) => (Some(entries), None),
            Entries::Map(entries) => (None, Some(entries)),
        };
        let list = list
            .into_iter()
            .flat_map(|entries| entries.iter())
            .map(move |(name, node)| (Name::from_bytes(name.bytes(data)), node));
        let map = map
            .into_iter()
            .flat_map(|entries| entries.iter())
            .map(|(name, node)| (name.clone(), node));
        list.chain(map)
    }
}

/// The items of a parsed array from one on, as reading the array's bytes from an
/// item reads them ([`ObjectLike::from_items`]).
pub(crate) struct Items<'r, 'a> {
    pub(crate) data: &'a [u8],
    pub(crate) items: &'r [Node],
    pub(crate) at: usize,
}

impl<'a> Items<'_, 'a> {
    /// The next item, which is no reference, as a `T`, as `T::read` reads it.
    pub(crate) fn next_as<T: ObjectLike<'a>>(&mut self, ctx: &ReaderContext<'a>) -> Option<T> {
        let node = self.items.get(self.at)?;
        if matches!(node, Node::Ref(_)) {
            return None;
        }
        self.at += 1;
        node.object(self.data, ctx).cast()
    }

    /// The next item as a `T`, resolved where it is a reference, as reading a
    /// `MaybeRef<T>` and resolving it reads it.
    pub(crate) fn next_resolved<T: ObjectLike<'a>>(
        &mut self,
        ctx: &ReaderContext<'a>,
    ) -> Option<T> {
        match self.items.get(self.at)? {
            Node::Ref(reference) => {
                self.at += 1;
                ctx.xref().get_with::<T>((*reference).into(), ctx)
            }
            _ => T::from_items(self, ctx),
        }
    }
}
