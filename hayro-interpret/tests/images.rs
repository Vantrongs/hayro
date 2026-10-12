//! Decoding an image costs memory in proportion to the data it holds, not to the size
//! its dictionary declares: an image 100 000 × 100 000 with a one-byte stream must
//! not reserve 10 GB before reading it.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageData, ImageDrawProps,
    InterpreterSettings, SoftMask,
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
