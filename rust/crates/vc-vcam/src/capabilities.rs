//! 流能力广告。
//!
//! `vc-vcam-verify.exe --dll <dll> --capabilities` 需要 C 导出
//! `VCStreamCapabilities`（旧名称，见历史 C++ 实现 `vcam/vcam.cpp`）。本
//! Rust crate 另外导出 `vc_vcam_capabilities`；两者返回同一份 JSON 字符串，
//! 其值来源为历史 C++ 能力载荷的黄金基准，供
//! `vc-core::vcam::require_capabilities` 的能力门原样使用。

use core::ffi::c_char;

/// 与历史 C++ 能力载荷逐字节一致，不含结尾 NUL。
pub const CAPABILITIES_JSON: &str = "{\"stream_revision\":2,\"codecs\":[\"raw\"],\"pixel_format\":\"nv12\",\"variable_resolution\":true,\"dda_fps_limit\":true,\"shm_version\":3,\"max_dimension\":8192}";

/// 两个导出符号共享的、以 NUL 结尾的字节串。
pub static CAPABILITIES_CSTR: &[u8] =
    b"{\"stream_revision\":2,\"codecs\":[\"raw\"],\"pixel_format\":\"nv12\",\"variable_resolution\":true,\"dda_fps_limit\":true,\"shm_version\":3,\"max_dimension\":8192}\0";

/// [`CAPABILITIES_CSTR`] 的 `const char*` 视图。
///
/// 该指针具有静态生存期，调用方不得释放。
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
