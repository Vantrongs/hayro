//! A transparency group renders under a clip as what it draws renders without it:
//! a group's layer limited to the pixels it draws in covers none of the pixels the
//! clip covers in part twice.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};

#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

const CLIPS: [&str; 3] = [
    // A curve, which covers the pixels on its edge in part.
    "50 90 m 72 90 90 72 90 50 c 90 28 72 10 50 10 c 28 10 10 28 10 50 c \
     10 72 28 90 50 90 c W n",
    // A rectangle whose edges cross pixels.
    "10.5 10.25 60.3 60.6 re W n",
    // A rectangle of whole pixels.
    "10 10 60 60 re W n",
];

/// The pixels of a 100 pt page that sets `clip` and then draws `content`, where
/// `/G` is a transparency group filling the page red and `/D` sets the darken
/// blend mode.
fn pixels(clip: &str, content: &str) -> Vec<u8> {
    let pdf = Pdf::new(fixtures::one_page(
        100,
        "/Contents 4 0 R /Resources << /XObject << /G 5 0 R >> \
         /ExtGState << /D << /BM /Darken >> >> >>",
        vec![
            fixtures::stream("", format!("{clip} {content}").as_bytes()),
            fixtures::stream(
                "/Type /XObject /Subtype /Form /Group << /S /Transparency >> \
                 /BBox [0 0 100 100] /Resources << >>",
                b"1 0 0 rg 0 0 100 100 re f",
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
    pixmap.data_as_u8_slice().to_vec()
}

fn assert_same(clip: &str, a: &str, b: &str) {
    let (a, b) = (pixels(clip, a), pixels(clip, b));
    let differing = a.iter().zip(&b).filter(|(a, b)| a != b).count();
    assert_eq!(differing, 0, "{clip}");
}

#[test]
fn a_group_under_a_clip_renders_as_its_contents_do() {
    for clip in CLIPS {
        assert_same(clip, "/G Do", "1 0 0 rg 0 0 100 100 re f");
    }
}

/// A black square filled and stroked along whole pixels under the darken blend
/// mode, which makes it a knockout group, over white is the black square it
/// covers, under clips inside it. (Where a clip crosses both the fill and the
/// stroke, the stroke clears the fill only as far as the clip covers it.)
#[test]
fn a_knockout_group_under_a_clip_renders_as_what_it_paints_does() {
    for clip in [
        "50 75 m 64 75 75 64 75 50 c 75 36 64 25 50 25 c 36 25 25 36 25 50 c \
         25 64 36 75 50 75 c W n",
        "30.5 30.25 40.3 40.6 re W n",
        "30 30 40 40 re W n",
    ] {
        assert_same(
            clip,
            "/D gs 0 g 0 G 2 w 20 20 60 60 re b",
            "0 g 19 19 62 62 re f",
        );
    }
}
