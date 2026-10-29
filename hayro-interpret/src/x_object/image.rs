use super::decode::{DecodedImage, DecodedMask, decode_image, decode_mask};
use crate::WarningSinkFn;
use crate::cache::Cache;
use crate::color::{ColorComponents, ColorSpace};
use crate::context::Context;
use crate::device::Device;
use crate::interpret::state::ActiveTransferFunction;
use crate::ocg::Kept;
use crate::{BlendMode, CacheKey, Image, ImageDrawProps, RasterImage, StencilImage};
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::stream::OwnedStream;
use hayro_syntax::object::{Array, Dict, Name, Object, Stream};
use hayro_syntax::work::Meter;
use kurbo::Affine;
use smallvec::SmallVec;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct ImageXObject<'a> {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) color_space: Option<ColorSpace>,
    pub(crate) cache: Cache,
    pub(crate) interpolate: bool,
    pub(crate) kind: ImageKind,
    pub(crate) stream: Stream<'a>,
    pub(crate) transfer_function: Option<ActiveTransferFunction>,
    pub(crate) warning_sink: WarningSinkFn,
    /// Whether the colour space is a name looked up in the resources (which the spec
    /// allows only for inline images), so it depends on where the image is drawn.
    pub(crate) cs_by_name: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageKind {
    Image,
    Mask,
    StencilMask,
}

impl ImageKind {
    fn is_mask(self) -> bool {
        self != Self::Image
    }
}

impl<'a> ImageXObject<'a> {
    pub(crate) fn new(
        stream: &Stream<'a>,
        resolve_cs: impl FnOnce(&Name<'_>) -> Option<ColorSpace>,
        warning_sink: &WarningSinkFn,
        cache: &Cache,
        transfer_function: Option<ActiveTransferFunction>,
    ) -> Option<Self> {
        Self::new_inner(
            stream,
            resolve_cs,
            warning_sink,
            cache,
            ImageKind::Image,
            transfer_function,
        )
    }

    pub(crate) fn new_mask(
        stream: &Stream<'a>,
        warning_sink: &WarningSinkFn,
        cache: &Cache,
    ) -> Option<Self> {
        Self::new_inner(stream, |_| None, warning_sink, cache, ImageKind::Mask, None)
    }

    fn new_inner(
        stream: &Stream<'a>,
        resolve_cs: impl FnOnce(&Name<'_>) -> Option<ColorSpace>,
        warning_sink: &WarningSinkFn,
        cache: &Cache,
        mut kind: ImageKind,
        transfer_function: Option<ActiveTransferFunction>,
    ) -> Option<Self> {
        let dict = stream.dict();

        let is_stencil_mask = dict
            .get::<bool>(IM)
            .or_else(|| dict.get::<bool>(IMAGE_MASK))
            .unwrap_or(false);

        if is_stencil_mask {
            kind = ImageKind::StencilMask;
        }

        let mut cs_by_name = false;
        let image_cs = if kind.is_mask() {
            // Masks are always single-channel.
            Some(ColorSpace::device_gray())
        } else {
            let cs_obj = dict
                .get::<Object<'_>>(CS)
                .or_else(|| dict.get::<Object<'_>>(COLORSPACE));

            cs_obj
                .clone()
                .and_then(|c| ColorSpace::new(c, cache))
                // Inline images can also refer to color spaces by name.
                // Apparently, some PDF producers also do this for normal images,
                // though the PDF spec forbids it. See https://github.com/LaurenzV/hayro/pull/1311.
                .or_else(|| {
                    let name = cs_obj?.into_name()?;
                    cs_by_name = true;
                    resolve_cs(&name)
                })
        };

        let interpolate = dict
            .get::<bool>(I)
            .or_else(|| dict.get::<bool>(INTERPOLATE))
            .unwrap_or(false);

        let width = dict.get::<u32>(W).or_else(|| dict.get::<u32>(WIDTH))?;
        let height = dict.get::<u32>(H).or_else(|| dict.get::<u32>(HEIGHT))?;

        if width == 0 || height == 0 {
            return None;
        }

        Some(Self {
            width,
            cache: cache.clone(),
            height,
            color_space: image_cs,
            warning_sink: warning_sink.clone(),
            transfer_function,
            interpolate,
            stream: stream.clone(),
            kind,
            cs_by_name,
        })
    }

    /// Draws the image, its dictionary `own` held where it was read, whose `/OC`
    /// decides whether it shows.
    pub(crate) fn draw<'b>(
        &self,
        context: &mut Context<'b>,
        device: &mut impl Device<'b>,
        own: &Kept<Dict<'b>, Dict<'a>>,
    ) {
        if !context.ocg_state.is_visible() {
            return;
        }

        let has_oc = context.ocg_state.begin_named(own, OC);
        if !context.ocg_state.is_visible() {
            if has_oc {
                context.ocg_state.end_marked_content();
            }
            return;
        }

        let width = self.width as f64;
        let height = self.height as f64;

        context.save_state();
        context.pre_concat_affine(Affine::new([
            1.0 / width,
            0.0,
            0.0,
            -1.0 / height,
            0.0,
            1.0,
        ]));
        let transform = context.ctm();

        let has_alpha = self.has_mask();

        let mut soft_mask = std::mem::take(&mut context.compositing_mut().soft_mask);
        let blend_mode = std::mem::take(&mut context.compositing_mut().blend_mode);

        // If image has a soft mask, the soft mask from the graphics state
        // should be discarded.
        if has_alpha {
            soft_mask = None;
        }

        device.push_transparency_group(
            context.fill_paint().alpha,
            std::mem::take(&mut soft_mask),
            blend_mode,
        );

        // Read here, through the document, which counts it, and not again.
        let sampled = Arc::new(SampledImage::read(self, true));
        let image = if self.kind.is_mask() {
            Image::Stencil(StencilImage {
                paint: context.get_paint(false),
                stream: self.stream.clone(),
                image: sampled,
            })
        } else {
            Image::Raster(RasterImage {
                stream: self.stream.clone(),
                image: sampled,
                cs_by_name: self.cs_by_name,
            })
        };

        device.draw_image(
            image,
            ImageDrawProps {
                transform,
                soft_mask: None,
                blend_mode: BlendMode::default(),
            },
        );
        device.pop_transparency_group();

        context.restore_state(device);

        if has_oc {
            context.ocg_state.end_marked_content();
        }
    }

    fn has_mask(&self) -> bool {
        let dict = self.stream.dict();

        dict.contains_key(SMASK_IN_DATA) || dict.contains_key(SMASK) || dict.contains_key(MASK)
    }
}

impl CacheKey for ImageXObject<'_> {
    fn cache_key(&self) -> u128 {
        self.stream.cache_key()
    }
}

/// What decoding an image reads, read from its dictionary once, as it is drawn
/// ([`Self::read`]), through the document, which counts what that keeps: its stream
/// owned ([`OwnedStream`]), with how its data decode, the entries of its dictionary
/// decoding reads, and its masks, read alike. Decoding reads no object of the
/// document, so it keeps nothing in the document, and what holds the image holds
/// nothing decoding read, however often it decodes it.
pub(crate) struct SampledImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) kind: ImageKind,
    pub(crate) interpolate: bool,
    pub(crate) color_space: Option<ColorSpace>,
    pub(crate) transfer_function: Option<ActiveTransferFunction>,
    pub(crate) warning_sink: WarningSinkFn,
    pub(crate) stream: OwnedStream,
    /// `/BitsPerComponent`.
    pub(crate) bpc: Option<u8>,
    /// The pairs of `/Decode`, where it is an array.
    pub(crate) decode: Option<SmallVec<[(f32, f32); 4]>>,
    pub(crate) masks: Masks,
}

/// A mask stream as decoding reads it: `None` where it is no mask
/// ([`ImageXObject::new_mask`] refuses it).
pub(crate) type MaskImage = Option<Box<SampledImage>>;

/// What an image's dictionary says of its masks; nothing for a mask's own.
#[derive(Default)]
pub(crate) struct Masks {
    /// `/SMask`, where it is a stream.
    pub(crate) smask: Option<MaskImage>,
    /// The soft mask's `/Matte`.
    pub(crate) matte: Option<ColorComponents>,
    /// `/Mask`, where it is a stream.
    pub(crate) mask: Option<MaskImage>,
    /// `/Mask`, where it is an array: a colour key.
    pub(crate) color_key: Option<SmallVec<[u16; 4]>>,
    /// Whether `/SMaskInData` is 1.
    pub(crate) in_data: bool,
    /// Whether the dictionary has `/SMask`, `/Mask` or `/SMaskInData`, of any value.
    pub(crate) any: bool,
}

impl SampledImage {
    /// What decoding `image` reads, with its masks if `masks`.
    pub(crate) fn read(image: &ImageXObject<'_>, masks: bool) -> Self {
        let dict = image.stream.dict();
        let mask = |stream: Stream<'_>| -> MaskImage {
            let mask = ImageXObject::new_mask(&stream, &image.warning_sink, &image.cache)?;
            Some(Box::new(Self::read(&mask, false)))
        };
        let masks = if masks {
            let smask = dict.get::<Stream<'_>>(SMASK);
            Masks {
                matte: smask
                    .as_ref()
                    .and_then(|s| s.dict().get::<ColorComponents>(MATTE)),
                smask: smask.map(mask),
                mask: dict.get::<Stream<'_>>(MASK).map(mask),
                color_key: dict.get::<SmallVec<[u16; 4]>>(MASK),
                in_data: dict.get::<u8>(SMASK_IN_DATA) == Some(1),
                any: image.has_mask(),
            }
        } else {
            Masks::default()
        };
        Self {
            width: image.width,
            height: image.height,
            kind: image.kind,
            interpolate: image.interpolate,
            color_space: image.color_space.clone(),
            transfer_function: image.transfer_function.clone(),
            warning_sink: image.warning_sink.clone(),
            stream: image.stream.to_owned_stream(image.height),
            bpc: dict
                .get::<u8>(BPC)
                .or_else(|| dict.get::<u8>(BITS_PER_COMPONENT)),
            decode: dict
                .get::<Array<'_>>(D)
                .or_else(|| dict.get::<Array<'_>>(DECODE))
                .map(|a| {
                    let mut pairs = a.flex_iter();
                    core::iter::from_fn(|| pairs.next::<(f32, f32)>()).collect()
                }),
            masks,
        }
    }

    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    pub(crate) fn decoded_mask(&self, target_dimension: Option<(u32, u32)>) -> Option<DecodedMask> {
        if !self.kind.is_mask() {
            return None;
        }
        decode_mask(self, target_dimension, &Meter::default())
    }

    pub(crate) fn decoded_image(
        &self,
        target_dimension: Option<(u32, u32)>,
    ) -> Option<DecodedImage> {
        if self.kind.is_mask() {
            return None;
        }
        decode_image(self, target_dimension, &Meter::default())
    }
}
