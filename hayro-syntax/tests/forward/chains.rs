//! Encoders of the filters a stream's data pass through in a chain, and the data and
//! damage the tests of the readers feed them.

use std::collections::BTreeMap;
use std::io::Write;

/// A seeded xorshift generator.
pub(crate) struct Rng(pub(crate) u64);

impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number in `0..n`.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub(crate) fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

pub(crate) fn zlib(data: &[u8], level: u32) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(level));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

pub(crate) fn deflate(data: &[u8], level: u32) -> Vec<u8> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// `data` LZW-encoded as `lzw::decode` reads it: codes most significant bit first,
/// widened one code early if `early`, the table cleared before it fills.
pub(crate) fn lzw(data: &[u8], early: bool, rng: &mut Rng) -> Vec<u8> {
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

pub(crate) fn ascii_hex(data: &[u8], rng: &mut Rng) -> Vec<u8> {
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

pub(crate) fn ascii_85(data: &[u8], rng: &mut Rng) -> Vec<u8> {
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

pub(crate) fn run_length(data: &[u8], rng: &mut Rng) -> Vec<u8> {
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
pub(crate) fn data(rng: &mut Rng) -> Vec<u8> {
    let len = match rng.below(10) {
        0 => 0,
        1..=5 => rng.below(2_000),
        6..=8 => rng.below(40_000),
        _ => 100_000 + rng.below(200_000),
    };
    match rng.below(3) {
        0 => super::noise(len, rng.next() as u32),
        1 => (0..len)
            .map(|i| b"0 0 m 10 20 l S q 1 0 0 1 5 5 cm Q\n"[(i * 7 + i / 300) % 35])
            .collect(),
        _ => (0..len).map(|i| ((i / (1 + i % 300)) % 7) as u8).collect(),
    }
}

/// Damage the encoded bytes may carry: cut short, followed by garbage, or with bytes
/// changed, the first or the last too.
pub(crate) fn damaged(mut encoded: Vec<u8>, rng: &mut Rng) -> Vec<u8> {
    match rng.below(12) {
        0 if !encoded.is_empty() => {
            let at = rng.below(encoded.len());
            encoded.truncate(at);
        }
        1 => encoded.extend(super::noise(1 + rng.below(40), rng.next() as u32)),
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
pub(crate) fn layer(data: &[u8], rng: &mut Rng) -> (String, String, Vec<u8>) {
    let predicted = |rng: &mut Rng, data: &[u8]| -> (String, Vec<u8>) {
        if rng.chance(60) {
            return ("null".into(), data.to_vec());
        }
        // Rows of any bytes: each predictor decodes them to something. Pixels of any
        // size up to 12 bytes; TIFF prediction of other than 8-bit components only
        // the whole decoder reverses.
        let bits = [1, 2, 4, 8, 16];
        let (predictor, colors, bpc) = match rng.below(8) {
            0 => (2, 1 + rng.below(6), 8),
            1 => (2, 1 + rng.below(3), bits[rng.below(5)]),
            _ => (10 + rng.below(6), 1 + rng.below(6), bits[rng.below(5)]),
        };
        let columns = 1 + rng.below(60);
        let params = format!(
            "<< /Predictor {predictor} /Colors {colors} /BitsPerComponent {bpc} /Columns {columns} >>"
        );
        let mut rows = data.to_vec();
        if predictor >= 10 {
            let row = (columns * colors * bpc).div_ceil(8) + 1;
            for (i, b) in rows.iter_mut().enumerate() {
                if i % row == 0 {
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
                1 => super::stored_zlib(&rows, 1 + rng.below(70_000)),
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
