use super::aes::{AES128Cipher, AES256Cipher, padding_len};
use super::rc4::Rc4;
use super::{DecryptionTarget, Decryptor, DecryptorTag, decrypt_rc_aes};
use crate::object::ObjectIdentifier;

/// A cursor over stored PDF bytes, with CBC blocks addressed directly.
pub struct DecryptedReader<'a> {
    data: &'a [u8],
    cipher: Cipher,
    pos: usize,
    end: usize,
}

enum Cipher {
    Plain,
    Rc4(Rc4),
    Aes128(AES128Cipher),
    Aes256(AES256Cipher),
}

impl Cipher {
    fn block(&self, data: &[u8], at: usize) -> [u8; 16] {
        let (previous, block) = data[at..at + 32].split_at(16);
        let previous = previous.try_into().unwrap();
        let block = block.try_into().unwrap();
        match self {
            Self::Aes128(c) => c.decrypt_cbc_block(block, previous),
            Self::Aes256(c) => c.decrypt_cbc_block(block, previous),
            _ => unreachable!(),
        }
    }
}

impl<'a> DecryptedReader<'a> {
    pub(crate) fn plain(data: &'a [u8]) -> Self {
        Self {
            data,
            cipher: Cipher::Plain,
            pos: 0,
            end: data.len(),
        }
    }

    pub(crate) fn new(
        data: &'a [u8],
        decryptor: &Decryptor,
        id: ObjectIdentifier,
        target: DecryptionTarget<'_>,
    ) -> Option<Self> {
        let (key, method) = decryptor.method(target)?;
        let cipher = match method {
            DecryptorTag::None => Cipher::Plain,
            DecryptorTag::Rc4 => {
                Cipher::Rc4(decrypt_rc_aes(key, id, false, |key| Some(Rc4::new(key)))?)
            }
            DecryptorTag::Aes128 => {
                Cipher::Aes128(decrypt_rc_aes(key, id, true, AES128Cipher::new)?)
            }
            DecryptorTag::Aes256 => Cipher::Aes256(AES256Cipher::new(key)?),
        };
        let end = match &cipher {
            Cipher::Aes128(_) | Cipher::Aes256(_) => {
                let len = data.len().checked_sub(16)? / 16 * 16;
                if len == 0 {
                    0
                } else {
                    len - padding_len(&cipher.block(data, len - 16))
                }
            }
            _ => data.len(),
        };
        Some(Self {
            data,
            cipher,
            pos: 0,
            end,
        })
    }

    /// Whether skipping can address the target block directly.
    pub fn can_seek(&self) -> bool {
        !matches!(self.cipher, Cipher::Rc4(_))
    }

    /// Reads the next decrypted bytes.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.end - self.pos);
        match &mut self.cipher {
            Cipher::Plain | Cipher::Rc4(_) => {
                out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                if let Cipher::Rc4(c) = &mut self.cipher {
                    c.decrypt_in_place(&mut out[..n]);
                }
            }
            cipher => {
                let mut written = 0;
                while written < n {
                    let at = self.pos + written;
                    let offset = at % 16;
                    let block = cipher.block(self.data, at - offset);
                    let take = (16 - offset).min(n - written);
                    out[written..written + take].copy_from_slice(&block[offset..offset + take]);
                    written += take;
                }
            }
        }
        self.pos += n;
        n
    }

    /// Advances through the decrypted data, returning the number of available bytes skipped.
    pub fn skip(&mut self, n: usize) -> usize {
        let n = n.min(self.end - self.pos);
        if let Cipher::Rc4(c) = &mut self.cipher {
            c.skip(n);
        }
        self.pos += n;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{CryptDictionary, DecryptorData};
    use alloc::vec::Vec;

    #[test]
    fn skipped_reads_match_whole_decryption_on_block_boundaries() {
        let data: Vec<_> = (0..=255).cycle().take(4096).collect();
        let id = ObjectIdentifier::new(91, 2);
        let config = |tag| {
            let filter = CryptDictionary {
                cfm: tag,
                _length: 16,
            };
            DecryptorData {
                stream_filter: filter,
                string_filter: filter,
                crypt_filters: crate::hash::Map::default(),
            }
        };
        let codecs = [
            Decryptor::None,
            Decryptor::Rc4 {
                key: alloc::vec![1;16],
            },
            Decryptor::Aes128 {
                key: alloc::vec![2;16],
                dict: config(DecryptorTag::Aes128),
            },
            Decryptor::Aes256 {
                key: alloc::vec![3;32],
                dict: config(DecryptorTag::Aes256),
            },
        ];
        for codec in codecs {
            let expected = codec.decrypt(id, &data, DecryptionTarget::Stream).unwrap();
            for start in [0, 1, 15, 16, 17, 2007, 4044, 4080, 4096] {
                let mut cursor =
                    DecryptedReader::new(&data, &codec, id, DecryptionTarget::Stream).unwrap();
                assert_eq!(cursor.can_seek(), !matches!(codec, Decryptor::Rc4 { .. }));
                let moved = cursor.skip(start);
                assert_eq!(moved, start.min(expected.len()));
                let mut buf = [0; 37];
                let n = cursor.read(&mut buf);
                assert_eq!(
                    &buf[..n],
                    &expected[moved..(moved + 37).min(expected.len())]
                );
            }
        }
    }
}
