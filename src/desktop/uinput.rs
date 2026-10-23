//! Kernel virtual keyboard and pointer through `/dev/uinput`.
//!
//! Unlike portal Notify calls, uinput events are ordinary kernel input events:
//! they work on the greeter and lock screen, need no screen-share consent, and
//! reach every compositor. Absolute motion is a 0..=65535 tablet axis over the
//! compositor's whole desktop; relative motion, buttons and wheels live on a
//! second device so libinput does not mix pointer models.
//!
//! Access is normally granted by a udev rule:
//!
//! ```text
//! KERNEL=="uinput", SUBSYSTEM=="misc", OPTIONS+="static_node=uinput", TAG+="uaccess"
//! ```
//!
//! `UU_INPUT_BACKEND=uinput` forces this backend, `native` disables it, and
//! `auto` (the default) uses it for portal sessions when the device is
//! writable while keeping the verified XTest path on X11.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Backend, InputSink};

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SIDE: u16 = 0x113;
pub const BTN_EXTRA: u16 = 0x114;
pub const ABS_MAX: i32 = 65535;

const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    absinfo: AbsInfo,
}

#[repr(C)]
struct InputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

struct Device(File);

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: the descriptor is owned by this Device and still open.
        unsafe {
            libc::ioctl(self.0.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

impl Device {
    fn ioctl(&self, request: libc::c_ulong, value: libc::c_ulong) -> std::io::Result<()> {
        // SAFETY: uinput ioctls take an integer argument or a pointer supplied
        // by the caller; all callers below pass initialized storage.
        if unsafe { libc::ioctl(self.0.as_raw_fd(), request, value) } < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn create(
        name: &str,
        product: u16,
        configure: impl FnOnce(&Device) -> std::io::Result<()>,
    ) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/uinput")?;
        let device = Device(file);
        configure(&device)?;
        let mut setup = UinputSetup {
            id: InputId {
                bustype: 0x06, // BUS_VIRTUAL
                vendor: 0x1d6b,
                product,
                version: 1,
            },
            name: [0; 80],
            ff_effects_max: 0,
        };
        let bytes = name.as_bytes();
        let length = bytes.len().min(79);
        setup.name[..length].copy_from_slice(&bytes[..length]);
        // SAFETY: UI_DEV_SETUP reads the initialized UinputSetup.
        if unsafe { libc::ioctl(device.0.as_raw_fd(), UI_DEV_SETUP, &setup) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        device.ioctl(UI_DEV_CREATE, 0)?;
        Ok(device)
    }

    fn abs(&self, code: u16) -> std::io::Result<()> {
        self.ioctl(UI_SET_ABSBIT, u64::from(code))?;
        let setup = UinputAbsSetup {
            code,
            _pad: 0,
            absinfo: AbsInfo {
                value: 0,
                minimum: 0,
                maximum: ABS_MAX,
                fuzz: 0,
                flat: 0,
                resolution: 0,
            },
        };
        // SAFETY: UI_ABS_SETUP reads the initialized UinputAbsSetup.
        if unsafe { libc::ioctl(self.0.as_raw_fd(), UI_ABS_SETUP, &setup) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn emit(&mut self, events: &[(u16, u16, i32)]) -> std::io::Result<()> {
        // Use a real timestamp so compositors that order by event time treat
        // synthetic input like a physical device.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let time = libc::timeval {
            tv_sec: now.as_secs() as libc::time_t,
            tv_usec: now.subsec_micros() as libc::suseconds_t,
        };
        let mut buffer = Vec::with_capacity((events.len() + 1) * std::mem::size_of::<InputEvent>());
        for &(kind, code, value) in events
            .iter()
            .chain(std::iter::once(&(EV_SYN, SYN_REPORT, 0)))
        {
            let event = InputEvent {
                time,
                kind,
                code,
                value,
            };
            // SAFETY: InputEvent is plain-old-data and initialized.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (&event as *const InputEvent).cast::<u8>(),
                    std::mem::size_of::<InputEvent>(),
                )
            };
            buffer.extend_from_slice(bytes);
        }
        self.0.write_all(&buffer)
    }
}

/// The two virtual devices. The keyboard also carries relative motion and
/// wheel axes; the pointer carries absolute axes and buttons.
pub struct Devices {
    keyboard: Mutex<Device>,
    pointer: Mutex<Device>,
}

impl Devices {
    pub fn open() -> std::io::Result<Self> {
        let keyboard = Device::create("openuur keyboard", 1, |device| {
            device.ioctl(UI_SET_EVBIT, u64::from(EV_KEY))?;
            for code in 1..=248u16 {
                device.ioctl(UI_SET_KEYBIT, u64::from(code))?;
            }
            device.ioctl(UI_SET_EVBIT, u64::from(EV_REL))?;
            device.ioctl(UI_SET_RELBIT, u64::from(REL_X))?;
            device.ioctl(UI_SET_RELBIT, u64::from(REL_Y))?;
            for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
                device.ioctl(UI_SET_KEYBIT, u64::from(button))?;
            }
            Ok(())
        })?;
        let pointer = Device::create("openuur pointer", 2, |device| {
            device.ioctl(UI_SET_EVBIT, u64::from(EV_KEY))?;
            for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                device.ioctl(UI_SET_KEYBIT, u64::from(button))?;
            }
            device.ioctl(UI_SET_EVBIT, u64::from(EV_REL))?;
            device.ioctl(UI_SET_RELBIT, u64::from(REL_WHEEL))?;
            device.ioctl(UI_SET_RELBIT, u64::from(REL_HWHEEL))?;
            device.ioctl(UI_SET_EVBIT, u64::from(EV_ABS))?;
            device.abs(ABS_X)?;
            device.abs(ABS_Y)
        })?;
        Ok(Self {
            keyboard: Mutex::new(keyboard),
            pointer: Mutex::new(pointer),
        })
    }

    pub fn key(&self, code: u16, pressed: bool) -> std::io::Result<()> {
        self.keyboard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .emit(&[(EV_KEY, code, i32::from(pressed))])
    }

    /// Release every code this service can emit, even if the userspace ledger
    /// missed a press. Physical devices are never touched.
    pub fn release_all(&self) -> std::io::Result<()> {
        let mut keys: Vec<(u16, u16, i32)> = (1..=248).map(|code| (EV_KEY, code, 0)).collect();
        for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
            keys.push((EV_KEY, button, 0));
        }
        let buttons: Vec<(u16, u16, i32)> = [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA]
            .into_iter()
            .map(|code| (EV_KEY, code, 0))
            .collect();
        let keyboard = self
            .keyboard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .emit(&keys);
        let pointer = self
            .pointer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .emit(&buttons);
        keyboard.and(pointer)
    }

    pub fn move_absolute(&self, x: i32, y: i32) -> std::io::Result<()> {
        self.pointer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .emit(&[
                (EV_ABS, ABS_X, x.clamp(0, ABS_MAX)),
                (EV_ABS, ABS_Y, y.clamp(0, ABS_MAX)),
            ])
    }

    pub fn move_relative(&self, dx: i32, dy: i32) -> std::io::Result<()> {
        self.keyboard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .emit(&[(EV_REL, REL_X, dx), (EV_REL, REL_Y, dy)])
    }

    pub fn button(&self, code: u16, pressed: bool) -> std::io::Result<()> {
        self.pointer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .emit(&[(EV_KEY, code, i32::from(pressed))])
    }

    pub fn wheel(&self, vertical: i32, horizontal: i32) -> std::io::Result<()> {
        let mut events = Vec::new();
        if vertical != 0 {
            events.push((EV_REL, REL_WHEEL, vertical));
        }
        if horizontal != 0 {
            events.push((EV_REL, REL_HWHEEL, horizontal));
        }
        if events.is_empty() {
            return Ok(());
        }
        self.pointer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .emit(&events)
    }
}

/// UU wire buttons are 1 = left, 2 = right, 4 = middle.
pub fn button_code(button: i32) -> Option<u16> {
    match button {
        1 => Some(BTN_LEFT),
        2 => Some(BTN_RIGHT),
        4 => Some(BTN_MIDDLE),
        8 => Some(BTN_SIDE),
        9 => Some(BTN_EXTRA),
        _ => None,
    }
}

/// Wire wheel deltas arrive as notch counts, Windows 120-unit deltas or small
/// smooth deltas. Normalize to whole notches, keeping the remainder.
fn notches(value: i32) -> f64 {
    let value = f64::from(value);
    let magnitude = value.abs();
    if magnitude == 0.0 {
        0.0
    } else if magnitude <= 5.0 {
        value
    } else if magnitude >= 100.0 {
        value / 120.0
    } else {
        value / 10.0
    }
}

struct Wheel {
    x: f64,
    y: f64,
}

impl Wheel {
    fn push(&mut self, dx: i32, dy: i32) -> (i32, i32) {
        self.x = (self.x + notches(dx)).clamp(-100.0, 100.0);
        self.y = (self.y + notches(dy)).clamp(-100.0, 100.0);
        let vertical = self.y.trunc();
        self.y -= vertical;
        let horizontal = self.x.trunc();
        self.x -= horizontal;
        (horizontal as i32, vertical as i32)
    }
}

/// Input backend policy: `uinput` forces it, `native` disables it, `auto`
/// selects it for portal sessions when `/dev/uinput` is accessible. X11 keeps
/// XTest because it already handles the live keymap and terminal shortcuts.
pub fn wanted(backend: Backend) -> bool {
    match std::env::var("UU_INPUT_BACKEND")
        .unwrap_or_else(|_| "auto".to_owned())
        .to_ascii_lowercase()
        .as_str()
    {
        "uinput" => true,
        "native" | "portal" | "x11" | "wayland" => false,
        _ => backend == Backend::Wayland,
    }
}

/// An [`InputSink`] that emits kernel events. It owns no capture state, so a
/// portal reopen or display switch leaves the devices untouched.
pub struct UinputInput {
    devices: Devices,
    wheel: Mutex<Wheel>,
}

impl UinputInput {
    pub fn open() -> std::io::Result<Self> {
        Ok(Self {
            devices: Devices::open()?,
            wheel: Mutex::new(Wheel { x: 0.0, y: 0.0 }),
        })
    }
}

impl InputSink for UinputInput {
    fn pointer_absolute(&self, x: f64, y: f64) {
        let x = (x.clamp(0.0, 1.0) * f64::from(ABS_MAX)).round() as i32;
        let y = (y.clamp(0.0, 1.0) * f64::from(ABS_MAX)).round() as i32;
        if let Err(error) = self.devices.move_absolute(x, y) {
            tracing::warn!("uinput absolute pointer failed: {}", error.kind());
        }
    }

    fn pointer_relative(&self, dx: i32, dy: i32) {
        if let Err(error) = self.devices.move_relative(dx, dy) {
            tracing::warn!("uinput relative pointer failed: {}", error.kind());
        }
    }

    fn button(&self, button: i32, pressed: bool) {
        let Some(code) = button_code(button) else {
            return;
        };
        if let Err(error) = self.devices.button(code, pressed) {
            tracing::warn!("uinput button failed: {}", error.kind());
        }
    }

    fn scroll(&self, dx: i32, dy: i32) {
        let (horizontal, vertical) = self
            .wheel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(dx, dy);
        // Positive Windows deltas are up/right, which are also the positive
        // Linux REL_WHEEL/REL_HWHEEL directions.
        if let Err(error) = self.devices.wheel(vertical, horizontal) {
            tracing::warn!("uinput wheel failed: {}", error.kind());
        }
    }

    fn key(&self, vk: i32, pressed: bool) {
        let Some(code) = evdev_for_vk(vk) else {
            return;
        };
        if let Err(error) = self.devices.key(code, pressed) {
            tracing::warn!("uinput key failed: {}", error.kind());
        }
    }

    fn text(&self, text: &str) {
        // Unicode cannot be expressed as an ordinary evdev keycode. ASCII
        // text still works through the US layout; CJK input uses the portal
        // path when uinput is not selected.
        let mut shift = false;
        for ch in text.chars() {
            let Some((code, needs_shift)) = ascii_key(ch) else {
                continue;
            };
            if needs_shift != shift {
                shift = needs_shift;
                let _ = self.devices.key(EVDEV_LEFTSHIFT, shift);
            }
            let _ = self.devices.key(code, true);
            let _ = self.devices.key(code, false);
        }
        if shift {
            let _ = self.devices.key(EVDEV_LEFTSHIFT, false);
        }
    }

    fn reset(&self) {
        if let Err(error) = self.devices.release_all() {
            tracing::warn!("uinput input reset failed: {}", error.kind());
        }
        *self
            .wheel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Wheel { x: 0.0, y: 0.0 };
    }
}

const EVDEV_LEFTSHIFT: u16 = 42;

/// Base keysym (as produced by `input_map::windows_vk_to_keysym`) to a
/// physical evdev keycode at its US-layout position. The compositor applies
/// the user's own layout, exactly as for a hardware keyboard.
pub fn keysym_to_evdev(keysym: u64) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    const KEYPAD: [u16; 10] = [82, 79, 80, 81, 75, 76, 77, 71, 72, 73];
    let code = match keysym {
        0x61..=0x7a => LETTERS[(keysym - 0x61) as usize],
        0x31..=0x39 => (keysym - 0x31) as u16 + 2,
        0x30 => 11,
        0xffb0..=0xffb9 => KEYPAD[(keysym - 0xffb0) as usize],
        0xffbe..=0xffc7 => (keysym - 0xffbe) as u16 + 59,
        0xffc8 => 87,
        0xffc9 => 88,
        0xffca..=0xffd5 => (keysym - 0xffca) as u16 + 183,
        0xff0b => 76,
        0xff08 => 14,
        0xff09 => 15,
        0xff0d => 28,
        0xff1b => 1,
        0x20 => 57,
        0xff13 => 119,
        0xffe5 => 58,
        0xff7f => 69,
        0xff14 => 70,
        0xff61 => 99,
        0xffe1 => 42,
        0xffe2 => 54,
        0xffe3 => 29,
        0xffe4 => 97,
        0xffe9 => 56,
        0xffea => 100,
        0xffeb => 125,
        0xffec => 126,
        0xff67 => 127,
        0xff55 => 104,
        0xff56 => 109,
        0xff57 => 107,
        0xff50 => 102,
        0xff51 => 105,
        0xff52 => 103,
        0xff53 => 106,
        0xff54 => 108,
        0xff63 => 110,
        0xffff => 111,
        0xffaa => 55,
        0xffab => 78,
        0xffac => 121,
        0xffad => 74,
        0xffae => 83,
        0xffaf => 98,
        0xff8d => 96,
        0x3b => 39,
        0x3d => 13,
        0x2c => 51,
        0x2d => 12,
        0x2e => 52,
        0x2f => 53,
        0x60 => 41,
        0x5b => 26,
        0x5c => 43,
        0x5d => 27,
        0x27 => 40,
        0x1008ff12 => 113,
        0x1008ff11 => 114,
        0x1008ff13 => 115,
        0x1008ff17 => 163,
        0x1008ff16 => 165,
        0x1008ff15 => 166,
        0x1008ff14 => 164,
        _ => return None,
    };
    Some(code)
}

pub fn evdev_for_vk(vk: i32) -> Option<u16> {
    super::input_map::windows_vk_to_keysym(vk).and_then(|keysym| keysym_to_evdev(u64::from(keysym)))
}

/// Printable ASCII on a US layout, with whether Shift is required.
fn ascii_key(ch: char) -> Option<(u16, bool)> {
    let (keysym, shift) = match ch {
        'a'..='z' => (ch as u64, false),
        'A'..='Z' => (ch.to_ascii_lowercase() as u64, true),
        '0'..='9' => (ch as u64, false),
        ' ' => (0x20, false),
        '\n' | '\r' => (0xff0d, false),
        '\t' => (0xff09, false),
        '-' => (0x2d, false),
        '_' => (0x2d, true),
        '=' => (0x3d, false),
        '+' => (0x3d, true),
        '[' => (0x5b, false),
        '{' => (0x5b, true),
        ']' => (0x5d, false),
        '}' => (0x5d, true),
        '\\' => (0x5c, false),
        '|' => (0x5c, true),
        ';' => (0x3b, false),
        ':' => (0x3b, true),
        '\'' => (0x27, false),
        '"' => (0x27, true),
        ',' => (0x2c, false),
        '<' => (0x2c, true),
        '.' => (0x2e, false),
        '>' => (0x2e, true),
        '/' => (0x2f, false),
        '?' => (0x2f, true),
        '`' => (0x60, false),
        '~' => (0x60, true),
        '!' => (0x31, true),
        '@' => (0x32, true),
        '#' => (0x33, true),
        '$' => (0x34, true),
        '%' => (0x35, true),
        '^' => (0x36, true),
        '&' => (0x37, true),
        '*' => (0x38, true),
        '(' => (0x39, true),
        ')' => (0x30, true),
        _ => return None,
    };
    Some((keysym_to_evdev(keysym)?, shift))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_keysyms_to_physical_us_positions() {
        assert_eq!(keysym_to_evdev(0x61), Some(30)); // a
        assert_eq!(keysym_to_evdev(0x7a), Some(44)); // z
        assert_eq!(keysym_to_evdev(0x30), Some(11)); // 0
        assert_eq!(keysym_to_evdev(0x31), Some(2)); // 1
        assert_eq!(keysym_to_evdev(0x2d), Some(12)); // minus -> underscore with Shift
        assert_eq!(keysym_to_evdev(0xffe1), Some(42)); // Shift_L
        assert_eq!(keysym_to_evdev(0xffc9), Some(88)); // F12
        assert_eq!(keysym_to_evdev(0xffb5), Some(76)); // KP_5
        assert_eq!(keysym_to_evdev(0x12345), None);
    }

    #[test]
    fn every_mapped_windows_vk_has_a_physical_key() {
        let missing: Vec<i32> = (0..=0xff)
            .filter(|vk| {
                super::super::input_map::windows_vk_to_keysym(*vk)
                    .is_some_and(|keysym| keysym_to_evdev(u64::from(keysym)).is_none())
            })
            .collect();
        assert!(
            missing.is_empty(),
            "VKs without evdev mapping: {missing:x?}"
        );
    }

    #[test]
    fn buttons_and_wheel_normalization() {
        assert_eq!(button_code(1), Some(BTN_LEFT));
        assert_eq!(button_code(2), Some(BTN_RIGHT));
        assert_eq!(button_code(4), Some(BTN_MIDDLE));
        assert_eq!(button_code(3), None);
        let mut wheel = Wheel { x: 0.0, y: 0.0 };
        assert_eq!(wheel.push(0, 120), (0, 1));
        assert_eq!(wheel.push(120, -120), (1, -1));
        assert_eq!(wheel.push(0, 5), (0, 5));
        assert_eq!(wheel.push(10, 10), (1, 1));
    }

    #[test]
    #[ignore = "requires a writable /dev/uinput"]
    fn opens_emits_and_releases_kernel_devices() {
        let input = UinputInput::open().expect("open /dev/uinput");
        input.pointer_absolute(0.5, 0.5);
        input.pointer_relative(4, -3);
        input.button(1, true);
        input.key(0x41, true);
        input.scroll(0, 120);
        input.reset();
    }

    #[test]
    fn ascii_typing_uses_shift_only_when_needed() {
        assert_eq!(ascii_key('a'), Some((30, false)));
        assert_eq!(ascii_key('A'), Some((30, true)));
        assert_eq!(ascii_key('_'), Some((12, true)));
        assert_eq!(ascii_key('?'), Some((53, true)));
        assert_eq!(ascii_key('你'), None);
    }
}
