use crate::Renderer;
use hayro_interpret::encode::{
    Cost, EncodedShadingPattern, EncodedShadingType, Unmetered, texture_dimensions,
};
use hayro_interpret::gradient::{SampleRamp, SvgGradientKind};
use hayro_interpret::pattern::{Pattern, ShadingPattern};
use hayro_interpret::util::x_y_advances;
use hayro_interpret::{CacheKey, Paint};
use kurbo::{Affine, BezPath, Rect};
use std::sync::Arc;
use vello_cpu::color::{AlphaColor, DynamicColor, Srgb};
use vello_cpu::peniko::{ColorStop, Gradient, ImageQuality, ImageSampler};
use vello_cpu::{Image, ImageSource, PaintType, PixelMetadata, Pixmap, peniko};

pub(super) struct PreparedRamp {
    shading: u128,
    opacity: u32,
    samples: u64,
    setup: Cost,
    ramp: Option<Arc<SampleRamp>>,
}

impl Renderer<'_> {
    fn prepare_shading(&self, pattern: &ShadingPattern, bbox: Rect) -> EncodedShadingPattern {
        let mut encoded = pattern.encode();
        if pattern.transfer_function.is_some()
            || !matches!(encoded.shading_type, EncodedShadingType::RadialAxial(_))
        {
            return encoded.prepare_for(bbox);
        }
        let samples = bbox.area().ceil() as u64;
        let shading = pattern.shading.cache_key();
        let opacity = pattern.opacity.to_bits();
        let mut slot = self.global.last_ramp.borrow_mut();
        // Direct sampling can warm the tint table after preparation exhausted its budget.
        if let Some(previous) = slot.as_ref().filter(|previous| {
            previous.shading == shading
                && previous.opacity == opacity
                && if previous.ramp.is_some() {
                    samples >= previous.samples
                } else {
                    samples <= previous.samples && previous.setup == encoded.setup_cost()
                }
        }) {
            encoded.set_sample_ramp(previous.ramp.clone());
        } else {
            let setup = encoded.setup_cost();
            let ramp = encoded
                .sample_ramp_for(samples, &mut Unmetered)
                .expect("unmetered");
            encoded.set_sample_ramp(ramp.clone());
            *slot = Some(PreparedRamp {
                shading,
                opacity,
                setup,
                samples,
                ramp,
            });
        }
        encoded
    }

    #[must_use]
    pub(super) fn set_paint(
        &mut self,
        paint: &Paint<'_>,
        path_bbox: impl Fn() -> Rect,
        is_stroke: bool,
    ) -> Option<BezPath> {
        let mut paint_transform = Affine::IDENTITY;
        let mut clip_path = None;

        let paint: PaintType = match paint.clone() {
            Paint::Color(c) => {
                let c = c.to_rgba().to_rgba8();
                AlphaColor::from_rgba8(c[0], c[1], c[2], c[3]).into()
            }
            Paint::Pattern(p) => {
                let path_transform = self.ctx.transform();
                let bbox = {
                    let mut bbox = path_transform.transform_rect_bbox(path_bbox());

                    if is_stroke {
                        let (a1, a2) = x_y_advances(path_transform);
                        let factor = a1.length().max(a2.length()) * self.ctx.stroke().width;
                        bbox = bbox.inflate(factor, factor);
                    }

                    bbox.intersect(Rect::new(
                        0.0,
                        0.0,
                        self.ctx.width() as f64,
                        self.ctx.height() as f64,
                    ))
                };

                match *p {
                    Pattern::Shading(s) => {
                        const NATIVE_GRADIENT_TOLERANCE: f32 = 0.01;

                        clip_path = s.clip_path();
                        let encoded = self.prepare_shading(&s, bbox);

                        if let EncodedShadingType::RadialAxial(gradient) = &encoded.shading_type
                            && let Some(native) =
                                gradient.as_svg_gradient(&encoded, NATIVE_GRADIENT_TOLERANCE)
                        {
                            paint_transform = path_transform.inverse() * native.transform;

                            let stops = native
                                .stops
                                .iter()
                                .map(|stop| ColorStop {
                                    offset: stop.offset,
                                    color: DynamicColor::from_alpha_color(AlphaColor::<Srgb>::new(
                                        stop.color,
                                    )),
                                })
                                .collect::<Vec<_>>();

                            let gradient = match native.kind {
                                SvgGradientKind::Linear { start, end } => {
                                    Gradient::new_linear(start, end)
                                }
                                SvgGradientKind::Radial {
                                    start_center,
                                    start_radius,
                                    end_center,
                                    end_radius,
                                } => Gradient::new_two_point_radial(
                                    start_center,
                                    start_radius,
                                    end_center,
                                    end_radius,
                                ),
                            }
                            .with_extend(peniko::Extend::Pad)
                            .with_stops(stops.as_slice());

                            PaintType::Gradient(gradient)
                        } else {
                            let bbox = bbox.expand();
                            let (width, height) = texture_dimensions(bbox, 1.0);
                            let mut image = Vec::with_capacity(width as usize * height as usize);
                            let mut may_have_transparency = false;
                            let (width, height, transform) =
                                encoded.sample_texture(bbox, 1.0, |sample| {
                                    let pixel =
                                        AlphaColor::<Srgb>::new(sample).premultiply().to_rgba8();
                                    may_have_transparency |= pixel.a != 255;
                                    image.push(pixel);
                                });
                            paint_transform = path_transform.inverse() * transform;

                            let pixmap = Pixmap::from_parts(
                                bytemuck::cast_vec(image),
                                width as u16,
                                height as u16,
                                PixelMetadata {
                                    may_have_transparency,
                                    ..Default::default()
                                },
                            );

                            let image = Image {
                                image: ImageSource::Pixmap(Arc::new(pixmap)),
                                sampler: ImageSampler {
                                    x_extend: peniko::Extend::Pad,
                                    y_extend: peniko::Extend::Pad,
                                    quality: ImageQuality::Medium,
                                    alpha: 1.0,
                                },
                            };

                            PaintType::Image(image)
                        }
                    }
                    Pattern::Tiling(t) => {
                        let (image, transform) = self.tiling_paint(&t, bbox, is_stroke);
                        paint_transform = path_transform.inverse() * transform;

                        PaintType::Image(image)
                    }
                }
            }
        };

        self.ctx.set_paint_transform(paint_transform);
        self.ctx.set_paint(paint);

        clip_path
    }
}

#[cfg(test)]
#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Context, GlobalState, Pdf, RenderCache, RenderContext, RenderSettings};
    use hayro_interpret::{InterpreterSettings, hayro_syntax::content::TypedIter, interpret};

    #[test]
    fn warming_a_tint_table_changes_the_next_ramp_decision() {
        let shading = b"<< /ShadingType 2 /Coords [0 0 10 0] /Extend [true true] /Function 5 0 R
            /ColorSpace [/Separation /Spot /DeviceRGB
            << /FunctionType 2 /Domain [0 1] /C0 [0 0 0] /C1 [1 0 0] /N 1 >>] >>";
        let function = fixtures::stream(
            "/FunctionType 0 /Domain [0 1] /Range [0 1] /Size [2] /BitsPerSample 8",
            &[0, 1],
        );
        let pdf = Pdf::new(fixtures::one_page(
            10,
            "/Resources << /Shading << /S 4 0 R >> >>",
            vec![shading.to_vec(), function],
        ))
        .unwrap();
        let cache = RenderCache::new();
        let global = GlobalState::new(&cache, &RenderSettings::default());
        let mut ctx = RenderContext::new(10, 10);
        let mut renderer = Renderer::new(&mut ctx, &global);
        let mut state = Context::new(
            Affine::IDENTITY,
            Rect::new(0., 0., 10., 10.),
            &cache.interpreter_cache,
            pdf.xref(),
            InterpreterSettings::default(),
        );
        let mut draw = || {
            interpret(
                TypedIter::new(b"/S sh"),
                pdf.pages()[0].resources(),
                &mut state,
                &mut renderer,
            );
            let slot = global.last_ramp.borrow();
            let prepared = slot.as_ref().unwrap();
            (prepared.setup, prepared.ramp.is_some())
        };
        let (cold, first) = draw();
        assert!(cold > Cost::ZERO && !first);
        let second = draw();
        global.last_ramp.take();
        assert_eq!(second, draw());
        assert_eq!(second, (Cost::ZERO, true));
    }
}
