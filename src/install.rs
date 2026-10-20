//! `openuur install`: install and enable `openuurd` as a systemd service.
//!
//! Run as a normal user it installs a **user** unit
//! (`~/.config/systemd/user/openuurd.service`). Run through `sudo` it installs
//! a **system** unit (`/etc/systemd/system/openuurd.service`) for the account
//! that invoked sudo (or `--user <account>`).
//!
//! The generated unit points at the `openuurd` binary next to the running
//! `openuur`, and at the invoking account's config file. Nothing secret is
//! written; the config file remains the only credential store.

use std::ffi::{CStr, CString};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT_NAME: &str = "openuurd.service";

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// `openuurd` binary; defaults to the sibling of the running executable or PATH.
    pub binary: Option<PathBuf>,
    /// Host config file; defaults to the target account's config.
    pub config: Option<PathBuf>,
    /// System install only: account that owns the graphical session.
    pub user: Option<String>,
    /// Write and enable the unit but do not start it now.
    pub no_start: bool,
}

#[derive(Debug)]
pub struct InstallReport {
    pub system: bool,
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

fn default_config(user_home: Option<&Path>) -> Result<PathBuf, String> {
    match user_home {
        Some(home) => Ok(home.join(".config/openuur/config.toml")),
        None => crate::config::Config::path().map_err(|error| error.to_string()),
    }
}

/// First Wayland socket in the target account's runtime directory.
fn wayland_display(uid: u32) -> Option<String> {
    let directory = PathBuf::from(format!("/run/user/{uid}"));
    let mut entries: Vec<String> = std::fs::read_dir(directory)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("wayland-") || name.ends_with(".lock") {
                return None;
            }
            entry
                .file_type()
                .ok()
                .is_some_and(|kind| kind.is_socket())
                .then_some(name)
        })
        .collect();
    entries.sort();
    entries.into_iter().next()
}

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
            _ => out.push(character),
        }
    }
    out.push('"');
    out
}

fn user_unit(binary: &Path, config: &Path) -> String {
    format!(
        "[Unit]\n\
Description=openuur UU native terminal and Wayland desktop host\n\
After=graphical-session.target\n\
PartOf=graphical-session.target\n\
Wants=graphical-session.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={} run --config {}\n\
Restart=on-failure\n\
RestartSec=3\n\
Environment=RUST_LOG=openuur=info,openuurd=info\n\
Environment=XDG_SESSION_TYPE=wayland\n\
Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=%t/bus\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        escape_arg(&binary.to_string_lossy()),
        escape_arg(&config.to_string_lossy()),
    )
}

fn system_unit(
    binary: &Path,
    config: &Path,
    user: &str,
    uid: u32,
    wayland: Option<&str>,
) -> String {
    let mut environment = String::new();
    environment.push_str(&format!("Environment=XDG_RUNTIME_DIR=/run/user/{uid}\n"));
    environment.push_str(&format!(
        "Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus\n"
    ));
    environment.push_str("Environment=XDG_SESSION_TYPE=wayland\n");
    if let Some(display) = wayland {
        environment.push_str(&format!("Environment=WAYLAND_DISPLAY={display}\n"));
    }
    format!(
        "[Unit]\n\
Description=openuur UU native host (system service)\n\
After=network-online.target systemd-user-sessions.service\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
User={user}\n\
{environment}\
Environment=RUST_LOG=openuur=info,openuurd=info\n\
ExecStart={} run --config {}\n\
Restart=on-failure\n\
RestartSec=3\n\
PrivateTmp=true\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n",
        escape_arg(&binary.to_string_lossy()),
        escape_arg(&config.to_string_lossy()),
    )
}

fn write_unit(path: &Path, contents: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let parent = path
        .parent()
        .ok_or_else(|| "unit path has no parent".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temporary = path.with_extension("service.tmp");
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
    permissions.set_mode(0o644);
    std::fs::set_permissions(&temporary, permissions).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("replace {}: {error}", path.display()))
}

fn systemctl(system: bool, args: &[&str], warnings: &mut Vec<String>) -> bool {
    let mut command = Command::new("systemctl");
    if !system {
        command.arg("--user");
    }
    command.args(args);
    match command.output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            let message = String::from_utf8_lossy(&output.stderr);
            warnings.push(format!(
                "systemctl {} failed: {}",
                args.join(" "),
                message.lines().next().unwrap_or("unknown error")
            ));
            false
        }
        Err(error) => {
            warnings.push(format!("systemctl unavailable: {error}"));
            false
        }
    }
}

/// Install, enable and (unless `no_start`) start the service.
pub fn run(options: InstallOptions) -> Result<InstallReport, String> {
    let system = is_root();
    let (target_user, user_home) = if system {
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
        (Some((user, uid)), Some(home))
    } else {
        let user = current_user_name();
        // A user install must honour OPENUUR_CONFIG / XDG_CONFIG_HOME.
        (user.map(|user| (user, unsafe { libc::geteuid() })), None)
    };

    let binary = find_binary(options.binary.as_deref())?;
    let config = match options.config {
        Some(path) => path.canonicalize().unwrap_or(path),
        None => default_config(user_home.as_deref())?,
    };
    let unit_path = if system {
        PathBuf::from("/etc/systemd/system").join(UNIT_NAME)
    } else {
        dirs::config_dir()
            .or_else(|| user_home.as_ref().map(|home| home.join(".config")))
            .ok_or_else(|| "cannot determine the user config directory".to_owned())?
            .join("systemd/user")
            .join(UNIT_NAME)
    };

    let mut warnings = Vec::new();
    if !config.is_file() {
        warnings.push(format!(
            "{} does not exist yet; run `openuur login` before starting the service",
            config.display()
        ));
    }

    let contents = if system {
        let (user, uid) = target_user
            .as_ref()
            .expect("system install has a target account");
        let display = wayland_display(*uid);
        if display.is_none() {
            warnings.push(format!(
                "no Wayland socket found in /run/user/{uid}; the host will only serve the terminal until the desktop session is available"
            ));
        }
        system_unit(&binary, &config, user, *uid, display.as_deref())
    } else {
        user_unit(&binary, &config)
    };
    write_unit(&unit_path, &contents)?;

    let mut enabled = systemctl(system, &["daemon-reload"], &mut warnings);
    enabled &= systemctl(system, &["enable", UNIT_NAME], &mut warnings);
    let mut started = false;
    if enabled && !options.no_start {
        started = systemctl(system, &["start", UNIT_NAME], &mut warnings);
    }

    Ok(InstallReport {
        system,
        unit_path,
        binary,
        config,
        user: target_user.map(|(user, _)| user),
        enabled,
        started,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_unit_has_graphical_dependencies_and_escaped_paths() {
        let unit = user_unit(
            Path::new("/home/u/.local/bin/openuurd"),
            Path::new("/home/u/.config/openuur/config.toml"),
        );
        assert!(unit.contains("After=graphical-session.target"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("ExecStart=/home/u/.local/bin/openuurd run --config /home/u/.config/openuur/config.toml"));
        assert!(unit.contains("Environment=DBUS_SESSION_BUS_ADDRESS=unix:path=%t/bus"));
    }

    #[test]
    fn system_unit_sets_the_graphical_account_and_runtime_dir() {
        let unit = system_unit(
            Path::new("/usr/local/bin/openuurd"),
            Path::new("/home/u/.config/openuur/config.toml"),
            "u",
            1000,
            Some("wayland-1"),
        );
        assert!(unit.contains("User=u"));
        assert!(unit.contains("Environment=XDG_RUNTIME_DIR=/run/user/1000"));
        assert!(unit.contains("Environment=WAYLAND_DISPLAY=wayland-1"));
        assert!(unit.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn paths_with_spaces_are_quoted() {
        assert_eq!(escape_arg("/opt/a b/openuurd"), "\"/opt/a b/openuurd\"");
        assert_eq!(escape_arg("/opt/a/openuurd"), "/opt/a/openuurd");
        assert_eq!(escape_arg("/opt/100%/openuurd"), "\"/opt/100%%/openuurd\"");
    }
}
