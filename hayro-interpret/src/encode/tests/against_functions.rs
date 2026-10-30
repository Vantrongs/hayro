//! Axial and radial shadings against their functions evaluated directly, over
//! generated functions and color spaces: every pixel of a texture, and every
//! pixel an axial shading's gradient stops paint, is within a level of the
//! color the shading's function and color space give at its parameter.

use super::shading_pattern;
use crate::encode::{EncodedShadingPattern, EncodedShadingType};
use crate::gradient::levels_apart;
use kurbo::{Point, Rect};
use smallvec::smallvec;

/// A pixel may be off by the rounding of the color it is compared with and of
/// the stops it is drawn from, each half a level, and by what straight lines
/// between stops a pixel's step apart miss of a curve: no more than a fiftieth
/// of a level for the curves here.
const LEVELS: f32 = 1.0 + 0.02;

/// Where the color at an input lies off the line through its neighbours this
/// share of a pixel's step to either side by more than this many levels, it has
/// a feature narrower than a pixel.
const SHARP_LEVELS: f32 = 0.5;
const SHARP_SHARE: f64 = 1.0 / 32.0;

/// xorshift64*, so the cases are the same in every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1_u64 << 53) as f64
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// A number in `[0, 1]` written to two places.
    fn value(&mut self) -> String {
        format!("{:.2}", self.unit())
    }

    fn values(&mut self, n: usize) -> String {
        (0..n).map(|_| self.value()).collect::<Vec<_>>().join(" ")
    }
}

/// A color space and the outputs a shading's function gives it.
struct Space {
    name: &'static str,
    /// The `/ColorSpace` entry; a tint transform is object 7.
    entry: &'static str,
    tint: Option<&'static str>,
    outputs: usize,
    /// Whether the color is the function's outputs themselves, clamped.
    straight: bool,
}

const SPACES: [Space; 4] = [
    Space {
        name: "gray",
        entry: "/DeviceGray",
        tint: None,
        outputs: 1,
        straight: true,
    },
    Space {
        name: "rgb",
        entry: "/DeviceRGB",
        tint: None,
        outputs: 3,
        straight: true,
    },
    Space {
        name: "separation",
        entry: "[/Separation /Spot /DeviceRGB 7 0 R]",
        tint: Some("<< /FunctionType 2 /Domain [0 1] /C0 [1 1 1] /C1 [0.2 0.5 0.9] /N 1 >>"),
        outputs: 1,
        straight: false,
    },
    Space {
        name: "devicen",
        entry: "[/DeviceN [/A /B] /DeviceGray 7 0 R]",
        tint: Some(
            "<< /FunctionType 4 /Domain [0 1 0 1] /Range [0 1] /Length 7 >>\nstream\n{ mul }\nendstream",
        ),
        outputs: 2,
        straight: false,
    },
];

/// A generated function, object 6 of the file.
struct Generated {
    name: String,
    object: String,
    /// Straight between points of its own: a sampled function, a linear
    /// exponential one, a stitching of them.
    lines: bool,
}

/// A sampled function of `outputs` outputs over `size` samples, most of each
/// output's samples its neighbour's and some a lone spike.
fn sampled(rng: &mut Rng, order: u32, size: usize, outputs: usize) -> Generated {
    let mut samples = Vec::with_capacity(size * outputs);
    let mut level = [0_u8; 4];
    for _ in 0..size {
        for level in level.iter_mut().take(outputs) {
            match rng.below(4) {
                0 => *level = rng.below(256) as u8,
                1 => {
                    samples.push(255 - *level);
                    continue;
                }
                _ => {}
            }
            samples.push(*level);
        }
    }
    sampled_of(
        &format!("type0 order {order} size {size}"),
        order,
        size,
        outputs,
        &samples,
    )
}

fn sampled_of(name: &str, order: u32, size: usize, outputs: usize, samples: &[u8]) -> Generated {
    let hex: String = samples.iter().map(|s| format!("{s:02X}")).collect();
    let range = vec!["0 1"; outputs].join(" ");
    Generated {
        name: name.to_string(),
        object: format!(
            "<< /FunctionType 0 /Domain [0 1] /Range [{range}] /Size [{size}] /BitsPerSample 8 \
             /Order {order} /Filter /ASCIIHexDecode /Length {} >>\nstream\n{hex}>\nendstream",
            hex.len() + 1
        ),
        lines: true,
    }
}

fn exponential(rng: &mut Rng, n: f64, outputs: usize) -> Generated {
    Generated {
        name: format!("type2 n {n}"),
        object: format!(
            "<< /FunctionType 2 /Domain [0 1] /C0 [{}] /C1 [{}] /N {n} >>",
            rng.values(outputs),
            rng.values(outputs)
        ),
        lines: n == 1.0,
    }
}

/// A stitching of constant and linear parts, each part's ends drawn apart from
/// its neighbours', so it steps at most of its bounds; some parts encoded
/// backwards.
fn stitching(rng: &mut Rng, outputs: usize) -> Generated {
    let parts = 2 + rng.below(4) as usize;
    let mut bounds: Vec<f64> = (1..parts).map(|_| rng.unit()).collect();
    bounds.sort_by(f64::total_cmp);
    let functions: Vec<String> = (0..parts)
        .map(|_| {
            let c0 = rng.values(outputs);
            let c1 = if rng.below(2) == 0 {
                c0.clone()
            } else {
                rng.values(outputs)
            };
            format!("<< /FunctionType 2 /Domain [0 1] /C0 [{c0}] /C1 [{c1}] /N 1 >>")
        })
        .collect();
    let encode: Vec<&str> = (0..parts)
        .map(|_| if rng.below(3) == 0 { "1 0" } else { "0 1" })
        .collect();
    Generated {
        name: format!("type3 parts {parts}"),
        object: format!(
            "<< /FunctionType 3 /Domain [0 1] /Bounds [{}] /Encode [{}] /Functions [{}] >>",
            bounds
                .iter()
                .map(|b| format!("{b:.4}"))
                .collect::<Vec<_>>()
                .join(" "),
            encode.join(" "),
            functions.join(" ")
        ),
        lines: true,
    }
}

/// A PostScript function that steps from one constant to another at two
/// thresholds.
fn steps(rng: &mut Rng, outputs: usize) -> Generated {
    let (mut a, mut b) = (rng.unit(), rng.unit());
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    let program = format!(
        "{{ dup {a:.4} lt {{ pop {} }} {{ {b:.4} lt {{ {} }} {{ {} }} ifelse }} ifelse }}",
        rng.values(outputs),
        rng.values(outputs),
        rng.values(outputs)
    );
    let range = vec!["0 1"; outputs].join(" ");
    Generated {
        name: "type4 steps".to_string(),
        object: format!(
            "<< /FunctionType 4 /Domain [0 1] /Range [{range}] /Length {} >>\nstream\n{program}\nendstream",
            program.len()
        ),
        lines: false,
    }
}

/// The functions of `outputs` outputs the cases draw.
fn functions(rng: &mut Rng, outputs: usize) -> Vec<Generated> {
    let mut out = Vec::new();
    if outputs == 1 {
        // A ring at t = 3/8 between dark samples.
        out.push(sampled_of(
            "type0 lone ring",
            1,
            9,
            1,
            &[0, 0, 0, 255, 0, 0, 0, 0, 0],
        ));
    }
    for order in [1, 3] {
        for size in [2, 9, 33, 1024] {
            out.push(sampled(rng, order, size, outputs));
        }
    }
    for n in [0.5, 1.0, 2.0, 7.0] {
        out.push(exponential(rng, n, outputs));
    }
    for _ in 0..4 {
        out.push(stitching(rng, outputs));
    }
    for _ in 0..2 {
        out.push(steps(rng, outputs));
    }
    out
}

/// Where a shading is drawn and its parameter at a point there.
struct Geometry {
    name: &'static str,
    entry: String,
    bbox: Rect,
    t: fn(Point) -> f64,
}

fn geometries() -> [Geometry; 3] {
    // `t` of the circle `|p - c0 - t (c1 - c0)| = r0 + t (r1 - r0)` through
    // `p`, the larger, past either circle taken at it.
    fn radial(p: Point, [x0, y0, r0, x1, y1, r1]: [f64; 6]) -> f64 {
        let (px, py, dx, dy, dr) = (p.x - x0, p.y - y0, x1 - x0, y1 - y0, r1 - r0);
        let a = dx * dx + dy * dy - dr * dr;
        let b = -2.0 * (px * dx + py * dy + r0 * dr);
        let c = px * px + py * py - r0 * r0;
        let root = (b * b - 4.0 * a * c).max(0.0).sqrt();
        let t = [(-b + root) / (2.0 * a), (-b - root) / (2.0 * a)]
            .into_iter()
            .filter(|t| r0 + t * dr >= 0.0)
            .fold(f64::NEG_INFINITY, f64::max);
        t.clamp(0.0, 1.0)
    }
    const CONCENTRIC: [f64; 6] = [64.0, 64.0, 4.0, 64.0, 64.0, 60.0];
    const NESTED: [f64; 6] = [56.0, 60.0, 6.0, 66.0, 64.0, 58.0];
    let coords = |c: [f64; 6]| c.map(|v| v.to_string()).join(" ");
    [
        Geometry {
            name: "axial",
            entry: "/ShadingType 2 /Coords [0 0 256 0]".to_string(),
            bbox: Rect::new(0.0, 0.0, 256.0, 2.0),
            t: |p| (p.x / 256.0).clamp(0.0, 1.0),
        },
        Geometry {
            name: "concentric",
            entry: format!("/ShadingType 3 /Coords [{}]", coords(CONCENTRIC)),
            bbox: Rect::new(0.0, 0.0, 128.0, 128.0),
            t: |p| radial(p, CONCENTRIC),
        },
        Geometry {
            name: "nested",
            entry: format!("/ShadingType 3 /Coords [{}]", coords(NESTED)),
            bbox: Rect::new(0.0, 0.0, 128.0, 128.0),
            t: |p| radial(p, NESTED),
        },
    ]
}

/// The color the shading's function and color space give at `t`, with its
/// opacity and transfer function.
fn direct(encoded: &EncodedShadingPattern, t: f64) -> [f32; 4] {
    let EncodedShadingType::RadialAxial(shading) = &encoded.shading_type else {
        unreachable!("an axial or radial shading");
    };
    let values = shading.function.eval(&smallvec![t as f32]);
    encoded.finish(values.map(|v| encoded.color_space.to_rgba(&v, 1.0)))
}

/// The [`table_step`](crate::gradient::table_step) of the function's outputs
/// at `t`, the shading's own as its native gradient is checked against.
fn table_step(encoded: &EncodedShadingPattern, t: f64) -> f32 {
    let EncodedShadingType::RadialAxial(shading) = &encoded.shading_type else {
        unreachable!("an axial or radial shading");
    };
    shading
        .function
        .eval(&smallvec![t as f32])
        .map_or(0.0, |values| crate::gradient::table_step(encoded, &values))
}

/// Whether the color has a feature narrower than `step` within `step` of `t`:
/// a color off the line through its neighbours, [`SHARP_SHARE`] of `step` to
/// either side, by more than [`SHARP_LEVELS`], as at a step of the function or
/// where its slope has no bound.
fn sharp_near(encoded: &EncodedShadingPattern, t: f64, step: f64) -> bool {
    let (from, to) = ((t - step).max(0.0), (t + step).min(1.0));
    let n = (2.0 / SHARP_SHARE) as usize;
    let colors: Vec<[f32; 4]> = (0..=n)
        .map(|i| direct(encoded, from + (to - from) * i as f64 / n as f64))
        .collect();
    colors.windows(3).any(|w| {
        let line = [0, 1, 2, 3].map(|c| (w[0][c] + w[2][c]) / 2.0);
        levels_apart(&w[1], &line) > SHARP_LEVELS
    })
}

/// The color straight lines between `stops` give at `t`, from above where two
/// share it, as a gradient is drawn.
fn between(stops: &[crate::gradient::SvgGradientStop], t: f32) -> [f32; 4] {
    let next = stops.partition_point(|s| s.offset <= t);
    match (next.checked_sub(1).map(|i| stops[i]), stops.get(next)) {
        (Some(a), Some(b)) if b.offset > a.offset => {
            let s = (t - a.offset) / (b.offset - a.offset);
            [0, 1, 2, 3].map(|c| a.color[c] + (b.color[c] - a.color[c]) * s)
        }
        (Some(stop), _) => stop.color,
        (None, Some(stop)) => stop.color,
        (None, None) => [0.0; 4],
    }
}

/// What the comparisons found.
#[derive(Default)]
struct Tally {
    compared: u64,
    /// Pixels off by more than [`LEVELS`] at a feature narrower than a pixel,
    /// by case.
    sharp: std::collections::BTreeMap<String, u64>,
    failures: Vec<String>,
}

impl Tally {
    /// Compares a pixel at `p` painted `got`, `lines` where every pixel must be
    /// within a level, a feature of the function narrower than a pixel or not.
    fn pixel(
        &mut self,
        encoded: &EncodedShadingPattern,
        geometry: &Geometry,
        p: Point,
        got: [f32; 4],
        (lines, straight): (bool, bool),
        case: &str,
    ) {
        self.compared += 1;
        let t = (geometry.t)(p);
        let want = direct(encoded, t);
        let off = levels_apart(&got, &want);
        let tables = if straight {
            0.0
        } else {
            table_step(encoded, t)
        };
        if off <= LEVELS + tables {
            return;
        }
        // A pixel's step of the parameter: how much it changes over a pixel
        // along its gradient.
        let across = |dx: f64, dy: f64| {
            ((geometry.t)(Point::new(p.x + dx, p.y + dy))
                - (geometry.t)(Point::new(p.x - dx, p.y - dy)))
                / 2.0
        };
        let step = across(1.0, 0.0).hypot(across(0.0, 1.0));
        if !lines && sharp_near(encoded, t, step) {
            *self.sharp.entry(case.to_string()).or_default() += 1;
            return;
        }
        if self.failures.len() < 20 {
            self.failures.push(format!(
                "{case} at {p:?}, t {t}: {got:?}, the function's {want:?}, {off} levels"
            ));
        } else {
            self.failures.push(String::new());
        }
    }
}

/// Every pixel an axial or radial shading's texture paints, and every pixel an
/// axial one's gradient stops paint as a renderer draws them, is within a level
/// of the color its function and color space give at the pixel's parameter:
/// sampled functions of order 1 and 3 whose samples spike, exponential ones of
/// `N` 0.5, 1, 2 and 7, stitchings of constant and linear parts that step at
/// their bounds, PostScript ones that step, into gray, RGB, a `Separation`
/// space and a `DeviceN` one. Where the function is straight between points
/// of its own into gray or RGB, every pixel is; elsewhere a pixel is excused
/// at a feature of the color narrower than its step of the parameter, and
/// those are counted.
#[test]
fn shadings_paint_within_a_level_of_their_functions() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut tally = Tally::default();
    let mut lines_cases = 0;
    for space in &SPACES {
        for function in functions(&mut rng, space.outputs) {
            let lines = function.lines && space.straight;
            if lines {
                lines_cases += 1;
            }
            for geometry in geometries() {
                let case = format!("{} {} {}", geometry.name, space.name, function.name);
                let mut objects = vec![
                    "<< /PatternType 2 /Shading 5 0 R >>".to_string(),
                    format!(
                        "<< {} /ColorSpace {} /Extend [true true] /Function 6 0 R >>",
                        geometry.entry, space.entry
                    ),
                    function.object.clone(),
                ];
                if let Some(tint) = space.tint {
                    objects.push(tint.to_string());
                }
                let encoded = shading_pattern(&objects).encode();
                let mut texture = vec![];
                let (width, _, _) = encoded
                    .sample_texture(geometry.bbox, 1.0, |row| texture.extend_from_slice(row));
                for (i, got) in texture.iter().enumerate() {
                    let p = Point::new(
                        f64::from(i as u32 % width) + 0.5,
                        f64::from(i as u32 / width) + 0.5,
                    );
                    tally.pixel(
                        &encoded,
                        &geometry,
                        p,
                        *got,
                        (lines, space.straight),
                        &format!("{case} texture"),
                    );
                }
                let EncodedShadingType::RadialAxial(shading) = &encoded.shading_type else {
                    unreachable!("an axial or radial shading");
                };
                if let Some(gradient) = shading.as_svg_gradient(&encoded, geometry.bbox) {
                    for x in 0..geometry.bbox.width() as u32 {
                        let p = Point::new(f64::from(x) + 0.5, 0.5);
                        let t = (encoded.base_transform * p).x as f32;
                        let got = between(&gradient.stops, t.clamp(0.0, 1.0));
                        tally.pixel(
                            &encoded,
                            &geometry,
                            p,
                            got,
                            (lines, space.straight),
                            &format!("{case} stops"),
                        );
                    }
                }
            }
        }
    }
    eprintln!(
        "{} pixels compared, {} excused at a feature narrower than a pixel, {} off",
        tally.compared,
        tally.sharp.values().sum::<u64>(),
        tally.failures.len()
    );
    for (case, n) in &tally.sharp {
        eprintln!("  excused {n:6} {case}");
    }
    assert!(lines_cases > 20, "{lines_cases}");
    assert!(
        tally.failures.is_empty(),
        "{} pixels off, among them:\n{}",
        tally.failures.len(),
        tally
            .failures
            .iter()
            .filter(|f| !f.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(tally.compared > 1_000_000, "{}", tally.compared);
    // Features narrower than a pixel are only where the function has them:
    // samples closer than a pixel, a PostScript function's steps.
    for case in tally.sharp.keys() {
        assert!(
            case.contains("size 1024") || case.contains("type4"),
            "{case}"
        );
    }
}
