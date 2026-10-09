//! Restartable decryption with a borrowed ciphertext and bounded cipher state.

use super::aes::{AES128Cipher, AES256Cipher};
use super::rc4::Rc4;
use super::{DecryptionTarget, Decryptor, DecryptorTag, decrypt_rc_aes};
use crate::object::ObjectIdentifier;
use alloc::borrow::Cow;
use alloc::vec::Vec;

pub(crate) struct DecryptedReader<'a> {
    data: Cow<'a, [u8]>,
    cipher: Cipher,
    pos: usize,
    block: [u8; 16],
    taken: usize,
    end: usize,
}

enum Cipher {
    None,
    Rc4 {
        key: [u8; 16],
        key_len: usize,
        current: Rc4,
    },
    Aes128(AES128Cipher),
    Aes256(AES256Cipher),
}

impl Decryptor {
    pub(crate) fn reader<'a>(
        &self,
        id: ObjectIdentifier,
        data: &'a [u8],
        target: DecryptionTarget,
    ) -> Option<DecryptedReader<'a>> {
        let (key, tag) = match self {
            Self::None => (&[][..], DecryptorTag::None),
            Self::Rc4 { key } => (key.as_slice(), DecryptorTag::Rc4),
            Self::Aes128 { key, dict } | Self::Aes256 { key, dict } => {
                let filter = match target {
                    DecryptionTarget::String => dict.string_filter,
                    DecryptionTarget::Stream => dict.stream_filter,
                };
                (key.as_slice(), filter.cfm)
            }
        };
        let cipher = match tag {
            DecryptorTag::None => Cipher::None,
            DecryptorTag::Rc4 => decrypt_rc_aes(key, id, false, |key| {
                let current = Rc4::new(key);
                let mut saved_key = [0; 16];
                saved_key[..key.len()].copy_from_slice(key);
                Some(Cipher::Rc4 {
                    key: saved_key,
                    key_len: key.len(),
                    current,
                })
            })?,
            DecryptorTag::Aes128 => decrypt_rc_aes(key, id, true, |key| {
                Some(Cipher::Aes128(AES128Cipher::new(key)?))
            })?,
            DecryptorTag::Aes256 => Cipher::Aes256(AES256Cipher::new(key)?),
        };
        if matches!(cipher, Cipher::Aes128(_) | Cipher::Aes256(_)) && data.len() < 16 {
            return None;
        }
        let mut reader = DecryptedReader::raw(Cow::Borrowed(data));
        reader.cipher = cipher;
        reader.rewind();
        Some(reader)
    }
}

impl<'a> DecryptedReader<'a> {
    pub(crate) fn raw(data: Cow<'a, [u8]>) -> Self {
        Self {
            data,
            cipher: Cipher::None,
            pos: 0,
            block: [0; 16],
            taken: 0,
            end: 0,
        }
    }

    /// Borrowed unencrypted bytes can be indexed without copying the input.
    #[cfg(feature = "unsafe")]
    pub(crate) fn borrowed_plaintext(&self) -> Option<&'a [u8]> {
        match (&self.cipher, &self.data) {
            (Cipher::None, Cow::Borrowed(data)) => Some(data),
            _ => None,
        }
    }

    pub(crate) fn rewind(&mut self) {
        self.pos = match &mut self.cipher {
            Cipher::None => 0,
            Cipher::Rc4 {
                key,
                key_len,
                current,
            } => {
                *current = Rc4::new(&key[..*key_len]);
                0
            }
            Cipher::Aes128(_) | Cipher::Aes256(_) => 16,
        };
        self.taken = 0;
        self.end = 0;
    }

    pub(crate) fn read(&mut self, output: &mut [u8]) -> usize {
        match &mut self.cipher {
            Cipher::None | Cipher::Rc4 { .. } => {
                let n = output.len().min(self.data.len() - self.pos);
                output[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                if let Cipher::Rc4 { current, .. } = &mut self.cipher {
                    current.decrypt_in_place(&mut output[..n]);
                }
                self.pos += n;
                n
            }
            Cipher::Aes128(_) | Cipher::Aes256(_) => {
                let mut written = 0;
                while written < output.len() {
                    if self.taken == self.end {
                        if self.data.len() - self.pos < 16 {
                            break;
                        }
                        let ciphertext = self.data[self.pos..self.pos + 16].try_into().unwrap();
                        self.block = match &self.cipher {
                            Cipher::Aes128(cipher) => cipher.decrypt_block(ciphertext),
                            Cipher::Aes256(cipher) => cipher.decrypt_block(ciphertext),
                            _ => unreachable!(),
                        };
                        for (plain, previous) in self
                            .block
                            .iter_mut()
                            .zip(&self.data[self.pos - 16..self.pos])
                        {
                            *plain ^= previous;
                        }
                        self.pos += 16;
                        self.taken = 0;
                        self.end = 16;
                        // Match decrypt_cbc: ignore an incomplete trailing block and
                        // remove padding only if every byte of the last block agrees.
                        if self.data.len() - self.pos < 16 {
                            let padding = usize::from(self.block[15]);
                            if (1..=16).contains(&padding)
                                && self.block[16 - padding..]
                                    .iter()
                                    .all(|&b| usize::from(b) == padding)
                            {
                                self.end -= padding;
                            }
                        }
                    }
                    let n = (output.len() - written).min(self.end - self.taken);
                    output[written..written + n]
                        .copy_from_slice(&self.block[self.taken..self.taken + n]);
                    self.taken += n;
                    written += n;
                }
                written
            }
        }
    }

    /// The permissive Flate fallback needs the complete decrypted input.
    pub(crate) fn into_data(mut self) -> Cow<'a, [u8]> {
        if matches!(self.cipher, Cipher::None) {
            return self.data;
        }
        self.rewind();
        let mut data = Vec::new();
        let mut buf = [0; 8192];
        loop {
            let n = self.read(&mut buf);
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
        }
        Cow::Owned(data)
    }
}

#[cfg(feature = "unsafe")]
impl std::io::Read for DecryptedReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        Ok(Self::read(self, output))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CryptDictionary, DecryptorData};
    use super::*;

    fn decryptor(tag: DecryptorTag) -> Decryptor {
        let filter = CryptDictionary {
            cfm: tag,
            _length: 32,
        };
        Decryptor::Aes256 {
            key: alloc::vec![17; 32],
            dict: DecryptorData {
                stream_filter: filter,
                string_filter: filter,
            },
        }
    }

    fn check(decryptor: &Decryptor, data: &[u8]) {
        let id = ObjectIdentifier::new(1234, 17);
        let expected = decryptor.decrypt(id, data, DecryptionTarget::Stream);
        for chunk in [1, 7, 16, 19, 8192] {
            let Some(mut reader) = decryptor.reader(id, data, DecryptionTarget::Stream) else {
                assert!(expected.is_none());
                continue;
            };
            for prefix in [0, 1, 15, 17, 37, data.len() + 1] {
                reader.read(&mut alloc::vec![0; prefix]);
                reader.rewind();
                let mut actual = Vec::new();
                let mut buf = alloc::vec![0; chunk];
                loop {
                    let n = reader.read(&mut buf);
                    if n == 0 {
                        break;
                    }
                    actual.extend_from_slice(&buf[..n]);
                }
                assert_eq!(
                    actual.as_slice(),
                    expected.as_deref().unwrap(),
                    "chunk {chunk}, prefix {prefix}"
                );
                assert_eq!(reader.read(&mut buf), 0);
            }
        }
    }

    #[test]
    fn every_cipher_matches_whole_decryption_across_chunks_and_rewinds() {
        for decryptor in [
            Decryptor::None,
            Decryptor::Rc4 {
                key: alloc::vec![13; 5],
            },
            decryptor(DecryptorTag::None),
            decryptor(DecryptorTag::Rc4),
            decryptor(DecryptorTag::Aes128),
            decryptor(DecryptorTag::Aes256),
        ] {
            for len in (0..66).chain([8193]) {
                let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
                check(&decryptor, &bytes);
            }
        }
    }

    #[test]
    fn aes_padding_and_incomplete_blocks_match_whole_decryption() {
        let cipher = AES256Cipher::new(&[17; 32]).unwrap();
        let decryptor = decryptor(DecryptorTag::Aes256);
        for len in 0..32 {
            let mut data = alloc::vec![9; 16];
            data.extend(cipher.encrypt_cbc(&alloc::vec![42; len], &[9; 16]));
            check(&decryptor, &data);
            // The legacy decoder ignores trailing partial blocks and still unpads
            // the last complete block; the incremental reader must agree.
            data.extend([1, 2, 3]);
            check(&decryptor, &data);
        }
    }
}
