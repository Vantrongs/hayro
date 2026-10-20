//! A device that draws a filled and stroked path without the knockout group around
//! it, as a Type3 glyph shape and a stencil pattern cell do, draws the stroke over
//! the fill: there is no group whose backdrop a knockout could show, so nothing
//! drawn before is cleared.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};

#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

/// The colour at (`x`, `y`) in PDF space of a 100 pt page with `resources` and
/// `content`, whose further objects are numbered from 5.
fn pixel(resources: &str, content: &str, objects: Vec<Vec<u8>>, (x, y): (u16, u16)) -> [u8; 3] {
    let mut all = vec![fixtures::stream("", content.as_bytes())];
    all.extend(objects);
    let pdf = Pdf::new(fixtures::one_page(
        100,
        &format!("/Contents 4 0 R /Resources << {resources} >>"),
        all,
    ))
    .expect("valid pdf");
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
    let p = pixmap.sample(x, 99 - y);
    [p.r, p.g, p.b]
}

/// Whether `actual` is `expected` but for rounding.
fn near(actual: [u8; 3], expected: [u8; 3]) -> bool {
    actual
        .iter()
        .zip(expected)
        .all(|(&a, e)| a.abs_diff(e) <= 2)
}

/// A shape glyph filled and stroked at alpha 0.5 in red over a green page keeps the
/// page under its stroke: no white, which has blue, shows through there.
#[test]
fn a_type3_shape_glyph_stroke_keeps_the_page_under_it() {
    let glyph = fixtures::stream(
        "",
        b"1000 0 0 0 0 0 1000 1000 d1 100 w 200 200 600 600 re B",
    );
    let font = b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
        /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /a 5 0 R >> \
        /Encoding << /Type /Encoding /Differences [97 /a] >> \
        /FirstChar 97 /LastChar 97 /Widths [1000] >>"
        .to_vec();
    let at_stroke = pixel(
        "/Font << /F 6 0 R >> /ExtGState << /G << /ca 0.5 /CA 0.5 >> >>",
        "0 1 0 rg 0 0 100 100 re f /G gs 1 0 0 rg 1 0 0 RG \
         BT /F 100 Tf 0 0 Td (a) Tj ET",
        vec![glyph, font],
        (20, 50),
    );
    assert_eq!(at_stroke[2], 0, "{at_stroke:?}");
}

/// A stencil pattern cell that fills itself whole and then fills and strokes a
/// square at alpha 0.5 paints its colour whole along the stroke's outer edge too,
/// where the stroke covers half a pixel: the cell's paint is opaque, so only a
/// stroke that cleared the fill under it first would leave that pixel lighter.
#[test]
fn a_stencil_pattern_cell_stroke_keeps_what_the_cell_drew_before() {
    let cell = fixtures::stream(
        "/PatternType 1 /PaintType 2 /TilingType 1 /BBox [0 0 100 100] \
         /XStep 100 /YStep 100 /Resources << /ExtGState << /A << /ca 0.5 /CA 0.5 >> >> >>",
        b"0 0 100 100 re f /A gs 10 w 20.5 20.5 59 59 re B",
    );
    let at_edge = pixel(
        "/ColorSpace << /PR [/Pattern /DeviceRGB] >> /Pattern << /T 5 0 R >>",
        "/PR cs 1 0 0 /T scn 0 0 100 100 re f",
        vec![cell],
        (15, 50),
    );
    assert!(near(at_edge, [255, 0, 0]), "{at_edge:?}");
}
