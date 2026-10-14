//! Page tree entries and the objects loading the pages reads, through [`Objects`],
//! which resolves each object once.

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
        let dict = object.and_then(dict_of);
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

/// The dictionary `object` is, or a stream's, as a reference read as a dictionary
/// finds it.
pub(super) fn dict_of(object: Object<'_>) -> Option<Dict<'_>> {
    match object {
        Object::Dict(dict) => Some(dict),
        Object::Stream(stream) => Some(stream.dict().clone()),
        _ => None,
    }
}

/// The objects met so far while loading the pages, by identifier: the only way from
/// a reference loading reads — a /Kids entry, a /Parent, the catalog's /Pages, an
/// inherited entry, a resource category — to its object, so that each object is
/// parsed once, however many entries refer to it.
pub(super) struct Objects<'a> {
    ctx: ReaderContext<'a>,
    objects: FxHashMap<ObjectIdentifier, Option<Object<'a>>>,
}

impl<'a> Objects<'a> {
    pub(super) fn new(ctx: ReaderContext<'a>) -> Self {
        Self {
            ctx,
            objects: FxHashMap::default(),
        }
    }

    /// The node the catalog's /Pages names, if it has one.
    pub(super) fn tree(&mut self) -> Option<Node<'a>> {
        let xref = self.ctx.xref();
        let tree = xref
            .get_with::<Dict<'a>>(xref.root_id(), &self.ctx)?
            .get_raw::<Object<'a>>(PAGES)?;
        Some(self.node(tree))
    }

    /// The node `entry` is or refers to.
    pub(super) fn node(&mut self, entry: MaybeRef<Object<'a>>) -> Node<'a> {
        let id = match &entry {
            MaybeRef::Ref(id) => Some(ObjectIdentifier::from(*id)),
            MaybeRef::NotRef(_) => None,
        };
        Node::resolved(id, self.resolve(entry))
    }

    /// The object `entry` is or refers to; `None` for a reference to nothing.
    pub(super) fn resolve(&mut self, entry: MaybeRef<Object<'a>>) -> Option<Object<'a>> {
        match entry {
            MaybeRef::NotRef(object) => Some(object),
            MaybeRef::Ref(id) => {
                let id = ObjectIdentifier::from(id);
                let ctx = &self.ctx;
                self.objects
                    .entry(id)
                    .or_insert_with(|| ctx.xref().get_with(id, ctx))
                    .clone()
            }
        }
    }

    /// The object `dict` has for `key`; `None` without the key, or for a reference
    /// to nothing.
    pub(super) fn get(&mut self, dict: &Dict<'a>, key: &[u8]) -> Option<Object<'a>> {
        self.resolve(dict.get_raw::<Object<'a>>(key)?)
    }
}
