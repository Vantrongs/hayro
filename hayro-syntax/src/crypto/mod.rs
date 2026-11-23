//! Cryptographic implementations for hayro, ported from pdf.js.
//!
//! **Important note**: Please keep in mind that these haven't been
//! audited and should not be used for security-critical purposes, like creating new
//! encrypted PDFs. They solely serve the purpose of being able to decrypt and read
//! _already_ encrypted documents, where security isn't really relevant.
//!
//! # Crypt filters
//!
//! Which crypt filter decrypts each string and stream of a document encrypted by the
//! standard security handler, by these rules (pinned by `tests/crypt_filters.rs`).
//!
//! 1. A document of `/V 1` or `/V 2` decrypts every string and stream with RC4 and the
//!    file key. It has no crypt filters: `/CF`, `/StmF` and `/StrF` are not read.
//! 2. In a document of `/V 4` or `/V 5`, `/StmF` names the crypt filter of its streams
//!    and `/StrF` that of its strings, each looked up among the entries of `/CF` by its
//!    name, whatever the name. A crypt filter's method is its `/CFM`: `/V2` is RC4,
//!    `/AESV2` is AES-128 and `/AESV3` AES-256; `/None` and `/Identity` are no
//!    encryption. Every method uses the file key the handler computes, whatever the
//!    crypt filter's `/Length`: RC4 and AES-128 with the key of the object holding the
//!    data (the PDF specification's algorithm 1), AES-256 with the file key itself. A
//!    method that cannot use the file key, AES-256 with a key of 128 bits, fails each
//!    stream it decrypts. A string that cannot be decrypted is read as stored.
//! 3. `/StmF` or `/StrF` leaves its streams or strings as stored where it names
//!    `/Identity` and `/CF` defines no crypt filter of that name, a name `/CF` does not
//!    define, an entry of `/CF` that is no dictionary or whose `/CFM` is missing,
//!    `/None`, `/Identity` or another name rule 2 does not know, or any name where
//!    `/CF` is no dictionary.
//! 4. A document of `/V 4` or `/V 5` whose `/StmF` or `/StrF` is missing or no name
//!    cannot be opened ([`DecryptionError::InvalidEncryption`]).
//! 5. A stream with a `/Crypt` filter is not decrypted by `/StmF`. A `/Crypt` filter
//!    whose parameters have no `/Name` that is a name, or whose `/Name` is `/Identity`,
//!    decrypts nothing, wherever it stands among the stream's filters.
//! 6. A `/Crypt` filter whose `/Name` is another name decrypts the stream with the
//!    crypt filter of `/CF` of that name, its method as in rule 2 (`/None` and
//!    `/Identity` being no encryption), where it is the first of the filters the stream
//!    names (an entry that names no filter does not count). It fails the stream for
//!    every reading ([`crate::object::stream::DecodeFailure::Decryption`]) where `/CF`
//!    does not define that name, where the entry is no dictionary or its `/CFM` is
//!    missing or of no method rule 2 knows, and where another filter comes before it,
//!    so that only the first `/Crypt` filter may name one. `/Name` and the parameters
//!    may be given by reference.
//! 7. In a document whose `/CF` is missing or no dictionary, every `/V` included, a
//!    `/Crypt` filter named `/StdCF` decrypts with RC4 and the file key, and one of any
//!    other name but `/Identity` fails its stream.
//! 8. In a document that is not encrypted, a `/Crypt` filter does nothing.
//! 9. A stream's filters ([`crate::object::Stream::filters`]) include its `/Crypt`
//!    filters, which decoding passes over. Its raw data
//!    ([`crate::object::Stream::try_raw_data`]) are decrypted by the `/Crypt` filter
//!    and not decoded, so that a writer copying them leaves every `/Crypt` filter out.
//!    The readers of decoded data a piece at a time
//!    ([`crate::object::Stream::decoded_reader`] and the strict one) pass over them too.
//! 10. A stream with the name `/XRef` as its type is read as stored.
//!     `/EncryptMetadata` false enters the file key only: the metadata stream is decrypted like any other unless its own
//!     `/Crypt` filter says otherwise. `/EFF` is not read: an embedded file is
//!     decrypted by `/StmF`. An inline image is never decrypted.
//! 11. A stream without data reads as empty. AES data are the IV and whole blocks:
//!     other data fail every strict reading and the raw data, while the lenient
//!     decoding decrypts the whole blocks after the IV and fails data shorter than the
//!     IV.

use crate::crypto::DecryptionError::InvalidEncryption;
use crate::crypto::aes::{AES128Cipher, AES256Cipher};
use crate::crypto::rc4::Rc4;
use crate::object;
use crate::object::dict::keys::{
    AESV2, AESV3, CF, CFM, ENCRYPT_META_DATA, FILTER, IDENTITY, LENGTH, NONE, O, OE, P, R, STD_CF,
    STM_F, STR_F, U, UE, V,
};
use crate::object::{Dict, Name, ObjectIdentifier};
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp;
use core::ops::Deref;

mod aes;
mod md5;
mod rc4;
pub(crate) mod reader;
mod sha256;
mod sha384;
mod sha512;

const PASSWORD_PADDING: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

/// An error that occurred during decryption.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DecryptionError {
    /// The ID entry is missing in the PDF.
    MissingIDEntry,
    /// No password or a wrong one was provided.
    PasswordProtected,
    /// The PDF has invalid encryption.
    InvalidEncryption,
    /// The PDF uses an unsupported encryption algorithm.
    UnsupportedAlgorithm,
}

impl core::fmt::Display for DecryptionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingIDEntry => write!(f, "missing ID entry"),
            Self::PasswordProtected => write!(f, "missing or incorrect password"),
            Self::InvalidEncryption => write!(f, "invalid encryption"),
            Self::UnsupportedAlgorithm => write!(f, "unsupported encryption algorithm"),
        }
    }
}

impl core::error::Error for DecryptionError {}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum DecryptorTag {
    None,
    Rc4,
    Aes128,
    Aes256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decryptor {
    None,
    /// A document of `/V 1` or `/V 2`: RC4 with the file key for every string and
    /// stream (rule 1).
    Rc4 {
        key: Vec<u8>,
    },
    /// A document of `/V 4` or `/V 5`, whose crypt filters choose the method
    /// (rules 2 to 7).
    Filters {
        key: Vec<u8>,
        filters: CryptFilters,
    },
}

/// The crypt filters of a document of `/V 4` or `/V 5`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CryptFilters {
    /// The method of `/StmF`, [`DecryptorTag::None`] where it names none (rule 3).
    stream: DecryptorTag,
    /// The method of `/StrF`, [`DecryptorTag::None`] where it names none (rule 3).
    string: DecryptorTag,
    /// The method of each crypt filter of `/CF` by name, `None` for one that names no
    /// method rule 2 knows; `None` where `/CF` is missing or no dictionary.
    defined: Option<BTreeMap<Vec<u8>, Option<DecryptorTag>>>,
}

impl CryptFilters {
    /// The crypt filters of encryption dictionary `dict`; [`InvalidEncryption`] where
    /// `/StmF` or `/StrF` is missing or no name (rule 4).
    fn read(dict: &Dict<'_>) -> Result<Self, DecryptionError> {
        let stream = dict.get::<Name<'_>>(STM_F).ok_or(InvalidEncryption)?;
        let string = dict.get::<Name<'_>>(STR_F).ok_or(InvalidEncryption)?;
        let defined = dict.get::<Dict<'_>>(CF).map(|cf| {
            cf.keys()
                .map(|name| {
                    let method = cf
                        .get::<Dict<'_>>(name.deref())
                        .and_then(|entry| method_of(&entry));
                    (name.to_vec(), method)
                })
                .collect::<BTreeMap<_, _>>()
        });
        let of_document = |name: &Name<'_>| {
            defined
                .as_ref()
                .and_then(|defined| *defined.get(name.deref())?)
                .unwrap_or(DecryptorTag::None)
        };
        Ok(Self {
            stream: of_document(&stream),
            string: of_document(&string),
            defined,
        })
    }

    /// The method of the crypt filter `name` names for a `/Crypt` filter, by rules 6
    /// and 7; `None` where it names none.
    fn named(&self, name: &[u8]) -> Option<DecryptorTag> {
        match &self.defined {
            None => (name == STD_CF).then_some(DecryptorTag::Rc4),
            Some(defined) => *defined.get(name)?,
        }
    }
}

/// The method of crypt filter dictionary `entry` by its `/CFM` (rule 2); `None` where
/// it is missing or names no method rule 2 knows.
fn method_of(entry: &Dict<'_>) -> Option<DecryptorTag> {
    Some(match entry.get::<Name<'_>>(CFM)?.deref() {
        NONE | IDENTITY => DecryptorTag::None,
        b"V2" => DecryptorTag::Rc4,
        AESV2 => DecryptorTag::Aes128,
        AESV3 => DecryptorTag::Aes256,
        _ => return None,
    })
}

#[derive(Debug, Copy, Clone)]
pub(crate) enum DecryptionTarget<'n> {
    String,
    Stream,
    /// A stream under a /Crypt filter that names one of the document's crypt
    /// filters, by that name.
    Named(&'n [u8]),
}

impl Decryptor {
    /// The key and method that decrypt `target`, by rules 2, 6 and 7 of the module
    /// doc; `None` where a `/Crypt` filter names no crypt filter of the document with a
    /// method.
    fn method(&self, target: DecryptionTarget<'_>) -> Option<(&[u8], DecryptorTag)> {
        match self {
            Self::None => Some((&[], DecryptorTag::None)),
            Self::Rc4 { key } => match target {
                DecryptionTarget::String | DecryptionTarget::Stream => {
                    Some((key, DecryptorTag::Rc4))
                }
                DecryptionTarget::Named(name) => {
                    (name == STD_CF).then_some((key.as_slice(), DecryptorTag::Rc4))
                }
            },
            Self::Filters { key, filters } => {
                let tag = match target {
                    DecryptionTarget::String => filters.string,
                    DecryptionTarget::Stream => filters.stream,
                    DecryptionTarget::Named(name) => filters.named(name)?,
                };
                Some((key, tag))
            }
        }
    }

    /// Whether `data` decrypts whole: AES needs its IV and whole blocks, and the
    /// decryption leaves a trailing partial block out. No data is whole. Data that
    /// do not decrypt at all, by no method or by one that cannot use the key, are not.
    pub(crate) fn decrypts_whole(&self, data: &[u8], target: DecryptionTarget<'_>) -> bool {
        let Some((key, tag)) = self.method(target) else {
            return false;
        };
        let blocks = || data.is_empty() || (data.len() >= 16 && data.len().is_multiple_of(16));
        match tag {
            DecryptorTag::None | DecryptorTag::Rc4 => true,
            DecryptorTag::Aes128 => blocks(),
            DecryptorTag::Aes256 => AES256Cipher::new(key).is_some() && blocks(),
        }
    }

    pub(crate) fn decrypt(
        &self,
        id: ObjectIdentifier,
        data: &[u8],
        target: DecryptionTarget<'_>,
    ) -> Option<Vec<u8>> {
        let (key, tag) = self.method(target)?;
        match tag {
            DecryptorTag::None => Some(data.to_vec()),
            DecryptorTag::Rc4 => decrypt_rc4(key, data, id),
            DecryptorTag::Aes128 => decrypt_aes128(key, data, id),
            DecryptorTag::Aes256 => decrypt_aes256(key, data),
        }
    }
}

pub(crate) fn get(
    dict: &Dict<'_>,
    id: &[u8],
    password: &[u8],
) -> Result<Decryptor, DecryptionError> {
    let filter = dict.get::<Name<'_>>(FILTER).ok_or(InvalidEncryption)?;

    if filter.deref() != b"Standard" {
        return Err(DecryptionError::UnsupportedAlgorithm);
    }

    let encryption_v = dict.get::<u8>(V).ok_or(InvalidEncryption)?;
    let encrypt_metadata = dict.get::<bool>(ENCRYPT_META_DATA).unwrap_or(true);
    let revision = dict.get::<u8>(R).ok_or(InvalidEncryption)?;
    let length = match encryption_v {
        1 => 40,
        2 => dict.get::<u16>(LENGTH).unwrap_or(40),
        4 => dict.get::<u16>(LENGTH).unwrap_or(128),
        5 => 256,
        _ => return Err(DecryptionError::UnsupportedAlgorithm),
    };

    // Rules 1 to 4 of the module doc: crypt filters from /V 4 on, read before the
    // password is checked, so that an invalid dictionary is reported as such.
    let filters = match encryption_v {
        1 | 2 => None,
        _ => Some(CryptFilters::read(dict)?),
    };

    let byte_length = length / 8;
    if byte_length == 0 {
        return Err(InvalidEncryption);
    }

    let owner_string = dict.get::<object::String<'_>>(O).ok_or(InvalidEncryption)?;
    let user_string = dict.get::<object::String<'_>>(U).ok_or(InvalidEncryption)?;
    let permissions = {
        let raw = dict.get::<i64>(P).ok_or(InvalidEncryption)?;

        if raw < 0 {
            u32::from_be_bytes((raw as i32).to_be_bytes())
        } else {
            raw as u32
        }
    };

    let mut decryption_key = if revision <= 4 {
        let key = decryption_key_rev1234(
            password,
            encrypt_metadata,
            revision,
            byte_length,
            &owner_string,
            permissions,
            id,
        )?;
        authenticate_user_password_rev234(revision, &key, id, &user_string)?;

        key
    } else {
        decryption_key_rev56(dict, revision, password, &owner_string, &user_string)?
    };

    // See pdf.js issue 19484.
    if encryption_v == 4 && decryption_key.len() < 16 {
        decryption_key.resize(16, 0);
    }

    Ok(match filters {
        None => Decryptor::Rc4 {
            key: decryption_key,
        },
        Some(filters) => Decryptor::Filters {
            key: decryption_key,
            filters,
        },
    })
}

fn decrypt_aes256(key: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let (iv, data) = data.split_at_checked(16)?;
    let iv: [u8; 16] = iv.try_into().ok()?;
    let cipher = AES256Cipher::new(key)?;
    Some(cipher.decrypt_cbc(data, &iv, true))
}

fn decrypt_aes128(key: &[u8], data: &[u8], id: ObjectIdentifier) -> Option<Vec<u8>> {
    decrypt_rc_aes(key, id, true, |key| {
        let cipher = AES128Cipher::new(key)?;
        let (iv, data) = data.split_at_checked(16)?;
        let iv: [u8; 16] = iv.try_into().ok()?;

        Some(cipher.decrypt_cbc(data, &iv, true))
    })
}

fn decrypt_rc4(key: &[u8], data: &[u8], id: ObjectIdentifier) -> Option<Vec<u8>> {
    decrypt_rc_aes(key, id, false, |key| {
        let mut rc = Rc4::new(key);
        Some(rc.decrypt(data))
    })
}

/// Algorithm 1: Encryption of data using the RC4 or AES algorithms
fn decrypt_rc_aes<T>(
    key: &[u8],
    id: ObjectIdentifier,
    aes: bool,
    with_key: impl FnOnce(&[u8]) -> Option<T>,
) -> Option<T> {
    let n = key.len();
    let mut key = key.to_vec();

    key.extend(&id.obj_number.to_le_bytes()[..3]);
    key.extend(&id.gen_number.to_le_bytes()[..2]);

    if aes {
        key.extend(b"sAlT");
    }

    let hash = md5::calculate(&key);

    let final_key = &hash[..cmp::min(16, n + 5)];

    with_key(final_key)
}

/// Algorithm 2.B: Computing a hash (revision 6 and later)
fn compute_hash_rev56(
    password: &[u8],
    validation_salt: &[u8],
    user_string: Option<&[u8]>,
    revision: u8,
) -> Result<[u8; 32], DecryptionError> {
    let mut k = {
        let mut input = Vec::new();
        input.extend_from_slice(password);
        input.extend_from_slice(validation_salt);

        if let Some(user_string) = user_string {
            input.extend_from_slice(user_string);
        }

        let hash = sha256::calculate(&input);

        // Revision 5 only uses the initial hash.
        if revision == 5 {
            return Ok(hash);
        }

        hash.to_vec()
    };

    let mut round: u16 = 0;

    loop {
        let k1 = {
            let mut single: Vec<u8> = vec![];
            single.extend(password);
            single.extend(&k);

            if let Some(user_string) = user_string {
                single.extend(user_string);
            }

            single.repeat(64)
        };

        let e = {
            let aes = AES128Cipher::new(&k[..16]).ok_or(InvalidEncryption)?;
            let mut res = aes.encrypt_cbc(&k1, &k[16..32].try_into().unwrap());

            res.truncate(k1.len());

            res
        };

        let num = u128::from_be_bytes(e[..16].try_into().unwrap()) % 3;

        k = match num {
            0 => sha256::calculate(&e).to_vec(),
            1 => sha384::calculate(&e).to_vec(),
            2 => sha512::calculate(&e).to_vec(),
            _ => unreachable!(),
        };

        round += 1;

        if round > 63 {
            let last_byte = *e.last().unwrap();

            if (last_byte as u16) <= round - 32 {
                break;
            }
        }
    }

    let mut result = [0_u8; 32];
    result.copy_from_slice(&k[..32]);
    Ok(result)
}

fn decryption_key_rev1234(
    password: &[u8],
    encrypt_metadata: bool,
    revision: u8,
    byte_length: u16,
    owner_string: &object::String<'_>,
    permissions: u32,
    id: &[u8],
) -> Result<Vec<u8>, DecryptionError> {
    let mut md5_input = vec![];

    // TODO: Convert to PDFDocEncoding.
    let mut padded_password = [0_u8; 32];
    let copy_len = password.len().min(32);
    padded_password[..copy_len].copy_from_slice(&password[..copy_len]);
    if copy_len < 32 {
        padded_password[copy_len..].copy_from_slice(&PASSWORD_PADDING[..(32 - copy_len)]);
    }

    md5_input.extend(&padded_password);

    md5_input.extend(owner_string.as_ref());

    md5_input.extend(permissions.to_le_bytes());

    md5_input.extend(id);

    if !encrypt_metadata && revision >= 4 {
        md5_input.extend(&[0xff, 0xff, 0xff, 0xff]);
    }

    let mut hash = md5::calculate(&md5_input);

    if byte_length as usize > hash.len() {
        return Err(InvalidEncryption);
    }

    if revision >= 3 {
        for _ in 0..50 {
            hash = md5::calculate(&hash[..byte_length as usize]);
        }
    }

    let decryption_key = hash[..byte_length as usize].to_vec();
    Ok(decryption_key)
}

fn authenticate_user_password_rev234(
    revision: u8,
    decryption_key: &[u8],
    id: &[u8],
    user_string: &object::String<'_>,
) -> Result<(), DecryptionError> {
    let result = match revision {
        2 => user_password_rev2(decryption_key),
        3 | 4 => user_password_rev34(decryption_key, id),
        _ => return Err(InvalidEncryption),
    };

    match revision {
        2 => {
            if result.as_slice() != user_string.as_ref() {
                return Err(DecryptionError::PasswordProtected);
            }
        }
        3 | 4 => {
            if Some(&result[..16]) != user_string.as_ref().get(0..16) {
                return Err(DecryptionError::PasswordProtected);
            }
        }
        _ => unreachable!(),
    }

    Ok(())
}

fn user_password_rev2(decryption_key: &[u8]) -> Vec<u8> {
    let mut rc = Rc4::new(decryption_key);
    rc.decrypt(&PASSWORD_PADDING)
}

fn user_password_rev34(decryption_key: &[u8], id: &[u8]) -> Vec<u8> {
    let mut rc = Rc4::new(decryption_key);

    let mut input = vec![];
    input.extend(PASSWORD_PADDING);

    input.extend(id);
    let hash = md5::calculate(&input);

    let mut encrypted = rc.encrypt(&hash);

    for i in 1..=19 {
        let mut key = decryption_key.to_vec();
        for byte in &mut key {
            *byte ^= i;
        }

        let mut rc = Rc4::new(&key);
        encrypted = rc.encrypt(&encrypted);
    }

    encrypted.resize(32, 0);
    encrypted
}

fn decryption_key_rev56(
    dict: &Dict<'_>,
    revision: u8,
    password: &[u8],
    owner_string: &object::String<'_>,
    user_string: &object::String<'_>,
) -> Result<Vec<u8>, DecryptionError> {
    // TODO: Apply SASLprep.
    let password = &password[..password.len().min(127)];

    let string_len = if revision <= 4 { 32 } else { 48 };

    let trimmed_os = owner_string.get(..string_len).ok_or(InvalidEncryption)?;

    let (owner_hash, owner_tail) = trimmed_os.split_at_checked(32).ok_or(InvalidEncryption)?;
    let (owner_validation_salt, owner_key_salt) =
        owner_tail.split_at_checked(8).ok_or(InvalidEncryption)?;

    let trimmed_us = user_string.get(..string_len).ok_or(InvalidEncryption)?;
    let (user_hash, user_tail) = trimmed_us.split_at_checked(32).ok_or(InvalidEncryption)?;
    let (user_validation_salt, user_key_salt) =
        user_tail.split_at_checked(8).ok_or(InvalidEncryption)?;

    if compute_hash_rev56(password, owner_validation_salt, Some(trimmed_us), revision)?
        == owner_hash
    {
        let intermediate_owner_key =
            compute_hash_rev56(password, owner_key_salt, Some(trimmed_us), revision)?;

        let oe_string = dict
            .get::<object::String<'_>>(OE)
            .ok_or(InvalidEncryption)?;

        if oe_string.len() != 32 {
            return Err(InvalidEncryption);
        }

        let cipher = AES256Cipher::new(&intermediate_owner_key).ok_or(InvalidEncryption)?;
        let zero_iv = [0_u8; 16];

        Ok(cipher.decrypt_cbc(&oe_string, &zero_iv, false))
    } else if compute_hash_rev56(password, user_validation_salt, None, revision)? == user_hash {
        let intermediate_key = compute_hash_rev56(password, user_key_salt, None, revision)?;

        let ue_string = dict
            .get::<object::String<'_>>(UE)
            .ok_or(InvalidEncryption)?;

        if ue_string.len() != 32 {
            return Err(InvalidEncryption);
        }

        let cipher = AES256Cipher::new(&intermediate_key).ok_or(InvalidEncryption)?;
        let zero_iv = [0_u8; 16];

        Ok(cipher.decrypt_cbc(&ue_string, &zero_iv, false))
    } else {
        Err(DecryptionError::PasswordProtected)
    }

    // TODO: Verify Perms.
}

/// Encrypted test files.
#[cfg(test)]
pub(crate) mod fixture {
    use super::aes::AES128Cipher;
    use super::rc4::Rc4;
    use super::{PASSWORD_PADDING, md5};
    use alloc::format;
    use alloc::vec::Vec;

    /// An object of a test file.
    pub(crate) enum Part<'a> {
        /// An object without strings, written as it is.
        Plain(&'a str),
        /// A stream with `dict`'s entries and `data`, encrypted unless `encrypt` is false.
        Stream {
            dict: &'a str,
            data: &'a [u8],
            encrypt: bool,
        },
    }

    const ID: [u8; 16] = *b"hayro-test-file!";
    const PERMISSIONS: i32 = -4;

    fn rc4_rounds(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = Rc4::new(key).encrypt(data);
        for i in 1..=19_u8 {
            let key: Vec<u8> = key.iter().map(|b| b ^ i).collect();
            out = Rc4::new(&key).encrypt(&out);
        }
        out
    }

    /// A file encrypted with AES-128 (/V 4 /R 4) and the empty user and owner
    /// password, whose objects are numbered from 1, the first being the catalog.
    pub(crate) fn aes128(objects: &[Part<'_>]) -> Vec<u8> {
        // Algorithm 3: /O from the empty owner password.
        let mut hash = md5::calculate(&PASSWORD_PADDING);
        for _ in 0..50 {
            hash = md5::calculate(&hash);
        }
        let owner = rc4_rounds(&hash, &PASSWORD_PADDING);
        // Algorithm 2: the file key.
        let mut input = PASSWORD_PADDING.to_vec();
        input.extend(&owner);
        input.extend(PERMISSIONS.to_le_bytes());
        input.extend(ID);
        let mut key = md5::calculate(&input);
        for _ in 0..50 {
            key = md5::calculate(&key);
        }
        // Algorithm 5: /U.
        let mut input = PASSWORD_PADDING.to_vec();
        input.extend(ID);
        let mut user = rc4_rounds(&key, &md5::calculate(&input));
        user.extend([0; 16]);

        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>();
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, part) in objects.iter().enumerate() {
            let number = i + 1;
            offsets.push(out.len());
            out.extend(format!("{number} 0 obj\n").bytes());
            match part {
                Part::Plain(object) => out.extend(object.bytes()),
                Part::Stream {
                    dict,
                    data,
                    encrypt,
                } => {
                    let data = if *encrypt {
                        let mut input = key.to_vec();
                        input.extend(&(number as u32).to_le_bytes()[..3]);
                        input.extend([0, 0]);
                        input.extend(b"sAlT");
                        let object_key = md5::calculate(&input);
                        let iv = [7; 16];
                        let mut encrypted = iv.to_vec();
                        encrypted.extend(
                            AES128Cipher::new(&object_key)
                                .unwrap()
                                .encrypt_cbc(data, &iv),
                        );
                        encrypted
                    } else {
                        data.to_vec()
                    };
                    out.extend(format!("<< {dict} /Length {} >>\nstream\n", data.len()).bytes());
                    out.extend(data);
                    out.extend(b"\nendstream");
                }
            }
            out.extend(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for offset in offsets {
            out.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        let id = hex(&ID).concat();
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R /ID [<{id}> <{id}>] /Encrypt << /Filter \
                 /Standard /V 4 /R 4 /Length 128 /CF << /StdCF << /CFM /AESV2 /AuthEvent \
                 /DocOpen /Length 16 >> >> /StmF /StdCF /StrF /StdCF /P {PERMISSIONS} /O <{}> \
                 /U <{}> >> >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1,
                hex(&owner).concat(),
                hex(&user).concat(),
            )
            .bytes(),
        );
        out
    }
}
