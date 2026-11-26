//! Raster coverage and allocation checks for images and annotation appearances.
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};

#[path = "../../hayro-syntax/tests/common/allocations.rs"]
mod allocations;
#[path = "../../hayro-interpret/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;
#[path = "../../hayro-interpret/tests/common/rendering.rs"]
mod rendering;

fn raster(data: Vec<u8>) -> vello_cpu::Pixmap {
    let pdf = Pdf::new(data).unwrap();
    render(
        &pdf.pages()[0],
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings::default(),
    )
}

#[test]
fn virtual_tail_is_green_at_constant_cost() {
    let short = raster(rendering::short_image(2));
    assert_eq!(&short.data_as_u8_slice()[..4], &[255, 0, 0, 255]);
    let (large, cost) = allocations::measure(|| raster(rendering::short_image(1_000_000)));
    let p = large.sample(16, 16);
    assert_eq!([p.r, p.g, p.b, p.a], [0, 255, 0, 255]);
    assert!(cost.peak < 2_000_000, "{cost:?}");
}

#[test]
fn annotation_group_controls_the_appearance() {
    for on in [true, false] {
        let p = raster(rendering::group_annotation(on)).sample(16, 16);
        assert_eq!([p.r, p.g, p.b, p.a] == [0, 255, 0, 255], on);
    }
}

#[test]
fn independently_short_color_and_soft_mask_match_padded_pixels() {
    let page = |color: &[u8], alpha: &[u8]| {
        raster(fixtures::one_page(
            32,
            "/Contents 4 0 R /Resources << /XObject << /I 5 0 R >> >>",
            vec![
                fixtures::stream("", b"32 0 0 32 0 0 cm /I Do"),
                fixtures::stream(
                    "/Subtype /Image /Width 1 /Height 4 /ColorSpace /DeviceGray /BitsPerComponent 8 /Decode [1 0] /SMask 6 0 R",
                    color,
                ),
                fixtures::stream(
                    "/Subtype /Image /Width 1 /Height 4 /ColorSpace /DeviceGray /BitsPerComponent 8 /Decode [1 0]",
                    alpha,
                ),
            ],
        ))
    };
    let short = page(&[220], &[0, 192]);
    let dense = page(&[220, 0, 0, 0], &[0, 192, 0, 0]);
    for y in [4, 12, 20, 28] {
        assert_eq!(short.sample(16, y), dense.sample(16, y));
    }
}
