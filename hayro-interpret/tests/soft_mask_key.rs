//! A soft mask is drawn once for every use that draws it alike: once for all the
//! clips it is set under where drawing it reads no clip, else once for each box
//! of clips; once for all the forms that set it where its group has resources of
//! its own, else once for each form's resources.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterSettings,
    SoftMask, SoftMasks,
};
use kurbo::{Affine, BezPath};

mod common;

/// The soft masks drawn for the paths drawn, each once for the uses it serves.
#[derive(Default)]
struct Rec {
    masks: SoftMasks<()>,
    drawn: usize,
}

impl<'a> Device<'a> for Rec {
    fn draw_path(&mut self, _: &BezPath, props: DrawProps<'a>, _: &DrawMode) {
        if let Some(mask) = props.soft_mask
            && self.masks.get(&mask).is_none()
        {
            let interpreted = mask.interpret(&mut Self::default());
            self.masks.insert(interpreted, ());
            self.drawn += 1;
        }
    }
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
    fn knock_out(&mut self, _: &BezPath, _: Affine, _: &DrawMode) {}
}

/// The number of soft masks drawn for a page that draws two forms, each with
/// resources of its own and under a clip of its own, which set one soft mask whose
/// group has the box `group_box` and `group_entries` and draws `group_content`.
fn drawn(group_box: &str, group_entries: &str, group_content: &str) -> usize {
    let form = |extra: &str| {
        common::stream(
            &format!(
                "/Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                 /Resources << /ExtGState << /G 7 0 R >> {extra} >>"
            ),
            b"/G gs 0 0 100 100 re f",
        )
    };
    let file = common::one_page(
        100,
        "/Contents 4 0 R /Resources << /XObject << /F1 5 0 R /F2 6 0 R >> >>",
        vec![
            common::stream(
                "",
                b"q 0 0 80 80 re W n /F1 Do Q q 20 20 80 80 re W n /F2 Do Q",
            ),
            form(""),
            form("/Font << >>"),
            b"<< /SMask << /S /Alpha /G 8 0 R >> >>".to_vec(),
            common::stream(
                &format!(
                    "/Type /XObject /Subtype /Form /BBox {group_box} \
                     /Group << /S /Transparency >> {group_entries}"
                ),
                group_content.as_bytes(),
            ),
        ],
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    let mut rec = Rec::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut rec);
    rec.drawn
}

const SHADING: &str = "/Resources << /Shading << /Sh << /ShadingType 2 \
    /ColorSpace /DeviceGray /Coords [0 0 100 0] /Function << /FunctionType 2 \
    /Domain [0 1] /C0 [0] /C1 [1] /N 1 >> >> >> >>";

#[test]
fn a_soft_mask_that_fills_a_shape_is_drawn_once_under_every_clip_and_form() {
    for group_box in ["[30 30 70 70]", "[0 0 100 100]"] {
        assert_eq!(drawn(group_box, "/Resources << >>", "0 0 100 100 re f"), 1);
    }
}

#[test]
fn a_soft_mask_whose_group_reads_the_forms_resources_is_drawn_for_each_form() {
    assert_eq!(drawn("[30 30 70 70]", "", "0 0 100 100 re f"), 2);
}

/// `sh` fills the box of the clips and samples the shading from its corner, so the
/// mask is drawn for each box, even where the boxes leave one part of the group's.
#[test]
fn a_soft_mask_that_reads_the_clips_is_drawn_for_each_box_of_clips() {
    for group_box in ["[30 30 70 70]", "[0 0 100 100]"] {
        assert_eq!(drawn(group_box, SHADING, "/Sh sh"), 2);
    }
}

/// Whether a clip is pushed depends on whether it covers the box of the clips.
#[test]
fn a_soft_mask_whose_group_clips_with_a_rectangle_is_drawn_for_each_box_of_clips() {
    let content = "10 10 50 50 re W n 0 0 100 100 re f";
    assert_eq!(drawn("[0 0 100 100]", "/Resources << >>", content), 2);
}
