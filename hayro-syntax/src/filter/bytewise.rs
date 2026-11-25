//! The decoders that take their data a byte or a few at a time: LZW, ASCII hex,
//! ASCII base-85 and run length. Whole decoding, the strict stages of a chain and
//! the lenient reader run the same steps over their own input, so they decode the
//! same bytes and end the same.
//!
//! Strict decoding fails on data a decoder cannot read, a `z` within an ASCII
//! base-85 group included. Lenient decoding keeps the bytes made before such data and
//! ends there, as `MuPDF` does (observed with `PyMuPDF` 1.28.2): a partial ASCII base-85
//! group or ASCII hex digit before it is dropped, an ASCII base-85 group of one
//! character at the end is ignored, a group whose value overflows 32 bits wraps
//! around, and run-length data end where they end without an end of data marker.
//! Both keep the bytes of a run cut short by the end of the data, which is no
//! corruption.

use crate::filter::ascii_hex::decode_hex_digit;
use crate::filter::lzw_flate::flate::fallback::{Input, Slice};
use crate::filter::lzw_flate::lzw;
use crate::trivia::is_white_space_character;
use crate::work::{Meter, POLL_BYTES};
use alloc::vec::Vec;

/// Where a decoding is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    Going,
    /// At the end of the data.
    Done,
    /// At data that cannot be decoded, for a strict decoder, or once the meter's
    /// hook asked to stop.
    Failed,
}

/// A decoder that takes a byte or a few at a time, and where it is in the data.
#[derive(Clone)]
pub(crate) enum Decoder {
    Lzw(lzw::Decoder),
    /// The high digit of the byte being read.
    AsciiHex(Option<u8>),
    Ascii85(Group),
    RunLength,
}

/// The digits of the ASCII base-85 group being read.
#[derive(Clone, Copy, Default)]
pub(crate) struct Group {
    digits: [u8; 5],
    len: u8,
}

impl Decoder {
    pub(crate) fn lzw(early_change: bool) -> Self {
        Self::Lzw(lzw::Decoder::new(early_change))
    }

    pub(crate) fn ascii_hex() -> Self {
        Self::AsciiHex(None)
    }

    pub(crate) fn ascii_85() -> Self {
        Self::Ascii85(Group::default())
    }

    /// Back to the start of the data.
    pub(crate) fn start_over(&mut self) {
        *self = match self {
            Self::Lzw(decoder) => Self::lzw(decoder.early_change()),
            Self::AsciiHex(_) => Self::ascii_hex(),
            Self::Ascii85(_) => Self::ascii_85(),
            Self::RunLength => Self::RunLength,
        }
    }

    /// The heap bytes a copy of it holds.
    #[cfg(any(feature = "unsafe", feature = "images"))]
    pub(crate) fn heap_bytes(&self) -> usize {
        match self {
            Self::Lzw(decoder) => decoder.heap_bytes(),
            _ => 0,
        }
    }

    /// The most heap bytes a copy of it holds, at any position.
    #[cfg(feature = "unsafe")]
    pub(crate) fn heap_bound(&self) -> usize {
        match self {
            Self::Lzw(_) => lzw::Decoder::HEAP_BOUND,
            _ => 0,
        }
    }

    /// Decodes `input` into `out` until `out` holds `until` bytes (a run may make a
    /// few more), the data end or fail, or the input is [`Input::low`] before a step
    /// (a step reads at most 129 bytes, a run of bytes as they are), each byte taken
    /// and made counted on `meter`.
    pub(crate) fn make(
        &mut self,
        input: &mut impl Input,
        out: &mut Vec<u8>,
        until: usize,
        strict: bool,
        meter: &Meter<'_>,
    ) -> State {
        match self {
            Self::Lzw(decoder) => loop {
                if out.len() >= until || input.low() {
                    break State::Going;
                }
                match decoder.step(input, out, meter) {
                    Some(true) => {}
                    Some(false) => break State::Done,
                    None if strict || meter.cancelled() => break State::Failed,
                    // The bytes of the codes before an invalid one are kept.
                    None => break State::Done,
                }
            },
            Self::AsciiHex(high) => ascii_hex(input, high, out, until, strict, meter),
            Self::Ascii85(group) => ascii_85(input, group, out, until, strict, meter),
            Self::RunLength => run_length(input, out, until, strict, meter),
        }
    }
}

/// `data` decoded whole by `decoder`, its work counted on `meter`: `Ok(None)` where
/// a strict decoder fails or the hook asks to stop, `Err` once it made more than
/// `limit` bytes.
pub(crate) fn decode_within(
    mut decoder: Decoder,
    data: &[u8],
    strict: bool,
    meter: &Meter<'_>,
    limit: usize,
) -> Result<Option<Vec<u8>>, ()> {
    let mut decoded = Vec::new();
    let mut input = Slice::new(data);
    // Made a poll's worth at a time. Room of a poll's worth or less grows as pushing
    // grows it; past that, a step makes no more than the room holds, which doubles on
    // the meter once a step could fill it, as pushing would double it. So the room
    // doubles once more than pushing would only for data ending within a step of it.
    let state = loop {
        let (len, room) = (decoded.len(), decoded.capacity());
        let mut until = len.saturating_add(POLL_BYTES).min(limit.saturating_add(1));
        if room >= POLL_BYTES {
            if room - len <= STEP_AT_MOST {
                meter.reserve_exact(&mut decoded, room);
            }
            until = until.min(decoded.capacity() - STEP_AT_MOST);
        }
        match decoder.make(&mut input, &mut decoded, until, strict, meter) {
            State::Going if decoded.len() <= limit => {}
            state => break state,
        }
    };
    if decoded.len() > limit {
        return Err(());
    }
    Ok(match state {
        State::Done if !meter.cancelled() => Some(decoded),
        _ => None,
    })
}

/// The most bytes a decoder's step makes past the bytes it was asked for: an LZW
/// code's, at most the table's size.
pub(crate) const STEP_AT_MOST: usize = 4096;

/// [`decode_within`] without a limit.
pub(crate) fn decode(
    decoder: Decoder,
    data: &[u8],
    strict: bool,
    meter: &Meter<'_>,
) -> Option<Vec<u8>> {
    decode_within(decoder, data, strict, meter, usize::MAX).unwrap_or(None)
}

/// ASCII hex: up to a `>`, white space skipped, a last digit alone taken as the high
/// one.
fn ascii_hex(
    input: &mut impl Input,
    high: &mut Option<u8>,
    out: &mut Vec<u8>,
    until: usize,
    strict: bool,
    meter: &Meter<'_>,
) -> State {
    loop {
        if out.len() >= until || input.low() {
            return State::Going;
        }
        let Some(byte) = input.next(meter) else {
            break;
        };
        if meter.work(1).is_err() {
            return State::Failed;
        }
        if byte == b'>' {
            break;
        }
        if is_white_space_character(byte) {
            continue;
        }
        match (decode_hex_digit(byte), high.take()) {
            (Some(digit), Some(high)) => out.push(high << 4 | digit),
            (Some(digit), None) => *high = Some(digit),
            (None, _) if strict => return State::Failed,
            (None, _) => return State::Done,
        }
    }
    if let Some(high) = high.take() {
        out.push(high << 4);
    }
    State::Done
}

/// ASCII base-85, white space skipped, up to a `~` or the end of the data, where a
/// partial group is decoded as if padded with `u`.
fn ascii_85(
    input: &mut impl Input,
    group: &mut Group,
    out: &mut Vec<u8>,
    until: usize,
    strict: bool,
    meter: &Meter<'_>,
) -> State {
    let end = |group: &mut Group, out: &mut Vec<u8>| match group.flush(out, strict) {
        Some(()) => State::Done,
        None => State::Failed,
    };
    while out.len() < until && !input.low() {
        let Some(b) = input.next(meter) else {
            // Be lenient and accept what we have (see PDFBOX-5910).
            return end(group, out);
        };
        if meter.work(1).is_err() {
            return State::Failed;
        }
        if is_white_space_character(b) {
            continue;
        }
        match b {
            b'!'..=b'u' => {
                group.digits[usize::from(group.len)] = b - b'!';
                group.len += 1;
                if group.len == 5 && group.flush(out, strict).is_none() {
                    return State::Failed;
                }
            }
            b'z' if group.len == 0 => out.extend_from_slice(&[0, 0, 0, 0]),
            b'~' => return end(group, out),
            // An invalid character, or a `z` within a group.
            _ if strict => return State::Failed,
            _ => return State::Done,
        }
    }
    State::Going
}

impl Group {
    /// Appends the bytes of the group, the digits it lacks taken as `u`, and empties
    /// it; `None` where a strict decoder fails on it.
    fn flush(&mut self, out: &mut Vec<u8>, strict: bool) -> Option<()> {
        const POW_85: [u32; 5] = [52200625, 614125, 7225, 85, 1];
        let len = usize::from(core::mem::take(&mut self.len));
        match len {
            0 => return Some(()),
            // A single character is not valid.
            1 if strict => return None,
            1 => return Some(()),
            _ => {}
        }
        let mut digits = [u32::from(b'u' - b'!'); 5];
        for (digit, &d) in digits.iter_mut().zip(&self.digits[..len]) {
            *digit = u32::from(d);
        }
        let value = match strict {
            true => digits
                .iter()
                .zip(POW_85)
                .try_fold(0_u32, |v, (&d, p)| v.checked_add(d.checked_mul(p)?))?,
            false => digits
                .iter()
                .zip(POW_85)
                .fold(0_u32, |v, (&d, p)| v.wrapping_add(d.wrapping_mul(p))),
        };
        out.extend_from_slice(&value.to_be_bytes()[..len - 1]);
        Some(())
    }
}

/// Run length, up to its end of data marker. Data that end in a run of bytes as they
/// are that is cut short keep the bytes it holds; strict decoding fails on data that
/// end otherwise without the marker.
fn run_length(
    input: &mut impl Input,
    out: &mut Vec<u8>,
    until: usize,
    strict: bool,
    meter: &Meter<'_>,
) -> State {
    let cut = match strict {
        true => State::Failed,
        false => State::Done,
    };
    while out.len() < until && !input.low() {
        let Some(length) = input.next(meter) else {
            return cut;
        };
        if meter.work(usize::from(length) + 1).is_err() {
            return State::Failed;
        }
        match length {
            128 => return State::Done,
            0..=127 => {
                for _ in 0..=length {
                    match input.next(meter) {
                        Some(byte) => out.push(byte),
                        None => return State::Done,
                    }
                }
            }
            _ => {
                let Some(byte) = input.next(meter) else {
                    return cut;
                };
                out.extend(core::iter::repeat_n(byte, 257 - usize::from(length)));
            }
        }
    }
    State::Going
}

#[cfg(test)]
mod tests {
    use super::{Decoder, decode};
    use crate::work::Meter;
    use alloc::vec::Vec;

    /// Decoded strictly and leniently.
    fn both(decoder: Decoder, data: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let meter = Meter::default();
        (
            decode(decoder.clone(), data, true, &meter),
            decode(decoder, data, false, &meter),
        )
    }

    /// The bytes 1 to 8, in two ASCII base-85 groups.
    const GROUPS: &[u8] = b"!<N?+\"U52;";

    /// Lenient decoding keeps what comes before data it cannot read, as `MuPDF` does;
    /// strict decoding fails there, where it keeps nothing.
    #[test]
    fn ascii_85_keeps_the_groups_before_damage() {
        let eight: Vec<u8> = (1..=8).collect();
        for (case, tail, lenient) in [
            // An invalid character, or a `z`, after part of a group: the part goes.
            ("invalid", &b"{"[..], &eight[..]),
            ("invalid in a group", b"9j{", &eight),
            ("z in a group", b"9jz", &eight),
            // A lone digit at the end is ignored.
            ("lone digit at ~>", b"A~>", &eight),
            ("lone digit at the end", b"A", &eight),
        ] {
            let data = [GROUPS, tail].concat();
            assert_eq!(
                both(Decoder::ascii_85(), &data),
                (None, Some(lenient.to_vec())),
                "{case}"
            );
        }
        // A group past 2^32 wraps around, as `uuuuu` does in `MuPDF`.
        let data = [GROUPS, b"uuuuu", GROUPS].concat();
        let mut wrapped = (1..=8).collect::<Vec<u8>>();
        wrapped.extend([0x08, 0x78, 0x0e, 0xc4]);
        wrapped.extend(1..=8);
        assert_eq!(both(Decoder::ascii_85(), &data), (None, Some(wrapped)));
        // A `z` between groups is four zeros for both.
        let data = [GROUPS, b"z~>"].concat();
        let zeros = [&(1..=8).collect::<Vec<u8>>()[..], &[0; 4]].concat();
        assert_eq!(
            both(Decoder::ascii_85(), &data),
            (Some(zeros.clone()), Some(zeros))
        );
    }

    #[test]
    fn ascii_hex_keeps_the_bytes_before_damage() {
        assert_eq!(
            both(Decoder::ascii_hex(), b"0102 0x03>"),
            (None, Some(alloc::vec![1, 2]))
        );
        // The digit before the damage is dropped, though one alone at the end counts.
        assert_eq!(
            both(Decoder::ascii_hex(), b"01023x>"),
            (None, Some(alloc::vec![1, 2]))
        );
        assert_eq!(
            both(Decoder::ascii_hex(), b"01023>"),
            (Some(alloc::vec![1, 2, 0x30]), Some(alloc::vec![1, 2, 0x30]))
        );
    }

    #[test]
    fn run_length_keeps_the_runs_before_its_end() {
        let runs = [2, 1, 2, 3, 254, 9];
        // Without an end of data marker, or cut in a repeated run.
        for data in [&runs[..], &[&runs[..], &[250]].concat()] {
            assert_eq!(
                both(Decoder::RunLength, data),
                (None, Some(alloc::vec![1, 2, 3, 9, 9, 9]))
            );
        }
        // A run of bytes as they are cut short keeps its bytes, for both.
        let cut = [&runs[..], &[5, 7, 8]].concat();
        let kept = alloc::vec![1, 2, 3, 9, 9, 9, 7, 8];
        assert_eq!(
            both(Decoder::RunLength, &cut),
            (Some(kept.clone()), Some(kept))
        );
    }

    #[test]
    fn lzw_keeps_the_codes_before_an_invalid_one() {
        // Clear, `A`, `B`, then code 300, past the table's 260 entries.
        let codes = [256_u16, 65, 66, 300];
        let mut bits = 0_u64;
        for code in codes {
            bits = bits << 9 | u64::from(code);
        }
        let data = (bits << 28).to_be_bytes()[..5].to_vec();
        assert_eq!(
            both(Decoder::lzw(true), &data),
            (None, Some(alloc::vec![b'A', b'B']))
        );
    }
}
