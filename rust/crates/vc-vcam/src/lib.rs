//! `vc_vcam.dll` —— Rust 实现的 VirtualCapture DirectShow 源滤镜。
//!
//! 对外接口：
//! * 进程内 COM 服务器 `DllGetClassObject` / `DllCanUnloadNow`
//!   （CLSID `{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}`）；
//! * `DllRegisterServer` / `DllUnregisterServer`，与历史 C++
//!   `vcam/vcam.cpp` 滤镜一致优先写 HKCU（HKLM 尽力而为）；
//! * `VCStreamCapabilities`（旧验证器符号）与
//!   `vc_vcam_capabilities`，二者返回同一份冻结 JSON（Rust 能力门
//!   见 `vc-core::vcam::require_capabilities`）。
//!
//! 滤镜读取接收端产生的 SHM v3（布局见 `docs/spec/shm-v3.md`），把会话
//! 几何以 NV12 `VIDEOINFOHEADER` 对外公布，自身从不生成像素：等待首个
//! 校验通过的 SHM 帧，之后仅在接收端暂时落后时重复最后一帧好帧。

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

/// `CoCreateInstance`/`IClassFactory` 使用的 COM 类工厂入口。
///
/// # Safety
/// `rclsid`、`riid` 与 `ppv` 必须遵守 COM 进程内服务器契约。
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    unsafe { com::dll_get_class_object(rclsid, riid, ppv) }
}

/// 仅当没有存活的 filter/allocator/enumerator 且无服务器锁时返回 `S_OK`。
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    com::dll_can_unload_now()
}

/// 在 HKCU 下注册滤镜（获许可时连同 HKLM）。
#[unsafe(no_mangle)]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    com::register_server()
}

/// 删除 [`DllRegisterServer`] 写入的注册项。
#[unsafe(no_mangle)]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    com::unregister_server()
}

/// 供 `vc-vcam-verify.exe --capabilities` 使用的旧导出。
///
/// 返回指针具有静态生存期，调用方不得释放。
#[unsafe(no_mangle)]
pub extern "C" fn vc_vcam_capabilities() -> *const c_char {
    capabilities::capabilities_cstr()
}

/// 同一能力导出的 C++ 兼容拼写。
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
