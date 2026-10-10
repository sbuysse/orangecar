//! Call state machine, fed by URCs (fast) and `AT+CLCC` snapshots
//! (authoritative). URCs can be lost — SIMCom's routing of URCs across its two
//! AT ports is not verified on our unit — so every state the UI shows must
//! also be reachable from CLCC alone; URCs only make it quicker.
//!
//! Pure: no I/O, time passed in, so the transcripts in the tests drive it.

use std::time::Duration;

use serde::Serialize;
use tokio::time::Instant;

/// How long an ended call stays visible ("Call ended") before `call: null`.
pub const ENDED_LINGER: Duration = Duration::from_secs(3);
/// A call we created from a local dial or a RING may not show up in CLCC for
/// a moment; an empty list inside this window does not end it.
pub const APPEAR_GRACE: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Dialing,
    Alerting,
    Incoming,
    Active,
    Held,
    Ended,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Dialing => "dialing",
            State::Alerting => "alerting",
            State::Incoming => "incoming",
            State::Active => "active",
            State::Held => "held",
            State::Ended => "ended",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    In,
    Out,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

/// One `+CLCC: <id>,<dir>,<stat>,<mode>,<mpty>[,"<number>",<type>[,"<alpha>"]]`
/// line (3GPP TS 27.007 §7.18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClccEntry {
    pub id: u32,
    pub direction: Direction,
    pub state: State,
    pub number: String,
}

/// Parse one `+CLCC:` line; data/fax calls (mode ≠ 0) are not ours → None.
pub fn parse_clcc(line: &str) -> Option<ClccEntry> {
    let rest = line.strip_prefix("+CLCC:")?.trim();
    let f = crate::services::cell::split_fields(rest);
    if f.len() < 5 {
        return None;
    }
    let id: u32 = f[0].parse().ok()?;
    let direction = match f[1].as_str() {
        "0" => Direction::Out,
        "1" => Direction::In,
        _ => return None,
    };
    let state = match f[2].as_str() {
        "0" => State::Active,
        "1" => State::Held,
        "2" => State::Dialing,
        "3" => State::Alerting,
        // 5 = waiting (a second incoming call): shown as incoming.
        "4" | "5" => State::Incoming,
        _ => return None,
    };
    if f[3] != "0" {
        return None; // 0 = voice
    }
    Some(ClccEntry { id, direction, state, number: f.get(5).cloned().unwrap_or_default() })
}

/// Which call of a snapshot the UI follows: the one with the user's attention.
fn focus(entries: &[ClccEntry]) -> Option<&ClccEntry> {
    let rank = |s: State| match s {
        State::Active => 0,
        State::Incoming => 1,
        State::Alerting => 2,
        State::Dialing => 3,
        State::Held => 4,
        State::Ended => 5,
    };
    entries.iter().min_by_key(|e| rank(e.state))
}

#[derive(Debug, Clone)]
pub struct Call {
    /// CLCC index once known (0 until the first snapshot lists the call).
    pub id: u32,
    pub state: State,
    pub direction: Direction,
    pub number: String,
    pub name: Option<String>,
    /// Whether the contact lookup ran for the current number.
    pub name_looked_up: bool,
    pub started_wall: chrono::DateTime<chrono::Utc>,
    pub active_since: Option<Instant>,
    pub muted: bool,
    /// We hung up / declined it ourselves.
    pub local_hangup: bool,
    grace_until: Instant,
    ended_at: Option<Instant>,
    /// Duration frozen at the end.
    final_duration: Option<Duration>,
}

impl Call {
    fn new(id: u32, state: State, direction: Direction, number: String, now: Instant) -> Self {
        Call {
            id,
            state,
            direction,
            number,
            name: None,
            name_looked_up: false,
            started_wall: chrono::Utc::now(),
            active_since: (state == State::Active).then_some(now),
            muted: false,
            local_hangup: false,
            grace_until: now + APPEAR_GRACE,
            ended_at: None,
            final_duration: None,
        }
    }

    pub fn duration(&self, now: Instant) -> Duration {
        self.final_duration.unwrap_or_else(|| self.active_since.map(|t| now.saturating_duration_since(t)).unwrap_or_default())
    }

    pub fn is_live(&self) -> bool {
        self.state != State::Ended
    }
}

/// What gets appended to the dashboard's call history when a call ends.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CallRecord {
    pub number: String,
    pub name: String,
    pub direction: Direction,
    /// Incoming, never answered, not declined by us.
    pub missed: bool,
    pub duration_s: u64,
    /// When the call appeared (RFC 3339, UTC).
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Ring,
    CallerId(String),
    VoiceBegin,
    /// VOICE CALL: END / NO CARRIER / BUSY / NO ANSWER, or the modem is gone.
    Ended,
    /// A full CLCC snapshot.
    Snapshot(Vec<ClccEntry>),
    /// Our ATD was accepted.
    Dialed(String),
    LocalHangup,
    Muted(bool),
}

#[derive(Debug, Default)]
pub struct Tracker {
    pub call: Option<Call>,
}

impl Tracker {
    pub fn live(&self) -> Option<&Call> {
        self.call.as_ref().filter(|c| c.is_live())
    }

    pub fn state(&self) -> Option<State> {
        self.live().map(|c| c.state)
    }

    /// Apply one event. Returns the history record when it ended a call.
    pub fn on_event(&mut self, ev: Event, now: Instant) -> Option<CallRecord> {
        match ev {
            Event::Ring => {
                if self.live().is_none() {
                    self.call = Some(Call::new(0, State::Incoming, Direction::In, String::new(), now));
                }
                None
            }
            Event::CallerId(number) => {
                if let Some(c) = self.call.as_mut().filter(|c| c.is_live() && c.direction == Direction::In) {
                    if c.number.is_empty() && !number.is_empty() {
                        c.number = number;
                        c.name_looked_up = false;
                    }
                }
                None
            }
            Event::VoiceBegin => {
                // With no known call, wait for the CLCC poll the engine runs on
                // every URC rather than invent a direction.
                if let Some(c) = self.call.as_mut().filter(|c| c.is_live()) {
                    set_state(c, State::Active, now);
                }
                None
            }
            Event::Dialed(number) => {
                self.call = Some(Call::new(0, State::Dialing, Direction::Out, number, now));
                None
            }
            Event::LocalHangup => {
                if let Some(c) = self.call.as_mut().filter(|c| c.is_live()) {
                    c.local_hangup = true;
                }
                None
            }
            Event::Muted(on) => {
                if let Some(c) = self.call.as_mut().filter(|c| c.is_live()) {
                    c.muted = on;
                }
                None
            }
            Event::Ended => self.end(now),
            Event::Snapshot(entries) => self.snapshot(entries, now),
        }
    }

    fn snapshot(&mut self, entries: Vec<ClccEntry>, now: Instant) -> Option<CallRecord> {
        let Some(e) = focus(&entries).cloned() else {
            // No call at all: whatever we show is over, unless it was created a
            // moment ago and the modem has not listed it yet.
            let ends = self.live().is_some_and(|c| now >= c.grace_until);
            return if ends { self.end(now) } else { None };
        };
        // A call that just ended may still be listed for a moment: do not
        // resurrect it while "Call ended" is on screen.
        if let Some(c) = self.call.as_ref().filter(|c| !c.is_live()) {
            if c.id == 0 || c.id == e.id {
                return None;
            }
        }
        let mut record = None;
        if let Some(c) = self.call.as_ref().filter(|c| c.is_live()) {
            // A different call took the focus (ours ended and another one came
            // in between two polls): close ours first.
            if c.id != 0 && c.id != e.id {
                record = self.end(now);
                self.call = None;
            }
        }
        match self.call.as_mut().filter(|c| c.is_live()) {
            Some(c) => {
                c.id = e.id;
                c.direction = e.direction;
                if !e.number.is_empty() && c.number != e.number {
                    c.number = e.number;
                    c.name_looked_up = false;
                }
                set_state(c, e.state, now);
            }
            // Listed but unknown to us: the app restarted mid-call, or the URCs
            // went to the other AT port.
            None => self.call = Some(Call::new(e.id, e.state, e.direction, e.number, now)),
        }
        record
    }

    fn end(&mut self, now: Instant) -> Option<CallRecord> {
        let c = self.call.as_mut().filter(|c| c.is_live())?;
        c.final_duration = Some(c.duration(now));
        c.state = State::Ended;
        c.ended_at = Some(now);
        let missed = c.direction == Direction::In && c.active_since.is_none() && !c.local_hangup;
        Some(CallRecord {
            number: c.number.clone(),
            name: c.name.clone().unwrap_or_default(),
            direction: c.direction,
            missed,
            duration_s: c.final_duration.unwrap_or_default().as_secs(),
            at: c.started_wall.to_rfc3339(),
        })
    }

    /// Drop the ended call once it has been shown long enough.
    pub fn tick(&mut self, now: Instant) {
        if let Some(c) = &self.call {
            if c.ended_at.is_some_and(|t| now.saturating_duration_since(t) >= ENDED_LINGER) {
                self.call = None;
            }
        }
    }
}

fn set_state(c: &mut Call, state: State, now: Instant) {
    if state == State::Active && c.active_since.is_none() {
        c.active_since = Some(now);
    }
    c.state = state;
}

/// Same phone number, tolerant of the formats in contacts.json vs CLIP:
/// "+447700900101", "00447700900101" and "07700900101" are the same line.
pub fn same_number(a: &str, b: &str) -> bool {
    let digits = |s: &str| -> String {
        let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
        d.strip_prefix("00").map(str::to_string).unwrap_or(d)
    };
    let (a, b) = (digits(a), digits(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    // National vs international form: compare the subscriber part.
    a.len() >= 9 && b.len() >= 9 && a[a.len() - 9..] == b[b.len() - 9..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clcc(lines: &[&str]) -> Vec<ClccEntry> {
        lines.iter().filter_map(|l| parse_clcc(l)).collect()
    }

    #[test]
    fn clcc_lines_parse() {
        let e = parse_clcc("+CLCC: 1,0,3,0,0,\"+447700900101\",145").unwrap();
        assert_eq!((e.id, e.direction, e.state, e.number.as_str()), (1, Direction::Out, State::Alerting, "+447700900101"));
        let e = parse_clcc("+CLCC: 2,1,4,0,0,\"\",128").unwrap();
        assert_eq!((e.direction, e.state, e.number.as_str()), (Direction::In, State::Incoming, ""));
        assert_eq!(parse_clcc("+CLCC: 1,1,5,0,0").unwrap().state, State::Incoming);
        assert!(parse_clcc("+CLCC: 1,0,0,1,0,\"x\",129").is_none(), "data call");
        assert!(parse_clcc("+CLCC: 1,0").is_none());
        assert!(parse_clcc("+CLCC: 1,0,9,0,0").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn outgoing_call_answered_then_remote_hangup() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.on_event(Event::Dialed("+447700900101".into()), t0);
        assert_eq!(t.state(), Some(State::Dialing));
        // Not yet listed: the grace keeps it.
        assert!(t.on_event(Event::Snapshot(vec![]), t0 + Duration::from_secs(1)).is_none());
        assert_eq!(t.state(), Some(State::Dialing));
        t.on_event(Event::Snapshot(clcc(&["+CLCC: 1,0,3,0,0,\"+447700900101\",145"])), t0 + Duration::from_secs(2));
        assert_eq!(t.state(), Some(State::Alerting));
        t.on_event(Event::VoiceBegin, t0 + Duration::from_secs(5));
        assert_eq!(t.state(), Some(State::Active));
        let rec = t.on_event(Event::Ended, t0 + Duration::from_secs(65)).unwrap();
        assert_eq!(rec.direction, Direction::Out);
        assert!(!rec.missed);
        assert_eq!(rec.duration_s, 60);
        // A second end does not log twice.
        assert!(t.on_event(Event::Ended, t0 + Duration::from_secs(66)).is_none());
        // Still listed for a moment after VOICE CALL: END: no resurrection.
        t.on_event(Event::Snapshot(clcc(&["+CLCC: 1,0,0,0,0,\"+447700900101\",145"])), t0 + Duration::from_secs(66));
        assert_eq!(t.call.as_ref().unwrap().state, State::Ended);
        t.tick(t0 + Duration::from_secs(67));
        assert!(t.call.is_some(), "ended lingers 3 s");
        t.tick(t0 + Duration::from_secs(68));
        assert!(t.call.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn incoming_call_missed_and_declined() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.on_event(Event::Ring, t0);
        t.on_event(Event::CallerId("+447700900202".into()), t0);
        let c = t.live().unwrap();
        assert_eq!((c.state, c.direction, c.number.as_str()), (State::Incoming, Direction::In, "+447700900202"));
        // RING repeats: same call, caller id kept.
        t.on_event(Event::Ring, t0 + Duration::from_secs(3));
        assert_eq!(t.live().unwrap().number, "+447700900202");
        // The caller gives up; the URC is lost, CLCC goes empty after the grace.
        let rec = t.on_event(Event::Snapshot(vec![]), t0 + Duration::from_secs(20)).unwrap();
        assert!(rec.missed);
        assert_eq!(rec.number, "+447700900202");

        // Declined by us: not "missed".
        let mut t = Tracker::default();
        t.on_event(Event::Ring, t0);
        t.on_event(Event::LocalHangup, t0 + Duration::from_secs(1));
        let rec = t.on_event(Event::Ended, t0 + Duration::from_secs(1)).unwrap();
        assert!(!rec.missed);
    }

    #[tokio::test(start_paused = true)]
    async fn incoming_call_answered() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.on_event(Event::Ring, t0);
        t.on_event(Event::Snapshot(clcc(&["+CLCC: 1,1,4,0,0,\"+447700900202\",145"])), t0);
        t.on_event(Event::VoiceBegin, t0 + Duration::from_secs(4));
        t.on_event(Event::Snapshot(clcc(&["+CLCC: 1,1,0,0,0,\"+447700900202\",145"])), t0 + Duration::from_secs(5));
        assert_eq!(t.state(), Some(State::Active));
        assert_eq!(t.live().unwrap().duration(t0 + Duration::from_secs(14)), Duration::from_secs(10));
        let rec = t.on_event(Event::Snapshot(vec![]), t0 + Duration::from_secs(34)).unwrap();
        assert!(!rec.missed);
        assert_eq!(rec.duration_s, 30);
    }

    #[tokio::test(start_paused = true)]
    async fn clcc_alone_drives_every_state() {
        // No URC at all (they went to the other AT port).
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.on_event(Event::Snapshot(clcc(&["+CLCC: 1,1,4,0,0,\"+44\",129"])), t0);
        assert_eq!(t.state(), Some(State::Incoming));
        t.on_event(Event::Snapshot(clcc(&["+CLCC: 1,1,0,0,0,\"+44\",129"])), t0 + Duration::from_secs(2));
        assert_eq!(t.state(), Some(State::Active));
        t.on_event(Event::Snapshot(clcc(&["+CLCC: 1,1,1,0,0,\"+44\",129"])), t0 + Duration::from_secs(3));
        assert_eq!(t.state(), Some(State::Held));
        assert!(t.on_event(Event::Snapshot(vec![]), t0 + Duration::from_secs(9)).is_some());
        assert_eq!(t.call.as_ref().unwrap().state, State::Ended);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_call_replacing_ours_between_polls_closes_ours() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.on_event(Event::Snapshot(clcc(&["+CLCC: 1,0,0,0,0,\"+4401\",129"])), t0);
        let rec = t.on_event(Event::Snapshot(clcc(&["+CLCC: 2,1,4,0,0,\"+4402\",129"])), t0 + Duration::from_secs(9)).unwrap();
        assert_eq!(rec.number, "+4401");
        let c = t.live().unwrap();
        assert_eq!((c.id, c.state, c.number.as_str()), (2, State::Incoming, "+4402"));
    }

    #[test]
    fn numbers_match_across_formats() {
        assert!(same_number("+447700900101", "07700900101"));
        assert!(same_number("00447700900101", "+44 7700 900101"));
        assert!(!same_number("+447700900101", "+447700900100"));
        assert!(!same_number("", ""));
        assert!(same_number("112", "112"));
        assert!(!same_number("112", "0112"));
    }
}
