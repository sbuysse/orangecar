//! Android Auto head unit for a real phone.
//!
//! The Pi is the head unit: the user's phone, plugged into a USB host port,
//! projects Android Auto to it; the video goes unchanged (H.264) to the
//! dashboard page `aa.html` over a WebSocket and is decoded there with
//! WebCodecs, touches on that page go back to the phone.
//!
//! Layers, each testable without a phone:
//! - [`proto`]  protobuf messages (prost derives, tags from aasdk's aap_protobuf)
//! - [`framing`] frames with per-frame encryption, as aasdk does it
//! - [`tls`]    TLS 1.2 client with the head-unit certificate
//! - [`session`] the protocol state machine, sans-IO
//! - [`touch`]  multi-touch → AA TouchEvent
//! - [`browser`] page protocol + GOP cache
//! - [`usb_host`] AOA switch + bulk endpoints (needs the phone)
//!
//! This is separate from the crate's prober (`main.rs`, `handshake.rs`),
//! which tests OUR bridge and speaks its dialect.

pub mod browser;
pub mod config;
pub mod framing;
pub mod ids;
pub mod proto;
pub mod session;
pub mod tls;
pub mod touch;
pub mod usb_host;
