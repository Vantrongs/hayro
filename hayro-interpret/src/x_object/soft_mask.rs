use super::form::FormXObject;
use crate::color::{Color, ColorComponents, ColorSpace};
use crate::context::{Context, InterpreterCache};
use crate::device::Device;
use crate::function::{Function, TransferFunction};
use crate::interpret::state::State;
use crate::util::hash128;
use crate::{CacheKey, InterpreterSettings};
use hayro_syntax::object::Name;
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::{Dict, Object};
use hayro_syntax::page::Resources;
use hayro_syntax::xref::XRef;
use kurbo::{Affine, Rect, Shape};
use smallvec::smallvec;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::rc::Rc;

/// Type type of mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskType {
    /// A luminosity mask.
    Luminosity,
    /// An alpha mask.
    Alpha,
}

struct Repr<'a> {
    /// What the mask is drawn from, as a key ([`SoftMask::new`]).
    key: u128,
    group: FormXObject<'a>,
    mask_type: MaskType,
    parent_resources: Resources<'a>,
    root_transform: Affine,
    bbox: Rect,
    interpreter_cache: InterpreterCache<'a>,
    transfer_function: Option<TransferFunction>,
    settings: InterpreterSettings,
    background: Color,
    xref: &'a XRef,
    nesting_depth: u32,
}

/// A soft mask. Two are equal where they are drawn from the same: see
/// [`SoftMask::new`].
#[derive(Clone)]
pub struct SoftMask<'a>(Rc<Repr<'a>>);

impl Hash for SoftMask<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.key.hash(state);
    }
}

impl Debug for SoftMask<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SoftMask({:x})", self.0.key)
    }
}

impl PartialEq for SoftMask<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.0.key == other.0.key
    }
}

impl Eq for SoftMask<'_> {}

impl CacheKey for SoftMask<'_> {
    fn cache_key(&self) -> u128 {
        self.0.key
    }
}

impl<'a> SoftMask<'a> {
    pub(crate) fn new(
        dict: &Dict<'a>,
        context: &Context<'a>,
        parent_resources: Resources<'a>,
    ) -> Option<Self> {
        let group_stream = dict.get::<Stream<'_>>(G)?;
        let group = FormXObject::new(&group_stream)?;
        // The mask is drawn from all of these: its dictionary (`/S`, `/G`, `/BC`,
        // `/TR`), the transform it is drawn in, the part of its group's box the clips
        // it is set under leave, and, where its group has no resources of its own,
        // the resources it falls back to; each dictionary named by where it is read
        // from, as the document holds it while it is loaded. Outside its group's box
        // the mask is its backdrop, so clips that leave the same part of the box draw
        // it alike, but for the phase of a shading sampled from the box's corner and
        // the antialiasing of the box's edges, and one serves them all, where drawing
        // it again for each would cost a page each time. It is drawn in the clips'
        // box of the first.
        let bbox = context.bbox();
        let [x0, y0, x1, y1] = group.bbox.map(f64::from);
        let group_box = context.get().ctm * group.matrix * Rect::new(x0, y0, x1, y1).to_path(0.1);
        let group_box = group_box.bounding_box();
        let drawn = bbox.intersect(group_box);
        let fallback = (!group.has_resources()).then(|| {
            [
                &parent_resources.ext_g_states,
                &parent_resources.fonts,
                &parent_resources.properties,
                &parent_resources.color_spaces,
                &parent_resources.x_objects,
                &parent_resources.patterns,
                &parent_resources.shadings,
            ]
            .map(Dict::content_key)
        });
        let key = hash128(&(
            dict.content_key(),
            context.get().ctm.cache_key(),
            drawn.cache_key(),
            fallback,
        ));
        let transfer_function = dict
            .get::<Object<'_>>(TR)
            .and_then(|o| Function::new(&o))
            .map(TransferFunction::new);
        let (mask_type, background) = match dict.get::<Name<'_>>(S)?.deref() {
            LUMINOSITY => {
                let cs = ColorSpace::new(
                    group.dict.get::<Dict<'_>>(GROUP)?.get::<Object<'_>>(CS)?,
                    &context.interpreter_cache.object_cache,
                )?;
                let color = dict
                    .get::<ColorComponents>(BC)
                    .map(|c| Color::new(cs, c, 1.0))
                    .unwrap_or(Color::new(ColorSpace::device_gray(), smallvec![0.0], 1.0));

                (MaskType::Luminosity, color)
            }
            ALPHA => (
                MaskType::Alpha,
                // Background color attribute should only be used with luminosity masks.
                Color::new(ColorSpace::device_gray(), smallvec![0.0], 1.0),
            ),
            _ => return None,
        };
        let nesting_depth = context.nesting_depth() + 1;

        Some(Self(Rc::new(Repr {
            key,
            group,
            mask_type,
            root_transform: context.get().ctm,
            transfer_function,
            bbox,
            interpreter_cache: context.interpreter_cache.clone(),
            settings: context.settings.clone(),
            xref: context.xref,
            background,
            parent_resources,
            nesting_depth,
        })))
    }

    /// Interpret the contents of the mask into the given device.
    pub fn interpret(&self, device: &mut impl Device<'a>) {
        let state = State::new(self.0.root_transform);
        let mut ctx = Context::new_with(
            self.0.root_transform,
            self.0.bbox,
            &self.0.interpreter_cache,
            self.0.xref,
            self.0.settings.clone(),
            state,
            self.0.nesting_depth,
        );
        self.0
            .group
            .draw(&self.0.parent_resources, &mut ctx, device);
    }

    /// Return the underlying mask type.
    pub fn mask_type(&self) -> MaskType {
        self.0.mask_type
    }

    /// The background color against which the mask should be composited.
    pub fn background_color(&self) -> Color {
        self.0.background.clone()
    }

    /// Return the transfer function that should be used for the mask.
    pub fn transfer_function(&self) -> Option<&TransferFunction> {
        self.0.transfer_function.as_ref()
    }
}
