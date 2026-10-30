use crate::encode::Cost;

#[cfg(test)]
thread_local! {
    /// The evaluations run.
    pub(crate) static EVALS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
use crate::function::shape::{Line, Side, Span};
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
        if let ([x], [_]) = (input.as_slice(), self.sizes.as_slice()) {
            // Of one input, the value at it of its line there, as its shape is.
            let (d0, d1) = *self.clamper.domain.first()?;
            let x = x.min(d1).max(d0);
            return Some(self.line(x, Side::At)?.at(x));
        }
        let mut out = self.decoded(input)?;
        self.clamper.clamp_output(&mut out);
        Some(out)
    }

    /// The function of one input on `side` of `x`, within its domain, as
    /// `eval` gives it ([`Function::span`](crate::Function)): straight between
    /// two samples, whatever `/Order` says, as it interpolates them; flat where
    /// the input encodes past the first or the last sample, and is clipped to
    /// it; its values clipped to its range ([`Line::clipped`]). A curve where it
    /// has more inputs or lacks samples.
    pub(crate) fn span(&self, x: f32, side: Side) -> Span {
        match self.line(x, side) {
            Some(line) => Span::Line(line),
            None => {
                let (d0, d1) = self.clamper.domain.first().copied().unwrap_or((0.0, 1.0));
                Span::Curve(d0, d1)
            }
        }
    }

    /// The line of a function of one input on `side` of `x`, within its domain,
    /// `x` below its end where `side` is above and above its start where `side`
    /// is below: the encoded input's cell of the samples, the first or the last
    /// sample alone past them, put where it maps from and cut to the domain, its
    /// values clipped to the range. `None` for more inputs, or a sample missing.
    fn line(&self, x: f32, side: Side) -> Option<Line> {
        let (Some(&(d0, d1)), Some(&(e0, e1)), &[n]) = (
            self.clamper.domain.first(),
            self.encode.first(),
            self.sizes.as_slice(),
        ) else {
            return None;
        };
        if n == 0 {
            return None;
        }
        let m = self.range.len();
        let most = (2_u64.pow(u32::from(self.bits_per_sample)) - 1) as f32;
        let sample = |k: f32| -> Option<Values> {
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
        let last = (n - 1) as f32;
        let range = Some(self.range.as_slice());
        // A side at the domain's ends is the side within it.
        let side = match side {
            Side::At if x < d1 => Side::Above,
            Side::At => Side::Below,
            side => side,
        };
        if n == 1 || e0 == e1 || d0 == d1 {
            // One cell, or one place in it, for every input: `e0` itself, the
            // first sample where the domain is a point, as encoding it reads.
            let key = if d0 == d1 { 0.0 } else { e0.max(0.0).min(last) };
            let (k0, k1) = (key.floor(), key.ceil());
            let (v0, v1) = (sample(k0)?, sample(k1)?);
            let value = Line {
                x0: k0,
                x1: k1,
                v0,
                v1,
            }
            .at(key);
            return Some(Line::constant(d0, d1, value).clipped(range, x, side));
        }
        let u = interpolate(x, d0, d1, e0, e1);
        let side_k = side.through(e1 > e0);
        let in_key = if u < 0.0 || u == 0.0 && side_k == Side::Below {
            Line::constant(f32::NEG_INFINITY, 0.0, sample(0.0)?)
        } else if u > last || u == last && side_k == Side::Above {
            Line::constant(last, f32::INFINITY, sample(last)?)
        } else {
            let k = match side_k {
                Side::Below => u.ceil() - 1.0,
                Side::At | Side::Above => u.floor(),
            }
            .max(0.0)
            .min(last - 1.0);
            Line {
                x0: k,
                x1: k + 1.0,
                v0: sample(k)?,
                v1: sample(k + 1.0)?,
            }
        };
        Some(in_key.mapped((d0, d1), (e0, e1)).clipped(range, x, side))
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
