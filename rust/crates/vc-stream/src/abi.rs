//! 版本化 C ABI。调用方拥有文本缓冲/内存，并且必须保持 DLL 存活，直到所有
//! 句柄被销毁。Start 只是排队一个 worker，不保证流已就绪。worker 仍在收尾
//! 或处在驱动调用中时，Stop 与 destroy 返回 1。

use std::ffi::{CStr, c_char, c_void};
use std::fs::{File, OpenOptions};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde::Deserialize;
use serde_json::{Value, json};
use vc_media::{CodecConfig, OperationState};
use vc_source::SourceConfig;

use crate::windows::{self, CommonArgs, Progress, ReceiveArgs, SendArgs};

pub const ABI_VERSION: u32 = 1;
const ROLE_SENDER: u32 = 1;
const ROLE_RECEIVER: u32 = 2;

pub type TextQuery =
    unsafe extern "C" fn(*mut c_char, u32, *mut u32, *mut c_char, u32, *mut u32) -> i32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VcStreamApiV1 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub list_adapters: TextQuery,
    pub start_session: unsafe extern "C" fn(
        u32,
        *const c_char,
        *mut *mut c_void,
        *mut c_char,
        u32,
        *mut u32,
    ) -> i32,
    pub session_status: unsafe extern "C" fn(
        *mut c_void,
        *mut c_char,
        u32,
        *mut u32,
        *mut c_char,
        u32,
        *mut u32,
    ) -> i32,
    pub stop_session: unsafe extern "C" fn(*mut c_void, *mut c_char, u32, *mut u32) -> i32,
    pub destroy_session: unsafe extern "C" fn(*mut c_void) -> i32,
    pub capabilities: TextQuery,
}

static API_V1: VcStreamApiV1 = VcStreamApiV1 {
    abi_version: ABI_VERSION,
    struct_size: std::mem::size_of::<VcStreamApiV1>() as u32,
    list_adapters,
    start_session,
    session_status,
    stop_session,
    destroy_session,
    capabilities,
};

#[unsafe(no_mangle)]
pub extern "C" fn vc_module_get_api(requested_version: u32) -> *const VcStreamApiV1 {
    if requested_version == ABI_VERSION {
        &API_V1
    } else {
        ptr::null()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SenderRequest {
    adapter: String,
    width: u32,
    height: u32,
    fps: u32,
    mtu: usize,
    session: String,
    frames: u64,
    #[serde(default)]
    pacing_us: u64,
    source: SourceConfig,
    codec: CodecConfig,
    source_mac: [u8; 6],
    dst_mac: [u8; 6],
    stats: PathBuf,
    hashlog: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiverRequest {
    adapter: String,
    width: u32,
    height: u32,
    fps: u32,
    mtu: usize,
    session: String,
    codec: CodecConfig,
    source_mac: [u8; 6],
    #[serde(default)]
    expect_frames: u64,
    #[serde(default)]
    timeout_s: u64,
    shm: String,
    stats: PathBuf,
    hashlog: PathBuf,
}

enum Worker {
    Sender(SendArgs),
    Receiver(ReceiveArgs),
}

struct Session {
    thread: Mutex<Option<JoinHandle<()>>>,
    stop: Arc<AtomicBool>,
    completion: Arc<Mutex<Option<Result<(), String>>>>,
    progress: Arc<Progress>,
    operation: OperationState,
}

impl Session {
    fn stop(&self) -> bool {
        self.stop.store(true, Ordering::Release);
        let mut thread = self.thread.lock().unwrap_or_else(|e| e.into_inner());
        if thread.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return false;
        }
        if let Some(worker) = thread.take() {
            let _ = worker.join();
        }
        true
    }
}

unsafe fn c_text(value: *const c_char) -> Result<&'static str, String> {
    if value.is_null() {
        return Err("request JSON is null".into());
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map_err(|_| "request JSON must be UTF-8".into())
}

unsafe fn write_text(output: *mut c_char, capacity: u32, written: *mut u32, text: &str) -> i32 {
    if written.is_null() {
        return -1;
    }
    let Ok(required) = u32::try_from(text.len().saturating_add(1)) else {
        return -1;
    };
    unsafe {
        *written = required;
    }
    if capacity < required || output.is_null() {
        return 1;
    }
    unsafe {
        ptr::copy_nonoverlapping(text.as_ptr(), output.cast::<u8>(), text.len());
        *output.add(text.len()) = 0;
    }
    0
}

unsafe fn failure(output: *mut c_char, capacity: u32, written: *mut u32, text: &str) -> i32 {
    if !written.is_null() {
        unsafe {
            *written = text.len().saturating_add(1).min(u32::MAX as usize) as u32;
        }
    }
    if !output.is_null() && capacity > 0 {
        let mut count = text.len().min(capacity as usize - 1);
        while !text.is_char_boundary(count) {
            count -= 1;
        }
        unsafe {
            ptr::copy_nonoverlapping(text.as_ptr(), output.cast::<u8>(), count);
            *output.add(count) = 0;
        }
    }
    -1
}

fn common(
    adapter: String,
    width: u32,
    height: u32,
    fps: u32,
    mtu: usize,
    session: String,
) -> CommonArgs {
    CommonArgs {
        adapter,
        width,
        height,
        fps,
        mtu,
        session,
        ethertype: vc_transport::wire::ETHERTYPE,
    }
}

fn prepare_worker(role: u32, text: &str) -> Result<Worker, String> {
    match role {
        ROLE_SENDER => {
            let r: SenderRequest =
                serde_json::from_str(text).map_err(|e| format!("invalid sender request: {e}"))?;
            let args = SendArgs {
                common: common(r.adapter, r.width, r.height, r.fps, r.mtu, r.session),
                frames: r.frames,
                batch: None,
                pacing_us: r.pacing_us,
                pattern: r.source.pattern.parse()?,
                source: r.source,
                codec: r.codec,
                source_mac: r.source_mac,
                dst_mac: r.dst_mac,
                stats: Some(r.stats),
                hashlog: Some(r.hashlog),
                ready: None,
                stop: None,
                progress: None,
                hash_file: None,
                stats_file: None,
                operation: OperationState::new(),
            };
            windows::validate_sender(&args).map_err(|e| e.to_string())?;
            Ok(Worker::Sender(args))
        }
        ROLE_RECEIVER => {
            let r: ReceiverRequest =
                serde_json::from_str(text).map_err(|e| format!("invalid receiver request: {e}"))?;
            if r.shm.is_empty() || r.shm.contains('\0') {
                return Err("SHM name is empty or contains NUL".into());
            }
            let args = ReceiveArgs {
                common: common(r.adapter, r.width, r.height, r.fps, r.mtu, r.session),
                source_mac: Some(r.source_mac),
                expect_frames: r.expect_frames,
                timeout_s: r.timeout_s,
                ready: None,
                stats: Some(r.stats),
                hashlog: Some(r.hashlog),
                shm: Some(r.shm),
                codec: r.codec,
                stop: None,
                progress: None,
                hash_file: None,
                stats_file: None,
                transport_file: None,
                operation: OperationState::new(),
            };
            windows::validate_receiver(&args).map_err(|e| e.to_string())?;
            Ok(Worker::Receiver(args))
        }
        _ => Err(format!("unsupported stream role {role}")),
    }
}

fn reserve(path: &Path) -> Result<File, String> {
    if !path.is_absolute() {
        return Err(format!(
            "evidence path must be absolute: {}",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create evidence directory: {e}"))?;
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            format!(
                "create evidence {} (existing evidence is never overwritten): {e}",
                path.display()
            )
        })
}

/// 预约本次会话的全部证据文件；任一失败时只回滚本次调用刚创建的空文件。
///
/// 已有证据在调用方的存在性检查中已被拒绝；这里删除的只是本次 `create_new`
/// 成功创建、尚未写入任何数据的文件，避免一次失败的启动用空文件占住 session id。
fn reserve_evidence(
    stats: &Path,
    hashlog: &Path,
    transport: Option<&Path>,
) -> Result<(File, File, Option<File>), String> {
    let stats_file = reserve(stats)?;
    let hash_file = match reserve(hashlog) {
        Ok(file) => file,
        Err(error) => {
            drop(stats_file);
            discard_reserved(&[stats]);
            return Err(error);
        }
    };
    let transport_file = match transport.map(reserve).transpose() {
        Ok(file) => file,
        Err(error) => {
            drop(stats_file);
            drop(hash_file);
            discard_reserved(&[stats, hashlog]);
            return Err(error);
        }
    };
    Ok((stats_file, hash_file, transport_file))
}

/// 删除本次启动刚预约的空证据文件；调用方必须先关闭/确认这些文件由自己创建。
fn discard_reserved(paths: &[&Path]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

unsafe extern "C" fn list_adapters(
    output: *mut c_char,
    capacity: u32,
    written: *mut u32,
    error: *mut c_char,
    error_capacity: u32,
    error_written: *mut u32,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<String, String> {
        let api = windows::Npcap::load().map_err(|e| e.to_string())?;
        let mut items = api.adapters().map_err(|e| e.to_string())?;
        items.sort_by(|a, b| a.0.cmp(&b.0));
        serde_json::to_string(&items).map_err(|e| e.to_string())
    }));
    match result {
        Ok(Ok(text)) => unsafe { write_text(output, capacity, written, &text) },
        Ok(Err(text)) => unsafe { failure(error, error_capacity, error_written, &text) },
        Err(_) => unsafe {
            failure(
                error,
                error_capacity,
                error_written,
                "panic while enumerating Npcap adapters",
            )
        },
    }
}

unsafe extern "C" fn capabilities(
    output: *mut c_char,
    capacity: u32,
    written: *mut u32,
    error: *mut c_char,
    error_capacity: u32,
    error_written: *mut u32,
) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| windows::capabilities().to_string())) {
        Ok(text) => unsafe { write_text(output, capacity, written, &text) },
        Err(_) => unsafe {
            failure(
                error,
                error_capacity,
                error_written,
                "panic while reading capabilities",
            )
        },
    }
}

unsafe extern "C" fn start_session(
    role: u32,
    request_json: *const c_char,
    output_handle: *mut *mut c_void,
    error_buffer: *mut c_char,
    error_capacity: u32,
    error_written: *mut u32,
) -> i32 {
    if output_handle.is_null() {
        return unsafe {
            failure(
                error_buffer,
                error_capacity,
                error_written,
                "output handle is null",
            )
        };
    }
    unsafe {
        *output_handle = ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<*mut c_void, String> {
        windows::initialize_timebase()
            .map_err(|error| format!("initialize QPC timebase: {error:#}"))?;
        let mut worker = prepare_worker(role, unsafe { c_text(request_json)? })?;
        let (args, stats, hashlog, encoded_receiver) = match &worker {
            Worker::Sender(a) => (
                &a.common,
                a.stats.as_ref().unwrap(),
                a.hashlog.as_ref().unwrap(),
                false,
            ),
            Worker::Receiver(a) => (
                &a.common,
                a.stats.as_ref().unwrap(),
                a.hashlog.as_ref().unwrap(),
                a.codec.codec != "raw",
            ),
        };
        let session_id = args.session.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let operation = OperationState::new();
        let completion = Arc::new(Mutex::new(None));
        let progress = Arc::new(Progress::new(
            json!({"ready":false,"session_id":session_id,"adapter":args.adapter}),
        ));
        let transport_path = encoded_receiver.then(|| windows::transport_path(hashlog));
        if [stats, hashlog]
            .into_iter()
            .chain(transport_path.as_ref())
            .any(|p| p.exists())
        {
            return Err("session evidence already exists; refusing to overwrite it".into());
        }
        let (stats_file, hash_file, transport_file) =
            reserve_evidence(stats, hashlog, transport_path.as_deref())?;
        // 线程启动失败时要回滚本次预约；届时 worker 已被 move 进闭包，
        // 不能再用借自 worker 的路径，先取出拥有所有权的副本。
        let stats_rollback = stats.to_path_buf();
        let hashlog_rollback = hashlog.to_path_buf();
        let transport_rollback = transport_path.clone();
        match &mut worker {
            Worker::Sender(a) => {
                a.stop = Some(stop.clone());
                a.progress = Some(progress.clone());
                a.operation = operation.clone();
                a.hash_file = Some(hash_file);
                a.stats_file = Some(stats_file);
            }
            Worker::Receiver(a) => {
                a.stop = Some(stop.clone());
                a.progress = Some(progress.clone());
                a.operation = operation.clone();
                a.hash_file = Some(hash_file);
                a.stats_file = Some(stats_file);
                a.transport_file = transport_file;
            }
        }
        let thread_completion = completion.clone();
        let join = match std::thread::Builder::new()
            .name(format!("vc-stream-{session_id}"))
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| match worker {
                    Worker::Sender(a) => windows::send(a),
                    Worker::Receiver(a) => windows::receive(a),
                }))
                .map_err(|_| "data-plane worker panicked".to_owned())
                .and_then(|result| result.map_err(|error| format!("{error:#}")));
                *thread_completion.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            }) {
            Ok(join) => join,
            Err(error) => {
                // 线程没有起来：闭包已被丢弃并关闭句柄，回滚本次预约的空文件。
                discard_reserved(&[stats_rollback.as_path(), hashlog_rollback.as_path()]);
                if let Some(path) = transport_rollback.as_deref() {
                    discard_reserved(&[path]);
                }
                return Err(format!("start worker thread: {error}"));
            }
        };
        let session = Box::new(Session {
            thread: Mutex::new(Some(join)),
            stop,
            completion,
            progress,
            operation,
        });
        Ok(Box::into_raw(session).cast::<c_void>())
    }));
    match result {
        Ok(Ok(handle)) => {
            unsafe {
                *output_handle = handle;
            }
            0
        }
        Ok(Err(text)) => unsafe { failure(error_buffer, error_capacity, error_written, &text) },
        Err(_) => unsafe {
            failure(
                error_buffer,
                error_capacity,
                error_written,
                "panic while starting session",
            )
        },
    }
}

unsafe fn session_ref<'a>(handle: *mut c_void) -> Result<&'a Session, String> {
    if handle.is_null() {
        Err("session handle is null".into())
    } else {
        Ok(unsafe { &*handle.cast::<Session>() })
    }
}

unsafe extern "C" fn session_status(
    handle: *mut c_void,
    output: *mut c_char,
    capacity: u32,
    written: *mut u32,
    error: *mut c_char,
    error_capacity: u32,
    error_written: *mut u32,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<String, String> {
        let session = unsafe { session_ref(handle)? };
        let completion = session.completion.lock().unwrap_or_else(|e| e.into_inner());
        let mut status = session.progress.snapshot();
        status["running"] = json!(completion.is_none());
        status["stopping"] = json!(completion.is_none() && session.stop.load(Ordering::Acquire));
        status["pid"] = json!(std::process::id());
        status["module"] = json!("vc_stream.dll");
        status["abi_version"] = json!(ABI_VERSION);
        if session.operation.timed_out() {
            session.stop.store(true, Ordering::Release);
            status["failed"] = json!(true);
            status["cleanup_pending"] = json!(completion.is_none());
            status["operation_timeout"] = json!(session.operation.timed_out_operation());
            status["error"] = json!(format!(
                "PyroWave {}操作超过5秒；保留活动worker与DLL等待清理，未终止宿主",
                session.operation.timed_out_operation().unwrap_or("原生")
            ));
        }
        status["exit_code"] = match completion.as_ref() {
            None => Value::Null,
            Some(Ok(())) => json!(0),
            Some(Err(text)) => {
                status["error"] = json!(text);
                json!(1)
            }
        };
        serde_json::to_string(&status).map_err(|e| e.to_string())
    }));
    match result {
        Ok(Ok(text)) => unsafe { write_text(output, capacity, written, &text) },
        Ok(Err(text)) => unsafe { failure(error, error_capacity, error_written, &text) },
        Err(_) => unsafe {
            failure(
                error,
                error_capacity,
                error_written,
                "panic while reading session status",
            )
        },
    }
}

unsafe extern "C" fn stop_session(
    handle: *mut c_void,
    error: *mut c_char,
    capacity: u32,
    written: *mut u32,
) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| {
        unsafe { session_ref(handle) }.map(Session::stop)
    })) {
        Ok(Ok(joined)) => i32::from(!joined),
        Ok(Err(text)) => unsafe { failure(error, capacity, written, &text) },
        Err(_) => unsafe { failure(error, capacity, written, "panic while stopping session") },
    }
}

unsafe extern "C" fn destroy_session(handle: *mut c_void) -> i32 {
    if handle.is_null() {
        return -1;
    }
    catch_unwind(AssertUnwindSafe(|| {
        let session = unsafe { &*handle.cast::<Session>() };
        if !session.stop() {
            return 1;
        }
        drop(unsafe { Box::from_raw(handle.cast::<Session>()) });
        0
    }))
    .unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_negotiation_rejects_unknown_abi() {
        assert!(!vc_module_get_api(ABI_VERSION).is_null());
        assert!(vc_module_get_api(ABI_VERSION + 1).is_null());
    }
    #[test]
    fn start_failure_always_clears_handle() {
        let mut handle = ptr::dangling_mut::<c_void>();
        let mut written = 0;
        let mut error = [0i8; 256];
        let code = unsafe {
            start_session(
                99,
                c"{}".as_ptr(),
                &mut handle,
                error.as_mut_ptr(),
                256,
                &mut written,
            )
        };
        assert_eq!(code, -1);
        assert!(handle.is_null());
        assert!(written > 1);
    }
    #[test]
    fn errors_are_negative_even_with_small_buffers() {
        let mut output = [0i8; 4];
        let mut written = 0;
        assert_eq!(
            unsafe { failure(output.as_mut_ptr(), 4, &mut written, "failure") },
            -1
        );
        assert_eq!(output[3], 0);
        assert_eq!(written, 8);
    }
    #[test]
    fn stop_never_destroys_running_worker() {
        let (tx, rx) = std::sync::mpsc::channel();
        let session = Box::new(Session {
            thread: Mutex::new(Some(std::thread::spawn(move || rx.recv().unwrap()))),
            stop: Arc::new(AtomicBool::new(false)),
            completion: Arc::new(Mutex::new(None)),
            progress: Arc::new(Progress::new(json!({}))),
            operation: OperationState::new(),
        });
        let ptr = Box::into_raw(session).cast::<c_void>();
        assert_eq!(unsafe { destroy_session(ptr) }, 1);
        assert!(
            unsafe { session_ref(ptr) }
                .unwrap()
                .stop
                .load(Ordering::Acquire)
        );
        tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while unsafe { destroy_session(ptr) } == 1 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    fn unique_directory(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("vc-abi-{tag}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn failed_reservation_rolls_back_only_the_files_it_created() {
        let directory = unique_directory("reserve-rollback");

        // hashlog 已有证据：stats 预约成功后必须回滚，旧 hashlog 原样保留。
        let stats = directory.join("s.stats.json");
        let hashlog = directory.join("h.hash.jsonl");
        std::fs::write(&hashlog, b"keep").unwrap();
        let error = reserve_evidence(&stats, &hashlog, None).unwrap_err();
        assert!(error.contains("existing evidence"));
        assert!(
            !stats.exists(),
            "failed reservation must remove its empty stats file"
        );
        assert_eq!(std::fs::read(&hashlog).unwrap(), b"keep");

        // transport 预约失败：本次创建的 stats/hashlog 都回滚，旧 transport 保留。
        let stats = directory.join("s2.stats.json");
        let hashlog = directory.join("h2.hash.jsonl");
        let transport = directory.join("h2.transport.jsonl");
        std::fs::write(&transport, b"old").unwrap();
        reserve_evidence(&stats, &hashlog, Some(&transport)).unwrap_err();
        assert!(!stats.exists());
        assert!(!hashlog.exists());
        assert_eq!(std::fs::read(&transport).unwrap(), b"old");

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn successful_reservation_returns_all_three_files() {
        let directory = unique_directory("reserve-ok");
        let stats = directory.join("s.stats.json");
        let hashlog = directory.join("h.hash.jsonl");
        let transport = directory.join("h.transport.jsonl");
        let (stats_file, hash_file, transport_file) =
            reserve_evidence(&stats, &hashlog, Some(&transport)).unwrap();
        assert!(stats.exists() && hashlog.exists() && transport.exists());
        drop((stats_file, hash_file, transport_file));
        let _ = std::fs::remove_dir_all(&directory);
    }
}
