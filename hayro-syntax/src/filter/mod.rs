//! Results from decoding filtered data streams.

mod ascii_85;
pub(crate) mod ascii_hex;
#[cfg(feature = "images")]
mod ccitt;
pub(crate) mod chain;
#[cfg(feature = "images")]
mod dct;
#[cfg(feature = "images")]
mod jbig2;
#[cfg(feature = "images")]
mod jpx;
pub(crate) mod lzw_flate;
mod png;
pub(crate) mod reader;
mod run_length;

use crate::filter::lzw_flate::PredictorParams;
use crate::object::dict::keys::*;
use crate::object::stream::{DecodeFailure, FilterResult, ImageDecodeParams, OwnedStream};
use crate::object::{Dict, Name, Stream};
use crate::work::Meter;
use core::ops::Deref;

/// A data filter.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Filter {
    /// ASCII hexadecimal encoding.
    AsciiHexDecode,
    /// ASCII base-85 encoding.
    Ascii85Decode,
    /// Lempel-Ziv-Welch (LZW) compression.
    LzwDecode,
    /// DEFLATE compression (zlib/gzip).
    FlateDecode,
    /// Run-length encoding compression.
    RunLengthDecode,
    /// CCITT Group 3 or Group 4 fax compression.
    CcittFaxDecode,
    /// JBIG2 compression for bi-level images.
    Jbig2Decode,
    /// JPEG (DCT) compression.
    DctDecode,
    /// JPEG 2000 compression.
    JpxDecode,
    /// Encryption filter.
    Crypt,
}

impl Filter {
    fn debug_name(&self) -> &'static str {
        match self {
            Self::AsciiHexDecode => "ascii_hex",
            Self::Ascii85Decode => "ascii_85",
            Self::LzwDecode => "lzw",
            Self::FlateDecode => "flate",
            Self::RunLengthDecode => "run-length",
            Self::CcittFaxDecode => "ccit_fax",
            Self::Jbig2Decode => "jbig2",
            Self::DctDecode => "dct",
            Self::JpxDecode => "jpx",
            Self::Crypt => "crypt",
        }
    }

    pub(crate) fn from_name(name: Name<'_>) -> Option<Self> {
        match name.deref() {
            ASCII_HEX_DECODE | ASCII_HEX_DECODE_ABBREVIATION => Some(Self::AsciiHexDecode),
            ASCII85_DECODE | ASCII85_DECODE_ABBREVIATION => Some(Self::Ascii85Decode),
            LZW_DECODE | LZW_DECODE_ABBREVIATION => Some(Self::LzwDecode),
            FLATE_DECODE | FLATE_DECODE_ABBREVIATION => Some(Self::FlateDecode),
            RUN_LENGTH_DECODE | RUN_LENGTH_DECODE_ABBREVIATION => Some(Self::RunLengthDecode),
            CCITTFAX_DECODE | CCITTFAX_DECODE_ABBREVIATION => Some(Self::CcittFaxDecode),
            JBIG2_DECODE => Some(Self::Jbig2Decode),
            DCT_DECODE | DCT_DECODE_ABBREVIATION => Some(Self::DctDecode),
            JPX_DECODE => Some(Self::JpxDecode),
            CRYPT => Some(Self::Crypt),
            _ => {
                warn!("unknown filter: {}", name.as_str());

                None
            }
        }
    }
}

/// A filter with what decoding it reads of its parameters, read from them once
/// ([`Step::of`]): decoding reads nothing else of a stream's dictionary, so a key no
/// filter reads is never resolved, and decoding needs no reader of the document.
// Without image decoders, nothing reads the parameters of image filters.
#[cfg_attr(not(feature = "images"), allow(dead_code))]
#[derive(Clone, Debug)]
pub(crate) enum Step {
    AsciiHex,
    Ascii85,
    RunLength,
    /// The predictor, `None` where its parameters are refused
    /// ([`PredictorParams::from_params`]).
    Lzw(Option<PredictorParams>),
    /// As [`Self::Lzw`].
    Flate(Option<PredictorParams>),
    Ccitt(CcittParams),
    /// `/ColorTransform`.
    Dct(Option<u8>),
    /// `/JBIG2Globals`, where it is a stream.
    Jbig2(Option<OwnedStream>),
    Jpx,
    /// Passed over by decoding (rule 9 of the crypt filter rules, `crate::crypto`);
    /// its `/Name` decides the decryption ([`crate::object::stream::Decoding`]).
    Crypt,
}

impl Step {
    /// `filter` with what decoding it reads of `params`, for an image `height` rows
    /// high, 0 where none is given; JBIG2 globals only if `globals` (the globals of a
    /// JBIG2 image, not those of its globals). `params` are resolved only for a
    /// filter whose decoder reads them.
    pub(crate) fn of<'a>(
        filter: Filter,
        params: impl FnOnce() -> Dict<'a>,
        globals: bool,
        height: u32,
    ) -> Self {
        match filter {
            Filter::AsciiHexDecode => Self::AsciiHex,
            Filter::Ascii85Decode => Self::Ascii85,
            Filter::RunLengthDecode => Self::RunLength,
            Filter::LzwDecode => Self::Lzw(PredictorParams::from_params(&params())),
            Filter::FlateDecode => Self::Flate(PredictorParams::from_params(&params())),
            Filter::CcittFaxDecode => Self::Ccitt(CcittParams::of(&params(), height)),
            Filter::DctDecode => Self::Dct(params().get::<u8>(COLOR_TRANSFORM)),
            Filter::Jbig2Decode => Self::Jbig2(
                globals
                    .then(|| params().get::<Stream<'_>>(JBIG2_GLOBALS))
                    .flatten()
                    .map(|g| g.owned(false, 0)),
            ),
            Filter::JpxDecode => Self::Jpx,
            Filter::Crypt => Self::Crypt,
        }
    }

    pub(crate) fn filter(&self) -> Filter {
        match self {
            Self::AsciiHex => Filter::AsciiHexDecode,
            Self::Ascii85 => Filter::Ascii85Decode,
            Self::RunLength => Filter::RunLengthDecode,
            Self::Lzw(_) => Filter::LzwDecode,
            Self::Flate(_) => Filter::FlateDecode,
            Self::Ccitt(_) => Filter::CcittFaxDecode,
            Self::Dct(_) => Filter::DctDecode,
            Self::Jbig2(_) => Filter::Jbig2Decode,
            Self::Jpx => Filter::JpxDecode,
            Self::Crypt => Filter::Crypt,
        }
    }

    /// The filter applied to `data`, its work counted on `meter`;
    /// [`DecodeFailure::Cancelled`] once the meter's hook asks to stop. If `strict`,
    /// data a lenient decoder would recover part of is a failure. `expected` is the
    /// size the decoded data should have, 0 where none is known, which a decoder
    /// that grows its output reserves no more than while it made less.
    pub(crate) fn apply(
        &self,
        data: &[u8],
        #[cfg_attr(not(feature = "images"), allow(unused))] image_params: &ImageDecodeParams,
        meter: &Meter<'_>,
        strict: bool,
        expected: usize,
    ) -> Result<FilterResult<'static>, DecodeFailure> {
        // `MuPDF` refuses a stream whose predictor parameters it refuses.
        fn predictor(p: &Option<PredictorParams>) -> Result<&PredictorParams, DecodeFailure> {
            p.as_ref().ok_or(DecodeFailure::StreamDecode)
        }
        let res = match self {
            Self::AsciiHex => ascii_hex::decode(data, meter)
                .map(FilterResult::from_data)
                .ok_or(DecodeFailure::StreamDecode),
            Self::Ascii85 => ascii_85::decode(data, meter)
                .map(FilterResult::from_data)
                .ok_or(DecodeFailure::StreamDecode),
            Self::RunLength => run_length::decode(data, meter)
                .map(FilterResult::from_data)
                .ok_or(DecodeFailure::StreamDecode),
            Self::Lzw(p) => predictor(p).and_then(|p| {
                lzw_flate::lzw::decode(data, p, meter)
                    .map(FilterResult::from_data)
                    .ok_or(DecodeFailure::StreamDecode)
            }),
            Self::Flate(p) => predictor(p).and_then(|p| {
                lzw_flate::flate::decode(data, p, meter, strict, expected)
                    .map(FilterResult::from_data)
                    .ok_or(DecodeFailure::StreamDecode)
            }),
            #[cfg(feature = "images")]
            Self::Dct(color_transform) => dct::decode(data, *color_transform, image_params, meter)
                .ok_or(DecodeFailure::ImageDecode),
            #[cfg(feature = "images")]
            Self::Ccitt(params) => {
                ccitt::decode(data, params, image_params, meter).ok_or(DecodeFailure::ImageDecode)
            }
            #[cfg(feature = "images")]
            Self::Jbig2(globals) => jbig2::decode(data, globals.as_ref(), image_params, meter)
                .ok_or(DecodeFailure::ImageDecode),
            #[cfg(feature = "images")]
            Self::Jpx => jpx::decode(data, image_params, meter).ok_or(DecodeFailure::ImageDecode),
            #[cfg(not(feature = "images"))]
            Self::Dct(_) | Self::Ccitt(_) | Self::Jbig2(_) | Self::Jpx => {
                warn!("image decoding is not supported (enable the `images` feature)");
                Err(DecodeFailure::ImageDecode)
            }
            Self::Crypt => Err(DecodeFailure::StreamDecode),
        };

        // A decoder may end early on a stop as on data cut short.
        if meter.cancelled() {
            return Err(DecodeFailure::Cancelled);
        }

        if res.is_err() {
            warn!("failed to apply filter {}", self.filter().debug_name());
        }

        res
    }
}

/// What CCITT fax decoding reads of its parameters.
#[cfg_attr(not(feature = "images"), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct CcittParams {
    pub(crate) k: i32,
    pub(crate) columns: u32,
    /// The image's height where it is given, else `/Rows`, which then alone is read:
    /// `/Rows` should match the height, but where it does not, the height takes
    /// precedence (as in other viewers).
    pub(crate) rows: u32,
    pub(crate) end_of_block: bool,
    pub(crate) end_of_line: bool,
    pub(crate) encoded_byte_align: bool,
    pub(crate) black_is_1: bool,
}

impl CcittParams {
    /// What decoding an image `height` rows high, 0 where none is given, reads of
    /// `params`.
    pub(crate) fn of(params: &Dict<'_>, height: u32) -> Self {
        Self {
            k: params.get::<i32>(K).unwrap_or(0),
            columns: params.get::<usize>(COLUMNS).unwrap_or(1728) as u32,
            rows: match height {
                0 => params.get::<u32>(ROWS).unwrap_or(0),
                height => height,
            },
            end_of_block: params.get::<bool>(END_OF_BLOCK).unwrap_or(true),
            end_of_line: params.get::<bool>(END_OF_LINE).unwrap_or(false),
            encoded_byte_align: params.get::<bool>(ENCODED_BYTE_ALIGN).unwrap_or(false),
            black_is_1: params.get::<bool>(BLACK_IS_1).unwrap_or(false),
        }
    }
}
