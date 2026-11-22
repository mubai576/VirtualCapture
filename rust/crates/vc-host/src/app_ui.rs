//! 原生桌面入口。两个角色共用窗口，确认前不写启动偏好、不启动任何角色。

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::Disableable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::prelude::*;
use gpui_kit::{
    Bounds, Context, Div, Entity, Font, FontFallbacks, FontWeight, QuitMode, Render, Subscription,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};
use vc_capture::{DesktopAction, DesktopService, DesktopStatus, Options};
use vc_core::startup::{self, DesktopRole};

use crate::host_ui::{self, ACCENT, BG, BORDER, ERR, FIELD, HostApp, OK, SURFACE, TEXT, TEXT_DIM};

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
                if service.is_some() {
                    self.watch_capture(window, cx);
                }
                View::Capture(CapturePane {
                    service,
                    status: DesktopStatus::default(),
                    notice,
                })
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
        let Some(service) = &mut pane.service else {
            return false;
        };
        match service.poll() {
            None => {
                let status = service.status();
                if pane.status != status {
                    pane.status = status;
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
                pane.notice = Some(format!("采集服务未运行：{error}。请检查配置和端口后再试。"));
                cx.notify();
                false
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
                                pane.service.is_some() && (!pane.status.ready || pane.status.busy),
                            )
                            .on_click(cx.listener(|app, _, window, cx| {
                                app.request_capture(DesktopAction::SelectRole, window, cx);
                            })),
                    ),
            )
    }
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
