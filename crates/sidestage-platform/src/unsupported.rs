//! unsupported backend (placeholder until implemented).

use crate::Platform;

pub fn create() -> anyhow::Result<Box<dyn Platform>> {
    anyhow::bail!("Sidestage does not support this operating system yet")
}

#[allow(dead_code)]
pub fn run_helper(_args: &[String]) -> i32 {
    2
}
