use crate::Renderer;
use hayro_interpret::Paint;
use hayro_interpret::encode::{EncodedShadingType, texture_dimensions};
use hayro_interpret::gradient::{SvgGradient, SvgGradientKind};
use hayro_interpret::pattern::Pattern;
use hayro_interpret::util::x_y_advances;
use kurbo::{Affine, BezPath, Rect, Shape};
use std::sync::Arc;
use vello_common::encode::{EncodeExt, EncodedPaint};
use vello_cpu::color::{AlphaColor, DynamicColor, Srgb};
use vello_cpu::peniko::{ColorStop, Gradient, ImageQuality, ImageSampler};
use vello_cpu::{Image, ImageSource, Level, PaintType, PixelMetadata, Pixmap, peniko};

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

                match *p {
                    Pattern::Shading(s) => {
                        clip_path = s.clip_path();
                        let encoded = s.encode();
                        let mut bbox = (*path_transform * path_bbox().to_path(0.0)).bounding_box();

                        if is_stroke {
                            // Try to account for stroke in bbox.
                            let (a1, a2) = x_y_advances(path_transform);
                            let factor = a1.length().max(a2.length()) * self.ctx.stroke().width;
                            bbox = bbox.inflate(factor, factor);
                        }

                        bbox = bbox.intersect(Rect::new(
                            0.0,
                            0.0,
                            self.ctx.width() as f64,
                            self.ctx.height() as f64,
                        ));

                        let level = self.ctx.render_settings().level;
                        let native = match &encoded.shading_type {
                            EncodedShadingType::RadialAxial(gradient) => gradient
                                .as_svg_gradient(&encoded, bbox)
                                .and_then(|native| native_gradient(&native, level)),
                            _ => None,
                        };
                        if let Some((gradient, transform)) = native {
                            // vello reads a gradient at each pixel's centre, as the
                            // shading is sampled.
                            paint_transform = path_transform.inverse() * transform;
                            PaintType::Gradient(gradient)
                        } else {
                            let (width, height) = texture_dimensions(bbox, 1.0);
                            let mut image = Vec::with_capacity(width as usize * height as usize);
                            let mut may_have_transparency = false;
                            let (width, height, transform) =
                                encoded.sample_texture(bbox, 1.0, |row| {
                                    for &sample in row {
                                        let pixel = AlphaColor::<Srgb>::new(sample)
                                            .premultiply()
                                            .to_rgba8();
                                        may_have_transparency |= pixel.a != 255;
                                        image.push(pixel);
                                    }
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
                                    x_extend: peniko::Extend::Repeat,
                                    y_extend: peniko::Extend::Repeat,
                                    quality: ImageQuality::Medium,
                                    alpha: 1.0,
                                },
                            };

                            PaintType::Image(image)
                        }
                    }
                    Pattern::Tiling(t) => {
                        const MAX_PIXMAP_SIZE: f32 = 3000.0;
                        // TODO: Raise this limit and perform downsampling if reached
                        // (see pdftc_100k_0138.pdf).
                        const MIN_PIXMAP_SIZE: f32 = 1.0;

                        let bbox = t.bbox;
                        let max_x_scale = MAX_PIXMAP_SIZE / bbox.width() as f32;
                        let min_x_scale = MIN_PIXMAP_SIZE / bbox.width() as f32;
                        let max_y_scale = MAX_PIXMAP_SIZE / bbox.height() as f32;
                        let min_y_scale = MIN_PIXMAP_SIZE / bbox.height() as f32;

                        let (mut xs, mut ys) = {
                            let (x, y) = x_y_advances(&(t.matrix));
                            (x.length() as f32, y.length() as f32)
                        };
                        xs = xs.max(min_x_scale).min(max_x_scale);
                        ys = ys.max(min_y_scale).min(max_y_scale);

                        let x_step = xs * t.x_step;
                        let y_step = ys * t.y_step;

                        let pix_width = x_step.abs().round().max(1.0) as u16;
                        let pix_height = y_step.abs().round().max(1.0) as u16;

                        let xs = pix_width as f32 / t.x_step.abs();
                        let ys = pix_height as f32 / t.y_step.abs();

                        let scaled_width = bbox.width() as f32 * xs;
                        let scaled_height = bbox.height() as f32 * ys;

                        let mut ctx = self.child_context(pix_width, pix_height);
                        let mut renderer = Renderer::new(&mut ctx, self.global);
                        renderer.inside_pattern = true;
                        let mut initial_transform = Affine::scale_non_uniform(xs as f64, ys as f64)
                            * Affine::translate((-bbox.x0, -bbox.y0));
                        t.interpret(&mut renderer, initial_transform, is_stroke);
                        let mut pix = Pixmap::new(pix_width, pix_height);
                        renderer.ctx.flush();
                        let mut resources = vello_cpu::Resources::default();
                        renderer.ctx.render(&mut pix, &mut resources);

                        // TODO: Fix these
                        if x_step < 0.0 {
                            initial_transform *=
                                Affine::new([-1.0, 0.0, 0.0, 1.0, scaled_width as f64, 0.0]);
                        }

                        if y_step < 0.0 {
                            initial_transform *=
                                Affine::new([1.0, 0.0, 0.0, -1.0, 0.0, scaled_height as f64]);
                        }

                        paint_transform =
                            path_transform.inverse() * t.matrix * initial_transform.inverse();

                        let image = Image {
                            image: ImageSource::Pixmap(Arc::new(pix)),
                            sampler: ImageSampler {
                                x_extend: peniko::Extend::Repeat,
                                y_extend: peniko::Extend::Repeat,
                                quality: ImageQuality::Medium,
                                alpha: t.opacity,
                            },
                        };

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

/// The gradient vello draws for `native`, and the transform from the paint's
/// space to the device, where it paints within a level of its stops
/// ([`SvgGradient::paints_within_a_level`]): its table of colors, as vello
/// builds it at `level`, is fine enough for them. `None` where it is not, so
/// the shading is sampled instead.
fn native_gradient(native: &SvgGradient, level: Level) -> Option<(Gradient, Affine)> {
    let gradient = drawn(native);
    let mut paints = Vec::new();
    gradient.encode_into(&mut paints, Affine::IDENTITY, None)?;
    // Fewer than two stops encode a solid color, which needs no table.
    let fits = match paints.pop() {
        Some(EncodedPaint::Gradient(encoded)) => vello_common::fearless_simd::dispatch!(
            level,
            simd => native.paints_within_a_level(encoded.u8_lut(simd).lut())
        ),
        _ => true,
    };
    fits.then_some((gradient, native.transform))
}

/// The gradient vello draws for `native`'s stops.
fn drawn(native: &SvgGradient) -> Gradient {
    let stops = native
        .stops
        .iter()
        .map(|stop| ColorStop {
            offset: stop.offset,
            color: DynamicColor::from_alpha_color(AlphaColor::<Srgb>::new(stop.color)),
        })
        .collect::<Vec<_>>();
    match native.kind {
        SvgGradientKind::Linear { start, end } => Gradient::new_linear(start, end),
    }
    .with_extend(peniko::Extend::Pad)
    .with_stops(stops.as_slice())
}

#[cfg(test)]
mod tests {
    use super::{drawn, native_gradient};
    use hayro_interpret::gradient::{SvgGradient, SvgGradientKind, SvgGradientStop};
    use kurbo::{Affine, Point};
    use vello_common::encode::{EncodeExt, EncodedPaint};
    use vello_cpu::Level;

    fn gray(offset: f32, level: f32) -> SvgGradientStop {
        let v = level / 255.0;
        SvgGradientStop {
            offset,
            color: [v, v, v, 1.0],
        }
    }

    /// Where [`rise`] starts: nine tenths of the way from the 1638th of 4096
    /// equal steps to the next, past the 1638th entry of a table of 4096 at
    /// 1638/4095.
    const RISE: f32 = 1638.9 / 4096.0;

    /// Gray rising from 100 to 103 levels from [`RISE`] over nine tenths of an
    /// entry of 4096, flat around, for a gradient 1000 pixels long: a step of a
    /// color table of three levels, a fourth of a pixel wide, as the shading
    /// drawn directly may be off by there, and flat at the entry before it.
    fn rise() -> SvgGradient {
        let x0 = RISE;
        SvgGradient {
            kind: SvgGradientKind::Linear {
                start: Point::ZERO,
                end: Point::new(1.0, 0.0),
            },
            transform: Affine::IDENTITY,
            stops: vec![
                gray(0.0, 100.0),
                gray(1.0 / 4095.0, 100.0),
                gray(x0 - 0.2 / 4095.0, 100.0),
                gray(x0, 100.0),
                gray(x0 + 0.9 / 4095.0, 103.0),
                gray(1.0, 103.0),
            ],
            range: (0.0, 1.0),
            pixel_step: 1.0 / 1000.0,
            table_steps: vec![0.0, 0.0, 0.0, 3.0, 3.0, 0.0],
        }
    }

    /// A gradient whose color steps by three levels within an entry, a step of
    /// a color table, is drawn natively: each entry of vello's table holds the
    /// color of its stops at its own place, the entry just before the step
    /// the flat 100 there.
    #[test]
    fn a_gradient_stepping_within_an_entry_is_drawn_natively() {
        let native = rise();
        let mut paints = Vec::new();
        drawn(&native)
            .encode_into(&mut paints, Affine::IDENTITY, None)
            .expect("a gradient");
        let Some(EncodedPaint::Gradient(encoded)) = paints.pop() else {
            panic!("a gradient's table");
        };
        let lut = vello_common::fearless_simd::dispatch!(
            Level::new(),
            simd => encoded.u8_lut(simd).lut().to_vec()
        );
        assert_eq!(lut.len(), 4096);
        const { assert!((1638.0 / 4095.0) < RISE) };
        assert_eq!(lut[1638][0], 100);
        assert!(native.paints_within_a_level(&lut));
        assert!(native_gradient(&native, Level::new()).is_some());
    }
}
