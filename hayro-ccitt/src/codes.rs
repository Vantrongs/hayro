use crate::bit_reader::BitReader;
use crate::context::Color;
use crate::tables::{BLACK_LOOKUP, LOOKUP_BITS, LOOKUP_INVALID, WHITE_LOOKUP};
use crate::{DecodeError, Decoder, Result};

/// 2D coding modes (T.4 Section 4.2.1.3.2, T.6 Section 2.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Pass mode (T.4 Section 4.2.1.3.2a, T.6 Section 2.2.3.1).
    Pass,
    /// Horizontal mode (T.4 Section 4.2.1.3.2c, T.6 Section 2.2.3.3).
    Horizontal,
    /// Vertical mode with offset (T.4 Section 4.2.1.3.2b, T.6 Section 2.2.3.2).
    Vertical(i8),
}

impl BitReader<'_> {
    /// Decode a run length for the specified color.
    #[inline(always)]
    pub(crate) fn decode_run(&mut self, color: Color, decoder: &mut impl Decoder) -> Result<u32> {
        match color {
            Color::White => self.decode_run_inner(&WHITE_LOOKUP, decoder),
            Color::Black => self.decode_run_inner(&BLACK_LOOKUP, decoder),
        }
    }

    /// Decode a 2D mode code (T.4 Table 4, T.6 Table 1).
    #[inline(always)]
    pub(crate) fn decode_mode(&mut self) -> Result<Mode> {
        let bits = self.peek_u32();

        if bits >> 31 == 1 {
            self.consume(1)?;
            return Ok(Mode::Vertical(0));
        }

        let zeros = bits.leading_zeros();
        // The bit following the first 1 determines the direction of vertical
        // modes.
        let offset = if bits.checked_shl(zeros + 1).unwrap_or(0) >> 31 == 1 {
            1
        } else {
            -1
        };

        let (mode, len) = match zeros {
            1 => (Mode::Vertical(offset), 3),
            2 => (Mode::Horizontal, 3),
            3 => (Mode::Pass, 4),
            4 => (Mode::Vertical(2 * offset), 6),
            5 => (Mode::Vertical(3 * offset), 7),
            _ => {
                self.consume(6)?;
                return Err(DecodeError::InvalidCode);
            }
        };

        self.consume(len)?;

        Ok(mode)
    }

    /// Read EOL (End-of-Line) codes if present (T.4 Section 4.1.2).
    ///
    /// EOL is defined as `000000000001` (11 zeros followed by a 1).
    /// Fill bits (T.4 Section 4.1.3) may precede the EOL as a variable-length
    /// string of zeros.
    pub(crate) fn read_eol_if_available(&mut self, decoder: &mut impl Decoder) -> Result<usize> {
        self.read_eols(usize::MAX, decoder)
    }

    /// Like [`Self::read_eol_if_available`], but reads at most `max` EOLs.
    pub(crate) fn read_eols(&mut self, max: usize, decoder: &mut impl Decoder) -> Result<usize> {
        let mut count = 0;

        // An EOL starts with 11 zeros, while a line mostly starts with a code
        // that has a 1 among them.
        while count < max
            && self.peek_u32().leading_zeros() >= 11
            && let Some((zeros, after)) = self.zero_run(decoder)?
        {
            if zeros < 11 {
                break;
            }

            *self = after;
            count += 1;
        }

        Ok(count)
    }

    /// Advance to the start of the next EOL (including any fill preceding it),
    /// without consuming it. Returns `false` and doesn't advance if there is
    /// none.
    pub(crate) fn skip_to_eol(&mut self, decoder: &mut impl Decoder) -> Result<bool> {
        let mut reader = self.clone();

        while let Some((zeros, after)) = reader.zero_run(decoder)? {
            if zeros >= 11 {
                *self = reader;
                return Ok(true);
            }

            reader = after;
        }

        Ok(false)
    }

    /// Decode a run length using the given lookup table (T.4 Section 4.1.1, T.6 Section 2.2.4).
    ///
    /// Run lengths 0-63 use terminating codes.
    /// Run lengths 64+ use one or more make-up codes followed by a terminating code.
    #[inline(always)]
    fn decode_run_inner(
        &mut self,
        lookup: &[u16; 1 << LOOKUP_BITS],
        decoder: &mut impl Decoder,
    ) -> Result<u32> {
        let mut total: u32 = 0;

        loop {
            let entry = lookup[(self.peek_u32() >> (32 - LOOKUP_BITS)) as usize];
            self.consume((entry >> 12) as usize)?;

            let len = entry & 0xFFF;

            if len == LOOKUP_INVALID {
                return Err(DecodeError::InvalidCode);
            }

            total = total.checked_add(len as u32).ok_or(DecodeError::Overflow)?;

            // For decoding black/white runs, less than 64 means we have
            // a terminating code.
            if len < 64 {
                return Ok(total);
            }

            self.poll(decoder)?;
        }
    }

    /// Count the zeros at the current position and return their number with a
    /// reader positioned after the 1 that terminates them, if there is one.
    fn zero_run(&self, decoder: &mut impl Decoder) -> Result<Option<(usize, Self)>> {
        let mut reader = self.clone();
        let mut zeros = 0;

        loop {
            reader.poll(decoder)?;
            let leading = reader.peek_u32().leading_zeros() as usize;

            if leading < 32 {
                // Bits past the end of the data are zero, so the 1 is part of
                // it and can be consumed.
                reader.consume(leading + 1)?;
                return Ok(Some((zeros + leading, reader)));
            }

            if reader.consume(32).is_err() {
                return Ok(None);
            }

            zeros += 32;
        }
    }
}
