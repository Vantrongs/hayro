use super::{ToLuma, ToRgb, copy_cost};
use crate::encode::Cost;

#[derive(Debug, Clone)]
pub(crate) struct DeviceGray;

impl ToRgb for DeviceGray {
    fn cost(&self) -> Cost {
        copy_cost(1)
    }

    fn setup_cost(&self) -> Cost {
        Cost::ZERO
    }

    fn convert(&self, input: &[u8], output: &mut [u8]) -> Option<()> {
        for (gray, output) in input.iter().zip(output.as_chunks_mut::<3>().0) {
            output.copy_from_slice(&[*gray, *gray, *gray]);
        }

        Some(())
    }
}

impl ToLuma for DeviceGray {
    fn to_luma(&self, _input: &mut [u8]) -> Option<()> {
        Some(())
    }
}
