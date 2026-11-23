// Keep in sync with `hayro-postscript/src/string/ascii_85.rs`, which decodes as
// strict decoding does.

use crate::filter::bytewise::{self, Decoder};
use crate::work::Meter;
use alloc::vec::Vec;

/// The data decoded, each byte read counted on `meter`; `None` once its hook asks to
/// stop, or where a strict decoder finds data it cannot read.
pub(crate) fn decode(data: &[u8], meter: &Meter<'_>, strict: bool) -> Option<Vec<u8>> {
    bytewise::decode(Decoder::ascii_85(), data, strict, meter)
}
