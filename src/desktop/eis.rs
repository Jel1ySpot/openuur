//! Portal EIS (libei) input.
//!
//! xdg-desktop-portal's `ConnectToEIS` returns a socket to the compositor's
//! input-emulation server. Absolute coordinates are then expressed in the EIS
//! region matching the captured stream's `mapping_id`, which preserves
//! fractional scaling and rotation that `NotifyPointerMotionAbsolute` cannot
//! address reliably.
//!
//! The portal and libei are optional at build time: the D-Bus method is called
//! lazily and libei is opened with `dlopen`. Any missing method, library or
//! capability leaves the caller on the legacy portal Notify path.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::fd::IntoRawFd;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use ashpd::desktop::remote_desktop::RemoteDesktop;
use ashpd::desktop::Session;

const EI_DEVICE_CAP_POINTER: c_int = 1 << 0;
const EI_DEVICE_CAP_POINTER_ABSOLUTE: c_int = 1 << 1;
const EI_DEVICE_CAP_KEYBOARD: c_int = 1 << 2;
const EI_DEVICE_CAP_SCROLL: c_int = 1 << 4;
const EI_DEVICE_CAP_BUTTON: c_int = 1 << 5;
const EI_DEVICE_CAP_TEXT: c_int = 1 << 6;

const EI_EVENT_DISCONNECT: c_int = 2;
const EI_EVENT_SEAT_ADDED: c_int = 3;
const EI_EVENT_DEVICE_ADDED: c_int = 5;
const EI_EVENT_DEVICE_REMOVED: c_int = 6;
const EI_EVENT_DEVICE_PAUSED: c_int = 7;
const EI_EVENT_DEVICE_RESUMED: c_int = 8;

type EiNewSender = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type EiUnref = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type EiConfigureName = unsafe extern "C" fn(*mut c_void, *const c_char);
type EiSetupBackendFd = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type EiGetFd = unsafe extern "C" fn(*mut c_void) -> c_int;
type EiDispatch = unsafe extern "C" fn(*mut c_void) -> c_int;
type EiGetEvent = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type EiEventGetType = unsafe extern "C" fn(*mut c_void) -> c_int;
type EiEventGetSeat = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type EiEventGetDevice = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type EiEventUnref = unsafe extern "C" fn(*mut c_void);
type EiSeatBindCapabilities = unsafe extern "C" fn(*mut c_void, ...);
type EiDeviceHasCapability = unsafe extern "C" fn(*mut c_void, c_int) -> bool;
type EiDeviceGetRegion = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;
type EiRegionGetMappingId = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type EiRegionGetX = unsafe extern "C" fn(*mut c_void) -> u32;
type EiRegionGetY = unsafe extern "C" fn(*mut c_void) -> u32;
type EiRegionGetWidth = unsafe extern "C" fn(*mut c_void) -> u32;
type EiRegionGetHeight = unsafe extern "C" fn(*mut c_void) -> u32;
type EiDeviceStartEmulating = unsafe extern "C" fn(*mut c_void, u32);
type EiDeviceFrame = unsafe extern "C" fn(*mut c_void, u64);
type EiNow = unsafe extern "C" fn(*mut c_void) -> u64;
type EiDevicePointerMotion = unsafe extern "C" fn(*mut c_void, f64, f64);
type EiDevicePointerMotionAbsolute = unsafe extern "C" fn(*mut c_void, f64, f64);
type EiDeviceButtonButton = unsafe extern "C" fn(*mut c_void, u32, bool);
type EiDeviceScrollDiscrete = unsafe extern "C" fn(*mut c_void, i32, i32);
type EiDeviceKeyboardKey = unsafe extern "C" fn(*mut c_void, u32, bool);
type EiDeviceTextUtf8WithLength = unsafe extern "C" fn(*mut c_void, *const c_char, usize);

struct EiLib {
    new_sender: EiNewSender,
    unref: EiUnref,
    configure_name: EiConfigureName,
    setup_backend_fd: EiSetupBackendFd,
    get_fd: EiGetFd,
    dispatch: EiDispatch,
    get_event: EiGetEvent,
    event_get_type: EiEventGetType,
    event_get_seat: EiEventGetSeat,
    event_get_device: EiEventGetDevice,
    event_unref: EiEventUnref,
    seat_bind_capabilities: EiSeatBindCapabilities,
    device_has_capability: EiDeviceHasCapability,
    device_get_region: EiDeviceGetRegion,
    region_get_mapping_id: EiRegionGetMappingId,
    region_get_x: EiRegionGetX,
    region_get_y: EiRegionGetY,
    region_get_width: EiRegionGetWidth,
    region_get_height: EiRegionGetHeight,
    device_start_emulating: EiDeviceStartEmulating,
    device_frame: EiDeviceFrame,
    now: EiNow,
    device_pointer_motion: EiDevicePointerMotion,
    device_pointer_motion_absolute: EiDevicePointerMotionAbsolute,
    device_button_button: EiDeviceButtonButton,
    device_scroll_discrete: EiDeviceScrollDiscrete,
    device_keyboard_key: EiDeviceKeyboardKey,
    device_text_utf8_with_length: EiDeviceTextUtf8WithLength,
}

// The function pointers are immutable after `OnceLock` initialization; libei
// serializes calls internally with its own backend lock.
unsafe impl Send for EiLib {}
unsafe impl Sync for EiLib {}

impl EiLib {
    unsafe fn load() -> Option<Self> {
        let handle = ["libei.so.1", "libei.so"].into_iter().find_map(|name| {
            let name = CString::new(name).ok()?;
            let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            (!handle.is_null()).then_some(handle)
        })?;
        macro_rules! symbol {
            ($name:literal, $ty:ty) => {{
                let name = CString::new($name).ok()?;
                let pointer = unsafe { libc::dlsym(handle, name.as_ptr()) };
                if pointer.is_null() {
                    unsafe { libc::dlclose(handle) };
                    return None;
                }
                unsafe { std::mem::transmute::<*mut c_void, $ty>(pointer) }
            }};
        }
        Some(Self {
            new_sender: symbol!("ei_new_sender", EiNewSender),
            unref: symbol!("ei_unref", EiUnref),
            configure_name: symbol!("ei_configure_name", EiConfigureName),
            setup_backend_fd: symbol!("ei_setup_backend_fd", EiSetupBackendFd),
            get_fd: symbol!("ei_get_fd", EiGetFd),
            dispatch: symbol!("ei_dispatch", EiDispatch),
            get_event: symbol!("ei_get_event", EiGetEvent),
            event_get_type: symbol!("ei_event_get_type", EiEventGetType),
            event_get_seat: symbol!("ei_event_get_seat", EiEventGetSeat),
            event_get_device: symbol!("ei_event_get_device", EiEventGetDevice),
            event_unref: symbol!("ei_event_unref", EiEventUnref),
            seat_bind_capabilities: symbol!("ei_seat_bind_capabilities", EiSeatBindCapabilities),
            device_has_capability: symbol!("ei_device_has_capability", EiDeviceHasCapability),
            device_get_region: symbol!("ei_device_get_region", EiDeviceGetRegion),
            region_get_mapping_id: symbol!("ei_region_get_mapping_id", EiRegionGetMappingId),
            region_get_x: symbol!("ei_region_get_x", EiRegionGetX),
            region_get_y: symbol!("ei_region_get_y", EiRegionGetY),
            region_get_width: symbol!("ei_region_get_width", EiRegionGetWidth),
            region_get_height: symbol!("ei_region_get_height", EiRegionGetHeight),
            device_start_emulating: symbol!("ei_device_start_emulating", EiDeviceStartEmulating),
            device_frame: symbol!("ei_device_frame", EiDeviceFrame),
            now: symbol!("ei_now", EiNow),
            device_pointer_motion: symbol!("ei_device_pointer_motion", EiDevicePointerMotion),
            device_pointer_motion_absolute: symbol!(
                "ei_device_pointer_motion_absolute",
                EiDevicePointerMotionAbsolute
            ),
            device_button_button: symbol!("ei_device_button_button", EiDeviceButtonButton),
            device_scroll_discrete: symbol!("ei_device_scroll_discrete", EiDeviceScrollDiscrete),
            device_keyboard_key: symbol!("ei_device_keyboard_key", EiDeviceKeyboardKey),
            device_text_utf8_with_length: symbol!(
                "ei_device_text_utf8_with_length",
                EiDeviceTextUtf8WithLength
            ),
        })
    }
}

fn library() -> Option<&'static EiLib> {
    // SAFETY: dlopen/dlsym only return pointers; every pointer is checked and
    // stored with the signature documented by libei.h.
    static LIB: LazyLock<Option<EiLib>> = LazyLock::new(|| unsafe { EiLib::load() });
    LIB.as_ref()
}

/// Call the portal's `ConnectToEIS` on an authorized RemoteDesktop session.
async fn connect_fd(session: &Session<RemoteDesktop>) -> Result<std::os::fd::OwnedFd, String> {
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| format!("EIS session bus: {error}"))?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.RemoteDesktop",
    )
    .await
    .map_err(|error| format!("EIS proxy: {error}"))?;
    let options: HashMap<String, zbus::zvariant::Value<'static>> = HashMap::new();
    // Session implements Serialize with signature `o`, which is exactly what
    // ConnectToEIS expects as its first argument.
    let (fd,) = proxy
        .call::<_, _, (zbus::zvariant::OwnedFd,)>("ConnectToEIS", &(session, options))
        .await
        .map_err(|error| format!("ConnectToEIS: {error}"))?;
    Ok(fd.into())
}

struct EiDevice {
    pointer: *mut c_void,
    absolute: *mut c_void,
    keyboard: *mut c_void,
    button: *mut c_void,
    scroll: *mut c_void,
    text: *mut c_void,
}

impl Default for EiDevice {
    fn default() -> Self {
        Self {
            pointer: std::ptr::null_mut(),
            absolute: std::ptr::null_mut(),
            keyboard: std::ptr::null_mut(),
            button: std::ptr::null_mut(),
            scroll: std::ptr::null_mut(),
            text: std::ptr::null_mut(),
        }
    }
}

struct Region {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// One EIS connection bound to a portal session.
pub(crate) struct EisInput {
    lib: &'static EiLib,
    ei: *mut c_void,
    devices: Vec<*mut c_void>,
    current: EiDevice,
    mapping: String,
    sequence: u32,
    failed: bool,
    target_size: (u32, u32),
    wheel_x: f64,
    wheel_y: f64,
}

unsafe impl Send for EisInput {}

impl EisInput {
    /// Whether libei is loadable. Never call ConnectToEIS otherwise: a
    /// successful connection disables the portal Notify methods.
    pub(crate) fn supported() -> bool {
        library().is_some()
    }

    /// Connect asynchronously, then perform the blocking libei handshake on a
    /// worker thread. `mapping_id` selects the screen region for absolute
    /// pointer input; `size` is the stream's coordinate space.
    pub async fn connect(
        session: &Session<RemoteDesktop>,
        mapping_id: &str,
        size: (u32, u32),
    ) -> Result<Self, String> {
        let fd = connect_fd(session).await?;
        let mapping_id = mapping_id.to_owned();
        let mut input = tokio::task::spawn_blocking(move || Self::from_fd(fd, mapping_id))
            .await
            .map_err(|error| format!("EIS worker: {error}"))??;
        input.target_size = size;
        if !input.ready() {
            input.wait_ready(Duration::from_secs(3));
        }
        if input.ready() {
            tracing::info!(
                "portal EIS input ready: region mapping matches {}x{}",
                size.0,
                size.1
            );
            Ok(input)
        } else {
            Err("portal EIS did not expose pointer, keyboard and button devices".to_owned())
        }
    }

    fn from_fd(fd: std::os::fd::OwnedFd, mapping: String) -> Result<Self, String> {
        let lib = library().ok_or_else(|| "libei is not installed".to_owned())?;
        let raw = fd.into_raw_fd();
        // SAFETY: every call matches libei.h; `ei` owns `raw` after setup.
        unsafe {
            let ei = (lib.new_sender)(std::ptr::null_mut());
            if ei.is_null() {
                libc::close(raw);
                return Err("ei_new_sender failed".to_owned());
            }
            let name = CString::new("openuur").expect("static name has no NUL");
            (lib.configure_name)(ei, name.as_ptr());
            if (lib.setup_backend_fd)(ei, raw) != 0 {
                // libei takes ownership of the descriptor even when setup
                // fails; unreffing the context closes it.
                (lib.unref)(ei);
                return Err("libei could not take the portal EIS socket".to_owned());
            }
            Ok(Self {
                lib,
                ei,
                devices: Vec::new(),
                current: EiDevice::default(),
                mapping,
                sequence: 0,
                failed: false,
                target_size: (0, 0),
                wheel_x: 0.0,
                wheel_y: 0.0,
            })
        }
    }

    fn target_size(&self) -> (u32, u32) {
        self.target_size
    }

    /// Track the stream dimensions used to convert normalized viewer input.
    pub(crate) fn set_target(&mut self, size: (u32, u32)) {
        self.target_size = size;
    }

    /// Select the EIS region that matches the active portal stream.
    pub(crate) fn select_mapping(&mut self, mapping: &str) {
        if self.mapping != mapping {
            self.mapping = mapping.to_owned();
            self.refresh_devices();
        }
    }

    fn pump(&mut self) {
        if self.failed || self.ei.is_null() {
            return;
        }
        // SAFETY: `ei` is a live libei context and events are unref'd exactly
        // once. Device pointers remain owned by the context.
        unsafe {
            if (self.lib.dispatch)(self.ei) < 0 {
                self.failed = true;
                return;
            }
            loop {
                let event = (self.lib.get_event)(self.ei);
                if event.is_null() {
                    break;
                }
                let kind = (self.lib.event_get_type)(event);
                match kind {
                    EI_EVENT_SEAT_ADDED => {
                        let seat = (self.lib.event_get_seat)(event);
                        if !seat.is_null() {
                            (self.lib.seat_bind_capabilities)(
                                seat,
                                EI_DEVICE_CAP_POINTER,
                                EI_DEVICE_CAP_POINTER_ABSOLUTE,
                                EI_DEVICE_CAP_KEYBOARD,
                                EI_DEVICE_CAP_BUTTON,
                                EI_DEVICE_CAP_SCROLL,
                                EI_DEVICE_CAP_TEXT,
                                std::ptr::null::<c_void>(),
                            );
                        }
                    }
                    EI_EVENT_DEVICE_ADDED | EI_EVENT_DEVICE_RESUMED => {
                        let device = (self.lib.event_get_device)(event);
                        if !device.is_null() {
                            if !self.devices.contains(&device) {
                                self.devices.push(device);
                            }
                            if kind == EI_EVENT_DEVICE_RESUMED {
                                self.sequence = self.sequence.wrapping_add(1).max(1);
                                (self.lib.device_start_emulating)(device, self.sequence);
                                self.refresh_devices();
                            }
                        }
                    }
                    EI_EVENT_DEVICE_PAUSED | EI_EVENT_DEVICE_REMOVED => {
                        let device = (self.lib.event_get_device)(event);
                        self.devices.retain(|candidate| *candidate != device);
                        self.refresh_devices();
                    }
                    EI_EVENT_DISCONNECT => {
                        tracing::warn!("portal EIS disconnected");
                        self.failed = true;
                    }
                    _ => {}
                }
                (self.lib.event_unref)(event);
            }
        }
    }

    fn wait_ready(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.ready() && !self.failed && Instant::now() < deadline {
            self.pump();
            if self.ready() || self.failed {
                break;
            }
            // SAFETY: ei_get_fd returns a descriptor owned by the context.
            let fd = unsafe { (self.lib.get_fd)(self.ei) };
            if fd < 0 {
                self.failed = true;
                break;
            }
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            let millis = remaining.as_millis().min(100) as c_int;
            // SAFETY: one initialized pollfd.
            unsafe {
                libc::poll(&mut pollfd, 1, millis);
            }
        }
    }

    fn refresh_devices(&mut self) {
        let mut current = EiDevice::default();
        for device in &self.devices {
            // SAFETY: device pointers are live until EI_EVENT_DEVICE_REMOVED,
            // which removes them from `self.devices`.
            unsafe {
                let has = |capability| (self.lib.device_has_capability)(*device, capability);
                if current.pointer.is_null() && has(EI_DEVICE_CAP_POINTER) {
                    current.pointer = *device;
                }
                if current.absolute.is_null()
                    && has(EI_DEVICE_CAP_POINTER_ABSOLUTE)
                    && self.region_for(*device, &self.mapping).is_some()
                {
                    current.absolute = *device;
                }
                if current.keyboard.is_null() && has(EI_DEVICE_CAP_KEYBOARD) {
                    current.keyboard = *device;
                }
                if current.button.is_null() && has(EI_DEVICE_CAP_BUTTON) {
                    current.button = *device;
                }
                if current.scroll.is_null() && has(EI_DEVICE_CAP_SCROLL) {
                    current.scroll = *device;
                }
                if current.text.is_null() && has(EI_DEVICE_CAP_TEXT) {
                    current.text = *device;
                }
            }
        }
        self.current = current;
    }

    fn region_for(&self, device: *mut c_void, mapping: &str) -> Option<Region> {
        for index in 0.. {
            // SAFETY: the device is live and libei returns NULL past the end.
            let region = unsafe { (self.lib.device_get_region)(device, index) };
            if region.is_null() {
                return None;
            }
            // SAFETY: region lives as long as the device.
            let id = unsafe { (self.lib.region_get_mapping_id)(region) };
            if id.is_null() {
                continue;
            }
            let id = unsafe { CStr::from_ptr(id) }.to_string_lossy();
            if id == mapping {
                // SAFETY: accessors take a live region.
                return Some(unsafe {
                    Region {
                        x: (self.lib.region_get_x)(region),
                        y: (self.lib.region_get_y)(region),
                        width: (self.lib.region_get_width)(region),
                        height: (self.lib.region_get_height)(region),
                    }
                });
            }
        }
        None
    }

    fn active_region(&self) -> Option<Region> {
        if self.current.absolute.is_null() {
            return None;
        }
        self.region_for(self.current.absolute, &self.mapping)
    }

    fn ready(&self) -> bool {
        !self.current.absolute.is_null()
            && !self.current.keyboard.is_null()
            && !self.current.button.is_null()
            && !self.current.scroll.is_null()
    }

    fn alive(&mut self) -> bool {
        self.pump();
        !self.failed
    }

    fn emit(&mut self, device: *mut c_void) {
        if device.is_null() {
            return;
        }
        // SAFETY: device is live and belongs to this context.
        unsafe {
            (self.lib.device_frame)(device, (self.lib.now)(self.ei));
        }
    }

    pub(crate) fn pointer_absolute(&mut self, x: f64, y: f64) {
        if !self.alive() {
            return;
        }
        let Some(region) = self.active_region() else {
            return;
        };
        if region.width == 0 || region.height == 0 {
            return;
        }
        let (width, height) = self.target_size();
        let x = x.clamp(0.0, 1.0) * f64::from(width.saturating_sub(1));
        let y = y.clamp(0.0, 1.0) * f64::from(height.saturating_sub(1));
        let target_x = f64::from(region.x) + x * f64::from(region.width) / f64::from(width.max(1));
        let target_y =
            f64::from(region.y) + y * f64::from(region.height) / f64::from(height.max(1));
        let device = self.current.absolute;
        // SAFETY: live absolute device.
        unsafe {
            (self.lib.device_pointer_motion_absolute)(device, target_x, target_y);
        }
        self.emit(device);
    }

    pub(crate) fn pointer_relative(&mut self, dx: i32, dy: i32) {
        if !self.alive() || self.current.pointer.is_null() {
            return;
        }
        let device = self.current.pointer;
        // SAFETY: live relative pointer device.
        unsafe {
            (self.lib.device_pointer_motion)(device, f64::from(dx), f64::from(dy));
        }
        self.emit(device);
    }

    pub(crate) fn button(&mut self, wire: i32, pressed: bool) {
        if !self.alive() || self.current.button.is_null() {
            return;
        }
        let Some(code) = super::uinput::button_code(wire) else {
            return;
        };
        let device = self.current.button;
        // SAFETY: live button device.
        unsafe {
            (self.lib.device_button_button)(device, u32::from(code), pressed);
        }
        self.emit(device);
    }

    pub(crate) fn scroll(&mut self, dx: i32, dy: i32) {
        if !self.alive() || self.current.scroll.is_null() {
            return;
        }
        self.wheel_x = (self.wheel_x + notches(dx)).clamp(-100.0, 100.0);
        self.wheel_y = (self.wheel_y + notches(dy)).clamp(-100.0, 100.0);
        let horizontal = self.wheel_x.trunc();
        self.wheel_x -= horizontal;
        let vertical = self.wheel_y.trunc();
        self.wheel_y -= vertical;
        if horizontal == 0.0 && vertical == 0.0 {
            return;
        }
        let device = self.current.scroll;
        // SAFETY: live scroll device. EIS uses pixel-independent discrete
        // steps; positive Windows deltas are up/right, so vertical is inverted.
        unsafe {
            (self.lib.device_scroll_discrete)(
                device,
                horizontal as i32 * 120,
                -(vertical as i32) * 120,
            );
        }
        self.emit(device);
    }

    pub(crate) fn key(&mut self, vk: i32, pressed: bool) {
        if !self.alive() || self.current.keyboard.is_null() {
            return;
        }
        let Some(code) = super::uinput::evdev_for_vk(vk) else {
            return;
        };
        let device = self.current.keyboard;
        // SAFETY: live keyboard device.
        unsafe {
            (self.lib.device_keyboard_key)(device, u32::from(code), pressed);
        }
        self.emit(device);
    }

    pub(crate) fn text(&mut self, text: &str) {
        if !self.alive() || self.current.text.is_null() {
            return;
        }
        let Ok(text) = CString::new(text.replace('\0', "")) else {
            return;
        };
        let device = self.current.text;
        // SAFETY: live text device and a NUL-terminated UTF-8 buffer.
        unsafe {
            (self.lib.device_text_utf8_with_length)(device, text.as_ptr(), text.as_bytes().len());
        }
        self.emit(device);
    }

    /// Release every key and button this service may hold. EIS has no global
    /// release-all either, so sweep the known evdev ranges on our own devices.
    pub(crate) fn release_all(&mut self) {
        if !self.alive() {
            return;
        }
        if !self.current.keyboard.is_null() {
            for code in 1..=248u32 {
                // SAFETY: live keyboard device.
                unsafe {
                    (self.lib.device_keyboard_key)(self.current.keyboard, code, false);
                }
            }
            self.emit(self.current.keyboard);
        }
        if !self.current.button.is_null() {
            for code in [0x110u32, 0x111, 0x112, 0x113, 0x114] {
                // SAFETY: live button device.
                unsafe {
                    (self.lib.device_button_button)(self.current.button, code, false);
                }
            }
            self.emit(self.current.button);
        }
        self.wheel_x = 0.0;
        self.wheel_y = 0.0;
    }
}

impl Drop for EisInput {
    fn drop(&mut self) {
        if !self.ei.is_null() {
            // SAFETY: the context is owned by this value and unref'd once.
            unsafe {
                (self.lib.unref)(self.ei);
            }
            self.ei = std::ptr::null_mut();
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_normalization_matches_wire_units() {
        assert_eq!(notches(120), 1.0);
        assert_eq!(notches(-240), -2.0);
        assert_eq!(notches(10), 1.0);
        assert_eq!(notches(1), 1.0);
        assert_eq!(notches(0), 0.0);
    }
}
