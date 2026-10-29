use crate::filter::png::{self, RowFilter};
use crate::object::dict::keys::{BITS_PER_COMPONENT, COLORS, COLUMNS, EARLY_CHANGE, PREDICTOR};
use crate::object::{Dict, Object};
use crate::work::{Cancelled, Meter, POLL_BYTES};
use alloc::vec::Vec;
use core::ops::Range;

pub(crate) mod flate {
    use super::*;
    use crate::filter::lzw_flate::{PredictorParams, apply_predictor};

    /// The data inflated and its predictor reversed, the work counted on `meter`;
    /// `None` once its hook asks to stop, or, if `strict`, where the data is corrupt
    /// rather than cut short (see [`fallback::decode_within`]). `expected` is the size the
    /// inflated data should have, 0 where none is known: room is not reserved past
    /// it while the data is shorter ([`inflate`]).
    ///
    /// The data is read as zlib, else as raw deflate, else by the permissive decoder.
    /// If `strict`, a reading that finds the data corrupt gives way to the next one.
    /// Otherwise a reading that made a byte is kept, up to where it found the data
    /// corrupt: what it made from the data read so far cannot change with the data
    /// after, so a reader that hands out bytes as they are made
    /// ([`DecodedReader`](crate::filter::reader::DecodedReader)) yields these bytes.
    /// `MuPDF` keeps the zlib reading of such data too (observed in `PyMuPDF` 1.28.2).
    pub(crate) fn decode(
        data: &[u8],
        params: &PredictorParams,
        meter: &Meter<'_>,
        strict: bool,
        expected: usize,
    ) -> Option<Vec<u8>> {
        decode_within(data, params, meter, strict, expected, usize::MAX).unwrap_or(None)
    }

    /// `decoded` holding no room past its bytes, which a caller may keep while it
    /// reads them.
    fn fitted(mut decoded: Vec<u8>) -> Vec<u8> {
        decoded.shrink_to_fit();
        decoded
    }

    /// A decoder made more than the limit it was given; `tried` decoders before it
    /// found the data corrupt (zlib, then raw deflate, then the permissive one; only
    /// the last without the `unsafe` feature).
    pub(crate) struct Larger {
        pub(crate) tried: usize,
    }

    /// [`decode`], stopped where a decoder made more than `limit` bytes.
    #[cfg(feature = "unsafe")]
    pub(crate) fn decode_within(
        data: &[u8],
        params: &PredictorParams,
        meter: &Meter<'_>,
        strict: bool,
        expected: usize,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, Larger> {
        let larger = |tried| move |_| Larger { tried };
        let mut decoded = None;
        for (tried, zlib) in [(0, true), (1, false)] {
            match inflate(data, zlib, meter, expected, limit).map_err(larger(tried))? {
                Inflated::Whole(made) => decoded = Some(made),
                Inflated::Broken(made) if !strict && !made.is_empty() => decoded = Some(made),
                Inflated::Broken(_) => continue,
                Inflated::Cancelled => return Ok(None),
            }
            break;
        }
        let decoded = match decoded {
            Some(decoded) => decoded,
            None => {
                warn!("flate stream is broken, decoding with fallback");
                match fallback::decode_within(data, meter, strict, limit) {
                    Ok(Some(decoded)) => decoded,
                    Ok(None) | Err(fallback::Stopped::Cancelled) => return Ok(None),
                    Err(fallback::Stopped::Larger) => return Err(Larger { tried: 2 }),
                }
            }
        };
        Ok(apply_predictor(decoded, params, meter).map(fitted))
    }

    /// How a reading of the data as zlib or as raw deflate ended.
    #[cfg(feature = "unsafe")]
    enum Inflated {
        /// It read through the stream, or to the end of the data.
        Whole(Vec<u8>),
        /// It found the data corrupt for its format after making these bytes.
        Broken(Vec<u8>),
        /// The meter's hook asked to stop.
        Cancelled,
    }

    /// `data` inflated whole, read as zlib if `zlib`, else as raw deflate, the work
    /// counted on `meter`. Data that ends before its stream does yields what it holds,
    /// and bytes after the stream's end are ignored.
    /// `Err` once it made more than `limit` bytes. Its room grows as upstream's does
    /// ([`room`]), but while it made less than `expected` no further than that and a
    /// little more, which the end of the stream is read into: the room of a large
    /// image's data ends at its size, and other data holds no more than upstream.
    #[cfg(feature = "unsafe")]
    fn inflate(
        data: &[u8],
        zlib: bool,
        meter: &Meter<'_>,
        expected: usize,
        limit: usize,
    ) -> Result<Inflated, ()> {
        if meter
            .making_state(crate::filter::reader::inflate_heap())
            .is_err()
        {
            return Ok(Inflated::Cancelled);
        }
        let mut decoder = zlib_rs::Inflate::new(zlib, 15);
        let mut out = Vec::new();
        // The input bytes the decoder took, and those it was offered and counted.
        let (mut taken, mut offered) = (0, 0);
        loop {
            if out.len() == out.capacity() {
                let made = out.len();
                let room = room(made, data.len(), expected, limit) - made;
                meter.reserve_exact(&mut out, room);
            }
            // Input is offered half of POLL_BYTES at a time and counted before it is
            // taken, and output is made no further than the next poll, so at most
            // POLL_BYTES and one piece of input are worked on between two polls.
            // Calls that make less are slower: each copies what it made into zlib's
            // history window, and matches reaching back past its output read from
            // there.
            let end = data.len().min(taken + POLL_BYTES / 2);
            if meter.work(end.saturating_sub(offered)).is_err() {
                return Ok(Inflated::Cancelled);
            }
            offered = offered.max(end);
            let input = &data[taken..end];
            let len = out.len();
            let room = (out.capacity() - len).min(meter.until_poll());
            out.resize(len + room, 0);
            let flush = if input.is_empty() {
                zlib_rs::InflateFlush::Finish
            } else {
                zlib_rs::InflateFlush::NoFlush
            };
            let (out_before, in_before) = (decoder.total_out(), decoder.total_in());
            let status = decoder.decompress(input, &mut out[len..], flush);
            let written = (decoder.total_out() - out_before) as usize;
            let took = (decoder.total_in() - in_before) as usize;
            out.truncate(len + written);
            taken += took;
            if meter.work(written).is_err() {
                return Ok(Inflated::Cancelled);
            }
            match status {
                Ok(_) if out.len() > limit => return Err(()),
                Ok(zlib_rs::Status::StreamEnd) => return Ok(Inflated::Whole(out)),
                Ok(_) if written > 0 || took > 0 => {}
                Ok(_) if input.is_empty() => return Ok(Inflated::Whole(out)),
                _ => return Ok(Inflated::Broken(out)),
            }
        }
    }

    /// The room a full output buffer holding the `made` bytes inflated from `len`
    /// encoded bytes grows to. Upstream (036fb21c) inflates with `read_to_end`, whose
    /// room doubles from 32 bytes when it is full: the smallest power of two above
    /// what it holds. So does this room, so that it is never larger than upstream's
    /// at the same point of the same data, however the data inflate along it; it
    /// only stops short of upstream's at what the data should make, while they make
    /// less, and at a little past the limit. The first room may be larger than
    /// upstream's, up to 32 KiB, as this decoder needs that much less than upstream's
    /// decoder and its input buffer; it is at least what zlib's fast loop needs.
    #[cfg(feature = "unsafe")]
    fn room(made: usize, len: usize, expected: usize, limit: usize) -> usize {
        let doubled = match made {
            0 => len.clamp(1024, POLL_BYTES / 2),
            _ => made
                .checked_add(1)
                .and_then(usize::checked_next_power_of_two)
                .unwrap_or(usize::MAX),
        };
        let end = if made < expected {
            expected.min(limit)
        } else {
            limit
        };
        doubled.min(end.saturating_add(1024))
    }

    /// [`decode`], stopped where the decoder made more than `limit` bytes.
    #[cfg(not(feature = "unsafe"))]
    pub(crate) fn decode_within(
        data: &[u8],
        params: &PredictorParams,
        meter: &Meter<'_>,
        strict: bool,
        _expected: usize,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, Larger> {
        let decoded = match fallback::decode_within(data, meter, strict, limit) {
            Ok(Some(decoded)) => decoded,
            Ok(None) | Err(fallback::Stopped::Cancelled) => return Ok(None),
            Err(fallback::Stopped::Larger) => return Err(Larger { tried: 0 }),
        };
        Ok(apply_predictor(decoded, params, meter).map(fitted))
    }

    /// Ported from <https://github.com/mozilla/pdf.js/blob/master/src/core/flate_stream.js>
    /// TODO: Rewrite this in idiomatic Rust.
    pub(crate) mod fallback {
        use crate::work::{Cancelled, Meter};
        use alloc::vec;
        use alloc::vec::Vec;

        /// The data decoded, its work counted on `meter`. Decoding stops at the first
        /// thing that is no valid deflate data and keeps what came before. Data that
        /// ends early, or with a wrong or missing checksum, is not corrupt: what it
        /// holds is all decoded.
        #[cfg(test)]
        pub(crate) fn decode_lenient(data: &[u8], meter: &Meter<'_>) -> Result<Vec<u8>, Cancelled> {
            match run(data, meter, usize::MAX) {
                Ok((decoded, _)) => Ok(decoded),
                Err(_) => Err(Cancelled),
            }
        }

        /// The most bytes a step makes past the bytes it was asked for: a match.
        const STEP_PAST: usize = 257;

        /// Why [`decode_within`] stopped.
        pub(crate) enum Stopped {
            Cancelled,
            /// It made more than its limit.
            Larger,
        }

        /// [`decode_lenient`], stopped once it made more than `limit` bytes; if
        /// `strict`, `None` where the data is corrupt.
        pub(crate) fn decode_within(
            data: &[u8],
            meter: &Meter<'_>,
            strict: bool,
            limit: usize,
        ) -> Result<Option<Vec<u8>>, Stopped> {
            let (decoded, corrupt) = run(data, meter, limit)?;
            if corrupt && strict {
                warn!("flate stream is corrupt");
                return Ok(None);
            }
            Ok(Some(decoded))
        }

        /// The bytes decoded, stopped once they are more than `limit`, and whether the
        /// decoder stopped at data that is no valid deflate.
        fn run(data: &[u8], meter: &Meter<'_>, limit: usize) -> Result<(Vec<u8>, bool), Stopped> {
            let mut stream = FlateStream::new(Slice::new(data));
            let pending = limit.saturating_add(1);
            stream.most = pending.saturating_add(STEP_PAST);
            let stepped = stream.step(meter, pending);
            #[cfg(test)]
            WORK.set((stream.input.taken(), stream.made()));
            stepped.map_err(|_| Stopped::Cancelled)?;
            if stream.made() > limit {
                return Err(Stopped::Larger);
            }
            Ok((core::mem::take(&mut stream.output), stream.corrupt))
        }

        /// Where the decoder takes its bytes from, one at a time.
        pub(crate) trait Input {
            /// The next byte; `None` at the end.
            fn next(&mut self, meter: &Meter<'_>) -> Option<u8>;
            /// The byte `ahead` bytes after the next one, which stays next.
            fn peek(&mut self, ahead: usize, meter: &Meter<'_>) -> Option<u8>;
            /// The bytes taken so far.
            fn taken(&self) -> usize;
            /// Takes the next bytes, at most `max` and at least one where any is left.
            fn take(&mut self, max: usize, meter: &Meter<'_>) -> &[u8];
            /// Whether fewer bytes are at hand than the longest step of a decoder
            /// reads, and more are to come: a decoder then stops before its next step,
            /// to go on once more are at hand. Input that holds all of its data never
            /// is.
            fn low(&self) -> bool {
                false
            }
        }

        /// Bytes all at hand.
        pub(crate) struct Slice<'a> {
            data: &'a [u8],
            at: usize,
        }

        impl<'a> Slice<'a> {
            pub(crate) fn new(data: &'a [u8]) -> Self {
                Self { data, at: 0 }
            }
        }

        impl Input for Slice<'_> {
            fn next(&mut self, _: &Meter<'_>) -> Option<u8> {
                let byte = *self.data.get(self.at)?;
                self.at += 1;
                Some(byte)
            }

            fn peek(&mut self, ahead: usize, _: &Meter<'_>) -> Option<u8> {
                self.data.get(self.at + ahead).copied()
            }

            fn taken(&self) -> usize {
                self.at
            }

            fn take(&mut self, max: usize, _: &Meter<'_>) -> &[u8] {
                let n = max.min(self.data.len() - self.at);
                self.at += n;
                &self.data[self.at - n..self.at]
            }
        }

        #[cfg(test)]
        std::thread_local! {
            /// The bytes the last whole decoding on this thread took and made.
            pub(crate) static WORK: core::cell::Cell<(usize, usize)> =
                const { core::cell::Cell::new((0, 0)) };
        }

        /// The decoder, which can stop between two codes and go on later
        /// ([`Self::step`]), so that it is read a part at a time: its output is the
        /// history back references reach into, then the bytes not handed out yet. It
        /// also stops before a block or a code where its input is [`Input::low`]: the
        /// longest, a block's header and tables, reads fewer than 600 bytes.
        pub(crate) struct FlateStream<I: Input> {
            pub(crate) input: I,
            code_buf: u32,
            code_size: u8,
            /// The bytes made, from the `dropped`th on.
            output: Vec<u8>,
            /// The room `output` grows to at most where that holds what is made
            /// ([`grow`]).
            most: usize,
            dropped: usize,
            /// The bytes of `output` handed out ([`Self::hand`]).
            handed: usize,
            /// Whether the zlib header was looked for.
            started: bool,
            /// The tables of the compressed block being decoded, and the bytes taken
            /// when it started.
            block: Option<(HuffmanTable, HuffmanTable, usize)>,
            /// The bytes of the stored block being copied left to copy.
            stored: usize,
            eof: bool,
            /// Whether decoding stopped at, or passed over, data that is no valid
            /// deflate, rather than at the end of the stream or of the data.
            pub(crate) corrupt: bool,
        }

        /// The farthest a back reference reaches.
        const WINDOW: usize = 32 * 1024;

        /// Makes room in `output` for `n` more bytes where it has none, as pushing
        /// them grows a `Vec` (as upstream's decoder grows its output), to at least
        /// twice the room; but no further than `most` where that holds them. The room
        /// is made on `meter` ([`Meter::reserve_exact`]).
        #[inline]
        fn grow(output: &mut Vec<u8>, n: usize, most: usize, meter: &Meter<'_>) {
            let (len, room) = (output.len(), output.capacity());
            if len + n > room {
                let doubled = (2 * room).max(len + n).max(8);
                meter.reserve_exact(output, doubled.min(most.max(len + n)) - len);
            }
        }

        impl<I: Input> FlateStream<I> {
            pub(crate) fn new(input: I) -> Self {
                Self {
                    input,
                    code_buf: 0,
                    code_size: 0,
                    output: Vec::new(),
                    most: usize::MAX,
                    dropped: 0,
                    handed: 0,
                    started: false,
                    block: None,
                    stored: 0,
                    eof: false,
                    corrupt: false,
                }
            }

            /// A decoder of `input` whose bytes are handed out as it makes them, each
            /// [`Self::step`] asked for at most `step` pending bytes: its output never
            /// needs more room than the history and a step's bytes.
            pub(crate) fn stepping(input: I, step: usize) -> Self {
                let mut stream = Self::new(input);
                stream.most = 2 * WINDOW + step + STEP_PAST;
                stream
            }

            /// The bytes made in all.
            fn made(&self) -> usize {
                self.dropped + self.output.len()
            }

            /// A copy reading `input`, its output and tables copied on `meter`
            /// ([`Meter::copied`]).
            #[cfg(any(feature = "unsafe", feature = "images"))]
            pub(crate) fn copy(&self, input: I, meter: &Meter<'_>) -> Result<Self, Cancelled> {
                let table = |table: &HuffmanTable| {
                    Ok::<_, Cancelled>(HuffmanTable {
                        codes: meter.copied(&table.codes)?,
                        max_len: table.max_len,
                    })
                };
                let block = match &self.block {
                    Some((lengths, distances, at)) => {
                        Some((table(lengths)?, table(distances)?, *at))
                    }
                    None => None,
                };
                Ok(Self {
                    input,
                    code_buf: self.code_buf,
                    code_size: self.code_size,
                    output: meter.copied(&self.output)?,
                    most: self.most,
                    dropped: self.dropped,
                    handed: self.handed,
                    started: self.started,
                    block,
                    stored: self.stored,
                    eof: self.eof,
                    corrupt: self.corrupt,
                })
            }

            /// The bytes made and not handed out yet.
            pub(crate) fn pending(&self) -> &[u8] {
                &self.output[self.handed..]
            }

            /// Hands out `n` pending bytes, and drops what back references can no
            /// longer reach of what was handed out.
            pub(crate) fn hand(&mut self, n: usize) {
                self.handed += n;
                if self.handed >= 2 * WINDOW {
                    let drop = self.handed - WINDOW;
                    self.output.drain(..drop);
                    self.dropped += drop;
                    self.handed -= drop;
                }
            }

            /// Whether all data was decoded.
            pub(crate) fn done(&self) -> bool {
                self.started && self.block.is_none() && self.stored == 0 && self.eof
            }

            /// Decodes until the end of the data or of the stream, or until at least
            /// `pending` bytes made are not handed out: each block counts the bytes it
            /// took, and at least one, when it ends.
            pub(crate) fn step(
                &mut self,
                meter: &Meter<'_>,
                pending: usize,
            ) -> Result<(), Cancelled> {
                if !self.started {
                    if self.input.low() {
                        return Ok(());
                    }
                    self.started = true;
                    self.skip_header(meter);
                }
                loop {
                    if self.stored > 0 {
                        let from = self.input.taken();
                        let copied = self.copy_stored(meter, pending);
                        meter.work(self.input.taken() - from + usize::from(copied))?;
                        if !copied {
                            return Ok(());
                        }
                    }
                    if let Some((lit, dist, from)) = self.block.take() {
                        if !self.read_codes(lit, dist, from, meter, pending)? {
                            return Ok(());
                        }
                        meter.work(self.input.taken() - from + 1)?;
                    }
                    if !self.eof && self.input.low() {
                        return Ok(());
                    }
                    if self.eof || self.input.peek(0, meter).is_none() {
                        self.eof = true;
                        return Ok(());
                    }
                    if self.output.len() - self.handed >= pending {
                        return Ok(());
                    }
                    let from = self.input.taken();
                    self.read_block(meter, pending)?;
                    if self.block.is_some() {
                        return Ok(());
                    }
                    if self.stored > 0 {
                        // The block's header; its data count as they are copied.
                        meter.work(self.input.taken() - from)?;
                        continue;
                    }
                    meter.work(self.input.taken() - from + 1)?;
                }
            }

            /// Takes a zlib header, if the data starts with one.
            fn skip_header(&mut self, meter: &Meter<'_>) {
                if let (Some(cmf), Some(flg)) =
                    (self.input.peek(0, meter), self.input.peek(1, meter))
                    && (cmf & 0x0f) == 0x08
                    && ((cmf as u16) << 8 | flg as u16).is_multiple_of(31)
                    && (flg & 0x20) == 0
                {
                    self.input.next(meter);
                    self.input.next(meter);
                }
            }

            /// Stops decoding at data that is no valid deflate.
            fn corrupt(&mut self) {
                self.corrupt = true;
                self.eof = true;
            }

            fn get_bits(&mut self, bits: u8, meter: &Meter<'_>) -> Option<u32> {
                while self.code_size < bits {
                    let b = self.input.next(meter)?;
                    self.code_buf |= (b as u32) << self.code_size;
                    self.code_size += 8;
                }

                let result = self.code_buf & ((1 << bits) - 1);
                self.code_buf >>= bits;
                self.code_size -= bits;

                Some(result)
            }

            fn get_code(&mut self, table: &HuffmanTable, meter: &Meter<'_>) -> Option<u16> {
                let codes = &table.codes;
                let max_len = table.max_len;

                while self.code_size < max_len {
                    if let Some(b) = self.input.next(meter) {
                        self.code_buf |= (b as u32) << self.code_size;
                        self.code_size += 8;
                    } else {
                        // Premature end of stream
                        break;
                    }
                }

                let code = codes.get((self.code_buf & ((1 << max_len) - 1)) as usize)?;
                let code_len = (code >> 16) as u8;
                let code_val = code & 0xffff;

                if code_len < 1 {
                    // No code of the table starts with these bits.
                    self.corrupt = true;
                    return None;
                }
                if self.code_size < code_len {
                    return None;
                }

                self.code_buf >>= code_len;
                self.code_size -= code_len;

                Some(code_val as u16)
            }

            /// Reads a block, or, for a compressed one, its tables and its codes up to
            /// `pending` bytes not handed out ([`Self::read_codes`]).
            fn read_block(&mut self, meter: &Meter<'_>, pending: usize) -> Result<(), Cancelled> {
                let from = self.input.taken();
                // Read block header
                let hdr = match self.get_bits(3, meter) {
                    Some(h) => h,
                    None => {
                        warn!("bad block header in flate stream");
                        self.eof = true;
                        return Ok(());
                    }
                };

                if (hdr & 1) != 0 {
                    self.eof = true;
                }

                let hdr = hdr >> 1;

                match hdr {
                    0 => self.read_uncompressed_block(meter),
                    1 | 2 => {
                        let (lit, dist) = if hdr == 1 {
                            (get_fixed_lit_table(), get_fixed_dist_table())
                        } else {
                            match self.read_dynamic_tables(meter) {
                                Some(tables) => tables,
                                None => {
                                    self.eof = true;
                                    return Ok(());
                                }
                            }
                        };
                        meter.work(lit.codes.len() + dist.codes.len())?;
                        if !self.read_codes(lit, dist, from, meter, pending)? {
                            return Ok(());
                        }
                    }
                    _ => {
                        warn!("unknown block type in flate stream");
                        self.corrupt();
                    }
                }
                Ok(())
            }

            fn read_uncompressed_block(&mut self, meter: &Meter<'_>) {
                // Skip any remaining bits in current byte
                self.code_buf = 0;
                self.code_size = 0;

                let mut bytes = [0_u16; 4];
                for byte in &mut bytes {
                    match self.input.next(meter) {
                        Some(b) => *byte = b as u16,
                        None => {
                            warn!("bad block header in flate stream");
                            self.eof = true;
                            return;
                        }
                    }
                }
                let [len_low, len_high, nlen_low, nlen_high] = bytes;
                let block_len = len_low | (len_high << 8);
                let check = nlen_low | (nlen_high << 8);

                if check != !block_len && (block_len != 0 || check != 0) {
                    // Ignoring error for bad "empty" block
                    warn!("bad uncompressed block length in flate stream");
                    self.corrupt = true;
                }

                if block_len == 0 {
                    if self.input.peek(0, meter).is_none() {
                        self.eof = true;
                    }
                } else {
                    self.stored = usize::from(block_len);
                }
            }

            /// Copies the stored block's data until at least `pending` bytes made are
            /// not handed out: to its end, or the end of the data (true), or not, or
            /// until no byte is at hand while more are to come (false).
            fn copy_stored(&mut self, meter: &Meter<'_>, pending: usize) -> bool {
                while self.stored > 0 {
                    let room = pending.saturating_sub(self.output.len() - self.handed);
                    if room == 0 {
                        return false;
                    }
                    let low = self.input.low();
                    let bytes = self.input.take(self.stored.min(room), meter);
                    if bytes.is_empty() && low {
                        return false;
                    }
                    if bytes.is_empty() {
                        self.stored = 0;
                        self.eof = true;
                        return true;
                    }
                    self.stored -= bytes.len();
                    grow(&mut self.output, bytes.len(), self.most, meter);
                    self.output.extend_from_slice(bytes);
                }
                true
            }

            /// Reads the codes of a compressed block whose tables are `lit` and `dist`
            /// and which started when `from` bytes were taken: to its end (true), or
            /// until `pending` bytes made are not handed out, keeping the block to go
            /// on with (false). Counts each code's output.
            fn read_codes(
                &mut self,
                lit_code_table: HuffmanTable,
                dist_code_table: HuffmanTable,
                from: usize,
                meter: &Meter<'_>,
                pending: usize,
            ) -> Result<bool, Cancelled> {
                loop {
                    if self.output.len() - self.handed >= pending || self.input.low() {
                        self.block = Some((lit_code_table, dist_code_table, from));
                        return Ok(false);
                    }
                    let code1 = match self.get_code(&lit_code_table, meter) {
                        Some(c) => c,
                        None => {
                            self.eof = true;
                            return Ok(true);
                        }
                    };

                    if code1 < 256 {
                        grow(&mut self.output, 1, self.most, meter);
                        self.output.push(code1 as u8);
                        meter.work(1)?;
                    } else if code1 == 256 {
                        return Ok(true);
                    } else {
                        let code1 = code1 - 257;
                        let Some(&length_info) = LENGTH_DECODE.get(code1 as usize) else {
                            warn!("invalid length code {} in flate stream", code1 + 257);
                            self.corrupt();
                            return Ok(true);
                        };
                        let extra_bits = (length_info >> 16) as u8;
                        let mut length = (length_info & 0xffff) as usize;

                        if extra_bits > 0 {
                            if let Some(extra) = self.get_bits(extra_bits, meter) {
                                length += extra as usize;
                            } else {
                                self.eof = true;
                                return Ok(true);
                            }
                        }

                        let dist_code = match self.get_code(&dist_code_table, meter) {
                            Some(c) => c,
                            None => {
                                self.eof = true;
                                return Ok(true);
                            }
                        };

                        let dist_info = match DIST_DECODE.get(dist_code as usize) {
                            Some(&info) => info,
                            None => {
                                warn!("invalid distance code {} in flate stream", dist_code);

                                self.corrupt();
                                return Ok(true);
                            }
                        };

                        let extra_bits = (dist_info >> 16) as u8;
                        let mut distance = (dist_info & 0xffff) as usize;

                        if extra_bits > 0 {
                            if let Some(extra) = self.get_bits(extra_bits, meter) {
                                distance += extra as usize;
                            } else {
                                self.eof = true;
                                return Ok(true);
                            }
                        }

                        // Copy from previous output; a distance back past the start of
                        // the output copies nothing.
                        if distance > self.made() {
                            self.corrupt = true;
                        } else {
                            for _ in 0..length {
                                let byte = self.output[self.output.len() - distance];
                                grow(&mut self.output, 1, self.most, meter);
                                self.output.push(byte);
                            }
                        }
                        meter.work(length + 1)?;
                    }
                }
            }

            fn read_dynamic_tables(
                &mut self,
                meter: &Meter<'_>,
            ) -> Option<(HuffmanTable, HuffmanTable)> {
                let num_lit_codes = self.get_bits(5, meter)? as usize + 257;
                let num_dist_codes = self.get_bits(5, meter)? as usize + 1;
                let num_code_len_codes = self.get_bits(4, meter)? as usize + 4;

                // Build code length code table
                let mut code_len_code_lengths = vec![0_u8; 19];
                for i in 0..num_code_len_codes {
                    code_len_code_lengths[CODE_LEN_CODE_MAP[i] as usize] =
                        self.get_bits(3, meter)? as u8;
                }

                let code_len_table = generate_huffman_table(&code_len_code_lengths);

                // Read code lengths
                let total_codes = num_lit_codes + num_dist_codes;
                let mut code_lengths = vec![0_u8; total_codes];
                let mut i = 0;

                while i < total_codes {
                    let code = self.get_code(&code_len_table, meter)?;

                    match code {
                        0..=15 => {
                            code_lengths[i] = code as u8;
                            i += 1;
                        }
                        16 => {
                            // Repeat previous
                            let repeat_count = self.get_bits(2, meter)? as usize + 3;
                            let prev = if i > 0 { code_lengths[i - 1] } else { 0 };
                            for _ in 0..repeat_count {
                                if i < total_codes {
                                    code_lengths[i] = prev;
                                    i += 1;
                                }
                            }
                        }
                        17 => {
                            // Repeat zero 3-10 times
                            let repeat_count = self.get_bits(3, meter)? as usize + 3;
                            for _ in 0..repeat_count {
                                if i < total_codes {
                                    code_lengths[i] = 0;
                                    i += 1;
                                }
                            }
                        }
                        18 => {
                            // Repeat zero 11-138 times
                            let repeat_count = self.get_bits(7, meter)? as usize + 11;
                            for _ in 0..repeat_count {
                                if i < total_codes {
                                    code_lengths[i] = 0;
                                    i += 1;
                                }
                            }
                        }
                        _ => {
                            self.corrupt = true;
                            return None;
                        }
                    }
                }

                let lit_table = generate_huffman_table(&code_lengths[..num_lit_codes]);
                let dist_table = generate_huffman_table(&code_lengths[num_lit_codes..]);

                Some((lit_table, dist_table))
            }
        }

        #[derive(Clone)]
        struct HuffmanTable {
            codes: Vec<u32>,
            max_len: u8,
        }

        fn generate_huffman_table(lengths: &[u8]) -> HuffmanTable {
            let _n = lengths.len();

            // Find max code length
            let max_len = lengths.iter().cloned().max().unwrap_or(0);

            if max_len == 0 {
                return HuffmanTable {
                    codes: vec![0; 1],
                    max_len: 1,
                };
            }

            // Build the table
            let size = 1 << max_len;
            let mut codes = vec![0_u32; size];

            let mut code = 0_u32;
            for len in 1..=max_len {
                for (val, &length) in lengths.iter().enumerate() {
                    if length == len {
                        // Bit-reverse the code
                        let mut code2 = 0_u32;
                        let mut t = code;
                        for _ in 0..len {
                            code2 = (code2 << 1) | (t & 1);
                            t >>= 1;
                        }

                        // Fill the table entries
                        let skip = 1 << len;
                        let mut i = code2 as usize;
                        while i < size {
                            codes[i] = ((len as u32) << 16) | (val as u32);
                            i += skip;
                        }
                        code += 1;
                    }
                }
                code <<= 1;
            }

            HuffmanTable { codes, max_len }
        }

        fn get_fixed_lit_table() -> HuffmanTable {
            HuffmanTable {
                codes: FIXED_LIT_CODE_TAB.to_vec(),
                max_len: 9,
            }
        }

        fn get_fixed_dist_table() -> HuffmanTable {
            HuffmanTable {
                codes: FIXED_DIST_CODE_TAB.to_vec(),
                max_len: 5,
            }
        }

        const CODE_LEN_CODE_MAP: [u8; 19] = [
            16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
        ];

        const LENGTH_DECODE: [u32; 29] = [
            0x00003, 0x00004, 0x00005, 0x00006, 0x00007, 0x00008, 0x00009, 0x0000a, 0x1000b,
            0x1000d, 0x1000f, 0x10011, 0x20013, 0x20017, 0x2001b, 0x2001f, 0x30023, 0x3002b,
            0x30033, 0x3003b, 0x40043, 0x40053, 0x40063, 0x40073, 0x50083, 0x500a3, 0x500c3,
            0x500e3, 0x00102,
        ];

        const DIST_DECODE: [u32; 30] = [
            0x00001, 0x00002, 0x00003, 0x00004, 0x10005, 0x10007, 0x20009, 0x2000d, 0x30011,
            0x30019, 0x40021, 0x40031, 0x50041, 0x50061, 0x60081, 0x600c1, 0x70101, 0x70181,
            0x80201, 0x80301, 0x90401, 0x90601, 0xa0801, 0xa0c01, 0xb1001, 0xb1801, 0xc2001,
            0xc3001, 0xd4001, 0xd6001,
        ];

        const FIXED_LIT_CODE_TAB: [u32; 512] = [
            0x70100, 0x80050, 0x80010, 0x80118, 0x70110, 0x80070, 0x80030, 0x900c0, 0x70108,
            0x80060, 0x80020, 0x900a0, 0x80000, 0x80080, 0x80040, 0x900e0, 0x70104, 0x80058,
            0x80018, 0x90090, 0x70114, 0x80078, 0x80038, 0x900d0, 0x7010c, 0x80068, 0x80028,
            0x900b0, 0x80008, 0x80088, 0x80048, 0x900f0, 0x70102, 0x80054, 0x80014, 0x8011c,
            0x70112, 0x80074, 0x80034, 0x900c8, 0x7010a, 0x80064, 0x80024, 0x900a8, 0x80004,
            0x80084, 0x80044, 0x900e8, 0x70106, 0x8005c, 0x8001c, 0x90098, 0x70116, 0x8007c,
            0x8003c, 0x900d8, 0x7010e, 0x8006c, 0x8002c, 0x900b8, 0x8000c, 0x8008c, 0x8004c,
            0x900f8, 0x70101, 0x80052, 0x80012, 0x8011a, 0x70111, 0x80072, 0x80032, 0x900c4,
            0x70109, 0x80062, 0x80022, 0x900a4, 0x80002, 0x80082, 0x80042, 0x900e4, 0x70105,
            0x8005a, 0x8001a, 0x90094, 0x70115, 0x8007a, 0x8003a, 0x900d4, 0x7010d, 0x8006a,
            0x8002a, 0x900b4, 0x8000a, 0x8008a, 0x8004a, 0x900f4, 0x70103, 0x80056, 0x80016,
            0x8011e, 0x70113, 0x80076, 0x80036, 0x900cc, 0x7010b, 0x80066, 0x80026, 0x900ac,
            0x80006, 0x80086, 0x80046, 0x900ec, 0x70107, 0x8005e, 0x8001e, 0x9009c, 0x70117,
            0x8007e, 0x8003e, 0x900dc, 0x7010f, 0x8006e, 0x8002e, 0x900bc, 0x8000e, 0x8008e,
            0x8004e, 0x900fc, 0x70100, 0x80051, 0x80011, 0x80119, 0x70110, 0x80071, 0x80031,
            0x900c2, 0x70108, 0x80061, 0x80021, 0x900a2, 0x80001, 0x80081, 0x80041, 0x900e2,
            0x70104, 0x80059, 0x80019, 0x90092, 0x70114, 0x80079, 0x80039, 0x900d2, 0x7010c,
            0x80069, 0x80029, 0x900b2, 0x80009, 0x80089, 0x80049, 0x900f2, 0x70102, 0x80055,
            0x80015, 0x8011d, 0x70112, 0x80075, 0x80035, 0x900ca, 0x7010a, 0x80065, 0x80025,
            0x900aa, 0x80005, 0x80085, 0x80045, 0x900ea, 0x70106, 0x8005d, 0x8001d, 0x9009a,
            0x70116, 0x8007d, 0x8003d, 0x900da, 0x7010e, 0x8006d, 0x8002d, 0x900ba, 0x8000d,
            0x8008d, 0x8004d, 0x900fa, 0x70101, 0x80053, 0x80013, 0x8011b, 0x70111, 0x80073,
            0x80033, 0x900c6, 0x70109, 0x80063, 0x80023, 0x900a6, 0x80003, 0x80083, 0x80043,
            0x900e6, 0x70105, 0x8005b, 0x8001b, 0x90096, 0x70115, 0x8007b, 0x8003b, 0x900d6,
            0x7010d, 0x8006b, 0x8002b, 0x900b6, 0x8000b, 0x8008b, 0x8004b, 0x900f6, 0x70103,
            0x80057, 0x80017, 0x8011f, 0x70113, 0x80077, 0x80037, 0x900ce, 0x7010b, 0x80067,
            0x80027, 0x900ae, 0x80007, 0x80087, 0x80047, 0x900ee, 0x70107, 0x8005f, 0x8001f,
            0x9009e, 0x70117, 0x8007f, 0x8003f, 0x900de, 0x7010f, 0x8006f, 0x8002f, 0x900be,
            0x8000f, 0x8008f, 0x8004f, 0x900fe, 0x70100, 0x80050, 0x80010, 0x80118, 0x70110,
            0x80070, 0x80030, 0x900c1, 0x70108, 0x80060, 0x80020, 0x900a1, 0x80000, 0x80080,
            0x80040, 0x900e1, 0x70104, 0x80058, 0x80018, 0x90091, 0x70114, 0x80078, 0x80038,
            0x900d1, 0x7010c, 0x80068, 0x80028, 0x900b1, 0x80008, 0x80088, 0x80048, 0x900f1,
            0x70102, 0x80054, 0x80014, 0x8011c, 0x70112, 0x80074, 0x80034, 0x900c9, 0x7010a,
            0x80064, 0x80024, 0x900a9, 0x80004, 0x80084, 0x80044, 0x900e9, 0x70106, 0x8005c,
            0x8001c, 0x90099, 0x70116, 0x8007c, 0x8003c, 0x900d9, 0x7010e, 0x8006c, 0x8002c,
            0x900b9, 0x8000c, 0x8008c, 0x8004c, 0x900f9, 0x70101, 0x80052, 0x80012, 0x8011a,
            0x70111, 0x80072, 0x80032, 0x900c5, 0x70109, 0x80062, 0x80022, 0x900a5, 0x80002,
            0x80082, 0x80042, 0x900e5, 0x70105, 0x8005a, 0x8001a, 0x90095, 0x70115, 0x8007a,
            0x8003a, 0x900d5, 0x7010d, 0x8006a, 0x8002a, 0x900b5, 0x8000a, 0x8008a, 0x8004a,
            0x900f5, 0x70103, 0x80056, 0x80016, 0x8011e, 0x70113, 0x80076, 0x80036, 0x900cd,
            0x7010b, 0x80066, 0x80026, 0x900ad, 0x80006, 0x80086, 0x80046, 0x900ed, 0x70107,
            0x8005e, 0x8001e, 0x9009d, 0x70117, 0x8007e, 0x8003e, 0x900dd, 0x7010f, 0x8006e,
            0x8002e, 0x900bd, 0x8000e, 0x8008e, 0x8004e, 0x900fd, 0x70100, 0x80051, 0x80011,
            0x80119, 0x70110, 0x80071, 0x80031, 0x900c3, 0x70108, 0x80061, 0x80021, 0x900a3,
            0x80001, 0x80081, 0x80041, 0x900e3, 0x70104, 0x80059, 0x80019, 0x90093, 0x70114,
            0x80079, 0x80039, 0x900d3, 0x7010c, 0x80069, 0x80029, 0x900b3, 0x80009, 0x80089,
            0x80049, 0x900f3, 0x70102, 0x80055, 0x80015, 0x8011d, 0x70112, 0x80075, 0x80035,
            0x900cb, 0x7010a, 0x80065, 0x80025, 0x900ab, 0x80005, 0x80085, 0x80045, 0x900eb,
            0x70106, 0x8005d, 0x8001d, 0x9009b, 0x70116, 0x8007d, 0x8003d, 0x900db, 0x7010e,
            0x8006d, 0x8002d, 0x900bb, 0x8000d, 0x8008d, 0x8004d, 0x900fb, 0x70101, 0x80053,
            0x80013, 0x8011b, 0x70111, 0x80073, 0x80033, 0x900c7, 0x70109, 0x80063, 0x80023,
            0x900a7, 0x80003, 0x80083, 0x80043, 0x900e7, 0x70105, 0x8005b, 0x8001b, 0x90097,
            0x70115, 0x8007b, 0x8003b, 0x900d7, 0x7010d, 0x8006b, 0x8002b, 0x900b7, 0x8000b,
            0x8008b, 0x8004b, 0x900f7, 0x70103, 0x80057, 0x80017, 0x8011f, 0x70113, 0x80077,
            0x80037, 0x900cf, 0x7010b, 0x80067, 0x80027, 0x900af, 0x80007, 0x80087, 0x80047,
            0x900ef, 0x70107, 0x8005f, 0x8001f, 0x9009f, 0x70117, 0x8007f, 0x8003f, 0x900df,
            0x7010f, 0x8006f, 0x8002f, 0x900bf, 0x8000f, 0x8008f, 0x8004f, 0x900ff,
        ];

        const FIXED_DIST_CODE_TAB: [u32; 32] = [
            0x50000, 0x50010, 0x50008, 0x50018, 0x50004, 0x50014, 0x5000c, 0x5001c, 0x50002,
            0x50012, 0x5000a, 0x5001a, 0x50006, 0x50016, 0x5000e, 0x00000, 0x50001, 0x50011,
            0x50009, 0x50019, 0x50005, 0x50015, 0x5000d, 0x5001d, 0x50003, 0x50013, 0x5000b,
            0x5001b, 0x50007, 0x50017, 0x5000f, 0x00000,
        ];
    }
}

pub(crate) mod lzw {
    use crate::filter::bytewise;
    use crate::filter::lzw_flate::flate::fallback::Input;
    use crate::filter::lzw_flate::{PredictorParams, apply_predictor};
    use crate::work::Meter;
    use alloc::vec::Vec;

    /// Decode a LZW-encoded stream, its work counted on `meter`; `None` once its hook
    /// asks to stop, or where a strict decoder finds an invalid code.
    pub(crate) fn decode(
        data: &[u8],
        params: &PredictorParams,
        meter: &Meter<'_>,
        strict: bool,
    ) -> Option<Vec<u8>> {
        decode_within(data, params, meter, strict, usize::MAX).unwrap_or(None)
    }

    /// [`decode`], `Err` once it made more than `limit` bytes.
    pub(crate) fn decode_within(
        data: &[u8],
        params: &PredictorParams,
        meter: &Meter<'_>,
        strict: bool,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, ()> {
        let decoder = bytewise::Decoder::lzw(params.early_change);
        let Some(decoded) = bytewise::decode_within(decoder, data, strict, meter, limit)? else {
            return Ok(None);
        };

        Ok(apply_predictor(decoded, params, meter))
    }

    const CLEAR_TABLE: usize = 256;
    const EOD: usize = 257;
    const MAX_ENTRIES: usize = 4096;
    const INITIAL_SIZE: usize = 258;

    /// The decoder, a code at a time ([`Self::step`]), its codes read most
    /// significant bit first.
    #[derive(Clone)]
    pub(crate) struct Decoder {
        table: Table,
        bit_size: u8,
        prev: Option<usize>,
        /// Bits taken from the input and not read yet, the last `bits` of `buffer`.
        buffer: u64,
        bits: u8,
    }

    impl Decoder {
        /// The most heap bytes a decoder holds: its table, full.
        #[cfg(feature = "unsafe")]
        pub(crate) const HEAP_BOUND: usize = MAX_ENTRIES * size_of::<Entry>();

        pub(crate) fn new(early_change: bool) -> Self {
            let table = Table::new(early_change);
            Self {
                bit_size: table.code_length(),
                table,
                prev: None,
                buffer: 0,
                bits: 0,
            }
        }

        pub(crate) fn early_change(&self) -> bool {
            self.table.early_change
        }

        /// The heap bytes it holds.
        #[cfg(any(feature = "unsafe", feature = "images"))]
        pub(crate) fn heap_bytes(&self) -> usize {
            self.table.entries.capacity() * size_of::<Entry>()
        }

        /// The next `bit_size` bits; `None` where the input ends first.
        fn code(&mut self, input: &mut impl Input, meter: &Meter<'_>) -> Option<usize> {
            while self.bits < self.bit_size {
                self.buffer = (self.buffer << 8) | u64::from(input.next(meter)?);
                self.bits += 8;
            }
            self.bits -= self.bit_size;
            Some(((self.buffer >> self.bits) & ((1 << self.bit_size) - 1)) as usize)
        }

        /// Decodes the next code into `decoded`: false at the end of the data,
        /// `None` where it is invalid or the meter's hook asks to stop, with nothing
        /// appended.
        pub(crate) fn step(
            &mut self,
            input: &mut impl Input,
            decoded: &mut Vec<u8>,
            meter: &Meter<'_>,
        ) -> Option<bool> {
            meter.work(1).ok()?;
            let next = match self.code(input, meter) {
                Some(code) => code,
                None => {
                    warn!("premature EOF in LZW stream, EOD code missing");
                    return Some(false);
                }
            };
            let table = &mut self.table;

            match next {
                CLEAR_TABLE => {
                    table.clear();
                    self.prev = None;
                    self.bit_size = table.code_length();
                }
                EOD => return Some(false),
                new => {
                    if new > table.size() {
                        warn!("invalid LZW code: {} (table size: {})", new, table.size());
                        return None;
                    }

                    if new < table.size() {
                        meter.work(table.len(new)).ok()?;
                        table.write(new, decoded);

                        if let Some(prev_code) = self.prev {
                            table.register(prev_code, table.first(new));
                        }
                    } else if let Some(prev_code) = self.prev {
                        let new_entry = table.register(prev_code, table.first(prev_code))?;
                        meter.work(table.len(new_entry)).ok()?;
                        table.write(new_entry, decoded);
                    } else {
                        warn!("LZW decode error: code {new} not found and prev is None");
                        return None;
                    }

                    self.bit_size = table.code_length();
                    self.prev = Some(new);
                }
            }
            Some(true)
        }
    }

    /// The codes past the first 256 bytes and the clear and end codes, each the code
    /// it extends by a byte.
    #[derive(Clone)]
    struct Table {
        early_change: bool,
        /// Code `INITIAL_SIZE + i` is `entries[i]`.
        entries: Vec<Entry>,
    }

    #[derive(Clone, Copy)]
    struct Entry {
        prefix: u16,
        last: u8,
        first: u8,
        len: u16,
    }

    impl Table {
        fn new(early_change: bool) -> Self {
            Self {
                early_change,
                entries: Vec::new(),
            }
        }

        /// The entry of code `code`, a valid one past the first 256.
        fn entry(&self, code: usize) -> Entry {
            self.entries[code - INITIAL_SIZE]
        }

        /// The bytes of valid code `code`.
        fn len(&self, code: usize) -> usize {
            match code {
                0..=255 => 1,
                _ => usize::from(self.entry(code).len),
            }
        }

        /// The first byte of valid code `code`.
        fn first(&self, code: usize) -> u8 {
            match code {
                0..=255 => code as u8,
                _ => self.entry(code).first,
            }
        }

        /// Appends the bytes of valid code `code` to `out`.
        fn write(&self, code: usize, out: &mut Vec<u8>) {
            let start = out.len();
            out.resize(start + self.len(code), 0);
            let mut code = code;
            let mut at = out.len();
            while code >= INITIAL_SIZE {
                let entry = self.entry(code);
                at -= 1;
                out[at] = entry.last;
                code = usize::from(entry.prefix);
            }
            out[start] = code as u8;
        }

        /// Adds the code of valid code `prev`'s bytes and `byte`, if the table has
        /// room; returns it.
        fn register(&mut self, prev: usize, byte: u8) -> Option<usize> {
            if self.size() >= MAX_ENTRIES {
                return None;
            }
            self.entries.push(Entry {
                prefix: prev as u16,
                last: byte,
                first: self.first(prev),
                len: self.len(prev) as u16 + 1,
            });
            Some(self.size() - 1)
        }

        fn clear(&mut self) {
            self.entries.clear();
        }

        fn size(&self) -> usize {
            INITIAL_SIZE + self.entries.len()
        }

        fn code_length(&self) -> u8 {
            const TEN: usize = 512;
            const ELEVEN: usize = 1024;
            const TWELVE: usize = 2048;

            let adjusted = self.size() + usize::from(self.early_change);

            if adjusted >= TWELVE {
                12
            } else if adjusted >= ELEVEN {
                11
            } else if adjusted >= TEN {
                10
            } else {
                9
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PredictorParams {
    /// 1 for none, 2 for TIFF prediction, 10 to 15 for PNG prediction.
    pub(crate) predictor: u8,
    /// 1 to 32.
    pub(crate) colors: u8,
    /// 1, 2, 4, 8 or 16.
    pub(crate) bits_per_component: u8,
    /// At least 1, and few enough that a row's bits fit 32 bits.
    pub(crate) columns: usize,
    pub(crate) early_change: bool,
}

impl Default for PredictorParams {
    fn default() -> Self {
        Self {
            predictor: 1,
            colors: 1,
            bits_per_component: 8,
            columns: 1,
            early_change: true,
        }
    }
}

impl PredictorParams {
    /// The parameters of `dict` as `MuPDF` reads them (observed in `PyMuPDF` 1.28.2):
    /// each number as [`Number::viewer_i32`] gives it, a value that is no number as
    /// absent. A predictor it does not know is none; under a predictor, colors,
    /// bits per component or columns below one are their defaults. `None` where it
    /// refuses the stream: more than 32 colors, bits per component other than 1, 2,
    /// 4, 8 or 16, or so many columns that a row's bits come near 2^31; also where
    /// the reading of a number is not established.
    pub(crate) fn from_params(dict: &Dict<'_>) -> Option<Self> {
        let read = |key: &[u8], default: i32| match dict.get::<Object<'_>>(key) {
            Some(Object::Number(n)) => n.viewer_i32(),
            _ => Some(default),
        };
        let mut params = Self {
            // Read by LZW alone, so a reading not established leaves it the default.
            early_change: read(EARLY_CHANGE, 1).is_none_or(|e| e != 0),
            ..Self::default()
        };
        params.predictor = match read(PREDICTOR, 1)? {
            p @ (2 | 10..=15) => p as u8,
            _ => return Some(params),
        };
        let at_least_one = |v: i32, default: i32| if v < 1 { default } else { v };
        let colors = at_least_one(read(COLORS, 1)?, 1);
        let bpc = at_least_one(read(BITS_PER_COMPONENT, 8)?, 8);
        let columns = at_least_one(read(COLUMNS, 1)?, 1);
        if colors > 32 || !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
            return None;
        }
        let bits = colors * bpc;
        if i64::from(columns) * i64::from(bits) > i64::from(i32::MAX - 7)
            || columns >= i32::MAX / bits
        {
            return None;
        }
        params.colors = colors as u8;
        params.bits_per_component = bpc as u8;
        params.columns = columns as usize;
        Some(params)
    }
}

/// How the rows of a predictor are laid out and reversed.
#[derive(Clone, Copy)]
struct Rows {
    /// The bytes of a decoded row.
    len: usize,
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    /// A row filter byte before each row, which is unfiltered a byte at a time with
    /// pixels of `bpp` bytes.
    Png { bpp: usize },
    /// TIFF prediction of 8-bit components: each byte adds the one `colors` before.
    Tiff8 { colors: usize },
    /// TIFF prediction of components of 1, 2, 4 or 16 bits.
    Tiff { colors: usize, bpc: u8 },
}

impl Rows {
    /// `None` without a predictor.
    fn of(params: &PredictorParams) -> Option<Self> {
        let colors = usize::from(params.colors);
        let bpc = params.bits_per_component;
        let bits = colors * usize::from(bpc);
        let len = (bits * params.columns).div_ceil(8);
        let kind = match params.predictor {
            1 => return None,
            2 if bpc == 8 => Kind::Tiff8 { colors },
            2 => Kind::Tiff { colors, bpc },
            _ => Kind::Png {
                bpp: bits.div_ceil(8),
            },
        };
        Some(Self { len, kind })
    }
}

/// The data with its predictor reversed, in pieces whose work counts on `meter`;
/// `None` once its hook asks to stop. A last row cut short is reversed as far as it
/// goes and kept, as `MuPDF` keeps it: a PNG row needs a byte after its filter byte.
pub(crate) fn apply_predictor(
    data: Vec<u8>,
    params: &PredictorParams,
    meter: &Meter<'_>,
) -> Option<Vec<u8>> {
    let Some(rows) = Rows::of(params) else {
        return Some(data);
    };
    match rows.kind {
        Kind::Png { bpp } => unpredict_png(data, rows.len, bpp, meter),
        Kind::Tiff8 { colors } => unpredict_tiff8(data, rows.len, colors, meter),
        Kind::Tiff { colors, bpc } => {
            let components = colors * params.columns;
            unpredict_tiff(data, rows.len, colors, bpc, components, meter)
        }
    }
}

/// The bytes of a part of a row reversed between two polls: whole pixels of `bpp`.
fn part_len(bpp: usize) -> usize {
    (POLL_BYTES / bpp).max(1) * bpp
}

fn unpredict_png(mut data: Vec<u8>, len: usize, bpp: usize, meter: &Meter<'_>) -> Option<Vec<u8>> {
    let part_len = part_len(bpp);
    let (mut src, mut dst) = (0, 0);
    while src < data.len() {
        let filter = RowFilter::from_u8(data[src]);
        // The row's bytes after its filter byte, fewer in a last row cut short.
        let k = (data.len() - src - 1).min(len);
        for start in (0..k).step_by(part_len) {
            let end = (start + part_len).min(k);
            // The row moves left, so the bytes a part overwrites were moved before.
            data.copy_within(src + 1 + start..src + 1 + end, dst + start);
            let (done, rest) = data.split_at_mut(dst);
            let previous = done.len().checked_sub(len).map_or(&[][..], |p| &done[p..]);
            if let Some(filter) = filter {
                png::unfilter_part(filter, bpp, previous, &mut rest[..k], start..end);
            }
            meter.work(end - start).ok()?;
        }
        src += len + 1;
        dst += k;
    }
    data.truncate(dst);
    Some(data)
}

fn unpredict_tiff8(
    mut data: Vec<u8>,
    len: usize,
    colors: usize,
    meter: &Meter<'_>,
) -> Option<Vec<u8>> {
    let part_len = part_len(colors);
    for row in data.chunks_mut(len) {
        for start in (0..row.len()).step_by(part_len) {
            let end = (start + part_len).min(row.len());
            png::unfilter_part(RowFilter::Sub, colors, &[], row, start..end);
            meter.work(end - start).ok()?;
        }
    }
    Some(data)
}

fn unpredict_tiff(
    mut data: Vec<u8>,
    len: usize,
    colors: usize,
    bpc: u8,
    components: usize,
    meter: &Meter<'_>,
) -> Option<Vec<u8>> {
    // In a last row cut short inside a 16-bit component, `MuPDF` takes the missing low
    // byte from where the row before held it, as stored (observed in `PyMuPDF`
    // 1.28.2; in a first row it reads a byte not established, taken as zero here).
    let (full, cut) = (data.len() / len, data.len() % len);
    let missing = match full.checked_sub(1) {
        Some(before) if bpc == 16 && cut % 2 == 1 => data[before * len + cut],
        _ => 0,
    };
    for row in data.chunks_mut(len) {
        if bpc == 16 {
            unpredict_tiff16(row, colors, missing, meter)?;
        } else {
            unpredict_tiff_bits(row, colors, bpc, components, meter)?;
        }
    }
    Some(data)
}

/// A row of 16-bit components, each the sum of itself and the one `colors` before;
/// a last component cut short takes `missing` as its low byte, and keeps its high one.
fn unpredict_tiff16(row: &mut [u8], colors: usize, missing: u8, meter: &Meter<'_>) -> Option<()> {
    let n = row.len().div_ceil(2);
    let part = part_len(2) / 2;
    for start in (colors..n).step_by(part) {
        let end = (start + part).min(n);
        for s in start..end {
            let at = |row: &[u8], s: usize| {
                u16::from_be_bytes([row[2 * s], row.get(2 * s + 1).copied().unwrap_or(missing)])
            };
            let [high, low] = at(row, s).wrapping_add(at(row, s - colors)).to_be_bytes();
            row[2 * s] = high;
            if let Some(byte) = row.get_mut(2 * s + 1) {
                *byte = low;
            }
        }
        meter.work(2 * (end - start)).ok()?;
    }
    Some(())
}

/// A row of components of `bpc` (1, 2 or 4) bits, most significant first, each the
/// sum of itself and the one `colors` before modulo 2^bpc; a row holds `components`,
/// a last row cut short as many as its bytes do. Bits after the last component of a
/// row are cleared, as `MuPDF` clears them.
fn unpredict_tiff_bits(
    row: &mut [u8],
    colors: usize,
    bpc: u8,
    components: usize,
    meter: &Meter<'_>,
) -> Option<()> {
    let n = tiff_bits_held(row, bpc, components);
    let part = part_len(1) * 8 / usize::from(bpc);
    for start in (colors..n).step_by(part) {
        let end = (start + part).min(n);
        unpredict_tiff_bits_part(row, colors, bpc, start..end);
        meter.work((end - start) * usize::from(bpc) / 8 + 1).ok()?;
    }
    clear_tiff_bits_tail(row, bpc, n);
    Some(())
}

/// The components of `bpc` bits a row of TIFF prediction holds: `components`, a
/// last row cut short as many as its bytes do.
fn tiff_bits_held(row: &[u8], bpc: u8, components: usize) -> usize {
    components.min(row.len() * 8 / usize::from(bpc))
}

/// Reverses TIFF prediction of `bpc`-bit components `range` of `row` (from `colors`
/// on), the components before them reversed.
fn unpredict_tiff_bits_part(row: &mut [u8], colors: usize, bpc: u8, range: Range<usize>) {
    let bpc = usize::from(bpc);
    let mask = (1_u8 << bpc) - 1;
    let shift = |s: usize| 8 - bpc - s * bpc % 8;
    for s in range {
        let left = (row[(s - colors) * bpc / 8] >> shift(s - colors)) & mask;
        let byte = &mut row[s * bpc / 8];
        let v = ((*byte >> shift(s)) & mask).wrapping_add(left) & mask;
        *byte = (*byte & !(mask << shift(s))) | (v << shift(s));
    }
}

/// Clears the bits after the `n` components of `bpc` bits a row holds.
fn clear_tiff_bits_tail(row: &mut [u8], bpc: u8, n: usize) {
    let used = n * usize::from(bpc) % 8;
    if used != 0 {
        row[n * usize::from(bpc) / 8] &= !(0xFF >> used);
    }
}

/// The most bytes of TIFF prediction an [`Unpredictor`] reads and reverses at a time.
const TIFF_PIECE: usize = 8192;

/// Reverses a predictor as the data are read, as `apply_predictor` reverses it for
/// the whole data at once. PNG prediction is reversed row by row, each row in parts
/// as its bytes arrive and held whole for the next one to read from. TIFF prediction
/// of components of 1, 2, 4 or 8 bits is reversed a piece of at most [`TIFF_PIECE`]
/// bytes at a time, holding only the last component of each colour, so neither the
/// work between two counts on the meter nor the memory grows with a row.
#[derive(Clone)]
pub(crate) enum Unpredictor {
    Png(PngRows),
    Tiff(TiffPieces),
}

#[derive(Clone)]
pub(crate) struct PngRows {
    /// The bytes of a pixel.
    bpp: usize,
    /// The bytes of a decoded row, after its filter byte.
    row_len: usize,
    /// The encoded row being read, its filter byte first, of which the `done` bytes
    /// after the filter byte are reversed.
    input: Vec<u8>,
    done: usize,
    /// The last row decoded, after its filter byte; empty before the first, as
    /// `apply_predictor` passes it.
    prev: Vec<u8>,
}

#[derive(Clone)]
pub(crate) struct TiffPieces {
    /// The bits of a component: 1, 2, 4 or 8.
    bpc: u8,
    /// The components of a pixel: 1 to 32.
    colors: u8,
    /// The components of a row, and its bytes.
    components: u32,
    row_len: u32,
    /// The bytes of the row being read that were reversed.
    at: u32,
    /// The last piece reversed.
    piece: Vec<u8>,
    /// The last component of each colour reversed in the row being read; zero before
    /// its first, which adds nothing to the first component of each colour.
    last: [u8; 32],
}

impl Unpredictor {
    /// `None` for predictor 1 (no prediction) and for the TIFF prediction of
    /// 16-bit components, which only `apply_predictor` reverses.
    pub(crate) fn new(params: &PredictorParams) -> Option<Self> {
        let rows = Rows::of(params)?;
        let tiff = |colors: usize, bpc: u8| {
            Some(Self::Tiff(TiffPieces {
                bpc,
                colors: colors.try_into().ok()?,
                components: colors.checked_mul(params.columns)?.try_into().ok()?,
                row_len: rows.len.try_into().ok()?,
                at: 0,
                piece: Vec::new(),
                last: [0; 32],
            }))
        };
        match rows.kind {
            Kind::Png { bpp } => Some(Self::Png(PngRows {
                bpp,
                row_len: rows.len,
                input: Vec::new(),
                done: 0,
                prev: Vec::new(),
            })),
            Kind::Tiff8 { colors } => tiff(colors, 8),
            Kind::Tiff { bpc: 16, .. } => None,
            Kind::Tiff { colors, bpc } => tiff(colors, bpc),
        }
    }

    /// Reads the next bytes through `read`, which fills a buffer and returns how many
    /// bytes it put there (0 at the end of the data), and reverses them: true with
    /// them in [`Self::ready`], false at the end of the data, with nothing there. A
    /// last row cut short is kept, as `apply_predictor` keeps it. The work of
    /// reversing counts on `meter`; after an error, reading starts again from the
    /// first row.
    pub(crate) fn fill<E: From<Cancelled>>(
        &mut self,
        read: impl FnMut(&mut [u8]) -> Result<usize, E>,
        meter: &Meter<'_>,
    ) -> Result<bool, E> {
        let filled = self.resume(read, meter);
        if filled.is_err() {
            self.reset();
        }
        filled
    }

    /// [`Self::fill`], but an error leaves it where it was, with the bytes read before
    /// the error kept: a `read` that fails while its bytes are still to come lets the
    /// next call go on where this one stopped.
    pub(crate) fn resume<E: From<Cancelled>>(
        &mut self,
        mut read: impl FnMut(&mut [u8]) -> Result<usize, E>,
        meter: &Meter<'_>,
    ) -> Result<bool, E> {
        match self {
            Self::Png(rows) => rows.fill(&mut read, meter),
            Self::Tiff(pieces) => pieces.fill(&mut read, meter),
        }
    }

    /// The bytes the last [`Self::fill`] decoded: a row for PNG prediction, a piece
    /// of rows for TIFF prediction; empty before the first.
    pub(crate) fn ready(&self) -> &[u8] {
        match self {
            Self::Png(rows) => rows.prev.get(1..).unwrap_or_default(),
            Self::Tiff(pieces) => &pieces.piece,
        }
    }

    /// A copy, its rows copied on `meter` ([`Meter::copied`]): a PNG row is as wide as
    /// the data declares.
    #[cfg(any(feature = "unsafe", feature = "images"))]
    pub(crate) fn copy(&self, meter: &Meter<'_>) -> Result<Self, Cancelled> {
        Ok(match self {
            Self::Png(rows) => Self::Png(PngRows {
                bpp: rows.bpp,
                row_len: rows.row_len,
                input: meter.copied(&rows.input)?,
                done: rows.done,
                prev: meter.copied(&rows.prev)?,
            }),
            Self::Tiff(pieces) => Self::Tiff(TiffPieces {
                bpc: pieces.bpc,
                colors: pieces.colors,
                components: pieces.components,
                row_len: pieces.row_len,
                at: pieces.at,
                piece: meter.copied(&pieces.piece)?,
                last: pieces.last,
            }),
        })
    }

    /// The heap bytes it holds.
    #[cfg(feature = "unsafe")]
    pub(crate) fn heap_bytes(&self) -> usize {
        match self {
            Self::Png(rows) => rows.input.capacity() + rows.prev.capacity(),
            Self::Tiff(pieces) => pieces.piece.capacity(),
        }
    }

    /// The most heap bytes a copy holds, at any position (a copy's capacities are its
    /// lengths): for PNG prediction the encoded row being read and the last one
    /// decoded, for TIFF prediction a piece.
    #[cfg(feature = "unsafe")]
    pub(crate) fn heap_bound(&self) -> Option<usize> {
        match self {
            Self::Png(rows) => rows.row_len.checked_add(1)?.checked_mul(2),
            Self::Tiff(_) => Some(TIFF_PIECE),
        }
    }

    /// Back to the first row.
    pub(crate) fn reset(&mut self) {
        match self {
            Self::Png(rows) => {
                rows.input.clear();
                rows.prev.clear();
                rows.done = 0;
            }
            Self::Tiff(pieces) => {
                pieces.piece.clear();
                pieces.at = 0;
                pieces.last = [0; 32];
            }
        }
    }
}

impl PngRows {
    /// Reads the rest of the next row, reversing it as its bytes arrive; false if the
    /// data end before it, where a last row cut short is kept if it holds a byte
    /// after its filter byte. The row grows with the bytes read, so a declared width
    /// alone allocates nothing.
    fn fill<E: From<Cancelled>>(
        &mut self,
        read: &mut impl FnMut(&mut [u8]) -> Result<usize, E>,
        meter: &Meter<'_>,
    ) -> Result<bool, E> {
        let len = self.row_len + 1;
        while self.input.len() < len {
            let have = self.input.len();
            let next = len.min(have + TIFF_PIECE);
            // A wide row's room grows on the meter, not between two polls.
            meter.reserve(&mut self.input, next - have);
            self.input.resize(next, 0);
            let read = read(&mut self.input[have..]);
            self.input.truncate(have + *read.as_ref().unwrap_or(&0));
            if read? == 0 {
                if self.input.len() <= 1 {
                    self.input.clear();
                    self.prev.clear();
                    self.done = 0;
                    return Ok(false);
                }
                break;
            }
            self.advance(POLL_BYTES, meter)?;
        }
        self.advance(0, meter)?;
        core::mem::swap(&mut self.prev, &mut self.input);
        self.input.clear();
        self.done = 0;
        Ok(true)
    }

    /// Reverses the bytes of the row read so far that were not reversed before, once
    /// there are at least `min` of them: those that make whole pixels, or all of them
    /// for `min` 0, where the row is read.
    fn advance(&mut self, min: usize, meter: &Meter<'_>) -> Result<(), Cancelled> {
        let (bpp, done) = (self.bpp, self.done);
        let Some((&mut filter, row)) = self.input.split_first_mut() else {
            return Ok(());
        };
        let end = match min {
            0 => row.len(),
            _ => row.len() / bpp * bpp,
        };
        if end <= done || end - done < min {
            return Ok(());
        }
        if let Some(filter) = RowFilter::from_u8(filter) {
            let previous = self.prev.get(1..).unwrap_or_default();
            png::unfilter_part(filter, bpp, previous, row, done..end);
        }
        self.done = end;
        meter.work(end - done)
    }
}

impl TiffPieces {
    /// Reads the next piece and reverses it; false at the end of the data.
    fn fill<E: From<Cancelled>>(
        &mut self,
        read: &mut impl FnMut(&mut [u8]) -> Result<usize, E>,
        meter: &Meter<'_>,
    ) -> Result<bool, E> {
        self.piece.resize(TIFF_PIECE, 0);
        let read = read(&mut self.piece);
        self.piece.truncate(*read.as_ref().unwrap_or(&0));
        if read? == 0 {
            return Ok(false);
        }
        self.reverse();
        meter.work(self.piece.len())?;
        Ok(true)
    }

    /// Reverses the piece, which continues the row being read, and may hold the rows
    /// after it: each component, `bpc` bits from the most significant, adds the last
    /// one of its colour modulo 2^bpc. Bits after the last component of a row are
    /// cleared, as `apply_predictor` clears them.
    fn reverse(&mut self) {
        let bpc = usize::from(self.bpc);
        let per_byte = 8 / bpc;
        let mask = u8::MAX >> (8 - bpc);
        let colors = usize::from(self.colors);
        let (components, row_len) = (self.components as usize, self.row_len as usize);
        let mut at = self.at as usize;
        let mut colour = at * per_byte % colors;
        for byte in &mut self.piece {
            if at == row_len {
                at = 0;
                colour = 0;
                self.last = [0; 32];
            }
            let held = (components - at * per_byte).min(per_byte);
            let mut out = 0;
            for k in 0..held {
                let shift = 8 - bpc * (k + 1);
                let v = ((*byte >> shift) & mask).wrapping_add(self.last[colour]) & mask;
                self.last[colour] = v;
                out |= v << shift;
                colour += 1;
                if colour == colors {
                    colour = 0;
                }
            }
            *byte = out;
            at += 1;
        }
        self.at = at as u32;
    }
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use crate::filter::lzw_flate::{PredictorParams, apply_predictor, flate, lzw};
    use crate::filter::png::{self, RowFilter};
    use crate::work::{Meter, POLL_BYTES};

    #[test]
    fn decode_lzw() {
        let input = [0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01];
        let decoded =
            lzw::decode(&input, &PredictorParams::default(), &Meter::default(), false).unwrap();

        assert_eq!(decoded, vec![45, 45, 45, 45, 45, 65, 45, 45, 45, 66]);
    }

    #[test]
    fn decode_flate_zlib() {
        let input = [
            0x78, 0x9c, 0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x7, 0x0, 0x5, 0x8c, 0x1, 0xf5,
        ];

        let decoded = flate::decode(&input, &PredictorParams::default(), &Meter::default(), false, 0).unwrap();
        assert_eq!(decoded, b"Hello");
    }

    #[test]
    fn decode_flate() {
        let input = [0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x7, 0x0];

        let decoded = flate::decode(&input, &PredictorParams::default(), &Meter::default(), false, 0).unwrap();
        assert_eq!(decoded, b"Hello");
    }

    /// As flate2's reader decodes them: a stream cut short yields the bytes it holds,
    /// and bytes after a stream's end are ignored.
    #[cfg(feature = "unsafe")]
    #[test]
    fn decode_flate_cut_short_or_followed_by_more() {
        use std::io::{Read, Write};
        let data: Vec<u8> = (0..300_000_u64).map(|i| (i * i / 7) as u8).collect();
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&data).unwrap();
        let encoded = encoder.finish().unwrap();
        let cut = &encoded[..encoded.len() / 2];
        let mut expected = Vec::new();
        flate2::read::ZlibDecoder::new(cut)
            .read_to_end(&mut expected)
            .unwrap();
        assert!(expected.len() > POLL_BYTES && data.starts_with(&expected));
        let decoded = flate::decode(cut, &PredictorParams::default(), &Meter::default(), false, 0);
        assert_eq!(decoded, Some(expected));
        let mut followed = encoded.clone();
        followed.extend(b"more");
        let decoded = flate::decode(&followed, &PredictorParams::default(), &Meter::default(), false, 0);
        assert_eq!(decoded, Some(data));
    }

    /// TIFF prediction of a row of any width is reversed a bounded piece at a time,
    /// each piece counted on the meter before the next is read, at every depth.
    #[test]
    fn tiff_prediction_is_reversed_in_counted_pieces() {
        use super::{TIFF_PIECE, Unpredictor};
        use core::cell::Cell;
        for bpc in [1, 2, 4, 8] {
            let columns = (4 * POLL_BYTES * 8) / bpc + 3;
            let p = PredictorParams { predictor: 2, colors: 1, bits_per_component: bpc as u8, columns, early_change: true };
            let data: Vec<u8> = (0..columns * bpc / 8 + 1).map(|i| (i * 37 % 251) as u8).collect();
            let polls = Cell::new(0);
            let hook = || { polls.set(polls.get() + 1); false };
            let meter = Meter::new(Some(&hook));
            let mut unpredictor = Unpredictor::new(&p).unwrap();
            let (mut at, mut out) = (0, Vec::new());
            // Reading from a slice counts nothing, so every count is the reversal's.
            let mut read = |buf: &mut [u8]| -> Result<usize, crate::work::Cancelled> {
                let n = buf.len().min(data.len() - at);
                buf[..n].copy_from_slice(&data[at..at + n]);
                at += n;
                Ok(n)
            };
            while unpredictor.fill(&mut read, &meter).unwrap() {
                let piece = unpredictor.ready();
                assert!(piece.len() <= TIFF_PIECE, "{bpc}: a piece of {}", piece.len());
                out.extend_from_slice(piece);
            }
            assert_eq!(polls.get(), data.len() / POLL_BYTES, "{bpc}: the reversal is counted");
            assert_eq!(Some(out), apply_predictor(data.clone(), &p, &Meter::default()), "{bpc}");
        }
    }

    fn predictor_expected() -> Vec<u8> {
        vec![
            // Row 1
            127, 127, 127, 125, 129, 127, 123, 130, 128, 
            // Row 2
            128, 129, 126, 126, 132, 124, 121, 127, 126, 
            // Row 3
            131, 130, 122, 133, 129, 128, 127, 100, 126,
        ]
    }

    fn predictor_test(predictor: u8, input: &[u8]) {
        let params = PredictorParams {
            predictor,
            colors: 3,
            bits_per_component: 8,
            columns: 3,
            early_change: false,
        };

        let expected = predictor_expected();
        let out = apply_predictor(input.to_vec(), &params, &Meter::default()).unwrap();

        assert_eq!(expected, out);
    }

    #[test]
    fn predictor_sub() {
        predictor_test(
            11,
            &[
                // Row 1
                1, 127, 127, 127, 254, 2, 0, 254, 1, 1, 
                // Row 2
                1, 128, 129, 126, 254, 3, 254, 251, 251, 2, 
                // Row 3
                1, 131, 130, 122, 2, 255, 6, 250, 227, 254,
            ],
        );
    }

    #[test]
    fn predictor_up() {
        predictor_test(
            12,
            &[
                // Row 1
                2, 127, 127, 127, 125, 129, 127, 123, 130, 128, 
                // Row 2
                2, 1, 2, 255, 1, 3, 253, 254, 253, 254, 
                // Row 3
                2, 3, 1, 252, 7, 253, 4, 6, 229, 0,
            ],
        );
    }

    #[test]
    fn predictor_avg() {
        predictor_test(
            13,
            &[
                // Row 1
                3, 127, 127, 127, 62, 66, 64, 61, 66, 65, 
                // Row 2
                3, 65, 66, 63, 0, 3, 254, 253, 252, 0, 
                // Row 3
                3, 67, 66, 59, 5, 254, 5, 0, 228, 255,
            ],
        );
    }

    #[test]
    fn predictor_paeth() {
        predictor_test(
            14,
            &[
                // Row 1
                4, 127, 127, 127, 254, 2, 0, 254, 1, 1, 
                // Row 2
                4, 1, 2, 255, 1, 3, 254, 254, 251, 2, 
                // Row 3
                4, 3, 1, 252, 5, 253, 6, 1, 229, 254,
            ],
        );
    }

    /// Rows wider than a piece, each with another filter, reverse as whole rows do.
    #[test]
    fn wide_rows_reverse_as_whole_rows() {
        let row_len = (2 * POLL_BYTES / 3 + 5) * 3;
        let mut seed = 7_u32;
        let mut input = Vec::new();
        for filter in 0..5 {
            input.push(filter);
            input.extend((0..row_len).map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (seed >> 16) as u8
            }));
        }
        let mut expected = Vec::new();
        for row in input.chunks(row_len + 1) {
            let mut current = row[1..].to_vec();
            let previous = &expected[expected.len().saturating_sub(row_len)..];
            png::unfilter(RowFilter::from_u8(row[0]).unwrap(), 3, previous, &mut current);
            expected.extend(current);
        }
        let params = PredictorParams {
            predictor: 15,
            colors: 3,
            bits_per_component: 8,
            columns: row_len / 3,
            early_change: false,
        };
        assert_eq!(apply_predictor(input, &params, &Meter::default()).unwrap(), expected);
    }
}
