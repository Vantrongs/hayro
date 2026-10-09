//! Every subpath handed to a device begins with a `MoveTo`, as `kurbo::BezPath`
//! requires: a segment after `h` or `re` continues from the closed subpath's start.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps,
    InterpreterCache, InterpreterSettings, SoftMask, interpret_page,
};
use kurbo::{Affine, BezPath, PathEl, Point, Rect};

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
}

/// The paths drawn by `content` on a 100 × 100 pt page, in page space (y up).
fn paths(content: &str) -> Vec<Vec<PathEl>> {
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>".to_string(),
        format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len() + 1
        ),
    ];
    let mut out = String::from("%PDF-1.7\n");
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
    }
    let xref = out.len();
    out.push_str(&format!(
        "xref\n0 {}\n0000000000 65535 f \n",
        objs.len() + 1
    ));
    for o in offsets {
        out.push_str(&format!("{o:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objs.len() + 1
    ));
    let pdf = Pdf::new(out.into_bytes()).expect("valid pdf");
    let page = &pdf.pages()[0];
    let cache = InterpreterCache::new();
    let mut ctx = Context::new(
        Affine::IDENTITY,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        &cache,
        page.xref(),
        InterpreterSettings::default(),
    );
    let mut rec = Rec::default();
    interpret_page(page, &mut ctx, &mut rec);
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
