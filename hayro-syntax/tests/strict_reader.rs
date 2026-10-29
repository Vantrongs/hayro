//! `Stream::decoded_strict_reader` reads what `Stream::decoded_strict` returns, through
//! every chain of filters, in memory that does not grow with what the filters make of
//! the data, and stops when its meter's hook asks it to.

use hayro_syntax::Pdf;
use hayro_syntax::object::stream::{DecodeFailure, ReadError, StrictReader};
use hayro_syntax::object::{ObjectIdentifier, Stream};
use hayro_syntax::work::{Meter, POLL_BYTES};
use std::collections::BTreeMap;

mod common;
use common::allocations::measure;
use common::chains::{Rng, ascii_hex, damaged, data, layer, lzw, zlib};

/// A document whose object 3 is a stream with `dict` entries holding `data`.
fn stream_file(dict: &str, data: &[u8]) -> Pdf {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
        common::stream(dict, data),
    ];
    Pdf::new(common::file(&objects, "")).unwrap()
}

fn object_3(pdf: &Pdf) -> Stream<'_> {
    pdf.xref().get(ObjectIdentifier::new(3, 0)).unwrap()
}

/// How a reading ended: the bytes after the last restart up to the end, or the error;
/// and how often it restarted.
fn read_all(
    reader: &mut StrictReader<'_>,
    rng: &mut Rng,
    meter: &Meter<'_>,
) -> (Result<Vec<u8>, ReadError>, usize) {
    let mut buf = vec![0; 70_000];
    let mut out = Vec::new();
    let mut restarts = 0;
    loop {
        let len = 1 + rng.below(70_000);
        match reader.read(&mut buf[..len], meter) {
            Ok(0) => return (Ok(out), restarts),
            Ok(n) => {
                assert!(n <= POLL_BYTES);
                out.extend_from_slice(&buf[..n]);
            }
            Err(ReadError::Restarted) => {
                restarts += 1;
                out.clear();
            }
            Err(error) => return (Err(error), restarts),
        }
    }
}

/// The reader yields what `decoded_strict` returns, and fails where it fails, for
/// chains of up to three filters of every kind over data of every sort, whole, cut
/// short, damaged or followed by garbage, at any layer, read in pieces of every
/// size; and a rewound reader reads the same again with no restart.
#[test]
fn the_reader_reads_what_strict_decoding_returns() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut outcomes = BTreeMap::new();
    for case in 0..3000 {
        let mut encoded = data(&mut rng);
        let (mut filters, mut params) = (Vec::new(), Vec::new());
        for _ in 0..1 + rng.below(3) {
            let (filter, param, layer) = layer(&encoded, &mut rng);
            encoded = damaged(layer, &mut rng);
            filters.insert(0, filter);
            params.insert(0, param);
        }
        let dict = format!(
            "/Filter [{}] /DecodeParms [{}]",
            filters.join(" "),
            params.join(" ")
        );
        let pdf = stream_file(&dict, &encoded);
        let stream = object_3(&pdf);
        let whole = stream.decoded_strict().map(|d| d.into_owned());
        let Some(mut reader) = stream.decoded_strict_reader(&Meter::default()).unwrap() else {
            assert!(
                params
                    .iter()
                    .any(|p| p.starts_with("<< /Predictor 2 ")
                        && !p.contains("/BitsPerComponent 8 ")),
                "case {case}: {dict}: no reader"
            );
            *outcomes.entry("whole").or_insert(0) += 1;
            continue;
        };
        let (read, restarts) = read_all(&mut reader, &mut rng, &Meter::default());
        match (&whole, &read) {
            (Ok(whole), Ok(read)) => {
                assert!(
                    whole == read,
                    "case {case}: {dict}: {} vs {} bytes",
                    whole.len(),
                    read.len()
                );
            }
            (Err(DecodeFailure::StreamDecode), Err(ReadError::Failed)) => {}
            _ => panic!(
                "case {case}: {dict}: {:?} vs {:?}",
                whole.as_ref().map(Vec::len),
                read.as_ref().map(Vec::len)
            ),
        }
        reader.rewind();
        let again = read_all(&mut reader, &mut rng, &Meter::default());
        assert_eq!(again, (read.clone(), 0), "case {case}: {dict}: read again");
        let outcome = match (read.is_ok(), restarts) {
            (true, 0) => "read",
            (true, _) => "read after a restart",
            (false, 0) => "failed",
            (false, _) => "failed after a restart",
        };
        *outcomes.entry(outcome).or_insert(0) += 1;
    }
    println!("{outcomes:?}");
    // Every outcome is reached, restarts where zlib-rs decodes first.
    let expected = if cfg!(feature = "unsafe") { 5 } else { 3 };
    assert_eq!(outcomes.len(), expected, "{outcomes:?}");
}

/// Predicted rows whose last row is cut short at every length, under PNG and TIFF
/// prediction and pixels of every kind, and LZW data whose last code is cut short,
/// each in an outer filter or not: the reader reads what strict decoding returns, and
/// that keeps the row cut short, past its PNG filter byte.
#[test]
fn rows_and_codes_cut_short_read_what_strict_decoding_returns() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut pieces = Rng(0x9e37_79b9_7f4a_7c15);
    let hex = |data: &[u8]| -> Vec<u8> {
        data.iter()
            .flat_map(|b| format!("{b:02X}").into_bytes())
            .chain(*b">")
            .collect()
    };
    let mut check = |filter: &str, parms: &str, encoded: &[u8]| {
        let mut decoded = Vec::new();
        for outer in [false, true] {
            let (dict, data) = if outer {
                let dict = format!("/Filter [/AHx {filter}] /DecodeParms [null {parms}]");
                (dict, hex(encoded))
            } else {
                (
                    format!("/Filter {filter} /DecodeParms {parms}"),
                    encoded.to_vec(),
                )
            };
            let pdf = stream_file(&dict, &data);
            let stream = object_3(&pdf);
            let whole = stream.decoded_strict().unwrap().into_owned();
            match stream.decoded_strict_reader(&Meter::default()).unwrap() {
                Some(mut reader) => {
                    let (read, _) = read_all(&mut reader, &mut pieces, &Meter::default());
                    assert_eq!(read.as_ref(), Ok(&whole), "{dict}");
                }
                None => assert!(parms.starts_with("<< /Predictor 2 ") && !parms.contains(" 8 ")),
            }
            decoded.push(whole);
        }
        assert_eq!(decoded[0], decoded[1], "{filter} {parms}");
        decoded.swap_remove(0)
    };
    for (predictor, colors, bpc, columns) in [
        (12_usize, 1_usize, 8_usize, 5_usize),
        (12, 3, 8, 2),
        (12, 5, 8, 2),
        (12, 3, 4, 3),
        (12, 3, 16, 1),
        (2, 3, 8, 2),
        (2, 5, 8, 2),
        (2, 1, 16, 3),
        (2, 1, 4, 11),
    ] {
        let row = (colors * bpc * columns).div_ceil(8);
        let png = usize::from(predictor >= 10);
        let parms = format!(
            "<< /Predictor {predictor} /Colors {colors} /BitsPerComponent {bpc} /Columns {columns} >>"
        );
        for len in 2 * (row + png)..=3 * (row + png) {
            let mut rows = common::noise(len, len as u32);
            if png == 1 {
                for b in rows.iter_mut().step_by(row + 1) {
                    *b %= 5;
                }
            }
            let cut = len % (row + png);
            let expected = len / (row + png) * row + cut.saturating_sub(png);
            let lzw = lzw(&rows, true, &mut rng);
            for (filter, encoded) in [("/FlateDecode", zlib(&rows, 6)), ("/LZWDecode", lzw)] {
                let decoded = check(filter, &parms, &encoded);
                assert_eq!(decoded.len(), expected, "{filter} {parms}: {len} bytes");
            }
        }
    }
    // LZW cut inside its last code or its end-of-data code, with a predictor or not.
    let data = common::noise(3000, 5);
    let encoded = lzw(&data, true, &mut rng);
    for cut in 1..=4 {
        let encoded = &encoded[..encoded.len() - cut];
        for parms in ["null", "<< /Predictor 12 /Colors 3 /Columns 7 >>"] {
            check("/LZWDecode", parms, encoded);
        }
    }
}

/// Data with a bad checksum, which zlib rejects and the permissive decoder reads in
/// strict mode too, inside another Flate filter.
fn bad_checksum(data: &[u8]) -> Vec<u8> {
    let mut inner = zlib(data, 6);
    let last = inner.len() - 1;
    inner[last] ^= 1;
    inner
}

/// The peak heap bytes reading object 3 of `pdf` held, the bytes it read, and the work
/// counted.
fn peak_of_reading(pdf: &Pdf) -> (usize, usize, usize) {
    let stream = object_3(pdf);
    let mut buf = vec![0; POLL_BYTES];
    let ((read, work), allocations) = measure(|| {
        #[cfg(debug_assertions)]
        hayro_syntax::work::TOLD.set(0);
        let mut reader = stream
            .decoded_strict_reader(&Meter::default())
            .unwrap()
            .unwrap();
        let mut read = 0;
        loop {
            match reader.read(&mut buf, &Meter::default()) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(ReadError::Restarted) => read = 0,
                Err(e) => panic!("{e:?}"),
            }
        }
        #[cfg(debug_assertions)]
        let work = hayro_syntax::work::TOLD.get();
        #[cfg(not(debug_assertions))]
        let work = read;
        (read, work)
    });
    (allocations.peak, read, work)
}

/// Flate inside Flate that decodes to 16 or 64 MiB is read in the same memory, well
/// below what it decodes to, and with work proportional to it; so is data the inner
/// Flate filter needs the permissive decoder for.
#[test]
fn nested_flate_is_read_in_memory_bounded_whatever_it_decodes_to() {
    type Encode = fn(&[u8]) -> Vec<u8>;
    let cases: [(&str, Encode, [usize; 2]); 2] = [
        ("zlib", |d| zlib(d, 9), [16 << 20, 64 << 20]),
        ("bad checksum", bad_checksum, [8 << 20, 32 << 20]),
    ];
    for (name, inner, sizes) in cases {
        let peaks = sizes.map(|size| {
            // Runs of 32 bytes: the inner data is large enough to fill the outer
            // decoder's window many times over.
            let noise = common::noise(size / 32, 5);
            let data: Vec<u8> = (0..size).map(|i| noise[i / 32]).collect();
            let encoded = zlib(&inner(&data), 9);
            let pdf = stream_file("/Filter [/FlateDecode /FlateDecode]", &encoded);
            let (peak, read, work) = peak_of_reading(&pdf);
            assert_eq!(read, size, "{name}");
            assert!(
                peak < 2 << 20,
                "{name}: {size} bytes read with a peak of {peak}"
            );
            assert!(
                (size..8 * size).contains(&work),
                "{name}: {size} bytes read with {work} bytes of work"
            );
            println!("{name}: {size} bytes: peak {peak}, work {work}");
            peak
        });
        // Within a decoder's buffers of what the smaller data held, or of a layer
        // held whole, up to 1 MiB (`HELD`), which smaller data may not reach.
        assert!(
            peaks[1] < peaks[0].max(1 << 20) + 128 * 1024,
            "{name}: peaks {peaks:?} grow with the data"
        );
    }
}

/// Flate inside Flate whose outer data have a checksum zlib rejects, found once the
/// inner data were read, is read in memory that does not grow with what the outer
/// filter makes: here a few bytes of content followed by 8 or 32 MiB the inner filter
/// does not read.
#[test]
fn a_damaged_outer_flate_is_read_in_memory_bounded_whatever_it_decodes_to() {
    let inner = zlib(b"q Q", 9);
    let peaks = [8 << 20, 32 << 20].map(|tail| {
        let mut data = inner.clone();
        data.resize(inner.len() + tail, 0);
        let pdf = stream_file("/Filter [/FlateDecode /FlateDecode]", &bad_checksum(&data));
        assert_eq!(object_3(&pdf).decoded_strict().unwrap().as_ref(), b"q Q");
        let (peak, read, work) = peak_of_reading(&pdf);
        assert_eq!(read, 3);
        assert!(
            work < 8 * tail,
            "{tail} bytes after the content read with {work} bytes of work"
        );
        println!("{tail} bytes after the content: peak {peak}, work {work}");
        peak
    });
    assert!(
        peaks[1] < (2 << 20).min(peaks[0] + 128 * 1024),
        "peaks {peaks:?} grow with the data"
    );
}

/// A predictor over rows of one byte is read many rows at a time: a read fills the
/// buffer it is given, so that short rows do not cost a read each.
#[test]
fn short_predictor_rows_are_read_many_at_a_time() {
    let data: Vec<u8> = (0..65_536_u32).map(|i| (i * 7) as u8).collect();
    let parms = "/Predictor 2 /Colors 1 /BitsPerComponent 8 /Columns 1";
    let pdf = stream_file(
        &format!("/Filter /FlateDecode /DecodeParms << {parms} >>"),
        &zlib(&data, 6),
    );
    let stream = object_3(&pdf);
    assert_eq!(stream.decoded_strict().unwrap().as_ref(), &data[..]);
    let meter = Meter::default();
    let mut reader = stream.decoded_strict_reader(&meter).unwrap().unwrap();
    let mut buf = vec![0; 4096];
    assert_eq!(reader.read(&mut buf, &meter), Ok(4096));
    assert_eq!(buf, data[..4096]);
}

/// A hook that asks to stop ends the reading within a bounded amount of work, and
/// every read after fails the same.
#[test]
#[cfg(debug_assertions)]
fn a_stop_ends_the_reading_soon() {
    use hayro_syntax::work::TOLD;
    use std::cell::Cell;
    let data: Vec<u8> = (0..16 << 20).map(|i| (i / 4096 % 7) as u8).collect();
    let chains = [
        (
            "/Filter [/FlateDecode /FlateDecode]",
            zlib(&zlib(&data, 9), 9),
        ),
        (
            "/Filter [/FlateDecode /FlateDecode]",
            zlib(&bad_checksum(&data), 9),
        ),
        (
            "/Filter [/AHx /FlateDecode]",
            ascii_hex(&zlib(&data, 9), &mut Rng(1)),
        ),
    ];
    for (dict, encoded) in chains {
        let pdf = stream_file(dict, &encoded);
        let stream = object_3(&pdf);
        for stop_at in [1, 3, 20] {
            let polls = Cell::new(0);
            let told_at_stop = Cell::new(0);
            let hook = || {
                polls.set(polls.get() + 1);
                let stop = polls.get() == stop_at;
                if stop {
                    told_at_stop.set(TOLD.get());
                }
                stop
            };
            let meter = Meter::new(Some(&hook));
            TOLD.set(0);
            let mut reader = stream.decoded_strict_reader(&meter).unwrap().unwrap();
            let (read, _) = read_all(&mut reader, &mut Rng(7), &meter);
            assert_eq!(read, Err(ReadError::Cancelled), "{dict}");
            assert_eq!(polls.get(), stop_at, "{dict}");
            let after = TOLD.get() - told_at_stop.get();
            assert!(
                after < 2 * POLL_BYTES,
                "{dict}: {after} bytes of work after the stop"
            );
            let mut buf = [0; 16];
            assert_eq!(reader.read(&mut buf, &meter), Err(ReadError::Cancelled));
        }
    }
}

/// A filter that ends before the data of the filter before it do still fails where
/// those data fail, as decoding them whole fails first.
#[test]
fn an_end_does_not_hide_a_failure_before_it() {
    // A Flate stream ends in an AHx stream that fails after it.
    let mut hex = ascii_hex(&zlib(b"0 0 m 10 10 l S", 6), &mut Rng(3));
    hex.retain(|&b| b != b'>');
    hex.extend(b"00".repeat(1 << 20));
    hex.extend(b"Q>");
    // An AHx stream ends in a Flate stream with a block of no valid type after it.
    let mut text = b"414243>".to_vec();
    text.extend(common::noise(100_000, 9));
    let mut flate = vec![0x78, 0x01];
    for block in text.chunks(65_535) {
        let len = block.len() as u16;
        flate.push(0);
        flate.extend(len.to_le_bytes());
        flate.extend((!len).to_le_bytes());
        flate.extend_from_slice(block);
    }
    flate.push(0b111);
    for (dict, data) in [
        ("/Filter [/AHx /FlateDecode]", hex),
        ("/Filter [/FlateDecode /AHx]", flate),
    ] {
        let pdf = stream_file(dict, &data);
        let stream = object_3(&pdf);
        let whole = stream.decoded_strict().map(|d| d.len());
        assert!(
            matches!(whole, Err(DecodeFailure::StreamDecode)),
            "{dict}: {whole:?}"
        );
        let mut reader = stream
            .decoded_strict_reader(&Meter::default())
            .unwrap()
            .unwrap();
        let (read, _) = read_all(&mut reader, &mut Rng(5), &Meter::default());
        assert_eq!(read, Err(ReadError::Failed), "{dict}");
    }
}

/// Runs `f` on a thread whose stack is `stack` bytes.
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

/// Chains of 10,000 and 100,000 filters, `ASCIIHex` or `Flate`, over empty data are read,
/// read again and dropped on a 256 KiB stack, as `decoded_strict` reads them, in
/// memory that grows with the number of filters by a few dozen bytes each, as their
/// names in the file do, not by the buffers and window of a filter's decoder.
#[test]
fn deep_chains_are_read_on_a_small_stack_in_memory_of_their_filters() {
    for filter in ["/AHx", "/Fl"] {
        let peaks = [10_000, 100_000].map(|n| {
            let dict = format!("/Filter [{}]", [filter].repeat(n).join(" "));
            on_stack(256 << 10, move || {
                let pdf = stream_file(&dict, b"");
                let stream = object_3(&pdf);
                assert_eq!(stream.decoded_strict().unwrap().len(), 0);
                let (reads, allocations) = measure(|| {
                    let meter = Meter::default();
                    let mut reader = stream.decoded_strict_reader(&meter).unwrap().unwrap();
                    let first = read_all(&mut reader, &mut Rng(3), &meter);
                    reader.rewind();
                    (first, read_all(&mut reader, &mut Rng(4), &meter))
                });
                assert_eq!(reads.0, (Ok(Vec::new()), 0), "{n} {filter}");
                assert_eq!(reads.1, (Ok(Vec::new()), 0), "{n} {filter}: read again");
                println!("{n} {filter}: peak {}", allocations.peak);
                allocations.peak
            })
        });
        assert!(
            peaks[1] < peaks[0] + 64 * 90_000,
            "{filter}: peaks {peaks:?} for 10,000 and 100,000 filters"
        );
    }
}

/// Flate filters nested `n` deep, each with a checksum zlib rejects at its end, are
/// read as `decoded_strict` reads them, with work proportional to what the filters
/// make: a filter that goes on with the next decoder decodes its own data again, not
/// those of every filter before it.
#[test]
#[cfg(debug_assertions)]
fn nested_bad_checksums_are_read_with_work_linear_in_their_number() {
    use hayro_syntax::work::{STATE_MADE, STATES_MADE, TOLD};
    let data = b"0 0 1 rg 20 20 160 160 re f\n".repeat(40);
    for n in [16, 32, 64, 128] {
        let mut encoded = data.clone();
        // What all the filters make.
        let mut made = 0;
        for _ in 0..n {
            made += encoded.len();
            encoded = bad_checksum(&encoded);
        }
        let pdf = stream_file(
            &format!("/Filter [{}]", ["/Fl"].repeat(n).join(" ")),
            &encoded,
        );
        let stream = object_3(&pdf);
        assert_eq!(stream.decoded_strict().unwrap().as_ref(), data, "{n}");
        TOLD.set(0);
        STATE_MADE.set(0);
        STATES_MADE.set(0);
        let mut reader = stream
            .decoded_strict_reader(&Meter::default())
            .unwrap()
            .unwrap();
        let (read, _) = read_all(&mut reader, &mut Rng(5), &Meter::default());
        // The decoders' states made count apart, two at most for each filter.
        let work = TOLD.get() - STATE_MADE.get();
        let states = STATES_MADE.get();
        assert!(
            states <= 2 * n,
            "{n} filters: {states} decoders' states made"
        );
        assert_eq!(read, Ok(data.clone()), "{n}");
        println!("{n} filters: work {work} for {made} bytes made");
        // Each filter takes its data and makes its output with zlib, then with the
        // permissive decoder.
        assert!(
            work < 6 * made,
            "{n} filters: work {work} for {made} bytes made"
        );
    }
}

/// [`read_all`], comparing the bytes with `expected` as they come rather than keeping
/// them: `Ok(None)` where they differ.
fn read_compared(
    reader: &mut StrictReader<'_>,
    expected: &[u8],
    rng: &mut Rng,
    meter: &Meter<'_>,
) -> (Result<Option<usize>, ReadError>, usize) {
    let mut buf = vec![0; 70_000];
    let (mut at, mut same, mut restarts) = (0, true, 0);
    loop {
        let len = 1 + rng.below(70_000);
        match reader.read(&mut buf[..len], meter) {
            Ok(0) => return (Ok(same.then_some(at)), restarts),
            Ok(n) => {
                same &= expected.get(at..at + n) == Some(&buf[..n]);
                at += n;
            }
            Err(ReadError::Restarted) => {
                restarts += 1;
                (at, same) = (0, true);
            }
            Err(error) => return (Err(error), restarts),
        }
    }
}

/// Chains of Flate filters, each making more than a megabyte, some with a checksum
/// zlib rejects, are read as `decoded_strict` reads them, with work proportional to
/// what each filter makes, a pass more for each damaged filter, and restart at most
/// twice for each damaged filter and twice for the last one; rewound, they read the
/// same again with no restart. They are read in less memory than decoding them whole
/// holds: with at most 17 filters, a megabyte held at most and the state and buffers
/// of each filter's decoder, none held whole; past that, one in 17 held whole.
#[test]
fn deep_expanding_chains_read_what_strict_decoding_returns() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut cases: Vec<(usize, Vec<usize>)> = vec![
        (12, vec![]),
        (12, vec![5]),
        (12, vec![11]),
        (12, vec![0]),
        (12, (0..12).collect()),
        (40, vec![]),
        (40, vec![3, 20]),
    ];
    for _ in 0..6 {
        let n = 2 + rng.below(11);
        let damaged = (0..n).filter(|_| rng.chance(25)).collect();
        cases.push((n, damaged));
    }
    for (case, (n, damaged)) in cases.into_iter().enumerate() {
        // Noise, which no filter makes smaller: each makes about as much.
        let content = common::noise(if n > 17 { 1_200_000 } else { 3_000_000 }, case as u32);
        let mut encoded = content.clone();
        // The innermost filter, the last one read, is layer 0.
        for layer in 0..n {
            encoded = match rng.below(3) {
                0 => zlib(&encoded, 1),
                _ => common::stored_zlib(&encoded, 65_535),
            };
            if damaged.contains(&layer) {
                let last = encoded.len() - 1;
                encoded[last] ^= 1;
            }
        }
        let dict = format!("/Filter [{}]", ["/Fl"].repeat(n).join(" "));
        let pdf = stream_file(&dict, &encoded);
        let stream = object_3(&pdf);
        let (whole, decoding) = measure(|| stream.decoded_strict().map(|d| d.into_owned()));
        let expected = whole.as_deref().unwrap_or(&[]);
        let ((read, restarts, again, work), allocations) = measure(|| {
            #[cfg(debug_assertions)]
            hayro_syntax::work::TOLD.set(0);
            let meter = Meter::default();
            let mut reader = stream.decoded_strict_reader(&meter).unwrap().unwrap();
            let (read, restarts) = read_compared(&mut reader, expected, &mut rng, &meter);
            #[cfg(debug_assertions)]
            let work = hayro_syntax::work::TOLD.get();
            #[cfg(not(debug_assertions))]
            let work = 0;
            reader.rewind();
            let again = read_compared(&mut reader, expected, &mut rng, &meter);
            (read, restarts, again, work)
        });
        let name = format!("case {case}: {n} filters, damaged {damaged:?}");
        match (&whole, &read) {
            (Ok(whole), Ok(read)) => assert_eq!(*read, Some(whole.len()), "{name}"),
            (Err(DecodeFailure::StreamDecode), Err(ReadError::Failed)) => {}
            _ => panic!("{name}: {:?} vs {read:?}", whole.as_ref().map(Vec::len)),
        }
        assert_eq!(again, (read, 0), "{name}: read again");
        assert!(
            restarts <= 2 * damaged.len() + 2,
            "{name}: {restarts} restarts"
        );
        // Each pass takes a filter's data and makes its output: the first pass, a
        // second where a filter streamed restarts, the last filter's two more
        // decoders, and the permissive decoder of each damaged filter.
        // Each pass takes a filter's data and makes its output: the first pass, the
        // last filter's two more decoders, and for each damaged filter a pass to its
        // end, its raw decoder's and its permissive decoder's.
        assert!(
            work < (3 * damaged.len() + 6) * n * content.len(),
            "{name}: {work} bytes of work"
        );
        let most = match n {
            ..=17 => decoding.peak.min((1 << 20) + n * (256 << 10)),
            _ => decoding.peak,
        };
        assert!(
            allocations.peak < most,
            "{name}: a peak of {}, {} decoding whole",
            allocations.peak,
            decoding.peak
        );
        println!(
            "{name}: {restarts} restarts, peak {} ({} decoding whole), work {work}",
            allocations.peak, decoding.peak
        );
    }
}
