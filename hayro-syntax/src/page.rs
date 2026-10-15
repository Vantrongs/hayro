//! Reading the pages of a PDF document.

use crate::content::{TypedIter, UntypedIter};
use crate::hash;
use crate::lost::{Lost, Memo};
use crate::object::Array;
use crate::object::Dict;
use crate::object::Name;
use crate::object::Rect;
use crate::object::Stream;
use crate::object::array::ArrayIter;
use crate::object::dict::keys::*;
use crate::object::stream::DecodeFailure;
use crate::object::{MaybeRef, Null, Object, ObjectIdentifier};
use crate::reader::ReaderContext;
use crate::sync::{Arc, OnceLock};
use crate::transform::Transform;
use crate::util::FloatExt;
use crate::xref::{ContentKey, XRef};
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Deref;

mod inherit;
mod node;

use inherit::Listings;
pub use inherit::{Inheritable, Inheritance, InheritanceError};
use node::{Node, Objects, dict_of};

/// A structure holding the pages of a PDF document.
pub struct Pages<'a> {
    pages: Vec<Page<'a>>,
    xref: &'a XRef,
    /// What reading the page tree lost.
    lost: Lost,
}

impl<'a> Pages<'a> {
    // The pages the page tree lists, depth first; where the catalog has no page tree
    // that lists a page, every page object of the file, in object order.
    //
    // MuPDF numbers pages by the root's /Count (`pdf_count_pages` in MuPDF's
    // `pdf-page.c`) and, where the tree is odd, finds each page by the nodes' /Count
    // (`pdf_lookup_page_loc_imp`), so a damaged /Count hides pages or refuses the
    // file. The walk ignores /Count: it hides no page the /Kids hold.
    //
    // What reading the tree loses — a node, a /Kids array or a node's /Type that
    // cannot be read, or, where no tree lists a page, any object — may have held
    // pages: it is kept for the document ([`Self::unreadable`]). What reading a
    // page's entries loses is kept for the page ([`Page::unreadable`]). Neither is
    // told to a computation being watched while the pages are read.
    pub(crate) fn new(ctx: &ReaderContext<'a>, xref: &'a XRef) -> Option<Self> {
        let ((pages, listings, objects), lost) = xref.apart(|| {
            let mut objects = Objects::new(ctx.clone());
            let tree = objects.tree().filter(Node::is_dict);
            let (mut pages, listings) = tree
                .map(|tree| walk(tree, &mut objects))
                .unwrap_or_default();
            if pages.is_empty() {
                pages = brute_force(xref);
            }
            (pages, listings, objects)
        });
        let pages = with_inherited(pages, Arc::new(listings), objects, ctx);

        Some(Self { pages, xref, lost })
    }

    /// An object the page tree refers to that the file lists but that cannot be read,
    /// if there is one: a node, a /Kids array, or a node's /Type, which may have held
    /// pages, or, where no page tree lists a page and every page object of the file
    /// is a page, any object. The pages are those that can be read, as viewers list
    /// them, and a copy of all of them may lose some.
    pub fn unreadable(&self) -> Option<ObjectIdentifier> {
        self.lost
    }

    /// Return the xref table (of the document the pages belong to).   
    pub fn xref(&self) -> &'a XRef {
        self.xref
    }
}

impl<'a> Deref for Pages<'a> {
    type Target = [Page<'a>];

    fn deref(&self) -> &Self::Target {
        &self.pages
    }
}

#[cfg(test)]
std::thread_local! {
    /// The entries looked up in page tree nodes on this thread, which the tests
    /// bound to show that reading the tree takes linear time.
    static LOOKUPS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    /// The /Kids entries read from their arrays and stored on this thread, which
    /// the tests bound to show that shared arrays are read and kept once.
    static KIDS_READ: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    static KIDS_STORED: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

#[cfg(test)]
fn kids_read() {
    KIDS_READ.set(KIDS_READ.get() + 1);
}

#[cfg(test)]
fn kids_stored(entries: usize) {
    KIDS_STORED.set(KIDS_STORED.get() + entries);
}

/// A state for each /Kids array a walk of the page tree reads: the nodes whose
/// /Kids is read from the same bytes share one, whether it is one object or written
/// into one node's dictionary, under any of the entries of a damaged table that lead
/// to it, so that the walk reads the array once however many nodes list it. The walks
/// read /Kids only through this.
///
/// The state's kids are read in the context of the first node that reads them, and a
/// page written into the array in each listing node's ([`Self::own`]): what the kids
/// hold is the same in every one, as the bytes say how they decrypt
/// ([`ContentKey`]), which keys the states.
struct KidsStates<'a, T> {
    states: Vec<T>,
    shared: hash::Map<ContentKey<'a>, usize>,
}

impl<T> Default for KidsStates<'_, T> {
    fn default() -> Self {
        Self {
            states: vec![],
            shared: hash::Map::default(),
        }
    }
}

impl<'a, T> KidsStates<'a, T> {
    /// The index of the state of `node`'s /Kids, made by `new` from its entries if
    /// there is none, and the array, read in the node's context.
    fn of(
        &mut self,
        node: &Node<'a>,
        objects: &mut Objects<'a>,
        new: impl FnOnce(ArrayIter<'a>, &mut Objects<'a>) -> T,
    ) -> (usize, Array<'a>) {
        let kids = objects
            .get(node.dict(), KIDS)
            .and_then(Object::into_array)
            .unwrap_or_default();
        let key = kids.content_key();
        let at = match self.shared.get(&key) {
            Some(at) => *at,
            None => {
                let at = self.states.len();
                self.states.push(new(kids.raw_iter(), objects));
                // An empty array is no object's bytes.
                if !kids.data().is_empty() {
                    self.shared.insert(key, at);
                }
                at
            }
        };
        (at, kids)
    }

    /// `kid`, read from the array of a state as the node listing `kids` reads it.
    fn own(kid: &Node<'a>, kids: &Array<'a>) -> Node<'a> {
        match kid.id() {
            None if kid.is_dict() => Node::page(None, &kid.dict().in_context(kids.ctx())),
            _ => kid.clone(),
        }
    }
}

/// A page tree entry and the index in the [`Listings`] of the node listing it.
type Listed<'a> = (Node<'a>, Option<usize>);

// The pages below `tree`, depth first, and the nodes listing them. A node's kids are
// read the first time the walk reaches it: a node listed again, as a shared subtree,
// in a cycle or in a /Kids array that two nodes share, adds nothing, which keeps the
// walk linear in the file. A page listed again is a page again, as in MuPDF.
//
// The nodes listing one /Kids array, by its bytes, share the entries read from it
// (see `KidsStates`): a node read from it was read then, and adds nothing when
// another node lists it, so each node listing the array lists the pages read from
// it and goes on reading where the others stopped. Each array is thus read once,
// and its pages listed again for each node that lists it, which is the walk's
// output.
fn walk<'a>(tree: Node<'a>, objects: &mut Objects<'a>) -> (Vec<Listed<'a>>, Listings<'a>) {
    /// A /Kids array: the entries not read yet, and the pages among those read.
    struct Kids<'a> {
        unread: ArrayIter<'a>,
        pages: Vec<Node<'a>>,
    }

    let mut pages = vec![];
    let mut listings = Listings::default();
    if !objects.is_tree(&tree) {
        pages.push((tree, None));
        return (pages, listings);
    }
    let mut kids = KidsStates::default();
    // Each node whose kids are being read: its index in `listings`, the index of
    // its /Kids in `kids`, the array as it reads it, and how many of the pages read
    // from them it has listed.
    let mut stack: Vec<(usize, usize, Array<'a>, usize)> = vec![];
    let mut next = Some((tree, None));
    loop {
        if let Some((node, lister)) = next.take()
            && let Some(at) = listings.read(&node, lister, objects)
        {
            let (array, own) = kids.of(&node, objects, |unread, _| Kids {
                unread,
                pages: vec![],
            });
            stack.push((at, array, own, 0));
        }
        let Some((at, array, own, listed)) = stack.last_mut() else {
            return (pages, listings);
        };
        let array = &mut kids.states[*array];
        if let Some(page) = array.pages.get(*listed) {
            pages.push((KidsStates::<'a, Kids<'a>>::own(page, own), Some(*at)));
            *listed += 1;
        } else if let Some(kid) = array.unread.next() {
            #[cfg(test)]
            kids_read();
            let kid = objects.node(kid);
            if objects.is_tree(&kid) {
                next = Some((kid, Some(*at)));
            } else {
                #[cfg(test)]
                kids_stored(1);
                array.pages.push(kid);
            }
        } else {
            listings.close(*at);
            stack.pop();
        }
    }
}

// Every dictionary of the file that has /Contents and, if typed, is a /Page. Better
// than nothing where the page tree is lost, though the order is the objects', which
// may not be the document's. An object that cannot be read may be a page: it is
// lost.
fn brute_force<'a>(xref: &'a XRef) -> Vec<Listed<'a>> {
    xref.note(xref.unreadable_object());
    let mut pages = vec![];
    for object in xref.objects() {
        if let Some(dict) = object.into_dict()
            && dict.contains_key(CONTENTS)
            && dict
                .get::<Name<'_>>(TYPE)
                .is_none_or(|kind| kind.deref() == PAGE)
        {
            pages.push((Node::page(dict.obj_id(), &dict), None));
        }
    }
    pages
}

/// The entries a page inherits, in the order of [`Inheritable::ALL`], and what
/// looking them up lost.
type Inherited<'a> = ([Result<Option<Object<'a>>, InheritanceError>; 4], Lost);

// Each of `entries` as a page with the entries it inherits, found as MuPDF finds
// them once it has loaded every page, which reads these entries: by then a /Parent
// loop any of the lookups meets has been repaired (see `Inheritance`). An entry
// that cannot be found, because a /Parent chain loops even so, is not inherited.
//
// What pages share is read once for all: the categories of resources that are one
// dictionary, which they share through a node or which is one object, and the
// rectangle of a box array. What reading it lost is kept with it, for each page
// that uses it ([`Page::unreadable`]).
fn with_inherited<'a>(
    entries: Vec<Listed<'a>>,
    listings: Arc<Listings<'a>>,
    objects: Objects<'a>,
    ctx: &ReaderContext<'a>,
) -> Vec<Page<'a>> {
    let xref = ctx.xref();
    let mut tree = Inheritance::with(objects);
    let look_up = |tree: &mut Inheritance<'a>| -> Vec<Inherited<'a>> {
        entries
            .iter()
            .map(|(page, lister)| {
                xref.apart(|| {
                    Inheritable::ALL.map(|key| tree.lookup(page, &listings, *lister, key))
                })
            })
            .collect()
    };
    let mut inherited = look_up(&mut tree);
    if tree.is_repaired() {
        inherited = look_up(&mut tree);
    }
    let mut objects = tree.into_objects();
    // By what the dictionary is read from, which pages share where they share it.
    let mut categories = Memo::<ContentKey<'a>, Resources<'a>>::default();
    let mut rects = Memo::<ContentKey<'a>, Option<Rect>>::default();
    entries
        .into_iter()
        .zip(inherited)
        .map(|((page, lister), (inherited, looked_up))| {
            let ((boxes, resources), read) = xref.apart(|| {
                let [media_box, crop_box, rotate, resources] =
                    inherited.map(|entry| entry.ok().flatten());
                let resources = resources.and_then(Object::into_dict).unwrap_or_default();
                let resources = categories
                    .get(xref, resources.content_key(), || {
                        Resources::read(|key| objects.get(&resources, key).and_then(dict_of))
                    })
                    .clone();
                let mut rect = |entry: Option<Object<'a>>| match entry {
                    Some(Object::Array(array)) => {
                        *rects.get(xref, array.content_key(), || rect_of(&array, &mut objects))
                    }
                    _ => None,
                };
                ((rect(media_box), rect(crop_box), rotate), resources)
            });
            Page::new(
                page,
                listings.clone(),
                lister,
                boxes,
                resources,
                looked_up.or(read),
                ctx,
            )
        })
        .collect()
}

/// The rectangle of box `array`, as [`Rect`] reads one: four numbers, each written
/// in the array or an object it refers to, read once for all the boxes that refer
/// to it ([`Objects`]).
fn rect_of<'a>(array: &Array<'a>, objects: &mut Objects<'a>) -> Option<Rect> {
    let mut items = array.raw_iter();
    let mut number = || objects.resolve(items.next()?)?.into_f32().map(f64::from);
    let [x0, y0, x1, y1] = [number()?, number()?, number()?, number()?];
    Some(Rect::new(x0.min(x1), y0.min(y1), x1.max(x0), y1.max(y0)))
}

/// A part of a page's /Contents that cannot be read; the object is `None` for a part
/// written directly into the page or its /Contents array.
#[derive(Debug, Copy, Clone)]
pub enum ContentError {
    /// The part is no stream, or its object cannot be parsed.
    Unreadable(Option<ObjectIdentifier>),
    /// The part's stream cannot be decrypted or decoded.
    Decode(Option<ObjectIdentifier>, DecodeFailure),
}

/// Identifies what a page draws and where, for a cache of what is copied or drawn of
/// pages: its dictionary's bytes ([`Dict::content_key`]), which hold its content and
/// what else it names, the resource categories it is drawn with, its own or
/// inherited, its media and crop boxes and its rotation. Pages the page tree lists
/// with one dictionary, under the same inherited resources and boxes, have equal
/// keys, so that one copy of what they draw serves them all.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct PageKey<'a> {
    dict: ContentKey<'a>,
    resources: [ContentKey<'a>; 7],
    boxes: [u64; 8],
    rotation: u8,
}

/// The decoded data of each body an array of contents names, decoded once however
/// many times it names it.
#[derive(Default)]
struct Decoded<'a>(hash::Map<ContentKey<'a>, Option<Vec<u8>>>, Option<Vec<u8>>);

impl<'a> Decoded<'a> {
    /// The data of the body `key` names, `decode`d the first time it is asked for;
    /// a part with no key is decoded each time.
    fn get(
        &mut self,
        key: Option<ContentKey<'a>>,
        decode: impl FnOnce() -> Option<Vec<u8>>,
    ) -> Option<&[u8]> {
        self.try_get::<()>(key, || Ok(decode())).ok().flatten()
    }

    /// [`Self::get`], where decoding can fail, which is not kept.
    fn try_get<E>(
        &mut self,
        key: Option<ContentKey<'a>>,
        decode: impl FnOnce() -> Result<Option<Vec<u8>>, E>,
    ) -> Result<Option<&[u8]>, E> {
        let Some(key) = key else {
            self.1 = decode()?;
            return Ok(self.1.as_deref());
        };
        if !self.0.contains_key(&key) {
            let data = decode()?;
            self.0.insert(key, data);
        }
        Ok(self.0[&key].as_deref())
    }
}

/// The rotation of the page.
#[derive(Debug, Copy, Clone)]
pub enum Rotation {
    /// No rotation.
    None,
    /// A rotation of 90 degrees.
    Horizontal,
    /// A rotation of 180 degrees.
    Flipped,
    /// A rotation of 270 degrees.
    FlippedHorizontal,
}

/// A PDF page.
pub struct Page<'a> {
    id: Option<ObjectIdentifier>,
    inner: Dict<'a>,
    media_box: Rect,
    crop_box: Rect,
    rotation: Rotation,
    /// The content, and what reading it lost.
    page_streams: OnceLock<(Option<Vec<u8>>, Lost)>,
    resources: Resources<'a>,
    unreadable: Lost,
    /// The nodes the page tree lists pages under, and the index of the one listing
    /// this page.
    listings: Arc<Listings<'a>>,
    lister: Option<usize>,
    ctx: ReaderContext<'a>,
}

impl<'a> Page<'a> {
    fn new(
        node: Node<'a>,
        listings: Arc<Listings<'a>>,
        lister: Option<usize>,
        (media_box, crop_box, rotate): (Option<Rect>, Option<Rect>, Option<Object<'a>>),
        resources: Resources<'a>,
        unreadable: Lost,
        reader_ctx: &ReaderContext<'a>,
    ) -> Self {
        let media_box = media_box.unwrap_or(A4);
        let crop_box = crop_box.unwrap_or(media_box);

        let rotation = match rotate
            .and_then(Object::into_i32)
            .unwrap_or(0)
            .rem_euclid(360)
        {
            0 => Rotation::None,
            90 => Rotation::Horizontal,
            180 => Rotation::Flipped,
            270 => Rotation::FlippedHorizontal,
            _ => Rotation::None,
        };

        Self {
            id: node.id(),
            inner: node.into_dict(),
            media_box,
            crop_box,
            rotation,
            page_streams: OnceLock::new(),
            resources,
            unreadable,
            listings,
            lister,
            ctx: reader_ctx.clone(),
        }
    }

    /// What the page draws and where ([`PageKey`]).
    pub fn key(&self) -> PageKey<'a> {
        let r = &self.resources;
        let corners = |rect: Rect| [rect.x0, rect.y0, rect.x1, rect.y1].map(f64::to_bits);
        let [m0, m1, m2, m3] = corners(self.media_box);
        let [c0, c1, c2, c3] = corners(self.crop_box);
        PageKey {
            dict: self.inner.content_key(),
            resources: [
                &r.ext_g_states,
                &r.fonts,
                &r.properties,
                &r.color_spaces,
                &r.x_objects,
                &r.patterns,
                &r.shadings,
            ]
            .map(Dict::content_key),
            boxes: [m0, m1, m2, m3, c0, c1, c2, c3],
            rotation: self.rotation as u8,
        }
    }

    /// The page object's identifier; `None` for a page written directly into a /Kids
    /// array. A /Kids entry that is not a dictionary is a page with no entries.
    pub fn obj_id(&self) -> Option<ObjectIdentifier> {
        self.id
    }

    fn operations_impl(&self) -> Option<UntypedIter<'_>> {
        let stream = self.page_stream()?;
        let iter = UntypedIter::new(stream);

        Some(iter)
    }

    /// Return the decoded content stream of the page; what reading it lost is told
    /// to the computation being watched each time ([`crate::lost`]). A stream an
    /// array of contents names more than once is decoded once.
    pub fn page_stream(&self) -> Option<&[u8]> {
        let (data, lost) = self.page_streams.get_or_init(|| {
            self.xref().watch(|| {
                if let Some(stream) = self.inner.get::<Stream<'_>>(CONTENTS) {
                    Some(stream.decoded().ok()?.into_owned())
                } else if let Some(array) = self.inner.get::<Array<'_>>(CONTENTS) {
                    let mut decoded = Decoded::default();
                    let mut collected = vec![];
                    // As `array.iter::<Stream>()` reads it: up to the first part that
                    // is no stream.
                    for item in array.raw_iter() {
                        let key = self.body_key(&item);
                        let stream = match item {
                            MaybeRef::Ref(r) => {
                                self.xref().get_with::<Stream<'_>>(r.into(), array.ctx())
                            }
                            MaybeRef::NotRef(object) => object.into_stream(),
                        };
                        let Some(stream) = stream else {
                            break;
                        };
                        if let Some(data) =
                            decoded.get(key, || Some(stream.decoded().ok()?.into_owned()))
                        {
                            collected.extend_from_slice(data);
                            // Streams must have at least one whitespace in-between.
                            collected.push(b' ');
                        }
                    }

                    Some(collected)
                } else {
                    warn!("contents entry of page was neither stream nor array of streams");

                    None
                }
            })
        });
        self.xref().note(*lost);
        data.as_deref()
    }

    /// The decoded content of the page, as [`Self::page_stream`] joins it, or the
    /// first part that cannot be read: unlike `page_stream`, which leaves such a part
    /// out, so that a page that lost content is not taken for a whole one. A missing
    /// or `null` /Contents, or a part that is `null`, is no content. A stream an
    /// array of contents names more than once is decoded once, though the content
    /// holds it each time.
    pub fn try_page_stream(&self) -> Result<Vec<u8>, ContentError> {
        let Some(contents) = self.inner.get_raw::<Object<'a>>(CONTENTS) else {
            return Ok(vec![]);
        };
        let resolved = |object: MaybeRef<Object<'a>>| -> Result<_, ContentError> {
            match object {
                MaybeRef::NotRef(object) => Ok((None, object)),
                MaybeRef::Ref(r) => {
                    let id = ObjectIdentifier::from(r);
                    match self
                        .ctx
                        .xref()
                        .get_checked_with::<Object<'a>>(id, &self.ctx)
                    {
                        Ok(object) => Ok((Some(id), object.unwrap_or(Object::Null(Null)))),
                        Err(id) => Err(ContentError::Unreadable(Some(id))),
                    }
                }
            }
        };
        let decode = |(id, object): (Option<ObjectIdentifier>, Object<'a>)| match object {
            Object::Null(_) => Ok(None),
            Object::Stream(stream) => stream
                .decoded_strict()
                .map(|data| Some(data.into_owned()))
                .map_err(|failure| ContentError::Decode(id, failure)),
            _ => Err(ContentError::Unreadable(id)),
        };

        match resolved(contents)? {
            (_, Object::Array(array)) => {
                let mut decoded = Decoded::default();
                let mut collected = vec![];
                for item in array.raw_iter() {
                    let key = self.body_key(&item);
                    let part = resolved(item)?;
                    let data = decoded.try_get(key, || decode(part))?;
                    if let Some(data) = data {
                        collected.extend_from_slice(data);
                        // Streams must have at least one whitespace in-between.
                        collected.push(b' ');
                    }
                }
                Ok(collected)
            }
            single => Ok(decode(single)?.unwrap_or_default()),
        }
    }

    /// The body a part of an array of contents leads to, where it is a reference.
    fn body_key<T>(&self, item: &MaybeRef<T>) -> Option<ContentKey<'a>> {
        match item {
            MaybeRef::Ref(r) => self.ctx.xref().content_key((*r).into()),
            MaybeRef::NotRef(_) => None,
        }
    }

    /// An object the page's boxes, rotation or resources were read with that the
    /// file lists but that cannot be read, if there is one ([`crate::lost`]): an entry
    /// it inherits ([`Inheritable`]) or an object on the way to it, a number of a box,
    /// or a category of its resources. Such a page reads as if the object were
    /// `null`, as viewers draw it, so a copy of it would lose what the source draws.
    /// What else is read of the page tells the computation reading it itself, its
    /// content ([`Self::try_page_stream`]) and what its dictionary and resources
    /// refer to; what reading the page tree lost is the document's
    /// ([`Pages::unreadable`]).
    pub fn unreadable(&self) -> Option<ObjectIdentifier> {
        self.unreadable
    }

    /// Get the resources of the page.
    pub fn resources(&self) -> &Resources<'a> {
        &self.resources
    }

    /// Get the media box of the page.
    pub fn media_box(&self) -> Rect {
        self.media_box
    }

    /// Get the rotation of the page.
    pub fn rotation(&self) -> Rotation {
        self.rotation
    }

    /// Get the crop box of the page.
    pub fn crop_box(&self) -> Rect {
        self.crop_box
    }

    /// Return the intersection of crop box and media box.
    pub fn intersected_crop_box(&self) -> Rect {
        self.crop_box().intersect(self.media_box())
    }

    /// Return the base dimensions of the page (same as `intersected_crop_box`, but with special
    /// handling applied for zero-area pages).
    pub fn base_dimensions(&self) -> (f32, f32) {
        let crop_box = self.intersected_crop_box();

        if (crop_box.width() as f32).is_nearly_zero() || (crop_box.height() as f32).is_nearly_zero()
        {
            (A4.width() as f32, A4.height() as f32)
        } else {
            (
                crop_box.width().max(1.0) as f32,
                crop_box.height().max(1.0) as f32,
            )
        }
    }

    /// Return the with and height of the page that should be assumed when rendering the page.
    ///
    /// Depending on the document, it is either based on the media box or the crop box
    /// of the page. In addition to that, it also takes the rotation of the page into account.
    pub fn render_dimensions(&self) -> (f32, f32) {
        let (mut base_width, mut base_height) = self.base_dimensions();

        if matches!(
            self.rotation(),
            Rotation::Horizontal | Rotation::FlippedHorizontal
        ) {
            core::mem::swap(&mut base_width, &mut base_height);
        }

        (base_width, base_height)
    }

    /// Return an untyped iterator over the operators of the page's content stream.
    pub fn operations(&self) -> UntypedIter<'_> {
        self.operations_impl().unwrap_or(UntypedIter::empty())
    }

    /// Get the raw dictionary of the page.
    pub fn raw(&self) -> &Dict<'a> {
        &self.inner
    }

    /// Get the xref table (of the document the page belongs to).
    pub fn xref(&self) -> &'a XRef {
        self.ctx.xref()
    }

    /// Return a typed iterator over the operators of the page's content stream.
    pub fn typed_operations(&self) -> TypedIter<'_> {
        TypedIter::from_untyped(self.operations())
    }

    /// Return the initial transform that should be applied when rendering.
    ///
    /// This accounts for the mismatch between PDF's y-up and most renderers'
    /// y-down coordinate system, the rotation of the page and the offset of
    /// the crop box.
    pub fn initial_transform(&self, invert_y: bool) -> Transform {
        let crop_box = self.intersected_crop_box();
        let (_, base_height) = self.base_dimensions();
        let (width, height) = self.render_dimensions();

        let horizontal_t = Transform::ROTATE_CW_90 * Transform::translate((0.0, -width as f64));
        let flipped_horizontal_t =
            Transform::translate((0.0, height as f64)) * Transform::ROTATE_CCW_90;

        let rotation_transform = match self.rotation() {
            Rotation::None => Transform::IDENTITY,
            Rotation::Horizontal => {
                if invert_y {
                    horizontal_t
                } else {
                    flipped_horizontal_t
                }
            }
            Rotation::Flipped => {
                Transform::scale(-1.0) * Transform::translate((-width as f64, -height as f64))
            }
            Rotation::FlippedHorizontal => {
                if invert_y {
                    flipped_horizontal_t
                } else {
                    horizontal_t
                }
            }
        };

        let inversion_transform = if invert_y {
            Transform::new([1.0, 0.0, 0.0, -1.0, 0.0, base_height as f64])
        } else {
            Transform::IDENTITY
        };

        rotation_transform
            * inversion_transform
            * Transform::translate((-crop_box.x0, -crop_box.y0))
    }
}

/// A structure keeping track of the resources of a page.
#[derive(Clone, Debug)]
pub struct Resources<'a> {
    /// The raw dictionary of external graphics states.
    pub ext_g_states: Dict<'a>,
    /// The raw dictionary of fonts.
    pub fonts: Dict<'a>,
    /// The raw dictionary of properties.
    pub properties: Dict<'a>,
    /// The raw dictionary of color spaces.
    pub color_spaces: Dict<'a>,
    /// The raw dictionary of x objects.
    pub x_objects: Dict<'a>,
    /// The raw dictionary of patterns.
    pub patterns: Dict<'a>,
    /// The raw dictionary of shadings.
    pub shadings: Dict<'a>,
}

impl<'a> Resources<'a> {
    /// Create a new `Resources` object.
    pub fn new(resources: Dict<'a>) -> Self {
        Self::read(|key| resources.get::<Dict<'_>>(key))
    }

    /// The resources whose category for each key is `category`'s.
    fn read(mut category: impl FnMut(&'static [u8]) -> Option<Dict<'a>>) -> Self {
        let mut category = |key| category(key).unwrap_or_default();
        Self {
            ext_g_states: category(EXT_G_STATE),
            fonts: category(FONT),
            color_spaces: category(COLORSPACE),
            x_objects: category(XOBJECT),
            patterns: category(PATTERN),
            shadings: category(SHADING),
            properties: category(PROPERTIES),
        }
    }

    /// Get an external graphics state by name.
    pub fn get_ext_g_state(&self, name: &Name<'_>) -> Option<Dict<'a>> {
        self.ext_g_states.get(name)
    }

    /// Get a color space by name.
    pub fn get_color_space(&self, name: &Name<'_>) -> Option<Object<'a>> {
        self.color_spaces.get(name)
    }

    /// Get a font by name.
    pub fn get_font(&self, name: &Name<'_>) -> Option<Dict<'a>> {
        self.fonts.get(name)
    }

    /// Get a pattern by name.
    pub fn get_pattern(&self, name: &Name<'_>) -> Option<Object<'a>> {
        self.patterns.get(name)
    }

    /// Get an x object by name.
    pub fn get_x_object(&self, name: &Name<'_>) -> Option<Stream<'a>> {
        self.x_objects.get(name)
    }

    /// Get a shading by name.
    pub fn get_shading(&self, name: &Name<'_>) -> Option<Object<'a>> {
        self.shadings.get(name)
    }
}

// <https://github.com/apache/pdfbox/blob/a53a70db16ea3133994120bcf1e216b9e760c05b/pdfbox/src/main/java/org/apache/pdfbox/pdmodel/common/PDRectangle.java#L38>
const POINTS_PER_INCH: f64 = 72.0;
const POINTS_PER_MM: f64 = 1.0 / (10.0 * 2.54) * POINTS_PER_INCH;

/// The dimension of an A4 page.
pub const A4: Rect = Rect {
    x0: 0.0,
    y0: 0.0,
    x1: 210.0 * POINTS_PER_MM,
    y1: 297.0 * POINTS_PER_MM,
};

pub(crate) mod cached {
    use crate::page::Pages;
    use crate::reader::ReaderContext;
    use crate::xref::XRef;
    use core::ops::Deref;

    // Keep in sync with the implementation in `sync`. We duplicate it here
    // to make it more visible since we have unsafe code here.
    #[cfg(feature = "std")]
    pub(crate) use std::sync::Arc;

    #[cfg(not(feature = "std"))]
    pub(crate) use alloc::rc::Rc as Arc;

    pub(crate) struct CachedPages {
        pages: Pages<'static>,
        // NOTE: `pages` references the data in `xref`, so it's important that `xref`
        // appears after `pages` in the struct definition to ensure correct drop order.
        _xref: Arc<XRef>,
    }

    impl CachedPages {
        pub(crate) fn new(xref: Arc<XRef>) -> Option<Self> {
            // SAFETY:
            // - The XRef's location is stable in memory:
            //   - We wrapped it in a `Arc` (or `Rc` in `no_std`), which implements `StableDeref`.
            //   - The struct owns the `Arc`, ensuring that the inner value is not dropped during the whole
            //     duration.
            // - The internal 'static lifetime is not leaked because its rewritten
            //   to the self-lifetime in `pages()`.
            let xref_reference: &'static XRef = unsafe { core::mem::transmute(xref.deref()) };

            let ctx = ReaderContext::new(xref_reference, false);
            let pages = Pages::new(&ctx, xref_reference)?;

            Some(Self { pages, _xref: xref })
        }

        pub(crate) fn get(&self) -> &Pages<'_> {
            &self.pages
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{A4, Inheritable, Inheritance};
    use crate::object::{Dict, ObjectIdentifier, Rect};
    use crate::pdf::Pdf;
    use alloc::borrow::ToOwned;
    use alloc::string::String;
    use alloc::vec::Vec;

    const CATALOG: &str = "<< /Type /Catalog /Pages 2 0 R >>";

    /// A PDF of `objects`, numbered from 1; the first is the catalog.
    fn pdf(objects: &[&str]) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = vec![];
        for (i, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{object}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for offset in offsets {
            out.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        out
    }

    // Each page's object number (0 for a direct one) and MediaBox width, or `None`
    // where the document does not open.
    fn pages(objects: &[&str]) -> Option<Vec<(i32, Option<f64>)>> {
        let pdf = Pdf::new(pdf(objects)).ok()?;
        Some(
            pdf.pages()
                .iter()
                .map(|page| {
                    let number = page.obj_id().map_or(0, |id| id.obj_number);
                    (number, page.raw().get::<Rect>(b"MediaBox").map(|r| r.x1))
                })
                .collect(),
        )
    }

    fn pages_node(count: &str, kids: &str) -> String {
        format!("<< /Type /Pages /Count {count} /Kids [{kids}] >>")
    }

    const PAGE: &str = "<< /Type /Page /MediaBox [0 0 1600 1000] >>";
    const SMALL: &str = "<< /Type /Page /MediaBox [0 0 200 100] >>";

    #[test]
    fn a_node_without_type_is_a_node() {
        let node = "<< /Kids [4 0 R] /Count 1 >>";
        let tree = pages_node("1", "3 0 R");
        assert_eq!(
            pages(&[CATALOG, &tree, node, PAGE]),
            Some(vec![(4, Some(1600.0))])
        );
    }

    #[test]
    fn a_node_without_type_but_with_a_media_box_is_a_page() {
        let node = "<< /Kids [4 0 R] /Count 1 /MediaBox [0 0 300 300] >>";
        let tree = pages_node("1", "3 0 R");
        assert_eq!(
            pages(&[CATALOG, &tree, node, PAGE]),
            Some(vec![(3, Some(300.0))])
        );
    }

    #[test]
    fn a_page_with_a_bad_number_is_kept() {
        let tree = pages_node("3", "3 0 R 4 0 R 5 0 R");
        let bad = "<< /Type /Page /MediaBox [0 0 1e3 2e3] /Rotate 9x >>";
        let pdf = pdf(&[CATALOG, &tree, PAGE, bad, PAGE]);
        let pdf = Pdf::new(pdf).unwrap();
        let ids: Vec<_> = pdf.pages().iter().map(|p| p.obj_id()).collect();
        assert_eq!(ids, [3, 4, 5].map(|n| Some(ObjectIdentifier::new(n, 0))));
        let bad: &Dict<'_> = pdf.pages()[1].raw();
        assert!(bad.get::<crate::object::Array<'_>>(b"MediaBox").is_some());
    }

    #[test]
    fn a_damaged_count_hides_no_page() {
        for count in [
            "",
            "/Count 0",
            "/Count 1",
            "/Count -1",
            "/Count 5",
            "/Count 2.5",
        ] {
            let tree = format!("<< /Type /Pages {count} /Kids [3 0 R 4 0 R] >>");
            assert_eq!(
                pages(&[CATALOG, &tree, PAGE, SMALL]),
                Some(vec![(3, Some(1600.0)), (4, Some(200.0))]),
                "{count}"
            );
        }
    }

    #[test]
    fn a_count_lower_inside_the_tree_hides_no_page() {
        // MuPDF searches by /Count past the untyped node, and the first node's
        // /Count 1 hides its second kid there.
        let tree = pages_node("2", "3 0 R 5 0 R");
        let low = "<< /Type /Pages /Kids [4 0 R 6 0 R] /Count 1 >>";
        let untyped = "<< /Kids [7 0 R] /Count 1 >>";
        let other = "<< /Type /Page /MediaBox [0 0 300 100] >>";
        assert_eq!(
            pages(&[CATALOG, &tree, low, PAGE, untyped, SMALL, other]),
            Some(vec![(4, Some(1600.0)), (6, Some(200.0)), (7, Some(300.0))])
        );
    }

    #[test]
    fn kids_that_are_not_pages_are_empty_pages() {
        let tree = pages_node("4", "3 0 R 9 0 R 5 4 0 R");
        let other = "<< /Type /Foo /MediaBox [0 0 300 100] >>";
        assert_eq!(
            pages(&[CATALOG, &tree, PAGE, other]),
            Some(vec![
                (3, Some(1600.0)),
                (9, None),
                (0, None),
                (4, Some(300.0))
            ])
        );
    }

    #[test]
    fn a_page_listed_twice_is_two_pages() {
        let tree = pages_node("2", "3 0 R 3 0 R");
        assert_eq!(
            pages(&[CATALOG, &tree, PAGE]),
            Some(vec![(3, Some(1600.0)), (3, Some(1600.0))])
        );
        // Listed by two nodes, it is a page under each.
        let tree = pages_node("2", "3 0 R 4 0 R");
        let node = pages_node("1", "5 0 R");
        assert_eq!(
            pages(&[CATALOG, &tree, &node, &node, PAGE]),
            Some(vec![(5, Some(1600.0)), (5, Some(1600.0))])
        );
    }

    #[test]
    fn a_node_listed_again_adds_nothing() {
        // A cycle back to the root, then the page after it.
        let tree = pages_node("2", "3 0 R 4 0 R 5 0 R");
        let cycle = pages_node("1", "2 0 R");
        assert_eq!(
            pages(&[CATALOG, &tree, PAGE, &cycle, SMALL]),
            Some(vec![(3, Some(1600.0)), (5, Some(200.0))])
        );
        // A subtree the root lists twice.
        let tree = pages_node("2", "3 0 R 3 0 R");
        let node = pages_node("1", "4 0 R");
        assert_eq!(
            pages(&[CATALOG, &tree, &node, PAGE]),
            Some(vec![(4, Some(1600.0))])
        );
    }

    #[test]
    fn shared_subtrees_are_read_in_linear_time() {
        // Each node lists the next twice: walking every listing would take 2^40 steps.
        // The last node lists the page twice, which is two pages.
        let depth = 40;
        let mut objects = vec![CATALOG.to_owned()];
        for level in 0..depth {
            let next = level + 3;
            objects.push(pages_node("1", &format!("{next} 0 R {next} 0 R")));
        }
        objects.push(PAGE.to_owned());
        let objects: Vec<&str> = objects.iter().map(String::as_str).collect();
        let start = std::time::Instant::now();
        let page = (depth + 2, Some(1600.0));
        assert_eq!(pages(&objects), Some(vec![page, page]));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    // The /XObject names of each page's resources, its MediaBox width and its
    // rotation in quarter turns.
    fn inherited(objects: &[&str]) -> Vec<(Vec<String>, f64, u8)> {
        inherited_in(pdf(objects))
    }

    fn inherited_in(file: Vec<u8>) -> Vec<(Vec<String>, f64, u8)> {
        let pdf = Pdf::new(file).unwrap();
        pdf.pages()
            .iter()
            .map(|page| {
                let names = page
                    .resources()
                    .x_objects
                    .keys()
                    .map(|name| name.as_str().to_owned())
                    .collect();
                let quarters = match page.rotation() {
                    super::Rotation::None => 0,
                    super::Rotation::Horizontal => 1,
                    super::Rotation::Flipped => 2,
                    super::Rotation::FlippedHorizontal => 3,
                };
                (names, page.media_box().x1, quarters)
            })
            .collect()
    }

    fn named(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    // A page inherits through its /Parent, as MuPDF reads it, not from the node
    // that lists it; past a direct `null`, and not past an entry that is no box.
    #[test]
    fn a_page_inherits_from_its_parent() {
        let tree = "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 100 100] \
                    /Resources << /XObject << /Root 9 0 R >> >> >>";
        let other = "<< /Type /Pages /Kids [] /Count 0 /MediaBox [0 0 300 300] /Rotate 90 \
                     /Resources << /XObject << /X1 9 0 R >> >> >>";
        let listed = "<< /Type /Page /Parent 5 0 R /MediaBox null >>";
        let bad_box = "<< /Type /Page /Parent 2 0 R /MediaBox (none) /Resources 9 0 R >>";
        assert_eq!(
            inherited(&[CATALOG, tree, listed, bad_box, other]),
            [(named(&["X1"]), 300.0, 1), (named(&[]), A4.x1, 0)]
        );
    }

    // A /Parent loop makes MuPDF set every node's /Parent to the node that lists
    // it, the last listing in its walk winning: here node 6, listed by nodes 3 and
    // 5, over node 4, which lists the page between them.
    #[test]
    fn a_parent_loop_is_repaired_from_the_last_listing() {
        let form = |name: &str| format!("/Resources << /XObject << /{name} 9 0 R >> >>");
        let objects = [
            CATALOG.to_owned(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /MediaBox [0 0 300 300] >>".to_owned(),
            "<< /Type /Pages /Kids [6 0 R] >>".to_owned(),
            format!("<< /Type /Pages /Kids [7 0 R] {} >>", form("Z")),
            "<< /Type /Pages /Kids [6 0 R] >>".to_owned(),
            format!("<< /Type /Pages /Kids [7 0 R] {} >>", form("X")),
            "<< /Type /Page /Parent 7 0 R >>".to_owned(),
        ];
        let objects: Vec<&str> = objects.iter().map(String::as_str).collect();
        let page = (named(&["X"]), 300.0, 0);
        assert_eq!(inherited(&objects), [page.clone(), page]);
    }

    #[test]
    fn a_parent_loop_in_shared_subtrees_is_repaired_in_linear_time() {
        // As `shared_subtrees_are_read_in_linear_time`, with a page whose /Parent
        // is itself, which makes the lookups repair the tree: walking every listing
        // would take 2^40 steps.
        let depth = 40;
        let mut objects = vec![CATALOG.to_owned()];
        for level in 0..depth {
            let next = level + 3;
            let entries = if level == 0 {
                "/MediaBox [0 0 300 300]"
            } else {
                ""
            };
            objects.push(format!(
                "<< /Type /Pages /Kids [{next} 0 R {next} 0 R] {entries} >>"
            ));
        }
        objects.push(format!("<< /Type /Page /Parent {} 0 R >>", depth + 2));
        let objects: Vec<&str> = objects.iter().map(String::as_str).collect();
        let start = std::time::Instant::now();
        let page = (named(&[]), 300.0, 0);
        assert_eq!(inherited(&objects), [page.clone(), page]);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    // Where the /Parent chain ends at an entry that has no /Parent, or a /Parent
    // that is no dictionary, the entry inherits through the nodes that list it, as
    // the page tree shows them: page 3 has no /Parent, page 4's /Parent is a
    // number, and page 6's /Parent is node 5, which has no /Parent. Page 7's
    // /Parent is a node outside the tree, which is where it inherits from.
    #[test]
    fn an_entry_without_a_parent_inherits_from_the_nodes_listing_it() {
        let objects = [
            CATALOG,
            "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 7 0 R] /Count 4 /MediaBox [0 0 300 300] \
             /Rotate 90 /Resources << /XObject << /F 9 0 R >> >> >>",
            "<< /Type /Page >>",
            "<< /Type /Page /Parent 5 >>",
            "<< /Type /Pages /Kids [6 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 5 0 R >>",
            "<< /Type /Page /Parent 8 0 R >>",
            "<< /Type /Pages /Kids [] /Count 0 >>",
        ];
        let page = (named(&["F"]), 300.0, 1);
        assert_eq!(
            inherited(&objects),
            [page.clone(), page.clone(), page, (named(&[]), A4.x1, 0)]
        );
    }

    // The root lists node 3 first and then a chain of 255 nodes ending at node 3
    // too. A walk that reaches node 3 first through the chain, 257 levels down,
    // must still set the /Parent of page 4, whose /Parent is itself, below it.
    #[test]
    fn a_parent_loop_is_repaired_below_a_node_first_met_deep_down() {
        let chain = 255;
        let mut objects = vec![
            CATALOG.to_owned(),
            "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Pages /Kids [4 0 R] /Count 1 /Resources << /XObject << /F 9 0 R >> >> >>"
                .to_owned(),
            "<< /Type /Page /Parent 4 0 R /MediaBox [0 0 100 100] >>".to_owned(),
        ];
        for i in 0..chain {
            let next = if i + 1 < chain { 5 + i + 1 } else { 3 };
            objects.push(pages_node("1", &format!("{next} 0 R")));
        }
        let objects: Vec<&str> = objects.iter().map(String::as_str).collect();
        assert_eq!(inherited(&objects), [(named(&["F"]), 100.0, 0)]);
    }

    // A /Parent loop is repaired at any depth, past a kid that is no dictionary,
    // into which `MuPDF` fails to write a /Parent: page 3's /Parent is itself, and
    // the root lists a chain of 300 nodes whose last lists a page whose /Parent is
    // itself and a reference to nothing, which is an empty page.
    #[test]
    fn a_parent_loop_is_repaired_at_any_depth_past_a_kid_that_is_no_dictionary() {
        let depth = 300;
        let mut objects = vec![
            CATALOG.to_owned(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 3 /MediaBox [0 0 300 300] >>".to_owned(),
            "<< /Type /Page /Parent 3 0 R >>".to_owned(),
        ];
        let page = 4 + depth;
        for i in 0..depth {
            let kids = if i + 1 < depth {
                format!("{} 0 R", 5 + i)
            } else {
                format!("{page} 0 R 9999 0 R")
            };
            objects.push(pages_node("1", &kids));
        }
        objects.push(format!("<< /Type /Page /Parent {page} 0 R >>"));
        let objects: Vec<&str> = objects.iter().map(String::as_str).collect();
        let page = (named(&[]), 300.0, 0);
        assert_eq!(inherited(&objects), [page.clone(), page.clone(), page]);
    }

    // The root lists a chain of `depth` nodes, each listing the next, and the last
    // lists `pages` pages. Only the root has entries a page inherits; each node and
    // page has a /Parent to the node listing it where `parents` holds.
    fn nested(depth: usize, pages: usize, parents: bool) -> Vec<u8> {
        let first_page = depth + 3;
        let parent = |of: usize| {
            if parents {
                format!("/Parent {of} 0 R")
            } else {
                String::new()
            }
        };
        let mut objects = vec![
            CATALOG.to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /MediaBox [0 0 200 100] /Rotate 90 \
             /Resources << /XObject << /F 9 0 R >> >> >>"
                .to_owned(),
        ];
        for level in 0..depth {
            let number = level + 3;
            let kids: Vec<String> = if level + 1 < depth {
                vec![format!("{} 0 R", number + 1)]
            } else {
                (0..pages)
                    .map(|p| format!("{} 0 R", first_page + p))
                    .collect()
            };
            objects.push(format!(
                "<< /Type /Pages /Kids [{}] {} >>",
                kids.join(" "),
                parent(number - 1)
            ));
        }
        for _ in 0..pages {
            objects.push(format!("<< /Type /Page {} >>", parent(depth + 2)));
        }
        let objects: Vec<&str> = objects.iter().map(String::as_str).collect();
        pdf(&objects)
    }

    // A page under 100,000 nested nodes opens and is dropped on a thread with the
    // 1 MB stack of a render thread: nothing takes stack in proportion to the depth.
    #[test]
    fn a_page_under_deeply_nested_nodes_opens_and_drops_on_a_small_stack() {
        for parents in [false, true] {
            let file = nested(100_000, 1, parents);
            let read = std::thread::Builder::new()
                .stack_size(1 << 20)
                .spawn(move || {
                    let pdf = Pdf::new(file).unwrap();
                    let page = &pdf.pages()[0];
                    let names: Vec<String> = page
                        .resources()
                        .x_objects
                        .keys()
                        .map(|name| name.as_str().to_owned())
                        .collect();
                    (names, page.media_box().x1)
                })
                .unwrap()
                .join()
                .unwrap();
            assert_eq!(read, (named(&["F"]), 200.0), "parents: {parents}");
        }
    }

    // The entries `pages` pages under `depth` nested nodes inherit are found with a
    // number of dictionary lookups linear in the nodes and pages, whether the pages
    // reach the root through /Parent or only through the nodes listing them.
    #[test]
    fn inherited_entries_are_found_in_linear_time() {
        for parents in [false, true] {
            let lookups = |n: usize| {
                let file = nested(n, n, parents);
                super::LOOKUPS.set(0);
                let pdf = Pdf::new(file).unwrap();
                assert!(pdf.pages().iter().all(|page| page.media_box().x1 == 200.0));
                super::LOOKUPS.get()
            };
            let [small, large] = [1000, 2000].map(lookups);
            assert!(
                large < small * 21 / 10,
                "parents: {parents}: {small} lookups for 1000, {large} for 2000"
            );
        }
    }

    // Each /Kids array is a shared object whose two direct nodes list the next array:
    // walking every listing would take 2^40 steps. A direct node met again adds
    // nothing, as an indirect one: the last array's page is a page under each of the
    // two nodes listing it.
    #[test]
    fn shared_kids_arrays_of_direct_nodes_are_read_in_linear_time() {
        let depth: i32 = 40;
        let mut objects = vec![
            CATALOG.to_owned(),
            "<< /Type /Pages /Kids 3 0 R >>".to_owned(),
        ];
        for level in 0..depth {
            let next = level + 4;
            let node = format!("<< /Type /Pages /Kids {next} 0 R >>");
            objects.push(format!("[{node} {node}]"));
        }
        objects.push(format!("[{} 0 R]", depth + 4));
        objects.push(PAGE.to_owned());
        let objects: Vec<&str> = objects.iter().map(String::as_str).collect();
        let page = (depth + 4, Some(1600.0));
        assert_eq!(pages(&objects), Some(vec![page, page]));
    }

    // A root and `n` nodes all list one /Kids array object, which lists those nodes
    // and a page: the page is a page under each, and the array is read once. With
    // `parent_loop`, the page's /Parent is itself, which makes the lookups repair
    // the tree, reading the array once more.
    fn shared_kids(n: usize, parent_loop: bool) -> Vec<u8> {
        let page = n + 4;
        let nodes: Vec<String> = (4..page).map(|node| format!("{node} 0 R")).collect();
        let mut objects = vec![
            CATALOG.to_owned(),
            "<< /Type /Pages /Kids 3 0 R /MediaBox [0 0 200 100] >>".to_owned(),
            format!("[{} {page} 0 R]", nodes.join(" ")),
        ];
        objects.extend((0..n).map(|_| "<< /Type /Pages /Kids 3 0 R >>".to_owned()));
        let parent = if parent_loop {
            format!("/Parent {page} 0 R")
        } else {
            String::new()
        };
        objects.push(format!("<< /Type /Page {parent} >>"));
        let objects: Vec<&str> = objects.iter().map(String::as_str).collect();
        pdf(&objects)
    }

    // The /Kids entries read and stored for such a file grow with it, not with its
    // square, with and without the repair.
    #[test]
    fn a_kids_array_many_nodes_list_is_read_and_stored_once() {
        for parent_loop in [false, true] {
            let counts = |n: usize| {
                let file = shared_kids(n, parent_loop);
                super::KIDS_READ.set(0);
                super::KIDS_STORED.set(0);
                let pdf = Pdf::new(file).unwrap();
                assert_eq!(pdf.pages().len(), n + 1);
                for page in pdf.pages().iter() {
                    assert_eq!(page.obj_id(), Some(ObjectIdentifier::new(n as i32 + 4, 0)));
                    assert_eq!(page.media_box().x1, 200.0);
                }
                (super::KIDS_READ.get(), super::KIDS_STORED.get())
            };
            let [small, large] = [500, 1000].map(counts);
            assert!(
                large.0 < small.0 * 21 / 10 && large.1 < small.1 * 21 / 10,
                "parent loop: {parent_loop}: (read, stored) {small:?} for 500, {large:?} for 1000"
            );
        }
    }

    /// A file of `objects`, numbered from 1, the first being the catalog, whose table
    /// also lists each number of `aliases` at the object of the number paired with it:
    /// the headers there name another number, so the table is rebuilt, keeping them.
    fn aliased(objects: &[String], aliases: &[(usize, usize)]) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = vec![0];
        for (i, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{object}\nendobj\n", i + 1).bytes());
        }
        let size = aliases
            .iter()
            .map(|(n, _)| n + 1)
            .fold(offsets.len(), usize::max);
        let mut entries = vec![0; size];
        entries[..offsets.len()].copy_from_slice(&offsets);
        for &(number, target) in aliases {
            entries[number] = offsets[target];
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {size}\n").bytes());
        for offset in entries {
            let kind = if offset == 0 { "65535 f" } else { "00000 n" };
            out.extend(format!("{offset:010} {kind} \n").bytes());
        }
        out.extend(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").bytes(),
        );
        out
    }

    // A page, 3, and a node, 5, that `n` entries of the table lead to: with `direct`,
    // its /Kids array, written into it, lists itself under all of them, and its
    // /Rotate, which the pages could inherit, is an array as large; otherwise each of
    // `n` nodes lists a /Kids array object, 4, under one of `n` entries leading to
    // it, which lists them all. With `parent_loop`, the page's /Parent is itself,
    // which makes the lookups repair the tree. The file grows with `n`, and is one
    // page.
    fn aliased_kids(n: usize, direct: bool, parent_loop: bool) -> Vec<u8> {
        let parent = if parent_loop { "/Parent 3 0 R" } else { "" };
        let mut objects = vec![
            CATALOG.to_owned(),
            pages_node("1", "3 0 R 5 0 R"),
            format!("<< /Type /Page /MediaBox [0 0 200 100] {parent} >>"),
        ];
        let refs = |numbers: core::ops::Range<usize>| {
            numbers
                .map(|k| format!("{k} 0 R"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let aliases: Vec<(usize, usize)> = if direct {
            objects.push("null".to_owned());
            objects.push(format!(
                "<< /Type /Pages /Kids [{}] /Rotate [{}] >>",
                refs(5..n + 5),
                " 0".repeat(n)
            ));
            (6..n + 5).map(|alias| (alias, 5)).collect()
        } else {
            objects.push(format!("[{}]", refs(5..n + 5)));
            objects.extend((0..n).map(|k| format!("<< /Type /Pages /Kids {} 0 R >>", n + 5 + k)));
            (n + 5..2 * n + 5).map(|alias| (alias, 4)).collect()
        };
        aliased(&objects, &aliases)
    }

    // Each /Kids array is read once, however many entries of the table lead to it or
    // to the node holding it, and so is each entry of the node: the /Kids entries
    // read, the node entries looked up, and the bytes of dictionaries and arrays
    // parsed for such a file grow with it, not with its square, with and without the
    // repair.
    #[test]
    fn a_kids_array_entries_lead_to_is_read_once() {
        use crate::object::dict::PARSED;
        for direct in [true, false] {
            for parent_loop in [false, true] {
                let counts = |n: usize| {
                    let file = aliased_kids(n, direct, parent_loop);
                    super::KIDS_READ.set(0);
                    super::LOOKUPS.set(0);
                    PARSED.set((0, 0));
                    let pdf = Pdf::new(file).unwrap();
                    let pages = pdf.pages();
                    assert_eq!(pages.len(), 1);
                    assert_eq!(pages[0].obj_id(), Some(ObjectIdentifier::new(3, 0)));
                    assert_eq!(pages[0].media_box().x1, 200.0);
                    [super::KIDS_READ.get(), super::LOOKUPS.get(), PARSED.get().1]
                };
                let [small, large] = [500, 1000].map(counts);
                assert!(
                    (0..3).all(|i| large[i] < small[i] * 21 / 10),
                    "direct: {direct}, parent loop: {parent_loop}: [kids read, lookups, \
                     bytes parsed] {small:?} for 500, {large:?} for 1000"
                );
            }
        }
    }

    /// The objects other than the catalog that `run` resolves more than once.
    fn resolved_again(run: impl FnOnce()) -> Vec<(ObjectIdentifier, usize)> {
        crate::xref::RESOLVED.set(Some(crate::hash::Map::default()));
        run();
        let resolved = crate::xref::RESOLVED.take().unwrap();
        let mut again: Vec<_> = resolved
            .into_iter()
            .filter(|&(id, count)| count > 1 && id != ObjectIdentifier::new(1, 0))
            .collect();
        again.sort();
        again
    }

    /// Each page's object number, once it is checked that no object is resolved
    /// twice while `file` loads, nor while an [`Inheritance`] of its own looks up
    /// every entry of every page, as a reader of the pages does. Parsing then takes
    /// time linear in the file, however often an object is listed.
    fn resolved_once(file: Vec<u8>) -> Vec<i32> {
        let mut pdf = None;
        assert_eq!(resolved_again(|| pdf = Some(Pdf::new(file).unwrap())), []);
        let pdf = pdf.unwrap();
        let lookups = || {
            let mut tree = Inheritance::new(&pdf);
            for page in pdf.pages().iter() {
                for key in Inheritable::ALL {
                    let _ = tree.get(page, key);
                }
            }
        };
        assert_eq!(resolved_again(lookups), []);
        pdf.pages()
            .iter()
            .map(|page| page.obj_id().map_or(0, |id| id.obj_number))
            .collect()
    }

    // The root lists node 3 `n` times, and node 3 lists an empty node `n` times and
    // page 5, whose /Parent, with `parent_loop`, is itself, which makes the lookups
    // repair the tree: the file grows with `n`, and is one page.
    fn repeated_nodes(n: usize, parent_loop: bool) -> Vec<u8> {
        let parent = if parent_loop { "/Parent 5 0 R" } else { "" };
        let objects = [
            CATALOG.to_owned(),
            pages_node("1", &["3 0 R"; 1000][..n].join(" ")),
            pages_node("1", &format!("{} 5 0 R", ["4 0 R"; 1000][..n].join(" "))),
            pages_node("0", ""),
            format!("<< /Type /Page /MediaBox [0 0 200 100] {parent} >>"),
        ];
        pdf(&objects.each_ref().map(String::as_str))
    }

    #[test]
    fn a_node_listed_many_times_is_resolved_once() {
        for parent_loop in [false, true] {
            for n in [500, 1000] {
                assert_eq!(
                    resolved_once(repeated_nodes(n, parent_loop)),
                    [5],
                    "parent loop: {parent_loop}, n: {n}"
                );
            }
        }
    }

    // A page as large as the file, listed `n` times, is `n` pages, read once.
    #[test]
    fn a_page_listed_many_times_is_resolved_once() {
        for n in [500, 1000] {
            let page = format!(
                "<< /Type /Page /MediaBox [0 0 200 100] /Pad [{}] >>",
                "0 ".repeat(n)
            );
            let tree = pages_node(&n.to_string(), &"3 0 R ".repeat(n));
            assert_eq!(resolved_once(pdf(&[CATALOG, &tree, &page])), vec![3; n]);
        }
    }

    // Pages without /Parent inherit the root's /Resources, an object as large as the
    // file, through the node listing them: each page finds it, and it is read once.
    #[test]
    fn an_entry_pages_inherit_through_the_listing_is_resolved_once() {
        for n in [500, 1000] {
            let pages: Vec<String> = (0..n).map(|page| format!("{} 0 R", page + 4)).collect();
            let tree = format!(
                "<< /Type /Pages /Kids [{}] /Resources 3 0 R >>",
                pages.join(" ")
            );
            let names: Vec<String> = (0..n).map(|name| format!("/X{name} 9 0 R")).collect();
            let resources = format!("<< /XObject << {} >> >>", names.join(" "));
            let mut objects = vec![CATALOG, &tree, &resources];
            objects.extend([SMALL; 1000][..n].iter());
            let numbers: Vec<i32> = (4..n as i32 + 4).collect();
            assert_eq!(resolved_once(pdf(&objects)), numbers);
            let pdf = Pdf::new(pdf(&objects)).unwrap();
            assert!(
                pdf.pages()
                    .iter()
                    .all(|page| page.resources().x_objects.keys().count() == n)
            );
        }
    }

    /// A damaged file without a cross-reference table, whose objects, each a number,
    /// a generation and a body, are found by scanning it.
    fn recovered(objects: &[(i32, i32, &str)]) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        for (number, generation, body) in objects {
            out.extend(format!("{number} {generation} obj\n{body}\nendobj\n").bytes());
        }
        out.extend(b"trailer\n<< /Root 1 0 R >>\n%%EOF\n");
        out
    }

    /// The entries and bytes of the dictionaries parsed while `file` loads and an
    /// [`Inheritance`] of its own looks up every entry of every page, as a reader of
    /// the pages does, once it is checked that each page has the `fonts` fonts.
    fn parsed(file: Vec<u8>, fonts: usize) -> (usize, usize) {
        use crate::object::dict::PARSED;
        PARSED.set((0, 0));
        let pdf = Pdf::new(file).unwrap();
        let mut tree = Inheritance::new(&pdf);
        for page in pdf.pages().iter() {
            for key in Inheritable::ALL {
                let _ = tree.get(page, key);
            }
        }
        let parsed = PARSED.get();
        for page in pdf.pages().iter() {
            assert_eq!(page.resources().fonts.len(), fonts);
        }
        parsed
    }

    /// A file whose pages are `n` entries of its table that lead to one page of an
    /// array of `n` numbers: the table is rebuilt, and keeps the entries. Without a
    /// `tree`, the catalog's /Pages is free, and every page object is a page.
    fn aliased_page(n: usize, tree: bool) -> Vec<u8> {
        let kids: Vec<String> = (5..n + 5).map(|k| format!("{k} 0 R")).collect();
        let catalog = if tree {
            CATALOG
        } else {
            "<< /Type /Catalog /Pages 4 0 R >>"
        };
        let objects = [
            catalog.to_string(),
            format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 2 0 R /Pad [{}] >>",
                " 0".repeat(n)
            ),
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = vec![];
        for (k, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{object}\nendobj\n", k + 1).bytes());
        }
        let page = offsets[2];
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", n + 5).bytes());
        for offset in offsets
            .iter()
            .copied()
            .chain([0])
            .chain(core::iter::repeat_n(page, n))
        {
            let kind = if offset == 0 { "00000 f" } else { "00000 n" };
            out.extend(format!("{offset:010} {kind} \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                n + 5
            )
            .bytes(),
        );
        out
    }

    // So too where the page tree lists no page, and every page object is a page.
    #[test]
    fn entries_leading_to_one_object_without_a_page_tree_parse_it_once() {
        let [small, large] = [250, 500].map(|n| {
            let file = aliased_page(n, false);
            let pdf = Pdf::new(file.clone()).unwrap();
            // The entries, and the table's entry of the page's own number.
            assert_eq!(pdf.pages().len(), n + 1);
            parsed(file, 0).1
        });
        assert!(
            large <= small * 22 / 10,
            "{small} bytes parsed for 250, {large} for 500"
        );
    }

    // Entries of a damaged table that lead to one object are each a page, but the
    // object is parsed once: loading the pages parses bytes linear in the file.
    #[test]
    fn entries_leading_to_one_object_parse_it_once() {
        let [small, large] = [250, 500].map(|n| {
            let file = aliased_page(n, true);
            let pdf = Pdf::new(file.clone()).unwrap();
            assert_eq!(pdf.pages().len(), n);
            assert!(pdf.pages().iter().all(|page| page.media_box().x1 == 100.0));
            parsed(file, 0).1
        });
        assert!(
            large <= small * 22 / 10,
            "{small} bytes parsed for 250, {large} for 500"
        );
    }

    // Pages that share their resources, through a node or as one object, directly or
    // through an object of their own, share each category, parsed once: a file with
    // `n` pages and `n` fonts that every page has is parsed in time linear in it.
    #[test]
    fn resources_pages_share_are_parsed_once() {
        let fonts = |n: usize| {
            let names: Vec<String> = (0..n).map(|i| format!("/F{i} {i}")).collect();
            format!("<< {} >>", names.join(" "))
        };
        let file = |n: usize, variant: &str| {
            let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", i + 5)).collect();
            let (node, own, third, fourth) = match variant {
                "inherited, direct /Font" => (
                    "/Resources 3 0 R".to_owned(),
                    "",
                    format!("<< /Font {} >>", fonts(n)),
                    "null".to_owned(),
                ),
                "inherited, indirect /Font" => (
                    "/Resources 3 0 R".to_owned(),
                    "",
                    "<< /Font 4 0 R >>".to_owned(),
                    fonts(n),
                ),
                "inherited, written into the node" => (
                    format!("/Resources << /Font {} >>", fonts(n)),
                    "",
                    "null".to_owned(),
                    "null".to_owned(),
                ),
                "own, one object" => (
                    String::new(),
                    "/Resources 3 0 R",
                    format!("<< /Font {} >>", fonts(n)),
                    "null".to_owned(),
                ),
                "own, written into each page, indirect /Font" => (
                    String::new(),
                    "/Resources << /Font 4 0 R >>",
                    "null".to_owned(),
                    fonts(n),
                ),
                _ => unreachable!(),
            };
            let tree = format!(
                "<< /Type /Pages /Count {n} /Kids [{}] {node} >>",
                kids.join(" ")
            );
            let page = format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] {own} >>");
            let mut objects = vec![CATALOG, &tree, &third, &fourth];
            objects.extend(core::iter::repeat_n(page.as_str(), n));
            pdf(&objects)
        };
        for variant in [
            "inherited, direct /Font",
            "inherited, indirect /Font",
            "inherited, written into the node",
            "own, one object",
            "own, written into each page, indirect /Font",
        ] {
            let [small, large] = [256, 512].map(|n| parsed(file(n, variant), n));
            assert!(
                large.0 < small.0 * 21 / 10 && large.1 < small.1 * 21 / 10,
                "{variant}: (entries, bytes) {small:?} for 256 pages, {large:?} for 512"
            );
        }
    }

    // A damaged file whose `n` pages are in one object stream, which the scan that
    // rebuilds its table reads, uncompressed, past each page: the stream is read
    // once, and no page is read as a stream, which searches the rest of the file.
    #[test]
    fn pages_in_one_object_stream_load_in_linear_time() {
        let work = |n: usize| {
            let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", i + 3)).collect();
            let (mut header, mut data) = (String::new(), String::new());
            for i in 0..n {
                header += &format!("{} {} ", i + 3, data.len());
                data += "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>\n";
            }
            let stream = format!(
                "<< /Type /ObjStm /N {n} /First {} /Length {} >>\nstream\n{header}{data}\nendstream",
                header.len(),
                header.len() + data.len()
            );
            let tree = format!("<< /Type /Pages /Count {n} /Kids [{}] >>", kids.join(" "));
            let file = recovered(&[(1, 0, CATALOG), (2, 0, &tree), (n as i32 + 3, 0, &stream)]);
            crate::work::OBJECT_STREAM_ENTRIES.set(0);
            crate::object::stream::SEARCHED.set(0);
            let pdf = Pdf::new(file).unwrap();
            assert_eq!(pdf.pages().len(), n);
            assert!(pdf.pages().iter().all(|page| page.media_box().x1 == 200.0));
            (
                crate::work::OBJECT_STREAM_ENTRIES.get(),
                crate::object::stream::SEARCHED.get(),
            )
        };
        let [small, large] = [500, 1000].map(work);
        assert!(
            large.0 < small.0 * 21 / 10 && large.1 < small.1 * 21 / 10 + 100,
            "(entries, bytes searched) {small:?} for 500 pages, {large:?} for 1000"
        );
    }

    // A damaged object stream whose header lists `n` numbers at one offset holds one
    // object, which the entry of each number is, each in its own context: the `n`
    // pages they are parse it once, so loading them parses bytes linear in the file.
    #[test]
    fn entries_at_one_offset_of_an_object_stream_parse_it_once() {
        let parsed = |n: usize| {
            let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", i + 3)).collect();
            let header: String = (0..n).map(|i| format!("{} 0 ", i + 3)).collect();
            let keys: String = (0..n).map(|i| format!("/K{i} 0 ")).collect();
            let page = format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] {keys}>>");
            let stream = format!(
                "<< /Type /ObjStm /N {n} /First {} /Length {} >>\nstream\n{header}{page}\nendstream",
                header.len(),
                header.len() + page.len()
            );
            let tree = format!("<< /Type /Pages /Count {n} /Kids [{}] >>", kids.join(" "));
            let file = recovered(&[(1, 0, CATALOG), (2, 0, &tree), (n as i32 + 3, 0, &stream)]);
            let pdf = Pdf::new(file.clone()).unwrap();
            let pages = pdf.pages();
            assert_eq!(pages.len(), n);
            for (i, page) in pages.iter().enumerate() {
                assert_eq!(page.media_box().x1, 200.0);
                assert_eq!(page.obj_id(), Some(ObjectIdentifier::new(i as i32 + 3, 0)));
            }
            parsed(file, 0)
        };
        let [small, large] = [250, 500].map(parsed);
        assert!(
            large.0 < small.0 * 22 / 10 && large.1 < small.1 * 22 / 10,
            "(entries, bytes) parsed {small:?} for 250 entries, {large:?} for 500"
        );
    }

    /// A file whose body holds `objects` in order, each a number and a body, a number
    /// possibly twice, and whose table lists each number at its first definition; at
    /// `objects[at]` for `(number, at)` in `misplaced`; and not at all for a number
    /// in `unlisted`.
    fn damaged(objects: &[(i32, &str)], misplaced: &[(i32, usize)], unlisted: &[i32]) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut first = std::collections::BTreeMap::new();
        let mut offsets = vec![];
        for (number, body) in objects {
            first.entry(*number).or_insert(out.len());
            offsets.push(out.len());
            out.extend(format!("{number} 0 obj\n{body}\nendobj\n").bytes());
        }
        let listed = misplaced.iter().map(|(number, _)| number);
        let size = first.keys().chain(listed).max().unwrap() + 1;
        let xref = out.len();
        out.extend(format!("xref\n0 {size}\n0000000000 65535 f \n").bytes());
        for number in 1..size {
            let offset = match misplaced.iter().find(|(n, _)| *n == number) {
                Some((_, at)) => Some(offsets[*at]),
                None => first.get(&number).copied(),
            };
            match offset.filter(|_| !unlisted.contains(&number)) {
                Some(offset) => out.extend(format!("{offset:010} 00000 n \n").bytes()),
                None => out.extend(b"0000000000 65535 f \n"),
            }
        }
        out.extend(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").bytes(),
        );
        out
    }

    // A table with an entry that does not lead to its object is rebuilt from the file
    // before any page is read, so every page reads the rebuilt table, which takes
    // the later of two definitions: here both pages inherit node 3's second box,
    // though the entry that is wrong is only met on the second page.
    #[test]
    fn a_damaged_table_is_rebuilt_before_any_page_inherits_from_it() {
        let file = damaged(
            &[
                (1, CATALOG),
                (2, "<< /Type /Pages /Kids [4 0 R 5 0 R] /Count 2 >>"),
                (3, "<< /Type /Pages /MediaBox [0 0 100 100] >>"),
                (4, "<< /Type /Page /Parent 3 0 R >>"),
                (5, "<< /Type /Page /Parent 3 0 R /Rotate 6 0 R >>"),
                (6, "90"),
                (3, "<< /Type /Pages /MediaBox [0 0 200 200] >>"),
            ],
            &[(6, 4)],
            &[],
        );
        assert_eq!(inherited_in(file), [(vec![], 200.0, 0), (vec![], 200.0, 1)]);
    }

    // The root's /Resources, which the table does not list, is found in the rebuilt
    // table by every page that inherits it.
    #[test]
    fn an_object_only_the_rebuilt_table_lists_is_inherited_by_every_page() {
        let file = damaged(
            &[
                (1, CATALOG),
                (
                    2,
                    "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /Resources 7 0 R \
                     /MediaBox [0 0 300 300] >>",
                ),
                (3, "<< /Type /Page /Parent 2 0 R >>"),
                (4, "<< /Type /Page /Parent 2 0 R /Rotate 8 0 R >>"),
                (7, "<< /XObject << /A 9 0 R >> >>"),
                (8, "90"),
            ],
            &[(8, 2)],
            &[7],
        );
        assert_eq!(
            inherited_in(file),
            [(named(&["A"]), 300.0, 0), (named(&["A"]), 300.0, 1)]
        );
    }

    // Where the table names one definition of the root and the file ends with
    // another, the pages are those of the later one, and inherit from it alone.
    #[test]
    fn a_root_defined_twice_is_read_from_the_rebuilt_table_alone() {
        let file = damaged(
            &[
                (1, CATALOG),
                (
                    2,
                    "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 \
                     /Resources << /XObject << /A 9 0 R >> >> >>",
                ),
                (3, "<< /Type /Page /Rotate 4 0 R /Resources << >> >>"),
                (4, "90"),
                (5, "<< /Type /Page /Parent 2 0 R >>"),
                (2, "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>"),
            ],
            &[(4, 2)],
            &[],
        );
        assert_eq!(inherited_in(file), [(vec![], A4.x1, 1), (vec![], A4.x1, 0)]);
    }

    // Where an entry leads to an object whose header names another object, the table
    // is rebuilt and keeps the entry, which the scan does not find under its number:
    // the object is the entry's, /Rotate 90 here, and the scan does not take it for
    // the later definition of the catalog it is headed as.
    #[test]
    fn an_object_numbered_as_another_the_table_lists_is_the_entry_s() {
        let file = damaged(
            &[
                (1, CATALOG),
                (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                (3, "<< /Type /Page /MediaBox [0 0 300 300] /Rotate 4 0 R >>"),
                (1, "90"),
            ],
            &[(4, 3)],
            &[],
        );
        assert_eq!(inherited_in(file), [(vec![], 300.0, 1)]);
    }

    // Where two entries lead to one object, which the table lists under its header's
    // number too, both are that object: the entry headed otherwise does not take it
    // from the number the header names.
    #[test]
    fn an_object_two_entries_lead_to_is_both_entries() {
        let file = damaged(
            &[
                (1, CATALOG),
                (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                (3, "<< /Type /Page /MediaBox 4 0 R /CropBox 5 0 R >>"),
                (4, "[0 0 300 300]"),
            ],
            &[(5, 3)],
            &[],
        );
        let pdf = Pdf::new(file).unwrap();
        let page = &pdf.pages()[0];
        assert_eq!((page.media_box().x1, page.crop_box().x1), (300.0, 300.0));
    }

    const DRAWING: &[u8] = b"0 0 1 rg 10 10 80 80 re f";

    fn drawing() -> String {
        let data = core::str::from_utf8(DRAWING).unwrap();
        format!("<< /Length {} >>\nstream\n{data}\nendstream", DRAWING.len())
    }

    /// Each page of `file`: its object number, its media box and its content.
    fn drawn(file: Vec<u8>) -> Vec<(i32, [f64; 4], Vec<u8>)> {
        let pdf = Pdf::new(file).unwrap();
        pdf.pages()
            .iter()
            .map(|page| {
                let number = page.obj_id().map_or(0, |id| id.obj_number);
                let m = page.media_box();
                (
                    number,
                    [m.x0, m.y0, m.x1, m.y1],
                    page.try_page_stream().unwrap(),
                )
            })
            .collect()
    }

    // An object is the one its entry names, not the one its header names: node 2 and
    // its /Type, object 3, are both headed `1 0 obj`, and reading /Type from node 2
    // is no cycle, so node 2 is a node and page 4 is the page.
    #[test]
    fn an_entry_read_from_a_node_headed_as_another_object_is_no_cycle() {
        let file = damaged(
            &[
                (1, CATALOG),
                (
                    1,
                    "<< /Type 3 0 R /Kids [4 0 R] /Count 1 /MediaBox [0 0 300 300] >>",
                ),
                (1, "/Pages"),
                (
                    4,
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 5 0 R >>",
                ),
                (5, &drawing()),
            ],
            &[(2, 1), (3, 2)],
            &[],
        );
        assert_eq!(
            drawn(file),
            [(4, [0.0, 0.0, 100.0, 100.0], DRAWING.to_vec())]
        );
    }

    // So too for /Kids: node 2's /Kids, object 3, lists page 5 before page 4, and both
    // are headed `1 0 obj`.
    #[test]
    fn kids_read_from_a_node_headed_as_another_object_keep_their_order() {
        let page = |size: &str| {
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {size}] /Contents 6 0 R >>")
        };
        let file = damaged(
            &[
                (1, CATALOG),
                (1, "<< /Type /Pages /Kids 3 0 R /Count 2 >>"),
                (1, "[5 0 R 4 0 R]"),
                (4, &page("100 200")),
                (5, &page("300 400")),
                (6, &drawing()),
            ],
            &[(2, 1), (3, 2)],
            &[],
        );
        assert_eq!(
            drawn(file),
            [
                (5, [0.0, 0.0, 300.0, 400.0], DRAWING.to_vec()),
                (4, [0.0, 0.0, 100.0, 200.0], DRAWING.to_vec()),
            ]
        );
    }

    // Where an entry leads to another object's second definition and the file holds
    // the entry's own object, the rebuilt table has that object: page 3's box is
    // object 6, 600 by 700, not the second object 7 that entry 6 leads to.
    #[test]
    fn an_object_the_scan_finds_is_read_rather_than_the_one_its_entry_leads_to() {
        let file = damaged(
            &[
                (1, CATALOG),
                (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                (
                    3,
                    "<< /Type /Page /Parent 2 0 R /MediaBox 6 0 R /Contents 8 0 R >>",
                ),
                (7, "[0 0 200 300]"),
                (7, "[0 0 400 500]"),
                (6, "[0 0 600 700]"),
                (8, &drawing()),
            ],
            &[(6, 4)],
            &[],
        );
        assert_eq!(
            drawn(file),
            [(3, [0.0, 0.0, 600.0, 700.0], DRAWING.to_vec())]
        );
    }

    // Where a damaged file holds two generations of an object, the loader keeps
    // both, and so does the page tree: each generation of node 3 lists its own page,
    // which inherits its own node's box and resources, through its /Parent, through
    // the node listing it, or, where its /Parent is itself, through the repaired
    // /Parent entries.
    #[test]
    fn two_generations_of_a_node_are_two_nodes() {
        let node = |page: i32, size: i32, name: &str| {
            format!(
                "<< /Type /Pages /Kids [{page} 0 R] /MediaBox [0 0 {size} {size}] \
                 /Resources << /XObject << /{name} 9 0 R >> >> >>"
            )
        };
        let (first, second) = (node(4, 300, "A"), node(5, 200, "B"));
        for parents in [["3 0 R", "3 1 R"], ["", ""], ["4 0 R", "5 0 R"]] {
            let page = |parent: &str| match parent {
                "" => "<< /Type /Page >>".to_owned(),
                parent => format!("<< /Type /Page /Parent {parent} >>"),
            };
            let (page_4, page_5) = (page(parents[0]), page(parents[1]));
            let file = recovered(&[
                (1, 0, CATALOG),
                (2, 0, "<< /Type /Pages /Kids [3 0 R 3 1 R] >>"),
                (3, 0, &first),
                (3, 1, &second),
                (4, 0, &page_4),
                (5, 0, &page_5),
            ]);
            assert_eq!(
                inherited_in(file),
                [(named(&["A"]), 300.0, 0), (named(&["B"]), 200.0, 0)],
                "{parents:?}"
            );
        }
    }

    // Two generations of a page are two pages, each with its own box and resources.
    #[test]
    fn two_generations_of_a_page_are_two_pages() {
        let page = |size: i32, name: &str| {
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {size} {size}] \
                 /Resources << /XObject << /{name} 9 0 R >> >> >>"
            )
        };
        let (first, second) = (page(300, "A"), page(200, "B"));
        let file = recovered(&[
            (1, 0, CATALOG),
            (2, 0, "<< /Type /Pages /Kids [3 0 R 3 1 R] >>"),
            (3, 0, &first),
            (3, 1, &second),
        ]);
        let pdf = Pdf::new(file.clone()).unwrap();
        let ids: Vec<_> = pdf.pages().iter().map(|page| page.obj_id()).collect();
        assert_eq!(
            ids,
            [
                Some(ObjectIdentifier::new(3, 0)),
                Some(ObjectIdentifier::new(3, 1))
            ]
        );
        assert_eq!(
            inherited_in(file),
            [(named(&["A"]), 300.0, 0), (named(&["B"]), 200.0, 0)]
        );
    }

    #[test]
    fn a_tree_without_pages_has_none() {
        let tree = pages_node("1", "");
        assert_eq!(pages(&[CATALOG, &tree]), Some(vec![]));
        assert_eq!(pages(&["<< /Type /Catalog >>", PAGE]), Some(vec![]));
    }

    #[test]
    fn without_a_page_tree_every_page_object_is_a_page() {
        let page = "<< /Type /Page /MediaBox [0 0 300 300] /Contents 4 0 R >>";
        let untyped = "<< /MediaBox [0 0 400 400] /Contents 4 0 R >>";
        let note = "<< /Type /Annot /Subtype /Text /Contents (not a page) >>";
        let content = "<< /Length 0 >>\nstream\n\nendstream";
        for catalog in ["<< /Type /Catalog >>", CATALOG] {
            let tree = pages_node("2", "");
            assert_eq!(
                pages(&[catalog, &tree, page, content, untyped, note]),
                Some(vec![(3, Some(300.0)), (5, Some(400.0))]),
                "{catalog}"
            );
        }
    }

    mod content {
        use crate::crypto::fixture::{Part, aes128};
        use crate::object::ObjectIdentifier;
        use crate::object::stream::DecodeFailure;
        use crate::page::ContentError;
        use crate::pdf::Pdf;
        use alloc::vec::Vec;

        const RED: &[u8] = b"1 0 0 rg 0 0 10 10 re f";
        const BLUE: &[u8] = b"0 0 1 rg 10 0 10 10 re f";
        /// zlib of "0 1 0 rg 20 0 10 10 re f".
        const GREEN_FLATE: &[u8] = &[
            0x78, 0x9c, 0x33, 0x50, 0x30, 0x54, 0x30, 0x50, 0x28, 0x4a, 0x57, 0x30, 0x32, 0x00,
            0xd2, 0x86, 0x60, 0x54, 0x94, 0xaa, 0x90, 0x06, 0x00, 0x3b, 0x65, 0x05, 0x1c,
        ];

        const CATALOG: Part<'_> = Part::Plain("<< /Type /Catalog /Pages 2 0 R >>");
        const TREE: Part<'_> = Part::Plain("<< /Type /Pages /Kids [3 0 R] /Count 1 >>");

        fn page(contents: &str) -> String {
            format!("<< /Type /Page /MediaBox [0 0 100 100] /Contents {contents} >>")
        }

        /// The page's content read strictly and leniently.
        fn read(file: Vec<u8>) -> (Result<Vec<u8>, ContentError>, Vec<u8>) {
            let pdf = Pdf::new(file).unwrap();
            let page = &pdf.pages()[0];
            let lenient = page.page_stream().unwrap_or_default().to_vec();
            (page.try_page_stream(), lenient)
        }

        fn joined(parts: &[&[u8]]) -> Vec<u8> {
            parts
                .iter()
                .flat_map(|p| p.iter().chain(b" "))
                .copied()
                .collect()
        }

        #[test]
        fn an_identity_crypt_filter_leaves_its_stream_unencrypted() {
            for dict in [
                "/Filter /Crypt /DecodeParms << /Name /Identity >>",
                "/Filter /Crypt",
                "/Filter [/Crypt] /DecodeParms [<< /Name /Identity >>]",
            ] {
                let page = page("[4 0 R 5 0 R]");
                let file = aes128(&[
                    CATALOG,
                    TREE,
                    Part::Plain(&page),
                    Part::Stream {
                        dict,
                        data: RED,
                        encrypt: false,
                    },
                    Part::Stream {
                        dict: "",
                        data: BLUE,
                        encrypt: true,
                    },
                ]);
                let (strict, lenient) = read(file);
                assert_eq!(strict.unwrap(), joined(&[RED, BLUE]), "{dict}");
                assert_eq!(lenient, joined(&[RED, BLUE]), "{dict}");
            }
        }

        #[test]
        fn an_identity_crypt_filter_goes_before_the_others() {
            let page = page("4 0 R");
            let file = aes128(&[
                CATALOG,
                TREE,
                Part::Plain(&page),
                Part::Stream {
                    dict: "/Filter [/Crypt /FlateDecode] /DecodeParms [<< /Name /Identity >> null]",
                    data: GREEN_FLATE,
                    encrypt: false,
                },
            ]);
            assert_eq!(read(file).0.unwrap(), b"0 1 0 rg 20 0 10 10 re f");
        }

        // A crypt filter naming one of the document's decrypts with it, as the
        // document's stream filter does (MuPDF's `pdf_open_crypt_with_filter`).
        #[test]
        fn a_named_crypt_filter_decrypts_its_stream() {
            let page = page("[4 0 R 5 0 R]");
            let file = aes128(&[
                CATALOG,
                TREE,
                Part::Plain(&page),
                Part::Stream {
                    dict: "/Filter /Crypt /DecodeParms << /Name /StdCF >>",
                    data: RED,
                    encrypt: true,
                },
                Part::Stream {
                    dict: "/Filter [/Crypt /FlateDecode] /DecodeParms [<< /Name /StdCF >> null]",
                    data: GREEN_FLATE,
                    encrypt: true,
                },
            ]);
            let (strict, lenient) = read(file.clone());
            let both = joined(&[RED, b"0 1 0 rg 20 0 10 10 re f"]);
            assert_eq!(strict.unwrap(), both);
            assert_eq!(lenient, both);
            // The raw data a writer copies without the /Crypt filter are decrypted.
            let pdf = Pdf::new(file).unwrap();
            let raw = |n| {
                let stream = pdf
                    .xref()
                    .get::<crate::object::Stream<'_>>(ObjectIdentifier::new(n, 0))
                    .unwrap();
                stream.try_raw_data().unwrap().to_vec()
            };
            assert_eq!(raw(4), RED);
            assert_eq!(raw(5), GREEN_FLATE);
        }

        #[test]
        fn a_part_that_cannot_be_read_is_an_error() {
            let id = |n| Some(ObjectIdentifier::new(n, 0));
            let page = page("[4 0 R 5 0 R]");
            let file = |part: Part<'_>| {
                aes128(&[
                    CATALOG,
                    TREE,
                    Part::Plain(&page),
                    Part::Stream {
                        dict: "",
                        data: RED,
                        encrypt: true,
                    },
                    part,
                ])
            };
            // Data that is no Flate stream.
            let (strict, lenient) = read(file(Part::Stream {
                dict: "/Filter /FlateDecode",
                data: b"not flate at all, not flate at all",
                encrypt: true,
            }));
            assert!(
                matches!(strict, Err(ContentError::Decode(i, DecodeFailure::StreamDecode)) if i == id(5))
            );
            // A lenient reading recovers nothing of it.
            assert_eq!(lenient, joined(&[RED, b""]));
            // A crypt filter the document does not define, and one after another
            // filter, which is not supported.
            for dict in [
                "/Filter /Crypt /DecodeParms << /Name /Other >>",
                "/Filter [/FlateDecode /Crypt] /DecodeParms [null << /Name /StdCF >>]",
            ] {
                let (strict, lenient) = read(file(Part::Stream {
                    dict,
                    data: BLUE,
                    encrypt: true,
                }));
                assert!(
                    matches!(strict, Err(ContentError::Decode(i, DecodeFailure::Decryption)) if i == id(5)),
                    "{dict}"
                );
                assert_eq!(lenient, joined(&[RED]), "{dict}");
            }
            // Data that does not decrypt: AES needs whole blocks.
            let (strict, _) = read(file(Part::Stream {
                dict: "",
                data: BLUE,
                encrypt: false,
            }));
            assert!(
                matches!(strict, Err(ContentError::Decode(i, DecodeFailure::Decryption)) if i == id(5))
            );
            // An object that is no stream.
            let (strict, _) = read(file(Part::Plain("<< /Length 3 >>")));
            assert!(matches!(strict, Err(ContentError::Unreadable(i)) if i == id(5)));
        }

        #[test]
        fn no_contents_is_an_empty_page() {
            // A missing /Contents, a null one, a null part and a part the file lacks.
            for contents in ["null", "[null 9 0 R]", "9 0 R"] {
                let page = page(contents);
                let file = aes128(&[CATALOG, TREE, Part::Plain(&page)]);
                assert_eq!(read(file).0.unwrap(), b"", "{contents}");
            }
            let bare = "<< /Type /Page /MediaBox [0 0 100 100] >>";
            let file = aes128(&[CATALOG, TREE, Part::Plain(bare)]);
            assert_eq!(read(file).0.unwrap(), b"");
            // An empty stream, which has no IV to decrypt with.
            let page = page("4 0 R");
            let empty = Part::Stream {
                dict: "",
                data: b"",
                encrypt: false,
            };
            let file = aes128(&[CATALOG, TREE, Part::Plain(&page), empty]);
            assert_eq!(read(file).0.unwrap(), b"");
        }
    }

    #[test]
    fn a_table_entry_in_a_missing_object_stream_rebuilds_the_table() {
        // Object 2, the page tree, is listed in object stream 10, which the table lacks.
        let data = std::fs::read("../hayro-tests/pdfs/custom/lopdf_issue_449_2.pdf").unwrap();
        let pdf = Pdf::new(data).unwrap();
        let ids: Vec<_> = pdf.pages().iter().map(|p| p.obj_id()).collect();
        assert_eq!(ids, [Some(ObjectIdentifier::new(3, 0))]);
    }

    #[test]
    fn a_page_tree_written_into_the_catalog_is_read() {
        let catalog = "<< /Type /Catalog /Pages << /Type /Pages /Kids [3 0 R] /Count 1 >> >>";
        assert_eq!(
            pages(&[catalog, "null", PAGE]),
            Some(vec![(3, Some(1600.0))])
        );
    }

    /// Each page's object number and width, what the document lost, and each page.
    type Losses = (Vec<(i32, f64)>, Option<i32>, Vec<Option<i32>>);

    /// The [`Losses`] of a file of `objects`.
    fn lost(objects: &[&str]) -> Losses {
        let pdf = Pdf::new(pdf(objects)).unwrap();
        let number = |id: Option<ObjectIdentifier>| id.map(|id| id.obj_number);
        let pages = pdf.pages();
        (
            pages
                .iter()
                .map(|page| (number(page.obj_id()).unwrap_or(0), page.media_box().x1))
                .collect(),
            number(pages.unreadable()),
            pages.iter().map(|page| number(page.unreadable())).collect(),
        )
    }

    // A /Kids array, a node or a node's /Type that the file lists but that cannot be
    // read may have held pages: the document tells it, and lists the pages that can be
    // read, as MuPDF lists them (a node that cannot be read is an empty page there).
    // A /Kids array the file does not hold loses nothing.
    #[test]
    fn a_page_tree_object_that_cannot_be_read_is_lost_to_the_document() {
        let tree = pages_node("2", "3 0 R 4 0 R");
        let node = |entries: &str| format!("<< /Type /Pages /Parent 2 0 R {entries} >>");
        let typed = "<< /Type 5 0 R /Parent 2 0 R /Kids [6 0 R] >>";
        let wide = "<< /Type /Page /MediaBox [0 0 600 500] >>";
        let a4 = A4.x1;
        for (third, fifth, pages, document) in [
            (node("/Kids 5 0 R"), "[6 0 R", vec![(4, 200.0)], Some(5)),
            (
                node("/Kids 5 0 R"),
                "[6 0 R]",
                vec![(6, 600.0), (4, 200.0)],
                None,
            ),
            (node("/Kids 9 0 R"), "null", vec![(4, 200.0)], None),
            (
                "<< /Type /Pages /Kids [6 0 R] /Broken >".to_owned(),
                "null",
                vec![(3, a4), (4, 200.0)],
                Some(3),
            ),
            (
                typed.to_owned(),
                "<< /Broken >",
                vec![(3, a4), (4, 200.0)],
                Some(5),
            ),
            (
                typed.to_owned(),
                "/Pages",
                vec![(6, 600.0), (4, 200.0)],
                None,
            ),
        ] {
            let (listed, lost, each) = lost(&[CATALOG, &tree, &third, SMALL, fifth, wide]);
            assert_eq!(
                (listed, lost),
                (pages, document),
                "{third}, object 5: {fifth}"
            );
            assert!(each.iter().all(Option::is_none), "{third}: {each:?}");
        }
    }

    // A box pages inherit whose number is an object the file lists but that cannot be
    // read is lost to each of them, though it is read once for all; one the file does
    // not hold makes no box, and loses nothing. A number pages share is resolved once
    // however many boxes hold it.
    #[test]
    fn a_box_whose_number_cannot_be_read_is_lost_to_every_page_drawn_with_it() {
        let tree = |number: &str| {
            pages_node("2", "3 0 R 4 0 R")
                .replace(" >>", &format!(" /MediaBox [0 0 {number} 700] >>"))
        };
        let page = "<< /Type /Page /Parent 2 0 R >>";
        let a4 = A4.x1;
        for (number, fifth, width, page_lost) in [
            ("5 0 R", "<< /Broken >", a4, Some(5)),
            ("5 0 R", "[1 2", a4, Some(5)),
            ("5 0 R", "600", 600.0, None),
            ("9 0 R", "null", a4, None),
        ] {
            let (listed, document, each) = lost(&[CATALOG, &tree(number), page, page, fifth]);
            assert_eq!(listed, vec![(3, width), (4, width)], "{number}: {fifth}");
            assert_eq!(document, None);
            assert_eq!(each, vec![page_lost; 2], "{number}: {fifth}");
        }
        // Each page's own box, all holding one number.
        let n = 50;
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + i)).collect();
        let mut objects = vec![
            CATALOG.to_owned(),
            pages_node(&n.to_string(), &kids.join(" ")),
            "600".to_owned(),
        ];
        objects.extend(
            (0..n).map(|_| "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 3 0 R 700] >>".to_owned()),
        );
        let objects: Vec<&str> = objects.iter().map(String::as_str).collect();
        let file = pdf(&objects);
        let mut widths = vec![];
        assert_eq!(
            resolved_again(|| {
                let pdf = Pdf::new(file).unwrap();
                widths = pdf.pages().iter().map(|p| p.media_box().x1).collect();
            }),
            []
        );
        assert_eq!(widths, vec![600.0; n]);
    }

    // A category of resources whose object is a dictionary followed by stream data
    // that cannot be read is read as the dictionary, as MuPDF reads it: its fonts
    // are there, and nothing is lost. Read as a stream, it is lost.
    #[test]
    fn a_category_with_stream_data_that_cannot_be_read_is_its_dictionary() {
        let page =
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Resources << /Font 4 0 R >> >>";
        let category = "<< /F1 5 0 R /Length 9999 >>\nstream\nBT /F1 12 Tf ET";
        let font = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";
        let file = pdf(&[CATALOG, &pages_node("1", "3 0 R"), page, category, font]);
        let pdf = Pdf::new(file).unwrap();
        let page = &pdf.pages()[0];
        assert_eq!(page.unreadable(), None);
        assert!(page.resources().fonts.contains_key(b"F1"));
        let (stream, lost) = pdf.xref().watch(|| {
            pdf.xref()
                .get::<crate::object::Stream<'_>>(ObjectIdentifier::new(4, 0))
        });
        assert!(stream.is_none());
        assert_eq!(lost, Some(ObjectIdentifier::new(4, 0)));
        let (dict, lost) = pdf
            .xref()
            .watch(|| pdf.xref().get::<Dict<'_>>(ObjectIdentifier::new(4, 0)));
        assert!(dict.is_some_and(|dict| dict.contains_key(b"F1")));
        assert_eq!(lost, None);
    }
}
