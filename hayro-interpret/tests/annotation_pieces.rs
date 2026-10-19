//! The pieces of repeated shapes (the dividers of a comb field, the steps of a
//! squiggle) that a generator draws and counts are exactly those that paint, and
//! what cannot paint is never what refuses a page.
//!
//! The reference here is built from what the content means (PDF 32000-1 8.5), not
//! from how the generators search: a piece paints where its outline, as kurbo's
//! stroker makes it at the width written, with butt caps and joins mitred up to
//! 10, meets the part of the page shown that every clip in force leaves, each clip
//! as written (every number rounded to 32 bits, as viewers read content), grown on
//! the page by how far a stroke paints beyond its outline (a quarter of a point
//! across and up, in the page's units: `/UserUnit` points each). A clip that
//! leaves that part no area (a line or a point) leaves nothing, before growing.
//! The steps of a squiggle are those of its zigzag as written: a step whose two
//! corners are one point as written paints nothing of its own, and the steps
//! either side of it meet in a join there. An annotation with more such pieces
//! than the limit is refused, and only then, or where a piece that paints has a
//! number that cannot be written (2^23 or more); what paints nothing never refuses,
//! however far from the page. (A squiggle finer than the 32-bit numbers it is
//! written in, along the edge of what shows, refuses for the steps the generator
//! looks at one by one too: `annotation_cost.rs` holds those; the shapes here are
//! coarser.) The reference takes every piece of shapes of a few thousand, one by
//! one; the generators find them among billions.

use hayro_interpret::annotation::{Appearances, Look, Unsupported, set_most_pieces};
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{AnnotationSelection, InterpreterSettings};
use kurbo::{Affine, BezPath, Cap, Join, PathEl, Point, Rect, Shape, Stroke, StrokeOpts, Vec2};

mod common;

/// How far beyond its outline a stroke paints, in points.
const BEYOND: f64 = 0.25;

/// The limit the random shapes are checked at: their pieces that paint number
/// from none to several times it.
const LIMIT: u32 = 40;

/// The least magnitude that cannot be written in content.
const UNWRITABLE: f64 = 8_388_608.0;

/// The content generated for `annot` on a page that shows `visible`, in units of
/// `unit` points; empty when it draws nothing.
fn look(annot: &str, visible: Rect, unit: f64) -> Result<String, Unsupported> {
    let pdf = Pdf::new(common::one_page(
        200,
        "/Annots [4 0 R]",
        vec![format!("<< /Type /Annot {annot} >>").into_bytes()],
    ))
    .expect("valid pdf");
    let appearances = Appearances::new(pdf.xref(), InterpreterSettings::default().font_resolver);
    let drawn = appearances.page(&pdf.pages()[0], AnnotationSelection::View, visible, unit);
    let look = drawn
        .iter()
        .find(|d| d.index() == 0)
        .map(|d| d.look().clone());
    match look {
        None | Some(Look::Nothing) => Ok(String::new()),
        Some(Look::Generated(a)) => Ok(a.content.clone()),
        Some(Look::Unsupported(why)) => Err(why),
        Some(Look::Stream { .. }) => panic!("{annot}: drawn by a stream"),
    }
}

/// A small linear congruential generator: the same shapes each run.
struct Random(u64);

impl Random {
    fn next(&mut self, lo: f64, hi: f64) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        lo + (hi - lo) * ((self.0 >> 11) as f64 / (1_u64 << 53) as f64)
    }

    fn pick<T: Copy>(&mut self, of: &[T]) -> T {
        of[(self.next(0.0, of.len() as f64) as usize).min(of.len() - 1)]
    }

    /// A number far from the page, either side: 10^6 to 10^7.
    fn far(&mut self) -> f64 {
        self.pick(&[-1.0, 1.0]) * self.next(1e6, 1e7)
    }
}

fn cross(a: Vec2, b: Vec2) -> f64 {
    a.x * b.y - a.y * b.x
}

fn rect_corners(r: Rect) -> Vec<Point> {
    vec![
        Point::new(r.x0, r.y0),
        Point::new(r.x1, r.y0),
        Point::new(r.x1, r.y1),
        Point::new(r.x0, r.y1),
    ]
}

fn area(p: &[Point]) -> f64 {
    (0..p.len())
        .map(|i| cross(p[i].to_vec2(), p[(i + 1) % p.len()].to_vec2()))
        .sum::<f64>()
        / 2.0
}

/// The part of the convex polygon `poly` inside the convex polygon `by`; empty
/// where `by` has no area.
fn cut(poly: Vec<Point>, by: &[Point]) -> Vec<Point> {
    let n = by.len();
    let sign = area(by).signum();
    if area(by) == 0.0 {
        return vec![];
    }
    let mut poly = poly;
    for i in 0..n {
        if poly.is_empty() {
            break;
        }
        let (a, b) = (by[i], by[(i + 1) % n]);
        let side = |p: Point| cross(b - a, p - a) * sign;
        let mut kept = vec![];
        for (j, &p) in poly.iter().enumerate() {
            let q = poly[(j + 1) % poly.len()];
            let (sp, sq) = (side(p), side(q));
            if sp >= 0.0 {
                kept.push(p);
            }
            if (sp >= 0.0) != (sq >= 0.0) {
                kept.push(p.lerp(q, sp / (sp - sq)));
            }
        }
        poly = kept;
    }
    poly
}

/// The convex polygon `region` grown by `b` across and up: the hull of its corners
/// moved to each corner of that square.
fn grown(region: &[Point], b: f64) -> Vec<Point> {
    hull(
        region
            .iter()
            .flat_map(|p| [(-b, -b), (b, -b), (b, b), (-b, b)].map(|(x, y)| *p + Vec2::new(x, y)))
            .collect(),
    )
}

/// The convex hull of `points`, counterclockwise.
fn hull(mut points: Vec<Point>) -> Vec<Point> {
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
                && cross(
                    hull[hull.len() - 1] - hull[hull.len() - 2],
                    p - hull[hull.len() - 2],
                ) <= 0.0
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

/// Whether the segment `a`–`b` meets the convex polygon `g`: no side of either
/// parts them.
fn segment_meets(a: Point, b: Point, g: &[Point]) -> bool {
    let n = g.len();
    let axes = (0..n)
        .map(|i| (g[(i + 1) % n] - g[i]).turn_90())
        .chain([(b - a).turn_90()]);
    for axis in axes {
        let project = |p: Point| axis.dot(p.to_vec2());
        let (g0, g1) = g
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
                (lo.min(project(*p)), hi.max(project(*p)))
            });
        let (s0, s1) = (project(a).min(project(b)), project(a).max(project(b)));
        if s1 < g0 || s0 > g1 {
            return false;
        }
    }
    true
}

/// Whether the convex polygons `a` and `g` meet: no side of either parts them.
fn convex_meets(a: &[Point], g: &[Point]) -> bool {
    if a.is_empty() || g.is_empty() {
        return false;
    }
    let sides = |p: &[Point]| {
        (0..p.len())
            .map(|i| (p[(i + 1) % p.len()] - p[i]).turn_90())
            .collect::<Vec<_>>()
    };
    sides(a).into_iter().chain(sides(g)).all(|axis| {
        let range = |p: &[Point]| {
            p.iter()
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), q| {
                    let v = axis.dot(q.to_vec2());
                    (lo.min(v), hi.max(v))
                })
        };
        let ((a0, a1), (g0, g1)) = (range(a), range(g));
        a1 >= g0 && a0 <= g1
    })
}

/// The join kurbo's stroker makes at `c` between the steps from `a` and to `b`,
/// `width` wide: of the outline of a stub of each step either side of `c`, the
/// part on the outer side of the turn, which neither step's own stroke covers
/// (the stubs' butt ends lie on the steps' own ends). Empty where the steps go
/// straight on.
fn join(a: Point, c: Point, b: Point, width: f64) -> Vec<Point> {
    let (u1, u2) = ((c - a).normalize(), (b - c).normalize());
    let outer = u1 - u2;
    if outer.hypot() <= 1e-12 {
        return vec![];
    }
    let stub = 1e-9 * (1.0 + c.x.abs().max(c.y.abs()));
    let mut path = BezPath::new();
    path.move_to(c - u1 * stub);
    path.line_to(c);
    path.line_to(c + u2 * stub);
    let mut points = vec![c];
    kurbo::flatten(stroke(path, width), 1e-9, |el| {
        if let PathEl::MoveTo(p) | PathEl::LineTo(p) = el
            && (p - c).dot(outer) > 1e-6 * width * outer.hypot()
        {
            points.push(p);
        }
    });
    hull(points)
}

/// Whether the region `outline` fills (nonzero) meets the convex polygon `g`.
fn meets(outline: &BezPath, g: &[Point]) -> bool {
    if g.is_empty() {
        return false;
    }
    if g.iter().any(|p| outline.winding(*p) != 0) {
        return true;
    }
    let (mut last, mut start, mut found) = (Point::ZERO, Point::ZERO, false);
    kurbo::flatten(outline, 1e-9, |el| {
        let (a, b) = match el {
            PathEl::MoveTo(q) => {
                (last, start) = (q, q);
                return;
            }
            PathEl::LineTo(q) => (last, q),
            PathEl::ClosePath => (last, start),
            _ => unreachable!("flattened"),
        };
        found |= segment_meets(a, b, g);
        last = b;
    });
    found
}

fn stroke(path: BezPath, width: f64) -> BezPath {
    let style = Stroke::new(width)
        .with_join(Join::Miter)
        .with_miter_limit(10.0)
        .with_caps(Cap::Butt);
    kurbo::stroke(path, &style, &StrokeOpts::default(), 1e-9)
}

/// A number as written in content, rounded to 32 bits as viewers read it.
fn f32ed(v: f64) -> f64 {
    f64::from(v as f32)
}

/// Whether `p` cannot be written in content.
fn unwritable(p: Point) -> bool {
    !(f32ed(p.x).abs() < UNWRITABLE && f32ed(p.y).abs() < UNWRITABLE)
}

/// The numbers and operator of each line of content, each number read as viewers
/// read it, in 32 bits.
fn operations(content: &str) -> Vec<(Vec<f64>, &str)> {
    content
        .lines()
        .map(|l| {
            let words: Vec<&str> = l.split(' ').collect();
            let (op, nums) = words.split_last().expect("an operator");
            let nums = nums.iter().filter_map(|n| n.parse::<f32>().ok());
            (nums.map(f64::from).collect(), *op)
        })
        .collect()
}

/// A clip: the rectangle `x y w h` under a transform.
type Clip = (Affine, [f64; 4]);

/// The clips in force where the content first starts a path with `m`, and the
/// transform there: as written; `None` where it starts none.
fn clips_written(content: &str) -> Option<(Affine, Vec<Clip>)> {
    let mut ctm = Affine::IDENTITY;
    let mut clips = vec![];
    let mut saved = vec![];
    let mut rect = None;
    for (n, op) in operations(content) {
        match op {
            "q" => saved.push((ctm, clips.len())),
            "Q" => {
                let (c, k) = saved.pop().expect("q before Q");
                ctm = c;
                clips.truncate(k);
            }
            "cm" => ctm *= Affine::new([n[0], n[1], n[2], n[3], n[4], n[5]]),
            "re" => rect = Some([n[0], n[1], n[2], n[3]]),
            "n" => clips.push((ctm, rect.take().expect("a clip's rectangle"))),
            "m" => return Some((ctm, clips)),
            _ => {}
        }
    }
    None
}

/// What the page shows of `visible` under `clips` (each a rectangle `x y w h`, as
/// written, under a transform onto the page), grown by `beyond`: empty where the
/// clips leave it no area.
fn shown(visible: Rect, clips: &[Clip], beyond: f64) -> (Vec<Point>, Vec<Point>) {
    let mut region = rect_corners(visible);
    for (ctm, [x, y, w, h]) in clips {
        let corners = [(*x, *y), (x + w, *y), (x + w, y + h), (*x, y + h)]
            .map(|(x, y)| *ctm * Point::new(x, y));
        region = cut(region, &corners);
    }
    if area(&region).abs() > 0.0 {
        let g = grown(&region, beyond);
        (region, g)
    } else {
        (vec![], vec![])
    }
}

/// Whether two parts of the page shown, one from the clips as written and one from
/// the clips the generator is to write, are the same but for rounding.
fn same_shown(a: &[Point], b: &[Point]) -> bool {
    if a.is_empty() || b.is_empty() {
        return a.is_empty() == b.is_empty();
    }
    let bounds = |p: &[Point]| {
        p.iter()
            .fold(Rect::from_points(p[0], p[0]), |r, q| r.union_pt(*q))
    };
    let (a, b) = (bounds(a), bounds(b));
    let close = |u: f64, v: f64| (u - v).abs() <= 1e-5 * (1.0 + u.abs().max(v.abs()));
    close(a.x0, b.x0) && close(a.y0, b.y0) && close(a.x1, b.x1) && close(a.y1, b.y1)
}

/// What a check found: the pieces that paint, and whether the shape was refused.
struct Checked {
    painting: usize,
    refused: bool,
}

/// Checks a comb field against the reference: `rect`, turned `turn` degrees,
/// `max_len` cells, a border `w` wide.
fn check_comb(
    rect: [f64; 4],
    turn: i32,
    max_len: u32,
    w: f64,
    visible: Rect,
    unit: f64,
    limit: u32,
) -> Checked {
    let [x0, y0, x1, y1] = rect.map(f32ed);
    let annot = format!(
        "/Subtype /Widget /FT /Tx /Ff 16777216 /MK << /BC [0] /R {turn} >> /MaxLen {max_len} \
         /BS << /W {w} >> /Rect [{x0} {y0} {x1} {y1}]"
    );
    let w = f32ed(w);
    let r = Rect::new(x0, y0, x1, y1).abs();
    // The box, turned onto the rectangle as PDF 32000-1 12.5.5 places an
    // appearance: its turned box's bounds scaled onto /Rect.
    // Swapped for 90 and 270, but not -90, which is turned as 270 (observed).
    let (fw, fh) = if matches!(turn % 360, 90 | 270) {
        (r.height(), r.width())
    } else {
        (r.width(), r.height())
    };
    // The box is written in 32 bits where it places the appearance and lays out
    // the cells, and clipped to as it is.
    let (box_w, box_h) = (fw, fh);
    let (fw, fh) = (f32ed(fw), f32ed(fh));
    let rotation = match turn.rem_euclid(360) {
        0 => Affine::IDENTITY,
        90 => Affine::new([0.0, 1.0, -1.0, 0.0, 0.0, 0.0]),
        180 => Affine::new([-1.0, 0.0, 0.0, -1.0, 0.0, 0.0]),
        270 => Affine::new([0.0, -1.0, 1.0, 0.0, 0.0, 0.0]),
        _ => Affine::rotate(f64::from(turn).to_radians()),
    };
    let bounds = (rotation * Rect::new(0.0, 0.0, fw, fh).to_path(0.1)).bounding_box();
    let place = Affine::translate((r.x0, r.y0))
        * Affine::scale_non_uniform(r.width() / bounds.width(), r.height() / bounds.height())
        * Affine::translate((-bounds.x0, -bounds.y0))
        * rotation;
    // As written: each coefficient rounded.
    let place = Affine::new(place.as_coeffs().map(f32ed));
    // Drawn under the box's clip and the clip to the box inside the border, each
    // `x y w h re` as written.
    let clips = [
        Rect::new(0.0, 0.0, box_w, box_h),
        Rect::new(w, w, box_w - w, box_h - w).abs(),
    ]
    .map(|c| (place, edges(c)));
    let beyond = BEYOND / unit;
    let (spec, _) = shown(visible, &clips, beyond);
    set_most_pieces(limit);
    let got = look(&annot, visible, unit);
    // The clips as the content writes them, where it draws a line: what they show
    // is what those it is to write show.
    let written = got.as_ref().ok().and_then(|c| clips_written(c));
    let (place, (region, g)) = match &written {
        Some((ctm, written)) => {
            let (region, g) = shown(visible, written, beyond);
            assert!(same_shown(&region, &spec), "{annot}: clips {written:?}");
            (*ctm, (region, g))
        }
        None => (place, shown(visible, &clips, beyond)),
    };
    let cells = f64::from(max_len);
    let step = (fw - 2.0 * w) / cells;
    let (wy0, wy1) = (f32ed(w), f32ed(fh - w));
    // Of no length as written, a line paints nothing; under a clip of no area,
    // nothing paints.
    let paints: Vec<f64> = if max_len < 2 || wy0 == wy1 || region.is_empty() {
        vec![]
    } else {
        (1..max_len)
            .map(|i| f32ed(w + step * f64::from(i)))
            .filter(|&u| {
                let mut line = BezPath::new();
                line.move_to((u, wy0));
                line.line_to((u, wy1));
                meets(&(place * stroke(line, w)), &g)
            })
            .collect()
    };
    let refused = paints.len() > limit as usize
        || (!paints.is_empty()
            && (place.as_coeffs().iter().any(|c| c.abs() >= UNWRITABLE)
                || paints.iter().any(|u| u.abs() >= UNWRITABLE)));
    match got {
        Err(why) => {
            assert!(
                refused,
                "{annot} {visible:?} unit {unit}: {why:?}, {} paint",
                paints.len()
            );
            assert_eq!(why, Unsupported::Coordinates, "{annot}");
        }
        Ok(content) => {
            assert!(
                !refused,
                "{annot} {visible:?} unit {unit}: {} paint",
                paints.len()
            );
            let drawn: Vec<f64> = operations(&content)
                .into_iter()
                .filter(|(_, op)| *op == "m")
                .map(|(n, _)| n[0])
                .collect();
            assert_eq!(drawn, paints, "{annot} {visible:?} unit {unit}\n{content}");
        }
    }
    Checked {
        painting: paints.len(),
        refused,
    }
}

/// The zigzag of the quad `tl tr bl br` as written: its corners (steps of a
/// seventh of its height along its bottom edge, from its bottom left to the first
/// at or past its bottom right; the corners alternately on the bottom edge and on
/// the line a seventh of the way up), and its width. No corner for a quad of no
/// height or length.
fn zigzag(q: [Point; 4]) -> (Vec<Point>, f64) {
    let [tl, _, bl, br] = q;
    let tr = q[1];
    let height = (tl - bl).hypot();
    let len = (br - bl).hypot();
    if height == 0.0 || len == 0.0 {
        return (vec![], 0.0);
    }
    let delta = height / 7.0;
    let steps = (len / delta).ceil() as usize;
    let (top_left, top_right) = (bl.lerp(tl, 1.0 / 7.0), br.lerp(tr, 1.0 / 7.0));
    let corners = (0..=steps)
        .map(|k| {
            let t = k as f64 * delta / len;
            let p = if k % 2 == 0 {
                bl.lerp(br, t)
            } else {
                top_left.lerp(top_right, t)
            };
            Point::new(f32ed(p.x), f32ed(p.y))
        })
        .collect();
    (corners, f32ed(height / 16.0))
}

/// The clip of a squiggle of `quads`: the box of every quad, each grown by its
/// width (a sixteenth of its height), as `x y w h`; `None` where it has no area.
fn squiggle_clip(quads: &[[Point; 4]]) -> Option<[f64; 4]> {
    let boxes = quads.iter().map(|q| {
        let width = (q[0] - q[2]).hypot() / 16.0;
        Rect::from_points(q[0], q[1])
            .union_pt(q[2])
            .union_pt(q[3])
            .inset(width)
    });
    let clip = boxes.reduce(|a, b| a.union(b))?;
    (clip.width() > 0.0 && clip.height() > 0.0).then(|| edges(clip))
}

/// The rectangle `r` as a clip writes it, `x y w h`, each as a viewer reads it;
/// where one of those cannot be written, the clip is written cut to what the page
/// shows, which leaves each side that shows where a viewer reads that side.
fn edges(r: Rect) -> [f64; 4] {
    let written = [r.x0, r.y0, r.width(), r.height()].map(f32ed);
    if written.iter().all(|v| v.abs() < UNWRITABLE) {
        return written;
    }
    let [x0, y0, x1, y1] = [r.x0, r.y0, r.x1, r.y1].map(f32ed);
    [x0, y0, x1 - x0, y1 - y0]
}

/// The steps of the zigzag through `corners`, `width` wide, that paint in `g`:
/// those of some length as written whose own stroke meets it, or the join they
/// share with the nearest step of some length on either side.
fn steps_that_paint(corners: &[Point], width: f64, g: &[Point]) -> Vec<usize> {
    if g.is_empty() {
        return vec![];
    }
    let long: Vec<usize> = (0..corners.len().saturating_sub(1))
        .filter(|&k| corners[k] != corners[k + 1])
        .collect();
    let c = corners;
    (0..long.len())
        .filter(|&i| {
            let k = long[i];
            let mut line = BezPath::new();
            line.move_to(c[k]);
            line.line_to(c[k + 1]);
            meets(&stroke(line, width), g)
                || (i > 0 && convex_meets(&join(c[long[i - 1]], c[k], c[k + 1], width), g))
                || (i + 1 < long.len()
                    && convex_meets(&join(c[k], c[k + 1], c[long[i + 1] + 1], width), g))
        })
        .map(|i| long[i])
        .collect()
}

/// Checks a squiggle of the quads `quads` (each `tl tr bl br`) against the
/// reference; the limit holds for all their steps.
fn check_squiggle(quads: &[[Point; 4]], visible: Rect, unit: f64, limit: u32) -> Checked {
    let quads: Vec<[Point; 4]> = quads
        .iter()
        .map(|q| q.map(|p| Point::new(f32ed(p.x), f32ed(p.y))))
        .collect();
    let numbers: Vec<String> = quads
        .iter()
        .flatten()
        .map(|p| format!("{} {}", p.x, p.y))
        .collect();
    let annot = format!(
        "/Subtype /Squiggly /C [0] /Rect [0 0 1 1] /QuadPoints [{}]",
        numbers.join(" ")
    );
    let beyond = BEYOND / unit;
    // One clip for all quads.
    let clip = squiggle_clip(&quads);
    let spec = clip.map(|c| vec![(Affine::IDENTITY, c)]);
    set_most_pieces(limit);
    let got = look(&annot, visible, unit);
    let written = got.as_ref().ok().and_then(|c| clips_written(c));
    let g = match (&written, &spec) {
        (_, None) => vec![],
        (Some((_, written)), Some(spec)) => {
            let (region, g) = shown(visible, written, beyond);
            assert!(
                same_shown(&region, &shown(visible, spec, beyond).0),
                "{annot}: clip {written:?}"
            );
            g
        }
        (None, Some(spec)) => shown(visible, spec, beyond).1,
    };
    let mut all: Vec<(usize, usize)> = vec![];
    let mut zigzags = vec![];
    let mut refused = false;
    let mut painting = 0;
    for (n, q) in quads.iter().enumerate() {
        let (corners, width) = zigzag(*q);
        let paints = steps_that_paint(&corners, width, &g);
        refused |= paints
            .iter()
            .any(|&k| unwritable(corners[k]) || unwritable(corners[k + 1]));
        painting += paints.len();
        all.extend(paints.into_iter().map(|k| (n, k)));
        zigzags.push(corners);
    }
    // The limit holds for the annotation's steps in all.
    refused |= painting > limit as usize;
    match got {
        Err(why) => {
            assert!(
                refused,
                "{annot} {visible:?} unit {unit}: {why:?}, {all:?} paint"
            );
            assert_eq!(why, Unsupported::Coordinates, "{annot}");
        }
        Ok(content) => {
            assert!(!refused, "{annot} {visible:?} unit {unit}: {all:?} paint");
            assert_eq!(
                drawn_steps(&content, &zigzags),
                all,
                "{annot} {visible:?} unit {unit}\n{content}"
            );
        }
    }
    Checked { painting, refused }
}

/// The steps `content` draws, as the quad and step of each, found by walking the
/// zigzags in order: a run starts at a corner (`m`), and each `l` draws the next
/// step of some length, past steps of none.
fn drawn_steps(content: &str, zigzags: &[Vec<Point>]) -> Vec<(usize, usize)> {
    let mut drawn = vec![];
    // The quad and the corner the last drawn step ends at.
    let (mut n, mut at) = (0, 0);
    for (nums, op) in operations(content) {
        if !matches!(op, "m" | "l") || nums.len() != 2 {
            continue;
        }
        let p = Point::new(nums[0], nums[1]);
        if op == "m" {
            // The next corner there, in this quad or a later one.
            let found = (n..zigzags.len()).find_map(|m| {
                let from = if m == n { at } else { 0 };
                (from..zigzags[m].len())
                    .find(|&k| zigzags[m][k] == p)
                    .map(|k| (m, k))
            });
            (n, at) = found.unwrap_or_else(|| panic!("a run starts off the zigzags at {p:?}"));
        } else {
            let z = &zigzags[n];
            let mut k = at;
            while k + 1 < z.len() && z[k + 1] == z[k] {
                k += 1;
            }
            assert!(
                k + 1 < z.len() && z[k + 1] == p,
                "a step to {p:?} drawn after {k}"
            );
            drawn.push((n, k));
            at = k + 1;
        }
    }
    drawn
}

/// Where the shapes are placed: the area shown, or the page, its parts or more.
fn visible(rng: &mut Random) -> Rect {
    match rng.pick(&[0, 0, 1, 2]) {
        0 => Rect::new(0.0, 0.0, 200.0, 200.0),
        1 => Rect::new(
            rng.next(0.0, 120.0),
            rng.next(0.0, 120.0),
            rng.next(130.0, 200.0),
            rng.next(130.0, 200.0),
        ),
        _ => Rect::new(-50.0, -50.0, 250.0, 250.0),
    }
}

/// A point on the page, beside it, above or below it, or around its edges.
fn place(rng: &mut Random) -> (f64, f64) {
    let near = [
        -60.0, -5.0, -1.0, -0.3, 0.0, 0.2, 100.0, 199.5, 200.3, 201.0, 260.0,
    ];
    let x = rng.pick(&near) + rng.next(-1.0, 1.0);
    let y = rng.pick(&near) + rng.next(-1.0, 1.0);
    (x, y)
}

/// Random comb fields, turned by right angles and others, with thin and thick
/// borders, borders of half the field's width or height (whose clip inside the
/// border has no area), on, beside, above and below the page and partly on it, far
/// from it, in units of a point and of other sizes: those that paint more pieces
/// than the limit are refused, and the others draw exactly those that paint.
#[test]
fn combs_draw_and_count_exactly_the_dividers_that_paint() {
    let mut rng = Random(0x5eed_c0b5);
    let (mut refused, mut drawn, mut none, mut flat) = (0, 0, 0, 0);
    for _ in 0..500 {
        let (mut x, mut y) = place(&mut rng);
        if rng.next(0.0, 1.0) < 0.1 {
            // Far from the page, wholly.
            if rng.next(0.0, 1.0) < 0.5 {
                x = rng.far();
            } else {
                y = rng.far();
            }
        }
        let (fw, fh) = (
            f32ed(rng.pick(&[5.0, 20.0, 80.0, 160.0]) * rng.next(0.8, 1.2)),
            f32ed(rng.pick(&[0.5, 4.0, 20.0, 40.0]) * rng.next(0.8, 1.2)),
        );
        let turn: i32 = rng.pick(&[0, 0, 90, 180, 270, -90, 45, 30, 135, 200, 13]);
        let swapped = matches!(turn % 360, 90 | 270);
        // The box's width and height, which the field's own are swapped into.
        let (bw, bh) = if swapped { (fh, fw) } else { (fw, fh) };
        let w = match rng.pick(&[0, 1, 2, 3, 4, 5, 6]) {
            0 => 0.01,
            1 => 0.3,
            2 => 1.0,
            3 => fh * rng.next(0.3, 0.6),
            4 => fh * rng.next(1.0, 2.0),
            // The clip inside the border of no width or height.
            5 => bw / 2.0,
            _ => bh / 2.0,
        };
        flat += usize::from(w == bw / 2.0 || w == bh / 2.0);
        let max_len = rng.pick(&[2, 3, 30, 60, 200, 300]);
        let unit = rng.pick(&[1.0, 1.0, 1.0, 0.5, 100.0, 1000.0]);
        let v = visible(&mut rng);
        let rect = [x - fw / 2.0, y - fh / 2.0, x + fw / 2.0, y + fh / 2.0];
        let c = check_comb(rect, turn, max_len, w, v, unit, LIMIT);
        match (c.refused, c.painting) {
            (true, _) => refused += 1,
            (false, 0) => none += 1,
            _ => drawn += 1,
        }
    }
    // Each outcome is reached.
    assert!(
        refused > 20 && drawn > 50 && none > 50 && flat > 30,
        "refused {refused}, drawn {drawn}, none {none}, flat {flat}"
    );
}

/// A random squiggle quad near the page: flat and steep, slanted, with sides apart
/// from parallel, of no height.
fn near_quad(rng: &mut Random) -> [Point; 4] {
    let (x, y) = place(rng);
    let h = rng.pick(&[0.0, 0.05, 1.0, 3.0, 7.0, 14.0]) * rng.next(0.8, 1.2);
    let len = rng.pick(&[1.0, 20.0, 60.0, 150.0]) * rng.next(0.8, 1.2);
    let angle = rng.pick(&[0.0, 0.0, 90.0, 180.0, 30.0, -70.0]) + rng.next(-3.0, 3.0);
    let along = Vec2::from_angle(f64::to_radians(angle)) * len;
    let up = along.turn_90().normalize();
    // Steep: the top leaning over by many heights.
    let lean = along.normalize() * rng.pick(&[0.0, 0.0, 1.0, 40.0]) * h;
    let spread = up * h * rng.pick(&[0.0, 0.0, 0.1, -0.3]);
    let bl = Point::new(x, y);
    let br = bl + along;
    [bl + up * h + lean, br + up * h + lean + spread, bl, br]
}

/// A second quad of a squiggle: one far from the page (10^6 to 10^7 away); one
/// just off the page, which widens the clip the quads share over it; or one from
/// far off to the page, of a height that makes its steps a few thousand.
fn other_quad(rng: &mut Random) -> [Point; 4] {
    let p = Point::new;
    match rng.pick(&[0, 1, 2]) {
        0 => {
            let (x, y) = if rng.next(0.0, 1.0) < 0.5 {
                (rng.far(), rng.next(-50.0, 250.0))
            } else {
                (rng.next(-50.0, 250.0), rng.far())
            };
            let (len, h) = (rng.next(1.0, 150.0), rng.next(0.5, 14.0));
            [p(x, y + h), p(x + len, y + h), p(x, y), p(x + len, y)]
        }
        1 => {
            // Below or left of the page, its clip reaching onto it.
            let h = rng.next(0.3, 7.0);
            let gap = rng.next(0.05, 1.5);
            let (x0, x1) = (rng.next(-20.0, 50.0), rng.next(100.0, 220.0));
            let q = [p(x0, -gap), p(x1, -gap), p(x0, -gap - h), p(x1, -gap - h)];
            if rng.next(0.0, 1.0) < 0.5 {
                q
            } else {
                q.map(|c| p(c.y, c.x))
            }
        }
        _ => {
            let start = rng.far();
            let end = rng.next(-20.0, 220.0);
            let len = (end - start).abs();
            let h = len * 7.0 / rng.next(500.0, 3000.0);
            let y = rng.next(-h, 200.0);
            [p(start, y + h), p(end, y + h), p(start, y), p(end, y)]
        }
    }
}

/// Random squiggles of one quad near the page, alone or with a quad far from it,
/// just off it, or reaching from far off to it, in units of a point and of other
/// sizes: those that paint more steps than the limit are refused, and the others
/// draw exactly those that paint, under the clip the quads share.
#[test]
fn squiggles_draw_and_count_exactly_the_steps_that_paint() {
    let mut rng = Random(0x5eed_5a16);
    let (mut refused, mut drawn, mut none, mut pairs) = (0, 0, 0, 0);
    for _ in 0..500 {
        let mut quads = vec![near_quad(&mut rng)];
        if rng.next(0.0, 1.0) < 0.5 {
            let other = other_quad(&mut rng);
            if rng.next(0.0, 1.0) < 0.5 {
                quads.push(other);
            } else {
                quads.insert(0, other);
            }
            pairs += 1;
        }
        let unit = rng.pick(&[1.0, 1.0, 1.0, 0.5, 100.0, 1000.0]);
        let v = visible(&mut rng);
        let c = check_squiggle(&quads, v, unit, LIMIT);
        match (c.refused, c.painting) {
            (true, _) => refused += 1,
            (false, 0) => none += 1,
            _ => drawn += 1,
        }
    }
    assert!(
        refused > 20 && drawn > 50 && none > 50 && pairs > 200,
        "refused {refused}, drawn {drawn}, none {none}, pairs {pairs}"
    );
}

/// The shapes merge reviews 9 and 10 found counted though they paint nothing:
/// a comb below the page whose border is wider than it is high, its lines reaching
/// onto the page but cut off by its box; squiggles whose invisible quad below the
/// page has steps within the fixed miter reach but strokes and joins farther than
/// a stroke paints beyond them, beside a visible quad; and a quad whose other quad
/// widens the clip they share onto the page, whose steps there paint.
#[test]
fn shapes_off_the_page_count_nothing() {
    let page = Rect::new(0.0, 0.0, 200.0, 200.0);
    for unit in [1.0, 100.0] {
        let c = check_comb(
            [20.0, -0.000_002, 180.0, -0.000_001],
            0,
            2000,
            0.00001,
            page,
            unit,
            LIMIT,
        );
        assert_eq!(c.painting, 0);
    }
    let p = |x: f64, y: f64| Point::new(x, y);
    let shown = [
        p(20.0, 114.0),
        p(180.0, 114.0),
        p(20.0, 100.0),
        p(180.0, 100.0),
    ];
    for below in [
        [p(0.0, -7.9), p(200.0, -7.9), p(0.0, -0.9), p(200.0, -0.9)],
        [p(7.0, -7.9), p(207.0, -7.9), p(0.0, -0.9), p(200.0, -0.9)],
        [p(0.0, -7.9), p(200.0, -8.6), p(0.0, -0.9), p(200.0, -0.9)],
    ] {
        let c = check_squiggle(&[shown, below], page, 1.0, 100);
        assert_eq!(c.painting, 80);
    }
    // Review 10: the second quad, 0.2 to 0.9 below the page, its own box clear of
    // it, widens the clip the two share onto the page's bottom, where its steps
    // paint: 2081 with those of the visible quad, refused below that limit.
    let widening = [p(0.0, -0.9), p(200.0, -0.9), p(0.0, -0.2), p(200.0, -0.2)];
    let c = check_squiggle(&[shown, widening], page, 1.0, 100);
    assert!(c.refused);
    let c = check_squiggle(&[shown, widening], page, 1.0, 3000);
    assert!(!c.refused);
    assert_eq!(c.painting, 2081);
}

/// The steps a squiggle of `quads` draws on a 200 pt page, as the quad of each, of
/// the first `of` quads: a step of another fails the test.
fn squiggle_steps(quads: &str, of: usize) -> Result<Vec<usize>, Unsupported> {
    set_most_pieces(262_144);
    let content = look(
        &format!("/Subtype /Squiggly /C [0] /Rect [0 0 1 1] /QuadPoints [{quads}]"),
        Rect::new(0.0, 0.0, 200.0, 200.0),
        1.0,
    )?;
    let numbers: Vec<f64> = quads.split(' ').map(|n| n.parse().unwrap()).collect();
    let zigzags: Vec<Vec<Point>> = numbers
        .chunks(8)
        .take(of)
        .map(|q| {
            let p = |i: usize| Point::new(q[2 * i], q[2 * i + 1]);
            zigzag([p(0), p(1), p(2), p(3)]).0
        })
        .collect();
    Ok(drawn_steps(&content, &zigzags)
        .into_iter()
        .map(|(n, _)| n)
        .collect())
}

/// Review 10, finding 1: beside the visible quad, a quad 0.3 below the page
/// reaching 4·10^6 pt left of it, whose strokes and joins stay below -0.2999: the
/// visible quad's 80 steps are drawn, as with the same quad starting at 0, though
/// its far corners move by half a point as written (its own corners near the page
/// do not).
#[test]
fn a_far_corner_does_not_widen_what_steps_reach() {
    let shown = "20 114 180 114 20 100 180 100";
    for x in ["-4000000", "0"] {
        let quads = format!("{shown} {x} -.3007 200 -.3007 {x} -.3 200 -.3");
        let steps = squiggle_steps(&quads, 1);
        assert_eq!(steps, Ok(vec![0; 80]), "{x}");
    }
}

/// The comb fields of review 10 whose width or height is twice the border's: the
/// clip inside the border is `1 1 0 38 re` or `10 10 0 80 re`, of no area, and
/// no line paints, however many cells; they draw what one cell draws, also turned.
#[test]
fn a_comb_whose_clip_has_no_area_draws_no_line() {
    let page = Rect::new(0.0, 0.0, 200.0, 200.0);
    for (rect, w, turn) in [
        ("[20 20 22 60]", "1", 0),
        ("[20 20 40 120]", "10", 0),
        ("[20 20 60 22]", "1", 90),
        ("[20 20 120 40]", "10", 270),
        ("[20 20 22 60]", "1", 180),
    ] {
        let annot = |max_len: &str| {
            format!(
                "/Subtype /Widget /FT /Tx /Ff 16777216 /MK << /BC [0] /R {turn} >> \
                 /MaxLen {max_len} /BS << /W {w} >> /Rect {rect}"
            )
        };
        set_most_pieces(262_144);
        let many = look(&annot("1000000"), page, 1.0);
        let one = look(&annot("1"), page, 1.0);
        assert!(
            many.as_ref().is_ok_and(|c| c.contains(" re\nS\n")),
            "{rect}: {many:?}"
        );
        assert_eq!(many, one, "{rect}");
        assert!(!many.unwrap().contains(" m\n"), "{rect}");
    }
}
