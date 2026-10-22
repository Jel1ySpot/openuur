//! Official `SimpleAction` system commands and host-wide settings.
//!
//! Handles lock, show desktop, system monitor, reboot/power-off, host mute
//! (with virtual-audio routing so the viewer keeps hearing audio) and the
//! "lock after the remote session" switch. Settings are host-wide and
//! persisted, like the official host, so a reconnect still applies them.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use super::initialization::action;
use super::system_metrics::{self, Permission, Settings as MetricSettings};

pub const FF_SIMPLE_ACTION: u64 = 2;
pub const FF_SYSTEM_METRICS: u64 = 2;

#[derive(Debug, Default, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
struct Settings {
    #[serde(default)]
    auto_lock: bool,
    #[serde(default)]
    keep_mute: bool,
    #[serde(default)]
    mute_permanent: bool,
}

struct HostState {
    settings: Settings,
    /// Mute state of the physical output before remote control muted it.
    restore: Option<bool>,
    desktop_sessions: usize,
}

fn settings_path() -> Option<std::path::PathBuf> {
    crate::config::Config::path()
        .ok()
        .map(|path| path.with_file_name("host_settings.json"))
}

fn state() -> &'static Mutex<HostState> {
    static STATE: OnceLock<Mutex<HostState>> = OnceLock::new();
    STATE.get_or_init(|| {
        let settings = settings_path()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Mutex::new(HostState {
            settings,
            restore: None,
            desktop_sessions: 0,
        })
    })
}

fn save(settings: &Settings) {
    let Some(path) = settings_path() else { return };
    let result = serde_json::to_vec_pretty(settings)
        .map_err(|error| error.to_string())
        .and_then(|json| std::fs::write(&path, json).map_err(|error| error.to_string()));
    if let Err(error) = result {
        tracing::warn!("host settings not saved: {error}");
    }
}

fn quiet(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Reboot or power off across init systems. systemd gives the best polkit
/// integration; logind's own action works without it, and the portable
/// commands cover OpenRC, runit, s6 and SysV (where the daemon may be root).
fn power_action(action: &str) -> bool {
    quiet("systemctl", &["--no-ask-password", action])
        || quiet("loginctl", &["--no-ask-password", action])
        || quiet(action, &[])
}

fn detached(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|mut child| {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        })
        .unwrap_or(false)
}

/// Mute state of the default output (`None` when no sound server is usable).
pub fn host_muted() -> Option<bool> {
    if let Ok(output) = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        if output.status.success() {
            return Some(String::from_utf8_lossy(&output.stdout).contains("MUTED"));
        }
    }
    let output = Command::new("pactl")
        .args(["get-sink-mute", "@DEFAULT_SINK@"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).contains("yes"))
}

pub fn set_host_muted(muted: bool) -> bool {
    let flag = if muted { "1" } else { "0" };
    quiet("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", flag])
        || quiet("pactl", &["set-sink-mute", "@DEFAULT_SINK@", flag])
}

pub fn lock_session() -> bool {
    // Use the user's display session; loginctl defaults to the caller's.
    quiet("loginctl", &["lock-session"])
}

/// The on/off value of a toggle action: `SimpleAction.value` (field 5), else
/// the args JSON/text; default on.
pub fn toggle_value(value: Option<u64>, args: &[u8]) -> bool {
    if let Some(value) = value {
        return value != 0;
    }
    let text = String::from_utf8_lossy(args);
    let text = text.trim();
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(text) {
        let scalar = |value: &serde_json::Value| {
            value
                .as_bool()
                .or_else(|| value.as_i64().map(|number| number != 0))
                .or_else(|| {
                    value
                        .as_str()
                        .map(|text| matches!(text, "1" | "true" | "on"))
                })
        };
        if let Some(enabled) = scalar(&json) {
            return enabled;
        }
        if let Some(enabled) = json
            .as_object()
            .and_then(|object| object.values().find_map(scalar))
        {
            return enabled;
        }
    }
    !matches!(text, "0" | "false" | "off")
}

/// Args shape for diagnostics: JSON keys and value types, never values.
pub fn args_shape(args: &[u8]) -> String {
    match serde_json::from_slice::<serde_json::Value>(args) {
        Ok(serde_json::Value::Object(object)) => format!(
            "{:?}",
            object
                .iter()
                .map(|(key, value)| {
                    (
                        key.chars().take(32).collect::<String>(),
                        match value {
                            serde_json::Value::Bool(_) => "bool",
                            serde_json::Value::Number(_) => "number",
                            serde_json::Value::String(_) => "string",
                            _ => "other",
                        },
                    )
                })
                .collect::<Vec<_>>()
        ),
        Ok(_) => "json scalar".to_owned(),
        Err(_) => format!("{} bytes non-json", args.len()),
    }
}

fn mute_for_session(state: &mut HostState) {
    if state.restore.is_none() {
        state.restore = host_muted();
        set_host_muted(true);
    }
    crate::desktop::virtual_audio::enable();
}

fn restore_mute(state: &mut HostState) {
    crate::desktop::virtual_audio::disable();
    if let Some(previous) = state.restore.take() {
        set_host_muted(previous);
    }
}

/// Handle for one desktop connection; system actions are host-wide.
#[derive(Default)]
pub struct PeerActions {
    started: AtomicBool,
}

impl PeerActions {
    /// Run one action. Returns true when the settings report should be resent.
    /// Blocking (spawns system tools); call from `spawn_blocking`.
    pub fn handle(
        &self,
        action: u64,
        value: Option<u64>,
        args: &[u8],
        show_desktop: impl FnOnce(),
    ) -> bool {
        match action {
            action::SHOW_DESKTOP => {
                show_desktop();
                tracing::info!("system action: show desktop");
                return false;
            }
            action::LOCK_SYSTEM => {
                tracing::info!("system action: lock session ok={}", lock_session());
                return false;
            }
            action::SHOW_TASKMGR => {
                let ok = detached("gnome-system-monitor", &[])
                    || detached("plasma-systemmonitor", &[])
                    || detached("xfce4-taskmanager", &[])
                    || detached("mate-system-monitor", &[])
                    || detached("lxqt-taskmanager", &[])
                    || detached("gnome-terminal", &["--", "top"]);
                tracing::info!("system action: task manager ok={ok}");
                return false;
            }
            action::REBOOT_SYSTEM => {
                tracing::warn!("system action: reboot requested by viewer");
                tracing::info!("reboot ok={}", power_action("reboot"));
                return false;
            }
            action::SHUTDOWN_SYSTEM => {
                tracing::warn!("system action: shutdown requested by viewer");
                tracing::info!("poweroff ok={}", power_action("poweroff"));
                return false;
            }
            _ => {}
        }
        let mut state = state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = state.settings;
        match action {
            action::MUTE_SYSTEM | action::MUTE_TEMPORARY => {
                let enabled = toggle_value(value, args);
                state.settings.keep_mute = enabled;
                if enabled {
                    mute_for_session(&mut state);
                } else {
                    restore_mute(&mut state);
                }
                tracing::info!(
                    "system action: mute action={action} on={enabled} virtual_sink={} default_muted={:?}",
                    crate::desktop::virtual_audio::active(),
                    host_muted()
                );
            }
            action::MUTE_PERMANENT => {
                let enabled = toggle_value(value, args);
                state.settings.mute_permanent = enabled;
                if enabled {
                    state.settings.keep_mute = true;
                    mute_for_session(&mut state);
                }
                tracing::info!(
                    "system action: mute permanent={enabled} virtual_sink={} default_muted={:?}",
                    crate::desktop::virtual_audio::active(),
                    host_muted()
                );
            }
            action::MUTE_RESTORE => {
                state.settings.keep_mute = false;
                state.settings.mute_permanent = false;
                restore_mute(&mut state);
                tracing::info!(
                    "system action: mute restored virtual_sink={} default_muted={:?}",
                    crate::desktop::virtual_audio::active(),
                    host_muted()
                );
            }
            action::SET_AUTO_LOCK => {
                // Observed: enabling sends value 2, disabling value 1.
                let enabled = match value {
                    Some(value) => value == 2,
                    None => toggle_value(None, args),
                };
                state.settings.auto_lock = enabled;
                tracing::info!("system action: auto lock after session={enabled}");
            }
            _ => return false,
        }
        if state.settings != before {
            save(&state.settings);
        }
        true
    }

    /// First desktop connection: apply "静音运行" if it was left enabled.
    pub fn session_started(&self) {
        if self.started.swap(true, Ordering::Relaxed) {
            return;
        }
        let mut state = state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.desktop_sessions += 1;
        if state.desktop_sessions == 1 && state.settings.keep_mute {
            mute_for_session(&mut state);
            tracing::info!("host muted for remote session");
        }
    }

    /// Last desktop connection ended: undo a session-only mute and lock if the
    /// viewer asked for it.
    pub fn session_ended(&self) {
        if !self.started.swap(false, Ordering::Relaxed) {
            return;
        }
        let mut state = state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.desktop_sessions = state.desktop_sessions.saturating_sub(1);
        if state.desktop_sessions > 0 {
            return;
        }
        crate::desktop::virtual_audio::disable();
        if !state.settings.mute_permanent {
            restore_mute(&mut state);
        }
        if state.settings.auto_lock {
            tracing::info!("auto lock after remote session ok={}", lock_session());
        }
    }

    fn settings(&self) -> Settings {
        state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .settings
    }

    /// SYSTEM_METRICS payload (Message field 13).
    pub fn metrics_payload(
        &self,
        sound: bool,
        capture: Permission,
        operation: Permission,
        control: Permission,
        location: &str,
    ) -> Vec<u8> {
        let settings = self.settings();
        system_metrics::payload(
            capture,
            operation,
            control,
            location,
            MetricSettings {
                sound,
                keep_mute: settings.keep_mute,
                mute_permanent: settings.mute_permanent,
                auto_lock: settings.auto_lock,
            },
        )
    }
}

/// Session RPCs answered with success: 15 send_video_track_req -> 11 (the
/// single desktop track is always sent) and 21 virtual_audio_driver_policy_req
/// -> 17 (the sink monitor is captured directly).
pub fn session_rpc_response(rpc: &[super::wire::Field]) -> Option<Vec<u8>> {
    let body = |number: u32| super::wire::blob(rpc, number);
    let (_request, response_field) = [(15u32, 11u32), (21, 17)]
        .into_iter()
        .find_map(|(request, response)| body(request).map(|value| (value, response)))?;
    let header = body(1).unwrap_or_default();
    let mut out = Vec::new();
    super::wire::bytes(1, header, &mut out);
    super::wire::bytes(response_field, &[], &mut out);
    Some(out)
}

/// Plugin RPCs (16..20): no Linux plugins. Query answers "disabled"; the other
/// requests answer `error_code=1`.
pub fn plugin_response(rpc: &[super::wire::Field]) -> Option<Vec<u8>> {
    let get = |number: u32| super::wire::blob(rpc, number);
    let (request_body, response_field, id_field) = [
        (16u32, 12u32, 3u32),
        (17, 13, 3),
        (18, 14, 2),
        (19, 15, 2),
        (20, 16, 2),
    ]
    .into_iter()
    .find_map(|(request, response, id)| get(request).map(|body| (body, response, id)))?;
    let plugin_id = super::wire::decode(request_body)
        .and_then(|fields| super::wire::blob(&fields, 1).map(<[u8]>::to_vec))
        .unwrap_or_default();
    let mut payload = Vec::new();
    if response_field != 12 {
        super::wire::int(1, 1, &mut payload);
    }
    super::wire::bytes(id_field, &plugin_id, &mut payload);
    let mut out = Vec::new();
    super::wire::bytes(1, get(1).unwrap_or_default(), &mut out);
    super::wire::bytes(response_field, &payload, &mut out);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_values_from_value_field_json_and_text() {
        assert!(!toggle_value(Some(0), b"1"));
        assert!(toggle_value(Some(1), b""));
        assert!(!toggle_value(None, br#"{"enable":false}"#));
        assert!(toggle_value(None, br#"{"value":1}"#));
        assert!(!toggle_value(None, b"0"));
        assert!(toggle_value(None, b""));
    }

    #[test]
    fn diagnostics_never_include_values() {
        let shape = args_shape(br#"{"enable":true,"secret":"abc"}"#);
        assert!(shape.contains("enable") && !shape.contains("abc"));
    }

    #[test]
    fn session_rpc_answers_are_empty_successes() {
        let mut rpc = Vec::new();
        super::super::wire::bytes(1, &[8, 7], &mut rpc);
        super::super::wire::bytes(15, &[], &mut rpc);
        let request = super::super::wire::decode(&rpc).unwrap();
        let response = session_rpc_response(&request).unwrap();
        let fields = super::super::wire::decode(&response).unwrap();
        assert_eq!(super::super::wire::blob(&fields, 1), Some(&[8, 7][..]));
        assert_eq!(super::super::wire::blob(&fields, 11), Some(&[][..]));
    }

    #[test]
    fn plugin_query_answers_disabled() {
        let mut query = Vec::new();
        super::super::wire::bytes(1, b"wow", &mut query);
        let mut rpc = Vec::new();
        super::super::wire::bytes(1, &[8, 9], &mut rpc);
        super::super::wire::bytes(16, &query, &mut rpc);
        let request = super::super::wire::decode(&rpc).unwrap();
        let response = plugin_response(&request).unwrap();
        let fields = super::super::wire::decode(&response).unwrap();
        assert_eq!(super::super::wire::blob(&fields, 1), Some(&[8, 9][..]));
        assert!(super::super::wire::blob(&fields, 12).is_some());
        assert!(super::super::wire::blob(&fields, 12)
            .unwrap()
            .ends_with(b"wow"));
    }
}
