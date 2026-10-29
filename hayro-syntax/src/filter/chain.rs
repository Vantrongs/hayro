//! A stream's data decoded through all its filters a piece at a time, as
//! [`Stream::decoded_strict`](crate::object::Stream::decoded_strict) decodes them
//! whole, in memory bounded by the decoders' state, windows and buffers, however
//! much the filters make of the data and however many there are.
//!
//! The filters are a flat list of [`Layer`]s. Each but the last is decoded in turn
//! into a buffer while what it makes is at most [`HELD`] bytes, as whole decoding
//! decodes it; one that makes more is streamed instead: the layers above read it as
//! it decodes. At most [`LIVE`] layers below the last are streamed at once; past
//! that a layer is held whatever it makes. So the stages alive at once, the stack a
//! read goes down and the memory of their windows do not grow with the number of
//! filters, and a layer whose output is held decides its decoder before the layers
//! above read any of it.
//!
//! Each filter is a [`Stage`] that reads the one before it. A Flate stage that finds
//! its data corrupt for the decoder it uses goes on with the next one whole decoding
//! would try (zlib, raw deflate, then the permissive decoder), from the start: it
//! rewinds the stages before it and voids what it handed out
//! ([`ReadError::Restarted`]), and the stages after it start over, their own
//! decoders chosen anew, as whole decoding chooses them for data it decoded anew.
//! A stage that ends, fails or restarts reads the stages before it to their end
//! first, as whole decoding decodes them whole first: one of them may yet fail or
//! restart, which voids what the stage made of their data.
//!
//! A restart costs at most one more decoding of the layers streamed below the one
//! restarting and of those above it. A stage restarts only once the stages before it
//! read to their end with no restart, so their decoders are final, and its own new
//! decoder is too: a layer streamed restarts at most twice, and a run of at most
//! [`LIVE`] streamed layers and the one over them restarts at most `2 * LIVE + 2`
//! times, however its layers are damaged.

use crate::filter::lzw_flate::flate::fallback::{FlateStream, Input};
use crate::filter::lzw_flate::{PredictorParams, Unpredictor, flate, lzw};
use crate::filter::reader::ReadError;
use crate::filter::{ascii_85, ascii_hex, run_length};
use crate::trivia::is_white_space_character;
use crate::work::{Meter, POLL_BYTES};
use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::vec::Vec;

/// The most bytes a layer below the last one makes and still has them held, as the
/// writer holds a stream it reads to its end: past that, streaming the layer, its
/// state, window and buffers about a tenth of that, saves memory.
const HELD: usize = 1 << 20;

/// The most layers below the last one streamed at once: their decoders' state and
/// buffers, about a tenth of [`HELD`] each, take about twice as much as holding one,
/// and each restart of one decodes them all again.
const LIVE: usize = 16;

/// The bytes the permissive Flate decoder makes at a time in a stage, which it holds
/// besides its window.
const PERMISSIVE_STEP: usize = 16 * 1024;

/// A filter of the chain, its predictor reversed after it where it has one, by an
/// [`Unpredictor`] where a stage reverses it (boxed: a long chain holds many layers
/// with none).
pub(crate) enum Layer {
    Flate(PredictorParams, Option<Box<Unpredictor>>),
    Lzw(PredictorParams, Option<Box<Unpredictor>>),
    AsciiHex,
    Ascii85,
    RunLength,
}

impl Layer {
    /// `data` decoded whole, as [`Stream::decoded_strict`] decodes the layer, where it
    /// makes at most `limit` bytes; else `Err` with the decoders that found the data
    /// corrupt before the one that made more.
    ///
    /// [`Stream::decoded_strict`]: crate::object::Stream::decoded_strict
    fn decode(
        &self,
        data: &[u8],
        meter: &Meter<'_>,
        limit: usize,
    ) -> Result<Result<Vec<u8>, ReadError>, usize> {
        let decoded = match self {
            Self::Flate(params, _) => {
                flate::decode_within(data, params, meter, true, 0, limit).map_err(|l| l.tried)?
            }
            Self::Lzw(params, _) => {
                lzw::decode_within(data, params, meter, limit).map_err(|()| 0_usize)?
            }
            // Each makes at most as many bytes as half, four times and 64 times the data.
            Self::AsciiHex => ascii_hex::decode(data, meter),
            Self::Ascii85 if data.len().saturating_mul(4) <= limit => ascii_85::decode(data, meter),
            Self::RunLength if data.len().saturating_mul(64) <= limit => {
                run_length::decode(data, meter)
            }
            Self::Ascii85 | Self::RunLength => return Err(0),
        };
        Ok(match (decoded, meter.cancelled()) {
            (_, true) => Err(ReadError::Cancelled),
            (Some(decoded), false) => Ok(decoded),
            (None, false) => Err(ReadError::Failed),
        })
    }

    /// The layer's stage, reading `from`, its decoder past the first `tried` whole
    /// decoding would try, which found the data corrupt.
    fn stage<'a>(&self, from: Box<dyn Stage + 'a>, tried: usize) -> Box<dyn Stage + 'a> {
        let predicted = |stage: Box<dyn Stage + 'a>, unpredictor: &Option<Box<Unpredictor>>| {
            match unpredictor {
                None => stage,
                Some(unpredictor) => Box::new(Predict {
                    from: stage,
                    unpredictor: Unpredictor::clone(unpredictor),
                    taken: 0,
                }) as Box<dyn Stage + 'a>,
            }
        };
        match self {
            Self::Flate(_, unpredictor) => {
                predicted(Box::new(Flate::new(from, Which::after(tried))), unpredictor)
            }
            Self::Lzw(params, unpredictor) => predicted(
                Box::new(Bytewise::lzw(from, params.early_change)),
                unpredictor,
            ),
            Self::AsciiHex => Box::new(Bytewise::ascii_hex(from)),
            Self::Ascii85 => Box::new(Bytewise::ascii_85(from)),
            Self::RunLength => Box::new(Bytewise::run_length(from)),
        }
    }
}

/// The decoded data of a stream, read from its start in pieces (see
/// [`Stream::decoded_strict_reader`](crate::object::Stream::decoded_strict_reader)).
pub struct StrictReader<'a> {
    /// The stored data, decrypted.
    stored: Data<'a>,
    layers: Vec<Layer>,
    /// The last layer's stage, over the layers below it held or streamed, built at the
    /// first read.
    chain: Option<Box<dyn Stage + 'a>>,
    /// Whether bytes were read since the start or the last restart.
    handed: bool,
    /// The error that ended the reads, until a rewind.
    ended: Option<ReadError>,
}

impl<'a> StrictReader<'a> {
    pub(crate) fn new(stored: Cow<'a, [u8]>, layers: Vec<Layer>) -> Self {
        let stored = match stored {
            Cow::Borrowed(data) => Data::Borrowed(data),
            Cow::Owned(data) => Data::Shared(Rc::new(data)),
        };
        Self {
            stored,
            layers,
            chain: None,
            handed: false,
            ended: None,
        }
    }

    /// Fills `buf` with the next decoded bytes and returns how many: at most
    /// [`POLL_BYTES`], 0 only at the end or for an empty `buf`.
    /// [`ReadError::Restarted`] voids every byte read before, and the next read
    /// returns the data from their start; the bytes read after the last restart up to
    /// the end are those `decoded_strict` returns. [`ReadError::Failed`] where
    /// `decoded_strict` fails to decode them, after which every read fails, as it does
    /// once `meter`'s hook asked to stop ([`ReadError::Cancelled`]). The work counts
    /// on `meter`, a bounded amount between two polls of its hook.
    pub fn read(&mut self, buf: &mut [u8], meter: &Meter<'_>) -> Result<usize, ReadError> {
        if let Some(error) = self.ended {
            return Err(error);
        }
        let len = buf.len().min(POLL_BYTES);
        if len == 0 {
            return Ok(0);
        }
        let result = match meter.cancelled() {
            false => self.read_chain(&mut buf[..len], meter),
            true => Err(ReadError::Cancelled),
        };
        // A decoder may end early on a stop as on data cut short.
        let result = match meter.cancelled() {
            false => result,
            true => Err(ReadError::Cancelled),
        };
        match result {
            Ok(n) => self.handed |= n > 0,
            Err(ReadError::Restarted) => self.handed = false,
            Err(error) => self.ended = Some(error),
        }
        result
    }

    /// [`Self::read`] from the stages, built first if they are not; a restart before
    /// any byte was read is read past.
    fn read_chain(&mut self, buf: &mut [u8], meter: &Meter<'_>) -> Result<usize, ReadError> {
        loop {
            let chain = match &mut self.chain {
                Some(chain) => chain,
                None => {
                    let chain = self.build(meter)?;
                    self.chain.insert(chain)
                }
            };
            match chain.read(buf, meter) {
                Err(ReadError::Restarted) if !self.handed => {}
                result => return result,
            }
        }
    }

    /// The stages: each layer but the last decoded whole into a buffer where it reads
    /// one and makes at most [`HELD`] bytes, else streamed, up to [`LIVE`] at once,
    /// the next one then read whole into a buffer; and the last one over them.
    fn build(&self, meter: &Meter<'_>) -> Result<Box<dyn Stage + 'a>, ReadError> {
        let mut from = Source::Held(self.stored.clone());
        let Some((last, layers)) = self.layers.split_last() else {
            return Ok(from.stage());
        };
        let mut live = 0;
        for layer in layers {
            from = match from {
                Source::Held(data) => match layer.decode(data.as_slice(), meter, HELD) {
                    Ok(decoded) => Source::Held(Data::Held(decoded?)),
                    Err(tried) => {
                        live = 1;
                        Source::Live(layer.stage(Source::Held(data).stage(), tried))
                    }
                },
                Source::Live(stage) if live < LIVE => {
                    live += 1;
                    Source::Live(layer.stage(stage, 0))
                }
                Source::Live(stage) => {
                    let mut stage = layer.stage(stage, 0);
                    let mut out = Vec::new();
                    let mut filled = 0;
                    loop {
                        // Grown by half, so that a large output is held with at most
                        // half as much room to spare, and cleared a piece at a time as
                        // reads fill it, each byte once however few a read gives.
                        if filled == out.len() {
                            if out.capacity() - filled < POLL_BYTES {
                                out.reserve_exact((filled / 2).max(POLL_BYTES));
                            }
                            out.resize(filled + POLL_BYTES, 0);
                        }
                        let read = stage.read(&mut out[filled..], meter);
                        match read {
                            Ok(0) => break,
                            Ok(n) => filled += n,
                            // A streamed layer's restart, or this one's on what they
                            // made: the stages start over themselves.
                            Err(ReadError::Restarted) => filled = 0,
                            Err(error) => return Err(error),
                        }
                    }
                    out.truncate(filled);
                    live = 0;
                    // Drops the stages below, which made all they will.
                    Source::Held(Data::Held(out))
                }
            };
        }
        Ok(last.stage(from.stage(), 0))
    }

    /// Back to the start of the data, each decoder kept as restarts chose it, so that
    /// data read to their end once are read again with no restart.
    pub fn rewind(&mut self) {
        self.ended = None;
        self.handed = false;
        if let Some(chain) = &mut self.chain {
            chain.rewind();
        }
    }
}

/// A filter's decoder over the data the stage before it yields.
pub(crate) trait Stage {
    /// [`StrictReader::read`], without its checks of the meter: on
    /// [`ReadError::Restarted`] from the stage before it, a stage starts over with the
    /// decoder whole decoding starts with and passes it on.
    fn read(&mut self, buf: &mut [u8], meter: &Meter<'_>) -> Result<usize, ReadError>;
    /// Back to the start, the stages before it too, each keeping its decoder.
    fn rewind(&mut self);
}

/// Bytes a stage reads from their start: the stored data or a layer's held output.
#[derive(Clone)]
enum Data<'a> {
    Borrowed(&'a [u8]),
    /// Decrypted, kept for a reader that starts over.
    Shared(Rc<Vec<u8>>),
    Held(Vec<u8>),
}

impl Data<'_> {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Borrowed(data) => data,
            Self::Shared(data) => data,
            Self::Held(data) => data,
        }
    }
}

/// What a layer reads: bytes held, or the stage of a layer streamed.
enum Source<'a> {
    Held(Data<'a>),
    Live(Box<dyn Stage + 'a>),
}

impl<'a> Source<'a> {
    fn stage(self) -> Box<dyn Stage + 'a> {
        match self {
            Self::Held(data) => Box::new(Stored::new(data)),
            Self::Live(stage) => stage,
        }
    }
}

/// The stored data, decrypted, or a layer's held output.
struct Stored<'a> {
    data: Data<'a>,
    at: usize,
}

impl<'a> Stored<'a> {
    fn new(data: Data<'a>) -> Self {
        Self { data, at: 0 }
    }
}

impl Stage for Stored<'_> {
    /// Counts nothing: what reads the bytes counts those it takes.
    fn read(&mut self, buf: &mut [u8], meter: &Meter<'_>) -> Result<usize, ReadError> {
        if meter.cancelled() {
            return Err(ReadError::Cancelled);
        }
        let rest = &self.data.as_slice()[self.at..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.at += n;
        Ok(n)
    }

    fn rewind(&mut self) {
        self.at = 0;
    }
}

/// How many bytes a decoder that takes them a few at a time is given from the stage
/// before it at once.
const PULLED: usize = 8192;

/// The bytes of the stage before, for a decoder that takes them a few at a time;
/// the first error reading them ends them, kept for the stage to handle.
struct Pulled<'a> {
    from: Box<dyn Stage + 'a>,
    buf: Vec<u8>,
    /// The bytes the last read of the stage before gave.
    got: usize,
    at: usize,
    taken: usize,
    end: bool,
    error: Option<ReadError>,
}

impl<'a> Pulled<'a> {
    fn new(from: Box<dyn Stage + 'a>) -> Self {
        Self {
            from,
            buf: Vec::new(),
            got: 0,
            at: 0,
            taken: 0,
            end: false,
            error: None,
        }
    }

    /// Makes `ahead` + 1 bytes available, if there are as many.
    fn fill(&mut self, ahead: usize, meter: &Meter<'_>) -> bool {
        while self.buf.len() - self.at <= ahead {
            if self.end {
                return false;
            }
            self.buf.drain(..self.at);
            self.at = 0;
            // Room for twice what the last read gave, so that data that are few do
            // not cost a large buffer cleared each time.
            let len = self.buf.len();
            self.buf.resize(len + (2 * self.got).clamp(256, PULLED), 0);
            let read = self.from.read(&mut self.buf[len..], meter);
            self.got = *read.as_ref().unwrap_or(&0);
            self.buf.truncate(len + self.got);
            match read {
                Ok(0) => self.end = true,
                Ok(_) => {}
                Err(error) => {
                    self.end = true;
                    self.error = Some(error);
                }
            }
        }
        true
    }

    /// The bytes at hand, at least one where any is left.
    fn available(&mut self, meter: &Meter<'_>) -> &[u8] {
        self.fill(0, meter);
        &self.buf[self.at..]
    }

    fn consume(&mut self, n: usize) {
        self.at += n;
        self.taken += n;
    }

    /// Reads the rest of the bytes, counting them, so that an error reading them is
    /// found ([`Self::error`]).
    fn drain(&mut self, meter: &Meter<'_>) {
        loop {
            let n = self.available(meter).len();
            if n == 0 || meter.work(n).is_err() {
                return;
            }
            self.consume(n);
        }
    }

    /// The error that ended the bytes, if one did; after a restart of the stages
    /// before, the bytes start over.
    fn error(&mut self) -> Option<ReadError> {
        let error = self.error.take()?;
        if error == ReadError::Restarted {
            self.start_over();
        }
        Some(error)
    }

    fn start_over(&mut self) {
        self.buf.clear();
        self.at = 0;
        self.taken = 0;
        self.end = false;
        self.error = None;
    }

    fn rewind(&mut self) {
        self.from.rewind();
        self.start_over();
    }
}

impl Input for Pulled<'_> {
    #[inline]
    fn next(&mut self, meter: &Meter<'_>) -> Option<u8> {
        if self.at == self.buf.len() && !self.fill(0, meter) {
            return None;
        }
        let byte = self.buf[self.at];
        self.consume(1);
        Some(byte)
    }

    #[inline]
    fn peek(&mut self, ahead: usize, meter: &Meter<'_>) -> Option<u8> {
        if let Some(&byte) = self.buf.get(self.at + ahead) {
            return Some(byte);
        }
        self.fill(ahead, meter).then(|| self.buf[self.at + ahead])
    }

    fn taken(&self) -> usize {
        self.taken
    }

    fn take(&mut self, max: usize, meter: &Meter<'_>) -> &[u8] {
        self.fill(0, meter);
        let n = max.min(self.buf.len() - self.at);
        self.consume(n);
        &self.buf[self.at - n..self.at]
    }
}

/// Bytes a stage made and has not handed out yet.
#[derive(Default)]
struct Made {
    buf: Vec<u8>,
    handed: usize,
}

impl Made {
    fn is_empty(&self) -> bool {
        self.handed == self.buf.len()
    }

    fn hand(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.buf.len() - self.handed);
        buf[..n].copy_from_slice(&self.buf[self.handed..][..n]);
        self.handed += n;
        if self.is_empty() {
            self.clear();
        }
        n
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.handed = 0;
    }
}

/// Where a stage's decoding is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Going,
    /// At the end of the data, the stages before read to their end.
    Done,
    /// At data that cannot be decoded, the stages before read to their end.
    Failed,
}

/// What a Flate decoder found when it made bytes.
enum Found {
    More,
    End,
    /// Data corrupt for the decoder in use.
    Corrupt,
}

/// A Flate decoder, in the order whole decoding tries them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Which {
    #[cfg(feature = "unsafe")]
    Zlib,
    #[cfg(feature = "unsafe")]
    Raw,
    /// The permissive decoder, strict: data it finds corrupt fail.
    Permissive,
}

impl Which {
    fn first() -> Self {
        Self::after(0)
    }

    /// The decoder after the first `tried`.
    fn after(tried: usize) -> Self {
        #[cfg(feature = "unsafe")]
        return match tried {
            0 => Self::Zlib,
            1 => Self::Raw,
            _ => Self::Permissive,
        };
        #[cfg(not(feature = "unsafe"))]
        return {
            let _ = tried;
            Self::Permissive
        };
    }
}

/// A Flate stage.
struct Flate<'a> {
    mode: Mode<'a>,
    which: Which,
    state: State,
    made: Made,
    /// The bytes zlib-rs made at its last call.
    #[cfg(feature = "unsafe")]
    made_last: usize,
}

enum Mode<'a> {
    /// zlib-rs.
    #[cfg(feature = "unsafe")]
    Inflating {
        input: Pulled<'a>,
        decoder: zlib_rs::Inflate,
    },
    Permissive(Box<FlateStream<Pulled<'a>>>),
    /// Between two decoders.
    Switching,
}

impl<'a> Mode<'a> {
    fn new(input: Pulled<'a>, which: Which) -> Self {
        match which {
            #[cfg(feature = "unsafe")]
            Which::Zlib => Self::Inflating {
                input,
                decoder: zlib_rs::Inflate::new(true, 15),
            },
            #[cfg(feature = "unsafe")]
            Which::Raw => Self::Inflating {
                input,
                decoder: zlib_rs::Inflate::new(false, 15),
            },
            Which::Permissive => Self::Permissive(Box::new(FlateStream::new(input))),
        }
    }

    fn input(&mut self) -> &mut Pulled<'a> {
        match self {
            #[cfg(feature = "unsafe")]
            Self::Inflating { input, .. } => input,
            Self::Permissive(stream) => &mut stream.input,
            Self::Switching => unreachable!("a stage between decoders is not read"),
        }
    }

    fn into_input(self) -> Pulled<'a> {
        match self {
            #[cfg(feature = "unsafe")]
            Self::Inflating { input, .. } => input,
            Self::Permissive(stream) => stream.input,
            Self::Switching => unreachable!("a stage between decoders is not read"),
        }
    }
}

impl<'a> Flate<'a> {
    fn new(from: Box<dyn Stage + 'a>, which: Which) -> Self {
        Self {
            mode: Mode::new(Pulled::new(from), which),
            which,
            state: State::Going,
            made: Made::default(),
            #[cfg(feature = "unsafe")]
            made_last: 0,
        }
    }

    /// Starts over with decoder `which`, on the stages before rewound if `rewind`,
    /// else on those that started over themselves.
    fn start(&mut self, which: Which, rewind: bool) {
        let mut input = core::mem::replace(&mut self.mode, Mode::Switching).into_input();
        if rewind {
            input.rewind();
        }
        self.mode = Mode::new(input, which);
        self.which = which;
        self.state = State::Going;
        self.made.clear();
    }

    /// Makes more bytes, or finds the end or corrupt data.
    fn make(&mut self, meter: &Meter<'_>) -> Result<Found, ReadError> {
        match &mut self.mode {
            #[cfg(feature = "unsafe")]
            Mode::Inflating { input, decoder } => {
                // As `flate::inflate` decodes the whole data.
                let made = &mut self.made.buf;
                loop {
                    let available = input.available(meter);
                    let empty = available.is_empty();
                    let flush = if empty {
                        zlib_rs::InflateFlush::Finish
                    } else {
                        zlib_rs::InflateFlush::NoFlush
                    };
                    // Room for twice what the last call made, at least what zlib's
                    // fast loop needs.
                    let room = (2 * self.made_last).clamp(1024, POLL_BYTES);
                    made.resize(room.min(meter.until_poll().max(1)), 0);
                    let (out_before, in_before) = (decoder.total_out(), decoder.total_in());
                    let status = decoder.decompress(available, made, flush);
                    let written = (decoder.total_out() - out_before) as usize;
                    let took = (decoder.total_in() - in_before) as usize;
                    made.truncate(written);
                    self.made_last = written;
                    input.consume(took);
                    meter.work(took + written)?;
                    return Ok(match status {
                        Ok(zlib_rs::Status::StreamEnd) => Found::End,
                        Ok(_) if written > 0 => Found::More,
                        Ok(_) if took > 0 => continue,
                        Ok(_) if empty => Found::End,
                        _ => Found::Corrupt,
                    });
                }
            }
            Mode::Permissive(stream) => {
                stream.step(meter, PERMISSIVE_STEP)?;
                let pending = stream.pending();
                self.made.buf.extend_from_slice(pending);
                let n = pending.len();
                stream.hand(n);
                Ok(match (stream.done(), stream.corrupt) {
                    (false, _) => Found::More,
                    (true, false) => Found::End,
                    (true, true) => Found::Corrupt,
                })
            }
            Mode::Switching => unreachable!("a stage between decoders is not read"),
        }
    }
}

impl Stage for Flate<'_> {
    fn read(&mut self, buf: &mut [u8], meter: &Meter<'_>) -> Result<usize, ReadError> {
        while self.made.is_empty() {
            match self.state {
                State::Done => return Ok(0),
                State::Failed => return Err(ReadError::Failed),
                State::Going => {}
            }
            let found = self.make(meter)?;
            if !matches!(found, Found::More) {
                self.mode.input().drain(meter);
            }
            if let Some(error) = self.mode.input().error() {
                if error == ReadError::Restarted {
                    self.start(Which::first(), false);
                }
                self.made.clear();
                return Err(error);
            }
            match found {
                Found::More => {}
                Found::End => self.state = State::Done,
                #[cfg(feature = "unsafe")]
                Found::Corrupt if self.which == Which::Zlib => {
                    self.start(Which::Raw, true);
                    return Err(ReadError::Restarted);
                }
                #[cfg(feature = "unsafe")]
                Found::Corrupt if self.which == Which::Raw => {
                    warn!("flate stream is broken, decoding with fallback");
                    self.start(Which::Permissive, true);
                    return Err(ReadError::Restarted);
                }
                Found::Corrupt => {
                    warn!("flate stream is corrupt");
                    self.made.clear();
                    self.state = State::Failed;
                }
            }
        }
        Ok(self.made.hand(buf))
    }

    fn rewind(&mut self) {
        self.start(self.which, true);
    }
}

/// A predictor reversed as `apply_predictor` reverses it, which never fails where
/// [`Unpredictor`] takes its parameters.
struct Predict<'a> {
    from: Box<dyn Stage + 'a>,
    /// Holds the bytes decoded last, of which `taken` were handed out.
    unpredictor: Unpredictor,
    taken: usize,
}

impl Stage for Predict<'_> {
    /// As many rows as `buf` holds, so that short rows do not cost a read each.
    fn read(&mut self, buf: &mut [u8], meter: &Meter<'_>) -> Result<usize, ReadError> {
        let mut n = 0;
        while n < buf.len() {
            if self.taken == self.unpredictor.ready().len() {
                self.taken = 0;
                let from = &mut self.from;
                if !self.unpredictor.fill(|buf| from.read(buf, meter), meter)? {
                    break;
                }
                continue;
            }
            let row = &self.unpredictor.ready()[self.taken..];
            let k = (buf.len() - n).min(row.len());
            meter.work(k)?;
            buf[n..n + k].copy_from_slice(&row[..k]);
            self.taken += k;
            n += k;
        }
        Ok(n)
    }

    fn rewind(&mut self) {
        self.from.rewind();
        self.unpredictor.reset();
        self.taken = 0;
    }
}

/// The decoders that take a byte or a few at a time.
enum Decoder {
    Lzw(lzw::Decoder, bool),
    /// The high digit of the byte being read.
    AsciiHex(Option<u8>),
    /// The digits of the group being read.
    Ascii85(Vec<u8>),
    RunLength,
}

impl Decoder {
    fn start_over(&mut self) {
        *self = match self {
            Self::Lzw(_, early_change) => {
                Self::Lzw(lzw::Decoder::new(*early_change), *early_change)
            }
            Self::AsciiHex(_) => Self::AsciiHex(None),
            Self::Ascii85(_) => Self::Ascii85(Vec::new()),
            Self::RunLength => Self::RunLength,
        }
    }
}

/// A stage of a decoder that takes a byte or a few at a time.
struct Bytewise<'a> {
    input: Pulled<'a>,
    decoder: Decoder,
    state: State,
    made: Made,
}

impl<'a> Bytewise<'a> {
    fn new(from: Box<dyn Stage + 'a>, decoder: Decoder) -> Self {
        Self {
            input: Pulled::new(from),
            decoder,
            state: State::Going,
            made: Made::default(),
        }
    }

    fn lzw(from: Box<dyn Stage + 'a>, early_change: bool) -> Self {
        let decoder = Decoder::Lzw(lzw::Decoder::new(early_change), early_change);
        Self::new(from, decoder)
    }

    fn ascii_hex(from: Box<dyn Stage + 'a>) -> Self {
        Self::new(from, Decoder::AsciiHex(None))
    }

    fn ascii_85(from: Box<dyn Stage + 'a>) -> Self {
        Self::new(from, Decoder::Ascii85(Vec::new()))
    }

    fn run_length(from: Box<dyn Stage + 'a>) -> Self {
        Self::new(from, Decoder::RunLength)
    }

    fn start_over(&mut self) {
        self.decoder.start_over();
        self.state = State::Going;
        self.made.clear();
    }

    /// Decodes until a poll's worth is made or the data end or fail.
    fn make(&mut self, meter: &Meter<'_>) -> State {
        let out = &mut self.made.buf;
        let input = &mut self.input;
        match &mut self.decoder {
            Decoder::Lzw(decoder, _) => loop {
                match decoder.step(input, out, meter) {
                    Some(true) if out.len() < POLL_BYTES => {}
                    Some(true) => break State::Going,
                    Some(false) => break State::Done,
                    None => break State::Failed,
                }
            },
            Decoder::AsciiHex(high) => ascii_hex(input, high, out, meter),
            Decoder::Ascii85(group) => ascii_85(input, group, out, meter),
            Decoder::RunLength => run_length(input, out, meter),
        }
    }
}

impl Stage for Bytewise<'_> {
    fn read(&mut self, buf: &mut [u8], meter: &Meter<'_>) -> Result<usize, ReadError> {
        while self.made.is_empty() {
            match self.state {
                State::Done => return Ok(0),
                State::Failed => return Err(ReadError::Failed),
                State::Going => {}
            }
            let state = self.make(meter);
            if state != State::Going {
                self.input.drain(meter);
            }
            if let Some(error) = self.input.error() {
                if error == ReadError::Restarted {
                    self.start_over();
                }
                self.made.clear();
                return Err(error);
            }
            if state == State::Failed {
                // Whole decoding returns none of the data.
                self.made.clear();
            }
            self.state = state;
        }
        Ok(self.made.hand(buf))
    }

    fn rewind(&mut self) {
        self.input.rewind();
        self.start_over();
    }
}

/// As `ascii_hex::decode` decodes the data: up to a `>`, white space skipped, a last
/// digit alone taken as the high one.
fn ascii_hex(
    input: &mut Pulled<'_>,
    high: &mut Option<u8>,
    out: &mut Vec<u8>,
    meter: &Meter<'_>,
) -> State {
    loop {
        if out.len() >= POLL_BYTES {
            return State::Going;
        }
        let available = input.available(meter);
        if available.is_empty() {
            break;
        }
        let end = available.iter().position(|&b| b == b'>');
        let len = end.unwrap_or(available.len());
        for &byte in &available[..len] {
            if is_white_space_character(byte) {
                continue;
            }
            let Some(digit) = ascii_hex::decode_hex_digit(byte) else {
                return State::Failed;
            };
            match high.take() {
                Some(high) => out.push(high << 4 | digit),
                None => *high = Some(digit),
            }
        }
        input.consume(len + usize::from(end.is_some()));
        if meter.work(len + 1).is_err() {
            return State::Failed;
        }
        if end.is_some() {
            break;
        }
    }
    if let Some(high) = high.take() {
        out.push(high << 4);
    }
    State::Done
}

/// As `ascii_85::decode` decodes the data.
fn ascii_85(
    input: &mut Pulled<'_>,
    group: &mut Vec<u8>,
    out: &mut Vec<u8>,
    meter: &Meter<'_>,
) -> State {
    const POW_85: [u32; 5] = [52200625, 614125, 7225, 85, 1];
    let flush = |group: &mut Vec<u8>, out: &mut Vec<u8>| -> Option<()> {
        let len = group.len();
        match len {
            0 => return Some(()),
            // A single character is not valid.
            1 => return None,
            _ => {}
        }
        let mut digits = [u32::from(b'u' - b'!'); 5];
        for (digit, b) in digits.iter_mut().zip(group.iter()) {
            *digit = u32::from(b - b'!');
        }
        let value = digits[0]
            .checked_mul(POW_85[0])?
            .checked_add(digits[1].checked_mul(POW_85[1])?)?
            .checked_add(digits[2].checked_mul(POW_85[2])?)?
            .checked_add(digits[3].checked_mul(POW_85[3])?)?
            .checked_add(digits[4])?;
        out.extend_from_slice(&value.to_be_bytes()[..len - 1]);
        group.clear();
        Some(())
    };
    let end = |flushed: Option<()>| match flushed {
        Some(()) => State::Done,
        None => State::Failed,
    };
    while out.len() < POLL_BYTES {
        let Some(b) = input.next(meter) else {
            // Be lenient and accept what we have (see PDFBOX-5910).
            return end(flush(group, out));
        };
        if meter.work(1).is_err() {
            return State::Failed;
        }
        if is_white_space_character(b) {
            continue;
        }
        match b {
            b'!'..=b'u' => {
                group.push(b);
                if group.len() == 5 && flush(group, out).is_none() {
                    return State::Failed;
                }
            }
            b'z' => {
                if flush(group, out).is_none() {
                    return State::Failed;
                }
                out.extend_from_slice(&[0, 0, 0, 0]);
            }
            b'~' => return end(flush(group, out)),
            _ => return State::Failed,
        }
    }
    State::Going
}

/// As `run_length::decode` decodes the data: data that end without an end of data
/// marker fail, unless they end in a run of bytes as they are that is cut short.
fn run_length(input: &mut Pulled<'_>, out: &mut Vec<u8>, meter: &Meter<'_>) -> State {
    while out.len() < POLL_BYTES {
        let Some(length) = input.next(meter) else {
            return State::Failed;
        };
        if meter.work(usize::from(length) + 1).is_err() {
            return State::Failed;
        }
        match length {
            128 => return State::Done,
            0..=127 => {
                let n = usize::from(length) + 1;
                if input.peek(n - 1, meter).is_none() {
                    return State::Done;
                }
                for _ in 0..n {
                    out.extend(input.next(meter));
                }
            }
            _ => {
                let Some(byte) = input.next(meter) else {
                    return State::Failed;
                };
                out.extend(core::iter::repeat_n(byte, 257 - usize::from(length)));
            }
        }
    }
    State::Going
}
