//! Native UU Remote WebRTC host/viewer support.
//!
//! This is the official-protocol path: REST room join/create, Socket.IO
//! signaling, ICE/STUN/TURN, DTLS/SCTP data channels, `gv_pb` control
//! messages, and the UU `TERM` terminal framing.

pub mod api;
pub mod desktop;
pub mod host;
pub mod initialization;
pub mod locked_video;
pub mod network_diagnostics;
pub mod rtc;
pub mod rtp_metadata;
pub mod session_ice;
pub mod signal;
pub mod system_actions;
pub mod system_metrics;
pub mod term;
pub mod viewer;
pub mod wire;

/// Version sent as `ConnectOptions.client_version`; keep it at the official
/// client version, not the openuur crate version.
pub const UU_CLIENT_VERSION: &str = "4.43.0";

pub mod video_settings;

pub mod port_mapping;

pub mod clipboard;

pub mod file_transfer;

pub mod kcp;
