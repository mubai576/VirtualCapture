//! VirtualCapture 产品共享的 Rust wire、校验、分片与重组。
//!
//! 本 crate 实现 `proto/vc_wire.h` 冻结的 28 字节包头、包载荷 CRC32、
//! 整帧 FNV-1a64，以及 Raw NV12 的分片和有界乱序重组。
//! 它不读取配置、不开网卡、不接触 DShow，不改变冻结协议。
//!
//! 接入方式：
//! - Rust sender 用 [`fragment::fragments`] 生成包头/载荷，用
//!   [`fragment::write_ethernet`] 写入发帧缓冲，再由 Npcap 发送。
//! - Rust receiver 把 Npcap 回调里的完整以太帧交给
//!   [`reassembly::Reassembler::push_ethernet`]；它返回完成帧，交由发布/哈希线程处理。
//! - 会话宽高、MTU、源 MAC 来自控制面，包头本身**不携带**这些信息（proto 冻结）。
//!
//! 本 crate 还带一个独立诊断 bin `vc-transport`（源码 `src/bin/vc-transport.rs`）：
//! 用 [`pcap`] 模块离线读取经典 pcap，`inspect` 统计抓包结构，`replay` 按会话几何
//! 重组并输出完成帧哈希。它不开网卡、不依赖 Npcap，也不参与部署。
//!
//! 未包含：DDA/WGC/GPU 采集、SHM/DShow 发布、pattern 自检、Npcap FFI。
//! 这些仍由现有 C++ 实现负责；本 crate 不声称吞吐或延迟结论。
//!
//! 帧号比较使用 RFC 1982 序号算术（`a.wrapping_sub(b) < 2^31`），因此跨 2^32 回绕
//! 不会把新帧误判成旧帧；非回绕场景与 C++ 的数值比较完全一致。详见
//! `docs/proposals/rust-data-plane-20261114.md`。

pub mod checksum;
pub mod encoded;
pub mod fragment;
pub mod pcap;
pub mod reassembly;
pub mod wire;

pub use fragment::{Fragment, FragmentIter, fragments, write_ethernet};
pub use reassembly::{
    CompletedFrame, Config, IgnoreReason, LatestFrame, PushOutcome, Reassembler, Stats,
    TimingSummary,
};
pub use wire::{
    ETHERNET_HEADER_LEN, ETHERTYPE, Geometry, HEADER_LEN, Header, L2_OVERHEAD, MAGIC,
    MAX_DIMENSION, MAX_MTU, WireError, is_after, packet_count, payload_len_at, payload_max_for_mtu,
};
