//! `Stream::decoded_strict_reader` reads what `Stream::decoded_strict` returns, through
//! every chain of filters, in memory that does not grow with what the filters make of
//! the data, and stops when its meter's hook asks it to.

use hayro_syntax::Pdf;
use hayro_syntax::object::stream::{DecodeFailure, ReadError, StrictReader};
use hayro_syntax::object::{ObjectIdentifier, Stream};
use hayro_syntax::work::{Meter, POLL_BYTES};
use std::collections::BTreeMap;
use std::io::Write;

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

fn object_3(pdf: &Pdf) -> Stream<'_> {
    pdf.xref().get(ObjectIdentifier::new(3, 0)).unwrap()
}

/// A seeded xorshift generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
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

fn zlib(data: &[u8], level: u32) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(level));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn deflate(data: &[u8], level: u32) -> Vec<u8> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// `data` LZW-encoded as `lzw::decode` reads it: codes most significant bit first,
/// widened one code early if `early`, the table cleared before it fills.
fn lzw(data: &[u8], early: bool, rng: &mut Rng) -> Vec<u8> {
    let width = |next: usize| match next - 1 + usize::from(early) {
        0..512 => 9,
        512..1024 => 10,
        1024..2048 => 11,
        _ => 12,
    };
    let mut out = Vec::new();
    let (mut bits, mut count) = (0_u64, 0);
    let mut put = |code: usize, width: u32| {
        bits = bits << width | code as u64;
        count += width;
        while count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    };
    let mut table = BTreeMap::new();
    let mut next = 258;
    put(256, width(next));
    let mut word: Vec<u8> = Vec::new();
    let code = |table: &BTreeMap<Vec<u8>, usize>, word: &[u8]| match word {
        [byte] => usize::from(*byte),
        _ => table[word],
    };
    for &byte in data {
        let mut longer = word.clone();
        longer.push(byte);
        if word.is_empty() || table.contains_key(&longer) {
            word = longer;
            continue;
        }
        put(code(&table, &word), width(next));
        table.insert(longer, next);
        next += 1;
        if next >= 4094 || rng.chance(1) {
            put(256, width(next));
            table.clear();
            next = 258;
        }
        word = vec![byte];
    }
    if !word.is_empty() {
        put(code(&table, &word), width(next));
        next += 1;
    }
    put(257, width(next));
    put(0, 7);
    out
}

fn ascii_hex(data: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    for &b in data {
        out.extend_from_slice(format!("{b:02X}").as_bytes());
        if rng.chance(5) {
            out.push(b"\n \t\r"[rng.below(4)]);
        }
    }
    if rng.chance(50) {
        out.pop();
    }
    if rng.chance(80) {
        out.push(b'>');
    }
    out
}

fn ascii_85(data: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    for group in data.chunks(4) {
        if group == [0; 4] {
            out.push(b'z');
            continue;
        }
        let mut word = [0; 4];
        word[..group.len()].copy_from_slice(group);
        let mut value = u32::from_be_bytes(word);
        let mut digits = [0; 5];
        for d in digits.iter_mut().rev() {
            *d = b'!' + (value % 85) as u8;
            value /= 85;
        }
        out.extend_from_slice(&digits[..group.len() + 1]);
        if rng.chance(5) {
            out.push(b' ');
        }
    }
    match rng.below(3) {
        0 => out.extend_from_slice(b"~>"),
        1 => out.push(b'~'),
        _ => {}
    }
    out
}

fn run_length(data: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let same = rest.iter().take_while(|&&b| b == rest[0]).count().min(128);
        if same >= 2 {
            out.push((257 - same) as u8);
            out.push(rest[0]);
            rest = &rest[same..];
        } else {
            let n = rest.len().min(1 + rng.below(128));
            out.push((n - 1) as u8);
            out.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
        }
    }
    if rng.chance(80) {
        out.push(128);
    }
    out
}

/// Data to encode: text that compresses well, noise that does not, or runs.
fn data(rng: &mut Rng) -> Vec<u8> {
    let len = match rng.below(10) {
        0 => 0,
        1..=5 => rng.below(2_000),
        6..=8 => rng.below(40_000),
        _ => 100_000 + rng.below(200_000),
    };
    match rng.below(3) {
        0 => common::noise(len, rng.next() as u32),
        1 => (0..len)
            .map(|i| b"0 0 m 10 20 l S q 1 0 0 1 5 5 cm Q\n"[(i * 7 + i / 300) % 35])
            .collect(),
        _ => (0..len).map(|i| ((i / (1 + i % 300)) % 7) as u8).collect(),
    }
}

/// Damage the encoded bytes may carry: cut short, followed by garbage, or with bytes
/// changed, the first or the last too.
fn damaged(mut encoded: Vec<u8>, rng: &mut Rng) -> Vec<u8> {
    match rng.below(12) {
        0 if !encoded.is_empty() => {
            let at = rng.below(encoded.len());
            encoded.truncate(at);
        }
        1 => encoded.extend(common::noise(1 + rng.below(40), rng.next() as u32)),
        2 if !encoded.is_empty() => {
            for _ in 0..1 + rng.below(3) {
                let at = rng.below(encoded.len());
                encoded[at] ^= 1 << rng.below(8);
            }
        }
        3 if !encoded.is_empty() => {
            let last = encoded.len() - 1;
            encoded[last] ^= 1;
        }
        4 if !encoded.is_empty() => encoded[0] ^= 0x20,
        _ => {}
    }
    encoded
}

/// A filter of the chain and its parameters, encoding `data`.
fn layer(data: &[u8], rng: &mut Rng) -> (String, String, Vec<u8>) {
    let predicted = |rng: &mut Rng, data: &[u8]| -> (String, Vec<u8>) {
        if rng.chance(60) {
            return ("null".into(), data.to_vec());
        }
        // Rows of any bytes: each predictor decodes them to something.
        let (predictor, colors, bpc) = match rng.below(8) {
            0 => (2, 1 + rng.below(4), 8),
            1 => (2, 1, 4), // only the whole decoder reverses it
            _ => (10 + rng.below(6), 1 + rng.below(4), [8, 16][rng.below(2)]),
        };
        let columns = 1 + rng.below(60);
        let params = format!(
            "<< /Predictor {predictor} /Colors {colors} /BitsPerComponent {bpc} /Columns {columns} >>"
        );
        let mut rows = data.to_vec();
        if predictor >= 10 {
            for (i, b) in rows.iter_mut().enumerate() {
                if i % (columns * colors * bpc / 8 + 1) == 0 {
                    *b %= 6;
                }
            }
        }
        (params, rows)
    };
    match rng.below(9) {
        0..=2 => {
            let (params, rows) = predicted(rng, data);
            let encoded = match rng.below(4) {
                0 => deflate(&rows, 1 + rng.below(9) as u32),
                1 => common::stored_zlib(&rows, 1 + rng.below(70_000)),
                _ => zlib(&rows, rng.below(10) as u32),
            };
            ("/FlateDecode".into(), params, encoded)
        }
        3 | 4 => {
            let (params, rows) = predicted(rng, data);
            let early = rng.chance(70);
            let params = match params.strip_suffix(" >>") {
                Some(p) => format!("{p} /EarlyChange {} >>", u8::from(early)),
                None => format!("<< /EarlyChange {} >>", u8::from(early)),
            };
            ("/LZWDecode".into(), params, lzw(&rows, early, rng))
        }
        5 => ("/AHx".into(), "null".into(), ascii_hex(data, rng)),
        6 => ("/A85".into(), "null".into(), ascii_85(data, rng)),
        7 => (
            "/RunLengthDecode".into(),
            "null".into(),
            run_length(data, rng),
        ),
        _ => ("/Unknown".into(), "null".into(), data.to_vec()),
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
    for case in 0..1500 {
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
                    .any(|p| p.contains("/Predictor 2 /Colors 1 /BitsPerComponent 4")),
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
                peak < 1 << 20,
                "{name}: {size} bytes read with a peak of {peak}"
            );
            assert!(
                (size..8 * size).contains(&work),
                "{name}: {size} bytes read with {work} bytes of work"
            );
            println!("{name}: {size} bytes: peak {peak}, work {work}");
            peak
        });
        // Within a decoder's buffer doubling its capacity.
        assert!(
            peaks[1] < peaks[0] + 128 * 1024,
            "{name}: peaks {peaks:?} grow with the data"
        );
    }
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
