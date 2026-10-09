//! Area (box) resampling of image planes onto coarser grids.
//!
//! A plane `n` texels wide divided into `g` bins (`1 <= g <= n`): bin `i` covers
//! `[i n / g, (i + 1) n / g)` of the plane, and its value is the mean of the texels
//! over that interval, each weighted by how much of it the bin covers. Both axes
//! alike; the result is exact (integer sums, rounded once to nearest), so it does not
//! depend on how a plane is cut into requests. Because a bin is at least one texel
//! long, a texel touches at most two bins per axis: rows stream through two
//! accumulator rows, and nothing source-sized is kept.

use core::ops::Range;

/// One axis: `n` texels onto `g` bins, in units where a texel is `g` long and a bin
/// `n` (both divided by their greatest common divisor).
#[derive(Clone, Copy, Debug)]
struct Axis {
    n: u64,
    g: u64,
}

impl Axis {
    fn new(n: u32, g: u32) -> Self {
        let d = gcd(n as u64, g as u64).max(1);
        Self {
            n: n as u64 / d,
            g: g as u64 / d,
        }
    }

    /// The texels bin `i` overlaps.
    fn texels(&self, i: u64) -> Range<u64> {
        i * self.n / self.g..((i + 1) * self.n).div_ceil(self.g)
    }

    /// How much of texel `k` bin `i` covers.
    fn weight(&self, k: u64, i: u64) -> u64 {
        let end = ((k + 1) * self.g).min((i + 1) * self.n);
        let start = (k * self.g).max(i * self.n);
        end.saturating_sub(start)
    }

    /// The bins texel `k` overlaps.
    fn bins(&self, k: u64) -> Range<u64> {
        k * self.g / self.n..((k + 1) * self.g).div_ceil(self.n)
    }

    fn is_identity(&self) -> bool {
        self.n == self.g
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// The texels of a plane `size` texels large that bins `window` (`[x0, y0, x1, y1)`)
/// of the plane divided into `grid` bins overlap, as `[x0, y0, x1, y1)`.
pub fn area_texels(size: (u32, u32), grid: (u32, u32), window: [u32; 4]) -> [u32; 4] {
    let (x, y) = (Axis::new(size.0, grid.0), Axis::new(size.1, grid.1));
    let xs = x.texels(window[0] as u64).start..x.texels(window[2] as u64 - 1).end;
    let ys = y.texels(window[1] as u64).start..y.texels(window[3] as u64 - 1).end;
    [xs.start, ys.start, xs.end, ys.end].map(|v| v as u32)
}

/// Area-resamples part of a plane `size` texels large, of `channels` bytes per texel:
/// `src` holds texels `src_window` (`[x0, y0, x1, y1)`, row by row), which must
/// include `area_texels(size, grid, window)`. Returns bins `window` of the plane
/// divided into `grid` bins, row by row.
pub fn area_resample(
    src: &[u8],
    src_window: [u32; 4],
    size: (u32, u32),
    channels: usize,
    grid: (u32, u32),
    window: [u32; 4],
) -> Vec<u8> {
    let mut area = Area::new(size, grid, window, channels);
    let stride = (src_window[2] - src_window[0]) as usize * channels;
    let cols = area.columns();
    let skip = (cols.start - src_window[0]) as usize * channels;
    let len = (cols.end - cols.start) as usize * channels;
    for y in area.rows() {
        let row = (y - src_window[1]) as usize * stride + skip;
        area.push(&src[row..row + len]);
    }
    area.finish().0
}

/// A bin's texels in a row of the columns an `Area` reads: the first and last (relative
/// to the first column read) and their weights; the texels between weigh a whole
/// texel.
#[derive(Clone, Copy, Debug)]
struct Span {
    first: usize,
    last: usize,
    first_weight: u64,
    last_weight: u64,
}

/// Area resampling fed row by row (see the module documentation): the source rows
/// `rows()`, each the texels `columns()` of `channels` bytes, in order.
pub(crate) struct Area {
    x: Axis,
    y: Axis,
    channels: usize,
    window: [u32; 4],
    cols: Range<u32>,
    spans: Vec<Span>,
    /// Vertically weighted sums of the current bin row and the next, per byte of the
    /// columns read.
    acc: Acc,
    /// The weight of the rows added to each of them.
    present: [u64; 2],
    /// The source row the next `push` adds.
    next_row: u64,
    /// The bin row `acc[0]` holds.
    bin_row: u64,
    /// The finished bin rows, and each one's coverage (`present` of `y.n`).
    out: Vec<u8>,
    coverage: Vec<u64>,
}

impl Area {
    /// Bins `window` (`[x0, y0, x1, y1)`, non-empty, within `grid`) of a plane `size`
    /// texels large divided into `grid` bins (`1..=size` on each axis).
    pub(crate) fn new(
        size: (u32, u32),
        grid: (u32, u32),
        window: [u32; 4],
        channels: usize,
    ) -> Self {
        let (x, y) = (Axis::new(size.0, grid.0), Axis::new(size.1, grid.1));
        let cols =
            x.texels(window[0] as u64).start as u32..x.texels(window[2] as u64 - 1).end as u32;
        let base = cols.start as u64;
        let spans = (window[0] as u64..window[2] as u64)
            .map(|i| {
                let t = x.texels(i);
                Span {
                    first: (t.start - base) as usize,
                    last: (t.end - 1 - base) as usize,
                    first_weight: x.weight(t.start, i),
                    last_weight: x.weight(t.end - 1, i),
                }
            })
            .collect();
        let width = (cols.end - cols.start) as usize * channels;
        let bins = (window[2] - window[0]) as usize * channels;
        let first_row = y.texels(window[1] as u64).start;
        Self {
            x,
            y,
            channels,
            window,
            cols,
            spans,
            // A sum is at most 255 `y.n`.
            acc: if 255 * y.n <= u32::MAX as u64 {
                Acc::Narrow([vec![0; width], vec![0; width]])
            } else {
                Acc::Wide([vec![0; width], vec![0; width]])
            },
            present: [0; 2],
            next_row: first_row,
            bin_row: window[1] as u64,
            out: Vec::with_capacity(bins * (window[3] - window[1]) as usize),
            coverage: Vec::with_capacity((window[3] - window[1]) as usize),
        }
    }

    /// The source rows to push, in order.
    pub(crate) fn rows(&self) -> Range<u32> {
        let first = self.y.texels(self.window[1] as u64).start;
        let end = self.y.texels(self.window[3] as u64 - 1).end;
        first as u32..end as u32
    }

    /// The texels of each row to push.
    pub(crate) fn columns(&self) -> Range<u32> {
        self.cols.clone()
    }

    /// Adds the next source row (`columns()` texels).
    pub(crate) fn push(&mut self, row: &[u8]) {
        let k = self.next_row;
        self.next_row += 1;
        if self.y.is_identity() && self.x.is_identity() {
            self.out.extend_from_slice(row);
            self.coverage.push(self.y.n);
            self.bin_row += 1;
            return;
        }
        for i in self.y.bins(k) {
            // Bins before the window are not kept; the row reaches at most the bin
            // after the current one.
            if i < self.bin_row || i >= self.window[3] as u64 {
                continue;
            }
            let slot = (i - self.bin_row) as usize;
            let w = self.y.weight(k, i);
            self.present[slot] += w;
            match &mut self.acc {
                // `w` is at most `y.n`.
                Acc::Narrow(acc) => accumulate(&mut acc[slot], row, w as u32),
                Acc::Wide(acc) => accumulate(&mut acc[slot], row, w),
            }
        }
        // The bin row ends with this source row.
        while self.bin_row < self.window[3] as u64
            && (k + 1) * self.y.g >= (self.bin_row + 1) * self.y.n
        {
            self.emit();
        }
    }

    /// Reduces `acc[0]` horizontally into the next output row, and moves on.
    fn emit(&mut self) {
        match &mut self.acc {
            Acc::Narrow(acc) => {
                reduce(
                    &acc[0],
                    &self.spans,
                    self.channels,
                    self.x,
                    self.y,
                    &mut self.out,
                );
                acc.swap(0, 1);
                acc[1].fill(0);
            }
            Acc::Wide(acc) => {
                reduce(
                    &acc[0],
                    &self.spans,
                    self.channels,
                    self.x,
                    self.y,
                    &mut self.out,
                );
                acc.swap(0, 1);
                acc[1].fill(0);
            }
        }
        self.coverage.push(self.present[0]);
        self.present = [self.present[1], 0];
        self.bin_row += 1;
    }

    /// Drops the rows pushed, to push them again from the first.
    pub(crate) fn reset(&mut self) {
        match &mut self.acc {
            Acc::Narrow(acc) => acc.iter_mut().for_each(|a| a.fill(0)),
            Acc::Wide(acc) => acc.iter_mut().for_each(|a| a.fill(0)),
        }
        self.present = [0; 2];
        self.next_row = self.y.texels(self.window[1] as u64).start;
        self.bin_row = self.window[1] as u64;
        self.out.clear();
        self.coverage.clear();
    }

    /// The bins, row by row, after every row pushed; when rows are missing (the data
    /// ended), the bins they fall in hold what the rows present add (missing texels
    /// count as zero), with `Some` coverage per bin row in `0..=255`.
    pub(crate) fn finish(mut self) -> (Vec<u8>, Option<Vec<u8>>) {
        let full = self.y.n;
        while self.bin_row < self.window[3] as u64 {
            if self.y.is_identity() && self.x.is_identity() {
                let len = (self.window[2] - self.window[0]) as usize * self.channels;
                self.out.resize(self.out.len() + len, 0);
                self.coverage.push(0);
                self.bin_row += 1;
            } else {
                self.emit();
            }
        }
        let coverage = self.coverage.iter().any(|&c| c < full).then(|| {
            self.coverage
                .iter()
                .map(|&c| div_round(255 * c, full))
                .collect()
        });
        (self.out, coverage)
    }
}

/// `s / d` rounded to the nearest integer (halves up), for `s <= 255 d`.
#[inline]
fn div_round(s: u64, d: u64) -> u8 {
    ((s + d / 2) / d) as u8
}

/// Vertical sums, in 32 bits where they fit (twice the lanes per vector).
enum Acc {
    Narrow([Vec<u32>; 2]),
    Wide([Vec<u64>; 2]),
}

/// Adds `row`, weighted by `w`, to `acc`.
#[inline]
fn accumulate<A>(acc: &mut [A], row: &[u8], w: A)
where
    A: Copy + From<u8> + core::ops::Mul<Output = A> + core::ops::AddAssign,
{
    for (a, &v) in acc.iter_mut().zip(row) {
        *a += w * A::from(v);
    }
}

/// Reduces vertical sums `acc` (per byte of the columns read) horizontally into one
/// row of bins.
fn reduce<A: Copy + Into<u64>>(
    acc: &[A],
    spans: &[Span],
    c: usize,
    x: Axis,
    y: Axis,
    out: &mut Vec<u8>,
) {
    if x.is_identity() {
        // `x.n` is 1: each sum is over one column.
        let d = Rounding::new(y.n);
        out.extend(acc.iter().map(|&s| d.round(s.into())));
        return;
    }
    let Some(d) = (x.n * y.n).checked_mul(256).map(|d| d / 256) else {
        // Sums or their rounding may not fit in 64 bits.
        let d = x.n as u128 * y.n as u128;
        for span in spans {
            for ch in 0..c {
                let at = |t: usize| acc[t * c + ch].into() as u128;
                let s = if span.first == span.last {
                    span.first_weight as u128 * at(span.first)
                } else {
                    let inner: u128 = (span.first + 1..span.last).map(at).sum();
                    span.first_weight as u128 * at(span.first)
                        + x.g as u128 * inner
                        + span.last_weight as u128 * at(span.last)
                };
                out.push(((s + d / 2) / d) as u8);
            }
        }
        return;
    };
    // Sums are at most 255 `d`.
    let d = Rounding::new(d);
    if c == 1 {
        for span in spans {
            let first = span.first_weight * acc[span.first].into();
            let s = if span.first == span.last {
                first
            } else {
                let inner: u64 = acc[span.first + 1..span.last]
                    .iter()
                    .map(|&a| a.into())
                    .sum();
                first + x.g * inner + span.last_weight * acc[span.last].into()
            };
            out.push(d.round(s));
        }
        return;
    }
    for span in spans {
        for ch in 0..c {
            let at = |t: usize| acc[t * c + ch].into();
            let s = if span.first == span.last {
                span.first_weight * at(span.first)
            } else {
                let inner: u64 = (span.first + 1..span.last).map(at).sum();
                span.first_weight * at(span.first) + x.g * inner + span.last_weight * at(span.last)
            };
            out.push(d.round(s));
        }
    }
}

/// `s / d` rounded to the nearest integer (halves up) for `s <= 255 d`, by a
/// multiplication where `d` allows (Granlund and Montgomery, "Division by invariant
/// integers using multiplication", 1994, theorem 4.2: with `l = ceil(log2 d)` and
/// `m = ceil(2^(N + l) / d)`, `floor(n / d) = floor(n m / 2^(N + l))` for every
/// `n < 2^N`).
#[derive(Clone, Copy)]
struct Rounding {
    d: u64,
    /// `m` and `N + l`, when `n m` fits in 128 bits.
    magic: Option<(u128, u32)>,
}

impl Rounding {
    fn new(d: u64) -> Self {
        let l = 64 - (d - 1).leading_zeros();
        // Numerators `s + d / 2` are below 256 d, so below 2^(8 + l).
        let n_bits = 8 + l;
        let k = n_bits + l;
        // `m` is below 2^(n_bits + 1): `n m` is below 2^(2 n_bits + 1).
        let magic = (2 * n_bits < 127).then(|| ((1_u128 << k).div_ceil(u128::from(d)), k));
        Self { d, magic }
    }

    #[inline]
    fn round(&self, s: u64) -> u8 {
        let n = s + self.d / 2;
        match self.magic {
            Some((m, k)) => ((n as u128 * m) >> k) as u8,
            None => (n / self.d) as u8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference: each bin's exact weighted mean, from rational overlaps computed
    /// texel by texel.
    fn reference(src: &[u8], size: (u32, u32), c: usize, grid: (u32, u32)) -> Vec<u8> {
        let (n, m) = (size.0 as u128, size.1 as u128);
        let (g, h) = (grid.0 as u128, grid.1 as u128);
        let overlap =
            |a0: u128, a1: u128, b0: u128, b1: u128| a1.min(b1).saturating_sub(a0.max(b0));
        let mut out = Vec::new();
        for j in 0..h {
            for i in 0..g {
                for ch in 0..c {
                    let (mut s, mut w) = (0_u128, 0_u128);
                    for y in 0..m {
                        // In units of 1 / (g h) texel area: texel x spans [x g, (x+1) g).
                        let wy = overlap(y * h, (y + 1) * h, j * m, (j + 1) * m);
                        for x in 0..n {
                            let wx = overlap(x * g, (x + 1) * g, i * n, (i + 1) * n);
                            let v = src[((y * n + x) as usize) * c + ch] as u128;
                            s += wx * wy * v;
                            w += wx * wy;
                        }
                    }
                    out.push(((2 * s + w) / (2 * w)) as u8);
                }
            }
        }
        out
    }

    fn noise(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 24) as u8
            })
            .collect()
    }

    /// Every grid of small odd and even planes equals the reference, whole and cut
    /// into windows of every size.
    #[test]
    fn bins_are_exact_weighted_means_in_any_window() {
        for (size, c) in [
            ((7, 5), 1),
            ((6, 4), 3),
            ((5, 9), 2),
            ((1, 3), 1),
            ((13, 2), 4),
        ] {
            let src = noise(
                size.0 as usize * size.1 as usize * c,
                size.0 * 31 + c as u32,
            );
            for gx in 1..=size.0 {
                for gy in 1..=size.1 {
                    let grid = (gx, gy);
                    let whole = reference(&src, size, c, grid);
                    let all = [0, 0, size.0, size.1];
                    assert_eq!(
                        area_resample(&src, all, size, c, grid, [0, 0, gx, gy]),
                        whole,
                        "{size:?} -> {grid:?}"
                    );
                    // Every window, from exactly the texels it needs.
                    for (x0, x1) in (0..gx).flat_map(|a| (a + 1..=gx).map(move |b| (a, b))) {
                        for (y0, y1) in (0..gy).flat_map(|a| (a + 1..=gy).map(move |b| (a, b))) {
                            let window = [x0, y0, x1, y1];
                            let t = area_texels(size, grid, window);
                            let part: Vec<u8> = (t[1]..t[3])
                                .flat_map(|y| {
                                    let row = (y * size.0) as usize * c;
                                    src[row + t[0] as usize * c..row + t[2] as usize * c].to_vec()
                                })
                                .collect();
                            let got = area_resample(&part, t, size, c, grid, window);
                            let expected: Vec<u8> = (y0..y1)
                                .flat_map(|j| {
                                    let row = (j * gx) as usize * c;
                                    whole[row + x0 as usize * c..row + x1 as usize * c].to_vec()
                                })
                                .collect();
                            assert_eq!(got, expected, "{size:?} -> {grid:?} {window:?}");
                        }
                    }
                }
            }
        }
    }

    /// The multiplication rounds as the division does, at the ends of the range and
    /// around every multiple of `d` for divisors of every size.
    #[test]
    fn rounding_by_multiplication_is_exact() {
        let mut ds: Vec<u64> = (1..=300).collect();
        for b in 9..=56 {
            let p = 1_u64 << b;
            ds.extend([p - 1, p, p + 1, p / 3 * 2 + 1]);
        }
        for d in ds {
            let r = Rounding::new(d);
            let mut ss = vec![0, 1, d / 2, 255 * d, 255 * d - 1];
            for q in [0, 1, 2, 127, 128, 254] {
                for off in [0, 1, d / 2, d / 2 + 1, d - 1] {
                    ss.push((q * d + off).min(255 * d));
                    ss.push((q * d + off).saturating_sub(d / 2).min(255 * d));
                }
            }
            for s in ss {
                assert_eq!(r.round(s), div_round(s, d), "{s} / {d}");
            }
        }
    }

    #[test]
    fn known_values() {
        // 3 texels into 2 bins: [0, 1.5) and [1.5, 3).
        assert_eq!(
            area_resample(&[0, 0, 255], [0, 0, 3, 1], (3, 1), 1, (2, 1), [0, 0, 2, 1]),
            [0, 170]
        );
        // A 1-texel line in 4 texels, reduced to 1: a quarter of its value.
        assert_eq!(
            area_resample(
                &[0, 200, 0, 0],
                [0, 0, 4, 1],
                (4, 1),
                1,
                (1, 1),
                [0, 0, 1, 1]
            ),
            [50]
        );
        // Halves round up.
        assert_eq!(
            area_resample(&[0, 1], [0, 0, 2, 1], (2, 1), 1, (1, 1), [0, 0, 1, 1]),
            [1]
        );
    }

    /// Rows missing at the end count as zero, and their bins report the coverage of
    /// the rows present.
    #[test]
    fn missing_rows_are_reported_as_coverage() {
        let mut area = Area::new((2, 4), (1, 2), [0, 0, 1, 2], 1);
        assert_eq!(area.rows(), 0..4);
        area.push(&[100, 100]);
        area.push(&[100, 100]);
        area.push(&[100, 100]);
        let (data, coverage) = area.finish();
        assert_eq!(data, [100, 50]);
        assert_eq!(coverage, Some(vec![255, 128]));
        // At full size, missing rows are empty.
        let mut area = Area::new((2, 3), (2, 3), [0, 0, 2, 3], 1);
        area.push(&[7, 8]);
        let (data, coverage) = area.finish();
        assert_eq!(data, [7, 8, 0, 0, 0, 0]);
        assert_eq!(coverage, Some(vec![255, 0, 0]));
    }
}
