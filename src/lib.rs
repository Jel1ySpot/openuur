#![allow(clippy::items_after_test_module)]
//! openuur: UU Remote native terminal client and host.
//!
//! The protocol pieces in this crate are based on static analysis of
//! UU Remote 4.43.0.3607 (`GameViewer.exe`, `uuyc-cli.exe`, `streamer.dll`).
//! See `../docs/protocol.md` for the recovered details and their limits.

pub mod api;
pub mod auth;
pub mod config;
pub mod desktop;
pub mod error;
pub mod install;
pub mod terminal;
pub mod util;
pub mod uu_native;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CLIENT_VERSION: &str = "V4.9.1";
