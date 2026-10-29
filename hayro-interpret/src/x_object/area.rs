//! Area (box) resampling of image planes onto coarser grids.
//!
//! A plane `n` texels wide divided into `g` bins (`1 <= g <= n`): bin `i` covers
//! `[i n / g, (i + 1) n / g)` of the plane, and its value is the mean of the texels
//! over that interval, each weighted by how much of it the bin covers. Both axes
//! alike; the result is exact (integer sums, rounded once to nearest), so it does not
//! depend on how a plane is cut into requests. Because a bin is at least one texel
//! long, a texel touches at most two bins per axis: the sums stream through two rows,
//! either of sums down each column, reduced across as a row of bins ends, or, where
//! bins are wide, of bins, each source row summed across first. Either way what is
//! kept grows with the bins asked for, not with the source. Work over a row is done a
//! piece at a time, each counted on a meter, however wide the row.

use core::ops::Range;
use hayro_syntax::work::{Cancelled, Meter, POLL_BYTES};

#[cfg(test)]
std::thread_local! {
    /// The texels pushed on this thread, which tests bound.
    pub(crate) static PUSHED: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

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
    let meter = Meter::default();
    Area::new(size, grid, window, channels, &meter)
        .and_then(|area| resample(area, src, src_window))
        .expect("a meter without a hook never stops")
}

/// `area` fed from `src`, which holds texels `src_window`.
fn resample(mut area: Area<'_>, src: &[u8], src_window: [u32; 4]) -> Result<Vec<u8>, Cancelled> {
    let channels = area.channels;
    let stride = (src_window[2] - src_window[0]) as usize * channels;
    let cols = area.columns();
    let skip = (cols.start - src_window[0]) as usize * channels;
    let len = (cols.end - cols.start) as usize * channels;
    for y in area.rows() {
        let row = (y - src_window[1]) as usize * stride + skip;
        area.push(&src[row..row + len])?;
    }
    Ok(area.finish()?.0)
}

/// Runs `f` over `0..len` in pieces of at most `POLL_BYTES`, counting each on `meter`.
fn in_pieces(
    meter: &Meter<'_>,
    len: usize,
    mut f: impl FnMut(Range<usize>),
) -> Result<(), Cancelled> {
    let mut start = 0;
    while start < len {
        let end = len.min(start + POLL_BYTES);
        f(start..end);
        meter.work(end - start)?;
        start = end;
    }
    Ok(())
}

/// Fills `data` with `value` a piece at a time, counting each on `meter`.
fn fill<T: Copy>(meter: &Meter<'_>, data: &mut [T], value: T) -> Result<(), Cancelled> {
    in_pieces(meter, data.len(), |r| data[r].fill(value))
}

/// The most columns per bin for which an `Area` sums down the columns first: for
/// wider bins, summing each row across first is no slower, and keeps a sum per bin
/// instead of per column.
const COLUMNS_PER_BIN: u64 = 16;

/// A bin's texels in a row (relative to the first column an `Area` reads): the first
/// and last and their weights; the texels between weigh a whole texel.
#[derive(Clone, Copy, Debug)]
struct Span {
    first: usize,
    last: usize,
    first_weight: u64,
    last_weight: u64,
}

/// The spans of bins `window` over columns `cols`.
fn spans(
    x: Axis,
    window: [u32; 4],
    cols: &Range<u32>,
    meter: &Meter<'_>,
) -> Result<Vec<Span>, Cancelled> {
    let base = u64::from(cols.start);
    let w0 = u64::from(window[0]);
    let mut spans = Vec::with_capacity((window[2] - window[0]) as usize);
    in_pieces(meter, (window[2] - window[0]) as usize, |bins| {
        spans.extend(bins.map(|b| {
            let i = w0 + b as u64;
            let t = x.texels(i);
            Span {
                first: (t.start - base) as usize,
                last: (t.end - 1 - base) as usize,
                first_weight: x.weight(t.start, i),
                last_weight: x.weight(t.end - 1, i),
            }
        }));
    })?;
    Ok(spans)
}

/// Area resampling fed row by row (see the module documentation): the source rows
/// `rows()`, in order, each the texels `columns()` of `channels` bytes, pushed whole
/// or in parts from left to right. Its work counts on `meter`.
pub(crate) struct Area<'m> {
    x: Axis,
    y: Axis,
    channels: usize,
    window: [u32; 4],
    cols: Range<u32>,
    sums: Sums,
    /// The weight of the rows added to each of the two rows summed.
    present: [u64; 2],
    /// The source row being pushed.
    next_row: u64,
    /// The first bin row summed.
    bin_row: u64,
    /// The finished bin rows, and each one's coverage (`present` of `y.n`). At the
    /// plane's size, the row being pushed goes straight to `out`.
    out: Vec<u8>,
    coverage: Vec<u64>,
    meter: &'m Meter<'m>,
}

/// The sums of an `Area` below the plane's size.
enum Sums {
    None,
    Columns(Columns),
    /// Each row summed across first, in the narrowest integers that hold a bin's sum,
    /// at most 255 `x.n y.n`.
    U32(Bins<u32>),
    U64(Bins<u64>),
    U128(Bins<u128>),
}

/// Sums down each column read, of the two rows of bins summed, per byte of the
/// columns, reduced across into a row of bins as it ends.
struct Columns {
    spans: Vec<Span>,
    acc: Acc,
}

/// Sums down columns, in 32 bits where they fit (twice the lanes per vector).
enum Acc {
    Narrow([Vec<u32>; 2]),
    Wide([Vec<u64>; 2]),
}

impl Columns {
    fn new(
        x: Axis,
        y: Axis,
        window: [u32; 4],
        cols: &Range<u32>,
        channels: usize,
        meter: &Meter<'_>,
    ) -> Result<Self, Cancelled> {
        let width = (cols.end - cols.start) as usize * channels;
        Ok(Self {
            spans: spans(x, window, cols, meter)?,
            // A sum is at most 255 `y.n`.
            acc: if 255 * y.n <= u64::from(u32::MAX) {
                Acc::Narrow([vec![0; width], vec![0; width]])
            } else {
                Acc::Wide([vec![0; width], vec![0; width]])
            },
        })
    }

    /// Adds `texels`, weighted by `w`, to row `slot` of the sums from byte `at` on.
    fn push(&mut self, slot: usize, w: u64, at: usize, texels: &[u8]) {
        match &mut self.acc {
            // `w` is at most `y.n`.
            Acc::Narrow(acc) => {
                accumulate(&mut acc[slot][at..at + texels.len()], texels, w as u32);
            }
            Acc::Wide(acc) => accumulate(&mut acc[slot][at..at + texels.len()], texels, w),
        }
    }

    /// Adds `texel`, weighted by `w`, to each column of row `slot` of the sums from
    /// byte `at` on.
    fn push_rest(
        &mut self,
        slot: usize,
        w: u64,
        at: usize,
        texel: &[u8],
        meter: &Meter<'_>,
    ) -> Result<(), Cancelled> {
        let c = texel.len();
        match &mut self.acc {
            Acc::Narrow(acc) => in_pieces(meter, acc[slot][at..].len() / c, |r| {
                accumulate_texel(
                    &mut acc[slot][at + r.start * c..at + r.end * c],
                    texel,
                    w as u32,
                );
            }),
            Acc::Wide(acc) => in_pieces(meter, acc[slot][at..].len() / c, |r| {
                accumulate_texel(&mut acc[slot][at + r.start * c..at + r.end * c], texel, w);
            }),
        }
    }

    fn emit(
        &mut self,
        c: usize,
        x: Axis,
        y: Axis,
        out: &mut Vec<u8>,
        meter: &Meter<'_>,
    ) -> Result<(), Cancelled> {
        match &mut self.acc {
            Acc::Narrow(acc) => emit(acc, &self.spans, c, x, y, out, meter),
            Acc::Wide(acc) => emit(acc, &self.spans, c, x, y, out, meter),
        }
    }

    fn clear(&mut self, meter: &Meter<'_>) -> Result<(), Cancelled> {
        match &mut self.acc {
            Acc::Narrow(acc) => acc.iter_mut().try_for_each(|a| fill(meter, a, 0)),
            Acc::Wide(acc) => acc.iter_mut().try_for_each(|a| fill(meter, a, 0)),
        }
    }
}

struct Bins<A> {
    spans: Vec<Span>,
    /// Whether the texels between a span's ends sum in 32 bits.
    short_spans: bool,
    /// The source row being pushed, summed across each bin, per byte of the bins.
    row: Vec<A>,
    /// The two rows of bins summed, per byte of the bins.
    acc: [Vec<A>; 2],
}

impl<A: Sum> Bins<A> {
    fn new(
        x: Axis,
        window: [u32; 4],
        cols: &Range<u32>,
        channels: usize,
        meter: &Meter<'_>,
    ) -> Result<Self, Cancelled> {
        let bins = (window[2] - window[0]) as usize * channels;
        Ok(Self {
            spans: spans(x, window, cols, meter)?,
            // A span has at most `ceil(n / g) + 1` texels.
            short_spans: 255 * (x.n.div_ceil(x.g) + 1) <= u64::from(u32::MAX),
            row: vec![A::default(); bins],
            acc: [vec![A::default(); bins], vec![A::default(); bins]],
        })
    }

    /// Adds `texels`, of `c` bytes each from texel `base` (relative to the first
    /// column read) of the row being pushed.
    fn push(&mut self, x: Axis, w0: u64, first: u64, base: usize, texels: &[u8], c: usize) {
        let count = texels.len() / c;
        let end = base + count;
        let bins = x.bins(first).start.saturating_sub(w0)
            ..x.bins(first + count as u64 - 1).end.saturating_sub(w0);
        let (mut lo, mut hi) = (
            bins.start as usize,
            (bins.end as usize).min(self.spans.len()),
        );
        // Spans cut by the ends of the part.
        if lo < hi && self.spans[lo].first < base {
            self.add_span(lo, x.g, base, texels, c);
            lo += 1;
        }
        if lo < hi && self.spans[hi - 1].last >= end {
            self.add_span(hi - 1, x.g, base, texels, c);
            hi -= 1;
        }
        let (spans, row) = (&self.spans[lo..hi], &mut self.row[lo * c..hi * c]);
        if x.g == 1 && self.short_spans && lo < hi {
            // Every bin is `n` whole texels.
            let texels =
                &texels[(spans[0].first - base) * c..(spans[hi - lo - 1].last + 1 - base) * c];
            let n = x.n as usize;
            match c {
                1 => return add_blocks::<1, A>(n, texels, row),
                3 => return add_blocks::<3, A>(n, texels, row),
                4 => return add_blocks::<4, A>(n, texels, row),
                _ => {}
            }
        }
        match c {
            1 if self.short_spans => add_spans::<1, A>(spans, x.g, base, texels, row),
            2 if self.short_spans => add_spans::<2, A>(spans, x.g, base, texels, row),
            3 if self.short_spans => add_spans::<3, A>(spans, x.g, base, texels, row),
            4 if self.short_spans => add_spans::<4, A>(spans, x.g, base, texels, row),
            _ => (lo..hi).for_each(|i| self.add_span(i, x.g, base, texels, c)),
        }
    }

    fn add_span(&mut self, i: usize, g: u64, base: usize, texels: &[u8], c: usize) {
        add_span(
            self.spans[i],
            g,
            base,
            texels,
            &mut self.row[i * c..(i + 1) * c],
        );
    }

    /// Adds `texel` for every column of the row being pushed from column `first` on,
    /// per bin by how much of it those columns cover.
    fn push_rest(
        &mut self,
        x: Axis,
        w0: u64,
        first: u64,
        texel: &[u8],
        meter: &Meter<'_>,
    ) -> Result<(), Cancelled> {
        let c = texel.len();
        let from = x.bins(first).start.saturating_sub(w0) as usize;
        let bins = (self.row.len() / c).saturating_sub(from);
        in_pieces(meter, bins, |r| {
            let sums = &mut self.row[(from + r.start) * c..(from + r.end) * c];
            for (i, sums) in (w0 + (from + r.start) as u64..).zip(sums.chunks_exact_mut(c)) {
                let w = A::of(((i + 1) * x.n).saturating_sub((first * x.g).max(i * x.n)));
                for (s, &v) in sums.iter_mut().zip(texel) {
                    *s += w * A::of(u64::from(v));
                }
            }
        })
    }

    /// Adds the row pushed, weighted by `w`, to row `slot` of the bins.
    fn add(&mut self, slot: usize, w: u64, meter: &Meter<'_>) -> Result<(), Cancelled> {
        let w = A::of(w);
        let pieces = self.acc[slot].chunks_mut(POLL_BYTES);
        for (acc, row) in pieces.zip(self.row.chunks(POLL_BYTES)) {
            for (a, &v) in acc.iter_mut().zip(row) {
                *a += w * v;
            }
            meter.work(row.len())?;
        }
        Ok(())
    }

    /// Clears the row pushed.
    fn clear_row(&mut self, meter: &Meter<'_>) -> Result<(), Cancelled> {
        fill(meter, &mut self.row, A::default())
    }

    /// Rounds the first row of bins into `out`, and makes the second the first.
    fn emit(
        &mut self,
        x: Axis,
        y: Axis,
        out: &mut Vec<u8>,
        meter: &Meter<'_>,
    ) -> Result<(), Cancelled> {
        for piece in self.acc[0].chunks(POLL_BYTES) {
            A::round(piece, x.n, y.n, out);
            meter.work(piece.len())?;
        }
        self.acc.swap(0, 1);
        fill(meter, &mut self.acc[1], A::default())
    }

    fn clear(&mut self, meter: &Meter<'_>) -> Result<(), Cancelled> {
        self.clear_row(meter)?;
        self.acc
            .iter_mut()
            .try_for_each(|a| fill(meter, a, A::default()))
    }
}

/// A bin's sum.
trait Sum:
    Copy
    + Default
    + core::ops::Add<Output = Self>
    + core::ops::AddAssign
    + core::ops::Mul<Output = Self>
{
    /// `v`, which the sum holds.
    fn of(v: u64) -> Self;

    /// Each of `sums` divided by `n m` and rounded, into `out`.
    fn round(sums: &[Self], n: u64, m: u64, out: &mut Vec<u8>);
}

impl Sum for u32 {
    #[inline]
    fn of(v: u64) -> Self {
        v as Self
    }

    fn round(sums: &[Self], n: u64, m: u64, out: &mut Vec<u8>) {
        let d = Rounding::new(n * m);
        out.extend(sums.iter().map(|&s| d.round(u64::from(s))));
    }
}

impl Sum for u64 {
    #[inline]
    fn of(v: u64) -> Self {
        v
    }

    fn round(sums: &[Self], n: u64, m: u64, out: &mut Vec<u8>) {
        let d = Rounding::new(n * m);
        out.extend(sums.iter().map(|&s| d.round(s)));
    }
}

impl Sum for u128 {
    #[inline]
    fn of(v: u64) -> Self {
        v.into()
    }

    fn round(sums: &[Self], n: u64, m: u64, out: &mut Vec<u8>) {
        let d = Self::from(n) * Self::from(m);
        out.extend(sums.iter().map(|&s| ((s + d / 2) / d) as u8));
    }
}

/// Calls `$f` with the `Bins` of `$sums`, if any.
macro_rules! with_bins {
    ($sums:expr, $bins:ident => $f:expr) => {
        match $sums {
            Sums::None | Sums::Columns(_) => {}
            Sums::U32($bins) => $f,
            Sums::U64($bins) => $f,
            Sums::U128($bins) => $f,
        }
    };
}

/// The two rows summed from bin row `first`, before bin row `end`, that source row `k`
/// adds to, and its weight in each.
fn row_bins(y: Axis, k: u64, first: u64, end: u64) -> impl Iterator<Item = (usize, u64)> {
    // Bins before the window are not kept; the row reaches at most the bin after the
    // current one.
    y.bins(k)
        .filter(move |&i| i >= first && i < end)
        .map(move |i| ((i - first) as usize, y.weight(k, i)))
}

impl<'m> Area<'m> {
    /// Bins `window` (`[x0, y0, x1, y1)`, non-empty, within `grid`) of a plane `size`
    /// texels large divided into `grid` bins (`1..=size` on each axis), the work
    /// counted on `meter`.
    pub(crate) fn new(
        size: (u32, u32),
        grid: (u32, u32),
        window: [u32; 4],
        channels: usize,
        meter: &'m Meter<'m>,
    ) -> Result<Self, Cancelled> {
        let cols = columns(Axis::new(size.0, grid.0), window);
        let bins = u64::from(window[2] - window[0]);
        let across = u64::from(cols.end - cols.start) > COLUMNS_PER_BIN * bins;
        Self::summed(size, grid, window, channels, across, meter)
    }

    /// As `new`, summing each row across first when `across`, else down the columns
    /// first.
    fn summed(
        size: (u32, u32),
        grid: (u32, u32),
        window: [u32; 4],
        channels: usize,
        across: bool,
        meter: &'m Meter<'m>,
    ) -> Result<Self, Cancelled> {
        let (x, y) = (Axis::new(size.0, grid.0), Axis::new(size.1, grid.1));
        let cols = columns(x, window);
        let bins = (window[2] - window[0]) as usize * channels;
        let d = u128::from(x.n) * u128::from(y.n);
        let sums = if x.is_identity() && y.is_identity() {
            Sums::None
        } else if !across {
            Sums::Columns(Columns::new(x, y, window, &cols, channels, meter)?)
        } else if 255 * d <= u128::from(u32::MAX) {
            Sums::U32(Bins::new(x, window, &cols, channels, meter)?)
        } else if 256 * d <= u128::from(u64::MAX) {
            Sums::U64(Bins::new(x, window, &cols, channels, meter)?)
        } else {
            Sums::U128(Bins::new(x, window, &cols, channels, meter)?)
        };
        Ok(Self {
            x,
            y,
            channels,
            window,
            sums,
            cols,
            present: [0; 2],
            next_row: y.texels(window[1] as u64).start,
            bin_row: window[1] as u64,
            out: Vec::with_capacity(bins * (window[3] - window[1]) as usize),
            coverage: Vec::with_capacity((window[3] - window[1]) as usize),
            meter,
        })
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
    pub(crate) fn push(&mut self, row: &[u8]) -> Result<(), Cancelled> {
        self.push_texels(self.cols.start, row)?;
        self.end_row()
    }

    /// Adds texels from column `first` on of the source row being pushed: the texels
    /// of `columns()` after those pushed before.
    pub(crate) fn push_texels(&mut self, first: u32, texels: &[u8]) -> Result<(), Cancelled> {
        let piece = POLL_BYTES / self.channels;
        for (k, part) in texels.chunks(piece * self.channels).enumerate() {
            self.push_part(first + (k * piece) as u32, part);
            self.meter.work(part.len())?;
        }
        Ok(())
    }

    /// `push_texels` of at most a piece.
    fn push_part(&mut self, first: u32, texels: &[u8]) {
        #[cfg(test)]
        PUSHED.set(PUSHED.get() + (texels.len() / self.channels) as u64);
        let (x, c) = (self.x, self.channels);
        let base = (first - self.cols.start) as usize;
        match &mut self.sums {
            Sums::None => self.out.extend_from_slice(texels),
            Sums::Columns(columns) => {
                let end = u64::from(self.window[3]);
                for (slot, w) in row_bins(self.y, self.next_row, self.bin_row, end) {
                    columns.push(slot, w, base * c, texels);
                }
            }
            sums => {
                let w0 = u64::from(self.window[0]);
                with_bins!(sums, bins => bins.push(x, w0, u64::from(first), base, texels, c));
            }
        }
    }

    /// Adds the rest of the source row being pushed, from column `first` to the end
    /// of `columns()`, every texel `texel`, in time the bins bound, not the columns.
    pub(crate) fn push_rest(&mut self, first: u32, texel: &[u8]) -> Result<(), Cancelled> {
        let (x, c, meter) = (self.x, self.channels, self.meter);
        match &mut self.sums {
            Sums::None => {
                let out = &mut self.out;
                in_pieces(meter, (self.cols.end - first) as usize, |r| {
                    r.for_each(|_| out.extend_from_slice(texel));
                })?;
            }
            _ if texel.iter().all(|&v| v == 0) => {}
            Sums::Columns(columns) => {
                // At most `COLUMNS_PER_BIN` columns a bin.
                let at = (first - self.cols.start) as usize * c;
                let end = u64::from(self.window[3]);
                for (slot, w) in row_bins(self.y, self.next_row, self.bin_row, end) {
                    columns.push_rest(slot, w, at, texel, meter)?;
                }
            }
            sums => {
                let w0 = u64::from(self.window[0]);
                with_bins!(sums, bins => bins.push_rest(x, w0, u64::from(first), texel, meter)?);
            }
        }
        Ok(())
    }

    /// Ends the source row being pushed, all of whose texels are pushed.
    pub(crate) fn end_row(&mut self) -> Result<(), Cancelled> {
        // A row whose texels added nothing (zero summed down the columns) still
        // counts, so that a run of such rows polls the meter.
        self.meter.work(1)?;
        let k = self.next_row;
        self.next_row += 1;
        if let Sums::None = self.sums {
            self.coverage.push(self.y.n);
            self.bin_row += 1;
            return Ok(());
        }
        let meter = self.meter;
        for (slot, w) in row_bins(self.y, k, self.bin_row, u64::from(self.window[3])) {
            self.present[slot] += w;
            with_bins!(&mut self.sums, bins => bins.add(slot, w, meter)?);
        }
        with_bins!(&mut self.sums, bins => bins.clear_row(meter)?);
        // The bin row ends with this source row.
        while self.bin_row < self.window[3] as u64
            && (k + 1) * self.y.g >= (self.bin_row + 1) * self.y.n
        {
            self.emit()?;
        }
        Ok(())
    }

    /// Rounds the first row of bins summed into `out`, and moves on.
    fn emit(&mut self) -> Result<(), Cancelled> {
        let (x, y, c, meter) = (self.x, self.y, self.channels, self.meter);
        match &mut self.sums {
            Sums::Columns(columns) => columns.emit(c, x, y, &mut self.out, meter)?,
            sums => with_bins!(sums, bins => bins.emit(x, y, &mut self.out, meter)?),
        }
        self.coverage.push(self.present[0]);
        self.present = [self.present[1], 0];
        self.bin_row += 1;
        Ok(())
    }

    /// Drops the rows pushed, to push them again from the first, or to push none.
    pub(crate) fn reset(&mut self) -> Result<(), Cancelled> {
        let meter = self.meter;
        match &mut self.sums {
            Sums::Columns(columns) => columns.clear(meter)?,
            sums => with_bins!(sums, bins => bins.clear(meter)?),
        }
        self.present = [0; 2];
        self.next_row = self.y.texels(self.window[1] as u64).start;
        self.bin_row = self.window[1] as u64;
        self.out.clear();
        self.coverage.clear();
        Ok(())
    }

    /// The bins, row by row, after every row pushed; when rows are missing (the data
    /// ended), the bins they fall in hold what the rows present add (missing texels
    /// count as zero), with `Some` coverage per bin row in `0..=255`.
    pub(crate) fn finish(mut self) -> Result<(Vec<u8>, Option<Vec<u8>>), Cancelled> {
        let (full, meter) = (self.y.n, self.meter);
        while self.bin_row < self.window[3] as u64 {
            if let Sums::None = self.sums {
                let len = (self.window[2] - self.window[0]) as usize * self.channels;
                let out = &mut self.out;
                in_pieces(meter, len, |r| out.resize(out.len() + r.len(), 0))?;
                self.coverage.push(0);
                self.bin_row += 1;
            } else {
                self.emit()?;
            }
        }
        let mut partial = false;
        in_pieces(meter, self.coverage.len(), |r| {
            partial |= self.coverage[r].iter().any(|&c| c < full);
        })?;
        if !partial {
            return Ok((self.out, None));
        }
        let mut coverage = Vec::with_capacity(self.coverage.len());
        in_pieces(meter, self.coverage.len(), |r| {
            coverage.extend(self.coverage[r].iter().map(|&c| div_round(255 * c, full)));
        })?;
        Ok((self.out, Some(coverage)))
    }
}

/// The texels of a plane on axis `x` that bins `window` overlap across.
fn columns(x: Axis, window: [u32; 4]) -> Range<u32> {
    x.texels(window[0] as u64).start as u32..x.texels(window[2] as u64 - 1).end as u32
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

/// Adds `texel`, weighted by `w`, to each texel of `acc`.
fn accumulate_texel<A>(acc: &mut [A], texel: &[u8], w: A)
where
    A: Copy + From<u8> + core::ops::Mul<Output = A> + core::ops::AddAssign,
{
    for sums in acc.chunks_exact_mut(texel.len()) {
        accumulate(sums, texel, w);
    }
}

/// Reduces the bin row `acc[0]` into `out` and makes `acc[1]` the current one.
fn emit<A: Copy + Default + Into<u64>>(
    acc: &mut [Vec<A>; 2],
    spans: &[Span],
    c: usize,
    x: Axis,
    y: Axis,
    out: &mut Vec<u8>,
    meter: &Meter<'_>,
) -> Result<(), Cancelled> {
    let sums = &acc[0];
    if x.is_identity() {
        // `x.n` is 1: each sum is over one column.
        let d = Rounding::new(y.n);
        for piece in sums.chunks(POLL_BYTES) {
            out.extend(piece.iter().map(|&s| d.round(s.into())));
            meter.work(piece.len())?;
        }
    } else {
        // A span is at most `COLUMNS_PER_BIN` columns.
        let columns = COLUMNS_PER_BIN as usize;
        for piece in spans.chunks(POLL_BYTES / columns) {
            reduce(sums, piece, c, x, y, out);
            meter.work(piece.len() * columns)?;
        }
    }
    acc.swap(0, 1);
    fill(meter, &mut acc[1], A::default())
}

/// Reduces vertical sums `acc` (per byte of the columns read) horizontally into the
/// bins of `spans`, wider than a column.
fn reduce<A: Copy + Into<u64>>(
    acc: &[A],
    spans: &[Span],
    c: usize,
    x: Axis,
    y: Axis,
    out: &mut Vec<u8>,
) {
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

/// Adds to `row` (sums per byte of the bins) `spans`, short enough for 32-bit sums,
/// over `texels`: `C`-byte texels from texel `base` on, which hold the spans whole.
#[inline]
fn add_spans<const C: usize, A: Sum>(
    spans: &[Span],
    g: u64,
    base: usize,
    texels: &[u8],
    row: &mut [A],
) {
    let (texels, row) = (texels.as_chunks::<C>().0, row.as_chunks_mut::<C>().0);
    let g = A::of(g);
    for (span, sums) in spans.iter().zip(row) {
        let (first, fw) = (texels[span.first - base], A::of(span.first_weight));
        if span.last == span.first {
            for ch in 0..C {
                sums[ch] += fw * A::of(u64::from(first[ch]));
            }
            continue;
        }
        let (last, lw) = (texels[span.last - base], A::of(span.last_weight));
        let mut inner = [0_u32; C];
        for texel in &texels[span.first + 1 - base..span.last - base] {
            for ch in 0..C {
                inner[ch] += u32::from(texel[ch]);
            }
        }
        for ch in 0..C {
            sums[ch] += fw * A::of(u64::from(first[ch]))
                + g * A::of(u64::from(inner[ch]))
                + lw * A::of(u64::from(last[ch]));
        }
    }
}

/// Adds to `row` (sums per byte of the bins) the sums of `texels` (of `C` bytes), `n`
/// to a bin.
#[inline]
fn add_blocks<const C: usize, A: Sum>(n: usize, texels: &[u8], row: &mut [A]) {
    for (block, sums) in texels.chunks_exact(n * C).zip(row.as_chunks_mut::<C>().0) {
        let mut s = [0_u32; C];
        for texel in block.as_chunks::<C>().0 {
            for ch in 0..C {
                s[ch] += u32::from(texel[ch]);
            }
        }
        for ch in 0..C {
            sums[ch] += A::of(u64::from(s[ch]));
        }
    }
}

/// Adds to `sums` (a bin's, per channel) its `span`'s texels among `texels`, which
/// start at texel `base`, weighted across: the inner ones by `g`, a whole texel.
fn add_span<A: Sum>(span: Span, g: u64, base: usize, texels: &[u8], sums: &mut [A]) {
    let c = sums.len();
    let end = base + texels.len() / c;
    let at = |t: usize| &texels[(t - base) * c..(t - base + 1) * c];
    let mut add = |t: usize, w: u64| {
        for (s, &v) in sums.iter_mut().zip(at(t)) {
            *s += A::of(w) * A::of(u64::from(v));
        }
    };
    if (base..end).contains(&span.first) {
        add(span.first, span.first_weight);
    }
    if span.last == span.first {
        return;
    }
    if (base..end).contains(&span.last) {
        add(span.last, span.last_weight);
    }
    for t in (span.first + 1).max(base)..span.last.min(end) {
        add(t, g);
    }
}

/// `s / d` rounded to the nearest integer (halves up), for `s <= 255 d`.
#[inline]
fn div_round(s: u64, d: u64) -> u8 {
    ((s + d / 2) / d) as u8
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
    use crate::fixtures::noise;

    /// The reference: each bin of `window` as its exact weighted mean, from rational
    /// overlaps computed texel by texel over `src`, the texels `t` of a plane `size`
    /// texels large.
    fn reference(
        src: &[u8],
        t: [u32; 4],
        size: (u32, u32),
        c: usize,
        grid: (u32, u32),
        window: [u32; 4],
    ) -> Vec<u8> {
        let (n, m) = (size.0 as u128, size.1 as u128);
        let (g, h) = (grid.0 as u128, grid.1 as u128);
        let overlap =
            |a0: u128, a1: u128, b0: u128, b1: u128| a1.min(b1).saturating_sub(a0.max(b0));
        let width = (t[2] - t[0]) as usize;
        let mut out = Vec::new();
        for j in window[1] as u128..window[3] as u128 {
            for i in window[0] as u128..window[2] as u128 {
                for ch in 0..c {
                    let (mut s, mut w) = (0_u128, 0_u128);
                    for y in t[1]..t[3] {
                        // In units of 1 / (g h) texel area: texel x spans [x g, (x+1) g).
                        let y128 = y as u128;
                        let wy = overlap(y128 * h, (y128 + 1) * h, j * m, (j + 1) * m);
                        for x in t[0]..t[2] {
                            let x128 = x as u128;
                            let wx = overlap(x128 * g, (x128 + 1) * g, i * n, (i + 1) * n);
                            let texel = (y - t[1]) as usize * width + (x - t[0]) as usize;
                            s += wx * wy * src[texel * c + ch] as u128;
                            w += wx * wy;
                        }
                    }
                    out.push(((2 * s + w) / (2 * w)) as u8);
                }
            }
        }
        out
    }

    /// Every grid of small odd and even planes equals the reference, whole and cut
    /// into windows of every size, summed in either order.
    #[test]
    fn bins_are_exact_weighted_means_in_any_window() {
        for across in [false, true] {
            exact_in_any_window(across);
        }
    }

    fn exact_in_any_window(across: bool) {
        let meter = Meter::default();
        let resample = |src: &[u8], t, size, c, grid, window| {
            let area = Area::summed(size, grid, window, c, across, &meter).unwrap();
            resample(area, src, t).unwrap()
        };
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
                    let all = [0, 0, size.0, size.1];
                    let whole = reference(&src, all, size, c, grid, [0, 0, gx, gy]);
                    assert_eq!(
                        resample(&src, all, size, c, grid, [0, 0, gx, gy]),
                        whole,
                        "{size:?} -> {grid:?} across {across}"
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
                            let got = resample(&part, t, size, c, grid, window);
                            let expected: Vec<u8> = (y0..y1)
                                .flat_map(|j| {
                                    let row = (j * gx) as usize * c;
                                    whole[row + x0 as usize * c..row + x1 as usize * c].to_vec()
                                })
                                .collect();
                            assert_eq!(
                                got, expected,
                                "{size:?} -> {grid:?} {window:?} across {across}"
                            );
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
        let meter = Meter::default();
        let mut area = Area::new((2, 4), (1, 2), [0, 0, 1, 2], 1, &meter).unwrap();
        assert_eq!(area.rows(), 0..4);
        area.push(&[100, 100]).unwrap();
        area.push(&[100, 100]).unwrap();
        area.push(&[100, 100]).unwrap();
        let (data, coverage) = area.finish().unwrap();
        assert_eq!(data, [100, 50]);
        assert_eq!(coverage, Some(vec![255, 128]));
        // At full size, missing rows are empty.
        let mut area = Area::new((2, 3), (2, 3), [0, 0, 2, 3], 1, &meter).unwrap();
        area.push(&[7, 8]).unwrap();
        let (data, coverage) = area.finish().unwrap();
        assert_eq!(data, [7, 8, 0, 0, 0, 0]);
        assert_eq!(coverage, Some(vec![255, 0, 0]));
    }

    /// The texels `t` of `src` binned, summed across first when `across`, each row
    /// pushed in parts of `part` texels.
    fn in_parts(
        src: &[u8],
        t: [u32; 4],
        (size, c, grid, window): ((u32, u32), usize, (u32, u32), [u32; 4]),
        across: bool,
        part: usize,
    ) -> Vec<u8> {
        let meter = Meter::default();
        let mut area = Area::summed(size, grid, window, c, across, &meter).unwrap();
        assert_eq!(area.columns(), t[0]..t[2]);
        for row in src.chunks((t[2] - t[0]) as usize * c) {
            for (k, texels) in row.chunks(part * c).enumerate() {
                area.push_texels(t[0] + (k * part) as u32, texels).unwrap();
            }
            area.end_row().unwrap();
        }
        area.finish().unwrap().0
    }

    /// Rows pushed in parts give the bins whole rows give, summed in either order, also
    /// over planes so large that a bin's sum needs 64 or 128 bits.
    #[test]
    fn rows_pushed_in_parts_give_the_same_bins() {
        for (size, c, grid, window) in [
            ((13, 2), 4, (5, 1), [0, 0, 5, 1]),
            ((13, 5), 3, (13, 2), [2, 0, 9, 2]),
            ((40, 3), 1, (3, 3), [0, 0, 3, 3]),
            ((40, 3), 2, (7, 2), [1, 1, 6, 2]),
            ((40, 3), 1, (5, 3), [0, 0, 5, 3]),
            ((24, 4), 3, (6, 2), [1, 0, 5, 2]),
            ((12, 3), 4, (4, 1), [0, 0, 4, 1]),
            ((100_003, 100_019), 3, (100_002, 100_018), [7, 5, 12, 8]),
            (
                (1_000_000_007, 1_000_000_009),
                4,
                (1_000_000_000, 999_999_999),
                [9, 4, 15, 7],
            ),
        ] {
            let t = area_texels(size, grid, window);
            let len = ((t[2] - t[0]) * (t[3] - t[1])) as usize * c;
            let src = noise(len, size.0 + c as u32);
            let expected = reference(&src, t, size, c, grid, window);
            for (across, part) in [false, true]
                .into_iter()
                .flat_map(|a| [1, 2, 3, 64, usize::MAX / 8].map(|p| (a, p)))
            {
                let got = in_parts(&src, t, (size, c, grid, window), across, part);
                assert_eq!(
                    got, expected,
                    "{size:?} -> {grid:?} {window:?} by {part} across {across}"
                );
            }
        }
    }

    /// The rest of a row pushed as one texel gives the bins that pushing that texel
    /// in every column gives, at the plane's size and summed in either order.
    #[test]
    fn the_rest_of_a_row_adds_as_its_texels() {
        let size = (41, 4);
        let src = noise(41 * 4 * 3, 12);
        for (grid, window) in [
            ((41, 4), [3, 1, 30, 4]),
            ((20, 2), [0, 0, 20, 2]),
            ((7, 3), [2, 0, 6, 3]),
            ((2, 3), [1, 1, 2, 3]),
        ] {
            for (across, texel) in [false, true]
                .into_iter()
                .flat_map(|a| [[0, 0, 0], [255, 7, 0]].map(|t| (a, t)))
            {
                let t = area_texels(size, grid, window);
                let rows = |area: &mut Area<'_>| {
                    for y in t[1]..t[3] - 1 {
                        let at = (y * 41) as usize * 3;
                        area.push(&src[at + t[0] as usize * 3..at + t[2] as usize * 3])
                            .unwrap();
                    }
                    let at = ((t[3] - 1) * 41 + t[0]) as usize * 3;
                    area.push_texels(t[0], &src[at..at + 3]).unwrap();
                };
                let meter = Meter::default();
                let mut area = Area::summed(size, grid, window, 3, across, &meter).unwrap();
                rows(&mut area);
                area.push_rest(t[0] + 1, &texel).unwrap();
                area.end_row().unwrap();
                let mut every = Area::summed(size, grid, window, 3, across, &meter).unwrap();
                rows(&mut every);
                for col in t[0] + 1..t[2] {
                    every.push_texels(col, &texel).unwrap();
                }
                every.end_row().unwrap();
                assert_eq!(
                    area.finish().unwrap(),
                    every.finish().unwrap(),
                    "{grid:?} {window:?} {texel:?} across {across}"
                );
            }
        }
    }
}
