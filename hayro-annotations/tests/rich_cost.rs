//! End-to-end scaling for rich `FreeText` annotation synthesis.
use std::fmt::Write;
use std::time::{Duration, Instant};

use hayro_annotations::{Appearances, Look, Unsupported, View};
use hayro_interpret::{AnnotationSelection, InterpreterSettings};

mod common;

fn generate(input: &str) -> Result<usize, Unsupported> {
    let annotation = format!(
        "<< /Subtype /FreeText /Rect [10 10 100 80] /BS << /W 0 >> /RC (\u{feff}{input}) >>"
    );
    let pdf = common::document("/Annots [4 0 R]", vec![annotation]);
    let page = &pdf.pages()[0];
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    let drawn = appearances.page(page, AnnotationSelection::View, &View::of(page));
    match drawn[0].look() {
        Look::Generated(appearance) => Ok(appearance.glyphs.len()),
        Look::Unsupported(why) => Err(*why),
        _ => panic!("expected a synthesized annotation or refusal"),
    }
}

fn check_rich(input: &str, count: usize) {
    let mut samples = [Duration::ZERO; 5];
    for sample in &mut samples {
        let start = Instant::now();
        assert_eq!(generate(input), Ok(1));
        *sample = start.elapsed();
    }
    samples.sort();
    eprintln!(
        "{count} attributes, {} bytes, median {:?}",
        input.len(),
        samples[2]
    );
    assert!(samples[2] < Duration::from_secs(3));
}

fn check(attrs: &str, count: usize) {
    check_rich(&format!("<p{attrs}>A</p>"), count);
}

#[test]
fn many_distinct_and_duplicate_attributes() {
    for count in [32_000, 64_000, 128_000] {
        let mut attributes = String::new();
        for i in 0..count {
            write!(attributes, " a{i}=x").unwrap();
        }
        check(&attributes, count);
        check(&" a=x".repeat(count), count);
    }
}

#[test]
fn ignored_names_from_one_atom_bucket() {
    let mut attributes = String::new();
    let (mut counter, mut delta, mut shift, mut count) = (0_u64, 0_u64, 0, 0);
    for byte in include_bytes!("one_bucket_attribute_names.bin") {
        delta |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            counter += delta;
            write!(attributes, " n{counter:011x}=x").unwrap();
            (delta, shift) = (0, 0);
            count += 1;
            if matches!(count, 16_384 | 32_768 | 65_536) {
                check(&attributes, count);
            }
        }
    }
    assert_eq!(count, 65_536);
}

fn attributes_whose_names_hash_alike() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut attributes = String::new();
    for a in CHARS {
        for b in CHARS {
            for c in CHARS {
                let abc = String::from_utf8(vec![*a, *b, *c]).unwrap();
                write!(attributes, " {abc}z{abc}=x").unwrap();
            }
        }
    }
    attributes
}

#[test]
fn short_names_in_a_paragraph_and_repeated_body() {
    let attrs = attributes_whose_names_hash_alike();
    check(&attrs, 46_656);
    check_rich(&format!("<body{attrs}><body a=b><p>A</p>"), 46_656);
}

#[test]
fn rich_text_recovery_bounds() {
    let open: String = (0..200).map(|i| format!("<b title={i}>")).collect();
    for (input, error) in [
        (
            format!("<p>漢{}</p>", "&".repeat(1_600_000)),
            Unsupported::Character('漢'),
        ),
        (
            format!("{}{}", "<span>".repeat(100_000), "</p>".repeat(100_000)),
            Unsupported::RichText,
        ),
        (
            format!("<p>{open}{}", "</p><p>x".repeat(50_000)),
            Unsupported::RichText,
        ),
    ] {
        let start = Instant::now();
        assert_eq!(generate(&input), Err(error));
        assert!(start.elapsed() < Duration::from_secs(10));
    }
}
