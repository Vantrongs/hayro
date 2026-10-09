use super::*;
use hayro_syntax::object::stream::{JpegCheckpoint, JpegRows};

const ARC_HEADER: usize = 2 * size_of::<usize>();

#[derive(Default)]
pub(super) struct JpegIndex {
    generation: u64,
    rejected: bool,
    base: Option<Arc<JpegCheckpoint>>,
    entries: Vec<Arc<JpegCheckpoint>>,
    interval: u32,
}

impl JpegIndex {
    pub(super) fn allocation_size(&self) -> usize {
        size_of::<Mutex<Self>>()
            + self.entries.capacity() * size_of::<Arc<JpegCheckpoint>>()
            + self.base.as_ref().map_or(0, |s| s.source_storage_bytes())
            + self
                .base
                .iter()
                .chain(self.entries.iter())
                .map(|s| s.storage_bytes() + ARC_HEADER)
                .sum::<usize>()
    }

    fn install(&mut self, base: JpegCheckpoint, height: u32, width: u32) -> Option<()> {
        let fixed = size_of::<Mutex<Self>>()
            + size_of::<Mutex<Checkpoints>>()
            + base.source_storage_bytes();
        let size = base.storage_bytes() + ARC_HEADER;
        let slots = CHECKPOINT_BUDGET.checked_sub(fixed + size)?
            / (size + size_of::<Arc<JpegCheckpoint>>());
        self.interval = height
            .div_ceil((slots + 1) as u32)
            .max(MIN_CHECKPOINT_BYTES.div_ceil(width as usize) as u32)
            .div_ceil(8)
            * 8;
        let slots = slots.min((height / self.interval) as usize);
        self.entries = Vec::with_capacity(slots);
        self.base = Some(Arc::new(base));
        if self.allocation_size() + size_of::<Mutex<Checkpoints>>() > CHECKPOINT_BUDGET {
            self.base = None;
            self.entries = Vec::new();
            return None;
        }
        Some(())
    }

    fn invalidate(&mut self) {
        self.generation += 1;
        self.rejected = true;
        self.base = None;
        self.entries = Vec::new();
    }

    fn needs_checkpoint(&self, row: usize, generation: u64) -> bool {
        !self.rejected
            && self.generation == generation
            && self.entries.len() < self.entries.capacity()
            && self
                .entries
                .binary_search_by_key(&row, |s| s.next_row())
                .is_err()
    }

    fn save(&mut self, state: JpegCheckpoint, generation: u64) {
        if self.rejected
            || self.generation != generation
            || self.entries.len() == self.entries.capacity()
        {
            return;
        }
        let at = match self
            .entries
            .binary_search_by_key(&state.next_row(), |s| s.next_row())
        {
            Ok(_) => return,
            Err(at) => at,
        };
        if self.allocation_size()
            + size_of::<Mutex<Checkpoints>>()
            + state.storage_bytes()
            + ARC_HEADER
            <= CHECKPOINT_BUDGET
        {
            self.entries.insert(at, Arc::new(state));
        }
    }
}

pub(super) struct JpegStreamed<'a> {
    pub(super) samples: Streamed<'a>,
    index: &'a Mutex<JpegIndex>,
}

impl<'a> JpegStreamed<'a> {
    pub(super) fn new(
        obj: &ImageXObject<'a>,
        owner: &'a OwnedStream,
        flate: &'a Mutex<Checkpoints>,
        index: &'a Mutex<JpegIndex>,
    ) -> Option<Self> {
        let dict = obj.stream.dict();
        if dict.contains_key(SMASK) || dict.contains_key(MASK) || dict.contains_key(SMASK_IN_DATA) {
            return None;
        }
        let samples = Streamed::samples(obj, owner, flate)?;
        if samples.components != 1 {
            return None;
        }
        {
            let locked = index.lock().ok()?;
            if locked.rejected {
                return None;
            }
            if locked.base.is_some() {
                return Some(Self { samples, index });
            }
        }
        // Parsing and marker validation can read all encoded bytes; neither holds the index lock.
        let mut rows = owner.jpeg_rows((obj.width, obj.height))?;
        let base = rows.checkpoint().ok()?;
        let mut locked = index.lock().ok()?;
        if locked.rejected {
            return None;
        }
        if locked.base.is_none() {
            locked.install(base, obj.height, obj.width)?;
        }
        Some(Self { samples, index })
    }

    pub(super) fn checkpoint_bytes(&self) -> Result<usize, RequestError> {
        Ok(self
            .index
            .lock()
            .map_err(|_| RequestError::Failed)?
            .allocation_size()
            + size_of::<Mutex<Checkpoints>>())
    }

    pub(super) fn run(
        &self,
        area: &mut Area,
        cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<(u32, u64), RequestError> {
        if cancel.is_some_and(|c| c()) {
            return Err(RequestError::Cancelled);
        }
        match self.run_rows(area, cancel) {
            Ok(rows) => Ok((rows, 0)),
            Err(Stop::Cancelled) => Err(RequestError::Cancelled),
            Err(Stop::Failed) => {
                self.index
                    .lock()
                    .map_err(|_| RequestError::Failed)?
                    .invalidate();
                area.reset();
                let decoded = decode_image(&self.samples.obj, None).ok_or(RequestError::Failed)?;
                Decoded::new(decoded.image, decoded.alpha)
                    .run(Plane::Image, area, cancel)
                    .map(|n| (n, 0))
            }
        }
    }

    fn run_rows(&self, area: &mut Area, cancel: Option<&dyn Fn() -> bool>) -> Result<u32, Stop> {
        let wanted = area.rows();
        let (state, interval, generation) = {
            let locked = self.index.lock().map_err(|_| Stop::Failed)?;
            if locked.rejected {
                return Err(Stop::Failed);
            }
            let state = locked
                .entries
                .iter()
                .rev()
                .find(|s| s.next_row() <= wanted.start as usize)
                .or(locked.base.as_ref())
                .ok_or(Stop::Failed)?;
            (Arc::clone(state), locked.interval, locked.generation)
        };
        let mut rows: JpegRows = state.resume().map_err(|_| Stop::Failed)?;
        let first = rows.next_row() as u32;
        drop(state);
        let width = self.samples.obj.width as usize;
        let columns = area.columns();
        let mut band = vec![0; rows.output_buffer_size()];
        let mut rgb = Vec::new();
        let mut decoded = 0;
        while rows.next_row() < wanted.end as usize {
            let y = rows.next_row() as u32;
            if cancelled(cancel, y - first) {
                return Err(Stop::Cancelled);
            }
            let count = rows.read_mcu_row(&mut band).map_err(|_| Stop::Failed)?;
            if count == 0 {
                return Err(Stop::Failed);
            }
            decoded += count as u32;
            if (rows.next_row() as u32).is_multiple_of(interval)
                && self
                    .index
                    .lock()
                    .map_err(|_| Stop::Failed)?
                    .needs_checkpoint(rows.next_row(), generation)
            {
                let state = rows.checkpoint().map_err(|_| Stop::Failed)?;
                self.index
                    .lock()
                    .map_err(|_| Stop::Failed)?
                    .save(state, generation);
            }
            for at in 0..count {
                let row = y + at as u32;
                if row >= wanted.start && row < wanted.end {
                    let pixels = &mut band
                        [at * width + columns.start as usize..at * width + columns.end as usize];
                    let colour = self.samples.convert_colour(pixels, &mut rgb)?;
                    area.push(colour);
                }
            }
        }
        if self.index.lock().map_err(|_| Stop::Failed)?.generation != generation {
            return Err(Stop::Failed);
        }
        Ok(decoded)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{assert_streams_as_decoded, asset, image, pdf, request, whole};
    use super::*;
    use hayro_syntax::Pdf;

    fn jpeg_pdf(bytes: &[u8], w: u32, h: u32, extra: &str) -> Pdf {
        pdf(&[(
            image(
                &format!("/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /DCTDecode {extra}"),
                w,
                h,
            ),
            bytes.to_vec(),
        )])
    }

    #[test]
    fn jpeg_regions_match_whole_decode_at_odd_sizes_restarts_and_inversion() {
        for (bytes, w, h) in [
            (&include_bytes!("fixtures/7x9.jpg")[..], 7, 9),
            (&include_bytes!("fixtures/33x41.jpg")[..], 33, 41),
        ] {
            for decode in ["", "/Decode [1 0]"] {
                assert_streams_as_decoded(&[(
                    image(
                        &format!(
                            "/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /DCTDecode {decode}"
                        ),
                        w,
                        h,
                    ),
                    bytes.to_vec(),
                )]);
            }
        }
    }

    #[test]
    fn jpeg_mutable_bands_preserve_transfer_and_rgb_expansion() {
        use super::super::tests::{assert_asset_streams_as_decoded, transfer};
        for four in [false, true] {
            for decode in ["", "/Decode [1 0]"] {
                let pdf = jpeg_pdf(include_bytes!("fixtures/257x4097.jpg"), 257, 4097, decode);
                let mut asset = asset(&pdf, &Cache::new());
                Arc::get_mut(&mut asset.0).unwrap().transfer_function = Some(transfer(four));
                assert_asset_streams_as_decoded(&asset);
            }
        }
    }

    #[test]
    fn jpeg_reopen_seeks_and_retains_the_original_document() {
        let pdf = jpeg_pdf(include_bytes!("fixtures/257x4097.jpg"), 257, 4097, "");
        let asset = asset(&pdf, &Cache::new());
        let mut decoded = whole(&asset);
        drop(pdf);
        let mut cold = asset.open(None).unwrap();
        assert!(cold.is_streamed());
        request(&mut cold, Plane::Image, (257, 4097), [0, 3000, 257, 3020]);
        drop(cold);
        let mut warm = asset.open(None).unwrap();
        let got = request(&mut warm, Plane::Image, (257, 4097), [3, 2800, 250, 2820]);
        let expected = request(
            &mut decoded,
            Plane::Image,
            (257, 4097),
            [3, 2800, 250, 2820],
        );
        assert_eq!(got.data, expected.data);
        assert!(got.rows_decoded < 1024, "{}", got.rows_decoded);
        assert_eq!(
            warm.checkpoint_bytes().unwrap(),
            asset.checkpoint_bytes().unwrap()
        );
        println!(
            "JPEG warm rows={} retained={}",
            got.rows_decoded,
            asset.checkpoint_bytes().unwrap()
        );
        assert!(asset.checkpoint_bytes().unwrap() <= CHECKPOINT_BUDGET);
    }

    #[test]
    fn jpeg_checkpoint_owns_input_after_asset_and_pdf_are_dropped() {
        let pdf = jpeg_pdf(include_bytes!("fixtures/33x41.jpg"), 33, 41, "");
        let asset = asset(&pdf, &Cache::new());
        let expected = request(&mut whole(&asset), Plane::Image, (33, 41), [0, 0, 33, 41]).data;
        drop(asset.open(None).unwrap());
        let checkpoint = Arc::clone(asset.0.jpeg.lock().unwrap().base.as_ref().unwrap());
        drop(asset);
        drop(pdf);
        let mut rows = checkpoint.resume().unwrap();
        let mut decoded = Vec::new();
        while rows.output_buffer_size() > 0 {
            let mut band = vec![0; rows.output_buffer_size()];
            rows.read_mcu_row(&mut band).unwrap();
            decoded.extend(band);
        }
        assert_eq!(decoded, expected);
    }

    #[test]
    fn jpeg_cancel_reentrancy_concurrent_readers_and_eviction() {
        let pdf = jpeg_pdf(include_bytes!("fixtures/257x4097.jpg"), 257, 4097, "");
        let asset = asset(&pdf, &Cache::new());
        let mut cold = asset.open(None).unwrap();
        request(&mut cold, Plane::Image, (257, 4097), [0, 3000, 257, 3020]);
        let cancel = || {
            assert!(asset.checkpoint_bytes().unwrap() <= CHECKPOINT_BUDGET);
            true
        };
        assert_eq!(
            cold.request(&ImageRequest {
                plane: Plane::Image,
                grid: (257, 4097),
                window: [0, 2800, 257, 2820],
                cancel: Some(&cancel)
            })
            .unwrap_err(),
            RequestError::Cancelled
        );
        let polls = std::cell::Cell::new(0);
        let mid_cancel = || {
            assert!(asset.checkpoint_bytes().unwrap() <= CHECKPOINT_BUDGET);
            polls.set(polls.get() + 1);
            polls.get() >= 3
        };
        assert_eq!(
            cold.request(&ImageRequest {
                plane: Plane::Image,
                grid: (257, 4097),
                window: [0, 3100, 257, 3200],
                cancel: Some(&mid_cancel)
            })
            .unwrap_err(),
            RequestError::Cancelled
        );
        assert_eq!(polls.get(), 3);
        let expected = request(
            &mut whole(&asset),
            Plane::Image,
            (257, 4097),
            [0, 2800, 257, 2820],
        );
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        request(
                            &mut asset.open(None).unwrap(),
                            Plane::Image,
                            (257, 4097),
                            [0, 2800, 257, 2820],
                        )
                    })
                })
                .collect();
            for worker in workers {
                assert_eq!(worker.join().unwrap().data, expected.data);
            }
        });
        let warm = request(&mut cold, Plane::Image, (257, 4097), [0, 2800, 257, 2820]);
        asset.0.jpeg.lock().unwrap().entries.clear();
        let evicted = request(&mut cold, Plane::Image, (257, 4097), [0, 2800, 257, 2820]);
        assert_eq!(warm.data, evicted.data);
        assert!(warm.rows_decoded < evicted.rows_decoded);
    }

    #[test]
    fn jpeg_rejected_generation_discards_partial_bins_and_reopens_whole() {
        let pdf = jpeg_pdf(include_bytes!("fixtures/257x4097.jpg"), 257, 4097, "");
        let asset = asset(&pdf, &Cache::new());
        let mut source = asset.open(None).unwrap();
        let expected = request(&mut whole(&asset), Plane::Image, (71, 113), [0, 0, 71, 113]);
        let polls = std::cell::Cell::new(0);
        let reject = || {
            polls.set(polls.get() + 1);
            if polls.get() == 3 {
                asset.0.jpeg.lock().unwrap().invalidate();
            }
            false
        };
        let got = source
            .request(&ImageRequest {
                plane: Plane::Image,
                grid: (71, 113),
                window: [0, 0, 71, 113],
                cancel: Some(&reject),
            })
            .unwrap();
        assert_eq!(got.data, expected.data);
        let index = asset.0.jpeg.lock().unwrap();
        assert!(index.rejected);
        assert!(index.base.is_none());
        assert!(index.entries.is_empty());
        drop(index);
        assert!(!asset.open(None).unwrap().is_streamed());
    }

    #[test]
    fn jpeg_checkpoints_count_raw_bands_base_source_and_vector_capacity() {
        let pdf = jpeg_pdf(include_bytes!("fixtures/32769x529.jpg"), 32769, 529, "");
        let asset = asset(&pdf, &Cache::new());
        let mut source = asset.open(None).unwrap();
        assert!(source.is_streamed());
        let expected = request(
            &mut whole(&asset),
            Plane::Image,
            (32769, 529),
            [19, 480, 37, 529],
        );
        let actual = request(&mut source, Plane::Image, (32769, 529), [19, 480, 37, 529]);
        assert_eq!(actual.data, expected.data);
        let mut index = asset.0.jpeg.lock().unwrap();
        assert!(!index.entries.is_empty());
        let actual_size = size_of::<Mutex<JpegIndex>>()
            + size_of::<Mutex<Checkpoints>>()
            + index.entries.capacity() * size_of::<Arc<JpegCheckpoint>>()
            + index.base.as_ref().unwrap().source_storage_bytes()
            + index
                .base
                .iter()
                .chain(index.entries.iter())
                .map(|s| s.storage_bytes() + ARC_HEADER)
                .sum::<usize>();
        assert_eq!(
            actual_size,
            index.allocation_size() + size_of::<Mutex<Checkpoints>>()
        );
        assert!(actual_size <= CHECKPOINT_BUDGET);
        println!(
            "JPEG wide checkpoint retained={} slots={}",
            actual_size,
            index.entries.len()
        );
        let mut reader = index.base.as_ref().unwrap().resume().unwrap();
        let generation = index.generation;
        let state = reader.checkpoint().unwrap();
        index.invalidate();
        index.save(state, generation);
        assert!(index.entries.is_empty());
        assert!(index.base.is_none());
    }

    #[test]
    fn jpeg_truncation_matches_whole_and_incompatible_dimensions_use_original_path() {
        let bytes = include_bytes!("fixtures/33x41.jpg");
        for cut in [bytes.len() - 2, bytes.len() - 31, bytes.len() / 2] {
            let pdf = jpeg_pdf(&bytes[..cut], 33, 41, "");
            let asset = asset(&pdf, &Cache::new());
            let obj = asset.xobject().unwrap();
            if decode_image(&obj, None).is_some() {
                let mut source = asset.open(None).unwrap();
                let a = request(&mut source, Plane::Image, (33, 41), [0, 0, 33, 41]);
                let b = request(&mut whole(&asset), Plane::Image, (33, 41), [0, 0, 33, 41]);
                assert_eq!(a.data, b.data, "cut {cut}");
            }
        }
        for (w, h) in [(32, 41), (34, 41), (33, 40), (33, 42)] {
            let pdf = jpeg_pdf(bytes, w, h, "");
            let asset = asset(&pdf, &Cache::new());
            assert!(!asset.open(None).unwrap().is_streamed());
        }
    }

    #[test]
    fn jpeg_additional_scan_markers_are_rejected_before_first_pixels() {
        let mut bytes = include_bytes!("fixtures/33x41.jpg").to_vec();
        let restart = bytes
            .windows(2)
            .position(|w| w[0] == 255 && (0xd0..=0xd7).contains(&w[1]))
            .unwrap();
        bytes[restart + 1] = 0xda;
        let pdf = jpeg_pdf(&bytes, 33, 41, "");
        let asset = asset(&pdf, &Cache::new());
        let obj = asset.xobject().unwrap();
        assert!(
            JpegStreamed::new(&obj, &asset.0.stream, &asset.0.checkpoints, &asset.0.jpeg).is_none()
        );
        assert!(asset.0.jpeg.lock().unwrap().base.is_none());
    }
}
