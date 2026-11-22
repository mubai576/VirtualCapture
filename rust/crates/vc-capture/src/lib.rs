//! 被采集机无界面后台：REST 控制 API，装载并监管进程内 `vc_stream.dll` 数据面会话。

mod api;
mod desktop;
mod state;

pub use desktop::{DesktopAction, DesktopService, DesktopStatus};

use std::net::{IpAddr, SocketAddr};
use std::process::Command;
use std::sync::{Arc, Mutex};

use clap::Args;

use crate::state::Agent;

#[derive(Args, Clone, Debug)]
pub struct Options {
    /// 监听地址。
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
    /// 监听端口，主机省略端口时也使用 5143。
    #[arg(long, default_value_t = 5143)]
    port: u16,
    /// 打印当前配置后退出。
    #[arg(long)]
    print_config: bool,
    /// 只列网卡后退出（诊断用）。
    #[arg(long)]
    list_adapters: bool,
    /// 允许本机诊断使用回环/虚拟网卡；默认列表与自动选卡只接受物理网卡。
    #[arg(long)]
    allow_loopback_diagnostics: bool,
    /// 在统一桌面窗口内运行采集后台（内部入口）。
    #[arg(long, hide = true)]
    desktop: bool,
    /// 打印更新令牌文件路径与令牌后退出，用于主机一次性配对。
    #[arg(long)]
    print_update_token: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self::listening("0.0.0.0".into(), 5143)
    }
}

impl Options {
    pub fn listening(bind: String, port: u16) -> Self {
        Self {
            bind,
            port,
            print_config: false,
            list_adapters: false,
            allow_loopback_diagnostics: false,
            desktop: false,
            print_update_token: false,
        }
    }

    pub fn is_diagnostic(&self) -> bool {
        self.print_config || self.list_adapters || self.print_update_token
    }

    pub fn is_desktop(&self) -> bool {
        self.desktop
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.bind, self.port)
    }

    /// 固定 ROOT 引导到当前版本时保留采集参数，旧拆分版也接受这些选项。
    pub fn append_args(&self, command: &mut Command) {
        command
            .arg("--bind")
            .arg(&self.bind)
            .arg("--port")
            .arg(self.port.to_string());
        for (enabled, flag) in [
            (self.print_config, "--print-config"),
            (self.list_adapters, "--list-adapters"),
            (
                self.allow_loopback_diagnostics,
                "--allow-loopback-diagnostics",
            ),
            (self.desktop, "--desktop"),
            (self.print_update_token, "--print-update-token"),
        ] {
            if enabled {
                command.arg(flag);
            }
        }
    }
}

#[tokio::main]
pub async fn run(cli: Options, has_console: bool) -> anyhow::Result<()> {
    serve(cli, has_console, None).await
}

#[tokio::main]
async fn run_desktop(cli: Options, control: desktop::Control) -> anyhow::Result<()> {
    serve(cli, false, Some(control)).await
}

async fn serve(
    cli: Options,
    has_console: bool,
    desktop: Option<desktop::Control>,
) -> anyhow::Result<()> {
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
        let module = vc_core::dataplane::load().map_err(anyhow::Error::msg)?;
        let items = vc_core::adapters::enumerate(&module).map_err(anyhow::Error::msg)?;
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
            desktop: desktop.is_some(),
        },
        shutdown_tx.clone(),
    );

    let bind: IpAddr = cli.bind.parse()?;
    let addr = SocketAddr::new(bind, cli.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    if let Some(control) = desktop {
        control.ready(agent.clone(), shutdown_tx);
    }
    println!(
        "VirtualCapture capture {} listening on http://{} root={}",
        env!("CARGO_PKG_VERSION"),
        addr,
        vc_core::paths::root().display()
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(agent, shutdown_rx, has_console))
    .await?;
    Ok(())
}

#[cfg(windows)]
async fn console_shutdown(has_console: bool) {
    if !has_console {
        std::future::pending::<()>().await;
    }
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close};
    let mut interrupt = ctrl_c().expect("无法监听 Ctrl-C");
    let mut cancel = ctrl_break().expect("无法监听 Ctrl-Break");
    let mut close = ctrl_close().expect("无法监听关闭事件");
    tokio::select! {
        _ = interrupt.recv() => {},
        _ = cancel.recv() => {},
        _ = close.recv() => {},
    }
}

#[cfg(not(windows))]
async fn console_shutdown(_: bool) {
    let _ = tokio::signal::ctrl_c().await;
}

/// 前台控制台事件或 `/update/activate` 请求优雅退出；OTA 后台无需控制台。
async fn shutdown_signal(
    agent: Arc<Mutex<Agent>>,
    mut update_shutdown: tokio::sync::watch::Receiver<bool>,
    has_console: bool,
) {
    tokio::select! {
        _ = console_shutdown(has_console) => {},
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
