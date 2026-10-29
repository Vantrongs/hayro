use crate::bit_reader::BitReader;
use crate::context::{DecoderContext, decode_2d_line};
use crate::{Decoder, Result};

/// Decodes the next line of Group 4 data (T.6), if the data holds one: false,
/// with no line decoded, once it ends.
pub(crate) fn step(
    ctx: &mut DecoderContext,
    reader: &mut BitReader<'_>,
    decoder: &mut impl Decoder,
) -> Result<bool> {
    reader.poll(decoder)?;
    // EOLs aren't part of Group 4, but PDF requires decoders to accept them
    // anyway. An EOFB consists of two consecutive EOLs. Data that follows
    // an EOFB (after padding to a byte boundary) can start like another
    // EOL, so no more than that is read.
    let eol_count = if ctx.settings.end_of_block {
        reader.read_eols(2, decoder)?
    } else {
        reader.read_eol_if_available(decoder)?
    };

    if ctx.settings.end_of_block && eol_count >= 2 {
        return Ok(false);
    }

    if (ctx.settings.rows != 0 && ctx.decoded_rows == ctx.settings.rows) || reader.at_end() {
        return Ok(false);
    }

    decode_2d_line(ctx, reader, decoder)?;
    ctx.next_line(decoder);

    if ctx.settings.rows_are_byte_aligned {
        reader.align();
    }

    Ok(true)
}
