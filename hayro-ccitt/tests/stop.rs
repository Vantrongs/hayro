//! `Decoder::stop` is asked within a bounded number of bits whatever the data,
//! and told every bit read.

use crate::{G3_1D, G4};
use hayro_ccitt::{
    DecodeError, DecodeSettings, Decoder, DecoderContext, EncodingMode, POLL_BITS, decode,
};

/// The most bits read between two checks for an ask: a mode code and two run
/// codes, or a word of fill.
const MAX_BETWEEN_CHECKS: usize = 32;

/// Bits written most significant first, as CCITT packs them.
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    len: usize,
}

impl Bits {
    fn put(&mut self, code: &str) -> &mut Self {
        for bit in code.bytes() {
            if self.len.is_multiple_of(8) {
                self.out.push(0);
            }
            if bit == b'1' {
                *self.out.last_mut().unwrap() |= 0x80 >> (self.len % 8);
            }
            self.len += 1;
        }
        self
    }

    fn zero_bytes(&mut self, count: usize) -> &mut Self {
        assert!(self.len.is_multiple_of(8));
        self.out.extend(vec![0; count]);
        self.len += 8 * count;
        self
    }

    fn ones(&mut self, count: usize) -> &mut Self {
        assert!(self.len.is_multiple_of(8));
        self.out.extend(vec![0xFF; count]);
        self.len += 8 * count;
        self
    }
}

const EOL: &str = "000000000001";
/// Horizontal mode with a white and a black run of 0: codes that write nothing.
const EMPTY_HORIZONTAL: &str = "001_00110101_0000110111";
const V0: &str = "1";
const WHITE_8: &str = "10011";

/// Streams whose bits are all read once, with the settings they decode with:
/// codes that write nothing, a long fill before an EOL, a line followed by bits
/// that are skipped up to the next EOL, and many one-bit lines.
fn streams() -> Vec<(&'static str, Vec<u8>, DecodeSettings)> {
    let settings = |encoding: EncodingMode, rows: u32| {
        let mut settings = DecodeSettings::new(8, rows);
        settings.encoding = encoding;
        settings
    };

    let mut empty_codes = Bits::default();
    for _ in 0..100_000 {
        empty_codes.put(&EMPTY_HORIZONTAL.replace('_', ""));
    }
    empty_codes.put(V0).put(EOL).put(EOL);

    let mut fill = Bits::default();
    fill.zero_bytes(100_000).put("1").put(WHITE_8);

    let mut skipped = Bits::default();
    skipped.put(EOL).put(WHITE_8);
    skipped.put(&"1".repeat(8 - skipped.len % 8));
    skipped.ones(100_000).put(EOL).put(WHITE_8);

    let mut lines = Bits::default();
    lines.ones(100_000);

    vec![
        ("codes writing nothing", empty_codes.out, settings(G4, 1)),
        ("long fill", fill.out, settings(G3_1D, 1)),
        ("skipped bits", skipped.out, settings(G3_1D, 2)),
        ("one-bit lines", lines.out, settings(G4, 0)),
    ]
}

/// Records each ask, and asks to stop at the `stop_at`-th.
struct Asks {
    bits: Vec<usize>,
    rows: usize,
    stop_at: usize,
}

impl Asks {
    fn new(stop_at: usize) -> Self {
        Self {
            bits: Vec::new(),
            rows: 0,
            stop_at,
        }
    }
}

impl Decoder for Asks {
    fn push_row(&mut self, _: &[u32]) {
        self.rows += 1;
    }

    fn stop(&mut self, bits: usize) -> bool {
        self.bits.push(bits);
        self.bits.len() >= self.stop_at
    }
}

#[test]
fn asks_are_told_every_bit_within_a_bounded_number() {
    for (name, data, settings) in streams() {
        let mut asks = Asks::new(usize::MAX);
        let read = decode(&data, &mut asks, &mut DecoderContext::new(settings))
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        assert_eq!(read, data.len(), "{name}");
        assert!(asks.rows >= 1, "{name}");
        assert_eq!(asks.bits.iter().sum::<usize>(), 8 * read, "{name}");
        let most = asks.bits.iter().max().unwrap();
        assert!(*most <= POLL_BITS + MAX_BETWEEN_CHECKS, "{name}: {most}");
        assert!(
            asks.bits.len() > 8 * read / (POLL_BITS + MAX_BETWEEN_CHECKS),
            "{name}"
        );
    }
}

#[test]
fn decoding_stops_at_the_first_ask_that_says_so() {
    for (name, data, settings) in streams() {
        let mut asks = Asks::new(2);
        let result = decode(&data, &mut asks, &mut DecoderContext::new(settings));

        assert_eq!(result, Err(DecodeError::Stopped), "{name}");
        assert_eq!(asks.bits.len(), 2, "{name}");
        let read = asks.bits.iter().sum::<usize>();
        assert!(
            read <= 2 * (POLL_BITS + MAX_BETWEEN_CHECKS),
            "{name}: {read}"
        );
        assert!(asks.rows <= read, "{name}: {} rows", asks.rows);
    }
}
