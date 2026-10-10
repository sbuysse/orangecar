//! Car media keys → rascar-app.
//!
//! The steering-wheel / head-unit media buttons reach us as `hidSendReport`
//! from the car's "Consumer keypad" HID device (decoded in `airplay::hid`).
//! The dashboard owns every player (radio and TV play in the page, YouTube
//! Music in an mpv the app drives), so a key is relayed to rascar-app as one
//! UDP datagram `key <name>` — the same loopback pattern as the touch echo
//! (7702), the meeting control (7703) and the screen control (7704). The app
//! pushes it to the page over its WebSocket, which applies it to the source
//! that is playing.

use crate::airplay::hid::CarKey;

/// UDP, loopback only. Next to the screen control (7704).
pub const CAR_KEY_ADDR: &str = "127.0.0.1:7705";

/// The datagram rascar-app parses (`app/src/main.rs` `parse_car_key`).
pub fn datagram(key: CarKey) -> String {
    format!("key {}", key.wire_name())
}

/// Fire-and-forget. Returns whether the datagram left; nobody listening (app
/// down) is not an error for the CarPlay session.
pub fn send(key: CarKey) -> bool {
    use std::sync::OnceLock;
    static SOCK: OnceLock<Option<std::net::UdpSocket>> = OnceLock::new();
    let sock = SOCK.get_or_init(|| {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").ok()?;
        s.set_nonblocking(true).ok()?;
        Some(s)
    });
    sock.as_ref().is_some_and(|s| s.send_to(datagram(key).as_bytes(), CAR_KEY_ADDR).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagrams_match_what_the_app_accepts() {
        // The app whitelists exactly these names (app/src/main.rs CAR_KEYS).
        // Hook/Drop replaced the merged "hangup" when modem calls arrived; the
        // app also still accepts "hangup" from an older carplay binary.
        let all = [CarKey::Play, CarKey::Pause, CarKey::PlayPause, CarKey::Next, CarKey::Previous, CarKey::Stop, CarKey::Hook, CarKey::Drop];
        let got: Vec<String> = all.iter().map(|k| datagram(*k)).collect();
        assert_eq!(got, ["key play", "key pause", "key play-pause", "key next", "key previous", "key stop", "key hook", "key drop"]);
    }
}
