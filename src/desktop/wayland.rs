//! Wayland desktop backend: xdg-desktop-portal ScreenCast/RemoteDesktop and
//! PipeWire. A supervisor owns the portal session and rebuilds it after a
//! lock, a compositor-side session close or a display-configuration change,
//! keeping the same `DesktopSession` handle and encoder output settings.

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
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::{mpsc, watch};

#[derive(Debug)]
enum InputEvent {
    Absolute { x: f64, y: f64 },
    Relative { dx: i32, dy: i32 },
    Button { button: i32, pressed: bool },
    Scroll { dx: i32, dy: i32 },
    Key { vk: i32, pressed: bool },
    Text { text: String },
}

struct WaylandInput {
    tx: mpsc::UnboundedSender<InputEvent>,
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
}

/// One live portal session plus its PipeWire streams.
struct Opened {
    manager: PortalManager,
    pipewire: PipeWireThreadManager,
    handle: Arc<PortalSessionHandle>,
    screens: Vec<ScreenInfo>,
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

fn config(options: &DesktopOptions, restore_token: Option<String>, persist: bool) -> PortalConfig {
    PortalConfig {
        cursor_mode: ashpd::desktop::screencast::CursorMode::Embedded,
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

async fn open(
    options: &DesktopOptions,
    clipboard: Option<&Arc<ClipboardManager>>,
    persistence: &AtomicBool,
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
                return Err(format!("portal session: {error}"));
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
        });
    }
    if screens.is_empty() {
        return Err("portal returned only empty screen streams".to_owned());
    }
    Ok(Opened {
        manager,
        pipewire,
        handle: Arc::new(handle),
        screens,
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
    let first = open(&options, clipboard_manager.as_ref(), &persistence).await?;
    let _ = super::desktop_power::wake().await;
    let inhibitor = match super::desktop_power::IdleInhibitor::acquire().await {
        Ok(inhibitor) => Some(inhibitor),
        Err(error) => {
            tracing::warn!("desktop idle inhibition unavailable: {error}");
            None
        }
    };
    let screens = first.screens.clone();
    let active = screens.first().map(|screen| screen.id).unwrap_or(0);
    let input_target = first.active_target();
    let (frames_tx, frames_rx) = watch::channel(None);
    let (generation_tx, _) = watch::channel(0u64);
    let state = Arc::new(DesktopState::new(
        screens,
        active,
        input_target,
        frames_tx,
        generation_tx,
    ));
    let shared_session: SharedSession = Arc::new(RwLock::new(Some(first.handle.clone())));
    let locked = super::session_lock::watch_locked();
    let clipboard = clipboard_manager.as_ref().map(|manager| {
        super::clipboard::start_portal(manager.clone(), shared_session.clone(), locked.clone())
    });
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let input = Arc::new(WaylandInput { tx: input_tx });
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
        inhibitor,
        persistence,
    ));
    Ok(DesktopSession {
        backend: Backend::Wayland,
        state,
        frames: frames_rx,
        input,
        stop,
        task,
        clipboard: std::sync::Mutex::new(clipboard),
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
    _inhibitor: Option<super::desktop_power::IdleInhibitor>,
    persistence: Arc<AtomicBool>,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_transform = HashMap::<u32, Option<u32>>::new();
    let mut wheel = super::uinput_scroll::WheelBackend::default();
    let (closed_tx, mut closed_rx) = mpsc::channel::<()>(1);
    spawn_closed_watcher(&current.handle, closed_tx.clone());
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let mut reopen = false;
        tokio::select! {
            _ = tick.tick() => {
                while let Some(frame) = current.pipewire.try_recv_frame() {
                    let Some(screen) = current.screens.iter().find(|screen| screen.id == frame.monitor_index) else { continue; };
                    if frame.width != screen.width || frame.height != screen.height {
                        tracing::info!(
                            "display configuration changed: {}x{} -> {}x{}; restarting capture",
                            screen.width, screen.height, frame.width, frame.height
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
                if *locked.borrow() != Some(false) {
                    continue;
                }
                // Coalesce queued pointer motion so D-Bus calls cannot build a backlog.
                for _ in 0..32 {
                    let Ok(next) = input_rx.try_recv() else { break };
                    let motion = matches!(event, InputEvent::Absolute { .. } | InputEvent::Relative { .. })
                        && matches!(next, InputEvent::Absolute { .. } | InputEvent::Relative { .. });
                    if motion {
                        event = next;
                    } else {
                        let target = state.input_target.read().map(|value| *value).unwrap_or((0, 0, 0));
                        dispatch_input(
                            current.manager.remote_desktop().as_ref(),
                            current.handle.ashpd_session(),
                            target,
                            event,
                            &mut wheel,
                        )
                        .await;
                        event = next;
                    }
                }
                let target = state.input_target.read().map(|value| *value).unwrap_or((0, 0, 0));
                dispatch_input(
                    current.manager.remote_desktop().as_ref(),
                    current.handle.ashpd_session(),
                    target,
                    event,
                    &mut wheel,
                )
                .await;
            }
            _ = closed_rx.recv() => {
                tracing::info!("portal capture session closed; restarting capture");
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
                }
            }
        }
        if reopen {
            if let Err(error) = recover(
                &mut current,
                &state,
                &stop,
                &options,
                clipboard.as_ref(),
                &shared_session,
                &mut locked,
                &closed_tx,
                &persistence,
            )
            .await
            {
                tracing::warn!("portal capture recovery failed: {error}");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            last_transform.clear();
        }
    }
    let _ = current.pipewire.shutdown();
    tracing::info!("Wayland desktop capture stopped");
}

fn spawn_closed_watcher(handle: &Arc<PortalSessionHandle>, tx: mpsc::Sender<()>) {
    use futures_util::StreamExt;
    let handle = handle.clone();
    tokio::spawn(async move {
        if let Ok(mut stream) = handle.ashpd_session().receive_closed().await {
            let _ = stream.next().await;
            let _ = tx.send(()).await;
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
    closed_tx: &mpsc::Sender<()>,
    persistence: &AtomicBool,
) -> Result<(), String> {
    // Do not prompt or reopen while the session is locked. GNOME reopens the
    // session with the persisted token after unlock.
    while !stop.load(Ordering::Acquire) && *locked.borrow() != Some(false) {
        if locked.changed().await.is_err() {
            return Err("lock watcher stopped".to_owned());
        }
    }
    let _ = current.pipewire.shutdown();
    let _ = super::desktop_power::wake().await;
    let mut delay = std::time::Duration::from_millis(250);
    loop {
        if stop.load(Ordering::Acquire) {
            return Err("desktop stopped".to_owned());
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
        match open(options, clipboard, persistence).await {
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
    if frame.width != width || frame.height != height {
        return None;
    }
    let bytes = frame.data()?;
    if bytes.is_empty() {
        return None;
    }
    let transform = frame.transform();
    if transform != 0 {
        if *last_transform != Some(transform) {
            tracing::warn!("Wayland frame transform {transform} is not implemented");
            *last_transform = Some(transform);
        }
        return None;
    }
    let packed = width as usize * height as usize * 4;
    let mut data = vec![0u8; packed];
    convert_format(
        bytes,
        &mut data,
        frame.format,
        PixelFormat::BGRA,
        width,
        height,
        if frame.stride == 0 {
            width * 4
        } else {
            frame.stride
        },
        width * 4,
    )
    .ok()?;
    if frame.is_bottom_up() {
        let stride = width as usize * 4;
        for row in 0..height as usize / 2 {
            let top = row * stride;
            let bottom = (height as usize - 1 - row) * stride;
            for i in 0..stride {
                data.swap(top + i, bottom + i);
            }
        }
    }
    Some(CapturedFrame {
        width,
        height,
        data: Arc::new(data),
    })
}

async fn dispatch_input(
    remote: &lamco_portal::RemoteDesktopManager,
    session: &Session<RemoteDesktop>,
    target: (u32, u32, u32),
    event: InputEvent,
    wheel: &mut super::uinput_scroll::WheelBackend,
) {
    let (stream_node, width, height) = target;
    let result = match event {
        InputEvent::Absolute { x, y } => {
            let px = x.clamp(0.0, 1.0) * (width.saturating_sub(1)) as f64;
            let py = y.clamp(0.0, 1.0) * (height.saturating_sub(1)) as f64;
            remote
                .notify_pointer_motion_absolute(session, stream_node, px, py)
                .await
        }
        InputEvent::Relative { dx, dy } => {
            remote
                .notify_pointer_motion(session, dx as f64, dy as f64)
                .await
        }
        InputEvent::Button { button, pressed } => {
            let Some(button) = wire_button(button) else {
                return;
            };
            remote.notify_pointer_button(session, button, pressed).await
        }
        InputEvent::Scroll { dx, dy } => {
            // A real wheel device produces axis_value120, which clients such
            // as Zed require; KWin's portal fake input only sends continuous
            // axis events. Fall back to the portal when uinput is unavailable.
            if wheel.send(dx, dy) {
                return;
            }
            remote
                .notify_pointer_axis(session, smooth_scroll(dx), smooth_scroll(dy))
                .await
        }
        InputEvent::Key { vk, pressed } => {
            let Some(keysym) = windows_vk_to_keysym(vk) else {
                return;
            };
            remote
                .notify_keyboard_keysym(session, keysym as i32, pressed)
                .await
        }
        InputEvent::Text { text } => {
            for ch in text.chars() {
                let keysym = match ch {
                    '\n' | '\r' => 0xff0d,
                    '\t' => 0xff09,
                    c => 0x0100_0000 + c as i32,
                };
                if let Err(error) = remote.notify_keyboard_keysym(session, keysym, true).await {
                    tracing::debug!("Wayland text press failed: {error}");
                    return;
                }
                let _ = remote.notify_keyboard_keysym(session, keysym, false).await;
            }
            return;
        }
    };
    if let Err(error) = result {
        tracing::warn!("Wayland input failed: {error}");
    }
}

fn wire_button(button: i32) -> Option<i32> {
    // UU uses 1/2/4 for left/right/middle. The portal RemoteDesktop API
    // expects Linux evdev button codes (BTN_LEFT/BTN_RIGHT/BTN_MIDDLE),
    // not X11 or UU button numbers.
    match button {
        1 => Some(0x110),
        2 => Some(0x111),
        4 => Some(0x112),
        _ => None,
    }
}

/// Normalize UU wheel deltas to portal axis units.
///
/// The official client sends smooth pixel deltas (tens per event), so those
/// pass through unchanged, matching the portal's "same as pointer motion"
/// contract. Windows wheel deltas (120 per notch) map to one 10-unit axis
/// step, and notch counts (1..5) map to 10 units each.
fn smooth_scroll(value: i32) -> f64 {
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

/// Windows virtual-key to XKB keysym. The portal backend translates keysyms
/// against the active keyboard layout; unknown layout-specific keys are ignored.
fn windows_vk_to_keysym(vk: i32) -> Option<u32> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_buttons_follow_uuit_bitmask() {
        // UU 1/2/4 -> evdev BTN_LEFT/BTN_RIGHT/BTN_MIDDLE.
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
        // Smooth pixel deltas pass through unchanged.
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
}
