//! Placeholder for the future X11 backend.
//!
//! X11 support can implement the same `DesktopSession`/`InputSink` contract
//! using XShm/XTest without changing the host integration.

pub async fn start() -> Result<super::DesktopSession, String> {
    Err("X11 desktop capture is not implemented yet; use Wayland for now".to_owned())
}
