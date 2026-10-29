//! An ICC profile that many colour spaces are built on is decoded once for the
//! document, whichever colour spaces read it.

#![cfg(debug_assertions)]

use hayro_interpret::InterpreterSettings;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::hayro_syntax::work::TOLD;

mod common;

/// The bytes of the profile's decoded data: no profile, so each colour space falls
/// back to the alternate its `/N` names, after decoding it.
const PROFILE: usize = 256 << 10;

/// The work told drawing one page that fills a square in each of `n` indexed colour
/// spaces, each with a palette of its own, all on one ICC profile.
fn work_drawing_palettes(n: usize) -> usize {
    let spaces: String = (0..n)
        .map(|i| format!("/CS{i} [/Indexed [/ICCBased 5 0 R] 0 <{:02x}0000>] ", i))
        .collect();
    let content: String = (0..n)
        .map(|i| format!("/CS{i} cs 0 sc {i} 0 1 1 re f "))
        .collect();
    let mut objects = common::one_page_objects(
        100,
        &format!("/Contents 4 0 R /Resources << /ColorSpace << {spaces}>> >>"),
    );
    objects.push(common::stream("", content.as_bytes()));
    objects.push(common::stream(
        "/N 3 /Filter /FlateDecode",
        &common::stored_zlib(&vec![0; PROFILE], 65_535),
    ));
    let pdf = Pdf::new(common::file(&objects, "")).expect("valid pdf");
    TOLD.set(0);
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut Ignored);
    TOLD.get()
}

/// Draws nothing.
struct Ignored;

impl<'a> hayro_interpret::Device<'a> for Ignored {
    fn draw_path(
        &mut self,
        _: &kurbo::BezPath,
        _: hayro_interpret::DrawProps<'a>,
        _: &hayro_interpret::DrawMode,
    ) {
    }
    fn push_clip_path(&mut self, _: &hayro_interpret::ClipPath) {}
    fn push_transparency_group(
        &mut self,
        _: f32,
        _: Option<hayro_interpret::SoftMask<'a>>,
        _: hayro_interpret::BlendMode,
    ) {
    }
    fn draw_glyph_run(
        &mut self,
        _: &hayro_interpret::font::GlyphRun<'_, 'a>,
        _: hayro_interpret::DrawProps<'a>,
        _: &hayro_interpret::DrawMode,
    ) {
    }
    fn draw_image(
        &mut self,
        _: hayro_interpret::Image<'a, '_>,
        _: hayro_interpret::ImageDrawProps<'a>,
    ) {
    }
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// Twice the palettes on one profile take no second decoding of it (each palette
/// decoded the profile again: twice the work).
#[test]
fn palettes_on_one_profile_decode_it_once() {
    let (small, large) = (work_drawing_palettes(8), work_drawing_palettes(16));
    assert!(small >= PROFILE, "{small}");
    assert!(
        large < small + PROFILE / 2,
        "{small} bytes of work for 8 palettes, {large} for 16"
    );
}
