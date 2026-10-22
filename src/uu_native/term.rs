//! UU terminal frames (`TERM`) and PTY session adapter.
//!
//! Layout: `"TERM"`, version 1, type, two flag bytes, session u32 LE,
//! payload length u32 LE, payload. Control payloads are JSON.

use crate::terminal::session::SessionEvent;
use crate::terminal::session::SessionManager;
use crate::terminal::session::{OpenRequest, SessionList, OUTPUT_CHUNK};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

pub const OPEN: u8 = 1;
pub const STDIN: u8 = 2;
pub const RESIZE: u8 = 3;
pub const CLOSE: u8 = 4;
pub const STDOUT: u8 = 5;
pub const EXIT: u8 = 6;
pub const SIGNAL: u8 = 7;
pub const ERROR: u8 = 8;
pub const LIST_REQ: u8 = 9;
pub const LIST_RESP: u8 = 10;
pub const KILL_REQ: u8 = 11;
pub const KILL_RESP: u8 = 12;
pub const OPEN_RESP: u8 = 13;
pub const UNLOCK_REQ: u8 = 14;
pub const UNLOCK_RESP: u8 = 15;
pub const DETACH: u8 = 16;

pub const HEADER: usize = 16;
pub const MAX_PAYLOAD: usize = 0x10_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub session: u32,
    pub payload: Vec<u8>,
}

pub fn encode(kind: u8, session: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(b"TERM\x01");
    out.push(kind);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&session.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn decode(data: &[u8]) -> Option<Frame> {
    if data.len() < HEADER || &data[..4] != b"TERM" || data[4] != 1 {
        return None;
    }
    let kind = data[5];
    if !(OPEN..=DETACH).contains(&kind) {
        return None;
    }
    let session = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let len = u32::from_le_bytes(data[12..16].try_into().ok()?) as usize;
    if len > MAX_PAYLOAD || data.len() != HEADER + len {
        return None;
    }
    Some(Frame {
        kind,
        session,
        payload: data[HEADER..].to_vec(),
    })
}

pub fn is_frame(data: &[u8]) -> bool {
    data.len() >= 4 && &data[..4] == b"TERM"
}

fn json(payload: &[u8]) -> Value {
    serde_json::from_slice(payload).unwrap_or(Value::Null)
}

fn dims(value: &Value) -> (u16, u16) {
    let get = |key: &str, default: u64| {
        value
            .get(key)
            .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
            .unwrap_or(default)
            .clamp(1, 1000) as u16
    };
    (get("cols", 80), get("rows", 24))
}

fn command_in_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
    })
}

fn resolve_shell(name: &str) -> Option<String> {
    if std::path::Path::new(name).is_absolute() {
        return std::path::Path::new(name)
            .is_file()
            .then(|| name.to_owned());
    }
    if command_in_path(name) {
        return Some(name.to_owned());
    }
    let candidate = match name.to_ascii_lowercase().as_str() {
        "bash" => "bash",
        "sh" | "cmd" | "powershell" | "pwsh" => "sh",
        "zsh" => "zsh",
        _ => return None,
    };
    if command_in_path(candidate) {
        return Some(candidate.to_owned());
    }
    let absolute = format!("/bin/{candidate}");
    std::path::Path::new(&absolute)
        .is_file()
        .then_some(absolute)
}

/// The official client hardcodes `/bin/bash` when it opens a Linux terminal.
/// SSH ignores client shell choices entirely, so that placeholder means
/// "use the account's login shell".
fn is_client_default_shell(shell: &str) -> bool {
    matches!(
        shell.trim(),
        "sh" | "bash"
            | "dash"
            | "/bin/sh"
            | "/bin/bash"
            | "/bin/dash"
            | "/usr/bin/sh"
            | "/usr/bin/bash"
            | "/usr/bin/dash"
    )
}

fn reply(code: i64, message: &str) -> Vec<u8> {
    json!({"code": code, "message": message})
        .to_string()
        .into_bytes()
}

/// One official-client terminal session. Output is mirrored to the peer that
/// currently owns the BINARY channel; sessions outlive a peer connection.
///
/// A viewer is identified by a token so a stale attachment can never steal or
/// release output that belongs to a newer attach on the same session.
pub struct HostTerminal {
    manager: SessionManager,
    peer: mpsc::UnboundedSender<Vec<u8>>,
    attached: Mutex<HashMap<u16, u64>>,
    next_token: AtomicU64,
    /// Bounded queue of this peer's terminal events. A viewer that cannot keep
    /// up is detached by the session instead of stalling the PTY.
    output: mpsc::Sender<SessionEvent>,
    events: Mutex<Option<mpsc::Receiver<SessionEvent>>>,
    closed: AtomicBool,
    unlocked: AtomicBool,
}

impl HostTerminal {
    pub fn new(manager: SessionManager, peer: mpsc::UnboundedSender<Vec<u8>>) -> Arc<Self> {
        let unlocked = !manager.has_password();
        let (output, events) = mpsc::channel(256);
        Arc::new(Self {
            manager,
            peer,
            attached: Mutex::new(HashMap::new()),
            next_token: AtomicU64::new(1),
            output,
            events: Mutex::new(Some(events)),
            closed: AtomicBool::new(false),
            unlocked: AtomicBool::new(unlocked),
        })
    }

    fn next_token(&self) -> u64 {
        loop {
            let token = self.next_token.fetch_add(1, Ordering::Relaxed);
            if token != 0 {
                return token;
            }
        }
    }

    fn owns(&self, session_id: u16, token: u64) -> bool {
        self.attached
            .lock()
            .map(|attached| attached.get(&session_id) == Some(&token))
            .unwrap_or(false)
    }

    /// Release only this peer's attachment. The session keeps recording output
    /// for the next viewer.
    fn detach(&self, session_id: u16) {
        let token = self
            .attached
            .lock()
            .ok()
            .and_then(|mut attached| attached.remove(&session_id));
        if let Some(token) = token {
            self.manager.detach(session_id, token);
        }
    }

    fn detach_all(&self) {
        let sessions: Vec<u16> = self
            .attached
            .lock()
            .map(|attached| attached.keys().copied().collect())
            .unwrap_or_default();
        for session_id in sessions {
            self.detach(session_id);
        }
    }

    fn attach(self: &Arc<Self>, session_id: u16) -> crate::error::Result<u64> {
        let token = self.next_token();
        // Claim the session before attach queues the replay, so the event loop
        // never mistakes a stale event for the new viewer's output.
        self.attached
            .lock()
            .map_err(|_| crate::error::Error::Session("terminal state poisoned".to_owned()))?
            .insert(session_id, token);
        if let Err(error) = self.manager.attach(session_id, token, self.output.clone()) {
            if let Ok(mut attached) = self.attached.lock() {
                attached.remove(&session_id);
            }
            return Err(error);
        }
        Ok(token)
    }

    pub fn handle(self: &Arc<Self>, data: &[u8]) {
        let Some(frame) = decode(data) else {
            tracing::warn!("UU terminal frame rejected: {} bytes", data.len());
            return;
        };
        let session = frame.session;
        if frame.kind != STDIN {
            tracing::debug!(
                "UU terminal frame: kind={} session={} payload={}",
                frame.kind,
                frame.session,
                frame.payload.len()
            );
        }
        match frame.kind {
            OPEN => {
                if !self.unlocked.load(std::sync::atomic::Ordering::Acquire) {
                    tracing::info!("UU terminal open: password required");
                    self.send(
                        OPEN_RESP,
                        session,
                        json!({
                            "code": 401,
                            "message": "device_locked",
                            "action": "device_locked_prompt"
                        })
                        .to_string()
                        .as_bytes(),
                    );
                    return;
                }
                let value = json(&frame.payload);
                let (cols, rows) = dims(&value);
                let request = OpenRequest {
                    session_id: if session != 0 && session <= u16::MAX as u32 {
                        Some(session as u16)
                    } else {
                        None
                    },
                    name: None,
                    shell: value
                        .get("shell")
                        .and_then(Value::as_str)
                        .and_then(resolve_shell)
                        .filter(|shell| !is_client_default_shell(shell)),
                    cols: Some(cols),
                    rows: Some(rows),
                };
                match self.manager.open(&request) {
                    Ok(response) => {
                        let id = u32::from(response.session_id);
                        tracing::info!(
                            "UU terminal open: session={} created={} shell={} cols={} rows={}",
                            response.session_id,
                            response.created,
                            response.shell.as_deref().unwrap_or("-"),
                            cols,
                            rows
                        );
                        let payload =
                            serde_json::to_vec(&response).unwrap_or_else(|_| reply(0, "ok"));
                        // The response is queued before attach, so the replay
                        // (queued by attach) can never precede it.
                        self.send(OPEN_RESP, id, &payload);
                        if let Err(error) = self.attach(response.session_id) {
                            tracing::warn!("UU terminal attach failed: {error}");
                            self.send(ERROR, id, &reply(1, &error.to_string()));
                        }
                    }
                    Err(error) => {
                        tracing::warn!("UU terminal open failed: {error}");
                        self.send(OPEN_RESP, session, &reply(1, &error.to_string()));
                    }
                }
            }
            STDIN => {
                if !self.unlocked.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                if let Ok(id) = u16::try_from(session) {
                    let _ = self.manager.write(id, &frame.payload);
                }
            }
            RESIZE => {
                if let Ok(id) = u16::try_from(session) {
                    let value = json(&frame.payload);
                    let (cols, rows) = dims(&value);
                    let _ = self.manager.resize(id, cols, rows);
                }
            }
            SIGNAL => {
                if let Ok(id) = u16::try_from(session) {
                    let value = json(&frame.payload);
                    let name = value
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| {
                            value.as_object().and_then(|o| {
                                o.values()
                                    .find_map(|v| v.as_str().map(str::to_owned))
                                    .or_else(|| {
                                        o.values().find_map(|v| v.as_i64().map(|n| n.to_string()))
                                    })
                            })
                        })
                        .unwrap_or_else(|| "SIGINT".to_owned());
                    let _ = self.manager.signal(id, &name);
                }
            }
            CLOSE => {
                if let Ok(id) = u16::try_from(session) {
                    self.detach(id);
                    // A closed terminal hangs the shell up, like closing an
                    // SSH session; the session stays listable until it exits.
                    if let Err(error) = self.manager.close(id) {
                        tracing::debug!("UU terminal close failed: {error}");
                    }
                }
            }
            DETACH => {
                if let Ok(id) = u16::try_from(session) {
                    self.detach(id);
                }
            }
            KILL_REQ => {
                let id = if session != 0 {
                    u16::try_from(session).ok()
                } else {
                    let value = json(&frame.payload);
                    value
                        .get("session_id")
                        .or_else(|| value.get("sessionId"))
                        .or_else(|| value.get("id"))
                        .and_then(Value::as_u64)
                        .or_else(|| {
                            value
                                .as_object()
                                .and_then(|o| o.values().find_map(Value::as_u64))
                        })
                        .and_then(|v| u16::try_from(v).ok())
                };
                match id {
                    Some(id) => {
                        let result = self.manager.kill(id);
                        match &result {
                            Ok(()) => tracing::info!("UU terminal session {id} killed"),
                            Err(error) => tracing::warn!("UU terminal kill failed: {error}"),
                        }
                        self.detach(id);
                        let payload = match result {
                            Ok(()) => reply(0, "ok"),
                            Err(error) => reply(1, &error.to_string()),
                        };
                        self.send(KILL_RESP, u32::from(id), &payload);
                    }
                    None => self.send(KILL_RESP, session, &reply(1, "session not found")),
                }
            }
            LIST_REQ => {
                let sessions: Vec<Value> = self
                    .manager
                    .list()
                    .sessions
                    .into_iter()
                    .map(|item| {
                        json!({
                            "session_id": item.session_id,
                            "name": item.session_name,
                            "shell": item.shell,
                            "state": item.state,
                            "created_at_ms": item.created_at_ms,
                            "attachable": item.attachable,
                        })
                    })
                    .collect();
                let payload = serde_json::to_vec(&sessions).unwrap_or_else(|_| b"[]".to_vec());
                self.send(LIST_RESP, session, &payload);
            }
            UNLOCK_REQ => {
                let value = json(&frame.payload);
                let password = value
                    .get("password")
                    .and_then(Value::as_str)
                    .or_else(|| value.as_str())
                    .unwrap_or("");
                if self.manager.verify_password(password) {
                    self.unlocked
                        .store(true, std::sync::atomic::Ordering::Release);
                    self.send(UNLOCK_RESP, session, &reply(0, "ok"));
                } else {
                    self.send(UNLOCK_RESP, session, &reply(1, "invalid password"));
                }
            }
            _ => self.send(ERROR, session, &reply(1, "unsupported frame")),
        }
    }

    pub fn event_loop(self: &Arc<Self>) {
        let Some(mut events) = self.events.lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        let terminal = self.clone();
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    event = events.recv() => event,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        if terminal.closed.load(Ordering::Acquire) {
                            break;
                        }
                        continue;
                    }
                };
                let Some(event) = event else {
                    break;
                };
                match event {
                    SessionEvent::Attach {
                        session_id,
                        token,
                        snapshot,
                    } => {
                        if !terminal.owns(session_id, token) {
                            continue;
                        }
                        for chunk in snapshot.chunks(OUTPUT_CHUNK) {
                            terminal.send(STDOUT, u32::from(session_id), chunk);
                        }
                    }
                    SessionEvent::Output {
                        session_id,
                        token,
                        data,
                    } => {
                        if !terminal.owns(session_id, token) {
                            continue;
                        }
                        for chunk in data.chunks(OUTPUT_CHUNK) {
                            terminal.send(STDOUT, u32::from(session_id), chunk);
                        }
                    }
                    SessionEvent::Exited {
                        session_id,
                        token,
                        exit_code,
                    } => {
                        if !terminal.owns(session_id, token) {
                            continue;
                        }
                        if let Ok(mut attached) = terminal.attached.lock() {
                            attached.remove(&session_id);
                        }
                        let payload = json!({"exit_code": exit_code}).to_string();
                        terminal.send(EXIT, u32::from(session_id), payload.as_bytes());
                    }
                }
            }
        });
    }

    pub fn close(&self) {
        self.detach_all();
        self.closed.store(true, Ordering::Release);
    }

    fn send(&self, kind: u8, session: u32, payload: &[u8]) {
        let _ = self.peer.send(encode(kind, session, payload));
    }

    pub fn sessions(&self) -> SessionList {
        self.manager.list()
    }
}

/// Minimal terminal client used by `openuur term`.
pub struct ViewerTerminal {
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl ViewerTerminal {
    pub fn new(tx: mpsc::UnboundedSender<Vec<u8>>) -> Self {
        Self { tx }
    }

    pub fn send(&self, kind: u8, session: u32, payload: &[u8]) -> bool {
        self.tx.send(encode(kind, session, payload)).is_ok()
    }
}

/// Reads from a local terminal and writes to a remote PTY. The caller supplies
/// a blocking raw-mode guard on Unix.
pub fn spawn_stdin(tx: mpsc::UnboundedSender<Vec<u8>>, session: u32) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(encode(STDIN, session, &buffer[..n])).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(unix)]
pub struct RawMode {
    original: libc::termios,
}

#[cfg(unix)]
impl RawMode {
    pub fn enable() -> Option<Self> {
        use std::os::fd::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
            return None;
        }
        let original = term;
        unsafe { libc::cfmakeraw(&mut term) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) } != 0 {
            return None;
        }
        Some(Self { original })
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        unsafe {
            libc::tcsetattr(fd, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(not(unix))]
pub struct RawMode;

#[cfg(not(unix))]
impl RawMode {
    pub fn enable() -> Option<Self> {
        None
    }
}

impl ViewerTerminal {
    pub fn write_stdout(data: &[u8]) {
        let mut out = std::io::stdout();
        let _ = out.write_all(data);
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_default_shell_placeholder_is_ignored() {
        assert!(is_client_default_shell("/bin/bash"));
        assert!(is_client_default_shell("bash"));
        assert!(is_client_default_shell("/usr/bin/sh"));
        assert!(!is_client_default_shell("/bin/zsh"));
        assert!(!is_client_default_shell("/run/current-system/sw/bin/nu"));
    }

    #[test]
    fn frame_layout_matches_uu() {
        let frame = encode(OPEN_RESP, 7, b"{}");
        assert_eq!(&frame[..8], b"TERM\x01\r\0\0");
        assert_eq!(&frame[8..16], &[7, 0, 0, 0, 2, 0, 0, 0]);
        assert_eq!(decode(&frame).unwrap().session, 7);
        assert!(decode(&frame[..17]).is_none());
    }
}

#[cfg(test)]
mod host_shell_tests {
    #[test]
    fn bash_session_stays_alive_and_echoes() {
        let manager = crate::terminal::session::SessionManager::new(None);
        let mut events = manager.subscribe();
        let open = crate::terminal::session::OpenRequest {
            session_id: None,
            name: None,
            shell: Some("bash".into()),
            cols: Some(80),
            rows: Some(24),
        };
        let response = manager.open(&open).unwrap();
        manager
            .write(response.session_id, b"echo DIRECT_OK; exit 3\n")
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut output = Vec::new();
        while std::time::Instant::now() < deadline {
            if let Ok(event) = events.try_recv() {
                match event {
                    crate::terminal::session::SessionEvent::Output {
                        session_id, data, ..
                    } if session_id == response.session_id => output.extend(data),
                    crate::terminal::session::SessionEvent::Exited { .. } => {
                        break;
                    }
                    _ => {}
                }
            } else {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("DIRECT_OK"));
    }
}

#[cfg(test)]
mod reattach_tests {
    use super::*;

    fn occurrences(haystack: &[u8], needle: &str) -> usize {
        String::from_utf8_lossy(haystack).matches(needle).count()
    }

    async fn stdout_until(
        rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
        marker: &str,
        timeout: Duration,
    ) -> Vec<u8> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut output = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return output;
            }
            let Ok(Some(data)) = tokio::time::timeout(remaining, rx.recv()).await else {
                return output;
            };
            if let Some(frame) = decode(&data) {
                if frame.kind == STDOUT {
                    output.extend(frame.payload);
                    if String::from_utf8_lossy(&output).contains(marker) {
                        return output;
                    }
                }
            }
        }
    }

    /// The official client's `/bin/bash` placeholder is replaced with the
    /// account shell. Tests need a known shell, so pass an absolute path that
    /// is not one of the placeholder spellings.
    fn absolute_bash() -> String {
        let mut placeholders = Vec::new();
        if let Some(path) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&path) {
                let candidate = directory.join("bash");
                if !candidate.is_file() {
                    continue;
                }
                let candidate = candidate.to_string_lossy().into_owned();
                if !is_client_default_shell(&candidate) {
                    return candidate;
                }
                placeholders.push(candidate);
            }
        }
        placeholders
            .into_iter()
            .next()
            .unwrap_or_else(|| "/bin/sh".to_owned())
    }

    #[tokio::test]
    async fn reopening_replays_output_exactly_once() {
        let manager = SessionManager::new(None);
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let terminal = HostTerminal::new(manager.clone(), tx);
        terminal.event_loop();
        let payload = format!(r#"{{"cols":80,"rows":24,"shell":"{}"}}"#, absolute_bash());
        terminal.handle(&encode(OPEN, 0, payload.as_bytes()));
        let open = decode(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(open.kind, OPEN_RESP);
        assert_eq!(json(&open.payload)["code"], 0);
        let id = open.session;

        // A just-spawned login shell can miss input written before it reads.
        let mut ready = false;
        let mut probed = Vec::new();
        for _ in 0..100 {
            terminal.handle(&encode(STDIN, id, b"printf 'READY_%s\\n' MARK\r\n"));
            let probe = stdout_until(&mut rx, "READY_MARK", Duration::from_millis(200)).await;
            if String::from_utf8_lossy(&probe).contains("READY_MARK") {
                ready = true;
                break;
            }
            if probe.len() > 200 {
                probed = probe;
            }
        }
        assert!(
            ready,
            "the shell never executed a probe: {:?}",
            String::from_utf8_lossy(&probed)
        );

        terminal.handle(&encode(STDIN, id, b"printf 'FIRST_%s\\n' MARK\r\n"));
        let first = stdout_until(&mut rx, "FIRST_MARK", Duration::from_secs(5)).await;
        assert!(
            String::from_utf8_lossy(&first).contains("FIRST_MARK"),
            "no first mark in {:?}",
            String::from_utf8_lossy(&first)
        );

        terminal.handle(&encode(DETACH, id, b""));
        terminal.handle(&encode(STDIN, id, b"printf 'SECOND_%s\\n' MARK\r\n"));
        let mut leaked = Vec::new();
        while let Ok(Some(data)) = tokio::time::timeout(Duration::from_millis(400), rx.recv()).await
        {
            if let Some(frame) = decode(&data) {
                if frame.kind == STDOUT {
                    leaked.extend(frame.payload);
                }
            }
        }
        assert!(
            !String::from_utf8_lossy(&leaked).contains("SECOND_MARK"),
            "a detached viewer received output"
        );

        terminal.handle(&encode(OPEN, id, br#"{"cols":80,"rows":24}"#));
        let reopened = decode(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(reopened.kind, OPEN_RESP);
        let replay = stdout_until(&mut rx, "SECOND_MARK", Duration::from_secs(5)).await;
        assert_eq!(occurrences(&replay, "FIRST_MARK"), 1);
        assert_eq!(occurrences(&replay, "SECOND_MARK"), 1);
        terminal.handle(&encode(KILL_REQ, id, b""));
        terminal.close();
    }
}

#[cfg(test)]
mod unlock_tests {
    use super::*;
    #[test]
    fn password_gates_terminal_open() {
        let manager = crate::terminal::session::SessionManager::new(Some("secret".into()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let terminal = HostTerminal::new(manager, tx);
        terminal.handle(&encode(
            OPEN,
            0,
            br#"{"cols":80,"rows":24,"option":"none"}"#,
        ));
        let denied = decode(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(denied.kind, OPEN_RESP);
        assert_eq!(json(&denied.payload)["code"], 401);
        terminal.handle(&encode(UNLOCK_REQ, 0, br#"{"password":"wrong"}"#));
        assert_eq!(
            json(&decode(&rx.try_recv().unwrap()).unwrap().payload)["code"],
            1
        );
        terminal.handle(&encode(UNLOCK_REQ, 0, br#"{"password":"secret"}"#));
        assert_eq!(
            json(&decode(&rx.try_recv().unwrap()).unwrap().payload)["code"],
            0
        );
        terminal.handle(&encode(
            OPEN,
            0,
            br#"{"cols":80,"rows":24,"option":"none","shell":"bash"}"#,
        ));
        let opened = decode(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(json(&opened.payload)["code"], 0);
    }
}
