//! Grayscale JPEG data read in bands of rows, independent of the document's lifetime.

use super::{OwnedInner, OwnedStream};
use crate::PdfData;
use crate::filter::Filter;
use crate::sync::Arc;
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
        let stream = self.get()?;
        if !matches!(stream.decryption(), Ok(None))
            || !matches!(stream.filters().as_slice(), [Filter::DctDecode])
        {
            return None;
        }
        let bytes = match &self.0 {
            OwnedInner::Indirect { xref, .. } => xref.xref().file_data()?.clone(),
            OwnedInner::Inline { data, .. } => PdfData::from(Arc::new(data.clone())),
        };
        let all = bytes.as_ref();
        let start = (stream.data.as_ptr() as usize).checked_sub(all.as_ptr() as usize)?;
        let range = start..start.checked_add(stream.data.len())?;
        // Data from a decoded object stream lies elsewhere.
        if !core::ptr::eq(all.get(range.clone())?, stream.data) {
            return None;
        }
        Some(JpegInput { bytes, range })
    }
}
