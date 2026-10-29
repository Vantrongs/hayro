//! Images draw as `MuPDF` draws them: the cases in `short.txt` and `decode.txt`, with
//! the colour `PyMuPDF` 1.28.2 drew at the centre of each cell of a grid over the image.
//!
//! In `short.txt`, each image's colour, soft mask, mask stream, colour key or `/Matte`
//! has its data cut short. The image keeps its declared grid and a mask lies on it; a
//! sample the data does not hold reads as stored zero, past every byte held of 8-bit
//! samples and past the whole rows held of other depths; under a colour key or
//! `/Matte`, the rows not held whole are transparent. A colour key with fewer entries
//! than two per component is no mask. CCITT data that breaks off within a row keeps
//! the pixels decoded of it, the rest white.
//!
//! In `decode.txt`, the decode arrays of images, soft masks, mask streams and stencil
//! masks are short, empty, not arrays, hold non-numbers, or map 0 and 1 to values that
//! do not sum to one: an entry missing or not a number reads as 0, and a stencil
//! mask's sample is inverted before it is mapped.

use super::differential::assert_streams_as_whole;
use super::tests::{asset, pdf};
use super::{ImageAsset, ImageRequest, PixelFormat, Plane};

/// The colour of the page under the image.
const PAGE: [f64; 3] = [255.0, 0.0, 0.0];
/// The colour a stencil mask paints.
const PAINT: [f64; 3] = [0.0, 0.0, 255.0];

struct Case {
    name: String,
    grid: (u32, u32),
    /// The first grid row `expected` holds.
    first_row: usize,
    /// The image's dictionary and stored data, then its mask's, if any.
    objects: Vec<(String, Vec<u8>)>,
    expected: Vec<Vec<String>>,
}

fn cases(text: &str) -> Vec<Case> {
    let mut cases: Vec<Case> = Vec::new();
    let mut dict = String::new();
    for line in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        let case = cases.last_mut();
        match key {
            "case" => cases.push(Case {
                name: value.to_string(),
                grid: (0, 0),
                first_row: 0,
                objects: Vec::new(),
                expected: Vec::new(),
            }),
            "grid" => {
                let (w, h) = value.split_once(' ').unwrap();
                case.unwrap().grid = (w.parse().unwrap(), h.parse().unwrap());
            }
            "rows" => {
                let (first, _) = value.split_once(' ').unwrap();
                case.unwrap().first_row = first.parse().unwrap();
            }
            "image" | "mask" => dict = format!("/Type /XObject /Subtype /Image {value}"),
            "data" => {
                let data = (0..value.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&value[i..i + 2], 16).unwrap())
                    .collect();
                case.unwrap()
                    .objects
                    .push((std::mem::take(&mut dict), data));
            }
            "expect" => case
                .unwrap()
                .expected
                .push(value.split(' ').map(str::to_string).collect()),
            _ => panic!("unknown line {line}"),
        }
    }
    cases
}

/// A plane's texels at full size, with their format and the size of a texel in the
/// image's grid.
struct Texels {
    data: Vec<u8>,
    format: PixelFormat,
    width: u32,
    height: u32,
    to_image: (f64, f64),
}

impl Texels {
    fn of(asset: &ImageAsset, plane: Plane) -> Option<Self> {
        let mut source = asset.open(None, None).ok()?;
        let layout = *source.layout().plane(plane)?;
        let (w, h) = (layout.width, layout.height);
        let region = source
            .request(&ImageRequest {
                plane,
                grid: (w, h),
                window: [0, 0, w, h],
                cancel: None,
            })
            .ok()?;
        Some(Self {
            data: region.data,
            format: region.format,
            width: w,
            height: h,
            to_image: layout.to_image,
        })
    }

    /// The texel at `(x, y)` of the image's grid: premultiplied colour, 0 to 1, and
    /// alpha.
    fn at(&self, (x, y): (f64, f64)) -> ([f64; 3], f64) {
        let col = ((x / self.to_image.0) as u32).min(self.width - 1);
        let row = ((y / self.to_image.1) as u32).min(self.height - 1);
        let c = self.format.channels();
        let t = &self.data[(row * self.width + col) as usize * c..][..c];
        let v = |i: usize| f64::from(t[i]) / 255.0;
        match self.format {
            PixelFormat::Luma => ([v(0); 3], 1.0),
            PixelFormat::LumaAlpha => ([v(0); 3], v(1)),
            PixelFormat::Rgb => ([v(0), v(1), v(2)], 1.0),
            PixelFormat::RgbAlpha => ([v(0), v(1), v(2)], v(3)),
            PixelFormat::Alpha => (PAINT.map(|p| p / 255.0 * v(0)), v(0)),
        }
    }
}

/// The colour drawn over the page at the centre of each cell of `grid` over the image:
/// nothing where opening or requesting the image fails.
fn drawn(asset: &ImageAsset, grid: (u32, u32)) -> Vec<Vec<[u8; 3]>> {
    let image = Texels::of(asset, Plane::Image);
    let mask = Texels::of(asset, Plane::Mask);
    let (w, h) = (f64::from(asset.width()), f64::from(asset.height()));
    (0..grid.1)
        .map(|r| {
            (0..grid.0)
                .map(|c| {
                    let at = (
                        (f64::from(c) + 0.5) * w / f64::from(grid.0),
                        (f64::from(r) + 0.5) * h / f64::from(grid.1),
                    );
                    let (colour, alpha) = image.as_ref().map_or(([0.0; 3], 0.0), |i| i.at(at));
                    let m = mask.as_ref().map_or(1.0, |m| m.at(at).1);
                    let mut out = [0; 3];
                    for ((o, c), p) in out.iter_mut().zip(colour).zip(PAGE) {
                        *o = (c * m * 255.0 + p * (1.0 - alpha * m)).round() as u8;
                    }
                    out
                })
                .collect()
        })
        .collect()
}

/// The colour a token of `short.txt` names; `None` for `*`.
fn colour(token: &str) -> Option<[u8; 3]> {
    Some(match token {
        "*" => return None,
        "R" => [255, 0, 0],
        "K" => [0, 0, 0],
        "W" => [255, 255, 255],
        "G" => [0, 255, 0],
        "B" => [0, 0, 255],
        hex => [0, 2, 4].map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()),
    })
}

/// The most a channel may differ from `MuPDF`'s, which rounds compositing its own way.
const TOLERANCE: u8 = 3;

/// The cases of `text`, `count` of them, draw as `MuPDF` drew them, streamed as decoded
/// whole.
fn assert_draw_as_mupdf(text: &str, count: usize) {
    let cases = cases(text);
    assert_eq!(cases.len(), count);
    let mut failed = Vec::new();
    for case in &cases {
        let pdf = pdf(&case.objects);
        let asset = asset(&pdf);
        assert_streams_as_whole(&asset, &case.name);
        let drawn = &drawn(&asset, case.grid)[case.first_row..];
        let differs = case.expected.iter().zip(drawn).any(|(expected, drawn)| {
            expected.iter().zip(drawn).any(|(token, got)| {
                colour(token).is_some_and(|want| {
                    want.iter()
                        .zip(got)
                        .any(|(a, b)| a.abs_diff(*b) > TOLERANCE)
                })
            })
        });
        if differs {
            let rows: Vec<String> = drawn
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|c| format!("{:02x}{:02x}{:02x}", c[0], c[1], c[2]))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect();
            failed.push(format!(
                "{}: expected\n  {}\ndrawn\n  {}",
                case.name,
                case.expected
                    .iter()
                    .map(|r| r.join(" "))
                    .collect::<Vec<_>>()
                    .join("\n  "),
                rows.join("\n  ")
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} cases differ:\n{}",
        failed.len(),
        cases.len(),
        failed.join("\n")
    );
}

#[test]
fn images_whose_data_ends_early_draw_as_mupdf_draws_them() {
    assert_draw_as_mupdf(include_str!("short.txt"), 92);
}

#[test]
fn decode_arrays_draw_as_mupdf_draws_them() {
    assert_draw_as_mupdf(include_str!("decode.txt"), 55);
}
