//! Flate data is inflated into room no larger than upstream (036fb21c) inflates it
//! into, however the data inflate along it and whatever size it is told; an image's
//! samples into room reserved no further than the size its dictionary tells while
//! they are shorter; and any stream's data returned holding no room past its bytes.
//! The bytes held at most stay under ceilings.

use hayro_syntax::Pdf;
use hayro_syntax::object::stream::ImageDecodeParams;
use hayro_syntax::object::{ObjectIdentifier, Stream};
use std::io::Write;

mod common;
use common::allocations::measure;
use common::ceilings::check_ceilings;

/// Rows of 1,024 bytes: the first half white, the rest noise, so that the first
/// half inflates a hundred times faster than the rest.
const WIDTH: usize = 1024;
const HEIGHT: usize = 8192;

/// `len` bytes, the first half white, the rest noise.
fn samples(len: usize) -> Vec<u8> {
    let mut data = vec![0xFF; len / 2];
    let mut x = 0x2545_F491_u32;
    data.extend((len / 2..len).map(|_| {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x as u8
    }));
    data
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn object_3(pdf: &Pdf) -> Stream<'_> {
    pdf.xref().get(ObjectIdentifier::new(3, 0)).unwrap()
}

/// A file whose object 3 is a stream of `dict` and `data`.
fn file_with_stream(dict: &str, data: &[u8]) -> Pdf {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
        common::stream(dict, data),
    ];
    Pdf::new(common::file(&objects, "")).unwrap()
}

#[test]
fn inflated_data_fits_its_room() {
    let samples = samples(WIDTH * HEIGHT);
    let dict = format!(
        "/Type /XObject /Subtype /Image /Width {WIDTH} /Height {HEIGHT} \
         /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode"
    );
    let pdf = file_with_stream(&dict, &zlib(&samples));
    let stream = object_3(&pdf);
    let params = ImageDecodeParams {
        bpc: Some(8),
        num_components: Some(1),
        width: WIDTH as u32,
        height: HEIGHT as u32,
        ..Default::default()
    };

    // Owned data is taken as it is, its room included.
    let (image, image_used) = measure(|| stream.decoded_image(&params).unwrap().data.into_owned());
    assert_eq!(image, samples);
    assert_eq!(image.capacity(), samples.len());
    drop(image);

    // Without the image's size, as a content stream is decoded.
    let (data, data_used) = measure(|| stream.decoded().unwrap().into_owned());
    assert_eq!(data, samples);
    assert_eq!(data.capacity(), samples.len());

    // Each decoder has its own: zlib-rs with `unsafe`, the permissive one without.
    #[cfg(feature = "unsafe")]
    let (path, write) = (
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/ceilings/flate_room_unsafe.txt"
        ),
        "WRITE_CEILINGS=1 cargo test -p hayro-syntax --features unsafe --test flate_room",
    );
    #[cfg(not(feature = "unsafe"))]
    let (path, write) = (
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ceilings/flate_room.txt"),
        "WRITE_CEILINGS=1 cargo test -p hayro-syntax --test flate_room",
    );
    check_ceilings(
        path,
        write,
        &[
            ("image_peak_bytes", image_used.peak),
            ("stream_peak_bytes", data_used.peak),
        ],
    );
}

/// How a stream is told the size of its data: not at all, as a content stream, or
/// as an image of one row `width` bytes wide, whose data the decoder expects to
/// take `width + 1` bytes (a predictor's byte for the row).
#[derive(Clone, Copy, Debug)]
enum Hint {
    None,
    Width(usize),
}

impl Hint {
    /// No size, then the size of `len` bytes told right, twice over and half.
    fn all(len: usize) -> [Self; 4] {
        [
            Self::None,
            Self::Width(len.max(1)),
            Self::Width(2 * len + 1),
            Self::Width(len / 2 + 1),
        ]
    }
}

/// Where ours, under each hint, returns other bytes than upstream's decoding of
/// `encoded`, or holds at some time more than upstream's held at most.
fn more_than_upstream(name: &str, encoded: &[u8]) -> Vec<String> {
    let (reference, upstream) = measure(|| common::upstream_flate::decode(encoded));
    let Some(reference) = reference else {
        return vec![format!("{name}: upstream decodes nothing")];
    };
    let pdf = file_with_stream("/Filter /FlateDecode", encoded);
    let stream = object_3(&pdf);
    let mut wrong = vec![];
    for hint in Hint::all(reference.len()) {
        let (data, ours) = measure(|| match hint {
            Hint::None => stream.decoded().unwrap().into_owned(),
            Hint::Width(width) => {
                let params = ImageDecodeParams {
                    bpc: Some(8),
                    num_components: Some(1),
                    width: width as u32,
                    height: 1,
                    ..Default::default()
                };
                stream.decoded_image(&params).unwrap().data.into_owned()
            }
        });
        if data != reference {
            wrong.push(format!("{name}, {hint:?}: other bytes than upstream's"));
        }
        if ours.peak > upstream.peak {
            wrong.push(format!(
                "{name}, {hint:?}: peak {} bytes where upstream's is {} for {} bytes",
                ours.peak,
                upstream.peak,
                reference.len()
            ));
        }
    }
    wrong
}

/// `len` bytes of `shape`: pieces of noise, of one byte repeated and of text-like
/// bytes, each up to 64 KiB, mixed by `seed`; noise then zeros; or zeros then noise.
fn shaped(shape: &str, len: usize, seed: u32) -> Vec<u8> {
    match shape {
        "mixed" => {
            let mut x = seed.wrapping_mul(2_654_435_761) | 1;
            let mut next = move || {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as usize
            };
            let mut data = Vec::with_capacity(len);
            while data.len() < len {
                let n = (1 + next() % 65_536).min(len - data.len());
                match next() % 3 {
                    0 => data.extend(common::noise(n, next() as u32)),
                    1 => data.extend(std::iter::repeat_n(next() as u8, n)),
                    _ => data.extend(b"0 0 1 rg 10 10 50 50 re f\n".iter().cycle().take(n)),
                }
            }
            data
        }
        "noise then zeros" => {
            let mut data = common::noise(len / 2, seed);
            data.resize(len, 0);
            data
        }
        "zeros then noise" => {
            let mut data = vec![0; len - len / 2];
            data.extend(common::noise(len / 2, seed));
            data
        }
        _ => unreachable!(),
    }
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// Ours holds no more than upstream (036fb21c), whose `read_to_end` doubles its room
/// from 32 bytes, holding up to twice the data besides its decoder's state and
/// input buffer, and whose permissive decoder's room doubles as it pushes bytes,
/// whatever size a stream is told and however its data inflates along it.
#[test]
fn inflating_holds_no_more_than_upstream() {
    let mut wrong = vec![];
    let mut check = |name: &str, encoded: &[u8]| wrong.extend(more_than_upstream(name, encoded));
    // The review's case: the rate of the first half predicts less than the rest.
    check(
        "8,388,607 bytes, noise then zeros",
        &zlib(&shaped("noise then zeros", 8_388_607, 1)),
    );
    for shape in ["mixed", "noise then zeros", "zeros then noise"] {
        for k in [5, 10, 15, 16, 17, 20] {
            for len in [(1 << k) - 1, 1 << k, (1 << k) + 1] {
                let data = shaped(shape, len, k);
                check(&format!("{len} bytes, {shape}"), &zlib(&data));
            }
        }
    }
    for len in [0, 1, 31, 32, 33, 100, 1000] {
        let data = shaped("mixed", len, 7);
        check(&format!("{len} bytes"), &zlib(&data));
        check(&format!("{len} bytes, raw deflate"), &deflate(&data));
    }
    let data = shaped("mixed", 300_000, 3);
    check("raw deflate", &deflate(&data));
    check("stored blocks", &common::stored_zlib(&data, 65_535));
    // The first room is sized before the decoder knows how little the data makes.
    for len in [0, 100, 300_000] {
        let mut tailed = zlib(&data[..len]);
        tailed.extend(common::noise(200_000, 5));
        check(
            &format!("{len} bytes, 200,000 bytes of junk after"),
            &tailed,
        );
    }
    let encoded = zlib(&data);
    check("cut short", &encoded[..encoded.len() / 2]);

    // Data the zlib decoders refuse, which the permissive one reads.
    let mut bad_checksum = encoded.clone();
    *bad_checksum.last_mut().unwrap() ^= 1;
    check("a wrong checksum", &bad_checksum);
    for len in [(1 << 16) - 1, 1 << 16, (1 << 16) + 1, 1 << 20] {
        let data = shaped("mixed", len, 9);
        let mut encoded = common::stored_zlib(&data, 65_535);
        *encoded.last_mut().unwrap() ^= 1;
        check(&format!("{len} stored bytes, a wrong checksum"), &encoded);
        let mut encoded = zlib(&data);
        *encoded.last_mut().unwrap() ^= 1;
        check(&format!("{len} bytes, a wrong checksum"), &encoded);
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
