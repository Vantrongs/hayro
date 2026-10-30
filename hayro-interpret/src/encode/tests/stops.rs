//! The stops of axial and radial shadings: where they are, what they cost, and
//! whether a renderer's table of their colors paints them.

use super::{Recording, shading_pattern, shading_patterns};
use crate::encode::{EncodedShadingPattern, EncodedShadingType, Exhausted};
use crate::gradient::{SvgGradient, SvgGradientKind, SvgGradientStop};
use kurbo::{Affine, Point, Rect};
use smallvec::smallvec;

fn axial(coords: &str, color_space: &str, function: &str) -> EncodedShadingPattern {
    shading_pattern(&[
        "<< /PatternType 2 /Shading 5 0 R >>".to_string(),
        format!(
            "<< /ShadingType 2 /ColorSpace {color_space} /Coords [{coords}] /Extend [true true] \
             /Function 6 0 R >>"
        ),
        function.to_string(),
    ])
    .encode()
}

fn stops(encoded: &EncodedShadingPattern, bbox: Rect) -> Vec<SvgGradientStop> {
    let EncodedShadingType::RadialAxial(shading) = &encoded.shading_type else {
        unreachable!("an axial shading");
    };
    shading
        .as_svg_gradient(encoded, bbox)
        .expect("an axial shading within its extension")
        .stops
}

const STEPS: &str = "<< /FunctionType 4 /Domain [0 1] /Range [0 1] /Length 31 >>\nstream\n\
                     { 0.5 lt { 0.2 } { 0.7 } ifelse }\nendstream";

/// A stitching steps at its bound in two stops at one place, the colors from
/// below it and from above, exactly; between its bounds its linear parts are
/// a stop at each end, and nothing between.
#[test]
fn a_stitching_steps_in_two_stops_at_its_bound() {
    let encoded = axial(
        "0 0 1000 0",
        "/DeviceGray",
        "<< /FunctionType 3 /Domain [0 1] /Bounds [0.4] /Encode [0 1 0 1] /Functions [\
         << /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [0.25] /N 1 >> \
         << /FunctionType 2 /Domain [0 1] /C0 [0.75] /C1 [1] /N 1 >>] >>",
    );
    let EncodedShadingType::RadialAxial(shading) = &encoded.shading_type else {
        unreachable!("an axial shading");
    };
    let at = |t: f32| shading.function.eval(&smallvec![t]).expect("a value")[0];
    let stops = stops(&encoded, Rect::new(0.0, 0.0, 1000.0, 1.0));
    let gray: Vec<(f32, f32)> = stops.iter().map(|s| (s.offset, s.color[0])).collect();
    // The ends are what the function evaluates to there.
    assert_eq!(
        gray,
        [(0.0, at(0.0)), (0.4, 0.25), (0.4, 0.75), (1.0, at(1.0))],
        "{stops:?}"
    );
}

/// Where the colors are tabulated, as a PostScript function's are, the table
/// holds a stop for each step of the parameter a pixel takes, and those the
/// step needs besides: it grows with the pixels the shading's line spans, four
/// times as many for a line four times as long, not with the pixels painted, as
/// many for a box a row tall as for one a hundred rows tall. Its work is taken
/// before it is done, and a meter that refuses it gets no stops.
#[test]
fn a_table_grows_with_the_pixels_its_line_spans() {
    let count = |width: f64, height: f64| {
        let encoded = axial(&format!("0 0 {width} 0"), "/DeviceGray", STEPS);
        stops(&encoded, Rect::new(0.0, 0.0, width, height)).len()
    };
    let (short, long) = (count(256.0, 1.0), count(1024.0, 1.0));
    assert!(short > 256 && short < 256 + 64, "{short}");
    assert!(long > 4 * 256 && long < 4 * 256 + 64, "{long}");
    assert_eq!(count(256.0, 100.0), short);

    let encoded = axial("0 0 256 0", "/DeviceGray", STEPS);
    let EncodedShadingType::RadialAxial(shading) = &encoded.shading_type else {
        unreachable!("an axial shading");
    };
    let bbox = Rect::new(0.0, 0.0, 256.0, 1.0);
    let mut meter = Recording {
        left: u64::MAX,
        asked: vec![],
        held: vec![],
    };
    shading
        .as_svg_gradient_metered(&encoded, bbox, &mut meter)
        .expect("given all it asks");
    let asked: u64 = meter.asked.iter().sum();
    let held: usize = meter.held.iter().sum();
    assert!(held >= short * size_of::<SvgGradientStop>(), "{held}");
    let encoded = axial("0 0 256 0", "/DeviceGray", STEPS);
    let EncodedShadingType::RadialAxial(shading) = &encoded.shading_type else {
        unreachable!("an axial shading");
    };
    let mut meter = Recording {
        left: asked - 1,
        asked: vec![],
        held: vec![],
    };
    assert!(matches!(
        shading.as_svg_gradient_metered(&encoded, bbox, &mut meter),
        Err(Exhausted)
    ));
}

/// The table of a gradient drawn as vello draws it: `n` premultiplied 8-bit
/// colors, the `i`th the stops' at `i / (n - 1)`.
fn table(stops: &[SvgGradientStop], n: usize) -> Vec<[u8; 4]> {
    (0..n)
        .map(|i| {
            let c = crate::gradient::color_at(stops, i as f32 / (n - 1) as f32);
            [c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3]].map(|v| (v * 255.0 + 0.5) as u8)
        })
        .collect()
}

fn gradient(stops: Vec<SvgGradientStop>, pixel_step: f32) -> SvgGradient {
    SvgGradient {
        kind: SvgGradientKind::Linear {
            start: Point::ZERO,
            end: Point::new(1.0, 0.0),
        },
        transform: Affine::IDENTITY,
        stops,
        range: (0.0, 1.0),
        pixel_step,
        table_steps: Vec::new(),
    }
}

/// A table paints a gradient within a level where each entry is its stops'
/// color at the entry's place, and a step of the stops falls within an entry
/// no longer than a pixel's step; a step within a longer entry, a curve the
/// entries cut across, or an entry off by two levels, does not.
#[test]
fn a_table_paints_within_a_level_only_where_it_is_fine_enough() {
    let stop = |offset: f32, gray: f32| SvgGradientStop {
        offset,
        color: [gray, gray, gray, 1.0],
    };
    let ramp = vec![stop(0.0, 0.0), stop(1.0, 1.0)];
    assert!(gradient(ramp.clone(), 1.0 / 300.0).paints_within_a_level(&table(&ramp, 256)));
    let mut off = table(&ramp, 256);
    off[100][0] += 2;
    assert!(!gradient(ramp, 1.0 / 300.0).paints_within_a_level(&off));

    // A step at 0.3, within the entry from 76/255 to 77/255.
    let step = vec![
        stop(0.0, 0.0),
        stop(0.3, 0.2),
        stop(0.3, 0.8),
        stop(1.0, 1.0),
    ];
    let lut = table(&step, 256);
    assert!(gradient(step.clone(), 1.0 / 200.0).paints_within_a_level(&lut));
    assert!(!gradient(step, 1.0 / 1000.0).paints_within_a_level(&lut));

    // A rise of the color narrower than refining resolves, a millionth wide,
    // is a step; one a pixel's step wide is a slope an entry must follow.
    let rise = |width: f32| {
        vec![
            stop(0.0, 0.0),
            stop(0.3, 0.2),
            stop(0.3 + width, 0.8),
            stop(1.0, 1.0),
        ]
    };
    let lut = table(&rise(1e-6), 256);
    assert!(gradient(rise(1e-6), 1.0 / 200.0).paints_within_a_level(&lut));
    let lut = table(&rise(1.0 / 200.0), 256);
    assert!(!gradient(rise(1.0 / 200.0), 1.0 / 200.0).paints_within_a_level(&lut));

    // Such a rise across the end of an entry, the entry from 75/255 to 76/255
    // read along it, is a step too.
    let across = vec![
        stop(0.0, 0.0),
        stop(0.2980, 0.2),
        stop(0.29806, 0.8),
        stop(1.0, 1.0),
    ];
    let lut = table(&across, 256);
    assert!(gradient(across, 1.0 / 200.0).paints_within_a_level(&lut));

    // A rise of two levels within an entry, where hayro's color tables step by
    // two levels, as the shading drawn directly may be off by.
    let stair = vec![
        stop(0.0, 0.0),
        stop(0.3, 0.4),
        stop(0.3 + 0.5 / 255.0, 0.4 + 2.0 / 255.0),
        stop(1.0, 1.0),
    ];
    let lut = table(&stair, 256);
    assert!(!gradient(stair.clone(), 1.0 / 200.0).paints_within_a_level(&lut));
    let mut tables = gradient(stair, 1.0 / 200.0);
    tables.table_steps = vec![0.0, 2.0, 2.0, 0.0];
    assert!(tables.paints_within_a_level(&lut));

    // Stops a pixel apart along a curve, read from 256 entries for 1000 pixels.
    let curve: Vec<SvgGradientStop> = (0..=1000)
        .map(|i| {
            let t = i as f32 / 1000.0;
            stop(t, (t * 6.0).sin() * 0.5 + 0.5)
        })
        .collect();
    assert!(!gradient(curve.clone(), 1.0 / 1000.0).paints_within_a_level(&table(&curve, 256)));
    assert!(gradient(curve.clone(), 1.0 / 1000.0).paints_within_a_level(&table(&curve, 4096)));
}

/// The step of the grid is at most the parameter's least change over a pixel:
/// an axial line 300 pixels long changes by 1/300 a pixel, so 1/512; drawn at
/// two pixels a unit, by half that.
#[test]
fn the_grid_steps_no_more_than_a_pixel() {
    let encoded = axial("0 0 300 0", "/DeviceGray", STEPS);
    let EncodedShadingType::RadialAxial(shading) = &encoded.shading_type else {
        unreachable!("an axial shading");
    };
    assert_eq!(shading.grid_step(&encoded, 1.0), 1.0 / 512.0);
    assert_eq!(shading.grid_step(&encoded, 2.0), 1.0 / 1024.0);
}

/// A shading a page paints in many shapes finds each color once: read again
/// where it paints a second shape, over part of the first's, its stops there
/// evaluate its function nowhere, take less work, and are the first's.
#[test]
fn a_shading_painted_again_finds_its_colors_found() {
    use crate::function::TYPE0_EVALS;
    let objects = [
        "<< /PatternType 2 /Shading 5 0 R >>".to_string(),
        "<< /ShadingType 2 /ColorSpace /DeviceCMYK /Coords [0 0 300 0] /Extend [true true] \
         /Function 6 0 R >>"
            .to_string(),
        "<< /FunctionType 0 /Domain [0 1] /Range [0 1 0 1 0 1 0 1] /Size [3] \
         /BitsPerSample 8 /Filter /ASCIIHexDecode /Length 27 >>\nstream\n\
         FF000000 00FF0000 0000FF00>\nendstream"
            .to_string(),
    ];
    let mut paints = Vec::new();
    for (pattern, end) in shading_patterns(&objects, 2).into_iter().zip([1.0, 0.4]) {
        let encoded = pattern.encode();
        let EncodedShadingType::RadialAxial(shading) = &encoded.shading_type else {
            unreachable!("an axial shading");
        };
        let mut meter = Recording {
            left: u64::MAX,
            asked: vec![],
            held: vec![],
        };
        TYPE0_EVALS.set(0);
        let stops = shading
            .stops_metered(&encoded, (0.0, end), 1.0, &mut meter)
            .expect("given all it asks");
        paints.push((stops, TYPE0_EVALS.get(), meter.asked.iter().sum::<u64>()));
    }
    let [
        (first, first_evals, first_asked),
        (second, second_evals, second_asked),
    ] = &paints[..]
    else {
        unreachable!("two paints");
    };
    assert!(*first_evals > 0);
    assert_eq!(*second_evals, 0);
    assert!(
        second_asked < first_asked,
        "{second_asked} of {first_asked}"
    );
    assert!(second.len() > 2 && second.len() < first.len());
    for stop in second {
        assert!(first.contains(stop), "{stop:?}");
    }
}

/// A gradient made for a shape that reaches a part of the shading runs over
/// that part alone, so a renderer's table of as many colors paints it finer: a
/// wave of thirty periods along 4000 pixels, a table's 4096 entries step by six
/// levels and more across it, but within a level across a twentieth of it.
#[test]
fn a_gradient_spans_the_part_its_shape_reaches() {
    let function = "<< /FunctionType 4 /Domain [0 1] /Range [0 1] /Length 33 >>\nstream\n\
                    { 10800 mul sin 0.5 mul 0.5 add }\nendstream";
    let encoded = axial("0 0 4000 0", "/DeviceGray", function);
    let EncodedShadingType::RadialAxial(shading) = &encoded.shading_type else {
        unreachable!("an axial shading");
    };
    let fits = |bbox: Rect| {
        let gradient = shading
            .as_svg_gradient(&encoded, bbox)
            .expect("an axial shading within its extension");
        assert_eq!(gradient.stops[0].offset, 0.0);
        assert_eq!(gradient.stops[gradient.stops.len() - 1].offset, 1.0);
        gradient.paints_within_a_level(&table(&gradient.stops, 4096))
    };
    assert!(!fits(Rect::new(0.0, 0.0, 4000.0, 1.0)));
    assert!(fits(Rect::new(1000.0, 0.0, 1200.0, 1.0)));
}

/// In a color space hayro reads through color tables, the stops where the
/// color changes from a neighbour's by more than a level and a half carry the
/// table step of the function's outputs there, as the shading is checked
/// against; in gray and RGB, none.
#[test]
fn stops_where_tables_step_carry_the_step() {
    use crate::gradient::{LEVEL_TOLERANCE, levels_apart, table_step};
    let lab = "[/Lab << /WhitePoint [0.9505 1 1.089] /Range [-128 127 -128 127] >>]";
    let function = "<< /FunctionType 2 /Domain [0 1] /C0 [0 -100 -100] /C1 [100 100 100] /N 1 >>";
    let encoded = axial("0 0 300 0", lab, function);
    let EncodedShadingType::RadialAxial(shading) = &encoded.shading_type else {
        unreachable!("an axial shading");
    };
    let gradient = shading
        .as_svg_gradient(&encoded, Rect::new(0.0, 0.0, 300.0, 1.0))
        .expect("an axial shading within its extension");
    let stops = &gradient.stops;
    assert_eq!(gradient.table_steps.len(), stops.len());
    let mut stepped = 0;
    for (i, step) in gradient.table_steps.iter().enumerate() {
        let jumps = [i.checked_sub(1), Some(i + 1)]
            .into_iter()
            .flatten()
            .any(|j| {
                stops
                    .get(j)
                    .is_some_and(|n| levels_apart(&n.color, &stops[i].color) > LEVEL_TOLERANCE)
            });
        let x = stops[i].offset;
        let want = table_step(
            &encoded,
            &shading.function.eval(&smallvec![x]).expect("a value"),
        );
        assert_eq!(*step, if jumps { want } else { 0.0 }, "stop {i} at {x}");
        stepped += usize::from(jumps && want > 0.0);
    }
    assert!(stepped > 0, "{stops:?}");

    let gray = axial("0 0 300 0", "/DeviceGray", STEPS);
    let EncodedShadingType::RadialAxial(shading) = &gray.shading_type else {
        unreachable!("an axial shading");
    };
    let gradient = shading
        .as_svg_gradient(&gray, Rect::new(0.0, 0.0, 300.0, 1.0))
        .expect("an axial shading within its extension");
    assert!(gradient.table_steps.is_empty());
}
