//! Reading the pages of a PDF document.

use crate::content::{TypedIter, UntypedIter};
use crate::object::Array;
use crate::object::Dict;
use crate::object::Name;
use crate::object::Rect;
use crate::object::Stream;
use crate::object::dict::keys::*;
use crate::object::{MaybeRef, Object, ObjectIdentifier};
use crate::reader::{Readable, Reader, ReaderContext, ReaderExt};
use crate::sync::{FxHashMap, FxHashSet, OnceLock};
use crate::transform::Transform;
use crate::util::FloatExt;
use crate::xref::XRef;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Deref;

struct DictOnly<'a, const PAGE_CANDIDATE: bool = false>(Option<Dict<'a>>);

impl<'a, const PAGE_CANDIDATE: bool> Readable<'a> for DictOnly<'a, PAGE_CANDIDATE> {
    const HANDLES_TYPE_MISMATCH: bool = true;

    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        if r.peek_bytes(2) != Some(b"<<") {
            return Some(Self(None));
        }
        if PAGE_CANDIDATE {
            crate::object::dict::read_dict_with_key(r, ctx, CONTENTS).map(Self)
        } else {
            Dict::read(r, ctx).map(|dict| Self(Some(dict)))
        }
    }

    fn read_indirect(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        if r.peek_bytes(2) != Some(b"<<") {
            if PAGE_CANDIDATE || r.peek_byte() != Some(b'[') {
                Object::skip_indirect_value(r)?;
            }
            return Some(Self(None));
        }

        let mut probe = r.clone();
        if probe.skip::<Dict<'_>>(false).is_some() {
            probe.skip_white_spaces_and_comments();
            if probe.peek_tag(b"stream").is_some() {
                return Some(Self(None));
            }
        }
        Self::read(r, ctx)
    }
}

#[derive(Default)]
struct Attributes<'a> {
    media_box: OnceLock<Option<Rect>>,
    crop_box: OnceLock<Option<Rect>>,
    rotate: OnceLock<Option<i32>>,
    resources: OnceLock<Option<Resources<'a>>>,
}

struct InheritedNode<'a> {
    dict: Dict<'a>,
    parent: Option<usize>,
    cycle_len: usize,
    cached: Attributes<'a>,
}

impl<'a> InheritedNode<'a> {
    fn new(dict: Dict<'a>, parent: Option<usize>) -> Self {
        Self {
            dict,
            parent,
            cycle_len: 0,
            cached: Attributes::default(),
        }
    }
}

fn inherited<'a, T: Clone>(
    dict: &Dict<'a>,
    nodes: &[InheritedNode<'a>],
    mut parent: Option<usize>,
    cache: impl for<'b> Fn(&'b Attributes<'a>) -> &'b OnceLock<Option<T>>,
    mut read: impl FnMut(&Dict<'a>) -> Option<T>,
) -> Option<T> {
    if let Some(value) = read(dict) {
        return Some(value);
    }
    let mut path = Vec::new();
    let mut cycle_left = None;
    let value = loop {
        let Some(index) = parent else { break None };
        let node = &nodes[index];
        if let Some(value) = cache(&node.cached).get() {
            break value.clone();
        }
        if node.cycle_len != 0 {
            let left = cycle_left.get_or_insert(node.cycle_len);
            if *left == 0 {
                break None;
            }
            *left -= 1;
        }
        path.push(index);
        if let Some(value) = read(&node.dict) {
            break Some(value);
        }
        parent = node.parent;
    };
    for index in path {
        cache(&nodes[index].cached).get_or_init(|| value.clone());
    }
    value
}

#[derive(Default)]
struct PageParents<'a> {
    parents: FxHashMap<ObjectIdentifier, Option<usize>>,
    nodes: Vec<InheritedNode<'a>>,
}

impl<'a> PageParents<'a> {
    fn link(&mut self, page: &Dict<'a>, xref: &'a XRef) -> Option<usize> {
        let start = self.nodes.len();
        let mut next = page.get_raw::<Dict<'a>>(PARENT);
        let mut tail = None;
        while let Some(parent) = next {
            let dict = match parent {
                MaybeRef::NotRef(dict) => dict,
                MaybeRef::Ref(reference) => {
                    let id = reference.into();
                    if let Some(&known) = self.parents.get(&id) {
                        tail = known;
                        if let Some(index) = known.filter(|&index| index >= start) {
                            let cycle = &mut self.nodes[index..];
                            let len = cycle.len();
                            for node in cycle {
                                node.cycle_len = len;
                            }
                        }
                        break;
                    }
                    let ctx = ReaderContext::new(xref, false);
                    let dict = xref.get_with::<DictOnly<'_>>(id, &ctx).and_then(|d| d.0);
                    self.parents
                        .insert(id, dict.as_ref().map(|_| self.nodes.len()));
                    let Some(dict) = dict else { break };
                    dict
                }
            };
            next = dict.get_raw(PARENT);
            self.nodes
                .push(InheritedNode::new(dict, Some(self.nodes.len() + 1)));
        }
        if let Some(last) = self.nodes[start..].last_mut() {
            last.parent = tail;
            Some(start)
        } else {
            tail
        }
    }
}

/// A structure holding the pages of a PDF document.
pub struct Pages<'a> {
    pages: Vec<Page<'a>>,
    xref: &'a XRef,
}

impl<'a> Pages<'a> {
    pub(crate) fn new(
        pages_dict: &Dict<'a>,
        ctx: &ReaderContext<'a>,
        xref: &'a XRef,
    ) -> Option<(Self, bool)> {
        let mut pages = vec![];
        let saw_kid = resolve_pages(pages_dict, &mut pages, ctx)?;

        Some((Self { pages, xref }, saw_kid))
    }

    pub(crate) fn new_brute_force(ctx: &ReaderContext<'a>, xref: &'a XRef) -> Option<Self> {
        let mut pages = vec![];
        let mut parents = PageParents::default();
        let mut rejected_resources = FxHashSet::default();
        for DictOnly(candidate) in xref.objects::<DictOnly<'_, true>>() {
            if let Some(dict) = candidate {
                let inherited = parents.link(&dict, xref);
                pages.push(Page::new(
                    &dict,
                    &parents.nodes,
                    inherited,
                    ctx,
                    &mut rejected_resources,
                ));
            }
        }
        (!pages.is_empty()).then_some(Self { pages, xref })
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

fn kids_has_entries(kids: &Array<'_>) -> bool {
    let mut r = Reader::new(kids.data());
    r.skip_white_spaces_and_comments();
    !r.at_end()
}

fn resolve_pages<'a>(
    pages_dict: &Dict<'a>,
    entries: &mut Vec<Page<'a>>,
    reader_ctx: &ReaderContext<'a>,
) -> Option<bool> {
    let mut rejected_resources = FxHashSet::default();
    let mut visited = FxHashSet::default();

    if let Some(id) = pages_dict.obj_id() {
        visited.insert(id);
    }

    let root_kids = pages_dict.get::<Array<'_>>(KIDS)?;
    let mut saw_kid = kids_has_entries(&root_kids);
    let mut stack = vec![root_kids.raw_iter()];
    let mut nodes = vec![InheritedNode::new(pages_dict.clone(), None)];

    while let Some(frame) = stack.last_mut() {
        let Some(kid) = frame.next() else {
            stack.pop();
            nodes.pop();

            continue;
        };

        saw_kid = true;
        let dict = match kid {
            MaybeRef::Ref(id) => {
                if !visited.insert(id.into()) {
                    continue;
                }

                reader_ctx
                    .xref()
                    .get_with::<Dict<'a>>(id.into(), reader_ctx)
            }
            MaybeRef::NotRef(object) => object.into_dict(),
        };

        let Some(dict) = dict else {
            continue;
        };

        let parent = Some(nodes.len() - 1);
        match dict.get::<Name<'_>>(TYPE).as_deref() {
            Some(PAGES) => {
                if let Some(kids) = dict.get::<Array<'_>>(KIDS) {
                    saw_kid |= kids_has_entries(&kids);
                    nodes.push(InheritedNode::new(dict, parent));
                    stack.push(kids.raw_iter());
                }
            }
            // Lenient: treat a missing or unknown `Type` as a page (corpus 0083781).
            _ => {
                entries.push(Page::new(
                    &dict,
                    &nodes,
                    parent,
                    reader_ctx,
                    &mut rejected_resources,
                ));
            }
        }
    }

    Some(saw_kid)
}

/// The rotation of the page.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
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
        dict: &Dict<'a>,
        nodes: &[InheritedNode<'a>],
        parent: Option<usize>,
        reader_ctx: &ReaderContext<'a>,
        rejected_resources: &mut FxHashSet<ObjectIdentifier>,
    ) -> Self {
        let media_box =
            inherited(dict, nodes, parent, |c| &c.media_box, |d| d.get(MEDIA_BOX)).unwrap_or(A4);
        let crop_box = inherited(dict, nodes, parent, |c| &c.crop_box, |d| d.get(CROP_BOX))
            .unwrap_or(media_box);
        let rotate = inherited(dict, nodes, parent, |c| &c.rotate, |d| d.get(ROTATE));
        let resources = inherited(
            dict,
            nodes,
            parent,
            |c| &c.resources,
            |d| {
                let resources = match d.get_raw::<Dict<'_>>(RESOURCES)? {
                    MaybeRef::NotRef(dict) => dict,
                    MaybeRef::Ref(id) => {
                        let id = id.into();
                        if rejected_resources.contains(&id) {
                            return None;
                        }
                        let DictOnly(dict) =
                            d.ctx().xref().get_with::<DictOnly<'_>>(id, d.ctx())?;
                        if dict.is_none() {
                            rejected_resources.insert(id);
                        }
                        dict?
                    }
                };
                Some(Resources::new(resources))
            },
        )
        .unwrap_or_default();
        let rotation = match rotate.unwrap_or(0).rem_euclid(360) {
            0 => Rotation::None,
            90 => Rotation::Horizontal,
            180 => Rotation::Flipped,
            270 => Rotation::FlippedHorizontal,
            _ => Rotation::None,
        };

        Self {
            inner: dict.clone(),
            media_box,
            crop_box,
            rotation,
            page_streams: OnceLock::new(),
            resources,
            ctx: reader_ctx.clone(),
        }
    }

    fn operations_impl(&self) -> Option<UntypedIter<'_>> {
        let stream = self.page_stream()?;
        let iter = UntypedIter::new(stream);

        Some(iter)
    }

    /// Return the decoded content stream of the page.
    pub fn page_stream(&self) -> Option<&[u8]> {
        let convert_single = |s: Stream<'_>| {
            let data = s.decoded_page_content().ok()?;
            Some(data.into_owned())
        };

        self.page_streams
            .get_or_init(|| {
                if let Some(stream) = self.inner.get::<Stream<'_>>(CONTENTS) {
                    convert_single(stream)
                } else if let Some(array) = self.inner.get::<Array<'_>>(CONTENTS) {
                    let streams = array
                        .refs()
                        .filter_map(|r| self.xref().get_with::<Stream<'_>>(r.into(), &self.ctx))
                        .flat_map(convert_single);

                    let mut collected = vec![];

                    for stream in streams {
                        collected.extend(stream);
                        // Streams must have at least one whitespace in-between.
                        collected.push(b' ');
                    }

                    Some(collected)
                } else {
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

    /// Return the dimensions of the intersected crop box, or A4 for zero-area pages.
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

    /// Return the rotated width and height of the page.
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

    /// Return the initial transform for rendering the page.
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
#[derive(Clone, Debug, Default)]
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

    // Keep in sync with the `sync` module.
    #[cfg(feature = "std")]
    pub(crate) use std::sync::Arc;

    #[cfg(not(feature = "std"))]
    pub(crate) use alloc::rc::Rc as Arc;

    pub(crate) struct CachedPages {
        pages: Pages<'static>,
        // Must come after `pages`, which borrows from it.
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
            let pages = xref.retry_after_repair(|repaired| {
                let pages_ref = xref_reference
                    .get_with::<crate::object::Dict<'_>>(xref.root_id(), &ctx)
                    .and_then(|catalog| catalog.get_ref(crate::object::dict::keys::PAGES))
                    .map(Into::into)
                    .unwrap_or(xref.trailer_data().pages_ref);
                let tree = xref_reference
                    .get_with(pages_ref, &ctx)
                    .and_then(|p| Pages::new(&p, &ctx, xref_reference));
                match tree {
                    Some((pages, true)) if repaired && pages.is_empty() => {
                        Pages::new_brute_force(&ctx, xref_reference).or(Some(pages))
                    }
                    Some((pages, _)) => Some(pages),
                    None => Pages::new_brute_force(&ctx, xref_reference),
                }
            })?;

            Some(Self { pages, _xref: xref })
        }

        pub(crate) fn get(&self) -> &Pages<'_> {
            &self.pages
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfinished_array_parent_does_not_trigger_repair() {
        let mut reader = Reader::new(b"[ 0 1");
        assert!(matches!(
            DictOnly::<'_, false>::read_indirect(&mut reader, ReaderContext::dummy_ref()),
            Some(DictOnly(None))
        ));
        assert_eq!(reader.peek_byte(), Some(b'['));
    }
}
