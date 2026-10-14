//! Page tree entries, read through [`Nodes`], which resolves each object once.

use crate::object::dict::keys::{KIDS, MEDIA_BOX, PAGES, TYPE};
use crate::object::{Dict, MaybeRef, Name, Object, ObjectIdentifier};
use crate::reader::ReaderContext;
use crate::sync::FxHashMap;

/// A page tree entry: a dictionary, or an empty one for any other object.
#[derive(Clone)]
pub(super) struct Node<'a> {
    id: Option<ObjectIdentifier>,
    dict: Dict<'a>,
    is_dict: bool,
}

impl<'a> Node<'a> {
    fn resolved(id: Option<ObjectIdentifier>, object: Option<Object<'a>>) -> Self {
        let dict = match object {
            Some(Object::Dict(dict)) => Some(dict),
            Some(Object::Stream(stream)) => Some(stream.dict().clone()),
            _ => None,
        };
        Self {
            id,
            is_dict: dict.is_some(),
            dict: dict.unwrap_or_else(Dict::empty),
        }
    }

    /// A page's own entry, which is a dictionary.
    pub(super) fn page(id: Option<ObjectIdentifier>, dict: &Dict<'a>) -> Self {
        Self::resolved(id, Some(Object::Dict(dict.clone())))
    }

    /// The entry's object; `None` for one written directly into a /Kids array.
    pub(super) fn id(&self) -> Option<ObjectIdentifier> {
        self.id
    }

    pub(super) fn dict(&self) -> &Dict<'a> {
        &self.dict
    }

    pub(super) fn into_dict(self) -> Dict<'a> {
        self.dict
    }

    pub(super) fn is_dict(&self) -> bool {
        self.is_dict
    }

    // Whether MuPDF sees the entry: a direct `null` is stored as no entry.
    pub(super) fn has(&self, key: &[u8]) -> bool {
        #[cfg(test)]
        super::LOOKUPS.set(super::LOOKUPS.get() + 1);
        !matches!(
            self.dict.get_raw::<Object<'a>>(key),
            None | Some(MaybeRef::NotRef(Object::Null(_)))
        )
    }

    // Whether the entry lists kids rather than being a page: /Type /Pages, or, as
    // MuPDF reads a node without /Type (`pdf_lookup_page_loc_imp`), /Kids and no
    // /MediaBox. Any other entry is a page, including one that is no dictionary.
    pub(super) fn is_tree(&self) -> bool {
        if self.has(TYPE) {
            self.dict.get::<Name<'_>>(TYPE).as_deref() == Some(PAGES)
        } else {
            self.has(KIDS) && !self.has(MEDIA_BOX)
        }
    }
}

/// The page tree entries met so far, by object: the only way from a /Kids entry,
/// a /Parent or the catalog's /Pages to a [`Node`], so that each object of the
/// tree is parsed once, however many entries list it or have it as /Parent.
pub(super) struct Nodes<'a> {
    ctx: ReaderContext<'a>,
    nodes: FxHashMap<ObjectIdentifier, Node<'a>>,
    /// Whether the xref table had been repaired when `nodes` were read.
    repaired: bool,
}

impl<'a> Nodes<'a> {
    pub(super) fn new(ctx: ReaderContext<'a>) -> Self {
        Self {
            repaired: ctx.xref().is_repaired(),
            ctx,
            nodes: FxHashMap::default(),
        }
    }

    /// The node the catalog's /Pages names, if it has one.
    pub(super) fn tree(&mut self) -> Option<Node<'a>> {
        let xref = self.ctx.xref();
        let tree = xref
            .get_with::<Dict<'a>>(xref.root_id(), &self.ctx)?
            .get_raw::<Object<'a>>(PAGES)?;
        Some(self.get(tree))
    }

    /// The node `entry` is or refers to.
    pub(super) fn get(&mut self, entry: MaybeRef<Object<'a>>) -> Node<'a> {
        let id = match entry {
            MaybeRef::Ref(id) => ObjectIdentifier::from(id),
            MaybeRef::NotRef(object) => return Node::resolved(None, Some(object)),
        };
        self.forget_if_repaired();
        if let Some(node) = self.nodes.get(&id) {
            return node.clone();
        }
        let node = Node::resolved(Some(id), self.ctx.xref().get_with(id, &self.ctx));
        self.forget_if_repaired();
        self.nodes.insert(id, node.clone());
        node
    }

    // A repaired xref table, which an object read may repair, can find other objects
    // than the table they were read from: those are read again, as each is the first
    // time it is met. The table is repaired once, so an object is read at most twice.
    fn forget_if_repaired(&mut self) {
        let repaired = self.ctx.xref().is_repaired();
        if repaired != self.repaired {
            self.repaired = repaired;
            self.nodes.clear();
        }
    }
}
