//! Local Wayland/X11 text clipboard bridge.
//!
//! KDE exposes the clipboard through `org.kde.klipper`; wl-clipboard is used
//! on compositors that provide it. Only UTF-8 text is handled. Clipboard
//! contents are never logged.

#[cfg(all(target_os = "linux", feature = "desktop"))]
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::watch;

pub const MAX_TEXT: usize = 30 * 1024;

/// Focus generation. Any desktop input marks the remote view focused, which
/// makes the portal clipboard re-read the local selection (GNOME clipboard is
/// focus-aware). The official viewer does the same on FocusGained.
fn focus_sender() -> &'static watch::Sender<u64> {
    static FOCUS: std::sync::OnceLock<watch::Sender<u64>> = std::sync::OnceLock::new();
    FOCUS.get_or_init(|| watch::channel(0u64).0)
}

pub fn mark_focus() {
    focus_sender().send_modify(|generation| *generation = generation.wrapping_add(1));
}

pub fn focus_receiver() -> watch::Receiver<u64> {
    focus_sender().subscribe()
}

#[cfg(all(target_os = "linux", feature = "desktop"))]
fn text_mime(mime: &str) -> bool {
    matches!(
        mime,
        "text/plain;charset=utf-8" | "text/plain" | "UTF8_STRING"
    )
}

#[derive(Clone)]
pub struct ClipboardBridge {
    pub incoming: watch::Sender<Option<String>>,
    pub outgoing: watch::Receiver<Option<String>>,
    pub ready: watch::Receiver<bool>,
}

pub fn start() -> ClipboardBridge {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    let x11 = std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
    // Pure X11 always uses the native XFixes selection; never require xclip,
    // xsel or a desktop-specific helper there.
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    if !wayland && x11 {
        return start_x11();
    }
    if command_exists("qdbus") && qdbus_clipboard_works() {
        return start_qdbus();
    }
    // wl-copy/wl-paste are useless without a Wayland compositor.
    if wayland && command_exists("wl-copy") && command_exists("wl-paste") {
        return start_wl();
    }
    // A Wayland session without wl-clipboard may still expose a working
    // XWayland selection.
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    if x11 {
        return start_x11();
    }
    unavailable()
}

fn unavailable() -> ClipboardBridge {
    let (incoming, _) = watch::channel(None);
    let (_, outgoing) = watch::channel(None);
    let (ready_tx, ready) = watch::channel(false);
    ready_tx.send_replace(false);
    ClipboardBridge {
        incoming,
        outgoing,
        ready,
    }
}

fn command_exists(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
    })
}

fn qdbus_clipboard_works() -> bool {
    std::process::Command::new("qdbus")
        .args(["org.kde.klipper", "/klipper", "getClipboardContents"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn start_qdbus() -> ClipboardBridge {
    let (incoming, mut input) = watch::channel::<Option<String>>(None);
    let (output, outgoing) = watch::channel::<Option<String>>(None);
    let (ready_tx, ready) = watch::channel(false);
    tokio::spawn(async move {
        ready_tx.send_replace(true);
        let mut last = None::<String>;
        let mut tick = tokio::time::interval(Duration::from_millis(300));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = output.closed() => break,
                changed = input.changed() => {
                    if changed.is_err() { break; }
                    let Some(text) = input.borrow_and_update().clone() else { continue; };
                    if text.len() > MAX_TEXT { continue; }
                    let result = Command::new("qdbus")
                        .args(["org.kde.klipper", "/klipper", "setClipboardContents", &text])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .await;
                    if result.is_ok_and(|status| status.success()) {
                        last = Some(text);
                    }
                }
                _ = tick.tick() => {
                    let Ok(result) = Command::new("qdbus")
                        .args(["org.kde.klipper", "/klipper", "getClipboardContents"])
                        .stdin(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .output()
                        .await
                    else { continue; };
                    if !result.status.success() || result.stdout.len() > MAX_TEXT { continue; }
                    let Ok(mut text) = String::from_utf8(result.stdout) else { continue; };
                    if let Some(stripped) = text.strip_suffix('\n') {
                        text = stripped.to_owned();
                    }
                    if last.as_deref() != Some(text.as_str()) {
                        last = Some(text.clone());
                        output.send_replace(Some(text));
                    }
                }
            }
        }
    });
    ClipboardBridge {
        incoming,
        outgoing,
        ready,
    }
}

fn start_wl() -> ClipboardBridge {
    let (incoming, mut input) = watch::channel::<Option<String>>(None);
    let (output, outgoing) = watch::channel::<Option<String>>(None);
    let (ready_tx, ready) = watch::channel(false);
    tokio::spawn(async move {
        ready_tx.send_replace(true);
        let mut owner: Option<Child> = None;
        let mut last = None::<String>;
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = output.closed() => break,
                changed = input.changed() => {
                    if changed.is_err() { break; }
                    let Some(text) = input.borrow_and_update().clone() else { continue; };
                    if text.len() > MAX_TEXT { continue; }
                    if let Some(mut child) = owner.take() { let _ = child.kill().await; }
                    if let Ok(mut child) = Command::new("wl-copy")
                        .args(["--foreground", "--type", "text/plain;charset=utf-8"])
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .spawn()
                    {
                        if let Some(mut stdin) = child.stdin.take() {
                            if stdin.write_all(text.as_bytes()).await.is_ok() {
                                last = Some(text);
                            }
                        }
                        owner = Some(child);
                    }
                }
                _ = tick.tick() => {
                    let Ok(mut child) = Command::new("wl-paste")
                        .args(["--no-newline", "--type", "text"])
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .spawn()
                    else { continue; };
                    let Some(stdout) = child.stdout.take() else { continue; };
                    let mut bytes = Vec::new();
                    if stdout.take((MAX_TEXT + 1) as u64).read_to_end(&mut bytes).await.is_err() { continue; }
                    if bytes.len() > MAX_TEXT || !child.wait().await.is_ok_and(|s| s.success()) { continue; }
                    let Ok(text) = String::from_utf8(bytes) else { continue; };
                    if last.as_deref() != Some(text.as_str()) {
                        last = Some(text.clone());
                        output.send_replace(Some(text));
                    }
                }
            }
        }
        if let Some(mut child) = owner {
            let _ = child.kill().await;
        }
    });
    ClipboardBridge {
        incoming,
        outgoing,
        ready,
    }
}

/// Portal clipboard bridge for GNOME/KDE Wayland sessions.
///
/// Local selection changes arrive through `SelectionOwnerChanged`; viewer text
/// is announced with `SetSelection` and served through `SelectionTransfer`.
/// The session handle is replaced by the capture supervisor after recovery;
/// the bridge always reads the current one. Nothing is transferred while the
/// session is locked.
#[cfg(all(target_os = "linux", feature = "desktop"))]
pub fn start_portal(
    manager: Arc<lamco_portal::ClipboardManager>,
    session: super::wayland::SharedSession,
    mut locked: watch::Receiver<Option<bool>>,
) -> ClipboardBridge {
    use futures_util::StreamExt;

    let (incoming, mut incoming_rx) = watch::channel::<Option<String>>(None);
    let (outgoing, outgoing_rx) = watch::channel::<Option<String>>(None);
    let (ready_tx, ready) = watch::channel(false);
    let pending = Arc::new(std::sync::Mutex::new(None::<String>));
    let last_outgoing = Arc::new(std::sync::Mutex::new(None::<String>));
    let last_mime = Arc::new(std::sync::Mutex::new(None::<String>));

    {
        let manager = manager.clone();
        let outgoing = outgoing.clone();
        let last_mime = last_mime.clone();
        let pending = pending.clone();
        tokio::spawn(async move {
            let Ok(clipboard) = ashpd::desktop::clipboard::Clipboard::new().await else {
                return;
            };
            let Ok(owner_stream) = clipboard
                .receive_selection_owner_changed::<ashpd::desktop::remote_desktop::RemoteDesktop>()
                .await
            else {
                return;
            };
            let Ok(transfer_stream) = clipboard
                .receive_selection_transfer::<ashpd::desktop::remote_desktop::RemoteDesktop>()
                .await
            else {
                return;
            };
            let mut owner = Box::pin(owner_stream);
            let mut transfer = Box::pin(transfer_stream);
            loop {
                tokio::select! {
                    event = owner.next() => {
                        let Some((session, change)) = event else { break };
                        if change.session_is_owner().unwrap_or(false) {
                            continue;
                        }
                        let Some(mime) = change
                            .mime_types()
                            .iter()
                            .find(|mime| text_mime(mime))
                            .cloned()
                        else {
                            continue;
                        };
                        if let Ok(mut current) = last_mime.lock() {
                            *current = Some(mime.clone());
                        }
                        let Ok(bytes) = manager.read_local_clipboard(&session, &mime).await else {
                            continue;
                        };
                        if bytes.len() > MAX_TEXT {
                            continue;
                        }
                        let Ok(text) = String::from_utf8(bytes) else {
                            continue;
                        };
                        outgoing.send_replace(Some(text));
                    }
                    event = transfer.next() => {
                        let Some((session, mime, serial)) = event else { break };
                        if !text_mime(&mime) {
                            continue;
                        }
                        let text = pending.lock().ok().and_then(|mut value| value.take());
                        let Some(text) = text else {
                            let _ = clipboard
                                .selection_write_done(&session, serial, false)
                                .await;
                            continue;
                        };
                        let _ = manager
                            .write_selection_data(&session, serial, text.into_bytes())
                            .await;
                    }
                }
            }
        });
    }

    {
        let manager = manager.clone();
        let pending = pending.clone();
        let last_outgoing = last_outgoing.clone();
        let last_mime = last_mime.clone();
        let mut focus = focus_receiver();
        tokio::spawn(async move {
            ready_tx.send_replace(true);
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = outgoing.closed() => break,
                    changed = incoming_rx.changed() => {
                        if changed.is_err() { break; }
                        if *locked.borrow() != Some(false) { continue; }
                        let Some(text) = incoming_rx.borrow_and_update().clone() else { continue; };
                        if text.len() > MAX_TEXT { continue; }
                        let Some(handle) = session.read().ok().and_then(|session| session.clone()) else {
                            continue;
                        };
                        if let Ok(mut pending) = pending.lock() {
                            *pending = Some(text.clone());
                        }
                        let mimes = ["text/plain;charset=utf-8", "text/plain"];
                        let options = ashpd::desktop::clipboard::SetSelectionOptions::default()
                            .set_mime_types(&mimes);
                        match manager
                            .portal_clipboard()
                            .set_selection(handle.ashpd_session(), options)
                            .await
                        {
                            Ok(()) => {
                                if let Ok(mut last) = last_outgoing.lock() {
                                    *last = Some(text);
                                }
                            }
                            Err(error) => {
                                tracing::debug!("portal clipboard set selection failed: {error}");
                                if let Ok(mut pending) = pending.lock() {
                                    *pending = None;
                                }
                            }
                        }
                    }
                    changed = focus.changed() => {
                        if changed.is_err() { break; }
                        if *locked.borrow() != Some(false) { continue; }
                        let Some(handle) = session.read().ok().and_then(|session| session.clone()) else {
                            continue;
                        };
                        let mime = last_mime
                            .lock()
                            .ok()
                            .and_then(|value| value.clone())
                            .unwrap_or_else(|| "text/plain;charset=utf-8".to_owned());
                        let Ok(bytes) = manager.read_local_clipboard(handle.ashpd_session(), &mime).await else {
                            continue;
                        };
                        if bytes.len() > MAX_TEXT { continue; }
                        let Ok(text) = String::from_utf8(bytes) else { continue; };
                        let already = last_outgoing
                            .lock()
                            .ok()
                            .is_some_and(|last| last.as_deref() == Some(text.as_str()));
                        if !already {
                            outgoing.send_replace(Some(text));
                        }
                    }
                    changed = locked.changed() => {
                        if changed.is_err() { break; }
                        if *locked.borrow() == Some(true) {
                            if let Ok(mut pending) = pending.lock() { *pending = None; }
                        }
                    }
                    _ = tick.tick() => {
                        if *locked.borrow() != Some(false) { continue; }
                        // Some backends omit SelectionOwnerChanged; a slow poll
                        // still picks up local text without echoing our own
                        // announcement.
                        let Some(handle) = session.read().ok().and_then(|session| session.clone()) else {
                            continue;
                        };
                        let mime = last_mime
                            .lock()
                            .ok()
                            .and_then(|value| value.clone())
                            .unwrap_or_else(|| "text/plain;charset=utf-8".to_owned());
                        let Ok(bytes) = manager.read_local_clipboard(handle.ashpd_session(), &mime).await else {
                            continue;
                        };
                        if bytes.len() > MAX_TEXT { continue; }
                        let Ok(text) = String::from_utf8(bytes) else { continue; };
                        let already = last_outgoing
                            .lock()
                            .ok()
                            .is_some_and(|last| last.as_deref() == Some(text.as_str()));
                        if !already {
                            outgoing.send_replace(Some(text));
                        }
                    }
                }
            }
        });
    }

    ClipboardBridge {
        incoming,
        outgoing: outgoing_rx,
        ready,
    }
}

/// Native X11 CLIPBOARD owner and XFixes observer.
///
/// One thread owns a private connection, a 1x1 window and the selection
/// handshake. Local changes claim `CLIPBOARD`; another client's copy arrives
/// through XFixes and is requested as UTF8_STRING (falling back to STRING or
/// TEXT when the owner refuses). Only complete UTF-8 text within [`MAX_TEXT`]
/// is forwarded; INCR transfers and non-text targets are ignored rather than
/// guessed.
#[cfg(all(target_os = "linux", feature = "desktop"))]
pub fn start_x11() -> ClipboardBridge {
    start_x11_at(None)
}

/// Start the native X11 bridge against an explicit server (tests, alternate
/// displays). `None` uses the ambient `DISPLAY`.
#[cfg(all(target_os = "linux", feature = "desktop"))]
pub fn start_x11_at(display: Option<&str>) -> ClipboardBridge {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xfixes::{self, SelectionEventMask};
    use x11rb::protocol::xproto::{AtomEnum, EventMask, WindowClass};
    use x11rb::protocol::Event;

    struct X11 {
        connection: x11rb::rust_connection::RustConnection,
        window: u32,
        selection: u32,
        utf8: u32,
        text: u32,
        string: u32,
        targets: u32,
        property_targets: u32,
        property_text: u32,
        owned: Option<Vec<u8>>,
        owns: bool,
        pending: Option<Pending>,
    }

    enum Pending {
        Targets { time: u32 },
        Text,
    }

    fn intern(connection: &impl x11rb::connection::Connection, name: &[u8]) -> Option<u32> {
        x11rb::protocol::xproto::intern_atom(connection, false, name)
            .ok()?
            .reply()
            .ok()
            .map(|reply| reply.atom)
    }

    fn setup(display: Option<&str>) -> Result<X11, String> {
        let (connection, screen_num) = x11rb::connect(display)
            .map_err(|error| format!("X11 clipboard connection: {error}"))?;
        let root = connection.setup().roots[screen_num].root;
        xfixes::query_version(&connection, 6, 0)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        let window = connection
            .generate_id()
            .map_err(|error| format!("X11 clipboard window id: {error}"))?;
        x11rb::protocol::xproto::create_window(
            &connection,
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &x11rb::protocol::xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| error.to_string())?;
        let selection = intern(&connection, b"CLIPBOARD")
            .ok_or_else(|| "X11 CLIPBOARD atom unavailable".to_owned())?;
        let utf8 = intern(&connection, b"UTF8_STRING")
            .ok_or_else(|| "X11 UTF8_STRING atom unavailable".to_owned())?;
        let text =
            intern(&connection, b"TEXT").ok_or_else(|| "X11 TEXT atom unavailable".to_owned())?;
        let string = u32::from(AtomEnum::STRING);
        let targets = intern(&connection, b"TARGETS")
            .ok_or_else(|| "X11 TARGETS atom unavailable".to_owned())?;
        let property_targets = intern(&connection, b"OPENUUR_CLIPBOARD_TARGETS")
            .ok_or_else(|| "X11 clipboard property unavailable".to_owned())?;
        let property_text = intern(&connection, b"OPENUUR_CLIPBOARD_TEXT")
            .ok_or_else(|| "X11 clipboard property unavailable".to_owned())?;
        let mask = SelectionEventMask::SET_SELECTION_OWNER
            | SelectionEventMask::SELECTION_WINDOW_DESTROY
            | SelectionEventMask::SELECTION_CLIENT_CLOSE;
        xfixes::select_selection_input(&connection, window, selection, mask)
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| error.to_string())?;
        connection.flush().map_err(|error| error.to_string())?;
        Ok(X11 {
            connection,
            window,
            selection,
            utf8,
            text,
            string,
            targets,
            property_targets,
            property_text,
            owned: None,
            owns: false,
            pending: None,
        })
    }

    fn answer(request: &x11rb::protocol::xproto::SelectionRequestEvent, state: &X11) {
        use x11rb::protocol::xproto::{self, AtomEnum, PropMode};
        if request.selection != state.selection || !state.owns {
            return;
        }
        let Some(data) = &state.owned else {
            return;
        };
        let property = if request.property == x11rb::NONE {
            request.target
        } else {
            request.property
        };
        let reply = x11rb::protocol::xproto::SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: x11rb::NONE,
        };
        let mut reply = reply;
        if request.target == state.targets {
            let atoms = [state.targets, state.utf8, state.text, state.string];
            if xproto::change_property(
                &state.connection,
                PropMode::REPLACE,
                request.requestor,
                property,
                AtomEnum::ATOM,
                32,
                atoms.len() as u32,
                &atoms
                    .iter()
                    .flat_map(|atom| atom.to_le_bytes())
                    .collect::<Vec<_>>(),
            )
            .is_ok()
            {
                reply.property = property;
            }
        } else if [state.utf8, state.text, state.string].contains(&request.target)
            && xproto::change_property(
                &state.connection,
                PropMode::REPLACE,
                request.requestor,
                property,
                state.utf8,
                8,
                data.len() as u32,
                data,
            )
            .is_ok()
        {
            reply.property = property;
        }
        let _ = xproto::send_event(
            &state.connection,
            false,
            request.requestor,
            EventMask::NO_EVENT,
            reply,
        );
        let _ = state.connection.flush();
    }

    fn request_text(state: &mut X11, target: u32, time: u32) {
        use x11rb::protocol::xproto;
        let _ = xproto::delete_property(&state.connection, state.window, state.property_text);
        let _ = xproto::convert_selection(
            &state.connection,
            state.window,
            state.selection,
            target,
            state.property_text,
            time,
        );
        let _ = state.connection.flush();
    }

    fn handle_selection_notify(
        state: &mut X11,
        event: &x11rb::protocol::xproto::SelectionNotifyEvent,
        outgoing: &watch::Sender<Option<String>>,
    ) {
        use x11rb::protocol::xproto::{self, AtomEnum};
        if event.selection != state.selection {
            return;
        }
        let Some(pending) = state.pending.take() else {
            return;
        };
        if event.property == x11rb::NONE {
            // The owner refused TARGETS. Many minimal X11 applications still
            // serve UTF8_STRING directly; ask once before giving up.
            if let Pending::Targets { time } = pending {
                request_text(state, state.utf8, time);
                state.pending = Some(Pending::Text);
            }
            return;
        }
        let property_matches = match pending {
            Pending::Targets { .. } => event.property == state.property_targets,
            Pending::Text => event.property == state.property_text,
        };
        if !property_matches {
            state.pending = Some(pending);
            return;
        }
        let length = MAX_TEXT.div_ceil(4);
        let Some(reply) = xproto::get_property(
            &state.connection,
            true,
            state.window,
            event.property,
            AtomEnum::ANY,
            0,
            length as u32,
        )
        .ok()
        .and_then(|cookie| cookie.reply().ok()) else {
            return;
        };
        let _ = xproto::delete_property(&state.connection, state.window, event.property);
        match pending {
            Pending::Targets { time } => {
                if event.property != state.property_targets || reply.format != 32 {
                    return;
                }
                let targets: Vec<u32> = reply
                    .value32()
                    .map(|values| values.collect())
                    .unwrap_or_default();
                let target = [state.utf8, state.string, state.text]
                    .into_iter()
                    .find(|candidate| targets.contains(candidate));
                if let Some(target) = target {
                    request_text(state, target, time);
                    state.pending = Some(Pending::Text);
                    let _ = targets;
                }
            }
            Pending::Text => {
                if event.property != state.property_text
                    || reply.format != 8
                    || reply.value.len() > MAX_TEXT
                    || reply.type_ != state.utf8
                        && reply.type_ != state.text
                        && reply.type_ != state.string
                {
                    return;
                }
                let Ok(text) = String::from_utf8(reply.value) else {
                    return;
                };
                outgoing.send_replace(Some(text));
            }
        }
    }

    let (incoming, mut input) = watch::channel::<Option<String>>(None);
    let (output, outgoing) = watch::channel::<Option<String>>(None);
    let (ready_tx, ready) = watch::channel(false);
    let display = display.map(str::to_owned);
    std::thread::Builder::new()
        .name("openuur-x11-clipboard".into())
        .spawn(move || {
            let mut state = match setup(display.as_deref()) {
                Ok(state) => state,
                Err(error) => {
                    tracing::warn!("native X11 clipboard unavailable: {error}");
                    ready_tx.send_replace(false);
                    return;
                }
            };
            ready_tx.send_replace(true);
            while !output.is_closed() && input.has_changed().is_ok() {
                if input.has_changed().unwrap_or(false) {
                    if let Some(text) = input.borrow_and_update().clone() {
                        if text.len() <= MAX_TEXT {
                            use x11rb::protocol::xproto;
                            let bytes = text.into_bytes();
                            let _ = xproto::set_selection_owner(
                                &state.connection,
                                state.window,
                                state.selection,
                                x11rb::CURRENT_TIME,
                            );
                            let owner =
                                xproto::get_selection_owner(&state.connection, state.selection)
                                    .ok()
                                    .and_then(|cookie| cookie.reply().ok());
                            if owner.is_some_and(|reply| reply.owner == state.window) {
                                state.owned = Some(bytes);
                                state.owns = true;
                                let _ = state.connection.flush();
                            } else {
                                tracing::warn!("native X11 clipboard ownership failed");
                            }
                        }
                    }
                }
                loop {
                    let event = match state.connection.poll_for_event() {
                        Ok(Some(event)) => event,
                        Ok(None) => break,
                        Err(error) => {
                            tracing::debug!("X11 clipboard event read failed: {error}");
                            return;
                        }
                    };
                    match event {
                        Event::SelectionRequest(request) => answer(&request, &state),
                        Event::SelectionClear(event) => {
                            if event.selection == state.selection {
                                state.owns = false;
                                state.owned = None;
                            }
                        }
                        Event::XfixesSelectionNotify(event) => {
                            if event.selection != state.selection
                                || event.owner == state.window
                                || event.owner == x11rb::NONE
                            {
                                continue;
                            }
                            let _ = x11rb::protocol::xproto::delete_property(
                                &state.connection,
                                state.window,
                                state.property_targets,
                            );
                            let _ = x11rb::protocol::xproto::convert_selection(
                                &state.connection,
                                state.window,
                                state.selection,
                                state.targets,
                                state.property_targets,
                                event.selection_timestamp,
                            );
                            let _ = state.connection.flush();
                            state.pending = Some(Pending::Targets {
                                time: event.selection_timestamp,
                            });
                        }
                        Event::SelectionNotify(event) => {
                            handle_selection_notify(&mut state, &event, &output)
                        }
                        _ => {}
                    }
                }
                let _ = state.connection.flush();
                std::thread::sleep(Duration::from_millis(10));
            }
            use x11rb::protocol::xproto;
            let _ = xproto::destroy_window(&state.connection, state.window);
            let _ = state.connection.flush();
        })
        .ok();
    ClipboardBridge {
        incoming,
        outgoing,
        ready,
    }
}

#[cfg(all(test, target_os = "linux", feature = "desktop"))]
mod x11_tests {
    use super::*;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{self, AtomEnum, EventMask, PropMode, WindowClass};

    struct Xvfb(Child);
    impl Drop for Xvfb {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn start_xvfb() -> (Xvfb, String) {
        use std::io::BufRead;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::os::unix::process::CommandExt;
        let (reader, writer) = UnixStream::pair().unwrap();
        let fd = writer.as_raw_fd();
        let mut command = Command::new("Xvfb");
        command
            .args([
                "-displayfd",
                "3",
                "-screen",
                "0",
                "1280x720x24",
                "-nolisten",
                "tcp",
            ])
            .args(["+extension", "RANDR", "+extension", "XFIXES"]);
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("Xvfb");
        drop(writer);
        let mut reader = std::io::BufReader::new(reader);
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        (Xvfb(child), format!(":{}", line.trim()))
    }

    fn atom(connection: &impl Connection, name: &[u8]) -> u32 {
        xproto::intern_atom(connection, false, name)
            .unwrap()
            .reply()
            .unwrap()
            .atom
    }

    fn reply(connection: &impl Connection, requestor: u32, property: u32, target: u32) {
        let event = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: x11rb::CURRENT_TIME,
            requestor,
            selection: atom(connection, b"CLIPBOARD"),
            target,
            property,
        };
        xproto::send_event(connection, false, requestor, EventMask::NO_EVENT, event).unwrap();
        connection.flush().unwrap();
    }

    #[tokio::test]
    #[ignore = "requires Xvfb with XFIXES"]
    async fn xfixes_clipboard_round_trip() {
        let (_server, display) = start_xvfb();
        let mut bridge = start_x11_at(Some(&display));
        let ready = tokio::time::timeout(
            Duration::from_secs(3),
            bridge.ready.wait_for(|value| *value),
        )
        .await
        .unwrap();
        assert!(ready.is_ok());

        let external = "external ✓ text";
        let (connection, screen) = x11rb::connect(Some(&display)).unwrap();
        let root = connection.setup().roots[screen].root;
        let window = connection.generate_id().unwrap();
        xproto::create_window(
            &connection,
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new(),
        )
        .unwrap();
        let clipboard = atom(&connection, b"CLIPBOARD");
        let utf8 = atom(&connection, b"UTF8_STRING");
        let targets = atom(&connection, b"TARGETS");
        xproto::set_selection_owner(&connection, window, clipboard, x11rb::CURRENT_TIME).unwrap();
        connection.flush().unwrap();
        let owner = xproto::get_selection_owner(&connection, clipboard)
            .unwrap()
            .reply()
            .unwrap()
            .owner;
        assert_eq!(owner, window);

        // SelectionRequest events are delivered to the connection owning the
        // selection window, so this connection stays in the serving thread.
        let serving = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                while let Ok(Some(event)) = connection.poll_for_event() {
                    let x11rb::protocol::Event::SelectionRequest(request) = event else {
                        continue;
                    };
                    if request.target == targets {
                        let atoms = [targets, utf8];
                        let bytes: Vec<u8> =
                            atoms.iter().flat_map(|atom| atom.to_le_bytes()).collect();
                        xproto::change_property(
                            &connection,
                            PropMode::REPLACE,
                            request.requestor,
                            request.property,
                            AtomEnum::ATOM,
                            32,
                            atoms.len() as u32,
                            &bytes,
                        )
                        .unwrap();
                        reply(
                            &connection,
                            request.requestor,
                            request.property,
                            request.target,
                        );
                    } else if request.target == utf8 {
                        xproto::change_property(
                            &connection,
                            PropMode::REPLACE,
                            request.requestor,
                            request.property,
                            utf8,
                            8,
                            external.len() as u32,
                            external.as_bytes(),
                        )
                        .unwrap();
                        reply(
                            &connection,
                            request.requestor,
                            request.property,
                            request.target,
                        );
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let received = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if bridge.outgoing.changed().await.is_err() {
                    return None;
                }
                if let Some(text) = bridge.outgoing.borrow_and_update().clone() {
                    return Some(text);
                }
            }
        })
        .await
        .expect("clipboard owner reply timed out")
        .expect("clipboard owner reply missing");
        assert_eq!(received, external);
        serving.join().unwrap();

        // Viewer -> host: the bridge must become the CLIPBOARD owner and serve
        // the text to an ordinary X11 client.
        bridge
            .incoming
            .send_replace(Some("host → viewer".to_owned()));
        let (reader_connection, reader_screen) = x11rb::connect(Some(&display)).unwrap();
        let reader_root = reader_connection.setup().roots[reader_screen].root;
        let reader = reader_connection.generate_id().unwrap();
        xproto::create_window(
            &reader_connection,
            x11rb::COPY_DEPTH_FROM_PARENT,
            reader,
            reader_root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new(),
        )
        .unwrap();
        let reader_property = atom(&reader_connection, b"READER_CLIPBOARD");
        let mut text = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            while let Ok(Some(event)) = reader_connection.poll_for_event() {
                if let x11rb::protocol::Event::SelectionNotify(event) = event {
                    if event.property == x11rb::NONE {
                        continue;
                    }
                    let reply = xproto::get_property(
                        &reader_connection,
                        true,
                        reader,
                        reader_property,
                        AtomEnum::ANY,
                        0,
                        1024,
                    )
                    .unwrap()
                    .reply()
                    .unwrap();
                    text = Some(String::from_utf8(reply.value).unwrap());
                }
            }
            if text.is_some() {
                break;
            }
            // Wait until the bridge has claimed the selection, then ask
            // again if a request raced its claim.
            let owner = xproto::get_selection_owner(&reader_connection, clipboard)
                .unwrap()
                .reply()
                .unwrap()
                .owner;
            if owner != x11rb::NONE {
                xproto::convert_selection(
                    &reader_connection,
                    reader,
                    clipboard,
                    utf8,
                    reader_property,
                    x11rb::CURRENT_TIME,
                )
                .unwrap();
                reader_connection.flush().unwrap();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(text.as_deref(), Some("host → viewer"));
    }
}
