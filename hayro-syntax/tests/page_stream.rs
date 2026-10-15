//! A page's decoded content is held once.

use hayro_syntax::Pdf;

mod common;
use common::allocations::measure;

/// Flate content is kept as decoded, not copied into the page's cache.
#[test]
fn decoded_content_is_not_copied() {
    let data = vec![b' '; (1 << 20) - 16];
    let content = common::stream("/Filter /FlateDecode", &common::stored_zlib(&data, 65_535));
    let pdf = Pdf::new(common::one_page(100, "/Contents 4 0 R", vec![content])).unwrap();
    let page = &pdf.pages()[0];
    let (content, used) = measure(|| page.page_stream().unwrap());
    assert_eq!(content, data);
    // The decoder's growing buffer fits; a second content-sized buffer does not.
    assert!(used.peak < data.len() * 3 / 2, "{used:?}");
}

/// A stream an array of contents names many times is decoded once, by the strict
/// and the lenient reading alike, though the content holds it each time.
#[test]
fn a_stream_named_many_times_is_decoded_once() {
    use hayro_syntax::work::TOLD;

    let data = common::noise(4096, 7);
    let stored = common::stored_zlib(&data, 1000);
    let content = common::stream("/Filter /FlateDecode", &stored);
    for n in [64, 128] {
        let entries = format!("/Contents [{}]", "4 0 R ".repeat(n));
        let pdf = Pdf::new(common::one_page(100, &entries, vec![content.clone()])).unwrap();
        let page = &pdf.pages()[0];
        let expected: Vec<u8> = (0..n)
            .flat_map(|_| data.iter().copied().chain(*b" "))
            .collect();
        TOLD.set(0);
        assert_eq!(page.try_page_stream().unwrap(), expected);
        let strict = TOLD.get();
        TOLD.set(0);
        assert_eq!(page.page_stream().unwrap(), expected);
        let lenient = TOLD.get();
        // One decoding takes the stored bytes and makes the data.
        let once = stored.len() + data.len();
        assert!(
            strict <= 2 * once && lenient <= 2 * once,
            "{n} names: {strict} and {lenient} bytes of work, {once} to decode once"
        );
    }
}
