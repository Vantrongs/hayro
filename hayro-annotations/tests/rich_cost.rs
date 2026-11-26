use hayro_annotations::{Align, Color, Family, Size, Style, rich_text};
use std::fmt::Write;
use std::time::{Duration, Instant};

fn check(attributes: &str, count: usize) {
    let input = format!("<p{attributes}>A</p>");
    let style = Style {
        face: Family::Sans.into(),
        size: Size::Points(12.),
        color: Color::BLACK,
        align: Align::Left,
    };
    let mut samples = [Duration::ZERO; 5];
    for sample in &mut samples {
        let start = Instant::now();
        let spans = rich_text(&input, &style, true).unwrap();
        *sample = start.elapsed();
        assert_eq!(spans[0].text, "A\n");
    }
    samples.sort();
    eprintln!(
        "{count} attributes, {} bytes, median {:?}",
        input.len(),
        samples[2]
    );
    assert!(samples[2] < Duration::from_secs(3));
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
