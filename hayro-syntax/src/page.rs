//! Reading the pages of a PDF document.

use crate::content::{TypedIter, UntypedIter};
use crate::object::Array;
use crate::object::Dict;
use crate::object::Name;
use crate::object::Rect;
use crate::object::Stream;
use crate::object::array::ArrayIter;
use crate::object::dict::keys::*;
use crate::object::{MaybeRef, Object, ObjectIdentifier};
use crate::reader::ReaderContext;
use crate::sync::{FxHashMap, OnceLock};
use crate::transform::Transform;
use crate::util::FloatExt;
use crate::xref::XRef;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Deref;

/// Attributes that can be inherited.
#[derive(Debug, Clone)]
struct PagesContext<'a> {
    media_box: Option<Rect>,
    crop_box: Option<Rect>,
    rotate: Option<i32>,
    resources: Dict<'a>,
}

impl<'a> PagesContext<'a> {
    fn new() -> Self {
        Self {
            media_box: None,
            crop_box: None,
            rotate: None,
            resources: Dict::empty(),
        }
    }

    /// The attributes a page below `node` inherits.
    fn with(mut self, node: &Dict<'a>) -> Self {
        self.media_box = node.get::<Rect>(MEDIA_BOX).or(self.media_box);
        self.crop_box = node.get::<Rect>(CROP_BOX).or(self.crop_box);
        self.rotate = node.get::<i32>(ROTATE).or(self.rotate);
        if let Some(resources) = node.get::<Dict<'a>>(RESOURCES) {
            self.resources = resources;
        }
        self
    }
}

/// A structure holding the pages of a PDF document.
pub struct Pages<'a> {
    pages: Vec<Page<'a>>,
    xref: &'a XRef,
}

impl<'a> Pages<'a> {
    // The pages in the order MuPDF numbers them, or `None` where MuPDF fails to count
    // them or to find one of them.
    //
    // MuPDF takes the page count from the root's /Count (`pdf_count_pages` in
    // MuPDF's `pdf-page.c`). It walks the tree depth first; a tree with fewer pages
    // than that sets the count (`pdf_load_page_tree_internal`). Where the walk meets
    // a node that is neither /Pages nor /Page, a cycle, or more pages than the count,
    // MuPDF finds each page by descending with the nodes' /Count instead
    // (`pdf_lookup_page_loc_imp`).
    pub(crate) fn new(ctx: &ReaderContext<'a>, xref: &'a XRef) -> Option<Self> {
        let tree = xref
            .get_with::<Dict<'a>>(xref.root_id(), ctx)
            .and_then(|root| root.get_raw::<Object<'a>>(PAGES))
            .map(|tree| Node::new(tree, ctx));
        let count = tree
            .as_ref()
            .map_or(0, |tree| to_int(tree.dict.get::<Object<'a>>(COUNT)));
        if count < 0 || i64::from(count) >= xref.xref_len() {
            return None;
        }
        let pages = match tree {
            Some(tree) if count > 0 => {
                walk(&tree, count as usize, ctx).or_else(|| Lookup::new(tree, ctx).pages(count))?
            }
            _ => vec![],
        };

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

/// A page tree entry: a dictionary, or an empty one for any other object.
#[derive(Clone)]
struct Node<'a> {
    id: Option<ObjectIdentifier>,
    dict: Dict<'a>,
}

impl<'a> Node<'a> {
    fn new(object: MaybeRef<Object<'a>>, ctx: &ReaderContext<'a>) -> Self {
        let (id, object) = match object {
            MaybeRef::Ref(id) => (Some(id.into()), ctx.xref().get_with(id.into(), ctx)),
            MaybeRef::NotRef(object) => (None, Some(object)),
        };
        let dict = match object {
            Some(Object::Dict(dict)) => dict,
            Some(Object::Stream(stream)) => stream.dict().clone(),
            _ => Dict::empty(),
        };
        Self { id, dict }
    }

    // The object number MuPDF tells objects apart by, 0 for a direct object.
    fn number(&self) -> i32 {
        self.id.map_or(0, |id| id.obj_number)
    }

    // Whether MuPDF sees the entry: a direct `null` is stored as no entry.
    fn has(&self, key: &[u8]) -> bool {
        !matches!(
            self.dict.get_raw::<Object<'a>>(key),
            None | Some(MaybeRef::NotRef(Object::Null(_)))
        )
    }

    fn type_is(&self, name: &[u8]) -> bool {
        self.dict.get::<Name<'_>>(TYPE).as_deref() == Some(name)
    }

    fn kids(&self) -> ArrayIter<'a> {
        self.dict
            .get::<Array<'a>>(KIDS)
            .unwrap_or_default()
            .raw_iter()
    }

    fn page(self, ctx: &PagesContext<'a>, reader_ctx: &ReaderContext<'a>) -> Page<'a> {
        Page::new(self.id, &self.dict, ctx, reader_ctx)
    }
}

/// `pdf_to_int`: an integer cut to C `int`, a real rounded half up, anything else 0.
fn to_int(object: Option<Object<'_>>) -> i32 {
    match object {
        Some(Object::Number(n)) if n.is_integer() => n.as_i64() as i32,
        Some(Object::Number(n)) => {
            // `(int)floorf(f + 0.5)`; C converts an out-of-range float to `int` as x86
            // does: INT_MIN.
            let half_up = (f64::from(n.as_f32()) + 0.5) as f32;
            if (-2_147_483_648.0..2_147_483_648.0).contains(&half_up) {
                let cut = half_up as i32;
                if cut as f32 > half_up { cut - 1 } else { cut }
            } else {
                i32::MIN
            }
        }
        _ => 0,
    }
}

/// `pdf_cycle` gives up on a node with this many /Pages nodes above it.
const MAX_CYCLE_DEPTH: usize = 257;

// `pdf_load_page_tree_imp`: the pages depth first, at most `count`; `None` where
// MuPDF's walk throws.
fn walk<'a>(tree: &Node<'a>, count: usize, ctx: &ReaderContext<'a>) -> Option<Vec<Page<'a>>> {
    struct Frame<'a> {
        number: i32,
        kids: ArrayIter<'a>,
        ctx: PagesContext<'a>,
    }

    let mut pages = vec![];
    let mut stack: Vec<Frame<'a>> = vec![];
    let mut next = Some((tree.clone(), PagesContext::new()));
    loop {
        if let Some((node, pages_ctx)) = next.take() {
            if node.type_is(PAGES) {
                let number = node.number();
                if number > 0
                    && (stack.len() >= MAX_CYCLE_DEPTH || stack.iter().any(|f| f.number == number))
                {
                    return None;
                }
                stack.push(Frame {
                    number,
                    kids: node.kids(),
                    ctx: pages_ctx.with(&node.dict),
                });
            } else if node.type_is(PAGE) && pages.len() < count {
                pages.push(node.page(&pages_ctx, ctx));
            } else {
                return None;
            }
        }
        let Some(frame) = stack.last_mut() else {
            return Some(pages);
        };
        match frame.kids.next() {
            Some(kid) => next = Some((Node::new(kid, ctx), frame.ctx.clone())),
            None => {
                stack.pop();
            }
        }
    }
}

/// A kid as `pdf_lookup_page_loc_imp` sees it.
#[derive(Clone)]
enum Kid<'a> {
    /// A node and its /Count.
    Tree(Node<'a>, i32),
    Leaf(Node<'a>),
}

// MuPDF's search for each page by /Count, with each node's kids read once.
struct Lookup<'a> {
    tree: Node<'a>,
    ctx: ReaderContext<'a>,
    kids: FxHashMap<(i32, usize), Vec<Kid<'a>>>,
}

impl<'a> Lookup<'a> {
    fn new(tree: Node<'a>, ctx: &ReaderContext<'a>) -> Self {
        Self {
            tree,
            ctx: ctx.clone(),
            kids: FxHashMap::default(),
        }
    }

    fn pages(mut self, count: i32) -> Option<Vec<Page<'a>>> {
        (0..count).map(|needle| self.page(needle)).collect()
    }

    // `pdf_lookup_page_loc_imp`: page `needle`, or `None` where MuPDF throws.
    fn page(&mut self, needle: i32) -> Option<Page<'a>> {
        let mut node = self.tree.clone();
        let mut skip = needle;
        let mut marks = vec![];
        let mut pages_ctx = PagesContext::new();
        loop {
            let number = node.number();
            // A direct node has no number of its own; its bytes tell it apart.
            let key = (
                number,
                node.dict.data().as_ptr() as usize * usize::from(number == 0),
            );
            let ctx = &self.ctx;
            let kids = self.kids.entry(key).or_insert_with(|| {
                node.kids()
                    .map(|kid| {
                        let kid = Node::new(kid, ctx);
                        let tree = if kid.has(TYPE) {
                            kid.type_is(PAGES)
                        } else {
                            kid.has(KIDS) && !kid.has(MEDIA_BOX)
                        };
                        if tree {
                            let count = to_int(kid.dict.get::<Object<'a>>(COUNT));
                            Kid::Tree(kid, count)
                        } else {
                            Kid::Leaf(kid)
                        }
                    })
                    .collect()
            });
            if kids.is_empty() || (number > 0 && marks.contains(&number)) {
                return None;
            }
            marks.push(number);
            pages_ctx = pages_ctx.with(&node.dict);
            let mut below = None;
            for kid in kids.iter() {
                match kid {
                    Kid::Tree(kid, count) => {
                        if skip < *count {
                            below = Some(kid.clone());
                            break;
                        }
                        skip = skip.wrapping_sub(*count);
                    }
                    Kid::Leaf(kid) => {
                        if skip == 0 {
                            return Some(kid.clone().page(&pages_ctx, &self.ctx));
                        }
                        skip -= 1;
                    }
                }
            }
            node = below?;
        }
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
    page_streams: OnceLock<Option<Vec<u8>>>,
    resources: Resources<'a>,
    ctx: ReaderContext<'a>,
}

impl<'a> Page<'a> {
    fn new(
        id: Option<ObjectIdentifier>,
        dict: &Dict<'a>,
        ctx: &PagesContext<'a>,
        reader_ctx: &ReaderContext<'a>,
    ) -> Self {
        let media_box = dict.get::<Rect>(MEDIA_BOX).or(ctx.media_box).unwrap_or(A4);

        let crop_box = dict
            .get::<Rect>(CROP_BOX)
            .or(ctx.crop_box)
            .unwrap_or(media_box);

        let rotation = match dict
            .get::<i32>(ROTATE)
            .or(ctx.rotate)
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
            dict.get::<Dict<'a>>(RESOURCES)
                .unwrap_or_else(|| ctx.resources.clone()),
        );

        Self {
            id,
            inner: dict.clone(),
            media_box,
            crop_box,
            rotation,
            page_streams: OnceLock::new(),
            resources,
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
    use crate::object::{Dict, ObjectIdentifier, Rect};
    use crate::pdf::Pdf;

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
    fn count_lower_than_the_tree_keeps_the_first_pages() {
        let tree = pages_node("1", "3 0 R 4 0 R");
        assert_eq!(
            pages(&[CATALOG, &tree, PAGE, SMALL]),
            Some(vec![(3, Some(1600.0))])
        );
    }

    #[test]
    fn count_higher_than_the_tree_is_the_tree() {
        let tree = pages_node("3", "3 0 R 4 0 R");
        assert_eq!(
            pages(&[CATALOG, &tree, PAGE, SMALL, "null"]),
            Some(vec![(3, Some(1600.0)), (4, Some(200.0))])
        );
    }

    #[test]
    fn a_count_mupdf_rejects_refuses_the_document() {
        for count in ["-1", "4"] {
            let tree = pages_node(count, "3 0 R");
            assert_eq!(pages(&[CATALOG, &tree, PAGE]), None, "{count}");
        }
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
    }

    #[test]
    fn a_page_mupdf_cannot_find_refuses_the_document() {
        // A cycle sends MuPDF to search by /Count, which finds no second page.
        let tree = pages_node("2", "3 0 R 4 0 R");
        let cycle = "<< /Type /Pages /Kids [2 0 R] /Count 1 >>";
        assert_eq!(pages(&[CATALOG, &tree, PAGE, cycle]), None);
        // An intermediate /Count lower than its kids hides the last page.
        let tree = pages_node("3", "3 0 R 5 0 R");
        let low = "<< /Type /Pages /Kids [4 0 R 6 0 R] /Count 1 >>";
        let untyped = "<< /Kids [7 0 R] /Count 1 >>";
        assert_eq!(
            pages(&[CATALOG, &tree, low, PAGE, untyped, SMALL, SMALL]),
            None
        );
    }

    #[test]
    fn a_search_by_count_skips_by_each_node_count() {
        // The untyped node sends MuPDF to search by /Count; the first node's /Count 1
        // hides its second kid, so page 2 is the untyped node's kid.
        let tree = pages_node("2", "3 0 R 5 0 R");
        let low = "<< /Type /Pages /Kids [4 0 R 6 0 R] /Count 1 >>";
        let untyped = "<< /Kids [7 0 R] /Count 1 >>";
        let other = "<< /Type /Page /MediaBox [0 0 300 100] >>";
        assert_eq!(
            pages(&[CATALOG, &tree, low, PAGE, untyped, SMALL, other]),
            Some(vec![(4, Some(1600.0)), (7, Some(300.0))])
        );
    }

    #[test]
    fn a_tree_without_pages_has_none() {
        let tree = pages_node("1", "");
        assert_eq!(pages(&[CATALOG, &tree]), Some(vec![]));
        assert_eq!(pages(&["<< /Type /Catalog >>", PAGE]), Some(vec![]));
        let tree = pages_node("0", "3 0 R");
        assert_eq!(pages(&[CATALOG, &tree, PAGE]), Some(vec![]));
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
