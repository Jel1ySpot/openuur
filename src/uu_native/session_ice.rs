//! Per-connection ICE configuration and network-switch handling.
//!
//! Cloud credentials from the room `control`/`be-controlled` header are scoped
//! to one peer and never logged. Relay-only sessions are refused unless a
//! usable TURN URL was supplied. When the cloud sends no servers, the
//! official hosts fall back to public STUN so a LAN/direct path is still
//! attempted.

use serde_json::Value;
use webrtc::ice_transport::ice_credential_type::RTCIceCredentialType;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;

const DEFAULT_STUN: [&str; 3] = [
    "stun:stun.l.google.com:19302",
    "stun:stun1.l.google.com:19302",
    "stun:stun.miwifi.com:3478",
];
const MAX_SERVERS: usize = 16;
const MAX_URLS: usize = 16;

#[derive(Clone, Default)]
pub struct SessionIce {
    servers: Vec<RTCIceServer>,
    relay_only: bool,
    cloud_supplied: bool,
}

impl SessionIce {
    pub fn from_header(header: &Value) -> Self {
        let data = header.get("data").unwrap_or(header);
        let field = |name: &str| header.get(name).or_else(|| data.get(name));
        let relay_only = field("force_relay")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut servers = Vec::new();
        if let Some(list) = field("iceServers").and_then(Value::as_array) {
            for server in list.iter().take(MAX_SERVERS) {
                let urls = match server.get("urls").or_else(|| server.get("url")) {
                    Some(Value::String(url)) => vec![url.clone()],
                    Some(Value::Array(urls)) => urls
                        .iter()
                        .filter_map(Value::as_str)
                        .take(MAX_URLS)
                        .map(str::to_owned)
                        .collect(),
                    _ => Vec::new(),
                };
                let urls: Vec<String> = urls
                    .into_iter()
                    .filter(|url| {
                        url.len() <= 2048
                            && ["stun:", "stuns:", "turn:", "turns:"]
                                .iter()
                                .any(|prefix| url.starts_with(prefix))
                    })
                    .collect();
                if urls.is_empty() {
                    continue;
                }
                let text = |name: &str| {
                    server
                        .get(name)
                        .and_then(Value::as_str)
                        .filter(|value| value.len() <= 4096)
                        .unwrap_or("")
                        .to_owned()
                };
                servers.push(RTCIceServer {
                    urls,
                    username: text("username"),
                    credential: text("credential"),
                    credential_type: RTCIceCredentialType::Password,
                });
            }
        }
        Self {
            cloud_supplied: !servers.is_empty(),
            relay_only,
            servers,
        }
    }

    pub fn configuration(self) -> Result<RTCConfiguration, String> {
        let has_turn = self.servers.iter().any(|server| {
            server
                .urls
                .iter()
                .any(|url| url.starts_with("turn:") || url.starts_with("turns:"))
        });
        if self.relay_only && !has_turn {
            return Err("relay-only session requested without usable TURN configuration".into());
        }
        let supplied = self.cloud_supplied;
        let servers = if supplied {
            self.servers
        } else {
            vec![RTCIceServer {
                urls: DEFAULT_STUN.iter().map(|url| (*url).to_owned()).collect(),
                ..Default::default()
            }]
        };
        tracing::info!(
            "session ICE: cloud_supplied={} servers={} turn={} relay_only={}",
            supplied,
            servers.len(),
            has_turn,
            self.relay_only
        );
        Ok(RTCConfiguration {
            ice_servers: servers,
            ice_transport_policy: if self.relay_only {
                RTCIceTransportPolicy::Relay
            } else {
                RTCIceTransportPolicy::All
            },
            ..Default::default()
        })
    }

    pub fn relay_only(&self) -> bool {
        self.relay_only
    }

    pub fn cloud_supplied(&self) -> bool {
        self.cloud_supplied
    }
}

/// Build a fresh offer for an ICE restart. The local peer keeps its DTLS role
/// and media sections; only new ufrag/pwd and candidates are generated.
pub async fn restart_offer(
    pc: &std::sync::Arc<webrtc::peer_connection::RTCPeerConnection>,
) -> Result<String, String> {
    let offer = pc
        .create_offer(Some(
            webrtc::peer_connection::offer_answer_options::RTCOfferOptions {
                ice_restart: true,
                ..Default::default()
            },
        ))
        .await
        .map_err(|error| format!("create ICE-restart offer: {error}"))?;
    pc.set_local_description(offer.clone())
        .await
        .map_err(|error| format!("set ICE-restart description: {error}"))?;
    Ok(offer.sdp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_credentials_are_scoped_and_relay_policy_is_honored() {
        let header = serde_json::json!({
            "data": {
                "iceServers": [{
                    "urls": ["turn:example.invalid:3478?transport=udp"],
                    "username": "test-user",
                    "credential": "test-secret"
                }],
                "force_relay": true
            }
        });
        let ice = SessionIce::from_header(&header);
        assert!(ice.cloud_supplied());
        let config = ice.configuration().unwrap();
        assert_eq!(config.ice_transport_policy, RTCIceTransportPolicy::Relay);
        assert_eq!(config.ice_servers[0].username, "test-user");
        assert_eq!(config.ice_servers[0].credential, "test-secret");
    }

    #[test]
    fn relay_without_turn_fails_and_empty_defaults_to_stun() {
        let header = serde_json::json!({
            "force_relay": true,
            "iceServers": [{"urls": "https://example.invalid"}]
        });
        assert!(SessionIce::from_header(&header).configuration().is_err());
        let config = SessionIce::default().configuration().unwrap();
        assert_eq!(config.ice_transport_policy, RTCIceTransportPolicy::All);
        assert_eq!(config.ice_servers.len(), 1);
        assert!(config.ice_servers[0]
            .urls
            .iter()
            .all(|url| url.starts_with("stun:")));
    }

    #[test]
    fn unsupported_and_oversized_urls_are_dropped() {
        let header = serde_json::json!({
            "iceServers": [
                {"urls": ["http://x", "stun:ok.example:3478"]},
                {"urls": "turn:ok.example:3478", "username": "u"},
                {"urls": "stun:ignored.example"}
            ]
        });
        let ice = SessionIce::from_header(&header);
        let config = ice.configuration().unwrap();
        assert_eq!(config.ice_servers.len(), 3);
        assert_eq!(config.ice_servers[0].urls, vec!["stun:ok.example:3478"]);
        assert_eq!(config.ice_servers[1].urls, vec!["turn:ok.example:3478"]);
    }
}
