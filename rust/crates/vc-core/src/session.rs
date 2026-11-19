//! Rust 控制面 v1 会话契约；显式参数回显，不与 Python agent 混跑。
use crate::{adapters::AdapterInfo, config::Capture};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const IMPLEMENTATION: &str = "virtualcapture-rust";
pub const API_REVISION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SenderRequest {
    pub capture: Capture,
    pub session_id: String,
    pub source_mac: String,
    #[serde(default)]
    pub auto_adapter: bool,
    #[serde(default)]
    pub adapter: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedSession {
    pub capture: Capture,
    /// Raw 为按实际网卡与几何计算的精确值；编码会话为 None（DLL 按帧动态切包）。
    pub batch_packets: Option<u32>,
    pub session_id: String,
    pub source_mac: String,
    pub adapter: AdapterInfo,
}

impl SenderRequest {
    pub fn validate_identity(&self) -> Result<(), String> {
        if !(3..=80).contains(&self.session_id.len())
            || !self
                .session_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err("session_id 必须是 3..80 位 ASCII 字母/数字/连字符。".into());
        }
        let bytes = parse_mac(&self.source_mac).ok_or("source_mac 必须是完整 MAC 地址。")?;
        if bytes[0] & 3 != 2 {
            return Err("source_mac 必须是本地管理的单播 MAC。".into());
        }
        if self.auto_adapter && !self.adapter.is_empty() {
            return Err("auto_adapter 与显式 adapter 不能同时提供。".into());
        }
        Ok(())
    }

    pub fn confirm(&self, response: &PreparedSession) -> Result<(), String> {
        let expected_batch =
            crate::process::expected_batch_packets(&self.capture, &response.adapter.name)?;
        if self.capture != response.capture
            || expected_batch != response.batch_packets
            || self.session_id != response.session_id
            || self.source_mac != response.source_mac
            || (!self.adapter.is_empty() && self.adapter != response.adapter.name)
        {
            return Err("对端未确认相同采集参数、会话或网卡，拒绝继续。".into());
        }
        Ok(())
    }
}

pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let parts: Vec<_> = text.split(':').collect();
    if parts.len() != 6 || parts.iter().any(|p| p.len() != 2) {
        return None;
    }
    let mut bytes = [0; 6];
    for (i, part) in parts.iter().enumerate() {
        bytes[i] = u8::from_str_radix(part, 16).ok()?;
    }
    Some(bytes)
}

pub fn require_peer(health: &Value) -> Result<(), String> {
    if health["implementation"] != IMPLEMENTATION
        || health["api_revision"] != API_REVISION
        || health["role"] != "capture"
        || health["ok"] != true
    {
        return Err("对端不是兼容的 Rust 采集后台；请在独立目录启动 vc-capture。".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn python_health_cannot_be_accepted_as_rust() {
        assert!(
            require_peer(&serde_json::json!({"ok":true,"role":"capture","version":"0.5.0"}))
                .is_err()
        );
    }
    #[test]
    fn rejects_multicast_and_path_session_ids() {
        let mut request = SenderRequest {
            capture: Capture::default(),
            session_id: "s-test".into(),
            source_mac: "02:01:02:03:04:05".into(),
            auto_adapter: true,
            adapter: String::new(),
        };
        assert!(request.validate_identity().is_ok());
        request.source_mac = "03:01:02:03:04:05".into();
        assert!(request.validate_identity().is_err());
        request.source_mac = "02:01:02:03:04:05".into();
        request.session_id = "../../evidence".into();
        assert!(request.validate_identity().is_err());
    }

    #[test]
    fn confirm_requires_exact_raw_batch_and_null_for_encoded() {
        let request = SenderRequest {
            capture: Capture::default(),
            session_id: "s-test".into(),
            source_mac: "02:01:02:03:04:05".into(),
            auto_adapter: true,
            adapter: String::new(),
        };
        let adapter = AdapterInfo {
            name: r"\Device\NPF_{A}".into(),
            ..Default::default()
        };
        let mut response = PreparedSession {
            capture: request.capture.clone(),
            batch_packets: Some(69),
            session_id: request.session_id.clone(),
            source_mac: request.source_mac.clone(),
            adapter: adapter.clone(),
        };
        assert!(request.confirm(&response).is_ok());
        response.batch_packets = Some(68);
        assert!(request.confirm(&response).is_err());

        let mut encoded_request = request.clone();
        encoded_request.capture.codec = "mjpeg".into();
        encoded_request.capture.codec_experimental = true;
        encoded_request.capture.encoder_backend = "turbojpeg:cpu".into();
        encoded_request.capture.decoder_backend = "turbojpeg:cpu".into();
        let mut encoded_response = PreparedSession {
            capture: encoded_request.capture.clone(),
            batch_packets: None,
            ..response
        };
        assert!(encoded_request.confirm(&encoded_response).is_ok());
        encoded_response.batch_packets = Some(1);
        assert!(encoded_request.confirm(&encoded_response).is_err());
    }
}
