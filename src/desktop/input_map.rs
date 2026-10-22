//! Controller shortcut translation for desktop input.
//!
//! Viewers send Windows virtual-key codes. A macOS viewer expresses Cmd as
//! VK_LWIN/VK_RWIN, so without translation Cmd+C would arrive on Linux as
//! Super+C. When the controller is macOS, Cmd+C/V/A/F/X/Z become Ctrl+key
//! (Ctrl+Shift+key while a terminal window is focused) and every other Cmd
//! chord keeps its Super meaning. Independently, Ctrl+V typed while an X11
//! terminal window is focused becomes Ctrl+Shift+V, the paste shortcut most
//! terminal emulators use.

use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::InputSink;

const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_A: i32 = 0x41;
const VK_C: i32 = 0x43;
const VK_F: i32 = 0x46;
const VK_V: i32 = 0x56;
const VK_X: i32 = 0x58;
const VK_Z: i32 = 0x5a;
const VK_LWIN: i32 = 0x5b;
const VK_RWIN: i32 = 0x5c;

/// Client platform value the signaling protocol uses for macOS.
const CLIENT_MAC: u64 = 4;

fn is_cmd(vk: i32) -> bool {
    matches!(vk, VK_LWIN | VK_RWIN)
}

fn is_ctrl(vk: i32) -> bool {
    matches!(vk, VK_CONTROL | 0xa2 | 0xa3)
}

fn is_shift(vk: i32) -> bool {
    matches!(vk, 0x10 | 0xa0 | 0xa1)
}

fn is_alt(vk: i32) -> bool {
    matches!(vk, 0x12 | 0xa4 | 0xa5)
}

fn is_modifier(vk: i32) -> bool {
    is_shift(vk) || is_ctrl(vk) || is_alt(vk)
}

fn is_shortcut(vk: i32) -> bool {
    matches!(vk, VK_C | VK_V | VK_A | VK_F | VK_X | VK_Z)
}

fn is_clipboard(vk: i32) -> bool {
    matches!(vk, VK_C | VK_V)
}

type Focus = Arc<dyn Fn() -> bool + Send + Sync>;

/// Whether the controller is macOS.
///
/// The signaling header wins; the ConnectOptions platform is the fallback.
/// `UU_MAC_SHORTCUTS=1|0` overrides both.
pub fn controller_is_mac(header: &Value, platform: Option<u64>) -> bool {
    detect_mac(
        header,
        platform,
        switch(std::env::var("UU_MAC_SHORTCUTS").ok().as_deref()),
    )
}

fn detect_mac(header: &Value, platform: Option<u64>, forced: Option<bool>) -> bool {
    if let Some(forced) = forced {
        return forced;
    }
    if let Some(value) = find_value(header, &["controller_platform", "controllerPlatform"]) {
        match value {
            Value::String(text) => {
                let text = text.trim().to_ascii_lowercase();
                if matches!(text.as_str(), "4" | "mac" | "macos" | "darwin") {
                    return true;
                }
                if !text.is_empty() {
                    return false;
                }
            }
            Value::Number(number) => return number.as_u64() == Some(CLIENT_MAC),
            _ => {}
        }
    }
    platform == Some(CLIENT_MAC)
}

/// `UU_TERMINAL_SHORTCUTS=0|false|off|no` disables Ctrl+V translation.
fn terminal_shortcuts_enabled() -> bool {
    switch(std::env::var("UU_TERMINAL_SHORTCUTS").ok().as_deref()) != Some(false)
}

fn switch(value: Option<&str>) -> Option<bool> {
    let value = value?.trim().to_ascii_lowercase();
    match value.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn find_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    for key in keys {
        if let Some(found) = value.get(*key) {
            if !found.is_null() {
                return Some(found);
            }
        }
    }
    value.get("data").and_then(|data| find_value(data, keys))
}

#[cfg(all(target_os = "linux", feature = "desktop"))]
fn default_focus() -> Focus {
    Arc::new(|| super::x11::active_window_is_terminal().unwrap_or(false))
}

#[cfg(not(all(target_os = "linux", feature = "desktop")))]
fn default_focus() -> Focus {
    Arc::new(|| false)
}

/// Per-peer input mapping in front of the backend `InputSink`.
pub struct MappedInput {
    inner: Arc<dyn InputSink>,
    mac: bool,
    terminal_shortcuts: bool,
    focus: Focus,
    /// Cleared whenever the session lock state changes.
    lock: Mutex<Option<tokio::sync::watch::Receiver<Option<bool>>>>,
    state: Mutex<State>,
}

impl MappedInput {
    /// `x11_focus` selects X11 terminal-focus probing; Wayland sessions have
    /// no EWMH active window and must not spawn an X connection just for it.
    pub fn new(inner: Arc<dyn InputSink>, mac: bool, x11_focus: bool) -> Self {
        let focus: Focus = if x11_focus {
            default_focus()
        } else {
            Arc::new(|| false)
        };
        Self::with(inner, mac, terminal_shortcuts_enabled(), focus)
    }

    /// Drop translated state on every lock-state change, including a lock and
    /// unlock that both happened while input was paused.
    pub fn with_lock(mut self, lock: tokio::sync::watch::Receiver<Option<bool>>) -> Self {
        self.lock = Mutex::new(Some(lock));
        self
    }

    fn with(inner: Arc<dyn InputSink>, mac: bool, terminal_shortcuts: bool, focus: Focus) -> Self {
        Self {
            inner,
            mac,
            terminal_shortcuts,
            focus,
            lock: Mutex::new(None),
            state: Mutex::new(State::default()),
        }
    }
}

#[derive(Default)]
struct State {
    cmd: Option<Cmd>,
    translated: Option<Translation>,
    ctrl: bool,
    shift_held: bool,
    /// Whether this layer pressed Shift for a terminal paste key.
    terminal_paste: Option<bool>,
}

struct Cmd {
    vk: i32,
    /// Whether a key chord consumed this Cmd press.
    used: bool,
}

struct Translation {
    key: i32,
    /// Whether this layer pressed Ctrl/Shift, so release only what it owns.
    ctrl: bool,
    shift: bool,
    /// The Cmd key forwarded as Super, released with the chord.
    super_vk: Option<i32>,
}

impl State {
    /// Returns whether the event was fully handled.
    fn key(
        &mut self,
        input: &dyn InputSink,
        mac: bool,
        terminal_shortcuts: bool,
        focus: &Focus,
        vk: i32,
        pressed: bool,
    ) -> bool {
        // Track modifier presses that reach the host unchanged, so a
        // synthesized chord never duplicates or releases a modifier the
        // viewer is already holding.
        if is_shift(vk) {
            self.shift_held = pressed;
        }
        if is_ctrl(vk) {
            self.ctrl = pressed;
        }

        if mac && is_cmd(vk) {
            if pressed {
                if self.cmd.is_none() {
                    self.cmd = Some(Cmd { vk, used: false });
                }
            } else if self.cmd.as_ref().is_some_and(|cmd| cmd.vk == vk) {
                // Only the Cmd key that opened the pending state releases it;
                // a second Cmd key cannot turn it into a stray Super tap.
                let cmd = self.cmd.take().unwrap();
                if !cmd.used {
                    // A bare Cmd press is still a Super tap for the host.
                    input.key(vk, true);
                    input.key(vk, false);
                }
            }
            return true;
        }
        if mac && pressed {
            if let Some(translation) = &self.translated {
                if translation.key == vk {
                    // Auto-repeat of a translated key keeps its modifiers.
                    input.key(vk, true);
                    return true;
                }
            }
            if self.translated.is_none() {
                if let Some(cmd) = self.cmd.as_mut() {
                    if is_modifier(vk) {
                        // The viewer pressed Shift/Ctrl/Alt while holding
                        // Cmd. Forward it unchanged and keep Cmd pending so a
                        // following shortcut still translates.
                        return false;
                    }
                    cmd.used = true;
                    let mut translation = Translation {
                        key: vk,
                        ctrl: false,
                        shift: false,
                        super_vk: None,
                    };
                    if is_shortcut(vk) {
                        translation.ctrl = !self.ctrl;
                        translation.shift =
                            !self.shift_held && is_clipboard(vk) && terminal_shortcuts && focus();
                        if translation.ctrl {
                            input.key(VK_CONTROL, true);
                        }
                        if translation.shift {
                            input.key(VK_SHIFT, true);
                        }
                        input.key(vk, true);
                    } else {
                        translation.super_vk = Some(cmd.vk);
                        input.key(cmd.vk, true);
                        input.key(vk, true);
                    }
                    self.translated = Some(translation);
                    return true;
                }
            }
        }
        if mac && !pressed {
            if let Some(translation) = &self.translated {
                if translation.key == vk {
                    let translation = self.translated.take().unwrap();
                    input.key(vk, false);
                    if translation.shift {
                        input.key(VK_SHIFT, false);
                    }
                    if translation.ctrl {
                        input.key(VK_CONTROL, false);
                    }
                    if let Some(super_vk) = translation.super_vk {
                        input.key(super_vk, false);
                    }
                    return true;
                }
            }
        }

        if terminal_shortcuts && vk == VK_V {
            if pressed {
                if self.ctrl && self.terminal_paste.is_none() && focus() {
                    let shift = !self.shift_held;
                    if shift {
                        input.key(VK_SHIFT, true);
                    }
                    self.terminal_paste = Some(shift);
                }
            } else if let Some(shift) = self.terminal_paste.take() {
                input.key(vk, false);
                if shift {
                    input.key(VK_SHIFT, false);
                }
                return true;
            }
        }
        false
    }

    /// Drop translated state without waiting for key releases. Used when the
    /// host stops delivering input (session lock, suspend).
    fn reset(&mut self, input: &dyn InputSink) {
        if let Some(translation) = self.translated.take() {
            input.key(translation.key, false);
            if translation.shift {
                input.key(VK_SHIFT, false);
            }
            if translation.ctrl {
                input.key(VK_CONTROL, false);
            }
            if let Some(super_vk) = translation.super_vk {
                input.key(super_vk, false);
            }
        }
        if let Some(shift) = self.terminal_paste.take() {
            if shift {
                input.key(VK_SHIFT, false);
            }
        }
        self.cmd = None;
        self.ctrl = false;
        self.shift_held = false;
    }
}

impl InputSink for MappedInput {
    fn pointer_absolute(&self, x: f64, y: f64) {
        self.inner.pointer_absolute(x, y);
    }

    fn pointer_relative(&self, dx: i32, dy: i32) {
        self.inner.pointer_relative(dx, dy);
    }

    fn button(&self, button: i32, pressed: bool) {
        self.inner.button(button, pressed);
    }

    fn scroll(&self, dx: i32, dy: i32) {
        self.inner.scroll(dx, dy);
    }

    fn key(&self, vk: i32, pressed: bool) {
        let handled = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(lock) = self
                .lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_mut()
            {
                if lock.has_changed().unwrap_or(false) {
                    let _ = lock.borrow_and_update();
                    state.reset(self.inner.as_ref());
                }
            }
            state.key(
                self.inner.as_ref(),
                self.mac,
                self.terminal_shortcuts,
                &self.focus,
                vk,
                pressed,
            )
        };
        if !handled {
            self.inner.key(vk, pressed);
        }
    }

    fn text(&self, text: &str) {
        self.inner.text(text);
    }

    fn reset(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.reset(self.inner.as_ref());
    }
}

/// Windows virtual-key to XKB keysym, shared by the portal and wlroots
/// backends. Backends translate the keysym against the active layout.
#[cfg_attr(not(all(target_os = "linux", feature = "desktop")), allow(dead_code))]
pub(crate) fn windows_vk_to_keysym(vk: i32) -> Option<u32> {
    Some(match vk {
        0x30..=0x39 => vk as u32,
        0x41..=0x5a => (vk + 0x20) as u32,
        0x60..=0x69 => 0xffb0 + (vk - 0x60) as u32,
        0x70..=0x87 => 0xffbe + (vk - 0x70) as u32,
        0x08 => 0xff08,
        0x09 => 0xff09,
        0x0c => 0xff0b,
        0x0d => 0xff0d,
        0x10 | 0xa0 => 0xffe1,
        0xa1 => 0xffe2,
        0x11 | 0xa2 => 0xffe3,
        0xa3 => 0xffe4,
        0x12 | 0xa4 => 0xffe9,
        0xa5 => 0xffea,
        0x13 => 0xff13,
        0x14 => 0xffe5,
        0x1b => 0xff1b,
        0x20 => 0x0020,
        0x21 => 0xff55,
        0x22 => 0xff56,
        0x23 => 0xff57,
        0x24 => 0xff50,
        0x25 => 0xff51,
        0x26 => 0xff52,
        0x27 => 0xff53,
        0x28 => 0xff54,
        0x2c => 0xff61,
        0x2d => 0xff63,
        0x2e => 0xffff,
        0x5b => 0xffeb,
        0x5c => 0xffec,
        0x5d => 0xff67,
        0x6a => 0xffaa,
        0x6b => 0xffab,
        0x6c => 0xffac,
        0x6d => 0xffad,
        0x6e => 0xffae,
        0x6f => 0xffaf,
        0x90 => 0xff7f,
        0x91 => 0xff14,
        0xba => 0x003b,
        0xbb => 0x003d,
        0xbc => 0x002c,
        0xbd => 0x002d,
        0xbe => 0x002e,
        0xbf => 0x002f,
        0xc0 => 0x0060,
        0xdb => 0x005b,
        0xdc => 0x005c,
        0xdd => 0x005d,
        0xde => 0x0027,
        _ => return None,
    })
}

/// UU wire button to Linux evdev button code.
#[cfg_attr(not(all(target_os = "linux", feature = "desktop")), allow(dead_code))]
pub(crate) fn wire_button(button: i32) -> Option<i32> {
    // UU uses 1/2/4 for left/right/middle.
    match button {
        1 => Some(0x110),
        2 => Some(0x111),
        4 => Some(0x112),
        _ => None,
    }
}

/// Normalize UU wheel deltas to Wayland axis units (10 per notch).
#[cfg_attr(not(all(target_os = "linux", feature = "desktop")), allow(dead_code))]
pub(crate) fn smooth_scroll(value: i32) -> f64 {
    let value = f64::from(value);
    let magnitude = value.abs();
    if magnitude == 0.0 {
        0.0
    } else if magnitude <= 5.0 {
        value * 10.0
    } else if magnitude >= 100.0 {
        value / 12.0
    } else {
        value
    }
}

/// Map a character to an X11 keysym. Latin-1 codepoints are their own keysym;
/// everything above uses the Unicode keysym range.
#[cfg_attr(not(all(target_os = "linux", feature = "desktop")), allow(dead_code))]
pub(crate) fn keysym_for(ch: char) -> Option<u32> {
    match ch {
        '\r' | '\n' => Some(0xff0d),
        '\t' => Some(0xff09),
        '\u{8}' => Some(0xff08),
        '\u{1b}' => Some(0xff1b),
        ch => {
            let codepoint = ch as u32;
            match codepoint {
                0x20..=0x7e | 0xa0..=0xff => Some(codepoint),
                0x100..=0x10ffff => Some(0x0100_0000 | codepoint),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recording {
        events: Mutex<Vec<String>>,
    }

    impl Recording {
        fn take(&self) -> Vec<String> {
            let mut events = self
                .events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *events)
        }
    }

    impl InputSink for Recording {
        fn pointer_absolute(&self, _x: f64, _y: f64) {}
        fn pointer_relative(&self, _dx: i32, _dy: i32) {}
        fn button(&self, _button: i32, _pressed: bool) {}
        fn scroll(&self, _dx: i32, _dy: i32) {}
        fn key(&self, vk: i32, pressed: bool) {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(format!("{vk:02x}:{}", if pressed { "down" } else { "up" }));
        }
        fn text(&self, _text: &str) {}
    }

    fn mapped(mac: bool, terminal: bool, shortcuts: bool) -> (Arc<Recording>, MappedInput) {
        let sink = Arc::new(Recording::default());
        let input = MappedInput::with(sink.clone(), mac, shortcuts, Arc::new(move || terminal));
        (sink, input)
    }

    #[test]
    fn mac_cmd_c_becomes_ctrl_c() {
        let (sink, input) = mapped(true, false, true);
        input.key(VK_LWIN, true);
        input.key(VK_C, true);
        input.key(VK_C, false);
        input.key(VK_LWIN, false);
        assert_eq!(sink.take(), ["11:down", "43:down", "43:up", "11:up"]);
    }

    #[test]
    fn mac_cmd_v_in_a_terminal_becomes_ctrl_shift_v() {
        let (sink, input) = mapped(true, true, true);
        input.key(VK_LWIN, true);
        input.key(VK_V, true);
        input.key(VK_V, false);
        input.key(VK_LWIN, false);
        assert_eq!(
            sink.take(),
            ["11:down", "10:down", "56:down", "56:up", "10:up", "11:up"]
        );
    }

    #[test]
    fn shift_already_held_keeps_ctrl_shift_c() {
        let (sink, input) = mapped(true, false, true);
        input.key(VK_SHIFT, true);
        input.key(VK_LWIN, true);
        input.key(VK_C, true);
        input.key(VK_C, false);
        input.key(VK_LWIN, false);
        input.key(VK_SHIFT, false);
        assert_eq!(
            sink.take(),
            ["10:down", "11:down", "43:down", "43:up", "11:up", "10:up"]
        );
    }

    #[test]
    fn modifier_pressed_while_cmd_is_held_keeps_cmd_pending() {
        let (sink, input) = mapped(true, false, true);
        input.key(VK_LWIN, true);
        input.key(VK_SHIFT, true);
        input.key(VK_C, true);
        input.key(VK_C, false);
        input.key(VK_SHIFT, false);
        input.key(VK_LWIN, false);
        // Shift is forwarded raw; the chord still becomes Ctrl+Shift+C.
        assert_eq!(
            sink.take(),
            ["10:down", "11:down", "43:down", "43:up", "11:up", "10:up"]
        );
    }

    #[test]
    fn held_ctrl_is_not_released_by_a_cmd_shortcut() {
        let (sink, input) = mapped(true, false, true);
        input.key(VK_CONTROL, true);
        input.key(VK_LWIN, true);
        input.key(VK_C, true);
        input.key(VK_C, false);
        input.key(VK_LWIN, false);
        input.key(VK_CONTROL, false);
        assert_eq!(sink.take(), ["11:down", "43:down", "43:up", "11:up"]);
    }

    #[test]
    fn a_second_cmd_key_does_not_corrupt_the_chord() {
        let (sink, input) = mapped(true, false, true);
        input.key(VK_LWIN, true);
        input.key(VK_RWIN, true);
        input.key(VK_RWIN, false);
        input.key(VK_C, true);
        input.key(VK_C, false);
        input.key(VK_LWIN, false);
        assert_eq!(sink.take(), ["11:down", "43:down", "43:up", "11:up"]);
    }

    #[test]
    fn reset_releases_translated_keys() {
        let (sink, input) = mapped(true, true, true);
        input.key(VK_LWIN, true);
        input.key(VK_C, true);
        input.reset();
        assert_eq!(
            sink.take(),
            ["11:down", "10:down", "43:down", "43:up", "10:up", "11:up"]
        );
        input.key(VK_C, false);
        input.key(VK_LWIN, false);
        assert_eq!(sink.take(), ["43:up"]);
    }

    #[test]
    fn a_lock_change_clears_translated_state() {
        let sink = Arc::new(Recording::default());
        let (lock_tx, lock_rx) = tokio::sync::watch::channel(None);
        let input =
            MappedInput::with(sink.clone(), true, true, Arc::new(|| true)).with_lock(lock_rx);
        input.key(VK_LWIN, true);
        input.key(VK_C, true);
        lock_tx.send_replace(Some(true));
        input.key(VK_C, false);
        assert_eq!(
            sink.take(),
            ["11:down", "10:down", "43:down", "43:up", "10:up", "11:up", "43:up"]
        );
    }

    #[test]
    fn other_cmd_chords_stay_super() {
        let (sink, input) = mapped(true, false, true);
        input.key(VK_LWIN, true);
        input.key(0x51, true); // Q
        input.key(0x51, false);
        input.key(VK_LWIN, false);
        assert_eq!(sink.take(), ["5b:down", "51:down", "51:up", "5b:up"]);
    }

    #[test]
    fn bare_cmd_is_a_super_tap() {
        let (sink, input) = mapped(true, false, true);
        input.key(VK_LWIN, true);
        input.key(VK_LWIN, false);
        assert_eq!(sink.take(), ["5b:down", "5b:up"]);
    }

    #[test]
    fn ctrl_c_is_not_translated() {
        let (sink, input) = mapped(true, false, true);
        input.key(0xa2, true);
        input.key(VK_C, true);
        input.key(VK_C, false);
        input.key(0xa2, false);
        assert_eq!(sink.take(), ["a2:down", "43:down", "43:up", "a2:up"]);
    }

    #[test]
    fn ctrl_v_in_a_terminal_becomes_ctrl_shift_v_for_any_controller() {
        let (sink, input) = mapped(false, true, true);
        input.key(VK_CONTROL, true);
        input.key(VK_V, true);
        input.key(VK_V, false);
        input.key(VK_CONTROL, false);
        assert_eq!(
            sink.take(),
            ["11:down", "10:down", "56:down", "56:up", "10:up", "11:up"]
        );
    }

    #[test]
    fn terminal_shortcuts_can_be_disabled() {
        let (sink, input) = mapped(false, true, false);
        input.key(VK_CONTROL, true);
        input.key(VK_V, true);
        input.key(VK_V, false);
        input.key(VK_CONTROL, false);
        assert_eq!(sink.take(), ["11:down", "56:down", "56:up", "11:up"]);
    }

    #[test]
    fn cmd_repeat_does_not_duplicate_the_press() {
        let (sink, input) = mapped(true, false, true);
        input.key(VK_LWIN, true);
        input.key(VK_LWIN, true);
        input.key(VK_C, true);
        input.key(VK_C, false);
        input.key(VK_LWIN, false);
        assert_eq!(sink.take(), ["11:down", "43:down", "43:up", "11:up"]);
    }

    #[test]
    fn header_and_platform_detection() {
        assert!(detect_mac(
            &serde_json::json!({"controller_platform": "4"}),
            None,
            None
        ));
        assert!(detect_mac(
            &serde_json::json!({"controller_platform": "mac"}),
            None,
            None
        ));
        assert!(detect_mac(
            &serde_json::json!({"data": {"controller_platform": 4}}),
            None,
            None
        ));
        assert!(!detect_mac(
            &serde_json::json!({"controller_platform": "windows"}),
            Some(CLIENT_MAC),
            None
        ));
        assert!(detect_mac(&Value::Null, Some(CLIENT_MAC), None));
        assert!(!detect_mac(&Value::Null, Some(1), None));
        assert!(detect_mac(&Value::Null, Some(1), Some(true)));
        assert!(!detect_mac(&Value::Null, Some(CLIENT_MAC), Some(false)));
        assert_eq!(switch(Some("ON")), Some(true));
        assert_eq!(switch(Some("off")), Some(false));
        assert_eq!(switch(Some("maybe")), None);
    }

    #[test]
    fn wire_buttons_follow_uuit_bitmask() {
        assert_eq!(wire_button(1), Some(0x110));
        assert_eq!(wire_button(2), Some(0x111));
        assert_eq!(wire_button(4), Some(0x112));
        assert_eq!(wire_button(3), None);
    }

    #[test]
    fn wheel_deltas_normalize_to_axis_units() {
        assert_eq!(smooth_scroll(0), 0.0);
        assert_eq!(smooth_scroll(1), 10.0);
        assert_eq!(smooth_scroll(-3), -30.0);
        assert_eq!(smooth_scroll(120), 10.0);
        assert_eq!(smooth_scroll(-120), -10.0);
        assert_eq!(smooth_scroll(15), 15.0);
        assert_eq!(smooth_scroll(-44), -44.0);
    }

    #[test]
    fn keyboard_mapping_uses_unshifted_letters_and_named_keys() {
        assert_eq!(windows_vk_to_keysym(65), Some(u32::from(b'a')));
        assert_eq!(windows_vk_to_keysym(160), Some(0xffe1));
        assert_eq!(windows_vk_to_keysym(37), Some(0xff51));
        assert_eq!(windows_vk_to_keysym(999), None);
    }

    #[test]
    fn characters_map_to_x11_keysyms() {
        assert_eq!(keysym_for('a'), Some(0x61));
        assert_eq!(keysym_for('你'), Some(0x0100_4f60));
        assert_eq!(keysym_for('\n'), Some(0xff0d));
        assert_eq!(keysym_for('\u{7}'), None);
    }
}
