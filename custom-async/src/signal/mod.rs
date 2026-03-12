//! Async signal utilities integrated with the custom runtime.
//!
//! - `ctrl_c` provides cross-platform Ctrl-C support.
//! - `unix` exposes Unix-specific signal handling.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{ctrl_c, signal, CtrlC, Signal, SignalKind};
#[cfg(windows)]
pub use windows::{ctrl_c, CtrlC};
