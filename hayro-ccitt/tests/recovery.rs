use crate::{Collector, G3_1D, G3_2D, check, load_reference, tests_dir};
use hayro_ccitt::{DecodeSettings, decode};

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

#[test]
fn data_that_ends_within_a_row() {
    let data = std::fs::read(tests_dir().join("streams").join("g4.bin")).unwrap();
    let reference = load_reference("base");
    let mut settings = crate::settings_for(211, 96);
    let decoded = |cut: usize, settings: &DecodeSettings| {
        let mut rows = Collector::new(211);
        let ended = decode(&data[..cut], &mut rows, settings);
        (ended, rows.rows)
    };
    let (ended, rows) = decoded(541, &settings);
    assert_eq!(ended, Ok(541));
    assert_eq!(rows.len(), 80);
    assert!(rows[..79] == reference[..79]);
    assert!(rows[79][..143] == reference[79][..143]);
    assert!(!reference[79][143] && rows[79][143..].iter().all(|&white| white));
    // The data ends within the first code of row 2.
    let (ended, rows) = decoded(5, &settings);
    assert_eq!(ended, Ok(5));
    assert!(rows == reference[..2]);
    settings.strict = true;
    assert!(decoded(541, &settings).0.is_err());
}

#[test]
fn a_cut_code_that_completes_a_row_fails_strict_decoding() {
    let data = [0x75];
    for strict in [false, true] {
        let mut settings = crate::settings_for(3, 2);
        settings.encoding = G3_1D;
        settings.strict = strict;
        let mut rows = Collector::new(3);
        let ended = decode(&data, &mut rows, &settings);
        if strict {
            assert!(ended.is_err(), "{ended:?}");
        } else {
            assert_eq!(ended, Ok(1));
            assert!(rows.rows == [vec![true, true, false], vec![true; 3]]);
        }
    }
}

#[test]
fn truncated_line_g4() {
    check("g4_truncated_line", "g4_truncated_line", |_| {});
}
