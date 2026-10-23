//! WebRTC peer setup for the native UU protocol.

use base64::Engine;
use flate2::read::GzDecoder;
use serde_json::Value;
use std::io::Read;
use std::sync::{Arc, Mutex};
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
    // Virtual/overlay links (ZeroTier, container bridges, libvirt) advertise
    // candidates that cannot carry the media path. Offering them makes the
    // viewer spend its connectivity checks on unreachable addresses instead
    // of the physical LAN/public interface.
    settings.set_interface_filter(Box::new(ice_interface_allowed));
    Ok(APIBuilder::new()
        .with_media_engine(media)
        .with_setting_engine(settings)
        .with_interceptor_registry(registry)
        .build())
}

fn ice_interface_allowed(name: &str) -> bool {
    !["zt", "docker", "br-", "veth", "virbr", "lo"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
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
    bundle_media: Option<&(String, u16)>,
) -> Option<Value> {
    let init = candidate.to_json().ok()?;
    // webrtc-rs reports an empty `sdpMid` on gathered candidates and a fixed
    // index 0. The official viewer validates the media association, so use the
    // active BUNDLE m-line that produced the answer for every candidate.
    let (mid, index) = match bundle_media {
        Some((mid, index)) => (mid.clone(), Some(*index)),
        None => (
            init.sdp_mid
                .as_deref()
                .filter(|mid| !mid.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| "0".to_owned()),
            init.sdp_mline_index,
        ),
    };
    Some(serde_json::json!({
        "candidate": init.candidate,
        "sdpMid": mid,
        "sdpMLineIndex": index,
    }))
}

/// Pick the active media section from the BUNDLE group. The answer's first
/// bundled m-line carries the ICE transport shared by every data channel.
fn bundle_media(sdp: &str) -> Option<(String, u16)> {
    let preferred: Vec<&str> = sdp
        .lines()
        .find_map(|line| line.strip_prefix("a=group:BUNDLE "))?
        .split_whitespace()
        .collect();
    let mut sections: Vec<(bool, Option<String>)> = Vec::new();
    for line in sdp.lines() {
        if let Some(media) = line.strip_prefix("m=") {
            let port = media.split_whitespace().nth(1)?;
            sections.push((port != "0", None));
        } else if let Some(mid) = line.strip_prefix("a=mid:") {
            if let Some(section) = sections.last_mut() {
                section.1 = Some(mid.to_owned());
            }
        }
    }
    for mid in preferred {
        for (index, (active, section_mid)) in sections.iter().enumerate() {
            if *active && section_mid.as_deref() == Some(mid) {
                return Some((mid.to_owned(), index.try_into().ok()?));
            }
        }
    }
    None
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

pub async fn local_answer(
    pc: &Arc<RTCPeerConnection>,
    bundle: &Mutex<Option<(String, u16)>>,
) -> Result<String, String> {
    let answer = pc
        .create_answer(None)
        .await
        .map_err(|e| format!("create answer: {e}"))?;
    tracing::debug!("native UU answer shape: {}", sdp_shape(&answer.sdp));
    // Record the association before `set_local_description` starts ICE
    // gathering, otherwise the first candidate can be sent with an empty mid.
    if let Some(media) = bundle_media(&answer.sdp) {
        *bundle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(media);
    }
    pc.set_local_description(answer.clone())
        .await
        .map_err(|e| format!("set local answer: {e}"))?;
    Ok(with_max_message_size(&answer.sdp))
}

pub async fn local_offer(
    pc: &Arc<RTCPeerConnection>,
    bundle: &Mutex<Option<(String, u16)>>,
) -> Result<String, String> {
    let offer = pc
        .create_offer(None)
        .await
        .map_err(|e| format!("create offer: {e}"))?;
    tracing::debug!("native UU offer shape: {}", sdp_shape(&offer.sdp));
    if let Some(media) = bundle_media(&offer.sdp) {
        *bundle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(media);
    }
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

    #[test]
    fn bundle_media_uses_the_first_active_bundled_section() {
        let sdp = "v=0\r\na=group:BUNDLE 2 1\r\n\
                   m=audio 0 RTP/AVP 0\r\na=mid:0\r\n\
                   m=video 9 RTP/AVP 96\r\na=mid:1\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:2\r\n";
        assert_eq!(bundle_media(sdp), Some(("2".to_owned(), 2)));
        let reordered = "v=0\r\na=group:BUNDLE 1 2\r\n\
                         m=video 9 RTP/AVP 96\r\na=mid:1\r\n\
                         m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:2\r\n";
        assert_eq!(bundle_media(reordered), Some(("1".to_owned(), 0)));
        assert_eq!(bundle_media("v=0\r\n"), None);
    }

    #[test]
    fn candidate_json_replaces_webrtc_empty_mid_with_bundle_media() {
        use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
        use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
        use webrtc::ice_transport::ice_protocol::RTCIceProtocol;
        let candidate = RTCIceCandidate {
            foundation: "1".to_owned(),
            priority: 1,
            address: "192.0.2.1".to_owned(),
            protocol: RTCIceProtocol::Udp,
            port: 5000,
            typ: RTCIceCandidateType::Host,
            component: 1,
            ..Default::default()
        };
        let media = ("video".to_owned(), 1);
        let value = candidate_json(&candidate, Some(&media)).unwrap();
        assert_eq!(value["sdpMid"], "video");
        assert_eq!(value["sdpMLineIndex"], 1);
        let fallback = candidate_json(&candidate, None).unwrap();
        assert_eq!(fallback["sdpMid"], "0");
        assert_eq!(fallback["sdpMLineIndex"], 0);
    }

    #[test]
    fn ice_skips_overlay_and_container_interfaces_only() {
        for name in ["ztfp6jdvog", "docker0", "br-1a2b", "veth12", "virbr0", "lo"] {
            assert!(!ice_interface_allowed(name), "{name}");
        }
        for name in ["eno1", "enp5s0", "wlp3s0", "eth0"] {
            assert!(ice_interface_allowed(name), "{name}");
        }
    }
}
