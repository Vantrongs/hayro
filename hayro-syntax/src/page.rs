//! Reading the pages of a PDF document.

use crate::content::{TypedIter, UntypedIter};
use crate::hash;
use crate::lost::{Lost, Memo};
use crate::object::Array;
use crate::object::Dict;
use crate::object::Name;
use crate::object::Rect;
use crate::object::Stream;
use crate::object::dict::keys::*;
use crate::object::stream::DecodeFailure;
use crate::object::{MaybeRef, Null, Object, ObjectIdentifier};
use crate::reader::ReaderContext;
use crate::sync::OnceLock;
use crate::transform::Transform;
use crate::util::FloatExt;

use crate::xref::{ContentKey, XRef};
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Deref;

mod inherit;
mod tree;

pub use inherit::{Inheritable, Inheritance, InheritanceError};

/// The pages in listing order, including repeated leaves.
pub struct Pages<'a> {
    pages: Vec<Page<'a>>,
    xref: &'a XRef,
    lost: Lost,
    root: Option<tree::Key<'a>>,
}

impl<'a> Pages<'a> {
    pub(crate) fn new(ctx: &ReaderContext<'a>, xref: &'a XRef) -> Option<Self> {
        let (walked, walk_lost) = xref.apart(|| {
            let (id, root) = tree::root(xref)?;
            let kids = root.get_single::<Array<'_>>(KIDS);
            if kids.is_none() && root.get_single::<Name<'_>>(TYPE).as_deref() != Some(PAGE) {
                return None;
            }
            let (tree, pages) = tree::walk(xref, id, root);
            if xref.is_repaired()
                && kids.is_some_and(|a| a.raw_iter().next().is_some())
                && pages.is_empty()
            {
                return None;
            }
            Some((tree, pages))
        });
        let ((tree, listed), lost) = match walked {
            Some((tree, listed)) => ((tree, listed), walk_lost),
            _ => {
                let (scanned, lost) = xref.apart(|| tree::scan(xref));
                let lost = lost.or(walk_lost);
                ((tree::Tree::default(), scanned), lost)
            }
        };
        let pages = Inherited::find(xref, &tree, &listed)
            .into_iter()
            .zip(listed)
            .map(|((inherited, unreadable), listed)| Page::new(listed, inherited, unreadable, ctx))
            .collect();
        Some(Self {
            pages,
            xref,
            lost,
            root: tree.root,
        })
    }

    /// The first unreadable listed object encountered while reading this item.
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

/// An error reading a page content stream or an element of its contents array.
#[derive(Debug, Copy, Clone)]
pub enum ContentError {
    /// The part is no stream, or its object cannot be parsed.
    Unreadable(Option<ObjectIdentifier>),
    /// The part's stream cannot be decrypted or decoded.
    Decode(Option<ObjectIdentifier>, DecodeFailure),
}

/// A page dictionary with its effective resources, boxes and rotation.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct PageKey<'a> {
    dict: ContentKey<'a>,
    resources: [ContentKey<'a>; 7],
    boxes: [u64; 8],
    rotation: u8,
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

struct Inherited<'a> {
    media_box: Option<Rect>,
    crop_box: Option<Rect>,
    rotation: Rotation,
    resources: Resources<'a>,
}

impl<'a> Inherited<'a> {
    fn find(
        xref: &'a XRef,
        tree: &tree::Tree<'a>,
        listed: &[tree::Listed<'a>],
    ) -> Vec<(Self, Lost)> {
        let mut lookups = inherit::Lookups::new(xref, tree.root, Some(tree));
        let mut boxes = Memo::<ContentKey<'a>, Option<Rect>>::default();
        let mut resources = Memo::<ContentKey<'a>, Resources<'a>>::default();
        let mut each = |lookups: &mut inherit::Lookups<'_, 'a>| -> Vec<(Self, Lost)> {
            listed
                .iter()
                .map(|page| {
                    let start = inherit::Start {
                        key: tree::Key::of(page.id, &page.dict),
                        dict: &page.dict,
                        under: page.under,
                    };
                    xref.apart(|| {
                        let [media_box, crop_box, rotate, found] =
                            Inheritable::ALL.map(|key| lookups.get(&start, key).ok().flatten());
                        let mut rect = |value: Option<Object<'a>>| match value {
                            Some(Object::Array(array)) => {
                                *boxes.get(xref, array.content_key(), || {
                                    Rect::try_from(Object::Array(array.clone())).ok()
                                })
                            }
                            _ => None,
                        };
                        let media_box = rect(media_box);
                        let crop_box = rect(crop_box);
                        let resources = match found {
                            Some(Object::Dict(dict)) => resources
                                .get(xref, dict.content_key(), || {
                                    Resources::read(|key| tree::value(xref, &dict, key))
                                })
                                .clone(),
                            _ => Resources::new(Dict::empty()),
                        };
                        Self {
                            media_box,
                            crop_box,
                            rotation: rotation(rotate),
                            resources,
                        }
                    })
                })
                .collect()
        };
        let found = each(&mut lookups);
        if lookups.repaired() {
            each(&mut lookups)
        } else {
            found
        }
    }
}

fn rotation(value: Option<Object<'_>>) -> Rotation {
    let Some(Object::Number(number)) = value else {
        return Rotation::None;
    };
    let Ok(degrees) = i32::try_from(number.as_i64()) else {
        return Rotation::None;
    };
    match degrees.rem_euclid(360) {
        90 => Rotation::Horizontal,
        180 => Rotation::Flipped,
        270 => Rotation::FlippedHorizontal,
        _ => Rotation::None,
    }
}

/// A PDF page.
pub struct Page<'a> {
    id: Option<ObjectIdentifier>,
    inner: Dict<'a>,
    media_box: Rect,
    crop_box: Rect,
    rotation: Rotation,
    page_streams: OnceLock<(Option<Box<[u8]>>, Lost)>,
    resources: Resources<'a>,
    unreadable: Lost,
    ctx: ReaderContext<'a>,
    under: Option<u32>,
}

impl<'a> Page<'a> {
    fn new(
        listed: tree::Listed<'a>,
        inherited: Inherited<'a>,
        unreadable: Lost,
        reader_ctx: &ReaderContext<'a>,
    ) -> Self {
        let media_box = inherited.media_box.unwrap_or(A4);
        let crop_box = inherited.crop_box.unwrap_or(media_box);

        Self {
            id: listed.id,
            inner: listed.dict,
            media_box,
            crop_box,
            rotation: inherited.rotation,
            page_streams: OnceLock::new(),
            resources: inherited.resources,
            unreadable,
            ctx: reader_ctx.clone(),
            under: listed.under,
        }
    }

    /// Identify pages with the same dictionary and inherited drawing parameters.
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

    /// Return the cached decoded content of the page.
    pub fn page_stream(&self) -> Option<&[u8]> {
        let (data, lost) = self.page_streams.get_or_init(|| {
            self.xref()
                .watch(|| self.read_page_stream().map(Vec::into_boxed_slice))
        });
        self.xref().note(*lost);
        data.as_deref()
    }

    /// Decode and concatenate the readable content streams.
    pub fn read_page_stream(&self) -> Option<Vec<u8>> {
        if let Some(stream) = self.inner.get::<Stream<'_>>(CONTENTS) {
            Some(stream.decoded_page_content().ok()?.into_owned())
        } else if let Some(array) = self.inner.get::<Array<'_>>(CONTENTS) {
            let mut ranges = hash::Map::<_, core::ops::Range<usize>>::default();
            let mut collected = vec![];
            // A part that is no stream is passed over, as `MuPDF` passes it
            // over (`PyMuPDF` 1.28.2, observed): a null, a number, a dictionary,
            // a reference to none.
            for item in array.raw_iter() {
                let key = self.body_key(&item);
                if let Some(range) = key.and_then(|k| ranges.get(&k)) {
                    collected.extend_from_within(range.clone());
                    continue;
                }
                let stream = match item {
                    MaybeRef::Ref(r) => self.xref().get_with::<Stream<'_>>(r.into(), array.ctx()),
                    MaybeRef::NotRef(object) => object.into_stream(),
                };
                let Some(stream) = stream else {
                    continue;
                };
                let start = collected.len();
                if let Ok(data) = stream.decoded_page_content() {
                    collected.reserve(data.len() + 1);
                    collected.extend_from_slice(&data);
                    // Streams must have at least one whitespace in-between.
                    collected.push(b' ');
                }
                if let Some(key) = key {
                    ranges.insert(key, start..collected.len());
                }
            }

            Some(collected)
        } else {
            None
        }
    }

    /// Return the concatenated decoded content or the first content error.
    pub fn try_page_stream(&self) -> Result<Vec<u8>, ContentError> {
        let mut content = vec![];
        self.try_page_stream_to(&mut |data| content.extend_from_slice(data))?;
        Ok(content)
    }

    /// Emits each decoded content stream and the separators between array entries.
    pub fn try_page_stream_to(&self, out: &mut dyn FnMut(&[u8])) -> Result<(), ContentError> {
        let xref = self.xref();
        let (result, lost) = xref.watch(|| {
            let resolve = |item: MaybeRef<Object<'a>>| {
                let id = item.as_obj_ref().map(Into::into);
                (id, item.resolve(&self.ctx).unwrap_or(Object::Null(Null)))
            };
            let emit = |id, object, spaced, out: &mut dyn FnMut(&[u8])| match object {
                Object::Stream(stream) => {
                    let data = stream
                        .decoded_strict()
                        .map_err(|e| ContentError::Decode(id, e))?;
                    out(&data);
                    if spaced {
                        out(b" ");
                    }
                    Ok(())
                }
                Object::Null(_) => Ok(()),
                _ => Err(ContentError::Unreadable(id)),
            };
            if let Some(contents) = self.inner.get_raw(CONTENTS) {
                let (id, value) = resolve(contents);
                if let Object::Array(array) = value {
                    for item in array.raw_iter() {
                        let (id, obj) = resolve(item);
                        emit(id, obj, true, out)?;
                    }
                } else {
                    emit(id, value, false, out)?;
                }
            } else if self.inner.contains_key(CONTENTS) {
                return Err(ContentError::Unreadable(self.id));
            }
            Ok(())
        });
        match lost {
            Some(id) => Err(ContentError::Unreadable(Some(id))),
            None => result,
        }
    }

    fn body_key<T>(&self, item: &MaybeRef<T>) -> Option<ContentKey<'a>> {
        match item {
            MaybeRef::Ref(r) => self.ctx.xref().content_key((*r).into()),
            MaybeRef::NotRef(_) => None,
        }
    }

    /// The first unreadable listed object encountered while reading this item.
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

    use crate::sync::Arc;

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
            let pages = Pages::new(&ctx, xref_reference)?;

            Some(Self { pages, _xref: xref })
        }

        pub(crate) fn get(&self) -> &Pages<'_> {
            &self.pages
        }
    }
}
