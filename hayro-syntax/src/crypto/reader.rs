//! Restartable decryption with a borrowed ciphertext and bounded cipher state.

use super::aes::{AES128Cipher, AES256Cipher};
use super::rc4::Rc4;
use super::{DecryptionTarget, Decryptor, DecryptorTag, decrypt_rc_aes};
use crate::object::ObjectIdentifier;
use alloc::borrow::Cow;
use alloc::vec::Vec;

pub(crate) struct DecryptedReader<'a> {
    data: Cow<'a, [u8]>,
    state: DecryptedState,
}

/// Cipher cursor only: encoded bytes stay in the owning stream.
#[derive(Clone)]
pub(crate) struct DecryptedState {
    cipher: Cipher,
    pos: usize,
    block: [u8; 16],
    taken: usize,
    end: usize,
}

#[derive(Clone)]
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
        reader.state.cipher = cipher;
        reader.rewind();
        Some(reader)
    }
}

impl<'a> DecryptedReader<'a> {
    pub(crate) fn raw(data: Cow<'a, [u8]>) -> Self {
        Self {
            data,
            state: DecryptedState {
                cipher: Cipher::None,
                pos: 0,
                block: [0; 16],
                taken: 0,
                end: 0,
            },
        }
    }

    #[cfg(feature = "unsafe")]
    pub(crate) fn borrowed_input(&self) -> Option<&'a [u8]> {
        match &self.data {
            Cow::Borrowed(data) => Some(data),
            Cow::Owned(_) => None,
        }
    }

    #[cfg(feature = "unsafe")]
    pub(crate) fn checkpoint(&self) -> DecryptedState {
        self.state.clone()
    }

    #[cfg(feature = "unsafe")]
    pub(crate) fn restore(&mut self, state: &DecryptedState) {
        self.state = state.clone();
    }

    pub(crate) fn rewind(&mut self) {
        self.state.pos = match &mut self.state.cipher {
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
        self.state.taken = 0;
        self.state.end = 0;
    }

    pub(crate) fn read(&mut self, output: &mut [u8]) -> usize {
        match &mut self.state.cipher {
            Cipher::None | Cipher::Rc4 { .. } => {
                let n = output.len().min(self.data.len() - self.state.pos);
                output[..n].copy_from_slice(&self.data[self.state.pos..self.state.pos + n]);
                if let Cipher::Rc4 { current, .. } = &mut self.state.cipher {
                    current.decrypt_in_place(&mut output[..n]);
                }
                self.state.pos += n;
                n
            }
            Cipher::Aes128(_) | Cipher::Aes256(_) => {
                let mut written = 0;
                while written < output.len() {
                    if self.state.taken == self.state.end {
                        if self.data.len() - self.state.pos < 16 {
                            break;
                        }
                        let ciphertext = self.data[self.state.pos..self.state.pos + 16]
                            .try_into()
                            .unwrap();
                        self.state.block = match &self.state.cipher {
                            Cipher::Aes128(cipher) => cipher.decrypt_block(ciphertext),
                            Cipher::Aes256(cipher) => cipher.decrypt_block(ciphertext),
                            _ => unreachable!(),
                        };
                        for (plain, previous) in self
                            .state
                            .block
                            .iter_mut()
                            .zip(&self.data[self.state.pos - 16..self.state.pos])
                        {
                            *plain ^= previous;
                        }
                        self.state.pos += 16;
                        self.state.taken = 0;
                        self.state.end = 16;
                        // Match decrypt_cbc: ignore an incomplete trailing block and
                        // remove padding only if every byte of the last block agrees.
                        if self.data.len() - self.state.pos < 16 {
                            let padding = usize::from(self.state.block[15]);
                            if (1..=16).contains(&padding)
                                && self.state.block[16 - padding..]
                                    .iter()
                                    .all(|&b| usize::from(b) == padding)
                            {
                                self.state.end -= padding;
                            }
                        }
                    }
                    let n = (output.len() - written).min(self.state.end - self.state.taken);
                    output[written..written + n]
                        .copy_from_slice(&self.state.block[self.state.taken..self.state.taken + n]);
                    self.state.taken += n;
                    written += n;
                }
                written
            }
        }
    }

    /// The permissive Flate fallback needs the complete decrypted input.
    pub(crate) fn into_data(mut self) -> Cow<'a, [u8]> {
        if matches!(self.state.cipher, Cipher::None) {
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

    #[cfg(feature = "unsafe")]
    fn encrypt(decryptor: &Decryptor, plain: &[u8]) -> Vec<u8> {
        let reader = decryptor
            .reader(
                ObjectIdentifier::new(1234, 17),
                &[9; 16],
                DecryptionTarget::Stream,
            )
            .unwrap();
        let mut encrypted = alloc::vec![9; 16];
        match reader.state.cipher {
            Cipher::None => return plain.to_vec(),
            Cipher::Rc4 { mut current, .. } => return current.encrypt(plain),
            Cipher::Aes128(cipher) => encrypted.extend(cipher.encrypt_cbc(plain, &[9; 16])),
            Cipher::Aes256(cipher) => encrypted.extend(cipher.encrypt_cbc(plain, &[9; 16])),
        }
        encrypted
    }

    #[cfg(feature = "unsafe")]
    #[test]
    fn cipher_checkpoints_preserve_partial_blocks_and_final_padding() {
        for tag in [
            DecryptorTag::Rc4,
            DecryptorTag::Aes128,
            DecryptorTag::Aes256,
        ] {
            let decryptor = decryptor(tag);
            let plain: Vec<u8> = (0..73).collect();
            let encrypted = encrypt(&decryptor, &plain);
            let open = || {
                decryptor
                    .reader(
                        ObjectIdentifier::new(1234, 17),
                        &encrypted,
                        DecryptionTarget::Stream,
                    )
                    .unwrap()
            };
            for offset in [1, 7, 15, 17, 70, 73] {
                let mut reader = open();
                let mut prefix = alloc::vec![0; offset];
                assert_eq!(reader.read(&mut prefix), offset);
                let state = reader.checkpoint();
                if matches!(tag, DecryptorTag::Aes128 | DecryptorTag::Aes256) && offset < 70 {
                    assert!(state.taken > 0 && state.taken < state.end);
                }
                drop(reader);
                for _ in 0..2 {
                    let mut reopened = open();
                    reopened.restore(&state);
                    let mut actual = Vec::new();
                    let mut buf = [0; 7];
                    loop {
                        let n = reopened.read(&mut buf);
                        if n == 0 {
                            break;
                        }
                        actual.extend_from_slice(&buf[..n]);
                    }
                    assert_eq!(actual, plain[offset..]);
                    reopened.rewind();
                    let mut all = [0; 73];
                    assert_eq!(reopened.read(&mut all), 73);
                    assert_eq!(all.as_slice(), plain);
                }
            }
        }
    }

    #[cfg(feature = "unsafe")]
    #[test]
    fn encrypted_flate_checkpoints_resume_predictors_and_cipher_state() {
        use crate::filter::lzw_flate::{PredictorParams, apply_predictor};
        use crate::filter::reader::{DecodedReader, ReadError};
        use crate::object::Stream;
        use crate::reader::{Reader, ReaderExt};
        use std::io::Write;
        for tag in [
            DecryptorTag::Rc4,
            DecryptorTag::Aes128,
            DecryptorTag::Aes256,
        ] {
            for predictor in [1, 2, 10, 11, 12, 13, 14, 15] {
                let params = PredictorParams {
                    predictor,
                    columns: 263,
                    ..PredictorParams::default()
                };
                let mut plain: Vec<u8> = (0..264 * 200).map(|i| (i % 251) as u8).collect();
                if predictor >= 10 {
                    for (i, row) in plain.chunks_mut(264).enumerate() {
                        row[0] = (i % 5) as u8;
                    }
                }
                let expected = apply_predictor(plain.clone(), &params).unwrap();
                let mut encoder =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(&plain).unwrap();
                let zlib = encoder.finish().unwrap();
                let decryptor = decryptor(tag);
                for raw in [false, true] {
                    let encoded = if raw { &zlib[2..zlib.len() - 4] } else { &zlib };
                    let encrypted = encrypt(&decryptor, encoded);
                    let dict = Reader::new(b"/Filter /FlateDecode ID")
                        .read_without_context::<crate::object::dict::InlineImageDict<'_>>()
                        .unwrap();
                    let owner = Stream::new(&encrypted, dict.get_dict().clone()).to_owned_stream();
                    let stream = owner.get().unwrap();
                    let Cow::Borrowed(input) = stream.raw_data() else {
                        panic!("borrowed")
                    };
                    let open = || {
                        DecodedReader::flate(
                            decryptor
                                .reader(
                                    ObjectIdentifier::new(1234, 17),
                                    input,
                                    DecryptionTarget::Stream,
                                )
                                .unwrap(),
                            &params,
                        )
                        .unwrap()
                    };
                    let mut reader = open();
                    let mut prefix = alloc::vec![0; 12_345];
                    if raw {
                        assert_eq!(reader.read(&mut prefix), Err(ReadError::Restarted));
                    }
                    let mut offset = 0;
                    while offset < prefix.len() {
                        offset += reader.read(&mut prefix[offset..]).unwrap();
                    }
                    assert_eq!(prefix, expected[..prefix.len()]);
                    let checkpoint = reader
                        .checkpoint(&owner)
                        .unwrap()
                        .expect("encrypted Flate checkpoint");
                    assert_eq!(Some(checkpoint.allocation_size()), reader.checkpoint_size());
                    assert!(
                        checkpoint.allocation_size() <= reader.checkpoint_size_bound().unwrap()
                    );
                    drop(reader);
                    for chunk in [1, 397, 8193] {
                        let mut reopened = open();
                        reopened.restore(&checkpoint).unwrap();
                        assert_eq!(reopened.inflated_bytes(), 0);
                        for rewind in [false, true] {
                            if rewind {
                                reopened.rewind();
                            }
                            let mut actual = Vec::new();
                            let mut buf = alloc::vec![0; chunk];
                            loop {
                                let n = reopened.read(&mut buf).unwrap();
                                if n == 0 {
                                    break;
                                }
                                actual.extend_from_slice(&buf[..n]);
                            }
                            assert_eq!(actual, expected[if rewind { 0 } else { prefix.len() }..]);
                        }
                    }
                }
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
