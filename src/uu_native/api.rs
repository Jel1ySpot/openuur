//! REST room lifecycle for the native protocol.

use crate::api::ApiClient;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::util::find_string;
use serde_json::Value;

#[derive(Clone)]
pub struct Room {
    pub token: String,
    pub servers: Vec<String>,
    pub ws_timeout_ms: u64,
}

pub fn client(config: &Config) -> Result<ApiClient> {
    let token = config.require_token()?;
    let mut api = ApiClient::from_config(config, Some(token.clone()))?;
    if let Some(claims) = crate::util::jwt_claims(&token) {
        let claim = |key: &str| {
            claims
                .get(key)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        };
        if let (Some(client_id), Some(device_id), Some(system_id)) =
            (claim("client_id"), claim("device_id"), claim("system_id"))
        {
            api.set_identity(client_id, device_id, system_id, claim("sub"));
        }
    }
    Ok(api)
}

pub async fn create(config: &Config) -> Result<Room> {
    let raw = client(config)?.create_host_room().await?;
    parse_room(&raw.raw)
}

pub async fn join(config: &Config, device: &str) -> Result<Room> {
    let raw = client(config)?.join_room(device, false).await?;
    parse_room(&raw.raw)
}

fn parse_room(raw: &Value) -> Result<Room> {
    let data = raw.get("data").unwrap_or(raw);
    let token = data
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol("room response has no token".to_owned()))?;
    let mut servers = Vec::new();
    if let Some(server) = data.get("signaling_server").and_then(Value::as_str) {
        servers.push(server.to_owned());
    }
    if let Some(list) = data.get("signaling_list").and_then(Value::as_array) {
        for item in list {
            if let Some(server) = item.as_str() {
                servers.push(server.to_owned());
            } else if let Some(server) = item.get("url").and_then(Value::as_str) {
                servers.push(server.to_owned());
            }
        }
    }
    servers.sort_by_key(|server| {
        let dcdn = if server.contains("sig-dcdn") { 0 } else { 1 };
        (dcdn, server.clone())
    });
    servers.dedup();
    if servers.is_empty() {
        return Err(Error::Protocol(
            "room response has no signaling server".to_owned(),
        ));
    }
    let ws_timeout_ms = data
        .get("ws_connect_timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(12_000)
        .clamp(1_000, 60_000);
    Ok(Room {
        token: token.to_owned(),
        servers,
        ws_timeout_ms,
    })
}

pub fn response_message(raw: &Value) -> Option<String> {
    find_string(raw, &["msg", "message"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_parsers_prefer_dcdn_and_accept_object_entries() {
        let raw = serde_json::json!({"data":{"token":"t","signaling_list":[
            {"url":"wss://sig-3207-e.example/"},
            "wss://sig-dcdn-3207-e.example/"
        ]}});
        let room = parse_room(&raw).unwrap();
        assert_eq!(room.token, "t");
        assert_eq!(room.servers[0], "wss://sig-dcdn-3207-e.example/");
        assert_eq!(room.servers.len(), 2);
    }
}
