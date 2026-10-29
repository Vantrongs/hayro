use crate::crypto::reader::{DecryptedReader, Seekable};
use crate::filter::reader::DecodedReader;
use crate::object::Dict;
use crate::object::dict::keys::{
    BLACK_IS_1, COLUMNS, ENCODED_BYTE_ALIGN, END_OF_BLOCK, END_OF_LINE, K, ROWS,
};
use crate::object::stream::{FilterResult, ImageColorSpace, ImageData, ImageDecodeParams};
use crate::work::{Cancelled, Meter, POLL_BYTES};
use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
#[cfg(feature = "unsafe")]
use hayro_ccitt::RowState;
use hayro_ccitt::{DecodeSettings, Decoder, EncodingMode, RowDecoder, Source};

/// Makes room in `output` for `needed` bytes: it grows to the `declared` bytes
/// divided by eight as often as that leaves room for them, and no further once they
/// are at most [`POLL_BYTES`]. It so holds less than eight times `needed`, or at most
/// `POLL_BYTES`: a stream cut short or stopped costs what it decoded, not what `Rows`
/// and `Columns` declare, while the last growth to the declared size copies an eighth
/// of it. Past the declared size, or without one, it doubles.
#[inline(always)]
fn reserve(output: &mut Vec<u8>, needed: usize, declared: usize) {
    if needed > output.capacity() {
        grow_to(output, needed, declared);
    }
}

#[cold]
#[inline(never)]
fn grow_to(output: &mut Vec<u8>, needed: usize, declared: usize) {
    let mut target = declared;
    while target > POLL_BYTES && target / 8 >= needed {
        target /= 8;
    }
    if target < needed {
        target = needed.max(2 * output.len());
    }
    output.reserve_exact(target - output.len());
}

/// How a stream's CCITT data decodes: its settings, and the byte its white pixels
/// are written as, black ones being its complement.
#[derive(Clone, Copy)]
struct Setup {
    settings: DecodeSettings,
    white: u8,
}

impl Setup {
    fn new(params: &Dict<'_>, image_params: &ImageDecodeParams) -> Self {
        let k = params.get::<i32>(K).unwrap_or(0);

        let columns = params.get::<usize>(COLUMNS).unwrap_or(1728) as u32;
        // `Rows` should match the height of the image, but in case it doesn't,
        // the image height takes precedence (as in other viewers).
        let rows = if image_params.height > 0 {
            image_params.height
        } else {
            params.get::<u32>(ROWS).unwrap_or(0)
        };

        let mut settings = DecodeSettings::new(columns, rows);
        settings.end_of_block = params.get::<bool>(END_OF_BLOCK).unwrap_or(true);
        settings.end_of_line = params.get::<bool>(END_OF_LINE).unwrap_or(false);
        settings.rows_are_byte_aligned = params.get::<bool>(ENCODED_BYTE_ALIGN).unwrap_or(false);
        settings.encoding = if k < 0 {
            EncodingMode::Group4
        } else if k == 0 {
            EncodingMode::Group3_1D
        } else {
            EncodingMode::Group3_2D
        };
        // The value that pixels which are white in the encoding are written as.
        let white: u8 = if params.get::<bool>(BLACK_IS_1).unwrap_or(false) {
            0x00
        } else {
            0xFF
        };
        Self { settings, white }
    }

    /// What decoding reports of the samples: 8-bit gray, or of a bit for an indexed
    /// image (`packed`), in rows of the columns, as many as the image declares.
    fn image_data(&self, packed: bool) -> ImageData {
        ImageData {
            alpha: None,
            color_space: Some(ImageColorSpace::Gray),
            bits_per_component: if packed { 1 } else { 8 },
            width: self.settings.columns,
            height: self.settings.rows,
        }
    }

    /// The bytes of a row, of a bit a pixel if `packed`.
    fn row_len(&self, packed: bool) -> usize {
        let columns = self.settings.columns as usize;
        if packed { columns.div_ceil(8) } else { columns }
    }
}

/// An image's CCITT data, read as the bytes [`decode`] makes of it, a piece at a time
/// as rows decode: 8-bit gray rows, or rows of a bit a pixel for an indexed image, as
/// many as decode before the data ends or breaks off (the image pads the rest as it
/// pads any data that ends early). It holds the decoder's state and a row's colour
/// changes, not the rows, and of encrypted data a window of it decrypted.
pub(crate) struct CcittBytes<'a> {
    data: Encoded<'a>,
    setup: Setup,
    /// Whether the rows are of a bit a pixel.
    packed: bool,
    /// Made at the first read, and again after a rewind.
    decoder: Option<Decoding<'a>>,
    /// The bytes of the decoder's last row that were read, and those left.
    at: usize,
    left: usize,
    state: State,
    /// Whether a meter's hook asked to stop, after which reads fail until a rewind.
    stopped: bool,
}

/// The encoded data.
enum Encoded<'a> {
    Plain(&'a [u8]),
    /// Decrypted as it is read (boxed: a cipher's state is large).
    Encrypted(Box<DecryptedReader<'a>>),
    /// What other filters make of the stored data, read through a feed.
    Decoded(Rc<Through<'a>>),
}

/// The decoder, and what encrypted or decoded data is read through.
struct Decoding<'a> {
    rows: RowDecoder<'a>,
    through: Option<Rc<Through<'a>>>,
}

/// What the decoder reads data other than the stored bytes through.
#[cfg_attr(
    feature = "unsafe",
    expect(
        clippy::large_enum_variant,
        reason = "held in an `Rc`, one allocation whichever variant it is"
    )
)]
enum Through<'a> {
    Window(Window<'a>),
    Feed(Feed<'a>),
}

impl Source for Through<'_> {
    fn read_at(&self, pos: usize, out: &mut [u8]) -> usize {
        match self {
            Self::Window(window) => window.read_at(pos, out),
            Self::Feed(feed) => feed.read_at(pos, out),
        }
    }
}

impl<'a> Through<'a> {
    fn feed(&self) -> Option<&Feed<'a>> {
        match self {
            Self::Feed(feed) => Some(feed),
            Self::Window(_) => None,
        }
    }
}

/// Where a reader of plain CCITT data was ([`CcittBytes::save`]): the decoder's
/// state, `None` before it started, and the bytes of its last row read.
#[cfg(feature = "unsafe")]
#[derive(Clone)]
pub(crate) struct CcittState {
    rows: Option<RowState>,
    at: usize,
    left: usize,
    done: bool,
}

#[cfg(feature = "unsafe")]
impl CcittState {
    pub(crate) fn heap_bytes(&self) -> usize {
        self.rows.as_ref().map_or(0, RowState::heap_bytes)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// A decoded row is being read, or none before the first.
    Row,
    /// The data ended or broke off, and the bytes with it.
    Done,
}

impl<'a> CcittBytes<'a> {
    /// The bytes `data` decodes to with filter parameters `params`.
    pub(crate) fn new(
        data: DecryptedReader<'a>,
        params: &Dict<'_>,
        image_params: &ImageDecodeParams,
    ) -> Self {
        let data = match data.plain_borrowed() {
            Some(plain) => Encoded::Plain(plain),
            None => Encoded::Encrypted(Box::new(data)),
        };
        Self::of(
            data,
            Setup::new(params, image_params),
            image_params.is_indexed,
        )
    }

    /// The bytes the data `reader` decodes to make with filter parameters `params`.
    pub(crate) fn decoded(
        reader: DecodedReader<'a>,
        params: &Dict<'_>,
        image_params: &ImageDecodeParams,
    ) -> Self {
        let feed = Feed {
            state: RefCell::new(FeedState {
                reader,
                bytes: Kept::default(),
                start: 0,
                ended: false,
            }),
            ahead: Cell::new(WINDOW),
            least: Cell::new(WINDOW),
            short: Cell::new(false),
            before: Cell::new(None),
        };
        Self::of(
            Encoded::Decoded(Rc::new(Through::Feed(feed))),
            Setup::new(params, image_params),
            image_params.is_indexed,
        )
    }

    fn of(data: Encoded<'a>, setup: Setup, packed: bool) -> Self {
        Self {
            data,
            setup,
            packed,
            decoder: None,
            at: 0,
            left: 0,
            state: State::Row,
            stopped: false,
        }
    }

    /// The bytes of a row.
    fn row_len(&self) -> usize {
        self.setup.row_len(self.packed)
    }

    /// The data, where it is neither encrypted nor what other filters make: a reader
    /// of it goes on from a saved state ([`Self::save`]).
    #[cfg(feature = "unsafe")]
    pub(crate) fn plain(&self) -> Option<&'a [u8]> {
        match &self.data {
            Encoded::Plain(data) => Some(data),
            _ => None,
        }
    }

    /// Where it is in plain data, to go on from with [`Self::restore`]; `None` for
    /// other data, or once a meter's hook stopped it.
    #[cfg(feature = "unsafe")]
    pub(crate) fn save(&self) -> Option<CcittState> {
        self.plain()?;
        (!self.stopped).then(|| CcittState {
            rows: self.decoder.as_ref().map(|d| d.rows.state()),
            at: self.at,
            left: self.left,
            done: self.state == State::Done,
        })
    }

    /// Goes on from `state`, saved of a reader of the same data.
    #[cfg(feature = "unsafe")]
    pub(crate) fn restore(&mut self, state: &CcittState) {
        let Encoded::Plain(data) = self.data else {
            return;
        };
        self.decoder = state.rows.as_ref().map(|saved| {
            let mut rows = RowDecoder::new(data, self.setup.settings);
            rows.resume(saved);
            Decoding {
                rows,
                through: None,
            }
        });
        (self.at, self.left) = (state.at, state.left);
        self.state = if state.done { State::Done } else { State::Row };
        self.stopped = false;
    }

    /// The most heap bytes a saved state holds.
    #[cfg(feature = "unsafe")]
    pub(crate) fn heap_bound(&self) -> usize {
        RowState::heap_bound(&self.setup.settings)
    }

    /// Back to the start of the data.
    pub(crate) fn rewind(&mut self) {
        self.decoder = None;
        self.at = 0;
        self.left = 0;
        self.state = State::Row;
        self.stopped = false;
    }

    /// Fills `out` with the next bytes and returns how many, 0 only at the end. The
    /// decoding counts on `meter` a step per bit read and per byte decrypted, and
    /// the bytes made; once its hook asked to stop, reads fail until a rewind.
    pub(crate) fn read(&mut self, out: &mut [u8], meter: &Meter<'_>) -> Result<usize, Cancelled> {
        let mut filled = 0;
        while filled < out.len() {
            let n = (out.len() - filled).min(self.left);
            if n > 0 {
                let piece = &mut out[filled..filled + n];
                piece.fill(self.setup.white);
                let spans = self.write_row(piece);
                self.taken(n);
                meter.work(n + spans)?;
                filled += n;
                continue;
            }
            if !self.next_row(meter)? {
                break;
            }
        }
        Ok(filled)
    }

    /// Moves past the next `n` bytes as [`Self::read`] reads them, without making
    /// them; returns how many the data held. Only the decoding counts on `meter`.
    pub(crate) fn skip(&mut self, n: usize, meter: &Meter<'_>) -> Result<usize, Cancelled> {
        let mut skipped = 0;
        while skipped < n {
            let k = (n - skipped).min(self.left);
            if k > 0 {
                self.taken(k);
                skipped += k;
                continue;
            }
            if !self.next_row(meter)? {
                break;
            }
        }
        Ok(skipped)
    }

    /// Notes `n` more bytes of the row read.
    fn taken(&mut self, n: usize) {
        self.at += n;
        self.left -= n;
    }

    /// The bytes [`decode`] makes, appended to `output`, room made for each piece as
    /// [`reserve`] makes it towards `declared` bytes: the decoding counted on `meter`,
    /// `None` once its hook asks to stop.
    fn read_to_end(
        &mut self,
        output: &mut Vec<u8>,
        declared: usize,
        meter: &Meter<'_>,
    ) -> Option<()> {
        loop {
            let n = self.left.min(POLL_BYTES);
            if n > 0 {
                let len = output.len();
                reserve(output, len + n, declared);
                output.resize(len + n, self.setup.white);
                let spans = self.write_row(&mut output[len..]);
                self.taken(n);
                meter.work(n + spans).ok()?;
                continue;
            }
            if !self.next_row(meter).ok()? {
                return Some(());
            }
        }
    }

    /// Writes the next bytes of the row being read over `out`, which is white; returns
    /// the black spans written.
    fn write_row(&self, out: &mut [u8]) -> usize {
        let (State::Row, Some(decoding)) = (self.state, &self.decoder) else {
            return 0;
        };
        let changes = decoding.rows.last_row();
        let columns = self.setup.settings.columns as usize;
        if self.packed {
            black_bits(changes, columns, self.at, out)
        } else {
            black_bytes(changes, columns, self.at, !self.setup.white, out)
        }
    }

    /// Moves to the next row decoded; false at the end.
    fn next_row(&mut self, meter: &Meter<'_>) -> Result<bool, Cancelled> {
        if self.stopped {
            return Err(Cancelled);
        }
        let row_len = self.row_len();
        (self.at, self.left) = (0, 0);
        match self.state {
            // A row of no bytes ends the data as it is read.
            _ if row_len == 0 => {
                self.state = State::Done;
                return Ok(false);
            }
            State::Done => return Ok(false),
            State::Row => {}
        }
        let decoding = match &mut self.decoder {
            Some(decoding) => decoding,
            None => match start(&self.data, self.setup.settings, meter) {
                Ok(decoding) => self.decoder.insert(decoding),
                Err(Cancelled) => {
                    self.stopped = true;
                    return Err(Cancelled);
                }
            },
        };
        let result = loop {
            // The decoder as it was, for a row to decode again once more data are
            // ready.
            let feed = decoding.through.as_deref().and_then(Through::feed);
            let saved = match feed {
                Some(feed) => {
                    if feed.prepare(decoding.rows.byte_position(), meter).is_err() {
                        self.stopped = true;
                        return Err(Cancelled);
                    }
                    Some(decoding.rows.clone())
                }
                None => None,
            };
            let mut sink = Sink {
                through: decoding.through.as_deref(),
                meter,
            };
            let result = decoding.rows.next_row(&mut sink);
            let decrypted = sink.decrypted();
            if meter.work(decrypted).is_err() || meter.cancelled() {
                self.stopped = true;
                return Err(Cancelled);
            }
            match (saved, feed) {
                (Some(saved), Some(feed)) if feed.read_past() => decoding.rows = saved,
                _ => break result,
            }
        };
        // The rows decoded before data that breaks off are kept, as those before its
        // end are.
        if let Ok(true) = result {
            self.left = row_len;
            return Ok(true);
        }
        self.state = State::Done;
        Ok(false)
    }
}

/// Starts decoding `data`: plain data from its bytes, encrypted data through a window,
/// decoded data through a feed, its first bytes made ready with `meter` (the
/// decoder reads some as it starts).
fn start<'a>(
    data: &Encoded<'a>,
    settings: DecodeSettings,
    meter: &Meter<'_>,
) -> Result<Decoding<'a>, Cancelled> {
    Ok(match data {
        Encoded::Plain(bytes) => Decoding {
            rows: RowDecoder::new(bytes, settings),
            through: None,
        },
        Encoded::Decoded(through) => {
            let feed = through.feed();
            if let Some(feed) = feed {
                feed.restart();
            }
            // The decoder reads the first bytes as it is made: made again until they
            // are ready, so that no decoder that read past them is kept.
            loop {
                if let Some(feed) = feed {
                    feed.prepare(0, meter)?;
                }
                let rows = RowDecoder::from_source(through.clone(), settings);
                if !feed.is_some_and(Feed::read_past) {
                    break Decoding {
                        rows,
                        through: Some(through.clone()),
                    };
                }
            }
        }
        Encoded::Encrypted(reader) => {
            let reader = Seekable::new(DecryptedReader::clone(reader));
            let window = Rc::new(Through::Window(Window {
                state: RefCell::new(WindowState {
                    reader,
                    bytes: Vec::new(),
                    start: 0,
                }),
                decrypted: Cell::new(0),
            }));
            Decoding {
                rows: RowDecoder::from_source(window.clone(), settings),
                through: Some(window),
            }
        }
    })
}

/// Counts the bits the decoder reads and the bytes decrypted on a meter; the rows it
/// pushes are read from it ([`RowDecoder::last_row`]).
struct Sink<'s, 'a, 'm> {
    through: Option<&'s Through<'a>>,
    meter: &'m Meter<'m>,
}

impl Sink<'_, '_, '_> {
    /// The bytes decrypted since this was last asked.
    fn decrypted(&self) -> usize {
        match self.through {
            Some(Through::Window(window)) => window.decrypted.take(),
            _ => 0,
        }
    }
}

impl Decoder for Sink<'_, '_, '_> {
    fn push_row(&mut self, _: &[u32]) {}

    /// A bit read counts a step, as a code takes at least one.
    fn stop(&mut self, bits: usize) -> bool {
        self.meter.work(bits + self.decrypted()).is_err()
    }
}

/// Decrypted bytes the CCITT decoder reads behind where it reads: it goes back to the
/// start of a damaged line, which this holds for lines of up to this many encoded
/// bytes; for a line longer than that, decryption goes back to its start
/// ([`Seekable::back_to`]).
const WINDOW: usize = 64 * 1024;

/// The bytes decrypted at a time.
const DECRYPT_CHUNK: usize = 8 * 1024;

/// Encrypted CCITT data, decrypted as the decoder reads it, holding at most twice
/// [`WINDOW`] bytes of it and a chunk.
struct Window<'a> {
    state: RefCell<WindowState<'a>>,
    /// The bytes decrypted, not yet counted on a meter.
    decrypted: Cell<usize>,
}

struct WindowState<'a> {
    reader: Seekable<'a>,
    /// Decrypted bytes, the first at `start` in the data.
    bytes: Vec<u8>,
    start: usize,
}

impl Source for Window<'_> {
    fn read_at(&self, pos: usize, out: &mut [u8]) -> usize {
        let mut state = self.state.borrow_mut();
        let end = pos + out.len();
        if pos < state.start {
            state.start = state.reader.back_to(pos);
            state.bytes.clear();
        }
        while state.start + state.bytes.len() < end {
            if state.bytes.len() >= 2 * WINDOW {
                let drop = state.bytes.len() - WINDOW;
                state.bytes.drain(..drop);
                state.start += drop;
            }
            let held = state.bytes.len();
            state.bytes.resize(held + DECRYPT_CHUNK, 0);
            let state = &mut *state;
            let n = state.reader.read(&mut state.bytes[held..]);
            state.bytes.truncate(held + n);
            if n == 0 {
                break;
            }
            self.decrypted.set(self.decrypted.get() + n);
        }
        let held = &state.bytes[(pos - state.start).min(state.bytes.len())..];
        let n = held.len().min(out.len());
        out[..n].copy_from_slice(&held[..n]);
        n
    }
}

/// CCITT data other filters make, read from them as the decoder reads it. Reading
/// them counts on a meter, which the decoder's reads of a source do not carry: so
/// before each row the bytes from where its decoding starts on are made ready, a
/// window's worth past it, and a row whose decoding reads past them is decoded again
/// from where it started, with twice as many ready (a decoder that read past them
/// took their end for the data's, so it is not kept). Decoding reads no byte before
/// where a row's decoding starts, so the feed keeps those of the row decoded last, a
/// window and a chunk, of which a long run of zero bytes, the fill a Group 3 line
/// may be padded with however long, takes no room ([`Kept`]).
struct Feed<'a> {
    state: RefCell<FeedState<'a>>,
    /// The bytes made ready past where a row's decoding starts.
    ahead: Cell<usize>,
    /// The bytes made ready for a row at first.
    least: Cell<usize>,
    /// Whether the decoder read past the bytes ready since this was last asked.
    short: Cell<bool>,
    /// The first byte the decoder read before the bytes held, where it did, which a
    /// decoder keeping to its contract does not.
    before: Cell<Option<usize>>,
}

struct FeedState<'a> {
    reader: DecodedReader<'a>,
    /// Decoded bytes, the first at `start` in the data.
    bytes: Kept,
    start: usize,
    /// Whether `bytes` reach the end of the data.
    ended: bool,
}

impl Feed<'_> {
    /// Back to the start of the data.
    fn restart(&self) {
        let mut state = self.state.borrow_mut();
        state.reader.rewind();
        state.bytes.clear();
        state.start = 0;
        state.ended = false;
        self.short.set(false);
        self.before.set(None);
    }

    /// Whether the decoder read past the bytes ready, or before them, since this was
    /// last asked: then more are made ready for the row's next decoding, else as many
    /// as at first for the next row.
    fn read_past(&self) -> bool {
        let short = self.short.take();
        self.ahead.set(match short {
            true => self.ahead.get().saturating_mul(2),
            false => self.least.get(),
        });
        short
    }

    /// Makes the bytes from `from` on ready, [`Self::ahead`] of them or up to the
    /// end, reading the filters before with `meter`, and drops those before `from`.
    fn prepare(&self, from: usize, meter: &Meter<'_>) -> Result<(), Cancelled> {
        let from = match self.before.take() {
            Some(before) => before.min(from),
            None => from,
        };
        let mut state = self.state.borrow_mut();
        let state = &mut *state;
        if from < state.start {
            state.reader.rewind();
            state.bytes.clear();
            state.ended = false;
            state.start = state.reader.skip(from, meter)?;
        }
        // Dropped a window at a time, so that the bytes held move once per window.
        let gone = (from - state.start).min(state.bytes.len());
        if gone >= WINDOW {
            state.bytes.drop_front(gone);
            state.start += gone;
        }
        let end = from.saturating_add(self.ahead.get());
        let mut chunk = [0; DECRYPT_CHUNK];
        while !state.ended && state.start + state.bytes.len() < end {
            let wanted = end - (state.start + state.bytes.len());
            let read = state
                .reader
                .read(&mut chunk[..wanted.min(DECRYPT_CHUNK)], meter)?;
            state.bytes.extend(&chunk[..read]);
            state.ended = read == 0;
        }
        Ok(())
    }
}

impl Source for Feed<'_> {
    fn read_at(&self, pos: usize, out: &mut [u8]) -> usize {
        let state = self.state.borrow();
        let Some(at) = pos.checked_sub(state.start) else {
            self.before.set(Some(pos));
            self.short.set(true);
            return 0;
        };
        let n = state.bytes.copy(at, out);
        if n < out.len() && !state.ended {
            self.short.set(true);
        }
        n
    }
}

/// The fewest zero bytes in a row that [`Kept`] keeps as their number.
const ZERO_RUN: usize = 64;

/// Bytes kept from a position on, a run of at least [`ZERO_RUN`] zero bytes kept as
/// its length: so fill, which carries no data, takes no room however long it is.
#[derive(Default)]
struct Kept {
    /// The bytes, without those of the runs.
    bytes: Vec<u8>,
    /// The runs left out of `bytes`, in order.
    runs: Vec<Run>,
    /// The bytes kept, those of the runs included.
    len: usize,
    /// The zero bytes `bytes` ends with, after its last run.
    zeros: usize,
}

#[derive(Clone, Copy)]
struct Run {
    /// The byte of `Kept::bytes` the run comes before.
    at: usize,
    /// Its first byte, and its zero bytes.
    from: usize,
    len: usize,
}

impl Kept {
    fn len(&self) -> usize {
        self.len
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Appends `data`.
    fn extend(&mut self, mut data: &[u8]) {
        while let Some((&first, _)) = data.split_first() {
            let n = data
                .iter()
                .position(|&b| (b == 0) != (first == 0))
                .unwrap_or(data.len());
            let (piece, rest) = data.split_at(n);
            data = rest;
            self.len += n;
            if first != 0 {
                self.bytes.extend_from_slice(piece);
                self.zeros = 0;
                continue;
            }
            match self.runs.last_mut() {
                Some(run) if run.at == self.bytes.len() => run.len += n,
                _ if self.zeros + n >= ZERO_RUN => {
                    self.bytes.truncate(self.bytes.len() - self.zeros);
                    self.runs.push(Run {
                        at: self.bytes.len(),
                        from: self.len - n - self.zeros,
                        len: self.zeros + n,
                    });
                    self.zeros = 0;
                }
                _ => {
                    self.bytes.extend_from_slice(piece);
                    self.zeros += n;
                }
            }
        }
    }

    /// Copies the bytes from `at` on into `out`; returns how many it keeps there.
    fn copy(&self, at: usize, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len.saturating_sub(at));
        let mut done = 0;
        // The runs that end after `at`, and the zero bytes of those before them.
        let mut next = self.runs.partition_point(|r| r.from + r.len <= at);
        while done < n {
            let pos = at + done;
            let (gone, run) = self.before(next);
            match run {
                Some(run) if run.from <= pos => {
                    let k = (run.from + run.len - pos).min(n - done);
                    out[done..done + k].fill(0);
                    done += k;
                    next += 1;
                }
                _ => {
                    let i = pos - gone;
                    let end = run.map_or(self.bytes.len(), |r| r.at);
                    let k = (end - i).min(n - done);
                    out[done..done + k].copy_from_slice(&self.bytes[i..i + k]);
                    done += k;
                }
            }
        }
        n
    }

    /// The zero bytes of the runs before run `i`, and run `i` if there is one.
    fn before(&self, i: usize) -> (usize, Option<Run>) {
        let gone = match i.checked_sub(1).map(|j| self.runs[j]) {
            Some(r) => r.from + r.len - r.at,
            None => 0,
        };
        (gone, self.runs.get(i).copied())
    }

    /// Drops the first `n` bytes kept.
    fn drop_front(&mut self, n: usize) {
        let n = n.min(self.len);
        let i = self.runs.partition_point(|r| r.from + r.len <= n);
        let (gone, run) = self.before(i);
        // Where byte `n` lies: in run `i`, or among the bytes before it.
        let cut = match run {
            Some(run) if run.from <= n => run.at,
            _ => n - gone,
        };
        self.bytes.drain(..cut);
        self.runs.drain(..i);
        for run in &mut self.runs {
            run.at -= cut;
            if run.from < n {
                run.len -= n - run.from;
                run.from = n;
            }
            run.from -= n;
        }
        self.len -= n;
        self.zeros = self.zeros.min(self.bytes.len());
    }
}

/// What [`decode`] reports of the samples it decodes with filter parameters `params`,
/// from the parameters alone.
pub(crate) fn image_data(params: &Dict<'_>, image_params: &ImageDecodeParams) -> ImageData {
    Setup::new(params, image_params).image_data(image_params.is_indexed)
}

/// The data decoded, its work counted on `meter`; `None` once its hook asks to stop.
pub(crate) fn decode(
    data: &[u8],
    params: &Dict<'_>,
    image_params: &ImageDecodeParams,
    meter: &Meter<'_>,
) -> Option<FilterResult<'static>> {
    let setup = Setup::new(params, image_params);
    let mut bytes = CcittBytes::of(Encoded::Plain(data), setup, image_params.is_indexed);
    let mut image_data = setup.image_data(bytes.packed);
    let declared = bytes.row_len().checked_mul(setup.settings.rows as usize)?;
    let mut decoded = Vec::new();
    bytes.read_to_end(&mut decoded, declared, meter)?;
    if image_data.height == 0 {
        image_data.height = decoded.len().checked_div(bytes.row_len()).unwrap_or(0) as u32;
    }
    Some(FilterResult {
        data: Cow::Owned(decoded),
        image_data: Some(image_data),
    })
}

/// The black spans of a row of `columns` pixels whose colour changes at `changes`
/// (black from the first, white from the second, and so on), clipped to pixels
/// `from..to`.
fn black_spans(
    changes: &[u32],
    columns: usize,
    from: usize,
    to: usize,
) -> impl Iterator<Item = (usize, usize)> + '_ {
    // The changes at or before `from`: past a black one, `from` lies in its span.
    let first = changes.partition_point(|&c| c as usize <= from) / 2;
    changes[2 * first..]
        .chunks(2)
        .map(move |span| {
            (
                span[0] as usize,
                span.get(1).map_or(columns, |&e| e as usize),
            )
        })
        .take_while(move |&(start, _)| start < to)
        .map(move |(start, end)| (start.max(from), end.min(to)))
}

/// Writes `black` over the black pixels among bytes `from..` of an 8-bit row, into
/// `out`; returns the spans written.
fn black_bytes(changes: &[u32], columns: usize, from: usize, black: u8, out: &mut [u8]) -> usize {
    let mut spans = 0;
    for (start, end) in black_spans(changes, columns, from, from + out.len()) {
        fill_short(&mut out[start - from..end - from], black);
        spans += 1;
    }
    spans
}

/// Flips the bits of the black pixels among bytes `from..` of a row of a bit a pixel,
/// into `out`, and clears those after the row's last pixel; returns the spans written.
fn black_bits(changes: &[u32], columns: usize, from: usize, out: &mut [u8]) -> usize {
    let to = (8 * (from + out.len())).min(columns);
    let mut spans = 0;
    for (start, end) in black_spans(changes, columns, 8 * from, to) {
        let (first, last) = (start / 8 - from, (end - 1) / 8 - from);
        let first_mask = 0xFF_u8 >> (start % 8);
        let last_mask = 0xFF_u8 << (7 - (end - 1) % 8);
        if first == last {
            out[first] ^= first_mask & last_mask;
        } else {
            out[first] ^= first_mask;
            out[last] ^= last_mask;
            out[first + 1..last].iter_mut().for_each(|b| *b = !*b);
        }
        spans += 1;
    }
    if from + out.len() == columns.div_ceil(8)
        && let Some(last) = out.last_mut()
    {
        *last &= 0xFF_u8 << ((8 - columns % 8) % 8);
    }
    spans
}

#[inline(always)]
fn fill_short<T: Copy>(slice: &mut [T], value: T) {
    let len = slice.len();

    // Most spans are short, and for them a `memset` call costs more than the
    // stores themselves. Overlapping fixed-size stores cover every length up
    // to 32.
    match len {
        0 => {}
        1..4 => {
            slice[0] = value;
            slice[len / 2] = value;
            slice[len - 1] = value;
        }
        4..8 => {
            slice[..4].copy_from_slice(&[value; 4]);
            slice[len - 4..].copy_from_slice(&[value; 4]);
        }
        8..16 => {
            slice[..8].copy_from_slice(&[value; 8]);
            slice[len - 8..].copy_from_slice(&[value; 8]);
        }
        16..=32 => {
            slice[..16].copy_from_slice(&[value; 16]);
            slice[len - 16..].copy_from_slice(&[value; 16]);
        }
        _ => slice.fill(value),
    }
}

#[cfg(test)]
mod tests {
    use super::{Seekable, Source, WINDOW, Window, WindowState, decode};
    use crate::crypto::{DecryptionTarget, Decryptor};
    use crate::object::FromBytes;
    use crate::object::ObjectIdentifier;
    use crate::object::dict::Dict;
    use crate::object::stream::ImageDecodeParams;
    use crate::work::Meter;

    /// The bytes CCITT data in ASCII hex decode to, read through the feed with `ahead`
    /// bytes made ready before each row, in pieces of `chunk` bytes.
    fn through_a_feed(hex: &[u8], params: &Dict<'_>, ahead: usize, chunk: usize) -> Vec<u8> {
        use super::{CcittBytes, Encoded};
        use crate::crypto::reader::DecryptedReader;
        use crate::filter::reader::{Codec, DecodedReader};
        let reader = DecodedReader::decoding(
            DecryptedReader::raw(alloc::borrow::Cow::Borrowed(hex)),
            &[Codec::AsciiHex],
        )
        .unwrap();
        let mut bytes = CcittBytes::decoded(reader, params, &ImageDecodeParams::default());
        if let Encoded::Decoded(through) = &bytes.data {
            let feed = through.feed().unwrap();
            feed.ahead.set(ahead);
            feed.least.set(ahead);
        }
        let mut out = Vec::new();
        let mut buf = alloc::vec![0; chunk];
        loop {
            match bytes.read(&mut buf, &Meter::default()).unwrap() {
                0 => return out,
                n => out.extend_from_slice(&buf[..n]),
            }
        }
    }

    /// Group 3 and 4 data, with lines damaged and cut short, which the decoder reads
    /// again from a line's start, and cut after a sample of bytes, read through the
    /// feed with a byte made ready before each row, so that rows are decoded again
    /// as they read past it, and with a window: as decoding the data whole does.
    #[test]
    fn a_feed_reads_as_whole_decoding() {
        let streams: [(&str, &[u8]); 5] = [
            (
                "/K -1",
                include_bytes!("../../../hayro-ccitt/tests/streams/g4.bin"),
            ),
            (
                "/K 0 /EndOfLine true",
                include_bytes!("../../../hayro-ccitt/tests/streams/g3_1d_corrupt_line.bin"),
            ),
            (
                "/K 0 /EndOfLine true",
                include_bytes!("../../../hayro-ccitt/tests/streams/g3_1d_truncated_line.bin"),
            ),
            (
                "/K 1 /EndOfLine true",
                include_bytes!("../../../hayro-ccitt/tests/streams/g3_2d_truncated_line.bin"),
            ),
            (
                "/K 0",
                include_bytes!("../../../hayro-ccitt/tests/streams/g3_1d_long_fill.bin"),
            ),
        ];
        for (params, data) in streams {
            let dict = alloc::format!("<< {params} /Columns 211 >>");
            let params = Dict::from_bytes(dict.as_bytes()).unwrap();
            for cut in (0..=data.len()).step_by(37).chain([data.len()]) {
                let data = &data[..cut];
                let whole = decode(
                    data,
                    &params,
                    &ImageDecodeParams::default(),
                    &Meter::default(),
                )
                .unwrap()
                .data
                .into_owned();
                let hex: Vec<u8> = data
                    .iter()
                    .flat_map(|b| alloc::format!("{b:02x}").into_bytes())
                    .collect();
                for (ahead, chunk) in [(1, 211), (WINDOW, 4096)] {
                    let read = through_a_feed(&hex, &params, ahead, chunk);
                    assert!(read == whole, "{dict}: cut after {cut}, {ahead} ahead");
                }
            }
        }
    }

    /// Bytes kept with their long zero runs as lengths read back as kept, from any
    /// position after appends of any size and drops from the front, and a run of a
    /// million zero bytes takes no room.
    #[test]
    fn kept_bytes_read_back_as_appended() {
        use super::{Kept, ZERO_RUN};
        let mut state = 7_u32;
        let mut next = |n: u32| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state % n
        };
        let mut kept = Kept::default();
        let mut plain: Vec<u8> = Vec::new();
        for _ in 0..3000 {
            match next(10) {
                0..=5 => {
                    let len = next(3) as usize * ZERO_RUN / 2 + next(40) as usize;
                    let zero = next(3) > 0;
                    let piece: Vec<u8> = (0..len)
                        .map(|_| if zero { 0 } else { next(3) as u8 })
                        .collect();
                    kept.extend(&piece);
                    plain.extend_from_slice(&piece);
                }
                6 => {
                    let n = next(plain.len() as u32 + 1) as usize;
                    kept.drop_front(n);
                    plain.drain(..n);
                }
                _ => {}
            }
            assert_eq!(kept.len(), plain.len());
            for _ in 0..4 {
                let at = next(plain.len() as u32 + 2) as usize;
                let mut out = [0xAA; 8];
                let wanted = 1 + next(8) as usize;
                let n = kept.copy(at, &mut out[..wanted]);
                let expected = plain.get(at..).unwrap_or_default();
                assert_eq!(n, wanted.min(expected.len()));
                assert_eq!(out[..n], expected[..n]);
            }
        }
        kept.clear();
        kept.extend(&[1]);
        kept.extend(&alloc::vec![0; 1 << 20]);
        kept.extend(&[2]);
        assert!(kept.bytes.capacity() < 4 * ZERO_RUN && kept.len() == (1 << 20) + 2);
        let mut out = [0xAA; 3];
        assert_eq!(kept.copy((1 << 20) - 1, &mut out), 3);
        assert_eq!(out, [0, 0, 2]);
    }

    #[test]
    fn issue1258() {
        let params = Dict::from_bytes(b"<< /K 0 /Columns 8 /Rows 1 >>").unwrap();

        let decoded = decode(
            &[0x35, 0x14],
            &params,
            &ImageDecodeParams::default(),
            &Meter::default(),
        )
        .unwrap();

        assert_eq!(decoded.data.as_ref(), &[0; 8]);
        assert_eq!(decoded.image_data.unwrap().height, 1);
    }

    #[test]
    fn unknown_rows() {
        let params = Dict::from_bytes(b"<< /K 0 /Columns 8 >>").unwrap();

        let decoded = decode(
            &[0x35, 0x14],
            &params,
            &ImageDecodeParams::default(),
            &Meter::default(),
        )
        .unwrap();
        assert_eq!(decoded.data.as_ref(), &[0; 8]);
        assert_eq!(decoded.image_data.unwrap().height, 1);

        let image_params = ImageDecodeParams {
            is_indexed: true,
            ..Default::default()
        };
        let decoded = decode(&[0x35, 0x14], &params, &image_params, &Meter::default()).unwrap();
        assert_eq!(decoded.data.as_ref(), &[0]);
        assert_eq!(decoded.image_data.unwrap().height, 1);
    }

    /// Writing the black spans of a row counts on the meter, however short they are.
    #[cfg(all(debug_assertions, feature = "std"))]
    #[test]
    fn short_spans_count() {
        use crate::work::TOLD;

        // A row of alternating white and black pixels in horizontal mode: `001`, a
        // white run of 1 (`000111`) and a black run of 1 (`010`), twice per 3 bytes.
        let pairs = 100_000;
        let data = [0x23, 0xA2, 0x3A].repeat(pairs / 2);
        let dict = format!("<< /K -1 /Columns {} /Rows 1 >>", 2 * pairs);
        let params = Dict::from_bytes(dict.as_bytes()).unwrap();
        let told = |is_indexed| {
            let image_params = ImageDecodeParams {
                is_indexed,
                ..Default::default()
            };
            TOLD.set(0);
            let decoded = decode(&data, &params, &image_params, &Meter::default()).unwrap();
            (TOLD.get(), decoded.data.into_owned())
        };
        let (luma, data) = told(false);
        assert!(data.chunks(2).all(|pair| pair == [0xFF, 0x00]));
        let (bits, data) = told(true);
        assert!(data.iter().all(|&byte| byte == 0xAA));
        // The row's bytes and a step per bit read, at least one per code, count anyway;
        // each black span adds one.
        assert!(luma >= 2 * pairs + pairs + pairs, "{luma}");
        assert!(bits >= 2 * pairs / 8 + pairs + pairs, "{bits}");
    }

    /// RC4 data read through a window reads as the data decrypted whole, read
    /// forward, back within the window, and back past it, holding no more than the
    /// window allows. (AES data is read through one in `tests/streaming.rs`.)
    #[test]
    fn a_window_reads_as_whole_decryption() {
        let data: Vec<u8> = (0..300_000_u32).map(|i| (i * 7 % 251) as u8).collect();
        let id = ObjectIdentifier::new(12, 0);
        {
            let decryptor = Decryptor::Rc4 {
                key: alloc::vec![13; 5],
            };
            let plain = decryptor
                .decrypt(id, &data, DecryptionTarget::Stream)
                .unwrap();
            let reader = Seekable::new(
                decryptor
                    .reader(id, &data, DecryptionTarget::Stream)
                    .unwrap(),
            );
            let window = Window {
                state: core::cell::RefCell::new(WindowState {
                    reader,
                    bytes: Vec::new(),
                    start: 0,
                }),
                decrypted: core::cell::Cell::new(0),
            };
            let len = plain.len();
            for pos in [
                0,
                5,
                3 * WINDOW,
                3 * WINDOW - 100,
                3 * WINDOW - WINDOW / 2,
                len - 3,
                WINDOW / 3,
                len - 8,
                10,
            ] {
                let mut out = [0xAA; 8];
                let n = (len - pos).min(8);
                assert_eq!(window.read_at(pos, &mut out), n, "at {pos}");
                assert_eq!(out[..n], plain[pos..pos + n], "at {pos}");
                let held = window.state.borrow().bytes.capacity();
                assert!(held <= 2 * WINDOW + super::DECRYPT_CHUNK, "{held} held");
            }
        }
    }
}
