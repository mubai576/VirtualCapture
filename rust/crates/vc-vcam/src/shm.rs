//! 冻结的 SHM v3 线格式，与接收端和虚拟卡共享（布局规范见
//! `docs/spec/shm-v3.md`；Rust 写入端为 `vc-stream/src/shm.rs`）。本模块
//! 只使用只读视图。
//!
//! ```text
//! [0, 64)                     vc_shm_hdr_t       (ShmHeader)
//! [64, 64 + 64*12)            vc_shm_slot_t[12]  (ShmSlot)
//! [4096 + i*frame_bytes, ...) 槽 i 的完整帧数据
//! ```
//!
//! 读取端遵循 D11 seqlock 协议：普通对齐 volatile 读取，拷贝帧数据，然后
//! 仅在 `slot.seq` 未变化时接受。发布以 `(slot, seq)` 标识，而不是
//! `latest_seq`。

use core::sync::atomic::{AtomicI32, Ordering};
use std::ptr;

use crate::geometry::StreamGeometry;

pub const SHM_MAGIC_V3: u32 = 0x3341_4356; // "VCA3"
pub const SHM_MAGIC_V2: u32 = 0x3241_4356; // "VCA2"
pub const SHM_VERSION_V3: u32 = 3;
pub const SHM_VERSION_V2: u32 = 2;
pub const SHM_SLOTS: u32 = 12;
pub const SHM_DATA_OFF: u32 = 4096;
pub const SHM_HASH_NONE: u32 = 0xFFFF_FFFF;
/// v2 固定帧大小（640x640 NV12）。
pub const SHM_V2_FRAME_BYTES: u32 = 614_400;

/// `vc_shm_hdr_t`（64 字节，`#pragma pack(1)`；本布局不会产生隐式填充）。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ShmHeader {
    pub magic: u32,
    pub version: u32,
    pub slots: u32,
    pub frame_bytes: u32,
    pub data_off: u32,
    pub latest_slot: i32,
    pub latest_seq: i32,
    pub writer_pid: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub reserved: [u8; 20],
}

/// `vc_shm_slot_t`（64 字节，`#pragma pack(1)`）。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ShmSlot {
    pub seq: i32,
    pub frame_id: u32,
    pub qpc_first_ns: u64,
    pub qpc_pub_ns: u64,
    pub hash: u64,
    pub hash_fid: u32,
    pub reserved: [u8; 28],
}

const _: () = assert!(core::mem::size_of::<ShmHeader>() == 64);
const _: () = assert!(core::mem::size_of::<ShmSlot>() == 64);
const _: () = assert!(64 + 64 * SHM_SLOTS as usize <= SHM_DATA_OFF as usize);

/// 会话帧大小对应的映射总大小（对应历史 C++ `vc_shm_size`）。
pub const fn shm_size(frame_bytes: u32) -> usize {
    SHM_DATA_OFF as usize + SHM_SLOTS as usize * frame_bytes as usize
}

/// 校验已映射的头（对应历史 C++ `vc_shm_valid`）。
pub fn valid_header(h: &ShmHeader) -> bool {
    if h.slots != SHM_SLOTS || h.data_off != SHM_DATA_OFF {
        return false;
    }
    if h.magic == SHM_MAGIC_V2 && h.version == SHM_VERSION_V2 {
        return h.frame_bytes == SHM_V2_FRAME_BYTES;
    }
    h.magic == SHM_MAGIC_V3
        && h.version == SHM_VERSION_V3
        && h.width >= 2
        && h.height >= 2
        && h.width <= 8192
        && h.height <= 8192
        && h.width & 1 == 0
        && h.height & 1 == 0
        && h.fps >= 1
        && h.fps <= 1000
        && (h.frame_bytes as u64) == (h.width as u64) * (h.height as u64) * 3 / 2
}

/// 校验一个映射视图。
///
/// # Safety
///
/// `view` 必须为空，或指向至少 64 个可读字节（完整的 [`ShmHeader`]），且
/// 该内存在调用期间保持映射。
pub unsafe fn valid_ptr(view: *const u8) -> bool {
    if view.is_null() {
        return false;
    }
    valid_header(&*(view as *const ShmHeader))
}

/// 有效头所公布的几何。v2 头不携带几何，恒映射到 640x640@240 会话契约。
pub fn geometry_from_header(h: &ShmHeader) -> Option<StreamGeometry> {
    if !valid_header(h) {
        return None;
    }
    if h.magic == SHM_MAGIC_V2 {
        return Some(StreamGeometry::DEFAULT);
    }
    StreamGeometry::new(h.width, h.height, h.fps)
}

/// 返回 64 字节槽头的地址。
///
/// # Safety
///
/// `view` 必须指向至少 `64 + 64 * SHM_SLOTS` 字节的已映射 SHM 区域，且
/// `slot` 小于 [`SHM_SLOTS`]，使返回地址落在映射内部。
pub unsafe fn slot_ptr(view: *mut u8, slot: u32) -> *mut ShmSlot {
    view.add(64 + 64 * slot as usize) as *mut ShmSlot
}

/// 返回某帧槽载荷的地址。
///
/// # Safety
///
/// `view` 必须指向带可读 [`ShmHeader`] 的有效映射 SHM 区域，`slot` 小于
/// [`SHM_SLOTS`]，且映射覆盖按该头计算的
/// `SHM_DATA_OFF + slot * frame_bytes + frame_bytes` 字节。
pub unsafe fn data_ptr(view: *mut u8, slot: u32) -> *mut u8 {
    let frame_bytes = (*(view as *const ShmHeader)).frame_bytes as usize;
    view.add(SHM_DATA_OFF as usize + slot as usize * frame_bytes)
}

/// 当前发布计数（`hdr.latest_seq`），以对齐 volatile 读取。
///
/// # Safety
///
/// `view` 必须指向包含可读 [`ShmHeader`] 的已映射 SHM 区域，并在调用期间
/// 保持有效。
pub unsafe fn latest_seq(view: *const u8) -> i32 {
    ptr::read_volatile(&(*(view as *const ShmHeader)).latest_seq)
}

/// 最新发布的槽索引，以对齐 volatile 读取。
///
/// # Safety
///
/// `view` 必须指向包含可读 [`ShmHeader`] 的已映射 SHM 区域，并在调用期间
/// 保持有效。
pub unsafe fn latest_slot(view: *const u8) -> i32 {
    ptr::read_volatile(&(*(view as *const ShmHeader)).latest_slot)
}

/// `VCamPeek`：返回稳定的最新发布令牌；写入端正在填槽时返回 `None`。令牌
/// 为 `(slot, seq)`，用于去重。
///
/// # Safety
///
/// `view` 必须为空，或指向头能通过 [`valid_header`] 的已映射 SHM 区域；
/// 整个映射在调用期间必须有效。
pub unsafe fn peek(view: *mut u8) -> Option<(u32, i32)> {
    if !valid_ptr(view) {
        return None;
    }
    let i = latest_slot(view);
    if !(0..SHM_SLOTS as i32).contains(&i) {
        return None;
    }
    let seq = ptr::read_volatile(&(*slot_ptr(view, i as u32)).seq);
    if seq & 1 != 0 {
        return None;
    }
    Some((i as u32, seq))
}

/// 一次成功 seqlock 拷贝的结果。
#[derive(Clone, Copy, Debug)]
pub struct SlotCopy {
    pub frame_id: u32,
    pub qpc_pub_ns: u64,
}

/// `VCamCopySlot`：把一帧拷入 `dst`（至少 `hdr.frame_bytes` 个可写字节），
/// 并报告该拷贝是否稳定。
///
/// # Safety
///
/// `view` 必须为空，或指向有效的已映射 SHM 区域，其头和帧槽在调用期间保持
/// 可读。`dst` 必须为空，或指向至少 `hdr.frame_bytes` 个可写字节，且不与
/// 映射区域重叠。
pub unsafe fn copy_slot(view: *mut u8, slot: u32, seq: i32, dst: *mut u8) -> Option<SlotCopy> {
    if dst.is_null() || slot >= SHM_SLOTS {
        return None;
    }
    let s = slot_ptr(view, slot);
    let frame_id = ptr::read_volatile(&(*s).frame_id);
    let qpc_pub_ns = ptr::read_volatile(&(*s).qpc_pub_ns);
    core::sync::atomic::fence(Ordering::Acquire);
    let frame_bytes = (*(view as *const ShmHeader)).frame_bytes as usize;
    ptr::copy_nonoverlapping(data_ptr(view, slot), dst, frame_bytes);
    core::sync::atomic::fence(Ordering::Acquire);
    if ptr::read_volatile(&(*s).seq) == seq {
        Some(SlotCopy {
            frame_id,
            qpc_pub_ns,
        })
    } else {
        None
    }
}

// ---- vcam -> 接收端反馈环（`<map>.vcam`） ----

pub const VSTAT_MAGIC: u32 = 0x5453_5656; // "VVST"
pub const VSTAT_CAP: u32 = 1024;
pub const VSTAT_HDR_SIZE: usize = 16;
pub const VSTAT_ENTRY_SIZE: usize = 32;
pub const VSTAT_SIZE: usize = VSTAT_HDR_SIZE + VSTAT_CAP as usize * VSTAT_ENTRY_SIZE;
pub const VSTAT_FLAG_REPEAT: u32 = 1;

/// `vc_vstat_hdr_t`（16 字节）。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VstatHeader {
    pub magic: u32,
    pub widx: i32,
    pub cap: u32,
    pub reserved: u32,
}

/// `vc_vstat_entry_t`（32 字节）。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VstatEntry {
    pub n: u32,
    pub frame_id: u32,
    pub pub_ns: u64,
    pub deliver_ns: u64,
    pub flags: u32,
    pub reserved: u32,
}

const _: () = assert!(core::mem::size_of::<VstatHeader>() == VSTAT_HDR_SIZE);
const _: () = assert!(core::mem::size_of::<VstatEntry>() == VSTAT_ENTRY_SIZE);
const _: () = assert!(VSTAT_SIZE == 32_784);

/// 若该反馈映射是刚刚创建的，则初始化其头部。
///
/// # Safety
///
/// `base` 必须为空，或指向至少 [`VSTAT_SIZE`] 个可写字节，且在调用期间
/// 保持映射。
pub unsafe fn vstat_init(base: *mut u8) {
    if base.is_null() {
        return;
    }
    let h = base as *mut VstatHeader;
    if ptr::read_volatile(ptr::addr_of!((*h).magic)) != VSTAT_MAGIC {
        ptr::write_volatile(ptr::addr_of_mut!((*h).cap), VSTAT_CAP);
        core::sync::atomic::fence(Ordering::SeqCst);
        ptr::write_volatile(ptr::addr_of_mut!((*h).magic), VSTAT_MAGIC);
    }
}

/// 发布一条投递记录。
///
/// # Safety
///
/// `base` 必须为空，或指向至少 [`VSTAT_SIZE`] 个可写字节，且在调用期间
/// 保持映射。
pub unsafe fn vstat_push(base: *mut u8, frame_id: u32, pub_ns: u64, deliver_ns: u64, flags: u32) {
    if base.is_null() {
        return;
    }
    let widx = AtomicI32::from_ptr(base.add(4) as *mut i32);
    let k = widx.fetch_add(1, Ordering::SeqCst);
    let index = (k as u32) % VSTAT_CAP;
    let e = base.add(VSTAT_HDR_SIZE + index as usize * VSTAT_ENTRY_SIZE) as *mut VstatEntry;
    ptr::write_volatile(ptr::addr_of_mut!((*e).n), 0);
    core::sync::atomic::fence(Ordering::SeqCst);
    ptr::write_volatile(ptr::addr_of_mut!((*e).frame_id), frame_id);
    ptr::write_volatile(ptr::addr_of_mut!((*e).pub_ns), pub_ns);
    ptr::write_volatile(ptr::addr_of_mut!((*e).deliver_ns), deliver_ns);
    ptr::write_volatile(ptr::addr_of_mut!((*e).flags), flags);
    core::sync::atomic::fence(Ordering::SeqCst);
    ptr::write_volatile(ptr::addr_of_mut!((*e).n), k as u32 + 1);
}

/// 对应历史 C++ `vc_qpc_to_ns`：避免 `counter * 1e9` 溢出。
pub const fn qpc_to_ns(counter: u64, freq: u64) -> u64 {
    (counter / freq) * 1_000_000_000 + (counter % freq) * 1_000_000_000 / freq
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    fn fake_ring(frame_bytes: u32) -> Vec<u64> {
        let bytes = shm_size(frame_bytes);
        vec![0u64; bytes.div_ceil(8)]
    }

    fn write_v3_header(view: *mut u8, width: u32, height: u32, fps: u32, latest_slot: i32) {
        let h = view as *mut ShmHeader;
        let frame_bytes = width * height * 3 / 2;
        unsafe {
            ptr::write(
                h,
                ShmHeader {
                    magic: SHM_MAGIC_V3,
                    version: SHM_VERSION_V3,
                    slots: SHM_SLOTS,
                    frame_bytes,
                    data_off: SHM_DATA_OFF,
                    latest_slot,
                    latest_seq: 1,
                    writer_pid: 4242,
                    width,
                    height,
                    fps,
                    reserved: [0; 20],
                },
            );
        }
    }

    #[test]
    fn frozen_field_offsets() {
        assert_eq!(offset_of!(ShmHeader, magic), 0);
        assert_eq!(offset_of!(ShmHeader, version), 4);
        assert_eq!(offset_of!(ShmHeader, slots), 8);
        assert_eq!(offset_of!(ShmHeader, frame_bytes), 12);
        assert_eq!(offset_of!(ShmHeader, data_off), 16);
        assert_eq!(offset_of!(ShmHeader, latest_slot), 20);
        assert_eq!(offset_of!(ShmHeader, latest_seq), 24);
        assert_eq!(offset_of!(ShmHeader, writer_pid), 28);
        assert_eq!(offset_of!(ShmHeader, width), 32);
        assert_eq!(offset_of!(ShmHeader, height), 36);
        assert_eq!(offset_of!(ShmHeader, fps), 40);
        assert_eq!(offset_of!(ShmHeader, reserved), 44);

        assert_eq!(offset_of!(ShmSlot, seq), 0);
        assert_eq!(offset_of!(ShmSlot, frame_id), 4);
        assert_eq!(offset_of!(ShmSlot, qpc_first_ns), 8);
        assert_eq!(offset_of!(ShmSlot, qpc_pub_ns), 16);
        assert_eq!(offset_of!(ShmSlot, hash), 24);
        assert_eq!(offset_of!(ShmSlot, hash_fid), 32);
        assert_eq!(offset_of!(ShmSlot, reserved), 36);

        assert_eq!(offset_of!(VstatHeader, magic), 0);
        assert_eq!(offset_of!(VstatHeader, widx), 4);
        assert_eq!(offset_of!(VstatHeader, cap), 8);
        assert_eq!(offset_of!(VstatEntry, n), 0);
        assert_eq!(offset_of!(VstatEntry, frame_id), 4);
        assert_eq!(offset_of!(VstatEntry, pub_ns), 8);
        assert_eq!(offset_of!(VstatEntry, deliver_ns), 16);
        assert_eq!(offset_of!(VstatEntry, flags), 24);
        assert_eq!(offset_of!(VstatEntry, reserved), 28);
    }

    #[test]
    fn frozen_constants() {
        assert_eq!(SHM_MAGIC_V3.to_le_bytes(), *b"VCA3");
        assert_eq!(SHM_MAGIC_V2.to_le_bytes(), *b"VCA2");
        assert_eq!(VSTAT_MAGIC.to_le_bytes(), *b"VVST");
        assert_eq!(SHM_V2_FRAME_BYTES, 640 * 640 * 3 / 2);
        assert_eq!(shm_size(614_400), 4096 + 12 * 614_400);
    }

    #[test]
    fn header_validation_accepts_v3_and_v2() {
        let mut buf = fake_ring(12);
        let view = buf.as_mut_ptr() as *mut u8;
        write_v3_header(view, 4, 2, 60, -1);
        assert!(unsafe { valid_ptr(view) });
        let h = unsafe { &*(view as *const ShmHeader) };
        assert_eq!(geometry_from_header(h), StreamGeometry::new(4, 2, 60));

        unsafe {
            ptr::write(
                view as *mut ShmHeader,
                ShmHeader {
                    magic: SHM_MAGIC_V2,
                    version: SHM_VERSION_V2,
                    slots: SHM_SLOTS,
                    frame_bytes: SHM_V2_FRAME_BYTES,
                    data_off: SHM_DATA_OFF,
                    latest_slot: 0,
                    latest_seq: 0,
                    writer_pid: 0,
                    width: 0,
                    height: 0,
                    fps: 0,
                    reserved: [0; 20],
                },
            );
        }
        assert!(unsafe { valid_ptr(view) });
        assert_eq!(
            geometry_from_header(unsafe { &*(view as *const ShmHeader) }),
            Some(StreamGeometry::DEFAULT)
        );
    }

    #[test]
    fn header_validation_rejects_bad_geometry() {
        let mut buf = fake_ring(12);
        let view = buf.as_mut_ptr() as *mut u8;

        let cases: [(u32, u32, u32, u32); 4] = [
            (3, 2, 60, 9),    // 宽为奇数
            (4, 3, 60, 18),   // 高为奇数
            (4, 2, 0, 12),    // fps 低于范围
            (4, 2, 1001, 12), // fps 高于范围
        ];
        for (w, h, fps, bytes) in cases {
            write_v3_header(view, 4, 2, 60, -1);
            let hdr = view as *mut ShmHeader;
            unsafe {
                (*hdr).width = w;
                (*hdr).height = h;
                (*hdr).fps = fps;
                (*hdr).frame_bytes = bytes;
            }
            assert!(
                !unsafe { valid_ptr(view) },
                "case {w}x{h}@{fps} bytes {bytes}"
            );
        }

        write_v3_header(view, 4, 2, 60, -1);
        unsafe {
            (*(view as *mut ShmHeader)).frame_bytes = 13;
        }
        assert!(!unsafe { valid_ptr(view) });

        write_v3_header(view, 4, 2, 60, -1);
        unsafe {
            (*(view as *mut ShmHeader)).slots = 11;
        }
        assert!(!unsafe { valid_ptr(view) });

        write_v3_header(view, 4, 2, 60, -1);
        unsafe {
            (*(view as *mut ShmHeader)).data_off = 2048;
        }
        assert!(!unsafe { valid_ptr(view) });
    }

    #[test]
    fn peek_and_copy_follow_seqlock() {
        let mut buf = fake_ring(12);
        let view = buf.as_mut_ptr() as *mut u8;
        write_v3_header(view, 4, 2, 60, 3);
        unsafe {
            ptr::write(
                slot_ptr(view, 3),
                ShmSlot {
                    seq: 2,
                    frame_id: 77,
                    qpc_first_ns: 10,
                    qpc_pub_ns: 0x1122_3344_5566_7788,
                    hash: 0,
                    hash_fid: SHM_HASH_NONE,
                    reserved: [0; 28],
                },
            );
            for i in 0..12u8 {
                *data_ptr(view, 3).add(i as usize) = i + 1;
            }
        }

        assert_eq!(unsafe { peek(view) }, Some((3, 2)));
        assert_eq!(unsafe { latest_seq(view) }, 1);
        assert_eq!(unsafe { latest_slot(view) }, 3);

        let mut frame = [0u8; 12];
        let copy = unsafe { copy_slot(view, 3, 2, frame.as_mut_ptr()) }.expect("stable copy");
        assert_eq!(copy.frame_id, 77);
        assert_eq!(copy.qpc_pub_ns, 0x1122_3344_5566_7788);
        assert_eq!(frame, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);

        // 写入端序号变化后，该拷贝应被判为撕裂。
        unsafe {
            (*slot_ptr(view, 3)).seq = 4;
        }
        assert!(unsafe { copy_slot(view, 3, 2, frame.as_mut_ptr()) }.is_none());

        // 奇数（填充中）槽不是已发布状态。
        unsafe {
            (*slot_ptr(view, 3)).seq = 5;
        }
        assert_eq!(unsafe { peek(view) }, None);

        // 越界槽应被忽略。
        unsafe {
            (*(view as *mut ShmHeader)).latest_slot = 12;
        }
        assert_eq!(unsafe { peek(view) }, None);
    }

    #[test]
    fn vstat_ring_wraps_and_keeps_last_entry() {
        let mut buf = vec![0u64; VSTAT_SIZE.div_ceil(8)];
        let base = buf.as_mut_ptr() as *mut u8;
        unsafe {
            vstat_init(base);
            let h = &*(base as *const VstatHeader);
            assert_eq!(h.magic, VSTAT_MAGIC);
            assert_eq!(h.cap, VSTAT_CAP);

            vstat_push(base, 1, 100, 200, 0);
            vstat_push(base, 2, 300, 400, VSTAT_FLAG_REPEAT);
            let h = &*(base as *const VstatHeader);
            assert_eq!(h.widx, 2);
            let e0 = &*(base.add(VSTAT_HDR_SIZE) as *const VstatEntry);
            let e1 = &*(base.add(VSTAT_HDR_SIZE + VSTAT_ENTRY_SIZE) as *const VstatEntry);
            assert_eq!(
                (e0.n, e0.frame_id, e0.pub_ns, e0.deliver_ns),
                (1, 1, 100, 200)
            );
            assert_eq!(
                (e1.n, e1.frame_id, e1.pub_ns, e1.deliver_ns, e1.flags),
                (2, 2, 300, 400, VSTAT_FLAG_REPEAT)
            );

            // 回绕到同一槽；n 保留全局计数。
            *((base.add(4)) as *mut i32) = VSTAT_CAP as i32;
            vstat_push(base, 4, 700, 800, 0);
            let e_last = &*(base.add(VSTAT_HDR_SIZE) as *const VstatEntry);
            assert_eq!(e_last.n, VSTAT_CAP + 1);
            assert_eq!(e_last.frame_id, 4);
        }
    }

    #[test]
    fn qpc_conversion_does_not_overflow() {
        // 10 MHz 计数器约 30 分钟后：朴素写法 c*1e9 会溢出。
        let ns = qpc_to_ns(18_000_000_000, 10_000_000);
        assert_eq!(ns, 1_800_000_000_000);
        assert_eq!(qpc_to_ns(0, 10_000_000), 0);
        assert_eq!(qpc_to_ns(1, 10_000_000), 100);
    }
}
