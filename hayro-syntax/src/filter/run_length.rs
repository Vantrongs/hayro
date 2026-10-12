use crate::reader::Reader;
use crate::work::Meter;
use alloc::vec;
use alloc::vec::Vec;

/// The data decoded, each run counted on `meter`; `None` once its hook asks to stop.
pub(crate) fn decode(data: &[u8], meter: &Meter<'_>) -> Option<Vec<u8>> {
    let mut reader = Reader::new(data);
    let mut decoded = vec![];

    loop {
        let length = reader.read_byte()?;
        meter.work(usize::from(length) + 1).ok()?;

        match length {
            128 => break,
            0..=127 => {
                // PDFBOX-3990, just abort early if stream is invalid.
                let Some(bytes) = reader.read_bytes(length as usize + 1) else {
                    break;
                };

                decoded.extend(bytes);
            }
            _ => {
                let length = 257 - length as usize;
                decoded.extend([reader.read_byte()?].repeat(length));
            }
        }
    }

    Some(decoded)
}

#[cfg(test)]
mod tests {
    use crate::filter::run_length::decode;
    use crate::work::Meter;

    #[test]
    fn run_length() {
        let input = vec![4, 10, 11, 12, 13, 14, 253, 3, 128];
        assert_eq!(
            decode(&input, &Meter::default()).unwrap(),
            vec![10, 11, 12, 13, 14, 3, 3, 3, 3]
        );
    }
}
