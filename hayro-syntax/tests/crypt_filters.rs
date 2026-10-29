//! Which crypt filter decrypts each stream and string of an encrypted document, which
//! streams are read as stored, and how a stream's list of filters and their parameters
//! is read, observed only through the crate's public API: `Pdf::new`, `XRef::get`, the
//! readings of `Stream`, `FilterEntries`, `Page::page_stream` and
//! `Page::try_page_stream`. The rules these tests pin are stated in the doc of the
//! crypt filter code (`hayro-syntax/src/crypto/mod.rs`, "rule N" below) and of
//! `hayro_syntax::object::stream::FilterEntries` ("filter rule N");
//! `tests/predictor.rs` (`filters_keep_their_parameters_in_place_as_mupdf_reads_them`)
//! pins the rest of how the parameters are matched to the filters (filter rule 3).
//!
//! Files are built here with the empty user password: RC4 of 40 bits (`/V 1 /R 2`) and
//! of 128 bits (`/V 2 /R 3`), crypt filters of `/V 4 /R 4`, with `/EncryptMetadata`
//! true or false, and AES-256 (`/V 5 /R 5`). Each test encrypts each stream and string
//! with the method it expects to decrypt it, and expects the plain data back, or stores
//! it as is where it expects it read as stored; a stream that cannot be decrypted is
//! [`DecodeFailure::Decryption`] for every reading, except where rule 11 says.
//!
//! With `CRYPT_FILTERS_PDFS=<dir>` set, each file a test reads is written there under
//! the test's name with a `.hayro` file of what this crate reads from each object: a
//! stream decoded strictly and its raw data, or a dictionary's `/T` string, in hex. A
//! script that prints the same from `PyMuPDF` and compares the decoded data, the
//! strings and whether the document opens is kept outside this repository; where a test
//! says what `PyMuPDF` 1.28.2 reads, it was checked that way. `PyMuPDF` differs
//! deliberately where a test says so; its other differences are noted as observed. Its
//! raw data of a stream keep what a `/Crypt` filter encrypts, unlike
//! `Stream::try_raw_data` (rule 9).

use aes::cipher::block_padding::{NoPadding, Pkcs7};
use aes::cipher::{BlockEncryptMut, KeyIvInit};
use hayro_syntax::object::stream::{DecodeFailure, FilterEntries};
use hayro_syntax::object::{Dict, Object, ObjectIdentifier, Stream};
use hayro_syntax::page::ContentError;
use hayro_syntax::work::Meter;
use hayro_syntax::{DecryptionError, Filter, LoadPdfError, Pdf};
use sha2::{Digest, Sha256};
use std::io::Write;

mod common;

type Aes128Cbc = cbc::Encryptor<aes::Aes128>;
type Aes256Cbc = cbc::Encryptor<aes::Aes256>;

const PADDING: [u8; 32] = [
    0x28, 0xbf, 0x4e, 0x5e, 0x4e, 0x75, 0x8a, 0x41, 0x64, 0x00, 0x4e, 0x56, 0xff, 0xfa, 0x01, 0x08,
    0x2e, 0x2e, 0x00, 0xb6, 0xd0, 0x68, 0x3e, 0x80, 0x2f, 0x0c, 0xa9, 0xfe, 0x64, 0x53, 0x69, 0x7a,
];
const ID: &[u8; 16] = b"crypt-filters-id";
const OWNER: [u8; 32] = [0x4f; 32];
const PERMISSIONS: i32 = -4;
const IV: [u8; 16] = [5; 16];
const AES256_KEY: [u8; 32] = [0x6b; 32];

const CONTENT: &[u8] = b"0 0 1 rg 0 0 10 10 re f";
const OTHER: &[u8] = b"1 0 0 rg 10 0 10 10 re f";
const TEXT: &[u8] = b"a string of the document";

/// The usual crypt filters of `/V 4` and `/V 5`: `/StdCF` for streams and strings.
const AESV2: &str = "/CF << /StdCF << /CFM /AESV2 /Length 16 >> >> /StmF /StdCF /StrF /StdCF";
const AESV3: &str = "/CF << /StdCF << /CFM /AESV3 /Length 32 >> >> /StmF /StdCF /StrF /StdCF";

/// The standard security handler a file is encrypted with, with the empty password.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Handler {
    /// RC4 with a 40-bit key, `/V 1` `/R` 2.
    R2,
    /// RC4 with a 128-bit key, `/V 2` `/R` 3.
    R3,
    /// Crypt filters with a 128-bit key, `/V 4 /R 4`.
    R4,
    /// [`Handler::R4`] with `/EncryptMetadata` false.
    R4Clear,
    /// Crypt filters with a 256-bit key, `/V 5` `/R` 5.
    R5,
}

/// How a stream's data or a string is stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cipher {
    /// As is.
    Stored,
    /// RC4 with the key of its object.
    Rc4,
    /// AES-128 with the key of its object, the IV first.
    Aes128,
    /// AES-256 with the file key, the IV first.
    Aes256,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    common::rc4::rc4(key, data)
}

impl Handler {
    /// The file key.
    fn key(self) -> Vec<u8> {
        if self == Self::R5 {
            return AES256_KEY.to_vec();
        }
        // Algorithm 2.
        let mut input = PADDING.to_vec();
        input.extend(OWNER);
        input.extend(PERMISSIONS.to_le_bytes());
        input.extend(ID);
        if self == Self::R4Clear {
            input.extend([0xff; 4]);
        }
        let mut hash = md5::compute(&input).0;
        if self == Self::R2 {
            return hash[..5].to_vec();
        }
        for _ in 0..50 {
            hash = md5::compute(hash).0;
        }
        hash.to_vec()
    }

    /// The entries of the encryption dictionary that the handler sets.
    fn entries(self) -> String {
        let key = self.key();
        let owner = hex(&OWNER);
        match self {
            // Algorithm 4.
            Self::R2 => format!(
                "/V 1 /R 2 /O <{owner}> /U <{}> /P {PERMISSIONS}",
                hex(&rc4(&key, &PADDING))
            ),
            Self::R3 | Self::R4 | Self::R4Clear => {
                // Algorithm 5.
                let mut input = PADDING.to_vec();
                input.extend(ID);
                let mut user = rc4(&key, &md5::compute(&input).0);
                for i in 1..=19_u8 {
                    let key: Vec<u8> = key.iter().map(|b| b ^ i).collect();
                    user = rc4(&key, &user);
                }
                user.extend([0; 16]);
                let version = match self {
                    Self::R3 => "/V 2 /R 3",
                    Self::R4 => "/V 4 /R 4",
                    _ => "/V 4 /R 4 /EncryptMetadata false",
                };
                format!(
                    "{version} /Length 128 /O <{owner}> /U <{}> /P {PERMISSIONS}",
                    hex(&user)
                )
            }
            Self::R5 => {
                // Algorithms 8 and 2.A with the empty password.
                let (validation, salt) = ([1_u8; 8], [2_u8; 8]);
                let user: Vec<u8> = Sha256::digest(validation)
                    .iter()
                    .copied()
                    .chain(validation)
                    .chain(salt)
                    .collect();
                let mut ue = AES256_KEY;
                Aes256Cbc::new_from_slices(&Sha256::digest(salt), &[0; 16])
                    .unwrap()
                    .encrypt_padded_mut::<NoPadding>(&mut ue, 32)
                    .unwrap();
                format!(
                    "/V 5 /R 5 /Length 256 /O <{}> /U <{}> /OE <{}> /UE <{}> /P {PERMISSIONS}",
                    hex(&[0; 48]),
                    hex(&user),
                    hex(&[0; 32]),
                    hex(&ue)
                )
            }
        }
    }
}

/// `data` stored with `cipher` as object `number` of a file whose key is `key`.
fn encrypt(key: &[u8], cipher: Cipher, number: u32, data: &[u8]) -> Vec<u8> {
    // Algorithm 1.
    let object_key = |aes: bool| {
        let mut input = key.to_vec();
        input.extend(&number.to_le_bytes()[..3]);
        input.extend([0, 0]);
        if aes {
            input.extend(b"sAlT");
        }
        md5::compute(&input).0[..(key.len() + 5).min(16)].to_vec()
    };
    let mut padded = vec![0; data.len() + 16];
    padded[..data.len()].copy_from_slice(data);
    let encrypted = match cipher {
        Cipher::Stored => return data.to_vec(),
        Cipher::Rc4 => return rc4(&object_key(false), data),
        Cipher::Aes128 => Aes128Cbc::new_from_slices(&object_key(true), &IV)
            .unwrap()
            .encrypt_padded_mut::<Pkcs7>(&mut padded, data.len())
            .unwrap(),
        Cipher::Aes256 => Aes256Cbc::new_from_slices(key, &IV)
            .unwrap()
            .encrypt_padded_mut::<Pkcs7>(&mut padded, data.len())
            .unwrap(),
    };
    IV.iter().chain(encrypted.iter()).copied().collect()
}

/// An object of a test file.
enum Obj<'a> {
    /// Written as it is.
    Plain(&'a str),
    /// A dictionary whose `/T` is a string holding `text`, stored with `cipher`.
    Text(&'a [u8], Cipher),
    /// A stream with `dict` entries besides `/Length`, holding `data` stored with
    /// `cipher`.
    Stream(&'a str, &'a [u8], Cipher),
}

const CATALOG: Obj<'_> = Obj::Plain("<< /Type /Catalog /Pages 2 0 R >>");
const NO_PAGES: Obj<'_> = Obj::Plain("<< /Type /Pages /Kids [] /Count 0 >>");

/// The objects of a file numbered from 1, the first being the catalog, and the offset
/// of each.
fn body(handler: Option<Handler>, objects: &[Obj<'_>]) -> (Vec<u8>, Vec<usize>) {
    let key = handler.map(Handler::key).unwrap_or_default();
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        let number = i as u32 + 1;
        offsets.push(out.len());
        out.extend(format!("{number} 0 obj\n").bytes());
        match object {
            Obj::Plain(body) => out.extend(body.bytes()),
            Obj::Text(text, cipher) => out.extend(
                format!("<< /T <{}> >>", hex(&encrypt(&key, *cipher, number, text))).bytes(),
            ),
            Obj::Stream(dict, data, cipher) => {
                let data = encrypt(&key, *cipher, number, data);
                out.extend(format!("<< {dict} /Length {} >>\nstream\n", data.len()).bytes());
                out.extend(data);
                out.extend(b"\nendstream");
            }
        }
        out.extend(b"\nendobj\n");
    }
    (out, offsets)
}

/// The trailer entries of a file encrypted by `handler` whose encryption dictionary
/// also holds `entries`.
fn encryption(handler: Option<Handler>, entries: &str) -> String {
    let id = hex(ID);
    let encrypt = handler.map_or(String::new(), |handler| {
        format!(
            "/Encrypt << /Filter /Standard {} {entries} >>",
            handler.entries()
        )
    });
    format!("/ID [<{id}> <{id}>] {encrypt}")
}

/// A file of `objects` with a cross-reference table, encrypted by `handler` with
/// `entries` in its encryption dictionary, or not encrypted without a handler.
fn file(handler: Option<Handler>, entries: &str, objects: &[Obj<'_>]) -> Vec<u8> {
    let (mut out, offsets) = body(handler, objects);
    let size = objects.len() + 1;
    let xref = out.len();
    out.extend(format!("xref\n0 {size}\n0000000000 65535 f \n").bytes());
    for offset in offsets {
        out.extend(format!("{offset:010} 00000 n \n").bytes());
    }
    out.extend(
        format!(
            "trailer\n<< /Size {size} /Root 1 0 R {} >>\nstartxref\n{xref}\n%%EOF\n",
            encryption(handler, entries)
        )
        .bytes(),
    );
    out
}

/// [`file`] whose table is a cross-reference stream, stored as is, that follows the
/// objects, its `/Type` `xref_type`.
fn file_with_xref_stream(
    handler: Handler,
    entries: &str,
    objects: &[Obj<'_>],
    xref_type: &str,
) -> Vec<u8> {
    let (mut out, mut offsets) = body(Some(handler), objects);
    let number = objects.len() + 1;
    offsets.push(out.len());
    let mut table = vec![0, 0, 0, 0, 0, 0xff, 0xff];
    for offset in offsets.iter() {
        table.push(1);
        table.extend((*offset as u32).to_be_bytes());
        table.extend([0, 0]);
    }
    out.extend(
        format!(
            "{number} 0 obj\n<< /Type {xref_type} /Size {} /W [1 4 2] /Root 1 0 R {} \
             /Length {} >>\nstream\n",
            number + 1,
            encryption(Some(handler), entries),
            table.len()
        )
        .bytes(),
    );
    out.extend(&table);
    out.extend(b"\nendstream\nendobj\n");
    out.extend(format!("startxref\n{}\n%%EOF\n", offsets[number - 1]).bytes());
    out
}

/// A file whose object 3 is a stream with `dict` entries holding `data` stored with
/// `cipher`.
fn one_stream(
    handler: Option<Handler>,
    entries: &str,
    dict: &str,
    data: &[u8],
    cipher: Cipher,
) -> Vec<u8> {
    file(
        handler,
        entries,
        &[CATALOG, NO_PAGES, Obj::Stream(dict, data, cipher)],
    )
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// `data` LZW-encoded in 9-bit codes: a clear-table code, each byte, the end-of-data
/// code.
fn lzw(data: &[u8]) -> Vec<u8> {
    let codes = [256].into_iter().chain(data.iter().map(|&b| u32::from(b)));
    let (mut bits, mut count, mut out) = (0_u64, 0, Vec::new());
    for code in codes.chain([257]) {
        bits = bits << 9 | u64::from(code);
        count += 9;
        while count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    out.push((bits << (8 - count)) as u8);
    out
}

/// What a failure or a value is shown as: hex, or `ERR` and the failure.
fn shown(read: &Result<Vec<u8>, String>) -> String {
    match read {
        Ok(data) => hex(data),
        Err(e) => format!("ERR {e}"),
    }
}

/// Writes `file` under `name`, and what this crate reads of each object in it, where
/// `CRYPT_FILTERS_PDFS` names a directory.
fn dump(name: &str, file: &[u8], pdf: Result<&Pdf, &LoadPdfError>) {
    let Some(dir) = std::env::var_os("CRYPT_FILTERS_PDFS") else {
        return;
    };
    let dir = std::path::Path::new(&dir);
    std::fs::write(dir.join(format!("{name}.pdf")), file).unwrap();
    let mut out = String::new();
    match pdf {
        Err(e) => out.push_str(&format!("open ERR {e:?}\n")),
        Ok(pdf) => {
            for n in 1..64 {
                match pdf.xref().get::<Object<'_>>(ObjectIdentifier::new(n, 0)) {
                    Some(Object::Stream(stream)) => {
                        let decoded = stream
                            .decoded_strict()
                            .map(|d| d.to_vec())
                            .map_err(|e| format!("{e:?}"));
                        let raw = stream
                            .try_raw_data()
                            .map(|d| d.to_vec())
                            .map_err(|e| format!("{e:?}"));
                        out.push_str(&format!(
                            "{n} stream {} raw {}\n",
                            shown(&decoded),
                            shown(&raw)
                        ));
                    }
                    Some(Object::Dict(dict)) => {
                        if let Some(text) = dict.get::<hayro_syntax::object::String<'_>>(b"T") {
                            out.push_str(&format!("{n} string {}\n", hex(&text)));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    std::fs::write(dir.join(format!("{name}.hayro")), out).unwrap();
}

fn open(name: &str, file: Vec<u8>) -> Pdf {
    let pdf = Pdf::new(file.clone());
    dump(name, &file, pdf.as_ref());
    pdf.unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

fn object(pdf: &Pdf, n: i32) -> Stream<'_> {
    pdf.xref().get(ObjectIdentifier::new(n, 0)).unwrap()
}

fn text(pdf: &Pdf, n: i32) -> Vec<u8> {
    let dict: Dict<'_> = pdf.xref().get(ObjectIdentifier::new(n, 0)).unwrap();
    dict.get::<hayro_syntax::object::String<'_>>(b"T")
        .unwrap()
        .to_vec()
}

/// All that `read` gives, three bytes at a time; `None` where it fails.
fn read_all(
    mut read: impl FnMut(&mut [u8]) -> Result<usize, hayro_syntax::object::stream::ReadError>,
) -> Option<Vec<u8>> {
    let mut buf = [0; 3];
    let mut out = Vec::new();
    loop {
        match read(&mut buf) {
            Ok(0) => return Some(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => return None,
        }
    }
}

/// What every strict reading of `stream` decodes it to — `decoded_strict` and the
/// strict reader that reads a piece at a time where it offers one — or the failure
/// they share; a panic where they differ, where the lenient `decoded` differs from
/// data they give, or where the lenient incremental reader differs from `decoded`.
fn read(stream: &Stream<'_>) -> Result<Vec<u8>, String> {
    let name = |e: DecodeFailure| format!("{e:?}");
    let strict = stream.decoded_strict().map(|d| d.to_vec()).map_err(name);
    let lenient = stream.decoded().map(|d| d.to_vec()).map_err(name);
    if strict.is_ok() {
        assert_eq!(lenient, strict, "decoded and decoded_strict");
    }
    match stream.decoded_strict_reader(&Meter::default()) {
        Err(e) => assert_eq!(Err(name(e)), strict, "decoded_strict_reader"),
        Ok(None) => {}
        Ok(Some(mut reader)) => {
            let read = read_all(|buf| reader.read(buf, &Meter::default()));
            assert_eq!(read, strict.clone().ok(), "decoded_strict_reader");
        }
    }
    if let Some(mut reader) = stream.decoded_reader() {
        let read = read_all(|buf| reader.read(buf, &Meter::default()));
        assert_eq!(read, lenient.ok(), "decoded_reader");
    }
    strict
}

/// The raw data of `stream`, decrypted but not decoded, or the failure; `raw_data`
/// gives the same, or nothing on a failure.
fn raw(stream: &Stream<'_>) -> Result<Vec<u8>, String> {
    let raw = stream.try_raw_data().map(|d| d.to_vec());
    assert_eq!(
        stream.raw_data().to_vec(),
        raw.clone().unwrap_or_default(),
        "raw_data"
    );
    raw.map_err(|e| format!("{e:?}"))
}

/// Whether every reading of `stream` fails to decrypt it, the lenient ones included,
/// and no incremental reader is offered.
fn refused(stream: &Stream<'_>) -> bool {
    read(stream) == decryption_failure()
        && matches!(stream.decoded(), Err(DecodeFailure::Decryption))
        && raw(stream) == decryption_failure()
        && stream.decoded_reader().is_none()
}

fn decryption_failure() -> Result<Vec<u8>, String> {
    Err("Decryption".to_owned())
}

/// Rule 1: RC4 documents (`/V 1` and `/V 2`) decrypt every stream and string with RC4
/// and the file key; they have no crypt filters, and `/CF`, `/StmF` and `/StrF` are not
/// read. `PyMuPDF` 1.28.2 reads the same.
#[test]
fn rc4_documents_decrypt_every_stream_and_string() {
    for handler in [Handler::R2, Handler::R3] {
        for entries in ["", "/StmF /Identity /StrF /Identity", AESV2] {
            let name = format!("rc4_{handler:?}_{}", entries.len());
            let pdf = open(
                &name,
                file(
                    Some(handler),
                    entries,
                    &[
                        CATALOG,
                        NO_PAGES,
                        Obj::Stream("", CONTENT, Cipher::Rc4),
                        Obj::Text(TEXT, Cipher::Rc4),
                    ],
                ),
            );
            assert_eq!(read(&object(&pdf, 3)), Ok(CONTENT.to_vec()), "{name}");
            assert_eq!(raw(&object(&pdf, 3)), Ok(CONTENT.to_vec()), "{name}");
            assert_eq!(text(&pdf, 4), TEXT, "{name}");
        }
    }
}

/// Rule 2: in a `/V 4` or `/V 5` document, `/StmF` names the crypt filter of the
/// streams and `/StrF` that of the strings, each looked up in `/CF` by name, whatever
/// the name; `/CFM` `/V2` is RC4, `/AESV2` AES-128 and `/AESV3` AES-256, each with the
/// file key whatever the crypt filter's `/Length`. `PyMuPDF` 1.28.2 reads the same,
/// except that it refuses to open a document whose `/StmF` or `/StrF` names another
/// crypt filter than `/StdCF`, the name the PDF specification gives the standard one,
/// deliberately not followed here; it asks for a password where the crypt filter's
/// `/Length` is shorter than the file key; and it decrypts `/AESV2` in a `/V 5`
/// document as `/AESV3`.
#[test]
fn the_stream_and_string_filters_name_crypt_filters_of_cf() {
    #[rustfmt::skip]
    let cases: &[(Handler, &str, Cipher, Cipher)] = &[
        (Handler::R4, AESV2, Cipher::Aes128, Cipher::Aes128),
        (Handler::R5, AESV3, Cipher::Aes256, Cipher::Aes256),
        (Handler::R4, "/CF << /StdCF << /CFM /V2 >> >> /StmF /StdCF /StrF /StdCF",
            Cipher::Rc4, Cipher::Rc4),
        (Handler::R4, "/CF << /StdCF << /CFM /AESV2 >> >> /StmF /Identity /StrF /StdCF",
            Cipher::Stored, Cipher::Aes128),
        (Handler::R4, "/CF << /StdCF << /CFM /AESV2 >> >> /StmF /StdCF /StrF /Identity",
            Cipher::Aes128, Cipher::Stored),
        (Handler::R4, "/CF << /A << /CFM /AESV2 >> /B << /CFM /V2 >> >> /StmF /A /StrF /B",
            Cipher::Aes128, Cipher::Rc4),
        (Handler::R4, "/CF << /A << /CFM /AESV2 >> /B << /CFM /V2 >> >> /StmF /B /StrF /A",
            Cipher::Rc4, Cipher::Aes128),
        // The /Length of a crypt filter is not read: the file key is used whole.
        (Handler::R4, "/CF << /StdCF << /CFM /V2 /Length 5 >> >> /StmF /StdCF /StrF /StdCF",
            Cipher::Rc4, Cipher::Rc4),
        // Nor a shorter one with AES-256.
        (Handler::R5, "/CF << /StdCF << /CFM /AESV3 /Length 16 >> >> /StmF /StdCF /StrF /StdCF",
            Cipher::Aes256, Cipher::Aes256),
        (Handler::R5, "/CF << /StdCF << /CFM /AESV2 >> >> /StmF /StdCF /StrF /StdCF",
            Cipher::Aes128, Cipher::Aes128),
    ];
    for (i, &(handler, entries, stream, string)) in cases.iter().enumerate() {
        let pdf = open(
            &format!("document_filters_{i}"),
            file(
                Some(handler),
                entries,
                &[
                    CATALOG,
                    NO_PAGES,
                    Obj::Stream("", CONTENT, stream),
                    Obj::Text(TEXT, string),
                ],
            ),
        );
        assert_eq!(read(&object(&pdf, 3)), Ok(CONTENT.to_vec()), "{entries}");
        assert_eq!(text(&pdf, 4), TEXT, "{entries}");
    }
}

/// Rule 3: `/StmF` or `/StrF` that names `/Identity`, a name `/CF` does not define, a
/// crypt filter of `/CF` that is no dictionary or whose `/CFM` is missing, `/None`,
/// `/Identity` or another name rule 2 does not know, or any name in a document whose
/// `/CF` is missing or no dictionary, leaves its streams or strings as stored.
/// `PyMuPDF` 1.28.2 reads the same where `/CF` defines the crypt filter as a
/// dictionary; it refuses to open the document where `/CF` does not define the name or
/// its entry is no dictionary, and where `/CF` is missing or no dictionary it decrypts
/// `/StdCF` with RC4 and the file key.
#[test]
fn a_document_filter_that_names_no_method_leaves_its_data_as_stored() {
    let cases = [
        "/CF << /StdCF << /CFM /AESV2 >> >> /StmF /Identity /StrF /Identity",
        "/CF << /StdCF << /CFM /AESV2 >> >> /StmF /Other /StrF /Other",
        "/CF << /StdCF 5 >> /StmF /StdCF /StrF /StdCF",
        "/CF << /StdCF << /Length 16 >> >> /StmF /StdCF /StrF /StdCF",
        "/CF << /StdCF << /CFM /None >> >> /StmF /StdCF /StrF /StdCF",
        "/CF << /StdCF << /CFM /Identity >> >> /StmF /StdCF /StrF /StdCF",
        "/CF << /StdCF << /CFM /Unknown >> >> /StmF /StdCF /StrF /StdCF",
        "/CF 5 /StmF /StdCF /StrF /StdCF",
        // A document without /CF.
        "/StmF /StdCF /StrF /StdCF",
    ];
    for (i, entries) in cases.into_iter().enumerate() {
        let pdf = open(
            &format!("stored_by_document_filter_{i}"),
            file(
                Some(Handler::R4),
                entries,
                &[
                    CATALOG,
                    NO_PAGES,
                    Obj::Stream("", CONTENT, Cipher::Stored),
                    Obj::Text(TEXT, Cipher::Stored),
                ],
            ),
        );
        assert_eq!(read(&object(&pdf, 3)), Ok(CONTENT.to_vec()), "{entries}");
        assert_eq!(text(&pdf, 4), TEXT, "{entries}");
    }
}

/// Rule 3: `/StmF` and `/StrF` that name `/Identity` where `/CF` defines a crypt filter
/// of that name use it; a `/Crypt` filter named `/Identity` still leaves its stream as
/// stored (rule 5). `PyMuPDF` 1.28.2 reads the same.
#[test]
fn identity_defined_in_cf_is_used_by_the_document_filters() {
    let entries = "/CF << /Identity << /CFM /AESV2 >> >> /StmF /Identity /StrF /Identity";
    let pdf = open(
        "identity_in_cf",
        file(
            Some(Handler::R4),
            entries,
            &[
                CATALOG,
                NO_PAGES,
                Obj::Stream("", CONTENT, Cipher::Aes128),
                Obj::Text(TEXT, Cipher::Aes128),
                // A /Crypt filter named /Identity stays no encryption.
                Obj::Stream(
                    "/Filter /Crypt /DecodeParms << /Name /Identity >>",
                    OTHER,
                    Cipher::Stored,
                ),
            ],
        ),
    );
    assert_eq!(read(&object(&pdf, 3)), Ok(CONTENT.to_vec()));
    assert_eq!(text(&pdf, 4), TEXT);
    assert_eq!(read(&object(&pdf, 5)), Ok(OTHER.to_vec()));
}

/// Rule 4: a `/V 4` or `/V 5` document without `/StmF` or `/StrF`, or with one that is
/// no name, cannot be opened. `PyMuPDF` 1.28.2 opens it and reads such an entry as
/// `/Identity`.
#[test]
fn a_document_filter_that_is_missing_refuses_the_document() {
    for (i, entries) in [
        "/CF << /StdCF << /CFM /AESV2 >> >> /StrF /StdCF",
        "/CF << /StdCF << /CFM /AESV2 >> >> /StmF /StdCF",
        "/CF << /StdCF << /CFM /AESV2 >> >>",
        "/CF << /StdCF << /CFM /AESV2 >> >> /StmF (StdCF) /StrF /StdCF",
    ]
    .into_iter()
    .enumerate()
    {
        for handler in [Handler::R4, Handler::R5] {
            let file = one_stream(Some(handler), entries, "", CONTENT, Cipher::Stored);
            let pdf = Pdf::new(file.clone());
            dump(
                &format!("missing_document_filter_{handler:?}_{i}"),
                &file,
                pdf.as_ref(),
            );
            assert!(
                matches!(
                    pdf,
                    Err(LoadPdfError::Decryption(DecryptionError::InvalidEncryption))
                ),
                "{handler:?} {entries}"
            );
        }
    }
}

/// Rule 2: a method that cannot use the file key — AES-256 with a 128-bit key — fails
/// each stream it decrypts, and leaves each string as stored. `PyMuPDF` 1.28.2 refuses
/// to open the document.
#[test]
fn aes_256_with_a_128_bit_key_fails() {
    let entries = "/CF << /StdCF << /CFM /AESV3 >> >> /StmF /StdCF /StrF /StdCF";
    let pdf = open(
        "aesv3_in_v4",
        file(
            Some(Handler::R4),
            entries,
            &[
                CATALOG,
                NO_PAGES,
                Obj::Stream("", &[7; 32], Cipher::Stored),
                Obj::Text(&[7; 32], Cipher::Stored),
            ],
        ),
    );
    assert!(refused(&object(&pdf, 3)));
    assert_eq!(text(&pdf, 4), [7; 32]);
}

/// Rule 5: a stream with a `/Crypt` filter whose parameters have no `/Name`, or name
/// `/Identity`, is stored as is, whatever `/StmF` says, wherever the `/Crypt` filter
/// stands among the filters. The parameters are matched to the filters as for any
/// filter (filter rule 3). `PyMuPDF` 1.28.2 reads the same.
#[test]
fn a_crypt_filter_without_a_name_or_named_identity_leaves_the_stream_as_stored() {
    let flate = zlib(CONTENT);
    #[rustfmt::skip]
    let cases: &[(&str, &[u8])] = &[
        ("/Filter /Crypt", CONTENT),
        ("/Filter /Crypt /DecodeParms << /Name /Identity >>", CONTENT),
        ("/Filter /Crypt /DecodeParms << /Type /CryptFilterDecodeParms >>", CONTENT),
        ("/Filter /Crypt /DecodeParms null", CONTENT),
        // /Name that is no name.
        ("/Filter /Crypt /DecodeParms << /Name (StdCF) >>", CONTENT),
        ("/Filter [/Crypt] /DecodeParms [<< /Name /Identity >>]", CONTENT),
        ("/Filter [null /Crypt] /DecodeParms [null << /Name /Identity >>]", CONTENT),
        ("/Filter [null /Crypt]", CONTENT),
        ("/Filter [/Crypt /FlateDecode] /DecodeParms [<< /Name /Identity >> null]", &flate),
        ("/Filter [/FlateDecode /Crypt] /DecodeParms [null << /Name /Identity >>]", &flate),
        ("/Filter [/FlateDecode /Crypt]", &flate),
        // A /Name of the parameters of another filter does not count.
        ("/Filter [/Crypt /FlateDecode] /DecodeParms [null << /Name /StdCF >>]", &flate),
        // A dictionary of parameters for an array of filters is none.
        ("/Filter [/Crypt] /DecodeParms << /Name /StdCF >>", CONTENT),
    ];
    for (i, &(dict, data)) in cases.iter().enumerate() {
        let pdf = open(
            &format!("unnamed_crypt_{i}"),
            one_stream(Some(Handler::R4), AESV2, dict, data, Cipher::Stored),
        );
        assert_eq!(read(&object(&pdf, 3)), Ok(CONTENT.to_vec()), "{dict}");
        assert_eq!(raw(&object(&pdf, 3)), Ok(data.to_vec()), "{dict}");
    }
}

/// Rule 6: a stream whose first filter is a `/Crypt` filter with `/Name` naming a crypt
/// filter of `/CF` other than `/Identity` is decrypted by that one instead of `/StmF`;
/// the raw data are decrypted and not decoded. Entries of the filters that name no
/// filter are passed over in counting the first; `/Name` and the parameters may be
/// references. `PyMuPDF` 1.28.2 decodes the same where the name is `/StdCF`, written
/// directly, and fails the stream where it is another name (rule 2) or a reference.
#[test]
fn a_crypt_filter_named_in_cf_decrypts_its_stream() {
    let flate = zlib(CONTENT);
    let entries = "/CF << /StdCF << /CFM /V2 >> /Mine << /CFM /AESV2 >> >> \
                   /StmF /Identity /StrF /Identity";
    #[rustfmt::skip]
    let cases: &[(&str, &[u8], Cipher, &[&str])] = &[
        ("/Filter /Crypt /DecodeParms << /Name /StdCF >>", CONTENT, Cipher::Rc4, &[]),
        ("/Filter [/Crypt /FlateDecode] /DecodeParms [<< /Name /StdCF >> null]",
            &flate, Cipher::Rc4, &[]),
        ("/Filter [/Crypt /FlateDecode] /DecodeParms [<< /Name /StdCF >>]",
            &flate, Cipher::Rc4, &[]),
        ("/Filter [null /Crypt /Fl] /DecodeParms [null << /Name /StdCF >>]",
            &flate, Cipher::Rc4, &[]),
        ("/Filter [/Foo /Crypt /Fl] /DecodeParms [null << /Name /StdCF >>]",
            &flate, Cipher::Rc4, &[]),
        // A second /Crypt filter without a name, or named /Identity.
        ("/Filter [/Crypt /Crypt] /DecodeParms [<< /Name /StdCF >> << /Name /Identity >>]",
            CONTENT, Cipher::Rc4, &[]),
        ("/Filter [/Crypt /Crypt] /DecodeParms [<< /Name /StdCF >>]",
            CONTENT, Cipher::Rc4, &[]),
        // The parameters and the name through references.
        ("/Filter /Crypt /DecodeParms 4 0 R", CONTENT, Cipher::Rc4, &["<< /Name /StdCF >>"]),
        ("/Filter /Crypt /DecodeParms << /Name 4 0 R >>", CONTENT, Cipher::Rc4, &["/StdCF"]),
        ("/Filter 4 0 R /DecodeParms 5 0 R", CONTENT, Cipher::Rc4,
            &["[/Crypt]", "[<< /Name /StdCF >>]"]),
        ("/DP << /Name /StdCF >> /Filter /Crypt /DecodeParms << /Name /Mine >>",
            CONTENT, Cipher::Rc4, &[]),
        // Names other than /StdCF, which `PyMuPDF` refuses.
        ("/Filter /Crypt /DecodeParms << /Name /Mine >>", CONTENT, Cipher::Aes128, &[]),
        ("/Filter [/Crypt /Fl] /DecodeParms [<< /Name /Mine >>]", &flate, Cipher::Aes128, &[]),
    ];
    for (i, &(dict, data, cipher, extra)) in cases.iter().enumerate() {
        let mut objects = vec![CATALOG, NO_PAGES, Obj::Stream(dict, data, cipher)];
        objects.extend(extra.iter().map(|e| Obj::Plain(e)));
        let pdf = open(
            &format!("named_crypt_{i}"),
            file(Some(Handler::R4), entries, &objects),
        );
        assert_eq!(read(&object(&pdf, 3)), Ok(CONTENT.to_vec()), "{dict}");
        assert_eq!(raw(&object(&pdf, 3)), Ok(data.to_vec()), "{dict}");
    }
}

/// Rule 6: a `/Crypt` filter naming a crypt filter that `/CF` does not define, or that
/// is no dictionary, or whose `/CFM` is missing or of no method rule 2 knows, fails its
/// stream for every reading; one whose `/CFM` is `/None` or `/Identity` leaves it as
/// stored. `PyMuPDF` 1.28.2 reads the same, except that it leaves the stream as stored
/// where `/CFM` is unknown, deliberately not followed here: data whose encryption
/// cannot be undone are not passed on as plain; and where `/CFM` is missing, which the
/// PDF specification reads as `/None`, so that failing it is suspected wrong.
#[test]
fn a_crypt_filter_naming_no_method_of_cf_fails() {
    let refusals = [
        ("/CF << /StdCF << /CFM /AESV2 >> >>", "Other"),
        ("/CF << /StdCF 5 >>", "StdCF"),
        ("/CF << /StdCF << /Length 16 >> >>", "StdCF"),
        ("/CF << /StdCF << /CFM /Unknown >> >>", "StdCF"),
    ];
    for (i, (cf, name)) in refusals.into_iter().enumerate() {
        let entries = format!("{cf} /StmF /Identity /StrF /Identity");
        let dict = format!("/Filter /Crypt /DecodeParms << /Name /{name} >>");
        let pdf = open(
            &format!("undefined_crypt_{i}"),
            one_stream(Some(Handler::R4), &entries, &dict, CONTENT, Cipher::Stored),
        );
        assert!(refused(&object(&pdf, 3)), "{cf} {name}");
    }
    for method in ["None", "Identity"] {
        let pdf = open(
            &format!("none_crypt_{method}"),
            one_stream(
                Some(Handler::R4),
                &format!("/CF << /StdCF << /CFM /{method} >> >> /StmF /Identity /StrF /Identity"),
                "/Filter /Crypt /DecodeParms << /Name /StdCF >>",
                CONTENT,
                Cipher::Stored,
            ),
        );
        assert_eq!(read(&object(&pdf, 3)), Ok(CONTENT.to_vec()), "{method}");
    }
}

/// Rule 6: a `/Crypt` filter with a name other than `/Identity` that is not the first
/// filter fails its stream. `PyMuPDF` 1.28.2 decrypts in its place the data the
/// filters before it decoded, deliberately not followed here: the PDF specification
/// requires a `/Crypt` filter to come first.
#[test]
fn a_named_crypt_filter_after_another_filter_fails() {
    let entries = "/CF << /StdCF << /CFM /AESV2 >> >> /StmF /StdCF /StrF /StdCF";
    let key = Handler::R4.key();
    let aes = |data: &[u8]| encrypt(&key, Cipher::Aes128, 3, data);
    let mut hexed = hex(&aes(CONTENT)).into_bytes();
    hexed.push(b'>');
    let cases: [(&str, Vec<u8>); 5] = [
        (
            "/Filter [/FlateDecode /Crypt] /DecodeParms [null << /Name /StdCF >>]",
            zlib(&aes(CONTENT)),
        ),
        (
            "/Filter [/AHx /Crypt] /DecodeParms [null << /Name /StdCF >>]",
            hexed,
        ),
        (
            "/Filter [/Crypt /Crypt] /DecodeParms [null << /Name /StdCF >>]",
            aes(CONTENT),
        ),
        (
            "/Filter [/Crypt /Crypt] /DecodeParms [<< /Name /Identity >> << /Name /StdCF >>]",
            aes(CONTENT),
        ),
        (
            "/Filter [/Crypt /Crypt] /DecodeParms [<< /Name /StdCF >> << /Name /StdCF >>]",
            aes(&aes(CONTENT)),
        ),
    ];
    for (i, (dict, data)) in cases.iter().enumerate() {
        let pdf = open(
            &format!("crypt_after_another_{i}"),
            one_stream(Some(Handler::R4), entries, dict, data, Cipher::Stored),
        );
        assert!(refused(&object(&pdf, 3)), "{dict}");
    }
}

/// Rule 7: in an RC4 document, and in a `/V 4` document without `/CF`, a `/Crypt`
/// filter named `/StdCF` decrypts with RC4 and the file key, and any other name but
/// `/Identity` fails. `PyMuPDF` 1.28.2 decodes the same.
#[test]
fn crypt_filters_without_cf_know_only_stdcf() {
    for (handler, entries) in [
        (Handler::R2, ""),
        (Handler::R3, ""),
        (Handler::R4, "/StmF /StdCF /StrF /StdCF"),
        (Handler::R5, "/StmF /StdCF /StrF /StdCF"),
    ] {
        let name = |n: &str| format!("crypt_without_cf_{handler:?}_{n}");
        let pdf = open(
            &name("stdcf"),
            one_stream(
                Some(handler),
                entries,
                "/Filter /Crypt /DecodeParms << /Name /StdCF >>",
                CONTENT,
                Cipher::Rc4,
            ),
        );
        assert_eq!(read(&object(&pdf, 3)), Ok(CONTENT.to_vec()), "{handler:?}");
        let pdf = open(
            &name("identity"),
            one_stream(
                Some(handler),
                entries,
                "/Filter /Crypt /DecodeParms << /Name /Identity >>",
                CONTENT,
                Cipher::Stored,
            ),
        );
        assert_eq!(read(&object(&pdf, 3)), Ok(CONTENT.to_vec()), "{handler:?}");
        let pdf = open(
            &name("other"),
            one_stream(
                Some(handler),
                entries,
                "/Filter /Crypt /DecodeParms << /Name /Other >>",
                CONTENT,
                Cipher::Rc4,
            ),
        );
        assert!(refused(&object(&pdf, 3)), "{handler:?}");
    }
}

/// Rule 8: in a document that is not encrypted, a `/Crypt` filter does nothing,
/// whatever it names. `PyMuPDF` 1.28.2 reads the same.
#[test]
fn crypt_filters_in_a_document_not_encrypted_do_nothing() {
    for (i, dict) in [
        "/Filter /Crypt /DecodeParms << /Name /StdCF >>",
        "/Filter /Crypt /DecodeParms << /Name /Other >>",
        "/Filter [/AHx /Crypt] /DecodeParms [null << /Name /StdCF >>]",
    ]
    .into_iter()
    .enumerate()
    {
        let data = if dict.contains("AHx") {
            format!("{}>", hex(CONTENT)).into_bytes()
        } else {
            CONTENT.to_vec()
        };
        let pdf = open(
            &format!("crypt_not_encrypted_{i}"),
            one_stream(None, "", dict, &data, Cipher::Stored),
        );
        assert_eq!(read(&object(&pdf, 3)), Ok(CONTENT.to_vec()), "{dict}");
    }
}

/// Rule 10: an inline image is never decrypted, whatever its `/Crypt` filter names.
#[test]
fn an_inline_image_is_never_decrypted() {
    use hayro_syntax::content::ops::TypedInstruction;
    let content = b"BI /W 1 /H 1 /BPC 8 /CS /G /F [/Crypt /AHx] /DP [<< /Name /StdCF >> null] \
                    ID 41> EI";
    let pdf = open(
        "inline_image",
        file(
            Some(Handler::R4),
            AESV2,
            &[
                CATALOG,
                Obj::Plain("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                Obj::Plain("<< /Type /Page /MediaBox [0 0 100 100] /Contents 4 0 R >>"),
                Obj::Stream("", content, Cipher::Aes128),
            ],
        ),
    );
    let data = pdf.pages()[0].page_stream().unwrap().to_vec();
    assert_eq!(data, content);
    let mut ops = hayro_syntax::content::TypedIter::new(&data);
    let Some(TypedInstruction::InlineImage(image)) = ops.next() else {
        panic!("no inline image");
    };
    assert_eq!(image.0.decoded().unwrap().as_ref(), b"A");
}

/// Rule 9: the filters a stream lists include its `/Crypt` filters, which decoding
/// passes over; a stream with a `/Crypt` filter offers no incremental reader of its
/// decoded data (`Stream::decoded_reader`), though it offers the strict one.
#[test]
fn crypt_filters_are_listed_and_have_no_incremental_reader() {
    let pdf = open(
        "listed",
        one_stream(
            Some(Handler::R4),
            AESV2,
            "/Filter [/Crypt /FlateDecode] /DecodeParms [<< /Name /StdCF >> null]",
            &zlib(CONTENT),
            Cipher::Aes128,
        ),
    );
    let stream = object(&pdf, 3);
    assert_eq!(
        stream.filters().as_slice(),
        [Filter::Crypt, Filter::FlateDecode]
    );
    assert!(stream.decoded_reader().is_none());
    assert!(!stream.can_read_incrementally());
    assert!(
        stream
            .decoded_strict_reader(&Meter::default())
            .unwrap()
            .is_some()
    );
    // Without a /Crypt filter the reader is offered.
    let pdf = open(
        "listed_without_crypt",
        one_stream(
            Some(Handler::R4),
            AESV2,
            "/Filter /FlateDecode",
            &zlib(CONTENT),
            Cipher::Aes128,
        ),
    );
    assert!(object(&pdf, 3).decoded_reader().is_some());
    // Nor where the `/Crypt` filter is the only one.
    let pdf = open(
        "listed_alone",
        one_stream(
            Some(Handler::R4),
            AESV2,
            "/Filter /Crypt /DecodeParms << /Name /StdCF >>",
            CONTENT,
            Cipher::Aes128,
        ),
    );
    let stream = object(&pdf, 3);
    assert_eq!(read(&stream), Ok(CONTENT.to_vec()));
    assert!(stream.decoded_reader().is_none());
    assert!(!stream.can_read_incrementally());
}

/// Rule 10: of the streams of an encrypted document read as objects, only one whose
/// `/Type` is the string `(XRef)` is read as stored; a cross-reference stream, whose
/// `/Type` is the name `/XRef`, is decrypted, though the table is read from it as
/// stored when the document is opened. `PyMuPDF` 1.28.2 opens both documents and
/// decrypts both streams read as objects.
#[test]
fn only_a_stream_typed_by_the_string_xref_is_read_as_stored() {
    for (i, xref_type) in ["/XRef", "(XRef)"].into_iter().enumerate() {
        let pdf = open(
            &format!("xref_stream_{i}"),
            file_with_xref_stream(
                Handler::R4,
                AESV2,
                &[
                    CATALOG,
                    NO_PAGES,
                    Obj::Stream("", CONTENT, Cipher::Aes128),
                    Obj::Stream("/Type (XRef)", OTHER, Cipher::Stored),
                ],
                xref_type,
            ),
        );
        assert_eq!(read(&object(&pdf, 3)), Ok(CONTENT.to_vec()), "{xref_type}");
        assert_eq!(read(&object(&pdf, 4)), Ok(OTHER.to_vec()), "{xref_type}");
        let table = read(&object(&pdf, 5));
        if xref_type == "/XRef" {
            assert_eq!(table, decryption_failure());
        } else {
            assert_eq!(table.unwrap().len(), 6 * 7);
        }
    }
}

/// Rule 10: `/EncryptMetadata` false changes the file key only: the metadata stream is
/// decrypted by `/StmF` like any other, unless a `/Crypt` filter says otherwise.
/// `PyMuPDF` 1.28.2 reads the same: nothing of the metadata stored as is, the rest
/// whole.
#[test]
fn metadata_not_encrypted_needs_its_own_crypt_filter() {
    let pdf = open(
        "metadata_clear",
        file(
            Some(Handler::R4Clear),
            AESV2,
            &[
                Obj::Plain("<< /Type /Catalog /Pages 2 0 R /Metadata 3 0 R >>"),
                NO_PAGES,
                Obj::Stream("/Type /Metadata /Subtype /XML", CONTENT, Cipher::Stored),
                Obj::Stream(
                    "/Type /Metadata /Subtype /XML /Filter [/Crypt] /DecodeParms [<< /Name /Identity >>]",
                    CONTENT,
                    Cipher::Stored,
                ),
                Obj::Stream("", OTHER, Cipher::Aes128),
                Obj::Text(TEXT, Cipher::Aes128),
            ],
        ),
    );
    // Stored as is, it is no whole number of AES blocks.
    assert_eq!(read(&object(&pdf, 3)), decryption_failure());
    assert_eq!(object(&pdf, 3).decoded().unwrap().as_ref(), b"");
    assert_eq!(read(&object(&pdf, 4)), Ok(CONTENT.to_vec()));
    assert_eq!(read(&object(&pdf, 5)), Ok(OTHER.to_vec()));
    assert_eq!(text(&pdf, 6), TEXT);
}

/// Rule 10: `/EFF` is not read: an embedded file stream is decrypted by `/StmF` like
/// any other. `PyMuPDF` 1.28.2 reads the same.
#[test]
fn embedded_files_are_decrypted_by_the_stream_filter() {
    let entries = "/CF << /StdCF << /CFM /AESV2 >> /Mine << /CFM /V2 >> >> \
                   /StmF /StdCF /StrF /StdCF /EFF /Mine";
    let pdf = open(
        "embedded_file",
        file(
            Some(Handler::R4),
            entries,
            &[
                CATALOG,
                NO_PAGES,
                Obj::Stream("/Type /EmbeddedFile", CONTENT, Cipher::Aes128),
            ],
        ),
    );
    assert_eq!(read(&object(&pdf, 3)), Ok(CONTENT.to_vec()));
}

/// Rule 11: an encrypted stream without data reads as empty; AES data that are not a
/// whole number of blocks after the IV fail every strict reading and the raw data,
/// while the lenient decoding leaves the partial block out, and fails data shorter
/// than the IV. `PyMuPDF` 1.28.2 reads the empty streams the same, and the others as
/// empty.
#[test]
fn aes_data_must_be_whole_blocks() {
    let pdf = open(
        "aes_blocks",
        file(
            Some(Handler::R4),
            AESV2,
            &[
                CATALOG,
                NO_PAGES,
                Obj::Stream("", b"", Cipher::Stored),
                Obj::Stream(
                    "/Filter /Crypt /DecodeParms << /Name /StdCF >>",
                    b"",
                    Cipher::Stored,
                ),
                Obj::Stream("", &[7; 15], Cipher::Stored),
                Obj::Stream("", &[7; 40], Cipher::Stored),
            ],
        ),
    );
    for n in [3, 4] {
        assert_eq!(read(&object(&pdf, n)), Ok(vec![]), "{n}");
        assert_eq!(raw(&object(&pdf, n)), Ok(vec![]), "{n}");
    }
    for n in [5, 6] {
        let stream = object(&pdf, n);
        assert!(matches!(
            stream.decoded_strict(),
            Err(DecodeFailure::Decryption)
        ));
        assert_eq!(raw(&stream), decryption_failure());
        let lenient = stream.decoded().map(|d| d.len());
        match n {
            // Shorter than the IV.
            5 => assert!(matches!(lenient, Err(DecodeFailure::Decryption))),
            _ => assert_eq!(lenient.unwrap(), 16),
        }
    }
}

/// Rule 2: a string that does not decrypt — AES data shorter than the IV — reads as
/// stored. `PyMuPDF` 1.28.2 reads the same.
#[test]
fn a_string_that_does_not_decrypt_reads_as_stored() {
    let pdf = open(
        "short_string",
        file(
            Some(Handler::R4),
            AESV2,
            &[CATALOG, NO_PAGES, Obj::Text(b"short", Cipher::Stored)],
        ),
    );
    assert_eq!(text(&pdf, 3), b"short");
}

/// Rule 6: a page's content leaves out a part whose `/Crypt` filter fails; the strict
/// reading names it. `PyMuPDF` 1.28.2 fails the same part.
#[test]
fn a_page_leaves_out_a_part_whose_crypt_filter_fails() {
    let pdf = open(
        "page_content",
        file(
            Some(Handler::R4),
            AESV2,
            &[
                CATALOG,
                Obj::Plain("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                Obj::Plain(
                    "<< /Type /Page /MediaBox [0 0 100 100] /Contents [4 0 R 5 0 R 6 0 R] >>",
                ),
                Obj::Stream("", CONTENT, Cipher::Aes128),
                Obj::Stream(
                    "/Filter /Crypt /DecodeParms << /Name /Other >>",
                    OTHER,
                    Cipher::Aes128,
                ),
                Obj::Stream("/Filter /Crypt", OTHER, Cipher::Stored),
            ],
        ),
    );
    let page = &pdf.pages()[0];
    let mut expected = CONTENT.to_vec();
    expected.push(b' ');
    expected.extend(OTHER);
    expected.push(b' ');
    assert_eq!(page.page_stream().unwrap(), expected);
    assert!(matches!(
        page.try_page_stream(),
        Err(ContentError::Decode(Some(id), DecodeFailure::Decryption))
            if id == ObjectIdentifier::new(5, 0)
    ));
}

/// The filter key and filters [`FilterEntries::of`] reads from dictionary `dict`.
fn entries_of(dict: &str, extra: &[&str]) -> (String, Vec<Option<Filter>>) {
    let mut objects = vec![CATALOG, NO_PAGES, Obj::Plain(dict)];
    objects.extend(extra.iter().map(|e| Obj::Plain(e)));
    let pdf = open("entries", file(None, "", &objects));
    let dict: Dict<'_> = pdf.xref().get(ObjectIdentifier::new(3, 0)).unwrap();
    let entries = FilterEntries::of(&dict);
    let filters = entries.entries().map(|e| e.filter).collect();
    (String::from_utf8(entries.key.to_vec()).unwrap(), filters)
}

/// Filter rule 1: the filters are read from `/F` where it is a name; else from
/// `/Filter` where it is a name; else from `/F` where it is an array, else from
/// `/Filter` where it is one; a reference counts as what it leads to. A stream object
/// with `/F` names an external file and is not read, so `/F` gives filters to an inline
/// image only. `PyMuPDF` 1.28.2 reads the data of a stream object with `/F` (a file
/// specification) as stored.
#[test]
fn the_filters_are_read_from_f_or_filter() {
    use Filter::{AsciiHexDecode as Hex, FlateDecode as Fl};
    /// A dictionary, the objects from 4 on, the key read and the filters.
    type Case<'a> = (&'a str, &'a [&'a str], &'a str, &'a [Option<Filter>]);
    #[rustfmt::skip]
    let cases: &[Case<'_>] = &[
        ("<< /F /AHx /Filter /Fl >>", &[], "F", &[Some(Hex)]),
        ("<< /F [/AHx] /Filter /Fl >>", &[], "Filter", &[Some(Fl)]),
        ("<< /F [/AHx] /Filter [/Fl] >>", &[], "F", &[Some(Hex)]),
        ("<< /F 5 /Filter [/Fl] >>", &[], "Filter", &[Some(Fl)]),
        ("<< /F /Foo /Filter /Fl >>", &[], "F", &[None]),
        ("<< /F 4 0 R /Filter /Fl >>", &["/AHx"], "F", &[Some(Hex)]),
        ("<< /F 4 0 R /Filter /Fl >>", &["[/AHx]"], "Filter", &[Some(Fl)]),
        ("<< /F 4 0 R /Filter /Fl >>", &["5"], "Filter", &[Some(Fl)]),
        ("<< /Filter 4 0 R >>", &["[/AHx 5 0 R]", "/Fl"], "Filter", &[Some(Hex), Some(Fl)]),
        ("<< /Filter 5 >>", &[], "Filter", &[]),
        ("<< >>", &[], "Filter", &[]),
    ];
    for &(dict, extra, key, filters) in cases {
        assert_eq!(
            entries_of(dict, extra),
            (key.to_owned(), filters.to_vec()),
            "{dict} {extra:?}"
        );
    }
    let pdf = open(
        "external_file",
        one_stream(
            None,
            "",
            "/F (file.bin) /Filter /AHx",
            b"41>",
            Cipher::Stored,
        ),
    );
    assert!(
        pdf.xref()
            .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
            .is_none()
    );
    // An inline image takes /F before /Filter.
    use hayro_syntax::content::ops::TypedInstruction;
    let content = b"BI /W 1 /H 1 /BPC 8 /CS /G /F /AHx /Filter /A85 ID 41> EI";
    let mut ops = hayro_syntax::content::TypedIter::new(content);
    let Some(TypedInstruction::InlineImage(image)) = ops.next() else {
        panic!("no inline image");
    };
    assert_eq!(image.0.decoded().unwrap().as_ref(), b"A");
}

/// Filter rule 2: a filter is named by its full name or its abbreviation;
/// `/JBIG2Decode`, `/JPXDecode` and `/Crypt` have none. A name of no filter names none.
/// `PyMuPDF` 1.28.2 decodes the same where the data decode (the image filters are
/// given no data).
#[test]
fn filters_are_named_by_full_name_or_abbreviation() {
    #[rustfmt::skip]
    let cases: &[(&str, Vec<u8>, Option<Filter>)] = &[
        ("/ASCIIHexDecode", b"41>".to_vec(), Some(Filter::AsciiHexDecode)),
        ("/AHx", b"41>".to_vec(), Some(Filter::AsciiHexDecode)),
        ("/ASCII85Decode", b"5l~>".to_vec(), Some(Filter::Ascii85Decode)),
        ("/A85", b"5l~>".to_vec(), Some(Filter::Ascii85Decode)),
        ("/LZWDecode", lzw(b"A"), Some(Filter::LzwDecode)),
        ("/LZW", lzw(b"A"), Some(Filter::LzwDecode)),
        ("/FlateDecode", zlib(b"A"), Some(Filter::FlateDecode)),
        ("/Fl", zlib(b"A"), Some(Filter::FlateDecode)),
        ("/RunLengthDecode", vec![0, b'A', 128], Some(Filter::RunLengthDecode)),
        ("/RL", vec![0, b'A', 128], Some(Filter::RunLengthDecode)),
        ("/CCITTFaxDecode", vec![], Some(Filter::CcittFaxDecode)),
        ("/CCF", vec![], Some(Filter::CcittFaxDecode)),
        ("/DCTDecode", vec![], Some(Filter::DctDecode)),
        ("/DCT", vec![], Some(Filter::DctDecode)),
        ("/JBIG2Decode", vec![], Some(Filter::Jbig2Decode)),
        ("/JPXDecode", vec![], Some(Filter::JpxDecode)),
        ("/Crypt", b"A".to_vec(), Some(Filter::Crypt)),
        ("/JBIG2", b"A".to_vec(), None),
        ("/JPX", b"A".to_vec(), None),
        ("/Cr", b"A".to_vec(), None),
        ("/Fl2", b"A".to_vec(), None),
        ("/flatedecode", b"A".to_vec(), None),
    ];
    for (i, (name, data, filter)) in cases.iter().enumerate() {
        let pdf = open(
            &format!("filter_name_{i}"),
            one_stream(None, "", &format!("/Filter {name}"), data, Cipher::Stored),
        );
        let stream = object(&pdf, 3);
        assert_eq!(stream.filters().first().copied(), *filter, "{name}");
        if !data.is_empty() {
            assert_eq!(read(&stream), Ok(b"A".to_vec()), "{name}");
        }
    }
}

/// Filter rule 4: each entry of the filters is given with its parameters as written and
/// whether they are reached through a reference, so that a writer can copy the
/// filters it keeps.
#[test]
fn filter_entries_keep_what_is_written() {
    let objects = [
        CATALOG,
        NO_PAGES,
        Obj::Plain("<< /Filter [/Crypt 4 0 R /Foo] /DecodeParms [<< /Name /Identity >> 5 0 R] >>"),
        Obj::Plain("/Fl"),
        Obj::Plain("<< /Predictor 1 >>"),
        Obj::Plain("<< /Filter [/AHx] /DecodeParms 7 0 R >>"),
        Obj::Plain("[null]"),
    ];
    let pdf = open("written", file(None, "", &objects));
    let dict: Dict<'_> = pdf.xref().get(ObjectIdentifier::new(3, 0)).unwrap();
    let entries = FilterEntries::of(&dict);
    let shown: Vec<_> = entries
        .entries()
        .map(|e| {
            (
                e.filter,
                format!("{:?}", e.written),
                e.params_written.map(|p| format!("{p:?}")),
                e.params.len(),
                e.params_by_reference,
            )
        })
        .collect();
    assert_eq!(shown.len(), 3);
    assert_eq!(shown[0].0, Some(Filter::Crypt));
    assert_eq!(shown[0].3, 1);
    assert!(!shown[0].4);
    assert_eq!(shown[1].0, Some(Filter::FlateDecode));
    assert!(shown[1].1.contains("Ref"), "{:?}", shown[1]);
    assert_eq!(shown[1].3, 1);
    assert!(shown[1].4);
    assert_eq!(shown[2].0, None);
    assert_eq!(shown[2].2, None);
    assert_eq!(
        entries.applied().map(|(f, _)| f).collect::<Vec<_>>(),
        [Filter::Crypt, Filter::FlateDecode]
    );
    // Parameters of an array given by reference are all reached through it.
    let dict: Dict<'_> = pdf.xref().get(ObjectIdentifier::new(6, 0)).unwrap();
    let entry = FilterEntries::of(&dict).entries().next().unwrap();
    assert!(entry.params_by_reference);
    assert_eq!(entry.params.len(), 0);
}
