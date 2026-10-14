//! The entries a page inherits from the page tree, found as `MuPDF` finds them.

use super::{Node, Page};
use crate::object::dict::keys::{KIDS, PAGES, PARENT};
use crate::object::{Array, Dict, MaybeRef, Null, Object};
use crate::pdf::Pdf;
use crate::reader::ReaderContext;
use crate::sync::{FxHashMap, FxHashSet};
use alloc::vec;
use alloc::vec::Vec;

/// Why an inherited entry cannot be found.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum InheritanceError {
    /// The page's /Parent chain loops even after the page tree's /Parent entries
    /// are repaired.
    ParentLoop,
    /// The repair meets a page tree entry that is no dictionary, which `MuPDF` fails
    /// to write /Parent into.
    Repair,
}

/// Values a page inherits through /Parent, as `pdf_dict_get_inheritable` (`MuPDF`'s
/// `pdf-object.c`) finds them: on the page or the nearest /Parent that has the key,
/// past a direct `null`, which `MuPDF` stores as no entry, but stopping at a reference
/// even if it leads nowhere.
///
/// A /Parent chain that loops makes `MuPDF` set each page tree node's /Parent to the
/// node that lists it (`pdf_repair_page_tree_parents` in `pdf-repair.c`), once, for
/// that lookup and every later one; this keeps that state, so lookups made in
/// `MuPDF`'s order find `MuPDF`'s values. A page's [`Page::resources`], boxes and
/// rotation are found by lookups made once every page of the document has been
/// looked up, as when `MuPDF` draws a page after loading them all.
pub struct Inheritance<'a> {
    ctx: ReaderContext<'a>,
    repaired: Option<Result<FxHashMap<Key, Node<'a>>, InheritanceError>>,
}

/// What tells nodes apart: the object number, or for a direct dictionary its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Key {
    Object(i32),
    Direct(usize),
}

impl<'a> Node<'a> {
    fn key(&self) -> Key {
        match self.id {
            Some(id) => Key::Object(id.obj_number),
            None => Key::Direct(self.dict.data().as_ptr() as usize),
        }
    }

    // `pdf_to_num`: the object number, 0 for a direct object.
    fn number(&self) -> i32 {
        self.id.map_or(0, |id| id.obj_number)
    }
}

impl<'a> Inheritance<'a> {
    /// No /Parent repaired yet.
    pub fn new(pdf: &'a Pdf) -> Self {
        Self::with(ReaderContext::new(pdf.xref(), false))
    }

    pub(super) fn with(ctx: ReaderContext<'a>) -> Self {
        Self {
            ctx,
            repaired: None,
        }
    }

    /// The value of `key` for `page`: `None` if neither it nor a /Parent has the key,
    /// `null` for a reference to nothing.
    pub fn get(
        &mut self,
        page: &Page<'a>,
        key: &[u8],
    ) -> Result<Option<Object<'a>>, InheritanceError> {
        self.lookup(&Node::page(page.obj_id(), page.raw()), key)
    }

    pub(super) fn lookup(
        &mut self,
        start: &Node<'a>,
        key: &[u8],
    ) -> Result<Option<Object<'a>>, InheritanceError> {
        if let Some(found) = self.search(start, key) {
            return Ok(found);
        }
        if let Err(error) = self.repaired.get_or_insert_with(|| tree_parents(&self.ctx)) {
            return Err(*error);
        }
        self.search(start, key).ok_or(InheritanceError::ParentLoop)
    }

    /// Whether the page tree's /Parent entries have been repaired.
    pub(super) fn is_repaired(&self) -> bool {
        self.repaired.is_some()
    }

    /// The value, or `None` if the chain loops before it is found.
    fn search(&self, start: &Node<'a>, key: &[u8]) -> Option<Option<Object<'a>>> {
        let repaired = self.repaired.as_ref().and_then(|r| r.as_ref().ok());
        let mut node = start.clone();
        let mut seen = FxHashSet::default();
        loop {
            if !seen.insert(node.key()) {
                return None;
            }
            if node.has(key) {
                let value = node.dict.get::<Object<'a>>(key);
                return Some(Some(value.unwrap_or(Object::Null(Null))));
            }
            node = match repaired.and_then(|map| map.get(&node.key())) {
                Some(parent) => parent.clone(),
                None => match node.dict.get_raw::<Object<'a>>(PARENT) {
                    Some(parent) => Node::new(parent, &self.ctx),
                    None => return Some(None),
                },
            };
        }
    }
}

// `walk_page_tree` in MuPDF's `pdf-repair.c`: each node's /Parent set to the node
// whose /Kids lists it, depth first, the last one listing it winning; a node already
// above itself or with 257 nodes above it is passed over (`pdf_cycle`).
//
// MuPDF walks a node's kids again each time the node is listed, which takes time
// exponential in nested sharing. The last listing that walk meets is the first one
// met by a walk that takes each node's kids last first, so that walk, reading a
// node's kids only the first time it meets the node, sets the same parents in time
// linear in the nodes and listings. It differs only where passing a node over
// depends on the path to it — a node shared within 257 levels of the cut, or a /Kids
// cycle entered through a shared node — where a later, repeated walk of MuPDF's may
// set parents this one does not.
fn tree_parents<'a>(ctx: &ReaderContext<'a>) -> Result<FxHashMap<Key, Node<'a>>, InheritanceError> {
    struct Frame<'a> {
        node: Node<'a>,
        /// The kids left, first first, taken from the end; `None` for a direct
        /// `null`, which `MuPDF` skips.
        kids: Vec<Option<Node<'a>>>,
    }

    let frame = |node: Node<'a>| {
        let kids = node
            .dict
            .get::<Array<'a>>(KIDS)
            .map(|kids| {
                kids.raw_iter()
                    .map(|kid| match kid {
                        MaybeRef::NotRef(Object::Null(_)) => None,
                        kid => Some(Node::new(kid, ctx)),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Frame { node, kids }
    };
    let mut parents = FxHashMap::default();
    let xref = ctx.xref();
    let Some(root) = xref
        .get_with::<Dict<'a>>(xref.root_id(), ctx)
        .and_then(|catalog| catalog.get_raw::<Object<'a>>(PAGES))
    else {
        return Ok(parents);
    };
    let root = Node::new(root, ctx);
    let mut met = FxHashSet::default();
    met.insert(root.key());
    let mut above = FxHashSet::default();
    above.insert(root.number());
    let mut stack = vec![frame(root)];
    while let Some(top) = stack.last_mut() {
        let Some(kid) = top.kids.pop() else {
            if let Some(done) = stack.pop() {
                above.remove(&done.node.number());
            }
            continue;
        };
        let Some(kid) = kid else { continue };
        let number = kid.number();
        if number > 0 && (stack.len() >= 257 || above.contains(&number)) {
            continue;
        }
        if !met.insert(kid.key()) {
            continue;
        }
        if !kid.is_dict {
            return Err(InheritanceError::Repair);
        }
        let parent = stack.last().map(|f| f.node.clone());
        parents.insert(kid.key(), parent.expect("a frame above"));
        above.insert(number);
        stack.push(frame(kid));
    }
    Ok(parents)
}
