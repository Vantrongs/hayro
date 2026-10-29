use super::mask::{DecodedMask, decode_mask};
use super::{
    ColourKey, DecodeContext, Rows, Texel, complete, decode_context, decode_u8_samples, in_pieces,
    into_owned, matte, pixel_piece, room_for, unpack_samples,
};
use crate::color::{ColorSpace, ToLuma, ToRgb};
use crate::interpret::state::ActiveTransferFunction;
use crate::x_object::image::ImageXObject;
use crate::{ImageData, LumaData, RgbData};
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::stream::FilterResult;
use hayro_syntax::work::{Meter, POLL_BYTES};
use smallvec::SmallVec;
use std::borrow::Cow;

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
    /// The colour: the texels its data holds, and past them what `rows` says.
    image: ImageData,
    rows: Rows,
    /// Alpha on the grid the colour declares holds the colour's texels and covers
    /// its rows; a mask on another grid lies on the rows the colour declares.
    alpha: Option<(LumaData, Rows)>,
}

impl DecodedImage {
    /// `image` covering `rows`, with `alpha`; `None` unless each holds the texels of
    /// the rows it says ([`Rows::hold`]).
    fn new(image: ImageData, rows: Rows, alpha: Option<(LumaData, Rows)>) -> Option<Self> {
        let (len, channels) = match &image {
            ImageData::Rgb(d) => (d.data.len(), 3),
            ImageData::Luma(d) => (d.data.len(), 1),
        };
        let holds = rows.hold(len, image.width(), image.height(), channels)
            && alpha
                .as_ref()
                .is_none_or(|(a, r)| r.hold(a.data.len(), a.width, a.height, 1));
        holds.then_some(Self { image, rows, alpha })
    }

    /// The rows the colour's texels lie in.
    pub(crate) fn height(&self) -> u32 {
        self.image.height()
    }

    pub(crate) fn into_parts(self) -> (ImageData, Rows, Option<(LumaData, Rows)>) {
        (self.image, self.rows, self.alpha)
    }

    /// The colour and alpha with every texel of the rows they cover, those past the
    /// texels held each plane's fill: as large as the image intact. `None` where
    /// that does not fit in memory, before either is filled ([`room_for`]).
    pub(crate) fn into_whole(self) -> Option<(ImageData, Option<LumaData>)> {
        let meter = Meter::default();
        let mut image = self.image;
        let mut alpha = self.alpha;
        let texels = |width: u32, rows: Rows| (width as usize).checked_mul(rows.declared as usize);
        let width = image.width();
        let (data, height, channels) = image.rows_mut();
        let mut colour = None;
        if let Some(fill) = self.rows.fill {
            let texels = texels(width, self.rows)?;
            room_for(data, texels, channels, &meter)?;
            colour = Some((texels, fill));
        }
        let mut alpha_fill = None;
        if let Some((a, rows)) = &mut alpha
            && let Some(fill) = rows.fill
        {
            let texels = texels(a.width, *rows)?;
            room_for(&mut a.data, texels, 1, &meter)?;
            alpha_fill = Some((texels, fill));
        }
        if let Some((texels, fill)) = colour {
            complete(data, texels, &fill[..channels], &meter)?;
            *height = self.rows.declared;
        }
        let alpha = match alpha {
            Some((mut a, rows)) => {
                if let Some((texels, fill)) = alpha_fill {
                    complete(&mut a.data, texels, &fill[..1], &meter)?;
                    a.height = rows.declared;
                }
                Some(a)
            }
            None => None,
        };
        Some((image, alpha))
    }
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
    color_key_mask: Option<ColourKey>,
    decoded_color_key_mask: Option<LumaData>,
    meter: &'m Meter<'m>,
}

impl<'a, 'b, 'm> ImageDecoder<'a, 'b, 'm> {
    fn new(
        obj: &'a ImageXObject<'b>,
        target_dimension: Option<(u32, u32)>,
        meter: &'m Meter<'m>,
    ) -> Option<Self> {
        let (ctx, color_key_mask) = decode_context(obj, target_dimension, meter)?;

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
        let fill = self.fill()?;
        let mut image = self.decode_image()?;
        let mut rows = Rows {
            declared: self.ctx.declared,
            fill: fill.map(|f| f.colour),
        };
        let matte = self.resolve_matte();
        let on_grid = |(a, a_rows): &(LumaData, Rows)| {
            (a.width, a_rows.declared) == (self.ctx.width, rows.declared)
        };
        // A mask lies on the grid the colour declares.
        let (mut alpha, matte) = match matte {
            Some((alpha, matte_rgb)) if on_grid(&alpha) => (Some(alpha), Some(matte_rgb)),
            _ => {
                let texels = image.texels();
                (self.decode_alpha(texels, fill), None)
            }
        };
        if let Some((a, a_rows)) = &mut alpha
            && (a.width, a_rows.declared) == (self.ctx.width, rows.declared)
        {
            share_rows(&mut image, &rows, a, a_rows, self.meter)?;
            if let Some(matte_rgb) = matte {
                unpremultiply(&mut image, &a.data, &matte_rgb, self.meter)?;
                if let (Some(colour), Some(alpha)) = (&mut rows.fill, a_rows.fill) {
                    let channels = image.channels();
                    unpremultiply_samples(
                        &mut colour[..channels],
                        channels,
                        &alpha[..1],
                        &matte_rgb,
                    );
                }
            }
        }

        DecodedImage::new(image, rows, alpha)
    }

    /// What the samples the data does not hold become, decoded as a stored zero sample
    /// is: its colour, and its alpha under a colour key; `None` inside where they are
    /// transparent instead.
    fn fill(&self) -> Option<Option<Fill>> {
        if self.ctx.transparent {
            return Some(None);
        }
        let n = self.ctx.color_space.num_components() as usize;
        let bytes = (n * usize::from(self.ctx.bits_per_component)).div_ceil(8);
        let mut one = ImageDecoder {
            obj: self.obj,
            ctx: DecodeContext {
                decoded: FilterResult {
                    data: Cow::Owned(vec![0; bytes]),
                    image_data: None,
                },
                width: 1,
                height: 1,
                declared: 1,
                transparent: false,
                scale_factors: self.ctx.scale_factors,
                color_space: self.ctx.color_space.clone(),
                bits_per_component: self.ctx.bits_per_component,
                decode_arr: self.ctx.decode_arr.clone(),
            },
            target_dimension: None,
            color_key_mask: self.color_key_mask.clone(),
            decoded_color_key_mask: None,
            meter: self.meter,
        };
        let key = match one.color_key_mask {
            Some(_) => Some(one.decode_color_key_mask()?.data[0]),
            None => None,
        };
        let image = one.decode_image()?;
        let mut colour = [0; 3];
        let data = match &image {
            ImageData::Rgb(d) => &d.data,
            ImageData::Luma(d) => &d.data,
        };
        colour[..data.len()].copy_from_slice(data);
        Some(Some(Fill { colour, key }))
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
            let data = into_owned(core::mem::take(&mut self.ctx.decoded.data), 0, meter)?;
            self.ctx.decoded.data = data.into();

            if self.color_key_mask.is_some() {
                self.decoded_color_key_mask = self.decode_color_key_mask();
            }

            if invert {
                super::invert(self.ctx.decoded.data.to_mut(), meter)?;
            }

            core::mem::take(&mut self.ctx.decoded.data).into_owned()
        } else {
            decode_u8_samples(
                &self.ctx.decoded.data,
                self.ctx.width,
                self.ctx.height,
                &self.ctx.color_space,
                self.ctx.bits_per_component,
                &self.ctx.decode_arr,
                meter,
            )?
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
            let mut output = self.meter.room(decoded.len() / n * 3);
            for input in decoded.chunks(pixels * n) {
                let (at, len) = (output.len(), input.len() / n * 3);
                self.meter.work(len).ok()?;
                output.resize(at + len, 0);
                color_space.convert(input, &mut output[at..])?;
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

    /// The alpha of the colour, which holds `texels`, whose samples the data does not
    /// hold become `fill`.
    fn decode_alpha(&mut self, texels: usize, fill: Option<Fill>) -> Option<(LumaData, Rows)> {
        // If the alpha channel is invalid, return no alpha so the main image can
        // still be returned (see PDFJS-19611).
        let dict = self.obj.stream.dict();
        let rows = |alpha: Option<u8>| Rows {
            declared: self.ctx.declared,
            fill: fill.map(|_| [alpha.unwrap_or(0), 0, 0]),
        };

        if let Some(1) = dict.get::<u8>(SMASK_IN_DATA) {
            let mut data = self
                .ctx
                .decoded
                .image_data
                .as_mut()
                .and_then(|image| image.alpha.take())?;
            let more = texels.saturating_sub(data.len());
            self.meter.reserve_exact(&mut data, more);
            data.resize(texels, 0);

            Some((
                LumaData {
                    data,
                    width: self.ctx.width,
                    height: self.ctx.height,
                    interpolate: self.obj.interpolate,
                    scale_factors: self.ctx.scale_factors,
                },
                rows(None),
            ))
            // Note: `SMASK` field takes precedence over `MASK`, so order matters here.
        } else if let Some(s_mask) = dict
            .get::<Stream<'_>>(SMASK)
            .or_else(|| dict.get::<Stream<'_>>(MASK))
        {
            let obj = ImageXObject::new_mask(&s_mask, &self.obj.warning_sink, &self.obj.cache)?;

            decode_mask(&obj, self.target_dimension, self.meter).map(DecodedMask::into_parts)
        } else if self.color_key_mask.is_some() {
            let mask = self.decoded_color_key_mask.take();
            Some((
                mask.or_else(|| self.decode_color_key_mask())?,
                rows(fill.and_then(|f| f.key)),
            ))
        } else {
            None
        }
    }

    fn resolve_matte(&self) -> Option<((LumaData, Rows), [u8; 3])> {
        let components = self.ctx.color_space.num_components() as usize;
        let matte = matte(self.obj, components, self.meter).ok()??;
        let s_mask = self.obj.stream.dict().get::<Stream<'_>>(SMASK)?;

        // In theory, matte needs to be applied in the image's original color space,
        // but we always do it in RGB for now.
        let mut matte_rgb = [0_u8; 3];
        self.ctx.color_space.convert_values(&matte, &mut matte_rgb);

        let mask_obj = ImageXObject::new_mask(&s_mask, &self.obj.warning_sink, &self.obj.cache)?;
        let alpha = decode_mask(&mask_obj, self.target_dimension, self.meter)?;

        Some((alpha.into_parts(), matte_rgb))
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
        let mut data = self.meter.room(components.len() / num_components);

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

        Some(LumaData {
            data,
            width: self.ctx.width,
            height: self.ctx.height,
            interpolate: self.obj.interpolate,
            scale_factors: self.ctx.scale_factors,
        })
    }
}

/// What the samples the data does not hold become (`ImageDecoder::fill`).
#[derive(Clone, Copy)]
struct Fill {
    colour: Texel,
    /// The alpha a colour key gives them.
    key: Option<u8>,
}

/// The colour and alpha on its grid made to hold the same texels: the alpha's past
/// the colour's dropped where those are transparent, and otherwise the plane holding
/// fewer completed with its fill up to the other's. Past them, the alpha is
/// transparent where the colour is.
fn share_rows(
    image: &mut ImageData,
    rows: &Rows,
    alpha: &mut LumaData,
    alpha_rows: &mut Rows,
    meter: &Meter<'_>,
) -> Option<()> {
    let width = alpha.width as usize;
    let (data, height, channels) = image.rows_mut();
    let held = data.len() / channels;
    let texels = match rows.fill {
        Some(fill) => {
            let texels = held.max(alpha.data.len());
            complete(data, texels, &fill[..channels], meter)?;
            texels
        }
        None => held,
    };
    *height = texels.div_ceil(width) as u32;
    alpha.data.truncate(texels);
    let alpha_fill = alpha_rows.fill.unwrap_or_default();
    complete(&mut alpha.data, texels, &alpha_fill[..1], meter)?;
    alpha.height = *height;
    alpha_rows.fill = rows.fill.map(|_| alpha_fill);
    Some(())
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
