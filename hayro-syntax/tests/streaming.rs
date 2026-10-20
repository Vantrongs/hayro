//! `Stream::decoded_reader` reads what `Stream::decoded` returns, in memory that grows
//! with the data read, not with what the dictionary declares or the stream holds.

use hayro_syntax::Pdf;
use hayro_syntax::object::stream::{DecodeFailure, ImageDecodeParams};
use hayro_syntax::object::stream::{DecodedReader, ReadError};
use hayro_syntax::object::{ObjectIdentifier, Stream};
use hayro_syntax::work::{Cancelled, Meter, POLL_BYTES};
use std::cell::Cell;

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

fn zlib(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// A row declared far longer than the data allocates nothing for its length: the
/// data are a first row cut short, which is kept (its filter byte, then `1 2 3`).
#[test]
fn a_declared_predictor_row_is_not_allocated_before_data_arrives() {
    // 268,435,454 columns of one byte are the most `MuPDF` takes.
    for columns in [1 << 20, 268_435_454] {
        for (row, expected) in [(&[][..], &[][..]), (&[0, 1, 2, 3][..], &[1, 2, 3][..])] {
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
                let (data, _) = read_up_to(&mut reader, 8, 4);
                assert_eq!(data, expected);
                reader.rewind();
                let (data, _) = read_up_to(&mut reader, 8, 4);
                assert_eq!(data, expected);
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

/// Once its meter stops, a reader of Flate data reads nothing more, not even the bytes
/// it inflated ahead or a checkpoint carried, whatever the size of the reads; and when
/// the hook asks to stop between polls, at most what was inflated ahead is read.
#[test]
fn a_stopped_meter_stops_every_later_read() {
    let data: Vec<u8> = (0..1_000_000_u32).map(|i| ((i / 7) % 251) as u8).collect();
    let pdf = stream_file("/Filter /FlateDecode", &zlib(&data));
    let stream = object_3(&pdf);
    let stop = Cell::new(false);
    let hook = || stop.get();
    let mut buf = [0; 257];

    let meter = Meter::new(Some(&hook));
    let mut reader = stream.decoded_reader().unwrap();
    assert_eq!(reader.read(&mut buf[..1], &meter), Ok(1));
    stop.set(true);
    let mut read = 0;
    let result = loop {
        match reader.read(&mut buf[..1 + read % 2 * 256], &meter) {
            Ok(n) if n > 0 => read += n,
            other => break other,
        }
    };
    assert_eq!(result, Err(ReadError::Cancelled));
    assert!(read < 2 * POLL_BYTES, "{read} bytes read after the stop");

    let meter = Meter::new(Some(&hook));
    assert_eq!(meter.poll(), Err(Cancelled));
    let mut reader = stream.decoded_reader().unwrap();
    assert_eq!(reader.read(&mut buf[..1], &Meter::default()), Ok(1));
    #[cfg(feature = "unsafe")]
    let readers = {
        let checkpoint = reader.checkpoint(&stream.to_owned_stream()).unwrap();
        let mut resumed = stream.decoded_reader().unwrap();
        assert!(resumed.restore(&checkpoint));
        [reader, resumed]
    };
    #[cfg(not(feature = "unsafe"))]
    let readers = [reader];
    for mut reader in readers {
        for len in [1, 257, 1, 257] {
            assert_eq!(
                reader.read(&mut buf[..len], &meter),
                Err(ReadError::Cancelled)
            );
        }
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

/// A CCITT stream that ends at once holds about as much whatever its dictionary
/// declares: its output grows with the rows decoded, not with `Rows` or `Columns`.
#[test]
fn a_declared_ccitt_size_is_not_allocated_before_rows_are_decoded() {
    // Two EOL codes: the end of the data in Group 4.
    let eofb = [0x00, 0x10, 0x01];
    for is_indexed in [false, true] {
        let peak = |columns: u32, rows: u32| {
            let pdf = stream_file(
                &format!(
                    "/Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns {columns} /Rows {rows} >>"
                ),
                &eofb,
            );
            let stream = object_3(&pdf);
            let params = ImageDecodeParams {
                is_indexed,
                ..Default::default()
            };
            let (_, decoded) = measure(|| stream.decoded_image(&params));
            decoded.peak
        };
        let small = peak(8, 1);
        for (columns, rows) in [(8, 65_535), (1 << 20, 65_535), (u32::MAX, 65_535)] {
            let large = peak(columns, rows);
            assert!(
                large < small + 4096,
                "{columns} x {rows}: {large} vs {small}"
            );
        }
    }
}

/// A CCITT stream that decodes a few rows of many declared holds at most eight
/// times what it decoded: room is made ahead of the rows, but not for all of them.
#[test]
fn a_ccitt_stream_cut_short_holds_a_multiple_of_its_rows() {
    // Eight white rows, a bit each against the white row above, then the end of data.
    let data = [0xFF, 0x00, 0x10, 0x01];
    let columns = 1 << 16;
    for is_indexed in [false, true] {
        let row = if is_indexed { columns / 8 } else { columns };
        let pdf = stream_file(
            &format!(
                "/Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns {columns} /Rows 65535 >>"
            ),
            &data,
        );
        let stream = object_3(&pdf);
        let params = ImageDecodeParams {
            is_indexed,
            ..Default::default()
        };
        let (decoded, held) = measure(|| stream.decoded_image(&params).unwrap().data.len());
        assert_eq!(decoded, 8 * row);
        assert!(held.peak <= 8 * 9 * row, "{held:?}");
    }
}

/// A CCITT stream that decodes a few rows of many declared whitens and counts at
/// most [`POLL_BYTES`] past its rows: room is made ahead by steps of up to eight
/// times what was decoded, but not written until rows come.
#[cfg(debug_assertions)]
#[test]
fn a_ccitt_stream_cut_short_whitens_little_past_its_rows() {
    use hayro_syntax::work::TOLD;
    // 200 white rows, a bit each against the white row above, then the end of data.
    let mut data = vec![0xFF; 25];
    data.extend([0x00, 0x10, 0x01]);
    let columns = 1024;
    for is_indexed in [false, true] {
        let row = if is_indexed { columns / 8 } else { columns };
        let pdf = stream_file(
            &format!(
                "/Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns {columns} /Rows 65535 >>"
            ),
            &data,
        );
        let stream = object_3(&pdf);
        let params = ImageDecodeParams {
            is_indexed,
            ..Default::default()
        };
        TOLD.set(0);
        let decoded = stream.decoded_image(&params).unwrap().data.len();
        let told = TOLD.get();
        assert_eq!(decoded, 200 * row);
        // The white written ahead, each row's bytes once more as it is written, and
        // a step per bit read.
        let bound = decoded + POLL_BYTES + decoded + 8 * data.len();
        assert!(told <= bound, "indexed {is_indexed}: {told} > {bound}");
    }
}

/// A CCITT row declared as wide as `Columns` allows is written a counted piece at a
/// time, with room made for what was written: a stop asked for at the first poll
/// ends it holding a few pieces, not the row.
#[test]
fn a_wide_ccitt_row_is_stopped_before_room_is_made_for_it() {
    // One white row, a bit against the white row above, then the end of data.
    let data = [0x80, 0x00, 0x10, 0x01];
    let stop = || true;
    for is_indexed in [false, true] {
        let pdf = stream_file(
            "/Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns 4294967294 /Rows 65535 >>",
            &data,
        );
        let stream = object_3(&pdf);
        let params = ImageDecodeParams {
            is_indexed,
            ..Default::default()
        };
        let meter = Meter::new(Some(&stop));
        let (result, held) = measure(|| stream.decoded_image_metered(&params, &meter).err());
        assert!(
            matches!(result, Some(DecodeFailure::Cancelled)),
            "{result:?}"
        );
        assert!(held.peak <= 8 * POLL_BYTES + 4096, "{held:?}");
    }
}

/// A small CCITT image is decoded into an output allocated once, at its declared
/// size, with lists of color changes that do not grow line by line, as images drawn
/// by the thousand are: the allocations of a decode are counted exactly.
#[test]
fn a_small_ccitt_image_allocates_its_output_once() {
    // Sixteen rows of 80 pixels, each 20 white and black runs of 2: the first in
    // horizontal mode (`001`, `0111`, `11`), the others as vertical codes that follow
    // the row above; then an EOFB.
    let bits = format!(
        "{}{}{}",
        "001011111".repeat(20),
        "1".repeat(40 * 15),
        "000000000001".repeat(2)
    );
    let data: Vec<u8> = bits
        .as_bytes()
        .chunks(8)
        .map(|byte| (0..8).fold(0, |acc, i| acc << 1 | u8::from(byte.get(i) == Some(&b'1'))))
        .collect();
    let pdf = stream_file(
        "/Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns 80 /Rows 16 >>",
        &data,
    );
    let stream = object_3(&pdf);
    let params = ImageDecodeParams::default();
    let (decoded, used) = measure(|| stream.decoded_image(&params).unwrap().data.into_owned());
    assert_eq!(decoded, [0xFF, 0xFF, 0x00, 0x00].repeat(20 * 16));
    // The output, the two lists of changes, and one allocation outside the CCITT
    // decoder and its output; 13 when the output grew from an eighth of its size and
    // the lists change by change.
    assert_eq!(used.count, 4, "{used:?}");
}
