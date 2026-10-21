//! Objects parsed with every array and dictionary in them, which reads of the body
//! of an object the table lists are made from ([`crate::xref`]): a read of any type,
//! of the object or of anything in it, walks what was parsed and never lexes the
//! body's bytes again, however many reads share the parse. The table hands a body's
//! parse to the reads made while one holds it, and keeps it from its third parse on.
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
use crate::object::number::starts_number;
use crate::object::{
    Array, Bytes, Dict, MAX_OBJECT_NESTING_DEPTH, MaybeRef, Name, Null, Number, ObjRef, Object,
    ObjectLike, String, Written, leaf,
};
use crate::reader::{Reader, ReaderContext, ReaderExt};
use crate::sync::{Arc, Weak};
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
    /// ([`Nodes`], [`Keys`]). An entry takes a few dozen bytes, where a map takes
    /// about twice that and grows by doubling; a dictionary read again is kept
    /// while its file is open, and a file whose pages each list all the resources
    /// of the pages before them holds tens of thousands of entries.
    Dict(Run, Keys),
}

/// Where the keys of a dictionary start among the keys of its body, and how one is
/// found among them: those of a small dictionary entry by entry, in the file's
/// order ([`LISTED`]); those of a larger one by halving, sorted unless they came in
/// order; and those of [`TABLED`] or more that came out of order by the body's key
/// table ([`KeyTable`]), in the file's order, so that parsing sorts none of them.
/// Without `std`, which has no hash keyed at random, those too are sorted.
#[derive(Clone, Copy)]
pub(crate) struct Keys(u32);

impl Keys {
    /// Set where the keys are found by the body's key table.
    const HASHED: u32 = 1 << 31;

    /// Keys from `at` on, found by the body's key table where `hashed`; none for a body
    /// of 2 Gi keys or more, which parsing would hold in more than 48 GiB.
    fn new(at: usize, hashed: bool) -> Option<Self> {
        let at = u32::try_from(at).ok().filter(|at| *at < Self::HASHED)?;
        Some(Self(at | if hashed { Self::HASHED } else { 0 }))
    }

    fn at(self) -> usize {
        (self.0 & !Self::HASHED) as usize
    }

    #[cfg(feature = "std")]
    fn hashed(self) -> bool {
        self.0 & Self::HASHED != 0
    }

    /// The same, from `at` on.
    fn moved(self, at: usize) -> Self {
        Self(at as u32 | (self.0 & Self::HASHED))
    }
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
        u32::try_from(nodes.len()).ok()?;
        Some(Self {
            span,
            at: u32::try_from(at).ok()?,
            len: u32::try_from(nodes.len() - at).ok()?,
        })
    }

    /// Its nodes, among `nodes`; none, wherever it is, where it is empty, as
    /// dropping the garbage leaves an empty one where it was ([`Scratch::compact`]).
    fn nodes(self, nodes: &[Node]) -> &[Node] {
        if self.len == 0 {
            return &[];
        }
        let at = self.at as usize;
        &nodes[at..at + self.len as usize]
    }
}

/// The items and values of every array and dictionary of a body, parsed together,
/// and the keys of its dictionaries, which every read made of the parse shares;
/// none where it has none. Held behind one pointer, as the table keeps one for each
/// object ([`crate::xref`]); keys are held apart from the nodes as they take less
/// room.
#[derive(Clone, Default)]
pub(crate) struct Nodes(Option<Arc<Body>>);

struct Body {
    nodes: Box<[Node]>,
    keys: Box<[Text]>,
    /// Where the keys of its dictionaries of [`TABLED`] keys or more that came out
    /// of order are, where it has any ([`Keys`]).
    #[cfg(feature = "std")]
    table: Option<KeyTable>,
}

/// Where the keys of the dictionaries of a body of [`TABLED`] keys or more that came
/// out of order are among its keys ([`Keys`]): slots, each empty or a key's place, a key found
/// from the slot of a hash, keyed at random, of the key and where its dictionary's
/// keys start, slot by slot. A third more slots than keys at least, 4 bytes each,
/// so that at most 3 in 4 are taken and a key is found in a few: a dictionary of
/// tens of thousands of keys, which a file whose pages each list the resources of
/// the pages before them holds, is parsed in time linear in its keys, where sorting
/// them took that time times their logarithm, and each key is found in a step,
/// where halving took a logarithm of steps.
#[cfg(feature = "std")]
struct KeyTable {
    state: hash::State,
    slots: Box<[u32]>,
}

#[cfg(feature = "std")]
impl KeyTable {
    const EMPTY: u32 = u32::MAX;

    /// Where `key`, of the dictionary whose keys start at `at`, is first looked for.
    fn slot(&self, at: usize, key: &[u8]) -> usize {
        use core::hash::BuildHasher;
        self.state.hash_one((at, key)) as usize & (self.slots.len() - 1)
    }

    /// The table of the dictionaries of `nodes`, whose own is the first, held from
    /// `start` on in `data`, and whose keys are `keys`; none where none of them has
    /// its keys found by one ([`Keys`]).
    fn of(nodes: &[Node], keys: &[Text], data: &[u8], start: usize) -> Option<Self> {
        let hashed: usize = nodes
            .iter()
            .map(|node| match node {
                Node::Dict(run, at) if at.hashed() => run.len as usize,
                _ => 0,
            })
            .sum();
        if hashed == 0 {
            return None;
        }
        // Fewer than 2^31 keys ([`Keys::new`]), so the room fits a usize, and a
        // key's place a slot.
        let room = (hashed + hashed / 3 + 1).next_power_of_two();
        crate::work::told(room);
        let mut table = Self {
            state: hash::State::default(),
            slots: alloc::vec![Self::EMPTY; room].into_boxed_slice(),
        };
        table.add(nodes, keys, &nodes[0], data, start);
        Some(table)
    }

    /// Adds the keys of the dictionaries of `node`, all the way down, which is held
    /// in the bytes of `data` from `start` on.
    fn add(&mut self, nodes: &[Node], keys: &[Text], node: &Node, data: &[u8], start: usize) {
        let (run, at) = match node {
            Node::Array(run) => (run, None),
            Node::Dict(run, at) => (run, Some(*at)),
            _ => return,
        };
        // Where its own bytes start, which its keys and its items' places are kept
        // relative to.
        let from = start + run.span.0;
        if let Some(at) = at.filter(|at| at.hashed()) {
            let held = &data[from..];
            let mask = self.slots.len() - 1;
            let own = &keys[at.at()..at.at() + run.len as usize];
            for (place, key) in (at.at()..).zip(own) {
                let key = key.as_slice(held);
                crate::work::told(1 + key.len());
                let mut slot = self.slot(at.at(), key);
                while self.slots[slot] != Self::EMPTY {
                    slot = (slot + 1) & mask;
                }
                // Fewer than 2^31 keys ([`Keys::new`]).
                self.slots[slot] = place as u32;
            }
        }
        for item in run.nodes(nodes) {
            self.add(nodes, keys, item, data, from);
        }
    }

    /// Where `key` is among the `len` keys of a dictionary, those from `at` on among
    /// `keys`, kept relative to `held`.
    fn find(&self, keys: &[Text], at: usize, len: usize, held: &[u8], key: &[u8]) -> Option<usize> {
        let mask = self.slots.len() - 1;
        let mut slot = self.slot(at, key);
        loop {
            let place = self.slots[slot];
            if place == Self::EMPTY {
                return None;
            }
            // The key of another dictionary is passed over.
            let found = (place as usize).wrapping_sub(at);
            if found < len && keys[place as usize].as_slice(held) == key {
                return Some(found);
            }
            slot = (slot + 1) & mask;
        }
    }
}

impl Nodes {
    /// Those `parsed` holds, which this takes, leaving its room: the nodes of a body
    /// held from `start` on in `data`.
    fn of(parsed: &mut Scratch, _data: &[u8], _start: usize) -> Self {
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
        let (nodes, keys) = (take(&mut parsed.nodes), take(&mut parsed.keys));
        Self(Some(Arc::new(Body {
            #[cfg(feature = "std")]
            table: KeyTable::of(&nodes, &keys, _data, _start),
            nodes,
            keys,
        })))
    }

    fn nodes(&self) -> &[Node] {
        self.0.as_ref().map_or(&[], |body| &body.nodes)
    }

    fn keys(&self) -> &[Text] {
        self.0.as_ref().map_or(&[], |body| &body.keys)
    }

    #[cfg(feature = "std")]
    fn table(&self) -> Option<&KeyTable> {
        self.0.as_ref()?.table.as_ref()
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
        let ((), nodes) = Parse::body(data, ctx, 0, |parse| {
            let (run, keys) = parse.dict(r, 0, 0)?;
            parse.held.nodes[0] = Node::Dict(run, keys);
            Some(())
        })?;
        Some(Self(nodes))
    }

    /// The dictionary, read from `data`, which it was parsed in, in `ctx`.
    pub(crate) fn dict<'a>(&self, data: &'a [u8], ctx: &ReaderContext<'a>) -> Dict<'a> {
        Dict::parsed(Part::root(data, &self.0, ctx))
    }
}

/// What the reads of the arrays and dictionaries of one read of an object share:
/// the bytes the object's node is kept relative to, the nodes of its body, and the
/// context they are read in.
pub(crate) struct Held<'a> {
    data: &'a [u8],
    nodes: Nodes,
    ctx: ReaderContext<'a>,
}

/// An array or a dictionary of an object read from its body: what the read holds,
/// which reading what it holds shares, its node, the `at`th of the body's, and
/// where the bytes of what holds it start in the bytes the read holds. In 32 bits
/// each, so that it takes two words, as an object holds one ([`Object`]): one that
/// starts 4 GiB or more into those bytes holds what it is read from itself.
#[derive(Clone)]
pub(crate) struct Part<'a> {
    held: Arc<Held<'a>>,
    at: u32,
    offset: u32,
}

/// At most this many entries are kept in the file's order, which a key is searched
/// in entry by entry; more are searched by halving or by the key table ([`Keys`]).
const LISTED: usize = 8;

/// At least this many keys out of order are found by the body's key table
/// ([`KeyTable`]), which holds 5 to 11 bytes more for each; fewer are sorted, a
/// logarithm of comparisons each, 10 at most.
const TABLED: usize = 1024;

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

    /// What this index finds `key` by, the bytes hashed counted
    /// ([`crate::work::TOLD`]).
    #[cfg(feature = "std")]
    fn hash(&self, key: &[u8]) -> Hashed {
        use core::hash::BuildHasher;
        crate::work::told(1 + key.len());
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
    /// How many of the nodes and keys are garbage: those of values a key's later
    /// entry replaced, which nothing holds ([`Scratch::replaced`]).
    garbage: (usize, usize),
}

/// Drops what `node`'s arrays and dictionaries hold, all the way down, among `nodes`
/// and `keys`, decoded strings and names included, leaving their places to be
/// dropped with the garbage; how many nodes and keys those are.
fn drop_held(nodes: &mut [Node], keys: &mut [Text], node: &Node) -> (usize, usize) {
    let (run, at) = match *node {
        Node::Array(run) => (run, None),
        Node::Dict(run, at) => (run, Some(at.at())),
        _ => return (0, 0),
    };
    let len = run.len as usize;
    if len == 0 {
        return (0, 0);
    }
    let mut held = (len, 0);
    if let Some(at) = at {
        keys[at..at + len].fill(Text::At((0, 0)));
        held.1 = len;
    }
    let at = run.at as usize;
    for item in at..at + len {
        let item = core::mem::replace(&mut nodes[item], Node::Null);
        let (n, k) = drop_held(nodes, keys, &item);
        held = (held.0 + n, held.1 + k);
    }
    held
}

/// The nodes and keys being moved, from where they were to new room.
struct Moving {
    from: (Vec<Node>, Vec<Text>),
    to: (Vec<Node>, Vec<Text>),
}

impl Moving {
    /// Moves the nodes and keys that `node`'s arrays and dictionaries hold to the
    /// end of the new room, those of each after those of what it holds.
    fn node(&mut self, node: &mut Node) {
        let (run, keys) = match node {
            Node::Array(run) if run.len > 0 => (run, None),
            Node::Dict(run, keys) if run.len > 0 => (run, Some(keys)),
            _ => return,
        };
        let (at, len) = (run.at as usize, run.len as usize);
        for item in at..at + len {
            let mut held = core::mem::replace(&mut self.from.0[item], Node::Null);
            self.node(&mut held);
            self.from.0[item] = held;
        }
        // No more are moved than were there, as many as a run fits ([`Run::of`]).
        run.at = self.to.0.len() as u32;
        let items = self.from.0[at..at + len].iter_mut();
        self.to
            .0
            .extend(items.map(|item| core::mem::replace(item, Node::Null)));
        if let Some(keys) = keys {
            let at = keys.at();
            *keys = keys.moved(self.to.1.len());
            let names = self.from.1[at..at + len].iter_mut();
            self.to
                .1
                .extend(names.map(|name| core::mem::replace(name, Text::At((0, 0)))));
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
        self.garbage = (0, 0);
        #[cfg(feature = "std")]
        if self.items.capacity() <= KEPT
            && self.entries.capacity() <= KEPT
            && self.nodes.capacity() <= KEPT
            && self.keys.capacity() <= KEPT
        {
            SCRATCHES.with_borrow_mut(|kept| kept.push(self));
        }
    }

    /// Drops `old`, the value of an entry a key's later entry replaced, whose nodes
    /// and keys are garbage from now on. The garbage is dropped once it is a third
    /// of all parsing holds, so that parsing holds half again what it keeps at most,
    /// however often a key is written again, and dropping it takes three times the
    /// work of making it at most ([`Self::compact`]).
    fn replaced(&mut self, old: &Node) {
        let (nodes, keys) = drop_held(&mut self.nodes, &mut self.keys, old);
        if nodes + keys == 0 {
            return;
        }
        crate::work::told(nodes + keys);
        self.garbage = (self.garbage.0 + nodes, self.garbage.1 + keys);
        let all = self.nodes.len() + self.keys.len() + self.items.len() + self.entries.len();
        if 3 * (self.garbage.0 + self.garbage.1) >= all {
            self.compact((0, 0));
        }
    }

    /// Drops the garbage: moves the nodes and keys that the object's own node, the
    /// first, and the items and entries of the arrays and dictionaries being parsed
    /// hold to new room, with room for `more` besides, and frees the old.
    fn compact(&mut self, more: (usize, usize)) {
        let Self {
            items,
            entries,
            nodes,
            keys,
            garbage,
        } = self;
        crate::work::told(nodes.len() + keys.len() + items.len() + entries.len());
        let mut moving = Moving {
            to: (
                Vec::with_capacity(nodes.len() - garbage.0 + more.0),
                Vec::with_capacity(keys.len() - garbage.1 + more.1),
            ),
            from: (core::mem::take(nodes), core::mem::take(keys)),
        };
        let mut own = core::mem::replace(&mut moving.from.0[0], Node::Null);
        moving.to.0.push(Node::Null);
        moving.node(&mut own);
        moving.to.0[0] = own;
        for item in items.iter_mut() {
            moving.node(item);
        }
        for (_, value) in entries.iter_mut() {
            moving.node(value);
        }
        (*nodes, *keys) = moving.to;
        *garbage = (0, 0);
    }

    /// Makes room for `more` nodes and keys, of what is `depth` deep, exactly where
    /// they are the last parsed, those of the object read ([`Nodes::of`]), and drops
    /// the garbage before them, so that a body holds none.
    fn last(&mut self, depth: usize, more: (usize, usize)) {
        if depth > 0 {
            return;
        }
        if self.garbage != (0, 0) {
            self.compact(more);
        } else {
            self.nodes.reserve_exact(more.0);
            self.keys.reserve_exact(more.1);
        }
    }
}

impl<'r, 'a> Parse<'r, 'a> {
    /// What `read` parses of `data` in `ctx`, and the nodes of its arrays and
    /// dictionaries. Where it fails, it fails whole: an item or value that does not
    /// parse fails what holds it ([`Self::item`]).
    fn body<T>(
        data: &'a [u8],
        ctx: &'r ReaderContext<'a>,
        start: usize,
        read: impl FnOnce(&mut Self) -> Option<T>,
    ) -> Option<(T, Nodes)> {
        let mut parse = Self {
            data,
            ctx,
            held: Scratch::take(),
        };
        // The place of what is read, first, where it is an array or a dictionary,
        // its own run being the last.
        parse.held.nodes.push(Node::Null);
        let read = read(&mut parse).map(|read| {
            debug_assert_eq!(parse.held.garbage, (0, 0), "the object's run dropped it");
            (read, Nodes::of(&mut parse.held, data, start))
        });
        parse.held.keep();
        read
    }

    /// An item of an array or a value of a dictionary `depth` deep, as
    /// `MaybeRef<Object>` skips it: a reference, or else an object.
    fn item(&mut self, r: &mut Reader<'a>, start: usize, depth: usize) -> Option<Node> {
        // A reference starts with a number, which most items do not.
        if r.peek_byte().is_some_and(starts_number)
            && let Some(reference) = r.read_without_context::<ObjRef>()
        {
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
    fn dict(&mut self, r: &mut Reader<'a>, start: usize, depth: usize) -> Option<(Run, Keys)> {
        if depth >= MAX_OBJECT_NESTING_DEPTH {
            return None;
        }
        let from = r.offset();
        // Where the keys of the entries are kept relative to.
        let held = &self.data[from..];
        // Its entries are those from here on, a value's own having been moved to
        // the nodes once it is parsed, and so are its values' nodes and keys.
        let base = self.held.entries.len();
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
                self.held.replaced(&old);
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
        // Keys that came out of order: found by the body's table, or else sorted.
        let hashed = match large {
            Some(Large::Indexed(index)) => {
                index.keep();
                true
            }
            _ => false,
        };
        let bytes = bytes?;
        let more = self.held.entries.len() - base;
        self.held.last(depth, (more, more));
        let Scratch {
            entries,
            nodes,
            keys,
            ..
        } = &mut self.held;
        let tabled = cfg!(feature = "std") && hashed && more >= TABLED;
        if hashed && !tabled {
            // Each key once, so any sort orders them alike.
            entries[base..]
                .sort_unstable_by(|(a, _), (b, _)| order(a.as_slice(held), b.as_slice(held)));
        }
        let (at, keys_at) = (nodes.len(), keys.len());
        for (key, value) in entries.drain(base..) {
            keys.push(key);
            nodes.push(value);
        }
        let run = Run::of((from - start, from - start + bytes.len()), at, nodes)?;
        Some((run, Keys::new(keys_at, tabled)?))
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
        let more = self.held.items.len() - base;
        self.held.last(depth, (more, 0));
        let Scratch { items, nodes, .. } = &mut self.held;
        let at = nodes.len();
        nodes.extend(items.drain(base..));
        Run::of((from - start, r.offset() - 1 - start), at, nodes)
    }
}

/// An array or a dictionary parsed where a body holds it, while a read holds its
/// nodes ([`Top::once`]).
#[derive(Clone)]
pub(crate) struct Once {
    at: usize,
    body: Weak<Body>,
}

impl Once {
    /// The object, if a read still holds its nodes.
    pub(crate) fn top(&self) -> Option<Top> {
        let body = self.body.upgrade()?;
        Some(Top {
            at: self.at,
            node: body.nodes[0].clone(),
            nodes: Nodes(Some(body)),
        })
    }

    /// The dictionary of a stream, if a read still holds its nodes.
    pub(crate) fn dict(&self) -> Option<TopDict> {
        Some(TopDict(Nodes(Some(self.body.upgrade()?))))
    }

    /// Whether no read holds its nodes any more, which are dropped.
    pub(crate) fn ended(&self) -> bool {
        self.body.strong_count() == 0
    }
}

impl TopDict {
    /// The dictionary, while a read holds its nodes.
    pub(crate) fn once(&self) -> Option<Once> {
        Some(Once {
            at: 0,
            body: Arc::downgrade(self.0.0.as_ref()?),
        })
    }
}

/// An object parsed where a body holds it: its node, where it starts in the bytes
/// it is read from, which where its parts are is kept relative to, and the nodes of
/// its arrays and dictionaries, of which its own, where it is one, is the first.
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
        let (node, nodes) = Parse::body(data, ctx, at, |parse| {
            let node = parse.object(r, at, 0)?;
            match node {
                Node::Array(_) | Node::Dict(..) => parse.held.nodes[0] = node.clone(),
                // Nothing else was parsed, so the body has no nodes.
                _ => parse.held.nodes.clear(),
            }
            Some(node)
        })?;
        Some(Self { at, node, nodes })
    }

    /// The array or dictionary, while a read holds its nodes; none for another
    /// object, which holds none.
    pub(crate) fn once(&self) -> Option<Once> {
        Some(Once {
            at: self.at,
            body: Arc::downgrade(self.nodes.0.as_ref()?),
        })
    }

    /// The dictionary `dict`, parsed where it is kept relative to the start of the
    /// bytes it is read from.
    pub(crate) fn dict(dict: TopDict) -> Self {
        Self {
            at: 0,
            node: dict.0.nodes()[0].clone(),
            nodes: dict.0,
        }
    }

    /// The object, read from `data`, which it was parsed in, in `ctx`.
    pub(crate) fn object<'a>(&self, data: &'a [u8], ctx: &ReaderContext<'a>) -> Object<'a> {
        let data = &data[self.at..];
        self.node
            .object(data, || Part::root(data, &self.nodes, ctx))
    }

    /// The object as written ([`Written`]), read from `data`, which it was parsed
    /// in, in `ctx`.
    pub(crate) fn written<'a>(&self, data: &'a [u8], ctx: &ReaderContext<'a>) -> Written<'a> {
        let data = &data[self.at..];
        self.node
            .written(data, || Part::root(data, &self.nodes, ctx))
    }
}

impl Node {
    /// The object, held in `data`, an array or a dictionary as `part` reads it;
    /// `null` for a reference, which only [`Part::item`] reads.
    fn object<'a>(&self, data: &'a [u8], part: impl FnOnce() -> Part<'a>) -> Object<'a> {
        match self {
            Self::Null | Self::Ref(_) => Object::Null(Null),
            Self::Boolean(value) => Object::Boolean(*value),
            Self::Number(number, _) => Object::Number(*number),
            Self::String(text) => Object::String(String::from_bytes(text.bytes(data))),
            Self::Name(text) => Object::Name(Name::from_bytes(text.bytes(data))),
            Self::Array(_) => Object::Array(Array::parsed(part())),
            Self::Dict(..) => Object::Dict(Dict::parsed(part())),
        }
    }

    /// The item, held in `data`, an array or a dictionary as `part` reads it, as
    /// written ([`Written`]).
    fn written<'a>(&self, data: &'a [u8], part: impl FnOnce() -> Part<'a>) -> Written<'a> {
        match self {
            Self::Ref(reference) => Written::Ref(*reference),
            Self::Number(_, (from, to)) => Written::Number(&data[*from as usize..*to as usize]),
            Self::Null => Written::Null,
            Self::Boolean(value) => Written::Boolean(*value),
            Self::String(text) => Written::String(String::from_bytes(text.bytes(data))),
            Self::Name(text) => Written::Name(Name::from_bytes(text.bytes(data))),
            Self::Array(_) => Written::Array(Array::parsed(part())),
            Self::Dict(..) => Written::Dict(Dict::parsed(part())),
        }
    }
}

impl<'a> Part<'a> {
    /// The object whose node is the first of `nodes`, an array or a dictionary,
    /// kept relative to `data`, read in `ctx`.
    fn root(data: &'a [u8], nodes: &Nodes, ctx: &ReaderContext<'a>) -> Self {
        Self {
            held: Arc::new(Held {
                data,
                nodes: nodes.clone(),
                ctx: ctx.clone(),
            }),
            at: 0,
            offset: 0,
        }
    }

    fn node(&self) -> &Node {
        &self.held.nodes.nodes()[self.at as usize]
    }

    fn run(&self) -> Run {
        match self.node() {
            Node::Array(run) | Node::Dict(run, _) => *run,
            _ => unreachable!("a part is an array or a dictionary"),
        }
    }

    /// The bytes of the array's items, or of the dictionary.
    pub(crate) fn data(&self) -> &'a [u8] {
        let (from, to) = self.run().span;
        let (offset, data) = (self.offset as usize, self.held.data);
        &data[offset + from..offset + to]
    }

    /// The context it is read in.
    pub(crate) fn ctx(&self) -> &ReaderContext<'a> {
        &self.held.ctx
    }

    /// This, read in `ctx`.
    pub(crate) fn in_context(&self, ctx: &ReaderContext<'a>) -> Self {
        Self {
            held: Arc::new(Held {
                data: self.held.data,
                nodes: self.held.nodes.clone(),
                ctx: ctx.clone(),
            }),
            ..*self
        }
    }

    /// The array's items, or the dictionary's values.
    pub(crate) fn items(&self) -> &[Node] {
        self.run().nodes(self.held.nodes.nodes())
    }

    pub(crate) fn len(&self) -> usize {
        self.run().len as usize
    }

    /// The dictionary's keys, one for each value; none for an array.
    fn keys(&self) -> &[Text] {
        match self.node() {
            Node::Dict(run, at) if run.len > 0 => {
                let at = at.at();
                &self.held.nodes.keys()[at..at + run.len as usize]
            }
            _ => &[],
        }
    }

    /// Where the value of `key` is among the dictionary's values.
    pub(crate) fn find(&self, key: &[u8]) -> Option<usize> {
        let (keys, data) = (self.keys(), self.data());
        #[cfg(feature = "std")]
        if let Node::Dict(run, at) = self.node()
            && at.hashed()
            && let Some(table) = self.held.nodes.table()
        {
            return table.find(self.held.nodes.keys(), at.at(), run.len as usize, data, key);
        }
        if keys.len() > LISTED {
            keys.binary_search_by(|name| name.as_slice(data).cmp(key))
                .ok()
        } else {
            keys.iter().position(|name| name.as_slice(data) == key)
        }
    }

    /// The dictionary's keys, in no particular order.
    pub(crate) fn names(&self) -> impl Iterator<Item = Name<'a>> + '_ {
        let data = self.data();
        self.keys()
            .iter()
            .map(move |name| Name::from_bytes(name.bytes(data)))
    }

    /// The `at`th item or value; `null` for a reference, which only [`Self::item`]
    /// reads.
    pub(crate) fn object(&self, at: usize) -> Object<'a> {
        self.items()[at].object(self.data(), || self.part(at))
    }

    /// The `at`th item or value, as written ([`Written`]).
    pub(crate) fn written(&self, at: usize) -> Written<'a> {
        self.items()[at].written(self.data(), || self.part(at))
    }

    /// The `at`th item or value.
    pub(crate) fn item(&self, at: usize) -> MaybeRef<Object<'a>> {
        match &self.items()[at] {
            Node::Ref(reference) => MaybeRef::Ref(*reference),
            _ => MaybeRef::NotRef(self.object(at)),
        }
    }

    /// The `at`th item or value as a `T`, as reading a `T` from its bytes reads it,
    /// but for the reference it may be.
    pub(crate) fn item_as<T: ObjectLike<'a>>(&self, at: usize) -> Option<MaybeRef<T>> {
        match self.item(at) {
            MaybeRef::Ref(reference) => Some(MaybeRef::Ref(reference)),
            MaybeRef::NotRef(object) => object.cast::<T>().map(MaybeRef::NotRef),
        }
    }

    /// The `at`th item or value, an array or a dictionary, which shares what this
    /// read holds.
    fn part(&self, at: usize) -> Self {
        let run = self.run();
        // A run ends where the nodes of its body do, in 32 bits ([`Run::of`]).
        let node = run.at + at as u32;
        let offset = self.offset as usize + run.span.0;
        match u32::try_from(offset) {
            Ok(offset) => Self {
                held: self.held.clone(),
                at: node,
                offset,
            },
            Err(_) => Self {
                held: Arc::new(Held {
                    data: &self.held.data[offset..],
                    nodes: self.held.nodes.clone(),
                    ctx: self.held.ctx.clone(),
                }),
                at: node,
                offset: 0,
            },
        }
    }
}

/// The items of a parsed array from one on, as reading the array's bytes from an
/// item reads them ([`ObjectLike::from_items`]).
pub(crate) struct Items<'r, 'a> {
    pub(crate) part: &'r Part<'a>,
    pub(crate) at: usize,
}

impl<'a> Items<'_, 'a> {
    /// The next item, which is no reference, as a `T`, as `T::read` reads it.
    pub(crate) fn next_as<T: ObjectLike<'a>>(&mut self) -> Option<T> {
        let node = self.part.items().get(self.at)?;
        if matches!(node, Node::Ref(_)) {
            return None;
        }
        self.at += 1;
        self.part.object(self.at - 1).cast()
    }

    /// The next item as a `T`, resolved where it is a reference, as reading a
    /// `MaybeRef<T>` and resolving it reads it.
    pub(crate) fn next_resolved<T: ObjectLike<'a>>(
        &mut self,
        ctx: &ReaderContext<'a>,
    ) -> Option<T> {
        match self.part.items().get(self.at)? {
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
                    drop_held(
                        &mut body.nodes().to_vec(),
                        &mut body.keys().to_vec(),
                        &body.nodes()[0]
                    ),
                    (body.nodes().len() - 1, body.keys().len()),
                    "{text}"
                );
                let parsed = Object::Dict(parsed.dict(data, &ctx));
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
