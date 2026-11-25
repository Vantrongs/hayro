//! Page tree and recovery inheritance.

use hayro_syntax::{
    Pdf,
    object::{Name, ObjectIdentifier, Rect},
    page::{A4, Rotation},
};
use std::fmt::Write;

fn document(objects: &[&str]) -> Pdf {
    Pdf::new(document_bytes(objects, |_| {})).unwrap()
}

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
fn nearest_values_win_in_tree_and_recovery() {
    for kids in ["", "/Kids [3 0 R]"] {
        let root = format!(
            "<< /Type /Pages {kids} /MediaBox [0 0 32 18] /Rotate 90 /Resources << /Font << /A << >> >> >> >>"
        );
        let pdf = document(&[
            CATALOG,
            &root,
            "<< /Type /Pages /Parent 2 0 R /Kids [4 0 R 5 0 R] /CropBox [0 0 12 10] /Rotate 180 >>",
            "<< /Type /Page /Parent 3 0 R /Contents [] >>",
            "<< /Type /Page /Parent 3 0 R /Contents [] /MediaBox [0 0 8 6] /CropBox [0 0 5 4] /Rotate 270 /Resources << >> >>",
        ]);
        let pages = pdf.pages();
        assert_eq!(pages.len(), 2);
        let font = Name::new_unescaped(b"A");
        assert_eq!(pages[0].media_box().width(), 32.0);
        assert_eq!(pages[0].render_dimensions(), (12.0, 10.0));
        assert_eq!(pages[0].rotation(), Rotation::Flipped);
        assert!(pages[0].resources().get_font(&font).is_some());
        assert_eq!(pages[1].media_box().width(), 8.0);
        assert_eq!(pages[1].rotation(), Rotation::FlippedHorizontal);
        assert_eq!(pages[1].render_dimensions(), (4.0, 5.0));
        assert!(pages[1].resources().get_font(&font).is_none());
    }
}

#[test]
fn cyclic_parents_preserve_each_pages_nearest_values() {
    for reverse in [false, true] {
        let mut objects = [
            CATALOG,
            "<< /MediaBox [0 0 32 18] /Parent << /Rotate 90 /Parent 3 0 R >> >>",
            "<< /MediaBox [0 0 12 10] /Parent 2 0 R >>",
            "<< /Type /Page /Contents [] /Parent 2 0 R >>",
            "<< /Type /Page /Contents [] /Parent 3 0 R >>",
            "<< /Type /Page /Contents [] /Parent 6 0 R /MediaBox [0 0 5 4] >>",
            "<< /Type /Page /Contents [] /Parent << /Parent 99 0 R /MediaBox [0 0 7 6] >> >>",
        ];
        if reverse {
            objects.swap(3, 4);
        }
        let pdf = document(&objects);
        let sizes: Vec<_> = pdf.pages().iter().map(|p| p.render_dimensions()).collect();
        let mut expected = [(18.0, 32.0), (10.0, 12.0), (5.0, 4.0), (7.0, 6.0)];
        if reverse {
            expected.swap(0, 1);
        }
        assert_eq!(sizes, expected);
    }
}

#[test]
fn stream_parent_is_ignored() {
    let pdf = document(&[
        CATALOG,
        "<< /MediaBox [0 0 20 10] /Length 0 >>\nstream\n\nendstream",
        "<< /Type /Page /Contents [] /Parent 2 0 R >>",
    ]);
    assert_eq!(
        pdf.pages()[0].render_dimensions(),
        (A4.width() as f32, A4.height() as f32)
    );
}

#[test]
fn repair_during_scan_refreshes_all_pages() {
    for kids in ["", "/Kids [3 0 R 4 0 R 5 0 R]"] {
        let root = format!("<< /Type /Pages {kids} /MediaBox 6 0 R >>");
        let data = document_bytes(
            &[
                CATALOG,
                &root,
                "<< /Type /Page /Contents [] /Parent 2 0 R >>",
                "null",
                "<< /Type /Page /Contents [] /Parent 2 0 R >>",
                "[0 0 32 18]",
            ],
            |offsets| {
                *offsets[3].as_mut().unwrap() += 1;
                offsets[5] = None;
            },
        );
        let pdf = Pdf::new(data).unwrap();
        let sizes: Vec<_> = pdf.pages().iter().map(|p| p.render_dimensions()).collect();
        assert_eq!(sizes, [(32.0, 18.0); 2]);
    }
}

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

#[test]
fn unusable_attributes_preserve_inheritance() {
    let pdf = document(&[
        CATALOG,
        "<< /MediaBox [0 0 32 18] /CropBox [0 0 12 10] /Rotate 90 /Resources << /Font << /A << >> >> >> >>",
        "<< /Parent 2 0 R /MediaBox null /CropBox /Bad /Rotate 99 0 R /Resources false >>",
        "<< /Type /Page /Contents [] /Parent 3 0 R /MediaBox 99 0 R /CropBox null /Rotate /Bad /Resources 99 0 R >>",
        "<< /Type /Page /Contents [] /Parent 3 0 R /Rotate 17 >>",
    ]);
    let pages = pdf.pages();
    for page in pages.iter() {
        assert_eq!(page.media_box().width(), 32.0);
        assert_eq!(page.crop_box().height(), 10.0);
        assert!(
            page.resources()
                .get_font(&Name::new_unescaped(b"A"))
                .is_some()
        );
    }
    assert_eq!(pages[0].rotation(), Rotation::Horizontal);
    assert_eq!(pages[1].rotation(), Rotation::None);
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
        // Repair borrows the unfiltered carrier. Only the member lookup allocates a copy.
        let padding = 64 * 1024;
        assert_eq!(allocated(2 * padding) - allocated(padding), padding);
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

    #[test]
    fn rejected_resource_stream_allocation_is_independent_of_header_size() {
        for tree in [true, false] {
            for parents in [1, 32] {
                let allocated = |entries| {
                    let kids = (0..parents)
                        .map(|i| format!("{} 0 R ", 4 + 2 * i))
                        .collect::<String>();
                    let root = if tree {
                        format!("<< /Kids [{kids}] >>")
                    } else {
                        "<< >>".into()
                    };
                    let mut objects = vec![
                        CATALOG.to_owned(),
                        root,
                        format!(
                            "<< /Length 0 {} >>\nstream\n\nendstream",
                            dict_entries(entries)
                        ),
                    ];
                    for i in 0..parents {
                        objects.push(format!(
                            "<< /Type /Pages /Kids [{} 0 R] /Resources 3 0 R >>",
                            5 + 2 * i
                        ));
                        objects.push(format!("<< /Parent {} 0 R /Contents [] >>", 4 + 2 * i));
                    }
                    measure(&objects, |pdf| {
                        assert_eq!(pdf.pages().len(), parents);
                        assert!(
                            pdf.pages()
                                .iter()
                                .all(|p| p.resources().x_objects.is_empty())
                        );
                    })
                };
                assert_eq!(allocated(1), allocated(256));
            }
        }
    }

    fn allocated(tree: bool, pages: usize, depth: usize, entries: usize) -> usize {
        let resources = depth + pages + 2;
        let mut objects = vec![CATALOG.to_owned()];
        for i in 0..depth {
            let id = i + 2;
            let child_count = if i + 1 == depth { pages } else { 1 };
            let kids = if tree || i != 0 {
                let refs = (id + 1..=id + child_count)
                    .map(|id| format!("{id} 0 R "))
                    .collect::<String>();
                format!("/Kids [{refs}]")
            } else {
                String::new()
            };
            let parent = if i == 0 {
                String::new()
            } else {
                format!("/Parent {} 0 R", id - 1)
            };
            objects.push(format!(
                "<< /Type /Pages {kids} {parent} /Resources {resources} 0 R >>"
            ));
        }
        objects.extend(std::iter::repeat_n(
            format!("<< /Type /Page /Contents [] /Parent {} 0 R >>", depth + 1),
            pages,
        ));
        objects.push(format!("<< /XObject << {} >> >>", dict_entries(entries)));
        measure(&objects, |pdf| {
            assert_eq!(pdf.pages().len(), pages);
            assert!(
                pdf.pages()
                    .iter()
                    .all(|p| p.resources().x_objects.len() == entries)
            );
        })
    }

    #[test]
    fn shared_xobject_allocations_are_additive() {
        for tree in [false, true] {
            let growth = |pages, depth| {
                allocated(tree, pages, depth, 256) - allocated(tree, pages, depth, 1)
            };
            let single = growth(1, 1);
            assert!(single > 0);
            for (pages, depth) in [(100, 1), (1, 32), (100, 32)] {
                assert_eq!(
                    growth(pages, depth),
                    single,
                    "{pages} pages, depth {depth}, tree={tree}"
                );
            }
        }
    }

    fn measure(objects: &[String], check: impl FnOnce(&Pdf)) -> usize {
        let objects: Vec<_> = objects.iter().map(String::as_str).collect();
        let data = document_bytes(&objects, |_| {});
        let before = BYTES.get();
        check(&Pdf::new(data).unwrap());
        BYTES.get() - before
    }

    fn dict_entries(count: usize) -> String {
        let mut dict = String::new();
        for i in 0..count {
            write!(dict, "/X{i} 0 ").unwrap();
        }
        dict
    }

    #[test]
    fn shadowed_resource_categories_are_never_expanded() {
        for tree in [false, true] {
            for (ancestor, leaf, array) in [
                (
                    "/Resources << /XObject SHARED >>",
                    "/Resources << >>",
                    false,
                ),
                ("/Resources SHARED", "/Resources << >>", false),
                ("/MediaBox SHARED", "/MediaBox [0 0 10 20]", true),
            ] {
                let allocated = |count| {
                    let depth = 32;
                    let mut objects = vec![CATALOG.to_owned()];
                    let shared = format!("{} 0 R", depth + 3);
                    for i in 0..depth {
                        let kids = if tree || i != 0 {
                            format!("/Kids [{} 0 R]", i + 3)
                        } else {
                            String::new()
                        };
                        let parent = if i == 0 {
                            String::new()
                        } else {
                            format!("/Parent {} 0 R", i + 1)
                        };
                        let attr = ancestor.replace("SHARED", &shared);
                        objects.push(format!("<< /Type /Pages {kids} {parent} {attr} >>"));
                    }
                    objects.push(format!(
                        "<< /Type /Page /Parent {} 0 R /Contents [] {leaf} >>",
                        depth + 1
                    ));
                    objects.push(if array {
                        format!("[0 0 32 18 {}]", "0 ".repeat(count))
                    } else {
                        format!("<< {} >>", dict_entries(count))
                    });
                    measure(&objects, |pdf| {
                        assert_eq!(pdf.pages().len(), 1);
                        assert!(pdf.pages()[0].resources().x_objects.is_empty());
                        if array {
                            assert_eq!(pdf.pages()[0].render_dimensions(), (10.0, 20.0));
                        }
                    })
                };
                assert_eq!(allocated(512), allocated(1), "{ancestor}, tree={tree}");
            }
        }
    }

    #[test]
    fn rejected_parent_allocations_are_additive() {
        for stream in [false, true] {
            let allocated =
                |pages, count| {
                    let parent = if stream {
                        format!(
                            "<< /Length 0 {} >>\nstream\n\nendstream",
                            dict_entries(count)
                        )
                    } else {
                        format!("[{}]", "0 ".repeat(count))
                    };
                    let mut objects = vec![CATALOG.to_owned(), parent];
                    objects.extend(std::iter::repeat_n(
                        "<< /Contents [] /Parent 2 0 R >>".to_owned(),
                        pages,
                    ));
                    measure(&objects, |pdf| {
                        assert_eq!(pdf.pages().len(), pages);
                        assert!(pdf.pages().iter().all(|page| page.render_dimensions()
                            == (A4.width() as f32, A4.height() as f32)));
                    })
                };
            let growth = |pages| allocated(pages, 256) - allocated(pages, 1);
            assert!(growth(32) <= 2 * growth(1));
        }
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
fn repair_refreshes_compressed_rectangle() {
    for kids in ["", "/Kids [3 0 R 4 0 R 5 0 R]"] {
        for corrupt in [false, true] {
            let data = compressed_pdf(kids, corrupt, false, 0);
            let pdf = Pdf::new(data).unwrap();
            let expected = if corrupt { 32.0 } else { 11.0 };
            let sizes: Vec<_> = pdf.pages().iter().map(|p| p.render_dimensions()).collect();
            assert_eq!(sizes, [(expected, 18.0); 2], "{kids:?} corrupt={corrupt}");
        }
    }
}

#[test]
fn self_referencing_resources_respect_the_referring_page() {
    for kids in ["", "/Kids [3 0 R 4 0 R]"] {
        for self_id in [3, 4] {
            let page = format!(
                "<< /Type /Page /Parent 2 0 R /Contents [] /Font << /A << >> >> /Resources {self_id} 0 R >>"
            );
            let pdf = document(&[CATALOG, &format!("<< /Type /Pages {kids} >>"), &page, &page]);
            assert_eq!(pdf.pages().len(), 2);
            for (index, page) in pdf.pages().iter().enumerate() {
                assert_eq!(
                    page.resources()
                        .get_font(&Name::new_unescaped(b"A"))
                        .is_some(),
                    index + 3 != self_id
                );
            }
        }
    }
}

#[test]
fn stream_resources_preserve_inherited_fonts() {
    let pdf = document(&[
        CATALOG,
        "<< /Resources << /Font << /A << >> >> >> >>",
        "<< /Parent 2 0 R /Resources 5 0 R >>",
        "<< /Contents [] /Parent 3 0 R >>",
        "<< /Length 0 >>\nstream\n\nendstream",
    ]);
    assert!(
        pdf.pages()[0]
            .resources()
            .get_font(&Name::new_unescaped(b"A"))
            .is_some()
    );
}

#[test]
fn shared_resource_subcategories_keep_their_referring_context() {
    for kids in ["", "/Kids [3 0 R 4 0 R]"] {
        for target in [3, 4] {
            let page = "<< /Type /Page /Contents [] /Parent 2 0 R /A << >> /Resources 5 0 R >>";
            let pdf = document(&[
                CATALOG,
                &format!("<< /Type /Pages {kids} >>"),
                page,
                page,
                &format!("<< /Font {target} 0 R >>"),
            ]);
            assert_eq!(pdf.pages().len(), 2);
            for (index, page) in pdf.pages().iter().enumerate() {
                assert_eq!(
                    page.resources()
                        .get_font(&Name::new_unescaped(b"A"))
                        .is_some(),
                    index + 3 != target
                );
            }
        }
    }
}

#[test]
fn rejected_stream_resources_preserve_fonts_without_reading_length() {
    let kids = (6..22).map(|id| format!("{id} 0 R ")).collect::<String>();
    let root = format!("<< /Kids [{kids}] /Resources << /Font << /A << >> >> >> >>");
    for tail in [1, 4096] {
        let stream = format!("<< /Length 4 0 R >>\nstream\n{}", "x".repeat(tail));
        let mut objects = vec![CATALOG, &root, &stream, "0", "42"];
        objects.extend(std::iter::repeat_n(
            "<< /Parent 2 0 R /Resources 3 0 R >>",
            16,
        ));
        let pdf = Pdf::new(document_bytes(&objects, |offsets| {
            *offsets[3].as_mut().unwrap() += 1;
            offsets[4] = None;
        }))
        .unwrap();
        assert_eq!(pdf.pages().len(), 16);
        assert!(
            pdf.pages()
                .iter()
                .all(|p| p.resources().get_font(&Name::new_unescaped(b"A")).is_some())
        );
        assert_eq!(pdf.xref().get::<i32>(ObjectIdentifier::new(5, 0)), None);
    }
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

#[test]
fn invalid_body_triggers_page_recovery() {
    for invalid in [
        "garbage",
        "trueish",
        "falsehood",
        "nullable",
        "12bad",
        "12garbage",
        "[ 1 2",
    ] {
        let body = format!("{invalid}\nendobj\n3 0 obj\n<< /Contents [] /Rotate 90 >>");
        let pdf = document(&[CATALOG, "<< >>", &body]);
        assert_eq!(pdf.pages().len(), 1, "{invalid}");
        assert_eq!(pdf.pages()[0].rotation(), Rotation::Horizontal);
    }
}

enum Carrier {
    BadOffset,
    BadFilterOffset,
    Neighbor,
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
    let (header, tail, count) = match variant {
        Carrier::Neighbor => (format!("6 0 9 {} ", member.len() + 1), " stream", 2),
        _ => ("6 0 ".to_owned(), "", 1),
    };
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
        Carrier::Neighbor => vec![(2, 7, 1)],
        Carrier::BadOffset => vec![],
    };
    finish_compressed_pdf(
        data,
        offsets,
        carrier + usize::from(matches!(variant, Carrier::BadOffset)),
        &extra,
    )
}

#[test]
fn compressed_dictionary_stops_before_neighboring_member() {
    for (root, page, member) in [
        (
            "<< /Kids [3 0 R] >>",
            "<< /Resources 6 0 R >>",
            "<< /Font << /B << >> >> >>",
        ),
        (
            "<< >>",
            "<< /Contents [] /Parent 6 0 R >>",
            "<< /Resources << /Font << /B << >> >> >> >>",
        ),
        (
            "<< >>",
            "null",
            "<< /Contents [] /Resources << /Font << /B << >> >> >> >>",
        ),
    ] {
        let pdf = Pdf::new(member_pdf(root, page, member, Carrier::Neighbor)).unwrap();
        assert!(
            pdf.pages()[0]
                .resources()
                .get_font(&Name::new_unescaped(b"B"))
                .is_some()
        );
    }
}

#[test]
fn shadowed_media_box_does_not_repair_the_xref() {
    let data = document_bytes(
        &[
            CATALOG,
            "<< /Kids [3 0 R] /MediaBox 4 0 R >>",
            "<< /Contents [] /Parent 2 0 R /MediaBox [0 0 10 20] >>",
            "[0 0 32 18]",
            "42",
            "null\nendobj\n4 0 obj\n[ >>",
        ],
        |offsets| {
            offsets[3] = offsets[5].map(|pos| pos + "6 0 obj\nnull\nendobj\n".len());
            offsets[4] = None;
        },
    );
    let pdf = Pdf::new(data).unwrap();
    assert_eq!(pdf.pages()[0].render_dimensions(), (10.0, 20.0));
    let sentinel = ObjectIdentifier::new(5, 0);
    assert_eq!(pdf.xref().get::<i32>(sentinel), None);
    assert_eq!(
        pdf.xref()
            .get::<Rect>(ObjectIdentifier::new(4, 0))
            .unwrap()
            .width(),
        32.0
    );
    assert_eq!(pdf.xref().get::<i32>(sentinel), Some(42));
}
