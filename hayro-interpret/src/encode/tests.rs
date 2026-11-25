use super::*;
use crate::cache::Cache;
use hayro_syntax::{
    Pdf,
    object::{Dict, ObjectIdentifier},
};

struct Allow {
    steps: u64,
    bytes: usize,
}
impl Meter for Allow {
    fn scratch_mark(&self) -> usize {
        usize::MAX - self.bytes
    }
    fn release_scratch(&mut self, mark: usize) {
        self.bytes = usize::MAX - mark;
    }
    fn release_bytes(&mut self, bytes: usize) {
        self.bytes += bytes;
    }
    fn work(&mut self, cost: Cost) -> bool {
        self.steps.checked_sub(cost.steps()).is_some_and(|n| {
            self.steps = n;
            true
        })
    }
    fn hold(&mut self, bytes: usize) -> bool {
        self.bytes.checked_sub(bytes).is_some_and(|n| {
            self.bytes = n;
            true
        })
    }
}

fn pattern(shading: Vec<u8>, function: Vec<u8>) -> EncodedShadingPattern {
    let pdf = Pdf::new(crate::fixtures::one_page(
        100,
        "",
        vec![
            b"<< /PatternType 2 /Shading 5 0 R >>".to_vec(),
            shading,
            function,
        ],
    ))
    .unwrap();
    let dict = pdf
        .xref()
        .get::<Dict<'_>>(ObjectIdentifier::new(4, 0))
        .unwrap();
    ShadingPattern::new(&dict, &Cache::new()).unwrap().encode()
}

#[test]
fn calculator_samples_are_reserved_before_evaluation() {
    let p = pattern(
        b"<< /ShadingType 3 /Coords [0 0 0 0 0 100] /ColorSpace /DeviceGray /Function 6 0 R >>"
            .to_vec(),
        crate::fixtures::stream(
            "/FunctionType 4 /Domain [0 1] /Range [0 1]",
            b"{ 46080 mul sin abs }",
        ),
    );
    let area = Rect::new(0., 0., 100., 3.);
    let positions: Vec<_> = (0..3)
        .flat_map(|y| (0..100).map(move |x| Point::new(f64::from(x) + 0.5, f64::from(y) + 0.5)))
        .collect();
    let cost = p.point_cost().times(positions.len() as u64).steps();
    let mut meter = Allow {
        steps: cost - 1,
        bytes: 1600,
    };
    let before = crate::function::test_operator_count();
    assert!(
        p.sample_texture_metered(area, 1., &mut meter, |_| panic!("refused row"))
            .is_err()
    );
    assert!(
        p.sample_points_metered(&positions, &mut meter, |_| panic!("refused point"))
            .is_err()
    );
    assert_eq!(before, crate::function::test_operator_count());
    let mut rows = Vec::new();
    let mut meter = Allow {
        steps: cost,
        bytes: 1600,
    };
    p.sample_texture_metered(area, 1., &mut meter, |row| rows.extend_from_slice(row))
        .unwrap();
    assert!(crate::function::test_operator_count() > before);
    let mut points = Vec::new();
    p.sample_points_metered(&positions, &mut Unmetered, |c| points.push(c))
        .unwrap();
    assert_eq!(points, rows);
    assert!(rows.iter().any(|c| c[0] > 0.9));
}

#[test]
fn mesh_rows_hold_only_painted_spans_and_points_keep_subpixel_positions() {
    let p = pattern(
        crate::fixtures::stream(
            "/ShadingType 4 /ColorSpace /DeviceGray /BitsPerCoordinate 8 /BitsPerComponent 8 /BitsPerFlag 8 /Decode [0 255 0 255 0 1]",
            &[0, 0, 0, 255, 0, 2, 0, 255, 0, 0, 2, 255],
        ),
        b"null".to_vec(),
    );
    let mut meter = Allow {
        steps: 100_000,
        bytes: 4096,
    };
    let mut rows = Vec::new();
    p.sample_texture_metered(Rect::new(0., 0., 10., 1000.), 1., &mut meter, |r| {
        rows.extend_from_slice(r);
    })
    .unwrap();
    assert_eq!(rows[0], [1.; 4]);
    assert_eq!(rows[100], [0.; 4]);
    assert_eq!(meter.bytes, 4096);
    let pts = [
        Point::new(0.9, 1.2),
        Point::new(0.7, 1.2),
        Point::new(1e12, 1e12),
    ];
    let mut got = Vec::new();
    p.sample_points_metered(&pts, &mut Unmetered, |c| got.push(c))
        .unwrap();
    assert_eq!(got, [[0.; 4], [1.; 4], [0.; 4]]);
    let mut meter = Allow {
        steps: 5,
        bytes: usize::MAX,
    };
    assert!(
        p.sample_points_metered(&pts, &mut meter, |_| panic!("refused mesh"))
            .is_err()
    );
}

#[test]
fn extending_a_function_domain_applies_to_both_texture_interfaces() {
    use kurbo::Shape;
    let mut p = pattern(
        b"<< /ShadingType 1 /Domain [0 2 0 2] /ColorSpace /DeviceGray /Function 6 0 R >>".to_vec(),
        crate::fixtures::stream(
            "/FunctionType 4 /Domain [0 2 0 2] /Range [0 1]",
            b"{ pop pop 1 }",
        ),
    );
    let shape = Rect::new(0., 0., 2., 2.).to_path(0.1);
    let area = Rect::new(0., 0., 3., 3.);
    p.extend_within_metered(&shape, None, Affine::IDENTITY, area, 0.1, &mut Unmetered)
        .unwrap();
    assert_eq!(p.extended_edges(), 15);
    let texture = p.render_texture(area, 1.);
    assert!(texture.data.iter().all(|&n| n == 255));
}

#[test]
fn preparing_a_sampled_ramp_keeps_a_peak_between_native_table_entries() {
    let mut samples = [0; 4096];
    samples[1234] = 255;
    let mut p = pattern(
        b"<< /ShadingType 2 /Coords [0 0 4095 0] /Extend [true true] /ColorSpace /DeviceGray /Function 6 0 R >>".to_vec(),
        crate::fixtures::stream(
            "/FunctionType 0 /Domain [0 1] /Range [0 1] /Size [4096] /BitsPerSample 8",
            &samples,
        ),
    );
    let points: Vec<_> = (0..=16)
        .map(|i| Point::new(1233.0 + i as f64 / 8.0, 0.5))
        .collect();
    let mut expected = Vec::new();
    p.sample_points_metered(&points, &mut Unmetered, |c| expected.push(c))
        .unwrap();
    let mut refused = Allow {
        steps: 100,
        bytes: usize::MAX,
    };
    assert!(p.sample_ramp(&mut refused).is_err());
    assert_eq!(refused.bytes, usize::MAX);
    let mut meter = Allow {
        steps: u64::MAX,
        bytes: 32768,
    };
    let ramp = p.sample_ramp_for(1_000_000, &mut meter).unwrap().unwrap();
    let held = 32768 - meter.bytes;
    assert!(
        (1..samples.len() * size_of::<f32>()).contains(&held),
        "{held}"
    );
    assert!(meter.steps < u64::MAX);
    p.set_sample_ramp(Some(ramp));
    let EncodedShadingType::RadialAxial(s) = &p.shading_type else {
        unreachable!()
    };
    assert!(s.as_svg_gradient(&p, 0.01).is_none());
    let mut actual = Vec::new();
    p.sample_points_metered(&points, &mut Unmetered, |c| actual.push(c))
        .unwrap();
    assert_eq!(actual[8], [1.0; 4]);
    for (a, b) in actual.iter().zip(expected) {
        assert!(
            crate::color::color_error(*a, b) <= 1.0 / 255.0,
            "{a:?} {b:?}"
        );
    }
}

#[test]
fn a_sampled_color_that_saturates_between_knots_keeps_its_plateau() {
    let mut p = pattern(
        b"<< /ShadingType 2 /Coords [0 0 100 0] /ColorSpace /DeviceGray /Function 6 0 R >>"
            .to_vec(),
        crate::fixtures::stream(
            "/FunctionType 0 /Domain [0 1] /Range [0 10] /Size [2] /BitsPerSample 8",
            &[0, 255],
        ),
    );
    let at = [Point::new(25., 1.)];
    let mut direct = Vec::new();
    p.sample_points_metered(&at, &mut Unmetered, |c| direct.push(c))
        .unwrap();
    let ramp = p.sample_ramp(&mut Unmetered).unwrap();
    p.set_sample_ramp(ramp);
    let mut prepared = Vec::new();
    p.sample_points_metered(&at, &mut Unmetered, |c| prepared.push(c))
        .unwrap();
    assert_eq!(direct, [[1.; 4]]);
    assert_eq!(prepared, direct);
}

#[test]
fn zoomed_stitching_bound_uses_its_one_sided_colors() {
    let p = pattern(
        b"<< /ShadingType 2 /Coords [-39950 0 40050 0] /Extend [true true] /ColorSpace /DeviceGray /Function 6 0 R >>".to_vec(),
        b"<< /FunctionType 3 /Domain [0 1] /Bounds [.5] /Encode [0 1 0 1] /Functions [
          << /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [0] /N 1 >>
          << /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [1] /N 1 >> ] >>".to_vec(),
    );
    let EncodedShadingType::RadialAxial(s) = &p.shading_type else {
        unreachable!()
    };
    assert!(s.as_svg_gradient(&p, 0.01).is_none());
    let points = [Point::new(49.5, 0.), Point::new(50.5, 0.)];
    let mut values = Vec::new();
    p.sample_points_metered(&points, &mut Unmetered, |c| values.push(c[0]))
        .unwrap();
    assert_eq!(values, [0., 1.]);
}

#[test]
fn the_device_span_limits_only_sharp_native_ramps() {
    let step = "[/Separation /Ink /DeviceGray << /FunctionType 3 /Domain [0 1] /Bounds [.5] /Encode [0 1 0 1] /Functions [
        << /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [0] /N 1 >>
        << /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [1] /N 1 >> ] >>]";
    for (width, space, native) in [
        (4000, step, true),
        (80000, step, false),
        (80000, "/DeviceGray", true),
    ] {
        let mut p = pattern(format!("<< /ShadingType 2 /Coords [0 0 {width} 0] /Extend [true true] /ColorSpace {space} /Function 6 0 R >>").into_bytes(),
            crate::fixtures::stream("/FunctionType 0 /Domain [0 1] /Range [0 1] /Size [2] /BitsPerSample 8", &[0,255]));
        p.set_sample_ramp(p.sample_ramp(&mut Unmetered).unwrap());
        let EncodedShadingType::RadialAxial(s) = &p.shading_type else {
            unreachable!()
        };
        assert_eq!(
            s.as_svg_gradient(&p, 0.01).is_some(),
            native,
            "{width} {space}"
        );
    }
}

#[test]
fn a_clipped_stitch_keeps_its_endpoint_color_separate() {
    let mut p = pattern(b"<< /ShadingType 2 /Coords [0 0 8192 0] /Domain [0 .5] /ColorSpace /DeviceGray /Function 6 0 R >>".to_vec(),
        b"<< /FunctionType 3 /Domain [0 1] /Bounds [.5] /Encode [0 1 0 1] /Functions [ << /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [0] /N 1 >> << /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [1] /N 1 >> ] >>".to_vec());
    for prepared in [false, true] {
        if prepared {
            p.set_sample_ramp(p.sample_ramp(&mut Unmetered).unwrap());
        }
        let points = [8190.5, 8191.5, 8192.].map(|x| Point::new(x, 0.));
        let mut colors = Vec::new();
        p.sample_points_metered(&points, &mut Unmetered, |c| colors.push(c[0]))
            .unwrap();
        assert_eq!(colors, [0., 0., 1.]);
    }
}

#[test]
fn a_transfer_step_is_sampled_before_native_admission() {
    use crate::function::{Function, TransferFunction};
    use hayro_syntax::object::{FromBytes, Object};
    let mut p = pattern(b"<< /ShadingType 2 /Coords [0 0 80000 0] /Extend [true true] /ColorSpace /DeviceGray /Function 6 0 R >>".to_vec(),
        b"<< /FunctionType 2 /Domain [0 1] /N 1 >>".to_vec());
    let f=Object::from_bytes(b"<< /FunctionType 3 /Domain [0 1] /Bounds [.5] /Encode [0 1 0 1] /Functions [ << /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [0] /N 1 >> << /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [1] /N 1 >> ] >>").unwrap();
    p.transfer_function = Some(ActiveTransferFunction::Single(TransferFunction::new(
        Function::new(&f).unwrap(),
    )));
    p.set_sample_ramp(p.sample_ramp(&mut Unmetered).unwrap());
    let EncodedShadingType::RadialAxial(s) = &p.shading_type else {
        unreachable!()
    };
    assert!(s.as_svg_gradient(&p, 0.01).is_none());
    let mut colors = Vec::new();
    p.sample_points_metered(
        &[Point::new(39999.5, 0.), Point::new(40000.5, 0.)],
        &mut Unmetered,
        |c| colors.push(c[0]),
    )
    .unwrap();
    assert_eq!(colors, [0., 1.]);
}

#[test]
fn sampled_peak_survives_lattice_and_patch_interpolation() {
    let mut patch = vec![
        0, 0, 0, 0, 85, 0, 170, 0, 255, 85, 255, 170, 255, 255, 255, 255, 170, 255, 85, 255, 0,
        170, 0, 85, 0,
    ];
    patch.extend([0, 0, 255, 255]);
    for (kind, data) in [
        (
            "5 /VerticesPerRow 2",
            vec![0, 0, 0, 255, 0, 255, 0, 255, 0, 255, 255, 255],
        ),
        ("6 /BitsPerFlag 8", patch),
    ] {
        let mut p = pattern(
            crate::fixtures::stream(
                &format!(
                    "/ShadingType {kind} /BitsPerCoordinate 8 /BitsPerComponent 8 /Decode [0 128 0 16 0 1] /Function 6 0 R /ColorSpace /DeviceGray"
                ),
                &data,
            ),
            crate::fixtures::stream(
                "/FunctionType 0 /Domain [0 1] /Range [0 1] /Size [9] /BitsPerSample 8",
                &[0, 255, 0, 0, 0, 0, 0, 0, 0],
            ),
        );
        let area = Rect::new(-0.5, 0., 127.5, 16.);
        for prepared in [false, true] {
            if prepared {
                p.set_sample_ramp(p.sample_ramp(&mut Unmetered).unwrap());
            }
            let mut dense = Vec::new();
            p.sample_texture(area, 1., |c| dense.push(c));
            let mut rows = Vec::new();
            p.sample_texture_metered(area, 1., &mut Unmetered, |r| rows.extend_from_slice(r))
                .unwrap();
            let mut sample = Vec::new();
            p.sample_points_metered(&[Point::new(16., 8.5)], &mut Unmetered, |c| sample.push(c))
                .unwrap();
            for v in [dense[8 * 128 + 16], rows[8 * 128 + 16], sample[0]] {
                assert!(v[0] > 0.99, "{kind} {v:?}");
            }
        }
    }
}

#[test]
fn max_mesh_code_selects_the_upper_stitching_piece() {
    for (range, bound, domain) in [("0 41", "41", "42"), (".15 .7", ".7", "1")] {
        check_constant_mesh(
            "5 /VerticesPerRow 2",
            (16, 16),
            range,
            bound,
            domain,
            &[0, 0, 255, 255, 0, 255, 0, 255, 255, 255, 255, 255],
        );
    }
}

#[test]
fn constant_coons_patch_preserves_stitch_boundary() {
    check_constant_patch(6);
}

#[test]
fn constant_tensor_patch_preserves_stitch_boundary() {
    check_constant_patch(7);
}

fn check_constant_patch(kind: u8) {
    let mut data = vec![
        0, 0, 0, 0, 85, 0, 170, 0, 255, 85, 255, 170, 255, 255, 255, 255, 170, 255, 85, 255, 0,
        170, 0, 85, 0,
    ];
    if kind == 7 {
        data.extend([85, 85, 85, 170, 170, 170, 170, 85]);
    }
    data.extend([255; 4]);
    check_constant_mesh(
        &format!("{kind} /BitsPerFlag 8"),
        (128, 16),
        "0 .7",
        ".7",
        "1",
        &data,
    );
}

fn check_constant_mesh(
    kind: &str,
    (w, h): (u16, u16),
    range: &str,
    bound: &str,
    domain: &str,
    data: &[u8],
) {
    let mut p = pattern(
        crate::fixtures::stream(
            &format!("/ShadingType {kind} /BitsPerCoordinate 8 /BitsPerComponent 8 /Decode [0 {w} 0 {h} {range}] /Function 6 0 R /ColorSpace /DeviceGray"),
            data,
        ),
        format!("<< /FunctionType 3 /Domain [0 {domain}] /Bounds [{bound}] /Encode [0 1 0 1] /Functions [
          << /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [0] /N 1 >>
          << /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [1] /N 1 >> ] >>").into_bytes(),
    );
    let area = Rect::new(0., 0., w.into(), h.into());
    let white = |c| assert_eq!(c, [1.; 4]);
    for prepared in [false, true] {
        if prepared {
            let ramp = p.sample_ramp(&mut Unmetered).unwrap().unwrap();
            p.set_sample_ramp(Some(ramp));
        }
        p.sample_texture(area, 1., white);
        p.sample_texture_metered(area, 1., &mut Unmetered, |row| {
            row.iter().copied().for_each(white);
        })
        .unwrap();
        p.sample_points_metered(
            &[Point::new(3.25, 7.5), Point::new(12.1, 8.9)],
            &mut Unmetered,
            white,
        )
        .unwrap();
    }
}

fn direct_mesh(
    kind: u8,
    space: &str,
    range: &str,
    n: usize,
    function: Vec<u8>,
) -> EncodedShadingPattern {
    let flags = if kind == 5 {
        "/VerticesPerRow 2"
    } else {
        "/BitsPerFlag 8"
    };
    let mut data = Vec::new();
    if kind <= 5 {
        for (x, y) in [(0, 0), (255, 0), (0, 255), (255, 255)]
            .into_iter()
            .take(if kind == 4 { 3 } else { 4 })
        {
            if kind == 4 {
                data.push(0);
            }
            data.extend([x, y]);
            data.extend(std::iter::repeat_n(0, n - 1));
            data.push(x);
        }
    } else {
        data.extend([
            0, 0, 0, 0, 85, 0, 170, 0, 255, 85, 255, 170, 255, 255, 255, 255, 170, 255, 85, 255, 0,
            170, 0, 85, 0,
        ]);
        if kind == 7 {
            data.extend([85, 85, 85, 170, 170, 170, 170, 85]);
        }
        data.extend([0, 0, 255, 255]);
    }
    pattern(
        crate::fixtures::stream(
            &format!(
                "/ShadingType {kind} {flags} /ColorSpace {space} /BitsPerCoordinate 8 /BitsPerComponent 8 /Decode [0 5 0 5 {}]",
                format!(" {range}").repeat(n)
            ),
            &data,
        ),
        function,
    )
}

fn check_midpoint(p: &EncodedShadingPattern) {
    let expected = p.color_space.to_rgba(&[0.5], 1.).to_rgba8();
    for scale in [1., 3.] {
        let area = Rect::new(0., 0., 32., 16.);
        let mut rows = Vec::new();
        let (w, _, _) = p
            .sample_texture_metered(area, scale, &mut Unmetered, |r| rows.extend_from_slice(r))
            .unwrap();
        let mut dense = Vec::new();
        p.sample_texture(area, scale, |c| dense.push(c));
        let index = (0.5 * scale) as usize * w as usize + (2.5 * scale) as usize;
        let mut point = Vec::new();
        p.sample_points_metered(&[Point::new(2.5, 0.5)], &mut Unmetered, |c| point.push(c))
            .unwrap();
        for c in [rows[index], dense[index], point[0]] {
            let bytes = c.map(|v| (v * 255.).round() as u8);
            assert_eq!(bytes, expected, "scale {scale}");
        }
    }
}

macro_rules! calibrated_mesh_test {
    ($name:ident,$kind:literal) => {
        #[test]
        fn $name() {
            check_midpoint(&direct_mesh(
                $kind,
                "[/CalGray << /WhitePoint [1 1 1] >>]",
                "0 1",
                1,
                b"null".to_vec(),
            ));
        }
    };
}
calibrated_mesh_test!(calibrated_free_form_midpoint, 4);
calibrated_mesh_test!(calibrated_lattice_midpoint, 5);
calibrated_mesh_test!(calibrated_coons_midpoint, 6);
calibrated_mesh_test!(calibrated_tensor_midpoint, 7);

#[test]
fn direct_gray_clips_after_interpolation() {
    let p = direct_mesh(4, "/DeviceGray", "-1 1", 1, b"null".to_vec());
    p.sample_points_metered(&[Point::new(2.5, 0.5)], &mut Unmetered, |c| {
        assert_eq!(c, [0., 0., 0., 1.]);
    })
    .unwrap();
}

#[test]
fn five_ink_mesh_interpolates_every_component() {
    let p = direct_mesh(
        4,
        "[/DeviceN [/A /B /C /D /E] /DeviceGray 6 0 R]",
        "0 1",
        5,
        crate::fixtures::stream(
            "/FunctionType 4 /Domain [0 1 0 1 0 1 0 1 0 1] /Range [0 1]",
            b"{ 5 1 roll pop pop pop pop dup mul }",
        ),
    );
    let area = Rect::new(0., 0., 32., 16.);
    let mut meter = Allow {
        steps: u64::MAX,
        bytes: 4096,
    };
    let mut rows = Vec::new();
    p.sample_texture_metered(area, 1., &mut meter, |r| rows.extend_from_slice(r))
        .unwrap();
    assert_eq!(meter.bytes, 4096);
    let mut dense = Vec::new();
    p.sample_texture(area, 1., |c| dense.push(c));
    assert_eq!(rows, dense);
    assert_eq!(rows[2], p.convert(&[0., 0., 0., 0., 0.5]));
}

#[test]
fn sparse_five_ink_mesh() {
    for kind in [4, 6] {
        let mut data = Vec::new();
        if kind == 4 {
            for (x, y) in [(0, 0), (255, 252), (252, 252)] {
                data.extend([0, x, y, 0, 0, 0, 0, 255]);
            }
        } else {
            data.push(0);
            for (u, v) in [
                (0, 0),
                (0, 1),
                (0, 2),
                (0, 3),
                (1, 3),
                (2, 3),
                (3, 3),
                (3, 2),
                (3, 1),
                (3, 0),
                (2, 0),
                (1, 0),
            ] {
                data.extend([u + 84 * v, 84 * v]);
            }
            data.extend([0, 0, 0, 0, 255].repeat(4));
        }
        let p = pattern(
            crate::fixtures::stream(
                &format!(
                    "/ShadingType {kind} /BitsPerFlag 8 /ColorSpace [/DeviceN [/A /B /C /D /E] /DeviceGray 6 0 R] /BitsPerCoordinate 8 /BitsPerComponent 8 /Decode [0 3900 0 3900 {}]",
                    "0 1 ".repeat(5)
                ),
                &data,
            ),
            crate::fixtures::stream(
                "/FunctionType 4 /Domain [0 1 0 1 0 1 0 1 0 1] /Range [0 1]",
                b"{ 5 1 roll pop pop pop pop }",
            ),
        );
        let bytes = (256 << 20) - 3900 * 3900 * 4;
        let mut meter = Allow {
            steps: u64::MAX,
            bytes,
        };
        let mut painted = false;
        p.sample_texture_metered(Rect::new(0., 0., 3900., 3900.), 1., &mut meter, |row| {
            painted |= row.iter().any(|p| p[3] == 1.);
        })
        .unwrap();
        assert!(painted);
        assert_eq!(meter.bytes, bytes);
    }
}

#[test]
fn pan_invariant_spot_mesh() {
    for kind in [4, 5] {
        let data = if kind == 4 {
            vec![0, 0, 0, 0, 0, 255, 0, 255, 0, 0, 255, 0]
        } else {
            vec![0, 0, 0, 255, 0, 255, 0, 255, 0, 255, 255, 255]
        };
        let flags = if kind == 4 {
            "/BitsPerFlag 8"
        } else {
            "/VerticesPerRow 2"
        };
        // Overdraw makes the narrowest view spill its pending spans onto a dense canvas.
        for repetitions in [1, 48] {
            let p = pattern(
                crate::fixtures::stream(
                    &format!(
                        "/ShadingType {kind} {flags} /ColorSpace [/Separation /Spot /DeviceGray 6 0 R] /BitsPerCoordinate 8 /BitsPerComponent 8 /Decode [0 1800 0 10 .498 .502]"
                    ),
                    &data.repeat(repetitions),
                ),
                crate::fixtures::stream(
                    "/FunctionType 4 /Domain [0 1] /Range [0 1]",
                    b"{ .5 le { 0 } { 1 } ifelse }",
                ),
            );
            let points: Vec<_> = (880..920)
                .map(|x| Point::new(f64::from(x) + 0.5, 0.5))
                .collect();
            let mut expected = Vec::new();
            p.sample_points_metered(&points, &mut Unmetered, |c| expected.push(c))
                .unwrap();
            assert_eq!(expected.iter().position(|p| p[0] == 1.), Some(20));
            for start in [0, 600, 880] {
                let area = Rect::new(start as f64, 0., 920., 1.);
                let mut rows = Vec::new();
                p.sample_texture_metered(area, 1., &mut Unmetered, |r| rows.extend_from_slice(r))
                    .unwrap();
                let mut dense = Vec::new();
                p.sample_texture(area, 1., |c| dense.push(c));
                for actual in [rows, dense] {
                    assert_eq!(
                        actual[880 - start..],
                        expected,
                        "type {kind} origin {start}"
                    );
                }
            }
        }
    }
}

#[test]
fn small_bilinear_patch() {
    for kind in [6, 7] {
        for (space, n, function) in [
            ("/DeviceGray", 1, ""),
            ("/DeviceRGB", 3, ""),
            ("[/CalGray << /WhitePoint [1 1 1] >>]", 1, ""),
            ("/DeviceGray", 1, "/Function 6 0 R"),
        ] {
            let mut data = vec![
                0, 0, 0, 0, 85, 0, 170, 0, 255, 85, 255, 170, 255, 255, 255, 255, 170, 255, 85,
                255, 0, 170, 0, 85, 0,
            ];
            if kind == 7 {
                data.extend([85, 85, 85, 170, 170, 170, 170, 85]);
            }
            for c in [0, 0, 255, 0] {
                data.extend(std::iter::repeat_n(c, n));
            }
            let p = pattern(
                crate::fixtures::stream(
                    &format!(
                        "/ShadingType {kind} /BitsPerFlag 8 /ColorSpace {space} /BitsPerCoordinate 8 /BitsPerComponent 8 /Decode [0 7 0 7 {}] {function}",
                        "0 1 ".repeat(n)
                    ),
                    &data,
                ),
                b"<< /FunctionType 2 /Domain [0 1] /N 1 >>".to_vec(),
            );
            let area = Rect::new(0., 0., 7., 7.);
            let mut rows = Vec::new();
            p.sample_texture_metered(area, 1., &mut Unmetered, |r| rows.extend_from_slice(r))
                .unwrap();
            let mut dense = Vec::new();
            p.sample_texture(area, 1., |c| dense.push(c));
            let points: Vec<_> = (0..49)
                .map(|i| Point::new(f64::from(i % 7) + 0.5, f64::from(i / 7) + 0.5))
                .collect();
            let mut samples = Vec::new();
            p.sample_points_metered(&points, &mut Unmetered, |c| samples.push(c))
                .unwrap();
            for actual in [rows, dense, samples] {
                for (pt, c) in points.iter().zip(actual) {
                    let expected = p.convert(&[(pt.x * pt.y / 49.) as f32; 3][..n]);
                    for i in 0..4 {
                        assert!(
                            (expected[i] - c[i]).abs() <= 1. / 255. + 1e-6,
                            "type {kind} {space} {pt:?} {c:?} {expected:?}"
                        );
                    }
                }
            }
        }
    }
}

fn check_tint_threshold(space: &str, n: usize) {
    let program = if n == 1 {
        "{ .2 le { 0 } { 1 } ifelse }"
    } else {
        "{ pop .2 le { 0 } { 1 } ifelse }"
    };
    let p = direct_mesh(
        4,
        space,
        "0 1",
        n,
        crate::fixtures::stream(
            &format!(
                "/FunctionType 4 /Domain [{}] /Range [0 1]",
                "0 1 ".repeat(n)
            ),
            program.as_bytes(),
        ),
    );
    assert_eq!(
        [0.196, 0.2, 0.204].map(|t| p.color_space.to_rgba(&[t, t][..n], 1.).to_rgba8()[0]),
        [0, 0, 255]
    );
}
#[test]
fn separation_tint_threshold() {
    check_tint_threshold("[/Separation /Ink /DeviceGray 6 0 R]", 1);
}
#[test]
fn one_ink_tint_threshold() {
    check_tint_threshold("[/DeviceN [/A] /DeviceGray 6 0 R]", 1);
}
#[test]
fn two_ink_tint_threshold() {
    check_tint_threshold("[/DeviceN [/A /B] /DeviceGray 6 0 R]", 2);
}

#[test]
fn transfer_byte_threshold() {
    use crate::function::{Function, TransferFunction};
    use hayro_syntax::object::Object;
    let pdf = Pdf::new(crate::fixtures::one_page(
        100,
        "",
        vec![crate::fixtures::stream(
            "/FunctionType 4 /Domain [0 1] /Range [0 1]",
            b"{ .2 le { 0 } { 1 } ifelse }",
        )],
    ))
    .unwrap();
    let obj = pdf
        .xref()
        .get::<Object<'_>>(ObjectIdentifier::new(4, 0))
        .unwrap();
    let transfer = TransferFunction::new(Function::new(&obj).unwrap());
    assert_eq!(&transfer.samples()[50..=52], &[0, 0, 255]);
}

fn tensor_fixture(data: &[u8]) -> EncodedShadingPattern {
    let pdf = Pdf::new(data.to_vec()).unwrap();
    let dict = pdf
        .xref()
        .get::<Dict<'_>>(ObjectIdentifier::new(6, 0))
        .unwrap();
    ShadingPattern::new(&dict, &Cache::new()).unwrap().encode()
}

#[test]
fn disconnected_patch_grid() {
    let sample = |data: &[u8], x| {
        let mut pixels = Vec::new();
        tensor_fixture(data)
            .sample_texture_metered(Rect::new(x, 31., 168., 32.), 1., &mut Unmetered, |row| {
                pixels.extend_from_slice(row);
            })
            .unwrap();
        pixels
    };
    let plain = include_bytes!("../../tests/fixtures/patch-curve.pdf");
    let grid = include_bytes!("../../tests/fixtures/patch-curve-grid.pdf");
    let expected = sample(plain, 0.);
    assert_eq!(expected[86][3], 0.);
    assert_eq!(expected, sample(grid, 0.));
    assert_eq!(&expected[80..], sample(grid, 80.));
}

#[test]
fn combined_patch_curvature() {
    tensor_fixture(include_bytes!("../../tests/fixtures/patch-combined.pdf"))
        .sample_points_metered(&[Point::new(20.5, 49.5)], &mut Unmetered, |p| {
            assert_eq!(p, [0., 0., 0., 1.]);
        })
        .unwrap();
}

#[test]
fn curved_patch_seam() {
    let pattern = tensor_fixture(include_bytes!("../../tests/fixtures/patch-seam.pdf"));
    pattern
        .sample_points_metered(&[Point::new(5.5, 3.5)], &mut Unmetered, |p| {
            assert_eq!(p[3], 1.);
        })
        .unwrap();
}

#[test]
fn narrow_bowed_patch() {
    let pattern = tensor_fixture(include_bytes!("../../tests/fixtures/patch-bowed.pdf"));
    let mut coverage = Vec::new();
    pattern
        .sample_points_metered(
            &[Point::new(11.5, 106.5), Point::new(11.5, 108.5)],
            &mut Unmetered,
            |p| coverage.push(p),
        )
        .unwrap();
    assert_eq!(coverage, [[0., 0., 0., 1.], [0.; 4]]);
}

#[test]
fn pan_invariant_tensor_mesh() {
    let pattern = tensor_fixture(include_bytes!("../../tests/fixtures/patch-pan.pdf"));
    let samples = |x| {
        let mut row = Vec::new();
        pattern
            .sample_texture_metered(Rect::new(x, 1., 16640., 2.), 1., &mut Unmetered, |r| {
                row.extend_from_slice(r);
            })
            .unwrap();
        row.split_off(row.len() - 256)
    };
    let wide = samples(0.);
    assert_eq!(wide, samples(16384.));
    let points: Vec<_> = (16384..16640)
        .map(|x| Point::new(f64::from(x) + 0.5, 1.5))
        .collect();
    let mut colors = Vec::new();
    pattern
        .sample_points_metered(&points, &mut Unmetered, |p| colors.push(p))
        .unwrap();
    assert_eq!(wide, colors);
}

#[test]
fn flat_patch_narrow_step_contour() {
    let step = crate::fixtures::stream(
        "/FunctionType 4 /Domain [0 1] /Range [0 1]",
        b"{ 0.5 le { 0 } { 1 } ifelse }",
    );
    for kind in [6_u8, 7] {
        // Straight 100x100 square: Coons boundary (12 pts) + optional 4 tensor interior pts.
        // Corner order in PDF: c00 (0,0), c01 (0,100), c11 (100,100), c10 (100,0).
        let mut data = vec![
            0, 0, 0, 0, 85, 0, 170, 0, 255, 85, 255, 170, 255, 255, 255, 255, 170, 255, 85, 255, 0,
            170, 0, 85, 0,
        ];
        if kind == 7 {
            data.extend([85, 85, 85, 170, 170, 170, 170, 85]);
        }
        // Only c11 = 0.5001 (> 0.5); c00, c01, c10 = 0.4999 (< 0.5). Contour is u*v = 0.5.
        data.extend([0, 0, 255, 0]);
        let p = pattern(
            crate::fixtures::stream(
                &format!(
                    "/ShadingType {kind} /BitsPerFlag 8 /ColorSpace /DeviceGray /BitsPerCoordinate 8 /BitsPerComponent 8 /Decode [0 100 0 100 0.4999 0.5001] /Function 6 0 R"
                ),
                &data,
            ),
            step.clone(),
        );
        let mut out = Vec::new();
        p.sample_points_metered(
            &[Point::new(70.0, 70.0), Point::new(72.5, 72.5)],
            &mut Unmetered,
            |c| out.push(c),
        )
        .unwrap();
        assert_eq!(out, [[0., 0., 0., 1.], [1., 1., 1., 1.]], "type {kind}");
    }
}

#[test]
fn deep_zoom_arch_flatness() {
    // Coons patch: bottom straight on y=0, top arch y(t) = 72000*t*(1-t) with x(t) = 24000*t.
    // Encode 16-bit coordinates over [0, 24000] x [0, 24000].
    let enc = |v: u32| -> [u8; 2] { (((v * 65535 + 12000) / 24000).min(65535) as u16).to_be_bytes() };
    let pts: &[(u32, u32)] = &[
        (0, 0),
        (0, 0),
        (0, 0),
        (0, 0), // left edge (degenerate at (0,0))
        (8000, 24000),
        (16000, 24000),
        (24000, 0), // top arch: (0,0)-(8000,24000)-(16000,24000)-(24000,0)
        (24000, 0),
        (24000, 0),
        (24000, 0), // right edge (degenerate at (24000,0))
        (16000, 0),
        (8000, 0), // bottom edge back to (0,0)
    ];
    let mut data = vec![0_u8];
    for &(x, y) in pts {
        data.extend(enc(x));
        data.extend(enc(y));
    }
    data.extend([0, 0, 0, 0]);
    let p = pattern(
        crate::fixtures::stream(
            "/ShadingType 6 /BitsPerFlag 8 /ColorSpace /DeviceGray /BitsPerCoordinate 16 /BitsPerComponent 8 /Decode [0 24000 0 24000 0 1]",
            &data,
        ),
        b"null".to_vec(),
    );
    // At scale 24, d=10 chord misses by 0.412 dev px at t=1025/2048; d=11 hits the curve.
    let x = 24000.0 * 1025.0 / 2048.0;
    let y_in = 17999.974;
    let y_out = 18000.01;
    let mut rows = Vec::new();
    let area = Rect::new(x - 0.5, 17999.5, x + 0.5, 18000.5);
    let (w, _, grid) = p
        .sample_texture_metered(area, 24.0, &mut Unmetered, |r| rows.extend_from_slice(r))
        .unwrap();
    let sample_at = |px: f64, py: f64| {
        let g = grid.inverse() * Point::new(px, py);
        rows[(g.y.floor() as usize) * w as usize + (g.x.floor() as usize)]
    };
    assert_eq!(sample_at(x, y_in), [0., 0., 0., 1.]);
    assert_eq!(sample_at(x, y_out), [0.; 4]);
}

#[test]
fn metered_samples_distinguish_coverage_and_function_failure() {
    let function = crate::fixtures::stream(
        "/FunctionType 4 /Domain [0 1] /Range [0 1]",
        b"{ .5 gt { pop } { 1 } ifelse }",
    );
    let axial = pattern(b"<< /ShadingType 2 /Coords [0 0 4 0] /ColorSpace /DeviceGray /Background [0] /Function 6 0 R >>".to_vec(),function.clone());
    let mesh = pattern(
        crate::fixtures::stream(
            "/ShadingType 5 /VerticesPerRow 2 /BitsPerCoordinate 8 /BitsPerComponent 8 /Decode [0 4 0 4 0 1] /ColorSpace /DeviceGray /Background [0] /Function 6 0 R",
            &[0, 0, 0, 255, 0, 255, 0, 255, 0, 255, 255, 255],
        ),
        function,
    );
    let area = Rect::new(-1., 0., 4., 1.);
    let positions = [-0.5, 0.5, 1.5, 2.5, 3.5].map(|x| Point::new(x, 0.5));
    for pattern in [axial, mesh] {
        let mut rows = Vec::new();
        pattern
            .sample_texture_samples_metered(area, 1., &mut Unmetered, |r| rows.extend_from_slice(r))
            .unwrap();
        let mut points = Vec::new();
        pattern
            .sample_points_samples_metered(&positions, &mut Unmetered, |s| points.push(s))
            .unwrap();
        use ShadingSample::{Failed, Outside, Painted};
        assert_eq!(
            rows,
            [Outside, Painted([1.; 4]), Painted([1.; 4]), Failed, Failed]
        );
        assert_eq!(points, rows);
        let mut rgba = Vec::new();
        pattern
            .sample_texture_metered(area, 1., &mut Unmetered, |r| rgba.extend_from_slice(r))
            .unwrap();
        assert_eq!(pattern.background_rgba(), [0., 0., 0., 1.]);
        assert_eq!(
            rgba,
            [
                pattern.background_rgba(),
                [1.; 4],
                [1.; 4],
                [0.; 4],
                [0.; 4]
            ]
        );
    }
}

fn classified_at(pattern: &EncodedShadingPattern, area: Rect) -> Vec<ShadingSample> {
    let mut rows = Vec::new();
    let (w, h, grid) = pattern
        .sample_texture_samples_metered(area, 1., &mut Unmetered, |row| rows.extend_from_slice(row))
        .unwrap();
    let centers: Vec<_> = (0..h)
        .flat_map(|y| {
            (0..w).map(move |x| grid * Point::new(f64::from(x) + 0.5, f64::from(y) + 0.5))
        })
        .collect();
    let mut points = Vec::new();
    pattern
        .sample_points_samples_metered(&centers, &mut Unmetered, |s| points.push(s))
        .unwrap();
    assert_eq!(rows, points);
    points
}

#[test]
fn translated_bbox_matches_the_paint_clip() {
    use kurbo::Shape;
    let pdf = Pdf::new(crate::fixtures::one_page(100, "", vec![
        b"<< /PatternType 2 /Matrix [1 0 0 1 10 20] /Shading 5 0 R >>".to_vec(),
        b"<< /ShadingType 2 /BBox [0 0 1 1] /Coords [0 0 4 0] /ColorSpace /DeviceGray /Function 6 0 R >>".to_vec(),
        b"<< /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [1] /N 1 >>".to_vec()
    ])).unwrap();
    let dict = pdf
        .xref()
        .get::<Dict<'_>>(ObjectIdentifier::new(4, 0))
        .unwrap();
    let p = ShadingPattern::new(&dict, &Cache::new()).unwrap();
    let clip = p.clip_path().unwrap();
    let encoded = p.encode();
    let area = Rect::new(10., 20., 13., 21.);
    let coverage = classified_at(&encoded, area);
    let mut texture = Vec::new();
    encoded.sample_texture(area, 1., |c| texture.push(c));
    let mut metered = Vec::new();
    encoded
        .sample_texture_metered(area, 1., &mut Unmetered, |r| metered.extend_from_slice(r))
        .unwrap();
    let centers = [10.5, 11.5, 12.5].map(|x| Point::new(x, 20.5));
    let mut points = Vec::new();
    encoded
        .sample_points_metered(&centers, &mut Unmetered, |c| points.push(c))
        .unwrap();
    assert_eq!(points, texture);
    assert_eq!(metered, texture);
    for ((sample, center), rgba) in coverage.into_iter().zip(centers).zip(texture) {
        assert_eq!(
            sample,
            if clip.contains(center) {
                ShadingSample::Painted(rgba)
            } else {
                ShadingSample::Outside
            }
        );
    }
}

#[test]
fn isolated_mesh_uses_half_open_spans() {
    for (code, covered) in [
        (b"{ pop 1 }".as_slice(), ShadingSample::Painted([1.; 4])),
        (b"{ pop pop }", ShadingSample::Failed),
    ] {
        let p = pattern(
            crate::fixtures::stream(
                "/ShadingType 4 /Function 6 0 R /ColorSpace /DeviceGray /BitsPerCoordinate 8 /BitsPerComponent 8 /BitsPerFlag 8 /Decode [0 4 0 4 0 1]",
                &[0, 0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0],
            ),
            crate::fixtures::stream("/FunctionType 4 /Domain [0 1] /Range [0 1]", code),
        );
        for (i, sample) in classified_at(&p, Rect::new(0., 0., 4., 4.))
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                sample,
                if i % 4 + i / 4 < 3 {
                    covered
                } else {
                    ShadingSample::Outside
                }
            );
        }
    }
}

#[test]
fn unextended_axial_endpoints_use_geometric_precision() {
    let p = pattern(
        b"<< /ShadingType 2 /Coords [0 0 1 0] /ColorSpace /DeviceGray /Function 6 0 R >>".to_vec(),
        b"<< /FunctionType 2 /Domain [0 1] /C0 [1] /C1 [1] /N 1 >>".to_vec(),
    );
    let epsilon = 2_f64.powi(-30);
    for x in [-epsilon, 0., epsilon, 1. - epsilon, 1., 1. + epsilon] {
        assert_eq!(
            classified_at(&p, Rect::new(x - 0.5, 0., x + 0.5, 1.)),
            [if (0.0..=1.0).contains(&x) {
                ShadingSample::Painted([1.; 4])
            } else {
                ShadingSample::Outside
            }]
        );
    }
}
