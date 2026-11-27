//! Windows 编解码控制器：Raw NV12 与 PyroWave（Vulkan GPU 计算）。
//!
//! 公开 API 只接受 [`CodecConfig`] 与会话几何；后端身份在 [`crate::validate`]
//! 中精确校验。编码输出为既有 48B VCAU 封装 + 编码载荷；Raw 直接返回 NV12。
//! 解码先验证封装/哈希/kind/codec/尺寸/帧号，再校验编码载荷本身。
//! PyroWave 在 `open` 返回前用同几何合成 NV12 完成一次真实编解码预热，
//! 强制创建 Vulkan 管线；预热帧/哈希不进入正式帧号与统计。
//! MJPEG（wire id 3）的 CPU/SIMD 后端已移除，打开时明确拒绝。
//! 没有任何自动回退：后端缺失、身份不符或解码失败都明确报错。

mod deadline;
mod device;
mod dll;
mod pyrowave;

pub use deadline::OperationState;

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use vc_transport::Geometry;

use crate::au::{AuSession, parse_au};
use crate::config::{CodecConfig, MJPEG_UNSUPPORTED, validate};

use dll::DllDirectory;

/// 预热访问单元只用于初始化；正式发送端仍从帧号 0 开始。
const WARMUP_FRAME_ID: u32 = 0;

/// 编解码 DLL 的默认目录：当前模块/可执行文件所在目录。
pub fn default_library_directory() -> Result<PathBuf> {
    Ok(DllDirectory::default_for_current_module()?
        .path()
        .to_path_buf())
}

fn library_directory(directory: Option<&Path>) -> Result<DllDirectory> {
    match directory {
        Some(directory) => DllDirectory::new(directory),
        None => DllDirectory::default_for_current_module(),
    }
}

/// 同几何、确定性、整数生成的 NV12 预热帧；只用于创建管线，不进入传输。
fn warmup_nv12(width: u32, height: u32) -> Vec<u8> {
    let width = width as usize;
    let height = height as usize;
    let y_bytes = width * height;
    let mut frame = Vec::with_capacity(y_bytes + y_bytes / 2);
    for y in 0..height {
        for x in 0..width {
            let tile = ((x / 32) + (y / 32)) & 3;
            frame.push(32 + (tile as u8) * 48);
        }
    }
    for y in 0..height / 2 {
        for x in 0..width / 2 {
            let tile = ((x / 16) + (y / 16)) & 3;
            frame.push(48 + (tile as u8) * 32);
            frame.push(160 - (tile as u8) * 24);
        }
    }
    frame
}

/// 用正式编码器实例真实编码一帧同几何合成 NV12，强制创建 Vulkan 管线。
fn warmup_pyrowave_encoder(
    codec: &mut pyrowave::PyroWaveEncoder,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<()> {
    let source = warmup_nv12(width, height);
    let started = Instant::now();
    let _ = codec
        .warmup(&source, WARMUP_FRAME_ID)
        .context("PyroWave 编码器预热失败")?;
    eprintln!(
        "[vc-media] PyroWave 编码器预热完成 {}x{}@{} {} ms",
        width,
        height,
        fps,
        started.elapsed().as_millis()
    );
    Ok(())
}

/// 解码器初始化时还没有正式 AU：用同一显式后端、同几何与同一动态帧预算真实
/// 编码一帧作为自环输入，再让新解码器同步解码一次。两端输出都丢弃，只保留管线。
fn warmup_pyrowave_decoder(
    decoder: &mut pyrowave::PyroWaveDecoder,
    config: &CodecConfig,
    width: u32,
    height: u32,
    fps: u32,
    directory: &DllDirectory,
    state: &OperationState,
) -> Result<()> {
    let started = Instant::now();
    let source = warmup_nv12(width, height);
    let mut encoder =
        pyrowave::PyroWaveEncoder::open(directory, config, width, height, fps, state)?;
    let encoded = encoder.warmup(&source, WARMUP_FRAME_ID)?;
    drop(encoder);
    let session = AuSession::pyrowave(width, height, fps);
    let unit = parse_au(&encoded, session, WARMUP_FRAME_ID, config.max_au_bytes)?;
    let decoded = decoder.warmup(unit.payload)?;
    ensure!(
        decoded.len() == source.len(),
        "PyroWave 解码器预热输出 {} 字节，同几何 NV12 需要 {} 字节",
        decoded.len(),
        source.len()
    );
    ensure!(
        decoded[..width as usize * height as usize]
            .iter()
            .any(|&byte| byte != 0),
        "PyroWave 解码器预热没有写回亮度"
    );
    eprintln!(
        "[vc-media] PyroWave 解码器预热完成 {}x{}@{} {} ms",
        width,
        height,
        fps,
        started.elapsed().as_millis()
    );
    Ok(())
}

enum EncoderKind {
    Raw,
    PyroWave(Box<pyrowave::PyroWaveEncoder>),
}

/// 单帧编码器；同一时间只允许一帧在途（由调用方串行使用）。
pub struct Encoder {
    config: CodecConfig,
    width: u32,
    height: u32,
    fps: u32,
    frame_bytes: usize,
    operation_state: OperationState,
    kind: EncoderKind,
}

impl Encoder {
    /// 从当前模块目录加载编解码 DLL（Raw 不需要）。
    pub fn open(config: &CodecConfig, width: u32, height: u32, fps: u32) -> Result<Self> {
        Self::build(config, width, height, fps, None, &OperationState::new())
    }

    /// 同 [`Encoder::open`]，并在创建原生后端之前交出会话观察句柄。
    ///
    /// PyroWave 的创建/销毁同样受 5 秒看门狗覆盖；若创建期间超时，`open` 会返回
    /// 粘性超时错误，调用方可用该句柄在其他线程观察超时，且不得再销毁原生句柄。
    pub fn open_observed(
        config: &CodecConfig,
        width: u32,
        height: u32,
        fps: u32,
        state: &OperationState,
    ) -> Result<Self> {
        Self::build(config, width, height, fps, None, state)
    }

    /// 显式指定绝对依赖目录（宿主/测试用）；仍只接受 [`CodecConfig`] 中的显式身份。
    pub fn open_in_directory(
        config: &CodecConfig,
        width: u32,
        height: u32,
        fps: u32,
        directory: &Path,
    ) -> Result<Self> {
        Self::build(
            config,
            width,
            height,
            fps,
            Some(directory),
            &OperationState::new(),
        )
    }

    /// 同 [`Encoder::open_in_directory`]，并在创建原生后端之前交出会话观察句柄。
    pub fn open_in_directory_observed(
        config: &CodecConfig,
        width: u32,
        height: u32,
        fps: u32,
        directory: &Path,
        state: &OperationState,
    ) -> Result<Self> {
        Self::build(config, width, height, fps, Some(directory), state)
    }

    fn build(
        config: &CodecConfig,
        width: u32,
        height: u32,
        fps: u32,
        directory: Option<&Path>,
        state: &OperationState,
    ) -> Result<Self> {
        validate(config, width, height, fps)?;
        let geometry = Geometry::new(width, height)?;
        let kind = match config.codec.as_str() {
            "raw" => EncoderKind::Raw,
            "mjpeg" => anyhow::bail!("{MJPEG_UNSUPPORTED}"),
            "pyrowave" => {
                let directory = library_directory(directory)?;
                EncoderKind::PyroWave(Box::new(pyrowave::PyroWaveEncoder::open(
                    &directory, config, width, height, fps, state,
                )?))
            }
            other => anyhow::bail!("codec={other} 不受支持"),
        };
        let mut encoder = Self {
            config: config.clone(),
            width,
            height,
            fps,
            frame_bytes: geometry.frame_bytes(),
            operation_state: state.clone(),
            kind,
        };
        if let EncoderKind::PyroWave(codec) = &mut encoder.kind {
            warmup_pyrowave_encoder(codec, width, height, fps)?;
        }
        Ok(encoder)
    }

    /// 该会话的同步操作状态；PyroWave 超时只置位此句柄，不影响其他会话或进程。
    pub fn operation_state(&self) -> OperationState {
        self.operation_state.clone()
    }

    /// Raw 返回输入 NV12 切片；编码模式返回预分配缓冲中的完整 48B VCAU + 编码载荷切片（零分配）。
    pub fn encode_ref<'a>(&'a mut self, nv12: &'a [u8], frame_id: u32) -> Result<&'a [u8]> {
        ensure!(
            nv12.len() == self.frame_bytes,
            "NV12 输入 {} 字节，与会话 {}x{} 需要 {} 字节不符",
            nv12.len(),
            self.width,
            self.height,
            self.frame_bytes
        );
        match &mut self.kind {
            EncoderKind::Raw => Ok(nv12),
            EncoderKind::PyroWave(codec) => codec.encode_ref(nv12, frame_id),
        }
    }

    /// Raw 返回输入 NV12；编码模式返回完整 48B VCAU + 编码载荷。
    pub fn encode(&mut self, nv12: &[u8], frame_id: u32) -> Result<Vec<u8>> {
        self.encode_ref(nv12, frame_id).map(|slice| slice.to_vec())
    }

    pub fn settings(&self) -> Value {
        match &self.kind {
            EncoderKind::Raw => json!({
                "family": "raw",
                "identity": "raw",
                "codec": self.config.codec,
                "width": self.width,
                "height": self.height,
                "fps": self.fps,
                "pixel_format": "nv12",
                "frame_bytes": self.frame_bytes,
                "compression": false,
                "experimental": false,
            }),
            EncoderKind::PyroWave(codec) => codec.settings(),
        }
    }
}

enum DecoderKind {
    Raw,
    PyroWave(Box<pyrowave::PyroWaveDecoder>),
}

/// 单帧解码器；每帧独立，断档时调用 [`Decoder::reset`]。
pub struct Decoder {
    config: CodecConfig,
    width: u32,
    height: u32,
    fps: u32,
    frame_bytes: usize,
    operation_state: OperationState,
    kind: DecoderKind,
}

impl Decoder {
    /// 从当前模块目录加载编解码 DLL（Raw 不需要）。
    pub fn open(config: &CodecConfig, width: u32, height: u32, fps: u32) -> Result<Self> {
        Self::build(config, width, height, fps, None, &OperationState::new())
    }

    /// 同 [`Decoder::open`]，并在创建原生后端之前交出会话观察句柄。
    ///
    /// PyroWave 的创建/重置/销毁同样受 5 秒看门狗覆盖；创建期间超时以粘性错误返回。
    pub fn open_observed(
        config: &CodecConfig,
        width: u32,
        height: u32,
        fps: u32,
        state: &OperationState,
    ) -> Result<Self> {
        Self::build(config, width, height, fps, None, state)
    }

    /// 显式指定绝对依赖目录（宿主/测试用）。
    pub fn open_in_directory(
        config: &CodecConfig,
        width: u32,
        height: u32,
        fps: u32,
        directory: &Path,
    ) -> Result<Self> {
        Self::build(
            config,
            width,
            height,
            fps,
            Some(directory),
            &OperationState::new(),
        )
    }

    /// 同 [`Decoder::open_in_directory`]，并在创建原生后端之前交出会话观察句柄。
    pub fn open_in_directory_observed(
        config: &CodecConfig,
        width: u32,
        height: u32,
        fps: u32,
        directory: &Path,
        state: &OperationState,
    ) -> Result<Self> {
        Self::build(config, width, height, fps, Some(directory), state)
    }

    fn build(
        config: &CodecConfig,
        width: u32,
        height: u32,
        fps: u32,
        directory: Option<&Path>,
        state: &OperationState,
    ) -> Result<Self> {
        validate(config, width, height, fps)?;
        let geometry = Geometry::new(width, height)?;
        let kind = match config.codec.as_str() {
            "raw" => DecoderKind::Raw,
            "mjpeg" => anyhow::bail!("{MJPEG_UNSUPPORTED}"),
            "pyrowave" => {
                let directory = library_directory(directory)?;
                DecoderKind::PyroWave(Box::new(pyrowave::PyroWaveDecoder::open(
                    &directory, width, height, fps, state,
                )?))
            }
            other => anyhow::bail!("codec={other} 不受支持"),
        };
        let mut decoder = Self {
            config: config.clone(),
            width,
            height,
            fps,
            frame_bytes: geometry.frame_bytes(),
            operation_state: state.clone(),
            kind,
        };
        if let DecoderKind::PyroWave(codec) = &mut decoder.kind {
            let directory = library_directory(directory)?;
            warmup_pyrowave_decoder(codec, config, width, height, fps, &directory, state)
                .context("PyroWave 解码器预热失败")?;
        }
        Ok(decoder)
    }

    /// 该会话的同步操作状态；PyroWave 超时只置位此句柄，不影响其他会话或进程。
    pub fn operation_state(&self) -> OperationState {
        self.operation_state.clone()
    }

    /// 校验 VCAU 封装后解码；EOS 返回空 Vec。Raw 直接校验帧字节数并返回 NV12。
    pub fn decode(&mut self, au: &[u8], frame_id: u32) -> Result<Vec<u8>> {
        match &mut self.kind {
            DecoderKind::Raw => {
                ensure!(
                    au.len() == self.frame_bytes,
                    "Raw NV12 输入 {} 字节，与会话 {}x{} 需要 {} 字节不符",
                    au.len(),
                    self.width,
                    self.height,
                    self.frame_bytes
                );
                Ok(au.to_vec())
            }
            DecoderKind::PyroWave(codec) => {
                let session = AuSession::pyrowave(self.width, self.height, self.fps);
                let unit = parse_au(au, session, frame_id, self.config.max_au_bytes)?;
                if unit.is_eos() {
                    return Ok(Vec::new());
                }
                let mut out = vec![0u8; self.frame_bytes];
                codec.decode_into(unit.payload, &mut out)?;
                Ok(out)
            }
        }
    }

    /// 对调用方已通过 `parse_au` 校验的非 EOS 编码载荷直接解码进复用的 NV12 缓冲区，
    /// 避免重复 VCAU 校验与重复 FNV-1a64 计算。
    pub fn decode_verified_payload_into(&mut self, payload: &[u8], out: &mut [u8]) -> Result<()> {
        ensure!(
            out.len() == self.frame_bytes,
            "NV12 输出 {} 字节，与会话 {}x{} 需要 {} 字节不符",
            out.len(),
            self.width,
            self.height,
            self.frame_bytes
        );
        match &mut self.kind {
            DecoderKind::Raw => {
                ensure!(
                    payload.len() == self.frame_bytes,
                    "Raw NV12 输入 {} 字节，与会话 {}x{} 需要 {} 字节不符",
                    payload.len(),
                    self.width,
                    self.height,
                    self.frame_bytes
                );
                out.copy_from_slice(payload);
                Ok(())
            }
            DecoderKind::PyroWave(codec) => codec.decode_into(payload, out),
        }
    }

    /// 丢弃解码器帧状态；PyroWave 保留已创建的 Vulkan 设备，只重建解码器。
    pub fn reset(&mut self) -> Result<()> {
        match &mut self.kind {
            DecoderKind::Raw => Ok(()),
            DecoderKind::PyroWave(codec) => codec.reset(),
        }
    }

    pub fn settings(&self) -> Value {
        match &self.kind {
            DecoderKind::Raw => json!({
                "family": "raw",
                "identity": "raw",
                "codec": self.config.codec,
                "width": self.width,
                "height": self.height,
                "fps": self.fps,
                "pixel_format": "nv12",
                "frame_bytes": self.frame_bytes,
                "compression": false,
                "experimental": false,
            }),
            DecoderKind::PyroWave(codec) => codec.settings(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmup_nv12_is_exact_deterministic_and_nonflat() {
        for (width, height) in [(2u32, 2u32), (64, 64), (640, 640)] {
            let frame = warmup_nv12(width, height);
            let y_bytes = width as usize * height as usize;
            assert_eq!(frame.len(), y_bytes + y_bytes / 2);
            assert_eq!(frame, warmup_nv12(width, height));
            assert!(frame.iter().all(|&byte| byte != 0));
        }
        let frame = warmup_nv12(640, 640);
        let y_plane = &frame[..640 * 640];
        assert!(
            y_plane.windows(2).any(|pair| pair[0] != pair[1]),
            "预热帧必须有实际内容"
        );
    }
}
