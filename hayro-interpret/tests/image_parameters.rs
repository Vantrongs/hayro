//! What decoding an image reads besides its data is read once, as the image is drawn,
//! and only what decoding uses: decoding reads no object of the document, so
//! parameters many images share in one object stream are not read again for every
//! image decoded, the document open or not, and keys of the parameters no filter
//! reads are never resolved.

#![cfg(debug_assertions)]

use hayro_interpret::annotation::View;
use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::hayro_syntax::work::OBJECT_STREAM_ENTRIES;
use hayro_interpret::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageAsset, ImageDrawProps,
    ImageRequest, InterpreterCache, InterpreterSettings, Plane, SoftMask, interpret_page,
};
use kurbo::{Affine, BezPath, Rect};

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
/// decoding each image's asset after it was drawn, the document dropped.
fn entries_read(n: usize) -> (usize, usize) {
    let pdf = images_sharing_an_object_stream(n);
    OBJECT_STREAM_ENTRIES.set(0);
    let mut drawn = Drawn::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut drawn);
    let drawing = OBJECT_STREAM_ENTRIES.get();
    assert_eq!(drawn.decoded, (0..n as u8).collect::<Vec<_>>());
    drop(pdf);
    OBJECT_STREAM_ENTRIES.set(0);
    decode_assets(&drawn.assets, &(0..n as u8).collect::<Vec<_>>());
    (drawing, OBJECT_STREAM_ENTRIES.get())
}

/// Each image decoded once the document was gone read the object stream's whole
/// header again: work in the square of the images.
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

/// `pages` pages, each drawing one image, 1 × 1 and grey 7, which they share, whose
/// `/DecodeParms` hold, besides `/Predictor 1`, `ignored` keys no filter reads.
fn pages_sharing_parameters_with_ignored_keys(pages: usize, ignored: usize) -> Pdf {
    let kids: String = (0..pages).map(|i| format!("{} 0 R ", 3 + i)).collect();
    let (content, image, parameters) = (3 + pages, 4 + pages, 5 + pages);
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!("<< /Type /Pages /Kids [{kids}] /Count {pages} >>").into_bytes(),
    ];
    for _ in 0..pages {
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents {content} 0 R \
                 /Resources << /XObject << /Im {image} 0 R >> >> >>"
            )
            .into_bytes(),
        );
    }
    objects.push(common::stream("", b"q 10 0 0 10 0 0 cm /Im Do Q"));
    objects.push(common::stream(
        &format!(
            "/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray \
             /BitsPerComponent 8 /Filter /FlateDecode /DecodeParms {parameters} 0 R"
        ),
        &common::stored_zlib(&[7], 64),
    ));
    let keys: String = (0..ignored).map(|i| format!("/Ignored{i} {i} ")).collect();
    objects.push(format!("<< /Predictor 1 {keys}>>").into_bytes());
    Pdf::new(common::file(&objects, "")).expect("valid pdf")
}

/// The bytes examined drawing every page of `pdf` with one cache, each image
/// decoded as it is drawn and again from its asset.
fn examined_drawing_every_page(pdf: &Pdf) -> usize {
    use hayro_interpret::hayro_syntax::work::EXAMINED;
    let cache = InterpreterCache::new();
    EXAMINED.set(0);
    for page in pdf.pages().iter() {
        let mut ctx = Context::new(
            Affine::IDENTITY,
            Rect::new(0.0, 0.0, 10.0, 10.0),
            &cache,
            page.xref(),
            InterpreterSettings::default(),
        );
        let mut drawn = Drawn::default();
        interpret_page(page, &View::of(page), &mut ctx, &mut drawn);
        assert_eq!(drawn.decoded, [7]);
        decode_assets(&drawn.assets, &[7]);
    }
    EXAMINED.get()
}

/// Opens each of `assets` and checks that its one pixel is the grey `expected` gives.
fn decode_assets(assets: &[ImageAsset], expected: &[u8]) {
    assert_eq!(assets.len(), expected.len());
    for (asset, &grey) in assets.iter().zip(expected) {
        let mut source = asset.open(None, None).expect("the image opens");
        let region = source
            .request(&ImageRequest {
                plane: Plane::Image,
                grid: (1, 1),
                window: [0, 0, 1, 1],
                cancel: None,
            })
            .expect("the image decodes");
        assert_eq!(region.data[0], grey);
    }
}

/// Pages that share an image whose parameters hold keys no filter reads read only
/// the keys the filter reads: drawing twice the pages, the dictionary twice the
/// keys, takes twice the work (each page read every key, four times the work).
#[test]
fn ignored_parameters_shared_by_every_page_cost_linear_work() {
    let small = examined_drawing_every_page(&pages_sharing_parameters_with_ignored_keys(64, 64));
    let large = examined_drawing_every_page(&pages_sharing_parameters_with_ignored_keys(128, 128));
    assert!(
        large <= small * 22 / 10,
        "{small} bytes examined for 64 pages, {large} for 128"
    );
}

/// A key of an image's parameters that no filter reads is not resolved, nor is the
/// object stream it refers into decoded, drawing the image or decoding it.
#[test]
fn ignored_parameters_are_not_resolved() {
    let content = b"q 10 0 0 10 0 0 cm /Im Do Q";
    let mut objects = common::one_page_objects(
        10,
        "/Contents 4 0 R /Resources << /XObject << /Im 5 0 R >> >>",
    );
    objects.push(common::stream("", content));
    // The object stream holding object 6, which only the ignored key names, follows.
    objects.push(common::stream(
        "/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray \
         /BitsPerComponent 8 /Filter /FlateDecode \
         /DecodeParms << /Predictor 1 /Ignored 6 0 R >>",
        &common::stored_zlib(&[9], 64),
    ));
    let padding = vec![b"0".to_vec(); 1024];
    let pdf = Pdf::new(common::file_with_object_stream(&objects, &padding)).expect("valid pdf");
    OBJECT_STREAM_ENTRIES.set(0);
    let mut drawn = Drawn::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut drawn);
    assert_eq!(drawn.decoded, [9]);
    decode_assets(&drawn.assets, &[9]);
    assert_eq!(OBJECT_STREAM_ENTRIES.get(), 0);
}
