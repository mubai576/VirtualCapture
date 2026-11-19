//! `vc_vcam.dll` — VirtualCapture DirectShow source filter in Rust.
//!
//! Public surface:
//! * the in-process COM server `DllGetClassObject` / `DllCanUnloadNow`
//!   (CLSID `{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}`);
//! * `DllRegisterServer` / `DllUnregisterServer`, HKCU-first like the C++
//!   `vcam/vcam.cpp` filter (HKLM is best effort);
//! * `VCStreamCapabilities` (legacy verifier symbol) and
//!   `vc_vcam_capabilities`, both returning the frozen JSON from
//!   `common/stream.h`.
//!
//! The filter reads SHM v3 produced by the receiver, advertises the session
//! geometry as NV12 `VIDEOINFOHEADER`, and never generates pixels: it waits
//! for the first verified SHM frame and afterwards only repeats the last good
//! frame when the receiver is momentarily behind.

#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

pub mod abi;
pub mod capabilities;
pub mod com;
pub mod geometry;
pub mod media;
pub mod shm;
pub mod win;

use core::ffi::{c_char, c_void};

use windows::core::{GUID, HRESULT};

/// COM class factory entry point used by `CoCreateInstance`/`IClassFactory`.
///
/// # Safety
/// `rclsid`, `riid` and `ppv` must follow the COM in-process server contract.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    unsafe { com::dll_get_class_object(rclsid, riid, ppv) }
}

/// `S_OK` only when no filter/allocator/enumerator and no server lock is live.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    com::dll_can_unload_now()
}

/// Registers the filter under HKCU (and HKLM when permitted).
#[unsafe(no_mangle)]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    com::register_server()
}

/// Removes the registrations written by [`DllRegisterServer`].
#[unsafe(no_mangle)]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    com::unregister_server()
}

/// Legacy export consumed by `vc-vcam-verify.exe --capabilities`.
///
/// The returned pointer is static and must not be freed.
#[unsafe(no_mangle)]
pub extern "C" fn vc_vcam_capabilities() -> *const c_char {
    capabilities::capabilities_cstr()
}

/// C++-compatible spelling of the same capability export.
#[unsafe(no_mangle)]
pub extern "C" fn VCStreamCapabilities() -> *const c_char {
    capabilities::capabilities_cstr()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_exports_return_the_same_static_string() {
        let a = vc_vcam_capabilities();
        let b = VCStreamCapabilities();
        assert_eq!(a, b);
        assert!(!a.is_null());
    }
}
