//! 被采集机 `vc-capture` REST 客户端（主机使用）。阻塞式，供后台工作线程调用。
//!
//! 所有请求都有全局超时；响应体有大小上限。非 2xx 必须读取并解析错误体，
//! 不能把对端的具体校验原因丢掉。

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::adapters::AdapterInfo;

/// 单次控制请求的总超时（连接 + 发送 + 读取）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// 响应体上限，避免异常/恶意对端耗尽主机内存。
const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;
/// 非 JSON 错误体在消息里最多保留的字符数。
const ERROR_BODY_CHARS: usize = 512;

#[derive(Clone)]
pub struct PeerClient {
    base: String,
    agent: ureq::Agent,
    update_token: Option<String>,
}

impl PeerClient {
    pub fn new(base: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            // 自行处理非 2xx，才能读到对端返回的 {"error","details"}。
            .http_status_as_error(false)
            .build();
        Self {
            base: base.trim().trim_end_matches('/').to_string(),
            agent: config.into(),
            update_token: None,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// 更新令牌仅用于更新端点，不随健康/配置请求发送。
    pub fn with_update_token(mut self, token: Option<String>) -> Self {
        self.update_token = token;
        self
    }

    fn update_authorization(&self) -> Result<String, String> {
        let token = self
            .update_token
            .as_deref()
            .ok_or("请先配对被采集机的更新令牌。")?;
        if !crate::update::valid_token(token) {
            return Err("更新令牌必须是64位十六进制字符。".into());
        }
        Ok(format!("Bearer {token}"))
    }

    fn request(&self, path: &str, body: Option<&Value>) -> Result<Value, String> {
        if self.base.is_empty() {
            return Err(format!("{path} 请求失败：未配置被采集机地址。"));
        }
        let url = format!("{}{path}", self.base);
        // Fresh NIC enumeration can take 10s native + 15s PowerShell, plus capability
        // probing. Keep ordinary health/stats responsive, but let preflight finish.
        let timeout = if matches!(
            path,
            "/adapters" | "/adapters?refresh=true" | "/sender/prepare" | "/sender/start"
        ) {
            Duration::from_secs(40)
        } else {
            REQUEST_TIMEOUT
        };
        let authorization = if path.starts_with("/update/") {
            Some(self.update_authorization()?)
        } else {
            None
        };
        let result = match body {
            Some(body) => {
                let mut request = self
                    .agent
                    .post(&url)
                    .config()
                    .timeout_global(Some(timeout))
                    .build();
                if let Some(value) = &authorization {
                    request = request.header("Authorization", value);
                }
                request.send_json(body)
            }
            None => {
                let mut request = self
                    .agent
                    .get(&url)
                    .config()
                    .timeout_global(Some(timeout))
                    .build();
                if let Some(value) = &authorization {
                    request = request.header("Authorization", value);
                }
                request.call()
            }
        };
        let mut response = result.map_err(|error| match error {
            ureq::Error::Timeout(_) => format!(
                "{path} 请求超时（{} 秒）；对端可能离线或网卡无响应。",
                timeout.as_secs()
            ),
            other => format!("{path} 请求失败：{other}"),
        })?;
        let status = response.status();
        let code = status.as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_string()
            .map_err(|error| format!("{path} 读取响应失败（HTTP {code}）：{error}"))?;
        if !status.is_success() {
            return Err(format_http_error(path, code, &text));
        }
        let text = text.trim();
        if text.is_empty() {
            return Err(format!("{path} 响应为空（HTTP {code}），预期 JSON。"));
        }
        serde_json::from_str(text)
            .map_err(|error| format!("{path} 响应不是 JSON（HTTP {code}）：{error}"))
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        self.request(path, None)
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        self.request(path, Some(body))
    }

    pub fn health(&self) -> Result<Value, String> {
        self.get("/health")
    }

    pub fn version(&self) -> Result<String, String> {
        let value = self.get("/version")?;
        value
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "/version 响应缺少 version 字符串。".to_string())
    }

    pub fn config(&self) -> Result<Value, String> {
        self.get("/config")
    }

    pub fn save_config(&self, patch: &Value) -> Result<Value, String> {
        self.post("/config", patch)
    }

    /// 读取网卡列表；`refresh=true` 强制对端重新枚举（用于启动前获取新鲜路由/网卡）。
    pub fn adapters(&self) -> Result<Vec<AdapterInfo>, String> {
        let value = self.get("/adapters")?;
        adapters_from(&value)
    }

    pub fn refresh_adapters(&self) -> Result<Vec<AdapterInfo>, String> {
        let value = self.get("/adapters?refresh=true")?;
        adapters_from(&value)
    }

    /// 只做启动前预检：对端校验网卡/路由并回显将要使用的精确配置，不产生副作用。
    pub fn prepare_sender(&self, body: &Value) -> Result<Value, String> {
        self.post("/sender/prepare", body)
    }

    pub fn sender_start(&self, body: &Value) -> Result<Value, String> {
        self.post("/sender/start", body)
    }

    /// 只停止指定会话；对端若不匹配应保持其他采集会话不动。
    pub fn sender_stop_for(&self, session_id: &str) -> Result<Value, String> {
        self.post(
            "/sender/stop",
            &serde_json::json!({ "session_id": session_id }),
        )
    }

    pub fn sender_stats(&self) -> Result<Value, String> {
        self.get("/sender/stats")
    }

    pub fn update_status(&self) -> Result<Value, String> {
        self.get("/update/status")
    }

    pub fn activate_update(&self, build_id: &str) -> Result<Value, String> {
        self.post(
            "/update/activate",
            &serde_json::json!({"build_id": build_id}),
        )
    }

    pub fn rollback_update(&self, build_id: &str) -> Result<Value, String> {
        self.post(
            "/update/rollback",
            &serde_json::json!({"build_id": build_id}),
        )
    }

    /// 文件预检、哈希与上传由主机工作线程执行；不占用渲染线程。
    pub fn stage_update(&self, zip: &Path) -> Result<Value, String> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        crate::update::inspect_zip(zip)?;
        let mut file = std::fs::File::open(zip).map_err(|e| format!("无法读取更新包：{e}"))?;
        let bytes = file.metadata().map_err(|e| e.to_string())?.len();
        const MAX_UPLOAD: u64 = 512 * 1024 * 1024;
        if bytes == 0 || bytes > MAX_UPLOAD {
            return Err("更新包必须为1字节到512MiB。".into());
        }
        let mut payload = Vec::new();
        file.by_ref()
            .take(MAX_UPLOAD + 1)
            .read_to_end(&mut payload)
            .map_err(|e| e.to_string())?;
        if payload.len() as u64 != bytes {
            return Err("更新包在读取期间发生变化，请重试。".into());
        }
        let digest = format!("{:x}", Sha256::digest(&payload));
        let authorization = self.update_authorization()?;
        let mut response = self
            .agent
            .post(format!("{}/update/stage", self.base))
            .header("Authorization", authorization)
            .header("X-VC-SHA256", digest)
            .header("Content-Type", "application/zip")
            .config()
            .timeout_global(Some(Duration::from_secs(180)))
            .build()
            .send(payload.as_slice())
            .map_err(|e| format!("更新包上传失败：{e}"))?;
        let code = response.status().as_u16();
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_string()
            .map_err(|e| format!("更新响应读取失败：{e}"))?;
        if !response.status().is_success() {
            return Err(format_http_error("/update/stage", code, &text));
        }
        serde_json::from_str(&text).map_err(|e| format!("更新响应格式无效：{e}"))
    }
}

fn adapters_from(value: &Value) -> Result<Vec<AdapterInfo>, String> {
    let Some(items) = value.get("items") else {
        return Err("/adapters 响应缺少 items 数组。".into());
    };
    if !items.is_array() {
        return Err("/adapters 响应的 items 不是数组。".into());
    }
    serde_json::from_value(items.clone()).map_err(|error| format!("adapters 响应格式无效：{error}"))
}

/// 把对端错误响应转成可操作的消息。优先读取 `{"error": "...", "details": [...]}`。
fn format_http_error(path: &str, code: u16, text: &str) -> String {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        let mut message = value
            .get("error")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .unwrap_or("对端拒绝请求")
            .to_string();
        let details: Vec<&str> = value
            .get("details")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|detail| !detail.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if !details.is_empty() {
            message = format!("{message}：{}", details.join("；"));
        }
        if let Some(extra) = value.get("message").and_then(Value::as_str) {
            let extra = extra.trim();
            if !extra.is_empty() && !message.contains(extra) {
                message = format!("{message}：{extra}");
            }
        }
        return format!("{path} 返回 HTTP {code}：{message}");
    }
    if trimmed.is_empty() {
        return format!("{path} 返回 HTTP {code}（无响应体）。");
    }
    format!(
        "{path} 返回 HTTP {code}：{}",
        truncate_chars(trimmed, ERROR_BODY_CHARS)
    )
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn serve_once(response: String) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            // 先读完整请求头，再按 Content-Length 读完 body，避免响应时对端仍在发送。
            let header_end = loop {
                let read = stream.read(&mut buffer).unwrap_or(0);
                if read == 0 {
                    break None;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    break Some(position + 4);
                }
            };
            if let Some(header_end) = header_end {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while request.len() < header_end + content_length {
                    let read = stream.read(&mut buffer).unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
            }
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (format!("http://{addr}"), handle)
    }

    fn http_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn non_2xx_response_body_is_preserved() {
        let body =
            r#"{"error":"配置校验失败","details":["capture.width 必须是 2..8192 的偶数。"]}"#;
        let (base, handle) = serve_once(http_response("400 Bad Request", body));
        let client = PeerClient::new(&base);
        let error = client
            .save_config(&json!({"capture": {"width": 1}}))
            .unwrap_err();
        let request = handle.join().unwrap();
        assert!(request.starts_with("POST /config "), "{request}");
        assert!(error.contains("HTTP 400"), "{error}");
        assert!(error.contains("配置校验失败"), "{error}");
        assert!(error.contains("capture.width"), "{error}");
    }

    #[test]
    fn refresh_adapters_uses_refresh_query_and_parses_items() {
        let body = r#"{"items":[{"name":"\\Device\\NPF_{X}","description":"Intel I226-V","receive_link_bps":1000000000,"transmit_link_bps":1000000000}]}"#;
        let (base, handle) = serve_once(http_response("200 OK", body));
        let client = PeerClient::new(&base);
        let items = client.refresh_adapters().unwrap();
        let request = handle.join().unwrap();
        assert!(
            request.starts_with("GET /adapters?refresh=true "),
            "{request}"
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].description, "Intel I226-V");
        assert_eq!(items[0].receive_link_bps, Some(1_000_000_000));
    }

    #[test]
    fn sender_stop_for_scopes_session_id() {
        let (base, handle) = serve_once(http_response("200 OK", r#"{"stopped":true}"#));
        let client = PeerClient::new(&base);
        client.sender_stop_for("s-abc").unwrap();
        let request = handle.join().unwrap();
        assert!(request.starts_with("POST /sender/stop "), "{request}");
        assert!(request.contains("\"session_id\""), "{request}");
        assert!(request.contains("\"s-abc\""), "{request}");
    }

    #[test]
    fn update_token_is_sent_only_to_update_routes() {
        let token = "ab".repeat(32);
        let (base, handle) = serve_once(http_response("200 OK", r#"{"ok":true}"#));
        PeerClient::new(&base)
            .with_update_token(Some(token.clone()))
            .health()
            .unwrap();
        assert!(
            !handle
                .join()
                .unwrap()
                .to_ascii_lowercase()
                .contains("authorization:")
        );
        let (base, handle) = serve_once(http_response("200 OK", r#"{"ok":true}"#));
        PeerClient::new(&base)
            .with_update_token(Some(token.clone()))
            .update_status()
            .unwrap();
        let request = handle.join().unwrap();
        assert!(request.starts_with("GET /update/status "));
        assert!(request.contains(&format!("Bearer {token}")));
    }

    #[test]
    fn update_without_pairing_is_rejected_locally() {
        let error = PeerClient::new("http://127.0.0.1:1")
            .update_status()
            .unwrap_err();
        assert!(error.contains("配对"));
        let error = PeerClient::new("http://127.0.0.1:1")
            .with_update_token(Some("bad\r\nheader".into()))
            .update_status()
            .unwrap_err();
        assert!(error.contains("64"));
    }

    #[test]
    fn adapters_without_items_is_an_error() {
        let (base, _handle) = serve_once(http_response("200 OK", r#"{"exe":"vc-sender.exe"}"#));
        let client = PeerClient::new(&base);
        let error = client.adapters().unwrap_err();
        assert!(error.contains("items"), "{error}");
    }

    #[test]
    fn empty_success_body_is_an_error() {
        let (base, _handle) = serve_once(http_response("204 No Content", ""));
        let client = PeerClient::new(&base);
        let error = client.health().unwrap_err();
        assert!(error.contains("响应为空"), "{error}");
    }

    #[test]
    fn error_formatting_handles_plain_text_and_truncation() {
        let message = format_http_error("/x", 500, &"a".repeat(1000));
        assert!(message.contains("HTTP 500"));
        assert!(message.chars().count() < 600);

        let message = format_http_error("/x", 503, "raw failure\nline");
        assert!(message.contains("raw failure line"));
    }

    #[test]
    fn empty_base_fails_without_panicking() {
        let client = PeerClient::new("   ");
        let error = client.health().unwrap_err();
        assert!(error.contains("未配置被采集机地址"), "{error}");
    }
}
