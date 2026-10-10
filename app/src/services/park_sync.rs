//! Flush everything to disk when the car is put in Park.
//!
//! The car cuts the Pi's power without warning: over 17 measured drives, USB
//! power died 7 s to ~7 min after the last shift into P (most within 7-26 s).
//! Each of those cuts lost the tail of the journal and could lose the last
//! seconds of SQLite / JSON state. A shift INTO Park is the last reliable
//! signal before that cut, so on it we checkpoint the trips database WAL, call
//! sync(2), and flush journald when we are allowed to.
//!
//! Then, once the engine is confirmed off while still in Park, the Pi powers
//! itself off cleanly (sync first). The 12 V socket is switched — one boot per
//! drive on every day of the journal — so the halted board loses power anyway
//! and boots again at the next ignition; a clean halt only beats the cut to it.
//! `RASCAR_PARK_POWEROFF=0` disables the power-off.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::gps::GpsService;
use super::obd::ObdService;

/// P must be reported continuously for this long before we act, so a lever
/// swept through P (or one glitched sentence) does not fire a sync.
const P_HOLD: Duration = Duration::from_secs(1);

/// At most one sync per this interval, however often the lever moves.
const COOLDOWN: Duration = Duration::from_secs(30);

/// How often the current gear is sampled. `$PASCD` arrives at 1 Hz; a finer
/// tick only shortens the delay between the confirming sample and the sync.
const TICK: Duration = Duration::from_millis(250);

/// The engine must be confirmed off, in Park, for this long before power-off:
/// covers a restart right after stopping (forgot something, repositioning).
const OFF_HOLD: Duration = Duration::from_secs(10);

/// OBD silence counts as "ignition off" only if the engine was seen running
/// this recently. If OBD never worked this boot (adapter down), silence proves
/// nothing and the Pi never powers itself off.
const RUNNING_MEMORY: Duration = Duration::from_secs(600);

/// Below this the engine is stopped (the ECU still answers with ignition on).
const STOPPED_RPM: f64 = 50.0;
/// Above this the engine is running.
const RUNNING_RPM: f64 = 400.0;

/// What OBD says about the engine on one tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Engine {
    /// Live data, rpm above `RUNNING_RPM`.
    Running,
    /// Live data, rpm under `STOPPED_RPM` (ignition on, engine off).
    Stopped,
    /// No live data (bus asleep = ignition off, or the adapter is gone).
    Silent,
    /// Live data in between (cranking, stalling): no conclusion.
    Unclear,
}

/// Pure power-off decision: last known gear P and the engine confirmed off
/// for `OFF_HOLD`, at most once.
#[derive(Debug, Default)]
pub struct PowerOffDetector {
    last_gear: Option<char>,
    last_running: Option<Instant>,
    off_since: Option<Instant>,
    fired: bool,
}

impl PowerOffDetector {
    /// Feed the current gear (None = unknown/stale, keeps the last known one:
    /// the head unit may stop sending before the board loses power) and the
    /// engine state; returns true exactly once, when the Pi should power off.
    pub fn observe(&mut self, gear: Option<char>, engine: Engine, now: Instant) -> bool {
        if self.fired {
            return false;
        }
        if let Some(g) = gear {
            self.last_gear = Some(g);
        }
        if engine == Engine::Running {
            self.last_running = Some(now);
        }
        let recently_running = self.last_running.is_some_and(|t| now.saturating_duration_since(t) <= RUNNING_MEMORY);
        let engine_off = recently_running && matches!(engine, Engine::Stopped | Engine::Silent);
        if self.last_gear != Some('P') || !engine_off {
            self.off_since = None;
            return false;
        }
        let since = *self.off_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= OFF_HOLD {
            self.fired = true;
            return true;
        }
        false
    }
}

fn engine_state(o: &super::obd::ObdData) -> Engine {
    if !o.is_connected || o.stale {
        return Engine::Silent;
    }
    match o.values.get("rpm").copied() {
        Some(r) if r > RUNNING_RPM => Engine::Running,
        Some(r) if r < STOPPED_RPM => Engine::Stopped,
        _ => Engine::Unclear,
    }
}

/// Upper bounds on each step's wait. The whole routine stays under ~3 s.
const CHECKPOINT_TIMEOUT: Duration = Duration::from_millis(1200);
const SYNC_TIMEOUT: Duration = Duration::from_millis(1200);
const JOURNAL_TIMEOUT: Duration = Duration::from_millis(600);

/// Pure transition detector: "a known driving gear (R/N/D), then P held for
/// `P_HOLD`", rate-limited to one hit per `COOLDOWN`.
///
/// Unknown/stale gear (`None`) neither arms nor disarms it: it only breaks a
/// running P hold. So a session that starts in P (gear U, then P) never fires,
/// while D -> (5 s feed gap) -> P still does.
#[derive(Debug, Default)]
pub struct ParkDetector {
    /// The last known gear was R, N or D, and no sync has fired since.
    armed: bool,
    /// When the current uninterrupted run of P samples started.
    p_since: Option<Instant>,
    /// When the last sync fired.
    last_fire: Option<Instant>,
}

impl ParkDetector {
    /// Feed the current gear; returns true exactly when a sync should run.
    pub fn observe(&mut self, gear: Option<char>, now: Instant) -> bool {
        match gear {
            Some('R' | 'N' | 'D') => {
                self.armed = true;
                self.p_since = None;
                false
            }
            Some('P') => {
                if !self.armed {
                    return false;
                }
                let since = *self.p_since.get_or_insert(now);
                let held = now.saturating_duration_since(since) >= P_HOLD;
                let cooled = self.last_fire.is_none_or(|t| now.saturating_duration_since(t) >= COOLDOWN);
                // Still in P while cooling down: stay armed, so a re-park
                // inside the cooldown is synced as soon as it expires.
                if held && cooled {
                    self.armed = false;
                    self.last_fire = Some(now);
                    return true;
                }
                false
            }
            _ => {
                self.p_since = None;
                false
            }
        }
    }
}

/// Watch the car's gear and run `prepare_for_power_loss` on every park, then
/// power off once the engine is confirmed off in Park.
pub fn spawn(gps: Arc<GpsService>, obd: Arc<ObdService>, trips_db: String) {
    let poweroff_enabled = std::env::var("RASCAR_PARK_POWEROFF").map(|v| v != "0").unwrap_or(true);
    tokio::spawn(async move {
        let mut detector = ParkDetector::default();
        let mut off = PowerOffDetector::default();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let gear = gps.current_gear().await;
            let engine = engine_state(&*obd.data.read().await);
            let now = Instant::now();
            if detector.observe(gear, now) {
                // Detached: a slow disk must not delay the next gear sample.
                tokio::spawn(prepare_for_power_loss(trips_db.clone()));
            }
            if poweroff_enabled && off.observe(gear, engine, now) {
                info!("park power-off: in P, engine off for {} s — syncing, then powering off", OFF_HOLD.as_secs());
                prepare_for_power_loss(trips_db.clone()).await;
                power_off().await;
            }
        }
    });
    info!(
        "park sync armed (sync on shift into P; power-off when the engine is off in P: {})",
        if poweroff_enabled { "on" } else { "off, RASCAR_PARK_POWEROFF=0" }
    );
}

/// `systemctl poweroff` (through `sudo -n` when not root, as for the journal).
async fn power_off() {
    // SAFETY: geteuid(2) takes no arguments and cannot fail.
    let root = unsafe { libc::geteuid() } == 0;
    let mut cmd = if root {
        tokio::process::Command::new("systemctl")
    } else {
        let mut c = tokio::process::Command::new("sudo");
        c.args(["-n", "systemctl"]);
        c
    };
    let res = cmd
        .arg("poweroff")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(Duration::from_secs(5), res).await {
        Ok(Ok(st)) if st.success() => info!("park power-off: requested"),
        Ok(Ok(st)) => warn!("park power-off: systemctl poweroff exited with {st}"),
        Ok(Err(e)) => warn!("park power-off: cannot run systemctl: {e}"),
        Err(_) => warn!("park power-off: systemctl poweroff did not return in 5 s"),
    }
}

/// Force pending state to disk before the car may cut power. Every step is
/// bounded and runs off the async workers; a step that times out keeps going
/// in its blocking thread, we just stop waiting for it.
pub async fn prepare_for_power_loss(trips_db: String) {
    // 4G data usage counted since its last commit (≤ 60 s); first, so the
    // sync(2) below covers it. Bounded: it must not delay the trips flush.
    let _ = tokio::time::timeout(Duration::from_secs(3), super::datausage::flush_for_power_loss()).await;
    let t = Instant::now();
    let checkpoint = run_bounded(CHECKPOINT_TIMEOUT, move || checkpoint_wal(&trips_db)).await;
    let checkpoint_ms = t.elapsed().as_millis();

    // sync(2) after the checkpoint so the truncated WAL and the database pages
    // it wrote back are covered too (the connection runs synchronous=NORMAL).
    let t = Instant::now();
    let sync = run_bounded(SYNC_TIMEOUT, || {
        // SAFETY: sync(2) takes no arguments and cannot fail.
        unsafe { libc::sync() };
        Ok(String::new())
    })
    .await;
    let sync_ms = t.elapsed().as_millis();

    let t = Instant::now();
    let journal = journal_sync().await;
    let journal_ms = t.elapsed().as_millis();

    let note = |r: &Result<String, String>| match r {
        Ok(s) if s.is_empty() => String::new(),
        Ok(s) => format!(" ({s})"),
        Err(e) => format!(" (FAILED: {e})"),
    };
    info!(
        "park sync: checkpoint {checkpoint_ms} ms{}, sync {sync_ms} ms{}, journal {journal_ms} ms{}",
        note(&checkpoint),
        note(&sync),
        note(&journal)
    );
}

/// Run a blocking job on the blocking pool and wait for it at most `limit`.
async fn run_bounded<F>(limit: Duration, job: F) -> Result<String, String>
where
    F: FnOnce() -> Result<String, String> + Send + 'static,
{
    match tokio::time::timeout(limit, tokio::task::spawn_blocking(job)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => Err(format!("task: {e}")),
        Err(_) => Err(format!("timed out after {} ms", limit.as_millis())),
    }
}

/// `PRAGMA wal_checkpoint(TRUNCATE)` on a fresh connection.
///
/// Fresh, because the recorder and the fuel service open their own short-lived
/// connections; checkpointing is database-wide, so any connection will do. The
/// busy timeout stays below `CHECKPOINT_TIMEOUT` so SQLite gives up on its own
/// rather than leaving a blocking thread behind.
fn checkpoint_wal(path: &str) -> Result<String, String> {
    if !std::path::Path::new(path).exists() {
        return Ok("no trips.db".into());
    }
    let conn = rusqlite::Connection::open(path).map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_millis(800)).map_err(|e| e.to_string())?;
    // Returns (busy, wal frames, frames checkpointed); busy=1 means a reader or
    // writer held the database and the checkpoint did not complete.
    let (busy, log, done): (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(|e| e.to_string())?;
    if busy != 0 {
        return Err(format!("busy, {done}/{log} frames"));
    }
    Ok(String::new())
}

/// `journalctl --sync`, which only root may do: it talks to journald's
/// `io.systemd.journal` varlink socket, mode 0600 root. rascar-app runs as
/// the dashboard user (User= in rascar-app.service), which has passwordless
/// sudo on the car, so it goes through `sudo -n` (fails at once instead of prompting where
/// that is not so). The journal is the record a power cut truncates most.
async fn journal_sync() -> Result<String, String> {
    // SAFETY: geteuid(2) takes no arguments and cannot fail.
    let root = unsafe { libc::geteuid() } == 0;
    let mut cmd = if root {
        tokio::process::Command::new("journalctl")
    } else {
        let mut c = tokio::process::Command::new("sudo");
        c.args(["-n", "journalctl"]);
        c
    };
    let child = cmd
        .arg("--sync")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return Err(e.to_string()),
    };
    match tokio::time::timeout(JOURNAL_TIMEOUT, child.wait()).await {
        Ok(Ok(st)) if st.success() => Ok(String::new()),
        Ok(Ok(st)) => Err(format!("exit {st}")),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => {
            let _ = child.start_kill();
            warn!("journalctl --sync did not finish in {} ms", JOURNAL_TIMEOUT.as_millis());
            Err("timed out".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_off_after_engine_stops_in_park() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut d = PowerOffDetector::default();
        assert!(!d.observe(Some('D'), Engine::Running, at(0)));
        assert!(!d.observe(Some('P'), Engine::Running, at(5)));
        assert!(!d.observe(Some('P'), Engine::Stopped, at(6)));
        assert!(!d.observe(Some('P'), Engine::Stopped, at(15)));
        // Head unit gone (gear unknown), bus asleep: last known gear is P.
        assert!(d.observe(None, Engine::Silent, at(16)));
        assert!(!d.observe(None, Engine::Silent, at(30)), "fires once");
    }

    #[test]
    fn no_power_off_on_a_restart_or_outside_park_or_without_obd_history() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut d = PowerOffDetector::default();
        d.observe(Some('D'), Engine::Running, at(0));
        d.observe(Some('P'), Engine::Stopped, at(1));
        assert!(!d.observe(Some('P'), Engine::Running, at(8)), "restarted within the hold");
        assert!(!d.observe(Some('P'), Engine::Stopped, at(9)));
        assert!(!d.observe(Some('P'), Engine::Stopped, at(18)), "hold restarted at 9 s");
        assert!(d.observe(Some('P'), Engine::Stopped, at(19)));

        // Engine off at a light in D (stop-start): never.
        let mut d = PowerOffDetector::default();
        d.observe(Some('D'), Engine::Running, at(0));
        assert!(!d.observe(Some('D'), Engine::Stopped, at(1)));
        assert!(!d.observe(Some('D'), Engine::Stopped, at(60)));

        // OBD never live this boot: silence proves nothing.
        let mut d = PowerOffDetector::default();
        assert!(!d.observe(Some('P'), Engine::Silent, at(0)));
        assert!(!d.observe(Some('P'), Engine::Silent, at(120)));

        // Engine last seen running more than 10 min ago (parked, ignition on).
        let mut d = PowerOffDetector::default();
        d.observe(Some('D'), Engine::Running, at(0));
        assert!(!d.observe(Some('P'), Engine::Silent, at(700)));
        assert!(!d.observe(Some('P'), Engine::Silent, at(720)));
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Feed `(offset_ms, gear)` samples; return the offsets that fired.
    fn run(samples: &[(u64, Option<char>)]) -> Vec<u64> {
        let t0 = Instant::now();
        let mut d = ParkDetector::default();
        samples.iter().filter(|(t, g)| d.observe(*g, t0 + ms(*t))).map(|(t, _)| *t).collect()
    }

    #[test]
    fn drive_then_park_fires_once_after_the_hold() {
        let fired = run(&[(0, Some('D')), (1000, Some('P')), (1500, Some('P')), (2000, Some('P')), (3000, Some('P'))]);
        assert_eq!(fired, vec![2000]);
    }

    #[test]
    fn reverse_and_neutral_arm_it_too() {
        assert_eq!(run(&[(0, Some('R')), (100, Some('P')), (1100, Some('P'))]), vec![1100]);
        assert_eq!(run(&[(0, Some('N')), (100, Some('P')), (1100, Some('P'))]), vec![1100]);
    }

    #[test]
    fn session_starting_in_park_never_fires() {
        assert!(run(&[(0, None), (1000, Some('P')), (2000, Some('P')), (5000, Some('P'))]).is_empty());
    }

    #[test]
    fn sweeping_through_p_does_not_fire() {
        // D -> P for 0.5 s -> R: the hold was never reached.
        assert!(run(&[(0, Some('D')), (1000, Some('P')), (1500, Some('P')), (1600, Some('R'))]).is_empty());
    }

    #[test]
    fn unknown_gear_breaks_the_hold_but_keeps_the_arming() {
        // D, then a feed gap (None) before P: still a park.
        let fired = run(&[(0, Some('D')), (500, None), (6000, None), (7000, Some('P')), (8000, Some('P'))]);
        assert_eq!(fired, vec![8000]);
        // A gap in the middle of the hold restarts it.
        let fired = run(&[(0, Some('D')), (1000, Some('P')), (1500, None), (1800, Some('P')), (2500, Some('P')), (2800, Some('P'))]);
        assert_eq!(fired, vec![2800]);
    }

    #[test]
    fn re_park_within_cooldown_is_deferred_not_dropped() {
        let fired = run(&[
            (0, Some('D')),
            (1000, Some('P')),
            (2000, Some('P')),  // fires
            (5000, Some('D')),
            (6000, Some('P')),
            (7000, Some('P')),  // held, but only 5 s after the first sync
            (31000, Some('P')), // 29 s: still cooling
            (32000, Some('P')), // 30 s: fires
            (40000, Some('P')), // stays parked: nothing more
        ]);
        assert_eq!(fired, vec![2000, 32000]);
    }

    #[test]
    fn staying_parked_fires_only_once() {
        let mut s = vec![(0, Some('D'))];
        s.extend((1..200).map(|i| (i * 1000, Some('P'))));
        assert_eq!(run(&s), vec![2000]);
    }

    #[test]
    fn unknown_letters_are_ignored() {
        assert!(run(&[(0, Some('U')), (1000, Some('P')), (2000, Some('P'))]).is_empty());
    }

    #[tokio::test]
    async fn checkpoint_truncates_the_wal_and_survives_a_missing_db() {
        let dir = std::env::temp_dir().join(format!("rascar-park-sync-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("trips.db");
        let path = db.to_string_lossy().into_owned();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE t(x); INSERT INTO t VALUES (1);")
            .unwrap();
        let wal = dir.join("trips.db-wal");
        assert!(std::fs::metadata(&wal).unwrap().len() > 0, "test setup: WAL should hold the insert");

        let r = run_bounded(CHECKPOINT_TIMEOUT, {
            let p = path.clone();
            move || checkpoint_wal(&p)
        })
        .await;
        assert_eq!(r, Ok(String::new()));
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0, "TRUNCATE must empty the WAL");
        drop(conn);

        let missing = dir.join("nope.db").to_string_lossy().into_owned();
        assert_eq!(checkpoint_wal(&missing), Ok("no trips.db".into()));
        // The whole routine stays bounded even end to end.
        tokio::time::timeout(Duration::from_secs(4), prepare_for_power_loss(path)).await.expect("park sync not bounded");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
