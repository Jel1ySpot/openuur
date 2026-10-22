//! XFixes cursor tracking.
//!
//! XFixes reports cursor-image changes (not pointer motion) on the connection
//! that selected `DISPLAY_CURSOR`. The thread keeps one image and publishes it
//! when the cursor serial changes, so a later phase can ship the shape over
//! the CONTROL channel without polling the server.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, CursorNotifyMask};
use x11rb::protocol::Event;

use super::super::CursorImage;

const POLL_INTERVAL: Duration = Duration::from_millis(16);

pub(crate) fn start(
    display: Option<String>,
    updates: watch::Sender<Option<CursorImage>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("openuur-x11-cursor".into())
        .spawn(move || {
            if let Err(error) = run(display.as_deref(), &updates, &stop) {
                tracing::debug!("X11 cursor tracking stopped: {error}");
            }
        })
        .map_err(|error| format!("cannot start the X11 cursor thread: {error}"))?;
    Ok(())
}

fn run(
    display: Option<&str>,
    updates: &watch::Sender<Option<CursorImage>>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let (connection, screen_num) =
        x11rb::connect(display).map_err(|error| format!("X11 cursor connection: {error}"))?;
    let root = connection.setup().roots[screen_num].root;
    let version = xfixes::query_version(&connection, 6, 0)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?;
    if version.major_version < 1 {
        return Err("XFixes version 1 or newer is required".to_owned());
    }
    xfixes::select_cursor_input(&connection, root, CursorNotifyMask::DISPLAY_CURSOR)
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| error.to_string())?;
    let _ = connection.flush();

    let mut serial = None;
    // Publish the current shape immediately; the first notify may be far away.
    if let Some(image) = read_cursor(&connection)? {
        serial = Some(image.serial);
        updates.send_replace(Some(image));
    }

    while !stop.load(Ordering::Acquire) {
        while let Ok(Some(event)) = connection.poll_for_event() {
            if let Event::XfixesCursorNotify(event) = event {
                if serial == Some(event.cursor_serial) {
                    continue;
                }
                if let Some(image) = read_cursor(&connection)? {
                    if serial != Some(image.serial) {
                        serial = Some(image.serial);
                        updates.send_replace(Some(image));
                    }
                }
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

fn read_cursor(connection: &impl Connection) -> Result<Option<CursorImage>, String> {
    let reply = xfixes::get_cursor_image(connection)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?;
    if reply.width == 0 || reply.height == 0 {
        return Ok(None);
    }
    Ok(Some(CursorImage {
        serial: reply.cursor_serial,
        width: reply.width,
        height: reply.height,
        xhot: reply.xhot,
        yhot: reply.yhot,
        argb: Arc::new(reply.cursor_image),
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn premultiplied_argb_converts_to_unpremultiplied_rgba() {
        // 50% alpha, red already multiplied by alpha.
        assert_eq!(
            crate::desktop::cursor::argb_to_rgba(0x80_8000_00),
            [255, 0, 0, 128]
        );
        assert_eq!(
            crate::desktop::cursor::argb_to_rgba(0xff_12_34_56),
            [0x12, 0x34, 0x56, 0xff]
        );
        assert_eq!(crate::desktop::cursor::argb_to_rgba(0), [0, 0, 0, 0]);
    }
}
