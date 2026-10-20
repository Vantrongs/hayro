use super::mask::decode_mask;
use super::{
    DecodeContext, decode_context, decode_u8_samples, fix_image_length, in_pieces, into_owned,
    pixel_piece, unpack_samples,
};
use crate::color::{ColorComponents, ColorSpace, ToLuma, ToRgb};
use crate::interpret::state::ActiveTransferFunction;
use crate::x_object::image::ImageXObject;
use crate::{ImageData, LumaData, RgbData};
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::work::{Meter, POLL_BYTES};
use smallvec::SmallVec;

/// Whether 8-bit samples are used as they are (`Some(false)`), inverted
/// (`Some(true)`), or go through the decode array (`None`): the decode array maps them
/// onto the colour space's ranges or the reverse.
pub(crate) fn direct_invert(
    color_space: &ColorSpace,
    bits_per_component: u8,
    decode_arr: &[(f32, f32)],
) -> Option<bool> {
    let component_ranges = color_space.component_ranges();
    let default_decode = color_space.default_decode_arr(bits_per_component as f32);
    let inverted_default_decode =
        color_space.inverted_default_decode_arr(bits_per_component as f32);
    let inverted_component_ranges = component_ranges
        .iter()
        .map(|(min, max)| (*max, *min))
        .collect::<SmallVec<[(f32, f32); 4]>>();
    let is_indexed = color_space.is_indexed();

    if bits_per_component == 8
        && (decode_arr == component_ranges.as_slice()
            || is_indexed && decode_arr == default_decode.as_slice())
    {
        Some(false)
    } else if bits_per_component == 8
        && (decode_arr == inverted_component_ranges.as_slice()
            || is_indexed && decode_arr == inverted_default_decode.as_slice())
    {
        Some(true)
    } else {
        None
    }
}

pub(crate) struct DecodedImage {
    pub(crate) image: ImageData,
    pub(crate) alpha: Option<LumaData>,
}

/// The image decoded whole, its work counted on `meter`; `None` also once the
/// meter's hook asks to stop.
pub(crate) fn decode_image(
    obj: &ImageXObject<'_>,
    target_dimension: Option<(u32, u32)>,
    meter: &Meter<'_>,
) -> Option<DecodedImage> {
    let decoded = ImageDecoder::new(obj, target_dimension, meter)?.decode();
    // A step may end early on a stop as on data cut short.
    decoded.filter(|_| !meter.cancelled())
}

struct ImageDecoder<'a, 'b, 'm> {
    obj: &'a ImageXObject<'b>,
    ctx: DecodeContext<'b>,
    target_dimension: Option<(u32, u32)>,
    color_key_mask: Option<SmallVec<[u16; 4]>>,
    decoded_color_key_mask: Option<LumaData>,
    meter: &'m Meter<'m>,
}

impl<'a, 'b, 'm> ImageDecoder<'a, 'b, 'm> {
    fn new(
        obj: &'a ImageXObject<'b>,
        target_dimension: Option<(u32, u32)>,
        meter: &'m Meter<'m>,
    ) -> Option<Self> {
        let ctx = decode_context(obj, target_dimension, meter)?;
        let color_key_mask = obj.stream.dict().get::<SmallVec<[u16; 4]>>(MASK);

        Some(Self {
            obj,
            ctx,
            target_dimension,
            color_key_mask,
            decoded_color_key_mask: None,
            meter,
        })
    }

    fn decode(mut self) -> Option<DecodedImage> {
        let mut image = self.decode_image()?;
        let alpha = self.decode_alpha(&mut image);

        Some(DecodedImage { image, alpha })
    }

    fn decode_image(&mut self) -> Option<ImageData> {
        let num_components = self.ctx.color_space.num_components() as usize;

        // To prevent a panic when calling the `chunks` method.
        if num_components == 0 {
            return None;
        }

        let direct_invert = direct_invert(
            &self.ctx.color_space,
            self.ctx.bits_per_component,
            &self.ctx.decode_arr,
        );

        let meter = self.meter;
        let mut components = if let Some(invert) = direct_invert {
            // This is actually the most common case, where the PDF is embedded
            // in such a way where we don't need to decode. In this case,
            // we can use the raw decoded component values directly.
            let mut data = into_owned(core::mem::take(&mut self.ctx.decoded.data), meter)?;
            fix_image_length(
                &mut data,
                self.ctx.width,
                &mut self.ctx.height,
                0,
                num_components,
                meter,
            )?;
            self.ctx.decoded.data = data.into();

            if self.color_key_mask.is_some() {
                self.decoded_color_key_mask = self.decode_color_key_mask();
            }

            if invert {
                super::invert(self.ctx.decoded.data.to_mut(), meter)?;
            }

            core::mem::take(&mut self.ctx.decoded.data).into_owned()
        } else {
            let mut components = decode_u8_samples(
                &self.ctx.decoded.data,
                self.ctx.width,
                self.ctx.height,
                &self.ctx.color_space,
                self.ctx.bits_per_component,
                &self.ctx.decode_arr,
                meter,
            )?;

            fix_image_length(
                &mut components,
                self.ctx.width,
                &mut self.ctx.height,
                0,
                num_components,
                meter,
            )?;

            components
        };

        if self
            .obj
            .transfer_function
            .as_ref()
            .is_none_or(|t| matches!(t, ActiveTransferFunction::Single(_)))
            && self.to_luma(&mut components)?
        {
            if let Some(transfer_function) = &self.obj.transfer_function {
                in_pieces(&mut components, 1, meter, |piece| {
                    transfer_function.apply_to(piece);
                    Some(())
                })?;
            }

            return Some(ImageData::Luma(LumaData {
                data: components,
                width: self.ctx.width,
                height: self.ctx.height,
                interpolate: self.obj.interpolate,
                scale_factors: self.ctx.scale_factors,
            }));
        }

        let mut rgb_data = self.convert_to_rgb(components)?;

        self.apply_transfer_function(&mut rgb_data)?;

        Some(ImageData::Rgb(rgb_data))
    }

    /// Whether the colour space converts `components` to luma, which it then did, a
    /// piece at a time; `None` once the meter's hook asks to stop.
    fn to_luma(&self, components: &mut [u8]) -> Option<bool> {
        let n = self.ctx.color_space.num_components() as usize;
        let first = components.len().min(pixel_piece(n) * n);
        let (first, rest) = components.split_at_mut(first);
        if self.ctx.color_space.to_luma(first).is_none() {
            return Some(false);
        }
        self.meter.work(first.len()).ok()?;
        in_pieces(rest, n, self.meter, |piece| {
            self.ctx.color_space.to_luma(piece)
        })?;
        Some(true)
    }

    fn convert_to_rgb(&self, mut decoded: Vec<u8>) -> Option<RgbData> {
        let color_space = &self.ctx.color_space;
        let n = color_space.num_components() as usize;
        let pixels = pixel_piece(n);
        let first = decoded.len().min(pixels * n);
        let (head, rest) = decoded.split_at_mut(first);
        if color_space.convert_in_place(head).is_some() {
            self.meter.work(first).ok()?;
            in_pieces(rest, n, self.meter, |piece| {
                color_space.convert_in_place(piece)
            })?;
        } else {
            let mut output = vec![0; self.ctx.width as usize * self.ctx.height as usize * 3];
            for (input, output) in decoded
                .chunks(pixels * n)
                .zip(output.chunks_mut(pixels * 3))
            {
                color_space.convert(input, output)?;
                self.meter.work(output.len()).ok()?;
            }
            decoded = output;
        }

        Some(RgbData {
            data: decoded,
            width: self.ctx.width,
            height: self.ctx.height,
            interpolate: self.obj.interpolate,
            scale_factors: self.ctx.scale_factors,
        })
    }

    fn apply_transfer_function(&self, rgb_data: &mut RgbData) -> Option<()> {
        if let Some(transfer_function) = &self.obj.transfer_function {
            in_pieces(&mut rgb_data.data, 3, self.meter, |piece| {
                transfer_function.apply_to(piece);
                Some(())
            })?;
        }
        Some(())
    }

    fn decode_alpha(&mut self, image: &mut ImageData) -> Option<LumaData> {
        if let Some((alpha, matte_rgb)) = self.resolve_matte()
            && alpha.width == self.ctx.width
            && alpha.height == self.ctx.height
        {
            unpremultiply(image, &alpha.data, &matte_rgb, self.meter)?;

            return Some(alpha);
        }

        // If the alpha channel is invalid, return no alpha so the main image can
        // still be returned (see PDFJS-19611).
        let dict = self.obj.stream.dict();

        if let Some(1) = dict.get::<u8>(SMASK_IN_DATA) {
            let mut data = self
                .ctx
                .decoded
                .image_data
                .as_mut()
                .and_then(|image| image.alpha.take())?;
            fix_image_length(
                &mut data,
                self.ctx.width,
                &mut self.ctx.height,
                0,
                1,
                self.meter,
            )?;

            Some(LumaData {
                data,
                width: self.ctx.width,
                height: self.ctx.height,
                interpolate: self.obj.interpolate,
                scale_factors: self.ctx.scale_factors,
            })
            // Note: `SMASK` field takes precedence over `MASK`, so order matters here.
        } else if let Some(s_mask) = dict
            .get::<Stream<'_>>(SMASK)
            .or_else(|| dict.get::<Stream<'_>>(MASK))
        {
            let obj = ImageXObject::new_mask(&s_mask, &self.obj.warning_sink, &self.obj.cache)?;

            decode_mask(&obj, self.target_dimension, self.meter).map(|decoded| decoded.luma)
        } else if self.color_key_mask.is_some() {
            self.decoded_color_key_mask
                .take()
                .or_else(|| self.decode_color_key_mask())
        } else {
            None
        }
    }

    fn resolve_matte(&self) -> Option<(LumaData, [u8; 3])> {
        let s_mask = self.obj.stream.dict().get::<Stream<'_>>(SMASK)?;
        let matte = s_mask.dict().get::<ColorComponents>(MATTE)?;

        if matte.len() != self.ctx.color_space.num_components() as usize {
            return None;
        }

        // In theory, matte needs to be applied in the image's original color space,
        // but we always do it in RGB for now.
        let mut matte_rgb = [0_u8; 3];
        self.ctx.color_space.convert_values(&matte, &mut matte_rgb);

        let mask_obj = ImageXObject::new_mask(&s_mask, &self.obj.warning_sink, &self.obj.cache)?;
        let alpha = decode_mask(&mask_obj, self.target_dimension, self.meter)?.luma;

        Some((alpha, matte_rgb))
    }

    fn decode_color_key_mask(&self) -> Option<LumaData> {
        let color_key_mask = self.color_key_mask.as_deref()?;
        let num_components = self.ctx.color_space.num_components() as usize;
        let components = unpack_samples(
            &self.ctx.decoded.data,
            self.ctx.width,
            self.ctx.height,
            num_components,
            self.ctx.bits_per_component,
            self.meter,
        )?;
        let mut data = Vec::with_capacity(self.ctx.width as usize * self.ctx.height as usize);

        for piece in components.chunks(pixel_piece(num_components) * num_components) {
            for pixel in piece.chunks_exact(num_components) {
                let mut value = 0;

                for (component, min_max) in pixel.iter().zip(color_key_mask.as_chunks::<2>().0) {
                    if *component > min_max[1] || *component < min_max[0] {
                        value = 255;
                    }
                }

                data.push(value);
            }
            self.meter.work(piece.len()).ok()?;
        }

        let mut height = self.ctx.height;
        fix_image_length(&mut data, self.ctx.width, &mut height, 0, 1, self.meter)?;

        Some(LumaData {
            data,
            width: self.ctx.width,
            height,
            interpolate: self.obj.interpolate,
            scale_factors: self.ctx.scale_factors,
        })
    }
}

/// Undoes `/Matte` premultiplication of `image`, a counted piece at a time.
fn unpremultiply(
    image: &mut ImageData,
    alpha: &[u8],
    matte_rgb: &[u8],
    meter: &Meter<'_>,
) -> Option<()> {
    let (data, channels) = match image {
        ImageData::Rgb(rgb) => (&mut rgb.data, 3),
        ImageData::Luma(luma) => (&mut luma.data, 1),
    };
    let pixels = POLL_BYTES / channels;
    for (data, alpha) in data.chunks_mut(pixels * channels).zip(alpha.chunks(pixels)) {
        unpremultiply_samples(data, channels, alpha, matte_rgb);
        meter.work(data.len()).ok()?;
    }
    Some(())
}

/// Undoes `/Matte` premultiplication of `channels`-channel colour samples (RGB, or
/// luma against the matte's first component) by their `alpha`.
pub(crate) fn unpremultiply_samples(
    data: &mut [u8],
    channels: usize,
    alpha: &[u8],
    matte_rgb: &[u8],
) {
    for (pixel, &a) in data.chunks_exact_mut(channels).zip(alpha.iter()) {
        if a == 0 {
            continue;
        }
        let inv_alpha = 255.0 / a as f32;
        for (c, &m) in pixel.iter_mut().zip(matte_rgb.iter()) {
            let m = m as f32;
            *c = (m + (*c as f32 - m) * inv_alpha) as u8;
        }
    }
}
