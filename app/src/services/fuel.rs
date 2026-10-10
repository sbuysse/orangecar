//! Fuel wallet — refill detection, price per litre and the euros of fuel left.
//!
//! The OBD fuel level (PID 2F) is smoothed over a minute (median: the gauge
//! sloshes several percent in corners) and compared to a low-watermark kept
//! on disk, so a refill done while the Pi was off is still seen at the next
//! start. A rise of [`REFILL_MIN_RISE_PCT`] held for [`REFILL_CONFIRM_CYCLES`]
//! cycles records a *pending* refill; the dashboard then asks the driver what
//! was paid. Confirmed refills credit the wallet, every trip's cost
//! (`trips.cost_eur`, see `trip.rs`) is debited from it.
//!
//! Lives in `trips.db` next to the trips it is debited by.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use super::obd::ObdService;
use super::settings::SettingsService;

/// Level samples (one per cycle) the median is taken over. The ECU already
/// filters PID 2F heavily (measured: wobble 0.45 pt σ, no change in corners
/// or braking), so a short window is enough.
const WINDOW: usize = 5;
/// Samples needed before the median is trusted after a (re)connect.
const MIN_SAMPLES: usize = 3;
const CYCLE: std::time::Duration = std::time::Duration::from_secs(5);
/// Rise of the smoothed level over the low-watermark that counts as a refill,
/// in percentage points. Replayed over 4 days of car data: the largest
/// non-refill rise was 3.1 pts (a fresh reading after a restart), 3 pts gave 2
/// false alarms, 5 pts none — 6 keeps twice the worst noise.
const REFILL_MIN_RISE_PCT: f64 = 6.0;
/// Consecutive cycles the rise must hold before it is recorded.
const REFILL_CONFIRM_CYCLES: u32 = 2;
/// While a refill is pending, a still-rising gauge extends it for this long
/// after detection. This car reports the new level in one step; other cars
/// fill the needle slowly.
const SETTLE_SECS: i64 = 600;
/// Prices outside this range (€/L) are typing errors or a level estimate gone
/// wrong: the refill still credits the wallet, the price is not used.
const PLAUSIBLE_PRICE: (f64, f64) = (0.8, 3.5);

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Refill {
    pub id: i64,
    pub ts: i64,
    pub level_before: Option<f64>,
    pub level_after: Option<f64>,
    /// Litres from the level rise × tank capacity (None for a manual entry).
    pub litres_est: Option<f64>,
    pub litres: Option<f64>,
    pub amount_eur: Option<f64>,
    pub price_eur_l: Option<f64>,
    pub status: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FuelStatus {
    /// Smoothed fuel level, % (None: the car does not report it, or no OBD).
    pub level_pct: Option<f64>,
    pub litres_in_tank: Option<f64>,
    /// Euros of fuel left: refills entered minus trip costs since the first
    /// one. None until a refill has been entered.
    pub balance_eur: Option<f64>,
    /// Price per litre trips are costed at.
    pub price_eur_l: f64,
    /// True when `price_eur_l` comes from a refill, false = the Settings value.
    pub price_from_refill: bool,
    /// A detected refill waiting for the amount paid.
    pub pending: Option<Refill>,
    /// True when `level_pct` is a live reading; false = the last level seen
    /// (engine off, OBD silent), kept so the range stays on screen.
    pub level_live: bool,
    /// Consumption the range is computed with, L/100 km.
    pub avg_l100: f64,
    /// True when `avg_l100` comes from recent trips, false = the Settings value.
    pub avg_from_trips: bool,
    /// Estimated distance left on the fuel in the tank, km.
    pub range_km: Option<f64>,
}

/// Trips over this window give the average consumption for the range...
const RANGE_WINDOW_SECS: i64 = 30 * 86_400;
/// ...once they add up to at least this much distance with measured fuel.
const RANGE_MIN_KM: f64 = 50.0;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Watermark {
    level_pct: Option<f64>,
    ts: i64,
}

pub struct FuelService {
    db_path: String,
    state_path: PathBuf,
    obd: Arc<ObdService>,
    settings: Arc<SettingsService>,
    pub status: Arc<RwLock<FuelStatus>>,
}

impl FuelService {
    /// `db_path` is the trips database (its `trips` table must exist).
    pub fn new(db_path: String, obd: Arc<ObdService>, settings: Arc<SettingsService>) -> Result<Self> {
        let conn = rusqlite::Connection::open(&db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS fuel_refills (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                level_before REAL,
                level_after REAL,
                litres_est REAL,
                litres REAL,
                amount_eur REAL,
                price_eur_l REAL,
                status TEXT NOT NULL CHECK (status IN ('pending','confirmed','dismissed'))
            );
            CREATE INDEX IF NOT EXISTS idx_fuel_refills_ts ON fuel_refills(ts);",
        )?;
        let state_path = PathBuf::from(&db_path).with_file_name("fuel_state.json");
        Ok(Self {
            db_path,
            state_path,
            obd,
            settings,
            status: Arc::new(RwLock::new(FuelStatus::default())),
        })
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut mark: Watermark = std::fs::read(&me.state_path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            let mut window: VecDeque<f64> = VecDeque::with_capacity(WINDOW);
            let mut rise_cycles = 0u32;
            loop {
                let level = usable_level(&*me.obd.data.read().await);
                // A dead bus, a lost link or a stopped engine restarts the
                // window: last-known values must never be mixed with the next drive's.
                match level {
                    Some(v) => {
                        if window.len() == WINDOW {
                            window.pop_front();
                        }
                        window.push_back(v);
                    }
                    _ => window.clear(),
                }
                let smoothed = (window.len() >= MIN_SAMPLES).then(|| median(&window));
                if let Some(level) = smoothed {
                    me.track_level(level, &mut mark, &mut rise_cycles).await;
                }
                // Without a live reading, the last level seen (the watermark
                // follows the level down, so it is the current one within a
                // point) keeps the range on screen between drives.
                me.refresh(smoothed, mark.level_pct).await;
                tokio::time::sleep(CYCLE).await;
            }
        });
        info!("fuel service started");
    }

    /// Low-watermark bookkeeping and refill detection for one smoothed sample.
    async fn track_level(&self, level: f64, mark: &mut Watermark, rise_cycles: &mut u32) {
        let now = chrono::Utc::now().timestamp();
        let Some(low) = mark.level_pct else {
            *mark = Watermark { level_pct: Some(level), ts: now };
            self.save_mark(mark).await;
            return;
        };
        match classify(low, level) {
            Step::Rise => {
                *rise_cycles += 1;
                if *rise_cycles >= REFILL_CONFIRM_CYCLES {
                    *rise_cycles = 0;
                    let tank = self.settings.settings.read().await.fuel_tank_l;
                    match self.db(move |c| record_refill(c, now, low, level, tank)).await {
                        Some(id) => info!("refill detected #{id}: {low:.0}% → {level:.0}%"),
                        None => warn!("refill detected ({low:.0}% → {level:.0}%) but not stored"),
                    }
                    *mark = Watermark { level_pct: Some(level), ts: now };
                    self.save_mark(mark).await;
                }
            }
            Step::Lower => {
                *rise_cycles = 0;
                // Persist only whole-point moves: a few writes per tank, not one per cycle.
                let moved = (low - level) >= 0.5;
                mark.level_pct = Some(level);
                mark.ts = now;
                if moved {
                    self.save_mark(mark).await;
                }
            }
            Step::Noise => *rise_cycles = 0,
        }
        // A pending refill follows a needle that is still climbing.
        let tank = self.settings.settings.read().await.fuel_tank_l;
        self.db(move |c| extend_pending(c, now, level, tank)).await;
    }

    async fn save_mark(&self, mark: &Watermark) {
        let path = self.state_path.clone();
        let Ok(data) = serde_json::to_vec(mark) else { return };
        let res = tokio::task::spawn_blocking(move || super::settings::write_durably(&path, &data)).await;
        if !matches!(res, Ok(Ok(()))) {
            warn!("fuel watermark not saved: {res:?}");
        }
    }

    /// Recompute the published status from the database.
    async fn refresh(&self, live: Option<f64>, last_seen: Option<f64>) {
        let (tank, fallback, l100_setting) = {
            let s = self.settings.settings.read().await;
            (s.fuel_tank_l, s.fuel_price_eur_l, s.fuel_l100)
        };
        let since = chrono::Utc::now().timestamp() - RANGE_WINDOW_SECS;
        let Some((balance, price, pending, lpp, recent)) =
            self.db(move |c| wallet(c, tank).map(|(b, p, q, l)| (b, p, q, l, recent_consumption(c, since)))).await
        else {
            return;
        };
        let level = live.or(last_seen);
        let (avg_l100, from_trips) = match recent {
            Some((litres, km)) if km >= RANGE_MIN_KM && litres > 0.0 => (100.0 * litres / km, true),
            _ => (l100_setting, false),
        };
        let litres = level.map(|l| l * lpp);
        let mut st = self.status.write().await;
        st.level_pct = level.map(|l| l.round());
        st.level_live = live.is_some();
        st.litres_in_tank = litres.map(round1);
        st.avg_l100 = round1(avg_l100);
        st.avg_from_trips = from_trips;
        st.range_km = litres.filter(|_| avg_l100 > 0.5).map(|l| (l / avg_l100 * 100.0).round());
        st.balance_eur = balance.map(round2);
        st.price_from_refill = price.is_some();
        st.price_eur_l = price.unwrap_or(fallback);
        st.pending = pending;
    }

    /// Price per litre a trip segment is costed at right now.
    pub async fn price_eur_l(&self) -> f64 {
        self.status.read().await.price_eur_l.max(0.0)
    }

    /// Enter the amount paid: confirms the pending refill `id`, or records a
    /// manual one when `id` is None. `litres` overrides the level estimate.
    pub async fn confirm(&self, id: Option<i64>, amount_eur: f64, litres: Option<f64>) -> Result<Refill, String> {
        if !(amount_eur.is_finite() && amount_eur > 0.0 && amount_eur < 1000.0) {
            return Err("amount must be between 0 and 1000 €".into());
        }
        let litres = litres.filter(|l| l.is_finite() && *l > 0.0 && *l < 200.0);
        let now = chrono::Utc::now().timestamp();
        let res = self
            .db(move |c| Some(confirm_refill(c, id, now, amount_eur, litres)))
            .await
            .unwrap_or_else(|| Err("database unavailable".into()));
        if res.is_ok() {
            // Bind first: a guard in the argument would live across refresh()'s write lock.
            let level = self.status.read().await.level_pct;
            self.refresh(None, level).await;
        }
        res
    }

    pub async fn dismiss(&self, id: i64) -> bool {
        let ok = self
            .db(move |c| {
                c.execute("UPDATE fuel_refills SET status='dismissed' WHERE id=?1 AND status='pending'", [id])
                    .ok()
                    .map(|n| n == 1)
            })
            .await
            .unwrap_or(false);
        if ok {
            // Bind first: a guard in the argument would live across refresh()'s write lock.
            let level = self.status.read().await.level_pct;
            self.refresh(None, level).await;
        }
        ok
    }

    pub async fn recent_refills(&self, limit: u32) -> Vec<Refill> {
        self.db(move |c| {
            let mut st = c
                .prepare(&format!("SELECT {REFILL_COLS} FROM fuel_refills WHERE status='confirmed' ORDER BY ts DESC LIMIT ?1"))
                .ok()?;
            let rows = st.query_map([limit], row_to_refill).ok()?.filter_map(|r| r.ok()).collect();
            Some(rows)
        })
        .await
        .unwrap_or_default()
    }

    async fn db<T: Send + 'static>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> Option<T> + Send + 'static,
    ) -> Option<T> {
        let path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open(&path).ok()?;
            conn.busy_timeout(std::time::Duration::from_secs(5)).ok()?;
            f(&conn)
        })
        .await
        .ok()
        .flatten()
    }
}

/// Below this the engine is not running.
const MIN_RUNNING_RPM: f64 = 300.0;

/// The fuel level if it can be trusted: live data, engine running, 0 < level ≤ 100.
/// At key-off the ECU answers 0 % for a moment (car log:
/// 41 2F 00 between readings of 87 %, with MAP at 100 kPa = engine stopped);
/// three such samples in a fresh window would pull the watermark down and the
/// next real reading would look like a refill.
fn usable_level(o: &super::obd::ObdData) -> Option<f64> {
    if !o.is_connected || o.stale || o.rpm < MIN_RUNNING_RPM {
        return None;
    }
    o.values.get("fuel").copied().filter(|v| *v > 0.0 && *v <= 100.0)
}

#[derive(Debug, PartialEq)]
enum Step {
    /// Clearly above the low-watermark: a refill if it holds.
    Rise,
    /// Below the watermark: fuel was burned, the watermark follows.
    Lower,
    /// Within slosh range above the watermark.
    Noise,
}

fn classify(low: f64, level: f64) -> Step {
    if level - low >= REFILL_MIN_RISE_PCT {
        Step::Rise
    } else if level < low {
        Step::Lower
    } else {
        Step::Noise
    }
}

fn median(w: &VecDeque<f64>) -> f64 {
    let mut v: Vec<f64> = w.iter().copied().collect();
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

/// Litres per gauge point: the median of confirmed refills where the driver
/// typed the litres and the gauge rose at least 20 points, else tank / 100.
/// The first real refill (2026-10-07: 55 L for 15.3 -> 97.3 %, 0.67 L/pt)
/// contradicted a fitted non-linear curve, so the car's own refills decide.
fn litres_per_point(c: &rusqlite::Connection, tank: f64) -> f64 {
    let learned: Vec<f64> = c
        .prepare(
            "SELECT litres / (level_after - level_before) FROM fuel_refills
             WHERE status='confirmed' AND litres IS NOT NULL
               AND level_before IS NOT NULL AND level_after - level_before >= 20
             ORDER BY ts DESC LIMIT 5",
        )
        .and_then(|mut st| st.query_map([], |r| r.get::<_, f64>(0))?.collect())
        .unwrap_or_default();
    let mut v: Vec<f64> = learned.into_iter().filter(|x| x.is_finite() && *x > 0.1 && *x < 2.0).collect();
    if v.is_empty() {
        return tank / 100.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

const REFILL_COLS: &str = "id, ts, level_before, level_after, litres_est, litres, amount_eur, price_eur_l, status";

fn row_to_refill(r: &rusqlite::Row) -> rusqlite::Result<Refill> {
    Ok(Refill {
        id: r.get(0)?,
        ts: r.get(1)?,
        level_before: r.get(2)?,
        level_after: r.get(3)?,
        litres_est: r.get(4)?,
        litres: r.get(5)?,
        amount_eur: r.get(6)?,
        price_eur_l: r.get(7)?,
        status: r.get(8)?,
    })
}

fn pending_refill(c: &rusqlite::Connection) -> Option<Refill> {
    c.query_row(
        &format!("SELECT {REFILL_COLS} FROM fuel_refills WHERE status='pending' ORDER BY ts DESC LIMIT 1"),
        [],
        row_to_refill,
    )
    .ok()
}

/// A new pending refill, or the existing pending one extended: two fills in a
/// row without answering are one amount to ask for.
fn record_refill(c: &rusqlite::Connection, now: i64, before: f64, after: f64, tank: f64) -> Option<i64> {
    if let Some(p) = pending_refill(c) {
        let before = p.level_before.unwrap_or(before);
        let est = round1((after - before).max(0.0) * litres_per_point(c, tank));
        c.execute(
            "UPDATE fuel_refills SET ts=?1, level_after=?2, litres_est=?3 WHERE id=?4",
            rusqlite::params![now, after, est, p.id],
        )
        .ok()?;
        return Some(p.id);
    }
    let est = round1((after - before).max(0.0) * litres_per_point(c, tank));
    c.execute(
        "INSERT INTO fuel_refills (ts, level_before, level_after, litres_est, status) VALUES (?1, ?2, ?3, ?4, 'pending')",
        rusqlite::params![now, before, after, est],
    )
    .ok()?;
    Some(c.last_insert_rowid())
}

fn extend_pending(c: &rusqlite::Connection, now: i64, level: f64, tank: f64) -> Option<()> {
    let p = pending_refill(c)?;
    let (before, after) = (p.level_before?, p.level_after?);
    if level > after && now - p.ts <= SETTLE_SECS {
        let est = round1((level - before) * litres_per_point(c, tank));
        c.execute(
            "UPDATE fuel_refills SET level_after=?1, litres_est=?2 WHERE id=?3",
            rusqlite::params![level, est, p.id],
        )
        .ok()?;
    }
    Some(())
}

fn confirm_refill(
    c: &rusqlite::Connection,
    id: Option<i64>,
    now: i64,
    amount: f64,
    litres: Option<f64>,
) -> Result<Refill, String> {
    let id = match id {
        Some(id) => id,
        None => {
            c.execute("INSERT INTO fuel_refills (ts, status) VALUES (?1, 'pending')", [now])
                .map_err(|e| e.to_string())?;
            c.last_insert_rowid()
        }
    };
    let est: Option<f64> = c
        .query_row("SELECT litres_est FROM fuel_refills WHERE id=?1 AND status='pending'", [id], |r| r.get(0))
        .map_err(|_| "no such pending refill".to_string())?;
    let price = litres
        .or(est)
        .filter(|l| *l > 0.0)
        .map(|l| amount / l)
        .filter(|p| (PLAUSIBLE_PRICE.0..=PLAUSIBLE_PRICE.1).contains(p))
        .map(|p| (p * 1000.0).round() / 1000.0);
    c.execute(
        "UPDATE fuel_refills SET status='confirmed', amount_eur=?1, litres=?2, price_eur_l=?3 WHERE id=?4",
        rusqlite::params![amount, litres, price, id],
    )
    .map_err(|e| e.to_string())?;
    c.query_row(&format!("SELECT {REFILL_COLS} FROM fuel_refills WHERE id=?1"), [id], row_to_refill)
        .map_err(|e| e.to_string())
}

/// (litres, km) of trips since `since` that have measured fuel.
fn recent_consumption(c: &rusqlite::Connection, since: i64) -> Option<(f64, f64)> {
    c.query_row(
        "SELECT COALESCE(SUM(fuel_l), 0), COALESCE(SUM(distance_km), 0) FROM trips
         WHERE start_ts >= ?1 AND fuel_l > 0",
        [since],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

/// (balance €, price €/L of the latest refill that has one, pending refill,
/// litres per gauge point).
fn wallet(c: &rusqlite::Connection, tank: f64) -> Option<(Option<f64>, Option<f64>, Option<Refill>, f64)> {
    let (first, credited): (Option<i64>, Option<f64>) = c
        .query_row(
            "SELECT MIN(ts), SUM(amount_eur) FROM fuel_refills WHERE status='confirmed' AND amount_eur IS NOT NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()?;
    let balance = match (first, credited) {
        (Some(first), Some(credited)) => {
            let spent: f64 = c
                .query_row(
                    "SELECT COALESCE(SUM(cost_eur), 0) FROM trips WHERE start_ts >= ?1",
                    [first],
                    |r| r.get(0),
                )
                .ok()?;
            Some(credited - spent)
        }
        _ => None,
    };
    let price: Option<f64> = c
        .query_row(
            "SELECT price_eur_l FROM fuel_refills WHERE status='confirmed' AND price_eur_l IS NOT NULL
             ORDER BY ts DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    Some((balance, price, pending_refill(c), litres_per_point(c, tank)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE trips (id INTEGER PRIMARY KEY, start_ts INTEGER, end_ts INTEGER,
                                 distance_km REAL, fuel_l REAL NOT NULL DEFAULT 0, cost_eur REAL NOT NULL DEFAULT 0);
             CREATE TABLE fuel_refills (id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
                level_before REAL, level_after REAL, litres_est REAL, litres REAL, amount_eur REAL,
                price_eur_l REAL, status TEXT NOT NULL);",
        )
        .unwrap();
        c
    }

    #[test]
    fn classify_separates_refill_burn_and_slosh() {
        assert_eq!(classify(20.0, 31.0), Step::Rise);
        assert_eq!(classify(20.0, 25.0), Step::Noise);
        assert_eq!(classify(20.0, 19.0), Step::Lower);
    }

    #[test]
    fn key_off_zero_and_stopped_engine_are_not_levels() {
        let mut o = crate::services::obd::ObdData::default();
        o.is_connected = true;
        o.rpm = 800.0;
        o.values.insert("fuel".into(), 87.0);
        assert_eq!(usable_level(&o), Some(87.0));
        o.values.insert("fuel".into(), 0.0);
        assert_eq!(usable_level(&o), None);
        o.values.insert("fuel".into(), 87.0);
        o.rpm = 0.0;
        assert_eq!(usable_level(&o), None);
        o.rpm = 800.0;
        o.stale = true;
        assert_eq!(usable_level(&o), None);
    }

    #[test]
    fn median_ignores_a_slosh_spike() {
        let w: VecDeque<f64> = [40.0, 41.0, 70.0, 39.0, 40.0].into_iter().collect();
        assert_eq!(median(&w), 40.0);
    }

    #[test]
    fn litres_per_point_learns_from_typed_refills_only() {
        let c = db();
        assert_eq!(litres_per_point(&c, 63.0), 0.63);
        // The 2026-10-07 refill: 55 L typed for 15.3 -> 97.3 %.
        let id = record_refill(&c, 1000, 15.3, 97.3, 63.0).unwrap();
        confirm_refill(&c, Some(id), 1100, 99.0, Some(55.0)).unwrap();
        assert!((litres_per_point(&c, 63.0) - 55.0 / 82.0).abs() < 1e-9);
        // An accepted estimate (no typed litres) teaches nothing.
        let id = record_refill(&c, 2000, 20.0, 90.0, 63.0).unwrap();
        confirm_refill(&c, Some(id), 2100, 80.0, None).unwrap();
        assert!((litres_per_point(&c, 63.0) - 55.0 / 82.0).abs() < 1e-9);
        assert_eq!(pending_refill(&c), None);
        let id = record_refill(&c, 3000, 30.0, 80.0, 63.0).unwrap();
        assert_eq!(pending_refill(&c).unwrap().id, id);
        assert_eq!(pending_refill(&c).unwrap().litres_est, Some(round1(50.0 * 55.0 / 82.0)));
    }

    #[test]
    fn recent_consumption_only_counts_trips_with_fuel() {
        let c = db();
        c.execute("INSERT INTO trips VALUES (1, 1000, 2000, 40, 3.2, 5.8)", []).unwrap();
        c.execute("INSERT INTO trips VALUES (2, 3000, 4000, 30, 0, 0)", []).unwrap(); // pre-meter trip
        c.execute("INSERT INTO trips VALUES (3, 10, 20, 99, 9.9, 9.9)", []).unwrap(); // too old
        let (l, km) = recent_consumption(&c, 500).unwrap();
        assert!((l - 3.2).abs() < 1e-9 && (km - 40.0).abs() < 1e-9);
    }

    #[test]
    fn classify_uses_the_replayed_threshold() {
        assert_eq!(classify(15.3, 21.4), Step::Rise);
        assert_eq!(classify(40.0, 43.1), Step::Noise);
    }

    #[test]
    fn detected_refill_is_priced_from_the_amount_and_credits_the_wallet() {
        let c = db();
        let id = record_refill(&c, 1000, 20.0, 80.0, 50.0).unwrap();
        // The needle keeps climbing a little after detection.
        extend_pending(&c, 1100, 84.0, 50.0).unwrap();
        let p = pending_refill(&c).unwrap();
        assert_eq!(p.litres_est, Some(32.0));
        let r = confirm_refill(&c, Some(id), 1200, 57.6, Some(32.0)).unwrap();
        assert_eq!(r.price_eur_l, Some(1.8));
        c.execute("INSERT INTO trips VALUES (1, 2000, 3000, 30, 2.0, 3.6)", []).unwrap();
        // A trip from before the first refill is not debited.
        c.execute("INSERT INTO trips VALUES (2, 500, 900, 10, 1.0, 1.8)", []).unwrap();
        let (bal, price, pending, _) = wallet(&c, 50.0).unwrap();
        assert_eq!(bal.map(round2), Some(54.0));
        assert_eq!(price, Some(1.8));
        assert!(pending.is_none());
    }

    #[test]
    fn an_implausible_price_still_credits_but_is_not_used() {
        let c = db();
        let id = record_refill(&c, 1000, 40.0, 52.0, 50.0).unwrap(); // ~6.5 L estimated
        let r = confirm_refill(&c, Some(id), 1100, 60.0, None).unwrap(); // 10 €/L
        assert_eq!(r.price_eur_l, None);
        let (bal, price, _, _) = wallet(&c, 50.0).unwrap();
        assert_eq!(bal, Some(60.0));
        assert_eq!(price, None);
    }

    #[test]
    fn manual_entry_with_litres_and_second_detection_extends_pending() {
        let c = db();
        let r = confirm_refill(&c, None, 1000, 30.0, Some(16.0)).unwrap();
        assert_eq!(r.price_eur_l, Some(1.875));
        let a = record_refill(&c, 2000, 10.0, 30.0, 50.0).unwrap();
        let b = record_refill(&c, 2100, 30.0, 60.0, 50.0).unwrap();
        assert_eq!(a, b);
        // Learned from the manual refill? No: it has no gauge rise, so tank / 100.
        assert_eq!(pending_refill(&c).unwrap().litres_est, Some(25.0));
        assert!(confirm_refill(&c, Some(999), 3000, 10.0, None).is_err());
    }
}
