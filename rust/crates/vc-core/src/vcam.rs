//! 虚拟采集卡（vc-vcam.dll）注册与状态。
//!
//! 复用现有 C++ DLL，不重写 DShow filter。注册写 HKCU\\Software\\Classes\\CLSID\\{...}，
//! 产品固定 `FrameMap=<shm>`、`Motion=0`。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 与 `vcam/vcam.cpp` 一致的 CLSID。
pub const CLSID: &str = "{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}";

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct VcamStatus {
    /// 本目录下找到的 DLL 绝对路径。
    pub dll: String,
    /// 注册表里当前的 InprocServer32 路径。
    pub registered: String,
    /// 注册项是否指向本目录 DLL。
    pub match_local: bool,
}

impl VcamStatus {
    pub fn managed(&self) -> bool {
        self.match_local && !self.registered.is_empty()
    }
}

pub fn dll_path() -> PathBuf {
    crate::paths::vcam_dll()
}

pub fn status() -> VcamStatus {
    let dll = dll_path();
    let registered = read_registered_dll().unwrap_or_default();
    let match_local = !dll.as_os_str().is_empty()
        && !registered.is_empty()
        && normalize(&registered) == normalize(&dll.display().to_string());
    VcamStatus {
        dll: dll.display().to_string(),
        registered,
        match_local,
    }
}

fn normalize(path: &str) -> String {
    path.replace('/', "\\").to_ascii_lowercase()
}

/// 隔离子进程只读探测实际 DLL，不注册 COM 或接管现用设备。
pub fn require_capabilities() -> Result<(), String> {
    let mut probe = crate::paths::native_exe("vc-vcam-verify.exe");
    if !probe.is_file() {
        // --native-dir may be build/Release while --root is an isolated runtime.
        if let Some(build) = dll_path().parent().and_then(|dir| dir.parent()) {
            let candidate = build.join("vcam_verify/Release/vc-vcam-verify.exe");
            if candidate.is_file() {
                probe = candidate;
            }
        }
    }
    if !probe.is_file() {
        for config in ["Release", "Debug"] {
            let candidate = crate::paths::root()
                .join("build/vcam_verify")
                .join(config)
                .join("vc-vcam-verify.exe");
            if candidate.is_file() {
                probe = candidate;
                break;
            }
        }
    }
    let output = crate::process::run_bounded(
        std::process::Command::new(&probe)
            .arg("--dll")
            .arg(dll_path())
            .arg("--capabilities"),
        std::time::Duration::from_secs(5),
    )?;
    if !output.status.success() {
        return Err(format!(
            "虚拟卡能力探测失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let caps: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("虚拟卡能力响应无效：{e}"))?;
    if caps["stream_revision"] != 2
        || caps["variable_resolution"] != true
        || caps["shm_version"] != 3
    {
        return Err("虚拟卡缺少 stream_revision=2 / 可变尺寸 / SHM v3 能力。".into());
    }
    Ok(())
}

/// 注册本目录 DLL，并写入产品固定 FrameMap/Motion。
pub fn register(shm: &str) -> Result<VcamStatus, String> {
    let dll = dll_path();
    if !dll.exists() {
        return Err(format!("找不到 vc-vcam.dll：{}", dll.display()));
    }
    let regsvr = std::env::var("SystemRoot")
        .map(|root| format!("{root}\\System32\\regsvr32.exe"))
        .unwrap_or_else(|_| r"C:\Windows\System32\regsvr32.exe".into());
    let current = status();
    if !current.registered.is_empty() && !current.match_local {
        return Err("虚拟采集卡已注册到其他目录；先关闭并停用已有主机，避免覆盖稳定部署。".into());
    }
    let output = crate::process::run_bounded(
        std::process::Command::new(&regsvr).arg("/s").arg(&dll),
        std::time::Duration::from_secs(10),
    )?;
    if !output.status.success() {
        return Err(format!(
            "regsvr32 注册失败（退出码 {:?}）：{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    write_string_value("FrameMap", shm)?;
    write_dword_value("Motion", 0)?;
    let status = status();
    if !status.match_local {
        return Err(format!(
            "注册完成但 InprocServer32 未指向本目录 DLL（当前：{}）。",
            status.registered
        ));
    }
    Ok(status)
}

/// 只回收本目录匹配的注册，不碰其他目录。
pub fn unregister() -> Result<VcamStatus, String> {
    let current = status();
    if current.registered.is_empty() {
        return Ok(current);
    }
    if !current.match_local {
        return Err("虚拟采集卡注册路径不符，未回收其他目录的注册。".into());
    }
    let dll = dll_path();
    let regsvr = std::env::var("SystemRoot")
        .map(|root| format!("{root}\\System32\\regsvr32.exe"))
        .unwrap_or_else(|_| r"C:\Windows\System32\regsvr32.exe".into());
    let output = crate::process::run_bounded(
        std::process::Command::new(&regsvr)
            .args(["/s", "/u"])
            .arg(&dll),
        std::time::Duration::from_secs(10),
    )?;
    if !output.status.success() {
        return Err(format!(
            "regsvr32 注销失败（退出码 {:?}）。",
            output.status.code()
        ));
    }
    let after = status();
    if !after.registered.is_empty() {
        return Err("注销后虚拟采集卡注册仍存在，需重试停止。".into());
    }
    Ok(after)
}

#[cfg(windows)]
fn read_registered_dll() -> Option<String> {
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };
    let subkey = format!(r"Software\Classes\CLSID\{CLSID}\InprocServer32");
    let subkey = wide(&subkey);
    let mut key = std::ptr::null_mut();
    let opened =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut key) };
    if opened != 0 {
        return None;
    }
    let name = wide("");
    let mut kind = 0u32;
    let mut size = 0u32;
    let mut ok = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            std::ptr::null_mut(),
            &mut size,
        )
    } == 0;
    if !ok || kind != REG_SZ || size == 0 {
        unsafe { RegCloseKey(key) };
        return None;
    }
    let mut buffer = vec![0u16; (size / 2) as usize + 1];
    let mut size = (buffer.len() * 2) as u32;
    ok = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            buffer.as_mut_ptr() as *mut u8,
            &mut size,
        )
    } == 0;
    unsafe { RegCloseKey(key) };
    if !ok {
        return None;
    }
    Some(from_wide(&buffer))
}

#[cfg(not(windows))]
fn read_registered_dll() -> Option<String> {
    None
}

#[cfg(windows)]
fn write_string_value(name: &str, value: &str) -> Result<(), String> {
    let wide = wide(value);
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    write_value(name, bytes, 1)
}

#[cfg(windows)]
fn write_dword_value(name: &str, value: u32) -> Result<(), String> {
    write_value(name, &value.to_le_bytes(), 4)
}

#[cfg(windows)]
fn write_value(name: &str, bytes: &[u8], kind: u32) -> Result<(), String> {
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_SET_VALUE, RegCloseKey, RegCreateKeyExW, RegSetValueExW,
    };
    let subkey = wide(&format!(r"Software\Classes\CLSID\{CLSID}"));
    let mut key = std::ptr::null_mut();
    let created = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if created != 0 {
        return Err(format!("无法打开虚拟卡注册表键（错误 {created}）。"));
    }
    let name = wide(name);
    let result = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            kind,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    unsafe { RegCloseKey(key) };
    if result != 0 {
        return Err(format!("写入虚拟卡注册表值失败（错误 {result}）。"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn write_string_value(_name: &str, _value: &str) -> Result<(), String> {
    Err("虚拟采集卡仅支持 Windows。".into())
}

#[cfg(not(windows))]
fn write_dword_value(_name: &str, _value: u32) -> Result<(), String> {
    Err("虚拟采集卡仅支持 Windows。".into())
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn from_wide(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}
