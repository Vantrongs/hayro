//! Interpreting a page keeps none of its decoded content: a reader that keeps a
//! document open while it draws page after page would otherwise hold the content of
//! every page it drew for as long as the document is open.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterSettings,
    SoftMask,
};
use kurbo::BezPath;

#[path = "../../hayro-syntax/tests/common/allocations.rs"]
mod allocations;
mod common;

struct Nothing;

impl<'a> Device<'a> for Nothing {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// A page whose content is `bytes` long.
fn page(bytes: usize) -> Pdf {
    let content = "0 0 m\n".repeat(bytes / 6);
    let objects = vec![common::stream("", content.as_bytes())];
    let pdf = Pdf::new(common::one_page(200, "/Contents 4 0 R", objects)).expect("valid pdf");
    assert_eq!(pdf.pages().len(), 1);
    pdf
}

#[test]
fn interpreting_a_page_keeps_none_of_its_content() {
    const CONTENT: usize = 1 << 20;
    // What a thread allocates once, the first time it interprets, is not counted.
    common::interpret_first_page(&page(60), InterpreterSettings::default(), &mut Nothing);
    let pdf = page(CONTENT);
    let ((), used) = allocations::measure(|| {
        common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut Nothing);
    });
    assert!(used.held < CONTENT / 16, "{used:?}");
}
