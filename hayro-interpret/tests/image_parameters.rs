//! Images whose filters' parameters many images share in one object stream: decoding
//! each image reads them where the document's table keeps them, which drawing the
//! image read once for the document, so the object stream is not decoded again for
//! every image decoded.

#![cfg(debug_assertions)]

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::hayro_syntax::work::OBJECT_STREAM_ENTRIES;
use hayro_interpret::{
    BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageAsset, ImageDrawProps,
    ImageRequest, InterpreterSettings, Plane, SoftMask,
};
use kurbo::BezPath;

mod common;

/// The image assets drawn, and the grey of each image decoded as it is drawn.
#[derive(Default)]
struct Drawn {
    assets: Vec<ImageAsset>,
    decoded: Vec<u8>,
}

impl<'a> Device<'a> for Drawn {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, image: Image<'a, '_>, _: ImageDrawProps<'a>) {
        let Image::Raster(raster) = image else {
            panic!("a raster image is drawn");
        };
        self.assets.push(raster.asset());
        raster.with_rgba(
            |image, _| {
                if let hayro_interpret::ImageData::Luma(luma) = image {
                    self.decoded.push(luma.data[0]);
                }
            },
            None,
        );
    }
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// One page drawing `n` 1 × 1 grey images, image `i` of grey `i`, each with its
/// `/DecodeParms` in one object stream holding those of all of them.
fn images_sharing_an_object_stream(n: usize) -> Pdf {
    let content: String = (0..n)
        .map(|i| format!("q 10 0 0 10 0 0 cm /Im{i} Do Q "))
        .collect();
    let images: String = (0..n).map(|i| format!("/Im{i} {} 0 R ", 5 + i)).collect();
    let mut objects = common::one_page_objects(
        100,
        &format!("/Contents 4 0 R /Resources << /XObject << {images}>> >>"),
    );
    objects.push(common::stream("", content.as_bytes()));
    // The parameters follow the images, numbered from 5 + n.
    for i in 0..n {
        objects.push(common::stream(
            &format!(
                "/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray \
                 /BitsPerComponent 8 /Filter /FlateDecode /DecodeParms {} 0 R",
                5 + n + i
            ),
            &common::stored_zlib(&[i as u8], 64),
        ));
    }
    let parameters = vec![b"<< /Predictor 1 >>".to_vec(); n];
    Pdf::new(common::file_with_object_stream(&objects, &parameters)).expect("valid pdf")
}

/// The entries of object stream headers read drawing the page of `n` images, and
/// decoding each image's asset after it was drawn.
fn entries_read(n: usize) -> (usize, usize) {
    let pdf = images_sharing_an_object_stream(n);
    OBJECT_STREAM_ENTRIES.set(0);
    let mut drawn = Drawn::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut drawn);
    let drawing = OBJECT_STREAM_ENTRIES.get();
    assert_eq!(drawn.decoded, (0..n as u8).collect::<Vec<_>>());
    OBJECT_STREAM_ENTRIES.set(0);
    for (i, asset) in drawn.assets.iter().enumerate() {
        let reader = asset.reader();
        let mut source = reader.open(None, None).expect("the image opens");
        let region = source
            .request(&ImageRequest {
                plane: Plane::Image,
                grid: (1, 1),
                window: [0, 0, 1, 1],
                cancel: None,
            })
            .expect("the image decodes");
        assert_eq!(region.data[0], i as u8);
    }
    (drawing, OBJECT_STREAM_ENTRIES.get())
}

/// Each image decoded read the object stream's whole header again: work in the
/// square of the images.
#[test]
fn images_sharing_an_object_stream_of_parameters_read_it_once() {
    let (drawing, decoding) = entries_read(16);
    let (drawing_twice, decoding_twice) = entries_read(32);
    assert_eq!((decoding, decoding_twice), (0, 0));
    assert!(
        drawing_twice <= 2 * drawing,
        "{drawing} entries read for 16 images, {drawing_twice} for 32"
    );
}
