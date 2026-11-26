//! Builds typed marks and refuses unsupported geometry only where it may be visible.

use super::marks::{Mark, Marks};
use super::{Glyph, Unsupported};
use crate::hash;
use kurbo::{Affine, Point, Vec2};
use smallvec::SmallVec;
use std::rc::Rc;

// Beyond 2²³, adjacent coordinates can round a full user-space unit apart.
fn written(v: f64) -> Option<f32> {
    let n = v as f32;
    (n.abs() < 8_388_608.0).then_some(n)
}

pub(super) fn as_written(v: f64) -> f64 {
    f64::from(v as f32)
}

pub(super) fn written_all<const L: usize>(v: [f64; L]) -> Result<[f32; L], Unsupported> {
    let mut n = [0.0; L];
    for (n, v) in n.iter_mut().zip(v) {
        *n = written(v).ok_or(Unsupported::Coordinates)?;
    }
    Ok(n)
}

#[derive(Debug)]
#[must_use]
pub(super) struct Refused(());

pub(super) enum Part {
    Near {
        points: Vec<Point>,
        reach: f64,
        local: Rounding,
    },
    Anywhere,
}

impl Part {
    pub(super) fn near(points: Vec<Point>, reach: f64) -> Self {
        Self::Near {
            points,
            reach,
            local: Rounding::NONE,
        }
    }

    pub(super) fn rect(r: kurbo::Rect) -> Self {
        Self::near(corners(r).to_vec(), 0.0)
    }

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

pub(super) fn corners(r: kurbo::Rect) -> [Point; 4] {
    [(r.x0, r.y0), (r.x1, r.y0), (r.x1, r.y1), (r.x0, r.y1)].map(|(x, y)| Point::new(x, y))
}

pub(super) fn stroke_reach(width: f64) -> f64 {
    width.abs() / 2.0 * MITER_LIMIT.max(std::f64::consts::SQRT_2)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Color {
    Gray(f64),
    Rgb([f64; 3]),
    Cmyk([f64; 4]),
}

impl Color {
    pub(super) const BLACK: Self = Self::Gray(0.0);

    pub(super) fn of(components: &[f64]) -> Option<Self> {
        let c = |i: usize| components[i];
        match components.len() {
            0 => None,
            1 | 2 => Some(Self::Gray(c(0))),
            3 => Some(Self::Rgb([c(0), c(1), c(2)])),
            4.. => Some(Self::Cmyk([c(0), c(1), c(2), c(3)])),
        }
    }

    pub(super) fn rgb(self) -> Self {
        match self {
            Self::Gray(g) => Self::Rgb([g; 3]),
            Self::Rgb(_) => self,
            Self::Cmyk([c, m, y, k]) => Self::Rgb([c, m, y].map(|v| 1.0 - (v + k).min(1.0))),
        }
    }

    pub(super) fn luminance(self) -> f64 {
        match self.rgb() {
            Self::Rgb([r, g, b]) => 0.3 * r + 0.59 * g + 0.11 * b,
            Self::Gray(g) => g,
            Self::Cmyk(_) => 0.0,
        }
    }

    pub(super) fn map(self, f: impl Fn(f64) -> f64) -> Self {
        match self {
            Self::Gray(g) => Self::Gray(f(g)),
            Self::Rgb(c) => Self::Rgb(c.map(f)),
            Self::Cmyk(c) => Self::Cmyk(c.map(f)),
        }
    }
}

pub(super) const MITER_LIMIT: f64 = 10.0;

type Points = SmallVec<[Point; 8]>;

#[derive(Clone, Debug)]
pub(super) struct Region {
    shown: Option<Points>,
    grow: f64,
    tolerance: f64,
    ctm: Affine,
    abs: Affine,
    chain: Affine,
    chain_count: f64,
}

const U: f64 = 5.960_464_477_539_063e-8;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Rounding {
    count: Vec2,
    summed: Vec2,
}

impl Rounding {
    pub(super) const NONE: Self = Self {
        count: Vec2::ZERO,
        summed: Vec2::ZERO,
    };

    pub(super) fn of(count: f64, most: f64) -> Self {
        Self {
            count: Vec2::new(count, 0.0),
            summed: Vec2::new(count * most.abs(), 0.0),
        }
    }

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

    pub(super) fn plus(self, other: Self) -> Self {
        Self {
            count: self.count + other.count,
            summed: self.summed + other.summed,
        }
    }

    pub(super) fn point(x: Self, y: Self) -> Self {
        Self {
            count: Vec2::new(x.count.x, y.count.x),
            summed: Vec2::new(x.summed.x, y.summed.x),
        }
    }

    pub(super) fn either(self, other: Self) -> Self {
        let max = |a: Vec2, b: Vec2| Vec2::new(a.x.max(b.x), a.y.max(b.y));
        Self {
            count: max(self.count, other.count),
            summed: max(self.summed, other.summed),
        }
    }

    pub(super) fn scaled(self, k: f64) -> Self {
        Self {
            count: self.count,
            summed: self.summed * k.abs(),
        }
    }

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

fn times(k: f64, v: f64) -> f64 {
    if k == 0.0 { 0.0 } else { k * v }
}

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

pub(super) const TOLERANCE: f64 = 1.0;

const VIEWER_ROUNDINGS: f64 = 40.0;

const AXES: [Vec2; 2] = [Vec2::new(1.0, 0.0), Vec2::new(0.0, 1.0)];

impl Region {
    pub(super) fn is_empty(&self) -> bool {
        self.shown.as_ref().is_some_and(|shown| shown.is_empty())
    }

    fn axes(shown: &[Point]) -> impl Iterator<Item = Vec2> + '_ {
        (0..shown.len())
            .map(move |i| (shown[(i + 1) % shown.len()] - shown[i]).turn_90())
            .chain(AXES)
    }

    fn extent(&self, shown: &[Point], n: Vec2) -> (f64, f64) {
        let (lo, hi) = along(shown.iter().copied(), n);
        let g = self.grow * (n.x.abs() + n.y.abs());
        (lo - g, hi + g)
    }

    fn on_page(&self, points: &[Point]) -> Points {
        points.iter().map(|p| self.ctm * *p).collect()
    }

    pub(super) fn meets(&self, points: &[Point]) -> bool {
        self.near(points, 0.0)
    }

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

    fn page(&self) -> f64 {
        self.shown
            .as_deref()
            .unwrap_or_default()
            .iter()
            .fold(0.0_f64, |m, p| m.max(p.x.abs()).max(p.y.abs()))
    }

    fn meets_on_page(&self, shown: &[Point], page: &[Point], reach: f64, r: Vec2) -> bool {
        Self::axes(shown).chain(sides(page)).all(|n| {
            let (g0, g1) = self.extent(shown, n);
            let (p0, p1) = along(page.iter().copied(), n);
            let r = reach * n.hypot() + times(n.x.abs(), r.x) + times(n.y.abs(), r.y);
            p1 + r >= g0 && p0 - r <= g1
        })
    }

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

    pub(super) fn slack(&self, most: f64, local: Rounding) -> f64 {
        let (_, r) = self.as_any_viewer_computes(&[], &[Point::new(most, most)], local);
        let r = r.x.max(r.y);
        let [a, b, c, d, _, _] = self.ctm.as_coeffs();
        // The least stretch: |det| over the most, which is at most the norm below.
        let least = (a * d - b * c).abs() / (a * a + b * b + c * c + d * d).sqrt();
        let s = r * std::f64::consts::SQRT_2 / least;
        if s.is_nan() { f64::INFINITY } else { s }
    }

    pub(super) fn paints(&self, extent: &Extent, reach: f64, local: Rounding) -> Option<f64> {
        self.painting(local).paints(extent, reach)
    }

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

    pub(super) fn near(&self, points: &[Point], reach: f64) -> bool {
        self.may_show(points, reach, Rounding::NONE).is_some()
    }

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

struct Axis {
    n: Vec2,
    along: [f64; 3],
    shown: (f64, f64),
    place: bool,
    side: bool,
}

pub(super) struct Painting<'r> {
    region: &'r Region,
    local: Rounding,
    axes: Vec<Axis>,
    stretch: f64,
}

impl Painting<'_> {
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

fn side(k: f64, lo: f64, hi: f64) -> (f64, f64) {
    if k == 0.0 {
        (0.0, 0.0)
    } else if k > 0.0 {
        (k * lo, k * hi)
    } else {
        (k * hi, k * lo)
    }
}

fn sides(points: &[Point]) -> SmallVec<[Vec2; 8]> {
    let h = hull(Points::from_slice(points));
    match h.len() {
        0 | 1 => SmallVec::new(),
        2 => smallvec::smallvec![(h[1] - h[0]).turn_90()],
        n => (0..n).map(|i| (h[(i + 1) % n] - h[i]).turn_90()).collect(),
    }
}

fn along(points: impl Iterator<Item = Point>, n: Vec2) -> (f64, f64) {
    points.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
        let v = n.dot(p.to_vec2());
        (lo.min(v), hi.max(v))
    })
}

fn area(p: &[Point]) -> f64 {
    (0..p.len())
        .map(|i| p[i].to_vec2().cross(p[(i + 1) % p.len()].to_vec2()))
        .sum::<f64>()
        / 2.0
}

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

#[derive(Clone, Debug)]
struct State {
    region: Region,
    width: f64,
}

pub(super) struct Dash {
    values: Rc<[f32]>,
    ok: bool,
}

impl Dash {
    pub(super) fn of(entries: &[f64]) -> Self {
        Self {
            values: entries.iter().map(|v| *v as f32).collect(),
            ok: entries.iter().all(|v| written(*v).is_some()),
        }
    }
    pub(super) fn is_solid(&self) -> bool {
        self.values.is_empty()
    }
}

struct Frame {
    saved: State,
    held_at: usize,
    held_failed: bool,
    written: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Extent {
    lo: Point,
    hi: Point,
    lost: bool,
}

impl Extent {
    const NONE: Self = Self {
        lo: Point::new(f64::INFINITY, f64::INFINITY),
        hi: Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY),
        lost: false,
    };

    pub(super) fn written(points: impl IntoIterator<Item = Point>) -> Self {
        let mut extent = Self::NONE;
        for p in points {
            extent.add(Point::new(as_written(p.x), as_written(p.y)));
        }
        extent
    }

    pub(super) fn of(points: impl IntoIterator<Item = Point>) -> Self {
        let mut extent = Self::NONE;
        for p in points {
            extent.add(p);
        }
        extent
    }

    pub(super) fn is_box(&self) -> bool {
        !self.lost && self.lo.x <= self.hi.x
    }

    pub(super) fn union(self, other: Self) -> Self {
        Self {
            lo: Point::new(self.lo.x.min(other.lo.x), self.lo.y.min(other.lo.y)),
            hi: Point::new(self.hi.x.max(other.hi.x), self.hi.y.max(other.hi.y)),
            lost: self.lost || other.lost,
        }
    }

    pub(super) fn middle(&self) -> Point {
        Point::new(
            self.lo.x / 2.0 + self.hi.x / 2.0,
            self.lo.y / 2.0 + self.hi.y / 2.0,
        )
    }

    fn add(&mut self, p: Point) {
        if p.is_finite() {
            self.lo = Point::new(self.lo.x.min(p.x), self.lo.y.min(p.y));
            self.hi = Point::new(self.hi.x.max(p.x), self.hi.y.max(p.y));
        } else {
            self.lost = true;
        }
    }
}

struct Subpath {
    marks: Vec<Mark>,
    points: Vec<Point>,
    extent: Extent,
    failed: bool,
    rounding: Rounding,
}

impl Subpath {
    fn new() -> Self {
        Self {
            marks: Vec::new(),
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

pub(super) struct Out {
    content: Marks,
    failed: bool,
    refused: Option<Unsupported>,
    decided: Option<usize>,
    glyphs: Vec<Rc<Glyph>>,
    named: hash::Set<Rc<str>>,
    state: State,
    frames: Vec<Frame>,
    held: Vec<Mark>,
    held_failed: bool,
    path: Vec<Subpath>,
}

impl Default for Out {
    fn default() -> Self {
        Self::with(None, 0.0, f64::INFINITY)
    }
}

impl Out {
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
            content: Marks::default(),
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
            held: Vec::new(),
            held_failed: false,
            path: Vec::new(),
        }
    }

    pub(super) fn region(&self) -> Region {
        self.state.region.clone()
    }

    pub(super) fn reach(&self, stroke: bool) -> f64 {
        match stroke_reach(self.state.width) {
            _ if !stroke => 0.0,
            r if r.is_nan() => f64::INFINITY,
            r => r,
        }
    }

    pub(super) fn meets(&self, points: &[Point]) -> bool {
        self.state.region.meets(points)
    }

    pub(super) fn slack(&self, most: f64, local: Rounding) -> f64 {
        self.state.region.slack(most, local)
    }

    pub(super) fn finish<T>(
        self,
        drew: Result<T, Refused>,
    ) -> Result<(T, Marks, Vec<Rc<Glyph>>), Unsupported> {
        if let Some(why) = self.refused {
            return Err(why);
        }
        match drew {
            Ok(drew) if !self.failed => Ok((drew, self.content, self.glyphs)),
            // A refusal is made only where one of these is recorded.
            _ => Err(Unsupported::Coordinates),
        }
    }

    // A refusal is evaluated under the clips active at the point where the part would paint.
    pub(super) fn refuse(&mut self, why: Unsupported, part: Part) -> Result<(), Refused> {
        let depth = self.frames.len();
        self.decided = Some(self.decided.map_or(depth, |d| d.min(depth)));
        if !self.shows(&part) {
            return Ok(());
        }
        self.refused.get_or_insert(why);
        Err(Refused(()))
    }

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

    pub(super) fn shows(&self, part: &Part) -> bool {
        self.state.region.shows(part)
    }

    pub(super) fn wrote(&mut self, drew: Option<()>) -> Result<(), Refused> {
        drew.ok_or_else(|| {
            debug_assert!(self.failed, "Out gives None only where a piece failed");
            self.failed = true;
            Refused(())
        })
    }

    pub(super) fn writes(
        &mut self,
        draw: impl FnOnce(&mut Self) -> Option<()>,
    ) -> Result<(), Refused> {
        let drew = draw(self);
        self.wrote(drew)
    }

    fn release(&mut self) {
        self.content.0.append(&mut self.held);
        self.failed |= self.held_failed;
        self.held_failed = false;
        for f in &mut self.frames {
            f.written = true;
        }
    }

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
        self.content.0.push(Mark::Glyph(glyph.clone()));
        if self.named.insert(glyph.name.clone()) {
            self.glyphs.push(glyph.clone());
        }
        Some(true)
    }

    pub(super) fn nums<const N: usize>(
        &mut self,
        nums: &[f64; N],
        build: fn([f32; N]) -> Mark,
    ) -> Option<()> {
        self.act(
            build(nums.map(|v| v as f32)),
            nums.iter().all(|v| written(*v).is_some()),
        );
        Some(())
    }

    pub(super) fn computed<const N: usize>(
        &mut self,
        nums: &[f64; N],
        build: fn([f32; N]) -> Mark,
        local: Rounding,
    ) -> Option<()> {
        self.nums(nums, build)?;
        if let Some(sub) = self.path.last_mut() {
            sub.rounding = sub.rounding.either(local);
        }
        Some(())
    }

    pub(super) fn op(&mut self, mark: Mark) {
        self.act(mark, true);
    }

    fn act(&mut self, mark: Mark, ok: bool) {
        match mark {
            Mark::Move(_) | Mark::Line(_) | Mark::Curve(_) | Mark::Rect(_) | Mark::Close => {
                if matches!(mark, Mark::Move(_) | Mark::Rect(_)) || self.path.is_empty() {
                    self.path.push(Subpath::new());
                }
                let sub = self.path.last_mut().unwrap();
                match mark {
                    Mark::Move([x, y]) | Mark::Line([x, y]) => {
                        sub.add(Point::new(x.into(), y.into()));
                    }
                    Mark::Curve(v) => {
                        for [x, y] in v.as_chunks::<2>().0 {
                            sub.add(Point::new((*x).into(), (*y).into()));
                        }
                    }
                    Mark::Rect(v) => {
                        let [x, y, w, h] = v.map(f64::from);
                        for p in [(x, y), (x + w, y), (x + w, y + h), (x, y + h)] {
                            sub.add(p.into());
                        }
                    }
                    _ => {}
                }
                sub.marks.push(mark);
                sub.failed |= !ok;
            }
            Mark::Paint {
                stroke,
                fill,
                close,
            } => {
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
                    self.content.0.extend(p.marks);
                    self.failed |= p.failed;
                }
                self.content.0.push(Mark::Paint {
                    close: close && last_kept,
                    fill,
                    stroke,
                });
            }
            Mark::Discard => {
                self.path.clear();
            }
            Mark::Clip => self.clip_path(),
            Mark::Save => {
                self.frames.push(Frame {
                    saved: self.state.clone(),
                    held_at: self.held.len(),
                    held_failed: self.held_failed,
                    written: false,
                });
                self.held.push(mark);
            }
            Mark::Restore => {
                let Some(frame) = self.frames.pop() else {
                    self.content.0.push(mark);
                    return;
                };
                if frame.written {
                    // What was set since the last thing painted in it is not needed.
                    self.held.clear();
                    self.held_failed = false;
                    self.content.0.push(mark);
                } else {
                    self.held.truncate(frame.held_at);
                    self.held_failed = frame.held_failed;
                }
                self.state = frame.saved;
                if self.decided.is_some_and(|d| d > self.frames.len()) {
                    self.decided = None;
                }
            }
            Mark::Glyph(_) => unreachable!("glyphs are placed by form()"),
            _ => {
                if let Mark::Transform(v) = mark {
                    self.state.region.transform(Affine::new(v.map(f64::from)));
                }
                if let Mark::Width(w) = mark {
                    self.state.width = w.into();
                }
                self.held.push(mark);
                self.held_failed |= !ok;
            }
        }
    }

    pub(super) fn path(&mut self, path: &kurbo::BezPath) -> Option<()> {
        use kurbo::PathEl;
        let mut last = Point::ZERO;
        for el in path.elements() {
            match *el {
                PathEl::MoveTo(p) => {
                    self.nums(&[p.x, p.y], Mark::Move)?;
                    last = p;
                }
                PathEl::LineTo(p) => {
                    self.nums(&[p.x, p.y], Mark::Line)?;
                    last = p;
                }
                PathEl::QuadTo(c, p) => {
                    let a = last + (c - last) * (2.0 / 3.0);
                    let b = p + (c - p) * (2.0 / 3.0);
                    self.nums(&[a.x, a.y, b.x, b.y, p.x, p.y], Mark::Curve)?;
                    last = p;
                }
                PathEl::CurveTo(a, b, p) => {
                    self.nums(&[a.x, a.y, b.x, b.y, p.x, p.y], Mark::Curve)?;
                    last = p;
                }
                PathEl::ClosePath => self.op(Mark::Close),
            }
        }
        Some(())
    }

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

    pub(super) fn fill_color(&mut self, color: &Color) -> Option<()> {
        self.color(*color, false)
    }

    pub(super) fn stroke_color(&mut self, color: &Color) -> Option<()> {
        self.color(*color, true)
    }

    fn color(&mut self, color: Color, stroke: bool) -> Option<()> {
        let component = |v: f64| v.clamp(0., 1.) as f32;
        let components = match color {
            Color::Gray(v) => super::marks::Ink::Gray([component(v)]),
            Color::Rgb(v) => super::marks::Ink::Rgb(v.map(component)),
            Color::Cmyk(v) => super::marks::Ink::Cmyk(v.map(component)),
        };
        self.act(
            Mark::Color { stroke, components },
            components.components().iter().all(|v| v.is_finite()),
        );
        Some(())
    }

    pub(super) fn stroke_state(&mut self, width: f64, dash: Option<&Rc<Dash>>) -> Option<()> {
        self.op(Mark::Round(false));
        self.nums(&[MITER_LIMIT], |[m]| Mark::Miter(m))?;
        self.nums(&[width], |[w]| Mark::Width(w))?;
        match dash {
            None => self.held.push(Mark::Dash(Rc::from([]))),
            Some(dash) => {
                self.held.push(Mark::Dash(dash.values.clone()));
                self.held_failed |= !dash.ok;
            }
        }
        Some(())
    }

    pub(super) fn clip(&mut self, r: kurbo::Rect) -> Option<()> {
        self.clips_after_decision();
        let (ok, corners, rect) = self.clip_rect(r)?;
        self.state.region.cut(&corners);
        // The path being built, if any, ends in the clip, unpainted.
        self.path.clear();
        self.held.extend([Mark::Rect(rect), Mark::Clip]);
        self.held_failed |= !ok;
        Some(())
    }

    fn clips_after_decision(&mut self) {
        if self.decided.is_some() {
            debug_assert!(false, "a clip written after a part under it was told");
            self.failed = true;
        }
    }

    fn clip_rect(&self, r: kurbo::Rect) -> Option<(bool, [Point; 4], [f32; 4])> {
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

            *v = f64::from(n);
        }

        let [x, y, w, h] = v;
        let ctm = self.state.region.ctm;
        let corners =
            [(x, y), (x + w, y), (x + w, y + h), (x, y + h)].map(|(x, y)| ctm * Point::new(x, y));
        Some((ok, corners, v.map(|n| n as f32)))
    }

    fn clip_path(&mut self) {
        self.clips_after_decision();
        let path = std::mem::take(&mut self.path);
        let mut points = vec![];
        for p in path {
            if p.extent.lost {
                points.push(Point::new(f64::NAN, f64::NAN));
            } else {
                points.extend(&p.points);
            }
            self.held.extend(p.marks);
            self.held_failed |= p.failed;
        }
        self.held.push(Mark::Clip);
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
    use crate::Unsupported;
    use crate::marks::Mark;
    use kurbo::{Point, Rect};

    const PAGE: Rect = Rect::new(0.0, 0.0, 200.0, 200.0);

    #[test]
    fn what_cannot_paint_is_left_out() {
        // A square far off the page, its numbers past what can be written.
        let mut out = Out::new(PAGE, 1.0);
        out.op(Mark::Save);
        out.nums(&[1.0, 0.0, 0.0, 1.0, 1e7, 1e7], Mark::Transform);
        out.clip(Rect::new(0.0, 0.0, 100.0, 100.0));
        out.nums(&[0.0, 0.0, 100.0, 100.0], Mark::Rect);
        out.op(Mark::FILL);
        out.op(Mark::Restore);
        assert_eq!(
            out.finish(Ok(())).map(|c| c.1.to_string()),
            Ok(String::new())
        );
        // A stroke 1 wide 3 below the page reaches 5 from its line: it paints
        // within a quarter of a point of the page; at 4.5 below, it does not.
        for (y, drawn) in [(-3.0, true), (-5.3, false)] {
            let mut out = Out::new(PAGE, 1.0);
            out.stroke_state(1.0, None);
            out.nums(&[10.0, y], Mark::Move);
            out.nums(&[100.0, y], Mark::Line);
            out.op(Mark::STROKE);
            let content = out.finish(Ok(())).unwrap().1.to_string();
            assert_eq!(content.contains(" l\nS\n"), drawn, "{y}: {content}");
        }
        // Of a path, the subpath off the page is left out; the one closed by `s`
        // being left out, the stroke closes none.
        let mut out = Out::new(PAGE, 1.0);
        out.nums(&[10.0, 10.0], Mark::Move);
        out.nums(&[20.0, 10.0], Mark::Line);
        out.nums(&[1e7, 10.0], Mark::Move);
        out.nums(&[1e7, 20.0], Mark::Line);
        out.op(Mark::CLOSE_STROKE);
        assert_eq!(
            out.finish(Ok(())).unwrap().1.to_string(),
            "10 10 m\n20 10 l\nS\n"
        );
        // Partly on the page, a number that cannot be written fails it.
        let mut out = Out::new(PAGE, 1.0);
        out.nums(&[-1e7, 10.0], Mark::Move);
        out.nums(&[20.0, 10.0], Mark::Line);
        out.op(Mark::STROKE);
        assert!(out.finish(Ok(())).is_err());
    }

    #[test]
    fn a_part_is_told_under_the_clips_written() {
        let mut out = Out::new(PAGE, 1.0);
        out.op(Mark::Save);
        out.clip(Rect::new(300.0, 0.0, 400.0, 200.0));
        let part = || Part::rect(Rect::new(100.0, 100.0, 350.0, 150.0));
        assert!(out.refuse(Unsupported::Character('\0'), part()).is_ok());
        out.op(Mark::Restore);
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

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "a clip written after"))]
    fn a_clip_after_a_part_told_fails() {
        let mut out = Out::new(PAGE, 1.0);
        out.op(Mark::Save);
        let part = Part::rect(Rect::new(250.0, 0.0, 300.0, 50.0));
        assert!(out.refuse(Unsupported::Character('\0'), part).is_ok());
        out.op(Mark::Save);
        out.clip(Rect::new(0.0, 0.0, 50.0, 50.0));
        assert_eq!(
            out.finish(Ok(())).map(|_| ()),
            Err(Unsupported::Coordinates)
        );
    }

    #[test]
    fn clips_are_cut_to_the_page_and_of_no_area_show_nothing() {
        let mut out = Out::new(PAGE, 1.0);
        out.clip(Rect::new(-1e7, 10.0, 100.0, 20.0));
        out.nums(&[0.0, 10.0, 10.0, 10.0], Mark::Rect);
        out.op(Mark::FILL);
        let marks = out.finish(Ok(())).unwrap().1;
        assert!(
            matches!(&marks.0[..], [Mark::Rect(rect), Mark::Clip, ..]
            if *rect == [-1.45025, 10.0, 101.45025, 10.0]),
            "{marks}"
        );
        let mut out = Out::new(PAGE, 1.0);
        out.clip(Rect::new(1.0, 1.0, 1.0, 39.0));
        assert!(out.region().is_empty());
        out.nums(&[0.0, 0.0, 10.0, 10.0], Mark::Rect);
        out.op(Mark::FILL);
        assert_eq!(out.finish(Ok(())).unwrap().1.to_string(), "");
    }

    fn next(seed: &mut u64) -> f64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed >> 11) as f64 / (1_u64 << 53) as f64
    }

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
            out.nums(&m, Mark::Transform);
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
