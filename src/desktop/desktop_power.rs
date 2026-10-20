//! GNOME display wake and idle inhibition.
//!
//! Acquiring an inhibitor keeps a private session-bus connection alive for the
//! lifetime of the guard. `wake` clears Mutter's `PowerSaveMode`; it does not
//! unlock the session or change lock-screen policy. Every D-Bus call is
//! bounded so a stalled bus cannot block desktop startup.

use std::time::Duration;

use zbus::zvariant::Value;

const MUTTER_DEST: &str = "org.gnome.Mutter.DisplayConfig";
const MUTTER_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const SESSION_DEST: &str = "org.gnome.SessionManager";
const SESSION_PATH: &str = "/org/gnome/SessionManager";
const SESSION_IFACE: &str = "org.gnome.SessionManager";
const CALL_TIMEOUT: Duration = Duration::from_secs(3);

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

/// Clear the display power-save mode (wake the monitor).
pub async fn wake() -> Result<(), String> {
    if !gnome_active() {
        return Ok(());
    }
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| format!("session bus unavailable: {error}"))?;
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
