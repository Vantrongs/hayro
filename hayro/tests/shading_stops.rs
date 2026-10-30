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
    /// A name, the span of the axis in pixels, the function and its value.
    type Case = (&'static str, (u8, u8), Vec<u8>, fn(f64) -> f64);
    let cases: [Case; 3] = [
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

/// A stitching of `pieces` lines rising from 0 to 1 and falling back in turn.
fn zigzag(pieces: usize) -> (Vec<u8>, impl Fn(f64) -> f64) {
    let bounds: Vec<String> = (1..pieces)
        .map(|i| format!("{}", i as f64 / pieces as f64))
        .collect();
    let functions: Vec<&str> = (0..pieces)
        .map(|i| {
            if i % 2 == 0 {
                "<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>"
            } else {
                "<< /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1 >>"
            }
        })
        .collect();
    let function = format!(
        "<< /FunctionType 3 /Domain [0 1] /Bounds [{}] /Encode [{}] /Functions [{}] >>",
        bounds.join(" "),
        vec!["0 1"; pieces].join(" "),
        functions.join(" ")
    );
    let exact = move |t: f64| {
        let at = (t * pieces as f64).min(pieces as f64 - 1e-9);
        let (i, f) = (at.floor() as usize, at.fract());
        if i % 2 == 0 { f } else { 1.0 - f }
    };
    (function.into_bytes(), exact)
}

/// An axial shading whose colors zigzag 40 times across 1600 pixels, 6.4
/// levels a pixel, too steeply for a table of 1024 colors over its length:
/// along the page, rotated by 30 degrees and sheared, it paints its function
/// at each pixel's centre within a level, as a texture along its line.
#[test]
fn a_steep_axial_shading_paints_its_function_at_each_pixel_at_any_angle() {
    const SIZE: u32 = 1600;
    let (function, exact) = zigzag(40);
    let (c, s) = (30_f64.to_radians().cos(), 30_f64.to_radians().sin());
    for (name, m) in [
        ("along", [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
        ("rotated", [c, s, -s, c, 400.0, 0.0]),
        ("sheared", [1.0, 0.0, 0.5, 1.0, 0.0, 0.0]),
    ] {
        let page = fixtures::one_page(
            SIZE,
            "/Contents 4 0 R /Resources << /Pattern << /P 7 0 R >> >>",
            vec![
                fixtures::stream(
                    "",
                    format!("/Pattern cs /P scn 0 0 {SIZE} {SIZE} re f").as_bytes(),
                ),
                format!(
                    "<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 {SIZE} 0] \
                     /Extend [true true] /Function 6 0 R >>"
                )
                .into_bytes(),
                function.clone(),
                format!(
                    "<< /PatternType 2 /Shading 5 0 R /Matrix [{} {} {} {} {} {}] >>",
                    m[0], m[1], m[2], m[3], m[4], m[5]
                )
                .into_bytes(),
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
        let det = m[0] * m[3] - m[1] * m[2];
        let mut compared = 0;
        for y in [10_u16, 800, 1590] {
            for x in 0..SIZE as u16 {
                // The page's y is up, from the bottom; the pattern's point there.
                let (px, py) = (
                    f64::from(x) + 0.5 - m[4],
                    f64::from(SIZE) - f64::from(y) - 0.5 - m[5],
                );
                let qx = (m[3] * px - m[2] * py) / det;
                let t = (qx / f64::from(SIZE)).clamp(0.0, 1.0);
                let want = (exact(t) * 255.0 + 0.5).floor();
                let got = f64::from(pixmap.sample(x, y).r);
                assert!(
                    (got - want).abs() <= 1.0,
                    "{name} at ({x}, {y}): {got}, the function's {want}"
                );
                compared += 1;
            }
        }
        assert_eq!(compared, 3 * SIZE as usize);
    }
}

/// The pixmap of a `size` × `size` page that fills `fill` with a shading
/// pattern of `shading`, object 5, whose function is `function`, object 6,
/// placed by `matrix`.
fn painted(
    size: u32,
    fill: &str,
    shading: &str,
    function: &str,
    matrix: &str,
) -> vello_cpu::Pixmap {
    let page = fixtures::one_page(
        size,
        "/Contents 4 0 R /Resources << /Pattern << /P 7 0 R >> >>",
        vec![
            fixtures::stream("", format!("/Pattern cs /P scn {fill} re f").as_bytes()),
            shading.as_bytes().to_vec(),
            function.as_bytes().to_vec(),
            format!("<< /PatternType 2 /Shading 5 0 R /Matrix [{matrix}] >>").into_bytes(),
        ],
    );
    let pdf = Pdf::new(page).expect("valid pdf");
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
}

/// An axial shading whose function ramps from black to white over the first
/// `ramp` of its parameter and stays white past it, along a line of `80 /
/// ramp` units, paints its function at each pixel of a shape reaching 70
/// pixels along it: the gradient's ends lie `ramp` apart in the shading's
/// parameter, around and below the distance under which vello takes a
/// gradient's ends for one point and paints its first color alone, but its
/// colors span 80 pixels.
#[test]
fn an_axial_shading_whose_stops_lie_close_paints_its_ramp() {
    for ramp in [8e-7, 5e-7, 9.99e-7, 1e-6, 1.01e-6, 2e-6, 1e-3] {
        let length = 80.0 / ramp;
        let pixmap = painted(
            100,
            "0 0 70 100",
            &format!(
                "<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 {length} 0] \
                 /Extend [true true] /Function 6 0 R >>"
            ),
            &format!(
                "<< /FunctionType 3 /Domain [0 1] /Bounds [{ramp}] /Encode [0 1 0 1] /Functions [\
                 << /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >> \
                 << /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [1] /N 1 >>] >>"
            ),
            "1 0 0 1 0 0",
        );
        for x in 1..69_u16 {
            let want = ((f64::from(x) + 0.5) / 80.0 * 255.0 + 0.5).floor();
            let got = f64::from(pixmap.sample(x, 50).r);
            assert!(
                (got - want).abs() <= 1.0,
                "ramp {ramp} at {x}: {got}, the function's {want}"
            );
        }
    }
}

/// Radial circles written ten million times smaller under a matrix ten million
/// times larger are the same circles, a cone, and paint the same pixels: the
/// precision of their coordinates read relatively to their size, as well as
/// absolutely, tells both from touching.
#[test]
fn radial_circles_scaled_against_their_matrix_paint_alike() {
    let function = "<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>";
    let paint = |coords: &str, matrix: &str| {
        painted(
            200,
            "0 0 200 200",
            &format!(
                "<< /ShadingType 3 /ColorSpace /DeviceGray /Coords [{coords}] \
                 /Extend [true true] /Function 6 0 R >>"
            ),
            function,
            matrix,
        )
    };
    let written = paint("0 0 0 200 0 100", "1 0 0 1 0 0");
    let scaled = paint("0 0 0 .00002 0 .00001", "10000000 0 0 10000000 0 0");
    let mut apart = 0;
    for y in 0..200 {
        for x in 0..200 {
            let (a, b) = (written.sample(x, y), scaled.sample(x, y));
            if [a.r, a.g, a.b, a.a] != [b.r, b.g, b.b, b.a] {
                apart += 1;
            }
        }
    }
    assert_eq!(apart, 0);
    // The original cone leaves the page's centre (100, 100), outside it, unpainted.
    assert_eq!(written.sample(100, 100).r, 255);
}
