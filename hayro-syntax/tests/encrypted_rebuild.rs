//! A damaged encrypted document is rebuilt from a scan of its file, and the objects
//! of its object streams, which only its decryptor reads, are found too.

use hayro_syntax::Pdf;

mod common;

const KEY: [u8; 32] = [7; 32];

/// An AES-256 encrypted document whose catalog is object 1 and whose page tree and
/// page are in the object stream 2, and whose `startxref` leads nowhere.
fn damaged_document() -> Vec<u8> {
    let held = [
        (4, b"<< /Type /Pages /Kids [5 0 R] /Count 1 >>".as_slice()),
        (5, b"<< /Type /Page /Parent 4 0 R /MediaBox [0 0 10 10] >>"),
    ];
    let mut offsets = String::new();
    let mut objects = Vec::new();
    for (number, object) in held {
        offsets.push_str(&format!("{number} {} ", objects.len()));
        objects.extend_from_slice(object);
        objects.push(b' ');
    }
    let mut plain = offsets.clone().into_bytes();
    plain.extend(&objects);
    let file = common::file(
        &[
            b"<< /Type /Catalog /Pages 4 0 R >>".to_vec(),
            common::stream(
                &format!("/Type /ObjStm /N 2 /First {}", offsets.len()),
                &common::aes256_cbc(KEY, &plain),
            ),
            common::aes256_encryption(KEY),
        ],
        &common::aes256_trailer(3),
    );
    let at = file.windows(10).rposition(|w| w == b"startxref\n").unwrap() + 10;
    let mut damaged = file[..at].to_vec();
    damaged.extend_from_slice(b"9\n%%EOF\n");
    damaged
}

#[test]
fn a_damaged_encrypted_document_finds_the_objects_of_its_object_streams() {
    let pdf = Pdf::new(damaged_document()).expect("rebuilt");
    assert_eq!(pdf.pages().len(), 1);
}
