//! Lookup of inheritable page entries.

use super::tree::{Key, Parents, Tree};
use super::{Page, Pages};
use crate::hash;
use crate::lost::Lost;
use crate::object::dict::keys::{CROP_BOX, MEDIA_BOX, PARENT, RESOURCES, ROTATE};
use crate::object::{Dict, MaybeRef, Null, Object};
use crate::pdf::Pdf;
use crate::xref::{ContentKey, XRef};
use alloc::vec::Vec;

/// An error following the parent chain.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum InheritanceError {
    /// The parent chain still loops after repair.
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

/// Cached inheritance lookups for a document, with one parent repair on the first loop.
pub struct Inheritance<'a>(Lookups<'a, 'a>);

impl<'a> Inheritance<'a> {
    /// No /Parent repaired yet.
    pub fn new(pdf: &'a Pdf) -> Self {
        let pages: &'a Pages<'a> = pdf.pages();
        Self(Lookups::new(pdf.xref(), pages.root, None))
    }

    /// Find the nearest entry, passing over direct nulls and resolving the first reference.
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

pub(super) struct Start<'a, 'p> {
    pub(super) key: Key<'a>,
    pub(super) dict: &'p Dict<'a>,
    pub(super) under: Option<u32>,
}

#[derive(Clone)]
enum Reach<'a> {
    Found(Object<'a>),
    Ended(Key<'a>),
    Loop,
}

#[derive(Clone)]
struct Outcome<'a> {
    reach: Reach<'a>,
    lost: Lost,
}

type Read<T> = (T, Lost);

pub(super) struct Lookups<'t, 'a> {
    xref: &'a XRef,
    root: Option<Key<'a>>,
    walked: Option<&'t Tree<'a>>,
    read: Option<Tree<'a>>,
    repaired: Option<(Parents<'a>, Lost)>,
    own: hash::Map<(ContentKey<'a>, u8), Read<Option<Object<'a>>>>,
    parents: hash::Map<ContentKey<'a>, Read<Option<(Key<'a>, Dict<'a>)>>>,
    chains: hash::Map<(Key<'a>, u8), Outcome<'a>>,
    listed: hash::Map<(u32, u8), Read<Option<Object<'a>>>>,
    path: Vec<(Key<'a>, Lost)>,
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
            path: Vec::new(),
            above: Vec::new(),
        }
    }

    pub(super) fn repaired(&self) -> bool {
        self.repaired.is_some()
    }

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

    fn own(&mut self, dict: &Dict<'a>, key: Inheritable) -> Read<Option<Object<'a>>> {
        let xref = self.xref;
        self.own
            .entry((dict.content_key(), key as u8))
            .or_insert_with(|| {
                xref.apart(|| match dict.get_raw::<Object<'a>>(key.name())? {
                    MaybeRef::NotRef(Object::Null(_)) => None,
                    MaybeRef::NotRef(value) => Some(value),
                    MaybeRef::Ref(r) => Some(
                        xref.get::<Object<'a>>(r.into())
                            .unwrap_or(Object::Null(Null)),
                    ),
                })
            })
            .clone()
    }

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
            .or_insert_with(|| {
                xref.apart(|| match dict.get_raw::<Object<'a>>(PARENT)? {
                    MaybeRef::Ref(r) => {
                        let id = r.into();
                        Some((Key::Object(id), xref.get::<Dict<'a>>(id)?))
                    }
                    MaybeRef::NotRef(Object::Dict(parent)) => {
                        Some((Key::Written(parent.content_key()), parent))
                    }
                    _ => None,
                })
            })
            .clone()
    }

    fn chain(&mut self, start: &Start<'a, '_>, key: Inheritable) -> Outcome<'a> {
        let k = key as u8;
        // The entries passed, each with the first object reading it lost.
        let mut path = core::mem::take(&mut self.path);

        let mut at = (start.key, start.dict.clone());
        let tail = loop {
            let (entry, dict) = &at;
            if let Some(outcome) = self.chains.get(&(*entry, k)) {
                break outcome.clone();
            }
            // A second visit before the path is completed closes a cycle.
            self.chains.insert(
                (*entry, k),
                Outcome {
                    reach: Reach::Loop,
                    lost: None,
                },
            );
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
        self.path = path;
        outcome
    }

    fn tree(&mut self) -> &Tree<'a> {
        if let Some(tree) = self.walked {
            return tree;
        }
        let xref = self.xref;
        self.read.get_or_insert_with(|| Tree::read(xref))
    }

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
