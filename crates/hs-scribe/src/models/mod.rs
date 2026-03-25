pub mod layout;
pub mod table_structure;

use anyhow::Result;

/// Common interface for all ML Models
pub trait ModelInference {
    type Input;
    type Output;

    fn infer(&self, input: Self::Input) -> Result<Self::Output>;
    fn name(&self) -> &str;
    fn warmup(&self) -> Result<String>;
}
