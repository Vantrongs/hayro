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

/// Where `text` places its glyphs in 10-point Helvetica, drawn by Liberation Sans,
/// with `differences`.
fn placed(text: &str, differences: &str) -> Vec<f64> {
    shown(&format!("{text} Tj"), differences)
}

/// Where the text operators `ops` place their glyphs, as [`placed`].
fn shown(ops: &str, differences: &str) -> Vec<f64> {
    let file = common::one_page(
        100,
        "/Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >>",
        vec![
            common::stream("", format!("BT /F1 10 Tf 0 50 Td {ops} ET").as_bytes()),
            format!(
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding << \
                 /Differences {differences} >> >>"
            )
            .into_bytes(),
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
    rec.0.iter().map(|x| (x * 100.0).round() / 100.0).collect()
}

#[test]
fn a_glyph_the_metrics_lack_advances_by_the_fonts_width() {
    // А is 667 thousandths of an em wide in Liberation Sans, Б 656.
    assert_eq!(
        placed("<808141>", "[128 /afii10017 /afii10018]"),
        [0.0, 6.67, 13.23]
    );
}

/// An entry of /Differences that is neither a number nor a name, a reference to no
/// object among them, is passed over without taking a code, as `MuPDF` reads it
/// (`PyMuPDF` 1.28.2: `[65 /x 8 0 R /y null /z]` gives codes 65 to 67 x, y, z).
#[test]
fn an_entry_of_the_differences_that_is_no_name_takes_no_code() {
    // B is Б, 656 thousandths of an em, where the dangling reference takes no code;
    // it would be Helvetica's B, 667, if it ended the list.
    let expected = [0.0, 6.67, 13.23];
    for differences in [
        "[65 /afii10017 8 0 R /afii10018 null /A]",
        "[65 /afii10017 (B) /afii10018 /A]",
    ] {
        assert_eq!(placed("(ABC)", differences), expected, "{differences}");
    }
}

/// A `TJ` array with an entry that is neither a number nor a string draws nothing
/// and moves the text on by nothing, and the text after it is drawn, as `MuPDF`
/// draws it (`PyMuPDF` 1.28.2: `[(AB) null (CD)] TJ (EF) Tj` draws EF where it
/// would stand alone; so with `true`, `truefoo`, a name, an array or a dictionary).
#[test]
fn a_text_array_with_an_entry_of_another_kind_draws_nothing() {
    let none = "[]";
    let alone = shown("(EF) Tj", none);
    assert_eq!(alone.len(), 2);
    for entry in ["null", "true", "truefoo", "/N", "[1]", "<< >>"] {
        let ops = format!("[(AB) {entry} (CD)] TJ (EF) Tj");
        assert_eq!(shown(&ops, none), alone, "{entry}");
    }
    assert_eq!(shown("[(AB) -100 (CD)] TJ (EF) Tj", none).len(), 6);
}

/// A `TJ` array's entries are each read once, deciding whether the array is drawn
/// as they are drawn: 100 more entries of 10 bytes make the readers look at
/// `TJ_EXAMINED` more bytes, the array parsed as an operand and its entries read
/// once, with what reading a token looks at past its end (reading them a second time
/// to decide first, as before, made it 8,400). The count is exact: a change that
/// moves it sets `TJ_EXAMINED` to the count the failure names.
#[test]
fn a_text_array_is_read_once() {
    use hayro_interpret::hayro_syntax::work::EXAMINED;
    let examined = |n: usize| {
        let ops = format!("[{}] TJ", "(AB) -100 ".repeat(n));
        EXAMINED.set(0);
        assert_eq!(shown(&ops, "[]").len(), 2 * n);
        EXAMINED.get()
    };
    assert_eq!(examined(200) - examined(100), TJ_EXAMINED);
}

const TJ_EXAMINED: usize = 5_300;
