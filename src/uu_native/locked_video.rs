//! Synthetic black video for a locked desktop.
//!
//! The official Android viewer only reaches its auto-unlock flow after it has
//! received video from the desktop target. While the session is locked we send
//! one pre-encoded black frame per second on the existing track, tagged with
//! the UU capture-index/new-frame extensions. The frame is 1280x720 and is not
//! a monitor inventory claim.

use std::process::Stdio;
use std::time::Duration;

use crate::desktop::video_track::Codec;

/// Encode one black keyframe with the configured software encoder.
pub async fn black_frame(codec: Codec, chroma: u64) -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(move || black_frame_blocking(codec, chroma))
        .await
        .map_err(|error| format!("locked video worker: {error}"))?
}

fn black_frame_blocking(codec: Codec, chroma: u64) -> Result<Vec<u8>, String> {
    let ffmpeg = std::env::var("UU_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_owned());
    let full_chroma = chroma == crate::uu_native::video_settings::CHROMA_444;
    let pixel_format = if full_chroma { "yuv444p" } else { "yuv420p" };
    let mut command = std::process::Command::new(ffmpeg);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
        "color=c=black:s=1280x720:r=1",
        "-frames:v",
        "1",
        "-an",
        "-pix_fmt",
        pixel_format,
        "-threads",
        "1",
        "-preset",
        "ultrafast",
        "-tune",
        "zerolatency",
    ]);
    match codec {
        Codec::H264 => {
            let profile = if full_chroma { "high444" } else { "baseline" };
            command.args([
                "-c:v",
                "libx264",
                "-profile:v",
                profile,
                "-x264-params",
                "keyint=1:repeat-headers=1:aud=1",
                "-f",
                "h264",
            ]);
        }
        Codec::H265 => {
            let profile = if full_chroma { "main444-8" } else { "main" };
            command.args([
                "-c:v",
                "libx265",
                "-profile:v",
                profile,
                "-x265-params",
                "keyint=1:repeat-headers=1:aud=1:pools=1:frame-threads=1:log-level=error",
                "-f",
                "hevc",
            ]);
        }
    }
    command
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = command
        .output()
        .map_err(|error| format!("locked video encoder: {error}"))?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(format!(
            "locked video encoder failed: status={}",
            output.status
        ));
    }
    Ok(output.stdout)
}

/// Cached black frame per (codec, chroma). Encoding takes a few hundred
/// milliseconds, so do it once per desktop connection.
#[derive(Default)]
pub struct BlackFrameCache {
    h264_420: Option<Vec<u8>>,
    h264_444: Option<Vec<u8>>,
    h265_420: Option<Vec<u8>>,
    h265_444: Option<Vec<u8>>,
}

impl BlackFrameCache {
    pub async fn get(&mut self, codec: Codec, chroma: u64) -> Result<Vec<u8>, String> {
        let full = chroma == crate::uu_native::video_settings::CHROMA_444;
        let slot = match (codec, full) {
            (Codec::H264, false) => &mut self.h264_420,
            (Codec::H264, true) => &mut self.h264_444,
            (Codec::H265, false) => &mut self.h265_420,
            (Codec::H265, true) => &mut self.h265_444,
        };
        if let Some(frame) = slot {
            return Ok(frame.clone());
        }
        let frame = black_frame(codec, chroma).await?;
        *slot = Some(frame.clone());
        Ok(frame)
    }
}

/// 1 fps cadence used while the desktop is locked.
pub const LOCKED_FRAME_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(test)]
mod tests {
    #[test]
    fn locked_frame_interval_is_one_second() {
        assert_eq!(
            super::LOCKED_FRAME_INTERVAL,
            std::time::Duration::from_secs(1)
        );
    }
}
