//! PyroWave 1.x C ABI（`libpyrowave-shared-1.dll`）的 Vulkan 计算编/解码。
//!
//! 当前契约见 `references/encoded-stream.md`：
//! - 实际计算在 Vulkan 1.3 GPU 上（intra-only）；PyroWave 不是固定厂商硬件
//!   编码器，本 crate 也不声称零拷贝；
//! - 阶段一只走 CPU 缓冲：编码上传 NV12、解码同步回读 YUV420P 后再交错成 NV12；
//! - 每帧预算 `min(max_bitrate/8/fps, max_au-48, raw NV12)`，单帧单 packet；
//! - 解码器重置只重建 decoder，保留已创建的 Vulkan 设备；
//! - 设备信息查询 PyroWave 已选中的同一设备，不另建探测设备；
//! - 无 Vulkan 1.3 设备、库缺失或原生调用失败都直接报错，绝不回退 Raw/CPU。

use std::ffi::c_int;
use std::ptr;

use anyhow::{Context, Result, bail, ensure};
use libloading::os::windows::Library;
use serde_json::{Value, json};

use super::OperationState;
use super::deadline::OperationDeadline;
use super::device;
use super::dll::DllDirectory;
use crate::au::{AU_HEADER_BYTES, AuSession, CODEC_PYROWAVE, build_au};
use crate::config::{CodecConfig, pyrowave_frame_budget};

const CANDIDATES: &[&str] = &["libpyrowave-shared-1.dll"];
const PYROWAVE_SUCCESS: c_int = 0;
const PYROWAVE_ERROR_NO_VULKAN: c_int = -5;
const PYROWAVE_CHROMA_SUBSAMPLING_420: c_int = 0;
const PYROWAVE_CPU_BUFFER_FORMAT_NV12: c_int = 0;
const PYROWAVE_CPU_BUFFER_FORMAT_YUV420P: c_int = 1;

type PyroDevice = *mut std::ffi::c_void;
type PyroEncoder = *mut std::ffi::c_void;
type PyroDecoder = *mut std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CpuBuffer {
    data: [*mut std::ffi::c_void; 3],
    row_stride_in_bytes: [usize; 3],
    plane_size_in_bytes: [usize; 3],
    width: c_int,
    height: c_int,
    format: c_int,
}

impl CpuBuffer {
    fn zeroed() -> Self {
        Self {
            data: [ptr::null_mut(); 3],
            row_stride_in_bytes: [0; 3],
            plane_size_in_bytes: [0; 3],
            width: 0,
            height: 0,
            format: 0,
        }
    }
}

#[repr(C)]
struct RateControl {
    maximum_bitstream_size: usize,
}

#[repr(C)]
struct EncoderCreateInfo {
    device: PyroDevice,
    width: c_int,
    height: c_int,
    chroma: c_int,
}

#[repr(C)]
struct DecoderCreateInfo {
    device: PyroDevice,
    width: c_int,
    height: c_int,
    chroma: c_int,
    fragment_path: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Packet {
    offset: usize,
    size: usize,
}

type GetApiVersion = unsafe extern "C" fn(*mut u32, *mut u32, *mut u32);
type CreateDefaultDevice = unsafe extern "C" fn(*mut PyroDevice) -> c_int;
type GetVkDeviceHandles = unsafe extern "C" fn(
    PyroDevice,
    *mut *mut std::ffi::c_void,
    *mut *mut std::ffi::c_void,
    *mut *mut std::ffi::c_void,
);
type DestroyDevice = unsafe extern "C" fn(PyroDevice);
type EncoderCreate = unsafe extern "C" fn(*const EncoderCreateInfo, *mut PyroEncoder) -> c_int;
type EncoderDestroy = unsafe extern "C" fn(PyroEncoder);
type EncoderEncodeCpu =
    unsafe extern "C" fn(PyroEncoder, *const CpuBuffer, *const RateControl) -> c_int;
type EncoderComputeNumPackets = unsafe extern "C" fn(PyroEncoder, usize, *mut usize) -> c_int;
type EncoderPacketize = unsafe extern "C" fn(
    PyroEncoder,
    *mut Packet,
    usize,
    *mut usize,
    *mut std::ffi::c_void,
    usize,
) -> c_int;
type DecoderCreate = unsafe extern "C" fn(*const DecoderCreateInfo, *mut PyroDecoder) -> c_int;
type DecoderDestroy = unsafe extern "C" fn(PyroDecoder);
type DecoderPushPacket = unsafe extern "C" fn(PyroDecoder, *const std::ffi::c_void, usize) -> c_int;
type DecoderDecodeIsReady = unsafe extern "C" fn(PyroDecoder, bool) -> bool;
type DecoderDecodeCpuBufferSynchronous =
    unsafe extern "C" fn(PyroDecoder, *const CpuBuffer) -> c_int;

struct PyroApi {
    get_api_version: GetApiVersion,
    create_default_device: CreateDefaultDevice,
    device_get_vk_device_handles: GetVkDeviceHandles,
    device_destroy: DestroyDevice,
    encoder_create: EncoderCreate,
    encoder_destroy: EncoderDestroy,
    encoder_encode_cpu: EncoderEncodeCpu,
    encoder_compute_num_packets: EncoderComputeNumPackets,
    encoder_packetize: EncoderPacketize,
    decoder_create: DecoderCreate,
    decoder_destroy: DecoderDestroy,
    decoder_push_packet: DecoderPushPacket,
    decoder_decode_is_ready: DecoderDecodeIsReady,
    decoder_decode_cpu_buffer_synchronous: DecoderDecodeCpuBufferSynchronous,
}

impl PyroApi {
    unsafe fn load(library: &Library) -> Result<Self> {
        unsafe fn symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T> {
            // SAFETY: 调用方保证 DLL 生命周期覆盖所有函数指针使用。
            let symbol = unsafe { library.get::<T>(name) }.with_context(|| {
                format!(
                    "PyroWave DLL 缺少符号 {}",
                    String::from_utf8_lossy(&name[..name.len().saturating_sub(1)])
                )
            })?;
            Ok(*symbol)
        }
        Ok(Self {
            get_api_version: unsafe { symbol(library, b"pyrowave_get_api_version\0") }?,
            create_default_device: unsafe { symbol(library, b"pyrowave_create_default_device\0") }?,
            device_get_vk_device_handles: unsafe {
                symbol(library, b"pyrowave_device_get_vk_device_handles\0")
            }?,
            device_destroy: unsafe { symbol(library, b"pyrowave_device_destroy\0") }?,
            encoder_create: unsafe { symbol(library, b"pyrowave_encoder_create\0") }?,
            encoder_destroy: unsafe { symbol(library, b"pyrowave_encoder_destroy\0") }?,
            encoder_encode_cpu: unsafe { symbol(library, b"pyrowave_encoder_encode_cpu\0") }?,
            encoder_compute_num_packets: unsafe {
                symbol(library, b"pyrowave_encoder_compute_num_packets\0")
            }?,
            encoder_packetize: unsafe { symbol(library, b"pyrowave_encoder_packetize\0") }?,
            decoder_create: unsafe { symbol(library, b"pyrowave_decoder_create\0") }?,
            decoder_destroy: unsafe { symbol(library, b"pyrowave_decoder_destroy\0") }?,
            decoder_push_packet: unsafe { symbol(library, b"pyrowave_decoder_push_packet\0") }?,
            decoder_decode_is_ready: unsafe {
                symbol(library, b"pyrowave_decoder_decode_is_ready\0")
            }?,
            decoder_decode_cpu_buffer_synchronous: unsafe {
                symbol(library, b"pyrowave_decoder_decode_cpu_buffer_synchronous\0")
            }?,
        })
    }
}

fn result_text(result: c_int) -> &'static str {
    match result {
        PYROWAVE_SUCCESS => "ok",
        1 => "timeout",
        -1 => "generic error",
        -2 => "invalid argument",
        -3 => "out of host memory",
        -4 => "out of device memory",
        PYROWAVE_ERROR_NO_VULKAN => "no Vulkan device",
        -6 => "not implemented",
        -7 => "unsupported external handle",
        -8 => "failed external handle",
        _ => "unknown error",
    }
}

fn check(result: c_int, what: &str) -> Result<()> {
    if result != PYROWAVE_SUCCESS {
        bail!("PyroWave {what}: {} ({result})", result_text(result));
    }
    Ok(())
}

fn verify_api_version(api: &PyroApi) -> Result<()> {
    let mut major = 0u32;
    let mut minor = 0u32;
    let mut patch = 0u32;
    // SAFETY: 稳定入口点，只写三个 u32。
    unsafe { (api.get_api_version)(&mut major, &mut minor, &mut patch) };
    ensure!(
        major == 1,
        "PyroWave ABI 主版本 {major}.{minor}.{patch} 不受支持（需要 1.x）"
    );
    Ok(())
}

fn device_json(api: &PyroApi, device: PyroDevice) -> Value {
    let mut instance: *mut std::ffi::c_void = ptr::null_mut();
    let mut physical: *mut std::ffi::c_void = ptr::null_mut();
    // SAFETY: device 由 PyroWave 创建；只查询句柄。
    unsafe {
        (api.device_get_vk_device_handles)(device, &mut instance, &mut physical, ptr::null_mut())
    };
    device::settings(instance, physical)
}

/// GPU-only 必须以已选设备的真实属性为证；未知或软件设备不能靠后端名称蒙混过关。
fn require_gpu_device(info: &Value) -> Result<()> {
    ensure!(
        info["status"] == "selected"
            && matches!(
                info["type"].as_str(),
                Some("integrated_gpu" | "discrete_gpu")
            ),
        "PyroWave GPU-only 要求可确认的集成或独立 GPU，拒绝 CPU/虚拟/未知 Vulkan 设备：{info}"
    );
    let api = info["api_version"].as_u64().unwrap_or(0);
    ensure!(
        api <= u32::MAX as u64 && api >> 29 == 0 && api >= ((1 << 22) | (3 << 12)),
        "PyroWave 需要 Vulkan 1.3 或更高版本的 GPU，实际 API 值为 {api}"
    );
    Ok(())
}

fn load_device(api: &PyroApi) -> Result<(PyroDevice, Value)> {
    let mut device: PyroDevice = ptr::null_mut();
    check(
        // SAFETY: 标准设备创建入口。
        unsafe { (api.create_default_device)(&mut device) },
        "设备创建",
    )?;
    ensure!(!device.is_null(), "PyroWave 返回了空设备句柄");
    let info = device_json(api, device);
    if let Err(error) = require_gpu_device(&info) {
        // 安全：尚未创建编解码器；拒绝的软件/未知设备必须在本次受看门狗保护的操作内释放。
        unsafe { (api.device_destroy)(device) };
        return Err(error);
    }
    Ok((device, info))
}

/// 创建编码器；失败路径在同一同步操作内清理已创建的句柄与设备。
fn create_encoder(
    api: &PyroApi,
    width: u32,
    height: u32,
) -> Result<(PyroDevice, PyroEncoder, Value)> {
    let (device, device_info) = load_device(api)?;
    let create = EncoderCreateInfo {
        device,
        width: width as c_int,
        height: height as c_int,
        chroma: PYROWAVE_CHROMA_SUBSAMPLING_420,
    };
    let mut encoder: PyroEncoder = ptr::null_mut();
    // SAFETY: create info 已按 C 布局填写。
    let result = unsafe { (api.encoder_create)(&create, &mut encoder) };
    if result != PYROWAVE_SUCCESS {
        // 与 C++ 析构路径一致：失败时清理可能已创建的句柄与设备。
        if !encoder.is_null() {
            unsafe { (api.encoder_destroy)(encoder) };
        }
        unsafe { (api.device_destroy)(device) };
        bail!("PyroWave 编码器创建: {} ({result})", result_text(result));
    }
    if encoder.is_null() {
        unsafe { (api.device_destroy)(device) };
        bail!("PyroWave 返回了空编码器句柄");
    }
    Ok((device, encoder, device_info))
}

pub(super) struct PyroWaveEncoder {
    // 通常在 Drop 时随结构卸载；粘性超时后改为有意泄漏，见 Drop。
    _library: Option<Library>,
    api: PyroApi,
    device: PyroDevice,
    encoder: PyroEncoder,
    width: u32,
    height: u32,
    fps: u32,
    budget: usize,
    bitstream: Vec<u8>,
    device_info: Value,
    deadline: OperationDeadline,
}

impl PyroWaveEncoder {
    pub(super) fn open(
        directory: &DllDirectory,
        config: &CodecConfig,
        width: u32,
        height: u32,
        fps: u32,
        state: &OperationState,
    ) -> Result<Self> {
        let library = directory.load(CANDIDATES, "PyroWave 运行库")?;
        // SAFETY: DLL 由 `library` 持有；设备/编码器在 Drop 中销毁。
        let api = unsafe { PyroApi::load(&library)? };
        verify_api_version(&api)?;
        let budget = pyrowave_frame_budget(config, width, height, fps)?;
        ensure!(budget >= 1024, "PyroWave 每帧预算 {budget} 字节低于 1024");
        let mut deadline = OperationDeadline::start_observed(state)?;
        // 设备与编码器的创建（含失败清理）同属一次同步操作。超时后看门狗只置
        // 会话粘性标志；原生句柄可能已挂起，绝不在此后再调用销毁（由父层保持存活）。
        let created = {
            let _armed = deadline.begin("创建")?;
            create_encoder(&api, width, height)
        };
        if let Err(error) = deadline.check() {
            // 超时可能发生在创建/失败清理的原生调用中：保持 DLL 加载，避免在
            // 孤立/挂起的原生句柄仍存在时执行卸载（父层保持后端存活）。
            std::mem::forget(library);
            return Err(error);
        }
        let (device, encoder, device_info) = created?;
        Ok(Self {
            _library: Some(library),
            api,
            device,
            encoder,
            width,
            height,
            fps,
            budget: budget as usize,
            bitstream: vec![0u8; budget as usize],
            device_info,
            deadline,
        })
    }

    pub(super) fn encode(&mut self, nv12: &[u8], frame_id: u32) -> Result<Vec<u8>> {
        // 编码/取包整体是一个同步操作；超时只由看门狗设置会话粘性标志，不终止进程。
        let outcome = {
            let _armed = self.deadline.begin("编码")?;
            self.encode_native(nv12, frame_id)
        };
        // 调用期间发生过超时：以粘性错误覆盖原生返回值，后续调用同样被拒绝。
        self.deadline.check()?;
        outcome
    }

    /// 用同几何合成 NV12 真实编码一次，强制创建 Vulkan 计算管线。
    ///
    /// 返回的访问单元由调用方丢弃；正式帧号、统计和哈希仍从 0 开始。预热与正式
    /// 编码使用同一份配置/帧预算，不改变任何正式编码参数。
    pub(super) fn warmup(&mut self, nv12: &[u8], frame_id: u32) -> Result<Vec<u8>> {
        let outcome = {
            let _armed = self.deadline.begin("预热")?;
            self.encode_native(nv12, frame_id)
        };
        self.deadline.check()?;
        outcome
    }

    fn encode_native(&mut self, nv12: &[u8], frame_id: u32) -> Result<Vec<u8>> {
        let y_bytes = self.width as usize * self.height as usize;
        ensure!(
            nv12.len() == y_bytes + y_bytes / 2,
            "PyroWave 需要完整 NV12：{}x{} 需 {} 字节，实际 {}",
            self.width,
            self.height,
            y_bytes + y_bytes / 2,
            nv12.len()
        );
        let mut source = CpuBuffer::zeroed();
        source.data[0] = nv12.as_ptr() as *mut std::ffi::c_void;
        source.data[1] = unsafe { nv12.as_ptr().add(y_bytes) } as *mut std::ffi::c_void;
        source.row_stride_in_bytes[0] = self.width as usize;
        source.row_stride_in_bytes[1] = self.width as usize;
        source.plane_size_in_bytes[0] = y_bytes;
        source.plane_size_in_bytes[1] = y_bytes / 2;
        source.width = self.width as c_int;
        source.height = self.height as c_int;
        source.format = PYROWAVE_CPU_BUFFER_FORMAT_NV12;
        let rate = RateControl {
            maximum_bitstream_size: self.budget,
        };
        check(
            // SAFETY: NV12 两个平面按契约指向调用方输入。
            unsafe { (self.api.encoder_encode_cpu)(self.encoder, &source, &rate) },
            "encode",
        )?;

        let mut count = 0usize;
        check(
            // SAFETY: 仅在上一次编码成功后调用。
            unsafe {
                (self.api.encoder_compute_num_packets)(self.encoder, self.budget, &mut count)
            },
            "packet count",
        )?;
        ensure!(
            count == 1,
            "PyroWave 帧被拆成 {count} 个 packet；当前只接受单 packet"
        );
        let mut packet = Packet { offset: 0, size: 0 };
        let mut packet_count = 1usize;
        check(
            // SAFETY: 输出缓冲固定为 budget 字节；packet_count 为输入/输出。
            unsafe {
                (self.api.encoder_packetize)(
                    self.encoder,
                    &mut packet,
                    self.budget,
                    &mut packet_count,
                    self.bitstream.as_mut_ptr().cast(),
                    self.bitstream.len(),
                )
            },
            "packetize",
        )?;
        ensure!(
            packet_count == 1 && packet.offset == 0,
            "PyroWave packet 布局异常：count={packet_count} offset={}",
            packet.offset
        );
        let end = packet
            .offset
            .checked_add(packet.size)
            .context("PyroWave packet 偏移溢出")?;
        ensure!(
            end <= self.bitstream.len(),
            "PyroWave packet 越出 bitstream 缓冲"
        );
        let payload = &self.bitstream[packet.offset..end];
        ensure!(
            payload.len() >= 8,
            "PyroWave bitstream 至少 8 字节（当前 {} 字节）",
            payload.len()
        );
        let total = AU_HEADER_BYTES + payload.len();
        ensure!(
            total <= self.budget + AU_HEADER_BYTES,
            "PyroWave 访问单元 {total} 字节超过每帧预算 {} + 48",
            self.budget
        );
        build_au(
            AuSession {
                codec: CODEC_PYROWAVE,
                width: self.width,
                height: self.height,
                fps: self.fps,
            },
            frame_id,
            payload,
        )
    }

    pub(super) fn settings(&self) -> Value {
        json!({
            "family": "pyrowave",
            "identity": crate::config::PYROWAVE_IDENTITY,
            "revision": 1,
            "width": self.width,
            "height": self.height,
            "fps": self.fps,
            "subsampling": "420",
            "max_bytes_per_frame": self.budget,
            "upload_path": "cpu_nv12",
            "decode_path": "cpu_yuv420p",
            "compute": "vulkan",
            "gpu_compute": true,
            "vulkan_min_version": "1.3",
            "zero_copy": false,
            "device": self.device_info.clone(),
        })
    }
}

impl Drop for PyroWaveEncoder {
    fn drop(&mut self) {
        // 销毁也是同步调用，必须受同一个 5 秒看门狗覆盖；已粘性超时则不再触碰原生后端。
        if !self.deadline.timed_out()
            && let Ok(_armed) = self.deadline.begin("销毁")
        {
            if !self.encoder.is_null() {
                // SAFETY: 编码器由本结构独占；先销毁编码器再销毁设备。
                unsafe { (self.api.encoder_destroy)(self.encoder) };
                self.encoder = ptr::null_mut();
            }
            if !self.device.is_null() {
                // SAFETY: 同上。
                unsafe { (self.api.device_destroy)(self.device) };
                self.device = ptr::null_mut();
            }
        }
        // 会话已粘性超时：原生后端可能永久挂起。不 join 任何原生线程，也不卸载
        // 仍可能持有挂起 GPU 对象的 DLL；句柄与 DLL 由父层保持存活（有意泄漏）。
        if self.deadline.timed_out()
            && let Some(library) = self._library.take()
        {
            std::mem::forget(library);
        }
    }
}

pub(super) struct PyroWaveDecoder {
    // 通常在 Drop 时随结构卸载；粘性超时后改为有意泄漏，见 Drop。
    _library: Option<Library>,
    api: PyroApi,
    device: PyroDevice,
    decoder: PyroDecoder,
    width: u32,
    height: u32,
    fps: u32,
    plane_cb: Vec<u8>,
    plane_cr: Vec<u8>,
    device_info: Value,
    deadline: OperationDeadline,
}

impl PyroWaveDecoder {
    pub(super) fn open(
        directory: &DllDirectory,
        width: u32,
        height: u32,
        fps: u32,
        state: &OperationState,
    ) -> Result<Self> {
        let library = directory.load(CANDIDATES, "PyroWave 运行库")?;
        // SAFETY: DLL 由 `library` 持有；设备/解码器在 Drop 中销毁。
        let api = unsafe { PyroApi::load(&library)? };
        verify_api_version(&api)?;
        let mut deadline = OperationDeadline::start_observed(state)?;
        // 设备与解码器的创建（含失败清理）同属一次同步操作；超时后不再销毁挂起句柄。
        let created = {
            let _armed = deadline.begin("创建")?;
            load_decoder(&api, width, height)
        };
        if let Err(error) = deadline.check() {
            // 超时可能发生在创建/失败清理的原生调用中：保持 DLL 加载，避免在
            // 孤立/挂起的原生句柄仍存在时执行卸载（父层保持后端存活）。
            std::mem::forget(library);
            return Err(error);
        }
        let (device, decoder, device_info) = created?;
        let y_bytes = width as usize * height as usize;
        Ok(Self {
            _library: Some(library),
            api,
            device,
            decoder,
            width,
            height,
            fps,
            plane_cb: vec![0; y_bytes / 4],
            plane_cr: vec![0; y_bytes / 4],
            device_info,
            deadline,
        })
    }

    /// 解码已通过 VCAU 封装的单 packet bitstream，返回完整 NV12（含亮度）。
    pub(super) fn decode(&mut self, bitstream: &[u8]) -> Result<Vec<u8>> {
        // push/就绪检查/同步解码/回读是一个整体同步操作；超时只置会话粘性标志。
        let outcome = {
            let _armed = self.deadline.begin("解码")?;
            self.decode_native(bitstream)
        };
        self.deadline.check()?;
        outcome
    }

    /// 用真实单 packet bitstream 解码一次同几何合成帧，强制创建解码管线。
    ///
    /// 输出由调用方丢弃；预热不是正式帧，不进入统计、哈希或帧号序列。
    pub(super) fn warmup(&mut self, bitstream: &[u8]) -> Result<Vec<u8>> {
        let outcome = {
            let _armed = self.deadline.begin("预热")?;
            self.decode_native(bitstream)
        };
        self.deadline.check()?;
        outcome
    }

    fn decode_native(&mut self, bitstream: &[u8]) -> Result<Vec<u8>> {
        ensure!(!self.decoder.is_null(), "PyroWave 解码器未打开");
        check(
            // SAFETY: bitstream 借用调用方载荷，调用期间有效。
            unsafe {
                (self.api.decoder_push_packet)(
                    self.decoder,
                    bitstream.as_ptr().cast(),
                    bitstream.len(),
                )
            },
            "push packet",
        )?;
        // SAFETY: 只读查询当前帧是否完整。
        if !unsafe { (self.api.decoder_decode_is_ready)(self.decoder, false) } {
            bail!("PyroWave 帧不完整（缺少 packet）");
        }
        let y_bytes = self.width as usize * self.height as usize;
        let mut out = vec![0u8; y_bytes + y_bytes / 2];
        let mut destination = CpuBuffer::zeroed();
        destination.data[0] = out.as_mut_ptr().cast();
        destination.data[1] = self.plane_cb.as_mut_ptr().cast();
        destination.data[2] = self.plane_cr.as_mut_ptr().cast();
        destination.row_stride_in_bytes[0] = self.width as usize;
        destination.row_stride_in_bytes[1] = self.width as usize / 2;
        destination.row_stride_in_bytes[2] = self.width as usize / 2;
        destination.plane_size_in_bytes[0] = y_bytes;
        destination.plane_size_in_bytes[1] = self.plane_cb.len();
        destination.plane_size_in_bytes[2] = self.plane_cr.len();
        destination.width = self.width as c_int;
        destination.height = self.height as c_int;
        destination.format = PYROWAVE_CPU_BUFFER_FORMAT_YUV420P;
        check(
            // SAFETY: 三个目标平面互不重叠，大小与 stride 已按契约填写。
            unsafe { (self.api.decoder_decode_cpu_buffer_synchronous)(self.decoder, &destination) },
            "decode",
        )?;
        // Cb/Cr → NV12 UV 交错；亮度已直接写入 out 的 Y 平面，绝不留旧数据。
        let chroma_width = self.width as usize / 2;
        let uv = y_bytes;
        for row in 0..self.height as usize / 2 {
            let cb = &self.plane_cb[row * chroma_width..(row + 1) * chroma_width];
            let cr = &self.plane_cr[row * chroma_width..(row + 1) * chroma_width];
            let target =
                &mut out[uv + row * self.width as usize..uv + (row + 1) * self.width as usize];
            for column in 0..chroma_width {
                target[2 * column] = cb[column];
                target[2 * column + 1] = cr[column];
            }
        }
        Ok(out)
    }

    /// 丢弃未完成 packet 并重建解码器；保留已选中的 Vulkan 设备。
    pub(super) fn reset(&mut self) -> Result<()> {
        // 销毁与重建是同一个同步操作；超时后不再触碰可能挂起的原生后端。
        let outcome = {
            let _armed = self.deadline.begin("重置")?;
            self.reset_native()
        };
        self.deadline.check()?;
        outcome
    }

    fn reset_native(&mut self) -> Result<()> {
        if !self.decoder.is_null() {
            // SAFETY: 解码器由本结构独占。
            unsafe { (self.api.decoder_destroy)(self.decoder) };
            self.decoder = ptr::null_mut();
        }
        match create_decoder(&self.api, self.device, self.width, self.height) {
            Ok(decoder) => {
                self.decoder = decoder;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn settings(&self) -> Value {
        json!({
            "family": "pyrowave",
            "identity": crate::config::PYROWAVE_IDENTITY,
            "revision": 1,
            "width": self.width,
            "height": self.height,
            "fps": self.fps,
            "subsampling": "420",
            // 解码端没有编码预算；双端逐项确认时编码端回显实际值。
            "max_bytes_per_frame": 0,
            "upload_path": "cpu_nv12",
            "decode_path": "cpu_yuv420p",
            "compute": "vulkan",
            "gpu_compute": true,
            "vulkan_min_version": "1.3",
            "zero_copy": false,
            "device": self.device_info.clone(),
        })
    }
}

/// 创建解码器会话；失败路径在同一同步操作内销毁尚未交给解码器的设备。
fn load_decoder(
    api: &PyroApi,
    width: u32,
    height: u32,
) -> Result<(PyroDevice, PyroDecoder, Value)> {
    let (device, device_info) = load_device(api)?;
    match create_decoder(api, device, width, height) {
        Ok(decoder) => Ok((device, decoder, device_info)),
        Err(error) => {
            // SAFETY: 设备尚未交给任何解码器。
            unsafe { (api.device_destroy)(device) };
            Err(error)
        }
    }
}

fn create_decoder(
    api: &PyroApi,
    device: PyroDevice,
    width: u32,
    height: u32,
) -> Result<PyroDecoder> {
    ensure!(!device.is_null(), "PyroWave 设备已销毁，无法重建解码器");
    let create = DecoderCreateInfo {
        device,
        width: width as c_int,
        height: height as c_int,
        chroma: PYROWAVE_CHROMA_SUBSAMPLING_420,
        fragment_path: false,
    };
    let mut decoder: PyroDecoder = ptr::null_mut();
    // SAFETY: create info 已按 C 布局填写。
    check(
        unsafe { (api.decoder_create)(&create, &mut decoder) },
        "解码器创建",
    )?;
    ensure!(!decoder.is_null(), "PyroWave 返回了空解码器句柄");
    Ok(decoder)
}

impl Drop for PyroWaveDecoder {
    fn drop(&mut self) {
        // 销毁也是同步调用，必须受同一个 5 秒看门狗覆盖；已粘性超时则不再触碰原生后端。
        if !self.deadline.timed_out()
            && let Ok(_armed) = self.deadline.begin("销毁")
        {
            if !self.decoder.is_null() {
                // SAFETY: 解码器由本结构独占；先销毁解码器再销毁设备。
                unsafe { (self.api.decoder_destroy)(self.decoder) };
                self.decoder = ptr::null_mut();
            }
            if !self.device.is_null() {
                // SAFETY: 同上。
                unsafe { (self.api.device_destroy)(self.device) };
                self.device = ptr::null_mut();
            }
        }
        // 会话已粘性超时：原生后端可能永久挂起。不 join 任何原生线程，也不卸载
        // 仍可能持有挂起 GPU 对象的 DLL；句柄与 DLL 由父层保持存活（有意泄漏）。
        if self.deadline.timed_out()
            && let Some(library) = self._library.take()
        {
            std::mem::forget(library);
        }
    }
}

#[cfg(test)]
mod gpu_requirement_tests {
    use super::*;

    #[test]
    fn accepts_only_proven_hardware_gpu_with_vulkan_1_3() {
        for kind in ["integrated_gpu", "discrete_gpu"] {
            let info =
                json!({"status":"selected", "type":kind, "api_version":(1u32 << 22) | (3 << 12)});
            assert!(require_gpu_device(&info).is_ok());
        }
        for kind in ["cpu", "virtual_gpu", "other", ""] {
            let info =
                json!({"status":"selected", "type":kind, "api_version":(1u32 << 22) | (3 << 12)});
            assert!(require_gpu_device(&info).is_err());
        }
    }

    #[test]
    fn unknown_device_or_old_vulkan_cannot_bypass_gpu_only() {
        for info in [
            json!({"status":"unavailable"}),
            json!({"status":"selected", "type":"discrete_gpu"}),
            json!({"status":"selected", "type":"discrete_gpu", "api_version":(1u32 << 22) | (2 << 12)}),
            json!({"status":"selected", "type":"discrete_gpu", "api_version":u64::MAX}),
        ] {
            assert!(require_gpu_device(&info).is_err());
        }
    }
}
