//! Virtual wheel device through `/dev/uinput`.
//!
//! KWin's portal fake-input backend forwards `NotifyPointerAxis` as a
//! continuous `wl_pointer.axis` event with an unknown source and no
//! `axis_value120`. Clients that only handle discrete scrolling (Zed's GPUI,
//! for example) silently ignore those events. A real `REL_WHEEL` device makes
//! the compositor emit `axis_discrete` / `axis_value120` alongside the
//! continuous event, so every toolkit scrolls.
//!
//! The device is created lazily on the first scroll event and removed when
//! the session ends. When `/dev/uinput` is unavailable we fall back to the
//! portal's continuous axis events.

const EV_SYN: u16 = 0x00;
const EV_REL: u16 = 0x02;
const SYN_REPORT: u16 = 0;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;

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
struct InputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

struct Device(std::fs::File);

impl Drop for Device {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::ioctl(self.0.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

impl Device {
    fn ioctl(&self, request: libc::c_ulong, value: libc::c_ulong) -> std::io::Result<()> {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::ioctl(self.0.as_raw_fd(), request, value) } < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn create(name: &str) -> std::io::Result<Self> {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")?;
        let device = Self(file);
        device.ioctl(UI_SET_EVBIT, EV_REL.into())?;
        device.ioctl(UI_SET_RELBIT, REL_WHEEL.into())?;
        device.ioctl(UI_SET_RELBIT, REL_HWHEEL.into())?;

        let mut setup = UinputSetup {
            id: InputId {
                bustype: 0x06, // BUS_VIRTUAL
                vendor: 0x1d6b,
                product: 0x0002,
                version: 1,
            },
            name: [0; 80],
            ff_effects_max: 0,
        };
        let bytes = name.as_bytes();
        setup.name[..bytes.len().min(79)].copy_from_slice(&bytes[..bytes.len().min(79)]);
        if unsafe { libc::ioctl(device.0.as_raw_fd(), UI_DEV_SETUP, &setup) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        device.ioctl(UI_DEV_CREATE, 0)?;
        Ok(device)
    }

    fn emit(&self, events: &[(u16, u16, i32)]) -> std::io::Result<()> {
        use std::io::Write;

        let zero = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let mut buffer = Vec::with_capacity((events.len() + 1) * std::mem::size_of::<InputEvent>());
        for &(kind, code, value) in events
            .iter()
            .chain(std::iter::once(&(EV_SYN, SYN_REPORT, 0)))
        {
            let event = InputEvent {
                time: zero,
                kind,
                code,
                value,
            };
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (&event as *const InputEvent).cast::<u8>(),
                    std::mem::size_of::<InputEvent>(),
                )
            };
            buffer.extend_from_slice(bytes);
        }
        (&self.0).write_all(&buffer)
    }
}

/// One notch is 120 units, matching the Windows wheel delta and the kernel's
/// high-resolution wheel convention.
const NOTCH: i64 = 120;

pub struct Wheel {
    device: Device,
    remainder: (i64, i64),
}

impl Wheel {
    fn open() -> std::io::Result<Self> {
        Ok(Self {
            device: Device::create("openuur-wheel")?,
            remainder: (0, 0),
        })
    }

    fn scroll(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        self.remainder.0 = (self.remainder.0 + i64::from(dx)).clamp(-12_000, 12_000);
        self.remainder.1 = (self.remainder.1 + i64::from(dy)).clamp(-12_000, 12_000);
        let vertical = (self.remainder.1 / NOTCH) as i32;
        let horizontal = (self.remainder.0 / NOTCH) as i32;
        self.remainder.1 %= NOTCH;
        self.remainder.0 %= NOTCH;
        if vertical == 0 && horizontal == 0 {
            return Ok(());
        }
        let mut events = Vec::with_capacity(2);
        if vertical != 0 {
            events.push((EV_REL, REL_WHEEL, vertical.clamp(-100, 100)));
        }
        if horizontal != 0 {
            events.push((EV_REL, REL_HWHEEL, horizontal.clamp(-100, 100)));
        }
        self.device.emit(&events)
    }
}

/// Scroll delivery state for one desktop session.
#[derive(Default)]
pub enum WheelBackend {
    #[default]
    Unknown,
    Uinput(Wheel),
    Portal,
}

impl WheelBackend {
    /// Send one scroll event. Returns `true` when a real wheel device handled
    /// it; `false` asks the caller to use the portal's continuous axis.
    pub fn send(&mut self, dx: i32, dy: i32) -> bool {
        if matches!(self, Self::Unknown) {
            match Wheel::open() {
                Ok(wheel) => {
                    tracing::info!("uinput wheel device ready");
                    *self = Self::Uinput(wheel);
                }
                Err(error) => {
                    tracing::warn!("uinput wheel unavailable ({error}); using portal axis events");
                    *self = Self::Portal;
                    return false;
                }
            }
        }
        match self {
            Self::Uinput(wheel) => match wheel.scroll(dx, dy) {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!("uinput wheel write failed: {error}; falling back to portal");
                    *self = Self::Portal;
                    false
                }
            },
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual check: requires a writable `/dev/uinput`.
    #[test]
    #[ignore = "requires /dev/uinput access"]
    fn wheel_device_creates_and_emits() {
        let mut wheel = Wheel::open().expect("open /dev/uinput");
        wheel.scroll(0, 120).unwrap();
        wheel.scroll(-240, 0).unwrap();
        // Sub-notch deltas stay buffered without emitting.
        wheel.scroll(0, 40).unwrap();
        wheel.scroll(0, 40).unwrap();
        wheel.scroll(0, 39).unwrap();
        wheel.scroll(0, 1).unwrap();
    }
}
