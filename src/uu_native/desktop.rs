//! Native UU desktop stream: video/audio tracks and portal input wiring.

use super::signal::Sender;
use super::video_settings::{self, VideoSettings};
use crate::desktop::encoder::{Encoder, EncoderConfig};
use crate::desktop::video_track::{Codec, VideoTrack};
use crate::desktop::{self, DesktopSession};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use webrtc::media::Sample;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

pub async fn add_track(pc: &Arc<RTCPeerConnection>) -> Result<Arc<VideoTrack>, String> {
    let track = Arc::new(VideoTrack::new());
    pc.add_track(track.clone())
        .await
        .map_err(|error| format!("add video track: {error}"))?;
    Ok(track)
}

pub async fn add_audio_track(
    pc: &Arc<RTCPeerConnection>,
) -> Result<Arc<TrackLocalStaticSample>, String> {
    let track = desktop::audio::track();
    pc.add_track(track.clone())
        .await
        .map_err(|error| format!("add audio track: {error}"))?;
    Ok(track)
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    pc: Arc<RTCPeerConnection>,
    track: Arc<VideoTrack>,
    desktop: Arc<DesktopSession>,
    sender: Sender,
    client_id: String,
    ice_id: String,
    app_id: String,
    settings: watch::Receiver<VideoSettings>,
) {
    tokio::spawn(async move {
        if !wait_connected(&pc).await {
            return;
        }
        let (available_h264, available_h265) = track.available();
        let initial = *settings.borrow();
        let mut current = if initial.codec == Codec::H265 && available_h265 {
            initial
        } else if available_h264 {
            VideoSettings {
                codec: Codec::H264,
                ..initial
            }
        } else if available_h265 {
            VideoSettings {
                codec: Codec::H265,
                ..initial
            }
        } else {
            tracing::warn!("viewer negotiated no supported video codec");
            return;
        };
        announce(
            &sender, &client_id, &ice_id, &app_id, &desktop, &track, current,
        );
        let mut encoder: Option<Encoder> = None;
        let mut force_cpu = false;
        let locked = crate::desktop::session_lock::watch_locked();
        let mut black = crate::uu_native::locked_video::BlackFrameCache::default();
        let mut last_black: Option<std::time::Instant> = None;
        let mut locked_codec: Option<Codec> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_tick = tokio::time::Instant::now();
        loop {
            tick.tick().await;
            if matches!(
                pc.connection_state(),
                RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed
            ) {
                break;
            }
            // A network switch produces a temporary Disconnected state. Keep
            // the encoder and track alive; ICE restart (host or viewer) may
            // reconnect. Only Closed/Failed end the stream.
            if pc.connection_state() == RTCPeerConnectionState::Disconnected {
                continue;
            }
            let next = *settings.borrow();
            let next = if next.codec == Codec::H265 && available_h265 {
                next
            } else if available_h264 {
                VideoSettings {
                    codec: Codec::H264,
                    ..next
                }
            } else {
                next
            };
            if *locked.borrow() == Some(true) {
                // The official Android viewer reaches auto-unlock only after
                // it has received video for the desktop target. Keep the track
                // alive with one synthetic black keyframe per second.
                if encoder.take().is_some() {
                    tracing::info!("desktop locked; switching to synthetic black video");
                }
                current = next;
                if last_black.is_some_and(|sent| {
                    sent.elapsed() < crate::uu_native::locked_video::LOCKED_FRAME_INTERVAL
                }) {
                    continue;
                }
                let codec = if next.codec == Codec::H265 && available_h265 {
                    Codec::H265
                } else if available_h264 {
                    Codec::H264
                } else if available_h265 {
                    Codec::H265
                } else {
                    break;
                };
                let frame = match black.get(codec, next.chroma).await {
                    Ok(frame) => frame,
                    Err(error) => {
                        tracing::warn!("locked desktop black frame unavailable: {error}");
                        last_black = Some(std::time::Instant::now());
                        continue;
                    }
                };
                let sample = Sample {
                    data: bytes::Bytes::from(frame),
                    duration: crate::uu_native::locked_video::LOCKED_FRAME_INTERVAL,
                    ..Default::default()
                };
                let extensions = crate::uu_native::rtp_metadata::video_extensions(0);
                if let Err(error) = track
                    .write_sample_with_extensions(codec, &sample, &extensions)
                    .await
                {
                    tracing::warn!("locked desktop video write failed: {error}");
                    break;
                }
                locked_codec = Some(codec);
                last_black = Some(std::time::Instant::now());
                continue;
            }
            if locked_codec.take().is_some() {
                tracing::info!("desktop unlocked; resuming captured video");
                last_black = None;
                encoder = None;
            }
            let (native_width, native_height) = desktop.active_size();
            let mut config = config_for(native_width, native_height, next);
            if force_cpu {
                config.kind = crate::desktop::encoder::EncoderKind::Cpu;
                // The software encoder supports 4:4:4 even when the failed
                // hardware encoder did not.
                config.chroma = next.chroma;
            }
            let restart = encoder
                .as_ref()
                .is_none_or(|encoder| encoder.config() != config);
            if restart {
                if encoder.is_some() {
                    tracing::info!("restarting desktop encoder: {config:?}");
                }
                match Encoder::start_config(config) {
                    Ok(started) => {
                        encoder = Some(started);
                        current = next;
                        announce(
                            &sender, &client_id, &ice_id, &app_id, &desktop, &track, current,
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            "desktop encoder {} unavailable: {error}",
                            config.kind.label()
                        );
                        return;
                    }
                }
            }
            let Some(active) = encoder.as_mut() else {
                continue;
            };
            if last_tick.elapsed() < Duration::from_millis(1000 / u64::from(current.fps.max(5))) {
                continue;
            }
            last_tick = tokio::time::Instant::now();
            if let Some(frame) = desktop.latest_frame() {
                if frame.width != active.input_size().0 || frame.height != active.input_size().1 {
                    continue;
                }
                if let Err(error) = active.push(&frame.data).await {
                    tracing::warn!("desktop encoder input stopped: {error}");
                    break;
                }
            }
            while let Some(unit) = active.try_next_sample() {
                let sample = Sample {
                    data: bytes::Bytes::from(unit),
                    duration: Duration::from_millis(1000 / u64::from(current.fps.max(5))),
                    ..Default::default()
                };
                let extensions = crate::uu_native::rtp_metadata::video_extensions(0);
                if let Err(error) = track
                    .write_sample_with_extensions(active.codec(), &sample, &extensions)
                    .await
                {
                    tracing::warn!("desktop video write failed: {error}");
                    return;
                }
            }
            if !active.is_running() {
                let kind = active.config().kind;
                if kind != crate::desktop::encoder::EncoderKind::Cpu {
                    // A compiled-in hardware encoder can still fail to
                    // initialize (driver/device loss). Fall back once to the
                    // matching software encoder instead of ending the stream.
                    tracing::warn!("{} encoder stopped; falling back to software", kind.label());
                    force_cpu = true;
                    encoder = None;
                    continue;
                }
                break;
            }
        }
        tracing::info!("desktop stream ended");
    });
}

fn config_for(width: u32, height: u32, settings: VideoSettings) -> EncoderConfig {
    let mut config = EncoderConfig::new(width, height);
    config.output = settings.output;
    config.fps = settings.fps;
    config.codec = settings.codec;
    config.max_kbps = settings.max_kbps();
    config.chroma = settings.chroma;
    config.kind = crate::desktop::encoder::detect_kind();
    if !crate::desktop::encoder::full_chroma_available(config.kind) {
        config.chroma = crate::uu_native::video_settings::CHROMA_420;
    }
    config
}

async fn wait_connected(pc: &Arc<RTCPeerConnection>) -> bool {
    for _ in 0..200 {
        match pc.connection_state() {
            RTCPeerConnectionState::Connected => return true,
            RTCPeerConnectionState::Closed
            | RTCPeerConnectionState::Failed
            | RTCPeerConnectionState::Disconnected => return false,
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    false
}

fn announce(
    sender: &Sender,
    client_id: &str,
    ice_id: &str,
    app_id: &str,
    desktop: &DesktopSession,
    track: &VideoTrack,
    settings: VideoSettings,
) {
    let (width, height) = desktop.active_size();
    let screen_list = desktop.screens();
    let screens = video_settings::screen_sources_list(
        &screen_list,
        desktop.screen(),
        settings.output,
        "Linux desktop",
    );
    let full_chroma =
        crate::desktop::encoder::full_chroma_available(crate::desktop::encoder::detect_kind());
    sender.event(
        "forward_setting",
        serde_json::json!({
            "client_id": client_id,
            "data": {
                "device_capability": {
                    "display_info": screen_list.iter().map(|screen| serde_json::json!({
                        "id": screen.id,
                        "fps": settings.fps,
                        "type": 0,
                        "hdr": -1
                    })).collect::<Vec<_>>(),
                    "video_codec_capability": codec_caps(width, height, track, full_chroma),
                    "ice_id": ice_id
                },
                "type": "device_capability"
            }
        }),
    );
    sender.event(
        "forward_setting",
        serde_json::json!({
            "client_id": client_id,
            "data": {
                "sender_para_info": {
                    "capture_impl": "PipeWire",
                    "encoder_impl": crate::desktop::encoder::detect_kind().label(),
                    "ice_id": ice_id,
                    "sender_media_infos": [{
                        "capture_impl": "PipeWire",
                        "encoder_impl": crate::desktop::encoder::detect_kind().label(),
                        "video_track_index": 0
                    }],
                    "video_track_index": 0
                },
                "type": "sender_para_info"
            }
        }),
    );
    sender.event(
        "ice_finished_log",
        serde_json::json!({"ice_id": ice_id, "app_control_id": app_id}),
    );
    // These are CONTROL messages, but the existing desktop path already uses
    // the signaling channel for UU-owned capability events; the page is only
    // used by viewers that subscribe to it.
    let _ = screens;
}

fn codec_caps(
    width: u32,
    height: u32,
    track: &VideoTrack,
    full_chroma: bool,
) -> Vec<serde_json::Value> {
    let (h264, h265) = track.available();
    let mut caps = Vec::new();
    // The first entry carries the maximum size, as the official hosts do.
    for codec in [h264.then_some(1u64), h265.then_some(2u64)]
        .into_iter()
        .flatten()
    {
        for chroma in [1u64].into_iter().chain(full_chroma.then_some(3)) {
            let (cap_width, cap_height) = if caps.is_empty() {
                (width, height)
            } else {
                (0, 0)
            };
            caps.push(serde_json::json!({
                "video_codec": codec,
                "chroma_sampling": chroma,
                "codec_impl": 5,
                "bit_depth": 8,
                "width": cap_width,
                "height": cap_height
            }));
        }
    }
    caps
}
