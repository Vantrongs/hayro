//! A Group 3 line is pushed before the EOLs and data after it are read.

use crate::stop::{Bits, EOL, WHITE_8};
use crate::{Collector, G3_1D, G3_2D};
use hayro_ccitt::{DecodeSettings, DecoderContext, RowDecoder, Source, decode};
use std::cell::Cell;
use std::rc::Rc;

/// Data that counts the bytes read of it.
struct Counted {
    data: Vec<u8>,
    read: Cell<usize>,
}

impl Source for Counted {
    fn read_at(&self, pos: usize, out: &mut [u8]) -> usize {
        let held = self.data.get(pos..).unwrap_or_default();
        let n = held.len().min(out.len());
        out[..n].copy_from_slice(&held[..n]);
        self.read.set(self.read.get() + n);
        n
    }
}

/// A line of eight white pixels followed by `eols` EOLs, in 1D or 2D data (an EOL
/// there followed by a 1D tag bit, as an RTC has it), its first row read through a
/// source reads a few bytes, however many EOLs follow, whether the image declares
/// that row alone, two rows, or none. Read on, the rows and the end are those of
/// decoding the data whole, which reads every EOL.
#[test]
fn a_line_is_pushed_before_the_eols_after_it() {
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
                let mut settings = DecodeSettings::new(8, rows);
                settings.encoding = encoding;
                let source = Rc::new(Counted {
                    data: bits.out.clone(),
                    read: Cell::new(0),
                });
                let mut decoder = RowDecoder::from_source(source.clone(), settings);
                let mut by_rows = Collector::new(8);
                assert_eq!(decoder.next_row(&mut by_rows), Ok(true), "{case}");
                let read = source.read.get();
                assert!(read <= 16, "{case}: {read} bytes read for the first row");
                let ended = loop {
                    match decoder.next_row(&mut by_rows) {
                        Ok(true) => {}
                        Ok(false) => break Ok(()),
                        Err(e) => break Err(e),
                    }
                };
                let mut whole = Collector::new(8);
                let consumed = decode(&bits.out, &mut whole, &mut DecoderContext::new(settings));
                assert_eq!(ended, consumed.map(|_| ()), "{case}");
                assert_eq!(consumed, Ok(bits.out.len()), "{case}");
                assert!(by_rows.rows == whole.rows, "{case}");
                assert_eq!(whole.rows, [vec![true; 8]], "{case}");
            }
        }
    }
}
