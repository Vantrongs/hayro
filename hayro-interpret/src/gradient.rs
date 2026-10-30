//! The colors of axial and radial shadings by their parameter, as stops of a
//! gradient whose error is known.
//!
//! PDF shadings are expensive to evaluate: a function, a color space, a tint
//! transform's program. So their colors are found once at stops and looked up
//! between them, by the shading's own sampling and, for an axial shading, as an
//! SVG-like linear gradient a renderer draws natively. Where the color is
//! straight between two inputs of the function, as a sampled function's is
//! between its samples, the stops are those inputs, and exact
//! ([`Function::span`](crate::Function)); each step of the function is two stops
//! at one place, the values from either side. Elsewhere the color is tabulated
//! at device resolution: at least a stop for each step the parameter takes
//! from a pixel to its neighbour ([`EncodedRadialAxialShading::grid_step`]), so a
//! feature of the function a pixel can show is between no two stops.

use crate::color::{AlphaColor, ColorSpace};
use crate::encode::{
    Cost, EncodedRadialAxialShading, EncodedShadingPattern, Exhausted, Meter, RadialAxialParams,
    Unmetered, take,
};
use crate::function::Values;
use crate::function::shape::{self, Piece};
use crate::interpret::state::ActiveTransferFunction;
use hayro_syntax::hash;
use kurbo::{Affine, Point, Rect};
use smallvec::smallvec;
use std::sync::{Mutex, OnceLock};

/// How far, in 8-bit levels, a stop's color may lie off the line through its
/// neighbours before the intervals beside it are split
/// ([`refine`]): more than the colors' own rounding
/// to 8 bits puts it off, a level at most. Lines between stops so placed stray
/// from a curve by about a fourth of it.
const BEND_LEVELS: f32 = 1.0;

/// [`BEND_LEVELS`] for colors not rounded, gray's and RGB's.
const STRAIGHT_BEND_LEVELS: f32 = 0.5;

/// Into how many parts at most refining splits a step of the grid.
const BEND_SPLITS: u16 = 16;

/// How far, in 8-bit levels, an entry of a renderer's table of a gradient's
/// colors may lie from its stops ([`SvgGradient::paints_within_a_level`]):
/// a level, and half of one for the rounding of each.
pub(crate) const LEVEL_TOLERANCE: f32 = 1.5;

/// An SVG-like gradient.
pub struct SvgGradient {
    /// The kind of the gradient.
    pub kind: SvgGradientKind,
    /// The transform from the gradient's own space, where its line runs from
    /// its `start` to its `end`, into the renderer's coordinate space.
    pub transform: Affine,
    /// The stops of the gradient in the sRGB color space, the first at its
    /// start and the last at its end: the shading's stops from the one at or
    /// before the start of the part a shape reaches to the one at or past its
    /// end, so a renderer's table of their colors spans that part alone.
    pub stops: Vec<SvgGradientStop>,
    /// The part of the gradient, from its start, 0, to its end, 1, the shape it
    /// was made for reaches: past it the stops do not tell the shading's colors.
    pub range: (f32, f32),
    /// The most the gradient's parameter changes over a unit of the space it is
    /// drawn in, a device pixel.
    pub pixel_step: f32,
    /// For each of `stops`, how far its color moves where hayro's color tables
    /// read a component of the function's outputs there as the next 8-bit value
    /// ([`table_step`]), as much as the shading drawn directly may be off
    /// there; empty for gray and RGB, which read no tables.
    pub table_steps: Vec<f32>,
}

/// The kind of the SVG gradient.
pub enum SvgGradientKind {
    /// Linear gradient.
    Linear {
        /// The normalized start point of the gradient line.
        start: Point,
        /// The normalized end point of the gradient line.
        end: Point,
    },
}

/// A color stop for an SVG-like gradient.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvgGradientStop {
    /// Normalized offset.
    pub offset: f32,
    /// RGBA color components in sRGB.
    pub color: [f32; 4],
}

impl SvgGradient {
    /// Whether a renderer that paints a point at `t` the entry
    /// `trunc(t × (lut.len() - 1))` of `lut`, premultiplied colors of 8 bits in
    /// equal steps from 0 to 1, paints every point of `range` within a level of
    /// these stops, as their colors rounded to 8 bits: within 1.5 levels of them
    /// at every place an entry is read for, and by the [`table_steps`](Self::table_steps) of the
    /// stops there besides, the most a pixel of the shading drawn directly may
    /// be off where hayro's color tables step. vello's gradients are drawn so. A
    /// feature of the stops narrower than refining resolves
    /// ([`BEND_SPLITS`] parts of a pixel's step), as a step of the function is,
    /// within an entry may be missed by as much as it changes where an entry is
    /// no longer than a pixel's step, so it lies within a pixel of each point
    /// painted wrong; a longer one fails. Linear in the entries and stops.
    pub fn paints_within_a_level(&self, lut: &[[u8; 4]]) -> bool {
        let n = lut.len();
        if n < 2 || self.stops.is_empty() {
            return false;
        }
        let cell = 1.0 / (n - 1) as f64;
        let (lo, hi) = (f64::from(self.range.0), f64::from(self.range.1));
        let first = ((lo / cell).floor() as usize).min(n - 1);
        let last = ((hi / cell).floor() as usize).min(n - 1);
        let stops = &self.stops;
        let offset = |k: usize| f64::from(stops[k].offset);
        let narrow = f64::from(self.pixel_step) / f64::from(BEND_SPLITS);
        let excusable = cell <= f64::from(self.pixel_step);
        let allowance = |k: usize| self.table_steps.get(k).copied().unwrap_or(0.0);
        let around = |next: usize| allowance(next).max(next.checked_sub(1).map_or(0.0, allowance));
        let off = |color: [f32; 4], entry: &[u8; 4], allowed: f32| {
            let allowed = f64::from(LEVEL_TOLERANCE + allowed);
            let alpha = f64::from(color[3]);
            let premultiplied = [
                f64::from(color[0]) * alpha,
                f64::from(color[1]) * alpha,
                f64::from(color[2]) * alpha,
                alpha,
            ];
            premultiplied
                .iter()
                .zip(entry)
                .any(|(want, &got)| (f64::from(got) - want * 255.0).abs() >= allowed)
        };
        // The first stop at or past the start of the entry, moving with it.
        let mut begin = 0;
        for (i, entry) in lut.iter().enumerate().take(last + 1).skip(first) {
            let (a, b) = ((i as f64 * cell).max(lo), ((i + 1) as f64 * cell).min(hi));
            while begin < stops.len() && offset(begin) < a {
                begin += 1;
            }
            let mut end = begin;
            while end < stops.len() && offset(end) <= b {
                end += 1;
            }
            let inside = &stops[begin..end];
            // The stops just past `a`, and from `b` on.
            let mut past_a = begin;
            while past_a < stops.len() && offset(past_a) <= a {
                past_a += 1;
            }
            let from_b = end
                - inside
                    .iter()
                    .rev()
                    .take_while(|s| f64::from(s.offset) >= b)
                    .count();
            // The lines the entry is read across: from the one through `a` to
            // the one through `b`.
            let read = &stops[past_a.saturating_sub(1)..(from_b + 1).min(stops.len())];
            let sharp = read
                .windows(2)
                .any(|w| f64::from(w[1].offset - w[0].offset) < narrow && w[0].color != w[1].color);
            if sharp && excusable {
                continue;
            }
            if off(lerp_between(stops, past_a, a as f32), entry, around(past_a))
                || off(lerp_between(stops, from_b, b as f32), entry, around(from_b))
                || inside
                    .iter()
                    .zip(begin..)
                    .any(|(s, k)| off(s.color, entry, allowance(k)))
            {
                return false;
            }
        }
        true
    }

    /// Whether every stop is of one color, so a renderer that paints that color
    /// alone paints them all.
    pub fn is_one_color(&self) -> bool {
        self.stops.windows(2).all(|w| w[0].color == w[1].color)
    }

    /// The work [`paints_within_a_level`](Self::paints_within_a_level) and
    /// building the table of `entries` it checks take.
    pub fn table_check_cost(&self, entries: usize) -> Cost {
        Cost::of(entries.saturating_mul(2).saturating_add(self.stops.len()))
    }

    /// Whether every table of at least `entries` colors that holds at each
    /// entry the stops' color at its place, rounded to 8 bits, paints within
    /// a level of the stops, as [`paints_within_a_level`](Self::paints_within_a_level)
    /// would find of it, from the stops alone: an entry is read up to an entry
    /// after its place, so off by half a level of rounding and by as much as
    /// the colors change over an entry before the place read, which the
    /// steepest line within an entry before it bounds. Opaque stops only, and
    /// lines no steeper than [`TABLE_BOUND_LINES`] in an entry; `false`, for
    /// the table to be checked, otherwise. Linear in the stops.
    pub fn within_a_level_of_any_table(&self, entries: usize) -> bool {
        let stops = &self.stops;
        if entries < 2 || stops.len() < 2 || stops.iter().any(|s| s.color[3] != 1.0) {
            return false;
        }
        let cell = 1.0 / (entries - 1) as f64;
        let budget = f64::from(LEVEL_TOLERANCE) - 0.5;
        // Each line's steepest channel, in 8-bit levels over an entry.
        let over_entry = |k: usize| -> f64 {
            let (a, b) = (stops[k], stops[k + 1]);
            let most = (0..3)
                .map(|c| f64::from((b.color[c] - a.color[c]).abs()))
                .fold(0.0, f64::max);
            if most == 0.0 {
                return 0.0;
            }
            let width = f64::from(b.offset - a.offset);
            if width <= 0.0 {
                return f64::INFINITY;
            }
            most * 255.0 * cell / width
        };
        let mut first = 0;
        for k in 0..stops.len() - 1 {
            // The lines within an entry before any place on line `k`.
            let from = f64::from(stops[k].offset) - cell;
            while first < k && f64::from(stops[first + 1].offset) <= from {
                first += 1;
            }
            if k - first >= TABLE_BOUND_LINES {
                return false;
            }
            if (first..=k).map(over_entry).fold(0.0, f64::max) >= budget {
                return false;
            }
        }
        true
    }

    /// The [`row`](Self::row) that paints within a level of the stops
    /// ([`row_paints_within_a_level`](Self::row_paints_within_a_level)), of the
    /// fewest texels a pixel among [`ROW_TEXELS_PER_PIXEL`] and no more than
    /// `max_texels` texels: `None` when none does, as where the stops step within [`range`](Self::range) by twice
    /// the tolerance or more, which a row read between its texels blends over
    /// a texel at any width, found before any row is made. Each row
    /// made, and checked, is taken of `meter` first.
    pub fn row_within_a_level_metered(
        &self,
        meter: &mut dyn Meter,
        max_texels: usize,
    ) -> Result<Option<GradientRow>, Exhausted> {
        let (r0, r1) = (f64::from(self.range.0), f64::from(self.range.1));
        take(meter, Cost::of(self.stops.len()))?;
        // A step of twice the tolerance or more is off by half of it or more
        // from one side or the other wherever it lies between two texels.
        let premultiplied = |c: [f32; 4]| [c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3]];
        let allowance = |k: usize| self.table_steps.get(k).copied().unwrap_or(0.0);
        let steps = (1..self.stops.len()).any(|k| {
            let (a, b) = (self.stops[k - 1], self.stops[k]);
            a.offset == b.offset
                && (r0..=r1).contains(&f64::from(a.offset))
                && levels_apart(&premultiplied(a.color), &premultiplied(b.color))
                    >= 2.0 * (LEVEL_TOLERANCE + allowance(k - 1).max(allowance(k)))
        });
        if steps {
            return Ok(None);
        }
        let pixels = (r1 - r0) / f64::from(self.pixel_step).max(f64::from(f32::MIN_POSITIVE));
        for per_pixel in ROW_TEXELS_PER_PIXEL {
            let texels = (pixels * f64::from(per_pixel)).ceil().max(1.0);
            if texels > max_texels as f64 {
                return Ok(None);
            }
            let texels = texels as usize;
            take(
                meter,
                Cost::of(texels.saturating_mul(3).saturating_add(self.stops.len())),
            )?;
            if !meter.hold(texels.saturating_mul(2 * size_of::<[f32; 4]>())) {
                return Err(Exhausted);
            }
            let row = self.row(per_pixel);
            if self.row_paints_within_a_level(&row) {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    /// The colors of a texture one texel high that holds the gradient along
    /// its line, over the part of it its shape reaches: `per_pixel` texels for
    /// each device pixel it spans, each the stops' color at the place of its
    /// centre; and the transform from the texture to the gradient's own space,
    /// where its line runs along `x` ([`transform`](Self::transform) takes
    /// that to the device). A renderer that reads a texture between the
    /// centres of its texels, padded past its ends, draws the gradient so.
    pub fn row(&self, per_pixel: u32) -> GradientRow {
        let SvgGradientKind::Linear { start, end } = self.kind;
        let (r0, r1) = (f64::from(self.range.0), f64::from(self.range.1));
        let pixels = (r1 - r0) / f64::from(self.pixel_step).max(f64::from(f32::MIN_POSITIVE));
        let texels = ((pixels * f64::from(per_pixel)).ceil() as usize).max(1);
        let width = (r1 - r0) / texels as f64;
        let colors = (0..texels)
            .map(|i| color_at(&self.stops, (r0 + (i as f64 + 0.5) * width) as f32))
            .collect();
        let length = end.x - start.x;
        GradientRow {
            colors,
            to_gradient: Affine::new([width * length, 0.0, 0.0, 1.0, start.x + r0 * length, 0.0]),
        }
    }

    /// Whether `row`, its colors rounded to 8 bits and read between the
    /// centres of its texels, padded past its ends, paints every point of
    /// [`range`](Self::range) within a level of the stops, as
    /// [`paints_within_a_level`](Self::paints_within_a_level) checks a table:
    /// within 1.5 levels at each stop and by its [`table_steps`](Self::table_steps)
    /// besides, once 8-bit reads are rounded, a feature narrower than a pixel
    /// too, as the shading sampled at each pixel paints it. Linear in the
    /// texels and stops.
    pub fn row_paints_within_a_level(&self, row: &GradientRow) -> bool {
        let n = row.colors.len();
        let stops = &self.stops;
        if n == 0 || stops.is_empty() {
            return false;
        }
        let premultiplied = |c: [f32; 4]| [c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3]];
        // The texels as a renderer holds them: premultiplied, in 8 bits.
        let rounded: Vec<[f32; 4]> = row
            .colors
            .iter()
            .map(|&c| premultiplied(c).map(|v| (v * 255.0 + 0.5).floor() / 255.0))
            .collect();
        let (r0, r1) = (f64::from(self.range.0), f64::from(self.range.1));
        let width = (r1 - r0) / n as f64;
        let centre = |i: usize| r0 + (i as f64 + 0.5) * width;
        let allowance = |k: usize| self.table_steps.get(k).copied().unwrap_or(0.0);
        // The row as read at `at`, and how far apart the two texels read there
        // lie, in levels.
        let read = |at: f64| -> ([f32; 4], f64) {
            let j = ((at - r0) / width - 0.5).floor();
            if j < 0.0 {
                return (rounded[0], 0.0);
            }
            let i = (j as usize).min(n - 1);
            if i + 1 >= n {
                return (rounded[n - 1], 0.0);
            }
            let (a, b) = (rounded[i], rounded[i + 1]);
            let apart = (0..4)
                .map(|c| f64::from((b[c] - a[c]).abs()) * 255.0)
                .fold(0.0, f64::max);
            (lerp_color(a, b, ((at - centre(i)) / width) as f32), apart)
        };
        // A renderer that reads in 8 bits rounds the color read, and the
        // weight of each texel, to 8 bits: off by half a level and by the
        // texels' difference over 510 besides.
        let off = |want: [f32; 4], (got, apart): ([f32; 4], f64), allowed: f32| {
            let want = premultiplied(want);
            let allowed = f64::from(LEVEL_TOLERANCE + allowed) - 0.5 - apart / 510.0;
            (0..4).any(|c| (f64::from(got[c]) - f64::from(want[c])).abs() * 255.0 >= allowed)
        };
        let start = stops.partition_point(|s| f64::from(s.offset) < r0);
        let end = stops.partition_point(|s| f64::from(s.offset) <= r1);
        // The colors between the stops and the row are both straight but at
        // the stops and the texels' centres, where the texels hold the
        // colors, so they lie farthest apart at the stops and the ends.
        let around = |next: usize| {
            allowance(next.min(stops.len() - 1)).max(next.checked_sub(1).map_or(0.0, allowance))
        };
        for at in [r0, r1] {
            let next = stops.partition_point(|s| f64::from(s.offset) <= at);
            if off(lerp_between(stops, next, at as f32), read(at), around(next)) {
                return false;
            }
        }
        (start..end).all(|k| {
            let at = f64::from(stops[k].offset);
            !off(stops[k].color, read(at), allowance(k))
        })
    }
}

/// A gradient as a texture one texel high ([`SvgGradient::row`]).
pub struct GradientRow {
    /// The texels' colors, straight RGBA in sRGB, from the start of the part
    /// of the gradient its shape reaches to its end.
    pub colors: Vec<[f32; 4]>,
    /// The transform from the texture, a texel a unit, to the gradient's own
    /// space.
    pub to_gradient: Affine,
}

/// How many texels a device pixel a gradient's [`row`](SvgGradient::row) is
/// tried at: two, so a step read between two texels is read so within the
/// pixel it falls in, and more where the colors bend between texels.
pub const ROW_TEXELS_PER_PIXEL: [u32; 4] = [2, 4, 8, 16];

/// The most lines of stops within an entry of a table before a place that
/// [`SvgGradient::within_a_level_of_any_table`] bounds the colors by; past
/// it the table is built and checked.
const TABLE_BOUND_LINES: usize = 16;

impl EncodedRadialAxialShading {
    /// Convert an axial shading into an SVG-like gradient over the part of it
    /// `path_bbox` reaches.
    pub fn as_svg_gradient(
        &self,
        pattern: &EncodedShadingPattern,
        path_bbox: Rect,
    ) -> Option<SvgGradient> {
        self.as_svg_gradient_metered(pattern, path_bbox, &mut Unmetered)
            .expect("an unmetered conversion is never refused")
    }

    /// [`as_svg_gradient`](Self::as_svg_gradient), taking its work and memory of
    /// `meter` before it does it ([`stops_metered`](Self::stops_metered)).
    /// `Exhausted` when `meter` refuses; `Ok(None)` for a shading no such
    /// gradient draws: a radial one, or an axial one whose ends `path_bbox`
    /// reaches past where it does not extend.
    pub fn as_svg_gradient_metered(
        &self,
        pattern: &EncodedShadingPattern,
        path_bbox: Rect,
        meter: &mut dyn Meter,
    ) -> Result<Option<SvgGradient>, Exhausted> {
        // A two-point radial gradient of an SVG renderer tells a cone from
        // circles that touch within a tolerance and paints in `f32`, as vello's
        // does: it paints near-touching circles, and circles of a thousandth of
        // a unit, as others, and points 10^9 units out as on their neighbours'
        // circles. The shading paints its points itself.
        let RadialAxialParams::Axial = self.params else {
            return Ok(None);
        };
        let reached = pattern.base_transform.transform_rect_bbox(path_bbox);
        let (min_x, max_x) = (reached.min_x(), reached.max_x());
        if !((self.extend[0] || min_x >= 0.0) && (self.extend[1] || max_x <= 1.0)) {
            return Ok(None);
        }
        let range = (min_x.clamp(0.0, 1.0) as f32, max_x.clamp(0.0, 1.0) as f32);
        let [a, _, c, _, _, _] = pattern.base_transform.as_coeffs();
        take(meter, pattern.setup_cost())?;
        let mut stops = self.stops_metered(pattern, range, 1.0, meter)?;
        let table_steps = self.table_steps_metered(pattern, &stops, meter)?;
        // The gradient runs from its first stop to its last, so a renderer's
        // table of its colors spans only the part the shape reaches.
        let (first, last) = match (stops.first(), stops.last()) {
            (Some(first), Some(last)) if first.offset < last.offset => (first.offset, last.offset),
            _ => (0.0, 1.0),
        };
        let length = last - first;
        for stop in &mut stops {
            stop.offset = ((stop.offset - first) / length).clamp(0.0, 1.0);
        }
        let along = |t: f32| ((t - first) / length).clamp(0.0, 1.0);
        // The line runs from (0, 0) to (1, 0) of the gradient's own space, and
        // the transform takes that to the stops' part of the shading's: a
        // renderer that takes ends closer than a threshold for one point, as
        // vello does before it applies the transform, then sees ends a unit
        // apart however close the stops lie in the shading's parameter.
        let onto_stops = Affine::new([f64::from(length), 0.0, 0.0, 1.0, f64::from(first), 0.0]);
        Ok(Some(SvgGradient {
            kind: SvgGradientKind::Linear {
                start: Point::ZERO,
                end: Point::new(1.0, 0.0),
            },
            transform: pattern.base_transform.inverse() * onto_stops,
            stops,
            range: (along(range.0), along(range.1)),
            pixel_step: (a.hypot(c) / f64::from(length)) as f32,
            table_steps,
        }))
    }

    /// [`SvgGradient::table_steps`] of `stops`, of the shading's parameter: in
    /// a color space other than gray and RGB, the [`table_step`] at each stop,
    /// of the function's outputs at it, as the shading evaluates them; kept in
    /// its [`StopMemo`], and each taken of `meter` before it is found: the
    /// function evaluated and its colors converted.
    fn table_steps_metered(
        &self,
        pattern: &EncodedShadingPattern,
        stops: &[SvgGradientStop],
        meter: &mut dyn Meter,
    ) -> Result<Vec<f32>, Exhausted> {
        let look = self.memo.look(pattern);
        if look.straight_space() {
            return Ok(Vec::new());
        }
        let look_key = look.key();
        let [t0, t1] = self.domain;
        let value_cost = pattern.value_cost();
        let mut found = self.memo.table_steps.lock().unwrap();
        let mut steps = Vec::with_capacity(stops.len());
        for stop in stops {
            let x = t0 + (t1 - t0) * stop.offset;
            let key = (look_key, x.to_bits());
            let step = match found.get(&key) {
                Some(step) => *step,
                None => {
                    take(meter, value_cost)?;
                    if !meter.hold(MEMO_ENTRY_BYTES) {
                        return Err(Exhausted);
                    }
                    let step = match self.function.eval(&smallvec![x]) {
                        Some(values) => {
                            take(meter, value_cost.times(2 * values.len() as u64))?;
                            look.table_step(&values)
                        }
                        None => 0.0,
                    };
                    if found.len() >= MEMO_ENTRIES {
                        found.clear();
                    }
                    found.insert(key, step);
                    step
                }
            };
            steps.push(step);
        }
        Ok(steps)
    }

    /// The stops of its colors over `range`, of its parameter from its start,
    /// 0, to its end, 1: from the stop at or before the range's start to the one
    /// at or past its end. Where the color is straight between points of the
    /// function's shape, those points, with one added wherever a channel crosses
    /// 0 or 1, where the colors are clamped; elsewhere, and wherever the color
    /// space or a transfer function bends the colors, the points of its shape
    /// ([`kept`]) and the color at each multiple of
    /// [`grid_step`](Self::grid_step) between them, refined where they bend
    /// ([`refine`]). Each piece of the shape ends in a stop of its
    /// own value there, so a step of the function is two stops at one place.
    ///
    /// The colors are looked up in the shading's [`StopMemo`] first, shared by
    /// every paint of it: a page that paints one shading in hundreds of shapes
    /// evaluates each color once.
    ///
    /// The work and memory are taken of `meter` first, but for the tables its
    /// colors are looked up in, which the caller takes
    /// ([`EncodedShadingPattern::setup_cost`]): the shape, the first time
    /// ([`shape_metered`](Self::shape_metered)), each stop held, and each value
    /// evaluated, as it is.
    pub(crate) fn stops_metered(
        &self,
        pattern: &EncodedShadingPattern,
        range: (f32, f32),
        scale: f32,
        meter: &mut dyn Meter,
    ) -> Result<Vec<SvgGradientStop>, Exhausted> {
        let [t0, t1] = self.domain;
        let (lo, hi) = (
            range.0.min(range.1).clamp(0.0, 1.0),
            range.0.max(range.1).clamp(0.0, 1.0),
        );
        let value_cost = pattern.value_cost();
        let look = self.memo.look(pattern);
        let look_key = look.key();
        // Lines are straight colors where the color space and no transfer
        // function bends them; their colors, and those evaluated, are then
        // those before they are rounded to 8 bits, so they bend only as the
        // function does.
        let lines_straight = look.lines_straight();
        let mut colors = self.memo.colors.lock().unwrap();
        // The color at `x` of the function, or of its point `values` there:
        // looked up, or found, taken of `meter` first, and kept.
        let mut at = |meter: &mut dyn Meter,
                      x: f32,
                      point: Option<(&Values, Side)>|
         -> Result<[f32; 4], Exhausted> {
            let side = point.map_or(Side::Evaluated, |(_, side)| side);
            let key = (look_key, x.to_bits(), side as u8);
            let color = match colors.get(&key) {
                Some(color) => *color,
                None => {
                    take(meter, value_cost)?;
                    if !meter.hold(MEMO_ENTRY_BYTES) {
                        return Err(Exhausted);
                    }
                    let color = match point {
                        Some((values, _)) => look.color(Some(values)),
                        None => look.color(self.function.eval(&smallvec![x]).as_ref()),
                    };
                    if colors.len() >= MEMO_ENTRIES {
                        colors.clear();
                    }
                    colors.insert(key, color);
                    color
                }
            };
            Ok(color)
        };
        if t0 == t1 || !(t0.is_finite() && t1.is_finite()) {
            let color = at(meter, t0, None)?;
            return Ok(vec![
                SvgGradientStop { offset: 0.0, color },
                SvgGradientStop { offset: 1.0, color },
            ]);
        }
        // The input of the function at `t`, as the shading evaluates it.
        let x_at = |t: f32| t0 + (t1 - t0) * t;
        let t_of = |x: f32| (x - t0) / (t1 - t0);
        let (x_lo, x_hi) = {
            let (a, b) = (x_at(lo), x_at(hi));
            (a.min(b), a.max(b))
        };
        let shape = self.shape_metered(meter)?;
        let step = self.grid_step(pattern, scale);

        // What each piece the range reaches holds: the points of its lines, and
        // the multiples of the step within it and the range where the colors
        // bend.
        let reached: Vec<&StopPiece> = shape
            .iter()
            .filter(|piece| {
                let (a, b) = piece.span();
                b >= x_lo && a <= x_hi
            })
            .collect();
        let grid_in = |a: f32, b: f32| -> (i64, i64) {
            let (ta, tb) = (t_of(a.max(x_lo)), t_of(b.min(x_hi)));
            let (ta, tb) = (ta.min(tb), ta.max(tb));
            (
                (f64::from(ta) / f64::from(step)).floor() as i64 - 1,
                (f64::from(tb) / f64::from(step)).ceil() as i64 + 1,
            )
        };
        let mut stops_held = 0_u64;
        for piece in &reached {
            match piece {
                StopPiece::Lines(points) => {
                    let points = within(points, x_lo, x_hi);
                    let n = points.len() as u64;
                    if lines_straight {
                        // A stop where any of three channels crosses 0 or 1.
                        stops_held = stops_held.saturating_add(n.saturating_mul(7));
                    } else {
                        let (k0, k1) = grid_in(points[0].0, points[points.len() - 1].0);
                        let grid = (k1 - k0 + 1).max(0) as u64;
                        let n = kept(points, grid).count() as u64;
                        stops_held = stops_held.saturating_add(n).saturating_add(grid);
                    }
                }
                StopPiece::Curve(a, b) => {
                    let (k0, k1) = grid_in(*a, *b);
                    let grid = (k1 - k0 + 1).max(0) as u64 + 2;
                    stops_held = stops_held.saturating_add(grid);
                }
            }
        }
        take(meter, Cost::new(4).times(stops_held))?;
        if !meter.hold(
            usize::try_from(stops_held)
                .unwrap_or(usize::MAX)
                .saturating_mul(size_of::<SvgGradientStop>()),
        ) {
            return Err(Exhausted);
        }

        let mut stops: Vec<SvgGradientStop> =
            Vec::with_capacity(usize::try_from(stops_held).unwrap_or(0));
        let bend = if lines_straight {
            STRAIGHT_BEND_LEVELS
        } else {
            BEND_LEVELS
        };
        let finest = step / f32::from(BEND_SPLITS) * (t1 - t0).abs();
        for piece in reached {
            let mut nodes: Vec<(f32, f32, [f32; 4])> = Vec::new();
            match piece {
                StopPiece::Lines(points) => {
                    let points = within(points, x_lo, x_hi);
                    if lines_straight {
                        straight_lines(&look, points, &t_of, &mut nodes);
                    } else {
                        let (a, b) = (points[0].0, points[points.len() - 1].0);
                        let (k0, k1) = grid_in(a, b);
                        for i in kept(points, (k1 - k0 + 1).max(0) as u64) {
                            let (x, values, side) = &points[i];
                            nodes.push((*x, t_of(*x), at(meter, *x, Some((values, *side)))?));
                        }
                        grid(&mut nodes, (k0, k1), step, (a, b), &x_at, &mut |x| {
                            at(meter, x, None)
                        })?;
                    }
                }
                StopPiece::Curve(a, b) => {
                    // Its ends of its own values: from above at its start, as a
                    // stitching evaluates at a bound, and from below at its end.
                    nodes.push((*a, t_of(*a), at(meter, *a, None)?));
                    let end = if b > a { b.next_down() } else { *b };
                    nodes.push((*b, t_of(*b), at(meter, end, None)?));
                    grid(
                        &mut nodes,
                        grid_in(*a, *b),
                        step,
                        (*a, *b),
                        &x_at,
                        &mut |x| at(meter, x, None),
                    )?;
                }
            }
            // In the order of the input; the ends stay first and last.
            nodes.sort_by(|p, q| p.0.total_cmp(&q.0));
            if !(lines_straight && matches!(piece, StopPiece::Lines(_))) {
                refine(&mut nodes, (finest, bend), meter, &t_of, &mut |meter, x| {
                    at(meter, x, None)
                })?;
            }
            for (_, t, color) in nodes {
                stops.push(SvgGradientStop { offset: t, color });
            }
        }
        if t1 < t0 {
            stops.reverse();
        }
        // From the stop at or before the range's start to the one at or past its
        // end; from the first where the range starts at it, whose value the
        // shading takes there and before it.
        let start = if stops.first().is_some_and(|s| lo <= s.offset) {
            0
        } else {
            stops.partition_point(|s| s.offset <= lo).saturating_sub(1)
        };
        let end = stops
            .partition_point(|s| s.offset <= hi)
            .min(stops.len().saturating_sub(1));
        if start > 0 || end + 1 < stops.len() {
            stops = stops[start..=end.max(start)].to_vec();
        }
        Ok(stops)
    }

    /// The function's shape over the shading's domain, found the first time,
    /// its work and memory taken of `meter` first.
    fn shape_metered(&self, meter: &mut dyn Meter) -> Result<&[StopPiece], Exhausted> {
        if let Some(shape) = self.memo.shape.get() {
            return Ok(shape);
        }
        let (steps, points) = self.function.shape_cost();
        take(meter, steps)?;
        let bytes = usize::try_from(points)
            .unwrap_or(usize::MAX)
            .saturating_mul(size_of::<StopPoint>());
        if !meter.hold(bytes) {
            return Err(Exhausted);
        }
        let [t0, t1] = self.domain;
        let shape = StopPiece::of(&self.function.shape(t0.min(t1), t0.max(t1)));
        Ok(self.memo.shape.get_or_init(|| shape))
    }

    /// The step of the shading's parameter its colors are tabulated at where
    /// they bend: the largest power of two no larger than the least it changes
    /// over a device pixel anywhere, `1 / scale` units of the space
    /// `base_transform` takes points from, so each pixel's neighbours lie at
    /// least a step away and a
    /// feature of the colors wider than a pixel's step holds a stop. An axial
    /// shading's parameter changes alike everywhere, by the length of the first
    /// row of `base_transform` scaled by a pixel; a radial one's, `t` of the circle
    /// `|p - t p1| = r0 + t (r1 - r0)` through `p`, by `1 / |u·p1 + r1 - r0|`
    /// along `u`, the unit vector from that circle's centre to `p`, so by at
    /// least `1 / (|p1| + |r1 - r0|)` in the shading's space, and by at least the
    /// smaller singular value of that scaled transform times that over a pixel; the
    /// line touching circles are painted by changes by at least half of it.
    /// Within 2^-24 and a half: `t` is no finer in `f32`.
    pub(crate) fn grid_step(&self, pattern: &EncodedShadingPattern, scale: f32) -> f32 {
        let [a, b, c, d, _, _] =
            (pattern.base_transform * Affine::scale(1.0 / f64::from(scale))).as_coeffs();
        let least = match &self.params {
            RadialAxialParams::Axial => a.hypot(c),
            RadialAxialParams::Radial { p1, r, .. } => {
                let det = (a * d - b * c).abs();
                let squares = a * a + b * b + c * c + d * d;
                let most = ((squares + (squares * squares - 4.0 * det * det).max(0.0).sqrt())
                    / 2.0)
                    .sqrt();
                (det / most) / (2.0 * p1.to_vec2().hypot() + (r.y - r.x).abs())
            }
        };
        const FINEST: f32 = 1.0 / (1 << 24) as f32;
        if least.partial_cmp(&f64::from(FINEST)) != Some(std::cmp::Ordering::Greater) {
            return FINEST;
        }
        if least >= 0.5 {
            return 0.5;
        }
        // The power of two at most `least`.
        let mut step = 0.5_f64;
        while step > least {
            step /= 2.0;
        }
        step as f32
    }
}

/// What finding a radial or axial shading's stops learns of it, kept for every
/// paint of the shading: a page may paint one in hundreds of shapes, each a
/// gradient over the part of it the shape reaches. Its colors and table steps
/// are found from a [`Look`] alone, which only the memo makes, of its own color
/// space, and kept under that look's key: paints under other transfer
/// functions or opacities find their own.
#[derive(Debug)]
pub struct StopMemo {
    /// The shading's color space, which every color kept is converted in.
    color_space: ColorSpace,
    /// Its function's shape over its domain, as stops hold it
    /// ([`EncodedRadialAxialShading::shape_metered`]).
    shape: OnceLock<Vec<StopPiece>>,
    /// The colors found, by the look, the input of the function, and whether
    /// it is the value evaluated there or a point of the shape and which of
    /// those at one input ([`Side`]). At most [`MEMO_ENTRIES`], then emptied.
    colors: Mutex<hash::Map<ColorKey, [f32; 4]>>,
    /// The [`Look::table_step`] found at each input of the function, by the
    /// look, at most [`MEMO_ENTRIES`], then emptied.
    table_steps: Mutex<hash::Map<(LookKey, u32), f32>>,
}

impl StopMemo {
    /// The memo of a shading in `color_space`.
    pub(crate) fn new(color_space: ColorSpace) -> Self {
        Self {
            color_space,
            shape: OnceLock::new(),
            colors: Mutex::default(),
            table_steps: Mutex::default(),
        }
    }

    /// How `pattern`, a paint of the memo's shading, colors the function's
    /// outputs.
    pub(crate) fn look<'a>(&'a self, pattern: &'a EncodedShadingPattern) -> Look<'a> {
        Look {
            color_space: &self.color_space,
            opacity: pattern.opacity,
            transfer: pattern.transfer_function.as_ref(),
        }
    }
}

/// How a paint of a shading colors its function's outputs: converted in the
/// shading's color space, then given the paint's opacity and transfer
/// function. Every color a [`StopMemo`] keeps is found from this alone, and
/// kept under its [`key`](Self::key).
pub(crate) struct Look<'a> {
    color_space: &'a ColorSpace,
    opacity: f32,
    transfer: Option<&'a ActiveTransferFunction>,
}

/// A [`Look`]'s place in a [`StopMemo`]: its opacity's bits, and what its
/// transfer function maps each 8-bit value to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct LookKey(u32, Option<u128>);

impl Look<'_> {
    /// What the look's colors depend on but the memo's color space, which is
    /// the same for every look of one memo.
    pub(crate) fn key(&self) -> LookKey {
        let Self {
            color_space: _,
            opacity,
            transfer,
        } = self;
        LookKey(opacity.to_bits(), transfer.map(crate::CacheKey::cache_key))
    }

    /// Whether the color space's colors are straight in the function's outputs.
    fn straight_space(&self) -> bool {
        self.color_space.straight_rgb(&[]).is_some()
    }

    /// Whether colors are straight in the function's outputs, as the color
    /// space and no transfer function leave them.
    fn lines_straight(&self) -> bool {
        self.straight_space() && self.transfer.is_none()
    }

    /// The color, after the opacity and transfer function, of a color in the
    /// color space; transparent black of none.
    fn finish(&self, color: Option<AlphaColor>) -> [f32; 4] {
        finished(color, self.opacity, self.transfer)
    }

    /// The color of the function's `values`, transparent black of none. Of a
    /// color space whose colors are straight, where colors are straight
    /// ([`lines_straight`](Self::lines_straight)), as they are before they are
    /// rounded to 8 bits and clamped as the color space clamps them.
    fn color(&self, values: Option<&Values>) -> [f32; 4] {
        let Some(values) = values else {
            return self.finish(None);
        };
        if self.lines_straight()
            && let Some(rgb) = self.color_space.straight_rgb(values)
        {
            return self.finish(Some(AlphaColor::new(clamped(rgb))));
        }
        self.finish(Some(self.color_space.to_rgba(values, 1.0)))
    }

    /// How far the color of the function's `values` moves where hayro's color
    /// tables read one of them as the next 8-bit value, in 8-bit levels: a
    /// color space other than gray and RGB reads each to 8 bits, so its colors
    /// step from one to the next, and straight lines between stops cut across
    /// the steps.
    pub(crate) fn table_step(&self, values: &Values) -> f32 {
        let color = |v: &[f32]| self.finish(Some(self.color_space.to_rgba(v, 1.0)));
        let at = color(values);
        let mut most = 0.0_f32;
        for i in 0..values.len() {
            for d in [-1.0 / 255.0, 1.0 / 255.0] {
                let mut moved = values.clone();
                moved[i] = (moved[i] + d).clamp(0.0, 1.0);
                most = most.max(levels_apart(&color(&moved), &at));
            }
        }
        most
    }
}

/// `color` after `opacity` and `transfer`, transparent black of none: what a
/// shading pattern does to the colors of its shading.
pub(crate) fn finished(
    color: Option<AlphaColor>,
    opacity: f32,
    transfer: Option<&ActiveTransferFunction>,
) -> [f32; 4] {
    color
        .map(|v| {
            let mut components = v.components();
            components[3] *= opacity;
            if let Some(tf) = transfer {
                return tf.apply(&AlphaColor::new(components)).components();
            }
            components
        })
        .unwrap_or([0.0, 0.0, 0.0, 0.0])
}

/// The most colors a [`StopMemo`] keeps: those of a few thousand pixels' steps
/// for each of a few shapes of a function, a few megabytes.
const MEMO_ENTRIES: usize = 1 << 16;

/// The memory a color kept in a [`StopMemo`] holds, its key and its table's
/// share besides.
const MEMO_ENTRY_BYTES: usize = 2 * size_of::<(ColorKey, [f32; 4])>();

/// A color's place in a [`StopMemo`]: the [`Look`] it is found in, the bits of
/// the function's input, and its [`Side`].
type ColorKey = (LookKey, u32, u8);

/// Where a color of a function at an input comes from: its value evaluated
/// there, the only point of its shape there, or of several there the value
/// from below the input, at it, or from above it.
#[derive(Clone, Copy, Debug)]
enum Side {
    Evaluated = 0,
    Point = 1,
    Below = 2,
    At = 3,
    Above = 4,
}

/// A point of a function's lines as a stop: its input, its values, and which
/// of the values at that input it is.
type StopPoint = (f32, Values, Side);

/// A piece of a function's shape ([`Piece`]) as a gradient's stops hold it,
/// found once for every paint of a shading.
#[derive(Debug)]
enum StopPiece {
    /// Its points, but for the value at an input between the values from
    /// below and above it, which a gradient's stops cannot hold, at the
    /// shape's ends alone, where the gradient extends it.
    Lines(Vec<StopPoint>),
    /// From one input to another, of no shape known here.
    Curve(f32, f32),
}

impl StopPiece {
    /// The pieces of `shape` as stops hold them.
    fn of(shape: &[Piece]) -> Vec<Self> {
        let (from, to) = (
            shape.first().map_or(f32::NAN, |p| p.span().0),
            shape.last().map_or(f32::NAN, |p| p.span().1),
        );
        shape
            .iter()
            .map(|piece| match piece {
                Piece::Lines(points) => Self::Lines(
                    points
                        .iter()
                        .enumerate()
                        .filter_map(|(i, (x, values))| {
                            let side = match shape::side_of(points, i) {
                                None => Side::Point,
                                Some(shape::Side::Below) => Side::Below,
                                Some(shape::Side::At) => Side::At,
                                Some(shape::Side::Above) => Side::Above,
                            };
                            let end = *x == from || *x == to;
                            (!matches!(side, Side::At) || end).then(|| (*x, values.clone(), side))
                        })
                        .collect(),
                ),
                Piece::Curve(a, b) => Self::Curve(*a, *b),
            })
            .collect()
    }

    /// Where the piece starts and ends.
    fn span(&self) -> (f32, f32) {
        match self {
            Self::Lines(points) => (
                points.first().map_or(f32::NAN, |p| p.0),
                points.last().map_or(f32::NAN, |p| p.0),
            ),
            Self::Curve(a, b) => (*a, *b),
        }
    }
}

/// The most two colors' channels lie apart, in 8-bit levels.
pub(crate) fn levels_apart(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(a, b)| (a - b).abs() * 255.0)
        .fold(0.0, f32::max)
}

/// A straight RGB color, opaque, clamped as the color space clamps it.
fn clamped(rgb: [f32; 3]) -> [f32; 4] {
    let clamp = |v: f32| if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
    [clamp(rgb[0]), clamp(rgb[1]), clamp(rgb[2]), 1.0]
}

/// The nodes of lines whose colors are straight between `points`, each color
/// clamped as the color space clamps it: the points, and a point wherever a
/// channel crosses 0 or 1 between two.
fn straight_lines(
    look: &Look<'_>,
    points: &[StopPoint],
    t_of: &impl Fn(f32) -> f32,
    nodes: &mut Vec<(f32, f32, [f32; 4])>,
) {
    let rgb_of = |values: &[f32]| look.color_space.straight_rgb(values).unwrap_or([0.0; 3]);
    let opaque = |rgb: [f32; 3]| look.finish(Some(AlphaColor::new(clamped(rgb))));
    for (i, (x, values, _)) in points.iter().enumerate() {
        let rgb = rgb_of(values);
        if let Some((x0, v0, _)) = i.checked_sub(1).map(|i| &points[i])
            && x > x0
        {
            let before = rgb_of(v0);
            let mut crossings: smallvec::SmallVec<[f32; 6]> = smallvec::SmallVec::new();
            for (p, q) in before.iter().zip(&rgb) {
                for side in [0.0, 1.0] {
                    if (p - side) * (q - side) < 0.0 {
                        crossings.push((side - p) / (q - p));
                    }
                }
            }
            crossings.sort_by(f32::total_cmp);
            for s in crossings {
                let at = x0 + (x - x0) * s;
                let rgb = [0, 1, 2].map(|c| before[c] + (rgb[c] - before[c]) * s);
                nodes.push((at, t_of(at), opaque(rgb)));
            }
        }
        nodes.push((*x, t_of(*x), opaque(rgb)));
    }
}

/// Adds to `nodes` the color at each multiple of `step` from `k0` to `k1` whose
/// input lies strictly within `span`.
fn grid(
    nodes: &mut Vec<(f32, f32, [f32; 4])>,
    (k0, k1): (i64, i64),
    step: f32,
    (a, b): (f32, f32),
    x_at: &impl Fn(f32) -> f32,
    color_at: &mut impl FnMut(f32) -> Result<[f32; 4], Exhausted>,
) -> Result<(), Exhausted> {
    for k in k0..=k1 {
        let t = (k as f64 * f64::from(step)) as f32;
        let x = x_at(t);
        if x > a && x < b {
            nodes.push((x, t, color_at(x)?));
        }
    }
    Ok(())
}

/// Splits at its middle each interval between `nodes`, in the order of their
/// inputs, next to a node off the line through its neighbours by more than
/// `bend` levels, until none is or each such interval is no wider than
/// `finest`: where the colors bend within a step of the grid, as a color space
/// bends straight components or a function steps, the stops follow them, so
/// straight lines between stops stray by no more than a fourth of that from
/// them, but for features narrower than `finest`. Each stop added is taken of
/// `meter` first, and `color_at` takes what finding its color does.
fn refine(
    nodes: &mut Vec<(f32, f32, [f32; 4])>,
    (finest, bend): (f32, f32),
    meter: &mut dyn Meter,
    t_of: &impl Fn(f32) -> f32,
    color_at: &mut impl FnMut(&mut dyn Meter, f32) -> Result<[f32; 4], Exhausted>,
) -> Result<(), Exhausted> {
    loop {
        let n = nodes.len();
        let mut split = vec![false; n.saturating_sub(1)];
        for k in 1..n.saturating_sub(1) {
            let (a, m, b) = (&nodes[k - 1], &nodes[k], &nodes[k + 1]);
            if !(a.0 < m.0 && m.0 < b.0) {
                continue;
            }
            let s = (m.0 - a.0) / (b.0 - a.0);
            let bends = (0..4).any(|c| {
                let line = a.2[c] + (b.2[c] - a.2[c]) * s;
                (m.2[c] - line).abs() * 255.0 > bend
            });
            if bends {
                split[k - 1] = true;
                split[k] = true;
            }
        }
        let mut middles = 0_usize;
        for (i, split) in split.iter_mut().enumerate() {
            let (a, b) = (nodes[i].0, nodes[i + 1].0);
            let middle = a + (b - a) / 2.0;
            *split &= b - a > finest && a < middle && middle < b;
            middles += usize::from(*split);
        }
        if middles == 0 {
            return Ok(());
        }
        take(meter, Cost::new(4).times(middles as u64))?;
        if !meter.hold(middles.saturating_mul(size_of::<SvgGradientStop>())) {
            return Err(Exhausted);
        }
        let mut out = Vec::with_capacity(n + middles);
        for (i, node) in nodes.iter().enumerate() {
            out.push(*node);
            if split.get(i).copied().unwrap_or(false) {
                let (a, b) = (node.0, nodes[i + 1].0);
                let x = a + (b - a) / 2.0;
                out.push((x, t_of(x), color_at(meter, x)?));
            }
        }
        *nodes = out;
    }
}

/// The points of lines whose colors bend, as a color space bends them, that are
/// stops besides the grid's `grid`: all of them where they are no more than the
/// grid's, so a feature as wide as a pixel's step keeps its points; where they
/// are more, the ends and the two sides of each step, as the grid then holds a
/// stop wherever two points lie a pixel's step apart and the rest are features
/// narrower than a pixel, and evaluating the color at each of thousands of
/// samples would cost what the pixels shown do not need.
fn kept(points: &[StopPoint], grid: u64) -> impl Iterator<Item = usize> {
    let all = points.len() as u64 <= grid.saturating_add(2);
    let last = points.len().saturating_sub(1);
    points.iter().enumerate().filter_map(move |(i, point)| {
        let step = |j: Option<usize>| {
            j.and_then(|j| points.get(j))
                .is_some_and(|p| p.0 == point.0)
        };
        (all || i == 0 || i == last || step(i.checked_sub(1)) || step(Some(i + 1))).then_some(i)
    })
}

/// The points of `points` that a line through them over `lo` to `hi` needs,
/// from the last at or before `lo`, or every one at the first input where
/// `lo` is at or before it, to every one at the first input at or past `hi`,
/// as the values there from below, at it, and from above.
fn within(points: &[StopPoint], lo: f32, hi: f32) -> &[StopPoint] {
    let last = points.len() - 1;
    let i0 = if lo <= points[0].0 {
        0
    } else {
        points.partition_point(|p| p.0 <= lo) - 1
    };
    let j = points.partition_point(|p| p.0 < hi).min(last);
    let i1 = points.partition_point(|p| p.0 <= points[j].0) - 1;
    &points[i0..=i1.max(i0)]
}

/// The color `stops` give at `t`, from the start, 0, to the end, 1: between the
/// last stop at or before it and the next, as a gradient's stops are drawn; at a
/// bound where the function steps, two stops share it and the later one's color
/// starts there, as the function's does; at or before the first stop, its
/// color, as at its start the function's value there is the first of the
/// stops there, before any from above it.
pub(crate) fn color_at(stops: &[SvgGradientStop], t: f32) -> [f32; 4] {
    match stops.first() {
        Some(first) if t <= first.offset => first.color,
        _ => lerp_between(stops, stops.partition_point(|stop| stop.offset <= t), t),
    }
}

/// The color at `t` of the line between the stop before `next` and `next`.
fn lerp_between(stops: &[SvgGradientStop], next: usize, t: f32) -> [f32; 4] {
    match (
        next.checked_sub(1).map(|i| stops[i]),
        stops.get(next).copied(),
    ) {
        (Some(before), Some(after)) if after.offset > before.offset => lerp_color(
            before.color,
            after.color,
            (t - before.offset) / (after.offset - before.offset),
        ),
        (Some(stop), _) | (None, Some(stop)) => stop.color,
        (None, None) => [0.0; 4],
    }
}

fn lerp_color(c0: [f32; 4], c1: [f32; 4], t: f32) -> [f32; 4] {
    [
        c0[0] + (c1[0] - c0[0]) * t,
        c0[1] + (c1[1] - c0[1]) * t,
        c0[2] + (c1[2] - c0[2]) * t,
        c0[3] + (c1[3] - c0[3]) * t,
    ]
}
