//! `TilingPattern::paint_key` is equal for uses that draw the same cell and differs
//! where the cell does: the colour of an uncoloured pattern and the matrix in
//! effect, which a form's matrix changes and the current transform does not (a
//! pattern's space is the page's or the form's); the opacity, applied to the cell as
//! a whole, does not change it. A shading pattern's key changes with the transfer
//! function in effect, which changes its colours.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::pattern::Pattern;
use hayro_interpret::{
    BlendMode, CacheKey, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps,
    InterpreterSettings, Paint, SoftMask,
};
use kurbo::BezPath;

mod common;

/// The key of each patterned fill or stroke, in order: a tiling pattern's paint
/// key, a shading pattern's cache key.
#[derive(Default)]
struct Keys(Vec<u128>);

impl<'a> Device<'a> for Keys {
    fn draw_path(&mut self, _: &BezPath, props: DrawProps<'a>, mode: &DrawMode) {
        if let Paint::Pattern(p) = &props.paint {
            self.0.push(match p.as_ref() {
                Pattern::Tiling(t) => t.paint_key(matches!(mode, DrawMode::Stroke(_))),
                Pattern::Shading(s) => s.cache_key(),
            });
        }
    }
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

fn keys(content: &[u8]) -> Vec<u128> {
    let objects = vec![
        common::stream("", content),
        common::stream(
            "/PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] /XStep 10 /YStep 10 \
             /Resources << >>",
            b"1 0 0 rg 0 0 5 5 re f",
        ),
        common::stream(
            "/PatternType 1 /PaintType 2 /TilingType 1 /BBox [0 0 10 10] /XStep 10 /YStep 10 \
             /Resources << >>",
            b"0 0 5 5 re f",
        ),
        common::stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Matrix [2 0 0 2 0 0] \
             /Resources << /Pattern << /C 5 0 R >> >>",
            b"/Pattern cs /C scn 0 0 20 20 re f",
        ),
        common::stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 200 200] \
             /Resources << /Pattern << /C 5 0 R >> >>",
            b"/Pattern cs /C scn 0 0 20 20 re f",
        ),
    ];
    let file = common::one_page(
        200,
        "/Contents 4 0 R /Resources << /Pattern << /C 5 0 R /U 6 0 R /S << /PatternType 2 \
         /Shading << /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 100 0] \
         /Function << /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >> >> >> >> \
         /ColorSpace << /CsU [/Pattern /DeviceRGB] >> /XObject << /F 7 0 R /G 8 0 R >> \
         /ExtGState << /H << /ca 0.5 >> /I << /TR << /FunctionType 2 /Domain [0 1] \
         /C0 [1] /C1 [0] /N 1 >> >> /J << /TR << /FunctionType 2 /Domain [0 1] /C0 [1] \
         /C1 [0] /N 1 >> >> /D << /TR /Identity >> >> >>",
        objects,
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    let mut rec = Keys::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut rec);
    rec.0
}

#[test]
fn equal_keys_are_equal_drawings() {
    let k = keys(
        b"/Pattern cs /C scn 0 0 50 50 re f 60 0 50 50 re f \
          q 2 0 0 2 0 0 cm 0 60 20 20 re f Q /F Do \
          /CsU cs 1 0 0 /U scn 0 120 20 20 re f 30 120 20 20 re f \
          0 0 1 /U scn 60 120 20 20 re f \
          /CsU CS 1 0 0 /U SCN 0 180 m 50 180 l S",
    );
    let [
        same_a,
        same_b,
        under_cm,
        in_form,
        red_a,
        red_b,
        blue,
        red_stroke,
    ] = k[..]
    else {
        panic!("{k:?}")
    };
    assert_eq!(same_a, same_b, "the same coloured pattern twice");
    assert_eq!(same_a, under_cm, "under another current transform");
    assert_ne!(same_a, in_form, "in a form with a matrix");
    assert_eq!(
        red_a, red_b,
        "the same uncoloured pattern in the same colour"
    );
    assert_ne!(red_a, blue, "in another colour");
    assert_eq!(red_a, red_stroke, "the stroke paint in the same colour");
}

/// A form one level deeper may draw fewer forms in the cell (the interpreter
/// stops at a depth), so the cell is another one.
#[test]
fn a_deeper_use_is_another_cell() {
    let k = keys(b"/Pattern cs /C scn 0 0 50 50 re f /G Do");
    assert_eq!(k.len(), 2);
    assert_ne!(k[0], k[1]);
}

#[test]
fn opacity_does_not_change_the_cell() {
    let k = keys(b"/Pattern cs /C scn 0 0 50 50 re f q /H gs 60 0 50 50 re f Q");
    assert_eq!(k.len(), 2);
    assert_eq!(k[0], k[1]);
}

#[test]
fn a_transfer_function_changes_a_shading_key() {
    let k = keys(
        b"/Pattern cs /S scn 0 0 50 50 re f q /I gs 60 0 50 50 re f Q \
          q /J gs 120 0 50 50 re f Q q /I gs /D gs 0 60 50 50 re f Q",
    );
    let [plain, inverted, inverted_again, reset] = k[..] else {
        panic!("{k:?}")
    };
    assert_ne!(plain, inverted, "under an inverting transfer function");
    assert_eq!(
        inverted, inverted_again,
        "under an equal one in another object"
    );
    assert_eq!(plain, reset, "after /TR /Identity");
}
