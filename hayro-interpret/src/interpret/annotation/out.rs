//! The content stream of a generated appearance. Generators write it only through
//! [`Out`], whose text they cannot reach: operators are fixed text, and every number,
//! a coordinate or a colour component, is converted by [`written`], so that one that
//! cannot be written fails the whole appearance instead of reaching the content.
//! A glyph is written once, as a form the content draws wherever the glyph is
//! ([`Out::form`]).

use super::{Glyph, Unsupported};
use rustc_hash::FxHashSet;
use std::fmt::Write;
use std::rc::Rc;

/// `v` as it is written in content: a 32-bit number, the precision content streams
/// are read in. `None` for no number, or from 2^23 on, where 32-bit numbers are a
/// unit or more apart, so a viewer computing the appearance in them rounds its
/// points apart from ours (observed: a circle whose border is 2^24 wide collapses to
/// one point in `MuPDF` and is drawn). Every number of a generated appearance and of a
/// [`super::Placement`] is converted here.
fn written(v: f64) -> Option<f32> {
    let n = v as f32;
    (n.abs() < 8_388_608.0).then_some(n)
}

/// [`written`] of each of `v`; [`Unsupported::Coordinates`] when one cannot be.
pub(super) fn written_all<const L: usize>(v: [f64; L]) -> Result<[f32; L], Unsupported> {
    let mut n = [0.0; L];
    for (n, v) in n.iter_mut().zip(v) {
        *n = written(v).ok_or(Unsupported::Coordinates)?;
    }
    Ok(n)
}

/// A colour (PDF 32000-1 8.6.4): gray, RGB or CMYK components as given, each
/// clamped to 0–1 where it is written, as renderers clamp it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Color {
    Gray(f64),
    Rgb([f64; 3]),
    Cmyk([f64; 4]),
}

impl Color {
    pub(super) const BLACK: Self = Self::Gray(0.0);

    /// The colour of `components`, as viewers read a colour array: gray of the first
    /// for one or two, RGB for three, CMYK of the first four for four or more; `None`
    /// for none (transparent).
    pub(super) fn of(components: &[f64]) -> Option<Self> {
        let c = |i: usize| components[i];
        match components.len() {
            0 => None,
            1 | 2 => Some(Self::Gray(c(0))),
            3 => Some(Self::Rgb([c(0), c(1), c(2)])),
            4.. => Some(Self::Cmyk([c(0), c(1), c(2), c(3)])),
        }
    }

    /// The same colour in RGB, as viewers convert it for a push button: CMYK by
    /// `1 - min(1, c + k)`, unclamped below.
    pub(super) fn rgb(self) -> Self {
        match self {
            Self::Gray(g) => Self::Rgb([g; 3]),
            Self::Rgb(_) => self,
            Self::Cmyk([c, m, y, k]) => Self::Rgb([c, m, y].map(|v| 1.0 - (v + k).min(1.0))),
        }
    }

    /// The luminance of the colour in RGB, unclamped.
    pub(super) fn luminance(self) -> f64 {
        match self.rgb() {
            Self::Rgb([r, g, b]) => 0.3 * r + 0.59 * g + 0.11 * b,
            Self::Gray(g) => g,
            Self::Cmyk(_) => 0.0,
        }
    }

    /// Each component through `f`.
    pub(super) fn map(self, f: impl Fn(f64) -> f64) -> Self {
        match self {
            Self::Gray(g) => Self::Gray(f(g)),
            Self::Rgb(c) => Self::Rgb(c.map(f)),
            Self::Cmyk(c) => Self::Cmyk(c.map(f)),
        }
    }

    /// The components, clamped to 0–1.
    fn components(&self) -> Vec<f64> {
        let clamped = |c: &[f64]| c.iter().map(|c| c.clamp(0.0, 1.0)).collect();
        match self {
            Self::Gray(g) => clamped(&[*g]),
            Self::Rgb(c) => clamped(c),
            Self::Cmyk(c) => clamped(c),
        }
    }
}

/// Content stream text. Numbers reach it only through [`written`]; one that cannot
/// be written fails the whole appearance, whatever the generator does next.
#[derive(Default)]
pub(super) struct Out {
    content: String,
    failed: bool,
    /// The glyphs drawn, each once, and their names.
    glyphs: Vec<Rc<Glyph>>,
    named: FxHashSet<Rc<str>>,
}

impl Out {
    /// The content written and the glyphs it draws; [`Unsupported::Coordinates`]
    /// when a number could not be written.
    pub(super) fn finish(self) -> Result<(String, Vec<Rc<Glyph>>), Unsupported> {
        if self.failed {
            Err(Unsupported::Coordinates)
        } else {
            Ok((self.content, self.glyphs))
        }
    }

    /// Draws the form of `glyph` where the current transform puts it ([`super::text::Pen`]).
    pub(super) fn form(&mut self, glyph: &Rc<Glyph>) {
        self.content.push('/');
        self.content.push_str(&glyph.name);
        self.op(" Do");
        if self.named.insert(glyph.name.clone()) {
            self.glyphs.push(glyph.clone());
        }
    }

    fn num(&mut self, v: f64) -> Option<()> {
        let Some(n) = written(v) else {
            self.failed = true;
            return None;
        };
        write!(self.content, "{n} ").ok()
    }

    /// `nums`, then operator `op`.
    pub(super) fn nums(&mut self, nums: &[f64], op: &'static str) -> Option<()> {
        for n in nums {
            self.num(*n)?;
        }
        self.op(op);
        Some(())
    }

    /// `path` as path construction operators, to a thousandth of a point.
    pub(super) fn path(&mut self, path: &kurbo::BezPath) -> Option<()> {
        use kurbo::PathEl;
        let mut last = kurbo::Point::ZERO;
        for el in path.elements() {
            let pts: &[kurbo::Point] = match el {
                PathEl::MoveTo(p) | PathEl::LineTo(p) => &[*p],
                PathEl::QuadTo(c, p) => {
                    // The cubic of the same curve.
                    let c1 = last + (*c - last) * (2.0 / 3.0);
                    let c2 = *p + (*c - *p) * (2.0 / 3.0);
                    &[c1, c2, *p]
                }
                PathEl::CurveTo(a, b, p) => &[*a, *b, *p],
                PathEl::ClosePath => {
                    self.op("h");
                    continue;
                }
            };
            for p in pts {
                self.num(round(p.x))?;
                self.num(round(p.y))?;
            }
            self.op(match el {
                PathEl::MoveTo(_) => "m",
                PathEl::LineTo(_) => "l",
                _ => "c",
            });
            last = *pts.last()?;
        }
        Some(())
    }

    /// Whether `ink`, a box around points, after `at` can reach `clip`; `None`,
    /// failing the content, when a corner cannot be written, which a point inside it
    /// then may not either.
    pub(super) fn box_reaches(
        &mut self,
        at: kurbo::Affine,
        ink: kurbo::Rect,
        clip: kurbo::Rect,
    ) -> Option<bool> {
        let corners = [
            (ink.x0, ink.y0),
            (ink.x1, ink.y0),
            (ink.x0, ink.y1),
            (ink.x1, ink.y1),
        ]
        .map(|(x, y)| at * kurbo::Point::new(x, y));
        let b = corners
            .iter()
            .fold(kurbo::Rect::from_points(corners[0], corners[0]), |r, &p| {
                r.union_pt(p)
            });
        self.within(b)?;
        let c = clip.abs();
        Some(b.x1 >= c.x0 && b.x0 <= c.x1 && b.y1 >= c.y0 && b.y0 <= c.y1)
    }

    /// `None`, failing the content, when a corner of `area` cannot be written.
    pub(super) fn within(&mut self, area: kurbo::Rect) -> Option<()> {
        let ok = [area.x0, area.y0, area.x1, area.y1]
            .iter()
            .all(|&v| written(v).is_some());
        if !ok {
            self.failed = true;
        }
        ok.then_some(())
    }

    /// Operator `op`, without operands.
    pub(super) fn op(&mut self, op: &'static str) {
        self.content.push_str(op);
        self.content.push('\n');
    }

    /// `color` as the fill colour.
    pub(super) fn fill_color(&mut self, color: &Color) -> Option<()> {
        let op = match color {
            Color::Gray(_) => "g",
            Color::Rgb(_) => "rg",
            Color::Cmyk(_) => "k",
        };
        self.nums(&color.components(), op)
    }

    /// `color` as the stroke colour.
    pub(super) fn stroke_color(&mut self, color: &Color) -> Option<()> {
        let op = match color {
            Color::Gray(_) => "G",
            Color::Rgb(_) => "RG",
            Color::Cmyk(_) => "K",
        };
        self.nums(&color.components(), op)
    }

    /// The stroke state every generated appearance starts from.
    pub(super) fn stroke_state(&mut self, width: f64, dash: &[f64]) -> Option<()> {
        self.op("0 J 0 j 10 M");
        self.nums(&[width], "w")?;
        self.content.push('[');
        for d in dash {
            self.num(*d)?;
        }
        self.op("] 0 d");
        Some(())
    }

    /// Clips what follows to `r`.
    pub(super) fn clip(&mut self, r: kurbo::Rect) -> Option<()> {
        self.nums(&[r.x0, r.y0, r.width(), r.height()], "re")?;
        self.op("W n");
        Some(())
    }
}

/// `v` to a thousandth, without the trailing zeros.
fn round(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0 + 0.0
}
