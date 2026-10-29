use crate::crypto::reader::{DecryptedReader, Seekable};
use crate::filter::reader::{ChainState, DecodedReader};
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
/// changes, not the rows, and of encrypted data or what other filters make a window
/// of it.
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
    Decoded(Box<Feed<'a>>),
}

/// The decoder, and the window encrypted data is read through.
struct Decoding<'a> {
    rows: RowDecoder<'a>,
    window: Option<Rc<Window<'a>>>,
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
        Self::of(
            Encoded::Decoded(Box::new(Feed::new(reader))),
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
    /// other data, or once a meter's hook stopped it. Copying the decoder's state
    /// counts on `meter` ([`Meter::copied`]).
    #[cfg(feature = "unsafe")]
    pub(crate) fn save(&self, meter: &Meter<'_>) -> Result<Option<CcittState>, Cancelled> {
        if self.plain().is_none() || self.stopped {
            return Ok(None);
        }
        let rows = match &self.decoder {
            Some(d) => Some(d.rows.state_with(|changes| meter.copied(changes))?),
            None => None,
        };
        Ok(Some(CcittState {
            rows,
            at: self.at,
            left: self.left,
            done: self.state == State::Done,
        }))
    }

    /// Goes on from `state`, saved of a reader of the same data, copying it on
    /// `meter`; once its hook asks to stop, it fails, with the reader unchanged.
    #[cfg(feature = "unsafe")]
    pub(crate) fn restore(
        &mut self,
        state: &CcittState,
        meter: &Meter<'_>,
    ) -> Result<(), Cancelled> {
        let Encoded::Plain(data) = self.data else {
            return Ok(());
        };
        self.decoder = match &state.rows {
            Some(saved) => {
                let mut rows = RowDecoder::new(data, self.setup.settings);
                rows.resume_with(saved, |changes| meter.copied(changes))?;
                Some(Decoding { rows, window: None })
            }
            None => None,
        };
        (self.at, self.left) = (state.at, state.left);
        self.state = if state.done { State::Done } else { State::Row };
        self.stopped = false;
        Ok(())
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
            None => self.decoder.insert(start(&self.data, self.setup.settings)),
        };
        let mut sink = Sink {
            window: decoding.window.as_deref(),
            meter,
        };
        let result = match &self.data {
            Encoded::Decoded(feed) => {
                feed.row_starts(decoding.rows.byte_position());
                decoding.rows.next_row_in(&Fed { feed, meter }, &mut sink)
            }
            _ => decoding.rows.next_row(&mut sink),
        };
        let decrypted = sink.decrypted();
        // A stop while the feed read the filters before ends the data the decoder
        // reads, and so leaves its row unfinished: it is not kept.
        if meter.work(decrypted).is_err() || meter.cancelled() {
            self.stopped = true;
            return Err(Cancelled);
        }
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
/// decoded data through a feed, from its start.
fn start<'a>(data: &Encoded<'a>, settings: DecodeSettings) -> Decoding<'a> {
    match data {
        Encoded::Plain(bytes) => Decoding {
            rows: RowDecoder::new(bytes, settings),
            window: None,
        },
        Encoded::Decoded(feed) => {
            feed.restart();
            Decoding {
                rows: RowDecoder::detached(settings),
                window: None,
            }
        }
        Encoded::Encrypted(reader) => {
            let reader = Seekable::new(DecryptedReader::clone(reader));
            let window = Rc::new(Window {
                state: RefCell::new(WindowState {
                    reader,
                    bytes: Vec::new(),
                    start: 0,
                }),
                decrypted: Cell::new(0),
            });
            Decoding {
                rows: RowDecoder::from_source(window.clone(), settings),
                window: Some(window),
            }
        }
    }
}

/// Counts the bits the decoder reads and the bytes decrypted on a meter; the rows it
/// pushes are read from it ([`RowDecoder::last_row`]).
struct Sink<'s, 'a, 'm> {
    window: Option<&'s Window<'a>>,
    meter: &'m Meter<'m>,
}

impl Sink<'_, '_, '_> {
    /// The bytes decrypted since this was last asked.
    fn decrypted(&self) -> usize {
        self.window.map_or(0, |window| window.decrypted.take())
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

/// CCITT data other filters make, read from them as the decoder reads it, the work
/// counted on the meter of the row being decoded ([`Fed`]). Decoding reads mostly
/// forward, and again from where a row's decoding starts, never before it
/// ([`RowDecoder::byte_position`]): so the feed holds the bytes from there on, up to
/// [`HOLD`] of them, and past that the last half of those before the byte read. Before
/// it drops bytes from the row's start on, it saves the filters' state and the bytes
/// it holds from there ([`Mark`]); a read before the bytes held goes back to that
/// state and decodes on from it. So a row whose code reads on however long, a long
/// fill or codes that write nothing, holds a bounded window, and is decoded once,
/// and again from its start at most once for each time the decoder goes back.
struct Feed<'a> {
    state: RefCell<FeedState<'a>>,
}

/// The bytes a feed holds at most, besides a chunk being read.
const HOLD: usize = 2 * WINDOW;

struct FeedState<'a> {
    reader: DecodedReader<'a>,
    /// Decoded bytes, the first at `start` in the data.
    bytes: Vec<u8>,
    start: usize,
    /// Whether `bytes` reach the end of the data.
    ended: bool,
    /// Where the decoding of the row being decoded started.
    row: usize,
    mark: Option<Mark>,
    /// [`HOLD`], and fewer for tests.
    hold: usize,
}

/// The filters' state where a feed was once, and the bytes it held from the start
/// of a row up to there.
struct Mark {
    state: ChainState,
    bytes: Vec<u8>,
    start: usize,
    ended: bool,
}

/// A feed lent to the decoding of a row, with the meter that counts it.
struct Fed<'f, 'a, 'm> {
    feed: &'f Feed<'a>,
    meter: &'m Meter<'m>,
}

impl Source for Fed<'_, '_, '_> {
    fn read_at(&self, pos: usize, out: &mut [u8]) -> usize {
        let mut state = self.feed.state.borrow_mut();
        // A stop ends the data, which the caller then finds on the meter.
        state.read_at(pos, out, self.meter).unwrap_or(0)
    }
}

impl<'a> Feed<'a> {
    fn new(reader: DecodedReader<'a>) -> Self {
        Self {
            state: RefCell::new(FeedState {
                reader,
                bytes: Vec::new(),
                start: 0,
                ended: false,
                row: 0,
                mark: None,
                hold: HOLD,
            }),
        }
    }

    /// Back to the start of the data.
    fn restart(&self) {
        let mut state = self.state.borrow_mut();
        state.reader.rewind();
        state.bytes.clear();
        (state.start, state.ended, state.row) = (0, false, 0);
        state.mark = None;
    }

    /// Notes that a row's decoding starts at byte `row`: a mark of a row before is
    /// dropped, where the bytes from `row` on are still held.
    fn row_starts(&self, row: usize) {
        let mut state = self.state.borrow_mut();
        state.row = row;
        if state.start <= row {
            state.mark = None;
        }
    }
}

impl FeedState<'_> {
    /// Copies the bytes from `pos` on into `out`, reading the filters before with
    /// `meter` as far as they are needed; returns how many the data hold there.
    fn read_at(
        &mut self,
        pos: usize,
        out: &mut [u8],
        meter: &Meter<'_>,
    ) -> Result<usize, Cancelled> {
        if pos < self.start {
            self.back(pos, meter)?;
        }
        let end = pos.saturating_add(out.len());
        let chunk = DECRYPT_CHUNK.min(self.hold);
        while !self.ended && self.start + self.bytes.len() < end {
            if self.bytes.len() + chunk > self.hold {
                self.drop_before(pos.saturating_sub(self.hold / 2), meter)?;
            }
            let held = self.bytes.len();
            self.bytes.resize(held + chunk, 0);
            let read = self.reader.read(&mut self.bytes[held..], meter);
            self.bytes.truncate(held + *read.as_ref().unwrap_or(&0));
            self.ended = read? == 0;
        }
        let held = self.bytes.get(pos - self.start..).unwrap_or_default();
        let n = held.len().min(out.len());
        out[..n].copy_from_slice(&held[..n]);
        Ok(n)
    }

    /// Drops the bytes before `to`, marking where the filters are first where they
    /// include bytes of the row being decoded. The mark's copying counts on `meter`
    /// ([`Meter::copied`]): the filters' state holds a predictor's row, as wide as the
    /// data declares. A stop drops nothing.
    fn drop_before(&mut self, to: usize, meter: &Meter<'_>) -> Result<(), Cancelled> {
        let to = to.clamp(self.start, self.start + self.bytes.len());
        if to > self.row && self.mark.is_none() && self.start <= self.row {
            let bytes = meter.copied(&self.bytes[self.row - self.start..])?;
            self.mark = self.reader.save(meter)?.map(|state| Mark {
                state,
                bytes,
                start: self.row,
                ended: self.ended,
            });
        }
        self.bytes.drain(..to - self.start);
        self.start = to;
        Ok(())
    }

    /// Goes back to hold byte `pos` next: to the mark where it holds it, else to the
    /// start of the data, decoding the filters again up to `pos`. Copying the mark
    /// counts on `meter`, as saving it did.
    fn back(&mut self, pos: usize, meter: &Meter<'_>) -> Result<(), Cancelled> {
        if let Some(mark) = &self.mark
            && mark.start <= pos
        {
            let bytes = meter.copied(&mark.bytes)?;
            if self.reader.load(&mark.state, meter)? {
                self.bytes = bytes;
                (self.start, self.ended) = (mark.start, mark.ended);
                return Ok(());
            }
        }
        self.reader.rewind();
        self.bytes.clear();
        self.start = self.reader.skip(pos, meter)?;
        self.ended = self.start < pos;
        Ok(())
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
    use super::{HOLD, Seekable, Source, WINDOW, Window, WindowState, decode};
    use crate::crypto::{DecryptionTarget, Decryptor};
    use crate::object::FromBytes;
    use crate::object::ObjectIdentifier;
    use crate::object::dict::Dict;
    use crate::object::stream::ImageDecodeParams;
    use crate::work::Meter;

    /// The bytes CCITT data in ASCII hex decode to, read through a feed that holds
    /// `hold` bytes, in pieces of `chunk` bytes.
    fn through_a_feed(hex: &[u8], params: &Dict<'_>, hold: usize, chunk: usize) -> Vec<u8> {
        use super::{CcittBytes, Encoded};
        use crate::crypto::reader::DecryptedReader;
        use crate::filter::reader::{Codec, DecodedReader};
        let reader = DecodedReader::decoding(
            DecryptedReader::raw(alloc::borrow::Cow::Borrowed(hex)),
            &[Codec::AsciiHex],
        )
        .unwrap();
        let mut bytes = CcittBytes::decoded(reader, params, &ImageDecodeParams::default());
        if let Encoded::Decoded(feed) = &bytes.data {
            feed.state.borrow_mut().hold = hold;
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
    /// again from a line's start, and cut after a sample of bytes, read through a
    /// feed that holds 16 bytes, so that the decoder going back reads bytes the feed
    /// dropped, and through one that holds its default: as decoding the data whole
    /// does.
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
                for (hold, chunk) in [(16, 211), (HOLD, 4096)] {
                    let read = through_a_feed(&hex, &params, hold, chunk);
                    assert!(read == whole, "{dict}: cut after {cut}, {hold} held");
                }
            }
        }
    }

    /// A feed that dropped the bytes of a row goes back to them through its mark,
    /// and, without one, by decoding the filters again from their start: either way
    /// it reads the bytes from any position the row's decoding may go back to.
    #[test]
    fn a_feed_goes_back_to_bytes_it_dropped() {
        use super::Feed;
        use crate::crypto::reader::DecryptedReader;
        use crate::filter::reader::{Codec, DecodedReader};
        let data: Vec<u8> = (0..5000_u32).map(|i| (i * 7 % 251) as u8).collect();
        let hex: Vec<u8> = data
            .iter()
            .flat_map(|b| alloc::format!("{b:02x}").into_bytes())
            .collect();
        let reader = DecodedReader::decoding(
            DecryptedReader::raw(alloc::borrow::Cow::Borrowed(&hex)),
            &[Codec::AsciiHex],
        )
        .unwrap();
        let feed = Feed::new(reader);
        let meter = Meter::default();
        for marked in [true, false] {
            feed.restart();
            feed.state.borrow_mut().hold = 64;
            feed.row_starts(1000);
            for pos in (1000..4000).step_by(4) {
                let mut out = [0; 8];
                let n = feed.state.borrow_mut().read_at(pos, &mut out, &meter);
                assert_eq!(n, Ok(8));
                assert_eq!(out, data[pos..pos + 8]);
            }
            {
                let mut state = feed.state.borrow_mut();
                assert!(state.start > 1000 && state.bytes.capacity() < 256);
                assert!(state.mark.is_some());
                if !marked {
                    state.mark = None;
                }
            }
            for pos in [1000, 1001, 2345, 4990, 1500] {
                let mut out = [0xAA; 8];
                let n = feed.state.borrow_mut().read_at(pos, &mut out, &meter);
                let expected = &data[pos..(pos + 8).min(data.len())];
                assert_eq!(n, Ok(expected.len()), "back to {pos}, marked {marked}");
                assert_eq!(out[..expected.len()], *expected, "back to {pos}");
            }
        }
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
