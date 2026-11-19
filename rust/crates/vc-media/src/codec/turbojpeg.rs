//! TurboJPEG 3.1.2 C ABI 的 MJPEG 编/解码（CPU/SIMD，4:2:0，固定质量）。
//!
//! 与 `common/mjpeg_codec.h` 对齐：
//! - 编码端 NV12 拆成 Y/Cb/Cr 平面，`tj3CompressFromYUVPlanes8` 直接编码 YUV420，
//!   `TJPARAM_NOREALLOC` + 固定输出缓冲，超限即失败，不重分配/不降质量。
//! - 解码端严格校验 baseline 8-bit YCbCr420、会话精确尺寸、非渐进/非算术，
//!   再解码到 Y/Cb/Cr 并交错回 NV12；不做 RGB 中转或范围转换。
//! - 每帧独立，失败不会自动换后端。

use std::ffi::{CStr, c_char, c_int, c_uchar, c_void};
use std::ptr;

use anyhow::{Context, Result, bail, ensure};
use libloading::os::windows::Library;
use serde_json::{Value, json};

use super::dll::DllDirectory;
use crate::au::{AU_HEADER_BYTES, AuSession, CODEC_MJPEG, build_au};

const CANDIDATES: &[&str] = &["turbojpeg.dll", "libturbojpeg.dll"];
/// 固定依赖版本（`docs/runbooks/mjpeg.md`）；TurboJPEG 3 没有运行时版本查询。
const VERSION: &str = "3.1.2";

const TJINIT_COMPRESS: c_int = 0;
const TJINIT_DECOMPRESS: c_int = 1;

const TJSAMP_420: c_int = 2;
const TJCS_YCBCR: c_int = 1;

const TJPARAM_STOPONWARNING: c_int = 0;
const TJPARAM_NOREALLOC: c_int = 2;
const TJPARAM_QUALITY: c_int = 3;
const TJPARAM_SUBSAMP: c_int = 4;
const TJPARAM_JPEGWIDTH: c_int = 5;
const TJPARAM_JPEGHEIGHT: c_int = 6;
const TJPARAM_PRECISION: c_int = 7;
const TJPARAM_COLORSPACE: c_int = 8;
const TJPARAM_FASTDCT: c_int = 10;
const TJPARAM_PROGRESSIVE: c_int = 12;
const TJPARAM_SCANLIMIT: c_int = 13;
const TJPARAM_ARITHMETIC: c_int = 14;
const TJPARAM_MAXMEMORY: c_int = 23;
const TJPARAM_MAXPIXELS: c_int = 24;

type TjHandle = *mut c_void;

type TjInit = unsafe extern "C" fn(c_int) -> TjHandle;
type TjDestroy = unsafe extern "C" fn(TjHandle);
type TjSet = unsafe extern "C" fn(TjHandle, c_int, c_int) -> c_int;
type TjGet = unsafe extern "C" fn(TjHandle, c_int) -> c_int;
type TjGetErrorStr = unsafe extern "C" fn(TjHandle) -> *mut c_char;
type TjJpegBufSize = unsafe extern "C" fn(c_int, c_int, c_int) -> usize;
type TjCompressFromYuvPlanes8 = unsafe extern "C" fn(
    TjHandle,
    *const *const c_uchar,
    c_int,
    *const c_int,
    c_int,
    *mut *mut c_uchar,
    *mut usize,
) -> c_int;
type TjDecompressHeader = unsafe extern "C" fn(TjHandle, *const c_uchar, usize) -> c_int;
type TjDecompressToYuvPlanes8 =
    unsafe extern "C" fn(TjHandle, *const c_uchar, usize, *mut *mut c_uchar, *mut c_int) -> c_int;

struct TjApi {
    init: TjInit,
    destroy: TjDestroy,
    set: TjSet,
    get: TjGet,
    error_str: TjGetErrorStr,
    jpeg_buf_size: TjJpegBufSize,
    compress_from_yuv_planes8: TjCompressFromYuvPlanes8,
    decompress_header: TjDecompressHeader,
    decompress_to_yuv_planes8: TjDecompressToYuvPlanes8,
}

impl TjApi {
    unsafe fn load(library: &Library) -> Result<Self> {
        unsafe fn symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T> {
            // SAFETY: 调用方保证 DLL 生命周期覆盖所有函数指针使用。
            let symbol = unsafe { library.get::<T>(name) }.with_context(|| {
                format!(
                    "TurboJPEG DLL 缺少符号 {}",
                    String::from_utf8_lossy(&name[..name.len().saturating_sub(1)])
                )
            })?;
            Ok(*symbol)
        }
        Ok(Self {
            init: unsafe { symbol(library, b"tj3Init\0") }?,
            destroy: unsafe { symbol(library, b"tj3Destroy\0") }?,
            set: unsafe { symbol(library, b"tj3Set\0") }?,
            get: unsafe { symbol(library, b"tj3Get\0") }?,
            error_str: unsafe { symbol(library, b"tj3GetErrorStr\0") }?,
            jpeg_buf_size: unsafe { symbol(library, b"tj3JPEGBufSize\0") }?,
            compress_from_yuv_planes8: unsafe { symbol(library, b"tj3CompressFromYUVPlanes8\0") }?,
            decompress_header: unsafe { symbol(library, b"tj3DecompressHeader\0") }?,
            decompress_to_yuv_planes8: unsafe { symbol(library, b"tj3DecompressToYUVPlanes8\0") }?,
        })
    }
}

struct TjSettings {
    role: &'static str,
    width: u32,
    height: u32,
    fps: u32,
    quality: u32,
}

fn settings_json(settings: &TjSettings) -> Value {
    json!({
        "family": "turbojpeg",
        "identity": crate::config::MJPEG_IDENTITY,
        "version": VERSION,
        "role": settings.role,
        "quality": settings.quality,
        "width": settings.width,
        "height": settings.height,
        "fps": settings.fps,
        "subsampling": "420",
        "dct": "fast_integer",
        "rate_control": "constant_quality",
        "target_bitrate_applied": false,
        "encoder_in_flight": 1,
        "config_submitted": true,
    })
}

pub(super) struct MjpegEncoder {
    _library: Library,
    api: TjApi,
    handle: TjHandle,
    width: u32,
    height: u32,
    fps: u32,
    quality: u32,
    max_au_bytes: usize,
    chroma: Vec<u8>,
    output: Vec<u8>,
}

impl MjpegEncoder {
    pub(super) fn open(
        directory: &DllDirectory,
        quality: u32,
        max_au_bytes: usize,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self> {
        let library = directory.load(CANDIDATES, "TurboJPEG 运行库")?;
        // SAFETY: DLL 由 `library` 持有；句柄在 Drop 中销毁。
        let api = unsafe { TjApi::load(&library)? };
        let handle = unsafe { (api.init)(TJINIT_COMPRESS) };
        ensure!(!handle.is_null(), "TurboJPEG 压缩初始化失败");
        let y_bytes = width as usize * height as usize;
        let mut encoder = Self {
            _library: library,
            api,
            handle,
            width,
            height,
            fps,
            quality,
            max_au_bytes,
            chroma: vec![0; y_bytes / 2],
            output: Vec::new(),
        };
        encoder.configure()?;
        Ok(encoder)
    }

    fn configure(&mut self) -> Result<()> {
        let maximum = unsafe {
            (self.api.jpeg_buf_size)(self.width as c_int, self.height as c_int, TJSAMP_420)
        };
        ensure!(maximum > 0, "TurboJPEG 无法计算 JPEG 输出缓冲大小");
        let capacity = maximum.min(self.max_au_bytes - AU_HEADER_BYTES);
        ensure!(
            capacity > 0,
            "max_au_bytes={} 不足以容纳 48B 封装与 JPEG 输出",
            self.max_au_bytes
        );
        self.set(TJPARAM_STOPONWARNING, 1, "strict warnings")?;
        self.set(TJPARAM_FASTDCT, 1, "fast integer DCT")?;
        self.set(TJPARAM_QUALITY, self.quality as c_int, "quality")?;
        self.set(TJPARAM_SUBSAMP, TJSAMP_420, "4:2:0 subsampling")?;
        self.set(TJPARAM_NOREALLOC, 1, "bounded output")?;
        self.output = vec![0u8; capacity];
        Ok(())
    }

    fn set(&self, parameter: c_int, value: c_int, what: &str) -> Result<()> {
        let result = unsafe { (self.api.set)(self.handle, parameter, value) };
        if result != 0 {
            bail!("JPEG {what}: {}", self.error_text());
        }
        Ok(())
    }

    fn error_text(&self) -> String {
        let pointer = unsafe { (self.api.error_str)(self.handle) };
        if pointer.is_null() {
            "TurboJPEG 未提供错误信息".into()
        } else {
            // SAFETY: TurboJPEG 返回以 NUL 结尾的静态/句柄内字符串。
            unsafe { CStr::from_ptr(pointer) }
                .to_string_lossy()
                .into_owned()
        }
    }

    pub(super) fn encode(&mut self, nv12: &[u8], frame_id: u32) -> Result<Vec<u8>> {
        let y_bytes = self.width as usize * self.height as usize;
        ensure!(
            nv12.len() == y_bytes + y_bytes / 2,
            "MJPEG 需要完整 NV12：{}x{} 需 {} 字节，实际 {}",
            self.width,
            self.height,
            y_bytes + y_bytes / 2,
            nv12.len()
        );
        let uv = y_bytes / 4;
        for index in 0..uv {
            self.chroma[index] = nv12[y_bytes + 2 * index];
            self.chroma[uv + index] = nv12[y_bytes + 2 * index + 1];
        }
        let planes = [nv12.as_ptr(), self.chroma.as_ptr(), unsafe {
            self.chroma.as_ptr().add(uv)
        }];
        let strides = [
            self.width as c_int,
            (self.width / 2) as c_int,
            (self.width / 2) as c_int,
        ];
        let mut output = self.output.as_mut_ptr();
        let mut output_size = self.output.len();
        let result = unsafe {
            (self.api.compress_from_yuv_planes8)(
                self.handle,
                planes.as_ptr(),
                self.width as c_int,
                strides.as_ptr(),
                self.height as c_int,
                &mut output,
                &mut output_size,
            )
        };
        if result != 0 {
            bail!(
                "JPEG 编码失败（输出上限 {} 字节）: {}",
                self.output.len(),
                self.error_text()
            );
        }
        ensure!(
            output == self.output.as_mut_ptr(),
            "TurboJPEG 违反 NOREALLOC，重分配了固定输出缓冲"
        );
        ensure!(
            output_size <= self.output.len(),
            "TurboJPEG 输出 {} 字节超过固定缓冲 {} 字节",
            output_size,
            self.output.len()
        );
        let payload = &self.output[..output_size];
        let total = AU_HEADER_BYTES + payload.len();
        ensure!(
            total <= self.max_au_bytes,
            "MJPEG 访问单元 {total} 字节超过 max_au_bytes={}",
            self.max_au_bytes
        );
        build_au(
            AuSession {
                codec: CODEC_MJPEG,
                width: self.width,
                height: self.height,
                fps: self.fps,
            },
            frame_id,
            payload,
        )
    }

    pub(super) fn settings(&self) -> Value {
        settings_json(&TjSettings {
            role: "encoder",
            width: self.width,
            height: self.height,
            fps: self.fps,
            quality: self.quality,
        })
    }
}

impl Drop for MjpegEncoder {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: 句柄由本结构独占。
            unsafe { (self.api.destroy)(self.handle) };
            self.handle = ptr::null_mut();
        }
    }
}

pub(super) struct MjpegDecoder {
    _library: Library,
    api: TjApi,
    handle: TjHandle,
    width: u32,
    height: u32,
    fps: u32,
    quality: u32,
    chroma: Vec<u8>,
}

impl MjpegDecoder {
    pub(super) fn open(
        directory: &DllDirectory,
        quality: u32,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self> {
        let library = directory.load(CANDIDATES, "TurboJPEG 运行库")?;
        // SAFETY: DLL 由 `library` 持有；句柄在 Drop 中销毁。
        let api = unsafe { TjApi::load(&library)? };
        let handle = unsafe { (api.init)(TJINIT_DECOMPRESS) };
        ensure!(!handle.is_null(), "TurboJPEG 解压初始化失败");
        let y_bytes = width as usize * height as usize;
        let mut decoder = Self {
            _library: library,
            api,
            handle,
            width,
            height,
            fps,
            quality,
            chroma: vec![0; y_bytes / 2],
        };
        decoder.configure()?;
        Ok(decoder)
    }

    fn configure(&mut self) -> Result<()> {
        self.set(TJPARAM_STOPONWARNING, 1, "strict warnings")?;
        self.set(TJPARAM_FASTDCT, 1, "fast integer DCT")?;
        self.set(
            TJPARAM_MAXPIXELS,
            (self.width as i64 * self.height as i64) as c_int,
            "pixel limit",
        )?;
        self.set(TJPARAM_MAXMEMORY, 16, "intermediate memory limit (MiB)")?;
        self.set(TJPARAM_SCANLIMIT, 1, "scan limit")?;
        Ok(())
    }

    fn set(&self, parameter: c_int, value: c_int, what: &str) -> Result<()> {
        let result = unsafe { (self.api.set)(self.handle, parameter, value) };
        if result != 0 {
            bail!("JPEG {what}: {}", self.error_text());
        }
        Ok(())
    }

    fn get(&self, parameter: c_int) -> c_int {
        unsafe { (self.api.get)(self.handle, parameter) }
    }

    fn error_text(&self) -> String {
        let pointer = unsafe { (self.api.error_str)(self.handle) };
        if pointer.is_null() {
            "TurboJPEG 未提供错误信息".into()
        } else {
            // SAFETY: TurboJPEG 返回以 NUL 结尾的静态/句柄内字符串。
            unsafe { CStr::from_ptr(pointer) }
                .to_string_lossy()
                .into_owned()
        }
    }

    /// 解码已通过 VCAU 封装的 JPEG 载荷，返回完整 NV12（含亮度）。
    pub(super) fn decode(&mut self, jpeg: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            jpeg.len() >= 4
                && jpeg[0] == 0xff
                && jpeg[1] == 0xd8
                && jpeg[jpeg.len() - 2] == 0xff
                && jpeg[jpeg.len() - 1] == 0xd9,
            "JPEG 载荷必须是完整 SOI..EOI"
        );
        let result =
            unsafe { (self.api.decompress_header)(self.handle, jpeg.as_ptr(), jpeg.len()) };
        if result != 0 {
            bail!("JPEG 头解析失败: {}", self.error_text());
        }
        let width = self.get(TJPARAM_JPEGWIDTH);
        let height = self.get(TJPARAM_JPEGHEIGHT);
        let subsampling = self.get(TJPARAM_SUBSAMP);
        let colorspace = self.get(TJPARAM_COLORSPACE);
        let precision = self.get(TJPARAM_PRECISION);
        let progressive = self.get(TJPARAM_PROGRESSIVE);
        let arithmetic = self.get(TJPARAM_ARITHMETIC);
        ensure!(
            width == self.width as c_int
                && height == self.height as c_int
                && subsampling == TJSAMP_420
                && colorspace == TJCS_YCBCR
                && precision == 8
                && progressive == 0
                && arithmetic == 0,
            "JPEG 必须为 baseline 8-bit YCbCr420 且与会话 {}x{} 完全一致；\
             实际 {width}x{height} subsamp={subsampling} colorspace={colorspace} \
             precision={precision} progressive={progressive} arithmetic={arithmetic}。\
             不做转换回退",
            self.width,
            self.height
        );

        let y_bytes = self.width as usize * self.height as usize;
        let uv = y_bytes / 4;
        let mut out = vec![0u8; y_bytes + y_bytes / 2];
        let mut planes = [out.as_mut_ptr(), self.chroma.as_mut_ptr(), unsafe {
            self.chroma.as_mut_ptr().add(uv)
        }];
        let mut strides = [
            self.width as c_int,
            (self.width / 2) as c_int,
            (self.width / 2) as c_int,
        ];
        let result = unsafe {
            (self.api.decompress_to_yuv_planes8)(
                self.handle,
                jpeg.as_ptr(),
                jpeg.len(),
                planes.as_mut_ptr(),
                strides.as_mut_ptr(),
            )
        };
        if result != 0 {
            bail!("JPEG 解码失败: {}", self.error_text());
        }
        ensure!(
            strides
                == [
                    self.width as c_int,
                    (self.width / 2) as c_int,
                    (self.width / 2) as c_int
                ],
            "TurboJPEG 返回了意外的 YUV 平面 stride"
        );
        // Cb/Cr → NV12 UV 交错；亮度已由 TurboJPEG 直接写入 out 的 Y 平面。
        for index in 0..uv {
            out[y_bytes + 2 * index] = self.chroma[index];
            out[y_bytes + 2 * index + 1] = self.chroma[uv + index];
        }
        Ok(out)
    }

    /// 重建 TurboJPEG 解压句柄（每帧独立，不需要保留帧状态）。
    pub(super) fn reset(&mut self) -> Result<()> {
        if !self.handle.is_null() {
            // SAFETY: 句柄由本结构独占。
            unsafe { (self.api.destroy)(self.handle) };
            self.handle = ptr::null_mut();
        }
        let handle = unsafe { (self.api.init)(TJINIT_DECOMPRESS) };
        ensure!(!handle.is_null(), "TurboJPEG 解压重置失败");
        self.handle = handle;
        self.configure()
    }

    pub(super) fn settings(&self) -> Value {
        settings_json(&TjSettings {
            role: "decoder",
            width: self.width,
            height: self.height,
            fps: self.fps,
            quality: self.quality,
        })
    }
}

impl Drop for MjpegDecoder {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: 句柄由本结构独占。
            unsafe { (self.api.destroy)(self.handle) };
            self.handle = ptr::null_mut();
        }
    }
}
