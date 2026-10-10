//! "Give the screen back to the car's native UI, and come back."
//!
//! CarPlay has no phone → car "please show your own UI" request. The phone
//! states resource ownership with `modesChanged`, and handing the screen away
//! is simply: stop our screen stream, `TEARDOWN` it, then announce MainScreen
//! as the accessory's (entity 2, permanentEntity 2). The car shows its native
//! UI. Pressing its CarPlay icon makes it send `requestUI` on the event
//! channel, and we take the screen back with `modesChanged` (MainScreen 1/1)
//! and a fresh screen SETUP (catplay `c2a/proxy_rpc/car_manager.rs:257-305`
//! for the give-away order, `:609-614` + `modes_arbiter.rs:89-111` for the
//! come-back).
//!
//! This module only holds the *desired* owner, fed by:
//!   * rascar-app (dashboard button → `POST /api/carplay/native`), which sends
//!     `screen car` to [`SCREEN_CTRL_ADDR`];
//!   * `screen phone` on the same socket (tests, or a future "back" button),
//!     taking the screen back without waiting for the car's `requestUI`;
//!   * the session itself, on the car's `requestUI` ([`request`]).
//!
//! The session's screen loop (`carplay_session::run_carplay`) watches it and
//! performs the transitions; the owner the car was last TOLD about lives there,
//! next to the `modesChanged` builder, so every body says the same thing.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tracing::{info, warn};

/// UDP, loopback only. Next to the meeting control (7703).
pub const SCREEN_CTRL_ADDR: &str = "127.0.0.1:7704";

/// Who should be showing its UI on the car's main screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenOwner {
    /// Us: the dashboard is streamed to the car.
    Phone,
    /// The car's native UI (radio, navigation, settings).
    Car,
}

/// Kill switch: `RASCAR_SCREEN_HANDBACK=off` ignores every `screen car` and
/// keeps the dashboard on the car screen for the whole session. It is not
/// verified yet that the Jaguar keeps a session alive with no screen stream.
pub fn enabled() -> bool {
    crate::debug::env_flag("RASCAR_SCREEN_HANDBACK", true)
}

fn state() -> &'static watch::Sender<ScreenOwner> {
    static STATE: OnceLock<watch::Sender<ScreenOwner>> = OnceLock::new();
    STATE.get_or_init(|| {
        let (tx, _) = watch::channel(ScreenOwner::Phone);
        let txc = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = listen(txc).await {
                warn!("screen: control listener stopped: {e:#}");
            }
        });
        tx
    })
}

/// The desired owner. The listener starts on first use.
pub fn subscribe() -> watch::Receiver<ScreenOwner> {
    state().subscribe()
}

/// Ask for `owner` from inside the process (the car's `requestUI`, or a new
/// session resetting to the dashboard).
pub fn request(owner: ScreenOwner) {
    state().send_if_modified(|cur| {
        let changed = *cur != owner;
        *cur = owner;
        changed
    });
}

async fn listen(tx: watch::Sender<ScreenOwner>) -> Result<()> {
    let sock = tokio::net::UdpSocket::bind(SCREEN_CTRL_ADDR).await.with_context(|| format!("bind {SCREEN_CTRL_ADDR}"))?;
    info!("screen: listening for screen-owner requests on {SCREEN_CTRL_ADDR}");
    let mut buf = [0u8; 64];
    loop {
        let (n, from) = match sock.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                warn!("screen: recv: {e}");
                continue;
            }
        };
        if !from.ip().is_loopback() {
            continue;
        }
        let Some(owner) = parse(&buf[..n]) else {
            warn!("screen: ignoring control datagram {:?}", String::from_utf8_lossy(&buf[..n]));
            continue;
        };
        if owner == ScreenOwner::Car && !enabled() {
            info!("screen: `screen car` ignored (RASCAR_SCREEN_HANDBACK=off)");
            continue;
        }
        info!("screen: control request → {owner:?}");
        tx.send_if_modified(|cur| {
            let changed = *cur != owner;
            *cur = owner;
            changed
        });
    }
}

/// `screen car` / `screen phone` (anything else is ignored).
pub fn parse(msg: &[u8]) -> Option<ScreenOwner> {
    match std::str::from_utf8(msg).ok()?.trim() {
        "screen car" => Some(ScreenOwner::Car),
        "screen phone" => Some(ScreenOwner::Phone),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, ScreenOwner};

    #[test]
    fn control_messages() {
        assert_eq!(parse(b"screen car"), Some(ScreenOwner::Car));
        assert_eq!(parse(b"screen car\n"), Some(ScreenOwner::Car));
        assert_eq!(parse(b"  screen phone \r\n"), Some(ScreenOwner::Phone));
        assert_eq!(parse(b"screen"), None);
        assert_eq!(parse(b"screen tv"), None);
        assert_eq!(parse(b"SCREEN CAR"), None);
        assert_eq!(parse(b"call 1"), None);
        assert_eq!(parse(&[0xff, 0xfe]), None);
        assert_eq!(parse(b""), None);
    }
}
