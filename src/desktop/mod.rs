//! Desktop capture and input backends for the native UU host.
//!
//! The host only depends on the contracts in this module. Wayland is
//! implemented with xdg-desktop-portal ScreenCast + RemoteDesktop and
//! PipeWire; an X11 backend can implement the same contract later.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::watch;

pub mod apps;
pub mod audio;
pub mod clipboard;
pub mod desktop_power;
pub mod encoder;
pub mod hevc;
pub mod portal_permissions;
pub mod session_lock;
pub mod video_track;
pub mod virtual_audio;
pub use encoder::FPS;

#[cfg(all(target_os = "linux", feature = "desktop"))]
mod uinput_scroll;
#[cfg(not(target_os = "linux"))]
mod unsupported;
#[cfg(all(target_os = "linux", feature = "desktop"))]
mod wayland;
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
    Wayland,
    X11,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenInfo {
    pub id: u32,
    /// PipeWire node used for portal absolute-pointer input.
    pub node_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
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
        let active = if screens.iter().any(|screen| screen.id == active) {
            active
        } else {
            screens.first().map(|screen| screen.id).unwrap_or(0)
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
        self.active.store(active, Ordering::Release);
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
}

pub struct DesktopSession {
    pub backend: Backend,
    state: Arc<DesktopState>,
    frames: watch::Receiver<Option<CapturedFrame>>,
    input: Arc<dyn InputSink>,
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
    clipboard: std::sync::Mutex<Option<clipboard::ClipboardBridge>>,
}

impl DesktopSession {
    pub fn latest_frame(&self) -> Option<CapturedFrame> {
        self.frames.borrow().clone()
    }

    pub fn input(&self) -> Arc<dyn InputSink> {
        self.input.clone()
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
        self.stop.store(true, Ordering::Release);
        self.task.abort();
    }
}

impl Drop for DesktopSession {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Open the native desktop with default options. X11 is a deliberate stub.
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
        #[cfg(all(target_os = "linux", not(feature = "desktop")))]
        Backend::Wayland => {
            let _ = options;
            Err(
                "this build has no desktop capture support; rebuild with the `desktop` feature"
                    .to_owned(),
            )
        }
        #[cfg(target_os = "linux")]
        Backend::X11 => x11::start().await,
        #[cfg(not(target_os = "linux"))]
        _ => unsupported::start().await,
    }
}

pub fn detect_backend() -> Result<Backend, String> {
    #[cfg(all(target_os = "linux", not(feature = "desktop")))]
    {
        Err(
            "this build has no desktop capture support; rebuild with the `desktop` feature"
                .to_owned(),
        )
    }
    #[cfg(any(not(target_os = "linux"), feature = "desktop"))]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
        {
            return Ok(Backend::Wayland);
        }
        Err(
            "no Wayland session detected; the X11 desktop backend is not implemented yet"
                .to_owned(),
        )
    }
}
