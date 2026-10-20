use crate::object::Dict;
use crate::object::dict::keys::{
    BLACK_IS_1, COLUMNS, ENCODED_BYTE_ALIGN, END_OF_BLOCK, END_OF_LINE, K, ROWS,
};
use crate::object::stream::{FilterResult, ImageColorSpace, ImageData, ImageDecodeParams};
use crate::work::{Meter, POLL_BYTES};
use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::iter;
use hayro_ccitt::{DecodeSettings, Decoder, DecoderContext, EncodingMode};

/// Appends `count` copies of `byte` to `output`, a counted piece at a time; stops
/// appending once `meter`'s hook asks to stop.
fn push_counted(output: &mut Vec<u8>, byte: u8, count: usize, meter: &Meter<'_>) {
    let mut left = count;
    while left > 0 && !meter.cancelled() {
        let n = left.min(POLL_BYTES);
        output.extend(iter::repeat_n(byte, n));
        left -= n;
        let _ = meter.work(n);
    }
}

/// Grows `output` with white rows up to `end`, the end of the row about to be
/// written, counted on `meter`; `false`, with the row left out, once its hook asks
/// to stop. The output grows with the rows decoded: a stream cut short costs what
/// it holds, not what `Rows` declares.
fn grow(output: &mut Vec<u8>, end: usize, white: u8, meter: &Meter<'_>) -> bool {
    push_counted(output, white, end.saturating_sub(output.len()), meter);
    output.len() >= end
}

/// White pixels a stream that ends early is padded with whatever it decoded.
const MIN_WHITE_PAD: usize = 1 << 16;

/// The data decoded, its work counted on `meter`; `None` once its hook asks to stop.
pub(crate) fn decode(
    data: &[u8],
    params: &Dict<'_>,
    image_params: &ImageDecodeParams,
    meter: &Meter<'_>,
) -> Option<FilterResult<'static>> {
    let k = params.get::<i32>(K).unwrap_or(0);

    let columns = params.get::<usize>(COLUMNS).unwrap_or(1728) as u32;
    // `Rows` should match the height of the image, but in case it doesn't,
    // the image height takes precedence (as in other viewers).
    let rows = if image_params.height > 0 {
        image_params.height
    } else {
        params.get::<u32>(ROWS).unwrap_or(0)
    };
    let output_len = (columns as usize).checked_mul(rows as usize)?;

    let mut settings = DecodeSettings::new(columns, rows);
    settings.end_of_block = params.get::<bool>(END_OF_BLOCK).unwrap_or(true);
    settings.end_of_line = params.get::<bool>(END_OF_LINE).unwrap_or(false);
    settings.rows_are_byte_aligned = params.get::<bool>(ENCODED_BYTE_ALIGN).unwrap_or(false);
    settings.encoding = if k < 0 {
        EncodingMode::Group4
    } else if k == 0 {
        EncodingMode::Group3_1D
    } else {
        EncodingMode::Group3_2D
    };
    // The value that pixels which are white in the encoding are written as.
    let white: u8 = if params.get::<bool>(BLACK_IS_1).unwrap_or(false) {
        0x00
    } else {
        0xFF
    };

    // Whenever possible (if we don't have an indexed color space), we convert
    // the data as 8-bit instead of 1-bit, so that it can be easier converted
    // into an RGBA8 image.

    let (decoded, bpc, decoded_rows) = if image_params.is_indexed {
        struct BitPackDecoder<'m> {
            output: Vec<u8>,
            columns: u32,
            stride: usize,
            white: u8,
            /// The bits of the last byte in a row that belong to the image.
            last_byte_mask: u8,
            decoded_rows: u32,
            meter: &'m Meter<'m>,
        }

        impl Decoder for BitPackDecoder<'_> {
            fn push_row(&mut self, changes: &[u32]) {
                let start = self.decoded_rows as usize * self.stride;
                let end = start + self.stride;

                if !grow(&mut self.output, end, self.white, self.meter) {
                    return;
                }

                let row = &mut self.output[start..end];

                // The row is prefilled with white, so flipping the bits of a
                // span turns it black.
                for (start, end) in black_spans(changes, self.columns) {
                    let (first, last) = (start / 8, (end - 1) / 8);
                    let first_mask = 0xFF_u8 >> (start % 8);
                    let last_mask = 0xFF_u8 << (7 - (end - 1) % 8);

                    if first == last {
                        row[first] ^= first_mask & last_mask;
                    } else {
                        row[first] ^= first_mask;
                        fill_counted(&mut row[first + 1..last], !self.white, self.meter);
                        row[last] ^= last_mask;
                    }
                }

                if let Some(last) = row.last_mut() {
                    *last &= self.last_byte_mask;
                }

                self.decoded_rows += 1;
            }

            fn stop(&mut self) -> bool {
                self.meter.work(1).is_err()
            }
        }

        let stride = (columns as usize).div_ceil(8);
        stride.checked_mul(rows as usize)?;
        let mut decoder = BitPackDecoder {
            output: Vec::new(),
            columns,
            stride,
            white,
            last_byte_mask: 0xFF_u8 << ((8 - columns % 8) % 8),
            decoded_rows: 0,
            meter,
        };
        let mut context = DecoderContext::new(settings);
        let result = hayro_ccitt::decode(data, &mut decoder, &mut context);

        // If we decoded at least one row, let's be lenient and return what we got.
        // See also 0001763.pdf.
        if result.is_err() && decoder.decoded_rows == 0 || meter.cancelled() {
            return None;
        }

        decoder
            .output
            .truncate(decoder.decoded_rows as usize * stride);

        (decoder.output, 1, decoder.decoded_rows)
    } else {
        struct Luma8Decoder<'m> {
            output: Vec<u8>,
            columns: u32,
            white: u8,
            decoded_rows: u32,
            meter: &'m Meter<'m>,
        }

        impl Decoder for Luma8Decoder<'_> {
            fn push_row(&mut self, changes: &[u32]) {
                let start = self.decoded_rows as usize * self.columns as usize;
                let end = start + self.columns as usize;

                if !grow(&mut self.output, end, self.white, self.meter) {
                    return;
                }

                let row = &mut self.output[start..end];

                for (start, end) in black_spans(changes, self.columns) {
                    fill_counted(&mut row[start..end], !self.white, self.meter);
                }

                self.decoded_rows += 1;
            }

            fn stop(&mut self) -> bool {
                self.meter.work(1).is_err()
            }
        }

        let mut decoder = Luma8Decoder {
            output: Vec::new(),
            columns,
            white,
            decoded_rows: 0,
            meter,
        };
        let mut context = DecoderContext::new(settings);
        let result = hayro_ccitt::decode(data, &mut decoder, &mut context);

        if result.is_err() && decoder.decoded_rows == 0 || meter.cancelled() {
            return None;
        }

        // Rows a stream that ends early leaves out are white, while padding them takes
        // no more than the rows decoded (or a small image); otherwise the image ends
        // with the rows decoded, as one whose data breaks off does.
        let missing = output_len.saturating_sub(decoder.output.len());
        if result.is_ok() && rows != 0 && missing <= decoder.output.len().max(MIN_WHITE_PAD) {
            push_counted(&mut decoder.output, white, missing, meter);
            if meter.cancelled() {
                return None;
            }
        }

        (decoder.output, 8, decoder.decoded_rows)
    };

    Some(FilterResult {
        data: Cow::Owned(decoded),
        image_data: Some(ImageData {
            alpha: None,
            color_space: Some(ImageColorSpace::Gray),
            bits_per_component: bpc,
            width: settings.columns,
            height: if rows == 0 { decoded_rows } else { rows },
        }),
    })
}

fn black_spans(changes: &[u32], columns: u32) -> impl Iterator<Item = (usize, usize)> + '_ {
    changes.chunks(2).map(move |span| {
        let end = span.get(1).copied().unwrap_or(columns);
        (span[0] as usize, end as usize)
    })
}

/// Fills `slice` with `value`; a slice longer than [`POLL_BYTES`] a counted piece
/// at a time, stopping once `meter`'s hook asks to stop. Shorter ones are counted
/// with the row they are in ([`grow`]).
#[inline(always)]
fn fill_counted(slice: &mut [u8], value: u8, meter: &Meter<'_>) {
    if slice.len() <= POLL_BYTES {
        fill_short(slice, value);
        return;
    }

    for piece in slice.chunks_mut(POLL_BYTES) {
        if meter.cancelled() {
            return;
        }
        piece.fill(value);
        let _ = meter.work(piece.len());
    }
}

#[inline(always)]
fn fill_short<T: Copy>(slice: &mut [T], value: T) {
    let len = slice.len();

    // Most spans are short, and for them a `memset` call costs more than the
    // stores themselves. Overlapping fixed-size stores cover every length up
    // to 32.
    match len {
        0 => {}
        1..4 => {
            slice[0] = value;
            slice[len / 2] = value;
            slice[len - 1] = value;
        }
        4..8 => {
            slice[..4].copy_from_slice(&[value; 4]);
            slice[len - 4..].copy_from_slice(&[value; 4]);
        }
        8..16 => {
            slice[..8].copy_from_slice(&[value; 8]);
            slice[len - 8..].copy_from_slice(&[value; 8]);
        }
        16..=32 => {
            slice[..16].copy_from_slice(&[value; 16]);
            slice[len - 16..].copy_from_slice(&[value; 16]);
        }
        _ => slice.fill(value),
    }
}

#[cfg(test)]
mod tests {
    use super::decode;
    use crate::object::FromBytes;
    use crate::object::dict::Dict;
    use crate::object::stream::ImageDecodeParams;
    use crate::work::Meter;

    #[test]
    fn issue1258() {
        let params = Dict::from_bytes(b"<< /K 0 /Columns 8 /Rows 1 >>").unwrap();

        let decoded = decode(
            &[0x35, 0x14],
            &params,
            &ImageDecodeParams::default(),
            &Meter::default(),
        )
        .unwrap();

        assert_eq!(decoded.data.as_ref(), &[0; 8]);
        assert_eq!(decoded.image_data.unwrap().height, 1);
    }

    #[test]
    fn unknown_rows() {
        let params = Dict::from_bytes(b"<< /K 0 /Columns 8 >>").unwrap();

        let decoded = decode(
            &[0x35, 0x14],
            &params,
            &ImageDecodeParams::default(),
            &Meter::default(),
        )
        .unwrap();
        assert_eq!(decoded.data.as_ref(), &[0; 8]);
        assert_eq!(decoded.image_data.unwrap().height, 1);

        let image_params = ImageDecodeParams {
            is_indexed: true,
            ..Default::default()
        };
        let decoded = decode(&[0x35, 0x14], &params, &image_params, &Meter::default()).unwrap();
        assert_eq!(decoded.data.as_ref(), &[0]);
        assert_eq!(decoded.image_data.unwrap().height, 1);
    }
}
