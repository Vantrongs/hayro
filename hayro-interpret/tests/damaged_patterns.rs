//! The boxes, matrices and steps of patterns and shadings are read as viewers read
//! damaged ones, as those of forms are: an entry that is not a number reads as 0 in
//! its place, missing ones are 0, and entries beyond those used are ignored.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::pattern::Pattern;
use hayro_interpret::shading::ShadingType;
use hayro_interpret::{
    BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterSettings,
    Paint, SoftMask,
};
use kurbo::{Affine, BezPath, Rect, Shape};

mod common;

/// What each fill is painted with, in short.
#[derive(Debug, PartialEq)]
enum Painted {
    Tiling {
        bbox: Rect,
        step: (f32, f32),
        matrix: Affine,
    },
    Shading {
        matrix: Affine,
        clip: Option<Rect>,
        function_matrix: Option<Affine>,
    },
    Color,
}

#[derive(Default)]
struct Rec(Vec<Painted>);

impl<'a> Device<'a> for Rec {
    fn draw_path(&mut self, _: &BezPath, props: DrawProps<'a>, _: &DrawMode) {
        self.0.push(match &props.paint {
            Paint::Pattern(p) => match p.as_ref() {
                Pattern::Tiling(t) => Painted::Tiling {
                    bbox: t.bbox,
                    step: (t.x_step, t.y_step),
                    matrix: t.matrix,
                },
                Pattern::Shading(s) => Painted::Shading {
                    matrix: s.matrix,
                    clip: s.clip_path().as_ref().map(Shape::bounding_box),
                    function_matrix: match s.shading.shading_type().as_ref() {
                        ShadingType::FunctionBased { matrix, .. } => Some(*matrix),
                        _ => None,
                    },
                },
            },
            Paint::Color(_) => Painted::Color,
        });
    }
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// What a page filling a square with pattern object `pattern` paints.
fn painted(pattern: Vec<u8>, extra: Vec<Vec<u8>>) -> Vec<Painted> {
    let mut objects = vec![
        common::stream("", b"/Pattern cs /P0 scn 20 20 160 160 re f"),
        pattern,
    ];
    objects.extend(extra);
    let file = common::one_page(
        200,
        "/Contents 4 0 R /Resources << /Pattern << /P0 5 0 R >> >>",
        objects,
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    let mut rec = Rec::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut rec);
    rec.0
}

fn tiling(entries: &str) -> Vec<Painted> {
    painted(
        common::stream(
            &format!(
                "/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /Resources << >> \
                 {entries}"
            ),
            b"1 0 0 rg 0 0 10 10 re f",
        ),
        vec![],
    )
}

#[test]
fn a_damaged_tiling_pattern_is_read_in_place() {
    let read = |bbox: [f64; 4], matrix: [f64; 6]| {
        vec![Painted::Tiling {
            bbox: Rect::new(bbox[0], bbox[1], bbox[2], bbox[3]),
            step: (20.0, 20.0),
            matrix: Affine::new(matrix),
        }]
    };
    let steps = "/XStep 20 /YStep 20";
    let id = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    for (bbox, matrix) in [
        ("[0 null 20 20]", ""),
        ("[0 0 20 20 5]", ""),
        ("[0 0 20 20]", "/Matrix [2 0 0 2 0 (x)]"),
        ("[0 0 20 20]", "/Matrix [2 0 0 2]"),
    ] {
        let want = if matrix.is_empty() {
            id
        } else {
            [2.0, 0.0, 0.0, 2.0, 0.0, 0.0]
        };
        assert_eq!(
            tiling(&format!("/BBox {bbox} {steps} {matrix}")),
            read([0.0, 0.0, 20.0, 20.0], want),
            "{bbox} {matrix}"
        );
    }
    // A box entry that is not a number reads as 0, here a box of no width: nothing.
    assert_eq!(
        tiling(&format!("/BBox [0 0 /x 20] {steps}")),
        [Painted::Color]
    );
}

fn axial(pattern: &str, shading: &str) -> Vec<Painted> {
    painted(
        format!(
            "<< /Type /Pattern /PatternType 2 /Shading << /ShadingType 2 \
             /ColorSpace /DeviceRGB /Coords [20 0 180 0] /Function << /FunctionType 2 \
             /Domain [0 1] /C0 [1 0 0] /C1 [0 0 1] /N 1 >> {shading} >> {pattern} >>"
        )
        .into_bytes(),
        vec![],
    )
}

#[test]
fn a_damaged_shading_is_read_in_place() {
    let half = Affine::new([0.5, 0.0, 0.0, 0.5, 0.0, 0.0]);
    let shaded = |matrix: Affine, clip: Option<Rect>| {
        vec![Painted::Shading {
            matrix,
            clip,
            function_matrix: None,
        }]
    };
    for matrix in [
        "[0.5 0 0 0.5]",
        "[0.5 0 0 0.5 null 0]",
        "[0.5 0 0 0.5 0 (x)]",
    ] {
        assert_eq!(
            axial(&format!("/Matrix {matrix}"), ""),
            shaded(half, None),
            "{matrix}"
        );
    }
    let square = Some(Rect::new(0.0, 0.0, 100.0, 100.0));
    for bbox in ["[/x 0 100 100]", "[0 0 100 100 5]"] {
        assert_eq!(
            axial("", &format!("/BBox {bbox}")),
            shaded(Affine::IDENTITY, square),
            "{bbox}"
        );
    }
    // A function-based shading's own matrix.
    let function = painted(
        b"<< /Type /Pattern /PatternType 2 /Shading << /ShadingType 1 \
           /ColorSpace /DeviceRGB /Matrix [100 0 0 100 50 (x)] /Function 6 0 R >> >>"
            .to_vec(),
        vec![common::stream(
            "/FunctionType 4 /Domain [0 1 0 1] /Range [0 1 0 1 0 1]",
            b"{ pop dup 0 exch }",
        )],
    );
    assert_eq!(
        function,
        [Painted::Shading {
            matrix: Affine::IDENTITY,
            clip: None,
            function_matrix: Some(Affine::new([100.0, 0.0, 0.0, 100.0, 50.0, 0.0])),
        }]
    );
}
