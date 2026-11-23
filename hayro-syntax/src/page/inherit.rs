//! The entries a page inherits from the page tree: `/MediaBox`, `/CropBox`, /Rotate and
//! /Resources ([`Inheritable`]), each found on its own by these rules.
//!
//! 1. The value is the page's own entry, or else that of the nearest entry up its
//!    /Parent chain that has it.
//! 2. A direct `null` value is no entry: the lookup goes on past it. Any other value
//!    is the entry and ends the lookup, whatever it is: a value of the wrong kind, or
//!    a reference, which is resolved and reads as `null` where it leads to nothing
//!    or to an object the file lists but that cannot be read.
//! 3. The chain goes from an entry to its /Parent where that is a dictionary (a
//!    stream counting as its dictionary), resolved through a reference. A /Parent
//!    that is missing, no dictionary, a reference to nothing, or to an object that
//!    cannot be read ends the chain at the entry holding it.
//! 4. Where the chain ends without the value, the nodes that the walk of the /Kids
//!    listed the page under give it, nearest first ([`super::Pages`]): where the
//!    chain ended at the page itself, the nodes above this listing of the page;
//!    where it ended at a node the walk read and this listing of the page is below
//!    it, the nodes above that node; otherwise, where the node is outside the tree
//!    or the page is not below it, nothing. A page listed under two nodes thus
//!    finds, under each, what the nodes above that listing give, and a page that
//!    lost its /Parent keeps the boxes and resources the tree gives it. `PyMuPDF`
//!    1.28.2 finds none of these values, deliberately so. A page
//!    found without the tree has no listing, and finds nothing this way.
//! 5. Where following a chain meets an entry it passed, the /Parent entries are
//!    repaired, once for each [`Inheritance`], and the lookup is made again; it and
//!    every later lookup of that [`Inheritance`] follow the repaired entries, the
//!    lookups before it those as written. `PyMuPDF` 1.28.2 shows the same order:
//!    a page's value read before another page's loop is met can differ from the one
//!    read after.
//! 6. The repair gives every entry the tree reaches below its root, nodes and pages
//!    alike, written into a /Kids array or not, the /Parent of the node that lists it
//!    last in a depth-first walk of the /Kids, each node's kids first to last, that
//!    enters a node at each of its listings: where a node is listed in a /Kids cycle
//!    below itself, the walk that decides is one that takes each node's kids last to
//!    first and enters a node the first time it meets it only, in which the first
//!    listing met wins. The walk reads the /Kids of every dictionary it reaches,
//!    pages included, passes over kids that are `null` or no dictionary, and reaches
//!    any depth. The root's /Parent, and that of an entry the walk does not reach,
//!    stay as written. `PyMuPDF` 1.28.2, deliberately not followed here, fails to
//!    load a page whose lookup repairs a tree with a kid that is no dictionary, and
//!    leaves entries more than about 256 levels down unrepaired (observed: repaired
//!    at 250 levels, not at 260).
//! 7. A chain that still meets an entry it passed fails the lookup
//!    ([`InheritanceError::ParentLoop`]).
//! 8. A lookup that reads an object the file lists but that cannot be read — the
//!    value, an entry of a node the value is inherited through, or a /Parent on the
//!    way — tells the computation being watched ([`crate::lost`]), each time the
//!    lookup is made, though the object is read once.
//! 9. The lookups of every entry of every page take time linear in the nodes the
//!    chains pass and the pages, and a repair time linear in the entries of the tree
//!    and their listings, however the tree shares nodes and /Kids arrays.

use super::tree::{Key, Parents, Tree};
use super::{Page, Pages};
use crate::hash;
use crate::lost::Lost;
use crate::object::dict::keys::{CROP_BOX, MEDIA_BOX, PARENT, RESOURCES, ROTATE};
use crate::object::{Dict, MaybeRef, Null, Object};
use crate::pdf::Pdf;
use crate::xref::{ContentKey, XRef};
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
}

/// Lookups of the entries pages inherit, by the rules of this module's doc: one
/// repairs the /Parent entries for itself and every later lookup of this
/// [`Inheritance`]. A page's [`Page::resources`], boxes and rotation are found by
/// lookups made once every page of the document has been looked up.
pub struct Inheritance<'a>(Lookups<'a, 'a>);

impl<'a> Inheritance<'a> {
    /// No /Parent repaired yet.
    pub fn new(pdf: &'a Pdf) -> Self {
        let pages: &'a Pages<'a> = pdf.pages();
        Self(Lookups::new(pdf.xref(), pages.root, None))
    }

    /// The value of `key` for `page`: `None` if neither it nor a /Parent has the key,
    /// `null` for a reference to nothing or to an object the file lists but that
    /// cannot be read, which is lost ([`crate::lost`]).
    pub fn get(
        &mut self,
        page: &Page<'a>,
        key: Inheritable,
    ) -> Result<Option<Object<'a>>, InheritanceError> {
        let start = Start {
            key: Key::of(page.id, &page.inner),
            dict: &page.inner,
            under: page.under,
        };
        self.0.get(&start, key)
    }
}

/// The page a lookup starts from: its key, its dictionary, and the node that lists
/// it, if the walk listed it.
pub(super) struct Start<'a, 'p> {
    pub(super) key: Key<'a>,
    pub(super) dict: &'p Dict<'a>,
    pub(super) under: Option<u32>,
}

/// Where following a chain of /Parent entries from an entry led.
#[derive(Clone)]
enum Reach<'a> {
    /// To the value.
    Found(Object<'a>),
    /// To this entry, whose /Parent ends the chain, without the value.
    Ended(Key<'a>),
    /// Back to an entry it passed.
    Loop,
}

/// Where a chain led, and the first object reading it lost.
#[derive(Clone)]
struct Outcome<'a> {
    reach: Reach<'a>,
    lost: Lost,
}

/// A value read once, and the first object reading it lost.
type Read<T> = (T, Lost);

/// The lookups of [`Inheritance`], which [`Pages`] makes before it holds them.
/// Every lookup is kept, so that each entry an entry of the tree holds is read once,
/// and each chain is followed once from each entry, in each state of the /Parent
/// entries (rule 9).
pub(super) struct Lookups<'t, 'a> {
    xref: &'a XRef,
    /// The root of the tree, above which no node is.
    root: Option<Key<'a>>,
    /// The nodes the walk read, which [`Pages`] lends while it walks the tree, and
    /// which are otherwise read again the first time a lookup needs them: only a
    /// chain that ends below the root, at a page without /Parent or at a /Parent
    /// that is no dictionary, does, and the pages do not hold them meanwhile.
    walked: Option<&'t Tree<'a>>,
    read: Option<Tree<'a>>,
    /// The /Parent entries of the repair, once a lookup made it, and the first
    /// object the repair lost.
    repaired: Option<(Parents<'a>, Lost)>,
    /// The value of each key each dictionary holds itself, by where its bytes are.
    own: hash::Map<(ContentKey<'a>, u8), Read<Option<Object<'a>>>>,
    /// The /Parent each dictionary holds, as written, by where its bytes are.
    parents: hash::Map<ContentKey<'a>, Read<Option<(Key<'a>, Dict<'a>)>>>,
    /// Where the chain from each entry leads for each key, in the current state.
    chains: hash::Map<(Key<'a>, u8), Outcome<'a>>,
    /// The value of each key for the nodes above each node the walk read, and that
    /// node, nearest first.
    listed: hash::Map<(u32, u8), Read<Option<Object<'a>>>>,
    /// Room for the entries a chain passes, kept from one lookup to the next.
    passed: hash::Set<Key<'a>>,
    path: Vec<(Key<'a>, Lost)>,
    /// Room for the nodes a lookup through the listing passes.
    above: Vec<(u32, Lost)>,
}

impl<'t, 'a> Lookups<'t, 'a> {
    pub(super) fn new(xref: &'a XRef, root: Option<Key<'a>>, walked: Option<&'t Tree<'a>>) -> Self {
        Self {
            xref,
            root,
            walked,
            read: None,
            repaired: None,
            own: hash::Map::default(),
            parents: hash::Map::default(),
            chains: hash::Map::default(),
            listed: hash::Map::default(),
            passed: hash::Set::default(),
            path: Vec::new(),
            above: Vec::new(),
        }
    }

    /// Whether a lookup repaired the /Parent entries.
    pub(super) fn repaired(&self) -> bool {
        self.repaired.is_some()
    }

    /// The value of `key` for the page `start`: rules 1 to 8.
    pub(super) fn get(
        &mut self,
        start: &Start<'a, '_>,
        key: Inheritable,
    ) -> Result<Option<Object<'a>>, InheritanceError> {
        let mut outcome = self.chain(start, key);
        if matches!(outcome.reach, Reach::Loop) && self.repaired.is_none() {
            self.xref.note(outcome.lost);
            self.repaired = Some(Tree::repair(self.xref));
            self.chains.clear();
            outcome = self.chain(start, key);
        }
        // An object the repair lost may have listed any entry, and so have given it
        // another /Parent: every lookup that follows the repaired entries is told,
        // as of any value computed once and kept ([`crate::lost`]).
        if let Some((_, lost)) = &self.repaired {
            self.xref.note(*lost);
        }
        self.xref.note(outcome.lost);
        match outcome.reach {
            Reach::Loop => Err(InheritanceError::ParentLoop),
            Reach::Found(value) => Ok(Some(value)),
            Reach::Ended(end) => Ok(self.listing(start, end, key)),
        }
    }

    /// The value of `key` that `dict` holds itself: none for a direct `null`, and
    /// `null` for a reference that leads to nothing or is lost.
    fn own(&mut self, dict: &Dict<'a>, key: Inheritable) -> Read<Option<Object<'a>>> {
        let xref = self.xref;
        self.own
            .entry((dict.content_key(), key as u8))
            .or_insert_with(|| match dict.get_raw::<Object<'a>>(key.name()) {
                None | Some(MaybeRef::NotRef(Object::Null(_))) => (None, None),
                Some(MaybeRef::NotRef(value)) => (Some(value), None),
                // Read as an object of its own, so that a reference to the object
                // holding `dict` is that object ([`super::tree::value`]).
                Some(MaybeRef::Ref(r)) => {
                    let (value, lost) = xref.apart(|| xref.get::<Object<'a>>(r.into()));
                    (Some(value.unwrap_or(Object::Null(Null))), lost)
                }
            })
            .clone()
    }

    /// The /Parent of the entry `key`, `dict`, in the current state: as the repair
    /// gives it, or as written, where it is a dictionary.
    fn parent(&mut self, key: &Key<'a>, dict: &Dict<'a>) -> Read<Option<(Key<'a>, Dict<'a>)>> {
        if let Some(parent) = self
            .repaired
            .as_ref()
            .and_then(|(parents, _)| parents.get(key))
        {
            return (Some(parent.clone()), None);
        }
        let xref = self.xref;
        self.parents
            .entry(dict.content_key())
            .or_insert_with(|| match dict.get_raw::<Object<'a>>(PARENT) {
                Some(MaybeRef::Ref(r)) => {
                    let id = r.into();
                    let (parent, lost) = xref.apart(|| xref.get::<Dict<'a>>(id));
                    (parent.map(|parent| (Key::Object(id), parent)), lost)
                }
                Some(MaybeRef::NotRef(Object::Dict(parent))) => {
                    (Some((Key::Written(parent.content_key()), parent)), None)
                }
                _ => (None, None),
            })
            .clone()
    }

    /// Where the /Parent chain from `start` leads for `key` (rules 1 to 3, 5 and 7),
    /// kept for every entry it passes.
    fn chain(&mut self, start: &Start<'a, '_>, key: Inheritable) -> Outcome<'a> {
        let k = key as u8;
        // The entries passed, each with the first object reading it lost.
        let mut path = core::mem::take(&mut self.path);
        let mut passed = core::mem::take(&mut self.passed);
        let mut at = (start.key, start.dict.clone());
        let tail = loop {
            let (entry, dict) = &at;
            if let Some(outcome) = self.chains.get(&(*entry, k)) {
                break outcome.clone();
            }
            if !passed.insert(*entry) {
                break Outcome {
                    reach: Reach::Loop,
                    lost: None,
                };
            }
            let (value, own_lost) = self.own(dict, key);
            if let Some(value) = value {
                path.push((*entry, own_lost));
                break Outcome {
                    reach: Reach::Found(value),
                    lost: None,
                };
            }
            let (parent, parent_lost) = self.parent(entry, dict);
            path.push((*entry, own_lost.or(parent_lost)));
            match parent {
                Some(parent) => at = parent,
                None => {
                    break Outcome {
                        reach: Reach::Ended(*entry),
                        lost: None,
                    };
                }
            }
        };
        let mut outcome = tail;
        for (entry, lost) in path.drain(..).rev() {
            outcome.lost = lost.or(outcome.lost);
            self.chains.insert((entry, k), outcome.clone());
        }
        passed.clear();
        (self.path, self.passed) = (path, passed);
        outcome
    }

    /// The nodes the walk read.
    fn tree(&mut self) -> &Tree<'a> {
        if let Some(tree) = self.walked {
            return tree;
        }
        let xref = self.xref;
        self.read.get_or_insert_with(|| Tree::read(xref))
    }

    /// The value of `key` the nodes the walk listed the page `start` under give it,
    /// where its /Parent chain ended at `end` without one (rule 4).
    fn listing(
        &mut self,
        start: &Start<'a, '_>,
        end: Key<'a>,
        key: Inheritable,
    ) -> Option<Object<'a>> {
        let under = start.under?;
        let from = if end == start.key {
            Some(under)
        } else if Some(end) == self.root {
            // Every listing is below the root, and no node above it.
            None
        } else {
            let tree = self.tree();
            let node = tree.read_as(&end)?;
            if !tree.below(under, node) {
                return None;
            }
            tree.node(node).above
        }?;
        let (value, lost) = self.through_listing(from, key);
        self.xref.note(lost);
        value
    }

    /// The value of `key` of node `from` or, without one, of the nearest node above
    /// it that has it, kept for every node passed.
    fn through_listing(&mut self, from: u32, key: Inheritable) -> Read<Option<Object<'a>>> {
        let k = key as u8;
        let mut path = core::mem::take(&mut self.above);
        let mut at = Some(from);
        let tail = loop {
            let Some(node) = at else {
                break (None, None);
            };
            if let Some(read) = self.listed.get(&(node, k)) {
                break read.clone();
            }
            let (dict, above) = {
                let node = self.tree().node(node);
                (node.dict.clone(), node.above)
            };
            let (value, lost) = self.own(&dict, key);
            path.push((node, lost));
            if value.is_some() {
                break (value, None);
            }
            at = above;
        };
        let mut read = tail;
        for (node, lost) in path.drain(..).rev() {
            read.1 = lost.or(read.1);
            self.listed.insert((node, k), read.clone());
        }
        self.above = path;
        read
    }
}
