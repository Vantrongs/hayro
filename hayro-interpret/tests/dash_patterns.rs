//! A dash pattern the PDF specification does not allow (an entry below zero, or
//! every entry zero) is drawn as `MuPDF` draws it (observed in `PyMuPDF` 1.28.2):
//! not at all if its entries sum to zero, otherwise solid. A renderer given such a
//! pattern walks it backwards, or never ends on one whose entries sum below zero.

use hayro_interpret::font::{Glyph, GlyphRun};
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterSettings,
    SoftMask,
};
use kurbo::BezPath;

mod common;

#[derive(Debug, PartialEq)]
enum Ev {
    Fill,
    /// A stroke, with its dash array and offset.
    Stroke(Vec<f32>, f32),
    Clip,
}

#[derive(Default)]
struct Rec(Vec<Ev>);

impl Rec {
    fn push(&mut self, mode: &DrawMode) {
        match mode {
            DrawMode::Fill(_) => self.0.push(Ev::Fill),
            DrawMode::Stroke(s) => self
                .0
                .push(Ev::Stroke(s.dash_array.to_vec(), s.dash_offset)),
            _ => panic!("{mode:?}"),
        }
    }
}

impl<'a> Device<'a> for Rec {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, mode: &DrawMode) {
        self.push(mode);
    }
    fn push_clip_path(&mut self, _: &ClipPath) {
        self.0.push(Ev::Clip);
    }
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, mode: &DrawMode) {
        // Invisible text is passed on for text extraction alone.
        if matches!(mode, DrawMode::Invisible) {
            return;
        }
        for glyph in run.glyphs() {
            match &**glyph {
                Glyph::Outline(_) => self.push(mode),
                // Its own program draws a Type 3 glyph, as the renderers do.
                Glyph::Type3(t3) => {
                    t3.interpret(self, props.transform, glyph.transform(), &props.paint);
                }
            }
        }
    }
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// What `content` draws on a page whose `/G` graphics state is `gs`, whose `/F` is
/// Helvetica, and whose `/T` is a Type 3 font: its `a` fills its square, its `b`
/// strokes a square inside it.
fn drawn(gs: &str, content: &str) -> Vec<Ev> {
    let file = common::one_page(
        100,
        &format!(
            "/Contents 4 0 R /Resources << /ExtGState << /G << {gs} >> >> \
             /Font << /F << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> \
             /T << /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] \
             /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /a 5 0 R /b 6 0 R >> \
             /Encoding << /Type /Encoding /Differences [97 /a /b] >> \
             /FirstChar 97 /LastChar 98 /Widths [1000 1000] /Resources << >> >> >> >>"
        ),
        vec![
            common::stream("", content.as_bytes()),
            common::stream("", b"1000 0 0 0 1000 1000 d1 0 0 1000 1000 re f"),
            common::stream("", b"1000 0 0 0 1000 1000 d1 100 w 200 200 600 600 re S"),
        ],
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    let mut rec = Rec::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut rec);
    rec.0
}

fn line(dash: &str) -> Vec<Ev> {
    drawn("", &format!("{dash} 10 50 m 90 50 l S"))
}

/// A solid stroke, its offset as set.
fn solid(offset: f32) -> Vec<Ev> {
    vec![Ev::Stroke(vec![], offset)]
}

#[test]
fn a_pattern_below_zero_is_drawn_solid() {
    for (array, offset) in [
        ("[-1 -1]", 0.0),
        ("[-3]", 0.0),
        ("[2 -1]", 0.0),
        ("[1 -2]", 0.0),
        ("[0 -1]", 0.0),
        ("[3 -1 3 -1]", 5.0),
        ("[-1 -1]", -3.0),
    ] {
        let dash = format!("{array} {offset} d");
        assert_eq!(line(&dash), solid(offset), "{dash}");
    }
    assert_eq!(
        drawn("/D [[-1 -1] 0]", "/G gs 10 50 m 90 50 l S"),
        solid(0.0)
    );
}

#[test]
fn a_pattern_summing_to_zero_hides_the_stroke() {
    for dash in [
        "[1 -1] 0 d",
        "[-1 1] 0 d",
        "[0 0] 0 d",
        "[0] 0 d",
        "[0 0] 3 d",
    ] {
        assert_eq!(line(dash), [], "{dash}");
    }
    assert_eq!(drawn("/D [[1 -1] 0]", "/G gs 10 50 m 90 50 l S"), []);
    // The fill of a path filled and stroked stays, and so does a glyph's.
    assert_eq!(drawn("", "[1 -1] 0 d 20 20 60 60 re B"), [Ev::Fill]);
    assert_eq!(
        drawn("", "[1 -1] 0 d BT /F 40 Tf 2 Tr 10 40 Td (I) Tj ET"),
        [Ev::Fill]
    );
    assert_eq!(
        drawn("", "[1 -1] 0 d BT /F 40 Tf 1 Tr 10 40 Td (I) Tj ET"),
        []
    );
}

#[test]
fn a_hidden_stroke_shows_again_once_its_pattern_is_replaced_or_restored() {
    assert_eq!(drawn("", "q [1 -1] 0 d Q 10 50 m 90 50 l S"), solid(0.0),);
    assert_eq!(
        drawn("", "[1 -1] 0 d [4 4] 2 d 10 50 m 90 50 l S"),
        [Ev::Stroke(vec![4.0, 4.0], 2.0)]
    );
}

#[test]
fn a_valid_pattern_is_kept() {
    assert_eq!(line("[4 4] 2 d"), [Ev::Stroke(vec![4.0, 4.0], 2.0)]);
    // A negative offset is kept for the renderer to wrap into the period.
    assert_eq!(line("[4 4] -2 d"), [Ev::Stroke(vec![4.0, 4.0], -2.0)]);
    // A dash of no length is lengthened a little, as before.
    assert_eq!(line("[0 5] 0 d"), [Ev::Stroke(vec![0.01, 5.0], 0.0)]);
}

/// A pattern that hides strokes hides a glyph's strokes alone, under every text
/// render mode, as `PyMuPDF` 1.28.2 draws each of these (a page of red fill, blue
/// stroke, and a green rectangle after the text that a text clip cuts): an
/// outline glyph's fill and its clip stay; a Type 3 glyph's program runs, its
/// fills drawn and its own strokes hidden. `MuPDF` draws a Type 3 glyph under
/// every mode but 3 and 7, and adds none to the clip.
#[test]
fn a_hidden_stroke_keeps_a_glyphs_fills_and_clip() {
    for mode in 0..8 {
        for dash in ["", "[0 0] 0 d", "[1 -1] 0 d"] {
            let solid = dash.is_empty();
            let shown = !matches!(mode, 3 | 7);
            for (font, text, fill, stroke, clip) in [
                (
                    "/F 60",
                    "(I)",
                    matches!(mode, 0 | 2 | 4 | 6),
                    solid && matches!(mode, 1 | 2 | 5 | 6),
                    mode >= 4,
                ),
                ("/T 50", "(a)", shown, false, false),
                ("/T 50", "(b)", false, solid && shown, false),
            ] {
                let events = drawn(
                    "",
                    &format!(
                        "4 w {dash} BT {font} Tf {mode} Tr 25 25 Td {text} Tj ET 0 0 20 100 re f"
                    ),
                );
                // The rectangle after the text is the last fill.
                let glyph = &events[..events.len() - 1];
                let has = |ev: fn(&Ev) -> bool| glyph.iter().any(ev);
                assert_eq!(
                    (
                        has(|e| *e == Ev::Fill),
                        has(|e| matches!(e, Ev::Stroke(..))),
                        has(|e| *e == Ev::Clip)
                    ),
                    (fill, stroke, clip),
                    "{font} {text} {mode} Tr {dash:?}: {events:?}"
                );
            }
        }
    }
}
