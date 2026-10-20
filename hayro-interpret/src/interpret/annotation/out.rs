//! The content stream of a generated appearance. Generators write it only through
//! [`Out`], whose text they cannot reach: operators are fixed text, and every number,
//! a coordinate or a colour component, is converted by [`written`], so that one that
//! cannot be written fails the whole appearance instead of reaching the content.
//! A glyph is written once, as a form the content draws wherever the glyph is
//! ([`Out::form`]). What cannot paint where the page shows it is not written
//! ([`Out`], [`Region`]), and a part of an appearance its generator cannot draw as
//! viewers draw it refuses the appearance only where it may show under the clips it
//! is drawn under, which are written before it is told: a generator fails only with
//! [`Refused`], which only [`Out`] makes ([`Out::refuse`]).

use super::{Glyph, Unsupported};
use hayro_syntax::hash;
use kurbo::{Affine, Point, Vec2};
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

/// `v` as a viewer reads it where it is written: the 32-bit number written for it
/// (whether or not [`written`] lets it be written).
pub(super) fn as_written(v: f64) -> f64 {
    f64::from(v as f32)
}

/// [`written`] of each of `v`; [`Unsupported::Coordinates`] when one cannot be.
pub(super) fn written_all<const L: usize>(v: [f64; L]) -> Result<[f32; L], Unsupported> {
    let mut n = [0.0; L];
    for (n, v) in n.iter_mut().zip(v) {
        *n = written(v).ok_or(Unsupported::Coordinates)?;
    }
    Ok(n)
}

/// A generated appearance refused, which is all a generator fails with: made only
/// by [`Out`], once it has recorded why, which [`Out::finish`] tells. [`Out::refuse`]
/// makes one only where the part that cannot be drawn may show where the page shows
/// it; [`Out::wrote`], where a piece that may show could not be written.
#[derive(Debug)]
#[must_use]
pub(super) struct Refused(());

/// Where a part of an appearance may paint that its generator cannot draw as viewers
/// draw it ([`Out::refuse`]), under the clips written so far: a part is told only
/// where all the clips it is drawn under are written, as it carries none of its own.
pub(super) enum Part {
    /// Within `reach` of the convex hull of `points`, of the current space, each
    /// computed as `local` says ([`Rounding`]).
    Near {
        points: Vec<Point>,
        reach: f64,
        local: Rounding,
    },
    /// Anywhere the page shows something: no entry the generator reads bounds
    /// where viewers draw it.
    Anywhere,
}

impl Part {
    /// Within `reach` of the convex hull of `points`, each where it lies.
    pub(super) fn near(points: Vec<Point>, reach: f64) -> Self {
        Self::Near {
            points,
            reach,
            local: Rounding::NONE,
        }
    }

    /// In box `r`.
    pub(super) fn rect(r: kurbo::Rect) -> Self {
        Self::near(corners(r).to_vec(), 0.0)
    }

    /// This part, its points each computed as `local` says.
    pub(super) fn computed(self, local: Rounding) -> Self {
        match self {
            Self::Near { points, reach, .. } => Self::Near {
                points,
                reach,
                local,
            },
            Self::Anywhere => Self::Anywhere,
        }
    }
}

/// The corners of `r`, counterclockwise from its bottom left.
pub(super) fn corners(r: kurbo::Rect) -> [Point; 4] {
    [(r.x0, r.y0), (r.x1, r.y0), (r.x1, r.y1), (r.x0, r.y1)].map(|(x, y)| Point::new(x, y))
}

/// How far beyond its outline a stroke `width` wide paints: half the width times
/// the miter limit, or the half-diagonal of a square cap's corner where that is
/// more.
pub(super) fn stroke_reach(width: f64) -> f64 {
    width.abs() / 2.0 * MITER_LIMIT.max(std::f64::consts::SQRT_2)
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

/// The miter limit of every stroke ([`Out::stroke_state`]): a join reaches at most
/// half of it times the line width from its corner, a bevel where a miter would
/// reach further.
pub(super) const MITER_LIMIT: f64 = 10.0;

/// What the page shows of the content's current space, and how far beyond its
/// outline a piece of it paints there: the area the page shows, cut by every clip
/// in force as it is written (each number as a viewer reads it, in the space the
/// transforms written put it in), grown on the page by `grow` across and up. A
/// clip that leaves it no area, a line or a point, leaves nothing: what it shows
/// is empty however close a piece comes. Made only by [`Out::region`], from what
/// the content wrote; the generators that choose pieces test them against it, and
/// [`Out`] tests every painting operator against it, each piece wherever a viewer
/// computing its place in 32-bit numbers may put it ([`Rounding`]).
#[derive(Clone, Debug)]
pub(super) struct Region {
    /// The part shown, on the page: a convex polygon of some area, counterclockwise,
    /// or none; `None` where no page bounds it.
    shown: Option<Vec<Point>>,
    /// Half the side of the square a piece paints beyond its outline, on the page.
    grow: f64,
    /// How far from where it lies a viewer may draw a piece that is drawn, on the
    /// page: one that may lie farther cannot be drawn where `bake` draws it.
    tolerance: f64,
    /// From the current space to the page, as written.
    ctm: Affine,
    /// The same transforms composed with each coefficient's magnitude: of a point
    /// `p`, `abs * |p|` bounds the magnitudes summed in computing `ctm * p`.
    abs: Affine,
    /// What a viewer composing the transforms written in 32-bit numbers rounds
    /// ([`Rounding`]): of each coefficient, the magnitudes its roundings round,
    /// summed, carried through the transforms after them, so that `chain * |p|`
    /// sums those of `p`'s place; and the most roundings one passes through.
    chain: Affine,
    chain_count: f64,
}

/// u = 2⁻²⁴, the most a 32-bit number rounds a value by, relative to it.
const U: f64 = 5.960_464_477_539_063e-8;

/// How far a viewer computing a point in 32-bit arithmetic, in any order, may put
/// it from its value: for each coordinate, across and up, the roundings it takes,
/// `count` along the longest chain of them, and the magnitudes they round,
/// `summed`. Each rounding moves what it rounds by at most u of it, and after n
/// roundings that is at most 1/(1 − n u) of what it is computed exactly from
/// magnitudes, so the error is at most u · `summed` / (1 − u `count`)
/// ([`Rounding::bound`]; Higham, *Accuracy and Stability of Numerical Algorithms*,
/// 3.1–3.5): of `count` roundings of a value each at most M, the forward bound
/// γₙ M.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Rounding {
    count: Vec2,
    summed: Vec2,
}

impl Rounding {
    /// No rounding: a number as written.
    pub(super) const NONE: Self = Self {
        count: Vec2::ZERO,
        summed: Vec2::ZERO,
    };

    /// `count` roundings, each of a value at most `most` across.
    pub(super) fn of(count: f64, most: f64) -> Self {
        Self {
            count: Vec2::new(count, 0.0),
            summed: Vec2::new(count * most.abs(), 0.0),
        }
    }

    /// A sum across of `lead` and `n` terms, each at most `most` and `total` in all,
    /// in magnitude, each term itself computed in `per_term` roundings of a value
    /// no larger than it: in whatever order, or tree, a viewer adds them. Each
    /// addition rounds a partial sum, at most the magnitudes of the terms under it,
    /// so the roundings sum each term's magnitude as many times as it is added
    /// into: most where the largest are added first, one at a time; the `j`
    /// largest are at most `lead + min(total, j most)`.
    pub(super) fn sum(lead: f64, n: f64, most: f64, total: f64, per_term: f64) -> Self {
        let (lead, most, total) = (lead.abs(), most.abs(), total.abs());
        // Adding terms that are all 0 rounds nothing.
        if n <= 0.0 || most == 0.0 {
            return Self::NONE;
        }
        // Σ_{j=2}^{n+1} min(total, j most): j most up to q, total after.
        let partial = if most > 0.0 && total.is_finite() {
            let q = (total / most).floor().clamp(1.0, n + 1.0);
            most * (q * (q + 1.0) / 2.0 - 1.0) + (n + 1.0 - q) * total
        } else {
            n * total
        };
        Self {
            count: Vec2::new(n + per_term, 0.0),
            summed: Vec2::new(n * lead + partial + per_term * total, 0.0),
        }
    }

    /// Both: a number computed from two so computed.
    pub(super) fn plus(self, other: Self) -> Self {
        Self {
            count: self.count + other.count,
            summed: self.summed + other.summed,
        }
    }

    /// Of a point, `x` across and `y` up, each computed as a [`Rounding`] across.
    pub(super) fn point(x: Self, y: Self) -> Self {
        Self {
            count: Vec2::new(x.count.x, y.count.x),
            summed: Vec2::new(x.summed.x, y.summed.x),
        }
    }

    /// The larger of two, coordinate by coordinate: for points each computed as
    /// one of them says.
    fn either(self, other: Self) -> Self {
        let max = |a: Vec2, b: Vec2| Vec2::new(a.x.max(b.x), a.y.max(b.y));
        Self {
            count: max(self.count, other.count),
            summed: max(self.summed, other.summed),
        }
    }

    /// In a space `k` times as large.
    pub(super) fn scaled(self, k: f64) -> Self {
        Self {
            count: self.count,
            summed: self.summed * k.abs(),
        }
    }

    /// The most the point may lie from its value, across and up: u · `summed` /
    /// (1 − u `count`); none where no rounding rounds anything but 0.
    pub(super) fn bound(self) -> Vec2 {
        let one = |count: f64, summed: f64| {
            let left = 1.0 - U * count;
            if summed == 0.0 {
                0.0
            } else if left > 0.0 {
                summed * (U / left)
            } else {
                f64::INFINITY
            }
        };
        Vec2::new(
            one(self.count.x, self.summed.x),
            one(self.count.y, self.summed.y),
        )
    }
}

/// `k v`, 0 for `k` 0 whatever `v`: a coefficient of 0 carries nothing, however
/// uncertain.
fn times(k: f64, v: f64) -> f64 {
    if k == 0.0 { 0.0 } else { k * v }
}

/// How many roundings a viewer composing `c` then `m` takes for each coefficient,
/// whatever `c` holds: a product rounds unless its factor of `m` is 0 or ±1, and
/// each term of `m`'s factor not 0, and the translation, add one but the first.
fn composed_roundings(m: [f64; 6]) -> [f64; 6] {
    let [a, b, c, d, e, f] = m;
    let terms = |factors: &[f64], translation: bool| {
        let nonzero = factors.iter().filter(|v| **v != 0.0).count() + usize::from(translation);
        let products = factors
            .iter()
            .filter(|v| **v != 0.0 && v.abs() != 1.0)
            .count();
        (products + nonzero.saturating_sub(1)) as f64
    };
    [
        terms(&[a, b], false),
        terms(&[a, b], false),
        terms(&[c, d], false),
        terms(&[c, d], false),
        terms(&[e, f], true),
        terms(&[e, f], true),
    ]
}

/// How far a drawn piece may lie from where a viewer computing in 32-bit numbers
/// draws it before a page compared at 2 px/pt shows it apart: 1 pt, the 2 px within
/// which `annot_diff`'s comparison finds a pixel's ink (`LOSS_RADIUS` at `SCALE`).
/// A piece whose place is less certain is refused ([`Out`]).
pub(super) const TOLERANCE: f64 = 1.0;

/// How many roundings, each of at most the magnitudes a point's place sums or the
/// page's, a viewer takes in computing the place of a point it reads: the place is
/// a product of at most 7 affine factors (`/Matrix`, the placement, the page's
/// translation, turn and flip, `/UserUnit`, the device scale) and the point, each
/// product summing 3 terms: 21 roundings, and 1 to the rasterizer's grid. The
/// placement's entries the viewer computes itself from `/Rect` and the box under
/// `/Matrix`, in at most 8 roundings each, its translation of two terms as large as
/// the sum: 16 more, 38 in all. The transforms generated content writes add their
/// own ([`Region::chain`]).
const VIEWER_ROUNDINGS: f64 = 40.0;

/// The axes across x and up.
const AXES: [Vec2; 2] = [Vec2::new(1.0, 0.0), Vec2::new(0.0, 1.0)];

impl Region {
    /// Whether nothing shows.
    pub(super) fn is_empty(&self) -> bool {
        self.shown.as_ref().is_some_and(Vec::is_empty)
    }

    /// The normals of the sides of what shows grown: of its own sides and of the
    /// square's.
    fn axes(shown: &[Point]) -> impl Iterator<Item = Vec2> + '_ {
        (0..shown.len())
            .map(move |i| (shown[(i + 1) % shown.len()] - shown[i]).turn_90())
            .chain(AXES)
    }

    /// What shows grown, along `n`.
    fn extent(&self, shown: &[Point], n: Vec2) -> (f64, f64) {
        let (lo, hi) = along(shown.iter().copied(), n);
        let g = self.grow * (n.x.abs() + n.y.abs());
        (lo - g, hi + g)
    }

    /// `points`, of the current space, on the page.
    fn on_page(&self, points: &[Point]) -> Vec<Point> {
        points.iter().map(|p| self.ctm * *p).collect()
    }

    /// Whether the convex hull of `points`, in the current space, may meet what
    /// shows grown wherever a viewer computing their places in 32-bit numbers puts
    /// them ([`Self::may_show`]). Also where a point is no finite number, whose
    /// place cannot be told (it then cannot be written either).
    pub(super) fn meets(&self, points: &[Point]) -> bool {
        self.near(points, 0.0)
    }

    /// `points`, of the current space, on the page as transformed further by
    /// `then` (each number as read), and how far, across and up, a viewer computing
    /// in 32-bit numbers may put each from there, each way ([`Rounding`]): of
    /// [`VIEWER_ROUNDINGS`] roundings each at most the magnitudes the transforms
    /// sum, or the page's, which its own transform to the device sums too, twice
    /// (its translation, and the device's offset into it); those composing the
    /// transforms written ([`Self::chain`]); and those of computing each point,
    /// `local`, carried to the page.
    fn as_any_viewer_computes(
        &self,
        then: &[[f32; 6]],
        points: &[Point],
        local: Rounding,
    ) -> (Vec<Point>, Vec2) {
        let (mut ctm, mut abs) = (self.ctm, self.abs);
        for m in then {
            let m = m.map(f64::from);
            ctm *= Affine::new(m);
            abs *= Affine::new(m.map(f64::abs));
        }
        let page = self
            .shown
            .as_deref()
            .unwrap_or_default()
            .iter()
            .fold(0.0_f64, |m, p| m.max(p.x.abs()).max(p.y.abs()));
        // Each coordinate on the page sums the magnitudes its row of the transforms
        // takes: a coefficient of 0 is one as a viewer computes it too, so that a
        // magnitude across moves no place up where no transform turns one into
        // the other.
        let (mut most, mut chained) = (Vec2::ZERO, Vec2::ZERO);
        for p in points {
            let p = Point::new(p.x.abs(), p.y.abs());
            let (s, c) = (abs * p, self.chain * p);
            most = Vec2::new(most.x.max(s.x), most.y.max(s.y));
            chained = Vec2::new(chained.x.max(c.x), chained.y.max(c.y));
        }
        let [a, b, c, d, _, _] = abs.as_coeffs();
        let l = local;
        let row = |own: f64, kx: f64, ky: f64| {
            let count = |k: f64, count: f64, summed: f64| {
                if times(k, summed) > 0.0 { count } else { 0.0 }
            };
            Rounding {
                count: Vec2::new(
                    VIEWER_ROUNDINGS
                        + self.chain_count
                        + count(kx, l.count.x, l.summed.x).max(count(ky, l.count.y, l.summed.y)),
                    0.0,
                ),
                summed: Vec2::new(own + times(kx, l.summed.x) + times(ky, l.summed.y), 0.0),
            }
        };
        let own = Vec2::new(most.x + 2.0 * page, most.y + 2.0 * page) * VIEWER_ROUNDINGS + chained;
        let r = Rounding::point(row(own.x, a, c), row(own.y, b, d)).bound();
        let r = Vec2::new(
            if r.x.is_nan() { f64::INFINITY } else { r.x },
            if r.y.is_nan() { f64::INFINITY } else { r.y },
        );
        let on_page = points.iter().map(|p| ctm * *p).collect();
        (on_page, r)
    }

    /// Whether the convex hull of `page`, points on the page each within `r.x`
    /// across and `r.y` up of where they lie and `reach` all round, meets what
    /// shows grown, `shown`: no side of either parts them.
    fn meets_on_page(&self, shown: &[Point], page: &[Point], reach: f64, r: Vec2) -> bool {
        Self::axes(shown).chain(sides(page)).all(|n| {
            let (g0, g1) = self.extent(shown, n);
            let (p0, p1) = along(page.iter().copied(), n);
            let r = reach * n.hypot() + times(n.x.abs(), r.x) + times(n.y.abs(), r.y);
            p1 + r >= g0 && p0 - r <= g1
        })
    }

    /// Whether what a form placed by `placement` paints (its box, clipped, after
    /// its matrix and transform, each number as a viewer reads it, whether or not
    /// it can be written) may meet what shows grown, wherever a viewer computing in
    /// 32-bit numbers puts it: false only where no such viewer can put any of it
    /// there. Also true where a number is none.
    pub(super) fn may_show_form(&self, placement: &super::Placement) -> bool {
        let Some(shown) = &self.shown else {
            return true;
        };
        if shown.is_empty() {
            return false;
        }
        let [x0, y0, x1, y1] = placement.bbox.map(f64::from);
        let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(|(x, y)| Point::new(x, y));
        let (page, r) = self.as_any_viewer_computes(
            &[placement.transform, placement.matrix],
            &corners,
            Rounding::NONE,
        );
        if !page.iter().all(|p| p.is_finite()) {
            return true;
        }
        self.meets_on_page(shown, &page, 0.0, r)
    }

    /// Transformed by `m`, which a `cm` writes: a viewer composes it with the
    /// transform before, rounding each coefficient as [`composed_roundings`]
    /// counts, each rounding at most the magnitudes that coefficient sums.
    fn transform(&mut self, m: Affine) {
        let abs_m = Affine::new(m.as_coeffs().map(f64::abs));
        let k = composed_roundings(m.as_coeffs());
        let rounded = (self.abs * abs_m).as_coeffs();
        let mut new = [0.0; 6];
        for i in 0..6 {
            new[i] = k[i] * rounded[i];
        }
        let carried = (self.chain * abs_m).as_coeffs();
        for i in 0..6 {
            new[i] += carried[i];
        }
        self.chain = Affine::new(new);
        self.chain_count += k.iter().fold(0.0_f64, |m, v| m.max(*v));
        self.ctm *= m;
        self.abs *= abs_m;
    }

    /// Whether a piece whose outline lies within `reach` (in the current space) of
    /// the convex hull of `points`, each computed as `local` says, may paint in
    /// what shows grown, wherever a viewer computing in 32-bit numbers puts it
    /// ([`Self::as_any_viewer_computes`]): `None` where no viewer can; else how far
    /// on the page, across or up, a viewer may put it from where it lies
    /// (infinite where a point is no finite number, whose place cannot be told).
    pub(super) fn may_show(&self, points: &[Point], reach: f64, local: Rounding) -> Option<f64> {
        let (page, r) = self.as_any_viewer_computes(&[], points, local);
        let Some(shown) = &self.shown else {
            return Some(r.x.max(r.y));
        };
        if shown.is_empty() || points.is_empty() {
            return None;
        }
        if !page.iter().all(|p| p.is_finite()) {
            return Some(f64::INFINITY);
        }
        let [a, b, c, d, _, _] = self.ctm.as_coeffs();
        // The most the transform stretches a length: no more than this.
        let reach = reach * (a * a + b * b + c * c + d * d).sqrt();
        self.meets_on_page(shown, &page, reach, r)
            .then_some(r.x.max(r.y))
    }

    /// How far, in the current space, a viewer computing in 32-bit numbers may put
    /// a point of magnitude at most `most` across and up, computed as `local` says,
    /// from where it lies: its distance on the page ([`Self::as_any_viewer_computes`])
    /// over the least the transform stretches a length. For a search to widen what
    /// it looks for by, so that it passes over no piece [`Self::may_show`] keeps.
    pub(super) fn slack(&self, most: f64, local: Rounding) -> f64 {
        let (_, r) = self.as_any_viewer_computes(&[], &[Point::new(most, most)], local);
        let r = r.x.max(r.y);
        let [a, b, c, d, _, _] = self.ctm.as_coeffs();
        // The least stretch: |det| over the most, which is at most the norm below.
        let least = (a * d - b * c).abs() / (a * a + b * b + c * c + d * d).sqrt();
        let s = r * std::f64::consts::SQRT_2 / least;
        if s.is_nan() { f64::INFINITY } else { s }
    }

    /// Whether the convex hull of `points`, in the current space, may come within
    /// `reach` of what shows grown, `reach` measured in the current space, wherever
    /// a viewer puts it ([`Self::may_show`]): true when it does; it may be true
    /// when it does not (a bound for passing over what cannot reach it).
    pub(super) fn near(&self, points: &[Point], reach: f64) -> bool {
        self.may_show(points, reach, Rounding::NONE).is_some()
    }

    /// Whether every point of the current space within `margin` of one of
    /// `points`, across and up, lies in what shows grown.
    pub(super) fn holds(&self, points: &[Point], margin: f64) -> bool {
        let Some(shown) = &self.shown else {
            return true;
        };
        if shown.is_empty() {
            return false;
        }
        let corners: Vec<Point> = points
            .iter()
            .flat_map(|p| {
                [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
                    .map(|(x, y)| *p + Vec2::new(x, y) * margin)
            })
            .collect();
        let page = self.on_page(&corners);
        page.iter().all(|p| p.is_finite())
            && Self::axes(shown).all(|n| {
                let (g0, g1) = self.extent(shown, n);
                page.iter().all(|p| (g0..=g1).contains(&n.dot(p.to_vec2())))
            })
    }

    /// The `u` for which the convex hull of `base` moved by `u dir`, in the current
    /// space, may meet what shows grown wherever a viewer puts it, `u` at most
    /// `far` either way ([`Self::may_show`]): a span, as both are convex; `None`
    /// for none. Along each side's normal the hull's extent moves with `u` and must
    /// meet that of what shows.
    pub(super) fn span(&self, base: &[Point], dir: Vec2, far: f64) -> Option<(f64, f64)> {
        let Some(shown) = &self.shown else {
            return Some((f64::NEG_INFINITY, f64::INFINITY));
        };
        if shown.is_empty() || base.is_empty() {
            return None;
        }
        let page = self.on_page(base);
        let reach: Vec<Point> = base
            .iter()
            .flat_map(|p| [*p + dir * far, *p - dir * far])
            .collect();
        let (_, r) = self.as_any_viewer_computes(&[], &reach, Rounding::NONE);
        let dir = self.ctm * Point::new(dir.x, dir.y) - self.ctm * Point::ZERO;
        let (mut lo, mut hi) = (f64::NEG_INFINITY, f64::INFINITY);
        for n in Self::axes(shown).chain(sides(&page)) {
            let (g0, g1) = self.extent(shown, n);
            let r = times(n.x.abs(), r.x) + times(n.y.abs(), r.y);
            let (g0, g1) = (g0 - r, g1 + r);
            let (p0, p1) = along(page.iter().copied(), n);
            let k = n.dot(dir);
            // They meet along `n` where `p1 + u k >= g0` and `p0 + u k <= g1`.
            if k == 0.0 {
                if p1 < g0 || p0 > g1 {
                    return None;
                }
            } else {
                let (a, b) = ((g0 - p1) / k, (g1 - p0) / k);
                lo = lo.max(a.min(b));
                hi = hi.min(a.max(b));
            }
        }
        (lo <= hi).then_some((lo, hi))
    }

    /// Cut by the convex polygon `clip`, on the page: nothing where what is left
    /// has no area.
    fn cut(&mut self, clip: &[Point]) {
        let shown = match self.shown.take() {
            None => {
                // No page to cut: what shows stays unbounded, but a clip of no
                // area leaves nothing.
                if area(&hull(clip.to_vec())) > 0.0 {
                    self.shown = None;
                } else {
                    self.shown = Some(vec![]);
                }
                return;
            }
            Some(shown) => shown,
        };
        let by = hull(clip.to_vec());
        let mut left = if area(&by) > 0.0 { shown } else { vec![] };
        for i in 0..by.len() {
            if left.is_empty() {
                break;
            }
            let (a, b) = (by[i], by[(i + 1) % by.len()]);
            let inside = |p: Point| (b - a).cross(p - a);
            let mut kept = Vec::with_capacity(left.len() + 1);
            for (j, &p) in left.iter().enumerate() {
                let q = left[(j + 1) % left.len()];
                let (dp, dq) = (inside(p), inside(q));
                if dp >= 0.0 {
                    kept.push(p);
                }
                if (dp >= 0.0) != (dq >= 0.0) {
                    kept.push(p.lerp(q, dp / (dp - dq)));
                }
            }
            left = kept;
        }
        self.shown = Some(if area(&left) > 0.0 { left } else { vec![] });
    }

    /// A box of the current space that holds what shows grown, wider by a margin
    /// that writing its numbers cannot take back; `None` where no page bounds it
    /// or the transform cannot be undone.
    fn bounds(&self) -> Option<kurbo::Rect> {
        let shown = self.shown.as_ref()?;
        let det = self.ctm.determinant();
        if !(det != 0.0 && det.is_finite()) {
            return None;
        }
        let back = self.ctm.inverse();
        let g = self.grow;
        let mut points = shown.iter().flat_map(|p| {
            [(-g, -g), (g, -g), (g, g), (-g, g)].map(|(x, y)| back * (*p + Vec2::new(x, y)))
        });
        let first = points.next()?;
        let b = points.fold(kurbo::Rect::from_points(first, first), |r, p| r.union_pt(p));
        let size = [b.x0, b.y0, b.x1, b.y1]
            .iter()
            .fold(1.0_f64, |m, v| m.max(v.abs()));
        let b = b.inset(1.0 + size * 1e-3);
        b.is_finite().then_some(b)
    }
}

/// The normals of the sides of the convex hull of `points`, every one finite:
/// with those of another convex polygon, the axes along which the two are apart
/// if they are apart at all.
fn sides(points: &[Point]) -> Vec<Vec2> {
    let h = hull(points.to_vec());
    match h.len() {
        0 | 1 => vec![],
        2 => vec![(h[1] - h[0]).turn_90()],
        n => (0..n).map(|i| (h[(i + 1) % n] - h[i]).turn_90()).collect(),
    }
}

/// The least and most of `points` along `n`.
fn along(points: impl Iterator<Item = Point>, n: Vec2) -> (f64, f64) {
    points.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
        let v = n.dot(p.to_vec2());
        (lo.min(v), hi.max(v))
    })
}

/// The signed area of the polygon `p`, positive counterclockwise.
fn area(p: &[Point]) -> f64 {
    (0..p.len())
        .map(|i| p[i].to_vec2().cross(p[(i + 1) % p.len()].to_vec2()))
        .sum::<f64>()
        / 2.0
}

/// The convex hull of `points`, counterclockwise.
fn hull(mut points: Vec<Point>) -> Vec<Point> {
    if !points.iter().all(|p| p.is_finite()) {
        return vec![];
    }
    points.sort_by(|p, q| p.x.total_cmp(&q.x).then(p.y.total_cmp(&q.y)));
    points.dedup();
    if points.len() < 3 {
        return points;
    }
    // Andrew's monotone chain.
    let mut hull: Vec<Point> = vec![];
    for pass in 0..2 {
        let start = hull.len();
        for &p in &points {
            while hull.len() >= start + 2
                && (hull[hull.len() - 1] - hull[hull.len() - 2]).cross(p - hull[hull.len() - 2])
                    <= 0.0
            {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
        if pass == 0 {
            points.reverse();
        }
    }
    hull
}

/// The graphics state the content has set: what shows of the current space, and
/// the line width.
#[derive(Clone, Debug)]
struct State {
    region: Region,
    width: f64,
}

/// A `q` not yet ended: the state it saves, where its text starts in what is held
/// back, and whether it has been written.
struct Frame {
    saved: State,
    held_at: usize,
    held_failed: bool,
    written: bool,
}

/// A subpath of the path being built: its text, its points and their box in the
/// current space, and whether a number of it cannot be written.
struct Subpath {
    text: String,
    /// Its finite points, control points included, whose hull holds it: a clip
    /// cuts what shows by that hull, not by the box, which a turned clip overfills.
    points: Vec<Point>,
    lo: Point,
    hi: Point,
    /// Whether a point of it is no finite number, whose place cannot be told.
    lost: bool,
    failed: bool,
    /// How its points were computed.
    rounding: Rounding,
}

impl Subpath {
    fn new() -> Self {
        Self {
            text: String::new(),
            points: vec![],
            lo: Point::new(f64::INFINITY, f64::INFINITY),
            hi: Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY),
            lost: false,
            failed: false,
            rounding: Rounding::NONE,
        }
    }

    fn add(&mut self, p: Point) {
        if p.is_finite() {
            self.points.push(p);
            self.lo = Point::new(self.lo.x.min(p.x), self.lo.y.min(p.y));
            self.hi = Point::new(self.hi.x.max(p.x), self.hi.y.max(p.y));
        } else {
            self.lost = true;
        }
    }

    /// The corners of its box grown by `reach`; no number where a point of it is
    /// none (such a box reaches everywhere), none where it has no point.
    fn corners(&self, reach: f64) -> Vec<Point> {
        if self.lost {
            return vec![Point::new(f64::NAN, f64::NAN)];
        }
        if self.lo.x.partial_cmp(&self.hi.x).is_none_or(|o| o.is_gt()) {
            return vec![];
        }
        let (lo, hi) = (
            self.lo - Vec2::new(reach, reach),
            self.hi + Vec2::new(reach, reach),
        );
        vec![lo, Point::new(hi.x, lo.y), hi, Point::new(lo.x, hi.y)]
    }
}

/// How an operator acts on the content: by its last word.
enum Kind {
    /// Starts a subpath (`m`, `re`) or adds to the current one (`l`, `c`, `v`,
    /// `y`, `h`).
    Starts,
    Adds,
    /// Paints the path: fills, strokes, or both; and whether it closes the last
    /// subpath first, the operator that does the same but that.
    Paints {
        stroke: bool,
        open: Option<&'static str>,
    },
    /// Ends the path unpainted.
    Ends,
    /// Clips to the path and ends it (`W n`).
    Clips,
    Save,
    Restore,
    Transforms,
    Width,
    /// Sets other state: colours, caps, joins, the miter limit, dashes.
    Sets,
}

impl Kind {
    fn of(op: &str) -> Option<Self> {
        if op == "W n" {
            return Some(Self::Clips);
        }
        Some(match op.rsplit(' ').next()? {
            "m" | "re" => Self::Starts,
            "l" | "c" | "v" | "y" | "h" => Self::Adds,
            "f" | "F" | "f*" => Self::Paints {
                stroke: false,
                open: op.starts_with("h ").then_some("f"),
            },
            "S" => Self::Paints {
                stroke: true,
                open: None,
            },
            "B" | "B*" => Self::Paints {
                stroke: true,
                open: None,
            },
            "s" => Self::Paints {
                stroke: true,
                open: Some("S"),
            },
            "b" => Self::Paints {
                stroke: true,
                open: Some("B"),
            },
            "b*" => Self::Paints {
                stroke: true,
                open: Some("B*"),
            },
            "n" => Self::Ends,
            "q" => Self::Save,
            "Q" => Self::Restore,
            "cm" => Self::Transforms,
            "w" => Self::Width,
            "J" | "j" | "M" | "d" | "g" | "G" | "rg" | "RG" | "k" | "K" => Self::Sets,
            _ => return None,
        })
    }
}

/// Content stream text. Numbers reach it only through [`written`]; one that cannot
/// be written fails the whole appearance where it is written, whatever the
/// generator does next.
///
/// Only what can paint where the page shows it is written: a painting operator
/// whose subpaths, each by the box of its points grown by how far its stroke can
/// reach (half the line width times the miter limit), or a glyph by its box, all
/// lie apart from what shows ([`Region`]) is left out with them, and so is each
/// such subpath of one that paints; the state set before it (`q`, `cm`, clips,
/// colours, widths) is held back until something painted needs it, and left out
/// where nothing does. What is left out is not written, so its numbers need not be
/// writable: an annotation far off the page, or the part of one, draws nothing
/// rather than failing. A clip is written cut to a box that holds what shows, which
/// cuts nothing that shows.
pub(super) struct Out {
    content: String,
    failed: bool,
    /// Why the appearance is refused, the first part refused that may show
    /// ([`Out::refuse`]).
    refused: Option<Unsupported>,
    /// The depth, in `q`s not yet ended, of the outermost frame in which a part was
    /// told left out or refused ([`Out::refuse`]) whose frame has not ended: a clip
    /// written then may be one that part is drawn under, which it was not told
    /// under ([`Out::clip`]).
    decided: Option<usize>,
    /// The glyphs drawn, each once, and their names.
    glyphs: Vec<Rc<Glyph>>,
    named: hash::Set<Rc<str>>,
    state: State,
    frames: Vec<Frame>,
    /// The state set since the last thing painted, held back.
    held: String,
    held_failed: bool,
    path: Vec<Subpath>,
}

impl Default for Out {
    /// Content no page bounds: everything it paints is written.
    fn default() -> Self {
        Self::with(None, 0.0, f64::INFINITY)
    }
}

impl Out {
    /// Content for a page that shows `visible`, in units of `unit` points: a piece
    /// paints [`super::PAINTS_BEYOND`] points beyond its outline there, and may be
    /// drawn [`TOLERANCE`] points from where a viewer draws it. A page of no unit,
    /// or of none that is a number, shows no piece more than what lies in it.
    pub(super) fn new(visible: kurbo::Rect, unit: f64) -> Self {
        let v = visible.abs();
        // An area of no size or of none that is a number shows nothing; one of
        // endless size is bounded far beyond any writable number.
        const FAR: f64 = 1e300;
        let clamp = |x: f64| x.clamp(-FAR, FAR);
        let shown = if v.x0.is_nan() || v.y0.is_nan() || v.x1.is_nan() || v.y1.is_nan() {
            vec![]
        } else {
            let (x0, y0, x1, y1) = (clamp(v.x0), clamp(v.y0), clamp(v.x1), clamp(v.y1));
            let p = vec![
                Point::new(x0, y0),
                Point::new(x1, y0),
                Point::new(x1, y1),
                Point::new(x0, y1),
            ];
            if area(&p) > 0.0 { p } else { vec![] }
        };
        let unit = unit.abs();
        let (beyond, tolerance) = if unit > 0.0 && unit.is_finite() {
            (super::PAINTS_BEYOND / unit, TOLERANCE / unit)
        } else {
            (0.0, TOLERANCE)
        };
        Self::with(Some(shown), beyond, tolerance)
    }

    fn with(shown: Option<Vec<Point>>, grow: f64, tolerance: f64) -> Self {
        Self {
            content: String::new(),
            failed: false,
            refused: None,
            decided: None,
            glyphs: Vec::new(),
            named: hash::Set::default(),
            state: State {
                region: Region {
                    shown,
                    grow,
                    tolerance,
                    ctm: Affine::IDENTITY,
                    abs: Affine::IDENTITY,
                    chain: Affine::new([0.0; 6]),
                    chain_count: 0.0,
                },
                width: 1.0,
            },
            frames: Vec::new(),
            held: String::new(),
            held_failed: false,
            path: Vec::new(),
        }
    }

    /// What the page shows of the current space, under the clips written so far.
    pub(super) fn region(&self) -> Region {
        self.state.region.clone()
    }

    /// Whether the convex hull of `points`, in the current space, can paint where
    /// the page shows it ([`Region::meets`]).
    pub(super) fn meets(&self, points: &[Point]) -> bool {
        self.state.region.meets(points)
    }

    /// How far, in the current space, a viewer may put a point of magnitude at
    /// most `most`, computed as `local` says, from where it lies ([`Region::slack`]).
    pub(super) fn slack(&self, most: f64, local: Rounding) -> f64 {
        self.state.region.slack(most, local)
    }

    /// What the generator made of the content, `drew`, the content written and the
    /// glyphs it draws; or why the appearance is refused: the first part refused
    /// where it may show ([`Out::refuse`]), or [`Unsupported::Coordinates`] where a
    /// number written could not be.
    pub(super) fn finish<T>(
        self,
        drew: Result<T, Refused>,
    ) -> Result<(T, String, Vec<Rc<Glyph>>), Unsupported> {
        if let Some(why) = self.refused {
            return Err(why);
        }
        match drew {
            Ok(drew) if !self.failed => Ok((drew, self.content, self.glyphs)),
            // A refusal is made only where one of these is recorded.
            _ => Err(Unsupported::Coordinates),
        }
    }

    /// A part of the appearance the generator cannot draw as viewers draw it, for
    /// `why`, which may paint within `part` under the clips written so far: where
    /// that may show where the page shows it ([`Region::may_show`]), the appearance
    /// is refused for it, and the generator stops; where it may not, the part is left
    /// out, as what viewers draw of it does not show either, and the generator goes
    /// on with the rest. A part the page shows nothing of is left out wherever it
    /// lies.
    ///
    /// The part is told where it would be drawn, under every clip it is drawn under
    /// and no other: until the frame it is told in ends, no clip may be written
    /// ([`Out::clip`]).
    pub(super) fn refuse(&mut self, why: Unsupported, part: Part) -> Result<(), Refused> {
        let depth = self.frames.len();
        self.decided = Some(self.decided.map_or(depth, |d| d.min(depth)));
        if !self.shows(&part) {
            return Ok(());
        }
        self.refused.get_or_insert(why);
        Err(Refused(()))
    }

    /// `made`, or, where it is why a part within `part()` cannot be drawn, that part
    /// left out (`None`) or the appearance refused for it, as [`Out::refuse`] says.
    pub(super) fn part<T>(
        &mut self,
        made: Result<T, Unsupported>,
        part: impl FnOnce() -> Part,
    ) -> Result<Option<T>, Refused> {
        match made {
            Ok(made) => Ok(Some(made)),
            Err(why) => self.refuse(why, part()).map(|()| None),
        }
    }

    /// Whether what `part` may paint may show where the page shows it.
    pub(super) fn shows(&self, part: &Part) -> bool {
        match part {
            Part::Anywhere => !self.state.region.is_empty(),
            Part::Near {
                points,
                reach,
                local,
            } => self.state.region.may_show(points, *reach, *local).is_some(),
        }
    }

    /// `drew`, what drawing pieces through `Out` came to: `None` where a piece that
    /// may show could not be written ([`Out::form`], [`Out::within`]), which fails
    /// the content where it was drawn, refused.
    pub(super) fn wrote(&mut self, drew: Option<()>) -> Result<(), Refused> {
        drew.ok_or_else(|| {
            debug_assert!(self.failed, "Out gives None only where a piece failed");
            self.failed = true;
            Refused(())
        })
    }

    /// What `draw` wrote through this `Out`, as [`Out::wrote`] says.
    pub(super) fn writes(
        &mut self,
        draw: impl FnOnce(&mut Self) -> Option<()>,
    ) -> Result<(), Refused> {
        let drew = draw(self);
        self.wrote(drew)
    }

    /// Writes what was held back, before something painted.
    fn release(&mut self) {
        self.content.push_str(&self.held);
        self.failed |= self.held_failed;
        self.held.clear();
        self.held_failed = false;
        for f in &mut self.frames {
            f.written = true;
        }
    }

    /// Draws the form of `glyph` where the current transform puts it ([`super::text::Pen`]),
    /// its place computed as `local` says in the current space, where its box may
    /// paint on the page ([`Region::may_show`]): `Some(true)` when it is drawn,
    /// `Some(false)` when no viewer puts it where the page shows it, `None`, failing
    /// the content, when one may and may put it farther from where it lies than
    /// [`TOLERANCE`].
    pub(super) fn form(&mut self, glyph: &Rc<Glyph>, local: Rounding) -> Option<bool> {
        let [x0, y0, x1, y1] = glyph.bbox.map(f64::from);
        let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(|(x, y)| Point::new(x, y));
        let Some(r) = self.state.region.may_show(&corners, 0.0, local) else {
            return Some(false);
        };
        if r.is_nan() || r > self.state.region.tolerance {
            self.failed = true;
            return None;
        }
        self.release();
        self.content.push('/');
        self.content.push_str(&glyph.name);
        self.content.push_str(" Do\n");
        if self.named.insert(glyph.name.clone()) {
            self.glyphs.push(glyph.clone());
        }
        Some(true)
    }

    /// `nums`, then operator `op`.
    pub(super) fn nums(&mut self, nums: &[f64], op: &'static str) -> Option<()> {
        let mut text = String::new();
        let mut ok = true;
        let mut values = Vec::with_capacity(nums.len());
        for &v in nums {
            ok &= written(v).is_some();
            let n = v as f32;
            // Writing into a `String` cannot fail.
            let _ = write!(text, "{n} ");
            values.push(f64::from(n));
        }
        text.push_str(op);
        text.push('\n');
        self.act(op, &text, &values, ok);
        Some(())
    }

    /// `nums`, path construction operator `op`, its points computed as `local`
    /// says ([`Rounding`]): the subpath is tested wherever a viewer may put them.
    pub(super) fn computed(
        &mut self,
        nums: &[f64],
        op: &'static str,
        local: Rounding,
    ) -> Option<()> {
        self.nums(nums, op)?;
        if let Some(sub) = self.path.last_mut() {
            sub.rounding = sub.rounding.either(local);
        }
        Some(())
    }

    /// Operator `op`, without operands.
    pub(super) fn op(&mut self, op: &'static str) {
        let text = format!("{op}\n");
        self.act(op, &text, &[], true);
    }

    /// Acts on operator `op` written as `text`, its operands `values` as written,
    /// `ok` when each could be.
    fn act(&mut self, op: &str, text: &str, values: &[f64], ok: bool) {
        let Some(kind) = Kind::of(op) else {
            // An operator of no kind above is written as it is, with what it needs.
            self.release();
            for p in std::mem::take(&mut self.path) {
                self.content.push_str(&p.text);
                self.failed |= p.failed;
            }
            self.content.push_str(text);
            self.failed |= !ok;
            return;
        };
        let point = |i: usize| Point::new(values[2 * i], values[2 * i + 1]);
        match kind {
            Kind::Starts | Kind::Adds => {
                if matches!(kind, Kind::Starts) || self.path.is_empty() {
                    self.path.push(Subpath::new());
                }
                let Some(sub) = self.path.last_mut() else {
                    return;
                };
                sub.text.push_str(text);
                sub.failed |= !ok;
                if op.ends_with("re") && values.len() == 4 {
                    let (x, y, w, h) = (values[0], values[1], values[2], values[3]);
                    for p in [(x, y), (x + w, y), (x + w, y + h), (x, y + h)] {
                        sub.add(Point::new(p.0, p.1));
                    }
                } else {
                    for i in 0..values.len() / 2 {
                        sub.add(point(i));
                    }
                }
            }
            Kind::Paints { stroke, open } => {
                let reach = if stroke {
                    stroke_reach(self.state.width)
                } else {
                    0.0
                };
                let path = std::mem::take(&mut self.path);
                let n = path.len();
                let region = &self.state.region;
                let mut refused = false;
                let kept: Vec<(usize, Subpath)> = path
                    .into_iter()
                    .enumerate()
                    .filter(|(_, p)| {
                        match region.may_show(&p.corners(reach), 0.0, p.rounding) {
                            None => false,
                            Some(r) => {
                                // Drawn where it lies, it may be too far from where a
                                // viewer draws it: refused.
                                refused |= r.is_nan() || r > region.tolerance;
                                true
                            }
                        }
                    })
                    .collect();
                self.failed |= refused;
                if kept.is_empty() {
                    return;
                }
                // Where the last subpath is left out, the one closed would be
                // another: the same operator that does not close.
                let last_kept = kept.last().is_some_and(|(i, _)| *i + 1 == n);
                self.release();
                for (_, p) in kept {
                    self.content.push_str(&p.text);
                    self.failed |= p.failed;
                }
                match open.filter(|_| !last_kept) {
                    Some(open) => {
                        self.content.push_str(open);
                        self.content.push('\n');
                    }
                    None => self.content.push_str(text),
                }
            }
            Kind::Ends => {
                self.path.clear();
            }
            Kind::Clips => self.clip_path(),
            Kind::Save => {
                self.frames.push(Frame {
                    saved: self.state.clone(),
                    held_at: self.held.len(),
                    held_failed: self.held_failed,
                    written: false,
                });
                self.held.push_str(text);
            }
            Kind::Restore => {
                let Some(frame) = self.frames.pop() else {
                    self.content.push_str(text);
                    return;
                };
                if frame.written {
                    // What was set since the last thing painted in it is not needed.
                    self.held.clear();
                    self.held_failed = false;
                    self.content.push_str(text);
                } else {
                    self.held.truncate(frame.held_at);
                    self.held_failed = frame.held_failed;
                }
                self.state = frame.saved;
                if self.decided.is_some_and(|d| d > self.frames.len()) {
                    self.decided = None;
                }
            }
            Kind::Transforms | Kind::Width | Kind::Sets => {
                if let (Kind::Transforms, [a, b, c, d, e, f]) = (&kind, values) {
                    let m = Affine::new([*a, *b, *c, *d, *e, *f]);
                    self.state.region.transform(m);
                }
                if let (Kind::Width, [w]) = (&kind, values) {
                    self.state.width = *w;
                }
                self.held.push_str(text);
                self.held_failed |= !ok;
            }
        }
    }

    /// `path` as path construction operators.
    pub(super) fn path(&mut self, path: &kurbo::BezPath) -> Option<()> {
        use kurbo::PathEl;
        let mut last = Point::ZERO;
        for el in path.elements() {
            let pts: &[Point] = match el {
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
            let nums: Vec<f64> = pts.iter().flat_map(|p| [p.x, p.y]).collect();
            self.nums(
                &nums,
                match el {
                    PathEl::MoveTo(_) => "m",
                    PathEl::LineTo(_) => "l",
                    _ => "c",
                },
            )?;
            last = *pts.last()?;
        }
        Some(())
    }

    /// `None`, failing the content, when `area` can paint where the page shows it
    /// and a corner of it cannot be written.
    pub(super) fn within(&mut self, area: kurbo::Rect) -> Option<()> {
        let ok = [area.x0, area.y0, area.x1, area.y1]
            .iter()
            .all(|&v| written(v).is_some());
        if ok {
            return Some(());
        }
        let corners = [
            Point::new(area.x0, area.y0),
            Point::new(area.x1, area.y0),
            Point::new(area.x1, area.y1),
            Point::new(area.x0, area.y1),
        ];
        if self.meets(&corners) {
            self.failed = true;
            return None;
        }
        Some(())
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

    /// The stroke state every generated appearance starts from: butt caps, mitred
    /// joins up to [`MITER_LIMIT`].
    pub(super) fn stroke_state(&mut self, width: f64, dash: &[f64]) -> Option<()> {
        self.op("0 J 0 j");
        self.nums(&[MITER_LIMIT], "M")?;
        self.nums(&[width], "w")?;
        let mut text = String::from("[");
        let mut ok = true;
        for &d in dash {
            ok &= written(d).is_some();
            let _ = write!(text, "{} ", d as f32);
        }
        text.push_str("] 0 d\n");
        self.act("d", &text, &[], ok);
        Some(())
    }

    /// Clips what follows to `r`; one that cannot be written as it is, cut to a box
    /// that holds what shows ([`Region::bounds`]), which cuts nothing that shows,
    /// so that its numbers are those of what shows, whatever lies beyond. Fails the
    /// content after a part told in a frame not yet ended ([`Out::refuse`]).
    pub(super) fn clip(&mut self, r: kurbo::Rect) -> Option<()> {
        self.clips_after_decision();
        let (text, ok, corners) = self.clip_rect(r)?;
        self.state.region.cut(&corners);
        // The path being built, if any, ends in the clip, unpainted.
        self.path.clear();
        self.held.push_str(&text);
        self.held_failed |= !ok;
        Some(())
    }

    /// Fails the content where a clip is written while a part told left out or
    /// refused may still be drawn under it ([`Out::refuse`]): the generator told it
    /// before laying out all it is clipped by.
    fn clips_after_decision(&mut self) {
        if self.decided.is_some() {
            debug_assert!(false, "a clip written after a part under it was told");
            self.failed = true;
        }
    }

    /// The clip to `r` as [`Out::clip`] writes it, whether each of its numbers can
    /// be written, and its corners as a viewer reads them, on the page.
    fn clip_rect(&self, r: kurbo::Rect) -> Option<(String, bool, [Point; 4])> {
        let mut r = r;
        let as_is = [r.x0, r.y0, r.width(), r.height()]
            .iter()
            .all(|v| written(*v).is_some());
        if let Some(b) = self.state.region.bounds().filter(|_| !as_is) {
            let a = r.abs();
            if !(a.x0 >= b.x0 && a.y0 >= b.y0 && a.x1 <= b.x1 && a.y1 <= b.y1) {
                let cut = a.intersect(b);
                r = if cut.width() > 0.0 && cut.height() > 0.0 {
                    cut
                } else {
                    kurbo::Rect::ZERO
                };
            }
        }
        let nums = [r.x0, r.y0, r.width(), r.height()];
        let mut text = String::new();
        let mut ok = true;
        let mut v = [0.0; 4];
        for (n, v) in nums.iter().zip(&mut v) {
            ok &= written(*n).is_some();
            let n = *n as f32;
            let _ = write!(text, "{n} ");
            *v = f64::from(n);
        }
        text.push_str("re\nW n\n");
        let [x, y, w, h] = v;
        let ctm = self.state.region.ctm;
        let corners =
            [(x, y), (x + w, y), (x + w, y + h), (x, y + h)].map(|(x, y)| ctm * Point::new(x, y));
        Some((text, ok, corners))
    }

    /// Clips what follows to the path built (`W n`), and ends it: what shows is
    /// cut by the convex hull of the boxes of its subpaths, which holds the area it
    /// clips to.
    fn clip_path(&mut self) {
        self.clips_after_decision();
        let path = std::mem::take(&mut self.path);
        let mut points = vec![];
        for p in &path {
            if p.lost {
                points.push(Point::new(f64::NAN, f64::NAN));
            } else {
                points.extend(&p.points);
            }
            self.held.push_str(&p.text);
            self.held_failed |= p.failed;
        }
        self.held.push_str("W n\n");
        if points.iter().all(|p| p.is_finite()) {
            let ctm = self.state.region.ctm;
            let on_page: Vec<Point> = points.iter().map(|p| ctm * *p).collect();
            self.state.region.cut(&on_page);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Out, Part};
    use crate::interpret::annotation::Unsupported;
    use kurbo::Rect;

    const PAGE: Rect = Rect::new(0.0, 0.0, 200.0, 200.0);

    /// What cannot paint on the page is left out, with the state set for it, and
    /// its numbers need not be writable; what can is written, and fails where a
    /// number of it cannot be.
    #[test]
    fn what_cannot_paint_is_left_out() {
        // A square far off the page, its numbers past what can be written.
        let mut out = Out::new(PAGE, 1.0);
        out.op("q");
        out.nums(&[1.0, 0.0, 0.0, 1.0, 1e7, 1e7], "cm");
        out.clip(Rect::new(0.0, 0.0, 100.0, 100.0));
        out.nums(&[0.0, 0.0, 100.0, 100.0], "re");
        out.op("f");
        out.op("Q");
        assert_eq!(out.finish(Ok(())).map(|c| c.1), Ok(String::new()));
        // A stroke 1 wide 3 below the page reaches 5 from its line: it paints
        // within a quarter of a point of the page; at 4.5 below, it does not.
        for (y, drawn) in [(-3.0, true), (-5.3, false)] {
            let mut out = Out::new(PAGE, 1.0);
            out.stroke_state(1.0, &[]);
            out.nums(&[10.0, y], "m");
            out.nums(&[100.0, y], "l");
            out.op("S");
            let content = out.finish(Ok(())).unwrap().1;
            assert_eq!(content.contains(" l\nS\n"), drawn, "{y}: {content}");
        }
        // Of a path, the subpath off the page is left out; the one closed by `s`
        // being left out, the stroke closes none.
        let mut out = Out::new(PAGE, 1.0);
        out.nums(&[10.0, 10.0], "m");
        out.nums(&[20.0, 10.0], "l");
        out.nums(&[1e7, 10.0], "m");
        out.nums(&[1e7, 20.0], "l");
        out.op("s");
        assert_eq!(out.finish(Ok(())).unwrap().1, "10 10 m\n20 10 l\nS\n");
        // Partly on the page, a number that cannot be written fails it.
        let mut out = Out::new(PAGE, 1.0);
        out.nums(&[-1e7, 10.0], "m");
        out.nums(&[20.0, 10.0], "l");
        out.op("S");
        assert!(out.finish(Ok(())).is_err());
    }

    /// A part is told under the clips written: one whose clip leaves nothing of
    /// the page is left out, and after it the generator draws on.
    #[test]
    fn a_part_is_told_under_the_clips_written() {
        let mut out = Out::new(PAGE, 1.0);
        out.op("q");
        out.clip(Rect::new(300.0, 0.0, 400.0, 200.0));
        let part = || Part::rect(Rect::new(100.0, 100.0, 350.0, 150.0));
        assert!(out.refuse(Unsupported::Character('\0'), part()).is_ok());
        out.op("Q");
        // The frame it was told in ended: a clip may follow.
        out.clip(Rect::new(0.0, 0.0, 50.0, 50.0));
        assert!(out.refuse(Unsupported::Character('\0'), part()).is_ok());
        assert!(out.finish(Ok(())).is_ok());
        let mut out = Out::new(PAGE, 1.0);
        assert!(out.refuse(Unsupported::Character('\0'), part()).is_err());
        assert_eq!(
            out.finish(Ok(())).map(|_| ()),
            Err(Unsupported::Character('\0'))
        );
    }

    /// A clip written after a part was told, in a frame that has not ended, is one
    /// the part may be drawn under but was not told under: it fails the content.
    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "a clip written after"))]
    fn a_clip_after_a_part_told_fails() {
        let mut out = Out::new(PAGE, 1.0);
        out.op("q");
        let part = Part::rect(Rect::new(250.0, 0.0, 300.0, 50.0));
        assert!(out.refuse(Unsupported::Character('\0'), part).is_ok());
        out.op("q");
        out.clip(Rect::new(0.0, 0.0, 50.0, 50.0));
        assert_eq!(
            out.finish(Ok(())).map(|_| ()),
            Err(Unsupported::Coordinates)
        );
    }

    /// A clip that cannot be written as it is is written cut to what the page
    /// shows; one of no area leaves nothing to paint.
    #[test]
    fn clips_are_cut_to_the_page_and_of_no_area_show_nothing() {
        let mut out = Out::new(PAGE, 1.0);
        out.clip(Rect::new(-1e7, 10.0, 100.0, 20.0));
        out.nums(&[0.0, 10.0, 10.0, 10.0], "re");
        out.op("f");
        let content = out.finish(Ok(())).unwrap().1;
        assert!(
            content.starts_with("-1.45025 10 101.45025 10 re\nW n\n"),
            "{content}"
        );
        let mut out = Out::new(PAGE, 1.0);
        out.clip(Rect::new(1.0, 1.0, 1.0, 39.0));
        assert!(out.region().is_empty());
        out.nums(&[0.0, 0.0, 10.0, 10.0], "re");
        out.op("f");
        assert_eq!(out.finish(Ok(())).unwrap().1, "");
    }
}
