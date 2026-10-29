//! A font kept in a cache that serves page after page draws on each page as it
//! draws on that page alone, whatever pages were drawn with the cache before.

use hayro_interpret::annotation::View;
use hayro_interpret::font::{Glyph, GlyphRun};
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps,
    InterpreterCache, InterpreterSettings, Paint, SoftMask, interpret_page,
};
use kurbo::{Affine, BezPath, Rect};

mod common;

/// The colour of each path filled with a colour.
#[derive(Default)]
struct Fills(Vec<[u8; 4]>);

impl<'a> Device<'a> for Fills {
    fn draw_path(&mut self, _: &BezPath, props: DrawProps<'a>, _: &DrawMode) {
        if let Paint::Color(color) = &props.paint {
            self.0.push(color.to_rgba().to_rgba8());
        }
    }
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, _: &DrawMode) {
        for placed in run.glyphs() {
            if let Glyph::Type3(glyph) = &**placed {
                glyph.interpret(self, props.transform, placed.transform(), &props.paint);
            }
        }
    }
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// The fills of each page of `pdf` in `order`, drawn with one cache.
fn fills(pdf: &Pdf, order: &[usize]) -> Vec<Vec<[u8; 4]>> {
    let cache = InterpreterCache::new();
    let pages = pdf.pages();
    order
        .iter()
        .map(|&i| {
            let page = &pages[i];
            let mut ctx = Context::new(
                Affine::IDENTITY,
                Rect::new(0.0, 0.0, 100.0, 100.0),
                &cache,
                page.xref(),
                InterpreterSettings::default(),
            );
            let mut fills = Fills::default();
            interpret_page(page, &View::of(page), &mut ctx, &mut fills);
            fills.0
        })
        .collect()
}

/// A Type 3 font drawn with the resources of the first page, its own, which the
/// second page draws it with too, through resources of its own: drawn after the
/// first page, the second drew the glyph with its own resources, as reading the
/// font's through the first page's refused them as a reference back to that page's.
#[test]
fn a_type3_font_draws_with_its_resources_whatever_page_came_before() {
    let page = |resources: u32| {
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources {resources} 0 R \
             /Contents 7 0 R >>"
        )
        .into_bytes()
    };
    let resources = |lookup: &str| {
        format!(
            "<< /Font << /F1 8 0 R >> /ColorSpace << /CS0 [/Indexed /DeviceRGB 0 <{lookup}>] >> >>"
        )
        .into_bytes()
    };
    let file = common::file(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_vec(),
            page(5),
            page(6),
            resources("FF0000"),
            resources("0000FF"),
            common::stream("", b"BT /F1 100 Tf 0 0 Td (a) Tj ET"),
            b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 100 100] \
              /FontMatrix [0.01 0 0 0.01 0 0] /CharProcs << /a 9 0 R >> \
              /Encoding << /Type /Encoding /Differences [97 /a] >> /FirstChar 97 \
              /LastChar 97 /Widths [100] /Resources 5 0 R >>"
                .to_vec(),
            common::stream("", b"100 0 d0 /CS0 cs 0 sc 0 0 100 100 re f"),
        ],
        "",
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    let red = vec![[255, 0, 0, 255]];
    assert_eq!(fills(&pdf, &[0, 1]), [red.clone(), red.clone()]);
    assert_eq!(fills(&pdf, &[1, 0]), [red.clone(), red]);
}
