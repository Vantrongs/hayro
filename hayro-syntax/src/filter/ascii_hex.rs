use crate::trivia::is_white_space_character;
use crate::util::ByteBuf;

pub(crate) fn decode_into<B: ByteBuf>(data: &[u8], recover_prefix: bool) -> Option<B> {
    let mut has_whitespace = false;
    let mut digit_count = 0;
    for &byte in data {
        if byte.is_ascii_hexdigit() {
            digit_count += 1;
        } else if is_white_space_character(byte) {
            has_whitespace = true;
        } else {
            if byte != b'>' {
                if !recover_prefix {
                    return None;
                }
                digit_count -= digit_count % 2;
            }
            break;
        }
    }

    let bytes = data.iter().copied();
    Some(if has_whitespace {
        decode_pairs(bytes.filter(|&b| !is_white_space_character(b)), digit_count)
    } else {
        decode_pairs(bytes, digit_count)
    })
}

fn decode_pairs<B: ByteBuf>(bytes: impl Iterator<Item = u8>, digit_count: usize) -> B {
    let mut digits = bytes.take(digit_count).map_while(decode_hex_digit);
    let mut decoded = B::with_capacity(digit_count.div_ceil(2));
    while let Some(hi) = digits.next() {
        decoded.push(hi << 4 | digits.next().unwrap_or(0));
    }

    decoded
}

#[inline(always)]
pub(crate) fn decode_hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'A'..=b'F' => Some(c - b'A' + 10),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::decode_into;
    use alloc::vec::Vec;

    #[test]
    fn decode_simple() {
        let input = b"AF3E2901>";
        assert_eq!(
            decode_into::<Vec<u8>>(input, false).unwrap(),
            vec![0xaf, 0x3e, 0x29, 0x01]
        );
    }

    #[test]
    fn decode_whitespaces() {
        let input = b"AF3   E2   901>";
        assert_eq!(
            decode_into::<Vec<u8>>(input, false).unwrap(),
            vec![0xaf, 0x3e, 0x29, 0x01]
        );
    }

    #[test]
    fn decode_without_gt() {
        let input = b"AF3E2901";
        assert_eq!(
            decode_into::<Vec<u8>>(input, false).unwrap(),
            vec![0xaf, 0x3e, 0x29, 0x01]
        );
    }

    #[test]
    fn decode_with_padding() {
        let input = b"AF3E291>";
        assert_eq!(
            decode_into::<Vec<u8>>(input, false).unwrap(),
            vec![0xaf, 0x3e, 0x29, 0x10]
        );
    }

    #[test]
    fn decode_invalid_low_nibble() {
        for input in [b"AB3.45>".as_slice(), b"AB 3 .45>"] {
            assert!(decode_into::<Vec<u8>>(input, false).is_none());
            let decoded = decode_into::<Vec<u8>>(input, true).unwrap();
            assert_eq!(decoded, vec![0xab]);
            assert_eq!(decoded.capacity(), 1);
        }
    }
}
