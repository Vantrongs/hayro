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
#[cfg(not(feature = "std"))]
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

    #[inline]
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
    /// An array, whose items are its run of the nodes of its body.
    Array(Run),
    /// A dictionary, whose values are its run of the nodes of its body, and whose
    /// keys, one for each value, are those of its body from the one this tells on
    /// ([`Nodes`]). Those of a small dictionary are in the file's order, those of a
    /// larger one sorted by key ([`LISTED`]).
    Dict(Run, u32),
}

/// Where an array or a dictionary is in the bytes of what holds it, and where its
/// run of nodes is among the nodes of its body ([`Nodes`]), in 32 bits, so that the
/// node of an array or a dictionary is no larger than a string's: a body of more
/// than 4 Gi nodes, which parsing would hold in more than 128 GiB, does not parse.
#[derive(Clone, Copy)]
pub(crate) struct Run {
    span: Span,
    at: u32,
    len: u32,
}

impl Run {
    /// The run of `nodes` from `at` on, of a container at `span`.
    fn of(span: Span, at: usize, nodes: &[Node]) -> Option<Self> {
        Some(Self {
            span,
            at: u32::try_from(at).ok()?,
            len: u32::try_from(nodes.len() - at).ok()?,
        })
    }

    /// Its nodes, among `nodes`; none, wherever it is, where it is empty, as
    /// moving the nodes of a dictionary's values leaves an empty one where it was
    /// ([`Scratch::compact`]).
    fn nodes(self, nodes: &[Node]) -> &[Node] {
        if self.len == 0 {
            return &[];
        }
        let at = self.at as usize;
        &nodes[at..at + self.len as usize]
    }
}

/// The items and values of every array and dictionary of a body, parsed once, and
/// the keys of its dictionaries, which every read of the body shares; none where it
/// has none. Held behind one pointer, as the table keeps one for each object
/// ([`crate::xref`]); keys are held apart from the nodes as they take less room.
#[derive(Clone, Default)]
pub(crate) struct Nodes(Option<Arc<Body>>);

struct Body {
    nodes: Box<[Node]>,
    keys: Box<[Text]>,
}

impl Nodes {
    /// Those `parsed` holds, which this takes, leaving its room.
    fn of(parsed: &mut Scratch) -> Self {
        /// What `parsed` holds: copied where its room is kept for the next body
        /// ([`Scratch::keep`]), else taken with its room, which the last run parsed
        /// filled exactly ([`Parse::last`]), so that a large body is not held twice.
        fn take<T>(parsed: &mut Vec<T>) -> Box<[T]> {
            #[cfg(feature = "std")]
            if parsed.capacity() <= KEPT {
                let mut held = Vec::with_capacity(parsed.len());
                held.append(parsed);
                return held.into_boxed_slice();
            }
            core::mem::take(parsed).into_boxed_slice()
        }
        if parsed.nodes.is_empty() && parsed.keys.is_empty() {
            return Self(None);
        }
        Self(Some(Arc::new(Body {
            nodes: take(&mut parsed.nodes),
            keys: take(&mut parsed.keys),
        })))
    }

    fn nodes(&self) -> &[Node] {
        self.0.as_ref().map_or(&[], |body| &body.nodes)
    }

    fn keys(&self) -> &[Text] {
        self.0.as_ref().map_or(&[], |body| &body.keys)
    }
}

/// A dictionary parsed where a body holds it, a stream's: the nodes of its body, of
/// which its own is the first, so that it is held behind one pointer, as
/// [`Nodes`] is.
#[derive(Clone)]
pub(crate) struct TopDict(Nodes);

impl TopDict {
    /// The dictionary `r` is at in `data`, read in `ctx` as reading an indirect
    /// object's dictionary reads it: where it is is kept relative to the start of
    /// `data`.
    pub(crate) fn read<'a>(
        r: &mut Reader<'a>,
        data: &'a [u8],
        ctx: &ReaderContext<'a>,
    ) -> Option<Self> {
        let ((), nodes) = Parse::body(data, ctx, |parse| {
            // Its place, first, as the dictionary's values are the last run.
            parse.held.nodes.push(Node::Null);
            let (run, keys) = parse.dict(r, 0, 0)?;
            parse.held.nodes[0] = Node::Dict(run, keys);
            Some(())
        })?;
        Some(Self(nodes))
    }

    /// The dictionary.
    pub(crate) fn dict(&self) -> DictNode {
        match self.0.nodes().first() {
            Some(Node::Dict(run, keys)) => self.0.dict(*run, *keys),
            _ => unreachable!("a parsed dictionary's node is the first of its body"),
        }
    }
}

/// An array parsed, with the nodes of its body.
#[derive(Clone)]
pub(crate) struct ArrayNode {
    nodes: Nodes,
    run: Run,
}

/// A dictionary parsed, with the nodes of its body. A list of entries takes a few
/// dozen bytes an entry, where a map takes about twice that and grows by doubling;
/// every dictionary of a file is parsed when it opens and kept while it is open, so
/// a file whose pages each list all the resources of the pages before them holds
/// tens of thousands of entries.
#[derive(Clone)]
pub(crate) struct DictNode {
    nodes: Nodes,
    run: Run,
    keys: u32,
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
/// ([`Index`]), and this many nodes' for the next body ([`Scratch`]); a larger one
/// is freed, so that a thread holds no more than that once it read a file, whatever
/// the file's dictionaries.
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

/// What parsing is reading: the bytes `r` reads, in `ctx`, which decrypts strings,
/// and where it keeps what it parsed so far.
struct Parse<'r, 'a> {
    data: &'a [u8],
    ctx: &'r ReaderContext<'a>,
    held: Scratch,
}

/// Where parsing a body keeps the items and entries of the arrays and dictionaries
/// it is in, one inside another, and the nodes and keys of those it parsed, each
/// run in one piece: a container's are moved in as it ends.
///
/// Kept emptied on a thread for the next body ([`Scratch::take`]), so that parsing a
/// body allocates once, for its nodes ([`Nodes::of`]).
#[derive(Default)]
struct Scratch {
    items: Vec<Node>,
    entries: Vec<(Text, Node)>,
    nodes: Vec<Node>,
    keys: Vec<Text>,
}

/// Where the nodes and keys of a value's arrays and dictionaries are, from and up
/// to, each in one piece: parsing places them as it parses the value.
#[derive(Clone, Copy, Default)]
struct Extent {
    nodes: (usize, usize),
    keys: (usize, usize),
}

impl Extent {
    fn len(self) -> usize {
        self.nodes.1 - self.nodes.0 + self.keys.1 - self.keys.0
    }

    /// Where this and `other` are, which is in one piece with it.
    fn and(self, other: Self) -> Self {
        fn and(a: (usize, usize), b: (usize, usize)) -> (usize, usize) {
            match (a.0 == a.1, b.0 == b.1) {
                (true, _) => b,
                (_, true) => a,
                _ => (a.0.min(b.0), a.1.max(b.1)),
            }
        }
        Self {
            nodes: and(self.nodes, other.nodes),
            keys: and(self.keys, other.keys),
        }
    }
}

#[cfg(feature = "std")]
std::thread_local! {
    static SCRATCHES: core::cell::RefCell<Vec<Scratch>> =
        const { core::cell::RefCell::new(Vec::new()) };
}

impl Scratch {
    /// One emptied, kept on this thread, or else a new one.
    fn take() -> Self {
        #[cfg(feature = "std")]
        if let Some(scratch) = SCRATCHES.with_borrow_mut(Vec::pop) {
            return scratch;
        }
        Self::default()
    }

    /// Keeps this, emptied, for the next body on this thread, unless one of its
    /// parts has room for more than [`KEPT`].
    fn keep(mut self) {
        self.items.clear();
        self.entries.clear();
        self.nodes.clear();
        self.keys.clear();
        #[cfg(feature = "std")]
        if self.items.capacity() <= KEPT
            && self.entries.capacity() <= KEPT
            && self.nodes.capacity() <= KEPT
            && self.keys.capacity() <= KEPT
        {
            SCRATCHES.with_borrow_mut(|kept| kept.push(self));
        }
    }

    /// Where the nodes and keys of `node`'s arrays and dictionaries are.
    fn extent(&self, node: &Node) -> Extent {
        let (run, keys) = match node {
            Node::Array(run) => (run, (0, 0)),
            Node::Dict(run, keys) => {
                let keys = *keys as usize;
                (run, (keys, keys + run.len as usize))
            }
            _ => return Extent::default(),
        };
        let at = run.at as usize;
        let own = Extent {
            nodes: (at, at + run.len as usize),
            keys,
        };
        run.nodes(&self.nodes)
            .iter()
            .fold(own, |extent, item| extent.and(self.extent(item)))
    }

    /// Moves the nodes and keys of the values of the entries from `base` on down to
    /// `to`, over those of the values they replaced, which it drops: the nodes and
    /// keys from `to` on are those of the values of these entries, and of those
    /// replaced.
    fn compact(&mut self, base: usize, to: (usize, usize)) {
        // In the order they were parsed in, which is where each starts.
        let mut parsed: Vec<(Extent, usize)> = (base..self.entries.len())
            .map(|at| (self.extent(&self.entries[at].1), at))
            .filter(|(extent, _)| extent.len() > 0)
            .collect();
        parsed.sort_unstable_by_key(|(extent, _)| extent.nodes.0);
        let (mut node, mut key) = to;
        for (extent, at) in parsed {
            let keys = if extent.keys.0 == extent.keys.1 {
                0
            } else {
                extent.keys.0 - key
            };
            let shift = (extent.nodes.0 - node, keys);
            self.entries[at].1.shift(shift);
            for from in extent.nodes.0..extent.nodes.1 {
                self.nodes.swap(node, from);
                self.nodes[node].shift(shift);
                node += 1;
            }
            for from in extent.keys.0..extent.keys.1 {
                self.keys.swap(key, from);
                key += 1;
            }
        }
        self.nodes.truncate(node);
        self.keys.truncate(key);
    }
}

impl<'r, 'a> Parse<'r, 'a> {
    /// What `read` parses of `data` in `ctx`, and the nodes of its arrays and
    /// dictionaries. Where it fails, it fails whole: an item or value that does not
    /// parse fails what holds it ([`Self::item`]).
    fn body<T>(
        data: &'a [u8],
        ctx: &'r ReaderContext<'a>,
        read: impl FnOnce(&mut Self) -> Option<T>,
    ) -> Option<(T, Nodes)> {
        let mut parse = Self {
            data,
            ctx,
            held: Scratch::take(),
        };
        let read = read(&mut parse).map(|read| (read, Nodes::of(&mut parse.held)));
        parse.held.keep();
        read
    }

    /// Makes room in `parsed` for `more`, exactly where they are the last parsed,
    /// those of what is `depth` deep, the object read ([`Nodes::of`]).
    fn last<T>(depth: usize, parsed: &mut Vec<T>, more: usize) {
        if depth == 0 {
            parsed.reserve_exact(more);
        }
    }

    /// An item of an array or a value of a dictionary `depth` deep, as
    /// `MaybeRef<Object>` skips it: a reference, or else an object.
    fn item(&mut self, r: &mut Reader<'a>, start: usize, depth: usize) -> Option<Node> {
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
    fn object(&mut self, r: &mut Reader<'a>, start: usize, depth: usize) -> Option<Node> {
        match r.peek_byte()? {
            b'<' if r.peek_bytes(2) == Some(b"<<") => {
                let (run, keys) = self.dict(r, start, depth)?;
                Some(Node::Dict(run, keys))
            }
            b'[' => Some(Node::Array(self.array(r, start, depth)?)),
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
    /// in what holds it from `start`: the run of its values, and where its keys
    /// start.
    fn dict(&mut self, r: &mut Reader<'a>, start: usize, depth: usize) -> Option<(Run, u32)> {
        if depth >= MAX_OBJECT_NESTING_DEPTH {
            return None;
        }
        let from = r.offset();
        // Where the keys of the entries are kept relative to.
        let held = &self.data[from..];
        // Its entries are those from here on, a value's own having been moved to
        // the nodes once it is parsed, and so are its values' nodes and keys.
        let base = self.held.entries.len();
        let region = (self.held.nodes.len(), self.held.keys.len());
        // The nodes and keys of the values replaced, which are dropped once they
        // are as many as those of the values kept, so that parsing holds at most
        // twice those, however often a key is written again, and as the dictionary
        // ends, so that the nodes of a dictionary parsed are those of what it
        // holds, which is where [`Scratch::extent`] finds them.
        let mut replaced = 0;
        // A key's later entry replaces its value at once, as the map of a dictionary
        // read from its bytes keeps the last: the key is found among the entries one
        // by one while they are few, and past `LISTED` as [`Large`] says, the keys
        // compared counted ([`same`], [`order`]).
        let mut large: Option<Large> = None;
        let bytes = parse_dict_with(r, false, depth + 1, Some(b"<<"), b">>", |r, key, _| {
            let key = Name::new(key)?;
            let value = self.item(r, from, depth + 1)?;
            let entries = &mut self.held.entries;
            if let Some(Large::Ascending) = large
                && entries[base..]
                    .last()
                    .is_some_and(|(last, _)| order(last.as_slice(held), &key).is_ge())
            {
                large = Some(Large::Indexed(Index::of(&entries[base..], held)));
            }
            let found = match &mut large {
                Some(Large::Indexed(index)) => index.find(&entries[base..], held, &key),
                Some(Large::Ascending) => None,
                None => entries[base..]
                    .iter()
                    .position(|(listed, _)| same(listed.as_slice(held), &key)),
            };
            if let Some(at) = found {
                let old = core::mem::replace(&mut entries[base + at].1, value);
                let scratch = &mut self.held;
                replaced += scratch.extent(&old).len();
                let all = scratch.nodes.len() - region.0 + scratch.keys.len() - region.1;
                if replaced > 0 && 2 * replaced >= all {
                    scratch.compact(base, region);
                    replaced = 0;
                }
                return Some(());
            }
            entries.push((Text::of(key.into_bytes(), self.data, from), value));
            if large.is_none() && entries.len() - base > LISTED {
                let ascending = entries[base..]
                    .windows(2)
                    .all(|pair| order(pair[0].0.as_slice(held), pair[1].0.as_slice(held)).is_lt());
                large = Some(if ascending {
                    Large::Ascending
                } else {
                    Large::Indexed(Index::of(&entries[base..], held))
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
        if replaced > 0 {
            self.held.compact(base, region);
        }
        let Scratch {
            entries,
            nodes,
            keys,
            ..
        } = &mut self.held;
        if sort {
            // Each key once, so any sort orders them alike.
            entries[base..]
                .sort_unstable_by(|(a, _), (b, _)| order(a.as_slice(held), b.as_slice(held)));
        }
        Self::last(depth, nodes, entries.len() - base);
        Self::last(depth, keys, entries.len() - base);
        let (at, keys_at) = (nodes.len(), keys.len());
        for (key, value) in entries.drain(base..) {
            keys.push(key);
            nodes.push(value);
        }
        let run = Run::of((from - start, from - start + bytes.len()), at, nodes)?;
        Some((run, u32::try_from(keys_at).ok()?))
    }

    /// An array `depth` deep, as `Array::skip_with_depth` skips it, where its items
    /// are in what holds it from `start`.
    fn array(&mut self, r: &mut Reader<'a>, start: usize, depth: usize) -> Option<Run> {
        if depth >= MAX_OBJECT_NESTING_DEPTH {
            return None;
        }
        let from = r.offset() + 1;
        // Its items are those from here on, an item's own having been moved to the
        // nodes once it is parsed.
        let base = self.held.items.len();
        parse_array_with(r, |r| {
            let item = self.item(r, from, depth + 1)?;
            self.held.items.push(item);
            Some(())
        })?;
        let Scratch { items, nodes, .. } = &mut self.held;
        let at = nodes.len();
        Self::last(depth, nodes, items.len() - base);
        nodes.extend(items.drain(base..));
        Run::of((from - start, r.offset() - 1 - start), at, nodes)
    }
}

/// An object parsed where a body holds it: its node, where it starts in the bytes
/// it is read from, which where its parts are is kept relative to, and the nodes of
/// its arrays and dictionaries.
#[derive(Clone)]
pub(crate) struct Top {
    at: usize,
    node: Node,
    nodes: Nodes,
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
        let (node, nodes) = Parse::body(data, ctx, |parse| parse.object(r, at, 0))?;
        Some(Self { at, node, nodes })
    }

    /// The dictionary `dict`, parsed where it is kept relative to the start of the
    /// bytes it is read from.
    pub(crate) fn dict(dict: TopDict) -> Self {
        let node = dict.dict();
        Self {
            at: 0,
            node: Node::Dict(node.run, node.keys),
            nodes: dict.0,
        }
    }

    /// The object, read from `data`, which it was parsed in, in `ctx`.
    pub(crate) fn object<'a>(&self, data: &'a [u8], ctx: &ReaderContext<'a>) -> Object<'a> {
        self.node.object(&self.nodes, &data[self.at..], ctx)
    }

    /// The object as written ([`Written`]), read from `data`, which it was parsed
    /// in, in `ctx`.
    pub(crate) fn written<'a>(&self, data: &'a [u8], ctx: &ReaderContext<'a>) -> Written<'a> {
        self.node.written(&self.nodes, &data[self.at..], ctx)
    }
}

impl Node {
    /// The object, of the body whose nodes are `nodes`, held in `data`, read in
    /// `ctx`; `null` for a reference, which only [`Self::item`] reads.
    pub(crate) fn object<'a>(
        &self,
        nodes: &Nodes,
        data: &'a [u8],
        ctx: &ReaderContext<'a>,
    ) -> Object<'a> {
        match self {
            Self::Null | Self::Ref(_) => Object::Null(Null),
            Self::Boolean(value) => Object::Boolean(*value),
            Self::Number(number, _) => Object::Number(*number),
            Self::String(text) => Object::String(String::from_bytes(text.bytes(data))),
            Self::Name(text) => Object::Name(Name::from_bytes(text.bytes(data))),
            Self::Array(run) => Object::Array(Array::parsed(data, nodes.array(*run), ctx)),
            Self::Dict(run, keys) => Object::Dict(Dict::parsed(data, nodes.dict(*run, *keys), ctx)),
        }
    }

    /// The item, of the body whose nodes are `nodes`, held in `data`, read in `ctx`,
    /// as written ([`Written`]).
    pub(crate) fn written<'a>(
        &self,
        nodes: &Nodes,
        data: &'a [u8],
        ctx: &ReaderContext<'a>,
    ) -> Written<'a> {
        match self {
            Self::Ref(reference) => Written::Ref(*reference),
            Self::Number(_, (from, to)) => Written::Number(&data[*from as usize..*to as usize]),
            Self::Null => Written::Null,
            Self::Boolean(value) => Written::Boolean(*value),
            Self::String(text) => Written::String(String::from_bytes(text.bytes(data))),
            Self::Name(text) => Written::Name(Name::from_bytes(text.bytes(data))),
            Self::Array(run) => Written::Array(Array::parsed(data, nodes.array(*run), ctx)),
            Self::Dict(run, keys) => {
                Written::Dict(Dict::parsed(data, nodes.dict(*run, *keys), ctx))
            }
        }
    }

    /// The item, of the body whose nodes are `nodes`, held in `data`, read in `ctx`.
    pub(crate) fn item<'a>(
        &self,
        nodes: &Nodes,
        data: &'a [u8],
        ctx: &ReaderContext<'a>,
    ) -> MaybeRef<Object<'a>> {
        match self {
            Self::Ref(reference) => MaybeRef::Ref(*reference),
            node => MaybeRef::NotRef(node.object(nodes, data, ctx)),
        }
    }

    /// The item, of the body whose nodes are `nodes`, held in `data`, read in `ctx`,
    /// as a `T`, as reading a `T` from its bytes reads it, but for the reference it
    /// may be.
    pub(crate) fn item_as<'a, T: ObjectLike<'a>>(
        &self,
        nodes: &Nodes,
        data: &'a [u8],
        ctx: &ReaderContext<'a>,
    ) -> Option<MaybeRef<T>> {
        match self.item(nodes, data, ctx) {
            MaybeRef::Ref(reference) => Some(MaybeRef::Ref(reference)),
            MaybeRef::NotRef(object) => object.cast::<T>().map(MaybeRef::NotRef),
        }
    }
}

impl Node {
    /// This, its run moved `shift` nodes and keys down, where it is an array or a
    /// dictionary.
    fn shift(&mut self, shift: (usize, usize)) {
        // Each is moved down from where it is, so no shift is more than it; an
        // empty run is read from nothing wherever it is.
        let (nodes, keys) = (shift.0 as u32, shift.1 as u32);
        match self {
            Self::Array(run) if run.len > 0 => run.at -= nodes,
            Self::Dict(run, at) if run.len > 0 => {
                run.at -= nodes;
                *at -= keys;
            }
            _ => {}
        }
    }
}

impl Nodes {
    fn array(&self, run: Run) -> ArrayNode {
        ArrayNode {
            nodes: self.clone(),
            run,
        }
    }

    fn dict(&self, run: Run, keys: u32) -> DictNode {
        DictNode {
            nodes: self.clone(),
            run,
            keys,
        }
    }
}

impl ArrayNode {
    /// Where the items are in what holds the array.
    pub(crate) fn span(&self) -> Span {
        self.run.span
    }

    /// The items.
    pub(crate) fn items(&self) -> &[Node] {
        self.run.nodes(self.nodes.nodes())
    }

    /// The nodes of the body, which the items' own arrays and dictionaries are of.
    pub(crate) fn nodes(&self) -> &Nodes {
        &self.nodes
    }
}

impl DictNode {
    /// Where the dictionary is in what holds it.
    pub(crate) fn span(&self) -> Span {
        self.run.span
    }

    /// The nodes of the body, which the values' own arrays and dictionaries are of.
    pub(crate) fn nodes(&self) -> &Nodes {
        &self.nodes
    }

    /// The values, and the key of each.
    fn entries_held(&self) -> (&[Text], &[Node]) {
        let values = self.run.nodes(self.nodes.nodes());
        if values.is_empty() {
            return (&[], values);
        }
        let at = self.keys as usize;
        (&self.nodes.keys()[at..at + values.len()], values)
    }

    pub(crate) fn len(&self) -> usize {
        self.run.len as usize
    }

    /// The value of `key`, of the dictionary whose bytes are `data`.
    pub(crate) fn get(&self, key: &[u8], data: &[u8]) -> Option<&Node> {
        let (keys, values) = self.entries_held();
        let at = if keys.len() > LISTED {
            keys.binary_search_by(|name| name.as_slice(data).cmp(key))
                .ok()?
        } else {
            keys.iter().position(|name| name.as_slice(data) == key)?
        };
        Some(&values[at])
    }

    /// The entries of the dictionary whose bytes are `data`, in no particular order.
    pub(crate) fn entries<'s, 'a: 's>(
        &'s self,
        data: &'a [u8],
    ) -> impl Iterator<Item = (Name<'a>, &'s Node)> + 's {
        let (keys, values) = self.entries_held();
        keys.iter()
            .zip(values)
            .map(move |(name, node)| (Name::from_bytes(name.bytes(data)), node))
    }
}

/// The items of a parsed array from one on, as reading the array's bytes from an
/// item reads them ([`ObjectLike::from_items`]).
pub(crate) struct Items<'r, 'a> {
    pub(crate) data: &'a [u8],
    /// The nodes of the body, which the items' own arrays and dictionaries are of.
    pub(crate) nodes: &'r Nodes,
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
        node.object(self.nodes, self.data, ctx).cast()
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
    use alloc::format;
    use alloc::string::String;

    struct Random(u64);

    impl Random {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n as u64) as usize
        }

        /// A value `depth` deep: a number, a name, or an array or a dictionary,
        /// empty ones among them, whose keys are of `keys` and written again.
        fn value(&mut self, depth: usize, keys: usize) -> String {
            match self.below(if depth > 3 { 2 } else { 6 }) {
                0 => format!("{}", self.below(100)),
                1 => format!("/N{}", self.below(3)),
                2 => String::from("[]"),
                3 => String::from("<<>>"),
                4 => {
                    let items: Vec<String> = (0..self.below(5))
                        .map(|_| self.value(depth + 1, keys))
                        .collect();
                    format!("[{}]", items.join(" "))
                }
                _ => self.dict(depth + 1, keys),
            }
        }

        fn dict(&mut self, depth: usize, keys: usize) -> String {
            let mut text = String::from("<<");
            for _ in 0..self.below(3 * keys) {
                let key = self.below(keys);
                text.push_str(&format!(" /K{key} {}", self.value(depth, keys)));
            }
            text.push_str(" >>");
            text
        }
    }

    /// The nodes and keys of what `node` holds, among `nodes`.
    fn held(nodes: &[Node], node: &Node) -> (usize, usize) {
        let (run, own) = match node {
            Node::Array(run) => (run, 0),
            Node::Dict(run, _) => (run, run.len as usize),
            _ => return (0, 0),
        };
        run.nodes(nodes)
            .iter()
            .map(|item| held(nodes, item))
            .fold((run.len as usize, own), |(n, k), (a, b)| (n + a, k + b))
    }

    /// A dictionary whose keys are written again, valued with arrays and
    /// dictionaries, some empty, whose own keys are written again, on both sides of
    /// the keys past which they are sorted, parses to what reading its bytes reads,
    /// however the nodes of the values it kept were moved over those it replaced.
    #[test]
    fn a_dictionary_whose_keys_are_written_again_parses_as_it_reads() {
        let mut random = Random(0x5eed);
        for keys in [2, 4, 12] {
            for _ in 0..500 {
                let text = random.dict(0, keys);
                let data = text.as_bytes();
                let ctx = ReaderContext::dummy();
                let parsed = TopDict::read(&mut Reader::new(data), data, &ctx).unwrap();
                // The body holds the nodes and keys of the values kept, and no more.
                let body = &parsed.0;
                assert_eq!(
                    held(body.nodes(), &body.nodes()[0]),
                    (body.nodes().len() - 1, body.keys().len()),
                    "{text}"
                );
                let parsed = Object::Dict(Dict::parsed(data, parsed.dict(), &ctx));
                let read = Reader::new(data)
                    .read_with_context::<Dict<'_>>(&ctx)
                    .unwrap();
                assert_eq!(
                    format!("{parsed}"),
                    format!("{}", Object::Dict(read)),
                    "{text}"
                );
            }
        }
    }

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
