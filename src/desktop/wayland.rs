//! Wayland desktop backend: xdg-desktop-portal ScreenCast/RemoteDesktop and
//! PipeWire. A supervisor owns the portal session and rebuilds it after a
//! lock, a compositor-side session close or a display-configuration change,
//! keeping the same `DesktopSession` handle and encoder output settings.

use super::rotation::{frame_matches, orient_frame};
use super::{
    Backend, CapturedFrame, DesktopOptions, DesktopSession, DesktopState, InputSink, ScreenInfo,
    SourceKind,
};
use ashpd::desktop::remote_desktop::RemoteDesktop;
use ashpd::desktop::screencast::SourceType;
use ashpd::desktop::Session;
use lamco_pipewire::{
    convert_format, PipeWireThreadManager, PixelFormat, StreamConfig, VideoFrame,
};
use lamco_portal::{ClipboardManager, PortalConfig, PortalManager, PortalSessionHandle};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, watch};

#[derive(Debug)]
enum InputEvent {
    Absolute {
        x: f64,
        y: f64,
    },
    Relative {
        dx: i32,
        dy: i32,
    },
    Button {
        button: i32,
        pressed: bool,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    Key {
        vk: i32,
        pressed: bool,
    },
    Text {
        text: String,
    },
    /// Release everything this sink may still hold (lock, suspend, peer stop).
    Reset,
}

/// Keys and buttons the viewer pressed through the portal and has not
/// released. The portal has no "release all" call, so the host must send the
/// matching releases before the session stops accepting input.
#[derive(Default)]
struct Held {
    keys: HashSet<i32>,
    buttons: HashSet<i32>,
    /// Keysyms currently pressed by synthesized `Text` input.
    text: HashSet<i32>,
}

impl Held {
    fn track(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Key { vk, pressed } => {
                if *pressed {
                    self.keys.insert(*vk);
                } else {
                    self.keys.remove(vk);
                }
            }
            InputEvent::Button { button, pressed } => {
                if *pressed {
                    self.buttons.insert(*button);
                } else {
                    self.buttons.remove(button);
                }
            }
            _ => {}
        }
    }

    fn keys(&self) -> Vec<i32> {
        self.keys.iter().copied().collect()
    }

    fn buttons(&self) -> Vec<i32> {
        self.buttons.iter().copied().collect()
    }

    fn text(&self) -> Vec<i32> {
        self.text.iter().copied().collect()
    }
}

struct WaylandInput {
    tx: mpsc::UnboundedSender<InputEvent>,
    /// Coalesce resets: the host asks for one per CONTROL message while the
    /// screen is locked, and the supervisor may not be draining (it can be
    /// inside a portal recovery) during that time.
    reset_pending: Arc<AtomicBool>,
}

impl InputSink for WaylandInput {
    fn pointer_absolute(&self, x: f64, y: f64) {
        let _ = self.tx.send(InputEvent::Absolute { x, y });
    }
    fn pointer_relative(&self, dx: i32, dy: i32) {
        let _ = self.tx.send(InputEvent::Relative { dx, dy });
    }
    fn button(&self, button: i32, pressed: bool) {
        let _ = self.tx.send(InputEvent::Button { button, pressed });
    }
    fn scroll(&self, dx: i32, dy: i32) {
        let _ = self.tx.send(InputEvent::Scroll { dx, dy });
    }
    fn key(&self, vk: i32, pressed: bool) {
        let _ = self.tx.send(InputEvent::Key { vk, pressed });
    }
    fn text(&self, text: &str) {
        let _ = self.tx.send(InputEvent::Text {
            text: text.to_owned(),
        });
    }
    fn reset(&self) {
        if !self.reset_pending.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(InputEvent::Reset);
        }
    }
}

/// One live portal session plus its PipeWire streams.
struct Opened {
    manager: PortalManager,
    pipewire: PipeWireThreadManager,
    handle: Arc<PortalSessionHandle>,
    screens: Vec<ScreenInfo>,
    /// EIS input on the same portal session. `None` keeps the Notify path.
    eis: Option<super::eis::EisInput>,
}

impl Opened {
    fn active_target(&self) -> (u32, u32, u32) {
        self.screens
            .first()
            .map(|screen| (screen.node_id, screen.width, screen.height))
            .unwrap_or((0, 0, 0))
    }
}

/// Shared session handle used by the portal clipboard bridge.
pub(crate) type SharedSession = Arc<RwLock<Option<Arc<PortalSessionHandle>>>>;

fn source_type(source: SourceKind) -> SourceType {
    match source {
        SourceKind::Monitor => SourceType::Monitor,
        SourceKind::Window => SourceType::Window,
        SourceKind::Virtual => SourceType::Virtual,
    }
}

/// The viewer draws its own pointer at the coordinates it sends. An embedded
/// compositor cursor therefore appears as a second pointer on GNOME/KDE
/// Wayland, so hide it unless `UU_PORTAL_CURSOR=embedded` asks otherwise.
fn cursor_mode() -> ashpd::desktop::screencast::CursorMode {
    match std::env::var("UU_PORTAL_CURSOR")
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Ok("embedded") => ashpd::desktop::screencast::CursorMode::Embedded,
        _ => ashpd::desktop::screencast::CursorMode::Hidden,
    }
}

fn config(options: &DesktopOptions, restore_token: Option<String>, persist: bool) -> PortalConfig {
    PortalConfig {
        cursor_mode: cursor_mode(),
        source_type: source_type(options.source).into(),
        devices: ashpd::desktop::remote_desktop::DeviceType::Keyboard
            | ashpd::desktop::remote_desktop::DeviceType::Pointer,
        allow_multiple: true,
        // Some portal backends reject persistence for combined
        // RemoteDesktop sessions. `open` retries without it on that error.
        persist_mode: if persist {
            ashpd::desktop::PersistMode::ExplicitlyRevoked
        } else {
            ashpd::desktop::PersistMode::DoNot
        },
        restore_token: if persist { restore_token } else { None },
    }
}

fn persistence_unavailable(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("cannot persist") || error.contains("sessions cannot persist")
}

/// Portal/session errors that end the desktop stream instead of being retried.
///
/// These map to the host's numeric portal results: 4 = consent denied or
/// cancelled, 5 = consent timed out, 6 = required monitor/keyboard/pointer
/// permission missing, 8 = unusable frame format or size, 14 = dimensions
/// changed and a reconnect is required. Reopening in these states only loops
/// permission dialogs or fails forever, so the capture is stopped.
const TERMINAL_PORTAL_CODES: [u8; 5] = [4, 5, 6, 8, 14];

/// Best-effort numeric portal result from a rendered error. The portal crate
/// does not preserve the engine's integer code, so well-known messages are
/// classified; unknown errors stay retryable.
pub fn portal_error_code(error: &str) -> Option<u8> {
    let text = error.to_ascii_lowercase();
    for code in TERMINAL_PORTAL_CODES {
        let plain = format!("code {code}");
        let colon = format!("code: {code}");
        if text.contains(&plain) || text.contains(&colon) {
            return Some(code);
        }
    }
    if text.contains("dimensions changed")
        || text.contains("dimension changed")
        || text.contains("reconnect required")
    {
        return Some(14);
    }
    if text.contains("single monitor")
        || text.contains("keyboard/pointer")
        || text.contains("keyboard and pointer")
        || text.contains("permission required")
    {
        return Some(6);
    }
    if text.contains("timed out") || text.contains("timeout") || text.contains("no response") {
        return Some(5);
    }
    if text.contains("unsupported frame")
        || text.contains("unsupported format")
        || text.contains("unusable frame")
        || text.contains("no streams available")
        || text.contains("no screen stream")
        || text.contains("only empty screen")
    {
        return Some(8);
    }
    if text.contains("denied")
        || text.contains("cancelled")
        || text.contains("canceled")
        || text.contains("permission")
    {
        return Some(4);
    }
    None
}

pub fn terminal_portal_error(error: &str) -> bool {
    portal_error_code(error).is_some_and(|code| TERMINAL_PORTAL_CODES.contains(&code))
}

/// A `Session.Closed` response code that permanently ends this capture.
pub fn terminal_portal_code(code: u32) -> bool {
    TERMINAL_PORTAL_CODES.contains(&(code as u8)) && code <= u8::MAX as u32
}

async fn open(
    options: &DesktopOptions,
    clipboard: Option<&Arc<ClipboardManager>>,
    persistence: &AtomicBool,
    allow_eis: bool,
) -> Result<Opened, String> {
    let key = super::portal_permissions::key(options.source.as_str());
    let (manager, handle, token) = loop {
        let persist = persistence.load(Ordering::Acquire);
        let restore = if persist {
            super::portal_permissions::restore_token(&key).or_else(|| options.restore_token.clone())
        } else {
            None
        };
        let manager = PortalManager::new(config(options, restore, persist))
            .await
            .map_err(|error| format!("xdg-desktop-portal: {error}"))?;
        match manager
            .create_session("openuur-desktop".to_owned(), clipboard.map(Arc::as_ref))
            .await
        {
            Ok((handle, token)) => break (manager, handle, token),
            Err(error) => {
                let message = error.to_string();
                if persist && persistence_unavailable(&message) {
                    tracing::info!(
                        "portal does not persist remote-desktop sessions; retrying without a restore token"
                    );
                    persistence.store(false, Ordering::Release);
                    continue;
                }
                super::portal_permissions::record_permission(&key, false);
                let message = format!("portal session: {error}");
                return Err(match portal_error_code(&message) {
                    Some(code) => format!("{message} (portal code {code})"),
                    None => message,
                });
            }
        }
    };
    if let Some(token) = token.as_deref() {
        super::portal_permissions::save_token(&key, token);
    }
    super::portal_permissions::record_permission(&key, true);
    if handle.streams().is_empty() {
        return Err("portal returned no screen stream".to_owned());
    }
    let portal_fd = handle.pipewire_fd();
    let raw_fd = unsafe { libc::dup(portal_fd) };
    if raw_fd < 0 {
        return Err("cannot duplicate portal PipeWire fd".to_owned());
    }
    // The duplicated descriptor is owned by the PipeWire thread.
    unsafe { libc::close(portal_fd) };
    let pipewire =
        PipeWireThreadManager::new(raw_fd).map_err(|error| format!("PipeWire: {error}"))?;
    let mut screens = Vec::new();
    for (index, stream) in handle.streams().iter().enumerate() {
        if stream.size.0 == 0 || stream.size.1 == 0 {
            continue;
        }
        let id = index as u32;
        let name = format!("openuur-desktop-{id}");
        let stream_config = StreamConfig::new(name)
            .with_resolution(stream.size.0, stream.size.1)
            .with_dmabuf(false)
            .with_buffer_count(3);
        let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
        pipewire
            .send_command(lamco_pipewire::PipeWireThreadCommand::CreateStream {
                stream_id: id,
                node_id: stream.node_id,
                config: stream_config,
                response_tx,
            })
            .map_err(|error| format!("PipeWire create stream {id}: {error}"))?;
        response_rx
            .recv()
            .map_err(|_| format!("PipeWire create stream {id} response lost"))?
            .map_err(|error| format!("PipeWire create stream {id}: {error}"))?;
        screens.push(ScreenInfo {
            id,
            node_id: stream.node_id,
            x: stream.position.0,
            y: stream.position.1,
            width: stream.size.0,
            height: stream.size.1,
            primary: screens.is_empty(),
            mapping_id: stream.mapping_id.clone(),
        });
    }
    if screens.is_empty() {
        return Err("portal returned only empty screen streams".to_owned());
    }
    let eis = if allow_eis && super::eis::EisInput::supported() {
        match screens.iter().find_map(|screen| {
            screen
                .mapping_id
                .as_deref()
                .map(|mapping| (mapping.to_owned(), (screen.width, screen.height)))
        }) {
            Some((mapping, size)) => {
                match super::eis::EisInput::connect(handle.ashpd_session(), &mapping, size).await {
                    Ok(input) => Some(input),
                    Err(error) => {
                        tracing::info!(
                            "portal EIS unavailable ({error}); using legacy portal input"
                        );
                        None
                    }
                }
            }
            None => {
                tracing::debug!("portal stream has no mapping_id; using legacy portal input");
                None
            }
        }
    } else {
        None
    };
    Ok(Opened {
        manager,
        pipewire,
        handle: Arc::new(handle),
        screens,
        eis,
    })
}

pub async fn start(options: DesktopOptions) -> Result<DesktopSession, String> {
    let clipboard_manager = if options.clipboard {
        match ClipboardManager::new().await {
            Ok(manager) => Some(Arc::new(manager)),
            Err(error) => {
                tracing::warn!("portal clipboard unavailable: {error}");
                None
            }
        }
    } else {
        None
    };
    let persistence = Arc::new(AtomicBool::new(
        std::env::var("UU_PORTAL_PERSIST").map_or(true, |value| value != "0"),
    ));
    // Decide the input backend before opening the portal: EIS and uinput are
    // mutually exclusive because ConnectToEIS disables the Notify methods.
    let uinput = super::uinput::wanted(Backend::Wayland)
        .then(super::uinput::UinputInput::open)
        .transpose();
    let use_uinput = uinput.as_ref().is_ok_and(Option::is_some);
    let first = open(
        &options,
        clipboard_manager.as_ref(),
        &persistence,
        !use_uinput,
    )
    .await?;
    let _ = super::desktop_power::wake().await;
    let screens = first.screens.clone();
    let active = screens.first().map(|screen| screen.id).unwrap_or(0);
    let input_target = first.active_target();
    let (frames_tx, frames_rx) = watch::channel(None);
    let (generation_tx, _) = watch::channel(0u64);
    // Wayland has no global cursor observation; the client draws its own.
    let (_cursor_tx, cursor_rx) = watch::channel(None);
    let state = Arc::new(DesktopState::new(
        screens,
        active,
        input_target,
        frames_tx,
        generation_tx,
    ));
    let shared_session: SharedSession = Arc::new(RwLock::new(Some(first.handle.clone())));
    let locked = super::session_lock::watch_locked();
    // The logind inhibitor is owned by the host peers, not by this capture
    // session; the watch only tells capture/input to pause while suspended.
    let power = super::session_power::RemoteSessionPower::subscribe();
    let clipboard = clipboard_manager.as_ref().map(|manager| {
        super::clipboard::start_portal(manager.clone(), shared_session.clone(), locked.clone())
    });
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let reset_pending = Arc::new(AtomicBool::new(false));
    // uinput is layout-independent and works on the lock screen. It is the
    // default for portal sessions when /dev/uinput is writable; the portal
    // Notify path remains the fallback.
    let (input, input_keepalive): (Arc<dyn InputSink>, _) = match uinput {
        Ok(Some(devices)) => {
            tracing::info!("desktop input: uinput virtual keyboard and pointer");
            (Arc::new(devices), Some(input_tx))
        }
        Ok(None) => (
            Arc::new(WaylandInput {
                tx: input_tx,
                reset_pending: reset_pending.clone(),
            }),
            None,
        ),
        Err(error) => {
            tracing::warn!(
                "uinput unavailable ({}); using portal remote-desktop input",
                error.kind()
            );
            (
                Arc::new(WaylandInput {
                    tx: input_tx,
                    reset_pending: reset_pending.clone(),
                }),
                None,
            )
        }
    };
    let cleanup = Arc::new(tokio::sync::Notify::new());
    let stop = Arc::new(AtomicBool::new(false));
    let task = tokio::spawn(supervise(
        first,
        state.clone(),
        input_rx,
        stop.clone(),
        options,
        clipboard_manager,
        shared_session,
        locked,
        power,
        persistence,
        reset_pending,
        input_keepalive,
        cleanup.clone(),
    ));
    let abort = task.abort_handle();
    Ok(DesktopSession {
        backend: Backend::Wayland,
        state,
        frames: frames_rx,
        cursor: cursor_rx,
        input,
        stop,
        abort,
        cleanup,
        wake: None,
        clipboard: std::sync::Mutex::new(clipboard),
        display: None,
        display_guard: std::sync::Mutex::new(None),
    })
}

#[allow(clippy::too_many_arguments)]
async fn supervise(
    mut current: Opened,
    state: Arc<DesktopState>,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    stop: Arc<AtomicBool>,
    options: DesktopOptions,
    clipboard: Option<Arc<ClipboardManager>>,
    shared_session: SharedSession,
    mut locked: watch::Receiver<Option<bool>>,
    mut power: watch::Receiver<bool>,
    persistence: Arc<AtomicBool>,
    reset_pending: Arc<AtomicBool>,
    _input_keepalive: Option<mpsc::UnboundedSender<InputEvent>>,
    cleanup: Arc<tokio::sync::Notify>,
) {
    let allow_eis = _input_keepalive.is_none();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_transform = HashMap::<u32, Option<u32>>::new();
    let mut wheel = super::uinput_scroll::WheelBackend::default();
    let mut held = Held::default();
    let mut reopen_pending = false;
    let (closed_tx, mut closed_rx) = mpsc::channel::<u32>(1);
    spawn_closed_watcher(&current.handle, closed_tx.clone());
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let mut reopen = false;
        tokio::select! {
            _ = tick.tick() => {
                if *power.borrow() {
                    // Drain so the compositor queue cannot back up, and do not
                    // publish stale frames while the machine is suspended.
                    while current.pipewire.try_recv_frame().is_some() {}
                    continue;
                }
                while let Some(frame) = current.pipewire.try_recv_frame() {
                    let Some(screen) = current.screens.iter().find(|screen| screen.id == frame.monitor_index) else { continue; };
                    let raw = (frame.width, frame.height);
                    let transform = frame.transform();
                    if !frame_matches(screen, raw, transform) {
                        tracing::info!(
                            "display configuration changed: {}x{} (transform {}) -> {}x{}; restarting capture",
                            screen.width, screen.height, transform, raw.0, raw.1
                        );
                        reopen = true;
                        break;
                    }
                    if let Some(converted) = convert_frame(
                        frame,
                        screen.width,
                        screen.height,
                        last_transform.entry(screen.id).or_insert(None),
                    ) {
                        if screen.id == state.active.load(Ordering::Acquire) {
                            state.publish_frame(converted);
                        }
                    }
                }
                if *locked.borrow() == Some(true) {
                    state.clear_frame();
                }
            }
            event = input_rx.recv() => {
                let Some(mut event) = event else { break; };
                // Reset is handled even while paused: it is the release path.
                if matches!(event, InputEvent::Reset) {
                    let remote = current.manager.remote_desktop();
                    release_held(
                        &mut current.eis,
                        remote.as_ref(),
                        current.handle.ashpd_session(),
                        &state,
                        &mut held,
                        &mut wheel,
                    )
                    .await;
                    reset_pending.store(false, Ordering::Release);
                    continue;
                }
                if *power.borrow() || *locked.borrow() != Some(false) {
                    // Input is paused. Held state was released on the lock or
                    // suspend transition; releases arriving now cannot reach
                    // the compositor anyway.
                    continue;
                }
                // Long text input checks this between characters so a lock or
                // suspend stops the paste instead of waiting for thousands of
                // D-Bus calls.
                let paused = || {
                    stop.load(Ordering::Acquire)
                        || *power.borrow()
                        || *locked.borrow() != Some(false)
                };
                // Coalesce queued pointer motion so D-Bus calls cannot build a backlog.
                for _ in 0..32 {
                    let Ok(next) = input_rx.try_recv() else { break };
                    let motion = matches!(event, InputEvent::Absolute { .. } | InputEvent::Relative { .. })
                        && matches!(next, InputEvent::Absolute { .. } | InputEvent::Relative { .. });
                    if motion {
                        event = next;
                        continue;
                    }
                    if matches!(event, InputEvent::Reset) {
                        let remote = current.manager.remote_desktop();
                        release_held(
                            &mut current.eis,
                            remote.as_ref(),
                            current.handle.ashpd_session(),
                            &state,
                            &mut held,
                            &mut wheel,
                        )
                        .await;
                        reset_pending.store(false, Ordering::Release);
                    } else {
                        let target = state.input_target.read().map(|value| *value).unwrap_or((0, 0, 0));
                        let delivered = if let Some(eis) = current.eis.as_mut() {
                            dispatch_eis(eis, &state, &event)
                        } else {
                            dispatch_input(
                                current.manager.remote_desktop().as_ref(),
                                current.handle.ashpd_session(),
                                target,
                                &event,
                                &mut wheel,
                                &mut held,
                                &paused,
                            )
                            .await
                        };
                        if delivered {
                            held.track(&event);
                        }
                    }
                    event = next;
                }
                if matches!(event, InputEvent::Reset) {
                    let remote = current.manager.remote_desktop();
                    release_held(
                        &mut current.eis,
                        remote.as_ref(),
                        current.handle.ashpd_session(),
                        &state,
                        &mut held,
                        &mut wheel,
                    )
                    .await;
                    reset_pending.store(false, Ordering::Release);
                    continue;
                }
                let target = state.input_target.read().map(|value| *value).unwrap_or((0, 0, 0));
                let delivered = if let Some(eis) = current.eis.as_mut() {
                    dispatch_eis(eis, &state, &event)
                } else {
                    dispatch_input(
                        current.manager.remote_desktop().as_ref(),
                        current.handle.ashpd_session(),
                        target,
                        &event,
                        &mut wheel,
                        &mut held,
                        &paused,
                    )
                    .await
                };
                if delivered {
                    held.track(&event);
                }
            }
            code = closed_rx.recv() => {
                let code = code.unwrap_or(0);
                if terminal_portal_code(code) {
                    tracing::error!(
                        "portal closed the session with terminal code {code}; not reopening"
                    );
                    state.clear_frame();
                    break;
                }
                tracing::info!("portal capture session closed (code {code}); restarting capture");
                reopen = true;
            }
            changed = locked.changed() => {
                if changed.is_err() {
                    break;
                }
                if *locked.borrow() == Some(false) {
                    // GNOME closes the portal stream while locked; a fresh
                    // unlocked transition gets an immediate recovery attempt.
                    reopen = true;
                } else {
                    // The viewer releases during the lock never reach the
                    // compositor; release what we still hold ourselves.
                    let remote = current.manager.remote_desktop();
                    release_held(
                        &mut current.eis,
                        remote.as_ref(),
                        current.handle.ashpd_session(),
                        &state,
                        &mut held,
                        &mut wheel,
                    )
                    .await;
                }
            }
            changed = power.changed() => {
                if changed.is_err() {
                    break;
                }
                if *power.borrow() {
                    tracing::info!("system suspending; pausing desktop capture");
                    state.clear_frame();
                    let remote = current.manager.remote_desktop();
                    release_held(
                        &mut current.eis,
                        remote.as_ref(),
                        current.handle.ashpd_session(),
                        &state,
                        &mut held,
                        &mut wheel,
                    )
                    .await;
                } else {
                    tracing::info!("system resumed; resuming desktop capture");
                    // Frame transforms and portal sizing may have changed
                    // while the machine was asleep.
                    last_transform.clear();
                }
            }
        }
        if reopen {
            // A portal reopen can prompt the user; defer it until the machine
            // is awake, but remember the request.
            reopen_pending = true;
        }
        if reopen_pending && !*power.borrow() {
            reopen_pending = false;
            // Releases queued behind the reopen would be addressed to the old
            // portal session; release before it is replaced.
            {
                let remote = current.manager.remote_desktop();
                release_held(
                    &mut current.eis,
                    remote.as_ref(),
                    current.handle.ashpd_session(),
                    &state,
                    &mut held,
                    &mut wheel,
                )
                .await;
            }
            match recover(
                &mut current,
                &state,
                &stop,
                &options,
                clipboard.as_ref(),
                &shared_session,
                &mut locked,
                &mut power,
                &closed_tx,
                &persistence,
                allow_eis,
            )
            .await
            {
                Ok(()) => {
                    // Releases that failed against the closed session are
                    // retried on the replacement one.
                    let remote = current.manager.remote_desktop();
                    release_held(
                        &mut current.eis,
                        remote.as_ref(),
                        current.handle.ashpd_session(),
                        &state,
                        &mut held,
                        &mut wheel,
                    )
                    .await;
                }
                Err(error) if terminal_portal_error(&error) => {
                    tracing::error!("portal capture ended and will not be reopened: {error}");
                    state.clear_frame();
                    break;
                }
                Err(error) => {
                    tracing::warn!("portal capture recovery failed: {error}");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
            last_transform.clear();
        }
    }
    // The peer is going away: release anything still pressed before the
    // portal session closes, otherwise the compositor keeps the modifiers and
    // buttons latched for the next session.
    {
        let remote = current.manager.remote_desktop();
        release_held(
            &mut current.eis,
            remote.as_ref(),
            current.handle.ashpd_session(),
            &state,
            &mut held,
            &mut wheel,
        )
        .await;
    }
    let _ = current.pipewire.shutdown();
    // Let `DesktopSession::stop` abort promptly instead of waiting out its
    // grace period once the releases above are done.
    cleanup.notify_one();
    tracing::info!("Wayland desktop capture stopped");
}

fn spawn_closed_watcher(handle: &Arc<PortalSessionHandle>, tx: mpsc::Sender<u32>) {
    use futures_util::StreamExt;
    let handle = handle.clone();
    tokio::spawn(async move {
        if let Ok(mut stream) = handle.ashpd_session().receive_closed().await {
            let code = stream
                .next()
                .await
                .and_then(|fields| {
                    fields
                        .get("response")
                        .or_else(|| fields.get("reason"))
                        .cloned()
                })
                .and_then(|value| u32::try_from(&value).ok())
                .unwrap_or(0);
            let _ = tx.send(code).await;
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn recover(
    current: &mut Opened,
    state: &Arc<DesktopState>,
    stop: &AtomicBool,
    options: &DesktopOptions,
    clipboard: Option<&Arc<ClipboardManager>>,
    shared_session: &SharedSession,
    locked: &mut watch::Receiver<Option<bool>>,
    power: &mut watch::Receiver<bool>,
    closed_tx: &mpsc::Sender<u32>,
    persistence: &AtomicBool,
    allow_eis: bool,
) -> Result<(), String> {
    // Do not prompt or reopen while the session is locked. GNOME reopens the
    // session with the persisted token after unlock.
    while !stop.load(Ordering::Acquire) && (*power.borrow() || *locked.borrow() != Some(false)) {
        tokio::select! {
            changed = power.changed() => {
                if changed.is_err() {
                    return Err("power watcher stopped".to_owned());
                }
            }
            changed = locked.changed() => {
                if changed.is_err() {
                    return Err("lock watcher stopped".to_owned());
                }
            }
        }
    }
    let _ = current.pipewire.shutdown();
    let _ = super::desktop_power::wake().await;
    let mut delay = std::time::Duration::from_millis(250);
    loop {
        if stop.load(Ordering::Acquire) {
            return Err("desktop stopped".to_owned());
        }
        if *power.borrow() {
            if power.changed().await.is_err() {
                return Err("power watcher stopped".to_owned());
            }
            continue;
        }
        loop {
            if stop.load(Ordering::Acquire) {
                return Err("desktop stopped".to_owned());
            }
            let lock_state = *locked.borrow();
            match lock_state {
                Some(false) => break,
                Some(true) => {
                    match tokio::time::timeout(std::time::Duration::from_secs(5), locked.changed())
                        .await
                    {
                        Ok(Ok(())) => continue,
                        Ok(Err(_)) => return Err("lock watcher stopped".to_owned()),
                        Err(_) => continue,
                    }
                }
                // No logind session: do not wait forever for a state that
                // may never be reported.
                None => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    break;
                }
            }
        }
        current.eis = None;
        match open(options, clipboard, persistence, allow_eis).await {
            Ok(opened) => {
                let screens = opened.screens.clone();
                let previous = state.active.load(Ordering::Acquire);
                let active = if screens.iter().any(|screen| screen.id == previous) {
                    previous
                } else {
                    screens
                        .iter()
                        .find(|screen| screen.primary)
                        .or_else(|| screens.first())
                        .map(|screen| screen.id)
                        .unwrap_or(0)
                };
                let target = opened.active_target();
                if let Ok(mut session) = shared_session.write() {
                    *session = Some(opened.handle.clone());
                }
                state.publish(screens, active, target);
                spawn_closed_watcher(&opened.handle, closed_tx.clone());
                *current = opened;
                tracing::info!("portal capture session restored");
                return Ok(());
            }
            Err(error) => {
                if terminal_portal_error(&error) {
                    return Err(error);
                }
                tracing::warn!("portal capture reopen failed: {error}");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(10));
            }
        }
    }
}

fn convert_frame(
    frame: VideoFrame,
    width: u32,
    height: u32,
    last_transform: &mut Option<u32>,
) -> Option<CapturedFrame> {
    let raw_width = frame.width;
    let raw_height = frame.height;
    let transform = frame.transform();
    if *last_transform != Some(transform) {
        tracing::debug!("Wayland frame transform changed to {transform}");
        *last_transform = Some(transform);
    }
    if raw_width != width || raw_height != height {
        let swapped = matches!(transform, 1 | 3 | 5 | 7);
        let oriented = if swapped {
            (raw_height, raw_width)
        } else {
            (raw_width, raw_height)
        };
        if oriented != (width, height) {
            return None;
        }
    }
    let bytes = frame.data()?;
    if bytes.is_empty() {
        return None;
    }
    let packed = raw_width as usize * raw_height as usize * 4;
    let mut raw = vec![0u8; packed];
    convert_format(
        bytes,
        &mut raw,
        frame.format,
        PixelFormat::BGRA,
        raw_width,
        raw_height,
        if frame.stride == 0 {
            raw_width * 4
        } else {
            frame.stride
        },
        raw_width * 4,
    )
    .ok()?;
    if frame.is_bottom_up() {
        let stride = raw_width as usize * 4;
        for row in 0..raw_height as usize / 2 {
            let top = row * stride;
            let bottom = (raw_height as usize - 1 - row) * stride;
            for index in 0..stride {
                raw.swap(top + index, bottom + index);
            }
        }
    }
    let (data, out_width, out_height) = if transform == 0 {
        (raw, raw_width, raw_height)
    } else {
        orient_frame(&raw, raw_width, raw_height, transform)?
    };
    if out_width != width || out_height != height {
        return None;
    }
    Some(CapturedFrame {
        width,
        height,
        data: Arc::new(data),
    })
}
/// A hung portal must not wedge the supervisor: every input call is bounded.
const INPUT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn never_paused() -> bool {
    false
}

async fn timed<E: std::fmt::Display>(
    future: impl std::future::Future<Output = Result<(), E>>,
) -> Result<(), String> {
    match tokio::time::timeout(INPUT_TIMEOUT, future).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("portal input call timed out".to_owned()),
    }
}

async fn release_held(
    eis: &mut Option<super::eis::EisInput>,
    remote: &lamco_portal::RemoteDesktopManager,
    session: &Session<RemoteDesktop>,
    state: &DesktopState,
    held: &mut Held,
    wheel: &mut super::uinput_scroll::WheelBackend,
) {
    if let Some(eis) = eis.as_mut() {
        eis.release_all();
        *held = Held::default();
        return;
    }
    let keys = held.keys();
    let buttons = held.buttons();
    let text = held.text();
    if keys.is_empty() && buttons.is_empty() && text.is_empty() {
        return;
    }
    let target = state
        .input_target
        .read()
        .map(|value| *value)
        .unwrap_or((0, 0, 0));
    // Only forget an entry after the portal accepted its release, so a pass
    // that ran against a session GNOME already closed is retried against the
    // replacement session instead of silently losing the release.
    for vk in keys {
        let release = InputEvent::Key { vk, pressed: false };
        if dispatch_input(
            remote,
            session,
            target,
            &release,
            wheel,
            held,
            &never_paused,
        )
        .await
        {
            held.keys.remove(&vk);
        }
    }
    for button in buttons {
        let release = InputEvent::Button {
            button,
            pressed: false,
        };
        if dispatch_input(
            remote,
            session,
            target,
            &release,
            wheel,
            held,
            &never_paused,
        )
        .await
        {
            held.buttons.remove(&button);
        }
    }
    for keysym in text {
        match timed(remote.notify_keyboard_keysym(session, keysym, false)).await {
            Ok(()) => {
                held.text.remove(&keysym);
            }
            Err(error) => tracing::debug!("Wayland text release failed: {error}"),
        }
    }
}

/// Deliver one event through EIS. EIS coordinates are region-relative and
/// already account for the compositor's scaling and rotation.
fn dispatch_eis(eis: &mut super::eis::EisInput, state: &DesktopState, event: &InputEvent) -> bool {
    let mut can_target_absolute = false;
    if let Some(screen) = state.active_screen() {
        eis.set_target((screen.width, screen.height));
        if let Some(mapping) = screen.mapping_id.as_deref() {
            eis.select_mapping(mapping);
            can_target_absolute = true;
        }
    }
    match event {
        InputEvent::Absolute { x, y } => {
            if can_target_absolute {
                eis.pointer_absolute(*x, *y);
            }
        }
        InputEvent::Relative { dx, dy } => eis.pointer_relative(*dx, *dy),
        InputEvent::Button { button, pressed } => eis.button(*button, *pressed),
        InputEvent::Scroll { dx, dy } => eis.scroll(*dx, *dy),
        InputEvent::Key { vk, pressed } => eis.key(*vk, *pressed),
        InputEvent::Text { text } => eis.text(text),
        InputEvent::Reset => {
            eis.release_all();
        }
    }
    true
}

/// Returns true when the event reached the portal (or was intentionally
/// ignored), so callers only update held state after delivery.
async fn dispatch_input(
    remote: &lamco_portal::RemoteDesktopManager,
    session: &Session<RemoteDesktop>,
    target: (u32, u32, u32),
    event: &InputEvent,
    wheel: &mut super::uinput_scroll::WheelBackend,
    held: &mut Held,
    paused: &(dyn Fn() -> bool + Sync),
) -> bool {
    let (stream_node, width, height) = target;
    let result = match event {
        InputEvent::Absolute { x, y } => {
            let px = x.clamp(0.0, 1.0) * (width.saturating_sub(1)) as f64;
            let py = y.clamp(0.0, 1.0) * (height.saturating_sub(1)) as f64;
            timed(remote.notify_pointer_motion_absolute(session, stream_node, px, py)).await
        }
        InputEvent::Relative { dx, dy } => {
            timed(remote.notify_pointer_motion(session, *dx as f64, *dy as f64)).await
        }
        InputEvent::Button { button, pressed } => {
            let Some(button) = super::input_map::wire_button(*button) else {
                return true;
            };
            timed(remote.notify_pointer_button(session, button, *pressed)).await
        }
        InputEvent::Scroll { dx, dy } => {
            // A real wheel device produces axis_value120, which clients such
            // as Zed require; KWin's portal fake input only sends continuous
            // axis events. Fall back to the portal when uinput is unavailable.
            if wheel.send(*dx, *dy) {
                return true;
            }
            timed(remote.notify_pointer_axis(
                session,
                super::input_map::smooth_scroll(*dx),
                super::input_map::smooth_scroll(*dy),
            ))
            .await
        }
        InputEvent::Key { vk, pressed } => {
            let Some(keysym) = super::input_map::windows_vk_to_keysym(*vk) else {
                return true;
            };
            timed(remote.notify_keyboard_keysym(session, keysym as i32, *pressed)).await
        }
        InputEvent::Text { text } => {
            for ch in text.chars() {
                // A lock or suspend in the middle of a paste must stop it,
                // not queue thousands of calls into the locked session.
                if paused() {
                    return true;
                }
                let keysym = match ch {
                    '\n' | '\r' => 0xff0d,
                    '\t' => 0xff09,
                    c => 0x0100_0000 + c as i32,
                };
                held.text.insert(keysym);
                if let Err(error) =
                    timed(remote.notify_keyboard_keysym(session, keysym, true)).await
                {
                    tracing::debug!("Wayland text press failed: {error}");
                    return false;
                }
                match timed(remote.notify_keyboard_keysym(session, keysym, false)).await {
                    Ok(()) => {
                        held.text.remove(&keysym);
                    }
                    Err(error) => {
                        tracing::debug!("Wayland text release failed: {error}");
                        return false;
                    }
                }
            }
            return true;
        }
        InputEvent::Reset => return true,
    };
    match result {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!("Wayland input failed: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_kinds_map_to_portal_types() {
        assert_eq!(source_type(SourceKind::Monitor), SourceType::Monitor);
        assert_eq!(source_type(SourceKind::Window), SourceType::Window);
        assert_eq!(source_type(SourceKind::Virtual), SourceType::Virtual);
    }

    #[test]
    fn persistent_remote_desktop_rejection_is_recognized() {
        assert!(persistence_unavailable(
            "Session creation failed: Source selection: Portal request failed: org.freedesktop.portal.Error.InvalidArgument: Remote desktop sessions cannot persist"
        ));
        assert!(!persistence_unavailable("permission denied"));
    }

    #[test]
    fn portal_frames_rotate_without_changing_pixels() {
        // 2x2 BGRA: red, green / blue, white.
        let mut source = Vec::new();
        for pixel in [
            [255u8, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ] {
            source.extend_from_slice(&pixel);
        }
        let (rotated, width, height) = orient_frame(&source, 2, 2, 1).unwrap();
        assert_eq!((width, height), (2, 2));
        // 90° CCW moves the top-left pixel to the bottom-left.
        assert_eq!(&rotated[8..12], &source[0..4]);
        assert_eq!(&rotated[0..4], &source[4..8]);
        let (flipped, _, _) = orient_frame(&source, 2, 2, 4).unwrap();
        assert_eq!(&flipped[0..4], &source[4..8]);
        assert_eq!(&flipped[4..8], &source[0..4]);
        let (half, _, _) = orient_frame(&source, 2, 2, 2).unwrap();
        assert_eq!(&half[0..4], &source[12..16]);
        assert!(orient_frame(&source, 2, 2, 8).is_none());
    }

    #[test]
    fn rotation_swaps_rectangular_axes() {
        let mut source = Vec::new();
        source.extend_from_slice(&[1, 2, 3, 4]);
        source.extend_from_slice(&[5, 6, 7, 8]);
        let (ccw, width, height) = orient_frame(&source, 2, 1, 1).unwrap();
        assert_eq!((width, height), (1, 2));
        assert_eq!(ccw, vec![5, 6, 7, 8, 1, 2, 3, 4]);
        let (cw, width, height) = orient_frame(&source, 2, 1, 3).unwrap();
        assert_eq!((width, height), (1, 2));
        assert_eq!(cw, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn frame_geometry_respects_rotation() {
        let screen = ScreenInfo {
            id: 1,
            node_id: 0,
            x: 0,
            y: 0,
            width: 1080,
            height: 1920,
            primary: true,
            mapping_id: None,
        };
        assert!(frame_matches(&screen, (1920, 1080), 1));
        assert!(frame_matches(&screen, (1920, 1080), 3));
        assert!(frame_matches(&screen, (1080, 1920), 0));
        assert!(!frame_matches(&screen, (1080, 1920), 1));
    }

    #[test]
    fn terminal_portal_errors_are_not_reopened() {
        for (text, code) in [
            ("User denied permission", 4),
            ("request cancelled by the user", 4),
            ("permission request timed out", 5),
            ("keyboard/pointer permission required", 6),
            ("unsupported frame format or size", 8),
            ("dimensions changed; reconnect required", 14),
        ] {
            assert_eq!(portal_error_code(text), Some(code), "{text}");
            assert!(terminal_portal_error(text), "{text}");
        }
        assert_eq!(portal_error_code("portal error code 4"), Some(4));
        assert_eq!(
            portal_error_code("Portal request failed (code: 14)"),
            Some(14)
        );
        assert!(terminal_portal_code(8));
        assert!(!terminal_portal_code(9));
        assert_eq!(portal_error_code("PipeWire connection failed"), None);
        assert!(!terminal_portal_error("session closed or stream lost"));
    }
}
