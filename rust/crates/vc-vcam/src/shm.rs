//! Frozen SHM v3 wire layout shared with the C++ receiver and vcam
//! (`vcam/vcam_shm.h`, canonical).  Only read-only views are used here.
//!
//! ```text
//! [0, 64)                     vc_shm_hdr_t       (ShmHeader)
//! [64, 64 + 64*12)            vc_shm_slot_t[12]  (ShmSlot)
//! [4096 + i*frame_bytes, ...) frame bytes of slot i
//! ```
//!
//! The reader is the D11 seqlock protocol: plain aligned volatile loads, copy
//! the frame, then accept only when `slot.seq` is unchanged.  Publications are
//! identified by `(slot, seq)`, not by `latest_seq`.

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
/// Fixed v2 frame size (640x640 NV12).
pub const SHM_V2_FRAME_BYTES: u32 = 614_400;

/// `vc_shm_hdr_t` (64 bytes, `#pragma pack(1)`; no implicit padding occurs).
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

/// `vc_shm_slot_t` (64 bytes, `#pragma pack(1)`).
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

/// Total mapping size for a session frame size (C++ `vc_shm_size`).
pub const fn shm_size(frame_bytes: u32) -> usize {
    SHM_DATA_OFF as usize + SHM_SLOTS as usize * frame_bytes as usize
}

/// C++ `vc_shm_valid` for an already-mapped header.
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

/// Validates a mapped view.
///
/// # Safety
///
/// `view` must be null or point to at least 64 readable bytes (a complete
/// [`ShmHeader`]) that stay mapped for the duration of the call.
pub unsafe fn valid_ptr(view: *const u8) -> bool {
    if view.is_null() {
        return false;
    }
    valid_header(&*(view as *const ShmHeader))
}

/// Geometry advertised by a valid header.  v2 carries no geometry and always
/// maps to the 640x640@240 session contract.
pub fn geometry_from_header(h: &ShmHeader) -> Option<StreamGeometry> {
    if !valid_header(h) {
        return None;
    }
    if h.magic == SHM_MAGIC_V2 {
        return Some(StreamGeometry::DEFAULT);
    }
    StreamGeometry::new(h.width, h.height, h.fps)
}

/// Returns the address of the 64-byte slot header.
///
/// # Safety
///
/// `view` must point to a mapped SHM region of at least
/// `64 + 64 * SHM_SLOTS` bytes and `slot` must be smaller than [`SHM_SLOTS`],
/// so the returned address lies inside the mapping.
pub unsafe fn slot_ptr(view: *mut u8, slot: u32) -> *mut ShmSlot {
    view.add(64 + 64 * slot as usize) as *mut ShmSlot
}

/// Returns the address of a frame slot's payload.
///
/// # Safety
///
/// `view` must point to a valid mapped SHM region with a readable
/// [`ShmHeader`], `slot` must be smaller than [`SHM_SLOTS`], and the mapping
/// must cover `SHM_DATA_OFF + slot * frame_bytes + frame_bytes` bytes computed
/// from that header.
pub unsafe fn data_ptr(view: *mut u8, slot: u32) -> *mut u8 {
    let frame_bytes = (*(view as *const ShmHeader)).frame_bytes as usize;
    view.add(SHM_DATA_OFF as usize + slot as usize * frame_bytes)
}

/// Current publish counter (`hdr.latest_seq`), read with an aligned volatile load.
///
/// # Safety
///
/// `view` must point to a mapped SHM region containing a readable
/// [`ShmHeader`] for the duration of the call.
pub unsafe fn latest_seq(view: *const u8) -> i32 {
    ptr::read_volatile(&(*(view as *const ShmHeader)).latest_seq)
}

/// Latest published slot index, read with an aligned volatile load.
///
/// # Safety
///
/// `view` must point to a mapped SHM region containing a readable
/// [`ShmHeader`] for the duration of the call.
pub unsafe fn latest_slot(view: *const u8) -> i32 {
    ptr::read_volatile(&(*(view as *const ShmHeader)).latest_slot)
}

/// `VCamPeek`: stable latest publish token, or `None` while the writer is
/// filling.  The token is `(slot, seq)` and is used for dedupe.
///
/// # Safety
///
/// `view` must be null or point to a mapped SHM region whose header passes
/// [`valid_header`]; the whole mapping must stay valid for the call.
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

/// Result of a successful seqlock copy.
#[derive(Clone, Copy, Debug)]
pub struct SlotCopy {
    pub frame_id: u32,
    pub qpc_pub_ns: u64,
}

/// `VCamCopySlot`: copy one frame into `dst` (must hold at least
/// `hdr.frame_bytes` writable bytes) and report whether the copy is stable.
///
/// # Safety
///
/// `view` must be null or point to a valid mapped SHM region whose header and
/// frame slots stay readable for the call.  `dst` must be null or point to at
/// least `hdr.frame_bytes` writable bytes that do not overlap the mapping.
pub unsafe fn copy_slot(view: *mut u8, slot: u32, seq: i32, dst: *mut u8) -> Option<SlotCopy> {
    if dst.is_null() || slot >= SHM_SLOTS {
        return None;
    }
    let s = slot_ptr(view, slot);
    let frame_id = ptr::read_volatile(&(*s).frame_id);
    let qpc_pub_ns = ptr::read_volatile(&(*s).qpc_pub_ns);
    core::sync::atomic::fence(Ordering::SeqCst);
    let frame_bytes = (*(view as *const ShmHeader)).frame_bytes as usize;
    ptr::copy_nonoverlapping(data_ptr(view, slot), dst, frame_bytes);
    core::sync::atomic::fence(Ordering::SeqCst);
    if ptr::read_volatile(&(*s).seq) == seq {
        Some(SlotCopy {
            frame_id,
            qpc_pub_ns,
        })
    } else {
        None
    }
}

// ---- vcam -> receiver feedback ring (`<map>.vcam`) ----

pub const VSTAT_MAGIC: u32 = 0x5453_5656; // "VVST"
pub const VSTAT_CAP: u32 = 1024;
pub const VSTAT_HDR_SIZE: usize = 16;
pub const VSTAT_ENTRY_SIZE: usize = 32;
pub const VSTAT_SIZE: usize = VSTAT_HDR_SIZE + VSTAT_CAP as usize * VSTAT_ENTRY_SIZE;
pub const VSTAT_FLAG_REPEAT: u32 = 1;

/// `vc_vstat_hdr_t` (16 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VstatHeader {
    pub magic: u32,
    pub widx: i32,
    pub cap: u32,
    pub reserved: u32,
}

/// `vc_vstat_entry_t` (32 bytes).
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

/// Initializes the feedback header if this mapping was freshly created.
///
/// # Safety
///
/// `base` must be null or point to at least [`VSTAT_SIZE`] writable bytes
/// that stay mapped for the call.
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

/// Publishes one delivery entry.
///
/// # Safety
///
/// `base` must be null or point to at least [`VSTAT_SIZE`] writable bytes
/// that stay mapped for the call.
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

/// C++ `vc_qpc_to_ns`: avoids `counter * 1e9` overflow.
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
            (3, 2, 60, 9),    // odd width
            (4, 3, 60, 18),   // odd height
            (4, 2, 0, 12),    // fps below range
            (4, 2, 1001, 12), // fps above range
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

        // A changed writer sequence marks the copy as torn.
        unsafe {
            (*slot_ptr(view, 3)).seq = 4;
        }
        assert!(unsafe { copy_slot(view, 3, 2, frame.as_mut_ptr()) }.is_none());

        // Odd (filling) slot is not published.
        unsafe {
            (*slot_ptr(view, 3)).seq = 5;
        }
        assert_eq!(unsafe { peek(view) }, None);

        // Out-of-range slot is ignored.
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

            // Wrap to the same slot; n keeps the global counter.
            *((base.add(4)) as *mut i32) = VSTAT_CAP as i32;
            vstat_push(base, 4, 700, 800, 0);
            let e_last = &*(base.add(VSTAT_HDR_SIZE) as *const VstatEntry);
            assert_eq!(e_last.n, VSTAT_CAP + 1);
            assert_eq!(e_last.frame_id, 4);
        }
    }

    #[test]
    fn qpc_conversion_does_not_overflow() {
        // 10 MHz counter after ~30 min: naive c*1e9 would wrap.
        let ns = qpc_to_ns(18_000_000_000, 10_000_000);
        assert_eq!(ns, 1_800_000_000_000);
        assert_eq!(qpc_to_ns(0, 10_000_000), 0);
        assert_eq!(qpc_to_ns(1, 10_000_000), 100);
    }
}
