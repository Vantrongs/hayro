use crate::{
    Collector, G3_1D, G3_2D, G4, check, decodes_the_same_by_rows, load_reference, tests_dir,
};
use hayro_ccitt::{DecodeSettings, DecoderContext, decode};

#[test]
fn corrupt_line_g3_1d() {
    check("g3_1d_corrupt_line", "g3_1d_corrupt_line", |s| {
        s.encoding = G3_1D;
        s.end_of_line = true;
    });
}

#[test]
fn truncated_line_g3_1d() {
    check("g3_1d_truncated_line", "g3_1d_truncated_line", |s| {
        s.encoding = G3_1D;
        s.end_of_line = true;
    });
}

#[test]
fn truncated_line_g3_2d() {
    check("g3_2d_truncated_line", "g3_2d_truncated_line", |s| {
        s.encoding = G3_2D;
        s.end_of_line = true;
    });
}

#[test]
fn eol_flag_without_eols() {
    check("g3_1d", "base", |s| {
        s.encoding = G3_1D;
        s.end_of_line = true;
    });
}

#[test]
fn run_past_line_end_g3_1d() {
    check("g3_1d_run_past_line_end", "g3_1d_run_past_line_end", |s| {
        s.encoding = G3_1D;
    });
}

/// Data that ends within a row: the rows before it are decoded, then the row it ends
/// within, the pixels decoded of it kept and the rest white (the page's row 79, cut
/// where its 144th pixel starts a black run), and decoding ends without an error,
/// by rows as whole. A row of which no pixel was decoded is not pushed. Strict
/// decoding fails.
#[test]
fn data_that_ends_within_a_row() {
    let data = std::fs::read(tests_dir().join("streams").join("g4.bin")).unwrap();
    let reference = load_reference("base");
    let mut settings = DecodeSettings::new(211, 96);
    settings.end_of_block = false;
    settings.encoding = G4;
    let decoded = |cut: usize, settings: DecodeSettings| {
        let mut rows = Collector::new(211);
        let ended = decode(&data[..cut], &mut rows, &mut DecoderContext::new(settings));
        decodes_the_same_by_rows("g4", &data[..cut], settings);
        (ended, rows.rows)
    };
    let (ended, rows) = decoded(541, settings);
    assert_eq!(ended, Ok(541));
    assert_eq!(rows.len(), 80);
    assert!(rows[..79] == reference[..79]);
    assert!(rows[79][..143] == reference[79][..143]);
    assert!(!reference[79][143] && rows[79][143..].iter().all(|&white| white));
    // The data ends within the first code of row 2.
    let (ended, rows) = decoded(5, settings);
    assert_eq!(ended, Ok(5));
    assert!(rows == reference[..2]);
    settings.strict = true;
    assert!(decoded(541, settings).0.is_err());
}
