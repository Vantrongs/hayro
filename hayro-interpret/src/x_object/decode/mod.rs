mod image;
pub(super) use image::{convert_band, premultiply, unblend_matte};
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
    declared: u32,
    logical_width: u32,
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

    let mut ctx = sample_context(obj, decoded)?;
    let stride = (ctx.width as usize)
        .checked_mul(usize::from(ctx.color_space.num_components()))?
        .checked_mul(usize::from(ctx.bits_per_component))?
        .div_ceil(8);
    if stride == 0 {
        return None;
    }
    let transparent = short_rows_transparent(obj, ctx.color_space.num_components());
    let available = fit_rows(
        ctx.decoded.data.len(),
        stride,
        ctx.height,
        ctx.bits_per_component,
        transparent,
    );
    ctx.height = u32::try_from(available.div_ceil(stride)).ok()?;
    match &mut ctx.decoded.data {
        std::borrow::Cow::Owned(v) => v.truncate(available),
        std::borrow::Cow::Borrowed(v) => *v = &v[..available],
    }
    let stride = if available > 0 && available < stride {
        let channels = usize::from(ctx.color_space.num_components());
        ctx.width = u32::try_from(available.div_ceil(channels)).ok()?;
        ctx.width as usize * channels
    } else {
        stride
    };
    if !available.is_multiple_of(stride) {
        ctx.decoded
            .data
            .to_mut()
            .resize(available.next_multiple_of(stride), 0);
    }
    Some(ctx)
}

pub(crate) fn fit_rows(
    bytes: usize,
    stride: usize,
    rows: u32,
    bits: u8,
    transparent: bool,
) -> usize {
    let n = bytes.min(stride.saturating_mul(rows as usize));
    if transparent || bits != 8 {
        n / stride * stride
    } else {
        n
    }
}

pub(crate) fn short_rows_transparent(obj: &ImageXObject<'_>, components: u8) -> bool {
    if obj.kind != ImageKind::Image {
        return false;
    }
    let dict = obj.stream.dict();
    if let Some(mask) = dict
        .get::<hayro_syntax::object::Stream<'_>>(SMASK)
        .or_else(|| dict.get::<hayro_syntax::object::Stream<'_>>(MASK))
    {
        return mask.dict().get::<Array<'_>>(MATTE).is_some();
    }
    color_key(obj, components).is_some()
}

fn color_key(obj: &ImageXObject<'_>, components: u8) -> Option<SmallVec<[u16; 8]>> {
    let dict = obj.stream.dict();
    if dict
        .get::<hayro_syntax::object::Stream<'_>>(SMASK)
        .is_some()
    {
        return None;
    }
    let components = usize::from(components);
    let key: SmallVec<_> = dict
        .get::<Array<'_>>(MASK)?
        .iter::<u16>()
        .take(2 * components)
        .collect();
    (components > 0 && key.len() == 2 * components).then_some(key)
}

pub(super) struct Tail {
    pub(super) width: u32,
    pub(super) declared: u32,
    pub(super) texel: Option<Vec<u8>>,
}

impl Tail {
    fn of(ctx: &DecodeContext<'_>, obj: &ImageXObject<'_>) -> Option<Self> {
        let texel =
            if (ctx.height < ctx.declared || ctx.width < ctx.logical_width)
                && !short_rows_transparent(obj, ctx.color_space.num_components())
            {
                let info = ctx.decoded.image_data.as_ref().map(|d| {
                    hayro_syntax::object::stream::ImageData {
                        width: 1,
                        height: 1,
                        color_space: d.color_space,
                        bits_per_component: d.bits_per_component,
                        alpha: None,
                    }
                });
                let bytes = (usize::from(ctx.bits_per_component)
                    * usize::from(ctx.color_space.num_components()))
                .div_ceil(8);
                Some(convert_band(obj, vec![0; bytes], info, 1, 1)?.0)
            } else {
                None
            };
        Some(Self {
            declared: ctx.declared,
            width: ctx.logical_width,
            texel,
        })
    }

    fn bands(self, data: crate::ImageData) -> SmallVec<[(crate::ImageData, kurbo::Affine); 3]> {
        use crate::{ImageData, LumaData, RgbData};
        use kurbo::Affine;
        let (w, h) = (data.width(), data.height());
        let mut bands = SmallVec::new();
        let gray = matches!(data, ImageData::Luma(_));
        if h != 0 {
            bands.push((data, Affine::IDENTITY));
        }
        if let Some(texel) = self.texel {
            for (left, top, width, height) in [
                (w, 0, self.width - w, h),
                (0, h, self.width, self.declared.saturating_sub(h)),
            ] {
                if width == 0 || height == 0 {
                    continue;
                }
                let image = if gray {
                    ImageData::Luma(LumaData {
                        data: texel.clone(),
                        width: 1,
                        height: 1,
                        interpolate: false,
                        scale_factors: (1., 1.),
                    })
                } else {
                    ImageData::Rgb(RgbData {
                        data: texel.clone(),
                        width: 1,
                        height: 1,
                        interpolate: false,
                        scale_factors: (1., 1.),
                    })
                };
                bands.push((
                    image,
                    Affine::translate((f64::from(left), f64::from(top)))
                        * Affine::scale_non_uniform(f64::from(width), f64::from(height)),
                ));
            }
        }
        bands
    }

    fn complete(&self, data: &mut Vec<u8>, width: &mut u32, height: &mut u32) -> Option<()> {
        if let Some(texel) = &self.texel {
            let len = (self.width as usize)
                .checked_mul(self.declared as usize)?
                .checked_mul(texel.len())?;
            data.try_reserve_exact(len.checked_sub(data.len())?).ok()?;
            if texel.len() == 1 {
                data.resize(len, texel[0]);
            } else {
                while data.len() < len {
                    data.extend_from_slice(texel);
                }
            }
            *height = self.declared;
            *width = self.width;
        }
        Some(())
    }
}

fn sample_context<'a>(
    obj: &ImageXObject<'a>,
    decoded: FilterResult<'a>,
) -> Option<DecodeContext<'a>> {
    let dict = obj.stream.dict();
    let dict_bpc = dict
        .get::<u8>(BPC)
        .or_else(|| dict.get::<u8>(BITS_PER_COMPONENT));
    let color_space = obj.color_space.clone();
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
        declared: height,
        logical_width: width,
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
        image.truncate(row_len * *height as usize);
    } else {
        *height = image.len().div_ceil(row_len) as u32;

        if !image.len().is_multiple_of(row_len) {
            image.extend(iter::repeat_n(filler, row_len - (image.len() % row_len)));
        }
    }

    if width == 0 { None } else { Some(()) }
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
    let capacity = width as usize * height as usize * num_components;
    let ranges = color_space.component_ranges();
    let indexed_hival = color_space.indexed_hival();

    if num_components == 0 {
        return Some(Vec::new());
    }

    let decode_component = |value: u32, component_index: usize| {
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
        1 | 2 | 4 | 8 => {
            let num_values = 1_usize << bits_per_component;
            let lut = (0..num_components)
                .flat_map(|component| {
                    (0..num_values).map(move |value| decode_component(value as u32, component))
                })
                .collect::<Option<Vec<_>>>()?;

            Some(if bits_per_component == 8 {
                decode_8bit_samples(data, num_components, &lut)
            } else {
                decode_packed_samples(
                    data,
                    width,
                    height,
                    num_components,
                    bits_per_component,
                    &lut,
                )
            })
        }
        1..16 => {
            let mut buf = Vec::with_capacity(capacity);
            for_each_sample(
                data,
                width,
                height,
                num_components,
                bits_per_component,
                |value, index| {
                    buf.push(decode_component(value, index % num_components)?);
                    Some(())
                },
            )?;

            Some(buf)
        }
        16 => Some(
            data.as_chunks::<2>()
                .0
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    decode_component(
                        u16::from_be_bytes([value[0], value[1]]) as u32,
                        index % num_components,
                    )
                })
                .collect::<Option<Vec<_>>>()?,
        ),
        _ => None,
    }
}

fn decode_8bit_samples(data: &[u8], num_components: usize, lut: &[u8]) -> Vec<u8> {
    if num_components == 1 {
        return data.iter().map(|value| lut[*value as usize]).collect();
    }

    let mut buf = Vec::with_capacity(data.len());
    for pixel in data.chunks(num_components) {
        for (component, value) in pixel.iter().enumerate() {
            buf.push(lut[component * 256 + *value as usize]);
        }
    }

    buf
}

fn decode_packed_samples(
    data: &[u8],
    width: u32,
    height: u32,
    num_components: usize,
    bits_per_component: u8,
    lut: &[u8],
) -> Vec<u8> {
    let bpc = bits_per_component as usize;
    let samples_per_byte = 8 / bpc;
    let num_values = 1 << bpc;
    let mask = num_values as u8 - 1;
    let row_samples = width as usize * num_components;
    let row_bytes = (row_samples * bpc).div_ceil(8);
    let row_data = |row: usize| data.get(row * row_bytes..).unwrap_or_default();

    if row_samples == 0 {
        return Vec::new();
    }

    if num_components == 1 {
        let mut expanded = [[0_u8; 8]; 256];
        for (byte, samples) in expanded.iter_mut().enumerate() {
            for (i, sample) in samples.iter_mut().take(samples_per_byte).enumerate() {
                let value = (byte >> (8 - bpc * (i + 1))) as u8 & mask;
                *sample = lut[value as usize];
            }
        }

        let mut buf = vec![0; row_samples * height as usize];
        for (index, out) in buf.chunks_exact_mut(row_samples).enumerate() {
            let row = row_data(index);
            match samples_per_byte {
                8 => expand_row::<8>(row, out, &expanded),
                4 => expand_row::<4>(row, out, &expanded),
                _ => expand_row::<2>(row, out, &expanded),
            }
        }

        return buf;
    }

    let mut buf = Vec::with_capacity(row_samples * height as usize);
    for row in 0..height as usize {
        let row = row_data(row);
        let mut component = 0;
        for sample in 0..row_samples {
            let bit = sample * bpc;
            let byte = row.get(bit / 8).copied().unwrap_or(0);
            let value = (byte >> (8 - bpc - bit % 8)) & mask;
            buf.push(lut[component * num_values + value as usize]);
            component += 1;
            if component == num_components {
                component = 0;
            }
        }
    }

    buf
}

fn expand_row<const N: usize>(row: &[u8], out: &mut [u8], expanded: &[[u8; 8]; 256]) {
    let (chunks, remainder) = out.as_chunks_mut::<N>();
    let available = chunks.len().min(row.len());

    for (chunk, byte) in chunks[..available].iter_mut().zip(row) {
        chunk.copy_from_slice(&expanded[*byte as usize][..N]);
    }
    for chunk in &mut chunks[available..] {
        chunk.copy_from_slice(&expanded[0][..N]);
    }

    if !remainder.is_empty() {
        let byte = row.get(chunks.len()).copied().unwrap_or(0);
        remainder.copy_from_slice(&expanded[byte as usize][..remainder.len()]);
    }
}

fn unpack_samples(
    data: &[u8],
    width: u32,
    height: u32,
    num_components: usize,
    bits_per_component: u8,
) -> Option<Vec<u16>> {
    let capacity = width as usize * height as usize * num_components;

    match bits_per_component {
        1..8 | 9..16 => {
            let mut buf = Vec::with_capacity(capacity);
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
        _ => None,
    }
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
                // Some images lack data, pad with zeros.
                visit(reader.read(bits_per_component).unwrap_or(0), index)?;
                index += 1;
            }
        }

        reader.align();
    }

    Some(())
}
