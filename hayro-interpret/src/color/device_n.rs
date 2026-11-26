use super::{ColorSpace, ToRgb, U8Lookup, u8_to_f32};
use crate::cache::Cache;
use crate::function::{Function, MAX_COMPONENTS};
use hayro_syntax::object::{Array, Name, Object};
use rustc_hash::FxHashMap;

const BATCH_PIXELS: usize = 4096;
const MAX_CACHED_COLORS: usize = 1 << 18;

#[derive(Debug, Clone)]
pub(crate) struct DeviceN {
    pub(super) alternate_space: ColorSpace,
    pub(super) num_components: u8,
    pub(super) tint_transform: Function,
    is_none: bool,
    pub(super) lookup: U8Lookup<[u8; 3]>,
}

impl DeviceN {
    pub(super) fn new(array: &Array<'_>, cache: &Cache, depth: u8) -> Option<Self> {
        let mut iter = array.flex_iter();
        let _ = iter.next::<Name<'_>>()?;
        let names = iter
            .next::<Array<'_>>()?
            .iter::<Name<'_>>()
            .collect::<Vec<_>>();
        let num_components = u8::try_from(names.len()).ok()?;
        let all_none = names.iter().all(|n| n.as_str() == "None");
        let alternate_space = ColorSpace::new_nested(iter.next::<Object<'_>>()?, cache, depth)?;
        let tint_transform = Function::new(&iter.next::<Object<'_>>()?)?;

        if num_components == 0 || num_components as usize > MAX_COMPONENTS {
            return None;
        }

        Some(Self {
            alternate_space,
            num_components,
            tint_transform,
            is_none: all_none,
            lookup: U8Lookup::default(),
        })
    }

    fn evaluator(&self) -> impl FnMut(&[u8], &mut Vec<u8>) + '_ {
        let num_components = self.num_components as usize;
        let num_encoded = self.alternate_space.num_components() as usize;
        let mut components = [0.0; MAX_COMPONENTS];
        let mut values = [0.0; MAX_COMPONENTS];
        let num_outputs = self.tint_transform.num_outputs();
        let mut evaluate_pixel = move |pixel: &[u8]| {
            let components = &mut components[..num_components];
            let values = &mut values[..num_outputs];
            for (component, value) in components.iter_mut().zip(pixel) {
                *component = u8_to_f32(*value);
            }

            if self.tint_transform.eval(components, values).is_some() {
                self.alternate_space.encode_values(values)
            } else {
                self.alternate_space
                    .encode_values(&self.alternate_space.initial_color())
            }
        };
        let mut cached = Vec::new();
        let mut offsets = FxHashMap::default();
        let mut previous = None;

        move |input, evaluated| {
            evaluated.clear();
            evaluated.reserve_exact(input.len() / num_components * num_encoded);
            if num_components == 1 || num_components > 8 || input.len() == num_components {
                for pixel in input.chunks_exact(num_components) {
                    evaluated.extend(evaluate_pixel(pixel));
                }

                return;
            }

            for pixel in input.chunks_exact(num_components) {
                let key = pixel
                    .iter()
                    .fold(0_u64, |key, value| (key << 8) | u64::from(*value));

                let offset = match previous {
                    Some((previous_key, offset)) if previous_key == key => Some(offset),
                    _ => match offsets.get(&key) {
                        Some(&offset) => Some(offset),
                        None if offsets.len() < MAX_CACHED_COLORS => {
                            let offset = cached.len();
                            cached.extend(evaluate_pixel(pixel));
                            offsets.insert(key, offset);

                            Some(offset)
                        }
                        None => None,
                    },
                };

                match offset {
                    Some(offset) => {
                        evaluated.extend_from_slice(&cached[offset..offset + num_encoded]);
                        previous = Some((key, offset));
                    }
                    None => evaluated.extend(evaluate_pixel(pixel)),
                }
            }
        }
    }

    fn convert_inner(&self, input: &[u8], output: &mut [u8]) -> Option<()> {
        let n = usize::from(self.num_components);
        if !output.len().is_multiple_of(3) || input.len() / n != output.len() / 3 {
            return None;
        }
        let mut evaluate = self.evaluator();
        let mut buf = Vec::new();
        if input.is_empty() {
            return self.alternate_space.convert(&[], output);
        }
        for (src, dst) in input
            .chunks(BATCH_PIXELS * n)
            .zip(output.chunks_mut(BATCH_PIXELS * 3))
        {
            evaluate(src, &mut buf);
            self.alternate_space.convert(&buf, dst)?;
        }
        Some(())
    }

    fn u8_lookup(&self) -> Option<&[[u8; 3]; 256]> {
        self.lookup.get_or_init(|input, output| {
            let mut buf = Vec::new();
            self.evaluator()(input, &mut buf);
            self.alternate_space.convert_table(&buf, output)
        })
    }
}

impl ToRgb for DeviceN {
    fn convert(&self, input: &[u8], output: &mut [u8]) -> Option<()> {
        if self.num_components == 1 {
            let lookup = self.u8_lookup()?;
            for (input, output) in input.iter().zip(output.as_chunks_mut::<3>().0) {
                output.copy_from_slice(&lookup[*input as usize]);
            }

            Some(())
        } else {
            self.convert_inner(input, output)
        }
    }

    fn convert_in_place(&self, input: &mut [u8]) -> Option<()> {
        if self.num_components != 3 {
            return None;
        }

        let mut evaluate = self.evaluator();
        if !input.len().is_multiple_of(3) {
            return None;
        }
        let mut buf = Vec::new();
        if input.is_empty() {
            return self.alternate_space.convert(&buf, input);
        }
        for chunk in input.chunks_mut(BATCH_PIXELS * 3) {
            evaluate(chunk, &mut buf);
            self.alternate_space.convert(&buf, chunk)?;
        }
        Some(())
    }

    fn is_none(&self) -> bool {
        self.is_none
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hayro_syntax::{Pdf, object::ObjectIdentifier};

    #[test]
    fn multi_batch_tints_match_whole_image() {
        let pdf = Pdf::new(crate::fixtures::one_page(
            1,
            "",
            vec![
                b"[/DeviceN [/A /B /C] /DeviceRGB 5 0 R]".to_vec(),
                crate::fixtures::stream(
                    "/FunctionType 4 /Domain [0 1 0 1 0 1] /Range [0 1 0 1 0 1]",
                    b"{ exch }",
                ),
            ],
        ))
        .unwrap();
        let array = pdf.xref().get(ObjectIdentifier::new(4, 0)).unwrap();
        let space = DeviceN::new(&array, &Cache::new(), 0).unwrap();
        let mut input: Vec<_> = (0..(BATCH_PIXELS * 2 + 17) * 3)
            .map(|i| (i % 251) as u8)
            .collect();
        let mut evaluated = Vec::new();
        space.evaluator()(&input, &mut evaluated);
        let mut expected = vec![0; input.len()];
        space
            .alternate_space
            .convert(&evaluated, &mut expected)
            .unwrap();
        let mut output = vec![0; input.len()];
        space.convert(&input, &mut output).unwrap();
        assert_eq!(output, expected);
        space.convert_in_place(&mut input).unwrap();
        assert_eq!(input, expected);
    }
}
