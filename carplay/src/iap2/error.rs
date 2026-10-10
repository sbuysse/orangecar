//! Error type for the iAP2 stack.
//!
//! Derived from usenocturne/iap2-rs (`src/error.rs`, GPL-3) — see `mod.rs` for
//! the full licensing note. The `bluer::Error` variant was dropped because this
//! stack is transport-generic (no Bluetooth dependency); all transport failures
//! surface as [`Iap2Error::Io`].

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Iap2Error {
    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Authentication failed: {0}")]
    AuthenticationFailed(String),

    #[error("Invalid packet: {0}")]
    InvalidPacket(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Timeout")]
    Timeout,

    #[error("Connection closed")]
    ConnectionClosed,
}

pub type Result<T> = std::result::Result<T, Iap2Error>;
