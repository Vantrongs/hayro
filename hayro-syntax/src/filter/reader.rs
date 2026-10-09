//! Stream data decoded incrementally, in bounded memory where the filters allow it.

use crate::filter::lzw_flate::{PredictorParams, Unpredictor, flate};
use alloc::borrow::Cow;
use alloc::vec::Vec;

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
}

/// The decoded data of a stream, read from its start in pieces (see
/// [`Stream::decoded_reader`](crate::object::Stream::decoded_reader)). It yields the
/// bytes [`Stream::decoded`](crate::object::Stream::decoded) returns, but holds only
/// the decoder's state and one predictor row, not the whole result, except where a
/// corrupt Flate stream needs the permissive decoder, which decodes it whole.
pub struct DecodedReader<'a> {
    source: Source<'a>,
    predictor: Option<Predicted>,
}

/// A predictor reversed row by row on top of the source.
struct Predicted {
    unpredictor: Unpredictor,
    /// The encoded row being filled.
    input: Vec<u8>,
    filled: usize,
    /// The decoded row, and how much of it was handed out.
    row: Vec<u8>,
    taken: usize,
}

enum Source<'a> {
    Raw {
        data: Cow<'a, [u8]>,
        pos: usize,
    },
    #[cfg(feature = "unsafe")]
    Zlib(flate2::bufread::ZlibDecoder<Input<'a>>),
    #[cfg(feature = "unsafe")]
    Deflate(flate2::bufread::DeflateDecoder<Input<'a>>),
    /// A Flate stream decoded whole by the permissive decoder.
    Whole {
        data: Vec<u8>,
        pos: usize,
    },
    /// Decoding failed.
    Failed,
}

/// Encoded bytes as a `BufRead`, owned so the decoder can keep them.
#[cfg(feature = "unsafe")]
struct Input<'a> {
    data: Cow<'a, [u8]>,
    pos: usize,
}

#[cfg(feature = "unsafe")]
impl std::io::Read for Input<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = copy_from(&self.data, &mut self.pos, buf);
        Ok(n)
    }
}

#[cfg(feature = "unsafe")]
impl std::io::BufRead for Input<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        Ok(&self.data[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.data.len());
    }
}

fn copy_from(data: &[u8], pos: &mut usize, buf: &mut [u8]) -> usize {
    let n = buf.len().min(data.len() - *pos);
    buf[..n].copy_from_slice(&data[*pos..*pos + n]);
    *pos += n;
    n
}

impl<'a> DecodedReader<'a> {
    /// The data as stored, without filters.
    pub(crate) fn raw(data: Cow<'a, [u8]>) -> Self {
        Self {
            source: Source::Raw { data, pos: 0 },
            predictor: None,
        }
    }

    /// Flate data with its predictor `params`; `None` when the predictor is one only
    /// the whole-data decoder handles.
    pub(crate) fn flate(data: Cow<'a, [u8]>, params: &PredictorParams) -> Option<Self> {
        let predictor = match params.predictor {
            1 => None,
            _ => {
                let unpredictor = Unpredictor::new(params)?;
                Some(Predicted {
                    input: alloc::vec![0; unpredictor.input_len()],
                    unpredictor,
                    filled: 0,
                    row: Vec::new(),
                    taken: 0,
                })
            }
        };
        #[cfg(feature = "unsafe")]
        let source = Source::Zlib(flate2::bufread::ZlibDecoder::new(Input { data, pos: 0 }));
        #[cfg(not(feature = "unsafe"))]
        let source = whole(&data);
        Some(Self { source, predictor })
    }

    /// Fills `buf` with the next decoded bytes and returns how many; 0 only at the end.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, ReadError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let Some(p) = &mut self.predictor else {
            return self.source.read(buf);
        };
        loop {
            if p.taken < p.row.len() {
                let n = copy_from(&p.row, &mut p.taken, buf);
                return Ok(n);
            }
            while p.filled < p.input.len() {
                match self.source.read(&mut p.input[p.filled..]) {
                    // A last row cut short is dropped, as `apply_predictor` drops it.
                    Ok(0) => return Ok(0),
                    Ok(n) => p.filled += n,
                    Err(e) => {
                        p.filled = 0;
                        p.row.clear();
                        p.taken = 0;
                        p.unpredictor.reset();
                        return Err(e);
                    }
                }
            }
            p.unpredictor.row(&p.input, &mut p.row);
            p.filled = 0;
            p.taken = 0;
        }
    }
}

fn whole(data: &[u8]) -> Source<'static> {
    match flate::fallback::decode(data) {
        Some(data) => Source::Whole { data, pos: 0 },
        None => Source::Failed,
    }
}

impl Source<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ReadError> {
        match self {
            Source::Raw { data, pos } => Ok(copy_from(data, pos, buf)),
            Source::Whole { data, pos } => Ok(copy_from(data, pos, buf)),
            Source::Failed => Err(ReadError::Failed),
            #[cfg(feature = "unsafe")]
            Source::Zlib(decoder) => {
                use std::io::Read;
                match decoder.read(buf) {
                    Ok(n) => Ok(n),
                    // As `flate::decode`: raw deflate next, from the start.
                    Err(_) => {
                        let Source::Zlib(decoder) = core::mem::replace(self, Source::Failed) else {
                            unreachable!()
                        };
                        let mut input = decoder.into_inner();
                        input.pos = 0;
                        *self = Source::Deflate(flate2::bufread::DeflateDecoder::new(input));
                        Err(ReadError::Restarted)
                    }
                }
            }
            #[cfg(feature = "unsafe")]
            Source::Deflate(decoder) => {
                use std::io::Read;
                match decoder.read(buf) {
                    Ok(n) => Ok(n),
                    // Then the permissive decoder.
                    Err(_) => {
                        warn!("flate stream is broken, decoding with fallback");
                        let Source::Deflate(decoder) = core::mem::replace(self, Source::Failed)
                        else {
                            unreachable!()
                        };
                        *self = whole(&decoder.into_inner().data);
                        Err(ReadError::Restarted)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DecodedReader, ReadError};
    use crate::filter::lzw_flate::{PredictorParams, apply_predictor, flate};
    use alloc::borrow::Cow;
    use alloc::vec::Vec;

    /// Everything `reader` yields, read `chunk` bytes at a time; a restart discards
    /// what was read before it.
    fn read_all(mut reader: DecodedReader<'_>, chunk: usize) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = alloc::vec![0; chunk];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return Some(out),
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(ReadError::Restarted) => out.clear(),
                Err(ReadError::Failed) => return None,
            }
        }
    }

    /// Deterministic pseudo-random bytes.
    fn noise(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 24) as u8
            })
            .collect()
    }

    /// `data` as a zlib stream of stored (uncompressed) blocks of at most `block`
    /// bytes, with its checksum.
    fn stored_zlib(data: &[u8], block: usize) -> Vec<u8> {
        let mut out = alloc::vec![0x78, 0x01];
        let blocks: Vec<&[u8]> = if data.is_empty() {
            alloc::vec![&[][..]]
        } else {
            data.chunks(block).collect()
        };
        for (i, b) in blocks.iter().enumerate() {
            out.push(u8::from(i + 1 == blocks.len()));
            let len = b.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(b);
        }
        let (mut a, mut b) = (1_u32, 0_u32);
        for &d in data {
            a = (a + u32::from(d)) % 65521;
            b = (b + a) % 65521;
        }
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
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
        let whole = flate::fallback::decode(encoded).and_then(|d| apply_predictor(d, p));
        #[cfg(feature = "unsafe")]
        let whole = {
            use std::io::Read;
            let mut d = Vec::new();
            flate2::read::ZlibDecoder::new(encoded)
                .read_to_end(&mut d)
                .ok()
                .and_then(|_| apply_predictor(d, p))
                .or(whole)
        };
        for chunk in (1..10).chain([1 << 16]) {
            let reader = DecodedReader::flate(Cow::Borrowed(encoded), p).expect("streamable");
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
                data.clear();
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
        ] {
            assert!(DecodedReader::flate(Cow::Borrowed(&[]), &p).is_none());
        }
    }

    #[cfg(feature = "unsafe")]
    mod compressed {
        use super::*;
        use crate::object::Dict;
        use std::io::Write;

        fn zlib(data: &[u8]) -> Vec<u8> {
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
            let expected = flate::decode(&encoded, &Dict::default());
            let reader =
                DecodedReader::flate(Cow::Borrowed(&encoded), &params(1, 1, 8, 1)).unwrap();
            let mut restarts = 0;
            let mut out = Vec::new();
            let mut buf = alloc::vec![0; 1000];
            let mut reader = reader;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(k) => out.extend_from_slice(&buf[..k]),
                    Err(ReadError::Restarted) => {
                        restarts += 1;
                        out.clear();
                    }
                    Err(ReadError::Failed) => panic!("failed"),
                }
            }
            assert!(restarts >= 1);
            assert_eq!(Some(out), expected);
        }
    }
}
