//! Windows shared-memory frame publisher for the frozen vcam SHM v3 contract.
//!
//! This is the writer half of `vcam/vcam_shm.h` (the canonical effective layout; that
//! header is authoritative, not `proto/`). It creates one named page-file mapping and
//! the legacy manual-reset event pair, then publishes complete NV12 frames with the
//! same 12-slot seqlock protocol as the C++ receiver's single writer:
//!
//! ```text
//! [0, 64)                         vc_shm_hdr_t
//! [64, 64 + 64 * 12)              vc_shm_slot_t[12]
//! [4096 + i * frame_bytes)        slot i frame bytes
//! ```
//!
//! Publication protocol (one writer thread only; every method takes `&mut self`):
//! - pick the next slot in round-robin order, never the current `latest` slot;
//! - bump `slot.seq` to odd (filling), set the no-hash marker, copy the frame;
//! - fill `frame_id`/`qpc_*`, bump `slot.seq` to even (ready), store `hdr.latest_slot`,
//!   increment `hdr.latest_seq`, then `ResetEvent(evt[(n+1)&1]); SetEvent(evt[n&1])`;
//! - after the latency-sensitive path, [`Publisher::mark_hash`] writes `slot.hash` and
//!   then `slot.hash_fid = frame_id`. A token whose slot has already been republished
//!   is ignored, so a late hasher can never write into a newer frame.
//!
//! Readers (vcam/monitor/verifiers) use `latest_seq` only as a wake hint, identify a
//! publish by `(latest_slot, slot.seq)`, and re-check `slot.seq` around the copy.
//! `slot.hash` is valid for that copy iff `hash_fid == frame_id`. The event pair is
//! manual-reset and session-local; a missed edge is bounded by the reader's timeout.
//!
//! The mapping is created `PAGE_READWRITE` with the caller-supplied name (callers
//! should use `Local\...` for a session-private object). An existing mapping is a
//! hard error: this type never opens, resets, or otherwise disturbs another writer.
//! Events may outlive a previous writer because a downstream reader still holds them;
//! like the legacy writer, we create/open them, reset both, and continue. No registry,
//! service, deployment, or network state is involved.

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

/// `"VCA3"`, matching `VC_SHM_MAGIC`.
const MAGIC: u32 = 0x3341_4356;
/// `VC_SHM_VERSION`.
const VERSION: u32 = 3;
/// `VC_SHM_SLOTS`.
const SLOT_COUNT: usize = 12;
/// `VC_SHM_DATA_OFF`.
const DATA_OFF: usize = 4096;
/// `sizeof(vc_shm_hdr_t)`.
const HEADER_SIZE: usize = 64;
/// `sizeof(vc_shm_slot_t)`.
const SLOT_SIZE: usize = 64;
/// `VC_SHM_HASH_NONE`; also the legacy "not hashed" marker.
const HASH_NONE: u32 = 0xFFFF_FFFF;
/// `vc_shm_valid` accepts 1..=1000.
const MAX_FPS: u32 = 1000;
/// Windows named-kernel-object limit (UTF-16 code units).
const MAX_OBJECT_NAME: usize = 32_767;

/// `vc_shm_hdr_t`, 64 bytes. Atomic fields are only accessed atomically by the writer;
/// readers see them through the legacy plain aligned-read contract.
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

/// `vc_shm_slot_t`, 64 bytes. `seq` is the seqlock (odd = filling, even = stable).
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

/// Marker stored in `hash_fid` before a frame is hashed.
///
/// The native reader's only validity test is `hash_fid == frame_id`, while the legacy
/// marker is `HASH_NONE`. Both collide for `frame_id == u32::MAX`. Keep the legacy
/// marker for every ordinary frame, and use `frame_id ^ HASH_NONE` (zero) only for
/// that one frame, so an old reader never accepts an unhashed slot. After
/// [`Publisher::mark_hash`] the real `frame_id` is stored and the hash becomes valid.
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
    // This must be called before any other Win32 call can overwrite the thread's
    // last-error value (in particular before CloseHandle).
    let code = unsafe { GetLastError() };
    let detail = io::Error::from_raw_os_error(code as i32);
    io::Error::new(
        detail.kind(),
        format!("{operation} failed (Win32 {code}): {detail}"),
    )
}

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// Identifies one published slot generation.
///
/// The token is an owning, `Copy` value with no borrow of the publisher, so it can be
/// moved to a hashing thread and passed back to [`Publisher::mark_hash`] later. It is
/// only meaningful for the publisher that returned it; tokens from any other publisher
/// (and tokens from reused slots) are ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublishToken {
    owner: u64,
    slot: u32,
    seq: u32,
    frame_id: u32,
    serial: u64,
}

impl PublishToken {
    /// Frame id of the publication this token refers to.
    pub fn frame_id(&self) -> u32 {
        self.frame_id
    }
}

/// Single-writer SHM publisher.
///
/// There is intentionally no internal writer lock: `&mut self` serializes publication
/// and [`Publisher::mark_hash`]. Do not clone or share the writer; the slot table and
/// event parity are owned by this object.
pub struct Publisher {
    map: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    events: [HANDLE; 2],
    geometry: Geometry,
    frame_bytes: usize,
    fps: u32,
    /// Next slot to try, giving round-robin reuse.
    cursor: usize,
    /// Slot exposed by `hdr.latest_slot`; never picked for reuse.
    latest: Option<usize>,
    /// Per-slot publication generation (process-local), used to reject stale tokens
    /// even if the 32-bit on-wire sequence wraps.
    serials: [u64; SLOT_COUNT],
    published: u64,
    owner: u64,
}

impl Publisher {
    /// Create the named mapping plus the `<name>.evt0` / `<name>.evt1` event pair.
    ///
    /// Fails closed if the mapping name already exists (another writer or a reader's
    /// view still keeps it alive). All partially created handles are released before
    /// an error is returned. `fps` is the immutable session rate stored in the header
    /// and must satisfy `1..=1000`; `Geometry` already guarantees valid NV12 sizes.
    pub fn new(name: &str, geometry: Geometry, fps: u32) -> io::Result<Self> {
        if fps == 0 || fps > MAX_FPS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("fps must be in 1..={MAX_FPS}, got {fps}"),
            ));
        }

        // Validate/allocate every name before creating any kernel object.
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
        // Check immediately: GetLastError() is meaningful for a successful
        // CreateFileMappingW and any intervening call would overwrite it.
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
                // Release everything created so far. An event may have existed already
                // (held by a downstream reader); closing our duplicate handle does not
                // remove the reader's object.
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
        // A stale event pair may have been left signaled by a previous writer. Legacy
        // behavior is to reset both; existing handles held by a reader stay valid.
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

    /// Copy one complete frame into the next slot and publish it.
    ///
    /// `data.len()` must equal the session `Geometry::frame_bytes()`; `first_ns` is the
    /// caller's QPC timestamp for the first packet of the frame. `now_ns` is called
    /// exactly once, after the copy and immediately before publication, for
    /// `slot.qpc_pub_ns` (same-QPC-clock measurements only).
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
            // even -> odd (filling). If a previous call panicked while filling, the
            // slot is already odd; +2 still changes the value and keeps readers out.
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
            // odd -> even = ready. SeqCst is the full release barrier the legacy
            // InterlockedIncrement provided: all slot fields are visible first.
            seq = (*slot).seq.fetch_add(1, Ordering::SeqCst).wrapping_add(1);

            let header = self.header_ptr();
            // Pointer first, then the monotonic publish counter (same order as C++).
            (*header).latest_slot.store(index as i32, Ordering::SeqCst);
            let n = (*header)
                .latest_seq
                .fetch_add(1, Ordering::SeqCst)
                .wrapping_add(1);
            let parity = (n as u32) & 1;
            // Manual-reset pair: clear the other parity, then wake this publish.
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

    /// Publish the FNV-1a64 result for a previously published frame.
    ///
    /// A stale token (slot reused, another publisher, or wrapped sequence) is silently
    /// ignored. The hash is written before `hash_fid`, so native readers either see no
    /// hash or the complete one. This method never blocks and never faults; it is safe
    /// to call from the writer thread after an off-path hasher returns.
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
            // The slot must still hold exactly the publication this token names.
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

    /// Number of frames successfully published through this mapping.
    pub fn published(&self) -> u64 {
        self.published
    }

    fn init_header(&self) {
        let header = self.header_ptr();
        unsafe {
            // Invalidate the new mapping while it is initialized. A reader can open the
            // name as soon as CreateFileMappingW succeeds and must reject magic 0.
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
                // Fresh mappings are zero-filled; keep the legacy `seq |= 1` (odd =
                // filling, never accepted) and the no-hash marker.
                (*slot).seq.fetch_or(1, Ordering::SeqCst);
                (*slot).hash_fid.store(HASH_NONE, Ordering::Relaxed);
            }

            fence(Ordering::Release);
            ptr::addr_of_mut!((*header).magic).write_volatile(MAGIC);
        }
    }

    /// Round-robin cursor, skipping the slot currently exposed as latest. With twelve
    /// slots and one latest there is always a candidate.
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

    /// `SYNCHRONIZE` from winnt.h; enabling Win32_Storage_FileSystem only for this
    /// constant would add a dependency the publisher itself does not need.
    const SYNCHRONIZE: u32 = 0x0010_0000;

    static NEXT_NAME: AtomicU32 = AtomicU32::new(0);

    /// Session-private names, unique across tests and processes.
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

        // First publish: n = 1 -> evt1 set, evt0 reset.
        assert_eq!(view.u32_at(20), 0);
        assert_eq!(view.i32_at(24), 1);
        // Legacy initializes slots odd (seq = 1), so first acquire adds 2 and the
        // first publication becomes seq 4; every later reuse adds 3 (odd+even).
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

        // Manual-reset: the event remains signaled until the next publish resets it.
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
        // Second publish: n = 2 -> evt0 set, evt1 reset.
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

        // A full ring turn reuses slot 0 while the old token is still outstanding.
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

        // A late result for the old frame is ignored, including its hash marker.
        publisher.mark_hash(old_token, 0xAAAA_BBBB_CCCC_DDDD);
        assert_eq!(view.u64_at(view.slot_offset(0) + 24), 0);
        assert_ne!(view.u32_at(view.slot_offset(0) + 32), 100);

        // The current token still applies, and a still later old result cannot clobber it.
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

        // Ordinary frames keep the legacy 0xFFFFFFFF marker.
        let normal = publisher
            .publish(5, 0, &frame(3, frame_bytes), || 0)
            .unwrap();
        let normal_slot = view.slot_offset(0);
        assert_eq!(view.u32_at(normal_slot + 32), HASH_NONE);
        assert_ne!(view.u32_at(normal_slot + 32), 5);
        publisher.mark_hash(normal, 0x55);
        assert_eq!(view.u32_at(normal_slot + 32), 5);

        // frame_id == HASH_NONE must not look hashed before mark_hash. The marker is
        // frame_id ^ HASH_NONE == 0, which an old reader compares against 0xFFFFFFFF.
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
        // Same slot/seq/frame id/serial, different owner: collision by shape only.
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

        // The original writer is untouched and still owns the ring.
        first.publish(2, 11, &frame(2, frame_bytes), || 21).unwrap();
        assert!(evt0.signaled(), "n = 2 signals evt0");
        assert!(!evt1.signaled());
        assert_eq!(first.published(), 2);

        drop(evt0);
        drop(evt1);
        drop(first);
        // All handles are released on drop, so a fresh writer can take the name.
        let replacement = Publisher::new(&name, geometry, 60).unwrap();
        assert_eq!(replacement.published(), 0);
    }

    #[test]
    fn partial_event_failure_releases_mapping_and_event() {
        let name = unique_name("partial");
        let geometry = test_geometry();
        let blocker_name = format!("{name}.evt1");
        let blocker_w = wide(&blocker_name);

        // Occupy the second event's name with a different kernel object type, so the
        // publisher maps the SHM, creates evt0, then fails on evt1.
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

        // The base mapping, view and evt0 were released: the same name is usable now.
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

        // The failed constructors left no mapping behind.
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
