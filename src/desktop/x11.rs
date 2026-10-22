//! X11 wake support and the placeholder for the future capture backend.
//!
//! Display wake needs only DPMS and XTest, so it is available even though the
//! X11 capture backend is still a stub. X11 capture can later implement the
//! same `DesktopSession`/`InputSink` contract using XShm/XTest.

pub async fn start() -> Result<super::DesktopSession, String> {
    Err("X11 desktop capture is not implemented yet; use Wayland for now".to_owned())
}

/// Force the monitor out of power saving and nudge the desktop with a
/// zero-motion pointer event.
///
/// Both mechanisms count as user activity. A server without DPMS but with
/// XTest (or the reverse) still wakes, which keeps Xvfb and minimal display
/// servers usable.
#[cfg(feature = "desktop")]
pub(crate) fn wake_display_blocking(display: Option<&str>) -> Result<(), String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::dpms::{self, DPMSMode};
    use x11rb::protocol::{xproto, xtest};

    let (connection, screen_num) = x11rb::connect(display).map_err(|error| error.to_string())?;
    let root = connection.setup().roots[screen_num].root;
    let dpms = dpms::force_level(&connection, DPMSMode::ON)
        .map_err(|error| error.to_string())
        .and_then(|cookie| cookie.check().map_err(|error| error.to_string()));
    // detail = 1 means relative motion; (0, 0) is a no-op move that still
    // counts as user activity for screensavers and compositors.
    let xtest = xtest::fake_input(
        &connection,
        xproto::MOTION_NOTIFY_EVENT,
        1,
        x11rb::CURRENT_TIME,
        root,
        0,
        0,
        0,
    )
    .map_err(|error| error.to_string())
    .and_then(|cookie| cookie.check().map_err(|error| error.to_string()));
    connection.flush().map_err(|error| error.to_string())?;
    if dpms.is_err() && xtest.is_err() {
        return Err(format!(
            "display wake failed: DPMS {}, XTest {}",
            dpms.err().unwrap_or_default(),
            xtest.err().unwrap_or_default()
        ));
    }
    Ok(())
}

#[cfg(not(feature = "desktop"))]
pub(crate) fn wake_display_blocking(_display: Option<&str>) -> Result<(), String> {
    Ok(())
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use super::*;
    use x11rb::protocol::dpms::{self, DPMSMode};

    struct Child(std::process::Child);

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    #[ignore = "requires Xvfb with the DPMS and XTEST extensions"]
    fn dpms_wake_turns_the_monitor_on() {
        let display = format!(":{}", 90 + (std::process::id() % 1000));
        let child = std::process::Command::new("Xvfb")
            .args([
                &display,
                "-screen",
                "0",
                "640x480x24",
                "+extension",
                "DPMS",
                "+extension",
                "XTEST",
                "-nolisten",
                "tcp",
            ])
            .spawn()
            .expect("Xvfb");
        let _child = Child(child);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let (connection, _screen) = loop {
            match x11rb::connect(Some(&display)) {
                Ok(connected) => break connected,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(error) => panic!("Xvfb did not start: {error}"),
            }
        };
        let dpms_available = match dpms::force_level(&connection, DPMSMode::OFF) {
            Ok(cookie) => match cookie.check() {
                Ok(()) => true,
                Err(x11rb::errors::ReplyError::ConnectionError(
                    x11rb::errors::ConnectionError::UnsupportedExtension,
                )) => false,
                Err(error) => panic!("DPMS force failed: {error}"),
            },
            Err(x11rb::errors::ConnectionError::UnsupportedExtension) => false,
            Err(error) => panic!("DPMS unavailable: {error}"),
        };
        wake_display_blocking(Some(&display)).expect("display wake");
        if dpms_available {
            let reply = dpms::info(&connection).unwrap().reply().unwrap();
            assert_eq!(reply.power_level, DPMSMode::ON);
        } else {
            // Xvfb builds without DPMS still exercise the XTest path.
            eprintln!("Xvfb has no DPMS extension; verified the XTest wake path only");
        }
    }
}
