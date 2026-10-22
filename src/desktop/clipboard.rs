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
use tokio::sync::{oneshot, watch};

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

pub struct ClipboardBridge {
    pub incoming: watch::Sender<Option<String>>,
    pub outgoing: watch::Receiver<Option<String>>,
    pub ready: oneshot::Receiver<bool>,
}

pub fn start() -> ClipboardBridge {
    if command_exists("qdbus") && qdbus_clipboard_works() {
        return start_qdbus();
    }
    // wl-copy/wl-paste are useless without a Wayland compositor, and claiming
    // the capability on X11 would make the host advertise a clipboard it
    // cannot serve.
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
    if wayland && command_exists("wl-copy") && command_exists("wl-paste") {
        return start_wl();
    }
    unavailable()
}

fn unavailable() -> ClipboardBridge {
    let (incoming, _) = watch::channel(None);
    let (_, outgoing) = watch::channel(None);
    let (ready_tx, ready) = oneshot::channel();
    let _ = ready_tx.send(false);
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
    let (ready_tx, ready) = oneshot::channel();
    tokio::spawn(async move {
        let _ = ready_tx.send(true);
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
    let (ready_tx, ready) = oneshot::channel();
    tokio::spawn(async move {
        let _ = ready_tx.send(true);
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
    let (ready_tx, ready) = oneshot::channel();
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
            let _ = ready_tx.send(true);
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
