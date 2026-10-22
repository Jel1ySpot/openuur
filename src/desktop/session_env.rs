//! Login-session discovery and process-environment projection.
//!
//! A host started as a system service does not inherit the desktop session's
//! environment: `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, and the session bus are
//! all missing. Portal, PipeWire, and zbus read those variables from the
//! process environment when they connect, so the host discovers the account's
//! active graphical session through logind, reads the session leader's
//! environment, and projects the relevant variables process-wide.
//!
//! Discovery is authoritative when logind answers: a user with no active
//! graphical session gets no fallback. When the system bus itself is not
//! available (non-systemd, containers), a verified process-environment session
//! keeps the host working. `:0` is never fabricated.
//!
//! A process-lifetime monitor polls every two seconds, publishes state through
//! a watch channel, and applies or clears the managed variables under one
//! global lock. Only those variables are touched; everything else in the host
//! process environment is left alone.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::watch;

use super::desktop_power::CALL_TIMEOUT;
use crate::desktop::session_power::{LOGIND_DEST, LOGIND_IFACE, LOGIND_PATH};

/// How long a desktop offer waits for a graphical session.
pub const SESSION_WAIT: Duration = Duration::from_secs(5);
/// Monitor polling interval.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Upper bound on one discovery round.
pub const DETECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Error text used when no graphical session can be found.
pub const WAITING: &str = "waiting for a graphical session";
const MAX_ENVIRON: u64 = 1 << 20;

/// Environment variables the host owns while a session is projected.
pub const MANAGED_KEYS: &[&str] = &[
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XDG_SESSION_TYPE",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_SESSION_ID",
    "XDG_CURRENT_DESKTOP",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionType {
    Wayland,
    X11,
}

impl SessionType {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "wayland" => Some(Self::Wayland),
            "x11" => Some(Self::X11),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wayland => "wayland",
            Self::X11 => "x11",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopKind {
    Gnome,
    Kde,
    Other,
}

impl DesktopKind {
    pub fn parse(value: &str) -> Self {
        for desktop in value.split(':') {
            match desktop.trim().to_ascii_lowercase().as_str() {
                "gnome" | "ubuntu" | "unity" => return Self::Gnome,
                "kde" | "plasma" | "plasmawayland" => return Self::Kde,
                _ => {}
            }
        }
        Self::Other
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Gnome => "GNOME",
            Self::Kde => "KDE",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnvSource {
    Logind,
    ProcessEnv,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEnv {
    pub session_id: String,
    pub session_type: SessionType,
    pub wayland_display: Option<String>,
    pub x11_display: Option<String>,
    pub runtime_dir: PathBuf,
    pub bus_address: String,
    pub desktop: DesktopKind,
    pub desktop_raw: String,
    pub leader_pid: u32,
    pub seat: String,
    pub source: SessionEnvSource,
}

impl SessionEnv {
    pub fn capture_display(&self) -> Option<&str> {
        match self.session_type {
            SessionType::Wayland => self.wayland_display.as_deref(),
            SessionType::X11 => self.x11_display.as_deref(),
        }
    }

    pub fn identity(&self) -> SessionIdentity {
        SessionIdentity {
            session_id: self.session_id.clone(),
            session_type: self.session_type,
            display: self.capture_display().map(str::to_owned),
            runtime_dir: self.runtime_dir.clone(),
            bus_address: self.bus_address.clone(),
            desktop: self.desktop,
            leader_pid: self.leader_pid,
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{} {} session {:?} on {}, display {}",
            self.desktop.label(),
            self.session_type.as_str(),
            self.session_id,
            if self.seat.is_empty() {
                "no seat"
            } else {
                &self.seat
            },
            self.capture_display().unwrap_or("-")
        )
    }
}

/// What a desktop peer is pinned to. Excludes log-only fields but keeps the
/// leader PID so a compositor restart invalidates old captures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionIdentity {
    pub session_id: String,
    pub session_type: SessionType,
    pub display: Option<String>,
    pub runtime_dir: PathBuf,
    pub bus_address: String,
    pub desktop: DesktopKind,
    pub leader_pid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    Unknown,
    Available(SessionEnv),
    NoSession,
    Unavailable(String),
}

impl SessionState {
    pub fn available(&self) -> Option<&SessionEnv> {
        match self {
            Self::Available(env) => Some(env),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub uid: u32,
    pub seat: String,
    pub leader_pid: u32,
    pub class: String,
    pub active: bool,
    pub session_type: String,
    pub remote: bool,
    pub display: String,
    pub desktop: String,
    pub path: String,
}

impl SessionRecord {
    pub fn is_candidate(&self, uid: u32) -> bool {
        self.uid == uid
            && self.active
            && !self.remote
            && self.class == "user"
            && SessionType::parse(&self.session_type).is_some()
    }

    fn kind(&self) -> Option<SessionType> {
        SessionType::parse(&self.session_type)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeaderEnv {
    pub wayland_display: Option<OsString>,
    pub x11_display: Option<OsString>,
    pub xdg_session_type: Option<String>,
    pub xdg_runtime_dir: Option<PathBuf>,
    pub bus_address: Option<String>,
    pub xdg_session_id: Option<String>,
    pub xdg_current_desktop: Option<String>,
    pub xdg_session_desktop: Option<String>,
    pub desktop_session: Option<String>,
}

impl LeaderEnv {
    /// Parse a NUL-separated `/proc/<pid>/environ`. Only whitelisted keys are
    /// retained; the rest of the block may contain secrets.
    pub fn parse(bytes: &[u8]) -> Self {
        let mut env = Self::default();
        for entry in bytes.split(|byte| *byte == 0) {
            let Some(index) = entry.iter().position(|byte| *byte == b'=') else {
                continue;
            };
            let (key, value) = entry.split_at(index);
            let value = &value[1..];
            let value_string = || String::from_utf8_lossy(value).into_owned();
            match key {
                b"WAYLAND_DISPLAY" => {
                    env.wayland_display = Some(OsString::from_vec(value.to_vec()))
                }
                b"DISPLAY" => env.x11_display = Some(OsString::from_vec(value.to_vec())),
                b"XDG_SESSION_TYPE" => env.xdg_session_type = Some(value_string()),
                b"XDG_RUNTIME_DIR" => {
                    env.xdg_runtime_dir =
                        Some(PathBuf::from(String::from_utf8_lossy(value).into_owned()))
                }
                b"DBUS_SESSION_BUS_ADDRESS" => env.bus_address = Some(value_string()),
                b"XDG_SESSION_ID" => env.xdg_session_id = Some(value_string()),
                b"XDG_CURRENT_DESKTOP" => env.xdg_current_desktop = Some(value_string()),
                b"XDG_SESSION_DESKTOP" => env.xdg_session_desktop = Some(value_string()),
                b"DESKTOP_SESSION" => env.desktop_session = Some(value_string()),
                _ => {}
            }
        }
        env
    }

    pub fn from_process_env() -> Self {
        let var = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
        Self {
            wayland_display: std::env::var_os("WAYLAND_DISPLAY").filter(|value| !value.is_empty()),
            x11_display: std::env::var_os("DISPLAY").filter(|value| !value.is_empty()),
            xdg_session_type: var("XDG_SESSION_TYPE"),
            xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            bus_address: var("DBUS_SESSION_BUS_ADDRESS"),
            xdg_session_id: var("XDG_SESSION_ID"),
            xdg_current_desktop: var("XDG_CURRENT_DESKTOP"),
            xdg_session_desktop: var("XDG_SESSION_DESKTOP"),
            desktop_session: var("DESKTOP_SESSION"),
        }
    }
}

pub fn target_uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn runtime_dir_for(uid: u32, leader: &LeaderEnv) -> PathBuf {
    match &leader.xdg_runtime_dir {
        Some(path) if path.is_absolute() => path.clone(),
        _ => PathBuf::from(format!("/run/user/{uid}")),
    }
}

/// Sorted `wayland-*` sockets in a runtime directory.
pub fn scan_wayland_sockets(runtime_dir: &Path) -> Vec<String> {
    use std::os::unix::fs::FileTypeExt;
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    let mut sockets: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("wayland-") || name.ends_with(".lock") {
                return None;
            }
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_socket())
                .map(|_| name)
        })
        .collect();
    sockets.sort();
    sockets
}

/// Read only the whitelisted variables from a session leader.
pub fn read_leader_environ(pid: u32) -> Option<LeaderEnv> {
    use std::io::Read;
    let file = std::fs::File::open(format!("/proc/{pid}/environ")).ok()?;
    let mut bytes = Vec::new();
    let mut reader = file.take(MAX_ENVIRON);
    reader.read_to_end(&mut bytes).ok()?;
    (!bytes.is_empty()).then(|| LeaderEnv::parse(&bytes))
}

/// Deterministic candidate ranking: the user's display session first, then a
/// seated session, then Wayland over X11, then the lowest session id.
pub fn select_session<'a>(
    records: &'a [SessionRecord],
    uid: u32,
    display_session: Option<&str>,
) -> Option<&'a SessionRecord> {
    let mut candidates: Vec<&SessionRecord> = records
        .iter()
        .filter(|record| record.is_candidate(uid))
        .collect();
    candidates.sort_by(|left, right| {
        let left_display = display_session == Some(left.id.as_str());
        let right_display = display_session == Some(right.id.as_str());
        right_display
            .cmp(&left_display)
            .then_with(|| (!right.seat.is_empty()).cmp(&(!left.seat.is_empty())))
            .then_with(|| {
                let left_wayland = left.kind() == Some(SessionType::Wayland);
                let right_wayland = right.kind() == Some(SessionType::Wayland);
                right_wayland.cmp(&left_wayland)
            })
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates.first().copied()
}

/// Build the effective environment for one record. Displays are never
/// fabricated: a Wayland session without a socket, and an X11 session without
/// `DISPLAY`, are errors.
pub fn resolve_env(
    record: &SessionRecord,
    leader: &LeaderEnv,
    default_runtime_dir: &Path,
    scanned_sockets: &[String],
) -> Result<SessionEnv, String> {
    let session_type = SessionType::parse(&record.session_type)
        .ok_or_else(|| format!("session {} has no graphical type", record.id))?;
    let runtime_dir = match &leader.xdg_runtime_dir {
        Some(path) if path.is_absolute() => path.clone(),
        _ => default_runtime_dir.to_path_buf(),
    };
    let wayland_display = leader
        .wayland_display
        .as_ref()
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            record
                .display
                .starts_with("wayland-")
                .then(|| record.display.clone())
        })
        .or_else(|| scanned_sockets.first().cloned());
    let x11_display = leader
        .x11_display
        .as_ref()
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            (!record.display.is_empty() && !record.display.starts_with("wayland-"))
                .then(|| record.display.clone())
        });
    match session_type {
        SessionType::Wayland if wayland_display.is_none() => {
            return Err("session has no Wayland socket".to_owned())
        }
        SessionType::X11 if x11_display.is_none() => {
            return Err("X11 session has no DISPLAY; refusing to guess :0".to_owned())
        }
        _ => {}
    }
    let bus_address = leader
        .bus_address
        .clone()
        .filter(|value| !value.is_empty() && !value.starts_with("autolaunch:"))
        .unwrap_or_else(|| format!("unix:path={}/bus", runtime_dir.display()));
    let desktop_raw = leader
        .xdg_current_desktop
        .clone()
        .or_else(|| leader.xdg_session_desktop.clone())
        .or_else(|| leader.desktop_session.clone())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| record.desktop.clone());
    Ok(SessionEnv {
        session_id: record.id.clone(),
        session_type,
        wayland_display,
        x11_display,
        runtime_dir,
        bus_address,
        desktop: DesktopKind::parse(&desktop_raw),
        desktop_raw,
        leader_pid: record.leader_pid,
        seat: record.seat.clone(),
        source: SessionEnvSource::Logind,
    })
}

// ---- logind --------------------------------------------------------------

/// Why one discovery round did not produce a usable session.
enum DetectFailure {
    /// Bus or transport trouble; the last known session may still be valid.
    Transient(String),
    /// logind answered, but the selected candidate cannot be used.
    Unusable(String),
}

struct LogindClient {
    connection: Option<zbus::Connection>,
}

impl LogindClient {
    fn new() -> Self {
        Self { connection: None }
    }

    async fn connection(&mut self) -> Result<zbus::Connection, String> {
        if let Some(connection) = &self.connection {
            return Ok(connection.clone());
        }
        let connection = tokio::time::timeout(CALL_TIMEOUT, zbus::Connection::system())
            .await
            .map_err(|_| "system bus connection timed out".to_owned())?
            .map_err(|error| format!("system bus unavailable: {error}"))?;
        self.connection = Some(connection.clone());
        Ok(connection)
    }

    /// logind answered but there is no usable graphical session.
    fn unusable<T>(&self, error: String) -> Result<T, DetectFailure> {
        Err(DetectFailure::Unusable(error))
    }

    /// Bus or transport trouble: the last known session may still be valid.
    fn transient<T>(&mut self, error: String) -> Result<T, DetectFailure> {
        self.connection = None;
        Err(DetectFailure::Transient(error))
    }

    async fn detect(&mut self) -> Result<Option<SessionEnv>, DetectFailure> {
        let uid = target_uid();
        let connection = match self.connection().await {
            Ok(connection) => connection,
            Err(error) => return Err(DetectFailure::Transient(error)),
        };
        // GetUser only supplies the `User.Display` hint; ListSessionsEx decides
        // whether a graphical session exists. logind frees the User object when
        // the last session ends, so NoSuchUser means "not logged in", not a bus
        // failure. Any other error only costs the hint for this round.
        let display_session = match get_user(&connection, uid).await {
            Ok(path) => user_display(&connection, &path).await,
            Err(error) => {
                tracing::debug!("logind GetUser unavailable: {error}");
                None
            }
        };
        let rows = match list_sessions_ex(&connection).await {
            Ok(rows) => rows,
            Err(error) => match list_sessions(&connection).await {
                Ok(rows) => rows,
                Err(fallback) => {
                    return self.transient(format!("{error}; {fallback}"));
                }
            },
        };
        // Only this user's rows can decide whether their session exists; a
        // failure fetching another user's properties must not look like "no
        // session".
        let rows: Vec<&SessionRow> = rows.iter().filter(|row| row.uid == uid).collect();
        let properties = futures_util::future::join_all(
            rows.iter()
                .map(|row| async { session_properties(&connection, &row.path).await }),
        )
        .await;
        let mut records = Vec::with_capacity(rows.len());
        for (row, properties) in rows.iter().zip(properties) {
            let properties = match properties {
                Ok(properties) => properties,
                Err(error) => return self.transient(error),
            };
            records.push(SessionRecord {
                id: row.id.clone(),
                uid: row.uid,
                seat: row.seat.clone(),
                leader_pid: uint_property(&properties, "Leader").unwrap_or(row.leader_pid),
                class: string_property(&properties, "Class").unwrap_or_default(),
                active: bool_property(&properties, "Active").unwrap_or(false),
                session_type: string_property(&properties, "Type").unwrap_or_default(),
                remote: bool_property(&properties, "Remote").unwrap_or(false),
                display: string_property(&properties, "Display").unwrap_or_default(),
                desktop: string_property(&properties, "Desktop").unwrap_or_default(),
                path: row.path.clone(),
            });
        }
        let Some(record) = select_session(&records, uid, display_session.as_deref()) else {
            return Ok(None);
        };
        let leader = read_leader_environ(record.leader_pid).unwrap_or_default();
        let runtime_dir = runtime_dir_for(uid, &leader);
        let sockets = if record.kind() == Some(SessionType::Wayland) {
            scan_wayland_sockets(&runtime_dir)
        } else {
            Vec::new()
        };
        // logind answered and this candidate is the one; if its environment
        // cannot be built, there is no session to keep using.
        match resolve_env(record, &leader, &runtime_dir, &sockets) {
            Ok(env) => Ok(Some(env)),
            Err(error) => self.unusable(error),
        }
    }
}

#[derive(Clone)]
struct SessionRow {
    id: String,
    uid: u32,
    seat: String,
    leader_pid: u32,
    path: String,
}

async fn get_user(connection: &zbus::Connection, uid: u32) -> Result<String, String> {
    let arguments = (uid,);
    let call = connection.call_method(
        Some(LOGIND_DEST),
        LOGIND_PATH,
        Some(LOGIND_IFACE),
        "GetUser",
        &arguments,
    );
    let reply = tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .map_err(|_| "GetUser timed out".to_owned())?
        .map_err(|error| format!("GetUser failed: {error}"))?;
    let path: zbus::zvariant::OwnedObjectPath = reply
        .body()
        .deserialize()
        .map_err(|error| format!("GetUser reply: {error}"))?;
    Ok(path.to_string())
}

async fn user_display(connection: &zbus::Connection, user_path: &str) -> Option<String> {
    let call = connection.call_method(
        Some(LOGIND_DEST),
        user_path,
        Some("org.freedesktop.DBus.Properties"),
        "GetAll",
        &("org.freedesktop.login1.User",),
    );
    let reply = tokio::time::timeout(CALL_TIMEOUT, call).await.ok()?.ok()?;
    let properties: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
        reply.body().deserialize().ok()?;
    let owned = properties.get("Display")?.try_clone().ok()?;
    let value = zbus::zvariant::Value::from(owned);
    let (session, _) = value
        .downcast::<(String, zbus::zvariant::OwnedObjectPath)>()
        .ok()?;
    (!session.is_empty()).then_some(session)
}

async fn session_properties(
    connection: &zbus::Connection,
    path: &str,
) -> Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>, String> {
    let call = connection.call_method(
        Some(LOGIND_DEST),
        path,
        Some("org.freedesktop.DBus.Properties"),
        "GetAll",
        &("org.freedesktop.login1.Session",),
    );
    let reply = tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .map_err(|_| "session properties timed out".to_owned())?
        .map_err(|error| format!("session properties failed: {error}"))?;
    reply
        .body()
        .deserialize()
        .map_err(|error| format!("session properties reply: {error}"))
}

async fn list_sessions_ex(connection: &zbus::Connection) -> Result<Vec<SessionRow>, String> {
    #[allow(clippy::type_complexity)]
    let call = connection.call_method(
        Some(LOGIND_DEST),
        LOGIND_PATH,
        Some(LOGIND_IFACE),
        "ListSessionsEx",
        &(),
    );
    let reply = tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .map_err(|_| "ListSessionsEx timed out".to_owned())?
        .map_err(|error| format!("ListSessionsEx failed: {error}"))?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        String,
        u32,
        String,
        String,
        u32,
        String,
        String,
        bool,
        u64,
        zbus::zvariant::OwnedObjectPath,
    )> = reply
        .body()
        .deserialize()
        .map_err(|error| format!("ListSessionsEx reply: {error}"))?;
    Ok(rows
        .into_iter()
        .map(
            |(id, uid, _user, seat, leader, _class, _tty, _idle, _since, path)| SessionRow {
                id,
                uid,
                seat,
                leader_pid: leader,
                path: path.to_string(),
            },
        )
        .collect())
}

async fn list_sessions(connection: &zbus::Connection) -> Result<Vec<SessionRow>, String> {
    let call = connection.call_method(
        Some(LOGIND_DEST),
        LOGIND_PATH,
        Some(LOGIND_IFACE),
        "ListSessions",
        &(),
    );
    let reply = tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .map_err(|_| "ListSessions timed out".to_owned())?
        .map_err(|error| format!("ListSessions failed: {error}"))?;
    let rows: Vec<(String, u32, String, String, zbus::zvariant::OwnedObjectPath)> = reply
        .body()
        .deserialize()
        .map_err(|error| format!("ListSessions reply: {error}"))?;
    Ok(rows
        .into_iter()
        .map(|(id, uid, _user, seat, path)| SessionRow {
            id,
            uid,
            seat,
            leader_pid: 0,
            path: path.to_string(),
        })
        .collect())
}

fn bool_property(
    properties: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    key: &str,
) -> Option<bool> {
    properties.get(key)?.downcast_ref::<bool>().ok()
}

fn string_property(
    properties: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    key: &str,
) -> Option<String> {
    let value = properties.get(key)?;
    value
        .downcast_ref::<&str>()
        .ok()
        .map(|text| (*text).to_owned())
        .or_else(|| value.downcast_ref::<String>().ok())
}

fn uint_property(
    properties: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    key: &str,
) -> Option<u32> {
    properties.get(key)?.downcast_ref::<u32>().ok()
}

// ---- detection state -----------------------------------------------------

static STATE: OnceLock<watch::Sender<SessionState>> = OnceLock::new();
static MONITOR: OnceLock<()> = OnceLock::new();
static AUTHORITATIVE: AtomicBool = AtomicBool::new(false);

fn state_sender() -> &'static watch::Sender<SessionState> {
    STATE.get_or_init(|| watch::channel(SessionState::Unknown).0)
}

pub fn subscribe() -> watch::Receiver<SessionState> {
    ensure_monitor();
    state_sender().subscribe()
}

/// Start the process-lifetime poller. It keeps the environment fresh across
/// signaling reconnects and room-less periods.
pub fn ensure_monitor() {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if MONITOR.set(()).is_err() {
        return;
    }
    handle.spawn(async move {
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut client = LogindClient::new();
        loop {
            tick.tick().await;
            let next = detect_with(&mut client).await;
            reconcile(next.available());
            publish_state(next);
        }
    });
}

/// One discovery round. Keeps the last applied session through a transient
/// system-bus failure and only uses the process-environment fallback when
/// logind has never been authoritative.
pub async fn detect() -> SessionState {
    let mut client = LogindClient::new();
    detect_with(&mut client).await
}

async fn detect_with(client: &mut LogindClient) -> SessionState {
    let outcome = tokio::time::timeout(DETECT_TIMEOUT, client.detect()).await;
    match outcome {
        Ok(Ok(Some(env))) => SessionState::Available(env),
        Ok(Ok(None)) => SessionState::NoSession,
        // The account's session exists but cannot be used; keeping an older
        // session of the same account would point capture at a dead display.
        Ok(Err(DetectFailure::Unusable(error))) => {
            tracing::debug!("graphical session candidate unusable: {error}");
            SessionState::NoSession
        }
        Ok(Err(DetectFailure::Transient(error))) => unavailable_or_last(error),
        Err(_) => unavailable_or_last("session discovery timed out".to_owned()),
    }
}

fn unavailable_or_last(error: String) -> SessionState {
    if let Some(last) = current() {
        tracing::debug!(
            "session discovery failed ({error}); keeping {}",
            last.describe()
        );
        return SessionState::Available(last);
    }
    if !authoritative() {
        if let Some(env) = process_env_session() {
            tracing::debug!("using the process environment for the graphical session");
            return SessionState::Available(env);
        }
    }
    SessionState::Unavailable(error)
}

/// A session derived from this process's own environment, verified against the
/// runtime directory. Used only when logind is unreachable.
fn process_env_session() -> Option<SessionEnv> {
    // A service that runs as root must not adopt a display-manager or another
    // user's environment; logind is the only trusted source there. This also
    // keeps the greeter out of reach on systems without logind.
    if unsafe { libc::geteuid() == 0 } && std::env::var_os("UU_SESSION_ENV_FALLBACK").is_none() {
        tracing::info!(
            "not trusting the process environment for the graphical session as root; \
             set UU_SESSION_ENV_FALLBACK=1 to override"
        );
        return None;
    }
    let leader = LeaderEnv::from_process_env();
    let uid = target_uid();
    let session_type = leader
        .xdg_session_type
        .as_deref()
        .and_then(SessionType::parse)
        .or_else(|| {
            (leader.wayland_display.is_some())
                .then_some(SessionType::Wayland)
                .or_else(|| leader.x11_display.is_some().then_some(SessionType::X11))
        })?;
    let runtime_dir = runtime_dir_for(uid, &leader);
    let sockets = match session_type {
        SessionType::Wayland => {
            let display = leader
                .wayland_display
                .as_ref()
                .map(|value| value.to_string_lossy().into_owned())
                .filter(|value| !value.is_empty())?;
            if !Path::new(&display).is_absolute() {
                // Relative Wayland names must name a real socket.
                if !runtime_dir.join(&display).exists() {
                    return None;
                }
            }
            scan_wayland_sockets(&runtime_dir)
        }
        SessionType::X11 => {
            leader.x11_display.as_ref()?;
            Vec::new()
        }
    };
    let record = SessionRecord {
        id: leader.xdg_session_id.clone().unwrap_or_default(),
        uid,
        seat: String::new(),
        leader_pid: 0,
        class: "user".to_owned(),
        active: true,
        session_type: session_type.as_str().to_owned(),
        remote: false,
        display: leader
            .wayland_display
            .as_ref()
            .or(leader.x11_display.as_ref())
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default(),
        desktop: leader.xdg_current_desktop.clone().unwrap_or_default(),
        path: String::new(),
    };
    let mut env = resolve_env(&record, &leader, &runtime_dir, &sockets).ok()?;
    env.source = SessionEnvSource::ProcessEnv;
    Some(env)
}

pub fn authoritative() -> bool {
    AUTHORITATIVE.load(Ordering::Acquire)
}

fn publish_state(state: SessionState) {
    let sender = state_sender();
    let unchanged = same_state(&sender.borrow(), &state);
    if unchanged {
        return;
    }
    match &state {
        SessionState::Available(env) => {
            tracing::info!("desktop session available: {}", env.describe())
        }
        SessionState::NoSession => {
            tracing::info!("desktop session lost; desktop captures will close")
        }
        SessionState::Unavailable(error) => {
            tracing::debug!("session discovery unavailable: {error}")
        }
        SessionState::Unknown => {}
    }
    // Only an actual logind answer makes later process-environment fallback
    // unsafe; a failed discovery round proves nothing.
    let logind_answered = match &state {
        SessionState::NoSession => true,
        SessionState::Available(env) => env.source == SessionEnvSource::Logind,
        SessionState::Unknown | SessionState::Unavailable(_) => false,
    };
    if logind_answered {
        AUTHORITATIVE.store(true, Ordering::Release);
    }
    let _ = sender.send_replace(state);
}

fn same_state(left: &SessionState, right: &SessionState) -> bool {
    match (left, right) {
        (SessionState::Available(left), SessionState::Available(right)) => left == right,
        (SessionState::Unavailable(_), SessionState::Unavailable(_)) => true,
        (SessionState::NoSession, SessionState::NoSession) => true,
        (SessionState::Unknown, SessionState::Unknown) => true,
        _ => false,
    }
}

pub async fn wait_for_session(timeout: Duration) -> Result<SessionEnv, String> {
    let mut receiver = subscribe();
    wait_for_session_rx(&mut receiver, timeout).await
}

async fn wait_for_session_rx(
    receiver: &mut watch::Receiver<SessionState>,
    timeout: Duration,
) -> Result<SessionEnv, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(env) = receiver.borrow().available() {
            return Ok(env.clone());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(WAITING.to_owned());
        }
        match tokio::time::timeout(remaining, receiver.changed()).await {
            Ok(Ok(())) => {}
            // The sender is process-lifetime; a closed channel means no
            // further updates can ever arrive.
            Ok(Err(_)) => return Err(WAITING.to_owned()),
            Err(_) => return Err(WAITING.to_owned()),
        }
    }
}

pub fn current() -> Option<SessionEnv> {
    ENV.get()
        .and_then(|env| env.lock().ok().and_then(|managed| managed.current.clone()))
}

// ---- process-environment projection -------------------------------------

trait EnvOps {
    fn get(&self, key: &str) -> Option<OsString>;
    fn set(&mut self, key: &str, value: OsString);
    fn remove(&mut self, key: &str);
}

struct ProcessEnv;

impl EnvOps for ProcessEnv {
    fn get(&self, key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }

    fn set(&mut self, key: &str, value: OsString) {
        std::env::set_var(key, value);
    }

    fn remove(&mut self, key: &str) {
        std::env::remove_var(key);
    }
}

struct ManagedEnv<S: EnvOps> {
    baseline: Option<BTreeMap<&'static str, Option<OsString>>>,
    current: Option<SessionEnv>,
    ops: S,
}

impl<S: EnvOps> ManagedEnv<S> {
    fn new(ops: S) -> Self {
        Self {
            baseline: None,
            current: None,
            ops,
        }
    }

    fn apply(&mut self, env: &SessionEnv) {
        if self.baseline.is_none() {
            self.baseline = Some(
                MANAGED_KEYS
                    .iter()
                    .map(|key| (*key, self.ops.get(key)))
                    .collect(),
            );
        }
        self.write("WAYLAND_DISPLAY", env.wayland_display.clone());
        self.write("DISPLAY", env.x11_display.clone());
        self.write(
            "XDG_SESSION_TYPE",
            Some(env.session_type.as_str().to_owned()),
        );
        self.write(
            "XDG_RUNTIME_DIR",
            Some(env.runtime_dir.to_string_lossy().into_owned()),
        );
        self.write("DBUS_SESSION_BUS_ADDRESS", Some(env.bus_address.clone()));
        self.write(
            "XDG_SESSION_ID",
            (!env.session_id.is_empty()).then(|| env.session_id.clone()),
        );
        self.write(
            "XDG_CURRENT_DESKTOP",
            (!env.desktop_raw.is_empty()).then(|| env.desktop_raw.clone()),
        );
        self.current = Some(env.clone());
    }

    fn write(&mut self, key: &str, value: Option<String>) {
        match value {
            Some(value) => self.ops.set(key, OsString::from(value)),
            None => self.ops.remove(key),
        }
    }

    fn clear(&mut self) {
        let Some(baseline) = self.baseline.clone() else {
            self.current = None;
            return;
        };
        for (key, value) in baseline {
            match value {
                Some(value) => self.ops.set(key, value),
                None => self.ops.remove(key),
            }
        }
        self.current = None;
    }

    fn reconcile(&mut self, next: Option<&SessionEnv>) {
        match next {
            Some(next) if self.current.as_ref() != Some(next) => self.apply(next),
            None if self.current.is_some() => self.clear(),
            _ => {}
        }
    }
}

static ENV: OnceLock<Mutex<ManagedEnv<ProcessEnv>>> = OnceLock::new();

fn managed_env() -> &'static Mutex<ManagedEnv<ProcessEnv>> {
    ENV.get_or_init(|| Mutex::new(ManagedEnv::new(ProcessEnv)))
}

pub fn apply(env: &SessionEnv) {
    if let Ok(mut managed) = managed_env().lock() {
        managed.apply(env);
    }
}

pub fn clear() {
    if let Ok(mut managed) = managed_env().lock() {
        managed.clear();
    }
}

pub fn reconcile(next: Option<&SessionEnv>) {
    if let Ok(mut managed) = managed_env().lock() {
        managed.reconcile(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, uid: u32) -> SessionRecord {
        SessionRecord {
            id: id.to_owned(),
            uid,
            seat: "seat0".to_owned(),
            leader_pid: 100,
            class: "user".to_owned(),
            active: true,
            session_type: "wayland".to_owned(),
            remote: false,
            display: "wayland-1".to_owned(),
            desktop: "sway".to_owned(),
            path: format!("/org/freedesktop/login1/session/{id}"),
        }
    }

    fn leader() -> LeaderEnv {
        LeaderEnv {
            wayland_display: Some(OsString::from("wayland-2")),
            x11_display: Some(OsString::from(":1")),
            xdg_session_type: Some("wayland".to_owned()),
            xdg_runtime_dir: Some(PathBuf::from("/run/user/1000")),
            bus_address: Some("unix:path=/run/user/1000/bus".to_owned()),
            xdg_session_id: Some("2".to_owned()),
            xdg_current_desktop: Some("GNOME".to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn candidate_filter_rejects_other_uid_inactive_remote_non_user_and_non_graphical() {
        let mut good = record("2", 1000);
        assert!(good.is_candidate(1000));
        good.uid = 1001;
        assert!(!good.is_candidate(1000));
        good.uid = 1000;
        good.active = false;
        assert!(!good.is_candidate(1000));
        good.active = true;
        good.remote = true;
        assert!(!good.is_candidate(1000));
        good.remote = false;
        good.class = "greeter".to_owned();
        assert!(!good.is_candidate(1000));
        good.class = "user".to_owned();
        good.session_type = "tty".to_owned();
        assert!(!good.is_candidate(1000));
    }

    #[test]
    fn display_session_wins_over_other_active_sessions() {
        let records = vec![record("1", 1000), record("2", 1000)];
        let selected = select_session(&records, 1000, Some("2")).unwrap();
        assert_eq!(selected.id, "2");
    }

    #[test]
    fn selection_prefers_seat_then_wayland_then_stable_id() {
        let mut unseated = record("1", 1000);
        unseated.seat.clear();
        let seated = record("2", 1000);
        let records = vec![unseated, seated];
        assert_eq!(select_session(&records, 1000, None).unwrap().id, "2");

        let mut x11 = record("0", 1000);
        x11.session_type = "x11".to_owned();
        let wayland = record("9", 1000);
        let records = vec![x11, wayland];
        assert_eq!(select_session(&records, 1000, None).unwrap().id, "9");

        let records = vec![record("b", 1000), record("a", 1000)];
        assert_eq!(select_session(&records, 1000, None).unwrap().id, "a");
    }

    #[test]
    fn leader_environ_parses_only_whitelisted_keys_and_ignores_malformed_entries() {
        let bytes = b"WAYLAND_DISPLAY=wayland-1\0SECRET_TOKEN=leak\0MALFORMED\0DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus\0";
        let env = LeaderEnv::parse(bytes);
        let debug = format!("{env:?}");
        assert_eq!(env.wayland_display.unwrap(), OsString::from("wayland-1"));
        assert_eq!(
            env.bus_address.unwrap(),
            "unix:path=/run/user/1000/bus".to_owned()
        );
        assert!(env.x11_display.is_none());
        assert!(!debug.contains("leak"));
    }

    #[test]
    fn leader_environ_preserves_non_utf8_display_bytes() {
        let mut bytes = b"WAYLAND_DISPLAY=".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        let env = LeaderEnv::parse(&bytes);
        assert_eq!(
            env.wayland_display.unwrap().as_encoded_bytes(),
            &[0xff, 0xfe]
        );
    }

    #[test]
    fn resolve_env_prefers_leader_display_then_logind_display_then_scanned_socket() {
        let mut record = record("2", 1000);
        record.display = "wayland-9".to_owned();
        let env = resolve_env(
            &record,
            &leader(),
            Path::new("/run/user/1000"),
            &["wayland-5".to_owned()],
        )
        .unwrap();
        assert_eq!(env.wayland_display.as_deref(), Some("wayland-2"));

        let env = resolve_env(
            &record,
            &LeaderEnv::default(),
            Path::new("/run/user/1000"),
            &["wayland-5".to_owned()],
        )
        .unwrap();
        assert_eq!(env.wayland_display.as_deref(), Some("wayland-9"));

        record.display.clear();
        let env = resolve_env(
            &record,
            &LeaderEnv::default(),
            Path::new("/run/user/1000"),
            &["wayland-5".to_owned()],
        )
        .unwrap();
        assert_eq!(env.wayland_display.as_deref(), Some("wayland-5"));
    }

    #[test]
    fn resolve_env_never_synthesizes_x11_display() {
        let mut record = record("2", 1000);
        record.session_type = "x11".to_owned();
        record.display.clear();
        let mut env = LeaderEnv::default();
        env.wayland_display = None;
        let error = resolve_env(&record, &env, Path::new("/run/user/1000"), &[]).unwrap_err();
        assert!(error.contains(":0"), "{error}");
        assert!(!error.ends_with("DISPLAY :0"));
    }

    #[test]
    fn resolve_env_defaults_the_bus_and_runtime_dir() {
        let record = record("2", 1000);
        let mut env = LeaderEnv::default();
        env.wayland_display = Some(OsString::from("wayland-1"));
        let resolved = resolve_env(&record, &env, Path::new("/run/user/1000"), &[]).unwrap();
        assert_eq!(resolved.runtime_dir, PathBuf::from("/run/user/1000"));
        assert_eq!(
            resolved.bus_address,
            "unix:path=/run/user/1000/bus".to_owned()
        );
    }

    #[test]
    fn resolve_env_keeps_xwayland_display_alongside_wayland_display() {
        let record = record("2", 1000);
        let env = resolve_env(&record, &leader(), Path::new("/run/user/1000"), &[]).unwrap();
        assert_eq!(env.wayland_display.as_deref(), Some("wayland-2"));
        assert_eq!(env.x11_display.as_deref(), Some(":1"));
    }

    #[test]
    fn scan_wayland_sockets_skips_lock_files_and_non_sockets() {
        let dir = std::env::temp_dir().join(format!("openuur-sockets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::net::UnixListener::bind(dir.join("wayland-1")).unwrap();
        std::os::unix::net::UnixListener::bind(dir.join("wayland-0")).unwrap();
        std::fs::write(dir.join("wayland-1.lock"), b"").unwrap();
        std::fs::write(dir.join("wayland-2"), b"").unwrap();
        let sockets = scan_wayland_sockets(&dir);
        assert_eq!(
            sockets,
            vec!["wayland-0".to_owned(), "wayland-1".to_owned()]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[derive(Default)]
    struct MapEnv(BTreeMap<String, OsString>);

    impl EnvOps for MapEnv {
        fn get(&self, key: &str) -> Option<OsString> {
            self.0.get(key).cloned()
        }

        fn set(&mut self, key: &str, value: OsString) {
            self.0.insert(key.to_owned(), value);
        }

        fn remove(&mut self, key: &str) {
            self.0.remove(key);
        }
    }

    fn managed() -> ManagedEnv<MapEnv> {
        let mut ops = MapEnv::default();
        ops.set("UNMANAGED", OsString::from("keep"));
        ops.set("DISPLAY", OsString::from(":9"));
        ManagedEnv::new(ops)
    }

    fn sample_env() -> SessionEnv {
        SessionEnv {
            session_id: "2".to_owned(),
            session_type: SessionType::Wayland,
            wayland_display: Some("wayland-1".to_owned()),
            x11_display: Some(":0".to_owned()),
            runtime_dir: PathBuf::from("/run/user/1000"),
            bus_address: "unix:path=/run/user/1000/bus".to_owned(),
            desktop: DesktopKind::Gnome,
            desktop_raw: "GNOME".to_owned(),
            leader_pid: 100,
            seat: "seat0".to_owned(),
            source: SessionEnvSource::Logind,
        }
    }

    #[test]
    fn apply_writes_exactly_the_managed_keys_and_leaves_others_untouched() {
        let mut env = managed();
        env.apply(&sample_env());
        assert_eq!(
            env.ops.0.get("WAYLAND_DISPLAY"),
            Some(&OsString::from("wayland-1"))
        );
        assert_eq!(env.ops.0.get("DISPLAY"), Some(&OsString::from(":0")));
        assert_eq!(
            env.ops.0.get("XDG_SESSION_TYPE"),
            Some(&OsString::from("wayland"))
        );
        assert_eq!(env.ops.0.get("XDG_SESSION_ID"), Some(&OsString::from("2")));
        assert_eq!(
            env.ops.0.get("XDG_CURRENT_DESKTOP"),
            Some(&OsString::from("GNOME"))
        );
        assert_eq!(env.ops.0.get("UNMANAGED"), Some(&OsString::from("keep")));
    }

    #[test]
    fn apply_removes_unset_managed_keys_from_the_previous_session() {
        let mut env = managed();
        env.apply(&sample_env());
        let mut x11 = sample_env();
        x11.session_type = SessionType::X11;
        x11.wayland_display = None;
        x11.x11_display = Some(":1".to_owned());
        env.apply(&x11);
        assert!(!env.ops.0.contains_key("WAYLAND_DISPLAY"));
        assert_eq!(env.ops.0.get("DISPLAY"), Some(&OsString::from(":1")));
        assert_eq!(
            env.ops.0.get("XDG_SESSION_TYPE"),
            Some(&OsString::from("x11"))
        );
    }

    #[test]
    fn clear_restores_the_pre_apply_values() {
        let mut env = managed();
        env.apply(&sample_env());
        env.clear();
        assert_eq!(env.ops.0.get("DISPLAY"), Some(&OsString::from(":9")));
        assert!(!env.ops.0.contains_key("WAYLAND_DISPLAY"));
        assert!(!env.ops.0.contains_key("XDG_SESSION_TYPE"));
        assert_eq!(env.ops.0.get("UNMANAGED"), Some(&OsString::from("keep")));
        assert!(env.current.is_none());
    }

    #[test]
    fn apply_then_clear_is_idempotent_and_captures_one_baseline() {
        let mut env = managed();
        env.apply(&sample_env());
        env.apply(&sample_env());
        env.clear();
        env.clear();
        assert_eq!(env.ops.0.get("DISPLAY"), Some(&OsString::from(":9")));
        assert!(env.baseline.is_some());
    }

    #[test]
    fn reconcile_skips_writes_when_the_session_is_unchanged() {
        let mut env = managed();
        let session = sample_env();
        env.apply(&session);
        let before = env.ops.0.clone();
        env.reconcile(Some(&session));
        assert_eq!(env.ops.0, before);
    }

    #[tokio::test]
    async fn wait_for_available_returns_immediately_when_available() {
        let (tx, mut rx) = watch::channel(SessionState::Available(sample_env()));
        let result = wait_for_session_rx(&mut rx, Duration::from_millis(50)).await;
        assert!(result.is_ok());
        drop(tx);
    }

    #[tokio::test]
    async fn wait_for_available_returns_when_published() {
        let (tx, mut rx) = watch::channel(SessionState::Unknown);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = tx.send(SessionState::Available(sample_env()));
        });
        let result = wait_for_session_rx(&mut rx, Duration::from_secs(2)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_for_available_times_out_with_waiting_message() {
        let (_tx, mut rx) = watch::channel(SessionState::NoSession);
        let error = wait_for_session_rx(&mut rx, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(error, WAITING);
    }

    #[tokio::test]
    #[ignore = "requires systemd-logind and an active graphical session"]
    async fn real_logind_reports_a_graphical_session() {
        match detect().await {
            SessionState::Available(env) => {
                assert!(!env.session_id.is_empty() || env.source == SessionEnvSource::ProcessEnv);
                assert!(env.capture_display().is_some());
            }
            SessionState::NoSession => eprintln!("no graphical session on this machine"),
            SessionState::Unavailable(error) => panic!("logind unavailable: {error}"),
            SessionState::Unknown => unreachable!("one-shot detect never returns Unknown"),
        }
    }
}
