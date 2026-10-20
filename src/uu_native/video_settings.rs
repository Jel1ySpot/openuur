//! Viewer capture settings and UU screen/codec control messages.
//!
//! The official viewer sends `RpcRequest` cases 2, 3, 4 and 6 for capture
//! settings, FPS, quality and resolution. The host answers with matching
//! `RpcResponse` cases and can report the applied state with
//! `CaptureSettingSync` (message 25).

use super::wire::{self, Field};
use crate::desktop::video_track::Codec;
use tokio::sync::watch;

pub const QUALITY_FAST: u64 = 1;
pub const QUALITY_GENERAL: u64 = 2;
pub const QUALITY_HD: u64 = 3;
pub const QUALITY_BLURAY: u64 = 4;
pub const QUALITY_AUTO: u64 = 5;
pub const QUALITY_CUSTOM: u64 = 6;
pub const CHROMA_420: u64 = 1;
pub const CHROMA_444: u64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSettings {
    pub codec: Codec,
    pub fps: u32,
    pub quality: u64,
    pub custom_mbps: u32,
    pub output: Option<(u32, u32)>,
    pub chroma: u64,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            codec: Codec::H264,
            fps: 60,
            quality: QUALITY_AUTO,
            custom_mbps: 0,
            output: None,
            chroma: CHROMA_420,
        }
    }
}

impl VideoSettings {
    pub fn cq(self) -> u32 {
        match self.quality {
            QUALITY_BLURAY => 17,
            QUALITY_HD => 21,
            QUALITY_GENERAL => 26,
            QUALITY_FAST => 30,
            _ => 18,
        }
    }

    pub fn max_kbps(self) -> Option<u32> {
        match self.quality {
            QUALITY_FAST => Some(1_000),
            QUALITY_GENERAL => Some(2_000),
            QUALITY_HD => Some(8_000),
            QUALITY_BLURAY => Some(20_000),
            QUALITY_CUSTOM if self.custom_mbps > 0 => Some(
                if self.custom_mbps > 1_000 {
                    self.custom_mbps
                } else {
                    self.custom_mbps * 1_000
                }
                .min(100_000),
            ),
            _ => None,
        }
    }

    pub fn output_size(self, native: (u32, u32)) -> (u32, u32) {
        self.output.unwrap_or(native)
    }
}

pub fn preferred_codec(h264: bool, h265: bool) -> Codec {
    let forced_h264 =
        std::env::var("UU_VIDEO_CODEC").is_ok_and(|value| value.eq_ignore_ascii_case("h264"));
    if h265 && !(forced_h264 && h264) {
        Codec::H265
    } else {
        Codec::H264
    }
}

pub fn scaled_sizes(native: (u32, u32)) -> Vec<(u32, u32)> {
    [5u32, 4, 3, 2]
        .iter()
        .map(|sixths| ((native.0 * sixths / 6) & !1, (native.1 * sixths / 6) & !1))
        .filter(|(width, height)| *width >= 640 && *height >= 360)
        .collect()
}

fn clamp_output(native: (u32, u32), width: u64, height: u64) -> Option<(u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    if width >= u64::from(native.0) || height >= u64::from(native.1) {
        return None;
    }
    Some(((width as u32).max(320) & !1, (height as u32).max(180) & !1))
}

fn from_option(value: u64) -> Option<u64> {
    match value {
        0 => None,
        1 => Some(30),
        2 => Some(60),
        3 => Some(90),
        4 => Some(144),
        value => Some(value),
    }
}

/// Apply an `RpcRequest` body. Returns the `RpcResponse` body when this was a
/// settings request.
pub fn apply_rpc(
    rpc: &[Field],
    settings: &watch::Sender<VideoSettings>,
    native: (u32, u32),
    _h264: bool,
    h265: bool,
) -> Option<Vec<u8>> {
    let header = wire::blob(rpc, 1).unwrap_or_default();
    let mut next = *settings.borrow();
    if let Some(request) = wire::blob(rpc, 2) {
        let fields = wire::decode(request)?;
        if wire::uint(&fields, 13) == Some(2) && h265 {
            next.codec = Codec::H265;
        }
        if let Some(fps) = wire::uint(&fields, 18)
            .filter(|value| *value > 0)
            .or_else(|| wire::uint(&fields, 1).and_then(from_option))
        {
            next.fps = fps as u32;
        }
        if let Some(quality) = wire::uint(&fields, 2).filter(|value| *value > 0) {
            next.quality = quality;
        }
        if let Some(bitrate) = wire::uint(&fields, 8).filter(|value| *value > 0) {
            next.custom_mbps = bitrate as u32;
        }
        match wire::uint(&fields, 7) {
            Some(CHROMA_444)
                if crate::desktop::encoder::full_chroma_available(
                    crate::desktop::encoder::detect_kind(),
                ) =>
            {
                next.chroma = CHROMA_444
            }
            Some(CHROMA_444) => {
                tracing::info!(
                    "chroma 4:4:4 requested but this encoder pipeline only supports 4:2:0"
                );
                next.chroma = CHROMA_420;
            }
            Some(CHROMA_420) => next.chroma = CHROMA_420,
            _ => {}
        }
        if wire::uint(&fields, 10) == Some(3) {
            next.output = None;
        } else if let Some((width, height)) = wire::uint(&fields, 16)
            .zip(wire::uint(&fields, 17))
            .or_else(|| wire::uint(&fields, 5).zip(wire::uint(&fields, 6)))
        {
            next.output = clamp_output(native, width, height);
        }
    } else if let Some(request) = wire::blob(rpc, 6) {
        let fields = wire::decode(request)?;
        let width = wire::uint(&fields, 1).unwrap_or(0);
        let height = wire::uint(&fields, 2).unwrap_or(0);
        next.output = clamp_output(native, width, height);
    } else if let Some(request) = wire::blob(rpc, 3) {
        let fields = wire::decode(request)?;
        if let Some(fps) = wire::uint(&fields, 1).and_then(from_option) {
            next.fps = fps as u32;
        }
    } else {
        let request = wire::blob(rpc, 4)?;
        let fields = wire::decode(request)?;
        if let Some(quality) = wire::uint(&fields, 1).filter(|value| *value > 0) {
            next.quality = quality;
        }
    }
    next.fps = next.fps.clamp(5, 144);
    settings.send_if_modified(|current| {
        if *current != next {
            tracing::info!(
                "desktop settings: codec={:?} fps={} quality={} max_kbps={:?} output={:?}",
                next.codec,
                next.fps,
                next.quality,
                next.max_kbps(),
                next.output
            );
            *current = next;
            true
        } else {
            false
        }
    });
    let mut body = Vec::new();
    wire::bytes(1, header, &mut body);
    wire::bytes(2, &[], &mut body);
    Some(body)
}

fn put_int(field: u32, value: u64, out: &mut Vec<u8>) {
    wire::int(field, value, out);
}

fn put_bytes(field: u32, value: &[u8], out: &mut Vec<u8>) {
    wire::bytes(field, value, out);
}

/// `Message` field 7 `ScreenSources` payload.
pub fn screen_sources(width: u32, height: u32, output: Option<(u32, u32)>, name: &str) -> Vec<u8> {
    let mut screen = Vec::new();
    put_int(1, 0, &mut screen);
    put_int(2, 60, &mut screen);
    let rect = |width: i32, height: i32| {
        let mut out = Vec::new();
        put_int(3, width as u64, &mut out);
        put_int(4, height as u64, &mut out);
        put_int(5, width as u64, &mut out);
        put_int(6, height as u64, &mut out);
        out
    };
    put_bytes(3, &rect(width as i32, height as i32), &mut screen);
    for (scaled_width, scaled_height) in scaled_sizes((width, height)) {
        put_bytes(
            3,
            &rect(scaled_width as i32, scaled_height as i32),
            &mut screen,
        );
    }
    let current = output.unwrap_or((width, height));
    put_bytes(4, &rect(current.0 as i32, current.1 as i32), &mut screen);
    put_bytes(6, &rect(width as i32, height as i32), &mut screen);
    put_int(7, 1, &mut screen);
    screen.push(65);
    screen.extend_from_slice(&1.0f64.to_le_bytes());
    if !name.is_empty() {
        put_bytes(10, name.as_bytes(), &mut screen);
    }
    put_int(12, 0, &mut screen);
    let mut payload = Vec::new();
    put_bytes(1, &screen, &mut payload);
    payload
}

/// `Message` field 18 `CodecNegotiation` payload.
pub fn codec_negotiation(width: u32, height: u32, fps: u32, h265: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    for codec in [Some(Codec::H264), h265.then_some(Codec::H265)]
        .into_iter()
        .flatten()
    {
        let mut cap = Vec::new();
        put_int(1, 1, &mut cap);
        put_int(2, u64::from(fps), &mut cap);
        put_int(3, codec.wire(), &mut cap);
        put_int(4, u64::from(width), &mut cap);
        put_int(5, u64::from(height), &mut cap);
        put_bytes(1, &cap, &mut payload);
    }
    payload
}

/// `Message` field 25 `CaptureSettingSync` payload.
pub fn sync_payload(settings: VideoSettings, native: (u32, u32)) -> Vec<u8> {
    let size = settings.output_size(native);
    let mut out = Vec::new();
    put_int(1, u64::from(settings.fps), &mut out);
    put_int(2, settings.quality, &mut out);
    put_int(3, settings.chroma, &mut out);
    if settings.custom_mbps > 0 {
        put_int(4, u64::from(settings.custom_mbps), &mut out);
    }
    put_int(5, u64::from(size.0), &mut out);
    put_int(6, u64::from(size.1), &mut out);
    put_int(9, u64::from(size.0), &mut out);
    put_int(10, u64::from(size.1), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(request_id: u64, case: u32, body: &[u8]) -> Vec<u8> {
        let mut header = Vec::new();
        wire::int(1, request_id, &mut header);
        let mut out = Vec::new();
        wire::bytes(1, &header, &mut out);
        wire::bytes(case, body, &mut out);
        out
    }

    #[test]
    fn applies_fps_quality_and_scales() {
        let (tx, rx) = watch::channel(VideoSettings::default());
        let mut body = Vec::new();
        wire::int(2, QUALITY_GENERAL, &mut body);
        wire::int(18, 60, &mut body);
        let request = wire::decode(&rpc(7, 2, &body)).unwrap();
        let response = apply_rpc(&request, &tx, (1920, 1080), true, true).unwrap();
        assert_eq!(rx.borrow().fps, 60);
        assert_eq!(rx.borrow().quality, QUALITY_GENERAL);
        assert!(response.windows(2).any(|w| w == [8, 7]));
    }

    #[test]
    fn resolution_is_clamped_to_native() {
        let (tx, rx) = watch::channel(VideoSettings::default());
        let mut body = Vec::new();
        wire::int(1, 1280, &mut body);
        wire::int(2, 720, &mut body);
        let request = wire::decode(&rpc(8, 6, &body)).unwrap();
        apply_rpc(&request, &tx, (1920, 1080), true, false).unwrap();
        assert_eq!(rx.borrow().output, Some((1280, 720)));
    }

    #[test]
    fn inventory_and_codecs_are_well_formed() {
        let sources = screen_sources(1920, 1080, Some((1280, 720)), "desk");
        let fields = wire::decode(&sources).unwrap();
        assert!(wire::blob(&fields, 1).is_some());
        let codecs = codec_negotiation(1920, 1080, 60, true);
        assert!(wire::decode(&codecs).is_some());
    }
}

/// `ScreenSources` for every portal monitor. The viewer selects one by id.
pub fn screen_sources_list(
    screens: &[crate::desktop::ScreenInfo],
    active: u32,
    output: Option<(u32, u32)>,
    name: &str,
) -> Vec<u8> {
    let mut payload = Vec::new();
    for screen in screens {
        let mut encoded = Vec::new();
        put_int(1, u64::from(screen.id), &mut encoded);
        put_int(2, 60, &mut encoded);
        let rect = |width: u32, height: u32| {
            let mut out = Vec::new();
            put_int(1, screen.x as i64 as u64, &mut out);
            put_int(2, screen.y as i64 as u64, &mut out);
            put_int(3, u64::from(width), &mut out);
            put_int(4, u64::from(height), &mut out);
            put_int(5, u64::from(width), &mut out);
            put_int(6, u64::from(height), &mut out);
            out
        };
        put_bytes(3, &rect(screen.width, screen.height), &mut encoded);
        for (scaled_width, scaled_height) in scaled_sizes((screen.width, screen.height)) {
            put_bytes(3, &rect(scaled_width, scaled_height), &mut encoded);
        }
        let current = output.unwrap_or((screen.width, screen.height));
        put_bytes(4, &rect(current.0, current.1), &mut encoded);
        put_bytes(6, &rect(screen.width, screen.height), &mut encoded);
        if screen.primary {
            put_int(7, 1, &mut encoded);
        }
        encoded.push(65);
        encoded.extend_from_slice(&1.0f64.to_le_bytes());
        let label = if screen.primary {
            name.to_owned()
        } else {
            format!("{name} {}", screen.id)
        };
        put_bytes(10, label.as_bytes(), &mut encoded);
        put_int(12, 0, &mut encoded);
        put_bytes(1, &encoded, &mut payload);
    }
    put_int(2, u64::from(active), &mut payload);
    payload
}
