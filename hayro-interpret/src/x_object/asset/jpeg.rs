//! Baseline grayscale JPEG images read in bands of rows for each request, resumed
//! from states saved along the bands.

use super::decoded::Decoded;
use super::index::{CHECKPOINT_BUDGET, Checkpoints, MIN_CHECKPOINT_BYTES};
use super::streamed::Colour;
use super::{DecodeWork, ImageLayout, Plane, Stop};
use crate::x_object::area::Area;
use crate::x_object::decode::decode_image;
use crate::x_object::image::ImageXObject;
use hayro_syntax::object::dict::keys::{MASK, SMASK, SMASK_IN_DATA};
use hayro_syntax::object::stream::{JpegCheckpoint, JpegRows, OwnedStream};
use hayro_syntax::work::{Cancelled, Meter};
use std::sync::Mutex;

/// JPEG states, by the row they resume at.
#[derive(Default)]
pub(super) enum JpegIndex {
    /// Not yet parsed.
    #[default]
    Unread,
    /// Boxed, so that the many assets of other images do not hold its room.
    Read(Box<Saved>),
    /// Not a JPEG rows can be read from, or a reader of it failed: it is decoded
    /// whole.
    Rejected,
}

pub(super) struct Saved {
    /// The state at the first row.
    base: JpegCheckpoint,
    /// Rows between saved states, a multiple of the eight a band holds.
    interval: u32,
    slots: usize,
    /// Sorted by row; capacity `slots`.
    entries: Vec<JpegCheckpoint>,
}

impl JpegIndex {
    /// The bytes the saved states take beyond the index itself.
    pub(super) fn heap_bytes(&self) -> usize {
        let Self::Read(saved) = self else {
            return 0;
        };
        size_of::<Saved>()
            + heap_bytes(&saved.base)
            + saved.entries.capacity() * size_of::<JpegCheckpoint>()
            + saved.entries.iter().map(heap_bytes).sum::<usize>()
    }
}

fn heap_bytes(state: &JpegCheckpoint) -> usize {
    state.storage_bytes() - size_of::<JpegCheckpoint>()
}

impl Saved {
    /// An index for `height` rows of `width` starting at `base`: as many states as
    /// the budget holds besides it, spread over the rows with it.
    fn new(base: JpegCheckpoint, width: u32, height: u32) -> Self {
        let slots = (CHECKPOINT_BUDGET - size_of::<Checkpoints>() - size_of::<Self>())
            .saturating_sub(heap_bytes(&base))
            / base.storage_bytes();
        let interval = height
            .div_ceil(slots as u32 + 1)
            .max(MIN_CHECKPOINT_BYTES.div_ceil(width as usize) as u32)
            .next_multiple_of(8);
        let slots = slots.min((height / interval) as usize);
        Self {
            base,
            interval,
            slots,
            entries: Vec::with_capacity(slots),
        }
    }

    /// Whether a state at `row` is due and has room.
    fn wants(&self, row: usize) -> bool {
        row.is_multiple_of(self.interval as usize)
            && self.entries.len() < self.slots
            && self.position(row).is_err()
    }

    fn position(&self, row: usize) -> Result<usize, usize> {
        self.entries.binary_search_by_key(&row, |s| s.next_row())
    }
}

/// A baseline grayscale JPEG read in bands.
pub(super) struct JpegStreamed<'a> {
    obj: ImageXObject<'a>,
    colour: Colour,
    index: &'a Mutex<JpegIndex>,
}

impl<'a> JpegStreamed<'a> {
    /// The image read in bands, if its data is a baseline grayscale JPEG of its size
    /// without masks, its samples are ones [`Colour`] converts, and no reader of it
    /// failed.
    /// A stop while parsing leaves the index unread.
    pub(super) fn new(
        obj: &ImageXObject<'a>,
        stream: &OwnedStream,
        index: &'a Mutex<JpegIndex>,
        meter: &Meter<'_>,
    ) -> Result<Option<Self>, Cancelled> {
        let dict = obj.stream.dict();
        if [SMASK, MASK, SMASK_IN_DATA]
            .iter()
            .any(|k| dict.contains_key(k))
        {
            return Ok(None);
        }
        let Some(colour) = Colour::new(obj).filter(|c| c.components() == 1) else {
            return Ok(None);
        };
        let Ok(locked) = index.lock() else {
            return Ok(None);
        };
        if matches!(*locked, JpegIndex::Unread) {
            drop(locked);
            // Parsing reads all of the data; the index is not locked meanwhile.
            let base = stream
                .jpeg_rows((obj.width, obj.height), meter)
                .and_then(|mut rows| rows.checkpoint().ok());
            meter.poll()?;
            let Ok(mut locked) = index.lock() else {
                return Ok(None);
            };
            if matches!(*locked, JpegIndex::Unread) {
                *locked = base.map_or(JpegIndex::Rejected, |base| {
                    JpegIndex::Read(Box::new(Saved::new(base, obj.width, obj.height)))
                });
            }
        } else {
            drop(locked);
        }
        let read = index
            .lock()
            .is_ok_and(|index| matches!(*index, JpegIndex::Read(_)));
        Ok(read.then(|| Self {
            obj: obj.clone(),
            colour,
            index,
        }))
    }

    pub(super) fn layout(&self) -> ImageLayout {
        ImageLayout {
            image: self.colour.plane(&self.obj, false),
            mask: None,
        }
    }

    /// Reads the image plane into `area`. Once a reader fails, the image is decoded
    /// whole, for this request and the asset's later ones, so that they all agree.
    pub(super) fn run(&self, area: &mut Area<'_>, meter: &Meter<'_>) -> Result<DecodeWork, Stop> {
        let mut decoded = 0;
        let rows = match self.read(area, meter, &mut decoded) {
            Ok(()) => decoded,
            Err(Stop::Failed) => {
                // A reader stopped by the meter fails as on bad data.
                meter.poll()?;
                decoded + self.decode_whole(area, meter)?
            }
            Err(stop) => return Err(stop),
        };
        Ok(DecodeWork {
            rows_decoded: u64::from(rows),
            bytes_inflated: 0,
        })
    }

    /// Reads bands from the state nearest before `area`'s rows, counting the rows
    /// in `decoded`; fails if a reader of the asset failed meanwhile.
    fn read(&self, area: &mut Area<'_>, meter: &Meter<'_>, decoded: &mut u32) -> Result<(), Stop> {
        let wanted = area.rows();
        let mut rows = self.resume(wanted.start, meter)?;
        let (width, columns) = (self.obj.width as usize, area.columns());
        let mut band = vec![0; rows.output_buffer_size()];
        let mut rgb = Vec::new();
        while (rows.next_row() as u32) < wanted.end {
            let y = rows.next_row() as u32;
            let count = rows.read_mcu_row(&mut band).map_err(|_| Stop::Failed)?;
            *decoded += count as u32;
            self.save(&mut rows)?;
            for (row, texels) in (y..).zip(band.chunks_exact_mut(width).take(count)) {
                if wanted.contains(&row) {
                    let texels = &mut texels[columns.start as usize..columns.end as usize];
                    area.push(self.colour.convert(texels, &mut rgb)?)?;
                }
            }
        }
        match *self.index.lock().map_err(|_| Stop::Failed)? {
            JpegIndex::Read(_) => Ok(()),
            _ => Err(Stop::Failed),
        }
    }

    /// A reader at the state nearest at or before row `before`, its work counted on
    /// `meter`.
    fn resume<'m>(&self, before: u32, meter: &'m Meter<'m>) -> Result<JpegRows<'m>, Stop> {
        let state = {
            let index = self.index.lock().map_err(|_| Stop::Failed)?;
            let JpegIndex::Read(saved) = &*index else {
                return Err(Stop::Failed);
            };
            saved
                .entries
                .iter()
                .rev()
                .find(|s| s.next_row() <= before as usize)
                .unwrap_or(&saved.base)
                .clone()
        };
        // Resuming parses the headers again, polling the meter's hook, which may read
        // the index.
        state.resume(meter).map_err(|_| Stop::Failed)
    }

    /// Saves `rows`' state if one is due there and the index has room.
    fn save(&self, rows: &mut JpegRows<'_>) -> Result<(), Stop> {
        let row = rows.next_row();
        let wanted = |index: &JpegIndex| matches!(index, JpegIndex::Read(s) if s.wants(row));
        if !wanted(&*self.index.lock().map_err(|_| Stop::Failed)?) {
            return Ok(());
        }
        // Copying the state does not hold the index.
        let state = rows.checkpoint().map_err(|_| Stop::Failed)?;
        let mut index = self.index.lock().map_err(|_| Stop::Failed)?;
        if let JpegIndex::Read(saved) = &mut *index
            && saved.wants(row)
            && let Err(at) = saved.position(row)
        {
            saved.entries.insert(at, state);
        }
        Ok(())
    }

    /// Rejects the asset's JPEG rows and reads `area` from the image decoded whole;
    /// returns the rows decoded, all of the image's.
    fn decode_whole(&self, area: &mut Area<'_>, meter: &Meter<'_>) -> Result<u32, Stop> {
        *self.index.lock().map_err(|_| Stop::Failed)? = JpegIndex::Rejected;
        area.reset()?;
        let decoded = decode_image(&self.obj, None, meter);
        meter.poll()?;
        let decoded = decoded.ok_or(Stop::Failed)?;
        let rows = decoded.image.height();
        Decoded::new(decoded.image, decoded.alpha, meter)?.run(Plane::Image, area)?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        assert_asset_streams_as_decoded, asset, image, pdf, request, rows, transfer, whole,
    };
    use super::super::{ImageAsset, ImageRequest, ImageSource, Inner, Plane, RequestError};
    use super::*;
    use hayro_syntax::Pdf;
    #[cfg(debug_assertions)]
    use hayro_syntax::work::{POLL_BYTES, TOLD};
    use std::cell::Cell;
    use std::sync::Arc;

    /// A baseline grayscale JPEG of `w` × `h`, with a restart marker every `restart`
    /// MCUs (none for 0).
    fn jpeg(w: u16, h: u16, restart: u16) -> Vec<u8> {
        let (w_, h_) = (usize::from(w), usize::from(h));
        let pixels: Vec<u8> = (0..h_)
            .flat_map(|y| (0..w_).map(move |x| ((x * 17 + y * 13 + x * y % 37) & 255) as u8))
            .collect();
        let mut out = Vec::new();
        let mut encoder = jpeg_encoder::Encoder::new(&mut out, 91);
        encoder.set_restart_interval(restart);
        encoder
            .encode(&pixels, w, h, jpeg_encoder::ColorType::Luma)
            .unwrap();
        out
    }

    /// A document whose object 3 is `bytes` as a `w` × `h` gray `DCTDecode` image.
    fn jpeg_pdf(bytes: &[u8], w: u32, h: u32, extra: &str) -> Pdf {
        let dict =
            format!("/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /DCTDecode {extra}");
        pdf(&[(image(&dict, w, h), bytes.to_vec())])
    }

    fn is_jpeg(source: &ImageSource<'_>) -> bool {
        matches!(source.inner, Inner::Jpeg(_))
    }

    fn index(asset: &ImageAsset) -> std::sync::MutexGuard<'_, JpegIndex> {
        asset.0.checkpoints.jpeg.lock().unwrap()
    }

    /// Bands at odd sizes, across restart markers, inverted, through a transfer
    /// function and expanded to RGB by one, return what decoding the image whole does.
    #[test]
    fn bands_stream_as_decoded() {
        for (w, h, restart) in [(7, 9, 0), (33, 41, 3)] {
            let bytes = jpeg(w, h, restart);
            for decode in ["", "/Decode [1 0]"] {
                for four in [None, Some(false), Some(true)] {
                    let pdf = jpeg_pdf(&bytes, w.into(), h.into(), decode);
                    let mut asset = asset(&pdf);
                    Arc::get_mut(&mut asset.0).unwrap().transfer_function = four.map(transfer);
                    assert!(is_jpeg(&asset.open(None, None).unwrap()));
                    assert_asset_streams_as_decoded(&asset);
                }
            }
        }
    }

    /// Reopened after its document was dropped, an image resumes from the band state
    /// nearest before a request's rows.
    #[test]
    fn a_reopened_image_resumes_from_a_saved_band() {
        let (w, h) = (257, 4097);
        let pdf = jpeg_pdf(&jpeg(w as u16, h as u16, 7), w, h, "");
        let asset = asset(&pdf);
        drop(pdf);
        let window = [3, 2800, 250, 2820];
        let expected = request(&mut whole(&asset), Plane::Image, (w, h), window);
        rows(&mut asset.open(None, None).unwrap(), 3000..3020);
        let mut warm = asset.open(None, None).unwrap();
        let before = warm.work();
        assert_eq!(
            request(&mut warm, Plane::Image, (w, h), window).data,
            expected.data
        );
        let decoded = warm.work().rows_decoded - before.rows_decoded;
        let interval = MIN_CHECKPOINT_BYTES
            .div_ceil(w as usize)
            .next_multiple_of(8) as u64;
        assert!(decoded < interval + 24, "{decoded} rows");
        assert!(asset.checkpoint_bytes().unwrap() <= CHECKPOINT_BUDGET);
    }

    /// A cancel hook may read the asset's checkpoint bytes while bands are read, also
    /// while long headers are parsed again, and sources of one asset read at once.
    #[test]
    fn the_index_is_not_locked_while_bands_are_read() {
        let (w, h) = (257, 4097);
        let plain = jpeg(w as u16, h as u16, 7);
        let with_headers = |segment: &[u8], count: usize| {
            let mut bytes = plain[..2].to_vec();
            bytes.extend(segment.repeat(count));
            bytes.extend(&plain[2..]);
            bytes
        };
        let mut table = vec![0xff, 0xdb, 0, 67, 0];
        table.extend([1; 64]);
        let mut comment = vec![0xff, 0xfe, 0xff, 0xff];
        comment.extend([b'c'; 65_533]);
        for bytes in [
            plain.clone(),
            with_headers(&table, 2048),
            with_headers(&comment, 32),
        ] {
            let pdf = jpeg_pdf(&bytes, w, h, "");
            let asset = asset(&pdf);
            let mut source = asset.open(None, None).unwrap();
            assert!(is_jpeg(&source));
            rows(&mut source, 3000..3020);
            let polls = Cell::new(0);
            let cancel = || {
                assert!(asset.checkpoint_bytes().unwrap() <= CHECKPOINT_BUDGET);
                polls.set(polls.get() + 1);
                polls.get() == 3
            };
            let request = ImageRequest {
                plane: Plane::Image,
                grid: (w, h),
                window: [0, 3100, w, 3200],
                cancel: Some(&cancel),
            };
            assert_eq!(
                source.request(&request).unwrap_err(),
                RequestError::Cancelled
            );
            assert_eq!(polls.get(), 3);
            let (expected, _) = rows(&mut whole(&asset), 2800..2820);
            std::thread::scope(|scope| {
                let read =
                    || scope.spawn(|| rows(&mut asset.open(None, None).unwrap(), 2800..2820).0);
                let (a, b) = (read(), read());
                assert_eq!(a.join().unwrap(), expected);
                assert_eq!(b.join().unwrap(), expected);
            });
        }
    }

    /// When another reader fails during a request, the request returns the image
    /// decoded whole, as do later opens; its work counts the bands read and every
    /// row of the whole decoding, whatever the window.
    #[test]
    fn a_failed_reader_leaves_the_image_to_whole_decoding() {
        let (w, h) = (257, 4097);
        let pdf = jpeg_pdf(&jpeg(w as u16, h as u16, 7), w, h, "");
        let grid = (71, 113);
        for (window, bands) in [([0, 0, 71, 113], h), ([5, 60, 9, 62], 2248)] {
            let asset = asset(&pdf);
            let mut source = asset.open(None, None).unwrap();
            let polls = Cell::new(0);
            let reject = || {
                polls.set(polls.get() + 1);
                if polls.get() == 3 {
                    *index(&asset) = JpegIndex::Rejected;
                }
                false
            };
            let request = ImageRequest {
                plane: Plane::Image,
                grid,
                window,
                cancel: Some(&reject),
            };
            let expected =
                super::super::tests::request(&mut whole(&asset), Plane::Image, grid, window);
            assert_eq!(source.request(&request).unwrap().data, expected.data);
            assert_eq!(
                source.work().rows_decoded,
                u64::from(bands + h),
                "{window:?}"
            );
            assert!(!is_jpeg(&asset.open(None, None).unwrap()));
        }
    }

    /// States of a wide image, each a band of coefficients, fill their slots within
    /// the budget.
    #[test]
    fn wide_states_fill_their_slots_within_the_budget() {
        let (w, h) = (20_000, 100);
        let pdf = jpeg_pdf(&jpeg(w as u16, h as u16, 5), w, h, "");
        let asset = asset(&pdf);
        let window = [19, 80, 37, 100];
        let expected = request(&mut whole(&asset), Plane::Image, (w, h), window);
        let actual = request(
            &mut asset.open(None, None).unwrap(),
            Plane::Image,
            (w, h),
            window,
        );
        assert_eq!(actual.data, expected.data);
        assert!(asset.checkpoint_bytes().unwrap() <= CHECKPOINT_BUDGET);
        let JpegIndex::Read(saved) = &*index(&asset) else {
            panic!("not read in bands");
        };
        assert!(saved.slots > 0);
        assert_eq!(saved.entries.len(), saved.slots);
        assert_eq!(saved.entries.capacity(), saved.slots);
    }

    /// Data cut short reads as decoding it whole does; data of another size than the
    /// dictionary's, or with a second scan, is decoded whole.
    #[test]
    fn other_data_reads_as_whole_decoding_does() {
        let bytes = jpeg(33, 41, 3);
        for cut in [bytes.len() - 2, bytes.len() - 31, bytes.len() / 2] {
            let pdf = jpeg_pdf(&bytes[..cut], 33, 41, "");
            let asset = asset(&pdf);
            if decode_image(&asset.xobject().unwrap(), None, &Meter::default()).is_some() {
                let (a, _) = rows(&mut asset.open(None, None).unwrap(), 0..41);
                let (b, _) = rows(&mut whole(&asset), 0..41);
                assert_eq!(a, b, "cut at {cut}");
            }
        }
        let mut second_scan = bytes.clone();
        let restart = bytes
            .windows(2)
            .position(|m| m[0] == 0xff && (0xd0..=0xd7).contains(&m[1]))
            .unwrap();
        second_scan[restart + 1] = 0xda;
        for (bytes, w, h) in [
            (&bytes, 32, 41),
            (&bytes, 34, 41),
            (&bytes, 33, 40),
            (&bytes, 33, 42),
            (&second_scan, 33, 41),
        ] {
            let pdf = jpeg_pdf(bytes, w, h, "");
            let asset = asset(&pdf);
            assert!(
                !asset.open(None, None).is_ok_and(|s| is_jpeg(&s)),
                "{w} × {h}"
            );
            assert!(matches!(*index(&asset), JpegIndex::Rejected));
        }
    }

    /// A first open stopped while checking the scan returns within three polls' worth
    /// of work and leaves the index unread; opening again reads the image in bands.
    /// Decoding a color JPEG whole stops the same way.
    #[test]
    #[cfg(debug_assertions)]
    fn a_first_open_is_cancelled_within_bounded_work() {
        let (w, h) = (1024_u16, 1024_u16);
        let pixels = crate::fixtures::noise(usize::from(w) * usize::from(h) * 3, 5);
        let encode = |color: jpeg_encoder::ColorType, bytes: &[u8]| {
            let mut out = Vec::new();
            jpeg_encoder::Encoder::new(&mut out, 100)
                .encode(bytes, w, h, color)
                .unwrap();
            out
        };
        let gray = encode(jpeg_encoder::ColorType::Luma, &pixels[..pixels.len() / 3]);
        let color = encode(jpeg_encoder::ColorType::Rgb, &pixels);
        for (name, bytes, space, banded) in [
            ("gray", gray, "/DeviceGray", true),
            ("color", color, "/DeviceRGB", false),
        ] {
            let dict = format!("/ColorSpace {space} /BitsPerComponent 8 /Filter /DCTDecode");
            let pdf = pdf(&[(image(&dict, w.into(), h.into()), bytes)]);
            let asset = asset(&pdf);
            let polls = Cell::new(0);
            let hook = || {
                polls.set(polls.get() + 1);
                polls.get() > 1
            };
            TOLD.set(0);
            let opened = asset.open(None, Some(&hook));
            assert!(matches!(opened, Err(RequestError::Cancelled)), "{name}");
            assert_eq!(polls.get(), 2, "{name}");
            assert!(
                TOLD.get() < 3 * POLL_BYTES,
                "{name}: {} bytes of work",
                TOLD.get()
            );
            assert!(matches!(*index(&asset), JpegIndex::Unread), "{name}");
            let mut source = asset.open(None, None).unwrap();
            assert_eq!(is_jpeg(&source), banded, "{name}");
            let window = [0, 1000, w.into(), h.into()];
            let grid = (w.into(), h.into());
            let a = request(&mut source, Plane::Image, grid, window);
            let b = request(&mut whole(&asset), Plane::Image, grid, window);
            assert_eq!(a.data, b.data, "{name}");
        }
    }

    /// Rows read from an asset's stream keep its bytes after the asset and the
    /// document are dropped.
    #[test]
    fn rows_keep_their_bytes() {
        let pdf = jpeg_pdf(&jpeg(33, 41, 3), 33, 41, "");
        let asset = asset(&pdf);
        let (expected, _) = rows(&mut whole(&asset), 0..41);
        let meter = Meter::default();
        let mut rows = asset.0.stream.jpeg_rows((33, 41), &meter).unwrap();
        drop((asset, pdf));
        let mut decoded = Vec::new();
        while rows.output_buffer_size() > 0 {
            let mut band = vec![0; rows.output_buffer_size()];
            rows.read_mcu_row(&mut band).unwrap();
            decoded.extend(band);
        }
        assert_eq!(decoded, expected);
    }
}
