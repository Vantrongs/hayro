use crate::bit_reader::{BitChunk, BitChunks, BitReader, BitWriter, bit_mask};
use crate::filter::png::{self, BytesPerPixel, RowFilter};
use crate::object::Dict;
use crate::object::dict::keys::{BITS_PER_COMPONENT, COLORS, COLUMNS, EARLY_CHANGE, PREDICTOR};
use crate::work::{Meter, POLL_BYTES};
use alloc::vec;
use alloc::vec::Vec;

pub(crate) mod flate {
    use super::*;
    use crate::filter::lzw_flate::{PredictorParams, apply_predictor};
    use crate::object::Dict;

    /// The data inflated and its predictor reversed, the work counted on `meter`;
    /// `None` once its hook asks to stop, or, if `strict`, where the data is corrupt
    /// rather than cut short (see [`fallback::decode`]).
    #[cfg(feature = "unsafe")]
    pub(crate) fn decode(
        data: &[u8],
        params: &Dict<'_>,
        meter: &Meter<'_>,
        strict: bool,
    ) -> Option<Vec<u8>> {
        let decoded = inflate(data, true, meter)
            .or_else(|| (!meter.cancelled()).then(|| inflate(data, false, meter))?)
            .or_else(|| {
                if meter.cancelled() {
                    return None;
                }
                warn!("flate stream is broken, decoding with fallback");

                fallback::decode(data, meter, strict).ok()?
            })?;
        let params = PredictorParams::from_params(params);
        apply_predictor(decoded, &params, meter)
    }

    /// `data` inflated whole, read as zlib if `zlib`, else as raw deflate, the work
    /// counted on `meter`; `None` when the data is corrupt for that format or the hook
    /// asks to stop. Data that ends before its stream does yields what it holds, and
    /// bytes after the stream's end are ignored.
    #[cfg(feature = "unsafe")]
    fn inflate(data: &[u8], zlib: bool, meter: &Meter<'_>) -> Option<Vec<u8>> {
        let mut decoder = zlib_rs::Inflate::new(zlib, 15);
        let mut out = Vec::new();
        // The input bytes the decoder took, and those it was offered and counted.
        let (mut taken, mut offered) = (0, 0);
        loop {
            if out.len() == out.capacity() {
                out.reserve(more_room(out.len(), taken, data.len()));
            }
            // Input is offered half of POLL_BYTES at a time and counted before it is
            // taken, and output is made no further than the next poll, so at most
            // POLL_BYTES and one piece of input are worked on between two polls.
            // Calls that make less are slower: each copies what it made into zlib's
            // history window, and matches reaching back past its output read from
            // there.
            let end = data.len().min(taken + POLL_BYTES / 2);
            meter.work(end.saturating_sub(offered)).ok()?;
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
            meter.work(written).ok()?;
            match status {
                Ok(zlib_rs::Status::StreamEnd) => return Some(out),
                Ok(_) if written > 0 || took > 0 => {}
                Ok(_) if input.is_empty() => return Some(out),
                _ => return None,
            }
        }
    }

    /// How much to grow a full output buffer holding the `made` bytes inflated from the
    /// first `taken` of `len` encoded bytes. The first room is at most a poll's worth,
    /// since encoded bytes may hold empty blocks or a tail after the stream's end, which
    /// make nothing. Later rooms fit what the rest would make at the rate so far, with an
    /// eighth to spare, each at least doubling and at most growing eightfold what was
    /// made, so a large stream is reallocated a few times rather than at every doubling.
    /// At least the room zlib's fast loop needs.
    #[cfg(feature = "unsafe")]
    fn more_room(made: usize, taken: usize, len: usize) -> usize {
        let room = match made {
            0 => len.min(POLL_BYTES),
            _ => {
                let rest = (len - taken) as u128 * made as u128 / taken.max(1) as u128;
                let rest = usize::try_from(rest + rest / 8).unwrap_or(usize::MAX);
                rest.clamp(made, made.saturating_mul(7))
            }
        };
        room.max(1024)
    }

    /// The data inflated and its predictor reversed, the work counted on `meter`;
    /// `None` once its hook asks to stop, or, if `strict`, where the data is corrupt
    /// rather than cut short (see [`fallback::decode`]).
    #[cfg(not(feature = "unsafe"))]
    pub(crate) fn decode(
        data: &[u8],
        params: &Dict<'_>,
        meter: &Meter<'_>,
        strict: bool,
    ) -> Option<Vec<u8>> {
        let decoded = fallback::decode(data, meter, strict).ok()??;
        let params = PredictorParams::from_params(params);
        apply_predictor(decoded, &params, meter)
    }

    /// Ported from <https://github.com/mozilla/pdf.js/blob/master/src/core/flate_stream.js>
    /// TODO: Rewrite this in idiomatic Rust.
    pub(crate) mod fallback {
        use crate::work::{Cancelled, Meter};
        use alloc::vec;
        use alloc::vec::Vec;

        /// The data decoded, its work counted on `meter`. Decoding stops at the first
        /// thing that is no valid deflate data and keeps what came before; if `strict`,
        /// such data is `None` instead. Data that ends early, or with a wrong or missing
        /// checksum, is not corrupt: what it holds is all decoded.
        pub(crate) fn decode(
            data: &[u8],
            meter: &Meter<'_>,
            strict: bool,
        ) -> Result<Option<Vec<u8>>, Cancelled> {
            let mut stream = FlateStream::new(Slice::new(data));
            let stepped = stream.step(meter, usize::MAX);
            #[cfg(test)]
            WORK.set((stream.input.taken(), stream.made()));
            stepped?;
            if stream.corrupt && strict {
                warn!("flate stream is corrupt");
                return Ok(None);
            }
            Ok(Some(core::mem::take(&mut stream.output)))
        }

        /// Where the decoder takes its bytes from, one at a time.
        pub(crate) trait Input {
            /// The next byte; `None` at the end.
            fn next(&mut self, meter: &Meter<'_>) -> Option<u8>;
            /// The byte `ahead` bytes after the next one, which stays next.
            fn peek(&mut self, ahead: usize, meter: &Meter<'_>) -> Option<u8>;
            /// The bytes taken so far.
            fn taken(&self) -> usize;
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
        }

        #[cfg(test)]
        std::thread_local! {
            /// The bytes the last whole decoding on this thread took and made.
            pub(crate) static WORK: core::cell::Cell<(usize, usize)> =
                const { core::cell::Cell::new((0, 0)) };
        }

        /// The decoder, which can stop between two codes and go on later
        /// ([`Self::step`]), so that it is read a part at a time: its output is the
        /// history back references reach into, then the bytes not handed out yet.
        pub(crate) struct FlateStream<I: Input> {
            pub(crate) input: I,
            code_buf: u32,
            code_size: u8,
            /// The bytes made, from the `dropped`th on.
            output: Vec<u8>,
            dropped: usize,
            /// The bytes of `output` handed out ([`Self::hand`]).
            handed: usize,
            /// Whether the zlib header was looked for.
            started: bool,
            /// The tables of the compressed block being decoded, and the bytes taken
            /// when it started.
            block: Option<(HuffmanTable, HuffmanTable, usize)>,
            eof: bool,
            /// Whether decoding stopped at, or passed over, data that is no valid
            /// deflate, rather than at the end of the stream or of the data.
            pub(crate) corrupt: bool,
        }

        /// The farthest a back reference reaches.
        const WINDOW: usize = 32 * 1024;

        impl<I: Input> FlateStream<I> {
            pub(crate) fn new(input: I) -> Self {
                Self {
                    input,
                    code_buf: 0,
                    code_size: 0,
                    output: Vec::new(),
                    dropped: 0,
                    handed: 0,
                    started: false,
                    block: None,
                    eof: false,
                    corrupt: false,
                }
            }

            /// The bytes made in all.
            fn made(&self) -> usize {
                self.dropped + self.output.len()
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
                self.started && self.block.is_none() && self.eof
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
                    self.started = true;
                    self.skip_header(meter);
                }
                loop {
                    if let Some((lit, dist, from)) = self.block.take() {
                        if !self.read_codes(lit, dist, from, meter, pending)? {
                            return Ok(());
                        }
                        meter.work(self.input.taken() - from + 1)?;
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
                    for _ in 0..block_len {
                        match self.input.next(meter) {
                            Some(b) => self.output.push(b),
                            None => {
                                self.eof = true;
                                return;
                            }
                        }
                    }
                }
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
                    if self.output.len() - self.handed >= pending {
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
    use crate::filter::lzw_flate::flate::fallback::{Input, Slice};
    use crate::filter::lzw_flate::{PredictorParams, apply_predictor};
    use crate::object::Dict;
    use crate::work::Meter;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Decode a LZW-encoded stream, its work counted on `meter`; `None` once its hook
    /// asks to stop.
    pub(crate) fn decode(data: &[u8], params: &Dict<'_>, meter: &Meter<'_>) -> Option<Vec<u8>> {
        let params = PredictorParams::from_params(params);

        let decoded = decode_impl(data, params.early_change, meter)?;

        apply_predictor(decoded, &params, meter)
    }

    const CLEAR_TABLE: usize = 256;
    const EOD: usize = 257;
    const MAX_ENTRIES: usize = 4096;
    const INITIAL_SIZE: u16 = 258;

    fn decode_impl(data: &[u8], early_change: bool, meter: &Meter<'_>) -> Option<Vec<u8>> {
        let mut decoder = Decoder::new(early_change);
        let mut input = Slice::new(data);
        let mut decoded = vec![];
        while decoder.step(&mut input, &mut decoded, meter)? {}
        Some(decoded)
    }

    /// The decoder, a code at a time ([`Self::step`]), its codes read most
    /// significant bit first.
    pub(crate) struct Decoder {
        table: Table,
        bit_size: u8,
        prev: Option<usize>,
        /// Bits taken from the input and not read yet, the last `bits` of `buffer`.
        buffer: u64,
        bits: u8,
    }

    impl Decoder {
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
        /// `None` where it is invalid or the meter's hook asks to stop.
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
                        let entry = table.get(new)?;
                        let first_byte = entry[0];
                        meter.work(entry.len()).ok()?;
                        decoded.extend_from_slice(entry);

                        if let Some(prev_code) = self.prev {
                            table.register(prev_code, first_byte);
                        }
                    } else if new == table.size()
                        && let Some(prev_code) = self.prev
                    {
                        let prev_entry = table.get(prev_code)?;
                        let first_byte = prev_entry[0];

                        let new_entry = table.register(prev_code, first_byte)?;
                        meter.work(new_entry.len()).ok()?;
                        decoded.extend_from_slice(new_entry);
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

    struct Table {
        early_change: bool,
        entries: Vec<Option<Vec<u8>>>,
    }

    impl Table {
        fn new(early_change: bool) -> Self {
            let mut entries: Vec<_> = (0..=255).map(|b| Some(vec![b])).collect();

            // Clear table and EOD don't have any data.
            entries.push(None); // 256 = CLEAR_TABLE
            entries.push(None); // 257 = EOD

            Self {
                early_change,
                entries,
            }
        }

        fn push(&mut self, entry: Vec<u8>) -> Option<&[u8]> {
            if self.entries.len() >= MAX_ENTRIES {
                None
            } else {
                self.entries.push(Some(entry));
                self.entries.last()?.as_ref().map(|v| &**v)
            }
        }

        fn register(&mut self, prev: usize, new_byte: u8) -> Option<&[u8]> {
            let prev_entry = self.get(prev)?;

            let mut new_entry = Vec::with_capacity(prev_entry.len() + 1);
            new_entry.extend(prev_entry);
            new_entry.push(new_byte);
            self.push(new_entry)
        }

        fn get(&self, index: usize) -> Option<&[u8]> {
            self.entries.get(index)?.as_ref().map(|v| &**v)
        }

        fn clear(&mut self) {
            self.entries.truncate(INITIAL_SIZE as usize);
        }

        fn size(&self) -> usize {
            self.entries.len()
        }

        fn code_length(&self) -> u8 {
            const TEN: usize = 512;
            const ELEVEN: usize = 1024;
            const TWELVE: usize = 2048;

            let adjusted = self.entries.len() + (if self.early_change { 1 } else { 0 });

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

pub(crate) struct PredictorParams {
    pub(crate) predictor: u8,
    pub(crate) colors: u8,
    pub(crate) bits_per_component: u8,
    pub(crate) columns: usize,
    pub(crate) early_change: bool,
}

impl PredictorParams {
    fn bits_per_pixel(&self) -> usize {
        usize::from(self.bits_per_component) * usize::from(self.colors)
    }

    fn row_length_in_bytes(&self) -> Option<usize> {
        let bits = self.bits_per_pixel();
        (self.columns / 8)
            .checked_mul(bits)?
            .checked_add(((self.columns % 8) * bits).div_ceil(8))
    }
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
    pub(crate) fn from_params(dict: &Dict<'_>) -> Self {
        Self {
            predictor: dict.get(PREDICTOR).unwrap_or(1),
            colors: dict.get(COLORS).unwrap_or(1),
            bits_per_component: dict.get(BITS_PER_COMPONENT).unwrap_or(8),
            columns: dict.get(COLUMNS).unwrap_or(1),
            early_change: dict.get::<u8>(EARLY_CHANGE).map(|e| e != 0).unwrap_or(true),
        }
    }
}

/// The data with its predictor reversed, in pieces whose work counts on `meter`;
/// `None` once its hook asks to stop.
pub(crate) fn apply_predictor(
    data: Vec<u8>,
    params: &PredictorParams,
    meter: &Meter<'_>,
) -> Option<Vec<u8>> {
    match params.predictor {
        1 => Some(data),
        i => {
            let is_png_predictor = i >= 10;

            let row_len = params.row_length_in_bytes()?;

            let total_row_len = if is_png_predictor {
                // + 1 Because each row must start with the predictor that is used for PNG predictors.
                row_len.checked_add(1)?
            } else {
                row_len
            };

            let num_rows = data.len().checked_div(total_row_len)?;

            if !matches!(params.bits_per_component, 1 | 2 | 4 | 8 | 16) {
                warn!("invalid bits per component {}", params.bits_per_component);

                return None;
            }

            let (bit_size, chunk_len) = if is_png_predictor {
                (8, params.bits_per_pixel().div_ceil(8))
            } else {
                (params.bits_per_component, params.colors as usize)
            };

            if bit_size == 8
                && let Some(tbpp) = BytesPerPixel::from_row_len(row_len, chunk_len)
            {
                return apply_predictor_8bit_png(
                    data,
                    i,
                    is_png_predictor,
                    row_len,
                    total_row_len,
                    num_rows,
                    tbpp,
                    meter,
                );
            }

            let zero_row = vec![0; row_len];
            let mut prev_row = BitChunks::new(&zero_row, bit_size, chunk_len)?;
            let zero_col = BitChunk::new(0, chunk_len);
            let mut out = vec![0; num_rows * row_len];
            let mut writer = BitWriter::new(&mut out, bit_size)?;

            for in_row in data.chunks_exact(total_row_len) {
                if is_png_predictor {
                    let predictor = in_row[0];
                    let in_data = &in_row[1..];
                    let in_data_chunks = BitChunks::new(in_data, bit_size, chunk_len)?;

                    match predictor {
                        1 => apply::<Sub>(
                            prev_row,
                            zero_col.clone(),
                            zero_col.clone(),
                            in_data_chunks,
                            &mut writer,
                            chunk_len,
                            bit_size,
                            meter,
                        )?,
                        2 => apply::<Up>(
                            prev_row,
                            zero_col.clone(),
                            zero_col.clone(),
                            in_data_chunks,
                            &mut writer,
                            chunk_len,
                            bit_size,
                            meter,
                        )?,
                        3 => apply::<Avg>(
                            prev_row,
                            zero_col.clone(),
                            zero_col.clone(),
                            in_data_chunks,
                            &mut writer,
                            chunk_len,
                            bit_size,
                            meter,
                        )?,
                        4 => apply::<Paeth>(
                            prev_row,
                            zero_col.clone(),
                            zero_col.clone(),
                            in_data_chunks,
                            &mut writer,
                            chunk_len,
                            bit_size,
                            meter,
                        )?,
                        _ => {
                            // Just copy the data.
                            let mut reader = BitReader::new(in_data);

                            while let Some(data) = reader.read(bit_size) {
                                meter.work(1).ok()?;
                                writer.write(data);
                            }
                        }
                    }
                } else if i == 2 {
                    apply::<Sub>(
                        prev_row,
                        zero_col.clone(),
                        zero_col.clone(),
                        BitChunks::new(in_row, bit_size, chunk_len)?,
                        &mut writer,
                        chunk_len,
                        bit_size,
                        meter,
                    )?;
                } else {
                    warn!("unknown predictor {i}");

                    return None;
                }

                let (data, new_writer) = writer.split_off();
                writer = new_writer;
                prev_row = BitChunks::new(data, bit_size, chunk_len)?;
            }

            Some(out)
        }
    }
}

/// Reverses a predictor row by row, as `apply_predictor` does for the whole data at
/// once: for the predictors and row layouts `apply_predictor_8bit_png` handles
/// (whole bytes per pixel), the only ones it accepts. A row is reversed in parts, as
/// its bytes arrive.
#[derive(Clone)]
pub(crate) struct Unpredictor {
    is_png_predictor: bool,
    tbpp: BytesPerPixel,
    /// The previous row, decoded after its predictor byte for PNG predictors; empty
    /// before the first, as `apply_predictor_8bit_png` passes it.
    prev: Vec<u8>,
    row_len: usize,
    /// The bytes of the row being read that are reversed.
    done: usize,
}

impl Unpredictor {
    /// `None` for predictor 1 (no prediction) and for the predictors and layouts
    /// `apply_predictor` handles otherwise (or rejects).
    pub(crate) fn new(params: &PredictorParams) -> Option<Self> {
        let is_png_predictor = params.predictor >= 10;
        if !(is_png_predictor || params.predictor == 2)
            || !matches!(params.bits_per_component, 1 | 2 | 4 | 8 | 16)
        {
            return None;
        }
        let row_len = params.row_length_in_bytes()?;
        let chunk_len = if is_png_predictor {
            params.bits_per_pixel().div_ceil(8)
        } else if params.bits_per_component == 8 {
            params.colors as usize
        } else {
            return None;
        };
        if row_len == 0 {
            return None;
        }
        row_len.checked_add(usize::from(is_png_predictor))?;
        let tbpp = BytesPerPixel::from_row_len(row_len, chunk_len)?;
        Some(Self {
            is_png_predictor,
            tbpp,
            prev: Vec::new(),
            row_len,
            done: 0,
        })
    }

    /// The bytes of one encoded row (with its predictor byte for PNG predictors).
    pub(crate) fn input_len(&self) -> usize {
        self.row_len + usize::from(self.is_png_predictor)
    }

    /// Reverses the predictor over the bytes of `input`, the encoded row read so far,
    /// that make whole pixels and were not reversed before, once there are at least
    /// `min` of them.
    pub(crate) fn advance(&mut self, input: &mut [u8], min: usize) {
        let (filter, row) = if self.is_png_predictor {
            let Some((&mut filter, row)) = input.split_first_mut() else {
                return;
            };
            (RowFilter::from_u8(filter), row)
        } else {
            (Some(RowFilter::Sub), input)
        };
        let end = row.len() / self.tbpp as usize * self.tbpp as usize;
        if end <= self.done || end - self.done < min {
            return;
        }
        if let Some(filter) = filter {
            png::unfilter_part(filter, self.tbpp, self.row(), row, self.done..end);
        }
        self.done = end;
    }

    /// Decodes the rest of `input`, which holds the whole encoded row, and takes it
    /// as the previous row ([`Self::row`]), leaving `input` empty.
    pub(crate) fn finish_row(&mut self, input: &mut Vec<u8>) {
        self.advance(input, 0);
        core::mem::swap(&mut self.prev, input);
        input.clear();
        self.done = 0;
    }

    /// The last row decoded; empty before the first.
    pub(crate) fn row(&self) -> &[u8] {
        self.prev
            .get(usize::from(self.is_png_predictor)..)
            .unwrap_or_default()
    }

    /// The heap bytes the previous row holds.
    #[cfg(feature = "unsafe")]
    pub(crate) fn heap_bytes(&self) -> usize {
        self.prev.capacity()
    }

    /// Back to the first row.
    pub(crate) fn reset(&mut self) {
        self.prev.clear();
        self.done = 0;
    }
}

#[expect(clippy::too_many_arguments)]
fn apply_predictor_8bit_png(
    mut data: Vec<u8>,
    predictor: u8,
    is_png_predictor: bool,
    row_len: usize,
    total_row_len: usize,
    num_rows: usize,
    tbpp: BytesPerPixel,
    meter: &Meter<'_>,
) -> Option<Vec<u8>> {
    if row_len == 0 || total_row_len == 0 {
        return None;
    }

    let out_len = num_rows * row_len;
    let part_len = POLL_BYTES / tbpp as usize * tbpp as usize;

    for row_idx in 0..num_rows {
        let src_start = row_idx * total_row_len;
        let dst_start = row_idx * row_len;

        let filter = if is_png_predictor {
            RowFilter::from_u8(data[src_start])
        } else if predictor == 2 {
            Some(RowFilter::Sub)
        } else {
            warn!("unknown predictor {predictor}");

            return None;
        };

        for start in (0..row_len).step_by(part_len) {
            let end = (start + part_len).min(row_len);
            if is_png_predictor {
                // The row moves left, so the bytes a part overwrites were moved before.
                data.copy_within(
                    src_start + 1 + start..src_start + 1 + end,
                    dst_start + start,
                );
            }

            let (done, rest) = data.split_at_mut(dst_start);
            let prev_row = if let Some(prev_start) = done.len().checked_sub(row_len) {
                &done[prev_start..]
            } else {
                &[]
            };
            let row = &mut rest[..row_len];

            if let Some(filter) = filter {
                png::unfilter_part(filter, tbpp, prev_row, row, start..end);
            }
            meter.work(end - start).ok()?;
        }
    }

    data.truncate(out_len);

    Some(data)
}

#[expect(clippy::too_many_arguments)]
fn apply<'a, T: Predictor>(
    prev_row: BitChunks<'a>,
    mut prev_col: BitChunk,
    mut top_left: BitChunk,
    cur_row: BitChunks<'a>,
    writer: &mut BitWriter<'a>,
    chunk_len: usize,
    bit_size: u8,
    meter: &Meter<'_>,
) -> Option<()> {
    for (cur_row, prev_row) in cur_row.zip(prev_row) {
        meter.work(chunk_len).ok()?;
        let old_pos = writer.cur_pos();

        for (((cur_row, prev_row), prev_col), top_left) in cur_row
            .iter()
            .zip(prev_row.iter())
            .zip(prev_col.iter())
            .zip(top_left.iter())
        {
            // Note that the wrapping behavior when adding inside the predictors is dependent on the
            // bit size, so it wouldn't be triggered for bits per component < 16. So we mask out
            // the bytes manually, which is equivalent to a wrapping add.
            writer.write(
                T::predict(cur_row, prev_row, prev_col, top_left) as u32 & bit_mask(bit_size),
            );
        }

        prev_col = {
            let out_data = writer.get_data();
            let mut reader = BitReader::new_with(out_data, old_pos);
            BitChunk::from_reader(&mut reader, bit_size, chunk_len).unwrap()
        };

        top_left = prev_row;
    }

    Some(())
}

trait Predictor {
    fn predict(cur_row: u16, prev_row: u16, prev_col: u16, top_left: u16) -> u16;
}

struct Sub;
impl Predictor for Sub {
    fn predict(cur_row: u16, _: u16, prev_col: u16, _: u16) -> u16 {
        cur_row.wrapping_add(prev_col)
    }
}

struct Up;
impl Predictor for Up {
    fn predict(cur_row: u16, prev_row: u16, _: u16, _: u16) -> u16 {
        cur_row.wrapping_add(prev_row)
    }
}

struct Avg;
impl Predictor for Avg {
    fn predict(cur_row: u16, prev_row: u16, prev_col: u16, _: u16) -> u16 {
        cur_row.wrapping_add(((prev_col as u32 + prev_row as u32) / 2) as u16)
    }
}

struct Paeth;
impl Predictor for Paeth {
    fn predict(cur_row: u16, prev_row: u16, prev_col: u16, top_left: u16) -> u16 {
        fn paeth(a: u16, b: u16, c: u16) -> u16 {
            let a = a as i32;
            let b = b as i32;
            let c = c as i32;

            let p = a + b - c;
            let pa = (p - a).abs();
            let pb = (p - b).abs();
            let pc = (p - c).abs();

            if pa <= pb && pa <= pc {
                a as u16
            } else if pb <= pc {
                b as u16
            } else {
                c as u16
            }
        }

        cur_row.wrapping_add(paeth(prev_col, prev_row, top_left))
    }
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use crate::filter::lzw_flate::{PredictorParams, apply_predictor, flate, lzw};
    use crate::filter::png::{self, BytesPerPixel, RowFilter};
    use crate::object::Dict;
    use crate::work::{Meter, POLL_BYTES};

    #[test]
    fn decode_lzw() {
        let input = [0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01];
        let decoded = lzw::decode(&input, &Dict::default(), &Meter::default()).unwrap();

        assert_eq!(decoded, vec![45, 45, 45, 45, 45, 65, 45, 45, 45, 66]);
    }

    #[test]
    fn decode_flate_zlib() {
        let input = [
            0x78, 0x9c, 0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x7, 0x0, 0x5, 0x8c, 0x1, 0xf5,
        ];

        let decoded = flate::decode(&input, &Dict::default(), &Meter::default(), false).unwrap();
        assert_eq!(decoded, b"Hello");
    }

    #[test]
    fn decode_flate() {
        let input = [0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x7, 0x0];

        let decoded = flate::decode(&input, &Dict::default(), &Meter::default(), false).unwrap();
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
        let decoded = flate::decode(cut, &Dict::default(), &Meter::default(), false);
        assert_eq!(decoded, Some(expected));
        let mut followed = encoded.clone();
        followed.extend(b"more");
        let decoded = flate::decode(&followed, &Dict::default(), &Meter::default(), false);
        assert_eq!(decoded, Some(data));
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
            png::unfilter(RowFilter::from_u8(row[0]).unwrap(), BytesPerPixel::Three, previous, &mut current);
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
