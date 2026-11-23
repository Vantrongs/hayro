//! ISO 32000-1 function evaluation at samples, stitching nodes, and adjacent inputs.
use super::Function;
use hayro_syntax::{
    Pdf,
    object::{Dict, ObjectIdentifier},
};

/// A function as the specification defines it, its numbers those a reader
/// holds them as, `f32`.
#[derive(Clone, Debug)]
enum Spec {
    /// 7.10.2, one input, linear interpolation.
    Sampled {
        domain: (f64, f64),
        range: Vec<(f64, f64)>,
        encode: (f64, f64),
        decode: Vec<(f64, f64)>,
        bits: u32,
        /// `size` samples of each output, the outputs of one sample together.
        samples: Vec<u32>,
        size: usize,
    },
    /// 7.10.3.
    Exponential {
        domain: (f64, f64),
        range: Option<Vec<(f64, f64)>>,
        c0: Vec<f64>,
        c1: Vec<f64>,
        n: f64,
    },
    /// 7.10.4.
    Stitching {
        domain: (f64, f64),
        range: Option<Vec<(f64, f64)>>,
        functions: Vec<Self>,
        bounds: Vec<f64>,
        encode: Vec<(f64, f64)>,
    },
}

/// `Interpolate(x, xmin, xmax, ymin, ymax)` of 7.10.2.
fn interpolate(x: f64, x_min: f64, x_max: f64, y_min: f64, y_max: f64) -> f64 {
    y_min + (x - x_min) * ((y_max - y_min) / (x_max - x_min))
}

fn clip(y: f64, (lo, hi): (f64, f64)) -> f64 {
    y.max(lo).min(hi)
}

/// A number as a reader holds it.
fn held(v: f64) -> f64 {
    f64::from(v as f32)
}

impl Spec {
    fn domain(&self) -> (f64, f64) {
        match self {
            Self::Sampled { domain, .. }
            | Self::Exponential { domain, .. }
            | Self::Stitching { domain, .. } => *domain,
        }
    }

    fn outputs(&self) -> usize {
        match self {
            Self::Sampled { range, .. } => range.len(),
            Self::Exponential { c0, .. } => c0.len(),
            Self::Stitching { functions, .. } => functions[0].outputs(),
        }
    }

    /// The outputs at `x`, as the specification defines them.
    fn eval(&self, x: f64) -> Vec<f64> {
        let (d0, d1) = self.domain();
        // "Input values outside the declared domain shall be clipped to the
        // nearest boundary value."
        let x = x.max(d0).min(d1);
        match self {
            Self::Sampled {
                range,
                encode,
                decode,
                bits,
                samples,
                size,
                ..
            } => {
                let e = interpolate(x, d0, d1, encode.0, encode.1);
                let e = e.max(0.0).min(*size as f64 - 1.0);
                let (i, j) = (e.floor() as usize, e.ceil() as usize);
                let m = range.len();
                (0..m)
                    .map(|o| {
                        let (a, b) = (f64::from(samples[i * m + o]), f64::from(samples[j * m + o]));
                        let r = a + (b - a) * (e - i as f64);
                        let most = f64::from((1_u64 << bits) as u32 - 1);
                        let r = interpolate(r, 0.0, most, decode[o].0, decode[o].1);
                        clip(r, range[o])
                    })
                    .collect()
            }
            Self::Exponential {
                range, c0, c1, n, ..
            } => {
                let y: Vec<f64> = c0
                    .iter()
                    .zip(c1)
                    .map(|(c0, c1)| c0 + x.powf(*n) * (c1 - c0))
                    .collect();
                match range {
                    Some(range) => y.iter().zip(range).map(|(y, r)| clip(*y, *r)).collect(),
                    None => y,
                }
            }
            Self::Stitching {
                range,
                functions,
                bounds,
                encode,
                ..
            } => {
                let k = functions.len();
                // `Bounds` with `Domain0` before it and `Domain1` after.
                let mut b = vec![d0];
                b.extend(bounds);
                b.push(d1);
                // "a series of half-open intervals, closed on the left and open
                // on the right (except the last, which is closed on the right
                // as well)".
                let i = (0..k)
                    .find(|&i| b[i] <= x && (x < b[i + 1] || i == k - 1 && x <= b[i + 1]))
                    .expect("an interval");
                // "If the last bound, Bounds k-2, is equal to Domain1, then x'
                // shall be defined to be Encode 2i."
                let encoded = if i == k - 1 && k >= 2 && b[k - 1] == d1 {
                    encode[i].0
                } else {
                    interpolate(x, b[i], b[i + 1], encode[i].0, encode[i].1)
                };
                let y = functions[i].eval(encoded);
                match range {
                    Some(range) => y.iter().zip(range).map(|(y, r)| clip(*y, *r)).collect(),
                    None => y,
                }
            }
        }
    }

    /// Where the function may bend or step, as inputs of it: the sides of its
    /// domain and its bounds, written in the file, and where an encoding puts
    /// a sample, the clipping of an encoded input, or a node of a function
    /// stitched, found through an encoding. Each with whether it is written.
    fn nodes(&self) -> Vec<(f64, bool)> {
        let (d0, d1) = self.domain();
        let mut out = vec![(d0, true), (d1, true)];
        match self {
            Self::Sampled { encode, size, .. } => {
                if encode.0 != encode.1 {
                    for k in 0..*size {
                        let x = interpolate(k as f64, encode.0, encode.1, d0, d1);
                        if x > d0 && x < d1 {
                            out.push((x, false));
                        }
                    }
                }
            }
            Self::Exponential { .. } => {}
            Self::Stitching {
                functions,
                bounds,
                encode,
                ..
            } => {
                let mut b = vec![d0];
                b.extend(bounds);
                b.push(d1);
                out.extend(bounds.iter().map(|&x| (x, true)));
                for (i, f) in functions.iter().enumerate() {
                    let (e0, e1) = encode[i];
                    if b[i] == b[i + 1] || e0 == e1 {
                        continue;
                    }
                    for (u, _) in f.nodes() {
                        let x = interpolate(u, e0, e1, b[i], b[i + 1]);
                        if x > b[i] && x < b[i + 1] {
                            out.push((x, false));
                        }
                    }
                    // Where the encoded input leaves the function's domain,
                    // which clips it there.
                    let (f0, f1) = f.domain();
                    for u in [f0, f1] {
                        let x = interpolate(u, e0, e1, b[i], b[i + 1]);
                        if x > b[i] && x < b[i + 1] {
                            out.push((x, false));
                        }
                    }
                }
            }
        }
        out
    }

    /// The function as a PDF object: a dictionary written in place, or a
    /// reference to a stream added to `objects`, numbered from `first`.
    fn write(&self, objects: &mut Vec<String>, first: usize) -> String {
        let pairs = |v: &[(f64, f64)]| {
            v.iter()
                .map(|(a, b)| format!("{a} {b}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let list = |v: &[f64]| v.iter().map(f64::to_string).collect::<Vec<_>>().join(" ");
        match self {
            Self::Sampled {
                domain,
                range,
                encode,
                decode,
                bits,
                samples,
                size,
            } => {
                assert_eq!(*bits, 8, "bytes of samples");
                let hex: String = samples.iter().map(|s| format!("{s:02X}")).collect();
                let data = format!("{hex}>");
                objects.push(format!(
                    "<< /FunctionType 0 /Domain [{}] /Range [{}] /Size [{size}] \
                     /BitsPerSample 8 /Encode [{}] /Decode [{}] /Filter /ASCIIHexDecode \
                     /Length {} >>\nstream\n{data}\nendstream",
                    pairs(&[*domain]),
                    pairs(range),
                    pairs(&[*encode]),
                    pairs(decode),
                    data.len()
                ));
                format!("{} 0 R", first + objects.len() - 1)
            }
            Self::Exponential {
                domain,
                range,
                c0,
                c1,
                n,
            } => format!(
                "<< /FunctionType 2 /Domain [{}] {} /C0 [{}] /C1 [{}] /N {n} >>",
                pairs(&[*domain]),
                range
                    .as_ref()
                    .map_or(String::new(), |r| format!("/Range [{}]", pairs(r))),
                list(c0),
                list(c1),
            ),
            Self::Stitching {
                domain,
                range,
                functions,
                bounds,
                encode,
            } => {
                let written: Vec<String> =
                    functions.iter().map(|f| f.write(objects, first)).collect();
                format!(
                    "<< /FunctionType 3 /Domain [{}] {} /Functions [{}] /Bounds [{}] \
                     /Encode [{}] >>",
                    pairs(&[*domain]),
                    range
                        .as_ref()
                        .map_or(String::new(), |r| format!("/Range [{}]", pairs(r))),
                    written.join(" "),
                    list(bounds),
                    pairs(encode),
                )
            }
        }
    }
}

/// xorshift64*, so the cases are the same in every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// A number of at most three places from `lo` to `hi`, as a reader holds
    /// it.
    fn number(&mut self, lo: f64, hi: f64) -> f64 {
        let steps = ((hi - lo) * 1000.0).round() as u64;
        held(lo + self.below(steps + 1) as f64 / 1000.0)
    }
}

/// A domain: `[0 1]` or another, wider or narrower, away from zero or across it.
fn domain(rng: &mut Rng) -> (f64, f64) {
    match rng.below(4) {
        0 => (0.0, 1.0),
        1 => (2.0, 5.0),
        2 => (-1.5, 0.25),
        _ => {
            let a = rng.number(-3.0, 3.0);
            (a, held(a + 0.125 + rng.number(0.0, 4.0)))
        }
    }
}

/// A sampled function of one output over `domain`, of 1, 2 or more samples,
/// its encoding in range, reversed, or past the samples at either end.
fn sampled(rng: &mut Rng, domain: (f64, f64)) -> Spec {
    let size = match rng.below(4) {
        0 => 1,
        1 => 2,
        _ => 3 + rng.below(9) as usize,
    };
    let last = size as f64 - 1.0;
    let encode = match rng.below(4) {
        0 => (0.0, last),
        1 => (last, 0.0),
        2 => (rng.number(-2.0, 0.0), rng.number(last, last + 2.0)),
        _ => (rng.number(-2.0, last + 2.0), rng.number(-2.0, last + 2.0)),
    };
    let decode = if rng.below(3) == 0 {
        (held(1.2), held(-0.2))
    } else {
        (0.0, 1.0)
    };
    Spec::Sampled {
        domain,
        range: vec![(0.0, 1.0)],
        encode,
        decode: vec![decode],
        bits: 8,
        samples: (0..size)
            .map(|_| [0, 255, rng.below(256) as u32][rng.below(3) as usize])
            .collect(),
        size,
    }
}

/// A line over `domain`, clipped to `[0 1]` or not where it stays within it.
fn line(rng: &mut Rng, domain: (f64, f64)) -> Spec {
    let (c0, c1) = (rng.number(-0.5, 1.5), rng.number(-0.5, 1.5));
    let unit = domain == (0.0, 1.0) && (0.0..=1.0).contains(&c0) && (0.0..=1.0).contains(&c1);
    Spec::Exponential {
        domain,
        range: (!unit || rng.below(2) == 0).then(|| vec![(0.0, 1.0)]),
        c0: vec![c0],
        c1: vec![c1],
        n: 1.0,
    }
}

/// A stitching over `domain` of `depth` levels at most: bounds with
/// intervals of no width first, between, and last among them, encodings in
/// order, reversed, and past the functions' domains.
fn stitching(rng: &mut Rng, domain: (f64, f64), depth: u32) -> Spec {
    let k = 1 + rng.below(4) as usize;
    let (d0, d1) = domain;
    let mut bounds: Vec<f64> = (0..k - 1).map(|_| rng.number(d0, d1)).collect();
    bounds.sort_by(f64::total_cmp);
    if k >= 2 {
        match rng.below(5) {
            0 => bounds[0] = d0,
            1 => bounds[k - 2] = d1,
            2 if k >= 3 => bounds[1] = bounds[0],
            _ => {}
        }
    }
    let functions = (0..k)
        .map(|_| {
            let child = (0.0, 1.0);
            match rng.below(if depth > 0 { 3 } else { 2 }) {
                0 => sampled(rng, child),
                1 => line(rng, child),
                _ => stitching(rng, child, depth - 1),
            }
        })
        .collect();
    let encode = (0..k)
        .map(|_| match rng.below(4) {
            0 => (0.0, 1.0),
            1 => (1.0, 0.0),
            2 => (rng.number(-0.5, 0.5), rng.number(0.5, 1.5)),
            _ => (rng.number(0.0, 1.0), rng.number(0.0, 1.0)),
        })
        .collect();
    Spec::Stitching {
        domain,
        range: (rng.below(2) == 0).then(|| vec![(0.0, 1.0)]),
        functions,
        bounds,
        encode,
    }
}

/// The reviewers' cases: samples `00 FF` encoded from -1 to 2, which
/// evaluates to `clamp(3t - 1, 0, 1)`, and reversed; and a stitching whose
/// last bound is `Domain1`, which evaluates to its last encoding's start
/// there.
fn named() -> Vec<Spec> {
    let clipped = |encode| Spec::Sampled {
        domain: (0.0, 1.0),
        range: vec![(0.0, 1.0)],
        encode,
        decode: vec![(0.0, 1.0)],
        bits: 8,
        samples: vec![0, 255],
        size: 2,
    };
    let ramp = Spec::Exponential {
        domain: (0.0, 1.0),
        range: None,
        c0: vec![0.0],
        c1: vec![1.0],
        n: 1.0,
    };
    vec![
        clipped((-1.0, 2.0)),
        clipped((2.0, -1.0)),
        Spec::Stitching {
            domain: (0.0, 1.0),
            range: None,
            functions: vec![ramp.clone(), ramp],
            bounds: vec![1.0],
            encode: vec![(0.0, 1.0), (held(0.25), 1.0)],
        },
    ]
}

/// The most a value may lie from the specification's, of `f32` arithmetic on
/// values within a unit.
const TOLERANCE: f64 = 1e-4;

#[test]
fn functions_match_the_spec_at_nodes_and_neighboring_floats() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut specs = named();
    for _ in 0..300 {
        let d = domain(&mut rng);
        specs.push(match rng.below(3) {
            0 => sampled(&mut rng, d),
            1 => line(&mut rng, d),
            _ => stitching(&mut rng, d, 2),
        });
    }
    for (case, spec) in specs.iter().enumerate() {
        let mut objects = Vec::new();
        let written = spec.write(&mut objects, 5);
        let mut body = vec![format!("<< /F {written} >>").into_bytes()];
        body.extend(objects.into_iter().map(String::into_bytes));
        let pdf = Pdf::new(crate::fixtures::one_page(1, "", body)).unwrap();
        let dict = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(4, 0))
            .unwrap();
        let f = Function::new(&dict.get(b"F").unwrap()).unwrap();
        assert_eq!(f.num_outputs(), spec.outputs());
        let (lo, hi) = spec.domain();
        let dense = (0..=1000).map(|i| (lo + (hi - lo) * f64::from(i) / 1000.0) as f32);
        let nodes = spec.nodes().into_iter().flat_map(|(x, _)| {
            let x = x as f32;
            [x.next_down(), x, x.next_up()]
        });
        for x in dense.chain(nodes) {
            let output = f.eval_one(&[x]).unwrap();
            for (got, expected) in output.iter().copied().zip(spec.eval(f64::from(x))) {
                assert!(
                    (f64::from(got) - expected).abs() < TOLERANCE,
                    "case {case}, x={x:?}: {got} vs {expected} for {spec:?}"
                );
            }
        }
    }
}
