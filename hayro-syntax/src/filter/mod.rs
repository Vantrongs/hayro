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

use crate::object::Dict;
use crate::object::Name;
use crate::object::dict::keys::*;
use crate::object::stream::{DecodeFailure, FilterResult, ImageDecodeParams};
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

    /// The filter applied to `data`, its work counted on `meter`;
    /// [`DecodeFailure::Cancelled`] once the meter's hook asks to stop. If `strict`,
    /// data a lenient decoder would recover part of is a failure.
    pub(crate) fn apply(
        &self,
        data: &[u8],
        params: &Dict<'_>,
        #[cfg_attr(not(feature = "images"), allow(unused))] image_params: &ImageDecodeParams,
        meter: &Meter<'_>,
        strict: bool,
    ) -> Result<FilterResult<'static>, DecodeFailure> {
        // `MuPDF` refuses a stream whose predictor parameters it refuses.
        let predictor =
            || lzw_flate::PredictorParams::from_params(params).ok_or(DecodeFailure::StreamDecode);
        let res =
            match self {
                Self::AsciiHexDecode => ascii_hex::decode(data, meter)
                    .map(FilterResult::from_data)
                    .ok_or(DecodeFailure::StreamDecode),
                Self::Ascii85Decode => ascii_85::decode(data, meter)
                    .map(FilterResult::from_data)
                    .ok_or(DecodeFailure::StreamDecode),
                Self::RunLengthDecode => run_length::decode(data, meter)
                    .map(FilterResult::from_data)
                    .ok_or(DecodeFailure::StreamDecode),
                Self::LzwDecode => predictor().and_then(|p| {
                    lzw_flate::lzw::decode(data, &p, meter)
                        .map(FilterResult::from_data)
                        .ok_or(DecodeFailure::StreamDecode)
                }),
                Self::FlateDecode => predictor().and_then(|p| {
                    lzw_flate::flate::decode(data, &p, meter, strict)
                        .map(FilterResult::from_data)
                        .ok_or(DecodeFailure::StreamDecode)
                }),
                #[cfg(feature = "images")]
                Self::DctDecode => {
                    dct::decode(data, params, image_params, meter).ok_or(DecodeFailure::ImageDecode)
                }
                #[cfg(feature = "images")]
                Self::CcittFaxDecode => ccitt::decode(data, params, image_params, meter)
                    .ok_or(DecodeFailure::ImageDecode),
                #[cfg(feature = "images")]
                Self::Jbig2Decode => jbig2::decode(data, params, image_params, meter)
                    .ok_or(DecodeFailure::ImageDecode),
                #[cfg(feature = "images")]
                Self::JpxDecode => {
                    jpx::decode(data, image_params, meter).ok_or(DecodeFailure::ImageDecode)
                }
                #[cfg(not(feature = "images"))]
                Self::DctDecode | Self::CcittFaxDecode | Self::Jbig2Decode | Self::JpxDecode => {
                    warn!("image decoding is not supported (enable the `images` feature)");
                    Err(DecodeFailure::ImageDecode)
                }
                _ => Err(DecodeFailure::StreamDecode),
            };

        // A decoder may end early on a stop as on data cut short.
        if meter.cancelled() {
            return Err(DecodeFailure::Cancelled);
        }

        if res.is_err() {
            warn!("failed to apply filter {}", self.debug_name());
        }

        res
    }
}
