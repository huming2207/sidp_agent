//! Soul Agent's SIDP v1 codec and host-side debug session.
//!
//! The wire format is shared with Soul Injector's `sidp_client` component.
//! Encoders use explicit little-endian fields, never native struct layouts.
pub mod codec;
pub mod gdb;
pub mod session;
pub mod transport;
