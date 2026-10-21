//! Flate data is inflated into room that fits it: an image's samples into room
//! reserved no further than the size its dictionary tells while they are shorter,
//! data of no known size into room at most twice what it makes, and any stream's
//! data returned holding no room past its bytes, though how fast the data inflates
//! changes along it. The bytes held at most stay under ceilings.

use hayro_syntax::Pdf;
use hayro_syntax::object::stream::ImageDecodeParams;
use hayro_syntax::object::{ObjectIdentifier, Stream};
use std::io::Write;

mod common;
use common::allocations::measure;
use common::ceilings::check_ceilings;

/// Rows of 1,024 bytes: the first half white, the rest noise, so that the first
/// half inflates a hundred times faster than the rest and the rate so far predicts
/// far more data than there is.
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

/// Data of no known size holds no more than upstream (036fb21c) holds for it:
/// `read_to_end` doubles its room, which holds more than the data and up to twice
/// it, here up to exactly twice, besides 80,320 bytes of decoder state and input
/// buffer (measured), however the rate of the data falls along it.
#[test]
fn data_of_no_known_size_holds_at_most_twice_it() {
    let len = WIDTH * HEIGHT;
    for len in [len, len + 1, len * 3 / 4, 70_000, 100] {
        let samples = samples(len);
        let pdf = file_with_stream("/Filter /FlateDecode", &zlib(&samples));
        let stream = object_3(&pdf);
        let (data, used) = measure(|| stream.decoded().unwrap().into_owned());
        assert_eq!(data, samples);
        assert!(
            used.peak <= 2 * len + 80_320,
            "peak {} bytes for {len} inflated",
            used.peak
        );
    }
}
