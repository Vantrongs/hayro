//! Annotations without `/AP` get the appearance `PDFium` generates for their subtype.

use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps,
    InterpreterCache, InterpreterSettings, Paint, SoftMask, interpret_page,
};
use kurbo::{Affine, BezPath, Rect, Shape};

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

/// A one-page PDF (200 × 200 pt, no content) with these annotation dictionaries.
fn pdf(annots: &[&str]) -> Vec<u8> {
    let mut objs = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
    ];
    let refs: Vec<String> = (0..annots.len())
        .map(|i| format!("{} 0 R", i + 4))
        .collect();
    objs.push(format!(
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << >> /Annots [{}] >>",
        refs.join(" ")
    ));
    objs.extend(annots.iter().map(|a| format!("<< /Type /Annot {a} >>")));
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
    for o in offsets {
        out.extend(format!("{o:010} 00000 n \n").bytes());
    }
    out.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .bytes(),
    );
    out
}

/// Draws the page in PDF user space (y up), so bounds read like the PDF numbers.
fn draw(annots: &[&str], print: bool) -> Vec<Ev> {
    let pdf = Pdf::new(pdf(annots)).expect("valid pdf");
    let page = &pdf.pages()[0];
    let cache = InterpreterCache::new();
    let settings = InterpreterSettings {
        annotation_print: print,
        ..Default::default()
    };
    let mut ctx = Context::new(
        Affine::IDENTITY,
        Rect::new(0.0, 0.0, 200.0, 200.0),
        &cache,
        page.xref(),
        settings,
    );
    let mut rec = Rec::default();
    interpret_page(page, &mut ctx, &mut rec);
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
