//! Requests of streamed images return what the same requests of the image decoded
//! whole return.

use super::rows::PIECE_BYTES;
use super::*;
use crate::cache::Cache;
use crate::fixtures::{file, noise, stored_deflate, stored_zlib, stream};
use hayro_syntax::Pdf;
use hayro_syntax::object::{ObjectIdentifier, Stream};
#[cfg(debug_assertions)]
use hayro_syntax::work::{POLL_BYTES, TOLD};
use std::cell::Cell;
use std::ops::Range;

/// A PDF whose objects 3 onwards are `objects`, each a dictionary and its stream
/// data.
pub(super) fn pdf(objects: &[(String, Vec<u8>)]) -> Pdf {
    let mut all = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
    ];
    all.extend(objects.iter().map(|(dict, data)| stream(dict, data)));
    Pdf::new(file(&all, "")).unwrap()
}

pub(super) fn image(dict: &str, w: u32, h: u32) -> String {
    format!("/Type /XObject /Subtype /Image /Width {w} /Height {h} {dict}")
}

pub(super) fn zlib(data: &[u8]) -> Vec<u8> {
    stored_zlib(data, 1000)
}

/// `data` in rows of `row_len` bytes, PNG-filtered (cycling None, Sub, Up, Average,
/// Paeth) with `bpp` bytes per pixel.
fn png_filter(data: &[u8], row_len: usize, bpp: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let zero = vec![0; row_len];
    for (r, row) in data.chunks(row_len).enumerate() {
        let prev = if r == 0 {
            &zero[..]
        } else {
            &data[(r - 1) * row_len..r * row_len]
        };
        let filter = (r % 5) as u8;
        out.push(filter);
        for i in 0..row.len() {
            let a = if i >= bpp { row[i - bpp] as i16 } else { 0 };
            let b = prev[i] as i16;
            let c = if i >= bpp { prev[i - bpp] as i16 } else { 0 };
            let p = match filter {
                0 => 0,
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                _ => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
            };
            out.push(row[i].wrapping_sub(p as u8));
        }
    }
    out
}

/// The image in object 3 as an asset.
pub(super) fn asset(pdf: &Pdf) -> ImageAsset {
    let stream = pdf
        .xref()
        .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
        .unwrap();
    let sink: WarningSinkFn = Arc::new(|_| {});
    ImageXObject::new(&stream, |_| None, &sink, &Cache::new(), None)
        .unwrap()
        .asset(None)
}

/// The image decoded whole, as images that are not streamed are.
pub(super) fn whole(asset: &ImageAsset) -> ImageSource<'_> {
    let meter = Meter::default();
    let decoded = decode_image(&asset.reader().xobject().unwrap(), None, &meter).unwrap();
    let decoded = Decoded::new(decoded.image, decoded.alpha, &meter).unwrap();
    ImageSource::new(
        decoded.layout(),
        Inner::Decoded(decoded),
        DecodeWork::default(),
    )
}

pub(super) fn request(
    source: &mut ImageSource<'_>,
    plane: Plane,
    grid: (u32, u32),
    window: [u32; 4],
) -> ImageRegion {
    source
        .request(&ImageRequest {
            plane,
            grid,
            window,
            cancel: None,
        })
        .unwrap()
}

/// The image plane's rows `rows` at full size, and the work reading them took.
pub(super) fn rows(source: &mut ImageSource<'_>, rows: Range<u32>) -> (Vec<u8>, DecodeWork) {
    let before = source.work();
    let plane = source.layout().image;
    let window = [0, rows.start, plane.width, rows.end];
    let data = request(source, Plane::Image, (plane.width, plane.height), window).data;
    let after = source.work();
    let work = DecodeWork {
        rows_decoded: after.rows_decoded - before.rows_decoded,
        bytes_inflated: after.bytes_inflated - before.bytes_inflated,
    };
    (data, work)
}

pub(super) fn is_streamed(source: &ImageSource<'_>) -> bool {
    !matches!(source.inner, Inner::Decoded(_))
}

/// Streamed requests of every plane, at several grids and windows, equal the same
/// requests of the image decoded whole.
pub(super) fn assert_streams_as_decoded(objects: &[(String, Vec<u8>)]) {
    assert_asset_streams_as_decoded(&asset(&pdf(objects)));
}

pub(super) fn assert_asset_streams_as_decoded(asset: &ImageAsset) {
    let reader = asset.reader();
    let mut streamed = reader.open(None, None).unwrap();
    assert!(is_streamed(&streamed));
    let mut decoded = whole(asset);
    assert_eq!(streamed.layout(), decoded.layout());
    let layout = *streamed.layout();
    for plane in [Plane::Image, Plane::Mask] {
        let Some(p) = layout.plane(plane) else {
            continue;
        };
        let (w, h) = (p.width, p.height);
        for grid in [
            (w, h),
            (w.div_ceil(2), h.div_ceil(3)),
            (1, 1),
            (w, 2.min(h)),
        ] {
            for window in [
                [0, 0, grid.0, grid.1],
                [grid.0 / 2, grid.1 / 2, grid.0, grid.1],
                [0, grid.1 - 1, 1.max(grid.0 / 3), grid.1],
            ] {
                let a = request(&mut streamed, plane, grid, window);
                let b = request(&mut decoded, plane, grid, window);
                assert_eq!(
                    (a.format, &a.data, a.to_image),
                    (b.format, &b.data, b.to_image),
                    "{plane:?} grid {grid:?} window {window:?}"
                );
            }
        }
    }
}

pub(super) fn transfer(four: bool) -> ActiveTransferFunction {
    use crate::function::{Function, TransferFunction};
    let pdf = pdf(&[(
        "/FunctionType 2 /Domain [0 1] /C0 [1] /C1 [0] /N 1".into(),
        Vec::new(),
    )]);
    let function = TransferFunction::new(
        Function::new(&pdf.xref().get(ObjectIdentifier::new(3, 0)).unwrap()).unwrap(),
    );
    if four {
        ActiveTransferFunction::Four(std::array::from_fn(|_| function.clone()))
    } else {
        ActiveTransferFunction::Single(function)
    }
}

#[test]
fn gray_and_rgb_stream_as_decoded() {
    let (w, h) = (13, 9);
    for (dict, data) in [
        (
            "/ColorSpace /DeviceGray /BitsPerComponent 8",
            noise(w * h, 1),
        ),
        (
            "/ColorSpace /DeviceGray /BitsPerComponent 8 /Decode [1 0]",
            noise(w * h, 1),
        ),
        (
            "/ColorSpace /DeviceRGB /BitsPerComponent 8",
            noise(w * h * 3, 2),
        ),
        (
            "/ColorSpace /DeviceRGB /BitsPerComponent 8 /Decode [1 0 1 0 1 0]",
            noise(w * h * 3, 2),
        ),
    ] {
        let (iw, ih) = (w as u32, h as u32);
        assert_streams_as_decoded(&[(image(dict, iw, ih), data.clone())]);
        let flate = format!("{dict} /Filter /FlateDecode");
        assert_streams_as_decoded(&[(image(&flate, iw, ih), zlib(&data))]);
        let n = data.len() / (w * h);
        let predicted = format!(
            "{dict} /Filter /FlateDecode /DecodeParms << /Predictor 15 /Colors {n} /Columns {w} >>"
        );
        assert_streams_as_decoded(&[(
            image(&predicted, iw, ih),
            zlib(&png_filter(&data, w * n, n)),
        )]);
    }
}

#[test]
fn transfer_functions_colour_keys_and_matte_stream_as_decoded() {
    for (space, channels) in [("DeviceGray", 1), ("DeviceRGB", 3)] {
        for four in [false, true] {
            for mask in [
                String::new(),
                format!("/Mask [{}]", "0 120 ".repeat(channels)),
                "/SMask 4 0 R".into(),
            ] {
                let (w, h) = (13, 9);
                let samples = noise(w * h * channels, 37);
                let pdf = pdf(&[
                    (
                        image(
                            &format!(
                                "/ColorSpace /{space} /BitsPerComponent 8 /Decode [{}] \
                                 /Filter /FlateDecode /DecodeParms << /Predictor 15 \
                                 /Colors {channels} /Columns {w} >> {mask}",
                                "1 0 ".repeat(channels)
                            ),
                            w as u32,
                            h as u32,
                        ),
                        zlib(&png_filter(&samples, w * channels, channels)),
                    ),
                    (
                        image(
                            &format!(
                                "/ColorSpace /DeviceGray /BitsPerComponent 8 /Matte [{}]",
                                "0.5 ".repeat(channels)
                            ),
                            w as u32,
                            h as u32,
                        ),
                        noise(w * h, 38),
                    ),
                ]);
                let mut asset = asset(&pdf);
                Arc::get_mut(&mut asset.0).unwrap().transfer_function = Some(transfer(four));
                assert_asset_streams_as_decoded(&asset);
            }
        }
    }
}

#[test]
fn masks_stream_as_decoded() {
    let (w, h) = (11_u32, 7_u32);
    let rgb = noise((w * h * 3) as usize, 3);
    let colour = "/ColorSpace /DeviceRGB /BitsPerComponent 8";
    let smask = |mw: u32, mh: u32, extra: &str| {
        (
            image(
                &format!("/ColorSpace /DeviceGray /BitsPerComponent 8 {extra}"),
                mw,
                mh,
            ),
            noise((mw * mh) as usize, mw + mh),
        )
    };
    // A soft mask on the colour's grid, on another grid, and one with /Matte.
    for mask in [
        smask(w, h, ""),
        smask(5, 3, ""),
        smask(w, h, "/Matte [0.2 0.5 1]"),
    ] {
        assert_streams_as_decoded(&[
            (image(&format!("{colour} /SMask 4 0 R"), w, h), rgb.clone()),
            mask,
        ]);
    }
    // Interpolation differs: another plane, though the size is the same.
    assert_streams_as_decoded(&[
        (image(&format!("{colour} /SMask 4 0 R"), w, h), rgb.clone()),
        smask(w, h, "/Interpolate true"),
    ]);
    // A bilevel mask stream of an odd width, inverted or not.
    for decode in ["", "/Decode [1 0]"] {
        assert_streams_as_decoded(&[
            (image(&format!("{colour} /Mask 4 0 R"), w, h), rgb.clone()),
            (
                image(&format!("/ImageMask true {decode}"), w, h),
                noise((w.div_ceil(8) * h) as usize, 9),
            ),
        ]);
    }
    // A colour key.
    assert_streams_as_decoded(&[(
        image(&format!("{colour} /Mask [0 120 60 255 0 200]"), w, h),
        rgb.clone(),
    )]);
}

/// A mask whose data is empty, broken, or ends after a few rows: the colour is drawn
/// unmasked, or under the rows the mask holds, stretched over the image, as when the
/// image is decoded whole.
#[test]
fn masks_that_end_early_stream_as_decoded() {
    let (w, h) = (11_u32, 7_u32);
    let rgb = noise((w * h * 3) as usize, 3);
    let colour = "/ColorSpace /DeviceRGB /BitsPerComponent 8";
    let gray = noise((w * h) as usize, 10);
    let bits = noise((w.div_ceil(8) * h) as usize, 11);
    let masks = [
        (
            "/SMask",
            "/ColorSpace /DeviceGray /BitsPerComponent 8",
            &gray,
            w,
        ),
        ("/Mask", "/ImageMask true", &bits, w.div_ceil(8)),
    ];
    for (key, dict, data, row) in masks {
        let row = row as usize;
        for cut in [&[][..], &data[..3 * row], &data[..3 * row + 1]] {
            for (filter, data) in [("", cut.to_vec()), ("/Filter /FlateDecode", zlib(cut))] {
                assert_streams_as_decoded(&[
                    (image(&format!("{colour} {key} 4 0 R"), w, h), rgb.clone()),
                    (image(&format!("{dict} {filter}"), w, h), data),
                ]);
            }
        }
        // Data a Flate decoder cannot read, not even as raw deflate.
        assert_streams_as_decoded(&[
            (image(&format!("{colour} {key} 4 0 R"), w, h), rgb.clone()),
            (
                image(&format!("{dict} /Filter /FlateDecode"), w, h),
                vec![0xFF; 40],
            ),
        ]);
    }
}

#[test]
fn data_that_ends_early_leaves_the_rest_transparent() {
    let (w, h) = (10_u32, 8_u32);
    let gray = noise((w * h) as usize, 4);
    // A short last row is padded; the rows after it are missing.
    let cut = &gray[..(w * 3 + 4) as usize];
    let dict = image("/ColorSpace /DeviceGray /BitsPerComponent 8", w, h);
    for (dict, data) in [
        (dict.clone(), cut.to_vec()),
        (format!("{dict} /Filter /FlateDecode"), zlib(cut)),
    ] {
        let pdf = pdf(&[(dict, data)]);
        let asset = asset(&pdf);
        let reader = asset.reader();
        let mut streamed = reader.open(None, None).unwrap();
        let mut decoded = whole(&asset);
        assert_eq!(decoded.layout().image.height, 4);
        let a = request(&mut streamed, Plane::Image, (w, h), [0, 0, w, h]);
        let b = request(&mut decoded, Plane::Image, (w, 4), [0, 0, w, 4]);
        assert_eq!(a.format, PixelFormat::LumaAlpha);
        let (present, missing) = a.data.split_at((w * 4 * 2) as usize);
        let expected: Vec<u8> = b.data.iter().flat_map(|&v| [v, 255]).collect();
        assert_eq!(present, expected);
        assert!(missing.iter().all(|&v| v == 0));
        assert_eq!(streamed.work().rows_decoded, 4);
        // Bins over present and missing rows are covered in part.
        let half = request(&mut streamed, Plane::Image, (1, 2), [0, 0, 1, 2]);
        assert_eq!(half.data[1], 255);
        assert_eq!(half.data[3], 0);
    }
}

#[test]
fn a_huge_declared_image_with_little_data_costs_the_data() {
    // The short row is kept (padding it takes under `MIN_ROW_PAD`); a whole decode
    // would hold `w` bytes, a request holds its bins.
    let (w, h) = (60_000, 1_000_000);
    let pdf = pdf(&[(
        image("/ColorSpace /DeviceGray /BitsPerComponent 8", w, h),
        vec![7; 100],
    )]);
    let asset = asset(&pdf);
    let reader = asset.reader();
    let mut source = reader.open(None, None).unwrap();
    let region = request(&mut source, Plane::Image, (100, 100), [0, 0, 100, 100]);
    assert_eq!(region.format, PixelFormat::LumaAlpha);
    assert_eq!(source.work().rows_decoded, 1);
    assert!(region.data.chunks(2).all(|t| t == [0, 0]));
    let region = request(&mut source, Plane::Image, (w, h), [0, 0, 4, 2]);
    assert_eq!(
        region.data,
        [7, 255, 7, 255, 7, 255, 7, 255, 0, 0, 0, 0, 0, 0, 0, 0]
    );
}

/// Rows read in pieces, under every kind of mask and from windows that start within
/// a byte of a bilevel mask, equal the rows decoded whole.
#[test]
fn rows_wider_than_a_piece_stream_as_decoded() {
    let (w, h) = (70_001_u32, 3_u32);
    let rgb = noise((w * h * 3) as usize, 13);
    let colour = "/ColorSpace /DeviceRGB /BitsPerComponent 8";
    let gray = |extra: &str| {
        (
            image(
                &format!("/ColorSpace /DeviceGray /BitsPerComponent 8 {extra}"),
                w,
                h,
            ),
            noise((w * h) as usize, 14),
        )
    };
    for mask in [gray(""), gray("/Matte [0.2 0.5 1]")] {
        assert_streams_as_decoded(&[
            (image(&format!("{colour} /SMask 4 0 R"), w, h), rgb.clone()),
            mask,
        ]);
    }
    assert_streams_as_decoded(&[
        (image(&format!("{colour} /Mask 4 0 R"), w, h), rgb.clone()),
        (
            image("/ImageMask true", w, h),
            noise((w.div_ceil(8) * h) as usize, 15),
        ),
    ]);
    assert_streams_as_decoded(&[(
        image(&format!("{colour} /Mask [0 120 60 255 0 200]"), w, h),
        rgb.clone(),
    )]);
}

/// A first row too short to pad, read in pieces, is dropped from texels kept whole
/// and from sums in either order, also after pieces of it were pushed, and when its
/// data ends past the window.
#[test]
fn a_short_wide_row_is_dropped() {
    let (w, h) = (300_000_u32, 3_u32);
    let pdf = pdf(&[(
        image("/ColorSpace /DeviceGray /BitsPerComponent 8", w, h),
        noise(70_000, 16),
    )]);
    let asset = asset(&pdf);
    let reader = asset.reader();
    let mut streamed = reader.open(None, None).unwrap();
    for grid in [(w, h), (w / 2, 2), (70, 2)] {
        for window in [[0, 0, grid.0, grid.1], [0, 0, grid.0 / 70, 1]] {
            let a = request(&mut streamed, Plane::Image, grid, window);
            assert_eq!(
                a.data.len(),
                ((window[2] - window[0]) * (window[3] - window[1]) * 2) as usize
            );
            assert!(a.data.iter().all(|&v| v == 0), "{grid:?} {window:?}");
        }
    }
    assert_eq!(streamed.work().rows_decoded, 0);
}

/// A row the data ends within pushes the texels its data holds, not the texels the
/// row's width declares: one byte of a row 20 million samples wide is dropped before
/// any texel is pushed, and the padding of a row kept, whose padded samples are not
/// zero once inverted, is pushed at once.
#[test]
fn a_row_cut_short_pushes_the_texels_it_holds() {
    let gray = "/ColorSpace /DeviceGray /BitsPerComponent 8 /Decode [1 0]";
    for (w, data, kept) in [(20_000_000, 1, 0), (2_000_000, 1_000_001, 1)] {
        let pdf = pdf(&[(image(gray, w, 2), noise(data, 17))]);
        let asset = asset(&pdf);
        let reader = asset.reader();
        let mut streamed = reader.open(None, None).unwrap();
        crate::x_object::area::PUSHED.set(0);
        let a = request(&mut streamed, Plane::Image, (2, 2), [1, 0, 2, 1]);
        let pushed = crate::x_object::area::PUSHED.get();
        assert!(pushed <= PIECE_BYTES as u64, "{w}: {pushed} texels pushed");
        assert_eq!(streamed.work().rows_decoded, kept);
        if kept == 0 {
            assert_eq!(a.data, [0, 0]);
        } else {
            // Whole decoding keeps the rows present.
            let b = request(&mut whole(&asset), Plane::Image, (2, 1), [1, 0, 2, 1]);
            assert_eq!((a.format, a.data), (b.format, b.data));
        }
    }
}

/// Rows the data ends within, read in pieces and padded past the data at once, equal
/// the rows decoded whole: with padded samples that are not zero once converted, a
/// colour key, and a mask whose data ends before or after the colour's.
#[test]
fn rows_cut_short_stream_as_decoded() {
    let (w, h) = (150_001_u32, 2_u32);
    let colour = "/ColorSpace /DeviceRGB /BitsPerComponent 8 /Decode [1 0 1 0 1 0]";
    // Into the last row, the first or the second; whole decoding keeps the rows
    // present.
    let second = noise((w * 3 + w * 3 / 2 + 7) as usize, 18);
    for (rows, rgb) in [(1, &second[..(w * 3 / 2 + 7) as usize]), (h, &second)] {
        assert_streams_as_decoded(&[(image(colour, w, rows), rgb.to_vec())]);
        assert_streams_as_decoded(&[(
            image(&format!("{colour} /Mask [0 120 60 255 0 200]"), w, rows),
            rgb.to_vec(),
        )]);
    }
    let gray = |cut: u32, extra: &str| {
        (
            image(
                &format!("/ColorSpace /DeviceGray /BitsPerComponent 8 {extra}"),
                w,
                h,
            ),
            noise(cut as usize, 19),
        )
    };
    for mask in [
        gray(w * 2, ""),
        gray(w + w / 4, ""),
        gray(w + w * 3 / 4, ""),
        gray(w + w / 4, "/Matte [0.2 0.5 1]"),
        gray(w + w * 3 / 4, "/Matte [0.2 0.5 1]"),
    ] {
        assert_streams_as_decoded(&[
            (
                image(&format!("{colour} /SMask 4 0 R"), w, h),
                second.clone(),
            ),
            mask,
        ]);
    }
    let row = w.div_ceil(8);
    for cut in [row + row / 4, row + row * 3 / 4] {
        assert_streams_as_decoded(&[
            (
                image(&format!("{colour} /Mask 4 0 R"), w, h),
                second.clone(),
            ),
            (
                image("/ImageMask true /Decode [1 0]", w, h),
                noise(cut as usize, 20),
            ),
        ]);
    }
}

#[test]
fn data_that_is_not_zlib_restarts_as_raw_deflate() {
    let (w, h) = (9_u32, 5_u32);
    assert_streams_as_decoded(&[(
        image(
            "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
            w,
            h,
        ),
        stored_deflate(&noise((w * h) as usize, 5), 1000),
    )]);
}

/// Colour and soft mask both raw deflate: each restarts once, and a restart of one
/// rewinds the other without making it restart again.
#[test]
fn colour_and_mask_that_both_restart_finish() {
    let (w, h) = (9_u32, 5_u32);
    let flate = "/BitsPerComponent 8 /Filter /FlateDecode";
    assert_streams_as_decoded(&[
        (
            image(
                &format!("/ColorSpace /DeviceRGB {flate} /SMask 4 0 R"),
                w,
                h,
            ),
            stored_deflate(&noise((w * h * 3) as usize, 7), 1000),
        ),
        (
            image(&format!("/ColorSpace /DeviceGray {flate}"), w, h),
            stored_deflate(&noise((w * h) as usize, 8), 1000),
        ),
    ]);
}

/// A request polls its hook as it starts, and is cancelled within two pieces of
/// work of the hook asking, wherever the work is: reading the columns asked for,
/// skipping to them, past the rest of the row or past rows before them, of the colour
/// or of a mask on its own grid, or pushing decoded texels. The source stays usable:
/// the same request then returns what decoding the image whole does.
#[test]
fn a_request_is_cancelled_within_two_pieces() {
    let w = 2_000_000_u32;
    let plain = "/ColorSpace /DeviceGray /BitsPerComponent 8";
    let gray = &format!("{plain} /Filter /FlateDecode");
    let masked = |key: &str, dict: &str, bytes: u32| {
        vec![
            (image(&format!("{plain} /{key} 4 0 R"), 1, 1), vec![7]),
            (image(dict, w, 1), zlib(&noise(bytes as usize, 22))),
        ]
    };
    let streamed_mask = vec![[0, 0, 1, 1], [w - 1, 0, w, 1]];
    for (objects, plane, grid, windows) in [
        (
            vec![(image(gray, w, 2), zlib(&noise(2 * w as usize, 21)))],
            Plane::Image,
            (w, 2),
            // Row 1 first: once row 0 is read whole, a state saved at row 1 skips it.
            vec![[0, 1, 1, 2], [0, 0, 1, 1], [w - 1, 0, w, 1], [0, 0, w, 1]],
        ),
        (masked("SMask", gray, w), Plane::Mask, (w, 1), streamed_mask),
        (
            masked("Mask", "/ImageMask true /Filter /FlateDecode", w / 8),
            Plane::Mask,
            (w, 1),
            vec![[w - 1, 0, w, 1]],
        ),
        (
            masked("SMask", &format!("{gray} /Decode [0.2 0.9]"), w),
            Plane::Mask,
            (w, 1),
            vec![[0, 0, w, 1]],
        ),
    ] {
        let asset = asset(&pdf(&objects));
        let reader = asset.reader();
        let mut streamed = reader.open(None, None).unwrap();
        assert!(is_streamed(&streamed));
        for window in windows {
            assert_cancelled(&mut streamed, plane, grid, window);
            let a = request(&mut streamed, plane, grid, window);
            let b = request(&mut whole(&asset), plane, grid, window);
            assert!(a.format == b.format && a.data == b.data, "{window:?}");
        }
    }
    let pdf = pdf(&[(image(gray, w, 2), zlib(&noise(2 * w as usize, 23)))]);
    let asset = asset(&pdf);
    let mut decoded = whole(&asset);
    let expected = request(&mut decoded, Plane::Image, (w, 2), [0, 0, w, 1]);
    assert_cancelled(&mut decoded, Plane::Image, (w, 2), [0, 0, w, 1]);
    let always = ImageRequest {
        plane: Plane::Image,
        grid: (w, 2),
        window: [0, 1, 1, 2],
        cancel: Some(&|| true),
    };
    assert!(matches!(
        decoded.request(&always),
        Err(RequestError::Cancelled)
    ));
    let a = request(&mut decoded, Plane::Image, (w, 2), [0, 0, w, 1]);
    assert!(a.data == expected.data);
}

/// A first open stopped by its hook returns within three polls' worth of work, wherever
/// the work is: counting a Flate soft mask's rows, decoding a mask whole, or decoding
/// an image that is not streamed whole (through a filter, or converting 16-bit
/// samples). The asset is left as it was: opening it again gives what decoding the
/// image whole does.
#[test]
#[cfg(debug_assertions)]
fn a_first_open_is_cancelled_within_bounded_work() {
    let (w, h) = (2000_u32, 2000_u32);
    let gray = "/ColorSpace /DeviceGray /BitsPerComponent 8";
    let hex = |data: &[u8]| {
        data.iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            .into_bytes()
    };
    let masked = |filter: &str, data: Vec<u8>| {
        vec![
            (image(&format!("{gray} /SMask 4 0 R"), 1, 1), vec![7]),
            (image(&format!("{gray} {filter}"), w, h), data),
        ]
    };
    let noise = noise((w * h) as usize, 31);
    for (name, objects) in [
        (
            "Flate soft mask",
            masked("/Filter /FlateDecode", zlib(&noise)),
        ),
        (
            "decoded soft mask",
            masked("/Filter /ASCIIHexDecode", hex(&noise)),
        ),
        (
            "hex image",
            vec![(
                image(&format!("{gray} /Filter /ASCIIHexDecode"), w, h),
                hex(&noise),
            )],
        ),
        (
            "16-bit image",
            vec![(
                image("/ColorSpace /DeviceGray /BitsPerComponent 16", w, h / 2),
                noise.clone(),
            )],
        ),
    ] {
        let pdf = pdf(&objects);
        let asset = asset(&pdf);
        let polls = Cell::new(0);
        let hook = || {
            polls.set(polls.get() + 1);
            polls.get() > 1
        };
        TOLD.set(0);
        rows::READ.set(0);
        let reader = asset.reader();
        let opened = reader.open(None, Some(&hook));
        assert!(matches!(opened, Err(RequestError::Cancelled)), "{name}");
        assert_eq!(polls.get(), 2, "{name}");
        let (told, read) = (TOLD.get(), rows::READ.get());
        assert!(
            told < 3 * POLL_BYTES && read < 3 * POLL_BYTES as u64,
            "{name}: {told} bytes of work, {read} bytes read"
        );
        let reader = asset.reader();
        let mut source = reader.open(None, None).unwrap();
        let mut expected = whole(&asset);
        for plane in [Plane::Image, Plane::Mask] {
            let Some(layout) = source.layout().plane(plane).copied() else {
                continue;
            };
            let grid = (layout.width, layout.height);
            let window = [0, 0, layout.width, layout.height];
            let a = request(&mut source, plane, grid, window);
            let b = request(&mut expected, plane, grid, window);
            assert!(a.format == b.format && a.data == b.data, "{name} {plane:?}");
        }
    }
}

/// Rows missing from the data are padded a piece at a time: a request of a huge grid
/// whose data is empty is cancelled within three polls' worth of work.
#[test]
#[cfg(debug_assertions)]
fn padding_missing_rows_is_cancelled_within_bounded_work() {
    let w = 2_000_000;
    let pdf = pdf(&[(
        image("/ColorSpace /DeviceGray /BitsPerComponent 8", w, 2),
        vec![],
    )]);
    let asset = asset(&pdf);
    let reader = asset.reader();
    let mut source = reader.open(None, None).unwrap();
    let request_of = |cancel| ImageRequest {
        plane: Plane::Image,
        grid: (w, 2),
        window: [0, 0, w, 2],
        cancel,
    };
    let polls = Cell::new(0);
    let hook = || {
        polls.set(polls.get() + 1);
        polls.get() > 1
    };
    TOLD.set(0);
    crate::x_object::area::PUSHED.set(0);
    let result = source.request(&request_of(Some(&hook)));
    assert!(matches!(result, Err(RequestError::Cancelled)));
    assert_eq!(polls.get(), 2);
    let told = TOLD.get();
    assert!(told < 3 * POLL_BYTES, "{told} bytes of work");
    let region = source.request(&request_of(None)).unwrap();
    assert_eq!(region.data.len(), 2 * w as usize * 2);
}

/// Rows of a mask decoded whole are skipped without work: a request of the last row of
/// a mask twenty million rows high counts next to nothing.
#[test]
#[cfg(debug_assertions)]
fn rows_of_a_decoded_mask_are_skipped_without_work() {
    let h = 20_000_000_u32;
    let gray = "/ColorSpace /DeviceGray /BitsPerComponent 8";
    let mut runs = Vec::new();
    for run in 0..h / 128 {
        runs.extend([129, (run % 251) as u8]);
    }
    runs.push(128);
    let pdf = pdf(&[
        (image(&format!("{gray} /SMask 4 0 R"), 1, 1), vec![7]),
        (
            image(&format!("{gray} /Filter /RunLengthDecode"), 1, h),
            runs,
        ),
    ]);
    let asset = asset(&pdf);
    let reader = asset.reader();
    let mut source = reader.open(None, None).unwrap();
    assert!(is_streamed(&source));
    let polls = Cell::new(0);
    let hook = || {
        polls.set(polls.get() + 1);
        false
    };
    TOLD.set(0);
    let region = source
        .request(&ImageRequest {
            plane: Plane::Mask,
            grid: (1, h),
            window: [0, h - 1, 1, h],
            cancel: Some(&hook),
        })
        .unwrap();
    assert_eq!(region.data, [((h / 128 - 1) % 251) as u8]);
    assert_eq!(polls.get(), 1);
    assert!(TOLD.get() < 1024, "{} bytes of work", TOLD.get());
}

/// `source`'s request of `window` with a hook that asks to stop from its second poll
/// on is cancelled, the hook polled twice, having read and pushed less than two
/// pieces.
fn assert_cancelled(
    source: &mut ImageSource<'_>,
    plane: Plane,
    grid: (u32, u32),
    window: [u32; 4],
) {
    let polls = Cell::new(0);
    let cancel = || {
        polls.set(polls.get() + 1);
        polls.get() > 1
    };
    rows::READ.set(0);
    crate::x_object::area::PUSHED.set(0);
    let result = source.request(&ImageRequest {
        plane,
        grid,
        window,
        cancel: Some(&cancel),
    });
    assert!(matches!(result, Err(RequestError::Cancelled)), "{window:?}");
    assert_eq!(polls.get(), 2, "{window:?}");
    let (read, pushed) = (rows::READ.get(), crate::x_object::area::PUSHED.get());
    assert!(
        read < 2 * PIECE_BYTES as u64 && pushed < 2 * PIECE_BYTES as u64,
        "{window:?}: {read} bytes read, {pushed} texels pushed"
    );
}

#[test]
fn requests_are_checked() {
    let (w, h) = (4_u32, 200_u32);
    let pdf = pdf(&[(
        image("/ColorSpace /DeviceGray /BitsPerComponent 8", w, h),
        noise((w * h) as usize, 6),
    )]);
    let asset = asset(&pdf);
    let reader = asset.reader();
    let mut source = reader.open(None, None).unwrap();
    for (plane, grid, window) in [
        (Plane::Mask, (w, h), [0, 0, 1, 1]),
        (Plane::Image, (w + 1, h), [0, 0, 1, 1]),
        (Plane::Image, (w, h), [0, 0, 0, 1]),
        (Plane::Image, (w, h), [0, 0, w + 1, 1]),
        (Plane::Image, (0, h), [0, 0, 0, 1]),
    ] {
        let result = source.request(&ImageRequest {
            plane,
            grid,
            window,
            cancel: None,
        });
        assert_eq!(result.unwrap_err(), RequestError::Invalid);
    }
}

/// A stencil mask's asset has one plane, the alpha `with_stencil` hands the paint
/// through: with the default `/Decode`, 0 bits paint (255) and 1 bits do not.
#[test]
fn a_stencil_asset_is_the_alpha_the_paint_is_drawn_through() {
    let (w, h) = (10, 3);
    let bits = [
        0b1010_1010,
        0b1100_0000,
        0b0000_0000,
        0b0000_0000,
        0b1111_1111,
        0b1100_0000,
    ];
    let pdf = pdf(&[(
        image("/ImageMask true /BitsPerComponent 1", w, h),
        bits.to_vec(),
    )]);
    let stream = pdf
        .xref()
        .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
        .unwrap();
    let sink: WarningSinkFn = Arc::new(|_| {});
    let obj = ImageXObject::new(&stream, |_| None, &sink, &Cache::new(), None).unwrap();
    let expected = obj.decoded_mask(None).unwrap().luma.data;
    assert_eq!(&expected[..10], &[0, 255, 0, 255, 0, 255, 0, 255, 0, 0]);
    let asset = obj.asset(None);
    let reader = asset.reader();
    let mut source = reader.open(None, None).unwrap();
    let layout = *source.layout();
    assert_eq!(layout.mask, None);
    assert_eq!(layout.image.format, PixelFormat::Alpha);
    assert_eq!((layout.image.width, layout.image.height), (w, h));
    let full = request(&mut source, Plane::Image, (w, h), [0, 0, w, h]);
    assert_eq!(full.data, expected);
    let halved = request(&mut source, Plane::Image, (5, 2), [0, 0, 5, 2]);
    assert_eq!(halved.data.len(), 10);
}
