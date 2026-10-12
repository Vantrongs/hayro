//! Decoding a stream whole stops within a bounded amount of work of its meter's hook
//! asking to, whatever the filter and the data. The work is counted in debug builds.
#![cfg(debug_assertions)]

use hayro_syntax::Pdf;
use hayro_syntax::object::stream::{DecodeFailure, ImageDecodeParams};
use hayro_syntax::object::{ObjectIdentifier, Stream};
use hayro_syntax::work::{Meter, POLL_BYTES, TOLD};
use std::cell::Cell;

mod common;
use common::stored_zlib;

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

/// Bits written least significant first, as Deflate packs them.
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    bit: u32,
}

impl Bits {
    fn put(&mut self, value: u32, len: u32) {
        for i in 0..len {
            if self.bit == 0 {
                self.out.push(0);
            }
            *self.out.last_mut().unwrap() |= (((value >> i) & 1) as u8) << self.bit;
            self.bit = (self.bit + 1) % 8;
        }
    }

    /// A Huffman code, which Deflate packs most significant bit first.
    fn code(&mut self, code: u32, len: u32) {
        let reversed = (0..len).fold(0, |r, i| r | ((code >> i) & 1) << (len - 1 - i));
        self.put(reversed, len);
    }
}

/// Zlib data of `len` zero bytes (a multiple of 258 plus one) in one block of fixed
/// codes: a literal, then matches of 258 at distance 1, each 13 bits.
fn zlib_zeros(len: usize) -> Vec<u8> {
    let mut bits = Bits::default();
    bits.put(0b011, 3);
    bits.code(0b0011_0000, 8);
    for _ in 0..(len - 1) / 258 {
        bits.code(0b1100_0101, 8);
        bits.code(0, 5);
    }
    bits.code(0, 7);
    let mut out = vec![0x78, 0x01];
    out.extend(bits.out);
    out.extend(((len as u32 % 65521) << 16 | 1).to_be_bytes());
    out
}

/// Zlib data of `blocks` empty stored blocks and a last empty one.
fn zlib_empty_blocks(blocks: usize) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    for last in (0..=blocks).map(|i| i == blocks) {
        out.extend([u8::from(last), 0, 0, 0xff, 0xff]);
    }
    out.extend(1_u32.to_be_bytes());
    out
}

/// LZW data of `codes` clear-table codes, nine bits each.
fn lzw_clears(codes: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut held) = (0_u32, 0);
    for _ in 0..codes {
        acc = acc << 9 | 256;
        held += 9;
        while held >= 8 {
            held -= 8;
            out.push((acc >> held) as u8);
        }
        acc &= (1 << held) - 1;
    }
    out
}

/// Decoding stops at the meter's second poll, within three polls' worth of work, and
/// decoding again without a hook to stop it gives the whole result.
#[test]
fn whole_decoding_is_cancelled_within_bounded_work() {
    let rows = 4;
    let columns = 2_000_000;
    let mut predicted = Vec::new();
    for _ in 0..rows {
        predicted.push(4);
        predicted.extend(common::noise(columns, 9));
    }
    let cases: Vec<(&str, String, Vec<u8>)> = vec![
        (
            "hex digits",
            "/Filter /ASCIIHexDecode".into(),
            b"00".repeat(2 << 20),
        ),
        ("hex spaces", "/Filter /ASCIIHexDecode".into(), {
            let mut data = b" ".repeat(4 << 20);
            data.extend(b"00>");
            data
        }),
        (
            "base-85 zeros",
            "/Filter /ASCII85Decode".into(),
            b"z".repeat(1 << 20),
        ),
        ("base-85 spaces", "/Filter /ASCII85Decode".into(), {
            let mut data = b" ".repeat(4 << 20);
            data.extend(b"z~>");
            data
        }),
        ("run length", "/Filter /RunLengthDecode".into(), {
            let mut data = [129, 7].repeat(1 << 16);
            data.push(128);
            data
        }),
        (
            "LZW clears",
            "/Filter /LZWDecode".into(),
            lzw_clears(4 << 20),
        ),
        (
            "Flate output",
            "/Filter /FlateDecode".into(),
            zlib_zeros(258 * 65_000 + 1),
        ),
        (
            "Flate empty blocks",
            "/Filter /FlateDecode".into(),
            zlib_empty_blocks(800_000),
        ),
        (
            "Flate wide predictor rows",
            format!("/Filter /FlateDecode /DecodeParms << /Predictor 15 /Columns {columns} >>"),
            stored_zlib(&predicted, 65_535),
        ),
        (
            "CCITT white lines",
            "/Filter /CCITTFaxDecode /DecodeParms << /K -1 /Columns 100000 /Rows 400 >>".into(),
            vec![0xff; 50],
        ),
    ];
    for (name, dict, data) in cases {
        let pdf = stream_file(&format!("{dict} /Length {}", data.len()), &data);
        assert_cancelled(name, &object_3(&pdf));
    }
    let data = common::noise(4 << 20, 3);
    let pdf = Pdf::new(common::encrypted_stream_file("", &data, [7; 32], [7; 32])).unwrap();
    assert_cancelled("decryption", &object_3(&pdf));
}

fn assert_cancelled(name: &str, stream: &Stream<'_>) {
    let params = ImageDecodeParams {
        width: 100_000,
        height: 400,
        ..Default::default()
    };
    let polls = Cell::new(0);
    let hook = || {
        polls.set(polls.get() + 1);
        polls.get() > 1
    };
    TOLD.set(0);
    let result = stream.decoded_image_metered(&params, &Meter::new(Some(&hook)));
    assert!(
        matches!(result, Err(DecodeFailure::Cancelled)),
        "{name}: {:?}",
        result.map(|r| r.data.len())
    );
    assert_eq!(polls.get(), 2, "{name}");
    assert!(
        TOLD.get() < 3 * POLL_BYTES,
        "{name}: {} bytes of work",
        TOLD.get()
    );
    let whole = stream.decoded_image(&params);
    assert!(whole.is_ok(), "{name}: {:?}", whole.err());
}
