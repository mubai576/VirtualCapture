//! Rust media data plane, shared by the diagnostic CLI and versioned product DLL.

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
