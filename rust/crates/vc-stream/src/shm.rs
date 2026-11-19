//! 冻结 vcam SHM v3 契约的 Windows 共享内存帧发布器。
//!
//! 本模块是该布局的写入端（规范有效布局见 `docs/spec/shm-v3.md`，它取代历史
//! `vcam/vcam_shm.h`；`proto/` 不是该布局的真相源）。它创建一个命名页文件
//! 映射和旧的 manual-reset 事件对，然后以历史 C++ 接收端单写者相同的 12 槽
//! seqlock 协议发布完整 NV12 帧：
//!
//! ```text
//! [0, 64)                         vc_shm_hdr_t
//! [64, 64 + 64 * 12)              vc_shm_slot_t[12]
//! [4096 + i * frame_bytes, ...)   槽 i 的完整帧数据
//! ```
//!
//! 发布协议（只有一个写线程；每个方法都取 `&mut self`）：
//! - 按轮转顺序挑选下一个槽，绝不选当前 `latest` 槽；
//! - 把 `slot.seq` 递增为奇数（填充中），写入无哈希标记，拷贝帧数据；
//! - 填充 `frame_id`/`qpc_*`，把 `slot.seq` 递增为偶数（就绪），写入
//!   `hdr.latest_slot`，递增 `hdr.latest_seq`，然后
//!   `ResetEvent(evt[(n+1)&1]); SetEvent(evt[n&1])`；
//! - 在延迟敏感路径之后，[`Publisher::mark_hash`] 先写 `slot.hash`，再写
//!   `slot.hash_fid = frame_id`。若令牌对应的槽已被重新发布，该令牌被忽略，
//!   因此迟到的哈希器绝不会写进更新的帧。
//!
//! 读取方（vcam/monitor/验证器）只把 `latest_seq` 当作唤醒提示，用
//! `(latest_slot, slot.seq)` 标识一次发布，并在拷贝前后复查 `slot.seq`。
//! 仅当 `hash_fid == frame_id` 时，`slot.hash` 才对该次拷贝有效。事件对是
//! manual-reset 且会话本地的；漏掉的边沿由读取方的超时兜底。
//!
//! 映射以 `PAGE_READWRITE` 和调用方提供的名称创建（会话私有对象应使用
//! `Local\...`）。同名映射已存在即硬错误：本类型绝不打开、重置或以其他方式
//! 干扰另一个写入端。事件可能因下游读取方仍持有句柄而由前一写入端遗留；与
//! 旧写入端一致，我们创建/打开它们，把两者重置后继续。不涉及注册表、服务、
//! 部署或网络状态。

#![cfg(windows)]

use std::io;
use std::mem::{offset_of, size_of};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering, fence};

use vc_transport::Geometry;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE,
    UnmapViewOfFile,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, ResetEvent, SetEvent,
};

/// `"VCA3"`，对应 `VC_SHM_MAGIC`。
const MAGIC: u32 = 0x3341_4356;
/// 对应 `VC_SHM_VERSION`。
const VERSION: u32 = 3;
/// 对应 `VC_SHM_SLOTS`。
const SLOT_COUNT: usize = 12;
/// 对应 `VC_SHM_DATA_OFF`。
const DATA_OFF: usize = 4096;
/// `vc_shm_hdr_t` 的大小（`sizeof(vc_shm_hdr_t)`）。
const HEADER_SIZE: usize = 64;
/// `vc_shm_slot_t` 的大小（`sizeof(vc_shm_slot_t)`）。
const SLOT_SIZE: usize = 64;
/// 对应 `VC_SHM_HASH_NONE`；同时是旧的“尚未哈希”标记。
const HASH_NONE: u32 = 0xFFFF_FFFF;
/// `vc_shm_valid` 接受 1..=1000。
const MAX_FPS: u32 = 1000;
/// Windows 命名内核对象上限（UTF-16 码元）。
const MAX_OBJECT_NAME: usize = 32_767;

/// `vc_shm_hdr_t`，64 字节。原子字段只由写入端以原子方式访问；读取端通过
/// 旧的普通对齐读取契约观察它们。
#[repr(C)]
struct ShmHeader {
    magic: u32,
    version: u32,
    slots: u32,
    frame_bytes: u32,
    data_off: u32,
    latest_slot: AtomicI32,
    latest_seq: AtomicI32,
    writer_pid: u32,
    width: u32,
    height: u32,
    fps: u32,
    reserved: [u8; 20],
}

/// `vc_shm_slot_t`，64 字节。`seq` 是 seqlock（奇数 = 填充中，偶数 = 稳定）。
#[repr(C)]
struct ShmSlot {
    seq: AtomicI32,
    frame_id: AtomicU32,
    qpc_first_ns: AtomicU64,
    qpc_pub_ns: AtomicU64,
    hash: AtomicU64,
    hash_fid: AtomicU32,
    reserved: [u8; 28],
}

const _: () = {
    assert!(size_of::<ShmHeader>() == HEADER_SIZE);
    assert!(offset_of!(ShmHeader, magic) == 0);
    assert!(offset_of!(ShmHeader, version) == 4);
    assert!(offset_of!(ShmHeader, slots) == 8);
    assert!(offset_of!(ShmHeader, frame_bytes) == 12);
    assert!(offset_of!(ShmHeader, data_off) == 16);
    assert!(offset_of!(ShmHeader, latest_slot) == 20);
    assert!(offset_of!(ShmHeader, latest_seq) == 24);
    assert!(offset_of!(ShmHeader, writer_pid) == 28);
    assert!(offset_of!(ShmHeader, width) == 32);
    assert!(offset_of!(ShmHeader, height) == 36);
    assert!(offset_of!(ShmHeader, fps) == 40);
    assert!(offset_of!(ShmHeader, reserved) == 44);

    assert!(size_of::<ShmSlot>() == SLOT_SIZE);
    assert!(offset_of!(ShmSlot, seq) == 0);
    assert!(offset_of!(ShmSlot, frame_id) == 4);
    assert!(offset_of!(ShmSlot, qpc_first_ns) == 8);
    assert!(offset_of!(ShmSlot, qpc_pub_ns) == 16);
    assert!(offset_of!(ShmSlot, hash) == 24);
    assert!(offset_of!(ShmSlot, hash_fid) == 32);
    assert!(offset_of!(ShmSlot, reserved) == 36);

    assert!(HEADER_SIZE + SLOT_SIZE * SLOT_COUNT <= DATA_OFF);
};

fn slot_offset(index: usize) -> usize {
    HEADER_SIZE + SLOT_SIZE * index
}

fn mapping_size(frame_bytes: usize) -> usize {
    DATA_OFF + SLOT_COUNT * frame_bytes
}

/// 帧被哈希之前存放在 `hash_fid` 中的标记。
///
/// 原生读取端唯一的有效性判断是 `hash_fid == frame_id`，而旧标记是
/// `HASH_NONE`。当 `frame_id == u32::MAX` 时两者会碰撞。普通帧一律沿用旧
/// 标记，只有那一帧使用 `frame_id ^ HASH_NONE`（零），使旧读取端绝不会把
/// 未哈希的槽当成有效。调用 [`Publisher::mark_hash`] 后写入真实 `frame_id`，
/// 哈希才生效。
fn hash_marker(frame_id: u32) -> u32 {
    if frame_id == HASH_NONE {
        frame_id ^ HASH_NONE
    } else {
        HASH_NONE
    }
}

fn wide_name(name: &str) -> io::Result<Vec<u16>> {
    if name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SHM object name must not be empty",
        ));
    }
    if name.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SHM object name must not contain NUL",
        ));
    }
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    if wide.len() >= MAX_OBJECT_NAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("SHM object name is longer than {MAX_OBJECT_NAME} UTF-16 units"),
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn last_os_error(operation: &str) -> io::Error {
    // 必须在任何其他可能覆盖线程 last-error 的 Win32 调用之前调用（尤其是
    // CloseHandle 之前）。
    let code = unsafe { GetLastError() };
    let detail = io::Error::from_raw_os_error(code as i32);
    io::Error::new(
        detail.kind(),
        format!("{operation} failed (Win32 {code}): {detail}"),
    )
}

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// 标识一次已发布槽的“代”。
///
/// 该令牌是自有（owning）的 `Copy` 值，不借用发布器，因此可以移到哈希线程，
/// 之后传回 [`Publisher::mark_hash`]。它只对返回它的发布器有意义；来自其他
/// 发布器的令牌（以及来自已复用槽的令牌）都会被忽略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublishToken {
    owner: u64,
    slot: u32,
    seq: u32,
    frame_id: u32,
    serial: u64,
}

impl PublishToken {
    /// 该令牌所指发布的帧 id。
    pub fn frame_id(&self) -> u32 {
        self.frame_id
    }
}

/// 单写者 SHM 发布器。
///
/// 有意不设内部写锁：发布与 [`Publisher::mark_hash`] 由 `&mut self` 串行化。
/// 不要克隆或共享写入端；槽表与事件奇偶性都归本对象所有。
pub struct Publisher {
    map: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    events: [HANDLE; 2],
    geometry: Geometry,
    frame_bytes: usize,
    fps: u32,
    /// 下一个尝试的槽，实现轮转复用。
    cursor: usize,
    /// `hdr.latest_slot` 当前暴露的槽；绝不再被选来复用。
    latest: Option<usize>,
    /// 每槽的发布代号（进程本地），即使 32 位在线序号回绕也能拒绝过期令牌。
    serials: [u64; SLOT_COUNT],
    published: u64,
    owner: u64,
}

impl Publisher {
    /// 创建命名映射以及 `<name>.evt0` / `<name>.evt1` 事件对。
    ///
    /// 若映射名已存在（另一个写入端，或读取方的视图仍使其存活）则以失败关闭：
    /// 返回错误前会释放所有已部分创建的句柄。`fps` 是存入头的不可变会话速率，
    /// 必须满足 `1..=1000`；`Geometry` 已保证 NV12 尺寸有效。
    pub fn new(name: &str, geometry: Geometry, fps: u32) -> io::Result<Self> {
        if fps == 0 || fps > MAX_FPS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("fps must be in 1..={MAX_FPS}, got {fps}"),
            ));
        }

        // 创建任何内核对象之前，先校验/分配所有名称。
        let name_w = wide_name(name)?;
        let event_names = [
            wide_name(&format!("{name}.evt0"))?,
            wide_name(&format!("{name}.evt1"))?,
        ];

        let frame_bytes = geometry.frame_bytes();
        if frame_bytes == 0 || frame_bytes > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid frame size: {frame_bytes} bytes"),
            ));
        }
        let map_size = mapping_size(frame_bytes);
        if map_size > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("SHM mapping is too large: {map_size} bytes"),
            ));
        }

        let map = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                ptr::null(),
                PAGE_READWRITE,
                0,
                map_size as u32,
                name_w.as_ptr(),
            )
        };
        if map.is_null() {
            return Err(last_os_error("CreateFileMappingW"));
        }
        // 立即检查：GetLastError() 对成功的 CreateFileMappingW 才有效，任何
        // 中间调用都会覆盖它。
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(map) };
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("named SHM mapping already exists: {name}"),
            ));
        }

        let view = unsafe { MapViewOfFile(map, FILE_MAP_WRITE, 0, 0, map_size) };
        if view.Value.is_null() {
            let err = last_os_error("MapViewOfFile");
            unsafe { CloseHandle(map) };
            return Err(err);
        }

        let mut events: [HANDLE; 2] = [ptr::null_mut(); 2];
        for (index, event_name) in event_names.iter().enumerate() {
            let event = unsafe { CreateEventW(ptr::null(), 1, 0, event_name.as_ptr()) };
            if event.is_null() {
                let err = last_os_error("CreateEventW");
                // 释放目前创建的所有对象。事件可能早已存在（由下游读取方持有）；
                // 关闭我们的重复句柄不会移除读取方的对象。
                unsafe {
                    UnmapViewOfFile(view);
                    CloseHandle(map);
                    for handle in events.iter().filter(|h| !h.is_null()) {
                        CloseHandle(*handle);
                    }
                }
                return Err(err);
            }
            events[index] = event;
        }
        // 前一个写入端可能留下已置位的事件对。旧行为是把两者都重置；读取方
        // 已持有的句柄仍然有效。
        for event in events {
            unsafe {
                let _ = ResetEvent(event);
            }
        }

        let publisher = Self {
            map,
            view,
            events,
            geometry,
            frame_bytes,
            fps,
            cursor: 0,
            latest: None,
            serials: [0; SLOT_COUNT],
            published: 0,
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
        };
        publisher.init_header();
        Ok(publisher)
    }

    /// 把一帧完整拷入下一个槽并发布。
    ///
    /// `data.len()` 必须等于会话 `Geometry::frame_bytes()`；`first_ns` 是调用方
    /// 记录的、该帧首包的 QPC 时间戳。`now_ns` 恰好在拷贝之后、发布之前调用
    /// 一次，用于 `slot.qpc_pub_ns`（只适用于同 QPC 时钟内的测量）。
    pub fn publish(
        &mut self,
        frame_id: u32,
        first_ns: u64,
        data: &[u8],
        now_ns: impl FnOnce() -> u64,
    ) -> io::Result<PublishToken> {
        if data.len() != self.frame_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "frame must be exactly {} bytes, got {}",
                    self.frame_bytes,
                    data.len()
                ),
            ));
        }

        let index = self.pick_slot();
        let slot = self.slot_ptr(index);

        unsafe {
            // 偶 -> 奇（填充中）。若上一次调用在填充中 panic，该槽已是奇数；
            // +2 仍会改变值并把读取方挡在外面。
            let was_odd = (*slot).seq.load(Ordering::Relaxed) & 1 != 0;
            (*slot)
                .seq
                .fetch_add(if was_odd { 2 } else { 1 }, Ordering::SeqCst);
            (*slot)
                .hash_fid
                .store(hash_marker(frame_id), Ordering::Relaxed);
            ptr::copy_nonoverlapping(data.as_ptr(), self.data_ptr(index), self.frame_bytes);
        }

        let published_ns = now_ns();

        let seq;
        unsafe {
            (*slot).frame_id.store(frame_id, Ordering::Relaxed);
            (*slot).qpc_first_ns.store(first_ns, Ordering::Relaxed);
            (*slot).qpc_pub_ns.store(published_ns, Ordering::Relaxed);
            // 奇 -> 偶 = 就绪。SeqCst 提供旧式 InterlockedIncrement 的完整
            // release 屏障：所有槽字段先变为可见。
            seq = (*slot).seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1);

            let header = self.header_ptr();
            // 先写指针，再递增单调发布计数（与 C++ 顺序一致）。
            (*header).latest_slot.store(index as i32, Ordering::SeqCst);
            let n = (*header)
                .latest_seq
                .fetch_add(1, Ordering::SeqCst)
                .wrapping_add(1);
            let parity = (n as u32) & 1;
            // manual-reset 事件对：先清除另一奇偶，再唤醒本次发布。
            let _ = ResetEvent(self.events[(parity as usize) ^ 1]);
            let _ = SetEvent(self.events[parity as usize]);
        }

        self.latest = Some(index);
        self.cursor = (index + 1) % SLOT_COUNT;
        let serial = self.serials[index].wrapping_add(1);
        self.serials[index] = serial;
        self.published = self.published.wrapping_add(1);

        Ok(PublishToken {
            owner: self.owner,
            slot: index as u32,
            seq: seq as u32,
            frame_id,
            serial,
        })
    }

    /// 为先前已发布的一帧发布 FNV-1a64 结果。
    ///
    /// 过期令牌（槽已复用、来自其他发布器或序号已回绕）会被静默忽略。哈希先于
    /// `hash_fid` 写入，因此原生读取端要么看不到哈希，要么看到完整的哈希。本
    /// 方法从不阻塞也从不触发异常；在旁路哈希器返回后，从写入线程调用是安全的。
    pub fn mark_hash(&mut self, token: PublishToken, hash: u64) {
        if token.owner != self.owner {
            return;
        }
        let index = token.slot as usize;
        if index >= SLOT_COUNT || self.serials[index] != token.serial {
            return;
        }
        let slot = self.slot_ptr(index);
        unsafe {
            // 该槽必须仍精确持有此令牌所指的那次发布。
            if (*slot).seq.load(Ordering::Acquire) != token.seq as i32 {
                return;
            }
            if (*slot).frame_id.load(Ordering::Acquire) != token.frame_id {
                return;
            }
            (*slot).hash.store(hash, Ordering::Relaxed);
            fence(Ordering::Release);
            (*slot).hash_fid.store(token.frame_id, Ordering::Release);
        }
    }

    /// 通过该映射成功发布的帧数。
    pub fn published(&self) -> u64 {
        self.published
    }

    fn init_header(&self) {
        let header = self.header_ptr();
        unsafe {
            // 初始化期间先让新映射无效。CreateFileMappingW 一成功，读取方就可能
            // 打开该名称，因此它必须拒绝 magic 0。
            ptr::addr_of_mut!((*header).magic).write_volatile(0);
            ptr::addr_of_mut!((*header).version).write_volatile(VERSION);
            ptr::addr_of_mut!((*header).slots).write_volatile(SLOT_COUNT as u32);
            ptr::addr_of_mut!((*header).frame_bytes).write_volatile(self.frame_bytes as u32);
            ptr::addr_of_mut!((*header).data_off).write_volatile(DATA_OFF as u32);
            (*header).latest_slot.store(-1, Ordering::Relaxed);
            (*header).latest_seq.store(0, Ordering::Relaxed);
            ptr::addr_of_mut!((*header).writer_pid).write_volatile(GetCurrentProcessId());
            ptr::addr_of_mut!((*header).width).write_volatile(self.geometry.width());
            ptr::addr_of_mut!((*header).height).write_volatile(self.geometry.height());
            ptr::addr_of_mut!((*header).fps).write_volatile(self.fps);

            for index in 0..SLOT_COUNT {
                let slot = self.slot_ptr(index);
                // 新映射为零填充；保留旧式 `seq |= 1`（奇数 = 填充中，永不接受）
                // 与无哈希标记。
                (*slot).seq.fetch_or(1, Ordering::SeqCst);
                (*slot).hash_fid.store(HASH_NONE, Ordering::Relaxed);
            }

            fence(Ordering::Release);
            ptr::addr_of_mut!((*header).magic).write_volatile(MAGIC);
        }
    }

    /// 轮转游标，跳过当前作为 latest 暴露的槽。十二个槽、仅一个 latest，因此
    /// 始终存在候选槽。
    fn pick_slot(&self) -> usize {
        let mut index = self.cursor % SLOT_COUNT;
        for _ in 0..SLOT_COUNT {
            if self.latest != Some(index) {
                return index;
            }
            index = (index + 1) % SLOT_COUNT;
        }
        self.cursor % SLOT_COUNT
    }

    fn header_ptr(&self) -> *mut ShmHeader {
        self.view.Value.cast::<ShmHeader>()
    }

    fn slot_ptr(&self, index: usize) -> *mut ShmSlot {
        debug_assert!(index < SLOT_COUNT);
        unsafe {
            self.view
                .Value
                .cast::<u8>()
                .add(slot_offset(index))
                .cast::<ShmSlot>()
        }
    }

    fn data_ptr(&self, index: usize) -> *mut u8 {
        debug_assert!(index < SLOT_COUNT);
        unsafe {
            self.view
                .Value
                .cast::<u8>()
                .add(DATA_OFF + index * self.frame_bytes)
        }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        unsafe {
            if !self.view.Value.is_null() {
                UnmapViewOfFile(self.view);
                self.view.Value = ptr::null_mut();
            }
            if !self.map.is_null() {
                CloseHandle(self.map);
                self.map = ptr::null_mut();
            }
            for event in &mut self.events {
                if !event.is_null() {
                    CloseHandle(*event);
                    *event = ptr::null_mut();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::slice;

    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Memory::{FILE_MAP_READ, OpenFileMappingW};
    use windows_sys::Win32::System::Threading::{OpenEventW, WaitForSingleObject};

    /// winnt.h 中的 `SYNCHRONIZE`；只为这个常量启用 Win32_Storage_FileSystem
    /// 会给发布器本身引入它不需要的依赖。
    const SYNCHRONIZE: u32 = 0x0010_0000;

    static NEXT_NAME: AtomicU32 = AtomicU32::new(0);

    /// 会话私有的名称，在测试与进程之间唯一。
    fn unique_name(tag: &str) -> String {
        let pid = unsafe { GetCurrentProcessId() };
        let serial = NEXT_NAME.fetch_add(1, Ordering::Relaxed);
        format!("Local\\vc-stream-selftest-{tag}-{pid}-{serial}")
    }

    fn wide(name: &str) -> Vec<u16> {
        name.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn test_geometry() -> Geometry {
        Geometry::new(4, 4).expect("4x4 NV12")
    }

    fn frame(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| seed.wrapping_add((i as u8).wrapping_mul(17)))
            .collect()
    }

    struct ReadView {
        map: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        size: usize,
    }

    impl ReadView {
        fn open(name: &str, size: usize) -> Self {
            let name_w = wide(name);
            let map = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name_w.as_ptr()) };
            assert!(
                !map.is_null(),
                "OpenFileMappingW({name}) failed: {}",
                io::Error::last_os_error()
            );
            let view = unsafe { MapViewOfFile(map, FILE_MAP_READ, 0, 0, size) };
            if view.Value.is_null() {
                let err = io::Error::last_os_error();
                unsafe { CloseHandle(map) };
                panic!("MapViewOfFile({name}) failed: {err}");
            }
            Self { map, view, size }
        }

        fn base(&self) -> *const u8 {
            self.view.Value.cast::<u8>()
        }

        fn bytes(&self, offset: usize, len: usize) -> &[u8] {
            assert!(offset + len <= self.size, "read outside mapped view");
            unsafe { slice::from_raw_parts(self.base().add(offset), len) }
        }

        fn u32_at(&self, offset: usize) -> u32 {
            u32::from_le_bytes(self.bytes(offset, 4).try_into().unwrap())
        }

        fn i32_at(&self, offset: usize) -> i32 {
            i32::from_le_bytes(self.bytes(offset, 4).try_into().unwrap())
        }

        fn u64_at(&self, offset: usize) -> u64 {
            u64::from_le_bytes(self.bytes(offset, 8).try_into().unwrap())
        }

        fn slot_offset(&self, index: usize) -> usize {
            HEADER_SIZE + SLOT_SIZE * index
        }
    }

    impl Drop for ReadView {
        fn drop(&mut self) {
            unsafe {
                if !self.view.Value.is_null() {
                    UnmapViewOfFile(self.view);
                }
                if !self.map.is_null() {
                    CloseHandle(self.map);
                }
            }
        }
    }

    struct ReadEvent(HANDLE);

    impl ReadEvent {
        fn open(name: &str, suffix: &str) -> Self {
            let event_name = format!("{name}{suffix}");
            let event_w = wide(&event_name);
            let handle = unsafe { OpenEventW(SYNCHRONIZE, 0, event_w.as_ptr()) };
            assert!(
                !handle.is_null(),
                "OpenEventW({event_name}) failed: {}",
                io::Error::last_os_error()
            );
            Self(handle)
        }

        fn signaled(&self) -> bool {
            unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
        }
    }

    impl Drop for ReadEvent {
        fn drop(&mut self) {
            unsafe {
                if !self.0.is_null() {
                    CloseHandle(self.0);
                }
            }
        }
    }

    #[test]
    fn raw_layout_is_the_frozen_vcam_shm_v3() {
        let name = unique_name("layout");
        let geometry = test_geometry();
        let frame_bytes = geometry.frame_bytes();
        let size = mapping_size(frame_bytes);
        let publisher = Publisher::new(&name, geometry, 60).unwrap();
        let view = ReadView::open(&name, size);

        assert_eq!(view.u32_at(0), MAGIC);
        assert_eq!(view.u32_at(4), VERSION);
        assert_eq!(view.u32_at(8), SLOT_COUNT as u32);
        assert_eq!(view.u32_at(12), frame_bytes as u32);
        assert_eq!(view.u32_at(16), DATA_OFF as u32);
        assert_eq!(view.i32_at(20), -1);
        assert_eq!(view.i32_at(24), 0);
        assert_eq!(view.u32_at(28), unsafe { GetCurrentProcessId() });
        assert_eq!(view.u32_at(32), geometry.width());
        assert_eq!(view.u32_at(36), geometry.height());
        assert_eq!(view.u32_at(40), 60);
        assert_eq!(view.bytes(44, 20), &[0u8; 20]);
        assert_eq!(size, DATA_OFF + SLOT_COUNT * frame_bytes);
        assert_eq!(publisher.published(), 0);

        for index in 0..SLOT_COUNT {
            let base = view.slot_offset(index);
            assert_eq!(view.i32_at(base), 1, "slot {index} starts filling");
            assert_eq!(view.u32_at(base + 4), 0);
            assert_eq!(view.u64_at(base + 8), 0);
            assert_eq!(view.u64_at(base + 16), 0);
            assert_eq!(view.u64_at(base + 24), 0);
            assert_eq!(view.u32_at(base + 32), HASH_NONE);
            assert_eq!(view.bytes(base + 36, 28), &[0u8; 28]);
        }
    }

    #[test]
    fn publish_sequence_events_and_hash_publication() {
        let name = unique_name("publish");
        let geometry = test_geometry();
        let frame_bytes = geometry.frame_bytes();
        let mut publisher = Publisher::new(&name, geometry, 120).unwrap();
        let view = ReadView::open(&name, mapping_size(frame_bytes));
        let evt0 = ReadEvent::open(&name, ".evt0");
        let evt1 = ReadEvent::open(&name, ".evt1");
        assert!(!evt0.signaled());
        assert!(!evt1.signaled());

        let data0 = frame(0x11, frame_bytes);
        let calls = Cell::new(0);
        let token = publisher
            .publish(7, 1234, &data0, || {
                calls.set(calls.get() + 1);
                5678
            })
            .unwrap();
        assert_eq!(calls.get(), 1, "now_ns runs exactly once");
        assert_eq!(publisher.published(), 1);
        assert_eq!(token.frame_id(), 7);

        // 首次发布：n = 1 -> 置位 evt1，重置 evt0。
        assert_eq!(view.u32_at(20), 0);
        assert_eq!(view.i32_at(24), 1);
        // 旧实现把槽初始化为奇数（seq = 1），故首次取用 +2，第一次发布后
        // seq 为 4；此后每次复用都 +3（奇 + 偶）。
        let slot0 = view.slot_offset(0);
        assert_eq!(view.i32_at(slot0), 4, "even = stable");
        assert_eq!(view.u32_at(slot0 + 4), 7);
        assert_eq!(view.u64_at(slot0 + 8), 1234);
        assert_eq!(view.u64_at(slot0 + 16), 5678);
        assert_eq!(view.u32_at(slot0 + 32), HASH_NONE);
        assert_ne!(view.u32_at(slot0 + 32), 7, "unhashed marker is not valid");
        assert_eq!(view.bytes(DATA_OFF, frame_bytes), &data0[..]);
        assert!(evt1.signaled());
        assert!(!evt0.signaled());

        // manual-reset：事件保持置位，直到下一次发布把它重置。
        assert!(evt1.signaled());

        publisher.mark_hash(token, 0xDEAD_BEEF_0123_4567);
        assert_eq!(view.u64_at(slot0 + 24), 0xDEAD_BEEF_0123_4567);
        assert_eq!(view.u32_at(slot0 + 32), 7);
        assert_eq!(view.u32_at(slot0 + 32), view.u32_at(slot0 + 4));

        let data1 = frame(0x22, frame_bytes);
        let token1 = publisher.publish(8, 2234, &data1, || 5679).unwrap();
        assert_eq!(publisher.published(), 2);
        assert_eq!(view.u32_at(20), 1);
        assert_eq!(view.i32_at(24), 2);
        let slot1 = view.slot_offset(1);
        assert_eq!(view.i32_at(slot1), 4);
        assert_eq!(view.u32_at(slot1 + 4), 8);
        assert_eq!(view.bytes(DATA_OFF + frame_bytes, frame_bytes), &data1[..]);
        // 第二次发布：n = 2 -> 置位 evt0，重置 evt1。
        assert!(evt0.signaled());
        assert!(!evt1.signaled());

        publisher.mark_hash(token1, 0x0102_0304_0506_0708);
        assert_eq!(view.u64_at(slot1 + 24), 0x0102_0304_0506_0708);
        assert_eq!(view.u32_at(slot1 + 32), 8);
    }

    #[test]
    fn round_robin_never_reuses_the_current_latest_slot() {
        let name = unique_name("ring");
        let geometry = test_geometry();
        let frame_bytes = geometry.frame_bytes();
        let mut publisher = Publisher::new(&name, geometry, 240).unwrap();
        let view = ReadView::open(&name, mapping_size(frame_bytes));

        let mut expected_slot = 0usize;
        let mut previous_latest = None;
        for frame_id in 0..25u32 {
            let data = frame(frame_id as u8, frame_bytes);
            let token = publisher
                .publish(frame_id, frame_id as u64, &data, || 0)
                .unwrap();
            assert_ne!(
                Some(expected_slot),
                previous_latest,
                "never overwrite the current latest slot"
            );
            assert_eq!(view.u32_at(20), expected_slot as u32);
            assert_eq!(view.i32_at(24), (frame_id + 1) as i32);
            let slot = view.slot_offset(expected_slot);
            assert_eq!(view.u32_at(slot + 4), frame_id);
            assert_eq!(
                view.bytes(DATA_OFF + expected_slot * frame_bytes, frame_bytes),
                &data[..]
            );
            publisher.mark_hash(token, u64::from(frame_id));
            previous_latest = Some(expected_slot);
            expected_slot = (expected_slot + 1) % SLOT_COUNT;
        }
        assert_eq!(publisher.published(), 25);
    }

    #[test]
    fn delayed_token_cannot_update_a_reused_slot() {
        let name = unique_name("stale");
        let geometry = test_geometry();
        let frame_bytes = geometry.frame_bytes();
        let mut publisher = Publisher::new(&name, geometry, 240).unwrap();
        let view = ReadView::open(&name, mapping_size(frame_bytes));

        let old_token = publisher
            .publish(100, 1, &frame(1, frame_bytes), || 2)
            .unwrap();
        assert_eq!(old_token.slot, 0);
        assert_eq!(view.u32_at(20), 0);

        // 一整轮之后槽 0 被复用，而旧令牌仍未归还。
        let mut new_token = old_token;
        for frame_id in 101..=112u32 {
            new_token = publisher
                .publish(frame_id, 3, &frame(2, frame_bytes), || 4)
                .unwrap();
        }
        assert_eq!(new_token.slot, 0, "slot 0 was reused");
        assert_ne!(new_token.serial, old_token.serial);
        assert_eq!(view.u32_at(20), 0);
        assert_eq!(view.u32_at(view.slot_offset(0) + 4), 112);

        // 旧帧的迟到结果被忽略，包括它的哈希标记。
        publisher.mark_hash(old_token, 0xAAAA_BBBB_CCCC_DDDD);
        assert_eq!(view.u64_at(view.slot_offset(0) + 24), 0);
        assert_ne!(view.u32_at(view.slot_offset(0) + 32), 100);

        // 当前令牌仍然有效，更晚到达的旧结果无法覆盖它。
        publisher.mark_hash(new_token, 0x0123_4567_89AB_CDEF);
        assert_eq!(view.u64_at(view.slot_offset(0) + 24), 0x0123_4567_89AB_CDEF);
        publisher.mark_hash(old_token, 0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(view.u64_at(view.slot_offset(0) + 24), 0x0123_4567_89AB_CDEF);
        assert_eq!(view.u32_at(view.slot_offset(0) + 32), 112);
    }

    #[test]
    fn maximum_frame_id_keeps_the_no_hash_marker_unambiguous() {
        let name = unique_name("sentinel");
        let geometry = test_geometry();
        let frame_bytes = geometry.frame_bytes();
        let mut publisher = Publisher::new(&name, geometry, 60).unwrap();
        let view = ReadView::open(&name, mapping_size(frame_bytes));

        // 普通帧沿用旧标记 0xFFFFFFFF。
        let normal = publisher
            .publish(5, 0, &frame(3, frame_bytes), || 0)
            .unwrap();
        let normal_slot = view.slot_offset(0);
        assert_eq!(view.u32_at(normal_slot + 32), HASH_NONE);
        assert_ne!(view.u32_at(normal_slot + 32), 5);
        publisher.mark_hash(normal, 0x55);
        assert_eq!(view.u32_at(normal_slot + 32), 5);

        // frame_id == HASH_NONE 在 mark_hash 之前绝不能看起来已哈希。标记为
        // frame_id ^ HASH_NONE == 0，旧读取端会拿它与 0xFFFFFFFF 比较。
        let sentinel = publisher
            .publish(HASH_NONE, 0, &frame(4, frame_bytes), || 0)
            .unwrap();
        let slot = view.slot_offset(1);
        assert_eq!(view.u32_at(slot + 4), HASH_NONE);
        assert_eq!(view.u32_at(slot + 32), 0);
        assert_ne!(view.u32_at(slot + 32), view.u32_at(slot + 4));

        publisher.mark_hash(sentinel, 0xFEED_FACE_CAFE_BEEF);
        assert_eq!(view.u32_at(slot + 32), HASH_NONE);
        assert_eq!(view.u32_at(slot + 32), view.u32_at(slot + 4));
        assert_eq!(view.u64_at(slot + 24), 0xFEED_FACE_CAFE_BEEF);
    }

    #[test]
    fn token_from_another_publisher_is_ignored() {
        let name_a = unique_name("owner-a");
        let name_b = unique_name("owner-b");
        let geometry = test_geometry();
        let frame_bytes = geometry.frame_bytes();
        let mut a = Publisher::new(&name_a, geometry, 60).unwrap();
        let mut b = Publisher::new(&name_b, geometry, 60).unwrap();
        let view_b = ReadView::open(&name_b, mapping_size(frame_bytes));

        let data = frame(9, frame_bytes);
        let token_a = a.publish(1, 0, &data, || 0).unwrap();
        let token_b = b.publish(1, 0, &data, || 0).unwrap();
        // slot/seq/frame id/serial 全部相同、owner 不同：仅形状碰撞。
        assert_eq!(token_a.slot, token_b.slot);
        assert_eq!(token_a.seq, token_b.seq);
        assert_eq!(token_a.serial, token_b.serial);
        assert_ne!(token_a.owner, token_b.owner);

        b.mark_hash(token_a, 0xDEAD);
        assert_eq!(view_b.u64_at(view_b.slot_offset(0) + 24), 0);
        assert_ne!(view_b.u32_at(view_b.slot_offset(0) + 32), 1);

        b.mark_hash(token_b, 0xBEEF);
        assert_eq!(view_b.u64_at(view_b.slot_offset(0) + 24), 0xBEEF);
        assert_eq!(view_b.u32_at(view_b.slot_offset(0) + 32), 1);
    }

    #[test]
    fn existing_mapping_fails_closed_without_disturbing_the_writer() {
        let name = unique_name("collision");
        let geometry = test_geometry();
        let frame_bytes = geometry.frame_bytes();
        let mut first = Publisher::new(&name, geometry, 60).unwrap();
        let evt0 = ReadEvent::open(&name, ".evt0");
        let evt1 = ReadEvent::open(&name, ".evt1");

        first.publish(1, 10, &frame(1, frame_bytes), || 20).unwrap();
        assert!(evt1.signaled(), "n = 1 signals evt1");

        let err = Publisher::new(&name, geometry, 60)
            .err()
            .expect("duplicate mapping must fail");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(evt1.signaled(), "failed duplicate did not reset events");
        assert!(!evt0.signaled());

        // 原写入端未受影响，仍拥有该环。
        first.publish(2, 11, &frame(2, frame_bytes), || 21).unwrap();
        assert!(evt0.signaled(), "n = 2 signals evt0");
        assert!(!evt1.signaled());
        assert_eq!(first.published(), 2);

        drop(evt0);
        drop(evt1);
        drop(first);
        // 所有句柄都随 drop 释放，因此新的写入端可以重新占用该名称。
        let replacement = Publisher::new(&name, geometry, 60).unwrap();
        assert_eq!(replacement.published(), 0);
    }

    #[test]
    fn partial_event_failure_releases_mapping_and_event() {
        let name = unique_name("partial");
        let geometry = test_geometry();
        let blocker_name = format!("{name}.evt1");
        let blocker_w = wide(&blocker_name);

        // 用另一种内核对象类型占用第二个事件名，使发布器先映射 SHM、创建
        // evt0，然后在 evt1 上失败。
        let blocker = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                4096,
                blocker_w.as_ptr(),
            )
        };
        assert!(
            !blocker.is_null(),
            "blocker mapping: {}",
            io::Error::last_os_error()
        );

        let err = Publisher::new(&name, geometry, 60)
            .err()
            .expect("evt1 collision must fail the constructor");
        assert_ne!(
            err.kind(),
            io::ErrorKind::AlreadyExists,
            "failure was past the SHM mapping"
        );
        unsafe { CloseHandle(blocker) };

        // 基映射、视图和 evt0 均已释放：现在可以重新使用该名称。
        let publisher = Publisher::new(&name, geometry, 60).unwrap();
        assert_eq!(publisher.published(), 0);
    }

    #[test]
    fn invalid_inputs_do_not_create_or_publish_state() {
        let name = unique_name("invalid");
        let geometry = test_geometry();
        let frame_bytes = geometry.frame_bytes();

        assert_eq!(
            Publisher::new(&name, geometry, 0)
                .err()
                .expect("fps 0 must fail")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            Publisher::new(&name, geometry, MAX_FPS + 1)
                .err()
                .expect("fps > 1000 must fail")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            Publisher::new("", geometry, 60)
                .err()
                .expect("empty name must fail")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            Publisher::new("Local\\bad\0name", geometry, 60)
                .err()
                .expect("NUL name must fail")
                .kind(),
            io::ErrorKind::InvalidInput
        );

        // 失败的构造函数没有留下任何映射。
        let mut publisher = Publisher::new(&name, geometry, 60).unwrap();
        let view = ReadView::open(&name, mapping_size(frame_bytes));
        let calls = Cell::new(0);
        let err = publisher
            .publish(1, 2, &[0u8; 1], || {
                calls.set(calls.get() + 1);
                3
            })
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(calls.get(), 0, "invalid frame never reaches now_ns");
        assert_eq!(publisher.published(), 0);
        assert_eq!(view.i32_at(20), -1);
        assert_eq!(view.i32_at(24), 0);

        drop(view);
        drop(publisher);
        let replacement = Publisher::new(&name, geometry, 60).unwrap();
        assert_eq!(replacement.published(), 0);
    }
}
