use super::decode::{DecodedImage, DecodedMask, decode_image, decode_mask};
use crate::WarningSinkFn;
use crate::cache::{Building, Cache, KeepsStreams};
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
    pub(crate) color_space: Option<ColorSpace>,
    pub(crate) cache: Cache,
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

    fn new_inner(
        stream: &Stream<'a>,
        resolve_cs: impl FnOnce(&Name<'_>) -> Option<ColorSpace>,
        warning_sink: &WarningSinkFn,
        cache: &Cache,
        mut kind: ImageKind,
        transfer_function: Option<ActiveTransferFunction>,
    ) -> Option<Self> {
        let dict = stream.dict();

        if image_mask(dict) {
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

        Some(Self {
            cache: cache.clone(),
            color_space: image_cs,
            warning_sink: warning_sink.clone(),
            transfer_function,
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

        // Read here, through the document, which counts it, and not again.
        let Some(sampled) = SampledImage::read(self) else {
            if has_oc {
                context.ocg_state.end_marked_content();
            }
            return;
        };
        let sampled = Arc::new(sampled);
        let width = sampled.width as f64;
        let height = sampled.height as f64;

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

        let has_alpha = sampled.masks.any;

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
}

impl CacheKey for ImageXObject<'_> {
    fn cache_key(&self) -> u128 {
        self.stream.cache_key()
    }
}

/// `/ImageMask` of an image's dictionary, which makes it a stencil mask.
fn image_mask(dict: &Dict<'_>) -> bool {
    dict.get::<bool>(IM)
        .or_else(|| dict.get::<bool>(IMAGE_MASK))
        .unwrap_or(false)
}

/// `/Interpolate` of an image's dictionary.
fn interpolate(dict: &Dict<'_>) -> bool {
    dict.get::<bool>(I)
        .or_else(|| dict.get::<bool>(INTERPOLATE))
        .unwrap_or(false)
}

/// The width and height an image's dictionary gives; `None` where either is missing
/// or 0.
fn size(dict: &Dict<'_>) -> Option<(u32, u32)> {
    let width = dict.get::<u32>(W).or_else(|| dict.get::<u32>(WIDTH))?;
    let height = dict.get::<u32>(H).or_else(|| dict.get::<u32>(HEIGHT))?;
    (width > 0 && height > 0).then_some((width, height))
}

/// What decoding an image reads of its stream alone, whatever it is drawn with
/// ([`Self::of`]): its stream owned ([`OwnedStream`]), with how its data decode, and
/// the entries of its dictionary decoding reads. Decoding reads no object of the
/// document, so it keeps nothing in the document, and what holds the image holds
/// nothing decoding read, however often it decodes it.
pub(crate) struct ImageRead {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) interpolate: bool,
    /// `/ImageMask`, which makes a mask stream a stencil mask.
    image_mask: bool,
    pub(crate) stream: OwnedStream,
    /// `/BitsPerComponent`.
    pub(crate) bpc: Option<u8>,
    /// The pairs of `/Decode`, where it is an array.
    pub(crate) decode: Option<SmallVec<[(f32, f32); 4]>>,
}

impl ImageRead {
    /// What decoding `stream` reads, read once for its object, through the document,
    /// which counts and keeps it: every placement of the image, on any page, shares
    /// it. An inline image, which is no object, is read for each placement: its
    /// bytes are in the content each time. `None` where the dictionary gives no size.
    fn of(stream: &Stream<'_>, cache: &impl KeepsStreams) -> Option<Arc<Self>> {
        cache.rooted_stream(stream, Self::build)
    }

    fn build(stream: Stream<'_>, _: &Building<'_>) -> Option<Arc<Self>> {
        let dict = stream.dict();
        let (width, height) = size(dict)?;
        Some(Arc::new(Self {
            width,
            height,
            interpolate: interpolate(dict),
            image_mask: image_mask(dict),
            stream: stream.to_owned_stream(height),
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
        }))
    }
}

/// What an image's dictionary says of its masks, read once for its object
/// ([`Self::of`]), the mask streams as [`ImageRead::of`] reads them. Only the masks
/// decoding can use are resolved, by its rules (`decode_alpha`, `Alpha::new`): none
/// of a stencil mask, which is decoded alone (`decode_mask`); `/Mask` neither where
/// `/SMask` is a stream, which takes precedence, nor where `/SMaskInData` is 1, which
/// takes the alpha from the data or, with `/Matte`, from `/SMask`.
#[derive(Default)]
pub(crate) struct MasksRead {
    /// `/SMask`, where it is a stream: `None` inside where it is no mask (it gives
    /// no size).
    soft: Option<Option<Arc<ImageRead>>>,
    /// The soft mask's `/Matte`.
    pub(crate) matte: Option<ColorComponents>,
    /// `/Mask`, where it is a stream, as `soft`.
    stencil: Option<Option<Arc<ImageRead>>>,
    /// `/Mask`, where it is an array: a colour key.
    pub(crate) color_key: Option<SmallVec<[u16; 4]>>,
    /// Whether `/SMaskInData` is 1.
    pub(crate) in_data: bool,
    /// Whether the dictionary has `/SMask`, `/Mask` or `/SMaskInData`, of any value,
    /// resolved or not: drawing drops the graphics state's soft mask then.
    pub(crate) any: bool,
}

impl MasksRead {
    /// What the dictionary of `stream`, an image's, says of its masks, read as
    /// [`ImageRead::of`] reads an image.
    fn of(stream: &Stream<'_>, cache: &Cache) -> Arc<Self> {
        cache.rooted_stream(stream, Self::build).unwrap_or_default()
    }

    fn build(stream: Stream<'_>, cache: &Building<'_>) -> Option<Arc<Self>> {
        let dict = stream.dict();
        let any =
            dict.contains_key(SMASK_IN_DATA) || dict.contains_key(SMASK) || dict.contains_key(MASK);
        if image_mask(dict) {
            return Some(Arc::new(Self {
                any,
                ..Self::default()
            }));
        }
        let mask = |stream: Stream<'_>| ImageRead::of(&stream, cache);
        let in_data = dict.get::<u8>(SMASK_IN_DATA) == Some(1);
        let smask = dict.get::<Stream<'_>>(SMASK);
        let stencil = match smask.is_none() && !in_data {
            true => dict.get::<Stream<'_>>(MASK),
            false => None,
        };
        let color_key = match smask.is_none() && !in_data && stencil.is_none() {
            true => dict.get::<SmallVec<[u16; 4]>>(MASK),
            false => None,
        };
        let matte = smask
            .as_ref()
            .and_then(|s| s.dict().get::<ColorComponents>(MATTE));
        // Under `/SMaskInData 1` the soft mask serves only to remove its `/Matte`.
        let soft = match matte.is_some() || !in_data {
            true => smask.map(mask),
            false => None,
        };
        Some(Arc::new(Self {
            matte,
            soft,
            stencil: stencil.map(mask),
            color_key,
            in_data,
            any,
        }))
    }
}

/// An image as it is drawn ([`Self::read`]): what decoding reads of its stream
/// alone ([`ImageRead`]), which every placement shares, with what differs between
/// placements: the colour space a name in the resources may give, the transfer
/// function, and where warnings go.
pub(crate) struct SampledImage {
    read: Arc<ImageRead>,
    pub(crate) kind: ImageKind,
    pub(crate) color_space: Option<ColorSpace>,
    pub(crate) transfer_function: Option<ActiveTransferFunction>,
    pub(crate) warning_sink: WarningSinkFn,
    pub(crate) masks: Masks,
}

impl core::ops::Deref for SampledImage {
    type Target = ImageRead;

    fn deref(&self) -> &ImageRead {
        &self.read
    }
}

/// A mask stream as decoding reads it: `None` where it is no mask (it gives no
/// size).
pub(crate) type MaskImage = Option<Box<SampledImage>>;

/// An image's masks as it is drawn: the mask streams, drawn as masks of it, and what
/// its dictionary says of them besides ([`MasksRead`]); nothing for a mask's own.
#[derive(Default)]
pub(crate) struct Masks {
    /// `/SMask`, where it is a stream.
    pub(crate) smask: Option<MaskImage>,
    /// `/Mask`, where it is a stream.
    pub(crate) mask: Option<MaskImage>,
    read: Arc<MasksRead>,
}

impl core::ops::Deref for Masks {
    type Target = MasksRead;

    fn deref(&self) -> &MasksRead {
        &self.read
    }
}

impl SampledImage {
    /// What decoding `image` reads, its masks included; `None` where its dictionary,
    /// read alone, gives no size.
    pub(crate) fn read(image: &ImageXObject<'_>) -> Option<Self> {
        let read = ImageRead::of(&image.stream, &image.cache)?;
        let masks = MasksRead::of(&image.stream, &image.cache);
        let mask = |mask: &Option<Arc<ImageRead>>| {
            let read = mask.clone()?;
            Some(Box::new(Self {
                kind: match read.image_mask {
                    true => ImageKind::StencilMask,
                    false => ImageKind::Mask,
                },
                read,
                // Masks are always single-channel.
                color_space: Some(ColorSpace::device_gray()),
                transfer_function: None,
                warning_sink: image.warning_sink.clone(),
                masks: Masks::default(),
            }))
        };
        Some(Self {
            read,
            kind: image.kind,
            color_space: image.color_space.clone(),
            transfer_function: image.transfer_function.clone(),
            warning_sink: image.warning_sink.clone(),
            masks: Masks {
                smask: masks.soft.as_ref().map(mask),
                mask: masks.stencil.as_ref().map(mask),
                read: masks,
            },
        })
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
