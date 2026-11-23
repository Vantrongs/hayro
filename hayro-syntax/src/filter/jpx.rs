use crate::bit_reader::BitWriter;
use crate::filter::FilterResult;
use crate::object::stream::{ImageColorSpace, ImageData, ImageDecodeParams};
use crate::work::{Meter, POLL_BYTES};
use alloc::borrow::Cow;
use alloc::vec;
use alloc::vec::Vec;
use hayro_jpeg2000::{ColorSpace, DecodeSettings};

impl ImageColorSpace {
    fn num_components(&self) -> u8 {
        match self {
            Self::Gray => 1,
            Self::Rgb => 3,
            Self::Cmyk => 4,
            Self::Unknown(num) => *num,
        }
    }
}

pub(crate) fn decode(
    data: &[u8],
    params: &ImageDecodeParams,
    meter: &Meter<'_>,
) -> Option<FilterResult<'static>> {
    use crate::object::stream::ImageColorSpace;

    let settings = DecodeSettings {
        resolve_palette_indices: false,
        strict: false,
        target_resolution: params.target_dimension,
        cancel: None,
    };

    let image = params
        .num_components
        .filter(|_| !params.is_indexed)
        .and_then(|channels| hayro_jpeg2000::Image::from_codestream(data, channels, &settings).ok())
        .or_else(|| hayro_jpeg2000::Image::new(data, &settings).ok())?;

    let width = image.width();
    let height = image.height();
    let cs = match image.color_space() {
        ColorSpace::Gray => ImageColorSpace::Gray,
        ColorSpace::RGB => ImageColorSpace::Rgb,
        ColorSpace::CMYK => ImageColorSpace::Cmyk,
        ColorSpace::Unknown { num_channels } => ImageColorSpace::Unknown(*num_channels),
        ColorSpace::Icc {
            num_channels: num_components,
            ..
        } => match num_components {
            1 => ImageColorSpace::Gray,
            3 => ImageColorSpace::Rgb,
            4 => ImageColorSpace::Cmyk,
            _ => return None,
        },
    };
    let has_alpha = image.has_alpha();
    let mut decoder_context = hayro_jpeg2000::DecoderContext::default();
    let channels = usize::from(cs.num_components()) + usize::from(has_alpha);
    let plane_bytes = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(channels)?
        .checked_mul(size_of::<f32>())?;
    meter.poll().ok()?;
    meter.work(plane_bytes).ok()?;
    let decoded = image.decode(&mut decoder_context).ok()?;
    let (color, alpha) = decoded
        .components()
        .split_at_checked(usize::from(cs.num_components()))?;
    if color.is_empty() || alpha.len() != usize::from(has_alpha) {
        return None;
    }
    let bpc = if params.is_indexed {
        color[0].bit_depth()
    } else if params.has_color_key {
        color.iter().map(|c| c.bit_depth()).max()?
    } else {
        8
    };
    let mixed =
        params.has_color_key && !params.is_indexed && color.iter().any(|c| c.bit_depth() != bpc);
    let component_depths = if mixed {
        color.iter().map(|c| c.bit_depth()).collect()
    } else {
        Vec::new()
    };
    let (data, alpha) = if bpc != 8 || mixed {
        let data = pack(color, bpc, mixed, width, height, meter)?;
        let alpha = if has_alpha {
            Some(pack(alpha, 8, false, width, height, meter)?)
        } else {
            None
        };
        (data, alpha)
    } else {
        let bitmap = decoded.data_u8().ok()?;
        meter.work(bitmap.len()).ok()?;
        if !has_alpha {
            (bitmap, None)
        } else {
            let total_channels = usize::from(cs.num_components()) + 1;
            let mut color_channels =
                Vec::with_capacity((bitmap.len() / total_channels) * cs.num_components() as usize);
            let mut alpha_channel = Vec::with_capacity(bitmap.len() / total_channels);

            let piece = POLL_BYTES / total_channels * total_channels;
            for piece in bitmap.chunks(piece) {
                for sample in piece.chunks_exact(total_channels) {
                    let (alpha, color) = sample.split_last()?;
                    alpha_channel.push(*alpha);
                    color_channels.extend_from_slice(color);
                }
                meter.work(piece.len()).ok()?;
            }

            (color_channels, Some(alpha_channel))
        }
    };

    Some(FilterResult {
        data: Cow::Owned(data),
        image_data: Some(ImageData {
            alpha,
            color_space: Some(cs),
            bits_per_component: bpc,
            component_depths,
            width,
            height,
        }),
    })
}

fn pack(
    components: &[hayro_jpeg2000::ComponentData],
    bpc: u8,
    native: bool,
    width: u32,
    height: u32,
    meter: &Meter<'_>,
) -> Option<Vec<u8>> {
    let max = |bits: u8| 1_u64.checked_shl(u32::from(bits))?.checked_sub(1);
    let out_max = max(bpc)?;
    let width = width as usize;
    let pixels = width.checked_mul(height as usize)?;
    if width == 0 || components.is_empty() || components.iter().any(|c| c.samples().len() != pixels)
    {
        return None;
    }
    let row_bits = width
        .checked_mul(components.len())?
        .checked_mul(usize::from(bpc))?;
    let mut data = vec![0; row_bits.div_ceil(8).checked_mul(height as usize)?];
    let mut writer = BitWriter::new(&mut data, bpc)?;
    let planes = components
        .iter()
        .map(|c| {
            let source_max = max(c.bit_depth())?;
            let limit = if native { source_max } else { out_max };
            Some((c.samples(), limit as f64 / source_max as f64, limit as u32))
        })
        .collect::<Option<Vec<_>>>()?;
    let chunk = (POLL_BYTES / planes.len()).max(1);
    for row in (0..pixels).step_by(width) {
        for start in (row..row + width).step_by(chunk) {
            let end = (start + chunk).min(row + width);
            meter.work((end - start) * planes.len()).ok()?;
            for i in start..end {
                for &(samples, scale, limit) in &planes {
                    let value = (f64::from(samples[i]) * scale + 0.5) as u32;
                    writer.write(value.min(limit))?;
                }
            }
        }
        writer.align();
    }
    Some(data)
}
