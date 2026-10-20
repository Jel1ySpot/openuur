use base64::Engine;
use rand::RngCore;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn random_device_id() -> String {
    // UU device ids are 16 lowercase alphanumeric characters. This value is
    // only a client-side identifier for header signing; the
    // server assigns real device ids when the official client registers.
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    let mut out = String::with_capacity(16);
    for _ in 0..16 {
        out.push(ALPHABET[(rng.next_u32() as usize) % ALPHABET.len()] as char);
    }
    out
}

/// Recursively search a JSON value for the first string at one of `keys`.
pub fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(v) = map.get(*key) {
                    if let Some(s) = v.as_str() {
                        return Some(s.to_owned());
                    }
                    if let Some(n) = v.as_i64() {
                        return Some(n.to_string());
                    }
                }
            }
            for v in map.values() {
                if let Some(found) = find_string(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(|v| find_string(v, keys)),
        _ => None,
    }
}

/// Recursively search a JSON value for the first scalar at one of `keys`.
pub fn find_i64(value: &Value, keys: &[&str]) -> Option<i64> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(v) = map.get(*key) {
                    if let Some(n) = v.as_i64() {
                        return Some(n);
                    }
                    if let Some(s) = v.as_str() {
                        if let Ok(n) = s.parse::<i64>() {
                            return Some(n);
                        }
                    }
                }
            }
            for v in map.values() {
                if let Some(found) = find_i64(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(|v| find_i64(v, keys)),
        _ => None,
    }
}

/// Strip a leading `Bearer ` from a token.
pub fn normalize_token(token: &str) -> String {
    token
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| token.trim().strip_prefix("bearer "))
        .unwrap_or(token.trim())
        .to_owned()
}

/// Normalize a platform value from the REST API into a stable lowercase name.
pub fn normalize_platform(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let s = s.to_ascii_lowercase();
            Some(match s.as_str() {
                "1" | "pc" | "windows" => "windows".to_owned(),
                "2" | "mac" | "macos" => "mac".to_owned(),
                "3" | "android" => "android".to_owned(),
                "4" | "ios" => "ios".to_owned(),
                "5" | "tv" => "tv".to_owned(),
                other => other.to_owned(),
            })
        }
        Value::Number(n) => n.as_i64().map(|n| match n {
            1 => "windows".to_owned(),
            2 => "mac".to_owned(),
            3 => "android".to_owned(),
            4 => "ios".to_owned(),
            5 => "tv".to_owned(),
            other => other.to_string(),
        }),
        _ => None,
    }
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Decode the payload of a JWT without verifying its signature. Used only to
/// recover identity hints for request headers.
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}
