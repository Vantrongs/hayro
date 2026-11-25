//! SVG tiling pattern regression tests.

use hayro_interpret::InterpreterSettings;
use hayro_svg::hayro_syntax::Pdf;
use hayro_svg::{RenderCache, SvgRenderSettings, convert};

#[test]
fn clipped_and_unclipped_zero_area_fills_share_blue_pattern() {
    let pdf = Pdf::new(include_bytes!("tiling_pattern.pdf").to_vec()).unwrap();
    let svg = convert(
        &pdf.pages()[0],
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &SvgRenderSettings::default(),
    );

    let mut patterns = svg.match_indices("<pattern ");
    let (start, _) = patterns.next().expect("missing pattern");
    assert!(patterns.next().is_none(), "expected one shared pattern");
    let (pattern_tag, contents) = svg[start..].split_once('>').unwrap();
    let (contents, _) = contents.split_once("</pattern>").unwrap();
    assert!(contents.contains(r##" fill="#0000ff""##));
    let paint = format!("url(#{})", attribute(pattern_tag, "id").unwrap());

    let (page, _) = svg.split_once("<defs").unwrap();
    let mut tags = page.split('<').skip(2).map(str::trim_end);
    for height in [24, 40, 72] {
        if height != 72 {
            let group = tags.next().unwrap();
            assert!(group.starts_with("g "), "expected clip group at y={height}");
            attribute(group, "clip-path").expect("missing group clip");
        }

        let path = tags.next().unwrap();
        assert!(path.starts_with("path "));
        assert_eq!(
            attribute(path, "d").unwrap(),
            format!("M0,{height} L80,{height}")
        );
        assert_eq!(attribute(path, "stroke"), Some(paint.as_str()));
        if height == 72 {
            assert_eq!(attribute(path, "clip-path"), None);
        } else {
            assert_eq!(tags.next(), Some("/g>"));
        }
    }
    assert!(tags.next().is_none());

    for tag in svg
        .split('<')
        .filter_map(|s| s.split_once('>').map(|(tag, _)| tag))
    {
        if let Some(clip) = attribute(tag, "clip-path") {
            let id = clip
                .strip_prefix("url(#")
                .unwrap()
                .strip_suffix(')')
                .unwrap();
            assert!(
                svg.contains(&format!(r#"<clipPath id="{id}">"#)),
                "missing {clip}"
            );
        }
    }
}

fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    tag.split_once(&format!(" {name}=\""))?
        .1
        .split_once('"')
        .map(|(value, _)| value)
}
