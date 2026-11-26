//! Rendering of optional-content groups and membership expressions.
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};

fn drawn(pdf: Vec<u8>) -> bool {
    let pdf = Pdf::new(pdf).expect("valid pdf");
    let pixmap = render(
        &pdf.pages()[0],
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: 1.0,
            y_scale: 1.0,
            bg_color: WHITE,
        },
    );
    let width = pixmap.width() as usize;
    let at = (25 * width + 25) * 4;
    let pixel = &pixmap.data_as_u8_slice()[at..at + 4];
    match pixel {
        [0, 255, 0, 255] => true,
        [255, 255, 255, 255] => false,
        other => panic!("unexpected center pixel {other:?}"),
    }
}

fn shows(props: &str, member: &str, objects: &[&str], content: &str) -> bool {
    page_shows(props, member, objects, content, "")
}

fn page_shows(props: &str, member: &str, objects: &[&str], content: &str, resources: &str) -> bool {
    use std::fmt::Write as _;
    let mut pdf = String::from("%PDF-1.7\n");
    let mut offsets = vec![];
    let heads = [
        format!("<< /Type /Catalog /Pages 2 0 R /OCProperties << {props} >> >>"),
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_owned(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 50 50] /Contents 4 0 R /Resources << {resources} /Properties << /M {member} >> >> >>"
        ),
        format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ),
    ];
    for body in heads
        .iter()
        .map(String::as_str)
        .chain(objects.iter().copied())
    {
        offsets.push(pdf.len());
        writeln!(pdf, "{} 0 obj\n{body}\nendobj", offsets.len()).unwrap();
    }
    let xref = pdf.len();
    let size = offsets.len() + 1;
    writeln!(pdf, "xref\n0 {size}\n0000000000 65535 f ").unwrap();
    for offset in offsets {
        writeln!(pdf, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(
        pdf,
        "trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF"
    )
    .unwrap();
    drawn(pdf.into_bytes())
}

const FILL: &str = "/OC /M BDC 0 1 0 rg 0 0 50 50 re f EMC";
const GROUP: &str = "<< /Type /OCG >>";
const OFF: &str = "/OCGs [5 0 R] /D << /BaseState /OFF >>";

#[test]
fn viewing_configuration_and_group_overrides() {
    let configs = format!(
        "{OFF} /Configs [<< /Intent /Design >> << /Intent [/View] /ON [5 0 R] >> << /Intent /All /OFF [5 0 R] >>]"
    );
    assert!(shows(&configs, "5 0 R", &[GROUP], FILL));
    let both = "/OCGs [5 0 R] /D << /ON [5 0 R] /OFF [5 0 R] >>";
    assert!(!shows(both, "5 0 R", &[GROUP], FILL));
    assert!(shows(OFF, "5 0 R", &["<< /Intent /Design >>"], FILL));
    let usage = "<< /Usage << /View << /ViewState /OFF >> >> >>";
    assert!(!shows("", "5 0 R", &[usage], FILL));
    let automatic = "/OCGs [5 0 R] /D << /AS [<< /Event /View /OCGs [5 0 R] /View << /ViewState /OFF >> >> << /Event /Print /OCGs [5 0 R] /View << /ViewState /ON >> >>] >>";
    assert!(!shows(automatic, "5 0 R", &[GROUP], FILL));
    assert!(shows(
        automatic,
        "5 0 R",
        &["<< /Usage << /View << /ViewState /ON >> >> >>"],
        FILL
    ));
}

#[test]
fn membership_policies() {
    for (groups, expected) in [
        ("[42]", [true; 4]),
        ("[5 0 R]", [false, false, true, true]),
        ("[6 0 R]", [true, true, false, false]),
        ("[5 0 R 6 0 R]", [true, false, true, false]),
        ("5 0 R", [false; 4]),
    ] {
        for (policy, expected) in ["AnyOn", "AllOn", "AnyOff", "AllOff"]
            .into_iter()
            .zip(expected)
        {
            let m = format!("<< /Type /OCMD /OCGs {groups} /P /{policy} >>");
            assert_eq!(shows(OFF, &m, &[GROUP, GROUP], FILL), expected, "{m}");
        }
    }
}

#[test]
fn expressions_and_depth() {
    for (ve, objects, on) in [
        ("[/And 5 0 R [/Not 6 0 R]]", vec![GROUP, GROUP], false),
        ("[/Or 5 0 R [/Not 5 0 R]]", vec![GROUP], true),
        ("5 0 R", vec!["[/Not 5 0 R]"], true),
        ("[/Or 5 0 R]", vec!["[/Not 5 0 R]"], false),
        ("[/Or 5 0 R [/Or 5 0 R]]", vec!["[/Not 5 0 R]"], true),
    ] {
        let member = format!("<< /Type /OCMD /VE {ve} /OCGs 5 0 R >>");
        assert_eq!(shows(OFF, &member, &objects, FILL), on, "{ve}");
    }
    let mut diamond: Vec<_> = (6..36).map(|n| format!("[/And {n} 0 R {n} 0 R]")).collect();
    diamond.push("[/Or << >>]".into());
    assert!(shows(
        "",
        "<< /Type /OCMD /VE 5 0 R >>",
        &diamond.iter().map(String::as_str).collect::<Vec<_>>(),
        FILL
    ));
}

#[test]
fn a_group_written_in_place_in_a_shared_list_is_configured() {
    assert!(!shows(
        "/OCGs 5 0 R /D << /BaseState /OFF >>",
        "<< /Type /OCMD /OCGs 5 0 R >>",
        &["[<< /Type /OCG >>]"],
        FILL
    ));
}

#[test]
fn a_group_that_is_a_stream_is_read_without_its_data() {
    assert!(!shows(
        "",
        "5 0 R",
        &[
            "<< /Type /OCG /Usage << /View << /ViewState /OFF >> >> /F (elsewhere) /Length 1 >>\nstream\nx\nendstream"
        ],
        FILL
    ));
}

#[test]
fn nested_content_inherits_parent_visibility() {
    assert!(!shows(
        OFF,
        "5 0 R",
        &[GROUP],
        "/OC /M BDC /Span << >> BDC 0 1 0 rg 0 0 50 50 re f EMC EMC"
    ));
}

#[test]
fn an_unclosed_level_in_a_form_ends_with_the_form() {
    let form = "<< /Subtype /Form /BBox [0 0 50 50] /Resources << /Properties << /M 5 0 R >> >> /Length 10 >>\nstream\n/OC /M BDC\nendstream";
    assert!(page_shows(
        OFF,
        "5 0 R",
        &[GROUP, form],
        "/X Do 0 1 0 rg 0 0 50 50 re f",
        "/XObject << /X 6 0 R >>"
    ));
}

#[test]
fn hidden_text_can_clip_subsequent_visible_content() {
    let font = "/Font << /F << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >>";
    for (mode_at_end, visible) in [(7, false), (0, true)] {
        let content = format!(
            "/OC /M BDC BT /F 40 Tf 7 Tr 1 0 0 1 60 60 Tm (A) Tj {mode_at_end} Tr ET EMC 0 1 0 rg 0 0 50 50 re f"
        );
        assert_eq!(page_shows(OFF, "5 0 R", &[GROUP], &content, font), visible);
    }
}
