//! Display wake and GNOME idle inhibition.
//!
//! `wake` is best effort across the session type: Mutter's `PowerSaveMode` on
//! GNOME, the generic ScreenSaver `SimulateUserActivity` call elsewhere, and
//! DPMS plus a zero-motion XTest event on X11. It never unlocks the session or
//! changes lock-screen policy. Every D-Bus and X11 call is bounded so a stalled
//! bus or display server cannot block desktop startup.
//!
//! Acquiring an inhibitor keeps a private session-bus connection alive for the
//! lifetime of the guard.

use std::time::Duration;

use zbus::zvariant::Value;

const MUTTER_DEST: &str = "org.gnome.Mutter.DisplayConfig";
const MUTTER_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const SESSION_DEST: &str = "org.gnome.SessionManager";
const SESSION_PATH: &str = "/org/gnome/SessionManager";
const SESSION_IFACE: &str = "org.gnome.SessionManager";
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// GNOME with a graphical session. Other desktops are left untouched.
pub fn gnome_active() -> bool {
    gnome_desktop() && std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
}

fn gnome_desktop() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("gnome"))
}

/// Wake the display for any supported session type.
pub async fn wake() -> Result<(), String> {
    wake_common(true).await
}

/// Wake the display for the session of a specific capture backend.
pub async fn wake_backend(backend: super::Backend) -> Result<(), String> {
    wake_common(matches!(backend, super::Backend::X11)).await
}

async fn wake_common(allow_x11: bool) -> Result<(), String> {
    let mut first_error = None;
    if gnome_active() {
        if let Err(error) = mutter_wake().await {
            first_error = Some(error);
        }
    }
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
        if let Err(error) = screensaver_wake().await {
            // KDE, Sway, and plain X11 sessions commonly do not export a
            // ScreenSaver service; absence is not a wake failure.
            tracing::debug!("generic screensaver wake unavailable: {error}");
        }
    }
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    if allow_x11 && x11_session() {
        match tokio::time::timeout(
            CALL_TIMEOUT,
            tokio::task::spawn_blocking(|| super::x11::wake_display_blocking(None)),
        )
        .await
        {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => first_error = Some(error),
            Ok(Err(error)) => first_error = Some(format!("X11 wake task failed: {error}")),
            Err(_) => first_error = Some("X11 wake timed out".to_owned()),
        }
    }
    #[cfg(not(all(target_os = "linux", feature = "desktop")))]
    let _ = allow_x11;
    // wlroots output-power management is handled by the native wlroots
    // backend; a portal session only needs Mutter or ScreenSaver activity.
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// A session-bus connection bounded by the same timeout as every call; a bus
/// that accepts the socket but stalls the handshake must not wedge capture
/// startup or portal recovery.
async fn session_connection() -> Result<zbus::Connection, String> {
    match tokio::time::timeout(CALL_TIMEOUT, zbus::Connection::session()).await {
        Ok(Ok(connection)) => Ok(connection),
        Ok(Err(error)) => Err(format!("session bus unavailable: {error}")),
        Err(_) => Err("session bus connection timed out".to_owned()),
    }
}

/// Clear the display power-save mode on GNOME (wake the monitor).
async fn mutter_wake() -> Result<(), String> {
    if !gnome_active() {
        return Ok(());
    }
    let connection = session_connection().await?;
    let value = Value::I32(0);
    let parameters = ("org.gnome.Mutter.DisplayConfig", "PowerSaveMode", value);
    let call = connection.call_method(
        Some(MUTTER_DEST),
        MUTTER_PATH,
        Some("org.freedesktop.DBus.Properties"),
        "Set",
        &parameters,
    );
    tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .map_err(|_| "display wake timed out".to_owned())?
        .map_err(|error| format!("display wake failed: {error}"))?;
    Ok(())
}

/// Ask the desktop screensaver to count simulated user activity.
async fn screensaver_wake() -> Result<(), String> {
    let connection = session_connection().await?;
    let mut last = "no ScreenSaver service".to_owned();
    for (destination, path, interface) in [
        (
            "org.freedesktop.ScreenSaver",
            "/ScreenSaver",
            "org.freedesktop.ScreenSaver",
        ),
        (
            "org.gnome.ScreenSaver",
            "/org/gnome/ScreenSaver",
            "org.gnome.ScreenSaver",
        ),
    ] {
        let call = connection.call_method(
            Some(destination),
            path,
            Some(interface),
            "SimulateUserActivity",
            &(),
        );
        match tokio::time::timeout(CALL_TIMEOUT, call).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(error)) => last = format!("{destination}: {error}"),
            Err(_) => last = format!("{destination}: timed out"),
        }
    }
    Err(last)
}

#[cfg(all(target_os = "linux", feature = "desktop"))]
fn x11_session() -> bool {
    std::env::var_os("DISPLAY").is_some()
        && std::env::var("XDG_SESSION_TYPE").as_deref() != Ok("wayland")
}

/// Blocking variant for `spawn_blocking` callers (auto unlock).
pub fn wake_blocking() -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("display wake runtime: {error}"))?;
    runtime.block_on(wake())
}

/// Idle-only inhibitor held until dropped.
pub struct IdleInhibitor {
    connection: Option<zbus::Connection>,
    cookie: u32,
}

impl IdleInhibitor {
    pub async fn acquire() -> Result<Self, String> {
        if !gnome_active() {
            return Ok(Self {
                connection: None,
                cookie: 0,
            });
        }
        let connection = zbus::Connection::session()
            .await
            .map_err(|error| format!("session bus unavailable: {error}"))?;
        let call = connection.call_method(
            Some(SESSION_DEST),
            SESSION_PATH,
            Some(SESSION_IFACE),
            "Inhibit",
            &("openuur", 0u32, "Remote desktop session is active", 8u32),
        );
        let reply = tokio::time::timeout(CALL_TIMEOUT, call)
            .await
            .map_err(|_| "idle inhibition timed out".to_owned())?
            .map_err(|error| format!("idle inhibition failed: {error}"))?;
        let cookie: u32 = reply
            .body()
            .deserialize()
            .map_err(|error| format!("idle inhibition reply: {error}"))?;
        tracing::info!("GNOME idle inhibitor acquired");
        Ok(Self {
            connection: Some(connection),
            cookie,
        })
    }

    pub fn is_active(&self) -> bool {
        self.connection.is_some()
    }
}

impl Drop for IdleInhibitor {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        let cookie = self.cookie;
        // Drop cannot await. Keep the private connection alive until Uninhibit
        // completes on a short-lived runtime; closing it also revokes the
        // inhibitor if the call fails.
        let _ = std::thread::Builder::new()
            .name("openuur-idle-release".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(runtime) = runtime {
                    runtime.block_on(async {
                        let arguments = (cookie,);
                        let call = connection.call_method(
                            Some(SESSION_DEST),
                            SESSION_PATH,
                            Some(SESSION_IFACE),
                            "Uninhibit",
                            &arguments,
                        );
                        let _ = tokio::time::timeout(CALL_TIMEOUT, call).await;
                    });
                }
                drop(connection);
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_gnome_desktops_are_ignored() {
        let previous = std::env::var("XDG_CURRENT_DESKTOP").ok();
        std::env::set_var("XDG_CURRENT_DESKTOP", "KDE");
        assert!(!gnome_desktop());
        std::env::set_var("XDG_CURRENT_DESKTOP", "ubuntu:GNOME");
        assert!(gnome_desktop());
        match previous {
            Some(value) => std::env::set_var("XDG_CURRENT_DESKTOP", value),
            None => std::env::remove_var("XDG_CURRENT_DESKTOP"),
        }
    }
}
