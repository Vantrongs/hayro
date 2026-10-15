//! Entries of a damaged encrypted file's table that lead to one object read what its
//! bytes hold, decrypted with the key they were encrypted with, that of the object
//! their header names: the file loads as it does unencrypted, and what is read once
//! for all the entries is read once.

use hayro_syntax::Pdf;
use hayro_syntax::object::{Array, Name, Object, ObjectIdentifier, Stream};
use hayro_syntax::page::{Inheritable, Inheritance};

mod common;
use common::allocations::measure;
use common::rc4::{self, PALETTE, Part, aliased};

/// The palette of color space /CS of `page`'s resources.
fn palette(pdf: &Pdf, page: usize) -> Vec<u8> {
    let space = pdf.pages()[page]
        .resources()
        .get_color_space(&Name::new(b"CS").unwrap())
        .and_then(Object::into_array)
        .expect("an indexed color space");
    palette_of(&space)
}

fn palette_of(space: &Array<'_>) -> Vec<u8> {
    match space.iter::<Object<'_>>().nth(3) {
        Some(Object::String(palette)) => palette.to_vec(),
        other => panic!("no palette: {other:?}"),
    }
}

// Page 2 is drawn with an entry the table puts at page 1's object, its resources or
// their color spaces: it is drawn with that object's palette, as the object was
// encrypted, read alone or through the page.
#[test]
fn pages_drawn_with_entries_leading_to_one_object_read_its_colors() {
    for (whole, alias) in [(true, 9), (false, 8)] {
        let pdf = Pdf::new(aliased(whole)).unwrap();
        assert_eq!(palette(&pdf, 0), PALETTE, "whole: {whole}");
        assert_eq!(palette(&pdf, 1), PALETTE, "whole: {whole}");
        let object = pdf
            .xref()
            .get::<Object<'_>>(ObjectIdentifier::new(alias, 0))
            .unwrap();
        let space = match object {
            Object::Dict(dict) if whole => dict
                .get::<hayro_syntax::object::Dict<'_>>(b"ColorSpace")
                .unwrap()
                .get::<Array<'_>>(b"CS")
                .unwrap(),
            Object::Dict(dict) => dict.get::<Array<'_>>(b"CS").unwrap(),
            other => panic!("{other:?}"),
        };
        assert_eq!(palette_of(&space), PALETTE, "whole: {whole}");
    }
}

const DRAWN: &str = "<< /Type /Page /MediaBox [0 0 100 100] /Contents 4 0 R \
                     /Resources << /ColorSpace << /CS [/Indexed /DeviceRGB 0 STRING] >> >> >>";

/// Each page's object and palette, for a file whose root lists `order` of object 3 and
/// entry 5, which the table puts at object 3, a node whose /Kids holds `kids`: the
/// node written into it holds a page drawn with a palette.
fn listed(
    order: [u32; 2],
    kids: &str,
    encrypted: bool,
) -> Vec<(Option<ObjectIdentifier>, Vec<u8>)> {
    let root = format!(
        "<< /Type /Pages /Kids [{} 0 R {} 0 R] /Count 2 >>",
        order[0], order[1]
    );
    let node = format!("<< /Type /Pages /Kids [{}] >>", kids.replace("PAGE", DRAWN));
    let file = rc4::file_with(
        &[
            Part::Object("<< /Type /Catalog /Pages 2 0 R >>", b""),
            Part::Object(&root, b""),
            Part::Object(&node, PALETTE),
            Part::Stream(b"/CS cs 0 sc 0 0 50 50 re f"),
        ],
        &[(5, 3)],
        encrypted,
    );
    let pdf = Pdf::new(file).unwrap();
    (0..pdf.pages().len())
        .map(|page| (pdf.pages()[page].obj_id(), palette(&pdf, page)))
        .collect()
}

// Entries leading to one node list what it lists as the same entries of the file
// unencrypted do, in its order, each page with the palette its bytes hold: a node
// written into the node's /Kids is one node, listed once, and a page written into it
// is a page under each entry.
#[test]
fn entries_leading_to_one_node_list_its_pages_as_the_file_unencrypted() {
    for (kids, pages) in [("<< /Type /Pages /Kids [PAGE] >>", 1), ("PAGE", 2)] {
        for order in [[3, 5], [5, 3]] {
            let encrypted = listed(order, kids, true);
            assert_eq!(encrypted, listed(order, kids, false), "{kids}, {order:?}");
            assert_eq!(encrypted.len(), pages, "{kids}, {order:?}");
            assert!(
                encrypted
                    .iter()
                    .all(|(id, palette)| id.is_none() && palette == PALETTE),
                "{kids}, {order:?}: {encrypted:?}"
            );
        }
    }
}

/// A file whose root lists object 3 and `n` entries the table puts at it: a node that
/// lists page 4, whose /Parent it is, and whose /Rotate is a string as large as the file.
fn aliased_node(n: u32) -> Vec<u8> {
    let kids: Vec<String> = (0..=n)
        .map(|i| format!("{} 0 R", if i == 0 { 3 } else { 4 + i }))
        .collect();
    let root = format!("<< /Type /Pages /Kids [{}] >>", kids.join(" "));
    let rotate = vec![b'A'; 64 * n as usize];
    let aliases: Vec<(u32, u32)> = (1..=n).map(|i| (4 + i, 3)).collect();
    rc4::file(
        &[
            Part::Object("<< /Type /Catalog /Pages 2 0 R >>", b""),
            Part::Object(&root, b""),
            Part::Object("<< /Type /Pages /Kids [4 0 R] /Rotate STRING >>", &rotate),
            Part::Object(
                "<< /Type /Page /Parent 3 0 R /MediaBox [0 0 600 700] >>",
                b"",
            ),
        ],
        &aliases,
    )
}

/// What loading `file` and looking up every inherited entry of every page, as the
/// geometry of the pages is found, allocates, once it is checked that each of the
/// `pages` pages is 600 × 700 pt and unrotated.
fn looked_up(file: Vec<u8>, pages: usize) -> common::allocations::Allocations {
    let ((), used) = measure(|| {
        let pdf = Pdf::new(file).unwrap();
        assert_eq!(pdf.pages().len(), pages);
        let mut tree = Inheritance::new(&pdf);
        for page in pdf.pages().iter() {
            assert_eq!(page.render_dimensions(), (600.0, 700.0));
            for key in [
                Inheritable::MediaBox,
                Inheritable::CropBox,
                Inheritable::Rotate,
                Inheritable::Resources,
            ] {
                tree.get(page, key).unwrap();
            }
        }
    });
    used
}

// A string a node holds is decrypted and held once however many entries lead to the
// node: what loading the file allocates and holds grows with the file, not with its
// square.
#[test]
fn a_string_a_node_many_entries_lead_to_holds_is_decrypted_once() {
    let [small, large] = [256, 512].map(|n| looked_up(aliased_node(n), n as usize + 1));
    assert!(
        large.allocated <= small.allocated * 25 / 10,
        "{} bytes allocated for 256 entries, {} for 512",
        small.allocated,
        large.allocated
    );
    assert!(
        large.peak <= small.peak * 25 / 10,
        "{} bytes held at most for 256 entries, {} for 512",
        small.peak,
        large.peak
    );
}

// A stream is identified by its body: entries of the table leading to one stream
// object find one identity, encrypted or not, so what a cache keeps of the stream
// is made once for all of them; two streams of the same bytes are two.
#[test]
fn entries_leading_to_one_stream_identify_it_once() {
    for encrypted in [true, false] {
        let file = rc4::file_with(
            &[
                Part::Object("<< /Type /Catalog /Pages 2 0 R >>", b""),
                Part::Object("<< /Type /Pages /Kids [] /Count 0 >>", b""),
                Part::Stream(b"0 0 1 1 re f"),
                Part::Stream(b"0 0 1 1 re f"),
            ],
            &[(5, 3), (6, 3)],
            encrypted,
        );
        let pdf = Pdf::new(file).unwrap();
        let identity = |n: i32| {
            pdf.xref()
                .get::<Stream<'_>>(ObjectIdentifier::new(n, 0))
                .and_then(|s| s.identity())
                .unwrap()
        };
        assert_eq!(identity(5), identity(3), "encrypted: {encrypted}");
        assert_eq!(identity(6), identity(3), "encrypted: {encrypted}");
        assert_ne!(identity(4), identity(3), "encrypted: {encrypted}");
    }
}

// A stream reads what its dictionary refers to alike through every entry leading to
// it, so that one identity is one decoding: a reference back to the stream itself, by
// another entry leading to it, is a cycle through either entry. Here /DecodeParms
// names the stream, whose dictionary sets a predictor that would turn [65, 1] into
// [65, 66]; MuPDF (PyMuPDF 1.28.2) reads [65, 1] through object 3.
#[test]
fn a_stream_reads_its_parameters_alike_through_every_entry_leading_to_it() {
    // zlib of [65, 1].
    const DATA: &[u8] = &[120, 156, 115, 100, 4, 0, 0, 133, 0, 67];
    for encrypted in [true, false] {
        for parms in ["4 0 R", "3 0 R"] {
            let entries =
                format!("/Filter /FlateDecode /DecodeParms {parms} /Predictor 2 /Columns 2");
            let file = rc4::file_with(
                &[
                    Part::Object("<< /Type /Catalog /Pages 2 0 R >>", b""),
                    Part::Object("<< /Type /Pages /Kids [] /Count 0 >>", b""),
                    Part::StreamWith(&entries, DATA),
                ],
                &[(4, 3)],
                encrypted,
            );
            let pdf = Pdf::new(file).unwrap();
            let stream = |n: i32| {
                pdf.xref()
                    .get::<Stream<'_>>(ObjectIdentifier::new(n, 0))
                    .unwrap()
            };
            for n in [3, 4] {
                assert_eq!(
                    stream(n).decoded().unwrap().as_ref(),
                    [65, 1],
                    "encrypted: {encrypted}, /DecodeParms {parms}, read as {n}"
                );
            }
            assert_eq!(stream(3).identity(), stream(4).identity());
        }
    }
}
