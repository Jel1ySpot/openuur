//! Native wlroots capture and input.
//!
//! The backend drives the compositor directly instead of going through
//! xdg-desktop-portal: `zwlr_screencopy_manager_v1` for frames,
//! `zwlr_virtual_pointer_v1` and `zwp_virtual_keyboard_v1` for input, and
//! `zwlr_output_power_manager_v1` to wake displays. One thread owns the
//! Wayland connection; the rest of the host only sees `DesktopSession`.
//!
//! GNOME and KDE sessions keep using the portal, which their compositors
//! support and which also supplies window and virtual sources.

mod capture;
mod input;
mod keymap;
mod protocol;

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch};
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{wl_output, wl_registry, wl_seat, wl_shm};
use wayland_client::{
    backend::WaylandError, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1::ZxdgOutputV1,
};
use wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1;

use super::encoder::display_fps;
use super::{
    session_lock, DesktopOptions, DesktopSession, DesktopState, InputSink, ScreenInfo, SourceKind,
};
use capture::{FrameData, Slot};
use input::{Command, Input};
use keymap::Keymap;
use protocol::virtual_keyboard::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use protocol::{
    PowerMode, ZwlrOutputPowerV1, ZwlrScreencopyManagerV1, ZwlrVirtualPointerManagerV1,
    ZwlrVirtualPointerV1,
};

/// One `wl_output` in the compositor layout.
struct OutputEntry {
    id: u32,
    output: wl_output::WlOutput,
    version: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    /// Logical layout position/size from `zxdg_output_v1`. wlroots always
    /// reports wl_output.geometry as (0,0), so this is the only source of
    /// real output placement; zeros fall back to the mode size.
    logical_x: i32,
    logical_y: i32,
    logical_width: u32,
    logical_height: u32,
    xdg: Option<ZxdgOutputV1>,
}

/// Data attached to a `wl_output` proxy.
struct OutputData {
    id: u32,
}

/// The frame currently in flight, if any.
pub(super) struct PendingFrame {
    pub(super) frame: protocol::ZwlrScreencopyFrameV1,
    pub(super) output_id: u32,
    pub(super) slot: Option<usize>,
    pub(super) y_invert: bool,
    started: Instant,
}

/// A key this client currently holds.
pub(super) struct HeldKey {
    pub(super) keycode: u8,
    pub(super) modifier: Option<u32>,
    pub(super) synthetic_shift: bool,
}

pub(super) struct WlrootsState {
    conn: Connection,
    qh: QueueHandle<WlrootsState>,
    globals: GlobalList,
    shm: wl_shm::WlShm,
    screencopy: ZwlrScreencopyManagerV1,
    #[allow(dead_code)]
    keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
    power_manager: Option<ZwlrOutputPowerManagerV1>,
    xdg_manager: Option<ZxdgOutputManagerV1>,
    outputs: Vec<OutputEntry>,
    output_globals: HashMap<u32, u32>,
    next_output_id: u32,
    slots: Vec<Slot>,
    pub(super) pending: Option<PendingFrame>,
    dirty: bool,
    pub(super) desktop: Option<Arc<DesktopState>>,
    locked_watch: watch::Receiver<Option<bool>>,
    power_watch: watch::Receiver<bool>,
    pub(super) locked: bool,
    pub(super) suspended: bool,
    pub(super) pointer: Option<ZwlrVirtualPointerV1>,
    pub(super) keyboard: Option<ZwpVirtualKeyboardV1>,
    keymap: Keymap,
    base_keymap: Keymap,
    pub(super) depressed: u32,
    /// How many held virtual keys map to each modifier bit. Several VKs share
    /// one keysym (both Shift keys, both Control keys), so a plain bitmask
    /// would clear the modifier when only one of them is released.
    pub(super) modifier_counts: HashMap<u32, u16>,
    /// Lock state last applied by our own injected Caps/Num Lock keys.
    pub(super) caps_locked: bool,
    pub(super) num_locked: bool,
    pub(super) held: HashMap<i32, HeldKey>,
    pub(super) buttons: std::collections::HashSet<i32>,
    power_objects: HashMap<u32, ZwlrOutputPowerV1>,
    commands: Receiver<Command>,
    wake: UnixStream,
    stop: Arc<AtomicBool>,
    started: Instant,
    next_frame: Instant,
    interval: Duration,
}

struct RunArgs {
    frames: watch::Sender<Option<super::CapturedFrame>>,
    generation: watch::Sender<u64>,
    commands: Option<Receiver<Command>>,
    wake: Option<UnixStream>,
    stop: Arc<AtomicBool>,
    locked: watch::Receiver<Option<bool>>,
    power: watch::Receiver<bool>,
    ready: Option<oneshot::Sender<Result<Arc<DesktopState>, String>>>,
    socket: Option<oneshot::Sender<Option<std::os::fd::OwnedFd>>>,
}

pub(crate) async fn start(options: DesktopOptions) -> Result<DesktopSession, String> {
    if options.source != SourceKind::Monitor {
        return Err(
            "the native wlroots backend captures whole outputs; use --desktop-backend portal \
             for window or virtual sources"
                .to_owned(),
        );
    }
    let (frames, frames_rx) = watch::channel(None);
    let (generation, _) = watch::channel(0u64);
    let (_cursor_tx, cursor_rx) = watch::channel(None);
    let (commands, command_rx) = mpsc::sync_channel(1024);
    let (wake_writer, wake_reader) = UnixStream::pair()
        .map_err(|error| format!("cannot create the input wake pipe: {error}"))?;
    wake_writer
        .set_nonblocking(true)
        .map_err(|error| format!("cannot configure the input wake pipe: {error}"))?;
    wake_reader
        .set_nonblocking(true)
        .map_err(|error| format!("cannot configure the input wake pipe: {error}"))?;
    let wake_writer = Arc::new(wake_writer);
    let input: Arc<dyn InputSink> = Arc::new(Input::new(commands.clone(), wake_writer.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let locked = session_lock::watch_locked();
    let power = super::session_power::RemoteSessionPower::subscribe();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (socket_tx, mut socket_rx) = oneshot::channel();
    let thread_stop = stop.clone();
    std::thread::Builder::new()
        .name("openuur-wlroots".into())
        .spawn(move || {
            run(RunArgs {
                frames,
                generation,
                commands: Some(command_rx),
                wake: Some(wake_reader),
                stop: thread_stop,
                locked,
                power,
                ready: Some(ready_tx),
                socket: Some(socket_tx),
            });
        })
        .map_err(|error| format!("cannot start the wlroots thread: {error}"))?;
    let desktop = match tokio::time::timeout(Duration::from_secs(10), ready_rx).await {
        Ok(Ok(Ok(desktop))) => desktop,
        Ok(Ok(Err(error))) => {
            stop.store(true, Ordering::Release);
            abort_startup(&mut socket_rx);
            return Err(error);
        }
        Ok(Err(_)) | Err(_) => {
            stop.store(true, Ordering::Release);
            abort_startup(&mut socket_rx);
            return Err("the wlroots connection exited during startup".to_owned());
        }
    };
    let commands_for_wake = commands.clone();
    let wake_for_wake = wake_writer;
    let wake_hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        input::request_wake(&commands_for_wake, &wake_for_wake);
    });
    Ok(DesktopSession {
        backend: super::Backend::Wlroots,
        state: desktop,
        frames: frames_rx,
        cursor: cursor_rx,
        input,
        stop,
        task: tokio::spawn(std::future::pending::<()>()),
        wake: Some(wake_hook),
        clipboard: std::sync::Mutex::new(None),
    })
}

/// Check that the compositor exposes the protocols this backend needs.
pub(crate) fn probe() -> Result<(), String> {
    struct Probe;
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    let conn = Connection::connect_to_env()
        .map_err(|error| format!("cannot connect to the Wayland compositor: {error}"))?;
    let (globals, queue) = registry_queue_init::<Probe>(&conn)
        .map_err(|error| format!("cannot read Wayland globals: {error}"))?;
    let _ = queue;
    let contents = globals.contents().clone_list();
    for required in [
        "wl_shm",
        "wl_seat",
        "zwlr_screencopy_manager_v1",
        "zwlr_virtual_pointer_manager_v1",
    ] {
        if !contents.iter().any(|global| global.interface == required) {
            return Err(format!("the compositor does not implement {required}"));
        }
    }
    if !contents
        .iter()
        .any(|global| global.interface == "wl_output")
    {
        return Err("the compositor advertises no outputs".to_owned());
    }
    Ok(())
}
/// Close the Wayland socket after a timed-out startup so the setup thread
/// cannot stay blocked in wayland-client's un-deadlined initial sync.
fn abort_startup(socket: &mut oneshot::Receiver<Option<std::os::fd::OwnedFd>>) {
    if let Ok(Some(fd)) = socket.try_recv() {
        unsafe {
            libc::shutdown(fd.as_raw_fd(), libc::SHUT_RDWR);
        }
    }
}

fn run(mut args: RunArgs) {
    let ready = args
        .ready
        .take()
        .expect("the wlroots thread owns its ready sender");
    match open(&mut args) {
        Ok((mut state, mut queue)) => {
            if let Some(desktop) = state.desktop.clone() {
                let _ = ready.send(Ok(desktop));
                state.event_loop(&mut queue);
            } else {
                let _ = ready.send(Err("the compositor returned no outputs".to_owned()));
            }
        }
        Err(error) => {
            let _ = ready.send(Err(error));
        }
    }
}

fn open(args: &mut RunArgs) -> Result<(WlrootsState, EventQueue<WlrootsState>), String> {
    if args.stop.load(Ordering::Acquire) {
        return Err("wlroots startup was cancelled".to_owned());
    }
    let conn = Connection::connect_to_env()
        .map_err(|error| format!("cannot connect to the Wayland compositor: {error}"))?;
    // Hand the caller a duplicate descriptor so a timed-out startup can shut
    // the socket down: wayland-client's initial sync has no read deadline.
    if let Some(socket) = args.socket.take() {
        let _ = socket.send(conn.backend().poll_fd().try_clone_to_owned().ok());
    }
    let (globals, mut queue) = registry_queue_init::<WlrootsState>(&conn)
        .map_err(|error| format!("cannot read Wayland globals: {error}"))?;
    if args.stop.load(Ordering::Acquire) {
        return Err("wlroots startup was cancelled".to_owned());
    }
    let qh = queue.handle();
    let bind = |interface: &str, range: std::ops::RangeInclusive<u32>| -> Result<(), String> {
        let present = globals
            .contents()
            .clone_list()
            .iter()
            .any(|global| global.interface == interface);
        if present && !range.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "the Wayland compositor does not implement {interface}; \
                 use --desktop-backend portal"
            ))
        }
    };
    bind("wl_shm", 1..=1)?;
    bind("wl_seat", 1..=7)?;
    bind("zwlr_screencopy_manager_v1", 1..=3)?;
    bind("zwlr_virtual_pointer_manager_v1", 1..=2)?;
    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|error| format!("wl_shm: {error}"))?;
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=7, ())
        .map_err(|error| format!("wl_seat: {error}"))?;
    let screencopy: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .map_err(|error| format!("zwlr_screencopy_manager_v1: {error}"))?;
    let pointer_manager: ZwlrVirtualPointerManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|error| format!("zwlr_virtual_pointer_manager_v1: {error}"))?;
    let keyboard_manager: Option<ZwpVirtualKeyboardManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    if keyboard_manager.is_none() {
        tracing::warn!("compositor has no virtual keyboard; remote keys are unavailable");
    }
    let power_manager: Option<ZwlrOutputPowerManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    // Logical output positions; wl_output.geometry is always (0,0) on
    // wlroots, so this protocol is what makes multi-monitor mapping correct.
    let xdg_manager: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 1..=3, ()).ok();
    let keymap = Keymap::load()?;
    let pointer = pointer_manager.create_virtual_pointer(Some(&seat), &qh, ());
    let keyboard = keyboard_manager
        .as_ref()
        .map(|manager| manager.create_virtual_keyboard(&seat, &qh, ()));
    let locked = session_lock::effective_locked(*args.locked.borrow());
    let suspended = *args.power.borrow();
    let mut state = WlrootsState {
        conn,
        qh,
        globals,
        shm,
        screencopy,
        keyboard_manager,
        power_manager,
        xdg_manager,
        outputs: Vec::new(),
        output_globals: HashMap::new(),
        next_output_id: 0,
        slots: Vec::new(),
        pending: None,
        dirty: false,
        desktop: None,
        locked_watch: args.locked.clone(),
        power_watch: args.power.clone(),
        locked,
        suspended,
        pointer: Some(pointer),
        keyboard,
        keymap: keymap.clone(),
        base_keymap: keymap,
        depressed: 0,
        modifier_counts: HashMap::new(),
        caps_locked: false,
        num_locked: false,
        held: HashMap::new(),
        buttons: std::collections::HashSet::new(),
        power_objects: HashMap::new(),
        commands: args
            .commands
            .take()
            .ok_or_else(|| "the wlroots command channel is already in use".to_owned())?,
        wake: args
            .wake
            .take()
            .ok_or_else(|| "the wlroots wake pipe is already in use".to_owned())?,
        stop: args.stop.clone(),
        started: Instant::now(),
        next_frame: Instant::now(),
        interval: Duration::from_millis(1000 / u64::from(display_fps().max(1))),
    };
    let initial_outputs: Vec<(u32, String, u32)> = state
        .globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|global| global.interface == "wl_output")
        .map(|global| (global.name, global.interface, global.version))
        .collect();
    for (name, _, version) in initial_outputs {
        state.bind_output(name, version);
    }
    queue
        .roundtrip(&mut state)
        .map_err(|error| format!("Wayland roundtrip failed: {error}"))?;
    if args.stop.load(Ordering::Acquire) {
        return Err("wlroots startup was cancelled".to_owned());
    }
    if state.outputs.is_empty() {
        return Err("the Wayland compositor advertises no outputs".to_owned());
    }
    let screens = state.screens();
    let active = screens.first().map(|screen| screen.id).unwrap_or(0);
    let desktop = Arc::new(DesktopState::new(
        screens,
        active,
        (0, 0, 0),
        args.frames.clone(),
        args.generation.clone(),
    ));
    state.desktop = Some(desktop);
    state.send_keymap();
    state.set_output_power(true);
    state.conn.flush().ok();
    Ok((state, queue))
}

impl WlrootsState {
    fn event_loop(&mut self, queue: &mut EventQueue<Self>) {
        while !self.stop.load(Ordering::Acquire) {
            if let Err(error) = queue.dispatch_pending(self) {
                tracing::warn!("wlroots event dispatch failed: {error}");
                break;
            }
            self.drain_commands();
            self.refresh_watches();
            if self.dirty {
                self.dirty = false;
                self.publish_screens();
            }
            if !self.flush_connection() {
                break;
            }
            let now = Instant::now();
            if now >= self.next_frame {
                if self.pending.is_none()
                    && !self.locked
                    && !self.suspended
                    && !self.request_frame()
                {
                    break;
                }
                self.next_frame = now + self.interval;
            }
            // A compositor that never answers a frame must not wedge capture.
            if self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.started.elapsed() > Duration::from_secs(2))
            {
                self.abandon_pending();
            }
            if !self.wait(queue, now) {
                break;
            }
        }
        self.release_all();
        self.abandon_pending();
    }

    /// Flush queued requests. A full socket is backpressure, not failure.
    fn flush_connection(&mut self) -> bool {
        match self.conn.flush() {
            Ok(()) => true,
            Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(error) => {
                tracing::debug!("Wayland flush failed: {error}");
                false
            }
        }
    }

    /// Wait for Wayland or input traffic. Returns false once the connection is
    /// unusable (compositor exit, socket error).
    fn wait(&mut self, _queue: &mut EventQueue<Self>, now: Instant) -> bool {
        let timeout = self
            .next_frame
            .saturating_duration_since(now)
            .min(Duration::from_millis(100));
        let mut fds = [
            libc::pollfd {
                fd: self.conn.backend().poll_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                return true;
            }
            tracing::debug!("Wayland poll failed: {error}");
            return false;
        }
        if fds[1].revents & libc::POLLIN != 0 {
            let mut buffer = [0u8; 64];
            use std::io::Read;
            let mut wake = &self.wake;
            while matches!(wake.read(&mut buffer), Ok(size) if size > 0) {}
        }
        if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            tracing::info!("Wayland compositor connection closed");
            return false;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            if let Some(guard) = self.conn.prepare_read() {
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(error))
                        if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => {
                        tracing::debug!("Wayland read failed: {error}");
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Queue one screencopy request and flush it so the wait below can see
    /// the reply. Returns false when the connection is unusable.
    fn request_frame(&mut self) -> bool {
        let active = self
            .desktop
            .as_ref()
            .map(|desktop| desktop.active_id())
            .unwrap_or(0);
        let Some(entry) = self
            .outputs
            .iter()
            .find(|output| output.id == active)
            .or_else(|| self.outputs.first())
        else {
            return true;
        };
        let output = entry.output.clone();
        let output_id = entry.id;
        let frame = self
            .screencopy
            .capture_output(0, &output, &self.qh, FrameData { output_id });
        self.pending = Some(PendingFrame {
            frame,
            output_id,
            slot: None,
            y_invert: false,
            started: Instant::now(),
        });
        self.flush_connection()
    }

    fn refresh_watches(&mut self) {
        if self.locked_watch.has_changed().unwrap_or(false) {
            let value = *self.locked_watch.borrow_and_update();
            let locked = session_lock::effective_locked(value);
            if locked != self.locked {
                self.locked = locked;
                if locked {
                    self.release_all();
                } else {
                    self.next_frame = Instant::now();
                }
            }
        }
        if self.power_watch.has_changed().unwrap_or(false) {
            let sleeping = *self.power_watch.borrow_and_update();
            if sleeping != self.suspended {
                self.suspended = sleeping;
                if sleeping {
                    self.release_all();
                } else {
                    self.set_output_power(true);
                    self.next_frame = Instant::now();
                }
            }
        }
    }

    fn screens(&self) -> Vec<ScreenInfo> {
        let mut screens: Vec<ScreenInfo> = self
            .outputs
            .iter()
            .filter(|output| output.width > 0 && output.height > 0)
            .map(|output| ScreenInfo {
                id: output.id,
                node_id: 0,
                x: output.x,
                y: output.y,
                width: output.width,
                height: output.height,
                primary: false,
            })
            .collect();
        if let Some(first) = screens.first_mut() {
            first.primary = true;
        }
        screens
    }

    /// Logical output layout used for absolute pointer mapping. Pointer
    /// coordinates are in compositor logical space, not buffer pixels.
    pub(super) fn input_screens(&self) -> Vec<ScreenInfo> {
        self.outputs
            .iter()
            .map(|output| {
                let width = if output.logical_width > 0 {
                    output.logical_width
                } else {
                    output.width
                };
                let height = if output.logical_height > 0 {
                    output.logical_height
                } else {
                    output.height
                };
                ScreenInfo {
                    id: output.id,
                    node_id: 0,
                    x: output.logical_x,
                    y: output.logical_y,
                    width,
                    height,
                    primary: false,
                }
            })
            .collect()
    }

    fn publish_screens(&mut self) {
        let Some(desktop) = &self.desktop else {
            return;
        };
        let screens = self.screens();
        // Publish even an empty list so removing the last output propagates.
        let active = screens.first().map(|screen| screen.id).unwrap_or(0);
        desktop.publish(screens, active, (0, 0, 0));
    }

    fn bind_output(&mut self, name: u32, version: u32) {
        if self.output_globals.contains_key(&name) {
            return;
        }
        let id = self.next_output_id;
        let version = version.min(4);
        let Ok(output) = self.globals.bind::<wl_output::WlOutput, _, _>(
            &self.qh,
            1..=version,
            OutputData { id },
        ) else {
            tracing::warn!("cannot bind wl_output {name}");
            return;
        };
        self.next_output_id = self.next_output_id.wrapping_add(1);
        let xdg = self
            .xdg_manager
            .as_ref()
            .map(|manager| manager.get_xdg_output(&output, &self.qh, OutputData { id }));
        self.outputs.push(OutputEntry {
            id,
            output: output.clone(),
            version,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            logical_x: 0,
            logical_y: 0,
            logical_width: 0,
            logical_height: 0,
            xdg,
        });
        self.output_globals.insert(name, id);
        if let Some(manager) = &self.power_manager {
            let power = manager.get_output_power(&output, &self.qh, ());
            self.power_objects.insert(id, power);
        }
        self.dirty = true;
    }

    fn remove_output(&mut self, name: u32) {
        let Some(id) = self.output_globals.remove(&name) else {
            return;
        };
        if let Some(index) = self.outputs.iter().position(|output| output.id == id) {
            let entry = self.outputs.remove(index);
            if let Some(xdg) = entry.xdg {
                xdg.destroy();
            }
            // wl_output.release exists from version 3; older objects can only
            // be dropped (the protocol has no destroy request).
            if entry.version >= 3 {
                entry.output.release();
            }
        }
        if let Some(power) = self.power_objects.remove(&id) {
            power.destroy();
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|frame| frame.output_id == id)
        {
            self.abandon_pending();
        }
        self.dirty = true;
    }

    pub(super) fn set_output_power(&mut self, on: bool) {
        let mode = if on { PowerMode::On } else { PowerMode::Off };
        for power in self.power_objects.values() {
            power.set_mode(mode);
        }
        if !self.flush_connection() {
            tracing::debug!("cannot update output power");
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WlrootsState {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == "wl_output" {
                    state.bind_output(name, version);
                }
            }
            wl_registry::Event::GlobalRemove { name } => state.remove_output(name),
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for WlrootsState {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WlrootsState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for WlrootsState {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, OutputData> for WlrootsState {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as Proxy>::Event,
        data: &OutputData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::Event;
        let Some(output) = state.outputs.iter_mut().find(|output| output.id == data.id) else {
            return;
        };
        match event {
            Event::LogicalPosition { x, y } => {
                output.logical_x = x;
                output.logical_y = y;
                state.dirty = true;
            }
            Event::LogicalSize { width, height } => {
                output.logical_width = width.max(0) as u32;
                output.logical_height = height.max(0) as u32;
                state.dirty = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, OutputData> for WlrootsState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        data: &OutputData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.iter_mut().find(|output| output.id == data.id) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                output.x = x;
                output.y = y;
                state.dirty = true;
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                let current = match flags {
                    WEnum::Value(flags) => flags.contains(wl_output::Mode::Current),
                    WEnum::Unknown(_) => true,
                };
                if current {
                    output.width = width as u32;
                    output.height = height as u32;
                    state.dirty = true;
                }
            }
            wl_output::Event::Done => {
                state.dirty = true;
            }
            wl_output::Event::Scale { .. }
            | wl_output::Event::Name { .. }
            | wl_output::Event::Description { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for WlrootsState {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrOutputPowerManagerV1, ()> for WlrootsState {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputPowerManagerV1,
        _: <ZwlrOutputPowerManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrOutputPowerV1, ()> for WlrootsState {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputPowerV1,
        event: <ZwlrOutputPowerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_v1::Event::Failed =
            event
        {
            tracing::debug!("output power mode request failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use super::super::{start_with, Backend, DesktopOptions};

    struct Sway {
        child: Child,
        runtime: std::path::PathBuf,
    }

    impl Drop for Sway {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.runtime);
        }
    }

    fn start_sway() -> Option<Sway> {
        let runtime = std::env::temp_dir().join(format!("openuur-sway-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir_all(&runtime).ok()?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).ok()?;
        let config = runtime.join("sway.conf");
        std::fs::write(&config, "").ok()?;
        let child = Command::new("sway")
            .arg("-c")
            .arg(&config)
            .arg("-d")
            .env("WLR_BACKENDS", "headless")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_RENDERER", "pixman")
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let sway = Sway {
            child,
            runtime: runtime.clone(),
        };
        // sway picks its own socket name (`wayland-1`, ...); discover it.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let socket = std::fs::read_dir(&runtime).ok().and_then(|entries| {
                entries.flatten().find_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    (name.starts_with("wayland-") && !name.ends_with(".lock")).then_some(name)
                })
            });
            if let Some(display) = socket {
                std::env::set_var("XDG_RUNTIME_DIR", &runtime);
                std::env::set_var("WAYLAND_DISPLAY", &display);
                return Some(sway);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    #[tokio::test]
    #[ignore = "requires sway with the headless backend (nix shell nixpkgs#sway)"]
    async fn sway_headless_captures_and_injects_input() {
        let Some(_sway) = start_sway() else {
            panic!("cannot start sway; run: nix shell nixpkgs#sway");
        };
        let session = tokio::time::timeout(
            Duration::from_secs(15),
            start_with(Backend::Wlroots, DesktopOptions::default()),
        )
        .await
        .expect("wlroots startup timed out")
        .expect("wlroots session");
        assert_eq!(session.screens().len(), 1);
        let frame = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(frame) = session.latest_frame() {
                    break frame;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("no screencopy frame");
        assert!(frame.width > 0 && frame.height > 0);
        assert_eq!(
            frame.data.len(),
            frame.width as usize * frame.height as usize * 4
        );
        // Injected input must not tear down the connection.
        let input = session.input();
        input.pointer_absolute(0.5, 0.5);
        input.button(1, true);
        input.button(1, false);
        input.scroll(0, 120);
        input.key(0x41, true);
        input.key(0x41, false);
        input.text("hello 世界");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(session.latest_frame().is_some());
        session.stop();
    }
}
