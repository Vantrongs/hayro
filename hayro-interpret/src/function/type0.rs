use crate::encode::Cost;

#[cfg(test)]
thread_local! {
    /// The evaluations run.
    pub(crate) static EVALS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
use crate::function::shape::{Piece, clipped};
use crate::function::{Clamper, TupleVec, Values, interpolate};
use hayro_syntax::bit_reader::BitReader;
use hayro_syntax::object::Array;
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::{BITS_PER_SAMPLE, DECODE, ENCODE, SIZE};
use smallvec::{SmallVec, smallvec};

/// A type 0 function (sampled function).
#[derive(Debug)]
pub(crate) struct Type0 {
    sizes: IntVec,
    strides: Vec<usize>,
    table: Vec<u32>,
    pub(super) clamper: Clamper,
    range: TupleVec,
    bits_per_sample: u8,
    encode: TupleVec,
    decode: TupleVec,
}

impl Type0 {
    /// Create a new type 0 function.
    pub(crate) fn new(stream: &Stream<'_>) -> Option<Self> {
        let dict = stream.dict();
        let bits_per_sample = dict.get::<u8>(BITS_PER_SAMPLE)?;

        if !matches!(bits_per_sample, 1 | 2 | 4 | 8 | 16 | 24 | 32) {
            error!("invalid bits per sample: {bits_per_sample}");

            return None;
        }

        let clamper = Clamper::new(dict)?;
        let range = clamper.range.clone()?;

        if range.is_empty() {
            warn!("encountered Type0 function with invalid range length 0.");

            return None;
        }

        let sizes = dict
            .get::<Array<'_>>(SIZE)?
            .items::<u32>()
            .map_while(|n| n)
            .collect::<IntVec>();

        let encode = dict
            .get::<TupleVec>(ENCODE)
            .unwrap_or(sizes.iter().map(|s| (0.0, (*s - 1) as f32)).collect());

        let decode = dict.get::<TupleVec>(DECODE).unwrap_or(range.clone());

        let mut data = {
            let decoded = stream.decoded().ok()?;
            let mut buf = vec![];
            let mut reader = BitReader::new(&decoded);

            while let Some(data) = reader.read(bits_per_sample) {
                buf.push(data);
            }

            buf
        };

        let mut stride = range.len();
        let mut strides = Vec::with_capacity(sizes.len());
        for size in &sizes {
            strides.push(stride);
            stride = stride.checked_mul(*size as usize)?;
        }
        let num_expected_entries = stride;

        if data.len() != num_expected_entries {
            warn!("Type0 function didn't have the expected number of sample entries.");
            data.truncate(num_expected_entries);
        }

        Some(Self {
            sizes,
            strides,
            clamper,
            range,
            bits_per_sample,
            table: data,
            encode,
            decode,
        })
    }

    /// The number of samples along each input.
    pub(crate) fn sizes(&self) -> &[u32] {
        &self.sizes
    }

    /// The number of outputs.
    pub(crate) fn outputs(&self) -> u64 {
        self.range.len() as u64
    }

    /// The most steps `eval` takes: interpolating between the samples around a
    /// point reads 2^m of them for m inputs, each with all its outputs.
    pub(crate) fn cost(&self) -> Cost {
        let (inputs, outputs) = (self.sizes.len(), self.range.len());
        // As many as a `u64` holds, if more.
        let corners = u32::try_from(inputs)
            .ok()
            .and_then(|inputs| 1_u64.checked_shl(inputs))
            .unwrap_or(u64::MAX);
        (Cost::of(outputs) + Cost::new(2)).times(corners)
            + Cost::of(inputs).times(4)
            + Cost::of(outputs).times(2)
            + self.clamper.cost()
    }

    /// Evaluate a type 0 function with the given input.
    pub(crate) fn eval(&self, input: Values) -> Option<Values> {
        #[cfg(test)]
        EVALS.set(EVALS.get() + 1);
        let mut out = self.decoded(input)?;
        self.clamper.clamp_output(&mut out);
        Some(out)
    }

    /// The function of one input as straight lines between its samples, as
    /// `eval` interpolates them, whatever `/Order` says: a point where the input
    /// encodes to each sample, and one at each side of its domain, clipped to its
    /// range with a point where a line crosses a side of it. A function of more
    /// inputs, or without its samples, is a curve.
    pub(crate) fn shape(&self) -> Piece {
        let curve = || {
            let (d0, d1) = self.clamper.domain.first().copied().unwrap_or((0.0, 1.0));
            Piece::Curve(d0, d1)
        };
        let (Some(&(d0, d1)), Some(&(e0, e1)), &[n]) = (
            self.clamper.domain.first(),
            self.encode.first(),
            self.sizes.as_slice(),
        ) else {
            return curve();
        };
        let m = self.range.len();
        let most = (2_u64.pow(u32::from(self.bits_per_sample)) - 1) as f32;
        let sample = |k: u32| -> Option<Values> {
            let at = (k as usize).checked_mul(m)?;
            let samples = self.table.get(at..at.checked_add(m)?)?;
            Some(
                samples
                    .iter()
                    .zip(self.decode.iter())
                    .map(|(x, decode)| interpolate(*x as f32, 0.0, most, decode.0, decode.1))
                    .collect(),
            )
        };
        let (Some(start), Some(end)) = (self.decoded(smallvec![d0]), self.decoded(smallvec![d1]))
        else {
            return curve();
        };
        if n == 0 || d0 > d1 {
            return curve();
        }
        let last = (n - 1) as f32;
        let key = |x: f32| interpolate(x, d0, d1, e0, e1).max(0.0).min(last);
        let (k0, k1) = (key(d0), key(d1));
        let mut points = vec![(d0, start)];
        if d0 < d1 && e0 != e1 {
            // The samples strictly between the keys of the domain's sides, in
            // the order of the input.
            let (lo, hi) = (k0.min(k1), k0.max(k1));
            let first = lo.floor() as u32 + 1;
            let last = hi.ceil() as u32;
            let mut inner = Vec::with_capacity((last.saturating_sub(first)) as usize);
            for k in first..last {
                let x = interpolate(k as f32, e0, e1, d0, d1);
                match sample(k) {
                    Some(value) if x > d0 && x < d1 => inner.push((x, value)),
                    Some(_) => {}
                    None => return curve(),
                }
            }
            if k1 < k0 {
                inner.reverse();
            }
            points.extend(inner);
        }
        points.push((d1, end));
        Piece::Lines(clipped(points, Some(&self.range)))
    }

    /// The samples interpolated at `input` and decoded, not yet clipped to the
    /// range.
    fn decoded(&self, mut input: Values) -> Option<Values> {
        if input.len() != self.sizes.len() {
            warn!("wrong number of arguments for sampled function");

            return None;
        }

        self.clamper.clamp_input(&mut input);

        let mut key = input;

        for (((x, domain), encode), size) in key
            .iter_mut()
            .zip(self.clamper.domain.iter())
            .zip(self.encode.iter())
            .zip(self.sizes.iter())
        {
            *x = interpolate(*x, domain.0, domain.1, encode.0, encode.1);
            *x = x.max(0.0).min(*size as f32 - 1.0);
        }

        let in_prev = key.iter().map(|v| v.floor() as u32).collect::<IntVec>();
        let in_next = key.iter().map(|v| v.ceil() as u32).collect::<IntVec>();

        let interpolator =
            Interpolator::new(&key, in_prev, in_next, &self.strides, self.range.len());

        let interpolated = interpolator.interpolate(&self.table)?;

        let out = interpolated
            .iter()
            .zip(self.decode.iter())
            .map(|(x, decode)| {
                interpolate(
                    *x,
                    0.0,
                    (2_u32.pow(self.bits_per_sample as u32) - 1) as f32,
                    decode.0,
                    decode.1,
                )
            })
            .collect::<SmallVec<_>>();

        Some(out)
    }
}

type FloatVec = SmallVec<[f32; 4]>;
type IntVec = SmallVec<[u32; 4]>;

// See <https://github.com/apache/pdfbox/blob/bb778d4784f354c36ce032e91a0cee2169a4c598/pdfbox/src/main/java/org/apache/pdfbox/pdmodel/common/function/PDFunctionType0.java#L252>
struct Interpolator<'a> {
    input: &'a [f32],
    strides: &'a [usize],
    in_prev: IntVec,
    in_next: IntVec,
    out_len: usize,
}

impl<'a> Interpolator<'a> {
    fn new(
        input: &'a [f32],
        in_prev: IntVec,
        in_next: IntVec,
        strides: &'a [usize],
        out_len: usize,
    ) -> Self {
        Self {
            input,
            in_prev,
            in_next,
            strides,
            out_len,
        }
    }

    fn interpolate(&self, table: &[u32]) -> Option<FloatVec> {
        let mut out = smallvec![0.0; self.out_len];
        self.interpolate_inner(0, 0, 1.0, table, &mut out)?;
        Some(out)
    }

    fn interpolate_inner(
        &self,
        step: usize,
        offset: usize,
        weight: f32,
        table: &[u32],
        out: &mut [f32],
    ) -> Option<()> {
        if step == self.input.len() {
            let sample = table.get(offset..offset + self.out_len)?;
            for (out, sample) in out.iter_mut().zip(sample) {
                *out += weight * *sample as f32;
            }
            return Some(());
        }

        let prev = self.in_prev[step];
        let next = self.in_next[step];
        let stride = self.strides[step];

        if prev == next {
            self.interpolate_inner(
                step + 1,
                offset + prev as usize * stride,
                weight,
                table,
                out,
            )
        } else {
            let next_weight = self.input[step] - prev as f32;
            self.interpolate_inner(
                step + 1,
                offset + prev as usize * stride,
                weight * (1.0 - next_weight),
                table,
                out,
            )?;
            self.interpolate_inner(
                step + 1,
                offset + next as usize * stride,
                weight * next_weight,
                table,
                out,
            )
        }
    }
}
