use crate::harness::assets::FALLBACK_FONTS;
use crate::harness::manifest::entries;
use crate::harness::runner::{Outcome, Test};
use crate::harness::snapshot::{Suite, check};
use crate::{interpreter_settings, render_svg, svg_render_settings, try_load_pdf};

pub(crate) fn tests() -> Vec<Test> {
    entries()
        .iter()
        .filter(|entry| entry.svg && !entry.ignore)
        .map(|entry| {
            let mut files = vec![entry.file.clone()];
            files.extend(FALLBACK_FONTS.map(String::from));

            Test::new(format!("svg::{}", entry.name), files, move || {
                let pdf = match try_load_pdf(&entry.file, &entry.password) {
                    Ok(pdf) => pdf,
                    Err(err) => return Outcome::Failed(err),
                };
                let pages = render_svg(
                    &pdf,
                    &entry.name,
                    interpreter_settings(),
                    svg_render_settings(),
                    entry.range.clone(),
                );
                check(Suite::Svg, &entry.name, pages, entry.max_diff_pixels)
            })
        })
        .chain([
            Test::unit("svg::sampled_peak_gradient", sampled_peak),
            Test::unit("svg::gradient_sample_budget", sample_budget),
            Test::unit("svg::gradient_affine_domain", affine_domain),
            Test::unit("svg::gradient_transfer_pair", transfer_pair),
            Test::unit("svg::gradient_offpage", offpage),
            Test::unit("svg::collapsed_band", collapsed_band),
            Test::unit("svg::close_stops", close_stops),
            Test::unit("svg::stitched_band", stitched_band),
            Test::unit("svg::narrow_exponential", narrow_exponential),
            Test::unit("svg::gradient_background_order", background_order),
            Test::unit("svg::gradient_fractional_edge", fractional_edge),
            Test::unit("svg::gradient_subpixel_mask", subpixel_mask),
            Test::unit("svg::gradient_narrow_encode", narrow_encode),
        ])
        .collect()
}

fn gradient(bytes: &[u8]) -> (String, resvg::tiny_skia::Pixmap) {
    scaled_gradient(bytes, 1)
}

fn scaled_gradient(bytes: &[u8], scale: u32) -> (String, resvg::tiny_skia::Pixmap) {
    let pdf = hayro_syntax::Pdf::new(bytes.to_vec()).unwrap();
    let svg = hayro_svg::convert(
        &pdf.pages()[0],
        &Default::default(),
        &Default::default(),
        &Default::default(),
    );
    let tree = resvg::usvg::Tree::from_str(&svg, &Default::default()).unwrap();
    let size = tree.size().to_int_size();
    let mut image =
        resvg::tiny_skia::Pixmap::new(size.width() * scale, size.height() * scale).unwrap();
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale as f32, scale as f32),
        &mut image.as_mut(),
    );
    (svg, image)
}

fn sample_budget() {
    let (svg, image) = gradient(include_bytes!(
        "../../hayro/tests/gradients/constant_samples.pdf"
    ));
    assert_eq!(svg.matches("<stop ").count(), 2);
    assert_eq!(image.pixel(0, 0).unwrap().red(), 0);
    let (svg, _) = gradient(include_bytes!(
        "../../hayro/tests/gradients/subset_samples.pdf"
    ));
    assert!(svg.contains("<linearGradient"));
    assert!(!svg.contains("<image"));
    for bytes in [
        include_bytes!("../../hayro/tests/gradients/over_budget_gray.pdf").as_slice(),
        include_bytes!("../../hayro/tests/gradients/over_budget_calgray.pdf").as_slice(),
    ] {
        let (svg, image) = gradient(bytes);
        assert!(svg.contains("<image"));
        assert!(!svg.contains("<stop "));
        let pixel = image.pixel(0, 0).unwrap();
        assert_eq!((pixel.red(), pixel.alpha()), (0, 255));
    }
}

fn affine_domain() {
    let (svg, image) = gradient(include_bytes!(
        "../../hayro/tests/gradients/affine_domain.pdf"
    ));
    assert!(svg.contains("<linearGradient"));
    assert!(!svg.contains("<image"));
    for x in 0..256 {
        assert!(image.pixel(x, 0).unwrap().red().abs_diff(x as u8) <= 1);
    }
}

fn transfer_pair() {
    for (bytes, spike_last) in [
        (
            include_bytes!("../../hayro/tests/gradients/transfer_pair_identity.pdf").as_slice(),
            true,
        ),
        (
            include_bytes!("../../hayro/tests/gradients/transfer_pair_spike.pdf").as_slice(),
            false,
        ),
    ] {
        let (_, image) = gradient(bytes);
        for x in 0..256 {
            let pixel = image.pixel(x, 0).unwrap();
            assert_eq!(pixel.alpha(), 255);
            if spike_last {
                assert_eq!(pixel.red(), if x == 33 { 255 } else { 0 }, "x={x}");
            } else {
                assert!(pixel.red().abs_diff(x as u8) <= 1, "x={x}");
            }
        }
    }
}

fn narrow_exponential() {
    for (pdf, expected, extended) in [
        (
            include_bytes!("../../hayro/tests/gradients/clamped_linear.pdf").as_slice(),
            128,
            false,
        ),
        (
            include_bytes!("../../hayro/tests/gradients/crossing_exponential.pdf").as_slice(),
            16,
            false,
        ),
        (
            include_bytes!("../../hayro/tests/gradients/narrow_exponential.pdf").as_slice(),
            100,
            false,
        ),
        (
            include_bytes!("../../hayro/tests/gradients/array_exponential.pdf").as_slice(),
            100,
            false,
        ),
        (
            include_bytes!("../../hayro/tests/gradients/rgb_exponential_false.pdf").as_slice(),
            100,
            false,
        ),
        (
            include_bytes!("../../hayro/tests/gradients/rgb_exponential_true.pdf").as_slice(),
            100,
            true,
        ),
        (
            include_bytes!("../../hayro/tests/gradients/negative_exponential.pdf").as_slice(),
            36,
            false,
        ),
    ] {
        let (svg, image) = scaled_gradient(pdf, 4);
        assert!(svg.contains("<linearGradient"));
        assert!(!svg.contains("<image"));
        for y in 0..image.height() {
            let p = image.pixel(0, y).unwrap();
            assert_eq!(
                (p.red(), p.green(), p.blue(), p.alpha()),
                (expected, expected, expected, 255)
            );
            assert_eq!(
                image.pixel(1, y).unwrap().alpha(),
                if extended { 255 } else { 0 }
            );
        }
    }
}

fn collapsed_band() {
    let (svg, image) = gradient(include_bytes!(
        "../../hayro/tests/gradients/collapsed_band.pdf"
    ));
    assert!(svg.contains("<image"));
    for (x, gray) in [0, 255, 0].into_iter().enumerate() {
        let p = image.pixel(x as u32, 0).unwrap();
        assert_eq!(
            (p.red(), p.green(), p.blue(), p.alpha()),
            (gray, gray, gray, 255)
        );
    }
}

fn stitched_band() {
    let (svg, image) = scaled_gradient(
        include_bytes!("../../hayro/tests/gradients/stitched_band.pdf"),
        4,
    );
    assert!(svg.contains("<linearGradient"));
    let p = image.pixel(0, 8).unwrap();
    assert_eq!(
        (p.red(), p.green(), p.blue(), p.alpha()),
        (255, 255, 255, 255)
    );
    assert_eq!(image.pixel(1, 8).unwrap().red(), 0);
    assert_eq!(image.pixel(8, 8).unwrap().alpha(), 0);
}

fn offpage() {
    let (svg, _) = gradient(include_bytes!(
        "../../hayro/tests/gradients/extended_exponential.pdf"
    ));
    assert!(svg.contains("<linearGradient"));
    assert!(!svg.contains("<image"));
    let (svg, image) = gradient(include_bytes!("../../hayro/tests/gradients/offpage.pdf"));
    let document = resvg::usvg::roxmltree::Document::parse(&svg).unwrap();
    let mut images = document
        .descendants()
        .filter(|node| node.has_tag_name("image"));
    let texture = images.next().unwrap();
    assert_eq!(texture.attribute("width"), Some("102"));
    assert_eq!(texture.attribute("height"), Some("102"));
    assert!(images.next().is_none());
    for y in 0..100 {
        for x in 0..100 {
            let expected = (((f64::from(x) + 0.5) / 100.0).powi(2) * 255.0).round() as u8;
            let pixel = image.pixel(x, y).unwrap();
            assert_eq!(pixel.alpha(), 255);
            assert!(
                pixel.red().abs_diff(expected) <= 1,
                "({x},{y}): {pixel:?} vs {expected}"
            );
        }
    }
}

fn background_order() {
    for (pdf, expected) in [
        (
            include_bytes!("../../hayro/tests/gradients/background_pattern_first.pdf").as_slice(),
            0,
        ),
        (
            include_bytes!("../../hayro/tests/gradients/background_sh_first.pdf").as_slice(),
            255,
        ),
    ] {
        let (_, image) = gradient(pdf);
        for x in [0, 7] {
            let p = image.pixel(x, 2).unwrap();
            assert_eq!((p.red(), p.alpha()), (expected, 255));
        }
    }
}

fn fractional_edge() {
    let (_, image) = scaled_gradient(
        include_bytes!("../../hayro/tests/gradients/fractional_edge.pdf"),
        4,
    );
    let p = image.pixel(0, 8).unwrap();
    assert!((1..64).contains(&p.red()), "{p:?}");
    assert_eq!(p.alpha(), 255);
    assert_eq!(image.pixel(2, 8).unwrap().red(), 0);
}

fn subpixel_mask() {
    let (_, image) = gradient(include_bytes!(
        "../../hayro/tests/gradients/subpixel_mask.pdf"
    ));
    let p = image.pixel(0, 2).unwrap();
    assert!((1..255).contains(&p.alpha()), "{p:?}");
    assert_eq!(image.pixel(1, 2).unwrap().alpha(), 0);
}

fn narrow_encode() {
    for pdf in [
        include_bytes!("../../hayro/tests/gradients/narrow_type0.pdf").as_slice(),
        include_bytes!("../../hayro/tests/gradients/narrow_type3.pdf").as_slice(),
        include_bytes!("../../hayro/tests/gradients/nested_narrow.pdf").as_slice(),
    ] {
        let (_, image) = gradient(pdf);
        for x in 0..64 {
            assert_eq!(image.pixel(x, 0), image.pixel(x, 1), "x={x}");
        }
    }
}

fn sampled_peak() {
    let (svg, image) = gradient(include_bytes!(
        "../../hayro/tests/gradients/sampled_peak.pdf"
    ));
    assert!(svg.contains("<linearGradient"));
    assert!(!svg.contains("<image"));
    for x in 0..100 {
        let gray = (1.0 - ((f64::from(x) + 0.5) / 12.5 - 3.0).abs()).max(0.0);
        let expected = (gray * 255.0).round() as u8;
        let pixel = image.pixel(x, 50).unwrap();
        assert_eq!(pixel.alpha(), 255);
        for channel in [pixel.red(), pixel.green(), pixel.blue()] {
            assert!(
                channel.abs_diff(expected) <= 1,
                "x={x}: {channel} vs {expected}"
            );
        }
    }
}

fn close_stops() {
    for (pdf, band) in [
        (
            include_bytes!("../../hayro/tests/gradients/close_red.pdf").as_slice(),
            [255, 0, 0],
        ),
        (
            include_bytes!("../../hayro/tests/gradients/close_white.pdf").as_slice(),
            [255, 255, 255],
        ),
    ] {
        let (svg, image) = gradient(pdf);
        for x in 0..10 {
            let p = image.pixel(x, 0).unwrap();
            let expected = if (2..7).contains(&x) { band } else { [0; 3] };
            assert_eq!([p.red(), p.green(), p.blue()], expected, "x={x}");
            assert_eq!(p.alpha(), 255);
        }
        assert!(svg.contains("<image"));
    }
    for pdf in [
        include_bytes!("../../hayro/tests/gradients/close_padding.pdf").as_slice(),
        include_bytes!("../../hayro/tests/gradients/close_end_padding.pdf").as_slice(),
    ] {
        let (svg, image) = gradient(pdf);
        assert_eq!(image.pixel(3, 4).unwrap().alpha(), 0);
        assert_eq!(image.pixel(6, 4).unwrap().alpha(), 255);
        assert!(svg.contains("<image"));
    }
}
