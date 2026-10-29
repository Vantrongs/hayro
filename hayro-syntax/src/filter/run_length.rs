use crate::filter::bytewise::{self, Decoder};
use crate::work::Meter;
use alloc::vec::Vec;

/// The data decoded, each run counted on `meter`; `None` once its hook asks to stop,
/// or where a strict decoder finds the data end without an end of data marker.
pub(crate) fn decode(data: &[u8], meter: &Meter<'_>, strict: bool) -> Option<Vec<u8>> {
    bytewise::decode(Decoder::RunLength, data, strict, meter)
}

#[cfg(test)]
mod tests {
    use crate::filter::run_length::decode;
    use crate::work::Meter;

    #[test]
    fn run_length() {
        let input = vec![4, 10, 11, 12, 13, 14, 253, 3, 128];
        for strict in [true, false] {
            assert_eq!(
                decode(&input, &Meter::default(), strict).unwrap(),
                vec![10, 11, 12, 13, 14, 3, 3, 3, 3]
            );
        }
    }
}
