//! Stream data decoded incrementally, in memory bounded by the decoders' state.
//!
//! The filters of a chain are a flat list of layers, the first decoding the stored
//! bytes and each other what the one below it makes, each with its decoder's state
//! and a step's worth of bytes. One loop drives them ([`Chain::pull`]): it steps
//! the top layer, and a layer with too few bytes at hand for its next step sends the
//! loop down to the layer below it, whose bytes it decodes on the way back up. No
//! read, rewind, checkpoint or drop goes from one layer to the next by a call, so
//! the stack stays the same however many filters there are, and no layer holds more
//! than a step of what it makes, however much that is.

#[cfg(any(feature = "unsafe", feature = "images"))]
use crate::crypto::reader::Cursor;
use crate::crypto::reader::DecryptedReader;
use crate::filter::bytewise::{Decoder, State};
#[cfg(feature = "images")]
use crate::filter::ccitt::CcittBytes;
#[cfg(all(feature = "images", feature = "unsafe"))]
use crate::filter::ccitt::CcittState;
use crate::filter::lzw_flate::flate::fallback::{FlateStream, Input};
use crate::filter::lzw_flate::{PredictorParams, Unpredictor};
#[cfg(feature = "unsafe")]
use crate::object::stream::OwnedStream;
use crate::work::{Cancelled, Meter, POLL_BYTES};
use alloc::boxed::Box;
use alloc::vec::Vec;

/// How many decrypted bytes are read at a time, and how many bytes the Flate
/// decoder is given at a time.
const INPUT_CHUNK: usize = 8192;

/// The bytes a decoder that reads a few bytes at a time wants at hand before a step,
/// unless the data end sooner ([`Input::low`]): more than its longest step reads, a
/// Flate block's header and tables (under 600 bytes) or a run length run (129).
const LOOKAHEAD: usize = 1024;

/// The most bytes such a decoder holds of the layer below it.
const FED_AT_MOST: usize = INPUT_CHUNK;

/// How many bytes the permissive Flate decoder makes ahead of the reads at most.
const PERMISSIVE_STEP: usize = 16 * 1024;

/// How many bytes the Flate decoder makes at a time. Calls that make less are
/// slower: each copies what it made into zlib's history window, and matches reaching
/// back past its output read from there.
#[cfg(feature = "unsafe")]
const OUTPUT_CHUNK: usize = POLL_BYTES;

/// The most bytes a decoder that takes a byte or a few at a time holds made: a poll's
/// worth, and the most one step makes past it (an LZW code's bytes).
#[cfg(feature = "unsafe")]
const MADE_AT_MOST: usize = POLL_BYTES + 4096;

/// The bytes [`DecodedReader::skip`] reads at a time, on the stack.
const SKIP_CHUNK: usize = 8 * 1024;

/// Why reading decoded data stopped before its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// The data turned out corrupt for the decoder in use, and decoding starts over
    /// with a more permissive one, as [`Stream::decoded`](crate::object::Stream::decoded)
    /// does: the bytes read so far are void, and the next read returns the data from
    /// its start.
    Restarted,
    /// The data cannot be decoded.
    Failed,
    /// The meter's hook asked to stop. The reader reads again only after a rewind.
    Cancelled,
}

impl From<Cancelled> for ReadError {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}

/// The decoded data of a stream, read from its start in pieces (see
/// [`Stream::decoded_reader`](crate::object::Stream::decoded_reader)). It yields the
/// bytes [`Stream::decoded`](crate::object::Stream::decoded) returns, but holds only
/// each filter's decoder state, up to [`POLL_BYTES`] it made ahead of the reads, a
/// chunk of its input and a predictor row, not the whole result, whatever the number
/// of filters. A byte handed out is never taken back: a Flate decoder is chosen
/// before its first byte, as lenient whole decoding chooses it
/// ([`flate::decode`](crate::filter::lzw_flate::flate::decode)), and other filters
/// keep what they made before data they cannot read, as lenient whole decoding does
/// ([`bytewise`](crate::filter::bytewise)). Every read counts its work on a [`Meter`],
/// and stops when the meter's hook asks to.
pub struct DecodedReader<'a> {
    source: Source<'a>,
}

#[cfg_attr(
    feature = "images",
    expect(
        clippy::large_enum_variant,
        reason = "a reader is one per stream, and most read a chain, kept inline"
    )
)]
enum Source<'a> {
    Chain(Chain<'a>),
    /// An image's CCITT data, decoded a row at a time.
    #[cfg(feature = "images")]
    Ccitt(Box<CcittBytes<'a>>),
}

/// The stored bytes and the layers of the filters over them, the first decoding the
/// stored bytes; none for data stored without a filter.
struct Chain<'a> {
    stored: Stored<'a>,
    layers: Vec<Layer>,
    /// The bytes the decoders made, those not read yet included.
    inflated: u64,
}

/// The stored bytes as the first layer takes them: read where they are when they
/// need no decryption, else decrypted a chunk at a time.
struct Stored<'a> {
    data: DecryptedReader<'a>,
    /// Decrypted bytes, of which the first `at` were taken.
    buf: Vec<u8>,
    at: usize,
}

/// A filter a reader decodes, with its predictor's parameters.
#[derive(Clone)]
pub(crate) enum Codec {
    Flate(PredictorParams),
    Lzw(PredictorParams),
    AsciiHex,
    Ascii85,
    RunLength,
}

/// A filter of a chain: its decoder, and the predictor reversed after it.
struct Layer {
    stage: Stage,
    predictor: Option<Predicted>,
}

enum Stage {
    #[cfg(feature = "unsafe")]
    Inflating(Inflating),
    /// Flate data read by the permissive decoder: data of which neither zlib nor raw
    /// deflate makes a byte, or, without the `unsafe` feature, any.
    Permissive(Box<Permissive>),
    /// A decoder that takes a byte or a few at a time.
    Bytewise(Stepping),
}

/// Flate data inflated by zlib-rs.
#[cfg(feature = "unsafe")]
struct Inflating {
    /// Made at the first step and dropped at the end of the data, so that a chain
    /// holds the state of no decoder that is done.
    decoder: Option<zlib_rs::Inflate>,
    /// Whether the data is read as zlib; after that fails before making a byte, as
    /// raw deflate.
    zlib: bool,
    /// Whether the decoder reached the end of the stream or of the data, or found
    /// the data corrupt after making bytes, which end them.
    done: bool,
    /// Room for a step's bytes, of which `handed..end` are to be read.
    output: Vec<u8>,
    handed: usize,
    end: usize,
}

struct Permissive {
    stream: FlateStream<Fed>,
    done: bool,
}

/// Data of a filter whose decoder takes a byte or a few at a time, decoded a poll's
/// worth at a time.
struct Stepping {
    fed: Fed,
    decoder: Decoder,
    /// Bytes made, of which `handed` were read.
    made: Vec<u8>,
    handed: usize,
    /// Whether the decoder reached the end of the data, or data it cannot read, which
    /// end them.
    done: bool,
}

/// A predictor reversed after a decoder.
struct Predicted {
    /// Holds the bytes decoded last, of which `taken` were handed out.
    unpredictor: Unpredictor,
    taken: usize,
    done: bool,
}

/// The bytes a decoder that reads a few at a time has at hand, copied from the layer
/// below it.
#[derive(Clone, Default)]
struct Fed {
    /// Bytes, of which the decoder took the first `at`.
    buf: Vec<u8>,
    at: usize,
    taken: usize,
    /// Whether the layer below has no more.
    ended: bool,
}

/// What a step of a layer came to.
enum Step {
    /// It has bytes to read.
    Made,
    /// It needs bytes the layer below has still to make.
    Starved,
    /// Its data ended.
    Done,
    /// Its Flate data turned out corrupt for its decoder before it made a byte: it
    /// goes on with the next decoder whole decoding would try, from the start.
    #[cfg(feature = "unsafe")]
    Restart,
}

/// Why a predictor's read of its decoder's bytes stopped.
enum Pull {
    Step(Step),
    Cancelled,
}

impl From<Cancelled> for Pull {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}

/// What a layer decodes: the stored bytes, or the layer below it.
enum Under<'r, 'a> {
    Stored(&'r mut Stored<'a>),
    Layer(&'r mut Layer),
}

/// A position in a stream's data to resume reading from (see
/// [`DecodedReader::checkpoint`]). The stored bytes are not copied; the checkpoint
/// keeps its stream, and with it those bytes, alive.
#[cfg(feature = "unsafe")]
pub struct DecodedCheckpoint {
    /// Keeps the bytes at `data` alive, so no other bytes can take their address.
    _stream: OwnedStream,
    /// The address and length of the stored bytes.
    data: (usize, usize),
    saved: Saved,
}

#[cfg(feature = "unsafe")]
enum Saved {
    Chain(ChainState),
    #[cfg(feature = "images")]
    Ccitt(CcittState),
}

/// Where a chain was: the decryption cursor, the decrypted bytes not taken yet, and
/// a copy of each layer holding only the bytes it had not handed on.
#[cfg(any(feature = "unsafe", feature = "images"))]
pub(crate) struct ChainState {
    cursor: Cursor,
    pending: Vec<u8>,
    layers: Box<[Layer]>,
}

impl<'a> DecodedReader<'a> {
    /// The data as stored, without filters.
    #[cfg(test)]
    pub(crate) fn raw(data: DecryptedReader<'a>) -> Self {
        Self::chain(data, Vec::new())
    }

    fn chain(data: DecryptedReader<'a>, layers: Vec<Layer>) -> Self {
        Self {
            source: Source::Chain(Chain {
                stored: Stored {
                    data,
                    buf: Vec::new(),
                    at: 0,
                },
                layers,
                inflated: 0,
            }),
        }
    }

    /// `stored` decoded by `codecs` in turn, as it is read; `None` when a predictor
    /// is one only the whole-data decoder reverses.
    pub(crate) fn decoding(stored: DecryptedReader<'a>, codecs: &[Codec]) -> Option<Self> {
        let layers = codecs.iter().map(Layer::new).collect::<Option<_>>()?;
        Some(Self::chain(stored, layers))
    }

    /// Flate data with its predictor `params` (see [`Self::decoding`]).
    #[cfg(test)]
    pub(crate) fn flate(data: DecryptedReader<'a>, params: &PredictorParams) -> Option<Self> {
        Self::decoding(data, &[Codec::Flate(params.clone())])
    }

    /// The bytes the reader yields from its start, where it knows them without
    /// reading: for data stored without a filter.
    #[cfg(feature = "images")]
    pub fn stored_len(&self) -> Option<usize> {
        match &self.source {
            Source::Chain(chain) if chain.layers.is_empty() => Some(chain.stored.data.len()),
            _ => None,
        }
    }

    /// An image's CCITT data, read as the bytes whole decoding makes of it.
    #[cfg(feature = "images")]
    pub(crate) fn ccitt(bytes: CcittBytes<'a>) -> Self {
        Self {
            source: Source::Ccitt(Box::new(bytes)),
        }
    }

    /// The bytes this reader's decoders produced, those of every filter and those not
    /// read yet included.
    pub fn inflated_bytes(&self) -> u64 {
        match &self.source {
            Source::Chain(chain) => chain.inflated,
            #[cfg(feature = "images")]
            Source::Ccitt(_) => 0,
        }
    }

    /// The most heap bytes a checkpoint of this reader holds
    /// ([`DecodedCheckpoint::heap_bytes`]), at any position; `None` when the reader
    /// cannot be checkpointed (see [`Self::checkpoint`]).
    #[cfg(feature = "unsafe")]
    pub fn checkpoint_heap_bound(&self) -> Option<usize> {
        self.checkpointable()?;
        match &self.source {
            Source::Chain(chain) => chain.heap_bound(),
            #[cfg(feature = "images")]
            Source::Ccitt(bytes) => Some(bytes.heap_bound()),
        }
    }

    /// The reader's position, to resume from with [`Self::restore`] on a reader of the
    /// same `stream`. `None` unless every filter is decoded incrementally (Flate by
    /// zlib-rs, not by the permissive decoder; no image filter but `CCITTFaxDecode`
    /// alone over data not encrypted) over `stream`'s stored bytes, or the reader
    /// reads them stored without a filter and decrypted with RC4 (which
    /// [`Self::skip`] otherwise moves past from the start of its keystream), or when a
    /// decoder's state cannot be allocated. Copying the decoders' state counts on
    /// `meter`, and fails once its hook asks to stop.
    #[cfg(feature = "unsafe")]
    pub fn checkpoint(
        &self,
        stream: &OwnedStream,
        meter: &Meter<'_>,
    ) -> Result<Option<DecodedCheckpoint>, Cancelled> {
        let Some(data) = self.checkpointable().filter(|data| stream.holds(data)) else {
            return Ok(None);
        };
        let saved = match &self.source {
            Source::Chain(chain) => chain.save(meter)?.map(Saved::Chain),
            #[cfg(feature = "images")]
            Source::Ccitt(bytes) => bytes.save(meter)?.map(Saved::Ccitt),
        };
        Ok(saved.map(|saved| DecodedCheckpoint {
            _stream: stream.clone(),
            data: address(data),
            saved,
        }))
    }

    /// Continues from `checkpoint`, as if the data before it had been read (with the
    /// decoders chosen then). False, with the reader unchanged, when the checkpoint is
    /// of other bytes or filters, or a decoder's state cannot be allocated. Copying
    /// the decoders' state counts on `meter`; once its hook asks to stop, it fails,
    /// with the reader unchanged.
    #[cfg(feature = "unsafe")]
    pub fn restore(
        &mut self,
        checkpoint: &DecodedCheckpoint,
        meter: &Meter<'_>,
    ) -> Result<bool, Cancelled> {
        if self.checkpointable().map(address) != Some(checkpoint.data) {
            return Ok(false);
        }
        match (&mut self.source, &checkpoint.saved) {
            (Source::Chain(chain), Saved::Chain(state)) => chain.load(state, meter),
            #[cfg(feature = "images")]
            (Source::Ccitt(bytes), Saved::Ccitt(state)) => {
                bytes.restore(state, meter)?;
                Ok(true)
            }
            #[cfg(feature = "images")]
            _ => Ok(false),
        }
    }

    /// Where the reader is, to go back to with [`Self::load`]: for a chain of
    /// filters, `None` for CCITT data or where a decoder's state cannot be allocated.
    /// Copying the decoders' state counts on `meter`.
    #[cfg(feature = "images")]
    pub(crate) fn save(&self, meter: &Meter<'_>) -> Result<Option<ChainState>, Cancelled> {
        match &self.source {
            Source::Chain(chain) => chain.save(meter),
            Source::Ccitt(_) => Ok(None),
        }
    }

    /// Goes back to `state`, saved of this reader; false, with the reader unchanged,
    /// where a decoder's state cannot be allocated. Copying the state counts on
    /// `meter`; once its hook asks to stop, it fails, with the reader unchanged.
    #[cfg(feature = "images")]
    pub(crate) fn load(
        &mut self,
        state: &ChainState,
        meter: &Meter<'_>,
    ) -> Result<bool, Cancelled> {
        match &mut self.source {
            Source::Chain(chain) => chain.load(state, meter),
            Source::Ccitt(_) => Ok(false),
        }
    }

    /// The document's bytes the reader reads, where it can be checkpointed: those
    /// filters decoded incrementally read, or those stored without a filter and
    /// decrypted with RC4.
    #[cfg(feature = "unsafe")]
    fn checkpointable(&self) -> Option<&'a [u8]> {
        match &self.source {
            Source::Chain(chain) if chain.layers.is_empty() => {
                chain.stored.data.is_rc4().then_some(())?;
                chain.stored.data.borrowed_data()
            }
            Source::Chain(chain) => {
                let permissive = |l: &Layer| matches!(l.stage, Stage::Permissive(_));
                match chain.layers.iter().any(permissive) {
                    true => None,
                    false => chain.stored.data.borrowed_data(),
                }
            }
            #[cfg(feature = "images")]
            Source::Ccitt(bytes) => bytes.plain(),
        }
    }

    /// The stored bytes read.
    #[cfg(all(test, feature = "unsafe"))]
    fn stored_read(&self) -> usize {
        match &self.source {
            Source::Chain(chain) => chain.stored.data.position(),
            #[cfg(feature = "images")]
            Source::Ccitt(_) => 0,
        }
    }

    /// Back to the start of the data, keeping the decoder chosen (so reading again
    /// does not try the others again).
    pub fn rewind(&mut self) {
        match &mut self.source {
            Source::Chain(chain) => chain.rewind(),
            #[cfg(feature = "images")]
            Source::Ccitt(bytes) => bytes.rewind(),
        }
    }

    /// Whether [`Self::skip`] moves past the data without decoding or copying it: for
    /// data stored without a filter, which is addressed directly (RC4 data makes the
    /// keystream of the bytes moved past).
    pub fn seeks(&self) -> bool {
        matches!(&self.source, Source::Chain(chain) if chain.layers.is_empty())
    }

    /// Moves past the next `n` decoded bytes, as [`Self::read`] reads them; returns how
    /// many the data held, fewer only at the end. Data stored without a filter is
    /// moved past without copying it ([`Self::seeks`]), CCITT rows without making
    /// their bytes; other data is read through. Once the meter's hook asked to stop,
    /// it fails, as reads do.
    pub fn skip(&mut self, n: usize, meter: &Meter<'_>) -> Result<usize, Cancelled> {
        if meter.cancelled() {
            return Err(Cancelled);
        }
        match &mut self.source {
            Source::Chain(chain) if chain.layers.is_empty() => {
                return chain.stored.data.skip(n, meter);
            }
            #[cfg(feature = "images")]
            Source::Ccitt(bytes) => return bytes.skip(n, meter),
            Source::Chain(_) => {}
        }
        let mut scratch = [0; SKIP_CHUNK];
        let mut skipped = 0;
        while skipped < n {
            let len = (n - skipped).min(SKIP_CHUNK);
            match self.read(&mut scratch[..len], meter)? {
                0 => break,
                got => skipped += got,
            }
        }
        Ok(skipped)
    }

    /// Fills `buf` with the next decoded bytes and returns how many: at most
    /// [`POLL_BYTES`], 0 only at the end. The work counts on `meter`; once its hook
    /// asked to stop, every read fails.
    pub fn read(&mut self, buf: &mut [u8], meter: &Meter<'_>) -> Result<usize, Cancelled> {
        let len = buf.len().min(POLL_BYTES);
        let buf = &mut buf[..len];
        if buf.is_empty() {
            return Ok(0);
        }
        // Bytes made ahead were counted when they were made, so handing them out
        // counts nothing that would find the stop.
        if meter.cancelled() {
            return Err(Cancelled);
        }
        match &mut self.source {
            Source::Chain(chain) => chain.read(buf, meter),
            #[cfg(feature = "images")]
            Source::Ccitt(bytes) => bytes.read(buf, meter),
        }
    }
}

#[cfg(feature = "unsafe")]
impl DecodedCheckpoint {
    /// The bytes it holds on the heap: the decoders' state and windows, and the input,
    /// output and rows it copied.
    pub fn heap_bytes(&self) -> usize {
        match &self.saved {
            Saved::Chain(state) => state.heap_bytes(),
            #[cfg(feature = "images")]
            Saved::Ccitt(state) => state.heap_bytes(),
        }
    }
}

#[cfg(feature = "unsafe")]
impl ChainState {
    fn heap_bytes(&self) -> usize {
        self.pending.capacity()
            + size_of_val(&*self.layers)
            + self.layers.iter().map(Layer::heap_bytes).sum::<usize>()
    }
}

/// A copy of each of `layers`, allocated at its length, which collecting an
/// iterator of options would not, counted on `meter` ([`Layer::copy`]); `None`
/// where a decoder's state cannot be allocated.
#[cfg(any(feature = "unsafe", feature = "images"))]
fn copies(layers: &[Layer], meter: &Meter<'_>) -> Result<Option<Vec<Layer>>, Cancelled> {
    let mut copies = Vec::with_capacity(layers.len());
    for layer in layers {
        match layer.copy(meter)? {
            Some(copy) => copies.push(copy),
            None => return Ok(None),
        }
    }
    Ok(Some(copies))
}

/// Identifies stored bytes by where they are.
#[cfg(feature = "unsafe")]
fn address(data: &[u8]) -> (usize, usize) {
    (data.as_ptr() as usize, data.len())
}

/// The heap bytes of a zlib-rs decoder's state and window.
#[cfg(feature = "unsafe")]
fn inflate_heap() -> usize {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static HEAP: AtomicUsize = AtomicUsize::new(0);
    match HEAP.load(Ordering::Relaxed) {
        0 => {
            let heap = zlib_rs::Inflate::new(true, 15).allocation_size();
            HEAP.store(heap, Ordering::Relaxed);
            heap
        }
        heap => heap,
    }
}

impl Chain<'_> {
    fn read(&mut self, buf: &mut [u8], meter: &Meter<'_>) -> Result<usize, Cancelled> {
        if self.layers.is_empty() {
            let n = self.stored.data.read(buf);
            meter.work(n)?;
            return Ok(n);
        }
        if !self.pull(meter)? {
            return Ok(0);
        }
        let Some(top) = self.layers.last_mut() else {
            return Ok(0);
        };
        let n = buf.len().min(top.out().len());
        // A predictor's rows were made by reversing the decoder's bytes, which handing
        // them out counts.
        if top.predictor.is_some() {
            meter.work(n)?;
        }
        buf[..n].copy_from_slice(&top.out()[..n]);
        top.hand(n);
        Ok(n)
    }

    /// Steps the layers until the top one has bytes to read (true) or its data ended
    /// (false): a layer that needs bytes the one below it has still to make sends the
    /// loop down to that one, and a layer that made bytes or ended sends it back up
    /// to the one above. Going down counts a step of work, so that the hook is
    /// polled however many layers there are.
    fn pull(&mut self, meter: &Meter<'_>) -> Result<bool, Cancelled> {
        let top = self.layers.len() - 1;
        let mut i = top;
        loop {
            let (below, rest) = self.layers.split_at_mut(i);
            let layer = &mut rest[0];
            if !layer.out().is_empty() {
                if i == top {
                    return Ok(true);
                }
                i += 1;
                continue;
            }
            let mut under = match below.last_mut() {
                Some(below) => Under::Layer(below),
                None => Under::Stored(&mut self.stored),
            };
            match layer.step(&mut under, &mut self.inflated, meter)? {
                Step::Made => {}
                Step::Done if i == top => return Ok(false),
                Step::Done => i += 1,
                Step::Starved => {
                    meter.work(1)?;
                    // A layer starves only for bytes another layer makes: the stored
                    // ones are all at hand.
                    i = i.checked_sub(1).expect("the first layer never starves");
                }
                #[cfg(feature = "unsafe")]
                Step::Restart => self.restart(i),
            }
        }
    }

    /// Moves layer `i` on to the next decoder in the order whole decoding tries them,
    /// after the one in use found the data corrupt before making a byte (so none was
    /// handed out): zlib, raw deflate, then the permissive decoder, each over the
    /// data from its start.
    #[cfg(feature = "unsafe")]
    fn restart(&mut self, i: usize) {
        self.stored.rewind();
        for layer in &mut self.layers[..i] {
            layer.rewind();
        }
        let layer = &mut self.layers[i];
        layer.rewind();
        if let Stage::Inflating(inflating) = &mut layer.stage
            && inflating.zlib
        {
            inflating.zlib = false;
            inflating.rewind();
            return;
        }
        warn!("flate stream is broken, decoding with fallback");
        layer.stage = Stage::permissive();
    }

    fn rewind(&mut self) {
        self.stored.rewind();
        for layer in &mut self.layers {
            layer.rewind();
        }
    }

    /// The most heap bytes a copy of its state holds.
    #[cfg(feature = "unsafe")]
    fn heap_bound(&self) -> Option<usize> {
        let pending = match self.stored.data.plain_rest() {
            Some(_) => 0,
            None => INPUT_CHUNK,
        };
        self.layers
            .iter()
            .try_fold(pending + size_of_val(&*self.layers), |bound, layer| {
                bound.checked_add(layer.heap_bound()?)
            })
    }

    /// A copy of its state, counted on `meter`; `None` where a decoder's state cannot
    /// be allocated.
    #[cfg(any(feature = "unsafe", feature = "images"))]
    fn save(&self, meter: &Meter<'_>) -> Result<Option<ChainState>, Cancelled> {
        let pending = match self.stored.data.plain_rest() {
            Some(_) => Vec::new(),
            None => meter.copied(&self.stored.buf[self.stored.at..])?,
        };
        Ok(copies(&self.layers, meter)?.map(|layers| ChainState {
            cursor: self.stored.data.cursor(),
            pending,
            layers: layers.into_boxed_slice(),
        }))
    }

    /// Whether `state` was saved of a chain of the same filters.
    #[cfg(any(feature = "unsafe", feature = "images"))]
    fn fits(&self, state: &ChainState) -> bool {
        self.layers.len() == state.layers.len()
            && self
                .layers
                .iter()
                .zip(&state.layers)
                .all(|(layer, saved)| layer.same_filter(saved))
    }

    /// Takes the state of `state`, saved of this chain, the copying counted on
    /// `meter`; false, with the chain unchanged, where `state` is of a chain that
    /// does not [`Self::fits`] it or a decoder's state cannot be allocated, and
    /// unchanged too where the meter's hook asks to stop.
    #[cfg(any(feature = "unsafe", feature = "images"))]
    fn load(&mut self, state: &ChainState, meter: &Meter<'_>) -> Result<bool, Cancelled> {
        if !self.fits(state) {
            return Ok(false);
        }
        let Some(copies) = copies(&state.layers, meter)? else {
            return Ok(false);
        };
        let pending = meter.copied(&state.pending)?;
        for (layer, copy) in self.layers.iter_mut().zip(copies) {
            *layer = copy;
        }
        self.stored.data.restore(&state.cursor);
        self.stored.buf = pending;
        self.stored.at = 0;
        Ok(true)
    }
}

impl Stored<'_> {
    /// The bytes not taken yet, decrypting more where none is left.
    fn out(&mut self) -> &[u8] {
        if self.data.plain_rest().is_none() && self.at == self.buf.len() {
            self.buf.resize(INPUT_CHUNK, 0);
            let n = self.data.read(&mut self.buf);
            self.buf.truncate(n);
            self.at = 0;
        }
        match self.data.plain_rest() {
            Some(rest) => rest,
            None => &self.buf[self.at..],
        }
    }

    fn hand(&mut self, n: usize) {
        match self.data.plain_rest() {
            Some(_) => self.data.advance(n),
            None => self.at += n,
        }
    }

    fn rewind(&mut self) {
        self.data.rewind();
        self.buf.clear();
        self.at = 0;
    }
}

impl Under<'_, '_> {
    fn out(&mut self) -> &[u8] {
        match self {
            Under::Stored(stored) => stored.out(),
            Under::Layer(layer) => layer.out(),
        }
    }

    fn hand(&mut self, n: usize) {
        match self {
            Under::Stored(stored) => stored.hand(n),
            Under::Layer(layer) => layer.hand(n),
        }
    }

    /// Whether it has no more bytes and will make none.
    fn ended(&mut self) -> bool {
        match self {
            Under::Stored(stored) => stored.out().is_empty(),
            Under::Layer(layer) => layer.ended(),
        }
    }
}

impl Layer {
    /// The layer of `codec`; `None` when its predictor is one only the whole-data
    /// decoder reverses.
    fn new(codec: &Codec) -> Option<Self> {
        let (stage, params) = match codec {
            Codec::Flate(params) => (Stage::flate(), Some(params)),
            Codec::Lzw(params) => (
                Stage::bytewise(Decoder::lzw(params.early_change)),
                Some(params),
            ),
            Codec::AsciiHex => (Stage::bytewise(Decoder::ascii_hex()), None),
            Codec::Ascii85 => (Stage::bytewise(Decoder::ascii_85()), None),
            Codec::RunLength => (Stage::bytewise(Decoder::RunLength), None),
        };
        let predictor = match params {
            Some(params) if params.predictor != 1 => Some(Predicted {
                unpredictor: Unpredictor::new(params)?,
                taken: 0,
                done: false,
            }),
            _ => None,
        };
        Some(Self { stage, predictor })
    }

    /// The bytes it made and has not handed on.
    fn out(&self) -> &[u8] {
        match &self.predictor {
            Some(p) => p.unpredictor.ready().get(p.taken..).unwrap_or_default(),
            None => self.stage.out(),
        }
    }

    fn hand(&mut self, n: usize) {
        match &mut self.predictor {
            Some(p) => p.taken += n,
            None => self.stage.hand(n),
        }
    }

    fn ended(&self) -> bool {
        self.out().is_empty()
            && match &self.predictor {
                Some(p) => p.done,
                None => self.stage.done(),
            }
    }

    /// Decodes the next bytes of what `under` makes, with no bytes of its own left
    /// to read.
    fn step(
        &mut self,
        under: &mut Under<'_, '_>,
        inflated: &mut u64,
        meter: &Meter<'_>,
    ) -> Result<Step, Cancelled> {
        let Self { stage, predictor } = self;
        let Some(p) = predictor else {
            return stage.step(under, inflated, meter);
        };
        if p.done {
            return Ok(Step::Done);
        }
        // The rows read so far stay in the unpredictor when its decoder starves.
        let read = |buf: &mut [u8]| -> Result<usize, Pull> {
            loop {
                let out = stage.out();
                if !out.is_empty() {
                    let n = buf.len().min(out.len());
                    buf[..n].copy_from_slice(&out[..n]);
                    stage.hand(n);
                    return Ok(n);
                }
                match stage.step(under, inflated, meter)? {
                    Step::Made => {}
                    Step::Done => return Ok(0),
                    step => return Err(Pull::Step(step)),
                }
            }
        };
        match p.unpredictor.resume(read, meter) {
            Ok(true) => {
                p.taken = 0;
                Ok(Step::Made)
            }
            Ok(false) => {
                p.done = true;
                Ok(Step::Done)
            }
            Err(Pull::Step(step)) => Ok(step),
            Err(Pull::Cancelled) => Err(Cancelled),
        }
    }

    fn rewind(&mut self) {
        self.stage.rewind();
        if let Some(p) = &mut self.predictor {
            p.unpredictor.reset();
            (p.taken, p.done) = (0, false);
        }
    }

    /// A copy holding only the bytes it has not handed on, and the predictor's rows,
    /// each piece counted on `meter` before it is copied, the layer itself too, so
    /// that copying any number of layers polls the hook ([`Meter::copied`]); `None`
    /// where a decoder's state cannot be allocated.
    #[cfg(any(feature = "unsafe", feature = "images"))]
    fn copy(&self, meter: &Meter<'_>) -> Result<Option<Self>, Cancelled> {
        meter.copying(size_of::<Self>())?;
        let Some(stage) = self.stage.copy(meter)? else {
            return Ok(None);
        };
        let predictor = match &self.predictor {
            Some(p) => Some(Predicted {
                unpredictor: p.unpredictor.copy(meter)?,
                taken: p.taken,
                done: p.done,
            }),
            None => None,
        };
        Ok(Some(Self { stage, predictor }))
    }

    /// Whether `other` decodes the same filter.
    #[cfg(any(feature = "unsafe", feature = "images"))]
    fn same_filter(&self, other: &Self) -> bool {
        let bytewise = |stage: &Stage| match stage {
            Stage::Bytewise(stepping) => Some(core::mem::discriminant(&stepping.decoder)),
            _ => None,
        };
        bytewise(&self.stage) == bytewise(&other.stage)
            && self.predictor.is_some() == other.predictor.is_some()
    }

    #[cfg(feature = "unsafe")]
    fn heap_bytes(&self) -> usize {
        let stage = match &self.stage {
            Stage::Inflating(inflating) => {
                inflating
                    .decoder
                    .as_ref()
                    .map_or(0, zlib_rs::Inflate::allocation_size)
                    + inflating.output.capacity()
            }
            Stage::Bytewise(stepping) => {
                stepping.decoder.heap_bytes()
                    + stepping.fed.buf.capacity()
                    + stepping.made.capacity()
            }
            // Never checkpointed ([`DecodedReader::checkpointable`]).
            Stage::Permissive(_) => 0,
        };
        stage
            + self
                .predictor
                .as_ref()
                .map_or(0, |p| p.unpredictor.heap_bytes())
    }

    /// The most heap bytes a copy holds, at any position; `None` for the permissive
    /// decoder, which is not checkpointed.
    #[cfg(feature = "unsafe")]
    fn heap_bound(&self) -> Option<usize> {
        let stage = match &self.stage {
            Stage::Inflating(_) => inflate_heap() + OUTPUT_CHUNK,
            Stage::Bytewise(stepping) => stepping.decoder.heap_bound() + FED_AT_MOST + MADE_AT_MOST,
            Stage::Permissive(_) => return None,
        };
        let predictor = match &self.predictor {
            Some(p) => p.unpredictor.heap_bound()?,
            None => 0,
        };
        stage.checked_add(predictor)
    }
}

impl Stage {
    /// Flate data, inflated by zlib-rs, or without the `unsafe` feature by the
    /// permissive decoder.
    fn flate() -> Self {
        #[cfg(feature = "unsafe")]
        return Self::Inflating(Inflating {
            decoder: None,
            zlib: true,
            done: false,
            output: Vec::new(),
            handed: 0,
            end: 0,
        });
        #[cfg(not(feature = "unsafe"))]
        return Self::permissive();
    }

    fn permissive() -> Self {
        Self::Permissive(Box::new(Permissive {
            stream: FlateStream::stepping(Fed::default(), PERMISSIVE_STEP),
            done: false,
        }))
    }

    fn bytewise(decoder: Decoder) -> Self {
        Self::Bytewise(Stepping {
            fed: Fed::default(),
            decoder,
            made: Vec::new(),
            handed: 0,
            done: false,
        })
    }

    fn out(&self) -> &[u8] {
        match self {
            #[cfg(feature = "unsafe")]
            Self::Inflating(inflating) => &inflating.output[inflating.handed..inflating.end],
            Self::Permissive(permissive) => permissive.stream.pending(),
            Self::Bytewise(stepping) => &stepping.made[stepping.handed..],
        }
    }

    fn hand(&mut self, n: usize) {
        match self {
            #[cfg(feature = "unsafe")]
            Self::Inflating(inflating) => inflating.handed += n,
            Self::Permissive(permissive) => permissive.stream.hand(n),
            Self::Bytewise(stepping) => stepping.handed += n,
        }
    }

    /// Whether its data ended: it makes no more bytes.
    fn done(&self) -> bool {
        match self {
            #[cfg(feature = "unsafe")]
            Self::Inflating(inflating) => inflating.done,
            Self::Permissive(permissive) => permissive.done,
            Self::Bytewise(stepping) => stepping.done,
        }
    }

    fn step(
        &mut self,
        under: &mut Under<'_, '_>,
        inflated: &mut u64,
        meter: &Meter<'_>,
    ) -> Result<Step, Cancelled> {
        match self {
            #[cfg(feature = "unsafe")]
            Self::Inflating(inflating) => inflating.step(under, inflated, meter),
            Self::Permissive(permissive) => permissive.step(under, inflated, meter),
            Self::Bytewise(stepping) => stepping.step(under, inflated, meter),
        }
    }

    fn rewind(&mut self) {
        match self {
            #[cfg(feature = "unsafe")]
            Self::Inflating(inflating) => inflating.rewind(),
            Self::Permissive(permissive) => {
                permissive.stream = FlateStream::stepping(Fed::default(), PERMISSIVE_STEP);
                permissive.done = false;
            }
            Self::Bytewise(stepping) => {
                stepping.fed = Fed::default();
                stepping.decoder.start_over();
                stepping.made.clear();
                stepping.handed = 0;
                stepping.done = false;
            }
        }
    }

    /// A copy, counted on `meter` ([`Layer::copy`]); `None` where a decoder's state
    /// cannot be allocated.
    #[cfg(any(feature = "unsafe", feature = "images"))]
    fn copy(&self, meter: &Meter<'_>) -> Result<Option<Self>, Cancelled> {
        Ok(Some(match self {
            #[cfg(feature = "unsafe")]
            Self::Inflating(inflating) => {
                let decoder = match &inflating.decoder {
                    Some(decoder) => {
                        meter.copying(decoder.allocation_size())?;
                        match decoder.try_clone() {
                            Ok(decoder) => Some(decoder),
                            Err(_) => return Ok(None),
                        }
                    }
                    None => None,
                };
                let output = meter.copied(&inflating.output[inflating.handed..inflating.end])?;
                Self::Inflating(Inflating {
                    decoder,
                    zlib: inflating.zlib,
                    done: inflating.done,
                    end: output.len(),
                    output,
                    handed: 0,
                })
            }
            Self::Permissive(permissive) => {
                let input = permissive.stream.input.rest(meter)?;
                Self::Permissive(Box::new(Permissive {
                    stream: permissive.stream.copy(input, meter)?,
                    done: permissive.done,
                }))
            }
            Self::Bytewise(stepping) => {
                meter.copying(stepping.decoder.heap_bytes())?;
                Self::Bytewise(Stepping {
                    fed: stepping.fed.rest(meter)?,
                    decoder: stepping.decoder.clone(),
                    made: meter.copied(&stepping.made[stepping.handed..])?,
                    handed: 0,
                    done: stepping.done,
                })
            }
        }))
    }
}

#[cfg(feature = "unsafe")]
impl Inflating {
    fn rewind(&mut self) {
        if let Some(decoder) = &mut self.decoder {
            decoder.reset(self.zlib);
        }
        self.done = false;
        (self.handed, self.end) = (0, 0);
    }

    /// Ends the data, dropping the decoder.
    fn finish(&mut self) {
        self.done = true;
        self.decoder = None;
    }

    /// Inflates the next bytes. Each call to the decoder counts the bytes it took and
    /// made, so a run of input that inflates to nothing is counted too.
    fn step(
        &mut self,
        under: &mut Under<'_, '_>,
        inflated: &mut u64,
        meter: &Meter<'_>,
    ) -> Result<Step, Cancelled> {
        if self.done {
            self.output = Vec::new();
            (self.handed, self.end) = (0, 0);
            return Ok(Step::Done);
        }
        loop {
            if under.out().is_empty() {
                if !under.ended() {
                    return Ok(Step::Starved);
                }
                // Data that end before any byte decode to nothing, whichever the
                // decoder: no decoder is made for them.
                if self.decoder.as_ref().is_none_or(|d| d.total_in() == 0) {
                    self.finish();
                    return Ok(Step::Done);
                }
            }
            let zlib = self.zlib;
            let decoder = self
                .decoder
                .get_or_insert_with(|| zlib_rs::Inflate::new(zlib, 15));
            if self.output.len() < OUTPUT_CHUNK {
                self.output.resize(OUTPUT_CHUNK, 0);
            }
            let input = under.out();
            let input = &input[..input.len().min(INPUT_CHUNK)];
            let flush = match input.is_empty() {
                true => zlib_rs::InflateFlush::Finish,
                false => zlib_rs::InflateFlush::NoFlush,
            };
            let (out_before, in_before) = (decoder.total_out(), decoder.total_in());
            let result = decoder.decompress(input, &mut self.output, flush);
            let written = (decoder.total_out() - out_before) as usize;
            let taken = (decoder.total_in() - in_before) as usize;
            let made_any = decoder.total_out() > 0;
            under.hand(taken);
            (self.handed, self.end) = (0, written);
            *inflated += written as u64;
            meter.work(written + taken)?;
            match result {
                Ok(zlib_rs::Status::StreamEnd) => self.finish(),
                Ok(_) if written > 0 => {}
                Ok(_) if taken > 0 => continue,
                Ok(_) => self.finish(),
                Err(_) if !made_any => return Ok(Step::Restart),
                Err(_) => self.finish(),
            }
            return Ok(match written {
                0 => Step::Done,
                _ => Step::Made,
            });
        }
    }
}

impl Permissive {
    fn step(
        &mut self,
        under: &mut Under<'_, '_>,
        inflated: &mut u64,
        meter: &Meter<'_>,
    ) -> Result<Step, Cancelled> {
        if self.done {
            return Ok(Step::Done);
        }
        loop {
            self.stream.input.top_up(under);
            self.stream.step(meter, PERMISSIVE_STEP)?;
            let made = self.stream.pending().len();
            *inflated += made as u64;
            if made > 0 {
                return Ok(Step::Made);
            }
            if !self.stream.input.low() {
                // Done: its window is no longer needed.
                self.done = true;
                self.stream = FlateStream::stepping(Fed::default(), PERMISSIVE_STEP);
                return Ok(Step::Done);
            }
            if under.out().is_empty() {
                return Ok(Step::Starved);
            }
        }
    }
}

impl Stepping {
    fn step(
        &mut self,
        under: &mut Under<'_, '_>,
        inflated: &mut u64,
        meter: &Meter<'_>,
    ) -> Result<Step, Cancelled> {
        if self.done {
            self.made = Vec::new();
            self.handed = 0;
            return Ok(Step::Done);
        }
        loop {
            self.fed.top_up(under);
            self.made.clear();
            self.handed = 0;
            let state = self
                .decoder
                .make(&mut self.fed, &mut self.made, POLL_BYTES, false, meter);
            // A stop ends the decoder's work as the end of the data would.
            if meter.cancelled() {
                return Err(Cancelled);
            }
            *inflated += self.made.len() as u64;
            if state != State::Going {
                self.done = true;
                self.fed = Fed::default();
            }
            if !self.made.is_empty() {
                return Ok(Step::Made);
            }
            if self.done {
                return Ok(Step::Done);
            }
            // It stopped with too few bytes at hand: those still below come first.
            if under.out().is_empty() {
                return Ok(Step::Starved);
            }
        }
    }
}

impl Fed {
    /// Takes as many bytes of `under` as it holds at most, after dropping those the
    /// decoder took.
    fn top_up(&mut self, under: &mut Under<'_, '_>) {
        if self.at > 0 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        while self.buf.len() < FED_AT_MOST {
            let bytes = under.out();
            if bytes.is_empty() {
                break;
            }
            let n = bytes.len().min(FED_AT_MOST - self.buf.len());
            self.buf.extend_from_slice(&bytes[..n]);
            under.hand(n);
        }
        self.ended = under.ended();
    }

    /// A copy of the bytes not taken, counted on `meter`.
    #[cfg(any(feature = "unsafe", feature = "images"))]
    fn rest(&self, meter: &Meter<'_>) -> Result<Self, Cancelled> {
        Ok(Self {
            buf: meter.copied(&self.buf[self.at..])?,
            at: 0,
            taken: self.taken,
            ended: self.ended,
        })
    }

    /// Whether the decoder ran out of bytes before their end, which a decoder
    /// stopping where the input is [`Input::low`] does not.
    fn check(&self, held: bool) {
        debug_assert!(held || self.ended, "a step read past the bytes at hand");
    }
}

/// The decoder counts the bytes it takes.
impl Input for Fed {
    #[inline]
    fn next(&mut self, _: &Meter<'_>) -> Option<u8> {
        let byte = self.buf.get(self.at).copied();
        self.check(byte.is_some());
        self.at += usize::from(byte.is_some());
        self.taken += usize::from(byte.is_some());
        byte
    }

    #[inline]
    fn peek(&mut self, ahead: usize, _: &Meter<'_>) -> Option<u8> {
        let byte = self.buf.get(self.at + ahead).copied();
        self.check(byte.is_some());
        byte
    }

    fn taken(&self) -> usize {
        self.taken
    }

    fn take(&mut self, max: usize, _: &Meter<'_>) -> &[u8] {
        let n = max.min(self.buf.len() - self.at);
        self.at += n;
        self.taken += n;
        &self.buf[self.at - n..self.at]
    }

    fn low(&self) -> bool {
        !self.ended && self.buf.len() - self.at < LOOKAHEAD
    }
}

#[cfg(test)]
#[path = "../../tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::DecodedReader;
    use super::fixtures::{noise, stored_zlib};
    use crate::crypto::reader::DecryptedReader;
    use crate::filter::lzw_flate::{PredictorParams, apply_predictor, flate};
    use crate::work::{Cancelled, Meter, POLL_BYTES};
    use alloc::borrow::Cow;
    use alloc::vec::Vec;

    /// Everything `reader` yields, read `chunk` bytes at a time.
    pub(crate) fn read_all(mut reader: DecodedReader<'_>, chunk: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = alloc::vec![0; chunk];
        loop {
            match reader.read(&mut buf, &Meter::default()) {
                Ok(0) => return out,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(Cancelled) => unreachable!(),
            }
        }
    }

    fn params(predictor: u8, colors: u8, bpc: u8, columns: usize) -> PredictorParams {
        PredictorParams {
            predictor,
            colors,
            bits_per_component: bpc,
            columns,
            early_change: true,
        }
    }

    /// The reader's bytes equal the whole-data decoder's for `encoded` under `p`, read
    /// in pieces of every size from 1 to 9 bytes and in one large piece.
    fn same_as_whole(encoded: &[u8], p: &PredictorParams) {
        let whole = flate::fallback::decode_lenient(encoded, &Meter::default())
            .ok()
            .and_then(|d| apply_predictor(d, p, &Meter::default()));
        #[cfg(feature = "unsafe")]
        let whole = {
            use std::io::Read;
            let mut d = Vec::new();
            flate2::read::ZlibDecoder::new(encoded)
                .read_to_end(&mut d)
                .ok()
                .and_then(|_| apply_predictor(d, p, &Meter::default()))
                .or(whole)
        };
        for chunk in (1..10).chain([1 << 16]) {
            let reader = DecodedReader::flate(DecryptedReader::raw(Cow::Borrowed(encoded)), p)
                .expect("streamable");
            assert_eq!(Some(read_all(reader, chunk)), whole, "chunk {chunk}");
        }
    }

    #[test]
    fn predicted_rows_match_the_whole_data_decoder() {
        // Every PNG row filter, the TIFF predictor, 1 to 5 colours, packed PNG rows,
        // PNG rows of no whole number of pixels.
        for (predictor, colors, bpc, columns) in [
            (12, 5, 8, 3),
            (13, 3, 4, 3),
            (2, 5, 8, 2),
            (10, 1, 8, 7),
            (11, 3, 8, 5),
            (12, 1, 8, 9),
            (13, 4, 8, 3),
            (14, 3, 8, 11),
            (15, 2, 8, 6),
            (15, 1, 1, 13),
            (15, 1, 4, 10),
            (15, 3, 16, 4),
            (2, 3, 8, 5),
            (2, 1, 8, 1),
        ] {
            let p = params(predictor, colors, bpc, columns);
            let row = (columns * colors as usize * bpc as usize).div_ceil(8)
                + usize::from(predictor >= 10);
            for rows in [0, 1, 5] {
                let mut data = noise(row * rows, rows as u32 + u32::from(predictor));
                if predictor >= 10 {
                    // Filter bytes 0 to 4, and an unknown one.
                    for (i, r) in data.chunks_mut(row).enumerate() {
                        r[0] = (i % 6) as u8;
                    }
                }
                // Whole rows, then a row cut short, which is kept.
                for cut in [0, row / 2] {
                    let mut data = data.clone();
                    data.extend(noise(cut, 99));
                    same_as_whole(&stored_zlib(&data, 7), &p);
                }
            }
        }
    }

    #[test]
    fn unpredicted_data_matches_the_whole_data_decoder() {
        let data = noise(100_000, 3);
        same_as_whole(&stored_zlib(&data, 65_535), &params(1, 1, 8, 1));
        same_as_whole(&stored_zlib(&[], 1), &params(1, 1, 8, 1));
        // Cut short: no end of stream, no checksum.
        let encoded = stored_zlib(&data, 4096);
        same_as_whole(&encoded[..50_000], &params(1, 1, 8, 1));
    }

    /// TIFF prediction of 16-bit components, which only the whole-data decoder
    /// reverses, is not read incrementally.
    #[test]
    fn tiff_prediction_of_16_bits_is_left_to_the_whole_data_decoder() {
        for p in [params(2, 3, 16, 3), params(2, 1, 16, 5)] {
            assert!(DecodedReader::flate(DecryptedReader::raw(Cow::Borrowed(&[])), &p).is_none());
        }
    }

    /// TIFF prediction of components of 1, 2 and 4 bits reads as the whole-data
    /// decoder reverses it, rows whole and a last row cut short at every length, in
    /// reads of any size.
    #[test]
    fn tiff_prediction_of_fewer_bits_reads_as_whole_decoding() {
        for (colors, bpc, columns) in [(1, 4, 3), (2, 1, 7), (1, 2, 5), (3, 2, 3), (1, 1, 45)] {
            let p = params(2, colors, bpc, columns);
            let row = (usize::from(colors) * usize::from(bpc) * columns).div_ceil(8);
            for len in 0..=4 * row {
                let data = noise(len, len as u32 + 1);
                let encoded = stored_zlib(&data, 65_535);
                let whole = apply_predictor(data.clone(), &p, &Meter::default()).unwrap();
                for chunk in [1, 3, 4096] {
                    let reader =
                        DecodedReader::flate(DecryptedReader::raw(Cow::Borrowed(&encoded)), &p)
                            .unwrap();
                    assert_eq!(
                        read_all(reader, chunk),
                        whole,
                        "{colors} {bpc} {columns} {len}"
                    );
                }
            }
        }
    }

    /// Rewound after reading part of the data or after a restart, a reader yields the
    /// data again from its start, with the decoder the restart chose.
    /// Handing out the bytes of a decoded predictor row counts as work: once the hook
    /// asks to stop, reads of a row of any width stop within a bounded amount of it.
    #[test]
    fn reading_a_decoded_row_is_cancelled_within_bounded_work() {
        for columns in [1 << 18, 1 << 21] {
            let mut row = noise(columns + 1, 5);
            row[0] = 4;
            let encoded = stored_zlib(&row, 65_535);
            let stop = core::cell::Cell::new(false);
            let hook = || stop.get();
            let meter = Meter::new(Some(&hook));
            let mut reader = DecodedReader::flate(
                DecryptedReader::raw(Cow::Borrowed(&encoded)),
                &params(15, 1, 8, columns),
            )
            .unwrap();
            let mut buf = alloc::vec![0; 1 << 16];
            assert_eq!(reader.read(&mut buf[..1], &meter), Ok(1));
            stop.set(true);
            let mut read = 0;
            let result = loop {
                match reader.read(&mut buf, &meter) {
                    Ok(n) if n > 0 => read += n,
                    other => break other,
                }
            };
            assert!(read < 2 * POLL_BYTES, "{read} bytes read after the stop");
            assert_eq!(result, Err(Cancelled));
        }
    }

    #[test]
    fn rewinding_reads_the_data_again() {
        let data = noise(5000, 3);
        let mut reader = DecodedReader::raw(DecryptedReader::raw(Cow::Borrowed(&data)));
        let mut buf = alloc::vec![0; 1234];
        assert_eq!(reader.read(&mut buf, &Meter::default()), Ok(1234));
        reader.rewind();
        assert_eq!(read_all(reader, 777), data);
    }

    #[cfg(feature = "unsafe")]
    mod compressed {
        use super::super::fixtures::stored_deflate;
        use super::*;
        use std::io::Write;

        pub(crate) fn zlib(data: &[u8]) -> Vec<u8> {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
            e.write_all(data).unwrap();
            e.finish().unwrap()
        }

        #[test]
        fn compressed_data_matches_the_whole_data_decoder() {
            // Repetitive enough to use back references across reads.
            let data: Vec<u8> = (0..300_000_u32).map(|i| ((i / 7) % 251) as u8).collect();
            same_as_whole(&zlib(&data), &params(1, 1, 8, 1));
            let rows: Vec<u8> = data
                .chunks(301)
                .flat_map(|r| core::iter::once(4).chain(r[1..].iter().copied()))
                .collect();
            same_as_whole(&zlib(&rows), &params(15, 3, 8, 100));
        }

        /// A zlib stream whose second block is invalid, after a first one that decodes:
        /// the reader keeps to zlib and ends where zlib found the data corrupt, with
        /// what `flate::decode` returns.
        #[test]
        fn data_corrupt_after_a_decoded_block_ends_there() {
            let data = noise(5000, 7);
            let mut encoded = stored_zlib(&data, 5000);
            // The first block is no longer the last; a block of the reserved type follows.
            encoded[2] = 0;
            let end = encoded.len() - 4;
            encoded.insert(end, 0b111);
            let expected = flate::decode(
                &encoded,
                &PredictorParams::default(),
                &Meter::default(),
                false,
                0,
            );
            assert_eq!(expected.as_ref(), Some(&data));
            for chunk in [1, 1000, 1 << 16] {
                let reader = DecodedReader::flate(
                    DecryptedReader::raw(Cow::Borrowed(&encoded)),
                    &params(1, 1, 8, 1),
                )
                .unwrap();
                assert_eq!(Some(read_all(reader, chunk)), expected);
            }
        }

        /// A valid zlib header, then data whose zlib reading breaks late while raw
        /// deflate reads all of it (the header's two bytes start a stored block): both
        /// the reader and whole decoding keep to the zlib reading, whose first bytes the
        /// reader hands out before the break is found.
        #[test]
        fn a_zlib_reading_that_breaks_late_is_kept() {
            let encoded = zlib_that_raw_deflate_reads_otherwise();
            let raw = {
                use std::io::Read;
                let mut d = Vec::new();
                flate2::read::DeflateDecoder::new(&encoded[..])
                    .read_to_end(&mut d)
                    .unwrap();
                d
            };
            assert_eq!(raw[..4], [0x01, 0x55, 0x55, 0x55]);
            let whole = flate::decode(
                &encoded,
                &PredictorParams::default(),
                &Meter::default(),
                false,
                0,
            )
            .unwrap();
            assert_eq!(whole[..4], [0xff, 0xff, 0, 0]);
            assert_ne!(whole, raw);
            for chunk in [1, 7, 4096, 1 << 16] {
                let reader = DecodedReader::flate(
                    DecryptedReader::raw(Cow::Borrowed(&encoded)),
                    &params(1, 1, 8, 1),
                )
                .unwrap();
                assert_eq!(read_all(reader, chunk), whole, "chunk {chunk}");
            }
        }

        /// Data that zlib reads for 130,988 bytes and then finds corrupt (an invalid
        /// code lengths set), and that raw deflate reads whole as other bytes.
        pub(crate) fn zlib_that_raw_deflate_reads_otherwise() -> Vec<u8> {
            let mut payload = alloc::vec![0x55; 65535];
            payload[65530..].copy_from_slice(&[0x00, 0xff, 0xff, 0x00, 0x00]);
            let mut encoded = alloc::vec![
                0x78, 0x01, 0x00, 0xfe, 0xff, 0x01, 0x00, 0xff, 0xff, 0x00, 0x00
            ];
            encoded.extend(payload);
            encoded.extend([0x00, 0xff, 0xff, 0x00, 0x00]);
            encoded.extend(core::iter::repeat_n(0x55, 65535));
            encoded.extend([0x01, 0x00, 0x00, 0xff, 0xff]);
            encoded
        }

        /// Valid zlib data with a byte changed at offsets spread over it: the reader,
        /// in pieces of several sizes, yields what lenient whole decoding returns,
        /// whichever reading that kept and wherever it found the data corrupt.
        #[test]
        fn damaged_zlib_data_reads_as_whole_decoding() {
            let data: Vec<u8> = (0..200_000_u32)
                .map(|i| ((i / 3) % 251) as u8 ^ (i % 7) as u8)
                .collect();
            let encoded = zlib(&data);
            let mut damaged_somewhere = 0;
            for at in (0..encoded.len())
                .step_by(97)
                .chain([0, 1, 2, encoded.len() - 1])
            {
                for flip in [0xff, 0x10, 0x01] {
                    let mut damaged = encoded.clone();
                    damaged[at] ^= flip;
                    let whole = flate::decode(
                        &damaged,
                        &PredictorParams::default(),
                        &Meter::default(),
                        false,
                        0,
                    )
                    .unwrap();
                    damaged_somewhere += usize::from(whole != data);
                    for chunk in [1000, 1 << 16] {
                        let reader = DecodedReader::flate(
                            DecryptedReader::raw(Cow::Borrowed(&damaged)),
                            &params(1, 1, 8, 1),
                        )
                        .unwrap();
                        assert_eq!(read_all(reader, chunk), whole, "byte {at} ^ {flip:#x}");
                    }
                }
            }
            assert!(damaged_somewhere > 0);
        }

        /// A hook that asks to stop from its second poll on.
        fn second_poll() -> impl Fn() -> bool {
            let polls = core::cell::Cell::new(0);
            move || {
                polls.set(polls.get() + 1);
                polls.get() > 1
            }
        }

        /// Read with a hook that asks to stop from its second poll on, `encoded` under
        /// `p` is cancelled within a bounded amount of work, whatever its size: the
        /// bytes taken (stored, and by the permissive decoder) and those made (inflated,
        /// and by the permissive decoder) each stay under three polls' work. Rewound,
        /// the reader then yields what whole decoding does, as does a fresh reader.
        fn cancelled_within_bounded_work(encoded: &[u8], p: &PredictorParams) {
            let whole = flate::decode(
                encoded,
                &PredictorParams::default(),
                &Meter::default(),
                false,
                0,
            )
            .and_then(|d| apply_predictor(d, p, &Meter::default()));
            let hook = second_poll();
            let meter = Meter::new(Some(&hook));
            let mut reader =
                DecodedReader::flate(DecryptedReader::raw(Cow::Borrowed(encoded)), p).unwrap();
            flate::fallback::WORK.set((0, 0));
            let mut buf = alloc::vec![0; 1 << 16];
            let result = loop {
                match reader.read(&mut buf, &meter) {
                    Ok(n) if n > 0 => {}
                    other => break other,
                }
            };
            assert_eq!(result, Err(Cancelled));
            let (fallback_taken, fallback_made) = flate::fallback::WORK.get();
            let taken = reader.stored_read() + fallback_taken;
            let made = reader.inflated_bytes() as usize + fallback_made;
            assert!(
                taken < 3 * POLL_BYTES && made < 3 * POLL_BYTES,
                "{taken} bytes taken, {made} made"
            );
            reader.rewind();
            assert_eq!(Some(read_all(reader, 1 << 16)), whole);
            let fresh =
                DecodedReader::flate(DecryptedReader::raw(Cow::Borrowed(encoded)), p).unwrap();
            assert_eq!(Some(read_all(fresh, 1 << 16)), whole);
        }

        /// A predictor row of two million bytes, whole or cut short, a run of empty
        /// deflate blocks, and data only the permissive decoder reads (stored blocks
        /// whose lengths do not check) are each cancelled within bounded work.
        #[test]
        fn reading_is_cancelled_within_bounded_work() {
            let columns = 2_000_000;
            let mut row = noise(columns + 1, 5);
            row[0] = 4;
            let png = params(15, 1, 8, columns);
            cancelled_within_bounded_work(&stored_zlib(&row, 65_535), &png);
            cancelled_within_bounded_work(&stored_zlib(&row[..columns / 2], 65_535), &png);

            let mut empty = alloc::vec![0x78, 0x01];
            for _ in 0..400_000 {
                empty.extend([0, 0, 0, 0xff, 0xff]);
            }
            let data = noise(10, 6);
            let mut last = stored_zlib(&data, 10);
            empty.extend(last.drain(2..));
            cancelled_within_bounded_work(&empty, &params(1, 1, 8, 1));

            let mut unchecked = stored_zlib(&noise(2_000_000, 7), 65_535);
            for block in (2..unchecked.len() - 4).step_by(65_535 + 5) {
                unchecked[block + 3..block + 5].fill(1);
            }
            cancelled_within_bounded_work(&unchecked, &params(1, 1, 8, 1));
        }

        /// Raw deflate (no zlib header) is read as such from the first read, as zlib
        /// finds the data corrupt before making a byte; a rewind keeps that decoder.
        #[test]
        fn raw_deflate_is_read_from_the_first_read_and_after_a_rewind() {
            let data = noise(5000, 3);
            let deflate = stored_deflate(&data, 1000);
            let mut reader = DecodedReader::flate(
                DecryptedReader::raw(Cow::Borrowed(&deflate)),
                &PredictorParams::default(),
            )
            .unwrap();
            let mut buf = alloc::vec![0; 1234];
            assert_eq!(reader.read(&mut buf, &Meter::default()), Ok(1234));
            assert_eq!(buf[..], data[..1234]);
            reader.rewind();
            let mut out = Vec::new();
            loop {
                match reader.read(&mut buf, &Meter::default()) {
                    Ok(0) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                    Err(e) => panic!("{e:?} after a rewind"),
                }
            }
            assert_eq!(out, data);
        }
    }
}
