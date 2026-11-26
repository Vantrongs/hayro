use crate::filter::png::{self, RowFilter};
use crate::object::dict::keys::{BITS_PER_COMPONENT, COLORS, COLUMNS, EARLY_CHANGE, PREDICTOR};
use crate::object::{Dict, Number};
use alloc::vec::Vec;

pub(super) struct Rows {
    pub(super) tiff: bool,
    pub(super) colors: usize,
    pub(super) bpc: usize,
    pub(super) columns: usize,
    pub(super) width: usize,
}

fn integer(params: &Dict<'_>, key: &[u8], default: i32) -> Option<i32> {
    let Some(number) = params.get::<Number>(key) else {
        return Some(default);
    };
    if number.is_integer() {
        return Some(number.as_i64() as i32);
    }
    let rounded = number.viewer_f32()? + 0.5;
    Some(if rounded >= 2_147_483_648.0 {
        i32::MIN
    } else {
        rounded as i32
    })
}

pub(super) fn early_change(params: &Dict<'_>) -> bool {
    integer(params, EARLY_CHANGE, 1) != Some(0)
}

impl Rows {
    pub(super) fn read(params: &Dict<'_>) -> Result<Option<Self>, ()> {
        let method = integer(params, PREDICTOR, 1).ok_or(())?;
        if !matches!(method, 2 | 10..=15) {
            return Ok(None);
        }
        let positive = |key, default| {
            integer(params, key, default)
                .map(|value| if value < 1 { default } else { value } as usize)
                .ok_or(())
        };
        let colors = positive(COLORS, 1)?;
        let bpc = positive(BITS_PER_COMPONENT, 8)?;
        let columns = positive(COLUMNS, 1)?;
        if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
            return Err(());
        }
        let width = colors
            .checked_mul(bpc)
            .and_then(|bits| bits.checked_mul(columns))
            .ok_or(())?
            .div_ceil(8);
        Ok(Some(Self {
            tiff: method == 2,
            colors,
            bpc,
            columns,
            width,
        }))
    }

    pub(super) fn bytes_per_pixel(&self) -> usize {
        (self.colors * self.bpc).div_ceil(8)
    }
}

pub(crate) fn apply(data: Vec<u8>, params: &Dict<'_>) -> Option<Vec<u8>> {
    let Some(rows) = Rows::read(params).ok()? else {
        return Some(data);
    };
    Some(if rows.tiff {
        tiff(data, rows.colors, rows.bpc, rows.columns, rows.width)
    } else {
        png(data, rows.bytes_per_pixel(), rows.width)
    })
}

fn tiff(
    mut data: Vec<u8>,
    colors: usize,
    bits_per_component: usize,
    columns: usize,
    row_len: usize,
) -> Vec<u8> {
    for row in data.chunks_mut(row_len) {
        tiff_row(row, colors, bits_per_component, columns);
    }
    data
}

pub(super) fn tiff_row(row: &mut [u8], colors: usize, bits_per_component: usize, columns: usize) {
    match bits_per_component {
        8 => png::unfilter_row(RowFilter::Sub, colors, &[], row),
        16 => {
            let distance = 2 * colors;

            for i in (distance..row.len() - 1).step_by(2) {
                let left = u16::from_be_bytes([row[i - distance], row[i - distance + 1]]);
                let value = u16::from_be_bytes([row[i], row[i + 1]]).wrapping_add(left);
                row[i..i + 2].copy_from_slice(&value.to_be_bytes());
            }
        }
        _ => {
            let per_byte = 8 / bits_per_component;
            let mask = (1_u8 << bits_per_component) - 1;
            let shift = |k: usize| 8 - bits_per_component * (k % per_byte + 1);
            let num_components = (columns * colors).min(row.len() * per_byte);

            for k in colors..num_components {
                let left = row[(k - colors) / per_byte] >> shift(k - colors);
                let byte = &mut row[k / per_byte];
                let value = (*byte >> shift(k)).wrapping_add(left) & mask;
                *byte = (*byte & !(mask << shift(k))) | (value << shift(k));
            }
            let used = num_components * bits_per_component % 8;
            if used != 0 {
                *row.last_mut().unwrap() &= 0xff << (8 - used);
            }
        }
    }
}

fn png(mut data: Vec<u8>, bytes_per_pixel: usize, row_len: usize) -> Vec<u8> {
    let mut src = 0;
    let mut dst = 0;

    while src < data.len() {
        let filter = data[src];
        let len = row_len.min(data.len() - src - 1);
        data.copy_within(src + 1..src + 1 + len, dst);

        let (done, rest) = data.split_at_mut(dst);
        let previous = &done[done.len().saturating_sub(row_len)..];

        if let Some(filter) = RowFilter::from_u8(filter) {
            png::unfilter_row(filter, bytes_per_pixel, previous, &mut rest[..len]);
        }

        src += row_len + 1;
        dst += len;
    }

    data.truncate(dst);

    data
}
