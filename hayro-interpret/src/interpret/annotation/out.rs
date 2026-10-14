//! The content stream of a generated appearance. Generators write it only through
//! [`Out`], whose text they cannot reach: operators are fixed text, and every number,
//! a coordinate or a colour component, is converted by [`written`], so that one that
//! cannot be written fails the whole appearance instead of reaching the content.

use super::Unsupported;
use std::fmt::Write;

/// `v` as it is written in content: a 32-bit number, the range content streams are
/// read in. `None` beyond that range, or for no number. Every number of a generated
/// appearance and of a [`super::Placement`] is converted here.
fn written(v: f64) -> Option<f32> {
    let n = v as f32;
    n.is_finite().then_some(n)
}

/// [`written`] of each of `v`; [`Unsupported::Coordinates`] when one cannot be.
pub(super) fn written_all<const L: usize>(v: [f64; L]) -> Result<[f32; L], Unsupported> {
    let mut n = [0.0; L];
    for (n, v) in n.iter_mut().zip(v) {
        *n = written(v).ok_or(Unsupported::Coordinates)?;
    }
    Ok(n)
}

/// A colour (PDF 32000-1 8.6.4): gray, RGB or CMYK components, each clamped to 0–1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Color {
    Gray(f64),
    Rgb([f64; 3]),
    Cmyk([f64; 4]),
}

impl Color {
    pub(super) const BLACK: Self = Self::Gray(0.0);

    /// The colour of `components`, as viewers read a colour array: gray for one, RGB
    /// for three, CMYK of the first four for four or more; `None` for none
    /// (transparent) or two.
    pub(super) fn of(components: &[f64]) -> Option<Self> {
        let c = |i: usize| components[i].clamp(0.0, 1.0);
        match components.len() {
            1 => Some(Self::Gray(c(0))),
            3 => Some(Self::Rgb([c(0), c(1), c(2)])),
            4.. => Some(Self::Cmyk([c(0), c(1), c(2), c(3)])),
            _ => None,
        }
    }

    /// Each component through `f`, clamped.
    pub(super) fn map(self, f: impl Fn(f64) -> f64) -> Self {
        let f = |c: f64| f(c).clamp(0.0, 1.0);
        match self {
            Self::Gray(g) => Self::Gray(f(g)),
            Self::Rgb(c) => Self::Rgb(c.map(f)),
            Self::Cmyk(c) => Self::Cmyk(c.map(f)),
        }
    }

    fn components(&self) -> &[f64] {
        match self {
            Self::Gray(g) => std::slice::from_ref(g),
            Self::Rgb(c) => c,
            Self::Cmyk(c) => c,
        }
    }
}

/// Content stream text. Numbers reach it only through [`written`]; one that cannot
/// be written fails the whole appearance, whatever the generator does next.
#[derive(Default)]
pub(super) struct Out {
    content: String,
    failed: bool,
}

impl Out {
    /// The content written; [`Unsupported::Coordinates`] when a number could not be.
    pub(super) fn finish(self) -> Result<String, Unsupported> {
        if self.failed {
            Err(Unsupported::Coordinates)
        } else {
            Ok(self.content)
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

    /// `path` as [`Out::path`] writes it, unless it lies wholly outside `clip`, where
    /// nothing of it would show; whether it was written. Its numbers pass [`written`]
    /// either way, so that clipping a path away never hides one that cannot be
    /// written.
    pub(super) fn path_within(&mut self, path: &kurbo::BezPath, clip: kurbo::Rect) -> Option<bool> {
        use kurbo::PathEl;
        for el in path.elements() {
            let pts: &[kurbo::Point] = match el {
                PathEl::MoveTo(p) | PathEl::LineTo(p) => &[*p],
                PathEl::QuadTo(c, p) => &[*c, *p],
                PathEl::CurveTo(a, b, p) => &[*a, *b, *p],
                PathEl::ClosePath => &[],
            };
            if pts
                .iter()
                .any(|p| written(p.x).is_none() || written(p.y).is_none())
            {
                self.failed = true;
                return None;
            }
        }
        let (b, c) = (path.control_box(), clip.abs());
        if b.x1 >= c.x0 && b.x0 <= c.x1 && b.y1 >= c.y0 && b.y0 <= c.y1 {
            self.path(path)?;
            Some(true)
        } else {
            Some(false)
        }
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
        self.nums(color.components(), op)
    }

    /// `color` as the stroke colour.
    pub(super) fn stroke_color(&mut self, color: &Color) -> Option<()> {
        let op = match color {
            Color::Gray(_) => "G",
            Color::Rgb(_) => "RG",
            Color::Cmyk(_) => "K",
        };
        self.nums(color.components(), op)
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
