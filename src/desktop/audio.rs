//! Desktop system audio as Opus over the viewer's audio m-line.
//!
//! FFmpeg captures the PulseAudio/PipeWire monitor source and emits Ogg/Opus
//! packets. `webrtc`'s OggReader turns them into one RTP sample per 20 ms.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use webrtc::media::io::ogg_reader::OggReader;
use webrtc::media::Sample;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

const FRAME: Duration = Duration::from_millis(20);

pub fn enabled() -> bool {
    std::env::var("UU_AUDIO").map_or(true, |value| value != "0")
}

pub fn source() -> String {
    super::virtual_audio::source()
}

struct Stopper {
    stop: Arc<AtomicBool>,
    child: Option<Child>,
}

impl Drop for Stopper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn start_encoder(source: &str) -> Result<Child, String> {
    let ffmpeg = std::env::var("UU_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_owned());
    Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "pulse",
            "-fragment_size",
            "1920",
            "-i",
            source,
            "-vn",
            "-c:a",
            "libopus",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-b:a",
            "128k",
            "-application",
            "lowdelay",
            "-frame_duration",
            "20",
            "-page_duration",
            "20000",
            "-flush_packets",
            "1",
            "-f",
            "ogg",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("start audio encoder: {error}"))
}

fn read_packets(
    stdout: impl Read,
    tx: &tokio::sync::mpsc::Sender<bytes::Bytes>,
) -> Result<(), String> {
    let (mut reader, _) =
        OggReader::new(stdout, false).map_err(|error| format!("audio Ogg header: {error}"))?;
    loop {
        let (payload, _) = reader
            .parse_next_page()
            .map_err(|error| format!("audio Ogg page: {error}"))?;
        if payload.starts_with(b"OpusTags") {
            continue;
        }
        if tx.blocking_send(payload.freeze()).is_err() {
            return Ok(());
        }
    }
}

fn start(
    source: &str,
    tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
) -> Result<(Stopper, tokio::task::JoinHandle<Result<(), String>>), String> {
    let mut child = start_encoder(source)?;
    let stdout = child.stdout.take().ok_or("audio encoder stdout missing")?;
    let stop = Arc::new(AtomicBool::new(false));
    let job = tokio::task::spawn_blocking(move || read_packets(stdout, &tx));
    Ok((
        Stopper {
            stop,
            child: Some(child),
        },
        job,
    ))
}

fn closed(pc: &RTCPeerConnection) -> bool {
    matches!(
        pc.connection_state(),
        RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed
    )
}

pub async fn stream(pc: Arc<RTCPeerConnection>, track: Arc<TrackLocalStaticSample>) {
    while pc.connection_state() != RTCPeerConnectionState::Connected {
        if closed(&pc) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut failures = 0u32;
    while !closed(&pc) {
        let current_source = source();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(25);
        let (stopper, job) = match start(&current_source, tx) {
            Ok(started) => started,
            Err(error) => {
                tracing::warn!("{error}");
                return;
            }
        };
        tracing::info!("desktop audio started: opus 48k stereo source={current_source}");
        let mut packets = 0u64;
        let mut health = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                packet = rx.recv() => {
                    let Some(data) = packet else { break; };
                    if let Err(error) = track.write_sample(&Sample { data, duration: FRAME, ..Default::default() }).await {
                        tracing::warn!("desktop audio send: {error}");
                        break;
                    }
                    packets += 1;
                }
                _ = health.tick() => {
                    if closed(&pc) || source() != current_source { break; }
                }
            }
        }
        drop(rx);
        drop(stopper);
        let result = job.await.unwrap_or_else(|error| Err(error.to_string()));
        if closed(&pc) {
            break;
        }
        if source() != current_source {
            failures = 0;
            continue;
        }
        tracing::warn!(
            "desktop audio stopped after {packets} packets: {:?}",
            result.err()
        );
        failures = if packets > 250 { 0 } else { failures + 1 };
        if failures >= 5 {
            tracing::warn!("desktop audio disabled for this connection");
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    tracing::info!("desktop audio ended");
}

pub fn track() -> Arc<TrackLocalStaticSample> {
    Arc::new(TrackLocalStaticSample::new(
        webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability {
            mime_type: "audio/opus".to_owned(),
            clock_rate: 48_000,
            channels: 2,
            ..Default::default()
        },
        "audio_0".to_owned(),
        "audio_0".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a local PulseAudio/PipeWire monitor source"]
    async fn captures_opus_packets_from_the_default_monitor() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (stopper, job) = start(&source(), tx).unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("audio capture timed out");
        assert!(packet.is_some_and(|packet| !packet.is_empty()));
        drop(rx);
        drop(stopper);
        let _ = job.await;
    }
}
