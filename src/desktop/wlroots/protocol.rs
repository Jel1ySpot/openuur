//! Protocol bindings used by the native wlroots backend.
//!
//! wlroots ships `zwp_virtual_keyboard_v1` in its own tree rather than in
//! wlr-protocols, so its interface definitions are generated from the
//! vendored XML. The remaining protocols come from `wayland-protocols-wlr`.

pub(crate) use wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_v1::{
    Mode as PowerMode, ZwlrOutputPowerV1,
};
pub(crate) use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{Event as FrameEvent, Flags as FrameFlags, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
pub(crate) use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

pub(crate) mod virtual_keyboard {
    //! Generated `zwp_virtual_keyboard_v1` client bindings.

    use wayland_client;
    use wayland_client::protocol::*;

    pub(crate) mod __interfaces {
        use wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/virtual-keyboard-unstable-v1.xml");
    }

    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("./protocols/virtual-keyboard-unstable-v1.xml");
}
