//! Car signalling for a cellular call: datagrams to rascar-carplay's
//! call-control socket (carplay/src/stream/call.rs, 127.0.0.1:7703).
//!
//! * `call 1` every [`HEARTBEAT`] while a call is dialing, alerting, active or
//!   held — the same heartbeat a meeting uses. It switches the car to the
//!   telephony stream and creates the `rascar_car_mic_in` source. carplay ends
//!   the call by itself 8 s after the last heartbeat, so a crash never leaves
//!   the car stuck in a phone call.
//! * `ring 1` every [`HEARTBEAT`] while an incoming call rings, `ring 0` when
//!   it stops (handled by carplay once the alert stream exists; ignored
//!   before).
//!
//! Coexistence with a meeting (the dashboard's Meet/Teams frame also sends
//! `call 1` through `/api/call`): the phone wins. While the phone holds the
//! telephony stream, `/api/call` does not relay the meeting's `call 0` (it
//! would hang the phone call up in the car); and when the phone call ends
//! while a meeting is still beating, we do not send `call 0` either, so the
//! meeting keeps its car microphone.

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

pub const CALL_CTRL_ADDR: &str = "127.0.0.1:7703";
pub const HEARTBEAT: Duration = Duration::from_secs(3);
/// carplay's own timeout for a heartbeat-less call (call.rs CALL_TIMEOUT).
const MEETING_TIMEOUT: Duration = Duration::from_secs(8);

static LAST_MEETING_BEAT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Called by `/api/call` for every meeting heartbeat / end.
pub fn note_meeting(active: bool) {
    if let Ok(mut g) = LAST_MEETING_BEAT.lock() {
        *g = active.then(std::time::Instant::now);
    }
}

pub fn meeting_active() -> bool {
    LAST_MEETING_BEAT.lock().ok().and_then(|g| *g).is_some_and(|t| t.elapsed() < MEETING_TIMEOUT)
}

/// Decides which datagrams to send; pure so the schedule is testable.
#[derive(Debug, Default)]
pub struct Signaller {
    call_on: bool,
    ring_on: bool,
    last_call: Option<Instant>,
    last_ring: Option<Instant>,
}

impl Signaller {
    /// The phone currently holds the car's telephony stream.
    pub fn owns_call(&self) -> bool {
        self.call_on
    }

    pub fn step(&mut self, want_call: bool, want_ring: bool, meeting: bool, now: Instant) -> Vec<&'static str> {
        let due = |last: Option<Instant>| last.is_none_or(|t| now.saturating_duration_since(t) >= HEARTBEAT);
        let mut out = Vec::new();
        // Ring first: on answer the car hears "ring 0" before "call 1".
        if want_ring {
            if !self.ring_on || due(self.last_ring) {
                out.push("ring 1");
                self.last_ring = Some(now);
            }
            self.ring_on = true;
        } else if self.ring_on {
            out.push("ring 0");
            self.ring_on = false;
            self.last_ring = None;
        }
        if want_call {
            if !self.call_on || due(self.last_call) {
                out.push("call 1");
                self.last_call = Some(now);
            }
            self.call_on = true;
        } else if self.call_on {
            self.call_on = false;
            self.last_call = None;
            if !meeting {
                out.push("call 0");
            }
        }
        out
    }
}

/// Where the datagrams go; a recorder in tests.
pub trait CarLink: Send {
    fn send(&mut self, msg: &'static str) -> impl std::future::Future<Output = ()> + Send;
}

/// The real link: one loopback UDP socket.
#[derive(Default)]
pub struct UdpCar {
    sock: Option<tokio::net::UdpSocket>,
}

impl CarLink for UdpCar {
    async fn send(&mut self, msg: &'static str) {
        if self.sock.is_none() {
            self.sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.ok();
        }
        if let Some(s) = &self.sock {
            if let Err(e) = s.send_to(msg.as_bytes(), CALL_CTRL_ADDR).await {
                tracing::warn!("phone: '{msg}' to rascar-carplay failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn heartbeat_schedule() {
        let mut s = Signaller::default();
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_millis(secs * 500);
        // Ringing: immediate ring 1, then every 3 s.
        assert_eq!(s.step(false, true, false, at(0)), vec!["ring 1"]);
        assert!(s.step(false, true, false, at(1)).is_empty());
        assert!(s.step(false, true, false, at(5)).is_empty());
        assert_eq!(s.step(false, true, false, at(6)), vec!["ring 1"]);
        // Answered: ring 0 then call 1 in the same step.
        assert_eq!(s.step(true, false, false, at(7)), vec!["ring 0", "call 1"]);
        assert!(s.owns_call());
        assert!(s.step(true, false, false, at(12)).is_empty());
        assert_eq!(s.step(true, false, false, at(13)), vec!["call 1"]);
        // Hung up: call 0 once.
        assert_eq!(s.step(false, false, false, at(14)), vec!["call 0"]);
        assert!(!s.owns_call());
        assert!(s.step(false, false, false, at(20)).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_meeting_keeps_the_car_microphone_after_the_call() {
        let mut s = Signaller::default();
        let t0 = Instant::now();
        assert_eq!(s.step(true, false, true, t0), vec!["call 1"]);
        // The meeting is still beating: no call 0 that would cut it.
        assert!(s.step(false, false, true, t0 + Duration::from_secs(1)).is_empty());
        assert!(!s.owns_call());
    }

    #[test]
    fn meeting_beats_expire() {
        note_meeting(true);
        assert!(meeting_active());
        note_meeting(false);
        assert!(!meeting_active());
    }
}
