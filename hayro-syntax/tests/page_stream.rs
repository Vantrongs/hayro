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
