//! Images whose data ends at every byte stream as they decode whole: generated
//! images of every sample depth whole decoding reads, one to four components,
//! decode arrays, colour keys, soft masks of every depth with and without `/Matte`
//! and decode arrays, on the image's grid and their own, mask streams,
//! stencil masks and CCITT data, raw and Flate, each cut after every byte of its
//! stored data, and of its data decoded. Flate data damaged at every byte, which
//! zlib may find corrupt after making bytes, streams as it decodes whole too.

use super::tests::{asset, image, is_streamed, pdf, request, try_whole};
use super::{ImageAsset, ImageRequest, ImageSource, Plane, RequestError};
use crate::fixtures::{noise, stored_zlib};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use std::io::Write;

const W: u32 = 13;
const H: u32 = 4;

/// The size of a soft mask on its own grid.
const OTHER_GRID: (u32, u32) = (5, 3);

/// `data` compressed, so that a cut falls within Huffman-coded blocks.
fn deflate(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// `n` bytes of samples whose second half repeats the first, so that Flate finds
/// matches, with every bit pattern in them.
fn samples(n: usize, seed: u32) -> Vec<u8> {
    let half = noise(n.div_ceil(2), seed);
    half.iter().chain(&half).copied().take(n).collect()
}

/// A way of storing data cut short: its filter entry, and each stored stream with
/// the byte it is cut after.
type Cuts = (&'static str, Vec<(usize, Vec<u8>)>);

/// The ways of storing `data` cut short, each with its filter entry and the stored
/// streams, with the byte each is cut after: the data cut after every byte, raw and
/// in a Flate stream of its own; and the data compressed, the compressed stream cut
/// after every byte.
fn cuts(data: &[u8]) -> [Cuts; 3] {
    cuts_at(data, |len| (0..=len).collect())
}

/// [`cuts`], after the bytes `at` gives for data of each length.
fn cuts_at(data: &[u8], at: impl Fn(usize) -> Vec<usize>) -> [Cuts; 3] {
    let compressed = deflate(data);
    let cut = |data: &[u8], wrap: fn(&[u8]) -> Vec<u8>| {
        at(data.len())
            .into_iter()
            .map(|k| (k, wrap(&data[..k])))
            .collect()
    };
    [
        ("", cut(data, <[u8]>::to_vec)),
        ("/Filter /FlateDecode", cut(data, |d| stored_zlib(d, 1000))),
        ("/Filter /FlateDecode", cut(&compressed, <[u8]>::to_vec)),
    ]
}

/// The cases checked, and the failures of those that failed.
#[derive(Default)]
struct Cases {
    count: usize,
    failed: Vec<String>,
}

impl Cases {
    /// Checks that `asset` streams as it decodes whole, noting a failure.
    fn check(&mut self, asset: &ImageAsset, case: &str) {
        self.count += 1;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_streams_as_whole(asset, case);
        }));
        if let Err(e) = result {
            let message = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            self.failed.push(message);
        }
    }

    /// Fails naming the cases that failed, if any.
    fn finish(self) {
        assert!(
            self.failed.is_empty(),
            "{} of {} cases failed; the first:\n{}",
            self.failed.len(),
            self.count,
            self.failed[..self.failed.len().min(20)].join("\n")
        );
    }
}

/// Requests of `asset` opened equal the same requests of it decoded whole, over the
/// same layout; where whole decoding fails, opening the image or requesting it does.
pub(super) fn assert_streams_as_whole(asset: &ImageAsset, case: &str) {
    let streamed = asset.open(None, None);
    let (mut streamed, mut whole) = match (streamed, try_whole(asset)) {
        (Ok(s), Some(w)) => (s, w),
        (Err(RequestError::Failed), None) => return,
        (Ok(mut s), None) => {
            let plane = s.layout().image;
            let result = s.request(&ImageRequest {
                plane: Plane::Image,
                grid: (plane.width, plane.height),
                window: [0, 0, plane.width, plane.height],
                cancel: None,
            });
            assert_eq!(
                result.err(),
                Some(RequestError::Failed),
                "{case}: whole decoding fails, a request does not"
            );
            return;
        }
        (Err(e), Some(_)) => panic!("{case}: opening fails ({e:?}), whole decoding does not"),
        (Err(e), None) => panic!("{case}: opening ends {e:?}, whole decoding fails"),
    };
    assert_eq!(streamed.layout(), whole.layout(), "{case}");
    assert_requests_equal(&mut streamed, &mut whole, case);
}

/// Requests of every plane of `streamed`, at full size and reduced, of the whole
/// plane, of its first row of bins (which a reader stops early for) and of a window
/// starting within it, equal those of `whole`, which has the same layout.
fn assert_requests_equal(streamed: &mut ImageSource<'_>, whole: &mut ImageSource<'_>, case: &str) {
    let layout = *streamed.layout();
    for plane in [Plane::Image, Plane::Mask] {
        let Some(p) = layout.plane(plane) else {
            continue;
        };
        for grid in [
            (p.width, p.height),
            (p.width.div_ceil(2), p.height.div_ceil(3)),
        ] {
            for window in [
                [0, 0, grid.0, grid.1],
                [0, 0, grid.0, 1],
                [grid.0 / 2, grid.1 / 2, grid.0, grid.1],
            ] {
                let a = request(streamed, plane, grid, window);
                let b = request(whole, plane, grid, window);
                assert_eq!(
                    (a.format, &a.data, a.to_image),
                    (b.format, &b.data, b.to_image),
                    "{case}: {plane:?} grid {grid:?} window {window:?}"
                );
            }
        }
    }
}

/// The first request of each plane of a fresh asset of `pdf`, for each of a column at
/// either edge and of the whole plane at full size and in one bin, equals that request
/// of the image decoded whole: a first request does not know yet where the colour's
/// or the mask's data ends, and a narrow one pushes part of a row before finding it.
fn assert_first_requests_equal(pdf: &hayro_syntax::Pdf, case: &str) {
    let reference = asset(pdf);
    let Some(mut whole) = try_whole(&reference) else {
        return;
    };
    let layout = *whole.layout();
    for plane in [Plane::Image, Plane::Mask] {
        let Some(p) = layout.plane(plane) else {
            continue;
        };
        let (w, h) = (p.width, p.height);
        for (grid, window) in [
            ((w, h), [0, 0, 1, h]),
            ((w, h), [w - 1, 0, w, h]),
            ((w, h), [0, 0, w, h]),
            ((1, 1), [0, 0, 1, 1]),
        ] {
            let fresh = asset(pdf);
            let mut streamed = fresh.open(None, None).expect(case);
            let a = streamed.request(&ImageRequest {
                plane,
                grid,
                window,
                cancel: None,
            });
            let a = a.unwrap_or_else(|e| panic!("{case}: {plane:?} {window:?} ends {e:?}"));
            let b = request(&mut whole, plane, grid, window);
            assert_eq!(
                (a.format, &a.data, a.to_image),
                (b.format, &b.data, b.to_image),
                "{case}: first request, {plane:?} grid {grid:?} window {window:?}"
            );
        }
    }
}

/// [`Cases::check`] and [`assert_first_requests_equal`] of `pdf`'s image.
fn check_first_requests(cases: &mut Cases, pdf: &hayro_syntax::Pdf, case: &str) {
    cases.check(&asset(pdf), case);
    cases.count += 1;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_first_requests_equal(pdf, case);
    }));
    if let Err(e) = result {
        let message = e
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        cases.failed.push(message);
    }
}

/// Colour and a mask on its grid whose data each end within a row, which a first
/// request finds on separate passes, each after pushing part of a row: 3-bit colour
/// 13 × 4 holding two rows and a byte under a 4-bit soft mask holding one row and a
/// byte, and a row of 131,073 bits holding 8,193 bytes under a 1-bit soft mask
/// holding as many.
#[test]
fn colour_and_mask_found_short_on_separate_passes_stream_as_whole() {
    let mut cases = Cases::default();
    let gray = |bits: u32| format!("/ColorSpace /DeviceGray /BitsPerComponent {bits}");
    let flate = "/Filter /FlateDecode";
    for (w, h, bits, colour_bytes, mask_bits, mask_bytes) in
        [(W, H, 3, 11, 4, 8), (131_073, 1, 1, 8193, 1, 8193)]
    {
        let objects = [
            (
                image(&format!("{} /SMask 4 0 R {flate}", gray(bits)), w, h),
                deflate(&samples(colour_bytes, 70)),
            ),
            (
                image(&format!("{} {flate}", gray(mask_bits)), w, h),
                deflate(&samples(mask_bytes, 71)),
            ),
        ];
        let case = format!(
            "{w}x{h} {bits}-bit, {colour_bytes} bytes, under {mask_bits}-bit soft mask, {mask_bytes} bytes"
        );
        check_first_requests(&mut cases, &pdf(&objects), &case);
    }
    cases.finish();
}

/// Colour under a mask on its grid, each Flate data cut after every byte of its
/// samples independently of the other: the first requests find the two ends in
/// either order, on one pass or on two.
#[test]
fn colour_and_mask_cut_independently_stream_as_whole() {
    let mut cases = Cases::default();
    let gray = |bits: usize| format!("/ColorSpace /DeviceGray /BitsPerComponent {bits}");
    let masks = [
        ("/SMask", gray(4), 4),
        ("/SMask", format!("{} /Decode [1 0]", gray(1)), 1),
        ("/SMask", gray(8), 8),
        ("/Mask", "/ImageMask true".to_string(), 1),
    ];
    for bits in [1_usize, 3] {
        let row = (W as usize * bits).div_ceil(8);
        let colour = samples(row * H as usize, 72);
        for (key, dict, mask_bits) in &masks {
            let mask_row = (W as usize * mask_bits).div_ceil(8);
            let mask = samples(mask_row * H as usize, 73);
            for k in 0..=colour.len() {
                for m in 0..=mask.len() {
                    let objects = [
                        (
                            image(
                                &format!("{} {key} 4 0 R /Filter /FlateDecode", gray(bits)),
                                W,
                                H,
                            ),
                            stored_zlib(&colour[..k], 1000),
                        ),
                        (
                            image(&format!("{dict} /Filter /FlateDecode"), W, H),
                            stored_zlib(&mask[..m], 1000),
                        ),
                    ];
                    let case = format!(
                        "{} under {dict}, colour cut after {k}, mask after {m}",
                        gray(bits)
                    );
                    check_first_requests(&mut cases, &pdf(&objects), &case);
                }
            }
        }
    }
    cases.finish();
}

/// A colour space of `n` components: its entry, and whether it is `Indexed`.
struct Space {
    entry: String,
    n: usize,
    indexed: bool,
}

/// The colour spaces of one to four components, `Indexed` among them for samples
/// that index a palette; object 5 is a `DeviceN` space's tint transform.
fn spaces(bits: usize) -> Vec<Space> {
    let space = |entry: &str, n| Space {
        entry: entry.into(),
        n,
        indexed: false,
    };
    let mut spaces = vec![
        space("/DeviceGray", 1),
        space(
            "[/CalGray << /WhitePoint [0.9505 1 1.089] /Gamma 2.2 >>]",
            1,
        ),
        space("[/DeviceN [/A /B] /DeviceGray 5 0 R]", 2),
        space("/DeviceRGB", 3),
        space("/DeviceCMYK", 4),
    ];
    if bits <= 8 {
        let hival = (1 << bits) - 1;
        let palette: String = noise(3 * (hival + 1), 40)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        spaces.push(Space {
            entry: format!("[/Indexed /DeviceRGB {hival} <{palette}>]"),
            n: 1,
            indexed: true,
        });
    }
    spaces
}

/// The tint transform of a two-component `DeviceN` space.
fn tint_transform() -> (String, Vec<u8>) {
    (
        "/FunctionType 4 /Domain [0 1 0 1] /Range [0 1]".into(),
        b"{ add 2 div }".to_vec(),
    )
}

/// Decode arrays of `space`: the default, inverted, and another.
fn decodes(space: &Space, bits: usize) -> [String; 3] {
    let max = (1_u32 << bits) - 1;
    let (inverted, other) = if space.indexed {
        (format!("{max} 0"), format!("1 {}", max / 2))
    } else {
        ("1 0".into(), "0.2 0.7".into())
    };
    let repeat = |pair: &str| format!("/Decode [{}]", format!("{pair} ").repeat(space.n));
    [String::new(), repeat(&inverted), repeat(&other)]
}

/// The alpha of an image: its dictionary entry, and the object it names (object 4),
/// complete.
fn alphas(space: &Space, bits: usize) -> Vec<(String, (String, Vec<u8>))> {
    let max = (1_u32 << bits) - 1;
    let gray = |w: u32, h: u32, extra: &str| {
        (
            image(
                &format!("/ColorSpace /DeviceGray /BitsPerComponent 8 {extra}"),
                w,
                h,
            ),
            noise((w * h) as usize, w + h),
        )
    };
    let none = (String::new(), gray(1, 1, ""));
    let key = format!("/Mask [{}]", format!("1 {} ", max / 2).repeat(space.n));
    let matte = format!("/Matte [{}]", "0.5 ".repeat(space.n));
    let (ow, oh) = OTHER_GRID;
    vec![
        none.clone(),
        (key, none.1.clone()),
        ("/SMask 4 0 R".into(), gray(W, H, "")),
        ("/SMask 4 0 R".into(), gray(W, H, &matte)),
        ("/SMask 4 0 R".into(), gray(ow, oh, "")),
        (
            "/Mask 4 0 R".into(),
            (
                image("/ImageMask true", W, H),
                noise((W.div_ceil(8) * H) as usize, 44),
            ),
        ),
    ]
}

/// Checks images of `bits` per component in every colour space, decode array and
/// alpha, their colour data cut short.
fn colour_cut(bits: usize) {
    let mut cases = Cases::default();
    for space in spaces(bits) {
        let row = (W as usize * space.n * bits).div_ceil(8);
        let data = samples(row * H as usize, (bits * 10 + space.n) as u32);
        for decode in decodes(&space, bits) {
            for (alpha, mask) in alphas(&space, bits) {
                let dict = format!(
                    "/ColorSpace {} /BitsPerComponent {bits} {decode} {alpha}",
                    space.entry
                );
                for (filter, stored) in cuts(&data) {
                    for (k, stored) in stored {
                        let pdf = pdf(&[
                            (image(&format!("{dict} {filter}"), W, H), stored),
                            mask.clone(),
                            tint_transform(),
                        ]);
                        let case = format!("{dict} {filter}, cut after {k}");
                        cases.check(&asset(&pdf), &case);
                    }
                }
            }
        }
    }
    cases.finish();
}

#[test]
fn colour_of_1_bit_cut_streams_as_whole() {
    colour_cut(1);
}

#[test]
fn colour_of_2_bits_cut_streams_as_whole() {
    colour_cut(2);
}

#[test]
fn colour_of_3_bits_cut_streams_as_whole() {
    colour_cut(3);
}

#[test]
fn colour_of_4_bits_cut_streams_as_whole() {
    colour_cut(4);
}

#[test]
fn colour_of_5_bits_cut_streams_as_whole() {
    colour_cut(5);
}

#[test]
fn colour_of_6_bits_cut_streams_as_whole() {
    colour_cut(6);
}

#[test]
fn colour_of_7_bits_cut_streams_as_whole() {
    colour_cut(7);
}

#[test]
fn colour_of_8_bits_cut_streams_as_whole() {
    colour_cut(8);
}

#[test]
fn colour_of_16_bits_cut_streams_as_whole() {
    colour_cut(16);
}

/// Images under every kind of mask stream, their mask's data cut short: soft masks
/// of every depth through their decode arrays, on the image's grid and on their own.
#[test]
fn masks_cut_stream_as_whole() {
    let mut cases = Cases::default();
    let (ow, oh) = OTHER_GRID;
    let gray = |bits: usize, extra: &str| {
        format!("/ColorSpace /DeviceGray /BitsPerComponent {bits} {extra}")
    };
    let mut masks = vec![
        ("/SMask", gray(8, ""), W, H, 8),
        ("/SMask", gray(8, "/Matte [0.5]"), W, H, 8),
        ("/SMask", gray(8, ""), ow, oh, 8),
        ("/SMask", gray(8, "/Decode [0.2 0.7]"), W, H, 8),
        ("/SMask", gray(1, "/Decode [0.2 0.7]"), W, H, 1),
        ("/SMask", gray(4, "/Matte [0.5]"), W, H, 4),
        ("/Mask", "/ImageMask true".into(), W, H, 1),
        ("/Mask", "/ImageMask true /Decode [1 0]".into(), W, H, 1),
        ("/Mask", "/ImageMask true /Decode [0.2 0.8]".into(), W, H, 1),
    ];
    for bits in 2..8 {
        masks.push(("/SMask", gray(bits, ""), W, H, bits));
        masks.push(("/SMask", gray(bits, "/Decode [1 0.3]"), ow, oh, bits));
    }
    for bits in [1_usize, 3, 8] {
        let row = (W as usize * bits).div_ceil(8);
        let colour = (
            format!("/ColorSpace /DeviceGray /BitsPerComponent {bits}"),
            samples(row * H as usize, 50),
        );
        for (key, dict, mw, mh, mask_bits) in &masks {
            let mask_row = (*mw as usize * mask_bits).div_ceil(8);
            let data = samples(mask_row * *mh as usize, 51);
            for (filter, stored) in cuts(&data) {
                for (k, stored) in stored {
                    let pdf = pdf(&[
                        (
                            image(&format!("{} {key} 4 0 R", colour.0), W, H),
                            colour.1.clone(),
                        ),
                        (image(&format!("{dict} {filter}"), *mw, *mh), stored),
                    ]);
                    let case = format!("{} under {dict} {filter}, mask cut after {k}", colour.0);
                    cases.check(&asset(&pdf), &case);
                }
            }
        }
    }
    cases.finish();
}

/// Stencil masks of every `/Decode` and depth, their data cut short.
#[test]
fn stencil_masks_cut_stream_as_whole() {
    let mut cases = Cases::default();
    for (w, h) in [(W, H), (16, 3)] {
        for dict in [
            "/ImageMask true",
            "/ImageMask true /Decode [1 0]",
            "/ImageMask true /Decode [0.2 0.8]",
            "/ImageMask true /BitsPerComponent 8",
            "/ImageMask true /BitsPerComponent 8 /Decode [1 0]",
        ] {
            let bits = if dict.contains('8') { 8 } else { 1 };
            let data = samples((w as usize * bits).div_ceil(8) * h as usize, 52);
            for (filter, stored) in cuts(&data) {
                for (k, stored) in stored {
                    let pdf = pdf(&[(image(&format!("{dict} {filter}"), w, h), stored)]);
                    let case = format!("{dict} {w}x{h} {filter}, cut after {k}");
                    cases.check(&asset(&pdf), &case);
                }
            }
        }
    }
    cases.finish();
}

/// The bytes to cut data of rows of `row_bytes` bytes after, `len` in all, where every
/// byte takes too long: every byte within `NEAR` of a row's start or middle (past the
/// first piece a request pushes of it), and every `STRIDE`th byte between.
fn sampled(len: usize, row_bytes: usize) -> Vec<usize> {
    const NEAR: usize = 4;
    const STRIDE: usize = 8191;
    let marks = (0..=2 * len / row_bytes).map(|k| k * row_bytes / 2);
    let mut at: Vec<usize> = marks
        .flat_map(|m| m.saturating_sub(NEAR)..=m + NEAR)
        .chain((0..len).step_by(STRIDE))
        .filter(|&k| k <= len)
        .chain([len])
        .collect();
    at.sort_unstable();
    at.dedup();
    at
}

/// The rows of the wide images.
const WIDE_H: u32 = 2;

/// A wide gray image of `bits` per sample: its dictionary, with `extra`, and data.
fn wide_gray(w: u32, bits: usize, extra: &str) -> (String, Vec<u8>) {
    (
        image(
            &format!("/ColorSpace /DeviceGray /BitsPerComponent {bits} {extra}"),
            w,
            WIDE_H,
        ),
        noise((w as usize * bits).div_ceil(8) * WIDE_H as usize, w),
    )
}

/// Images of `dict`, rows of `w` texels of `n` components of `bits` each wider
/// than a piece of work, so that a request finds Flate data ending within a row after
/// pushing part of it, under `mask` (object 4), cut short at a dense sample of bytes
/// ([`sampled`]): every byte of them takes minutes.
fn wide_cut(dict: &str, w: u32, n: usize, bits: usize, mask: Option<(String, Vec<u8>)>) {
    let mut cases = Cases::default();
    let row_bytes = (w as usize * n * bits).div_ceil(8);
    let data = samples(row_bytes * WIDE_H as usize, 53);
    let mask = mask.unwrap_or_else(|| wide_gray(1, 8, ""));
    for (filter, stored) in cuts_at(&data, |len| sampled(len, row_bytes)) {
        for (k, stored) in stored {
            let pdf = pdf(&[
                (image(&format!("{dict} {filter}"), w, WIDE_H), stored),
                mask.clone(),
            ]);
            let case = format!("{dict} {w} wide {filter}, cut after {k}");
            cases.check(&asset(&pdf), &case);
        }
    }
    cases.finish();
}

const WIDE: u32 = 140_001;
const GRAY_3: &str = "/ColorSpace /DeviceGray /BitsPerComponent 3";

#[test]
fn wide_rows_of_3_bits_cut_stream_as_whole() {
    wide_cut(GRAY_3, WIDE, 1, 3, None);
}

#[test]
fn wide_rows_under_a_colour_key_cut_stream_as_whole() {
    wide_cut(&format!("{GRAY_3} /Mask [1 4]"), WIDE, 1, 3, None);
}

#[test]
fn wide_rows_under_a_soft_mask_cut_stream_as_whole() {
    let mask = wide_gray(WIDE, 8, "");
    wide_cut(&format!("{GRAY_3} /SMask 4 0 R"), WIDE, 1, 3, Some(mask));
}

#[test]
fn wide_rows_under_matte_cut_stream_as_whole() {
    let mask = wide_gray(WIDE, 8, "/Matte [0.5]");
    wide_cut(&format!("{GRAY_3} /SMask 4 0 R"), WIDE, 1, 3, Some(mask));
}

#[test]
fn wide_rgb_rows_cut_stream_as_whole() {
    let rgb = "/ColorSpace /DeviceRGB /BitsPerComponent 8 /Decode [1 0 1 0 1 0]";
    wide_cut(rgb, 50_001, 3, 8, None);
}

#[test]
fn wide_rows_under_a_soft_mask_of_5_bits_cut_stream_as_whole() {
    let mask = wide_gray(WIDE, 5, "/Decode [1 0.3]");
    wide_cut(&format!("{GRAY_3} /SMask 4 0 R"), WIDE, 1, 3, Some(mask));
}

#[test]
fn wide_cmyk_rows_of_2_bits_cut_stream_as_whole() {
    let cmyk = "/ColorSpace /DeviceCMYK /BitsPerComponent 2";
    wide_cut(cmyk, 50_001, 4, 2, None);
}

#[test]
fn wide_stencil_rows_cut_stream_as_whole() {
    wide_cut("/ImageMask true", WIDE, 1, 1, None);
}

/// A 211 × 96 test page in each CCITT encoding, as `hayro-ccitt`'s tests have it.
const CCITT: [(i32, &[u8]); 3] = [
    (
        -1,
        include_bytes!("../../../../hayro-ccitt/tests/streams/g4.bin"),
    ),
    (
        0,
        include_bytes!("../../../../hayro-ccitt/tests/streams/g3_1d.bin"),
    ),
    (
        1,
        include_bytes!("../../../../hayro-ccitt/tests/streams/g3_2d.bin"),
    ),
];

/// CCITT images and stencil masks of the page in encoding `k`, its data cut after
/// every byte for a gray image, and after every eighth for the other kinds, which
/// differ in how rows become texels, not in which rows are kept.
fn ccitt_cut(k: i32, data: &[u8]) {
    let mut cases = Cases::default();
    for (i, kind) in [
        "/ColorSpace /DeviceGray /BitsPerComponent 1",
        "/ColorSpace /DeviceGray /BitsPerComponent 1 /Decode [1 0]",
        "/ImageMask true",
        "/ImageMask true /Decode [1 0]",
    ]
    .into_iter()
    .enumerate()
    {
        for (j, params) in ["", "/BlackIs1 true"].into_iter().enumerate() {
            let dict = format!(
                "{kind} /Filter /CCITTFaxDecode /DecodeParms << /K {k} /Columns 211 {params} >>"
            );
            let step = if i + j == 0 { 1 } else { 8 };
            for cut in (0..=data.len()).step_by(step) {
                let pdf = pdf(&[(image(&dict, 211, 96), data[..cut].to_vec())]);
                cases.check(&asset(&pdf), &format!("{dict}, cut after {cut}"));
            }
        }
    }
    cases.finish();
}

/// A mask: its dictionary, size and data.
type Mask = (String, u32, u32, Vec<u8>);

/// CCITT images that are indexed or have masks of every kind, and images whose
/// mask is CCITT data, the CCITT data cut after every eighth byte (the other data
/// complete), with the declared rows of the page and more than its data holds.
#[test]
fn indexed_and_masked_ccitt_cut_streams_as_whole() {
    let mut cases = Cases::default();
    let (k, data) = CCITT[0];
    let (w, h) = (211, 96);
    let ccitt = |params: &str| {
        format!("/Filter /CCITTFaxDecode /DecodeParms << /K {k} /Columns {w} {params} >>")
    };
    let gray1 = "/ColorSpace /DeviceGray /BitsPerComponent 1";
    let gray8 = |extra: &str| format!("/ColorSpace /DeviceGray /BitsPerComponent 8 {extra}");
    let indexed = "/ColorSpace [/Indexed /DeviceRGB 1 <ff0000 0000ff>] /BitsPerComponent 1";
    let flate_bits =
        |cw: u32, ch: u32| stored_zlib(&samples((cw as usize).div_ceil(8) * ch as usize, 60), 1000);
    let (ow, oh) = OTHER_GRID;
    for rows in [h, 150] {
        // CCITT colour, its mask complete: (colour dictionary, mask entry and object).
        let mut colours: Vec<(String, Option<Mask>)> = vec![
            (format!("{indexed} {}", ccitt("")), None),
            (
                format!("{indexed} /Decode [1 0] {}", ccitt("/BlackIs1 true")),
                None,
            ),
            (format!("{gray1} /Mask [0 100] {}", ccitt("")), None),
            (format!("{gray1} /Decode [0.2 0.8] {}", ccitt("")), None),
            (
                format!(
                    "/ColorSpace [/CalGray << /WhitePoint [0.9505 1 1.089] >>] {}",
                    ccitt("")
                ),
                None,
            ),
            (
                format!("/ImageMask true /Decode [0.2 0.8] {}", ccitt("")),
                None,
            ),
        ];
        for (key, mask, mw, mh, mask_data) in [
            ("/SMask", gray8(""), w, rows, noise((w * rows) as usize, 61)),
            (
                "/SMask",
                gray8("/Matte [0.5]"),
                w,
                rows,
                noise((w * rows) as usize, 62),
            ),
            ("/SMask", gray8(""), ow, oh, noise((ow * oh) as usize, 63)),
            (
                "/Mask",
                "/ImageMask true".into(),
                w,
                rows,
                flate_bits(w, rows),
            ),
        ] {
            let filter = if key == "/Mask" {
                "/Filter /FlateDecode"
            } else {
                ""
            };
            colours.push((
                format!("{gray1} {key} 4 0 R {}", ccitt("")),
                Some((format!("{mask} {filter}"), mw, mh, mask_data.clone())),
            ));
            colours.push((
                format!("{indexed} {key} 4 0 R {}", ccitt("")),
                Some((format!("{mask} {filter}"), mw, mh, mask_data)),
            ));
        }
        for (dict, mask) in &colours {
            for cut in (0..=data.len()).step_by(8).chain([data.len()]) {
                let mut objects = vec![(image(dict, w, rows), data[..cut].to_vec())];
                if let Some((mask, mw, mh, mask_data)) = mask {
                    objects.push((image(mask, *mw, *mh), mask_data.clone()));
                }
                let case = format!("{dict} under {mask:?}, {rows} rows, cut after {cut}");
                let asset = asset(&pdf(&objects));
                if cut == data.len() {
                    assert!(is_streamed(&asset.open(None, None).unwrap()), "{case}");
                }
                cases.check(&asset, &case);
            }
        }
        // Colour complete under a CCITT mask cut short.
        for (key, mask) in [
            ("/SMask", format!("{gray1} {}", ccitt(""))),
            ("/SMask", format!("{gray1} {}", ccitt("/BlackIs1 true"))),
            ("/SMask", format!("{gray1} /Decode [0.2 0.8] {}", ccitt(""))),
            ("/Mask", format!("/ImageMask true {}", ccitt(""))),
            (
                "/Mask",
                format!("/ImageMask true /Decode [1 0] {}", ccitt("")),
            ),
        ] {
            for colour in [
                format!("{gray1} /Filter /FlateDecode"),
                format!("{gray1} {}", ccitt("")),
            ] {
                let colour_data = if colour.contains("CCITT") {
                    data.to_vec()
                } else {
                    flate_bits(w, rows)
                };
                for cut in (0..=data.len()).step_by(8).chain([data.len()]) {
                    let objects = [
                        (
                            image(&format!("{colour} {key} 4 0 R"), w, rows),
                            colour_data.clone(),
                        ),
                        (image(&mask, w, rows), data[..cut].to_vec()),
                    ];
                    let case = format!("{colour} under {mask}, {rows} rows, mask cut after {cut}");
                    let asset = asset(&pdf(&objects));
                    if cut == data.len() {
                        assert!(is_streamed(&asset.open(None, None).unwrap()), "{case}");
                    }
                    cases.check(&asset, &case);
                }
            }
        }
    }
    cases.finish();
}

#[test]
fn ccitt_g4_cut_streams_as_whole() {
    ccitt_cut(CCITT[0].0, CCITT[0].1);
}

#[test]
fn ccitt_g3_1d_cut_streams_as_whole() {
    ccitt_cut(CCITT[1].0, CCITT[1].1);
}

#[test]
fn ccitt_g3_2d_cut_streams_as_whole() {
    ccitt_cut(CCITT[2].0, CCITT[2].1);
}

/// Gray images of 1 and 8 bits whose compressed data has a byte changed, at every
/// byte and in three ways: past a valid zlib header, zlib may find the data corrupt
/// after making bytes, which whole decoding keeps and a request of the first rows
/// returns before the rest is read.
#[test]
fn damaged_flate_streams_as_whole() {
    let mut cases = Cases::default();
    for (bits, w, h) in [(1_usize, 61_usize, 40_usize), (8, 64, 48)] {
        let data = samples((w * bits).div_ceil(8) * h, 54 + bits as u32);
        let compressed = deflate(&data);
        let dict = format!("/ColorSpace /DeviceGray /BitsPerComponent {bits} /Filter /FlateDecode");
        for at in 0..compressed.len() {
            for flip in [0xff, 0x01, 0x80] {
                let mut damaged = compressed.clone();
                damaged[at] ^= flip;
                let pdf = pdf(&[(image(&dict, w as u32, h as u32), damaged)]);
                cases.check(&asset(&pdf), &format!("{dict}, byte {at} ^ {flip:#x}"));
            }
        }
    }
    cases.finish();
}

/// Flate data of which neither zlib nor raw deflate reads a byte (stored blocks whose
/// lengths do not check), which the permissive decoder reads as a request reads it,
/// whole and cut after a sample of bytes: images of it stream as they decode whole.
#[test]
fn flate_only_the_permissive_decoder_reads_streams_as_whole() {
    let mut cases = Cases::default();
    for (dict, w, h, bits) in [
        (
            "/ColorSpace /DeviceGray /BitsPerComponent 1",
            61_u32,
            40_u32,
            1_usize,
        ),
        (
            "/ColorSpace /DeviceGray /BitsPerComponent 3 /SMask 4 0 R",
            W,
            H,
            3,
        ),
        ("/ColorSpace /DeviceGray /BitsPerComponent 8", 64, 48, 8),
        ("/ImageMask true", 61, 40, 1),
    ] {
        let data = samples((w as usize * bits).div_ceil(8) * h as usize, 74);
        let mut encoded = stored_zlib(&data, 100);
        for block in (2..encoded.len() - 4).step_by(100 + 5) {
            encoded[block + 3..block + 5].fill(1);
        }
        let mask = (
            image("/ColorSpace /DeviceGray /BitsPerComponent 4", w, h),
            samples((w as usize * 4).div_ceil(8) * h as usize, 75),
        );
        for cut in (0..=encoded.len()).step_by(7).chain([encoded.len()]) {
            let dict = format!("{dict} /Filter /FlateDecode");
            let objects = [(image(&dict, w, h), encoded[..cut].to_vec()), mask.clone()];
            let case = format!("{dict}, stored lengths that do not check, cut after {cut}");
            check_first_requests(&mut cases, &pdf(&objects), &case);
        }
    }
    cases.finish();
}

/// Data whose zlib reading breaks after 130,988 bytes, while raw deflate reads all of
/// it as other bytes (the zlib header starts a stored block): images of it stream
/// as they decode whole, whose first bytes come from the zlib reading.
#[test]
fn a_zlib_reading_that_breaks_late_streams_as_whole() {
    let mut payload = vec![0x55; 65535];
    payload[65530..].copy_from_slice(&[0x00, 0xff, 0xff, 0x00, 0x00]);
    let mut encoded = vec![
        0x78, 0x01, 0x00, 0xfe, 0xff, 0x01, 0x00, 0xff, 0xff, 0x00, 0x00,
    ];
    encoded.extend(payload);
    encoded.extend([0x00, 0xff, 0xff, 0x00, 0x00]);
    encoded.extend(std::iter::repeat_n(0x55, 65535));
    encoded.extend([0x01, 0x00, 0x00, 0xff, 0xff]);
    let mut cases = Cases::default();
    for (dict, w, h) in [
        ("/ColorSpace /DeviceGray /BitsPerComponent 1", 8, 1),
        ("/ColorSpace /DeviceGray /BitsPerComponent 1", 1024, 1100),
        ("/ColorSpace /DeviceGray /BitsPerComponent 8", 4, 1),
        ("/ColorSpace /DeviceGray /BitsPerComponent 8", 256, 600),
        ("/ImageMask true", 8, 1),
    ] {
        let dict = format!("{dict} /Filter /FlateDecode");
        let pdf = pdf(&[(image(&dict, w, h), encoded.clone())]);
        cases.check(&asset(&pdf), &format!("{dict} {w}x{h}"));
    }
    cases.finish();
    // The first pixels are white, as the zlib reading starts `ff ff`; raw deflate's
    // `01` would make seven black ones.
    let pdf = pdf(&[(
        image(
            "/ColorSpace /DeviceGray /BitsPerComponent 1 /Filter /FlateDecode",
            8,
            1,
        ),
        encoded,
    )]);
    let asset = asset(&pdf);
    let mut source = asset.open(None, None).unwrap();
    assert_eq!(
        request(&mut source, Plane::Image, (8, 1), [0, 0, 8, 1]).data,
        [255; 8]
    );
}
