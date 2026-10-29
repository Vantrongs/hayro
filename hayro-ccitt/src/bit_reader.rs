//! Bit-level reader for CCITT encoded data streams.

use crate::{DecodeError, Decoder, POLL_BITS, Result, Source};
use alloc::rc::Rc;

/// Where the encoded bytes are.
#[derive(Clone)]
pub(crate) enum Data<'a> {
    Slice(&'a [u8]),
    Source(Rc<dyn Source + 'a>),
    /// A source lent for a call ([`BitReader::lent`]).
    Lent(&'a dyn Source),
    /// None at hand: a source is lent to each call that reads.
    Detached,
}

#[derive(Clone)]
pub(crate) struct BitReader<'a> {
    data: Data<'a>,
    /// The bytes of `data`, where known: a source tells where it ends only once read
    /// there, so until then, `usize::MAX`, and the bytes up to 8 past the position
    /// the buffer was last filled at are known to be there.
    len: usize,
    bit_offset: usize,
    /// The bits starting at `bit_offset`, with the next bit in the most
    /// significant position. Always contains at least 32 valid bits, bits past
    /// the end of the data are zero.
    buffer: u64,
    /// The bit offset up to which bits can be consumed without refilling the
    /// buffer or reaching the end of the data.
    limit: usize,
    /// The bit offset from which on the decoder is asked whether to stop:
    /// [`POLL_BITS`] past the offset at which it was last asked.
    poll_at: usize,
    /// Whether a code was read past the end of the data ([`Self::consume_code`]).
    overrun: bool,
}

/// Where a [`BitReader`] is, without its data.
#[derive(Clone)]
pub(crate) struct Position {
    len: usize,
    bit_offset: usize,
    buffer: u64,
    limit: usize,
    poll_at: usize,
    overrun: bool,
}

impl<'a> BitReader<'a> {
    /// Where it is in its data.
    pub(crate) fn position(&self) -> Position {
        Position {
            len: self.len,
            bit_offset: self.bit_offset,
            buffer: self.buffer,
            limit: self.limit,
            poll_at: self.poll_at,
            overrun: self.overrun,
        }
    }

    /// A reader of `source` from `position`, taken of a reader of the same data.
    pub(crate) fn lent<'s>(source: &'s dyn Source, position: &Position) -> BitReader<'s> {
        let mut reader = BitReader {
            data: Data::Lent(source),
            len: 0,
            bit_offset: 0,
            buffer: 0,
            limit: 0,
            poll_at: 0,
            overrun: false,
        };
        reader.seek(position);
        reader.refill();
        reader
    }

    /// Goes on from `position`, taken of a reader of the same data.
    pub(crate) fn seek(&mut self, position: &Position) {
        let Position {
            len,
            bit_offset,
            buffer,
            limit,
            poll_at,
            overrun,
        } = *position;
        (self.len, self.bit_offset, self.buffer) = (len, bit_offset, buffer);
        (self.limit, self.poll_at, self.overrun) = (limit, poll_at, overrun);
    }

    #[inline(always)]
    pub(crate) fn new(data: Data<'a>) -> Self {
        let len = match &data {
            Data::Slice(bytes) => bytes.len(),
            Data::Source(_) | Data::Lent(_) | Data::Detached => usize::MAX,
        };
        let mut reader = Self {
            data,
            len,
            bit_offset: 0,
            buffer: 0,
            limit: 0,
            poll_at: POLL_BITS,
            overrun: false,
        };
        reader.refill();
        reader
    }

    #[inline(always)]
    pub(crate) fn read_bit(&mut self) -> Result<u32> {
        let bit = (self.buffer >> 63) as u32;
        self.consume(1)?;

        Ok(bit)
    }

    /// Return the next 32 bits without consuming them. Bits past the end of the
    /// data are zero.
    #[inline(always)]
    pub(crate) fn peek_u32(&self) -> u32 {
        (self.buffer >> 32) as u32
    }

    /// Consume up to 32 bits.
    #[inline(always)]
    pub(crate) fn consume(&mut self, num_bits: usize) -> Result<()> {
        let bit_offset = self.bit_offset + num_bits;

        if bit_offset > self.limit {
            return self.consume_slow(bit_offset);
        }

        self.bit_offset = bit_offset;
        self.buffer <<= num_bits;

        Ok(())
    }

    /// Consumes the `num_bits` bits of a code, up to 32, where the data may end within
    /// them: the bits past its end read as zero and the reader is left at the end
    /// ([`Self::overrun`]). Returns whether the code reached past the end.
    #[inline(always)]
    pub(crate) fn consume_code(&mut self, num_bits: usize) -> bool {
        let past_end = self.consume(num_bits).is_err();
        self.overrun |= past_end;
        past_end
    }

    /// Whether a code was read past the end of the data.
    #[inline(always)]
    pub(crate) fn overrun(&self) -> bool {
        self.overrun
    }

    /// Asks `decoder` whether to stop once [`POLL_BITS`] bits were read since
    /// it was last asked.
    #[inline(always)]
    pub(crate) fn poll(&mut self, decoder: &mut impl Decoder) -> Result<()> {
        if self.bit_offset >= self.poll_at {
            self.ask(decoder)
        } else {
            Ok(())
        }
    }

    /// Asks `decoder` whether to stop, telling it the bits read since it was
    /// last asked.
    #[cold]
    #[inline(never)]
    pub(crate) fn ask(&mut self, decoder: &mut impl Decoder) -> Result<()> {
        let read = self.bit_offset + POLL_BITS - self.poll_at;
        self.poll_at = self.bit_offset + POLL_BITS;

        if decoder.stop(read) {
            Err(DecodeError::Stopped)
        } else {
            Ok(())
        }
    }

    #[inline(always)]
    pub(crate) fn align(&mut self) {
        let bit_pos = self.bit_pos();

        if !bit_pos.is_multiple_of(8) {
            // The end of the data is always byte-aligned, so this can't fail.
            let _ = self.consume(8 - bit_pos);
        }
    }

    #[inline(always)]
    pub(crate) fn at_end(&self) -> bool {
        self.byte_pos() >= self.len
    }

    #[inline(always)]
    pub(crate) fn byte_pos(&self) -> usize {
        self.bit_offset >> 3
    }

    #[inline(always)]
    fn consume_slow(&mut self, bit_offset: usize) -> Result<()> {
        // Where the end is not known, the data reach 64 bits past the byte the buffer
        // was filled at, so they hold the up to 32 bits past `limit` consumed.
        let end = self.len.saturating_mul(8);
        self.bit_offset = bit_offset.min(end);
        self.refill();

        if bit_offset > end {
            Err(DecodeError::UnexpectedEof)
        } else {
            Ok(())
        }
    }

    #[inline(always)]
    fn bit_pos(&self) -> usize {
        self.bit_offset & 7
    }

    #[inline(always)]
    fn refill(&mut self) {
        let byte_pos = self.byte_pos();

        let word = match &self.data {
            Data::Slice(data) => {
                if let Some(bytes) = data.get(byte_pos..byte_pos + 8) {
                    u64::from_be_bytes(bytes.try_into().unwrap())
                } else {
                    let mut bytes = [0; 8];
                    let rest = data.get(byte_pos..).unwrap_or_default();
                    bytes[..rest.len()].copy_from_slice(rest);
                    u64::from_be_bytes(bytes)
                }
            }
            Data::Source(source) => source_word(&**source, byte_pos, &mut self.len),
            Data::Lent(source) => source_word(*source, byte_pos, &mut self.len),
            // Read again from a lent source before any bit is consumed.
            Data::Detached => 0,
        };

        self.buffer = word << self.bit_pos();
        // The buffer contains bits up to `byte_pos * 8 + 64`. Refill once fewer
        // than 32 of them are left.
        self.limit = (byte_pos * 8 + 32).min(self.len.saturating_mul(8));
    }
}

/// The 8 bytes of `source` from `byte_pos`, those past its end zero: a source tells
/// where it ends, which `len` is set to, only once read there.
fn source_word(source: &dyn Source, byte_pos: usize, len: &mut usize) -> u64 {
    let mut bytes = [0; 8];
    if byte_pos < *len {
        let n = (*len - byte_pos).min(8);
        let read = source.read_at(byte_pos, &mut bytes[..n]);
        if read < n {
            *len = byte_pos + read;
            bytes[read..].fill(0);
        }
    }
    u64::from_be_bytes(bytes)
}
