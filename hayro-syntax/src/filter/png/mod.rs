//! Taken from the png crate, see <https://github.com/image-rs/image-png/blob/master/src/filter/mod.rs>

use core::convert::TryInto;
use core::ops::Range;

mod paeth;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub(crate) enum BytesPerPixel {
    One = 1,
    Two = 2,
    Three = 3,
    Four = 4,
    Six = 6,
    Eight = 8,
}

// Note: This impl is not from the png crate.
impl BytesPerPixel {
    /// The pixel size the unrolled code handles, where it is one.
    pub(crate) fn of(bpp: usize) -> Option<Self> {
        match bpp {
            1 => Some(Self::One),
            2 => Some(Self::Two),
            3 => Some(Self::Three),
            4 => Some(Self::Four),
            6 => Some(Self::Six),
            8 => Some(Self::Eight),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum RowFilter {
    NoFilter = 0,
    Sub = 1,
    Up = 2,
    Avg = 3,
    Paeth = 4,
}

impl RowFilter {
    pub(crate) fn from_u8(n: u8) -> Option<Self> {
        match n {
            0 => Some(Self::NoFilter),
            1 => Some(Self::Sub),
            2 => Some(Self::Up),
            3 => Some(Self::Avg),
            4 => Some(Self::Paeth),
            _ => None,
        }
    }
}

// Note: This function is not from the png crate.
/// Unfilters bytes `part` of `current`, whose bytes before it are unfiltered, as
/// [`unfilter`] does for the whole row: a row is unfiltered in parts as its bytes
/// arrive. `part` starts at a pixel; it may end inside one only where the row does.
pub(crate) fn unfilter_part(
    filter: RowFilter,
    bpp: usize,
    previous: &[u8],
    current: &mut [u8],
    part: Range<usize>,
) {
    let Some(tbpp) = BytesPerPixel::of(bpp) else {
        unfilter_bytes(filter, bpp, previous, current, part);
        return;
    };
    let from = part.start.saturating_sub(bpp);
    let above = previous.get(from..part.end).unwrap_or(&[]);
    if part.start == 0 || filter == RowFilter::NoFilter {
        unfilter(filter, bpp, above, &mut current[part]);
        return;
    }
    // `unfilter` takes the pixels left and above-left of its first as zero. The pixel
    // before the part is filtered again against those zeros, so that unfiltering it
    // with the part gives it back and the part sees its true neighbours.
    let mut left = [0; 8];
    left[..bpp].copy_from_slice(&current[from..part.start]);
    for (i, v) in current[from..part.start].iter_mut().enumerate() {
        let b = above.get(i).copied().unwrap_or(0);
        *v = v.wrapping_sub(match filter {
            RowFilter::Up | RowFilter::Paeth => b,
            RowFilter::Avg => b / 2,
            RowFilter::NoFilter | RowFilter::Sub => 0,
        });
    }
    unfilter_pixels(filter, tbpp, above, &mut current[from..part.end]);
    debug_assert_eq!(current[from..part.start], left[..bpp]);
}

// Note: This function is not from the png crate.
/// Unfilters `current`, a row of pixels of `bpp` bytes, or its start, under the row
/// `previous` (empty for the first row). The last pixel may be cut short, in a row
/// whose length is no whole number of pixels or in a last row cut short: each of its
/// bytes is unfiltered from the bytes left of and above it, as those of a whole one.
pub(crate) fn unfilter(filter: RowFilter, bpp: usize, previous: &[u8], current: &mut [u8]) {
    match BytesPerPixel::of(bpp) {
        Some(tbpp) => unfilter_pixels(filter, tbpp, previous, current),
        None => unfilter_bytes(filter, bpp, previous, current, 0..current.len()),
    }
}

/// [`unfilter`] for the pixel sizes the unrolled code handles.
fn unfilter_pixels(filter: RowFilter, tbpp: BytesPerPixel, previous: &[u8], current: &mut [u8]) {
    let bpp = tbpp as usize;
    let whole = current.len() / bpp * bpp;
    unfilter_whole(filter, tbpp, previous, &mut current[..whole]);
    unfilter_bytes(filter, bpp, previous, current, whole..current.len());
}

// Note: This function is not from the png crate.
/// Unfilters bytes `range` of `current` one at a time, the bytes before them
/// unfiltered; a byte of `previous` past its end is zero.
fn unfilter_bytes(
    filter: RowFilter,
    bpp: usize,
    previous: &[u8],
    current: &mut [u8],
    range: Range<usize>,
) {
    if filter == RowFilter::NoFilter {
        return;
    }
    for i in range {
        let left = i.checked_sub(bpp);
        let a = left.map_or(0, |j| current[j]);
        let b = previous.get(i).copied().unwrap_or(0);
        let c = left.and_then(|j| previous.get(j)).copied().unwrap_or(0);
        current[i] = current[i].wrapping_add(match filter {
            RowFilter::NoFilter => 0,
            RowFilter::Sub => a,
            RowFilter::Up => b,
            RowFilter::Avg => ((u16::from(a) + u16::from(b)) / 2) as u8,
            RowFilter::Paeth => paeth::filter_paeth(a, b, c),
        });
    }
}

fn unfilter_whole(mut filter: RowFilter, tbpp: BytesPerPixel, previous: &[u8], current: &mut [u8]) {
    use self::RowFilter::*;

    // If the previous row is empty, then treat it as if it were filled with zeros.
    if previous.is_empty() {
        if filter == Paeth {
            filter = Sub;
        } else if filter == Up {
            filter = NoFilter;
        }
    }

    match filter {
        NoFilter => {}
        Sub => match tbpp {
            BytesPerPixel::One => {
                current.iter_mut().reduce(|&mut prev, curr| {
                    *curr = curr.wrapping_add(prev);
                    curr
                });
            }
            BytesPerPixel::Two => {
                let mut prev = [0; 2];
                for chunk in current.chunks_exact_mut(2) {
                    let new_chunk = [
                        chunk[0].wrapping_add(prev[0]),
                        chunk[1].wrapping_add(prev[1]),
                    ];
                    *TryInto::<&mut [u8; 2]>::try_into(chunk).unwrap() = new_chunk;
                    prev = new_chunk;
                }
            }
            BytesPerPixel::Three => {
                let mut prev = [0; 3];
                for chunk in current.chunks_exact_mut(3) {
                    let new_chunk = [
                        chunk[0].wrapping_add(prev[0]),
                        chunk[1].wrapping_add(prev[1]),
                        chunk[2].wrapping_add(prev[2]),
                    ];
                    *TryInto::<&mut [u8; 3]>::try_into(chunk).unwrap() = new_chunk;
                    prev = new_chunk;
                }
            }
            BytesPerPixel::Four => {
                let mut prev = [0; 4];
                for chunk in current.chunks_exact_mut(4) {
                    let new_chunk = [
                        chunk[0].wrapping_add(prev[0]),
                        chunk[1].wrapping_add(prev[1]),
                        chunk[2].wrapping_add(prev[2]),
                        chunk[3].wrapping_add(prev[3]),
                    ];
                    *TryInto::<&mut [u8; 4]>::try_into(chunk).unwrap() = new_chunk;
                    prev = new_chunk;
                }
            }
            BytesPerPixel::Six => {
                let mut prev = [0; 6];
                for chunk in current.chunks_exact_mut(6) {
                    let new_chunk = [
                        chunk[0].wrapping_add(prev[0]),
                        chunk[1].wrapping_add(prev[1]),
                        chunk[2].wrapping_add(prev[2]),
                        chunk[3].wrapping_add(prev[3]),
                        chunk[4].wrapping_add(prev[4]),
                        chunk[5].wrapping_add(prev[5]),
                    ];
                    *TryInto::<&mut [u8; 6]>::try_into(chunk).unwrap() = new_chunk;
                    prev = new_chunk;
                }
            }
            BytesPerPixel::Eight => {
                let mut prev = [0; 8];
                for chunk in current.chunks_exact_mut(8) {
                    let new_chunk = [
                        chunk[0].wrapping_add(prev[0]),
                        chunk[1].wrapping_add(prev[1]),
                        chunk[2].wrapping_add(prev[2]),
                        chunk[3].wrapping_add(prev[3]),
                        chunk[4].wrapping_add(prev[4]),
                        chunk[5].wrapping_add(prev[5]),
                        chunk[6].wrapping_add(prev[6]),
                        chunk[7].wrapping_add(prev[7]),
                    ];
                    *TryInto::<&mut [u8; 8]>::try_into(chunk).unwrap() = new_chunk;
                    prev = new_chunk;
                }
            }
        },
        Up => {
            for (curr, &above) in current.iter_mut().zip(previous) {
                *curr = curr.wrapping_add(above);
            }
        }
        Avg if previous.is_empty() => match tbpp {
            BytesPerPixel::One => {
                current.iter_mut().reduce(|&mut prev, curr| {
                    *curr = curr.wrapping_add(prev / 2);
                    curr
                });
            }
            BytesPerPixel::Two => {
                let mut prev = [0; 2];
                for chunk in current.chunks_exact_mut(2) {
                    let new_chunk = [
                        chunk[0].wrapping_add(prev[0] / 2),
                        chunk[1].wrapping_add(prev[1] / 2),
                    ];
                    *TryInto::<&mut [u8; 2]>::try_into(chunk).unwrap() = new_chunk;
                    prev = new_chunk;
                }
            }
            BytesPerPixel::Three => {
                let mut prev = [0; 3];
                for chunk in current.chunks_exact_mut(3) {
                    let new_chunk = [
                        chunk[0].wrapping_add(prev[0] / 2),
                        chunk[1].wrapping_add(prev[1] / 2),
                        chunk[2].wrapping_add(prev[2] / 2),
                    ];
                    *TryInto::<&mut [u8; 3]>::try_into(chunk).unwrap() = new_chunk;
                    prev = new_chunk;
                }
            }
            BytesPerPixel::Four => {
                let mut prev = [0; 4];
                for chunk in current.chunks_exact_mut(4) {
                    let new_chunk = [
                        chunk[0].wrapping_add(prev[0] / 2),
                        chunk[1].wrapping_add(prev[1] / 2),
                        chunk[2].wrapping_add(prev[2] / 2),
                        chunk[3].wrapping_add(prev[3] / 2),
                    ];
                    *TryInto::<&mut [u8; 4]>::try_into(chunk).unwrap() = new_chunk;
                    prev = new_chunk;
                }
            }
            BytesPerPixel::Six => {
                let mut prev = [0; 6];
                for chunk in current.chunks_exact_mut(6) {
                    let new_chunk = [
                        chunk[0].wrapping_add(prev[0] / 2),
                        chunk[1].wrapping_add(prev[1] / 2),
                        chunk[2].wrapping_add(prev[2] / 2),
                        chunk[3].wrapping_add(prev[3] / 2),
                        chunk[4].wrapping_add(prev[4] / 2),
                        chunk[5].wrapping_add(prev[5] / 2),
                    ];
                    *TryInto::<&mut [u8; 6]>::try_into(chunk).unwrap() = new_chunk;
                    prev = new_chunk;
                }
            }
            BytesPerPixel::Eight => {
                let mut prev = [0; 8];
                for chunk in current.chunks_exact_mut(8) {
                    let new_chunk = [
                        chunk[0].wrapping_add(prev[0] / 2),
                        chunk[1].wrapping_add(prev[1] / 2),
                        chunk[2].wrapping_add(prev[2] / 2),
                        chunk[3].wrapping_add(prev[3] / 2),
                        chunk[4].wrapping_add(prev[4] / 2),
                        chunk[5].wrapping_add(prev[5] / 2),
                        chunk[6].wrapping_add(prev[6] / 2),
                        chunk[7].wrapping_add(prev[7] / 2),
                    ];
                    *TryInto::<&mut [u8; 8]>::try_into(chunk).unwrap() = new_chunk;
                    prev = new_chunk;
                }
            }
        },
        Avg => match tbpp {
            BytesPerPixel::One => {
                let mut lprev = [0; 1];
                for (chunk, above) in current.chunks_exact_mut(1).zip(previous.chunks_exact(1)) {
                    let new_chunk =
                        [chunk[0].wrapping_add(((above[0] as u16 + lprev[0] as u16) / 2) as u8)];
                    *TryInto::<&mut [u8; 1]>::try_into(chunk).unwrap() = new_chunk;
                    lprev = new_chunk;
                }
            }
            BytesPerPixel::Two => {
                let mut lprev = [0; 2];
                for (chunk, above) in current.chunks_exact_mut(2).zip(previous.chunks_exact(2)) {
                    let new_chunk = [
                        chunk[0].wrapping_add(((above[0] as u16 + lprev[0] as u16) / 2) as u8),
                        chunk[1].wrapping_add(((above[1] as u16 + lprev[1] as u16) / 2) as u8),
                    ];
                    *TryInto::<&mut [u8; 2]>::try_into(chunk).unwrap() = new_chunk;
                    lprev = new_chunk;
                }
            }
            BytesPerPixel::Three => {
                let mut lprev = [0; 3];
                for (chunk, above) in current.chunks_exact_mut(3).zip(previous.chunks_exact(3)) {
                    let new_chunk = [
                        chunk[0].wrapping_add(((above[0] as u16 + lprev[0] as u16) / 2) as u8),
                        chunk[1].wrapping_add(((above[1] as u16 + lprev[1] as u16) / 2) as u8),
                        chunk[2].wrapping_add(((above[2] as u16 + lprev[2] as u16) / 2) as u8),
                    ];
                    *TryInto::<&mut [u8; 3]>::try_into(chunk).unwrap() = new_chunk;
                    lprev = new_chunk;
                }
            }
            BytesPerPixel::Four => {
                let mut lprev = [0; 4];
                for (chunk, above) in current.chunks_exact_mut(4).zip(previous.chunks_exact(4)) {
                    let new_chunk = [
                        chunk[0].wrapping_add(((above[0] as u16 + lprev[0] as u16) / 2) as u8),
                        chunk[1].wrapping_add(((above[1] as u16 + lprev[1] as u16) / 2) as u8),
                        chunk[2].wrapping_add(((above[2] as u16 + lprev[2] as u16) / 2) as u8),
                        chunk[3].wrapping_add(((above[3] as u16 + lprev[3] as u16) / 2) as u8),
                    ];
                    *TryInto::<&mut [u8; 4]>::try_into(chunk).unwrap() = new_chunk;
                    lprev = new_chunk;
                }
            }
            BytesPerPixel::Six => {
                let mut lprev = [0; 6];
                for (chunk, above) in current.chunks_exact_mut(6).zip(previous.chunks_exact(6)) {
                    let new_chunk = [
                        chunk[0].wrapping_add(((above[0] as u16 + lprev[0] as u16) / 2) as u8),
                        chunk[1].wrapping_add(((above[1] as u16 + lprev[1] as u16) / 2) as u8),
                        chunk[2].wrapping_add(((above[2] as u16 + lprev[2] as u16) / 2) as u8),
                        chunk[3].wrapping_add(((above[3] as u16 + lprev[3] as u16) / 2) as u8),
                        chunk[4].wrapping_add(((above[4] as u16 + lprev[4] as u16) / 2) as u8),
                        chunk[5].wrapping_add(((above[5] as u16 + lprev[5] as u16) / 2) as u8),
                    ];
                    *TryInto::<&mut [u8; 6]>::try_into(chunk).unwrap() = new_chunk;
                    lprev = new_chunk;
                }
            }
            BytesPerPixel::Eight => {
                let mut lprev = [0; 8];
                for (chunk, above) in current.chunks_exact_mut(8).zip(previous.chunks_exact(8)) {
                    let new_chunk = [
                        chunk[0].wrapping_add(((above[0] as u16 + lprev[0] as u16) / 2) as u8),
                        chunk[1].wrapping_add(((above[1] as u16 + lprev[1] as u16) / 2) as u8),
                        chunk[2].wrapping_add(((above[2] as u16 + lprev[2] as u16) / 2) as u8),
                        chunk[3].wrapping_add(((above[3] as u16 + lprev[3] as u16) / 2) as u8),
                        chunk[4].wrapping_add(((above[4] as u16 + lprev[4] as u16) / 2) as u8),
                        chunk[5].wrapping_add(((above[5] as u16 + lprev[5] as u16) / 2) as u8),
                        chunk[6].wrapping_add(((above[6] as u16 + lprev[6] as u16) / 2) as u8),
                        chunk[7].wrapping_add(((above[7] as u16 + lprev[7] as u16) / 2) as u8),
                    ];
                    *TryInto::<&mut [u8; 8]>::try_into(chunk).unwrap() = new_chunk;
                    lprev = new_chunk;
                }
            }
        },
        Paeth => paeth::unfilter(tbpp, previous, current),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// A row unfiltered in parts, split at every pixel boundary and at several at
    /// once, comes out as unfiltering it whole does, and as unfiltering it a byte at a
    /// time does: for every filter and pixel size, with a previous row and without,
    /// and with a last pixel cut short.
    #[test]
    fn a_row_unfiltered_in_parts_matches_it_whole() {
        let bytes = |n: usize, seed: u32| -> Vec<u8> {
            let mut x = seed.wrapping_mul(2_654_435_761) | 1;
            (0..n)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    (x >> 24) as u8
                })
                .collect()
        };
        for bpp in [1, 2, 3, 4, 5, 6, 7, 8, 12, 64] {
            for len in [9 * bpp, 9 * bpp - 1, 8 * bpp + 1] {
                for filter in 0..5 {
                    let filter = RowFilter::from_u8(filter).unwrap();
                    for previous in [Vec::new(), bytes(9 * bpp, 7)] {
                        let encoded = bytes(len, bpp as u32 + 11);
                        let mut whole = encoded.clone();
                        unfilter(filter, bpp, &previous, &mut whole);
                        let mut bytewise = encoded.clone();
                        unfilter_bytes(filter, bpp, &previous, &mut bytewise, 0..len);
                        assert_eq!(whole, bytewise, "{filter:?} bpp {bpp} len {len}");
                        let splits = (1..9).map(|k| alloc::vec![k * bpp]).chain([
                            alloc::vec![bpp, 2 * bpp, 5 * bpp],
                            (1..9).map(|k| k * bpp).collect(),
                        ]);
                        for split in splits {
                            let mut parts = encoded.clone();
                            let mut start = 0;
                            for end in split.into_iter().filter(|&e| e < len).chain([len]) {
                                unfilter_part(filter, bpp, &previous, &mut parts, start..end);
                                start = end;
                            }
                            assert_eq!(parts, whole, "{filter:?} bpp {bpp} len {len}");
                        }
                    }
                }
            }
        }
    }
}
