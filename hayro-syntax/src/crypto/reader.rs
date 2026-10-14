//! A stream's stored bytes, decrypted as they are read.

use super::aes::{AES128Cipher, AES256Cipher, padding_len};
use super::rc4::Rc4;
use super::{DecryptionTarget, Decryptor, DecryptorTag, decrypt_rc_aes};
use crate::object::ObjectIdentifier;
use crate::work::{Cancelled, Meter};
use alloc::borrow::Cow;
use alloc::vec::Vec;

/// Reads the bytes [`Decryptor::decrypt`] returns for the same data, in pieces, with
/// no more than one AES block of them held at a time.
pub(crate) struct DecryptedReader<'a> {
    data: Cow<'a, [u8]>,
    cursor: Cursor,
}

/// Where decryption stands: the position in the stored bytes and the cipher's
/// running state.
#[derive(Clone)]
pub(crate) struct Cursor {
    cipher: Cipher,
    pos: usize,
}

#[derive(Clone)]
enum Cipher {
    None,
    /// RC4 keeps its key to start over, being a running keystream.
    Rc4 {
        key: [u8; 16],
        key_len: usize,
        current: Rc4,
    },
    Aes(AesBlocks),
}

/// AES in CBC mode, one decrypted block at a time.
#[derive(Clone)]
struct AesBlocks {
    aes: Aes,
    /// The block being handed out, `taken` bytes of it so far, `end` long without
    /// padding.
    block: [u8; 16],
    taken: usize,
    end: usize,
}

#[derive(Clone)]
enum Aes {
    Aes128(AES128Cipher),
    Aes256(AES256Cipher),
}

impl Decryptor {
    /// A reader of `data` decrypted for object `id`; `None` where [`Self::decrypt`]
    /// would fail.
    pub(crate) fn reader<'a>(
        &self,
        id: ObjectIdentifier,
        data: &'a [u8],
        target: DecryptionTarget<'_>,
    ) -> Option<DecryptedReader<'a>> {
        let (key, tag) = self.method(target)?;
        let cipher = match tag {
            DecryptorTag::None => Cipher::None,
            DecryptorTag::Rc4 => decrypt_rc_aes(key, id, false, |key| {
                let mut saved = [0; 16];
                saved[..key.len()].copy_from_slice(key);
                Some(Cipher::Rc4 {
                    key: saved,
                    key_len: key.len(),
                    current: Rc4::new(key),
                })
            })?,
            DecryptorTag::Aes128 => {
                decrypt_rc_aes(key, id, true, |key| AES128Cipher::new(key).map(Aes::Aes128))?
                    .blocks(data)?
            }
            DecryptorTag::Aes256 => Aes::Aes256(AES256Cipher::new(key)?).blocks(data)?,
        };
        let mut reader = DecryptedReader::raw(Cow::Borrowed(data));
        reader.cursor.cipher = cipher;
        reader.rewind();
        Some(reader)
    }
}

impl Aes {
    /// The cipher for `data`, which starts with its IV.
    fn blocks(self, data: &[u8]) -> Option<Cipher> {
        (data.len() >= 16).then_some(Cipher::Aes(AesBlocks {
            aes: self,
            block: [0; 16],
            taken: 0,
            end: 0,
        }))
    }

    fn decrypt_cbc_block(&self, block: &[u8; 16], previous: &[u8; 16]) -> [u8; 16] {
        match self {
            Self::Aes128(aes) => aes.decrypt_cbc_block(block, previous),
            Self::Aes256(aes) => aes.decrypt_cbc_block(block, previous),
        }
    }
}

impl AesBlocks {
    fn read(&mut self, data: &[u8], pos: &mut usize, out: &mut [u8]) -> usize {
        let mut written = 0;
        while written < out.len() {
            if self.taken == self.end && !self.next_block(data, pos) {
                break;
            }
            let n = (out.len() - written).min(self.end - self.taken);
            out[written..written + n].copy_from_slice(&self.block[self.taken..self.taken + n]);
            self.taken += n;
            written += n;
        }
        written
    }

    /// Decrypts the next whole block, without the padding if it is the last; false
    /// at the end. A trailing partial block is ignored, as `decrypt_cbc` does.
    fn next_block(&mut self, data: &[u8], pos: &mut usize) -> bool {
        let Some(block) = data.get(*pos..*pos + 16) else {
            return false;
        };
        let previous = &data[*pos - 16..*pos];
        self.block = self
            .aes
            .decrypt_cbc_block(block.try_into().unwrap(), previous.try_into().unwrap());
        *pos += 16;
        self.taken = 0;
        self.end = 16;
        if data.len() - *pos < 16 {
            self.end -= padding_len(&self.block);
        }
        true
    }
}

impl<'a> DecryptedReader<'a> {
    /// `data` as it is, not encrypted.
    pub(crate) fn raw(data: Cow<'a, [u8]>) -> Self {
        Self {
            data,
            cursor: Cursor {
                cipher: Cipher::None,
                pos: 0,
            },
        }
    }

    /// The stored bytes, when they are borrowed from the document rather than owned.
    #[cfg(feature = "unsafe")]
    pub(crate) fn borrowed_data(&self) -> Option<&'a [u8]> {
        match &self.data {
            Cow::Borrowed(data) => Some(data),
            Cow::Owned(_) => None,
        }
    }

    #[cfg(feature = "unsafe")]
    pub(crate) fn cursor(&self) -> Cursor {
        self.cursor.clone()
    }

    /// Continues from `cursor`, taken from a reader of the same data.
    #[cfg(feature = "unsafe")]
    pub(crate) fn restore(&mut self, cursor: &Cursor) {
        self.cursor = cursor.clone();
    }

    /// The stored bytes read.
    #[cfg(all(test, feature = "unsafe"))]
    pub(crate) fn position(&self) -> usize {
        self.cursor.pos
    }

    /// Back to the start of the data.
    pub(crate) fn rewind(&mut self) {
        let c = &mut self.cursor;
        c.pos = match &mut c.cipher {
            Cipher::None => 0,
            Cipher::Rc4 {
                key,
                key_len,
                current,
            } => {
                *current = Rc4::new(&key[..*key_len]);
                0
            }
            Cipher::Aes(blocks) => {
                blocks.taken = 0;
                blocks.end = 0;
                16
            }
        };
    }

    /// Fills `out` with the next decrypted bytes and returns how many; 0 only at the
    /// end.
    pub(crate) fn read(&mut self, out: &mut [u8]) -> usize {
        let Cursor { cipher, pos } = &mut self.cursor;
        let n = match cipher {
            Cipher::Aes(blocks) => return blocks.read(&self.data, pos, out),
            Cipher::None | Cipher::Rc4 { .. } => out.len().min(self.data.len() - *pos),
        };
        out[..n].copy_from_slice(&self.data[*pos..*pos + n]);
        if let Cipher::Rc4 { current, .. } = cipher {
            current.decrypt_in_place(&mut out[..n]);
        }
        *pos += n;
        n
    }

    /// The stored bytes, when they need no decryption.
    pub(crate) fn plain(&self) -> Option<&[u8]> {
        matches!(self.cursor.cipher, Cipher::None).then_some(&*self.data)
    }

    /// All of the decrypted data, which the permissive Flate decoder needs at once,
    /// each piece decrypted counted on `meter`.
    pub(crate) fn decrypt_all(&mut self, meter: &Meter<'_>) -> Result<Vec<u8>, Cancelled> {
        self.rewind();
        let mut data = Vec::new();
        let mut buf = [0; 8192];
        loop {
            let n = self.read(&mut buf);
            if n == 0 {
                return Ok(data);
            }
            meter.work(n)?;
            data.extend_from_slice(&buf[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CryptDictionary, DecryptorData};
    use super::*;

    /// An AES-256 decryptor whose crypt filter is `tag`.
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
                filters: None,
            },
        }
    }

    /// The reader yields what whole decryption returns, read in pieces of several
    /// sizes, after a rewind from several places.
    fn read_all(reader: &mut DecryptedReader<'_>, chunk: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = alloc::vec![0; chunk];
        loop {
            let n = reader.read(&mut buf);
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    /// Read after a prefix and a rewind, or continued from the cursor after the prefix
    /// in another reader, `data` reads as whole decryption returns it.
    fn check(decryptor: &Decryptor, data: &[u8]) {
        let id = ObjectIdentifier::new(1234, 17);
        let expected = decryptor.decrypt(id, data, DecryptionTarget::Stream);
        for chunk in [1, 7, 16, 19, 8192] {
            let Some(mut reader) = decryptor.reader(id, data, DecryptionTarget::Stream) else {
                assert!(expected.is_none());
                continue;
            };
            let expected = expected.as_deref().unwrap();
            for prefix in [0, 1, 15, 17, 37, data.len() + 1] {
                reader.rewind();
                #[cfg_attr(not(feature = "unsafe"), allow(unused))]
                let read = reader.read(&mut alloc::vec![0; prefix]);
                #[cfg(feature = "unsafe")]
                {
                    let mut resumed = decryptor
                        .reader(id, data, DecryptionTarget::Stream)
                        .unwrap();
                    resumed.restore(&reader.cursor());
                    assert_eq!(
                        read_all(&mut resumed, chunk),
                        expected[read..],
                        "resumed at {read}"
                    );
                }
                reader.rewind();
                assert_eq!(
                    read_all(&mut reader, chunk),
                    expected,
                    "chunk {chunk}, prefix {prefix}"
                );
            }
        }
    }

    #[test]
    fn every_cipher_reads_as_whole_decryption() {
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

    /// Padding is removed from the last whole block only when it is valid, and a
    /// trailing partial block is ignored, as whole decryption does.
    #[test]
    fn aes_padding_and_partial_blocks_read_as_whole_decryption() {
        let cipher = AES256Cipher::new(&[17; 32]).unwrap();
        let decryptor = decryptor(DecryptorTag::Aes256);
        for len in 0..32 {
            let mut data = alloc::vec![9; 16];
            data.extend(cipher.encrypt_cbc(&alloc::vec![42; len], &[9; 16]));
            check(&decryptor, &data);
            data.extend([1, 2, 3]);
            check(&decryptor, &data);
        }
    }
}
