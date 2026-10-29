use crate::stop::Bits;
use crate::{G4, check, check_consumed, check_malformed};
use hayro_ccitt::{DecodeSettings, Decoder, DecoderContext, decode};

#[test]
fn plain() {
    check("g4", "base", |_| {});
}

#[test]
fn eofb() {
    check("g4_eofb", "base", |s| s.end_of_block = true);
}

#[test]
fn eofb_pad_bits() {
    check_consumed("g4_eofb_pad_bits", "base", Some(1086), |s| {
        s.end_of_block = true;
    });
}

#[test]
fn eofb_trailing_data() {
    check_consumed("g4_eofb_trailing_data", "base", Some(1086), |s| {
        s.end_of_block = true;
    });
}

#[test]
fn byte_align() {
    check("g4_byte_align", "base", |s| s.rows_are_byte_aligned = true);
}

#[test]
fn byte_align_eofb() {
    check("g4_byte_align_eofb", "base", |s| {
        s.rows_are_byte_aligned = true;
        s.end_of_block = true;
    });
}

#[test]
fn eol() {
    check("g4_eol", "base", |s| {
        s.end_of_line = true;
        s.end_of_block = true;
    });
}

#[test]
fn truncated() {
    check_malformed("malformed_g4_truncated", 8, 2, |s| s.encoding = G4);
}

/// A horizontal mode code whose white run passes thousands of changes of the line
/// above, which has a change at every pixel, then vertical mode codes that take the
/// next changes from that line: b1 is found past the run as a change at a time would
/// find it.
#[test]
fn a_run_past_many_changes_of_the_line_above() {
    const HORIZONTAL: &str = "001";
    const V0: &str = "1";
    const WHITE_1: &str = "000111";
    const BLACK_1: &str = "010";
    const WHITE_2560: &str = "000000011111";
    const WHITE_0: &str = "00110101";
    struct Rows(Vec<Vec<u32>>);
    impl Decoder for Rows {
        fn push_row(&mut self, changes: &[u32]) {
            self.0.push(changes.to_vec());
        }
    }
    for width in [2562_u32, 2570, 2 * 2560 + 2, 4 * 2560 + 100] {
        let mut bits = Bits::default();
        for _ in 0..width / 2 {
            bits.put(HORIZONTAL).put(WHITE_1).put(BLACK_1);
        }
        // White up to the last whole 2560, a black pixel, then as the line above,
        // the first code taking two pixels, each other one.
        let run = width - 2 - (width - 2) % 2560;
        bits.put(HORIZONTAL);
        for _ in 0..run / 2560 {
            bits.put(WHITE_2560);
        }
        bits.put(WHITE_0).put(BLACK_1);
        for _ in run + 2..width {
            bits.put(V0);
        }
        let mut settings = DecodeSettings::new(width, 2);
        settings.encoding = G4;
        let mut rows = Rows(Vec::new());
        decode(&bits.out, &mut rows, &mut DecoderContext::new(settings)).unwrap();
        let above: Vec<u32> = (1..width).collect();
        let below: Vec<u32> = [run, run + 1].into_iter().chain(run + 3..width).collect();
        assert_eq!(rows.0, [above, below], "{width} columns");
    }
}
