use super::{ColorSpace, ToRgb, U8Lookup, copy_cost};
use crate::cache::Cache;
use crate::encode::Cost;
use crate::function::Function;
use hayro_syntax::object::{Array, Name, Object};

#[derive(Debug, Clone)]
pub(crate) struct DeviceN {
    alternate_space: ColorSpace,
    pub(super) num_components: u8,
    tint_transform: Function,
    is_none: bool,
    lookup: U8Lookup<[u8; 3]>,
}

impl DeviceN {
    pub(super) fn new(array: &Array<'_>, cache: &Cache) -> Option<Self> {
        let mut iter = array.flex_iter();
        // Skip `/DeviceN`
        let _ = iter.next::<Name<'_>>()?;
        // Skip `Name`.
        let names = iter
            .next::<Array<'_>>()?
            .items::<Name<'_>>()
            .map_while(|n| n)
            .collect::<Vec<_>>();
        let num_components = u8::try_from(names.len()).ok()?;
        let all_none = names.iter().all(|n| n.as_str() == "None");
        let alternate_space = ColorSpace::new(iter.next::<Object<'_>>()?, cache)?;
        let tint_transform = Function::new(&iter.next::<Object<'_>>()?)?;

        if num_components == 0 {
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

    fn evaluate(&self, input: &[u8]) -> Vec<u8> {
        input
            .chunks_exact(self.num_components as usize)
            .flat_map(|n| {
                let input = n.iter().map(|value| *value as f32 / 255.0).collect();
                let values = self
                    .tint_transform
                    .eval(input)
                    .unwrap_or(self.alternate_space.initial_color());
                self.alternate_space.encode_values(&values)
            })
            .collect()
    }

    /// The steps evaluating a color takes: its tint transform's, and the alternate
    /// space's.
    fn evaluation_cost(&self) -> Cost {
        Cost::of(usize::from(self.num_components))
            + self.tint_transform.cost()
            + self.alternate_space.cost()
    }

    fn convert_inner(&self, input: &[u8], output: &mut [u8]) -> Option<()> {
        let evaluated = self.evaluate(input);
        self.alternate_space.convert(&evaluated, output)
    }

    fn u8_lookup(&self) -> Option<&[[u8; 3]; 256]> {
        self.lookup
            .get_or_init(|input, output| self.convert_inner(input, output))
    }
}

impl ToRgb for DeviceN {
    /// A tint of one component is looked up, others are evaluated.
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
