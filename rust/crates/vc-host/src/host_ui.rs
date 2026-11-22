//! 主机采集工作台：GPUI 展示层，所有会话与进程编排由 vc-core 管理。

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable, Icon, Selectable, Theme, ThemeMode};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Context, Div, Entity, Font, FontFallbacks, FontWeight, PathPromptOptions, Render,
    SharedString, Window, div, px, rgb,
};
use serde_json::{Value, json};

use vc_core::adapters::AdapterInfo;
use vc_core::budget::{self, Plan, PlanStatus};
use vc_core::config::Config;
use vc_core::host::{self, Command, HostController, HostScan, Snapshot};
use vc_core::vcam::VcamStatus;

pub(crate) const BG: u32 = 0xf2f4f7;
pub(crate) const SURFACE: u32 = 0xffffff;
pub(crate) const FIELD: u32 = 0xf5f7fa;
pub(crate) const BORDER: u32 = 0xe1e6ed;
pub(crate) const TEXT: u32 = 0x17263d;
pub(crate) const TEXT_DIM: u32 = 0x5d6c80;
const TEXT_FAINT: u32 = 0x5d6c80;
pub(crate) const ACCENT: u32 = 0x315cec;
const ACCENT_SOFT: u32 = 0xeef2ff;
pub(crate) const OK: u32 = 0x237650;
const WARN: u32 = 0x93600c;
pub(crate) const ERR: u32 = 0xb63c40;
const STAGE: u32 = 0x17263d;
const STAGE_LINE: u32 = 0x3f516c;
const STAGE_MUTED: u32 = 0xb4c1d4;

#[derive(Clone)]
struct UpdateBuild {
    build_id: String,
    version: String,
}

#[derive(Default)]
struct UpdateSummary {
    active: Option<UpdateBuild>,
    staged: Option<UpdateBuild>,
    /// 历史里最近一次真正激活过、且不是当前版本的发布，可作为回滚目标。
    rollback: Option<UpdateBuild>,
    /// 状态读取失败（含 401/404/超时）；不允许在 UI 里静默隐藏。
    error: Option<String>,
}

impl UpdateSummary {
    fn parse(value: &Value) -> Self {
        fn build(value: &Value) -> Option<UpdateBuild> {
            Some(UpdateBuild {
                build_id: value.get("build_id").and_then(Value::as_str)?.to_owned(),
                version: value
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            })
        }
        let active = build(&value["active"]);
        let rollback = value["history"].as_array().and_then(|history| {
            history.iter().rev().find_map(|entry| {
                if entry.get("state").and_then(Value::as_str) != Some("active") {
                    return None;
                }
                build(entry).filter(|item| {
                    active
                        .as_ref()
                        .is_none_or(|active| active.build_id != item.build_id)
                })
            })
        });
        Self {
            active,
            staged: build(&value["staged"]),
            rollback,
            error: value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
    }
}

pub(crate) struct HostApp {
    controller: HostController,
    snapshot: Snapshot,
    peer_input: Entity<InputState>,
    update_token: Entity<InputState>,
    /// 显式 MTU 输入框；点击 1500/9000 芯片时同步写入该值。
    mtu_input: Entity<InputState>,
    /// 输入框本地校验错误；只影响展示，不写入配置。
    mtu_error: Option<String>,
    startup_notice: Option<String>,
    show_details: bool,
    show_updates: bool,
    /// 已配对时展开令牌输入，用于更换配对令牌。
    pair_edit: bool,
    ready_path: Option<PathBuf>,
    ready_written: bool,
    restart_quit: bool,
}

pub(crate) fn init_theme(cx: &mut App) {
    Theme::change(ThemeMode::Light, None, cx);
    Theme::update(cx, |theme| {
        theme.font_family = "Microsoft YaHei UI".into();
        theme.font_size = px(14.);
        theme.radius = px(8.);
        theme.radius_lg = px(12.);
        theme.colors.primary = rgb(ACCENT).into();
        theme.colors.primary_hover = rgb(0x254bd2).into();
        theme.colors.primary_active = rgb(0x2041ba).into();
        theme.colors.primary_foreground = rgb(0xffffff).into();
        theme.colors.button_primary = rgb(ACCENT).into();
        theme.colors.button_primary_hover = rgb(0x254bd2).into();
        theme.colors.button_primary_active = rgb(0x2041ba).into();
        theme.colors.button_primary_foreground = rgb(0xffffff).into();
        theme.colors.background = rgb(SURFACE).into();
        theme.colors.foreground = rgb(TEXT).into();
        theme.colors.border = rgb(BORDER).into();
        theme.colors.input = rgb(BORDER).into();
        theme.colors.ring = rgb(ACCENT).into();
    });
}

impl HostApp {
    pub(crate) fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        ready_path: Option<PathBuf>,
    ) -> Self {
        let controller = host::spawn();
        let (initial_peer, initial_mtu, startup_notice) = match Config::load_root() {
            Ok(config) => (
                config
                    .peer_url
                    .strip_prefix("http://")
                    .unwrap_or(&config.peer_url)
                    .to_owned(),
                config.capture.mtu.to_string(),
                None,
            ),
            Err(error) => (
                String::new(),
                "9000".to_owned(),
                Some(format!("配置读取失败：{error}；修复文件后重新打开。")),
            ),
        };
        let peer_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("192.168.2.59:5143")
                .default_value(initial_peer)
        });
        let mtu_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("1500..9000")
                .default_value(initial_mtu)
        });
        let update_token = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("配对令牌（64 位十六进制）")
                .masked(true)
        });
        let snapshot = controller
            .shared
            .snapshot
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default();
        let app = Self {
            controller,
            snapshot,
            peer_input,
            update_token,
            mtu_input,
            mtu_error: None,
            startup_notice,
            show_details: false,
            show_updates: false,
            pair_edit: false,
            ready_path,
            ready_written: false,
            restart_quit: false,
        };
        app.start_ticker(cx);
        app
    }

    fn start_ticker(&self, cx: &mut Context<Self>) {
        let shared = self.controller.shared.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(350))
                    .await;
                let snapshot = shared.snapshot.lock().ok().map(|state| state.clone());
                if this
                    .update(cx, |view, cx| {
                        if let Some(snapshot) = snapshot {
                            view.snapshot = snapshot;
                        }
                        if !view.ready_written {
                            view.ready_written = true;
                            if let Some(path) = view.ready_path.clone()
                                && let Err(error) = vc_core::update::write_ready(&path)
                            {
                                view.startup_notice =
                                    Some(format!("更新就绪标记写入失败：{error}"));
                            }
                        }
                        if view.snapshot.restart_requested && !view.restart_quit {
                            view.restart_quit = true;
                            cx.quit();
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn role_selection_ready(&self) -> bool {
        self.snapshot.role_selection_ready
    }

    fn send(&self, command: Command) {
        let _ = self.controller.tx.send(command);
    }
    fn locked(&self) -> bool {
        self.snapshot.running || self.snapshot.busy || self.snapshot.cleanup_pending
    }
    fn connection(&self) -> (&'static str, u32) {
        if !self.snapshot.connected {
            ("未连接", TEXT_DIM)
        } else if self.snapshot.peer_online {
            ("已连接", OK)
        } else {
            ("连接中断", ERR)
        }
    }
    fn can_start(&self) -> bool {
        self.snapshot.connected
            && self.snapshot.peer_online
            && !self.locked()
            && self.snapshot.host_scan == HostScan::Ready
            && self
                .snapshot
                .host_adapters
                .iter()
                .any(|item| item.name == self.snapshot.host_adapter)
            && self
                .snapshot
                .plan
                .as_ref()
                .is_some_and(|plan| plan.can_start)
    }

    /// 本机所选接收网卡报告的 NlMtu；未选或系统未报告时为 None。
    fn host_nic_mtu(&self) -> Option<u32> {
        self.snapshot
            .host_adapters
            .iter()
            .find(|item| item.name == self.snapshot.host_adapter)
            .and_then(|item| item.mtu)
    }

    /// 被采集机发送网卡的 MTU 预览：显式选择按名称匹配，自动模式只接受
    /// 唯一已连接物理网卡；多张时要求明确选择。
    fn peer_nic_mtu(&self) -> Result<Option<u32>, String> {
        if !self.snapshot.peer_adapter.is_empty() {
            let adapter = self
                .snapshot
                .peer_adapters
                .iter()
                .find(|item| item.name == self.snapshot.peer_adapter)
                .ok_or_else(|| "所选发送网卡不在当前已连接网卡列表中。".to_string())?;
            if !adapter.is_connected_physical_nic() {
                return Err("所选发送网卡未连接或不是物理设备，请重新选择。".into());
            }
            return Ok(adapter.mtu);
        }
        Ok(
            vc_core::adapters::unique_connected_physical(&self.snapshot.peer_adapters)?
                .and_then(|item| item.mtu),
        )
    }

    /// 本次生效 MTU 的说明：自动模式取主机解析结果，显式模式取配置值。
    fn effective_mtu_text(&self) -> String {
        match (
            self.snapshot.config.capture.mtu_auto,
            self.snapshot.effective_mtu,
        ) {
            (true, Some(value)) => format!("{value}（自动）"),
            (true, None) => "待解析".into(),
            (false, _) => format!("{}（显式）", self.snapshot.config.capture.mtu),
        }
    }

    /// 网卡实际 MTU 与本次生效 MTU 的说明文本，供设置区与链路预算共用。
    fn mtu_hint(&self) -> String {
        let host = self
            .host_nic_mtu()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "未知".into());
        let peer = match self.peer_nic_mtu() {
            Ok(Some(value)) => value.to_string(),
            Ok(None) => "未知".into(),
            Err(error) => error,
        };
        format!(
            "网卡实际 MTU：本机 {host} · 被采集机 {peer}；生效 MTU：{}",
            self.effective_mtu_text()
        )
    }

    /// 校验输入框并写入显式 MTU；输入值不合法时只提示，不修改配置。
    fn apply_mtu(&mut self, cx: &mut Context<Self>) {
        let raw = self.mtu_input.read(cx).value().trim().to_string();
        match raw.parse::<u32>() {
            Ok(mtu) if (1500..=9000).contains(&mtu) => {
                self.mtu_error = None;
                self.send(Command::PatchCapture(
                    json!({"mtu_auto": false, "mtu": mtu}),
                ));
            }
            Ok(_) => self.mtu_error = Some("MTU 必须在 1500..9000 之间。".into()),
            Err(_) => self.mtu_error = Some("MTU 必须是整数。".into()),
        }
        cx.notify();
    }

    fn header(&self, cx: &mut Context<Self>) -> Div {
        let (state, color) = self.connection();
        div()
            .h(px(72.))
            .px(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .bg(rgb(SURFACE))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(36.))
                            .rounded(px(10.))
                            .bg(rgb(STAGE))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(symbol("capture", 20., 0xffffff)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("VirtualCapture"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(TEXT_DIM))
                                    .child("接收端"),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new("select-role")
                            .label("重新选择角色")
                            .ghost()
                            .compact()
                            .disabled(self.locked() || self.snapshot.restart_requested)
                            .on_click(cx.listener(|this, _, _, _| this.send(Command::SelectRole))),
                    )
                    .child(
                        Button::new("updates")
                            .label("更新")
                            .ghost()
                            .compact()
                            .selected(self.show_updates)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_updates = !this.show_updates;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT_DIM))
                            .child("主机工作台"),
                    )
                    .child(div().h(px(16.)).w(px(1.)).bg(rgb(BORDER)))
                    .child(status_pill(state, color)),
            )
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let locked = self.locked();
        div()
            .id("sidebar")
            .w(px(296.))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(SURFACE))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .id("sidebar-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .p(px(22.))
                    .flex()
                    .flex_col()
                    .gap(px(24.))
                    .child(self.connect_section(cx))
                    .child(divider())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(section_heading("monitor", "本机接收网卡"))
                            .child(self.host_adapters_section(cx, locked)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(section_heading("network", "被采集机发送网卡"))
                            .child(self.peer_adapters_section(cx, locked)),
                    )
                    .child(divider())
                    .child(self.link_summary())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(px(11.))
                            .text_color(rgb(TEXT_DIM))
                            .child(symbol("shield", 14., TEXT_DIM))
                            .child("Raw 原始画面 · 无压缩传输"),
                    ),
            )
    }

    fn connect_section(&self, cx: &mut Context<Self>) -> Div {
        let disabled = self.locked();
        let connect = Button::new("connect")
            .label("连接设备")
            .primary()
            .w_full()
            .h(px(38.))
            .disabled(disabled)
            .on_click(cx.listener(|this, _, _, app| {
                let raw = this.peer_input.read(app).value().to_string();
                if let Some(url) = peer_url_from_input(&raw) {
                    this.startup_notice = None;
                    this.send(Command::Connect(url));
                } else {
                    this.startup_notice = Some("请输入有效的 IPv4 地址，可附加端口。".into());
                }
            }));
        let refresh = Button::new("refresh")
            .label("刷新")
            .ghost()
            .compact()
            .disabled(disabled)
            .on_click(cx.listener(|this, _, _, _| this.send(Command::Refresh)));
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_heading("link", "连接被采集机"))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT_DIM))
                    .child("填写另一台电脑的 IP 地址"),
            )
            .child(div().h(px(38.)).child(Input::new(&self.peer_input)))
            .child(connect)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(TEXT_DIM))
                            .child(if disabled {
                                "会话处理中，设置已锁定"
                            } else {
                                "默认端口 5143"
                            }),
                    )
                    .child(refresh),
            )
    }

    fn host_adapters_section(&self, cx: &mut Context<Self>, locked: bool) -> Div {
        let mut section = div().flex().flex_col().gap_2();
        if self.snapshot.host_adapters.is_empty() {
            let (title, detail) = match self.snapshot.host_scan {
                HostScan::NotScanned => ("等待识别已连接网卡", "连接设备或点击刷新后识别。"),
                HostScan::NoPhysical => (
                    "未发现已连接的物理网卡",
                    "请检查 Npcap 和系统网卡连接状态后刷新。",
                ),
                HostScan::Failed => ("网卡识别失败", "请检查本机环境后点击刷新重试。"),
                HostScan::Ready => ("请选择接收网卡", "点击刷新重新识别网卡。"),
            };
            section = section.child(empty_hint(title, detail));
        }
        section = section.child(
            div()
                .text_size(px(11.))
                .text_color(rgb(TEXT_DIM))
                .child("仅显示已连接的物理网卡，虚拟设备、未连接设备与回环已隐藏。"),
        );
        for item in &self.snapshot.host_adapters {
            section = section.child(self.adapter_row(
                cx,
                item,
                self.snapshot.host_adapter == item.name,
                false,
                !locked,
            ));
        }
        section
    }

    fn peer_adapters_section(&self, cx: &mut Context<Self>, locked: bool) -> Div {
        let selected = self.snapshot.peer_adapter.is_empty();
        let mut auto = div()
            .id("peer-auto")
            .p_3()
            .rounded(px(9.))
            .border_1()
            .bg(rgb(if selected { ACCENT_SOFT } else { FIELD }))
            .border_color(rgb(if selected { 0xcad5fb } else { BORDER }))
            .flex()
            .items_center()
            .gap_2()
            .child(symbol(
                if selected { "check" } else { "network" },
                16.,
                if selected { ACCENT } else { TEXT_DIM },
            ))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("自动选择网卡"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(TEXT_DIM))
                            .child("仅限已连接物理网卡；多张请手动选择"),
                    ),
            );
        if !locked {
            auto = auto
                .cursor_pointer()
                .hover(|style| style.bg(rgb(ACCENT_SOFT)))
                .on_click(cx.listener(|this, _, _, _| {
                    this.send(Command::SelectPeerAdapter(String::new()))
                }));
        }
        let mut section = div().flex().flex_col().gap_2().child(auto);
        if self.snapshot.connected && self.snapshot.peer_adapters.is_empty() {
            section = section.child(empty_hint(
                "未发现已连接的物理网卡",
                "请检查被采集机 Npcap 和网卡连接后刷新。",
            ));
        }
        for item in &self.snapshot.peer_adapters {
            section = section.child(self.adapter_row(
                cx,
                item,
                self.snapshot.peer_adapter == item.name,
                true,
                !locked,
            ));
        }
        section
    }

    fn adapter_row(
        &self,
        cx: &mut Context<Self>,
        item: &AdapterInfo,
        selected: bool,
        peer: bool,
        enabled: bool,
    ) -> impl IntoElement {
        let name = item.name.clone();
        let title = item
            .label
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| item.description.clone());
        let speed = if peer {
            item.transmit_link_bps
        } else {
            item.receive_link_bps
        }
        .map(budget::human_bps)
        .unwrap_or_else(|| "速率未知".into());
        let mut row = div()
            .id(SharedString::from(format!("nic:{peer}:{name}")))
            .p_3()
            .rounded(px(9.))
            .border_1()
            .border_color(rgb(if selected { 0xcad5fb } else { BORDER }))
            .bg(rgb(if selected { ACCENT_SOFT } else { SURFACE }))
            .flex()
            .items_start()
            .gap_2()
            .child(symbol(
                if selected { "check" } else { "network" },
                16.,
                if selected { ACCENT } else { TEXT_DIM },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(TEXT_DIM))
                            .child(format!(
                                "{speed} · {}",
                                item.status.as_deref().unwrap_or("状态未知")
                            )),
                    ),
            );
        if enabled {
            row = row
                .cursor_pointer()
                .hover(|style| style.bg(rgb(FIELD)))
                .on_click(cx.listener(move |this, _, _, _| {
                    this.send(if peer {
                        Command::SelectPeerAdapter(name.clone())
                    } else {
                        Command::SelectHostAdapter(name.clone())
                    });
                }));
        }
        row
    }

    fn link_summary(&self) -> Div {
        let wire = self
            .snapshot
            .plan
            .as_ref()
            .map(|p| budget::human_bps(p.network.wire_bps))
            .unwrap_or_else(|| "待计算".into());
        let capacity = self
            .snapshot
            .plan
            .as_ref()
            .and_then(|p| p.network.usable_bps)
            .map(budget::human_bps)
            .unwrap_or_else(|| "等待连接".into());
        let (label, color) = self
            .snapshot
            .plan
            .as_ref()
            .map(plan_label)
            .unwrap_or(("等待检测", TEXT_DIM));
        let (vcam, _) = vcam_label(&self.snapshot.vcam);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(section_heading("activity", "链路预算"))
                    .child(div().text_size(px(11.)).text_color(rgb(color)).child(label)),
            )
            .child(key_value("预计线上开销", &wire))
            .child(key_value("预留后可用", &capacity))
            .child(key_value("链路 MTU", &self.effective_mtu_text()))
            .child(key_value("虚拟采集卡", vcam))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(TEXT_DIM))
                    .child("预算用于启动前检查，实际表现以运行数据为准。"),
            )
    }

    fn main_area(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("main")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .p(px(28.))
            .flex()
            .flex_col()
            .gap(px(20.))
            .when_some(self.notice(), |this, (title, body, color)| {
                this.child(notice(&title, &body, color))
            })
            .when(self.show_updates, |this| this.child(self.update_panel(cx)))
            .child(
                div()
                    .flex_shrink_0()
                    .rounded(px(14.))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(SURFACE))
                    .flex()
                    .flex_col()
                    .child(self.capture_section(cx))
                    .child(self.format_stage(cx)),
            )
            .child(self.status_panel(cx))
            .child(self.event_panel())
    }

    fn update_panel(&self, cx: &mut Context<Self>) -> Div {
        let locked = self.locked();
        let connected = self.snapshot.connected && self.snapshot.peer_online;
        let paired = self.snapshot.update_token_paired;
        let local = UpdateSummary::parse(&self.snapshot.local_update);
        let peer = UpdateSummary::parse(&self.snapshot.peer_update);
        let token_ready = vc_core::update::valid_token(self.update_token.read(cx).value().trim());
        let local_id = local.staged.as_ref().map(|build| build.build_id.clone());
        let local_ready = local_id.is_some() && !locked;
        let peer_id = peer.staged.as_ref().map(|build| build.build_id.clone());
        let peer_ready = peer_id.is_some() && !locked && connected && paired;
        let pick_local = {
            let disabled = locked;
            Button::new("pick-local-update")
                .label("选择 ZIP")
                .ghost()
                .compact()
                .disabled(disabled)
                .on_click(cx.listener(|this, _, _, cx| this.pick_update_zip(false, cx)))
        };
        let pick_peer = {
            let disabled = locked || !connected || !paired;
            Button::new("pick-peer-update")
                .label("选择 ZIP 上传")
                .ghost()
                .compact()
                .disabled(disabled)
                .on_click(cx.listener(|this, _, _, cx| this.pick_update_zip(true, cx)))
        };
        let apply_local = Button::new("apply-local-update")
            .label("应用更新")
            .primary()
            .compact()
            .disabled(!local_ready)
            .on_click(cx.listener(move |this, _, _, _| {
                if let Some(build_id) = local_id.clone() {
                    this.send(Command::ActivateLocalUpdate(build_id));
                }
            }));
        let apply_peer = Button::new("apply-peer-update")
            .label("应用更新")
            .primary()
            .compact()
            .disabled(!peer_ready)
            .on_click(cx.listener(move |this, _, _, _| {
                if let Some(build_id) = peer_id.clone() {
                    this.send(Command::ActivatePeerUpdate(build_id));
                }
            }));
        let pair = Button::new("pair-update-token")
            .label("配对")
            .compact()
            .disabled(locked || !token_ready)
            .on_click(cx.listener(|this, _, window, app| {
                let token = this.update_token.read(app).value().trim().to_string();
                if !token.is_empty() {
                    this.send(Command::SetPeerUpdateToken(token));
                    this.pair_edit = false;
                    this.update_token
                        .update(app, |state, cx| state.set_value("", window, cx));
                }
            }));
        let repair = Button::new("repair-update-token")
            .label("更换令牌")
            .ghost()
            .compact()
            .disabled(locked)
            .on_click(cx.listener(|this, _, _, cx| {
                this.pair_edit = true;
                cx.notify();
            }));
        let cancel_repair = Button::new("cancel-repair-update-token")
            .label("取消")
            .ghost()
            .compact()
            .on_click(cx.listener(|this, _, _, cx| {
                this.pair_edit = false;
                cx.notify();
            }));
        let peer_controls = if paired && !self.pair_edit {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().text_size(px(11.)).text_color(rgb(OK)).child("已配对"))
                .child(repair)
                .child(pick_peer)
                .child(apply_peer)
        } else {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h(px(32.))
                        .child(Input::new(&self.update_token)),
                )
                .child(pair)
                .when(paired, |this| this.child(cancel_repair))
        };
        let local_rollback = local.rollback.as_ref().map(|target| {
            let build_id = target.build_id.clone();
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(11.))
                        .text_color(rgb(TEXT_DIM))
                        .child(rollback_text(target)),
                )
                .child(
                    Button::new("rollback-local-update")
                        .label("回滚")
                        .ghost()
                        .compact()
                        .disabled(locked)
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.send(Command::RollbackLocalUpdate(build_id.clone()));
                        })),
                )
        });
        let peer_rollback = peer.rollback.as_ref().map(|target| {
            let build_id = target.build_id.clone();
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(11.))
                        .text_color(rgb(TEXT_DIM))
                        .child(rollback_text(target)),
                )
                .child(
                    Button::new("rollback-peer-update")
                        .label("回滚")
                        .ghost()
                        .compact()
                        .disabled(locked || !connected || !paired)
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.send(Command::RollbackPeerUpdate(build_id.clone()));
                        })),
                )
        });
        let local_error = local.error.as_deref().map(update_error_line);
        let peer_error = peer.error.as_deref().map(update_error_line);
        let message = self.snapshot.update_message.as_ref().map(|text| {
            let color = if text.contains("失败") || text.contains("错误") || text.contains("无效")
            {
                ERR
            } else {
                TEXT_DIM
            };
            div()
                .text_size(px(11.))
                .text_color(rgb(color))
                .child(text.clone())
        });
        div()
            .flex_shrink_0()
            .rounded(px(12.))
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(SURFACE))
            .p(px(16.))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(section_heading("download", "应用更新"))
                            .child(div().text_size(px(11.)).text_color(rgb(TEXT_DIM)).child(
                                if locked {
                                    "会话处理中，暂不可更新"
                                } else {
                                    "ZIP 由更新助手校验并切换"
                                },
                            )),
                    )
                    .child(
                        Button::new("refresh-updates")
                            .label("刷新状态")
                            .ghost()
                            .compact()
                            .disabled(locked)
                            .on_click(
                                cx.listener(|this, _, _, _| this.send(Command::RefreshUpdates)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("本机应用"),
                            )
                            .child(update_line("当前", &local.active))
                            .child(update_line("已暂存", &local.staged))
                            .when_some(local_error, |this, line| this.child(line))
                            .when_some(local_rollback, |this, row| this.child(row))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(pick_local)
                                    .child(apply_local),
                            ),
                    )
                    .child(div().w(px(1.)).bg(rgb(BORDER)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("被采集机"),
                            )
                            .child(update_line("当前", &peer.active))
                            .child(update_line("已暂存", &peer.staged))
                            .when_some(peer_error, |this, line| this.child(line))
                            .when_some(peer_rollback, |this, row| this.child(row))
                            .child(peer_controls)
                            .when(!connected, |this| {
                                this.child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(rgb(TEXT_DIM))
                                        .child("连接设备后可更新被采集机。"),
                                )
                            }),
                    ),
            )
            .when_some(message, |this, message| this.child(message))
    }

    fn pick_update_zip(&self, peer: bool, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(SharedString::from("选择更新 ZIP")),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                let command = if peer {
                    Command::StagePeerUpdate(path)
                } else {
                    Command::StageLocalUpdate(path)
                };
                let _ = this.update(cx, |this, cx| {
                    this.send(command);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn capture_action(&self, cx: &mut Context<Self>) -> Button {
        let running = self.snapshot.running;
        let cleanup = self.snapshot.cleanup_pending;
        let remote_pending = self.snapshot.remote_pending;
        let remote_running = self.snapshot.connected
            && self.snapshot.peer_online
            && self.snapshot.sender["running"] == true
            && self.snapshot.sender["session_id"]
                .as_str()
                .is_some_and(|id| !id.trim().is_empty());
        let busy = self.snapshot.busy;
        let intent = capture_intent(running, cleanup, remote_pending, remote_running);
        let stop_available = intent != CaptureIntent::Start;
        let label = if busy {
            "正在处理…"
        } else {
            match intent {
                CaptureIntent::Start => "开始采集",
                CaptureIntent::CleanupLocal => "清理本地残留",
                CaptureIntent::StopRemote if running => "停止采集",
                CaptureIntent::StopRemote if remote_running => "停止远端采集",
                CaptureIntent::StopRemote => "重试停止",
            }
        };
        let mut action = Button::new("toggle")
            .label(label)
            .h(px(42.))
            .px_5()
            .disabled(busy || (intent == CaptureIntent::Start && !self.can_start()))
            .loading(busy);
        action = if stop_available {
            action.danger()
        } else {
            action.primary()
        };
        action.on_click(cx.listener(move |this, _, _, _| {
            this.send(match intent {
                CaptureIntent::Start => Command::Start,
                CaptureIntent::StopRemote => Command::Stop,
                CaptureIntent::CleanupLocal => Command::CleanupLocal,
            });
        }))
    }

    fn notice(&self) -> Option<(String, String, u32)> {
        if let Some(error) = self
            .startup_notice
            .as_ref()
            .or(self.snapshot.error.as_ref())
        {
            return Some(("暂时无法继续".into(), error.clone(), ERR));
        }
        if self.snapshot.connected && !self.snapshot.peer_online {
            return Some((
                "设备连接已中断".into(),
                "请检查被采集机后台与网络连接，采集端指标已清空。".into(),
                ERR,
            ));
        }
        if self.snapshot.connected
            && let Some(plan) = &self.snapshot.plan
        {
            if let Some(error) = plan.errors.first() {
                return Some(("启动前检查未通过".into(), error.clone(), ERR));
            }
            if let Some(warning) = plan.warnings.first() {
                return Some(("请留意链路预算".into(), warning.clone(), WARN));
            }
        }
        None
    }

    fn format_stage(&self, cx: &mut Context<Self>) -> Div {
        let c = &self.snapshot.config.capture;
        // 只用于示意框布局，不参与像素生成或采集变换。
        let (width, height) = if c.width >= c.height {
            (72., 72. * c.height as f32 / c.width.max(1) as f32)
        } else {
            (72. * c.width as f32 / c.height.max(1) as f32, 72.)
        };
        div()
            .min_h(px(124.))
            .rounded_b(px(13.))
            .bg(rgb(STAGE))
            .px(px(22.))
            .py(px(20.))
            .flex()
            .items_center()
            .gap(px(20.))
            .text_color(rgb(0xffffff))
            .child(
                div()
                    .w(px(84.))
                    .h(px(80.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .w(px(width))
                            .h(px(height))
                            .relative()
                            .border_1()
                            .border_color(rgb(STAGE_LINE))
                            .rounded(px(3.))
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(div().h_full().w(px(1.)).bg(rgb(STAGE_LINE))),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(div().w_full().h(px(1.)).bg(rgb(STAGE_LINE))),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .top(px(-2.))
                                    .left(px(-2.))
                                    .size(px(10.))
                                    .border_t_2()
                                    .border_l_2()
                                    .border_color(rgb(0xc1d0e7)),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .bottom(px(-2.))
                                    .right(px(-2.))
                                    .size(px(10.))
                                    .border_b_2()
                                    .border_r_2()
                                    .border_color(rgb(0xc1d0e7)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(7.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_size(px(19.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(format!("{} × {}", c.width, c.height)),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(STAGE_MUTED))
                                    .child(format!("{} fps", c.fps)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(STAGE_MUTED))
                            .child(format!(
                                "{} / NV12 / {}",
                                match c.codec.as_str() {
                                    "raw" => "Raw 无压缩",
                                    "mjpeg" => "MJPEG（不受支持）",
                                    "pyrowave" => "PyroWave 小波",
                                    _ => "未知编码",
                                },
                                transform_label(&c.transform)
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(if self.snapshot.cleanup_pending {
                                WARN
                            } else {
                                STAGE_MUTED
                            }))
                            .child(if self.snapshot.cleanup_pending {
                                "上次会话资源待清理，原因见最近活动。"
                            } else {
                                "画幅示意，非实时预览"
                            }),
                    ),
            )
            .child(self.capture_action(cx))
    }

    fn capture_section(&self, cx: &mut Context<Self>) -> Div {
        let c = &self.snapshot.config.capture;
        let enabled = !self.locked();
        div()
            .min_w_0()
            .px(px(22.))
            .pt(px(20.))
            .pb(px(12.))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pb(px(18.))
                    .child(section_heading("sliders", "画面设置"))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(TEXT_DIM))
                            .child(if enabled {
                                "自动保存"
                            } else {
                                "会话中已锁定"
                            }),
                    ),
            )
            .child(divider())
            .child(
                setting_row("输出分辨率").child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .children(
                            [
                                (416, 416, "416 × 416"),
                                (640, 640, "640 × 640"),
                                (1280, 720, "720p"),
                                (1920, 1080, "1080p"),
                                (2560, 1440, "1440p"),
                            ]
                            .into_iter()
                            .map(|(w, h, label)| {
                                self.chip(
                                    cx,
                                    label,
                                    c.width == w && c.height == h,
                                    json!({"width":w,"height":h}),
                                    enabled,
                                )
                            }),
                        ),
                ),
            )
            .child(divider())
            .child(
                setting_row("目标帧率").child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .children([60, 120, 240].into_iter().map(|fps| {
                            self.chip(
                                cx,
                                &format!("{fps} fps"),
                                c.fps == fps,
                                json!({"fps":fps}),
                                enabled,
                            )
                        })),
                ),
            )
            .child(divider())
            .child(
                setting_row("画面适配").child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .children(
                            [
                                ("crop", "裁切"),
                                ("fit", "完整画面"),
                                ("fill", "铺满"),
                                ("stretch", "拉伸"),
                            ]
                            .into_iter()
                            .map(|(value, label)| {
                                self.chip(
                                    cx,
                                    label,
                                    c.transform == value,
                                    json!({"transform":value}),
                                    enabled,
                                )
                            }),
                        ),
                ),
            )
            .child(divider())
            .child(
                setting_row("采集来源").child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .children(
                            [("dda", "DDA 桌面"), ("wgc", "WGC"), ("pattern", "测试图案")]
                                .into_iter()
                                .map(|(value, label)| {
                                    // 桌面来源只走 GPU 转换；测试图案是合成源，必须显式关闭。
                                    self.chip(
                                        cx,
                                        label,
                                        c.mode == value,
                                        json!({
                                            "mode": value,
                                            "gpu_convert": value != "pattern",
                                        }),
                                        enabled,
                                    )
                                }),
                        ),
                ),
            )
            .child(divider())
            .child(
                setting_row("链路 MTU").child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .child(self.mtu_chip(
                                    cx,
                                    "自动（按网卡）",
                                    c.mtu_auto,
                                    json!({"mtu_auto": true}),
                                    enabled,
                                    None,
                                ))
                                .child(self.mtu_chip(
                                    cx,
                                    "1500",
                                    !c.mtu_auto && c.mtu == 1500,
                                    json!({"mtu_auto": false, "mtu": 1500}),
                                    enabled,
                                    Some(1500),
                                ))
                                .child(self.mtu_chip(
                                    cx,
                                    "9000",
                                    !c.mtu_auto && c.mtu == 9000,
                                    json!({"mtu_auto": false, "mtu": 9000}),
                                    enabled,
                                    Some(9000),
                                ))
                                .child(
                                    div()
                                        .w(px(96.))
                                        .h(px(34.))
                                        .child(Input::new(&self.mtu_input)),
                                )
                                .child(
                                    Button::new("apply-mtu")
                                        .label("应用")
                                        .compact()
                                        .disabled(!enabled)
                                        .on_click(cx.listener(|this, _, _, cx| this.apply_mtu(cx))),
                                ),
                        )
                        .when_some(self.mtu_error.clone(), |this, error| {
                            this.child(div().text_size(px(11.)).text_color(rgb(ERR)).child(error))
                        })
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(TEXT_DIM))
                                .child(self.mtu_hint()),
                        ),
                ),
            )
    }

    /// 芯片按钮的统一外观；点击行为由调用方附加。
    fn chip_style(&self, label: &str, id: String, selected: bool, enabled: bool) -> Button {
        Button::new(SharedString::from(id))
            .label(label.to_owned())
            .outline()
            .selected(selected)
            .disabled(!enabled)
            .h(px(34.))
            .px(px(12.))
            .rounded(px(7.))
            .text_size(px(12.))
            .border_color(rgb(if selected { 0xb8c8fa } else { BORDER }))
            .bg(rgb(if selected { ACCENT_SOFT } else { SURFACE }))
            .text_color(rgb(if !enabled {
                TEXT_FAINT
            } else if selected {
                ACCENT
            } else {
                TEXT_DIM
            }))
    }

    fn chip(
        &self,
        cx: &mut Context<Self>,
        label: &str,
        selected: bool,
        patch: Value,
        enabled: bool,
    ) -> Button {
        let mut chip = self.chip_style(label, format!("chip:{patch}"), selected, enabled);
        if enabled {
            chip =
                chip.on_click(cx.listener(move |this, _, _, _| {
                    this.send(Command::PatchCapture(patch.clone()))
                }));
        }
        chip
    }

    /// MTU 芯片：点击时同步输入框，避免“芯片已选中但输入框仍是旧值”。
    fn mtu_chip(
        &self,
        cx: &mut Context<Self>,
        label: &str,
        selected: bool,
        patch: Value,
        enabled: bool,
        sync: Option<u32>,
    ) -> Button {
        let mut chip = self.chip_style(label, format!("mtu-chip:{patch}"), selected, enabled);
        if enabled {
            chip = chip.on_click(cx.listener(move |this, _, window, app| {
                this.mtu_error = None;
                if let Some(value) = sync {
                    this.mtu_input.update(app, |state, cx| {
                        state.set_value(value.to_string(), window, cx)
                    });
                }
                this.send(Command::PatchCapture(patch.clone()));
            }));
        }
        chip
    }

    fn status_panel(&self, cx: &mut Context<Self>) -> Div {
        let running = self.snapshot.running;
        let live = self.snapshot.connected && self.snapshot.peer_online;
        let mut content = panel().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(section_heading("activity", "运行状态"))
                .child(status_pill(
                    if running { "采集中" } else { "待开始" },
                    if running { OK } else { TEXT_DIM },
                )),
        );
        if running {
            content = content.child(
                div()
                    .flex()
                    .gap(px(28.))
                    .child(reading(
                        "发送帧率",
                        field(&self.snapshot.sender, "fps_actual", live, num),
                        "fps",
                    ))
                    .child(reading(
                        "接收帧率",
                        field(&self.snapshot.receiver, "fps", true, num),
                        "fps",
                    ))
                    .child(reading(
                        "接收完成",
                        field(&self.snapshot.receiver, "complete", true, num),
                        "帧",
                    )),
            );
        } else {
            content = content.child(
                div()
                    .py_4()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .size(px(44.))
                            .rounded(px(12.))
                            .bg(rgb(FIELD))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(symbol("monitor", 22., TEXT_DIM)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(if self.snapshot.connected {
                                        "设备已就位，等待开始采集"
                                    } else {
                                        "连接后，你的画面将在这里启程"
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(TEXT_DIM))
                                    .child("开始采集后，将显示真实帧率、传输与完整性数据。"),
                            ),
                    ),
            );
        }
        content = content.child(divider()).child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(TEXT_DIM))
                        .child("双端帧率、丢包与哈希校验"),
                )
                .child(
                    Button::new("details")
                        .label(if self.show_details {
                            "收起明细"
                        } else {
                            "查看明细"
                        })
                        .ghost()
                        .compact()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_details = !this.show_details;
                            cx.notify();
                        })),
                ),
        );
        if self.show_details {
            content = content.child(self.stats_table(live, running));
        }
        content
    }

    fn stats_table(&self, live: bool, running: bool) -> Div {
        let sender = &self.snapshot.sender;
        let receiver = &self.snapshot.receiver;
        let sl = live && running;
        let mut table = div()
            .flex()
            .flex_col()
            .child(stat_row("指标", "采集端".into(), "本机接收端".into()))
            .child(stat_row(
                "实际帧率",
                field(sender, "fps_actual", sl, num),
                field_any(receiver, &["fps", "fps_actual"], running, num),
            ))
            .child(stat_row(
                "完成帧数",
                field(sender, "sent_frames", sl, num),
                field(receiver, "complete", running, num),
            ))
            .child(stat_row(
                "带宽 / 丢弃帧",
                field(sender, "bps", sl, bytes_bps),
                field(receiver, "dropped", running, num),
            ))
            .child(stat_row(
                "发送错误 / 丢包",
                field(sender, "send_errors", sl, num),
                field(receiver, "packets_lost", running, num),
            ))
            .child(stat_row(
                "哈希不一致",
                "—".into(),
                field(receiver, "hash_mismatch", running, num),
            ))
            .child(stat_row(
                "到帧间隔 p99",
                "—".into(),
                field_any(
                    receiver,
                    &["inter_arrival_p99_ms", "inter_arrival.p99_ms"],
                    running,
                    ms,
                ),
            ))
            .child(stat_row(
                "采集转换 p99",
                field(sender, "cap_lag_p99_ms", sl, ms),
                "—".into(),
            ))
            .child(stat_row(
                "发送分段 p99",
                field(sender, "pc1_seg_p99_ms", sl, ms),
                "—".into(),
            ))
            .child(stat_row(
                "重组分段 p99",
                "—".into(),
                field_any(receiver, &["rseg_p99_ms", "rseg.p99_ms"], running, ms),
            ))
            .child(stat_row(
                "虚拟卡交付 p99",
                "—".into(),
                field(receiver, "vseg_p99_ms", running, ms),
            ))
            .child(stat_row(
                "GPU / 虚拟卡交付",
                field(sender, "gpu_ms", sl, ms),
                field_any(receiver, &["vcam_delivered", "shm_published"], running, num),
            ))
            .child(stat_row(
                "退出码",
                field(sender, "exit_code", sl, display),
                field(receiver, "exit_code", running, display),
            ));
        for (side, stats, enabled) in [("采集端", sender, sl), ("接收端", receiver, running)]
        {
            if enabled && let Some(error) = stats.get("error").and_then(Value::as_str) {
                table = table.child(
                    div()
                        .py_2()
                        .text_size(px(12.))
                        .text_color(rgb(ERR))
                        .child(format!("{side}：{error}")),
                );
            }
        }
        table
    }

    fn event_panel(&self) -> Div {
        let events = self.snapshot.event_text();
        let mut content = div().flex().flex_col().gap_3().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(section_heading("clock", "最近活动"))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(TEXT_DIM))
                        .child("仅本次运行"),
                ),
        );
        if events.is_empty() {
            content = content.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT_DIM))
                    .child("连接、开始和停止的记录会显示在这里。"),
            );
        }
        for event in events.into_iter().take(6) {
            // 工作线程时间戳是固定长度的 ASCII 前缀，其后为事件正文。
            let (time, message) = if event.len() >= 10 && event.is_char_boundary(10) {
                event.split_at(10)
            } else {
                ("", event.as_str())
            };
            content = content.child(
                div()
                    .flex()
                    .items_start()
                    .gap_3()
                    .text_size(px(12.))
                    .child(div().text_color(rgb(TEXT_DIM)).child(time.to_owned()))
                    .child(div().flex_1().child(message.to_owned())),
            );
        }
        content
    }
}

impl Render for HostApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_size(px(14.))
            .font(Font {
                family: "Segoe UI".into(),
                fallbacks: Some(FontFallbacks::from_fonts(vec!["Microsoft YaHei UI".into()])),
                ..Default::default()
            })
            .child(self.header(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .overflow_hidden()
                    .child(self.sidebar(cx))
                    .child(self.main_area(cx)),
            )
    }
}

pub(crate) fn symbol(name: &str, pixels: f32, color: u32) -> Icon {
    let path = match name {
        "capture" => {
            "<path d='M8 3H4a1 1 0 0 0-1 1v4m13-5h4a1 1 0 0 1 1 1v4M3 16v4a1 1 0 0 0 1 1h4m8 0h4a1 1 0 0 0 1-1v-4'/><rect x='7' y='7' width='10' height='10' rx='2'/>"
        }
        "monitor" => "<rect x='3' y='4' width='18' height='13' rx='2'/><path d='M8 21h8m-4-4v4'/>",
        "network" => {
            "<rect x='8' y='3' width='8' height='6' rx='1'/><rect x='2' y='16' width='7' height='5' rx='1'/><rect x='15' y='16' width='7' height='5' rx='1'/><path d='M12 9v4H5.5v3M12 13h6.5v3'/>"
        }
        "link" => {
            "<path d='m10 13 4-4m-5 6-2 2a4 4 0 0 1-6-6l4-4a4 4 0 0 1 6 0m2 2 2-2a4 4 0 0 1 6 6l-4 4a4 4 0 0 1-6 0' transform='translate(1 0)'/>"
        }
        "activity" => "<path d='M2 12h4l3-8 6 16 3-8h4'/>",
        "sliders" => {
            "<path d='M4 6h7m4 0h5M4 12h2m4 0h10M4 18h10m4 0h2'/><circle cx='13' cy='6' r='2'/><circle cx='8' cy='12' r='2'/><circle cx='16' cy='18' r='2'/>"
        }
        "shield" => "<path d='m12 3 8 3v6c0 4-4 7-8 9-4-2-8-5-8-9V6z'/><path d='m8 12 3 3 5-6'/>",
        "clock" => "<circle cx='12' cy='12' r='9'/><path d='M12 7v5l3 2'/>",
        "check" => "<path d='m5 12 4 4L19 6'/>",
        "download" => "<path d='M12 3v11m0 0 4-4m-4 4-4-4M4 17v3h16v-3'/>",
        _ => "<circle cx='12' cy='12' r='9'/><path d='M12 7v6m0 4h.01'/>",
    };
    let svg = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='currentColor' stroke-width='1.7' stroke-linecap='round' stroke-linejoin='round'>{path}</svg>"
    );
    Icon::default()
        .data(svg.as_bytes())
        .size(px(pixels))
        .text_color(rgb(color))
}
fn panel() -> Div {
    div()
        .p(px(20.))
        .rounded(px(12.))
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(BORDER))
        .flex()
        .flex_col()
        .gap_4()
}
fn section_heading(icon: &str, title: &str) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(symbol(icon, 16., TEXT_DIM))
        .child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
}
fn setting_row(title: &str) -> Div {
    div().flex().items_start().gap(px(16.)).py(px(13.)).child(
        div()
            .w(px(92.))
            .flex_shrink_0()
            .pt(px(8.))
            .text_size(px(12.))
            .text_color(rgb(TEXT_DIM))
            .child(title.to_owned()),
    )
}
fn divider() -> Div {
    div().h(px(1.)).w_full().flex_shrink_0().bg(rgb(BORDER))
}
fn empty_hint(title: &str, detail: &str) -> Div {
    div()
        .p_3()
        .rounded(px(8.))
        .bg(rgb(FIELD))
        .flex()
        .flex_col()
        .gap_2()
        .child(div().text_size(px(12.)).child(title.to_owned()))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(TEXT_DIM))
                .child(detail.to_owned()),
        )
}
fn status_pill(label: &str, color: u32) -> Div {
    div()
        .px_3()
        .py_1()
        .rounded_full()
        .bg(rgb(FIELD))
        .flex()
        .items_center()
        .gap_2()
        .child(div().size(px(6.)).rounded_full().bg(rgb(color)))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(TEXT_DIM))
                .child(label.to_owned()),
        )
}
fn key_value(label: &str, value: &str) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .text_size(px(11.))
        .child(div().text_color(rgb(TEXT_DIM)).child(label.to_owned()))
        .child(value.to_owned())
}
fn notice(title: &str, body: &str, color: u32) -> Div {
    div()
        .px_4()
        .py_3()
        .rounded(px(9.))
        .bg(rgb(if color == ERR { 0xfff1f1 } else { 0xfff7e8 }))
        .flex()
        .items_start()
        .gap_3()
        .child(symbol("alert", 17., color))
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.to_owned()),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(TEXT_DIM))
                        .child(body.to_owned()),
                ),
        )
}
fn reading(label: &str, value: String, unit: &str) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(TEXT_DIM))
                .child(label.to_owned()),
        )
        .child(
            div()
                .flex()
                .items_end()
                .gap_2()
                .child(
                    div()
                        .text_size(px(28.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(value),
                )
                .child(
                    div()
                        .pb_1()
                        .text_size(px(12.))
                        .text_color(rgb(TEXT_DIM))
                        .child(unit.to_owned()),
                ),
        )
}
fn stat_row(label: &str, sender: String, receiver: String) -> Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .py_2()
        .border_b_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .w(px(150.))
                .flex_shrink_0()
                .text_size(px(12.))
                .text_color(rgb(TEXT_DIM))
                .child(label.to_owned()),
        )
        .child(div().flex_1().text_size(px(13.)).child(sender))
        .child(div().flex_1().text_size(px(13.)).child(receiver))
}
fn field(stats: &Value, key: &str, enabled: bool, format: fn(&Value) -> String) -> String {
    if enabled {
        stats.get(key).map(format).unwrap_or_else(|| "—".into())
    } else {
        "—".into()
    }
}
/// 依次尝试多个兼容键；点号表示读取嵌套对象（如 `rseg.p99_ms`）。
fn field_any(stats: &Value, keys: &[&str], enabled: bool, format: fn(&Value) -> String) -> String {
    if !enabled {
        return "—".into();
    }
    keys.iter()
        .find_map(|key| {
            let mut value = stats;
            for part in key.split('.') {
                value = value.get(part)?;
            }
            Some(format(value))
        })
        .unwrap_or_else(|| "—".into())
}
fn update_line(label: &str, build: &Option<UpdateBuild>) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_size(px(11.))
        .child(div().text_color(rgb(TEXT_DIM)).child(format!("{label}：")))
        .child(match build {
            Some(build) if build.version.is_empty() => build.build_id.clone(),
            Some(build) => format!("{} ({})", build.version, build.build_id),
            None => "—".into(),
        })
}
fn update_error_line(error: &str) -> Div {
    div()
        .text_size(px(11.))
        .text_color(rgb(ERR))
        .child(format!("状态读取失败：{error}"))
}
fn rollback_text(build: &UpdateBuild) -> String {
    if build.version.is_empty() {
        format!("回滚到 {}", build.build_id)
    } else {
        format!("回滚到 {} ({})", build.version, build.build_id)
    }
}
fn plan_label(plan: &Plan) -> (&'static str, u32) {
    match plan.network.status {
        PlanStatus::Fits => ("可容纳", OK),
        PlanStatus::Margin => ("余量不足", WARN),
        PlanStatus::Exceeds => ("超线速", ERR),
        PlanStatus::Down => ("网卡断开", ERR),
        PlanStatus::Unknown => ("等待检测", TEXT_DIM),
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureIntent {
    Start,
    /// 显式停止/重试停止：允许发现并按精确 session_id 停止远端会话。
    StopRemote,
    /// 只清理本机接收/虚拟卡：绝不探测或停止远端会话。
    CleanupLocal,
}

fn capture_intent(
    running: bool,
    cleanup: bool,
    remote_pending: bool,
    remote_running: bool,
) -> CaptureIntent {
    if running || remote_pending || remote_running {
        CaptureIntent::StopRemote
    } else if cleanup {
        CaptureIntent::CleanupLocal
    } else {
        CaptureIntent::Start
    }
}

fn vcam_label(status: &VcamStatus) -> (&'static str, u32) {
    if status.registration_unknown {
        ("虚拟卡状态不可读", WARN)
    } else if status.managed() {
        ("已就绪", OK)
    } else if !status.registered.is_empty() {
        ("其他目录已注册", WARN)
    } else {
        ("待启动", TEXT_DIM)
    }
}
fn transform_label(transform: &str) -> &'static str {
    match transform {
        "crop" => "裁切",
        "fit" => "完整画面",
        "fill" => "铺满",
        "stretch" => "拉伸",
        _ => "未知",
    }
}
fn display(value: &Value) -> String {
    match value {
        Value::Null => "—".into(),
        Value::Bool(v) => v.to_string(),
        Value::Number(n) => n
            .as_f64()
            .map(|v| {
                if v.fract() == 0. {
                    format!("{v:.0}")
                } else {
                    format!("{v:.2}")
                }
            })
            .unwrap_or_else(|| n.to_string()),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
fn num(value: &Value) -> String {
    display(value)
}
fn ms(value: &Value) -> String {
    value
        .as_f64()
        .map(|v| format!("{v:.2} ms"))
        .unwrap_or_else(|| "—".into())
}
fn bytes_bps(value: &Value) -> String {
    value
        .as_f64()
        .map(|v| budget::human_bps(v.max(0.) as u64))
        .unwrap_or_else(|| "—".into())
}
fn peer_url_from_input(raw: &str) -> Option<String> {
    let text = raw.trim().trim_end_matches('/');
    if text.is_empty() {
        return None;
    }
    let rest = text.strip_prefix("http://").unwrap_or(text);
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok()?),
        None => (rest, 5143),
    };
    let url = format!("http://{host}:{port}");
    vc_core::config::valid_peer_url(&url).then_some(url)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_intent_distinguishes_local_cleanup_from_remote_stop() {
        // 运行中/显式 Stop 未确认/远端有会话：必须走显式 Stop（精确会话）。
        assert_eq!(
            capture_intent(true, true, true, true),
            CaptureIntent::StopRemote
        );
        assert_eq!(
            capture_intent(false, true, true, false),
            CaptureIntent::StopRemote
        );
        assert_eq!(
            capture_intent(false, false, false, true),
            CaptureIntent::StopRemote
        );
        // 只有本地残留、远端无 claim：绝不能借清理按钮触发远端发现/停止。
        assert_eq!(
            capture_intent(false, true, false, false),
            CaptureIntent::CleanupLocal
        );
        assert_eq!(
            capture_intent(false, false, false, false),
            CaptureIntent::Start
        );
    }
}
