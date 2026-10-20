//! A soft mask drawn once for several uses draws each as drawing it for that use
//! alone does: a page that sets one mask under two clips renders as the page that
//! sets two copies of it, in either order, whatever the clips, the group's box and
//! the shading the group paints.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};

#[path = "../../hayro-syntax/tests/common/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;

/// A deterministic source of numbers for the generated cases.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number in `[lo, hi)` with two decimals, most of them fractional.
    fn between(&mut self, lo: f64, hi: f64) -> f64 {
        let unit = (self.next() % 10_000) as f64 / 10_000.0;
        ((lo + unit * (hi - lo)) * 100.0).round() / 100.0
    }
}

/// What a mask's group paints: its shading `/Sh`, the code of the Type 4 function,
/// object 7, which the shading may name, and its content.
struct Group {
    shading: String,
    function: String,
    content: String,
}

/// A function shading of object 7 over the group's whole box.
fn function_shading(function: String) -> Group {
    Group {
        shading: "<< /ShadingType 1 /ColorSpace /DeviceGray /Domain [0 100 0 100] \
                  /Function 7 0 R >>"
            .into(),
        function,
        content: "/Sh sh".into(),
    }
}

fn group(rng: &mut Rng, kind: u64) -> Group {
    let ramp = |rng: &mut Rng| {
        let (a, b) = (rng.between(0.0, 0.5), rng.between(0.5, 1.0));
        format!("<< /FunctionType 2 /Domain [0 1] /C0 [{a}] /C1 [{b}] /N 1 >>")
    };
    match kind {
        // Steps at a fractional x.
        0 => function_shading(format!(
            "{{ pop {} lt {{ 0 }} {{ 1 }} ifelse }}",
            rng.between(20.0, 80.0)
        )),
        // Saws with a fractional period.
        1 => function_shading(format!(
            "{{ pop {} div dup floor sub }}",
            rng.between(1.5, 9.0)
        )),
        // An axial shading of two ramps that jump at a fractional bound.
        2 => {
            let (x0, x1) = (rng.between(0.0, 30.0), rng.between(70.0, 100.0));
            let bound = rng.between(0.2, 0.8);
            Group {
                shading: format!(
                    "<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [{x0} 0 {x1} 0] \
                     /Extend [true true] /Function << /FunctionType 3 /Domain [0 1] \
                     /Functions [{} {}] /Bounds [{bound}] /Encode [0 1 0 1] >> >>",
                    ramp(rng),
                    ramp(rng)
                ),
                function: "{ pop pop 0 }".into(),
                content: "/Sh sh".into(),
            }
        }
        // A rectangle at fractional coordinates, which reads no clip.
        _ => {
            let (x, y) = (rng.between(0.0, 50.0), rng.between(0.0, 50.0));
            let (w, h) = (rng.between(10.0, 50.0), rng.between(10.0, 50.0));
            Group {
                shading: "<< /ShadingType 1 /ColorSpace /DeviceGray /Function 7 0 R >>".into(),
                function: "{ pop pop 0 }".into(),
                content: format!("1 g {x} {y} {w} {h} re f"),
            }
        }
    }
}

/// The page that sets `/A`, a luminosity mask of object 5 whose group has the box
/// `group_box` and paints `group`, under `first` and then under `second`, the clips'
/// rectangles as `re` takes them, filling the page blue each time; with `copies`,
/// the second use sets `/B`, a mask of object 6, which draws as object 5 does.
fn file(group: &Group, group_box: [f64; 4], [first, second]: [&str; 2], copies: bool) -> Vec<u8> {
    let content = format!(
        "q {first} re W n /A gs 0 0 1 rg 0 0 100 100 re f Q \
         q {second} re W n /{} gs 0 0 1 rg 0 0 100 100 re f Q",
        if copies { "B" } else { "A" }
    );
    let [x0, y0, x1, y1] = group_box;
    let form = fixtures::stream(
        &format!(
            "/Type /XObject /Subtype /Form /Group << /S /Transparency /CS /DeviceGray >> \
             /BBox [{x0} {y0} {x1} {y1}] /Resources << /Shading << /Sh {} >> >>",
            group.shading
        ),
        group.content.as_bytes(),
    );
    fixtures::one_page(
        100,
        "/Contents 4 0 R /Resources << /ExtGState << \
         /A << /SMask << /S /Luminosity /G 5 0 R >> >> \
         /B << /SMask << /S /Luminosity /G 6 0 R >> >> >> >>",
        vec![
            fixtures::stream("", content.as_bytes()),
            form.clone(),
            form,
            fixtures::stream(
                "/FunctionType 4 /Domain [0 100 0 100] /Range [0 1]",
                group.function.as_bytes(),
            ),
        ],
    )
}

fn rendered(file: Vec<u8>, scale: f32) -> Vec<u8> {
    let pdf = Pdf::new(file).expect("valid pdf");
    render(
        &pdf.pages()[0],
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
        },
    )
    .data_as_u8_slice()
    .to_vec()
}

/// The pixels that differ between the page that sets one mask under both clips and
/// the page that sets a copy of it under the second, in both orders of the clips.
fn differing(group: &Group, group_box: [f64; 4], clips: [&str; 2], scale: f32) -> [usize; 2] {
    [clips, [clips[1], clips[0]]].map(|clips| {
        let shared = rendered(file(group, group_box, clips, false), scale);
        let copies = rendered(file(group, group_box, clips, true), scale);
        assert_eq!(shared.len(), copies.len());
        shared
            .as_chunks::<4>()
            .0
            .iter()
            .zip(copies.as_chunks::<4>().0)
            .filter(|(a, b)| a != b)
            .count()
    })
}

/// Two clips that leave the same part of the group's box and sample a shading from
/// corners a fraction of a pixel apart.
#[test]
fn a_mask_under_clips_that_leave_one_part_of_its_box_draws_each_as_its_copy_does() {
    let group = function_shading("{ pop .1 mul dup floor sub }".into());
    let clips = [".1 .1 80 80", "20.7 20.7 79.3 79.3"];
    assert_eq!(
        differing(&group, [30.0, 30.0, 70.0, 70.0], clips, 1.0),
        [0, 0]
    );
}

#[test]
fn a_mask_drawn_once_draws_each_use_as_its_own_copy_does() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut differ = vec![];
    for case in 0..64 {
        let kind = case % 4;
        let scale = [1.0, 1.37][(case / 4 % 2) as usize];
        let group = group(&mut rng, kind);
        let group_box = [
            rng.between(0.0, 40.0),
            rng.between(0.0, 40.0),
            rng.between(60.0, 100.0),
            rng.between(60.0, 100.0),
        ];
        // Each side of the first clip inside the group's box or beyond it; the
        // second clip has the first's sides inside the box and others beyond it, so
        // that both leave the same part of the box, but in every fourth case, where
        // it is drawn anew.
        let side = |rng: &mut Rng, i: usize, inside: bool| {
            let outward = [-1.0, -1.0, 1.0, 1.0][i];
            if inside {
                group_box[i] - outward * rng.between(0.1, 15.0)
            } else {
                group_box[i] + outward * rng.between(0.05, 25.0)
            }
        };
        let inside: [bool; 4] = std::array::from_fn(|_| rng.next().is_multiple_of(2));
        let first: [f64; 4] = std::array::from_fn(|i| side(&mut rng, i, inside[i]));
        let anew = rng.next().is_multiple_of(4);
        let second: [f64; 4] = std::array::from_fn(|i| {
            if !anew && inside[i] {
                first[i]
            } else {
                let inside = rng.next().is_multiple_of(2);
                side(&mut rng, i, inside)
            }
        });
        let rect = |[x0, y0, x1, y1]: [f64; 4]| format!("{x0} {y0} {} {}", x1 - x0, y1 - y0);
        let (first, second) = (rect(first), rect(second));
        let pixels = differing(&group, group_box, [&first, &second], scale);
        if pixels != [0, 0] {
            differ.push((case, kind, pixels));
        }
    }
    assert_eq!(
        differ,
        vec![],
        "(case, kind, pixels that differ in each order)"
    );
}
