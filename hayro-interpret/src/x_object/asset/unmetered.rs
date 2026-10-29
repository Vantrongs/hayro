//! Work that grows with the data is done between two polls of a request's hook, a
//! piece at a time, never at once: for every shape of image a hostile file can hold,
//! opening it, requesting regions of it and decoding it whole allocate and copy at
//! most a few pieces' worth between two polls. The bytes are counted by this test
//! binary's allocator: those allocated, a copy's included, and those a reallocation
//! adds. The bytes a reallocation moves are not: the allocator moves them as it can
//! ([`Meter::reserve`]). Room made by [`Meter::room`] and [`Meter::reserve`] is
//! filled a counted piece at a time, so it counts as no work. Each poll also reads
//! the asset's statistics, which no lock held across a poll may block.

use super::tests::{asset, image, is_streamed, pdf, zlib};
use super::*;
use crate::chains::{self, Rng};
use crate::fixtures::noise;
use hayro_syntax::Pdf;
use hayro_syntax::object::{ObjectIdentifier, Stream};
use hayro_syntax::work::{POLL_BYTES, RESERVED};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};

/// How often the document parses an object's dictionary before it keeps it.
const PARSES: usize = 3;

/// What one stretch of work between two polls may allocate and copy: a few pieces
/// (read, decoded, converted), and a decoder's fixed state made after the poll that
/// charged it (a zlib decoder's state and window, some 40 KiB). The worst stretch
/// the shapes below make is some 160 KiB.
const BOUND: usize = 4 * POLL_BYTES + FIXED;
const FIXED: usize = 64 * 1024;

thread_local! {
    /// The bytes allocated on this thread, and those reallocations added, while
    /// counting.
    static COUNTS: Cell<Option<usize>> = const { Cell::new(None) };
}

fn count(allocated: usize) {
    let counting = COUNTS.try_with(|slot| {
        let Some(c) = slot.get() else {
            return false;
        };
        slot.set(Some(c + allocated));
        true
    });
    if counting == Ok(true) && allocated > POLL_BYTES {
        trace(allocated);
    }
}

thread_local! {
    static TRACING: Cell<bool> = const { Cell::new(false) };
}

/// With `UNMETERED_TRACE` set, prints where an allocation of `bytes`, more than a
/// piece, was made, unless the meter made it as room: to find the code a failure
/// names.
fn trace(bytes: usize) {
    if TRACING.replace(true) {
        return;
    }
    // What tracing allocates is not counted.
    let counts = COUNTS.with(|c| c.replace(None));
    if std::env::var_os("UNMETERED_TRACE").is_some() {
        let trace = std::backtrace::Backtrace::force_capture().to_string();
        if !trace.contains("src/work.rs") {
            let frames: Vec<_> = trace
                .lines()
                .filter(|l| l.contains("hayro") && !l.contains("unmetered"))
                .take(24)
                .collect();
            eprintln!("{bytes} bytes at\n{}", frames.join("\n"));
        }
    }
    COUNTS.with(|c| c.set(counts));
    TRACING.set(false);
}

struct Counting;

#[allow(unsafe_code)]
// SAFETY: forwards to `System`, only counting.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count(size.saturating_sub(layout.size()));
        // SAFETY: as for `alloc`; `ptr` came from this allocator.
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The work this thread did outside a meter's pieces so far: bytes allocated but
/// the room the meter made. Only its growth tells.
fn uncounted() -> i128 {
    let allocated = COUNTS.with(Cell::get).expect("counting");
    allocated as i128 - RESERVED.get() as i128
}

/// The most work done between two polls of a hook, by the operation it was polled
/// in.
#[derive(Default)]
struct Stretches {
    /// The work [`uncounted`] at the last poll.
    last: Cell<i128>,
    worst: RefCell<Vec<(String, usize)>>,
    operation: RefCell<&'static str>,
}

impl Stretches {
    /// Ends a stretch of the current operation, at a poll or at its end.
    fn end(&self) {
        let now = uncounted();
        let stretch = usize::try_from(now - self.last.replace(now)).unwrap_or(0);
        let operation = *self.operation.borrow();
        let mut worst = self.worst.borrow_mut();
        match worst.iter_mut().find(|(o, _)| o == operation) {
            Some((_, w)) => *w = (*w).max(stretch),
            None => worst.push((operation.to_owned(), stretch)),
        }
    }

    /// Runs `f` as `operation`, from a stretch's start.
    fn run<T>(&self, operation: &'static str, f: impl FnOnce() -> T) -> T {
        *self.operation.borrow_mut() = operation;
        self.last.set(uncounted());
        let result = f();
        self.end();
        result
    }
}

/// Opens, requests and decodes whole the image in object 3 of `pdf` with a hook
/// that ends a stretch at each poll and reads the asset's statistics; returns each
/// operation's largest stretch.
fn stretches(pdf: &Pdf) -> Vec<(String, usize)> {
    let asset = asset(pdf);
    // The process makes its CMYK profile once, on first use.
    ColorSpace::device_cmyk().to_rgba(&[0.0; 4], 1.0);
    // The document parses an object's dictionary a few times at most, however often
    // it is read, then keeps it: that parse is the document's, not the image's.
    for id in [3, 4] {
        for _ in 0..PARSES {
            drop(pdf.xref().get::<Stream<'_>>(ObjectIdentifier::new(id, 0)));
        }
    }
    COUNTS.with(|c| c.set(Some(0)));
    let s = Stretches::default();
    let hook = || {
        s.end();
        asset.checkpoint_bytes().expect("statistics");
        false
    };
    let hook: &dyn Fn() -> bool = &hook;
    let opened = s.run("open", || asset.open(None, Some(hook)));
    if let Ok(mut source) = opened {
        let streamed = is_streamed(&source);
        let layout = *source.layout();
        for plane in [Plane::Image, Plane::Mask] {
            let Some(p) = layout.plane(plane) else {
                continue;
            };
            let (w, h) = (p.width, p.height);
            let overview = (w.min(32), h.min(32));
            let corner = [w - w.min(64), h - 1, w, h];
            for (operation, grid, window) in [
                ("request overview", overview, [0, 0, overview.0, overview.1]),
                ("request last row", (w, h), corner),
                ("request last row again", (w, h), corner),
                ("request first row", (w, h), [0, 0, w.min(64), 1]),
                ("request last row once more", (w, h), corner),
            ] {
                let request = ImageRequest {
                    plane,
                    grid,
                    window,
                    cancel: Some(hook),
                };
                let _ = s.run(operation, || source.request(&request));
            }
        }
        if streamed {
            s.run("decode whole", || {
                let meter = Meter::new(Some(hook));
                let obj = asset.xobject().unwrap();
                if obj.kind == ImageKind::StencilMask {
                    drop(decode_mask(&obj, None, &meter));
                } else if let Some(decoded) = decode_image(&obj, None, &meter) {
                    drop(Decoded::new(decoded, &meter));
                }
            });
        }
    }
    drop(asset);
    COUNTS.with(|c| c.set(None));
    s.worst.into_inner()
}

/// Every shape in `shapes`, a name and a file, does no more work between two polls
/// than [`BOUND`].
fn assert_metered(shapes: Vec<(String, Pdf)>) {
    let mut over = Vec::new();
    for (name, pdf) in shapes {
        for (operation, worst) in stretches(&pdf) {
            println!("{name}: {operation}: {worst}");
            if worst > BOUND {
                over.push(format!("{name}: {operation}: {worst} bytes"));
            }
        }
    }
    assert!(
        over.is_empty(),
        "work between two polls:\n{}",
        over.join("\n")
    );
}

const GRAY1: &str = "/ColorSpace /DeviceGray /BitsPerComponent 1";

/// Each filter alone and in chains, and chains of up to 100,000 filters of each
/// kind over no data and over a little.
#[test]
fn filters_and_chains_of_them_are_metered() {
    let (w, h) = (2048, 2048);
    let packed = noise((w * h / 8) as usize, 3);
    let mut shapes = Vec::new();
    let mut rng = Rng(11);
    for length in [1, 2, 5, 17] {
        let mut data = packed.clone();
        let mut filters = Vec::new();
        let mut params = Vec::new();
        for _ in 0..length {
            let (filter, param, encoded) = loop {
                let layer = chains::layer(&data, &mut rng);
                if layer.0 != "/Unknown" {
                    break layer;
                }
            };
            filters.insert(0, filter);
            params.insert(0, param);
            data = encoded;
        }
        let dict = format!(
            "{GRAY1} /Filter [{}] /DecodeParms [{}]",
            filters.join(" "),
            params.join(" ")
        );
        shapes.push((
            format!("chain of {length}: {}", filters.join(" ")),
            pdf(&[(image(&dict, w, h), data)]),
        ));
    }
    for filter in [
        "/FlateDecode",
        "/LZWDecode",
        "/AHx",
        "/A85",
        "/RunLengthDecode",
    ] {
        for (length, data) in [(100_000, Vec::new()), (100_000, b"0".to_vec())] {
            let filters = [filter].repeat(length).join(" ");
            let dict = format!("{GRAY1} /Filter [{filters}]");
            shapes.push((
                format!("{length} x {filter} over {} bytes", data.len()),
                pdf(&[(image(&dict, 8, 8), data)]),
            ));
        }
    }
    assert_metered(shapes);
}

/// Data that ends within a pixel, of every colour space and depth, stored and behind
/// Flate: the samples held are kept, the texel the data ends within completed.
#[test]
fn data_ending_within_a_pixel_is_metered() {
    let spaces = [
        ("/DeviceGray", 1),
        ("/DeviceRGB", 3),
        ("/DeviceCMYK", 4),
        ("[/Indexed /DeviceRGB 1 <000000ffffff>]", 1),
        ("[/Lab << /WhitePoint [0.95 1 1.09] >>]", 3),
        (
            "[/Separation /A /DeviceGray << /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>]",
            1,
        ),
    ];
    let mut shapes = Vec::new();
    for (space, components) in spaces {
        for bpc in [1, 2, 4, 8, 16] {
            if space.starts_with("[/Indexed") && bpc == 16 {
                continue;
            }
            let w = 1024_u32;
            let row = (w as usize * components * bpc).div_ceil(8);
            let rows = (2 << 20) / row;
            let data = noise(rows * row + 1, 5);
            let dict = format!("/ColorSpace {space} /BitsPerComponent {bpc}");
            shapes.push((
                format!("{space} {bpc}-bit stored"),
                pdf(&[(image(&dict, w, 4 * rows as u32), data.clone())]),
            ));
            shapes.push((
                format!("{space} {bpc}-bit flate"),
                pdf(&[(
                    image(&format!("{dict} /Filter /FlateDecode"), w, 4 * rows as u32),
                    zlib(&data),
                )]),
            ));
        }
    }
    assert_metered(shapes);
}

/// Predictor rows of a MiB, and of 64 KiB, which saved states hold.
#[test]
fn wide_predictor_rows_are_metered() {
    let mut shapes = Vec::new();
    for (columns, predictor) in [(1 << 20, 10), (1 << 20, 2), (65_536, 15)] {
        let (w, h) = (257_u32, 4096_u32);
        let len = (w * h) as usize;
        let row = columns + usize::from(predictor >= 10);
        let mut data = noise(len.div_ceil(columns) * row, 7);
        if predictor >= 10 {
            for r in data.chunks_mut(row) {
                r[0] %= 5;
            }
        }
        let dict = format!(
            "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode \
             /DecodeParms << /Predictor {predictor} /Columns {columns} >>"
        );
        shapes.push((
            format!("predictor {predictor} of {columns} columns"),
            pdf(&[(image(&dict, w, h), zlib(&data))]),
        ));
    }
    assert_metered(shapes);
}

/// Data encrypted with RC4, stored and behind Flate.
#[test]
fn encrypted_data_is_metered() {
    use crate::rc4_fixtures::{Part, file};
    let n = 4096_u32;
    let data = noise((n * n / 8) as usize, 5);
    let mut shapes = Vec::new();
    for (name, filter, stored) in [
        ("stored", "", data.clone()),
        ("flate", "/Filter /FlateDecode", zlib(&data)),
    ] {
        let dict = image(&format!("{GRAY1} {filter}"), n, n);
        let bytes = file(
            &[
                Part::Object("<< /Type /Catalog /Pages 2 0 R >>", b""),
                Part::Object("<< /Type /Pages /Kids [] /Count 0 >>", b""),
                Part::StreamWith(&dict, &stored),
            ],
            &[],
        );
        shapes.push((format!("rc4 {name}"), Pdf::new(bytes).unwrap()));
    }
    assert_metered(shapes);
}

/// CCITT data alone and behind other filters: Group 4 rows that repeat the white row
/// above, each a single bit.
#[test]
fn ccitt_behind_filters_is_metered() {
    let (w, h) = (4096_u32, 4096_u32);
    let g4 = vec![0xff; (h / 8) as usize];
    let ccitt = format!("<< /K -1 /Columns {w} /Rows {h} >>");
    let mut rng = Rng(3);
    let mut shapes = Vec::new();
    for (name, filters, params, data) in [
        ("alone", "/CCITTFaxDecode", ccitt.clone(), g4.clone()),
        (
            "flate",
            "/FlateDecode /CCITTFaxDecode",
            format!("null {ccitt}"),
            zlib(&g4),
        ),
        (
            "a85 flate",
            "/A85 /FlateDecode /CCITTFaxDecode",
            format!("null null {ccitt}"),
            chains::ascii_85(&zlib(&g4), &mut rng),
        ),
    ] {
        let dict = format!("{GRAY1} /Filter [{filters}] /DecodeParms [{params}]");
        shapes.push((format!("ccitt {name}"), pdf(&[(image(&dict, w, h), data)])));
    }
    assert_metered(shapes);
}

/// Soft masks on the colour's grid and on their own, stencil masks, and mask
/// streams, their data ending early too.
#[test]
fn masks_are_metered() {
    let (w, h) = (2048_u32, 2048_u32);
    let gray8 = noise((w * h) as usize, 9);
    let packed = noise((w * h / 8) as usize, 4);
    let flate8 = "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode";
    let flate1 = format!("{GRAY1} /Filter /FlateDecode");
    let shapes = vec![
        (
            "soft mask alongside".to_owned(),
            pdf(&[
                (image(&format!("{flate8} /SMask 4 0 R"), w, h), zlib(&gray8)),
                (image(flate8, w, h), zlib(&gray8[..gray8.len() / 2 + 1])),
            ]),
        ),
        (
            "soft mask on its own grid".to_owned(),
            pdf(&[
                (image(&format!("{flate8} /SMask 4 0 R"), w, h), zlib(&gray8)),
                (
                    image(flate8, w / 2, h * 2),
                    zlib(&gray8[..gray8.len() / 2 + 1]),
                ),
            ]),
        ),
        (
            "stencil mask".to_owned(),
            pdf(&[(
                image("/ImageMask true /Filter /FlateDecode", w, h),
                zlib(&packed[..packed.len() / 2 + 1]),
            )]),
        ),
        (
            "mask stream".to_owned(),
            pdf(&[
                (image(&format!("{flate8} /Mask 4 0 R"), w, h), zlib(&gray8)),
                (
                    image("/ImageMask true /Filter /FlateDecode", w, h),
                    zlib(&packed),
                ),
            ]),
        ),
        (
            "matte".to_owned(),
            pdf(&[
                (
                    image(&format!("{flate8} /SMask 4 0 R"), w, h),
                    zlib(&gray8[..gray8.len() / 2 + 1]),
                ),
                (image(&format!("{flate8} /Matte [0.5]"), w, h), zlib(&gray8)),
            ]),
        ),
        (
            "8-bit cmyk with a mask".to_owned(),
            pdf(&[
                (
                    image(
                        "/ColorSpace /DeviceCMYK /BitsPerComponent 8 /SMask 4 0 R",
                        w,
                        h,
                    ),
                    noise((w * h) as usize + 1, 2),
                ),
                (image(&flate1, w, h), zlib(&packed)),
            ]),
        ),
    ];
    assert_metered(shapes);
}
