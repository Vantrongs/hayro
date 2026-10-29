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
#[derive(Clone)]
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
    /// at the end. A trailing partial block is ignored, as `decrypt_cbc` does, and so
    /// is a last block of padding alone: [`DecryptedReader::at`] counts a block taken
    /// as bytes read.
    fn next_block(&mut self, data: &[u8], pos: &mut usize) -> bool {
        let Some(block) = data.get(*pos..*pos + 16) else {
            return false;
        };
        let previous = &data[*pos - 16..*pos];
        let block = self
            .aes
            .decrypt_cbc_block(block.try_into().unwrap(), previous.try_into().unwrap());
        let mut end = 16;
        if data.len() - (*pos + 16) < 16 {
            end -= padding_len(&block);
        }
        if end == 0 {
            return false;
        }
        (self.block, self.taken, self.end) = (block, 0, end);
        *pos += 16;
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

    /// The stored bytes, when they need no decryption and are borrowed from the
    /// document rather than owned.
    #[cfg(feature = "images")]
    pub(crate) fn plain_borrowed(&self) -> Option<&'a [u8]> {
        self.plain()?;
        self.borrowed_data()
    }

    /// The stored bytes, when they are borrowed from the document rather than owned.
    #[cfg(any(feature = "unsafe", feature = "images"))]
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

    /// Moves past the next `n` decrypted bytes without copying them; returns how many
    /// the data held, fewer only at the end. Data stored as it is and AES data are
    /// addressed directly (an AES block decrypts from the stored block before it); RC4,
    /// a running keystream, makes the keystream of the bytes moved past, counted on
    /// `meter` a piece at a time.
    pub(crate) fn skip(&mut self, n: usize, meter: &Meter<'_>) -> Result<usize, Cancelled> {
        let at = self.at();
        let target = at.saturating_add(n).min(self.len());
        let Cursor { cipher, pos } = &mut self.cursor;
        match cipher {
            Cipher::None => *pos = target,
            Cipher::Rc4 { current, .. } => {
                while *pos < target {
                    let k = (target - *pos).min(crate::work::POLL_BYTES);
                    current.skip(k);
                    *pos += k;
                    meter.work(k)?;
                }
            }
            Cipher::Aes(blocks) => {
                // The block holding `target`, after the IV.
                *pos = 16 + target / 16 * 16;
                blocks.taken = 0;
                blocks.end = 0;
                if !target.is_multiple_of(16) && blocks.next_block(&self.data, pos) {
                    blocks.taken = target % 16;
                }
            }
        }
        Ok(target - at)
    }

    /// The decrypted bytes read.
    fn at(&self) -> usize {
        let Cursor { cipher, pos } = &self.cursor;
        match cipher {
            Cipher::None | Cipher::Rc4 { .. } => *pos,
            // Before the first block, the IV; past a block handed out, the block.
            Cipher::Aes(blocks) if blocks.end == 0 => *pos - 16,
            Cipher::Aes(blocks) => *pos - 32 + blocks.taken,
        }
    }

    /// Whether the data is decrypted with RC4, whose keystream runs from the start.
    #[cfg(feature = "unsafe")]
    pub(crate) fn is_rc4(&self) -> bool {
        matches!(self.cursor.cipher, Cipher::Rc4 { .. })
    }

    /// The bytes the data decrypts to: for AES, the whole blocks after the IV without
    /// the last one's padding, which is decrypted to find it.
    pub(crate) fn len(&self) -> usize {
        let Cipher::Aes(blocks) = &self.cursor.cipher else {
            return self.data.len();
        };
        // `Aes::blocks` takes data of an IV at least.
        let whole = (self.data.len() - 16) / 16;
        if whole == 0 {
            return 0;
        }
        let at = 16 * whole;
        let last = blocks.aes.decrypt_cbc_block(
            self.data[at..at + 16].try_into().unwrap(),
            self.data[at - 16..at].try_into().unwrap(),
        );
        at - padding_len(&last)
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

/// The decrypted bytes between two RC4 states a [`Seekable`] keeps.
#[cfg(feature = "images")]
const RC4_MARK: usize = 64 * 1024;

/// A [`DecryptedReader`] that also goes back, at a cost that does not grow with how
/// far into the data it goes back to: AES in CBC mode decrypts any block from the
/// stored block before it, and RC4 resumes from the state it had at the last multiple
/// of [`RC4_MARK`] decrypted bytes before, which it keeps for each it reached (258
/// bytes each, about 0.4% of the bytes read).
#[cfg(feature = "images")]
#[derive(Clone)]
pub(crate) struct Seekable<'a> {
    reader: DecryptedReader<'a>,
    /// The decrypted bytes before the next read.
    at: usize,
    /// For RC4, its state at each multiple of [`RC4_MARK`] decrypted bytes reached.
    marks: Vec<Rc4>,
}

#[cfg(feature = "images")]
impl<'a> Seekable<'a> {
    pub(crate) fn new(mut reader: DecryptedReader<'a>) -> Self {
        reader.rewind();
        let marks = match &reader.cursor.cipher {
            Cipher::Rc4 { current, .. } => alloc::vec![current.clone()],
            _ => Vec::new(),
        };
        Self {
            reader,
            at: 0,
            marks,
        }
    }

    /// Fills `out` with the next decrypted bytes, or as many as reach the next
    /// multiple of [`RC4_MARK`] for RC4, and returns how many; 0 only at the end.
    pub(crate) fn read(&mut self, out: &mut [u8]) -> usize {
        let rc4 = matches!(self.reader.cursor.cipher, Cipher::Rc4 { .. });
        let len = match rc4 {
            true => out.len().min(RC4_MARK - self.at % RC4_MARK),
            false => out.len(),
        };
        let n = self.reader.read(&mut out[..len]);
        self.at += n;
        if let Cipher::Rc4 { current, .. } = &self.reader.cursor.cipher
            && self.at.is_multiple_of(RC4_MARK)
            && self.marks.len() == self.at / RC4_MARK
        {
            self.marks.push(current.clone());
        }
        n
    }

    /// Goes back to decrypted byte `to`, at most where the next read starts, or to
    /// fewer than [`RC4_MARK`] bytes before it; returns where the next read starts.
    pub(crate) fn back_to(&mut self, to: usize) -> usize {
        let to = to.min(self.at);
        let cursor = &mut self.reader.cursor;
        self.at = match &mut cursor.cipher {
            Cipher::None => to,
            Cipher::Rc4 { current, .. } => {
                // Each multiple reached has its state, the start's included.
                let mark = to / RC4_MARK;
                current.clone_from(&self.marks[mark]);
                mark * RC4_MARK
            }
            Cipher::Aes(blocks) => {
                blocks.taken = 0;
                blocks.end = 0;
                to / 16 * 16
            }
        };
        // The stored bytes before the decrypted ones: an AES IV.
        cursor.pos = self.at + usize::from(matches!(cursor.cipher, Cipher::Aes(_))) * 16;
        self.at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decryptor of a document of `/V 5` (AES-256) whose file key is 32 bytes of
    /// 17, without `/CF`, whose stream filter and string filter both have method
    /// `tag` and a length of 32 bytes.
    fn decryptor(tag: DecryptorTag) -> Decryptor {
        Decryptor::Filters {
            key: alloc::vec![17; 32],
            filters: super::super::CryptFilters {
                stream: tag,
                string: tag,
                defined: None,
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
            #[cfg(feature = "images")]
            assert_eq!(reader.len(), expected.len());
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

    /// Gone back to any position read before, a [`Seekable`] reads on as whole
    /// decryption returns the data, having gone back fewer than [`RC4_MARK`] bytes
    /// before that position for RC4, and fewer than an AES block for AES.
    #[cfg(feature = "images")]
    #[test]
    fn going_back_reads_as_whole_decryption_from_near_the_position() {
        use super::{RC4_MARK, Seekable};
        let id = ObjectIdentifier::new(1234, 17);
        let data: Vec<u8> = (0..3 * RC4_MARK + 1000).map(|i| (i % 253) as u8).collect();
        for (decryptor, near) in [
            (Decryptor::None, 1),
            (
                Decryptor::Rc4 {
                    key: alloc::vec![13; 5],
                },
                RC4_MARK,
            ),
            (decryptor(DecryptorTag::Rc4), RC4_MARK),
            (decryptor(DecryptorTag::Aes128), 16),
            (decryptor(DecryptorTag::Aes256), 16),
        ] {
            let expected = decryptor
                .decrypt(id, &data, DecryptionTarget::Stream)
                .unwrap();
            let mut reader = Seekable::new(
                decryptor
                    .reader(id, &data, DecryptionTarget::Stream)
                    .unwrap(),
            );
            assert_eq!(reader.reader.len(), expected.len());
            let mut buf = alloc::vec![0; 7000];
            // Read through the data first, so that RC4 marks every multiple of it.
            while reader.read(&mut buf) > 0 {}
            let len = expected.len();
            for to in [
                len,
                len - 1,
                0,
                1,
                RC4_MARK,
                RC4_MARK - 1,
                2 * RC4_MARK + 17,
                16,
                15,
                33,
            ] {
                let at = reader.back_to(to);
                assert!(at <= to && to - at < near, "back to {to}: at {at}");
                let mut read = Vec::new();
                loop {
                    let n = reader.read(&mut buf);
                    if n == 0 {
                        break;
                    }
                    read.extend_from_slice(&buf[..n]);
                }
                assert_eq!(read, expected[at..], "back to {to}");
            }
        }
    }

    /// Moved past any number of bytes from any position, a reader reads on as whole
    /// decryption returns the data, having moved past as many as the data held.
    #[test]
    fn skipping_reads_on_as_whole_decryption() {
        let id = ObjectIdentifier::new(1234, 17);
        for decryptor in [
            Decryptor::None,
            Decryptor::Rc4 {
                key: alloc::vec![13; 5],
            },
            decryptor(DecryptorTag::Aes128),
            decryptor(DecryptorTag::Aes256),
        ] {
            for len in [0, 1, 15, 16, 17, 31, 32, 33, 100, 5000] {
                let data: Vec<u8> = (0..len).map(|i| (i * 7 % 253) as u8).collect();
                // AES data of less than an IV decrypts to nothing.
                let (Some(expected), Some(mut reader)) = (
                    decryptor.decrypt(id, &data, DecryptionTarget::Stream),
                    decryptor.reader(id, &data, DecryptionTarget::Stream),
                ) else {
                    continue;
                };
                for prefix in [0, 1, 15, 16, 17, 40, 4999] {
                    for n in [0, 1, 15, 16, 17, 33, 4096, usize::MAX] {
                        reader.rewind();
                        let read = reader.read(&mut alloc::vec![0; prefix]);
                        let skipped = reader.skip(n, &Meter::default()).unwrap();
                        assert_eq!(skipped, n.min(expected.len() - read), "{len} {prefix} {n}");
                        assert_eq!(
                            read_all(&mut reader, 7),
                            expected[read + skipped..],
                            "{len}: {prefix} read, {n} skipped"
                        );
                    }
                }
            }
        }
    }

    /// Data whose last block is all padding, read to its end or moved past to it,
    /// stays at its end: moving past more moves past nothing.
    #[test]
    fn a_last_block_of_padding_alone_ends_the_data() {
        let id = ObjectIdentifier::new(1234, 17);
        let cipher = AES256Cipher::new(&[17; 32]).unwrap();
        let decryptor = decryptor(DecryptorTag::Aes256);
        for len in [0, 16, 32] {
            let mut data = alloc::vec![9; 16];
            data.extend(cipher.encrypt_cbc(&alloc::vec![42; len], &[9; 16]));
            assert_eq!(data.len(), 16 + len + 16, "a block of padding alone");
            let mut reader = decryptor
                .reader(id, &data, DecryptionTarget::Stream)
                .unwrap();
            assert_eq!(reader.len(), len);
            for skip_first in [false, true] {
                reader.rewind();
                match skip_first {
                    true => assert_eq!(reader.skip(len, &Meter::default()), Ok(len)),
                    false => assert_eq!(read_all(&mut reader, 7), alloc::vec![42; len]),
                }
                assert_eq!(reader.read(&mut [0; 4]), 0, "{len}");
                assert_eq!(reader.skip(0, &Meter::default()), Ok(0), "{len}");
                assert_eq!(reader.skip(1, &Meter::default()), Ok(0), "{len}");
                assert_eq!(reader.read(&mut [0; 4]), 0, "{len}");
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
