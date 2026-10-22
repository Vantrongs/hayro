mod image;
mod mask;

pub(crate) use image::{DecodedImage, decode_image};
pub(crate) use mask::{DecodedMask, decode_mask};

use crate::InterpreterWarning;
use crate::color::ColorSpace;
use crate::function::interpolate;
use crate::x_object::image::{ImageKind, ImageXObject};
use hayro_syntax::bit_reader::BitReader;
use hayro_syntax::object::Array;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::stream::{FilterResult, ImageColorSpace, ImageDecodeParams};
use smallvec::SmallVec;
use std::iter;

struct DecodeContext<'a> {
    decoded: FilterResult<'a>,
    width: u32,
    height: u32,
    scale_factors: (f32, f32),
    color_space: ColorSpace,
    bits_per_component: u8,
    decode_arr: SmallVec<[(f32, f32); 4]>,
}

fn decode_context<'a>(
    obj: &ImageXObject<'a>,
    target_dimension: Option<(u32, u32)>,
) -> Option<DecodeContext<'a>> {
    let dict = obj.stream.dict();
    let dict_bpc = dict
        .get::<u8>(BPC)
        .or_else(|| dict.get::<u8>(BITS_PER_COMPONENT));
    let color_space = obj.color_space.clone();
    let is_indexed = obj.color_space.as_ref().is_some_and(|cs| cs.is_indexed());

    let decode_params = ImageDecodeParams {
        is_indexed,
        bpc: dict_bpc,
        num_components: color_space.as_ref().map(|c| c.num_components()),
        target_dimension,
        width: obj.width(),
        height: obj.height(),
    };

    let decoded = obj
        .stream
        .decoded_image(&decode_params)
        .map_err(|_| (obj.warning_sink)(InterpreterWarning::ImageDecodeFailure))
        .ok()?;

    let (mut scale_x, mut scale_y) = (1.0, 1.0);

    let (width, height) = decoded
        .image_data
        .as_ref()
        .map(|d| {
            scale_x = obj.width() as f32 / d.width as f32;
            scale_y = obj.height() as f32 / d.height as f32;

            (d.width, d.height)
        })
        .unwrap_or((obj.width(), obj.height()));

    let color_space = color_space
        .or_else(|| {
            decoded
                .image_data
                .as_ref()
                .map(|i| i.color_space)
                .and_then(|c| {
                    c.and_then(|c| match c {
                        ImageColorSpace::Gray => Some(ColorSpace::device_gray()),
                        ImageColorSpace::Rgb => Some(ColorSpace::device_rgb()),
                        ImageColorSpace::Cmyk => Some(ColorSpace::device_cmyk()),
                        ImageColorSpace::Unknown(_) => None,
                    })
                })
        })
        .unwrap_or(ColorSpace::device_gray());

    let fallback_bpc = if obj.kind == ImageKind::StencilMask {
        1
    } else {
        8
    };

    let bits_per_component = decoded
        .image_data
        .as_ref()
        .map(|i| i.bits_per_component)
        .or(dict_bpc)
        .unwrap_or(fallback_bpc);

    let decode_arr = dict
        .get::<Array<'_>>(D)
        .or_else(|| dict.get::<Array<'_>>(DECODE))
        .map(|a| a.iter::<(f32, f32)>().collect::<SmallVec<_>>())
        .unwrap_or(color_space.default_decode_arr(bits_per_component as f32));

    Some(DecodeContext {
        decoded,
        width,
        height,
        scale_factors: (scale_x, scale_y),
        color_space,
        bits_per_component,
        decode_arr,
    })
}

#[must_use]
fn fix_image_length<T: Copy>(
    image: &mut Vec<T>,
    width: u32,
    height: &mut u32,
    filler: T,
    num_components: usize,
) -> Option<()> {
    let row_len = width as usize * num_components;

    if (row_len * *height as usize) <= image.len() {
        // Too much data (or just the right amount), truncate it.
        image.truncate(row_len * *height as usize);
    } else {
        // Too little data, adapt the height and pad.
        *height = image.len().div_ceil(row_len) as u32;

        if !image.len().is_multiple_of(row_len) {
            image.extend(iter::repeat_n(filler, row_len - (image.len() % row_len)));
        }
    }

    if width == 0 || *height == 0 {
        None
    } else {
        Some(())
    }
}

fn decode_u8_samples(
    data: &[u8],
    width: u32,
    height: u32,
    color_space: &ColorSpace,
    bits_per_component: u8,
    decode: &[(f32, f32)],
) -> Option<Vec<u8>> {
    let source_max = 2.0_f32.powi(bits_per_component as i32) - 1.0;
    let num_components = color_space.num_components() as usize;
    let ranges = color_space.component_ranges();
    let indexed_hival = color_space.indexed_hival();

    let decode_component = |value: u32, index: usize| {
        let component_index = index % num_components;
        let (decode_min, decode_max) = *decode.get(component_index)?;
        let decoded = interpolate(value as f32, 0.0, source_max, decode_min, decode_max);

        if let Some(hival) = indexed_hival {
            Some((decoded + 0.5).clamp(0.0, hival as f32) as u8)
        } else {
            let (range_min, range_max) = *ranges.get(component_index)?;
            let normalized = if range_min == range_max {
                0.0
            } else {
                (decoded - range_min) / (range_max - range_min)
            };
            Some((normalized * 255.0 + 0.5) as u8)
        }
    };

    match bits_per_component {
        1..8 | 9..16 => {
            let height = sample_rows(data, width, height, num_components, bits_per_component);
            let mut buf = Vec::with_capacity(width as usize * num_components * height as usize);
            for_each_sample(
                data,
                width,
                height,
                num_components,
                bits_per_component,
                |value, index| {
                    buf.push(decode_component(value, index)?);
                    Some(())
                },
            )?;

            Some(buf)
        }
        8 => Some(
            data.iter()
                .enumerate()
                .map(|(index, value)| decode_component(*value as u32, index))
                .collect::<Option<Vec<_>>>()?,
        ),
        16 => Some(
            data.as_chunks::<2>()
                .0
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    decode_component(u16::from_be_bytes([value[0], value[1]]) as u32, index)
                })
                .collect::<Option<Vec<_>>>()?,
        ),
        _ => {
            warn!("unsupported bits per component: {bits_per_component}");
            None
        }
    }
}

fn unpack_samples(
    data: &[u8],
    width: u32,
    height: u32,
    num_components: usize,
    bits_per_component: u8,
) -> Option<Vec<u16>> {
    match bits_per_component {
        1..8 | 9..16 => {
            let height = sample_rows(data, width, height, num_components, bits_per_component);
            let mut buf = Vec::with_capacity(width as usize * num_components * height as usize);
            for_each_sample(
                data,
                width,
                height,
                num_components,
                bits_per_component,
                |value, _| {
                    buf.push(value as u16);
                    Some(())
                },
            )?;

            Some(buf)
        }
        8 => Some(data.iter().map(|value| *value as u16).collect()),
        16 => Some(
            data.as_chunks::<2>()
                .0
                .iter()
                .map(|value| u16::from_be_bytes([value[0], value[1]]))
                .collect(),
        ),
        _ => {
            warn!("unsupported bits per component: {bits_per_component}");
            None
        }
    }
}

/// The rows of packed samples, at most `height`, that `data` reaches; each row
/// starts on a byte. A last row the data fills in part counts, to be padded.
fn sample_rows(
    data: &[u8],
    width: u32,
    height: u32,
    num_components: usize,
    bits_per_component: u8,
) -> u32 {
    let row_bytes = (width as usize)
        .saturating_mul(num_components)
        .saturating_mul(bits_per_component as usize)
        .div_ceil(8)
        .max(1);
    data.len().div_ceil(row_bytes).min(height as usize) as u32
}

fn for_each_sample(
    data: &[u8],
    width: u32,
    height: u32,
    num_components: usize,
    bits_per_component: u8,
    mut visit: impl FnMut(u32, usize) -> Option<()>,
) -> Option<()> {
    let mut reader = BitReader::new(data);
    let mut index = 0;

    for _ in 0..height {
        for _ in 0..width {
            for _ in 0..num_components {
                // Some images seemingly don't have enough data for their last row,
                // so we just pad it with zeroes.
                visit(reader.read(bits_per_component).unwrap_or(0), index)?;
                index += 1;
            }
        }

        reader.align();
    }

    Some(())
}

#[cfg(test)]
mod tests {
    use super::{decode_u8_samples, fix_image_length, unpack_samples};
    use crate::color::ColorSpace;

    #[test]
    fn unpack_only_the_rows_the_data_holds() {
        // An 8 × 1000 image whose data ends after two rows.
        let samples = unpack_samples(&[0b1010_1010, 0xFF], 8, 1000, 1, 1).unwrap();

        assert_eq!(samples, [1, 0, 1, 0, 1, 0, 1, 0, 1, 1, 1, 1, 1, 1, 1, 1]);
    }

    #[test]
    fn decode_only_the_rows_the_data_holds() {
        // A 16 × 1000 image whose data ends in the middle of the first row,
        // which is padded with zeroes.
        let samples = decode_u8_samples(
            &[0xF0],
            16,
            1000,
            &ColorSpace::device_gray(),
            1,
            &[(0.0, 1.0)],
        )
        .unwrap();

        let mut expected = [0; 16];
        expected[..4].fill(255);
        assert_eq!(samples, expected);
    }

    #[test]
    fn a_partial_last_row_keeps_its_pixels() {
        // The one row of a 100 000 × 1 image whose data ends after eight samples
        // keeps them and is padded to its width.
        let mut samples = unpack_samples(&[0xAA], 100_000, 1, 1, 1).unwrap();
        let mut height = 1;
        fix_image_length(&mut samples, 100_000, &mut height, 0, 1).unwrap();

        assert_eq!(height, 1);
        assert_eq!(samples.len(), 100_000);
        assert_eq!(samples[..8], [1, 0, 1, 0, 1, 0, 1, 0]);
    }

    #[test]
    fn a_short_image_keeps_the_rows_its_data_holds() {
        // A 16 × 4 image whose data stops in its second row keeps that row,
        // padded, and drops the rows the data does not reach.
        let mut samples = unpack_samples(&[0xFF, 0x00, 0xF0], 16, 4, 1, 1).unwrap();
        let mut height = 4;
        fix_image_length(&mut samples, 16, &mut height, 0, 1).unwrap();

        assert_eq!(height, 2);
        assert_eq!(samples.len(), 32);
        assert_eq!(
            &samples[16..],
            [1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }
}
