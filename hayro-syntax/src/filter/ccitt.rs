use crate::crypto::reader::{DecryptedReader, Seekable};
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

/// White pixels a stream that ends early is padded with whatever it decoded.
const MIN_WHITE_PAD: usize = 1 << 16;

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

    /// Whether 8-bit rows left out after the `decoded` rows of data that decoded
    /// through (`complete`) are padded white: while padding them takes no more than
    /// the rows decoded (or a small image); otherwise the image ends with the rows
    /// decoded, as one whose data breaks off does.
    fn pads(&self, decoded: u32, complete: bool) -> bool {
        let (columns, rows) = (self.settings.columns as usize, self.settings.rows as usize);
        let len = decoded as usize * columns;
        let missing = (columns * rows).saturating_sub(len);
        complete && rows != 0 && missing <= len.max(MIN_WHITE_PAD)
    }
}

/// An image's CCITT data, read as the bytes [`decode`] makes of it, a piece at a time
/// as rows decode: 8-bit gray rows, or rows of a bit a pixel for an indexed image,
/// those of 8-bit data that ends early padded white as `decode` pads them. It holds
/// the decoder's state and a row's colour changes, not the rows, and of encrypted data
/// a window of it decrypted.
pub(crate) struct CcittBytes<'a> {
    data: Encoded<'a>,
    setup: Setup,
    /// Whether the rows are of a bit a pixel.
    packed: bool,
    /// Made at the first read, and again after a rewind.
    decoder: Option<Decoding<'a>>,
    /// The bytes of the row being read (the decoder's last row, or a white one) that
    /// were read, and those left.
    at: usize,
    left: usize,
    state: State,
    /// Whether the data broke off before its end.
    broken: bool,
    /// Whether a meter's hook asked to stop, after which reads fail until a rewind.
    stopped: bool,
}

/// The encoded data.
enum Encoded<'a> {
    Plain(&'a [u8]),
    /// Decrypted as it is read (boxed: a cipher's state is large).
    Encrypted(Box<DecryptedReader<'a>>),
}

/// The decoder, and the window encrypted data is read through.
struct Decoding<'a> {
    rows: RowDecoder<'a>,
    window: Option<Rc<Window<'a>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// A decoded row is being read, or none before the first.
    Row,
    /// The data ended, and white rows are read after it, this many more.
    Padding(u32),
    /// The data ended, and the bytes with it.
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

    fn of(data: Encoded<'a>, setup: Setup, packed: bool) -> Self {
        Self {
            data,
            setup,
            packed,
            decoder: None,
            at: 0,
            left: 0,
            state: State::Row,
            broken: false,
            stopped: false,
        }
    }

    /// The bytes of a row.
    fn row_len(&self) -> usize {
        self.setup.row_len(self.packed)
    }

    /// Back to the start of the data.
    pub(crate) fn rewind(&mut self) {
        self.decoder = None;
        self.at = 0;
        self.left = 0;
        self.state = State::Row;
        self.broken = false;
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

    /// Moves to the next row: one decoded, or a white one past the end of the data
    /// where `decode` pads it; false at the end.
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
            State::Padding(0) => {
                self.state = State::Done;
                return Ok(false);
            }
            State::Padding(rows) => {
                self.state = State::Padding(rows - 1);
                self.left = row_len;
                return Ok(true);
            }
            State::Row => {}
        }
        let (data, settings) = (&self.data, self.setup.settings);
        let decoding = self.decoder.get_or_insert_with(|| start(data, settings));
        let mut sink = Sink {
            window: decoding.window.as_deref(),
            meter,
        };
        let result = decoding.rows.next_row(&mut sink);
        let decrypted = sink.decrypted();
        if meter.work(decrypted).is_err() || meter.cancelled() {
            self.stopped = true;
            return Err(Cancelled);
        }
        let complete = match result {
            Ok(true) => {
                self.left = row_len;
                return Ok(true);
            }
            Ok(false) => true,
            Err(_) => false,
        };
        self.broken = !complete;
        let decoded = decoding.rows.decoded_rows();
        let rows = self.setup.settings.rows;
        // `decode` fails where the data breaks off before a row, and pads 8-bit rows.
        self.state =
            if (complete || decoded > 0) && !self.packed && self.setup.pads(decoded, complete) {
                State::Padding(rows - decoded)
            } else {
                State::Done
            };
        self.next_row(meter)
    }
}

/// Starts decoding `data`: plain data from its bytes, encrypted data through a window.
fn start<'a>(data: &Encoded<'a>, settings: DecodeSettings) -> Decoding<'a> {
    match data {
        Encoded::Plain(bytes) => Decoding {
            rows: RowDecoder::new(bytes, settings),
            window: None,
        },
        Encoded::Encrypted(reader) => {
            let reader = Seekable::new(DecryptedReader::clone(reader));
            let window = Rc::new(Window {
                len: reader.len(),
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
        self.window.map_or(0, |w| w.decrypted.take())
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
    /// The bytes the data decrypts to.
    len: usize,
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
    fn len(&self) -> usize {
        self.len
    }

    fn read_at(&self, pos: usize, out: &mut [u8]) {
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
        out[n..].fill(0);
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
    // If we decoded at least one row, let's be lenient and return what we got.
    // See also 0001763.pdf.
    if bytes.broken && decoded.is_empty() {
        return None;
    }
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
                len: reader.len(),
                state: core::cell::RefCell::new(WindowState {
                    reader,
                    bytes: Vec::new(),
                    start: 0,
                }),
                decrypted: core::cell::Cell::new(0),
            };
            assert_eq!(window.len(), plain.len());
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
                window.read_at(pos, &mut out[..n]);
                assert_eq!(out[..n], plain[pos..pos + n], "at {pos}");
                let held = window.state.borrow().bytes.capacity();
                assert!(held <= 2 * WINDOW + super::DECRYPT_CHUNK, "{held} held");
            }
        }
    }
}
