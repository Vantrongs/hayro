//! What decoding an image reads besides its data is read once, as the image is drawn,
//! and only what decoding uses: decoding reads no object of the document, so
//! parameters many images share in one object stream are not read again for every
//! image decoded, the document open or not, and keys of the parameters no filter
//! reads are never resolved.

#![cfg(debug_assertions)]

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::hayro_syntax::work::{BODIES_READ, OBJECT_STREAM_ENTRIES};
use hayro_interpret::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageAsset, ImageDrawProps,
    ImageRequest, InterpreterCache, InterpreterSettings, PixelFormat, Plane, SoftMask,
    interpret_page,
};
use kurbo::{Affine, BezPath, Rect};

#[path = "../../hayro-syntax/tests/common/allocations.rs"]
mod allocations;
mod common;

/// The image assets drawn, and the grey of each image decoded as it is drawn: of its
/// first pixel, and of all.
#[derive(Default)]
struct Drawn {
    assets: Vec<ImageAsset>,
    decoded: Vec<u8>,
    pixels: Vec<Vec<u8>>,
}

impl<'a> Device<'a> for Drawn {
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
    fn draw_image(&mut self, image: Image<'a, '_>, _: ImageDrawProps<'a>) {
        let Image::Raster(raster) = image else {
            panic!("a raster image is drawn");
        };
        self.assets.push(raster.asset());
        raster.with_rgba(
            |image, _| {
                if let hayro_interpret::ImageData::Luma(luma) = image {
                    self.decoded.push(luma.data[0]);
                    self.pixels.push(luma.data);
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
        interpret_page(page, &mut ctx, &mut drawn);
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
/// A key the image's filter does not read, here one only other filters read
/// (`/EarlyChange` is LZW's), is not resolved.
#[test]
fn ignored_parameters_are_not_resolved() {
    for key in ["/Ignored", "/EarlyChange"] {
        let content = b"q 10 0 0 10 0 0 cm /Im Do Q";
        let mut objects = common::one_page_objects(
            10,
            "/Contents 4 0 R /Resources << /XObject << /Im 5 0 R >> >>",
        );
        objects.push(common::stream("", content));
        // The object stream holding object 6, which only the ignored key names, follows.
        objects.push(common::stream(
            &format!(
                "/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray \
                 /BitsPerComponent 8 /Filter /FlateDecode \
                 /DecodeParms << /Predictor 1 {key} 6 0 R >>"
            ),
            &common::stored_zlib(&[9], 64),
        ));
        let padding = vec![b"0".to_vec(); 1024];
        let pdf = Pdf::new(common::file_with_object_stream(&objects, &padding)).expect("valid pdf");
        OBJECT_STREAM_ENTRIES.set(0);
        let mut drawn = Drawn::default();
        common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut drawn);
        assert_eq!(drawn.decoded, [9], "{key}");
        decode_assets(&drawn.assets, &[9]);
        assert_eq!(OBJECT_STREAM_ENTRIES.get(), 0, "{key}");
    }
}

/// The greys of the `width` × 1 image `asset` decodes to.
fn decode_row(asset: &ImageAsset, width: u32) -> Vec<u8> {
    let mut source = asset.open(None, None).expect("the image opens");
    let region = source
        .request(&ImageRequest {
            plane: Plane::Image,
            grid: (width, 1),
            window: [0, 0, width, 1],
            cancel: None,
        })
        .expect("the image decodes");
    region.data.to_vec()
}

/// The object stream entries read drawing, then decoding, an 8 × 1 CCITT image whose
/// `/Rows` is `rows`, where object 6 lies in an object stream no other object uses;
/// and the greys drawn and decoded.
fn ccitt_image(rows: &str) -> (usize, Vec<u8>, Vec<u8>) {
    let content = b"q 10 0 0 10 0 0 cm /Im Do Q";
    let mut objects = common::one_page_objects(
        10,
        "/Contents 4 0 R /Resources << /XObject << /Im 5 0 R >> >>",
    );
    objects.push(common::stream("", content));
    objects.push(common::stream(
        &format!(
            "/Type /XObject /Subtype /Image /Width 8 /Height 1 /ColorSpace /DeviceGray \
             /BitsPerComponent 1 /Filter /CCITTFaxDecode \
             /DecodeParms << /K 0 /Columns 8 /Rows {rows} >>"
        ),
        &[0x35, 0x14],
    ));
    let mut padding = vec![b"0".to_vec(); 1024];
    padding[0] = b"1".to_vec();
    let pdf = Pdf::new(common::file_with_object_stream(&objects, &padding)).expect("valid pdf");
    OBJECT_STREAM_ENTRIES.set(0);
    let mut drawn = Drawn::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut drawn);
    let [drawn_pixels] = <[Vec<u8>; 1]>::try_from(drawn.pixels).expect("one image drawn");
    let decoded = decode_row(&drawn.assets[0], 8);
    (OBJECT_STREAM_ENTRIES.get(), drawn_pixels, decoded)
}

/// The `/Rows` of a CCITT image's parameters is read only where its dictionary gives
/// no height: an image with one draws reading no object stream its `/Rows` refers
/// into, and the same pixels as with `/Rows` written in place.
#[test]
fn ccitt_rows_are_not_resolved_beside_a_height() {
    let (entries, drawn, decoded) = ccitt_image("6 0 R");
    let (_, expected_drawn, expected_decoded) = ccitt_image("1");
    assert_eq!(expected_drawn.len(), 8);
    assert_eq!((&drawn, &decoded), (&expected_drawn, &expected_decoded));
    assert_eq!(entries, 0);
}

/// The bytes allocated drawing a page that places one 1 × 1 grey image `placements`
/// times, the image passing through `filters` /Crypt filters (which leave an
/// unencrypted document's data as stored), and the grey of each placement.
fn placed_image(placements: usize, filters: usize) -> (usize, Vec<u8>) {
    let content: String = (0..placements)
        .map(|_| "q 10 0 0 10 0 0 cm /Im Do Q ")
        .collect();
    let mut objects = common::one_page_objects(
        10,
        "/Contents 4 0 R /Resources << /XObject << /Im 5 0 R >> >>",
    );
    objects.push(common::stream("", content.as_bytes()));
    objects.push(common::stream(
        &format!(
            "/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray \
             /BitsPerComponent 8 /Filter [{}]",
            "/Crypt ".repeat(filters)
        ),
        &[128],
    ));
    let pdf = Pdf::new(common::file(&objects, "")).expect("valid pdf");
    let mut drawn = Drawn::default();
    let ((), used) = allocations::measure(|| {
        common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut drawn);
    });
    (used.allocated, drawn.decoded)
}

/// Every placement of an image shares what decoding it reads, read once: drawing
/// twice the placements of an image with twice the filters allocates twice the
/// bytes (each placement read the filters again, four times the bytes).
#[test]
fn an_image_placed_many_times_is_read_once() {
    let (small, small_greys) = placed_image(100, 100);
    let (large, large_greys) = placed_image(200, 200);
    assert_eq!((small_greys, large_greys), (vec![128; 100], vec![128; 200]));
    assert!(
        large <= small * 22 / 10,
        "{small} bytes allocated for 100 placements of 100 filters, {large} for 200"
    );
}

/// The pixels of each image drawn, decoded as drawn and from its asset: a raster
/// image's colour and alpha, a stencil mask's alpha.
#[derive(Default)]
struct Pixels {
    drawn: Vec<Vec<u8>>,
    assets: Vec<ImageAsset>,
}

impl<'a> Device<'a> for Pixels {
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
    fn draw_image(&mut self, image: Image<'a, '_>, _: ImageDrawProps<'a>) {
        match image {
            Image::Raster(raster) => {
                self.assets.push(raster.asset());
                raster.with_rgba(
                    |image, alpha| {
                        let mut pixels = match image {
                            hayro_interpret::ImageData::Luma(luma) => luma.data,
                            hayro_interpret::ImageData::Rgb(rgb) => rgb.data,
                        };
                        pixels.extend(alpha.map(|a| a.data).unwrap_or_default());
                        self.drawn.push(pixels);
                    },
                    None,
                );
            }
            Image::Stencil(stencil) => {
                self.assets.push(stencil.asset());
                stencil.with_stencil(|luma, _| self.drawn.push(luma.data), None);
            }
        }
    }
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// A 2 × 1 image: the entries of its dictionary that tell its samples, and its data.
struct Sampled(String, Vec<u8>);

/// A raster image of greys 0x40 and 0xC0.
fn raster() -> Sampled {
    let dict = "/ColorSpace /DeviceGray /BitsPerComponent 8";
    Sampled(dict.into(), vec![0x40, 0xC0])
}

/// A stencil mask of bits 1 0.
fn stencil() -> Sampled {
    Sampled("/ImageMask true".into(), vec![0x80])
}

/// [`raster`], its samples a JPEG 2000 codestream holds.
fn jpx() -> Sampled {
    let dict = "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /JPXDecode";
    Sampled(dict.into(), GREYS_J2K.to_vec())
}

/// Greys 0x40 and 0xC0, 2 × 1, coded losslessly with one resolution level.
const GREYS_J2K: [u8; 86] = [
    0xff, 0x4f, 0xff, 0x51, 0x00, 0x29, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x07, 0x01, 0x01, 0xff, 0x52, 0x00,
    0x0c, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x04, 0x04, 0x00, 0x01, 0xff, 0x5c, 0x00, 0x04, 0x40,
    0x40, 0xff, 0x90, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x13, 0x00, 0x01, 0xff, 0x93, 0xcf,
    0xb4, 0x08, 0x0a, 0x0f, 0xff, 0xd9,
];

/// A JPEG of greys 0x40 and 0xC0, or of two colours, without a colour space: its
/// samples' components are those the JPEG has.
fn jpeg(color: jpeg_encoder::ColorType, samples: &[u8]) -> Sampled {
    let mut data = Vec::new();
    jpeg_encoder::Encoder::new(&mut data, 100)
        .encode(samples, 2, 1, color)
        .expect("a JPEG");
    Sampled("/BitsPerComponent 8 /Filter /DCTDecode".into(), data)
}

/// The object stream entries read drawing, then decoding, the 2 × 1 `image` with
/// `entries` in its dictionary; and its pixels drawn and from its asset. Object 6 is
/// a 2 × 1 soft mask of greys 255 and 0; object 7, `[0 255]`, a colour key that masks
/// every grey, lies in an object stream of 1024 objects no other object uses;
/// `mask_entries` go into object 6's dictionary.
fn masked_image(image: &Sampled, entries: &str, mask_entries: &str) -> (usize, Vec<Vec<u8>>) {
    let (read, pixels) = masked_image_read(image, entries, mask_entries);
    (read.object_stream_entries, pixels)
}

/// What drawing, then decoding, an image read of what its dictionary refers to.
#[derive(Debug, PartialEq)]
struct Read {
    /// Entries of object streams' headers ([`OBJECT_STREAM_ENTRIES`]).
    object_stream_entries: usize,
    /// Bytes of objects' bodies, its own included ([`BODIES_READ`]).
    bodies: usize,
}

/// [`masked_image`], with all it read.
fn masked_image_read(image: &Sampled, entries: &str, mask_entries: &str) -> (Read, Vec<Vec<u8>>) {
    let content = b"q 10 0 0 10 0 0 cm /Im Do Q";
    let mut objects = common::one_page_objects(
        10,
        "/Contents 4 0 R /Resources << /XObject << /Im 5 0 R >> >>",
    );
    objects.push(common::stream("", content));
    let Sampled(samples, data) = image;
    objects.push(common::stream(
        &format!("/Type /XObject /Subtype /Image /Width 2 /Height 1 {samples} {entries}"),
        data,
    ));
    let image = "/Type /XObject /Subtype /Image /Width 2 /Height 1";
    objects.push(common::stream(
        &format!("{image} /ColorSpace /DeviceGray /BitsPerComponent 8 {mask_entries}"),
        &[255, 0],
    ));
    let mut padding = vec![b"0".to_vec(); 1024];
    padding[0] = b"[0 255]".to_vec();
    let pdf = Pdf::new(common::file_with_object_stream(&objects, &padding)).expect("valid pdf");
    OBJECT_STREAM_ENTRIES.set(0);
    BODIES_READ.set(0);
    let mut drawn = Pixels::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut drawn);
    let mut pixels = drawn.drawn;
    pixels.extend(drawn.assets.iter().map(|asset| {
        let mut source = asset.open(None, None).expect("the image opens");
        let region = source
            .request(&ImageRequest {
                plane: Plane::Image,
                grid: (2, 1),
                window: [0, 0, 2, 1],
                cancel: None,
            })
            .expect("the image decodes");
        region.data.to_vec()
    }));
    assert_eq!(pixels.len(), 2, "{entries}");
    let read = Read {
        object_stream_entries: OBJECT_STREAM_ENTRIES.get(),
        bodies: BODIES_READ.get(),
    };
    (read, pixels)
}

/// A mask decoding cannot use is not resolved: a stencil mask's `/SMask` and `/Mask`
/// (it is decoded alone), and a raster image's `/Mask` beside an `/SMask` stream,
/// which takes precedence, or beside `/SMaskInData 1` for JPX data, which takes the
/// alpha from the data. The image draws reading no object stream that mask refers
/// into, and the same pixels as without it.
#[test]
fn masks_decoding_cannot_use_are_not_resolved() {
    // The colour key is used where nothing takes precedence: the fixture reads it.
    for image in [raster(), jpx()] {
        let (entries, keyed) = masked_image(&image, "/Mask 7 0 R", "");
        let (_, plain) = masked_image(&image, "", "");
        assert!(entries > 0 && keyed != plain, "{entries}, {keyed:?}");
    }
    // Each case, the entries it read, and whether its pixels are those without the mask.
    let read: Vec<_> = [
        ("stencil", stencil(), "/SMask 7 0 R", ""),
        ("stencil", stencil(), "/Mask 7 0 R", ""),
        ("raster", raster(), "/Mask 7 0 R", "/SMask 6 0 R"),
        ("jpx", jpx(), "/Mask 7 0 R", "/SMaskInData 1"),
    ]
    .into_iter()
    .map(|(kind, image, unused, beside)| {
        let (entries, pixels) = masked_image(&image, &format!("{beside} {unused}"), "");
        let (_, expected) = masked_image(&image, beside, "");
        let case = format!("{kind}: {unused} beside {beside:?}");
        (case, entries, pixels == expected)
    })
    .collect();
    assert!(read.iter().all(|r| r.1 == 0 && r.2), "{read:#?}");
}

/// Under `/SMaskInData 1` the soft mask of JPX data without `/Matte` serves no
/// decoding: what its dictionary refers to, here a `/Decode` in an otherwise unused
/// object stream, is not resolved, and the image draws as with a plain soft mask
/// under the same flag.
#[test]
fn a_soft_mask_beside_alpha_in_the_data_is_not_read() {
    let beside = "/SMaskInData 1 /SMask 6 0 R";
    let (entries, pixels) = masked_image(&jpx(), beside, "/Decode 7 0 R");
    let (_, expected) = masked_image(&jpx(), beside, "");
    assert_eq!((entries, pixels), (0, expected));
}

/// An array of an image without a colour space is read for the components its data
/// may have. Of data no DCT or JPX filter decodes, which are gray, its entries past
/// one component's are not read, here a reference into an object stream no other
/// object uses, and the image draws as without them. Of a JPEG or JPEG 2000 image,
/// whose components only decoding learns, its first 8 entries are read, for four
/// components, a reference among them included, and none past them, however many
/// follow; the image draws as with the array cut to its own components' entries.
#[test]
fn arrays_of_an_image_without_a_colour_space_are_read_for_its_data() {
    use jpeg_encoder::ColorType;
    let raw = Sampled("/BitsPerComponent 8".into(), vec![0x40, 0xC0]);
    let gray = jpeg(ColorType::Luma, &[0x40, 0xC0]);
    let gray_jpx = Sampled("/Filter /JPXDecode".into(), GREYS_J2K.to_vec());
    // `/Decode [1 0]` inverts the greys: the fixture reads the array.
    for image in [&raw, &gray, &gray_jpx] {
        let (_, inverted) = masked_image(image, "/Decode [1 0]", "");
        assert_ne!(inverted, masked_image(image, "", "").1);
    }
    // Each case: the image, its entries and its mask's, and those without the reference.
    let cases = [
        ("/Decode [1 0 7 0 R]", "", "/Decode [1 0]", ""),
        ("/Mask [0 100 7 0 R]", "", "/Mask [0 100]", ""),
        (
            "/SMask 6 0 R",
            "/Matte [0.5 0.5 7 0 R]",
            "/SMask 6 0 R",
            "/Matte [0.5 0.5]",
        ),
    ];
    let read: Vec<_> = cases
        .iter()
        .map(|&(entries, mask, without, mask_without)| {
            let (read, pixels) = masked_image(&raw, entries, mask);
            let (_, expected) = masked_image(&raw, without, mask_without);
            (format!("{entries} {mask}"), read, pixels == expected)
        })
        .collect();
    assert!(read.iter().all(|r| r.1 == 0 && r.2), "{read:#?}");
    // Each case: the entries read for four components, as the image's entries or its
    // mask's, and those a gray image reads. A reference among them is resolved.
    let cases = [
        ("/Decode", "1 0 7 0 R 0 0 0 0 0", "/Decode [1 0]"),
        ("/Mask", "0 100 0 0 0 0 0 0", "/Mask [0 100]"),
        ("/Matte", "0.5 0.5 0.5 0.5 7 0 R", "/Matte [0.5 0.5]"),
    ];
    // `n` entries after those: references to as many objects of the object stream,
    // or, to tell what reading them reads, numbers of as many bytes.
    let suffix = |n: usize, last: &str| -> String {
        (0..n)
            .map(|i| format!(" {} 0 {last}", 100 + i % 900))
            .collect()
    };
    for (kind, image) in [("gray JPEG", &gray), ("gray JPX", &gray_jpx)] {
        for (key, prefix, cut) in cases {
            let read = |entries: String| match key {
                "/Matte" => masked_image_read(image, "/SMask 6 0 R", &entries),
                _ => masked_image_read(image, &entries, ""),
            };
            let (_, expected) = read(cut.into());
            let mut entries_read = Vec::new();
            for n in [1, 10_000, 1_000_000] {
                let case = format!("{kind}: {key} [{prefix}] and {n} more");
                let (with_references, pixels) = read(format!("{key} [{prefix}{}]", suffix(n, "R")));
                let (with_numbers, _) = read(format!("{key} [{prefix}{}]", suffix(n, "0")));
                assert_eq!(with_references, with_numbers, "{case}");
                assert_eq!(pixels, expected, "{case}");
                entries_read.push(with_references.object_stream_entries);
            }
            let resolves = prefix.contains(" R");
            assert!(
                entries_read
                    .iter()
                    .all(|&e| (e > 0) == resolves && e == entries_read[0]),
                "{kind}: {key} [{prefix}]: {entries_read:?}"
            );
        }
    }
}

/// A region an asset gives: its format and texels.
type Region = (PixelFormat, Vec<u8>);

/// The pixels a `w` × `h` grey image with `entries` and `data` draws, and the
/// regions of the whole image and of its second row its asset gives; object 6, where
/// `mask` gives one, is a 2 × 2 grey image of its entries and data.
fn drawn_and_requested(
    (w, h): (u32, u32),
    entries: &str,
    data: &[u8],
    mask: Option<(&str, &[u8])>,
) -> (Vec<Vec<u8>>, Vec<Region>) {
    let content = b"q 10 0 0 10 0 0 cm /Im Do Q";
    let mut objects = common::one_page_objects(
        10,
        "/Contents 4 0 R /Resources << /XObject << /Im 5 0 R >> >>",
    );
    objects.push(common::stream("", content));
    let grey = "/Type /XObject /Subtype /Image /ColorSpace /DeviceGray /BitsPerComponent 8";
    objects.push(common::stream(
        &format!("{grey} /Width {w} /Height {h} {entries}"),
        data,
    ));
    if let Some((mask_entries, mask_data)) = mask {
        objects.push(common::stream(
            &format!("{grey} /Width 2 /Height 2 {mask_entries}"),
            mask_data,
        ));
    }
    let pdf = Pdf::new(common::file(&objects, "")).expect("valid pdf");
    let mut drawn = Pixels::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut drawn);
    assert_eq!(drawn.assets.len(), 1, "{entries}");
    let mut source = drawn.assets[0].open(None, None).expect("the image opens");
    let regions = [[0, 0, w, h], [0, 1, w, 2]].map(|window| {
        let region = source
            .request(&ImageRequest {
                plane: Plane::Image,
                grid: (w, h),
                window,
                cancel: None,
            })
            .expect("the image decodes");
        (region.format, region.data)
    });
    (drawn.drawn, regions.into())
}

/// The alpha of each texel of a region of `format`.
fn alpha((format, data): &Region) -> Vec<u8> {
    match format {
        PixelFormat::LumaAlpha => data.iter().skip(1).step_by(2).copied().collect(),
        PixelFormat::Luma => vec![255; data.len()],
        other => panic!("a grey image gives {other:?}"),
    }
}

/// `/SMaskInData 1` takes the alpha from JPX data alone: `MuPDF` draws an image of
/// other data as without it (observed through `PyMuPDF` 1.28.2), its colour key and
/// mask streams applied, and the rows its data do not hold transparent where those
/// make them so, as hayro drew them before alpha in the data suppressed its masks.
/// Drawn whole and requested by region, each image gives the alpha `MuPDF` does.
#[test]
fn alpha_in_the_data_is_taken_from_jpx_data_alone() {
    let mask: &[u8] = &[255, 128, 255, 0];
    // Each image: its size, entries, data and mask, and the alpha of its texels.
    let images = [
        ((1, 2), "/Mask [0 0]", &[0xFF][..], None, [255, 0].to_vec()),
        (
            (2, 2),
            "/Mask 6 0 R",
            &[64, 192],
            Some(("/Matte [0.5]", mask)),
            vec![255, 128, 0, 0],
        ),
        ((2, 2), "/Mask [0 255]", &[64, 192], None, vec![0; 4]),
        (
            (2, 2),
            "/Mask [0 100]",
            &[64, 192, 10, 200],
            None,
            vec![0, 255, 0, 255],
        ),
        (
            (2, 2),
            "/SMask 6 0 R",
            &[64, 192, 10, 20],
            Some(("", mask)),
            vec![255, 128, 255, 0],
        ),
    ];
    for (size, entries, data, mask, expected) in images {
        let flagged = format!("/SMaskInData 1 {entries}");
        let (drawn, regions) = drawn_and_requested(size, &flagged, data, mask);
        let without = drawn_and_requested(size, entries, data, mask);
        assert_eq!((&drawn, &regions), (&without.0, &without.1), "{entries}");
        let row = expected.len() / 2;
        let alphas = (alpha(&regions[0]), alpha(&regions[1]));
        assert_eq!(
            alphas,
            (expected.clone(), expected[row..].to_vec()),
            "{entries}"
        );
    }
}

/// The image assets drawn, recorded without decoding them, as a display list does.
#[derive(Default)]
struct Recorded(Vec<ImageAsset>);

impl<'a> Device<'a> for Recorded {
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
    fn draw_image(&mut self, image: Image<'a, '_>, _: ImageDrawProps<'a>) {
        let Image::Raster(raster) = image else {
            panic!("a raster image is drawn");
        };
        self.0.push(raster.asset());
    }
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// A JPEG without a colour space behind Flate, whose data run on past the JPEG with
/// `padding` zeros, and whose `/Decode` is read for the components the JPEG may have,
/// is recorded without inflating its data: the work counted while its page is
/// interpreted, every byte Flate makes included, is the same whatever the padding,
/// and a small part of it. Its asset decodes the JPEG.
#[test]
fn an_image_without_a_colour_space_is_recorded_without_inflating_its_data() {
    use hayro_interpret::hayro_syntax::work::TOLD;
    use std::io::Write;
    let Sampled(_, jpeg) = jpeg(jpeg_encoder::ColorType::Luma, &[0x40, 0xC0]);
    let recorded = |padding: usize| {
        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        zlib.write_all(&jpeg).unwrap();
        zlib.write_all(&vec![0; padding]).unwrap();
        let data = zlib.finish().unwrap();
        let content = b"q 10 0 0 10 0 0 cm /Im Do Q";
        let mut objects = common::one_page_objects(
            10,
            "/Contents 4 0 R /Resources << /XObject << /Im 5 0 R >> >>",
        );
        objects.push(common::stream("", content));
        objects.push(common::stream(
            "/Type /XObject /Subtype /Image /Width 2 /Height 1 /BitsPerComponent 8 \
             /Filter [/FlateDecode /DCTDecode] /Decode [0 1]",
            &data,
        ));
        let pdf = Pdf::new(common::file(&objects, "")).expect("valid pdf");
        let mut recorded = Recorded::default();
        TOLD.set(0);
        common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut recorded);
        let told = TOLD.get();
        let [asset] = &recorded.0[..] else {
            panic!("one image is recorded");
        };
        let mut source = asset.open(None, None).expect("the image opens");
        let region = source
            .request(&ImageRequest {
                plane: Plane::Image,
                grid: (2, 1),
                window: [0, 0, 2, 1],
                cancel: None,
            })
            .expect("the image decodes");
        let greys: Vec<u8> = region.data.iter().map(|&g| g / 16).collect();
        (told, greys)
    };
    let padding = 16 << 20;
    let (small, small_greys) = recorded(1 << 20);
    let (large, large_greys) = recorded(padding);
    assert_eq!((&small_greys, &large_greys), (&vec![4, 12], &vec![4, 12]));
    assert!(small == large && large < padding / 64, "{small}, {large}");
}
