//! `Stream::decoded_reader` reads what `Stream::decoded` returns, in memory that grows
//! with the data read, not with what the dictionary declares or the stream holds.

use hayro_syntax::Pdf;
use hayro_syntax::object::stream::ReadError;
use hayro_syntax::object::{ObjectIdentifier, Stream};
use hayro_syntax::work::Meter;

mod common;
use common::allocations::measure;

/// A document whose object 3 is a stream with `dict` entries holding `data`.
fn stream_file(dict: &str, data: &[u8]) -> Pdf {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
        common::stream(dict, data),
    ];
    Pdf::new(common::file(&objects, "")).unwrap()
}

/// `data` as object 3 of an AES-256 encrypted document.
fn encrypted_file(dict: &str, data: &[u8]) -> Pdf {
    Pdf::new(common::encrypted_stream_file(dict, data, [7; 32], [7; 32])).unwrap()
}

fn object_3(pdf: &Pdf) -> Stream<'_> {
    pdf.xref().get(ObjectIdentifier::new(3, 0)).unwrap()
}

/// Everything the reader yields, read `chunk` bytes at a time, read twice with a
/// rewind between; and how often it restarted.
fn read_twice(stream: &Stream<'_>, chunk: usize) -> (Vec<u8>, usize) {
    let mut reader = stream.decoded_reader().unwrap();
    let mut buf = vec![0; chunk];
    let mut restarts = 0;
    let mut passes = Vec::new();
    for _ in 0..2 {
        let mut out = Vec::new();
        loop {
            match reader.read(&mut buf, &Meter::default()) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(ReadError::Restarted) => {
                    restarts += 1;
                    out.clear();
                }
                Err(e) => panic!("{e:?}"),
            }
        }
        passes.push(out);
        reader.rewind();
    }
    assert_eq!(passes[0], passes[1], "a rewound reader reads the same");
    (passes.swap_remove(0), restarts)
}

#[cfg(feature = "unsafe")]
fn zlib(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn a_declared_predictor_row_is_not_allocated_before_data_arrives() {
    for columns in [1 << 20, usize::MAX / 32] {
        for row in [&[][..], &[0, 1, 2, 3][..]] {
            let pdf = stream_file(
                &format!(
                    "/Filter /FlateDecode /DecodeParms << /Predictor 12 /Colors 1 \
                     /BitsPerComponent 8 /Columns {columns} >>"
                ),
                &common::stored_zlib(row, usize::from(u16::MAX)),
            );
            let stream = object_3(&pdf);
            let (can, probe) = measure(|| stream.can_read_incrementally());
            assert!(can);
            assert!(probe.largest < 4096, "the probe allocated {probe:?}");
            let ((), read) = measure(|| {
                let mut reader = stream.decoded_reader().unwrap();
                assert_eq!(reader.read(&mut [0; 4], &Meter::default()), Ok(0));
                reader.rewind();
                assert_eq!(reader.read(&mut [0; 4], &Meter::default()), Ok(0));
            });
            assert!(read.largest < 128 * 1024, "reading allocated {read:?}");
        }
    }
}

#[test]
fn encrypted_stored_data_is_decrypted_as_it_is_read() {
    let data: Vec<u8> = (0..262_144).map(|i| (i % 251) as u8).collect();
    let pdf = encrypted_file("", &data);
    let stream = object_3(&pdf);
    let (can, probe) = measure(|| stream.can_read_incrementally());
    assert!(can);
    assert_eq!(probe.largest, 0);
    assert_eq!(read_twice(&stream, 37), (data.clone(), 0));
    let ((), prefix) = measure(|| {
        let mut reader = stream.decoded_reader().unwrap();
        let mut buf = [0; 37];
        assert_eq!(reader.read(&mut buf, &Meter::default()), Ok(37));
        assert_eq!(buf, data[..37]);
    });
    assert!(prefix.peak < 32 * 1024, "a prefix allocated {prefix:?}");
}

/// Encrypted zlib, and encrypted raw deflate, which restarts once: a rewind does not
/// repeat the restart.
#[cfg(feature = "unsafe")]
#[test]
fn encrypted_flate_restarts_and_rewinds_from_the_stored_bytes() {
    let data: Vec<u8> = (0..32_769_u32).map(|i| ((i / 7) % 251) as u8).collect();
    let zlib = zlib(&data);
    for (encoded, restarts) in [(&zlib[..], 0), (&zlib[2..zlib.len() - 4], 1)] {
        let pdf = encrypted_file("/Filter /FlateDecode", encoded);
        assert_eq!(read_twice(&object_3(&pdf), 397), (data.clone(), restarts));
    }
}

/// Corrupt encrypted Flate ends with what whole decoding returns, after restarting as
/// raw deflate and then with the permissive decoder.
#[cfg(feature = "unsafe")]
#[test]
fn encrypted_corrupt_flate_ends_as_whole_decoding_does() {
    // One valid, non-final stored block, followed by a reserved block type.
    let mut encoded = vec![0x78, 1, 0, 0x88, 0x13, 0x77, 0xec];
    encoded.extend((0..5000).map(|i| (i % 251) as u8));
    encoded.push(7);
    let pdf = encrypted_file("/Filter /FlateDecode", &encoded);
    let stream = object_3(&pdf);
    let expected = stream.decoded().unwrap();
    assert_eq!(read_twice(&stream, 397), (expected.to_vec(), 2));
}
