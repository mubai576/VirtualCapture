//! PyroWave 同步调用看门狗。
//!
//! 延续历史同步看门狗的操作边界：创建、预热、编码/取包、解码、
//! 重置和销毁各覆盖一次同步操作，单次操作 5 秒未返回即视为超时。
//!
//! 与 C++ 版本的关键差异：Rust 数据面运行在产品宿主进程/DLL 内，不能再像独立
//! sender/receiver 子进程那样用 `process::exit(124)` 收尸。挂起的 Vulkan/驱动调用
//! 无法安全展开栈、销毁设备或 join 被阻塞的调用线程，因此看门狗只做两件事：
//! 1. 把当前会话的 [`OperationState`] 置为粘性超时（供数据面会话/宿主观察）；
//! 2. 结束自己的监视线程，绝不终止进程、强制展开或触碰原生线程。
//!
//! 挂起调用如果之后返回，编解码入口会用 [`OperationDeadline::check`] 以粘性超时
//! 错误收尾；之后的原生调用一律被拒绝。会话由父层保持工作线程与 DLL 句柄存活
//! （不 join 挂起线程，也不销毁可能永久挂起的设备），不使用 `process::exit`。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

/// 单次同步编解码操作上限。
const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);

/// 单个编码器/解码器会话的同步操作状态。
///
/// 数据面会话在创建编解码器之前克隆该句柄交给后端（或创建后通过
/// `Encoder::operation_state`/`Decoder::operation_state` 获取），即可从其他线程
/// 观察 PyroWave 原生调用是否卡死。超时是粘性的：一旦置位不再清除，且只影响
/// 当前会话——不存在跨会话或进程级全局标志。
#[derive(Clone, Debug, Default)]
pub struct OperationState {
    inner: Arc<OperationStateInner>,
}

#[derive(Debug, Default)]
struct OperationStateInner {
    timed_out: AtomicBool,
    operation: Mutex<Option<&'static str>>,
}

impl OperationState {
    /// 新建一个未超时的会话状态。
    pub fn new() -> Self {
        Self::default()
    }

    /// 该会话是否已发生同步操作超时（粘性，置位后不再清除）。
    pub fn timed_out(&self) -> bool {
        self.inner.timed_out.load(Ordering::Acquire)
    }

    /// 首次超时的操作名（“创建”“预热”“编码”“解码”“重置”“销毁”）；未超时为 `None`。
    pub fn timed_out_operation(&self) -> Option<&'static str> {
        if !self.timed_out() {
            return None;
        }
        // 操作名在置位标志之前写入，观察到 `true` 后必然可见。
        *lock(&self.inner.operation)
    }

    fn mark_timed_out(&self, operation: &'static str) {
        {
            let mut current = lock(&self.inner.operation);
            if current.is_none() {
                *current = Some(operation);
            }
        }
        // Release 与 `timed_out` 的 Acquire 配对：先写操作名，再置粘性标志。
        self.inner.timed_out.store(true, Ordering::Release);
    }
}

struct State {
    generation: u64,
    deadline: Option<Instant>,
    operation: &'static str,
    stopping: bool,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
    timeout: Duration,
    observer: OperationState,
}

/// 每个 PyroWave 编码器/解码器各持有一个看门狗线程；构造失败视为打开失败。
pub(super) struct OperationDeadline {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

/// 在一次同步 FFI 调用期间保持计时；丢弃即解除。
///
/// 只持有共享状态的 `Arc`，不借用 [`OperationDeadline`]，因此可以在同一次调用中
/// 继续可变借用编/解码器本体。
pub(super) struct Armed {
    shared: Arc<Shared>,
}

impl OperationDeadline {
    /// 以调用方提供的会话观察句柄启动看门狗（创建前即可观察，创建超时也可见）。
    pub(super) fn start_observed(observer: &OperationState) -> Result<Self> {
        Self::start_with(OPERATION_TIMEOUT, observer.clone())
    }

    fn start_with(timeout: Duration, observer: OperationState) -> Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                generation: 0,
                deadline: None,
                operation: "同步调用",
                stopping: false,
            }),
            cv: Condvar::new(),
            timeout,
            observer,
        });
        let watched = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("vc-media-pyrowave-watchdog".into())
            .spawn(move || watch(watched))
            .context("无法启动 PyroWave 看门狗线程")?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    /// 开始一次同步原生调用。会话已超时时直接拒绝，不再触碰原生后端。
    pub(super) fn begin(&mut self, operation: &'static str) -> Result<Armed> {
        self.check()?;
        {
            let mut state = lock(&self.shared.state);
            state.deadline = Some(Instant::now() + self.shared.timeout);
            state.operation = operation;
            state.generation = state.generation.wrapping_add(1);
            self.shared.cv.notify_all();
        }
        Ok(Armed {
            shared: Arc::clone(&self.shared),
        })
    }

    /// 调用返回后检查：调用期间发生的超时以粘性错误覆盖原生返回值。
    pub(super) fn check(&self) -> Result<()> {
        if self.shared.observer.timed_out() {
            return Err(self.timeout_error());
        }
        Ok(())
    }

    /// 该会话是否已粘性超时。
    pub(super) fn timed_out(&self) -> bool {
        self.shared.observer.timed_out()
    }

    fn timeout_error(&self) -> anyhow::Error {
        let operation = self
            .shared
            .observer
            .timed_out_operation()
            .unwrap_or("同步调用");
        anyhow!(
            "PyroWave {operation}超时：超过 {:?} 未返回；看门狗只设置会话粘性标志，未终止进程；本会话后续原生调用一律拒绝",
            self.shared.timeout
        )
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        let mut state = lock(&self.shared.state);
        state.deadline = None;
        state.generation = state.generation.wrapping_add(1);
        self.shared.cv.notify_all();
    }
}

impl Drop for OperationDeadline {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.shared.state);
            state.stopping = true;
            state.deadline = None;
            self.shared.cv.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            // 只 join 本结构创建的看门狗线程：它只等 Condvar、从不调用原生代码，
            // 必然有界返回。卡在 Vulkan/驱动里的原生调用线程不属于这里，
            // 父层保持其存活，绝不在 API 停止路径上 join 它。
            let _ = worker.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn watch(shared: Arc<Shared>) {
    let mut state = lock(&shared.state);
    loop {
        if state.stopping {
            return;
        }
        let Some(deadline) = state.deadline else {
            state = shared
                .cv
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            continue;
        };
        let generation = state.generation;
        let now = Instant::now();
        if now >= deadline {
            // 记录粘性超时后退出：不终止进程、不展开栈、不触碰原生线程。
            record_timeout(&shared, &mut state);
            return;
        }
        let (next, timed_out) = shared
            .cv
            .wait_timeout_while(state, deadline - now, |state| {
                // 解除（deadline=None/代次变化）或停止时不触发。
                !state.stopping && state.deadline.is_some() && state.generation == generation
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next;
        if timed_out.timed_out()
            && !state.stopping
            && state.deadline.is_some()
            && state.generation == generation
        {
            record_timeout(&shared, &mut state);
            return;
        }
    }
}

fn record_timeout(shared: &Shared, state: &mut State) {
    shared.observer.mark_timed_out(state.operation);
    state.deadline = None;
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn start_for_test(timeout: Duration) -> (OperationDeadline, OperationState) {
        let observer = OperationState::new();
        let deadline = OperationDeadline::start_with(timeout, observer.clone()).unwrap();
        (deadline, observer)
    }

    fn wait_timed_out(state: &OperationState) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            if state.timed_out() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        state.timed_out()
    }

    #[test]
    fn timeout_is_sticky_reported_and_never_exits() {
        let (mut deadline, observer) = start_for_test(Duration::from_millis(20));
        let armed = deadline.begin("测试操作").unwrap();
        // 进程仍然存活（测试能继续运行）且观察句柄在期限内收到超时。
        assert!(wait_timed_out(&observer), "看门狗未在期限内记录超时");
        assert_eq!(observer.timed_out_operation(), Some("测试操作"));

        drop(armed);
        assert!(observer.timed_out(), "解除武装不得清除粘性超时");
        assert!(deadline.timed_out());
        let error = deadline.check().unwrap_err().to_string();
        assert!(error.contains("超时"), "错误应说明超时：{error}");
        assert!(error.contains("测试操作"), "错误应包含操作名：{error}");
        assert!(
            deadline.begin("编码").is_err(),
            "超时后必须拒绝新的原生操作"
        );

        // 看门狗记录超时后自行退出，销毁看门狗不得阻塞（不 join 挂起线程）。
        let dropped = Instant::now();
        drop(deadline);
        assert!(
            dropped.elapsed() < Duration::from_secs(2),
            "看门狗销毁不应阻塞"
        );
    }

    #[test]
    fn disarm_before_deadline_stays_clean() {
        let (mut deadline, observer) = start_for_test(Duration::from_millis(80));
        {
            let _armed = deadline.begin("编码").unwrap();
        }
        thread::sleep(Duration::from_millis(200));
        assert!(!observer.timed_out(), "按期解除的操作不得记录超时");
        assert!(deadline.check().is_ok());
        assert_eq!(observer.timed_out_operation(), None);
    }

    #[test]
    fn normal_operations_keep_rearming() {
        let (mut deadline, observer) = start_for_test(Duration::from_millis(80));
        for _ in 0..8 {
            let armed = deadline.begin("编码").unwrap();
            drop(armed);
            assert!(deadline.check().is_ok());
        }
        assert!(!observer.timed_out());
    }

    #[test]
    fn instances_are_isolated() {
        let (mut first, first_observer) = start_for_test(Duration::from_millis(20));
        let (mut second, second_observer) = start_for_test(Duration::from_secs(1));

        let first_armed = first.begin("编码").unwrap();
        let second_armed = second.begin("解码").unwrap();
        assert!(wait_timed_out(&first_observer), "第一个会话应超时");
        drop(first_armed);
        drop(second_armed);

        assert!(first.check().is_err());
        assert!(
            !second_observer.timed_out(),
            "一个会话超时不得影响另一个会话"
        );
        assert!(second.check().is_ok());
        assert_eq!(second_observer.timed_out_operation(), None);
    }
}
