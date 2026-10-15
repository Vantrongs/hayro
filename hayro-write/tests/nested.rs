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
    let data = zlib(&zlib(&content));
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
    out.extend(
        format!(
            "4 0 obj\n<< /Filter [/FlateDecode /FlateDecode] /Length {} >>\nstream\n",
            data.len()
        )
        .bytes(),
    );
    out.extend_from_slice(&data);
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
/// written in memory at most 3 MiB beyond what the copy holds, the same for both, with
/// work proportional to what it decodes to.
#[test]
fn nested_filters_are_copied_in_memory_bounded_whatever_they_decode_to() {
    let sizes = [8 << 20, 32 << 20];
    let results = sizes.map(|len| {
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
        let beyond = allocations.peak.saturating_sub(written);
        eprintln!(
            "{len} bytes decoded twice: peak {}, {written} written, work {work}",
            allocations.peak
        );
        assert!(
            beyond < 3 << 20,
            "{len} bytes decoded: a peak of {} for {written} bytes written",
            allocations.peak
        );
        assert!(
            (2 * len..16 * len).contains(&work),
            "{len} bytes decoded twice with {work} bytes of work"
        );
        beyond
    });
    assert!(
        results[1] < results[0] + (256 << 10),
        "the memory beyond the copy grows with the data: {results:?}"
    );
}
