//! 原生桌面入口。两个角色共用窗口，确认前不写启动偏好、不启动任何角色。

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use gpui_kit::component::Disableable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::switch::Switch;
use gpui_kit::prelude::*;
use gpui_kit::{
    Bounds, Context, Div, Entity, Font, FontFallbacks, FontWeight, QuitMode, Render, SharedString,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};
use vc_capture::{DesktopAction, DesktopService, DesktopStatus, Options};
use vc_core::Config;
use vc_core::nic_jumbo::{self, ApplyOutcome, NicJumboInfo};
use vc_core::startup::{self, DesktopRole};

use crate::host_ui::{
    self, ACCENT, ACCENT_SOFT, BG, BORDER, ERR, FIELD, HostApp, OK, SURFACE, TEXT, TEXT_DIM,
};

pub(crate) enum Start {
    Choose {
        previous: Option<DesktopRole>,
        error: Option<String>,
    },
    Role(DesktopRole),
}

struct CapturePane {
    service: Option<DesktopService>,
    status: DesktopStatus,
    notice: Option<String>,
    nics: Vec<NicJumboInfo>,
    selected_guid: Option<String>,
    nic_loading: bool,
    nic_applying: bool,
    nic_error: Option<String>,
    nic_feedback: Option<Result<String, String>>,
    capturing: bool,
    nic_query_rx: Option<mpsc::Receiver<Result<Vec<NicJumboInfo>, String>>>,
    nic_apply_rx: Option<mpsc::Receiver<Result<ApplyOutcome, String>>>,
}

struct DesktopApp {
    view: View,
    options: Options,
}

enum View {
    Choose {
        previous: Option<DesktopRole>,
        error: Option<String>,
    },
    Receive {
        workbench: Entity<HostApp>,
        _role_change: Subscription,
    },
    Capture(CapturePane),
}

impl DesktopApp {
    fn show(
        &mut self,
        start: Start,
        ready: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.view = match start {
            Start::Choose { previous, error } => {
                window.set_window_title("VirtualCapture · 选择本机角色");
                View::Choose { previous, error }
            }
            Start::Role(DesktopRole::Receive) => {
                window.set_window_title("VirtualCapture · 接收端");
                let workbench = cx.new(|cx| HostApp::new(window, cx, ready));
                let role_change =
                    cx.observe_in(&workbench, window, |app, workbench, window, cx| {
                        if workbench.read(cx).role_selection_ready() {
                            app.reselect(DesktopRole::Receive, window, cx);
                        }
                    });
                View::Receive {
                    workbench,
                    _role_change: role_change,
                }
            }
            Start::Role(DesktopRole::Capture) => {
                window.set_window_title("VirtualCapture · 采集端");
                let (service, notice) = match DesktopService::start(self.options.clone()) {
                    Ok(service) => (Some(service), None),
                    Err(error) => (None, Some(format!("无法启动采集服务：{error}"))),
                };
                self.watch_capture(window, cx);
                let mut pane = CapturePane {
                    service,
                    status: DesktopStatus::default(),
                    notice,
                    nics: Vec::new(),
                    selected_guid: None,
                    nic_loading: false,
                    nic_applying: false,
                    nic_error: None,
                    nic_feedback: None,
                    capturing: false,
                    nic_query_rx: None,
                    nic_apply_rx: None,
                };
                spawn_nic_query(&mut pane);
                View::Capture(pane)
            }
        };
        // 点击回调可能发生在 GPUI 绘制周期内；在当前事件结束后重新标脏视图。
        cx.defer_in(window, |_, window, cx| {
            cx.notify();
            window.refresh();
        });
    }

    fn reselect(&mut self, previous: DesktopRole, window: &mut Window, cx: &mut Context<Self>) {
        self.show(
            Start::Choose {
                previous: Some(previous),
                error: None,
            },
            None,
            window,
            cx,
        );
    }

    fn select(&mut self, role: DesktopRole, window: &mut Window, cx: &mut Context<Self>) {
        let View::Choose { error, .. } = &mut self.view else {
            return;
        };
        match startup::save(role) {
            Ok(()) => self.show(Start::Role(role), None, window, cx),
            Err(failure) => {
                *error = Some(format!(
                    "无法保存启动角色：{failure}。请检查运行目录的写入权限后重试。"
                ));
                cx.notify();
            }
        }
    }

    fn request_capture(
        &mut self,
        action: DesktopAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let View::Capture(pane) = &mut self.view else {
            return true;
        };
        if pane.nic_applying {
            pane.notice = Some("正在修改网卡巨帧设置，请等待完成。".into());
            cx.notify();
            return false;
        }
        if let Some(service) = &pane.service {
            if !pane.status.busy {
                pane.notice = service.request(action).err();
                pane.status = service.status();
                cx.notify();
            }
            // 关窗时也必须等待后台确认空闲、关闭监听并结束线程。
            false
        } else if action == DesktopAction::SelectRole {
            self.reselect(DesktopRole::Capture, window, cx);
            false
        } else {
            true
        }
    }

    fn refresh_nic_jumbo(&mut self, cx: &mut Context<Self>) {
        let View::Capture(pane) = &mut self.view else {
            return;
        };
        if pane.nic_loading || pane.nic_applying {
            return;
        }
        pane.nic_error = None;
        spawn_nic_query(pane);
        cx.notify();
    }

    fn select_nic_jumbo(&mut self, guid: String, cx: &mut Context<Self>) {
        let View::Capture(pane) = &mut self.view else {
            return;
        };
        if pane.nic_applying {
            return;
        }
        if pane.selected_guid.as_deref() != Some(guid.as_str()) {
            pane.selected_guid = Some(guid);
            pane.nic_feedback = None;
            cx.notify();
        }
    }

    fn toggle_nic_jumbo(&mut self, enable: bool, cx: &mut Context<Self>) {
        let View::Capture(pane) = &mut self.view else {
            return;
        };
        if pane.nic_loading || pane.nic_applying || pane.capturing || pane.status.busy {
            return;
        }
        let Some(selected) = selected_nic(pane) else {
            pane.nic_feedback = Some(Err("请先选择要设置巨帧的物理网卡。".into()));
            cx.notify();
            return;
        };
        if selected.is_jumbo_enabled() == enable {
            return;
        }
        let Some(targets) = selected.targets.clone() else {
            pane.nic_feedback = Some(Err(selected
                .unsupported_reason
                .clone()
                .unwrap_or_else(|| "当前网卡不支持 9000 巨帧切换。".into())));
            cx.notify();
            return;
        };
        let guid = selected.guid.clone();
        let guard = pane.service.as_ref().map(DesktopService::nic_guard);
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("vc-nic-jumbo-apply".into())
            .spawn(move || {
                let result = (|| {
                    if let Some(guard) = &guard {
                        guard.ensure_idle()?;
                    }
                    let outcome = nic_jumbo::apply_nic_jumbo(&guid, enable, &targets);
                    if let Some(guard) = &guard {
                        guard.invalidate_adapters_cache();
                    }
                    outcome
                })();
                let _ = tx.send(result);
            }) {
            Ok(_) => {
                pane.nic_applying = true;
                pane.nic_feedback = None;
                pane.notice = None;
                pane.nic_apply_rx = Some(rx);
            }
            Err(error) => {
                pane.nic_feedback = Some(Err(format!("无法启动网卡巨帧设置线程：{error}")));
            }
        }
        cx.notify();
    }

    fn watch_capture(&self, window: &Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                if !this.update_in(cx, Self::poll_capture).unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }

    fn poll_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let View::Capture(pane) = &mut self.view else {
            return false;
        };
        let mut changed = false;

        if let Some(rx) = &pane.nic_query_rx {
            match rx.try_recv() {
                Ok(Ok(nics)) => {
                    pane.selected_guid = select_default_nic(&nics, pane.selected_guid.as_deref());
                    pane.nics = nics;
                    pane.nic_loading = false;
                    pane.nic_error = None;
                    pane.nic_query_rx = None;
                    changed = true;
                }
                Ok(Err(error)) => {
                    pane.nic_loading = false;
                    pane.nic_error = Some(error);
                    pane.nic_query_rx = None;
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    pane.nic_loading = false;
                    pane.nic_query_rx = None;
                    changed = true;
                }
            }
        }

        if let Some(rx) = &pane.nic_apply_rx {
            match rx.try_recv() {
                Ok(Ok(outcome)) => {
                    pane.nic_applying = false;
                    pane.nic_apply_rx = None;
                    let verb = if outcome.enabled {
                        "已开启"
                    } else {
                        "已关闭"
                    };
                    pane.nic_feedback = Some(Ok(format!(
                        "{verb}巨帧优化：{} 当前 MTU {}（*JumboPacket={}）。",
                        outcome.label, outcome.nl_mtu, outcome.registry_value
                    )));
                    spawn_nic_query(pane);
                    changed = true;
                }
                Ok(Err(error)) => {
                    pane.nic_applying = false;
                    pane.nic_apply_rx = None;
                    pane.nic_feedback = Some(Err(error));
                    spawn_nic_query(pane);
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    pane.nic_applying = false;
                    pane.nic_apply_rx = None;
                    pane.nic_feedback = Some(Err("网卡巨帧设置线程意外结束。".into()));
                    changed = true;
                }
            }
        }

        let capturing = pane
            .service
            .as_ref()
            .is_some_and(DesktopService::is_capturing);
        if pane.capturing != capturing {
            pane.capturing = capturing;
            changed = true;
        }

        let Some(service) = &mut pane.service else {
            if changed {
                cx.notify();
            }
            return pane.nic_loading || pane.nic_applying;
        };
        match service.poll() {
            None => {
                let status = service.status();
                if pane.status != status {
                    pane.status = status;
                    changed = true;
                }
                if changed {
                    cx.notify();
                }
                true
            }
            Some(Ok(Some(DesktopAction::SelectRole))) => {
                self.reselect(DesktopRole::Capture, window, cx);
                false
            }
            // 关闭请求或 OTA：后台已退出；若在更新，更新器会重开实际角色。
            Some(Ok(_)) => {
                cx.quit();
                false
            }
            Some(Err(error)) => {
                pane.service = None;
                pane.status = DesktopStatus::default();
                pane.capturing = false;
                pane.notice = Some(format!("采集服务未运行：{error}。请检查配置和端口后再试。"));
                cx.notify();
                pane.nic_loading || pane.nic_applying
            }
        }
    }

    fn role_option(role: DesktopRole, cx: &mut Context<Self>) -> Div {
        let (icon, title, detail, action) = match role {
            DesktopRole::Capture => (
                "capture",
                "采集端",
                "截取这台电脑的画面并发送。",
                "选为采集端",
            ),
            DesktopRole::Receive => (
                "monitor",
                "接收端",
                "从另一台电脑接收画面并管理采集。",
                "选为接收端",
            ),
        };
        div()
            .flex_1()
            .min_w_0()
            .rounded(px(8.))
            .p_4()
            .bg(rgb(FIELD))
            .flex()
            .flex_col()
            .items_start()
            .gap_3()
            .child(host_ui::symbol(icon, 23., ACCENT))
            .child(
                div()
                    .text_size(px(18.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(div().flex_1().text_color(rgb(TEXT_DIM)).child(detail))
            .child(
                Button::new(icon)
                    .outline()
                    .primary()
                    .label(action)
                    .on_click(cx.listener(move |app, _, window, cx| app.select(role, window, cx))),
            )
    }

    fn chooser(previous: Option<DesktopRole>, error: Option<&str>, cx: &mut Context<Self>) -> Div {
        let flow = div()
            .w(px(76.))
            .pt(px(16.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_center()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT_DIM))
                    .child("画面传输"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .text_size(px(11.))
                    .text_color(rgb(ACCENT))
                    .child(div().flex_1().h(px(2.)).bg(rgb(ACCENT)))
                    .child("▶"),
            );
        let return_button = previous.map(|role| {
            let label = match role {
                DesktopRole::Capture => "返回采集端",
                DesktopRole::Receive => "返回接收端",
            };
            Button::new("return-to-role").ghost().label(label).on_click(
                cx.listener(move |app, _, window, cx| {
                    app.show(Start::Role(role), None, window, cx)
                }),
            )
        });
        panel()
            .child(heading("这台电脑负责哪一端？"))
            .child(
                div()
                    .flex()
                    .items_stretch()
                    .gap_3()
                    .child(Self::role_option(DesktopRole::Capture, cx))
                    .child(flow)
                    .child(Self::role_option(DesktopRole::Receive, cx)),
            )
            .children(error.map(notice))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .pt_4()
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(TEXT_DIM))
                            .child("选择后自动保存，下次启动直接进入。"),
                    )
                    .children(return_button),
            )
    }

    fn capture(&self, pane: &CapturePane, cx: &mut Context<Self>) -> Div {
        let (status, color) = if pane.service.is_none() {
            ("控制服务未运行", ERR)
        } else if pane.status.busy {
            ("正在等待采集服务退出", TEXT_DIM)
        } else if pane.status.ready {
            ("控制服务已就绪", OK)
        } else {
            ("正在启动控制服务", TEXT_DIM)
        };
        panel()
            .child(heading("采集端"))
            .child(
                div()
                    .bg(rgb(FIELD))
                    .p_4()
                    .rounded(px(8.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_color(rgb(color))
                            .child(div().size(px(7.)).rounded_full().bg(rgb(color)))
                            .child(status),
                    )
                    .child(
                        div()
                            .text_color(rgb(TEXT_DIM))
                            .child(self.options.address()),
                    ),
            )
            .child(
                div()
                    .text_color(rgb(TEXT_DIM))
                    .child("在接收端填写这台电脑的局域网 IP，再从接收端开始或停止采集。"),
            )
            .child(self.capture_jumbo_card(pane, cx))
            .children(
                pane.notice
                    .as_deref()
                    .or(pane.status.error.as_deref())
                    .map(notice),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .pt_4()
                    .child(
                        Button::new("capture-select-role")
                            .outline()
                            .label("重新选择角色")
                            .disabled(
                                pane.nic_applying
                                    || (pane.service.is_some()
                                        && (!pane.status.ready || pane.status.busy)),
                            )
                            .on_click(cx.listener(|app, _, window, cx| {
                                app.request_capture(DesktopAction::SelectRole, window, cx);
                            })),
                    ),
            )
    }

    fn capture_jumbo_card(&self, pane: &CapturePane, cx: &mut Context<Self>) -> Div {
        let selected = selected_nic(pane);
        let checked = selected.is_some_and(NicJumboInfo::is_jumbo_enabled);
        let switch_disabled = pane.nic_loading
            || pane.nic_applying
            || pane.capturing
            || pane.status.busy
            || selected.is_none_or(|nic| !nic.is_supported());
        let state_text = if pane.nic_applying {
            "处理中"
        } else if pane.nic_loading {
            "加载中"
        } else if checked {
            "已开启"
        } else {
            "已关闭"
        };
        let state_color = if pane.nic_applying || pane.nic_loading {
            TEXT_DIM
        } else if checked {
            OK
        } else {
            TEXT_DIM
        };

        let mut card = div()
            .bg(rgb(FIELD))
            .p_4()
            .rounded(px(8.))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child("开启网卡巨帧优化"),
                            )
                            .child(div().text_size(px(12.)).text_color(rgb(TEXT_DIM)).child(
                                "切换本机物理网卡巨帧（9000 / 1500），切换时网卡会短暂重连",
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .flex_shrink_0()
                            .child(
                                Button::new("capture-jumbo-refresh")
                                    .ghost()
                                    .compact()
                                    .label("刷新")
                                    .disabled(pane.nic_loading || pane.nic_applying)
                                    .on_click(
                                        cx.listener(|app, _, _, cx| app.refresh_nic_jumbo(cx)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(state_color))
                                    .child(state_text),
                            )
                            .child(
                                Switch::new("capture-jumbo-switch")
                                    .checked(checked)
                                    .disabled(switch_disabled)
                                    .accessibility_label("开启网卡巨帧优化")
                                    .on_click(cx.listener(|app, next: &bool, _, cx| {
                                        app.toggle_nic_jumbo(*next, cx);
                                    })),
                            ),
                    ),
            );

        if let Some(error) = &pane.nic_error {
            card = card.child(notice(error));
        } else if pane.nics.is_empty() {
            card = card.child(div().text_size(px(12.)).text_color(rgb(TEXT_DIM)).child(
                if pane.nic_loading {
                    "正在读取本机已连接物理网卡…"
                } else {
                    "未检测到已连接的物理网卡，请检查网线或适配器连接后刷新。"
                },
            ));
        } else {
            let mut list = div().flex().flex_col().gap_2();
            let can_select = pane.nics.len() > 1 && !pane.nic_applying;
            for nic in &pane.nics {
                let is_selected = pane.selected_guid.as_deref() == Some(nic.guid.as_str());
                let mtu_label = nic
                    .nl_mtu
                    .map(|mtu| format!("MTU {mtu}"))
                    .unwrap_or_else(|| "MTU 未知".into());
                let detail = if nic.is_supported() {
                    let reg = nic.current_registry_value.as_deref().unwrap_or("未知");
                    if nic.has_mtu_mismatch() {
                        format!("驱动 *JumboPacket={reg}，但 IPv4 MTU 未达到 9000")
                    } else {
                        format!("驱动 *JumboPacket={reg} · 支持 9000 / 1500 一键切换")
                    }
                } else {
                    nic.unsupported_reason
                        .clone()
                        .unwrap_or_else(|| "不支持 9000 巨帧".into())
                };
                let guid = nic.guid.clone();
                // 不添加矩形边框，避免干扰角色页的轮廓按钮识别。
                let mut row = div()
                    .id(SharedString::from(format!("capture-nic:{guid}")))
                    .px_3()
                    .py_2()
                    .rounded(px(6.))
                    .bg(rgb(if is_selected { ACCENT_SOFT } else { SURFACE }))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(13.))
                                    .font_weight(if is_selected {
                                        FontWeight::SEMIBOLD
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .child(nic.display()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(if nic.is_jumbo_enabled() {
                                        OK
                                    } else {
                                        TEXT_DIM
                                    }))
                                    .child(mtu_label),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT_DIM))
                            .child(detail),
                    );
                if can_select {
                    row = row
                        .cursor_pointer()
                        .on_click(cx.listener(move |app, _, _, cx| {
                            app.select_nic_jumbo(guid.clone(), cx);
                        }));
                }
                list = list.child(row);
            }
            card = card.child(list);
            if pane.nics.len() > 1 && pane.selected_guid.is_none() {
                card = card.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(TEXT_DIM))
                        .child("检测到多张已连接物理网卡，请先点击选择要优化的网卡。"),
                );
            }
        }

        if pane.capturing {
            card = card.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT_DIM))
                    .child("当前正在采集或待清理，请先在接收端停止后再切换巨帧。"),
            );
        }
        if pane.nic_applying {
            card = card.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(ACCENT))
                    .child("正在应用网卡巨帧设置（可能需要管理员授权并等待网卡重连）…"),
            );
        }
        if let Some(feedback) = &pane.nic_feedback {
            let (color, text) = match feedback {
                Ok(message) => (OK, message.as_str()),
                Err(error) => (ERR, error.as_str()),
            };
            card = card.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(color))
                    .child(text.to_owned()),
            );
        }
        card
    }
}

fn spawn_nic_query(pane: &mut CapturePane) {
    let (tx, rx) = mpsc::channel();
    match std::thread::Builder::new()
        .name("vc-nic-jumbo-query".into())
        .spawn(move || {
            let _ = tx.send(nic_jumbo::list_physical_nic_jumbo());
        }) {
        Ok(_) => {
            pane.nic_loading = true;
            pane.nic_query_rx = Some(rx);
        }
        Err(error) => {
            pane.nic_loading = false;
            pane.nic_error = Some(format!("无法启动网卡查询线程：{error}"));
        }
    }
}

fn selected_nic(pane: &CapturePane) -> Option<&NicJumboInfo> {
    let guid = pane.selected_guid.as_deref()?;
    pane.nics.iter().find(|nic| nic.guid == guid)
}

/// 单张物理网卡时直接选中；多张时只按用户保存的 `adapter` 唯一匹配，不按厂商偏置。
fn select_default_nic(nics: &[NicJumboInfo], previous: Option<&str>) -> Option<String> {
    if let Some(prev) = previous
        && nics.iter().any(|nic| nic.guid == prev)
    {
        return Some(prev.to_string());
    }
    if nics.len() == 1 {
        return Some(nics[0].guid.clone());
    }
    let saved = Config::load_root().ok()?.adapter;
    let saved = saved.trim();
    if saved.is_empty() {
        return None;
    }
    let lower = saved.to_ascii_lowercase();
    if let Some(by_guid) = nics.iter().find(|nic| lower.contains(&nic.guid)) {
        return Some(by_guid.guid.clone());
    }
    let mut matches = nics.iter().filter(|nic| {
        (!nic.label.is_empty() && nic.label.eq_ignore_ascii_case(saved))
            || (!nic.description.is_empty() && nic.description.eq_ignore_ascii_case(saved))
    });
    let first = matches.next()?;
    matches.next().is_none().then(|| first.guid.clone())
}

fn panel() -> Div {
    div()
        .w_full()
        .max_w(px(620.))
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(BORDER))
        .rounded(px(12.))
        .p(px(28.))
        .flex()
        .flex_col()
        .gap_5()
}

fn heading(text: &'static str) -> Div {
    div()
        .text_size(px(25.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(text)
}

fn notice(text: &str) -> Div {
    div()
        .text_color(rgb(ERR))
        .text_size(px(13.))
        .child(text.to_owned())
}

impl Render for DesktopApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = match &self.view {
            View::Receive { workbench, .. } => return workbench.clone().into_any_element(),
            View::Choose { previous, error } => Self::chooser(*previous, error.as_deref(), cx),
            View::Capture(pane) => self.capture(pane, cx),
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_size(px(14.))
            .font(Font {
                family: "Segoe UI".into(),
                fallbacks: Some(FontFallbacks::from_fonts(vec!["Microsoft YaHei UI".into()])),
                ..Default::default()
            })
            .child(pane)
            .into_any_element()
    }
}

pub(crate) fn run(start: Start, options: Options, ready: Option<PathBuf>) {
    gpui_kit::application()
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(move |cx| {
            gpui_kit::init(cx);
            host_ui::init_theme(cx);
            let options_window = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Default::default(),
                    size: size(px(1280.), px(900.)),
                })),
                window_min_size: Some(size(px(1080.), px(740.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("VirtualCapture".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options_window, cx, move |window, cx| {
                cx.new(|cx| {
                    let mut app = DesktopApp {
                        view: View::Choose {
                            previous: None,
                            error: None,
                        },
                        options,
                    };
                    app.show(start, ready, window, cx);
                    let weak = cx.weak_entity();
                    window.on_window_should_close(cx, move |window, cx| {
                        weak.update(cx, |app, cx| {
                            app.request_capture(DesktopAction::Exit, window, cx)
                        })
                        .unwrap_or(true)
                    });
                    app
                })
            })
            .expect("无法打开主窗口");
        });
}
