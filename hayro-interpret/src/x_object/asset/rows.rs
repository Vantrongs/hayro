//! Rows of stored samples read incrementally, piece by piece, and masks as rows of
//! alpha.

use super::{DecodeWork, Stop};
use crate::LumaData;
use crate::x_object::decode::{
    BILEVEL_MASK_LUT, MaskPath, SampleFormat, decode_mask, rows_present, sample_format,
};
use crate::x_object::image::{ImageKind, ImageXObject};
use core::ops::Range;
use hayro_syntax::object::Stream;
use hayro_syntax::object::stream::{DecodedCheckpoint, DecodedReader, ReadError};
use hayro_syntax::work::{Cancelled, Meter};
use std::sync::OnceLock;

#[cfg(test)]
std::thread_local! {
    /// The bytes rows read on this thread, which tests bound.
    pub(super) static READ: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// The most bytes of a row read at once: wider rows are read, and their texels
/// converted and resampled, a piece at a time.
pub(super) const PIECE_BYTES: usize = 64 * 1024;

/// The texels of `bytes` bytes each that a piece holds, of a window `columns` wide: a
/// multiple of eight, so that pieces of one-bit samples start on a byte.
pub(super) fn piece_texels(columns: usize, bytes: usize) -> usize {
    columns.min((PIECE_BYTES / bytes).max(8) & !7).max(1)
}

/// Why reading rows stopped before the last.
#[derive(Debug)]
pub(super) enum Break {
    /// A reader restarted: the pass starts over from the first row.
    Restart,
    Stop(Stop),
}

impl From<ReadError> for Break {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::Restarted => Self::Restart,
            ReadError::Failed => Self::Stop(Stop::Failed),
            ReadError::Cancelled => Self::Stop(Stop::Cancelled),
        }
    }
}

impl From<Stop> for Break {
    fn from(stop: Stop) -> Self {
        Self::Stop(stop)
    }
}

impl From<Cancelled> for Break {
    fn from(_: Cancelled) -> Self {
        Self::Stop(Stop::Cancelled)
    }
}

/// Rows of an image's or mask's stored samples, read incrementally, the reading
/// counted on a meter.
pub(super) struct RawRows<'a> {
    pub(super) reader: DecodedReader<'a>,
    meter: &'a Meter<'a>,
    /// Stored bytes per row.
    pub(super) row_bytes: usize,
    /// Bytes read since the start of the data.
    pub(super) total: usize,
    /// Bytes read of the row being read.
    at: usize,
    /// Holds the bytes read past.
    scratch: Vec<u8>,
}

/// What reading a row found.
pub(super) enum RawRow {
    Full,
    /// The data ended within the row after this many bytes.
    Partial(usize),
    End,
}

impl<'a> RawRows<'a> {
    pub(super) fn new(stream: &Stream<'a>, row_bytes: usize, meter: &'a Meter<'a>) -> Option<Self> {
        Some(Self {
            reader: stream.decoded_reader()?,
            meter,
            row_bytes,
            total: 0,
            at: 0,
            scratch: Vec::new(),
        })
    }

    /// Back to the first row, keeping the decoder a restart chose.
    pub(super) fn rewind(&mut self) {
        self.reader.rewind();
        self.total = 0;
        self.at = 0;
    }

    /// Continues from `state`, saved at the start of `row`; false if it does not
    /// resume this reader.
    pub(super) fn resume(&mut self, row: u32, state: &DecodedCheckpoint) -> bool {
        if !self.reader.restore(state) {
            return false;
        }
        self.total = row as usize * self.row_bytes;
        self.at = 0;
        true
    }

    /// Starts reading the next row.
    pub(super) fn begin(&mut self) {
        self.at = 0;
    }

    /// Bytes read of the row being read.
    pub(super) fn at(&self) -> usize {
        self.at
    }

    /// Reads the row's next bytes into `out` (at most a piece, no further than the
    /// row's end); returns how many the data held.
    pub(super) fn read(&mut self, out: &mut [u8]) -> Result<usize, Break> {
        let len = out.len().min(self.row_bytes - self.at);
        let mut filled = 0;
        while filled < len {
            match self.reader.read(&mut out[filled..len], self.meter) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => {
                    self.total = 0;
                    self.at = 0;
                    return Err(e.into());
                }
            }
        }
        self.at += filled;
        self.total += filled;
        #[cfg(test)]
        READ.set(READ.get() + filled as u64);
        Ok(filled)
    }

    /// Reads past the row's next `n` bytes; false if the data ended first.
    pub(super) fn skip(&mut self, n: usize) -> Result<bool, Break> {
        let end = (self.at + n).min(self.row_bytes);
        let mut scratch = std::mem::take(&mut self.scratch);
        if scratch.is_empty() {
            scratch = vec![0; self.row_bytes.min(PIECE_BYTES)];
        }
        let mut ended = false;
        while self.at < end && !ended {
            let len = (end - self.at).min(scratch.len());
            let read = self.read(&mut scratch[..len]);
            ended = read.as_ref().is_ok_and(|&got| got < len);
            if let Err(e) = read {
                self.scratch = scratch;
                return Err(e);
            }
        }
        self.scratch = scratch;
        Ok(!ended)
    }

    /// Reads past the rest of the row: what the row held.
    pub(super) fn finish(&mut self) -> Result<RawRow, Break> {
        self.skip(self.row_bytes - self.at)?;
        Ok(match self.at {
            0 => RawRow::End,
            at if at == self.row_bytes => RawRow::Full,
            at => RawRow::Partial(at),
        })
    }

    /// Reads past the next row: what it held.
    pub(super) fn skip_row(&mut self) -> Result<RawRow, Break> {
        self.begin();
        self.finish()
    }
}

/// Whether whole decoding keeps a last row the data fills in part (`rows_present`):
/// `part` of its `row_len` samples, `samples` decoded in all.
pub(super) fn keeps_partial_row(samples: usize, part: usize, row_len: usize) -> bool {
    rows_present(samples, row_len, u32::MAX) as usize > samples / row_len && part < row_len
}

/// A mask, read incrementally where its samples allow, else decoded whole; either way
/// with the rows its data holds, as `decode_mask` counts them.
pub(super) enum MaskSource<'a> {
    Streamed {
        obj: ImageXObject<'a>,
        path: RowPath,
        height: u32,
    },
    Decoded(LumaData),
}

/// How the samples of a mask read as rows become alpha: the `MaskPath`s other than
/// through the decode array.
#[derive(Clone, Copy)]
pub(super) enum RowPath {
    /// One bit per sample, each 0 or 255.
    Bilevel { invert: bool },
    /// One byte per sample.
    Bytes { invert: bool },
}

impl RowPath {
    fn of(path: MaskPath) -> Option<Self> {
        match path {
            MaskPath::Bilevel { invert } => Some(Self::Bilevel { invert }),
            MaskPath::Bytes { invert } => Some(Self::Bytes { invert }),
            MaskPath::Decoded => None,
        }
    }

    fn row_bytes(self, width: usize) -> usize {
        match self {
            Self::Bilevel { .. } => width.div_ceil(8),
            Self::Bytes { .. } => width,
        }
    }

    /// The samples `decode_mask_data` makes of `bytes` stored bytes of rows of `width`
    /// samples, before padding: a bilevel row cut short keeps its whole bytes' samples.
    fn samples(self, bytes: usize, width: usize) -> usize {
        match self {
            Self::Bilevel { .. } => {
                let row_bytes = self.row_bytes(width);
                bytes / row_bytes * width + (bytes % row_bytes).min(width / 8) * 8
            }
            Self::Bytes { .. } => bytes,
        }
    }
}

/// The rows a streamed mask's data holds, counted when an asset is first opened; `None`
/// when the data cannot be decoded.
pub(super) type MaskHeight = OnceLock<Option<u32>>;

impl<'a> MaskSource<'a> {
    /// The mask in `stream`, as `decode_alpha` reads a soft mask or a mask stream:
    /// `None` when it has no rows. The rows of a streamed mask are counted once, into
    /// `height`, by reading its data through, which adds to `work`; a count `meter`
    /// cancelled is not kept.
    pub(super) fn new(
        stream: &Stream<'a>,
        image: &ImageXObject<'a>,
        height: &MaskHeight,
        work: &mut DecodeWork,
        meter: &Meter<'_>,
    ) -> Result<Option<Self>, Cancelled> {
        let Some(mask) = ImageXObject::new_mask(stream, &image.warning_sink, &image.cache) else {
            return Ok(None);
        };
        let SampleFormat {
            color_space,
            bits_per_component,
            decode_arr,
        } = sample_format(&mask, None);
        let path = MaskPath::new(
            &color_space,
            bits_per_component,
            &decode_arr,
            mask.kind == ImageKind::StencilMask,
        );
        if let Some(path) = RowPath::of(path)
            && mask.stream.can_read_incrementally()
        {
            let held = match height.get() {
                Some(&held) => held,
                None => {
                    let held = rows_held(&mask, path, work, meter)?;
                    *height.get_or_init(|| held)
                }
            };
            return Ok(held.filter(|&h| h > 0).map(|height| Self::Streamed {
                obj: mask,
                path,
                height,
            }));
        }
        let decoded = decode_mask(&mask, None, meter);
        meter.poll()?;
        Ok(decoded.map(|m| Self::Decoded(m.luma)))
    }

    pub(super) fn size(&self) -> (u32, u32) {
        match self {
            Self::Streamed { obj, height, .. } => (obj.width(), *height),
            Self::Decoded(luma) => (luma.width, luma.height),
        }
    }

    pub(super) fn interpolate(&self) -> bool {
        match self {
            Self::Streamed { obj, .. } => obj.interpolate,
            Self::Decoded(luma) => luma.interpolate,
        }
    }

    /// The mask's rows, read on `meter`.
    pub(super) fn rows(&'a self, meter: &'a Meter<'a>) -> Option<MaskRows<'a>> {
        Some(match self {
            Self::Streamed { obj, path, .. } => {
                let width = obj.width() as usize;
                MaskRows::Streamed {
                    raw: Box::new(RawRows::new(&obj.stream, path.row_bytes(width), meter)?),
                    path: *path,
                    width,
                    bytes: Vec::new(),
                    last: 0,
                }
            }
            Self::Decoded(luma) => MaskRows::Decoded { luma, row: 0 },
        })
    }
}

/// The rows of `mask`'s data, as `decode_mask` counts them: its data read through,
/// as whole decoding reads it, adding the bytes inflated to `work`.
fn rows_held(
    mask: &ImageXObject<'_>,
    path: RowPath,
    work: &mut DecodeWork,
    meter: &Meter<'_>,
) -> Result<Option<u32>, Cancelled> {
    let Some(mut reader) = mask.stream.decoded_reader() else {
        return Ok(None);
    };
    let width = mask.width() as usize;
    let mut buf = vec![0; path.row_bytes(width).clamp(1, PIECE_BYTES)];
    let mut bytes = 0;
    let held = loop {
        match reader.read(&mut buf, meter) {
            Ok(0) => break Ok(Some(bytes)),
            Ok(n) => bytes += n,
            Err(ReadError::Restarted) => bytes = 0,
            Err(ReadError::Failed) => break Ok(None),
            Err(ReadError::Cancelled) => break Err(Cancelled),
        }
    };
    work.bytes_inflated += reader.inflated_bytes();
    Ok(held?.map(|held| rows_present(path.samples(held, width), width, mask.height())))
}

/// A mask's rows, as alpha, each read from its start in pieces of columns from left
/// to right. Every row asked for is present: a mask has the rows its data holds.
pub(super) enum MaskRows<'a> {
    Streamed {
        raw: Box<RawRows<'a>>,
        path: RowPath,
        width: usize,
        /// The bytes of a bilevel piece, and the last one read of the row, which a
        /// piece starting within it needs again.
        bytes: Vec<u8>,
        last: u8,
    },
    Decoded {
        luma: &'a LumaData,
        row: u32,
    },
}

impl MaskRows<'_> {
    /// Back to the first row; with `reader`, the reader too (after another reader of
    /// the pass restarted; one that restarted itself is already back at its start).
    pub(super) fn rewind(&mut self, reader: bool) {
        match self {
            MaskRows::Decoded { row, .. } => *row = 0,
            MaskRows::Streamed { raw, .. } => {
                if reader {
                    raw.rewind();
                }
                raw.begin();
            }
        }
    }

    /// The bytes the mask's incremental Flate decoder produced.
    pub(super) fn inflated_bytes(&self) -> u64 {
        match self {
            MaskRows::Streamed { raw, .. } => raw.reader.inflated_bytes(),
            MaskRows::Decoded { .. } => 0,
        }
    }

    /// Starts the next row.
    pub(super) fn begin_row(&mut self) {
        if let MaskRows::Streamed { raw, .. } = self {
            raw.begin();
        }
    }

    /// Writes the alpha of columns `cols` of the row into `out` (`cols` long), after
    /// the columns written before; returns how many of them the data holds, the rest
    /// of a last row cut short being transparent.
    pub(super) fn alpha(&mut self, cols: Range<usize>, out: &mut [u8]) -> Result<usize, Break> {
        match self {
            MaskRows::Decoded { luma, row } => {
                let at = *row as usize * luma.width as usize;
                out.copy_from_slice(&luma.data[at + cols.start..at + cols.end]);
                Ok(out.len())
            }
            MaskRows::Streamed {
                raw,
                path,
                width,
                bytes,
                last,
            } => match *path {
                RowPath::Bytes { invert } => {
                    raw.skip(cols.start - raw.at())?;
                    let got = raw.read(out)?;
                    if invert {
                        out[..got].iter_mut().for_each(|a| *a = 255 - *a);
                    }
                    out[got..].fill(0);
                    Ok(got)
                }
                RowPath::Bilevel { invert } => {
                    bilevel_alpha(raw, bytes, last, invert, *width, cols, out)
                }
            },
        }
    }

    /// Ends the row, reading past the rest of it.
    pub(super) fn end_row(&mut self) -> Result<(), Break> {
        match self {
            MaskRows::Decoded { row, .. } => *row += 1,
            MaskRows::Streamed { raw, .. } => {
                raw.finish()?;
            }
        }
        Ok(())
    }

    /// Reads past the next row.
    pub(super) fn skip_row(&mut self) -> Result<(), Break> {
        self.begin_row();
        self.end_row()
    }

    /// Moves past the next `n` rows: reads past each of a streamed mask's, and jumps
    /// over those of a mask decoded whole.
    pub(super) fn skip_rows(&mut self, n: u32) -> Result<(), Break> {
        match self {
            MaskRows::Decoded { row, .. } => *row += n,
            MaskRows::Streamed { .. } => (0..n).try_for_each(|_| self.skip_row())?,
        }
        Ok(())
    }
}

/// The alpha of columns `cols` of a bilevel row from `raw`, into `out`, its bytes
/// read into `bytes`; `last` is the last byte read of the row. Returns how many of
/// the columns the data holds.
fn bilevel_alpha(
    raw: &mut RawRows<'_>,
    bytes: &mut Vec<u8>,
    last: &mut u8,
    invert: bool,
    width: usize,
    cols: Range<usize>,
    out: &mut [u8],
) -> Result<usize, Break> {
    let (first, end) = (cols.start / 8, cols.end.div_ceil(8));
    // A piece starting within the byte the one before ended in has it in `last`.
    if raw.at() <= first {
        raw.skip(first - raw.at())?;
    }
    // Short of `first` when the data ended before it, and then nothing is read.
    let from = raw.at();
    bytes.resize(end - from.max(first), 0);
    let got = raw.read(bytes)?;
    let held = from + got;
    let xor = if invert { u64::MAX } else { 0 };
    let lut = &**BILEVEL_MASK_LUT;
    for byte in first..held.min(end) {
        let value = if byte < from {
            *last
        } else {
            bytes[byte - from]
        };
        let expanded = (lut[value as usize] ^ xor).to_ne_bytes();
        let texels = (byte * 8).max(cols.start)..(byte * 8 + 8).min(cols.end);
        out[texels.start - cols.start..texels.end - cols.start]
            .copy_from_slice(&expanded[texels.start - byte * 8..texels.end - byte * 8]);
    }
    if got > 0 {
        *last = bytes[got - 1];
    }
    if held >= end {
        return Ok(out.len());
    }
    // A last row cut short keeps the samples of its whole bytes; the rest are zero,
    // after inversion.
    let present = (held.min(width / 8) * 8).clamp(cols.start, cols.end);
    out[present - cols.start..].fill(0);
    Ok(present - cols.start)
}
