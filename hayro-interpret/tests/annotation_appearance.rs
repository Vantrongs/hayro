//! Annotations without `/AP` get the appearance `PDFium` generates for their subtype.

use hayro_interpret::annotation::{generate, normal_appearance, rect_transform};
use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::hayro_syntax::object::{Array, Dict};
use hayro_interpret::{
    BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterSettings,
    Paint, SoftMask,
};
use kurbo::{Affine, BezPath, Shape};

mod common;

#[derive(Debug, PartialEq)]
enum Ev {
    Fill {
        bbox: [i32; 4],
        rgba: [u8; 4],
    },
    Stroke {
        bbox: [i32; 4],
        rgba: [u8; 4],
        width: f32,
        dashed: bool,
    },
    Group {
        opacity: f32,
        blend: BlendMode,
    },
    EndGroup,
}

#[derive(Default)]
struct Rec(Vec<Ev>);

fn bbox(path: &BezPath) -> [i32; 4] {
    let b = path.bounding_box();
    [b.x0, b.y0, b.x1, b.y1].map(|v| v.round() as i32)
}

impl<'a> Device<'a> for Rec {
    fn draw_path(&mut self, path: &BezPath, props: DrawProps<'a>, mode: &DrawMode) {
        let Paint::Color(c) = &props.paint else {
            panic!("solid paint")
        };
        let rgba = c.to_rgba().to_rgba8();
        let mut path = path.clone();
        path.apply_affine(props.transform);
        let bbox = bbox(&path);
        let stroke = |s: &hayro_interpret::StrokeProps| Ev::Stroke {
            bbox,
            rgba,
            width: s.line_width,
            dashed: !s.dash_array.is_empty(),
        };
        match mode {
            DrawMode::Fill(_) => self.0.push(Ev::Fill { bbox, rgba }),
            DrawMode::Stroke(s) => self.0.push(stroke(s)),
            DrawMode::FillAndStroke(_, s) => {
                self.0.push(Ev::Fill { bbox, rgba });
                self.0.push(stroke(s));
            }
            DrawMode::Invisible => panic!("invisible draw"),
        }
    }
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, opacity: f32, _: Option<SoftMask<'a>>, blend: BlendMode) {
        self.0.push(Ev::Group { opacity, blend });
    }
    fn draw_glyph_run(&mut self, _: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {
        self.0.push(Ev::EndGroup);
    }
}

/// Draws a 200 × 200 pt page with these annotation dictionaries.
fn draw(annots: &[&str], print: bool) -> Vec<Ev> {
    let refs: Vec<String> = (0..annots.len())
        .map(|i| format!("{} 0 R", i + 4))
        .collect();
    let objects = annots
        .iter()
        .map(|a| format!("<< /Type /Annot {a} >>").into_bytes())
        .collect();
    let file = common::one_page(
        200,
        &format!("/Resources << >> /Annots [{}]", refs.join(" ")),
        objects,
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    let settings = InterpreterSettings {
        annotation_print: print,
        ..Default::default()
    };
    let mut rec = Rec::default();
    common::interpret_first_page(&pdf, settings, &mut rec);
    rec.0
}

const YELLOW: [u8; 4] = [255, 255, 0, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];
const RED: [u8; 4] = [255, 0, 0, 255];

#[test]
fn highlight_fills_quads_in_one_multiply_group_at_ca() {
    let ev = draw(
        &[
            "/Subtype /Highlight /F 4 /CA 0.5 /Rect [10 10 90 40] /QuadPoints [10 40 90 40 10 30 90 30 10 22 60 22 10 12 60 12]",
        ],
        false,
    );
    assert_eq!(
        ev,
        vec![
            Ev::Group {
                opacity: 0.5,
                blend: BlendMode::Multiply
            },
            Ev::Fill {
                bbox: [10, 12, 90, 40],
                rgba: YELLOW
            },
            Ev::EndGroup,
        ]
    );
}

#[test]
fn text_lines_follow_pdfium_geometry() {
    let quad = "/Rect [10 10 50 20] /QuadPoints [10 20 50 20 10 10 50 10]";
    let under = draw(&[&format!("/Subtype /Underline {quad}")], false);
    assert_eq!(
        under,
        vec![Ev::Stroke {
            bbox: [10, 11, 50, 11],
            rgba: BLACK,
            width: 1.0,
            dashed: false
        }]
    );
    let strike = draw(&[&format!("/Subtype /StrikeOut /C [1 0 0] {quad}")], false);
    assert_eq!(
        strike,
        vec![Ev::Stroke {
            bbox: [10, 15, 50, 15],
            rgba: RED,
            width: 1.0,
            dashed: false
        }]
    );
    let squiggle = draw(&[&format!("/Subtype /Squiggly {quad}")], false);
    assert_eq!(
        squiggle,
        vec![Ev::Stroke {
            bbox: [10, 10, 50, 12],
            rgba: BLACK,
            width: 1.0,
            dashed: false
        }]
    );
}

/// A Squiggly quad 10^9 pt wide is drawn across the page and a step beyond, not
/// along its whole width.
#[test]
fn a_huge_squiggly_is_drawn_across_the_page() {
    let ev = draw(
        &["/Subtype /Squiggly /Rect [10 10 50 20] \
           /QuadPoints [10 20 1000000000 20 10 10 1000000000 10]"],
        false,
    );
    assert_eq!(
        ev,
        vec![Ev::Stroke {
            bbox: [10, 10, 202, 12],
            rgba: BLACK,
            width: 1.0,
            dashed: false
        }]
    );
}

#[test]
fn ink_strokes_each_path_at_border_width_with_dash() {
    let ev = draw(
        &[
            "/Subtype /Ink /C [1 0 0] /BS << /W 3 /S /D /D [2 1] >> /Rect [0 0 100 100] /InkList [[10 10 20 30 40 20] [50 50 60 60]]",
        ],
        false,
    );
    assert_eq!(
        ev,
        vec![
            Ev::Stroke {
                bbox: [10, 10, 40, 30],
                rgba: RED,
                width: 3.0,
                dashed: true
            },
            Ev::Stroke {
                bbox: [50, 50, 60, 60],
                rgba: RED,
                width: 3.0,
                dashed: true
            },
        ]
    );
}

#[test]
fn square_and_circle_fill_ic_and_stroke_c_inside_rect() {
    let ev = draw(
        &["/Subtype /Square /IC [1 1 0] /C [1 0 0] /Border [0 0 2] /Rect [10 10 50 30]"],
        false,
    );
    let inner = [11, 11, 49, 29];
    assert_eq!(
        ev,
        vec![
            Ev::Fill {
                bbox: inner,
                rgba: YELLOW
            },
            Ev::Stroke {
                bbox: inner,
                rgba: RED,
                width: 2.0,
                dashed: false
            },
        ]
    );
    let circle = draw(
        &["/Subtype /Circle /IC [0] /Rect [10 10 50 30] /Border [0 0 0]"],
        false,
    );
    assert_eq!(
        circle,
        vec![Ev::Fill {
            bbox: [10, 10, 50, 30],
            rgba: BLACK
        }]
    );
    // No /C and no /IC: nothing to paint (the spec gives /C no default).
    assert_eq!(
        draw(&["/Subtype /Square /Rect [10 10 50 30]"], false),
        vec![]
    );
}

#[test]
fn note_is_a_20pt_icon_at_the_bottom_left() {
    let ev = draw(&["/Subtype /Text /Rect [100 100 140 160]"], false);
    let icon = [100, 100, 120, 120];
    assert_eq!(ev.len(), 2, "{ev:?}");
    let Ev::Fill { bbox, rgba } = &ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!(
        (*rgba, bbox[0] - icon[0] <= 1, bbox[3] - icon[3] <= 1),
        (YELLOW, true, true)
    );
    assert!(
        matches!(
            ev[1],
            Ev::Stroke {
                rgba: BLACK,
                width: 1.0,
                ..
            }
        ),
        "{ev:?}"
    );
}

#[test]
fn flags_and_existing_appearances_decide_what_is_generated() {
    let ink = "/Subtype /Ink /Rect [0 0 100 100] /InkList [[10 10 20 20]]";
    // Hidden; NoView on screen; without Print when printing.
    assert_eq!(draw(&[&format!("{ink} /F 2")], false), vec![]);
    assert_eq!(draw(&[&format!("{ink} /F 32")], false), vec![]);
    assert_eq!(draw(&[&format!("{ink} /F 0")], true), vec![]);
    assert_eq!(draw(&[&format!("{ink} /F 36")], true).len(), 1);
    // A normal appearance that cannot be drawn is not replaced.
    assert_eq!(
        draw(&[&format!("{ink} /AP << /N 99 0 R >>")], false),
        vec![]
    );
    // FreeText and Popup need text layout and are not generated.
    assert_eq!(
        draw(
            &["/Subtype /FreeText /Rect [0 0 100 100] /Contents (x) /DA (/Helv 12 Tf 0 g)"],
            false
        ),
        vec![]
    );
}

/// The first page's annotations, in a file with `objects` numbered from 4.
fn annotations(page_entries: &str, objects: Vec<Vec<u8>>) -> Pdf {
    Pdf::new(common::one_page(200, page_entries, objects)).expect("valid pdf")
}

#[test]
fn normal_appearance_follows_the_state_and_never_reads_a_dictionary_as_a_stream() {
    let pdf = annotations(
        "/Annots [4 0 R 5 0 R 6 0 R]",
        vec![
            // A check box whose states sit in an indirect dictionary (7).
            b"<< /Subtype /Widget /Rect [0 0 10 10] /AS /Yes /AP << /N 7 0 R >> >>".to_vec(),
            b"<< /Subtype /Widget /Rect [0 0 10 10] /AP << /N 7 0 R >> >>".to_vec(),
            // /N is a dictionary that is not a state dictionary; a stream follows it.
            b"<< /Subtype /Square /Rect [0 0 10 10] /AP << /N 10 0 R >> >>".to_vec(),
            b"<< /Yes 8 0 R /Off 9 0 R >>".to_vec(),
            common::stream("/BBox [0 0 10 10]", b"0 0 10 10 re f"),
            common::stream("/BBox [0 0 10 10]", b"0 0 m 10 10 l S"),
            b"<< /Unrelated true >>".to_vec(),
            common::stream("/BBox [0 0 10 10]", b"(not an appearance)"),
        ],
    );
    let annots: Vec<Dict<'_>> = pdf.pages()[0]
        .raw()
        .get::<Array<'_>>(b"Annots")
        .unwrap()
        .iter::<Dict<'_>>()
        .collect();
    let data = |i: usize| normal_appearance(&annots[i]).map(|s| s.decoded().unwrap().into_owned());
    assert_eq!(data(0).as_deref(), Some(&b"0 0 10 10 re f"[..]));
    assert_eq!(
        data(1).as_deref(),
        Some(&b"0 0 m 10 10 l S"[..]),
        "/Off without /AS"
    );
    assert_eq!(data(2), None);
}

#[test]
fn generate_is_what_the_renderer_draws_without_ap() {
    let pdf = annotations(
        "/Annots [4 0 R 5 0 R]",
        vec![
            b"<< /Subtype /Highlight /CA 0.5 /Rect [10 10 90 40] /QuadPoints [10 40 90 40 10 30 90 30] >>".to_vec(),
            b"<< /Subtype /Link /Rect [10 10 90 40] >>".to_vec(),
        ],
    );
    let annots: Vec<Dict<'_>> = pdf.pages()[0]
        .raw()
        .get::<Array<'_>>(b"Annots")
        .unwrap()
        .iter::<Dict<'_>>()
        .collect();
    let visible = kurbo::Rect::new(0.0, 0.0, 200.0, 200.0);
    let highlight = generate(&annots[0], visible).expect("a highlight is generated");
    assert_eq!(
        (highlight.opacity, highlight.blend),
        (0.5, BlendMode::Multiply)
    );
    assert_eq!(highlight.content, "1 1 0 rg\n10 30 80 10 re\nf\n");
    assert!(generate(&annots[1], visible).is_none());
}
