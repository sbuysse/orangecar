//! GPS service — reads position from gpsd.
//!
//! Provides real-time position, speed, altitude, bearing,
//! history for charts, and auto trip recording.

use anyhow::Result;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

const HISTORY_LEN: usize = 600;

/// A gpsd socket that stays open but silent for this long is treated as dead:
/// `next_line()` on a frozen connection never returns, which would pin the
/// supervisor (no reconnect, no mock fallback) and leave `has_fix: true` with
/// the last speed forever.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Clone, Default, Serialize)]
pub struct GpsData {
    pub latitude: f64,
    pub longitude: f64,
    pub speed_kmh: f64,
    pub altitude: f64,
    pub bearing: f64,
    pub accuracy: f64,
    pub has_fix: bool,
    /// True when the position was produced by the fallback mock generator
    /// (gpsd unavailable) rather than a real GPS receiver. Consumers that
    /// persist or act on position data (e.g. the trip recorder) must ignore
    /// mock data.
    pub is_mock: bool,
    /// Unix timestamp (seconds) of the last update, 0 if never updated.
    /// Consumers must treat an old fix as no fix — see `age_secs`.
    pub last_update: f64,
    /// Satellites in view from the car's `$GPGSV` cycle; `None` when no
    /// complete cycle has been received (GSV not offered or not requested).
    pub sats_in_view: Option<u16>,
    /// Satellites used in the fix (`$GPGGA` field 7). The car's fix is fused
    /// with wheel speed, so this is a GNSS-quality hint, not an accuracy.
    pub sats_used: Option<u16>,
    /// One entry per satellite of the last complete `$GPGSV` cycle.
    pub satellites: Vec<Satellite>,
}

/// One satellite from a `$GPGSV` sentence. Empty NMEA fields are `None`
/// (an empty SNR means "in view, not tracked").
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Satellite {
    pub prn: u16,
    #[serde(rename = "el")]
    pub elevation: Option<u8>,
    #[serde(rename = "az")]
    pub azimuth: Option<u16>,
    /// C/N0 in dB-Hz (00-99).
    pub snr: Option<u8>,
}

impl GpsData {
    /// Seconds since this fix was produced. `f64::MAX` when there has never
    /// been one, so a "too old?" test naturally rejects it.
    pub fn age_secs(&self) -> f64 {
        if self.last_update <= 0.0 {
            return f64::MAX;
        }
        (now_secs() - self.last_update).max(0.0)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub t: f64,
    pub v: f64,
    /// True when this sample came from the mock generator rather than a real
    /// receiver, so a chart can mark the fabricated stretch instead of
    /// presenting it as recorded driving.
    pub mock: bool,
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// The last gear lever position the car reported in `$PASCD`, and when.
/// Kept apart from `GpsData` because gpsd and the mock overwrite that struct
/// wholesale, which would wipe a gear only the car feed knows.
#[derive(Debug, Clone, Copy)]
pub struct GearSample {
    /// 'P', 'R', 'N', 'D' or 'U' (unknown, sent at session start).
    pub gear: char,
    /// Monotonic: a wall-clock jump at boot (RTC/NTP) must not revive or
    /// expire a reading.
    pub at: std::time::Instant,
}

/// A gear reading older than this is no reading: the car sends `$PASCD` at
/// 1 Hz, so 5 s of silence means the session is gone.
pub const GEAR_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// The gear a sample stands for at `now`: `None` when stale or unknown ('U').
pub fn effective_gear(sample: Option<GearSample>, now: std::time::Instant) -> Option<char> {
    let s = sample?;
    if now.saturating_duration_since(s.at) > GEAR_STALE_AFTER || s.gear == 'U' {
        return None;
    }
    Some(s.gear)
}

pub struct GpsService {
    pub current: Arc<RwLock<GpsData>>,
    pub speed_history: Arc<RwLock<VecDeque<HistoryEntry>>>,
    pub altitude_history: Arc<RwLock<VecDeque<HistoryEntry>>>,
    /// Last `$PASCD` gear from the car; read it through `current_gear()`.
    pub gear: Arc<RwLock<Option<GearSample>>>,
}

impl GpsService {
    pub fn new() -> Self {
        Self {
            current: Arc::new(RwLock::new(GpsData::default())),
            speed_history: Arc::new(RwLock::new(VecDeque::new())),
            altitude_history: Arc::new(RwLock::new(VecDeque::new())),
            gear: Arc::new(RwLock::new(None)),
        }
    }

    /// The car's current gear ('P'/'R'/'N'/'D'), or `None` when unknown or
    /// when no `$PASCD` arrived for `GEAR_STALE_AFTER`.
    pub async fn current_gear(&self) -> Option<char> {
        let sample = *self.gear.read().await;
        effective_gear(sample, std::time::Instant::now())
    }

    pub async fn start(&self) {
        let current = self.current.clone();
        let speed_hist = self.speed_history.clone();
        let alt_hist = self.altitude_history.clone();
        let gear = self.gear.clone();

        tokio::spawn(async move {
            // Supervise the gpsd reader: reconnect with backoff, and after
            // repeated failures fall back to the mock generator for one
            // backoff interval so GPS never silently dies, while still
            // periodically re-attempting gpsd.
            let mut failures: u32 = 0;
            loop {
                // The CAR's GPS first. It arrives over iAP2 as NMEA while a
                // CarPlay session is up, and it is a fused fix (we see GPGGA
                // quality 6 = dead reckoning, 17 satellites), so it keeps working
                // in tunnels and multi-storey car parks where the USB puck
                // cannot. When no session is running the file is absent or goes
                // quiet within seconds and we fall straight through to gpsd.
                match poll_car_gps(&car_gps_path(), current.clone(), speed_hist.clone(), alt_hist.clone(), gear.clone()).await {
                    Ok(true) => {
                        info!("car GPS feed went quiet — falling back to gpsd");
                        failures = 0;
                        continue;
                    }
                    Ok(false) => {}                       // file present, nothing new
                    Err(_) => {}                          // no CarPlay session has run
                }

                // gpsd, the mock and the backoff below all give way as soon as a
                // CarPlay session writes the car feed again.
                let gpsd = tokio::select! {
                    r = poll_gpsd(current.clone(), speed_hist.clone(), alt_hist.clone()) => r,
                    _ = car_feed_resumed() => {
                        info!("car GPS feed resumed — switching to it");
                        continue;
                    }
                };
                match gpsd {
                    // Received data then the stream closed cleanly: gpsd is
                    // healthy, so reset the failure count before reconnecting.
                    Ok(true) => {
                        warn!("gpsd stream closed — reconnecting");
                        failures = 0;
                    }
                    // Connected but never produced a fix.
                    Ok(false) => {
                        warn!("gpsd produced no data — reconnecting");
                        failures += 1;
                    }
                    Err(e) => {
                        warn!("gpsd not available: {e}");
                        failures += 1;
                    }
                }

                // Backoff capped at 30s.
                let backoff = std::cmp::min(2u64.saturating_mul(failures as u64), 30);

                if failures >= 3 {
                    // gpsd is staying down — run the mock generator while we
                    // wait out the backoff, so the UI keeps moving.
                    warn!("gpsd unavailable after {failures} attempts — using mock data");
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(backoff), async {
                        tokio::select! {
                            _ = mock_loop(current.clone(), speed_hist.clone(), alt_hist.clone()) => {}
                            _ = car_feed_resumed() => {}
                        }
                    })
                    .await;
                } else {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(backoff)) => {}
                        _ = car_feed_resumed() => {}
                    }
                }
            }
        });

        info!("GPS service started");
    }
}

/// Connect to gpsd and stream TPV records into shared state.
///
/// Returns `Ok(true)` if at least one record was received before the stream
/// closed, `Ok(false)` if it connected but produced nothing, or `Err` if the
/// connection itself failed.
async fn poll_gpsd(
    current: Arc<RwLock<GpsData>>,
    speed_hist: Arc<RwLock<VecDeque<HistoryEntry>>>,
    alt_hist: Arc<RwLock<VecDeque<HistoryEntry>>>,
) -> Result<bool> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::TcpStream;

    let stream = TcpStream::connect("127.0.0.1:2947").await?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();

    // Enable JSON reporting
    use tokio::io::AsyncWriteExt;
    writer
        .write_all(b"?WATCH={\"enable\":true,\"json\":true}\n")
        .await?;

    info!("connected to gpsd");

    let mut got_data = false;
    loop {
        // Bound the read: a half-open/frozen socket would otherwise park here
        // forever and the supervisor's backoff + mock fallback could never run.
        let line = match tokio::time::timeout(READ_TIMEOUT, lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            // Clean EOF or read error — let the supervisor reconnect.
            Ok(_) => break,
            Err(_) => {
                warn!("gpsd silent for {READ_TIMEOUT:?} — treating connection as dead");
                break;
            }
        };
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
            if json["class"] == "TPV" {
                got_data = true;
                let now = now_secs();

                // Horizontal accuracy: prefer gpsd's `eph` (estimated
                // horizontal position error, metres) when present, else derive
                // it from the `epx`/`epy` latitude/longitude error components.
                let accuracy = json["eph"].as_f64().or_else(|| {
                    match (json["epx"].as_f64(), json["epy"].as_f64()) {
                        (Some(x), Some(y)) => Some((x * x + y * y).sqrt()),
                        (Some(x), None) => Some(x),
                        (None, Some(y)) => Some(y),
                        _ => None,
                    }
                });

                let data = GpsData {
                    latitude: json["lat"].as_f64().unwrap_or(0.0),
                    longitude: json["lon"].as_f64().unwrap_or(0.0),
                    speed_kmh: json["speed"].as_f64().unwrap_or(0.0) * 3.6,
                    altitude: json["alt"].as_f64().unwrap_or(0.0),
                    bearing: json["track"].as_f64().unwrap_or(0.0),
                    accuracy: accuracy.unwrap_or(0.0),
                    has_fix: json["mode"].as_i64().unwrap_or(0) >= 2,
                    is_mock: false,
                    last_update: now,
                    ..Default::default()
                };

                update_history(&speed_hist, now, data.speed_kmh, false).await;
                update_history(&alt_hist, now, data.altitude, false).await;
                *current.write().await = data;
            }
        }
    }
    Ok(got_data)
}

/// Centre of the mock track (a small circle around it): the Royal
/// Observatory, Greenwich — a neutral spot, only ever shown as "mock".
const MOCK_CENTER: (f64, f64) = (51.4769, -0.0005);

async fn mock_loop(
    current: Arc<RwLock<GpsData>>,
    speed_hist: Arc<RwLock<VecDeque<HistoryEntry>>>,
    alt_hist: Arc<RwLock<VecDeque<HistoryEntry>>>,
) {
    let mut tick = 0.0f64;
    loop {
        let now = now_secs();
        tick += 1.0;

        let data = GpsData {
            latitude: MOCK_CENTER.0 + (tick / 60.0).sin() * 0.01,
            longitude: MOCK_CENTER.1 + (tick / 60.0).cos() * 0.01,
            speed_kmh: ((tick / 10.0).sin() * 80.0).abs(),
            altitude: 150.0 + (tick / 30.0).sin() * 20.0,
            bearing: (tick * 5.0) % 360.0,
            // Mock data is NOT a real fix: keep has_fix=false so the UI shows
            // "no fix" and, critically, so the trip recorder never persists
            // fabricated trips. `is_mock` makes the source explicit.
            has_fix: false,
            is_mock: true,
            last_update: now,
            ..Default::default()
        };

        // Mock samples go into the same history buffers as real ones, but are
        // tagged so `/api/gps/history` can't present them as recorded driving.
        update_history(&speed_hist, now, data.speed_kmh, true).await;
        update_history(&alt_hist, now, data.altitude, true).await;
        *current.write().await = data;

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn update_history(
    hist: &Arc<RwLock<VecDeque<HistoryEntry>>>,
    t: f64,
    v: f64,
    mock: bool,
) {
    let mut h = hist.write().await;
    if h.len() >= HISTORY_LEN {
        h.pop_front();
    }
    h.push_back(HistoryEntry { t, v, mock });
}

/// Where `rascar-carplay` appends the car's NMEA sentences.
const CAR_GPS_PATH: &str = "/var/lib/rascar-debug/car-gps.nmea";

/// A car-GPS file that produces nothing for this long is treated as "the car is
/// not connected", and we fall back to gpsd. The Jaguar emits roughly one
/// sentence per second while a CarPlay session is up.
const CAR_GPS_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// One position fix parsed out of NMEA. `None` fields were absent from the
/// sentence — GPRMC has no altitude, GPGGA has no course.
#[derive(Debug, Default, PartialEq)]
pub struct NmeaFix {
    pub latitude: f64,
    pub longitude: f64,
    pub speed_kmh: Option<f64>,
    pub bearing: Option<f64>,
    pub altitude: Option<f64>,
    pub hdop: Option<f64>,
    pub has_fix: bool,
    /// Satellites used (GGA only).
    pub sats_used: Option<u16>,
}

/// `ddmm.mmmm` + hemisphere -> signed decimal degrees.
fn nmea_degrees(value: &str, hemisphere: &str) -> Option<f64> {
    if value.is_empty() {
        return None;
    }
    let dot = value.find('.')?;
    // Degrees are everything before the last two digits of the integer part:
    // 2 digits for latitude, 3 for longitude — so derive it rather than assume.
    let deg_len = dot.checked_sub(2)?;
    let degrees: f64 = value[..deg_len].parse().ok()?;
    let minutes: f64 = value[deg_len..].parse().ok()?;
    let mut d = degrees + minutes / 60.0;
    if matches!(hemisphere, "S" | "W") {
        d = -d;
    }
    Some(d)
}

/// True when the sentence carries a `*HH` checksum equal to the XOR of every
/// byte between `$` and `*`.
fn nmea_checksum_ok(sentence: &str) -> bool {
    let Some(body) = sentence.strip_prefix('$') else { return false };
    let Some((data, cs)) = body.split_once('*') else { return false };
    let Ok(expected) = u8::from_str_radix(cs.trim(), 16) else { return false };
    cs.trim().len() == 2 && data.bytes().fold(0u8, |acc, b| acc ^ b) == expected
}

/// The gear lever position from Apple's `$PASCD` sentence.
///
/// `$PASCD,<car seconds-of-day>,C,<gear>,1,1,0.00,<speed m/s>*CS`, gear being
/// P/R/N/D, or U (unknown) at session start. The checksum is mandatory: a
/// corrupted gear could fire (or miss) the park sync.
pub fn parse_pascd_gear(sentence: &str) -> Option<char> {
    if !nmea_checksum_ok(sentence) {
        return None;
    }
    let body = sentence.strip_prefix('$')?.split('*').next()?;
    let f: Vec<&str> = body.split(',').collect();
    if f.first() != Some(&"PASCD") || f.len() < 4 {
        return None;
    }
    let mut chars = f[3].chars();
    match (chars.next(), chars.next()) {
        (Some(g @ ('P' | 'R' | 'N' | 'D' | 'U')), None) => Some(g),
        _ => None,
    }
}

/// Parse one NMEA sentence from the car into a fix.
///
/// Handles the two the Jaguar sends that carry position: `GPRMC` (position,
/// speed, course, validity) and `GPGGA` (position, altitude, fix quality, HDOP).
/// Apple's proprietary `$PASCD` carries no position and yields `None` here;
/// its gear is read by `parse_pascd_gear`.
///
/// Talker id is not checked beyond length, so `$GNRMC`/`$GLGGA` from a
/// multi-constellation receiver parse too.
pub fn parse_nmea_fix(sentence: &str) -> Option<NmeaFix> {
    let body = sentence.strip_prefix('$')?;
    let body = body.split('*').next()?;
    let f: Vec<&str> = body.split(',').collect();
    let kind = f.first()?;
    if kind.len() < 5 {
        return None;
    }
    match &kind[2..5] {
        "RMC" => {
            // $xxRMC,time,status,lat,N,lon,E,speedKnots,courseDeg,date,...
            if f.len() < 9 {
                return None;
            }
            Some(NmeaFix {
                latitude: nmea_degrees(f[3], f[4])?,
                longitude: nmea_degrees(f[5], f[6])?,
                // NMEA speed is in knots; 1 kn = 1.852 km/h exactly.
                speed_kmh: f[7].parse::<f64>().ok().map(|kn| kn * 1.852),
                bearing: f[8].parse::<f64>().ok(),
                altitude: None,
                hdop: None,
                has_fix: f[2] == "A",
                sats_used: None,
            })
        }
        "GGA" => {
            // $xxGGA,time,lat,N,lon,E,quality,sats,hdop,alt,M,...
            if f.len() < 10 {
                return None;
            }
            let quality: u8 = f[6].parse().unwrap_or(0);
            Some(NmeaFix {
                latitude: nmea_degrees(f[2], f[3])?,
                longitude: nmea_degrees(f[4], f[5])?,
                speed_kmh: None,
                bearing: None,
                altitude: f[9].parse::<f64>().ok(),
                hdop: f[8].parse::<f64>().ok(),
                // 0 = no fix. Everything else counts, INCLUDING 6 (dead
                // reckoning) — that is exactly the mode that makes the car's
                // fused fix worth having in a tunnel or a car park.
                has_fix: quality > 0,
                sats_used: f[7].parse::<u16>().ok(),
            })
        }
        _ => None,
    }
}

/// One `$xxGSV` sentence: part `msg_num` of `total` of a satellites-in-view cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct GsvPart {
    pub talker: String,
    pub total: u8,
    pub msg_num: u8,
    pub in_view: u16,
    pub sats: Vec<Satellite>,
}

/// Most satellites one cycle may carry (9 sentences x 4). Bounds the list.
const GSV_MAX_SATS: usize = 36;

/// Parse one `$xxGSV` sentence. The checksum is mandatory.
///
/// `$GPGSV,<total msgs>,<msg num>,<sats in view>,{<PRN>,<elev deg>,<azim deg>,<SNR dB-Hz>}x1..4[,<signal id>]*CS`
/// — up to four satellites per sentence; NMEA 4.10 receivers append a signal
/// id, which the field count tells apart.
pub fn parse_gsv(sentence: &str) -> Option<GsvPart> {
    if !nmea_checksum_ok(sentence) {
        return None;
    }
    let body = sentence.strip_prefix('$')?.split('*').next()?;
    let f: Vec<&str> = body.split(',').collect();
    let kind = *f.first()?;
    if kind.len() != 5 || &kind[2..] != "GSV" || f.len() < 4 {
        return None;
    }
    let total: u8 = f[1].parse().ok()?;
    let msg_num: u8 = f[2].parse().ok()?;
    let in_view: u16 = f[3].parse().ok()?;
    if total == 0 || msg_num == 0 || msg_num > total || total > 9 {
        return None;
    }
    // 4 header fields, then blocks of 4, then an optional signal id.
    let blocks = &f[4..];
    let blocks = if blocks.len() % 4 == 1 { &blocks[..blocks.len() - 1] } else { blocks };
    if blocks.len() % 4 != 0 || blocks.len() > 16 {
        return None;
    }
    let sats = blocks
        .chunks(4)
        // A padded empty block (",,,,") carries no satellite.
        .filter_map(|b| {
            Some(Satellite {
                prn: b[0].parse().ok()?,
                elevation: b[1].parse::<u8>().ok().filter(|e| *e <= 90),
                azimuth: b[2].parse::<u16>().ok().filter(|a| *a < 360),
                snr: b[3].parse::<u8>().ok().filter(|s| *s <= 99),
            })
        })
        .collect();
    Some(GsvPart { talker: kind[..2].to_string(), total, msg_num, in_view, sats })
}

/// Reassembles a multi-sentence GSV cycle. A cycle is published only when
/// every part arrived in order; anything out of sequence drops the cycle
/// rather than mixing two epochs.
#[derive(Debug, Default)]
pub struct GsvAssembler {
    talker: String,
    total: u8,
    next: u8,
    in_view: u16,
    sats: Vec<Satellite>,
}

impl GsvAssembler {
    /// Feed one part; returns `(sats in view, satellites)` when it completes a cycle.
    pub fn push(&mut self, part: GsvPart) -> Option<(u16, Vec<Satellite>)> {
        if part.msg_num == 1 {
            self.talker = part.talker;
            self.total = part.total;
            self.in_view = part.in_view;
            self.sats.clear();
        } else if part.msg_num != self.next || part.total != self.total || part.talker != self.talker {
            self.next = 0;
            self.sats.clear();
            return None;
        }
        self.sats.extend(part.sats);
        self.sats.truncate(GSV_MAX_SATS);
        if part.msg_num == self.total {
            self.next = 0;
            return Some((self.in_view, std::mem::take(&mut self.sats)));
        }
        self.next = part.msg_num + 1;
        None
    }
}

/// Resolves once the car's NMEA file is written after this call started
/// (its modification time moves). Pending forever while no session runs.
fn car_gps_path() -> String {
    std::env::var("RASCAR_CAR_GPS_PATH").unwrap_or_else(|_| CAR_GPS_PATH.to_string())
}

async fn car_feed_resumed() {
    let path = car_gps_path();
    let mtime = |p: &str| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let start = mtime(&path);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let now = mtime(&path);
        if now.is_some() && now != start {
            return;
        }
    }
}

/// Tail the car's NMEA file and stream fixes into shared state.
///
/// Returns `Ok(true)` once at least one fix was applied and the feed then went
/// stale, `Ok(false)` if the file exists but produced nothing within
/// `CAR_GPS_STALE_AFTER`, and `Err` if there is no file at all (no CarPlay
/// session has ever run).
///
/// Starts at END of file: on startup we want the car's CURRENT position, not to
/// replay a drive from three days ago at full speed through the trip recorder.
async fn poll_car_gps(
    path: &str,
    current: Arc<RwLock<GpsData>>,
    speed_hist: Arc<RwLock<VecDeque<HistoryEntry>>>,
    alt_hist: Arc<RwLock<VecDeque<HistoryEntry>>>,
    gear: Arc<RwLock<Option<GearSample>>>,
) -> Result<bool> {
    use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};

    let mut file = tokio::fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::End(0)).await?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut got_data = false;
    // GPRMC and GPGGA each carry half of what we need, so merge consecutive
    // sentences rather than letting one blank the other's fields.
    let mut merged = GpsData::default();
    // Staleness is the time since the last LINE. A timeout around read_line
    // never fired: at EOF it returns Ok(0) at once, so with the file present
    // and no session the loop polled forever and gpsd was never used.
    let mut last_line = std::time::Instant::now();
    let mut gsv = GsvAssembler::default();
    let mut gsv_at: Option<std::time::Instant> = None;

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                if last_line.elapsed() >= CAR_GPS_STALE_AFTER {
                    return Ok(got_data); // car disconnected or session ended
                }
                // EOF on a file that is still being appended to — wait briefly
                // rather than spinning, and try again.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
            Ok(_) => last_line = std::time::Instant::now(),
            Err(e) => return Err(e.into()),
        }

        if let Some(g) = parse_pascd_gear(line.trim()) {
            *gear.write().await = Some(GearSample { gear: g, at: std::time::Instant::now() });
            continue;
        }
        // Satellites in view: published with the next fix line, so a GSV
        // cycle never writes a position-less state over a live fix.
        if let Some(part) = parse_gsv(line.trim()) {
            if let Some((in_view, sats)) = gsv.push(part) {
                merged.sats_in_view = Some(in_view);
                merged.satellites = sats;
                gsv_at = Some(std::time::Instant::now());
            }
            continue;
        }
        let Some(fix) = parse_nmea_fix(line.trim()) else {
            continue;
        };
        // GSV stopped (or never came) while fixes continue: drop the old view.
        if gsv_at.is_some_and(|t| t.elapsed() > CAR_GPS_STALE_AFTER) {
            merged.sats_in_view = None;
            merged.satellites.clear();
            gsv_at = None;
        }
        if let Some(n) = fix.sats_used {
            merged.sats_used = Some(n);
        }
        let now = now_secs();
        merged.latitude = fix.latitude;
        merged.longitude = fix.longitude;
        if let Some(v) = fix.speed_kmh {
            merged.speed_kmh = v;
        }
        if let Some(v) = fix.bearing {
            merged.bearing = v;
        }
        if let Some(v) = fix.altitude {
            merged.altitude = v;
        }
        if let Some(h) = fix.hdop {
            // HDOP is unitless; turn it into metres with a nominal 5 m UERE.
            // Approximate on purpose — the car does not give us a real error
            // estimate, and claiming more precision than we have would be worse.
            merged.accuracy = h * 5.0;
        }
        merged.has_fix = fix.has_fix;
        merged.is_mock = false;
        merged.last_update = now;

        update_history(&speed_hist, now, merged.speed_kmh, false).await;
        update_history(&alt_hist, now, merged.altitude, false).await;
        *current.write().await = merged.clone();
        got_data = true;
    }
}

#[cfg(test)]
mod car_nmea_tests {
    use super::*;
    use std::io::Write;

    // An existing but idle car feed (no CarPlay session) must hand over to
    // gpsd after CAR_GPS_STALE_AFTER, not tail the file forever; and a line
    // appended meanwhile must be applied first.
    #[tokio::test]
    async fn idle_car_feed_goes_stale_and_live_lines_are_applied() {
        let path = std::env::temp_dir().join(format!("rascar-car-gps-test-{}.nmea", std::process::id()));
        std::fs::write(&path, format!("{GPRMC}\n")).unwrap();
        let cur = Arc::new(RwLock::new(GpsData::default()));
        let cur2 = cur.clone();
        let (sh, ah) = (Arc::new(RwLock::new(VecDeque::new())), Arc::new(RwLock::new(VecDeque::new())));
        let started = std::time::Instant::now();
        let p = path.to_string_lossy().into_owned();
        let gear = Arc::new(RwLock::new(None));
        let gear2 = gear.clone();
        let poll = tokio::spawn(async move { poll_car_gps(&p, cur2, sh, ah, gear2).await });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        std::fs::OpenOptions::new().append(true).open(&path).unwrap()
            .write_all(format!("{PASCD}\n{GPGGA}\n").as_bytes()).unwrap();
        let got = tokio::time::timeout(CAR_GPS_STALE_AFTER * 3, poll).await
            .expect("poll_car_gps never returned on an idle file").unwrap().unwrap();
        assert!(got, "the appended line was not applied");
        assert!(cur.read().await.has_fix);
        assert_eq!(gear.read().await.map(|s| s.gear), Some('D'), "the $PASCD gear was not stored");
        assert!(started.elapsed() >= CAR_GPS_STALE_AFTER);
        let _ = std::fs::remove_file(&path);
    }

    // Sentences in the exact format the Jaguar F-PACE sends over iAP2
    // (captured from the car, position moved to a neutral place).
    const GPGGA: &str =
        "$GPGGA,120000.00,4851.402000,N,00221.102000,E,6,17,0.8,35.0,M,44.8,M,,*54";
    const GPRMC: &str =
        "$GPRMC,120000.00,A,4851.402000,N,00221.102000,E,21.208,218.18,210926,,,E*50";
    const PASCD: &str = "$PASCD,43200.000,C,D,1,1,0.00,10.556*42";

    #[test]
    fn gprmc_gives_position_speed_and_course() {
        let f = parse_nmea_fix(GPRMC).expect("GPRMC must parse");
        assert!((f.latitude - 48.8567).abs() < 1e-5, "lat was {}", f.latitude);
        assert!((f.longitude - 2.3517).abs() < 1e-5, "lon was {}", f.longitude);
        // 21.208 kn * 1.852 = 39.277 km/h
        assert!((f.speed_kmh.unwrap() - 39.277).abs() < 0.01);
        assert!((f.bearing.unwrap() - 218.18).abs() < 0.01);
        assert!(f.has_fix, "status 'A' means the fix is valid");
    }

    #[test]
    fn gpgga_gives_altitude_and_accepts_dead_reckoning() {
        let f = parse_nmea_fix(GPGGA).expect("GPGGA must parse");
        assert_eq!(f.altitude, Some(35.0));
        assert_eq!(f.hdop, Some(0.8));
        assert!(
            f.has_fix,
            "quality 6 is dead reckoning — the fused mode that works in tunnels, \
             and must NOT be treated as 'no fix'"
        );
    }

    #[test]
    fn gpgga_quality_zero_is_no_fix() {
        let no_fix = GPGGA.replace(",E,6,17,", ",E,0,17,");
        assert!(!parse_nmea_fix(&no_fix).unwrap().has_fix);
    }

    #[test]
    fn apples_proprietary_sentence_is_ignored() {
        assert_eq!(parse_nmea_fix(PASCD), None, "$PASCD carries no position");
    }

    #[test]
    fn pascd_gear_is_parsed_for_every_lever_position() {
        // Byte-exact from the Pi's car-gps.nmea (2026-10-09).
        assert_eq!(parse_pascd_gear("$PASCD,43200.000,C,P,1,1,0.00,0.000*61"), Some('P'));
        assert_eq!(parse_pascd_gear(PASCD), Some('D'));
        // Re-checksummed variants for the other positions.
        for g in ['R', 'N', 'U'] {
            let s = with_checksum(&format!("PASCD,43200.000,C,{g},1,1,0.00,0.000"));
            assert_eq!(parse_pascd_gear(&s), Some(g), "{s}");
        }
    }

    #[test]
    fn pascd_with_bad_or_missing_checksum_is_rejected() {
        assert_eq!(parse_pascd_gear("$PASCD,43200.000,C,D,1,1,0.00,10.556*44"), None);
        assert_eq!(parse_pascd_gear("$PASCD,43200.000,C,D,1,1,0.00,10.556"), None);
        assert_eq!(parse_pascd_gear("$PASCD,43200.000,C,D,1,1,0.00,10.556*4"), None);
        assert_eq!(parse_pascd_gear("$PASCD,43200.000,C,D,1,1,0.00,10.556*ZZ"), None);
        // A single flipped bit in the gear field (D 0x44 -> E 0x45).
        assert_eq!(parse_pascd_gear("$PASCD,43200.000,C,E,1,1,0.00,10.556*43"), None);
    }

    #[test]
    fn malformed_pascd_yields_no_gear() {
        for body in [
            "PASCD,43200.000,C,X,1,1,0.00,0.000",  // unknown gear letter
            "PASCD,43200.000,C,DD,1,1,0.00,0.000", // two letters
            "PASCD,43200.000,C,,1,1,0.00,0.000",   // empty
            "PASCD,43200.000,C",                   // truncated
            "GPXYZ,43200.000,C,D,1,1,0.00,0.000",  // other sentence
        ] {
            assert_eq!(parse_pascd_gear(&with_checksum(body)), None, "{body}");
        }
        for s in ["", "$", "*", "$*00", "not nmea", GPRMC, GPGGA] {
            assert_eq!(parse_pascd_gear(s), None, "{s}");
        }
    }

    #[test]
    fn gear_goes_unknown_when_stale_or_u() {
        let t0 = std::time::Instant::now();
        let s = |gear| Some(GearSample { gear, at: t0 });
        assert_eq!(effective_gear(s('P'), t0), Some('P'));
        assert_eq!(effective_gear(s('P'), t0 + GEAR_STALE_AFTER), Some('P'));
        assert_eq!(effective_gear(s('P'), t0 + GEAR_STALE_AFTER + std::time::Duration::from_millis(1)), None);
        assert_eq!(effective_gear(s('U'), t0), None);
        assert_eq!(effective_gear(None, t0), None);
    }

    fn with_checksum(body: &str) -> String {
        format!("${body}*{:02X}", body.bytes().fold(0u8, |a, b| a ^ b))
    }

    #[test]
    fn southern_and_western_hemispheres_are_negative() {
        let s = GPRMC.replace(",N,", ",S,").replace(",E,21.208", ",W,21.208");
        let f = parse_nmea_fix(&s).unwrap();
        assert!(f.latitude < 0.0 && f.longitude < 0.0, "got {} {}", f.latitude, f.longitude);
    }

    #[test]
    fn longitude_uses_three_degree_digits_not_two() {
        // 00221.102000 is 2°21.102', NOT 22°1.102' — the split must be derived
        // from the decimal point, not hard-coded per field.
        let f = parse_nmea_fix(GPRMC).unwrap();
        assert!(f.longitude < 5.0, "longitude mis-split: {}", f.longitude);
    }

    #[test]
    fn junk_and_truncated_sentences_do_not_panic() {
        for s in ["", "$", "$GP", "not nmea", "$GPRMC,1,2", "$GPGGA,,,,,,,,,"] {
            let _ = parse_nmea_fix(s);
            let _ = parse_gsv(s);
        }
    }

    // A three-sentence cycle in the NMEA 0183 reference format (11 in view,
    // the last sentence padded with an empty block). The car declares GPGSV
    // but had never been asked for it, so no capture exists yet.
    const GSV1: &str = "$GPGSV,3,1,11,03,03,111,00,04,15,270,00,06,01,010,00,13,06,292,00*74";
    const GSV2: &str = "$GPGSV,3,2,11,14,25,170,00,16,57,208,39,18,67,296,40,19,40,246,00*74";
    const GSV3: &str = "$GPGSV,3,3,11,22,42,067,42,24,14,311,43,27,05,244,00,,,,*4D";

    #[test]
    fn gpgsv_parts_parse_with_four_satellites_each() {
        let p = parse_gsv(GSV2).expect("GSV must parse");
        assert_eq!((p.talker.as_str(), p.total, p.msg_num, p.in_view), ("GP", 3, 2, 11));
        assert_eq!(p.sats.len(), 4);
        assert_eq!(p.sats[1], Satellite { prn: 16, elevation: Some(57), azimuth: Some(208), snr: Some(39) });
        // The empty padding block is not a satellite.
        assert_eq!(parse_gsv(GSV3).unwrap().sats.len(), 3);
        // GSV is not a position.
        assert_eq!(parse_nmea_fix(GSV1), None);
    }

    #[test]
    fn gpgsv_untracked_satellite_has_no_snr_and_signal_id_is_tolerated() {
        let p = parse_gsv(&with_checksum("GPGSV,1,1,03,02,45,120,38,05,,,,12,30,300,")).unwrap();
        assert_eq!(p.sats.len(), 3);
        assert_eq!(p.sats[1], Satellite { prn: 5, elevation: None, azimuth: None, snr: None });
        assert_eq!(p.sats[2].snr, None);
        // NMEA 4.10 appends a signal id after the last block.
        let p = parse_gsv(&with_checksum("GPGSV,1,1,01,07,10,020,33,1")).unwrap();
        assert_eq!(p.sats, vec![Satellite { prn: 7, elevation: Some(10), azimuth: Some(20), snr: Some(33) }]);
    }

    #[test]
    fn gpgsv_checksum_is_mandatory() {
        assert_eq!(parse_gsv(&GSV1.replace("*74", "*75")), None);
        assert_eq!(parse_gsv(GSV1.split('*').next().unwrap()), None);
        assert_eq!(parse_gsv(&GSV2.replace(",39,", ",38,")), None, "corrupt SNR must not pass");
        assert_eq!(parse_gsv(&with_checksum("GPGSV,0,1,11")), None);
        assert_eq!(parse_gsv(&with_checksum("GPGSV,2,3,11")), None);
        assert_eq!(parse_gsv(&with_checksum("GPGSV,1,1,03,02,45,120")), None, "partial block");
    }

    #[test]
    fn gsv_cycle_is_published_only_when_complete_and_in_order() {
        let mut a = GsvAssembler::default();
        assert_eq!(a.push(parse_gsv(GSV1).unwrap()), None);
        assert_eq!(a.push(parse_gsv(GSV2).unwrap()), None);
        let (in_view, sats) = a.push(parse_gsv(GSV3).unwrap()).expect("cycle complete");
        assert_eq!(in_view, 11);
        assert_eq!(sats.len(), 11);
        assert_eq!(sats.iter().filter(|s| s.snr.unwrap_or(0) > 0).count(), 4, "SNR 39, 40, 42, 43");

        // A missing middle part drops the cycle instead of mixing epochs.
        assert_eq!(a.push(parse_gsv(GSV1).unwrap()), None);
        assert_eq!(a.push(parse_gsv(GSV3).unwrap()), None);
        // ...and the next full cycle is fine again.
        a.push(parse_gsv(GSV1).unwrap());
        a.push(parse_gsv(GSV2).unwrap());
        assert_eq!(a.push(parse_gsv(GSV3).unwrap()).map(|(_, s)| s.len()), Some(11));
    }

    #[test]
    fn gga_reports_satellites_used() {
        assert_eq!(parse_nmea_fix(GPGGA).unwrap().sats_used, Some(17));
        assert_eq!(parse_nmea_fix(GPRMC).unwrap().sats_used, None);
    }

    // GSV lines in the car feed fill the satellite list without disturbing the fix.
    #[tokio::test]
    async fn car_feed_gsv_lines_fill_satellites_alongside_the_fix() {
        let path = std::env::temp_dir().join(format!("rascar-car-gsv-test-{}.nmea", std::process::id()));
        std::fs::write(&path, "").unwrap();
        let cur = Arc::new(RwLock::new(GpsData::default()));
        let (sh, ah) = (Arc::new(RwLock::new(VecDeque::new())), Arc::new(RwLock::new(VecDeque::new())));
        let p = path.to_string_lossy().into_owned();
        let cur2 = cur.clone();
        let poll = tokio::spawn(async move { poll_car_gps(&p, cur2, sh, ah, Arc::new(RwLock::new(None))).await });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        std::fs::OpenOptions::new().append(true).open(&path).unwrap()
            .write_all(format!("{GPGGA}\n{GSV1}\n{GSV2}\n{GSV3}\n{GPRMC}\n").as_bytes()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        {
            let g = cur.read().await;
            assert!(g.has_fix);
            assert!((g.altitude - 35.0).abs() < 1e-9, "GGA altitude kept");
            assert!(g.speed_kmh > 39.0, "RMC speed kept");
            assert_eq!(g.sats_used, Some(17));
            assert_eq!(g.sats_in_view, Some(11));
            assert_eq!(g.satellites.len(), 11);
        }
        poll.abort();
        let _ = std::fs::remove_file(&path);
    }
}
