//! A reading of a stream reads no filter's parameters that it does not use: not those
//! of a filter it has no decoder for, nor of one whose decoder reads none, nor any
//! for its decryption but a crypt filter's, nor any once the dictionary leaves the
//! data no decryption, and none after the crypt filter that makes decryption fail.

#![cfg(debug_assertions)]

use hayro_syntax::Pdf;
use hayro_syntax::object::{ObjectIdentifier, Stream};
use hayro_syntax::work::{BODIES_READ, Meter, OBJECT_STREAM_ENTRIES};

mod common;

const KEY: [u8; 32] = [7; 32];

/// A document whose object 3 is a stream with `dict` entries holding `data`, AES-256
/// encrypted if `encrypted`, whose object 5 is an array of 64 Ki numbers, and whose
/// object 7 is `<< /Predictor 1 >>` in an object stream: the parameters name 5 or 7
/// where a reading must not read them. Its one page, object 6, is empty.
fn file(dict: &str, data: &[u8], encrypted: bool) -> Pdf {
    let seal = |data: &[u8]| match encrypted {
        true => common::aes256_cbc(KEY, data),
        false => data.to_vec(),
    };
    let (encryption, trailer) = match encrypted {
        true => (common::aes256_encryption(KEY), common::aes256_trailer(4)),
        false => (b"null".to_vec(), String::new()),
    };
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [6 0 R] /Count 1 >>".to_vec(),
        common::stream(dict, &seal(data)),
        encryption,
        format!("[{}]", "0 ".repeat(64 << 10)).into_bytes(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] >>".to_vec(),
    ];
    let packed = [b"<< /Predictor 1 >>".to_vec()];
    let file = common::sealed_file_with_object_stream(&objects, &packed, &trailer, seal);
    Pdf::new(file).expect("valid pdf")
}

/// What `read` gives of object 3 of a new [`file`] of `dict`, `data`, `encrypted`, its
/// failure named, and the bytes of bodies and the entries of object streams it read.
fn reading<T, E: std::fmt::Debug>(
    dict: &str,
    data: &[u8],
    encrypted: bool,
    read: impl FnOnce(&Stream<'_>) -> Result<T, E>,
) -> (Result<T, String>, [usize; 2]) {
    let pdf = file(dict, data, encrypted);
    let stream = pdf
        .xref()
        .get::<Stream<'_>>(ObjectIdentifier::new(3, 0))
        .unwrap();
    BODIES_READ.set(0);
    OBJECT_STREAM_ENTRIES.set(0);
    let read = read(&stream).map_err(|e| format!("{e:?}"));
    (read, [BODIES_READ.get(), OBJECT_STREAM_ENTRIES.get()])
}

/// The strict reader, which has no decoder for image filters, reads none of their
/// parameters: the caller decodes whole then, which reads what it uses.
#[test]
fn the_strict_reader_reads_no_parameters_of_an_image_filter() {
    for (filter, params) in [
        ("/JBIG2Decode", "<< /JBIG2Globals 5 0 R >>"),
        ("/CCITTFaxDecode", "<< /Rows 5 0 R >>"),
        ("/CCITTFaxDecode", "7 0 R"),
    ] {
        let dict = format!("/Filter [/AHx {filter}] /DecodeParms [null {params}]");
        let (reader, read) = reading(&dict, b"00>", false, |s| {
            s.decoded_strict_reader(&Meter::default())
                .map(|r| r.is_some())
        });
        assert_eq!((reader, read), (Ok(false), [0, 0]), "{dict}");
    }
}

/// Whether a stream reads incrementally, and its incremental reader, read the
/// parameters of no filter but a single Flate filter.
#[test]
fn an_incremental_reading_reads_no_parameters_of_more_filters() {
    for dict in [
        "/Filter [/AHx /CCITTFaxDecode] /DecodeParms [null << /Rows 5 0 R >>]",
        "/Filter [/FlateDecode /CCITTFaxDecode] /DecodeParms [null 7 0 R]",
    ] {
        let (can, read) = reading(dict, b"00>", false, |s| {
            Ok::<_, ()>(s.can_read_incrementally())
        });
        assert_eq!((can, read), (Ok(false), [0, 0]), "{dict}");
        let (reader, read) = reading(dict, b"00>", false, |s| {
            Ok::<_, ()>(s.decoded_reader().is_some())
        });
        assert_eq!((reader, read), (Ok(false), [0, 0]), "{dict}");
    }
}

/// A crypt filter that names a crypt filter after the first fails decryption: no
/// crypt filter's name after it is read, nor any filter's parameters, by any reading,
/// whether they are given in place or by reference.
#[test]
fn no_parameters_are_read_past_a_misplaced_crypt_filter() {
    for dict in [
        "/Filter [/AHx /Crypt /Crypt] /DecodeParms [null << /Name /StdCF >> << /Name 5 0 R >>]",
        "/Filter [/AHx /Crypt /CCITTFaxDecode] /DecodeParms [null << /Name /StdCF >> \
         << /Rows 5 0 R >>]",
        "/Filter [/AHx /Crypt /FlateDecode] /DecodeParms [null << /Name /StdCF >> 7 0 R]",
        "/Filter [/AHx /Crypt /Crypt] /DecodeParms [null << /Name /StdCF >> 7 0 R]",
    ] {
        let failure = Err("Decryption".to_owned());
        let (decoded, read) = reading(dict, b"00>", true, |s| s.decoded().map(|_| ()));
        assert_eq!((decoded, read), (failure.clone(), [0, 0]), "decoded {dict}");
        let (strict, read) = reading(dict, b"00>", true, |s| {
            s.decoded_strict_reader(&Meter::default()).map(|_| ())
        });
        assert_eq!(
            (strict, read),
            (failure.clone(), [0, 0]),
            "strict reader {dict}"
        );
        let (raw, read) = reading(dict, b"00>", true, |s| s.try_raw_data().map(|_| ()));
        assert_eq!((raw, read), (failure, [0, 0]), "raw data {dict}");
    }
}

/// The raw data read the parameters of no filter but a crypt filter, which may name
/// the crypt filter that decrypts them, and those of none where the document does
/// not decrypt them.
#[test]
fn raw_data_read_no_parameters_of_a_decoding_filter() {
    for encrypted in [false, true] {
        for dict in [
            "/Filter /FlateDecode /DecodeParms 7 0 R",
            "/Filter [/AHx /FlateDecode] /DecodeParms [null 7 0 R]",
            "/Filter [/Crypt /FlateDecode] /DecodeParms [<< /Name /StdCF >> 7 0 R]",
        ] {
            let (raw, read) = reading(dict, b"00>", encrypted, |s| {
                s.try_raw_data().map(|d| d.into_owned())
            });
            assert_eq!(
                (raw, read),
                (Ok(b"00>".to_vec()), [0, 0]),
                "encrypted {encrypted}: {dict}"
            );
        }
    }
}
