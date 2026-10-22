//! Input injection for the native wlroots backend.
//!
//! One thread owns the Wayland connection. `InputSink` calls enqueue a
//! command and poke a wake pipe; the loop drains the queue, converts Windows
//! virtual keys through xkbcommon and serializes modifiers with every key.

use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use wayland_client::protocol::wl_keyboard::{KeyState, KeymapFormat};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::{Connection, Dispatch, QueueHandle};

use super::protocol::virtual_keyboard::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use super::protocol::{ZwlrVirtualPointerManagerV1, ZwlrVirtualPointerV1};
use super::{HeldKey, WlrootsState};
use crate::desktop::input_map::{keysym_for, smooth_scroll, windows_vk_to_keysym, wire_button};
use crate::desktop::{InputSink, ScreenInfo};

pub(super) enum Command {
    Absolute { x: f64, y: f64 },
    Relative { dx: i32, dy: i32 },
    Button { button: i32, pressed: bool },
    Scroll { dx: i32, dy: i32 },
    Key { vk: i32, pressed: bool },
    Text(String),
    Wake,
}

/// Enqueue side of the input thread.
pub(super) struct Input {
    commands: SyncSender<Command>,
    wake: Arc<std::os::unix::net::UnixStream>,
}

impl Input {
    pub(super) fn new(
        commands: SyncSender<Command>,
        wake: Arc<std::os::unix::net::UnixStream>,
    ) -> Self {
        Self { commands, wake }
    }

    fn send(&self, command: Command, reliable: bool) {
        match self.commands.try_send(command) {
            Ok(()) => {}
            Err(TrySendError::Full(command)) if reliable => {
                std::thread::sleep(Duration::from_millis(1));
                let _ = self.commands.try_send(command);
            }
            Err(_) => return,
        }
        let _ = (&*self.wake).write(&[1]);
    }
}

impl InputSink for Input {
    fn pointer_absolute(&self, x: f64, y: f64) {
        self.send(Command::Absolute { x, y }, false);
    }

    fn pointer_relative(&self, dx: i32, dy: i32) {
        self.send(Command::Relative { dx, dy }, false);
    }

    fn button(&self, button: i32, pressed: bool) {
        self.send(Command::Button { button, pressed }, true);
    }

    fn scroll(&self, dx: i32, dy: i32) {
        self.send(Command::Scroll { dx, dy }, false);
    }

    fn key(&self, vk: i32, pressed: bool) {
        self.send(Command::Key { vk, pressed }, true);
    }

    fn text(&self, text: &str) {
        self.send(Command::Text(text.to_owned()), true);
    }
}

/// Wake the compositor-owned output-power object.
pub(super) fn request_wake(
    commands: &SyncSender<Command>,
    mut wake: &std::os::unix::net::UnixStream,
) {
    if commands.try_send(Command::Wake).is_ok() {
        let _ = wake.write(&[1]);
    }
}

impl WlrootsState {
    pub(super) fn drain_commands(&mut self) {
        while let Ok(command) = self.commands.try_recv() {
            if self.locked && !matches!(command, Command::Wake) {
                continue;
            }
            self.handle_command(command);
        }
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::Absolute { x, y } => {
                let screens = self
                    .desktop
                    .as_ref()
                    .map(|desktop| desktop.screens())
                    .unwrap_or_default();
                let active = self
                    .desktop
                    .as_ref()
                    .map(|desktop| desktop.active_id())
                    .unwrap_or(0);
                let Some((px, py, extent_x, extent_y)) = absolute_position(&screens, active, x, y)
                else {
                    return;
                };
                if let Some(pointer) = &self.pointer {
                    pointer.motion_absolute(self.now_ms(), px, py, extent_x, extent_y);
                    pointer.frame();
                }
            }
            Command::Relative { dx, dy } => {
                if let Some(pointer) = &self.pointer {
                    pointer.motion(self.now_ms(), f64::from(dx), f64::from(dy));
                    pointer.frame();
                }
            }
            Command::Button { button, pressed } => {
                let Some(code) = wire_button(button) else {
                    return;
                };
                if pressed {
                    self.buttons.insert(code);
                } else {
                    self.buttons.remove(&code);
                }
                if let Some(pointer) = &self.pointer {
                    let state = if pressed {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    };
                    pointer.button(self.now_ms(), code as u32, state);
                    pointer.frame();
                }
            }
            Command::Scroll { dx, dy } => {
                let vertical = -smooth_scroll(dy);
                let horizontal = smooth_scroll(dx);
                if vertical == 0.0 && horizontal == 0.0 {
                    return;
                }
                if let Some(pointer) = &self.pointer {
                    let time = self.now_ms();
                    pointer.axis_source(AxisSource::Wheel);
                    for (axis, value) in [
                        (Axis::VerticalScroll, vertical),
                        (Axis::HorizontalScroll, horizontal),
                    ] {
                        if value == 0.0 {
                            continue;
                        }
                        pointer.axis(time, axis, value);
                        pointer.axis_discrete(time, axis, value, (value / 10.0).round() as i32);
                    }
                    pointer.frame();
                }
            }
            Command::Key { vk, pressed } => {
                if pressed {
                    self.press_vk(vk);
                } else {
                    self.release_vk(vk);
                }
            }
            Command::Text(text) => self.type_text(&text),
            Command::Wake => self.set_output_power(true),
        }
    }

    pub(super) fn now_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    fn set_key(&self, keycode: u8, pressed: bool) {
        if let Some(keyboard) = &self.keyboard {
            let state = if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            };
            keyboard.key(self.now_ms(), u32::from(keycode), state as u32);
        }
    }

    fn send_modifiers(&self) {
        if let Some(keyboard) = &self.keyboard {
            keyboard.modifiers(self.depressed, 0, 0, 0);
        }
    }

    fn shift_bit(&self) -> u32 {
        self.keymap.shift_bit()
    }

    fn press_synthetic_shift(&mut self) -> bool {
        if self.depressed & self.shift_bit() != 0 {
            return false;
        }
        let Some((keycode, _)) = self.keymap.lookup(0xffe1) else {
            return false;
        };
        self.set_key(keycode, true);
        self.depressed |= self.shift_bit();
        self.send_modifiers();
        true
    }

    fn release_synthetic_shift(&mut self) {
        let Some((keycode, _)) = self.keymap.lookup(0xffe1) else {
            return;
        };
        self.set_key(keycode, false);
        self.depressed &= !self.shift_bit();
        self.send_modifiers();
    }

    fn press_vk(&mut self, vk: i32) {
        if self.held.contains_key(&vk) {
            return;
        }
        let Some(keysym) = windows_vk_to_keysym(vk) else {
            return;
        };
        let Some((keycode, shift)) = self.keymap.lookup(keysym) else {
            tracing::debug!("no keycode for virtual key {vk:#x}");
            return;
        };
        let modifier = self.keymap.modifier(keysym);
        let synthetic_shift = shift && modifier.is_none() && self.press_synthetic_shift();
        self.set_key(keycode, true);
        if let Some(bit) = modifier {
            self.depressed |= bit;
            self.send_modifiers();
        }
        self.held.insert(
            vk,
            HeldKey {
                keycode,
                modifier,
                synthetic_shift,
            },
        );
    }

    fn release_vk(&mut self, vk: i32) {
        let Some(held) = self.held.remove(&vk) else {
            return;
        };
        self.set_key(held.keycode, false);
        if let Some(bit) = held.modifier {
            self.depressed &= !bit;
            self.send_modifiers();
        }
        if held.synthetic_shift {
            self.release_synthetic_shift();
        }
    }

    fn tap(&mut self, keycode: u8, shift: bool) {
        let synthetic_shift = shift && self.press_synthetic_shift();
        self.set_key(keycode, true);
        self.set_key(keycode, false);
        if synthetic_shift {
            self.release_synthetic_shift();
        }
    }

    fn type_text(&mut self, text: &str) {
        let mut extras: Vec<char> = Vec::new();
        for ch in text.chars() {
            let keysym = keysym_for(ch);
            match keysym.and_then(|keysym| self.keymap.lookup(keysym)) {
                Some((keycode, shift)) => {
                    self.flush_extras(&mut extras);
                    self.tap(keycode, shift);
                }
                None => {
                    extras.push(ch);
                    if extras.len() == 16 {
                        self.flush_extras(&mut extras);
                    }
                }
            }
        }
        self.flush_extras(&mut extras);
    }

    /// Type characters the base keymap cannot produce through spare keycodes.
    fn flush_extras(&mut self, chars: &mut Vec<char>) {
        if chars.is_empty() {
            return;
        }
        let mut unique = Vec::new();
        for &ch in chars.iter() {
            if !unique.contains(&ch) {
                unique.push(ch);
            }
        }
        let mapping: Vec<(u32, char)> = unique
            .iter()
            .filter_map(|&ch| keysym_for(ch).map(|keysym| (keysym, ch)))
            .collect();
        let Some((temporary, assigned)) = self.keymap.with_extras(&mapping) else {
            chars.clear();
            return;
        };
        if self.keyboard.is_none() {
            chars.clear();
            return;
        }
        self.keymap = temporary;
        self.send_keymap();
        for ch in chars.iter() {
            if let Some(&(_, keycode)) = assigned.iter().find(|(assigned, _)| assigned == ch) {
                self.tap(keycode, false);
            }
        }
        self.keymap = self.base_keymap.clone();
        self.send_keymap();
        chars.clear();
    }

    pub(super) fn send_keymap(&self) {
        let Some(keyboard) = &self.keyboard else {
            return;
        };
        match keymap_fd(&self.keymap.text()) {
            Ok((fd, size)) => keyboard.keymap(KeymapFormat::XkbV1, fd.as_fd(), size),
            Err(error) => tracing::warn!("cannot share the xkb keymap: {error}"),
        }
    }

    /// Release every key and button this client holds. Called when the
    /// session locks, suspends or ends.
    pub(super) fn release_all(&mut self) {
        let held: Vec<i32> = self.held.keys().copied().collect();
        for vk in held {
            self.release_vk(vk);
        }
        let buttons: Vec<i32> = self.buttons.drain().collect();
        if let Some(pointer) = &self.pointer {
            for code in buttons {
                pointer.button(self.now_ms(), code as u32, ButtonState::Released);
            }
            pointer.frame();
        }
        self.depressed = 0;
        self.send_modifiers();
    }
}

/// Map normalized input coordinates onto the compositor's output layout.
/// Returns the absolute position and the layout extent expected by
/// `zwlr_virtual_pointer_v1.motion_absolute`.
pub(super) fn absolute_position(
    screens: &[ScreenInfo],
    active: u32,
    x: f64,
    y: f64,
) -> Option<(u32, u32, u32, u32)> {
    let screen = screens
        .iter()
        .find(|screen| screen.id == active)
        .or_else(|| screens.first())?;
    let min_x = screens.iter().map(|screen| screen.x).min().unwrap_or(0);
    let min_y = screens.iter().map(|screen| screen.y).min().unwrap_or(0);
    let max_x = screens
        .iter()
        .map(|screen| screen.x.saturating_add(screen.width as i32))
        .max()?;
    let max_y = screens
        .iter()
        .map(|screen| screen.y.saturating_add(screen.height as i32))
        .max()?;
    let extent_x = (max_x - min_x).max(1) as u32;
    let extent_y = (max_y - min_y).max(1) as u32;
    let local_x =
        (screen.x - min_x) as f64 + x.clamp(0.0, 1.0) * f64::from(screen.width.saturating_sub(1));
    let local_y =
        (screen.y - min_y) as f64 + y.clamp(0.0, 1.0) * f64::from(screen.height.saturating_sub(1));
    let px = (local_x.round() as u32).min(extent_x.saturating_sub(1));
    let py = (local_y.round() as u32).min(extent_y.saturating_sub(1));
    Some((px, py, extent_x, extent_y))
}

fn keymap_fd(text: &str) -> Result<(OwnedFd, u32), String> {
    let bytes = text.as_bytes();
    let len = bytes.len() + 1;
    let name = c"openuur-keymap";
    let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut data = bytes.to_vec();
    data.push(0);
    let written = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
    if written < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok((fd, len as u32))
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for WlrootsState {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for WlrootsState {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for WlrootsState {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for WlrootsState {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(id: u32, x: i32, y: i32, width: u32, height: u32) -> ScreenInfo {
        ScreenInfo {
            id,
            node_id: 0,
            x,
            y,
            width,
            height,
            primary: id == 0,
        }
    }

    #[test]
    fn absolute_positions_span_the_layout() {
        let screens = [screen(0, 0, 0, 1920, 1080), screen(1, 1920, 0, 1280, 1024)];
        assert_eq!(
            absolute_position(&screens, 0, 0.0, 0.0),
            Some((0, 0, 3200, 1080))
        );
        assert_eq!(
            absolute_position(&screens, 0, 1.0, 1.0),
            Some((1919, 1079, 3200, 1080))
        );
        assert_eq!(
            absolute_position(&screens, 1, 0.5, 0.5),
            Some((2560, 512, 3200, 1080))
        );
    }

    #[test]
    fn negative_layout_origins_are_shifted() {
        let screens = [screen(0, -1920, 0, 1920, 1080), screen(1, 0, 0, 1920, 1080)];
        assert_eq!(
            absolute_position(&screens, 1, 0.0, 0.0),
            Some((1920, 0, 3840, 1080))
        );
    }

    #[test]
    fn missing_screens_do_not_panic() {
        assert!(absolute_position(&[], 0, 0.5, 0.5).is_none());
    }

    #[test]
    fn scrolling_matches_the_viewer_direction() {
        // UU positive dy means wheel-up; wl_pointer positive vertical is down.
        assert_eq!(-smooth_scroll(120), -10.0);
        assert_eq!(-smooth_scroll(-120), 10.0);
    }
}
