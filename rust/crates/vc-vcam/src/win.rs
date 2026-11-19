//! 滤镜用到的 Win32 配套设施：注册表/环境变量配置、只读 SHM v3 映射及其
//! 唤醒事件、延迟反馈环。对应历史 C++ 的 `ShmEnsureOpen`/`ShmClose` 与注册
//! 辅助函数；SHM 布局见 `docs/spec/shm-v3.md`。
//!
//! 本模块不写注册表；只有 `DllRegisterServer` 会写，且仅在 `regsvr32` /
//! `vc-core::vcam::register` 显式调用时发生。

use core::ffi::c_void;
use core::mem::size_of;
use core::ptr;
use std::sync::OnceLock;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, HANDLE, HMODULE, INVALID_HANDLE_VALUE, WIN32_ERROR,
};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Environment::GetEnvironmentVariableW;
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_BASIC_INFORMATION,
    MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
    VirtualQuery,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD, REG_SZ, REG_VALUE_TYPE, RegCloseKey,
    RegOpenKeyExW, RegQueryValueExW,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentThread,
    GetThreadPriority, OpenEventW, SYNCHRONIZATION_ACCESS_RIGHTS, SetThreadPriority,
    THREAD_PRIORITY, THREAD_PRIORITY_HIGHEST,
};
use windows::core::PCWSTR;

use crate::geometry::{DEFAULT_HEIGHT, DEFAULT_WIDTH, StreamGeometry};
use crate::shm::{self, ShmHeader, ShmSlot, VSTAT_SIZE};

pub const CLSID_STR: &str = "{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}";
pub const CLSID_KEY: &str = r"Software\Classes\CLSID\{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}";
pub const VIDEO_INPUT_CATEGORY: &str = "{860BB310-5D01-11d0-BD3B-00A0C911CE86}";
pub const INSTANCE_KEY: &str = r"Software\Classes\CLSID\{860BB310-5D01-11d0-BD3B-00A0C911CE86}\Instance\{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}";
pub const SHM_DEFAULT: &str = r"Local\VirtualCaptureVcamFrame";
pub const FRAME_MAP_VALUE: &str = "FrameMap";
pub const MOTION_VALUE: &str = "Motion";
pub const VCAM_NAME: &str = "VirtualCapture";
pub const PIN_ID: &str = "Capture";
pub const SYNCHRONIZE_ACCESS: SYNCHRONIZATION_ACCESS_RIGHTS =
    SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(core::iter::once(0)).collect()
}

// ---------- 配置 ----------

/// `HKCU\Software\Classes\CLSID\{...}` 下的 `FrameMap` REG_SZ（由
/// `vc-core::vcam::register` 写入）。值缺失或无效时返回 `None`。
pub fn registry_frame_map() -> Option<String> {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(CLSID_KEY).as_ptr()),
            None,
            KEY_READ,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let mut buf = [0u16; 260];
        let mut cb: u32 = (buf.len() * 2) as u32;
        let mut ty = REG_VALUE_TYPE::default();
        let rc = RegQueryValueExW(
            key,
            PCWSTR(wide(FRAME_MAP_VALUE).as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut cb),
        );
        let _ = RegCloseKey(key);
        if rc != ERROR_SUCCESS || ty != REG_SZ {
            return None;
        }
        let chars = ((cb as usize) / 2).min(buf.len());
        if chars == 0 {
            return Some(String::new());
        }
        let len = buf[..chars].iter().position(|&c| c == 0).unwrap_or(chars);
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

/// `Motion` REG_DWORD。仅为对齐/诊断保留：Rust 滤镜从不伪造运动帧
/// （D12 运动画面生成未实现）。
pub fn registry_motion() -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(CLSID_KEY).as_ptr()),
            None,
            KEY_READ,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let mut value = 0u32;
        let mut cb: u32 = size_of::<u32>() as u32;
        let mut ty = REG_VALUE_TYPE::default();
        let rc = RegQueryValueExW(
            key,
            PCWSTR(wide(MOTION_VALUE).as_ptr()),
            None,
            Some(&mut ty),
            Some(&mut value as *mut u32 as *mut u8),
            Some(&mut cb),
        );
        let _ = RegCloseKey(key);
        rc == ERROR_SUCCESS && ty == REG_DWORD && value != 0
    }
}

fn env_frame_map() -> Option<String> {
    unsafe {
        let mut buf = [0u16; 260];
        let len = GetEnvironmentVariableW(PCWSTR(wide("VCAM_FRAME_MAP").as_ptr()), Some(&mut buf))
            as usize;
        if len == 0 || len >= buf.len() {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

/// 解析顺序与历史 C++ `ShmEnsureOpen` 一致：环境变量覆盖，其次注册表
/// `FrameMap`，最后使用默认的本地映射名。
pub fn frame_map_name() -> String {
    if let Some(name) = env_frame_map() {
        return name;
    }
    if let Some(name) = registry_frame_map() {
        return name;
    }
    SHM_DEFAULT.to_string()
}

// ---------- 只读映射 ----------

pub struct ReadMapping {
    map: HANDLE,
    base: *mut c_void,
    size: usize,
}

impl ReadMapping {
    /// 以只读方式打开并校验 `<name>`。映射大小至少须为
    /// `shm::shm_size(frame_bytes)`；不存在、已被拆除或无效时返回 `None`。
    /// 几何从 v3 头读取。
    pub fn open(name: &str) -> Option<Self> {
        unsafe {
            let wname = wide(name);
            let map = OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(wname.as_ptr())).ok()?;
            if map.is_invalid() {
                return None;
            }
            let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, 0);
            if view.Value.is_null() {
                let _ = CloseHandle(map);
                return None;
            }
            let base = view.Value;
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            if VirtualQuery(
                Some(base as *const c_void),
                &mut mbi,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            ) == 0
            {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(map);
                return None;
            }
            let h = &*(base as *const ShmHeader);
            if !shm::valid_header(h) || mbi.RegionSize < shm::shm_size(h.frame_bytes) {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(map);
                return None;
            }
            Some(Self {
                map,
                base,
                size: shm::shm_size(h.frame_bytes),
            })
        }
    }

    pub fn view(&self) -> *mut u8 {
        self.base as *mut u8
    }

    pub fn geometry(&self) -> Option<StreamGeometry> {
        unsafe {
            let h = &*(self.base as *const ShmHeader);
            shm::geometry_from_header(h)
        }
    }

    pub fn frame_bytes(&self) -> u32 {
        unsafe { (*(self.base as *const ShmHeader)).frame_bytes }
    }

    pub fn size(&self) -> usize {
        self.size
    }
}

impl Drop for ReadMapping {
    fn drop(&mut self) {
        unsafe {
            if !self.base.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.base });
                self.base = ptr::null_mut();
            }
            if !self.map.is_invalid() {
                let _ = CloseHandle(self.map);
                self.map = HANDLE::default();
            }
        }
    }
}

/// 接收端映射已在发布时，返回其公布的会话几何。滤镜创建时调用一次，使输出
/// 媒体类型在图枚举 pin 之前就与会话一致。
pub fn discover_geometry() -> Option<StreamGeometry> {
    let mapping = ReadMapping::open(&frame_map_name())?;
    mapping.geometry()
}

/// 默认几何仍为 640x640@240 的 v2 契约。
pub fn default_geometry() -> StreamGeometry {
    StreamGeometry::new(DEFAULT_WIDTH, DEFAULT_HEIGHT, crate::geometry::DEFAULT_FPS)
        .unwrap_or(StreamGeometry::DEFAULT)
}

// ---------- 可写映射 + 事件句柄 ----------

pub struct ShmSet {
    geometry: StreamGeometry,
    data_map: HANDLE,
    data_view: *mut u8,
    events: [HANDLE; 2],
    vstat_map: HANDLE,
    vstat_view: *mut u8,
}

// 裸视图指针只被唯一的 worker 线程使用，且仅在该线程 join 之后释放；
// COM 式访问由 FilterCore 串行化。
unsafe impl Send for ShmSet {}
unsafe impl Sync for ShmSet {}

impl ShmSet {
    /// 为 `geometry` 打开接收端映射。公布的会话几何不一致时直接失败（而非
    /// 适配）：滤镜实例的媒体类型不可变，必须重开设备。
    pub fn open(name: &str, geometry: StreamGeometry) -> Option<Self> {
        let mapping = ReadMapping::open(name)?;
        if mapping.geometry()? != geometry {
            return None;
        }
        let data_map = mapping.map;
        let data_view = mapping.base as *mut u8;
        core::mem::forget(mapping); // 所有权移入 ShmSet

        let mut events = [HANDLE::default(); 2];
        for (i, slot) in events.iter_mut().enumerate() {
            let event_name = format!("{name}.evt{i}");
            let wname = wide(&event_name);
            if let Ok(h) = unsafe { OpenEventW(SYNCHRONIZE_ACCESS, false, PCWSTR(wname.as_ptr())) }
            {
                *slot = h;
            }
        }

        let (vstat_map, vstat_view) = unsafe {
            let vstat_name = wide(&format!("{name}.vcam"));
            match CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                VSTAT_SIZE as u32,
                PCWSTR(vstat_name.as_ptr()),
            ) {
                Ok(map) => {
                    let view = MapViewOfFile(map, FILE_MAP_WRITE, 0, 0, VSTAT_SIZE);
                    if view.Value.is_null() {
                        let _ = CloseHandle(map);
                        (HANDLE::default(), ptr::null_mut())
                    } else {
                        (map, view.Value as *mut u8)
                    }
                }
                Err(_) => (HANDLE::default(), ptr::null_mut()),
            }
        };
        if !vstat_view.is_null() {
            unsafe { shm::vstat_init(vstat_view) };
        }

        Some(Self {
            geometry,
            data_map,
            data_view,
            events,
            vstat_map,
            vstat_view,
        })
    }

    pub fn geometry(&self) -> StreamGeometry {
        self.geometry
    }

    pub fn view(&self) -> *mut u8 {
        self.data_view
    }

    /// `evt[0]`/`evt[1]`；空句柄表示“未打开”（退化为轮询）。
    pub fn event(&self, index: usize) -> HANDLE {
        self.events.get(index).copied().unwrap_or_default()
    }

    pub fn vstat(&self) -> *mut u8 {
        self.vstat_view
    }

    /// 只读窥视 seqlock，返回最新的稳定令牌。
    pub fn peek(&self) -> Option<(u32, i32)> {
        unsafe { shm::peek(self.data_view) }
    }

    pub fn slot(&self, slot: u32) -> *mut ShmSlot {
        unsafe { shm::slot_ptr(self.data_view, slot) }
    }

    /// 对某个已发布槽做稳定的 seqlock 拷贝。
    ///
    /// # Safety
    ///
    /// `dst` 必须指向至少 `hdr.frame_bytes` 个可写字节，且不得与映射的 SHM
    /// 区域重叠；`slot`/`seq` 必须来自 [`ShmSet::peek`]，映射在调用期间须
    /// 保持存活。
    pub unsafe fn copy_slot(&self, slot: u32, seq: i32, dst: *mut u8) -> Option<shm::SlotCopy> {
        unsafe { shm::copy_slot(self.data_view, slot, seq, dst) }
    }
}

impl Drop for ShmSet {
    fn drop(&mut self) {
        unsafe {
            if !self.vstat_view.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.vstat_view as *mut c_void,
                });
                self.vstat_view = ptr::null_mut();
            }
            if !self.vstat_map.is_invalid() {
                let _ = CloseHandle(self.vstat_map);
                self.vstat_map = HANDLE::default();
            }
            for event in &mut self.events {
                if !event.is_invalid() {
                    let _ = CloseHandle(*event);
                    *event = HANDLE::default();
                }
            }
            if !self.data_view.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.data_view as *mut c_void,
                });
                self.data_view = ptr::null_mut();
            }
            if !self.data_map.is_invalid() {
                let _ = CloseHandle(self.data_map);
                self.data_map = HANDLE::default();
            }
        }
    }
}

// ---------- 计时 ----------

pub fn qpc_frequency() -> u64 {
    static FREQ: OnceLock<u64> = OnceLock::new();
    *FREQ.get_or_init(|| unsafe {
        let mut freq = 0i64;
        if QueryPerformanceFrequency(&mut freq).is_err() || freq <= 0 {
            1
        } else {
            freq as u64
        }
    })
}

pub fn qpc_ns() -> u64 {
    unsafe {
        let mut counter = 0i64;
        if QueryPerformanceCounter(&mut counter).is_err() {
            return 0;
        }
        shm::qpc_to_ns(counter as u64, qpc_frequency())
    }
}

// ---------- worker 线程调度 ----------

/// 沿用历史 worker 策略（`vcam/vcam.cpp:715-721`，于 `825-827` 回退）：
/// 把定时器周期提升到 1 ms，以满足 1 ms 无事件轮询与 1.5 帧重复截止时间；
/// 并把 worker 加入 MMCSS "Capture" 任务；MMCSS 不可用时，只把本 worker
/// 线程提升到 `THREAD_PRIORITY_HIGHEST`（绝不动宿主进程优先级）。
///
/// 该 guard 在 worker 的所有退出路径上恢复 MMCSS 句柄与定时器周期，包括
/// 提前 `break`/错误返回。
pub struct WorkerScheduling {
    mmcss: Option<HANDLE>,
    /// 应用 `HIGHEST` 回退前的线程优先级；恢复 MMCSS 句柄时由 MMCSS
    /// 自行恢复优先级。
    previous_priority: Option<i32>,
}

/// `GetThreadPriority` 失败时返回该值。
const PRIORITY_ERROR: i32 = i32::MAX;

impl WorkerScheduling {
    /// 必须在 worker 线程本身上调用：MMCSS 特性与线程优先级回退都只作用于
    /// 调用线程。
    pub fn raise() -> Self {
        unsafe {
            let _ = timeBeginPeriod(1);
            let mut task_index = 0u32;
            let mmcss =
                AvSetMmThreadCharacteristicsW(windows::core::w!("Capture"), &mut task_index).ok();
            let mut previous_priority = None;
            if mmcss.is_none() {
                let previous = GetThreadPriority(GetCurrentThread());
                if previous != PRIORITY_ERROR
                    && SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST).is_ok()
                {
                    previous_priority = Some(previous);
                }
            }
            Self {
                mmcss,
                previous_priority,
            }
        }
    }
}

impl Drop for WorkerScheduling {
    fn drop(&mut self) {
        if let Some(mmcss) = self.mmcss.take() {
            let _ = unsafe { AvRevertMmThreadCharacteristics(mmcss) };
        } else if let Some(previous) = self.previous_priority.take() {
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY(previous)) };
        }
        let _ = unsafe { timeEndPeriod(1) };
    }
}

// ---------- 模块路径（注册用） ----------

/// 本 DLL 的完整路径，用作 InprocServer32 数据。以本 crate 内某个函数的
/// 地址反查自身模块。
pub fn module_path() -> Option<Vec<u16>> {
    use windows::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };
    unsafe {
        let mut module = HMODULE::default();
        let flags =
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        let anchor = dll_module_anchor as *const () as usize as *const u16;
        GetModuleHandleExW(flags, PCWSTR(anchor), &mut module).ok()?;
        let mut buf = vec![0u16; 32768];
        let len = GetModuleFileNameW(Some(module), &mut buf);
        if len == 0 {
            return None;
        }
        buf.truncate(len as usize);
        buf.push(0);
        Some(buf)
    }
}

unsafe extern "system" fn dll_module_anchor() {}

// ---------- 小型转换 ----------

pub fn win32_ok(rc: WIN32_ERROR) -> bool {
    rc == ERROR_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Threading::GetThreadPriority;

    #[test]
    fn worker_scheduling_applies_policy_and_reverts() {
        let before = unsafe { GetThreadPriority(GetCurrentThread()) };
        let first = WorkerScheduling::raise();
        let fallback = first.mmcss.is_none();
        if fallback {
            // MMCSS 不可用：`HIGHEST` 回退只能作用于本线程。
            let during = unsafe { GetThreadPriority(GetCurrentThread()) };
            assert_eq!(during, THREAD_PRIORITY_HIGHEST.0);
        }
        drop(first);
        if fallback && before != PRIORITY_ERROR {
            let after = unsafe { GetThreadPriority(GetCurrentThread()) };
            assert_eq!(after, before, "HIGHEST fallback restores the old priority");
        }
        // 第二轮往返同时检查定时器周期的成对调用。
        drop(WorkerScheduling::raise());
    }
}
