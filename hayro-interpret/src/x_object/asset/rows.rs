//! Rows of stored samples read incrementally, piece by piece, and masks as rows of
//! alpha.

use super::Stop;
use super::streamed::incremental_samples;
use crate::x_object::decode::{
    BILEVEL_MASK_LUT, DecodedMask, MaskPath, SampleFormat, SampleRows, decode_mask,
    image_decode_params, mask_lut,
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
    /// The rows of colour and of masks moved past one at a time on this thread, which
    /// tests bound.
    pub(super) static STEPPED: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// Counts a row moved past one at a time ([`STEPPED`]).
pub(super) fn stepped() {
    #[cfg(test)]
    STEPPED.set(STEPPED.get() + 1);
}

/// The most bytes of a row read at once: wider rows are read, and their texels
/// converted and resampled, a piece at a time.
pub(super) const PIECE_BYTES: usize = 64 * 1024;

/// The texels of `bytes` bytes each that a piece holds, of a window `columns` wide: a
/// multiple of eight, so that pieces of one-bit samples start on a byte.
pub(super) fn piece_texels(columns: usize, bytes: usize) -> usize {
    columns.min((PIECE_BYTES / bytes).max(8) & !7).max(1)
}

/// Where the stored data of an image and of its mask stream end, in bytes from their
/// start, once known: from the stored length of data without a filter, or found by a
/// request reading to it. Whole decoding keeps of samples other than 8-bit only whole
/// rows, so a row the data ends within reads as stored zero; a request that has
/// pushed part of such a row before finding the end reads the image again, knowing
/// it.
#[derive(Default)]
pub(super) struct DataEnds {
    pub(super) colour: OnceLock<usize>,
    pub(super) mask: OnceLock<usize>,
}

impl DataEnds {
    /// How many of the two ends are known: it only grows, to two at most.
    pub(super) fn known(&self) -> usize {
        usize::from(self.colour.get().is_some()) + usize::from(self.mask.get().is_some())
    }
}

/// Rows of an image's or mask's stored samples, read incrementally, the reading
/// counted on a meter.
pub(super) struct RawRows<'a> {
    pub(super) reader: DecodedReader<'a>,
    meter: &'a Meter<'a>,
    /// Where the data ends, noted when a read reaches it.
    end: &'a OnceLock<usize>,
    /// Stored bytes per row.
    pub(super) row_bytes: usize,
    /// Bytes read since the start of the data.
    pub(super) total: usize,
    /// Bytes read of the row being read.
    at: usize,
}

/// What reading a row found.
pub(super) enum RawRow {
    Full,
    /// The data ended within the row.
    Partial,
    End,
}

impl<'a> RawRows<'a> {
    /// The rows of `obj`'s samples, `row_bytes` each, read on `meter`, noting where
    /// the data ends in `end`.
    pub(super) fn new(
        obj: &ImageXObject<'a>,
        row_bytes: usize,
        end: &'a OnceLock<usize>,
        meter: &'a Meter<'a>,
    ) -> Option<Self> {
        let reader = image_reader(obj)?;
        if let Some(len) = reader.stored_len() {
            end.get_or_init(|| len);
        }
        Some(Self {
            reader,
            meter,
            end,
            row_bytes,
            total: 0,
            at: 0,
        })
    }

    /// Whether the data holds row `y` whole: `None` until its end is known.
    pub(super) fn holds_row(&self, y: u32) -> Option<bool> {
        let end = *self.end.get()?;
        Some(end >= (y as usize + 1).saturating_mul(self.row_bytes))
    }

    /// Whether the data holds any byte of row `y`: `None` until its end is known.
    pub(super) fn reaches_row(&self, y: u32) -> Option<bool> {
        let end = *self.end.get()?;
        Some(end > (y as usize).saturating_mul(self.row_bytes))
    }

    /// Notes that the data ends where it is read to.
    fn ended(&self) {
        self.end.get_or_init(|| self.total);
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
        if filled < len {
            self.ended();
        }
        Ok(filled)
    }

    /// Moves past the row's next `n` bytes; false if the data ended first.
    pub(super) fn skip(&mut self, n: usize) -> Result<bool, Cancelled> {
        let wanted = n.min(self.row_bytes - self.at);
        let got = self.reader.skip(wanted, self.meter)?;
        self.at += got;
        self.total += got;
        #[cfg(test)]
        READ.set(READ.get() + got as u64);
        if got < wanted {
            self.ended();
        }
        Ok(got == wanted)
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
/// on the grid it declares, as `decode_mask` decodes it.
pub(super) enum MaskSource<'a> {
    Streamed {
        obj: ImageXObject<'a>,
        path: RowPath,
        /// The columns of its samples (the dictionary's width, or CCITT data's).
        width: u32,
        height: u32,
    },
    Decoded(DecodedMask),
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
    /// Bits per sample.
    fn bits(&self) -> u8 {
        match self {
            Self::Bilevel { .. } => 1,
            Self::Bytes { .. } => 8,
            Self::Mapped { bits, .. } => *bits,
        }
    }

    /// The alpha of a sample the data does not hold: stored zero's.
    pub(super) fn fill(&self) -> u8 {
        match self {
            Self::Bilevel { invert } | Self::Bytes { invert } => {
                if *invert {
                    255
                } else {
                    0
                }
            }
            Self::Mapped { lut, .. } => lut[0],
        }
    }

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

    /// How the samples of a mask lie in rows of `width` and `height`.
    fn rows(&self, width: u32, height: u32) -> SampleRows {
        SampleRows::new(width, 1, self.bits(), height)
    }
}

impl<'a> MaskSource<'a> {
    /// The mask in `stream`, as `decode_alpha` reads a soft mask or a mask stream:
    /// `None` when it is not an image or decoding it whole fails.
    pub(super) fn new(
        stream: &Stream<'a>,
        image: &ImageXObject<'a>,
        meter: &Meter<'_>,
    ) -> Result<Option<Self>, Cancelled> {
        let Some(mask) = ImageXObject::new_mask(stream, &image.warning_sink, &image.cache) else {
            return Ok(None);
        };
        let stencil = mask.kind == ImageKind::StencilMask;
        if let Some((grid, format)) = incremental_samples(&mask) {
            let path = MaskPath::new(
                &format.color_space,
                format.bits_per_component,
                &format.decode_arr,
                stencil,
            );
            if let Some(path) = RowPath::of(path, &format, stencil) {
                return Ok(Some(Self::Streamed {
                    obj: mask,
                    path,
                    width: grid.width,
                    height: grid.height,
                }));
            }
        }
        let decoded = decode_mask(&mask, None, meter);
        meter.poll()?;
        Ok(decoded.map(Self::Decoded))
    }

    pub(super) fn size(&self) -> (u32, u32) {
        match self {
            Self::Streamed { width, height, .. } => (*width, *height),
            Self::Decoded(m) => (m.luma.width, m.rows.declared),
        }
    }

    pub(super) fn interpolate(&self) -> bool {
        match self {
            Self::Streamed { obj, .. } => obj.interpolate,
            Self::Decoded(m) => m.luma.interpolate,
        }
    }

    /// The mask's rows, read on `meter`, noting where its data ends in `end`.
    pub(super) fn rows(
        &'a self,
        end: &'a OnceLock<usize>,
        meter: &'a Meter<'a>,
    ) -> Option<MaskRows<'a>> {
        Some(match self {
            Self::Streamed {
                obj,
                path,
                width,
                height,
            } => {
                let row_bytes = path.rows(*width, *height).row_bytes();
                MaskRows::Streamed {
                    raw: Box::new(RawRows::new(obj, row_bytes, end, meter)?),
                    path: path.clone(),
                    bytes: Vec::new(),
                    last: 0,
                    row: 0,
                    fill: false,
                    given: false,
                }
            }
            Self::Decoded(mask) => MaskRows::Decoded { mask, row: 0 },
        })
    }
}

/// The reader of `obj`'s samples, as whole decoding decodes them.
fn image_reader<'a>(obj: &ImageXObject<'a>) -> Option<DecodedReader<'a>> {
    obj.stream.image_reader(&image_decode_params(obj))
}

/// A mask's rows, as alpha, each read from its start in pieces of columns from left
/// to right. Every row it declares is present, a sample its data does not hold being
/// stored zero, as `decode_mask` pads it.
pub(super) enum MaskRows<'a> {
    Streamed {
        raw: Box<RawRows<'a>>,
        path: RowPath,
        /// The bytes of a bilevel piece, and the last one read of the row, which a
        /// piece starting within it needs again.
        bytes: Vec<u8>,
        last: u8,
        /// The row being read.
        row: u32,
        /// Whether the row being read is all stored zero: its data ends within it,
        /// and its samples are not 8-bit.
        fill: bool,
        /// Whether alpha of the row being read was given.
        given: bool,
    },
    Decoded {
        mask: &'a DecodedMask,
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

    /// The alpha of a sample the data does not hold.
    pub(super) fn fill(&self) -> u8 {
        match self {
            MaskRows::Streamed { path, .. } => path.fill(),
            // The rows past those held repeat the last, of stored zero.
            MaskRows::Decoded { mask, .. } => {
                let luma = &mask.luma;
                luma.data[(luma.height as usize - 1) * luma.width as usize]
            }
        }
    }

    /// Whether the row begun is all stored zero's alpha ([`Self::fill`]).
    pub(super) fn fills_row(&self) -> bool {
        match self {
            MaskRows::Streamed { fill, .. } => *fill,
            MaskRows::Decoded { mask, row } => *row >= mask.luma.height,
        }
    }

    /// Starts the next row.
    pub(super) fn begin_row(&mut self) {
        if let MaskRows::Streamed {
            raw,
            path,
            row,
            fill,
            given,
            ..
        } = self
        {
            raw.begin();
            *given = false;
            *fill = !reads_row(raw, path.bits(), *row);
        }
    }

    /// Writes the alpha of columns `cols` of the row into `out` (`cols` long), after
    /// the columns written before; returns whether the data holds all of them (the
    /// rest being stored zero's alpha). Fails with [`Stop::Retry`] where the data
    /// ends within a row of samples other than 8-bit whose alpha was given.
    pub(super) fn alpha(&mut self, cols: Range<usize>, out: &mut [u8]) -> Result<bool, Stop> {
        match self {
            MaskRows::Decoded { mask, row } => {
                let luma = &mask.luma;
                if *row >= luma.height {
                    out.fill(self.fill());
                    return Ok(false);
                }
                let at = *row as usize * luma.width as usize;
                out.copy_from_slice(&luma.data[at + cols.start..at + cols.end]);
                Ok(true)
            }
            MaskRows::Streamed {
                raw,
                path,
                bytes,
                last,
                fill,
                given,
                ..
            } => {
                if *fill {
                    out.fill(path.fill());
                    return Ok(false);
                }
                let held = match path {
                    RowPath::Bytes { invert } => {
                        let wanted = out.len();
                        let got = if raw.skip(cols.start - raw.at())? {
                            raw.read(out)?
                        } else {
                            0
                        };
                        if *invert {
                            out[..got].iter_mut().for_each(|a| *a = 255 - *a);
                        }
                        out[got..].fill(if *invert { 255 } else { 0 });
                        got == wanted
                    }
                    RowPath::Bilevel { invert } => {
                        bilevel_alpha(raw, bytes, last, *invert, cols, out)?
                    }
                    RowPath::Mapped { bits, lut } => {
                        mapped_alpha(raw, bytes, last, *bits, lut, cols, out)?
                    }
                };
                if !held && path.bits() != 8 {
                    // Only whole rows of these samples count.
                    if *given {
                        return Err(Stop::Retry);
                    }
                    *fill = true;
                    out.fill(path.fill());
                }
                *given = true;
                Ok(held)
            }
        }
    }

    /// Ends the row, reading past the rest of it. Fails with [`Stop::Retry`] where the
    /// data ends within a row of samples other than 8-bit whose alpha was given.
    pub(super) fn end_row(&mut self) -> Result<(), Stop> {
        match self {
            MaskRows::Decoded { row, .. } => *row += 1,
            MaskRows::Streamed {
                raw,
                path,
                row,
                fill,
                given,
                ..
            } => {
                *row += 1;
                if !*fill && !matches!(raw.finish()?, RawRow::Full) && path.bits() != 8 && *given {
                    return Err(Stop::Retry);
                }
            }
        }
        Ok(())
    }

    /// Reads past the next row.
    pub(super) fn skip_row(&mut self) -> Result<(), Stop> {
        stepped();
        self.begin_row();
        self.end_row()
    }

    /// Moves past the next `n` rows: reads past each of a streamed mask's its data
    /// holds, and jumps over the rest, and over those of a mask decoded whole.
    pub(super) fn skip_rows(&mut self, n: u32) -> Result<(), Stop> {
        for left in (1..=n).rev() {
            match self {
                MaskRows::Streamed { raw, path, row, .. } if !reads_row(raw, path.bits(), *row) => {
                    *row += left;
                    return Ok(());
                }
                MaskRows::Streamed { .. } => self.skip_row()?,
                MaskRows::Decoded { row, .. } => {
                    *row += left;
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

/// Whether a row reader whose samples are of `bits` reads the bytes of row `y`: all
/// of 8-bit samples the data reaches, else only whole rows; until the data's end is
/// known, every row.
fn reads_row(raw: &RawRows<'_>, bits: u8, y: u32) -> bool {
    let reads = if bits == 8 {
        raw.reaches_row(y)
    } else {
        raw.holds_row(y)
    };
    reads.unwrap_or(true)
}

/// The alpha of columns `cols` of a row of samples of `bits` through `lut`, into
/// `out`, its bytes read into `bytes`; `last` is the last byte read of the row. A
/// sample the data does not hold whole is stored zero, as decoding the mask whole
/// pads it. Returns whether the data holds every column.
fn mapped_alpha(
    raw: &mut RawRows<'_>,
    bytes: &mut Vec<u8>,
    last: &mut u8,
    bits: u8,
    lut: &[u8; 256],
    cols: Range<usize>,
    out: &mut [u8],
) -> Result<bool, Cancelled> {
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
        *alpha = if at + bits <= held_bits {
            let pair = [bytes[at / 8], bytes.get(at / 8 + 1).copied().unwrap_or(0)];
            lut[usize::from((u16::from_be_bytes(pair) >> (16 - bits - at % 8)) & mask)]
        } else {
            lut[0]
        };
    }
    Ok(offset + out.len() * bits <= held_bits)
}

/// The alpha of columns `cols` of a bilevel row from `raw`, into `out`, its bytes
/// read into `bytes`; `last` is the last byte read of the row. Returns whether the
/// data holds every column.
fn bilevel_alpha(
    raw: &mut RawRows<'_>,
    bytes: &mut Vec<u8>,
    last: &mut u8,
    invert: bool,
    cols: Range<usize>,
    out: &mut [u8],
) -> Result<bool, Cancelled> {
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
    Ok(held >= end)
}
