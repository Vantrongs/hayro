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
use smallvec::SmallVec;
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

/// The corners of a convex polygon, or points it is the hull of: those of the
/// regions and parts an appearance's content is tested against, which are mostly
/// boxes cut by boxes, held without an allocation.
type Points = SmallVec<[Point; 8]>;

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
    shown: Option<Points>,
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
    pub(super) fn either(self, other: Self) -> Self {
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
        self.shown.as_ref().is_some_and(|shown| shown.is_empty())
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
    fn on_page(&self, points: &[Point]) -> Points {
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
    ) -> (Points, Vec2) {
        let (mut ctm, mut abs) = (self.ctm, self.abs);
        for m in then {
            let m = m.map(f64::from);
            ctm *= Affine::new(m);
            abs *= Affine::new(m.map(f64::abs));
        }
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
        let r = self.offset(&abs, most, chained, local);
        let on_page = points.iter().map(|p| ctm * *p).collect();
        (on_page, r)
    }

    /// How far, across and up, a viewer computing in 32-bit numbers may put a point
    /// on the page from where it lies ([`Self::as_any_viewer_computes`]), whose
    /// place sums magnitudes `most` through the transforms `abs` and rounds `chained`
    /// in composing them, computed as `local` says. Not falling as `most` and
    /// `chained` grow, as each step adds or multiplies what is not negative.
    fn offset(&self, abs: &Affine, most: Vec2, chained: Vec2, local: Rounding) -> Vec2 {
        let page = self.page();
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
        Vec2::new(
            if r.x.is_nan() { f64::INFINITY } else { r.x },
            if r.y.is_nan() { f64::INFINITY } else { r.y },
        )
    }

    /// The largest magnitude of a coordinate of what shows, on the page; 0 where
    /// nothing bounds it.
    fn page(&self) -> f64 {
        self.shown
            .as_deref()
            .unwrap_or_default()
            .iter()
            .fold(0.0_f64, |m, p| m.max(p.x.abs()).max(p.y.abs()))
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

    /// Whether a subpath of `extent`, its points computed as `local` says, painting
    /// `reach` beyond them, may paint in what shows grown ([`Painting::paints`]):
    /// what [`Out`] tests each subpath it paints by.
    pub(super) fn paints(&self, extent: &Extent, reach: f64, local: Rounding) -> Option<f64> {
        self.painting(local).paints(extent, reach)
    }

    /// The test [`Self::paints`] makes of subpaths whose points are computed as
    /// `local` says, made once for testing many.
    pub(super) fn painting(&self, local: Rounding) -> Painting<'_> {
        let [a, b, c, d, e, f] = self.ctm.as_coeffs();
        let axes = match &self.shown {
            Some(shown) if !shown.is_empty() => Self::axes(shown)
                .chain([Vec2::new(a, b).turn_90(), Vec2::new(c, d).turn_90()])
                .enumerate()
                .map(|(i, n)| Axis {
                    n,
                    along: [n.x * a + n.y * b, n.x * c + n.y * d, n.x * e + n.y * f],
                    shown: self.extent(shown, n),
                    place: AXES.contains(&n),
                    side: i < shown.len(),
                })
                .collect(),
            _ => vec![],
        };
        // The most over the least the transform stretches a length (its singular
        // values), a little more for rounding: a box grown by `reach` times this
        // holds, on the page, a disc `reach` times the norm wide around the box.
        let norm = a * a + b * b + c * c + d * d;
        let det = (a * d - b * c).abs();
        let most = ((norm + (norm * norm - 4.0 * det * det).max(0.0).sqrt()) / 2.0).sqrt();
        let stretch = norm.sqrt() * most / det * (1.0 + 2_f64.powi(-20));
        Painting {
            region: self,
            local,
            axes,
            stretch: if stretch.is_nan() {
                f64::INFINITY
            } else {
                stretch
            },
        }
    }

    /// The bits of every number that decides what may show in it: equal keys tell
    /// every piece alike.
    pub(super) fn key(&self) -> Vec<u64> {
        let mut key = vec![];
        match &self.shown {
            None => key.push(0),
            Some(shown) => {
                key.push(1 + shown.len() as u64);
                key.extend(shown.iter().flat_map(|p| [p.x.to_bits(), p.y.to_bits()]));
            }
        }
        key.extend([self.grow, self.tolerance, self.chain_count].map(f64::to_bits));
        for m in [self.ctm, self.abs, self.chain] {
            key.extend(m.as_coeffs().map(f64::to_bits));
        }
        key
    }

    /// Whether what `part` may paint may show in what shows grown.
    pub(super) fn shows(&self, part: &Part) -> bool {
        match part {
            Part::Anywhere => !self.is_empty(),
            Part::Near {
                points,
                reach,
                local,
            } => self.may_show(points, *reach, *local).is_some(),
        }
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
        let corners: Points = points
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
        let reach: Points = base
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
                if area(&hull(Points::from_slice(clip))) > 0.0 {
                    self.shown = None;
                } else {
                    self.shown = Some(Points::new());
                }
                return;
            }
            Some(shown) => shown,
        };
        let by = hull(Points::from_slice(clip));
        let mut left = if area(&by) > 0.0 {
            shown
        } else {
            Points::new()
        };
        for i in 0..by.len() {
            if left.is_empty() {
                break;
            }
            let (a, b) = (by[i], by[(i + 1) % by.len()]);
            let inside = |p: Point| (b - a).cross(p - a);
            let mut kept = Points::with_capacity(left.len() + 1);
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
        self.shown = Some(if area(&left) > 0.0 {
            left
        } else {
            Points::new()
        });
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

/// A direction along which what shows grown and a box on the page may lie apart
/// ([`Painting`]): its normal `n`; what a point `(x, y)` of the current space
/// comes to along it on the page, `along[0] x + along[1] y + along[2]`; what shows
/// grown along it; whether it is an axis of the page, along which a place no
/// finite number holds is one that cannot be told; and whether it is the normal of
/// a side of what shows.
struct Axis {
    n: Vec2,
    along: [f64; 3],
    shown: (f64, f64),
    place: bool,
    /// Whether it is the normal of a side of what shows.
    side: bool,
}

/// The test of whether a subpath may paint where the page shows it
/// ([`Region::paints`]), for subpaths whose points are computed as `local` says.
///
/// It tests the box of the subpath's points grown by the reach, transformed to the
/// page, against what shows grown, wherever a viewer computing in 32-bit numbers
/// may put it ([`Region::may_show`]): the two are apart where they are apart along
/// a normal of a side of what shows, an axis of the page, or a normal of a side of
/// the transformed box, which runs along the transform's columns whatever the
/// reach. So the directions are fixed, and along each the box's extent is computed
/// from its sides: the least and most of `k x` for `x` between the box's sides is
/// `k` times one side or the other by the sign of `k`, alone.
///
/// Whether it may paint does not fall as the reach or the box grows, to the bit,
/// so that a box that holds the boxes of many subpaths, grown by the most reach
/// of any, that may not paint holds none that may ([`super::index::Index`]):
/// - the sides move out, each a rounded sum of a side and the reach, or a side
///   further out and the same reach;
/// - the most along a direction is a rounded sum of terms each a rounded product
///   of a fixed number and a side it does not fall with, and the least likewise
///   does not rise; rounding to nearest keeps the order of what it rounds;
/// - how far a viewer may put a point grows with the magnitudes of the farthest
///   corner, each a rounded sum of products of fixed numbers not negative
///   ([`Region::offset`]), a product by 0 being 0 even of an infinite magnitude;
/// - along a direction they are apart only where the most plus that offset is
///   below what shows, or the least less it above, and NaN keeps them together:
///   a NaN sum holds an infinite term that a larger reach keeps;
/// - a place no finite number holds along an axis of the page, the most infinite
///   upwards or NaN, or the least downwards, may paint anywhere, and stays so.
pub(super) struct Painting<'r> {
    region: &'r Region,
    local: Rounding,
    axes: Vec<Axis>,
    /// How much wider a box is grown in the current space than the disc
    /// [`Region::may_show`] grows a hull by on the page ([`Painting::may_meet`]).
    stretch: f64,
}

impl Painting<'_> {
    /// Whether a subpath of `extent`, painting `reach` beyond its points, may paint
    /// in what shows grown: `None` where no viewer can; else how far on the page,
    /// across or up, a viewer may put it from where it lies (infinite where a
    /// point is no finite number, whose place cannot be told). `reach` is a number.
    pub(super) fn paints(&self, extent: &Extent, reach: f64) -> Option<f64> {
        let region = self.region;
        debug_assert!(!reach.is_nan(), "a reach is a number");
        if extent.lost {
            return region.may_show(&[Point::new(f64::NAN, f64::NAN)], 0.0, self.local);
        }
        let Extent { lo, hi, .. } = *extent;
        if lo.x.partial_cmp(&hi.x).is_none_or(|o| o.is_gt()) {
            return region.may_show(&[], 0.0, self.local);
        }
        let (x0, x1, y0, y1) = (lo.x - reach, hi.x + reach, lo.y - reach, hi.y + reach);
        // The magnitudes of its farthest corner, through the transforms.
        let (x, y) = (x0.abs().max(x1.abs()), y0.abs().max(y1.abs()));
        let sum = |m: &Affine| {
            let [a, b, c, d, e, f] = m.as_coeffs();
            Vec2::new(times(a, x) + times(c, y) + e, times(b, x) + times(d, y) + f)
        };
        let r = region.offset(
            &region.abs,
            sum(&region.abs),
            sum(&region.chain),
            self.local,
        );
        if region.shown.is_none() {
            return Some(r.x.max(r.y));
        }
        // The least and most of the box along an axis.
        let extent = |axis: &Axis| {
            let [kx, ky, k] = axis.along;
            let ((a0, a1), (b0, b1)) = (side(kx, x0, x1), side(ky, y0, y1));
            (a0 + b0 + k, a1 + b1 + k)
        };
        let lost = self.axes.iter().filter(|a| a.place).any(|axis| {
            let (p0, p1) = extent(axis);
            p1.is_nan() || p1 == f64::INFINITY || p0.is_nan() || p0 == f64::NEG_INFINITY
        });
        if lost {
            return Some(f64::INFINITY);
        }
        let apart = self.axes.iter().any(|axis| {
            let (p0, p1) = extent(axis);
            let (g0, g1) = axis.shown;
            let r = times(axis.n.x.abs(), r.x) + times(axis.n.y.abs(), r.y);
            p1 + r < g0 || p0 - r > g1
        });
        // What shows is empty: no axis, and nothing paints.
        (!apart && !self.axes.is_empty()).then_some(r.x.max(r.y))
    }

    /// The least reach at which a subpath of `extent` may paint ([`Self::paints`]):
    /// it paints at every reach from there on and at none below. Found by halving
    /// the bit patterns of the reaches from 0 to infinity, which run in their order;
    /// `None` where it paints at none.
    #[cfg(test)]
    pub(super) fn least(&self, extent: &Extent) -> Option<f64> {
        let paints = |bits: u64| self.paints(extent, f64::from_bits(bits)).is_some();
        let (mut lo, mut hi) = (0.0_f64.to_bits(), f64::INFINITY.to_bits());
        if paints(lo) {
            return Some(0.0);
        }
        if !paints(hi) {
            return None;
        }
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if paints(mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        Some(f64::from_bits(hi))
    }

    /// Whether a piece within `reach` of the convex hull of points whose box is
    /// `extent`, each computed as `local` says, may meet what shows grown as
    /// [`Region::may_show`] tells it: true wherever that tells it may, and
    /// possibly where it does not. For finding, among many such pieces, those
    /// that test to tell: it does not fall as the box, the reach or the rounding
    /// grows, so that where it is false for a box that holds the boxes of many,
    /// grown by the most reach and rounding of any, it is false for each.
    ///
    /// The box, grown in the current space by `reach` times how much more the
    /// transform stretches a length than it stretches the least, holds on the page
    /// the hull of the points grown by the disc `may_show` grows it by; how far a
    /// viewer may put it is taken at the box's corners, which are no nearer than
    /// any point; and both are tested along the sides of what shows, every one of
    /// which `may_show` tests too, with what computing either in 64-bit numbers
    /// may move it by, 2^-40 of every magnitude summed, besides. So where it is
    /// false, `may_show` finds the two apart along the same side.
    pub(super) fn may_meet(&self, extent: &Extent, reach: f64, local: Rounding) -> bool {
        let region = self.region;
        if region.shown.is_none() || extent.lost {
            return true;
        }
        let Extent { lo, hi, .. } = *extent;
        // Of no point; and what shows is empty where there is no axis.
        if lo.x.partial_cmp(&hi.x).is_none_or(|o| o.is_gt()) || self.axes.is_empty() {
            return false;
        }
        let grow = reach * self.stretch;
        let (x0, x1, y0, y1) = (lo.x - grow, hi.x + grow, lo.y - grow, hi.y + grow);
        let (x, y) = (lo.x.abs().max(hi.x.abs()), lo.y.abs().max(hi.y.abs()));
        let sum = |m: &Affine| {
            let [a, b, c, d, e, f] = m.as_coeffs();
            Vec2::new(times(a, x) + times(c, y) + e, times(b, x) + times(d, y) + f)
        };
        let most = sum(&region.abs);
        let r = region.offset(&region.abs, most, sum(&region.chain), local);
        let computed = (most.x + most.y + region.page() + grow) * 2_f64.powi(-40);
        let r = Vec2::new(r.x + computed, r.y + computed);
        !self.axes.iter().filter(|a| a.side).any(|axis| {
            let [kx, ky, k] = axis.along;
            let ((a0, a1), (b0, b1)) = (side(kx, x0, x1), side(ky, y0, y1));
            let (p0, p1) = (a0 + b0 + k, a1 + b1 + k);
            let (g0, g1) = axis.shown;
            let r = times(axis.n.x.abs(), r.x) + times(axis.n.y.abs(), r.y);
            p1 + r < g0 || p0 - r > g1
        })
    }
}

/// The least and most of `k x` for `x` from `lo` to `hi`.
fn side(k: f64, lo: f64, hi: f64) -> (f64, f64) {
    if k == 0.0 {
        (0.0, 0.0)
    } else if k > 0.0 {
        (k * lo, k * hi)
    } else {
        (k * hi, k * lo)
    }
}

/// The normals of the sides of the convex hull of `points`, every one finite:
/// with those of another convex polygon, the axes along which the two are apart
/// if they are apart at all.
fn sides(points: &[Point]) -> SmallVec<[Vec2; 8]> {
    let h = hull(Points::from_slice(points));
    match h.len() {
        0 | 1 => SmallVec::new(),
        2 => smallvec::smallvec![(h[1] - h[0]).turn_90()],
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
fn hull(mut points: Points) -> Points {
    if !points.iter().all(|p| p.is_finite()) {
        return Points::new();
    }
    points.sort_by(|p, q| p.x.total_cmp(&q.x).then(p.y.total_cmp(&q.y)));
    points.dedup();
    if points.len() < 3 {
        return points;
    }
    // Andrew's monotone chain.
    let mut hull = Points::new();
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

/// State set and not yet painted with ([`Out`]): its text, and the dash patterns
/// among it, each where in the text it goes, held as made for every annotation
/// that sets it, so that holding one costs the same however long it is.
#[derive(Default)]
struct Held {
    text: String,
    dashes: Vec<(usize, Rc<Dash>)>,
}

impl Held {
    fn push_str(&mut self, text: &str) {
        // Room for a save, a clip and the state set around it at once, rather than
        // growing from a few bytes for every annotation.
        if self.text.capacity() == 0 {
            self.text.reserve(128);
        }
        self.text.push_str(text);
    }

    /// Where what is held ends, for [`Self::truncate`].
    fn mark(&self) -> (usize, usize) {
        (self.text.len(), self.dashes.len())
    }

    /// Left as it was at `mark`.
    fn truncate(&mut self, (text, dashes): (usize, usize)) {
        self.text.truncate(text);
        self.dashes.truncate(dashes);
    }

    fn clear(&mut self) {
        self.truncate((0, 0));
    }

    /// Writes what is held into `content`, and holds nothing.
    fn write_to(&mut self, content: &mut String) {
        let mut at = 0;
        for (i, dash) in self.dashes.drain(..) {
            content.push_str(&self.text[at..i]);
            content.push_str(&dash.text);
            at = i;
        }
        content.push_str(&self.text[at..]);
        self.text.clear();
    }
}

/// A dash pattern as content sets it, `[…] 0 d`, each entry written as a 32-bit
/// number, and whether each can be written ([`written`]): made once for the
/// document from a dash array (`super::dash`), and held as it is
/// ([`Out::stroke_state`]).
pub(super) struct Dash {
    text: String,
    ok: bool,
    empty: bool,
}

impl Dash {
    /// The pattern of `entries`, however many.
    pub(super) fn of(entries: &[f64]) -> Self {
        let mut text = String::from("[");
        let mut ok = true;
        for &d in entries {
            ok &= written(d).is_some();
            let _ = write!(text, "{} ", d as f32);
        }
        text.push_str("] 0 d\n");
        Self {
            text,
            ok,
            empty: entries.is_empty(),
        }
    }

    /// Whether it has no entry: a solid line.
    pub(super) fn is_solid(&self) -> bool {
        self.empty
    }
}

/// A `q` not yet ended: the state it saves, where its text starts in what is held
/// back, and whether it has been written.
struct Frame {
    saved: State,
    held_at: (usize, usize),
    held_failed: bool,
    written: bool,
}

/// The box of a subpath's points as a viewer reads them, and whether one of them is
/// no finite number, whose place cannot be told: what decides whether the subpath
/// may paint where the page shows it ([`Region::paints`]). A generator that walks a
/// shared array once for many annotations tells its pieces by this, as [`Out`]
/// tells what it writes.
#[derive(Clone, Copy, Debug)]
pub(super) struct Extent {
    lo: Point,
    hi: Point,
    lost: bool,
}

impl Extent {
    /// Of no point.
    const NONE: Self = Self {
        lo: Point::new(f64::INFINITY, f64::INFINITY),
        hi: Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY),
        lost: false,
    };

    /// Of a subpath of `points`, each written as [`Out::nums`] writes it.
    pub(super) fn written(points: impl IntoIterator<Item = Point>) -> Self {
        let mut extent = Self::NONE;
        for p in points {
            extent.add(Point::new(as_written(p.x), as_written(p.y)));
        }
        extent
    }

    /// Of the points `points`, as they are: for a test that must hold the hull of
    /// those very points ([`Painting::may_meet`]).
    pub(super) fn of(points: impl IntoIterator<Item = Point>) -> Self {
        let mut extent = Self::NONE;
        for p in points {
            extent.add(p);
        }
        extent
    }

    /// Whether it is a box: of some point, every one finite.
    pub(super) fn is_box(&self) -> bool {
        !self.lost && self.lo.x <= self.hi.x
    }

    /// The box that holds both boxes.
    pub(super) fn union(self, other: Self) -> Self {
        Self {
            lo: Point::new(self.lo.x.min(other.lo.x), self.lo.y.min(other.lo.y)),
            hi: Point::new(self.hi.x.max(other.hi.x), self.hi.y.max(other.hi.y)),
            lost: self.lost || other.lost,
        }
    }

    /// The middle of the box, across and up.
    pub(super) fn middle(&self) -> Point {
        Point::new(
            self.lo.x / 2.0 + self.hi.x / 2.0,
            self.lo.y / 2.0 + self.hi.y / 2.0,
        )
    }

    /// With `p`, as written.
    fn add(&mut self, p: Point) {
        if p.is_finite() {
            self.lo = Point::new(self.lo.x.min(p.x), self.lo.y.min(p.y));
            self.hi = Point::new(self.hi.x.max(p.x), self.hi.y.max(p.y));
        } else {
            self.lost = true;
        }
    }
}

/// A subpath of the path being built: its text, its points and their extent in the
/// current space, and whether a number of it cannot be written.
struct Subpath {
    text: String,
    /// Its finite points, control points included, whose hull holds it: a clip
    /// cuts what shows by that hull, not by the box, which a turned clip overfills.
    points: Vec<Point>,
    extent: Extent,
    failed: bool,
    /// How its points were computed.
    rounding: Rounding,
}

impl Subpath {
    fn new() -> Self {
        Self {
            text: String::new(),
            points: vec![],
            extent: Extent::NONE,
            failed: false,
            rounding: Rounding::NONE,
        }
    }

    fn add(&mut self, p: Point) {
        if p.is_finite() {
            self.points.push(p);
        }
        self.extent.add(p);
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
    held: Held,
    held_failed: bool,
    path: Vec<Subpath>,
    /// Where an operator's line is written ([`Out::take_line`]).
    line: String,
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
            Points::new()
        } else {
            let (x0, y0, x1, y1) = (clamp(v.x0), clamp(v.y0), clamp(v.x1), clamp(v.y1));
            let p = Points::from_slice(&[
                Point::new(x0, y0),
                Point::new(x1, y0),
                Point::new(x1, y1),
                Point::new(x0, y1),
            ]);
            if area(&p) > 0.0 { p } else { Points::new() }
        };
        let unit = unit.abs();
        let (beyond, tolerance) = if unit > 0.0 && unit.is_finite() {
            (super::PAINTS_BEYOND / unit, TOLERANCE / unit)
        } else {
            (0.0, TOLERANCE)
        };
        Self::with(Some(shown), beyond, tolerance)
    }

    fn with(shown: Option<Points>, grow: f64, tolerance: f64) -> Self {
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
            held: Held::default(),
            held_failed: false,
            path: Vec::new(),
            line: String::new(),
        }
    }

    /// What the page shows of the current space, under the clips written so far.
    pub(super) fn region(&self) -> Region {
        self.state.region.clone()
    }

    /// How far beyond its points a subpath painted now paints: as far as a stroke
    /// of the line width set reaches ([`stroke_reach`]), everywhere for a width
    /// that is no number, or nothing for a fill.
    pub(super) fn reach(&self, stroke: bool) -> f64 {
        match stroke_reach(self.state.width) {
            _ if !stroke => 0.0,
            r if r.is_nan() => f64::INFINITY,
            r => r,
        }
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
        self.state.region.shows(part)
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
        self.held.write_to(&mut self.content);
        self.failed |= self.held_failed;
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
        let mut text = self.take_line();
        let mut ok = true;
        let mut values = SmallVec::<[f64; 6]>::with_capacity(nums.len());
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
        self.line = text;
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
        // An operator's line written in place: operators are a few bytes.
        let mut line = [0_u8; 8];
        if let Some(text) = line.get_mut(..op.len() + 1) {
            let (name, end) = text.split_at_mut(op.len());
            name.copy_from_slice(op.as_bytes());
            end[0] = b'\n';
            if let Ok(text) = std::str::from_utf8(text) {
                self.act(op, text, &[], true);
                return;
            }
        }
        self.act(op, &format!("{op}\n"), &[], true);
    }

    /// The buffer an operator's line is written in, empty: taken, so that writing
    /// the line can read the rest of `self`, and put back after, so that lines
    /// reuse one allocation.
    fn take_line(&mut self) -> String {
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        line
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
                let reach = self.reach(stroke);
                let path = std::mem::take(&mut self.path);
                let n = path.len();
                let region = &self.state.region;
                let mut refused = false;
                let kept: Vec<(usize, Subpath)> = path
                    .into_iter()
                    .enumerate()
                    .filter(|(_, p)| {
                        match region.paints(&p.extent, reach, p.rounding) {
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
                    held_at: self.held.mark(),
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
    /// joins up to [`MITER_LIMIT`], `width`, and `dash`, or none. The dash is held
    /// as it is made for every annotation that sets it ([`Dash`]) and copied into
    /// the content only where something painted needs it.
    pub(super) fn stroke_state(&mut self, width: f64, dash: Option<&Rc<Dash>>) -> Option<()> {
        self.op("0 J 0 j");
        self.nums(&[MITER_LIMIT], "M")?;
        self.nums(&[width], "w")?;
        match dash {
            None => self.act("d", "[] 0 d\n", &[], true),
            Some(dash) => {
                self.held.dashes.push((self.held.text.len(), dash.clone()));
                self.held_failed |= !dash.ok;
            }
        }
        Some(())
    }

    /// Clips what follows to `r`; one that cannot be written as it is, cut to a box
    /// that holds what shows ([`Region::bounds`]), which cuts nothing that shows,
    /// so that its numbers are those of what shows, whatever lies beyond. Fails the
    /// content after a part told in a frame not yet ended ([`Out::refuse`]).
    pub(super) fn clip(&mut self, r: kurbo::Rect) -> Option<()> {
        self.clips_after_decision();
        let mut text = self.take_line();
        let (ok, corners) = self.clip_rect(r, &mut text)?;
        self.state.region.cut(&corners);
        // The path being built, if any, ends in the clip, unpainted.
        self.path.clear();
        self.held.push_str(&text);
        self.line = text;
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

    /// The clip to `r` as [`Out::clip`] writes it, written into `text`, whether
    /// each of its numbers can be written, and its corners as a viewer reads them,
    /// on the page.
    fn clip_rect(&self, r: kurbo::Rect, text: &mut String) -> Option<(bool, [Point; 4])> {
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
        Some((ok, corners))
    }

    /// Clips what follows to the path built (`W n`), and ends it: what shows is
    /// cut by the convex hull of the boxes of its subpaths, which holds the area it
    /// clips to.
    fn clip_path(&mut self) {
        self.clips_after_decision();
        let path = std::mem::take(&mut self.path);
        let mut points = vec![];
        for p in &path {
            if p.extent.lost {
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
    use super::{Extent, Out, Part, Rounding};
    use crate::interpret::annotation::Unsupported;
    use kurbo::{Point, Rect};

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
            out.stroke_state(1.0, None);
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

    /// A number from `seed`, from 0 below 1.
    fn next(seed: &mut u64) -> f64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed >> 11) as f64 / (1_u64 << 53) as f64
    }

    /// Whether a subpath may paint does not fall as its reach grows, to the bit
    /// ([`super::Painting`]): on generated views, units, transforms (turned,
    /// sheared, flipped, scaled up and down, of no area) and clips, generated
    /// boxes on, near and far off the page, of no size and huge, and each way of
    /// computing their points, it paints at a reach exactly where the reach is at
    /// least the least one found ([`super::Painting::least`]): at that reach and
    /// not at the reach just below it, and at reaches of every size, from 0 and
    /// the least numbers through to infinity, on either side of it.
    /// Content on a generated view, of a generated unit, under up to three
    /// generated transforms (turns, shears, flips, scales from 10^-4 to 10^4,
    /// some of no area) and, sometimes, a clip.
    fn generated_out(r: &mut u64, case: usize) -> Out {
        let (x, y) = (next(r) * 400.0 - 100.0, next(r) * 400.0 - 100.0);
        let view = Rect::new(x, y, x + next(r) * 300.0, y + next(r) * 300.0);
        let unit = [1.0, 0.5, 2.0, 1e-3][case % 4];
        let mut out = Out::new(view, unit);
        for _ in 0..(next(r) * 3.0) as usize {
            let scale = 10_f64.powf(next(r) * 8.0 - 4.0);
            let (t, shear) = (next(r) * 6.4, next(r) - 0.5);
            let flip = if next(r) < 0.3 { -1.0 } else { 1.0 };
            let mut m = [
                scale * t.cos(),
                scale * t.sin(),
                scale * (shear - t.sin()) * flip,
                scale * t.cos() * flip,
                next(r) * 400.0 - 200.0,
                next(r) * 400.0 - 200.0,
            ];
            if next(r) < 0.1 {
                // Of no area: one column a multiple of the other.
                (m[2], m[3]) = (m[0] * 2.0, m[1] * 2.0);
            }
            out.nums(&m, "cm");
        }
        if next(r) < 0.4 {
            let (cx, cy) = (next(r) * 300.0 - 50.0, next(r) * 300.0 - 50.0);
            out.clip(Rect::new(
                cx,
                cy,
                cx + next(r) * 200.0,
                cy + next(r) * 200.0,
            ));
        }
        out
    }

    /// Up to four generated points, near what a generated view shows or as far
    /// as 10^38 off, some the same, some between others.
    fn generated_points(r: &mut u64) -> Vec<Point> {
        let far = 10_f64.powf(next(r) * 40.0 - 2.0);
        let at = |r: &mut u64| {
            let near = next(r) < 0.5;
            let k = if near { 300.0 } else { far };
            Point::new(next(r) * k - k / 2.0, next(r) * k - k / 2.0)
        };
        let n = 1 + (next(r) * 4.0) as usize;
        let first = at(r);
        (0..n)
            .map(|i| {
                if i == 0 || next(r) < 0.2 {
                    first
                } else {
                    first + (at(r) - first) * next(r)
                }
            })
            .collect()
    }

    #[test]
    fn whether_a_subpath_paints_does_not_fall_as_its_reach_grows() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let (mut painted, mut never, mut at_zero) = (0, 0, 0);
        for case in 0..4_000 {
            let r = &mut seed;
            let out = generated_out(r, case);
            let region = out.region();
            let local = if next(r) < 0.5 {
                Rounding::NONE
            } else {
                Rounding::of(next(r) * 20.0, next(r) * 1e4)
            };
            let painting = region.painting(local);
            for _ in 0..8 {
                let extent = Extent::written(generated_points(r));
                let paints = |reach: f64| painting.paints(&extent, reach).is_some();
                let least = painting.least(&extent);
                // Reaches of every size, and of every bit pattern near the least.
                let mut reaches: Vec<f64> = vec![0.0, f64::from_bits(1), 1e-300, f64::INFINITY];
                reaches.extend((0..24).map(|_| 10_f64.powf(next(r) * 80.0 - 40.0)));
                reaches
                    .extend((0..8).map(|_| {
                        f64::from_bits((next(r) * f64::INFINITY.to_bits() as f64) as u64)
                    }));
                match least {
                    None => never += 1,
                    Some(0.0) => at_zero += 1,
                    Some(least) => {
                        painted += 1;
                        let bits = least.to_bits();
                        assert!(!paints(f64::from_bits(bits - 1)), "{extent:?} {least}");
                        for d in 1..=4 {
                            reaches.push(f64::from_bits(bits.saturating_sub(d)));
                            reaches.push(f64::from_bits((bits + d).min(f64::INFINITY.to_bits())));
                        }
                    }
                }
                for reach in reaches {
                    let at_least = least.is_some_and(|least| reach >= least);
                    assert_eq!(
                        paints(reach),
                        at_least,
                        "{extent:?} at {reach:e}, least {least:?}, in {region:?}"
                    );
                }
            }
        }
        // Every kind is met often.
        assert!(
            painted > 4_000 && never > 500 && at_zero > 4_000,
            "{painted} found, {never} never, {at_zero} at 0"
        );
    }

    /// What a tree of the parts of shared geometry rests on ([`super::super::index`]):
    /// whether a box may paint does not fall as the box grows, nor as its reach
    /// does, and whether it may meet what shows ([`super::Painting::may_meet`])
    /// does not fall as the box, its reach or its rounding grows; and it holds
    /// wherever [`super::Region::may_show`] tells the points in the box may show.
    /// On generated regions (turned, sheared, scaled, of no area, clipped) and
    /// generated points near and far, boxes holding them grown by generated
    /// amounts, reaches of all sizes, and roundings.
    #[test]
    fn a_box_that_holds_another_may_paint_where_it_may() {
        let mut seed = 0x1f83_d9ab_fb41_bd6b_u64;
        let (mut shows, mut met, mut paints) = (0, 0, 0);
        let rounding = |r: &mut u64| {
            if next(r) < 0.3 {
                Rounding::NONE
            } else {
                Rounding::of(next(r) * 40.0, 10_f64.powf(next(r) * 12.0 - 2.0))
            }
        };
        let reach = |r: &mut u64| match (next(r) * 6.0) as usize {
            0 => 0.0,
            1 => f64::INFINITY,
            _ => 10_f64.powf(next(r) * 20.0 - 6.0),
        };
        for case in 0..4_000 {
            let r = &mut seed;
            let out = generated_out(r, case);
            let region = out.region();
            let painting = region.painting(Rounding::NONE);
            for _ in 0..8 {
                let points = generated_points(r);
                let (rc, lc) = (reach(r), rounding(r));
                // The points' box, as they are and as written, and one holding it.
                let (exact, written) =
                    (Extent::of(points.clone()), Extent::written(points.clone()));
                let mut more = generated_points(r);
                more.truncate(1 + (next(r) * 2.0) as usize);
                let holding = exact.union(written).union(Extent::of(more));
                let (rp, lp) = (rc + reach(r) * next(r), lc.either(rounding(r)));
                if region.may_show(&points, rc, lc).is_some() {
                    shows += 1;
                    assert!(
                        painting.may_meet(&exact, rc, lc),
                        "{points:?} within {rc:e} of {lc:?} shows in {region:?}"
                    );
                }
                if painting.may_meet(&exact, rc, lc) {
                    met += 1;
                    assert!(
                        painting.may_meet(&holding, rp, lp),
                        "{exact:?} {rc:e} {lc:?} meets, {holding:?} {rp:e} {lp:?} not, in {region:?}"
                    );
                }
                if painting.paints(&written, rc).is_some() {
                    paints += 1;
                    assert!(
                        painting.paints(&holding, rp).is_some(),
                        "{written:?} {rc:e} paints, {holding:?} {rp:e} not, in {region:?}"
                    );
                }
            }
        }
        // Each is met often.
        assert!(
            shows > 4_000 && met > 4_000 && paints > 4_000,
            "{shows} show, {met} meet, {paints} paint"
        );
    }
}
