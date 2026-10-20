//! Non-Linux stub.

pub async fn start() -> Result<super::DesktopSession, String> {
    Err("desktop capture is only implemented on Linux Wayland".to_owned())
}
