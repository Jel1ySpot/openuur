use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub device_id: String,
    pub device_name: String,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub raw: Value,
}

impl Device {
    pub fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        let device_id = pick_string(obj, &["deviceId", "device_id", "id", "uuid", "deviceGuid"])?;
        let device_name = pick_string(
            obj,
            &[
                "deviceName",
                "device_name",
                "alias",
                "name",
                "nickname",
                "nickName",
            ],
        )
        .unwrap_or_else(|| device_id.clone());
        let online = pick_bool(obj, &["isOnline", "online", "is_online", "onlineState"])
            .or_else(|| {
                obj.get("status").and_then(|v| v.as_str()).map(|s| {
                    matches!(
                        s.to_ascii_lowercase().as_str(),
                        "online" | "connected" | "ready"
                    )
                })
            })
            .unwrap_or(false);
        let platform = obj
            .get("platform")
            .and_then(crate::util::normalize_platform);
        Some(Self {
            device_id,
            device_name,
            online,
            platform,
            raw: value.clone(),
        })
    }

    pub fn display_platform(&self) -> &str {
        self.platform.as_deref().unwrap_or("-")
    }
}

fn pick_string(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(v) = obj.get(*key) {
            if let Some(s) = v.as_str() {
                if !s.trim().is_empty() {
                    return Some(s.to_owned());
                }
            }
            if let Some(n) = v.as_i64() {
                return Some(n.to_string());
            }
        }
    }
    None
}

fn pick_bool(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<bool> {
    for key in keys {
        if let Some(v) = obj.get(*key) {
            if let Some(b) = v.as_bool() {
                return Some(b);
            }
            if let Some(n) = v.as_i64() {
                return Some(n != 0);
            }
            if let Some(s) = v.as_str() {
                return Some(matches!(s, "1" | "true" | "online" | "Online" | "ONLINE"));
            }
        }
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginOutcome {
    pub token: String,
    pub user_id: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomInfo {
    pub raw: Value,
}

impl RoomInfo {
    pub fn pick(&self, keys: &[&str]) -> Option<String> {
        crate::util::find_string(&self.raw, keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_camel_case_device() {
        let value = serde_json::json!({
            "deviceId": "dev-1",
            "deviceName": "home",
            "isOnline": true,
            "platform": 1
        });
        let device = Device::from_value(&value).unwrap();
        assert_eq!(device.device_id, "dev-1");
        assert_eq!(device.device_name, "home");
        assert!(device.online);
        assert_eq!(device.platform.as_deref(), Some("windows"));
    }

    #[test]
    fn parses_snake_case_device() {
        let value = serde_json::json!({
            "device_id": "dev-2",
            "device_name": "office",
            "online": 1,
            "platform": "mac"
        });
        let device = Device::from_value(&value).unwrap();
        assert_eq!(device.device_name, "office");
        assert!(device.online);
        assert_eq!(device.platform.as_deref(), Some("mac"));
    }
}
