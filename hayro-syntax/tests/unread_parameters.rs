//! A reading of a stream reads no key of its filters' parameters that it does not
//! use: not those of a filter it has no decoder for, nor any once the dictionary
//! leaves the data no decryption, and of the crypt filters' names none after the one
//! that decides that.

#![cfg(debug_assertions)]

use hayro_syntax::Pdf;
use hayro_syntax::object::{ObjectIdentifier, Stream};
use hayro_syntax::work::{BODIES_READ, Meter};

mod common;

const KEY: [u8; 32] = [7; 32];

/// A document whose object 3 is a stream with `dict` entries holding `data`, AES-256
/// encrypted if `encrypted`, and whose object 5 is an array of 64 Ki numbers, which
/// the parameters name where a reading must not read them.
fn file(dict: &str, data: &[u8], encrypted: bool) -> Pdf {
    let (data, encryption, trailer) = match encrypted {
        true => (
            common::aes256_cbc(KEY, data),
            common::aes256_encryption(KEY),
            common::aes256_trailer(4),
        ),
        false => (data.to_vec(), b"null".to_vec(), String::new()),
    };
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
        common::stream(dict, &data),
        encryption,
        format!("[{}]", "0 ".repeat(64 << 10)).into_bytes(),
    ];
    Pdf::new(common::file(&objects, &trailer)).expect("valid pdf")
}

/// What `read` gives of object 3 of `pdf`, its failure named, and the bytes of
/// bodies it read.
fn bodies_read<T, E: std::fmt::Debug>(
    pdf: &Pdf,
    read: impl FnOnce(&Stream<'_>) -> Result<T, E>,
) -> (Result<T, String>, usize) {
    let stream = pdf
        .xref()
        .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
        .unwrap();
    BODIES_READ.set(0);
    let read = read(&stream).map_err(|e| format!("{e:?}"));
    (read, BODIES_READ.get())
}

/// The strict reader, which has no decoder for image filters, reads none of their
/// parameters: the caller decodes whole then, which reads what it uses.
#[test]
fn the_strict_reader_reads_no_parameters_of_an_image_filter() {
    for params in ["<< /JBIG2Globals 5 0 R >>", "<< /Rows 5 0 R >>"] {
        let filter = match params.contains("JBIG2") {
            true => "/JBIG2Decode",
            false => "/CCITTFaxDecode",
        };
        let dict = format!("/Filter [/AHx {filter}] /DecodeParms [null {params}]");
        let pdf = file(&dict, b"00>", false);
        let (reader, read) = bodies_read(&pdf, |s| {
            s.decoded_strict_reader(&Meter::default())
                .map(|r| r.is_some())
        });
        assert_eq!((reader, read), (Ok(false), 0), "{dict}");
    }
}

/// Whether a stream reads incrementally, and its incremental reader, read the
/// parameters of no filter but a single Flate filter.
#[test]
fn an_incremental_reading_reads_no_parameters_of_more_filters() {
    let dict = "/Filter [/AHx /CCITTFaxDecode] /DecodeParms [null << /Rows 5 0 R >>]";
    let pdf = file(dict, b"00>", false);
    let (can, read) = bodies_read(&pdf, |s| Ok::<_, ()>(s.can_read_incrementally()));
    assert_eq!((can, read), (Ok(false), 0));
    let (reader, read) = bodies_read(&pdf, |s| Ok::<_, ()>(s.decoded_reader().is_some()));
    assert_eq!((reader, read), (Ok(false), 0));
}

/// A crypt filter that names a crypt filter after the first fails decryption: no
/// crypt filter's name after it is read, nor any filter's parameters, by any reading.
#[test]
fn no_parameters_are_read_past_a_misplaced_crypt_filter() {
    for dict in [
        "/Filter [/AHx /Crypt /Crypt] /DecodeParms [null << /Name /StdCF >> << /Name 5 0 R >>]",
        "/Filter [/AHx /Crypt /CCITTFaxDecode] /DecodeParms [null << /Name /StdCF >> \
         << /Rows 5 0 R >>]",
    ] {
        let pdf = file(dict, b"00>", true);
        let failure = Err("Decryption".to_owned());
        let (decoded, read) = bodies_read(&pdf, |s| s.decoded().map(|_| ()));
        assert_eq!((decoded, read), (failure.clone(), 0), "decoded {dict}");
        // The strict reader offers none for an image filter, before decrypting.
        let (strict, read) = bodies_read(&pdf, |s| {
            s.decoded_strict_reader(&Meter::default())
                .map(|r| r.is_some())
        });
        assert!(
            strict != Ok(true) && read == 0,
            "strict reader {dict}: {strict:?}, {read}"
        );
        let (raw, read) = bodies_read(&pdf, |s| s.try_raw_data().map(|_| ()));
        assert_eq!((raw, read), (failure, 0), "raw data {dict}");
    }
}
