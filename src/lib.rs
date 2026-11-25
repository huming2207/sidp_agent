//! Soul Agent's SIDP v1 codec and host-side debug session.
//!
//! The wire format is shared with Soul Injector's `sidp_client` component.
//! Encoders use explicit little-endian fields, never native struct layouts.
pub mod agent;
pub mod codec;
pub mod compile;
pub mod gdb;
pub mod job;
pub mod listener;
pub mod login;
pub mod manage;
pub mod mcp;
pub mod ntp;
pub mod power;
pub mod script;
pub mod session;
pub mod transport;
