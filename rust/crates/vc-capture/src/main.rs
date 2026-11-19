//! 被采集机无界面后台：REST 控制 API，拉起并监管原生 `vc-sender.exe`。

mod api;
mod state;

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use clap::Parser;

use crate::state::Agent;

#[derive(Parser, Debug)]
#[command(
    name = "vc-capture",
    version,
    about = "VirtualCapture 被采集机后台（Rust 控制面）"
)]
struct Cli {
    /// 监听地址。
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
    /// 监听端口（产品固定 5143）。
    #[arg(long, default_value_t = 5143)]
    port: u16,
    /// 运行根目录，默认自动探测（含 version.json 的仓库/便携包根）。
    #[arg(long)]
    root: Option<std::path::PathBuf>,
    /// 原生 exe/dll 目录（开发时指向 C++ 构建输出）。
    #[arg(long)]
    native_dir: Option<std::path::PathBuf>,
    /// 打印当前配置后退出。
    #[arg(long)]
    print_config: bool,
    /// 只列网卡后退出（诊断用）。
    #[arg(long)]
    list_adapters: bool,
    /// 允许本机诊断使用回环/虚拟网卡；默认列表与自动选卡只接受物理网卡。
    #[arg(long)]
    allow_loopback_diagnostics: bool,
    /// 打印更新令牌文件路径与令牌后退出，用于主机一次性配对。
    #[arg(long)]
    print_update_token: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(root) = &cli.root {
        vc_core::paths::set_root_override(root.clone());
    }
    if let Some(dir) = &cli.native_dir {
        vc_core::paths::set_native_dir_override(dir.clone());
    }
    vc_core::paths::validate_current_at(&vc_core::paths::root()).map_err(anyhow::Error::msg)?;
    vc_core::process::ensure_hidden_console();

    if cli.print_config {
        println!(
            "{}",
            serde_json::to_string_pretty(&vc_core::Config::load_root()?)?
        );
        return Ok(());
    }
    if cli.print_update_token {
        let token = vc_core::update::token().map_err(anyhow::Error::msg)?;
        println!(
            "update token file: {}",
            vc_core::paths::update_token_path().display()
        );
        println!("{token}");
        return Ok(());
    }
    if cli.list_adapters {
        let exe = vc_core::paths::native_exe("vc-sender.exe");
        let items = vc_core::adapters::enumerate(&exe).map_err(anyhow::Error::msg)?;
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    // 让主机可以配对更新；令牌只写本地文件，不进入日志。
    if let Err(error) = vc_core::update::token() {
        eprintln!("update token unavailable: {error}");
    }
    let config = vc_core::Config::load_root()?;
    let agent = Arc::new(Mutex::new(
        Agent::new(config).with_loopback_diagnostics(cli.allow_loopback_diagnostics),
    ));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let app = api::router(
        agent.clone(),
        api::ServerInfo {
            bind: cli.bind.clone(),
            port: cli.port,
        },
        shutdown_tx,
    );

    let bind: IpAddr = cli.bind.parse()?;
    let addr = SocketAddr::new(bind, cli.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!(
        "vc-capture {} listening on http://{} root={}",
        env!("CARGO_PKG_VERSION"),
        addr,
        vc_core::paths::root().display()
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(agent.clone(), shutdown_rx))
    .await?;
    Ok(())
}

/// Ctrl-C / Ctrl-Break / 控制台关闭，或 `/update/activate` 请求优雅退出。
async fn shutdown_signal(
    agent: Arc<Mutex<Agent>>,
    mut update_shutdown: tokio::sync::watch::Receiver<bool>,
) {
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close};
        let mut interrupt = ctrl_c().expect("无法监听 Ctrl-C");
        let mut cancel = ctrl_break().expect("无法监听 Ctrl-Break");
        let mut close = ctrl_close().expect("无法监听关闭事件");
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = cancel.recv() => {},
            _ = close.recv() => {},
            _ = update_shutdown.wait_for(|value| *value) => {},
        }
    }
    #[cfg(not(windows))]
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = update_shutdown.wait_for(|value| *value) => {},
    }
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(mut agent) = agent.lock() {
            agent.shutting_down = true;
            let _ = agent.stop(None);
        }
    })
    .await;
}
