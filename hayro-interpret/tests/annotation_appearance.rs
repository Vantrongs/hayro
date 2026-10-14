//! Annotations without `/AP` get the appearance `PDFium` generates for their subtype.

use hayro_interpret::annotation::{Look, Unsupported, look, rect_transform};
use hayro_interpret::font::GlyphRun;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::hayro_syntax::object::{Array, Dict};
use hayro_interpret::{
    AnnotationSelection, BlendMode, ClipPath, Device, DrawMode, DrawProps, Image, ImageDrawProps,
    InterpreterSettings, Paint, SoftMask,
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
fn draw(annots: &[&str], selection: AnnotationSelection) -> Vec<Ev> {
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
        annotations: selection,
        ..Default::default()
    };
    let mut rec = Rec::default();
    common::interpret_first_page(&pdf, settings, &mut rec);
    rec.0
}

const VIEW: AnnotationSelection = AnnotationSelection::View;
const YELLOW: [u8; 4] = [255, 255, 0, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];
const RED: [u8; 4] = [255, 0, 0, 255];

#[test]
fn highlight_fills_quads_in_one_multiply_group_at_ca() {
    let ev = draw(
        &[
            "/Subtype /Highlight /F 4 /CA 0.5 /Rect [10 10 90 40] /QuadPoints [10 40 90 40 10 30 90 30 10 22 60 22 10 12 60 12]",
        ],
        VIEW,
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
    let under = draw(&[&format!("/Subtype /Underline {quad}")], VIEW);
    assert_eq!(
        under,
        vec![Ev::Stroke {
            bbox: [10, 11, 50, 11],
            rgba: BLACK,
            width: 1.0,
            dashed: false
        }]
    );
    let strike = draw(&[&format!("/Subtype /StrikeOut /C [1 0 0] {quad}")], VIEW);
    assert_eq!(
        strike,
        vec![Ev::Stroke {
            bbox: [10, 15, 50, 15],
            rgba: RED,
            width: 1.0,
            dashed: false
        }]
    );
    let squiggle = draw(&[&format!("/Subtype /Squiggly {quad}")], VIEW);
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
        VIEW,
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
        VIEW,
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
        VIEW,
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
        VIEW,
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
        draw(&["/Subtype /Square /Rect [10 10 50 30]"], VIEW),
        vec![]
    );
}

#[test]
fn note_is_a_20pt_icon_at_the_bottom_left() {
    let ev = draw(&["/Subtype /Text /Rect [100 100 140 160]"], VIEW);
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
    use AnnotationSelection::{Flatten, Print, View};
    let ink = "/Subtype /Ink /C [0] /Rect [0 0 100 100] /InkList [[10 10 20 20]]";
    let drawn = |entries: &str, selection| draw(&[&format!("{ink} {entries}")], selection).len();
    // Hidden; NoView on screen; without Print when printing.
    assert_eq!(drawn("/F 2", View), 0);
    assert_eq!(drawn("/F 32", View), 0);
    assert_eq!(drawn("/F 0", Print), 0);
    assert_eq!(drawn("/F 36", Print), 1);
    // Flattened: all but Hidden and Invisible, whatever Print and NoView say.
    for (flags, n) in [
        ("", 1),
        ("/F 32", 1),
        ("/F 4", 1),
        ("/F 1", 0),
        ("/F 2", 0),
        ("/F 5", 0),
    ] {
        assert_eq!(drawn(flags, Flatten), n, "{flags}");
    }
    // A normal appearance that names no object is generated anew.
    assert_eq!(drawn("/AP << /N 99 0 R >>", View), 1);
    // A link is not flattened, even with an appearance.
    let link = "/Subtype /Link /Rect [0 0 100 100] /AP << /N 5 0 R >>";
    assert_eq!(draw(&[link], Flatten), vec![]);
}

#[test]
fn an_annotation_without_a_generator_warns_and_draws_nothing() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let file = common::one_page(
        200,
        "/Resources << >> /Annots [4 0 R]",
        vec![b"<< /Subtype /Widget /FT /Tx /V (x) /Rect [0 0 100 100] >>".to_vec()],
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    let warned = Arc::new(AtomicUsize::new(0));
    let count = warned.clone();
    let settings = InterpreterSettings {
        warning_sink: Arc::new(move |w| {
            if matches!(
                w,
                hayro_interpret::InterpreterWarning::UnsupportedAnnotation
            ) {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }),
        ..Default::default()
    };
    let mut rec = Rec::default();
    common::interpret_first_page(&pdf, settings, &mut rec);
    assert_eq!((rec.0, warned.load(Ordering::Relaxed)), (vec![], 1));
}

/// The first page's annotations, in a file with `objects` numbered from 4.
fn annotations(page_entries: &str, objects: Vec<Vec<u8>>) -> Pdf {
    Pdf::new(common::one_page(200, page_entries, objects)).expect("valid pdf")
}

/// The annotations of the first page.
fn annots<'a>(pdf: &'a Pdf) -> Vec<Dict<'a>> {
    pdf.pages()[0]
        .raw()
        .get::<Array<'_>>(b"Annots")
        .unwrap()
        .iter::<Dict<'_>>()
        .collect()
}

const PAGE: kurbo::Rect = kurbo::Rect::new(0.0, 0.0, 200.0, 200.0);

/// What `look` says, in short: the stream's data, `generated`, `nothing`, or
/// `unsupported`.
fn looks(annots: &[Dict<'_>]) -> Vec<String> {
    annots
        .iter()
        .map(|a| match look(a, PAGE) {
            Look::Stream(s) => String::from_utf8(s.decoded().unwrap().into_owned()).unwrap(),
            Look::Generated(_) => "generated".into(),
            Look::Nothing => "nothing".into(),
            Look::Unsupported(Unsupported::Kind) => "unsupported".into(),
            Look::Unsupported(why) => format!("{why:?}"),
        })
        .collect()
}

#[test]
fn the_normal_appearance_follows_the_state_and_is_generated_without_one() {
    let square = "/Subtype /Square /C [1 0 0] /Rect [0 0 10 10]";
    let entries = [
        // A state dictionary (indirect, 14): the state /AS names; none without /AS,
        // and none for a state it lacks.
        format!("{square} /AS /Yes /AP << /N 14 0 R >>"),
        format!("{square} /AP << /N 14 0 R >>"),
        format!("{square} /AS /No /AP << /N 14 0 R >>"),
        // /N a dictionary that is not a state dictionary, with a stream after it.
        format!("{square} /AP << /N 17 0 R >>"),
        // /N naming no object, null, a number; /AP without /N, empty.
        format!("{square} /AP << /N 99 0 R >>"),
        format!("{square} /AP << /N null /D 15 0 R >>"),
        format!("{square} /AP << /D 15 0 R >>"),
        format!("{square} /AP << >>"),
        // /AP naming no object, or not a dictionary: nothing.
        format!("{square} /AP 99 0 R"),
        // A stream without /BBox or with one of no area: nothing.
        format!("{square} /AP << /N 19 0 R >>"),
    ];
    let mut objects: Vec<Vec<u8>> = entries
        .iter()
        .map(|e| format!("<< /Type /Annot {e} >>").into_bytes())
        .collect();
    objects.extend([
        b"<< /Yes 15 0 R /Off 16 0 R >>".to_vec(),
        common::stream("/BBox [0 0 10 10]", b"yes"),
        common::stream("/BBox [0 0 10 10]", b"off"),
        b"<< /Unrelated true >>".to_vec(),
        common::stream("/BBox [0 0 10 10]", b"(not an appearance)"),
        common::stream("", b"no box"),
    ]);
    let refs: Vec<String> = (4..4 + entries.len()).map(|n| format!("{n} 0 R")).collect();
    let pdf = annotations(&format!("/Annots [{}]", refs.join(" ")), objects);
    assert_eq!(
        looks(&annots(&pdf)),
        [
            "yes",
            "generated",
            "generated",
            "generated",
            "generated",
            "generated",
            "generated",
            "generated",
            "nothing",
            "nothing"
        ]
    );
    let zero = common::stream("/BBox [5 5 5 20]", b"zero");
    let pdf = annotations(
        "/Annots [4 0 R]",
        vec![
            format!("<< {square} /AP << /N 5 0 R >> >>").into_bytes(),
            zero,
        ],
    );
    assert_eq!(looks(&annots(&pdf)), ["nothing"]);
}

#[test]
fn subtypes_that_draw_without_an_appearance() {
    let r = "/Rect [0 0 10 10]";
    let entries = [
        format!("/Subtype /Stamp /Name /Approved {r}"),
        format!("/Subtype /Stamp /Name /MyStamp {r}"),
        format!("/Subtype /Stamp {r}"),
        format!("/Subtype /Popup {r}"),
        format!("/Subtype /Movie {r}"),
        format!("/Subtype /Foo {r}"),
        format!("{r}"),
        format!("/Subtype /Widget /FT /Sig {r}"),
        format!("/Subtype /Widget /FT /Sig /V << >> {r}"),
        format!("/Subtype /Widget {r}"),
        // The field type is inherited from the parent field (22).
        format!("/Subtype /Widget /Parent 22 0 R /AS /Yes {r}"),
        format!("/Subtype /Square /C [0] /BS << /W 0 >> {r}"),
        format!("/Subtype /Ink /InkList [[1 1 5 5]] {r}"),
        // Widgets with nothing to show: no value, no border, no background, an off
        // state, a border of width 0.
        format!("/Subtype /Widget /FT /Ch /Ff 131072 /Opt [(a)] {r}"),
        format!("/Subtype /Widget /FT /Btn /AS /Off /V /Yes {r}"),
        format!("/Subtype /Widget /FT /Tx /V () /MK << /BC [0] >> /BS << /W 0 >> {r}"),
        format!("/Subtype /Widget /FT /Btn /Ff 65536 {r}"),
        // And with something.
        format!("/Subtype /Widget /FT /Tx /MK << /BG [1] >> {r}"),
    ];
    let mut objects: Vec<Vec<u8>> = entries
        .iter()
        .map(|e| format!("<< /Type /Annot {e} >>").into_bytes())
        .collect();
    objects.push(b"<< /FT /Btn /T (a) >>".to_vec());
    let refs: Vec<String> = (4..4 + entries.len()).map(|n| format!("{n} 0 R")).collect();
    let pdf = annotations(&format!("/Annots [{}]", refs.join(" ")), objects);
    assert_eq!(
        looks(&annots(&pdf)),
        [
            "unsupported",
            "nothing",
            "nothing",
            "nothing",
            "nothing",
            "nothing",
            "nothing",
            "unsupported",
            "nothing",
            "nothing",
            "unsupported",
            // Width 0 is the thinnest line; ink without /C has no colour.
            "generated",
            "nothing",
            "nothing",
            "nothing",
            "nothing",
            "nothing",
            "unsupported",
        ]
    );
}

#[test]
fn rect_transform_maps_the_transformed_box_onto_the_rect() {
    // A 20 × 10 box turned a quarter by its matrix spans x -10 to 0 and y 0 to 20;
    // the rect is 30 × 40, so the box is scaled as well as moved.
    let rect = kurbo::Rect::new(100.0, 200.0, 130.0, 240.0);
    let turned = Affine::new([0.0, 1.0, -1.0, 0.0, 0.0, 0.0]);
    let a = rect_transform(rect, [0.0, 0.0, 20.0, 10.0], turned);
    let corners = (a * turned * kurbo::Rect::new(0.0, 0.0, 20.0, 10.0).to_path(0.1)).bounding_box();
    assert_eq!(corners, rect);
}

#[test]
fn a_generated_appearance_has_its_opacity_and_blend() {
    let pdf = annotations(
        "/Annots [4 0 R]",
        vec![b"<< /Subtype /Highlight /CA 0.5 /Rect [10 10 90 40] /QuadPoints [10 40 90 40 10 30 90 30] >>".to_vec()],
    );
    let Look::Generated(highlight) = look(&annots(&pdf)[0], PAGE) else {
        panic!("a highlight is generated");
    };
    assert_eq!(
        (highlight.opacity, highlight.blend),
        (0.5, BlendMode::Multiply)
    );
    assert_eq!(highlight.content, "1 1 0 rg\n10 30 80 10 re\nf\n");
}
