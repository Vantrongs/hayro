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
    // What a thread allocates once, whichever thread runs the test, is not counted:
    // the largest dictionary is parsed once before.
    let warm = [&objects[..2], &objects[objects.len() - 1..]].concat();
    Pdf::new(file(&warm, ""))
        .unwrap()
        .xref()
        .get::<Dict<'_>>(ObjectIdentifier::new(3, 0))
        .unwrap();
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

/// Key `i`: a 64-byte prefix every key shares, then `i` in 5 digits.
fn long_key(i: usize) -> String {
    format!("{}{i:05}", "P".repeat(64))
}

/// A dictionary of `n` keys of a long common prefix ([`long_key`]), in their order
/// with a `step` of 1, shuffled with a prime step none of the `n` is a multiple of,
/// each written `times` times, the later ones after all the first, valued with the
/// time it is written.
fn dict_of_long_keys(n: usize, step: usize, times: usize) -> String {
    let mut text = String::from("<<");
    for time in 0..times {
        for i in (0..n).map(|i| i * step % n) {
            text.push_str(&format!(" /{} {time}", long_key(i)));
        }
    }
    text.push_str(" >>");
    text
}

/// The work ([`hayro_syntax::work::TOLD`]) of opening a file holding `dict` and
/// reading its every key, less that of the same file holding an empty dictionary.
#[cfg(debug_assertions)]
fn told_for(dict: &str, keys: usize) -> usize {
    use hayro_syntax::work::TOLD;
    let told = |dict: &str, keys: usize| {
        let objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
            dict.as_bytes().to_vec(),
        ];
        let data = file(&objects, "");
        TOLD.set(0);
        let pdf = Pdf::new(data).unwrap();
        let dict = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(3, 0))
            .unwrap();
        assert_eq!(dict.len(), keys);
        let told = TOLD.get();
        for i in 0..keys {
            assert!(dict.get::<i32>(long_key(i).as_bytes()).is_some());
        }
        told
    };
    // Opening reads the trailer into a map keyed at random, which compares its keys
    // only where their hashes collide: the least of a few opens does not.
    let least = |dict: &str, keys: usize| (0..4).map(|_| told(dict, keys)).min().unwrap();
    least(dict, keys) - least("<< >>", 0)
}

/// Every comparison of two keys a dictionary is parsed with is counted, in bytes
/// ([`hayro_syntax::work::TOLD`]), on both sides of the 8 keys past which they are
/// sorted: a key written again is compared whole with its first entry, a small
/// dictionary's key with each before it, and a large dictionary's keys at least
/// with their neighbours in order, each over the prefix they share. And the work
/// counted stays within the dictionary's bytes times 8, the keys a key is compared
/// with while there are few, and the logarithm of its keys sorting them takes.
#[cfg(debug_assertions)]
#[test]
fn comparing_a_dictionarys_keys_is_counted() {
    let key = long_key(0).len();
    let mut large = vec![];
    for (n, step, times) in [2, 7, 8, 9, 10, 16, 1000, 2000]
        .into_iter()
        .flat_map(|n| [(n, 1, 1), (n, 1, 2), (n, 7919, 1), (n, 7919, 2)])
    {
        let dict = dict_of_long_keys(n, step, times);
        let told = told_for(&dict, n);
        // Each distinct pair is compared over the prefix: while there are at most 8,
        // as each key is compared with each before it; past that, at least once, to
        // find them in order.
        let distinct = if n <= 8 { n * (n - 1) / 2 } else { n - 1 };
        let least = distinct * key + (times - 1) * n * key;
        let case = format!("{n} keys × {times} by {step}");
        assert!(told >= least, "{case}: {told} counted, {least} done");
        // Each key compared with at most 8 others while there are few, then each
        // with a logarithm of others to sort them.
        let log = (usize::BITS - n.leading_zeros()) as usize;
        let most = dict.len() * (8 + log);
        assert!(told <= most, "{case}: {told} counted, over {most}");
        if n >= 1000 {
            large.push(told);
        }
    }
    // The work of 2000 keys is that of 1000 doubled, and a logarithm's step.
    for at in 0..4 {
        let (small, large) = (large[at], large[at + 4]);
        assert!(
            large <= small * 25 / 10,
            "{small} for 1000 keys, {large} for 2000"
        );
    }
}

/// A dictionary of `keys` keys `K01`, `K02` and on, in their order or else the
/// other way, then `Z` written `times` times, each time valued with an array of 4096
/// numbers whose first is the time it is written.
fn repeated_large_values(keys: usize, ascending: bool, times: usize) -> String {
    let mut text = String::from("<<");
    for i in 1..=keys {
        let i = if ascending { i } else { keys + 1 - i };
        text.push_str(&format!(" /K{i:02} {i}"));
    }
    let rest = " 0".repeat(4095);
    for time in 0..times {
        text.push_str(&format!(" /Z [{time}{rest}]"));
    }
    text.push_str(" >>");
    text
}

/// A key written again replaces its value at once, on both sides of the 8 keys past
/// which a dictionary's keys are sorted, whether its keys came in order or not: the
/// memory parsing a dictionary holds at most is that of the values it keeps, and
/// one more, however often a key is written again with a large value.
#[test]
fn a_replaced_value_is_dropped_when_it_is_replaced() {
    for (keys, ascending) in [3, 8, 12]
        .into_iter()
        .flat_map(|keys| [(keys, true), (keys, false)])
    {
        let [once, twice, often] = [1, 2, 64].map(|times| {
            let objects = vec![
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Kids [] /Count 0 >>".to_vec(),
                repeated_large_values(keys, ascending, times).into_bytes(),
            ];
            let data = file(&objects, "");
            let (pdf, used) = measure(move || Pdf::new(data).unwrap());
            let dict = pdf
                .xref()
                .get::<Dict<'_>>(ObjectIdentifier::new(3, 0))
                .unwrap();
            assert_eq!(dict.len(), keys + 1);
            let last = dict.get::<hayro_syntax::object::Array<'_>>(b"Z").unwrap();
            assert_eq!(last.iter::<i32>().next(), Some(times as i32 - 1));
            assert_eq!(last.iter::<i32>().count(), 4096);
            used.peak
        });
        // Replacing a value once holds two at most; replacing it 63 times, no more.
        let case = format!("{keys} keys, ascending {ascending}");
        assert!(twice > once, "{case}: {once} bytes once, {twice} twice");
        assert!(
            often <= twice,
            "{case}: {twice} bytes replacing once, {often} replacing 63 times"
        );
    }
}
