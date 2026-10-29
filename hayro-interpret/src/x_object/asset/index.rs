//! Decoder states saved along an image's rows, so that a request far into a large
//! image resumes from the nearest one before its first row instead of decoding every
//! row from the start.

use super::jpeg::JpegIndex;
use super::rows::MaskRows;
use super::{Plane, Stop};
use hayro_syntax::object::stream::{DecodedCheckpoint, DecodedReader, OwnedStream};
use std::sync::Mutex;

/// The most bytes one asset's saved states take, the index's own included.
pub(super) const CHECKPOINT_BUDGET: usize = 2 * 1024 * 1024;
/// The fewest stored bytes between two saved states.
pub(super) const MIN_CHECKPOINT_BYTES: usize = 256 * 1024;

/// An asset's saved decoder states.
#[derive(Default)]
pub(super) struct Checkpoints {
    /// Of the colour (or a stencil mask's alpha), with a mask read alongside it.
    rows: Mutex<RowIndex>,
    /// Of a mask on its own grid.
    mask: Mutex<RowIndex>,
    pub(super) jpeg: Mutex<JpegIndex>,
}

/// Decoder states of a plane's rows, by the row they resume at.
#[derive(Default)]
struct RowIndex {
    /// Sorted by row; its capacity is the plan's slots once a state is saved.
    entries: Vec<Entry>,
}

struct Entry {
    row: u32,
    state: DecodedCheckpoint,
    /// The state of a mask read alongside, where its reader needs one; boxed, as most
    /// entries have none.
    mask: Option<Box<DecodedCheckpoint>>,
}

impl RowIndex {
    fn bytes(&self) -> usize {
        self.entries.capacity() * size_of::<Entry>()
            + self
                .entries
                .iter()
                .map(|e| {
                    let mask = e
                        .mask
                        .as_ref()
                        .map_or(0, |m| size_of::<DecodedCheckpoint>() + m.heap_bytes());
                    e.state.heap_bytes() + mask
                })
                .sum::<usize>()
    }
}

impl Checkpoints {
    /// The bytes the saved states take: the index, its slots, and each state's heap.
    pub(super) fn bytes(&self) -> Result<usize, Stop> {
        let rows = self.rows.lock().map_err(|_| Stop::Failed)?;
        let mask = self.mask.lock().map_err(|_| Stop::Failed)?;
        let jpeg = self.jpeg.lock().map_err(|_| Stop::Failed)?;
        Ok(size_of::<Self>() + rows.bytes() + mask.bytes() + jpeg.heap_bytes())
    }

    /// A checkpointer for a pass over `plane`, whose readers' states hold `bound` heap
    /// bytes at most, in `budget` bytes, `height` rows of `row_bytes` stored bytes, read
    /// from `stream` and a mask's `mask_stream` read alongside; and the row the pass
    /// continues at, `resume`d from the state nearest at or before row `before`.
    /// `None` when the readers cannot be checkpointed or not one state fits the budget.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn checkpointer<'c>(
        &'c self,
        plane: Plane,
        stream: OwnedStream,
        mask_stream: Option<OwnedStream>,
        bound: Option<usize>,
        (row_bytes, height, budget): (usize, u32, usize),
        before: u32,
        resume: impl FnOnce(u32, &DecodedCheckpoint, Option<&DecodedCheckpoint>) -> bool,
    ) -> Result<Option<(Checkpointer<'c>, u32)>, Stop> {
        let Some(plan) = bound.and_then(|bound| Plan::new(bound, row_bytes, height, budget)) else {
            return Ok(None);
        };
        let index = match plane {
            Plane::Image => &self.rows,
            Plane::Mask => &self.mask,
        };
        let entries = index.lock().map_err(|_| Stop::Failed)?;
        let start = match entries.entries.iter().rev().find(|e| e.row <= before) {
            Some(e) if resume(e.row, &e.state, e.mask.as_deref()) => e.row,
            Some(_) => return Err(Stop::Failed),
            None => 0,
        };
        let checkpointer = Checkpointer {
            index,
            stream,
            mask_stream,
            plan,
        };
        Ok(Some((checkpointer, start)))
    }
}

/// Where states are saved: every `interval` rows before the last, `slots` of them.
#[derive(Clone, Copy)]
struct Plan {
    interval: u32,
    slots: usize,
}

impl Plan {
    /// The spacing for `height` rows of `row_bytes` stored bytes whose states hold
    /// `heap_bound` heap bytes at most: as many states as `budget` holds, spread over
    /// the rows; `None` when not one fits. None is due at `height`, after the last
    /// row, where no request resumes.
    fn new(heap_bound: usize, row_bytes: usize, height: u32, budget: usize) -> Option<Self> {
        let count = budget.checked_sub(size_of::<Checkpoints>())?
            / size_of::<Entry>().checked_add(heap_bound)?;
        let interval = height
            .div_ceil(u32::try_from(count).ok().filter(|&c| c > 0)?)
            .max(MIN_CHECKPOINT_BYTES.div_ceil(row_bytes) as u32);
        let slots = (height.checked_sub(1)? / interval) as usize;
        (slots > 0).then_some(Self { interval, slots })
    }

    /// Whether a state is due at `row`.
    fn due(&self, row: u32) -> bool {
        row.is_multiple_of(self.interval)
            && (1..=self.slots).contains(&((row / self.interval) as usize))
    }
}

/// One pass's use of the index: it saves states every so many rows.
pub(super) struct Checkpointer<'c> {
    index: &'c Mutex<RowIndex>,
    stream: OwnedStream,
    mask_stream: Option<OwnedStream>,
    plan: Plan,
}

impl Checkpointer<'_> {
    /// The first row after `row` due a saved state.
    pub(super) fn next_due(&self, row: u32) -> u32 {
        (row / self.plan.interval + 1).saturating_mul(self.plan.interval)
    }

    /// Saves `reader`'s state, and that of a `mask` read alongside, as resuming at
    /// `row`, if `row` is due for one, the index has room for it and both can be saved.
    pub(super) fn after_row(
        &self,
        row: u32,
        reader: &DecodedReader<'_>,
        mask: Option<&MaskRows<'_>>,
    ) -> Result<(), Stop> {
        self.save(row, || {
            let state = reader.checkpoint(&self.stream)?;
            let mask = match mask {
                Some(m) => m.checkpoint(row, self.mask_stream.as_ref())?.map(Box::new),
                None => None,
            };
            Some((state, mask))
        })
    }

    /// Saves the state of `rows`, a mask on its own grid, as resuming at the row it
    /// reads next, if that is due for one, the index has room for it and the mask read
    /// every row before it whole.
    pub(super) fn after_mask_row(&self, rows: &MaskRows<'_>) -> Result<(), Stop> {
        let row = rows.row();
        self.save(row, || {
            Some((rows.checkpoint(row, Some(&self.stream))??, None))
        })
    }

    fn save(
        &self,
        row: u32,
        state: impl FnOnce() -> Option<(DecodedCheckpoint, Option<Box<DecodedCheckpoint>>)>,
    ) -> Result<(), Stop> {
        if !self.plan.due(row) {
            return Ok(());
        }
        let mut index = self.index.lock().map_err(|_| Stop::Failed)?;
        if index.entries.len() == self.plan.slots {
            return Ok(());
        }
        let Err(at) = index.entries.binary_search_by_key(&row, |e| e.row) else {
            return Ok(());
        };
        let Some((state, mask)) = state() else {
            return Ok(());
        };
        if index.entries.capacity() == 0 {
            index.entries.reserve_exact(self.plan.slots);
        }
        index.entries.insert(at, Entry { row, state, mask });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{asset, image, pdf, request, rows, whole, zlib};
    use super::super::{ImageAsset, ImageRequest, Plane, RequestError};
    use super::*;
    use crate::fixtures::{noise, stored_deflate};
    use hayro_syntax::Pdf;
    use hayro_syntax::work::POLL_BYTES;
    use std::cell::Cell;
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

    /// No state is due after the last row, where no request resumes: an image of
    /// one row saves none.
    #[test]
    fn no_state_is_due_after_the_last_row() {
        let plan = Plan::new(0, MIN_CHECKPOINT_BYTES, 4, CHECKPOINT_BUDGET).unwrap();
        assert_eq!((plan.interval, plan.slots), (1, 3));
        assert!((1..4).all(|row| plan.due(row)));
        assert!(!plan.due(0) && !plan.due(4));
        assert!(Plan::new(0, MIN_CHECKPOINT_BYTES, 1, CHECKPOINT_BUDGET).is_none());
    }

    fn saved(asset: &ImageAsset) -> usize {
        asset.0.checkpoints.rows.lock().unwrap().entries.len()
    }

    /// Reopened, an image resumes from the state nearest before a request's rows, for
    /// zlib, raw deflate and predicted rows, and returns what decoding it whole does.
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
                // A reader inflates up to `POLL_BYTES` ahead, which a saved state carries.
                let near = |inflated: u64, rows: u64| {
                    inflated.abs_diff(rows * u64::from(W)) <= POLL_BYTES as u64
                };
                assert!(near(cold.bytes_inflated, 3016), "{cold:?}");
                assert!(
                    near(resumed.bytes_inflated, resumed.rows_decoded),
                    "{resumed:?}"
                );
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
            let index = asset.0.checkpoints.rows.lock().unwrap();
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

    /// Zlib data whose checksum fails at the end keeps its zlib reading: the states
    /// saved before the end are kept, and a request returns what decoding the image
    /// whole does.
    #[test]
    fn a_checksum_that_fails_at_the_end_keeps_the_saved_states() {
        for (params, data) in [("", noise((W * H) as usize, 31)), (PNG, png_rows(31))] {
            let mut encoded = zlib(&data);
            *encoded.last_mut().unwrap() ^= 1;
            let pdf = gray(params, encoded);
            let asset = asset(&pdf);
            let mut source = asset.open(None, None).unwrap();
            let (_, cold) = rows(&mut source, 4000..H);
            let saved_before = saved(&asset);
            assert!(saved_before > 0);
            let (actual, warm) = rows(&mut source, 4000..H);
            let (expected, _) = rows(&mut whole(&asset), 4000..H);
            assert_eq!(actual, expected, "{params}");
            if params.is_empty() {
                assert_eq!(expected, data[4000 * W as usize..]);
            }
            assert_eq!(saved(&asset), saved_before);
            assert!(warm.rows_decoded < cold.rows_decoded, "{warm:?} {cold:?}");
        }
    }
}
