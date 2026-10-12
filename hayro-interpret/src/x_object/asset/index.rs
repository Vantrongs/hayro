//! Decoder states saved along an image's rows, so that a request far into a large
//! image resumes from the nearest one before its first row instead of decoding every
//! row from the start.

use super::Stop;
use super::rows::RawRows;
use hayro_syntax::object::stream::{DecodedCheckpoint, DecodedReader, OwnedStream};
use std::sync::Mutex;

/// The most bytes one asset's saved states take, the index's own included.
pub(super) const CHECKPOINT_BUDGET: usize = 2 * 1024 * 1024;
/// The fewest stored bytes between two saved states.
pub(super) const MIN_CHECKPOINT_BYTES: usize = 256 * 1024;

/// An asset's saved decoder states.
#[derive(Default)]
pub(super) struct Checkpoints {
    flate: Mutex<FlateIndex>,
}

/// Flate decoder states, by the row they resume at.
#[derive(Default)]
struct FlateIndex {
    /// Advanced when a reader restarts: readers of earlier generations decode the
    /// data another way, and their states are not saved.
    generation: u64,
    /// Sorted by row; its capacity is the plan's slots once a state is saved.
    entries: Vec<Entry>,
}

struct Entry {
    row: u32,
    state: DecodedCheckpoint,
}

impl Checkpoints {
    /// The bytes the saved states take: the index, its slots, and each state's heap.
    pub(super) fn bytes(&self) -> Result<usize, Stop> {
        let index = self.flate.lock().map_err(|_| Stop::Failed)?;
        Ok(size_of::<Self>()
            + index.entries.capacity() * size_of::<Entry>()
            + index
                .entries
                .iter()
                .map(|e| e.state.heap_bytes())
                .sum::<usize>())
    }

    /// A checkpointer for a pass whose stored rows `raw` reads, `height` rows of
    /// `row_bytes`, from `stream`; and the row `raw` continues at, resumed from the
    /// state nearest at or before row `before`. `None` when the reader cannot be
    /// checkpointed or not one state fits the budget.
    pub(super) fn checkpointer<'c>(
        &'c self,
        stream: &'c OwnedStream,
        raw: &mut RawRows<'_>,
        row_bytes: usize,
        height: u32,
        before: u32,
    ) -> Result<Option<(Checkpointer<'c>, u32)>, Stop> {
        let Some(plan) = raw
            .reader
            .checkpoint_heap_bound()
            .and_then(|bound| Plan::new(bound, row_bytes, height))
        else {
            return Ok(None);
        };
        let index = self.flate.lock().map_err(|_| Stop::Failed)?;
        let start = match index.entries.iter().rev().find(|e| e.row <= before) {
            Some(e) if raw.resume(e.row, &e.state) => e.row,
            Some(_) => return Err(Stop::Failed),
            None => 0,
        };
        let checkpointer = Checkpointer {
            index: &self.flate,
            stream,
            plan,
            generation: index.generation,
        };
        Ok(Some((checkpointer, start)))
    }
}

/// Where states are saved: every `interval` rows, `slots` of them at most.
#[derive(Clone, Copy)]
struct Plan {
    interval: u32,
    slots: usize,
}

impl Plan {
    /// The spacing for `height` rows of `row_bytes` stored bytes whose states hold
    /// `heap_bound` heap bytes at most: as many states as the budget holds, spread over
    /// the rows; `None` when not one fits.
    fn new(heap_bound: usize, row_bytes: usize, height: u32) -> Option<Self> {
        let count = (CHECKPOINT_BUDGET - size_of::<Checkpoints>())
            / size_of::<Entry>().checked_add(heap_bound)?;
        let interval = height
            .div_ceil(u32::try_from(count).ok().filter(|&c| c > 0)?)
            .max(MIN_CHECKPOINT_BYTES.div_ceil(row_bytes) as u32);
        let slots = (height / interval) as usize;
        (slots > 0).then_some(Self { interval, slots })
    }
}

/// One pass's use of the index: it saves states every so many rows, as long as no
/// reader restarted since it began.
pub(super) struct Checkpointer<'c> {
    index: &'c Mutex<FlateIndex>,
    stream: &'c OwnedStream,
    plan: Plan,
    generation: u64,
}

impl Checkpointer<'_> {
    /// Saves `reader`'s state as resuming at `row`, if `row` is due for one and the
    /// index has room for it.
    pub(super) fn after_row(&self, row: u32, reader: &DecodedReader<'_>) -> Result<(), Stop> {
        if !row.is_multiple_of(self.plan.interval) {
            return Ok(());
        }
        let mut index = self.index.lock().map_err(|_| Stop::Failed)?;
        if index.generation != self.generation || index.entries.len() == self.plan.slots {
            return Ok(());
        }
        let Err(at) = index.entries.binary_search_by_key(&row, |e| e.row) else {
            return Ok(());
        };
        let Some(state) = reader.checkpoint(self.stream) else {
            return Ok(());
        };
        if index.entries.capacity() == 0 {
            index.entries.reserve_exact(self.plan.slots);
        }
        index.entries.insert(at, Entry { row, state });
        Ok(())
    }

    /// Drops the saved states after a reader restarted: they decode the data the way
    /// that failed.
    pub(super) fn restarted(&mut self) -> Result<(), Stop> {
        let mut index = self.index.lock().map_err(|_| Stop::Failed)?;
        index.entries.clear();
        index.generation += 1;
        self.generation = index.generation;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{asset, image, pdf, request, whole, zlib};
    use super::super::{DecodeWork, ImageAsset, ImageRequest, ImageSource, Plane, RequestError};
    use super::*;
    use crate::fixtures::{noise, stored_deflate};
    use hayro_syntax::Pdf;
    use hayro_syntax::work::Meter;
    use std::cell::Cell;
    use std::ops::Range;
    use std::sync::Arc;

    const W: u32 = 257;
    const H: u32 = 4096;
    /// The rows between saved states of a `W`-wide gray image.
    const INTERVAL: u32 = MIN_CHECKPOINT_BYTES.div_ceil(W as usize) as u32;
    /// PNG-predicted rows of 263 samples, which straddle the image's rows.
    const PNG: &str = "/DecodeParms << /Predictor 15 /Columns 263 >>";

    /// A `W` × `H` gray Flate image with `params`, encoded as `encoded`.
    fn gray(params: &str, encoded: Vec<u8>) -> Pdf {
        let dict =
            format!("/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode {params}");
        pdf(&[(image(&dict, W, H), encoded)])
    }

    /// Noise as rows of 263 samples, PNG-predicted with filters 0 to 4.
    fn png_rows(seed: u32) -> Vec<u8> {
        let mut data = noise((W * H).div_ceil(263) as usize * 264, seed);
        for (i, row) in data.chunks_mut(264).enumerate() {
            row[0] = (i % 5) as u8;
        }
        data
    }

    /// The image plane's rows `rows` at full size, and the work reading them took.
    fn rows(source: &mut ImageSource<'_>, rows: Range<u32>) -> (Vec<u8>, DecodeWork) {
        let before = source.work();
        let plane = source.layout().image;
        let window = [0, rows.start, plane.width, rows.end];
        let data = request(source, Plane::Image, (plane.width, plane.height), window).data;
        let after = source.work();
        let work = DecodeWork {
            rows_decoded: after.rows_decoded - before.rows_decoded,
            bytes_inflated: after.bytes_inflated - before.bytes_inflated,
        };
        (data, work)
    }

    fn saved(asset: &ImageAsset) -> usize {
        asset.0.checkpoints.flate.lock().unwrap().entries.len()
    }

    /// Reopened, an image resumes from the state nearest before a request's rows, for
    /// zlib, raw deflate (saved after its restart) and predicted rows, and returns
    /// what decoding it whole does.
    #[test]
    fn a_request_resumes_from_a_state_an_earlier_request_saved() {
        let data = noise((W * H) as usize, 71);
        for (params, encoded) in [
            ("", zlib(&data)),
            ("", stored_deflate(&data, 1000)),
            (PNG, zlib(&png_rows(71))),
        ] {
            let pdf = gray(params, encoded);
            let asset = asset(&pdf);
            let (_, cold) = rows(&mut asset.open(None, None).unwrap(), 3000..3016);
            let mut warm = asset.open(None, None).unwrap();
            let (_, resumed) = rows(&mut warm, 2800..2816);
            assert!(
                resumed.rows_decoded < u64::from(INTERVAL + 16),
                "{resumed:?}"
            );
            if params.is_empty() {
                assert_eq!(cold.bytes_inflated, u64::from(3016 * W));
                assert_eq!(resumed.bytes_inflated, resumed.rows_decoded * u64::from(W));
            }
            let mut decoded = whole(&asset);
            for (grid, window) in [
                ((W, H), [0, 2800, W, 2816]),
                ((W, H), [3, 3077, 200, 3091]),
                ((31, 511), [1, 222, 27, 233]),
                ((W, H), [0, 21, W, 41]),
            ] {
                let a = request(&mut warm, Plane::Image, grid, window);
                let b = request(&mut decoded, Plane::Image, grid, window);
                assert_eq!(
                    (a.format, a.data),
                    (b.format, b.data),
                    "{params} {window:?}"
                );
            }
            assert!(asset.checkpoint_bytes().unwrap() <= CHECKPOINT_BUDGET);
            drop((warm, decoded));
            let weak = Arc::downgrade(&asset.0);
            drop(asset);
            assert!(
                weak.upgrade().is_none(),
                "a saved state keeps its asset alive"
            );
        }
    }

    /// Predictor rows count toward the budget: the states of a predictor narrower
    /// than the budget fill their slots within it; one wider is not saved.
    #[test]
    fn predictor_rows_count_toward_the_budget() {
        let (w, h) = (1024_u32, 3000_u32);
        for (columns, fits) in [(131_071, true), (1_100_003, false)] {
            let predicted = vec![0; (w * h).div_ceil(columns) as usize * (columns as usize + 1)];
            let dict = format!(
                "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode \
                 /DecodeParms << /Predictor 15 /Columns {columns} >>"
            );
            let pdf = pdf(&[(image(&dict, w, h), zlib(&predicted))]);
            let asset = asset(&pdf);
            for pass in 0..2 {
                let (data, work) = rows(&mut asset.open(None, None).unwrap(), h - 2..h);
                assert_eq!(data, vec![0; 2 * w as usize]);
                assert_eq!(pass == 1 && fits, work.rows_decoded < u64::from(h / 2));
                assert!(asset.checkpoint_bytes().unwrap() <= CHECKPOINT_BUDGET);
            }
            let index = asset.0.checkpoints.flate.lock().unwrap();
            let full = index.entries.len() == index.entries.capacity();
            assert!(
                full && index.entries.is_empty() != fits,
                "{columns} columns"
            );
        }
    }

    /// A cancel hook may read the asset's checkpoint bytes, and sources of one asset
    /// read at once.
    #[test]
    fn the_index_is_not_locked_while_rows_are_read() {
        let data = noise((W * H) as usize, 71);
        let pdf = gray("", zlib(&data));
        let asset = asset(&pdf);
        let mut source = asset.open(None, None).unwrap();
        rows(&mut source, 3000..3016);
        // Polled as the request starts, and again as the rows after the saved state
        // it resumed at are read.
        let polls = Cell::new(0);
        let cancel = || {
            polls.set(polls.get() + 1);
            asset.checkpoint_bytes().is_ok() && polls.get() == 2
        };
        let request = ImageRequest {
            plane: Plane::Image,
            grid: (W, H),
            window: [0, INTERVAL, W, INTERVAL + 600],
            cancel: Some(&cancel),
        };
        assert_eq!(
            source.request(&request).unwrap_err(),
            RequestError::Cancelled
        );
        assert_eq!(polls.get(), 2);
        std::thread::scope(|scope| {
            let read = |at: u32| {
                let asset = &asset;
                scope.spawn(move || rows(&mut asset.open(None, None).unwrap(), at..at + 16).0)
            };
            let (a, b) = (read(2800), read(3100));
            let at = |row: u32| row as usize * W as usize;
            assert_eq!(a.join().unwrap(), data[at(2800)..at(2816)]);
            assert_eq!(b.join().unwrap(), data[at(3100)..at(3116)]);
        });
    }

    /// After one reader restarted, a reader that began before the restart saves
    /// nothing; one that began after it does.
    #[test]
    fn a_reader_of_a_dropped_generation_saves_nothing() {
        let pdf = gray("", zlib(&noise((W * H) as usize, 71)));
        let asset = asset(&pdf);
        let obj = asset.xobject().unwrap();
        let (checkpoints, stream) = (&asset.0.checkpoints, &asset.0.stream);
        let meter = Meter::default();
        let mut raw = RawRows::new(&obj.stream, W as usize, &meter).unwrap();
        let checkpointer = |raw: &mut RawRows<'_>| {
            let (c, start) = checkpoints
                .checkpointer(stream, raw, W as usize, H, 0)
                .unwrap()
                .unwrap();
            assert_eq!(start, 0);
            c
        };
        let earlier = checkpointer(&mut raw);
        let mut later = checkpointer(&mut raw);
        later.restarted().unwrap();
        for _ in 0..INTERVAL {
            raw.skip_row().unwrap();
        }
        earlier.after_row(INTERVAL, &raw.reader).unwrap();
        assert_eq!(saved(&asset), 0);
        later.after_row(INTERVAL, &raw.reader).unwrap();
        assert_eq!(saved(&asset), 1);
    }

    /// A restart after states were saved (at a checksum that fails at the end) drops
    /// them, and the request returns what decoding the image whole does.
    #[test]
    fn a_late_restart_drops_the_saved_states() {
        for (params, data) in [("", noise((W * H) as usize, 31)), (PNG, png_rows(31))] {
            let mut encoded = zlib(&data);
            *encoded.last_mut().unwrap() ^= 1;
            let pdf = gray(params, encoded);
            let asset = asset(&pdf);
            let mut source = asset.open(None, None).unwrap();
            rows(&mut source, 2800..2816);
            assert!(saved(&asset) > 0);
            let (actual, _) = rows(&mut source, 4000..H);
            let (expected, _) = rows(&mut whole(&asset), 4000..H);
            assert_eq!(actual, expected, "{params}");
            assert_eq!(saved(&asset), 0);
        }
    }
}
