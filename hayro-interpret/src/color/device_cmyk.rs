use super::icc::ICCProfile;
use super::{ICC_COST, ToRgb};
use crate::encode::Cost;
use std::sync::LazyLock;

#[derive(Debug, Clone)]
pub(crate) struct DeviceCmyk;

impl ToRgb for DeviceCmyk {
    fn cost(&self) -> Cost {
        ICC_COST + Cost::new(4)
    }

    fn setup_cost(&self) -> Cost {
        Cost::ZERO
    }

    fn convert(&self, input: &[u8], output: &mut [u8]) -> Option<()> {
        CMYK_TRANSFORM.convert(input, output)
    }
}

static CMYK_TRANSFORM: LazyLock<ICCProfile> = LazyLock::new(|| {
    ICCProfile::new(
        include_bytes!("../../assets/CGATS001Compat-v2-micro.icc"),
        4,
    )
    .unwrap()
});
