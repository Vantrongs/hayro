//! Work that grows with the data is done between two polls of a request's hook, a
//! piece at a time, never at once: for every shape of image a hostile file can hold,
//! opening it, requesting regions of it and decoding it whole allocate and copy at
//! most a few pieces' worth between two polls. The bytes are counted by this test
//! binary's allocator: those allocated, a copy's included, and those a reallocation
//! adds. The bytes a reallocation moves are not: the allocator moves them as it can
//! ([`Meter::reserve`]). Room made by [`Meter::room`] and [`Meter::reserve`] is
//! filled a counted piece at a time, and that the CCITT decoder makes for a line's
//! colour changes a change per code read, so it counts as no work ([`reserved`]).
//! Work that allocates
//! nothing is bounded by the steps of the loops that work on data, counted where
//! they are taken apart from the meter ([`operations`]): a layer of a filter chain
//! reached, a byte a decoder took, a CCITT code read or reference change searched.
//! Each poll also reads the asset's statistics, which no lock held across a poll
//! may block.

use super::tests::{asset, image, is_streamed, pdf, request, whole, zlib};
use super::*;
use crate::chains::{self, Rng};
use crate::fixtures::noise;
use hayro_syntax::Pdf;
use hayro_syntax::object::{ObjectIdentifier, Stream};
use hayro_syntax::work::{POLL_BYTES, operations, reserved};
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

/// The steps of loops one stretch of work between two polls may take: a poll's
/// worth, as a step counts as a byte of work or more, and what one step of a
/// decoder takes past it (a call to zlib makes up to a poll's worth from a chunk of
/// input). The worst stretch the shapes below take is some 140,000 steps.
const STEPS: usize = 3 * POLL_BYTES;

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
    allocated as i128 - reserved() as i128
}

/// The most work done between two polls of a hook, by the operation it was polled
/// in: bytes allocated and steps of loops taken.
#[derive(Default)]
struct Stretches {
    /// The work [`uncounted`] at the last poll.
    last: Cell<i128>,
    /// The [`operations`] at the last poll.
    steps: Cell<usize>,
    worst: RefCell<Vec<(String, Stretch)>>,
    /// The work of all stretches.
    total: Cell<Stretch>,
    operation: RefCell<&'static str>,
}

/// The most bytes allocated and steps taken between two polls.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Stretch {
    bytes: usize,
    steps: usize,
}

impl Stretches {
    /// Ends a stretch of the current operation, at a poll or at its end.
    fn end(&self) {
        let now = uncounted();
        let bytes = usize::try_from(now - self.last.replace(now)).unwrap_or(0);
        let now = operations();
        let steps = now - self.steps.replace(now);
        let total = self.total.get();
        self.total.set(Stretch {
            bytes: total.bytes + bytes,
            steps: total.steps + steps,
        });
        let operation = *self.operation.borrow();
        let mut worst = self.worst.borrow_mut();
        let w = match worst.iter_mut().find(|(o, _)| o == operation) {
            Some((_, w)) => w,
            None => {
                worst.push((operation.to_owned(), Stretch::default()));
                &mut worst.last_mut().unwrap().1
            }
        };
        w.bytes = w.bytes.max(bytes);
        w.steps = w.steps.max(steps);
    }

    /// Runs `f` as `operation`, from a stretch's start.
    fn run<T>(&self, operation: &'static str, f: impl FnOnce() -> T) -> T {
        *self.operation.borrow_mut() = operation;
        self.last.set(uncounted());
        self.steps.set(operations());
        let result = f();
        self.end();
        result
    }
}

/// Opens, requests and decodes whole the image in object 3 of `pdf` with a hook
/// that ends a stretch at each poll and reads the asset's statistics; returns each
/// operation's largest stretch.
fn stretches(pdf: &Pdf) -> Vec<(String, Stretch)> {
    measure(pdf).0
}

/// [`stretches`], and the work of all of them.
fn measure(pdf: &Pdf) -> (Vec<(String, Stretch)>, Stretch) {
    let asset = asset(pdf);
    // The document parses an object's dictionary a few times at most, however often
    // it is read, then keeps it: that parse is the document's, not the image's.
    for id in [3, 4] {
        for _ in 0..PARSES {
            drop(pdf.xref().get::<Stream<'_>>(ObjectIdentifier::new(id, 0)));
        }
    }
    measure_asset(asset)
}

/// [`measure`], of `asset`, whose document's objects are read as they are to be.
fn measure_asset(asset: ImageAsset) -> (Vec<(String, Stretch)>, Stretch) {
    // The process makes its CMYK profile once, on first use.
    ColorSpace::device_cmyk().to_rgba(&[0.0; 4], 1.0);
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
    (s.worst.into_inner(), s.total.get())
}

/// Every shape in `shapes`, a name and a file, does no more work between two polls
/// than [`BOUND`] bytes and [`STEPS`] steps.
fn assert_metered(shapes: Vec<(String, Pdf)>) {
    let mut over = Vec::new();
    for (name, pdf) in shapes {
        for (operation, worst) in stretches(&pdf) {
            let Stretch { bytes, steps } = worst;
            println!("{name}: {operation}: {bytes} bytes, {steps} steps");
            if bytes > BOUND {
                over.push(format!("{name}: {operation}: {bytes} bytes"));
            }
            if steps > STEPS {
                over.push(format!("{name}: {operation}: {steps} steps"));
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

/// Chains of growing length of Flate filters over no data: requests reach every
/// layer, to plan a checkpoint of them and to read through them to the end.
#[test]
fn walks_over_long_chains_are_metered() {
    let mut shapes = Vec::new();
    for length in [100_000, 300_000] {
        let filters = ["/FlateDecode"].repeat(length).join(" ");
        let dict = format!("{GRAY1} /Filter [{filters}]");
        shapes.push((
            format!("{length} x /FlateDecode over no data"),
            pdf(&[(image(&dict, 8, 8), Vec::new())]),
        ));
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

/// Two Group 4 rows of `width` columns: one of pixels alternately white and black,
/// then one coded as a single white run over it all, after which the decoder looks
/// for the next change on the row above past each of its changes.
fn wide_reference(width: u32) -> Vec<u8> {
    const HORIZONTAL: &str = "001";
    const WHITE_1: &str = "000111";
    const BLACK_1: &str = "010";
    const WHITE_2560: &str = "000000011111";
    const WHITE_0: &str = "00110101";
    const BLACK_0: &str = "0000110111";
    assert!(width.is_multiple_of(2560));
    let mut codes = String::new();
    for _ in 0..width / 2 {
        codes.extend([HORIZONTAL, WHITE_1, BLACK_1]);
    }
    codes.push_str(HORIZONTAL);
    for _ in 0..width / 2560 {
        codes.push_str(WHITE_2560);
    }
    codes.extend([WHITE_0, BLACK_0]);
    let bits = codes.as_bytes();
    (0..bits.len().div_ceil(8))
        .map(|byte| {
            (0..8).fold(0_u8, |acc, bit| {
                acc << 1 | u8::from(bits.get(8 * byte + bit) == Some(&b'1'))
            })
        })
        .collect()
}

/// A row of CCITT data coded in a few bits whose reference row has a change at every
/// pixel, at growing widths: the search of the reference row counts as the work it
/// is.
#[test]
fn ccitt_reference_searches_are_metered() {
    let mut shapes = Vec::new();
    for width in [2560 * 16, 2560 * 64, 2560 * 256] {
        let params = format!("<< /K -1 /Columns {width} /Rows 2 >>");
        let dict = format!("{GRAY1} /Filter /CCITTFaxDecode /DecodeParms {params}");
        shapes.push((
            format!("ccitt {width} columns"),
            pdf(&[(image(&dict, width, 2), wide_reference(width))]),
        ));
    }
    assert_metered(shapes);
}

/// A colour-keyed image whose `/Mask` array runs on past its ranges, by a growing
/// number of entries a key ignores: opening, requesting and decoding it does the
/// same work, and gives the same pixels, however many there are.
#[test]
fn ignored_colour_key_entries_are_not_read() {
    let (w, h) = (64_u32, 64_u32);
    let gray = noise((w * h) as usize, 21);
    let mut first = None;
    for ignored in [0, 10_000, 1_000_000] {
        let dict = format!(
            "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode \
             /Mask [0 120{}]",
            " 7".repeat(ignored)
        );
        let pdf = pdf(&[(image(&dict, w, h), zlib(&gray))]);
        let pixels = {
            let asset = asset(&pdf);
            let mut streamed = asset.open(None, None).unwrap();
            assert!(is_streamed(&streamed));
            let all = [0, 0, w, h];
            let a = request(&mut streamed, Plane::Image, (w, h), all);
            let b = request(&mut whole(&asset), Plane::Image, (w, h), all);
            assert_eq!((a.format, &a.data), (b.format, &b.data));
            let alpha: Vec<_> = a
                .data
                .chunks(a.format.channels())
                .map(|p| p[p.len() - 1])
                .collect();
            assert!(alpha.contains(&0) && alpha.contains(&255), "{:?}", a.format);
            a.data
        };
        let work = (pixels, measure(&pdf));
        println!("{ignored} ignored entries: {:?}", work.1.1);
        match &first {
            None => first = Some(work),
            Some(first) => assert_eq!(&work, first, "{ignored} ignored entries"),
        }
    }
}

/// Raw deflate data: `blocks` dynamic blocks that make nothing, each with a literal
/// code oversubscribed as far as one goes (256 codes of one bit, then the end of the
/// block of 15), which only a permissive decoder reads; then a final stored block of
/// `data`. Filling the table of such a code a code at a time writes each one-bit
/// code over half of its 32,768 entries.
fn oversubscribed(blocks: usize, data: &[u8]) -> Vec<u8> {
    const CODE_LENGTH_ORDER: [u32; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut bits = Vec::new();
    let mut put = |value: u32, n: u32| bits.extend((0..n).map(|i| value >> i & 1 == 1));
    for _ in 0..blocks {
        // Not final, dynamic; 257 literal codes, one distance code, 19 code length codes.
        put(0, 1);
        put(2, 2);
        put(0, 5);
        put(0, 5);
        put(15, 4);
        // Code lengths 1 and 15 are coded in one bit each, 0 and 1.
        for symbol in CODE_LENGTH_ORDER {
            put(u32::from(symbol == 1 || symbol == 15), 3);
        }
        // Literals 0 to 255 of one bit, the end of the block of 15, the distance of one.
        for _ in 0..256 {
            put(0, 1);
        }
        put(1, 1);
        put(0, 1);
        // The end of the block: 15 zero bits.
        put(0, 15);
    }
    // Final, stored, then the rest of the byte.
    put(1, 1);
    put(0, 2);
    let mut out: Vec<u8> = bits
        .chunks(8)
        .map(|byte| {
            byte.iter()
                .rev()
                .fold(0, |acc, &bit| acc << 1 | u8::from(bit))
        })
        .collect();
    let len = u16::try_from(data.len()).unwrap();
    out.extend(len.to_le_bytes());
    out.extend((!len).to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// Flate data of a growing number of blocks whose literal code is oversubscribed:
/// building their tables counts as work before it is done, as much as the steps it
/// takes, and no stretch between two polls takes more than [`STEPS`].
#[test]
fn oversubscribed_flate_tables_are_metered() {
    use hayro_syntax::work::TOLD;
    let (w, h) = (8_u32, 8_u32);
    let gray = noise((w * h) as usize, 13);
    let dict = "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode";
    let mut shapes = Vec::new();
    let mut per_count = Vec::new();
    for blocks in [1, 4, 16] {
        let pdf = pdf(&[(image(dict, w, h), oversubscribed(blocks, &gray))]);
        let asset = asset(&pdf);
        let (told, steps) = (TOLD.with(Cell::get), operations());
        let mut source = asset.open(None, None).unwrap();
        assert!(is_streamed(&source));
        let region = request(&mut source, Plane::Image, (w, h), [0, 0, w, h]);
        let work = (TOLD.with(Cell::get) - told, operations() - steps);
        assert_eq!(region.data, gray, "{blocks} blocks");
        drop(source);
        let whole = request(&mut whole(&asset), Plane::Image, (w, h), [0, 0, w, h]);
        assert_eq!(whole.data, gray, "{blocks} blocks decoded whole");
        println!("{blocks} blocks: {} charged, {} steps", work.0, work.1);
        per_count.push((blocks, work));
        shapes.push((format!("{blocks} oversubscribed blocks"), pdf));
    }
    for pair in per_count.windows(2) {
        let [(fewer, (told_a, steps_a)), (more, (told_b, steps_b))] = pair else {
            unreachable!()
        };
        let (charged, taken) = (told_b - told_a, steps_b - steps_a);
        assert!(
            charged >= taken,
            "{fewer} to {more} blocks: {charged} charged for {taken} steps"
        );
    }
    assert_metered(shapes);
}

/// One-pixel 1-bit images whose arrays run on past the entries they are read by,
/// by a growing number of numbers: a soft mask's `/Matte`, which so gives no
/// matte, and the image's and the mask's `/Decode`; and a colour key's `/Mask`
/// and the image's `/Decode`. Opening, requesting and decoding each does the same
/// work, and gives the same pixels, however many there are.
#[test]
fn array_entries_past_those_read_are_not_read() {
    let bit = zlib(&[0x00]);
    let shapes = |suffix: &str| {
        let gray1 = format!("{GRAY1} /Filter /FlateDecode");
        [
            (
                "matte",
                pdf(&[
                    (
                        image(&format!("{gray1} /SMask 4 0 R /Decode [1 0{suffix}]"), 1, 1),
                        bit.clone(),
                    ),
                    (
                        image(
                            &format!("{gray1} /Matte [0.5{suffix}] /Decode [1 0{suffix}]"),
                            1,
                            1,
                        ),
                        bit.clone(),
                    ),
                ]),
            ),
            (
                "colour key",
                pdf(&[(
                    image(
                        &format!("{gray1} /Mask [0 0{suffix}] /Decode [1 0{suffix}]"),
                        1,
                        1,
                    ),
                    bit.clone(),
                )]),
            ),
        ]
    };
    let mut first = Vec::new();
    for numbers in [1, 10_000, 1_000_000] {
        for (i, (name, pdf)) in shapes(&" 7".repeat(numbers)).into_iter().enumerate() {
            let pixels = {
                let asset = asset(&pdf);
                let mut streamed = asset.open(None, None).unwrap();
                assert!(is_streamed(&streamed), "{name}");
                let a = request(&mut streamed, Plane::Image, (1, 1), [0, 0, 1, 1]);
                let b = request(&mut whole(&asset), Plane::Image, (1, 1), [0, 0, 1, 1]);
                assert_eq!((a.format, &a.data), (b.format, &b.data), "{name}");
                (a.format, a.data)
            };
            let work = (pixels, measure(&pdf));
            println!(
                "{name}, {numbers} numbers past: {:?} {:?}",
                work.0, work.1.1
            );
            match first.get(i) {
                None => first.push(work),
                Some(first) => assert_eq!(&work, first, "{name}, {numbers} numbers past"),
            }
        }
    }
}

/// The pixels of `asset` requested streamed, which decoding it whole gives too.
fn pixels(asset: &ImageAsset, name: &str) -> (PixelFormat, Vec<u8>) {
    let mut streamed = asset.open(None, None).unwrap();
    let a = request(&mut streamed, Plane::Image, (1, 1), [0, 0, 1, 1]);
    let b = request(&mut whole(asset), Plane::Image, (1, 1), [0, 0, 1, 1]);
    assert_eq!((a.format, &a.data), (b.format, &b.data), "{name}");
    (a.format, a.data)
}

/// One-pixel 1-bit images that refer to object 5, an array of numbers or a
/// dictionary, of a growing number of entries, that nothing read before:
/// - after the entries an array's rule reads: the image's and a soft mask's
///   `/Decode` after their pair, a colour key's `/Mask` after its pair and after a
///   name that ends its numbers, and a `/Matte` after a name that ends its one
///   number, after a second number that makes it no matte, and after a name that
///   leaves it none;
/// - where an entry the rule reads must be a number, so that the object's kind
///   decides: `/Decode [null 5 0 R]`, whose pair goes on past a null, `/Decode
///   [5 0 R 1]`, the soft mask's `/Matte [0.5 5 0 R]`, which reads the entry after
///   its number to tell, and a colour key's `/Mask [0 5 0 R]`;
/// - as a `/Decode` itself, where the object is a dictionary, which is no decode
///   array.
///
/// Each document is loaded fresh, which reads none of its objects but its page
/// tree's, and its image opened first: loading it, opening, requesting and decoding
/// the image allocates the same and takes the same steps between polls and in all,
/// and gives the same pixels, however large the object, and reads the object's
/// bytes once at most. Its kind is told by its first token, and it is not built:
/// where that decides, it is passed over once, to tell whether it can be read,
/// which a reference to an object that cannot be read loses.
#[cfg(debug_assertions)]
#[test]
fn referenced_objects_a_rule_reads_past_or_by_kind_are_not_built() {
    use crate::fixtures::{file, stream};
    use hayro_syntax::work::BODIES_READ;
    let bit = zlib(&[0x00]);
    let gray1 = format!("{GRAY1} /Filter /FlateDecode");
    let with_mask = |mask: &str| {
        vec![
            stream(&image(&format!("{gray1} /SMask 4 0 R"), 1, 1), &bit),
            stream(&image(&format!("{gray1} {mask}"), 1, 1), &bit),
        ]
    };
    let alone = |entries: &str| vec![stream(&image(&format!("{gray1} {entries}"), 1, 1), &bit)];
    // Each shape, whether an array refers to the object where an array is read,
    // and whether the object is read at all: past the entries a rule reads, it is
    // not; where its kind decides, it is passed over.
    let shapes: Vec<(&str, Vec<Vec<u8>>, bool, bool)> = vec![
        ("decode", alone("/Decode [1 0 5 0 R]"), true, false),
        (
            "soft mask's decode",
            with_mask("/Decode [1 0 5 0 R]"),
            true,
            false,
        ),
        (
            "matte of one",
            with_mask("/Matte [0.5 /N 5 0 R]"),
            true,
            false,
        ),
        (
            "matte of two",
            with_mask("/Matte [0.5 0.5 5 0 R]"),
            true,
            false,
        ),
        ("matte of none", with_mask("/Matte [/N 5 0 R]"), true, false),
        ("colour key", alone("/Mask [0 0 5 0 R]"), true, false),
        (
            "colour key cut short",
            alone("/Mask [0 /N 5 0 R]"),
            true,
            false,
        ),
        (
            "decode past a null",
            alone("/Decode [null 5 0 R]"),
            true,
            true,
        ),
        (
            "decode starting with it",
            alone("/Decode [5 0 R 1]"),
            true,
            true,
        ),
        (
            "matte deciding",
            with_mask("/Matte [0.5 5 0 R]"),
            true,
            true,
        ),
        ("colour key's pair", alone("/Mask [0 5 0 R]"), true, true),
        ("decode itself", alone("/Decode 5 0 R"), false, true),
    ];
    let object = |array: bool, n: usize| match array {
        true => format!("[{}]", " 7".repeat(n)),
        false => {
            let entries: String = (0..n).map(|i| format!("/K{i} 7 ")).collect();
            format!("<< {entries}>>")
        }
    };
    let mut grew = Vec::new();
    for (name, images, of_any_kind, passed) in &shapes {
        for (kind, array) in [("an array", true), ("a dictionary", false)] {
            // An array where an array is read is read.
            if array && !of_any_kind {
                continue;
            }
            let mut first = None;
            // A first run, on a document of its own, makes what the thread makes once,
            // on first use, which no run measured then makes.
            for (run, entries) in [1, 1, 10_000, 1_000_000].into_iter().enumerate() {
                // A page tree that loads without a scan of the file's objects.
                let mut all = vec![
                    b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                    b"<< /Type /Pages /Kids [6 0 R] /Count 1 >>".to_vec(),
                ];
                all.extend(images.iter().cloned());
                all.resize(4, b"null".to_vec());
                let referred = object(array, entries).into_bytes();
                let size = referred.len();
                all.push(referred);
                all.push(b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] >>".to_vec());
                let data = file(&all, "");
                let bodies = BODIES_READ.with(Cell::get);
                let pdf = Pdf::new(data).unwrap();
                let measured = measure_asset(asset(&pdf));
                let bodies = BODIES_READ.with(Cell::get) - bodies;
                let work = (measured, pixels(&asset(&pdf), name));
                println!(
                    "{name}, {kind} of {entries}: {:?} {:?} {bodies} body bytes, {:?}",
                    work.0.0, work.0.1, work.1
                );
                match &first {
                    _ if run == 0 => {}
                    None => first = Some((work, bodies)),
                    Some((first, read))
                        if work != *first || bodies > read + usize::from(*passed) * size =>
                    {
                        grew.push(format!("{name}, {kind} of {entries}"));
                    }
                    Some(_) => {}
                }
            }
        }
    }
    assert!(grew.is_empty(), "work grew with the object: {grew:?}");
}

/// The image of the inline image `content` holds, as an asset.
fn inline_asset(content: &[u8]) -> ImageAsset {
    use hayro_syntax::content::TypedIter;
    use hayro_syntax::content::ops::TypedInstruction;
    let mut ops = TypedIter::new(content);
    let stream = loop {
        if let TypedInstruction::InlineImage(image) = ops.next().expect("an inline image") {
            break image.0.clone();
        }
    };
    let sink: WarningSinkFn = Arc::new(|_| {});
    ImageXObject::new(&stream, |_| None, &sink, &Cache::new(), None)
        .unwrap()
        .asset(None)
}

/// One-pixel 1-bit inline images, whose dictionaries are read from their bytes,
/// with a dictionary of a growing number of entries where an entry of another kind
/// is: past the pair of a `/D` that is read, in place of a `/D` array, of a
/// `/SMask` stream, and of the number of a Flate filter's `/Predictor`. Opening, requesting and decoding each does the same work and
/// gives the same pixels, however many entries the dictionary has: it is not read.
#[test]
fn inline_entries_of_another_kind_are_not_read() {
    let bit = zlib(&[0x00]);
    let shapes = |entries: &str| {
        [
            (
                "past the decode pair",
                format!("/D [1 0 << {entries} >>]"),
                &[0][..],
            ),
            ("a decode dictionary", format!("/D << {entries} >>"), &[0]),
            (
                "a soft mask dictionary",
                format!("/SMask << {entries} >>"),
                &[0],
            ),
            (
                "a predictor dictionary",
                format!("/F /Fl /DP << /Predictor << {entries} >> >>"),
                &bit,
            ),
        ]
    };
    let mut first = Vec::new();
    let mut grew = Vec::new();
    for count in [1, 1_000, 100_000] {
        let entries: String = (0..count).map(|i| format!("/K{i} 0 ")).collect();
        for (i, (name, entry, data)) in shapes(&entries).into_iter().enumerate() {
            let mut content = format!("BI /W 1 /H 1 /BPC 1 /CS /G {entry} ID ").into_bytes();
            content.extend_from_slice(data);
            content.extend_from_slice(b" EI");
            let asset = inline_asset(&content);
            let work = (pixels(&asset, name), measure_asset(asset).1);
            println!("{name}, {count} entries: {:?} {:?}", work.0, work.1);
            match first.get(i) {
                None => first.push(work),
                Some(first) if work != *first => grew.push(format!("{name}, {count} entries")),
                Some(_) => {}
            }
        }
    }
    assert!(grew.is_empty(), "work grew with the dictionary: {grew:?}");
}

/// Images whose data holds nothing, declared so large that their planes, held whole
/// as `with_rgba` and `with_stencil` hand them over, do not fit in a vector: grey
/// and a stencil mask past `isize::MAX` bytes, RGB whose bytes overflow, and a soft
/// mask past `isize::MAX` under a colour that fits. Each fails as a decode does,
/// handing nothing over and warning, before anything that large is allocated; an
/// image as empty that fits is handed over whole, on its declared grid.
#[test]
fn planes_too_large_to_hold_whole_fail_before_they_are_allocated() {
    use crate::color::Color;
    use crate::{InterpreterWarning, Paint, RasterImage, StencilImage};
    use smallvec::smallvec;
    use std::sync::Mutex;
    const HUGE: u32 = u32::MAX;
    let gray8 = "/ColorSpace /DeviceGray /BitsPerComponent 8";
    let shapes = [
        ("grey", vec![(image(gray8, HUGE, HUGE), vec![])], false),
        (
            "rgb",
            vec![(
                image("/ColorSpace /DeviceRGB /BitsPerComponent 8", HUGE, HUGE),
                vec![],
            )],
            false,
        ),
        (
            "stencil",
            vec![(
                image("/ImageMask true /BitsPerComponent 1", HUGE, HUGE),
                vec![],
            )],
            false,
        ),
        (
            "soft mask",
            vec![
                (image(&format!("{gray8} /SMask 4 0 R"), 1, 1), vec![7]),
                (image(gray8, HUGE, HUGE), vec![]),
            ],
            false,
        ),
        ("grey that fits", vec![(image(gray8, 3, 2), vec![])], true),
    ];
    for (name, objects, fits) in shapes {
        let pdf = pdf(&objects);
        let stream = pdf
            .xref()
            .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
            .unwrap();
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let sink: WarningSinkFn = {
            let warnings = warnings.clone();
            Arc::new(move |w| warnings.lock().unwrap().push(w))
        };
        let obj = ImageXObject::new(&stream, |_| None, &sink, &Cache::new(), None).unwrap();
        // The image decodes: it is holding it whole that fails.
        let decodes = match obj.kind {
            ImageKind::StencilMask => obj.decoded_mask(None).is_some(),
            _ => obj.decoded_image(None).is_some(),
        };
        assert!(decodes, "{name}");
        let mut handed = None;
        COUNTS.with(|c| c.set(Some(0)));
        if obj.kind == ImageKind::StencilMask {
            let paint = Paint::Color(Color::new(ColorSpace::device_gray(), smallvec![0.0], 1.0));
            let stencil = StencilImage {
                paint,
                image_xobject: obj.clone(),
            };
            stencil.with_stencil(
                |l, _| handed = Some((l.width, l.height, l.data.len())),
                None,
            );
        } else {
            RasterImage(obj.clone()).with_rgba(
                |mut i, _| handed = Some((i.width(), i.height(), i.rows_mut().0.len())),
                None,
            );
        }
        let allocated = COUNTS.with(|c| c.replace(None)).unwrap();
        let warned = warnings.lock().unwrap().clone();
        println!("{name}: {handed:?}, {allocated} bytes allocated, {warned:?}");
        if fits {
            assert_eq!(handed, Some((3, 2, 6)), "{name}");
            assert!(warned.is_empty(), "{name}");
        } else {
            assert_eq!(handed, None, "{name}");
            assert!(
                matches!(warned[..], [InterpreterWarning::ImageDecodeFailure]),
                "{name}: {warned:?}"
            );
            assert!(
                allocated < POLL_BYTES,
                "{name}: {allocated} bytes allocated"
            );
        }
    }
}
