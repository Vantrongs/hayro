//! A table rebuilt because one of its entries leads to another object's header
//! keeps the objects it lists in object streams: an update that moves a page into an
//! object stream shows the page it moved, not the copy before it in the file, nor a
//! copy in another object stream, wherever that stream is in the file.

use hayro_syntax::Pdf;

mod common;

/// The two object streams of [`updated`].
#[derive(Clone, Copy)]
enum Order {
    /// The one the table lists the page in, 8, first.
    ListedFirst,
    /// The other, 10, first.
    ListedLast,
}

/// A page whose first revision, object 3 with the contents 4, is in the file, and
/// whose update, object 3 with the contents 5 in the object stream 8, is what the
/// table lists. The object stream 10 holds another object 3, with the contents 11,
/// which the table does not list, placed in the file by `order`. Where `stray`, the
/// table's entry 6 leads to the header of object 7, which rebuilds it.
fn updated(stray: bool, order: Order) -> Vec<u8> {
    let page = |contents: u32| {
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents {contents} 0 R >>")
    };
    let index = "3 0 ";
    let held = |contents: u32| {
        common::stream(
            &format!("/Type /ObjStm /N 1 /First {}", index.len()),
            format!("{index}{}", page(contents)).as_bytes(),
        )
    };
    let mut objects: Vec<(u32, Vec<u8>)> = vec![
        (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
        (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
        (3, page(4).into_bytes()),
        (4, common::stream("", b"(old) Tj")),
        (5, common::stream("", b"(new) Tj")),
        (7, b"<< >>".to_vec()),
        (11, common::stream("", b"(other) Tj")),
    ];
    let streams = [(8, held(5)), (10, held(11))];
    match order {
        Order::ListedFirst => objects.extend(streams),
        Order::ListedLast => objects.extend(streams.into_iter().rev()),
    }
    let mut out = b"%PDF-1.7\n".to_vec();
    // Each listed object: (type, field 2, field 3) of its row.
    let mut rows = std::collections::BTreeMap::new();
    for (number, body) in &objects {
        if *number != 3 {
            rows.insert(*number, (1_u8, out.len() as u32, 0_u16));
        }
        out.extend(format!("{number} 0 obj\n").bytes());
        out.extend(body);
        out.extend(b"\nendobj\n");
    }
    rows.insert(3, (2, 8, 0));
    if stray {
        rows.insert(6, rows[&7]);
    }
    let at = out.len();
    rows.insert(12, (1, at as u32, 0));
    let mut data = vec![];
    for number in 0..13 {
        let (kind, field, index) = rows.get(&number).copied().unwrap_or((0, 0, 0));
        data.push(kind);
        data.extend(field.to_be_bytes());
        data.extend(index.to_be_bytes());
    }
    out.extend(
        format!(
            "12 0 obj\n<< /Type /XRef /Size 13 /W [1 4 2] /Root 1 0 R /Length {} >>\nstream\n",
            data.len()
        )
        .bytes(),
    );
    out.extend(data);
    out.extend(format!("\nendstream\nendobj\nstartxref\n{at}\n%%EOF\n").bytes());
    out
}

fn contents(file: Vec<u8>) -> Vec<u8> {
    let pdf = Pdf::new(file).expect("readable");
    let pages = pdf.pages();
    assert_eq!(pages.len(), 1);
    pages[0].page_stream().expect("contents").to_vec()
}

#[test]
fn a_rebuilt_table_keeps_the_objects_it_lists_in_object_streams() {
    for order in [Order::ListedFirst, Order::ListedLast] {
        assert_eq!(contents(updated(false, order)), b"(new) Tj");
        assert_eq!(contents(updated(true, order)), b"(new) Tj");
    }
}
