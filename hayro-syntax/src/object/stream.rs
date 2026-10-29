//! Streams.

use crate::crypto::DecryptionTarget;
use crate::crypto::reader::DecryptedReader;
use crate::filter::Filter;
#[cfg(feature = "images")]
use crate::filter::ccitt::CcittBytes;
use crate::filter::chain::Layer;
pub use crate::filter::chain::StrictReader;
use crate::filter::lzw_flate::{PredictorParams, Unpredictor};
use crate::filter::reader::Codec;
#[cfg(feature = "unsafe")]
pub use crate::filter::reader::DecodedCheckpoint;
pub use crate::filter::reader::{DecodedReader, ReadError};
use crate::object::Dict;
use crate::object::Name;
use crate::object::ObjectIdentifier;
use crate::object::dict::InlineImageDict;
use crate::object::dict::keys::{DECODE_PARMS, DP, F, FILTER, IDENTITY, LENGTH, NAME, TYPE, XREF};
use crate::object::{Array, Object, ObjectLike, ObjectRefLike, Written};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};
use crate::sync::Arc;
use crate::trivia::is_white_space_character;
use crate::util::{OptionLog, find_needle};
use crate::work::{Cancelled, Meter};
use crate::xref::{StreamId, XRef};
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
///    parameters as written and resolved, and whether they are reached through a
///    reference, so that a writer can copy the entries it keeps.
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
    /// The parameters of a filter given by name: as written, and resolved.
    One(Written<'a>, Dict<'a>),
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
    /// The parameters, resolved: empty where they are no dictionary.
    pub params: Dict<'a>,
    /// Whether the parameters are reached through a reference: their entry, or
    /// the array of parameters it is in.
    pub params_by_reference: bool,
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
            (Filters::One(..), Some(written)) => {
                let params = resolved::<Dict<'a>>(&written, ctx).unwrap_or_default();
                Params::One(written, params)
            }
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
                let (params_written, params) = match params {
                    Params::One(written, params) => (Some(written.clone()), params.clone()),
                    _ => (None, Dict::empty()),
                };
                Some(FilterEntry {
                    filter: *filter,
                    written: written.clone(),
                    params_by_reference: matches!(params_written, Some(Written::Ref(_))),
                    params_written,
                    params,
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
            let (params_written, params, params_by_reference) = match &mut params {
                Some((items, ctx, by_reference)) => match items.next() {
                    Some(item) => {
                        let params = resolved::<Dict<'a>>(&item, ctx).unwrap_or_default();
                        let by_reference = *by_reference || matches!(item, Written::Ref(_));
                        (Some(item), params, by_reference)
                    }
                    None => (None, Dict::empty(), false),
                },
                None => (None, Dict::empty(), false),
            };
            FilterEntry {
                filter: name.and_then(Filter::from_name),
                written,
                params_written,
                params,
                params_by_reference,
            }
        }))
    }

    /// The entries that name a filter, the ones decoding applies.
    pub fn applied(&self) -> impl Iterator<Item = (Filter, FilterEntry<'a>)> + '_ {
        self.entries()
            .filter_map(|entry| Some((entry.filter?, entry)))
    }

    /// [`Self::applied`], each entry read, applied or not, counted on `meter` before
    /// it is, so that reading any number of them polls its hook: an `Err` once it
    /// asks to stop.
    pub fn applied_counted<'m>(
        &'m self,
        meter: &'m Meter<'_>,
    ) -> impl Iterator<Item = Result<(Filter, FilterEntry<'a>), Cancelled>> + 'm {
        self.entries()
            .filter_map(move |entry| match meter.work(ENTRY_WORK) {
                Ok(()) => Some(Ok((entry.filter?, entry))),
                Err(stop) => Some(Err(stop)),
            })
    }
}

/// The work an entry of a stream's filters counts as it is read: the entry made of
/// its name and parameters.
const ENTRY_WORK: usize = size_of::<FilterEntry<'static>>();

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
        self.raw_data_metered(&Meter::default(), true)
    }

    /// [`Self::raw_data`], decrypted a counted piece at a time; if `strict`, data that
    /// does not decrypt whole is a failure.
    fn raw_data_metered(
        &self,
        meter: &Meter<'_>,
        strict: bool,
    ) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        let Some((id, name)) = self.decryption(meter)? else {
            return Ok(Cow::Borrowed(self.data));
        };
        let xref = self.dict.ctx().xref();
        let target = Self::target(&name);
        let whole = xref.decrypts_whole(self.data, target);
        // Rule 11 of the crypt filter rules (`crate::crypto`): no data reads as empty
        // wherever they decrypt at all.
        if (strict || self.data.is_empty()) && !whole {
            return Err(DecodeFailure::Decryption);
        }
        if self.data.is_empty() {
            return Ok(Cow::Borrowed(self.data));
        }
        let mut reader = xref
            .decrypted_reader(id, self.data, target)
            .ok_or(DecodeFailure::Decryption)?;
        if reader.plain().is_some() {
            return Ok(Cow::Borrowed(self.data));
        }
        reader
            .decrypt_all(meter)
            .map(Cow::Owned)
            .map_err(|_| DecodeFailure::Cancelled)
    }

    /// How the data are decrypted, by rules 5, 6, 8 and 10 of the crypt filter rules
    /// (`crate::crypto`): `Ok(None)` where they are read as stored, else the object
    /// whose key decrypts them and the name of the crypt filter that does, `None`
    /// for the document's stream filter; [`DecodeFailure::Decryption`] where a named
    /// `/Crypt` filter is not the first filter; [`DecodeFailure::Cancelled`] once
    /// `meter`'s hook asks to stop as the filters are read.
    fn decryption(
        &self,
        meter: &Meter<'_>,
    ) -> Result<Option<(ObjectIdentifier, Option<Name<'a>>)>, DecodeFailure> {
        // Rules 8 and 10: what the document does not decrypt, an inline image included.
        let Some(id) = self.dict.ctx().decryption() else {
            return Ok(None);
        };
        if self
            .dict
            .get::<crate::object::String<'_>>(TYPE)
            .is_some_and(|ty| ty.as_ref() == XREF)
        {
            return Ok(None);
        }
        // Rules 5 and 6: the first filter may name a crypt filter; any /Crypt filter
        // that names none leaves the data as stored, unless the first one names one.
        let mut named = None;
        let mut stored = false;
        let filters = self.filter_entries();
        for (i, applied) in filters.applied_counted(meter).enumerate() {
            let (filter, entry) = applied.map_err(|_| DecodeFailure::Cancelled)?;
            if filter != Filter::Crypt {
                continue;
            }
            match entry.params.get::<Name<'a>>(NAME) {
                Some(name) if name.deref() != IDENTITY => {
                    if i > 0 {
                        return Err(DecodeFailure::Decryption);
                    }
                    named = Some(name);
                }
                _ => stored = true,
            }
        }
        Ok(match named {
            Some(name) => Some((id, Some(name))),
            None if stored => None,
            None => Some((id, None)),
        })
    }

    fn target<'n>(name: &'n Option<Name<'_>>) -> DecryptionTarget<'n> {
        match name {
            None => DecryptionTarget::Stream,
            Some(name) => DecryptionTarget::Named(name.deref()),
        }
    }

    /// The stream's data as the file stores them, neither decrypted nor decoded.
    pub(crate) fn stored(&self) -> &'a [u8] {
        self.data
    }

    /// Return the raw, underlying dictionary of the stream.
    pub fn dict(&self) -> &Dict<'a> {
        &self.dict
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
        self.decode(&ImageDecodeParams::default(), &Meter::default(), true)
            .map(|r| r.data)
    }

    /// [`Self::decoded_strict`], its work counted on `meter`, a bounded amount at a
    /// time: [`DecodeFailure::Cancelled`] once the meter's hook asks to stop.
    pub fn decoded_strict_metered(
        &self,
        meter: &Meter<'_>,
    ) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        self.decode(&ImageDecodeParams::default(), meter, true)
            .map(|r| r.data)
    }

    /// [`Self::decoded_image`], its work counted on `meter`, a bounded amount at a
    /// time: [`DecodeFailure::Cancelled`] once the meter's hook asks to stop.
    pub fn decoded_image_metered(
        &self,
        image_params: &ImageDecodeParams,
        meter: &Meter<'_>,
    ) -> Result<FilterResult<'a>, DecodeFailure> {
        self.decode(image_params, meter, false)
    }

    fn decode(
        &self,
        image_params: &ImageDecodeParams,
        meter: &Meter<'_>,
        strict: bool,
    ) -> Result<FilterResult<'a>, DecodeFailure> {
        let data = self.raw_data_metered(meter, strict)?;
        let filters = self.filter_entries();

        let mut current: Option<FilterResult<'a>> = None;

        let mut applied = filters.applied_counted(meter).peekable();
        while let Some(next) = applied.next() {
            let (filter, entry) = next.map_err(|_| DecodeFailure::Cancelled)?;
            if filter == Filter::Crypt {
                // Rule 9 of the crypt filter rules (`crate::crypto`): decoding passes over /Crypt.
                continue;
            }
            // The last filter makes the image's samples.
            let expected = match applied.peek() {
                None => image_params.expected_len(),
                Some(_) => 0,
            };
            let new = filter.apply(
                current.as_ref().map(|c| c.data.as_ref()).unwrap_or(&data),
                &entry.params,
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
        let filters = self.filter_entries();
        let mut layers = Vec::new();
        for applied in filters.applied_counted(meter) {
            let (filter, entry) = applied.map_err(|_| DecodeFailure::Cancelled)?;
            let params = &entry.params;
            // The parameters and the reversal of the predictor; `None` where whole
            // decoding refuses the parameters or only it reverses the predictor.
            let predicted = || {
                let predictor = PredictorParams::from_params(params)?;
                let unpredictor = match predictor.predictor {
                    1 => None,
                    _ => Some(Box::new(Unpredictor::new(&predictor)?)),
                };
                Some((predictor, unpredictor))
            };
            layers.push(match filter {
                // Rule 9 of the crypt filter rules (`crate::crypto`): decoding passes over /Crypt.
                Filter::Crypt => continue,
                Filter::FlateDecode => {
                    let Some((predictor, unpredictor)) = predicted() else {
                        return Ok(None);
                    };
                    Layer::Flate(predictor, unpredictor)
                }
                Filter::LzwDecode => {
                    let Some((predictor, unpredictor)) = predicted() else {
                        return Ok(None);
                    };
                    Layer::Lzw(predictor, unpredictor)
                }
                Filter::AsciiHexDecode => Layer::AsciiHex,
                Filter::Ascii85Decode => Layer::Ascii85,
                Filter::RunLengthDecode => Layer::RunLength,
                Filter::CcittFaxDecode
                | Filter::Jbig2Decode
                | Filter::DctDecode
                | Filter::JpxDecode => return Ok(None),
            });
        }
        let data = self.raw_data_metered(meter, true)?;
        Ok(Some(StrictReader::new(data, layers)))
    }

    /// The image data [`Self::decoded_image`] returns, read incrementally where the
    /// filters allow it: those [`Self::decoded_reader`] reads, and `CCITTFaxDecode` as
    /// the last filter after any of those, whose rows decode as they are read, the
    /// reader holding the decoder's state and a row's colour changes, and of
    /// encrypted or decoded data a window of it ([`Self::ccitt_image_data`] tells what
    /// they are). `None` otherwise, or where decryption would fail. Reading the
    /// filters and making their decoders counts on `meter`, and fails once its hook
    /// asks to stop.
    #[cfg(feature = "images")]
    pub fn image_reader(
        &self,
        image_params: &ImageDecodeParams,
        meter: &Meter<'_>,
    ) -> Result<Option<DecodedReader<'a>>, Cancelled> {
        let Some((entry, before)) = self.ccitt_filters(meter)? else {
            return self.decoded_reader(meter);
        };
        let Some(data) = self.decrypted(meter)? else {
            return Ok(None);
        };
        let bytes = match before.is_empty() {
            true => CcittBytes::new(data, &entry.params, image_params),
            false => {
                let Some(reader) = DecodedReader::decoding(data, &before, meter)? else {
                    return Ok(None);
                };
                CcittBytes::decoded(reader, &entry.params, image_params)
            }
        };
        Ok(Some(DecodedReader::ccitt(bytes)))
    }

    /// What [`Self::decoded_image`] reports of the samples of an image whose last
    /// filter is `CCITTFaxDecode`, after filters [`Self::decoded_reader`] reads, from
    /// the dictionary alone: 8-bit gray samples, or of a bit for an indexed image, in
    /// rows of its `Columns`, as many as the image declares. `None` for other filters.
    /// Reading the filters counts on `meter`.
    #[cfg(feature = "images")]
    pub fn ccitt_image_data(
        &self,
        image_params: &ImageDecodeParams,
        meter: &Meter<'_>,
    ) -> Result<Option<ImageData>, Cancelled> {
        Ok(self
            .ccitt_filters(meter)?
            .map(|(entry, _)| crate::filter::ccitt::image_data(&entry.params, image_params)))
    }

    /// Whether the filters allow [`Self::image_reader`], judged from the dictionary
    /// alone: no data is read or decrypted. Reading the filters counts on `meter`.
    #[cfg(feature = "images")]
    pub fn image_reads_incrementally(&self, meter: &Meter<'_>) -> Result<bool, Cancelled> {
        Ok(self.ccitt_filters(meter)?.is_some() || self.can_read_incrementally(meter)?)
    }

    /// The entry of the last filter (besides `/Crypt`), if it is `CCITTFaxDecode`
    /// and [`DecodedReader`] reads the filters before it, and their decoders; each
    /// entry read counts on `meter`.
    #[cfg(feature = "images")]
    fn ccitt_filters(
        &self,
        meter: &Meter<'_>,
    ) -> Result<Option<(FilterEntry<'a>, Vec<Codec>)>, Cancelled> {
        let filters = self.filter_entries();
        let mut before = Vec::new();
        let mut last = None;
        for applied in filters.applied_counted(meter) {
            let (filter, entry) = applied?;
            if filter == Filter::Crypt {
                continue;
            }
            // A filter before the last one is decoded as it is read.
            if let Some((filter, entry)) = last.replace((filter, entry)) {
                let Some(codec) = Self::codec(filter, &entry) else {
                    return Ok(None);
                };
                meter.reserve(&mut before, 1);
                before.push(codec);
            }
        }
        Ok(match last {
            Some((Filter::CcittFaxDecode, entry)) => Some((entry, before)),
            _ => None,
        })
    }

    /// The decoded data, read incrementally, when the stream's filters allow it: any
    /// number of Flate, LZW, ASCII hex, ASCII base-85 and run length filters, with no
    /// predictor, a PNG predictor, or the TIFF predictor of components of up to 8
    /// bits; the last 16 each read as the one before it decodes, any before them
    /// decoded whole on the first read. It yields the bytes [`Stream::decoded`]
    /// returns, without holding them all. `None` for other filters, or where
    /// decryption would fail. Reading the filters and making their decoders counts
    /// on `meter`, and fails once its hook asks to stop.
    pub fn decoded_reader(
        &self,
        meter: &Meter<'_>,
    ) -> Result<Option<DecodedReader<'a>>, Cancelled> {
        let Some(codecs) = self.codecs(meter)? else {
            return Ok(None);
        };
        match self.decrypted(meter)? {
            Some(data) => DecodedReader::decoding(data, &codecs, meter),
            None => Ok(None),
        }
    }

    /// The stored data, decrypted as it is read, as lenient decoding decrypts it;
    /// `None` where that fails. Reading the filters counts on `meter`.
    fn decrypted(&self, meter: &Meter<'_>) -> Result<Option<DecryptedReader<'a>>, Cancelled> {
        let decryption = match self.decryption(meter) {
            Ok(decryption) => decryption,
            Err(DecodeFailure::Cancelled) => return Err(Cancelled),
            Err(_) => return Ok(None),
        };
        Ok(self.decrypted_by(decryption))
    }

    fn decrypted_by(
        &self,
        decryption: Option<(ObjectIdentifier, Option<Name<'a>>)>,
    ) -> Option<DecryptedReader<'a>> {
        // Rules 5 to 11 of the crypt filter rules (`crate::crypto`): the data, decrypted, or `None`.
        Some(match decryption {
            None => DecryptedReader::raw(Cow::Borrowed(self.data)),
            Some((id, name)) => {
                let xref = self.dict.ctx().xref();
                let target = Self::target(&name);
                if self.data.is_empty() {
                    if !xref.decrypts_whole(self.data, target) {
                        return None;
                    }
                    DecryptedReader::raw(Cow::Borrowed(self.data))
                } else {
                    xref.decrypted_reader(id, self.data, target)?
                }
            }
        })
    }

    /// Whether the filters and predictor allow [`Stream::decoded_reader`], judged from
    /// the dictionary alone: no data is read or decrypted. Reading the filters counts
    /// on `meter`.
    pub fn can_read_incrementally(&self, meter: &Meter<'_>) -> Result<bool, Cancelled> {
        Ok(self.codecs(meter)?.is_some())
    }

    /// The decoders [`DecodedReader`] reads the filters with, in turn, passing over
    /// `/Crypt` as decoding does (rule 9 of the crypt filter rules, `crate::crypto`);
    /// `None` where a filter is one only whole decoding decodes, an image filter or a
    /// predictor only it reverses, or where it refuses a predictor's parameters. Each
    /// entry read counts on `meter`.
    fn codecs(&self, meter: &Meter<'_>) -> Result<Option<Vec<Codec>>, Cancelled> {
        let filters = self.filter_entries();
        let mut codecs = Vec::new();
        for applied in filters.applied_counted(meter) {
            let (filter, entry) = applied?;
            if filter == Filter::Crypt {
                continue;
            }
            let Some(codec) = Self::codec(filter, &entry) else {
                return Ok(None);
            };
            meter.reserve(&mut codecs, 1);
            codecs.push(codec);
        }
        Ok(Some(codecs))
    }

    /// The decoder [`DecodedReader`] reads `filter` with, its parameters in `entry`;
    /// `None` as [`Self::codecs`] tells.
    fn codec(filter: Filter, entry: &FilterEntry<'a>) -> Option<Codec> {
        let predicted = || {
            let params = PredictorParams::from_params(&entry.params)?;
            if params.predictor != 1 {
                Unpredictor::new(&params)?;
            }
            Some(params)
        };
        Some(match filter {
            Filter::FlateDecode => Codec::Flate(predicted()?),
            Filter::LzwDecode => Codec::Lzw(predicted()?),
            Filter::AsciiHexDecode => Codec::AsciiHex,
            Filter::Ascii85Decode => Codec::Ascii85,
            Filter::RunLengthDecode => Codec::RunLength,
            Filter::Crypt
            | Filter::CcittFaxDecode
            | Filter::Jbig2Decode
            | Filter::DctDecode
            | Filter::JpxDecode => return None,
        })
    }

    /// An owned handle to this stream, which [`OwnedStream::get`] turns back into it
    /// without borrowing the data it was read from: the object's identifier in the
    /// document for an indirect object, a copy of the dictionary and data for an
    /// inline image.
    pub fn to_owned_stream(&self) -> OwnedStream {
        OwnedStream(match self.dict.obj_id() {
            Some(id) => OwnedInner::Indirect {
                xref: self.dict.ctx().xref().clone(),
                id,
            },
            None => OwnedInner::Inline {
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
    Indirect { xref: XRef, id: ObjectIdentifier },
    Inline { dict: Arc<[u8]>, data: Arc<[u8]> },
}

impl OwnedStream {
    /// The stream; `None` if it can no longer be read.
    pub fn get(&self) -> Option<Stream<'_>> {
        match &self.0 {
            OwnedInner::Indirect { xref, id } => xref.get::<Stream<'_>>(*id),
            OwnedInner::Inline { dict, data } => {
                let dict = Reader::new(dict).read_without_context::<InlineImageDict<'_>>()?;
                Some(Stream::new(data, dict.get_dict().clone()))
            }
        }
    }

    /// Whether `data` are this stream's stored bytes, not merely equal to them.
    #[cfg(feature = "unsafe")]
    pub(crate) fn holds(&self, data: &[u8]) -> bool {
        self.get()
            .is_some_and(|stream| core::ptr::eq(stream.data, data))
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
