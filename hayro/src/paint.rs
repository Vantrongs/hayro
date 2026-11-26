use crate::Renderer;
use hayro_interpret::Paint;
use hayro_interpret::encode::{EncodedShadingPattern, EncodedShadingType, texture_dimensions};
use hayro_interpret::gradient::{PreparedShading, SvgGradient, SvgGradientKind, SvgGradientStop};
use hayro_interpret::pattern::{Pattern, ShadingPattern};
use hayro_interpret::util::x_y_advances;
use kurbo::{Affine, BezPath, Rect};
use std::sync::Arc;
use vello_cpu::color::{AlphaColor, DynamicColor, Srgb};
use vello_cpu::peniko::{ColorStop, Gradient, ImageQuality, ImageSampler};
use vello_cpu::{Image, ImageSource, PaintType, PixelMetadata, Pixmap, peniko};

const NATIVE_GRADIENT_TOLERANCE: f32 = 0.01;
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
                let path_transform = *self.ctx.transform();
                let bbox = {
                    let mut bbox = path_transform.transform_rect_bbox(path_bbox());

                    if is_stroke {
                        let (a1, a2) = x_y_advances(&path_transform);
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
                        clip_path = s.clip_path();
                        let (paint, transform) = self.shading_paint(s, bbox.expand());
                        paint_transform = path_transform.inverse() * transform;
                        paint
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

impl Renderer<'_> {
    fn shading_paint(&mut self, pattern: ShadingPattern, mut bbox: Rect) -> (PaintType, Affine) {
        let mut encoded = pattern.encode();
        if let EncodedShadingType::RadialAxial(g) = &mut encoded.shading_type {
            g.cover_placement(bbox, encoded.base_transform);
        }
        if let EncodedShadingType::RadialAxial(g) = &encoded.shading_type {
            if let Some(native) = g
                .as_svg_gradient(&encoded, STOP_TOLERANCE)
                .and_then(SvgGradient::with_raster_geometry)
                .and_then(|gradient| resolved_gradient(gradient, g.clamps_child_domain()))
            {
                return native_paint(native);
            }
            let texture_bbox = g.texture_bounds(bbox, &encoded);
            let compact = texture_bbox != bbox;
            bbox = texture_bbox;
            let matrix = pattern.matrix;
            let cache = self
                .gradient_cache
                .entry(PreparedShading::cache_key(&pattern))
                .or_default();
            cache.pixels += bbox.area();
            if cache.pixels >= PreparedShading::MIN_SAMPLES as f64 {
                encoded = cache
                    .shading
                    .get_or_init(|| PreparedShading::new(pattern))
                    .encode(matrix);
                if let EncodedShadingType::RadialAxial(g) = &mut encoded.shading_type {
                    g.cover_placement(bbox, encoded.base_transform);
                }
                if let EncodedShadingType::RadialAxial(g) = &encoded.shading_type
                    && !compact
                {
                    if g.is_extended()
                        && cache.pixels >= (STRIP_WIDTH * STRIP_ROWS) as f64
                        && let Some(transform) = strip_transform(encoded.base_transform, bbox)
                        && let Some(strip) = cache
                            .strip
                            .get_or_init(|| g.axial_strip(&encoded).map(strip_pixmap))
                    {
                        let image = Image {
                            image: ImageSource::Pixmap(strip.clone()),
                            sampler: ImageSampler {
                                quality: ImageQuality::Low,
                                y_extend: peniko::Extend::Reflect,
                                ..Default::default()
                            },
                        };
                        return (PaintType::Image(image), transform);
                    }
                    if let Some(native) = g.as_raster_gradient(&encoded) {
                        return native_paint(native);
                    }
                }
            }
        }
        texture_paint(&encoded.prepare_for(bbox), bbox)
    }
}

fn native_paint(mut native: SvgGradient) -> (PaintType, Affine) {
    let mut gradient = match native.kind {
        SvgGradientKind::Linear { start, end } => Gradient::new_linear(start, end),
        SvgGradientKind::Radial {
            start_center,
            start_radius,
            end_center,
            end_radius,
        } => {
            // Vello tests degeneracy in gradient coordinates before applying the transform.
            const DEGENERATE_THRESHOLD: f32 = 1.0e-6;
            let radius = start_radius.max(end_radius);
            let unit = if radius > 0.0 && radius <= DEGENERATE_THRESHOLD {
                f64::from(radius)
            } else {
                1.0
            };
            native.transform *= Affine::scale(unit);
            Gradient::new_two_point_radial(
                (start_center.to_vec2() / unit).to_point(),
                (f64::from(start_radius) / unit) as f32,
                (end_center.to_vec2() / unit).to_point(),
                (f64::from(end_radius) / unit) as f32,
            )
        }
    }
    .with_extend(peniko::Extend::Pad);
    gradient
        .stops
        .extend(native.stops.into_iter().map(|stop| ColorStop {
            offset: stop.offset,
            color: DynamicColor::from_alpha_color(AlphaColor::<Srgb>::new(stop.color)),
        }));

    (PaintType::Gradient(gradient), native.transform)
}

fn texture_paint(encoded: &EncodedShadingPattern, bbox: Rect) -> (PaintType, Affine) {
    let (width, height) = texture_dimensions(bbox, 1.0);
    let mut image = Vec::with_capacity(width as usize * height as usize);
    let mut may_have_transparency = false;
    let (width, height, transform) = encoded.sample_texture(bbox, 1.0, |sample| {
        let pixel = AlphaColor::<Srgb>::new(sample).premultiply().to_rgba8();
        may_have_transparency |= pixel.a != 255;
        image.push(pixel);
    });

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

    (PaintType::Image(image), transform)
}

pub(super) type GradientCache = rustc_hash::FxHashMap<u128, CachedGradient>;

#[derive(Default)]
pub(super) struct CachedGradient {
    pixels: f64,
    shading: std::cell::OnceCell<PreparedShading>,
    strip: std::cell::OnceCell<Option<Arc<Pixmap>>>,
}

const STRIP_WIDTH: usize = PreparedShading::STRIP_SAMPLES / STRIP_ROWS;
const STRIP_ROWS: usize = 16;

fn strip_pixmap(colors: Vec<[f32; 4]>) -> Arc<Pixmap> {
    let w = STRIP_WIDTH + 2;
    let mut pixmap = Pixmap::new(w as u16, STRIP_ROWS as u16);
    let mut transparent = false;
    for (row, pixels) in pixmap.data_mut().chunks_exact_mut(w).enumerate() {
        for (col, pixel) in pixels.iter_mut().enumerate() {
            let index = match col {
                0 => 0,
                c if c == w - 1 => colors.len() - 1,
                c => {
                    1 + (c - 1) * STRIP_ROWS
                        + if c % 2 == 1 {
                            row
                        } else {
                            STRIP_ROWS - 1 - row
                        }
                }
            };
            *pixel = AlphaColor::<Srgb>::new(colors[index])
                .premultiply()
                .to_rgba8();
            transparent |= pixel.a != 255;
        }
    }
    pixmap.set_may_have_transparency(transparent);
    Arc::new(pixmap)
}

fn strip_transform(transform: Affine, bbox: Rect) -> Option<Affine> {
    let [x, _, y, _, offset, _] = transform.as_coeffs();
    let fine = (STRIP_WIDTH * STRIP_ROWS) as f64;
    let period = (2 * STRIP_ROWS) as f64;
    // At pixel centers the transverse term is a whole mirror period, keeping the two axes independent.
    // A horizontal skew of at least one texel selects Vello's f64-stepping NNImagePainter.
    let (dx, dy) = if x.abs() > y.abs() {
        if (x * fine).abs() >= 1. {
            (0., period)
        } else {
            (period.copysign(x), -period.copysign(y))
        }
    } else {
        (period.copysign(x), 0.)
    };
    let width = STRIP_WIDTH as f64;
    let texture = Affine::new([
        x * width,
        x * fine + dx,
        y * width,
        y * fine + dy,
        1. + offset * width,
        offset * fine - (dx + dy) * 0.5,
    ]);
    let bounds = texture.transform_rect_bbox(bbox.inflate(8., 4.));
    // Keep subtexel addressing within the integer precision of Vello's f32 sampler.
    let inverse = texture.inverse();
    (inverse.is_finite()
        && bounds.x0.abs().max(bounds.x1.abs()) * (STRIP_ROWS as f64) < (1 << 22) as f64
        && bounds.y0.abs().max(bounds.y1.abs()) < (1 << 22) as f64)
        .then_some(inverse)
}

fn resolved_gradient(mut gradient: SvgGradient, clamped_child: bool) -> Option<SvgGradient> {
    if lut_resolves(&gradient.stops, NATIVE_GRADIENT_TOLERANCE) {
        return Some(gradient);
    }
    if !clamped_child {
        return None;
    }
    let i = gradient
        .stops
        .windows(2)
        .position(|p| p[0].color == p[1].color && p[1].offset - p[0].offset > 1. / 2047.)?;
    let mut pin = gradient.stops[i].clone();
    pin.offset += 1. / 2047.;
    gradient.stops.insert(i + 1, pin);
    lut_resolves(&gradient.stops, NATIVE_GRADIENT_TOLERANCE).then_some(gradient)
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
    fn folded_strip_samples_pixel_centers_and_pads_both_ends() {
        let count = PreparedShading::STRIP_SAMPLES;
        let color = |i: usize| [(i % 251) as f32 / 255., 0., 0., 1.];
        let colors = (0..count + 2).map(color).collect();
        let strip = strip_pixmap(colors);
        let rect = Rect::new(0., 0., 32., 32.);
        for (x, y) in [(0.043, 0.013), (-0.012, 0.056)] {
            let mapping = Affine::new([x, 0., y, 1., -0.125, 0.]);
            let mut ctx = vello_cpu::RenderContext::new(32, 32);
            ctx.set_paint_transform(strip_transform(mapping, rect).unwrap());
            ctx.set_paint(Image {
                image: ImageSource::Pixmap(strip.clone()),
                sampler: ImageSampler {
                    quality: ImageQuality::Low,
                    y_extend: peniko::Extend::Reflect,
                    ..Default::default()
                },
            });
            ctx.fill_rect(&rect);
            let mut out = Pixmap::new(32, 32);
            ctx.flush();
            ctx.render(&mut out, &mut vello_cpu::Resources::default());
            for row in 0..32 {
                for col in 0..32 {
                    let t = x * (f64::from(col) + 0.5) + y * (f64::from(row) + 0.5) - 0.125;
                    let texel = t * count as f64;
                    let index =
                        |v: f64| ((v.floor() as i64) + 1).clamp(0, count as i64 + 1) as usize;
                    let actual = out.sample(col, row).r;
                    assert!(
                        (index(texel - 0.125)..=index(texel + 0.125))
                            .any(|i| (i % 251) as u8 == actual),
                        "at ({col}, {row}), t={t}"
                    );
                }
            }
        }
    }

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
