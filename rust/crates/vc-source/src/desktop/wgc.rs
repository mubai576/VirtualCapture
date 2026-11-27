//! Windows Graphics Capture 与硬件 GPU 转换；旧 `sender/wgc.h` 仅保留在 Git 历史。
//!
//! 与 DDA 同构：帧来自 DWM 合成，窗口模式按标题子串选择（不含本进程窗口）。
//! 帧池回调始终把最新帧放进 `pending`，采集线程最多等待 20 ms；`Closed` 或
//! 回调错误是终结状态，绝不降级成“空闲超时”。尺寸变化时只在采集线程重建
//! 帧池，重建期间回调不消费旧池。

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEXTURE2D_DESC, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindow, IsWindowVisible,
};
use windows::core::{BOOL, HRESULT, IInspectable, Interface, factory};

use super::gpu::GpuConverter;
use super::util::create_device;
use crate::SourceConfig;
use crate::transform::{Transform, TransformMode};

const CAPTURE_TIMEOUT: Duration = Duration::from_millis(20);
/// 重建帧池前等待在途回调退出 `pool_gate` 的上限（与 native 的 1s 一致）。
const POOL_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const RO_E_CLOSED: HRESULT = HRESULT(0x8000_0013_u32 as i32);

/// 帧纹理尺寸 = 帧池尺寸，而 `ContentSize` 才是窗口真正有定义的像素区域。
fn content_extent(content_w: i32, content_h: i32, tex_w: u32, tex_h: u32) -> (u32, u32) {
    if content_w < 2 || content_h < 2 || tex_w < 2 || tex_h < 2 {
        return (0, 0);
    }
    let width = (content_w as u32).min(tex_w);
    let height = (content_h as u32).min(tex_h);
    if width < 2 || height < 2 {
        (0, 0)
    } else {
        (width, height)
    }
}

fn needs_pool_recreate(content_w: i32, content_h: i32, pool_w: u32, pool_h: u32) -> bool {
    // 最小化/过渡状态可能给出 <2 的 ContentSize；native 只在尺寸有效且变化时
    // 重建帧池，无效尺寸留给 content_extent 判为“本帧无新图像”。
    if content_w < 2 || content_h < 2 {
        return false;
    }
    content_w as u32 != pool_w || content_h as u32 != pool_h
}

struct WgcInner {
    pending: Option<Direct3D11CaptureFrame>,
    /// 仅用于让等待方在每次通知后重新检查 pending/终结标志。
    signal: u64,
}

/// FrameArrived / Closed 回调共享状态。
///
/// 回调只持有本对象的 `Arc`，不捕获 `WgcSource`；撤销事件 token 后已排队的
/// delegate 仍可安全访问这里。`pool_gate` 把 `TryGetNextFrame` 与帧池重建
/// 串行化，避免在旧池上取帧。
struct WgcSharedState {
    inner: Mutex<WgcInner>,
    frame_ready: Condvar,
    pool_gate: Mutex<()>,
    closing: AtomicBool,
    recreating: AtomicBool,
    item_closed: AtomicBool,
    callback_failed: AtomicBool,
    callback_code: AtomicI32,
    skipped: Arc<AtomicU64>,
}

impl WgcSharedState {
    fn new(skipped: Arc<AtomicU64>) -> Self {
        Self {
            inner: Mutex::new(WgcInner {
                pending: None,
                signal: 0,
            }),
            frame_ready: Condvar::new(),
            pool_gate: Mutex::new(()),
            closing: AtomicBool::new(false),
            recreating: AtomicBool::new(false),
            item_closed: AtomicBool::new(false),
            callback_failed: AtomicBool::new(false),
            callback_code: AtomicI32::new(0),
            skipped,
        }
    }

    fn lock_inner(&self) -> MutexGuard<'_, WgcInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn terminal(&self) -> bool {
        self.closing.load(Ordering::Acquire)
            || self.item_closed.load(Ordering::Acquire)
            || self.callback_failed.load(Ordering::Acquire)
    }

    fn fail(&self, code: HRESULT) {
        if code == RO_E_CLOSED {
            self.item_closed.store(true, Ordering::Release);
        } else {
            self.callback_code.store(code.0, Ordering::Release);
            self.callback_failed.store(true, Ordering::Release);
        }
        self.frame_ready.notify_all();
    }

    fn on_closed(&self) {
        self.item_closed.store(true, Ordering::Release);
        self.frame_ready.notify_all();
    }

    fn on_frame(&self, pool: &Direct3D11CaptureFramePool) {
        if self.closing.load(Ordering::Acquire) || self.recreating.load(Ordering::Acquire) {
            return;
        }
        let _gate = self
            .pool_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.closing.load(Ordering::Acquire) || self.recreating.load(Ordering::Acquire) {
            return;
        }
        let frame = match pool.TryGetNextFrame() {
            Ok(frame) => frame,
            Err(error) if is_no_frame_error(&error) => {
                // windows-rs 把 WinRT 的“S_OK + null out 参数”映射为
                // `Error::empty()`（code() == S_OK）。native 对空帧直接忽略，
                // 这里同样不算回调失败；真正的失败 HRESULT 为负值。
                return;
            }
            Err(error) => {
                self.fail(error.code());
                return;
            }
        };
        let previous = {
            let mut inner = self.lock_inner();
            if self.closing.load(Ordering::Acquire) {
                drop(inner);
                let _ = frame.Close();
                return;
            }
            let previous = inner.pending.replace(frame);
            if previous.is_some() {
                self.skipped.fetch_add(1, Ordering::Relaxed);
            }
            inner.signal = inner.signal.wrapping_add(1);
            previous
        };
        if let Some(previous) = previous {
            let _ = previous.Close();
        }
        self.frame_ready.notify_all();
    }

    /// 等待 pending 帧或截止时间/终结状态，返回 `None` 时由调用方区分原因。
    fn wait_frame(&self, deadline: Instant) -> Option<Direct3D11CaptureFrame> {
        let mut inner = self.lock_inner();
        loop {
            if let Some(frame) = inner.pending.take() {
                return Some(frame);
            }
            if self.terminal() {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _) = match self.frame_ready.wait_timeout(inner, deadline - now) {
                Ok(value) => value,
                Err(poisoned) => poisoned.into_inner(),
            };
            inner = guard;
        }
    }
}

/// `TryGetNextFrame` 的“当前无帧”在 windows-rs 中是 `Error::empty()`
/// （WinRT S_OK + null out 参数），其 `code()` 为 S_OK；失败 HRESULT 为负值。
fn is_no_frame_error(error: &windows::core::Error) -> bool {
    error.code().is_ok()
}

/// 有界地等待在途回调退出 `pool_gate`；超时显式失败，绝不无期限阻塞采集线程。
fn lock_pool_gate(shared: &WgcSharedState) -> Result<MutexGuard<'_, ()>> {
    let deadline = Instant::now() + POOL_DRAIN_TIMEOUT;
    loop {
        match shared.pool_gate.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    bail!("WGC callback drain timed out before pool recreate");
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

/// 按标题子串（大小写不敏感）查找可见、未最小化、非 cloaked、非本进程窗口。
struct WindowSearch {
    needle: String,
    found: Option<(HWND, u32)>,
}

unsafe extern "system" fn enum_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut WindowSearch) };
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() || unsafe { IsIconic(hwnd) }.as_bool() {
        return BOOL(1);
    }
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return BOOL(1);
    }
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if copied <= 0 {
        return BOOL(1);
    }
    buffer.truncate(copied as usize);
    let title = String::from_utf16_lossy(&buffer).to_lowercase();
    if !title.contains(&search.needle) {
        return BOOL(1);
    }
    let mut cloaked = BOOL(0);
    let cloaked_result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut BOOL as *mut c_void,
            size_of::<BOOL>() as u32,
        )
    };
    if cloaked_result.is_ok() && cloaked.as_bool() {
        return BOOL(1);
    }
    let mut process_id = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if process_id == unsafe { GetCurrentProcessId() } {
        return BOOL(1);
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err()
        || rect.right - rect.left < 2
        || rect.bottom - rect.top < 2
    {
        return BOOL(1);
    }
    search.found = Some((hwnd, process_id));
    BOOL(0)
}

fn find_window(needle: &str) -> Option<(HWND, u32)> {
    let mut search = WindowSearch {
        needle: needle.to_lowercase(),
        found: None,
    };
    let _ = unsafe {
        EnumWindows(
            Some(enum_window),
            LPARAM(&mut search as *mut WindowSearch as isize),
        )
    };
    search.found
}

fn frame_handler(
    state: &Arc<WgcSharedState>,
) -> TypedEventHandler<Direct3D11CaptureFramePool, IInspectable> {
    let state = Arc::clone(state);
    TypedEventHandler::new(
        move |sender: windows::core::Ref<'_, Direct3D11CaptureFramePool>, _| {
            if let Some(pool) = sender.as_ref() {
                state.on_frame(pool);
            }
            Ok(())
        },
    )
}

fn closed_handler(
    state: &Arc<WgcSharedState>,
) -> TypedEventHandler<GraphicsCaptureItem, IInspectable> {
    let state = Arc::clone(state);
    TypedEventHandler::new(move |_, _| {
        state.on_closed();
        Ok(())
    })
}

pub(crate) struct WgcSource {
    rt_device: IDirect3DDevice,
    item: GraphicsCaptureItem,
    pool: Option<Direct3D11CaptureFramePool>,
    session: Option<GraphicsCaptureSession>,
    frame_token: Option<i64>,
    closed_token: Option<i64>,
    shared: Arc<WgcSharedState>,
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
    transform_mode: TransformMode,
    /// 桌面采集固定走硬件 GPU 转换，没有 `Option` 或 CPU 兜底。
    gpu: GpuConverter,
    adapter_name: String,
    pool_width: u32,
    pool_height: u32,
    target_window: Option<HWND>,
    target_process: u32,
}

impl WgcSource {
    pub(crate) fn open(
        config: &SourceConfig,
        width: u32,
        height: u32,
        transform_mode: TransformMode,
        skipped: Arc<AtomicU64>,
    ) -> Result<Self> {
        // CreateFreeThreaded 要求 MTA。若当前线程已被初始化为 STA，显式失败
        // （native 的 init_apartment 同样抛错），不静默改用其它公寓。
        if let Err(error) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            if error.code() == RPC_E_CHANGED_MODE {
                bail!("WinRT apartment is not multi-threaded; WGC requires an MTA thread: {error}");
            }
            return Err(error).context("RoInitialize(RO_INIT_MULTITHREADED) failed");
        }

        let device = create_device()?;
        // FreeThreaded 帧池会在内部线程使用同一 D3D11 设备；即时上下文默认
        // 不保护并发访问，必须先启用驱动锁，不能仅靠 pending/pool_gate 的 Rust 锁。
        let multithread: ID3D11Multithread = device
            .context
            .cast()
            .context("WGC immediate context lacks ID3D11Multithread")?;
        let _ = unsafe { multithread.SetMultithreadProtected(true) };
        if !unsafe { multithread.GetMultithreadProtected() }.as_bool() {
            bail!("WGC could not enable D3D11 multithread protection");
        }
        let adapter_name = device.adapter_name.clone();
        let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice = device
            .device
            .cast()
            .context("WGC D3D11 device is not an IDXGIDevice")?;
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .context("CreateDirect3D11DeviceFromDXGIDevice failed")?;
        let rt_device: IDirect3DDevice = inspectable
            .cast()
            .context("WinRT Direct3D device cast failed")?;
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .context("GraphicsCaptureItem activation factory failed")?;

        let (item, target_window, target_process) = if config.window.is_empty() {
            let output = unsafe { device.adapter.EnumOutputs(config.output) }
                .with_context(|| format!("WGC output {} is not available", config.output))?;
            let output_desc =
                unsafe { output.GetDesc() }.context("WGC IDXGIOutput::GetDesc failed")?;
            let monitor = output_desc.Monitor;
            if monitor.is_invalid() {
                bail!("WGC output {} has no monitor", config.output);
            }
            let item = unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(monitor) }
                .with_context(|| {
                    format!(
                        "WGC CreateForMonitor failed for output {} (closed or protected content?)",
                        config.output
                    )
                })?;
            (item, None, 0)
        } else {
            let (window, process_id) = find_window(&config.window).with_context(|| {
                format!(
                    "WGC window not found (visible, non-minimized title substring): {:?}",
                    config.window
                )
            })?;
            let item = unsafe { interop.CreateForWindow::<GraphicsCaptureItem>(window) }
                .with_context(|| {
                    format!(
                        "WGC CreateForWindow failed for title substring {:?} (closed or protected content?)",
                        config.window
                    )
                })?;
            (item, Some(window), process_id)
        };

        let item_size = item
            .Size()
            .context("WGC GraphicsCaptureItem::Size failed")?;
        if item_size.Width < 2 || item_size.Height < 2 {
            bail!(
                "WGC capture item has unsupported size {}x{}",
                item_size.Width,
                item_size.Height
            );
        }
        let source_width = item_size.Width as u32;
        let source_height = item_size.Height as u32;
        Transform::new(source_width, source_height, width, height, transform_mode)?;

        // GPU 转换器先于帧池/会话建立：失败时不会留下已启动的采集会话，
        // 且失败即打开失败，不换 CPU/WARP。
        let gpu = GpuConverter::new(&device.device, &device.context, width, height).context(
            "WGC GPU 转换器初始化失败（桌面采集必须使用硬件 D3D11 GPU，不回退 CPU/WARP）",
        )?;

        let shared = Arc::new(WgcSharedState::new(skipped.clone()));
        let closed_token = item.Closed(&closed_handler(&shared)).context(
            "WGC capture item Closed handler registration failed (item already closed?)",
        )?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &rt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            item_size,
        )
        .context("WGC Direct3D11CaptureFramePool::CreateFreeThreaded failed")?;
        let frame_token = pool
            .FrameArrived(&frame_handler(&shared))
            .context("WGC FrameArrived handler registration failed")?;
        let session = pool
            .CreateCaptureSession(&item)
            .context("WGC CreateCaptureSession failed")?;
        // 光标默认包含；边框尽量关闭，Win10 无该属性时保持系统默认。
        let _ = session.SetIsCursorCaptureEnabled(true);
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture().context("WGC StartCapture failed")?;

        Ok(Self {
            rt_device,
            item,
            pool: Some(pool),
            session: Some(session),
            frame_token: Some(frame_token),
            closed_token: Some(closed_token),
            shared,
            source_width,
            source_height,
            output_width: width,
            output_height: height,
            transform_mode,
            gpu,
            adapter_name,
            pool_width: source_width,
            pool_height: source_height,
            target_window,
            target_process,
        })
    }

    pub(crate) fn source_dimensions(&self) -> (u32, u32) {
        (self.source_width, self.source_height)
    }

    pub(crate) fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    pub(crate) fn capture(
        &mut self,
        destination: &mut [u8],
        stop: Option<&AtomicBool>,
    ) -> Result<bool> {
        if stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
            return Ok(false);
        }
        if self.pool.is_none() {
            bail!("WGC not initialized");
        }
        self.check_window_alive();
        self.check_terminal()?;
        let deadline = Instant::now() + CAPTURE_TIMEOUT;
        let Some(frame) = self.shared.wait_frame(deadline) else {
            // 终结状态优先于空闲超时，不能把 Closed/回调错误伪装成 false。
            self.check_terminal()?;
            return Ok(false);
        };
        if let Err(error) = self.check_terminal() {
            let _ = frame.Close();
            return Err(error);
        }

        let prepared = (|| -> Result<(ID3D11Texture2D, u32, u32, i32, i32)> {
            let surface = frame.Surface().context("WGC frame Surface failed")?;
            let access: IDirect3DDxgiInterfaceAccess = surface
                .cast()
                .context("WGC frame surface lacks IDirect3DDxgiInterfaceAccess")?;
            let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
                .context("WGC frame surface is not ID3D11Texture2D")?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                bail!("WGC frame format {:?} is not B8G8R8A8_UNORM", desc.Format);
            }
            let content = frame
                .ContentSize()
                .context("WGC Direct3D11CaptureFrame::ContentSize failed")?;
            Ok((
                texture,
                desc.Width,
                desc.Height,
                content.Width,
                content.Height,
            ))
        })();

        let (texture, texture_width, texture_height, content_width, content_height) = match prepared
        {
            Ok(value) => value,
            Err(error) => {
                let _ = frame.Close();
                return Err(error);
            }
        };

        if needs_pool_recreate(
            content_width,
            content_height,
            self.pool_width,
            self.pool_height,
        ) {
            // 尺寸变化：本帧作废，先释放旧池引用再在采集线程重建。
            let _ = frame.Close();
            drop(texture);
            let width = u32::try_from(content_width)
                .ok()
                .filter(|value| *value >= 2)
                .context("WGC frame ContentSize is invalid for frame pool recreation")?;
            let height = u32::try_from(content_height)
                .ok()
                .filter(|value| *value >= 2)
                .context("WGC frame ContentSize is invalid for frame pool recreation")?;
            self.recreate_pool(width, height)?;
            return Ok(false);
        }
        if content_width > texture_width as i32 || content_height > texture_height as i32 {
            let _ = frame.Close();
            bail!("WGC frame texture smaller than ContentSize");
        }
        let (source_width, source_height) =
            content_extent(content_width, content_height, texture_width, texture_height);
        if source_width < 2 || source_height < 2 {
            // 最小化/无定义内容与 native 一致：本帧不算新图像，只等待下一帧。
            let _ = frame.Close();
            self.check_terminal()?;
            return Ok(false);
        }
        let transform = Transform::new(
            source_width,
            source_height,
            self.output_width,
            self.output_height,
            self.transform_mode,
        )
        .context("WGC source/output geometry is unsupported")?;

        let result = self
            .gpu
            .convert(&texture, &transform, destination, stop)
            .context("WGC GPU 转换失败；桌面采集不回退 CPU/WARP");
        let _ = frame.Close();
        let captured = result?;
        if captured {
            self.source_width = source_width;
            self.source_height = source_height;
        }
        Ok(captured)
    }

    /// 在采集线程（不是 FrameArrived 回调）重建帧池。先撤销 FrameArrived，
    /// 再通过 `pool_gate` 等待在途回调退出；失败为致命错误。
    fn recreate_pool(&mut self, width: u32, height: u32) -> Result<()> {
        let pool = self
            .pool
            .as_ref()
            .context("WGC frame pool is not initialized")?;
        let shared = Arc::clone(&self.shared);
        shared.recreating.store(true, Ordering::Release);
        if let Some(token) = self.frame_token.take() {
            let _ = pool.RemoveFrameArrived(token);
        }
        let gate = match lock_pool_gate(&shared) {
            Ok(gate) => gate,
            Err(error) => {
                // 与 native 一致：drain 失败后恢复回调，采集调用方收到致命错误。
                shared.recreating.store(false, Ordering::Release);
                return Err(error);
            }
        };
        {
            let mut inner = shared.lock_inner();
            if let Some(frame) = inner.pending.take() {
                let _ = frame.Close();
            }
            inner.signal = inner.signal.wrapping_add(1);
        }
        let result = (|| -> Result<()> {
            pool.Recreate(
                &self.rt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                SizeInt32 {
                    Width: width as i32,
                    Height: height as i32,
                },
            )
            .context("WGC frame pool Recreate failed")?;
            let token = pool
                .FrameArrived(&frame_handler(&shared))
                .context("WGC FrameArrived re-registration failed")?;
            self.frame_token = Some(token);
            self.pool_width = width;
            self.pool_height = height;
            // native 在重建成功时同步更新采集尺寸；下一帧转换前 stats 不再报旧值。
            self.source_width = width;
            self.source_height = height;
            Ok(())
        })();
        // 在仍持有 gate 时恢复 `recreating`，避免新回调观察到“未重建但旧池仍被锁”的窗口。
        shared.recreating.store(false, Ordering::Release);
        drop(gate);
        result
    }

    fn check_window_alive(&self) {
        let Some(window) = self.target_window else {
            return;
        };
        let mut process_id = 0u32;
        let alive = unsafe { IsWindow(Some(window)) }.as_bool()
            && unsafe { GetWindowThreadProcessId(window, Some(&mut process_id)) } != 0
            && process_id == self.target_process;
        if !alive {
            // Closed 事件在部分 Windows/驱动组合下不会及时派发；窗口句柄
            // 失效或 PID 变化一律视为采集项关闭，绝不重新按标题找窗口。
            self.shared.on_closed();
        }
    }

    fn check_terminal(&self) -> Result<()> {
        if self.shared.item_closed.load(Ordering::Acquire) {
            bail!("WGC capture item closed");
        }
        if self.shared.callback_failed.load(Ordering::Acquire) {
            bail!(
                "WGC frame callback failed (hr=0x{:08X})",
                self.shared.callback_code.load(Ordering::Acquire) as u32
            );
        }
        Ok(())
    }
}

impl Drop for WgcSource {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        self.shared.frame_ready.notify_all();
        if let Some(pool) = &self.pool
            && let Some(token) = self.frame_token.take()
        {
            let _ = pool.RemoveFrameArrived(token);
        }
        if let Some(token) = self.closed_token.take() {
            let _ = self.item.RemoveClosed(token);
        }
        // 在关闭会话/帧池前有界排空已进入 pool_gate 的回调，并保持与回调一致的锁顺序。
        let gate = lock_pool_gate(&self.shared).ok();
        if let Some(session) = self.session.take() {
            let _ = session.Close();
        }
        if let Some(pool) = self.pool.take() {
            let _ = pool.Close();
        }
        drop(gate);
        let mut inner = self.shared.lock_inner();
        if let Some(frame) = inner.pending.take() {
            let _ = frame.Close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_content_size_never_recreates_pool() {
        // 最小化/过渡态（<2）与 native 一致：不重建、不致命，只等下一帧。
        assert!(!needs_pool_recreate(0, 100, 640, 480));
        assert!(!needs_pool_recreate(100, 0, 640, 480));
        assert!(!needs_pool_recreate(-7, 100, 640, 480));
        assert!(!needs_pool_recreate(100, -1, 640, 480));
    }

    #[test]
    fn content_size_change_triggers_pool_recreate() {
        assert!(!needs_pool_recreate(640, 480, 640, 480));
        assert!(needs_pool_recreate(641, 480, 640, 480));
        assert!(needs_pool_recreate(640, 481, 640, 480));
        assert!(needs_pool_recreate(1920, 1080, 640, 480));
    }

    #[test]
    fn content_extent_clamps_to_texture_and_rejects_invalid() {
        assert_eq!(content_extent(640, 480, 640, 480), (640, 480));
        assert_eq!(content_extent(100, 50, 640, 480), (100, 50));
        // ContentSize 大于纹理：只保留纹理内的有效区域。
        assert_eq!(content_extent(700, 500, 640, 480), (640, 480));
        // ContentSize 或纹理 <2 都视为无有效内容。
        assert_eq!(content_extent(1, 50, 640, 480), (0, 0));
        assert_eq!(content_extent(50, 1, 640, 480), (0, 0));
        assert_eq!(content_extent(50, 50, 1, 480), (0, 0));
        assert_eq!(content_extent(50, 50, 640, 1), (0, 0));
        assert_eq!(content_extent(-10, 50, 640, 480), (0, 0));
    }

    #[test]
    fn pool_gate_drain_times_out_instead_of_blocking_forever() {
        let shared = WgcSharedState::new(Arc::new(AtomicU64::new(0)));
        let held = shared
            .pool_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let error = lock_pool_gate(&shared).expect_err("gate is held");
        assert!(error.to_string().contains("drain timed out"));
        drop(held);
        assert!(lock_pool_gate(&shared).is_ok());
    }

    #[test]
    fn empty_winrt_result_is_not_a_callback_failure() {
        assert!(is_no_frame_error(&windows::core::Error::empty()));
        let failure: windows::core::Error = HRESULT(RO_E_CLOSED.0).into();
        assert!(!is_no_frame_error(&failure));
    }
}
