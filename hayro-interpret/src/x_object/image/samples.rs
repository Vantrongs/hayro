use super::{ImageKind, ImageXObject};
use crate::WarningSinkFn;
use crate::color::{ColorComponents, ColorSpace};
use crate::interpret::state::ActiveTransferFunction;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::stream::OwnedStream;
use hayro_syntax::object::{Array, Object, Stream};
use smallvec::SmallVec;

pub(crate) trait Source {
    fn open_stream(&self) -> Option<Stream<'_>>;
}

impl Source for Stream<'_> {
    fn open_stream(&self) -> Option<Stream<'_>> {
        Some(self.clone())
    }
}

impl Source for OwnedStream {
    fn open_stream(&self) -> Option<Stream<'_>> {
        self.get()
    }
}

/// The operands of image decoding. A recorded image owns its streams and
/// resolves exactly the same operands as an image drawn immediately.
pub(crate) struct Samples<S> {
    pub(crate) stream: S,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) interpolate: bool,
    pub(crate) kind: ImageKind,
    pub(crate) color_space: Option<ColorSpace>,
    pub(crate) transfer_function: Option<ActiveTransferFunction>,
    pub(crate) warning_sink: WarningSinkFn,
    pub(crate) bpc: Option<u8>,
    pub(crate) decode: Option<SmallVec<[(f32, f32); 4]>>,
    pub(crate) in_data: u8,
    pub(crate) mask: Option<Box<Self>>,
    pub(crate) has_mask_stream: bool,
    pub(crate) has_soft_mask: bool,
    pub(crate) matte: Option<ColorComponents>,
    pub(crate) key: Option<SmallVec<[u32; 8]>>,
}

impl<S> Samples<S> {
    pub(crate) fn read<'a>(obj: &ImageXObject<'a>, capture: &impl Fn(&Stream<'a>) -> S) -> Self {
        let dict = obj.stream.dict();
        // With no declared colour space, the codec supplies the component count.
        let components = usize::from(
            obj.color_space
                .as_ref()
                .map_or(u8::MAX, ColorSpace::num_components),
        );
        let mut result = Self {
            stream: capture(&obj.stream),
            width: obj.width,
            height: obj.height,
            interpolate: obj.interpolate,
            kind: obj.kind,
            color_space: obj.color_space.clone(),
            transfer_function: obj.transfer_function.clone(),
            warning_sink: obj.warning_sink.clone(),
            bpc: dict
                .get::<u8>(BPC)
                .or_else(|| dict.get::<u8>(BITS_PER_COMPONENT)),
            // One extra pair distinguishes the exact default array from an overlong one.
            decode: dict
                .get::<Array<'_>>(D)
                .or_else(|| dict.get::<Array<'_>>(DECODE))
                .map(|a| a.iter::<(f32, f32)>().take(components + 1).collect()),
            in_data: 0,
            mask: None,
            has_mask_stream: false,
            has_soft_mask: false,
            matte: None,
            key: None,
        };
        if obj.kind == ImageKind::Image {
            result.in_data = dict.get::<u8>(SMASK_IN_DATA).unwrap_or(0);
            let mask = if let Some(soft) = dict.get::<Stream<'_>>(SMASK) {
                result.has_soft_mask = true;
                Some(soft)
            } else {
                match dict.get::<Object<'_>>(MASK) {
                    Some(Object::Stream(mask)) => Some(mask),
                    Some(Object::Array(bounds)) => {
                        result.key = Some(bounds.iter::<u32>().take(2 * components).collect());
                        None
                    }
                    _ => None,
                }
            };
            if let Some(mask) = mask {
                result.has_mask_stream = true;
                result.matte = mask
                    .dict()
                    .get::<Array<'_>>(MATTE)
                    .map(|a| a.iter::<f32>().take(components + 1).collect());
                result.mask = ImageXObject::new_mask(&mask, &obj.warning_sink, &obj.cache)
                    .map(|m| Box::new(Self::read(&m, capture)));
            }
        }
        result
    }

    pub(crate) fn color_key(&self, components: u8) -> Option<&[u32]> {
        let n = 2 * usize::from(components);
        (n != 0).then_some(())?;
        self.key.as_deref()?.get(..n)
    }

    pub(crate) fn transparent_tail(&self, components: u8) -> bool {
        if self.has_mask_stream {
            self.matte.is_some()
        } else {
            self.color_key(components).is_some()
        }
    }
}
