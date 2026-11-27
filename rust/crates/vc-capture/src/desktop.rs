//! 图形采集端的本机生命周期句柄，不增加 REST 停机/切角色接口。

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::{Options, state::Agent};
use tokio::sync::watch;

type Target = (Arc<Mutex<Agent>>, watch::Sender<bool>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopAction {
    SelectRole,
    Exit,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct DesktopStatus {
    pub ready: bool,
    pub busy: bool,
    pub error: Option<String>,
}

#[derive(Default)]
struct State {
    status: DesktopStatus,
    target: Option<Target>,
    action: Option<DesktopAction>,
}

#[derive(Clone, Default)]
pub(crate) struct Control(Arc<Mutex<State>>);

impl Control {
    pub(crate) fn ready(&self, agent: Arc<Mutex<Agent>>, shutdown: watch::Sender<bool>) {
        let mut state = self.0.lock().expect("图形采集状态锁不可用");
        state.target = Some((agent, shutdown));
        state.status.ready = true;
    }
}

pub struct DesktopService {
    control: Control,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl DesktopService {
    pub fn start(options: Options) -> std::io::Result<Self> {
        let control = Control::default();
        let worker = control.clone();
        let thread = std::thread::Builder::new()
            .name("vc-capture-service".into())
            .spawn(move || crate::run_desktop(options, worker).map_err(|e| format!("{e:#}")))?;
        Ok(Self {
            control,
            thread: Some(thread),
        })
    }

    pub fn status(&self) -> DesktopStatus {
        self.control
            .0
            .lock()
            .map(|state| state.status.clone())
            .unwrap_or_else(|_| DesktopStatus {
                busy: true,
                error: Some("图形采集状态锁不可用，未切换角色。".into()),
                ..Default::default()
            })
    }

    /// 检查在独立线程执行；GPU/REST 正忙时不阻塞窗口，也不强行停止会话。
    pub fn request(&self, action: DesktopAction) -> Result<(), String> {
        let target = {
            let mut state = self.control.0.lock().map_err(|_| "图形采集状态锁不可用")?;
            if state.status.busy {
                return Err("正在等待后台退出，请稍候。".into());
            }
            let target = state.target.clone().ok_or("采集后台尚未就绪，请稍候。")?;
            state.status.busy = true;
            state.status.error = None;
            target
        };
        let control = self.control.clone();
        let result = std::thread::Builder::new()
            .name("vc-capture-idle-check".into())
            .spawn(move || {
                let (agent, shutdown) = target;
                let result = (|| {
                    let mut agent = agent
                        .try_lock()
                        .map_err(|_| "后台正在处理请求，请稍后重试。")?;
                    require_idle(agent.running(), agent.shutting_down)?;
                    // 与 Start/OTA 使用同一把锁封口，确认后不再接受新会话。
                    agent.shutting_down = true;
                    let mut state = control.0.lock().map_err(|_| "图形采集状态锁不可用")?;
                    state.action = Some(action);
                    if shutdown.send(true).is_err() {
                        agent.shutting_down = false;
                        state.action = None;
                        return Err("采集后台退出通道已关闭。".to_owned());
                    }
                    Ok(())
                })();
                if let Err(error) = result
                    && let Ok(mut state) = control.0.lock()
                {
                    state.status.busy = false;
                    state.status.error = Some(error);
                }
            });
        if let Err(error) = result {
            if let Ok(mut state) = self.control.0.lock() {
                state.status.busy = false;
            }
            return Err(error.to_string());
        }
        Ok(())
    }

    /// 只 join 已结束线程；None 的成功原因代表 OTA 请求退出。
    pub fn poll(&mut self) -> Option<Result<Option<DesktopAction>, String>> {
        if !self.thread.as_ref()?.is_finished() {
            return None;
        }
        let result = self
            .thread
            .take()?
            .join()
            .unwrap_or_else(|_| Err("采集后台线程异常退出。".into()));
        let action = self.control.0.lock().ok().and_then(|state| state.action);
        Some(result.map(|()| action))
    }

    /// 非阻塞检查采集后台当前是否处于运行、待清理或退出中，供 UI 禁用网卡切换。
    pub fn is_capturing(&self) -> bool {
        let Ok(state) = self.control.0.try_lock() else {
            return true;
        };
        if state.status.busy {
            return true;
        }
        let Some((agent, _)) = &state.target else {
            return false;
        };
        let Ok(mut agent) = agent.try_lock() else {
            return true;
        };
        agent.running() || agent.shutting_down
    }

    /// 返回可在后台线程中使用的采集空闲检查与缓存刷新句柄。
    pub fn nic_guard(&self) -> CaptureNicGuard {
        CaptureNicGuard(self.control.clone())
    }
}

/// 供采集端修改网卡巨帧前检查会话空闲，并在完成后使 `/adapters` 缓存失效。
#[derive(Clone)]
pub struct CaptureNicGuard(Control);

impl CaptureNicGuard {
    pub fn ensure_idle(&self) -> Result<(), String> {
        let target = {
            let state = self.0.0.lock().map_err(|_| "图形采集状态锁不可用")?;
            if state.status.busy {
                return Err("正在等待采集后台退出，请稍后再调整网卡巨帧。".into());
            }
            state.target.clone()
        };
        if let Some((agent, _)) = target {
            let mut agent = agent
                .try_lock()
                .map_err(|_| "采集后台正在处理请求，请稍后再试。".to_string())?;
            require_idle_for_nic(agent.running(), agent.shutting_down)?;
        }
        Ok(())
    }

    pub fn invalidate_adapters_cache(&self) {
        let target = self.0.0.lock().ok().and_then(|state| state.target.clone());
        if let Some((agent, _)) = target
            && let Ok(mut agent) = agent.lock()
        {
            agent.invalidate_adapters_cache();
        }
    }
}

fn require_idle_for_nic(running_or_pending: bool, shutting_down: bool) -> Result<(), String> {
    if running_or_pending {
        return Err("采集正在运行或待清理，请先在接收端停止后再调整网卡巨帧。".into());
    }
    if shutting_down {
        return Err("采集后台正在更新或退出，不能修改网卡巨帧。".into());
    }
    Ok(())
}

fn require_idle(running_or_pending: bool, shutting_down: bool) -> Result<(), String> {
    if running_or_pending {
        return Err("采集正在运行或仍待清理，请先在接收端停止，再重新选择角色或退出。".into());
    }
    if shutting_down {
        return Err("采集后台正在更新或退出，不能切换角色。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_switch_rejects_running_pending_and_update() {
        assert!(require_idle(true, false).is_err());
        assert!(require_idle(false, true).is_err());
        assert!(require_idle(true, true).is_err());
        assert!(require_idle(false, false).is_ok());
    }

    #[test]
    fn nic_jumbo_guard_rejects_running_pending_and_shutdown() {
        assert!(require_idle_for_nic(true, false).is_err());
        assert!(require_idle_for_nic(false, true).is_err());
        assert!(require_idle_for_nic(true, true).is_err());
        assert!(require_idle_for_nic(false, false).is_ok());
    }
}
