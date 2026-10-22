use crate::error::{Error, Result};
use crate::util::now_ms;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenRequest {
    #[serde(default)]
    pub session_id: Option<u16>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenResponse {
    #[serde(default)]
    pub session_id: u16,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub created: bool,
    #[serde(default)]
    pub code: i32,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    #[serde(alias = "sessionId", alias = "id")]
    pub session_id: u16,
    #[serde(default, alias = "sessionName")]
    pub session_name: String,
    #[serde(default)]
    pub shell: String,
    #[serde(default)]
    pub state: String,
    #[serde(default, alias = "createdAt")]
    pub created_at_ms: i64,
    #[serde(default)]
    pub last_active_ms: i64,
    #[serde(default)]
    pub attachable: bool,
    #[serde(default)]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionList {
    #[serde(default)]
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Output {
        session_id: u16,
        token: u64,
        data: Vec<u8>,
    },
    /// Emitted while the sink lock is held, so the replayed snapshot is queued
    /// ahead of any subsequent live output for the same token.
    Attach {
        session_id: u16,
        token: u64,
        snapshot: Vec<u8>,
    },
    Exited {
        session_id: u16,
        token: u64,
        exit_code: i32,
    },
}

/// Bounded per-session output history replayed on reattach.
const SCROLLBACK_LIMIT: usize = 256 * 1024;
/// Replay and live output are chunked well below the SCTP message limit.
pub const OUTPUT_CHUNK: usize = 16 * 1024;

struct Scrollback {
    data: VecDeque<u8>,
}

impl Scrollback {
    fn new() -> Self {
        Self {
            data: VecDeque::new(),
        }
    }

    fn record(&mut self, bytes: &[u8]) {
        self.data.extend(bytes.iter().copied());
        let excess = self.data.len().saturating_sub(SCROLLBACK_LIMIT);
        for _ in 0..excess {
            self.data.pop_front();
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.data.iter().copied().collect()
    }
}

struct Sink {
    tx: mpsc::Sender<SessionEvent>,
    token: u64,
}

struct InnerSession {
    info: Mutex<SessionInfo>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    last_active: Arc<Mutex<Instant>>,
    /// Current viewer. Output ordering is guaranteed by holding this lock
    /// while appending history and sending to the sink.
    sink: Mutex<Option<Sink>>,
    scrollback: Mutex<Scrollback>,
    reader_done: AtomicBool,
}

impl InnerSession {
    fn info(&self) -> SessionInfo {
        let mut info = match self.info.lock() {
            Ok(info) => info.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        info.last_active_ms = self
            .last_active
            .lock()
            .map(|t| t.elapsed().as_millis() as i64)
            .unwrap_or(0);
        info
    }

    /// Signal the shell's process group. `portable-pty` makes the child a
    /// session leader, so a negative pid reaches a foreground job such as an
    /// editor as well as the shell itself.
    #[cfg(unix)]
    fn signal(&self, signal: i32) {
        let Ok(child) = self.child.lock() else {
            return;
        };
        let Some(pid) = child.process_id().and_then(|pid| i32::try_from(pid).ok()) else {
            return;
        };
        unsafe {
            if libc::kill(-pid, signal) != 0 {
                libc::kill(pid, signal);
            }
        }
    }

    #[cfg(not(unix))]
    fn signal(&self, _signal: i32) {}
}

struct Inner {
    sessions: BTreeMap<u16, Arc<InnerSession>>,
    next_id: u16,
}

impl Inner {
    fn allocate_id(&mut self) -> Result<u16> {
        for _ in 0..u16::MAX {
            let id = self.next_id.max(1);
            self.next_id = id.wrapping_add(1).max(1);
            if !self.sessions.contains_key(&id) {
                return Ok(id);
            }
        }
        Err(Error::Session("no free terminal session id".to_owned()))
    }
}

#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Mutex<Inner>>,
    events: broadcast::Sender<SessionEvent>,
    password: Option<String>,
}

impl Drop for SessionManager {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) != 1 {
            return;
        }
        if let Ok(inner) = self.inner.lock() {
            for session in inner.sessions.values() {
                if let Ok(mut child) = session.child.lock() {
                    let _ = child.kill();
                }
            }
        }
    }
}

impl SessionManager {
    pub fn new(password: Option<String>) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                sessions: BTreeMap::new(),
                next_id: 1,
            })),
            events,
            password,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    pub fn list(&self) -> SessionList {
        let inner = self.inner.lock().expect("session manager poisoned");
        SessionList {
            sessions: inner
                .sessions
                .values()
                .map(|session| session.info())
                .collect(),
        }
    }

    pub fn find_by_name(&self, name: &str) -> Option<SessionInfo> {
        let inner = self.inner.lock().ok()?;
        inner
            .sessions
            .values()
            .find(|s| s.info().session_name == name)
            .map(|s| s.info())
    }

    pub fn get(&self, session_id: u16) -> Option<SessionInfo> {
        let inner = self.inner.lock().ok()?;
        inner.sessions.get(&session_id).map(|s| s.info())
    }

    /// Create or attach a terminal session.
    pub fn open(&self, request: &OpenRequest) -> Result<OpenResponse> {
        let cols = request.cols.unwrap_or(120).clamp(1, 1000);
        let rows = request.rows.unwrap_or(30).clamp(1, 1000);
        let shell = request
            .shell
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(default_shell);

        if let Some(id) = request.session_id {
            if let Some(existing) = self.get(id) {
                if existing.state == "exited" {
                    return Err(Error::Session(format!(
                        "terminal session {id} has exited and is no longer attachable"
                    )));
                }
                return Ok(OpenResponse {
                    session_id: id,
                    name: Some(existing.session_name),
                    shell: Some(existing.shell),
                    created: false,
                    code: 0,
                    message: String::new(),
                });
            }
        }
        if let Some(name) = request.name.as_deref().filter(|s| !s.trim().is_empty()) {
            if let Some(existing) = self.find_by_name(name) {
                if existing.state == "exited" {
                    return Err(Error::Session(format!(
                        "terminal session {name:?} has exited; choose another name"
                    )));
                }
                return Ok(OpenResponse {
                    session_id: existing.session_id,
                    name: Some(existing.session_name),
                    shell: Some(existing.shell),
                    created: false,
                    code: 0,
                    message: String::new(),
                });
            }
        }

        let mut inner = self.inner.lock().expect("session manager poisoned");
        if let Some(name) = request.name.as_deref().filter(|s| !s.trim().is_empty()) {
            if inner
                .sessions
                .values()
                .any(|s| s.info().session_name == name)
            {
                return Err(Error::Session(format!(
                    "terminal session name already exists: {name}"
                )));
            }
        }
        let session_id = inner.allocate_id()?;
        let shell_name = Path::new(&shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&shell)
            .to_owned();
        let name = request
            .name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("{shell_name}-{session_id}"));

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        // Behave like an SSH login: run the account's shell with a login
        // profile, in the account's home, with the environment sshd provides.
        let shell_path = resolve_shell(&shell);
        let mut command = CommandBuilder::new(&shell_path);
        if is_login_shell(&shell_path) {
            command.arg("-l");
        }
        command.env("SHELL", absolute_shell(&shell_path));
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        if std::env::var_os("LANG").is_none() {
            command.env("LANG", "C.UTF-8");
        }
        if let Some(account) = account() {
            if !account.name.is_empty() {
                command.env("USER", &account.name);
                command.env("LOGNAME", &account.name);
            }
            if !account.home.is_empty() {
                command.env("HOME", &account.home);
                command.cwd(&account.home);
            }
        } else if let Some(home) = dirs::home_dir() {
            command.cwd(home);
        }

        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let master = pair.master;
        let child = Arc::new(Mutex::new(child));
        let last_active = Arc::new(Mutex::new(Instant::now()));
        let created_at_ms = now_ms();

        let session = Arc::new(InnerSession {
            info: Mutex::new(SessionInfo {
                session_id,
                session_name: name.clone(),
                shell: shell.clone(),
                state: "running".to_owned(),
                created_at_ms,
                last_active_ms: 0,
                attachable: true,
                exit_code: None,
            }),
            writer: Mutex::new(writer),
            master: Mutex::new(master),
            child: child.clone(),
            last_active: last_active.clone(),
            sink: Mutex::new(None),
            scrollback: Mutex::new(Scrollback::new()),
            reader_done: AtomicBool::new(false),
        });
        inner.sessions.insert(session_id, session.clone());

        spawn_reader(session_id, reader, session.clone(), self.events.clone());
        spawn_waiter(
            session_id,
            child,
            session,
            self.events.clone(),
            self.inner.clone(),
        );

        Ok(OpenResponse {
            session_id,
            name: Some(name),
            shell: Some(shell),
            created: true,
            code: 0,
            message: String::new(),
        })
    }

    pub fn write(&self, session_id: u16, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let inner = self.inner.lock().expect("session manager poisoned");
        let session = inner
            .sessions
            .get(&session_id)
            .ok_or_else(|| Error::Session(format!("terminal session {session_id} not found")))?;
        let mut writer = session
            .writer
            .lock()
            .map_err(|_| Error::Session("terminal writer poisoned".to_owned()))?;
        writer.write_all(data)?;
        writer.flush()?;
        if let Ok(mut last) = session.last_active.lock() {
            *last = Instant::now();
        }
        Ok(())
    }

    pub fn resize(&self, session_id: u16, cols: u16, rows: u16) -> Result<()> {
        let inner = self.inner.lock().expect("session manager poisoned");
        let session = inner
            .sessions
            .get(&session_id)
            .ok_or_else(|| Error::Session(format!("terminal session {session_id} not found")))?;
        let master = session
            .master
            .lock()
            .map_err(|_| Error::Session("terminal master poisoned".to_owned()))?;
        master.resize(PtySize {
            rows: rows.clamp(1, 1000),
            cols: cols.clamp(1, 1000),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    pub fn signal(&self, session_id: u16, signal: &str) -> Result<()> {
        let name = signal.trim().trim_start_matches("SIG").to_ascii_uppercase();
        if let Ok(number) = name.parse::<i32>() {
            return self.signal_group(session_id, number);
        }
        match name.as_str() {
            "INT" | "C" => self.write(session_id, b"\x03"),
            "QUIT" => self.write(session_id, b"\x1c"),
            "KILL" => self.kill(session_id),
            #[cfg(unix)]
            "TERM" => self.signal_group(session_id, libc::SIGTERM),
            #[cfg(unix)]
            "HUP" => self.signal_group(session_id, libc::SIGHUP),
            other => Err(Error::Session(format!("unsupported signal {other}"))),
        }
    }

    /// Deliver `signal` to the shell's process group, the way a hung-up SSH
    /// connection does. The session stays registered so the client can still
    /// list or reattach it.
    #[cfg(unix)]
    pub fn signal_group(&self, session_id: u16, signal: i32) -> Result<()> {
        let inner = self.inner.lock().expect("session manager poisoned");
        let session = inner
            .sessions
            .get(&session_id)
            .ok_or_else(|| Error::Session(format!("terminal session {session_id} not found")))?;
        session.signal(signal);
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn signal_group(&self, _session_id: u16, _signal: i32) -> Result<()> {
        Err(Error::Session(
            "signals are not supported on this platform".to_owned(),
        ))
    }

    /// Hang up the session without removing it, matching the `CLOSE` frame.
    pub fn close(&self, session_id: u16) -> Result<()> {
        #[cfg(unix)]
        {
            self.signal_group(session_id, libc::SIGHUP)
        }
        #[cfg(not(unix))]
        {
            self.kill(session_id)
        }
    }

    /// End a session and drop it from the list, matching the official
    /// client's `SIGHUP` followed by `SIGKILL`.
    pub fn kill(&self, session_id: u16) -> Result<()> {
        let mut inner = self.inner.lock().expect("session manager poisoned");
        let session = inner
            .sessions
            .remove(&session_id)
            .ok_or_else(|| Error::Session(format!("terminal session {session_id} not found")))?;
        if let Ok(mut sink) = session.sink.lock() {
            *sink = None;
        }
        #[cfg(unix)]
        {
            session.signal(libc::SIGHUP);
            session.signal(libc::SIGKILL);
        }
        #[cfg(not(unix))]
        if let Ok(mut child) = session.child.lock() {
            let _ = child.kill();
        }
        Ok(())
    }

    pub fn rename(&self, session_id: u16, new_name: &str) -> Result<()> {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return Err(Error::Session("terminal session name is empty".to_owned()));
        }
        let mut inner = self.inner.lock().expect("session manager poisoned");
        if inner
            .sessions
            .values()
            .any(|s| s.info().session_name == new_name)
        {
            return Err(Error::Session(format!(
                "terminal session name already exists: {new_name}"
            )));
        }
        let session = inner
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| Error::Session(format!("terminal session {session_id} not found")))?;
        session
            .info
            .lock()
            .map_err(|_| Error::Session("terminal info poisoned".to_owned()))?
            .session_name = new_name.to_owned();
        Ok(())
    }

    /// Attach a viewer and queue the buffered history behind the same lock that
    /// live output uses, so every byte is delivered exactly once and in order.
    pub fn attach(
        &self,
        session_id: u16,
        token: u64,
        tx: mpsc::Sender<SessionEvent>,
    ) -> Result<()> {
        let session = {
            let inner = self.inner.lock().expect("session manager poisoned");
            inner.sessions.get(&session_id).cloned()
        }
        .ok_or_else(|| Error::Session(format!("terminal session {session_id} not found")))?;
        let mut sink = session
            .sink
            .lock()
            .map_err(|_| Error::Session("terminal sink poisoned".to_owned()))?;
        let snapshot = session
            .scrollback
            .lock()
            .map_err(|_| Error::Session("terminal scrollback poisoned".to_owned()))?
            .snapshot();
        let permit = tx
            .try_reserve()
            .map_err(|_| Error::Session("terminal viewer is not keeping up".to_owned()))?;
        permit.send(SessionEvent::Attach {
            session_id,
            token,
            snapshot,
        });
        *sink = Some(Sink { tx, token });
        drop(sink);
        // Full-screen programs repaint on SIGWINCH, which gives a reattaching
        // viewer a coherent screen on top of the replayed history.
        #[cfg(unix)]
        session.signal(libc::SIGWINCH);
        Ok(())
    }

    /// Detach only if this viewer still owns the session, so a stale peer can
    /// never remove a newer attachment.
    pub fn detach(&self, session_id: u16, token: u64) {
        let session = self
            .inner
            .lock()
            .ok()
            .and_then(|inner| inner.sessions.get(&session_id).cloned());
        if let Some(session) = session {
            if let Ok(mut sink) = session.sink.lock() {
                if sink.as_ref().is_some_and(|sink| sink.token == token) {
                    *sink = None;
                }
            }
        }
    }

    #[cfg(test)]
    fn viewer(&self, session_id: u16) -> Option<u64> {
        let session = self.inner.lock().ok()?.sessions.get(&session_id).cloned()?;
        let token = session.sink.lock().ok()?.as_ref().map(|sink| sink.token);
        token
    }

    pub fn verify_password(&self, password: &str) -> bool {
        match &self.password {
            None => true,
            Some(expected) => expected == password,
        }
    }

    pub fn has_password(&self) -> bool {
        self.password.is_some()
    }
}

fn spawn_reader(
    session_id: u16,
    mut reader: Box<dyn Read + Send>,
    session: Arc<InnerSession>,
    events: broadcast::Sender<SessionEvent>,
) {
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut last) = session.last_active.lock() {
                        *last = Instant::now();
                    }
                    record_output(&session, session_id, &buffer[..n]);
                    let _ = events.send(SessionEvent::Output {
                        session_id,
                        token: 0,
                        data: buffer[..n].to_vec(),
                    });
                }
                Err(_) => break,
            }
        }
        session.reader_done.store(true, Ordering::Release);
    });
}

/// Append to history and forward to the current viewer under one lock, so the
/// replayed snapshot and the live stream can never overlap or leave a gap.
fn record_output(session: &InnerSession, session_id: u16, data: &[u8]) {
    let Ok(mut sink) = session.sink.lock() else {
        return;
    };
    if let Ok(mut scrollback) = session.scrollback.lock() {
        scrollback.record(data);
    }
    let Some(active) = sink.as_ref() else {
        return;
    };
    let event = SessionEvent::Output {
        session_id,
        token: active.token,
        data: data.to_vec(),
    };
    if active.tx.try_send(event).is_err() {
        // A viewer that cannot keep up is dropped instead of stalling the PTY.
        // History keeps recording, so it can reattach and replay later.
        tracing::warn!("terminal viewer for session {session_id} fell behind; detaching");
        *sink = None;
    }
}

fn spawn_waiter(
    session_id: u16,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    session: Arc<InnerSession>,
    events: broadcast::Sender<SessionEvent>,
    inner: Arc<Mutex<Inner>>,
) {
    thread::spawn(move || {
        let mut exit_code = -1;
        loop {
            let result = {
                let mut child = match child.lock() {
                    Ok(child) => child,
                    Err(_) => break,
                };
                child.try_wait()
            };
            match result {
                Ok(Some(status)) => {
                    exit_code = status.exit_code() as i32;
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(200)),
                Err(_) => break,
            }
        }
        // Give the reader a moment to drain the pty so EXIT is always the
        // final frame the viewer sees.
        let deadline = Instant::now() + Duration::from_millis(500);
        while !session.reader_done.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if let Ok(mut inner) = inner.lock() {
            // A finished shell has nothing to reattach to, so drop it from the
            // list the way an SSH session disappears when its shell exits.
            inner.sessions.remove(&session_id);
        }
        let viewer = session.sink.lock().ok().and_then(|mut sink| sink.take());
        if let Some(viewer) = viewer {
            let _ = viewer.tx.try_send(SessionEvent::Exited {
                session_id,
                token: viewer.token,
                exit_code,
            });
        }
        let _ = events.send(SessionEvent::Exited {
            session_id,
            token: 0,
            exit_code,
        });
    });
}

pub fn default_shell() -> String {
    if cfg!(windows) {
        "powershell".to_owned()
    } else {
        login_shell()
    }
}

/// The account's login shell, the way `sshd` picks it: the password database
/// entry, then `$SHELL`, then `bash`, then `sh`.
#[cfg(unix)]
fn login_shell() -> String {
    if let Some(account) = account() {
        if is_executable(&account.shell) {
            return account.shell;
        }
    }
    if let Ok(shell) = std::env::var("SHELL") {
        if is_executable(&shell) {
            return shell;
        }
    }
    if command_in_path("bash") {
        "bash".to_owned()
    } else {
        "/bin/sh".to_owned()
    }
}

#[cfg(not(unix))]
fn login_shell() -> String {
    "powershell".to_owned()
}

#[cfg(unix)]
struct Account {
    name: String,
    home: String,
    shell: String,
}

#[cfg(unix)]
fn account() -> Option<Account> {
    use std::ffi::CStr;

    let mut buffer = vec![0u8; 4096];
    loop {
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buffer.len() < (1 << 20) {
            let length = buffer.len() * 2;
            buffer.resize(length, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        let field = |value: *const libc::c_char| {
            if value.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(value) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        return Some(Account {
            name: field(entry.pw_name),
            home: field(entry.pw_dir),
            shell: field(entry.pw_shell),
        });
    }
}

#[cfg(unix)]
fn is_executable(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;

    !path.is_empty()
        && std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// Shells that accept `-l` and load their login profiles with it.
fn is_login_shell(shell: &str) -> bool {
    let name = Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    matches!(
        name,
        "sh" | "bash" | "dash" | "ksh" | "mksh" | "zsh" | "fish" | "nu"
    )
}

/// Absolute form of a shell name for the child's `SHELL` variable.
fn absolute_shell(shell: &str) -> String {
    if Path::new(shell).is_absolute() {
        return shell.to_owned();
    }
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join(shell))
                .find(|candidate| candidate.is_file())
        })
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or_else(|| shell.to_owned())
}

fn command_in_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
    })
}

fn resolve_shell(shell: &str) -> String {
    if cfg!(windows) {
        return match shell.to_ascii_lowercase().as_str() {
            "powershell" => "powershell.exe".to_owned(),
            "pwsh" => "pwsh.exe".to_owned(),
            "cmd" => "cmd.exe".to_owned(),
            "bash" => "bash.exe".to_owned(),
            "zsh" => "zsh.exe".to_owned(),
            other => other.to_owned(),
        };
    }
    if std::path::Path::new(shell).is_absolute() {
        return shell.to_owned();
    }
    let lower = shell.to_ascii_lowercase();
    let candidate = match lower.as_str() {
        "cmd" | "powershell" | "pwsh" => "sh",
        other => other,
    };
    if command_in_path(shell) {
        return shell.to_owned();
    }
    if command_in_path(candidate) {
        return candidate.to_owned();
    }
    let absolute = format!("/bin/{candidate}");
    if std::path::Path::new(&absolute).is_file() {
        absolute
    } else {
        shell.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_removes_the_session_from_the_list() {
        if cfg!(windows) {
            return;
        }
        let manager = SessionManager::new(None);
        let response = manager
            .open(&OpenRequest {
                session_id: None,
                name: None,
                shell: Some("sh".to_owned()),
                cols: Some(80),
                rows: Some(24),
            })
            .unwrap();
        assert!(manager.get(response.session_id).is_some());
        manager.kill(response.session_id).unwrap();
        assert!(manager.get(response.session_id).is_none());
        assert!(manager.list().sessions.is_empty());
        assert!(manager.kill(response.session_id).is_err());
    }

    #[test]
    fn default_shell_is_an_executable_login_shell() {
        if cfg!(windows) {
            return;
        }
        let shell = default_shell();
        assert!(
            std::path::Path::new(&shell).is_absolute() || command_in_path(&shell),
            "default shell {shell:?} does not resolve"
        );
        assert!(is_login_shell(&shell) || std::path::Path::new(&shell).is_file());
    }

    #[test]
    fn default_shell_follows_the_account_database() {
        if cfg!(windows) {
            return;
        }
        if let Some(account) = account() {
            if is_executable(&account.shell) {
                assert_eq!(default_shell(), account.shell);
            }
        }
    }

    #[test]
    fn default_shell_reaches_the_child_environment() {
        if cfg!(windows) {
            return;
        }
        let manager = SessionManager::new(None);
        let mut events = manager.subscribe();
        // Use a plain shell so the test does not depend on interactive line
        // editing; the environment setup is the same for every shell.
        let response = manager
            .open(&OpenRequest {
                session_id: None,
                name: None,
                shell: Some("sh".to_owned()),
                cols: Some(80),
                rows: Some(24),
            })
            .unwrap();
        manager.write(response.session_id, b"printenv\n").unwrap();
        let expected = format!("SHELL={}", absolute_shell("sh"));
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        while Instant::now() < deadline && !String::from_utf8_lossy(&output).contains(&expected) {
            if let Ok(event) = events.try_recv() {
                if let SessionEvent::Output {
                    session_id, data, ..
                } = event
                {
                    if session_id == response.session_id {
                        output.extend_from_slice(&data);
                    }
                }
            } else {
                thread::sleep(Duration::from_millis(20));
            }
        }
        manager.kill(response.session_id).unwrap();
        assert!(
            String::from_utf8_lossy(&output).contains(&expected),
            "child did not export {expected}"
        );
    }

    #[test]
    fn create_list_write_and_kill() {
        if cfg!(windows) {
            return;
        }
        let manager = SessionManager::new(None);
        let mut rx = manager.subscribe();
        let response = manager
            .open(&OpenRequest {
                session_id: None,
                name: Some("test".to_owned()),
                shell: Some("sh".to_owned()),
                cols: Some(80),
                rows: Some(24),
            })
            .unwrap();
        assert!(response.created);
        assert_eq!(manager.list().sessions.len(), 1);

        manager
            .write(response.session_id, b"echo openuur\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        while Instant::now() < deadline && !String::from_utf8_lossy(&output).contains("openuur") {
            if let Ok(event) = rx.try_recv() {
                if let SessionEvent::Output {
                    session_id, data, ..
                } = event
                {
                    if session_id == response.session_id {
                        output.extend_from_slice(&data);
                    }
                }
            } else {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("openuur"));

        manager.rename(response.session_id, "renamed").unwrap();
        assert_eq!(
            manager.get(response.session_id).unwrap().session_name,
            "renamed"
        );
        manager.kill(response.session_id).unwrap();
    }

    #[test]
    fn scrollback_keeps_the_newest_bytes() {
        let mut scrollback = Scrollback::new();
        scrollback.record(&vec![b'a'; SCROLLBACK_LIMIT]);
        scrollback.record(b"tail");
        let snapshot = scrollback.snapshot();
        assert_eq!(snapshot.len(), SCROLLBACK_LIMIT);
        assert!(snapshot.ends_with(b"tail"));
        assert!(snapshot.starts_with(b"a"));
    }

    /// Reads `Output` events until `marker` appears, or the deadline passes.
    async fn read_until(
        rx: &mut mpsc::Receiver<SessionEvent>,
        marker: &str,
        timeout: Duration,
    ) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut output = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return output;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(SessionEvent::Output { data, .. })) => {
                    output.extend(data);
                    if String::from_utf8_lossy(&output).contains(marker) {
                        return output;
                    }
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return output,
            }
        }
    }

    #[tokio::test]
    async fn reattaching_replays_history_and_respects_ownership() {
        if cfg!(windows) {
            return;
        }
        let manager = SessionManager::new(None);
        let mut events = manager.subscribe();
        let response = manager
            .open(&OpenRequest {
                session_id: None,
                name: None,
                shell: Some("sh".to_owned()),
                cols: Some(80),
                rows: Some(24),
            })
            .unwrap();
        let id = response.session_id;
        manager.write(id, b"echo HISTORY_MARK\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while Instant::now() < deadline && !String::from_utf8_lossy(&seen).contains("HISTORY_MARK")
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(SessionEvent::Output { data, .. })) => seen.extend(data),
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(
            String::from_utf8_lossy(&seen).contains("HISTORY_MARK"),
            "the session produced no initial output"
        );

        let (tx, mut rx) = mpsc::channel(16);
        manager.attach(id, 7, tx).unwrap();
        let Some(SessionEvent::Attach {
            token, snapshot, ..
        }) = rx.recv().await
        else {
            panic!("attach queued no replay");
        };
        assert_eq!(token, 7);
        assert!(
            String::from_utf8_lossy(&snapshot).contains("HISTORY_MARK"),
            "replay omitted earlier output"
        );

        // Live output continues to the attached viewer after the replay.
        manager.write(id, b"echo LIVE_MARK\n").unwrap();
        let live = read_until(&mut rx, "LIVE_MARK", Duration::from_secs(5)).await;
        assert!(String::from_utf8_lossy(&live).contains("LIVE_MARK"));

        // A stale token must not release the current viewer.
        manager.detach(id, 999);
        manager.write(id, b"echo STILL_ATTACHED\n").unwrap();
        let still = read_until(&mut rx, "STILL_ATTACHED", Duration::from_secs(5)).await;
        assert!(String::from_utf8_lossy(&still).contains("STILL_ATTACHED"));

        // The owning token releases the viewer; later output is not delivered.
        manager.detach(id, 7);
        manager.write(id, b"echo DETACHED_MARK\n").unwrap();
        let detached = read_until(&mut rx, "DETACHED_MARK", Duration::from_millis(400)).await;
        assert!(
            !String::from_utf8_lossy(&detached).contains("DETACHED_MARK"),
            "a detached viewer still received output"
        );
        manager.kill(id).unwrap();
    }

    /// Retries a probe until the shell actually executes input; a just-spawned
    /// login shell can miss bytes written before it starts reading.
    async fn wait_for_shell(
        manager: &SessionManager,
        events: &mut broadcast::Receiver<SessionEvent>,
        id: u16,
    ) {
        for _ in 0..100 {
            manager.write(id, b"printf 'READY_%s\\n' MARK\n").unwrap();
            let deadline = Instant::now() + Duration::from_millis(200);
            let mut seen = Vec::new();
            while Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match tokio::time::timeout(remaining, events.recv()).await {
                    Ok(Ok(SessionEvent::Output { data, .. })) => {
                        seen.extend(data);
                        if String::from_utf8_lossy(&seen).contains("READY_MARK") {
                            return;
                        }
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        }
        panic!("the shell never executed a probe");
    }

    #[tokio::test]
    async fn a_slow_viewer_is_detached_but_history_keeps_recording() {
        if cfg!(windows) {
            return;
        }
        let manager = SessionManager::new(None);
        let mut events = manager.subscribe();
        let response = manager
            .open(&OpenRequest {
                session_id: None,
                name: None,
                shell: Some("sh".to_owned()),
                cols: Some(80),
                rows: Some(24),
            })
            .unwrap();
        let id = response.session_id;
        wait_for_shell(&manager, &mut events, id).await;
        // One queued slot and no consumer: the second output event overflows.
        let (tx, _rx) = mpsc::channel(1);
        manager.attach(id, 1, tx).unwrap();
        assert_eq!(manager.viewer(id), Some(1));
        manager
            .write(
                id,
                b"head -c 200000 /dev/zero | tr '\\0' a; echo; printf 'SLOW_%s\\n' END\n",
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while Instant::now() < deadline && !String::from_utf8_lossy(&seen).contains("SLOW_END") {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(SessionEvent::Output { data, .. })) => seen.extend(data),
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(
            String::from_utf8_lossy(&seen).contains("SLOW_END"),
            "the flood never finished"
        );
        assert_eq!(
            manager.viewer(id),
            None,
            "the session kept a viewer that cannot keep up"
        );

        // History is independent of the detached viewer, so a new attach
        // still replays the flood.
        let (tx, mut rx) = mpsc::channel(4);
        manager.attach(id, 2, tx).unwrap();
        let Some(SessionEvent::Attach { snapshot, .. }) = rx.recv().await else {
            panic!("attach queued no replay");
        };
        assert!(
            snapshot.len() >= 200_000,
            "history lost output: {} bytes",
            snapshot.len()
        );
        assert!(String::from_utf8_lossy(&snapshot).contains("SLOW_END"));
        manager.kill(id).unwrap();
    }
}
