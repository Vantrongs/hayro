//! Dictionaries parsed when a file opens, which the file holds while it is open: a
//! key's value is its last entry's, however many entries the dictionary has, and the
//! memory a file of many large dictionaries holds stays under a ceiling.

use hayro_syntax::Pdf;
use hayro_syntax::object::{Dict, ObjectIdentifier};
use std::collections::BTreeMap;

mod common;
use common::allocations::measure;
use common::ceilings::check_ceilings;
use common::file;

/// A dictionary of keys `K0` to `K{n - 1}`, each valued its index, then, where there
/// are two keys or more, `K1` again written with an escape, and `K0` again, and the
/// values each key ends with.
fn dict_with_repeats(n: usize) -> (String, BTreeMap<String, i32>) {
    let mut text = String::from("<<");
    let mut values = BTreeMap::new();
    for i in 0..n {
        text.push_str(&format!(" /K{i} {i}"));
        values.insert(format!("K{i}"), i as i32);
    }
    if n >= 2 {
        text.push_str(" /#4B1 101 /K0 100");
        values.insert("K0".into(), 100);
        values.insert("K1".into(), 101);
    }
    text.push_str(" >>");
    (text, values)
}

#[test]
fn a_parsed_dictionary_keeps_each_keys_last_value() {
    for n in [1, 2, 7, 8, 9, 10, 16, 300] {
        let (text, values) = dict_with_repeats(n);
        let objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            text.clone().into_bytes(),
            format!("<< /Nested {text} >>").into_bytes(),
        ];
        let pdf = Pdf::new(file(&objects, "")).unwrap();
        let top = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(3, 0))
            .unwrap();
        let nested = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(4, 0))
            .unwrap()
            .get::<Dict<'_>>(b"Nested")
            .unwrap();
        for dict in [top, nested] {
            assert_eq!(dict.len(), values.len(), "{n} keys");
            let keys: Vec<String> = dict.keys().map(|k| k.as_str().to_owned()).collect();
            assert_eq!(keys, values.keys().cloned().collect::<Vec<_>>(), "{n} keys");
            for (key, value) in &values {
                assert_eq!(dict.get::<i32>(key.as_bytes()), Some(*value), "{n}: {key}");
            }
            assert_eq!(dict.get::<i32>(b"K"), None, "{n} keys");
            assert_eq!(
                dict.get::<i32>(format!("K{n}").as_bytes()),
                None,
                "{n} keys"
            );
            assert_eq!(dict.entries().count(), values.len(), "{n} keys");
        }
    }
}

/// A file of 300 dictionaries of 9 to 308 keys, as a file whose every page lists the
/// resources of the pages before it, is opened, which parses each dictionary and
/// keeps it: the bytes held at most and the allocations stay under their ceilings.
#[test]
fn opening_a_file_of_large_dictionaries_stays_under_its_ceilings() {
    let mut objects = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
    ];
    for i in 0..300 {
        let mut text = String::from("<<");
        for j in 0..i + 9 {
            text.push_str(&format!(" /Im{j} {} 0 R", j + 3));
        }
        text.push_str(" >>");
        objects.push(text.into_bytes());
    }
    let data = file(&objects, "");
    let (pdf, used) = measure(move || Pdf::new(data).unwrap());
    // Every dictionary is read, as the pages' resources are.
    for i in 0..300 {
        let dict = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(i + 3, 0))
            .unwrap();
        assert_eq!(dict.len(), i as usize + 9);
    }
    check_ceilings(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/ceilings/parsed_dicts.txt"
        ),
        "WRITE_CEILINGS=1 cargo test -p hayro-syntax --test parsed_dicts",
        &[
            ("open_peak_bytes", used.peak),
            ("open_allocations", used.count),
        ],
    );
}
