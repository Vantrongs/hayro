/*!
A crate for converting PDF pages into either `XObjects` or a new page via [`pdf-writer`](https://docs.rs/pdf-writer/).

This is an internal crate and not meant for external use. Therefore, it's not very
well-documented.
*/

#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[macro_use]
mod log;

mod primitive;

use crate::primitive::{WriteDirect, WriteIndirect};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use hayro_syntax::object::dict::keys::{
    COLORSPACE, EXT_G_STATE, FONT, GROUP, PATTERN, PROPERTIES, RESOURCES, SHADING, SUBTYPE, TYPE,
    XOBJECT,
};
use hayro_syntax::object::stream::DecodeFailure;
use hayro_syntax::object::{Dict, ObjRef, Object};
use hayro_syntax::page::{ContentError, Page, Resources, Rotation};
use hayro_syntax::xref::ContentKey;
use pdf_writer::{Chunk, Content, Filter, Finish, Name, Rect, Ref};
use rustc_hash::FxHashMap;
use std::collections::HashSet;
use std::ops::Deref;
use std::ops::DerefMut;

pub use hayro_syntax;
use hayro_syntax::Pdf;
pub use pdf_writer;
pub use pdf_writer::Settings as ChunkSettings;

/// Apply the extraction queries to the given PDF and return the results.
///
/// The copy never leaves out what the source draws: a page whose content cannot be
/// read whole is an error for its query, and the extraction fails where anything a
/// query or what it copies reads — the page's boxes and resources
/// ([`Page::unreadable`]), its content, every object copied — refers to an object the
/// file lists but that cannot be read ([`hayro_syntax::lost`]), however it is read,
/// as it does where the page tree lost one ([`hayro_syntax::page::Pages::unreadable`])
/// or a stream it copies cannot be decrypted whole. Streams whose dictionaries refer
/// to no other object are written once for all that copy to the same dictionary and
/// data, as fonts embedded in each of several pages are.
pub fn extract<'a>(
    pdf: &Pdf,
    new_ref: Box<dyn FnMut() -> Ref + 'a>,
    chunk_settings: ChunkSettings,
    queries: &[ExtractionQuery],
) -> Result<ExtractionResult, ExtractionError> {
    let pages = pdf.pages();
    if let Some(id) = pages.unreadable() {
        return Err(ExtractionError::UnreadablePageTree(id.into()));
    }
    let xref = pdf.xref();
    let mut ctx = ExtractionContext::new(new_ref, pdf, chunk_settings);

    for query in queries {
        let page = || {
            pages
                .get(query.page_index)
                .ok_or(ExtractionError::InvalidPageIndex(query.page_index))
        };

        ctx.page = query.page_index;
        let (res, lost) = xref.watch(|| match query.query_type {
            ExtractionQueryType::XObject => {
                let root_ref = ctx.new_ref();
                write_xobject(page()?, query.page_index, None, root_ref, &mut ctx).map(|_| root_ref)
            }
            ExtractionQueryType::UserSpaceXObject(bbox) => {
                let root_ref = ctx.new_ref();
                write_xobject(page()?, query.page_index, Some(bbox), root_ref, &mut ctx)
                    .map(|_| root_ref)
            }
            ExtractionQueryType::Page => {
                let root_ref = ctx.new_ref();
                write_page(page()?, root_ref, query.page_index, &mut ctx).map(|_| root_ref)
            }
            ExtractionQueryType::Form(obj) => write_form(obj, page()?, query.page_index, &mut ctx),
        });
        if let Some(id) = lost {
            ctx.fail(match res {
                Err(error) => error,
                Ok(_) => ExtractionError::UnreadablePage(query.page_index, id.into()),
            });
        }

        ctx.root_refs.push(res);
    }

    // Now we have shallowly extracted all pages, now go through all dependencies until there aren't
    // any anymore.
    write_dependencies(pdf, &mut ctx);

    if let Some(error) = ctx.error {
        return Err(error);
    }

    let mut global_chunk = Chunk::with_settings(chunk_settings);

    for chunk in &ctx.chunks {
        global_chunk.extend(chunk);
    }

    Ok(ExtractionResult {
        chunk: global_chunk,
        root_refs: ctx.root_refs,
        page_tree_parent_ref: ctx.page_tree_parent_ref,
    })
}

/// A type of extraction query, indicating as what kind of
/// object you want to extract the page.
#[derive(Copy, Clone, Debug)]
pub enum ExtractionQueryType {
    /// Extract the page as an `XObject`, placed and clipped as hayro reads the page's
    /// boxes.
    XObject,
    /// Extract the page as an `XObject` in the page's default user space, clipped to
    /// the box `[x0, y0, x1, y1]` the caller gives.
    UserSpaceXObject([f32; 4]),
    /// Extract the page as a new page.
    Page,
    /// Extract a stream, such as an annotation's appearance, as a form `XObject`
    /// drawn on the query's page.
    Form(ObjRef),
}

/// An extraction query.
#[derive(Copy, Clone, Debug)]
pub struct ExtractionQuery {
    query_type: ExtractionQueryType,
    page_index: usize,
}

impl ExtractionQuery {
    /// Create a new page extraction query with the given page index.
    pub fn new_page(page_index: usize) -> Self {
        Self {
            query_type: ExtractionQueryType::Page,
            page_index,
        }
    }

    /// Create a query that copies the stream `obj`, with everything it refers to, as a
    /// form `XObject` drawn on page `page_index`. The copy is typed `/XObject /Form`
    /// whatever the stream says: annotation appearance streams may leave the type out.
    /// A stream without a /Resources dictionary uses the page's, as renderers draw an
    /// appearance with the resources of its page; its copy gets those. Queries for
    /// the same stream share a copy where it has its own resources.
    pub fn new_form(obj: ObjRef, page_index: usize) -> Self {
        Self {
            query_type: ExtractionQueryType::Form(obj),
            page_index,
        }
    }

    /// Create a new `XObject` extraction query with the given page index.
    pub fn new_xobject(page_index: usize) -> Self {
        Self {
            query_type: ExtractionQueryType::XObject,
            page_index,
        }
    }

    /// Create a query that copies the page as an `XObject` in its default user space,
    /// with no /Matrix, clipped to `bbox`, `[x0, y0, x1, y1]`: for a caller that
    /// places the page by its own reading of the page's boxes.
    pub fn new_user_space_xobject(page_index: usize, bbox: [f32; 4]) -> Self {
        Self {
            query_type: ExtractionQueryType::UserSpaceXObject(bbox),
            page_index,
        }
    }
}

/// An error that occurred during page extraction.
#[derive(Debug, Copy, Clone)]
pub enum ExtractionError {
    /// An invalid page index was given.
    InvalidPageIndex(usize),
    /// A form query named an object that is not a stream.
    NotAStream(ObjRef),
    /// The content of the page with this index cannot be read whole.
    Content(usize, ContentError),
    /// What copying the page with this index read refers to this object, which the
    /// file lists but which cannot be read ([`hayro_syntax::lost`]).
    UnreadablePage(usize, ObjRef),
    /// The page tree refers to this object, which the file lists but which cannot be
    /// read, and which may have held pages
    /// ([`hayro_syntax::page::Pages::unreadable`]).
    UnreadablePageTree(ObjRef),
    /// A stream the copy needs cannot be decrypted whole.
    StreamData(ObjRef, DecodeFailure),
    /// An object the copy needs is listed in the file but cannot be parsed.
    Unreadable(ObjRef),
}

/// The result of an extraction.
#[derive(Debug)]
pub struct ExtractionResult {
    /// The chunk containing all objects as well as their dependencies.
    pub chunk: Chunk,
    /// The root references of the pages/XObject, one for each extraction query.
    pub root_refs: Vec<Result<Ref, ExtractionError>>,
    /// The reference to the page tree parent that was generated.
    pub page_tree_parent_ref: Ref,
}

/// A stream's copied dictionary, and its data's length and hash.
type StreamKey = (Vec<u8>, usize, u64);

/// What an extraction copies, and what it keeps of what it copied for the queries
/// after: these copies are of what a query read before, and do not read it again,
/// but whatever a query or a copy loses fails the whole extraction
/// ([`ExtractionContext::fail`]), so no query uses a copy that lost anything.
struct ExtractionContext<'a> {
    chunks: Vec<Chunk>,
    visited_objects: HashSet<ObjRef>,
    /// The objects left to copy, each with the index of the page whose copy refers
    /// to it first.
    to_visit_refs: Vec<(ObjRef, usize)>,
    /// The index of the page being copied, or whose copy refers to the object being
    /// copied first.
    page: usize,
    /// What each reference to a source object is written as: the copy's reference,
    /// or `None` for `null`, where the object does not exist.
    ref_targets: FxHashMap<ObjRef, Option<Ref>>,
    /// The copies of streams whose dictionaries refer to no other object: the source
    /// stream and its copy.
    shared_streams: FxHashMap<StreamKey, Vec<(ObjRef, Ref)>>,
    /// The copies of streams form queries asked for, by the stream and, for one
    /// that takes its page's resources, the page.
    forms: FxHashMap<(ObjRef, Option<usize>), Ref>,
    /// The copy of each resource category dictionary pages and forms are drawn
    /// with, by what it is read from, which pages that share it share.
    resource_categories: FxHashMap<ContentKey<'a>, Ref>,
    /// The first part of the source that could not be read whole.
    error: Option<ExtractionError>,
    root_refs: Vec<Result<Ref, ExtractionError>>,
    pdf: &'a Pdf,
    new_ref: Box<dyn FnMut() -> Ref + 'a>,
    ref_map: FxHashMap<ObjRef, Ref>,
    cached_content_streams: FxHashMap<usize, Ref>,
    page_tree_parent_ref: Ref,
    chunk_settings: ChunkSettings,
}

impl<'a> ExtractionContext<'a> {
    fn new(
        mut new_ref: Box<dyn FnMut() -> Ref + 'a>,
        pdf: &'a Pdf,
        chunk_settings: ChunkSettings,
    ) -> Self {
        let page_tree_parent_ref = new_ref();
        Self {
            chunks: vec![],
            visited_objects: HashSet::new(),
            to_visit_refs: Vec::new(),
            page: 0,
            ref_targets: FxHashMap::default(),
            shared_streams: FxHashMap::default(),
            forms: FxHashMap::default(),
            resource_categories: FxHashMap::default(),
            error: None,
            pdf,
            new_ref,
            ref_map: FxHashMap::default(),
            cached_content_streams: FxHashMap::default(),
            root_refs: Vec::new(),
            page_tree_parent_ref,
            chunk_settings,
        }
    }

    pub(crate) fn map_ref(&mut self, ref_: ObjRef) -> Ref {
        if let Some(ref_) = self.ref_map.get(&ref_) {
            *ref_
        } else {
            let new_ref = self.new_ref();
            self.ref_map.insert(ref_, new_ref);

            new_ref
        }
    }

    pub(crate) fn new_ref(&mut self) -> Ref {
        (self.new_ref)()
    }

    /// Records that part of the source could not be read whole.
    pub(crate) fn fail(&mut self, error: ExtractionError) {
        self.error.get_or_insert(error);
    }
}

fn write_dependencies(pdf: &Pdf, ctx: &mut ExtractionContext<'_>) {
    while let Some((ref_, page)) = ctx.to_visit_refs.pop() {
        // Don't visit objects twice!
        if ctx.visited_objects.contains(&ref_) {
            continue;
        }

        ctx.page = page;
        let (written, lost) = pdf.xref().watch(|| {
            let object = pdf.xref().get::<Object<'_>>(ref_.into())?;
            let mut chunk = Chunk::with_settings(ctx.chunk_settings);
            let new_ref = ctx.map_ref(ref_);
            object.write_indirect(&mut chunk, new_ref, ctx);
            ctx.chunks.push(chunk);
            Some(())
        });
        if let Some(id) = lost {
            ctx.fail(ExtractionError::UnreadablePage(page, id.into()));
        }
        if written.is_some() {
            ctx.visited_objects.insert(ref_);
        } else {
            // It was read when it was first referred to.
            warn!("failed to extract object with ref: {ref_:?}");
            ctx.fail(ExtractionError::Unreadable(ref_));
        }
    }
}

/// Extract the given pages from the PDF and resave them as a new PDF. This function shouldn't be
/// used directly and only exists for test purposes.
#[doc(hidden)]
pub fn extract_pages_to_pdf(hayro_pdf: &Pdf, page_indices: &[usize]) -> Vec<u8> {
    let mut pdf = pdf_writer::Pdf::new();
    let mut next_ref = Ref::new(1);
    let requests = page_indices
        .iter()
        .map(|i| ExtractionQuery {
            query_type: ExtractionQueryType::Page,
            page_index: *i,
        })
        .collect::<Vec<_>>();

    let catalog_id = next_ref.bump();

    let extracted = extract(
        hayro_pdf,
        Box::new(|| next_ref.bump()),
        ChunkSettings::default(),
        &requests,
    )
    .unwrap();
    pdf.catalog(catalog_id)
        .pages(extracted.page_tree_parent_ref);
    let count = extracted.root_refs.len();
    pdf.pages(extracted.page_tree_parent_ref)
        .kids(extracted.root_refs.iter().map(|r| r.unwrap()))
        .count(count as i32);
    pdf.extend(&extracted.chunk);

    pdf.finish()
}

/// Extract the given pages as XObjects from the PDF and resave them as a new PDF.
/// This function shouldn't be used directly and only exists for test purposes.
#[doc(hidden)]
pub fn extract_pages_as_xobject_to_pdf(hayro_pdf: &Pdf, page_indices: &[usize]) -> Vec<u8> {
    let hayro_pages = hayro_pdf.pages();
    let page_list = hayro_pages.as_ref();

    let mut pdf = pdf_writer::Pdf::new();
    let mut next_ref = Ref::new(1);

    let catalog_id = next_ref.bump();
    let requests = page_indices
        .iter()
        .map(|i| ExtractionQuery {
            query_type: ExtractionQueryType::XObject,
            page_index: *i,
        })
        .collect::<Vec<_>>();

    let extracted = extract(
        hayro_pdf,
        Box::new(|| next_ref.bump()),
        ChunkSettings::default(),
        &requests,
    )
    .unwrap();

    pdf.catalog(catalog_id)
        .pages(extracted.page_tree_parent_ref);
    let mut page_refs = vec![];

    for (x_object_ref, page_idx) in extracted.root_refs.iter().zip(page_indices) {
        let page = &page_list[*page_idx];
        let render_dimensions = page.render_dimensions();

        let mut content = Content::new();
        content.x_object(Name(b"O1"));

        let finished = content.finish();

        let page_id = next_ref.bump();
        let stream_id = next_ref.bump();
        page_refs.push(page_id);

        let mut page = pdf.page(page_id);
        page.resources()
            .x_objects()
            .pair(Name(b"O1"), x_object_ref.unwrap());
        page.media_box(Rect::new(
            0.0,
            0.0,
            render_dimensions.0,
            render_dimensions.1,
        ));
        page.parent(extracted.page_tree_parent_ref);
        page.contents(stream_id);
        page.finish();

        pdf.stream(stream_id, finished.as_slice());
    }

    let count = extracted.root_refs.len();
    pdf.pages(extracted.page_tree_parent_ref)
        .kids(page_refs)
        .count(count as i32);
    pdf.extend(&extracted.chunk);

    pdf.finish()
}

fn write_page<'a>(
    page: &Page<'a>,
    page_ref: Ref,
    page_idx: usize,
    ctx: &mut ExtractionContext<'a>,
) -> Result<(), ExtractionError> {
    drawn_with(page, ctx);
    let mut chunk = Chunk::with_settings(ctx.chunk_settings);
    // Note: We can cache content stream references, but _not_ the page references themselves.
    // Acrobat for some reason doesn't like duplicate page references in the page tree.
    let stream_ref = if let Some(cached) = ctx.cached_content_streams.get(&page_idx) {
        *cached
    } else {
        let content = page
            .try_page_stream()
            .map_err(|e| ExtractionError::Content(page_idx, e))?;
        let stream_ref = ctx.new_ref();

        chunk
            .stream(stream_ref, &deflate_encode(&content))
            .filter(Filter::FlateDecode);
        ctx.cached_content_streams.insert(page_idx, stream_ref);

        stream_ref
    };

    let mut pdf_page = chunk.page(page_ref);

    pdf_page
        .media_box(convert_rect(&page.media_box()))
        .crop_box(convert_rect(&page.crop_box()))
        .rotate(match page.rotation() {
            Rotation::None => 0,
            Rotation::Horizontal => 90,
            Rotation::Flipped => 180,
            Rotation::FlippedHorizontal => 270,
        })
        .parent(ctx.page_tree_parent_ref)
        .contents(stream_ref);

    let raw_dict = page.raw();

    if let Some(group) = raw_dict.get_raw::<Object<'_>>(GROUP) {
        group.write_direct(pdf_page.insert(Name(GROUP)), ctx);
    }

    serialize_resources(page.resources(), ctx, &mut pdf_page);

    pdf_page.finish();

    ctx.chunks.push(chunk);

    Ok(())
}

/// The page as a form: placed by hayro's reading of its boxes, or, with `bbox`, in
/// its user space clipped to `bbox`.
fn write_xobject<'a>(
    page: &Page<'a>,
    page_idx: usize,
    bbox: Option<[f32; 4]>,
    xobj_ref: Ref,
    ctx: &mut ExtractionContext<'a>,
) -> Result<(), ExtractionError> {
    drawn_with(page, ctx);
    let content = page
        .try_page_stream()
        .map_err(|e| ExtractionError::Content(page_idx, e))?;
    let mut chunk = Chunk::with_settings(ctx.chunk_settings);
    let encoded_stream = deflate_encode(&content);
    let mut x_object = chunk.form_xobject(xobj_ref, &encoded_stream);
    x_object.deref_mut().filter(Filter::FlateDecode);

    if let Some([x0, y0, x1, y1]) = bbox {
        x_object.bbox(Rect::new(x0, y0, x1, y1));
    } else {
        let bbox = page.crop_box();
        let initial_transform = page.initial_transform(false);

        x_object.bbox(Rect::new(
            bbox.x0 as f32,
            bbox.y0 as f32,
            bbox.x1 as f32,
            bbox.y1 as f32,
        ));

        let i = initial_transform.as_coeffs();
        x_object.matrix([
            i[0] as f32,
            i[1] as f32,
            i[2] as f32,
            i[3] as f32,
            i[4] as f32,
            i[5] as f32,
        ]);
    }

    serialize_resources(page.resources(), ctx, &mut x_object);

    let raw_dict = page.raw();

    if let Some(group) = raw_dict.get_raw::<Object<'_>>(GROUP) {
        group.write_direct(x_object.insert(Name(GROUP)), ctx);
    }

    x_object.finish();
    ctx.chunks.push(chunk);

    Ok(())
}

fn write_form<'a>(
    obj: ObjRef,
    page: &Page<'a>,
    page_idx: usize,
    ctx: &mut ExtractionContext<'a>,
) -> Result<Ref, ExtractionError> {
    // Read as `Object` first: a dictionary read as a stream may pick up the data of a
    // stream after it in the file.
    let Some(Object::Stream(stream)) = ctx.pdf.xref().get::<Object<'_>>(obj.into()) else {
        return Err(ExtractionError::NotAStream(obj));
    };
    // What a renderer draws the stream with: its own resources if they are a
    // dictionary, the page's otherwise.
    let own_resources = stream.dict().get::<Dict<'_>>(RESOURCES).is_some();
    if !own_resources {
        drawn_with(page, ctx);
    }
    let key = (obj, (!own_resources).then_some(page_idx));
    if let Some(form_ref) = ctx.forms.get(&key) {
        return Ok(*form_ref);
    }
    let data = stream
        .try_raw_data()
        .map_err(|failure| ExtractionError::StreamData(obj, failure))?;
    let form_ref = ctx.new_ref();
    ctx.forms.insert(key, form_ref);
    let mut chunk = Chunk::with_settings(ctx.chunk_settings);
    let mut form = chunk.form_xobject(form_ref, &data);
    let skip: &[&[u8]] = if own_resources {
        &[TYPE, SUBTYPE]
    } else {
        &[TYPE, SUBTYPE, RESOURCES]
    };
    primitive::write_stream_dict(stream.dict(), form.deref_mut(), ctx, skip);
    if !own_resources {
        serialize_resources(page.resources(), ctx, &mut form);
    }
    form.finish();
    ctx.chunks.push(chunk);

    Ok(form_ref)
}

/// Tells the query being copied what reading the boxes and resources of `page` it
/// copies lost.
fn drawn_with(page: &Page<'_>, ctx: &ExtractionContext<'_>) {
    ctx.pdf.xref().note(page.unreadable());
}

/// Writes `/Resources` into `writer`, each category a reference to the copy of its
/// dictionary, written once however many pages and forms are drawn with it, whatever
/// their other categories.
fn serialize_resources<'a>(
    resources: &Resources<'a>,
    ctx: &mut ExtractionContext<'a>,
    writer: &mut pdf_writer::Dict<'_>,
) {
    let categories = [
        (&resources.ext_g_states, EXT_G_STATE),
        (&resources.shadings, SHADING),
        (&resources.patterns, PATTERN),
        (&resources.x_objects, XOBJECT),
        (&resources.color_spaces, COLORSPACE),
        (&resources.fonts, FONT),
        (&resources.properties, PROPERTIES),
    ];
    let copies = categories.map(|(category, name)| {
        (!category.is_empty()).then(|| (name, category_copy(category, ctx)))
    });
    // Resource dictionary is always required (unless it can be inherited), so
    // let's just be safe and always write it.
    let mut output = writer.insert(Name(RESOURCES)).dict();
    for (name, copy) in copies.into_iter().flatten() {
        output.pair(Name(name), copy);
    }
}

/// The copy of resource category `category`, written the first time it is asked for.
fn category_copy<'a>(category: &Dict<'a>, ctx: &mut ExtractionContext<'a>) -> Ref {
    let key = category.content_key();
    if let Some(copy) = ctx.resource_categories.get(&key) {
        return *copy;
    }
    let copy = ctx.new_ref();
    ctx.resource_categories.insert(key, copy);
    let mut chunk = Chunk::with_settings(ctx.chunk_settings);
    let mut output = chunk.indirect(copy).dict();
    for (name, obj) in category.entries() {
        obj.write_direct(output.insert(Name(name.deref())), ctx);
    }
    output.finish();
    ctx.chunks.push(chunk);
    copy
}

/// `data` compressed for a `FlateDecode` stream.
///
/// Level 4 rather than zlib's default 6: on large drawings it takes about a quarter
/// of the time for about 2% more bytes.
pub fn deflate_encode(data: &[u8]) -> Vec<u8> {
    use std::io::Write;

    const COMPRESSION_LEVEL: u8 = 4;
    let mut e = ZlibEncoder::new(Vec::new(), Compression::new(COMPRESSION_LEVEL as u32));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn convert_rect(hy_rect: &hayro_syntax::object::Rect) -> Rect {
    Rect::new(
        hy_rect.x0 as f32,
        hy_rect.y0 as f32,
        hy_rect.x1 as f32,
        hy_rect.y1 as f32,
    )
}
