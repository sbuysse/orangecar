//! Trip recording service — auto-records trips via GPS + OBD, stores in SQLite.
//!
//! A trip starts when speed > 3 km/h and ends after 3 minutes of being idle (speed ≤ 3).
//! Points are recorded every 5 seconds and flushed to disk on every cycle, so a
//! power cut (ignition off) loses at most one interval rather than the whole
//! drive. All SQLite I/O is wrapped in spawn_blocking.

use anyhow::Result;
use chrono::{Local, NaiveDate, TimeZone};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use super::fuel::FuelService;
use super::gps::GpsService;
use super::obd::ObdService;
use super::settings::SettingsService;

#[derive(Debug, Clone, Serialize)]
pub struct TripPoint {
    pub ts: i64,
    pub lat: f64,
    pub lon: f64,
    pub speed: f64,
    pub alt: f64,
    pub rpm: f64,
    pub coolant: f64,
    pub boost: f64,
    pub throttle: f64,
    pub load: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trip {
    pub id: i64,
    pub start_ts: i64,
    pub end_ts: i64,
    pub distance_km: f64,
    pub fuel_l: f64,
    pub cost_eur: f64,
    pub points: Vec<TripPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TripSummary {
    pub id: i64,
    pub start_ts: i64,
    pub end_ts: i64,
    pub distance_km: f64,
    /// Estimated fuel burned (see [`super::obd::fuel_rate_lph`]) and its cost.
    pub fuel_l: f64,
    pub cost_eur: f64,
}

/// The trip being driven, for the car screen.
#[derive(Debug, Clone, Serialize)]
pub struct LiveTrip {
    pub id: i64,
    pub start_ts: i64,
    pub distance_km: f64,
    pub fuel_l: f64,
    pub cost_eur: f64,
}

/// A gap between two recorder cycles longer than this (suspend, stall) is not
/// costed with the fallback consumption: fuel burned in it is unknown.
const MAX_FUEL_DT_SECS: f64 = 15.0;

pub struct TripService {
    pub is_recording: Arc<RwLock<bool>>,
    /// The open trip, None between trips.
    pub live: Arc<RwLock<Option<LiveTrip>>>,
    db_path: String,
    gps: Arc<GpsService>,
    obd: Arc<ObdService>,
}

/// A GPS fix older than this is treated as no fix by the recorder: a frozen
/// gpsd connection keeps reporting the last speed forever, which would keep a
/// trip open (and its buffer growing) indefinitely.
const MAX_FIX_AGE_SECS: f64 = 15.0;

impl TripService {
    /// `retention_days` prunes trips older than that at startup (0 = keep all).
    pub fn new(gps: Arc<GpsService>, obd: Arc<ObdService>, retention_days: u32) -> Result<Self> {
        let db_path = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/root"))
            .join(".config/rascar/trips.db")
            .to_string_lossy()
            .to_string();
        if let Some(parent) = std::path::Path::new(&db_path).parent() {
            std::fs::create_dir_all(parent).ok();
        }

        // Init schema synchronously at startup
        let path = db_path.clone();
        let conn = rusqlite::Connection::open(&path)?;
        // WAL + synchronous=NORMAL: the recorder now commits on every 5s cycle,
        // and the default rollback journal with synchronous=FULL would fsync
        // twice per commit — needless write amplification on an SD card.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS trips (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                start_ts INTEGER NOT NULL,
                end_ts INTEGER NOT NULL,
                distance_km REAL NOT NULL
            );
            CREATE TABLE IF NOT EXISTS trip_points (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                trip_id INTEGER NOT NULL REFERENCES trips(id),
                ts INTEGER NOT NULL,
                lat REAL, lon REAL, speed REAL, alt REAL,
                rpm REAL, coolant REAL, boost REAL, throttle REAL, load REAL
            );
            CREATE INDEX IF NOT EXISTS idx_trip_points_trip_id ON trip_points(trip_id);
            CREATE INDEX IF NOT EXISTS idx_trips_start_ts ON trips(start_ts);",
        )?;

        // Fuel estimate per trip (2026-10-09). Older databases lack the columns.
        for col in ["fuel_l", "cost_eur"] {
            let has: bool = conn
                .prepare("SELECT 1 FROM pragma_table_info('trips') WHERE name = ?1")?
                .exists([col])?;
            if !has {
                conn.execute_batch(&format!("ALTER TABLE trips ADD COLUMN {col} REAL NOT NULL DEFAULT 0"))?;
            }
        }

        prune_old_trips(&conn, retention_days);

        Ok(Self {
            is_recording: Arc::new(RwLock::new(false)),
            live: Arc::new(RwLock::new(None)),
            db_path,
            gps,
            obd,
        })
    }

    /// The SQLite file trips live in (the fuel wallet shares it).
    pub fn db_path(&self) -> String {
        self.db_path.clone()
    }

    pub async fn start(&self, fuel: Arc<FuelService>, settings: Arc<SettingsService>) {
        let is_recording = self.is_recording.clone();
        let live = self.live.clone();
        let db_path = self.db_path.clone();
        let gps = self.gps.clone();
        let obd = self.obd.clone();
        let fuel_svc = fuel;

        tokio::spawn(async move {
            let mut current_trip_id: Option<i64> = None;
            let mut points_buf: Vec<TripPoint> = Vec::new();
            let mut idle_since: Option<std::time::Instant> = None;
            // Distance accumulated so far for the *current* trip, plus the last
            // point already written. Distance must be cumulative across flushes
            // now that points leave the buffer every cycle.
            let mut trip_distance_km = 0.0f64;
            let mut last_written: Option<(f64, f64)> = None;
            let mut trip_start_ts = 0i64;
            let mut totals = FuelTotals::default();
            let mut last_cycle = std::time::Instant::now();
            let mut last_fuel_meter = obd.data.read().await.fuel_used_l;

            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let dt_secs = last_cycle.elapsed().as_secs_f64();
                last_cycle = std::time::Instant::now();

                // Snapshot the fields we need out of the guards, then drop the
                // guards before any `.await` so we never hold a read lock
                // across spawn_blocking / is_recording.write().
                let (speed, lat, lon, alt, has_fix, is_mock, fix_age) = {
                    let g = gps.current.read().await;
                    (g.speed_kmh, g.latitude, g.longitude, g.altitude, g.has_fix, g.is_mock, g.age_secs())
                };
                let (rpm, coolant, boost, throttle, load, obd_live, fuel_meter) = {
                    let o = obd.data.read().await;
                    (o.rpm, o.coolant_temp, o.boost_pressure, o.throttle, o.engine_load, o.is_connected && !o.stale, o.fuel_used_l)
                };
                let now_ts = chrono::Utc::now().timestamp();

                // Only record from a real, *fresh* GPS fix. Mock/no-fix data
                // must never start or extend a trip (fabricated positions would
                // be persisted as real drives), and a frozen gpsd connection
                // keeps has_fix=true with a stale speed forever.
                let usable_fix = has_fix && !is_mock && fix_age <= MAX_FIX_AGE_SECS;

                // Fuel of the cycle that just elapsed, for the open trip
                // (idling at a light burns fuel too): the OBD fuel meter's
                // advance, integrated on every air-flow sample (obd::fuel_rate_lph).
                // Without live OBD, the distance at the Settings consumption.
                let meter_delta = (fuel_meter - last_fuel_meter).max(0.0);
                last_fuel_meter = fuel_meter;
                if current_trip_id.is_some() {
                    let litres = if obd_live {
                        meter_delta
                    } else if usable_fix && speed > 3.0 && dt_secs <= MAX_FUEL_DT_SECS {
                        let l100 = settings.settings.read().await.fuel_l100;
                        speed * dt_secs / 3600.0 * l100 / 100.0
                    } else {
                        0.0
                    };
                    let price = fuel_svc.price_eur_l().await;
                    totals.add(litres, price);
                }
                if speed > 3.0 && usable_fix {
                    idle_since = None;

                    if current_trip_id.is_none() {
                        // Start a new trip
                        let path = db_path.clone();
                        match tokio::task::spawn_blocking(move || {
                            let conn = open_conn(&path)?;
                            conn.execute(
                                "INSERT INTO trips (start_ts, end_ts, distance_km) VALUES (?1, ?1, 0)",
                                rusqlite::params![now_ts],
                            )?;
                            Ok::<i64, rusqlite::Error>(conn.last_insert_rowid())
                        })
                        .await
                        {
                            Ok(Ok(id)) => {
                                current_trip_id = Some(id);
                                trip_distance_km = 0.0;
                                last_written = None;
                                trip_start_ts = now_ts;
                                totals = FuelTotals::default();
                                *is_recording.write().await = true;
                                info!("trip started, id={id}");
                            }
                            Ok(Err(e)) => warn!("failed to start trip: {e}"),
                            Err(e) => warn!("spawn_blocking failed: {e}"),
                        }
                    }

                    let point = TripPoint {
                        ts: now_ts,
                        lat,
                        lon,
                        speed,
                        alt,
                        rpm,
                        coolant,
                        boost,
                        throttle,
                        load,
                    };
                    points_buf.push(point);

                    // Flush every cycle: an ignition cut is the normal way a
                    // drive ends, so anything still only in RAM is lost.
                    if let Some(trip_id) = current_trip_id {
                        flush(
                            &db_path,
                            trip_id,
                            &mut points_buf,
                            &mut trip_distance_km,
                            &mut last_written,
                            &totals,
                            now_ts,
                        )
                        .await;
                    }
                } else {
                    // Idle
                    let idle_start = idle_since.get_or_insert_with(std::time::Instant::now);

                    if idle_start.elapsed().as_secs() >= 180 {
                        // 3 min idle — close trip
                        if let Some(trip_id) = current_trip_id.take() {
                            flush(
                                &db_path,
                                trip_id,
                                &mut points_buf,
                                &mut trip_distance_km,
                                &mut last_written,
                                &totals,
                                now_ts,
                            )
                            .await;
                            info!(
                                "trip {trip_id} closed ({trip_distance_km:.2} km, {:.2} L, {:.2} €)",
                                totals.litres, totals.cost_eur
                            );
                            *is_recording.write().await = false;
                        }
                        points_buf.clear();
                        idle_since = None;
                    }
                }

                *live.write().await = current_trip_id.map(|id| LiveTrip {
                    id,
                    start_ts: trip_start_ts,
                    distance_km: (trip_distance_km * 10.0).round() / 10.0,
                    fuel_l: (totals.litres * 100.0).round() / 100.0,
                    cost_eur: (totals.cost_eur * 100.0).round() / 100.0,
                });
            }
        });

        info!("trip service started");
    }

    /// Trips starting on the given *local* calendar day.
    ///
    /// `start_ts` is stored as a UTC epoch, so the requested date is converted
    /// to a local-midnight..next-local-midnight range. Filtering on the UTC day
    /// instead pushes a 00:30 CEST drive into the previous day's bucket.
    pub async fn get_trips_for_date(&self, date: NaiveDate) -> Vec<TripSummary> {
        let path = self.db_path.clone();
        let local_midnight = |d: NaiveDate| -> Option<i64> {
            let naive = d.and_hms_opt(0, 0, 0)?;
            Local
                .from_local_datetime(&naive)
                .earliest()
                .map(|dt| dt.timestamp())
        };
        let start = local_midnight(date).unwrap_or(0);
        let end = date
            .succ_opt()
            .and_then(local_midnight)
            .unwrap_or(i64::MAX);

        tokio::task::spawn_blocking(move || -> Option<Vec<TripSummary>> {
            let conn = rusqlite::Connection::open(&path).ok()?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, start_ts, end_ts, distance_km, fuel_l, cost_eur FROM trips
                     WHERE start_ts >= ?1 AND start_ts < ?2 ORDER BY start_ts",
                )
                .ok()?;
            let rows: Vec<TripSummary> = stmt
                .query_map(rusqlite::params![start, end], |row| {
                    Ok(TripSummary {
                        id: row.get(0)?,
                        start_ts: row.get(1)?,
                        end_ts: row.get(2)?,
                        distance_km: row.get(3)?,
                        fuel_l: row.get(4)?,
                        cost_eur: row.get(5)?,
                    })
                })
                .ok()?
                .filter_map(|r| r.ok())
                .collect();
            Some(rows)
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
    }

    /// Number of trips per *local* day of one month (days without trips are
    /// absent). Same bucketing as [`Self::get_trips_for_date`]: the month is a
    /// local-midnight range and each `start_ts` is mapped to its local date.
    pub async fn get_trip_counts_for_month(
        &self,
        year: i32,
        month: u32,
    ) -> std::collections::BTreeMap<NaiveDate, u32> {
        let Some((start, end)) = month_bounds(&Local, year, month) else {
            return Default::default();
        };
        let path = self.db_path.clone();
        let starts = tokio::task::spawn_blocking(move || -> Option<Vec<i64>> {
            let conn = rusqlite::Connection::open(&path).ok()?;
            let mut stmt = conn
                .prepare("SELECT start_ts FROM trips WHERE start_ts >= ?1 AND start_ts < ?2")
                .ok()?;
            let rows = stmt
                .query_map(rusqlite::params![start, end], |row| row.get::<_, i64>(0))
                .ok()?
                .filter_map(|r| r.ok())
                .collect();
            Some(rows)
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
        count_by_local_day(&Local, &starts)
    }

    pub async fn get_trip_with_points(&self, id: i64) -> Option<Trip> {
        let path = self.db_path.clone();
        tokio::task::spawn_blocking(move || -> Option<Trip> {
            let conn = rusqlite::Connection::open(&path).ok()?;
            let (start_ts, end_ts, distance_km, fuel_l, cost_eur): (i64, i64, f64, f64, f64) = conn
                .query_row(
                    "SELECT start_ts, end_ts, distance_km, fuel_l, cost_eur FROM trips WHERE id = ?1",
                    rusqlite::params![id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .ok()?;

            let mut stmt = conn
                .prepare(
                    "SELECT ts, lat, lon, speed, alt, rpm, coolant, boost, throttle, load
                     FROM trip_points WHERE trip_id = ?1 ORDER BY ts",
                )
                .ok()?;
            let points: Vec<TripPoint> = stmt
                .query_map(rusqlite::params![id], |row| {
                    Ok(TripPoint {
                        ts: row.get(0)?,
                        lat: row.get(1)?,
                        lon: row.get(2)?,
                        speed: row.get(3)?,
                        alt: row.get(4)?,
                        rpm: row.get(5)?,
                        coolant: row.get(6)?,
                        boost: row.get(7)?,
                        throttle: row.get(8)?,
                        load: row.get(9)?,
                    })
                })
                .ok()?
                .filter_map(|r| r.ok())
                .collect();

            Some(Trip {
                id,
                start_ts,
                end_ts,
                distance_km,
                fuel_l,
                cost_eur,
                points,
            })
        })
        .await
        .ok()
        .flatten()
    }
}

/// Open a write connection with the per-connection pragmas set.
///
/// `journal_mode` is persisted in the database file, but `synchronous` is a
/// per-connection setting and must be re-applied on every open.
fn open_conn(path: &str) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

/// Persist the buffered points, extend the running distance and update the trip
/// summary. On success the buffer is emptied; on failure the points stay
/// buffered and are retried on the next cycle.
async fn flush(
    db_path: &str,
    trip_id: i64,
    points_buf: &mut Vec<TripPoint>,
    distance_km: &mut f64,
    last_written: &mut Option<(f64, f64)>,
    fuel: &FuelTotals,
    end_ts: i64,
) {
    // No new point (closing after the idle wait) still saves the fuel burned
    // while idling; the end time stays the last moving point.

    // Extend the running distance with this batch, chained onto the last point
    // already written so no segment is lost or double-counted across flushes.
    let mut dist = *distance_km;
    let mut prev = *last_written;
    for p in points_buf.iter() {
        if let Some((plat, plon)) = prev {
            dist += haversine(plat, plon, p.lat, p.lon);
        }
        prev = Some((p.lat, p.lon));
    }

    let path = db_path.to_string();
    let pts = points_buf.clone();
    let fuel = fuel.clone();
    let result =
        tokio::task::spawn_blocking(move || save_trip_points(&path, trip_id, &pts, end_ts, dist, &fuel)).await;

    match result {
        Ok(Ok(())) => {
            points_buf.clear();
            *distance_km = dist;
            *last_written = prev;
        }
        Ok(Err(e)) => warn!("failed to flush trip {trip_id}: {e} — retrying next cycle"),
        Err(e) => warn!("spawn_blocking failed: {e}"),
    }
}

fn save_trip_points(
    path: &str,
    trip_id: i64,
    points: &[TripPoint],
    end_ts: i64,
    distance_km: f64,
    fuel: &FuelTotals,
) -> rusqlite::Result<()> {
    let mut conn = open_conn(path)?;

    // One transaction for the whole batch. In autocommit mode each insert is
    // its own transaction with its own fsync — 720 of them per hour of driving
    // on an SD card.
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO trip_points (trip_id, ts, lat, lon, speed, alt, rpm, coolant, boost, throttle, load)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?;
        for p in points {
            stmt.execute(rusqlite::params![
                trip_id, p.ts, p.lat, p.lon, p.speed, p.alt, p.rpm, p.coolant, p.boost, p.throttle, p.load
            ])?;
        }

        // Update trip summary in the same transaction, so the row and its
        // points can never disagree.
        if !points.is_empty() {
            tx.execute(
                "UPDATE trips SET end_ts = ?1, distance_km = ?2 WHERE id = ?3",
                rusqlite::params![end_ts, distance_km, trip_id],
            )?;
        }
        tx.execute(
            "UPDATE trips SET fuel_l = ?1, cost_eur = ?2 WHERE id = ?3",
            rusqlite::params![fuel.litres, fuel.cost_eur, trip_id],
        )?;
    }
    tx.commit()
}

/// Fuel burned in the open trip and what it cost. Each cycle is costed at the
/// price of the moment, so a refill mid-trip changes only what follows it.
#[derive(Debug, Clone, Default)]
struct FuelTotals {
    litres: f64,
    cost_eur: f64,
}

impl FuelTotals {
    fn add(&mut self, litres: f64, price_eur_l: f64) {
        if litres.is_finite() && litres > 0.0 {
            self.litres += litres;
            self.cost_eur += litres * price_eur_l;
        }
    }
}

/// Delete trips (and their points) older than `retention_days`. 0 disables it.
fn prune_old_trips(conn: &rusqlite::Connection, retention_days: u32) {
    if retention_days == 0 {
        return;
    }
    let cutoff = chrono::Utc::now().timestamp() - (retention_days as i64) * 86_400;
    let res = conn.execute_batch(&format!(
        "BEGIN;
         DELETE FROM trip_points WHERE trip_id IN (SELECT id FROM trips WHERE start_ts < {cutoff});
         DELETE FROM trips WHERE start_ts < {cutoff};
         COMMIT;"
    ));
    match res {
        Ok(()) => info!("trip retention sweep done (older than {retention_days} days)"),
        Err(e) => warn!("trip retention sweep failed: {e}"),
    }
}

/// `[local midnight of the 1st, local midnight of the next month's 1st)` as
/// UTC epochs. Generic over the zone so tests can pin one.
fn month_bounds<Tz: TimeZone>(tz: &Tz, year: i32, month: u32) -> Option<(i64, i64)> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)?
    };
    let midnight = |d: NaiveDate| {
        tz.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest().map(|dt| dt.timestamp())
    };
    Some((midnight(first)?, midnight(next)?))
}

/// Count UTC epochs per local calendar date in `tz`.
fn count_by_local_day<Tz: TimeZone>(tz: &Tz, starts: &[i64]) -> std::collections::BTreeMap<NaiveDate, u32> {
    let mut out = std::collections::BTreeMap::new();
    for &ts in starts {
        if let Some(dt) = tz.timestamp_opt(ts, 0).single() {
            *out.entry(dt.date_naive()).or_insert(0) += 1;
        }
    }
    out
}

#[cfg(test)]
mod fuel_rate_tests {
    use super::*;

    #[test]
    fn totals_cost_each_litre_at_its_own_price() {
        let mut t = FuelTotals::default();
        t.add(1.0, 1.80);
        t.add(1.0, 2.00);
        t.add(-3.0, 2.00);
        assert!((t.litres - 2.0).abs() < 1e-9);
        assert!((t.cost_eur - 3.80).abs() < 1e-9);
    }
}

#[cfg(test)]
mod month_count_tests {
    use super::*;
    use chrono::FixedOffset;

    #[test]
    fn a_00_30_cest_trip_counts_on_its_local_day() {
        let cest = FixedOffset::east_opt(2 * 3600).unwrap();
        // 2026-09-01 00:30 CEST = 2026-08-31 22:30 UTC.
        let late = cest.with_ymd_and_hms(2026, 9, 1, 0, 30, 0).unwrap().timestamp();
        let noon = cest.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap().timestamp();
        let other = cest.with_ymd_and_hms(2026, 9, 17, 8, 0, 0).unwrap().timestamp();
        let c = count_by_local_day(&cest, &[late, noon, other]);
        assert_eq!(c.get(&NaiveDate::from_ymd_opt(2026, 9, 1).unwrap()), Some(&2));
        assert_eq!(c.get(&NaiveDate::from_ymd_opt(2026, 9, 17).unwrap()), Some(&1));
        assert_eq!(c.get(&NaiveDate::from_ymd_opt(2026, 8, 31).unwrap()), None);
    }

    #[test]
    fn month_bounds_are_local_midnights_and_wrap_december() {
        let cest = FixedOffset::east_opt(2 * 3600).unwrap();
        let (a, b) = month_bounds(&cest, 2026, 9).unwrap();
        assert_eq!(a, cest.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap().timestamp());
        assert_eq!(b, cest.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap().timestamp());
        let (_, dec_end) = month_bounds(&cest, 2026, 12).unwrap();
        assert_eq!(dec_end, cest.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap().timestamp());
        assert!(month_bounds(&cest, 2026, 13).is_none());
    }
}

fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6371.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    R * 2.0 * a.sqrt().asin()
}
