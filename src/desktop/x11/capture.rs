//! XDamage + MIT-SHM capture thread.
//!
//! One connection, one thread. The loop drains events with `poll_for_event`,
//! captures the active monitor when damage arrived (or on every tick when
//! DAMAGE is unavailable), and waits on the socket with a frame deadline so a
//! quiet desktop does not spin. Frames are published through the shared
//! `DesktopState`; nothing here blocks tokio.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch};
use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ReportLevel};
use x11rb::protocol::{randr, shm, xproto, Event};
use x11rb::rust_connection::RustConnection;

use super::super::{CapturedFrame, DesktopState, ScreenInfo};

const INIT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn start(
    display: Option<String>,
    probe: &super::Probe,
    state: Arc<DesktopState>,
    stop: Arc<AtomicBool>,
    locked: watch::Receiver<Option<bool>>,
) -> Result<(), String> {
    let (ready_tx, ready_rx) = oneshot::channel();
    let shm = probe.shm;
    let damage = probe.damage;
    let randr = probe.randr;
    std::thread::Builder::new()
        .name("openuur-x11-capture".into())
        .spawn(move || {
            match run(
                display.as_deref(),
                shm,
                damage,
                randr,
                state,
                stop,
                locked,
                ready_tx,
            ) {
                Ok(()) => {}
                Err(error) => tracing::warn!("X11 capture stopped: {error}"),
            }
        })
        .map_err(|error| format!("cannot start the X11 capture thread: {error}"))?;
    match tokio::time::timeout(INIT_TIMEOUT, ready_rx).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => Err(error),
        Ok(Err(_)) => Err("X11 capture thread exited during startup".to_owned()),
        Err(_) => Err("X11 capture startup timed out".to_owned()),
    }
}

fn run(
    display: Option<&str>,
    shm: bool,
    damage_available: bool,
    randr: bool,
    state: Arc<DesktopState>,
    stop: Arc<AtomicBool>,
    locked: watch::Receiver<Option<bool>>,
    ready: oneshot::Sender<Result<(), String>>,
) -> Result<(), String> {
    let (connection, screen_num) =
        x11rb::connect(display).map_err(|error| format!("X11 capture connection: {error}"))?;
    let setup = connection.setup();
    let root = setup.roots[screen_num].root;

    let mut randr_events = false;
    if randr {
        let mask = randr::NotifyMask::SCREEN_CHANGE
            | randr::NotifyMask::CRTC_CHANGE
            | randr::NotifyMask::OUTPUT_CHANGE;
        randr_events = randr::select_input(&connection, root, mask)
            .map(|cookie| cookie.check().is_ok())
            .unwrap_or(false);
    }
    let damage = if damage_available {
        let id = connection
            .generate_id()
            .map_err(|error| format!("X11 damage id: {error}"))?;
        match damage::create(&connection, id, root, ReportLevel::BOUNDING_BOX) {
            Ok(cookie) => match cookie.check() {
                Ok(()) => Some(id),
                Err(error) => {
                    tracing::debug!("X11 damage unavailable: {error}");
                    None
                }
            },
            Err(error) => {
                tracing::debug!("X11 damage unavailable: {error}");
                None
            }
        }
    } else {
        None
    };

    let mut segment = Buffer::new(shm);
    let power = super::super::session_power::RemoteSessionPower::subscribe();
    ready
        .send(Ok(()))
        .map_err(|_| "X11 capture receiver dropped".to_owned())?;

    let fps = crate::desktop::encoder::display_fps().max(1);
    let interval = Duration::from_millis(1000 / u64::from(fps));
    let mut next_frame = Instant::now();
    let mut dirty = true;
    let mut paused = false;
    let mut active = state.active_id();
    let mut damage_seq: Option<u32> = damage;
    let mut next_geometry = Instant::now() + Duration::from_secs(2);

    while !stop.load(Ordering::Acquire) {
        let mut layout_changed = false;
        loop {
            match connection.poll_for_event() {
                Ok(Some(Event::DamageNotify(_))) => dirty = true,
                Ok(Some(Event::RandrScreenChangeNotify(_))) if randr_events => {
                    layout_changed = true;
                }
                Ok(Some(Event::RandrNotify(_))) if randr_events => {
                    layout_changed = true;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    // Never leave the last frame on screen pretending the
                    // session is still live.
                    state.clear_frame();
                    return Err(format!("X11 event read failed: {error}"));
                }
            }
        }
        if layout_changed {
            if refresh_layout(&connection, root, &state) {
                dirty = true;
            }
        }
        // RandR older than 1.5 (and servers without RandR) only expose the
        // root size, so poll it rather than missing a resize.
        if Instant::now() >= next_geometry {
            if refresh_layout(&connection, root, &state) {
                dirty = true;
            }
            next_geometry = Instant::now() + Duration::from_secs(2);
        }
        // `switch_screen` only changes shared state; an idle desktop produces
        // no damage, so the active id itself must trigger a fresh capture.
        let current_active = state.active_id();
        if current_active != active {
            active = current_active;
            dirty = true;
        }

        let sleeping = *power.borrow();
        let locked_now = super::lock_active(&locked);
        if sleeping || locked_now {
            if !paused {
                state.clear_frame();
                paused = true;
            }
            dirty = true;
            std::thread::sleep(interval.min(Duration::from_millis(50)));
            continue;
        }
        paused = false;

        let now = Instant::now();
        if now >= next_frame {
            if dirty || damage_seq.is_none() {
                if let Some(screen) = state.active_screen() {
                    let width = screen.width;
                    let height = screen.height;
                    if width > 0 && height > 0 {
                        // Clear the damage region before the copy: anything
                        // painted from here on lands in an empty region, and
                        // its bounding box therefore notifies even when it
                        // fits inside the area that triggered this capture.
                        if let Some(damage) = damage_seq {
                            let _ = damage::subtract(&connection, damage, 0u32, 0u32);
                            let _ = connection.flush();
                        }
                        match segment.capture(&connection, root, &screen) {
                            Ok(data) => {
                                state.publish_frame(CapturedFrame {
                                    width,
                                    height,
                                    data: Arc::new(data),
                                });
                                dirty = false;
                            }
                            Err(error) => {
                                // The damage was already cleared, so retry on
                                // the next tick instead of waiting for new
                                // damage that may never come.
                                tracing::warn!("X11 capture failed: {error}");
                                dirty = true;
                            }
                        }
                    }
                }
            }
            // Advance the deadline even when nothing was captured; otherwise
            // an idle desktop leaves `wait` at zero and the loop spins on
            // poll(0) until the next damage event.
            next_frame = Instant::now() + interval;
        }

        let wait = next_frame.saturating_duration_since(Instant::now());
        wait_for_input(&connection, wait);
    }
    if let Some(damage) = damage_seq.take() {
        let _ = damage::destroy(&connection, damage);
    }
    segment.release(&connection);
    state.clear_frame();
    Ok(())
}

/// Refresh the screen inventory from RandR, falling back to the root
/// geometry when RandR 1.5 monitors are unavailable. Returns whether the
/// inventory changed.
fn refresh_layout(connection: &impl Connection, root: u32, state: &DesktopState) -> bool {
    let screens = if let Some(monitors) = super::randr_monitors(connection, root) {
        monitors
    } else {
        let reply = match xproto::get_geometry(connection, root)
            .ok()
            .map(|cookie| cookie.reply())
        {
            Some(Ok(reply)) => reply,
            _ => return false,
        };
        vec![ScreenInfo {
            id: 0,
            node_id: 0,
            x: 0,
            y: 0,
            width: u32::from(reply.width),
            height: u32::from(reply.height),
            primary: true,
            mapping_id: None,
        }]
    };
    if screens == state.screens() {
        return false;
    }
    let active = state.active_id();
    state.publish(screens, active, (0, 0, 0));
    true
}

/// Poll the connection socket for at most `wait`, so damage wakes the loop
/// before the frame deadline.
fn wait_for_input(connection: &RustConnection, wait: Duration) {
    let timeout = wait.as_millis().min(i32::MAX as u128) as i32;
    let mut descriptor = libc::pollfd {
        fd: connection.stream().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialized pollfd with a valid connection fd; the call does
    // not retain the pointer.
    unsafe {
        libc::poll(&mut descriptor, 1, timeout);
    }
}

/// One capture buffer, either a shared-memory segment or a plain `Vec`.
struct Buffer {
    shm: bool,
    segment: Option<SharedSegment>,
}

impl Buffer {
    fn new(shm: bool) -> Self {
        Self { shm, segment: None }
    }

    fn capture(
        &mut self,
        connection: &impl Connection,
        root: u32,
        screen: &super::super::ScreenInfo,
    ) -> Result<Vec<u8>, String> {
        let width = u16::try_from(screen.width).map_err(|_| "monitor is too wide".to_owned())?;
        let height = u16::try_from(screen.height).map_err(|_| "monitor is too tall".to_owned())?;
        let x = i16::try_from(screen.x).unwrap_or(0);
        let y = i16::try_from(screen.y).unwrap_or(0);
        let len = width as usize * height as usize * 4;

        if self.shm {
            let needs_buffer = self
                .segment
                .as_ref()
                .map(|segment| segment.len < len)
                .unwrap_or(true);
            if needs_buffer {
                if let Some(old) = self.segment.take() {
                    old.release(connection);
                }
                match SharedSegment::new(connection, len) {
                    Ok(segment) => self.segment = Some(segment),
                    Err(error) => {
                        // MIT-SHM 1.2 may be missing on older servers; the
                        // core GetImage path below always works.
                        tracing::warn!(
                            "X11 shared-memory capture unavailable ({error}); using GetImage"
                        );
                        self.shm = false;
                    }
                }
            }
            if let Some(segment) = self.segment.as_ref() {
                match segment.capture(connection, root, x, y, width, height, len) {
                    Ok(data) => return Ok(data),
                    Err(error) => {
                        tracing::warn!("X11 shared-memory image failed ({error}); using GetImage");
                        // One failure disables the fast path for the rest of
                        // the session so a broken server cannot log at frame
                        // rate.
                        self.shm = false;
                    }
                }
            }
            if let Some(old) = self.segment.take() {
                old.release(connection);
            }
        }

        let reply = xproto::get_image(
            connection,
            xproto::ImageFormat::Z_PIXMAP,
            root,
            x,
            y,
            width,
            height,
            !0,
        )
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?;
        if reply.data.len() < len {
            return Err(format!(
                "X11 image too short: {} of {len} bytes",
                reply.data.len()
            ));
        }
        let mut data = reply.data;
        data.truncate(len);
        force_alpha(&mut data);
        Ok(data)
    }

    fn release(&mut self, connection: &impl Connection) {
        if let Some(segment) = self.segment.take() {
            segment.release(connection);
        }
    }
}

/// Read-only XShmGetImage over a `shm_create_segment` fd.
struct SharedSegment {
    ptr: *mut u8,
    len: usize,
    seg: shm::Seg,
}

impl SharedSegment {
    fn new(connection: &impl Connection, len: usize) -> Result<Self, String> {
        let size = u32::try_from(len).map_err(|_| "capture buffer is too large".to_owned())?;
        let seg = connection
            .generate_id()
            .map_err(|error| format!("X11 shm id: {error}"))?;
        let reply = shm::create_segment(connection, seg, size, false)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        // SAFETY: anonymous shared mapping of the server-provided fd; the size
        // matches the segment and the mapping is checked for MAP_FAILED.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                reply.shm_fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            let _ = shm::detach(connection, seg);
            return Err("X11 shared memory mapping failed".to_owned());
        }
        Ok(Self {
            ptr: ptr.cast(),
            len,
            seg,
        })
    }

    fn capture(
        &self,
        connection: &impl Connection,
        root: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        len: usize,
    ) -> Result<Vec<u8>, String> {
        if len > self.len {
            return Err("capture buffer is smaller than the monitor".to_owned());
        }
        let reply = shm::get_image(
            connection,
            root,
            x,
            y,
            width,
            height,
            !0,
            u8::from(xproto::ImageFormat::Z_PIXMAP),
            self.seg,
            0,
        )
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?;
        if (reply.size as usize) < len {
            return Err(format!(
                "X11 shared image too short: {} of {len} bytes",
                reply.size
            ));
        }
        let mut data = vec![0u8; len];
        // SAFETY: `self.ptr` maps `self.len >= len` bytes and the server wrote
        // exactly `len` bytes into the front of it before the reply arrived.
        unsafe {
            std::ptr::copy_nonoverlapping(self.ptr, data.as_mut_ptr(), len);
        }
        force_alpha(&mut data);
        Ok(data)
    }

    fn release(self, connection: &impl Connection) {
        let _ = shm::detach(connection, self.seg);
        let _ = connection.flush();
        // Drop unmaps the shared pages.
    }
}

impl Drop for SharedSegment {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        // SAFETY: the mapping was created in `new` with this exact base/len.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
        self.ptr = std::ptr::null_mut();
    }
}

/// X pads the alpha byte; make every pixel opaque.
fn force_alpha(data: &mut [u8]) {
    for pixel in data.chunks_exact_mut(4) {
        pixel[3] = 0xff;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_is_forced_opaque() {
        let mut data = vec![1, 2, 3, 0, 4, 5, 6, 7];
        force_alpha(&mut data);
        assert_eq!(data, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }
}
