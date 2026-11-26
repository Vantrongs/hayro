//! Page tree and recovery inheritance.

use hayro_syntax::{
    Pdf,
    object::{ObjectIdentifier, Rect},
    page::Rotation,
};
use std::fmt::Write;

fn document_bytes(objects: &[&str], edit: impl FnOnce(&mut [Option<usize>])) -> Vec<u8> {
    let mut data = String::from("%PDF-1.7\n");
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(Some(data.len()));
        writeln!(data, "{} 0 obj\n{object}\nendobj", i + 1).unwrap();
    }
    let xref = data.len();
    let size = objects.len() + 1;
    writeln!(data, "xref\n0 {size}\n0000000000 65535 f ").unwrap();
    edit(&mut offsets);
    for offset in offsets {
        if let Some(offset) = offset {
            writeln!(data, "{offset:010} 00000 n ").unwrap();
        } else {
            writeln!(data, "0000000000 00000 f ").unwrap();
        }
    }
    writeln!(
        data,
        "trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF"
    )
    .unwrap();
    data.into_bytes()
}

const CATALOG: &str = "<< /Type /Catalog /Pages 2 0 R >>";

#[test]
fn repaired_tree_preserves_blank_pages_and_kids_order() {
    let data = document_bytes(
        &[
            CATALOG,
            "<< /Kids [5 0 R 3 0 R] >>",
            "<< /Type /Page /Contents [] /Rotate 90 >>",
            "<< /Type /Page /Contents [] >>",
            "<< /Type /Page /Rotate 180 >>",
            "null",
        ],
        |offsets| {
            offsets[1] = None;
            offsets[3] = None;
            *offsets[5].as_mut().unwrap() += 1;
        },
    );
    let pdf = Pdf::new(data).unwrap();
    let rotations: Vec<_> = pdf.pages().iter().map(|p| p.rotation()).collect();
    assert_eq!(rotations, [Rotation::Flipped, Rotation::Horizontal]);
}

mod allocations {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct CountingAllocator;

    thread_local! {
        static BYTES: Cell<usize> = const { Cell::new(0) };
        static FREED: Cell<usize> = const { Cell::new(0) };
    }

    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            BYTES.set(BYTES.get() + layout.size());
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            FREED.set(FREED.get().wrapping_add(layout.size()));
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[test]
    fn repaired_carrier_is_decoded_once_for_the_cache() {
        let allocated = |padding| {
            let member = format!("[0 0 47 18]{}", " ".repeat(padding));
            let pdf = Pdf::new(member_pdf(
                "<< /Kids [3 0 R] >>",
                "<< /Type /Page >>",
                &member,
                Carrier::BadOffset,
            ))
            .unwrap();
            let before = BYTES.get();
            let rect = pdf.xref().get::<Rect>(ObjectIdentifier::new(6, 0)).unwrap();
            assert_eq!(rect.width(), 47.0);
            BYTES.get() - before
        };
        // Repair scans the carrier once and the member lookup caches one decoded copy.
        let padding = 64 * 1024;
        assert_eq!(allocated(2 * padding) - allocated(padding), 2 * padding);
    }

    #[test]
    fn abandoned_carrier_retains_no_decoded_copy() {
        let retained = |padding| {
            let pdf = Pdf::new(compressed_pdf("/Kids [3 0 R]", false, true, padding)).unwrap();
            let before = (BYTES.get(), FREED.get());
            assert_eq!(
                pdf.xref()
                    .get::<Rect>(ObjectIdentifier::new(6, 0))
                    .unwrap()
                    .width(),
                47.0
            );
            (BYTES.get() - before.0) - (FREED.get() - before.1)
        };
        assert_eq!(retained(64 * 1024), retained(128 * 1024));
    }

    #[test]
    fn repair_during_filter_lookup_retains_one_decoded_carrier() {
        let retained = |padding| {
            let member = format!("[0 0 47 18]{}", " ".repeat(padding));
            let pdf = Pdf::new(member_pdf(
                "<< /Kids [3 0 R] >>",
                "<< >>",
                &member,
                Carrier::BadFilterOffset,
            ))
            .unwrap();
            let before = (BYTES.get(), FREED.get());
            let rect = pdf.xref().get::<Rect>(ObjectIdentifier::new(6, 0)).unwrap();
            assert_eq!(rect.width(), 47.0);
            (BYTES.get() - before.0) - (FREED.get() - before.1)
        };
        let padding = 64 * 1024;
        assert_eq!(retained(2 * padding) - retained(padding), padding);
    }
}

fn compressed_pdf(kids: &str, corrupt_page: bool, standalone: bool, padding: usize) -> Vec<u8> {
    let media_box = if standalone { "[0 0 9 18]" } else { "6 0 R" };
    let mut data = String::from("%PDF-1.7\n");
    let mut offsets = vec![0];
    for (i, body) in [
        CATALOG,
        &format!("<< /Type /Pages {kids} /MediaBox {media_box} >>"),
        "<< /Type /Page /Contents [] /Parent 2 0 R >>",
        "null",
        "<< /Type /Page /Contents [] /Parent 2 0 R >>",
    ]
    .iter()
    .enumerate()
    {
        offsets.push(data.len());
        writeln!(data, "{} 0 obj\n{body}\nendobj", i + 1).unwrap();
    }
    offsets[4] += usize::from(corrupt_page);
    let first_stream = data.len();
    for width in [11, 32] {
        let body = format!("6 0 [0 0 {width} 18]{}", " ".repeat(padding));
        writeln!(data, "7 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n{body}\nendstream\nendobj", body.len()).unwrap();
    }
    if standalone {
        writeln!(data, "6 0 obj\n[0 0 47 18]\nendobj").unwrap();
    }
    finish_compressed_pdf(data, offsets, first_stream + usize::from(standalone), &[])
}

fn finish_compressed_pdf(
    mut data: String,
    offsets: Vec<usize>,
    carrier: usize,
    extra: &[(u8, usize, u8)],
) -> Vec<u8> {
    let startxref = data.len();
    let mut entries = Vec::new();
    for (kind, value, index) in offsets
        .into_iter()
        .enumerate()
        .map(|(id, pos)| (u8::from(id != 0), pos, 0))
        .chain([(2, 7, 0), (1, carrier, 0), (1, startxref, 0)])
        .chain(extra.iter().copied())
    {
        entries.push(kind);
        entries.extend_from_slice(&u32::try_from(value).unwrap().to_be_bytes());
        entries.push(index);
    }
    writeln!(
        data,
        "8 0 obj\n<< /Type /XRef /Root 1 0 R /Size {} /W [1 4 1] /Length {} >>\nstream",
        entries.len() / 6,
        entries.len()
    )
    .unwrap();
    let mut data = data.into_bytes();
    data.extend(entries);
    data.extend(format!("\nendstream\nendobj\nstartxref\n{startxref}\n%%EOF\n").bytes());
    data
}

#[test]
fn carrier_repair_restarts_member_lookup() {
    let pdf = Pdf::new(compressed_pdf("/Kids [3 0 R 5 0 R]", false, true, 0)).unwrap();
    assert_eq!(pdf.pages()[0].media_box().width(), 9.0);
    for _ in 0..2 {
        assert_eq!(
            pdf.xref()
                .get::<Rect>(ObjectIdentifier::new(6, 0))
                .unwrap()
                .width(),
            47.0
        );
    }

    let (mut data, offsets) = member_pdf_prefix("<< /Kids [3 0 R] >>", "<< >>");
    let carrier = data.len();
    let header = "6 0 9 2 ";
    let body = format!("{header}0 [0 0 % endstream\n47 18]");
    writeln!(data, "7 0 obj\n<< /Type /ObjStm /N 2 /First {} /Length 6 0 R >>\nstream\n{body}\nendstream\nendobj\n6 0 obj\n{}\nendobj", header.len(), body.len()).unwrap();
    let pdf = Pdf::new(finish_compressed_pdf(
        data,
        offsets,
        carrier + 1,
        &[(2, 7, 1)],
    ))
    .unwrap();
    assert_eq!(
        pdf.xref().get::<usize>(ObjectIdentifier::new(6, 0)),
        Some(body.len())
    );
    assert_eq!(
        pdf.xref()
            .get::<Rect>(ObjectIdentifier::new(9, 0))
            .unwrap()
            .width(),
        47.0
    );
}

#[test]
fn member_read_restarts_after_indirect_element_repair() {
    let (mut data, offsets) = member_pdf_prefix("<< /Kids [] >>", "<< >>");
    let carrier = data.len();
    let body = "6 0 [0 0 9 0 R 18]";
    writeln!(
        data,
        "7 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n{body}\nendstream\nendobj",
        body.len()
    )
    .unwrap();
    let offset = data.len();
    data.push_str("9 0 obj\n11\nendobj\n6 0 obj\n[0 0 47 18]\nendobj\n");
    let pdf = Pdf::new(finish_compressed_pdf(
        data,
        offsets,
        carrier,
        &[(1, offset + 1, 0)],
    ))
    .unwrap();
    for _ in 0..2 {
        assert_eq!(
            pdf.xref()
                .get::<Rect>(ObjectIdentifier::new(6, 0))
                .unwrap()
                .width(),
            47.0
        );
    }
}

#[test]
fn refreshed_catalog_ignores_stream_header() {
    let pdf = Pdf::new(document_bytes(&[
        CATALOG, "<< /Kids [3 0 R] >>", "<< /Type /Page /Rotate 90 >>",
        "<< /Kids [] >>", "null\nendobj\n1 0 obj\n<< /Type /Catalog /Pages 4 0 R /Length 0 >>\nstream\n\nendstream"
    ], |offsets| *offsets[1].as_mut().unwrap() += 1)).unwrap();
    assert_eq!(pdf.pages().len(), 1);
    assert_eq!(pdf.pages()[0].rotation(), Rotation::Horizontal);
}

#[test]
fn repair_rereads_catalog_pages_reference() {
    let data = document_bytes(
        &[
            CATALOG,
            "<< >>",
            "<< /Type /Page /Contents [] /Rotate 90 >>",
            "<< /Type /Page /Rotate 180 >>",
            "<< /Kids [4 0 R 3 0 R] >>",
            "null\nendobj\n1 0 obj\n<< /Type /Catalog /Pages 5 0 R >>",
        ],
        |offsets| *offsets[5].as_mut().unwrap() += 1,
    );
    let pdf = Pdf::new(data).unwrap();
    let rotations: Vec<_> = pdf.pages().iter().map(|p| p.rotation()).collect();
    assert_eq!(rotations, [Rotation::Flipped, Rotation::Horizontal]);
}

#[test]
fn empty_kids_ignore_unlinked_pages() {
    for repaired in [false, true] {
        let pdf = Pdf::new(document_bytes(
            &[
                CATALOG,
                "<< /Type /Pages /Kids [] /Count 0 >>",
                "<< /Type /Page /Contents [] >>",
            ],
            |offsets| {
                if repaired {
                    *offsets[1].as_mut().unwrap() += 1;
                }
            },
        ))
        .unwrap();
        assert!(pdf.pages().is_empty(), "repaired = {repaired}");
    }
}

#[test]
fn repaired_tree_with_missing_kid_recovers_page() {
    for kids in ["[99 0 R]", "[--1]", "[-]"] {
        let root = format!("<< /Count 1 /Kids {kids} >>");
        let pdf = Pdf::new(document_bytes(
            &[
                CATALOG,
                "<< >>",
                "<< /Type /Page /Contents [] /Rotate 90 >>",
                &root,
                "null\nendobj\n1 0 obj\n<< /Type /Catalog /Pages 4 0 R >>",
            ],
            |offsets| *offsets[4].as_mut().unwrap() += 1,
        ))
        .unwrap();
        assert_eq!(pdf.pages().len(), 1, "{kids}");
        assert_eq!(pdf.pages()[0].rotation(), Rotation::Horizontal, "{kids}");
    }
}

enum Carrier {
    BadOffset,
    BadFilterOffset,
}

fn member_pdf_prefix(root: &str, page: &str) -> (String, Vec<usize>) {
    let mut data = String::from("%PDF-1.7\n");
    let mut offsets = vec![0];
    for (i, object) in [CATALOG, root, page, "null", "null"].iter().enumerate() {
        offsets.push(data.len());
        writeln!(data, "{} 0 obj\n{object}\nendobj", i + 1).unwrap();
    }
    (data, offsets)
}

fn member_pdf(root: &str, page: &str, member: &str, variant: Carrier) -> Vec<u8> {
    let (mut data, offsets) = member_pdf_prefix(root, page);
    let carrier = data.len();
    let (header, tail, count) = ("6 0 ", "", 1);
    let filter = if matches!(variant, Carrier::BadFilterOffset) {
        "/Filter 9 0 R"
    } else {
        ""
    };
    writeln!(data, "7 0 obj\n<< /Type /ObjStm /N {count} /First {} /Length {} {filter} >>\nstream\n{header}{member}{tail}\nendstream\nendobj", header.len(), header.len()+member.len()+tail.len()).unwrap();
    let extra = match variant {
        Carrier::BadFilterOffset => {
            let bad_offset = data.len() + 1;
            data.push_str("9 0 obj\n[]\nendobj\n");
            vec![(1, bad_offset, 0)]
        }
        Carrier::BadOffset => vec![],
    };
    finish_compressed_pdf(
        data,
        offsets,
        carrier + usize::from(matches!(variant, Carrier::BadOffset)),
        &extra,
    )
}
