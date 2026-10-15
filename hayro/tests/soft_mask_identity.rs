//! Soft masks are told apart by all they are made of, not by the group they draw.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};

#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

/// The colours at (25, 50) and (75, 50) of a 100 pt page drawing `content` with
/// `/L`, a luminosity mask, and `/A`, an alpha mask, of one group, object 5, which
/// fills the page black.
fn pixels(content: &str) -> [[u8; 3]; 2] {
    let pdf = Pdf::new(fixtures::one_page(
        100,
        "/Contents 4 0 R /Resources << /ExtGState << \
         /L << /SMask << /S /Luminosity /G 5 0 R >> >> \
         /A << /SMask << /S /Alpha /G 5 0 R >> >> >> >>",
        vec![
            fixtures::stream("", content.as_bytes()),
            fixtures::stream(
                "/Type /XObject /Subtype /Form /Group << /S /Transparency /CS /DeviceGray >> \
                 /BBox [0 0 100 100] /Resources << >>",
                b"0 g 0 0 100 100 re f",
            ),
        ],
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
    [(25, 50), (75, 50)].map(|(x, y): (u16, u16)| {
        let p = pixmap.sample(x, 99 - y);
        [p.r, p.g, p.b]
    })
}

/// Two masks of one group, a luminosity and an alpha mask, are two masks: black
/// masks out what the first draws and lets through all the second draws, in either
/// order (as `PyMuPDF` 1.28.2 renders the page).
#[test]
fn masks_of_one_group_are_told_apart_by_their_type() {
    let (lum, alpha) = (
        "q /L gs 1 0 0 rg 0 0 50 100 re f Q",
        "q /A gs 0 0 1 rg 50 0 50 100 re f Q",
    );
    let expected = [[255, 255, 255], [0, 0, 255]];
    assert_eq!(pixels(&format!("{lum} {alpha}")), expected);
    assert_eq!(pixels(&format!("{alpha} {lum}")), expected);
}
