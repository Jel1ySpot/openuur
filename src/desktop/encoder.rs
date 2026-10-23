//! FFmpeg encoder bridge for desktop frames.
//!
//! Frames are tightly packed BGRA. The encoder can downscale to the viewer's
//! requested stream size and can select software, NVENC or VAAPI encoding.
//! One access unit is emitted per WebRTC sample.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use super::video_track::Codec;

pub const FPS: u32 = 60;

/// Refresh rate reported to the viewer. `UU_DISPLAY_FPS` overrides the
/// nominal value; the encoder still follows the viewer's requested fps.
pub fn display_fps() -> u32 {
    std::env::var("UU_DISPLAY_FPS")
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .map(|value| value.round().clamp(24.0, 360.0) as u32)
        .unwrap_or(FPS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderKind {
    Cpu,
    Nvenc,
    Vaapi,
    Qsv,
    Amf,
}

impl EncoderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "software",
            Self::Nvenc => "nvenc",
            Self::Vaapi => "vaapi",
            Self::Qsv => "qsv",
            Self::Amf => "amf",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    /// Pixels sent to the viewer; `None` keeps the capture size.
    pub output: Option<(u32, u32)>,
    pub fps: u32,
    pub codec: Codec,
    pub kind: EncoderKind,
    /// Bitrate cap in kbit/s; `None` uses constant quality.
    pub max_kbps: Option<u32>,
    /// Constant-quality level (NVENC `-cq` / libx264/libx265 `-crf`).
    pub cq: u32,
    /// Official ChromaFormat: 1 = 4:2:0, 3 = 4:4:4 (真彩).
    pub chroma: u64,
}

impl EncoderConfig {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            output: None,
            fps: FPS,
            codec: Codec::H264,
            kind: detect_kind(),
            max_kbps: None,
            cq: 18,
            chroma: crate::uu_native::video_settings::CHROMA_420,
        }
    }

    pub fn output_size(self) -> (u32, u32) {
        self.output.unwrap_or((self.width, self.height))
    }

    pub fn full_chroma(self) -> bool {
        self.chroma == crate::uu_native::video_settings::CHROMA_444
            && full_chroma_available(self.kind)
    }
}

pub fn detect_kind() -> EncoderKind {
    match std::env::var("UU_VIDEO_ENCODER")
        .unwrap_or_else(|_| "auto".to_owned())
        .to_ascii_lowercase()
        .as_str()
    {
        "cpu" | "software" | "libx264" => EncoderKind::Cpu,
        "nvenc" | "nvidia" => EncoderKind::Nvenc,
        "vaapi" => EncoderKind::Vaapi,
        "qsv" | "quicksync" => EncoderKind::Qsv,
        "amf" => EncoderKind::Amf,
        _ => {
            if Path::new("/proc/driver/nvidia/version").is_file()
                || Path::new("/dev/nvidia0").exists()
            {
                EncoderKind::Nvenc
            } else {
                EncoderKind::Cpu
            }
        }
    }
}

/// 4:4:4 (真彩) is implemented for NVENC and the software encoders
/// (libx264/libx265). VAAPI, QSV and AMF keep 4:2:0.
pub fn full_chroma_available(kind: EncoderKind) -> bool {
    matches!(kind, EncoderKind::Cpu | EncoderKind::Nvenc)
}

/// `(-profile:v, -pix_fmt)` for the requested chroma format.
fn chroma_args(codec: Codec, kind: EncoderKind, chroma: u64) -> (&'static str, &'static str) {
    if chroma != crate::uu_native::video_settings::CHROMA_444 || !full_chroma_available(kind) {
        let profile = match (codec, kind) {
            (Codec::H265, _) => "main",
            (Codec::H264, EncoderKind::Vaapi) => "constrained_baseline",
            (Codec::H264, _) => "baseline",
        };
        return (profile, "yuv420p");
    }
    match (codec, kind) {
        (Codec::H264, EncoderKind::Nvenc) => ("high444p", "yuv444p"),
        (Codec::H264, _) => ("high444", "yuv444p"),
        (Codec::H265, EncoderKind::Nvenc) => ("rext", "yuv444p"),
        (Codec::H265, _) => ("main444-8", "yuv444p"),
    }
}

/// Legacy entry point: H.265 when a hardware encoder is selected, unless
/// `UU_VIDEO_CODEC` forces a codec or the real probe has failed.
pub fn preferred_codec() -> Codec {
    let h265 = !hevc_probed() || hevc_available();
    crate::uu_native::video_settings::preferred_codec(true, h265)
}

/// 0 = not probed, 1 = HEVC really encodes, 2 = unavailable.
static H265_STATE: AtomicU8 = AtomicU8::new(0);
/// Last probe result and time. Failures are retried after a cooldown so a
/// system service that starts before the user's DRM ACL is granted can pick
/// HEVC up once the graphical session is ready.
static H265_PROBE: Mutex<Option<(std::time::Instant, bool)>> = Mutex::new(None);
const HEVC_FAILURE_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// HEVC is only advertised after one real frame encodes: `ffmpeg -encoders`
/// lists libx265/hevc_nvenc even when the library or driver cannot initialize.
pub async fn probe_hevc_available() -> bool {
    if let Some((at, available)) = *H265_PROBE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        if available || at.elapsed() < HEVC_FAILURE_RETRY {
            return available;
        }
    }
    let requested = detect_kind();
    let mut candidates = vec![requested];
    if requested != EncoderKind::Cpu {
        candidates.push(EncoderKind::Cpu);
    }
    let mut available = false;
    for kind in candidates {
        if probe_hevc_kind(kind).await {
            tracing::info!("HEVC encoder probe: {} ready", kind.label());
            available = true;
            break;
        }
    }
    if !available {
        tracing::info!("HEVC encoder probe: unavailable; advertising H.264 only");
    }
    H265_STATE.store(if available { 1 } else { 2 }, Ordering::Release);
    *H265_PROBE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some((std::time::Instant::now(), available));
    available
}

/// Cached result of [`probe_hevc_available`]. Unknown counts as unavailable
/// until the host has performed the probe.
pub fn hevc_available() -> bool {
    H265_STATE.load(Ordering::Acquire) == 1
}

pub fn hevc_probed() -> bool {
    H265_STATE.load(Ordering::Acquire) != 0
}

fn has_irap(data: &[u8]) -> bool {
    let mut index = 0;
    while index + 3 < data.len() {
        let prefix = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        if let Some(kind) = hevc_nal_kind(data, index + prefix) {
            // BLA/IDR/CRA are IRAP pictures.
            if (16..=21).contains(&kind) {
                return true;
            }
        }
        index += prefix;
    }
    false
}

async fn probe_hevc_kind(kind: EncoderKind) -> bool {
    let mut config = EncoderConfig::new(256, 256);
    config.codec = Codec::H265;
    config.kind = kind;
    config.fps = 1;
    let ffmpeg = std::env::var("UU_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_owned());
    let mut command = Command::new(ffmpeg);
    command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if kind == EncoderKind::Vaapi {
        command.args([
            "-vaapi_device",
            &std::env::var("UU_VAAPI_DEVICE").unwrap_or_else(|_| "/dev/dri/renderD128".to_owned()),
        ]);
    }
    command.args([
        "-f",
        "lavfi",
        "-i",
        "color=black:size=256x256:rate=1",
        "-frames:v",
        "1",
        "-an",
    ]);
    if kind == EncoderKind::Vaapi {
        command.args(["-vf", "format=nv12,hwupload"]);
    }
    command.args(codec_args(config, "30"));
    command
        .args(["-f", "hevc", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(std::time::Duration::from_secs(5), command.output()).await {
        Ok(Ok(output)) => output.status.success() && has_irap(&output.stdout),
        _ => false,
    }
}

/// One FFmpeg codec argument list shared by the stream encoder and the
/// one-frame HEVC probe. The caller appends `-f <container> pipe:1`.
fn codec_args(config: EncoderConfig, gop: &str) -> Vec<String> {
    let maxrate = config
        .max_kbps
        .map(|k| format!("{k}k"))
        .unwrap_or_else(|| "20M".to_owned());
    let target = config
        .max_kbps
        .map(|k| format!("{}k", k * 3 / 4))
        .unwrap_or_else(|| "16M".to_owned());
    let buffer = config
        .max_kbps
        .map(|k| format!("{}k", (k * 2).clamp(500, 40_000)))
        .unwrap_or_else(|| "40M".to_owned());
    let cq = config.cq.to_string();
    let (profile, pix_fmt) = chroma_args(config.codec, config.kind, config.chroma);
    let mut args: Vec<String> = Vec::new();
    match (config.kind, config.codec) {
        (EncoderKind::Cpu, Codec::H264) => {
            args.extend(
                [
                    "-c:v",
                    "libx264",
                    "-preset",
                    "superfast",
                    "-tune",
                    "zerolatency",
                    "-profile:v",
                    profile,
                    "-pix_fmt",
                    pix_fmt,
                    "-g",
                    gop,
                    "-threads",
                    "4",
                    "-maxrate",
                    &maxrate,
                    "-bufsize",
                    &buffer,
                ]
                .map(str::to_owned),
            );
            if config.max_kbps.is_none() {
                args.extend(["-crf".to_owned(), cq]);
            } else {
                args.extend(["-b:v".to_owned(), target]);
            }
            args.extend(
                [
                    "-x264-params",
                    &format!("aud=1:repeat-headers=1:scenecut=0:keyint={gop}:min-keyint={gop}"),
                    "-flush_packets",
                    "1",
                ]
                .map(str::to_owned),
            );
        }
        (EncoderKind::Cpu, Codec::H265) => {
            args.extend(
                [
                    "-c:v",
                    "libx265",
                    "-preset",
                    "superfast",
                    "-tune",
                    "zerolatency",
                    "-profile:v",
                    profile,
                    "-pix_fmt",
                    pix_fmt,
                    "-g",
                    gop,
                    "-threads",
                    "4",
                    "-maxrate",
                    &maxrate,
                    "-bufsize",
                    &buffer,
                ]
                .map(str::to_owned),
            );
            if config.max_kbps.is_none() {
                args.extend(["-crf".to_owned(), cq]);
            } else {
                args.extend(["-b:v".to_owned(), target]);
            }
            args.extend(
                [
                    "-x265-params",
                    &format!(
                        "aud=1:repeat-headers=1:scenecut=0:keyint={gop}:min-keyint={gop}:bframes=0:rc-lookahead=0"
                    ),
                    "-flush_packets", "1",
                ]
                .map(str::to_owned),
            );
        }
        (EncoderKind::Nvenc, _) => {
            let encoder = if config.codec == Codec::H265 {
                "hevc_nvenc"
            } else {
                "h264_nvenc"
            };
            args.extend(
                [
                    "-c:v",
                    encoder,
                    "-preset",
                    "p4",
                    "-tune",
                    "ull",
                    "-profile:v",
                    profile,
                    "-pix_fmt",
                    pix_fmt,
                    "-g",
                    gop,
                    "-bf",
                    "0",
                    "-rc",
                    "vbr",
                    "-cq",
                    &cq,
                    "-b:v",
                    &target,
                    "-maxrate",
                    &maxrate,
                    "-bufsize",
                    &buffer,
                    "-rc-lookahead",
                    "0",
                    "-zerolatency",
                    "1",
                    "-aud",
                    "1",
                ]
                .map(str::to_owned),
            );
        }
        (EncoderKind::Vaapi, _) => {
            let encoder = if config.codec == Codec::H265 {
                "hevc_vaapi"
            } else {
                "h264_vaapi"
            };
            args.extend(
                [
                    "-c:v",
                    encoder,
                    "-profile:v",
                    profile,
                    "-g",
                    gop,
                    "-bf",
                    "0",
                    "-rc_mode",
                    "VBR",
                    "-b:v",
                    &target,
                    "-maxrate",
                    &maxrate,
                    "-bufsize",
                    &buffer,
                    "-qmin",
                    &cq,
                    "-async_depth",
                    "1",
                ]
                .map(str::to_owned),
            );
            if config.codec == Codec::H264 {
                args.extend(["-aud".to_owned(), "1".to_owned()]);
            }
        }
        (EncoderKind::Qsv, _) => {
            let encoder = if config.codec == Codec::H265 {
                "hevc_qsv"
            } else {
                "h264_qsv"
            };
            args.extend(
                [
                    "-c:v",
                    encoder,
                    "-preset",
                    "veryfast",
                    "-profile:v",
                    "main",
                    "-g",
                    gop,
                    "-bf",
                    "0",
                    "-b:v",
                    &target,
                    "-maxrate",
                    &maxrate,
                    "-bufsize",
                    &buffer,
                ]
                .map(str::to_owned),
            );
        }
        (EncoderKind::Amf, _) => {
            let encoder = if config.codec == Codec::H265 {
                "hevc_amf"
            } else {
                "h264_amf"
            };
            args.extend(
                [
                    "-c:v",
                    encoder,
                    "-quality",
                    "speed",
                    "-profile:v",
                    "main",
                    "-g",
                    gop,
                    "-bf",
                    "0",
                    "-b:v",
                    &target,
                    "-maxrate",
                    &maxrate,
                    "-bufsize",
                    &buffer,
                ]
                .map(str::to_owned),
            );
        }
    }
    args
}

fn container(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264",
        Codec::H265 => "hevc",
    }
}

pub struct Encoder {
    child: Child,
    stdin: ChildStdin,
    samples: mpsc::Receiver<Vec<u8>>,
    config: EncoderConfig,
    codec: Codec,
}

impl Encoder {
    pub fn start(width: u32, height: u32) -> Result<Self, String> {
        Self::start_config(EncoderConfig::new(width, height))
    }

    pub fn start_config(mut config: EncoderConfig) -> Result<Self, String> {
        if config.width == 0 || config.height == 0 || config.width > 8192 || config.height > 8192 {
            return Err("invalid desktop dimensions for encoder".to_owned());
        }
        if config.fps == 0 || config.fps > 144 {
            return Err("invalid encoder frame rate".to_owned());
        }
        if !full_chroma_available(config.kind) {
            config.chroma = crate::uu_native::video_settings::CHROMA_420;
        }
        let ffmpeg = std::env::var("UU_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_owned());
        let (out_w, out_h) = config.output_size();
        let odd_native = config.output.is_none()
            && (!config.width.is_multiple_of(2) || !config.height.is_multiple_of(2));
        let mut command = Command::new(ffmpeg);
        command
            .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .args(["-f", "rawvideo", "-pix_fmt", "bgra"])
            .args([
                "-s",
                &format!("{}x{}", config.width, config.height),
                "-r",
                &config.fps.to_string(),
            ])
            .arg("-i")
            .arg("pipe:0")
            .arg("-an");
        if config.kind == EncoderKind::Vaapi {
            command.args([
                "-vaapi_device",
                &std::env::var("UU_VAAPI_DEVICE")
                    .unwrap_or_else(|_| "/dev/dri/renderD128".to_owned()),
            ]);
        }
        let mut filters = Vec::new();
        if (out_w, out_h) != (config.width, config.height) && config.output.is_some() {
            filters.push(format!("scale={out_w}:{out_h}:flags=bilinear"));
        } else if odd_native {
            // H.264/HEVC 4:2:0 encoders and NVENC surface allocation require
            // even dimensions; portal geometry can be odd on scaled desktops.
            filters.push("pad=ceil(iw/2)*2:ceil(ih/2)*2:0:0:black".to_owned());
        }
        if config.kind == EncoderKind::Vaapi {
            // VAAPI is 4:2:0 only and uploads software frames after scaling.
            filters.push("format=nv12".to_owned());
            filters.push("hwupload".to_owned());
        }
        if !filters.is_empty() {
            command.args(["-vf", &filters.join(",")]);
        }
        let gop = config.fps.clamp(1, 60).to_string();
        let arguments = codec_args(config, &gop);
        command.args(&arguments);
        command.args(["-f", container(config.codec), "pipe:1"]);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start ffmpeg: {e}"))?;
        let stdin = child.stdin.take().ok_or("ffmpeg stdin unavailable")?;
        let mut stdout = child.stdout.take().ok_or("ffmpeg stdout unavailable")?;
        let stderr = child.stderr.take();
        if let Some(mut stderr) = stderr {
            tokio::spawn(async move {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text).await;
                if !text.trim().is_empty() {
                    tracing::warn!("desktop encoder: {}", text.trim());
                }
            });
        }
        let (tx, rx) = mpsc::channel(8);
        let codec = config.codec;
        tokio::spawn(async move {
            let mut pending = Vec::<u8>::with_capacity(256 * 1024);
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let n = match stdout.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                pending.extend_from_slice(&buffer[..n]);
                for unit in split_access_units(&mut pending, codec, false) {
                    if tx.send(unit).await.is_err() {
                        return;
                    }
                }
                if pending.len() > 16 * 1024 * 1024 {
                    pending.clear();
                }
            }
            for unit in split_access_units(&mut pending, codec, true) {
                let _ = tx.send(unit).await;
            }
        });
        Ok(Self {
            child,
            stdin,
            samples: rx,
            config,
            codec,
        })
    }

    pub fn input_size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn size(&self) -> (u32, u32) {
        self.config.output_size()
    }

    pub fn fps(&self) -> u32 {
        self.config.fps
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn config(&self) -> EncoderConfig {
        self.config
    }

    pub async fn push(&mut self, frame: &[u8]) -> Result<(), String> {
        let (width, height) = self.input_size();
        let expected = width as usize * height as usize * 4;
        if frame.len() != expected {
            return Err(format!(
                "frame is {} bytes, encoder expects {expected}",
                frame.len()
            ));
        }
        self.stdin
            .write_all(frame)
            .await
            .map_err(|e| format!("ffmpeg input: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("ffmpeg input: {e}"))?;
        Ok(())
    }

    pub async fn next_sample(&mut self) -> Option<Vec<u8>> {
        self.samples.recv().await
    }

    pub fn try_next_sample(&mut self) -> Option<Vec<u8>> {
        self.samples.try_recv().ok()
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// Access units: H.264 is split on AUD (type 9); H.265 uses picture
/// boundaries so `hevc_vaapi` (which writes no AUD for P-frames) still
/// produces one RTP sample per picture.
fn split_access_units(pending: &mut Vec<u8>, codec: Codec, flush: bool) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let positions = access_unit_positions(pending, codec);
        if positions.len() < 2 {
            break;
        }
        let end = positions[1];
        out.push(pending.drain(..end).collect());
    }
    if flush && !pending.is_empty() {
        out.push(std::mem::take(pending));
    }
    out
}

fn access_unit_positions(data: &[u8], codec: Codec) -> Vec<usize> {
    match codec {
        Codec::H264 => aud_positions(data),
        Codec::H265 => hevc_au_positions(data),
    }
}

fn aud_positions(data: &[u8]) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut index = 0;
    while index + 5 <= data.len() {
        let prefix = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        if data.get(index + prefix) == Some(&9) {
            positions.push(index);
        }
        index += prefix;
    }
    positions
}

/// Validated HEVC NAL type from the two-byte header.
fn hevc_nal_kind(data: &[u8], header: usize) -> Option<u8> {
    (header + 1 < data.len() && data[header] & 0x80 == 0 && data[header + 1] & 0x07 != 0)
        .then(|| (data[header] >> 1) & 63)
}

/// RFC 7798 §3.1: a new access unit starts at the first NAL after a VCL NAL
/// that is either a parameter set/AUD/prefix SEI or starts a new picture.
fn hevc_au_positions(data: &[u8]) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut previous_vcl = false;
    let mut first = true;
    let mut index = 0;
    while index + 3 < data.len() {
        let prefix = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        let header = index + prefix;
        if let Some(kind) = hevc_nal_kind(data, header) {
            let vcl = kind < 32;
            let starts_picture = vcl && data.get(header + 2).is_some_and(|byte| byte & 0x80 != 0);
            // Only these non-VCL types may open the next access unit.
            let opens_unit = matches!(kind, 32..=35 | 39 | 41..=44 | 48..=55);
            if first || (previous_vcl && (opens_unit || starts_picture)) {
                positions.push(index);
            }
            // Suffix SEI, filler and end markers stay with the picture.
            if vcl || opens_unit {
                previous_vcl = vcl;
            }
            first = false;
        }
        index += prefix;
    }
    positions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a local ffmpeg with libx264"]
    async fn encodes_synthetic_bgra_frames() {
        let mut encoder = Encoder::start(64, 48).unwrap();
        let frame = vec![32u8; 64 * 48 * 4];
        for _ in 0..4 {
            encoder.push(&frame).await.unwrap();
        }
        let sample = tokio::time::timeout(std::time::Duration::from_secs(5), encoder.next_sample())
            .await
            .expect("encoder output timed out")
            .expect("encoder output missing");
        assert!(sample.windows(4).any(|w| w == [0, 0, 0, 1]));
    }

    #[tokio::test]
    #[ignore = "requires a local ffmpeg with libx265"]
    async fn encodes_synthetic_hevc_frames() {
        let mut config = EncoderConfig::new(64, 48);
        config.codec = Codec::H265;
        config.kind = EncoderKind::Cpu;
        let mut encoder = Encoder::start_config(config).unwrap();
        let frame = vec![64u8; 64 * 48 * 4];
        for _ in 0..6 {
            encoder.push(&frame).await.unwrap();
        }
        let sample = tokio::time::timeout(std::time::Duration::from_secs(5), encoder.next_sample())
            .await
            .expect("HEVC encoder output timed out")
            .expect("HEVC encoder output missing");
        assert!(sample.windows(5).any(|w| w == [0, 0, 0, 1, 0x40]));
    }

    #[test]
    fn splits_on_aud_without_losing_prefix() {
        let mut data = vec![0, 0, 0, 1, 9, 1, 2, 0, 0, 0, 1, 9, 3, 4];
        let units = split_access_units(&mut data, Codec::H264, true);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0], vec![0, 0, 0, 1, 9, 1, 2]);
        assert_eq!(units[1], vec![0, 0, 0, 1, 9, 3, 4]);
        assert!(data.is_empty());
    }

    #[test]
    fn cq_and_crf_rate_control() {
        let mut config = EncoderConfig::new(1280, 720);
        config.kind = EncoderKind::Cpu;
        config.codec = Codec::H264;
        config.max_kbps = None;
        config.cq = 23;
        let args = codec_args(config, "60").join(" ");
        assert!(args.contains("-crf 23"), "{args}");
        assert!(!args.contains("-cq"), "{args}");

        config.kind = EncoderKind::Nvenc;
        let args = codec_args(config, "60").join(" ");
        assert!(args.contains("-cq 23"), "{args}");
        assert!(args.contains("-rc vbr"), "{args}");

        config.kind = EncoderKind::Cpu;
        config.codec = Codec::H265;
        config.max_kbps = Some(8_000);
        let args = codec_args(config, "60").join(" ");
        assert!(args.contains("-b:v 6000k"), "{args}");
        assert!(!args.contains("-crf"), "{args}");
    }

    #[test]
    fn cpu_hevc_probe_arguments_are_production_arguments() {
        let mut config = EncoderConfig::new(256, 256);
        config.codec = Codec::H265;
        config.kind = EncoderKind::Cpu;
        let args = codec_args(config, "30").join(" ");
        assert!(args.contains("-c:v libx265"), "{args}");
        assert!(args.contains("-tune zerolatency"), "{args}");
    }

    #[test]
    fn selects_h265_and_scaled_output() {
        let mut config = EncoderConfig::new(1920, 1080);
        config.output = Some((1280, 720));
        config.fps = 60;
        config.codec = Codec::H265;
        assert_eq!(config.output_size(), (1280, 720));
        assert_eq!(config.codec, Codec::H265);
    }
}
