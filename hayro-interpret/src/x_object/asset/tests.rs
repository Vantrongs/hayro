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

/// The image decoded whole, as images that are not streamed are; a stencil mask as
/// its alpha.
pub(super) fn whole(asset: &ImageAsset) -> ImageSource<'_> {
    try_whole(asset).unwrap()
}

/// [`whole`]; `None` where decoding the image whole fails.
pub(super) fn try_whole(asset: &ImageAsset) -> Option<ImageSource<'_>> {
    let meter = Meter::default();
    let obj = asset.xobject().unwrap();
    let decoded = if obj.kind == ImageKind::StencilMask {
        Decoded::stencil(decode_mask(&obj, None, &meter)?)
    } else {
        let decoded = decode_image(&obj, None, &meter)?;
        Decoded::new(decoded, &meter).unwrap()
    };
    Some(ImageSource::new(
        decoded.layout(),
        Inner::Decoded(decoded),
        DecodeWork::default(),
    ))
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
    let mut streamed = asset.open(None, None).unwrap();
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
        (
            &format!("/ColorSpace {} /BitsPerComponent 8", indexed(255)),
            noise(w * h, 3),
        ),
        (
            &format!(
                "/ColorSpace {} /BitsPerComponent 8 /Decode [255 0]",
                indexed(255)
            ),
            noise(w * h, 3),
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

/// An `Indexed` colour space over `DeviceRGB` of `hival + 1` colours.
fn indexed(hival: usize) -> String {
    let palette: String = noise(3 * (hival + 1), 40)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("[/Indexed /DeviceRGB {hival} <{palette}>]")
}

/// Samples of 1, 2 and 4 bits (and 3, outside the specification), in rows of a
/// width that does not fill their last byte, stream as they decode whole: gray, RGB
/// and indexed, through default, inverted and other decode arrays, stored, Flate, and
/// Flate with a predictor (which takes no 3-bit samples).
#[test]
fn low_bit_samples_stream_as_decoded() {
    let (w, h) = (13_u32, 9_u32);
    for bits in [1_usize, 2, 3, 4] {
        let max = (1 << bits) - 1;
        let spaces = [
            ("/DeviceGray".to_string(), 1, String::new()),
            ("/DeviceGray".into(), 1, "/Decode [1 0]".into()),
            ("/DeviceGray".into(), 1, "/Decode [0.2 0.7]".into()),
            ("/DeviceRGB".into(), 3, String::new()),
            ("/DeviceRGB".into(), 3, "/Decode [1 0 0 1 0.5 0.25]".into()),
            (indexed(max), 1, String::new()),
            (indexed(max), 1, format!("/Decode [{max} 0]")),
        ];
        for (space, n, decode) in spaces {
            let row = (w as usize * n * bits).div_ceil(8);
            let data = noise(row * h as usize, (bits + n) as u32);
            let dict = format!("/ColorSpace {space} /BitsPerComponent {bits} {decode}");
            let name = format!("{bits} bits, {space} {decode}");
            named(&name, || {
                assert_streams_as_decoded(&[(image(&dict, w, h), data.clone())]);
            });
            let flate = format!("{dict} /Filter /FlateDecode");
            named(&name, || {
                assert_streams_as_decoded(&[(image(&flate, w, h), zlib(&data))]);
            });
            if bits == 3 {
                continue;
            }
            let predicted = format!(
                "{dict} /Filter /FlateDecode /DecodeParms << /Predictor 15 /Colors {n} \
                 /BitsPerComponent {bits} /Columns {w} >>"
            );
            let filtered = zlib(&png_filter(&data, row, (n * bits).div_ceil(8)));
            named(&name, || {
                assert_streams_as_decoded(&[(image(&predicted, w, h), filtered)]);
            });
        }
    }
}

/// Runs `check`, naming `case` when it fails.
pub(super) fn named(case: &str, check: impl FnOnce()) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(check));
    assert!(result.is_ok(), "{case}");
}

/// Packed samples under every kind of alpha and a transfer function stream as they
/// decode whole: a colour key compares stored values.
#[test]
fn low_bit_samples_with_masks_stream_as_decoded() {
    let (w, h) = (13_u32, 9_u32);
    for (space, n, bits, key) in [
        ("/DeviceGray", 1, 2, "1 2"),
        ("/DeviceRGB", 3, 4, "0 7 3 9 8 15"),
        ("/DeviceGray", 1, 1, "1 1"),
    ] {
        for four in [false, true] {
            for mask in [
                String::new(),
                format!("/Mask [{key}]"),
                "/SMask 4 0 R".into(),
                "/SMask 5 0 R".into(),
                "/Mask 6 0 R".into(),
            ] {
                let row = (w as usize * n * bits).div_ceil(8);
                let pdf = pdf(&[
                    (
                        image(
                            &format!(
                                "/ColorSpace {space} /BitsPerComponent {bits} \
                                 /Filter /FlateDecode {mask}"
                            ),
                            w,
                            h,
                        ),
                        zlib(&noise(row * h as usize, 41)),
                    ),
                    (
                        image("/ColorSpace /DeviceGray /BitsPerComponent 8", w, h),
                        noise((w * h) as usize, 42),
                    ),
                    (
                        image(
                            &format!(
                                "/ColorSpace /DeviceGray /BitsPerComponent 8 /Matte [{}]",
                                "0.5 ".repeat(n)
                            ),
                            w,
                            h,
                        ),
                        noise((w * h) as usize, 43),
                    ),
                    (
                        image("/ImageMask true", w, h),
                        noise((w.div_ceil(8) * h) as usize, 44),
                    ),
                ]);
                let mut asset = asset(&pdf);
                Arc::get_mut(&mut asset.0).unwrap().transfer_function = Some(transfer(four));
                named(&format!("{space} {bits} bits {mask} four {four}"), || {
                    assert_asset_streams_as_decoded(&asset);
                });
            }
        }
    }
}

/// Stencil masks with no filter or Flate stream as they decode whole, as the alpha
/// the paint is drawn through, whatever their `/Decode` and width: rows of a width
/// not filling their last byte, and rows read in pieces from windows that start
/// within a byte.
#[test]
fn stencil_masks_stream_as_decoded() {
    for (w, h) in [(13_u32, 9_u32), (8, 3), (150_001, 2)] {
        let data = noise((w.div_ceil(8) * h) as usize, w);
        for decode in ["", "/Decode [1 0]", "/BitsPerComponent 1 /Decode [0 1]"] {
            let dict = format!("/ImageMask true {decode}");
            named(&format!("{w} {decode}"), || {
                assert_streams_as_decoded(&[(image(&dict, w, h), data.clone())]);
            });
            let flate = format!("{dict} /Filter /FlateDecode");
            named(&format!("{w} {decode} Flate"), || {
                assert_streams_as_decoded(&[(image(&flate, w, h), zlib(&data))]);
            });
        }
    }
}

/// A stencil mask whose `/Decode` maps its bits through other values than 0 and 1
/// streams, as it decodes whole.
#[test]
fn a_stencil_with_another_decode_streams_as_decoded() {
    let pdf = pdf(&[(
        image("/ImageMask true /Decode [0.2 0.8]", 13, 9),
        noise(18, 45),
    )]);
    let asset = asset(&pdf);
    assert!(is_streamed(&asset.open(None, None).unwrap()));
    differential::assert_streams_as_whole(&asset, "stencil /Decode [0.2 0.8]");
}

/// Rows of packed samples the data ends within stream as they decode whole: read as
/// stored zero, whose value is not zero once decoded, and a stencil mask's painted
/// or not as its `/Decode` says.
#[test]
fn packed_rows_cut_short_stream_as_decoded() {
    for w in [13_u32, 150_001] {
        for (dict, n, bits) in [
            (
                "/ColorSpace /DeviceGray /BitsPerComponent 1 /Decode [1 0]",
                1,
                1,
            ),
            (
                "/ColorSpace /DeviceRGB /BitsPerComponent 4 /Decode [1 0 1 0 1 0]",
                3,
                4,
            ),
            ("/ImageMask true", 1, 1),
            ("/ImageMask true /Decode [1 0]", 1, 1),
            ("/ImageMask true /BitsPerComponent 8", 1, 8),
        ] {
            let row = (w as usize * n * bits).div_ceil(8);
            let data = noise(2 * row, 46);
            // Into the last row, the first or the second.
            let half = row.div_ceil(2);
            for (rows, cut) in [(1, half), (2, row + half), (2, row + 1)] {
                named(&format!("{w} {dict} {cut}"), || {
                    assert_streams_as_decoded(&[(image(dict, w, rows), data[..cut].to_vec())]);
                });
            }
        }
    }
}

/// A stencil mask's rows its data does not hold whole are stored zero: painted, and
/// with `/Decode [1 0]` not painted.
#[test]
fn a_stencil_that_ends_early_reads_the_rest_as_zero() {
    let (w, h) = (10_u32, 8_u32);
    let row = w.div_ceil(8);
    let bits = noise((row * h) as usize, 47);
    for (decode, fill) in [("", 255), ("/Decode [1 0]", 0)] {
        let pdf = pdf(&[(
            image(&format!("/ImageMask true {decode}"), w, h),
            bits[..(row * 3 + 1) as usize].to_vec(),
        )]);
        let asset = asset(&pdf);
        let mut streamed = asset.open(None, None).unwrap();
        assert!(is_streamed(&streamed));
        let mut decoded = whole(&asset);
        assert_eq!(decoded.layout().image.height, h);
        let a = request(&mut streamed, Plane::Image, (w, h), [0, 0, w, h]);
        let b = request(&mut decoded, Plane::Image, (w, h), [0, 0, w, h]);
        assert_eq!(a.format, PixelFormat::Alpha);
        assert_eq!(a.data, b.data, "{decode}");
        let missing = &a.data[(w * 3) as usize..];
        assert!(missing.iter().all(|&v| v == fill), "{decode}");
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

/// 8-bit samples the data does not hold are stored zero: the short last row keeps
/// the bytes it holds, and the rows after it are black.
#[test]
fn data_that_ends_early_reads_the_rest_as_zero() {
    let (w, h) = (10_u32, 8_u32);
    let gray = noise((w * h) as usize, 4);
    let cut = &gray[..(w * 3 + 4) as usize];
    let dict = image("/ColorSpace /DeviceGray /BitsPerComponent 8", w, h);
    for (dict, data) in [
        (dict.clone(), cut.to_vec()),
        (format!("{dict} /Filter /FlateDecode"), zlib(cut)),
    ] {
        let pdf = pdf(&[(dict, data)]);
        let asset = asset(&pdf);
        let mut streamed = asset.open(None, None).unwrap();
        let mut decoded = whole(&asset);
        assert_eq!(decoded.layout().image.height, h);
        let a = request(&mut streamed, Plane::Image, (w, h), [0, 0, w, h]);
        let b = request(&mut decoded, Plane::Image, (w, h), [0, 0, w, h]);
        assert_eq!(a.format, PixelFormat::Luma);
        assert_eq!(a.data, b.data);
        let (held, missing) = a.data.split_at(cut.len());
        assert_eq!(held, cut);
        assert!(missing.iter().all(|&v| v == 0));
    }
}

#[test]
fn a_huge_declared_image_with_little_data_costs_the_data() {
    // A request holds its bins, where a whole decode would hold `w * h` bytes.
    let (w, h) = (60_000, 1_000_000);
    let pdf = pdf(&[(
        image("/ColorSpace /DeviceGray /BitsPerComponent 8", w, h),
        vec![7; 100],
    )]);
    let asset = asset(&pdf);
    let mut source = asset.open(None, None).unwrap();
    let region = request(&mut source, Plane::Image, (100, 100), [0, 0, 100, 100]);
    assert_eq!(region.format, PixelFormat::Luma);
    assert!(region.data.iter().all(|&v| v == 0));
    let region = request(&mut source, Plane::Image, (w, h), [0, 0, 4, 2]);
    assert_eq!(region.data, [7, 7, 7, 7, 0, 0, 0, 0]);
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

/// A row of packed samples the data ends within reads as stored zero, also when the
/// data's end is found only after pieces of the row were pushed: the Flate data's
/// length is not known before it is read, and the row is wider than a piece.
#[test]
fn a_packed_row_found_short_after_pushing_reads_as_zero() {
    let (w, h) = (300_000_u32, 3_u32);
    let row = (w as usize * 2).div_ceil(8);
    let held = row + PIECE_BYTES + 100;
    let pdf = pdf(&[(
        image(
            "/ColorSpace /DeviceGray /BitsPerComponent 2 /Decode [1 0] /Filter /FlateDecode",
            w,
            h,
        ),
        zlib(&noise(held, 16)),
    )]);
    let asset = asset(&pdf);
    let mut decoded = whole(&asset);
    assert_eq!(decoded.layout().image.height, h);
    for grid in [(w, h), (w / 2, 2), (70, 2)] {
        for window in [[0, 0, grid.0, grid.1], [0, 1, grid.0, 2]] {
            let mut streamed = asset.open(None, None).unwrap();
            let a = request(&mut streamed, Plane::Image, grid, window);
            let b = request(&mut decoded, Plane::Image, grid, window);
            assert_eq!(
                (a.format, &a.data),
                (b.format, &b.data),
                "{grid:?} {window:?}"
            );
        }
    }
    let mut streamed = asset.open(None, None).unwrap();
    let second = request(&mut streamed, Plane::Image, (w, h), [0, 1, w, 2]);
    assert!(second.data.iter().all(|&v| v == 255));
}

/// A row the data ends within pushes the texels its data holds, not the texels the
/// row's width declares: the rest of a row of one byte, whose stored zeros are not
/// zero once inverted, is pushed at once.
#[test]
fn a_row_cut_short_pushes_the_texels_it_holds() {
    let gray = "/ColorSpace /DeviceGray /BitsPerComponent 8 /Decode [1 0]";
    for w in [20_000_000, 2_000_000] {
        let pdf = pdf(&[(image(gray, w, 2), noise(1, 17))]);
        let asset = asset(&pdf);
        let mut streamed = asset.open(None, None).unwrap();
        crate::x_object::area::PUSHED.set(0);
        let a = streamed.request(&ImageRequest {
            plane: Plane::Image,
            grid: (2, 2),
            window: [1, 0, 2, 1],
            cancel: None,
        });
        let pushed = crate::x_object::area::PUSHED.get();
        assert!(pushed <= PIECE_BYTES as u64, "{w}: {pushed} texels pushed");
        assert_eq!(streamed.work().rows_decoded, 1);
        let a = a.unwrap();
        assert_eq!((a.format, a.data), (PixelFormat::Luma, vec![255]));
    }
}

/// Rows the data ends within, read in pieces and padded past the data at once, equal
/// the rows decoded whole: with padded samples that are not zero once converted, a
/// colour key, and a mask whose data ends before or after the colour's.
#[test]
fn rows_cut_short_stream_as_decoded() {
    let (w, h) = (150_001_u32, 2_u32);
    let colour = "/ColorSpace /DeviceRGB /BitsPerComponent 8 /Decode [1 0 1 0 1 0]";
    // Into the last row, the first or the second.
    let second = noise((w * 3 + w * 3 / 2 + 7) as usize, 18);
    for (rows, rgb) in [(1, &second[..(w * 3 / 2 + 7) as usize]), (h, &second)] {
        assert_streams_as_decoded(&[(image(colour, w, rows), rgb.to_vec())]);
        // Under a colour key, a row the data ends within is transparent: with no
        // whole row, opening or requesting the image fails as decoding it whole does.
        let keyed = pdf(&[(
            image(&format!("{colour} /Mask [0 120 60 255 0 200]"), w, rows),
            rgb.to_vec(),
        )]);
        differential::assert_streams_as_whole(&asset(&keyed), &format!("{rows} rows"));
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
fn data_that_is_not_zlib_reads_as_raw_deflate() {
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

/// Colour and soft mask both raw deflate, read alongside.
#[test]
fn colour_and_mask_both_raw_deflate_stream_as_decoded() {
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
/// skipping to them, past the rest of the row or past rows before them, of the
/// colour, of a stencil mask's bits or of a mask on its own grid, or pushing decoded
/// texels. The source stays usable: the same request then returns what decoding the
/// image whole does.
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
        (
            vec![(
                image("/ImageMask true /Filter /FlateDecode", w, 2),
                zlib(&noise(2 * w as usize / 8, 24)),
            )],
            Plane::Image,
            (w, 2),
            vec![[0, 1, 1, 2], [w - 1, 0, w, 1], [0, 0, w, 1]],
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
        let mut streamed = asset.open(None, None).unwrap();
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

/// The first open of an image with a soft mask or a mask stream reads none of its
/// data or the mask's, raw or Flate, so its work does not grow with the image's
/// height. Its requests still equal those of the image decoded whole.
#[test]
#[cfg(debug_assertions)]
fn a_first_open_reads_no_data() {
    let w = 512_u32;
    for flate in [false, true] {
        for (entry, mask_dict, mask_bytes) in [
            ("/SMask", "/ColorSpace /DeviceGray /BitsPerComponent 8", w),
            ("/Mask", "/ImageMask true", w / 8),
        ] {
            let mut work = Vec::new();
            for h in [256_u32, 1024] {
                let stored = |len: usize, seed: u32| {
                    let data = noise(len, seed);
                    if flate {
                        stored_zlib(&data, 65_535)
                    } else {
                        data
                    }
                };
                let filter = if flate { "/Filter /FlateDecode" } else { "" };
                let pdf = pdf(&[
                    (
                        image(
                            &format!(
                                "/ColorSpace /DeviceGray /BitsPerComponent 1 {entry} 4 0 R {filter}"
                            ),
                            w,
                            h,
                        ),
                        stored((w / 8 * h) as usize, h),
                    ),
                    (
                        image(&format!("{mask_dict} {filter}"), w, h),
                        stored((mask_bytes * h) as usize, h + 1),
                    ),
                ]);
                let asset = asset(&pdf);
                TOLD.set(0);
                rows::READ.set(0);
                let source = asset.open(None, None).unwrap();
                assert!(is_streamed(&source));
                let opened = (TOLD.get(), rows::READ.get(), source.work().bytes_inflated);
                println!(
                    "flate {flate}, {entry}, height {h}: first open (told, read, inflated) {opened:?}"
                );
                assert_eq!(
                    (opened.1, opened.2),
                    (0, 0),
                    "flate {flate}, {entry}, height {h}"
                );
                work.push(opened.0);
                if h == 256 {
                    assert_asset_streams_as_decoded(&asset);
                }
            }
            assert_eq!(work[0], work[1], "flate {flate}, {entry}");
        }
    }
}

/// A first open stopped by its hook returns within three polls' worth of work, wherever
/// the work is: decoding a mask whole, or decoding an image that is not streamed
/// whole (through a filter, or converting 16-bit samples). The asset is left as it was: opening it again gives what decoding the
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
        let opened = asset.open(None, Some(&hook));
        assert!(matches!(opened, Err(RequestError::Cancelled)), "{name}");
        assert_eq!(polls.get(), 2, "{name}");
        let (told, read) = (TOLD.get(), rows::READ.get());
        assert!(
            told < 3 * POLL_BYTES && read < 3 * POLL_BYTES as u64,
            "{name}: {told} bytes of work, {read} bytes read"
        );
        let mut source = asset.open(None, None).unwrap();
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
/// whose data holds one byte is cancelled within three polls' worth of work.
#[test]
#[cfg(debug_assertions)]
fn padding_missing_rows_is_cancelled_within_bounded_work() {
    let (w, h) = (65_537, 40);
    let pdf = pdf(&[(
        image("/ColorSpace /DeviceGray /BitsPerComponent 8", w, h),
        vec![7],
    )]);
    let asset = asset(&pdf);
    let mut source = asset.open(None, None).unwrap();
    let request_of = |cancel| ImageRequest {
        plane: Plane::Image,
        grid: (w, h),
        window: [0, 0, w, h],
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
    assert_eq!(region.data.len(), (w * h) as usize);
    assert_eq!(region.data[..2], [7, 0]);
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
    let mut source = asset.open(None, None).unwrap();
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

/// Rows past the data's end are jumped over, not stepped through: the last row of an
/// image two billion rows high whose data holds its first row, alone or under a
/// soft mask whose data holds as much, read alongside or on its own grid, raw or
/// Flate, counts next to nothing and equals that row of the same image three rows
/// high decoded whole. A request of the whole plane in one bin pushes every row,
/// each counted, and is cancelled within three polls' work.
#[test]
#[cfg(debug_assertions)]
fn rows_past_the_data_are_jumped_over() {
    let tall = 2_000_000_000_u32;
    let gray1 = "/ColorSpace /DeviceGray /BitsPerComponent 1";
    for flate in [false, true] {
        let filter = if flate { "/Filter /FlateDecode" } else { "" };
        let stored = |data: &[u8]| {
            if flate {
                stored_zlib(data, 1000)
            } else {
                data.to_vec()
            }
        };
        // The mask's width, if any (another than the colour's puts it on its own
        // grid), and the plane requested.
        for (mask, plane) in [
            (None, Plane::Image),
            (Some(8_u32), Plane::Image),
            (Some(16), Plane::Image),
            (Some(16), Plane::Mask),
        ] {
            let objects = |h: u32| {
                let entry = if mask.is_some() { "/SMask 4 0 R" } else { "" };
                let mut objects = vec![(
                    image(&format!("{gray1} {entry} {filter}"), 8, h),
                    stored(&[0xA5]),
                )];
                if let Some(mw) = mask {
                    let row = vec![0x5A; mw as usize / 8];
                    objects.push((image(&format!("{gray1} {filter}"), mw, h), stored(&row)));
                }
                objects
            };
            let case = format!("flate {flate}, mask {mask:?}, {plane:?}");
            let small = pdf(&objects(3));
            let small = asset(&small);
            let mut whole = whole(&small);
            let p = *whole.layout().plane(plane).unwrap();
            let expected = request(&mut whole, plane, (p.width, 3), [0, 2, p.width, 3]);
            let huge = pdf(&objects(tall));
            let huge = asset(&huge);
            let mut source = huge.open(None, None).unwrap();
            assert!(is_streamed(&source), "{case}");
            let polls = Cell::new(0);
            let hook = || {
                polls.set(polls.get() + 1);
                false
            };
            TOLD.set(0);
            rows::STEPPED.set(0);
            let last = source
                .request(&ImageRequest {
                    plane,
                    grid: (p.width, tall),
                    window: [0, tall - 1, p.width, tall],
                    cancel: Some(&hook),
                })
                .unwrap();
            assert_eq!(last.data, expected.data, "{case}");
            let (told, stepped) = (TOLD.get(), rows::STEPPED.get());
            assert!(
                told < 1024 && stepped <= 4 && polls.get() <= 1,
                "{case}: {told} bytes of work, {stepped} rows stepped, {} polls",
                polls.get()
            );
            let polls = Cell::new(0);
            let hook = || {
                polls.set(polls.get() + 1);
                polls.get() > 1
            };
            TOLD.set(0);
            let result = source.request(&ImageRequest {
                plane,
                grid: (1, 1),
                window: [0, 0, 1, 1],
                cancel: Some(&hook),
            });
            assert!(matches!(result, Err(RequestError::Cancelled)), "{case}");
            let told = TOLD.get();
            assert!(told < 3 * POLL_BYTES, "{case}: {told} bytes of work");
        }
    }
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
    let mut source = asset.open(None, None).unwrap();
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
    let expected = obj.decoded_mask(None).unwrap().luma().data.clone();
    assert_eq!(&expected[..10], &[0, 255, 0, 255, 0, 255, 0, 255, 0, 0]);
    let asset = obj.asset(None);
    let mut source = asset.open(None, None).unwrap();
    let layout = *source.layout();
    assert_eq!(layout.mask, None);
    assert_eq!(layout.image.format, PixelFormat::Alpha);
    assert_eq!((layout.image.width, layout.image.height), (w, h));
    let full = request(&mut source, Plane::Image, (w, h), [0, 0, w, h]);
    assert_eq!(full.data, expected);
    let halved = request(&mut source, Plane::Image, (5, 2), [0, 0, 5, 2]);
    assert_eq!(halved.data.len(), 10);
}
