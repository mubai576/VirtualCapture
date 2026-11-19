//! Standalone Rust Raw NV12 diagnostics. Does not change the product data plane.

pub mod pattern;
#[cfg(windows)]
pub mod shm;
