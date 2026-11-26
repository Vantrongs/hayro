use super::form::FormXObject;
use crate::color::{Color, ColorComponents, ColorSpace};
use crate::context::{Context, InterpreterCache};
use crate::device::Device;
use crate::function::TransferFunction;
use crate::interpret::state::State;
use crate::util::hash128;
use crate::{CacheKey, InterpreterSettings};
use hayro_syntax::object::Name;
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::object::{Dict, Object};
use hayro_syntax::page::Resources;
use hayro_syntax::xref::XRef;
use kurbo::Affine;
use rustc_hash::FxHashMap;
use smallvec::smallvec;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::rc::Rc;

/// The type of a soft mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskType {
    /// A luminosity mask.
    Luminosity,
    /// An alpha mask.
    Alpha,
}

struct Repr<'a> {
    key: u128,
    key_in_box: u128,
    group: FormXObject<'a>,
    mask_type: MaskType,
    parent_resources: Resources<'a>,
    root_transform: Affine,
    bbox: kurbo::Rect,
    interpreter_cache: InterpreterCache<'a>,
    transfer_function: Option<TransferFunction>,
    settings: InterpreterSettings,
    background: Color,
    xref: &'a XRef,
    viewing: Option<Rc<crate::ocg::ViewingState>>,
    nesting_depth: u32,
}

impl Hash for Repr<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key_in_box.hash(state);
    }
}

/// A soft mask.
#[derive(Clone, Hash)]
pub struct SoftMask<'a>(Rc<Repr<'a>>);

impl Debug for SoftMask<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SoftMask({:032x})", self.0.key_in_box)
    }
}

impl PartialEq for SoftMask<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cache_key() == other.cache_key()
    }
}

impl Eq for SoftMask<'_> {}

impl CacheKey for SoftMask<'_> {
    fn cache_key(&self) -> u128 {
        self.0.key_in_box
    }
}

/// Identifies the placements that can reuse an interpreted soft mask.
#[must_use = "a drawing is kept for the uses it serves through `SoftMasks::insert`"]
#[derive(Clone, Copy, Debug)]
pub struct Interpreted {
    key: u128,
    /// Where the drawing read the box, the mask's key with the box.
    in_box: Option<u128>,
}

/// Caches soft masks by bounding box only when their drawing reads it.
pub struct SoftMasks<T> {
    drawn: FxHashMap<u128, Drawings<T>>,
}

enum Drawings<T> {
    InAnyBox(T),
    ByBox(FxHashMap<u128, T>),
}

impl<T> Default for SoftMasks<T> {
    fn default() -> Self {
        Self {
            drawn: FxHashMap::default(),
        }
    }
}

impl<T> SoftMasks<T> {
    /// The drawing kept that serves `mask`, if any.
    pub fn get(&self, mask: &SoftMask<'_>) -> Option<&T> {
        match self.drawn.get(&mask.0.key)? {
            Drawings::InAnyBox(drawn) => Some(drawn),
            Drawings::ByBox(by_box) => by_box.get(&mask.0.key_in_box),
        }
    }

    /// Keeps `drawn`, what was drawn by the interpretation that returned
    /// `interpreted`, for the uses it serves.
    pub fn insert(&mut self, interpreted: Interpreted, drawn: T) {
        let Some(in_box) = interpreted.in_box else {
            self.drawn
                .insert(interpreted.key, Drawings::InAnyBox(drawn));
            return;
        };
        let drawings = self
            .drawn
            .entry(interpreted.key)
            .or_insert_with(|| Drawings::ByBox(FxHashMap::default()));
        match drawings {
            Drawings::ByBox(by_box) => {
                by_box.insert(in_box, drawn);
            }
            Drawings::InAnyBox(_) => {
                *drawings = Drawings::ByBox([(in_box, drawn)].into_iter().collect());
            }
        }
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
        let transfer_function = dict
            .get::<Object<'_>>(TR)
            .and_then(|o| TransferFunction::new(&o));
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
        let parent_resources_key = group
            .resources
            .is_none()
            .then(|| parent_resources.cache_key());

        let bbox = context.bbox();
        let key = hash128(&(
            dict.cache_key(),
            nesting_depth,
            parent_resources_key,
            context.ctm().cache_key(),
        ));
        Some(Self(Rc::new(Repr {
            key,
            key_in_box: hash128(&(key, bbox.cache_key())),
            group,
            mask_type,
            root_transform: context.ctm(),
            transfer_function,
            bbox,
            interpreter_cache: context.interpreter_cache.clone(),
            settings: context.settings.clone(),
            xref: context.xref,
            background,
            parent_resources,
            viewing: context.ocg_state.viewing.clone(),
            nesting_depth,
        })))
    }

    /// Interpret the contents of the mask into the given device.
    pub fn interpret(&self, device: &mut impl Device<'a>) -> Interpreted {
        let state = State::new(self.0.root_transform);
        let mut ctx = Context::new_with(
            self.0.bbox,
            &self.0.interpreter_cache,
            self.0.xref,
            self.0.settings.clone(),
            state,
            self.0.viewing.clone(),
            self.0.nesting_depth,
        );
        self.0
            .group
            .draw(&self.0.parent_resources, &mut ctx, device);
        Interpreted {
            key: self.0.key,
            in_box: ctx.bbox_read.get().then_some(self.0.key_in_box),
        }
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
