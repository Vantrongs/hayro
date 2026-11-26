use crate::object::Dict;
use crate::object::dict::keys::{
    BLACK_IS_1, COLUMNS, ENCODED_BYTE_ALIGN, END_OF_BLOCK, END_OF_LINE, K, ROWS,
};
use crate::object::stream::{FilterResult, ImageColorSpace, ImageData, ImageDecodeParams};
use alloc::borrow::Cow;
use alloc::vec::Vec;
use hayro_ccitt::{DecodeSettings, Decoder, EncodingMode};

pub(crate) fn decode(
    data: &[u8],
    params: &Dict<'_>,
    image_params: &ImageDecodeParams,
) -> Option<FilterResult<'static>> {
    Fax::new(params, image_params.height, None).decode(data)
}

pub(crate) struct Fax {
    settings: DecodeSettings,
    white: u8,
}

impl Fax {
    pub(crate) fn new(
        params: &Dict<'_>,
        height: u32,
        cancel: Option<crate::sync::CancelArc<core::sync::atomic::AtomicBool>>,
    ) -> Self {
        let k = params.get::<i32>(K).unwrap_or(0);

        let columns = params.get::<usize>(COLUMNS).unwrap_or(1728) as u32;
        // The image height takes precedence over `Rows`.
        let rows = if height > 0 {
            height
        } else {
            params.get::<u32>(ROWS).unwrap_or(0)
        };

        let settings = DecodeSettings {
            columns,
            rows,
            end_of_block: params.get::<bool>(END_OF_BLOCK).unwrap_or(true),
            end_of_line: params.get::<bool>(END_OF_LINE).unwrap_or(false),
            rows_are_byte_aligned: params.get::<bool>(ENCODED_BYTE_ALIGN).unwrap_or(false),
            encoding: if k < 0 {
                EncodingMode::Group4
            } else if k == 0 {
                EncodingMode::Group3_1D
            } else {
                EncodingMode::Group3_2D
            },
            strict: false,
            cancel: cancel.map(|flag| {
                hayro_ccitt::CancelHandle::new(move || {
                    flag.load(core::sync::atomic::Ordering::Relaxed)
                })
            }),
        };
        let white: u8 = if params.get::<bool>(BLACK_IS_1).unwrap_or(false) {
            0x00
        } else {
            0xFF
        };

        Self { settings, white }
    }

    pub(crate) fn decode(&self, data: &[u8]) -> Option<FilterResult<'static>> {
        let (columns, rows, white) = (self.settings.columns, self.settings.rows, self.white);
        struct BitPackDecoder {
            output: Vec<u8>,
            columns: u32,
            stride: usize,
            white: u8,
            last_byte_mask: u8,
            decoded_rows: u32,
        }

        impl Decoder for BitPackDecoder {
            fn push_row(&mut self, changes: &[u32]) {
                let start = self.output.len();
                let end = start + self.stride;

                self.output.resize(end, self.white);

                let row = &mut self.output[start..end];

                // Rows are prefilled with white, so flipping a span makes it black.
                for (start, end) in black_spans(changes, self.columns) {
                    let (first, last) = (start / 8, (end - 1) / 8);
                    let first_mask = 0xFF_u8 >> (start % 8);
                    let last_mask = 0xFF_u8 << (7 - (end - 1) % 8);

                    if first == last {
                        row[first] ^= first_mask & last_mask;
                    } else {
                        row[first] ^= first_mask;
                        fill_short(&mut row[first + 1..last], !self.white);
                        row[last] ^= last_mask;
                    }
                }

                if let Some(last) = row.last_mut() {
                    *last &= self.last_byte_mask;
                }

                self.decoded_rows += 1;
            }
        }

        let stride = (columns as usize).div_ceil(8);
        let mut decoder = BitPackDecoder {
            output: Vec::new(),
            columns,
            stride,
            white,
            last_byte_mask: 0xFF_u8 << ((8 - columns % 8) % 8),
            decoded_rows: 0,
        };
        let result = hayro_ccitt::decode(data, &mut decoder, &self.settings);

        // Lenient: return partial output if at least one row decoded (0001763.pdf).
        if self
            .settings
            .cancel
            .as_ref()
            .is_some_and(|c| c.is_cancelled())
            || result.is_err() && decoder.decoded_rows == 0
        {
            return None;
        }

        Some(FilterResult {
            data: Cow::Owned(decoder.output),
            image_data: Some(ImageData {
                alpha: None,
                color_space: Some(ImageColorSpace::Gray),
                bits_per_component: 1,
                width: columns,
                height: if rows == 0 {
                    decoder.decoded_rows
                } else {
                    rows
                },
            }),
        })
    }
}

fn black_spans(changes: &[u32], columns: u32) -> impl Iterator<Item = (usize, usize)> + '_ {
    changes.chunks(2).map(move |span| {
        let end = span.get(1).copied().unwrap_or(columns);
        (span[0] as usize, end as usize)
    })
}

#[inline(always)]
fn fill_short<T: Copy>(slice: &mut [T], value: T) {
    let len = slice.len();

    // Overlapping fixed-size stores beat `memset` for spans up to 32.
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

    #[test]
    fn issue1258() {
        let params = Dict::from_bytes(b"<< /K 0 /Columns 8 /Rows 1 >>").unwrap();

        let decoded = decode(&[0x35, 0x14], &params, &ImageDecodeParams::default()).unwrap();

        assert_eq!(decoded.data.as_ref(), &[0]);
        assert_eq!(decoded.image_data.unwrap().height, 1);
    }

    #[test]
    fn unknown_rows() {
        let params = Dict::from_bytes(b"<< /K 0 /Columns 8 >>").unwrap();

        let decoded = decode(&[0x35, 0x14], &params, &ImageDecodeParams::default()).unwrap();
        assert_eq!(decoded.data.as_ref(), &[0]);
        assert_eq!(decoded.image_data.unwrap().height, 1);

        let image_params = ImageDecodeParams {
            is_indexed: true,
            ..Default::default()
        };
        let decoded = decode(&[0x35, 0x14], &params, &image_params).unwrap();
        assert_eq!(decoded.data.as_ref(), &[0]);
        assert_eq!(decoded.image_data.unwrap().height, 1);
    }
}
