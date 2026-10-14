//! A non-embedded standard font advances a glyph its metrics lack by the width the
//! font given for it has, as when Liberation Sans stands in for Helvetica and the
//! text is Cyrillic.

use hayro_interpret::font::{FontQuery, GlyphRun, StandardFont};
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterSettings,
    SoftMask,
};
use kurbo::BezPath;
use std::sync::Arc;

mod common;

/// Where each glyph starts, on the x axis.
#[derive(Default)]
struct Rec(Vec<f64>);

impl<'a> Device<'a> for Rec {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, _: &DrawMode) {
        for g in run.glyphs() {
            self.0
                .push((props.transform * g.transform()).translation().x);
        }
    }
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

#[test]
fn a_glyph_the_metrics_lack_advances_by_the_fonts_width() {
    let file = common::one_page(
        100,
        "/Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >>",
        vec![
            common::stream("", b"BT /F1 10 Tf 0 50 Td <808141> Tj ET"),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding << \
              /Differences [128 /afii10017 /afii10018] >> >>"
                .to_vec(),
        ],
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    let liberation: &'static [u8] =
        include_bytes!("../../hayro-tests/assets/LiberationSans-Regular.ttf");
    let settings = InterpreterSettings {
        font_resolver: Arc::new(move |query| match query {
            FontQuery::Standard(StandardFont::Helvetica) => Some((Arc::new(liberation), 0)),
            _ => None,
        }),
        ..Default::default()
    };
    let mut rec = Rec::default();
    common::interpret_first_page(&pdf, settings, &mut rec);
    // А is 667 thousandths of an em wide in Liberation Sans, Б 656.
    let x: Vec<f64> = rec.0.iter().map(|x| (x * 100.0).round() / 100.0).collect();
    assert_eq!(x, [0.0, 6.67, 13.23]);
}
