//! Win32 plumbing used by the filter: registry/env configuration, the
//! read-only SHM v3 mapping and its wakeup events, and the latency feedback
//! ring.  Mirrors the C++ `ShmEnsureOpen`/`ShmClose` and registration helpers.
//!
//! Nothing here writes the registry; only `DllRegisterServer` does, and only
//! when explicitly called by `regsvr32` / `vc-core::vcam::register`.

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

// ---------- configuration ----------

/// `FrameMap` REG_SZ from `HKCU\Software\Classes\CLSID\{...}` (written by
/// `vc-core::vcam::register`).  Returns `None` for missing/invalid values.
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

/// `Motion` REG_DWORD.  Kept for parity/diagnostics only: the Rust filter
/// never fabricates motion frames (D12 motion generation is not implemented).
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

/// Resolution order matches the C++ `ShmEnsureOpen`: environment override,
/// then registry `FrameMap`, then the default local mapping name.
pub fn frame_map_name() -> String {
    if let Some(name) = env_frame_map() {
        return name;
    }
    if let Some(name) = registry_frame_map() {
        return name;
    }
    SHM_DEFAULT.to_string()
}

// ---------- read-only mapping ----------

pub struct ReadMapping {
    map: HANDLE,
    base: *mut c_void,
    size: usize,
}

impl ReadMapping {
    /// Opens and validates `<name>` read-only.  The mapping must be at least
    /// `vc_shm_size(frame_bytes)`; returns `None` when absent, torn down or
    /// invalid.  Geometry is read from the (v3) header.
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

/// Session geometry advertised by the receiver's mapping, if one is already
/// publishing.  Called once when the filter is created so the output media
/// type matches the session before the graph enumerates pins.
pub fn discover_geometry() -> Option<StreamGeometry> {
    let mapping = ReadMapping::open(&frame_map_name())?;
    mapping.geometry()
}

/// Default geometry remains the 640x640@240 v2 contract.
pub fn default_geometry() -> StreamGeometry {
    StreamGeometry::new(DEFAULT_WIDTH, DEFAULT_HEIGHT, crate::geometry::DEFAULT_FPS)
        .unwrap_or(StreamGeometry::DEFAULT)
}

// ---------- writable mapping + event handles ----------

pub struct ShmSet {
    geometry: StreamGeometry,
    data_map: HANDLE,
    data_view: *mut u8,
    events: [HANDLE; 2],
    vstat_map: HANDLE,
    vstat_view: *mut u8,
}

// Raw view pointers are used by the single worker thread and dropped only
// after that thread is joined; COM-style access is serialized by FilterCore.
unsafe impl Send for ShmSet {}
unsafe impl Sync for ShmSet {}

impl ShmSet {
    /// Opens the receiver mapping for `geometry`.  Fails (rather than
    /// adapting) when the published session geometry differs: the media type
    /// is immutable for a filter instance, so the device must be reopened.
    pub fn open(name: &str, geometry: StreamGeometry) -> Option<Self> {
        let mapping = ReadMapping::open(name)?;
        if mapping.geometry()? != geometry {
            return None;
        }
        let data_map = mapping.map;
        let data_view = mapping.base as *mut u8;
        core::mem::forget(mapping); // ownership moves into ShmSet

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

    /// `evt[0]`/`evt[1]`; a null handle means "not open" (poll instead).
    pub fn event(&self, index: usize) -> HANDLE {
        self.events.get(index).copied().unwrap_or_default()
    }

    pub fn vstat(&self) -> *mut u8 {
        self.vstat_view
    }

    /// Latest stable token, read-only seqlock peek.
    pub fn peek(&self) -> Option<(u32, i32)> {
        unsafe { shm::peek(self.data_view) }
    }

    pub fn slot(&self, slot: u32) -> *mut ShmSlot {
        unsafe { shm::slot_ptr(self.data_view, slot) }
    }

    /// Stable seqlock copy of one published slot.
    ///
    /// # Safety
    ///
    /// `dst` must point to at least `hdr.frame_bytes` writable bytes that do
    /// not overlap the mapped SHM region; `slot`/`seq` must come from
    /// [`ShmSet::peek`] and the mapping must stay alive for the call.
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

// ---------- timing ----------

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

// ---------- worker thread scheduling ----------

/// Legacy worker policy (`vcam/vcam.cpp:715-721`, reverted at `825-827`):
/// raise the timer period to 1 ms so the 1 ms eventless poll and the 1.5-frame
/// repeat deadline are honoured, and put the worker into the MMCSS "Capture"
/// task; when MMCSS is unavailable, raise only this worker thread to
/// `THREAD_PRIORITY_HIGHEST` (the host process priority is never touched).
///
/// The guard reverts the MMCSS handle and the timer period on every worker
/// exit path, including early `break`/error returns.
pub struct WorkerScheduling {
    mmcss: Option<HANDLE>,
    /// Previous thread priority when the `HIGHEST` fallback was applied;
    /// MMCSS restores its own priority when the handle is reverted.
    previous_priority: Option<i32>,
}

/// `GetThreadPriority` returns this on failure.
const PRIORITY_ERROR: i32 = i32::MAX;

impl WorkerScheduling {
    /// Must be called on the worker thread itself: both MMCSS characteristics
    /// and the thread-priority fallback apply to the calling thread.
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

// ---------- module path (registration) ----------

/// Full path of this DLL, used as InprocServer32 data.  Uses the address of
/// a function inside this crate to find its own module.
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

// ---------- tiny conversions ----------

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
            // MMCSS unavailable: the HIGHEST fallback must be applied only to this thread.
            let during = unsafe { GetThreadPriority(GetCurrentThread()) };
            assert_eq!(during, THREAD_PRIORITY_HIGHEST.0);
        }
        drop(first);
        if fallback && before != PRIORITY_ERROR {
            let after = unsafe { GetThreadPriority(GetCurrentThread()) };
            assert_eq!(after, before, "HIGHEST fallback restores the old priority");
        }
        // A second round-trip also checks that timer period calls stay paired.
        drop(WorkerScheduling::raise());
    }
}
