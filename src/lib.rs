//! Mongoose: a simple TCP/UDP port forwarder.

// Relies on Linux specifics (systemd, IP_PKTINFO, /proc, socket semantics).
#[cfg(not(target_os = "linux"))]
compile_error!("Mongoose currently only supports Linux");

// Modules are gated too, so the error above is the only one reported.
#[cfg(target_os = "linux")]
pub mod cli;
#[cfg(target_os = "linux")]
pub mod conflict;
#[cfg(target_os = "linux")]
pub mod forward;
#[cfg(target_os = "linux")]
pub mod logging;
#[cfg(target_os = "linux")]
pub mod service;
#[cfg(target_os = "linux")]
pub mod spec;
#[cfg(target_os = "linux")]
pub mod sys;
