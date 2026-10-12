//! `Stream::decoded_reader` reads what `Stream::decoded` returns, in memory that grows
//! with the data read, not with what the dictionary declares or the stream holds.

use hayro_syntax::Pdf;
use hayro_syntax::object::stream::{DecodedReader, ReadError};
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

/// The next `len` bytes the reader yields (fewer at the end), read `chunk` bytes at a
/// time; a restart voids what was read before it. Also how often it restarted.
fn read_up_to(reader: &mut DecodedReader<'_>, len: usize, chunk: usize) -> (Vec<u8>, usize) {
    let mut buf = vec![0; chunk];
    let mut restarts = 0;
    let mut out = Vec::new();
    while out.len() < len {
        let wanted = chunk.min(len - out.len());
        match reader.read(&mut buf[..wanted], &Meter::default()) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(ReadError::Restarted) => {
                restarts += 1;
                out.clear();
            }
            Err(e) => panic!("{e:?}"),
        }
    }
    (out, restarts)
}

/// Everything the reader yields, read `chunk` bytes at a time, read twice with a
/// rewind between; and how often it restarted.
fn read_twice(stream: &Stream<'_>, chunk: usize) -> (Vec<u8>, usize) {
    let mut reader = stream.decoded_reader().unwrap();
    let first = read_up_to(&mut reader, usize::MAX, chunk);
    reader.rewind();
    let second = read_up_to(&mut reader, usize::MAX, chunk);
    assert_eq!(
        second,
        (first.0.clone(), 0),
        "a rewound reader reads the same"
    );
    first
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

/// A checkpoint resumes Flate data after the reader, stream and document that took it
/// are gone: mid-row for predictors, mid-block for AES, with back references reaching
/// before it, and after raw deflate restarted. It holds the decoder's state, not the
/// stored bytes.
#[cfg(feature = "unsafe")]
#[test]
fn a_checkpoint_resumes_after_its_document_is_dropped() {
    // Incompressible, then a block repeated at a distance the window still reaches.
    let mut data = common::noise(150_000, 1);
    data.extend(common::noise(30_000, 2).repeat(4));
    let mut png_rows = data.clone();
    for (i, row) in png_rows.chunks_mut(264).enumerate() {
        row[0] = (i % 5) as u8;
    }
    let cases = [
        ("", &data),
        ("/DecodeParms << /Predictor 2 /Columns 263 >>", &data),
        ("/DecodeParms << /Predictor 15 /Columns 263 >>", &png_rows),
    ];
    for (params, plain) in cases {
        let zlib = zlib(plain);
        for encoded in [&zlib[..], &zlib[2..zlib.len() - 4]] {
            for encrypted in [false, true] {
                for offset in [1, 17, 200_003] {
                    resume_at(encrypted, params, encoded, offset);
                }
            }
        }
    }
}

#[cfg(feature = "unsafe")]
fn resume_at(encrypted: bool, params: &str, encoded: &[u8], offset: usize) {
    let dict = format!("/Filter /FlateDecode {params}");
    let (stream, checkpoint, expected) = {
        let pdf = match encrypted {
            true => encrypted_file(&dict, encoded),
            false => stream_file(&dict, encoded),
        };
        let stream = object_3(&pdf);
        let expected = stream.decoded().unwrap().into_owned();
        let owned = stream.to_owned_stream();
        let mut reader = stream.decoded_reader().unwrap();
        assert_eq!(read_up_to(&mut reader, offset, 4093).0, expected[..offset]);
        let (checkpoint, taken) = measure(|| reader.checkpoint(&owned).unwrap());
        assert!(checkpoint.heap_bytes() <= reader.checkpoint_heap_bound().unwrap());
        assert!(taken.largest < encoded.len() / 2, "copied {taken:?}");
        (owned, checkpoint, expected)
    };
    for chunk in [7, 4093] {
        let stream = stream.get().unwrap();
        let mut reader = stream.decoded_reader().unwrap();
        assert!(reader.restore(&checkpoint));
        let rest = read_up_to(&mut reader, usize::MAX, chunk);
        assert!(
            rest == (expected[offset..].to_vec(), 0),
            "{params} at {offset}"
        );
    }
}

/// Only an incrementally inflating reader is checkpointed, only for the stream whose
/// bytes it reads, and a checkpoint is not restored into a reader of equal bytes.
#[cfg(feature = "unsafe")]
#[test]
fn a_checkpoint_belongs_to_the_bytes_it_was_taken_from() {
    let data = common::noise(10_000, 3);
    let stored = stream_file("", &data);
    let reader = object_3(&stored).decoded_reader().unwrap();
    assert!(reader.checkpoint_heap_bound().is_none());
    assert!(
        reader
            .checkpoint(&object_3(&stored).to_owned_stream())
            .is_none()
    );

    let encoded = zlib(&data);
    let (a, b) = (
        stream_file("/Filter /FlateDecode", &encoded),
        stream_file("/Filter /FlateDecode", &encoded),
    );
    let (a, b) = (object_3(&a), object_3(&b));
    let mut reader = a.decoded_reader().unwrap();
    read_up_to(&mut reader, 100, 100);
    assert!(reader.checkpoint(&b.to_owned_stream()).is_none());
    let checkpoint = reader.checkpoint(&a.to_owned_stream()).unwrap();
    let mut other = b.decoded_reader().unwrap();
    assert!(!other.restore(&checkpoint));
    assert_eq!(read_up_to(&mut other, usize::MAX, 997), (data, 0));
}
