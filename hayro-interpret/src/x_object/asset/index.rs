//! Decoder states saved along an image's rows, so that a request far into a large
//! image resumes from the nearest one before its first row instead of decoding every
//! row from the start.

use super::jpeg::JpegIndex;
use super::rows::{MaskRows, RawRows};
use super::{Plane, Stop};
use hayro_syntax::object::stream::{DecodedCheckpoint, OwnedStream};
use hayro_syntax::work::Cancelled;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The most bytes one asset's saved states take, the index's own included.
pub(super) const CHECKPOINT_BUDGET: usize = 2 * 1024 * 1024;
/// The fewest stored bytes between two saved states.
pub(super) const MIN_CHECKPOINT_BYTES: usize = 256 * 1024;
/// The bytes a saved state takes besides its heap: it is shared, so that taking it
/// out of an index or putting it in copies nothing while the index is locked.
pub(super) const SHARED_STATE_BYTES: usize =
    size_of::<DecodedCheckpoint>() + 2 * size_of::<usize>();

/// An asset's saved decoder states.
#[derive(Default)]
pub(super) struct Checkpoints {
    /// Of the colour (or a stencil mask's alpha), with a mask read alongside it.
    rows: Locked<RowIndex>,
    /// Of a mask on its own grid.
    mask: Locked<RowIndex>,
    pub(super) jpeg: Locked<JpegIndex>,
}

/// The bytes an index holds on the heap.
pub(super) trait HeapBytes {
    fn heap_bytes(&self) -> usize;
}

/// An index shared by an asset's requests, which lock it only for steps that poll
/// no hook, and the bytes it holds, read without locking it: so a hook may read the
/// asset's statistics while one of its requests polls it, and no request waits on
/// another's decoding or copying.
#[derive(Default)]
pub(super) struct Locked<T> {
    index: Mutex<T>,
    bytes: AtomicUsize,
}

impl<T: HeapBytes> Locked<T> {
    /// What `step` makes of the index, locked meanwhile. A step is `Send`, so it
    /// holds no meter, whose counts are not `Sync`, nor a hook: no hook is polled
    /// while the index is locked. What it returns borrows nothing of the index.
    pub(super) fn with<R>(&self, step: impl FnOnce(&mut T) -> R + Send) -> Result<R, Stop> {
        let mut index = self.index.lock().map_err(|_| Stop::Failed)?;
        let made = step(&mut index);
        self.bytes.store(index.heap_bytes(), Ordering::Relaxed);
        Ok(made)
    }

    fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// Decoder states of a plane's rows, by the row they resume at.
#[derive(Default)]
struct RowIndex {
    /// Sorted by row; its capacity is the plan's slots once a state is saved.
    entries: Vec<Entry>,
}

struct Entry {
    row: u32,
    state: Arc<DecodedCheckpoint>,
    /// The state of a mask read alongside, where its reader needs one.
    mask: Option<Arc<DecodedCheckpoint>>,
}

impl HeapBytes for RowIndex {
    fn heap_bytes(&self) -> usize {
        let state = |s: &DecodedCheckpoint| SHARED_STATE_BYTES + s.heap_bytes();
        self.entries.capacity() * size_of::<Entry>()
            + self
                .entries
                .iter()
                .map(|e| state(&e.state) + e.mask.as_deref().map_or(0, state))
                .sum::<usize>()
    }
}

impl RowIndex {
    fn position(&self, row: u32) -> Result<usize, usize> {
        self.entries.binary_search_by_key(&row, |e| e.row)
    }
}

/// A state of the colour, and one of a mask read alongside, saved together.
type Pair = (DecodedCheckpoint, Option<DecodedCheckpoint>);

impl Checkpoints {
    /// The bytes the saved states take: the indexes, their slots, and each state's
    /// heap; read without locking an index.
    pub(super) fn bytes(&self) -> usize {
        size_of::<Self>() + self.rows.bytes() + self.mask.bytes() + self.jpeg.bytes()
    }

    /// A checkpointer for a pass over `plane`, whose readers' states hold `bound` heap
    /// bytes at most, in `budget` bytes, `height` rows of `row_bytes` stored bytes, read
    /// from `stream` and a mask's `mask_stream` read alongside; and the row the pass
    /// continues at, `resume`d from the state nearest at or before row `before`, which
    /// is taken out of the index, shared, before it is copied.
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
        resume: impl FnOnce(u32, &DecodedCheckpoint, Option<&DecodedCheckpoint>) -> Result<bool, Stop>,
    ) -> Result<Option<(Checkpointer<'c>, u32)>, Stop> {
        let Some(plan) = bound.and_then(|bound| Plan::new(bound, row_bytes, height, budget)) else {
            return Ok(None);
        };
        let index = match plane {
            Plane::Image => &self.rows,
            Plane::Mask => &self.mask,
        };
        let nearest = index.with(|index| {
            let e = index.entries.iter().rev().find(|e| e.row <= before)?;
            Some((e.row, e.state.clone(), e.mask.clone()))
        })?;
        let start = match nearest {
            Some((row, state, mask)) if resume(row, &state, mask.as_deref())? => row,
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
            / (size_of::<Entry>() + SHARED_STATE_BYTES).checked_add(heap_bound)?;
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
    index: &'c Locked<RowIndex>,
    stream: OwnedStream,
    mask_stream: Option<OwnedStream>,
    plan: Plan,
}

impl Checkpointer<'_> {
    /// The first row after `row` due a saved state.
    pub(super) fn next_due(&self, row: u32) -> u32 {
        (row / self.plan.interval + 1).saturating_mul(self.plan.interval)
    }

    /// Saves `raw`'s state, and that of a `mask` read alongside, as resuming at
    /// `row`, if `row` is due for one, the index has room for it and both can be saved.
    pub(super) fn after_row(
        &self,
        row: u32,
        raw: &RawRows<'_>,
        mask: Option<&MaskRows<'_>>,
    ) -> Result<(), Stop> {
        self.save(row, || {
            let Some(state) = raw.checkpoint(&self.stream)? else {
                return Ok(None);
            };
            let mask = match mask {
                Some(m) => match m.checkpoint(row, self.mask_stream.as_ref())? {
                    Some(mask) => mask,
                    None => return Ok(None),
                },
                None => None,
            };
            Ok(Some((state, mask)))
        })
    }

    /// Saves the state of `rows`, a mask on its own grid, as resuming at the row it
    /// reads next, if that is due for one, the index has room for it and the mask read
    /// every row before it whole.
    pub(super) fn after_mask_row(&self, rows: &MaskRows<'_>) -> Result<(), Stop> {
        let row = rows.row();
        self.save(row, || {
            Ok(match rows.checkpoint(row, Some(&self.stream))? {
                Some(Some(state)) => Some((state, None)),
                _ => None,
            })
        })
    }

    /// Saves the `state` made at `row`, if one is due there and the index has room
    /// for it. The state is copied before the index is locked to put it in; a pass
    /// that saved one there meanwhile keeps its own.
    fn save(
        &self,
        row: u32,
        state: impl FnOnce() -> Result<Option<Pair>, Cancelled>,
    ) -> Result<(), Stop> {
        if !self.plan.due(row) {
            return Ok(());
        }
        let slots = self.plan.slots;
        // Where a state at `row` goes, if the index has none there and room for it.
        let room = move |index: &RowIndex| {
            (index.entries.len() < slots)
                .then(|| index.position(row).err())
                .flatten()
        };
        if self.index.with(|index| room(index))?.is_none() {
            return Ok(());
        }
        let Some((state, mask)) = state()? else {
            return Ok(());
        };
        let entry = Entry {
            row,
            state: Arc::new(state),
            mask: mask.map(Arc::new),
        };
        // Dropped, where not put in, once the index is unlocked.
        let _unsaved = self.index.with(move |index| {
            let Some(at) = room(index) else {
                return Some(entry);
            };
            if index.entries.capacity() == 0 {
                index.entries.reserve_exact(slots);
            }
            index.entries.insert(at, entry);
            None
        })?;
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
        asset
            .0
            .checkpoints
            .rows
            .with(|index| index.entries.len())
            .unwrap()
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
            let (len, capacity) = asset
                .0
                .checkpoints
                .rows
                .with(|index| (index.entries.len(), index.entries.capacity()))
                .unwrap();
            assert!(len == capacity && (len == 0) != fits, "{columns} columns");
        }
    }

    /// A hook that reads the asset's statistics while a request saves a state, or
    /// resumes from one, copying a predictor row wider than a piece between its
    /// polls, does not wait on the index: both requests finish. After a request
    /// stopped while copying a state, the next returns what decoding the image whole
    /// does.
    #[cfg(debug_assertions)]
    #[test]
    fn a_hook_reading_statistics_while_a_state_is_copied_does_not_wait() {
        use hayro_syntax::work::STATE_COPIED;
        const COLUMNS: usize = 65_536;
        let rows_of = (W * H) as usize / COLUMNS + 1;
        let mut predicted = noise(rows_of * (COLUMNS + 1), 13);
        for row in predicted.chunks_mut(COLUMNS + 1) {
            row[0] %= 5;
        }
        let params = format!("/DecodeParms << /Predictor 15 /Columns {COLUMNS} >>");
        let pdf = gray(&params, zlib(&predicted));
        let window = [0, H - 2, W, H];
        let expected = request(&mut whole(&asset(&pdf)), Plane::Image, (W, H), window).data;
        // Stops once `stop_after` bytes of states were copied in the request; reads
        // the statistics at every poll.
        let read = |asset: &ImageAsset, stop_after: usize| {
            STATE_COPIED.set(0);
            let cancel = || {
                asset.checkpoint_bytes().unwrap();
                STATE_COPIED.get() > stop_after
            };
            let request = ImageRequest {
                plane: Plane::Image,
                grid: (W, H),
                window,
                cancel: Some(&cancel),
            };
            let got = asset.open(None, None).unwrap().request(&request);
            (got.map(|r| r.data), STATE_COPIED.get())
        };
        // The first request saves states; the second resumes from one.
        let asset = asset(&pdf);
        let (first, copied) = read(&asset, usize::MAX);
        assert_eq!(first.unwrap(), expected);
        assert!(copied > COLUMNS, "{copied} bytes saved");
        assert!(saved(&asset) > 0);
        let (second, restored) = read(&asset, usize::MAX);
        assert_eq!(second.unwrap(), expected);
        assert!(restored > COLUMNS, "{restored} bytes restored");
        // Stopped as a state is saved, and as one is restored: at the poll after its
        // first piece.
        for fresh in [true, false] {
            let stopped = match fresh {
                true => super::super::tests::asset(&pdf),
                false => asset.clone(),
            };
            let (got, _) = read(&stopped, 0);
            assert_eq!(got, Err(RequestError::Cancelled), "fresh {fresh}");
            let (after, _) = read(&stopped, usize::MAX);
            assert_eq!(after.unwrap(), expected, "fresh {fresh}");
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
