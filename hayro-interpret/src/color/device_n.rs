use super::copy_cost;
use super::{ColorSpace, ToRgb, U8Lookup};
use crate::cache::Building;
use crate::encode::Cost;
use crate::function::{Function, MAX_COMPONENTS};
use hayro_syntax::object::{Array, Name, Object};
use rustc_hash::FxHashMap;

const MAX_CACHED_COLORS: usize = 1 << 18;

#[derive(Debug, Clone)]
pub(crate) struct DeviceN {
    alternate_space: ColorSpace,
    pub(super) num_components: u8,
    tint_transform: Function,
    is_none: bool,
    lookup: U8Lookup<[u8; 3]>,
}

impl DeviceN {
    /// The steps evaluating a color takes: its tint transform's, and the alternate
    /// space's.
    fn evaluation_cost(&self) -> Cost {
        Cost::of(usize::from(self.num_components))
            + self.tint_transform.cost()
            + self.alternate_space.cost()
    }

    pub(super) fn new(array: &Array<'_>, cache: &Building<'_>, depth: u8) -> Option<Self> {
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

    fn evaluate_pixel(&self, pixel: &[u8]) -> smallvec::SmallVec<[u8; 4]> {
        let mut components = [0.; MAX_COMPONENTS];
        for (component, &byte) in components.iter_mut().zip(pixel) {
            *component = super::u8_to_f32(byte);
        }
        let mut values = [0.; MAX_COMPONENTS];
        let values = &mut values[..self.tint_transform.num_outputs()];
        if self
            .tint_transform
            .eval(&components[..pixel.len()], values)
            .is_some()
        {
            self.alternate_space.encode_values(values)
        } else {
            self.alternate_space
                .encode_values(&self.alternate_space.initial_color())
        }
    }

    fn evaluate(&self, input: &[u8]) -> Vec<u8> {
        let num_components = usize::from(self.num_components);
        let num_encoded = usize::from(self.alternate_space.num_components());
        let mut evaluated = Vec::with_capacity(input.len() / num_components * num_encoded);

        if num_components > 8 {
            for pixel in input.chunks_exact(num_components) {
                evaluated.extend(self.evaluate_pixel(pixel));
            }

            return evaluated;
        }

        let mut cached = Vec::new();
        let mut offsets = FxHashMap::default();
        let mut previous = None;

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
                        cached.extend(self.evaluate_pixel(pixel));
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
                None => evaluated.extend(self.evaluate_pixel(pixel)),
            }
        }

        evaluated
    }

    fn convert_inner(&self, input: &[u8], output: &mut [u8]) -> Option<()> {
        if input.len() == usize::from(self.num_components) {
            return self
                .alternate_space
                .convert(&self.evaluate_pixel(input), output);
        }
        let evaluated = self.evaluate(input);
        self.alternate_space.convert(&evaluated, output)
    }

    fn u8_lookup(&self) -> Option<&[[u8; 3]; 256]> {
        self.lookup
            .get_or_init(|input, output| self.convert_inner(input, output))
    }
}

impl ToRgb for DeviceN {
    fn cost(&self) -> Cost {
        if self.num_components == 1 {
            copy_cost(1)
        } else {
            self.evaluation_cost()
        }
    }

    /// The table a tint of one component is looked up in, until it is built.
    fn setup_cost(&self) -> Cost {
        let table = if self.num_components == 1 {
            self.lookup.setup_cost(self.evaluation_cost())
        } else {
            Cost::ZERO
        };
        table + self.alternate_space.setup_cost()
    }

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

        let evaluated = self.evaluate(input);
        self.alternate_space.convert(&evaluated, input)
    }

    fn is_none(&self) -> bool {
        self.is_none
    }
}
