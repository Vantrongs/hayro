//! A shading's `/Background` fills the parts of a shading pattern's shape the
//! shading leaves, and nothing that `sh` paints: the PDF specification applies it
//! "only when the shading is used as part of a shading pattern, not when it is
//! painted directly with the sh operator".

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};

#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

const WHITE_RGB: [u8; 3] = [255, 255, 255];
const GREEN: [u8; 3] = [0, 255, 0];
const RED: [u8; 3] = [255, 0, 0];

/// `/M` is a red triangle mesh with corners (40, 40), (60, 40) and (50, 60), `/A`
/// an axial shading from 40 to 60 across, not extended, and `/P` a shading pattern
/// of `/M`; each has a green background.
fn page(content: &str) -> Vec<u8> {
    let vertex = |x: u16, y: u16| format!("00{x:04X}{y:04X}FF0000");
    let mesh = [vertex(40, 40), vertex(60, 40), vertex(50, 60)].concat() + ">";
    fixtures::one_page(
        100,
        "/Contents 4 0 R /Resources << /Shading << /M 5 0 R /A 6 0 R >> \
         /Pattern << /P 7 0 R >> >>",
        vec![
            fixtures::stream("", content.as_bytes()),
            fixtures::stream(
                "/ShadingType 4 /ColorSpace /DeviceRGB /Background [0 1 0] \
                 /BitsPerCoordinate 16 /BitsPerComponent 8 /BitsPerFlag 8 \
                 /Decode [0 65535 0 65535 0 1 0 1 0 1] /Filter /ASCIIHexDecode",
                mesh.as_bytes(),
            ),
            b"<< /ShadingType 2 /ColorSpace /DeviceRGB /Background [0 1 0] \
              /Coords [40 0 60 0] /Function << /FunctionType 2 /Domain [0 1] \
              /C0 [1 0 0] /C1 [1 0 0] /N 1 >> >>"
                .to_vec(),
            b"<< /PatternType 2 /Shading 5 0 R >>".to_vec(),
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

#[test]
fn sh_paints_no_background() {
    let points = [(50, 45), (10, 10), (90, 90)];
    assert_eq!(
        pixels("/M sh", &points),
        [RED, WHITE_RGB, WHITE_RGB],
        "mesh"
    );
    assert_eq!(
        pixels("/A sh", &[(50, 50), (10, 50), (90, 50)]),
        [RED, WHITE_RGB, WHITE_RGB],
        "axial"
    );
}

#[test]
fn a_shading_patterns_background_fills_its_shape() {
    let fill = "/Pattern cs /P scn 0 0 100 100 re f";
    assert_eq!(
        pixels(fill, &[(50, 45), (10, 10), (90, 90)]),
        [RED, GREEN, GREEN]
    );
}
