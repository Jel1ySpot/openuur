//! Native X11 capture, input and cursor tracking.
//!
//! Capture uses RandR for the monitor inventory, XDamage for change
//! notification and MIT-SHM for the pixel transfer, falling back to core
//! `GetImage` when the extension is missing. Input goes through XTest on its
//! own connection so a slow capture never delays the pointer. Cursor shapes
//! come from XFixes on a third connection.
//!
//! Wayland-adjacent details follow `wayland.rs`: absolute pointer positions
//! are normalized, the session lock and system suspend pause capture and
//! input, and only the active monitor is encoded.

#[cfg(feature = "desktop")]
mod capture;
#[cfg(feature = "desktop")]
mod cursor;
#[cfg(feature = "desktop")]
mod input;

use super::{DesktopOptions, DesktopSession};

#[cfg(feature = "desktop")]
use std::sync::atomic::AtomicBool;
#[cfg(feature = "desktop")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "desktop")]
use tokio::sync::watch;

#[cfg(feature = "desktop")]
use super::{DesktopState, ScreenInfo, SourceKind};

#[cfg(feature = "desktop")]
pub async fn start(options: DesktopOptions) -> Result<DesktopSession, String> {
    start_at(None, options).await
}

#[cfg(not(feature = "desktop"))]
pub async fn start(_options: DesktopOptions) -> Result<DesktopSession, String> {
    Err("X11 desktop capture needs the `desktop` feature".to_owned())
}

/// Everything one connection can learn about the screen layout before the
/// capture threads start.
#[cfg(feature = "desktop")]
pub(crate) struct Probe {
    screens: Vec<ScreenInfo>,
    active: u32,
    shm: bool,
    damage: bool,
    randr: bool,
}

#[cfg(feature = "desktop")]
pub(crate) async fn start_at(
    display: Option<&str>,
    options: DesktopOptions,
) -> Result<DesktopSession, String> {
    start_at_with(display, options, super::session_lock::watch_locked()).await
}

/// Start capture with an explicit lock-state receiver. `start_at` passes the
/// session watcher; tests inject a fixed, unlocked state so a locked host
/// session cannot pause the capture they are exercising.
#[cfg(feature = "desktop")]
pub(crate) async fn start_at_with(
    display: Option<&str>,
    options: DesktopOptions,
    locked: watch::Receiver<Option<bool>>,
) -> Result<DesktopSession, String> {
    if options.source != SourceKind::Monitor {
        return Err(
            "the X11 backend captures a whole monitor; use --desktop-backend portal for \
             window or virtual sources"
                .to_owned(),
        );
    }
    let display = display.map(str::to_owned);
    let explicit_display = display.is_some();
    let probe = {
        let display = display.clone();
        // A stalled display socket must not wedge session start-up. The
        // blocking connect cannot be cancelled, but the caller fails fast.
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || probe(display.as_deref())),
        )
        .await
        .map_err(|_| "X11 server did not answer within 10 s".to_owned())?
        .map_err(|error| format!("X11 probe task failed: {error}"))??
    };
    let (frames_tx, frames) = watch::channel(None);
    let (generation_tx, _) = watch::channel(0u64);
    let state = Arc::new(DesktopState::new(
        probe.screens.clone(),
        probe.active,
        (0, 0, 0),
        frames_tx,
        generation_tx,
    ));
    let (cursor_tx, cursor) = watch::channel(None);
    let stop = Arc::new(AtomicBool::new(false));
    let input = input::start(display.clone(), state.clone(), stop.clone(), locked.clone()).await?;
    capture::start(display.clone(), &probe, state.clone(), stop.clone(), locked).await?;
    if let Err(error) = cursor::start(display, cursor_tx, stop.clone()) {
        // Cursor shapes are a nice-to-have; capture and input still work.
        tracing::warn!("X11 cursor tracking unavailable: {error}");
    }
    // Capture and input threads poll `stop`; this task only gives
    // `DesktopSession::stop` something to abort.
    let task = tokio::spawn(std::future::pending::<()>());
    // An explicit display is a test or alternate server; waking through it is
    // not the same as waking the ambient session.
    if !explicit_display {
        let _ = super::desktop_power::wake().await;
    }
    Ok(DesktopSession {
        backend: super::Backend::X11,
        state,
        frames,
        cursor,
        input,
        stop,
        task,
        wake: None,
        clipboard: Mutex::new(None),
    })
}

/// Probe the display server before starting threads.
#[cfg(feature = "desktop")]
fn probe(display: Option<&str>) -> Result<Probe, String> {
    use x11rb::connection::Connection;
    use x11rb::connection::RequestConnection as _;

    let (connection, screen_num) =
        x11rb::connect(display).map_err(|error| format!("X11 connection failed: {error}"))?;
    let setup = connection.setup();
    let screen = setup
        .roots
        .get(screen_num)
        .ok_or_else(|| "X11 screen index out of range".to_owned())?;
    let root = screen.root;

    let xtest = connection
        .extension_information("XTEST")
        .map_err(|error| error.to_string())?
        .is_some();
    if !xtest {
        return Err("the X11 server has no XTEST extension; remote input is impossible".to_owned());
    }
    if screen.root_depth != 24 && screen.root_depth != 32 {
        return Err(format!(
            "unsupported X11 root depth {}; only 24- and 32-bit TrueColor are supported",
            screen.root_depth
        ));
    }
    let bpp = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == screen.root_depth)
        .map(|format| format.bits_per_pixel)
        .unwrap_or(32);
    if bpp != 32 {
        return Err(format!(
            "unsupported X11 pixel size {bpp}; only 32 bits per pixel is supported"
        ));
    }
    if setup.image_byte_order != x11rb::protocol::xproto::ImageOrder::LSB_FIRST {
        return Err("X11 MSB-first pixel order is not supported".to_owned());
    }
    let Some(visual) = screen
        .allowed_depths
        .iter()
        .flat_map(|depth| depth.visuals.iter())
        .find(|visual| visual.visual_id == screen.root_visual)
    else {
        return Err("the X11 root visual is missing".to_owned());
    };
    if visual.class != x11rb::protocol::xproto::VisualClass::TRUE_COLOR {
        return Err("only TrueColor X11 visuals are supported".to_owned());
    }
    if visual.red_mask != 0x00ff_0000
        || visual.green_mask != 0x0000_ff00
        || visual.blue_mask != 0x0000_00ff
    {
        return Err(format!(
            "unsupported X11 channel masks {:06x}/{:06x}/{:06x}; capture assumes RGB",
            visual.red_mask, visual.green_mask, visual.blue_mask
        ));
    }

    let randr = connection
        .extension_information("RANDR")
        .map_err(|error| error.to_string())?
        .is_some();
    let damage = connection
        .extension_information("DAMAGE")
        .map_err(|error| error.to_string())?
        .is_some();
    let shm = connection
        .extension_information("MIT-SHM")
        .map_err(|error| error.to_string())?
        .is_some();

    let mut screens = Vec::new();
    if randr {
        if let Some(monitors) = randr_monitors(&connection, root) {
            screens = monitors;
        }
    }
    if screens.is_empty() {
        screens.push(ScreenInfo {
            id: 0,
            node_id: 0,
            x: 0,
            y: 0,
            width: u32::from(screen.width_in_pixels),
            height: u32::from(screen.height_in_pixels),
            primary: true,
        });
    }
    let active = screens
        .iter()
        .find(|screen| screen.primary)
        .or_else(|| screens.first())
        .map(|screen| screen.id)
        .unwrap_or(0);
    Ok(Probe {
        screens,
        active,
        shm,
        damage,
        randr,
    })
}

/// RandR monitor rectangles in root coordinates.
#[cfg(feature = "desktop")]
pub(crate) fn randr_monitors(
    connection: &impl x11rb::connection::Connection,
    root: u32,
) -> Option<Vec<ScreenInfo>> {
    use x11rb::protocol::randr;

    let version = randr::query_version(connection, 1, 5).ok()?.reply().ok()?;
    if version.major_version < 1 || (version.major_version == 1 && version.minor_version < 5) {
        return None;
    }
    let reply = randr::get_monitors(connection, root, true)
        .ok()?
        .reply()
        .ok()?;
    let monitors: Vec<ScreenInfo> = reply
        .monitors
        .iter()
        .filter(|monitor| monitor.width > 0 && monitor.height > 0)
        .map(|monitor| ScreenInfo {
            // The atom name is stable across reordering and hotplug, unlike a
            // list index, so `switch_screen` cannot silently retarget.
            id: monitor.name,
            node_id: 0,
            x: i32::from(monitor.x),
            y: i32::from(monitor.y),
            width: u32::from(monitor.width),
            height: u32::from(monitor.height),
            primary: monitor.primary,
        })
        .collect();
    (!monitors.is_empty()).then_some(monitors)
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
#[allow(dead_code)] // desktop_power's X11 wake branch is compiled out too
pub(crate) fn wake_display_blocking(_display: Option<&str>) -> Result<(), String> {
    Ok(())
}

#[cfg(not(feature = "desktop"))]
#[allow(dead_code)]
pub(crate) fn active_window_is_terminal() -> Option<bool> {
    None
}

/// Clamp a normalized coordinate into a screen-local pixel index.
#[cfg(feature = "desktop")]
pub(crate) fn normalized_to_pixel(value: f64, size: u32) -> f64 {
    value.clamp(0.0, 1.0) * f64::from(size.saturating_sub(1))
}

/// Terminal window classes that expect Ctrl+Shift+V for paste.
#[cfg(feature = "desktop")]
const TERMINAL_CLASSES: &[&str] = &[
    "alacritty",
    "console",
    "foot",
    "footclient",
    "ghostty",
    "gnome-terminal",
    "gnome-terminal-server",
    "guake",
    "hyper",
    "kgx",
    "kitty",
    "konsole",
    "lxterminal",
    "ptyxis",
    "qterminal",
    "rxvt",
    "sakura",
    "st",
    "st-256color",
    "tabby",
    "terminator",
    "terminology",
    "termite",
    "tilda",
    "tilix",
    "urxvt",
    "wezterm",
    "warp",
    "x-terminal-emulator",
    "xfce4-terminal",
    "xterm",
    "uxterm",
];

/// Whether the active X11 window is a terminal emulator.
///
/// The first call performs a query and seeds a background refresher; every
/// later call reads the last result without touching the X server. A frozen
/// display therefore slows focus detection at most once instead of blocking
/// the input path. `None` means the focus cannot be observed (no X11
/// display), which callers treat as "not a terminal".
#[cfg(feature = "desktop")]
pub(crate) fn active_window_is_terminal() -> Option<bool> {
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::Once;
    use std::time::Duration;
    use x11rb::connection::Connection as _;

    static STATE: AtomicU8 = AtomicU8::new(0);
    static STARTED: Once = Once::new();

    STARTED.call_once(|| {
        if std::env::var_os("DISPLAY").is_none() {
            return;
        }
        if let Some(value) = query_terminal_focus() {
            STATE.store(if value { 1 } else { 2 }, Ordering::Release);
        }
        let _ = std::thread::Builder::new()
            .name("openuur-focus".into())
            .spawn(|| {
                let mut connection = None;
                loop {
                    if connection.is_none() {
                        connection = x11rb::connect(None).ok();
                    }
                    let value = connection.as_ref().and_then(|(conn, screen)| {
                        let root = conn.setup().roots[*screen].root;
                        terminal_focus_with_connection(conn, root)
                    });
                    STATE.store(
                        match value {
                            Some(true) => 1,
                            Some(false) => 2,
                            None => 0,
                        },
                        Ordering::Release,
                    );
                    if value.is_none() {
                        connection = None;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            });
    });
    match STATE.load(Ordering::Acquire) {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

#[cfg(feature = "desktop")]
fn query_terminal_focus() -> Option<bool> {
    query_terminal_focus_on(None)
}

/// The display-parameterised form lets tests avoid touching `DISPLAY`.
#[cfg(feature = "desktop")]
fn query_terminal_focus_on(display: Option<&str>) -> Option<bool> {
    use x11rb::connection::Connection as _;
    if display.is_none() && std::env::var_os("DISPLAY").is_none() {
        return None;
    }
    let (connection, screen_num) = x11rb::connect(display).ok()?;
    let root = connection.setup().roots[screen_num].root;
    terminal_focus_with_connection(&connection, root)
}

#[cfg(feature = "desktop")]
fn terminal_focus_with_connection(
    connection: &impl x11rb::connection::Connection,
    root: u32,
) -> Option<bool> {
    use x11rb::protocol::xproto::{self, AtomEnum};

    let active_atom = xproto::intern_atom(connection, false, b"_NET_ACTIVE_WINDOW")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let mut window = x11rb::NONE;
    if active_atom != x11rb::NONE {
        if let Ok(reply) =
            xproto::get_property(connection, false, root, active_atom, AtomEnum::WINDOW, 0, 1)
                .ok()?
                .reply()
        {
            if let Some(active) = reply.value32().and_then(|mut values| values.next()) {
                window = active;
            }
        }
    }
    if window == x11rb::NONE {
        // No EWMH property (no window manager, or the manager is starting):
        // fall back to the server's input focus.
        window = xproto::get_input_focus(connection)
            .ok()?
            .reply()
            .ok()?
            .focus;
    }
    if window == x11rb::NONE || window == 1 {
        return Some(false);
    }
    Some(window_is_terminal(connection, window, 8))
}

/// A terminal WM_CLASS is matched exactly or by the last reverse-DNS
/// component: WezTerm uses `org.wezfurlong.wezterm` and Warp uses
/// `dev.warp.Warp`.
#[cfg(feature = "desktop")]
fn is_terminal_class(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    TERMINAL_CLASSES.contains(&lower.as_str())
        || lower
            .rsplit('.')
            .next()
            .is_some_and(|tail| TERMINAL_CLASSES.contains(&tail))
}

/// Check a window and, for focus children without WM_CLASS, its ancestors.
#[cfg(feature = "desktop")]
fn window_is_terminal(
    connection: &impl x11rb::connection::Connection,
    window: u32,
    depth: u8,
) -> bool {
    use x11rb::protocol::xproto::{self, AtomEnum};

    if window == x11rb::NONE || depth == 0 {
        return false;
    }
    let class = xproto::get_property(
        connection,
        false,
        window,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        0,
        1024,
    )
    .ok()
    .and_then(|cookie| cookie.reply().ok());
    if let Some(class) = class {
        let text = String::from_utf8_lossy(&class.value);
        let names: Vec<&str> = text.split('\0').filter(|part| !part.is_empty()).collect();
        if !names.is_empty() {
            return names.iter().any(|name| is_terminal_class(name));
        }
    }
    let Some(tree) = xproto::query_tree(connection, window)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
    else {
        return false;
    };
    tree.parent != x11rb::NONE
        && tree.parent != window
        && window_is_terminal(connection, tree.parent, depth - 1)
}

/// Whether the desktop is locked. Unknown state counts as unlocked so a host
/// without logind still serves input; `UU_LOCK_FAIL_CLOSED=1` flips that.
#[cfg(feature = "desktop")]
pub(crate) fn lock_active(locked: &watch::Receiver<Option<bool>>) -> bool {
    super::session_lock::effective_locked(*locked.borrow())
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use super::*;
    use crate::desktop::CapturedFrame;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    #[test]
    fn normalized_coordinates_stay_inside_the_screen() {
        assert_eq!(normalized_to_pixel(-1.0, 100), 0.0);
        assert_eq!(normalized_to_pixel(0.0, 100), 0.0);
        assert_eq!(normalized_to_pixel(1.0, 100), 99.0);
        assert_eq!(normalized_to_pixel(2.0, 100), 99.0);
        assert_eq!(normalized_to_pixel(0.5, 1), 0.0);
    }

    #[tokio::test]
    async fn window_and_virtual_sources_are_rejected_without_touching_x11() {
        for source in [SourceKind::Window, SourceKind::Virtual] {
            let result = start_at(
                None,
                DesktopOptions {
                    source,
                    ..DesktopOptions::default()
                },
            )
            .await;
            let Err(error) = result else {
                panic!("{source:?} source must be rejected by the X11 backend");
            };
            assert!(error.contains("whole monitor"), "{error}");
            assert!(error.contains("portal"), "{error}");
        }
    }

    struct Xvfb(Child);

    impl Drop for Xvfb {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Start Xvfb and learn its display from `-displayfd`, so a server that
    /// already exists on a guessed number can never be used by accident.
    fn start_xvfb(width: u32, height: u32) -> (Xvfb, String) {
        use std::io::BufRead;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::os::unix::process::CommandExt;

        let geometry = format!("{width}x{height}x24");
        let (reader, writer) = UnixStream::pair().expect("displayfd pipe");
        let writer_fd = writer.as_raw_fd();
        let mut command = Command::new("Xvfb");
        command
            .args([
                "-displayfd",
                "3",
                "-screen",
                "0",
                &geometry,
                "-nolisten",
                "tcp",
            ])
            .args(["+extension", "RANDR", "+extension", "DAMAGE"])
            .args(["+extension", "MIT-SHM", "+extension", "XFIXES"])
            .args(["+extension", "XTEST"]);
        // SAFETY: pre_exec only duplicates an already-open descriptor; it does
        // not allocate, lock, or otherwise run non-async-signal-safe code.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(writer_fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("Xvfb");
        drop(writer);
        let mut reader = std::io::BufReader::new(reader);
        let _ = reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(10)));
        let mut line = String::new();
        reader.read_line(&mut line).expect("Xvfb display number");
        let number = line.trim();
        assert!(!number.is_empty(), "Xvfb did not report a display");
        (Xvfb(child), format!(":{number}"))
    }

    fn poll_until<T>(timeout: Duration, mut check: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(value) = check() {
                return value;
            }
            assert!(Instant::now() < deadline, "condition timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_frame(session: &DesktopSession, timeout: Duration) -> CapturedFrame {
        poll_until(timeout, || session.latest_frame())
    }

    #[tokio::test]
    #[ignore = "requires Xvfb with RANDR, DAMAGE, MIT-SHM, XFIXES and XTEST"]
    async fn xvfb_capture_redraw_pointer_and_keyboard() {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto;
        use x11rb::protocol::xproto::ConnectionExt as _;

        let (_server, display) = start_xvfb(320, 240);
        // The harness must exercise the real backend, not the fallbacks.
        {
            use x11rb::connection::RequestConnection as _;
            let (probe, _screen) = x11rb::connect(Some(&display)).unwrap();
            for extension in ["DAMAGE", "MIT-SHM", "XFIXES", "XTEST", "RANDR"] {
                assert!(
                    probe.extension_information(extension).unwrap().is_some(),
                    "Xvfb is missing {extension}"
                );
            }
        }
        // Inject an unlocked state: a locked host session (or a fail-closed
        // env) must not pause the capture this test exercises.
        let (_locked_tx, locked) = watch::channel(Some(false));
        let session = start_at_with(Some(&display), DesktopOptions::default(), locked)
            .await
            .expect("X11 capture");
        assert_eq!(
            session
                .screens()
                .first()
                .map(|screen| (screen.width, screen.height)),
            Some((320, 240))
        );

        let first = wait_frame(&session, Duration::from_secs(5));
        assert_eq!((first.width, first.height), (320, 240));
        assert_eq!(first.data.len(), 320 * 240 * 4);

        // Drawing into the root must reach the capture through XDamage.
        let (connection, screen_num) = x11rb::connect(Some(&display)).unwrap();
        let root = connection.setup().roots[screen_num].root;
        let white = connection.setup().roots[screen_num].white_pixel;
        let gc = connection.generate_id().unwrap();
        connection
            .create_gc(gc, root, &xproto::CreateGCAux::new().foreground(white))
            .unwrap();
        connection
            .poly_fill_rectangle(
                root,
                gc,
                &[xproto::Rectangle {
                    x: 0,
                    y: 0,
                    width: 320,
                    height: 240,
                }],
            )
            .unwrap();
        connection.flush().unwrap();
        let changed = poll_until(Duration::from_secs(5), || {
            session
                .latest_frame()
                .filter(|frame| frame.data.as_slice() != first.data.as_slice())
        });
        assert!(changed
            .data
            .chunks_exact(4)
            .all(|pixel| pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200));

        // Absolute pointer input lands on the monitor.
        session.input().pointer_absolute(0.5, 0.5);
        poll_until(Duration::from_secs(5), || {
            let reply = xproto::query_pointer(&connection, root)
                .ok()?
                .reply()
                .ok()?;
            (reply.root_x == 160 && reply.root_y == 120).then_some(())
        });

        // A key press and release click through XTest and the keymap.
        let before = xproto::query_keymap(&connection)
            .unwrap()
            .reply()
            .unwrap()
            .keys;
        session.input().key(65, true);
        poll_until(Duration::from_secs(5), || {
            let keys = xproto::query_keymap(&connection)
                .unwrap()
                .reply()
                .unwrap()
                .keys;
            (keys != before).then_some(())
        });
        session.input().key(65, false);
        poll_until(Duration::from_secs(5), || {
            let keys = xproto::query_keymap(&connection)
                .unwrap()
                .reply()
                .unwrap()
                .keys;
            (keys == before).then_some(())
        });

        // The XFixes thread publishes the current shape.
        let mut cursor = session.cursor();
        poll_until(Duration::from_secs(5), || {
            cursor.borrow_and_update().clone()
        });

        // DPMS force-on plus the zero-motion XTest event must both resolve.
        wake_display_blocking(Some(&display)).expect("X11 display wake");
        session.stop();
    }

    #[tokio::test]
    #[ignore = "requires Xvfb"]
    async fn xvfb_terminal_focus_detection() {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{self, AtomEnum, PropMode};

        let (_server, display) = start_xvfb(320, 240);
        let (connection, screen_num) = x11rb::connect(Some(&display)).unwrap();
        let root = connection.setup().roots[screen_num].root;
        let window = connection.generate_id().unwrap();
        xproto::create_window(
            &connection,
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            10,
            10,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new(),
        )
        .unwrap();
        let active_atom = xproto::intern_atom(&connection, false, b"_NET_ACTIVE_WINDOW")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let set_class = |class: &[u8]| {
            xproto::change_property(
                &connection,
                PropMode::REPLACE,
                window,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                8,
                class.len() as u32,
                class,
            )
            .unwrap();
            connection.flush().unwrap();
        };
        let set_active = |active: u32| {
            xproto::change_property(
                &connection,
                PropMode::REPLACE,
                root,
                active_atom,
                AtomEnum::WINDOW,
                32,
                1,
                &active.to_le_bytes(),
            )
            .unwrap();
            connection.flush().unwrap();
        };
        set_class(b"xterm\0XTerm");
        set_active(window);
        assert_eq!(query_terminal_focus_on(Some(&display)), Some(true));
        // Reverse-DNS WM_CLASS names must still match by their last component.
        set_class(b"org.wezfurlong.wezterm\0org.wezfurlong.wezterm");
        assert_eq!(query_terminal_focus_on(Some(&display)), Some(true));
        set_class(b"example\0Example");
        assert_eq!(query_terminal_focus_on(Some(&display)), Some(false));
        // Another terminal-class window elsewhere must not affect the answer.
        let other = connection.generate_id().unwrap();
        xproto::create_window(
            &connection,
            x11rb::COPY_DEPTH_FROM_PARENT,
            other,
            root,
            0,
            0,
            10,
            10,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new(),
        )
        .unwrap();
        xproto::change_property(
            &connection,
            PropMode::REPLACE,
            other,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            8,
            b"xterm\0XTerm".len() as u32,
            b"xterm\0XTerm",
        )
        .unwrap();
        connection.flush().unwrap();
        assert_eq!(query_terminal_focus_on(Some(&display)), Some(false));
        // Without _NET_ACTIVE_WINDOW the server input focus is used; a focus
        // child without WM_CLASS inherits its parent's class.
        let child = connection.generate_id().unwrap();
        xproto::create_window(
            &connection,
            x11rb::COPY_DEPTH_FROM_PARENT,
            child,
            other,
            0,
            0,
            5,
            5,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new(),
        )
        .unwrap();
        xproto::map_window(&connection, other).unwrap();
        xproto::map_window(&connection, child).unwrap();
        xproto::delete_property(&connection, root, active_atom).unwrap();
        xproto::set_input_focus(
            &connection,
            xproto::InputFocus::NONE,
            child,
            x11rb::CURRENT_TIME,
        )
        .unwrap();
        connection.flush().unwrap();
        assert_eq!(query_terminal_focus_on(Some(&display)), Some(true));
        xproto::set_input_focus(
            &connection,
            xproto::InputFocus::NONE,
            x11rb::NONE,
            x11rb::CURRENT_TIME,
        )
        .unwrap();
        set_active(0);
        assert_eq!(query_terminal_focus_on(Some(&display)), Some(false));
        xproto::destroy_window(&connection, child).unwrap();
        xproto::destroy_window(&connection, window).unwrap();
        xproto::destroy_window(&connection, other).unwrap();
        connection.flush().unwrap();
    }
}
