//! A mesh shading is sampled only at the device pixels drawing it reads, in cells as
//! few as the pixels its triangles may cover: a thin triangle whose box is far
//! larger than the page takes no more memory than one inside it.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};

#[path = "../../hayro-syntax/tests/common/allocations.rs"]
mod allocations;
#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

/// A 200 pt page that paints a free-form triangle mesh of one sliver from the
/// page's upper left corner `size` to the right and down, 3 pt wide at the corner.
fn page(size: f64) -> Vec<u8> {
    let mut mesh = vec![];
    for (x, y, gray) in [(0.0, 0.0, 0_u8), (1.0, 1.0, 128), (0.0, 3.0 / size, 64)] {
        mesh.push(0);
        for v in [x, y] {
            let n = (v * f64::from(u32::MAX)).round() as u32;
            mesh.extend(n.to_be_bytes());
        }
        mesh.push(gray);
    }
    fixtures::one_page(
        200,
        "/Contents 4 0 R /Resources << /Shading << /S 5 0 R >> >>",
        vec![
            fixtures::stream("", format!("{size} 0 0 -{size} 0 200 cm /S sh").as_bytes()),
            fixtures::stream(
                "/ShadingType 4 /ColorSpace /DeviceGray /BitsPerCoordinate 32 \
                 /BitsPerComponent 8 /BitsPerFlag 8 /Decode [0 1 0 1 0 1]",
                &mesh,
            ),
        ],
    )
}

/// The most bytes rendering the page of a sliver scaled by `size` holds at once, and
/// how many of its pixels are not white.
fn render_page(size: f64) -> (usize, usize) {
    let pdf = Pdf::new(page(size)).expect("valid pdf");
    let (pixmap, allocations) = allocations::measure(|| {
        render(
            &pdf.pages()[0],
            &RenderCache::new(),
            &InterpreterSettings::default(),
            &RenderSettings::default(),
            &PixmapSettings {
                x_scale: 1.0,
                y_scale: 1.0,
                bg_color: WHITE,
            },
        )
    });
    let inked = (0..200)
        .flat_map(|y| (0..200).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let p = pixmap.sample(x, y);
            [p.r, p.g, p.b] != [255, 255, 255]
        })
        .count();
    (allocations.peak, inked)
}

#[test]
fn a_thin_triangle_far_larger_than_the_page_is_sampled_in_the_page() {
    let (small, _) = render_page(100.0);
    for size in [1000.0, 10000.0, 65534.0, 1e6] {
        let (peak, inked) = render_page(size);
        // The sliver crosses the page: it is drawn, from samples of the page alone.
        assert!(inked > 0, "{size}: nothing drawn");
        assert!(
            peak <= small + (1 << 20),
            "{size}: {peak} bytes at most, against {small} for a sliver of 100"
        );
    }
}
