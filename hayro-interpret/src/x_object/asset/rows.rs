//! Rows of stored samples read incrementally, piece by piece, and masks as rows of
//! alpha.

use super::DecodeWork;
use crate::LumaData;
use crate::x_object::decode::{
    BILEVEL_MASK_LUT, MaskPath, SampleFormat, SampleRows, decode_mask, mask_lut, sample_format,
};
use crate::x_object::image::{ImageKind, ImageXObject};
use core::ops::Range;
use hayro_syntax::object::Stream;
use hayro_syntax::object::stream::{DecodedCheckpoint, DecodedReader};
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
    /// The data ended within the row.
    Partial,
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
    pub(super) fn read(&mut self, out: &mut [u8]) -> Result<usize, Cancelled> {
        let len = out.len().min(self.row_bytes - self.at);
        let mut filled = 0;
        while filled < len {
            match self.reader.read(&mut out[filled..len], self.meter)? {
                0 => break,
                n => filled += n,
            }
        }
        self.at += filled;
        self.total += filled;
        #[cfg(test)]
        READ.set(READ.get() + filled as u64);
        Ok(filled)
    }

    /// Reads past the row's next `n` bytes; false if the data ended first.
    pub(super) fn skip(&mut self, n: usize) -> Result<bool, Cancelled> {
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
    pub(super) fn finish(&mut self) -> Result<RawRow, Cancelled> {
        self.skip(self.row_bytes - self.at)?;
        Ok(match self.at {
            0 => RawRow::End,
            at if at == self.row_bytes => RawRow::Full,
            _ => RawRow::Partial,
        })
    }

    /// Reads past the next row: what it held.
    pub(super) fn skip_row(&mut self) -> Result<RawRow, Cancelled> {
        self.begin();
        self.finish()
    }
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

/// How the samples of a mask read as rows become alpha (`MaskPath`).
#[derive(Clone)]
pub(super) enum RowPath {
    /// One bit per sample, each 0 or 255.
    Bilevel { invert: bool },
    /// One byte per sample.
    Bytes { invert: bool },
    /// One component of `bits` (fewer than eight) per sample, through the decode
    /// array: `lut[v]` is the alpha of stored value `v`.
    /// Boxed, so that the other paths' images stay as small.
    Mapped { bits: u8, lut: Box<[u8; 256]> },
}

impl RowPath {
    /// The path of a mask's samples of `format`, if they are read as rows.
    fn of(path: MaskPath, format: &SampleFormat, stencil: bool) -> Option<Self> {
        match path {
            MaskPath::Bilevel { invert } => Some(Self::Bilevel { invert }),
            MaskPath::Bytes { invert } => Some(Self::Bytes { invert }),
            MaskPath::Decoded if format.color_space.num_components() == 1 => {
                let bits = format.bits_per_component;
                Some(Self::Mapped {
                    bits,
                    lut: Box::new(mask_lut(bits, &format.decode_arr, stencil)?),
                })
            }
            MaskPath::Decoded => None,
        }
    }

    /// How the samples of `mask` lie in its rows.
    fn rows(&self, mask: &ImageXObject<'_>) -> SampleRows {
        let bits = match self {
            Self::Bilevel { .. } => 1,
            Self::Bytes { .. } => 8,
            Self::Mapped { bits, .. } => *bits,
        };
        SampleRows::new(mask.width(), 1, bits, mask.height())
    }
}

/// The rows whole decoding keeps of a stream's data, counted when an asset is first
/// opened; `None` when the data cannot be decoded.
pub(super) type RowsHeld = OnceLock<Option<u32>>;

impl<'a> MaskSource<'a> {
    /// The mask in `stream`, as `decode_alpha` reads a soft mask or a mask stream:
    /// `None` when it has no rows. The rows of a streamed mask are counted once, into
    /// `height`, by reading its data through, which adds to `work`; a count `meter`
    /// cancelled is not kept.
    pub(super) fn new(
        stream: &Stream<'a>,
        image: &ImageXObject<'a>,
        height: &RowsHeld,
        work: &mut DecodeWork,
        meter: &Meter<'_>,
    ) -> Result<Option<Self>, Cancelled> {
        let Some(mask) = ImageXObject::new_mask(stream, &image.warning_sink, &image.cache) else {
            return Ok(None);
        };
        let format = sample_format(&mask, None);
        let stencil = mask.kind == ImageKind::StencilMask;
        let path = MaskPath::new(
            &format.color_space,
            format.bits_per_component,
            &format.decode_arr,
            stencil,
        );
        if let Some(path) = RowPath::of(path, &format, stencil)
            && mask.stream.can_read_incrementally()
        {
            let held = match height.get() {
                Some(&held) => held,
                None => {
                    let held = rows_held(&mask.stream, path.rows(&mask), work, meter)?;
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
                let row_bytes = path.rows(obj).row_bytes();
                MaskRows::Streamed {
                    raw: Box::new(RawRows::new(&obj.stream, row_bytes, meter)?),
                    path: path.clone(),
                    width,
                    bytes: Vec::new(),
                    last: 0,
                }
            }
            Self::Decoded(luma) => MaskRows::Decoded { luma, row: 0 },
        })
    }
}

/// The rows whole decoding keeps of `stream`'s data, of samples lying in `rows`: the
/// length of data stored without a filter, else its data read, as whole decoding
/// reads it, until it ends or holds the rows declared (data after them changes
/// nothing), adding the bytes inflated to `work`.
pub(super) fn rows_held(
    stream: &Stream<'_>,
    rows: SampleRows,
    work: &mut DecodeWork,
    meter: &Meter<'_>,
) -> Result<Option<u32>, Cancelled> {
    let Some(mut reader) = stream.decoded_reader() else {
        return Ok(None);
    };
    if let Some(len) = reader.stored_len() {
        return Ok(Some(rows.rows_kept(len)));
    }
    let declared = rows.row_bytes().saturating_mul(rows.height as usize);
    let mut buf = vec![0; rows.row_bytes().min(PIECE_BYTES)];
    let mut bytes = 0;
    let held = loop {
        let wanted = buf.len().min(declared - bytes);
        if wanted == 0 {
            break Ok(bytes);
        }
        match reader.read(&mut buf[..wanted], meter) {
            Ok(0) => break Ok(bytes),
            Ok(n) => bytes += n,
            Err(Cancelled) => break Err(Cancelled),
        }
    };
    work.bytes_inflated += reader.inflated_bytes();
    Ok(Some(rows.rows_kept(held?)))
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
    pub(super) fn alpha(&mut self, cols: Range<usize>, out: &mut [u8]) -> Result<usize, Cancelled> {
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
            } => match path {
                RowPath::Bytes { invert } => {
                    raw.skip(cols.start - raw.at())?;
                    let got = raw.read(out)?;
                    if *invert {
                        out[..got].iter_mut().for_each(|a| *a = 255 - *a);
                    }
                    out[got..].fill(0);
                    Ok(got)
                }
                RowPath::Bilevel { invert } => {
                    bilevel_alpha(raw, bytes, last, *invert, *width, cols, out)
                }
                RowPath::Mapped { bits, lut } => {
                    mapped_alpha(raw, bytes, last, *bits, lut, cols, out)?;
                    Ok(out.len())
                }
            },
        }
    }

    /// Ends the row, reading past the rest of it.
    pub(super) fn end_row(&mut self) -> Result<(), Cancelled> {
        match self {
            MaskRows::Decoded { row, .. } => *row += 1,
            MaskRows::Streamed { raw, .. } => {
                raw.finish()?;
            }
        }
        Ok(())
    }

    /// Reads past the next row.
    pub(super) fn skip_row(&mut self) -> Result<(), Cancelled> {
        self.begin_row();
        self.end_row()
    }

    /// Moves past the next `n` rows: reads past each of a streamed mask's, and jumps
    /// over those of a mask decoded whole.
    pub(super) fn skip_rows(&mut self, n: u32) -> Result<(), Cancelled> {
        match self {
            MaskRows::Decoded { row, .. } => *row += n,
            MaskRows::Streamed { .. } => (0..n).try_for_each(|_| self.skip_row())?,
        }
        Ok(())
    }
}

/// The alpha of columns `cols` of a row of samples of `bits` through `lut`, into
/// `out`, its bytes read into `bytes`; `last` is the last byte read of the row. A
/// sample the data does not hold whole is stored zero, as decoding the mask whole
/// unpacks it, so every column has alpha.
fn mapped_alpha(
    raw: &mut RawRows<'_>,
    bytes: &mut Vec<u8>,
    last: &mut u8,
    bits: u8,
    lut: &[u8; 256],
    cols: Range<usize>,
    out: &mut [u8],
) -> Result<(), Cancelled> {
    let bits = usize::from(bits);
    let (first, end) = (cols.start * bits / 8, (cols.end * bits).div_ceil(8));
    if raw.at() <= first {
        raw.skip(first - raw.at())?;
    }
    // Past `first` only by the byte the piece before ended in, which is `last`; short
    // of it when the data ended before it.
    let from = raw.at();
    bytes.clear();
    bytes.resize(end - first, 0);
    if from > first {
        bytes[0] = *last;
    }
    let start = from.max(first);
    let got = raw.read(&mut bytes[start - first..])?;
    if got > 0 {
        *last = bytes[start - first + got - 1];
    }
    let held_bits = (start + got).saturating_sub(first) * 8;
    let mask = (1_u16 << bits) - 1;
    let offset = cols.start * bits - first * 8;
    for (i, alpha) in out.iter_mut().enumerate() {
        let at = offset + i * bits;
        let value = if at + bits <= held_bits {
            let pair = [bytes[at / 8], bytes.get(at / 8 + 1).copied().unwrap_or(0)];
            (u16::from_be_bytes(pair) >> (16 - bits - at % 8)) & mask
        } else {
            0
        };
        *alpha = lut[usize::from(value)];
    }
    Ok(())
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
) -> Result<usize, Cancelled> {
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
