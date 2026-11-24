//! Release decoded page content between renders.
#[path = "../../hayro-syntax/tests/common/allocations.rs"]
mod allocations;
#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

use hayro::{
    PixmapSettings, RenderCache, RenderSettings, hayro_interpret::InterpreterSettings,
    hayro_syntax::Pdf, render,
};

#[test]
fn clearing_content_releases_the_stream_and_preserves_pixels() {
    let mut content = b"1 0 0 rg 0 0 5 5 re f".to_vec();
    content.resize(64 * 1024, b' ');
    let pdf = Pdf::new(fixtures::one_page(
        10,
        "/Contents 4 0 R",
        vec![fixtures::stream("", &content)],
    ))
    .unwrap();
    let cache = RenderCache::new();
    let draw = || {
        render(
            &pdf.pages()[0],
            &cache,
            &InterpreterSettings::default(),
            &RenderSettings::default(),
            &PixmapSettings::default(),
        )
    };
    let first = draw();
    assert!(first.data_as_u8_slice().contains(&255));
    let (_, released) = allocations::measure(|| cache.clear_page_content());
    assert!(released.freed >= content.len());
    assert_eq!(draw().data_as_u8_slice(), first.data_as_u8_slice());
}
