//! Page tree entries and the objects loading the pages reads, through [`Objects`],
//! which resolves each object once.

use crate::object::dict::keys::{KIDS, MEDIA_BOX, PAGES, TYPE};
use crate::object::{Dict, MaybeRef, Object, ObjectIdentifier};
use crate::reader::ReaderContext;
use crate::sync::FxHashMap;
use crate::xref::{ContentKey, Parsed};

/// What a reference loading reads leads to: an object; `None` where the table lists
/// no object of its number; or, as `Err`, the object the table lists but that cannot
/// be read ([`crate::xref::XRef::get_checked`]), which what refers to it lost.
pub(super) type Resolved<'a> = Result<Option<Object<'a>>, ObjectIdentifier>;

/// A page tree entry: a dictionary, or an empty one for any other object.
#[derive(Clone)]
pub(super) struct Node<'a> {
    id: Option<ObjectIdentifier>,
    dict: Dict<'a>,
    is_dict: bool,
    /// Whether the entry refers to an object the file lists but that cannot be read:
    /// as a page, it lost what it draws.
    lost: bool,
}

impl<'a> Node<'a> {
    fn resolved(id: Option<ObjectIdentifier>, object: Resolved<'a>) -> Self {
        let lost = object.is_err();
        let dict = object.ok().flatten().and_then(dict_of);
        Self {
            id,
            is_dict: dict.is_some(),
            dict: dict.unwrap_or_else(Dict::empty),
            lost,
        }
    }

    /// A page's own entry, which is a dictionary.
    pub(super) fn page(id: Option<ObjectIdentifier>, dict: &Dict<'a>) -> Self {
        Self::resolved(id, Ok(Some(Object::Dict(dict.clone()))))
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

    /// The object the entry refers to, if the file lists it but it cannot be read.
    pub(super) fn lost(&self) -> Option<ObjectIdentifier> {
        self.id.filter(|_| self.lost)
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
/// parsed once, however many entries refer to it; and the only way loading reads an
/// entry of a page tree entry's dictionary, so that each is read once, however many
/// entries of a damaged table lead to the dictionary.
pub(super) struct Objects<'a> {
    ctx: ReaderContext<'a>,
    objects: FxHashMap<ObjectIdentifier, Resolved<'a>>,
    /// What reading the objects found at each offset of the file, so that objects
    /// of a damaged table whose entries lead to one object are each parsed once.
    parsed: Parsed<'a>,
    /// The entries read from dictionaries, by what the dictionary is read from and the
    /// key, as read in the context of the first entry that read them: another entry
    /// reads them in its own ([`Object::in_context`]), which reads the same objects.
    entries: FxHashMap<(ContentKey<'a>, &'static [u8]), Option<MaybeRef<Object<'a>>>>,
}

impl<'a> Objects<'a> {
    pub(super) fn new(ctx: ReaderContext<'a>) -> Self {
        Self {
            ctx,
            objects: FxHashMap::default(),
            parsed: Parsed::default(),
            entries: FxHashMap::default(),
        }
    }

    /// The entry `key` of `dict`, read in its context.
    pub(super) fn entry(
        &mut self,
        dict: &Dict<'a>,
        key: &'static [u8],
    ) -> Option<MaybeRef<Object<'a>>> {
        let entry = self
            .entries
            .entry((dict.content_key(), key))
            .or_insert_with(|| dict.get_raw::<Object<'a>>(key))
            .as_ref()?;
        Some(match entry {
            MaybeRef::Ref(r) => MaybeRef::Ref(*r),
            MaybeRef::NotRef(object) => MaybeRef::NotRef(object.in_context(dict.ctx())),
        })
    }

    // Whether MuPDF sees the entry: a direct `null` is stored as no entry.
    pub(super) fn has(&mut self, node: &Node<'a>, key: &'static [u8]) -> bool {
        #[cfg(test)]
        super::LOOKUPS.set(super::LOOKUPS.get() + 1);
        !matches!(
            self.entry(node.dict(), key),
            None | Some(MaybeRef::NotRef(Object::Null(_)))
        )
    }

    // Whether the entry lists kids rather than being a page: /Type /Pages, or, as
    // MuPDF reads a node without /Type (`pdf_lookup_page_loc_imp`), /Kids and no
    // /MediaBox. Any other entry is a page, including one that is no dictionary.
    pub(super) fn is_tree(&mut self, node: &Node<'a>) -> bool {
        if self.has(node, TYPE) {
            let kind = self.get(node.dict(), TYPE).and_then(Object::into_name);
            kind.as_deref() == Some(PAGES)
        } else {
            self.has(node, KIDS) && !self.has(node, MEDIA_BOX)
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

    /// The object `entry` is or refers to.
    pub(super) fn resolve(&mut self, entry: MaybeRef<Object<'a>>) -> Resolved<'a> {
        match entry {
            MaybeRef::NotRef(object) => Ok(Some(object)),
            MaybeRef::Ref(id) => {
                let id = ObjectIdentifier::from(id);
                let (ctx, parsed) = (&self.ctx, &mut self.parsed);
                self.objects
                    .entry(id)
                    .or_insert_with(|| match ctx.xref().get_parsed(id, ctx, parsed) {
                        None if ctx.xref().is_lost(id, ctx) => Err(id),
                        object => Ok(object),
                    })
                    .clone()
            }
        }
    }

    /// The object `dict` has for `key`, `Ok(None)` without the key.
    pub(super) fn get_checked(&mut self, dict: &Dict<'a>, key: &'static [u8]) -> Resolved<'a> {
        match self.entry(dict, key) {
            Some(entry) => self.resolve(entry),
            None => Ok(None),
        }
    }

    /// The object `dict` has for `key`; `None` without the key, or for a reference
    /// to nothing or to an object that cannot be read.
    pub(super) fn get(&mut self, dict: &Dict<'a>, key: &'static [u8]) -> Option<Object<'a>> {
        self.get_checked(dict, key).ok().flatten()
    }
}
