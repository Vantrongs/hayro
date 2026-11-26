//! Round trips for page and form streams with nested filters.

use hayro_write::hayro_syntax::Pdf;
use hayro_write::pdf_writer::Ref;
use hayro_write::{ChunkSettings, ExtractionQuery, extract};
use std::io::Write;

use hayro_write::hayro_syntax::object::{ObjectIdentifier, Stream};

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// One page whose content, `/Filter [/FlateDecode /FlateDecode]`, decodes to `len`
/// bytes of drawing.
fn drawing(len: usize) -> Vec<u8> {
    b"0 0 m 10 10 l S\n"
        .iter()
        .copied()
        .cycle()
        .take(len)
        .collect()
}

/// One page whose content stream has dictionary entries `entries` and holds `data`.
fn page(entries: &str, data: &[u8]) -> Vec<u8> {
    let objects: [&[u8]; 3] = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 700] /Contents 4 0 R >>",
    ];
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n", i + 1).bytes());
        out.extend_from_slice(body);
        out.extend(b"\nendobj\n");
    }
    offsets.push(out.len());
    out.extend(format!("4 0 obj\n<< {entries} /Length {} >>\nstream\n", data.len()).bytes());
    out.extend_from_slice(data);
    out.extend(b"\nendstream\nendobj\n");
    let table = out.len();
    out.extend(b"xref\n0 5\n0000000000 65535 f \n");
    for at in offsets {
        out.extend(format!("{at:010} 00000 n \n").bytes());
    }
    out.extend(format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{table}\n%%EOF\n").bytes());
    out
}

#[test]
fn copies_nested_content() {
    for len in [8 << 20, 32 << 20] {
        let content = drawing(len);
        copied(
            page(
                "/Filter [/FlateDecode /FlateDecode]",
                &zlib(&zlib(&content)),
            ),
            &content,
        );
    }
}

fn copied(file: Vec<u8>, expected: &[u8]) {
    let pdf = Pdf::new(file).unwrap();
    let queries = [
        ExtractionQuery::new_page(0),
        ExtractionQuery::new_xobject(0),
    ];
    let mut next = Ref::new(1);
    let result = extract(
        &pdf,
        Box::new(|| next.bump()),
        ChunkSettings::default(),
        &queries,
    )
    .unwrap();
    assert!(result.root_refs.iter().all(Result::is_ok));
    let mut out = pdf_writer::Pdf::new();
    out.extend(&result.chunk);
    result.write_catalog(&mut out.catalog(next.bump()));
    let p = result.root_refs[0].unwrap();
    out.pages(result.page_tree_parent_ref).kids([p]).count(1);
    let pdf = Pdf::new(out.finish()).unwrap();
    assert_eq!(
        pdf.pages()[0].try_page_stream().unwrap().as_slice(),
        expected
    );
    let form = ObjectIdentifier::new(result.root_refs[1].unwrap().get() as u32, 0);
    assert_eq!(
        pdf.xref()
            .get::<Stream<'_>>(form)
            .unwrap()
            .decoded()
            .unwrap()
            .as_ref(),
        expected
    );
}

/// A content of 100,000 filters over empty data is copied on a stack of 512 KiB, on
/// which a debug build copies a content of one filter.
#[test]
fn a_content_of_many_filters_is_copied_on_a_small_stack() {
    for filter in ["/AHx", "/Fl"] {
        let entries = format!("/Filter [{}]", [filter].repeat(100_000).join(" "));
        std::thread::Builder::new()
            .stack_size(512 << 10)
            .spawn(move || copied(page(&entries, b""), b""))
            .unwrap()
            .join()
            .unwrap();
    }
}

#[test]
fn nested_bad_checksums_preserve_decoded_drawing() {
    let data = b"0 0 1 rg 20 20 160 160 re f\n".repeat(40);
    for n in [16, 32, 64, 128] {
        let mut encoded = data.clone();

        for _ in 0..n {
            encoded = zlib(&encoded);
            let last = encoded.len() - 1;
            encoded[last] ^= 1;
        }
        let entries = format!("/Filter [{}]", ["/FlateDecode"].repeat(n).join(" "));
        copied(page(&entries, &encoded), &data);
    }
}
