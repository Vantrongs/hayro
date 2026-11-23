//! Inherited paints in uncolored Type 3 character procedures.
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::{RenderCache, RenderSettings, render};
#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

#[test]
fn fill_and_stroke_use_their_own_colors() {
    for mode in [0, 1, 2] {
        let pdf=Pdf::new(fixtures::one_page(100, "/Contents 4 0 R /Resources << /Font << /T 5 0 R >> >>",vec![
            fixtures::stream("",format!("1 0 0 rg 0 0 1 RG BT /T 100 Tf {mode} Tr (A) Tj ET").as_bytes()),
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] /FontMatrix [.001 0 0 .001 0 0] /CharProcs << /a 6 0 R >> /Encoding << /Differences [65 /a] >> /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>".to_vec(),
            fixtures::stream("", b"1000 0 0 0 1000 1000 d1 100 w 200 200 600 600 re B")
        ])).unwrap();
        let image = render(
            &pdf.pages()[0],
            &RenderCache::new(),
            &InterpreterSettings::default(),
            &RenderSettings::default(),
            &hayro::PixmapSettings::default(),
        );
        for (x, expected) in [(50, [255, 0, 0]), (20, [0, 0, 255])] {
            let p = image.sample(x, 50);
            assert_eq!([p.r, p.g, p.b], expected, "Tr {mode} at x={x}");
        }
    }
}

#[test]
fn translucent_fill_and_stroke_do_not_bleed_into_each_other() {
    let pdf = Pdf::new(fixtures::one_page(
        100,
        "/Contents 4 0 R /Resources << /Font << /T 5 0 R >> /ExtGState << /G << /ca 0.5 /CA 0.5 >> >> >>",
        vec![
            fixtures::stream("", b"/G gs 1 0 0 rg 0 0 1 RG BT /T 100 Tf (A) Tj ET"),
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] /FontMatrix [.001 0 0 .001 0 0] /CharProcs << /a 6 0 R >> /Encoding << /Differences [65 /a] >> /FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>".to_vec(),
            fixtures::stream("", b"1000 0 0 0 1000 1000 d1 100 w 200 200 600 600 re B"),
        ],
    ))
    .unwrap();
    let image = render(
        &pdf.pages()[0],
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &hayro::PixmapSettings {
            bg_color: vello_cpu::color::palette::css::WHITE,
            ..hayro::PixmapSettings::default()
        },
    );
    // Stroke covers x in [15, 25]; fill covers x in [20, 80].
    // Both outer (x=17) and inner (x=23) halves of the stroke must be pure half-opacity blue.
    for (x, expected) in [
        (17, [128, 128, 255]),
        (23, [128, 128, 255]),
        (50, [255, 128, 128]),
    ] {
        let p = image.sample(x, 50);
        for (a, e) in [p.r, p.g, p.b].into_iter().zip(expected) {
            assert!(a.abs_diff(e) <= 1, "at x={x}: got {:?}, expected {expected:?}", [p.r, p.g, p.b]);
        }
    }
}
