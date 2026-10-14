//! Reading the pages of a PDF document.

use crate::content::{TypedIter, UntypedIter};
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
use crate::xref::XRef;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Deref;

mod inherit;

use inherit::Listings;
pub use inherit::{Inheritable, Inheritance, InheritanceError};

/// A structure holding the pages of a PDF document.
pub struct Pages<'a> {
    pages: Vec<Page<'a>>,
    xref: &'a XRef,
}

impl<'a> Pages<'a> {
    // The pages the page tree lists, depth first; where the catalog has no page tree
    // that lists a page, every page object of the file, in object order.
    //
    // MuPDF numbers pages by the root's /Count (`pdf_count_pages` in MuPDF's
    // `pdf-page.c`) and, where the tree is odd, finds each page by the nodes' /Count
    // (`pdf_lookup_page_loc_imp`), so a damaged /Count hides pages or refuses the
    // file. The walk ignores /Count: it hides no page the /Kids hold.
    pub(crate) fn new(ctx: &ReaderContext<'a>, xref: &'a XRef) -> Option<Self> {
        let tree = xref
            .get_with::<Dict<'a>>(xref.root_id(), ctx)
            .and_then(|root| root.get_raw::<Object<'a>>(PAGES))
            .map(|tree| Node::new(tree, ctx))
            .filter(|tree| tree.is_dict);
        let (mut pages, listings) = tree.map(|tree| walk(tree, ctx)).unwrap_or_default();
        if pages.is_empty() {
            pages = brute_force(xref);
        }
        let pages = with_inherited(pages, Arc::new(listings), ctx);

        Some(Self { pages, xref })
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
}

/// A page tree entry: a dictionary, or an empty one for any other object.
#[derive(Clone)]
struct Node<'a> {
    id: Option<ObjectIdentifier>,
    dict: Dict<'a>,
    is_dict: bool,
}

impl<'a> Node<'a> {
    fn new(object: MaybeRef<Object<'a>>, ctx: &ReaderContext<'a>) -> Self {
        let (id, object) = match object {
            MaybeRef::Ref(id) => (Some(id.into()), ctx.xref().get_with(id.into(), ctx)),
            MaybeRef::NotRef(object) => (None, Some(object)),
        };
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

    // Whether MuPDF sees the entry: a direct `null` is stored as no entry.
    fn has(&self, key: &[u8]) -> bool {
        #[cfg(test)]
        LOOKUPS.set(LOOKUPS.get() + 1);
        !matches!(
            self.dict.get_raw::<Object<'a>>(key),
            None | Some(MaybeRef::NotRef(Object::Null(_)))
        )
    }

    // Whether the entry lists kids rather than being a page: /Type /Pages, or, as
    // MuPDF reads a node without /Type (`pdf_lookup_page_loc_imp`), /Kids and no
    // /MediaBox. Any other entry is a page, including one that is no dictionary.
    fn is_tree(&self) -> bool {
        if self.has(TYPE) {
            self.dict.get::<Name<'_>>(TYPE).as_deref() == Some(PAGES)
        } else {
            self.has(KIDS) && !self.has(MEDIA_BOX)
        }
    }

    // A page's own entry, which is a dictionary.
    fn page(id: Option<ObjectIdentifier>, dict: &Dict<'a>) -> Self {
        Self {
            id,
            dict: dict.clone(),
            is_dict: true,
        }
    }

    fn kids(&self) -> ArrayIter<'a> {
        self.dict
            .get::<Array<'a>>(KIDS)
            .unwrap_or_default()
            .raw_iter()
    }
}

/// A page tree entry and the index in the [`Listings`] of the node listing it.
type Listed<'a> = (Node<'a>, Option<usize>);

// The pages below `tree`, depth first, and the nodes listing them. A node's kids are
// read the first time the walk reaches it: a node listed again, as a shared subtree,
// in a cycle or in a /Kids array that two nodes share, adds nothing, which keeps the
// walk linear in the file. A page listed again is a page again, as in MuPDF.
fn walk<'a>(tree: Node<'a>, ctx: &ReaderContext<'a>) -> (Vec<Listed<'a>>, Listings<'a>) {
    let mut pages = vec![];
    let mut listings = Listings::default();
    let mut stack: Vec<(usize, ArrayIter<'a>)> = vec![];
    let mut next = Some(tree);
    loop {
        if let Some(node) = next.take() {
            let lister = stack.last().map(|(at, _)| *at);
            if !node.is_tree() {
                pages.push((node, lister));
            } else if let Some(at) = listings.read(&node, lister) {
                stack.push((at, node.kids()));
            }
        }
        let Some((at, kids)) = stack.last_mut() else {
            return (pages, listings);
        };
        match kids.next() {
            Some(kid) => next = Some(Node::new(kid, ctx)),
            None => {
                listings.close(*at);
                stack.pop();
            }
        }
    }
}

// Every dictionary of the file that has /Contents and, if typed, is a /Page. Better
// than nothing where the page tree is lost, though the order is the objects', which
// may not be the document's.
fn brute_force<'a>(xref: &'a XRef) -> Vec<Listed<'a>> {
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

/// The entries a page inherits, in the order of [`Inheritable::ALL`].
type Inherited<'a> = [Option<Object<'a>>; 4];

// Each of `entries` as a page with the entries it inherits, found as MuPDF finds
// them once it has loaded every page, which reads these entries: by then a /Parent
// loop any of the lookups meets has been repaired (see `Inheritance`). An entry
// that cannot be found, because a /Parent chain loops even so, is not inherited.
fn with_inherited<'a>(
    entries: Vec<Listed<'a>>,
    listings: Arc<Listings<'a>>,
    ctx: &ReaderContext<'a>,
) -> Vec<Page<'a>> {
    let mut tree = Inheritance::with(ctx.clone());
    let look_up = |tree: &mut Inheritance<'a>| -> Vec<Inherited<'a>> {
        entries
            .iter()
            .map(|(page, lister)| {
                Inheritable::ALL
                    .map(|key| tree.lookup(page, &listings, *lister, key).ok().flatten())
            })
            .collect()
    };
    let mut inherited = look_up(&mut tree);
    if tree.is_repaired() {
        inherited = look_up(&mut tree);
    }
    entries
        .into_iter()
        .zip(inherited)
        .map(|((page, lister), inherited)| {
            Page::new(page, listings.clone(), lister, inherited, ctx)
        })
        .collect()
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
    page_streams: OnceLock<Option<Vec<u8>>>,
    resources: Resources<'a>,
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
        inherited: Inherited<'a>,
        reader_ctx: &ReaderContext<'a>,
    ) -> Self {
        let [media_box, crop_box, rotate, resources] = inherited;
        let rect = |entry: Option<Object<'a>>| entry.and_then(|e| Rect::try_from(e).ok());
        let media_box = rect(media_box).unwrap_or(A4);
        let crop_box = rect(crop_box).unwrap_or(media_box);

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

        let resources = Resources::new(
            resources
                .and_then(Object::into_dict)
                .unwrap_or_else(Dict::empty),
        );

        Self {
            id: node.id,
            inner: node.dict,
            media_box,
            crop_box,
            rotation,
            page_streams: OnceLock::new(),
            resources,
            listings,
            lister,
            ctx: reader_ctx.clone(),
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

    /// Return the decoded content stream of the page.
    pub fn page_stream(&self) -> Option<&[u8]> {
        let convert_single = |s: Stream<'_>| {
            let data = s.decoded().ok()?;
            Some(data.into_owned())
        };

        self.page_streams
            .get_or_init(|| {
                if let Some(stream) = self.inner.get::<Stream<'_>>(CONTENTS) {
                    convert_single(stream)
                } else if let Some(array) = self.inner.get::<Array<'_>>(CONTENTS) {
                    let streams = array.iter::<Stream<'_>>().flat_map(convert_single);

                    let mut collected = vec![];

                    for stream in streams {
                        collected.extend(stream);
                        // Streams must have at least one whitespace in-between.
                        collected.push(b' ');
                    }

                    Some(collected)
                } else {
                    warn!("contents entry of page was neither stream nor array of streams");

                    None
                }
            })
            .as_ref()
            .map(|d| d.as_slice())
    }

    /// The decoded content of the page, as [`Self::page_stream`] joins it, or the
    /// first part that cannot be read: unlike `page_stream`, which leaves such a part
    /// out, so that a page that lost content is not taken for a whole one. A missing
    /// or `null` /Contents, or a part that is `null`, is no content.
    pub fn try_page_stream(&self) -> Result<Vec<u8>, ContentError> {
        let Some(contents) = self.inner.get_raw::<Object<'a>>(CONTENTS) else {
            return Ok(vec![]);
        };
        let resolved = |object: MaybeRef<Object<'a>>| -> Result<_, ContentError> {
            match object {
                MaybeRef::NotRef(object) => Ok((None, object)),
                MaybeRef::Ref(r) => {
                    let id = ObjectIdentifier::from(r);
                    match self.ctx.xref().get_with::<Object<'a>>(id, &self.ctx) {
                        Some(object) => Ok((Some(id), object)),
                        None if self.ctx.xref().has_entry(id) => {
                            Err(ContentError::Unreadable(Some(id)))
                        }
                        None => Ok((Some(id), Object::Null(Null))),
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
                let mut collected = vec![];
                for item in array.raw_iter() {
                    if let Some(data) = decode(resolved(item)?)? {
                        collected.extend(data);
                        // Streams must have at least one whitespace in-between.
                        collected.push(b' ');
                    }
                }
                Ok(collected)
            }
            single => Ok(decode(single)?.unwrap_or_default()),
        }
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
        let ext_g_states = resources.get::<Dict<'_>>(EXT_G_STATE).unwrap_or_default();
        let fonts = resources.get::<Dict<'_>>(FONT).unwrap_or_default();
        let color_spaces = resources.get::<Dict<'_>>(COLORSPACE).unwrap_or_default();
        let x_objects = resources.get::<Dict<'_>>(XOBJECT).unwrap_or_default();
        let patterns = resources.get::<Dict<'_>>(PATTERN).unwrap_or_default();
        let shadings = resources.get::<Dict<'_>>(SHADING).unwrap_or_default();
        let properties = resources.get::<Dict<'_>>(PROPERTIES).unwrap_or_default();

        Self {
            ext_g_states,
            fonts,
            color_spaces,
            properties,
            x_objects,
            patterns,
            shadings,
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
    use super::A4;
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
        let pdf = Pdf::new(pdf(objects)).unwrap();
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
}
