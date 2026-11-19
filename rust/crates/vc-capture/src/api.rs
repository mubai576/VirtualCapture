//! Rust REST v1；阻塞的原生探测/进程操作在 blocking pool 执行。
//!
//! `/update/*` 由本地更新令牌保护；`/health` 等只读/控制接口保持协议不变。
use crate::state::Agent;
use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Query, Request, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;
use vc_core::{
    paths,
    session::{self, SenderRequest},
    update::{self, ActivationRequest, Role},
};

type Shared = Arc<Mutex<Agent>>;
type Reply = (StatusCode, Json<Value>);

#[derive(Clone)]
pub struct ServerInfo {
    pub bind: String,
    pub port: u16,
}

#[derive(Clone)]
struct AppState {
    agent: Shared,
    server: ServerInfo,
    shutdown: watch::Sender<bool>,
}

/// 上传临时文件的兜底清理：无论处理器从哪条路径返回，文件句柄先析构、再删除。
struct UploadGuard {
    path: std::path::PathBuf,
}

impl UploadGuard {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for UploadGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn error(code: StatusCode, message: impl Into<String>) -> Reply {
    (code, Json(json!({"error":message.into()})))
}

fn authorized(headers: &HeaderMap) -> bool {
    let Some(expected) = update::peek_token() else {
        return false;
    };
    let supplied = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim();
    update::token_matches(&expected, supplied)
}

fn unauthorized() -> Reply {
    error(StatusCode::UNAUTHORIZED, "需要更新令牌")
}

async fn blocking(agent: Shared, task: impl FnOnce(&mut Agent) -> Reply + Send + 'static) -> Reply {
    tokio::task::spawn_blocking(move || match agent.lock() {
        Ok(mut agent) => task(&mut agent),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "状态锁不可用"),
    })
    .await
    .unwrap_or_else(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub fn router(agent: Shared, server: ServerInfo, shutdown: watch::Sender<bool>) -> Router {
    let state = AppState {
        agent,
        server,
        shutdown,
    };
    // 只在 OTA 路由放宽请求体上限，避免未鉴权的 /config 等接口接受超大请求。
    let updates = Router::new()
        .route("/update/status", get(update_status))
        .route("/update/stage", post(update_stage))
        .route("/update/activate", post(update_activate))
        .route("/update/rollback", post(update_rollback))
        .layer(DefaultBodyLimit::max(
            (update::MAX_UPLOAD_BYTES + 1024 * 1024) as usize,
        ));
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/config", get(config).post(save_config))
        .route("/adapters", get(adapters))
        .route("/sender/prepare", post(prepare))
        .route("/sender/start", post(start))
        .route("/sender/stop", post(stop))
        .route("/sender/stats", get(stats))
        .merge(updates)
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Reply {
    let running = match state.agent.lock() {
        Ok(mut agent) => agent.running(),
        Err(_) => false,
    };
    let build = paths::current_build_id();
    let layout = match paths::layout() {
        paths::Layout::Managed => "managed",
        paths::Layout::Legacy => "legacy",
    };
    (
        StatusCode::OK,
        Json(json!({"ok":true,"implementation":session::IMPLEMENTATION,
            "api_revision":session::API_REVISION,"version":env!("CARGO_PKG_VERSION"),"role":"capture",
            "running":running,"pid":std::process::id(),"root":paths::root(),"session_id":null,
            "build":build,"layout":layout})),
    )
}

async fn version() -> Reply {
    (
        StatusCode::OK,
        Json(
            json!({"version":env!("CARGO_PKG_VERSION"),"api_revision":session::API_REVISION,"implementation":session::IMPLEMENTATION}),
        ),
    )
}

async fn config(State(state): State<AppState>) -> Reply {
    blocking(state.agent, |agent| {
        (StatusCode::OK, Json(json!(agent.config)))
    })
    .await
}

async fn save_config(State(state): State<AppState>, Json(patch): Json<Value>) -> Reply {
    blocking(state.agent, move |agent| {
        if agent.running() || agent.shutting_down {
            return error(StatusCode::CONFLICT, "采集运行中或正在退出，拒绝修改配置。");
        }
        let mut config = agent.config.clone();
        let errors = config.apply_patch(&patch);
        if !errors.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"配置校验失败","details":errors})),
            );
        }
        if let Err(e) = config.save_root() {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("配置保存失败：{e}"),
            );
        }
        agent.config = config;
        (
            StatusCode::OK,
            Json(json!({"saved":true,"config":agent.config})),
        )
    })
    .await
}

#[derive(serde::Deserialize)]
struct AdapterQuery {
    #[serde(default)]
    refresh: bool,
}

async fn adapters(State(state): State<AppState>, Query(query): Query<AdapterQuery>) -> Reply {
    blocking(state.agent, move |agent| {
        match agent.adapters(query.refresh) {
            Ok(items) => (StatusCode::OK, Json(json!({"items":items}))),
            Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, e),
        }
    })
    .await
}

async fn prepare(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(request): Json<SenderRequest>,
) -> Reply {
    blocking(state.agent, move |agent| {
        match agent.prepare(&request, &peer.ip().to_string()) {
            Ok(prepared) => (StatusCode::OK, Json(json!(prepared))),
            Err(e) => error(StatusCode::CONFLICT, e),
        }
    })
    .await
}

async fn start(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(request): Json<SenderRequest>,
) -> Reply {
    blocking(state.agent, move |agent| {
        match agent.start(request, &peer.ip().to_string()) {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(e) => error(StatusCode::CONFLICT, e),
        }
    })
    .await
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StopRequest {
    session_id: String,
}

async fn stop(State(state): State<AppState>, Json(request): Json<StopRequest>) -> Reply {
    blocking(state.agent, move |agent| {
        match agent.stop(Some(&request.session_id)) {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(e) => error(StatusCode::CONFLICT, e),
        }
    })
    .await
}

async fn stats(State(state): State<AppState>) -> Reply {
    blocking(state.agent, |agent| (StatusCode::OK, Json(agent.stats()))).await
}

// ---------------------------------------------------------------- OTA

async fn update_status(State(state): State<AppState>, headers: HeaderMap) -> Reply {
    if !authorized(&headers) {
        return unauthorized();
    }
    let running = match state.agent.lock() {
        Ok(mut agent) => agent.running(),
        Err(_) => false,
    };
    let root = paths::root();
    match tokio::task::spawn_blocking(move || update::status_at(&root)).await {
        Ok(Ok(mut value)) => {
            value["running"] = json!(running);
            (StatusCode::OK, Json(value))
        }
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn update_stage(
    State(_state): State<AppState>,
    headers: HeaderMap,
    request: Request,
) -> Reply {
    if !authorized(&headers) {
        return unauthorized();
    }
    let Some(expected) = headers
        .get("x-vc-sha256")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| value.len() == 64)
    else {
        return error(StatusCode::BAD_REQUEST, "缺少有效的 X-VC-SHA256 头。");
    };
    let incoming = paths::incoming_dir();
    if let Err(e) = std::fs::create_dir_all(&incoming) {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("无法创建上传目录：{e}"),
        );
    }
    let upload = incoming.join(format!("upload-{}.zip", update::random_hex(8)));
    // 必须先于 file 声明：作用域退出时 file 先析构，Drop 再删文件（Windows 打开时不可删）。
    let _upload_guard = UploadGuard::new(upload.clone());
    let mut file = match tokio::fs::File::create(&upload).await {
        Ok(file) => file,
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("无法写入上传文件：{e}"),
            );
        }
    };
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut body = request.into_body();
    loop {
        let frame = match body.frame().await {
            Some(Ok(frame)) => frame,
            Some(Err(e)) => {
                return error(StatusCode::BAD_REQUEST, format!("上传中断：{e}"));
            }
            None => break,
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        total += data.len() as u64;
        if total > update::MAX_UPLOAD_BYTES {
            return error(StatusCode::PAYLOAD_TOO_LARGE, "上传包超过大小上限。");
        }
        hasher.update(&data);
        if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut file, &data).await {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("写入上传文件失败：{e}"),
            );
        }
    }
    if let Err(e) = file.sync_all().await {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("刷新上传文件失败：{e}"),
        );
    }
    drop(file);
    let digest = hex::encode(hasher.finalize());
    if !digest.eq_ignore_ascii_case(expected) {
        return error(StatusCode::BAD_REQUEST, "上传内容 SHA-256 与请求头不一致。");
    }
    let root = paths::root();
    let path = upload.clone();
    let result = tokio::task::spawn_blocking(move || update::stage_file(&root, &path)).await;
    match result {
        Ok(Ok(meta)) => (
            StatusCode::OK,
            Json(json!({"ok":true,"staged":meta,"restart_required":true})),
        ),
        Ok(Err(e)) => error(StatusCode::BAD_REQUEST, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetRequest {
    build_id: String,
}

async fn begin_activation(state: &AppState, target: String) -> Reply {
    {
        let Ok(mut agent) = state.agent.lock() else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "状态锁不可用");
        };
        if agent.running() {
            return error(StatusCode::CONFLICT, "采集运行中；先停止采集再激活更新。");
        }
        if agent.shutting_down {
            return error(StatusCode::CONFLICT, "后台正在退出。");
        }
        agent.shutting_down = true;
    }
    let root = paths::root();
    let request = ActivationRequest {
        role: Role::Capture,
        target: target.clone(),
        parent_pid: std::process::id(),
        port: state.server.port,
        bind: state.server.bind.clone(),
        wait_ms: 120_000,
        ready_ms: 30_000,
        require_ready: false,
    };
    let prepared =
        tokio::task::spawn_blocking(move || update::prepare_activation_at(&root, request)).await;
    match prepared {
        Ok(Ok(activation)) => {
            let shutdown = state.shutdown.clone();
            // 先让 202 响应完成，再优雅关闭 HTTP 服务；helper 等本进程退出后切换版本。
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let _ = shutdown.send(true);
            });
            (
                StatusCode::ACCEPTED,
                Json(json!({"ok":true,"activating":{"build_id":target,
                    "helper_pid":activation.helper_pid,"log":activation.log_path}})),
            )
        }
        Ok(Err(e)) => {
            if let Ok(mut agent) = state.agent.lock() {
                agent.shutting_down = false;
            }
            error(StatusCode::CONFLICT, e)
        }
        Err(e) => {
            if let Ok(mut agent) = state.agent.lock() {
                agent.shutting_down = false;
            }
            error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
    }
}

async fn update_activate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TargetRequest>,
) -> Reply {
    if !authorized(&headers) {
        return unauthorized();
    }
    begin_activation(&state, request.build_id).await
}

async fn update_rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<TargetRequest>,
) -> Reply {
    if !authorized(&headers) {
        return unauthorized();
    }
    let target = request.build_id;
    let root = paths::root();
    let check = target.clone();
    match tokio::task::spawn_blocking(move || update::verify_release_at(&root, &check)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return error(StatusCode::CONFLICT, e),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    begin_activation(&state, target).await
}
