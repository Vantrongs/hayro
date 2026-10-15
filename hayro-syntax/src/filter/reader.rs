//! Stream data decoded incrementally, in memory bounded by the decoder's state.

#[cfg(feature = "unsafe")]
use crate::crypto::reader::Cursor;
use crate::crypto::reader::DecryptedReader;
use crate::filter::lzw_flate::{PredictorParams, Unpredictor, flate};
#[cfg(feature = "unsafe")]
use crate::object::stream::OwnedStream;
use crate::work::{Cancelled, Meter, POLL_BYTES};
use alloc::borrow::Cow;
use alloc::vec::Vec;

/// How many decrypted encoded bytes the Flate decoder is given at a time.
#[cfg(feature = "unsafe")]
const INPUT_CHUNK: usize = 8192;

/// How many bytes the Flate decoder makes at a time for reads that ask for fewer. Calls
/// that make less are slower: each copies what it made into zlib's history window, and
/// matches reaching back past its output read from there.
#[cfg(feature = "unsafe")]
const OUTPUT_CHUNK: usize = POLL_BYTES;

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
/// the decoder's state, up to [`POLL_BYTES`] inflated ahead of the reads and a
/// predictor row, not the whole result, except where a corrupt Flate stream needs the
/// permissive decoder, which decodes it whole. Every
/// read counts its work on a [`Meter`], and stops when the meter's hook asks to.
pub struct DecodedReader<'a> {
    source: Source<'a>,
    predictor: Option<Predicted>,
    inflated: u64,
}

enum Source<'a> {
    /// The stored bytes, or everything the permissive Flate decoder produced.
    Raw(DecryptedReader<'a>),
    #[cfg(feature = "unsafe")]
    Inflating(Inflating<'a>),
    /// Flate data for the permissive decoder, which decodes it whole at the next read.
    Fallback(DecryptedReader<'a>),
    Failed,
}

/// Flate data inflated as it is read.
#[cfg(feature = "unsafe")]
struct Inflating<'a> {
    data: DecryptedReader<'a>,
    /// Decrypted encoded bytes, of which the decoder has taken `consumed`.
    input: Vec<u8>,
    consumed: usize,
    /// Inflated bytes, of which `handed` were read.
    output: Vec<u8>,
    handed: usize,
    decoder: zlib_rs::Inflate,
    /// Whether the data is read as zlib; after that fails, as raw deflate.
    zlib: bool,
}

/// A position in a stream's Flate data to resume reading from (see
/// [`DecodedReader::checkpoint`]): the decoder's state and history window, the
/// decryption cursor, and the input, output and predictor rows in flight. The stored bytes are
/// not copied; the checkpoint keeps its stream, and with it those bytes, alive.
#[cfg(feature = "unsafe")]
pub struct DecodedCheckpoint {
    /// Keeps the bytes at `data` alive, so no other bytes can take their address.
    _stream: OwnedStream,
    /// The address and length of the stored bytes.
    data: (usize, usize),
    decoder: zlib_rs::Inflate,
    zlib: bool,
    cursor: Cursor,
    /// Decrypted encoded bytes the decoder has not taken yet.
    input: Vec<u8>,
    /// Inflated bytes not read yet.
    output: Vec<u8>,
    predictor: Option<Predicted>,
}

/// A predictor reversed row by row on top of the source.
#[derive(Clone)]
struct Predicted {
    /// Holds the last decoded row, of which `taken` bytes were handed out.
    unpredictor: Unpredictor,
    /// The encoded row being read, reversed as far as its bytes allow.
    input: Vec<u8>,
    taken: usize,
}

impl<'a> DecodedReader<'a> {
    /// The data as stored, without filters.
    pub(crate) fn raw(data: DecryptedReader<'a>) -> Self {
        Self {
            source: Source::Raw(data),
            predictor: None,
            inflated: 0,
        }
    }

    /// Flate data with its predictor `params`; `None` when the predictor is one only
    /// the whole-data decoder handles.
    pub(crate) fn flate(data: DecryptedReader<'a>, params: &PredictorParams) -> Option<Self> {
        let predictor = match params.predictor {
            1 => None,
            _ => Some(Predicted {
                unpredictor: Unpredictor::new(params)?,
                input: Vec::new(),
                taken: 0,
            }),
        };
        #[cfg(feature = "unsafe")]
        let source = Source::Inflating(Inflating {
            data,
            input: Vec::new(),
            consumed: 0,
            output: Vec::new(),
            handed: 0,
            decoder: zlib_rs::Inflate::new(true, 15),
            zlib: true,
        });
        #[cfg(not(feature = "unsafe"))]
        let source = Source::Fallback(data);
        Some(Self {
            source,
            predictor,
            inflated: 0,
        })
    }

    /// The bytes this reader's incremental Flate decoder produced, including those a
    /// restart voided and those not read yet; not those the permissive decoder
    /// produced.
    pub fn inflated_bytes(&self) -> u64 {
        self.inflated
    }

    /// The most heap bytes a checkpoint of this reader holds
    /// ([`DecodedCheckpoint::heap_bytes`]), at any position; `None` when the reader
    /// cannot be checkpointed (see [`Self::checkpoint`]).
    #[cfg(feature = "unsafe")]
    pub fn checkpoint_heap_bound(&self) -> Option<usize> {
        let (inflating, _) = self.checkpointable()?;
        let predictor = match &self.predictor {
            Some(p) => p.heap_bound()?,
            None => 0,
        };
        (inflating.decoder.allocation_size() + INPUT_CHUNK + OUTPUT_CHUNK).checked_add(predictor)
    }

    /// The reader's position, to resume from with [`Self::restore`] on a reader of the
    /// same `stream`. `None` unless the reader inflates `stream`'s stored bytes
    /// incrementally (not as stored, nor after a restart to the permissive decoder),
    /// or when the decoder state cannot be allocated.
    #[cfg(feature = "unsafe")]
    pub fn checkpoint(&self, stream: &OwnedStream) -> Option<DecodedCheckpoint> {
        let (inflating, data) = self.checkpointable()?;
        if !stream.holds(data) {
            return None;
        }
        Some(DecodedCheckpoint {
            _stream: stream.clone(),
            data: address(data),
            decoder: inflating.decoder.try_clone().ok()?,
            zlib: inflating.zlib,
            cursor: inflating.data.cursor(),
            input: inflating.input[inflating.consumed..].to_vec(),
            output: inflating.output[inflating.handed..].to_vec(),
            predictor: self.predictor.clone(),
        })
    }

    /// Continues from `checkpoint`, as if the data before it had been read (the
    /// restarts that preceded it included). False, with the reader unchanged, when the
    /// checkpoint is of other bytes or its decoder state cannot be allocated.
    #[cfg(feature = "unsafe")]
    #[must_use]
    pub fn restore(&mut self, checkpoint: &DecodedCheckpoint) -> bool {
        let Source::Inflating(inflating) = &mut self.source else {
            return false;
        };
        if inflating.data.borrowed_data().map(address) != Some(checkpoint.data) {
            return false;
        }
        let Ok(decoder) = checkpoint.decoder.try_clone() else {
            return false;
        };
        inflating.decoder = decoder;
        inflating.zlib = checkpoint.zlib;
        inflating.data.restore(&checkpoint.cursor);
        inflating.input.clone_from(&checkpoint.input);
        inflating.consumed = 0;
        inflating.output.clone_from(&checkpoint.output);
        inflating.handed = 0;
        self.predictor.clone_from(&checkpoint.predictor);
        true
    }

    /// The incremental Flate decoder, and the document's bytes it reads.
    #[cfg(feature = "unsafe")]
    fn checkpointable(&self) -> Option<(&Inflating<'a>, &'a [u8])> {
        match &self.source {
            Source::Inflating(inflating) => Some((inflating, inflating.data.borrowed_data()?)),
            _ => None,
        }
    }

    /// The stored bytes read.
    #[cfg(all(test, feature = "unsafe"))]
    fn stored_read(&self) -> usize {
        match &self.source {
            Source::Raw(data) | Source::Fallback(data) => data.position(),
            #[cfg(feature = "unsafe")]
            Source::Inflating(inflating) => inflating.data.position(),
            Source::Failed => 0,
        }
    }

    /// Back to the start of the data, keeping the decoder a restart chose (so reading
    /// again does not repeat the restarts).
    pub fn rewind(&mut self) {
        if let Some(p) = &mut self.predictor {
            p.reset();
        }
        self.source.rewind();
    }

    /// Fills `buf` with the next decoded bytes and returns how many: at most
    /// [`POLL_BYTES`], 0 only at the end. The work counts on `meter`; once its hook
    /// asked to stop, every read fails.
    pub fn read(&mut self, buf: &mut [u8], meter: &Meter<'_>) -> Result<usize, ReadError> {
        let len = buf.len().min(POLL_BYTES);
        let buf = &mut buf[..len];
        if buf.is_empty() {
            return Ok(0);
        }
        // Bytes inflated ahead were counted when they were made, so handing them out
        // counts nothing that would find the stop.
        if meter.cancelled() {
            return Err(ReadError::Cancelled);
        }
        let Some(p) = &mut self.predictor else {
            return self.source.read(buf, &mut self.inflated, meter);
        };
        while p.taken == p.unpredictor.row().len() {
            if !p.fill(&mut self.source, &mut self.inflated, meter)? {
                return Ok(0);
            }
        }
        let row = &p.unpredictor.row()[p.taken..];
        let n = buf.len().min(row.len());
        meter.work(n)?;
        buf[..n].copy_from_slice(&row[..n]);
        p.taken += n;
        Ok(n)
    }
}

#[cfg(feature = "unsafe")]
impl DecodedCheckpoint {
    /// The bytes it holds on the heap: the decoder's state and window, and the input,
    /// output and rows it copied.
    pub fn heap_bytes(&self) -> usize {
        self.decoder.allocation_size()
            + self.input.capacity()
            + self.output.capacity()
            + self.predictor.as_ref().map_or(0, Predicted::heap_bytes)
    }
}

/// Identifies stored bytes by where they are.
#[cfg(feature = "unsafe")]
fn address(data: &[u8]) -> (usize, usize) {
    (data.as_ptr() as usize, data.len())
}

impl Predicted {
    #[cfg(feature = "unsafe")]
    fn heap_bytes(&self) -> usize {
        self.input.capacity() + self.unpredictor.heap_bytes()
    }

    /// The most a copy of these rows holds: the encoded row being read and the last
    /// one decoded (a copy's capacities are its lengths).
    #[cfg(feature = "unsafe")]
    fn heap_bound(&self) -> Option<usize> {
        self.unpredictor.input_len().checked_mul(2)
    }

    fn reset(&mut self) {
        self.input.clear();
        self.taken = 0;
        self.unpredictor.reset();
    }

    /// Reads the rest of the next row, reversing the predictor as its bytes arrive;
    /// false if the data ends first (a last row cut short is dropped, as
    /// `apply_predictor` drops it). The row grows with the bytes read, so a declared
    /// width alone allocates nothing.
    fn fill(
        &mut self,
        source: &mut Source<'_>,
        inflated: &mut u64,
        meter: &Meter<'_>,
    ) -> Result<bool, ReadError> {
        let mut chunk = [0; 8192];
        let len = self.unpredictor.input_len();
        while self.input.len() < len {
            let wanted = (len - self.input.len()).min(chunk.len());
            match source.read(&mut chunk[..wanted], inflated, meter) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.input.extend_from_slice(&chunk[..n]);
                    self.unpredictor.advance(&mut self.input, POLL_BYTES);
                }
                Err(e) => {
                    self.reset();
                    return Err(e);
                }
            }
        }
        self.unpredictor.finish_row(&mut self.input);
        self.taken = 0;
        Ok(true)
    }
}

impl Source<'_> {
    fn rewind(&mut self) {
        match self {
            Source::Raw(data) => data.rewind(),
            #[cfg(feature = "unsafe")]
            Source::Inflating(inflating) => inflating.rewind(),
            Source::Fallback(_) | Source::Failed => {}
        }
    }

    fn read(
        &mut self,
        buf: &mut [u8],
        inflated: &mut u64,
        meter: &Meter<'_>,
    ) -> Result<usize, ReadError> {
        match self {
            Source::Raw(data) => {
                let n = data.read(buf);
                meter.work(n)?;
                Ok(n)
            }
            #[cfg(feature = "unsafe")]
            Source::Inflating(inflating) => match inflating.read(buf, inflated, meter)? {
                Some(n) => Ok(n),
                None => {
                    self.restart();
                    Err(ReadError::Restarted)
                }
            },
            Source::Fallback(data) => {
                *self = fallback(data, meter)?;
                self.read(buf, inflated, meter)
            }
            Source::Failed => Err(ReadError::Failed),
        }
    }

    /// Moves on to the next decoder in the order whole decoding tries them: zlib, raw
    /// deflate, then the permissive decoder over all of the data.
    #[cfg(feature = "unsafe")]
    fn restart(&mut self) {
        if let Source::Inflating(inflating) = self
            && inflating.zlib
        {
            inflating.zlib = false;
            inflating.rewind();
            return;
        }
        if let Source::Inflating(inflating) = core::mem::replace(self, Source::Failed) {
            warn!("flate stream is broken, decoding with fallback");
            *self = Source::Fallback(inflating.data);
        }
    }
}

/// Everything the permissive decoder makes of `data`, as stored bytes.
fn fallback(
    data: &mut DecryptedReader<'_>,
    meter: &Meter<'_>,
) -> Result<Source<'static>, Cancelled> {
    let decrypted;
    let bytes = match data.plain() {
        Some(bytes) => bytes,
        None => {
            decrypted = data.decrypt_all(meter)?;
            &decrypted
        }
    };
    Ok(match flate::fallback::decode(bytes, meter, false)? {
        Some(decoded) => Source::Raw(DecryptedReader::raw(Cow::Owned(decoded))),
        None => Source::Failed,
    })
}

#[cfg(feature = "unsafe")]
impl Inflating<'_> {
    fn rewind(&mut self) {
        self.decoder.reset(self.zlib);
        self.data.rewind();
        self.input.clear();
        self.consumed = 0;
        self.output.clear();
        self.handed = 0;
    }

    /// Reads inflated bytes into `buf`: those made before and not read yet, else new
    /// ones, made straight into `buf` when it takes a whole chunk; `None` when the data
    /// is corrupt for this decoder.
    fn read(
        &mut self,
        buf: &mut [u8],
        inflated: &mut u64,
        meter: &Meter<'_>,
    ) -> Result<Option<usize>, Cancelled> {
        if self.handed == self.output.len() {
            if buf.len() >= OUTPUT_CHUNK {
                return self.inflate(buf, inflated, meter);
            }
            let mut output = core::mem::take(&mut self.output);
            output.resize(OUTPUT_CHUNK, 0);
            let made = self.inflate(&mut output, inflated, meter);
            output.truncate(match made {
                Ok(Some(n)) => n,
                _ => 0,
            });
            self.output = output;
            self.handed = 0;
            if made?.is_none() {
                return Ok(None);
            }
        }
        let n = buf.len().min(self.output.len() - self.handed);
        buf[..n].copy_from_slice(&self.output[self.handed..][..n]);
        self.handed += n;
        Ok(Some(n))
    }

    /// Inflates into `buf`; `None` when the data is corrupt for this decoder. Each
    /// step counts the bytes it took and made, so a run of input that inflates to
    /// nothing is counted too.
    fn inflate(
        &mut self,
        buf: &mut [u8],
        inflated: &mut u64,
        meter: &Meter<'_>,
    ) -> Result<Option<usize>, Cancelled> {
        loop {
            if self.consumed == self.input.len() {
                self.input.resize(INPUT_CHUNK, 0);
                let n = self.data.read(&mut self.input);
                self.input.truncate(n);
                self.consumed = 0;
            }
            let input = &self.input[self.consumed..];
            let flush = if input.is_empty() {
                zlib_rs::InflateFlush::Finish
            } else {
                zlib_rs::InflateFlush::NoFlush
            };
            let (out_before, in_before) = (self.decoder.total_out(), self.decoder.total_in());
            let result = self.decoder.decompress(input, buf, flush);
            let written = (self.decoder.total_out() - out_before) as usize;
            let taken = (self.decoder.total_in() - in_before) as usize;
            self.consumed += taken;
            *inflated += written as u64;
            meter.work(written + taken)?;
            match result {
                Ok(zlib_rs::Status::StreamEnd) => return Ok(Some(written)),
                Ok(_) if written > 0 => return Ok(Some(written)),
                Ok(_) if taken > 0 => continue,
                _ => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::fixtures::{noise, stored_zlib};
    use super::{DecodedReader, ReadError};
    use crate::crypto::reader::DecryptedReader;
    use crate::filter::lzw_flate::{PredictorParams, apply_predictor, flate};
    use crate::work::{Meter, POLL_BYTES};
    use alloc::borrow::Cow;
    use alloc::vec::Vec;

    /// Everything `reader` yields, read `chunk` bytes at a time; a restart discards
    /// what was read before it.
    pub(crate) fn read_all(mut reader: DecodedReader<'_>, chunk: usize) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = alloc::vec![0; chunk];
        loop {
            match reader.read(&mut buf, &Meter::default()) {
                Ok(0) => return Some(out),
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(ReadError::Restarted) => out.clear(),
                Err(ReadError::Failed) => return None,
                Err(ReadError::Cancelled) => unreachable!(),
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
        let whole = flate::fallback::decode(encoded, &Meter::default(), false)
            .unwrap()
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
            assert_eq!(read_all(reader, chunk), whole, "chunk {chunk}");
        }
    }

    #[test]
    fn predicted_rows_match_the_whole_data_decoder() {
        // Every PNG row filter, the TIFF predictor, 1 to 4 colours, packed PNG rows.
        for (predictor, colors, bpc, columns) in [
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
                // Whole rows, then a row cut short, which is dropped.
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

    /// Predictors and layouts only the whole-data decoder handles are not read
    /// incrementally.
    #[test]
    fn other_predictors_are_left_to_the_whole_data_decoder() {
        for p in [
            params(2, 1, 4, 3),
            params(2, 3, 16, 3),
            params(3, 1, 8, 3),
            params(15, 3, 8, 0),
            params(15, 5, 8, 3),
            params(11, 1, 3, 3),
            params(15, 1, 8, usize::MAX),
            params(15, 3, 16, usize::MAX),
            params(2, 1, 8, 0),
        ] {
            assert!(DecodedReader::flate(DecryptedReader::raw(Cow::Borrowed(&[])), &p).is_none());
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
            assert_eq!(result, Err(ReadError::Cancelled));
        }
    }

    #[test]
    fn rewinding_reads_the_data_again() {
        let data = noise(5000, 3);
        let mut reader = DecodedReader::raw(DecryptedReader::raw(Cow::Borrowed(&data)));
        let mut buf = alloc::vec![0; 1234];
        assert_eq!(reader.read(&mut buf, &Meter::default()), Ok(1234));
        reader.rewind();
        assert_eq!(read_all(reader, 777).unwrap(), data);
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
        /// the reader restarts with raw deflate, then the permissive decoder, and ends
        /// with what `flate::decode` returns.
        #[test]
        fn corrupt_data_restarts_as_flate_decode_does() {
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
            );
            let mut reader = DecodedReader::flate(
                DecryptedReader::raw(Cow::Borrowed(&encoded)),
                &params(1, 1, 8, 1),
            )
            .unwrap();
            let mut restarts = 0;
            let mut out = Vec::new();
            let mut buf = alloc::vec![0; 1000];
            loop {
                match reader.read(&mut buf, &Meter::default()) {
                    Ok(0) => break,
                    Ok(k) => out.extend_from_slice(&buf[..k]),
                    Err(ReadError::Restarted) => {
                        restarts += 1;
                        out.clear();
                    }
                    Err(e) => panic!("{e:?}"),
                }
            }
            assert_eq!(restarts, 2);
            assert_eq!(Some(out), expected);
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
                    Err(ReadError::Restarted) => {}
                    other => break other,
                }
            };
            assert_eq!(result, Err(ReadError::Cancelled));
            let (fallback_taken, fallback_made) = flate::fallback::WORK.get();
            let taken = reader.stored_read() + fallback_taken;
            let made = reader.inflated_bytes() as usize + fallback_made;
            assert!(
                taken < 3 * POLL_BYTES && made < 3 * POLL_BYTES,
                "{taken} bytes taken, {made} made"
            );
            reader.rewind();
            assert_eq!(read_all(reader, 1 << 16), whole);
            let fresh =
                DecodedReader::flate(DecryptedReader::raw(Cow::Borrowed(encoded)), p).unwrap();
            assert_eq!(read_all(fresh, 1 << 16), whole);
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

        /// Raw deflate (no zlib header) restarts once; a rewind keeps the decoder it
        /// chose.
        #[test]
        fn a_rewind_after_a_restart_keeps_the_chosen_decoder() {
            let data = noise(5000, 3);
            let deflate = stored_deflate(&data, 1000);
            let mut reader = DecodedReader::flate(
                DecryptedReader::raw(Cow::Borrowed(&deflate)),
                &PredictorParams::default(),
            )
            .unwrap();
            let mut buf = alloc::vec![0; 1234];
            assert_eq!(
                reader.read(&mut buf, &Meter::default()),
                Err(ReadError::Restarted)
            );
            assert_eq!(reader.read(&mut buf, &Meter::default()), Ok(1234));
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
