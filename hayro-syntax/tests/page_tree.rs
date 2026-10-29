//! Which pages a document lists, in what order, and what each page inherits from the
//! page tree, observed only through the crate's public API: `Pdf::pages`, the
//! accessors of `Page`, `Pages::unreadable`, `Page::unreadable` and `Inheritance`.
//! The rules these tests pin are stated in the doc of `hayro_syntax::page::Pages`
//! and of `hayro_syntax::page::Inheritance`.
//!
//! A page is shown as `id media crop rotation xobjects` ([`line`]): its object
//! (`N`, `N.G` for a generation other than 0, `d` for a dictionary written into a
//! /Kids array), its media box (`-` where none is found, which is A4), its crop box
//! (`=` where it is the media box), its rotation in degrees, and the names of the
//! `/XObject` category of its resources (`-` for none), which tell which /Resources
//! it inherits. With `PAGE_TREE_PDFS=<dir>` set, each file a test reads is written
//! there under the test's name, so that what `PyMuPDF` shows for it can be printed
//! in the same form; where a test says `PyMuPDF` 1.28.2 shows the same, it was
//! checked that way: page count, each page's object in order, and the entries each
//! page inherits, looked up once every page is loaded.

use hayro_syntax::Pdf;
use hayro_syntax::object::{Object, ObjectIdentifier, Rect};
use hayro_syntax::page::{A4, Inheritable, Inheritance, InheritanceError, Page, Rotation};
use std::time::Duration;

mod common;
use common::allocations::{Allocations, measure};
use common::cpu_time::thread_cpu_time;

const CATALOG: &str = "<< /Type /Catalog /Pages 2 0 R >>";

/// A file whose body holds `objects`, each a number and a body, in this order, and
/// whose table lists each at its place; object 1 is the catalog.
fn numbered(objects: &[(i32, &str)]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut entries = vec![];
    for (number, body) in objects {
        entries.push((*number, out.len()));
        out.extend(format!("{number} 0 obj\n{body}\nendobj\n").bytes());
    }
    entries.sort_unstable();
    let size = entries.last().map_or(1, |(number, _)| number + 1);
    let xref = out.len();
    out.extend(b"xref\n0 1\n0000000000 65535 f \n");
    for (number, offset) in entries {
        out.extend(format!("{number} 1\n{offset:010} 00000 n \n").bytes());
    }
    out.extend(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").bytes(),
    );
    out
}

/// A file of `objects` numbered from 1 in this order, the first being the catalog.
fn pdf(objects: &[&str]) -> Vec<u8> {
    let numbered_objects: Vec<(i32, &str)> = objects
        .iter()
        .enumerate()
        .map(|(i, body)| (i as i32 + 1, *body))
        .collect();
    numbered(&numbered_objects)
}

/// [`pdf`] of owned bodies.
fn pdf_of(objects: &[String]) -> Vec<u8> {
    let objects: Vec<&str> = objects.iter().map(String::as_str).collect();
    pdf(&objects)
}

/// A damaged file without a table, whose objects, each a number, a generation and a
/// body, are found by scanning it.
fn recovered(objects: &[(i32, i32, &str)]) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    for (number, generation, body) in objects {
        out.extend(format!("{number} {generation} obj\n{body}\nendobj\n").bytes());
    }
    out.extend(b"trailer\n<< /Root 1 0 R >>\n%%EOF\n");
    out
}

/// A /Pages node with `/Count count` listing `kids`.
fn pages_node(count: &str, kids: &str) -> String {
    format!("<< /Type /Pages /Count {count} /Kids [{kids}] >>")
}

/// Writes `file` under `name` where `PAGE_TREE_PDFS` names a directory.
fn dump(name: &str, file: &[u8]) {
    if let Some(dir) = std::env::var_os("PAGE_TREE_PDFS") {
        std::fs::write(std::path::Path::new(&dir).join(format!("{name}.pdf")), file).unwrap();
    }
}

fn open(name: &str, file: Vec<u8>) -> Pdf {
    dump(name, &file);
    Pdf::new(file).unwrap()
}

fn id(id: Option<ObjectIdentifier>) -> String {
    match id {
        None => "d".to_owned(),
        Some(id) if id.gen_number == 0 => id.obj_number.to_string(),
        Some(id) => format!("{}.{}", id.obj_number, id.gen_number),
    }
}

fn same(a: Rect, b: Rect) -> bool {
    [a.x0, a.y0, a.x1, a.y1] == [b.x0, b.y0, b.x1, b.y1]
}

fn rect(r: Rect) -> String {
    if same(r, A4) {
        "-".to_owned()
    } else {
        format!("[{} {} {} {}]", r.x0, r.y0, r.x1, r.y1)
    }
}

fn degrees(rotation: Rotation) -> u16 {
    match rotation {
        Rotation::None => 0,
        Rotation::Horizontal => 90,
        Rotation::Flipped => 180,
        Rotation::FlippedHorizontal => 270,
    }
}

/// A page as the module doc shows it.
fn line(page: &Page<'_>) -> String {
    let crop = if same(page.crop_box(), page.media_box()) {
        "=".to_owned()
    } else {
        rect(page.crop_box())
    };
    let mut names: Vec<String> = page
        .resources()
        .x_objects
        .keys()
        .map(|name| format!("/{}", name.as_str()))
        .collect();
    names.sort();
    let names = if names.is_empty() {
        "-".to_owned()
    } else {
        names.concat()
    };
    format!(
        "{} {} {crop} {} {names}",
        id(page.obj_id()),
        rect(page.media_box()),
        degrees(page.rotation())
    )
}

/// Each page of `file`, shown as the module doc says.
fn shown(name: &str, file: Vec<u8>) -> Vec<String> {
    open(name, file).pages().iter().map(line).collect()
}

/// Each page's object, as [`line`] shows it.
fn listed(name: &str, file: Vec<u8>) -> Vec<String> {
    open(name, file)
        .pages()
        .iter()
        .map(|page| id(page.obj_id()))
        .collect()
}

// ---------------------------------------------------------------------------------
// Which pages the page tree lists, and in what order.
// ---------------------------------------------------------------------------------

/// The pages are the page tree's leaves, depth first, each node's kids in the order
/// of its /Kids. `PyMuPDF` 1.28.2 shows the same.
#[test]
fn pages_are_listed_depth_first_in_kids_order() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [3 0 R 4 0 R 7 0 R] /Count 4 >>",
        "<< /Type /Page /Parent 2 0 R >>",
        "<< /Type /Pages /Parent 2 0 R /Kids [5 0 R 6 0 R] /Count 2 >>",
        "<< /Type /Page /Parent 4 0 R >>",
        "<< /Type /Page /Parent 4 0 R >>",
        "<< /Type /Page /Parent 2 0 R >>",
    ]);
    assert_eq!(listed("depth_first", file), ["3", "5", "6", "7"]);
}

/// A /Kids entry is a node, whose kids are listed in its place, when its /Type is
/// the name /Pages, read through a reference if need be; when it has no /Type (or a
/// direct `null` one), when it has /Kids and no `/MediaBox`, a direct `null` counting
/// as no entry and anything else, a reference to nothing included, as an entry.
/// Any other entry is a page, whatever its /Kids: /Type /Page, another name, or a
/// /Type that is no name. A node's /Kids that is no array, or a reference to
/// nothing, lists nothing; a stream is read as its dictionary. `PyMuPDF` 1.28.2
/// shows the same.
#[test]
fn a_kid_is_a_node_by_its_type_or_else_by_its_kids_and_media_box() {
    let page = "<< /Type /Page >>";
    let file = numbered(&[
        (1, CATALOG),
        (
            2,
            "<< /Type /Pages /Count 13 /Kids [10 0 R 12 0 R 14 0 R 16 0 R 18 0 R 20 0 R \
             22 0 R 24 0 R 26 0 R 28 0 R 30 0 R 32 0 R 34 0 R 35 0 R 36 0 R 37 0 R] >>",
        ),
        (10, "<< /Kids [11 0 R] /Count 1 >>"),
        (11, page),
        (12, "<< /Kids [13 0 R] /Count 1 /MediaBox [0 0 300 300] >>"),
        (13, page),
        (14, "<< /Kids [15 0 R] /Count 1 /MediaBox null >>"),
        (15, page),
        (16, "<< /Type null /Kids [17 0 R] /Count 1 >>"),
        (17, page),
        (
            18,
            "<< /Type /Pages /Kids [19 0 R] /Count 1 /MediaBox [0 0 300 300] >>",
        ),
        (19, page),
        (20, "<< /Type /Page /Kids [21 0 R] /Count 1 >>"),
        (21, page),
        (22, "<< /Type /Foo /Kids [23 0 R] /Count 1 >>"),
        (23, page),
        (24, "<< /Type 40 0 R /Kids [25 0 R] /Count 1 >>"),
        (25, page),
        (26, "<< /Type 5 /Kids [27 0 R] /Count 1 >>"),
        (27, page),
        (28, "<< /Type /Pages /Kids 42 0 R /Count 1 >>"),
        (29, page),
        (30, "<< /Kids [31 0 R] /Count 1 /MediaBox 41 0 R >>"),
        (31, page),
        (32, "<< /Kids null /Count 1 >>"),
        (34, "<< /Kids 41 0 R /Count 0 >>"),
        (35, "<< /Type /Pages /Kids << /A 1 >> /Count 0 >>"),
        (36, "<< /Type /Pages /Count 0 >>"),
        (
            37,
            "<< /Type /Pages /Kids [38 0 R] /Count 1 /Length 0 >>\nstream\n\nendstream",
        ),
        (38, page),
        (40, "/Pages"),
        (42, "[29 0 R]"),
    ]);
    assert_eq!(
        listed("kid_types", file),
        [
            "11", "12", "15", "17", "19", "20", "22", "25", "26", "29", "30", "32", "38"
        ]
    );
}

/// /Count is not read: a missing, low, high, negative or fractional /Count, at the
/// root or inside the tree, hides no page the /Kids list and adds none. A deliberate
/// difference: `PyMuPDF` 1.28.2 numbers pages by /Count, and lists
/// no page for a missing or zero /Count, the first for /Count 1, three of which the
/// third fails for /Count 2.5, refuses the file for /Count -1 or 5, and lists pages
/// 4 and 7 where the inner /Count is low.
#[test]
fn count_is_not_read() {
    let page = "<< /Type /Page /MediaBox [0 0 100 100] >>";
    let small = "<< /Type /Page /MediaBox [0 0 200 100] >>";
    for (i, count) in [
        "",
        "/Count 0",
        "/Count 1",
        "/Count -1",
        "/Count 5",
        "/Count 2.5",
    ]
    .into_iter()
    .enumerate()
    {
        let tree = format!("<< /Type /Pages {count} /Kids [3 0 R 4 0 R] >>");
        assert_eq!(
            listed(&format!("count_{i}"), pdf(&[CATALOG, &tree, page, small])),
            ["3", "4"],
            "{count}"
        );
    }
    // The first node's /Count 1 is lower than its kids.
    let tree = pages_node("2", "3 0 R 5 0 R");
    let low = "<< /Type /Pages /Kids [4 0 R 6 0 R] /Count 1 >>";
    let untyped = "<< /Kids [7 0 R] /Count 1 >>";
    assert_eq!(
        listed(
            "count_inner",
            pdf(&[CATALOG, &tree, low, page, untyped, small, page])
        ),
        ["4", "6", "7"]
    );
}

/// A /Kids entry that is no dictionary — a reference to nothing, to a number, or a
/// number, name, string, array or `null` written into the array — is a page with no
/// entries, listed in its place: its object is the one referred to, or none. A
/// dictionary or stream written into the array is a page without an object. A page
/// with no entries inherits what the nodes listing it hold, here nothing. `PyMuPDF`
/// 1.28.2 shows the same, except that it fails to load the page of the `null`.
#[test]
fn a_kid_that_is_no_dictionary_is_an_empty_page() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Count 11 /Kids [3 0 R 9 0 R 5 0 R 7 /N (s) [1] null \
         << /Type /Page /MediaBox [0 0 50 50] >> 4 0 R 6 0 R] >>",
        "<< /Type /Page /MediaBox [0 0 100 100] >>",
        "<< /Type /Foo /MediaBox [0 0 300 100] >>",
        "42",
        "<< /Type /Page /MediaBox [0 0 70 70] /Length 0 >>\nstream\n\nendstream",
        // Objects no page refers to, so that `PyMuPDF` takes /Count 11 for valid.
        "null",
        "null",
        "null",
        "null",
        "null",
        "null",
    ]);
    let pdf = open("kids_no_dicts", file);
    let lines: Vec<String> = pdf.pages().iter().map(line).collect();
    assert_eq!(
        lines,
        [
            "3 [0 0 100 100] = 0 -",
            "9 - = 0 -",
            "5 - = 0 -",
            "d - = 0 -",
            "d - = 0 -",
            "d - = 0 -",
            "d - = 0 -",
            "d - = 0 -",
            "d [0 0 50 50] = 0 -",
            "4 [0 0 300 100] = 0 -",
            "6 [0 0 70 70] = 0 -",
        ]
    );
    let empty: Vec<bool> = pdf
        .pages()
        .iter()
        .map(|page| page.raw().is_empty())
        .collect();
    assert_eq!(
        empty,
        [
            false, true, true, true, true, true, true, true, false, false, false
        ]
    );
}

/// A page whose entries hold a token that is no number is still a page, with its
/// dictionary. `PyMuPDF` 1.28.2 lists the same pages.
#[test]
fn a_page_with_a_bad_number_is_kept() {
    let tree = pages_node("3", "3 0 R 4 0 R 5 0 R");
    let page = "<< /Type /Page /MediaBox [0 0 100 100] >>";
    let bad = "<< /Type /Page /MediaBox [0 0 1e3 2e3] /Rotate 9x >>";
    let pdf = open("bad_number", pdf(&[CATALOG, &tree, page, bad, page]));
    let ids: Vec<String> = pdf.pages().iter().map(|page| id(page.obj_id())).collect();
    assert_eq!(ids, ["3", "4", "5"]);
    assert!(pdf.pages()[1].raw().contains_key(b"MediaBox"));
}

/// A page is listed each time a node lists it, in one /Kids or in several; a node
/// listed by two nodes lists its pages under each. `PyMuPDF` 1.28.2 shows the same.
#[test]
fn a_page_listed_twice_is_two_pages() {
    let page = "<< /Type /Page /MediaBox [0 0 100 100] >>";
    let tree = pages_node("2", "3 0 R 3 0 R");
    assert_eq!(
        listed("page_twice", pdf(&[CATALOG, &tree, page])),
        ["3", "3"]
    );
    let tree = pages_node("2", "3 0 R 4 0 R");
    let node = pages_node("1", "5 0 R");
    assert_eq!(
        listed("page_under_two", pdf(&[CATALOG, &tree, &node, &node, page])),
        ["5", "5"]
    );
}

/// A node is read the first time the walk meets it; met again — listed twice, in a
/// cycle back to an ancestor, or listing itself — it lists nothing more, and the
/// walk goes on after it. A deliberate difference: `PyMuPDF` 1.28.2 numbers pages
/// by /Count, so it fails to load the page after the cycle and the pages of the
/// node listing itself, and lists the page of the subtree listed twice twice.
#[test]
fn a_node_met_again_lists_nothing() {
    let page = "<< /Type /Page /MediaBox [0 0 100 100] >>";
    let small = "<< /Type /Page /MediaBox [0 0 200 100] >>";
    // A cycle back to the root, then the page after it.
    let tree = pages_node("2", "3 0 R 4 0 R 5 0 R");
    let cycle = pages_node("1", "2 0 R");
    assert_eq!(
        listed("node_cycle", pdf(&[CATALOG, &tree, page, &cycle, small])),
        ["3", "5"]
    );
    // A subtree the root lists twice.
    let tree = pages_node("2", "3 0 R 3 0 R");
    let node = pages_node("1", "4 0 R");
    assert_eq!(
        listed("node_twice", pdf(&[CATALOG, &tree, &node, page])),
        ["4"]
    );
    // A node listing itself, then a page.
    let tree = pages_node("2", "3 0 R");
    let node = pages_node("2", "3 0 R 4 0 R");
    assert_eq!(
        listed("node_itself", pdf(&[CATALOG, &tree, &node, page])),
        ["4"]
    );
}

/// Nodes that share one /Kids array each list its pages; a node in it met again
/// lists nothing, whether it is an object or a dictionary written into the array,
/// which is one node wherever the array is read from. `PyMuPDF` 1.28.2 shows the
/// same for the first and the last file; the second has no /Count it could number
/// pages by.
#[test]
fn nodes_sharing_a_kids_array_each_list_its_pages() {
    let page = "<< /Type /Page /MediaBox [0 0 100 100] >>";
    // Two nodes list array 5.
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 4 >>",
        "<< /Type /Pages /Kids 5 0 R /Count 2 >>",
        "<< /Type /Pages /Kids 5 0 R /Count 2 >>",
        "[6 0 R 7 0 R]",
        page,
        page,
    ]);
    assert_eq!(listed("shared_kids", file), ["6", "7", "6", "7"]);
    // The root and nodes 4 and 5 list array 3, which lists those nodes and page 6:
    // page 6 is a page under each.
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids 3 0 R >>",
        "[4 0 R 5 0 R 6 0 R]",
        "<< /Type /Pages /Kids 3 0 R >>",
        "<< /Type /Pages /Kids 3 0 R >>",
        page,
    ]);
    assert_eq!(listed("shared_kids_nested", file), ["6", "6", "6"]);
    // Array 3 holds two nodes written into it that list array 4, whose two nodes
    // list array 5, which lists page 6: under each of the two nodes of array 4.
    let node = |kids: i32| format!("<< /Type /Pages /Kids {kids} 0 R /Count 1 >>");
    let file = pdf_of(&[
        CATALOG.to_owned(),
        "<< /Type /Pages /Kids 3 0 R /Count 2 >>".to_owned(),
        format!("[{} {}]", node(4), node(4)),
        format!("[{} {}]", node(5), node(5)),
        "[6 0 R]".to_owned(),
        page.to_owned(),
    ]);
    assert_eq!(listed("shared_kids_direct", file), ["6", "6"]);
}

/// The catalog's /Pages is the root, a reference or written into the catalog; a
/// root that is no node is the one page. A root with no entries is a page, and so
/// the document's only one. `PyMuPDF` 1.28.2 reads the tree written into the
/// catalog the same, and lists no page where the root is a page or empty.
#[test]
fn the_root_is_the_catalogs_pages() {
    let page = "<< /Type /Page /MediaBox [0 0 100 100] >>";
    let catalog = "<< /Type /Catalog /Pages << /Type /Pages /Kids [3 0 R] /Count 1 >> >>";
    assert_eq!(
        listed("root_in_catalog", pdf(&[catalog, "null", page])),
        ["3"]
    );
    assert_eq!(
        shown("root_is_page", pdf(&[CATALOG, page])),
        ["2 [0 0 100 100] = 0 -"]
    );
    let content = "<< /Length 0 >>\nstream\n\nendstream";
    let with_contents = "<< /Type /Page /MediaBox [0 0 100 100] /Contents 4 0 R >>";
    assert_eq!(
        shown(
            "root_empty",
            pdf(&[CATALOG, "<< >>", with_contents, content])
        ),
        ["2 - = 0 -"]
    );
}

/// Where the catalog has no /Pages, or it is no dictionary or a reference to
/// nothing, or the tree lists no page, every dictionary of the file that has
/// /Contents (`null` included) and either no /Type, a /Type that is no name, or
/// /Type /Page is a page, in the order of the objects in the file, not of their
/// numbers. A stream is none. Such a page inherits through its /Parent alone.
/// `PyMuPDF` 1.28.2 lists no page for these files: finding pages without the tree
/// is this crate's own.
#[test]
fn without_a_page_tree_listing_a_page_every_page_object_is_a_page() {
    let content = "<< /Length 0 >>\nstream\n\nendstream";
    for (i, catalog) in [
        "<< /Type /Catalog >>",
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Catalog /Pages 99 0 R >>",
        "<< /Type /Catalog /Pages 10 0 R >>",
    ]
    .into_iter()
    .enumerate()
    {
        let file = numbered(&[
            (1, catalog),
            (2, "<< /Type /Pages /Kids [] /Count 0 >>"),
            (
                7,
                "<< /Type /Page /Parent 6 0 R /Contents 9 0 R /MediaBox null >>",
            ),
            (3, "<< /Contents null >>"),
            (
                6,
                "<< /Type /Pages /MediaBox [0 0 300 300] /Rotate 90 /Kids [3 0 R] >>",
            ),
            (4, "<< /Type 5 /Contents 9 0 R >>"),
            (
                5,
                "<< /Type /Annot /Subtype /Text /Contents (not a page) >>",
            ),
            (
                8,
                "<< /Type /Page /Contents 9 0 R /Length 0 >>\nstream\n\nendstream",
            ),
            (9, content),
            (10, "42"),
        ]);
        assert_eq!(
            shown(&format!("no_tree_{i}"), file),
            ["7 [0 0 300 300] = 90 -", "3 - = 0 -", "4 - = 0 -"],
            "{catalog}"
        );
    }
    // Neither a tree that lists no page nor a file without page objects has pages.
    let tree = pages_node("1", "");
    assert!(open("no_pages", pdf(&[CATALOG, &tree])).pages().is_empty());
    let page = "<< /Type /Page /MediaBox [0 0 100 100] >>";
    assert!(
        open("no_page_objects", pdf(&["<< /Type /Catalog >>", page]))
            .pages()
            .is_empty()
    );
}

/// Entries of a damaged table that lead to one page object are each a page, where
/// no page tree lists a page, as is the entry of the object's own number; the order
/// among entries that lead to one place in the file is not pinned. `PyMuPDF` 1.28.2
/// lists no page.
#[test]
fn without_a_page_tree_each_entry_leading_to_a_page_is_a_page() {
    let file = common::file_with_aliases(
        vec![
            b"<< /Type /Catalog >>".to_vec(),
            b"<< /Type /Page /MediaBox [0 0 100 100] /Contents 2 0 R >>".to_vec(),
        ],
        &[2, 2],
    );
    let mut ids = listed("aliased_no_tree", file);
    ids.sort();
    assert_eq!(ids, ["2", "3", "4"]);
}

/// Where a damaged file holds two generations of a node, each is a node: each lists
/// its own page, which inherits its own node's box and resources through its
/// /Parent, through the node listing it, or, where its /Parent is itself, through
/// the repaired /Parent entries. `PyMuPDF` 1.28.2 keeps the later generation only,
/// and lists its page under both.
#[test]
fn two_generations_of_a_node_are_two_nodes() {
    let node = |page: i32, size: i32, name: &str| {
        format!(
            "<< /Type /Pages /Kids [{page} 0 R] /Count 1 /MediaBox [0 0 {size} {size}] \
             /Resources << /XObject << /{name} 9 0 R >> >> >>"
        )
    };
    let (first, second) = (node(4, 300, "A"), node(5, 200, "B"));
    for (i, parents) in [["3 0 R", "3 1 R"], ["", ""], ["4 0 R", "5 0 R"]]
        .into_iter()
        .enumerate()
    {
        let page = |parent: &str| match parent {
            "" => "<< /Type /Page >>".to_owned(),
            parent => format!("<< /Type /Page /Parent {parent} >>"),
        };
        let (page_4, page_5) = (page(parents[0]), page(parents[1]));
        let file = recovered(&[
            (1, 0, CATALOG),
            (2, 0, "<< /Type /Pages /Kids [3 0 R 3 1 R] /Count 2 >>"),
            (3, 0, &first),
            (3, 1, &second),
            (4, 0, &page_4),
            (5, 0, &page_5),
        ]);
        assert_eq!(
            shown(&format!("generations_node_{i}"), file),
            ["4 [0 0 300 300] = 0 /A", "5 [0 0 200 200] = 0 /B"],
            "{parents:?}"
        );
    }
}

/// Two generations of a page are two pages, each with its own box and resources.
/// `PyMuPDF` 1.28.2 keeps the later generation only, and lists it twice.
#[test]
fn two_generations_of_a_page_are_two_pages() {
    let page = |size: i32, name: &str| {
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {size} {size}] \
             /Resources << /XObject << /{name} 9 0 R >> >> >>"
        )
    };
    let (first, second) = (page(300, "A"), page(200, "B"));
    let file = recovered(&[
        (1, 0, CATALOG),
        (2, 0, "<< /Type /Pages /Kids [3 0 R 3 1 R] /Count 2 >>"),
        (3, 0, &first),
        (3, 1, &second),
    ]);
    assert_eq!(
        shown("generations_page", file),
        ["3 [0 0 300 300] = 0 /A", "3.1 [0 0 200 200] = 0 /B"]
    );
}

// ---------------------------------------------------------------------------------
// What each page inherits.
// ---------------------------------------------------------------------------------

/// Each of `/MediaBox`, `/CropBox`, /Rotate and /Resources is the page's own, or else
/// that of the nearest node up its /Parent chain that has it, each entry on its
/// own. `PyMuPDF` 1.28.2 shows the same.
#[test]
fn each_entry_is_the_pages_or_the_nearest_parents_that_has_it() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 900 900] /Rotate 270 \
         /Resources << /XObject << /R 9 0 R >> >> >>",
        "<< /Type /Pages /Parent 2 0 R /Kids [4 0 R] /Count 1 /MediaBox [0 0 500 500] \
         /Rotate 180 >>",
        "<< /Type /Pages /Parent 3 0 R /Kids [5 0 R] /Count 1 /CropBox [10 10 400 400] >>",
        "<< /Type /Page /Parent 4 0 R /Rotate 90 >>",
    ]);
    assert_eq!(
        shown("nearest", file),
        ["5 [0 0 500 500] [10 10 400 400] 90 /R"]
    );
}

/// A direct `null` is no entry, and the lookup goes on past it; a reference, even
/// to nothing or to a `null` object, is the entry, whose value is then `null`; a
/// value that is no box, rotation or dictionary is the entry too, and stops the
/// lookup: the page then has no box (A4), no rotation or no resources. A crop box
/// that is none is the media box. `PyMuPDF` 1.28.2 finds the same entries.
#[test]
fn a_direct_null_is_no_entry_and_anything_else_is_one() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 6 0 R] /Count 4 /MediaBox [0 0 300 300] \
         /CropBox [0 0 200 200] /Rotate 90 /Resources << /XObject << /R 9 0 R >> >> >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox null /CropBox null /Rotate null \
         /Resources null >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox 99 0 R /CropBox 99 0 R /Rotate 99 0 R \
         /Resources 99 0 R >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox (none) /CropBox [0 0 1] /Rotate /R \
         /Resources 7 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox 7 0 R >>",
        "null",
    ]);
    assert_eq!(
        shown("null_entries", file),
        [
            "3 [0 0 300 300] [0 0 200 200] 90 /R",
            "4 - = 0 -",
            "5 - = 0 -",
            "6 - [0 0 200 200] 90 /R",
        ]
    );
}

/// A page inherits through its /Parent, not from the node listing it: page 3,
/// listed by the root, inherits from node 5, outside the tree. `PyMuPDF` 1.28.2
/// shows the same.
#[test]
fn a_page_inherits_through_its_parent_not_its_lister() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 100 100] \
         /Resources << /XObject << /Root 9 0 R >> >> >>",
        "<< /Type /Page /Parent 5 0 R /MediaBox null >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox (none) /Resources 9 0 R >>",
        "<< /Type /Pages /Kids [] /Count 0 /MediaBox [0 0 300 300] /Rotate 90 \
         /Resources << /XObject << /X1 9 0 R >> >> >>",
    ]);
    assert_eq!(
        shown("parent_not_lister", file),
        ["3 [0 0 300 300] = 90 /X1", "4 - = 0 -"]
    );
}

/// Where the /Parent chain ends — at an entry without /Parent, or whose /Parent is
/// no dictionary (a number, a name, a reference to nothing) — without the entry,
/// the lookup goes on through the nodes the walk of the /Kids listed that last
/// entry under, nearest first: all the nodes above the page where the chain ends at
/// the page itself, those above the last node where the page is listed below it,
/// and none where it is not, or where that node is outside the tree. A deliberate
/// difference: `PyMuPDF` 1.28.2 finds none of these entries, so a
/// page that lost its /Parent keeps the resources and boxes the tree gives it.
#[test]
fn where_the_parent_chain_ends_the_nodes_listing_its_end_give_the_entry() {
    let file = numbered(&[
        (1, CATALOG),
        (
            2,
            "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 7 0 R 10 0 R 11 0 R 13 0 R 16 0 R 17 0 R] \
             /Count 9 /MediaBox [0 0 300 300] /Rotate 90 \
             /Resources << /XObject << /F 9 0 R >> >> >>",
        ),
        // No /Parent.
        (3, "<< /Type /Page >>"),
        // A /Parent that is a number.
        (4, "<< /Type /Page /Parent 5 >>"),
        // A node without /Parent, whose page's /Parent is the node.
        (5, "<< /Type /Pages /Kids [6 0 R] /Count 1 >>"),
        (6, "<< /Type /Page /Parent 5 0 R >>"),
        // A /Parent outside the tree, which has no /Parent.
        (7, "<< /Type /Page /Parent 8 0 R >>"),
        (8, "<< /Type /Pages /Kids [] /Count 0 >>"),
        // A /Parent that is a name.
        (10, "<< /Type /Page /Parent /P >>"),
        // The nearest of the nodes listing the page that has the entry.
        (
            11,
            "<< /Type /Pages /Kids [12 0 R] /Count 1 /MediaBox [0 0 500 500] >>",
        ),
        (12, "<< /Type /Page >>"),
        // Page 15's /Parent is node 13, above node 14, which lists it: node 13's
        // listers give the entries, not node 14.
        (13, "<< /Type /Pages /Kids [14 0 R] /Count 1 >>"),
        (
            14,
            "<< /Type /Pages /Parent 13 0 R /Kids [15 0 R] /Count 1 /MediaBox [0 0 700 700] >>",
        ),
        (15, "<< /Type /Page /Parent 13 0 R >>"),
        // Page 16's /Parent is node 5, which does not list it.
        (16, "<< /Type /Page /Parent 5 0 R >>"),
        // A /Parent that is a reference to nothing.
        (17, "<< /Type /Page /Parent 99 0 R >>"),
    ]);
    let root = "[0 0 300 300] = 90 /F";
    let expected: Vec<String> = [
        ("3", root),
        ("4", root),
        ("6", root),
        ("7", "- = 0 -"),
        ("10", root),
        ("12", "[0 0 500 500] = 90 /F"),
        ("15", root),
        ("16", "- = 0 -"),
        ("17", root),
    ]
    .iter()
    .map(|(id, rest)| format!("{id} {rest}"))
    .collect();
    assert_eq!(shown("chain_end", file), expected);
}

/// A page without /Parent listed under two nodes inherits under each listing from
/// the nodes above that listing. A deliberate difference: `PyMuPDF` 1.28.2 finds
/// no entry.
#[test]
fn a_page_without_parent_inherits_from_each_listing() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /Rotate 90 >>",
        "<< /Type /Pages /Kids [5 0 R] /Count 1 /MediaBox [0 0 100 100] >>",
        "<< /Type /Pages /Kids [5 0 R] /Count 1 /MediaBox [0 0 200 200] >>",
        "<< /Type /Page >>",
    ]);
    assert_eq!(
        shown("listings", file),
        ["5 [0 0 100 100] = 90 -", "5 [0 0 200 200] = 90 -"]
    );
}

/// A /Parent chain that loops makes every lookup from then on follow repaired
/// /Parent entries: each entry of the tree below the root gets as its /Parent the
/// node that lists it last in a depth-first walk of the /Kids, kids first to last,
/// that reads every listing of a node. Here node 6, listed by nodes 3 and 5, gets
/// node 5, over node 4, which lists the page between them. `PyMuPDF` 1.28.2 shows
/// the same.
#[test]
fn a_parent_loop_is_repaired_from_the_last_listing() {
    let form = |name: &str| format!("/Resources << /XObject << /{name} 9 0 R >> >>");
    let file = pdf_of(&[
        CATALOG.to_owned(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 2 /MediaBox [0 0 300 300] >>".to_owned(),
        "<< /Type /Pages /Kids [6 0 R] /Count 1 >>".to_owned(),
        format!("<< /Type /Pages /Kids [7 0 R] /Count 1 {} >>", form("Z")),
        "<< /Type /Pages /Kids [6 0 R] /Count 0 >>".to_owned(),
        format!("<< /Type /Pages /Kids [7 0 R] /Count 1 {} >>", form("X")),
        "<< /Type /Page /Parent 7 0 R >>".to_owned(),
    ]);
    assert_eq!(
        shown("repair_last_listing", file),
        ["7 [0 0 300 300] = 0 /X", "7 [0 0 300 300] = 0 /X"]
    );
    // Page 5, listed by node 3 and then by node 4, gets node 4.
    let file = pdf_of(&[
        CATALOG.to_owned(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 300 300] >>".to_owned(),
        format!("<< /Type /Pages /Kids [5 0 R] /Count 1 {} >>", form("A")),
        format!("<< /Type /Pages /Kids [5 0 R] /Count 1 {} >>", form("B")),
        "<< /Type /Page /Parent 5 0 R >>".to_owned(),
    ]);
    assert_eq!(
        shown("repair_last_of_two", file),
        ["5 [0 0 300 300] = 0 /B", "5 [0 0 300 300] = 0 /B"]
    );
}

/// The repair reads every /Kids array of a dictionary, a page's included: page 4's
/// /Parent is itself, and page 3, which the root lists after it, lists it again, so
/// page 3 becomes page 4's /Parent. `PyMuPDF` 1.28.2 shows the same.
#[test]
fn the_repair_reads_the_kids_of_a_page_too() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [4 0 R 3 0 R] /Count 2 /MediaBox [0 0 300 300] >>",
        "<< /Type /Page /Kids [4 0 R] /MediaBox [0 0 100 100] >>",
        "<< /Type /Page /Parent 4 0 R >>",
    ]);
    assert_eq!(
        shown("repair_page_kids", file),
        ["4 [0 0 100 100] = 0 -", "3 [0 0 100 100] = 0 -"]
    );
}

/// The repair gives a dictionary written into a /Kids array the node listing it as
/// its /Parent too, and passes over kids that are `null` or no dictionary.
/// `PyMuPDF` 1.28.2 shows the same for the second file; on the first it fails to
/// load the pages of the `null` and the number, and the repair those make it start.
#[test]
fn the_repair_sets_the_parent_of_a_direct_page() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [null 7 << /Type /Page /Parent 3 0 R >> 4 0 R] /Count 4 \
         /MediaBox [0 0 300 300] >>",
        "<< /Type /Pages /Parent 3 0 R /MediaBox [0 0 100 100] >>",
        "<< /Type /Page /Parent 4 0 R >>",
    ]);
    // The `null` and the number are pages with no entries, inheriting the root's
    // box through the listing. The page written into the array finds the box of
    // node 3, outside the tree, until the loop of node 3's /Parent, met looking up
    // its crop box, is repaired; from then on its /Parent is the root.
    assert_eq!(
        shown("repair_direct", file),
        [
            "d [0 0 300 300] = 0 -",
            "d [0 0 300 300] = 0 -",
            "d [0 0 300 300] = 0 -",
            "4 [0 0 300 300] = 0 -",
        ]
    );
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [<< /Type /Page /Parent 3 0 R >> 4 0 R] /Count 2 \
         /MediaBox [0 0 300 300] >>",
        "<< /Type /Pages /Parent 3 0 R /MediaBox [0 0 100 100] >>",
        "<< /Type /Page /Parent 4 0 R >>",
    ]);
    assert_eq!(
        shown("repair_direct_only", file),
        ["d [0 0 300 300] = 0 -", "4 [0 0 300 300] = 0 -"]
    );
}

/// Where the walk of the repair meets a node again inside a /Kids cycle, the first
/// listing of it on the way down wins: page 5's /Parent becomes node 3, whose
/// /Parent becomes the root, not node 4, which lists node 3 again below it.
/// `PyMuPDF` 1.28.2 shows the same.
#[test]
fn the_repair_of_a_kids_cycle_keeps_the_listing_above_it() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 300] >>",
        "<< /Type /Pages /Kids [4 0 R 5 0 R] /Count 1 >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 0 /MediaBox [0 0 200 200] >>",
        "<< /Type /Page /Parent 5 0 R >>",
    ]);
    assert_eq!(shown("repair_cycle", file), ["5 [0 0 300 300] = 0 -"]);
}

/// The repair reaches every depth: the root lists node 3 first and then a chain of
/// 255 nodes ending at node 3 too, and page 4 below it, whose /Parent is itself,
/// gets node 3. `PyMuPDF` 1.28.2 shows the same.
#[test]
fn a_parent_loop_is_repaired_below_a_node_also_listed_deep_down() {
    let chain = 255;
    let mut objects = vec![
        CATALOG.to_owned(),
        "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Pages /Kids [4 0 R] /Count 1 /Resources << /XObject << /F 9 0 R >> >> >>"
            .to_owned(),
        "<< /Type /Page /Parent 4 0 R /MediaBox [0 0 100 100] >>".to_owned(),
    ];
    for i in 0..chain {
        let next = if i + 1 < chain { 5 + i + 1 } else { 3 };
        objects.push(pages_node("1", &format!("{next} 0 R")));
    }
    assert_eq!(
        shown("repair_deep_shared", pdf_of(&objects)),
        ["4 [0 0 100 100] = 0 /F"]
    );
}

/// The repair reaches past 256 levels and past a kid that is no dictionary: page 3's
/// /Parent is itself, and the root lists a chain of 300 nodes whose last lists page
/// 304, whose /Parent is itself, and a reference to nothing, which is a page with
/// no entries. A deliberate difference: `PyMuPDF` 1.28.2 leaves page 304, more than
/// 256 levels down, unrepaired and fails to load it, and finds no box for the
/// reference to nothing; where the kid that is no dictionary is within its reach
/// (fewer than 250 levels down), it fails to load page 3 too.
#[test]
fn a_parent_loop_is_repaired_at_any_depth_past_a_kid_that_is_no_dictionary() {
    let depth = 300;
    let mut objects = vec![
        CATALOG.to_owned(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 3 /MediaBox [0 0 300 300] >>".to_owned(),
        "<< /Type /Page /Parent 3 0 R >>".to_owned(),
    ];
    let page = 4 + depth;
    for i in 0..depth {
        let kids = if i + 1 < depth {
            format!("{} 0 R", 5 + i)
        } else {
            format!("{page} 0 R 9999 0 R")
        };
        objects.push(pages_node("1", &kids));
    }
    objects.push(format!("<< /Type /Page /Parent {page} 0 R >>"));
    let root = "[0 0 300 300] = 0 -";
    assert_eq!(
        shown("repair_deep", pdf_of(&objects)),
        [
            format!("3 {root}"),
            format!("{page} {root}"),
            format!("9999 {root}")
        ]
    );
}

/// The repair leaves the root's own /Parent: where it loops through the root, the
/// chain still loops after the repair, and an entry found before the loop is found
/// while one that is not is not inherited ([`InheritanceError::ParentLoop`]).
/// `PyMuPDF` 1.28.2 fails to load the page.
#[test]
fn a_loop_through_the_roots_parent_is_not_repaired() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Parent 3 0 R /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 300] >>",
        "<< /Type /Pages /Parent 2 0 R /Kids [4 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 3 0 R >>",
    ]);
    let pdf = open("root_loop", file);
    let pages: Vec<String> = pdf.pages().iter().map(line).collect();
    assert_eq!(pages, ["4 [0 0 300 300] = 0 -"]);
    let mut tree = Inheritance::new(&pdf);
    let page = &pdf.pages()[0];
    assert!(matches!(
        tree.get(page, Inheritable::MediaBox),
        Ok(Some(Object::Array(_)))
    ));
    for key in [
        Inheritable::CropBox,
        Inheritable::Rotate,
        Inheritable::Resources,
    ] {
        assert_eq!(
            tree.get(page, key).err(),
            Some(InheritanceError::ParentLoop),
            "{key:?}"
        );
    }
}

/// An [`Inheritance`] follows the /Parent entries as written until a lookup meets a
/// loop, and the repaired ones from that lookup on: page 3, whose /Parent is node 5
/// outside the tree, finds node 5's box before page 4's loop is met and the root's
/// after. The pages' own boxes, rotations and resources are found once every page
/// has been looked up, so page 3's is the root's. `PyMuPDF` 1.28.2 shows the same
/// when every page is loaded before the boxes are read.
#[test]
fn lookups_follow_the_repaired_parents_once_a_loop_was_met() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /MediaBox [0 0 300 300] >>",
        "<< /Type /Page /Parent 5 0 R >>",
        "<< /Type /Page /Parent 4 0 R >>",
        "<< /Type /Pages /Kids [] /Count 0 /MediaBox [0 0 100 100] >>",
    ]);
    let pdf = open("repair_order", file);
    let pages: Vec<String> = pdf.pages().iter().map(line).collect();
    assert_eq!(pages, ["3 [0 0 300 300] = 0 -", "4 [0 0 300 300] = 0 -"]);
    let width = |found: Result<Option<Object<'_>>, InheritanceError>| match found {
        Ok(Some(Object::Array(array))) => array.iter::<f64>().nth(2),
        other => panic!("{other:?}"),
    };
    let (page_3, page_4) = (&pdf.pages()[0], &pdf.pages()[1]);
    let mut tree = Inheritance::new(&pdf);
    assert_eq!(width(tree.get(page_3, Inheritable::MediaBox)), Some(100.0));
    assert_eq!(width(tree.get(page_4, Inheritable::MediaBox)), Some(300.0));
    assert_eq!(width(tree.get(page_3, Inheritable::MediaBox)), Some(300.0));
    // A new one starts from the /Parent entries as written.
    let mut tree = Inheritance::new(&pdf);
    assert_eq!(width(tree.get(page_3, Inheritable::MediaBox)), Some(100.0));
    // Without the entry, a lookup finds nothing.
    assert!(matches!(tree.get(page_3, Inheritable::Rotate), Ok(None)));
}

/// Inherited values are read through references, each number of a box too. A box
/// is the first four numbers of an array, in either order of its corners; an array
/// of fewer, or with one that is no number, is no box, and each number is read at
/// single precision (612.3 is 612.2999877929688). The rotation is the integer
/// part of the number, turned into 0 to 359; one that is no multiple of 90, or that
/// does not fit 32 bits, is none. A crop box is not cut to the media box. `PyMuPDF`
/// 1.28.2 finds the same entries; it turns /Rotate 90.7 into no rotation, where
/// this reads 90 (the rotation rule is the page's, not the tree's).
#[test]
fn inherited_values_are_read_through_references() {
    let file = numbered(&[
        (1, CATALOG),
        (
            2,
            "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 6 0 R 14 0 R 15 0 R] /Count 6 \
             /MediaBox 7 0 R /Rotate 8 0 R /Resources 9 0 R >>",
        ),
        (3, "<< /Type /Page /Parent 2 0 R >>"),
        (
            4,
            "<< /Type /Page /Parent 2 0 R /MediaBox [300 400 0 0 99] /Rotate 450 >>",
        ),
        (
            5,
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300] /CropBox [0 0 100 (x)] \
             /Rotate 45 >>",
        ),
        (
            6,
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] \
             /CropBox [-10 -10 1000 1000] /Rotate 90.7 >>",
        ),
        (7, "[0 0 10 0 R 11 0 R]"),
        (8, "-90"),
        (9, "<< /XObject 12 0 R >>"),
        (10, "300"),
        (11, "400.5"),
        (12, "<< /A 13 0 R /B 13 0 R >>"),
        (14, "<< /Type /Page /Parent 2 0 R /Rotate 2147483730 >>"),
        (
            15,
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612.3 792] >>",
        ),
    ]);
    assert_eq!(
        shown("references", file),
        [
            "3 [0 0 300 400.5] = 270 /A/B",
            "4 [0 0 300 400] = 90 /A/B",
            "5 - = 0 /A/B",
            "6 [0 0 300 300] [-10 -10 1000 1000] 90 /A/B",
            "14 [0 0 300 400.5] = 0 /A/B",
            "15 [0 0 612.2999877929688 792] = 270 /A/B",
        ]
    );
}

/// Inherited /Resources that are no dictionary, a stream included, are none. A
/// category of resources that is a stream is its dictionary, even where its data
/// cannot be read, which then loses nothing; read as a stream, it is lost.
/// `PyMuPDF` 1.28.2 reads /Resources that are a stream as the stream's dictionary,
/// so page 4 has /S there: a difference, reported, not pinned as intended.
#[test]
fn resources_that_are_no_dictionary_are_none() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>",
        "<< /Type /Page /Resources 7 >>",
        "<< /Type /Page /Resources 6 0 R >>",
        "<< /Type /Page /Resources << /XObject 7 0 R /Font 8 0 R >> >>",
        "<< /XObject << /S 9 0 R >> /Length 0 >>\nstream\n\nendstream",
        "<< /C 9 0 R /Length 0 >>\nstream\n\nendstream",
        "<< /F1 9 0 R /Length 9999 >>\nstream\nBT /F1 12 Tf ET",
    ]);
    let pdf = open("resources", file);
    let pages: Vec<String> = pdf.pages().iter().map(line).collect();
    assert_eq!(pages, ["3 - = 0 -", "4 - = 0 -", "5 - = 0 /C/Length"]);
    let page = &pdf.pages()[2];
    assert_eq!(page.unreadable(), None);
    assert!(page.resources().fonts.contains_key(b"F1"));
    let (stream, lost) = pdf.xref().watch(|| {
        pdf.xref()
            .get::<hayro_syntax::object::Stream<'_>>(ObjectIdentifier::new(8, 0))
    });
    assert!(stream.is_none());
    assert_eq!(lost, Some(ObjectIdentifier::new(8, 0)));
}

// ---------------------------------------------------------------------------------
// What cannot be read.
// ---------------------------------------------------------------------------------

/// Each page's object and media box width, what the document lost, and what each
/// page lost.
type Losses = (Vec<(String, f64)>, Option<i32>, Vec<Option<i32>>);

fn lost(name: &str, objects: &[&str]) -> Losses {
    let pdf = open(name, pdf(objects));
    let number = |id: Option<ObjectIdentifier>| id.map(|id| id.obj_number);
    let pages = pdf.pages();
    (
        pages
            .iter()
            .map(|page| (id(page.obj_id()), page.media_box().x1))
            .collect(),
        number(pages.unreadable()),
        pages.iter().map(|page| number(page.unreadable())).collect(),
    )
}

/// A /Kids array, a node or a node's /Type that the file lists but that cannot be
/// read may have held pages: the document tells it ([`Pages::unreadable`]), the
/// first met in the walk, and lists the pages that can be read; a node that cannot
/// be read is a page with no entries. A /Kids array the file does not hold loses
/// nothing. No page tells what the walk lost. `PyMuPDF` 1.28.2 lists the same
/// pages where the root's /Count matches them, and fails to load the second page
/// where a /Kids array cannot be read or is missing.
///
/// [`Pages::unreadable`]: hayro_syntax::page::Pages::unreadable
#[test]
fn a_page_tree_object_that_cannot_be_read_is_lost_to_the_document() {
    let tree = pages_node("2", "3 0 R 4 0 R");
    let small = "<< /Type /Page /MediaBox [0 0 200 100] >>";
    let node = |entries: &str| format!("<< /Type /Pages /Parent 2 0 R {entries} >>");
    let typed = "<< /Type 5 0 R /Parent 2 0 R /Kids [6 0 R] >>";
    let wide = "<< /Type /Page /MediaBox [0 0 600 500] >>";
    let a4 = A4.x1;
    for (i, (third, fifth, pages, document)) in [
        (node("/Kids 5 0 R"), "[6 0 R", vec![("4", 200.0)], Some(5)),
        (
            node("/Kids 5 0 R"),
            "[6 0 R]",
            vec![("6", 600.0), ("4", 200.0)],
            None,
        ),
        (node("/Kids 9 0 R"), "null", vec![("4", 200.0)], None),
        (
            "<< /Type /Pages /Kids [6 0 R] /Broken >".to_owned(),
            "null",
            vec![("3", a4), ("4", 200.0)],
            Some(3),
        ),
        (
            typed.to_owned(),
            "<< /Broken >",
            vec![("3", a4), ("4", 200.0)],
            Some(5),
        ),
        (
            typed.to_owned(),
            "/Pages",
            vec![("6", 600.0), ("4", 200.0)],
            None,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let (listed, document_lost, each) = lost(
            &format!("lost_tree_{i}"),
            &[CATALOG, &tree, &third, small, fifth, wide],
        );
        let pages: Vec<(String, f64)> = pages
            .into_iter()
            .map(|(id, width)| (id.to_owned(), width))
            .collect();
        assert_eq!(
            (listed, document_lost),
            (pages, document),
            "{third}, object 5: {fifth}"
        );
        assert!(each.iter().all(Option::is_none), "{third}: {each:?}");
    }
    // Of two nodes that cannot be read, the first the walk meets is told.
    let broken = "<< /Type /Pages /Broken >";
    let (_, document_lost, _) = lost(
        "lost_first",
        &[CATALOG, &pages_node("2", "4 0 R 3 0 R"), broken, broken],
    );
    assert_eq!(document_lost, Some(4));
}

/// An inherited entry, a number of a box, a category of resources or a /Parent that
/// the file lists but that cannot be read reads as `null` and is lost to each page
/// read with it ([`Page::unreadable`]), however it reaches the page — through its
/// /Parent or through the nodes listing it — and though it is read once for all;
/// the document loses nothing. A /Parent that cannot be read ends the chain.
/// Something the file does not hold makes no box, and loses nothing. `PyMuPDF`
/// 1.28.2 finds the same boxes for page 3, and, deliberately, none for page 4,
/// which has no /Parent, nor for the page whose /Parent cannot be read.
#[test]
fn an_inherited_object_that_cannot_be_read_is_lost_to_each_page_read_with_it() {
    let broken = "<< /Broken >";
    let a4 = A4.x1;
    let tree = |entries: &str| format!("<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 {entries} >>");
    let with_parent = "<< /Type /Page /Parent 2 0 R >>";
    let without = "<< /Type /Page >>";
    for (i, (entries, fifth, width, page_lost)) in [
        ("/MediaBox 5 0 R", broken, a4, Some(5)),
        ("/MediaBox [0 0 5 0 R 700]", broken, a4, Some(5)),
        ("/MediaBox [0 0 5 0 R 700]", "[1 2", a4, Some(5)),
        ("/MediaBox [0 0 5 0 R 700]", "600", 600.0, None),
        ("/MediaBox [0 0 9 0 R 700]", "null", a4, None),
        ("/Resources << /XObject 5 0 R >>", broken, a4, Some(5)),
    ]
    .into_iter()
    .enumerate()
    {
        let (listed, document, each) = lost(
            &format!("lost_inherited_{i}"),
            &[CATALOG, &tree(entries), with_parent, without, fifth],
        );
        assert_eq!(
            listed,
            [("3".to_owned(), width), ("4".to_owned(), width)],
            "{entries}: {fifth}"
        );
        assert_eq!(document, None);
        assert_eq!(each, [page_lost; 2], "{entries}: {fifth}");
    }
    // Each page's own box holds one number that cannot be read.
    let own = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 5 0 R 700] >>";
    let (listed, document, each) = lost("lost_own_boxes", &[CATALOG, &tree(""), own, own, broken]);
    assert_eq!(
        (listed, document, each),
        (
            vec![("3".to_owned(), a4), ("4".to_owned(), a4)],
            None,
            vec![Some(5); 2]
        )
    );
    // Page 3's /Parent cannot be read: it inherits through the root, which lists it.
    let (listed, document, each) = lost(
        "lost_parent",
        &[
            CATALOG,
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 300] >>",
            "<< /Type /Page /Parent 4 0 R >>",
            broken,
        ],
    );
    assert_eq!(
        (listed, document, each),
        (vec![("3".to_owned(), 300.0)], None, vec![Some(4)])
    );
}

/// [`Inheritance::get`] finds an entry that cannot be read as `null` and tells the
/// computation being watched each time it is asked, though it read it once.
#[test]
fn a_lookup_tells_what_it_lost_each_time() {
    let file = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox 4 0 R >>",
        "<< /Type /Page /Parent 2 0 R >>",
        "<< /Broken >",
    ]);
    let pdf = open("lookup_lost", file);
    let page = &pdf.pages()[0];
    let mut tree = Inheritance::new(&pdf);
    for _ in 0..2 {
        let (found, lost) = pdf.xref().watch(|| tree.get(page, Inheritable::MediaBox));
        assert!(matches!(found, Ok(Some(Object::Null(_)))));
        assert_eq!(lost, Some(ObjectIdentifier::new(4, 0)));
    }
    let (found, lost) = pdf.xref().watch(|| tree.get(page, Inheritable::Rotate));
    assert!(matches!(found, Ok(None)));
    assert_eq!(lost, None);
}

/// Where no page tree lists a page, any object the file lists but that cannot be
/// read may have been a page: the document tells the first.
#[test]
fn without_a_page_tree_any_object_that_cannot_be_read_is_lost_to_the_document() {
    let content = "<< /Length 0 >>\nstream\n\nendstream";
    let file = pdf(&[
        "<< /Type /Catalog >>",
        "<< /Type /Page /MediaBox [0 0 100 100] /Contents 3 0 R >>",
        content,
        "<< /Broken >",
    ]);
    let doc = open("lost_no_tree", file);
    assert_eq!(doc.pages().len(), 1);
    assert_eq!(doc.pages().unreadable(), Some(ObjectIdentifier::new(4, 0)));
    // With a tree, an object it does not reach is not lost.
    let file = pdf_of(&[
        CATALOG.to_owned(),
        pages_node("1", "3 0 R"),
        "<< /Type /Page /MediaBox [0 0 100 100] >>".to_owned(),
        "<< /Broken >".to_owned(),
    ]);
    assert_eq!(open("lost_tree_unreached", file).pages().unreadable(), None);
}

/// Where no page tree lists a page, the document tells the first object that cannot
/// be read the scan meets: reading the objects in file order, it meets what a page's
/// /Type refers to with the page, before the objects between them.
#[test]
fn without_a_page_tree_the_first_object_the_scan_cannot_read_is_told() {
    let file = pdf(&[
        "<< /Type /Catalog >>",
        "<< /Type 4 0 R /MediaBox [0 0 100 100] /Contents 5 0 R >>",
        "<< /Broken >",
        "<< /Broken >",
        "<< /Length 0 >>\nstream\n\nendstream",
    ]);
    let doc = open("lost_no_tree_first", file);
    assert_eq!(doc.pages().len(), 1);
    assert_eq!(doc.pages().unreadable(), Some(ObjectIdentifier::new(4, 0)));
}

/// /Resources that refers to the dictionary holding it — a node's or the page's own
/// — is that dictionary, its categories included, as `PyMuPDF` 1.28.2 draws such a
/// page's form.
#[test]
fn resources_that_refer_to_their_own_dictionary_are_that_dictionary() {
    let form =
        "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] /Length 0 >>\nstream\n\nendstream";
    let inherited = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [3 0 R] /Count 1 /Resources 2 0 R /XObject << /F 4 0 R >> >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>",
        form,
    ]);
    let own = pdf(&[
        CATALOG,
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources 3 0 R /XObject << /F 4 0 R >> >>",
        form,
    ]);
    for (name, file) in [
        ("self_resources_inherited", inherited),
        ("self_resources_own", own),
    ] {
        let doc = open(name, file);
        let page = &doc.pages()[0];
        assert!(
            page.resources().x_objects.contains_key(b"F".as_slice()),
            "{name}"
        );
        let found = Inheritance::new(&doc).get(page, Inheritable::Resources);
        assert!(
            matches!(found, Ok(Some(Object::Dict(_)))),
            "{name}: {found:?}"
        );
        assert_eq!(page.unreadable(), None, "{name}");
    }
}

// ---------------------------------------------------------------------------------
// Time, stack and memory.
// ---------------------------------------------------------------------------------

/// Loads `file` and looks up every inherited entry of every page with an
/// [`Inheritance`] of its own, as a reader of the pages does.
fn load(file: &[u8]) {
    let pdf = Pdf::new(file.to_vec()).unwrap();
    let mut tree = Inheritance::new(&pdf);
    for page in pdf.pages().iter() {
        for key in [
            Inheritable::MediaBox,
            Inheritable::CropBox,
            Inheritable::Rotate,
            Inheritable::Resources,
        ] {
            let _ = tree.get(page, key);
        }
    }
}

/// What [`load`]ing `file` once allocates, and the least processor time of this
/// thread, of three runs, it takes.
fn cost(file: &[u8]) -> (Allocations, Duration) {
    let ((), allocations) = measure(|| load(file));
    let time = (0..3)
        .map(|_| {
            let start = thread_cpu_time();
            load(file);
            thread_cpu_time() - start
        })
        .min()
        .unwrap();
    (allocations, time)
}

/// Checks that [`load`]ing the file `build` makes for `n`, once `check` passes on
/// it, takes work linear in it: the allocations for `2n` are at most a little over
/// twice those for `n`, and the bytes allocated and processor time at most three
/// times (tables that double their room, and other work on the host's cores and
/// caches, move those), where work quadratic in the file takes four.
fn grows_linearly(
    what: &str,
    n: usize,
    build: impl Fn(usize) -> Vec<u8>,
    check: impl Fn(&Pdf, usize),
) {
    let [(small, small_time), (large, large_time)] = [n, 2 * n].map(|n| {
        let file = build(n);
        check(&Pdf::new(file.clone()).unwrap(), n);
        cost(&file)
    });
    assert!(
        large.count <= small.count * 22 / 10 && large.allocated <= small.allocated * 3,
        "{what}: allocations (count, bytes) ({}, {}) for {n}, ({}, {}) for {}",
        small.count,
        small.allocated,
        large.count,
        large.allocated,
        2 * n
    );
    assert!(
        large_time <= small_time * 3,
        "{what}: {small_time:?} for {n}, {large_time:?} for {}",
        2 * n
    );
}

/// Each node lists the next twice: walking every listing would take 2^40 steps.
/// The last node lists the page twice, which is two pages. With `parent_loop`, the
/// page's /Parent is itself, which makes the lookups repair the tree.
fn doubled(depth: usize, parent_loop: bool) -> Vec<u8> {
    let mut objects = vec![CATALOG.to_owned()];
    for level in 0..depth {
        let next = level + 3;
        let entries = if level == 0 {
            "/MediaBox [0 0 300 300]"
        } else {
            ""
        };
        objects.push(format!(
            "<< /Type /Pages /Kids [{next} 0 R {next} 0 R] {entries} >>"
        ));
    }
    let parent = if parent_loop {
        format!("/Parent {} 0 R", depth + 2)
    } else {
        String::new()
    };
    objects.push(format!("<< /Type /Page {parent} >>"));
    pdf_of(&objects)
}

/// Shared subtrees are read once, with and without the repair: a tree that lists
/// each of 40 nodes twice loads in well under a second.
#[test]
fn shared_subtrees_are_read_in_linear_time() {
    let depth = 40;
    for parent_loop in [false, true] {
        let file = doubled(depth, parent_loop);
        let start = thread_cpu_time();
        let lines = shown(&format!("doubled_{parent_loop}"), file);
        let page = format!("{} [0 0 300 300] = 0 -", depth + 2);
        assert_eq!(lines, [page.clone(), page], "parent loop: {parent_loop}");
        assert!(
            thread_cpu_time() - start < Duration::from_secs(1),
            "parent loop: {parent_loop}"
        );
    }
}

/// The root lists a chain of `depth` nodes, each listing the next, and the last
/// lists `pages` pages. Only the root has entries a page inherits; each node and
/// page has a /Parent to the node listing it where `parents` holds.
fn nested(depth: usize, pages: usize, parents: bool) -> Vec<u8> {
    let first_page = depth + 3;
    let parent = |of: usize| {
        if parents {
            format!("/Parent {of} 0 R")
        } else {
            String::new()
        }
    };
    let mut objects = vec![
        CATALOG.to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /MediaBox [0 0 200 100] /Rotate 90 \
         /Resources << /XObject << /F 9 0 R >> >> >>"
            .to_owned(),
    ];
    for level in 0..depth {
        let number = level + 3;
        let kids: Vec<String> = if level + 1 < depth {
            vec![format!("{} 0 R", number + 1)]
        } else {
            (0..pages)
                .map(|p| format!("{} 0 R", first_page + p))
                .collect()
        };
        objects.push(format!(
            "<< /Type /Pages /Kids [{}] {} >>",
            kids.join(" "),
            parent(number - 1)
        ));
    }
    for _ in 0..pages {
        objects.push(format!("<< /Type /Page {} >>", parent(depth + 2)));
    }
    pdf_of(&objects)
}

/// A page under 100,000 nested nodes opens, inherits the root's entries, and is
/// dropped on a thread with the 1 MiB stack of a render thread, with and without
/// /Parent entries: nothing takes stack in proportion to the depth.
#[test]
fn a_page_under_deeply_nested_nodes_opens_and_drops_on_a_small_stack() {
    for parents in [false, true] {
        let file = nested(100_000, 1, parents);
        let read = std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || {
                let pdf = Pdf::new(file).unwrap();
                line(&pdf.pages()[0])
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(read, "100003 [0 0 200 100] = 90 /F", "parents: {parents}");
    }
}

/// The entries `n` pages under `n` nested nodes inherit are found in time linear in
/// the nodes and pages, whether the pages reach the root through /Parent or only
/// through the nodes listing them.
#[test]
fn inherited_entries_are_found_in_linear_time() {
    for parents in [false, true] {
        grows_linearly(
            &format!("parents: {parents}"),
            1000,
            |n| nested(n, n, parents),
            |pdf, n| {
                assert_eq!(pdf.pages().len(), n);
                assert!(pdf.pages().iter().all(|page| page.media_box().x1 == 200.0));
            },
        );
    }
}

/// A root and `n` nodes all list one /Kids array object, which lists those nodes
/// and a page: the page is a page under each. With `parent_loop`, the page's
/// /Parent is itself, which makes the lookups repair the tree.
fn shared_kids(n: usize, parent_loop: bool) -> Vec<u8> {
    let page = n + 4;
    let nodes: Vec<String> = (4..page).map(|node| format!("{node} 0 R")).collect();
    let mut objects = vec![
        CATALOG.to_owned(),
        "<< /Type /Pages /Kids 3 0 R /MediaBox [0 0 200 100] >>".to_owned(),
        format!("[{} {page} 0 R]", nodes.join(" ")),
    ];
    objects.extend((0..n).map(|_| "<< /Type /Pages /Kids 3 0 R >>".to_owned()));
    let parent = if parent_loop {
        format!("/Parent {page} 0 R")
    } else {
        String::new()
    };
    objects.push(format!("<< /Type /Page {parent} >>"));
    pdf_of(&objects)
}

/// A /Kids array many nodes list is read and kept once: loading such a file takes
/// work linear in it, with and without the repair.
#[test]
fn a_kids_array_many_nodes_list_is_read_once() {
    for parent_loop in [false, true] {
        grows_linearly(
            &format!("parent loop: {parent_loop}"),
            500,
            |n| shared_kids(n, parent_loop),
            |pdf, n| {
                assert_eq!(pdf.pages().len(), n + 1);
                for page in pdf.pages().iter() {
                    assert_eq!(page.obj_id(), Some(ObjectIdentifier::new(n as i32 + 4, 0)));
                    assert_eq!(page.media_box().x1, 200.0);
                }
            },
        );
    }
}

/// A page, 3, and a node, 5, that `n` entries of a damaged table lead to: with
/// `direct`, its /Kids array, written into it, lists itself under all of them, and
/// its /Rotate, which the pages could inherit, is an array as large; otherwise each
/// of `n` nodes lists a /Kids array object, 4, under one of `n` entries leading to
/// it, which lists them all. With `parent_loop`, the page's /Parent is itself,
/// which makes the lookups repair the tree. The file grows with `n`, and is one
/// page.
fn aliased_kids(n: usize, direct: bool, parent_loop: bool) -> Vec<u8> {
    let parent = if parent_loop { "/Parent 3 0 R" } else { "" };
    let mut objects = vec![
        CATALOG.to_owned(),
        pages_node("1", "3 0 R 5 0 R"),
        format!("<< /Type /Page /MediaBox [0 0 200 100] {parent} >>"),
    ];
    let refs = |numbers: core::ops::Range<usize>| {
        numbers
            .map(|k| format!("{k} 0 R"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let aliases: Vec<usize> = if direct {
        objects.push("null".to_owned());
        objects.push(format!(
            "<< /Type /Pages /Kids [{}] /Rotate [{}] >>",
            refs(5..n + 5),
            " 0".repeat(n)
        ));
        vec![5; n - 1]
    } else {
        objects.push(format!("[{}]", refs(5..n + 5)));
        objects.extend((0..n).map(|k| format!("<< /Type /Pages /Kids {} 0 R >>", n + 5 + k)));
        vec![4; n]
    };
    common::file_with_aliases(
        objects.into_iter().map(String::into_bytes).collect(),
        &aliases,
    )
}

/// Each /Kids array is read once, however many entries of the table lead to it or
/// to the node holding it, and so is each entry of the node: loading such a file
/// takes work linear in it, with and without the repair.
#[test]
fn a_kids_array_entries_lead_to_is_read_once() {
    for direct in [true, false] {
        for parent_loop in [false, true] {
            grows_linearly(
                &format!("direct: {direct}, parent loop: {parent_loop}"),
                500,
                |n| aliased_kids(n, direct, parent_loop),
                |pdf, _| {
                    let pages = pdf.pages();
                    assert_eq!(pages.len(), 1);
                    assert_eq!(pages[0].obj_id(), Some(ObjectIdentifier::new(3, 0)));
                    assert_eq!(pages[0].media_box().x1, 200.0);
                },
            );
        }
    }
}

/// The root lists node 3 `n` times, and node 3 lists an empty node `n` times and
/// page 5, whose /Parent, with `parent_loop`, is itself, which makes the lookups
/// repair the tree: a node listed many times is read once, and the file loads in
/// work linear in it.
#[test]
fn a_node_listed_many_times_is_read_once() {
    for parent_loop in [false, true] {
        grows_linearly(
            &format!("parent loop: {parent_loop}"),
            1000,
            |n| {
                let parent = if parent_loop { "/Parent 5 0 R" } else { "" };
                pdf_of(&[
                    CATALOG.to_owned(),
                    pages_node("1", &vec!["3 0 R"; n].join(" ")),
                    pages_node("1", &format!("{} 5 0 R", vec!["4 0 R"; n].join(" "))),
                    pages_node("0", ""),
                    format!("<< /Type /Page /MediaBox [0 0 200 100] {parent} >>"),
                ])
            },
            |pdf, _| {
                let ids: Vec<String> = pdf.pages().iter().map(|p| id(p.obj_id())).collect();
                assert_eq!(ids, ["5"]);
            },
        );
    }
}

/// A page as large as the file, listed `n` times, is `n` pages, read once.
#[test]
fn a_page_listed_many_times_is_read_once() {
    grows_linearly(
        "page listed n times",
        1000,
        |n| {
            let page = format!(
                "<< /Type /Page /MediaBox [0 0 200 100] /Pad [{}] >>",
                "0 ".repeat(n)
            );
            pdf_of(&[
                CATALOG.to_owned(),
                pages_node(&n.to_string(), &"3 0 R ".repeat(n)),
                page,
            ])
        },
        |pdf, n| {
            assert_eq!(pdf.pages().len(), n);
            assert!(pdf.pages().iter().all(|page| page.media_box().x1 == 200.0));
        },
    );
}

/// Pages without /Parent inherit the root's /Resources, an object as large as the
/// file, through the node listing them: each page finds it, and it is read once.
#[test]
fn an_entry_pages_inherit_through_the_listing_is_read_once() {
    grows_linearly(
        "resources through the listing",
        1000,
        |n| {
            let pages: Vec<String> = (0..n).map(|page| format!("{} 0 R", page + 4)).collect();
            let names: Vec<String> = (0..n).map(|name| format!("/X{name} 9 0 R")).collect();
            let mut objects = vec![
                CATALOG.to_owned(),
                format!(
                    "<< /Type /Pages /Kids [{}] /Resources 3 0 R >>",
                    pages.join(" ")
                ),
                format!("<< /XObject << {} >> >>", names.join(" ")),
            ];
            objects.extend((0..n).map(|_| "<< /Type /Page /MediaBox [0 0 200 100] >>".to_owned()));
            pdf_of(&objects)
        },
        |pdf, n| {
            assert_eq!(pdf.pages().len(), n);
            assert!(
                pdf.pages()
                    .iter()
                    .all(|page| page.resources().x_objects.keys().count() == n)
            );
        },
    );
}

/// A file whose pages are `n` entries of its table that lead to one page of an
/// array of `n` numbers: the table is rebuilt, and keeps the entries. Without a
/// `tree`, the catalog's /Pages is no node, and every page object is a page.
fn aliased_page(n: usize, tree: bool) -> Vec<u8> {
    let kids: Vec<String> = (4..n + 4).map(|k| format!("{k} 0 R")).collect();
    let catalog = if tree {
        CATALOG
    } else {
        "<< /Type /Catalog /Pages 9999 0 R >>"
    };
    let objects = vec![
        catalog.as_bytes().to_vec(),
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")).into_bytes(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 2 0 R /Pad [{}] >>",
            " 0".repeat(n)
        )
        .into_bytes(),
    ];
    common::file_with_aliases(objects, &vec![3; n])
}

/// Entries of a damaged table that lead to one page object are each a page, with
/// the page tree or without, but the object is read once: loading the pages takes
/// work linear in the file.
#[test]
fn entries_leading_to_one_page_read_it_once() {
    for tree in [true, false] {
        grows_linearly(
            &format!("tree: {tree}"),
            250,
            |n| aliased_page(n, tree),
            |pdf, n| {
                // Without the tree, the page's own number is a page too.
                let pages = if tree { n } else { n + 1 };
                assert_eq!(pdf.pages().len(), pages);
                assert!(pdf.pages().iter().all(|page| page.media_box().x1 == 100.0));
            },
        );
    }
}

/// Pages that share their resources, through a node or as one object, directly or
/// through an object of their own, share each category, read once: a file with `n`
/// pages and `n` fonts that every page has loads in work linear in it.
#[test]
fn resources_pages_share_are_read_once() {
    let fonts = |n: usize| {
        let names: Vec<String> = (0..n).map(|i| format!("/F{i} {i}")).collect();
        format!("<< {} >>", names.join(" "))
    };
    let file = |n: usize, variant: &str| {
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", i + 5)).collect();
        let (node, own, third, fourth) = match variant {
            "inherited, direct /Font" => (
                "/Resources 3 0 R".to_owned(),
                "",
                format!("<< /Font {} >>", fonts(n)),
                "null".to_owned(),
            ),
            "inherited, indirect /Font" => (
                "/Resources 3 0 R".to_owned(),
                "",
                "<< /Font 4 0 R >>".to_owned(),
                fonts(n),
            ),
            "inherited, written into the node" => (
                format!("/Resources << /Font {} >>", fonts(n)),
                "",
                "null".to_owned(),
                "null".to_owned(),
            ),
            "own, one object" => (
                String::new(),
                "/Resources 3 0 R",
                format!("<< /Font {} >>", fonts(n)),
                "null".to_owned(),
            ),
            "own, written into each page, indirect /Font" => (
                String::new(),
                "/Resources << /Font 4 0 R >>",
                "null".to_owned(),
                fonts(n),
            ),
            _ => unreachable!(),
        };
        let mut objects = vec![
            CATALOG.to_owned(),
            format!(
                "<< /Type /Pages /Count {n} /Kids [{}] {node} >>",
                kids.join(" ")
            ),
            third,
            fourth,
        ];
        objects.extend(
            (0..n)
                .map(|_| format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] {own} >>")),
        );
        pdf_of(&objects)
    };
    for variant in [
        "inherited, direct /Font",
        "inherited, indirect /Font",
        "inherited, written into the node",
        "own, one object",
        "own, written into each page, indirect /Font",
    ] {
        grows_linearly(
            variant,
            512,
            |n| file(n, variant),
            |pdf, n| {
                assert!(
                    pdf.pages()
                        .iter()
                        .all(|page| page.resources().fonts.len() == n)
                );
            },
        );
    }
}
