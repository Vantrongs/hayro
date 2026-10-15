//! PDF files, encrypted streams and allocation measurement for the integration tests.

// Each test crate uses a different part of this module.
#![allow(dead_code, unused_imports)]

pub(crate) mod allocations;
mod fixtures;
pub(crate) mod rc4;

pub(crate) use fixtures::{
    file, file_with_aliases, noise, one_page, one_page_objects, stored_zlib, stream,
};

use aes::cipher::block_padding::{NoPadding, Pkcs7};
use aes::cipher::{BlockEncryptMut, KeyIvInit};
use sha2::{Digest, Sha256};

type Aes256Cbc = cbc::Encryptor<aes::Aes256>;

const IV: [u8; 16] = [9; 16];

/// Trailer entries that encrypt a document with the dictionary at object `encrypt`.
pub(crate) fn aes256_trailer(encrypt: usize) -> String {
    format!("/Encrypt {encrypt} 0 R /ID [<01> <01>]")
}

/// An AES-256 (R5) encryption dictionary with empty passwords and file key `key`.
pub(crate) fn aes256_encryption(key: [u8; 32]) -> Vec<u8> {
    let salt = [3; 8];
    let hash = Sha256::digest(salt);
    let user: Vec<u8> = hash.iter().copied().chain(salt).chain(salt).collect();
    let mut ue = key;
    Aes256Cbc::new_from_slices(&hash, &[0; 16])
        .unwrap()
        .encrypt_padded_mut::<NoPadding>(&mut ue, 32)
        .unwrap();
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!(
        "<< /Filter /Standard /V 5 /R 5 /Length 256 /P -4 /O <{}> /U <{}> /UE <{}> \
         /StmF /StdCF /StrF /StdCF /CF << /StdCF << /CFM /AESV3 /Length 32 >> >> >>",
        hex(&[0; 48]),
        hex(&user),
        hex(&ue)
    )
    .into_bytes()
}

/// `data` encrypted with AES-256-CBC under `key` as a stream holds it: IV first.
pub(crate) fn aes256_cbc(key: [u8; 32], data: &[u8]) -> Vec<u8> {
    let mut padded = vec![0; data.len() + 16];
    padded[..data.len()].copy_from_slice(data);
    let encrypted = Aes256Cbc::new_from_slices(&key, &IV)
        .unwrap()
        .encrypt_padded_mut::<Pkcs7>(&mut padded, data.len())
        .unwrap();
    IV.iter().chain(encrypted.iter()).copied().collect()
}

/// A document with no pages whose object 3 is a stream with `dict` entries holding
/// `data` encrypted under `payload_key`, while the document's file key is `key`.
pub(crate) fn encrypted_stream_file(
    dict: &str,
    data: &[u8],
    key: [u8; 32],
    payload_key: [u8; 32],
) -> Vec<u8> {
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
        stream(dict, &aes256_cbc(payload_key, data)),
        aes256_encryption(key),
    ];
    file(&objects, &aes256_trailer(4))
}
