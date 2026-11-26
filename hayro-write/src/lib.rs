/*!
A crate for converting PDF pages into either `XObjects` or a new page via [`pdf-writer`](https://docs.rs/pdf-writer/).

This is an internal crate and not meant for external use. Therefore, it's not very
well-documented.
*/

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod primitive;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use hayro_syntax::hash;
use hayro_syntax::object::dict::keys::{
    BBOX, COLORSPACE, EXT_G_STATE, FONT, GROUP, MATRIX, OC, OCPROPERTIES, PATTERN, PROPERTIES,
    RESOURCES, SHADING, SUBTYPE, TYPE, XOBJECT,
};
use hayro_syntax::object::stream::DecodeFailure;
use hayro_syntax::object::{Dict, ObjRef, Object, Written};
use hayro_syntax::page::{ContentError, Page, PageKey, Resources, Rotation};

use hayro_syntax::xref::ContentKey;
use pdf_writer::{Chunk, Content, Filter, Finish, Name, Rect, Ref};
use std::ops::Deref;
use std::ops::DerefMut;

pub use hayro_syntax;
use hayro_syntax::Pdf;
pub use pdf_writer;
pub use pdf_writer::Settings as ChunkSettings;

/// Copy the queried objects and their dependencies into a shared chunk.
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

    let mut oc_properties = None;
    for query in queries {
        let page = || {
            pages
                .get(query.page_index)
                .ok_or(ExtractionError::InvalidPageIndex(query.page_index))
        };

        ctx.page = query.page_index;
        let (res, lost) = xref.watch(|| match query.query_type {
            ExtractionQueryType::XObject => {
                write_xobject(page()?, query.page_index, None, &mut ctx)
            }
            ExtractionQueryType::UserSpaceXObject(bbox) => {
                write_xobject(page()?, query.page_index, Some(bbox), &mut ctx)
            }
            ExtractionQueryType::Page => {
                let root_ref = ctx.new_ref();
                write_page(page()?, root_ref, query.page_index, &mut ctx).map(|_| root_ref)
            }
            ExtractionQueryType::Form(obj, placement) => {
                write_form(obj, placement, page()?, &mut ctx)
            }
            ExtractionQueryType::CatalogEntry(key) => {
                let reference = write_catalog_entry(key, &mut ctx);
                if key == OCPROPERTIES && xref.has_optional_content_groups() {
                    oc_properties = Some(reference);
                }
                Ok(reference)
            }
        });
        if let Some(id) = lost {
            ctx.fail(match res {
                Err(error) => error,
                Ok(_) => ExtractionError::UnreadablePage(query.page_index, id.into()),
            });
        }

        ctx.root_refs.push(res);
    }

    let single_source = queries.iter().any(|q| {
        matches!(
            q.query_type,
            ExtractionQueryType::Page | ExtractionQueryType::XObject
        )
    });
    if single_source && oc_properties.is_none() && xref.has_optional_content_groups() {
        let (reference, lost) = xref.watch(|| write_catalog_entry(OCPROPERTIES, &mut ctx));
        if let Some(id) = lost {
            return Err(ExtractionError::Unreadable(id.into()));
        }
        oc_properties = Some(reference);
    }
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
        oc_properties,
        root_refs: ctx.root_refs,
        page_tree_parent_ref: ctx.page_tree_parent_ref,
    })
}

/// The kind of object a page is extracted as.
#[derive(Copy, Clone, Debug)]
pub enum ExtractionQueryType {
    /// Copy a page as a form placed using its page boxes.
    XObject,
    /// Copy a page as a form clipped in its default user space.
    UserSpaceXObject([f32; 4]),
    /// Extract the page as a new page.
    Page,
    /// Copy a stream as a form with the requested placement.
    Form(ObjRef, FormPlacement),
    /// Copy an entry of the document's catalog as an object of its own.
    CatalogEntry(&'static [u8]),
}

/// The `/BBox` and `/Matrix` a form copied by [`ExtractionQuery::new_form`] gets.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FormPlacement {
    /// `[x0, y0, x1, y1]`.
    pub bbox: [f32; 4],
    /// `[a, b, c, d, e, f]`.
    pub matrix: [f32; 6],
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

    /// Copy an appearance as a form with the supplied box, matrix and fallback page resources.
    pub fn new_form(obj: ObjRef, page_index: usize, placement: FormPlacement) -> Self {
        Self {
            query_type: ExtractionQueryType::Form(obj, placement),
            page_index,
        }
    }

    /// Copy the named catalog entry, yielding a null object when absent.
    pub fn new_catalog_entry(key: &'static [u8]) -> Self {
        Self {
            query_type: ExtractionQueryType::CatalogEntry(key),
            page_index: 0,
        }
    }

    /// Copy the page as a form using its effective boxes.
    pub fn new_xobject(page_index: usize) -> Self {
        Self {
            query_type: ExtractionQueryType::XObject,
            page_index,
        }
    }

    /// Copy the page as a form clipped to the supplied box in default user space.
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
    /// A listed object used by this page could not be read.
    UnreadablePage(usize, ObjRef),
    /// A listed object used to enumerate pages could not be read.
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
    /// Optional content properties for a single-source catalog.
    pub oc_properties: Option<Ref>,
    /// The root references of the pages/XObject, one for each extraction query.
    pub root_refs: Vec<Result<Ref, ExtractionError>>,
    /// The reference to the page tree parent that was generated.
    pub page_tree_parent_ref: Ref,
}

impl ExtractionResult {
    /// Set the page tree and optional content properties of a single-source catalog.
    pub fn write_catalog(&self, catalog: &mut pdf_writer::writers::Catalog<'_>) {
        catalog.pages(self.page_tree_parent_ref);
        if let Some(id) = self.oc_properties {
            catalog.pair(Name(OCPROPERTIES), id);
        }
    }
}

type StreamKey = (Vec<u8>, usize, u64);

struct ExtractionContext<'a> {
    chunks: Vec<Chunk>,
    copies: hash::Map<ContentKey<'a>, Ref>,
    null: Option<Ref>,
    nothing: Option<Ref>,
    to_copy: Vec<(ObjRef, Ref, usize)>,
    page: usize,
    shared_streams: hash::Map<StreamKey, (ObjRef, Ref)>,
    forms: hash::Map<(ContentKey<'a>, [u32; 10], Option<PageKey<'a>>), Ref>,
    page_forms: hash::Map<(PageKey<'a>, Placement), Result<Ref, ContentError>>,
    resource_categories: hash::Map<ContentKey<'a>, Ref>,
    error: Option<ExtractionError>,
    root_refs: Vec<Result<Ref, ExtractionError>>,
    pdf: &'a Pdf,
    new_ref: Box<dyn FnMut() -> Ref + 'a>,
    page_contents: hash::Map<PageKey<'a>, Ref>,
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
            copies: hash::Map::default(),
            null: None,
            nothing: None,
            to_copy: Vec::new(),
            page: 0,
            shared_streams: hash::Map::default(),
            forms: hash::Map::default(),
            page_forms: hash::Map::default(),
            resource_categories: hash::Map::default(),
            error: None,
            pdf,
            new_ref,
            page_contents: hash::Map::default(),
            root_refs: Vec::new(),
            page_tree_parent_ref,
            chunk_settings,
        }
    }

    pub(crate) fn push(&mut self, chunk: Chunk) {
        self.chunks.push(chunk);
    }

    pub(crate) fn new_ref(&mut self) -> Ref {
        (self.new_ref)()
    }

    pub(crate) fn null(&mut self) -> Ref {
        if let Some(null) = self.null {
            return null;
        }
        let null = self.new_ref();
        let mut chunk = Chunk::with_settings(self.chunk_settings);
        chunk.indirect(null).primitive(pdf_writer::Null);
        self.push(chunk);
        self.null = Some(null);
        null
    }

    pub(crate) fn nothing(&mut self) -> Ref {
        if let Some(nothing) = self.nothing {
            return nothing;
        }
        let nothing = self.new_ref();
        self.nothing = Some(nothing);
        nothing
    }

    pub(crate) fn fail(&mut self, error: ExtractionError) {
        self.error.get_or_insert(error);
    }
}

fn write_catalog_entry(key: &[u8], ctx: &mut ExtractionContext<'_>) -> Ref {
    let xref = ctx.pdf.xref();
    let Some(catalog) = xref.get::<Dict<'_>>(xref.root_id()) else {
        return ctx.null();
    };
    let Some(value) = catalog.get_written(key) else {
        return ctx.null();
    };
    if let Written::Ref(r) = value {
        return primitive::target(r, ctx);
    }
    let copy = ctx.new_ref();
    let mut chunk = Chunk::with_settings(ctx.chunk_settings);
    primitive::write_indirect(&value, &mut chunk, copy, ctx);
    ctx.push(chunk);
    copy
}

fn write_dependencies(pdf: &Pdf, ctx: &mut ExtractionContext<'_>) {
    while let Some((source, copy, page)) = ctx.to_copy.pop() {
        ctx.page = page;
        let (written, lost) = pdf.xref().watch(|| {
            let object = pdf.xref().written(source.into())?;
            let mut chunk = Chunk::with_settings(ctx.chunk_settings);
            primitive::write_indirect(&object, &mut chunk, copy, ctx);
            ctx.push(chunk);
            Some(())
        });
        if let Some(id) = lost {
            ctx.fail(ExtractionError::UnreadablePage(page, id.into()));
        }
        if written.is_none() {
            // It was read when it was first referred to.

            ctx.fail(ExtractionError::Unreadable(source));
        }
    }
}

/// Extracts the given pages into a new PDF.
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
    extracted.write_catalog(&mut pdf.catalog(catalog_id));
    let count = extracted.root_refs.len();
    pdf.pages(extracted.page_tree_parent_ref)
        .kids(extracted.root_refs.iter().map(|r| r.unwrap()))
        .count(count as i32);
    pdf.extend(&extracted.chunk);

    pdf.finish()
}

/// Extracts the given pages as XObjects into a new PDF.
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

    extracted.write_catalog(&mut pdf.catalog(catalog_id));
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
    let key = page.key();
    let stream_ref = if let Some(cached) = ctx.page_contents.get(&key) {
        *cached
    } else {
        let content = encoded_content(page).map_err(|e| ExtractionError::Content(page_idx, e))?;
        let stream_ref = ctx.new_ref();

        chunk
            .stream(stream_ref, &content)
            .filter(Filter::FlateDecode);
        ctx.page_contents.insert(key, stream_ref);

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

    if let Some(group) = raw_dict.get_written(GROUP) {
        primitive::write(&group, pdf_page.insert(Name(GROUP)), ctx);
    }

    serialize_resources(page.resources(), ctx, &mut pdf_page);

    pdf_page.finish();

    ctx.push(chunk);

    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Placement {
    /// By hayro's reading of the page's boxes.
    Boxes,
    /// In the page's user space, clipped to this box, its numbers' bits.
    UserSpace([u32; 4]),
}

fn write_xobject<'a>(
    page: &Page<'a>,
    page_idx: usize,
    bbox: Option<[f32; 4]>,
    ctx: &mut ExtractionContext<'a>,
) -> Result<Ref, ExtractionError> {
    drawn_with(page, ctx);
    let placement = bbox.map_or(Placement::Boxes, |bbox| {
        Placement::UserSpace(bbox.map(f32::to_bits))
    });
    let key = (page.key(), placement);
    let copy = match ctx.page_forms.get(&key) {
        Some(copy) => *copy,
        None => {
            let copy = copy_xobject(page, bbox, ctx);
            ctx.page_forms.insert(key, copy);
            copy
        }
    };
    copy.map_err(|e| ExtractionError::Content(page_idx, e))
}

fn copy_xobject<'a>(
    page: &Page<'a>,
    bbox: Option<[f32; 4]>,
    ctx: &mut ExtractionContext<'a>,
) -> Result<Ref, ContentError> {
    let xobj_ref = ctx.new_ref();
    let encoded_stream = encoded_content(page)?;
    let mut chunk = Chunk::with_settings(ctx.chunk_settings);
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

    if let Some(group) = raw_dict.get_written(GROUP) {
        primitive::write(&group, x_object.insert(Name(GROUP)), ctx);
    }

    x_object.finish();
    ctx.push(chunk);

    Ok(xobj_ref)
}

fn encoded_content(page: &Page<'_>) -> Result<Vec<u8>, ContentError> {
    use std::io::Write;

    let mut encoder = encoder();
    page.try_page_stream_to(&mut |data| encoder.write_all(data).unwrap())?;
    Ok(encoder.finish().unwrap())
}

fn write_form<'a>(
    obj: ObjRef,
    placement: FormPlacement,
    page: &Page<'a>,
    ctx: &mut ExtractionContext<'a>,
) -> Result<Ref, ExtractionError> {
    // Read as `Object` first: a dictionary read as a stream may pick up the data of a
    // stream after it in the file.
    let xref = ctx.pdf.xref();
    let Some(Object::Stream(stream)) = xref.get::<Object<'_>>(obj.into()) else {
        return Err(ExtractionError::NotAStream(obj));
    };
    // Read, so listed where it can be read.
    let body = xref
        .content_key(obj.into())
        .ok_or(ExtractionError::NotAStream(obj))?;
    // What a renderer draws the stream with: its own resources if they are a
    // dictionary, the page's otherwise.
    let own_resources = stream.dict().get::<Dict<'_>>(RESOURCES).is_some();
    if !own_resources {
        drawn_with(page, ctx);
    }
    let mut bits = [0; 10];
    for (b, v) in bits
        .iter_mut()
        .zip(placement.bbox.iter().chain(&placement.matrix))
    {
        *b = v.to_bits();
    }
    let key = (body, bits, (!own_resources).then(|| page.key()));
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
    // Its /OC is left out: MuPDF draws an appearance whatever it says, and only
    // the annotation's own /OC hides it.
    let skip: &[&[u8]] = if own_resources {
        &[TYPE, SUBTYPE, BBOX, MATRIX, OC]
    } else {
        &[TYPE, SUBTYPE, BBOX, MATRIX, OC, RESOURCES]
    };
    primitive::write_stream_dict(&stream, form.deref_mut(), ctx, skip);
    let [x0, y0, x1, y1] = placement.bbox;
    form.bbox(Rect::new(x0, y0, x1, y1));
    form.matrix(placement.matrix);
    if !own_resources {
        serialize_resources(page.resources(), ctx, &mut form);
    }
    form.finish();
    ctx.push(chunk);

    Ok(form_ref)
}

fn drawn_with(page: &Page<'_>, ctx: &ExtractionContext<'_>) {
    ctx.pdf.xref().note(page.unreadable());
}

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

fn category_copy<'a>(category: &Dict<'a>, ctx: &mut ExtractionContext<'a>) -> Ref {
    let key = category.content_key();
    if let Some(copy) = ctx.resource_categories.get(&key) {
        return *copy;
    }
    let copy = ctx.new_ref();
    ctx.resource_categories.insert(key, copy);
    let mut chunk = Chunk::with_settings(ctx.chunk_settings);
    let mut output = chunk.indirect(copy).dict();
    for (name, obj) in category.written() {
        primitive::write(&obj, output.insert(Name(name.deref())), ctx);
    }
    output.finish();
    ctx.push(chunk);
    copy
}

/// Compress bytes for a `FlateDecode` stream.
pub fn deflate_encode(data: &[u8]) -> Vec<u8> {
    use std::io::Write;

    let mut e = encoder();
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn encoder() -> ZlibEncoder<Vec<u8>> {
    const COMPRESSION_LEVEL: u32 = 4;
    ZlibEncoder::new(Vec::new(), Compression::new(COMPRESSION_LEVEL))
}

fn convert_rect(hy_rect: &hayro_syntax::object::Rect) -> Rect {
    Rect::new(
        hy_rect.x0 as f32,
        hy_rect.y0 as f32,
        hy_rect.x1 as f32,
        hy_rect.y1 as f32,
    )
}
