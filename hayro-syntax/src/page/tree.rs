//! Page listing and parent repair.

use crate::hash;
use crate::lost::Lost;
use crate::object::dict::keys::{CONTENTS, KIDS, MEDIA_BOX, PAGE, PAGES, TYPE};
use crate::object::{Array, Dict, MaybeRef, Name, Object, ObjectIdentifier, ObjectLike};

use crate::xref::{ContentKey, XRef};
use alloc::vec::Vec;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Key<'a> {
    Object(ObjectIdentifier),
    Written(ContentKey<'a>),
}

impl<'a> Key<'a> {
    pub(crate) fn of(id: Option<ObjectIdentifier>, dict: &Dict<'a>) -> Self {
        match id {
            Some(id) => Self::Object(id),
            None => Self::Written(dict.content_key()),
        }
    }
}

pub(crate) struct Listed<'a> {
    pub(crate) id: Option<ObjectIdentifier>,
    pub(crate) dict: Dict<'a>,
    pub(crate) under: Option<u32>,
}

pub(crate) struct Node<'a> {
    pub(crate) dict: Dict<'a>,
    pub(crate) above: Option<u32>,
    pub(crate) last: u32,
}

pub(crate) type Parents<'a> = hash::Map<Key<'a>, (Key<'a>, Dict<'a>)>;

#[derive(Default)]
pub(crate) struct Tree<'a> {
    pub(crate) root: Option<Key<'a>>,
    nodes: Vec<Node<'a>>,
    read: hash::Map<Key<'a>, u32>,
}

impl<'a> Tree<'a> {
    pub(crate) fn read(xref: &'a XRef) -> Self {
        xref.apart(|| root(xref).map(|(id, root)| walk(xref, id, root).0))
            .0
            .unwrap_or_default()
    }

    pub(crate) fn node(&self, index: u32) -> &Node<'a> {
        &self.nodes[index as usize]
    }

    pub(crate) fn read_as(&self, key: &Key<'a>) -> Option<u32> {
        self.read.get(key).copied()
    }

    pub(crate) fn below(&self, under: u32, node: u32) -> bool {
        node <= under && under <= self.node(node).last
    }

    pub(crate) fn repair(xref: &'a XRef) -> (Parents<'a>, Lost) {
        xref.apart(|| repair(xref, root(xref)))
    }
}

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

fn present(dict: &Dict<'_>, key: &[u8]) -> bool {
    !matches!(
        dict.get_raw::<Object<'_>>(key),
        None | Some(MaybeRef::NotRef(Object::Null(_)))
    )
}

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

fn kids<'a>(xref: &'a XRef, dict: &Dict<'a>) -> Option<Array<'a>> {
    match dict.get_raw::<Object<'a>>(KIDS)? {
        MaybeRef::Ref(r) => xref.get::<Array<'a>>(r.into()),
        MaybeRef::NotRef(Object::Array(array)) => Some(array),
        MaybeRef::NotRef(_) => None,
    }
}

#[derive(Default)]
struct Arrays<'a> {
    by_array: hash::Map<ContentKey<'a>, u32>,
    by_dict: hash::Map<ContentKey<'a>, Option<u32>>,
}

impl<'a> Arrays<'a> {
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

struct Kids<'a> {
    items: crate::object::array::ArrayIter<'a>,
    read: usize,
    done: bool,
    pages: Vec<(Option<ObjectIdentifier>, Dict<'a>)>,
}

struct Frame {
    kids: u32,
    at: usize,
    page: usize,
    node: u32,
}

#[derive(Copy, Clone)]
enum Class {
    Page,
    Node,
}

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
    if !root.contains_key(KIDS) && !is_node(xref, &root) {
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
        let ((id, dict), lost) = xref.watch(|| entry(xref, item));
        if dict.is_none() && xref.is_repaired() && lost.is_none() {
            continue;
        }
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

struct Candidate<'a>(Option<Dict<'a>>);
impl<'a> crate::reader::Readable<'a> for Candidate<'a> {
    const HANDLES_TYPE_MISMATCH: bool = true;
    fn read(
        r: &mut crate::reader::Reader<'a>,
        ctx: &crate::reader::ReaderContext<'a>,
    ) -> Option<Self> {
        use crate::reader::ReaderExt;
        if r.peek_bytes(2) != Some(b"<<") {
            Object::skip_indirect_value(r)?;
            return Some(Self(None));
        }
        if !crate::object::dict::has_key(&mut r.clone(), ctx, CONTENTS)? {
            return Some(Self(None));
        }
        let dict = r.read::<Dict<'_>>(ctx)?;
        let mut probe = r.clone();
        if crate::object::stream::forward_stream_opener(&mut probe, &crate::reader::Unknown)
            .is_some()
        {
            return Some(Self(None));
        }
        Some(Self(Some(dict)))
    }
}
pub(crate) fn scan(xref: &XRef) -> Vec<Listed<'_>> {
    let ctx = crate::reader::ReaderContext::new(xref, false);

    xref.ids_in_file_order()
        .filter_map(|id| match xref.get_with::<Candidate<'_>>(id, &ctx)?.0 {
            Some(dict) if is_page_object(xref, &dict) => Some(Listed {
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

struct Reversed<'a> {
    items: Vec<MaybeRef<Object<'a>>>,
    low: usize,
}

struct Back<'a> {
    kids: u32,
    next: usize,
    node: (Key<'a>, Dict<'a>),
}

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

    // In reverse listing order the first parent encountered wins the repair.
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
