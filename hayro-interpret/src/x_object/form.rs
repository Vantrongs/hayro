use super::xobject_oc;
use crate::context::Context;
use crate::device::Device;
use crate::interpret::annotation::entries::Entries;
use crate::{ClipPath, FillRule, interpret};
use hayro_syntax::content::TypedIter;
use hayro_syntax::object::Dict;
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::*;
use hayro_syntax::page::Resources;
use kurbo::{Affine, Rect, Shape};
use std::borrow::Cow;

/// The `/BBox` and `/Matrix` of a form, read as viewers read damaged ones: each entry
/// keeps its place, one that is not a number reads as 0, and missing ones are 0.
/// A form whose `/BBox` is not an array has no box and is not drawn; one whose
/// `/Matrix` is not an array is drawn with the identity.
pub(crate) fn box_and_matrix(dict: &Dict<'_>) -> Option<([f64; 4], [f64; 6])> {
    let dict = Entries::new(dict.clone());
    let b = dict.numbers(BBOX, 4)?;
    Some(([b[0], b[1], b[2], b[3]], dict.matrix(MATRIX)))
}

pub(crate) struct FormXObject<'a> {
    pub(crate) decoded: Cow<'a, [u8]>,
    pub(crate) matrix: Affine,
    pub(crate) bbox: [f32; 4],
    is_transparency_group: bool,
    pub(crate) dict: Dict<'a>,
    resources: Option<Resources<'a>>,
}

impl<'a> FormXObject<'a> {
    pub(crate) fn new(stream: &Stream<'a>) -> Option<Self> {
        let (bbox, matrix) = box_and_matrix(stream.dict())?;
        Self::placed(stream, Affine::new(matrix), bbox.map(|v| v as f32))
    }

    /// The form of `stream` drawn with `matrix` and clipped to `bbox`, whatever its
    /// own entries say.
    pub(crate) fn placed(stream: &Stream<'a>, matrix: Affine, bbox: [f32; 4]) -> Option<Self> {
        let dict = stream.dict();

        let decoded = stream.decoded().ok()?;
        let resources = dict.get(RESOURCES).map(Resources::new);

        let is_transparency_group = dict.get::<Dict<'_>>(GROUP).is_some();

        Some(Self {
            decoded,
            matrix,
            is_transparency_group,
            bbox,
            dict: dict.clone(),
            resources,
        })
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

        let has_oc = xobject_oc(&self.dict, context);
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
                context.get().graphics_state.non_stroke_alpha,
                std::mem::take(&mut context.get_mut().graphics_state.soft_mask),
                std::mem::take(&mut context.get_mut().graphics_state.blend_mode),
            );

            context.get_mut().graphics_state.non_stroke_alpha = 1.0;
            context.get_mut().graphics_state.stroke_alpha = 1.0;
        }

        device.push_clip_path(&ClipPath {
            path: context.get().ctm
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
