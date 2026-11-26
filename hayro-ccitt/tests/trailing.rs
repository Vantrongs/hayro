use crate::{Bits, Collector, G3_1D, G3_2D, settings_for};
use hayro_ccitt::decode;

#[test]
fn a_row_followed_by_trailing_eols() {
    const EOL: &str = "000000000001";
    const WHITE_8: &str = "10011";
    for (encoding, line, eol) in [
        (G3_1D, WHITE_8.to_string(), EOL.to_string()),
        (G3_2D, format!("1{WHITE_8}"), format!("{EOL}1")),
    ] {
        for rows in [1, 2, 0] {
            for eols in [6, 1_000, 100_000] {
                let case = format!("{encoding:?}, {rows} rows, {eols} EOLs");
                let mut bits = Bits::default();
                bits.put(&line);
                for _ in 0..eols {
                    bits.put(&eol);
                }
                let mut settings = settings_for(8, rows);
                settings.encoding = encoding;
                let mut whole = Collector::new(8);
                settings.end_of_block = true;
                let consumed = decode(&bits.out, &mut whole, &settings);
                assert_eq!(consumed, Ok(bits.out.len()), "{case}");
                assert_eq!(whole.rows, [vec![true; 8]], "{case}");
            }
        }
    }
}
