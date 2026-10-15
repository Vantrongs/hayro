//! A stream's data decoded through all its filters a piece at a time, as
//! [`Stream::decoded_strict`](crate::object::Stream::decoded_strict) decodes them
//! whole, in memory bounded by the decoders' state, windows and buffers, however
//! much the filters make of the data and however they nest.
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

use crate::filter::lzw_flate::flate::fallback::{FlateStream, Input};
use crate::filter::lzw_flate::{PredictorParams, Unpredictor, lzw};
use crate::filter::reader::ReadError;
use crate::trivia::is_white_space_character;
use crate::work::{Meter, POLL_BYTES};
use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// The decoded data of a stream, read from its start in pieces (see
/// [`Stream::decoded_strict_reader`](crate::object::Stream::decoded_strict_reader)).
pub struct StrictReader<'a> {
    last: Box<dyn Stage + 'a>,
    /// The error that ended the reads, until a rewind.
    ended: Option<ReadError>,
}

impl<'a> StrictReader<'a> {
    pub(crate) fn new(last: Box<dyn Stage + 'a>) -> Self {
        Self { last, ended: None }
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
            false => self.last.read(&mut buf[..len], meter),
            true => Err(ReadError::Cancelled),
        };
        // A decoder may end early on a stop as on data cut short.
        let result = match meter.cancelled() {
            false => result,
            true => Err(ReadError::Cancelled),
        };
        if let Err(error) = result
            && error != ReadError::Restarted
        {
            self.ended = Some(error);
        }
        result
    }

    /// Back to the start of the data, each decoder kept as restarts chose it, so that
    /// data read to their end once are read again with no restart.
    pub fn rewind(&mut self) {
        self.ended = None;
        self.last.rewind();
    }

    /// The decoder each Flate filter reads with, in the order of the filters: a
    /// reader made with them reads as this one does after a rewind.
    pub(crate) fn choices(&self) -> Vec<Which> {
        let mut choices = Vec::new();
        self.last.choices(&mut choices);
        choices
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
    /// Adds the decoder of each Flate stage up to this one to `choices`, the first
    /// stage's first.
    fn choices(&self, choices: &mut Vec<Which>);
}

/// The stored data, decrypted.
pub(crate) struct Stored<'a> {
    data: Cow<'a, [u8]>,
    at: usize,
}

impl<'a> Stored<'a> {
    pub(crate) fn new(data: Cow<'a, [u8]>) -> Self {
        Self { data, at: 0 }
    }
}

impl Stage for Stored<'_> {
    /// Counts nothing: what reads the bytes counts those it takes.
    fn read(&mut self, buf: &mut [u8], meter: &Meter<'_>) -> Result<usize, ReadError> {
        if meter.cancelled() {
            return Err(ReadError::Cancelled);
        }
        let rest = &self.data[self.at..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.at += n;
        Ok(n)
    }

    fn rewind(&mut self) {
        self.at = 0;
    }

    fn choices(&self, _: &mut Vec<Which>) {}
}

/// How many bytes a decoder that takes them a few at a time is given from the stage
/// before it at once.
const PULLED: usize = 8192;

/// The bytes of the stage before, for a decoder that takes them a few at a time;
/// the first error reading them ends them, kept for the stage to handle.
pub(crate) struct Pulled<'a> {
    from: Box<dyn Stage + 'a>,
    buf: Vec<u8>,
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
            let len = self.buf.len();
            self.buf.resize(len + PULLED, 0);
            let read = self.from.read(&mut self.buf[len..], meter);
            self.buf.truncate(len + *read.as_ref().unwrap_or(&0));
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
    fn next(&mut self, meter: &Meter<'_>) -> Option<u8> {
        if !self.fill(0, meter) {
            return None;
        }
        let byte = self.buf[self.at];
        self.consume(1);
        Some(byte)
    }

    fn peek(&mut self, ahead: usize, meter: &Meter<'_>) -> Option<u8> {
        self.fill(ahead, meter).then(|| self.buf[self.at + ahead])
    }

    fn taken(&self) -> usize {
        self.taken
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
pub(crate) enum Which {
    #[cfg(feature = "unsafe")]
    Zlib,
    #[cfg(feature = "unsafe")]
    Raw,
    /// The permissive decoder, strict: data it finds corrupt fail.
    Permissive,
}

impl Which {
    fn first() -> Self {
        #[cfg(feature = "unsafe")]
        return Self::Zlib;
        #[cfg(not(feature = "unsafe"))]
        return Self::Permissive;
    }
}

/// A Flate stage.
pub(crate) struct Flate<'a> {
    mode: Mode<'a>,
    which: Which,
    state: State,
    made: Made,
}

enum Mode<'a> {
    /// zlib-rs.
    #[cfg(feature = "unsafe")]
    Inflating {
        input: Pulled<'a>,
        decoder: zlib_rs::Inflate,
    },
    Permissive(FlateStream<Pulled<'a>>),
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
            Which::Permissive => Self::Permissive(FlateStream::new(input)),
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
    /// A stage reading with decoder `which`, by default the first.
    pub(crate) fn new(from: Box<dyn Stage + 'a>, which: Option<Which>) -> Self {
        let which = which.unwrap_or_else(Which::first);
        Self {
            mode: Mode::new(Pulled::new(from), which),
            which,
            state: State::Going,
            made: Made::default(),
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
                    made.resize(POLL_BYTES.min(meter.until_poll().max(1)), 0);
                    let (out_before, in_before) = (decoder.total_out(), decoder.total_in());
                    let status = decoder.decompress(available, made, flush);
                    let written = (decoder.total_out() - out_before) as usize;
                    let took = (decoder.total_in() - in_before) as usize;
                    made.truncate(written);
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
                stream.step(meter, POLL_BYTES)?;
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

    fn choices(&self, choices: &mut Vec<Which>) {
        let from = match &self.mode {
            #[cfg(feature = "unsafe")]
            Mode::Inflating { input, .. } => &input.from,
            Mode::Permissive(stream) => &stream.input.from,
            Mode::Switching => unreachable!("a stage between decoders is not read"),
        };
        from.choices(choices);
        choices.push(self.which);
    }
}

/// A predictor reversed row by row, as `apply_predictor` reverses it, which never
/// fails where [`Unpredictor`] takes its parameters.
pub(crate) struct Predict<'a> {
    from: Box<dyn Stage + 'a>,
    unpredictor: Unpredictor,
    /// The encoded row being read, reversed as far as its bytes allow.
    input: Vec<u8>,
    /// The bytes of the last row decoded that were handed out.
    taken: usize,
}

impl Predict<'_> {
    fn start_over(&mut self) {
        self.input.clear();
        self.unpredictor.reset();
        self.taken = 0;
    }

    /// Reads the next row: false where the data end first (a last row cut short is
    /// dropped, as `apply_predictor` drops it).
    fn fill(&mut self, meter: &Meter<'_>) -> Result<bool, ReadError> {
        let mut chunk = [0; 8192];
        let len = self.unpredictor.input_len();
        while self.input.len() < len {
            let wanted = (len - self.input.len()).min(chunk.len());
            match self.from.read(&mut chunk[..wanted], meter) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.input.extend_from_slice(&chunk[..n]);
                    self.unpredictor.advance(&mut self.input, POLL_BYTES);
                }
                Err(e) => {
                    self.start_over();
                    return Err(e);
                }
            }
        }
        self.unpredictor.finish_row(&mut self.input);
        self.taken = 0;
        Ok(true)
    }
}

impl Stage for Predict<'_> {
    fn read(&mut self, buf: &mut [u8], meter: &Meter<'_>) -> Result<usize, ReadError> {
        while self.taken == self.unpredictor.row().len() {
            if !self.fill(meter)? {
                return Ok(0);
            }
        }
        let row = &self.unpredictor.row()[self.taken..];
        let n = buf.len().min(row.len());
        meter.work(n)?;
        buf[..n].copy_from_slice(&row[..n]);
        self.taken += n;
        Ok(n)
    }

    fn rewind(&mut self) {
        self.from.rewind();
        self.start_over();
    }

    fn choices(&self, choices: &mut Vec<Which>) {
        self.from.choices(choices);
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
pub(crate) struct Bytewise<'a> {
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

    pub(crate) fn lzw(from: Box<dyn Stage + 'a>, early_change: bool) -> Self {
        let decoder = Decoder::Lzw(lzw::Decoder::new(early_change), early_change);
        Self::new(from, decoder)
    }

    pub(crate) fn ascii_hex(from: Box<dyn Stage + 'a>) -> Self {
        Self::new(from, Decoder::AsciiHex(None))
    }

    pub(crate) fn ascii_85(from: Box<dyn Stage + 'a>) -> Self {
        Self::new(from, Decoder::Ascii85(Vec::new()))
    }

    pub(crate) fn run_length(from: Box<dyn Stage + 'a>) -> Self {
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

    fn choices(&self, choices: &mut Vec<Which>) {
        self.input.from.choices(choices);
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
            let Some(digit) = crate::filter::ascii_hex::decode_hex_digit(byte) else {
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

/// `stage` with the predictor of `params` reversed after it; `None` where only the
/// whole-data decoder reverses it.
pub(crate) fn predicted<'a>(
    stage: Box<dyn Stage + 'a>,
    params: &PredictorParams,
) -> Option<Box<dyn Stage + 'a>> {
    if params.predictor == 1 {
        return Some(stage);
    }
    Some(Box::new(Predict {
        from: stage,
        unpredictor: Unpredictor::new(params)?,
        input: Vec::new(),
        taken: 0,
    }))
}
