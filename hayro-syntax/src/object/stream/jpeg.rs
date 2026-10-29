//! Grayscale JPEG data read in bands of rows, independent of the document's lifetime.

use super::OwnedStream;
use crate::PdfData;
use crate::work::Meter;
use core::ops::Range;
use zune_jpeg::rows::{GrayscaleCheckpoint, GrayscaleRows};
use zune_jpeg::zune_core::options::DecoderOptions;

/// A stream's stored bytes, owned: the bytes they lie in and where.
pub struct JpegInput {
    bytes: PdfData,
    range: Range<usize>,
}

impl AsRef<[u8]> for JpegInput {
    fn as_ref(&self) -> &[u8] {
        &self.bytes.as_ref()[self.range.clone()]
    }
}

/// The rows of a baseline grayscale JPEG, in bands of up to eight, their work counted
/// on a [`Meter`].
pub type JpegRows<'m> = GrayscaleRows<'m, JpegInput>;
/// A position in [`JpegRows`] to resume reading from, which keeps the bytes alive.
pub type JpegCheckpoint = GrayscaleCheckpoint<JpegInput>;

impl OwnedStream {
    /// The stream's rows, when its one filter is `DCTDecode`, it is not encrypted, and
    /// its data is a single-scan baseline grayscale JPEG of `dimensions`. `None` for
    /// other data, which is decoded whole, and once `meter`'s hook asks to stop.
    pub fn jpeg_rows<'m>(
        &self,
        dimensions: (u32, u32),
        meter: &'m Meter<'m>,
    ) -> Option<JpegRows<'m>> {
        let input = self.jpeg_input()?;
        let options = DecoderOptions::default()
            .set_max_width(u16::MAX as usize)
            .set_max_height(u16::MAX as usize);
        let rows = JpegRows::new(input, options, meter).ok()?;
        let (w, h) = dimensions;
        (rows.dimensions() == (w as usize, h as usize)).then_some(rows)
    }

    fn jpeg_input(&self) -> Option<JpegInput> {
        self.0.decoding.is_plain_jpeg().then(|| JpegInput {
            bytes: self.0.bytes.clone(),
            range: self.0.range.clone(),
        })
    }
}
