//! Official CONTROL initialization messages (`gv_pb.Message`).
//!
//! These payloads are protocol facts recovered from the shipped clients:
//! ScreenSources (7), CaptureChange (8), ReportError (12), SystemMetrics
//! wrappers (13), SystemStateChange (15), QuerySystemState (16) and the
//! SimpleAction echo/FeatureFlag handshake.

use super::wire;
use super::wire::Field;

/// `gv_pb.SimpleAction.ActionType`.
pub mod action {
    pub const ECHO_REQUEST: u64 = 0;
    pub const ECHO_RESPONSE: u64 = 1;
    pub const SHOW_DESKTOP: u64 = 2;
    pub const SHOW_WINDOWS: u64 = 3;
    pub const CAPTURE_DESKTOP: u64 = 4;
    pub const CAPTURE_WINDOW: u64 = 5;
    pub const STOP_CAPTURE: u64 = 7;
    pub const RESTART_CAPTURE: u64 = 8;
    pub const REBOOT_SYSTEM: u64 = 9;
    pub const LOCK_SYSTEM: u64 = 10;
    pub const KEY_TOGGLE: u64 = 11;
    pub const SHUTDOWN_SYSTEM: u64 = 12;
    pub const SHOW_TASKMGR: u64 = 13;
    pub const MUTE_SYSTEM: u64 = 14;
    pub const AUTO_UNLOCK_SYSTEM: u64 = 15;
    pub const MUTE_RESTORE: u64 = 16;
    pub const MUTE_PERMANENT: u64 = 17;
    pub const MUTE_TEMPORARY: u64 = 18;
    pub const SET_AUTO_LOCK: u64 = 19;
    pub const STOP_ACQUIRE_UPDATE: u64 = 20;
    pub const QUIT_CAPTURE: u64 = 21;
    pub const ACQUIRE_SYSTEM_INFO: u64 = 22;
    pub const NEED_RESTART_USERAPP: u64 = 27;
}

/// `gv_pb.FeatureFlag` field numbers.
pub mod flag {
    pub const CAPTURE_SETTING: u32 = 1;
    pub const SIMPLE_ACTION: u32 = 2;
    pub const SYSTEM_METRICS: u32 = 3;
    pub const FILE_TRANSFER_FTP: u32 = 6;
    pub const CLIPBOARD: u32 = 8;
    pub const QOS_STAT: u32 = 9;
}

/// Versions this host actually implements.
pub const FF_CAPTURE_SETTING_CUSTOM_BITRATE: u64 = 4;
pub const FF_SIMPLE_ACTION_AUTO_LOCK: u64 = 2;
pub const FF_SYSTEM_METRICS_SETTING_AUTO_LOCK: u64 = 2;
pub const FF_FILE_TRANSFER_FTP_LIST_COMPRESS: u64 = 2;
pub const FF_CLIPBOARD_TEXT_CHANGE: u64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub clipboard: bool,
    pub file_transfer: bool,
    pub capture_setting: bool,
    pub simple_action: bool,
    pub system_metrics: bool,
    pub qos_stat: bool,
}

impl Capabilities {
    pub fn host_desktop(clipboard: bool) -> Self {
        Self {
            clipboard,
            file_transfer: true,
            capture_setting: true,
            simple_action: true,
            system_metrics: true,
            qos_stat: false,
        }
    }

    pub fn encode(self, out: &mut Vec<u8>) {
        let mut put = |field: u32, version: u64| {
            if version != 0 {
                wire::int(field, version, out);
            }
        };
        put(
            flag::CAPTURE_SETTING,
            if self.capture_setting {
                FF_CAPTURE_SETTING_CUSTOM_BITRATE
            } else {
                0
            },
        );
        put(
            flag::SIMPLE_ACTION,
            if self.simple_action {
                FF_SIMPLE_ACTION_AUTO_LOCK
            } else {
                0
            },
        );
        put(
            flag::SYSTEM_METRICS,
            if self.system_metrics {
                FF_SYSTEM_METRICS_SETTING_AUTO_LOCK
            } else {
                0
            },
        );
        put(
            flag::FILE_TRANSFER_FTP,
            if self.file_transfer {
                FF_FILE_TRANSFER_FTP_LIST_COMPRESS
            } else {
                0
            },
        );
        put(
            flag::CLIPBOARD,
            if self.clipboard {
                FF_CLIPBOARD_TEXT_CHANGE
            } else {
                0
            },
        );
        put(flag::QOS_STAT, if self.qos_stat { 1 } else { 0 });
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub pixel_width: i32,
    pub pixel_height: i32,
}

impl Rect {
    fn encode(self) -> Option<Vec<u8>> {
        if self.width <= 0 || self.height <= 0 || self.pixel_width <= 0 || self.pixel_height <= 0 {
            return None;
        }
        let mut out = Vec::new();
        for (field, value) in [
            (1u32, self.left),
            (2, self.top),
            (3, self.width),
            (4, self.height),
            (5, self.pixel_width),
            (6, self.pixel_height),
        ] {
            if value != 0 {
                wire::int(field, value as i64 as u64, &mut out);
            }
        }
        Some(out)
    }
}

#[derive(Debug, Clone)]
pub struct Screen {
    pub id: u32,
    pub fps: u32,
    pub rect: Rect,
    pub primary: bool,
    /// 0 physical, 1 virtual, 2 super screen.
    pub screen_type: u32,
    /// 0 unknown, 1 built-in, 2 not built-in.
    pub builtin_type: u32,
    pub dpr: f64,
    pub name: String,
    pub video_track_index: u32,
    pub scaled: Vec<(u32, u32)>,
    pub current: Option<(u32, u32)>,
}

impl Screen {
    fn encode(&self) -> Option<Vec<u8>> {
        if self.fps == 0 || !self.dpr.is_finite() || self.dpr <= 0.0 {
            return None;
        }
        let native = self.rect.encode()?;
        let sized = |(width, height): (u32, u32)| Rect {
            width: width as i32,
            height: height as i32,
            pixel_width: width as i32,
            pixel_height: height as i32,
            ..self.rect
        };
        let mut out = Vec::new();
        wire::int(1, u64::from(self.id), &mut out);
        wire::int(2, u64::from(self.fps), &mut out);
        wire::bytes(3, &native, &mut out);
        for size in &self.scaled {
            wire::bytes(3, &sized(*size).encode()?, &mut out);
        }
        let current = self.current.map_or_else(
            || native.clone(),
            |size| sized(size).encode().unwrap_or_default(),
        );
        wire::bytes(4, &current, &mut out);
        wire::int(5, u64::from(self.screen_type), &mut out);
        wire::bytes(6, &native, &mut out);
        if self.primary {
            wire::int(7, 1, &mut out);
        }
        // Field 8 is a double (fixed64), not a varint.
        out.push((8 << 3) | 1);
        out.extend_from_slice(&self.dpr.to_le_bytes());
        if !self.name.is_empty() {
            wire::bytes(10, self.name.as_bytes(), &mut out);
        }
        wire::int(12, u64::from(self.video_track_index), &mut out);
        wire::int(13, u64::from(self.builtin_type), &mut out);
        Some(out)
    }
}

/// `Message.screens` (field 7). Returns the body, not the envelope.
pub fn screen_sources(screens: &[Screen], current_id: u32) -> Option<Vec<u8>> {
    if screens.is_empty() || !screens.iter().any(|screen| screen.id == current_id) {
        return None;
    }
    let mut payload = Vec::new();
    for screen in screens {
        wire::bytes(1, &screen.encode()?, &mut payload);
    }
    wire::int(2, u64::from(current_id), &mut payload);
    Some(payload)
}

/// `Message.screens` (field 7) for the portal's measured streams.
#[allow(clippy::too_many_arguments)]
pub fn screen_sources_from(
    screens: &[crate::desktop::ScreenInfo],
    active: u32,
    output: Option<(u32, u32)>,
    name: &str,
    screen_type: u32,
    fps: u32,
) -> Option<Vec<u8>> {
    let mut encoded = Vec::new();
    for screen in screens {
        let label = if screen.primary {
            name.to_owned()
        } else {
            format!("{name} {}", screen.id)
        };
        encoded.push(Screen {
            id: screen.id,
            fps: fps.max(1),
            rect: Rect {
                left: screen.x,
                top: screen.y,
                width: screen.width as i32,
                height: screen.height as i32,
                pixel_width: screen.width as i32,
                pixel_height: screen.height as i32,
            },
            primary: screen.primary,
            screen_type,
            builtin_type: 0,
            dpr: 1.0,
            name: label,
            video_track_index: 0,
            scaled: crate::uu_native::video_settings::scaled_sizes((screen.width, screen.height)),
            current: if screen.id == active { output } else { None },
        });
    }
    screen_sources(&encoded, active)
}

/// `Message.capture_change` (field 8). `capture_type`: 0 desktop, 1 window,
/// 2 MuMu, 3 hook, 99 none.
pub fn capture_change(capture_type: u32, type_value: u32, description: &str) -> Option<Vec<u8>> {
    if !matches!(capture_type, 0 | 1 | 2 | 3 | 99) {
        return None;
    }
    let mut out = Vec::new();
    if capture_type != 0 {
        wire::int(1, u64::from(capture_type), &mut out);
    }
    if type_value != 0 {
        wire::int(2, u64::from(type_value), &mut out);
    }
    if !description.is_empty() {
        wire::bytes(3, description.as_bytes(), &mut out);
    }
    Some(out)
}

/// `Message.report_error` (field 12).
pub fn report_error(error_code: i32, message: &str) -> Vec<u8> {
    let mut out = Vec::new();
    wire::int(2, error_code as i64 as u64, &mut out);
    if !message.is_empty() {
        wire::bytes(3, message.as_bytes(), &mut out);
    }
    out
}

/// `Message.system_state_change` (field 15) with a SecureDesktopState.
pub fn secure_desktop(locked: bool, sas_shown: bool, supports_auto_logon: bool) -> Vec<u8> {
    let mut state = Vec::new();
    if locked {
        wire::int(1, 1, &mut state);
    }
    if sas_shown {
        wire::int(2, 1, &mut state);
    }
    if supports_auto_logon {
        wire::int(5, 1, &mut state);
    }
    let mut change = Vec::new();
    wire::bytes(1, &state, &mut change);
    change
}

/// `Message.system_state_change` (field 15) with PermissionState.
pub fn permissions(
    assistant: bool,
    video_capture: bool,
    content_visible: bool,
    audio_capture: bool,
    floating_ball: bool,
) -> Vec<u8> {
    let mut state = Vec::new();
    for (field, enabled) in [
        (1u32, assistant),
        (2, video_capture),
        (3, content_visible),
        (4, audio_capture),
        (5, floating_ball),
    ] {
        if enabled {
            wire::int(field, 1, &mut state);
        }
    }
    let mut change = Vec::new();
    wire::bytes(3, &state, &mut change);
    change
}

/// `Message.system_state_change` (field 15) with a CursorShape.
///
/// `pos_x`/`pos_y` are the hotspot, field 5 is a straight-alpha RGBA PNG, and
/// both coordinate scales are 1.0 (video pixels per screen point). The
/// zero-valued `cursor_type` and `screen_id` are omitted as proto3 defaults.
pub fn cursor_shape(cursor: &crate::desktop::CursorImage) -> Option<Vec<u8>> {
    let rgba = crate::desktop::cursor::rgba_from_argb(&cursor.argb);
    let png =
        crate::desktop::cursor::png_rgba8(u32::from(cursor.width), u32::from(cursor.height), &rgba);
    if png.is_empty() {
        return None;
    }
    let mut shape = Vec::new();
    if cursor.xhot != 0 {
        wire::int(1, u64::from(cursor.xhot), &mut shape);
    }
    if cursor.yhot != 0 {
        wire::int(2, u64::from(cursor.yhot), &mut shape);
    }
    wire::int(3, u64::from(cursor.width), &mut shape);
    wire::int(4, u64::from(cursor.height), &mut shape);
    wire::bytes(5, &png, &mut shape);
    wire::fixed64(7, 1.0f64.to_bits(), &mut shape);
    wire::fixed64(8, 1.0f64.to_bits(), &mut shape);
    let mut change = Vec::new();
    wire::bytes(2, &shape, &mut change);
    Some(change)
}

/// `SimpleAction` echo response body (field 3 content).
pub fn echo_response_body(
    request_seq: u64,
    capabilities: Capabilities,
    clipboard_ready: bool,
) -> Vec<u8> {
    let args = format!("{{\"seq\":{request_seq}}}");
    let mut out = Vec::new();
    wire::int(1, action::ECHO_RESPONSE, &mut out);
    wire::bytes(2, args.as_bytes(), &mut out);
    let mut capabilities = capabilities;
    capabilities.clipboard &= clipboard_ready;
    let mut flags = Vec::new();
    capabilities.encode(&mut flags);
    wire::bytes(4, &flags, &mut out);
    out
}

/// Full `Message` carrying an echo response (field 3).
pub fn echo_response(
    seq: u64,
    timestamp: u64,
    request_seq: u64,
    capabilities: Capabilities,
    clipboard_ready: bool,
) -> Vec<u8> {
    let body = echo_response_body(request_seq, capabilities, clipboard_ready);
    let mut out = Vec::new();
    if seq != 0 {
        wire::int(1, seq, &mut out);
    }
    if timestamp != 0 {
        wire::int(2, timestamp, &mut out);
    }
    wire::bytes(3, &body, &mut out);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleAction {
    pub action: u64,
    pub value: Option<u64>,
    pub key_toggle: Option<(u64, u64)>,
    pub flags: Option<Vec<Field>>,
    pub args: Vec<u8>,
    /// Outer `Message.seq` (used to correlate the echo response).
    pub seq: u64,
    pub timestamp: u64,
}

/// Decode `Message.simple_action` (field 3).
pub fn parse_simple_action(data: &[u8]) -> Option<SimpleAction> {
    let message = wire::decode(data)?;
    let action = wire::decode(wire::blob(&message, 3)?)?;
    Some(SimpleAction {
        action: wire::uint(&action, 1).unwrap_or(action::ECHO_REQUEST),
        value: wire::uint(&action, 5),
        key_toggle: wire::blob(&action, 3).and_then(wire::decode).map(|fields| {
            (
                wire::uint(&fields, 1).unwrap_or(0),
                wire::uint(&fields, 2).unwrap_or(0),
            )
        }),
        flags: wire::blob(&action, 4).and_then(wire::decode),
        args: wire::blob(&action, 2).unwrap_or_default().to_vec(),
        seq: wire::uint(&message, 1).unwrap_or(0),
        timestamp: wire::uint(&message, 2).unwrap_or(0),
    })
}

/// Decode `Message.query_system_state` (field 16); returns the StateId.
pub fn query_system_state(data: &[u8]) -> Option<u64> {
    let message = wire::decode(data)?;
    let query = wire::decode(wire::blob(&message, 16)?)?;
    Some(wire::uint(&query, 1).unwrap_or(0))
}

/// Seconds since the Unix epoch, for `Message.timestamp`.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(data: &[u8]) -> Vec<Field> {
        wire::decode(data).unwrap()
    }

    fn screen() -> Screen {
        Screen {
            id: 1,
            fps: 60,
            rect: Rect {
                left: -1920,
                top: 0,
                width: 1920,
                height: 1080,
                pixel_width: 1920,
                pixel_height: 1080,
            },
            primary: true,
            screen_type: 0,
            builtin_type: 0,
            dpr: 1.0,
            name: "display".into(),
            video_track_index: 0,
            scaled: vec![(1280, 720)],
            current: Some((1280, 720)),
        }
    }

    #[test]
    fn screen_inventory_round_trips_rects_and_dpr() {
        let body = screen_sources(&[screen()], 1).unwrap();
        let fields = decode(&body);
        assert_eq!(wire::uint(&fields, 2), Some(1));
        let display = wire::decode(wire::blob(&fields, 1).unwrap()).unwrap();
        let current = wire::decode(wire::blob(&display, 4).unwrap()).unwrap();
        assert_eq!(wire::uint(&current, 3), Some(1280));
        assert_eq!(wire::uint(&current, 6), Some(720));
        let native = wire::decode(wire::blob(&display, 6).unwrap()).unwrap();
        assert_eq!(wire::uint(&native, 3).unwrap(), 1920u64);
        assert_eq!(wire::uint(&native, 1).unwrap(), (-1920i64) as u64);
        assert!(display
            .iter()
            .any(|field| field.number == 8
                && field.value == wire::Value::Fixed64(1.0f64.to_le_bytes())));
        assert!(screen_sources(&[screen()], 2).is_none());
    }

    #[test]
    fn secure_desktop_and_permissions_are_explicit() {
        let change = decode(&secure_desktop(false, false, true));
        let state = wire::decode(wire::blob(&change, 1).unwrap()).unwrap();
        assert_eq!(wire::uint(&state, 1), None);
        assert_eq!(wire::uint(&state, 5), Some(1));
        let change = decode(&permissions(false, true, true, true, false));
        let state = wire::decode(wire::blob(&change, 3).unwrap()).unwrap();
        assert_eq!(wire::uint(&state, 2), Some(1));
        assert_eq!(wire::uint(&state, 4), Some(1));
        assert_eq!(wire::uint(&state, 1), None);
    }

    #[test]
    fn cursor_shape_carries_hotspot_size_and_png() {
        let cursor = crate::desktop::CursorImage {
            serial: 7,
            width: 2,
            height: 1,
            xhot: 1,
            yhot: 0,
            argb: std::sync::Arc::new(vec![0x80_8000_00, 0xff00_ff00]),
        };
        let change = decode(&cursor_shape(&cursor).unwrap());
        let shape = wire::decode(wire::blob(&change, 2).unwrap()).unwrap();
        assert_eq!(wire::uint(&shape, 1), Some(1));
        assert_eq!(wire::uint(&shape, 2), None);
        assert_eq!(wire::uint(&shape, 3), Some(2));
        assert_eq!(wire::uint(&shape, 4), Some(1));
        let png = wire::blob(&shape, 5).unwrap();
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        for field in [7u32, 8] {
            assert!(shape.iter().any(|item| item.number == field
                && item.value == wire::Value::Fixed64(1.0f64.to_le_bytes())));
        }
        // An empty image produces no message at all.
        let empty = crate::desktop::CursorImage {
            serial: 8,
            width: 0,
            height: 0,
            xhot: 0,
            yhot: 0,
            argb: std::sync::Arc::new(Vec::new()),
        };
        assert!(cursor_shape(&empty).is_none());
    }

    #[test]
    fn echo_response_advertises_only_ready_features() {
        let capabilities = Capabilities::host_desktop(true);
        let message = echo_response(1, 2, 99, capabilities, false);
        let action = wire::decode(wire::blob(&decode(&message), 3).unwrap()).unwrap();
        assert_eq!(wire::uint(&action, 1), Some(1));
        let args = wire::blob(&action, 2).unwrap();
        assert_eq!(args, br#"{"seq":99}"#);
        let flags = wire::decode(wire::blob(&action, 4).unwrap()).unwrap();
        let fields: Vec<(u32, u64)> = flags
            .iter()
            .map(|field| match field.value {
                wire::Value::Varint(value) => (field.number, value),
                _ => panic!("flag is not a varint"),
            })
            .collect();
        assert_eq!(fields, vec![(1, 4), (2, 2), (3, 2), (6, 2)]);
        let message = echo_response(1, 2, 7, capabilities, true);
        let action = wire::decode(wire::blob(&decode(&message), 3).unwrap()).unwrap();
        let flags = wire::decode(wire::blob(&action, 4).unwrap()).unwrap();
        assert!(flags
            .iter()
            .any(|field| field.number == 8 && field.value == wire::Value::Varint(3)));
    }

    #[test]
    fn parses_simple_action_and_query() {
        let mut action_body = Vec::new();
        wire::int(1, action::AUTO_UNLOCK_SYSTEM, &mut action_body);
        wire::bytes(2, br#"{"key":"secret"}"#, &mut action_body);
        wire::int(5, 0, &mut action_body);
        let mut message = Vec::new();
        wire::bytes(3, &action_body, &mut message);
        let parsed = parse_simple_action(&message).unwrap();
        assert_eq!(parsed.action, action::AUTO_UNLOCK_SYSTEM);
        assert_eq!(parsed.args, br#"{"key":"secret"}"#);
        let mut query = Vec::new();
        wire::int(1, 1, &mut query);
        let mut message = Vec::new();
        wire::bytes(16, &query, &mut message);
        assert_eq!(query_system_state(&message), Some(1));
    }
}
