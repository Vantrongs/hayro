//! A path filled and stroked by one operator is painted as viewers paint it: where
//! the stroke covers the fill, only the stroke shows, however translucent both are;
//! the fill is cleared there before the stroke is drawn.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterSettings,
    Paint, SoftMask,
};
use kurbo::{Affine, BezPath};

mod common;

#[derive(Debug, PartialEq)]
enum Ev {
    /// A fill or stroke, with the alpha of its colour and its blend mode.
    Draw {
        stroke: bool,
        alpha: u8,
        blend: BlendMode,
    },
    Group {
        opacity: f32,
        blend: BlendMode,
    },
    EndGroup,
    /// A clearing of the group by a stroke.
    KnockOut,
}

#[derive(Default)]
struct Rec(Vec<Ev>);

impl<'a> Device<'a> for Rec {
    fn draw_path(&mut self, _: &BezPath, props: DrawProps<'a>, mode: &DrawMode) {
        let Paint::Color(c) = &props.paint else {
            panic!("solid paint")
        };
        let alpha = c.to_rgba().to_rgba8()[3];
        let blend = props.blend_mode;
        match mode {
            DrawMode::Fill(_) => self.0.push(Ev::Draw {
                stroke: false,
                alpha,
                blend,
            }),
            DrawMode::Stroke(_) => self.0.push(Ev::Draw {
                stroke: true,
                alpha,
                blend,
            }),
            _ => panic!("{mode:?}"),
        }
    }
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, opacity: f32, _: Option<SoftMask<'a>>, blend: BlendMode) {
        self.0.push(Ev::Group { opacity, blend });
    }
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {
        self.0.push(Ev::EndGroup);
    }
    fn knock_out(&mut self, _: &BezPath, _: Affine, mode: &DrawMode) {
        assert!(matches!(mode, DrawMode::Stroke(_)), "{mode:?}");
        self.0.push(Ev::KnockOut);
    }
}

/// What `content` draws on a page whose `/G` graphics state is `gs`.
fn drawn(gs: &str, content: &str) -> Vec<Ev> {
    let file = common::one_page(
        100,
        &format!("/Contents 4 0 R /Resources << /ExtGState << /G << {gs} >> >> >>"),
        vec![common::stream("", content.as_bytes())],
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    let mut rec = Rec::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut rec);
    rec.0
}

const SQUARE: &str = "/G gs 10 w 0 0 1 RG 1 0 0 rg 25 25 50 50 re b";

#[test]
fn a_translucent_fill_and_stroke_are_one_group() {
    let opaque = |stroke| Ev::Draw {
        stroke,
        alpha: 255,
        blend: BlendMode::Normal,
    };
    for (gs, opacity, blend) in [
        ("/ca 0.5 /CA 0.5", 0.5, BlendMode::Normal),
        ("/BM /Multiply", 1.0, BlendMode::Multiply),
    ] {
        assert_eq!(
            drawn(gs, SQUARE),
            [
                Ev::Group { opacity, blend },
                opaque(false),
                Ev::KnockOut,
                opaque(true),
                Ev::EndGroup
            ],
            "{gs}"
        );
    }
    // Opaque, they are drawn as they are.
    assert_eq!(drawn("", SQUARE), [opaque(false), opaque(true)]);
}
