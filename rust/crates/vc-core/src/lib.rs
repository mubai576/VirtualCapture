//! VirtualCapture 控制面核心库。
//!
//! 主机与被采集机共用：配置、链路预算、数据面会话槽位、网卡枚举、虚拟卡注册、对端 HTTP 客户端。
//! 数据面（L2/Npcap、采集、DShow）通过版本化 `vc_stream.dll` 模块在进程内运行。

pub mod adapters;
pub mod budget;
pub mod client;
pub mod config;
pub mod dataplane;
pub mod host;
pub mod paths;
pub mod process;
pub mod session;
pub mod update;
pub mod vcam;
pub mod version;

pub use config::Config;
