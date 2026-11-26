use super::mask::{DecodedMask, decode_mask};
use super::{DecodeContext, decode_context, decode_u8_samples, fix_image_length, unpack_samples};
use crate::color::{ColorComponents, ToLuma, ToRgb};
use crate::interpret::state::ActiveTransferFunction;
use crate::x_object::image::ImageXObject;
use crate::{ImageData, LumaData, RgbData};
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::{Dict, Stream};
use smallvec::SmallVec;

pub(crate) struct DecodedImage {
    pub(crate) image: ImageData,
    pub(crate) alpha: Option<DecodedMask>,
    pub(in crate::x_object) tail: super::Tail,
}

impl DecodedImage {
    pub(crate) fn bands(
        self,
        mut draw: impl FnMut(ImageData, kurbo::Affine, Option<(LumaData, kurbo::Affine)>),
    ) {
        use kurbo::Affine;
        let width = self.tail.width;
        let height = if self.tail.texel.is_some() {
            self.tail.declared
        } else {
            self.image.height()
        };
        let (sx, sy) = self.image.scale_factors();
        let scale = Affine::scale_non_uniform(f64::from(sx), f64::from(sy));
        let Some(mask) = self.alpha else {
            if self.tail.texel.is_none() {
                if height > 0 {
                    draw(self.image, scale, None);
                }
                return;
            }
            for (color, t) in self.tail.bands(self.image) {
                draw(color, scale * t, None);
            }
            return;
        };
        let mask_height = if mask.tail.texel.is_some() {
            mask.tail.declared
        } else {
            mask.luma.height
        };
        if mask_height == 0 {
            return;
        }
        let mask_scale = scale
            * Affine::scale_non_uniform(
                f64::from(width) / f64::from(mask.tail.width),
                f64::from(height) / f64::from(mask_height),
            );
        if self.tail.texel.is_none() && mask.tail.texel.is_none() {
            if height > 0 {
                draw(self.image, scale, Some((mask.luma, mask_scale)));
            }
            return;
        }
        let mut colors = self.tail.bands(self.image);
        let masks = mask.bands();
        if colors.len() == 1 && masks.len() == 1 {
            let (color, t) = colors.pop().unwrap();
            let (alpha, m) = masks.into_iter().next().unwrap();
            draw(color, scale * t, Some((alpha, mask_scale * m)));
        } else {
            for (color, t) in colors {
                for (alpha, m) in &masks {
                    draw(
                        color.clone(),
                        scale * t,
                        Some((alpha.clone(), mask_scale * *m)),
                    );
                }
            }
        }
    }
}
pub(crate) fn decode_image(
    obj: &ImageXObject<'_>,
    target_dimension: Option<(u32, u32)>,
) -> Option<DecodedImage> {
    ImageDecoder::new(obj, target_dimension)?.decode()
}

pub(crate) fn convert_band(
    obj: &ImageXObject<'_>,
    data: Vec<u8>,
    metadata: Option<hayro_syntax::object::stream::ImageData>,
    width: u32,
    rows: u32,
) -> Option<(Vec<u8>, usize)> {
    let mut ctx = super::sample_context(
        obj,
        hayro_syntax::object::stream::FilterResult {
            data: std::borrow::Cow::Owned(data),
            image_data: metadata,
        },
    )?;
    ctx.width = width;
    ctx.height = rows;
    if obj.kind != crate::x_object::image::ImageKind::Image {
        return super::mask::decode_mask_data(
            ctx,
            obj.kind == crate::x_object::image::ImageKind::StencilMask,
        )
        .map(|(v, _)| (v, 1));
    }
    let mut decoder = ImageDecoder {
        obj,
        ctx,
        target_dimension: None,
    };
    let alpha = decoder.decode_color_key_mask(obj.stream.dict().get::<Stream<'_>>(SMASK).is_some());
    let (data, c) = match decoder.decode_image()? {
        ImageData::Luma(d) => (d.data, 1),
        ImageData::Rgb(d) => (d.data, 3),
    };
    if let Some(alpha) = alpha {
        let mut out = Vec::with_capacity(data.len() + alpha.data.len());
        premultiply(&mut out, &data, c, &alpha.data);
        Some((out, c + 1))
    } else {
        Some((data, c))
    }
}

struct SoftMask<'a> {
    repaired: bool,
    stream: Option<Stream<'a>>,
}

impl<'a> SoftMask<'a> {
    fn read(dict: &Dict<'a>) -> Self {
        let was_repaired = dict.xref().is_repaired();
        let mut mask = Self {
            repaired: was_repaired,
            stream: dict.get(SMASK),
        };
        mask.current(dict);
        mask
    }

    fn current(&mut self, dict: &Dict<'a>) -> &mut Option<Stream<'a>> {
        if !self.repaired && dict.xref().is_repaired() {
            self.stream = None;
            self.repaired = true;
            self.stream = dict.get(SMASK);
        }
        &mut self.stream
    }
}

struct ImageDecoder<'a, 'b> {
    obj: &'a ImageXObject<'b>,
    ctx: DecodeContext<'b>,
    target_dimension: Option<(u32, u32)>,
}

impl<'a, 'b> ImageDecoder<'a, 'b> {
    fn new(obj: &'a ImageXObject<'b>, target_dimension: Option<(u32, u32)>) -> Option<Self> {
        let ctx = decode_context(obj, target_dimension)?;

        Some(Self {
            obj,
            ctx,
            target_dimension,
        })
    }

    fn decode(mut self) -> Option<DecodedImage> {
        let tail = super::Tail::of(&self.ctx, self.obj)?;
        let mut soft_mask = SoftMask::read(self.obj.stream.dict());
        let key = self.decode_color_key_mask(soft_mask.stream.is_some());
        let mut image = self.decode_image()?;
        let alpha = self.decode_alpha(&mut image, key, &mut soft_mask);

        Some(DecodedImage { image, alpha, tail })
    }

    fn decode_image(&mut self) -> Option<ImageData> {
        let num_components = self.ctx.color_space.num_components() as usize;

        // To prevent a panic when calling the `chunks` method.
        if num_components == 0 {
            return None;
        }

        let component_ranges = self.ctx.color_space.component_ranges();
        let default_decode = self
            .ctx
            .color_space
            .default_decode_arr(self.ctx.bits_per_component as f32);
        let inverted_default_decode = self
            .ctx
            .color_space
            .inverted_default_decode_arr(self.ctx.bits_per_component as f32);
        let inverted_component_ranges = component_ranges
            .iter()
            .map(|(min, max)| (*max, *min))
            .collect::<SmallVec<[(f32, f32); 4]>>();
        let is_indexed = self.ctx.color_space.is_indexed();

        let direct_invert = if self.ctx.bits_per_component == 8
            && (self.ctx.decode_arr == component_ranges
                || is_indexed && self.ctx.decode_arr == default_decode)
        {
            Some(false)
        } else if self.ctx.bits_per_component == 8
            && (self.ctx.decode_arr == inverted_component_ranges
                || is_indexed && self.ctx.decode_arr == inverted_default_decode)
        {
            Some(true)
        } else {
            None
        };

        let mut components = if let Some(invert) = direct_invert {
            fix_image_length(
                self.ctx.decoded.data.to_mut(),
                self.ctx.width,
                &mut self.ctx.height,
                0,
                num_components,
            )?;

            if invert {
                for value in self.ctx.decoded.data.to_mut() {
                    *value = 255 - *value;
                }
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
            )?;

            fix_image_length(
                &mut components,
                self.ctx.width,
                &mut self.ctx.height,
                0,
                num_components,
            )?;

            components
        };

        if self
            .obj
            .transfer_function
            .as_ref()
            .is_none_or(|t| matches!(t, ActiveTransferFunction::Single(_)))
            && self.ctx.color_space.to_luma(&mut components).is_some()
        {
            if let Some(transfer_function) = &self.obj.transfer_function {
                transfer_function.apply_to(&mut components);
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

        self.apply_transfer_function(&mut rgb_data);

        Some(ImageData::Rgb(rgb_data))
    }

    fn convert_to_rgb(&self, mut decoded: Vec<u8>) -> Option<RgbData> {
        if self
            .ctx
            .color_space
            .convert_in_place(&mut decoded)
            .is_none()
        {
            let mut output = vec![0; self.ctx.width as usize * self.ctx.height as usize * 3];
            self.ctx.color_space.convert(&decoded, &mut output)?;
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

    fn apply_transfer_function(&self, rgb_data: &mut RgbData) {
        if let Some(transfer_function) = &self.obj.transfer_function {
            transfer_function.apply_to(&mut rgb_data.data);
        }
    }

    fn decode_alpha(
        &mut self,
        image: &mut ImageData,
        key: Option<LumaData>,
        soft_mask: &mut SoftMask<'b>,
    ) -> Option<DecodedMask> {
        let dict = self.obj.stream.dict();
        let (matte, embedded) = loop {
            let matte = soft_mask
                .current(dict)
                .as_ref()
                .and_then(|s| self.resolve_matte(s))
                .filter(|(alpha, _)| {
                    alpha.tail.width == self.ctx.width && alpha.tail.declared == self.ctx.declared
                });
            let embedded = matte.is_none() && dict.get::<u8>(SMASK_IN_DATA) == Some(1);
            if soft_mask.repaired == dict.xref().is_repaired() {
                break (matte, embedded);
            }
        };
        if let Some((mut alpha, matte_rgb)) = matte {
            alpha.tail.declared = self.ctx.height;
            alpha.luma.height = alpha.luma.height.min(self.ctx.height);
            alpha
                .luma
                .data
                .truncate(alpha.luma.width as usize * alpha.luma.height as usize);
            let alpha = alpha.into_whole()?;
            let (color, c) = match image {
                ImageData::Luma(d) => (&mut d.data, 1),
                ImageData::Rgb(d) => (&mut d.data, 3),
            };
            unblend_matte(color, c, &alpha.data, matte_rgb);

            return Some(DecodedMask::dense(alpha));
        }

        // Drop an invalid alpha channel so the image itself still renders (PDFJS-19611).
        if embedded {
            let mut data = self
                .ctx
                .decoded
                .image_data
                .as_mut()
                .and_then(|image| image.alpha.take())?;
            fix_image_length(&mut data, self.ctx.width, &mut self.ctx.height, 0, 1)?;

            Some(DecodedMask::dense(LumaData {
                data,
                width: self.ctx.width,
                height: self.ctx.height,
                interpolate: self.obj.interpolate,
                scale_factors: self.ctx.scale_factors,
            }))
            // `SMask` takes precedence over `Mask`.
        } else if let Some(s_mask) = soft_mask
            .current(dict)
            .take()
            .or_else(|| dict.get::<Stream<'_>>(MASK))
        {
            let obj = ImageXObject::new_mask(&s_mask, &self.obj.warning_sink, &self.obj.cache)?;

            decode_mask(&obj, self.target_dimension)
        } else {
            key.map(DecodedMask::dense)
        }
    }

    fn resolve_matte(&self, s_mask: &Stream<'_>) -> Option<(DecodedMask, [u8; 3])> {
        let matte = s_mask.dict().get::<ColorComponents>(MATTE)?;

        if matte.len() != self.ctx.color_space.num_components() as usize {
            return None;
        }

        // Matte is applied in RGB rather than the image's color space.
        let mut matte_rgb = [0_u8; 3];
        self.ctx.color_space.convert_values(&matte, &mut matte_rgb);

        let mask_obj = ImageXObject::new_mask(s_mask, &self.obj.warning_sink, &self.obj.cache)?;
        let alpha = decode_mask(&mask_obj, self.target_dimension)?;

        Some((alpha, matte_rgb))
    }

    fn decode_color_key_mask(&self, has_soft_mask: bool) -> Option<LumaData> {
        let color_key_mask = super::color_key(
            self.obj,
            self.ctx.color_space.num_components(),
            has_soft_mask,
        )?;
        let num_components = self.ctx.color_space.num_components() as usize;
        let components = unpack_samples(
            &self.ctx.decoded.data,
            self.ctx.width,
            self.ctx.height,
            num_components,
            self.ctx.bits_per_component,
        )?;
        let mut data = Vec::with_capacity(self.ctx.width as usize * self.ctx.height as usize);

        for pixel in components.chunks_exact(num_components) {
            let mut value = 0;

            for (component, min_max) in pixel.iter().zip(color_key_mask.as_chunks::<2>().0) {
                if *component > min_max[1] || *component < min_max[0] {
                    value = 255;
                }
            }

            data.push(value);
        }

        let mut height = self.ctx.height;
        fix_image_length(&mut data, self.ctx.width, &mut height, 0, 1)?;

        Some(LumaData {
            data,
            width: self.ctx.width,
            height,
            interpolate: self.obj.interpolate,
            scale_factors: self.ctx.scale_factors,
        })
    }
}

pub(in crate::x_object) fn unblend_matte(
    color: &mut [u8],
    channels: usize,
    alpha: &[u8],
    matte: [u8; 3],
) {
    for (pixel, &a) in color.chunks_exact_mut(channels).zip(alpha) {
        if a != 0 {
            let inv = 255.0 / f32::from(a);
            for (c, m) in pixel.iter_mut().zip(matte) {
                let m = f32::from(m);
                *c = (m + (f32::from(*c) - m) * inv) as u8;
            }
        }
    }
}

pub(in crate::x_object) fn premultiply(
    out: &mut Vec<u8>,
    color: &[u8],
    channels: usize,
    alpha: &[u8],
) {
    for (pixel, &a) in color.chunks_exact(channels).zip(alpha) {
        for &v in pixel {
            out.push((u16::from(v) * u16::from(a) / 255) as u8);
        }
        out.push(a);
    }
}
