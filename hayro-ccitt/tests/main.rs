//! Each test decodes a raw CCITT stream from `streams` and compares the result
//! against an image in `references`.

mod codes;
mod dimensions;
mod edges;
mod g3_1d;
mod g3_2d;
mod g4;
mod modes;
mod recovery;
mod settings;
mod stop;
mod trailing;

use hayro_ccitt::{
    DecodeSettings, Decoder, DecoderContext, EncodingMode, RowDecoder, Source, decode,
};
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

const G3_1D: EncodingMode = EncodingMode::Group3_1D;
const G3_2D: EncodingMode = EncodingMode::Group3_2D;
const G4: EncodingMode = EncodingMode::Group4;

fn tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// Rows of pixels, where `true` means white.
type Bitmap = Vec<Vec<bool>>;

fn load_reference(name: &str) -> Bitmap {
    let path = tests_dir().join("references").join(format!("{name}.png"));
    let image = image::open(&path)
        .unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()))
        .to_luma8();

    image
        .rows()
        .map(|row| row.map(|p| p.0[0] > 127).collect())
        .collect()
}

struct Collector {
    columns: u32,
    rows: Bitmap,
}

impl Collector {
    fn new(columns: u32) -> Self {
        Self {
            columns,
            rows: Vec::new(),
        }
    }
}

impl Decoder for Collector {
    fn push_row(&mut self, changes: &[u32]) {
        assert!(changes.windows(2).all(|w| w[0] < w[1]));
        assert!(changes.iter().all(|&c| c < self.columns));

        let row = (0..self.columns)
            .map(|x| changes.partition_point(|&c| c <= x) % 2 == 0)
            .collect();
        self.rows.push(row);
    }
}

/// Decode `stream` and check that it matches `reference` and that the whole
/// stream was consumed.
fn check(stream: &str, reference: &str, configure: impl FnOnce(&mut DecodeSettings)) {
    check_consumed(stream, reference, None, configure);
}

fn check_consumed(
    stream: &str,
    reference: &str,
    consumed: Option<usize>,
    configure: impl FnOnce(&mut DecodeSettings),
) {
    let path = tests_dir().join("streams").join(format!("{stream}.bin"));
    let data =
        std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let reference = load_reference(reference);

    let mut settings = DecodeSettings::new(reference[0].len() as u32, reference.len() as u32);
    settings.end_of_block = false;
    settings.encoding = G4;
    configure(&mut settings);

    let mut collector = Collector::new(settings.columns);
    let mut ctx = DecoderContext::new(settings);
    let read = decode(&data, &mut collector, &mut ctx)
        .unwrap_or_else(|e| panic!("{stream}: decoding failed: {e}"));
    decodes_the_same_by_rows(stream, &data, settings);

    let expected_rows = if settings.rows == 0 {
        reference.len()
    } else {
        reference.len().min(settings.rows as usize)
    };
    assert_eq!(
        collector.rows.len(),
        expected_rows,
        "{stream}: wrong number of rows"
    );

    for (y, (actual, expected)) in collector.rows.iter().zip(&reference).enumerate() {
        assert_eq!(
            actual.len(),
            expected.len(),
            "{stream}: row {y} has the wrong length"
        );

        if let Some(x) = actual.iter().zip(expected).position(|(a, e)| a != e) {
            panic!("{stream}: first mismatch at row {y}, column {x}");
        }
    }

    assert_eq!(
        read,
        consumed.unwrap_or(data.len()),
        "{stream}: wrong number of bytes consumed"
    );
}

/// Decode `stream`, which is malformed, and check that it doesn't panic.
fn check_malformed(
    stream: &str,
    columns: u32,
    rows: u32,
    configure: impl FnOnce(&mut DecodeSettings),
) {
    let path = tests_dir().join("streams").join(format!("{stream}.bin"));
    let data =
        std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));

    let mut settings = DecodeSettings::new(columns, rows);
    configure(&mut settings);

    let mut ctx = DecoderContext::new(settings);
    let _ = decode(&data, &mut Collector::new(columns), &mut ctx);
    decodes_the_same_by_rows(stream, &data, settings);
}

/// A [`RowDecoder`] asked for rows one at a time pushes the rows [`decode`] pushes,
/// and ends as it does.
fn decodes_the_same_by_rows(stream: &str, data: &[u8], settings: DecodeSettings) {
    let mut whole = Collector::new(settings.columns);
    let ended = decode(data, &mut whole, &mut DecoderContext::new(settings)).map(|_| ());
    let mut rows = RowDecoder::new(data, settings);
    let mut by_rows = Collector::new(settings.columns);
    // Where the decoder was before each row, and the rows pushed before it.
    let mut states = vec![(rows.state(), 0)];
    let ended_by_rows = loop {
        let result = rows.next_row(&mut by_rows);
        states.push((rows.state(), by_rows.rows.len()));
        match result {
            Ok(true) => {
                assert_eq!(rows.decoded_rows() as usize, by_rows.rows.len());
                let mut last = Collector::new(settings.columns);
                last.push_row(rows.last_row());
                assert!(
                    last.rows.last() == by_rows.rows.last(),
                    "{stream}: last row"
                );
            }
            Ok(false) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    assert_eq!(
        rows.next_row(&mut by_rows),
        Ok(false),
        "{stream}: rows after the end"
    );
    assert_eq!(ended_by_rows, ended, "{stream}: ends otherwise");
    assert!(by_rows.rows == whole.rows, "{stream}: other rows");

    // A decoder resumed from where one was goes on as that one did.
    let picked = [0, 1, states.len() / 2, states.len().saturating_sub(2)];
    for &(ref state, pushed) in picked.iter().filter_map(|&i| states.get(i)) {
        let mut resumed = RowDecoder::new(data, settings);
        resumed.resume(state);
        let mut rest = Collector::new(settings.columns);
        let ended_resumed = loop {
            match resumed.next_row(&mut rest) {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        assert_eq!(
            ended_resumed, ended,
            "{stream}: ends otherwise after {pushed} rows"
        );
        assert!(
            rest.rows == whole.rows[pushed.min(whole.rows.len())..],
            "{stream}: other rows after {pushed} rows"
        );
    }

    // Read through a source, which tells its end only as it is read there, no row's
    // decoding reads before where it started.
    let source = Rc::new(Floored {
        data: data.to_vec(),
        floor: Cell::new(0),
        below: Cell::new(None),
    });
    let mut rows = RowDecoder::from_source(source.clone(), settings);
    let mut by_source = Collector::new(settings.columns);
    let ended_by_source = loop {
        source.floor.set(rows.byte_position());
        match rows.next_row(&mut by_source) {
            Ok(true) => {}
            Ok(false) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    assert_eq!(
        ended_by_source, ended,
        "{stream}: ends otherwise from a source"
    );
    assert!(
        by_source.rows == whole.rows,
        "{stream}: other rows from a source"
    );
    assert_eq!(
        source.below.get(),
        None,
        "{stream}: read before a row's start"
    );
}

/// Data that notes the first byte read before `floor`.
struct Floored {
    data: Vec<u8>,
    floor: Cell<usize>,
    below: Cell<Option<(usize, usize)>>,
}

impl Source for Floored {
    fn read_at(&self, pos: usize, out: &mut [u8]) -> usize {
        if pos < self.floor.get() && self.below.get().is_none() {
            self.below.set(Some((pos, self.floor.get())));
        }
        let held = self.data.get(pos..).unwrap_or_default();
        let n = held.len().min(out.len());
        out[..n].copy_from_slice(&held[..n]);
        n
    }
}
