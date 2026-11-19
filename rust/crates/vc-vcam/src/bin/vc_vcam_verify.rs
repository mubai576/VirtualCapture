//! vc-vcam-verify.exe —— `vc_vcam.dll` 的隔离 DirectShow 验收器（Rust 移植）。
//!
//! 行为与历史 `tools/vcam_verify/vcam_verify.cpp` 的 `--dll` 模式一致：
//!
//! * `--dll <vc_vcam.dll> --capabilities`：只读 `LoadLibrary` 后读取
//!   `VCStreamCapabilities` 导出并打印，不创建任何 COM 对象、不写注册表；
//! * `--dll <vc_vcam.dll> [--shm <映射名>] ...`：进程内调用
//!   `DllGetClassObject`（CLSID `{B84C7E21-4A5F-4E2B-9C11-5A3D7F01229A}`），
//!   用自己的 `IPin`/`IMemInputPin` 接收端接上滤镜输出 pin，跑私有
//!   FilterGraph，逐帧 FNV-1a64 对账并检查 `IMediaSample` 时间戳，
//!   最后 Stop/Disconnect，确认 `DllCanUnloadNow()==S_OK` 后 `FreeLibrary`。
//!
//! 这里**不做**注册、枚举或访问任何已注册滤镜：未提供 `--dll` 时直接报错。
//! 这样验收只触碰显式给出的 DLL 与私有 SHM 映射，不会误连现用设备。
//!
//! 用法与退出码：
//!
//! ```text
//! vc-vcam-verify --dll <vc_vcam.dll> --capabilities
//! vc-vcam-verify --dll <vc_vcam.dll> [--shm <Local\映射名>] [--width N]
//!                [--height N] [--fps N] [--seconds N] [--hashlog <path>]
//!                [--allow-empty] [--block-receive] [--hold-samples]
//!                [--disconnect-running]
//! ```
//!
//! 0 = PASS，1 = FAIL（含卸载未归零），2 = 参数/用法错误。
//!
//! 输出格式被 `rust/tests/vcam_dll_smoke.py` 用正则锁定，请勿改动：
//!
//! ```text
//! Stop/Disconnect 0.741 ms block_receive=0 hold_samples=0 disconnect_running=0
//! DLL stream 640x640@240 samples=483 invalid=0 hr=0x00000000 PASS
//! ```
//!
//! 哈希日志每行一个 JSON 对象（字段顺序与 C++ 相同）：
//!
//! ```text
//! {"sample":0,"hash":"de7e7dc4af76c5aa","start_100ns":8791526}
//! ```

#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::ffi::{c_char, c_void};
use core::ptr;
use std::env;
use std::ffi::{CStr, OsStr, OsString};
use std::fs::File;
use std::io::Write;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, FreeLibrary, GetLastError, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::DirectShow::{
    IBaseFilter, IGraphBuilder, IMediaSample, IMemAllocator, IMemInputPin, IPin,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_FilterGraph, CLSID_MemoryAllocator, MEDIASUBTYPE_NV12,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    IClassFactory,
};
use windows::Win32::System::Environment::SetEnvironmentVariableW;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, Sleep, WaitForSingleObject,
};
use windows::core::{GUID, HRESULT, IUnknown, Interface, PCSTR, PCWSTR};

use vc_vcam::abi::{
    AllocatorPropertiesRaw, CLSID_VCAM, E_FAIL, E_NOINTERFACE, E_NOTIMPL, E_POINTER, IID_IUNKNOWN,
    IPinVtbl, IUnknownVtbl, MediaType, PINDIR_INPUT, PinInfoRaw, S_FALSE, S_OK,
    VFW_E_NOT_CONNECTED, VFW_E_TYPE_NOT_ACCEPTED, VideoInfoHeader, com_addref, com_release, failed,
    succeeded,
};

// ---------------------------------------------------------------------------
// 冻结FNV-1a64；小函数内联保留，避免隔离验证器依赖产品vc-transport。

/// 整帧 FNV-1a64：初值 0xcbf29ce484222325，质数 0x100000001b3。
fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// 命令行

struct Args {
    dll: PathBuf,
    map: String,
    hashlog: PathBuf,
    width: i32,
    height: i32,
    fps: i32,
    seconds: i32,
    capabilities: bool,
    allow_empty: bool,
    block_receive: bool,
    hold_samples: bool,
    disconnect_running: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            dll: PathBuf::new(),
            map: String::new(),
            hashlog: PathBuf::new(),
            width: 640,
            height: 640,
            fps: 240,
            seconds: 2,
            capabilities: false,
            allow_empty: false,
            block_receive: false,
            hold_samples: false,
            disconnect_running: false,
        }
    }
}

/// C++ `vc_cli_int`：只接受可选 `-` 加十进制数字；越界或非法直接 exit(2)。
fn cli_int(value: &OsStr) -> i32 {
    let text = value.to_string_lossy();
    let digits = text.strip_prefix('-').unwrap_or(&text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        eprintln!("invalid integer: {text}");
        std::process::exit(2);
    }
    match text.parse::<i64>() {
        Ok(number) if number >= i32::MIN as i64 && number <= i32::MAX as i64 => number as i32,
        // strtoll 的 ERANGE 与逐位解析失败在 C++ 里都报 out of range。
        Ok(_) | Err(_) => {
            eprintln!("integer out of range");
            std::process::exit(2);
        }
    }
}

fn next_value(args: &mut impl Iterator<Item = OsString>) -> OsString {
    match args.next() {
        Some(value) => value,
        None => {
            eprintln!("missing value");
            std::process::exit(2);
        }
    }
}

fn parse_args() -> Args {
    let mut parsed = Args::default();
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        match text.as_str() {
            "--dll" => parsed.dll = PathBuf::from(next_value(&mut args)),
            "--shm" => parsed.map = next_value(&mut args).to_string_lossy().into_owned(),
            "--hashlog" => parsed.hashlog = PathBuf::from(next_value(&mut args)),
            "--width" => parsed.width = cli_int(&next_value(&mut args)),
            "--height" => parsed.height = cli_int(&next_value(&mut args)),
            "--fps" => parsed.fps = cli_int(&next_value(&mut args)),
            "--seconds" => parsed.seconds = cli_int(&next_value(&mut args)),
            "--capabilities" => parsed.capabilities = true,
            "--allow-empty" => parsed.allow_empty = true,
            "--block-receive" => parsed.block_receive = true,
            "--hold-samples" => parsed.hold_samples = true,
            "--disconnect-running" => parsed.disconnect_running = true,
            other => {
                eprintln!("unknown arg {other}");
                std::process::exit(2);
            }
        }
    }
    parsed
}

/// 与 `VCStream` 的 `valid()` 相同；验证器不暴露 `--transform`，恒为 crop。
fn stream_valid(width: i32, height: i32, fps: i32) -> bool {
    width >= 2
        && height >= 2
        && width <= 8192
        && height <= 8192
        && width % 2 == 0
        && height % 2 == 0
        && (1..=1000).contains(&fps)
}

fn wide_os(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(core::iter::once(0)).collect()
}

/// 取 DLL 导出并转成目标函数指针类型。
///
/// # Safety
/// 调用方必须保证 `T` 与 `name` 在目标 DLL 中的真实导出签名一致；签名不符会
/// 产生未定义行为。这里只用于本文件声明的三个固定导出。
unsafe fn export_fn<T: Copy>(
    module: windows::Win32::Foundation::HMODULE,
    name: PCSTR,
) -> Option<T> {
    let address = GetProcAddress(module, name)?;
    Some(mem::transmute_copy(&address))
}

// ---------------------------------------------------------------------------
// 接收端 COM 对象
//
// C++ 的 `VerifySink` 用多继承同时暴露 IPin 和 IMemInputPin。DirectShow 对
// 这两个接口分别读取各自的 vtable（IPin 槽 3 是 Connect，IMemInputPin 槽 3
// 是 GetAllocator），因此不能共用一张表。这里沿用 crate 的 tear-off 方案：
// 每个接口是 `{ vtbl, owner }` 两指针结构，owner 在偏移 1，任一接口指针都能
// 还原对象；引用计数归对象本体所有。
//
// 线程模型：`Receive`/`ReceiveMultiple` 由 DLL 的采集 worker 线程调用；
// `BeginFlush`/`EndFlush` 由主线程在 Stop/Disconnect 时调用，两者可能并发。
// samples/invalid 用原子计数；held 与 hashlog 用 Mutex 保护；`flushed` 事件
// 用于唤醒阻塞中的 worker。所有 COM 回调都是 `unsafe extern "system"`，
// 入参指针只在调用期间有效，对象本身靠引用计数保证存活。

#[repr(C)]
struct SinkPin {
    vtbl: *const IPinVtbl,
    owner: *mut Sink,
}

/// `IMemInputPin` 的原始 vtable（strmif.h 顺序，紧随 IUnknown 三槽之后）。
#[repr(C)]
struct IMemInputPinVtbl {
    base__: IUnknownVtbl,
    GetAllocator: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    NotifyAllocator: unsafe extern "system" fn(*mut c_void, *mut c_void, i32) -> HRESULT,
    GetAllocatorRequirements:
        unsafe extern "system" fn(*mut c_void, *mut AllocatorPropertiesRaw) -> HRESULT,
    Receive: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    ReceiveMultiple:
        unsafe extern "system" fn(*mut c_void, *const *mut c_void, i32, *mut i32) -> HRESULT,
    ReceiveCanBlock: unsafe extern "system" fn(*mut c_void) -> HRESULT,
}

#[repr(C)]
struct SinkMem {
    vtbl: *const IMemInputPinVtbl,
    owner: *mut Sink,
}

#[repr(C)]
struct Sink {
    pin: SinkPin,
    mem: SinkMem,
    refs: AtomicU32,
    width: i32,
    height: i32,
    fps: i32,
    frame_bytes: usize,
    frame_100ns: i64,
    hashes: Mutex<Option<File>>,
    flushed: HANDLE,
    block_receive: bool,
    hold_samples: bool,
    held: Mutex<Vec<*mut c_void>>,
    samples: AtomicU64,
    invalid: AtomicU64,
}

// 对象会被主线程与 DLL worker 共享：原子/Mutex 字段自行同步；held 中的
// IMediaSample* 只在该互斥量内增删，且每个都持有 AddRef，故可跨线程转移。
// `flushed` 是内核 HANDLE，可在任意线程 SetEvent/ResetEvent/Wait。
unsafe impl Send for Sink {}
unsafe impl Sync for Sink {}

static SINK_PIN_VTBL: IPinVtbl = IPinVtbl {
    base__: IUnknownVtbl {
        QueryInterface: sink_query_interface,
        AddRef: sink_addref,
        Release: sink_release,
    },
    Connect: sink_connect,
    ReceiveConnection: sink_receive_connection,
    Disconnect: sink_disconnect,
    ConnectedTo: sink_connected_to,
    ConnectionMediaType: sink_connection_media_type,
    QueryPinInfo: sink_query_pin_info,
    QueryDirection: sink_query_direction,
    QueryId: sink_query_id,
    QueryAccept: sink_query_accept,
    EnumMediaTypes: sink_enum_media_types,
    QueryInternalConnections: sink_query_internal_connections,
    EndOfStream: sink_end_of_stream,
    BeginFlush: sink_begin_flush,
    EndFlush: sink_end_flush,
    NewSegment: sink_new_segment,
};

static SINK_MEM_VTBL: IMemInputPinVtbl = IMemInputPinVtbl {
    base__: IUnknownVtbl {
        QueryInterface: sink_query_interface,
        AddRef: sink_addref,
        Release: sink_release,
    },
    GetAllocator: sink_get_allocator,
    NotifyAllocator: sink_notify_allocator,
    GetAllocatorRequirements: sink_get_allocator_requirements,
    Receive: sink_receive,
    ReceiveMultiple: sink_receive_multiple,
    ReceiveCanBlock: sink_receive_can_block,
};

impl Drop for Sink {
    fn drop(&mut self) {
        // 对象即将销毁时已无 COM 调用在途（引用计数归零），但 BeginFlush 之后
        // 仍可能留有持住的 sample；这里兜底释放，避免泄漏分配器缓冲区。
        let held = match self.held.get_mut() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for sample in held.drain(..) {
            unsafe { com_release(sample) };
        }
        if !self.flushed.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.flushed);
            }
        }
    }
}

fn sink_owner(this: *mut c_void) -> *mut Sink {
    if this.is_null() {
        return ptr::null_mut();
    }
    // tear-off 布局固定为 { vtable, owner }；偏移 1 处即对象本体。
    unsafe { *((this as *mut *mut c_void).add(1)) as *mut Sink }
}

fn sink_lock_held(sink: &Sink) -> MutexGuard<'_, Vec<*mut c_void>> {
    match sink.held.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn sink_addref_impl(sink: *mut Sink) -> u32 {
    unsafe { (*sink).refs.fetch_add(1, Ordering::SeqCst) + 1 }
}

fn sink_release_impl(sink: *mut Sink) -> u32 {
    let previous = unsafe { (*sink).refs.fetch_sub(1, Ordering::SeqCst) };
    let remaining = previous.saturating_sub(1);
    if remaining == 0 {
        // 最后一个 COM 引用：销毁对象（Drop 释放 held/hashlog/flushed）。
        unsafe { drop(Box::from_raw(sink)) };
    }
    remaining
}

unsafe fn sink_new(
    width: i32,
    height: i32,
    fps: i32,
    hashlog: &Path,
    block_receive: bool,
    hold_samples: bool,
) -> *mut Sink {
    // C++ 用 std::ofstream(path, binary)；打开失败时静默降级为“不写哈希”。
    let hashes = if hashlog.as_os_str().is_empty() {
        None
    } else {
        File::create(hashlog).ok()
    };
    // C++ 未检查 CreateEvent 失败；失败时保持空句柄，后续等待会立即返回。
    let flushed = CreateEventW(None, true, false, PCWSTR::null()).unwrap_or_default();
    let frame_bytes = width as usize * height as usize * 3 / 2;
    let boxed = Box::new(Sink {
        pin: SinkPin {
            vtbl: &SINK_PIN_VTBL,
            owner: ptr::null_mut(),
        },
        mem: SinkMem {
            vtbl: &SINK_MEM_VTBL,
            owner: ptr::null_mut(),
        },
        refs: AtomicU32::new(1), // 调用方持有的一枚引用
        width,
        height,
        fps,
        frame_bytes,
        frame_100ns: 10_000_000i64 / fps as i64,
        hashes: Mutex::new(hashes),
        flushed,
        block_receive,
        hold_samples,
        held: Mutex::new(Vec::new()),
        samples: AtomicU64::new(0),
        invalid: AtomicU64::new(0),
    });
    let raw = Box::into_raw(boxed);
    // Box 落地后地址才固定，此时再写回 self 指针。
    unsafe {
        (*raw).pin.owner = raw;
        (*raw).mem.owner = raw;
    }
    raw
}

unsafe extern "system" fn sink_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = ptr::null_mut();
    if riid.is_null() {
        return E_POINTER;
    }
    let sink = sink_owner(this);
    if sink.is_null() {
        return E_POINTER;
    }
    let iid = *riid;
    let interface = if iid == IID_IUNKNOWN || iid == <IPin as Interface>::IID {
        &(*sink).pin as *const SinkPin as *mut c_void
    } else if iid == <IMemInputPin as Interface>::IID {
        &(*sink).mem as *const SinkMem as *mut c_void
    } else {
        return E_NOINTERFACE;
    };
    *out = interface;
    sink_addref_impl(sink);
    S_OK
}

unsafe extern "system" fn sink_addref(this: *mut c_void) -> u32 {
    let sink = sink_owner(this);
    if sink.is_null() {
        return 0;
    }
    sink_addref_impl(sink)
}

unsafe extern "system" fn sink_release(this: *mut c_void) -> u32 {
    let sink = sink_owner(this);
    if sink.is_null() {
        return 0;
    }
    sink_release_impl(sink)
}

unsafe extern "system" fn sink_connect(
    _this: *mut c_void,
    _peer: *mut c_void,
    _media_type: *const MediaType,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn sink_receive_connection(
    this: *mut c_void,
    _peer: *mut c_void,
    media_type: *const MediaType,
) -> HRESULT {
    let sink = sink_owner(this);
    if sink.is_null() || media_type.is_null() {
        return VFW_E_TYPE_NOT_ACCEPTED;
    }
    let sink = &*sink;
    let media_type = &*media_type;
    if media_type.cbFormat < mem::size_of::<VideoInfoHeader>() as u32
        || media_type.pbFormat.is_null()
    {
        return VFW_E_TYPE_NOT_ACCEPTED;
    }
    let video = &*(media_type.pbFormat as *const VideoInfoHeader);
    let matches = video.bmiHeader.biWidth == sink.width
        && video.bmiHeader.biHeight == sink.height
        && video.bmiHeader.biSizeImage as usize == sink.frame_bytes
        && video.AvgTimePerFrame == sink.frame_100ns
        && media_type.subtype == MEDIASUBTYPE_NV12;
    if matches {
        S_OK
    } else {
        VFW_E_TYPE_NOT_ACCEPTED
    }
}

unsafe extern "system" fn sink_disconnect(_this: *mut c_void) -> HRESULT {
    S_OK
}

unsafe extern "system" fn sink_connected_to(_this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if !out.is_null() {
        *out = ptr::null_mut();
    }
    VFW_E_NOT_CONNECTED
}

unsafe extern "system" fn sink_connection_media_type(
    _this: *mut c_void,
    _media_type: *mut MediaType,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn sink_query_pin_info(
    _this: *mut c_void,
    info: *mut PinInfoRaw,
) -> HRESULT {
    if info.is_null() {
        return E_POINTER;
    }
    // C++ ZeroMemory 后只填方向，pFilter 保持 null（sink 不属于任何滤镜）。
    ptr::write_bytes(info as *mut u8, 0, mem::size_of::<PinInfoRaw>());
    (*info).dir = PINDIR_INPUT;
    S_OK
}

unsafe extern "system" fn sink_query_direction(_this: *mut c_void, direction: *mut i32) -> HRESULT {
    if direction.is_null() {
        return E_POINTER;
    }
    *direction = PINDIR_INPUT;
    S_OK
}

unsafe extern "system" fn sink_query_id(_this: *mut c_void, _id: *mut *mut u16) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn sink_query_accept(
    this: *mut c_void,
    media_type: *const MediaType,
) -> HRESULT {
    sink_receive_connection(this, ptr::null_mut(), media_type)
}

unsafe extern "system" fn sink_enum_media_types(
    _this: *mut c_void,
    _enum: *mut *mut c_void,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn sink_query_internal_connections(
    _this: *mut c_void,
    _pins: *mut *mut c_void,
    _count: *mut u32,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn sink_end_of_stream(_this: *mut c_void) -> HRESULT {
    S_OK
}

unsafe extern "system" fn sink_begin_flush(this: *mut c_void) -> HRESULT {
    let sink = sink_owner(this);
    if sink.is_null() {
        return E_POINTER;
    }
    let sink = &*sink;
    // 先置事件再拿锁：阻塞在 Receive 末尾的 worker 立刻被唤醒，不会与这里
    // 争 held 锁；持住的 allocator sample 必须在本线程释放，释放后滤镜才能
    // Decommit 并 join。
    let _ = SetEvent(sink.flushed);
    let mut held = sink_lock_held(sink);
    for sample in held.drain(..) {
        com_release(sample);
    }
    S_OK
}

unsafe extern "system" fn sink_end_flush(this: *mut c_void) -> HRESULT {
    let sink = sink_owner(this);
    if sink.is_null() {
        return E_POINTER;
    }
    let _ = ResetEvent((*sink).flushed);
    S_OK
}

unsafe extern "system" fn sink_new_segment(
    _this: *mut c_void,
    _start: i64,
    _stop: i64,
    _rate: f64,
) -> HRESULT {
    S_OK
}

// ---------- IMemInputPin ----------

unsafe extern "system" fn sink_get_allocator(_this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = ptr::null_mut();
    // 与 C++ 一致：让系统内存分配器分配 sample，滤镜通过 SetProperties 设置
    // 2 个 frame_bytes 缓冲；sink 不自己持有分配器引用。
    let created: windows::core::Result<IMemAllocator> = CoCreateInstance(
        &CLSID_MemoryAllocator,
        None::<&IUnknown>,
        CLSCTX_INPROC_SERVER,
    );
    match created {
        Ok(allocator) => {
            *out = allocator.into_raw();
            S_OK
        }
        Err(error) => error.code(),
    }
}

unsafe extern "system" fn sink_notify_allocator(
    _this: *mut c_void,
    _allocator: *mut c_void,
    _read_only: i32,
) -> HRESULT {
    S_OK
}

unsafe extern "system" fn sink_get_allocator_requirements(
    _this: *mut c_void,
    _properties: *mut AllocatorPropertiesRaw,
) -> HRESULT {
    E_NOTIMPL
}

unsafe extern "system" fn sink_receive(this: *mut c_void, sample_ptr: *mut c_void) -> HRESULT {
    let sink = sink_owner(this);
    if sink.is_null() {
        return E_FAIL;
    }
    let sink = &*sink;
    if sample_ptr.is_null() {
        sink.invalid.fetch_add(1, Ordering::SeqCst);
        return E_FAIL;
    }
    // 借用滤镜/分配器持有的 sample，不在这里 AddRef；调用期间由调用方保证有效。
    let Some(sample) = IMediaSample::from_raw_borrowed(&sample_ptr) else {
        sink.invalid.fetch_add(1, Ordering::SeqCst);
        return E_FAIL;
    };
    if sample.GetActualDataLength() != sink.frame_bytes as i32 {
        sink.invalid.fetch_add(1, Ordering::SeqCst);
        return E_FAIL;
    }
    let pointer = match sample.GetPointer() {
        Ok(pointer) if !pointer.is_null() => pointer,
        _ => {
            sink.invalid.fetch_add(1, Ordering::SeqCst);
            return E_FAIL;
        }
    };
    let frame = std::slice::from_raw_parts(pointer, sink.frame_bytes);
    let hash = fnv1a64(frame);
    let mut start: i64 = 0;
    let mut end: i64 = 0;
    // C++ 忽略 GetTime 的失败；失败时 start/end 保持 0，随后按时间戳判 invalid。
    let _ = sample.GetTime(&mut start, &mut end);
    if end - start != sink.frame_100ns {
        sink.invalid.fetch_add(1, Ordering::SeqCst);
    }
    if let Some(file) = sink
        .hashes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_mut()
    {
        // 字段顺序/格式与 C++ 完全一致；sample 是本帧交付前的计数。
        let _ = writeln!(
            file,
            "{{\"sample\":{},\"hash\":\"{:016x}\",\"start_100ns\":{}}}",
            sink.samples.load(Ordering::SeqCst),
            hash,
            start
        );
    }
    if sink.hold_samples {
        // 模拟下游“持有 sample 不归还”：AddRef 后留存，BeginFlush 统一释放。
        let mut held = sink_lock_held(sink);
        if WaitForSingleObject(sink.flushed, 0) != WAIT_OBJECT_0 {
            com_addref(sample_ptr);
            held.push(sample_ptr);
        }
    }
    sink.samples.fetch_add(1, Ordering::SeqCst);
    if sink.block_receive {
        // 模拟卡死的下游：Stop 负责 BeginFlush 置事件，worker 必须能退出。
        let _ = WaitForSingleObject(sink.flushed, INFINITE);
    }
    S_OK
}

unsafe extern "system" fn sink_receive_multiple(
    this: *mut c_void,
    samples: *const *mut c_void,
    count: i32,
    done: *mut i32,
) -> HRESULT {
    if done.is_null() {
        return E_POINTER;
    }
    *done = 0;
    if count <= 0 || samples.is_null() {
        return S_OK;
    }
    for index in 0..count {
        let sample = *samples.add(index as usize);
        let hr = sink_receive(this, sample);
        if failed(hr) {
            return hr;
        }
        *done += 1;
    }
    S_OK
}

unsafe extern "system" fn sink_receive_can_block(this: *mut c_void) -> HRESULT {
    let sink = sink_owner(this);
    if sink.is_null() {
        return S_FALSE;
    }
    if (*sink).block_receive { S_OK } else { S_FALSE }
}

// ---------------------------------------------------------------------------
// 主流程

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
type DllCanUnloadNowFn = unsafe extern "system" fn() -> HRESULT;
type CapabilitiesFn = unsafe extern "C" fn() -> *const c_char;

fn print_capabilities(capabilities: CapabilitiesFn) {
    // 指针由 DLL 静态持有，直到 FreeLibrary 都有效；只读不改。
    let text = unsafe { CStr::from_ptr(capabilities()) };
    println!("{}", text.to_string_lossy());
}

/// `--dll` 模式的完整生命周期；返回进程退出码。
///
/// # Safety
///
/// 本函数会 `LoadLibrary` 调用方给出的 DLL 并执行其中的 COM 导出。DLL 必须
/// 是可信的 `vc_vcam.dll`；参数已在入口校验。
unsafe fn run_dll(args: &Args) -> u8 {
    let absolute = std::path::absolute(&args.dll).unwrap_or_else(|_| args.dll.clone());
    let dll_wide = wide_os(absolute.as_os_str());
    let module = match LoadLibraryW(PCWSTR(dll_wide.as_ptr())) {
        Ok(module) => module,
        Err(_) => {
            eprintln!("LoadLibrary failed: {}", GetLastError().0);
            return 1;
        }
    };

    let Some(capabilities) =
        export_fn::<CapabilitiesFn>(module, windows::core::s!("VCStreamCapabilities"))
    else {
        eprintln!("DLL lacks P6 stream capabilities");
        let _ = FreeLibrary(module);
        return 1;
    };
    if args.capabilities {
        print_capabilities(capabilities);
        let _ = FreeLibrary(module);
        return 0;
    }

    // C++ 原样把 --shm 值写入环境变量（不做绝对化）；Rust 滤镜按此名称打开
    // 私有映射。未给 --shm 时保留父进程环境，便于诊断。
    let map_wide = wide_os(OsStr::new(&args.map));
    if !args.map.is_empty() {
        let _ = SetEnvironmentVariableW(
            windows::core::w!("VCAM_FRAME_MAP"),
            PCWSTR(map_wide.as_ptr()),
        );
    }

    // 与 C++ 相同：MTA，忽略 CoInitializeEx 的返回值。
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

    let get_class_object =
        export_fn::<DllGetClassObjectFn>(module, windows::core::s!("DllGetClassObject"));
    let mut factory_raw: *mut c_void = ptr::null_mut();
    let mut hr = match get_class_object {
        Some(get) => get(
            &CLSID_VCAM,
            &<IClassFactory as Interface>::IID,
            &mut factory_raw,
        ),
        None => E_FAIL,
    };
    if succeeded(hr) && factory_raw.is_null() {
        hr = E_FAIL;
    }

    let mut filter: Option<IBaseFilter> = None;
    if succeeded(hr) {
        // DllGetClassObject 成功即返回一枚工厂引用，由 windows 包装接管并在
        // drop 时 Release；CreateInstance 成功再得到滤镜引用。
        let factory = IClassFactory::from_raw(factory_raw);
        let created: windows::core::Result<IBaseFilter> = factory.CreateInstance(None::<&IUnknown>);
        drop(factory);
        match created {
            Ok(value) => filter = Some(value),
            Err(error) => hr = error.code(),
        }
    }

    let mut graph: Option<IGraphBuilder> = None;
    if succeeded(hr) {
        let created: windows::core::Result<IGraphBuilder> =
            CoCreateInstance(&CLSID_FilterGraph, None::<&IUnknown>, CLSCTX_INPROC_SERVER);
        match created {
            Ok(value) => graph = Some(value),
            Err(error) => hr = error.code(),
        }
    }

    if succeeded(hr) {
        match graph.as_ref().expect("graph created above").AddFilter(
            filter.as_ref().expect("filter created above"),
            windows::core::w!("Isolated VirtualCapture"),
        ) {
            Ok(()) => {}
            Err(error) => hr = error.code(),
        }
    }

    let mut pin: Option<IPin> = None;
    if succeeded(hr) {
        match filter
            .as_ref()
            .expect("filter created above")
            .FindPin(windows::core::w!("Capture"))
        {
            Ok(value) => pin = Some(value),
            Err(error) => hr = error.code(),
        }
    }

    // C++ 在 FindPin 之后、Connect 之前无条件创建 sink（即使前面失败也会
    // 打开/截断 hashlog）。保持同样顺序，便于复现证据文件。
    let sink = sink_new(
        args.width,
        args.height,
        args.fps,
        &args.hashlog,
        args.block_receive,
        args.hold_samples,
    );

    if succeeded(hr) {
        // 把 sink 的 IPin tear-off 借用给滤镜 pin->Connect；滤镜会 QueryInterface
        // 出 IMemInputPin 并各持一枚引用，sink 初始引用仍归本线程。
        let sink_pin_raw = &(*sink).pin as *const SinkPin as *mut c_void;
        let sink_pin = IPin::from_raw_borrowed(&sink_pin_raw).expect("sink pin is non-null");
        match pin
            .as_ref()
            .expect("pin found above")
            .Connect(sink_pin, None)
        {
            Ok(()) => {}
            Err(error) => hr = error.code(),
        }
    }

    if succeeded(hr) {
        match filter.as_ref().expect("filter created above").Run(0) {
            Ok(()) => {}
            Err(error) => hr = error.code(),
        }
    }
    if succeeded(hr) {
        Sleep(args.seconds as u32 * 1000);
    }

    // Stop/Disconnect 必须在上限内完成：blocked / held-samples / disconnect
    // 三种场景都靠 BeginFlush 唤醒 worker 并释放 sample。
    let stop_start = Instant::now();
    if args.disconnect_running
        && let Some(pin) = &pin
    {
        let _ = pin.Disconnect();
    }
    if let Some(filter) = &filter {
        let _ = filter.Stop();
    }
    let stop_ms = stop_start.elapsed().as_secs_f64() * 1000.0;

    let invalid = (*sink).invalid.load(Ordering::SeqCst);
    let samples = (*sink).samples.load(Ordering::SeqCst);
    let pass =
        succeeded(hr) && invalid == 0 && (args.allow_empty || samples > 0) && stop_ms < 2000.0;
    println!(
        "Stop/Disconnect {:.3} ms block_receive={} hold_samples={} disconnect_running={}",
        stop_ms,
        args.block_receive as i32,
        args.hold_samples as i32,
        args.disconnect_running as i32
    );
    println!(
        "DLL stream {}x{}@{} samples={} invalid={} hr=0x{:08x} {}",
        args.width,
        args.height,
        args.fps,
        samples,
        invalid,
        hr.0 as u32,
        if pass { "PASS" } else { "FAIL" }
    );

    // 释放顺序与 C++ 一致：pin、filter、graph、sink，最后 CoUninitialize。
    // graph 持有滤镜引用，因此 filter 必须在 graph 之前显式释放。
    if let Some(pin) = &pin {
        let _ = pin.Disconnect();
    }
    drop(pin);
    drop(filter);
    drop(graph);
    // 最后一枚 sink 引用归本线程；drop 后对象销毁、hashlog 关闭/刷新。
    sink_release_impl(sink);
    CoUninitialize();

    // 卸载证明：Stop/Disconnect 后 DLL 内部不能再持有 filter/allocator 引用。
    let unload = export_fn::<DllCanUnloadNowFn>(module, windows::core::s!("DllCanUnloadNow"));
    let unload_ok = match unload {
        Some(unload) => unload() == S_OK,
        None => false,
    };
    if !unload_ok {
        eprintln!("DLL retained COM references after Stop/Disconnect");
        let _ = FreeLibrary(module);
        return 1;
    }
    let _ = FreeLibrary(module);
    if pass { 0 } else { 1 }
}

fn main() -> ExitCode {
    let args = parse_args();
    if !stream_valid(args.width, args.height, args.fps) || args.seconds < 1 || args.seconds > 120 {
        return ExitCode::from(2);
    }
    if args.dll.as_os_str().is_empty() {
        // 旧 C++ 会枚举已注册滤镜；Rust 版只做隔离验收，绝不触碰现用设备。
        eprintln!(
            "vc-vcam-verify requires --dll <vc_vcam.dll>; \
             registered-filter enumeration is not supported by this isolated verifier"
        );
        return ExitCode::from(2);
    }
    ExitCode::from(unsafe { run_dll(&args) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_matches_frozen_spec_vectors() {
        assert_eq!(fnv1a64(b""), 0xCBF2_9CE4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xAF63_DC4C_8601_EC8C);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_F739_67E8);
        assert_eq!(format!("{:016x}", fnv1a64(b"foobar")), "85944171f73967e8");
    }

    #[test]
    fn stream_validation_matches_vc_stream() {
        assert!(stream_valid(640, 640, 240));
        assert!(stream_valid(416, 416, 60));
        assert!(stream_valid(2, 2, 1));
        assert!(stream_valid(8192, 8192, 1000));
        assert!(!stream_valid(0, 640, 240));
        assert!(!stream_valid(641, 640, 240));
        assert!(!stream_valid(640, 639, 240));
        assert!(!stream_valid(8194, 640, 240));
        assert!(!stream_valid(640, 640, 0));
        assert!(!stream_valid(640, 640, 1001));
    }
}
