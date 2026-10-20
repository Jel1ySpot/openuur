use crate::error::{Error, Result};
use crate::util::now_ms;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

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
    Output { session_id: u16, data: Vec<u8> },
    Exited { session_id: u16, exit_code: i32 },
}

struct InnerSession {
    info: SessionInfo,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    last_active: Arc<Mutex<Instant>>,
}

impl InnerSession {
    fn info(&self) -> SessionInfo {
        let mut info = self.info.clone();
        info.last_active_ms = self
            .last_active
            .lock()
            .map(|t| t.elapsed().as_millis() as i64)
            .unwrap_or(0);
        info
    }
}

struct Inner {
    sessions: BTreeMap<u16, InnerSession>,
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
            .find(|s| s.info.session_name == name)
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
            if inner.sessions.values().any(|s| s.info.session_name == name) {
                return Err(Error::Session(format!(
                    "terminal session name already exists: {name}"
                )));
            }
        }
        let session_id = inner.allocate_id()?;
        let name = request
            .name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("{shell}-{session_id}"));

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut command = CommandBuilder::new(resolve_shell(&shell));
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        if let Some(home) = dirs::home_dir() {
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

        let session = InnerSession {
            info: SessionInfo {
                session_id,
                session_name: name.clone(),
                shell: shell.clone(),
                state: "running".to_owned(),
                created_at_ms,
                last_active_ms: 0,
                attachable: true,
                exit_code: None,
            },
            writer: Mutex::new(writer),
            master: Mutex::new(master),
            child: child.clone(),
            last_active: last_active.clone(),
        };
        inner.sessions.insert(session_id, session);

        spawn_reader(
            session_id,
            reader,
            self.events.clone(),
            self.inner.clone(),
            last_active,
        );
        spawn_waiter(session_id, child, self.events.clone(), self.inner.clone());

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
        match signal.to_ascii_uppercase().as_str() {
            "SIGINT" | "INT" | "C" => self.write(session_id, b"\x03"),
            "SIGQUIT" | "QUIT" => self.write(session_id, b"\x1c"),
            "SIGTERM" | "TERM" | "KILL" => self.kill(session_id),
            other => Err(Error::Session(format!("unsupported signal {other}"))),
        }
    }

    pub fn kill(&self, session_id: u16) -> Result<()> {
        let inner = self.inner.lock().expect("session manager poisoned");
        let session = inner
            .sessions
            .get(&session_id)
            .ok_or_else(|| Error::Session(format!("terminal session {session_id} not found")))?;
        let mut child = session
            .child
            .lock()
            .map_err(|_| Error::Session("terminal child poisoned".to_owned()))?;
        child.kill()?;
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
            .any(|s| s.info.session_name == new_name)
        {
            return Err(Error::Session(format!(
                "terminal session name already exists: {new_name}"
            )));
        }
        let session = inner
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| Error::Session(format!("terminal session {session_id} not found")))?;
        session.info.session_name = new_name.to_owned();
        Ok(())
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
    events: broadcast::Sender<SessionEvent>,
    inner: Arc<Mutex<Inner>>,
    last_active: Arc<Mutex<Instant>>,
) {
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut last) = last_active.lock() {
                        *last = Instant::now();
                    }
                    let data = buffer[..n].to_vec();
                    let _ = events.send(SessionEvent::Output { session_id, data });
                }
                Err(_) => break,
            }
            let exited = inner
                .lock()
                .ok()
                .and_then(|inner| {
                    inner
                        .sessions
                        .get(&session_id)
                        .map(|s| s.info.state == "exited")
                })
                .unwrap_or(true);
            if exited {
                break;
            }
        }
    });
}

fn spawn_waiter(
    session_id: u16,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
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
        if let Ok(mut inner) = inner.lock() {
            if let Some(session) = inner.sessions.get_mut(&session_id) {
                session.info.state = "exited".to_owned();
                session.info.attachable = false;
                session.info.exit_code = Some(exit_code);
                session.info.last_active_ms = 0;
            }
        }
        let _ = events.send(SessionEvent::Exited {
            session_id,
            exit_code,
        });
    });
}

pub fn default_shell() -> String {
    if cfg!(windows) {
        "powershell".to_owned()
    } else {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                if command_in_path("bash") {
                    "bash".to_owned()
                } else {
                    "sh".to_owned()
                }
            })
    }
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
                if let SessionEvent::Output { session_id, data } = event {
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
}
