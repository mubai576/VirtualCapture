//! Rust 媒体数据面，由诊断 CLI 与版本化产品 DLL 共用。

#![recursion_limit = "256"]

#[cfg(windows)]
pub mod abi;
#[cfg(windows)]
pub mod client;
pub mod pattern;
#[cfg(windows)]
pub mod shm;
#[cfg(windows)]
#[path = "windows.rs"]
pub mod windows;
