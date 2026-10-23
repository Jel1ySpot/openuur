//! X11 display-mode control for the viewer's resolution setting.
//!
//! The official Windows host changes the desktop mode instead of only scaling
//! the encoded stream. X11 has no portable mode API without linking libXrandr,
//! so this module drives `xrandr` with bounded, parseable arguments:
//!
//! - an existing mode with the requested size is selected directly;
//! - otherwise a VESA CVT reduced-blanking mode is added, which can be larger
//!   than the panel's native mode (virtual outputs and some drivers accept it);
//! - the mode in use before the first change is restored when the viewer picks
//!   "original" and after the last desktop session ends.
//!
//! `UU_DESKTOP_RESIZE=0` keeps the old behaviour: only the encoder scales.

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// "Original" / follow the remote display: restore the saved mode.
    Native,
    Size(u32, u32),
}

struct Original {
    display: Option<String>,
    output: String,
    mode: String,
}

static ORIGINAL: Mutex<Option<Original>> = Mutex::new(None);
static STREAMS: AtomicUsize = AtomicUsize::new(0);

/// Encoder limits (H.264 NVENC/VAAPI reject larger canvases) and the smallest
/// desktop the viewer can use without breaking its layout.
const MAX_SIDE: u32 = 4096;
const MIN_SIZE: (u32, u32) = (640, 480);

pub fn enabled() -> bool {
    std::env::var("UU_DESKTOP_RESIZE").map_or(true, |value| value != "0")
}

/// Keeps the changed mode while any desktop stream is alive. The last guard
/// restores the original mode after a short grace period, so a peer replacing
/// another (or a reconnect) does not bounce the display twice.
pub struct StreamGuard;

impl StreamGuard {
    pub fn new() -> Self {
        STREAMS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Default for StreamGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if STREAMS.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        if ORIGINAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none()
        {
            return;
        }
        // xrandr blocks; never run it on the async runtime thread that dropped
        // the session.
        let _ = std::thread::Builder::new()
            .name("openuur-display-restore".into())
            .spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if STREAMS.load(Ordering::Acquire) == 0 {
                    restore();
                }
            });
    }
}

/// Apply a viewer resolution request to the X11 display.
///
/// Returns the display size now in use, or `None` when this server cannot be
/// resized (missing xrandr, no connected output, or an error). The caller then
/// keeps encoder scaling as the fallback.
pub fn apply(display: Option<&str>, request: Request) -> Option<(u32, u32)> {
    match request {
        Request::Native => {
            restore();
            query(display).ok().and_then(|state| state.current)
        }
        Request::Size(width, height) => {
            let target = fit(width, height)?;
            match switch(display, target) {
                Ok(()) => query(display).ok().and_then(|state| state.current),
                Err(error) => {
                    tracing::warn!(
                        "X11 display resize to {}x{} failed: {error}",
                        target.0,
                        target.1
                    );
                    None
                }
            }
        }
    }
}

/// Even sides within the supported range, preserving the aspect ratio when a
/// request exceeds the encoder limit.
fn fit(width: u32, height: u32) -> Option<(u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    let scale = (f64::from(MAX_SIDE) / f64::from(width.max(height))).min(1.0);
    let width = (f64::from(width) * scale).round() as u32;
    let height = (f64::from(height) * scale).round() as u32;
    Some((width.max(MIN_SIZE.0) & !1, height.max(MIN_SIZE.1) & !1))
}

fn switch(display: Option<&str>, target: (u32, u32)) -> Result<(), String> {
    let state = query(display)?;
    if state.current == Some(target) {
        return Ok(());
    }
    {
        let mut original = ORIGINAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if original.is_none() {
            let mode = state.current_mode.clone().ok_or("current mode unknown")?;
            tracing::info!(
                "X11 display resize: saving original mode {mode} on {}",
                state.output
            );
            *original = Some(Original {
                display: display.map(str::to_owned),
                output: state.output.clone(),
                mode,
            });
        }
    }
    let mode = match state.modes.iter().find(|(_, size)| *size == target) {
        Some((name, _)) => name.clone(),
        None => {
            let (name, timings) = cvt_reduced(target.0, target.1);
            // An existing mode of this name from an earlier session makes
            // --newmode fail harmlessly; --addmode still needs to run.
            let mut args = vec!["--newmode".to_owned(), name.clone()];
            args.extend(timings);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let _ = xrandr(display, &args);
            xrandr(display, &["--addmode", &state.output, &name])?;
            name
        }
    };
    set_mode(display, &state.output, &mode, target)?;
    tracing::info!("X11 display resized to {}x{} ({mode})", target.0, target.1);
    Ok(())
}

/// Restore the mode saved before the first change, if any.
pub fn restore() {
    let Some(original) = ORIGINAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    else {
        return;
    };
    let size = query(original.display.as_deref())
        .ok()
        .and_then(|state| {
            state
                .modes
                .into_iter()
                .find(|(name, _)| *name == original.mode)
                .map(|(_, size)| size)
        })
        .ok_or_else(|| "original mode is no longer listed".to_owned());
    match size.and_then(|size| {
        set_mode(
            original.display.as_deref(),
            &original.output,
            &original.mode,
            size,
        )
    }) {
        Ok(()) => tracing::info!("X11 display mode restored to {}", original.mode),
        Err(error) => tracing::warn!("X11 display mode restore failed: {error}"),
    }
}

/// GNOME/mutter may re-apply its preferred mode after a mode-list change.
/// Verify the mode stuck instead of assuming a single xrandr call won.
fn set_mode(
    display: Option<&str>,
    output: &str,
    mode: &str,
    expected: (u32, u32),
) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..3 {
        if let Err(error) = xrandr(display, &["--output", output, "--mode", mode]) {
            last = error;
        } else {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            match query(display) {
                Ok(state) if state.current == Some(expected) => return Ok(()),
                Ok(state) => {
                    last = format!(
                        "{output} reports {:?}, expected {expected:?}",
                        state.current
                    )
                }
                Err(error) => last = error,
            }
        }
    }
    Err(if last.is_empty() {
        format!("{output} did not keep {mode}")
    } else {
        last
    })
}

fn xrandr(display: Option<&str>, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("xrandr");
    if let Some(display) = display.filter(|display| !display.is_empty()) {
        command.arg("--display").arg(display);
    }
    let output = command
        .args(args)
        .output()
        .map_err(|error| format!("xrandr unavailable: {}", error.kind()))?;
    if !output.status.success() {
        return Err(format!(
            "xrandr {} exited with {}",
            args.first().copied().unwrap_or(""),
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Debug)]
struct State {
    output: String,
    current: Option<(u32, u32)>,
    current_mode: Option<String>,
    modes: Vec<(String, (u32, u32))>,
}

fn query(display: Option<&str>) -> Result<State, String> {
    parse(&xrandr(display, &["--query"])?).ok_or_else(|| "no connected output with a mode".into())
}

/// The primary connected output (else the first connected one with a mode)
/// and its mode list.
fn parse(text: &str) -> Option<State> {
    let mut outputs: Vec<(bool, State)> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("   ") {
            let Some((_, state)) = outputs.last_mut() else {
                continue;
            };
            let mut parts = rest.split_whitespace();
            let Some(name) = parts.next() else { continue };
            let Some(size) = mode_size(name) else {
                continue;
            };
            if parts.any(|rate| rate.contains('*')) {
                state.current = Some(size);
                state.current_mode = Some(name.to_owned());
            }
            state.modes.push((name.to_owned(), size));
        } else if !line.starts_with(' ') {
            let mut parts = line.split_whitespace();
            let (Some(name), Some("connected")) = (parts.next(), parts.next()) else {
                outputs.push((
                    false,
                    State {
                        output: String::new(),
                        current: None,
                        current_mode: None,
                        modes: Vec::new(),
                    },
                ));
                continue;
            };
            let primary = line.contains(" connected primary ");
            outputs.push((
                primary,
                State {
                    output: name.to_owned(),
                    current: None,
                    current_mode: None,
                    modes: Vec::new(),
                },
            ));
        }
    }
    let mut connected: Vec<(bool, State)> = outputs
        .into_iter()
        .filter(|(_, state)| !state.output.is_empty() && state.current.is_some())
        .collect();
    let index = connected
        .iter()
        .position(|(primary, _)| *primary)
        .unwrap_or(0);
    (index < connected.len()).then(|| connected.swap_remove(index).1)
}

/// "1920x1080", "1920x1080R", "1920x1080_60.00", "1920x1080i" -> (1920, 1080).
fn mode_size(name: &str) -> Option<(u32, u32)> {
    let (width, rest) = name.split_once('x')?;
    let height: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((width.parse().ok()?, height.parse().ok()?))
}

/// VESA CVT reduced-blanking (v1) timings: 60 Hz, or 30 Hz when the 60 Hz
/// pixel clock exceeds HDMI 2.0 rates (virtual/forced connectors on integrated
/// GPUs reject faster modes).
fn cvt_reduced(width: u32, height: u32) -> (String, Vec<String>) {
    const H_BLANK: u32 = 160;
    const H_SYNC: u32 = 32;
    const H_FRONT: u32 = 48;
    const V_FRONT: u32 = 3;
    const MIN_V_BACK: u32 = 6;
    const MIN_VBLANK_US: f64 = 460.0;
    let v_sync = match (
        width * 3 == height * 4,
        width * 9 == height * 16,
        width * 10 == height * 16,
        width * 4 == height * 5,
        width * 9 == height * 15,
    ) {
        (true, ..) => 4,
        (_, true, ..) => 5,
        (_, _, true, ..) => 6,
        (_, _, _, true, _) | (.., true) => 7,
        _ => 10,
    };
    let timing = |refresh: f64| {
        let h_period = (1_000_000.0 / refresh - MIN_VBLANK_US) / f64::from(height);
        let vbi = ((MIN_VBLANK_US / h_period) as u32 + 1).max(V_FRONT + v_sync + MIN_V_BACK);
        let total_h = width + H_BLANK;
        let total_v = height + vbi;
        let clock =
            (refresh * f64::from(total_h) * f64::from(total_v) / 1_000_000.0 / 0.25).floor() * 0.25;
        (clock, total_h, total_v)
    };
    let (mut refresh, mut timings) = (60, timing(60.0));
    if timings.0 > 590.0 {
        refresh = 30;
        timings = timing(30.0);
    }
    let (clock, total_h, total_v) = timings;
    let name = format!("{width}x{height}_{refresh}R");
    let timings = vec![
        format!("{clock:.2}"),
        width.to_string(),
        (width + H_FRONT).to_string(),
        (width + H_FRONT + H_SYNC).to_string(),
        total_h.to_string(),
        height.to_string(),
        (height + V_FRONT).to_string(),
        (height + V_FRONT + v_sync).to_string(),
        total_v.to_string(),
        "+hsync".to_owned(),
        "-vsync".to_owned(),
    ];
    (name, timings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_blanking_matches_cvt() {
        let (name, timings) = cvt_reduced(1920, 1080);
        assert_eq!(name, "1920x1080_60R");
        assert_eq!(
            timings.join(" "),
            "138.50 1920 1968 2000 2080 1080 1083 1088 1111 +hsync -vsync"
        );
        assert_eq!(
            cvt_reduced(2560, 1600).1.join(" "),
            "268.50 2560 2608 2640 2720 1600 1603 1609 1646 +hsync -vsync"
        );
        assert_eq!(cvt_reduced(3840, 2160).0, "3840x2160_60R");
        assert_eq!(cvt_reduced(4096, 2560).0, "4096x2560_30R");
    }

    #[test]
    fn requested_sizes_are_even_and_bounded() {
        assert_eq!(fit(1280, 720), Some((1280, 720)));
        assert_eq!(fit(4112, 2580), Some((4096, 2570)));
        assert_eq!(fit(301, 201), Some((640, 480)));
        assert_eq!(fit(0, 1080), None);
    }

    #[test]
    fn parses_primary_output_modes_and_current_mode() {
        let text = r"Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 16384 x 16384
DP-1 disconnected (normal left inverted right x axis y axis)
HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 0mm x 0mm
   1024x768      60.00
   3840x2160R    29.98
   1920x1080R    59.93*+
HDMI-2 disconnected (normal left inverted right x axis y axis)
";
        let state = parse(text).unwrap();
        assert_eq!(state.output, "HDMI-1");
        assert_eq!(state.current, Some((1920, 1080)));
        assert_eq!(state.current_mode.as_deref(), Some("1920x1080R"));
        assert_eq!(state.modes.len(), 3);
        assert_eq!(state.modes[1], ("3840x2160R".to_owned(), (3840, 2160)));
        assert!(parse("Screen 0: minimum 320 x 200\nDP-1 disconnected (normal)\n").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Xvfb and xrandr"]
    fn queries_and_accepts_the_current_mode_on_a_real_server() {
        use std::io::BufRead;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::os::unix::process::CommandExt;
        let (reader, writer) = UnixStream::pair().unwrap();
        let fd = writer.as_raw_fd();
        let mut command = Command::new("Xvfb");
        command.args([
            "-displayfd",
            "3",
            "-screen",
            "0",
            "1280x720x24",
            "-nolisten",
            "tcp",
        ]);
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().expect("Xvfb");
        drop(writer);
        let mut reader = std::io::BufReader::new(reader);
        reader
            .get_ref()
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let display = format!(":{}", line.trim());
        assert_eq!(apply(Some(&display), Request::Native), Some((1280, 720)));
        assert_eq!(
            apply(Some(&display), Request::Size(1280, 720)),
            Some((1280, 720))
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn accepts_modes_larger_than_the_original_desktop() {
        let text = r"Screen 0: minimum 320 x 200, current 1280 x 720, maximum 16384 x 16384
eDP-1 connected primary 1280x720+0+0 (normal) 0mm x 0mm
   1280x720      60.00*+
   2560x1440_60R 59.95
";
        let state = parse(text).unwrap();
        assert_eq!(state.current, Some((1280, 720)));
        assert!(state.modes.iter().any(|(_, size)| *size == (2560, 1440)));
        assert_eq!(fit(2560, 1440), Some((2560, 1440)));
    }
}
