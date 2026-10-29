//! Requesting a region of an image's asset costs memory in proportion to the region
//! and the data the image holds, not to the size its dictionary declares: an image
//! 100 000 × 100 000 with a one-byte stream must not reserve 10 GB. (`with_rgba` and
//! `with_stencil` hand over every row the image declares, as large as the image
//! intact.) `RasterImage::pixels_key` tells images apart that decode differently.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, CacheKey, ClipPath, Device, DrawMode, DrawProps, Image, ImageData, ImageDrawProps,
    ImageRequest, InterpreterSettings, Plane, SoftMask,
};
use kurbo::BezPath;

#[path = "../../hayro-syntax/tests/common/allocations.rs"]
mod allocations;
#[path = "../../hayro-syntax/tests/common/ceilings.rs"]
mod ceilings;
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

/// A region of 100 × 100 bins over `plane` of the image `dict` with stream `data`
/// (object 5), `extra` being object 6 (a mask, say), asserting that opening and
/// requesting it stayed under 64 MiB.
fn huge_region(dict: &str, data: &[u8], extra: Option<(&str, &[u8])>, plane: Plane) -> Vec<u8> {
    let mut objects = vec![image(dict, data)];
    objects.extend(extra.map(|(dict, data)| image(dict, data)));
    let pdf = page(
        "q 100 0 0 100 0 0 cm /Im Do Q",
        "/XObject << /Im 5 0 R >>",
        objects,
    );
    let mut requested = Requested::new(plane, |_, _| ((100, 100), [0, 0, 100, 100]));
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut requested);
    let [(peak, data)] = requested.regions.try_into().expect("one image");
    assert!(peak < 64 << 20, "{dict}: peak {peak} bytes");
    data
}

/// Eight samples are far from a row of 100 000: every row reads as stored zero, black.
#[test]
fn a_huge_gray_image_with_one_byte_costs_little() {
    let dict = format!("{HUGE} /ColorSpace /DeviceGray /BitsPerComponent 1");
    let region = huge_region(&dict, &[0xAA], None, Plane::Image);
    assert_eq!(region, vec![0; 100 * 100]);
}

#[test]
fn a_huge_rgb_image_with_one_byte_costs_little() {
    let dict = format!("{HUGE} /ColorSpace /DeviceRGB /BitsPerComponent 4");
    let region = huge_region(&dict, &[0xAA], None, Plane::Image);
    assert_eq!(region, vec![0; 3 * 100 * 100]);
}

/// A stencil's stored zero is painted.
#[test]
fn a_huge_stencil_with_one_byte_costs_little() {
    let dict = format!("{HUGE} /ImageMask true /BitsPerComponent 1");
    let region = huge_region(&dict, &[0xAA], None, Plane::Image);
    assert_eq!(region, vec![255; 100 * 100]);
}

/// A soft mask's stored zero hides the colour.
#[test]
fn huge_soft_masks_with_one_byte_cost_little() {
    let image = "/Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /SMask 6 0 R";
    for bpc in [1, 2] {
        let mask = format!("{HUGE} /ColorSpace /DeviceGray /BitsPerComponent {bpc}");
        let region = huge_region(image, &[255, 0, 0], Some((&mask, &[0xAA])), Plane::Mask);
        assert_eq!(region, vec![0; 100 * 100], "{bpc}");
    }
}

/// Images decoded whole hold the rows their data holds and one of stored zero, which
/// the rows past it repeat: 16-bit samples, a filter that is not streamed, and a soft
/// mask through one.
#[test]
fn huge_images_decoded_whole_with_little_data_cost_little() {
    let gray = |bits: u8, extra: &str| {
        format!("{HUGE} /ColorSpace /DeviceGray /BitsPerComponent {bits} {extra}")
    };
    for (dict, data, fill) in [
        (gray(16, ""), &b"\xff\xff"[..], 0),
        (gray(16, "/Decode [1 0]"), b"\xff\xff", 255),
        (gray(8, "/Filter /ASCIIHexDecode"), b"ff>", 0),
        (
            gray(8, "/Filter /ASCIIHexDecode /Decode [1 0]"),
            b"00>",
            255,
        ),
    ] {
        let region = huge_region(&dict, data, None, Plane::Image);
        assert_eq!(region, vec![fill; 100 * 100], "{dict}");
    }
    let image = "/Width 1 /Height 1 /ColorSpace /DeviceRGB /BitsPerComponent 8 /SMask 6 0 R";
    let mask = gray(8, "/Filter /ASCIIHexDecode");
    let region = huge_region(image, &[255, 0, 0], Some((&mask, b"ff>")), Plane::Mask);
    assert_eq!(region, vec![0; 100 * 100]);
}

/// Whatever one byte decodes to, it is at most a few rows; the rest are black.
#[test]
fn a_huge_ccitt_image_with_one_byte_costs_little() {
    let dict = format!(
        "{HUGE} /ColorSpace /DeviceGray /BitsPerComponent 1 /Filter /CCITTFaxDecode \
         /DecodeParms << /K -1 /Columns 100000 /Rows 100000 >>"
    );
    let region = huge_region(&dict, &[0x00], None, Plane::Image);
    assert_eq!(region[100..], vec![0; 99 * 100]);
}

/// An image whose data stops in its second row keeps its declared rows: the first
/// as its data holds it, and the rest, the second among them, stored zero.
#[test]
fn a_short_image_keeps_its_declared_rows() {
    let dict = "/Width 16 /Height 4 /ColorSpace /DeviceGray /BitsPerComponent 1";
    assert_eq!(draw(dict, &[0xFF, 0x00, 0xF0], None), [(16, 4)]);
    let stencil = "/Width 16 /Height 4 /ImageMask true";
    assert_eq!(draw(stencil, &[0xFF, 0x00, 0xF0], None), [(16, 4)]);
    let mut pixels = Pixels::default();
    let pdf = page(
        "q 100 0 0 100 0 0 cm /Im Do Q",
        "/XObject << /Im 5 0 R >>",
        vec![image(dict, &[0xFF, 0x00, 0xF0])],
    );
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut pixels);
    let mut expected = [[255; 8], [0; 8]].concat();
    expected.extend([0; 48]);
    assert_eq!(pixels.0[0].1, expected);
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

/// What a region request of each image drawn, raster or stencil mask, allocated at
/// most, and its pixels: of `plane`, the grid and window `bins` gives for the plane's
/// size.
struct Requested {
    plane: Plane,
    bins: Bins,
    regions: Vec<(usize, Vec<u8>)>,
    /// The bytes of decoder states each asset saved for later requests.
    saved: Vec<usize>,
}

/// The grid and window to request of a plane of a width and height.
type Bins = fn(u32, u32) -> ((u32, u32), [u32; 4]);

impl Requested {
    fn new(plane: Plane, bins: Bins) -> Self {
        Self {
            plane,
            bins,
            regions: Vec::new(),
            saved: Vec::new(),
        }
    }
}

impl<'a> Device<'a> for Requested {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, image: Image<'a, '_>, _: ImageDrawProps<'a>) {
        let asset = match image {
            Image::Raster(r) => r.asset(),
            Image::Stencil(s) => s.asset(),
        };
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
        self.saved
            .push(asset.checkpoint_bytes().expect("saved states"));
    }
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
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

/// The peak bytes of opening the image `dict` with `data` and requesting the bins
/// `bins` gives, and the bytes of the decoder states the request saved.
fn region_cost(dict: &str, data: &[u8], bins: Bins) -> (usize, usize) {
    let pdf = page(
        "q 100 0 0 100 0 0 cm /Im Do Q",
        "/XObject << /Im 5 0 R >>",
        vec![image(dict, data)],
    );
    let mut requested = Requested::new(Plane::Image, bins);
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut requested);
    let [(peak, _)] = requested.regions[..] else {
        panic!("one image")
    };
    (peak, requested.saved[0])
}

/// A small region of a large bilevel image or stencil mask with Flate data, or of a
/// CCITT image, costs memory in proportion to the region, not to the image (whose
/// decoding whole held a byte per pixel, 64 MiB for the larger): opening it and
/// requesting a tile at full size or a thumbnail holds pieces of rows (a CCITT row's
/// colour changes) and the bins, besides the Flate decoder states a request saves
/// for later ones (at most 2 MiB); a tile of an image four times as large holds the
/// same, but for a piece of a row twice as wide. Each peak stays under its ceiling.
#[test]
fn a_small_region_of_a_large_bilevel_image_costs_the_region() {
    let tile: Bins = |w, h| ((w, h), [w / 2, h - 64, w / 2 + 64, h]);
    let thumbnail: Bins = |_, _| ((64, 64), [0, 0, 64, 64]);
    let gray = "/ColorSpace /DeviceGray /BitsPerComponent 1";
    // What a thread allocates once, before the first image is measured, is not
    // counted.
    region_cost(
        &format!("{gray} /Width 16 /Height 16 /Filter /FlateDecode"),
        &common::stored_zlib(&[0; 32], 1000),
        |w, h| ((w, h), [0, 0, 1, 1]),
    );
    let mut measured = Vec::new();
    /// The data of an image `side` pixels square.
    type Data = fn(u32) -> Vec<u8>;
    let flate = |side: u32| {
        let packed = common::noise((side / 8 * side) as usize, side);
        common::stored_zlib(&packed, 65_535)
    };
    // Group 4 rows, each white like the one above (vertical mode, no offset): a
    // bit each.
    let white_g4 = |side: u32| vec![0xFF; side as usize / 8];
    let kinds: [(&str, String, Data); 3] = [
        ("image", format!("{gray} /Filter /FlateDecode"), flate),
        (
            "stencil",
            "/ImageMask true /Filter /FlateDecode".into(),
            flate,
        ),
        (
            "ccitt",
            format!("{gray} /Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns COLUMNS >>"),
            white_g4,
        ),
    ];
    for (name, kind, data) in kinds {
        let mut working = Vec::new();
        for side in [4096_u32, 8192] {
            let data = data(side);
            let kind = kind.replace("COLUMNS", &side.to_string());
            let dict = format!("{kind} /Width {side} /Height {side}");
            let (peak, saved) = region_cost(&dict, &data, tile);
            assert!(saved <= 2 << 20, "{name} {side}: {saved} bytes saved");
            working.push(peak - saved);
            measured.push((format!("{name}_{side}_tile_peak"), peak));
            if side == 4096 {
                let (peak, _) = region_cost(&dict, &data, thumbnail);
                measured.push((format!("{name}_{side}_thumbnail_peak"), peak));
            }
        }
        // The larger image's rows are 512 bytes wider, which a skipped piece holds.
        assert!(
            working[1] <= working[0] + 512,
            "{name}: {} bytes besides saved states for 4096, {} for 8192",
            working[0],
            working[1]
        );
        assert!(working[1] < 1 << 20, "{name}: {} bytes", working[1]);
    }
    let measured: Vec<_> = measured.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    ceilings::check_ceilings(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/ceilings/bilevel_regions.txt"
        ),
        "WRITE_CEILINGS=1 cargo test -p hayro-interpret --test images",
        &measured,
    );
}

/// A small region of each kind of low-bit image that streams costs memory in
/// proportion to the region, not to the image: colour of fewer than eight bits in
/// colour spaces other than `DeviceGray` and `RGB`, soft masks of fewer than eight
/// bits or through a decode array, read alongside the colour or on their own grid,
/// stencil masks through a decode array, and TIFF prediction of fewer than eight
/// bits. Decoding any of them whole held a byte or more per pixel (4 MiB and more
/// for the smaller); a tile of an image four times as large holds the same, but for
/// the pieces of rows twice as wide that are read past.
#[test]
fn a_small_region_of_each_low_bit_kind_costs_the_region() {
    let tile: Bins = |w, h| ((w, h), [w / 2, h - 64, w / 2 + 64, h]);
    region_cost(
        "/ColorSpace /DeviceGray /BitsPerComponent 1 /Width 16 /Height 16 /Filter /FlateDecode",
        &common::stored_zlib(&[0; 32], 1000),
        |w, h| ((w, h), [0, 0, 1, 1]),
    );
    let flate = |side: u32, bits: u32| {
        let packed = common::noise((side * bits / 8 * side) as usize, side + bits);
        common::stored_zlib(&packed, 65_535)
    };
    let gray = |bits: u32, extra: &str| {
        format!("/ColorSpace /DeviceGray /BitsPerComponent {bits} {extra} /Filter /FlateDecode")
    };
    /// A kind: its dictionary, its colour's bits per pixel, the plane requested, and
    /// its mask (object 6): dictionary and bits per pixel.
    type Kind = (&'static str, String, u32, Plane, Option<(String, u32)>);
    let kinds: [Kind; 7] = [
        (
            "cmyk_2",
            "/ColorSpace /DeviceCMYK /BitsPerComponent 2 /Filter /FlateDecode".into(),
            8,
            Plane::Image,
            None,
        ),
        (
            "lab_4",
            "/ColorSpace [/Lab << /WhitePoint [0.95 1 1.09] >>] /BitsPerComponent 4 \
             /Filter /FlateDecode"
                .into(),
            12,
            Plane::Image,
            None,
        ),
        (
            "smask_5",
            gray(1, "/SMask 6 0 R"),
            1,
            Plane::Image,
            Some((gray(5, ""), 5)),
        ),
        (
            "smask_1_decoded",
            gray(2, "/SMask 6 0 R"),
            2,
            Plane::Image,
            Some((gray(1, "/Decode [0.2 0.7]"), 1)),
        ),
        (
            "smask_3_own_grid",
            gray(1, "/SMask 6 0 R"),
            1,
            Plane::Mask,
            Some((gray(3, "/Decode [1 0]"), 3)),
        ),
        (
            "stencil_decoded",
            "/ImageMask true /Decode [0.2 0.8] /Filter /FlateDecode".into(),
            1,
            Plane::Image,
            None,
        ),
        (
            "tiff_2",
            gray(
                2,
                "/DecodeParms << /Predictor 2 /BitsPerComponent 2 /Columns COLUMNS >>",
            ),
            2,
            Plane::Image,
            None,
        ),
    ];
    for (name, dict, bits, plane, mask) in kinds {
        let mut working = Vec::new();
        for side in [1024_u32, 2048] {
            let dict = dict.replace("COLUMNS", &side.to_string());
            let mut objects = vec![image(
                &format!("{dict} /Width {side} /Height {side}"),
                &flate(side, bits),
            )];
            // A mask on its own grid is half the image's size.
            let mask_side = if plane == Plane::Mask { side / 2 } else { side };
            if let Some((mask, mask_bits)) = &mask {
                objects.push(image(
                    &format!("{mask} /Width {mask_side} /Height {mask_side}"),
                    &flate(mask_side, *mask_bits),
                ));
            }
            let pdf = page(
                "q 100 0 0 100 0 0 cm /Im Do Q",
                "/XObject << /Im 5 0 R >>",
                objects,
            );
            let mut requested = Requested::new(plane, tile);
            common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut requested);
            let [(peak, _)] = requested.regions[..] else {
                panic!("{name}: one image")
            };
            let saved = requested.saved[0];
            assert!(saved <= 2 << 20, "{name} {side}: {saved} bytes saved");
            working.push(peak - saved);
        }
        // The larger image's rows, and its mask's, are wider by as much again, which a
        // piece read past holds, and for TIFF prediction the encoded and decoded rows.
        let rows = if name.starts_with("tiff") { 3 } else { 1 };
        let wider = rows * 1024 * (bits + mask.as_ref().map_or(0, |m| m.1)) as usize / 8;
        assert!(
            working[1] <= working[0] + wider,
            "{name}: {} bytes besides saved states for 1024, {} for 2048",
            working[0],
            working[1]
        );
        assert!(working[1] < 1 << 20, "{name}: {} bytes", working[1]);
    }
}

/// A small region of a CCITT image that is indexed or has a mask, or of an image
/// whose mask is CCITT data, costs memory in proportion to the region, as other
/// CCITT images do: decoding the larger whole held 64 MiB of samples and as much
/// again of alpha (three times as much colour for the indexed image). A tile of an
/// image four times as large holds the same, but for a piece of a mask's row twice
/// as wide.
#[test]
fn a_small_region_of_an_indexed_or_masked_ccitt_image_costs_the_region() {
    let tile: Bins = |w, h| ((w, h), [w / 2, h - 64, w / 2 + 64, h]);
    region_cost(
        "/ColorSpace /DeviceGray /BitsPerComponent 1 /Width 16 /Height 16 /Filter /FlateDecode",
        &common::stored_zlib(&[0; 32], 1000),
        |w, h| ((w, h), [0, 0, 1, 1]),
    );
    let ccitt = "/Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns SIDE >>";
    let gray = "/ColorSpace /DeviceGray /BitsPerComponent 1";
    // Group 4 rows, each white like the one above (vertical mode, no offset): a
    // bit each.
    let white_g4 = |side: u32| vec![0xFF; side as usize / 8];
    let flate =
        |side: u32| common::stored_zlib(&common::noise((side / 8 * side) as usize, side), 65_535);
    /// A kind: its dictionary and data, and its mask's (object 6).
    type Data = fn(u32) -> Vec<u8>;
    type Kind = (&'static str, String, Data, Option<(String, Data)>);
    let kinds: [Kind; 4] = [
        (
            "indexed",
            format!(
                "/ColorSpace [/Indexed /DeviceRGB 1 <ff0000 0000ff>] /BitsPerComponent 1 {ccitt}"
            ),
            white_g4,
            None,
        ),
        (
            "smask",
            format!("{gray} /SMask 6 0 R {ccitt}"),
            white_g4,
            Some((format!("{gray} /Filter /FlateDecode"), flate)),
        ),
        (
            "ccitt_smask",
            format!("{gray} /SMask 6 0 R /Filter /FlateDecode"),
            flate,
            Some((format!("{gray} {ccitt}"), white_g4)),
        ),
        (
            "ccitt_stencil_mask",
            format!("{gray} /Mask 6 0 R {ccitt}"),
            white_g4,
            Some((format!("/ImageMask true {ccitt}"), white_g4)),
        ),
    ];
    for (name, dict, data, mask) in kinds {
        let mut working = Vec::new();
        for side in [4096_u32, 8192] {
            let size = format!("/Width {side} /Height {side}");
            let dict = dict.replace("SIDE", &side.to_string());
            let mut objects = vec![image(&format!("{dict} {size}"), &data(side))];
            if let Some((mask, mask_data)) = &mask {
                let mask = mask.replace("SIDE", &side.to_string());
                objects.push(image(&format!("{mask} {size}"), &mask_data(side)));
            }
            let pdf = page(
                "q 100 0 0 100 0 0 cm /Im Do Q",
                "/XObject << /Im 5 0 R >>",
                objects,
            );
            let mut requested = Requested::new(Plane::Image, tile);
            common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut requested);
            let [(peak, _)] = requested.regions[..] else {
                panic!("{name}: one image")
            };
            let saved = requested.saved[0];
            assert!(saved <= 2 << 20, "{name} {side}: {saved} bytes saved");
            working.push(peak - saved);
        }
        // The larger image's rows are 512 bytes wider, which a piece of a bilevel
        // row read past holds.
        assert!(
            working[1] <= working[0] + 512,
            "{name}: {} bytes besides saved states for 4096, {} for 8192",
            working[0],
            working[1]
        );
        assert!(working[1] < 1 << 20, "{name}: {} bytes", working[1]);
    }
}

/// The last texel of a bilevel mask 40 million samples wide whose data ends halfway
/// through its row reads as stored zero, as the whole row does, holding a piece of
/// the row at most, not the bytes between the data's end and the texel.
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
                &vec![0xFF; (w / 16) as usize],
            ),
        ],
    );
    let mut requested = Requested::new(Plane::Mask, |w, h| ((w, h), [w - 1, 0, w, h]));
    common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut requested);
    let [(peak, data)] = &requested.regions[..] else {
        panic!("one image")
    };
    assert_eq!(data, &[255]);
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
