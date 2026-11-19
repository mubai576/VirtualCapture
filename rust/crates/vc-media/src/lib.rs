//! VirtualCapture 编解码模块（Rust 控制面）。
//!
//! 只支持当前项目批准的两种路径：
//! - **Raw NV12**：直通，不压缩（默认）。
//! - **PyroWave**：固定 `libpyrowave-shared-1.dll`，Vulkan 1.3 GPU 计算
//!   intra-only；阶段一 CPU 上传/回读，不是零拷贝或固定硬件编码器。
//!
//! MJPEG（codec id 3）曾使用 libjpeg-turbo CPU/SIMD，违反 GPU-only 要求，
//! 生产实现与 `turbojpeg.dll` 已移除；wire id 仍保留，仅用于安全拒绝旧流，
//! [`validate`]、`Encoder::open` 与 `Decoder::open` 都会给出明确错误。
//!
//! 编码输出与原生产品一致：非 Raw 返回 48 字节 VCAU 封装 + 编码载荷，
//! 由主传输按实际访问单元长度分片；Raw 返回 NV12 本体。解码严格校验封装、
//! 载荷哈希、会话尺寸/帧号及编码载荷。开发编码必须显式 `experimental=true`
//! 且两端身份精确匹配；不存在静默回退、自动换后端或降尺寸/降帧率。
//!
//! 编解码仍由上游库实现（PyroWave 及 Vulkan 驱动），本 crate 不链接、不包装
//! 任何项目 C++ 进程或 DLL。运行库必须放在模块/可执行文件同目录，使用绝对
//! 路径与安全搜索标志动态加载。
//!
//! 未包含：DShow/SHM 发布、Npcap 分片重组、采集与缩放、OTA/打包。
//! 本 crate 不声称双机、长稳、画质或实时吞吐验收通过。

pub mod au;

mod config;

#[cfg(windows)]
mod codec;

pub use au::{
    AU_HEADER_BYTES, AU_MAGIC, AU_REVISION, AccessUnit, AuSession, CODEC_MJPEG, CODEC_PYROWAVE,
    KIND_EOS, KIND_FRAME, build_au, build_au_kind, codec_id, codec_name, parse_au,
};
pub use config::{
    CodecConfig, MAX_AU_BYTES, MAX_BITRATE_BPS, MIN_AU_BYTES, MIN_BITRATE_BPS, MJPEG_UNSUPPORTED,
    PYROWAVE_IDENTITY, capabilities, pyrowave_frame_budget, validate,
};

#[cfg(windows)]
pub use codec::{Decoder, Encoder, OperationState, default_library_directory};
