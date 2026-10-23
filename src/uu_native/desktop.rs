//! Native UU desktop stream: video/audio tracks and portal input wiring.

use super::signal::Outbox;
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

/// Minimum gap between encoder restarts triggered by RTCP keyframe requests.
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(500);

pub async fn add_track(pc: &Arc<RTCPeerConnection>) -> Result<Arc<VideoTrack>, String> {
    let track = Arc::new(VideoTrack::new());
    let sender = pc
        .add_track(track.clone())
        .await
        .map_err(|error| format!("add video track: {error}"))?;
    spawn_rtcp_feedback(pc.clone(), sender, track.clone());
    Ok(track)
}

/// Read RTCP for the outgoing video track and record keyframe requests. The
/// official viewer sends PLI (and sometimes FIR) after packet loss; without a
/// reader the host only recovers at the next GOP boundary.
fn spawn_rtcp_feedback(
    pc: Arc<RTCPeerConnection>,
    sender: Arc<webrtc::rtp_transceiver::rtp_sender::RTCRtpSender>,
    track: Arc<VideoTrack>,
) {
    use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
    use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
    // Only a weak reference: a blocked `read_rtcp` must not keep the whole
    // peer connection alive after the viewer goes away.
    let state = Arc::downgrade(&pc);
    let task = tokio::spawn(async move {
        loop {
            match sender.read_rtcp().await {
                Ok((packets, _)) => {
                    if packets.iter().any(|packet| {
                        packet
                            .as_any()
                            .downcast_ref::<PictureLossIndication>()
                            .is_some()
                            || packet.as_any().downcast_ref::<FullIntraRequest>().is_some()
                    }) {
                        track.request_keyframe();
                    }
                }
                Err(error) => {
                    let Some(pc) = state.upgrade() else {
                        break;
                    };
                    let closed = matches!(
                        pc.connection_state(),
                        RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed
                    );
                    drop(pc);
                    if closed {
                        break;
                    }
                    tracing::debug!("video RTCP reader interrupted: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
    // `read_rtcp` can stay blocked on a sender that never started (an offer
    // that is cancelled before negotiation). Abort it once the peer is gone
    // so the connection, track, and scopes are released.
    let abort = task.abort_handle();
    let state = Arc::downgrade(&pc);
    tokio::spawn(async move {
        loop {
            let Some(pc) = state.upgrade() else {
                abort.abort();
                break;
            };
            let closed = matches!(
                pc.connection_state(),
                RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed
            );
            drop(pc);
            if closed {
                abort.abort();
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
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
    outbox: Outbox,
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
        let h265_ready = crate::desktop::encoder::probe_hevc_available().await && available_h265;
        let initial = *settings.borrow();
        let preferred = video_settings::preferred_codec(available_h264, h265_ready);
        let initial = if initial.codec == Codec::H265 && h265_ready {
            initial
        } else {
            VideoSettings {
                codec: preferred,
                ..initial
            }
        };
        let mut current = if initial.codec == Codec::H265 && h265_ready {
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
            &outbox, &client_id, &ice_id, &app_id, &desktop, &track, current,
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
        let mut last_keyframe: Option<std::time::Instant> = None;
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
            let next = if next.codec == Codec::H265 && h265_ready {
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
                let codec = if next.codec == Codec::H265 && h265_ready {
                    Codec::H265
                } else if available_h264 {
                    Codec::H264
                } else if h265_ready {
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
            // RTCP PLI/FIR: ffmpeg has no control socket to force an intra
            // frame, so restart the encoder to emit one immediately. Coalesce
            // bursts; each restart costs one process spawn.
            let mut keyframe_restart = false;
            // Evaluate the cheap gates first: `take_keyframe_request` consumes
            // the flag, so a request that arrives inside the throttle window
            // must stay pending until the window passes.
            if encoder.is_some()
                && last_keyframe.is_none_or(|at| at.elapsed() >= KEYFRAME_MIN_INTERVAL)
                && track.take_keyframe_request()
            {
                tracing::debug!("viewer requested a keyframe; restarting the encoder");
                encoder = None;
                last_keyframe = Some(std::time::Instant::now());
                keyframe_restart = true;
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
                // A restart caused by the capture size also needs a fresh
                // settings sync, even if VideoSettings did not change.
                let config_changed = encoder
                    .as_ref()
                    .is_some_and(|encoder| encoder.config() != config);
                if encoder.is_some() {
                    tracing::info!("restarting desktop encoder: {config:?}");
                }
                match Encoder::start_config(config) {
                    Ok(started) => {
                        encoder = Some(started);
                        // A fresh encoder always starts with a keyframe.
                        let _ = track.take_keyframe_request();
                        let changed = next != current;
                        current = next;
                        // A keyframe restart keeps the announced settings;
                        // configuration changes (including a new native size)
                        // need a new sync.
                        if !keyframe_restart || changed || config_changed {
                            announce(
                                &outbox, &client_id, &ice_id, &app_id, &desktop, &track, current,
                            );
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            "desktop encoder {} unavailable: {error}",
                            config.kind.label()
                        );
                        if config.kind != crate::desktop::encoder::EncoderKind::Cpu {
                            // The real HEVC probe may have succeeded through
                            // the software fallback while this hardware
                            // encoder cannot initialize. Retry once with the
                            // matching libx264/libx265 encoder.
                            force_cpu = true;
                            encoder = None;
                            continue;
                        }
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
    config.cq = settings.cq();
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
    outbox: &Outbox,
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
    outbox.event(
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
    outbox.event(
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
    outbox.event(
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
    let h265 = h265 && crate::desktop::encoder::hevc_available();
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
