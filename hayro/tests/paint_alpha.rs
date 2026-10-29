//! Each paint is drawn at the alpha of the graphics state it paints in, once, as
//! viewers draw it: a solid colour, a shading pattern, and a stencil or coloured
//! tiling pattern alike, whether a path is filled, or filled and stroked by one
//! operator, which viewers paint as a knockout group. The expected colours are those
//! a common viewer renders for the same pages.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};

#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

/// `/G` draws at alpha 0.5, `/H` at 1, `/K` strokes at 0.5 and fills at 1, `/M` in a
/// soft mask of 50 % grey, `/X` at alpha 0.5 with Multiply, `/Y` at 1 with Multiply. `/S` is a red shading,
/// `/T` a stencil and `/C` a red coloured tiling pattern, 10 pt cells filled whole;
/// `/O` and `/Q` are the same with cells of two overlapping squares; `/B` is a blue
/// coloured tiling pattern whose cells paint at alpha 0.5.
fn page(content: &str) -> Vec<u8> {
    let tiling = |paint_type: u8, cell: &str| {
        fixtures::stream(
            &format!(
                "/PatternType 1 /PaintType {paint_type} /TilingType 1 /BBox [0 0 10 10] \
                 /XStep 10 /YStep 10 /Resources << >>"
            ),
            cell.as_bytes(),
        )
    };
    let resources = "/Resources << /ExtGState << /G << /ca 0.5 /CA 0.5 >> /H << /ca 1 /CA 1 >> \
        /K << /ca 1 /CA 0.5 >> /M << /SMask << /S /Luminosity /G 8 0 R >> >> \
        /X << /ca 0.5 /CA 0.5 /BM /Multiply >> /Y << /BM /Multiply >> >> \
        /ColorSpace << /PR [/Pattern /DeviceRGB] >> \
        /Pattern << /S 5 0 R /T 6 0 R /C 7 0 R /O 9 0 R /Q 10 0 R /B 11 0 R >> >>";
    fixtures::one_page(
        100,
        &format!("/Contents 4 0 R {resources}"),
        vec![
            fixtures::stream("", content.as_bytes()),
            b"<< /PatternType 2 /Shading << /ShadingType 2 /ColorSpace /DeviceRGB \
              /Coords [0 0 100 0] /Function << /FunctionType 2 /Domain [0 1] \
              /C0 [1 0 0] /C1 [1 0 0] /N 1 >> /Extend [true true] >> >>"
                .to_vec(),
            tiling(2, "0 0 10 10 re f"),
            tiling(1, "1 0 0 rg 0 0 10 10 re f"),
            fixtures::stream(
                "/Type /XObject /Subtype /Form /Group << /S /Transparency /CS /DeviceRGB >> \
                 /BBox [0 0 100 100] /Resources << >>",
                b"0.5 g 0 0 100 100 re f",
            ),
            tiling(2, "0 0 8 8 re f 2 2 8 8 re f"),
            tiling(1, "1 0 0 rg 0 0 8 8 re f 2 2 8 8 re f"),
            fixtures::stream(
                "/PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] \
                 /XStep 10 /YStep 10 /Resources << /ExtGState << /A << /ca 0.5 >> >> >>",
                b"/A gs 0 0 1 rg 0 0 10 10 re f",
            ),
        ],
    )
}

/// The colours of the pixels at `points` (in PDF space) of the page drawing
/// `content` on white.
fn pixels(content: &str, points: &[(u16, u16)]) -> Vec<[u8; 3]> {
    let pdf = Pdf::new(page(content)).expect("valid pdf");
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
    points
        .iter()
        .map(|&(x, y)| {
            let p = pixmap.sample(x, 99 - y);
            [p.r, p.g, p.b]
        })
        .collect()
}

/// Whether `actual` is `expected` but for rounding.
fn near(actual: [u8; 3], expected: [u8; 3]) -> bool {
    actual
        .iter()
        .zip(expected)
        .all(|(&a, e)| a.abs_diff(e) <= 2)
}

const HALF_RED: [u8; 3] = [255, 128, 128];
const RED: [u8; 3] = [255, 0, 0];

const FILLS: [(&str, &str); 4] = [
    ("solid", "1 0 0 rg"),
    ("shading", "/Pattern cs /S scn"),
    ("stencil", "/PR cs 1 0 0 /T scn"),
    ("coloured", "/Pattern cs /C scn"),
];

/// Inside a square filled, or filled and stroked in blue, at alpha 0.5 or in a soft
/// mask of 50 %, the fill shows at half its colour, whatever paints it.
#[test]
fn a_fill_is_drawn_at_its_alpha_once_alone_or_stroked() {
    for gs in ["/G gs", "/M gs"] {
        for (paint, fill) in FILLS {
            for op in ["f", "b", "B"] {
                let content = format!("{gs} 10 w 0 0 1 RG {fill} 25 25 50 50 re {op}");
                let [inside] = pixels(&content, &[(50, 50)])[..] else {
                    unreachable!()
                };
                assert!(near(inside, HALF_RED), "{gs} {paint} {op}: {inside:?}");
            }
        }
    }
}

/// Opaque, where a stroke's antialiased edge lies over its own fill, the pair is as
/// opaque as either: a square filled and stroked in red off the pixel grid, with
/// Multiply or in a soft mask of 50 %, shows red or red at half on every pixel the
/// pair covers whole, as a common viewer draws it (`PyMuPDF` 1.28.2), where
/// translucent its stroke's edge shows lighter (see the test below).
#[test]
fn an_opaque_stroke_edge_over_its_fill_is_whole() {
    for (gs, expected) in [("/Y gs", RED), ("/M gs", HALF_RED)] {
        for op in ["b", "B"] {
            let content = format!("{gs} 3 w 1 0 0 RG 1 0 0 rg 25.4 25.4 50 50 re {op}");
            // The stroke covers x from 23.9 to 26.9 and 73.9 to 76.9, the fill the rest.
            let row: Vec<_> = (24..76).map(|x| (x, 50)).collect();
            for (&(x, _), pixel) in row.iter().zip(pixels(&content, &row)) {
                assert!(near(pixel, expected), "{content} at x {x}: {pixel:?}");
            }
        }
    }
}

/// A pattern paints at the alpha of the graphics state when it paints, not when it
/// was selected.
#[test]
fn a_pattern_paints_at_the_alpha_it_paints_in() {
    for (paint, fill) in FILLS {
        for (content, expected) in [
            (format!("/G gs {fill} /H gs 25 25 50 50 re f"), RED),
            (format!("/H gs {fill} /G gs 25 25 50 50 re f"), HALF_RED),
        ] {
            let [inside] = pixels(&content, &[(50, 50)])[..] else {
                unreachable!()
            };
            assert!(near(inside, expected), "{paint}: {content}: {inside:?}");
        }
    }
    // A stroke at its own alpha.
    for stroke in ["/Pattern CS /S SCN", "/PR CS 1 0 0 /T SCN"] {
        let content = format!("/K gs {stroke} 30 w 25 50 m 75 50 l S");
        let [on] = pixels(&content, &[(50, 50)])[..] else {
            unreachable!()
        };
        assert!(near(on, HALF_RED), "{content}: {on:?}");
    }
}

/// A tiling pattern's cell is painted at the alpha as a whole: where its shapes
/// overlap, it is no darker.
#[test]
fn a_tiling_pattern_cell_is_translucent_as_a_whole() {
    for fill in ["/PR cs 1 0 0 /O scn", "/Pattern cs /Q scn"] {
        let content = format!("/G gs {fill} 0 0 100 100 re f");
        for (point, colour) in [(55, 55), (51, 51), (59, 59)]
            .into_iter()
            .zip(pixels(&content, &[(55, 55), (51, 51), (59, 59)]))
        {
            assert!(near(colour, HALF_RED), "{fill} at {point:?}: {colour:?}");
        }
    }
}

/// Filled red and stroked 10 pt wide with a pattern whose cells paint blue at alpha
/// 0.5, by one operator: translucent or blended, where the stroke covers the fill
/// only the stroke shows; opaque and unblended, in a soft mask or not, the stroke is
/// drawn over the fill (the colours a common viewer renders, at the overlap, the
/// stroke outside the fill, the fill).
#[test]
fn a_translucent_stroke_paint_knocks_out_the_fill() {
    let points = [(27, 50), (22, 50), (50, 50)];
    for (gs, expected) in [
        ("/G gs", [[192, 192, 255], [192, 192, 255], HALF_RED]),
        ("/X gs", [[193, 193, 255], [193, 193, 255], [255, 129, 129]]),
        ("/Y gs", [[129, 129, 255], [129, 129, 255], RED]),
        ("/M gs", [[192, 128, 191], [192, 192, 255], HALF_RED]),
        ("/H gs", [[129, 0, 126], [129, 129, 255], RED]),
    ] {
        for op in ["b", "B"] {
            let content = format!("{gs} 10 w 1 0 0 rg /Pattern CS /B SCN 25 25 50 50 re {op}");
            let got = pixels(&content, &points);
            for ((point, got), expected) in points.iter().zip(got).zip(expected) {
                assert!(near(got, expected), "{content} at {point:?}: {got:?}");
            }
        }
    }
}

/// Filled and stroked in red by one operator at alpha 0.5, the stroke's inner edge
/// halfway across a pixel: the fill is knocked out by the stroke's coverage and the
/// stroke then drawn over it, so the pixel is lighter than the shape's inside, as a
/// common viewer draws it (`PyMuPDF` 1.28.2: 255, 161, 161 there, 255, 129, 129
/// inside), rather than even with the inside, as a knockout group's compositing by
/// shape would make it (PDF 32000-1 11.4.6.2).
#[test]
fn a_knocked_out_fill_shows_lighter_at_the_stroke_edge_as_viewers_draw_it() {
    let content = "/G gs 10 w 1 0 0 RG 1 0 0 rg 25.5 25.5 50 50 re B";
    let [edge, inside] = pixels(content, &[(30, 50), (50, 50)])[..] else {
        unreachable!()
    };
    assert!(near(edge, [255, 161, 161]), "{edge:?}");
    assert!(near(inside, HALF_RED), "{inside:?}");
}
