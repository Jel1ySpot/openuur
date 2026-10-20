//! WebRTC peer setup for the native UU protocol.

use base64::Engine;
use flate2::read::GzDecoder;
use serde_json::Value;
use std::io::Read;
use std::sync::Arc;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::dtls_transport::dtls_role::DTLSRole;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

pub const CHANNELS: [&str; 5] = [
    "CONTROL_DATA_CHANNEL",
    "TEXT_DATA_CHANNEL",
    "STREAMER_DATA_CHANNEL",
    "FILE_DATA_CHANNEL",
    "BINARY_DATA_CHANNEL",
];

pub fn api(answerer: bool) -> Result<webrtc::api::API, String> {
    let mut media = MediaEngine::default();
    media
        .register_default_codecs()
        .map_err(|e| format!("register codecs: {e}"))?;
    // The official viewer negotiates H.265 through an m-line we only add when
    // the desktop track supports it. Registering the codec here does not add
    // it to an answer by itself.
    crate::desktop::video_track::register_hevc(&mut media)
        .map_err(|e| format!("register HEVC: {e}"))?;
    for uri in [
        crate::uu_native::rtp_metadata::CAPTURE_INDEX_URI,
        crate::uu_native::rtp_metadata::NEW_FRAME_URI,
    ] {
        media
            .register_header_extension(
                webrtc::rtp_transceiver::rtp_codec::RTCRtpHeaderExtensionCapability {
                    uri: uri.into(),
                },
                webrtc::rtp_transceiver::rtp_codec::RTPCodecType::Video,
                None,
            )
            .map_err(|e| format!("register UU RTP extension: {e}"))?;
    }
    let registry = register_default_interceptors(Default::default(), &mut media)
        .map_err(|e| format!("register interceptors: {e}"))?;
    let mut settings = SettingEngine::default();
    if answerer {
        settings
            .set_answering_dtls_role(DTLSRole::Server)
            .map_err(|e| format!("set DTLS role: {e}"))?;
    }
    settings.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);
    Ok(APIBuilder::new()
        .with_media_engine(media)
        .with_setting_engine(settings)
        .with_interceptor_registry(registry)
        .build())
}

pub fn ice_config(value: &Value) -> Result<RTCConfiguration, String> {
    super::session_ice::SessionIce::from_header(value).configuration()
}

pub async fn peer(
    answerer: bool,
    config: RTCConfiguration,
) -> Result<Arc<RTCPeerConnection>, String> {
    let pc = api(answerer)?
        .new_peer_connection(config)
        .await
        .map_err(|e| format!("create peer connection: {e}"))?;
    Ok(Arc::new(pc))
}

pub async fn data_channels(
    pc: &Arc<RTCPeerConnection>,
) -> Result<Vec<Arc<RTCDataChannel>>, String> {
    let mut channels = Vec::new();
    for label in CHANNELS {
        channels.push(
            pc.create_data_channel(
                label,
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    ..Default::default()
                }),
            )
            .await
            .map_err(|e| format!("create data channel {label}: {e}"))?,
        );
    }
    Ok(channels)
}

pub fn channel_open(channel: &Arc<RTCDataChannel>) -> bool {
    channel.ready_state() == RTCDataChannelState::Open
}

pub async fn add_candidate(pc: &Arc<RTCPeerConnection>, value: &Value) -> Result<(), String> {
    let (candidate, mid, index) = if let Some(text) = value.as_str() {
        (text.to_owned(), Some("0".to_owned()), Some(0))
    } else {
        (
            value
                .get("candidate")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            value
                .get("sdpMid")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| Some("0".to_owned())),
            value
                .get("sdpMLineIndex")
                .and_then(Value::as_u64)
                .map(|v| v as u16)
                .or(Some(0)),
        )
    };
    if candidate.is_empty() {
        return Ok(());
    }
    pc.add_ice_candidate(RTCIceCandidateInit {
        candidate,
        sdp_mid: mid,
        sdp_mline_index: index,
        ..Default::default()
    })
    .await
    .map_err(|e| format!("add ICE candidate: {e}"))
}

pub fn candidate_json(
    candidate: &webrtc::ice_transport::ice_candidate::RTCIceCandidate,
) -> Option<Value> {
    let init = candidate.to_json().ok()?;
    Some(serde_json::json!({
        "candidate": init.candidate,
        "sdpMid": init.sdp_mid,
        "sdpMLineIndex": init.sdp_mline_index,
    }))
}

pub fn answer_sdp(value: &Value) -> Result<String, String> {
    if let Some(sdp) = value.get("sdp").and_then(Value::as_str) {
        if !sdp.is_empty() && sdp.len() <= 1024 * 1024 {
            return Ok(sdp.to_owned());
        }
    }
    if let Some(encoded) = value.get("gzip_sdp").and_then(Value::as_str) {
        if encoded.len() > 2 * 1024 * 1024 {
            return Err("compressed SDP too large".to_owned());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "invalid gzip_sdp".to_owned())?;
        let mut sdp = String::new();
        GzDecoder::new(bytes.as_slice())
            .take(1024 * 1024 + 1)
            .read_to_string(&mut sdp)
            .map_err(|_| "invalid gzip_sdp".to_owned())?;
        if !sdp.is_empty() && sdp.len() <= 1024 * 1024 {
            return Ok(sdp);
        }
    }
    Err("missing SDP".to_owned())
}

fn sdp_shape(sdp: &str) -> String {
    sdp.lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("m=")
                || line.starts_with("a=mid:")
                || line.starts_with("a=sctp-port:")
                || line.starts_with("a=max-message-size:")
                || line.starts_with("a=setup:")
                || line.starts_with("a=group:BUNDLE")
                || line.starts_with("a=ice-options:")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

pub fn with_max_message_size(sdp: &str) -> String {
    if sdp.contains("a=max-message-size:") {
        return sdp.to_owned();
    }
    let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(sdp.len() + 32);
    for line in sdp.split_inclusive('\n') {
        out.push_str(line);
        if line.trim_end().starts_with("a=sctp-port:") {
            out.push_str("a=max-message-size:262144");
            out.push_str(eol);
        }
    }
    out
}

pub fn with_quic(sdp: &str) -> String {
    if sdp.contains("a=x-uuremote-quic") {
        return sdp.to_owned();
    }
    if let Some(index) = sdp.find("a=group:BUNDLE") {
        let mut out = sdp.to_owned();
        out.insert_str(index, "a=x-uuremote-quic\r\n");
        out
    } else {
        sdp.to_owned()
    }
}

pub async fn set_offer(pc: &Arc<RTCPeerConnection>, sdp: &str) -> Result<(), String> {
    tracing::debug!("native UU remote offer shape: {}", sdp_shape(sdp));
    pc.set_remote_description(
        RTCSessionDescription::offer(sdp.to_owned()).map_err(|e| e.to_string())?,
    )
    .await
    .map_err(|e| format!("set remote offer: {e}"))
}

pub async fn set_answer(pc: &Arc<RTCPeerConnection>, sdp: &str) -> Result<(), String> {
    pc.set_remote_description(
        RTCSessionDescription::answer(sdp.to_owned()).map_err(|e| e.to_string())?,
    )
    .await
    .map_err(|e| format!("set remote answer: {e}"))
}

pub async fn local_answer(pc: &Arc<RTCPeerConnection>) -> Result<String, String> {
    let answer = pc
        .create_answer(None)
        .await
        .map_err(|e| format!("create answer: {e}"))?;
    tracing::debug!("native UU answer shape: {}", sdp_shape(&answer.sdp));
    pc.set_local_description(answer.clone())
        .await
        .map_err(|e| format!("set local answer: {e}"))?;
    Ok(with_max_message_size(&answer.sdp))
}

pub async fn local_offer(pc: &Arc<RTCPeerConnection>) -> Result<String, String> {
    let offer = pc
        .create_offer(None)
        .await
        .map_err(|e| format!("create offer: {e}"))?;
    tracing::debug!("native UU offer shape: {}", sdp_shape(&offer.sdp));
    pc.set_local_description(offer.clone())
        .await
        .map_err(|e| format!("set local offer: {e}"))?;
    Ok(with_max_message_size(&with_quic(&offer.sdp)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ice_uses_cloud_credentials_and_honors_relay_only() {
        // No cloud servers: public STUN keeps a direct/LAN path possible.
        let config = ice_config(&serde_json::json!({})).unwrap();
        assert_eq!(
            config.ice_transport_policy,
            webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy::All
        );
        assert!(config.ice_servers[0]
            .urls
            .iter()
            .all(|url| url.starts_with("stun:")));
        // Relay-only without TURN must fail rather than silently fall back.
        let stun = serde_json::json!({
            "iceServers": [{"urls": ["stun:example.invalid:3478"]}],
            "force_relay": true
        });
        assert!(ice_config(&stun).is_err());
        let turn = serde_json::json!({
            "data": {
                "iceServers": [{
                    "urls": ["turns:example.invalid:443?transport=tcp"],
                    "username": "u",
                    "credential": "p"
                }],
                "force_relay": true
            }
        });
        let config = ice_config(&turn).unwrap();
        assert_eq!(
            config.ice_transport_policy,
            webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy::Relay
        );
        assert_eq!(config.ice_servers.len(), 1);
        assert_eq!(config.ice_servers[0].username, "u");
    }

    #[test]
    fn max_message_and_quic_are_sent_copy_only() {
        let sdp = "v=0\r\na=group:BUNDLE 0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=sctp-port:5000\r\n";
        let out = with_max_message_size(&with_quic(sdp));
        assert!(out.contains("a=x-uuremote-quic"));
        assert!(out.contains("a=max-message-size:262144"));
        assert!(!sdp.contains("x-uuremote"));
    }
}
