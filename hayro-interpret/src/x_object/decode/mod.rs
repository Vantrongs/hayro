mod image;
mod mask;

pub(crate) use image::{DecodedImage, decode_image, direct_invert, unpremultiply_samples};
pub(crate) use mask::{BILEVEL_MASK_LUT, DecodedMask, MaskPath, decode_mask};

use crate::InterpreterWarning;
use crate::color::ColorSpace;
use crate::function::interpolate;
use crate::x_object::image::{ImageKind, ImageXObject};
use hayro_syntax::bit_reader::BitReader;
use hayro_syntax::object::Array;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::stream::{FilterResult, ImageColorSpace, ImageData, ImageDecodeParams};
use smallvec::SmallVec;

struct DecodeContext<'a> {
    decoded: FilterResult<'a>,
    width: u32,
    height: u32,
    scale_factors: (f32, f32),
    color_space: ColorSpace,
    bits_per_component: u8,
    decode_arr: SmallVec<[(f32, f32); 4]>,
}

/// How an image's samples are laid out and what they mean: from its dictionary, and
/// the image data a filter reported (`decode_context`).
pub(crate) struct SampleFormat {
    pub(crate) color_space: ColorSpace,
    pub(crate) bits_per_component: u8,
    pub(crate) decode_arr: SmallVec<[(f32, f32); 4]>,
}

pub(crate) fn sample_format(
    obj: &ImageXObject<'_>,
    image_data: Option<&ImageData>,
) -> SampleFormat {
    let dict = obj.stream.dict();
    let dict_bpc = dict
        .get::<u8>(BPC)
        .or_else(|| dict.get::<u8>(BITS_PER_COMPONENT));

    let color_space = obj
        .color_space
        .clone()
        .or_else(|| {
            image_data.map(|i| i.color_space).and_then(|c| {
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

    let bits_per_component = image_data
        .map(|i| i.bits_per_component)
        .or(dict_bpc)
        .unwrap_or(fallback_bpc);

    let decode_arr = dict
        .get::<Array<'_>>(D)
        .or_else(|| dict.get::<Array<'_>>(DECODE))
        .map(|a| a.iter::<(f32, f32)>().collect::<SmallVec<_>>())
        .unwrap_or(color_space.default_decode_arr(bits_per_component as f32));

    SampleFormat {
        color_space,
        bits_per_component,
        decode_arr,
    }
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
        width: obj.width,
        height: obj.height,
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
            scale_x = obj.width as f32 / d.width as f32;
            scale_y = obj.height as f32 / d.height as f32;

            (d.width, d.height)
        })
        .unwrap_or((obj.width, obj.height));

    let SampleFormat {
        color_space,
        bits_per_component,
        decode_arr,
    } = sample_format(obj, decoded.image_data.as_ref());

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
    let row_len = (width as usize).saturating_mul(num_components);
    // Too much data (or just the right amount) is truncated; too little adapts the
    // height and pads the last row.
    *height = rows_present(image.len(), row_len, *height);
    image.resize(row_len * *height as usize, filler);

    if width == 0 || *height == 0 {
        None
    } else {
        Some(())
    }
}

/// The rows of `row_len` samples, at most `height`, that `samples` decoded samples
/// fill. A last row they fill in part counts, to be padded, unless padding it would
/// take more than the samples present (and more than `MIN_ROW_PAD`): data cut short
/// costs memory in proportion to what it holds, never to what the image declares.
pub(crate) fn rows_present(samples: usize, row_len: usize, height: u32) -> u32 {
    if row_len == 0 {
        return 0;
    }
    let (full, part) = (samples / row_len, samples % row_len);
    let pad = row_len - part;
    let rows = if part > 0 && (pad <= samples || pad <= MIN_ROW_PAD) {
        full + 1
    } else {
        full
    };
    rows.min(height as usize) as u32
}

/// A short last row is padded up to this many samples even when the data before it
/// holds fewer.
const MIN_ROW_PAD: usize = 1 << 16;

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
            data.chunks_exact(2)
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
            data.chunks_exact(2)
                .map(|value| u16::from_be_bytes([value[0], value[1]]))
                .collect(),
        ),
        _ => {
            warn!("unsupported bits per component: {bits_per_component}");
            None
        }
    }
}

/// The rows of packed samples, at most `height`, that `data` holds (see
/// `rows_present`); each row starts on a byte.
fn sample_rows(
    data: &[u8],
    width: u32,
    height: u32,
    num_components: usize,
    bits_per_component: u8,
) -> u32 {
    let row_len = (width as usize).saturating_mul(num_components);
    let row_bytes = row_len
        .saturating_mul(bits_per_component as usize)
        .div_ceil(8)
        .max(1);
    let samples = (data.len() / row_bytes).saturating_mul(row_len)
        + (data.len() % row_bytes * 8 / bits_per_component.max(1) as usize).min(row_len);
    rows_present(samples, row_len, height)
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
