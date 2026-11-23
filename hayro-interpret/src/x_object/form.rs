use crate::annotation::entries::Entries;
use crate::context::Context;
use crate::device::Device;
use crate::ocg::Kept;
use crate::{ClipPath, FillRule, interpret};
use hayro_syntax::content::TypedIter;
use hayro_syntax::object::Dict;
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::page::Resources;
use kurbo::{Affine, Rect, Shape};
use std::borrow::Cow;

pub(crate) fn box_and_matrix(dict: &Entries<'_>) -> Option<([f64; 4], [f64; 6])> {
    let b = dict.numbers(BBOX, 4)?;
    Some(([b[0], b[1], b[2], b[3]], dict.matrix(MATRIX)))
}

pub(crate) struct FormXObject<'a> {
    pub(crate) decoded: Cow<'a, [u8]>,
    pub(crate) matrix: Affine,
    pub(crate) bbox: [f32; 4],
    is_transparency_group: bool,
    own_oc: bool,
    isolated: bool,
    knockout: bool,
    pub(crate) dict: Dict<'a>,
    pub(crate) resources: Option<Resources<'a>>,
}

impl<'a> FormXObject<'a> {
    pub(crate) fn new(stream: &Stream<'a>) -> Option<Self> {
        let (bbox, matrix) = box_and_matrix(&Entries::detached(stream.dict().clone()))?;
        Self::placed(stream, Affine::new(matrix), bbox.map(|v| v as f32))
    }

    /// The form of `stream` drawn with `matrix` and clipped to `bbox`, whatever its
    /// own entries say.
    pub(crate) fn placed(stream: &Stream<'a>, matrix: Affine, bbox: [f32; 4]) -> Option<Self> {
        let dict = stream.dict();
        let decoded = stream.decoded_data().ok()?;
        let resources = dict.get(RESOURCES).map(Resources::new);
        let group = dict.get::<Dict<'_>>(GROUP);
        let flag = |key| {
            group
                .as_ref()
                .and_then(|g| g.get::<bool>(key))
                .unwrap_or(false)
        };

        Some(Self {
            decoded,
            matrix,
            is_transparency_group: group.is_some(),
            own_oc: true,
            isolated: flag(I),
            knockout: flag(K),
            bbox,
            dict: dict.clone(),
            resources,
        })
    }

    /// [`Self::placed`] for an annotation's appearance stream, drawn whatever its own
    /// `/OC` says, as `MuPDF` draws one (`PyMuPDF` 1.28.2): only the annotation's
    /// `/OC` hides it ([`crate::interpret::annotation`]).
    pub(crate) fn appearance(stream: &Stream<'a>, matrix: Affine, bbox: [f32; 4]) -> Option<Self> {
        let mut form = Self::placed(stream, matrix, bbox)?;
        form.own_oc = false;
        Some(form)
    }

    pub(crate) fn draw(
        &self,
        resources: &Resources<'a>,
        context: &mut Context<'a>,
        device: &mut impl Device<'a>,
    ) {
        if !context.ocg_state.is_visible() {
            return;
        }

        if !context.begin_nested_interpretation() {
            return;
        }

        let has_oc = self.own_oc
            && context
                .ocg_state
                .begin_named(&Kept::Document(self.dict.clone()), OC);
        if !context.ocg_state.is_visible() {
            if has_oc {
                context.ocg_state.end_marked_content();
            }
            context.end_nested_interpretation();
            return;
        }

        let iter = TypedIter::new(self.decoded.as_ref());

        context.path_mut().truncate(0);
        context.save_state();
        context.pre_concat_affine(self.matrix);
        context.push_root_transform();

        if self.is_transparency_group {
            device.push_transparency_group(
                context.fill_paint().alpha,
                std::mem::take(&mut context.compositing_mut().soft_mask),
                std::mem::take(&mut context.compositing_mut().blend_mode),
                self.isolated,
                self.knockout,
            );

            context.fill_paint_mut().alpha = 1.0;
            context.stroke_paint_mut().alpha = 1.0;
        }

        device.push_clip_path(&ClipPath {
            path: context.ctm()
                * Rect::new(
                    self.bbox[0] as f64,
                    self.bbox[1] as f64,
                    self.bbox[2] as f64,
                    self.bbox[3] as f64,
                )
                .to_path(0.1),
            fill: FillRule::NonZero,
        });

        interpret(
            iter,
            self.resources.as_ref().unwrap_or(resources),
            context,
            device,
        );

        device.pop_clip();

        if self.is_transparency_group {
            device.pop_transparency_group();
        }

        context.pop_root_transform();
        context.restore_state(device);

        if has_oc {
            context.ocg_state.end_marked_content();
        }

        context.end_nested_interpretation();
    }
}
