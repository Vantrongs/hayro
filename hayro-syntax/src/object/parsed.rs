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
use crate::object::dict::parse_dict_with;
use crate::object::name::{order, same};
use crate::object::{
    Array, Bytes, Dict, MAX_OBJECT_NESTING_DEPTH, MaybeRef, Name, Null, Number, ObjRef, Object,
    ObjectLike, String, Written, leaf,
};
use crate::reader::{Reader, ReaderContext, ReaderExt};
use crate::sync::Arc;
use alloc::boxed::Box;
use alloc::vec::Vec;
use smallvec::SmallVec;

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

/// A dictionary: where it is, and each entry with where its key is, one entry a key:
/// those of a small dictionary in the file's order, those of a larger one sorted by
/// key ([`LISTED`]). A list takes a few dozen bytes an entry, where a map takes
/// about twice that and grows by doubling; every dictionary of a file is parsed when
/// it opens and kept while it is open, so a file whose pages each list all the
/// resources of the pages before them holds tens of thousands of entries.
pub(crate) struct DictNode {
    span: Span,
    entries: Box<[(Text, Node)]>,
}

/// At most this many entries are kept in the file's order, which a key is searched
/// in entry by entry; more are sorted by key and searched by halving.
const LISTED: usize = 8;

/// How the entry of a key is found among those of a dictionary of more than
/// [`LISTED`], so that a later entry of a key replaces the value at once, as the map
/// of a dictionary read from its bytes does, and parsing holds the values kept, not
/// every one replaced.
enum Large {
    /// Each key came after the one before it in order, as many files write them: a
    /// key after the last is new, and the entries need no sorting.
    Ascending,
    /// By the key's hash ([`Index`]).
    Indexed(Index),
}

/// Where each key of a dictionary of more than [`LISTED`] entries is among them, by
/// what [`Index::hash`] makes of the key.
///
/// Made once for each such dictionary being parsed at once on a thread, one inside
/// another, and kept emptied for the next ([`Index::take`]), so that a file of many
/// does not allocate and grow one for each.
#[derive(Default)]
struct Index(hash::Map<Hashed, usize>);

/// What an [`Index`] finds a key by: its hash, keyed at random for each index; without
/// `std`, whose maps are ordered ones ([`hash`]), the key itself.
#[cfg(feature = "std")]
type Hashed = u64;
#[cfg(not(feature = "std"))]
type Hashed = Name<'static>;

/// At most this many keys' room is kept for the next dictionary parsed on a thread
/// ([`Index`]); a larger index is freed, so that a thread holds no more than that
/// once it read a file, whatever the file's dictionaries.
#[cfg(feature = "std")]
const KEPT: usize = 1024;

#[cfg(feature = "std")]
std::thread_local! {
    static INDEXES: core::cell::RefCell<Vec<Index>> =
        const { core::cell::RefCell::new(Vec::new()) };
}

impl Index {
    /// An index of `entries`, each key once, whose keys are kept relative to `held`.
    fn of(entries: &[(Text, Node)], held: &[u8]) -> Self {
        let mut index = Self::take();
        for at in 0..entries.len() {
            // Each key once, so each is found new.
            index.find(&entries[..at], held, entries[at].0.as_slice(held));
        }
        index
    }

    /// One emptied, kept on this thread, or else a new one.
    fn take() -> Self {
        #[cfg(feature = "std")]
        if let Some(index) = INDEXES.with_borrow_mut(Vec::pop) {
            return index;
        }
        Self::default()
    }

    /// Keeps this, emptied, for the next dictionary on this thread, unless it has
    /// room for more than [`KEPT`] keys.
    fn keep(mut self) {
        self.0.clear();
        #[cfg(feature = "std")]
        if self.0.capacity() <= KEPT {
            INDEXES.with_borrow_mut(|kept| kept.push(self));
        }
    }

    /// What this index finds `key` by.
    #[cfg(feature = "std")]
    fn hash(&self, key: &[u8]) -> Hashed {
        use core::hash::BuildHasher;
        self.0.hasher().hash_one(key)
    }

    /// What this index finds `key` by.
    #[cfg(not(feature = "std"))]
    fn hash(&self, key: &[u8]) -> Hashed {
        Name::from_bytes(Bytes::decoded(SmallVec::from_slice(key)))
    }

    /// Where the entry of `key` is among `entries`, whose keys are kept relative to
    /// `held`, the keys compared counted ([`same`]); where it has none, its hash
    /// leads to the entry it is about to have, after the others, unless the hash
    /// already leads to another key's.
    fn find(&mut self, entries: &[(Text, Node)], held: &[u8], key: &[u8]) -> Option<usize> {
        let new = entries.len();
        let hash = self.hash(key);
        let at = *self.0.entry(hash).or_insert(new);
        if at == new {
            return None;
        }
        if same(entries[at].0.as_slice(held), key) {
            return Some(at);
        }
        // Another key has the same hash, which a file cannot aim at, as the hash is
        // keyed at random: found entry by entry.
        entries
            .iter()
            .position(|(listed, _)| same(listed.as_slice(held), key))
    }
}

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
        // Where the keys of the entries are kept relative to.
        let held = &self.data[from..];
        // A key's later entry replaces its value at once, as the map of a dictionary
        // read from its bytes keeps the last: the key is found among the entries one
        // by one while they are few, and past `LISTED` as [`Large`] says, the keys
        // compared counted ([`same`], [`order`]).
        let mut entries: SmallVec<[(Text, Node); LISTED]> = SmallVec::new();
        let mut large: Option<Large> = None;
        let bytes = parse_dict_with(r, false, depth + 1, Some(b"<<"), b">>", |r, key, _| {
            let key = Name::new(key)?;
            let value = self.item(r, from, depth + 1)?;
            if let Some(Large::Ascending) = large
                && entries
                    .last()
                    .is_some_and(|(last, _)| order(last.as_slice(held), &key).is_ge())
            {
                large = Some(Large::Indexed(Index::of(&entries, held)));
            }
            let found = match &mut large {
                Some(Large::Indexed(index)) => index.find(&entries, held, &key),
                Some(Large::Ascending) => None,
                None => entries
                    .iter()
                    .position(|(listed, _)| same(listed.as_slice(held), &key)),
            };
            if let Some(at) = found {
                entries[at].1 = value;
                return Some(());
            }
            entries.push((Text::of(key.into_bytes(), self.data, from), value));
            if large.is_none() && entries.len() > LISTED {
                let ascending = entries
                    .windows(2)
                    .all(|pair| order(pair[0].0.as_slice(held), pair[1].0.as_slice(held)).is_lt());
                large = Some(if ascending {
                    Large::Ascending
                } else {
                    Large::Indexed(Index::of(&entries, held))
                });
            }
            Some(())
        });
        let sort = match large {
            Some(Large::Indexed(index)) => {
                index.keep();
                true
            }
            _ => false,
        };
        let bytes = bytes?;
        if sort {
            // Each key once, so any sort orders them alike.
            entries.sort_unstable_by(|(a, _), (b, _)| order(a.as_slice(held), b.as_slice(held)));
        }
        let entries = entries.into_boxed_slice();
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
        self.entries.len()
    }

    /// The value of `key`, of the dictionary whose bytes are `data`.
    pub(crate) fn get(&self, key: &[u8], data: &[u8]) -> Option<&Node> {
        let entries = &self.entries;
        let at = if entries.len() > LISTED {
            entries
                .binary_search_by(|(name, _)| name.as_slice(data).cmp(key))
                .ok()?
        } else {
            entries
                .iter()
                .position(|(name, _)| name.as_slice(data) == key)?
        };
        Some(&entries[at].1)
    }

    /// The entries of the dictionary whose bytes are `data`, in no particular order.
    pub(crate) fn entries<'s, 'a: 's>(
        &'s self,
        data: &'a [u8],
    ) -> impl Iterator<Item = (Name<'a>, &'s Node)> + 's {
        self.entries
            .iter()
            .map(move |(name, node)| (Name::from_bytes(name.bytes(data)), node))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A key whose hash leads to another key's entry is found entry by entry, and
    /// the hash keeps leading to the first.
    #[test]
    fn a_key_whose_hash_another_has_is_found_among_the_entries() {
        let held = b"AB";
        let mut entries = vec![(Text::At((0, 1)), Node::Null)];
        let mut index = Index::default();
        assert_eq!(index.find(&entries, held, b"A"), None);
        // B's hash leads to A's entry, as if the two keys hashed alike.
        index.0.insert(index.hash(b"B"), 0);
        assert_eq!(index.find(&entries, held, b"B"), None);
        entries.push((Text::At((1, 2)), Node::Boolean(true)));
        assert_eq!(index.find(&entries, held, b"B"), Some(1));
        assert_eq!(index.find(&entries, held, b"A"), Some(0));
        assert_eq!(index.0.get(&index.hash(b"B")), Some(&0));
        assert_eq!(index.find(&entries, held, b"C"), None);
    }
}
