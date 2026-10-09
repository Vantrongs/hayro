//! Owned, exact grayscale JPEG rows.

use super::{Filter, OwnedInner, OwnedStream};
use core::ops::Range;
use zune_jpeg::rows::{GrayscaleCheckpoint, GrayscaleRows};
use zune_jpeg::zune_core::options::DecoderOptions;

/// An immutable lease of encoded JPEG bytes, sharing the PDF or inline stream.
pub struct JpegInput {
    owner: OwnedStream,
    range: Range<usize>,
}

impl AsRef<[u8]> for JpegInput {
    fn as_ref(&self) -> &[u8] {
        let bytes = match &self.owner.0 {
            OwnedInner::Indirect { xref, .. } => xref.file_data().unwrap().as_ref(),
            OwnedInner::Inline { data, .. } => data,
        };
        &bytes[self.range.clone()]
    }
}

/// Exact full-resolution grayscale bands over an owned stream lease.
pub type JpegRows = GrayscaleRows<JpegInput>;
/// Independent JPEG row-boundary state retaining its encoded source.
pub type JpegCheckpoint = GrayscaleCheckpoint<JpegInput>;

impl OwnedStream {
    /// Open single-filter, unencrypted grayscale JPEG rows with matching dimensions.
    /// Other JPEGs retain the ordinary whole-image decode and metadata repair path.
    pub fn jpeg_rows(&self, dimensions: (u32, u32)) -> Option<JpegRows> {
        let stream = self.get()?;
        if stream.decryption_object_id().is_some()
            || !matches!(stream.filters().as_slice(), [Filter::DctDecode])
        {
            return None;
        }
        let backing = match &self.0 {
            OwnedInner::Indirect { xref, .. } => xref.file_data()?.as_ref(),
            OwnedInner::Inline { data, .. } => data,
        };
        let start = (stream.data.as_ptr() as usize).checked_sub(backing.as_ptr() as usize)?;
        let end = start.checked_add(stream.data.len())?;
        if !core::ptr::eq(backing.get(start..end)?, stream.data) {
            return None;
        }
        let rows = JpegRows::new(
            JpegInput {
                owner: self.clone(),
                range: start..end,
            },
            DecoderOptions::default()
                .set_max_width(u16::MAX as usize)
                .set_max_height(u16::MAX as usize),
        )
        .ok()?;
        (rows.dimensions() == (dimensions.0 as usize, dimensions.1 as usize)).then_some(rows)
    }
}
