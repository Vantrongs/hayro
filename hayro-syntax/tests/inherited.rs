//! A value pages inherit from the page tree is held once for all of them, not
//! copied for each.

use hayro_syntax::Pdf;
use hayro_syntax::page::{Inheritable, Inheritance};

mod common;
use common::allocations::measure;

/// `n` pages under a node whose `key` is `value(n)`, a value as large as the file.
fn file(n: usize, key: &str, value: fn(usize) -> String) -> Vec<u8> {
    let kids: Vec<String> = (0..n).map(|page| format!("{} 0 R", page + 3)).collect();
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {n} /{key} {} >>",
            kids.join(" "),
            value(n)
        )
        .into_bytes(),
    ];
    objects.extend((0..n).map(|_| b"<< /Type /Page /Parent 2 0 R >>".to_vec()));
    common::file(&objects, "")
}

/// What loading `file` and looking up every entry of every page, as the geometry of
/// the pages is found, holds at most, once it is checked that `n` pages load.
fn peak(file: Vec<u8>, n: usize) -> usize {
    let ((), used) = measure(|| {
        let pdf = Pdf::new(file).unwrap();
        assert_eq!(pdf.pages().len(), n);
        let mut tree = Inheritance::new(&pdf);
        let mut found = vec![];
        for page in pdf.pages().iter() {
            for key in [
                Inheritable::MediaBox,
                Inheritable::CropBox,
                Inheritable::Rotate,
                Inheritable::Resources,
            ] {
                found.push(tree.get(page, key).unwrap());
            }
        }
        assert!(found.iter().any(Option::is_some));
    });
    used.peak
}

// A string or name the pages inherit, which reading decodes into bytes of its own,
// is held once however many pages inherit it: what loading a file holds grows with
// the file, not with its square.
#[test]
fn a_value_pages_inherit_is_held_once() {
    type Value = (&'static str, fn(usize) -> String);
    let values: [Value; 4] = [
        ("Rotate", |n| format!("<{}>", "41".repeat(64 * n))),
        ("MediaBox", |n| format!("({})", "\\101".repeat(64 * n))),
        ("CropBox", |n| format!("/{}", "#41".repeat(64 * n))),
        ("Resources", |n| format!("<{}>", "42".repeat(64 * n))),
    ];
    for (key, value) in values {
        let [small, large] = [500, 1000].map(|n| peak(file(n, key, value), n));
        assert!(
            large <= small * 25 / 10,
            "/{key}: {small} bytes held at most for 500 pages, {large} for 1000"
        );
    }
}
