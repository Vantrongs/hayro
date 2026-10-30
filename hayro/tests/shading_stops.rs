//! An axial shading pattern paints its function at each pixel's centre, within
//! a level, whether its stops are drawn as a native gradient or the shading is
//! sampled because the gradient's table of colors is too coarse for them.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};

#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

/// The gray of each pixel of the middle row of a 100 × 100 page filled with an
/// axial shading of `function`, object 6, from `x0` to `x1` across it.
fn row((x0, x1): (u8, u8), function: Vec<u8>) -> Vec<u8> {
    let page = fixtures::one_page(
        100,
        "/Contents 4 0 R /Resources << /Pattern << /P 7 0 R >> >>",
        vec![
            fixtures::stream("", b"/Pattern cs /P scn 0 0 100 100 re f"),
            format!(
                "<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [{x0} 0 {x1} 0] \
                 /Extend [true true] /Function 6 0 R >>"
            )
            .into_bytes(),
            function,
            b"<< /PatternType 2 /Shading 5 0 R >>".to_vec(),
        ],
    );
    let pdf = Pdf::new(page).expect("valid pdf");
    let pixmap = render(
        &pdf.pages()[0],
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: 1.0,
            y_scale: 1.0,
            bg_color: WHITE,
        },
    );
    (0..100).map(|x| pixmap.sample(x, 50).r).collect()
}

/// A sampled function of nine samples, one bright: its ring at 3/8 is painted
/// whole; a stitching that steps at 0.4, on both sides of the step; and a line
/// rising 12.75 levels a pixel, drawn natively, at each pixel's centre rather
/// than half a pixel off.
#[test]
fn an_axial_shading_paints_its_function_at_each_pixel() {
    type Exact = fn(f64) -> f64;
    let cases: [(&str, (u8, u8), Vec<u8>, Exact); 3] = [
        (
            "lone sample",
            (0, 100),
            fixtures::stream(
                "/FunctionType 0 /Domain [0 1] /Range [0 1] /Size [9] /BitsPerSample 8 \
                 /Filter /ASCIIHexDecode",
                b"000000FF0000000000>",
            ),
            |t| (1.0 - (t - 0.375).abs() * 8.0).max(0.0),
        ),
        (
            "stitching",
            (0, 100),
            b"<< /FunctionType 3 /Domain [0 1] /Bounds [0.4] /Encode [0 1 0 1] /Functions [\
              << /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [0.25] /N 1 >> \
              << /FunctionType 2 /Domain [0 1] /C0 [0.75] /C1 [1] /N 1 >>] >>"
                .to_vec(),
            |t| {
                if t < 0.4 {
                    t / 0.4 * 0.25
                } else {
                    0.75 + (t - 0.4) / 0.6 * 0.25
                }
            },
        ),
        (
            "steep line",
            (40, 60),
            b"<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>".to_vec(),
            |t| t,
        ),
    ];
    for (name, (x0, x1), function, exact) in cases {
        for (x, got) in row((x0, x1), function).into_iter().enumerate() {
            let t = ((x as f64 + 0.5 - f64::from(x0)) / f64::from(x1 - x0)).clamp(0.0, 1.0);
            let want = (exact(t) * 255.0 + 0.5).floor();
            assert!(
                (f64::from(got) - want).abs() <= 1.0,
                "{name} at {x}: {got}, the function's {want}"
            );
        }
    }
}
