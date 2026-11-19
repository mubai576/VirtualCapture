use std::path::Path;

use serde_json::Value;

/// 读取 `version.json` 的 version 字段；缺失返回 "dev"。
pub fn read() -> String {
    let path = crate::paths::root().join("version.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| {
            value
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "dev".to_string())
}

pub fn read_from(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned)
}
