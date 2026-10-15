//! Streams.

use crate::crypto::DecryptionTarget;
use crate::crypto::reader::DecryptedReader;
use crate::filter::Filter;
use crate::filter::lzw_flate::{PredictorParams, Unpredictor};
#[cfg(feature = "unsafe")]
pub use crate::filter::reader::DecodedCheckpoint;
pub use crate::filter::reader::{DecodedReader, ReadError};
use crate::object;
use crate::object::Dict;
use crate::object::Name;
use crate::object::dict::InlineImageDict;
use crate::object::dict::keys::{DECODE_PARMS, DP, F, FILTER, IDENTITY, LENGTH, NAME, TYPE};
use crate::object::{Array, ObjectIdentifier};
use crate::object::{Object, ObjectLike, ObjectRefLike};
use crate::reader::Reader;
use crate::reader::{Readable, ReaderContext, ReaderExt, Skippable};
use crate::sync::Arc;
use crate::trivia::is_white_space_character;
use crate::util::{OptionLog, find_needle};
use crate::work::Meter;
use crate::xref::{DocumentId, XRef};
use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::fmt::{Debug, Display, Formatter};
use core::ops::Deref;
use smallvec::SmallVec;

#[cfg(feature = "images")]
mod jpeg;
#[cfg(feature = "images")]
pub use jpeg::{JpegCheckpoint, JpegInput, JpegRows};

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
    /// A target resolution for the image. Note that this is only a hint so that
    /// in case it's possible, a version of the image will be extracted that
    /// is as close as possible to the hinted dimension.
    pub target_dimension: Option<(u32, u32)>,
    /// The width of the image as indicated by the image dictionary.
    pub width: u32,
    /// The height of the image as indicated by the image dictionary.
    pub height: u32,
}

impl<'a> Stream<'a> {
    pub(crate) fn new(data: &'a [u8], dict: Dict<'a>) -> Self {
        Self { dict, data }
    }

    fn filters_and_params(&self) -> FiltersAndParams<'a> {
        let mut collected_filters = SmallVec::new();
        let mut collected_params = SmallVec::new();

        if let Some(filter) = self
            .dict
            .get::<Name<'_>>(F)
            .or_else(|| self.dict.get::<Name<'_>>(FILTER))
            .and_then(Filter::from_name)
        {
            let params = self
                .dict
                .get::<Dict<'_>>(DP)
                .or_else(|| self.dict.get::<Dict<'_>>(DECODE_PARMS))
                .unwrap_or_default();

            collected_filters.push(filter);
            collected_params.push(params);
        } else if let Some(filters) = self
            .dict
            .get::<Array<'_>>(F)
            .or_else(|| self.dict.get::<Array<'_>>(FILTER))
        {
            let filters = filters.iter::<Name<'_>>().map(Filter::from_name);
            let mut params = self
                .dict
                .get::<Array<'_>>(DP)
                .or_else(|| self.dict.get::<Array<'_>>(DECODE_PARMS))
                .map(|a| a.iter::<Object<'_>>());

            for filter in filters {
                let params = params
                    .as_mut()
                    .and_then(|p| p.next())
                    .and_then(|p| p.into_dict())
                    .unwrap_or_default();

                if let Some(filter) = filter {
                    collected_filters.push(filter);
                    collected_params.push(params);
                }
            }
        }

        FiltersAndParams {
            filters: collected_filters,
            params: collected_params,
        }
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
        let Some((id, name)) = self.decryption()? else {
            return Ok(Cow::Borrowed(self.data));
        };
        let target = Self::target(&name);
        let xref = self.dict.ctx().xref();
        if self.data.is_empty() {
            // No data has no IV to decrypt with, and holds nothing.
            Ok(Cow::Borrowed(self.data))
        } else if !xref.decrypts_whole(self.data, target) {
            Err(DecodeFailure::Decryption)
        } else {
            xref.decrypt(id, self.data, target)
                .map(Cow::Owned)
                .ok_or(DecodeFailure::Decryption)
        }
    }

    /// [`Self::raw_data`], decrypted a counted piece at a time; if `strict`, data that
    /// does not decrypt whole is a failure.
    fn raw_data_metered(
        &self,
        meter: &Meter<'_>,
        strict: bool,
    ) -> Result<Cow<'a, [u8]>, DecodeFailure> {
        let Some((id, name)) = self.decryption()? else {
            return Ok(Cow::Borrowed(self.data));
        };
        let target = Self::target(&name);
        let xref = self.dict.ctx().xref();
        if self.data.is_empty() {
            return Ok(Cow::Borrowed(self.data));
        }
        if strict && !xref.decrypts_whole(self.data, target) {
            return Err(DecodeFailure::Decryption);
        }
        match xref.decrypted_reader(id, self.data, target) {
            Some(mut data) => data
                .decrypt_all(meter)
                .map(Cow::Owned)
                .map_err(|_| DecodeFailure::Cancelled),
            None => Err(DecodeFailure::Decryption),
        }
    }

    // How the data is decrypted: `None` where they are stored as is, else with the
    // key of the object, by the document's stream filter (no name) or the crypt
    // filter named. In an encrypted document a stream that is no cross-reference
    // stream is decrypted by the document's stream filter, unless it has /Crypt
    // filters, which replace it (`pdf_open_raw_filter` in MuPDF's `pdf-stream.c`).
    // A /Crypt filter without /Name or named /Identity passes the data through;
    // another names one of the document's crypt filters, which decrypts the data
    // (`build_filter` there). It must be the first filter, as the PDF specification
    // requires: decrypting data another filter decoded is not supported.
    fn decryption(&self) -> Result<Option<(ObjectIdentifier, Option<Name<'a>>)>, DecodeFailure> {
        let ctx = self.dict.ctx();
        if !ctx.xref().is_encrypted()
            || self
                .dict
                .get::<object::String<'_>>(TYPE)
                .is_some_and(|t| t.as_ref() == b"XRef")
        {
            return Ok(None);
        }
        let FiltersAndParams { filters, params } = self.filters_and_params();
        if !filters.contains(&Filter::Crypt) {
            return Ok(ctx.decryption().map(|id| (id, None)));
        }
        let mut named = None;
        for (i, (filter, params)) in filters.iter().zip(&params).enumerate() {
            if *filter != Filter::Crypt {
                continue;
            }
            match params.get::<Name<'a>>(NAME) {
                Some(name) if name.deref() != IDENTITY => {
                    let id = ctx.decryption().filter(|_| i == 0);
                    named = Some((id.ok_or(DecodeFailure::Decryption)?, Some(name)));
                }
                _ => {}
            }
        }
        Ok(named)
    }

    fn target<'n>(name: &'n Option<Name<'_>>) -> DecryptionTarget<'n> {
        name.as_deref()
            .map_or(DecryptionTarget::Stream, DecryptionTarget::Named)
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
    /// process: its document and its object. `None` for a stream that is no object of
    /// a loaded document (an inline image), or on a target without 64-bit atomics.
    pub fn identity(&self) -> Option<(DocumentId, ObjectIdentifier)> {
        Some((self.dict.ctx().xref().document_id()?, self.dict.obj_id()?))
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
        self.filters_and_params().filters
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
        let filters_and_params = self.filters_and_params();

        let mut current: Option<FilterResult<'a>> = None;

        for (filter, params) in filters_and_params
            .filters
            .iter()
            .zip(filters_and_params.params.iter())
        {
            if *filter == Filter::Crypt {
                // Decrypted with the raw data.
                continue;
            }
            let new = filter.apply(
                current.as_ref().map(|c| c.data.as_ref()).unwrap_or(&data),
                params,
                image_params,
                meter,
                strict,
            )?;
            current = Some(new);
        }

        Ok(current.unwrap_or(FilterResult {
            data,
            image_data: None,
        }))
    }

    /// The decoded data, read incrementally, when the stream's filters allow it: no
    /// filter, or Flate with no predictor or a byte-aligned one. It yields the bytes
    /// [`Stream::decoded`] returns, without holding them all. `None` for other
    /// filters, or where decryption would fail.
    pub fn decoded_reader(&self) -> Option<DecodedReader<'a>> {
        let filter = self.reader_filter()?;
        let data = match self.decryption().ok()? {
            Some((id, name)) => {
                self.dict
                    .ctx()
                    .xref()
                    .decrypted_reader(id, self.data, Self::target(&name))?
            }
            None => DecryptedReader::raw(Cow::Borrowed(self.data)),
        };
        match filter {
            None => Some(DecodedReader::raw(data)),
            Some(params) => DecodedReader::flate(data, &params),
        }
    }

    /// Whether the filters and predictor allow [`Stream::decoded_reader`], judged from
    /// the dictionary alone: no data is read or decrypted.
    pub fn can_read_incrementally(&self) -> bool {
        self.reader_filter().is_some()
    }

    /// The predictor of the one Flate filter, `Some(None)` for no filter, `None` when
    /// the filters cannot be read incrementally.
    fn reader_filter(&self) -> Option<Option<PredictorParams>> {
        let FiltersAndParams { filters, params } = self.filters_and_params();
        match filters.as_slice() {
            [] => Some(None),
            [Filter::FlateDecode] => {
                let params = PredictorParams::from_params(&params[0]);
                if params.predictor != 1 {
                    Unpredictor::new(&params)?;
                }
                Some(Some(params))
            }
            _ => None,
        }
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
    /// The stream of the same bytes, read in `ctx`, which its data is decrypted in.
    pub(crate) fn in_context(&self, ctx: &ReaderContext<'a>) -> Self {
        Self {
            dict: self.dict.in_context(ctx),
            data: self.data,
        }
    }

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
    #[cfg(test)]
    SEARCHED.set(SEARCHED.get() + found.unwrap_or(data.len()));
    found
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
    use crate::object::Stream;
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
}
