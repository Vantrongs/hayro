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
//! 10. Every stream of an encrypted document read as an object is decrypted, except one
//!     whose `/Type` is the string `(XRef)`: a cross-reference stream, whose `/Type` is
//!     the name `/XRef`, is read as stored when the document's table is read from it,
//!     and decrypted when it is read as an object. `/EncryptMetadata` false enters the
//!     file key only: the metadata stream is decrypted like any other unless its own
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
    /// The PDF is password-protected and no password (or a wrong one) was
    /// provided.
    PasswordProtected,
    /// The PDF has invalid encryption.
    InvalidEncryption,
    /// The PDF uses an unsupported encryption algorithm.
    UnsupportedAlgorithm,
}

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

/// Algorithm 1.A: Encryption of data using the AES algorithms
fn decrypt_aes256(key: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    // a) Use the 32-byte file encryption key for the AES-256 symmetric key algorithm,
    // along with the string or stream data to be encrypted.
    // Use the AES algorithm in Cipher Block Chaining (CBC) mode, which requires an initialization
    // vector. The block size parameter is set to 16 bytes, and the initialization
    // vector is a 16-byte random number that is stored as the first 16 bytes of the
    // encrypted stream or string.
    let (iv, data) = data.split_at_checked(16)?;
    let iv: [u8; 16] = iv.try_into().ok()?;
    let cipher = AES256Cipher::new(key)?;
    Some(cipher.decrypt_cbc(data, &iv, true))
}

fn decrypt_aes128(key: &[u8], data: &[u8], id: ObjectIdentifier) -> Option<Vec<u8>> {
    decrypt_rc_aes(key, id, true, |key| {
        // If using the AES algorithm, the Cipher Block Chaining (CBC) mode, which requires an initialization
        // vector, is used. The block size parameter is set to 16 bytes, and the initialization vector is a 16-byte
        // random number that is stored as the first 16 bytes of the encrypted stream or string.
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
    // a) Obtain the object number and generation number from the object identifier of
    // the string or stream to be encrypted (see 7.3.10, "Indirect objects"). If the
    // string is a direct object, use the identifier of the indirect object containing
    // it.
    let mut key = key.to_vec();

    // b) For all strings and streams without crypt filter specifier; treating the
    // object number and generation number as binary integers, extend the original
    // n-byte file encryption key to n + 5 bytes by appending the low-order 3 bytes of
    // the object number and the low-order 2 bytes of the generation number in that
    // order, low-order byte first.
    key.extend(&id.obj_number.to_le_bytes()[..3]);
    key.extend(&id.gen_number.to_le_bytes()[..2]);

    // If using the AES algorithm, extend the file encryption key an additional 4 bytes by adding the value
    // "sAlT", which corresponds to the hexadecimal values 0x73, 0x41, 0x6C, 0x54. (This addition is done
    // for backward compatibility and is not intended to provide additional security.)
    if aes {
        key.extend(b"sAlT");
    }

    // c) Initialise the MD5 hash function and pass the result of step (b) as input
    // to this function.
    let hash = md5::calculate(&key);

    // d) Use the first (n + 5) bytes, up to a maximum of 16, of the output
    // from the MD5 hash as the key for the RC4 or AES symmetric key algorithms,
    // along with the string or stream data to be encrypted.
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
    // Take the SHA-256 hash of the original input to the algorithm and name the resulting
    // 32 bytes, K.
    let mut k = {
        let mut input = Vec::new();
        input.extend_from_slice(password);
        input.extend_from_slice(validation_salt);

        if let Some(user_string) = user_string {
            input.extend_from_slice(user_string);
        }

        let hash = sha256::calculate(&input);

        // Apparently revision 5 only uses this hash.
        if revision == 5 {
            return Ok(hash);
        }

        hash.to_vec()
    };

    let mut round: u16 = 0;

    // Perform the following steps (a)-(d) 64 times:
    loop {
        // a) Make a new string, K1, consisting of 64 repetitions of the sequence:
        // input password, K, the 48-byte user key. The 48 byte user key is only used when
        // checking the owner password or creating the owner key. If checking the user
        // password or creating the user key, K1 is the concatenation of the input
        // password and K.
        let k1 = {
            let mut single: Vec<u8> = vec![];
            single.extend(password);
            single.extend(&k);

            if let Some(user_string) = user_string {
                single.extend(user_string);
            }

            single.repeat(64)
        };

        // b) Encrypt K1 with the AES-128 (CBC, no padding) algorithm,
        // using the first 16 bytes of K as the key and the second 16 bytes of K as the
        // initialization vector. The result of this encryption is E.
        let e = {
            let aes = AES128Cipher::new(&k[..16]).ok_or(InvalidEncryption)?;
            let mut res = aes.encrypt_cbc(&k1, &k[16..32].try_into().unwrap());

            // Remove padding that was added by `encrypt_cbc`.
            res.truncate(k1.len());

            res
        };

        // c) Taking the first 16 bytes of E as an unsigned big-endian integer,
        // compute the remainder, modulo 3. If the result is 0, the next hash used is
        // SHA-256, if the result is 1, the next hash used is SHA-384, if the result is
        // 2, the next hash used is SHA-512.
        let num = u128::from_be_bytes(e[..16].try_into().unwrap()) % 3;

        // d) Using the hash algorithm determined in step c, take the hash of E.
        // The result is a new value of K, which will be 32, 48, or 64 bytes in length.
        k = match num {
            0 => sha256::calculate(&e).to_vec(),
            1 => sha384::calculate(&e).to_vec(),
            2 => sha512::calculate(&e).to_vec(),
            _ => unreachable!(),
        };

        round += 1;

        // Repeat the process (a-d) with this new value for K. Following 64 rounds
        // (round number 0 to round number 63), do the following, starting with round
        // number 64:
        if round > 63 {
            // e) Look at the very last byte of E. If the value of that byte
            // (taken as an unsigned integer) is greater than the round number - 32,
            // repeat steps (a-d) again.
            let last_byte = *e.last().unwrap();

            // f) Repeat from steps (a-e) until the value of the last byte
            // is < (round number) - 32.
            // For some reason we need to use <= here?
            if (last_byte as u16) <= round - 32 {
                break;
            }
        }
    }

    // The first 32 bytes of the final K are the output of the algorithm.
    let mut result = [0_u8; 32];
    result.copy_from_slice(&k[..32]);
    Ok(result)
}

/// Algorithm 2: Computing a file encryption key in order to encrypt a document (revision 4 and earlier)
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
    // a) Pad or truncate password to 32 bytes using PASSWORD_PADDING.
    let mut padded_password = [0_u8; 32];
    let copy_len = password.len().min(32);
    padded_password[..copy_len].copy_from_slice(&password[..copy_len]);
    if copy_len < 32 {
        padded_password[copy_len..].copy_from_slice(&PASSWORD_PADDING[..(32 - copy_len)]);
    }

    // b) Initialise the MD5 hash function and pass the
    // result of step a) as input to this function.
    md5_input.extend(&padded_password);

    // c) Pass the value of the encryption dictionary's O entry
    // to the MD5 hash function.
    md5_input.extend(owner_string.as_ref());

    // d) Convert the integer value of the P entry to a 32-bit unsigned
    // binary number and pass these bytes to the MD5 hash function, low-order byte first.
    md5_input.extend(permissions.to_le_bytes());

    // e) Pass the first element of the file's file identifier array to the MD5 hash function.
    md5_input.extend(id);

    // f) (Security handlers of revision 4 or greater) If document metadata
    // is not being encrypted, pass 4 bytes with the value 0xFFFFFFFF to the MD5 hash function.
    if !encrypt_metadata && revision >= 4 {
        md5_input.extend(&[0xff, 0xff, 0xff, 0xff]);
    }

    // g) Finish the hash.
    let mut hash = md5::calculate(&md5_input);

    if byte_length as usize > hash.len() {
        return Err(InvalidEncryption);
    }

    // h) For revisions >= 3, do the following 50 times: Take the output from the previous
    // MD5 hash and pass the first n bytes of the output as input into a new MD5 hash,
    // where n is the number of bytes of the file encryption key as defined by the value
    // of the encryption dictionary's `Length` entry.
    if revision >= 3 {
        for _ in 0..50 {
            hash = md5::calculate(&hash[..byte_length as usize]);
        }
    }

    let decryption_key = hash[..byte_length as usize].to_vec();
    Ok(decryption_key)
}

/// Algorithm 6: Authenticating the user password
fn authenticate_user_password_rev234(
    revision: u8,
    decryption_key: &[u8],
    id: &[u8],
    user_string: &object::String<'_>,
) -> Result<(), DecryptionError> {
    // a) Perform all but the last step of Algorithm 4 (revision 2) or Algorithm 5 (revision 3 + 4).
    let result = match revision {
        2 => user_password_rev2(decryption_key),
        3 | 4 => user_password_rev34(decryption_key, id),
        _ => return Err(InvalidEncryption),
    };

    // b) If the result of step (a) is equal to the value of the encryption dictionary's
    // U entry (comparing on the first 16 bytes in the case of security handlers of
    // revision 3 or greater), the password supplied is the correct user password.
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

/// Algorithm 4: Computing the encryption dictionary’s U-entry value
/// (Security handlers of revision 2).
fn user_password_rev2(decryption_key: &[u8]) -> Vec<u8> {
    // a) Create a file encryption key based on the user password string.
    // b) Encrypt the 32-byte padding string using an RC4 encryption
    // function with the file encryption key from the preceding step.
    let mut rc = Rc4::new(decryption_key);
    rc.decrypt(&PASSWORD_PADDING)
}

/// Algorithm 5: Computing the encryption dictionary’s U (user password)
/// value (Security handlers of revision 3 or 4).
fn user_password_rev34(decryption_key: &[u8], id: &[u8]) -> Vec<u8> {
    // a) Create a file encryption key based on the user password string.
    let mut rc = Rc4::new(decryption_key);

    let mut input = vec![];
    // b) Initialise the MD5 hash function and pass the 32-byte padding string.
    input.extend(PASSWORD_PADDING);

    // c) Pass the first element of the file's file identifier array to the hash function
    // and finish the hash.
    input.extend(id);
    let hash = md5::calculate(&input);

    // d) Encrypt the 16-byte result of the hash, using an RC4 encryption function with
    // the encryption key from step (a).
    let mut encrypted = rc.encrypt(&hash);

    // e) Do the following 19 times: Take the output from the previous invocation of the
    // RC4 function and pass it as input to a new invocation of the function; use a file
    // encryption key generated by taking each byte of the original file encryption key
    // obtained in step (a) and performing an XOR (exclusive or) operation between that
    // byte and the single-byte value of the iteration counter (from 1 to 19).
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

/// Algorithm 2.A: Retrieving the file encryption key from an encrypted document in order to decrypt it (revision 6 and later)
fn decryption_key_rev56(
    dict: &Dict<'_>,
    revision: u8,
    password: &[u8],
    owner_string: &object::String<'_>,
    user_string: &object::String<'_>,
) -> Result<Vec<u8>, DecryptionError> {
    // a) The UTF-8 password string shall be generated from Unicode input by processing the input string with
    // the SASLprep (Internet RFC 4013) profile of stringprep (Internet RFC 3454) using the Normalize and BiDi
    // options, and then converting to a UTF-8 representation.
    // TODO: Do the above.
    // b) Truncate the UTF-8 representation to 127 bytes if it is longer than 127 bytes.
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

    // c) Test the password against the owner key by computing a hash using algorithm 2.B
    // with an input string consisting of the UTF-8 password concatenated with the 8 bytes of
    // owner Validation Salt, concatenated with the 48-byte U string. If the 32-byte result
    // matches the first 32 bytes of the O string, this is the owner password.
    if compute_hash_rev56(password, owner_validation_salt, Some(trimmed_us), revision)?
        == owner_hash
    {
        // d) Compute an intermediate owner key by computing a hash using algorithm 2.B with an input string
        // consisting of the UTF-8 owner password concatenated with the 8 bytes of owner Key Salt,
        // concatenated with the 48-byte U string. The 32-byte result is the key used to decrypt the 32-byte OE string
        // using AES-256 in CBC mode with no padding and an initialization vector of zero. The 32-byte result is the file encryption key.
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
        // e) Compute an intermediate user key by computing a hash using algorithm 2.B with an input string
        // consisting of the UTF-8 user password concatenated with the 8 bytes of user Key Salt. The 32-byte result
        // is the key used to decrypt the 32-byte UE string using AES-256 in CBC mode with no padding and an
        // initialization vector of zero. The 32-byte result is the file encryption key.
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

    // TODO:
    // f) Decrypt the 16-byte Perms string using AES-256 in ECB mode with an initialization vector of zero and
    // the file encryption key as the key. Verify that bytes 9-11 of the result are the characters "a", "d",
    // "b". Bytes 0-3 of the decrypted Perms entry, treated as a little-endian integer, are the user
    // permissions. They shall match the value in the P key.
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
