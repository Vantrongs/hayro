//! Decoding an image costs memory in proportion to the data it holds, not to the size
//! its dictionary declares: an image 100 000 × 100 000 with a one-byte stream must
//! not reserve 10 GB before reading it. `RasterImage::pixels_key` tells images apart
//! that decode differently.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, CacheKey, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageData,
    ImageDrawProps, InterpreterCache, InterpreterSettings, SoftMask, interpret_page,
};
use kurbo::{Affine, BezPath, Rect};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Counts the bytes in use and their peak, and refuses requests over 1 GiB, so a
/// decoder that sizes a buffer by the declared dimensions aborts the test instead of
/// exhausting the machine.
struct Capped;

static IN_USE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to `System`, only counting.
unsafe impl GlobalAlloc for Capped {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() > 1 << 30 {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller's contract is `System`'s.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = IN_USE.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        // SAFETY: `p` came from `alloc` above.
        unsafe { System.dealloc(p, layout) };
        IN_USE.fetch_sub(layout.size(), Relaxed);
    }
}

#[global_allocator]
static ALLOC: Capped = Capped;

/// Tests measure one at a time: the counters are global.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

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

/// A one-page PDF (100 × 100 pt) with `content`, `resources`, and `objects` (a
/// dictionary and, for a stream, its data) numbered from 5.
fn pdf(content: &str, resources: &str, objects: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
    let stream = |dict: &str, data: &[u8]| {
        let mut s = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        s.extend_from_slice(data);
        s.extend_from_slice(b"\nendstream");
        s
    };
    let mut objs = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R \
             /Resources << {resources} >> >>"
        )
        .into_bytes(),
        stream("", content.as_bytes()),
    ];
    for (dict, data) in objects {
        objs.push(match data {
            Some(data) => stream(dict, data),
            None => format!("<< {dict} >>").into_bytes(),
        });
    }
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// Interprets the first page of `pdf` into `device`.
fn interpret<'a>(pdf: &'a Pdf, device: &mut impl Device<'a>) {
    let page = &pdf.pages()[0];
    let cache = InterpreterCache::new();
    let mut ctx = Context::new(
        Affine::IDENTITY,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        &cache,
        page.xref(),
        InterpreterSettings::default(),
    );
    interpret_page(page, &mut ctx, device);
}

/// Draws the image `dict` with stream `data` (object 5) over the page, `extra` being
/// object 6 (a mask, say), and returns the sizes decoded, asserting that decoding
/// stayed under 64 MiB.
fn draw(dict: &str, data: &[u8], extra: Option<(&str, &[u8])>) -> Vec<(u32, u32)> {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let image = |dict: &str| format!("/Type /XObject /Subtype /Image {dict}");
    let mut objects = vec![(image(dict), Some(data))];
    if let Some((dict, data)) = extra {
        objects.push((image(dict), Some(data)));
    }
    let objects: Vec<_> = objects.iter().map(|(d, s)| (d.as_str(), *s)).collect();
    let pdf = Pdf::new(pdf(
        "q 100 0 0 100 0 0 cm /Im Do Q",
        "/XObject << /Im 5 0 R >>",
        &objects,
    ))
    .expect("valid pdf");
    let mut decoded = Decoded::default();
    let base = IN_USE.load(Relaxed);
    PEAK.store(base, Relaxed);
    interpret(&pdf, &mut decoded);
    let peak = PEAK.load(Relaxed) - base;
    assert!(peak < 64 << 20, "{dict}: peak {peak} bytes");
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
}

/// Drawing one image object twice gives one key; another object with the same
/// dictionary (whose `cache_key` is the same), the same object under a transfer
/// function, and an inline image give other keys or none.
#[test]
fn pixels_keys_tell_decoded_images_apart() {
    let gray = "/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray \
                /BitsPerComponent 8";
    let pdf = Pdf::new(pdf(
        "/A Do /A Do /B Do q /Inv gs /A Do Q BI /W 1 /H 1 /CS /G /BPC 8 ID \x50 EI",
        "/XObject << /A 5 0 R /B 6 0 R >> /ExtGState << /Inv << /TR 7 0 R >> >>",
        &[
            (gray, Some(&[0x40])),
            (gray, Some(&[0xC0])),
            ("/FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1", None),
        ],
    ))
    .expect("valid pdf");
    let mut keys = Keys::default();
    interpret(&pdf, &mut keys);
    let (k, cache): (Vec<_>, Vec<_>) = keys.0.into_iter().unzip();
    assert_eq!(k.len(), 5, "{k:?}");
    let (a, b, a_inverted) = (
        k[0].expect("a key"),
        k[2].expect("a key"),
        k[3].expect("a key"),
    );
    assert_eq!(k[1], Some(a));
    assert_eq!(cache[0], cache[2], "the same dictionary");
    assert_ne!(a, b);
    assert_ne!(a, a_inverted);
    assert_eq!(k[4], None, "inline image");
}
