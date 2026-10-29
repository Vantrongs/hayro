//! A page whose content has filters inside filters is copied in memory that does not
//! grow with what the filters make of its data, with work proportional to it.

use hayro_write::hayro_syntax::Pdf;
use hayro_write::hayro_syntax::work::{BODIES_READ, EXAMINED, TOLD};
use hayro_write::pdf_writer::Ref;
use hayro_write::{ChunkSettings, ExtractionQuery, extract};
use std::io::Write;

#[path = "../../hayro-syntax/tests/common/allocations.rs"]
mod allocations;

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// One page whose content, `/Filter [/FlateDecode /FlateDecode]`, decodes to `len`
/// bytes of drawing.
fn nested_flate_page(len: usize) -> Vec<u8> {
    let content: Vec<u8> = b"0 0 m 10 10 l S\n"
        .iter()
        .copied()
        .cycle()
        .take(len)
        .collect();
    page(
        "/Filter [/FlateDecode /FlateDecode]",
        &zlib(&zlib(&content)),
    )
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

/// The content decodes to 8 or 32 MiB, and its copy, a page and a form of it, is
/// written in memory of at most 1 MiB and a few times what the copy holds (its
/// compressor's output and the chunks it is put in), whatever it decodes to, with
/// work proportional to what it decodes to.
#[test]
fn nested_filters_are_copied_in_memory_bounded_whatever_they_decode_to() {
    for len in [8 << 20, 32 << 20] {
        let pdf = Pdf::new(nested_flate_page(len)).unwrap();
        let queries = [
            ExtractionQuery::new_page(0),
            ExtractionQuery::new_xobject(0),
        ];
        EXAMINED.set(0);
        TOLD.set(0);
        BODIES_READ.set(0);
        let (copy, allocations) = allocations::measure(|| {
            let mut next = Ref::new(1);
            extract(
                &pdf,
                Box::new(|| next.bump()),
                ChunkSettings::default(),
                &queries,
            )
            .unwrap()
        });
        let work = EXAMINED.get() + TOLD.get() + BODIES_READ.get();
        let written = copy.chunk.len();
        eprintln!(
            "{len} bytes decoded twice: peak {}, {written} written, work {work}",
            allocations.peak
        );
        assert!(
            allocations.peak < (1 << 20) + 4 * written,
            "{len} bytes decoded: a peak of {} for {written} bytes written",
            allocations.peak
        );
        assert!(
            (2 * len..16 * len).contains(&work),
            "{len} bytes decoded twice with {work} bytes of work"
        );
    }
}

/// The page copied as a page and as a form, and the work counted.
fn copied(file: Vec<u8>) -> (hayro_write::ExtractionResult, usize) {
    let pdf = Pdf::new(file).unwrap();
    let queries = [
        ExtractionQuery::new_page(0),
        ExtractionQuery::new_xobject(0),
    ];
    EXAMINED.set(0);
    TOLD.set(0);
    BODIES_READ.set(0);
    let mut next = Ref::new(1);
    let copy = extract(
        &pdf,
        Box::new(|| next.bump()),
        ChunkSettings::default(),
        &queries,
    )
    .unwrap();
    for root in &copy.root_refs {
        assert!(root.is_ok(), "{root:?}");
    }
    (copy, EXAMINED.get() + TOLD.get() + BODIES_READ.get())
}

/// A content of 100,000 filters over empty data is copied on a stack of 512 KiB, on
/// which a debug build copies a content of one filter.
#[test]
fn a_content_of_many_filters_is_copied_on_a_small_stack() {
    for filter in ["/AHx", "/Fl"] {
        let entries = format!("/Filter [{}]", [filter].repeat(100_000).join(" "));
        std::thread::Builder::new()
            .stack_size(512 << 10)
            .spawn(move || copied(page(&entries, b"")))
            .unwrap()
            .join()
            .unwrap();
    }
}

/// Content in Flate filters nested `n` deep, each with a checksum zlib rejects at its
/// end, is copied with work proportional to what the filters make.
#[test]
fn nested_bad_checksums_are_copied_with_work_linear_in_their_number() {
    let data = b"0 0 1 rg 20 20 160 160 re f\n".repeat(40);
    for n in [16, 32, 64, 128] {
        let mut encoded = data.clone();
        let mut made = 0;
        for _ in 0..n {
            made += encoded.len();
            encoded = zlib(&encoded);
            let last = encoded.len() - 1;
            encoded[last] ^= 1;
        }
        let entries = format!("/Filter [{}]", ["/FlateDecode"].repeat(n).join(" "));
        let (_, work) = copied(page(&entries, &encoded));
        eprintln!("{n} filters: work {work} for {made} bytes made");
        // Each filter takes its data and makes its output with zlib, then with the
        // permissive decoder, for the page and for the form, each content read twice:
        // once until the last filter restarts, then again.
        assert!(
            work < 24 * made + 64 * encoded.len() + 100_000,
            "{n} filters: work {work} for {made} bytes made"
        );
    }
}
