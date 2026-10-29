use crate::filter::CcittParams;
use crate::object::stream::{FilterResult, ImageColorSpace, ImageData, ImageDecodeParams};
use crate::work::{Meter, POLL_BYTES};
use alloc::borrow::Cow;
use alloc::vec::Vec;
use hayro_ccitt::{DecodeSettings, Decoder, DecoderContext, EncodingMode};

/// Makes room in `output` for `needed` bytes: it grows to the `declared` bytes
/// divided by eight as often as that leaves room for them, and no further once they
/// are at most [`POLL_BYTES`]. It so holds less than eight times `needed`, or at most
/// `POLL_BYTES`: a stream cut short or stopped costs what it decoded, not what `Rows`
/// and `Columns` declare, while the last growth to the declared size copies an eighth
/// of it. Past the declared size, or without one, it doubles.
#[inline(always)]
fn reserve(output: &mut Vec<u8>, needed: usize, declared: usize) {
    if needed > output.capacity() {
        grow_to(output, needed, declared);
    }
}

#[cold]
#[inline(never)]
fn grow_to(output: &mut Vec<u8>, needed: usize, declared: usize) {
    let mut target = declared;
    while target > POLL_BYTES && target / 8 >= needed {
        target /= 8;
    }
    if target < needed {
        target = needed.max(2 * output.len());
    }
    output.reserve_exact(target - output.len());
}

/// Appends `count` copies of `byte` to `output`, a counted piece at a time, with room
/// made as in [`reserve`]; stops appending once `meter`'s hook asks to stop.
fn push_counted(output: &mut Vec<u8>, byte: u8, count: usize, declared: usize, meter: &Meter<'_>) {
    let mut left = count;
    while left > 0 && !meter.cancelled() {
        let n = left.min(POLL_BYTES);
        let needed = output.len() + n;
        reserve(output, needed, declared);
        output.resize(needed, byte);
        left -= n;
        let _ = meter.work(n);
    }
}

/// Grows `output` with white rows up to `end`, the end of the row about to be
/// written, counted on `meter` (see [`push_counted`]); `false`, with the row left
/// out, once its hook asks to stop.
fn grow(output: &mut Vec<u8>, end: usize, declared: usize, white: u8, meter: &Meter<'_>) -> bool {
    push_counted(
        output,
        white,
        end.saturating_sub(output.len()),
        declared,
        meter,
    );
    output.len() >= end
}

/// Like [`grow`], but fills white [`POLL_BYTES`] ahead of `end` within the room
/// made, so that the rows that follow are written into it without growing `output`
/// row by row, while a stream that ends early whitens no more than that past its
/// last row rather than all the room, up to eight times the rows written.
#[cold]
#[inline(never)]
fn grow_room(
    output: &mut Vec<u8>,
    end: usize,
    declared: usize,
    white: u8,
    meter: &Meter<'_>,
) -> bool {
    reserve(output, end, declared);
    let ahead = output.capacity().min(end.saturating_add(POLL_BYTES));
    grow(output, ahead, declared, white, meter);
    output.len() >= end
}

/// White pixels a stream that ends early is padded with whatever it decoded.
const MIN_WHITE_PAD: usize = 1 << 16;

/// The widest row, in pixels, that is written whole into white room made ahead of
/// it ([`grow_room`]) and then counted at once ([`row_work`]): at most one and a half
/// times as much, which stays within [`POLL_BYTES`].
const WHOLE_ROW: usize = POLL_BYTES / 2;

/// The work of writing the black spans `changes` gives over a white row of `bytes`:
/// at most the row's bytes, and a step per span, counted without going through them.
#[inline(always)]
fn row_work(bytes: usize, changes: &[u32]) -> usize {
    bytes + changes.len().div_ceil(2)
}

/// The data decoded, its work counted on `meter`; `None` once its hook asks to stop.
pub(crate) fn decode(
    data: &[u8],
    params: &CcittParams,
    image_params: &ImageDecodeParams,
    meter: &Meter<'_>,
) -> Option<FilterResult<'static>> {
    let k = params.k;

    let columns = params.columns;
    let rows = params.rows;
    let output_len = (columns as usize).checked_mul(rows as usize)?;

    let mut settings = DecodeSettings::new(columns, rows);
    settings.end_of_block = params.end_of_block;
    settings.end_of_line = params.end_of_line;
    settings.rows_are_byte_aligned = params.encoded_byte_align;
    settings.encoding = if k < 0 {
        EncodingMode::Group4
    } else if k == 0 {
        EncodingMode::Group3_1D
    } else {
        EncodingMode::Group3_2D
    };
    // The value that pixels which are white in the encoding are written as.
    let white: u8 = if params.black_is_1 { 0x00 } else { 0xFF };

    // Whenever possible (if we don't have an indexed color space), we convert
    // the data as 8-bit instead of 1-bit, so that it can be easier converted
    // into an RGBA8 image.

    let (decoded, bpc, decoded_rows) = if image_params.is_indexed {
        struct BitPackDecoder<'m> {
            output: Vec<u8>,
            /// The bytes `Rows` rows take.
            declared: usize,
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

                if self.columns as usize <= WHOLE_ROW {
                    // Rows pushed after a stop, until the decoder asks again, cost nothing.
                    if self.meter.cancelled()
                        || end > self.output.len()
                            && !grow_room(
                                &mut self.output,
                                end,
                                self.declared,
                                self.white,
                                self.meter,
                            )
                    {
                        return;
                    }

                    let row = &mut self.output[start..end];

                    for (start, end) in black_spans(changes, self.columns) {
                        let (first, last) = (start / 8, (end - 1) / 8);
                        let first_mask = 0xFF_u8 >> (start % 8);
                        let last_mask = 0xFF_u8 << (7 - (end - 1) % 8);

                        if first == last {
                            row[first] ^= first_mask & last_mask;
                        } else {
                            row[first] ^= first_mask;
                            row[last] ^= last_mask;
                            fill_short(&mut row[first + 1..last], !self.white);
                        }
                    }

                    if let Some(last) = row.last_mut() {
                        *last &= self.last_byte_mask;
                    }

                    if self.meter.work(row_work(self.stride, changes)).is_ok() {
                        self.decoded_rows += 1;
                    }
                    return;
                }

                if !grow(&mut self.output, end, self.declared, self.white, self.meter) {
                    return;
                }

                let row = &mut self.output[start..end];

                // The row is prefilled with white, so flipping the bits of a
                // span turns it black.
                for (start, end) in black_spans(changes, self.columns) {
                    let (first, last) = (start / 8, (end - 1) / 8);
                    let first_mask = 0xFF_u8 >> (start % 8);
                    let last_mask = 0xFF_u8 << (7 - (end - 1) % 8);

                    let filled = if first == last {
                        row[first] ^= first_mask & last_mask;
                        self.meter.work(1).is_ok()
                    } else {
                        row[first] ^= first_mask;
                        row[last] ^= last_mask;
                        fill_counted(&mut row[first + 1..last], !self.white, self.meter)
                    };
                    if !filled {
                        return;
                    }
                }

                if let Some(last) = row.last_mut() {
                    *last &= self.last_byte_mask;
                }

                self.decoded_rows += 1;
            }

            /// A bit read counts a step, as a code takes at least one.
            fn stop(&mut self, bits: usize) -> bool {
                self.meter.work(bits).is_err()
            }
        }

        let stride = (columns as usize).div_ceil(8);
        let mut decoder = BitPackDecoder {
            output: Vec::new(),
            declared: stride.checked_mul(rows as usize)?,
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
            /// The bytes `Rows` rows take.
            declared: usize,
            columns: u32,
            white: u8,
            decoded_rows: u32,
            meter: &'m Meter<'m>,
        }

        impl Decoder for Luma8Decoder<'_> {
            fn push_row(&mut self, changes: &[u32]) {
                let start = self.decoded_rows as usize * self.columns as usize;
                let end = start + self.columns as usize;

                if self.columns as usize <= WHOLE_ROW {
                    // Rows pushed after a stop, until the decoder asks again, cost nothing.
                    if self.meter.cancelled()
                        || end > self.output.len()
                            && !grow_room(
                                &mut self.output,
                                end,
                                self.declared,
                                self.white,
                                self.meter,
                            )
                    {
                        return;
                    }

                    let row = &mut self.output[start..end];

                    for (start, end) in black_spans(changes, self.columns) {
                        fill_short(&mut row[start..end], !self.white);
                    }

                    if self.meter.work(row_work(row.len(), changes)).is_ok() {
                        self.decoded_rows += 1;
                    }
                    return;
                }

                if !grow(&mut self.output, end, self.declared, self.white, self.meter) {
                    return;
                }

                let row = &mut self.output[start..end];

                for (start, end) in black_spans(changes, self.columns) {
                    if !fill_counted(&mut row[start..end], !self.white, self.meter) {
                        return;
                    }
                }

                self.decoded_rows += 1;
            }

            /// A bit read counts a step, as a code takes at least one.
            fn stop(&mut self, bits: usize) -> bool {
                self.meter.work(bits).is_err()
            }
        }

        let mut decoder = Luma8Decoder {
            output: Vec::new(),
            declared: output_len,
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

        decoder
            .output
            .truncate(decoder.decoded_rows as usize * columns as usize);

        // Rows a stream that ends early leaves out are white, while padding them takes
        // no more than the rows decoded (or a small image); otherwise the image ends
        // with the rows decoded, as one whose data breaks off does.
        let missing = output_len.saturating_sub(decoder.output.len());
        if result.is_ok() && rows != 0 && missing <= decoder.output.len().max(MIN_WHITE_PAD) {
            push_counted(&mut decoder.output, white, missing, output_len, meter);
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

/// Fills `slice` with `value`, a counted piece at a time; `false`, with the rest
/// left out, once `meter`'s hook asks to stop. A span costs a step even when empty.
#[inline(always)]
fn fill_counted(slice: &mut [u8], value: u8, meter: &Meter<'_>) -> bool {
    if slice.len() <= POLL_BYTES {
        fill_short(slice, value);
        return meter.work(slice.len() + 1).is_ok();
    }

    slice.chunks_mut(POLL_BYTES).all(|piece| {
        piece.fill(value);
        meter.work(piece.len()).is_ok()
    })
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
    use crate::filter::CcittParams;
    use crate::object::FromBytes;
    use crate::object::dict::Dict;
    use crate::object::stream::ImageDecodeParams;
    use crate::work::Meter;

    #[test]
    fn issue1258() {
        let params = CcittParams::of(
            &Dict::from_bytes(b"<< /K 0 /Columns 8 /Rows 1 >>").unwrap(),
            0,
        );

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
        let params = CcittParams::of(&Dict::from_bytes(b"<< /K 0 /Columns 8 >>").unwrap(), 0);

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

    /// Writing the black spans of a row counts on the meter, however short they are.
    #[cfg(all(debug_assertions, feature = "std"))]
    #[test]
    fn short_spans_count() {
        use crate::work::TOLD;

        // A row of alternating white and black pixels in horizontal mode: `001`, a
        // white run of 1 (`000111`) and a black run of 1 (`010`), twice per 3 bytes.
        let pairs = 100_000;
        let data = [0x23, 0xA2, 0x3A].repeat(pairs / 2);
        let dict = format!("<< /K -1 /Columns {} /Rows 1 >>", 2 * pairs);
        let params = CcittParams::of(&Dict::from_bytes(dict.as_bytes()).unwrap(), 0);
        let told = |is_indexed| {
            let image_params = ImageDecodeParams {
                is_indexed,
                ..Default::default()
            };
            TOLD.set(0);
            let decoded = decode(&data, &params, &image_params, &Meter::default()).unwrap();
            (TOLD.get(), decoded.data.into_owned())
        };
        let (luma, data) = told(false);
        assert!(data.chunks(2).all(|pair| pair == [0xFF, 0x00]));
        let (bits, data) = told(true);
        assert!(data.iter().all(|&byte| byte == 0xAA));
        // The row's bytes and a step per bit read, at least one per code, count anyway;
        // each black span adds one.
        assert!(luma >= 2 * pairs + pairs + pairs, "{luma}");
        assert!(bits >= 2 * pairs / 8 + pairs + pairs, "{bits}");
    }
}
