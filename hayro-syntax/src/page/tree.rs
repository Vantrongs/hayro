//! The walks of the page tree: the one that lists the pages ([`super::Pages`]), and
//! the one that repairs the /Parent entries ([`super::inherit`]).
//!
//! Both read each /Kids array once, however many nodes, or entries of the table
//! leading to one node, list it, and keep their own stack, so that neither the time
//! they take nor the stack they use grows with how the tree shares nodes and arrays
//! or with how deep it is.

use crate::hash;
use crate::lost::Lost;
use crate::object::dict::keys::{CONTENTS, KIDS, MEDIA_BOX, PAGE, PAGES, TYPE};
use crate::object::{Array, Dict, MaybeRef, Name, Object, ObjectIdentifier, ObjectLike};
use crate::reader::ReaderContext;
use crate::xref::{ContentKey, Shallow, XRef};
use alloc::vec::Vec;

/// An entry of the page tree, told apart from the others: an object by its number
/// and generation, through whichever entry of the table it is reached, and a
/// dictionary written into another object by where its bytes are, so that it is one
/// entry wherever the object holding it is read from.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Key<'a> {
    Object(ObjectIdentifier),
    Written(ContentKey<'a>),
}

impl<'a> Key<'a> {
    /// The key of `dict`, the object `id` or, without one, written where it is.
    pub(crate) fn of(id: Option<ObjectIdentifier>, dict: &Dict<'a>) -> Self {
        match id {
            Some(id) => Self::Object(id),
            None => Self::Written(dict.content_key()),
        }
    }
}

/// A page as the walk lists it: its object, its dictionary, empty for an entry that
/// is no dictionary, and the node that lists it ([`Tree::nodes`]); none for the
/// document's only page where the root is no node, and for a page found without
/// the tree.
pub(crate) struct Listed<'a> {
    pub(crate) id: Option<ObjectIdentifier>,
    pub(crate) dict: Dict<'a>,
    pub(crate) under: Option<u32>,
}

/// A node the walk read, at its first listing.
pub(crate) struct Node<'a> {
    pub(crate) dict: Dict<'a>,
    /// The node that lists it there; none for the root.
    pub(crate) above: Option<u32>,
    /// The last of the nodes read below it: those read below a node are the ones
    /// after it up to this one, as the walk reads a node's whole subtree before it
    /// goes on.
    pub(crate) last: u32,
}

/// The /Parent each entry of the tree gets from the repair, with its dictionary.
pub(crate) type Parents<'a> = hash::Map<Key<'a>, (Key<'a>, Dict<'a>)>;

/// What the walk of the /Kids read: the root, and the nodes, each at its first
/// listing, which the pages it lists are found under.
#[derive(Default)]
pub(crate) struct Tree<'a> {
    pub(crate) root: Option<Key<'a>>,
    nodes: Vec<Node<'a>>,
    read: hash::Map<Key<'a>, u32>,
}

impl<'a> Tree<'a> {
    /// The tree as the walk that listed the pages read it, read again: the same
    /// walk of the same objects reads the same nodes in the same order. What
    /// reading it loses is no lookup's.
    pub(crate) fn read(xref: &'a XRef) -> Self {
        xref.apart(|| root(xref).map(|(id, root)| walk(xref, id, root).0))
            .0
            .unwrap_or_default()
    }

    /// The node `index`.
    pub(crate) fn node(&self, index: u32) -> &Node<'a> {
        &self.nodes[index as usize]
    }

    /// The node the walk read as `key`, if it read one.
    pub(crate) fn read_as(&self, key: &Key<'a>) -> Option<u32> {
        self.read.get(key).copied()
    }

    /// Whether a page that node `under` lists is below node `node`, or listed by it.
    pub(crate) fn below(&self, under: u32, node: u32) -> bool {
        node <= under && under <= self.node(node).last
    }

    /// The /Parent entries as the repair gives them ([`super::inherit`], rule 6),
    /// from the root, read again: a repair is rare, and the tree is not kept for it;
    /// and the first object reading the tree for it lost.
    pub(crate) fn repair(xref: &'a XRef) -> (Parents<'a>, Lost) {
        xref.apart(|| repair(xref, root(xref)))
    }
}

/// The value of `key` in `dict` as a `T`, read through a reference as [`XRef::get`]
/// reads an object, not through the chain of references `dict` was read through:
/// that chain holds the object `dict` is in, whose read is complete, so a reference
/// back to it is that object, not a cycle. Reading the object a reference leads to
/// still rejects a reference its own read meets again.
pub(super) fn value<'a, T: ObjectLike<'a>>(
    xref: &'a XRef,
    dict: &Dict<'a>,
    key: &[u8],
) -> Option<T> {
    match dict.get_raw::<T>(key)? {
        MaybeRef::Ref(r) => xref.get::<T>(r.into()),
        MaybeRef::NotRef(value) => Some(value),
    }
}

/// Whether a value of `dict` for `key` is an entry: a direct `null`, or a value that
/// cannot be read, is none.
fn present(dict: &Dict<'_>, key: &[u8]) -> bool {
    !matches!(
        dict.get_raw::<Object<'_>>(key),
        None | Some(MaybeRef::NotRef(Object::Null(_)))
    )
}

/// Whether `dict` is a node, whose kids are listed in its place, rather than a page.
fn is_node<'a>(xref: &'a XRef, dict: &Dict<'a>) -> bool {
    match dict.get_raw::<Object<'_>>(TYPE) {
        None | Some(MaybeRef::NotRef(Object::Null(_))) => {
            present(dict, KIDS) && !present(dict, MEDIA_BOX)
        }
        Some(MaybeRef::NotRef(Object::Name(name))) => name.as_ref() == PAGES,
        Some(MaybeRef::Ref(_)) => {
            value::<Name<'_>>(xref, dict, TYPE).is_some_and(|name| name.as_ref() == PAGES)
        }
        Some(MaybeRef::NotRef(_)) => false,
    }
}

/// The dictionary an item of a /Kids array is or refers to, a stream's dictionary
/// for a stream, and the object it refers to.
fn entry<'a>(
    xref: &'a XRef,
    item: MaybeRef<Object<'a>>,
) -> (Option<ObjectIdentifier>, Option<Dict<'a>>) {
    match item {
        MaybeRef::Ref(r) => {
            let id = ObjectIdentifier::from(r);
            (Some(id), xref.get::<Dict<'a>>(id))
        }
        MaybeRef::NotRef(Object::Dict(dict)) => (None, Some(dict)),
        MaybeRef::NotRef(Object::Stream(stream)) => (None, Some(stream.dict().clone())),
        MaybeRef::NotRef(_) => (None, None),
    }
}

/// The /Kids array of `dict`, read through a reference if need be.
fn kids<'a>(xref: &'a XRef, dict: &Dict<'a>) -> Option<Array<'a>> {
    match dict.get_raw::<Object<'a>>(KIDS)? {
        MaybeRef::Ref(r) => xref.get::<Array<'a>>(r.into()),
        MaybeRef::NotRef(Object::Array(array)) => Some(array),
        MaybeRef::NotRef(_) => None,
    }
}

/// The /Kids arrays a walk read, each once: the index of each array, by where its
/// bytes are, and that of the array of each dictionary, by where its bytes are, so
/// that entries of the table leading to one node read its /Kids once.
#[derive(Default)]
struct Arrays<'a> {
    by_array: hash::Map<ContentKey<'a>, u32>,
    by_dict: hash::Map<ContentKey<'a>, Option<u32>>,
}

impl<'a> Arrays<'a> {
    /// The index of the /Kids array of `dict`, if it has one, `add` making the
    /// state kept for an array not met before and returning its index.
    fn of(
        &mut self,
        xref: &'a XRef,
        dict: &Dict<'a>,
        add: impl FnOnce(Array<'a>) -> u32,
    ) -> Option<u32> {
        let key = dict.content_key();
        if let Some(index) = self.by_dict.get(&key) {
            return *index;
        }
        let index = kids(xref, dict).map(|array| {
            let array_key = array.content_key();
            match self.by_array.get(&array_key) {
                Some(index) => *index,
                None => {
                    let index = add(array);
                    self.by_array.insert(array_key, index);
                    index
                }
            }
        });
        self.by_dict.insert(key, index);
        index
    }
}

/// A /Kids array the listing walk reads: its items are read once, in order, as far
/// as some walk of it got, and the pages among them kept, so that a later walk lists
/// the pages up to there without reading the items again; the nodes among them were
/// all met by then, and list nothing more.
struct Kids<'a> {
    items: crate::object::array::ArrayIter<'a>,
    /// How many items were read; none after an item that cannot be read.
    read: usize,
    done: bool,
    pages: Vec<(Option<ObjectIdentifier>, Dict<'a>)>,
}

/// A walk of one /Kids array, for the node `node`, at its item `at`, past the pages
/// `page` of [`Kids::pages`].
struct Frame {
    kids: u32,
    at: usize,
    page: usize,
    node: u32,
}

/// What a dictionary is to the listing walk, found once for its bytes.
#[derive(Copy, Clone)]
enum Class {
    Page,
    Node,
}

/// The root of the page tree, the catalog's /Pages where it is a dictionary, and
/// its object, none where it is written into the catalog.
pub(crate) fn root<'a>(xref: &'a XRef) -> Option<(Option<ObjectIdentifier>, Dict<'a>)> {
    let catalog = xref.get::<Dict<'a>>(xref.root_id())?;
    match catalog.get_raw::<Object<'a>>(PAGES)? {
        MaybeRef::Ref(r) => {
            let id = ObjectIdentifier::from(r);
            Some((Some(id), xref.get::<Dict<'a>>(id)?))
        }
        MaybeRef::NotRef(Object::Dict(dict)) => Some((None, dict)),
        MaybeRef::NotRef(_) => None,
    }
}

/// The pages the tree with root `root`, the object `id` or written into the
/// catalog, lists, with what the walk read.
pub(crate) fn walk<'a>(
    xref: &'a XRef,
    id: Option<ObjectIdentifier>,
    root: Dict<'a>,
) -> (Tree<'a>, Vec<Listed<'a>>) {
    let root_key = Key::of(id, &root);
    let mut tree = Tree {
        root: Some(root_key),
        ..Tree::default()
    };
    if !is_node(xref, &root) {
        let page = Listed {
            id,
            dict: root,
            under: None,
        };
        return (tree, alloc::vec![page]);
    }
    let mut pages = Vec::new();
    let mut classes = hash::Map::<ContentKey<'a>, Class>::default();
    let mut arrays = Arrays::default();
    let mut all_kids: Vec<Kids<'a>> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();

    // Reads node `dict` as the node after the last, listed by `above`, and walks
    // its /Kids next.
    let mut enter = |tree: &mut Tree<'a>,
                     all_kids: &mut Vec<Kids<'a>>,
                     stack: &mut Vec<Frame>,
                     key: Key<'a>,
                     dict: Dict<'a>,
                     above: Option<u32>| {
        let node = tree.nodes.len() as u32;
        let kids = arrays.of(xref, &dict, |array| {
            all_kids.push(Kids {
                items: array.raw_iter(),
                read: 0,
                done: false,
                pages: Vec::new(),
            });
            all_kids.len() as u32 - 1
        });
        tree.nodes.push(Node {
            dict,
            above,
            last: node,
        });
        tree.read.insert(key, node);
        if let Some(kids) = kids {
            stack.push(Frame {
                kids,
                at: 0,
                page: 0,
                node,
            });
        }
    };

    enter(&mut tree, &mut all_kids, &mut stack, root_key, root, None);
    while let Some(frame) = stack.last_mut() {
        let kids = &mut all_kids[frame.kids as usize];
        if frame.at < kids.read {
            for (id, dict) in &kids.pages[frame.page..] {
                pages.push(Listed {
                    id: *id,
                    dict: dict.clone(),
                    under: Some(frame.node),
                });
            }
            frame.page = kids.pages.len();
            frame.at = kids.read;
            continue;
        }
        let item = if kids.done { None } else { kids.items.next() };
        let Some(item) = item else {
            kids.done = true;
            let node = frame.node;
            stack.pop();
            tree.nodes[node as usize].last = tree.nodes.len() as u32 - 1;
            continue;
        };
        kids.read += 1;
        frame.at += 1;
        let under = frame.node;
        let (id, dict) = entry(xref, item);
        let class = dict.as_ref().map(|dict| {
            *classes.entry(dict.content_key()).or_insert_with(|| {
                if is_node(xref, dict) {
                    Class::Node
                } else {
                    Class::Page
                }
            })
        });
        match (dict, class) {
            (Some(dict), Some(Class::Node)) => {
                let key = Key::of(id, &dict);
                // A node is met again where the walk read it before.
                if !tree.read.contains_key(&key) {
                    enter(&mut tree, &mut all_kids, &mut stack, key, dict, Some(under));
                }
            }
            (dict, _) => {
                let dict = dict.unwrap_or_default();
                kids.pages.push((id, dict.clone()));
                frame.page = kids.pages.len();
                pages.push(Listed {
                    id,
                    dict,
                    under: Some(under),
                });
            }
        }
    }
    (tree, pages)
}

/// What the document is where its tree lists no page: every dictionary of the file,
/// streams excepted, that has /Contents and no /Type, a /Type that is no name or
/// /Type /Page, in the order of the objects in the file, each found without the tree.
pub(crate) fn scan(xref: &XRef) -> Vec<Listed<'_>> {
    let ctx = ReaderContext::new(xref, false);
    xref.ids_in_file_order()
        .filter_map(|id| match xref.get_shallow_with(id, &ctx)? {
            Shallow::Value(Object::Dict(dict)) if is_page_object(xref, &dict) => Some(Listed {
                id: Some(id),
                dict,
                under: None,
            }),
            _ => None,
        })
        .collect()
}

fn is_page_object<'a>(xref: &'a XRef, dict: &Dict<'a>) -> bool {
    dict.contains_key(CONTENTS)
        && match value::<Object<'_>>(xref, dict, TYPE) {
            Some(Object::Name(name)) => name.as_ref() == PAGE,
            _ => true,
        }
}

/// A /Kids array the repair reads, last item first: its items, and the least index
/// a walk of it reached, from which on every item was met.
struct Reversed<'a> {
    items: Vec<MaybeRef<Object<'a>>>,
    low: usize,
}

/// A walk of the repair over one /Kids array, for the entry `node`, below its item
/// `next`.
struct Back<'a> {
    kids: u32,
    next: usize,
    node: (Key<'a>, Dict<'a>),
}

/// The /Parent the repair gives each entry the tree reaches below `root`: the entry
/// that lists it first in a depth-first walk that takes each entry's kids last to
/// first, reads the /Kids of every dictionary, and enters an entry the first time it
/// meets it only. Where no /Kids cycle lists a node below itself, that is the entry
/// that lists it last in a walk that takes kids first to last and enters an entry at
/// each of its listings: of two listings of an entry, neither below the other, the
/// one that comes later in that walk comes earlier in this one, and the entries this
/// walk does not enter again hold below them only listings it met the first time.
fn repair<'a>(xref: &'a XRef, root: Option<(Option<ObjectIdentifier>, Dict<'a>)>) -> Parents<'a> {
    let mut parents = Parents::default();
    let Some((id, root)) = root else {
        return parents;
    };
    let root_key = Key::of(id, &root);
    let mut seen = hash::Set::default();
    seen.insert(root_key);
    let mut arrays = Arrays::default();
    let mut all: Vec<Reversed<'a>> = Vec::new();
    let mut stack: Vec<Back<'a>> = Vec::new();

    let mut enter =
        |all: &mut Vec<Reversed<'a>>, stack: &mut Vec<Back<'a>>, key: Key<'a>, dict: Dict<'a>| {
            let kids = arrays.of(xref, &dict, |array| {
                let items: Vec<_> = array.raw_iter().collect();
                all.push(Reversed {
                    low: items.len(),
                    items,
                });
                all.len() as u32 - 1
            });
            if let Some(kids) = kids {
                stack.push(Back {
                    kids,
                    next: usize::MAX,
                    node: (key, dict),
                });
            }
        };

    enter(&mut all, &mut stack, root_key, root);
    while let Some(frame) = stack.last_mut() {
        let kids = &mut all[frame.kids as usize];
        let end = frame.next.min(kids.low);
        if end == 0 {
            stack.pop();
            continue;
        }
        let at = end - 1;
        kids.low = at;
        frame.next = at;
        let (id, Some(dict)) = entry(xref, kids.items[at].clone()) else {
            continue;
        };
        let key = Key::of(id, &dict);
        if seen.insert(key) {
            parents.insert(key, frame.node.clone());
            enter(&mut all, &mut stack, key, dict);
        }
    }
    parents
}
