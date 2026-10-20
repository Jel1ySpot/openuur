//! UU terminal frames (`TERM`) and PTY session adapter.
//!
//! Layout: `"TERM"`, version 1, type, two flag bytes, session u32 LE,
//! payload length u32 LE, payload. Control payloads are JSON.

use crate::terminal::session::SessionManager;
use crate::terminal::session::{OpenRequest, SessionList};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::Arc;
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
pub struct HostTerminal {
    manager: SessionManager,
    peer: mpsc::UnboundedSender<Vec<u8>>,
    attached: std::sync::Mutex<HashSet<u16>>,
    closed: std::sync::atomic::AtomicBool,
    unlocked: std::sync::atomic::AtomicBool,
}

impl HostTerminal {
    pub fn new(manager: SessionManager, peer: mpsc::UnboundedSender<Vec<u8>>) -> Arc<Self> {
        let unlocked = !manager.has_password();
        Arc::new(Self {
            manager,
            peer,
            attached: std::sync::Mutex::new(HashSet::new()),
            closed: std::sync::atomic::AtomicBool::new(false),
            unlocked: std::sync::atomic::AtomicBool::new(unlocked),
        })
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
                        self.attached.lock().unwrap().insert(response.session_id);
                        let payload =
                            serde_json::to_vec(&response).unwrap_or_else(|_| reply(0, "ok"));
                        self.send(OPEN_RESP, id, &payload);
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
                    self.attached.lock().unwrap().remove(&id);
                    // A closed terminal hangs the shell up, like closing an
                    // SSH session; the session stays listable until it exits.
                    if let Err(error) = self.manager.close(id) {
                        tracing::debug!("UU terminal close failed: {error}");
                    }
                }
            }
            DETACH => {
                if let Ok(id) = u16::try_from(session) {
                    self.attached.lock().unwrap().remove(&id);
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
                        self.attached.lock().unwrap().remove(&id);
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

    pub fn event_loop(
        self: Arc<Self>,
        mut events: tokio::sync::broadcast::Receiver<crate::terminal::session::SessionEvent>,
    ) {
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    event = events.recv() => event,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if self.closed.load(std::sync::atomic::Ordering::Acquire) { break; }
                        continue;
                    }
                };
                match event {
                    Ok(crate::terminal::session::SessionEvent::Output { session_id, data }) => {
                        if !self.attached.lock().unwrap().contains(&session_id) {
                            continue;
                        }
                        for chunk in data.chunks(16 * 1024) {
                            self.send(STDOUT, u32::from(session_id), chunk);
                        }
                    }
                    Ok(crate::terminal::session::SessionEvent::Exited {
                        session_id,
                        exit_code,
                    }) => {
                        if self.attached.lock().unwrap().contains(&session_id) {
                            let payload = json!({"exit_code": exit_code}).to_string();
                            self.send(EXIT, u32::from(session_id), payload.as_bytes());
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        });
    }

    pub fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
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
                    crate::terminal::session::SessionEvent::Output { session_id, data }
                        if session_id == response.session_id =>
                    {
                        output.extend(data)
                    }
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
