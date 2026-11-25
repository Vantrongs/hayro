use crate::Renderer;
use hayro_interpret::Paint;
use hayro_interpret::encode::{EncodedShadingType, texture_dimensions};
use hayro_interpret::gradient::{SvgGradientKind, SvgGradientStop};
use hayro_interpret::pattern::Pattern;
use hayro_interpret::util::x_y_advances;
use kurbo::{Affine, BezPath, Rect};
use std::sync::Arc;
use vello_cpu::color::{AlphaColor, DynamicColor, Srgb};
use vello_cpu::peniko::{ColorStop, Gradient, ImageQuality, ImageSampler};
use vello_cpu::{Image, ImageSource, PaintType, PixelMetadata, Pixmap, peniko};

const STOP_TOLERANCE: f32 = 1.0 / 255.0;

impl Renderer<'_> {
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

                        clip_path = s.shading.clip_path.clone();
                        let encoded = s.encode();

                        if let EncodedShadingType::RadialAxial(gradient) = &encoded.shading_type
                            && let Some(native) = gradient.as_svg_gradient(&encoded, STOP_TOLERANCE)
                            && lut_resolves(&native.stops, NATIVE_GRADIENT_TOLERANCE)
                        {
                            paint_transform = path_transform.inverse() * native.transform;

                            let mut gradient = match native.kind {
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
                            .with_extend(peniko::Extend::Pad);
                            gradient
                                .stops
                                .extend(native.stops.into_iter().map(|stop| ColorStop {
                                    offset: stop.offset,
                                    color: DynamicColor::from_alpha_color(AlphaColor::<Srgb>::new(
                                        stop.color,
                                    )),
                                }));

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
                        let (image, transform) = self.tiling_paint(&t, bbox);
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

fn lut_resolves(stops: &[SvgGradientStop], tolerance: f32) -> bool {
    // Match vello_common::encode::{determine_lut_size, GradientLut::new_inner}.
    let mut size = match stops.len() {
        2 => 256,
        3 => 512,
        _ => 1024,
    };
    for pair in stops.windows(2) {
        let width = pair[1].offset - pair[0].offset;
        if width == 0.0 && pair[0].color != pair[1].color {
            return false;
        }
        size = size.max(
            ((1.0 / width).ceil() as usize)
                .min(4096)
                .next_power_of_two(),
        );
    }
    let scale = (size - 1) as f32;
    // The stop approximation spends one byte; rounding both colors can round the difference up.
    let fits = |delta: f32| ((STOP_TOLERANCE + delta) * 255.0).ceil() <= tolerance * 255.0;
    let channel = |stop: &SvgGradientStop, c: usize| {
        if c == 3 {
            stop.color[c]
        } else {
            stop.color[c] * stop.color[3]
        }
    };
    for pair in stops.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let width = b.offset - a.offset;
        if width == 0.0 {
            continue;
        }
        if !(0..4).all(|c| fits((channel(b, c) - channel(a, c)).abs() / (width * scale))) {
            return false;
        }
        if stops.len() == 2 {
            continue;
        }
        // Interior cells follow one slope. Boundary cells can extrapolate another slope.
        for edge in [a.offset, b.offset] {
            let idx = (edge * scale) as usize;
            let segment = stops[1..stops.len() - 1]
                .partition_point(|s| (s.offset * size as f32) as usize <= idx);
            let (left, right) = (&stops[segment], &stops[segment + 1]);
            let t = idx as f32 / scale;
            for c in 0..4 {
                let slope = (channel(right, c) - channel(left, c))
                    / (right.offset - left.offset).max(1.0e-7);
                let sample = slope
                    .mul_add(t, channel(left, c) - left.offset * slope)
                    .clamp(0.0, 1.0);
                for x in [t.max(a.offset), ((idx + 1) as f32 / scale).min(b.offset)] {
                    let expected =
                        channel(a, c) + (channel(b, c) - channel(a, c)) * ((x - a.offset) / width);
                    if !fits((sample - expected).abs()) {
                        return false;
                    }
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lut_rejects_stitched_band() {
        let stops = [
            (0.0, 0.0),
            (0.025, 0.0),
            (0.025, 1.0),
            (0.075, 1.0),
            (0.075, 0.0),
            (1.0, 0.0),
        ]
        .map(|(offset, gray)| SvgGradientStop {
            offset,
            color: [gray, gray, gray, 1.0],
        });
        assert!(!lut_resolves(&stops, 0.01));
    }
}
