//! A soft mask is drawn once for every use that draws it alike: its key names what
//! drawing it reads, the part of its group's box the clips leave and not the rest
//! of the clips, nor the resources of the form that sets it where its group does
//! not read them.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, CacheKey, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps,
    InterpreterSettings, SoftMask,
};
use kurbo::{Affine, BezPath};

mod common;

/// The key of the soft mask of each path drawn.
#[derive(Default)]
struct Rec(Vec<Option<u128>>);

impl<'a> Device<'a> for Rec {
    fn draw_path(&mut self, _: &BezPath, props: DrawProps<'a>, _: &DrawMode) {
        self.0
            .push(props.soft_mask.as_ref().map(CacheKey::cache_key));
    }
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
    fn knock_out(&mut self, _: &BezPath, _: Affine, _: &DrawMode) {}
}

/// The mask keys of a page that draws two forms, each with resources of its own and
/// under a clip of its own, which set one soft mask whose group has the box
/// `group_box` and `group_entries`.
fn keys(group_box: &str, group_entries: &str) -> Vec<Option<u128>> {
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
                b"0 0 100 100 re f",
            ),
        ],
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    let mut rec = Rec::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut rec);
    rec.0
}

#[test]
fn a_soft_mask_whose_group_has_resources_is_one_under_every_clip_and_form() {
    let keys = keys("[30 30 70 70]", "/Resources << >>");
    assert_eq!(keys.len(), 2);
    assert!(keys[0].is_some());
    assert_eq!(keys[0], keys[1]);
}

#[test]
fn a_soft_mask_whose_group_reads_the_forms_resources_is_one_for_each_form() {
    let keys = keys("[30 30 70 70]", "");
    assert_eq!(keys.len(), 2);
    assert!(keys[0].is_some() && keys[1].is_some());
    assert_ne!(keys[0], keys[1]);
}

#[test]
fn a_soft_mask_is_one_for_each_part_of_its_box_the_clips_leave() {
    let keys = keys("[0 0 100 100]", "/Resources << >>");
    assert_eq!(keys.len(), 2);
    assert!(keys[0].is_some() && keys[1].is_some());
    assert_ne!(keys[0], keys[1]);
}
