//! Streams.

use crate::PdfData;
use crate::crypto::reader::DecryptedReader;
use crate::crypto::{DecryptionTarget, Decryptor};
use crate::filter::chain::Layer;
pub use crate::filter::chain::StrictReader;
use crate::filter::lzw_flate::{PredictorParams, Unpredictor};
#[cfg(feature = "unsafe")]
pub use crate::filter::reader::DecodedCheckpoint;
pub use crate::filter::reader::{DecodedReader, ReadError};
use crate::filter::{Filter, Step};
use crate::object::Dict;
use crate::object::Name;
use crate::object::ObjectIdentifier;
use crate::object::dict::keys::{DECODE_PARMS, DP, F, FILTER, IDENTITY, LENGTH, NAME, TYPE, XREF};
use crate::object::{Array, Object, ObjectLike, ObjectRefLike, Written};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};
use crate::sync::Arc;
use crate::trivia::is_white_space_character;
use crate::util::{OptionLog, find_needle};
use crate::work::Meter;
use crate::xref::StreamId;
use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::{Debug, Display, Formatter};
use core::ops::Deref;
use smallvec::SmallVec;

#[cfg(feature = "images")]
mod jpeg;
#[cfg(feature = "images")]
pub use jpeg::{JpegCheckpoint, JpegInput, JpegRows};

/// A stream's filters, each entry in its place with the parameters in the same
/// place ([`Stream::filter_entries`]), read one at a time: what reading them holds
/// does not grow with their number.
///
/// The filters of a dictionary are read by these rules (pinned by
/// `tests/crypt_filters.rs` and `tests/predictor.rs`):
///
/// 1. They are read from `/F` where it is a name; else from `/Filter` where it is a
///    name; else from `/F` where it is an array; else from `/Filter` where it is one; a
///    reference counts as what it leads to. Otherwise the dictionary has no filters. (A
///    stream object with `/F` names an external file and is not read, so `/F` gives
///    filters to an inline image only.)
/// 2. A filter is a name, or a reference to one, that is a filter's full name or its
///    abbreviation, matched exactly: `/AHx`, `/A85`, `/LZW`, `/Fl`, `/RL`, `/CCF` and
///    `/DCT`; `/JBIG2Decode`, `/JPXDecode` and `/Crypt` have none. An entry of an array
///    that names no filter — no name, a reference to no name, a name of no filter — is
///    passed over in its place: decoding leaves it out, and the parameters stay in
///    their places.
/// 3. The parameters are `/DP` where the dictionary has that key written as another
///    value than `null`, of whatever type, a reference included wherever it leads,
///    else `/DecodeParms`; none where there are no filters. For a filter given by name they are a dictionary, or a reference to one,
///    and none otherwise. For an array of filters they are an array, or a reference to
///    one, whose entry in each filter's place is that filter's: a dictionary, or a
///    reference to one, and none otherwise; parameters of another kind are none for
///    every filter.
/// 4. Each entry ([`FilterEntry`]) gives the filter, the entry as written, its
///    parameters as written, and whether they are reached through a reference, so
///    that a writer can copy the entries it keeps; its parameters are resolved only
///    where they are asked for ([`FilterEntry::params`]), so that a reading resolves
///    those it uses alone.
/// 5. Reading the filters parses each entry it reads once.
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
    /// A target resolution for the image. Note that this is only a hint so that
    /// in case it's possible, a version of the image will be extracted that
    /// is as close as possible to the hinted dimension.
    pub target_dimension: Option<(u32, u32)>,
    /// The width of the image as indicated by the image dictionary.
    pub width: u32,
    /// The height of the image as indicated by the image dictionary.
    pub height: u32,
}

impl ImageDecodeParams {
    /// The bytes of the image's samples, rows padded to whole bytes, with a byte for
    /// each row besides, which a PNG predictor's data has; 0 where the dictionary
    /// does not tell them.
    fn expected_len(&self) -> usize {
        let (Some(bpc), Some(components)) = (self.bpc, self.num_components) else {
            return 0;
        };
        let row_bits = u64::from(self.width) * u64::from(components) * u64::from(bpc);
        (row_bits.div_ceil(8) + 1)
            .checked_mul(u64::from(self.height))
            .and_then(|len| usize::try_from(len).ok())
            .unwrap_or(0)
    }
}

impl<'a> Stream<'a> {
    pub(crate) fn new(data: &'a [u8], dict: Dict<'a>) -> Self {
        Self { dict, data }
    }

    /// The stream's filters with their parameters, each in its place, as decoding
    /// reads them ([`FilterEntries::of`]).
    pub fn filter_entries(&self) -> FilterEntries<'a> {
        FilterEntries::of(&self.dict)
    }

    /// How the stream's data are decrypted and decoded, for an image `height` rows
    /// high, 0 where none is given, read from its dictionary ([`Decoding::of`]).
    fn decoding(&self, height: u32) -> Decoding {
        Decoding::of(&self.dict, true, height)
    }

    /// The decryptor of the stream's document.
    fn decryptor(&self) -> Option<&Decryptor> {
        self.dict.ctx().xref().decrypting()
    }

    /// Return the raw, decrypted data of the stream, empty where decryption fails.
    ///
    /// Stream filters will not be applied.
    pub fn raw_data(&self) -> Cow<'a, [u8]> {
        self.try_raw_data().unwrap_or_default()
    }

    /// The raw, decrypted data of the stream, or [`DecodeFailure::Decryption`] where
    /// it cannot be decrypted whole.
    ///
    /// Stream filters will not be applied. /Crypt filters are: the data are
    /// decrypted with the crypt filter the first one names, and the others pass the
    /// data through, so the caller drops them all.
    pub fn try_raw_data(&self) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        let decryption = Decoding::decryption_of(&self.dict, &self.filter_entries());
        raw_data(
            self.data,
            self.decryptor(),
            &decryption,
            &Meter::default(),
            true,
        )
    }

    /// The stream's data as the file stores them, neither decrypted nor decoded.
    pub(crate) fn stored(&self) -> &'a [u8] {
        self.data
    }

    /// Return the raw, underlying dictionary of the stream.
    pub fn dict(&self) -> &Dict<'a> {
        &self.dict
    }

    /// The stream read as where a resolution of references starts
    /// ([`Dict::as_root`]), its own body still being resolved: what it leads to is
    /// read alike whichever chain of references reached it, as its body, which its
    /// [`Self::identity`] names, decides alone, and a reference back to its body is
    /// refused as a cycle, as from any chain that reaches it.
    pub fn as_root(&self) -> Self {
        Self {
            dict: self.dict.rooted(ReaderContext::as_body_root),
            data: self.data,
        }
    }

    /// Identifies the stream among the streams of every document loaded in this
    /// process, by its body ([`StreamId`]): entries of a damaged table that lead to
    /// one body find one identity. `None` for a stream that is no object of a loaded
    /// document (an inline image, whose bytes may be freed and taken by others while a
    /// cache holds its identity), or on a target without 64-bit atomics.
    pub fn identity(&self) -> Option<StreamId> {
        self.dict.obj_id()?;
        self.dict.content_key().stream_id()
    }

    /// Return the object identifier of the stream.
    pub fn obj_id(&self) -> ObjectIdentifier {
        self.dict.obj_id().unwrap_or_else(|| {
            // In theory shouldn't ever happen, but could theoretically be
            // triggered in a crafted PDF.
            ObjectIdentifier::new(0, 0)
        })
    }

    /// Return the filters that are applied to the stream.
    pub fn filters(&self) -> SmallVec<[Filter; 2]> {
        self.filter_entries().applied().map(|(f, _)| f).collect()
    }

    /// Return the decoded data of the stream.
    ///
    /// Note that the result of this method will not be cached, so calling it multiple
    /// times is expensive.
    pub fn decoded(&self) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        self.decoded_image(&ImageDecodeParams::default())
            .map(|r| r.data)
    }

    /// Return the decoded data of the stream, and return image metadata
    /// if available.
    pub fn decoded_image(
        &self,
        image_params: &ImageDecodeParams,
    ) -> Result<FilterResult<'a>, DecodeFailure> {
        self.decoded_image_metered(image_params, &Meter::default())
    }

    /// [`Self::decoded`], but [`DecodeFailure::StreamDecode`] where the data is
    /// corrupt rather than what a lenient reading recovers of it: for a writer, which
    /// must not take part of the data for all of it. Data cut short is not corrupt.
    pub fn decoded_strict(&self) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        self.decoded_strict_metered(&Meter::default())
    }

    /// [`Self::decoded_strict`], its work counted on `meter`, a bounded amount at a
    /// time: [`DecodeFailure::Cancelled`] once the meter's hook asks to stop.
    pub fn decoded_strict_metered(
        &self,
        meter: &Meter<'_>,
    ) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        self.decoding(0)
            .decode(
                self.data,
                self.decryptor(),
                &ImageDecodeParams::default(),
                meter,
                true,
            )
            .map(|r| r.data)
    }

    /// [`Self::decoded_image`], its work counted on `meter`, a bounded amount at a
    /// time: [`DecodeFailure::Cancelled`] once the meter's hook asks to stop.
    pub fn decoded_image_metered(
        &self,
        image_params: &ImageDecodeParams,
        meter: &Meter<'_>,
    ) -> Result<FilterResult<'a>, DecodeFailure> {
        self.decoding(image_params.height).decode(
            self.data,
            self.decryptor(),
            image_params,
            meter,
            false,
        )
    }

    /// The data [`Self::decoded_strict`] returns, read a piece at a time through all
    /// the stream's filters, however they nest and however many there are, in memory
    /// bounded by the stored data, decrypted, and the decoders' state, windows and
    /// buffers of a bounded number of filters, besides the output of a filter that
    /// makes little ([`StrictReader`]); decryption counts on `meter`.
    /// `Err` where `decoded_strict` fails before decoding: where the data cannot be
    /// decrypted whole, or the hook asks to stop. `Ok(None)` where a filter has no
    /// decoder that reads a piece at a time, an image filter or a predictor only the
    /// whole decoder reverses, or where whole decoding refuses a predictor's
    /// parameters: the caller decodes whole then.
    pub fn decoded_strict_reader(
        &self,
        meter: &Meter<'_>,
    ) -> Result<Option<StrictReader<'a>>, DecodeFailure> {
        // The filters are read one at a time, as they become layers, and kept no
        // longer: what reading a chain of them holds is its layers alone. The
        // parameters of a filter no layer reads are not read, nor any where the
        // decryption, read from the crypt filters alone, fails.
        let filters = FilterEntries::of(&self.dict);
        let decryption = Decoding::decryption_of(&self.dict, &filters);
        if let Err(failure) = decryption {
            return Err(failure);
        }
        let steps = filters.applied().map(|(filter, entry)| match filter {
            Filter::CcittFaxDecode
            | Filter::Jbig2Decode
            | Filter::DctDecode
            | Filter::JpxDecode => None,
            _ => Some(Step::of(filter, || entry.params(), true, 0)),
        });
        let Some(layers) = layers(steps) else {
            return Ok(None);
        };
        let data = raw_data(self.data, self.decryptor(), &decryption, meter, true)?;
        Ok(Some(StrictReader::new(data, layers)))
    }

    /// The decoded data, read incrementally, when the stream's filters allow it: no
    /// filter, or Flate with no predictor, a PNG predictor, or the TIFF predictor of
    /// 8-bit components. It yields the bytes [`Stream::decoded`] returns, without
    /// holding them all. `None` for other filters, or where decryption would fail.
    pub fn decoded_reader(&self) -> Option<DecodedReader<'a>> {
        self.incremental_decoding()?
            .reader(self.data, self.decryptor())
    }

    /// Whether the filters and predictor allow [`Stream::decoded_reader`], judged from
    /// the dictionary alone: no data is read or decrypted.
    pub fn can_read_incrementally(&self) -> bool {
        let filters = FilterEntries::of(&self.dict);
        incremental(&filters).is_some_and(|steps| Decoding::reader_filter_of(&steps).is_some())
    }

    /// [`Self::decoding`] where the filters may allow [`Stream::decoded_reader`]: no
    /// filter or one Flate filter; `None`, no parameters read, for others.
    fn incremental_decoding(&self) -> Option<Decoding> {
        let filters = FilterEntries::of(&self.dict);
        let steps = incremental(&filters)?;
        Some(Decoding {
            decryption: Decoding::decryption_of(&self.dict, &filters),
            steps,
        })
    }

    /// The stream owned ([`OwnedStream`]): its stored data, and how they decode,
    /// read once from its dictionary, so that decoding it reads no object of its
    /// document. The data are the document's file's where they lie in it, else (an
    /// inline image's, or a stream's that a damaged file keeps in an object stream)
    /// a copy of them. They decode as an image `height` rows high, 0 where none is
    /// given, whatever height decoding them is given ([`ImageDecodeParams::height`]):
    /// what of the parameters decoding reads depends on it.
    pub fn to_owned_stream(&self, height: u32) -> OwnedStream {
        self.owned(true, height)
    }

    /// [`Self::to_owned_stream`], with JBIG2 globals if `globals`.
    pub(crate) fn owned(&self, globals: bool, height: u32) -> OwnedStream {
        let decoding = Decoding::of(&self.dict, globals, height);
        let xref = self.dict.ctx().xref();
        let in_file = xref.file_data().and_then(|file| {
            let all = file.as_ref();
            let start = (self.data.as_ptr() as usize).checked_sub(all.as_ptr() as usize)?;
            let range = start..start.checked_add(self.data.len())?;
            // Data in a decoded object stream, or in a content stream, lie elsewhere.
            core::ptr::eq(all.get(range.clone())?, self.data).then(|| (file.clone(), range))
        });
        let (bytes, range, copied) = match in_file {
            Some((file, range)) => (file, range, false),
            None => (PdfData::from(self.data.to_vec()), 0..self.data.len(), true),
        };
        OwnedStream(Arc::new(Owned {
            bytes,
            range,
            copied,
            decryptor: match decoding.decryption {
                Ok(Some(_)) => xref.decrypting_shared(),
                _ => None,
            },
            decoding,
        }))
    }
}

/// A stream owned ([`Stream::to_owned_stream`]): its stored data, which it holds
/// (the document's file, or a copy of them), the decryptor of its document, and how
/// the data decode, read from its dictionary once. Decoding it reads no object of
/// its document, so it keeps nothing in the document and nothing of what it read,
/// however often it is decoded. Cheap to clone.
#[derive(Clone)]
pub struct OwnedStream(Arc<Owned>);

struct Owned {
    /// The bytes the data lie in, and where.
    bytes: PdfData,
    range: core::ops::Range<usize>,
    /// Whether `bytes` are a copy of the data, which this holds alone.
    copied: bool,
    /// Where the data are decrypted.
    decryptor: Option<Arc<Decryptor>>,
    decoding: Decoding,
}

impl Debug for OwnedStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "OwnedStream (len: {})", self.0.range.len())
    }
}

impl OwnedStream {
    /// The stored data.
    pub(crate) fn data(&self) -> &[u8] {
        &self.0.bytes.as_ref()[self.0.range.clone()]
    }

    fn decryptor(&self) -> Option<&Decryptor> {
        self.0.decryptor.as_deref()
    }

    /// The filters applied, in order ([`Stream::filters`]).
    pub fn filters(&self) -> impl Iterator<Item = Filter> + '_ {
        self.0.decoding.steps.iter().map(Step::filter)
    }

    /// The decoded data ([`Stream::decoded`]).
    pub fn decoded(&self) -> Result<Cow<'_, [u8]>, DecodeFailure> {
        self.decoded_image_metered(&ImageDecodeParams::default(), &Meter::default())
            .map(|r| r.data)
    }

    /// The decoded data and image metadata ([`Stream::decoded_image_metered`]).
    pub fn decoded_image_metered(
        &self,
        image_params: &ImageDecodeParams,
        meter: &Meter<'_>,
    ) -> Result<FilterResult<'_>, DecodeFailure> {
        self.0
            .decoding
            .decode(self.data(), self.decryptor(), image_params, meter, false)
    }

    /// The decoded data read incrementally ([`Stream::decoded_reader`]).
    pub fn decoded_reader(&self) -> Option<DecodedReader<'_>> {
        self.0.decoding.reader(self.data(), self.decryptor())
    }

    /// Whether [`Self::decoded_reader`] reads the data
    /// ([`Stream::can_read_incrementally`]).
    pub fn can_read_incrementally(&self) -> bool {
        self.0.decoding.reader_filter().is_some()
    }

    /// The bytes this holds besides the document's file: a copy of the data, and
    /// those its JBIG2 globals hold.
    pub fn copied_bytes(&self) -> usize {
        let globals = self.0.decoding.steps.iter().map(|step| match step {
            Step::Jbig2(Some(globals)) => globals.copied_bytes(),
            _ => 0,
        });
        let copy = if self.0.copied { self.0.range.len() } else { 0 };
        copy + globals.sum::<usize>()
    }

    /// Whether `data` are this stream's stored bytes, not merely equal to them.
    #[cfg(feature = "unsafe")]
    pub(crate) fn holds(&self, data: &[u8]) -> bool {
        core::ptr::eq(self.data(), data)
    }
}

/// How a stream's data are decrypted and decoded, read from its dictionary once
/// ([`Self::of`]): decoding reads nothing else of the dictionary, nor any object of
/// its document, so a key no filter reads is never resolved.
#[derive(Clone, Debug)]
pub(crate) struct Decoding {
    /// [`Decoding::decryption_of`].
    decryption: Decryption,
    /// The filters, in order, each with what decoding it reads of its parameters.
    steps: SmallVec<[Step; 2]>,
}

/// How a stream's data are decrypted, by rules 5, 6, 8 and 10 of the crypt filter
/// rules (`crate::crypto`): `Ok(None)` where they are read as stored, else the object
/// whose key decrypts them and the name of the crypt filter that does, `None` for the
/// document's stream filter; [`DecodeFailure::Decryption`] where a named `/Crypt`
/// filter is not the first filter.
type Decryption = Result<Option<(ObjectIdentifier, Option<Box<[u8]>>)>, DecodeFailure>;

impl Decoding {
    /// How the data of a stream with dictionary `dict` decode as an image `height`
    /// rows high, 0 where none is given: its filters ([`FilterEntries`]), each with
    /// the keys of its parameters it reads ([`Step::of`]); the JBIG2 globals only if
    /// `globals`, so that a stream's globals name none. Where the dictionary leaves
    /// the data no decryption, no filter's parameters are read. What the data decide
    /// is not known here: the parameters of a filter after one that fails on the data,
    /// and DCT's `/ColorTransform` of data whose JPEG header is broken, are read as
    /// for data that decode, which read them.
    pub(crate) fn of(dict: &Dict<'_>, globals: bool, height: u32) -> Self {
        let filters = FilterEntries::of(dict);
        let decryption = Self::decryption_of(dict, &filters);
        let steps = match decryption {
            Err(_) => SmallVec::new(),
            Ok(_) => filters
                .applied()
                .map(|(filter, entry)| Step::of(filter, || entry.params(), globals, height))
                .collect(),
        };
        Self { decryption, steps }
    }

    /// How the data of a stream with dictionary `dict` and `filters` are decrypted
    /// ([`Decryption`]).
    fn decryption_of(dict: &Dict<'_>, filters: &FilterEntries<'_>) -> Decryption {
        let mut crypt = CryptFilters::of(dict);
        let mut applied = filters.applied().enumerate();
        while !crypt.decided() {
            let Some((i, (filter, entry))) = applied.next() else {
                break;
            };
            crypt.read(i, filter, || entry.params());
        }
        crypt.decryption()
    }

    /// The decoded data `data`, decrypted by `decryptor`, and their image metadata;
    /// if `strict`, [`DecodeFailure::StreamDecode`] where they are corrupt.
    fn decode<'d>(
        &self,
        data: &'d [u8],
        decryptor: Option<&Decryptor>,
        image_params: &ImageDecodeParams,
        meter: &Meter<'_>,
        strict: bool,
    ) -> Result<FilterResult<'d>, DecodeFailure> {
        let data = raw_data(data, decryptor, &self.decryption, meter, strict)?;

        let mut current: Option<FilterResult<'d>> = None;

        let mut applied = self
            .steps
            .iter()
            // Rule 9 of the crypt filter rules (`crate::crypto`): decoding passes over /Crypt.
            .filter(|step| !matches!(step, Step::Crypt))
            .peekable();
        while let Some(step) = applied.next() {
            // The last filter makes the image's samples.
            let expected = match applied.peek() {
                None => image_params.expected_len(),
                Some(_) => 0,
            };
            let new = step.apply(
                current.as_ref().map(|c| c.data.as_ref()).unwrap_or(&data),
                image_params,
                meter,
                strict,
                expected,
            )?;
            current = Some(new);
        }

        Ok(current.unwrap_or(FilterResult {
            data,
            image_data: None,
        }))
    }

    /// [`Stream::decoded_reader`] of `data`, decrypted by `decryptor`.
    fn reader<'d>(
        &self,
        data: &'d [u8],
        decryptor: Option<&Decryptor>,
    ) -> Option<DecodedReader<'d>> {
        let filter = self.reader_filter()?;
        // Rules 5 to 11 of the crypt filter rules (`crate::crypto`): the data, decrypted, or `None`.
        let data = match self.decryption.as_ref().ok()? {
            None => DecryptedReader::raw(Cow::Borrowed(data)),
            Some((id, name)) => {
                let target = target(name);
                if data.is_empty() {
                    if !decrypts_whole(decryptor, data, target) {
                        return None;
                    }
                    DecryptedReader::raw(Cow::Borrowed(data))
                } else {
                    decrypted_reader(decryptor, *id, data, target)?
                }
            }
        };
        match filter {
            None => Some(DecodedReader::raw(data)),
            Some(params) => DecodedReader::flate(data, &params),
        }
    }

    /// The predictor of the one Flate filter, `Some(None)` for no filter, `None` when
    /// the filters cannot be read incrementally.
    fn reader_filter(&self) -> Option<Option<PredictorParams>> {
        Self::reader_filter_of(&self.steps)
    }

    /// [`Self::reader_filter`] of `steps`.
    fn reader_filter_of(steps: &[Step]) -> Option<Option<PredictorParams>> {
        match steps {
            [] => Some(None),
            [Step::Flate(params)] => {
                let params = params.clone()?;
                if params.predictor != 1 {
                    Unpredictor::new(&params)?;
                }
                Some(Some(params))
            }
            _ => None,
        }
    }

    /// Whether the data are one JPEG, read as stored.
    #[cfg(feature = "images")]
    pub(crate) fn is_plain_jpeg(&self) -> bool {
        matches!(self.decryption, Ok(None)) && matches!(self.steps.as_slice(), [Step::Dct(_)])
    }
}

/// The steps of `filters` where they may allow [`Stream::decoded_reader`]: none, or
/// one Flate filter, whose parameters alone are read; `None`, none read, for others.
fn incremental(filters: &FilterEntries<'_>) -> Option<SmallVec<[Step; 2]>> {
    let mut applied = filters.applied();
    match (applied.next(), applied.next()) {
        (None, _) => Some(SmallVec::new()),
        (Some((filter @ Filter::FlateDecode, entry)), None) => Some(smallvec::smallvec![Step::of(
            filter,
            || entry.params(),
            true,
            0
        )]),
        _ => None,
    }
}

/// The layers [`StrictReader`] reads `steps` through, `None` where a filter has no
/// decoder that reads a piece at a time (a step `None`, whose parameters are not
/// read), or whole decoding refuses its predictor's parameters.
fn layers(steps: impl Iterator<Item = Option<Step>>) -> Option<Vec<Layer>> {
    // The parameters and the reversal of the predictor; `None` where whole decoding
    // refuses the parameters or only it reverses the predictor.
    let predicted = |predictor: Option<PredictorParams>| {
        let predictor = predictor?;
        let unpredictor = match predictor.predictor {
            1 => None,
            _ => Some(Box::new(Unpredictor::new(&predictor)?)),
        };
        Some((predictor, unpredictor))
    };
    let mut layers = Vec::new();
    for step in steps {
        layers.push(match step? {
            // Rule 9 of the crypt filter rules (`crate::crypto`): decoding passes over /Crypt.
            Step::Crypt => continue,
            Step::Flate(predictor) => {
                let (predictor, unpredictor) = predicted(predictor)?;
                Layer::Flate(predictor, unpredictor)
            }
            Step::Lzw(predictor) => {
                let (predictor, unpredictor) = predicted(predictor)?;
                Layer::Lzw(predictor, unpredictor)
            }
            Step::AsciiHex => Layer::AsciiHex,
            Step::Ascii85 => Layer::Ascii85,
            Step::RunLength => Layer::RunLength,
            Step::Ccitt(_) | Step::Jbig2(_) | Step::Dct(_) | Step::Jpx => return None,
        });
    }
    Some(layers)
}

/// What decides how a stream's data are decrypted, read from its dictionary and
/// filters one at a time ([`Decryption`]).
struct CryptFilters {
    /// The object whose key decrypts the data, by rules 8 and 10; `None` where the
    /// document does not decrypt them (an inline image, an xref stream), and no
    /// filter's `/Name` is read.
    id: Option<ObjectIdentifier>,
    /// The crypt filter the first filter names.
    named: Option<Box<[u8]>>,
    /// Whether a `/Crypt` filter names none.
    stored: bool,
    /// Whether a filter other than the first names a crypt filter.
    misplaced: bool,
}

impl CryptFilters {
    fn of(dict: &Dict<'_>) -> Self {
        // Rules 8 and 10: what the document does not decrypt, an inline image included.
        let id = dict.ctx().decryption().filter(|_| {
            !dict
                .get::<crate::object::String<'_>>(TYPE)
                .is_some_and(|ty| ty.as_ref() == XREF)
        });
        Self {
            id,
            named: None,
            stored: false,
            misplaced: false,
        }
    }

    /// Whether the decryption is decided whatever filters follow: where the document
    /// does not decrypt the data, or a crypt filter is misplaced. No filter is read
    /// then.
    fn decided(&self) -> bool {
        self.id.is_none() || self.misplaced
    }

    /// Reads applied filter `i`, `filter` with `params`, which are resolved only for
    /// a crypt filter.
    fn read<'a>(&mut self, i: usize, filter: Filter, params: impl FnOnce() -> Dict<'a>) {
        if filter != Filter::Crypt {
            return;
        }
        // Rules 5 and 6: the first filter may name a crypt filter; any /Crypt filter
        // that names none leaves the data as stored, unless the first one names one.
        match params().get::<Name<'_>>(NAME) {
            Some(name) if name.deref() != IDENTITY => {
                if i > 0 {
                    self.misplaced = true;
                } else {
                    self.named = Some(Box::from(name.deref()));
                }
            }
            _ => self.stored = true,
        }
    }

    fn decryption(self) -> Decryption {
        let Some(id) = self.id else {
            return Ok(None);
        };
        if self.misplaced {
            return Err(DecodeFailure::Decryption);
        }
        Ok(match self.named {
            Some(name) => Some((id, Some(name))),
            None if self.stored => None,
            None => Some((id, None)),
        })
    }
}

/// The crypt filter `name` names, `None` for the document's stream filter.
fn target(name: &Option<Box<[u8]>>) -> DecryptionTarget<'_> {
    match name {
        None => DecryptionTarget::Stream,
        Some(name) => DecryptionTarget::Named(name),
    }
}

/// Whether `decryptor`, none for a document that is not read from a file, decrypts
/// `data` whole.
fn decrypts_whole(
    decryptor: Option<&Decryptor>,
    data: &[u8],
    target: DecryptionTarget<'_>,
) -> bool {
    decryptor.is_none_or(|d| d.decrypts_whole(data, target))
}

/// A reader of `data` of object `id` decrypted by `decryptor`, as stored without one.
fn decrypted_reader<'d>(
    decryptor: Option<&Decryptor>,
    id: ObjectIdentifier,
    data: &'d [u8],
    target: DecryptionTarget<'_>,
) -> Option<DecryptedReader<'d>> {
    match decryptor {
        None => Some(DecryptedReader::raw(Cow::Borrowed(data))),
        Some(d) => d.reader(id, data, target),
    }
}

/// The stored `data`, decrypted by `decryptor` as `decryption` says, a counted piece at
/// a time; if `strict`, data that does not decrypt whole is a failure.
fn raw_data<'d>(
    data: &'d [u8],
    decryptor: Option<&Decryptor>,
    decryption: &Decryption,
    meter: &Meter<'_>,
    strict: bool,
) -> Result<Cow<'d, [u8]>, DecodeFailure> {
    let Some((id, name)) = decryption.as_ref().map_err(|e| *e)? else {
        return Ok(Cow::Borrowed(data));
    };
    let target = target(name);
    let whole = decrypts_whole(decryptor, data, target);
    // Rule 11 of the crypt filter rules (`crate::crypto`): no data reads as empty
    // wherever they decrypt at all.
    if (strict || data.is_empty()) && !whole {
        return Err(DecodeFailure::Decryption);
    }
    if data.is_empty() {
        return Ok(Cow::Borrowed(data));
    }
    let mut reader =
        decrypted_reader(decryptor, *id, data, target).ok_or(DecodeFailure::Decryption)?;
    if reader.plain().is_some() {
        return Ok(Cow::Borrowed(data));
    }
    reader
        .decrypt_all(meter)
        .map(Cow::Owned)
        .map_err(|_| DecodeFailure::Cancelled)
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
        warn!("attempted to skip a stream object");

        None
    }
}

impl<'a> Readable<'a> for Stream<'a> {
    fn read(r: &mut Reader<'a>, ctx: &ReaderContext<'a>) -> Option<Self> {
        let dict = r.read_with_context::<Dict<'_>>(ctx)?;
        Self::after_dict(r, dict)
    }
}

impl<'a> Stream<'a> {
    /// The stream of `dict`, read from `r` after the dictionary.
    pub(crate) fn after_dict(r: &mut Reader<'a>, dict: Dict<'a>) -> Option<Self> {
        let offset = r.offset();
        Self::by_length(r, &dict)?
            .or_else(|| {
                r.jump(offset);
                Self::searched_for(r, &dict)
            })
            .error_none("was unable to manually parse the stream")
    }

    /// The stream of `dict`, read from `r` after the dictionary, where its /Length
    /// leads to `endstream`, `r` after it; `None` for a stream of an external file,
    /// which is not read.
    pub(crate) fn by_length(r: &mut Reader<'a>, dict: &Dict<'a>) -> Option<Option<Self>> {
        Self::by_length_ending(r, dict, |r| {
            r.skip_white_spaces();
            r.forward_tag(b"endstream")
        })
    }

    /// [`Self::by_length`], where `ends` moves `r` from the end of the data past the
    /// white space and the `endstream` after them, if they are there.
    pub(crate) fn by_length_ending(
        r: &mut Reader<'a>,
        dict: &Dict<'a>,
        ends: impl FnOnce(&mut Reader<'a>) -> Option<()>,
    ) -> Option<Option<Self>> {
        if dict.contains_key(F) {
            warn!("encountered stream referencing external file, which is unsupported");

            return None;
        }
        Some(parse_proper(r, dict, ends))
    }

    /// The stream of `dict`, read from `r` after the dictionary, up to the first
    /// `endstream`, where /Length does not lead to one ([`Self::by_length`]).
    pub(crate) fn searched_for(r: &mut Reader<'a>, dict: &Dict<'a>) -> Option<Self> {
        warn!("failed to parse stream, trying to parse it manually");
        parse_fallback(r, dict)
    }
}

#[derive(Debug, Copy, Clone)]
/// A failure that can occur during decoding a data stream.
pub enum DecodeFailure {
    /// An image stream failed to decode.
    ImageDecode,
    /// A data stream failed to decode.
    StreamDecode,
    /// A failure occurred while decrypting a file.
    Decryption,
    /// An unknown failure occurred.
    Unknown,
    /// The hook of the [`Meter`] counting the work asked to stop.
    Cancelled,
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

fn parse_proper<'a>(
    r: &mut Reader<'a>,
    dict: &Dict<'a>,
    ends: impl FnOnce(&mut Reader<'a>) -> Option<()>,
) -> Option<Stream<'a>> {
    let length = dict.get::<u32>(LENGTH)?;

    r.skip_white_spaces_and_comments();
    r.forward_tag(b"stream")?;
    r.forward_tag(b"\n")
        .or_else(|| r.forward_tag(b"\r\n"))
        .or_else(|| r.forward_tag(b"\r"))?;
    let data = r.read_bytes(length as usize)?;
    ends(r)?;

    Some(Stream::new(data, dict.clone()))
}

fn parse_fallback<'a>(r: &mut Reader<'a>, dict: &Dict<'a>) -> Option<Stream<'a>> {
    let stream_offset = searched(r.tail()?, b"stream")?;
    r.read_bytes(stream_offset)?;
    r.forward_tag(b"stream")?;

    r.forward_tag(b"\n")
        .or_else(|| r.forward_tag(b"\r\n"))
        // Technically not allowed, but no reason to not try it.
        .or_else(|| r.forward_tag(b"\r"))?;

    let tail = r.tail()?;
    let endstream_offset = searched(tail, b"endstream")?;
    let data_end = trim_trailing_ascii_whitespace(&tail[..endstream_offset]);
    let data = tail.get(..data_end)?;

    r.read_bytes(endstream_offset)?;
    r.skip_white_spaces();
    r.forward_tag(b"endstream")?;

    Some(Stream::new(data, dict.clone()))
}

pub(crate) fn searched(data: &[u8], needle: &[u8]) -> Option<usize> {
    let found = find_needle(data, needle);
    crate::work::told(found.unwrap_or(data.len()));
    #[cfg(test)]
    SEARCHED.set(SEARCHED.get() + found.unwrap_or(data.len()));
    found
}

/// Where each `needle` is in `data`, as [`searched`] finds them one after another,
/// searching all of `data`.
pub(crate) fn searched_all<'a>(
    data: &'a [u8],
    needle: &'a [u8],
) -> impl Iterator<Item = usize> + 'a {
    crate::work::told(data.len());
    #[cfg(test)]
    SEARCHED.set(SEARCHED.get() + data.len());
    crate::util::find_all(data, needle)
}

#[cfg(test)]
std::thread_local! {
    /// The bytes searched for the keywords of streams whose /Length is wrong on this
    /// thread, which the tests bound to show that a file is searched in linear time.
    pub(crate) static SEARCHED: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

pub(crate) fn trim_trailing_ascii_whitespace(data: &[u8]) -> usize {
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

impl<'a> ObjectLike<'a> for Stream<'a> {
    const STREAM: bool = true;
}
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
    use crate::object::{Dict, Stream, Written};
    use crate::reader::Reader;
    use crate::reader::{ReaderContext, ReaderExt};

    #[test]
    fn display() {
        let mut reader = Reader::new(b"<< /Length 3 >> stream\nabc\nendstream");
        let stream = reader
            .read_with_context::<Stream<'_>>(&ReaderContext::dummy())
            .unwrap();

        assert_eq!(format!("{stream}"), "Stream (len: 3)");
    }

    #[test]
    fn stream() {
        let data = b"<< /Length 10 >> stream\nabcdefghij\nendstream";
        let mut r = Reader::new(data);
        let stream = r
            .read_with_context::<Stream<'_>>(&ReaderContext::dummy())
            .unwrap();

        assert_eq!(stream.data, b"abcdefghij");
    }

    #[test]
    fn stream_fallback() {
        let data = b"<< /Length 999 >> stream\nabcdefghij\nendstream";
        let mut r = Reader::new(data);
        let stream = r
            .read_with_context::<Stream<'_>>(&ReaderContext::dummy())
            .unwrap();

        assert_eq!(stream.data, b"abcdefghij");
    }

    // Reading a stream's filters with their parameters parses each of the entries it
    // reads once: the dictionaries and arrays parsed are those of reading each entry,
    // and each item of an array, once, as written.
    #[test]
    fn filter_entries_parse_each_entry_once() {
        use crate::object::dict::PARSED;
        let cases: [(&str, &[&[u8]]); 3] = [
            ("/F /AHx /DP << /K -1 /Columns 16 >>", &[b"F", b"DP"]),
            (
                "/Filter [/AHx /Fl] /DecodeParms [null << /Predictor 1 >>]",
                &[b"Filter", b"DecodeParms"],
            ),
            (
                "/F [/AHx] /DP null /DecodeParms [<< /A 1 >>]",
                &[b"F", b"DP", b"DecodeParms"],
            ),
        ];
        for (entries, read) in cases {
            // A dictionary of its own: in a stream object's, /F names a file.
            let dict = format!("<< {entries} >>");
            let dict = Reader::new(dict.as_bytes())
                .read_with_context::<Dict<'_>>(&ReaderContext::dummy())
                .unwrap();
            let stream = Stream::new(b"a", dict);
            PARSED.set((0, 0));
            for key in read {
                if let Written::Array(array) = stream.dict.get_written(key).unwrap() {
                    array.written().for_each(drop);
                }
            }
            let once = PARSED.get();
            PARSED.set((0, 0));
            let filters = stream.filter_entries();
            assert!(filters.entries().all(|e| e.filter.is_some()), "{entries}");
            assert_eq!(PARSED.get(), once, "{entries}");
        }
    }
}
