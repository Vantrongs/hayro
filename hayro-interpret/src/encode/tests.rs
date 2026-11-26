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
fn tint_work_is_reserved_for_the_table_or_for_each_sample() {
    let points = [Point::new(0.25, 0.0), Point::new(0.75, 0.0)];
    for (space, function, inputs, code, table) in [
        (
            "[/Separation /Spot /DeviceGray 6 0 R]",
            "/C0 [0] /C1 [1]",
            "[0 1]",
            "{ dup mul }",
            true,
        ),
        (
            "[/Indexed [/Separation /Spot /DeviceGray 6 0 R] 1 <00ff>]",
            "/C0 [0] /C1 [1]",
            "[0 1]",
            "{ dup mul }",
            true,
        ),
        (
            "[/DeviceN [/Spot] /DeviceGray 6 0 R]",
            "/C0 [0] /C1 [1]",
            "[0 1]",
            "{ dup mul }",
            true,
        ),
        (
            "[/DeviceN [/Spot /Black] /DeviceGray 6 0 R]",
            "/C0 [0 0] /C1 [1 1]",
            "[0 1 0 1]",
            "{ add 2 div }",
            false,
        ),
    ] {
        let pattern = pattern(
            format!("<< /ShadingType 2 /Coords [0 0 1 0] /ColorSpace {space} /Function << /FunctionType 2 /Domain [0 1] {function} /N 1 >> >>").into_bytes(),
            crate::fixtures::stream(&format!("/FunctionType 4 /Domain {inputs} /Range [0 1]"),code.as_bytes())
        );
        let cost = pattern.setup_cost() + pattern.point_cost().times(points.len() as u64);
        assert_eq!(pattern.setup_cost() != Cost::ZERO, table);
        let before = crate::function::test_operator_count();
        let mut empty = Allow {
            steps: u64::MAX,
            bytes: 0,
        };
        assert_eq!(
            pattern.sample_points_metered(&points, &mut empty, |_| panic!("unexpected sample")),
            Err(Exhausted)
        );
        assert_eq!(before, crate::function::test_operator_count());
        let mut meter = Allow {
            steps: cost.steps() - 1,
            bytes: usize::MAX,
        };
        assert!(
            pattern
                .sample_points_metered(&points, &mut meter, |_| panic!("published a refused tint"))
                .is_err()
        );
        assert_eq!(before, crate::function::test_operator_count());
        meter.steps = cost.steps();
        let mut colors = Vec::new();
        pattern
            .sample_points_metered(&points, &mut meter, |c| colors.push(c))
            .unwrap();
        assert_ne!(colors[0], colors[1]);
        assert!(before < crate::function::test_operator_count());
        assert_eq!(pattern.setup_cost(), Cost::ZERO);
    }
}
