use super::ascii_hex::decode_hex_digit;
use super::lzw_flate::lzw::Table;
use super::png::{RowFilter, unfilter_row};
use super::{Filter, predictor};
use crate::object::Dict;
use crate::object::dict::keys::*;
use crate::object::stream::{DecodeFailure, DecryptedReader};
use crate::trivia::is_white_space_character;
#[cfg(not(feature = "unsafe"))]
use alloc::boxed::Box;
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(feature = "unsafe"))]
use miniz_oxide::{
    DataFormat, MZError, MZFlush, MZStatus,
    inflate::stream::{InflateState, inflate},
};

const BLOCK: usize = 8192;

/// Decodes a filter chain from left to right, keeping pending output per stage.
pub struct DecodedReader<'a> {
    stored: DecryptedReader<'a>,
    layers: Vec<Layer>,
    source: Pending,
    cancel: Option<Arc<AtomicBool>>,
    failed: Option<usize>,
    strict: bool,
    inflated: u64,
}

#[derive(Default)]
struct Pending {
    bytes: Vec<u8>,
    pos: usize,
    end: bool,
}

struct Layer {
    codec: Codec,
    out: Pending,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum FlateMode {
    Wrapped,
    Raw,
    Detect,
}

enum Codec {
    #[cfg(feature = "images")]
    Fax(super::ccitt::Fax, Vec<u8>),
    Hex(Option<u8>),
    Base85 {
        value: u32,
        digits: u8,
    },
    Run {
        literal: usize,
        repeat: usize,
    },
    Lzw {
        table: Table,
        bits: u32,
        len: u8,
        previous: Option<usize>,
    },
    Flate {
        state: Inflater,
        mode: FlateMode,
        header: Vec<u8>,
        started: bool,
    },
    Predict(Predict),
}

struct Predict {
    method: u32,
    colors: usize,
    bpc: usize,
    columns: usize,
    width: usize,
    previous: Vec<u8>,
    row: Vec<u8>,
    filter: Option<u8>,
}

impl<'a> DecodedReader<'a> {
    pub(crate) fn new(
        stored: DecryptedReader<'a>,
        cancel: Option<Arc<AtomicBool>>,
        strict: bool,
    ) -> Self {
        Self {
            stored,
            layers: Vec::new(),
            source: Pending::default(),
            cancel,
            failed: None,
            strict,
            inflated: 0,
        }
    }

    pub(crate) fn add(
        &mut self,
        filter: Filter,
        params: &Dict<'_>,
        raw_modes: &[(usize, FlateMode)],
    ) -> Option<()> {
        let codec = match filter {
            #[cfg(feature = "images")]
            Filter::CcittFaxDecode if !self.strict => Codec::Fax(
                super::ccitt::Fax::new(params, 0, self.cancel.clone()),
                Vec::new(),
            ),
            Filter::AsciiHexDecode => Codec::Hex(None),
            Filter::Ascii85Decode => Codec::Base85 {
                value: 0,
                digits: 0,
            },
            Filter::RunLengthDecode => Codec::Run {
                literal: 0,
                repeat: 0,
            },
            Filter::LzwDecode => Codec::Lzw {
                table: Table::new(params.get::<u8>(EARLY_CHANGE) != Some(0)),
                bits: 0,
                len: 0,
                previous: None,
            },
            Filter::FlateDecode => {
                let mode = raw_modes
                    .iter()
                    .find(|(i, _)| *i == self.layers.len())
                    .map_or(
                        if self.strict {
                            FlateMode::Wrapped
                        } else {
                            FlateMode::Detect
                        },
                        |e| e.1,
                    );
                Codec::Flate {
                    state: Inflater::new(mode == FlateMode::Wrapped),
                    mode,
                    header: Vec::new(),
                    started: mode != FlateMode::Detect,
                }
            }
            _ => return None,
        };
        self.push(codec);
        if matches!(filter, Filter::FlateDecode | Filter::LzwDecode) {
            let method = params.get::<u32>(PREDICTOR).unwrap_or(1);
            if method != 1 {
                let colors = params.get::<usize>(COLORS).unwrap_or(1);
                let bpc = params.get::<usize>(BITS_PER_COMPONENT).unwrap_or(8);
                let columns = params.get::<usize>(COLUMNS).unwrap_or(1);
                let width = colors.checked_mul(bpc)?.checked_mul(columns)?.div_ceil(8);
                if width == 0
                    || !matches!(bpc, 1 | 2 | 4 | 8 | 16)
                    || !matches!(method, 2 | 10..=15)
                {
                    return None;
                }
                self.push(Codec::Predict(Predict {
                    method,
                    colors,
                    bpc,
                    columns,
                    width,
                    previous: Vec::new(),
                    row: Vec::new(),
                    filter: None,
                }));
            }
        }
        Some(())
    }

    fn push(&mut self, codec: Codec) {
        self.layers.push(Layer {
            codec,
            out: Pending::default(),
        });
    }

    /// Reads the next decoded bytes.
    pub fn read(&mut self, out: &mut [u8]) -> Result<usize, DecodeFailure> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.layers.is_empty() {
            self.poll()?;
            return Ok(self.stored.read(out));
        }
        self.pull(self.layers.len(), out)
    }

    /// Number of bytes produced by Flate stages.
    pub fn inflated_bytes(&self) -> u64 {
        self.inflated
    }

    /// Advances the cursor, seeking directly in unfiltered plain and AES data.
    pub fn skip(&mut self, n: usize) -> Result<usize, DecodeFailure> {
        if self.layers.is_empty() {
            self.poll()?;
            return Ok(self.stored.skip(n));
        }
        let mut buf = [0; BLOCK];
        let mut moved = 0;
        while moved < n {
            let count = self.read(&mut buf[..BLOCK.min(n - moved)])?;
            if count == 0 {
                break;
            }
            moved += count;
        }
        Ok(moved)
    }

    pub(crate) fn collect_strict(&mut self) -> Result<Vec<u8>, DecodeFailure> {
        let mut output = Vec::new();
        let mut buffer = [0; BLOCK];
        for level in (0..=self.layers.len()).rev() {
            loop {
                let n = match self.pull(level, &mut buffer) {
                    Ok(n) => n,
                    Err(DecodeFailure::StreamDecode) => break,
                    Err(e) => return Err(e),
                };
                if n == 0 {
                    break;
                }
                if level == self.layers.len() {
                    output.extend_from_slice(&buffer[..n]);
                }
            }
        }
        if self.failed.is_some() {
            Err(DecodeFailure::StreamDecode)
        } else {
            Ok(output)
        }
    }

    pub(crate) fn retry(&self) -> Option<(usize, FlateMode)> {
        let i = self.failed?;
        match self.layers[i].codec {
            Codec::Flate {
                mode: FlateMode::Wrapped,
                ..
            } => Some((i, FlateMode::Raw)),
            Codec::Flate {
                mode: FlateMode::Raw,
                ..
            } => Some((i, FlateMode::Detect)),
            _ => None,
        }
    }

    fn poll(&self) -> Result<(), DecodeFailure> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            Err(DecodeFailure::Cancelled)
        } else {
            Ok(())
        }
    }

    fn pull(&mut self, level: usize, out: &mut [u8]) -> Result<usize, DecodeFailure> {
        let mut i = level;
        loop {
            self.poll()?;
            if self.failed.is_some_and(|index| level > index) {
                return Err(DecodeFailure::StreamDecode);
            }
            let pending = if i == 0 {
                &mut self.source
            } else {
                &mut self.layers[i - 1].out
            };
            if pending.pos < pending.bytes.len() || pending.end {
                if i < level {
                    i += 1;
                    continue;
                }
                let n = out.len().min(pending.bytes.len() - pending.pos);
                out[..n].copy_from_slice(&pending.bytes[pending.pos..pending.pos + n]);
                pending.pos += n;
                return Ok(n);
            }
            if i == 0 {
                pending.bytes.resize(BLOCK, 0);
                let n = self.stored.read(&mut pending.bytes);
                pending.bytes.truncate(n);
                pending.pos = 0;
                pending.end = n == 0;
                continue;
            }
            let (before, after) = self.layers.split_at_mut(i - 1);
            let input = before.last_mut().map_or(&mut self.source, |l| &mut l.out);
            if input.pos == input.bytes.len() && !input.end {
                i -= 1;
                continue;
            }
            let layer = &mut after[0];
            layer.out.bytes.clear();
            layer.out.pos = 0;
            let Some((n, done)) = layer.codec.step(
                &input.bytes[input.pos..],
                input.end,
                &mut layer.out.bytes,
                self.strict,
            ) else {
                self.failed = Some(i - 1);
                return Err(DecodeFailure::StreamDecode);
            };
            input.pos += n;
            if matches!(layer.codec, Codec::Flate { .. }) {
                self.inflated = self.inflated.saturating_add(layer.out.bytes.len() as u64);
            }
            layer.out.end = done;
        }
    }
}

impl Codec {
    fn step(
        &mut self,
        data: &[u8],
        eof: bool,
        out: &mut Vec<u8>,
        strict: bool,
    ) -> Option<(usize, bool)> {
        #[cfg(feature = "images")]
        if let Self::Fax(fax, stored) = self {
            stored.extend_from_slice(data);
            if eof {
                *out = fax.decode(stored)?.data.into_owned();
                *stored = Vec::new();
            }
            return Some((data.len(), eof));
        }
        if let Self::Flate {
            state,
            started,
            header,
            ..
        } = self
        {
            if !*started {
                let n = (2 - header.len()).min(data.len());
                header.extend_from_slice(&data[..n]);
                if header.len() < 2 && !eof {
                    return Some((n, false));
                }
                let raw = !matches!(header.as_slice(),[a,b] if a&15==8 && a>>4<=7 && u16::from_be_bytes([*a,*b]).is_multiple_of(31));
                *started = true;
                if raw && !header.is_empty() {
                    let (used, done) = state.step(header, false, out)?;
                    if used != header.len() && !done {
                        return None;
                    }
                    return Some((n, done));
                }
                return Some((n, eof && data.is_empty() && header.is_empty()));
            }
            return state.step(data, eof, out);
        }
        if let Self::Predict(p) = self {
            return p.step(data, eof, out);
        }
        let mut n = 0;
        while n < data.len() && out.len() < BLOCK {
            let b = data[n];
            n += 1;
            match self {
                Self::Hex(high) => {
                    if is_white_space_character(b) {
                        continue;
                    }
                    if b == b'>' {
                        if let Some(v) = high.take() {
                            out.push(v << 4);
                        }
                        return Some((n, true));
                    }
                    let digit = decode_hex_digit(b)?;
                    if let Some(v) = high.take() {
                        out.push(v << 4 | digit);
                    } else {
                        *high = Some(digit);
                    }
                }
                Self::Base85 { value, digits } => {
                    if is_white_space_character(b) {
                        continue;
                    }
                    if b == b'~' {
                        break;
                    }
                    if b == b'z' && *digits == 0 {
                        out.extend_from_slice(&[0; 4]);
                        continue;
                    }
                    if !(b'!'..=b'u').contains(&b) {
                        return None;
                    }
                    *value = if strict {
                        value.checked_mul(85)?.checked_add(u32::from(b - b'!'))?
                    } else {
                        value.wrapping_mul(85).wrapping_add(u32::from(b - b'!'))
                    };
                    *digits += 1;
                    if *digits == 5 {
                        out.extend_from_slice(&value.to_be_bytes());
                        *digits = 0;
                        *value = 0;
                    }
                }
                Self::Run { literal, repeat } => {
                    if *literal > 0 {
                        out.push(b);
                        *literal -= 1;
                    } else if *repeat > 0 {
                        out.resize(out.len() + *repeat, b);
                        *repeat = 0;
                    } else if b == 128 {
                        return Some((n, true));
                    } else if b < 128 {
                        *literal = usize::from(b) + 1;
                    } else {
                        *repeat = 257 - usize::from(b);
                    }
                }
                Self::Lzw {
                    table,
                    bits,
                    len,
                    previous,
                } => {
                    *bits = (*bits << 8) | u32::from(b);
                    *len += 8;
                    let width = table.code_length();
                    if *len < width {
                        continue;
                    }
                    *len -= width;
                    let code = ((*bits >> *len) & ((1 << width) - 1)) as usize;
                    match code {
                        256 => {
                            table.clear();
                            *previous = None;
                        }
                        257 => return Some((n, true)),
                        _ => {
                            let first = if code == table.size() {
                                *table.get((*previous)?)?.first()?
                            } else {
                                *table.get(code)?.first()?
                            };
                            if let Some(prev) = *previous {
                                table.register(prev, first);
                            }
                            out.extend_from_slice(table.get(code)?);
                            *previous = Some(code);
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        let ended = eof && n == data.len()
            || matches!(self, Self::Base85 { .. }) && n != 0 && data[n - 1] == b'~';
        if ended {
            match self {
                Self::Hex(high) => {
                    if let Some(v) = high.take() {
                        out.push(v << 4);
                    }
                }
                Self::Base85 { digits: 1, .. } | Self::Run { literal: 0, .. } if strict => {
                    return None;
                }
                Self::Base85 { value, digits } if *digits > 1 => {
                    for _ in *digits..5 {
                        *value = if strict {
                            value.checked_mul(85)?.checked_add(84)?
                        } else {
                            value.wrapping_mul(85).wrapping_add(84)
                        };
                    }
                    out.extend_from_slice(&value.to_be_bytes()[..usize::from(*digits) - 1]);
                }
                _ => {}
            }
        }
        Some((n, ended))
    }
}

struct Inflater {
    #[cfg(feature = "unsafe")]
    inner: zlib_rs::Inflate,
    #[cfg(not(feature = "unsafe"))]
    inner: Box<InflateState>,
}
impl Inflater {
    fn new(wrapped: bool) -> Self {
        Self {
            #[cfg(feature = "unsafe")]
            inner: zlib_rs::Inflate::new(wrapped, 15),
            #[cfg(not(feature = "unsafe"))]
            inner: InflateState::new_boxed(if wrapped {
                DataFormat::Zlib
            } else {
                DataFormat::Raw
            }),
        }
    }

    fn step(&mut self, input: &[u8], eof: bool, out: &mut Vec<u8>) -> Option<(usize, bool)> {
        out.resize(BLOCK, 0);
        #[cfg(feature = "unsafe")]
        let (used, made, done) = {
            let before = (self.inner.total_in(), self.inner.total_out());
            let result = self
                .inner
                .decompress(input, out, zlib_rs::InflateFlush::NoFlush)
                .ok()?;
            (
                (self.inner.total_in() - before.0) as usize,
                (self.inner.total_out() - before.1) as usize,
                result == zlib_rs::Status::StreamEnd,
            )
        };
        #[cfg(not(feature = "unsafe"))]
        let (used, made, done) = {
            let r = inflate(&mut self.inner, input, out, MZFlush::None);
            let done = match r.status {
                Ok(MZStatus::StreamEnd) => true,
                Ok(_) | Err(MZError::Buf) => false,
                _ => return None,
            };
            (r.bytes_consumed, r.bytes_written, done)
        };
        out.truncate(made);
        if !done && used == 0 && made == 0 && !eof {
            return None;
        }
        Some((used, done || eof && used == 0 && made == 0))
    }
}

impl Predict {
    fn step(&mut self, data: &[u8], eof: bool, out: &mut Vec<u8>) -> Option<(usize, bool)> {
        let mut n = 0;
        if self.method != 2 && self.filter.is_none() && !data.is_empty() {
            self.filter = Some(data[0]);
            n = 1;
        }
        let take = (self.width - self.row.len()).min(data.len() - n);
        self.row.extend_from_slice(&data[n..n + take]);
        n += take;
        let end = eof && n == data.len();
        if self.row.len() == self.width || end && !self.row.is_empty() {
            if self.method == 2 {
                if self.bpc == 16 && self.row.len() % 2 == 1 {
                    let missing = self.previous.get(self.row.len()).copied().unwrap_or(0);
                    self.row.push(missing);
                    predictor::tiff_row(&mut self.row, self.colors, self.bpc, self.columns);
                    self.row.pop();
                } else if self.bpc == 16 {
                    self.previous.clone_from(&self.row);
                    predictor::tiff_row(&mut self.row, self.colors, self.bpc, self.columns);
                } else {
                    predictor::tiff_row(&mut self.row, self.colors, self.bpc, self.columns);
                }
            } else if let Some(filter) = self.filter.take().and_then(RowFilter::from_u8) {
                unfilter_row(
                    filter,
                    (self.colors * self.bpc).div_ceil(8),
                    &self.previous,
                    &mut self.row,
                );
            }
            out.extend_from_slice(&self.row);
            if self.method != 2 {
                core::mem::swap(&mut self.row, &mut self.previous);
            }
            self.row.clear();
        }
        Some((n, end))
    }
}
