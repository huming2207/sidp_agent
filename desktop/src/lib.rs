//! Soul Agent desktop app: sets up Soul Injector debugging and programming
//! without knowledge of the Soul Injector or its SIDP protocol.
//!
//! The device work is done by the `sidp-agent` library; this crate adds
//! profiles, toolchain discovery, debugger snippets and a GPUI interface.
//! Everything except [`ui`] is plain Rust and tested without a display.
#![warn(missing_docs)]

pub mod describe;
pub mod platform;
pub mod ports;
pub mod profile;
pub mod snippets;
pub mod target;
pub mod toolchain;
pub mod ui;
pub mod worker;

pub use ui::run;
