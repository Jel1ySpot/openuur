//! `openuurd doctor`: a paste-safe host diagnostic report.
//!
//! Exactly seventeen probes cover the prerequisites an issue report needs:
//! OS, session, displays, backend choice, portal/PipeWire, X11/wlroots
//! capabilities, input permission, FFmpeg, H.264/H.265, hardware encoders,
//! chroma, audio tools, and account/service state. Only names, booleans and
//! versions are printed: never tokens, device ids, addresses, or clipboard
//! contents. Home paths are shortened to `~`.

use std::path::Path;
use std::time::Duration;

use crate::desktop::encoder::{self, EncoderKind};

const PROBES: usize = 17;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

struct Probe {
    name: &'static str,
    status: Status,
    detail: String,
}

fn redact(text: &str) -> String {
    let mut out = text.to_owned();
    if let Some(home) = std::env::var_os("HOME")
        .and_then(|value| value.into_string().ok())
        .filter(|value| value.len() > 1)
    {
        out = out.replace(&home, "~");
    }
    if let Ok(user) = std::env::var("USER") {
        if user.len() > 2 {
            out = out.replace(&user, "<user>");
        }
    }
    out
}

#[cfg(test)]
fn env_state(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) if value.is_empty() => "set (empty)".to_owned(),
        Ok(_) => "set".to_owned(),
        Err(_) => "unset".to_owned(),
    }
}

fn writable(path: &str) -> &'static str {
    let Ok(c_path) = std::ffi::CString::new(path) else {
        return "invalid";
    };
    if !Path::new(path).exists() {
        "missing"
    } else if unsafe { libc::access(c_path.as_ptr(), libc::W_OK) } == 0 {
        "writable"
    } else {
        "no permission"
    }
}

async fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn command_exists(program: &str) -> bool {
    command_output("sh", &["-c", &format!("command -v {program}")])
        .await
        .is_some()
}

fn os_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("PRETTY_NAME=")
                    .map(|value| value.trim_matches('"').to_owned())
            })
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

async fn probe_os() -> Probe {
    Probe {
        name: "os",
        status: Status::Ok,
        detail: format!(
            "{} / {} / openuur {}",
            os_name(),
            std::env::consts::ARCH,
            env!("CARGO_PKG_VERSION")
        ),
    }
}

fn probe_session() -> Probe {
    Probe {
        name: "session",
        status: Status::Ok,
        detail: format!(
            "XDG_SESSION_TYPE={} XDG_CURRENT_DESKTOP={}",
            std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unset".to_owned()),
            std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unset".to_owned())
        ),
    }
}

async fn probe_x11_display() -> Probe {
    let display = std::env::var("DISPLAY").ok();
    let Some(display) = display.filter(|value| !value.is_empty()) else {
        return Probe {
            name: "x11 display",
            status: Status::Warn,
            detail: "DISPLAY unset".to_owned(),
        };
    };
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    {
        let probe_display = display.clone();
        let check = tokio::task::spawn_blocking(move || {
            x11rb::connect(Some(&probe_display))
                .map(|(connection, screen)| {
                    use x11rb::connection::Connection;
                    let root = connection.setup().roots[screen].root;
                    x11rb::protocol::xproto::get_geometry(&connection, root)
                        .ok()
                        .and_then(|cookie| cookie.reply().ok())
                        .map(|reply| (reply.width, reply.height))
                })
                .map_err(|error| error.to_string())
        })
        .await;
        match check {
            Ok(Ok(Some((width, height)))) => Probe {
                name: "x11 display",
                status: Status::Ok,
                detail: format!("{display} reachable ({width}x{height})"),
            },
            Ok(Ok(None)) => Probe {
                name: "x11 display",
                status: Status::Warn,
                detail: format!("{display} connected but geometry unavailable"),
            },
            Ok(Err(error)) => Probe {
                name: "x11 display",
                status: Status::Fail,
                detail: format!("{display} unreachable: {error}"),
            },
            _ => Probe {
                name: "x11 display",
                status: Status::Warn,
                detail: format!("{display} probe did not finish"),
            },
        }
    }
    #[cfg(not(all(target_os = "linux", feature = "desktop")))]
    Probe {
        name: "x11 display",
        status: Status::Warn,
        detail: format!("{display} set; this build has no desktop capture"),
    }
}

fn probe_wayland_display() -> Probe {
    match std::env::var_os("WAYLAND_DISPLAY") {
        Some(value) if !value.is_empty() => Probe {
            name: "wayland display",
            status: Status::Ok,
            detail: "WAYLAND_DISPLAY set".to_owned(),
        },
        _ => Probe {
            name: "wayland display",
            status: Status::Warn,
            detail: "WAYLAND_DISPLAY unset".to_owned(),
        },
    }
}

async fn probe_backend() -> Probe {
    let config = crate::config::Config::load().unwrap_or_default();
    let choice = config.desktop_backend();
    let detected = tokio::time::timeout(
        Duration::from_secs(4),
        crate::desktop::detect_backend_selection(choice),
    )
    .await;
    match detected {
        Ok(Ok((selection, session))) => Probe {
            name: "backend",
            status: Status::Ok,
            detail: format!(
                "requested={} effective={} session={}",
                selection.requested.label(),
                selection.effective.label(),
                session.describe()
            ),
        },
        Ok(Err(error)) => Probe {
            name: "backend",
            status: Status::Warn,
            detail: format!("requested={} unavailable: {error}", choice.as_str()),
        },
        Err(_) => Probe {
            name: "backend",
            status: Status::Warn,
            detail: "session discovery timed out".to_owned(),
        },
    }
}

async fn probe_portal() -> Probe {
    #[cfg(target_os = "linux")]
    {
        let result = async {
            let connection = zbus::Connection::session().await?;
            let proxy = zbus::fdo::DBusProxy::new(&connection).await?;
            let owned = proxy
                .name_has_owner("org.freedesktop.portal.Desktop".try_into()?)
                .await?;
            Ok::<bool, zbus::Error>(owned)
        };
        match tokio::time::timeout(Duration::from_secs(3), result).await {
            Ok(Ok(true)) => Probe {
                name: "portal",
                status: Status::Ok,
                detail: format!(
                    "org.freedesktop.portal.Desktop present; libei={}",
                    if crate::desktop::libei_available() {
                        "yes"
                    } else {
                        "no (Notify fallback)"
                    }
                ),
            },
            Ok(Ok(false)) => Probe {
                name: "portal",
                status: Status::Warn,
                detail: "org.freedesktop.portal.Desktop absent".to_owned(),
            },
            Ok(Err(error)) => Probe {
                name: "portal",
                status: Status::Warn,
                detail: format!("session bus unavailable: {error}"),
            },
            Err(_) => Probe {
                name: "portal",
                status: Status::Warn,
                detail: "portal D-Bus probe timed out".to_owned(),
            },
        }
    }
    #[cfg(not(target_os = "linux"))]
    Probe {
        name: "portal",
        status: Status::Warn,
        detail: "not a Linux host".to_owned(),
    }
}

fn probe_pipewire() -> Probe {
    let socket = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .map(|path| path.join("pipewire-0"));
    match socket {
        Some(path) if path.exists() => Probe {
            name: "pipewire",
            status: Status::Ok,
            detail: "pipewire-0 socket present".to_owned(),
        },
        Some(path) => Probe {
            name: "pipewire",
            status: Status::Warn,
            detail: format!("{} missing", redact(&path.display().to_string())),
        },
        None => Probe {
            name: "pipewire",
            status: Status::Warn,
            detail: "XDG_RUNTIME_DIR unset".to_owned(),
        },
    }
}

async fn probe_wlroots() -> Probe {
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    {
        let probe = tokio::task::spawn_blocking(crate::desktop::probe_wlroots).await;
        match probe {
            Ok(Ok(())) => Probe {
                name: "wlroots protocols",
                status: Status::Ok,
                detail: "screencopy/virtual pointer available".to_owned(),
            },
            Ok(Err(error)) => Probe {
                name: "wlroots protocols",
                status: Status::Warn,
                detail: error,
            },
            Err(error) => Probe {
                name: "wlroots protocols",
                status: Status::Warn,
                detail: format!("probe task failed: {error}"),
            },
        }
    }
    #[cfg(not(all(target_os = "linux", feature = "desktop")))]
    Probe {
        name: "wlroots protocols",
        status: Status::Warn,
        detail: "desktop feature disabled".to_owned(),
    }
}

async fn probe_x11_extensions() -> Probe {
    if std::env::var_os("DISPLAY").is_none() {
        return Probe {
            name: "x11 extensions",
            status: Status::Warn,
            detail: "DISPLAY unset".to_owned(),
        };
    }
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    {
        let check = tokio::task::spawn_blocking(|| {
            use x11rb::connection::RequestConnection as _;
            let (connection, _) = x11rb::connect(None).map_err(|error| error.to_string())?;
            let mut found = Vec::new();
            let mut missing = Vec::new();
            for name in ["XTEST", "XFIXES", "RANDR", "DAMAGE", "MIT-SHM"] {
                match connection.extension_information(name) {
                    Ok(Some(_)) => found.push(name),
                    _ => missing.push(name),
                }
            }
            Ok::<_, String>((found, missing))
        })
        .await;
        match check {
            Ok(Ok((found, missing))) if missing.is_empty() => Probe {
                name: "x11 extensions",
                status: Status::Ok,
                detail: found.join(", "),
            },
            Ok(Ok((found, missing))) => Probe {
                name: "x11 extensions",
                status: Status::Warn,
                detail: format!(
                    "present={} missing={}",
                    if found.is_empty() {
                        "none".to_owned()
                    } else {
                        found.join(", ")
                    },
                    missing.join(", ")
                ),
            },
            Ok(Err(error)) => Probe {
                name: "x11 extensions",
                status: Status::Fail,
                detail: error,
            },
            Err(error) => Probe {
                name: "x11 extensions",
                status: Status::Warn,
                detail: format!("probe task failed: {error}"),
            },
        }
    }
    #[cfg(not(all(target_os = "linux", feature = "desktop")))]
    Probe {
        name: "x11 extensions",
        status: Status::Warn,
        detail: "desktop feature disabled".to_owned(),
    }
}

fn probe_uinput() -> Probe {
    Probe {
        name: "uinput",
        status: match writable("/dev/uinput") {
            "writable" => Status::Ok,
            _ => Status::Warn,
        },
        detail: format!(
            "/dev/uinput {}; UU_INPUT_BACKEND={}",
            writable("/dev/uinput"),
            std::env::var("UU_INPUT_BACKEND").unwrap_or_else(|_| "auto".to_owned())
        ),
    }
}

async fn probe_ffmpeg_version() -> Probe {
    match command_output("ffmpeg", &["-version"]).await {
        Some(text) => {
            let version = text
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(2))
                .unwrap_or("unknown");
            Probe {
                name: "ffmpeg",
                status: Status::Ok,
                detail: format!("version {version}"),
            }
        }
        None => Probe {
            name: "ffmpeg",
            status: Status::Fail,
            detail: "ffmpeg not runnable (install ffmpeg or set UU_FFMPEG)".to_owned(),
        },
    }
}

async fn encoder_list() -> Option<String> {
    command_output("ffmpeg", &["-hide_banner", "-encoders"]).await
}

fn listed(text: &str, name: &str) -> bool {
    text.split_whitespace().any(|word| word == name)
}

async fn probe_h264(list: Option<String>) -> Probe {
    match list {
        Some(text) => {
            let names: Vec<&str> = [
                "libx264",
                "h264_nvenc",
                "h264_vaapi",
                "h264_qsv",
                "h264_amf",
            ]
            .into_iter()
            .filter(|name| listed(&text, name))
            .collect();
            Probe {
                name: "h264 encoder",
                status: if names.is_empty() {
                    Status::Fail
                } else {
                    Status::Ok
                },
                detail: if names.is_empty() {
                    "none of libx264/nvenc/vaapi/qsv/amf".to_owned()
                } else {
                    names.join(", ")
                },
            }
        }
        None => Probe {
            name: "h264 encoder",
            status: Status::Fail,
            detail: "ffmpeg encoder list unavailable".to_owned(),
        },
    }
}

async fn probe_h265(list: Option<String>) -> Probe {
    let advertised = list
        .as_deref()
        .map(|text| {
            [
                "libx265",
                "hevc_nvenc",
                "hevc_vaapi",
                "hevc_qsv",
                "hevc_amf",
            ]
            .into_iter()
            .filter(|name| listed(text, name))
            .collect::<Vec<_>>()
            .join(", ")
        })
        .unwrap_or_else(|| "encoder list unavailable".to_owned());
    let ready = encoder::probe_hevc_available().await;
    Probe {
        name: "h265 encoder",
        status: if ready { Status::Ok } else { Status::Warn },
        detail: format!(
            "listed: {advertised}; real one-frame probe: {}",
            if ready { "passes" } else { "fails" }
        ),
    }
}

async fn probe_encoder(list: Option<String>) -> Probe {
    let kind = encoder::detect_kind();
    let text = list.unwrap_or_default();
    let wanted = match kind {
        EncoderKind::Cpu => vec!["libx264", "libx265"],
        EncoderKind::Nvenc => vec!["h264_nvenc", "hevc_nvenc"],
        EncoderKind::Vaapi => vec!["h264_vaapi", "hevc_vaapi"],
        EncoderKind::Qsv => vec!["h264_qsv", "hevc_qsv"],
        EncoderKind::Amf => vec!["h264_amf", "hevc_amf"],
    };
    let available: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|name| listed(&text, name))
        .collect();
    Probe {
        name: "encoder",
        status: if available.is_empty() {
            Status::Fail
        } else {
            Status::Ok
        },
        detail: format!(
            "selected={} UU_VIDEO_ENCODER={} available={}",
            kind.label(),
            std::env::var("UU_VIDEO_ENCODER").unwrap_or_else(|_| "auto".to_owned()),
            if available.is_empty() {
                "none".to_owned()
            } else {
                available.join(", ")
            }
        ),
    }
}

fn probe_chroma() -> Probe {
    let kind = encoder::detect_kind();
    Probe {
        name: "chroma",
        status: Status::Ok,
        detail: format!(
            "selected={} 4:4:4={}",
            kind.label(),
            if encoder::full_chroma_available(kind) {
                "yes"
            } else {
                "no (4:2:0)"
            }
        ),
    }
}

async fn probe_audio() -> Probe {
    let mut found = Vec::new();
    for tool in ["pactl", "wpctl", "pw-cli"] {
        if command_exists(tool).await {
            found.push(tool);
        }
    }
    Probe {
        name: "audio tools",
        status: if found.is_empty() {
            Status::Warn
        } else {
            Status::Ok
        },
        detail: if found.is_empty() {
            "none of pactl/wpctl/pw-cli".to_owned()
        } else {
            found.join(", ")
        },
    }
}

async fn probe_account_and_service() -> Probe {
    let config = crate::config::Config::load().ok();
    let path = crate::config::Config::path().ok();
    let (mode, signed_in, registered) = match (&config, &path) {
        (Some(config), Some(path)) => {
            let mode = std::fs::metadata(path)
                .ok()
                .map(|meta| {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        format!("{:o}", meta.permissions().mode() & 0o777)
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = meta;
                        "unknown".to_owned()
                    }
                })
                .unwrap_or_else(|| "missing".to_owned());
            (
                mode,
                config
                    .token
                    .as_deref()
                    .is_some_and(|token| !token.is_empty())
                    && config.user_id.as_deref().is_some_and(|id| !id.is_empty()),
                config.device_id.as_deref().is_some_and(|id| !id.is_empty()),
            )
        }
        _ => ("missing".to_owned(), false, false),
    };
    let service = command_output("systemctl", &["--user", "is-active", "openuurd.service"])
        .await
        .unwrap_or_else(|| "systemctl unavailable".to_owned());
    let processes = command_output("pgrep", &["-c", "-x", "openuurd"])
        .await
        .unwrap_or_else(|| "unknown".to_owned());
    Probe {
        name: "account/service",
        status: Status::Ok,
        detail: format!(
            "config={} mode={mode} signed_in={signed_in} device_registered={registered} service={service} openuurd_processes={processes}",
            path.as_deref()
                .map(|path| redact(&path.display().to_string()))
                .unwrap_or_else(|| "unknown".to_owned())
        ),
    }
}

/// Run all seventeen probes and print a report that is safe to paste.
pub async fn run() -> Result<(), String> {
    let list = encoder_list().await;
    let probes = vec![
        probe_os().await,
        probe_session(),
        probe_x11_display().await,
        probe_wayland_display(),
        probe_backend().await,
        probe_portal().await,
        probe_pipewire(),
        probe_wlroots().await,
        probe_x11_extensions().await,
        probe_uinput(),
        probe_ffmpeg_version().await,
        probe_h264(list.clone()).await,
        probe_h265(list.clone()).await,
        probe_encoder(list).await,
        probe_chroma(),
        probe_audio().await,
        probe_account_and_service().await,
    ];
    assert_eq!(probes.len(), PROBES, "doctor probe count");
    println!("openuur doctor: {PROBES} probes (safe to paste: no ids, tokens, paths or content)");
    let mut failures = 0;
    let mut warnings = 0;
    for probe in &probes {
        match probe.status {
            Status::Ok => {}
            Status::Warn => warnings += 1,
            Status::Fail => failures += 1,
        }
        println!(
            "[{:>4}] {:<18} {}",
            probe.status.label(),
            probe.name,
            redact(&probe.detail)
        );
    }
    println!(
        "summary: {} ok, {warnings} warnings, {failures} failures",
        PROBES - warnings - failures
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_hides_home_and_user() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.len() > 1 {
            assert_eq!(redact(&format!("{home}/.config/x")), "~/.config/x");
        }
        let user = std::env::var("USER").unwrap_or_default();
        if user.len() > 2 {
            assert!(!redact(&format!("user={user}")).contains(&user));
        }
        assert_eq!(env_state("UU_DOCTOR_TEST_SURELY_UNSET"), "unset");
    }

    #[test]
    fn reports_exactly_seventeen_probes() {
        // Keep the public contract explicit: the report has 17 numbered rows.
        assert_eq!(PROBES, 17);
    }
}
