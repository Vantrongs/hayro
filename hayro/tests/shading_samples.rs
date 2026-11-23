//! Pixel samples for shading geometry, color functions and mesh identity.
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::{Pixmap, color::palette::css::WHITE};
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};

#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

fn raster(file: Vec<u8>, scale: f32) -> Pixmap {
    let pdf = Pdf::new(file).unwrap();
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

fn gradient(content: &str, shading: &str, function: Vec<u8>, scale: f32) -> Pixmap {
    raster(
        fixtures::one_page(
            100,
            "/Contents 4 0 R /Resources << /Shading << /S 5 0 R >> >>",
            vec![
                fixtures::stream("", content.as_bytes()),
                format!("<< {shading} /ColorSpace /DeviceGray /Function 6 0 R >>").into_bytes(),
                function,
            ],
        ),
        scale,
    )
}

#[test]
fn nine_samples_preserve_the_axial_peak() {
    let image = gradient(
        "/S sh",
        "/ShadingType 2 /Coords [0 0 100 0]",
        fixtures::stream(
            "/FunctionType 0 /Domain [0 1] /Range [0 1] /Size [9] /BitsPerSample 8",
            &[0, 0, 0, 255, 0, 0, 0, 0, 0],
        ),
        1.0,
    );
    for (x, gray) in [(25, 10), (31, 133), (37, 255)] {
        assert!(
            image.sample(x, 50).r.abs_diff(gray) <= 1,
            "{x}: {:?}",
            image.sample(x, 50)
        );
    }
}

#[test]
fn periodic_calculator_radial_preserves_each_sample() {
    let image = gradient(
        "/S sh",
        "/ShadingType 3 /Coords [50 50 0 50 50 100] /Extend [true true]",
        fixtures::stream(
            "/FunctionType 4 /Domain [0 1] /Range [0 1]",
            b"{ 46080 mul sin abs }",
        ),
        2.0,
    );
    for x in 0..200 {
        let radius = (f64::from(x) / 2.0 - 49.75).hypot(0.25);
        let gray = ((radius / 100.0 * 46080.0).to_radians().sin().abs() * 255.0).round() as u8;
        assert!(
            image.sample(x, 100).r.abs_diff(gray) <= 1,
            "{x}: {gray} {:?}",
            image.sample(x, 100)
        );
    }
}

#[test]
fn translating_both_radial_centers_preserves_pixels() {
    let draw = |offset: i32| {
        gradient(
            &format!("1 0 0 1 {} {} cm /S sh", -offset, -offset),
            &format!(
                "/ShadingType 3 /Coords [{offset} {offset} 0 {} {offset} 10.01]",
                offset + 10
            ),
            b"<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>".to_vec(),
            2.0,
        )
    };
    let a = draw(0);
    let b = draw(100000000);
    assert_ne!(a.sample(10, 199).r, 255);
    for y in 0..200 {
        for x in 0..200 {
            assert_eq!(a.sample(x, y), b.sample(x, y));
        }
    }
}

#[test]
fn identical_mesh_dictionaries_keep_distinct_colors_in_both_orders() {
    for kind in [4, 5] {
        let mesh = |x: u8, y: u8, color: [u8; 3]| {
            let (flag, extra) = if kind == 4 {
                (true, "/BitsPerFlag 8")
            } else {
                (false, "/VerticesPerRow 2")
            };
            let mut data = Vec::new();
            for (a, b) in [(x, y), (x + 40, y), (x, y + 40), (x + 40, y + 40)]
                .into_iter()
                .take(if flag { 3 } else { 4 })
            {
                if flag {
                    data.push(0);
                }
                data.extend([a, b]);
                data.extend(color);
            }
            fixtures::stream(
                &format!(
                    "/ShadingType {kind} /ColorSpace /DeviceRGB /BitsPerCoordinate 8 /BitsPerComponent 8 {extra} /Decode [0 255 0 255 0 1 0 1 0 1]"
                ),
                &data,
            )
        };
        for order in ["/A sh /B sh", "/B sh /A sh"] {
            let image = raster(
                fixtures::one_page(
                    100,
                    "/Contents 4 0 R /Resources << /Shading << /A 5 0 R /B 6 0 R >> >>",
                    vec![
                        fixtures::stream("", order.as_bytes()),
                        mesh(0, 0, [255, 0, 0]),
                        mesh(50, 50, [0, 0, 255]),
                    ],
                ),
                1.0,
            );
            for (x, y, color) in [(10, 89, [255, 0, 0]), (60, 39, [0, 0, 255])] {
                let p = image.sample(x, y);
                assert_eq!([p.r, p.g, p.b], color);
            }
        }
    }
}

#[test]
fn far_nonextended_circle_ends_before_the_page() {
    let image = gradient(
        "/S sh",
        "/ShadingType 3 /Coords [-1000000000 50 0 -1000000000 50 1000000000]",
        b"<< /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1 >>".to_vec(),
        2.0,
    );
    for x in 0..200 {
        assert_eq!(image.sample(x, 100).r, 255, "x={x}");
    }
}

#[test]
fn a_large_cone_keeps_the_later_extended_circle() {
    let image = gradient(
        "/S sh",
        "/ShadingType 3 /Coords [0 0 0 1000000.25 0 1000000] /Extend [true true]",
        b"<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>".to_vec(),
        2.0,
    );
    assert_eq!(image.sample(100, 100).r, 255);
}

#[test]
fn tiny_extended_radial_matches_a_unit_circle_at_device_scale() {
    let draw = |radius, scale| {
        let pattern = format!(
            "<< /PatternType 2 /Matrix [{scale} 0 0 {scale} 10.5 50.5] \
             /Shading << /ShadingType 3 /ColorSpace /DeviceRGB /Coords [0 0 0 0 0 {radius}] \
             /Extend [true true] /Function << /FunctionType 2 /Domain [0 1] \
             /C0 [1 0 0] /C1 [0 0 1] /N 1 >> >> >>"
        );
        raster(
            fixtures::one_page(
                100,
                "/Contents 4 0 R /Resources << /Pattern << /P 5 0 R >> >>",
                vec![
                    fixtures::stream("", b"/Pattern cs /P scn 0 0 100 100 re f"),
                    pattern.into_bytes(),
                ],
            ),
            1.,
        )
    };
    let unit = draw("1", "50");
    let tiny = draw(".0000005", "100000000");
    for (x, blue) in [(10, 0), (35, 128), (90, 255)] {
        let reference = unit.sample(x, 49);
        assert!(reference.b.abs_diff(blue) <= 1);
        assert_eq!(tiny.sample(x, 49), reference);
    }
}
