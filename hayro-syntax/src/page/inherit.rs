//! The entries a page inherits from the page tree, found as `MuPDF` finds them.

use super::{KidsStates, Node, Objects, Page};
use crate::object::dict::keys::{CROP_BOX, MEDIA_BOX, PARENT, RESOURCES, ROTATE};
use crate::object::{MaybeRef, Null, Object, ObjectIdentifier};
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
}

/// An entry a page inherits from the page tree (the PDF specification's table 31).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Inheritable {
    /// `/MediaBox`.
    MediaBox,
    /// `/CropBox`.
    CropBox,
    /// `/Rotate`.
    Rotate,
    /// `/Resources`.
    Resources,
}

impl Inheritable {
    /// Every inheritable entry, each at its own index.
    pub(super) const ALL: [Self; 4] =
        [Self::MediaBox, Self::CropBox, Self::Rotate, Self::Resources];

    fn name(self) -> &'static [u8] {
        match self {
            Self::MediaBox => MEDIA_BOX,
            Self::CropBox => CROP_BOX,
            Self::Rotate => ROTATE,
            Self::Resources => RESOURCES,
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Values a page inherits through /Parent, as `pdf_dict_get_inheritable` (`MuPDF`'s
/// `pdf-object.c`) finds them: on the page or the nearest /Parent that has the key,
/// past a direct `null`, which `MuPDF` stores as no entry, but stopping at a reference
/// even if it leads nowhere.
///
/// Where the chain ends at an entry without a /Parent, or with a /Parent that is
/// no dictionary, the lookup goes on through the nodes the page tree lists that
/// entry under, if it is the page or one of them, where `MuPDF` stops: a page that
/// lost its /Parent keeps the resources and boxes the tree gives it.
///
/// A /Parent chain that loops makes `MuPDF` set each page tree node's /Parent to the
/// node that lists it (`pdf_repair_page_tree_parents` in `pdf-repair.c`), once, for
/// that lookup and every later one; this keeps that state, so lookups made in
/// `MuPDF`'s order find `MuPDF`'s values. A page's [`Page::resources`], boxes and
/// rotation are found by lookups made once every page of the document has been
/// looked up, as when `MuPDF` draws a page after loading them all.
///
/// Where each node's /Parent chain leads is kept for each entry until the /Parent
/// entries are repaired, so the lookups for all pages take time linear in the
/// nodes the chains pass and the pages.
pub struct Inheritance<'a> {
    objects: Objects<'a>,
    repaired: Option<FxHashMap<Key, Node<'a>>>,
    chains: FxHashMap<Key, [Option<Chain<'a>>; 4]>,
}

/// Where a node's /Parent chain leads for one entry.
#[derive(Clone)]
enum Chain<'a> {
    /// To a node with the entry: its value, `null` for a reference to nothing.
    Found(Object<'a>),
    /// To this node, which has no /Parent, or one that is no dictionary.
    End(Key),
    /// Round in a loop.
    Loop,
    /// Being followed: a chain that meets the node again loops.
    Followed,
}

/// What tells nodes apart: the object number and generation, which the loader
/// keeps apart where a damaged file holds two generations of a number, or for a
/// direct dictionary its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Key {
    Object(ObjectIdentifier),
    Direct(usize),
}

impl<'a> Node<'a> {
    pub(super) fn key(&self) -> Key {
        match self.id() {
            Some(id) => Key::Object(id),
            None => Key::Direct(self.dict().data().as_ptr() as usize),
        }
    }
}

impl<'a> Inheritance<'a> {
    /// No /Parent repaired yet.
    pub fn new(pdf: &'a Pdf) -> Self {
        Self::with(Objects::new(ReaderContext::new(pdf.xref(), false)))
    }

    /// Resolving objects through `objects`.
    pub(super) fn with(objects: Objects<'a>) -> Self {
        Self {
            objects,
            repaired: None,
            chains: FxHashMap::default(),
        }
    }

    /// The value of `key` for `page`: `None` if neither it nor a /Parent has the key,
    /// `null` for a reference to nothing.
    pub fn get(
        &mut self,
        page: &Page<'a>,
        key: Inheritable,
    ) -> Result<Option<Object<'a>>, InheritanceError> {
        let node = Node::page(page.obj_id(), page.raw());
        self.lookup(&node, &page.listings, page.lister, key)
    }

    /// The value of `key` for `page`, which `listings` shows listed under the node at
    /// `lister`.
    pub(super) fn lookup(
        &mut self,
        page: &Node<'a>,
        listings: &Listings<'a>,
        lister: Option<usize>,
        key: Inheritable,
    ) -> Result<Option<Object<'a>>, InheritanceError> {
        let chain = match self.chain(page, key) {
            Chain::Loop if self.repaired.is_none() => {
                self.repaired = Some(tree_parents(&mut self.objects));
                self.chains.clear();
                self.chain(page, key)
            }
            chain => chain,
        };
        match chain {
            Chain::Found(value) => Ok(Some(value)),
            Chain::End(end) => Ok(listings.held(end, page.key(), lister, key).cloned()),
            Chain::Loop | Chain::Followed => Err(InheritanceError::ParentLoop),
        }
    }

    /// The objects the lookups have resolved.
    pub(super) fn into_objects(self) -> Objects<'a> {
        self.objects
    }

    /// Whether the page tree's /Parent entries have been repaired.
    pub(super) fn is_repaired(&self) -> bool {
        self.repaired.is_some()
    }

    /// Where the /Parent chain from `start` leads for `key`, kept for each node it
    /// passes, so that no chain is followed past a node twice.
    fn chain(&mut self, start: &Node<'a>, key: Inheritable) -> Chain<'a> {
        let slot = key.index();
        let mut passed = vec![];
        let mut node = start.clone();
        let chain = loop {
            let at = node.key();
            match self
                .chains
                .get(&at)
                .and_then(|chains| chains[slot].as_ref())
            {
                Some(Chain::Followed) => break Chain::Loop,
                Some(chain) => break chain.clone(),
                None => {}
            }
            passed.push(at);
            if let Some(value) = value(&node, key, &mut self.objects) {
                break Chain::Found(value);
            }
            let Some(parent) = self.parent(&node) else {
                break Chain::End(at);
            };
            self.chains.entry(at).or_default()[slot] = Some(Chain::Followed);
            node = parent;
        };
        for at in passed {
            self.chains.entry(at).or_default()[slot] = Some(chain.clone());
        }
        chain
    }

    /// The node's /Parent as the lookups see it: repaired, or one that is a
    /// dictionary.
    fn parent(&mut self, node: &Node<'a>) -> Option<Node<'a>> {
        match self.repaired.as_ref().and_then(|map| map.get(&node.key())) {
            Some(parent) => Some(parent.clone()),
            None => node
                .dict()
                .get_raw::<Object<'a>>(PARENT)
                .map(|parent| self.objects.node(parent))
                .filter(Node::is_dict),
        }
    }
}

/// The page tree nodes whose kids the walk of the /Kids reads, in the order it reads
/// them, so that the nodes a node lists, directly or not, follow it.
#[derive(Default)]
pub(super) struct Listings<'a> {
    nodes: Vec<Listing<'a>>,
    at: FxHashMap<Key, usize>,
}

struct Listing<'a> {
    /// The index past the last node this one lists, directly or not.
    end: usize,
    /// For each of [`Inheritable::ALL`], the value of the nearest of this node and
    /// the nodes listing it that has the entry, read once for all the pages that
    /// inherit it.
    held: [Option<Object<'a>>; 4],
}

impl<'a> Listings<'a> {
    /// Reads `node`, listed under the node at `lister`, resolving its entries through
    /// `objects`, and returns its index; `None` if it was read before, where it lists
    /// nothing again.
    pub(super) fn read(
        &mut self,
        node: &Node<'a>,
        lister: Option<usize>,
        objects: &mut Objects<'a>,
    ) -> Option<usize> {
        let at = self.nodes.len();
        if self.at.contains_key(&node.key()) {
            return None;
        }
        self.at.insert(node.key(), at);
        let held = Inheritable::ALL.map(|key| match value(node, key, objects) {
            Some(value) => Some(value),
            None => lister.and_then(|lister| self.nodes[lister].held[key.index()].clone()),
        });
        self.nodes.push(Listing { end: at + 1, held });
        Some(at)
    }

    /// Marks the node at `at` as listing every node read since.
    pub(super) fn close(&mut self, at: usize) {
        self.nodes[at].end = self.nodes.len();
    }

    /// The value `page`, listed under the node at `lister`, inherits for `key`
    /// through the nodes listing it above `end`, where its /Parent chain ends: all
    /// of them if `end` is the page itself, none if `end` does not list the page.
    fn held(
        &self,
        end: Key,
        page: Key,
        lister: Option<usize>,
        key: Inheritable,
    ) -> Option<&Object<'a>> {
        let lister = lister?;
        let from = if end == page {
            lister
        } else {
            let at = *self.at.get(&end)?;
            if !(at..self.nodes[at].end).contains(&lister) {
                return None;
            }
            at
        };
        self.nodes[from].held[key.index()].as_ref()
    }
}

// The value `node` has for `key`, `null` for a reference to nothing.
fn value<'a>(node: &Node<'a>, key: Inheritable, objects: &mut Objects<'a>) -> Option<Object<'a>> {
    node.has(key.name()).then(|| {
        objects
            .get(node.dict(), key.name())
            .unwrap_or(Object::Null(Null))
    })
}

// Each page tree entry's /Parent set to the node whose /Kids lists it, depth first,
// the last one listing it winning, as `MuPDF` sets them.
//
// `MuPDF` walks a node's kids again each time the node is listed, which takes time
// exponential in nested sharing. The last listing that walk meets is the first one
// met by a walk that takes each node's kids last first, so that walk, reading a
// node's kids only the first time it meets the node, sets the same parents in time
// linear in the nodes and listings. Where this differs, it sets more parents than
// `MuPDF`, which fails on a kid that is no dictionary and passes over a node with
// 257 nodes above it, and which may set other parents where a /Kids cycle is
// entered through a shared node. A kid that is no dictionary is passed over here.
//
// The nodes listing one /Kids array object share the kids left in it: those that
// one of them took were met then, so every other would pass over them. Each array
// is thus read and stored once, however many nodes list it.
fn tree_parents<'a>(objects: &mut Objects<'a>) -> FxHashMap<Key, Node<'a>> {
    let mut parents = FxHashMap::default();
    let Some(root) = objects.tree() else {
        return parents;
    };
    // The kids of each /Kids array not taken yet, first first, taken from the end;
    // `None` for a direct `null`, which `MuPDF` skips.
    let mut kids = KidsStates::<Vec<Option<Node<'a>>>>::default();
    let mut read = |kids: &mut KidsStates<_>, node: &Node<'a>| {
        kids.of(node, |kids| {
            let kids: Vec<_> = kids
                .map(|kid| {
                    #[cfg(test)]
                    super::kids_read();
                    match kid {
                        MaybeRef::NotRef(Object::Null(_)) => None,
                        kid => Some(objects.node(kid)),
                    }
                })
                .collect();
            #[cfg(test)]
            super::kids_stored(kids.len());
            kids
        })
    };
    let mut met = FxHashSet::default();
    met.insert(root.key());
    let mut stack = vec![(read(&mut kids, &root), root)];
    while let Some((array, node)) = stack.last() {
        let Some(kid) = kids.states[*array].pop() else {
            stack.pop();
            continue;
        };
        // A node met before is in the map already, or on the stack, above itself.
        let Some(kid) = kid.filter(|kid| kid.is_dict() && met.insert(kid.key())) else {
            continue;
        };
        parents.insert(kid.key(), node.clone());
        stack.push((read(&mut kids, &kid), kid));
    }
    parents
}
