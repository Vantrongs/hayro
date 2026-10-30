use crate::Renderer;
use hayro_interpret::Paint;
use hayro_interpret::encode::{EncodedShadingType, Unmetered, texture_dimensions};
use hayro_interpret::gradient::{GradientRow, SvgGradient, SvgGradientKind};
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
                        let axial = match &encoded.shading_type {
                            EncodedShadingType::RadialAxial(gradient) => gradient
                                .as_svg_gradient(&encoded, bbox)
                                .and_then(|native| axial_paint(&native, level)),
                            _ => None,
                        };
                        if let Some((paint, transform)) = axial {
                            // vello reads a gradient, and a texture, at each pixel's
                            // centre, as the shading is sampled.
                            paint_transform = path_transform.inverse() * transform;
                            paint
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

/// The paint of an axial shading's `native` gradient, and the transform from
/// the paint's space to the device: the gradient vello draws natively where
/// its table of colors paints within a level of the stops
/// ([`native_gradient`]), and otherwise a texture one texel high along its
/// line that does ([`SvgGradient::row_within_a_level_metered`]), so its work
/// follows the pixels the line spans, not those the shape covers. `None`
/// where neither does, so the shading is sampled over the shape.
fn axial_paint(native: &SvgGradient, level: Level) -> Option<(PaintType, Affine)> {
    if let Some((gradient, transform)) = native_gradient(native, level) {
        return Some((PaintType::Gradient(gradient), transform));
    }
    let row = native
        .row_within_a_level_metered(&mut Unmetered, usize::from(u16::MAX))
        .ok()??;
    let image = row_image(&row)?;
    Some((PaintType::Image(image), native.transform * row.to_gradient))
}

/// The texture of `row`, padded past its ends; `None` for one wider than a
/// pixmap holds, which [`axial_paint`] asks for none of.
fn row_image(row: &GradientRow) -> Option<Image> {
    let width = u16::try_from(row.colors.len()).ok()?;
    let mut may_have_transparency = false;
    let texels: Vec<_> = row
        .colors
        .iter()
        .map(|&color| {
            let pixel = AlphaColor::<Srgb>::new(color).premultiply().to_rgba8();
            may_have_transparency |= pixel.a != 255;
            pixel
        })
        .collect();
    let pixmap = Pixmap::from_parts(
        bytemuck::cast_vec(texels),
        width,
        1,
        PixelMetadata {
            may_have_transparency,
            ..Default::default()
        },
    );
    Some(Image {
        image: ImageSource::Pixmap(Arc::new(pixmap)),
        sampler: ImageSampler {
            x_extend: peniko::Extend::Pad,
            y_extend: peniko::Extend::Pad,
            quality: ImageQuality::Medium,
            alpha: 1.0,
        },
    })
}

/// The fewest colors vello's table of a gradient of four stops or more holds
/// (`vello_common::encode::GradientLut`): a test checks it.
const LEAST_TABLE_ENTRIES: usize = 1024;

/// The gradient vello draws for `native`, and the transform from the paint's
/// space to the device, where it paints within a level of its stops
/// ([`SvgGradient::paints_within_a_level`]): its table of colors, as vello
/// builds it at `level`, is fine enough for them, as the stops alone show
/// where they bend little enough over an entry
/// ([`SvgGradient::within_a_level_of_any_table`]). `None` where it is not.
fn native_gradient(native: &SvgGradient, level: Level) -> Option<(Gradient, Affine)> {
    let gradient = drawn(native);
    let mut paints = Vec::new();
    gradient.encode_into(&mut paints, Affine::IDENTITY, None)?;
    // vello paints a gradient it takes for degenerate in its first color
    // alone, encoding no table: that paints the stops only where they are all
    // that color.
    let Some(EncodedPaint::Gradient(encoded)) = paints.pop() else {
        return native
            .is_one_color()
            .then_some((gradient, native.transform));
    };
    let fits = native.stops.len() >= 4 && native.within_a_level_of_any_table(LEAST_TABLE_ENTRIES)
        || vello_common::fearless_simd::dispatch!(
            level,
            simd => native.paints_within_a_level(encoded.u8_lut(simd).lut())
        );
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
    use super::{LEAST_TABLE_ENTRIES, axial_paint, drawn, native_gradient};
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

    /// A gradient whose ends vello takes for one point, which it paints in its
    /// first color alone, is drawn natively only where its stops are all that
    /// color: a ramp so drawn is not, and goes to a row.
    #[test]
    fn a_gradient_vello_paints_in_one_color_is_native_only_where_it_is_one() {
        let point = |stops: Vec<SvgGradientStop>| SvgGradient {
            kind: SvgGradientKind::Linear {
                start: Point::ZERO,
                end: Point::new(1e-7, 0.0),
            },
            ..along(stops, (0.0, 1.0), 1.0 / 100.0)
        };
        let ramp = point(vec![gray(0.0, 0.0), gray(1.0, 255.0)]);
        assert!(native_gradient(&ramp, Level::new()).is_none());
        assert!(matches!(
            axial_paint(&ramp, Level::new()),
            Some((vello_cpu::PaintType::Image(_), _))
        ));
        let flat = point(vec![gray(0.0, 40.0), gray(0.5, 40.0), gray(1.0, 40.0)]);
        assert!(native_gradient(&flat, Level::new()).is_some());
    }

    /// Gray zigzagging 40 times between 0 and 255 across 1600 pixels, 6.4
    /// levels a pixel, too steeply for vello's table of 1024 colors, is drawn as
    /// a row along its line: of eight texels a pixel, the fewest that read its
    /// peaks, between texel centres an eighth of a pixel apart, within a level
    /// (0.4 levels below the peak, and half a level of rounding).
    #[test]
    fn a_gradient_too_steep_for_the_table_is_drawn_as_a_row() {
        let stops = (0..=40)
            .map(|i| gray(i as f32 / 40.0, if i % 2 == 0 { 0.0 } else { 255.0 }))
            .collect();
        let native = along(stops, (0.0, 1.0), 1.0 / 1600.0);
        assert!(native_gradient(&native, Level::new()).is_none());
        let Some((vello_cpu::PaintType::Image(image), _)) = axial_paint(&native, Level::new())
        else {
            panic!("a row");
        };
        let vello_cpu::ImageSource::Pixmap(pixmap) = image.image else {
            panic!("a pixmap");
        };
        assert_eq!(pixmap.height(), 1);
        assert!(
            (8 * 1600..=8 * 1600 + 1).contains(&pixmap.width()),
            "{}",
            pixmap.width()
        );
    }

    /// xorshift64*, so the cases are the same in every run.
    struct Rng(u64);

    impl Rng {
        fn unit(&mut self) -> f32 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1_u32 << 24) as f32
        }
    }

    fn along(stops: Vec<SvgGradientStop>, range: (f32, f32), pixel_step: f32) -> SvgGradient {
        SvgGradient {
            kind: SvgGradientKind::Linear {
                start: Point::ZERO,
                end: Point::new(1.0, 0.0),
            },
            transform: Affine::IDENTITY,
            stops,
            range,
            pixel_step,
            table_steps: vec![],
        }
    }

    /// vello's table of the gradient drawn for `native`.
    fn table(native: &SvgGradient) -> Vec<[u8; 4]> {
        let mut paints = Vec::new();
        drawn(native)
            .encode_into(&mut paints, Affine::IDENTITY, None)
            .expect("a gradient");
        let Some(EncodedPaint::Gradient(encoded)) = paints.pop() else {
            panic!("a gradient's table");
        };
        vello_common::fearless_simd::dispatch!(
            Level::new(),
            simd => encoded.u8_lut(simd).lut().to_vec()
        )
    }

    /// `n` stops at random places in order, some sharing one, at neither end
    /// some of the time.
    fn offsets(rng: &mut Rng, n: usize) -> Vec<f32> {
        let mut at: Vec<f32> = (0..n)
            .map(|_| {
                let u = rng.unit();
                if u < 0.05 {
                    0.0
                } else if u > 0.95 {
                    1.0
                } else {
                    u
                }
            })
            .collect();
        at.sort_by(f32::total_cmp);
        for i in 1..n {
            if rng.unit() < 0.1 {
                at[i] = at[i - 1];
            }
        }
        at
    }

    /// vello's table of a gradient of four stops or more holds
    /// [`LEAST_TABLE_ENTRIES`] colors or more, wherever its stops lie; of three,
    /// fewer.
    #[test]
    fn vellos_table_of_four_stops_holds_the_least_entries() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for case in 0..2000 {
            let n = 4 + case % 60;
            let stops = offsets(&mut rng, n)
                .into_iter()
                .map(|at| gray(at, (rng.unit() * 255.0).floor()))
                .collect();
            let len = table(&along(stops, (0.0, 1.0), 1e-3)).len();
            assert!(len >= LEAST_TABLE_ENTRIES, "{n} stops: {len}");
        }
        let three = vec![gray(0.0, 0.0), gray(0.5, 255.0), gray(1.0, 0.0)];
        assert!(table(&along(three, (0.0, 1.0), 1e-3)).len() < LEAST_TABLE_ENTRIES);
    }

    /// Where the stops alone show every table of [`LEAST_TABLE_ENTRIES`] or more
    /// paints within a level of them, vello's table does: over opaque gray and
    /// RGB stops whose lines change by up to 1.3 levels over an entry, up to a
    /// steepness random for each gradient, about where the bound falls, at
    /// random places, some sharing one, over random ranges and pixel steps.
    #[test]
    fn stops_within_a_level_of_any_table_are_within_a_level_of_vellos() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let entry = 1.0 / (LEAST_TABLE_ENTRIES - 1) as f32;
        let (mut within, mut vellos) = (0, 0);
        for case in 0..4000 {
            let n = 4 + case % 40;
            let at = offsets(&mut rng, n);
            let rgb = case % 2 == 1;
            let mut color = [rng.unit(), rng.unit(), rng.unit()];
            let steepest = rng.unit() * 1.3;
            let mut stops = Vec::with_capacity(n);
            for (i, &offset) in at.iter().enumerate() {
                if i > 0 {
                    let span = offset - at[i - 1];
                    for c in &mut color {
                        let levels = (rng.unit() * 2.0 - 1.0) * steepest * span / entry;
                        *c = (*c + levels / 255.0).clamp(0.0, 1.0);
                    }
                }
                let c = if rgb { color } else { [color[0]; 3] };
                stops.push(SvgGradientStop {
                    offset,
                    color: [c[0], c[1], c[2], 1.0],
                });
            }
            let (a, b) = (rng.unit(), rng.unit());
            let range = (a.min(b), a.max(b));
            let native = along(stops, range, rng.unit() * 4.0 * entry);
            if native.within_a_level_of_any_table(LEAST_TABLE_ENTRIES) {
                within += 1;
                let lut = table(&native);
                vellos += 1;
                assert!(
                    native.paints_within_a_level(&lut),
                    "case {case}: {} stops, {} entries",
                    native.stops.len(),
                    lut.len()
                );
            }
        }
        assert!(within > 1000, "{within}");
        assert_eq!(within, vellos);
    }
}
