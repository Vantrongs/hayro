//! Streams.

pub use crate::crypto::reader::DecryptedReader;
use crate::crypto::{DecryptionTarget, Decryptor};
use crate::filter::Filter;
#[cfg(feature = "images")]
pub use crate::filter::dct::JpegRows;
pub use crate::filter::forward::DecodedReader;
use crate::filter::forward::FlateMode;
use crate::object::Name;
use crate::object::dict::InlineImageDict;
use crate::object::dict::keys::{DECODE_PARMS, DP, F, FILTER, IDENTITY, LENGTH, NAME, TYPE, XREF};
use crate::object::{Array, ObjectIdentifier};
use crate::object::{Dict, Written};
use crate::object::{Object, ObjectLike, ObjectRefLike};
use crate::reader::Reader;
use crate::reader::{
    Ends, Readable, ReaderContext, ReaderExt, Skippable, Unknown, skip_white_spaces_and_comments_in,
};
use crate::sync::{Arc, CancelArc};
use crate::trivia::is_white_space_character;
use crate::util::find_needle;
use crate::xref::XRef;
use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::fmt::{Debug, Display, Formatter};
use core::sync::atomic::AtomicBool;
use smallvec::SmallVec;

/// Filters and parameters in their source order.
#[derive(Clone, Debug)]
pub struct FilterEntries<'a> {
    /// The key the filters are read from: /Filter, or /F, its abbreviation.
    pub key: &'static [u8],
    filters: Filters<'a>,
    params: Params<'a>,
}

/// The filters of a dictionary (filter rule 1).
#[derive(Clone, Debug)]
enum Filters<'a> {
    None,
    /// A filter given by name: the entry as written, and the filter it names.
    One(Written<'a>, Option<Filter>),
    /// An array of filters, reached through a reference or not.
    Many(Array<'a>),
}

/// The parameters of a dictionary's filters (filter rule 3).
#[derive(Clone, Debug)]
enum Params<'a> {
    None,
    /// The parameters of a filter given by name, as written, and the context they
    /// are resolved in.
    One(Written<'a>, ReaderContext<'a>),
    /// An array of parameters, whether it is reached through a reference.
    Many(Array<'a>, bool),
}

/// `written`, a reference resolved in `ctx`, where it is a `T`.
fn resolved<'a, T: ObjectLike<'a>>(written: &Written<'a>, ctx: &ReaderContext<'a>) -> Option<T> {
    let object = match written {
        Written::Ref(reference) => return ctx.xref().get_with((*reference).into(), ctx),
        Written::Name(name) => Object::Name(name.clone()),
        Written::Dict(dict) => Object::Dict(dict.clone()),
        Written::Array(array) => Object::Array(array.clone()),
        _ => return None,
    };
    object.cast()
}

/// An entry of a stream's filters ([`FilterEntries`]).
#[derive(Clone, Debug)]
pub struct FilterEntry<'a> {
    /// The filter the entry names; `None` where it names none, which decoding
    /// passes over in its place: an entry that is no name, a reference to no
    /// object, or a name of no filter.
    pub filter: Option<Filter>,
    /// The entry as written.
    pub written: Written<'a>,
    /// The entry of the parameters in the same place, as written; `None` where
    /// they have none there.
    pub params_written: Option<Written<'a>>,
    /// Whether the parameters are reached through a reference: their entry, or
    /// the array of parameters it is in.
    pub params_by_reference: bool,
    /// The context `params_written` is resolved in, where there is one.
    params_ctx: Option<ReaderContext<'a>>,
}

impl<'a> FilterEntry<'a> {
    /// The parameters, resolved: empty where they are no dictionary. They are
    /// resolved at each call, and at none where the reading asks for none.
    pub fn params(&self) -> Dict<'a> {
        match (&self.params_written, &self.params_ctx) {
            (Some(written), Some(ctx)) => resolved::<Dict<'a>>(written, ctx).unwrap_or_default(),
            _ => Dict::empty(),
        }
    }
}

impl<'a> FilterEntries<'a> {
    /// The filters of a stream with dictionary `dict`, read by the rules of
    /// [`FilterEntries`].
    pub fn of(dict: &Dict<'a>) -> Self {
        let ctx = dict.ctx();
        // Each key is read once, its value resolved once.
        let read = |key: &[u8]| {
            let written = dict.get_written(key)?;
            let object = resolved::<Object<'a>>(&written, ctx);
            Some((written, object))
        };
        let one = |written, name| Filters::One(written, Filter::from_name(name));
        let (key, filters) = match read(F) {
            Some((written, Some(Object::Name(name)))) => (F, one(written, name)),
            f => match (f, read(FILTER)) {
                (_, Some((written, Some(Object::Name(name))))) => (FILTER, one(written, name)),
                (Some((_, Some(Object::Array(array)))), _) => (F, Filters::Many(array)),
                (_, Some((_, Some(Object::Array(array))))) => (FILTER, Filters::Many(array)),
                _ => (FILTER, Filters::None),
            },
        };
        let params = match filters {
            Filters::None => None,
            _ => match dict.get_written(DP) {
                Some(Written::Null) | None => dict.get_written(DECODE_PARMS),
                dp => dp,
            },
        };
        let params = match (&filters, params) {
            (Filters::One(..), Some(written)) => Params::One(written, ctx.clone()),
            (Filters::Many(_), Some(written)) => match resolved::<Array<'a>>(&written, ctx) {
                Some(array) => Params::Many(array, matches!(written, Written::Ref(_))),
                None => Params::None,
            },
            _ => Params::None,
        };
        Self {
            key,
            filters,
            params,
        }
    }

    /// The entries in order, each with the parameters in its place.
    pub fn entries(&self) -> impl Iterator<Item = FilterEntry<'a>> + '_ {
        let one = match (&self.filters, &self.params) {
            (Filters::One(written, filter), params) => {
                let (params_written, params_ctx) = match params {
                    Params::One(written, ctx) => (Some(written.clone()), Some(ctx.clone())),
                    _ => (None, None),
                };
                Some(FilterEntry {
                    filter: *filter,
                    written: written.clone(),
                    params_by_reference: matches!(params_written, Some(Written::Ref(_))),
                    params_written,
                    params_ctx,
                })
            }
            _ => None,
        };
        let many = match &self.filters {
            Filters::Many(array) => Some((array.written(), array.ctx())),
            _ => None,
        };
        let mut params = match &self.params {
            Params::Many(array, by_reference) => {
                Some((array.written(), array.ctx(), *by_reference))
            }
            _ => None,
        };
        let many = many.into_iter().flat_map(|(written, ctx)| {
            written.map(move |written| (resolved::<Name<'a>>(&written, ctx), written))
        });
        one.into_iter().chain(many.map(move |(name, written)| {
            // The parameters in the filter's place, whatever the entry names.
            let (params_written, params_ctx, params_by_reference) = match &mut params {
                Some((items, ctx, by_reference)) => match items.next() {
                    Some(item) => {
                        let by_reference = *by_reference || matches!(item, Written::Ref(_));
                        (Some(item), Some((*ctx).clone()), by_reference)
                    }
                    None => (None, None, false),
                },
                None => (None, None, false),
            };
            FilterEntry {
                filter: name.and_then(Filter::from_name),
                written,
                params_written,
                params_by_reference,
                params_ctx,
            }
        }))
    }

    /// The entries that name a filter, the ones decoding applies.
    pub fn applied(&self) -> impl Iterator<Item = (Filter, FilterEntry<'a>)> + '_ {
        self.entries()
            .filter_map(|entry| Some((entry.filter?, entry)))
    }
}

struct FiltersAndParams<'a> {
    filters: SmallVec<[Filter; 2]>,
    params: SmallVec<[Dict<'a>; 2]>,
}

/// A stream of arbitrary data.
#[derive(Clone)]
pub struct Stream<'a> {
    dict: Dict<'a>,
    data: &'a [u8],
}

impl PartialEq for Stream<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.dict == other.dict && self.data == other.data
    }
}

/// Additional parameters for decoding images.
#[derive(Clone, PartialEq, Default)]
pub struct ImageDecodeParams {
    /// Whether the color space of the image is an indexed color space.
    pub is_indexed: bool,
    /// The bits per component of the image, if that information is available.
    pub bpc: Option<u8>,
    /// The components per channel of the image, if that information is available.
    pub num_components: Option<u8>,
    /// A hint for the desired image dimensions.
    pub target_dimension: Option<(u32, u32)>,
    /// The width of the image as indicated by the image dictionary.
    pub width: u32,
    /// The height of the image as indicated by the image dictionary.
    pub height: u32,
}

enum DecodeTarget<'a> {
    Data,
    PageContent,
    Image(&'a ImageDecodeParams),
}

impl<'a> Stream<'a> {
    /// Reads this object independently of the reference chain that reached it.
    pub fn as_root(&self) -> Self {
        Self {
            dict: self.dict.rooted(true),
            data: self.data,
        }
    }

    pub(crate) fn new(data: &'a [u8], dict: Dict<'a>) -> Self {
        Self { dict, data }
    }

    pub(crate) fn read_with_extent(
        r: &mut Reader<'a>,
        ctx: &ReaderContext<'a>,
    ) -> Option<(Self, usize)> {
        let dict = r.read_with_context::<Dict<'_>>(ctx)?;
        Self::read_after_dict(r, dict, &Unknown, &mut NextEndstream::default())
    }

    pub(crate) fn read_after_dict(
        r: &mut Reader<'a>,
        dict: Dict<'a>,
        ends: &impl Ends,
        next: &mut NextEndstream,
    ) -> Option<(Self, usize)> {
        forward_stream_opener(r, ends)?;
        let start = r.offset();
        let length = dict
            .get_single::<i64>(LENGTH)
            .filter(|&n| n >= 0)
            .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
        let data = length
            .and_then(|length| {
                let data = r.read_bytes(length)?;
                next.skip_white_spaces(r);
                r.forward_tag(b"endstream")?;
                Some(data)
            })
            .or_else(|| {
                r.jump(start);
                next.read(r)
            })?;
        // A recovered terminator can precede the declared end of the payload.
        let payload_end = start.saturating_add(length.unwrap_or(0));
        let end = r
            .offset()
            .max(payload_end.saturating_add(b"endstream".len()));
        Some((Self::new(data, dict), end))
    }

    fn filters_and_params(&self) -> FiltersAndParams<'a> {
        let (filters, params) = self
            .filter_entries()
            .applied()
            .map(|(f, e)| (f, e.params()))
            .unzip();
        FiltersAndParams { filters, params }
    }

    /// Filters paired with their original parameters.
    pub fn filter_entries(&self) -> FilterEntries<'a> {
        FilterEntries::of(&self.dict)
    }

    /// Decodes a terminal CCITT filter to packed rows.
    #[cfg(feature = "images")]
    pub fn fax_image(
        &self,
        params: &ImageDecodeParams,
        cancel: Option<CancelArc<AtomicBool>>,
    ) -> Option<FilterResult<'static>> {
        let mut prefix = self.filters_and_params();
        if prefix.filters.pop() != Some(Filter::CcittFaxDecode) {
            return None;
        }
        let fax =
            crate::filter::ccitt::Fax::new(&prefix.params.pop()?, params.height, cancel.clone());
        let data = self.image_input(&prefix, cancel)?;
        fax.decode(&data)
    }

    #[cfg(feature = "images")]
    fn image_input(
        &self,
        prefix: &FiltersAndParams<'_>,
        cancel: Option<CancelArc<AtomicBool>>,
    ) -> Option<Cow<'a, [u8]>> {
        if prefix.filters.is_empty() {
            self.try_raw_data().ok()
        } else {
            self.collect_filters(prefix, false, cancel).ok()
        }
    }

    /// Opens a terminal DCT filter that can emit native scanlines.
    #[cfg(feature = "images")]
    pub fn jpeg_rows(
        &self,
        params: &ImageDecodeParams,
        cancel: Option<CancelArc<AtomicBool>>,
    ) -> Option<JpegRows<'a>> {
        let mut prefix = self.filters_and_params();
        if prefix.filters.pop() != Some(Filter::DctDecode) {
            return None;
        }
        let jpeg_params = prefix.params.pop()?;
        let data = self.image_input(&prefix, cancel.clone())?;
        JpegRows::new(data, &jpeg_params, params, cancel)?.streaming()
    }

    /// Opens a forward reader of the stream's data filters.
    pub fn decoded_reader(
        &self,
        cancel: Option<CancelArc<AtomicBool>>,
    ) -> Result<DecodedReader<'a>, DecodeFailure> {
        self.forward(&self.filters_and_params(), cancel, false)
    }

    fn forward(
        &self,
        filters: &FiltersAndParams<'_>,
        cancel: Option<CancelArc<AtomicBool>>,
        strict: bool,
    ) -> Result<DecodedReader<'a>, DecodeFailure> {
        let mut reader = DecodedReader::new(self.decrypted_reader()?, cancel, strict);
        for (i, (filter, params)) in filters.filters.iter().zip(&filters.params).enumerate() {
            if i == 0 && *filter == Filter::Crypt {
                continue;
            }
            reader
                .add(*filter, params, None)
                .ok_or(DecodeFailure::StreamDecode)?;
        }
        Ok(reader)
    }

    /// Decodes all data filters, discarding tentative output on failure.
    pub fn decoded_strict(&self) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        let filters = self.filters_and_params();
        if filters.filters.is_empty()
            && !self.has_external_file()
            && !self.dict.ctx().xref().needs_decryption(self.dict.ctx())
        {
            return Ok(Cow::Borrowed(self.data));
        }
        self.collect_filters(&filters, true, None)
    }

    fn collect_filters(
        &self,
        filters: &FiltersAndParams<'_>,
        strict: bool,
        cancel: Option<CancelArc<AtomicBool>>,
    ) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        let mut reader = self.forward(filters, cancel.clone(), strict)?;
        match reader.collect_strict() {
            Err(DecodeFailure::StreamDecode) if reader.retry().is_some() => {}
            result => return result.map(Cow::Owned),
        }
        drop(reader);
        let mut input;
        let mut chain = DecodedReader::new(self.decrypted_reader()?, cancel.clone(), strict);
        for (i, (&filter, params)) in filters.filters.iter().zip(&filters.params).enumerate() {
            if i == 0 && filter == Filter::Crypt {
                continue;
            }
            let mode = if filter == Filter::FlateDecode && chain.ambiguous_flate_header()? {
                let bytes = chain.collect_strict()?;
                drop(chain);
                input = bytes;
                let mut mode = FlateMode::Wrapped;
                loop {
                    let mut probe =
                        DecodedReader::new(DecryptedReader::plain(&input), cancel.clone(), strict);
                    probe
                        .add(filter, params, Some(mode))
                        .ok_or(DecodeFailure::StreamDecode)?;
                    match probe.drain_strict(|_| {}) {
                        Ok(()) => break,
                        Err(DecodeFailure::StreamDecode) => {
                            mode = probe.retry().ok_or(DecodeFailure::StreamDecode)?;
                        }
                        Err(err) => return Err(err),
                    }
                }
                chain = DecodedReader::new(DecryptedReader::plain(&input), cancel.clone(), strict);
                Some(mode)
            } else {
                Some(FlateMode::Detect)
            };
            chain
                .add(filter, params, mode)
                .ok_or(DecodeFailure::StreamDecode)?;
        }
        chain.collect_strict().map(Cow::Owned)
    }

    /// Whether a file specification supplies the stream's data.
    pub fn has_external_file(&self) -> bool {
        matches!(
            self.dict.get::<Object<'_>>(F),
            Some(Object::String(_) | Object::Dict(_))
        )
    }

    /// Opens the stored bytes for incremental decryption.
    pub fn decrypted_reader(&self) -> Result<DecryptedReader<'a>, DecodeFailure> {
        if self.has_external_file() {
            return Err(DecodeFailure::StreamDecode);
        }
        let ctx = self.dict.ctx();
        let encrypted = self.dict.obj_id().is_some()
            && ctx.xref().needs_decryption(ctx)
            && self.dict.get::<Name<'_>>(TYPE).as_deref() != Some(XREF);
        let decryptor = if encrypted {
            ctx.xref().decryptor()
        } else {
            &Decryptor::None
        };
        let filter = encrypted.then(|| self.crypt_filter()).flatten();
        let target = filter.as_ref().map_or(DecryptionTarget::Stream, |name| {
            DecryptionTarget::Named(name)
        });
        DecryptedReader::new(self.data, decryptor, self.obj_id(), target)
            .ok_or(DecodeFailure::Decryption)
    }

    /// Return the decrypted but undecoded data of the stream.
    pub fn raw_data(&self) -> Cow<'a, [u8]> {
        self.try_raw_data().unwrap_or_default()
    }

    /// Decrypted stream bytes or a decryption error.
    pub fn try_raw_data(&self) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        if self.has_external_file() {
            return Err(DecodeFailure::StreamDecode);
        }
        let ctx = self.dict.ctx();

        if ctx.xref().needs_decryption(ctx)
            && self.dict.get::<Name<'_>>(TYPE).as_deref() != Some(XREF)
        {
            let crypt_filter = self.crypt_filter();
            let target = match &crypt_filter {
                Some(name) => DecryptionTarget::Named(name),
                None => DecryptionTarget::Stream,
            };

            match self.dict.obj_id() {
                Some(obj_id) => Ok(Cow::Owned(
                    ctx.xref()
                        .decrypt(obj_id, self.data, target)
                        .ok_or(DecodeFailure::StreamDecode)?,
                )),
                None => Ok(Cow::Borrowed(self.data)),
            }
        } else {
            Ok(Cow::Borrowed(self.data))
        }
    }

    /// Return the raw, underlying dictionary of the stream.
    pub fn dict(&self) -> &Dict<'a> {
        &self.dict
    }

    /// Return the object identifier of the stream.
    pub fn obj_id(&self) -> ObjectIdentifier {
        self.dict
            .obj_id()
            .unwrap_or_else(|| ObjectIdentifier::new(0, 0))
    }

    /// Return the filters that are applied to the stream.
    pub fn filters(&self) -> SmallVec<[Filter; 2]> {
        self.filters_and_params().filters
    }

    /// Return the decoded data of the stream.
    pub fn decoded(&self) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        self.decoded_image(&ImageDecodeParams::default())
            .map(|r| r.data)
    }

    /// Return the decoded data of a stream that doesn't use an image filter.
    pub fn decoded_data(&self) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        self.decode(DecodeTarget::Data).map(|r| r.data)
    }

    /// Read a text field value that is unfiltered or Flate-compressed, rejecting corrupt data.
    pub fn decoded_field_value(&self) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        if self.has_external_file() {
            return Err(DecodeFailure::StreamDecode);
        }
        let chain = self.filters_and_params();
        let mut filters = chain
            .filters
            .iter()
            .zip(&chain.params)
            .enumerate()
            .filter_map(|(i, (filter, params))| {
                (i != 0 || *filter != Filter::Crypt).then_some((filter, params))
            });
        match (filters.next(), filters.next()) {
            (None, _) => self.try_raw_data(),
            (Some((Filter::FlateDecode, _)), None) => {
                for key in [DP, DECODE_PARMS] {
                    if self.dict.get_ref(key).is_some()
                        || self
                            .dict
                            .get::<Array<'_>>(key)
                            .is_some_and(|a| a.raw_iter().any(|v| v.as_obj_ref().is_some()))
                    {
                        return Err(DecodeFailure::StreamDecode);
                    }
                }
                self.decoded_strict()
            }
            _ => Err(DecodeFailure::StreamDecode),
        }
    }

    /// Return the decoded data of the stream, with image metadata if available.
    pub fn decoded_image(
        &self,
        image_params: &ImageDecodeParams,
    ) -> Result<FilterResult<'a>, DecodeFailure> {
        self.decode(DecodeTarget::Image(image_params))
    }

    pub(crate) fn decoded_page_content(&self) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        self.decode(DecodeTarget::PageContent).map(|r| r.data)
    }

    fn decode(&self, target: DecodeTarget<'_>) -> Result<FilterResult<'a>, DecodeFailure> {
        if self.has_external_file() {
            return Err(DecodeFailure::StreamDecode);
        }

        let mut filters_and_params = self.filters_and_params();
        let first_image = filters_and_params.filters.iter().position(Filter::is_image);
        let image_params = match target {
            DecodeTarget::Image(params) => match first_image {
                None => {
                    let data = if filters_and_params.filters.is_empty() {
                        self.try_raw_data()?
                    } else {
                        self.collect_filters(&filters_and_params, false, None)?
                    };
                    return Ok(FilterResult {
                        data,
                        image_data: None,
                    });
                }
                Some(i) if i + 1 == filters_and_params.filters.len() => {
                    let filter = filters_and_params.filters.pop().unwrap();
                    let options = filters_and_params.params.pop().unwrap();
                    let stored = if filters_and_params.filters.is_empty() {
                        self.try_raw_data()?
                    } else {
                        self.collect_filters(&filters_and_params, false, None)?
                    };
                    return filter.apply(&stored, &options, params, false);
                }
                _ => params,
            },
            _ if first_image.is_some() => return Err(DecodeFailure::ImageDecode),
            _ => &ImageDecodeParams::default(),
        };

        let data = self.try_raw_data()?;

        let mut current: Option<FilterResult<'a>> = None;

        for (i, (filter, params)) in filters_and_params
            .filters
            .iter()
            .zip(filters_and_params.params.iter())
            .enumerate()
        {
            if i == 0 && *filter == Filter::Crypt {
                continue;
            }

            let new = filter.apply(
                current.as_ref().map(|c| c.data.as_ref()).unwrap_or(&data),
                params,
                image_params,
                matches!(target, DecodeTarget::PageContent),
            )?;
            current = Some(new);
        }

        Ok(current.unwrap_or(FilterResult {
            data,
            image_data: None,
        }))
    }

    fn crypt_filter(&self) -> Option<Name<'a>> {
        let FiltersAndParams {
            filters, params, ..
        } = self.filters_and_params();

        (filters.first() == Some(&Filter::Crypt)).then(|| {
            params
                .first()
                .and_then(|params| params.get::<Name<'a>>(NAME))
                .unwrap_or(Name::new_unescaped(IDENTITY))
        })
    }

    /// Owns the data needed to reopen this stream.
    pub fn to_owned_stream(&self) -> OwnedStream {
        let ctx = self.dict.ctx();
        OwnedStream(match self.dict.obj_id().and_then(|_| ctx.lookup_id()) {
            Some(id) => OwnedInner::Indirect {
                xref: ctx.xref().clone(),
                id,
            },
            None => OwnedInner::Inline {
                xref: ctx.xref().clone(),
                dict: Arc::from(self.dict.data()),
                data: Arc::from(self.data),
            },
        })
    }
}

/// A stream that owns what it needs to read it again (see [`Stream::to_owned_stream`]).
#[derive(Clone)]
pub struct OwnedStream(OwnedInner);

#[derive(Clone)]
enum OwnedInner {
    Indirect {
        xref: XRef,
        id: ObjectIdentifier,
    },
    Inline {
        xref: XRef,
        dict: Arc<[u8]>,
        data: Arc<[u8]>,
    },
}

impl OwnedStream {
    /// The stream; `None` if it can no longer be read.
    pub fn get(&self) -> Option<Stream<'_>> {
        match &self.0 {
            OwnedInner::Indirect { xref, id } => xref.get::<Stream<'_>>(*id),
            OwnedInner::Inline { xref, dict, data } => {
                let dict = Reader::new(dict)
                    .read_with_context::<InlineImageDict<'_>>(&ReaderContext::new(xref, true))?;
                Some(Stream::new(data, dict.get_dict().clone()))
            }
        }
    }
}

impl Debug for Stream<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "Stream (len: {:?})", self.data.len())
    }
}

impl Display for Stream<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "Stream (len: {})", self.data.len())
    }
}

impl Skippable for Stream<'_> {
    fn skip(_: &mut Reader<'_>, _: bool) -> Option<()> {
        // A stream can never appear in a dict/array, so it should never be skipped.

        None
    }
}

impl<'a> Readable<'a> for Stream<'a> {
    fn read(_: &mut Reader<'a>, _: &ReaderContext<'a>) -> Option<Self> {
        None
    }

    fn read_indirect(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let mut after_dict = r.clone();
        after_dict.skip::<Dict<'_>>(ctx.in_content_stream())?;
        forward_stream_opener(&mut after_dict, &Unknown)?;

        Self::read_with_extent(r, ctx).map(|(stream, _)| stream)
    }
}

pub(crate) fn forward_stream_opener(r: &mut Reader<'_>, ends: &impl Ends) -> Option<()> {
    skip_white_spaces_and_comments_in(r, ends);
    r.forward_tag(b"stream")?;
    r.read_eol()
}

#[derive(Debug, Copy, Clone)]
/// A failure that can occur during decoding a data stream.
pub enum DecodeFailure {
    /// Decoding was cancelled.
    Cancelled,
    /// An image stream failed to decode.
    ImageDecode,
    /// A data stream failed to decode.
    StreamDecode,
    /// A failure occurred while decrypting a file.
    Decryption,
    /// An unknown failure occurred.
    Unknown,
}

/// An image color space.
#[derive(Debug, Copy, Clone)]
pub enum ImageColorSpace {
    /// Grayscale color space.
    Gray,
    /// RGB color space.
    Rgb,
    /// CMYK color space.
    Cmyk,
    /// An unknown color space.
    Unknown(u8),
}

/// Additional data that is extracted from some image streams.
#[derive(Clone)]
pub struct ImageData {
    /// An optional alpha channel of the image.
    pub alpha: Option<Vec<u8>>,
    /// The color space of the image.
    pub color_space: Option<ImageColorSpace>,
    /// The bits per component of the image.
    pub bits_per_component: u8,
    /// The width of the image.
    pub width: u32,
    /// The height of the image.
    pub height: u32,
}

/// The result of applying a filter.
pub struct FilterResult<'a> {
    /// The decoded data.
    pub data: Cow<'a, [u8]>,
    /// Additional data that is extracted from JPX image streams.
    pub image_data: Option<ImageData>,
}

impl FilterResult<'_> {
    pub(crate) fn from_data(data: Vec<u8>) -> Self {
        Self {
            data: Cow::Owned(data),
            image_data: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct NextEndstream {
    searched_from: Option<usize>,
    // None after a search means it reached EOF.
    marker: Option<(usize, usize)>,
    whitespace: core::ops::Range<usize>,
}

impl NextEndstream {
    pub(crate) fn skip_white_spaces(&mut self, r: &mut Reader<'_>) {
        let start = r.offset();
        while r.peek_byte().is_some_and(is_white_space_character) {
            if self.whitespace.contains(&r.offset()) {
                r.jump(self.whitespace.end);
                break;
            }
            r.forward();
        }

        if start < r.offset() && !self.whitespace.contains(&start) {
            self.whitespace = start..r.offset();
        }
    }

    fn read<'a>(&mut self, r: &mut Reader<'a>) -> Option<&'a [u8]> {
        let start = r.offset();
        if self.searched_from.is_none_or(|from| start < from)
            || self.marker.is_some_and(|(_, marker)| start > marker)
        {
            let tail = r.tail()?;
            self.searched_from = Some(start);
            self.marker = find_needle(tail, b"endstream").map(|offset| {
                let whitespace = trim_trailing_ascii_whitespace(&tail[..offset]);
                (start + whitespace, start + offset)
            });
        }

        let (whitespace, marker) = self.marker?;
        r.jump(marker + b"endstream".len());
        r.range(start..whitespace.max(start))
    }
}

fn trim_trailing_ascii_whitespace(data: &[u8]) -> usize {
    let mut end = data.len();

    while data
        .get(end.wrapping_sub(1))
        .copied()
        .is_some_and(is_white_space_character)
    {
        end -= 1;
    }

    end
}

impl<'a> TryFrom<Object<'a>> for Stream<'a> {
    type Error = ();

    fn try_from(value: Object<'a>) -> Result<Self, Self::Error> {
        match value {
            Object::Stream(s) => Ok(s),
            _ => Err(()),
        }
    }
}

impl<'a> ObjectLike<'a> for Stream<'a> {}
impl<'a> ObjectRefLike<'a> for Stream<'a> {
    fn cast_ref<'b>(obj: &'b Object<'a>) -> Option<&'b Self> {
        match obj {
            Object::Stream(stream) => Some(stream),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::object::Stream;
    use crate::reader::Reader;
    use crate::reader::{ReaderContext, ReaderExt};

    #[test]
    fn field_values_refuse_corrupt_flate() {
        let mut reader =
            Reader::new(b"<< /Filter /FlateDecode /Length 1 >> stream\n\xff\nendstream");
        let stream = reader
            .read_indirect::<Stream<'_>>(&ReaderContext::dummy())
            .unwrap();
        assert!(stream.decoded_data().is_ok());
        assert!(stream.decoded_field_value().is_err());
    }

    #[test]
    fn display() {
        let mut reader = Reader::new(b"<< /Length 3 >> stream\nabc\nendstream");
        let stream = reader
            .read_indirect::<Stream<'_>>(&ReaderContext::dummy())
            .unwrap();

        assert_eq!(format!("{stream}"), "Stream (len: 3)");
    }

    #[test]
    fn stream() {
        let data = b"<< /Length 10 >> stream\nendstream!\nendstream";
        let mut r = Reader::new(data);
        let stream = r
            .read_indirect::<Stream<'_>>(&ReaderContext::dummy())
            .unwrap();

        assert_eq!(stream.data, b"endstream!");
    }

    #[test]
    fn stream_fallback() {
        let data = b"<< /Length 999 >> stream\nabcdefghij\nendstream";
        let mut r = Reader::new(data);
        let stream = r
            .read_indirect::<Stream<'_>>(&ReaderContext::dummy())
            .unwrap();

        assert_eq!(stream.data, b"abcdefghij");
    }

    #[test]
    fn endstream_cursor() {
        use super::NextEndstream;

        let input = b"abc  endstream def\nendstream";
        let mut next = NextEndstream::default();
        let mut r = Reader::new(input);
        assert_eq!(next.read(&mut r).unwrap(), b"abc");
        r.jump(4);
        assert_eq!(next.read(&mut r).unwrap(), b"");
        assert_eq!(next.searched_from, Some(0));
        assert_eq!(next.marker, Some((3, 5)));
        assert_eq!(next.read(&mut r).unwrap(), b" def");
        assert!(next.read(&mut r).is_none());
        assert!(next.read(&mut r).is_none());
        assert_eq!(next.searched_from, Some(input.len()));
        assert_eq!(next.marker, None);

        r.jump(0);
        assert_eq!(next.read(&mut r).unwrap(), b"abc");
        assert_eq!(next.read(&mut r).unwrap(), b" def");
        r.jump(1);
        assert_eq!(next.read(&mut r).unwrap(), b"bc");
    }

    #[test]
    fn declared_length_whitespace() {
        use super::NextEndstream;
        use crate::object::Dict;

        let mut next = NextEndstream::default();
        let body = b"stream\nx     ! endstream";
        for (bytes, whitespace) in [
            (b"<< /Length 3 >>", 10..13),
            (b"<< /Length 4 >>", 10..13),
            (b"<< /Length 1 >>", 8..13),
        ] {
            let mut reader = Reader::new(body);
            let dict = Reader::new(bytes)
                .read_without_context::<Dict<'_>>()
                .unwrap();
            assert_eq!(
                Stream::read_after_dict(&mut reader, dict, &super::Unknown, &mut next)
                    .unwrap()
                    .0
                    .data,
                b"x     !"
            );
            assert_eq!(next.whitespace, whitespace);
        }
    }

    #[test]
    fn stream_with_external_file() {
        let data = b"<< /Length 3 /F (data.bin) >> stream\nabc\nendstream";
        let mut r = Reader::new(data);
        let stream = r
            .read_indirect::<Stream<'_>>(&ReaderContext::dummy())
            .unwrap();

        assert_eq!(stream.data, b"abc");
        assert!(stream.dict().contains_key(crate::object::dict::keys::F));
        assert!(stream.decoded().is_err());
    }

    #[test]
    fn damaged_ascii_hex_image() {
        let mut reader = Reader::new(
            b"<< /Subtype /Image /Width 1 /Height 1 /BitsPerComponent 1 /ColorSpace /DeviceGray \
              /Filter /ASCIIHexDecode /Length 2 >> stream\n.>\nendstream",
        );
        let stream = reader
            .read_indirect::<Stream<'_>>(&ReaderContext::dummy())
            .unwrap();
        assert!(matches!(
            stream.decoded_image(&super::ImageDecodeParams::default()),
            Err(super::DecodeFailure::StreamDecode)
        ));
    }

    #[test]
    fn inline_filter_abbreviation_still_decodes() {
        let mut r = Reader::new(b"<< /F /AHx >>");
        let dict = r
            .read_with_context::<crate::object::Dict<'_>>(&ReaderContext::dummy())
            .unwrap();
        let stream = Stream::new(b"41>", dict);

        assert_eq!(stream.decoded().unwrap().as_ref(), b"A");
    }
}
