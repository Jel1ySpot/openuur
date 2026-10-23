//! Desktop capture and input backends for the native UU host.
//!
//! The host only depends on the contracts in this module. Wayland is
//! implemented with xdg-desktop-portal ScreenCast + RemoteDesktop and
//! PipeWire; the X11 backend implements the same contract with XDamage,
//! MIT-SHM and XTest.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::watch;

use session_env::{DesktopKind, SessionEnv, SessionType};

pub mod apps;
pub mod audio;
pub mod clipboard;
pub mod cursor;
pub mod desktop_power;
pub mod display_mode;
pub mod encoder;
pub mod hevc;
pub mod input_map;
pub mod lease;
pub mod portal_permissions;
pub mod rotation;
pub mod session_env;
pub mod session_lock;
pub mod session_power;
pub mod video_track;
pub mod virtual_audio;
pub use encoder::FPS;

#[cfg(all(target_os = "linux", feature = "desktop"))]
mod eis;
#[cfg(all(target_os = "linux", feature = "desktop"))]
pub mod uinput;
#[cfg(all(target_os = "linux", feature = "desktop"))]
mod uinput_scroll;
#[cfg(not(target_os = "linux"))]
mod unsupported;
#[cfg(all(target_os = "linux", feature = "desktop"))]
mod wayland;
/// Portal denial classification used by the host supervisor.
#[cfg(all(target_os = "linux", feature = "desktop"))]
pub use wayland::{portal_error_code, terminal_portal_code, terminal_portal_error};
#[cfg(all(target_os = "linux", feature = "desktop"))]
mod wlroots;
#[cfg(target_os = "linux")]
mod x11;

#[derive(Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed BGRA rows.
    pub data: Arc<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// xdg-desktop-portal ScreenCast + RemoteDesktop.
    Wayland,
    /// Native wlroots protocols (screencopy, virtual pointer/keyboard).
    Wlroots,
    X11,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Self::Wayland => "portal",
            Self::Wlroots => "wlroots",
            Self::X11 => "x11",
        }
    }

    /// Whether this build can actually capture with the backend today.
    pub fn implemented(self) -> bool {
        match self {
            Self::Wayland => cfg!(all(target_os = "linux", feature = "desktop")),
            Self::X11 => cfg!(all(target_os = "linux", feature = "desktop")),
            Self::Wlroots => cfg!(all(target_os = "linux", feature = "desktop")),
        }
    }
}

/// The operator's backend preference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BackendChoice {
    #[default]
    Auto,
    Portal,
    Wlroots,
    X11,
}

impl BackendChoice {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "portal" | "wayland" | "pipewire" => Ok(Self::Portal),
            "wlroots" | "wlr" | "native" => Ok(Self::Wlroots),
            "x11" | "xorg" => Ok(Self::X11),
            other => Err(format!(
                "unknown desktop backend {other:?}; use auto, portal, wlroots or x11"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Portal => "portal",
            Self::Wlroots => "wlroots",
            Self::X11 => "x11",
        }
    }
}

/// The backend requested by the operator and the one that will actually run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendSelection {
    pub requested: Backend,
    pub effective: Backend,
    pub fallback: bool,
}

/// Pure backend choice for a discovered session.
pub fn select_backend(choice: BackendChoice, session: &SessionEnv) -> Backend {
    match choice {
        BackendChoice::Portal => Backend::Wayland,
        BackendChoice::Wlroots => Backend::Wlroots,
        BackendChoice::X11 => Backend::X11,
        BackendChoice::Auto => match session.session_type {
            SessionType::X11 => Backend::X11,
            SessionType::Wayland => match session.desktop {
                DesktopKind::Gnome | DesktopKind::Kde => Backend::Wayland,
                DesktopKind::Other => Backend::Wlroots,
            },
        },
    }
}

/// The selected backend, without fallback. Auto resolves to wlroots for
/// non-GNOME/KDE Wayland sessions and to X11 for Xorg sessions.
pub fn effective_selection(choice: BackendChoice, session: &SessionEnv) -> BackendSelection {
    let requested = select_backend(choice, session);
    BackendSelection {
        requested,
        effective: requested,
        fallback: false,
    }
}

/// Resolve the backend for the requested capture source.
///
/// Only the portal can capture a window or a compositor virtual source. Auto
/// therefore falls back to the portal for those sources on a wlroots desktop;
/// an explicit wlroots request fails with an actionable message.
pub fn backend_for_source(
    selection: BackendSelection,
    source: SourceKind,
    automatic: bool,
) -> Result<Backend, String> {
    if source == SourceKind::Monitor {
        return Ok(selection.effective);
    }
    match selection.effective {
        // Only the portal can capture a window or a compositor virtual source.
        Backend::Wlroots | Backend::X11 if automatic => Ok(Backend::Wayland),
        Backend::Wlroots => Err("the native wlroots backend captures whole outputs; use \
             --desktop-backend portal for window or virtual sources"
            .to_owned()),
        Backend::X11 => Err("the native X11 backend captures whole outputs; use \
             --desktop-backend portal for window or virtual sources"
            .to_owned()),
        Backend::Wayland => Ok(Backend::Wayland),
    }
}

/// Resolve the selected backend from the current graphical session.
/// Whether the portal EIS input path can be used (libei is loadable).
#[cfg(all(target_os = "linux", feature = "desktop"))]
pub fn libei_available() -> bool {
    eis::EisInput::supported()
}

#[cfg(not(all(target_os = "linux", feature = "desktop")))]
pub fn libei_available() -> bool {
    false
}

pub async fn detect_backend(choice: BackendChoice) -> Result<Backend, String> {
    Ok(detect_backend_selection(choice).await?.0.effective)
}

/// Check that the active compositor exposes the native wlroots protocols.
pub(crate) fn probe_wlroots() -> Result<(), String> {
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    {
        wlroots::probe()
    }
    #[cfg(not(all(target_os = "linux", feature = "desktop")))]
    {
        Err("this build has no wlroots support".to_owned())
    }
}

pub async fn detect_backend_selection(
    choice: BackendChoice,
) -> Result<(BackendSelection, SessionEnv), String> {
    #[cfg(not(all(target_os = "linux", feature = "desktop")))]
    {
        let _ = choice;
        return Err(
            "this build has no desktop capture support; rebuild with the `desktop` feature"
                .to_owned(),
        );
    }
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    match session_env::detect().await {
        session_env::SessionState::Available(env) => Ok((effective_selection(choice, &env), env)),
        session_env::SessionState::NoSession | session_env::SessionState::Unknown => {
            Err(session_env::WAITING.to_owned())
        }
        session_env::SessionState::Unavailable(error) => {
            Err(format!("{} ({error})", session_env::WAITING))
        }
    }
}

/// What the portal is allowed to capture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourceKind {
    #[default]
    Monitor,
    Window,
    Virtual,
}

impl SourceKind {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "" | "monitor" | "screen" | "desktop" => Ok(Self::Monitor),
            "window" => Ok(Self::Window),
            "virtual" => Ok(Self::Virtual),
            other => Err(format!(
                "unknown display source {other:?}; use monitor, window or virtual"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::Window => "window",
            Self::Virtual => "virtual",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DesktopOptions {
    pub restore_token: Option<String>,
    pub source: SourceKind,
    pub clipboard: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenInfo {
    pub id: u32,
    /// PipeWire node used for portal absolute-pointer input.
    pub node_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    /// Portal ScreenCast `mapping_id` for the matching EIS region.
    pub mapping_id: Option<String>,
}

/// One pointer image from a backend that can observe the hardware cursor.
/// Only the X11 backend produces these today; Wayland has no global cursor.
#[derive(Clone, Debug)]
pub struct CursorImage {
    pub serial: u32,
    pub width: u16,
    pub height: u16,
    pub xhot: u16,
    pub yhot: u16,
    /// Premultiplied ARGB, row-major, X11 bit order (`0xAARRGGBB`).
    pub argb: Arc<Vec<u32>>,
}

/// A desktop input backend. Calls enqueue events and return immediately.
pub trait InputSink: Send + Sync {
    fn pointer_absolute(&self, x: f64, y: f64);
    fn pointer_relative(&self, dx: i32, dy: i32);
    /// UU wire button: 1 = left, 2 = right, 4 = middle.
    fn button(&self, button: i32, pressed: bool);
    fn scroll(&self, dx: i32, dy: i32);
    /// Windows virtual-key code from the official CONTROL JSON.
    fn key(&self, vk: i32, pressed: bool);
    fn text(&self, text: &str);
    /// Drop any translated key state after input was paused (lock/suspend).
    /// Backends that hold no translation state may ignore this.
    fn reset(&self) {}
}

/// Shared mutable capture state. The portal supervisor replaces the screen
/// inventory in place after a display-config change or session recovery, so a
/// peer keeps the same `DesktopSession` handle.
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
pub(crate) struct DesktopState {
    width: AtomicU32,
    height: AtomicU32,
    screens: RwLock<Vec<ScreenInfo>>,
    active: AtomicU32,
    input_target: RwLock<(u32, u32, u32)>,
    frames: watch::Sender<Option<CapturedFrame>>,
    generation: watch::Sender<u64>,
}

#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
impl DesktopState {
    pub(crate) fn new(
        screens: Vec<ScreenInfo>,
        active: u32,
        input_target: (u32, u32, u32),
        frames: watch::Sender<Option<CapturedFrame>>,
        generation: watch::Sender<u64>,
    ) -> Self {
        let size = screens
            .iter()
            .find(|screen| screen.id == active)
            .map(|screen| (screen.width, screen.height))
            .unwrap_or((0, 0));
        Self {
            width: AtomicU32::new(size.0),
            height: AtomicU32::new(size.1),
            screens: RwLock::new(screens),
            active: AtomicU32::new(active),
            input_target: RwLock::new(input_target),
            frames,
            generation,
        }
    }

    pub(crate) fn publish(
        &self,
        screens: Vec<ScreenInfo>,
        active: u32,
        input_target: (u32, u32, u32),
    ) {
        // Keep a screen switch that raced with this inventory update: the
        // stored id wins while it still exists in the new list, and the
        // compare-exchange avoids overwriting a switch made during this call.
        let mut current = self.active.load(Ordering::Acquire);
        let active = loop {
            let desired = if screens.iter().any(|screen| screen.id == current) {
                current
            } else if screens.iter().any(|screen| screen.id == active) {
                active
            } else {
                screens.first().map(|screen| screen.id).unwrap_or(0)
            };
            match self.active.compare_exchange(
                current,
                desired,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break desired,
                Err(actual) => current = actual,
            }
        };
        let size = screens
            .iter()
            .find(|screen| screen.id == active)
            .map(|screen| (screen.width, screen.height))
            .unwrap_or((0, 0));
        if let Ok(mut current) = self.screens.write() {
            *current = screens;
        }
        if let Ok(mut target) = self.input_target.write() {
            *target = input_target;
        }
        self.width.store(size.0, Ordering::Release);
        self.height.store(size.1, Ordering::Release);
        self.frames.send_replace(None);
        self.generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    pub(crate) fn publish_frame(&self, frame: CapturedFrame) {
        self.frames.send_replace(Some(frame));
    }

    pub(crate) fn clear_frame(&self) {
        self.frames.send_replace(None);
    }

    pub(crate) fn screens(&self) -> Vec<ScreenInfo> {
        self.screens
            .read()
            .map(|screens| screens.clone())
            .unwrap_or_default()
    }

    /// The active screen's rectangle, if the backend published one.
    pub(crate) fn active_screen(&self) -> Option<ScreenInfo> {
        let active = self.active.load(Ordering::Acquire);
        self.screens()
            .into_iter()
            .find(|screen| screen.id == active)
    }

    /// The active screen id (0 when the backend published no screens).
    pub(crate) fn active_id(&self) -> u32 {
        self.active.load(Ordering::Acquire)
    }
}

pub struct DesktopSession {
    pub backend: Backend,
    state: Arc<DesktopState>,
    frames: watch::Receiver<Option<CapturedFrame>>,
    cursor: watch::Receiver<Option<CursorImage>>,
    input: Arc<dyn InputSink>,
    stop: Arc<AtomicBool>,
    /// Backend task handle. Aborted after a short input-release grace period
    /// (see [`DesktopSession::stop`]).
    abort: tokio::task::AbortHandle,
    /// Signalled by the portal supervisor once it has released held input.
    cleanup: Arc<tokio::sync::Notify>,
    /// Backend-specific display wake (wlroots output power).
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    clipboard: std::sync::Mutex<Option<clipboard::ClipboardBridge>>,
    /// X11 display this session captures, when it has an explicit one.
    display: Option<String>,
    /// Keeps a changed X11 mode alive while this session runs; the last
    /// desktop session restores the original mode when it is stopped.
    display_guard: std::sync::Mutex<Option<Box<dyn Send + Sync>>>,
}

impl DesktopSession {
    pub fn latest_frame(&self) -> Option<CapturedFrame> {
        self.frames.borrow().clone()
    }

    /// Subscribe to frame updates instead of polling `latest_frame`.
    pub fn frames(&self) -> watch::Receiver<Option<CapturedFrame>> {
        self.frames.clone()
    }

    /// Pointer shapes, when the backend can observe them (X11 only).
    pub fn cursor(&self) -> watch::Receiver<Option<CursorImage>> {
        self.cursor.clone()
    }

    pub fn input(&self) -> Arc<dyn InputSink> {
        self.input.clone()
    }

    /// Apply a viewer resolution choice to the host display.
    ///
    /// Only the X11 backend changes the desktop mode; portal and wlroots keep
    /// the compositor geometry and expose the requested size through encoder
    /// scaling instead. Returns the size now in use when the mode changed.
    pub fn apply_resolution(&self, request: display_mode::Request) -> Option<(u32, u32)> {
        if self.backend != Backend::X11 || !display_mode::enabled() {
            return None;
        }
        display_mode::apply(self.display.as_deref(), request)
    }

    pub fn screen(&self) -> u32 {
        self.state.active.load(Ordering::Acquire)
    }

    pub fn width(&self) -> u32 {
        self.state.width.load(Ordering::Acquire)
    }

    pub fn height(&self) -> u32 {
        self.state.height.load(Ordering::Acquire)
    }

    /// Snapshot of the current portal streams.
    pub fn screens(&self) -> Vec<ScreenInfo> {
        self.state.screens()
    }

    pub fn active_size(&self) -> (u32, u32) {
        let id = self.screen();
        self.screens()
            .iter()
            .find(|screen| screen.id == id)
            .map(|screen| (screen.width, screen.height))
            .unwrap_or((self.width(), self.height()))
    }

    pub fn switch_screen(&self, id: u32) -> bool {
        let Some(screen) = self.screens().into_iter().find(|screen| screen.id == id) else {
            return false;
        };
        self.state.active.store(id, Ordering::Release);
        if let Ok(mut target) = self.state.input_target.write() {
            *target = (screen.node_id, screen.width, screen.height);
        }
        true
    }

    /// Increments whenever the portal session is replaced (display change,
    /// lock recovery, compositor restart).
    pub fn generation(&self) -> watch::Receiver<u64> {
        self.state.generation.subscribe()
    }

    /// Take the portal clipboard bridge for this desktop connection. It is
    /// created once and reused; a second caller gets `None`.
    pub fn take_clipboard(&self) -> Option<clipboard::ClipboardBridge> {
        self.clipboard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    pub fn stop(&self) {
        if self.stop.swap(true, Ordering::AcqRel) {
            return;
        }
        // Ask the backend to release keys and buttons it still holds. The
        // portal supervisor signals `cleanup` when that release is done.
        self.input.reset();
        // Release the display-mode lease before aborting the backend task so
        // the restore runs even when the peer keeps the session Arc alive.
        if self.backend == Backend::X11 {
            let _ = self
                .display_guard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
        }
        let abort = self.abort.clone();
        if self.backend != Backend::Wayland {
            // X11 and wlroots release from their own input threads, which
            // observe `stop`; the session task only needs aborting.
            abort.abort();
            return;
        }
        let cleanup = self.cleanup.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    // Wait for the release pass, but never leave a stuck
                    // portal holding the task forever. Each release call has
                    // its own 2 s bound, so a few held keys can take longer
                    // than the old fixed delay.
                    let _ = tokio::time::timeout(Duration::from_secs(8), cleanup.notified()).await;
                    abort.abort();
                });
            }
            Err(_) => abort.abort(),
        }
    }

    /// Wake the display using this session's backend.
    pub async fn wake(&self) {
        if let Some(wake) = &self.wake {
            wake();
        }
        if let Err(error) = desktop_power::wake_backend(self.backend).await {
            tracing::debug!("desktop wake failed: {error}");
        }
    }
}

impl Drop for DesktopSession {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Open the native desktop with default options.
pub async fn start(
    backend: Backend,
    restore_token: Option<String>,
) -> Result<DesktopSession, String> {
    start_with(
        backend,
        DesktopOptions {
            restore_token,
            ..Default::default()
        },
    )
    .await
}

pub async fn start_with(
    backend: Backend,
    options: DesktopOptions,
) -> Result<DesktopSession, String> {
    match backend {
        #[cfg(all(target_os = "linux", feature = "desktop"))]
        Backend::Wayland => wayland::start(options).await,
        #[cfg(all(target_os = "linux", feature = "desktop"))]
        Backend::Wlroots => wlroots::start(options).await,
        #[cfg(all(target_os = "linux", not(feature = "desktop")))]
        Backend::Wayland | Backend::Wlroots => {
            let _ = options;
            Err(
                "this build has no desktop capture support; rebuild with the `desktop` feature"
                    .to_owned(),
            )
        }
        #[cfg(target_os = "linux")]
        Backend::X11 => x11::start(options).await,
        #[cfg(not(target_os = "linux"))]
        _ => unsupported::start().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(session_type: SessionType, desktop: DesktopKind) -> SessionEnv {
        SessionEnv {
            session_id: "2".to_owned(),
            session_type,
            wayland_display: (session_type == SessionType::Wayland).then(|| "wayland-0".to_owned()),
            x11_display: (session_type == SessionType::X11).then(|| ":0".to_owned()),
            runtime_dir: "/run/user/1000".into(),
            bus_address: "unix:path=/run/user/1000/bus".to_owned(),
            desktop,
            desktop_raw: desktop.label().to_owned(),
            leader_pid: 1234,
            seat: "seat0".to_owned(),
            source: session_env::SessionEnvSource::Logind,
        }
    }

    #[test]
    fn auto_backend_follows_the_session_type() {
        assert_eq!(
            select_backend(
                BackendChoice::Auto,
                &env(SessionType::X11, DesktopKind::Other)
            ),
            Backend::X11
        );
        assert_eq!(
            select_backend(
                BackendChoice::Auto,
                &env(SessionType::Wayland, DesktopKind::Gnome)
            ),
            Backend::Wayland
        );
        assert_eq!(
            select_backend(
                BackendChoice::Auto,
                &env(SessionType::Wayland, DesktopKind::Kde)
            ),
            Backend::Wayland
        );
        assert_eq!(
            select_backend(
                BackendChoice::Auto,
                &env(SessionType::Wayland, DesktopKind::Other)
            ),
            Backend::Wlroots
        );
    }

    #[test]
    fn explicit_backends_ignore_the_session_type() {
        for (choice, expected) in [
            (BackendChoice::Portal, Backend::Wayland),
            (BackendChoice::Wlroots, Backend::Wlroots),
            (BackendChoice::X11, Backend::X11),
        ] {
            for desktop in [DesktopKind::Gnome, DesktopKind::Kde, DesktopKind::Other] {
                for session_type in [SessionType::Wayland, SessionType::X11] {
                    let selection = effective_selection(choice, &env(session_type, desktop));
                    assert_eq!(selection.effective, expected);
                    assert_eq!(selection.requested, expected);
                    assert!(!selection.fallback);
                }
            }
        }
    }

    #[test]
    fn auto_uses_the_native_backend_for_wlroots_desktops() {
        let selection = effective_selection(
            BackendChoice::Auto,
            &env(SessionType::Wayland, DesktopKind::Other),
        );
        assert_eq!(selection.requested, Backend::Wlroots);
        assert_eq!(selection.effective, Backend::Wlroots);
        assert!(!selection.fallback);
        // Portal desktops never report a fallback.
        let selection = effective_selection(
            BackendChoice::Auto,
            &env(SessionType::Wayland, DesktopKind::Kde),
        );
        assert_eq!(selection.effective, Backend::Wayland);
        assert!(!selection.fallback);
    }

    #[test]
    fn window_sources_use_the_portal_on_wlroots_desktops() {
        let session = env(SessionType::Wayland, DesktopKind::Other);
        let selection = effective_selection(BackendChoice::Auto, &session);
        assert_eq!(
            backend_for_source(selection, SourceKind::Monitor, true),
            Ok(Backend::Wlroots)
        );
        assert_eq!(
            backend_for_source(selection, SourceKind::Window, true),
            Ok(Backend::Wayland)
        );
        assert!(backend_for_source(selection, SourceKind::Virtual, false).is_err());
        let portal = effective_selection(BackendChoice::Portal, &session);
        assert_eq!(
            backend_for_source(portal, SourceKind::Window, false),
            Ok(Backend::Wayland)
        );
    }

    #[test]
    fn window_sources_use_the_portal_on_x11_desktops() {
        let session = env(SessionType::X11, DesktopKind::Other);
        let selection = effective_selection(BackendChoice::Auto, &session);
        assert_eq!(selection.effective, Backend::X11);
        assert_eq!(
            backend_for_source(selection, SourceKind::Monitor, true),
            Ok(Backend::X11)
        );
        assert_eq!(
            backend_for_source(selection, SourceKind::Window, true),
            Ok(Backend::Wayland)
        );
        assert!(backend_for_source(selection, SourceKind::Virtual, false).is_err());
    }

    #[test]
    fn backend_choice_parsing_accepts_aliases() {
        for value in ["", " auto ", "AUTO"] {
            assert_eq!(BackendChoice::parse(value), Ok(BackendChoice::Auto));
        }
        assert_eq!(BackendChoice::parse("wayland"), Ok(BackendChoice::Portal));
        assert_eq!(BackendChoice::parse("wlr"), Ok(BackendChoice::Wlroots));
        assert_eq!(BackendChoice::parse("xorg"), Ok(BackendChoice::X11));
        assert!(BackendChoice::parse("mir").is_err());
        assert_eq!(BackendChoice::Auto.as_str(), "auto");
        assert_eq!(BackendChoice::Portal.as_str(), "portal");
        assert_eq!(BackendChoice::Wlroots.as_str(), "wlroots");
        assert_eq!(BackendChoice::X11.as_str(), "x11");
    }
}
