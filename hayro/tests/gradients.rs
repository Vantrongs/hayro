//! Check axial and radial shading colors at device pixel centers.
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::vello_cpu::{Pixmap, RenderContext, Resources};
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};
use kurbo::Affine;

fn raster(data: &[u8], scale: f32) -> Pixmap {
    let pdf = Pdf::new(data.to_vec()).unwrap();
    render(
        &pdf.pages()[0],
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
        },
    )
}

#[test]
fn extended_exponential() {
    let image = raster(include_bytes!("gradients/extended_exponential.pdf"), 1.0);
    for x in 0..100 {
        let expected = (((f64::from(x) + 0.5) / 100.0).powi(2) * 255.0).round() as u8;
        assert!(image.sample(x, 50).r.abs_diff(expected) <= 2);
    }
}

#[test]
fn sampled_axial_peak() {
    let image = raster(include_bytes!("gradients/sampled_peak.pdf"), 1.0);
    for x in 0..100 {
        let gray = (1.0 - ((f64::from(x) + 0.5) / 12.5 - 3.0).abs()).max(0.0);
        assert_eq!(image.sample(x, 50).r, (gray * 255.0).round() as u8, "x={x}");
    }
}

#[test]
fn calculator_radial_rings() {
    let image = raster(include_bytes!("gradients/calculator_rings.pdf"), 2.0);
    for x in 0..200 {
        let radius = (f64::from(x) / 2.0 - 49.75).hypot(0.25);
        let gray = (radius / 100.0 * 46080.0).to_radians().sin().abs();
        assert_eq!(
            image.sample(x, 100).r,
            (gray * 255.0).round() as u8,
            "x={x}"
        );
    }
}

#[test]
fn nonextended_axial_endpoints() {
    let image = raster(include_bytes!("gradients/axial_ends.pdf"), 1.0);
    for (y, last_black) in [(0, 8), (1, 31), (2, 9)] {
        for x in 0..34 {
            let gray = if x <= last_black { 0 } else { 255 };
            assert_eq!(image.sample(x, y).r, gray, "x={x}, y={y}");
        }
    }
}

#[test]
fn transfer_peak_between_linear_gradient_stops() {
    let image = raster(include_bytes!("gradients/transfer_peak.pdf"), 1.0);
    for x in 0..256 {
        let gray = if x == 33 { 255 } else { 0 };
        assert_eq!(image.sample(x, 0).r, gray, "x={x}");
    }
}

#[test]
fn constant_nonextended_endpoints() {
    let shifted = raster(include_bytes!("gradients/constant_shift.pdf"), 1.0);
    assert_eq!(shifted.sample(8, 0).r, 0);
    assert_eq!(shifted.sample(9, 0).r, 255);
    let long = raster(include_bytes!("gradients/constant_long.pdf"), 1.0);
    assert_eq!(long.sample(9999, 0).r, 0);
    assert_eq!(long.sample(10000, 0).r, 255);
}

#[test]
fn continuous_stitch_cusps() {
    for (pdf, t, bound, left, peak) in [
        (
            include_bytes!("gradients/cusp0.pdf").as_slice(),
            0.5 - 0.3934505358338356,
            0.10740133374929428,
            0.14117923,
            0.65686637,
        ),
        (
            include_bytes!("gradients/cusp1.pdf").as_slice(),
            0.5 / 10.22,
            0.04980468,
            0.0,
            61.51 / 255.0,
        ),
    ] {
        let expected = f64::round((left + (peak - left) * (t / bound)) * 255.0) as u8;
        let actual = raster(pdf, 1.0).sample(0, 0).r;
        assert!(
            actual.abs_diff(expected) <= 2,
            "t={t}, expected {expected}, got {actual}"
        );
    }
}

#[test]
fn sampled_gradient_over_piece_budget() {
    for bytes in [
        include_bytes!("gradients/over_budget_gray.pdf").as_slice(),
        include_bytes!("gradients/over_budget_calgray.pdf").as_slice(),
    ] {
        assert_eq!(raster(bytes, 1.0).sample(0, 0).r, 0);
    }
}

#[test]
fn translucent_stitch_matches_calculator() {
    let image = raster(include_bytes!("gradients/translucent.pdf"), 1.0);
    for x in 0..8 {
        let actual = image.sample(x, 1).r;
        let reference = image.sample(x, 0).r;
        assert!(
            actual.abs_diff(reference) <= 2,
            "x={x}: {actual} vs {reference}"
        );
    }
}

#[test]
fn narrow_reversed_encode_matches_affine() {
    for pdf in [
        include_bytes!("gradients/narrow_type0.pdf").as_slice(),
        include_bytes!("gradients/narrow_type3.pdf").as_slice(),
        include_bytes!("gradients/raster_narrow_type0.pdf").as_slice(),
        include_bytes!("gradients/raster_narrow_type3.pdf").as_slice(),
        include_bytes!("gradients/nested_narrow.pdf").as_slice(),
    ] {
        let image = raster(pdf, 1.0);
        for x in 0..64 {
            assert_eq!(image.sample(x, 0), image.sample(x, 1), "x={x}");
        }
    }
}

fn crop(data: &[u8], width: u16, height: u16, transform: Affine) -> Pixmap {
    let pdf = Pdf::new(data.to_vec()).unwrap();
    let mut ctx = RenderContext::new(width, height);
    hayro::render_into(
        &pdf.pages()[0],
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &mut ctx,
        transform,
    );
    ctx.flush();
    let mut image = Pixmap::new(width, height);
    ctx.render(&mut image, &mut Resources::default());
    image
}

#[test]
fn nearly_tangent_circles_cover_the_left_of_the_focus() {
    for (size, shift) in [(400, 0_u16), (160, 120)] {
        let transform =
            Affine::translate((-f64::from(shift), -f64::from(shift))) * Affine::scale(2.);
        let image = crop(
            include_bytes!("gradients/radial_tangent.pdf"),
            size,
            size,
            transform,
        );
        let pixel = image.sample(199 - shift, 199 - shift);
        assert!(pixel.r.abs_diff(26) <= 1 && pixel.a == 255, "{pixel:?}");
    }
}

#[test]
fn radial_strip_boundary_preserves_coverage() {
    let image = crop(
        include_bytes!("gradients/radial_strip_boundary.pdf"),
        400,
        400,
        Affine::IDENTITY,
    );
    let pixel = image.sample(0, 1);
    assert!(pixel.r.abs_diff(26) <= 1 && pixel.a == 255, "{pixel:?}");
}

#[test]
fn wide_separation_strip_tracks_the_parameter() {
    let pdf = include_bytes!("gradients/wide_strip.pdf");
    let image = crop(pdf, 60000, 16, Affine::IDENTITY);
    for y in 0..16 {
        let row = crop(pdf, 60000, 1, Affine::translate((0., -f64::from(y))));
        for x in 0..60000 {
            let expected = row.sample(x, 0);
            let pixel = image.sample(x, y);
            assert!(
                pixel.r.abs_diff(expected.r) <= 2 && pixel.a == expected.a,
                "({x},{y}): {pixel:?}, expected {expected:?}"
            );
        }
    }
}

#[test]
fn nested_focal_covers_the_left_edge() {
    let image = crop(
        include_bytes!("gradients/nested_focal.pdf"),
        1,
        1,
        Affine::IDENTITY,
    );
    assert_eq!(image.sample(0, 0).a, 255);
}

#[test]
fn concentric_annulus_matches_calculator() {
    let image = crop(
        include_bytes!("gradients/concentric_annulus.pdf"),
        600,
        2,
        Affine::IDENTITY,
    );
    for x in 0..600 {
        let reference = image.sample(x, 0);
        assert_eq!(reference.a, 255);
        assert_eq!(image.sample(x, 1), reference, "x={x}");
    }
}
