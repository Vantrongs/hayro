//! Every subpath handed to a device begins with a `MoveTo`, as `kurbo::BezPath`
//! requires: a segment after `h` or `re` continues from the closed subpath's start.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterSettings,
    SoftMask,
};
use kurbo::{BezPath, PathEl, Point};

mod common;

#[derive(Default)]
struct Rec(Vec<Vec<PathEl>>);

impl<'a> Device<'a> for Rec {
    fn draw_path(&mut self, path: &BezPath, props: DrawProps<'a>, _: &DrawMode) {
        let mut path = path.clone();
        path.apply_affine(props.transform);
        self.0.push(path.elements().to_vec());
    }
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
    fn knock_out(&mut self, _: &BezPath, _: kurbo::Affine, _: &DrawMode) {}
}

/// The paths drawn by `content` on a 100 × 100 pt page, in page space (y up).
fn paths(content: &str) -> Vec<Vec<PathEl>> {
    let file = common::one_page(
        100,
        "/Contents 4 0 R",
        vec![common::stream("", content.as_bytes())],
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    let mut rec = Rec::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut rec);
    rec.0
}

fn pt(x: f64, y: f64) -> Point {
    Point::new(x, y)
}

#[test]
fn a_curve_after_close_starts_at_the_closed_subpaths_start() {
    let drawn = paths("0.5 w 10 10 m 20 10 l 10 10 l h 10 30 30 30 30 10 c S");
    assert_eq!(
        drawn,
        [vec![
            PathEl::MoveTo(pt(10.0, 10.0)),
            PathEl::LineTo(pt(20.0, 10.0)),
            PathEl::LineTo(pt(10.0, 10.0)),
            PathEl::ClosePath,
            PathEl::MoveTo(pt(10.0, 10.0)),
            PathEl::CurveTo(pt(10.0, 30.0), pt(30.0, 30.0), pt(30.0, 10.0)),
        ]]
    );
}

#[test]
fn segments_after_re_start_at_its_origin() {
    // `v` takes the current point as its first control point.
    let drawn = paths("10 20 30 40 re 50 60 l 70 80 90 90 v S");
    assert_eq!(
        drawn,
        [vec![
            PathEl::MoveTo(pt(10.0, 20.0)),
            PathEl::LineTo(pt(40.0, 20.0)),
            PathEl::LineTo(pt(40.0, 60.0)),
            PathEl::LineTo(pt(10.0, 60.0)),
            PathEl::ClosePath,
            PathEl::MoveTo(pt(10.0, 20.0)),
            PathEl::LineTo(pt(50.0, 60.0)),
            PathEl::CurveTo(pt(50.0, 60.0), pt(70.0, 80.0), pt(90.0, 90.0)),
        ]]
    );
}

#[test]
fn an_explicit_move_after_close_is_kept_as_is() {
    let drawn = paths("10 10 m 20 10 l h 50 50 m 60 60 l S");
    assert_eq!(
        drawn,
        [vec![
            PathEl::MoveTo(pt(10.0, 10.0)),
            PathEl::LineTo(pt(20.0, 10.0)),
            PathEl::ClosePath,
            PathEl::MoveTo(pt(50.0, 50.0)),
            PathEl::LineTo(pt(60.0, 60.0)),
        ]]
    );
}
