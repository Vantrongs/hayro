//! The pieces of repeated shapes (the dividers of a comb field, the steps of a
//! squiggle) that a generator draws and counts are exactly those that paint: whose
//! outline, as kurbo's stroker makes it at the same width, joins, miter limit and
//! caps, meets what the page shows under every clip the pieces are drawn in, grown
//! on the page by how far a stroke paints beyond its outline (a quarter of a point
//! across and up, in the page's units: `/UserUnit` points each). A shape with more
//! such pieces than the limit is refused, and only then. The reference here takes
//! every piece of shapes of a few hundred, one by one; the generators find them
//! among billions.

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

/// The part of the convex polygon `poly` inside the convex polygon `by`.
fn cut(poly: Vec<Point>, by: &[Point]) -> Vec<Point> {
    let n = by.len();
    let area: f64 = (0..n)
        .map(|i| cross(by[i].to_vec2(), by[(i + 1) % n].to_vec2()))
        .sum();
    let mut poly = poly;
    for i in 0..n {
        let (a, b) = (by[i], by[(i + 1) % n]);
        let side = |p: Point| cross(b - a, p - a) * area.signum();
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
        if poly.is_empty() {
            break;
        }
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
    let mut path = BezPath::new();
    path.move_to(c - u1 * 1e-9);
    path.line_to(c);
    path.line_to(c + u2 * 1e-9);
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

/// A number as written in a file, rounded to 32 bits as viewers read it.
fn f32ed(v: f64) -> f64 {
    f64::from(v as f32)
}

/// What a check found: the pieces that paint, and whether the shape was drawn as
/// they say.
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
    // Drawn under the box's clip and the clip to the box inside the border.
    let inner = Rect::new(w, w, fw - w, fh - w).abs();
    let (cx0, cy0, cx1, cy1) = (
        inner.x0.max(0.0),
        inner.y0.max(0.0),
        inner.x1.min(fw),
        inner.y1.min(fh),
    );
    let region = if cx0 <= cx1 && cy0 <= cy1 {
        let clip: Vec<Point> = rect_corners(Rect::new(cx0, cy0, cx1, cy1))
            .into_iter()
            .map(|p| place * p)
            .collect();
        cut(rect_corners(visible), &clip)
    } else {
        vec![]
    };
    let g = if region.is_empty() {
        vec![]
    } else {
        grown(&region, BEYOND / unit)
    };
    let cells = f64::from(max_len);
    let step = (fw - 2.0 * w) / cells;
    let paints: Vec<u32> = if max_len < 2 || fh - 2.0 * w == 0.0 {
        vec![]
    } else {
        (1..max_len)
            .filter(|&i| {
                let u = w + step * f64::from(i);
                let mut line = BezPath::new();
                line.move_to((u, w));
                line.line_to((u, fh - w));
                meets(&(place * stroke(line, w)), &g)
            })
            .collect()
    };
    set_most_pieces(limit);
    let got = look(&annot, visible, unit);
    let refused = paints.len() > limit as usize;
    match got {
        Err(why) => {
            assert!(
                refused,
                "{annot} {visible:?} unit {unit}: {why:?}, {paints:?} paint"
            );
            assert_eq!(why, Unsupported::Coordinates, "{annot}");
        }
        Ok(content) => {
            assert!(
                !refused,
                "{annot} {visible:?} unit {unit}: {} paint",
                paints.len()
            );
            let drawn: Vec<u32> = content
                .lines()
                .filter(|l| l.ends_with(" m"))
                .map(|l| {
                    let u: f64 = l.split(' ').next().unwrap().parse().unwrap();
                    let i = ((u - w) / step).round();
                    assert!(
                        (w + step * i - u).abs() < step.abs() / 4.0,
                        "{annot}: a line at {u}"
                    );
                    i as u32
                })
                .collect();
            assert_eq!(drawn, paints, "{annot} {visible:?} unit {unit}\n{content}");
        }
    }
    Checked {
        painting: paints.len(),
        refused,
    }
}

/// The corners of the zigzag of the quad `tl tr bl br`, as a viewer draws it
/// (steps of a seventh of its height along its bottom edge, from its bottom left
/// to the first at or past its bottom right; the corners alternately on the bottom
/// edge and on the line a seventh of the way up), as written; its width; and its
/// clip, its box grown by the width, as written.
fn zigzag(q: [Point; 4]) -> (Vec<Point>, f64, Rect) {
    let [tl, tr, bl, br] = q;
    let height = (tl - bl).hypot();
    let width = height / 16.0;
    let clip = Rect::from_points(tl, tr)
        .union_pt(bl)
        .union_pt(br)
        .inset(width);
    let [x, y, cw, ch] = [clip.x0, clip.y0, clip.width(), clip.height()].map(f32ed);
    let clip = Rect::new(x, y, x + cw, y + ch);
    if height == 0.0 {
        return (vec![], 0.0, clip);
    }
    let delta = height / 7.0;
    let len = (br - bl).hypot();
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
    (corners, f32ed(width), clip)
}

/// Checks a squiggle of the quads `quads` (each `tl tr bl br`) against the
/// reference; the limit holds for each quad.
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
    let mut all: Vec<(usize, usize)> = vec![];
    let mut corners_of = vec![];
    let mut refused = false;
    let mut painting = 0;
    for (n, q) in quads.iter().enumerate() {
        let (corners, width, clip) = zigzag(*q);
        let shown = Rect::new(
            visible.x0.max(clip.x0),
            visible.y0.max(clip.y0),
            visible.x1.min(clip.x1),
            visible.y1.min(clip.y1),
        );
        let g = if visible.overlaps(clip) {
            grown(&rect_corners(shown), BEYOND / unit)
        } else {
            vec![]
        };
        let paints: Vec<usize> = (0..corners.len().saturating_sub(1))
            .filter(|&k| {
                // The step's own stroke, butt-capped, or a join it shares with a
                // neighbour: drawn only where both steps are.
                let c = &corners;
                let mut line = BezPath::new();
                line.move_to(c[k]);
                line.line_to(c[k + 1]);
                meets(&stroke(line, width), &g)
                    || (k > 0 && convex_meets(&join(c[k - 1], c[k], c[k + 1], width), &g))
                    || (k + 2 < c.len() && convex_meets(&join(c[k], c[k + 1], c[k + 2], width), &g))
            })
            .collect();
        refused |= paints.len() > limit as usize;
        painting += paints.len();
        all.extend(paints.into_iter().map(|k| (n, k)));
        corners_of.push(corners);
    }
    set_most_pieces(limit);
    match look(&annot, visible, unit) {
        Err(why) => {
            assert!(
                refused,
                "{annot} {visible:?} unit {unit}: {why:?}, {all:?} paint"
            );
            assert_eq!(why, Unsupported::Coordinates, "{annot}");
        }
        Ok(content) => {
            assert!(!refused, "{annot} {visible:?} unit {unit}: {all:?} paint");
            // Each corner drawn, as the quad and corner nearest it; a step is drawn
            // where a run goes from one corner to the next.
            let nearest = |x: f64, y: f64| {
                let p = Point::new(x, y);
                let mut best = (f64::INFINITY, 0, 0);
                for (n, corners) in corners_of.iter().enumerate() {
                    for (k, c) in corners.iter().enumerate() {
                        let d = (*c - p).hypot();
                        if d < best.0 {
                            best = (d, n, k);
                        }
                    }
                }
                assert!(best.0 < 1e-3, "{annot}: a corner drawn at {x} {y}");
                (best.1, best.2)
            };
            let mut drawn = vec![];
            let mut at = None;
            for l in content.lines() {
                let parts: Vec<&str> = l.split(' ').collect();
                let op = parts.last().copied();
                if parts.len() != 3 || !matches!(op, Some("m" | "l")) {
                    continue;
                }
                let corner = nearest(parts[0].parse().unwrap(), parts[1].parse().unwrap());
                if op == Some("l") {
                    let (n, k) = at.expect("a run");
                    assert_eq!(corner, (n, k + 1), "{annot}: a run skips");
                    drawn.push((n, k));
                }
                at = Some(corner);
            }
            assert_eq!(drawn, all, "{annot} {visible:?} unit {unit}\n{content}");
        }
    }
    Checked { painting, refused }
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
/// borders, on, beside, above and below the page and partly on it, in units of
/// a point and of other sizes: those that paint more pieces than the limit are
/// refused, and the others draw exactly those that paint.
#[test]
fn combs_draw_and_count_exactly_the_dividers_that_paint() {
    let mut rng = Random(0x5eed_c0b5);
    let (mut refused, mut drawn, mut none) = (0, 0, 0);
    for _ in 0..400 {
        let (x, y) = place(&mut rng);
        let (fw, fh) = (
            rng.pick(&[5.0, 20.0, 80.0, 160.0]) * rng.next(0.8, 1.2),
            rng.pick(&[0.5, 4.0, 20.0, 40.0]) * rng.next(0.8, 1.2),
        );
        let turn = rng.pick(&[0, 0, 90, 180, 270, -90, 45, 30, 135, 200, 13]);
        let w = match rng.pick(&[0, 1, 2, 3, 4]) {
            0 => 0.01,
            1 => 0.3,
            2 => 1.0,
            3 => fh * rng.next(0.3, 0.6),
            _ => fh * rng.next(1.0, 2.0),
        };
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
        refused > 20 && drawn > 50 && none > 50,
        "refused {refused}, drawn {drawn}, none {none}"
    );
}

/// Random squiggles: flat and steep quads, slanted, with sides apart from
/// parallel, of no height, on, beside, above and below the page and partly on it,
/// in units of a point and of other sizes: those that paint more steps than the
/// limit are refused, and the others draw exactly those that paint.
#[test]
fn squiggles_draw_and_count_exactly_the_steps_that_paint() {
    let mut rng = Random(0x5eed_5a16);
    let (mut refused, mut drawn, mut none) = (0, 0, 0);
    for _ in 0..400 {
        let (x, y) = place(&mut rng);
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
        let q = [bl + up * h + lean, br + up * h + lean + spread, bl, br];
        let unit = rng.pick(&[1.0, 1.0, 1.0, 0.5, 100.0, 1000.0]);
        let v = visible(&mut rng);
        let c = check_squiggle(&[q], v, unit, LIMIT);
        match (c.refused, c.painting) {
            (true, _) => refused += 1,
            (false, 0) => none += 1,
            _ => drawn += 1,
        }
    }
    assert!(
        refused > 20 && drawn > 50 && none > 50,
        "refused {refused}, drawn {drawn}, none {none}"
    );
}

/// The shapes merge review 9 found counted though they paint nothing: a comb below
/// the page whose border is wider than it is high, its lines reaching onto the
/// page but cut off by its box; and squiggles whose invisible quad below the page
/// has steps within the fixed miter reach but strokes and joins farther than a
/// stroke paints beyond them, beside a visible quad. (Its own squiggle, 0.0001 pt
/// below the page, does paint: `PyMuPDF` 1.28.2 bake greys the page's bottom row,
/// ink 682 against 482 for the visible quad alone at 1 px/pt, so its 2 000 001
/// steps are refused, in `annotation_cost.rs`.)
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
}
