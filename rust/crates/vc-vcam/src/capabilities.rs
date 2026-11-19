//! Stream capability advertisement.
//!
//! `vc-vcam-verify.exe --dll <dll> --capabilities` requires the C export
//! `VCStreamCapabilities` (legacy name, see `vcam/vcam.cpp`).  The Rust crate
//! additionally exports `vc_vcam_capabilities`; both return the exact JSON
//! string from `common/stream.h::vc_stream_capabilities()` so the capability
//! gate in `vc-core::vcam::require_capabilities` keeps working unchanged.

use core::ffi::c_char;

/// Exact C++ capability payload, without the terminating NUL.
pub const CAPABILITIES_JSON: &str = "{\"stream_revision\":2,\"codecs\":[\"raw\"],\"pixel_format\":\"nv12\",\"variable_resolution\":true,\"dda_fps_limit\":true,\"shm_version\":3,\"max_dimension\":8192}";

/// NUL-terminated byte string shared by both exported symbols.
pub static CAPABILITIES_CSTR: &[u8] =
    b"{\"stream_revision\":2,\"codecs\":[\"raw\"],\"pixel_format\":\"nv12\",\"variable_resolution\":true,\"dda_fps_limit\":true,\"shm_version\":3,\"max_dimension\":8192}\0";

/// `const char*` view of [`CAPABILITIES_CSTR`].
///
/// The pointer has static lifetime and must not be freed by the caller.
pub fn capabilities_cstr() -> *const c_char {
    CAPABILITIES_CSTR.as_ptr() as *const c_char
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_matches_cpp_exactly() {
        assert_eq!(CAPABILITIES_JSON.len() + 1, CAPABILITIES_CSTR.len());
        assert_eq!(
            &CAPABILITIES_CSTR[..CAPABILITIES_CSTR.len() - 1],
            CAPABILITIES_JSON.as_bytes()
        );
        assert_eq!(*CAPABILITIES_CSTR.last().unwrap(), 0);
    }

    #[test]
    fn required_gate_fields_are_present() {
        let text = CAPABILITIES_JSON;
        assert!(text.contains("\"stream_revision\":2"));
        assert!(text.contains("\"variable_resolution\":true"));
        assert!(text.contains("\"shm_version\":3"));
        assert!(text.contains("\"max_dimension\":8192"));
        assert!(text.contains("\"pixel_format\":\"nv12\""));
    }
}
