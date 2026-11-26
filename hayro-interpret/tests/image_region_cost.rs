//! Memory used to fit an image with a short first row to a small grid.
use hayro_interpret::font::GlyphRun;
use hayro_interpret::{
    BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps, ImageRequest,
    InterpreterSettings, Plane, SoftMask,
};
use hayro_syntax::Pdf;
use kurbo::BezPath;

#[path = "../../hayro-syntax/tests/common/allocations.rs"]
mod allocations;
mod common;

struct Record<F>(F);

impl<'a, F: FnMut(Image<'a, '_>)> Device<'a> for Record<F> {
    fn draw_image(&mut self, image: Image<'a, '_>, _: ImageDrawProps<'a>) {
        (self.0)(image);
    }
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(
        &mut self,
        _: f32,
        _: Option<SoftMask<'a>>,
        _: BlendMode,
        _: bool,
        _: bool,
    ) {
    }
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

#[test]
fn a_small_fit_holds_only_stored_samples_and_output() {
    for width in [1024, 8192, 65536] {
        let pdf = Pdf::new(common::one_page(32,
            "/Contents 4 0 R /Resources << /XObject << /I 5 0 R >> >>", vec![
                common::stream("", b"/I Do"),
                common::stream(&format!("/Subtype /Image /Width {width} /Height 1 /BitsPerComponent 8 /ColorSpace /DeviceGray /Decode [1 0]"), &[255; 32]),
            ])).unwrap();
        let mut asset = None;
        let mut record = Record(|image: Image<'_, '_>| {
            if let Image::Raster(image) = image {
                asset = Some(image.asset());
            }
        });
        common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut record);
        let asset = asset.unwrap();
        let (result, used) = allocations::measure(|| {
            asset
                .open(None)
                .unwrap()
                .request(&ImageRequest {
                    plane: Plane::Image,
                    grid: (2, 1),
                    window: [0, 0, 2, 1],
                    cancel: None,
                })
                .unwrap()
        });
        let half = width / 2;
        assert_eq!(
            result.data,
            [((255 * (half - 32) + half / 2) / half) as u8, 255]
        );
        assert!(used.peak < 16_384, "width {width}: {used:?}");
    }
}

#[test]
fn extraction_callbacks_return_only_stored_rows() {
    for dict in [
        "/ColorSpace /DeviceGray /BitsPerComponent 8",
        "/ImageMask true /BitsPerComponent 1",
    ] {
        let pdf = Pdf::new(common::one_page(
            32,
            "/Contents 4 0 R /Resources << /XObject << /I 5 0 R >> >>",
            vec![
                common::stream("", b"/I Do"),
                common::stream(
                    &format!("/Subtype /Image /Width 1 /Height 1000000 {dict}"),
                    &[0],
                ),
            ],
        ))
        .unwrap();
        let mut dims = None;
        let mut record = Record(|image: Image<'_, '_>| {
            let (_, used) = allocations::measure(|| match image {
                Image::Raster(r) => r.with_rgba(
                    |d, _| {
                        let hayro_interpret::ImageData::Luma(d) = d else {
                            panic!()
                        };
                        dims = Some((d.width, d.height, d.data.len()));
                    },
                    None,
                ),
                Image::Stencil(s) => {
                    s.with_stencil(|d, _| dims = Some((d.width, d.height, d.data.len())), None);
                }
            });
            assert!(used.peak < 64 * 1024, "{used:?}");
        });
        common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut record);
        assert_eq!(dims, Some((1, 1, 1)));
    }
}
