//! The parts of geometry annotations share, such as the paths of one `/InkList` or the
//! quadrilaterals of one `/QuadPoints`, each by the box it may paint in, held in a
//! tree made once for the document whatever page shows them: annotations that share
//! the parts find those that may paint where a page shows them by descending it,
//! each looking at the boxes that hold a part found, and at few others, rather than
//! at every part.

use super::out::{Extent, Rounding};
use kurbo::Point;
use std::cell::{Cell, OnceCell, RefCell};

/// Where a part may paint: within `reach` of the box `extent`, its points computed
/// as `local` says; or, for a node of the tree, where any part under it may.
#[derive(Clone, Copy, Debug)]
pub(super) struct Bound {
    pub(super) extent: Extent,
    pub(super) reach: f64,
    pub(super) local: Rounding,
}

impl Bound {
    /// Within `reach` of `extent`, its points as written.
    pub(super) fn of(extent: Extent, reach: f64) -> Self {
        Self {
            extent,
            reach,
            local: Rounding::NONE,
        }
    }

    /// Where any of `self` and `other` may paint.
    fn union(self, other: Self) -> Self {
        Self {
            extent: self.extent.union(other.extent),
            reach: self.reach.max(other.reach),
            local: self.local.either(other.local),
        }
    }
}

/// The most parts a leaf of the tree holds.
const LEAF: usize = 8;

/// The parts of shared geometry, by where each may paint, in a tree of boxes:
/// each node holds the parts under it and where any of them may paint, its two
/// children each half of them, split across the longer side of the box of their
/// middles, down to leaves of at most [`LEAF`]. Parts whose bound is no box (of a
/// point no finite number) are held apart and looked at each time; parts that
/// paint nothing, of no bound, not at all. Made once for the document, by the
/// parts alone, and kept with them: nothing in it depends on a page. The first
/// search looks at each part instead, so that parts searched once, as those of
/// one annotation shown once, cost no tree.
pub(super) struct Index {
    /// The parts, as given and, once the tree is made, leaf by leaf: the place of
    /// each and where it may paint.
    parts: RefCell<Vec<(u32, Bound)>>,
    /// The nodes, the root first; a node's children are next to each other.
    nodes: OnceCell<Vec<Node>>,
    /// The parts whose bound is no box.
    apart: Vec<(u32, Bound)>,
    /// Whether the parts were searched.
    searched: Cell<bool>,
}

#[derive(Clone, Copy)]
struct Node {
    bound: Bound,
    /// Its parts, `parts[start..end]`.
    start: u32,
    end: u32,
    /// Its children, `nodes[left]` and `nodes[left + 1]`; 0 for a leaf.
    left: u32,
}

impl Node {
    /// A node yet to be made, of no parts.
    fn blank(bound: Bound) -> Self {
        Self {
            bound,
            start: 0,
            end: 0,
            left: 0,
        }
    }
}

impl Index {
    /// The index of the parts `bounds` gives, in order: `None` for one that paints
    /// nothing anywhere.
    pub(super) fn new(bounds: impl IntoIterator<Item = Option<Bound>>) -> Self {
        let mut parts = vec![];
        let mut apart = vec![];
        for (i, bound) in bounds.into_iter().enumerate() {
            let i = u32::try_from(i).expect("fewer parts than 2^32");
            match bound {
                Some(b) if b.extent.is_box() => {
                    // A reach that is no number reaches anywhere.
                    let reach = if b.reach.is_nan() {
                        f64::INFINITY
                    } else {
                        b.reach
                    };
                    parts.push((i, Bound { reach, ..b }));
                }
                Some(b) => apart.push((i, b)),
                None => {}
            }
        }
        Self {
            parts: RefCell::new(parts),
            nodes: OnceCell::new(),
            apart,
            searched: Cell::new(false),
        }
    }

    /// Makes the tree, putting the parts in its order.
    fn tree(&self) -> Vec<Node> {
        let mut parts = self.parts.borrow_mut();
        if parts.is_empty() {
            return vec![];
        }
        // The parts are split by their middles, held apart from their bounds so
        // that a split moves little; a node's bound is made of its children's.
        let mut middles: Vec<(Point, u32)> = (parts.iter().enumerate())
            .map(|(k, (_, b))| (b.extent.middle(), k as u32))
            .collect();
        let mut nodes = vec![Node::blank(parts[0].1)];
        fill(&mut nodes, &parts, &mut middles, 0, 0);
        *parts = middles.iter().map(|(_, k)| parts[*k as usize]).collect();
        nodes
    }

    /// The places, in order, of the parts whose bound `near` tells may paint and
    /// that `paints` tells do: `near` must not fall as a bound grows (as its box,
    /// reach or rounding does), and must hold for every part `paints` holds for.
    /// Each node looked at and each part tested is a unit of work: the first time,
    /// every part; then those that hold a part found, the nodes on the way down to
    /// them, and those beside these whose box `near` cannot tell from theirs.
    pub(super) fn find(
        &self,
        near: impl Fn(&Bound) -> bool,
        mut paints: impl FnMut(usize) -> bool,
    ) -> Vec<usize> {
        let mut found = vec![];
        for (i, bound) in &self.apart {
            super::work(1);
            if near(bound) && paints(*i as usize) {
                found.push(*i as usize);
            }
        }
        if self.nodes.get().is_none() && !self.searched.replace(true) {
            for (i, bound) in self.parts.borrow().iter() {
                super::work(1);
                if near(bound) && paints(*i as usize) {
                    found.push(*i as usize);
                }
            }
            found.sort_unstable();
            return found;
        }
        let nodes = self.nodes.get_or_init(|| self.tree());
        let parts = self.parts.borrow();
        // The nodes left to look at, on the stack of the thread: a node's
        // children are pushed only where it was taken off, so no more are left
        // than the tree is deep, and one more.
        let mut left = [0_u32; 64];
        let mut top = usize::from(!nodes.is_empty());
        while top > 0 {
            top -= 1;
            let node = &nodes[left[top] as usize];
            super::work(1);
            if !near(&node.bound) {
                continue;
            }
            if node.left == 0 {
                for (i, bound) in &parts[node.start as usize..node.end as usize] {
                    super::work(1);
                    if (node.end - node.start == 1 || near(bound)) && paints(*i as usize) {
                        found.push(*i as usize);
                    }
                }
            } else {
                assert!(top + 2 <= left.len(), "a tree of fewer than 2^60 parts");
                left[top] = node.left + 1;
                left[top + 1] = node.left;
                top += 2;
            }
        }
        found.sort_unstable();
        found
    }
}

/// Makes node `at` of the parts whose middles are `middles`, each with the place
/// of its bound in `parts`, which start at `start` among all, and its nodes below,
/// splitting `middles` across the longer side of their box at the middle one, each
/// half the parts nearest each other along it; tells the node's bound.
fn fill(
    nodes: &mut Vec<Node>,
    parts: &[(u32, Bound)],
    middles: &mut [(Point, u32)],
    at: usize,
    start: usize,
) -> Bound {
    let mut node = Node {
        start: start as u32,
        end: (start + middles.len()) as u32,
        ..nodes[at]
    };
    node.bound = if middles.len() <= LEAF {
        (middles.iter())
            .map(|(_, k)| parts[*k as usize].1)
            .reduce(Bound::union)
            .expect("a node holds a part")
    } else {
        let (mut lo, mut hi) = (middles[0].0, middles[0].0);
        for (m, _) in middles.iter() {
            (lo.x, lo.y, hi.x, hi.y) = (lo.x.min(m.x), lo.y.min(m.y), hi.x.max(m.x), hi.y.max(m.y));
        }
        let across = hi.x - lo.x >= hi.y - lo.y;
        let along = |m: &Point| if across { m.x } else { m.y };
        let half = middles.len() / 2;
        middles.select_nth_unstable_by(half, |a, b| along(&a.0).total_cmp(&along(&b.0)));
        let left = nodes.len();
        nodes.extend([nodes[at]; 2]);
        node.left = left as u32;
        let (first, second) = middles.split_at_mut(half);
        let first = fill(nodes, parts, first, left, start);
        fill(nodes, parts, second, left + 1, start + half).union(first)
    };
    nodes[at] = node;
    node.bound
}

#[cfg(test)]
mod tests {
    use super::super::out::{Extent, Out, Rounding};
    use super::{Bound, Index};
    use kurbo::{Point, Rect};

    /// A number from `seed`, from 0 below 1.
    fn next(seed: &mut u64) -> f64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed >> 11) as f64 / (1_u64 << 53) as f64
    }

    /// The tree finds exactly the parts a test finds that does not fall as a
    /// bound grows: whether the part's box may paint at its reach and an
    /// annotation's, and whether it may meet what shows at its reach and
    /// rounding. Of generated parts, clustered, spread, far off, some copies of
    /// others, some of a point no finite number; under generated views, turns
    /// and clips. Fewer nodes and parts are looked at than all.
    #[test]
    fn the_tree_finds_the_parts_a_test_of_each_finds() {
        let mut seed = 0x6c07_8965_6e1a_0f3d_u64;
        let (mut found_any, mut looked, mut all) = (0, 0, 0);
        for case in 0..400 {
            let r = &mut seed;
            let n = 1 + (next(r) * 300.0) as usize;
            let spread = 10_f64.powf(next(r) * 6.0);
            let parts: Vec<Option<Bound>> = (0..n)
                .map(|i| {
                    if i > 0 && next(r) < 0.1 {
                        return None;
                    }
                    let (x, y) = (
                        next(r) * spread - spread / 2.0,
                        next(r) * spread - spread / 2.0,
                    );
                    let size = next(r) * 50.0;
                    let mut points =
                        vec![Point::new(x, y), Point::new(x + size, y + size * next(r))];
                    if next(r) < 0.02 {
                        points.push(Point::new(f64::NAN, 0.0));
                    }
                    let reach = if next(r) < 0.3 { 0.0 } else { next(r) * 40.0 };
                    let local = if next(r) < 0.5 {
                        Rounding::NONE
                    } else {
                        Rounding::of(next(r) * 1e5, 10_f64.powf(next(r) * 10.0))
                    };
                    Some(Bound {
                        extent: Extent::of(points),
                        reach,
                        local,
                    })
                })
                .collect();
            let index = Index::new(parts.iter().copied());
            for _ in 0..4 {
                let (x, y) = (next(r) * 400.0 - 200.0, next(r) * 400.0 - 200.0);
                let mut out = Out::new(
                    Rect::new(x, y, x + next(r) * 300.0, y + next(r) * 300.0),
                    1.0,
                );
                if case % 3 == 1 {
                    let t = next(r) * 6.4;
                    out.nums(&[t.cos(), t.sin(), -t.sin(), t.cos(), 10.0, -20.0], "cm");
                }
                if case % 2 == 1 {
                    out.clip(Rect::new(x, y, x + next(r) * 200.0, y + next(r) * 200.0));
                }
                let region = out.region();
                let painting = region.painting(Rounding::NONE);
                let reach = next(r) * 10.0;
                let tests: [&dyn Fn(&Bound) -> bool; 2] = [
                    &|b| painting.paints(&b.extent, b.reach + reach).is_some(),
                    &|b| painting.may_meet(&b.extent, b.reach, b.local),
                ];
                for near in tests {
                    let each: Vec<usize> = (0..n)
                        .filter(|&i| parts[i].as_ref().is_some_and(near))
                        .collect();
                    let before = super::super::work_done();
                    let found = index.find(near, |_| true);
                    assert_eq!(found, each, "case {case}");
                    looked += super::super::work_done() - before;
                    all += n as u64;
                    found_any += usize::from(!found.is_empty());
                }
            }
        }
        assert!(found_any > 1_000, "{found_any} found any");
        assert!(looked < all, "{looked} looked at of {all}");
    }
}
