//! Flate data is inflated into room that fits it: an image's samples into room
//! reserved no further than the size its dictionary tells while they are shorter,
//! and any stream's data returned holding no room past its bytes, though how fast
//! the data inflates changes along it. The bytes held at most stay under ceilings.

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

fn samples() -> Vec<u8> {
    let mut data = vec![0xFF; WIDTH * HEIGHT / 2];
    let mut x = 0x2545_F491_u32;
    data.extend((0..WIDTH * HEIGHT / 2).map(|_| {
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

#[test]
fn inflated_data_fits_its_room() {
    let samples = samples();
    let dict = format!(
        "/Type /XObject /Subtype /Image /Width {WIDTH} /Height {HEIGHT} \
         /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode"
    );
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
        common::stream(&dict, &zlib(&samples)),
    ];
    let pdf = Pdf::new(common::file(&objects, "")).unwrap();
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
