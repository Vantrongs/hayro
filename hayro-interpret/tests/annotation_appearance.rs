//! Annotations without a usable `/AP` get an appearance generated from their entries.

use hayro_interpret::annotation::{Appearances, Look, Unsupported, rect_transform};
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
    /// The next fill or stroke blends in this mode rather than Normal.
    Blended(BlendMode),
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
        if props.blend_mode != BlendMode::Normal {
            self.0.push(Ev::Blended(props.blend_mode));
        }
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

/// Each quad is filled on its own in Multiply at /CA, as viewers fill them: where
/// quads overlap, the colour multiplies twice, rather than once for a group of
/// them.
#[test]
fn highlight_fills_each_quad_in_multiply_at_ca() {
    let ev = draw(
        &[
            "/Subtype /Highlight /F 4 /CA 0.5 /Rect [10 10 90 40] /QuadPoints [10 40 90 40 10 30 90 30 10 22 60 22 10 12 60 12]",
        ],
        VIEW,
    );
    let half_yellow = [255, 255, 0, 128];
    assert_eq!(
        ev,
        vec![
            Ev::Blended(BlendMode::Multiply),
            Ev::Fill {
                bbox: [8, 30, 92, 40],
                rgba: half_yellow
            },
            Ev::Blended(BlendMode::Multiply),
            Ev::Fill {
                bbox: [8, 12, 62, 22],
                rgba: half_yellow
            },
        ]
    );
}

/// A translucent fill and border of one operator are drawn as viewers draw them:
/// the border covers the fill under it rather than blending over it, as if the
/// pair were drawn opaque and then at /CA as one.
#[test]
fn a_translucent_square_border_covers_its_fill() {
    let ev = draw(
        &["/Subtype /Square /CA 0.5 /C [0 0 1] /IC [1 0 0] /BS << /W 10 >> /Rect [20 20 180 180]"],
        VIEW,
    );
    assert_eq!(
        ev,
        vec![
            Ev::Group {
                opacity: 0.5,
                blend: BlendMode::Normal
            },
            Ev::Fill {
                bbox: [25, 25, 175, 175],
                rgba: RED
            },
            Ev::Stroke {
                bbox: [25, 25, 175, 175],
                rgba: [0, 0, 255, 255],
                width: 10.0,
                dashed: false
            },
            Ev::EndGroup,
        ]
    );
}

/// Lines a sixteenth of the quad's height wide: an underline a seventh of it above
/// the bottom, a strikeout three sevenths, a zigzag a seventh high.
#[test]
fn text_lines_scale_with_the_quad() {
    let quad = "/Rect [10 10 50 24] /QuadPoints [10 24 50 24 10 10 50 10]";
    let line = |bbox, rgba| {
        vec![Ev::Stroke {
            bbox,
            rgba,
            width: 0.875,
            dashed: false,
        }]
    };
    assert_eq!(
        draw(&[&format!("/Subtype /Underline {quad}")], VIEW),
        line([10, 12, 50, 12], BLACK)
    );
    assert_eq!(
        draw(&[&format!("/Subtype /StrikeOut /C [1 0 0] {quad}")], VIEW),
        line([10, 16, 50, 16], RED)
    );
    assert_eq!(
        draw(&[&format!("/Subtype /Squiggly {quad}")], VIEW),
        line([10, 10, 50, 12], BLACK)
    );
}

/// A Squiggly quad 4·10^6 pt wide is drawn across the page and a few steps beyond,
/// not along its whole width.
#[test]
fn a_huge_squiggly_is_drawn_across_the_page() {
    let ev = draw(
        &["/Subtype /Squiggly /Rect [10 10 50 20] \
           /QuadPoints [10 20 4000000 20 10 10 4000000 10]"],
        VIEW,
    );
    let [Ev::Stroke { bbox, width, .. }] = ev.as_slice() else {
        panic!("{ev:?}");
    };
    assert_eq!(([bbox[0], bbox[1], bbox[3]], *width), ([10, 10, 11], 0.625));
    assert!((200..=210).contains(&bbox[2]), "{bbox:?}");
}

/// The paths of an Ink are one path stroked once, as viewers stroke it: where they
/// cross, a translucent ink is laid once.
#[test]
fn ink_strokes_its_paths_as_one_at_border_width_with_dash() {
    let ev = draw(
        &[
            "/Subtype /Ink /C [1 0 0] /BS << /W 3 /S /D /D [2 1] >> /Rect [0 0 100 100] /InkList [[10 10 20 30 40 20] [50 50 60 60]]",
        ],
        VIEW,
    );
    assert_eq!(
        ev,
        vec![Ev::Stroke {
            bbox: [10, 10, 60, 60],
            rgba: RED,
            width: 3.0,
            dashed: true
        }]
    );
}

/// An ink path of one point is a dot: a stroke of no length with round ends.
#[test]
fn an_ink_point_is_a_dot() {
    assert_eq!(
        draw(
            &["/Subtype /Ink /C [0] /BS << /W 4 >> /Rect [0 0 200 200] /InkList [[100 100]]"],
            VIEW
        ),
        vec![Ev::Stroke {
            bbox: [100, 100, 100, 100],
            rgba: BLACK,
            width: 4.0,
            dashed: false
        }]
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

/// The fills drawn, as bounding box and colour, skipping strokes and groups.
fn fills(ev: &[Ev]) -> Vec<([i32; 4], [u8; 4])> {
    ev.iter()
        .filter_map(|e| match e {
            Ev::Fill { bbox, rgba } => Some((*bbox, *rgba)),
            _ => None,
        })
        .collect()
}

/// Whether box `b` lies inside `outer`.
fn inside(b: [i32; 4], outer: [i32; 4]) -> bool {
    b[0] >= outer[0] && b[1] >= outer[1] && b[2] <= outer[2] && b[3] <= outer[3]
}

const WHITE: [u8; 4] = [255, 255, 255, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

#[test]
fn note_is_a_20pt_icon_at_the_top_left() {
    let ev = draw(&["/Subtype /Text /Rect [100 100 140 160]"], VIEW);
    let frame = [101, 141, 119, 159];
    assert_eq!(
        ev,
        vec![
            Ev::Fill {
                bbox: frame,
                rgba: WHITE
            },
            Ev::Stroke {
                bbox: frame,
                rgba: BLACK,
                width: 1.25,
                dashed: false
            },
            // Four lines of text in the symbol's 10 pt square.
            Ev::Fill {
                bbox: [105, 146, 115, 155],
                rgba: BLACK
            },
        ]
    );
    // /C fills the box; other icons draw a symbol in the same square.
    for icon in [
        "/Subtype /Text /Name /Help /C [1 1 0]",
        "/Subtype /FileAttachment",
        "/Subtype /FileAttachment /Name /Paperclip",
        "/Subtype /Sound /Name /Mic",
        "/Subtype /Sound /Name /Unknown",
    ] {
        let ev = fills(&draw(&[&format!("{icon} /Rect [100 100 140 160]")], VIEW));
        assert_eq!(ev.len(), 2, "{icon}: {ev:?}");
        let fill = if icon.contains("/C") { YELLOW } else { WHITE };
        assert_eq!(ev[0], (frame, fill), "{icon}");
        assert!(
            inside(ev[1].0, [104, 144, 116, 156]) && ev[1].1 == BLACK,
            "{icon}: {ev:?}"
        );
    }
}

#[test]
fn caret_is_centred_and_redact_outlines_its_area() {
    assert_eq!(
        draw(&["/Subtype /Caret /Rect [40 40 60 60] /RD [5 5 5 5]"], VIEW),
        vec![Ev::Fill {
            bbox: [40, 43, 60, 57],
            rgba: BLACK
        }]
    );
    assert_eq!(
        draw(&["/Subtype /Caret /Rect [0 0 100 100] /C [1 0 0]"], VIEW),
        vec![Ev::Fill {
            bbox: [40, 43, 60, 57],
            rgba: RED
        }]
    );
    let outline = |bbox| Ev::Stroke {
        bbox,
        rgba: RED,
        width: 1.0,
        dashed: false,
    };
    // Each quadrilateral, whatever the colours; without /QuadPoints, /Rect less 1 pt.
    assert_eq!(
        draw(
            &["/Subtype /Redact /C [0 0 1] /IC [0] /Rect [40 40 160 80] \
               /QuadPoints [40 80 160 80 40 60 160 60 40 60 100 60 40 40 100 40]"],
            VIEW
        ),
        vec![outline([40, 60, 160, 80]), outline([40, 40, 100, 60])]
    );
    assert_eq!(
        draw(&["/Subtype /Redact /Rect [40 40 160 80]"], VIEW),
        vec![outline([41, 41, 159, 79])]
    );
}

#[test]
fn a_standard_stamp_is_its_name_in_a_tilted_box() {
    let ev = draw(
        &["/Subtype /Stamp /Name /NotForPublicRelease /C [1 0 0] /Rect [5 75 195 125]"],
        VIEW,
    );
    let Ev::Stroke {
        bbox, rgba, width, ..
    } = ev[0]
    else {
        panic!("{ev:?}")
    };
    // The 186 × 44 box turned 0.6°: 2 pt taller than it is, inside /Rect.
    assert_eq!((rgba, width), (RED, 2.0));
    assert!(
        inside(bbox, [5, 75, 195, 125]) && bbox[3] - bbox[1] == 46,
        "{bbox:?}"
    );
    // Two lines of text, in /C.
    let text = fills(&ev);
    assert_eq!(text.len(), 2, "{ev:?}");
    assert!(
        text.iter().all(|&(b, c)| c == RED && inside(b, bbox)),
        "{ev:?}"
    );
    // Scaled to a smaller /Rect, centred.
    let small = draw(&["/Subtype /Stamp /Name /Draft /Rect [10 60 70 80]"], VIEW);
    let Ev::Stroke { bbox, width, .. } = small[0] else {
        panic!("{small:?}")
    };
    assert!(
        inside(bbox, [10, 60, 70, 80]) && (width - 2.0 * 60.0 / 190.0).abs() < 1e-3,
        "{small:?}"
    );
}

#[test]
fn free_text_fills_its_box_and_writes_in_the_default_appearance() {
    let ev = draw(
        &[
            "/Subtype /FreeText /Rect [10 10 110 60] /C [1 1 0] /DA (/Helv 12 Tf 1 0 0 rg) /Contents (Hi)",
        ],
        VIEW,
    );
    assert_eq!(
        ev[..2],
        [
            Ev::Fill {
                bbox: [10, 10, 110, 60],
                rgba: YELLOW
            },
            Ev::Stroke {
                bbox: [11, 11, 110, 60],
                rgba: RED,
                width: 1.0,
                dashed: false
            },
        ]
    );
    // "Hi" from 2 pt inside the border, its baseline 0.8 em below that.
    let text = fills(&ev[2..]);
    assert_eq!(text.len(), 1, "{ev:?}");
    assert!(
        inside(text[0].0, [12, 48, 30, 58]) && text[0].0[1] == 48,
        "{text:?}"
    );
    assert_eq!(text[0].1, RED);
}

#[test]
fn lines_and_polygons_draw_their_points_and_endings() {
    let ev = draw(
        &[
            "/Subtype /Line /L [20 20 120 20] /C [0 0 1] /IC [1 0 0] /LE [/None /ClosedArrow] /Rect [0 0 200 200]",
        ],
        VIEW,
    );
    assert_eq!(
        ev[0],
        Ev::Stroke {
            bbox: [20, 20, 120, 20],
            rgba: BLUE,
            width: 1.0,
            dashed: false
        }
    );
    // Coordinates a damaged /L lacks, or has as something else, are 0.
    for l in ["[20 150 180]", "[20 150 180 (x)]"] {
        let ev = draw(
            &[&format!("/Subtype /Line /L {l} /C [0] /Rect [0 0 200 200]")],
            VIEW,
        );
        assert_eq!(
            ev,
            vec![Ev::Stroke {
                bbox: [20, 0, 180, 150],
                rgba: BLACK,
                width: 1.0,
                dashed: false
            }],
            "{l}"
        );
    }
    // The arrow, filled with /IC.
    let arrow = fills(&ev);
    assert_eq!(arrow.len(), 1, "{ev:?}");
    assert!(
        inside(arrow[0].0, [110, 15, 120, 25]) && arrow[0].1 == RED,
        "{ev:?}"
    );
    assert_eq!(
        draw(
            &[
                "/Subtype /Polygon /Vertices [10 10 50 10 30 40] /C [1 0 0] /IC [1 1 0] /Rect [0 0 100 100]"
            ],
            VIEW
        ),
        vec![
            Ev::Fill {
                bbox: [10, 10, 50, 40],
                rgba: YELLOW
            },
            Ev::Stroke {
                bbox: [10, 10, 50, 40],
                rgba: RED,
                width: 1.0,
                dashed: false
            },
        ]
    );
}

#[test]
fn widgets_show_their_value_or_state() {
    let field = draw(
        &[
            "/Subtype /Widget /FT /Tx /V (Hello) /DA (/Helv 10 Tf 0 0 1 rg) /MK << /BG [1 1 0] /BC [1 0 0] >> /Rect [10 10 110 30]",
        ],
        VIEW,
    );
    assert_eq!(
        field[..2],
        [
            Ev::Fill {
                bbox: [10, 10, 110, 30],
                rgba: YELLOW
            },
            Ev::Stroke {
                bbox: [11, 11, 110, 30],
                rgba: RED,
                width: 1.0,
                dashed: false
            },
        ]
    );
    let text = fills(&field[2..]);
    assert!(
        text.len() == 1 && text[0].1 == BLUE && inside(text[0].0, [12, 12, 60, 28]),
        "{field:?}"
    );
    // A check box in its on state shows a check mark; off, nothing.
    let check = fills(&draw(
        &["/Subtype /Widget /FT /Btn /AS /Yes /Rect [10 10 30 30]"],
        VIEW,
    ));
    assert!(
        check.len() == 1 && check[0].1 == BLACK && inside(check[0].0, [10, 10, 30, 30]),
        "{check:?}"
    );
    // A radio button: its background as a disc and a dot.
    let radio = fills(&draw(
        &["/Subtype /Widget /FT /Btn /Ff 32768 /AS /a /MK << /BG [1 1 0] >> /Rect [10 10 30 30]"],
        VIEW,
    ));
    assert_eq!(radio[0], ([10, 10, 30, 30], YELLOW), "{radio:?}");
    assert!(
        radio[1].1 == BLACK && inside(radio[1].0, [15, 15, 25, 25]),
        "{radio:?}"
    );
    // An unsigned signature field: an orange tag at the top left with white text.
    let sig = fills(&draw(
        &["/Subtype /Widget /FT /Sig /Rect [10 10 110 70]"],
        VIEW,
    ));
    assert_eq!(sig.len(), 2, "{sig:?}");
    assert!(
        sig[0].0 == [10, 60, 55, 70] && sig[1].1 == WHITE && inside(sig[1].0, sig[0].0),
        "{sig:?}"
    );
}

/// A default appearance without `Tf` sets text in 12 pt whatever the field's size,
/// as viewers set it; a size of 0 fits the text to the field.
#[test]
fn text_without_a_font_size_is_12_pt() {
    let tops = |da: &str| {
        [
            "/FT /Tx /V (HH)",
            "/FT /Ch /Ff 131072 /V (HH)",
            "/FT /Btn /Ff 65536 /MK << /CA (HH) >>",
        ]
        .map(|field| {
            let text = fills(&draw(
                &[&format!(
                    "/Subtype /Widget {field} {da} /Rect [20 20 180 80]"
                )],
                VIEW,
            ));
            // The cap height of H, 0.718 em.
            text[0].0[3] - text[0].0[1]
        })
    };
    assert_eq!(tops(""), [9; 3]);
    assert_eq!(tops("/DA (0 g)"), [9; 3]);
    assert!(tops("/DA (/Helv 0 Tf 0 g)").iter().all(|&h| h > 20));
}

/// A push button's caption at an automatic size keeps four points from its bevel on
/// each side, across as well as up (observed: `UTF16` in a box 40 pt square is set at
/// 8.508 pt, from 7 to 33 pt across).
#[test]
fn a_push_buttons_automatic_caption_keeps_four_points_from_each_side() {
    let c = generated(
        "/Subtype /Widget /FT /Btn /Ff 65536 /Rect [20 80 60 120] /DA (/Helv 0 Tf 0 g) /MK << /BG [0.8] /CA (UTF16) >>",
    );
    let caption = c.rsplit("\n0 g\n").next().unwrap();
    let xs: Vec<f64> = caption
        .lines()
        .filter(|l| [" m", " l", " c"].iter().any(|op| l.ends_with(op)))
        .flat_map(|l| {
            let n: Vec<f64> = l.split(' ').filter_map(|n| n.parse().ok()).collect();
            n.as_chunks::<2>()
                .0
                .iter()
                .map(|[x, _]| *x)
                .collect::<Vec<_>>()
        })
        .collect();
    let (lo, hi) = xs
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &x| {
            (a.min(x), b.max(x))
        });
    assert!(lo >= 7.0 && hi <= 33.0 && hi - lo > 24.0, "{lo} {hi}");
}

/// A choice field with rich text (`/RV` or `/DS`) draws it as a text field does: a
/// list on several lines, a combo box on one; a list without `/RV` draws its options
/// from `/TI` on, one a line, in the style of `/DS` (observed).
#[test]
fn a_choice_field_draws_rich_text_as_a_text_field_does() {
    let w = "/Subtype /Widget /Rect [20 80 180 140] /DA (/Helv 12 Tf 0 g)";
    let rv = "/RV (<p>Rich value</p>)";
    let ds = "/DS (font-size: 9pt)";
    let opt = "/Opt [[(e) (zero)] (one) (two)] /TI 1";
    for (choice, text) in [
        (
            format!("/FT /Ch {rv} {opt}"),
            format!("/FT /Tx /Ff 4096 {rv}"),
        ),
        (
            format!("/FT /Ch /Ff 131072 /V (v) {rv}"),
            format!("/FT /Tx {rv}"),
        ),
        (
            format!("/FT /Ch {ds} {opt}"),
            format!("/FT /Tx /Ff 4096 {ds} /V (one\\ntwo)"),
        ),
    ] {
        assert_eq!(
            content(&format!("{w} {choice}"), vec![]),
            content(&format!("{w} {text}"), vec![]),
            "{choice}"
        );
    }
}

/// An appearance state `/AS` names that `/AP /N` has but that is no stream, a
/// reference to no object among them, draws nothing, a button's too; a state it
/// lacks, or that is null, is generated anew (observed).
#[test]
fn a_state_that_is_no_stream_draws_nothing() {
    let square = "/Subtype /Square /Rect [20 20 180 180] /IC [1 0 0] /AS /On";
    let check = "/Subtype /Widget /FT /Btn /Rect [20 20 60 60] /V /On /AS /On";
    for annot in [square, check] {
        for (states, drawn) in [
            ("<< /On 99 0 R >>", false),
            ("<< /On 5 >>", false),
            ("<< /On << >> >>", false),
            ("<< /On null >>", true),
            ("<< /Off 99 0 R >>", true),
        ] {
            let c = content(&format!("{annot} /AP << /N {states} >>"), vec![]);
            assert_eq!(c != "nothing", drawn, "{annot} {states}: {c}");
        }
    }
}

/// A `Redact` without quadrilaterals outlines its box a point inside, clipped to the
/// box: one under two points wide, or of no area, shows only what lies within it
/// (observed: none of the outline of a box of no width).
#[test]
fn a_redact_box_is_clipped_to_itself() {
    let c = content(
        "/Subtype /Redact /Rect [20 20 21 180] /QuadPoints << >>",
        vec![],
    );
    assert!(c.starts_with("q\n20 20 1 160 re\nW n\n"), "{c}");
}

/// A push button whose border is 2^23 or more wide is unsupported, as other fields
/// are, though it has no border colour: viewers lay its bevel and caption out from
/// the box inside the border.
#[test]
fn a_push_button_inside_a_border_too_wide_to_write_is_unsupported() {
    let button = "/Subtype /Widget /FT /Btn /Ff 65536 /Rect [20 80 180 120] /MK << /CA (A) >>";
    let drawn = content(&format!("{button} /BS << /W 2 >>"), vec![]);
    assert!(drawn.contains("\nf\n"), "{drawn}");
    assert_eq!(
        content(&format!("{button} /BS << /W 4294967298 >>"), vec![]),
        "Coordinates"
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
        // A damaged /F as viewers read it (see `a_damaged_field_flags_entry_reads_as_viewers_read_it`).
        ("/F -1", 0),
        ("/F 2.9", 0),
        ("/F 0.5", 0),
        ("/F 0.4", 1),
        ("/F 3.5", 1),
        ("/F -0.6", 0),
        ("/F 4294967298", 0),
        ("/F 4294967296", 1),
        ("/F 4294967298.0", 0),
        ("/F 2147483650.0", 1),
        ("/F -3000000000.0", 1),
        ("/F (x)", 1),
    ] {
        assert_eq!(drawn(flags, Flatten), n, "{flags}");
    }
    assert_eq!(drawn("/F 2.5", View), 0);
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
        // A character none of the fonts has.
        vec![b"<< /Subtype /FreeText /Contents <FEFF6F22> /Rect [0 0 100 100] >>".to_vec()],
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

/// What [`Appearances::look`] says for each annotation of the first page, in short:
/// the stream's data, `generated`, `nothing`, or `unsupported`.
fn looks(pdf: &Pdf) -> Vec<String> {
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    annots(pdf)
        .iter()
        .map(|a| match appearances.look(a, PAGE) {
            Look::Stream { stream, .. } => {
                String::from_utf8(stream.decoded().unwrap().into_owned()).unwrap()
            }
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
        looks(&pdf),
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
    assert_eq!(looks(&pdf), ["nothing"]);
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
        r.to_string(),
        format!("/Subtype /Widget /FT /Sig {r}"),
        format!("/Subtype /Widget /FT /Sig /V << >> {r}"),
        format!("/Subtype /Widget {r}"),
        // The field type is inherited from the parent field (23).
        format!("/Subtype /Widget /Parent 23 0 R /AS /Yes {r}"),
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
        // Text in a character no font has.
        format!("/Subtype /FreeText /Contents <FEFF6F22> {r}"),
    ];
    let mut objects: Vec<Vec<u8>> = entries
        .iter()
        .map(|e| format!("<< /Type /Annot {e} >>").into_bytes())
        .collect();
    objects.push(b"<< /FT /Btn /T (a) >>".to_vec());
    let refs: Vec<String> = (4..4 + entries.len()).map(|n| format!("{n} 0 R")).collect();
    let pdf = annotations(&format!("/Annots [{}]", refs.join(" ")), objects);
    assert_eq!(
        looks(&pdf),
        [
            // A stamp of another name shows it; without one, Draft.
            "generated",
            "generated",
            "generated",
            "nothing",
            "nothing",
            "nothing",
            "nothing",
            "generated",
            "nothing",
            "nothing",
            "generated",
            // Width 0 is the thinnest line; ink without /C has no colour.
            "generated",
            "nothing",
            "nothing",
            "nothing",
            "nothing",
            "nothing",
            "generated",
            "Character('漢')",
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
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    let Look::Generated(highlight) = appearances.look(&annots(&pdf)[0], PAGE) else {
        panic!("a highlight is generated");
    };
    assert_eq!(
        (highlight.opacity, highlight.blend),
        (0.5, BlendMode::Multiply)
    );
    // The sides bulge out √2/6 of the height, clipped to the quad a sixteenth of its
    // height wider and the bulges.
    assert_eq!(
        highlight.content,
        "q\n7.642977 29.375 84.71404 11.25 re\nW n\n1 1 0 rg\n10 30 m\n\
         7.642977 32.35702 7.642977 37.64298 10 40 c\n90 40 l\n\
         92.357025 37.64298 92.357025 32.35702 90 30 c\nf\nQ\n"
    );
}

/// Draws the first page of a file whose page has `/Annots [4 0 R]` and `objects`
/// numbered from 4.
fn draw_objects(objects: Vec<Vec<u8>>) -> Vec<Ev> {
    let pdf = annotations("/Resources << >> /Annots [4 0 R]", objects);
    let settings = InterpreterSettings {
        annotations: VIEW,
        ..Default::default()
    };
    let mut rec = Rec::default();
    common::interpret_first_page(&pdf, settings, &mut rec);
    rec.0
}

/// The strokes drawn, as bounding box.
fn strokes(ev: &[Ev]) -> Vec<[i32; 4]> {
    ev.iter()
        .filter_map(|e| match e {
            Ev::Stroke { bbox, .. } => Some(*bbox),
            _ => None,
        })
        .collect()
}

/// A `/Rect` that is short, or has something other than a number, reads the
/// coordinates it lacks as 0, as viewers read it: the annotation is drawn there,
/// with an appearance stream or without.
#[test]
fn a_damaged_rect_reads_what_it_lacks_as_zero() {
    // The border's path, half its width inside [40 0 160 80].
    let square = draw(&["/Subtype /Square /C [1 0 0] /Rect [40 80 160]"], VIEW);
    assert_eq!(strokes(&square), [[41, 1, 160, 80]], "{square:?}");
    // [40 80 0 120]: the icon at the top left, (0, 120).
    let note = fills(&draw(&["/Subtype /Text /Rect [40 80 (bad) 120]"], VIEW));
    assert_eq!(
        note.first().map(|f| f.0),
        Some([1, 101, 19, 119]),
        "{note:?}"
    );
    let text = fills(&draw(
        &["/Subtype /FreeText /Rect [40 80 160] /Contents (CHECK)"],
        VIEW,
    ));
    assert!(
        text.len() == 1 && inside(text[0].0, [40, 0, 160, 80]),
        "{text:?}"
    );
    let stream = draw_objects(vec![
        b"<< /Type /Annot /Subtype /Square /Rect [40 80 160] /AP << /N 5 0 R >> >>".to_vec(),
        common::stream("/BBox [0 0 10 10]", b"1 0 0 rg 0 0 10 10 re f"),
    ]);
    assert_eq!(fills(&stream), [([40, 0, 160, 80], RED)], "{stream:?}");
}

/// A line's caption keeps every line of `/Contents`, set on one line with the line
/// breaks as spaces, as viewers draw it.
#[test]
fn a_caption_keeps_all_its_lines() {
    let ev = draw(
        &["/Subtype /Line /L [20 100 280 100] /C [0] /Cap true \
           /Contents (FIRST\\nSECOND) /Rect [10 50 290 150]"],
        VIEW,
    );
    let text = fills(&ev);
    // "FIRST SECOND" in 12 pt Helvetica is 89.3 pt wide, centred on the line.
    assert_eq!(text.len(), 1, "{ev:?}");
    let [x0, _, x1, _] = text[0].0;
    assert!(x0 <= 106 && x1 >= 193, "{text:?}");
    // The line breaks around the whole caption, 3 pt from it: one path of two
    // segments.
    let content = generated(
        "/Subtype /Line /L [20 100 280 100] /C [0] /Cap true \
         /Contents (FIRST\\nSECOND) /Rect [10 50 290 150]",
    );
    let ends: Vec<i32> = content
        .lines()
        .take_while(|l| *l != "S")
        .filter(|l| l.ends_with(" m") || l.ends_with(" l"))
        .map(|l| l.split(' ').next().unwrap().parse::<f32>().unwrap().round() as i32)
        .collect();
    assert_eq!(ends, [20, 102, 198, 280], "{content}");
}

/// The generated content of the first annotation of a page with `annot`.
fn generated(annot: &str) -> String {
    let pdf = annotations(
        "/Annots [4 0 R]",
        vec![format!("<< /Type /Annot {annot} >>").into_bytes()],
    );
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    match appearances.look(&annots(&pdf)[0], PAGE) {
        Look::Generated(a) => a.content,
        _ => panic!("{annot}: not generated"),
    }
}

/// A check box or radio button turned by `/MK /R` takes the square at the bottom
/// left of its turned frame.
#[test]
fn check_boxes_and_radio_buttons_turn_with_the_field() {
    let wide = "/Rect [20 100 100 140]";
    let tall = "/Rect [20 20 60 120]";
    for (r, rect, square) in [
        (0, wide, [20, 100, 60, 140]),
        (90, wide, [60, 100, 100, 140]),
        (180, wide, [60, 100, 100, 140]),
        (270, wide, [20, 100, 60, 140]),
        (0, tall, [20, 20, 60, 60]),
        (90, tall, [20, 20, 60, 60]),
        (180, tall, [20, 80, 60, 120]),
        (270, tall, [20, 80, 60, 120]),
    ] {
        for kind in ["", "/Ff 32768"] {
            let ev = fills(&draw(
                &[&format!(
                    "/Subtype /Widget /FT /Btn {kind} /AS /Yes /MK << /R {r} /BG [1 1 0] >> {rect}"
                )],
                VIEW,
            ));
            assert_eq!(ev[0], (square, YELLOW), "{kind} /R {r} {rect}: {ev:?}");
            assert!(
                ev[1].1 == BLACK && inside(ev[1].0, square),
                "{kind} /R {r} {rect}: {ev:?}"
            );
        }
    }
}

/// Rich text keeps the size and colour of each span: a small red span after a
/// large black one fits on the line, inside the box.
#[test]
fn rich_text_keeps_the_style_of_each_span() {
    let rc = "<body><p style=\"font-family:Helvetica;font-size:12pt\"><span>A </span>\
              <span style=\"font-size:4pt;color:#ff0000\">IMPORTANT_DIMENSION_12.5_M</span></p></body>";
    let ev = draw(
        &[&format!(
            "/Subtype /FreeText /Rect [20 100 160 120] /BS << /W 0 >> /RC ({rc})"
        )],
        VIEW,
    );
    let text = fills(&ev);
    assert_eq!(text.len(), 2, "{ev:?}");
    assert!(text[0].1 == BLACK && text[1].1 == RED, "{ev:?}");
    // One line, 0.9 of the largest size below the top.
    for (bbox, _) in &text {
        assert!(inside(*bbox, [20, 106, 160, 120]), "{text:?}");
    }
}

/// A callout line whose `/CL` has something other than a number reads it as 0 and
/// keeps the other points where they are; a `/CL` of neither two nor three points
/// draws no callout, as viewers draw it.
#[test]
fn a_damaged_callout_keeps_its_points() {
    let callout = |cl: &str| {
        strokes(&draw(
            &[&format!(
                "/Subtype /FreeText /Rect [100 100 250 180] /IT /FreeTextCallout \
                 /CL [{cl}] /BS << /W 3 >> /Contents (CHECK)"
            )],
            VIEW,
        ))
    };
    let border = [102, 102, 249, 179];
    assert_eq!(
        callout("20 20 80 (bad) 150 120"),
        [[20, 0, 150, 120], border]
    );
    assert_eq!(callout("20 20 80 40"), [[20, 20, 80, 40], border]);
    for cl in ["20 20 80", "20 20 80 40 150", "20 20 80 40 150 120 5"] {
        assert_eq!(callout(cl), [border], "{cl}");
    }
}

/// Each line of rich text is 1.2 times its largest size high, its baseline 0.9 of
/// that size below its top, as viewers lay it out: an H 10, 20 and 10 pt high on
/// lines of their own, and a 10 pt H then a 20 pt one on a line, then a 10 pt one.
#[test]
fn rich_text_lines_are_as_high_as_their_largest_size() {
    let h = |size: u32, color: &str| {
        format!("<span style=\"font-size:{size}pt;color:{color}\">H</span>")
    };
    let baselines = |body: &str| {
        let rc = format!("<body style=\"font-family:Helvetica\">{body}</body>");
        fills(&draw(
            &[&format!(
                "/Subtype /FreeText /Rect [20 20 180 180] /BS << /W 0 >> /RC ({rc})"
            )],
            VIEW,
        ))
        .iter()
        .map(|(b, _)| b[1])
        .collect::<Vec<_>>()
    };
    let (r, k) = ("#ff0000", "#000000");
    assert_eq!(
        baselines(&format!(
            "<p>{}</p><p>{}</p><p>{}</p>",
            h(10, r),
            h(20, k),
            h(10, r)
        )),
        [171, 150, 135]
    );
    assert_eq!(
        baselines(&format!(
            "<p>{}{}</p><p>{}</p>",
            h(10, r),
            h(20, k),
            h(10, r)
        )),
        [162, 162, 147]
    );
}

/// A signature field is signed when its value, inherited or its own, is something:
/// a null value or one that names no object is no signature.
#[test]
fn a_signature_with_a_null_value_is_unsigned() {
    let sig = "/Subtype /Widget /FT /Sig /Rect [10 10 110 70]";
    let pdf = annotations(
        "/Annots [4 0 R 5 0 R 6 0 R 7 0 R]",
        vec![
            format!("<< {sig} /V null >>").into_bytes(),
            format!("<< {sig} /V 99 0 R >>").into_bytes(),
            format!("<< {sig} /V 8 0 R >>").into_bytes(),
            format!("<< {sig} /V null /Parent 9 0 R >>").into_bytes(),
            b"<< /Type /Sig >>".to_vec(),
            b"<< /V 8 0 R >>".to_vec(),
        ],
    );
    assert_eq!(
        looks(&pdf),
        ["generated", "generated", "nothing", "nothing"]
    );
    // Its tag is not turned with the field (observed).
    let tag = |mk: &str| content(&format!("{sig} {mk}"), vec![]);
    assert_eq!(tag("/MK << /R 90 >>"), tag(""));
    assert_eq!(tag("/MK << /R 91 >>"), tag(""));
}

/// A widget inherits field entries through any number of `/Parent` fields, and a
/// loop of parents ends the search.
#[test]
fn fields_inherit_through_deep_parents_and_loops_end() {
    for depth in [63, 64, 100, 1000] {
        let mut objects = vec![
            b"<< /Type /Annot /Subtype /Widget /Parent 5 0 R /Rect [10 10 110 30] >>".to_vec(),
        ];
        for n in 5..5 + depth {
            objects.push(if n == 4 + depth {
                b"<< /FT /Tx /V (Hello) >>".to_vec()
            } else {
                format!("<< /Parent {} 0 R >>", n + 1).into_bytes()
            });
        }
        let pdf = annotations("/Annots [4 0 R]", objects);
        assert_eq!(looks(&pdf), ["generated"], "{depth} parents");
    }
    // A widget whose parents loop is a broken field, drawn as nothing even with all
    // it needs of its own, as viewers draw it; a null entry is inherited.
    let pdf = annotations(
        "/Annots [4 0 R 7 0 R 9 0 R]",
        vec![
            b"<< /Type /Annot /Subtype /Widget /Parent 5 0 R /Rect [10 10 110 30] >>".to_vec(),
            b"<< /Parent 6 0 R >>".to_vec(),
            b"<< /Parent 5 0 R >>".to_vec(),
            b"<< /Type /Annot /Subtype /Widget /FT /Tx /V (Hello) /Parent 8 0 R \
               /Rect [10 10 110 30] >>"
                .to_vec(),
            b"<< /Parent 7 0 R >>".to_vec(),
            b"<< /Type /Annot /Subtype /Widget /FT null /Parent 10 0 R /Rect [10 10 110 30] >>"
                .to_vec(),
            b"<< /FT /Tx /V (Hello) >>".to_vec(),
        ],
    );
    assert_eq!(looks(&pdf), ["nothing", "nothing", "generated"]);
}

/// A comb field's `/MaxLen` of no cells makes no comb, as viewers read it: the value
/// is drawn as in a plain text field (larger ones are in `annotation_cost.rs`).
/// Cells finer than can be told apart are drawn as the band their dividers cover.
#[test]
fn comb_cells_follow_max_len() {
    let comb = |max_len: &str| {
        draw(
            &[&format!(
                "/Subtype /Widget /FT /Tx /Ff 16777216 /MaxLen {max_len} /V (AB) \
                 /MK << /BC [0] >> /Rect [20 100 180 130]"
            )],
            VIEW,
        )
    };
    // The border's path, half its width inside the field.
    let frame = [21, 101, 180, 130];
    for plain in ["0", "-5"] {
        assert_eq!(strokes(&comb(plain)), [frame], "{plain}");
    }
    // Two cells: one divider, at the middle of the inside.
    assert_eq!(strokes(&comb("2")), [frame, [100, 101, 100, 129]]);
    // The text over the dividers, which a border wider than a cell covers (observed).
    assert!(matches!(comb("2").last(), Some(Ev::Fill { .. })));
    // The dividers of 1000 cells 0.158 pt apart, 1 pt wide, cover the inside; they
    // are drawn before the text.
    let band = comb("1000");
    let Some(Ev::Stroke { bbox, width, .. }) =
        band.iter().rfind(|e| matches!(e, Ev::Stroke { .. }))
    else {
        panic!("{band:?}");
    };
    assert_eq!((*bbox, *width), ([21, 115, 179, 115], 28.0), "{band:?}");
}

/// A text markup quad of no height marks no text and draws nothing, as viewers draw it.
#[test]
fn a_quad_of_no_height_draws_nothing() {
    let quad = "/QuadPoints [0 100 200 100 0 100 200 100] /Rect [0 100 200 101]";
    for subtype in ["Underline", "StrikeOut", "Squiggly"] {
        let pdf = annotations(
            "/Annots [4 0 R]",
            vec![format!("<< /Subtype /{subtype} /C [0] {quad} >>").into_bytes()],
        );
        assert_eq!(looks(&pdf), ["nothing"], "{subtype}");
    }
}

/// An annotation without a `/Rect`, or with one that is not an array, draws nothing,
/// as viewers draw it.
#[test]
fn an_annotation_without_a_rect_draws_nothing() {
    let pdf = annotations(
        "/Annots [4 0 R 5 0 R 6 0 R]",
        vec![
            b"<< /Subtype /Text >>".to_vec(),
            b"<< /Subtype /Text /Rect 5 >>".to_vec(),
            b"<< /Subtype /Square /C [1 0 0] /AP << /N 7 0 R >> >>".to_vec(),
            common::stream("/BBox [0 0 10 10]", b"1 0 0 rg 0 0 10 10 re f"),
        ],
    );
    assert_eq!(looks(&pdf), ["nothing", "nothing", "nothing"]);
}

/// A widget without a `/Rect` is drawn by viewers at coordinates of ±2^32: a
/// button, or a text or choice field with a background, covers the page there or
/// shows nowhere, so it is unsupported; other widgets show nothing.
#[test]
fn a_widget_without_a_rect_that_shows_is_unsupported() {
    let fields = [
        ("/FT /Tx /V (A) /MK << /BG [0.9 0.9 1] >>", "Coordinates"),
        (
            "/FT /Ch /V (A) /Opt [(A)] /MK << /BG [0.9] >>",
            "Coordinates",
        ),
        ("/FT /Btn /AS /Yes", "Coordinates"),
        ("/FT /Tx /V (A) /MK << /BC [0] >>", "nothing"),
        ("/FT /Tx /V (A) /Rect 5 /MK << /BG [] >>", "nothing"),
        ("/FT /Sig /MK << /BG [0.9] >>", "nothing"),
    ];
    for (field, look) in fields {
        let pdf = annotations(
            "/Annots [4 0 R]",
            vec![format!("<< /Type /Annot /Subtype /Widget /T (f) {field} >>").into_bytes()],
        );
        assert_eq!(looks(&pdf), [look], "{field}");
    }
}

/// A line or polyline that draws an ending at a negative border width is
/// unsupported: viewers clip the ending by rules not reproduced. Without an ending,
/// or at a width of 0 or more, it is drawn.
#[test]
fn a_line_ending_at_a_negative_width_is_unsupported() {
    let cases = [
        (
            "/Subtype /Line /L [20 20 180 40] /LE [/Square /None] /BS << /W -2 >>",
            "Ending",
        ),
        (
            "/Subtype /Line /L [20 20 180 40] /LE [/None /OpenArrow] /Border [0 0 -1]",
            "Ending",
        ),
        (
            "/Subtype /PolyLine /Vertices [20 20 180 40] /LE [/Square /None] /BS << /W -2 >>",
            "Ending",
        ),
        (
            "/Subtype /Line /L [20 20 180 40] /BS << /W -2 >>",
            "generated",
        ),
        (
            "/Subtype /Line /L [20 20 180 40] /LE [/Square /None] /BS << /W 0 >>",
            "generated",
        ),
        (
            "/Subtype /PolyLine /Vertices [20 20 180 40] /BS << /W -2 >>",
            "generated",
        ),
        // A polygon has no endings.
        (
            "/Subtype /Polygon /Vertices [20 20 180 40 20 60] /LE [/Square /None] /BS << /W -2 >>",
            "generated",
        ),
    ];
    for (annot, look) in cases {
        let pdf = annotations(
            "/Annots [4 0 R]",
            vec![format!("<< /Type /Annot {annot} /C [0] /Rect [0 0 200 200] >>").into_bytes()],
        );
        assert_eq!(looks(&pdf), [look], "{annot}");
    }
}

/// Rich free text is clipped to its layout box as well as to the box less the
/// border: from the padding on the left to twice it on the right, from the
/// padding below the top to twice the default appearance's size below the bottom
/// (observed: `2 2 154 186 re W n` in a frame 160 high, flipped, at 14 pt), turned
/// with the text. Plain text is not.
#[test]
fn rich_free_text_is_clipped_to_its_layout_box() {
    let free_text = |entries: &str| {
        content(
            &format!(
                "/Subtype /FreeText /Rect [20 20 180 180] /DA (/Helv 14 Tf 0 g) \
                 /Contents (A) {entries}"
            ),
            vec![],
        )
    };
    let layout = "22 -8 m\n176 -8 l\n176 178 l\n22 178 l\nh\nW n";
    assert!(free_text("/DS (font-size:14pt)").contains(layout));
    assert!(free_text("/RC (<p>A</p>)").contains(layout));
    assert!(!free_text("").contains(layout));
    let turned = "208 22 m\n208 176 l\n22 176 l\n22 22 l\nh\nW n";
    assert!(free_text("/DS (font-size:14pt) /Rotate 90").contains(turned));
}

/// A text string that is not UTF-16 ends at its first zero byte, as viewers read
/// it; a zero character in UTF-16 is unsupported. In `ZapfDingbats` a control
/// character draws nothing and advances as a space (observed).
#[test]
fn text_ends_at_a_zero_byte() {
    let field = |v: &str, da: &str| {
        content(
            &format!("/Subtype /Widget /FT /Tx /V {v} /DA ({da} 0 g) /Rect [20 80 180 120]"),
            vec![],
        )
    };
    let helv = "/Helv 14 Tf";
    assert_eq!(field("(AB\\000CD)", helv), field("(AB)", helv));
    assert_eq!(field("<EFBBBF4142004344>", helv), field("(AB)", helv));
    assert_eq!(field("<FEFF0041000000420043>", helv), "Character('\\0')");
    let free_text = |contents: &str| {
        content(
            &format!(
                "/Subtype /FreeText /Rect [20 20 180 180] /DA ({helv} 0 g) /Contents {contents}"
            ),
            vec![],
        )
    };
    assert_eq!(free_text("(AB\\000CD)"), free_text("(AB)"));
    assert_eq!(
        free_text("(A) /RC <FEFF0041000000420043>"),
        "Character('\\0')"
    );
    let zadb = "/ZaDb 14 Tf";
    assert_eq!(field("(a\\ra)", zadb), field("(a a)", zadb));
    assert_eq!(field("(a\\001a)", zadb), field("(a a)", zadb));
}

/// A line of no length has no direction: viewers draw the points of an ending laid
/// along or across it from the page's origin and clip them to a box computed
/// otherwise, which is unsupported; an ending without a direction is drawn, and with
/// a leader line all of it is drawn in one point, which shows nothing (observed).
#[test]
fn a_line_of_no_length_draws_only_endings_without_a_direction() {
    let line = |entries: &str| {
        content(
            &format!(
                "/Subtype /Line /L [50 60 50 60] /C [0] /BS << /W 3 >> /Rect [0 0 200 200] {entries}"
            ),
            vec![],
        )
    };
    assert_eq!(line("/LE [/OpenArrow /None]"), "Ending");
    assert_eq!(line("/LE [/None /Slash]"), "Ending");
    assert!(line("/LE [/Square /None]").contains("41 51 18 18 re"));
    assert_eq!(line("/LE [/Circle /OpenArrow] /LL 20"), "nothing");
    // At the origin, the ending's points are its tip.
    let origin = content(
        "/Subtype /Line /L [0 0 0 0] /LE [/OpenArrow /None] /C [0] /Rect [0 0 200 200]",
        vec![],
    );
    assert!(origin.contains("0 0 m\n0 0 l\n0 0 l\nS"), "{origin}");
    let polyline = |vertices: &str| {
        content(
            &format!(
                "/Subtype /PolyLine /Vertices [{vertices}] /LE [/OpenArrow /None] /C [0] \
                 /Rect [0 0 200 200]"
            ),
            vec![],
        )
    };
    assert_eq!(polyline("50 60 50 60 100 100"), "Ending");
    assert_ne!(polyline("50 60 100 100 100 100"), "Ending");
    let callout = content(
        "/Subtype /FreeText /IT /FreeTextCallout /Rect [20 20 180 180] /RD [80 0 0 80] \
         /CL [50 60 50 60 100 60] /LE /OpenArrow /DA (/Helv 12 Tf 1 0 0 rg) /Contents (A)",
        vec![],
    );
    assert_eq!(callout, "Ending");
}

/// A coordinate too large to draw makes the annotation unsupported, not empty.
#[test]
fn a_coordinate_too_large_to_draw_is_unsupported() {
    let pdf = annotations(
        "/Annots [4 0 R]",
        vec![
            b"<< /Subtype /Polygon /Vertices [10 10 100000000 50 50 50] \
               /C [0] /IC [1 0 0] /Rect [0 0 200 200] >>"
                .to_vec(),
        ],
    );
    assert_eq!(looks(&pdf), ["Coordinates"]);
}

/// An array's entry that refers to no object, is null or is not a number reads as 0
/// and keeps its place, as viewers read it: the coordinates and colour components
/// after it keep their meaning.
#[test]
fn a_damaged_array_entry_reads_as_zero_in_its_place() {
    let same = |damaged: &str, read: &str| {
        let got = draw(&[damaged], VIEW);
        assert!(!got.is_empty(), "{damaged}");
        assert_eq!(got, draw(&[read], VIEW), "{damaged}");
    };
    same(
        "/Subtype /Square /Rect [40 99 0 R 160 80] /IC [1 0 0] /C [0]",
        "/Subtype /Square /Rect [40 0 160 80] /IC [1 0 0] /C [0]",
    );
    same(
        "/Subtype /Highlight /Rect [0 0 200 200] /QuadPoints [10 40 99 0 R 40 10 30 90 30]",
        "/Subtype /Highlight /Rect [0 0 200 200] /QuadPoints [10 40 0 40 10 30 90 30]",
    );
    same(
        "/Subtype /Ink /C [0] /Rect [0 0 200 200] /InkList [[10 10 99 0 R 50 50 50] 98 0 R [9 9 40 40]]",
        "/Subtype /Ink /C [0] /Rect [0 0 200 200] /InkList [[10 10 0 50 50 50] [9 9 40 40]]",
    );
    same(
        "/Subtype /Line /C [0] /Rect [0 0 200 200] /L [20 20 100 100] /LE [99 0 R /Circle]",
        "/Subtype /Line /C [0] /Rect [0 0 200 200] /L [20 20 100 100] /LE [/None /Circle]",
    );
    for c in ["[1 null 0]", "[1 /x 0]", "[1 99 0 R 0]"] {
        let red = "[1 0 0]";
        for annot in [
            "/Subtype /Line /Rect [20 20 100 100] /C {} /L [20 20 100 100]",
            "/Subtype /Square /Rect [20 20 100 100] /IC {}",
            "/Subtype /Ink /Rect [0 0 200 200] /C {} /InkList [[10 10 50 50]]",
            "/Subtype /FreeText /Rect [20 20 100 100] /C {} /DA (/Helv 12 Tf 0 g)",
            "/Subtype /Widget /FT /Tx /Rect [20 20 100 100] /MK << /BC {} /BG [0 0 1] >>",
            "/Subtype /Widget /FT /Tx /Rect [20 20 100 100] /MK << /BC [0] /BG {} >>",
        ] {
            same(&annot.replace("{}", c), &annot.replace("{}", red));
        }
    }
    // Four components or more are CMYK, of the first four.
    same(
        "/Subtype /Line /Rect [20 20 100 100] /C [1 0 0 0 0] /L [20 20 100 100]",
        "/Subtype /Line /Rect [20 20 100 100] /C [1 0 0 0] /L [20 20 100 100]",
    );
}

/// An appearance stream's `/BBox` and `/Matrix` are read as viewers read damaged ones:
/// an entry that is not a number as 0, a short array with 0 for what it lacks, a
/// `/Matrix` that is not an array as the identity. The renderer draws the stream where
/// they put it, which is where viewers draw it.
#[test]
fn a_damaged_box_or_matrix_places_the_appearance_as_viewers_do() {
    let drawn = |bbox: &str, matrix: &str| {
        let ev = draw_objects(vec![
            b"<< /Type /Annot /Subtype /Square /Rect [30 40 80 100] /AP << /N 5 0 R >> >>".to_vec(),
            common::stream(&format!("/BBox {bbox} {matrix}"), b"0 g 0 0 4 3 re f"),
        ]);
        fills(&ev).into_iter().map(|(b, _)| b).collect::<Vec<_>>()
    };
    // The box 5 × 6 times larger on /Rect: a 4 × 3 fill is 20 × 18.
    let upright = vec![[30, 40, 50, 58]];
    let flipped = vec![[30, 82, 50, 100]];
    assert_eq!(drawn("[0 0 10 10]", ""), upright);
    assert_eq!(drawn("[0 /x 10 10]", ""), upright);
    assert_eq!(drawn("[0 99 0 R 10 10 7]", ""), upright);
    assert_eq!(drawn("[0 0 10 10]", "/Matrix (x)"), upright);
    assert_eq!(drawn("[0 0 10 10]", "/Matrix [1 0 0 -1 0 0]"), flipped);
    assert_eq!(drawn("[0 0 10 10]", "/Matrix [1 0 0 -1 0 (bad)]"), flipped);
    assert_eq!(drawn("[0 0 10 10]", "/Matrix [1 0 0 -1]"), flipped);
    // A matrix of no area, and a box of none, draw nothing.
    assert_eq!(
        drawn("[0 0 10 10]", "/Matrix [1 0 0]"),
        Vec::<[i32; 4]>::new()
    );
    assert_eq!(drawn("[0 0 10]", ""), Vec::<[i32; 4]>::new());

    // A form drawn by the page's content reads its box and matrix the same way.
    let in_content = |bbox: &str, matrix: &str| {
        let pdf = annotations(
            "/Contents 4 0 R /Resources << /XObject << /X 5 0 R >> >>",
            vec![
                common::stream("", b"q 5 0 0 6 30 70 cm /X Do Q"),
                common::stream(
                    &format!("/Type /XObject /Subtype /Form /BBox {bbox} {matrix}"),
                    b"0 g 0 0 4 3 re f",
                ),
            ],
        );
        let mut rec = Rec::default();
        common::interpret_first_page(&pdf, InterpreterSettings::default(), &mut rec);
        fills(&rec.0)
            .into_iter()
            .map(|(b, _)| b)
            .collect::<Vec<_>>()
    };
    let upright = vec![[30, 70, 50, 88]];
    let flipped = vec![[30, 52, 50, 70]];
    assert_eq!(in_content("[0 0 10 10]", ""), upright);
    assert_eq!(in_content("[0 /x 10 10]", ""), upright);
    assert_eq!(in_content("[0 99 0 R 10 10 7]", ""), upright);
    assert_eq!(
        in_content("[0 0 10 10]", "/Matrix [1 0 0 -1 0 (bad)]"),
        flipped
    );
    assert_eq!(in_content("[0 0 10 10]", "/Matrix [1 0 0 -1]"), flipped);

    // A writer gets the same box and matrix to draw the stream with.
    let pdf = annotations(
        "/Annots [4 0 R]",
        vec![
            b"<< /Type /Annot /Subtype /Square /Rect [30 40 80 100] /AP << /N 5 0 R >> >>".to_vec(),
            common::stream("/BBox [0 /x 10 10] /Matrix [1 0 0 -1]", b"0 g 0 0 4 3 re f"),
        ],
    );
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    let Look::Stream { placement, .. } = appearances.look(&annots(&pdf)[0], PAGE) else {
        panic!("an appearance stream");
    };
    assert_eq!(placement.bbox(), [0.0, 0.0, 10.0, 10.0]);
    assert_eq!(placement.matrix(), [1.0, 0.0, 0.0, -1.0, 0.0, 0.0]);
    assert_eq!(placement.transform(), [5.0, 0.0, 0.0, 6.0, 30.0, 100.0]);
}

/// A number of 2^23 or more, where the 32-bit numbers content streams are read in
/// are a unit or more apart, fails the whole appearance, in whichever quadrilateral, path or entry it is: none is left out as
/// if it drew nothing. So does one in where an appearance stream is placed.
#[test]
fn every_number_too_large_to_write_is_unsupported() {
    const H: &str = "100000000";
    let quad = format!("/Rect [0 0 200 200] /QuadPoints [10 80 {H} 80 10 50 {H} 50]");
    let mut cases: Vec<String> = ["Highlight", "Underline", "StrikeOut", "Squiggly", "Redact"]
        .iter()
        .map(|s| format!("/Subtype /{s} /C [1 0 0] {quad}"))
        .collect();
    cases.extend([
        // One among quads that can be drawn.
        format!(
            "/Subtype /Highlight /Rect [0 0 200 200] \
             /QuadPoints [10 50 {H} 50 10 30 200 30 10 80 90 80 10 60 90 60]"
        ),
        format!("/Subtype /Ink /C [0] /Rect [0 0 200 200] /InkList [[10 10 {H} 50 50 50] [10 100 50 100]]"),
        format!("/Subtype /Square /C [0] /BS << /W {H} >> /Rect [10 10 50 50]"),
        format!("/Subtype /Line /C [0] /Rect [0 0 200 200] /L [20 20 100 100] /LL {H}"),
        format!("/Subtype /Polygon /C [0] /Rect [0 0 200 200] /Vertices [10 10 50 10 50 {H}]"),
        format!("/Subtype /Widget /FT /Btn /Ff 32768 /MK << /BG [0] >> /Rect [0 0 {H} {H}]"),
        format!("/Subtype /FreeText /Rect [0 0 200 200] /Contents (A) /DA (/Helv {H} Tf)"),
        // Text clipped away is checked as text drawn is.
        format!("/Subtype /Widget /FT /Tx /V (A) /DA (/Helv {H} Tf) /Rect [10 10 100 30]"),
        format!("/Subtype /Widget /FT /Tx /Ff 4096 /V (A) /DA (/Helv {H} Tf) /Rect [10 10 100 30]"),
        format!(
            "/Subtype /FreeText /Rect [0 0 200 200] /RC (<p style=\"font-size:{H}pt\">A</p>)"
        ),
        format!("/Subtype /FreeText /Rect [0 0 200 200] /DS (font-size:{H}pt) /Contents (A)"),
    ]);
    for annot in &cases {
        let pdf = annotations(
            "/Annots [4 0 R]",
            vec![format!("<< /Type /Annot {annot} >>").into_bytes()],
        );
        assert_eq!(looks(&pdf), ["Coordinates"], "{annot}");
    }
    for (rect, matrix) in [
        (format!("[0 {H} 200]"), ""),
        ("[0 0 200 200]".to_owned(), "/Matrix [1 0 0 1 0 1e40]"),
        (
            "[0 0 200 200]".to_owned(),
            &*format!("/Matrix [1 0 0 1 0 {H}]"),
        ),
    ] {
        let pdf = annotations(
            "/Annots [4 0 R]",
            vec![
                format!("<< /Type /Annot /Subtype /Square /Rect {rect} /AP << /N 5 0 R >> >>")
                    .into_bytes(),
                common::stream(&format!("/BBox [0 0 10 10] {matrix}"), b"0 g 0 0 4 3 re f"),
            ],
        );
        let expected = if matrix.contains("1e40") {
            // Not a number: the identity.
            "0 g 0 0 4 3 re f"
        } else {
            "Coordinates"
        };
        assert_eq!(looks(&pdf), [expected], "{rect} {matrix}");
    }
}

/// The numbers of the path operators and line widths in `content`, in order.
fn path_numbers(content: &str) -> Vec<f64> {
    content
        .lines()
        .filter(|l| [" m", " l", " c", " w"].iter().any(|op| l.ends_with(op)))
        .flat_map(|l| l.split(' ').filter_map(|t| t.parse::<f64>().ok()))
        .collect()
}

fn assert_near(got: &[f64], expected: &[f64], what: &str) {
    assert!(
        got.len() >= expected.len() && got.iter().zip(expected).all(|(g, e)| (g - e).abs() < 1e-3),
        "{what}: {got:?}"
    );
}

/// Text markup on a slanted, vertical or reversed quadrilateral is drawn in the
/// quadrilateral's own orientation, as viewers draw it: the numbers are those of the
/// appearances a common viewer generates for these quads.
#[test]
fn text_markup_follows_the_orientation_of_its_quads() {
    let content = |subtype: &str, quad: &str| {
        generated(&format!(
            "/Subtype /{subtype} /C [0] /Rect [0 0 200 200] /QuadPoints [{quad}]"
        ))
    };
    let slanted = "30 40 100 120 45 25 115 105";
    let vertical = "40 20 40 100 50 20 50 100";
    let reversed = "45 25 115 105 100 120 30 40";
    let cases: [(&str, &str, &[f64]); 12] = [
        (
            "Underline",
            slanted,
            &[1.325_825_2, 42.857_145, 27.142_857, 112.857_14, 107.142_86],
        ),
        (
            "StrikeOut",
            slanted,
            &[1.325_825_2, 38.571_428, 31.428_572, 108.571_43, 111.428_57],
        ),
        (
            "Squiggly",
            slanted,
            &[
                1.325_825_2,
                45.0,
                25.0,
                44.852_716,
                29.423_508,
                48.991_144,
                29.561_305,
            ],
        ),
        (
            "Highlight",
            slanted,
            &[
                45.0,
                25.0,
                37.944_36,
                24.529_625,
                30.470_376,
                32.944_36,
                30.0,
                40.0,
                100.0,
                120.0,
                107.055_64,
                120.470_378,
                114.529_628,
                112.055_64,
                115.0,
                105.0,
            ],
        ),
        (
            "Redact",
            slanted,
            &[1.0, 45.0, 25.0, 115.0, 105.0, 100.0, 120.0, 30.0, 40.0],
        ),
        (
            "Underline",
            vertical,
            &[0.625, 48.571_43, 20.0, 48.571_43, 100.0],
        ),
        (
            "StrikeOut",
            vertical,
            &[0.625, 45.714_288, 20.0, 45.714_288, 100.0],
        ),
        (
            "Squiggly",
            vertical,
            &[0.625, 50.0, 20.0, 48.571_43, 21.428_572, 50.0, 22.857_144],
        ),
        (
            "Highlight",
            vertical,
            &[
                50.0, 20.0, 47.6429, 17.642_899, 42.3571, 17.642_899, 40.0, 20.0, 40.0, 100.0,
                42.3571, 102.3571, 47.6429, 102.3571, 50.0, 100.0,
            ],
        ),
        (
            "Redact",
            vertical,
            &[1.0, 50.0, 20.0, 50.0, 100.0, 40.0, 100.0, 40.0, 20.0],
        ),
        (
            "Underline",
            reversed,
            &[6.860_780_9, 92.142_85, 106.428_57, 42.142_858, 49.285_715],
        ),
        (
            "Squiggly",
            reversed,
            &[
                6.860_780_9,
                100.0,
                120.0,
                84.766_76,
                97.998_76,
                79.346_95,
                96.396_51,
            ],
        ),
    ];
    for (subtype, quad, expected) in cases {
        assert_near(
            &path_numbers(&content(subtype, quad)),
            expected,
            &format!("{subtype} {quad}"),
        );
    }
    // The zigzag ends at the first corner at or past the bottom right corner.
    let squiggle = path_numbers(&content("Squiggly", slanted));
    assert_eq!(squiggle.len(), 1 + 2 * 37, "{squiggle:?}");
    assert_near(
        &squiggle[squiggle.len() - 2..],
        &[116.840_518, 107.103_44],
        "end",
    );
    // The clip: each quad's box a line width wider, and a highlight's bulges.
    assert!(
        content("Underline", slanted)
            .starts_with("q\n28.674175 23.674175 87.65165 97.65165 re\nW n\n"),
        "{}",
        content("Underline", slanted)
    );
}

/// A highlight quad whose bottom edge has no length has no direction: viewers put
/// its curves' control points at the origin, outside the clip of its quad.
#[test]
fn a_highlight_quad_without_a_bottom_edge_bulges_to_the_origin() {
    let content = generated(
        "/Subtype /Highlight /Rect [0 0 200 200] /QuadPoints [110 124 115 124 100 100 100 100]",
    );
    assert_near(
        &path_numbers(&content),
        &[
            100.0, 100.0, 0.0, 0.0, 0.0, 0.0, 110.0, 124.0, 115.0, 124.0, 0.0, 0.0, 0.0, 0.0,
            100.0, 100.0,
        ],
        &content,
    );
    assert!(
        content.starts_with("q\n98.375 98.375 18.25 27.25 re\nW n\n"),
        "{content}"
    );
}

/// A comment in rich text draws nothing, whatever `>` it holds: the text after it
/// stays on the first line, inside the box.
#[test]
fn a_comment_in_rich_text_draws_nothing() {
    let rc = format!(
        "<body style=\"font-family:Helvetica\"><p>A<!-- >{} --><span \
         style=\"color:#ff0000\">IMPORTANT</span></p></body>",
        "M".repeat(200)
    );
    let text = fills(&draw(
        &[&format!(
            "/Subtype /FreeText /Rect [10 80 190 140] /BS << /W 0 >> \
             /DA (/Helv 12 Tf 0 g) /RC ({rc})"
        )],
        VIEW,
    ));
    assert_eq!(
        text.iter().filter(|(_, c)| *c == RED).count(),
        1,
        "{text:?}"
    );
    for (bbox, _) in &text {
        assert!(inside(*bbox, [10, 125, 190, 140]), "{text:?}");
    }
}

/// Rich text asks the font resolver for the face of each part: the default face
/// where it names no family the standard fonts have, in its weight and slant, and
/// the standard fonts' bold and italic.
#[test]
fn rich_text_asks_for_the_face_of_each_part() {
    let rc = "<body><p>a<b>b</b><i>c</i></p><p style=\"font-family:Times New Roman;\
              font-weight:700\">d<i>e</i></p><p style=\"font:italic 9pt Helvetica\">f</p>\
              <p style=\"font-family:Courier;font-weight:bold;font-style:oblique\">g</p></body>";
    let asked = faces_asked(&format!(
        "/Subtype /FreeText /Rect [20 20 180 180] /BS << /W 0 >> /RC ({rc})"
    ));
    for face in [
        "default false false",
        "default true false",
        "default false true",
        "Times-Bold",
        "Times-BoldItalic",
        "Helvetica-Oblique",
        "Courier-BoldOblique",
    ] {
        assert!(asked.iter().any(|a| a == face), "{face}: {asked:?}");
    }
    assert!(!asked.iter().any(|a| a == "Times-Roman"), "{asked:?}");
}

/// The fonts the appearance of `annot` asks the font resolver for.
fn faces_asked(annot: &str) -> Vec<String> {
    use hayro_interpret::font::FontQuery;
    use std::sync::{Arc, Mutex};
    let asked = Arc::new(Mutex::new(Vec::new()));
    let record = asked.clone();
    let settings = InterpreterSettings {
        font_resolver: Arc::new(move |query: &FontQuery| {
            record.lock().unwrap().push(match query {
                FontQuery::Standard(s) => s.postscript_name().to_owned(),
                FontQuery::TextDefault { bold, italic } => format!("default {bold} {italic}"),
                FontQuery::Fallback(_) => "fallback".to_owned(),
            });
            Some(query.standard_substitute().get_font_data())
        }),
        ..Default::default()
    };
    let file = common::one_page(
        200,
        "/Resources << >> /Annots [4 0 R]",
        vec![format!("<< /Type /Annot {annot} >>").into_bytes()],
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    common::interpret_first_page(&pdf, settings, &mut Rec::default());
    asked.lock().unwrap().clone()
}

/// A default appearance's font is Times for `TiRo`, Courier for `Cour`,
/// `ZapfDingbats` for `ZaDb`, and Helvetica for any other name, whatever the form's
/// resources say it is: the faces viewers draw it in.
#[test]
fn a_default_appearance_font_is_one_of_four_names() {
    for (name, face) in [
        ("Helv", "Helvetica"),
        ("TiRo", "Times-Roman"),
        ("Cour", "Courier"),
        ("ZaDb", "ZapfDingbats"),
        ("TiBo", "Helvetica"),
        ("CoBo", "Helvetica"),
        ("Times-Roman", "Helvetica"),
        ("tiro", "Helvetica"),
    ] {
        for annot in [
            "/Subtype /FreeText /Rect [20 20 180 180] /Contents (4)",
            "/Subtype /Widget /FT /Tx /Rect [20 20 180 60] /V (4)",
        ] {
            let asked = faces_asked(&format!("{annot} /DA (/{name} 12 Tf 0 g)"));
            // Helvetica may be asked as well, for glyphs the face lacks.
            assert!(
                asked.iter().any(|a| a == face)
                    && asked.iter().all(|a| a == face || a == "Helvetica"),
                "{name} in {annot}: {asked:?}"
            );
        }
    }
}

/// Rich text is aligned by its `text-align`, whatever `/Q`; a justified line
/// reaches both sides of the box, the last line of its paragraph does not.
#[test]
fn rich_text_aligns_by_text_align_not_q() {
    let spans = |q: u8, body: &str| {
        let rc = format!("<body style=\"font-family:Helvetica\">{body}</body>");
        fills(&draw(
            &[&format!(
                "/Subtype /FreeText /Rect [20 20 180 180] /BS << /W 0 >> /Q {q} /RC ({rc})"
            )],
            VIEW,
        ))
        .iter()
        .map(|(b, _)| [b[0], b[2]])
        .collect::<Vec<_>>()
    };
    let left = spans(1, "<p>H</p>");
    assert_eq!(left[0][0], 21, "{left:?}");
    let right = spans(0, "<p style=\"text-align:right\">H</p>");
    assert_eq!(right[0][1], 179, "{right:?}");
    let justified = spans(
        0,
        "<p style=\"text-align:justify\">HH HH HH HH HH HH HH HH HH HH HH HH HH</p>",
    );
    // The first line from side to side, the last from the left.
    assert_eq!(
        (justified[0][0], justified[0][1]),
        (21, 179),
        "{justified:?}"
    );
}

/// The lines of a `FreeText` in `/Rect [20 20 180 180]`: the bottom of each fill.
fn free_text_rows(entries: &str) -> Vec<i32> {
    fills(&draw(
        &[&format!(
            "/Subtype /FreeText /Rect [20 20 180 180] /BS << /W 0 >> {entries}"
        )],
        VIEW,
    ))
    .iter()
    .map(|(b, _)| b[1])
    .collect()
}

/// A word wider than the box stays whole on a line of its own, overflowing it, as
/// viewers set it; the words after it go on the next line.
#[test]
fn a_word_wider_than_the_box_overflows_its_line() {
    let rows = free_text_rows(&format!(
        "/DA (/Helv 12 Tf 0 g) /Contents (ab {} cd)",
        "H".repeat(40)
    ));
    // One fill per line: "ab", the long word, "cd".
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows[0] > rows[1] && rows[1] > rows[2], "{rows:?}");
}

/// A carriage return before a paragraph's end is one line break, whichever span
/// each is in, as Acrobat writes `&#13;` at the end of its paragraphs.
#[test]
fn a_carriage_return_ending_a_paragraph_breaks_once() {
    let rows = free_text_rows(
        "/RC (<body style=\"font-family:Helvetica;font-size:10pt\"><p><span \
         style=\"color:#ff0000\">H&#13;</span></p><p>H</p></body>)",
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0] - rows[1], 12, "{rows:?}");
}

/// A rich text colour is read as CSS reads numbers: words a float parser also takes
/// for numbers, such as `nan` and `inf`, make it unsupported rather than a colour.
#[test]
fn a_rich_text_colour_of_no_css_number_is_unsupported() {
    for color in ["rgb(nan, 0, 0)", "rgb(inf, 0, 0)", "rgb(0, 0, infinity)"] {
        let pdf = annotations(
            "/Annots [4 0 R]",
            vec![
                format!(
                    "<< /Type /Annot /Subtype /FreeText /Rect [0 0 200 200] \
                     /RC (<p style=\"color:{color}\">A</p>) >>"
                )
                .into_bytes(),
            ],
        );
        assert_eq!(looks(&pdf), ["RichText"], "{color}");
    }
}

/// What a page with `/Annots annots` draws when annotations are flattened, object 4
/// being a red square.
fn red_square_after(annots: &str) -> Vec<Ev> {
    let file = common::one_page(
        200,
        &format!("/Resources << >> /Annots {annots}"),
        vec![b"<< /Type /Annot /Subtype /Square /Rect [20 20 100 100] /IC [1 0 0] >>".to_vec()],
    );
    let pdf = Pdf::new(file).expect("valid pdf");
    let settings = InterpreterSettings {
        annotations: AnnotationSelection::Flatten,
        ..Default::default()
    };
    let mut rec = Rec::default();
    common::interpret_first_page(&pdf, settings, &mut rec);
    rec.0
}

/// An entry of `/Annots` that is no annotation, null, or refers to no object, is
/// passed over: the annotations after it are drawn.
#[test]
fn a_damaged_annots_entry_hides_none_after_it() {
    let square = red_square_after("[4 0 R]");
    assert!(!square.is_empty());
    for annots in [
        "[null 4 0 R]",
        "[999 0 R 4 0 R]",
        "[5 4 0 R]",
        "[(x) [1] 4 0 R]",
    ] {
        assert_eq!(red_square_after(annots), square, "{annots}");
    }
}

/// A comment in a default appearance string is white space, as in content: the
/// size and colour around it are kept.
#[test]
fn a_comment_in_a_default_appearance_is_white_space() {
    let free_text = |da: &str| {
        generated(&format!(
            "/Subtype /FreeText /Rect [10 10 190 190] /BS << /W 0 >> /Contents (IMPORTANT) /DA ({da})"
        ))
    };
    let plain = free_text("/Helv 20 Tf 1 0 0 rg");
    for da in [
        "/Helv 20%comment\\nTf 1 0 0 rg",
        "/Helv%comment\\n20 Tf 1 0 0 rg",
        "/Helv 20 Tf %comment\\n1 0 0 rg",
        "/Helv 20 Tf 1 0 0%comment\\nrg",
        "%comment\\n/Helv 20 Tf 1 0 0 rg",
    ] {
        assert_eq!(free_text(da), plain, "{da}");
    }
    assert_ne!(free_text("/Helv 12 Tf 1 0 0 rg"), plain);
}

/// Field flags (`/Ff`) are read as `/F` is, as viewers read damaged ones: an integer
/// to its low 32 bits; a real rounded half up, to its low 32 bits when that is 2^32
/// or more away from 0, and to bit 31 alone between 2^31 and 2^32. A push button
/// stays one.
#[test]
fn a_damaged_field_flags_entry_reads_as_viewers_read_it() {
    let button = |ff: &str| {
        generated(&format!(
            "/Subtype /Widget /FT /Btn /Ff {ff} /Rect [20 20 180 60] \
             /MK << /BG [0.5] /BC [0] /CA (IMPORTANT) >>"
        ))
    };
    let push = button("65536");
    let check = button("0");
    assert_ne!(push, check);
    for ff in [
        "-1",
        "4295032832",
        "-4294901760",
        "65536.9",
        "65535.5",
        "2147418112.0",
        "-2147418112.0",
        "4295032832.0",
        "10000000000.0",
        "-4294967298.0",
    ] {
        assert_eq!(button(ff), push, "/Ff {ff}");
    }
    for ff in [
        "2147483648.0",
        "4294901760.0",
        "-4294901759.5",
        "-2147483904.0",
        "(x)",
    ] {
        assert_eq!(button(ff), check, "/Ff {ff}");
    }
}

/// A text field's value may be a text stream; its text is drawn as a string's is.
#[test]
fn a_text_field_draws_a_value_stream() {
    let field = "/Subtype /Widget /FT /Tx /DA (/Helv 12 Tf 0 g) /Rect [20 20 180 60]";
    let drawn = |value: &str, objects: Vec<Vec<u8>>| {
        let pdf = annotations(
            "/Annots [4 0 R]",
            [
                vec![format!("<< /Type /Annot {field} {value} >>").into_bytes()],
                objects,
            ]
            .concat(),
        );
        let appearances =
            Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
        match appearances.look(&annots(&pdf)[0], PAGE) {
            Look::Generated(a) => a.content,
            _ => panic!("{value}: not generated"),
        }
    };
    let string = drawn("/V (HELLO)", vec![]);
    assert_eq!(
        drawn("/V 5 0 R", vec![common::stream("", b"HELLO")]),
        string
    );
}

/// A square or circle is drawn inside its border width whether the border is drawn
/// or not, and at least 1 wide and high however narrow its `/Rect`: the content a
/// common viewer bakes for the same annotations.
#[test]
fn a_shape_lies_inside_its_border_width_and_is_never_thinner_than_1() {
    for c in ["", "/C [0 0 1]"] {
        let square = |rect: &str, w: &str| {
            generated(&format!(
                "/Subtype /Square /Rect {rect} /IC [1 0 0] {c} /BS << /W {w} >>"
            ))
        };
        let op = if c.is_empty() { "f" } else { "b" };
        for (rect, w, re) in [
            ("[40 40 160 160]", "6", "43 43 114 114 re"),
            ("[40 40 160 160]", "1", "40.5 40.5 119 119 re"),
            ("[40 40 160 160]", "0", "40 40 120 120 re"),
            ("[40 40 44 160]", "6", "43 43 1 114 re"),
            ("[44 160 40 40]", "6", "43 43 1 114 re"),
            ("[40 40 46.9 160]", "6", "43 43 1 114 re"),
            ("[40 40 40 160]", "0", "40 40 1 120 re"),
            ("[40 40 160 160]", "200", "140 140 1 1 re"),
        ] {
            let content = square(rect, w);
            assert!(
                content.contains(&format!("{re}\n{op}")),
                "{c} {rect} {w}: {content}"
            );
        }
        let circle = generated(&format!(
            "/Subtype /Circle /Rect [40 40 44 160] /IC [1 0 0] {c} /BS << /W 6 >>"
        ));
        assert!(circle.contains("43.5 157 m"), "{c}: {circle}");
    }
}

/// What the annotation `annot` (an annotation dictionary's entries, the file's other
/// objects numbered from 5) looks like on a 200 pt page: its generated content,
/// `nothing`, or why it cannot be drawn.
fn content(annot: &str, others: Vec<Vec<u8>>) -> String {
    let mut objects = vec![format!("<< /Type /Annot {annot} >>").into_bytes()];
    objects.extend(others);
    let pdf = annotations("/Annots [4 0 R]", objects);
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    match appearances.look(&annots(&pdf)[0], PAGE) {
        Look::Generated(a) => a.content,
        Look::Nothing => "nothing".into(),
        Look::Unsupported(why) => format!("{why:?}"),
        Look::Stream { .. } => "stream".into(),
    }
}

/// Flags that are reals are read as viewers read them, from the file's bytes: a
/// real beyond the precision of 32-bit floats keeps the annotation when its bits say
/// so (`PyMuPDF` 1.28.2 bakes a square flagged `16777217.0`), and one whose integer
/// part a 64-bit float does not hold makes the annotation unsupported, not hidden.
#[test]
fn real_flags_are_read_as_viewers_read_them() {
    let square = |flags: &str| {
        let pdf = annotations(
            "/Annots [4 0 R]",
            vec![
                format!("<< /Subtype /Square /F {flags} /IC [1 0 0] /Rect [20 20 180 180] >>")
                    .into_bytes(),
            ],
        );
        let annot = &annots(&pdf)[0];
        hayro_interpret::annotation::flattened(annot)
    };
    for (flags, kept) in [
        ("16777217.0", Ok(true)),
        ("16777218.0", Ok(false)),
        ("2.0", Ok(false)),
        ("4294967296", Ok(true)),
        ("4294967298", Ok(false)),
        ("9223372036854775808.0", Err(Unsupported::Number)),
        ("9223372036854775808", Err(Unsupported::Number)),
    ] {
        assert_eq!(square(flags), kept, "/F {flags}");
    }
    // The interpreter draws the square kept.
    let drawn = draw(
        &["/Subtype /Square /F 16777217.0 /IC [1 0 0] /Rect [20 20 180 180]"],
        VIEW,
    );
    assert!(
        drawn
            .iter()
            .any(|e| matches!(e, Ev::Fill { rgba: RED, .. })),
        "{drawn:?}"
    );
}

/// Viewers read a stream as text only where a field's value is: a `FreeText` draws
/// its `/Contents` string whatever streams its `/RC`, `/DS` and `/DA` are, and draws
/// no stream `/Contents`; a text field draws its stream `/V`.
#[test]
fn only_a_field_value_is_read_from_a_stream() {
    let stream = |data: &str| common::stream("", data.as_bytes());
    let plain = content(
        "/Subtype /FreeText /Rect [20 20 180 180] /Contents (WORLD)",
        vec![],
    );
    for key in ["RC", "DS", "DA"] {
        let with = content(
            &format!("/Subtype /FreeText /Rect [20 20 180 180] /Contents (WORLD) /{key} 5 0 R"),
            vec![stream("<p>HELLO</p>")],
        );
        assert_eq!(with, plain, "/{key}");
    }
    let streamed = content(
        "/Subtype /FreeText /Rect [20 20 180 180] /Contents 5 0 R",
        vec![stream("WORLD")],
    );
    let none = content("/Subtype /FreeText /Rect [20 20 180 180]", vec![]);
    assert_eq!(streamed, none);
    let field = "/Subtype /Widget /FT /Tx /DA (/Helv 12 Tf 0 g) /Rect [20 20 180 60]";
    assert_eq!(
        content(&format!("{field} /V 5 0 R"), vec![stream("WORLD")]),
        content(&format!("{field} /V (WORLD)"), vec![])
    );
}

/// A text field's own rich value `/RV` is what viewers draw, whatever its `/V` and
/// its `RichText` flag; one on a field above the widget is not (observed in `PyMuPDF`
/// 1.28.2).
#[test]
fn a_widget_draws_its_own_rich_value() {
    let rich = "/RV (<body><p style=\"font-size:12pt\">RICH</p></body>)";
    let widget = |own: &str, above: &str| {
        content(
            &format!(
                "/Subtype /Widget /Parent 5 0 R /DA (/Helv 12 Tf 0 g) /Rect [20 20 180 180] {own}"
            ),
            vec![format!("<< /FT /Tx {above} >>").into_bytes()],
        )
    };
    let shown = widget(&format!("/Ff 33554432 /V (PLAIN) {rich}"), "");
    assert_eq!(
        widget(&format!("/Ff 33554432 /V (OTHER) {rich}"), ""),
        shown
    );
    assert_eq!(widget(&format!("/V (PLAIN) {rich}"), ""), shown);
    assert_ne!(widget("/Ff 33554432 /V (PLAIN)", ""), shown);
    assert_eq!(
        widget("/V (PLAIN)", &format!("/Ff 33554432 {rich}")),
        widget("/V (PLAIN)", "/Ff 33554432")
    );
}

/// Square and Circle are drawn inside their rectangle differences, as given or as
/// a damaged one reads (observed: `/Rect [20 20 180 180] /RD [20 30 40 50]` bakes
/// `40 50 100 80 re`).
#[test]
fn square_and_circle_are_drawn_inside_their_rectangle_differences() {
    let square = |rd: &str| {
        content(
            &format!("/Subtype /Square /C [0] /IC [1 0 0] /Rect [20 20 180 180] /RD {rd}"),
            vec![],
        )
    };
    assert!(square("[20 30 40 50]").contains("40 50 100 80 re\nb\n"));
    // A missing or damaged difference reads 0; a side is never less than half the
    // border width in.
    assert!(square("[20 /X 40]").contains("40 20.5 100 159 re\nb\n"));
    let circle = content(
        "/Subtype /Circle /C [0] /IC [1 0 0] /Rect [20 20 180 180] /RD [20 30 40 50]",
        vec![],
    );
    let xs: Vec<f64> = circle
        .lines()
        .filter(|l| l.ends_with(" c") || l.ends_with(" m"))
        .flat_map(|l| {
            l.split(' ')
                .step_by(2)
                .filter_map(|n| n.parse().ok())
                .collect::<Vec<_>>()
        })
        .collect();
    let (min, max) = xs
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
    assert_eq!((min, max), (40.0, 140.0), "{circle}");
}

/// A dash array keeps all its entries, however many (`PyMuPDF` 1.28.2 bakes all
/// twelve).
#[test]
fn a_dash_array_keeps_all_its_entries() {
    let line = content(
        "/Subtype /Line /L [20 50 220 50] /C [0] /Rect [0 0 200 200] \
         /BS << /W 2 /S /D /D [1 2 3 4 5 6 7 8 9 10 30 2] >>",
        vec![],
    );
    assert!(
        line.contains("[1 2 3 4 5 6 7 8 9 10 30 2 ] 0 d\n"),
        "{line}"
    );
}

/// The points of the glyphs in content `c`: those of the path operators after its
/// last clip.
fn glyph_points(c: &str) -> Vec<(f64, f64)> {
    let text = c.rsplit("W n\n").next().unwrap();
    text.lines()
        .filter(|l| [" m", " l", " c"].iter().any(|op| l.ends_with(op)))
        .flat_map(|l| {
            let n: Vec<f64> = l.split(' ').filter_map(|n| n.parse().ok()).collect();
            n.as_chunks::<2>()
                .0
                .iter()
                .map(|[x, y]| (*x, *y))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The y of the glyphs' points in content `c` ([`glyph_points`]).
fn glyph_ys(c: &str) -> Vec<f64> {
    glyph_points(c).into_iter().map(|(_, y)| y).collect()
}

/// A comb field without a usable `/MaxLen` writes its text on one line aligned by
/// `/Q` in the field's whole width, without padding, at an automatic size as a comb
/// does (observed: `A1B2C3` at 14 pt in a field 160 wide starts at 0, 53.932 and
/// 107.864; at the automatic size, 40, at 11.04 aligned right).
#[test]
fn a_comb_without_max_len_is_aligned_by_q() {
    let left = |q: u8, size: u8| {
        let c = content(
            &format!(
                "/Subtype /Widget /FT /Tx /Ff 16777216 /MaxLen (1) /Q {q} /V (A1B2C3) \
                 /DA (/Helv {size} Tf 0 g) /Rect [20 80 180 120] /BS << /W 3 >>"
            ),
            vec![],
        );
        glyph_points(&c)
            .into_iter()
            .map(|(x, _)| x)
            .fold(f64::INFINITY, f64::min)
    };
    let near = |got: f64, want: f64| assert!((got - want).abs() < 0.01, "{got} != {want}");
    let start = left(0, 14);
    assert!((0.0..1.0).contains(&start), "{start}");
    near(left(1, 14) - start, 53.932);
    near(left(2, 14) - start, 107.864);
    near(left(2, 0) - left(0, 0), 160.0 - 3.724 * 40.0);
}

/// A multiline field lays its first line out below its top padding, but its
/// baseline at least a fifth of the size above the bottom padding, however low
/// the field (observed: /Helv 14 Tf at 4.8 in a field 20 high, 8.8 with a border
/// 3 wide).
#[test]
fn a_low_multiline_field_keeps_its_first_line_up() {
    let baseline = |height: u32, border: &str| {
        let c = content(
            &format!(
                "/Subtype /Widget /FT /Tx /Ff 4096 /V (I) /DA (/Helv 14 Tf 0 g) \
                 /Rect [20 20 200 {}] {border}",
                20 + height
            ),
            vec![],
        );
        glyph_ys(&c).into_iter().fold(f64::INFINITY, f64::min)
    };
    let near = |got: f64, want: f64| assert!((got - want).abs() < 0.01, "{got} != {want}");
    near(baseline(40, ""), 40.0 - 2.0 - 15.624);
    near(baseline(20, ""), 4.8);
    near(baseline(8, ""), 4.8);
    near(baseline(20, "/BS << /W 3 >>"), 8.8);
}

/// A negative default-appearance size is drawn as given, upside down and mirrored,
/// not as an automatic or positive size: a `FreeText`'s text then lies above its
/// first line, outside its box, as when baked (`PyMuPDF` 1.28.2 draws none of it).
#[test]
fn a_negative_text_size_is_drawn_as_given() {
    let free_text = |da: &str| {
        content(
            &format!("/Subtype /FreeText /Rect [20 20 180 180] /Contents (WORLD) /DA ({da})"),
            vec![],
        )
    };
    let ys = glyph_ys;
    let up = ys(&free_text("/Helv 12 Tf 0 g"));
    assert!(up.iter().all(|y| (160.0..180.0).contains(y)), "{up:?}");
    // Below a baseline at 189.2, upside down: what reaches into the box is clipped.
    let down = ys(&free_text("/Helv -12 Tf 0 g"));
    assert!(down.iter().all(|y| *y > 175.0), "{down:?}");
    // A text field draws it mirrored about its baseline, not fit to the field.
    let field = |da: &str| {
        content(
            &format!("/Subtype /Widget /FT /Tx /V (WORLD) /Rect [20 20 180 60] /DA ({da})"),
            vec![],
        )
    };
    assert_ne!(field("/Helv -12 Tf 0 g"), field("/Helv 0 Tf 0 g"));
    assert_ne!(field("/Helv -12 Tf 0 g"), field("/Helv 12 Tf 0 g"));
}
