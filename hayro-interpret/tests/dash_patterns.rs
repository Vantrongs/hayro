//! A dash pattern the PDF specification does not allow (an entry below zero, or
//! every entry zero) is drawn as `MuPDF` draws it (observed in `PyMuPDF` 1.28.2):
//! not at all if its entries sum to zero, otherwise solid. A renderer given such a
//! pattern walks it backwards, or never ends on one whose entries sum below zero.

use hayro_interpret::font::GlyphRun;
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
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, mode: &DrawMode) {
        self.push(mode);
    }
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// What `content` draws on a page whose `/G` graphics state is `gs` and whose `/F`
/// is Helvetica.
fn drawn(gs: &str, content: &str) -> Vec<Ev> {
    let file = common::one_page(
        100,
        &format!(
            "/Contents 4 0 R /Resources << /ExtGState << /G << {gs} >> >> \
             /Font << /F << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >>"
        ),
        vec![common::stream("", content.as_bytes())],
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
