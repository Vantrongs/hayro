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

fn asset(dict: &str, data: &[u8]) -> hayro_interpret::ImageAsset {
    let pdf = Pdf::new(common::one_page(
        32,
        "/Contents 4 0 R /Resources << /XObject << /I 5 0 R >> >>",
        vec![
            common::stream("", b"/I Do"),
            common::stream(
                &format!("/Subtype /Image /ColorSpace /DeviceGray {dict}"),
                data,
            ),
        ],
    ))
    .unwrap();
    let mut asset = None;
    common::interpret_first_page(
        &pdf,
        InterpreterSettings::default(),
        &mut Record(|image: Image<'_, '_>| {
            if let Image::Raster(r) = image {
                asset = Some(r.asset());
            }
        }),
    );
    asset.unwrap()
}

#[test]
fn whole_codecs_check_working_planes_before_allocation() {
    let jpeg = include_bytes!("fixtures/progressive-gray.jpg");
    let jpx = include_bytes!("../../hayro-jpeg2000/tests/streams/codestream_levels_8.j2k");
    for (dict, data, min) in [
        (
            "/Width 512 /Height 512 /BPC 8 /Filter /DCTDecode",
            jpeg.as_slice(),
            512 * 512 * 3,
        ),
        (
            "/Width 128 /Height 96 /BPC 8 /Filter /JPXDecode",
            jpx.as_slice(),
            128 * 96 * 5,
        ),
        (
            "/Width 4096 /Height 2048 /BPC 1 /Filter /CCITTFaxDecode /DecodeParms << /Columns 4096 >>",
            &[0x35, 0x14],
            4096 * 2048 / 8,
        ),
    ] {
        let image = asset(dict, data);
        let (opened, used) =
            allocations::measure(|| image.open_with_budget(None, |bytes| bytes < min));
        assert!(
            matches!(opened, Err(hayro_interpret::RequestError::Budget)),
            "{dict}"
        );
        assert!(used.largest < min, "{dict}: {used:?}");
    }
}

#[test]
fn request_scratch_covers_filter_and_keyed_conversion_allocations() {
    for (dict, data, grid, window) in [
        (
            "/Width 8192 /Height 32 /BPC 8 /Filter /FlateDecode /DecodeParms << /Columns 8192 /Predictor 12 >>",
            include_bytes!("fixtures/predicted-rows.zlib").as_slice(),
            (8192, 32),
            [0, 0, 1, 1],
        ),
        (
            "/Width 65536 /Height 1 /BPC 4 /Mask [0 0]",
            &[0x12; 32768],
            (1, 1),
            [0, 0, 1, 1],
        ),
    ] {
        let image = asset(dict, data);
        let mut source = image.open(None).unwrap();
        let scratch = source.request_scratch(Plane::Image, grid).unwrap()(window);
        let (_, used) = allocations::measure(|| {
            drop(
                source
                    .request(&ImageRequest {
                        plane: Plane::Image,
                        grid,
                        window,
                        cancel: None,
                    })
                    .unwrap(),
            )
        });
        assert!(used.peak <= scratch, "{dict}: {used:?} > {scratch}");
        assert!(
            used.held <= source.checkpoint_bytes(),
            "{dict}: {used:?} > {}",
            source.checkpoint_bytes()
        );
    }
}

#[test]
fn scratch_covers_a_backward_read_after_failed_spill() {
    if std::env::var_os("HAYRO_TEST_FAILED_SPILL").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "scratch_covers_a_backward_read_after_failed_spill",
            ])
            .env("HAYRO_TEST_FAILED_SPILL", "1")
            .env("TMPDIR", std::env::current_exe().unwrap())
            .env("TMP", std::env::current_exe().unwrap())
            .env("TEMP", std::env::current_exe().unwrap())
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let image = asset(
        "/Width 8192 /Height 32 /BPC 8 /Filter /FlateDecode /DecodeParms << /Columns 8192 /Predictor 12 >>",
        include_bytes!("fixtures/predicted-rows.zlib"),
    );
    let mut source = image.open(None).unwrap();
    let grid = (8192, 32);
    for y in [31, 0] {
        let window = [0, y, 1, y + 1];
        let scratch = source.request_scratch(Plane::Image, grid).unwrap()(window);
        let (region, used) = allocations::measure(|| {
            source
                .request(&ImageRequest {
                    plane: Plane::Image,
                    grid,
                    window,
                    cancel: None,
                })
                .unwrap()
        });
        assert!(region.rows_decoded > 0);
        assert!(used.peak <= scratch, "{y}: {used:?} > {scratch}");
    }
}
