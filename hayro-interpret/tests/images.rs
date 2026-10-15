//! Decoding an image costs memory in proportion to the data it holds, not to the size
//! its dictionary declares: an image 100 000 × 100 000 with a one-byte stream must
//! not reserve 10 GB before reading it. `RasterImage::pixels_key` tells images apart
//! that decode differently.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, CacheKey, ClipPath, Device, DrawMode, DrawProps, Image, ImageData, ImageDrawProps,
    ImageRequest, InterpreterSettings, Plane, SoftMask,
};
use kurbo::BezPath;

#[path = "../../hayro-syntax/tests/common/allocations.rs"]
mod allocations;
mod common;

/// The width and height of each colour plane, alpha plane and stencil decoded.
#[derive(Default)]
struct Decoded(Vec<(u32, u32)>);

impl<'a> Device<'a> for Decoded {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, image: Image<'a, '_>, _: ImageDrawProps<'a>) {
        match image {
            Image::Raster(r) => r.with_rgba(
                |image, alpha| {
                    self.0.push(match image {
                        ImageData::Rgb(d) => (d.width, d.height),
                        ImageData::Luma(d) => (d.width, d.height),
                    });
                    if let Some(a) = alpha {
                        self.0.push((a.width, a.height));
                    }
                },
                None,
            ),
            Image::Stencil(s) => s.with_stencil(|l, _| self.0.push((l.width, l.height)), None),
        }
    }
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
    fn knock_out(&mut self, _: &BezPath, _: kurbo::Affine, _: &DrawMode) {}
}

/// A 100 × 100 pt page with `content`, `resources`, and `objects` numbered from 5.
fn page(content: &str, resources: &str, objects: Vec<Vec<u8>>) -> Pdf {
    let mut all = vec![common::stream("", content.as_bytes())];
    all.extend(objects);
    let file = common::one_page(
        100,
        &format!("/Contents 4 0 R /Resources << {resources} >>"),
        all,
    );
    Pdf::new(file).expect("valid pdf")
}

fn image(dict: &str, data: &[u8]) -> Vec<u8> {
    common::stream(&format!("/Type /XObject /Subtype /Image {dict}"), data)
}

/// Draws the image `dict` with stream `data` (object 5) over the page, `extra` being
/// object 6 (a mask, say), and returns the sizes decoded, asserting that decoding
/// stayed under 64 MiB.
fn draw(dict: &str, data: &[u8], extra: Option<(&str, &[u8])>) -> Vec<(u32, u32)> {
    let mut objects = vec![image(dict, data)];
    objects.extend(extra.map(|(dict, data)| image(dict, data)));
    let pdf = page(
        "q 100 0 0 100 0 0 cm /Im Do Q",
        "/XObject << /Im 5 0 R >>",
        objects,
    );
    let mut decoded = Decoded::default();
    let ((), used) = allocations::measure(|| {
        common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut decoded);
    });
    assert!(used.peak < 64 << 20, "{dict}: peak {} bytes", used.peak);
    decoded.0
}

const HUGE: &str = "/Width 100000 /Height 100000";

#[test]
fn a_huge_gray_image_with_one_byte_costs_little() {
    let dict = format!("{HUGE} /ColorSpace /DeviceGray /BitsPerComponent 1");
    // Eight samples are far from a row of 100 000: nothing to draw.
    assert_eq!(draw(&dict, &[0xAA], None), []);
}

#[test]
fn a_huge_rgb_image_with_one_byte_costs_little() {
    let dict = format!("{HUGE} /ColorSpace /DeviceRGB /BitsPerComponent 4");
    assert_eq!(draw(&dict, &[0xAA], None), []);
}

#[test]
fn a_huge_stencil_with_one_byte_costs_little() {
    let dict = format!("{HUGE} /ImageMask true /BitsPerComponent 1");
    assert_eq!(draw(&dict, &[0xAA], None), []);
}

#[test]
fn huge_soft_masks_with_one_byte_cost_little() {
    let image = "/Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /SMask 6 0 R";
    for bpc in [1, 2] {
        let mask = format!("{HUGE} /ColorSpace /DeviceGray /BitsPerComponent {bpc}");
        // The colour is drawn without the mask, which holds no row.
        assert_eq!(
            draw(image, &[255, 0, 0], Some((&mask, &[0xAA]))),
            [(1, 1)],
            "{bpc}"
        );
    }
}

#[test]
fn a_huge_ccitt_image_with_one_byte_costs_little() {
    let dict = format!(
        "{HUGE} /ColorSpace /DeviceGray /BitsPerComponent 1 /Filter /CCITTFaxDecode \
         /DecodeParms << /K -1 /Columns 100000 /Rows 100000 >>"
    );
    // Whatever one byte decodes to, it is at most a few rows.
    for (_, height) in draw(&dict, &[0x00], None) {
        assert!(height < 100, "{height} rows");
    }
}

/// An image whose data stops in its second row keeps that row, padded, and drops
/// the rows the data does not reach.
#[test]
fn a_short_image_keeps_the_rows_its_data_holds() {
    let dict = "/Width 16 /Height 4 /ColorSpace /DeviceGray /BitsPerComponent 1";
    assert_eq!(draw(dict, &[0xFF, 0x00, 0xF0], None), [(16, 2)]);
}

/// The `pixels_key` and `cache_key` of each raster image drawn.
#[derive(Default)]
struct Keys(Vec<(Option<u128>, u128)>);

impl<'a> Device<'a> for Keys {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, image: Image<'a, '_>, _: ImageDrawProps<'a>) {
        if let Image::Raster(r) = image {
            self.0.push((r.pixels_key(), r.cache_key()));
        }
    }
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
    fn knock_out(&mut self, _: &BezPath, _: kurbo::Affine, _: &DrawMode) {}
}

/// Drawing one image object twice gives one key; another object with the same
/// dictionary, the same object under a transfer function, and an inline image give
/// other keys or none.
#[test]
fn pixels_keys_tell_decoded_images_apart() {
    let gray = "/Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8";
    let pdf = page(
        "/A Do /A Do /B Do q /Inv gs /A Do Q BI /W 1 /H 1 /CS /G /BPC 8 ID \x50 EI",
        "/XObject << /A 5 0 R /B 6 0 R >> /ExtGState << /Inv << /TR 7 0 R >> >>",
        vec![
            image(gray, &[0x40]),
            image(gray, &[0xC0]),
            b"<< /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1 >>".to_vec(),
        ],
    );
    let mut keys = Keys::default();
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut keys);
    let (k, cache): (Vec<_>, Vec<_>) = keys.0.into_iter().unzip();
    assert_eq!(k.len(), 5, "{k:?}");
    let (a, b, a_inverted) = (
        k[0].expect("a key"),
        k[2].expect("a key"),
        k[3].expect("a key"),
    );
    assert_eq!(k[1], Some(a));
    assert_eq!(cache[0], cache[1]);
    assert_ne!(cache[0], cache[2]);
    assert_ne!(a, b);
    assert_ne!(a, a_inverted);
    assert_eq!(k[4], None, "inline image");
}

/// The same object of two documents gives two keys.
#[test]
fn pixels_keys_tell_documents_apart() {
    let gray = "/Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8";
    let keys = |sample: u8| {
        let pdf = page(
            "/A Do",
            "/XObject << /A 5 0 R >>",
            vec![image(gray, &[sample])],
        );
        let mut keys = Keys::default();
        common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut keys);
        keys.0
    };
    let (a, b) = (keys(0x40), keys(0xC0));
    assert!(a[0].0.is_some());
    assert_ne!(a[0].0, b[0].0);
    assert_ne!(a[0].1, b[0].1);
}

/// What a region request of each raster image drawn allocated at most, and its
/// pixels: of `plane`, the grid and window `bins` gives for the plane's size.
struct Requested {
    plane: Plane,
    bins: Bins,
    regions: Vec<(usize, Vec<u8>)>,
}

/// The grid and window to request of a plane of a width and height.
type Bins = fn(u32, u32) -> ((u32, u32), [u32; 4]);

impl Requested {
    fn new(plane: Plane, bins: Bins) -> Self {
        Self {
            plane,
            bins,
            regions: Vec::new(),
        }
    }
}

impl<'a> Device<'a> for Requested {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, image: Image<'a, '_>, _: ImageDrawProps<'a>) {
        let Image::Raster(r) = image else { return };
        let asset = r.asset();
        let (data, used) = allocations::measure(|| {
            let mut source = asset.open(None, None).expect("streamed");
            let plane = *source.layout().plane(self.plane).expect("the plane");
            let (grid, window) = (self.bins)(plane.width, plane.height);
            let request = ImageRequest {
                plane: self.plane,
                grid,
                window,
                cancel: None,
            };
            source.request(&request).expect("a region").data
        });
        self.regions.push((used.peak, data));
    }
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
    fn knock_out(&mut self, _: &BezPath, _: kurbo::Affine, _: &DrawMode) {}
}

/// One output texel over rows two million samples wide, with a soft mask as wide,
/// holds pieces of the rows, not whole rows or a sum per column.
#[test]
fn a_region_of_wide_rows_holds_pieces_of_them() {
    let (w, h) = (2_000_000_u32, 2_u32);
    let gray = format!("/Width {w} /Height {h} /ColorSpace /DeviceGray /BitsPerComponent 8");
    let pdf = page(
        "q 100 0 0 100 0 0 cm /Im Do Q",
        "/XObject << /Im 5 0 R >>",
        vec![
            image(
                &format!("{gray} /SMask 6 0 R"),
                &vec![200; (w * h) as usize],
            ),
            image(&gray, &vec![255; (w * h) as usize]),
        ],
    );
    let mut requested = Requested::new(Plane::Image, |_, _| ((1, 1), [0, 0, 1, 1]));
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut requested);
    let [(peak, data)] = &requested.regions[..] else {
        panic!("one image")
    };
    assert_eq!(data, &[200, 255]);
    assert!(*peak < 1 << 20, "peak {peak} bytes");
}

/// The last texel of a bilevel mask 40 million samples wide whose data ends halfway
/// through its row holds a piece of the row, not the bytes between the data's end and
/// the texel.
#[test]
fn a_texel_past_a_masks_data_holds_a_piece() {
    let w = 40_000_000_u32;
    let pdf = page(
        "q 100 0 0 100 0 0 cm /Im Do Q",
        "/XObject << /Im 5 0 R >>",
        vec![
            image(
                "/Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /Mask 6 0 R",
                &[200],
            ),
            image(
                &format!("/Width {w} /Height 1 /ImageMask true"),
                &vec![0; (w / 16) as usize],
            ),
        ],
    );
    let mut requested = Requested::new(Plane::Mask, |w, h| ((w, h), [w - 1, 0, w, h]));
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut requested);
    let [(peak, data)] = &requested.regions[..] else {
        panic!("one image")
    };
    assert_eq!(data, &[0]);
    assert!(*peak < 1 << 20, "peak {peak} bytes");
}

/// The pixels and `pixels_key` of each raster image drawn.
#[derive(Default)]
struct Pixels(Vec<(Option<u128>, Vec<u8>)>);

impl<'a> Device<'a> for Pixels {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, image: Image<'a, '_>, _: ImageDrawProps<'a>) {
        if let Image::Raster(r) = image {
            let key = r.pixels_key();
            r.with_rgba(
                |image, _| {
                    self.0.push((
                        key,
                        match image {
                            ImageData::Rgb(d) => d.data,
                            ImageData::Luma(d) => d.data,
                        },
                    ));
                },
                None,
            );
        }
    }
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
    fn knock_out(&mut self, _: &BezPath, _: kurbo::Affine, _: &DrawMode) {}
}

/// An image drawn through two entries of the table that lead to it, its `/DecodeParms`
/// naming the second entry, has one key and the same pixels through both, in either
/// order: a reference from the image back to itself is a cycle by either entry, so
/// the predictor its own dictionary sets, which would make [65, 66] of [65, 1], is
/// not applied (`MuPDF`, `PyMuPDF` 1.28.2, reads [65, 1] through the first entry).
#[test]
fn an_image_drawn_through_two_entries_decodes_alike() {
    // zlib of [65, 1].
    const DATA: &[u8] = &[120, 156, 115, 100, 4, 0, 0, 133, 0, 67];
    for content in ["/A Do /B Do", "/B Do /A Do"] {
        let mut objects = common::one_page_objects(
            100,
            "/Contents 4 0 R /Resources << /XObject << /A 5 0 R /B 6 0 R >> >>",
        );
        objects.extend([
            common::stream("", content.as_bytes()),
            image(
                "/Width 2 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 \
                 /Filter /FlateDecode /DecodeParms 6 0 R /Predictor 2 /Columns 2",
                DATA,
            ),
        ]);
        let pdf = Pdf::new(common::file_with_aliases(objects, &[5])).expect("valid pdf");
        let mut pixels = Pixels::default();
        common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut pixels);
        let [first, second] = pixels.0.try_into().expect("two images");
        assert!(first.0.is_some(), "{content}");
        assert_eq!(first, second, "{content}");
        assert_eq!(first.1, [65, 1], "{content}");
    }
}
