//! `openuur install`: install and enable `openuurd` under the system's init.
//!
//! Systemd (system and user), OpenRC, runit, s6, SysV init and XDG autostart
//! are supported. System services start at boot without a graphical session:
//! `openuurd` discovers the user's session at runtime, so terminal, file
//! transfer and port mapping work before login while the desktop attaches
//! once the account logs in. The display-manager greeter is never captured.
//!
//! Run as a normal user it installs a **user** service; run through `sudo` it
//! installs a **system** service for the account that invoked sudo (or
//! `--user <account>`). Nothing secret is written; the config file remains the
//! only credential store.

use std::ffi::{CStr, CString};
use std::fmt;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

const UNIT_NAME: &str = "openuurd.service";
const START_SCRIPT: &str = "/etc/openuurd/start";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Init {
    Systemd,
    OpenRC,
    Runit,
    S6,
    Sysv,
    Xdg,
}

impl Init {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::OpenRC => "openrc",
            Self::Runit => "runit",
            Self::S6 => "s6",
            Self::Sysv => "sysv",
            Self::Xdg => "xdg",
        }
    }

    /// Parse an explicit `--init` value. `auto` is handled by [`run`].
    pub fn from_name(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "systemd" => Some(Self::Systemd),
            "openrc" => Some(Self::OpenRC),
            "runit" => Some(Self::Runit),
            "s6" => Some(Self::S6),
            "sysv" | "sysvinit" | "init.d" => Some(Self::Sysv),
            "xdg" | "autostart" => Some(Self::Xdg),
            _ => None,
        }
    }
}

impl fmt::Display for Init {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Init {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_name(value).ok_or_else(|| {
            format!("unknown init {value:?}; use systemd, openrc, runit, s6, sysv or xdg")
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    System,
    User,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
        }
    }

    pub fn from_name(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "system" => Some(Self::System),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Scope {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_name(value).ok_or_else(|| format!("unknown scope {value:?}; use system or user"))
    }
}

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// `openuurd` binary; defaults to the sibling of the running executable or PATH.
    pub binary: Option<PathBuf>,
    /// Host config file; defaults to the target account's config.
    pub config: Option<PathBuf>,
    /// System install only: account that owns the graphical session.
    pub user: Option<String>,
    /// Write and enable the service but do not start it now.
    pub no_start: bool,
    /// `auto` (default), `systemd`, `openrc`, `runit`, `s6`, `sysv` or `xdg`.
    pub init: Option<String>,
    /// `auto` (default), `system` or `user`.
    pub scope: Option<String>,
}

#[derive(Debug)]
pub struct InstallReport {
    pub init: Init,
    pub scope: Scope,
    /// The init service file (or XDG desktop entry).
    pub unit_path: PathBuf,
    pub binary: PathBuf,
    pub config: PathBuf,
    pub user: Option<String>,
    pub enabled: bool,
    pub started: bool,
    pub warnings: Vec<String>,
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn user_info(name: &str) -> Option<(u32, PathBuf)> {
    let name = CString::new(name).ok()?;
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    let uid = unsafe { (*entry).pw_uid };
    let home = unsafe { CStr::from_ptr((*entry).pw_dir) }
        .to_str()
        .ok()
        .map(PathBuf::from)?;
    Some((uid, home))
}

fn current_user_name() -> Option<String> {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .or_else(|| {
            let entry = unsafe { libc::getpwuid(libc::geteuid()) };
            if entry.is_null() {
                None
            } else {
                unsafe { CStr::from_ptr((*entry).pw_name) }
                    .to_str()
                    .ok()
                    .map(str::to_owned)
            }
        })
}

fn find_binary(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        let path = path
            .canonicalize()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if !path.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        return Ok(path);
    }
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            let sibling = directory.join("openuurd");
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
    }
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("openuurd");
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
    }
    candidates.push(PathBuf::from("/usr/local/bin/openuurd"));
    candidates.push(PathBuf::from("/usr/bin/openuurd"));
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| candidate.canonicalize().ok())
        .ok_or_else(|| {
            "cannot find openuurd; build it first or pass --binary /path/to/openuurd".to_owned()
        })
}

/// The config file the `openuur login` command would use for this account.
///
/// For the invoking account this is exactly `Config::path()`, which honours
/// `OPENUUR_CONFIG`, `XDG_STATE_HOME` and the legacy config location. For a
/// different account only its home is known, so mirror the same precedence
/// without that account's environment.
fn default_config(uid: u32, home: &Path) -> Result<PathBuf, String> {
    if uid == unsafe { libc::geteuid() } {
        if let Ok(path) = crate::config::Config::path() {
            return Ok(path);
        }
    }
    let state = home.join(".local/state/openuur/config.toml");
    let legacy = home.join(".config/openuur/config.toml");
    if !state.exists() && legacy.exists() {
        return Ok(legacy);
    }
    Ok(state)
}

/// Detect the running init from `/proc/1/comm` plus well-known runtime paths.
fn detect_init_with(comm: &str, exists: &dyn Fn(&Path) -> bool) -> Init {
    let comm = comm.trim();
    if comm == "systemd" || exists(Path::new("/run/systemd/system")) {
        return Init::Systemd;
    }
    if comm.contains("openrc") || exists(Path::new("/run/openrc")) {
        return Init::OpenRC;
    }
    if comm == "runit" || comm == "runsvdir" || exists(Path::new("/run/runit")) {
        return Init::Runit;
    }
    if comm.starts_with("s6") || exists(Path::new("/run/s6")) || exists(Path::new("/run/s6-rc")) {
        return Init::S6;
    }
    if (comm == "init" || comm == "sysvinit" || comm == "busybox")
        && exists(Path::new("/etc/inittab"))
    {
        return Init::Sysv;
    }
    Init::Xdg
}

fn detect_init() -> Init {
    let comm = std::fs::read_to_string("/proc/1/comm").unwrap_or_default();
    detect_init_with(&comm, &|path| path.exists())
}

fn resolve_scope(requested: Option<Scope>) -> Scope {
    requested.unwrap_or(if is_root() {
        Scope::System
    } else {
        Scope::User
    })
}

fn scope_arg(value: Option<&str>) -> Result<Scope, String> {
    match value {
        Some(value) if value.trim().eq_ignore_ascii_case("auto") => Ok(resolve_scope(None)),
        Some(value) => Scope::from_name(value)
            .ok_or_else(|| format!("unknown scope {value:?}; use auto, system or user")),
        None => Ok(resolve_scope(None)),
    }
}

/// Systemd has per-user units; other inits fall back to XDG autostart when the
/// scope was chosen automatically, and reject an explicit per-user request.
fn resolve_init(
    init: Init,
    scope: Scope,
    automatic: bool,
    warnings: &mut Vec<String>,
) -> Result<Init, String> {
    match (init, scope) {
        (Init::Xdg, Scope::System) => Err(
            "the XDG autostart entry only applies to a user session; use --scope user".to_owned(),
        ),
        (Init::Systemd | Init::Xdg, _) => Ok(init),
        (init, Scope::User) if automatic => {
            warnings.push(format!(
                "{init} has no per-user service; installing an XDG autostart entry instead"
            ));
            Ok(Init::Xdg)
        }
        (init, Scope::User) => Err(format!(
            "{init} has no per-user service; use --init xdg or --init systemd, or install a system service with sudo"
        )),
        (init, Scope::System) => Ok(init),
    }
}

/// Quote one argument for a systemd `ExecStart=` line.
fn escape_arg(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "/._-:+@".contains(character))
    {
        return value.to_owned();
    }
    let mut out = String::from("\"");
    for character in value.chars() {
        match character {
            '"' | '\\' => {
                out.push('\\');
                out.push(character);
            }
            '%' => out.push_str("%%"),
            // systemd expands $VAR even inside ExecStart quotes.
            '$' => out.push_str("$$"),
            _ => out.push(character),
        }
    }
    out.push('"');
    out
}

/// Quote one argument for a POSIX shell.
fn shell_quote(value: &str) -> String {
    let mut out = String::from("'");
    for character in value.chars() {
        if character == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(character);
        }
    }
    out.push('\'');
    out
}

/// Quote one argument for a Desktop Entry `Exec=` line.
fn desktop_quote(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "/._-:+@".contains(character))
    {
        return value.to_owned();
    }
    let mut out = String::from("\"");
    for character in value.chars() {
        match character {
            '"' | '\\' | '`' | '$' => {
                out.push('\\');
                out.push(character);
            }
            // Desktop Entry field codes start with %; a literal % is %%.
            '%' => out.push_str("%%"),
            _ => out.push(character),
        }
    }
    out.push('"');
    out
}

fn command_line(binary: &Path, config: &Path) -> String {
    format!(
        "{} run --config {}",
        escape_arg(&binary.to_string_lossy()),
        escape_arg(&config.to_string_lossy()),
    )
}

fn user_unit(binary: &Path, config: &Path) -> String {
    format!(
        "[Unit]\n\
Description=openuur UU native terminal and desktop host\n\
After=graphical-session.target\n\
PartOf=graphical-session.target\n\
Wants=graphical-session.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={}\n\
Restart=on-failure\n\
RestartSec=3\n\
Environment=RUST_LOG=openuur=info,openuurd=info\n\
Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=%t/bus\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        command_line(binary, config),
    )
}

fn system_unit(binary: &Path, config: &Path, user: &str, uid: u32, home: &Path) -> String {
    format!(
        "[Unit]\n\
Description=openuur UU native host (system service)\n\
After=network-online.target systemd-user-sessions.service\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
User={user}\n\
Environment=HOME={}\n\
Environment=XDG_RUNTIME_DIR=/run/user/{uid}\n\
Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus\n\
Environment=RUST_LOG=openuur=info,openuurd=info\n\
ExecStart={}\n\
Restart=on-failure\n\
RestartSec=3\n\
PrivateTmp=true\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n",
        escape_arg(&home.to_string_lossy()),
        command_line(binary, config),
    )
}

/// Shared launcher for the inits that cannot quote command arguments safely.
/// It exports the target account's session-independent environment and execs
/// the daemon; the init itself drops privileges.
fn start_script(binary: &Path, config: &Path, uid: u32, home: &Path) -> String {
    format!(
        "#!/bin/sh\n\
export HOME={}\n\
export XDG_RUNTIME_DIR={}\n\
export DBUS_SESSION_BUS_ADDRESS={}\n\
export RUST_LOG='openuur=info,openuurd=info'\n\
exec {} run --config {}\n",
        shell_quote(&home.to_string_lossy()),
        shell_quote(&format!("/run/user/{uid}")),
        shell_quote(&format!("unix:path=/run/user/{uid}/bus")),
        shell_quote(&binary.to_string_lossy()),
        shell_quote(&config.to_string_lossy()),
    )
}

fn openrc_script(user: &str) -> String {
    format!(
        "#!/sbin/openrc-run\n\
description=\"openuur UU native host\"\n\
command=\"{START_SCRIPT}\"\n\
command_user={}\n\
command_background=yes\n\
pidfile=\"/run/${{RC_SVCNAME}}.pid\"\n\
# Do not set output_log/error_log: start-stop-daemon drops privileges before\n\
# opening them, so an unprivileged command_user cannot create /var/log files.\n\
\n\
depend() {{\n\
    need net\n\
    after dbus\n\
}}\n",
        shell_quote(user),
    )
}

fn runit_run(user: &str) -> String {
    // chpst -u drops supplementary groups (unlike systemd/s6-setuidgid);
    // `su` applies the account's full group list. runsv runs the script in
    // its own process group, so TERM still reaches the daemon.
    format!(
        "#!/bin/sh\n\
exec 2>&1\n\
exec su -s /bin/sh -c 'exec {START_SCRIPT}' {}\n",
        shell_quote(user),
    )
}

fn s6_run(user: &str) -> String {
    format!(
        "#!/bin/sh\n\
exec 2>&1\n\
exec s6-setuidgid {} {START_SCRIPT}\n",
        shell_quote(user),
    )
}

const S6_TYPE: &str = "longrun\n";

fn sysv_script(user: &str) -> String {
    format!(
        "#!/bin/sh\n\
### BEGIN INIT INFO\n\
# Provides:          openuurd\n\
# Required-Start:    $network $remote_fs $syslog\n\
# Required-Stop:     $network $remote_fs $syslog\n\
# Default-Start:     2 3 4 5\n\
# Default-Stop:      0 1 6\n\
# Short-Description: openuur UU native host\n\
### END INIT INFO\n\
\n\
PIDFILE=/run/openuurd.pid\n\
\n\
case \"$1\" in\n\
    start)\n\
        start-stop-daemon --start --quiet --background --make-pidfile \\\n\
            --pidfile \"$PIDFILE\" --chuid {} --startas {START_SCRIPT}\n\
        ;;\n\
    stop)\n\
        start-stop-daemon --stop --quiet --retry TERM/10/KILL/5 --pidfile \"$PIDFILE\"\n\
        rm -f \"$PIDFILE\"\n\
        ;;\n\
    restart)\n\
        \"$0\" stop\n\
        \"$0\" start\n\
        ;;\n\
    status)\n\
        start-stop-daemon --status --quiet --pidfile \"$PIDFILE\"\n\
        ;;\n\
    *)\n\
        echo \"Usage: $0 {{start|stop|restart|status}}\"\n\
        exit 2\n\
        ;;\n\
esac\n",
        shell_quote(user),
    )
}

fn xdg_desktop(binary: &Path, config: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
Type=Application\n\
Name=openuur\n\
Comment=UU Remote native host\n\
Exec={} run --config {}\n\
Terminal=false\n\
X-GNOME-Autostart-enabled=true\n",
        desktop_quote(&binary.to_string_lossy()),
        desktop_quote(&config.to_string_lossy()),
    )
}

struct Layout {
    root: PathBuf,
    user_config: PathBuf,
    user_home: PathBuf,
}

impl Layout {
    fn host(user_home: PathBuf, user_config: PathBuf) -> Self {
        Self {
            root: PathBuf::from("/"),
            user_config,
            user_home,
        }
    }

    fn system(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    fn unit_path(&self, init: Init, scope: Scope) -> PathBuf {
        match (init, scope) {
            (Init::Systemd, Scope::System) => self.system("/etc/systemd/system").join(UNIT_NAME),
            (Init::Systemd, Scope::User) => self.user_config.join("systemd/user").join(UNIT_NAME),
            (Init::OpenRC, _) => self.system("/etc/init.d/openuurd"),
            (Init::Runit, _) => self.system("/etc/sv/openuurd/run"),
            (Init::S6, _) => self.system("/etc/s6/sv/openuurd/run"),
            (Init::Sysv, _) => self.system("/etc/init.d/openuurd"),
            (Init::Xdg, _) => self.user_config.join("autostart/openuurd.desktop"),
        }
    }
}

fn write_file(path: &Path, contents: &str, mode: u32) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "service path has no parent".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|error| format!("write {}: {error}", temporary.display()))?;
    file.write_all(contents.as_bytes())
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    let mut permissions = std::fs::metadata(&temporary)
        .map_err(|error| error.to_string())?
        .permissions();
    permissions.set_mode(mode);
    std::fs::set_permissions(&temporary, permissions).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("replace {}: {error}", path.display()))
}

type Runner<'a> = dyn Fn(&str, &[&str]) -> Result<(), String> + 'a;

fn execute(runner: &Runner<'_>, program: &str, args: &[&str], warnings: &mut Vec<String>) -> bool {
    match runner(program, args) {
        Ok(()) => true,
        Err(error) => {
            warnings.push(error);
            false
        }
    }
}

fn real_runner(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("{program} unavailable: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "{program} {} failed: {}",
        args.join(" "),
        message.lines().next().unwrap_or("unknown error")
    ))
}

fn command_exists(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
    })
}

/// Service files from a previous install under a different init.
fn other_init_artifacts(layout: &Layout, chosen: &Path) -> Vec<PathBuf> {
    [
        layout.system("/etc/systemd/system/openuurd.service"),
        layout.user_config.join("systemd/user/openuurd.service"),
        layout.system("/etc/init.d/openuurd"),
        layout.system("/etc/sv/openuurd"),
        layout.system("/etc/s6/sv/openuurd"),
        layout.user_config.join("autostart/openuurd.desktop"),
        layout.system("/etc/xdg/autostart/openuurd.desktop"),
    ]
    .into_iter()
    .filter(|path| path.exists() && path != chosen && !chosen.starts_with(path))
    .collect()
}

/// Create (or refresh) a supervision symlink for runit/s6.
fn link_service(layout: &Layout, source: &Path, scan_dir: &str) -> Result<PathBuf, String> {
    let scan = layout.system(scan_dir);
    std::fs::create_dir_all(&scan)
        .map_err(|error| format!("create {}: {error}", scan.display()))?;
    let target = scan.join("openuurd");
    if target.symlink_metadata().is_ok() {
        if target.read_link().ok() == Some(source.to_path_buf()) {
            return Ok(target);
        }
        std::fs::remove_file(&target)
            .map_err(|error| format!("replace {}: {error}", target.display()))?;
    }
    std::os::unix::fs::symlink(source, &target)
        .map_err(|error| format!("link {}: {error}", target.display()))?;
    Ok(target)
}

fn first_existing(layout: &Layout, candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|candidate| layout.system(candidate).is_dir())
}

fn enable_service(
    init: Init,
    scope: Scope,
    layout: &Layout,
    user: &str,
    no_start: bool,
    runner: &Runner<'_>,
    warnings: &mut Vec<String>,
) -> (bool, bool) {
    match init {
        Init::Systemd => {
            let mut base: Vec<&str> = Vec::new();
            if scope == Scope::User {
                base.push("--user");
            }
            let mut call = |args: &[&str]| {
                let mut all = base.clone();
                all.extend_from_slice(args);
                let refs: Vec<&str> = all.iter().map(|arg| *arg).collect();
                execute(runner, "systemctl", &refs, warnings)
            };
            let mut enabled = call(&["daemon-reload"]);
            enabled &= call(&["enable", UNIT_NAME]);
            let mut started = false;
            if enabled && !no_start {
                started = call(&["start", UNIT_NAME]);
            }
            (enabled, started)
        }
        Init::OpenRC => {
            let enabled = execute(
                runner,
                "rc-update",
                &["add", "openuurd", "default"],
                warnings,
            );
            let started = enabled
                && !no_start
                && execute(runner, "rc-service", &["openuurd", "start"], warnings);
            (enabled, started)
        }
        Init::Runit => {
            let scan = first_existing(
                layout,
                &["/run/service", "/var/service", "/etc/service", "/service"],
            )
            .unwrap_or("/var/service");
            let source = layout.system("/etc/sv/openuurd");
            let enabled = match link_service(layout, &source, scan) {
                Ok(_) => true,
                Err(error) => {
                    warnings.push(error);
                    false
                }
            };
            if enabled && no_start && layout.root == Path::new("/") {
                warnings.push(
                    "runit may start openuurd as soon as it is linked into the scan directory"
                        .to_owned(),
                );
            }
            let svdir = format!("SVDIR={scan}");
            let started = enabled
                && !no_start
                && (execute(
                    runner,
                    "env",
                    &[svdir.as_str(), "sv", "up", "openuurd"],
                    warnings,
                ) || execute(
                    runner,
                    "env",
                    &[svdir.as_str(), "sv", "start", "openuurd"],
                    warnings,
                ));
            (enabled, started)
        }
        Init::S6 => {
            if command_exists("s6-rc") && layout.system("/etc/s6/rc").is_dir() {
                warnings.push(
                    "s6-rc controls this system: add /etc/s6/sv/openuurd to a source directory and rebuild the database with s6-rc-compile/s6-rc-update"
                        .to_owned(),
                );
                return (false, false);
            }
            let scan = first_existing(
                layout,
                &["/run/service", "/var/service", "/etc/service", "/service"],
            )
            .unwrap_or("/var/service");
            let source = layout.system("/etc/s6/sv/openuurd");
            let enabled = match link_service(layout, &source, scan) {
                Ok(_) => true,
                Err(error) => {
                    warnings.push(error);
                    false
                }
            };
            if enabled && no_start && layout.root == Path::new("/") {
                warnings.push(
                    "s6 may start openuurd as soon as the service is linked into the scan directory"
                        .to_owned(),
                );
            }
            let started =
                enabled && !no_start && execute(runner, "s6-svscanctl", &["-a", scan], warnings);
            (enabled, started)
        }
        Init::Sysv => {
            let enabled = execute(runner, "update-rc.d", &["openuurd", "defaults"], warnings)
                || execute(runner, "chkconfig", &["--add", "openuurd"], warnings);
            let started = enabled
                && !no_start
                && execute(runner, "service", &["openuurd", "start"], warnings);
            (enabled, started)
        }
        Init::Xdg => {
            warnings.push(format!(
                "the XDG autostart entry starts at the next login for {user}; no service was started now"
            ));
            (true, false)
        }
    }
}

fn install_with(
    options: &InstallOptions,
    init: Init,
    scope: Scope,
    layout: &Layout,
    target: Option<(&str, u32, &Path)>,
    runner: &Runner<'_>,
    warnings: &mut Vec<String>,
) -> Result<InstallReport, String> {
    let binary = find_binary(options.binary.as_deref())?;
    let config_home = target.map(|(_, _, home)| home).unwrap_or(&layout.user_home);
    let config_uid = target
        .map(|(_, uid, _)| uid)
        .unwrap_or_else(|| unsafe { libc::geteuid() });
    let config = match &options.config {
        // A relative path would be resolved against the service's working
        // directory, not the user's shell.
        Some(path) => std::path::absolute(path).unwrap_or_else(|_| path.clone()),
        None => default_config(config_uid, config_home)?,
    };
    if !config.is_file() {
        warnings.push(format!(
            "{} does not exist yet; run `openuur login` before the service starts",
            config.display()
        ));
    }

    let unit_path = layout.unit_path(init, scope);
    for other in other_init_artifacts(layout, &unit_path) {
        warnings.push(format!(
            "another init's openuurd service already exists at {}; it may start a second daemon",
            other.display()
        ));
    }
    let (user, uid, home) = match target {
        Some((user, uid, home)) => (user.to_owned(), uid, home.to_path_buf()),
        None => (
            current_user_name().unwrap_or_else(|| "root".to_owned()),
            unsafe { libc::geteuid() },
            layout.user_home.clone(),
        ),
    };

    // The shared launcher only exists for the inits whose service files cannot
    // quote arbitrary argument paths.
    if matches!(init, Init::OpenRC | Init::Runit | Init::S6 | Init::Sysv) {
        let script = layout.system(START_SCRIPT);
        write_file(&script, &start_script(&binary, &config, uid, &home), 0o755)?;
    }

    match init {
        Init::Systemd => {
            let contents = if scope == Scope::System {
                system_unit(&binary, &config, &user, uid, &home)
            } else {
                user_unit(&binary, &config)
            };
            write_file(&unit_path, &contents, 0o644)?;
        }
        Init::OpenRC => write_file(&unit_path, &openrc_script(&user), 0o755)?,
        Init::Runit => write_file(&unit_path, &runit_run(&user), 0o755)?,
        Init::S6 => {
            write_file(&unit_path, &s6_run(&user), 0o755)?;
            write_file(&unit_path.with_file_name("type"), S6_TYPE, 0o644)?;
        }
        Init::Sysv => write_file(&unit_path, &sysv_script(&user), 0o755)?,
        Init::Xdg => write_file(&unit_path, &xdg_desktop(&binary, &config), 0o644)?,
    }

    let (enabled, started) = enable_service(
        init,
        scope,
        layout,
        &user,
        options.no_start,
        runner,
        warnings,
    );
    if scope == Scope::User && init == Init::Systemd && !options.no_start {
        warnings.push(format!(
            "tip: `loginctl enable-linger {user}` keeps the user service running after logout"
        ));
    }

    Ok(InstallReport {
        init,
        scope,
        unit_path,
        binary,
        config,
        user: (scope == Scope::System).then_some(user),
        enabled,
        started,
        warnings: std::mem::take(warnings),
    })
}

/// Install, enable and (unless `no_start`) start the service.
pub fn run(options: InstallOptions) -> Result<InstallReport, String> {
    let scope = scope_arg(options.scope.as_deref())?;
    let init = match options.init.as_deref() {
        Some(value) if value.trim().eq_ignore_ascii_case("auto") => detect_init(),
        Some(value) => Init::from_name(value).ok_or_else(|| {
            format!("unknown init {value:?}; use auto, systemd, openrc, runit, s6, sysv or xdg")
        })?,
        None => detect_init(),
    };
    if scope == Scope::User && is_root() {
        return Err(
            "run user-scope installs as the target account; --scope user does not use sudo"
                .to_owned(),
        );
    }
    if scope == Scope::System && !is_root() {
        return Err("system installs need root; run through sudo or pass --scope user".to_owned());
    }

    let automatic = options
        .init
        .as_deref()
        .is_none_or(|value| value.trim().eq_ignore_ascii_case("auto"));
    if automatic && init == Init::Xdg && scope == Scope::System {
        return Err(
            "cannot determine the init system; pass --init systemd, openrc, runit, s6 or sysv"
                .to_owned(),
        );
    }
    let mut warnings = Vec::new();
    let init = resolve_init(init, scope, automatic, &mut warnings)?;
    let (user, uid, home) = if scope == Scope::System {
        let user = options
            .user
            .clone()
            .or_else(|| {
                std::env::var("SUDO_USER")
                    .ok()
                    .filter(|user| !user.is_empty() && user != "root")
            })
            .ok_or_else(|| {
                "system install needs the graphical-session account; run through sudo or pass --user <account>"
                    .to_owned()
            })?;
        let (uid, home) = user_info(&user).ok_or_else(|| format!("unknown account {user:?}"))?;
        (user, uid, home)
    } else {
        let user = current_user_name().unwrap_or_else(|| "root".to_owned());
        let uid = unsafe { libc::geteuid() };
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        (user, uid, home)
    };
    let user_config = if scope == Scope::System {
        home.join(".config")
    } else {
        dirs::config_dir().unwrap_or_else(|| home.join(".config"))
    };
    let layout = Layout::host(home.clone(), user_config);
    let target = (scope == Scope::System).then_some((user.as_str(), uid, home.as_path()));

    install_with(
        &options,
        init,
        scope,
        &layout,
        target,
        &real_runner,
        &mut warnings,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn exists(known: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |path| known.contains(&path.to_string_lossy().as_ref())
    }

    #[test]
    fn init_detection_matrix() {
        assert_eq!(detect_init_with("systemd", &exists(&[])), Init::Systemd);
        assert_eq!(
            detect_init_with("init", &exists(&["/run/systemd/system"])),
            Init::Systemd
        );
        assert_eq!(detect_init_with("openrc-init", &exists(&[])), Init::OpenRC);
        assert_eq!(detect_init_with("runit", &exists(&[])), Init::Runit);
        assert_eq!(detect_init_with("runsvdir", &exists(&[])), Init::Runit);
        assert_eq!(detect_init_with("s6-svscan", &exists(&[])), Init::S6);
        assert_eq!(detect_init_with("s6-linux-init", &exists(&[])), Init::S6);
        assert_eq!(
            detect_init_with("init", &exists(&["/etc/inittab"])),
            Init::Sysv
        );
        assert_eq!(detect_init_with("init", &exists(&[])), Init::Xdg);
        // systemd wins over a leftover inittab.
        assert_eq!(
            detect_init_with("systemd", &exists(&["/etc/inittab"])),
            Init::Systemd
        );
    }

    #[test]
    fn scope_argument_accepts_auto_system_and_user() {
        assert_eq!(scope_arg(Some("system")).unwrap(), Scope::System);
        assert_eq!(scope_arg(Some("USER")).unwrap(), Scope::User);
        assert_eq!(scope_arg(Some(" auto ")).unwrap(), resolve_scope(None));
        assert!(scope_arg(Some("bogus")).is_err());
    }

    #[test]
    fn user_scope_falls_back_to_xdg_for_non_systemd_inits() {
        let mut warnings = Vec::new();
        assert_eq!(
            resolve_init(Init::OpenRC, Scope::User, true, &mut warnings).unwrap(),
            Init::Xdg
        );
        assert_eq!(warnings.len(), 1);
        assert!(resolve_init(Init::Xdg, Scope::System, true, &mut warnings).is_err());
        assert!(resolve_init(Init::OpenRC, Scope::User, false, &mut warnings).is_err());
        assert_eq!(
            resolve_init(Init::Systemd, Scope::User, true, &mut warnings).unwrap(),
            Init::Systemd
        );
    }

    #[test]
    fn systemd_units_do_not_snapshot_the_session_type() {
        let unit = user_unit(
            Path::new("/home/u/.local/bin/openuurd"),
            Path::new("/home/u/.local/state/openuur/config.toml"),
        );
        assert!(unit.contains("After=graphical-session.target"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit
            .contains("ExecStart=/home/u/.local/bin/openuurd run --config /home/u/.local/state/openuur/config.toml"));
        assert!(unit.contains("DBUS_SESSION_BUS_ADDRESS=unix:path=%t/bus"));
        assert!(!unit.contains("XDG_SESSION_TYPE"));
        assert!(!unit.contains("WAYLAND_DISPLAY"));

        let unit = system_unit(
            Path::new("/usr/local/bin/openuurd"),
            Path::new("/home/u/.local/state/openuur/config.toml"),
            "u",
            1000,
            Path::new("/home/u"),
        );
        assert!(unit.contains("User=u"));
        assert!(unit.contains("Environment=HOME=/home/u"));
        assert!(unit.contains("Environment=XDG_RUNTIME_DIR=/run/user/1000"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        assert!(!unit.contains("XDG_SESSION_TYPE"));
        assert!(!unit.contains("WAYLAND_DISPLAY"));
    }

    #[test]
    fn init_templates_drop_privileges_and_use_the_launcher() {
        assert!(openrc_script("u").contains(&format!("command=\"{START_SCRIPT}\"")));
        assert!(openrc_script("u").contains("command_user='u'"));
        assert!(runit_run("u").contains(&format!("su -s /bin/sh -c 'exec {START_SCRIPT}' 'u'")));
        assert!(s6_run("u").contains(&format!("s6-setuidgid 'u' {START_SCRIPT}")));
        assert!(sysv_script("u").contains("--chuid 'u' --startas /etc/openuurd/start"));
        assert_eq!(S6_TYPE, "longrun\n");
    }

    #[test]
    fn launcher_quotes_paths_with_spaces() {
        let script = start_script(
            Path::new("/opt/open uur/openuurd"),
            Path::new("/home/u/my config/config.toml"),
            1000,
            Path::new("/home/u"),
        );
        assert!(script.contains(
            "exec '/opt/open uur/openuurd' run --config '/home/u/my config/config.toml'"
        ));
        assert!(script.contains("export HOME='/home/u'"));
    }

    #[test]
    fn xdg_desktop_entry_quotes_exec() {
        let entry = xdg_desktop(Path::new("/opt/a b/openuurd"), Path::new("/home/u/c.toml"));
        assert!(entry.contains("Exec=\"/opt/a b/openuurd\" run --config /home/u/c.toml"));
        assert!(entry.contains("X-GNOME-Autostart-enabled=true"));
    }

    #[test]
    fn paths_with_spaces_are_quoted() {
        assert_eq!(escape_arg("/opt/a b/openuurd"), "\"/opt/a b/openuurd\"");
        assert_eq!(escape_arg("/opt/a/openuurd"), "/opt/a/openuurd");
        assert_eq!(escape_arg("/opt/100%/openuurd"), "\"/opt/100%%/openuurd\"");
        assert_eq!(escape_arg("/opt/$x/openuurd"), "\"/opt/$$x/openuurd\"");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(desktop_quote("$a `b` \"c\""), "\"\\$a \\`b\\` \\\"c\\\"\"");
        assert_eq!(desktop_quote("/opt/100%"), "\"/opt/100%%\"");
    }

    #[test]
    fn templates_are_written_into_a_temporary_root() {
        let root = std::env::temp_dir().join(format!(
            "openuur-install-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let binary = root.join("openuurd");
        std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
        let config = root.join("config.toml");
        std::fs::write(&config, b"token = \"x\"\n").unwrap();
        let layout = Layout {
            root: root.clone(),
            user_config: root.join("config"),
            user_home: root.join("home"),
        };
        let calls = RefCell::new(Vec::<String>::new());
        let runner = |program: &str, args: &[&str]| {
            calls
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            Ok(())
        };
        let options = InstallOptions {
            binary: Some(binary.clone()),
            config: Some(config.clone()),
            no_start: true,
            ..Default::default()
        };
        let mut warnings = Vec::new();
        let report = install_with(
            &options,
            Init::Runit,
            Scope::System,
            &layout,
            Some(("u", 1000, Path::new("/home/u"))),
            &runner,
            &mut warnings,
        )
        .unwrap();
        assert!(report.unit_path.starts_with(&root));
        assert!(
            std::fs::read_to_string(layout.system("/etc/sv/openuurd/run"))
                .unwrap()
                .contains("su -s /bin/sh")
        );
        assert!(layout.system("/etc/openuurd/start").is_file());
        assert!(root.join("var/service/openuurd").symlink_metadata().is_ok());
        assert!(calls.borrow().is_empty(), "no_start must not run sv");
        let _ = std::fs::remove_dir_all(&root);
    }
}
