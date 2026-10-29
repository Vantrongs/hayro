//! `Stream::decoded_reader` reads what `Stream::decoded` returns, in memory that grows
//! with the data read, not with what the dictionary declares or the stream holds.

use hayro_syntax::Pdf;
use hayro_syntax::object::stream::DecodedReader;
use hayro_syntax::object::stream::{DecodeFailure, ImageDecodeParams};
use hayro_syntax::object::{ObjectIdentifier, Stream};
use hayro_syntax::work::{Cancelled, Meter, POLL_BYTES};
use std::cell::Cell;

mod common;
use common::allocations::measure;
use common::chains;

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
/// time.
fn read_up_to(reader: &mut DecodedReader<'_>, len: usize, chunk: usize) -> Vec<u8> {
    let mut buf = vec![0; chunk];
    let mut out = Vec::new();
    while out.len() < len {
        let wanted = chunk.min(len - out.len());
        match reader.read(&mut buf[..wanted], &Meter::default()) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => panic!("{e:?}"),
        }
    }
    out
}

/// Everything the reader yields, read `chunk` bytes at a time, read twice with a
/// rewind between.
fn read_twice(stream: &Stream<'_>, chunk: usize) -> Vec<u8> {
    let mut reader = stream.decoded_reader().unwrap();
    let first = read_up_to(&mut reader, usize::MAX, chunk);
    reader.rewind();
    let second = read_up_to(&mut reader, usize::MAX, chunk);
    assert_eq!(second, first, "a rewound reader reads the same");
    first
}

fn zlib(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// TIFF prediction is reversed in pieces as the data arrive, by both readers: a row
/// of any width costs no memory that grows with it, at every depth the readers take,
/// and the bytes are those whole decoding makes, a last row cut short included.
#[test]
fn a_wide_tiff_predictor_row_is_reversed_in_bounded_pieces() {
    const ROW: usize = 1 << 20;
    for bpc in [1, 2, 4, 8] {
        for colors in [1, 3] {
            let columns = ROW * 8 / (bpc * colors) + 5;
            let row = (columns * bpc * colors).div_ceil(8);
            let data = common::noise(2 * row + row / 3, (bpc * colors) as u32);
            let pdf = stream_file(
                &format!(
                    "/Filter /FlateDecode /DecodeParms << /Predictor 2 /Colors {colors} \
                     /BitsPerComponent {bpc} /Columns {columns} >>"
                ),
                &common::stored_zlib(&data, usize::from(u16::MAX)),
            );
            let stream = object_3(&pdf);
            let whole = stream.decoded().unwrap();
            assert_eq!(whole.len(), data.len());
            // Each piece is compared as it is read, so that only the readers allocate.
            let matches = |at: &mut usize, piece: &[u8]| {
                let same = whole.get(*at..*at + piece.len()) == Some(piece);
                *at += piece.len();
                same
            };
            let ((), lenient) = measure(|| {
                let mut reader = stream.decoded_reader().unwrap();
                let (mut buf, mut at) = (vec![0; 1 << 16], 0);
                loop {
                    match reader.read(&mut buf, &Meter::default()).unwrap() {
                        0 => break,
                        n => assert!(matches(&mut at, &buf[..n]), "{bpc} {colors}: at {at}"),
                    }
                }
                assert_eq!(at, whole.len(), "{bpc} {colors}");
            });
            // Without `unsafe`, the lenient reader inflates the data whole.
            if cfg!(feature = "unsafe") {
                assert!(
                    lenient.peak < 256 * 1024,
                    "{bpc} {colors}: lenient {lenient:?}"
                );
            }
            let ((), strict) = measure(|| {
                let meter = Meter::default();
                let mut reader = stream.decoded_strict_reader(&meter).unwrap().unwrap();
                let (mut buf, mut at) = (vec![0; 1 << 16], 0);
                loop {
                    match reader.read(&mut buf, &meter).unwrap() {
                        0 => break,
                        n => assert!(matches(&mut at, &buf[..n]), "{bpc} {colors}: at {at}"),
                    }
                }
                assert_eq!(at, whole.len(), "{bpc} {colors}");
            });
            assert!(
                strict.peak < 256 * 1024,
                "{bpc} {colors}: strict {strict:?}"
            );
        }
    }
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
                let data = read_up_to(&mut reader, 8, 4);
                assert_eq!(data, expected);
                reader.rewind();
                let data = read_up_to(&mut reader, 8, 4);
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
    assert_eq!(read_twice(&stream, 37), data);
    let ((), prefix) = measure(|| {
        let mut reader = stream.decoded_reader().unwrap();
        let mut buf = [0; 37];
        assert_eq!(reader.read(&mut buf, &Meter::default()), Ok(37));
        assert_eq!(buf, data[..37]);
    });
    assert!(prefix.peak < 32 * 1024, "a prefix allocated {prefix:?}");
}

/// Encrypted zlib, and encrypted raw deflate, which is read as such once zlib finds
/// its first bytes corrupt: a rewind reads both again from the stored bytes.
#[cfg(feature = "unsafe")]
#[test]
fn encrypted_flate_rewinds_from_the_stored_bytes() {
    let data: Vec<u8> = (0..32_769_u32).map(|i| ((i / 7) % 251) as u8).collect();
    let zlib = zlib(&data);
    for encoded in [&zlib[..], &zlib[2..zlib.len() - 4]] {
        let pdf = encrypted_file("/Filter /FlateDecode", encoded);
        assert_eq!(read_twice(&object_3(&pdf), 397), data);
    }
}

/// Corrupt encrypted Flate ends where zlib found it corrupt, after the bytes it made
/// before, as whole decoding does.
#[cfg(feature = "unsafe")]
#[test]
fn encrypted_corrupt_flate_ends_as_whole_decoding_does() {
    // One valid, non-final stored block, followed by a reserved block type.
    let mut encoded = vec![0x78, 1, 0, 0x88, 0x13, 0x77, 0xec];
    let block: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
    encoded.extend(&block);
    encoded.push(7);
    let pdf = encrypted_file("/Filter /FlateDecode", &encoded);
    let stream = object_3(&pdf);
    let expected = stream.decoded().unwrap();
    assert_eq!(expected, block);
    assert_eq!(read_twice(&stream, 397), expected.to_vec());
}

/// Flate data of which neither zlib nor raw deflate reads a byte (stored blocks whose
/// lengths do not check) is read by the permissive decoder as it is read, plain or
/// encrypted: its first bytes hold about as much whatever the data's size, and all
/// of it reads as whole decoding returns it.
#[test]
fn flate_only_the_permissive_decoder_reads_is_read_as_it_goes() {
    for encrypted in [false, true] {
        let mut peaks = Vec::new();
        for bytes in [1 << 18, 1 << 20, 1 << 22] {
            let data = common::noise(bytes, 7);
            let mut encoded = common::stored_zlib(&data, 65_535);
            for block in (2..encoded.len() - 4).step_by(65_535 + 5) {
                encoded[block + 3..block + 5].fill(1);
            }
            let pdf = if encrypted {
                encrypted_file("/Filter /FlateDecode", &encoded)
            } else {
                stream_file("/Filter /FlateDecode", &encoded)
            };
            let stream = object_3(&pdf);
            let expected = stream.decoded().unwrap();
            assert!(expected[..] == data[..], "{bytes} bytes");
            let (first, reading) = measure(|| {
                let mut reader = stream.decoded_reader().unwrap();
                read_up_to(&mut reader, 64, 64)
            });
            assert_eq!(first, data[..64]);
            peaks.push(reading.peak);
            assert!(read_twice(&stream, 4099) == data, "{bytes} bytes");
        }
        assert!(peaks[0] < 160 * 1024, "encrypted {encrypted}: {peaks:?}");
        assert!(
            peaks.iter().all(|&p| p <= peaks[0] + 4096),
            "encrypted {encrypted}: {peaks:?}"
        );
    }
}

/// A checkpoint resumes Flate data after the reader, stream and document that took it
/// are gone: mid-row for predictors, mid-block for AES, with back references reaching
/// before it, and in raw deflate. It holds the decoder's state, not the
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
        assert_eq!(read_up_to(&mut reader, offset, 4093), expected[..offset]);
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
        assert!(rest == expected[offset..], "{params} at {offset}");
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
    assert_eq!(result, Err(Cancelled));
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
            assert_eq!(reader.read(&mut buf[..len], &meter), Err(Cancelled));
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
    assert_eq!(read_up_to(&mut other, usize::MAX, 997), data);
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

/// The image bytes an image reader yields, read in pieces of `chunk` bytes, read
/// twice with a rewind between.
fn read_image_twice(stream: &Stream<'_>, params: &ImageDecodeParams, chunk: usize) -> Vec<u8> {
    let mut reader = stream.image_reader(params).unwrap();
    let first = read_up_to(&mut reader, usize::MAX, chunk);
    reader.rewind();
    let second = read_up_to(&mut reader, usize::MAX, chunk);
    assert!(second == first, "a rewound reader reads the same");
    first
}

/// The bytes an image reader yields where it reads every other piece of `chunk`
/// bytes and skips the rest, each skipped byte zero, and the bytes it held.
fn read_skipping(stream: &Stream<'_>, params: &ImageDecodeParams, chunk: usize) -> Vec<u8> {
    let meter = Meter::new(None);
    let mut reader = stream.image_reader(params).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0; chunk];
    for k in 0.. {
        let got = if k % 2 == 0 {
            reader.read(&mut buf, &meter).unwrap()
        } else {
            let got = reader.skip(chunk, &meter).unwrap();
            buf[..got].fill(0);
            got
        };
        out.extend_from_slice(&buf[..got]);
        if got == 0 {
            return out;
        }
    }
    unreachable!()
}

/// CCITT data read through an image reader yields what decoding the image whole
/// returns, as it decodes: data of every encoding, gray and indexed, with
/// `BlackIs1`, a `/Width` other than its columns, ending before the rows declared,
/// breaking off, and holding no row.
#[test]
fn ccitt_image_bytes_read_as_whole_decoding_returns_them() {
    let streams = [
        (
            "-1",
            &include_bytes!("../../hayro-ccitt/tests/streams/g4.bin")[..],
        ),
        (
            "0",
            &include_bytes!("../../hayro-ccitt/tests/streams/g3_1d.bin")[..],
        ),
        (
            "1",
            &include_bytes!("../../hayro-ccitt/tests/streams/g3_2d.bin")[..],
        ),
    ];
    let mut cases = 0;
    for (k, data) in streams {
        for cut in [data.len(), data.len() / 2, 3, 0] {
            for (height, black_is_1, indexed) in [
                (96, "", false),
                (96, "/BlackIs1 true", true),
                (150, "", false),
                (150, "", true),
                (5000, "", false),
                (90, "/BlackIs1 true", false),
            ] {
                let dict = format!(
                    "/Filter /CCITTFaxDecode /DecodeParms << /K {k} /Columns 211 {black_is_1} >>"
                );
                let pdf = stream_file(&dict, &data[..cut]);
                let stream = object_3(&pdf);
                let params = ImageDecodeParams {
                    is_indexed: indexed,
                    width: 100,
                    height,
                    ..Default::default()
                };
                let whole = stream.decoded_image(&params);
                let reported = stream.ccitt_image_data(&params).unwrap();
                for chunk in [1, 7, 4096] {
                    let read = read_image_twice(&stream, &params, chunk);
                    let case = format!("K {k}, cut {cut}, {height} rows, {indexed}, chunk {chunk}");
                    let skipping = read_skipping(&stream, &params, chunk);
                    assert_eq!(skipping.len(), read.len(), "{case}: skipping");
                    for (i, (a, b)) in skipping.chunks(chunk).zip(read.chunks(chunk)).enumerate() {
                        assert!(i % 2 == 1 || a == b, "{case}: skipping, piece {i}");
                    }
                    match &whole {
                        Ok(whole) => {
                            assert!(read == whole.data.as_ref(), "{case}");
                            let image = whole.image_data.as_ref().unwrap();
                            assert_eq!(
                                (reported.width, reported.height, reported.bits_per_component),
                                (image.width, image.height, image.bits_per_component),
                                "{case}"
                            );
                        }
                        Err(_) => assert!(read.is_empty(), "{case}"),
                    }
                    cases += 1;
                }
            }
        }
    }
    assert_eq!(cases, 3 * 4 * 6 * 3);
}

/// Encrypted CCITT data is decrypted as its rows are read, through a window: reading
/// all rows of AES-encrypted Group 4 data holds about as much whatever the data's
/// size, and yields the rows of the same data stored plain. Making the reader
/// decrypts nothing.
#[test]
fn encrypted_ccitt_rows_hold_a_window_whatever_the_data_size() {
    let columns = 64;
    let dict = format!("/Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns {columns} >>");
    let mut peaks = Vec::new();
    for bytes in [1 << 18, 1 << 20, 1 << 22] {
        // White rows, a bit each against the white row above.
        let data = vec![0xFF; bytes];
        let rows = 8 * bytes as u32;
        let params = ImageDecodeParams {
            width: columns,
            height: rows,
            ..Default::default()
        };
        // The rows read, and a sum of their bytes.
        let count = |stream: &Stream<'_>| {
            let mut reader = stream.image_reader(&params).unwrap();
            let (mut buf, mut read, mut sum) = (vec![0; 1000], 0, 0_u64);
            loop {
                match reader.read(&mut buf, &Meter::default()).unwrap() {
                    0 => break,
                    n => {
                        read += n;
                        sum += buf[..n].iter().map(|&b| u64::from(b)).sum::<u64>();
                    }
                }
            }
            (read / columns as usize, sum)
        };
        let plain = stream_file(&dict, &data);
        let expected = count(&object_3(&plain));
        assert_eq!(expected.0, rows as usize);
        let encrypted = encrypted_file(&dict, &data);
        let stream = object_3(&encrypted);
        let (made, making) = measure(|| stream.image_reader(&params).is_some());
        assert!(made);
        assert!(
            making.largest < 1024,
            "making the reader allocated {making:?}"
        );
        let (read, reading) = measure(|| count(&stream));
        assert_eq!(read, expected, "{bytes} bytes");
        peaks.push(reading.peak);
    }
    assert!(peaks[0] < 160 * 1024, "{peaks:?}");
    assert!(peaks.iter().all(|&p| p <= peaks[0] + 4096), "{peaks:?}");
}

/// Bits written most significant first.
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    len: usize,
}

impl Bits {
    fn push(&mut self, code: &str) {
        for bit in code.bytes() {
            if self.len.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if bit == b'1' {
                *self.bytes.last_mut().unwrap() |= 0x80 >> (self.len % 8);
            }
            self.len += 1;
        }
    }
}

/// Encrypted Group 3 data whose damaged lines are each longer than the window of
/// decrypted bytes the CCITT decoder reads in: going back to the start of each, to
/// find the EOL after it, decrypts a bounded amount, not the data before it again,
/// so the bytes decrypted grow with the data, under AES and RC4 alike, and the rows
/// are those of the same data stored plain.
#[test]
#[cfg(debug_assertions)]
fn going_back_in_encrypted_ccitt_data_decrypts_a_bounded_amount() {
    use common::rc4::{self, Part};
    use hayro_syntax::work::TOLD;
    const EOL: &str = "000000000001";
    // Runs of no white and no black pixels: a line that never ends, to its EOL.
    const NOTHING: &str = "001101010000110111";
    let line_bytes = 160 * 1024;
    let dict = "/Filter /CCITTFaxDecode /DecodeParms << /K 0 /Columns 8 >>";
    for lines in [3, 12] {
        let mut bits = Bits::default();
        bits.push(EOL);
        for _ in 0..lines {
            for _ in 0..line_bytes * 8 / NOTHING.len() {
                bits.push(NOTHING);
            }
            bits.push(EOL);
        }
        for _ in 0..5 {
            bits.push(EOL);
        }
        let data = bits.bytes;
        let params = ImageDecodeParams {
            width: 8,
            height: lines,
            ..Default::default()
        };
        // The bytes read and the work counted reading them.
        let read = |stream: &Stream<'_>| {
            let mut reader = stream.image_reader(&params).unwrap();
            let before = TOLD.get();
            let bytes = read_up_to(&mut reader, usize::MAX, 4096);
            (bytes, TOLD.get() - before)
        };
        let plain = stream_file(dict, &data);
        let (expected, plain_work) = read(&object_3(&plain));
        assert_eq!(expected.len(), 8 * lines as usize);
        let aes = encrypted_file(dict, &data);
        let rc4 = Pdf::new(rc4::file(
            &[
                Part::Object("<< /Type /Catalog /Pages 2 0 R >>", b""),
                Part::Object("<< /Type /Pages /Kids [] /Count 0 >>", b""),
                Part::StreamWith(dict, &data),
            ],
            &[],
        ))
        .unwrap();
        for (cipher, pdf) in [("AES", aes), ("RC4", rc4)] {
            let (rows, work) = read(&object_3(&pdf));
            assert!(rows == expected, "{cipher}, {lines} lines: other rows");
            let decrypted = work - plain_work;
            // Each byte decrypted once going forward and once going back, and a
            // bounded amount for each line gone back to.
            let bound = 2 * data.len() + lines as usize * 128 * 1024;
            println!(
                "{cipher}, {lines} lines, {} bytes: {decrypted} decrypted",
                data.len()
            );
            assert!(
                decrypted <= bound,
                "{cipher}, {lines} lines of {line_bytes} bytes: {decrypted} bytes \
                 decrypted, over {bound}"
            );
        }
    }
}

/// A Group 3 row followed by EOLs is read before them, plain or encrypted: reading
/// the first row of a line of eight white pixels followed by 256 KiB to 4 MiB of EOLs
/// counts the same few bytes of work (bits read, bytes decrypted), whether the image
/// declares that row alone or two, and gives the row.
#[test]
#[cfg(debug_assertions)]
fn a_ccitt_row_is_read_before_the_eols_after_it() {
    use hayro_syntax::work::TOLD;
    // A white run of eight, then a pair of EOLs each three bytes.
    const LINE: u8 = 0x98;
    const EOLS: [u8; 3] = [0x00, 0x10, 0x01];
    let dict = "/Filter /CCITTFaxDecode /DecodeParms << /K 0 /Columns 8 >>";
    for height in [1, 2] {
        let params = ImageDecodeParams {
            width: 8,
            height,
            ..Default::default()
        };
        let mut works = Vec::new();
        for len in [1 << 18, 1 << 20, 4 << 20] {
            let mut data = vec![LINE];
            for _ in 0..len / 3 {
                data.extend(EOLS);
            }
            for (cipher, pdf) in [
                ("plain", stream_file(dict, &data)),
                ("AES", encrypted_file(dict, &data)),
            ] {
                let stream = object_3(&pdf);
                let mut reader = stream.image_reader(&params).unwrap();
                TOLD.set(0);
                let row = read_up_to(&mut reader, 8, 8);
                let work = TOLD.get();
                assert_eq!(
                    row,
                    [0xFF; 8],
                    "{cipher}, {height} rows, {} bytes",
                    data.len()
                );
                println!("{cipher}, {height} rows, {} bytes: {work} work", data.len());
                works.push((cipher, data.len(), work));
            }
        }
        for cipher in ["plain", "AES"] {
            let works: Vec<_> = works.iter().filter(|w| w.0 == cipher).collect();
            assert!(
                works.iter().all(|w| w.2 == works[0].2 && w.2 < 64 * 1024),
                "{cipher}, {height} rows: {works:?}"
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
    // The output and the two lists of changes, the parameters being read where the
    // stream's dictionary was; 13 when the output grew from an eighth of its size and
    // the lists change by change.
    assert_eq!(used.count, 3, "{used:?}");
}

/// The decoded data of `stream` read in pieces of random sizes, then again after a
/// rewind; `Err` once a read is cancelled.
fn read_in_pieces(
    reader: &mut DecodedReader<'_>,
    rng: &mut chains::Rng,
    meter: &Meter<'_>,
) -> Result<Vec<u8>, Cancelled> {
    let mut buf = vec![0; 70_000];
    let mut out = Vec::new();
    loop {
        let len = 1 + rng.below(70_000);
        match reader.read(&mut buf[..len], meter)? {
            0 => return Ok(out),
            n => {
                assert!(n <= POLL_BYTES);
                out.extend_from_slice(&buf[..n]);
            }
        }
    }
}

/// The reader yields what lenient decoding returns for chains of up to three filters
/// of every kind over data of every sort, whole, cut short, damaged or followed by
/// garbage at any layer, plain or encrypted, read in pieces of every size; a rewound
/// reader reads the same again. Only an image filter or a predictor only whole
/// decoding reverses leaves a chain without a reader.
#[test]
fn the_reader_reads_what_lenient_decoding_returns() {
    let mut rng = chains::Rng(0x51_7cc1_b727_220a);
    let (mut read, mut damaged_somewhere) = (0, 0);
    for case in 0..1500 {
        let mut encoded = chains::data(&mut rng);
        let intact = encoded.clone();
        let (mut filters, mut params) = (Vec::new(), Vec::new());
        for _ in 0..1 + rng.below(3) {
            let (filter, param, layer) = chains::layer(&encoded, &mut rng);
            encoded = chains::damaged(layer, &mut rng);
            filters.insert(0, filter);
            params.insert(0, param);
        }
        let dict = format!(
            "/Filter [{}] /DecodeParms [{}]",
            filters.join(" "),
            params.join(" ")
        );
        let pdf = match case % 4 {
            0 => encrypted_file(&dict, &encoded),
            _ => stream_file(&dict, &encoded),
        };
        let stream = object_3(&pdf);
        let whole = stream.decoded().map(|d| d.into_owned());
        let Some(mut reader) = stream.decoded_reader() else {
            assert!(
                filters.iter().any(|f| f == "/Unknown")
                    || params.iter().any(|p| p.starts_with("<< /Predictor 2 ")
                        && p.contains("/BitsPerComponent 16 ")),
                "case {case}: {dict}: no reader"
            );
            assert!(!stream.can_read_incrementally(), "case {case}: {dict}");
            continue;
        };
        assert!(stream.can_read_incrementally(), "case {case}: {dict}");
        let first = read_in_pieces(&mut reader, &mut rng, &Meter::default()).unwrap();
        match &whole {
            Ok(whole) => assert!(
                *whole == first,
                "case {case}: {dict}: {} vs {} bytes",
                whole.len(),
                first.len()
            ),
            Err(e) => panic!("case {case}: {dict}: {e:?}, read {} bytes", first.len()),
        }
        reader.rewind();
        let again = read_in_pieces(&mut reader, &mut rng, &Meter::default()).unwrap();
        assert!(again == first, "case {case}: {dict}: read again");
        read += 1;
        damaged_somewhere += usize::from(first != intact);
    }
    assert!(
        read > 1000 && damaged_somewhere > 100,
        "{read} read, {damaged_somewhere} damaged"
    );
}

/// Each filter the reader decodes a byte or a few at a time, alone and under Flate,
/// over data cut at every byte and with every byte changed: the reader yields what
/// lenient decoding returns, the bytes it made before the damage.
#[test]
fn damage_at_every_byte_reads_as_lenient_decoding() {
    let mut rng = chains::Rng(0x2545_f491_4f6c_dd1d);
    let data: Vec<u8> = (0..1500_u32).map(|i| (i * 7 / 5 % 251) as u8).collect();
    let flate = chains::zlib(&data, 6);
    let mut cases: Vec<(String, Vec<u8>)> = Vec::new();
    for (inner, encoded) in [("", data.clone()), ("/FlateDecode", flate)] {
        for (filter, layer) in [
            ("/LZWDecode", chains::lzw(&encoded, true, &mut rng)),
            ("/AHx", chains::ascii_hex(&encoded, &mut rng)),
            ("/A85", chains::ascii_85(&encoded, &mut rng)),
            ("/RunLengthDecode", chains::run_length(&encoded, &mut rng)),
        ] {
            cases.push((format!("/Filter [{filter} {inner}]"), layer));
        }
    }
    for (dict, encoded) in cases {
        let mut kept = std::collections::BTreeSet::new();
        for at in 0..encoded.len() {
            for damage in 0..3 {
                let mut damaged = encoded.clone();
                match damage {
                    0 => damaged.truncate(at),
                    1 => damaged[at] ^= 0xff,
                    _ => damaged[at] = b"{z\x80"[at % 3],
                }
                let pdf = stream_file(&dict, &damaged);
                let stream = object_3(&pdf);
                let whole = stream.decoded().unwrap().into_owned();
                for chunk in [1, 4096] {
                    let mut reader = stream.decoded_reader().unwrap();
                    let read = read_up_to(&mut reader, usize::MAX, chunk);
                    assert!(
                        read == whole,
                        "{dict}: damage {damage} at {at}, chunk {chunk}"
                    );
                }
                kept.insert(whole.len());
            }
        }
        // The damage ends the data at many places, not only at its start or end.
        assert!(kept.len() > 20, "{dict}: {} lengths kept", kept.len());
    }
}

/// Decoding damaged ASCII base-85, ASCII hex, run length and LZW data keeps what comes
/// before the damage, as `MuPDF` does (observed with `PyMuPDF` 1.28.2), and the reader
/// reads the same.
#[test]
fn lenient_decoding_keeps_what_comes_before_damage() {
    let eight: Vec<u8> = (1..=8).collect();
    // The bytes 1 to 8, in two ASCII base-85 groups.
    let groups = &b"!<N?+\"U52;"[..];
    let mut wrapped = eight.clone();
    wrapped.extend([0x08, 0x78, 0x0e, 0xc4]);
    wrapped.extend(&eight);
    let runs = [2, 1, 2, 3, 254, 9];
    let lzw = {
        // Clear, `A`, `B`, then code 300, past the table's 260 entries.
        let bits = [256_u64, 65, 66, 300].iter().fold(0, |b, c| b << 9 | c);
        (bits << 28).to_be_bytes()[..5].to_vec()
    };
    let cases: [(&str, Vec<u8>, Vec<u8>); 11] = [
        ("ASCII85Decode", [groups, b"{"].concat(), eight.clone()),
        ("ASCII85Decode", [groups, b"9j{"].concat(), eight.clone()),
        ("ASCII85Decode", [groups, b"9jz"].concat(), eight.clone()),
        ("ASCII85Decode", [groups, b"A~>"].concat(), eight.clone()),
        (
            "ASCII85Decode",
            [groups, b"uuuuu", groups].concat(),
            wrapped,
        ),
        ("ASCIIHexDecode", b"0102 0x03>".to_vec(), vec![1, 2]),
        ("ASCIIHexDecode", b"01023x>".to_vec(), vec![1, 2]),
        ("RunLengthDecode", runs.to_vec(), vec![1, 2, 3, 9, 9, 9]),
        (
            "RunLengthDecode",
            [&runs[..], &[250]].concat(),
            vec![1, 2, 3, 9, 9, 9],
        ),
        (
            "RunLengthDecode",
            [&runs[..], &[5, 7, 8]].concat(),
            vec![1, 2, 3, 9, 9, 9, 7, 8],
        ),
        ("LZWDecode", lzw, b"AB".to_vec()),
    ];
    for (filter, data, expected) in cases {
        let pdf = stream_file(&format!("/Filter /{filter}"), &data);
        let stream = object_3(&pdf);
        let decoded = stream.decoded().ok().map(|d| d.into_owned());
        assert_eq!(decoded.as_ref(), Some(&expected), "{filter} {data:?}");
        let mut reader = stream.decoded_reader().unwrap();
        assert_eq!(read_up_to(&mut reader, usize::MAX, 3), expected, "{filter}");
    }
}

fn on_stack<T: Send>(stack: usize, f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(stack)
            .spawn_scoped(scope, f)
            .unwrap()
            .join()
            .unwrap()
    })
}

/// Chains of 10,000 and 100,000 filters, ASCII hex or Flate, over empty data are read
/// in pieces, skipped, read again after a rewind, cancelled, checkpointed and dropped
/// on a 256 KiB stack, yielding what `decoded` returns: the stack a reader's
/// operations go down does not grow with the number of filters.
#[test]
fn deep_chains_are_read_on_a_small_stack() {
    for filter in ["/AHx", "/Fl"] {
        for n in [10_000, 100_000] {
            on_stack(256 << 10, move || {
                let dict = format!("/Filter [{}]", [filter].repeat(n).join(" "));
                let pdf = stream_file(&dict, b"");
                let stream = object_3(&pdf);
                let whole = stream.decoded().map(|d| d.into_owned()).unwrap_or_default();
                let mut reader = stream.decoded_reader().expect("a reader");
                let mut rng = chains::Rng(n as u64);
                let first = read_in_pieces(&mut reader, &mut rng, &Meter::default());
                assert_eq!(first, Ok(whole.clone()), "{n} {filter}");
                reader.rewind();
                assert_eq!(reader.skip(10, &Meter::default()), Ok(whole.len().min(10)));
                reader.rewind();
                let again = read_in_pieces(&mut reader, &mut rng, &Meter::default());
                assert_eq!(again, Ok(whole.clone()), "{n} {filter}: read again");
                #[cfg(feature = "unsafe")]
                {
                    assert!(reader.checkpoint_heap_bound().is_none());
                    assert!(reader.checkpoint(&stream.to_owned_stream()).is_none());
                }
                // Each filter counts a step of work, so the hook is polled once a
                // poll's worth of them are decoded.
                let stop = || true;
                let mut cancelled = stream.decoded_reader().unwrap();
                let mut buf = [0; 16];
                let read = cancelled.read(&mut buf, &Meter::new(Some(&stop)));
                assert!(n < POLL_BYTES || read == Err(Cancelled), "{n} {filter}: {read:?}");
                drop((reader, cancelled));
            });
        }
    }
}

/// Chains of more filters than the reader decodes as it reads (17 to 24), of every
/// kind, over data of every sort, some damaged, plain or encrypted, read in pieces:
/// the reader yields what lenient decoding returns, and again after a rewind and
/// after a read the meter's hook stopped.
#[test]
fn chains_longer_than_the_filters_read_incrementally_read_as_lenient_decoding() {
    let mut rng = chains::Rng(0x9e37_79b9_7f4a_7c15);
    let mut cases = 0;
    while cases < 40 {
        let mut encoded = chains::data(&mut rng);
        encoded.truncate(20_000);
        let (mut filters, mut params) = (Vec::new(), Vec::new());
        let n = 17 + rng.below(8);
        while filters.len() < n {
            let (filter, param, layer) = chains::layer(&encoded, &mut rng);
            if layer.len() > 100_000 || filter == "/Unknown" || param.contains("/Predictor 2 ")
            {
                continue;
            }
            encoded = match rng.chance(10) {
                true => chains::damaged(layer, &mut rng),
                false => layer,
            };
            filters.insert(0, filter);
            params.insert(0, param);
        }
        let dict = format!(
            "/Filter [{}] /DecodeParms [{}]",
            filters.join(" "),
            params.join(" ")
        );
        let pdf = match cases % 4 {
            0 => encrypted_file(&dict, &encoded),
            _ => stream_file(&dict, &encoded),
        };
        let stream = object_3(&pdf);
        let whole = stream.decoded().map(|d| d.into_owned()).unwrap_or_default();
        let mut reader = stream.decoded_reader().expect("a reader");
        let first = read_in_pieces(&mut reader, &mut rng, &Meter::default()).unwrap();
        assert!(first == whole, "{dict}: {} vs {} bytes", first.len(), whole.len());
        reader.rewind();
        let again = read_in_pieces(&mut reader, &mut rng, &Meter::default()).unwrap();
        assert!(again == whole, "{dict}: read again");
        let polls = Cell::new(0);
        let second = || {
            polls.set(polls.get() + 1);
            polls.get() > 1
        };
        let mut stopped = stream.decoded_reader().unwrap();
        let read = read_in_pieces(&mut stopped, &mut rng, &Meter::new(Some(&second)));
        if read.is_err() {
            stopped.rewind();
            let after = read_in_pieces(&mut stopped, &mut rng, &Meter::default()).unwrap();
            assert!(after == whole, "{dict}: read after a stop");
        }
        cases += 1;
    }
}
