//! Offline navigation: destination search and routing for the dashboard's own
//! map (frontend/js/navmap.js), with no network needed.
//!
//! - `/api/geo/search?q=…&lat=…&lon=…` reads the search index built by
//!   `setup/maps/build-geocoder.py` (`<maps_dir>/geocoder.sqlite`, SQLite FTS5,
//!   accent-insensitive) and ranks matches by text relevance, kind (towns before
//!   house numbers) and distance from the car.
//! - `POST /api/route {from:[lon,lat], to:[lon,lat]}` asks the local Valhalla
//!   service (`rascar-valhalla.service`, graph from `setup/maps/build-routing.sh`)
//!   for a car route in French and returns only what the page draws: the line,
//!   the totals and the maneuvers.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

static MAPS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Valhalla's HTTP service (loopback only, see rascar-valhalla.service).
const VALHALLA_URL: &str = "http://127.0.0.1:8002/route";

pub fn init(maps_dir: &str) {
    let _ = MAPS_DIR.set(PathBuf::from(maps_dir));
}

fn geocoder_path() -> PathBuf {
    MAPS_DIR.get().cloned().unwrap_or_else(|| PathBuf::from("/var/lib/rascar/maps")).join("geocoder.sqlite")
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Place {
    pub name: String,
    pub detail: String,
    pub kind: String,
    pub lat: f64,
    pub lon: f64,
    /// Straight-line distance from the car, when its position was given.
    pub km: Option<f64>,
}

/// FTS5 query for what the user typed: every word must match (accents
/// ignored by the tokenizer), the last one as a prefix since they are still
/// typing. Words are quoted, so FTS operators in the input are inert.
pub fn fts_query(input: &str) -> Option<String> {
    let words: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect();
    let (last, rest) = words.split_last()?;
    let mut parts: Vec<String> = rest.iter().map(|w| format!("\"{w}\"")).collect();
    parts.push(format!("\"{last}\"*"));
    Some(parts.join(" AND "))
}

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    6371.0 * 2.0 * a.sqrt().asin()
}

/// Search the offline index. Blocking (SQLite): call from `spawn_blocking`.
pub fn search(q: &str, near: Option<(f64, f64)>, limit: usize) -> anyhow::Result<Vec<Place>> {
    let Some(query) = fts_query(q) else { return Ok(Vec::new()) };
    let db = rusqlite::Connection::open_with_flags(geocoder_path(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // A generous candidate set by text relevance, re-ranked below with kind
    // and distance: "Rue de la Station" exists in a hundred towns.
    let mut stmt = db.prepare(
        "SELECT p.name, p.detail, p.kind, p.lat, p.lon, p.rank, bm25(place_fts) \
         FROM place_fts JOIN place p ON p.id = place_fts.rowid \
         WHERE place_fts MATCH ?1 ORDER BY bm25(place_fts) LIMIT 400",
    )?;
    let mut scored: Vec<(f64, Place)> = stmt
        .query_map([query], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, f64>(3)?, r.get::<_, f64>(4)?, r.get::<_, i64>(5)?, r.get::<_, f64>(6)?))
        })?
        .filter_map(Result::ok)
        .map(|(name, detail, kind, lat, lon, rank, bm25)| {
            let km = near.map(|(nlat, nlon)| haversine_km(nlat, nlon, lat, lon));
            // bm25 is negative, more negative = better match.
            let score = -bm25 + rank as f64 / 25.0 - km.map_or(0.0, |d| (1.0 + d).ln() * 0.8);
            (score, Place { name, detail, kind, lat, lon, km: km.map(|d| (d * 10.0).round() / 10.0) })
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    Ok(scored.into_iter().take(limit).map(|(_, p)| p).collect())
}

#[derive(Debug, Deserialize)]
pub struct RouteRequest {
    pub from: [f64; 2],
    pub to: [f64; 2],
    /// Pass-through points (no stop): follow a given path — our own route for
    /// its traffic time, or Google's faster route when we switch to it.
    #[serde(default)]
    pub via: Vec<[f64; 2]>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Maneuver {
    pub instruction: String,
    /// Valhalla maneuver type (turn left, roundabout, …) for the icon.
    #[serde(rename = "type")]
    pub kind: i64,
    pub km: f64,
    pub seconds: f64,
    /// Index into `shape` where the maneuver starts.
    pub at: usize,
    pub street: String,
    /// Spoken phrasings from Valhalla: ahead of the maneuver, at it, and
    /// after it ("Continuez pendant 2 km").
    pub alert: String,
    pub pre: String,
    pub post: String,
    /// Motorway signage: exit number, where it leads, the exit's name and the
    /// road branches of a ramp (e.g. "A1 / A2").
    pub exit: String,
    pub toward: String,
    pub exit_name: String,
    pub branch: String,
}

fn sign_text(sign: &serde_json::Value, key: &str, sep: &str) -> String {
    sign[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["text"].as_str())
        .collect::<Vec<_>>()
        .join(sep)
}

/// Posted speed limit over `shape[from..=to]` (km/h), from OSM maxspeed.
#[derive(Debug, Serialize, PartialEq)]
pub struct Limit {
    pub from: usize,
    pub to: usize,
    pub kmh: u32,
}

#[derive(Debug, Serialize)]
pub struct Route {
    /// `[lon, lat]` pairs.
    pub shape: Vec<[f64; 2]>,
    pub km: f64,
    pub seconds: f64,
    pub maneuvers: Vec<Maneuver>,
    pub limits: Vec<Limit>,
}

/// Decode a Valhalla shape (Google polyline, 6 decimal places) to `[lon, lat]`.
pub fn decode_polyline6(s: &str) -> Vec<[f64; 2]> {
    decode_polyline(s, 1e6)
}

/// Decode an encoded polyline with the given scale (1e5 Google, 1e6 Valhalla).
pub fn decode_polyline(s: &str, scale: f64) -> Vec<[f64; 2]> {
    let bytes = s.as_bytes();
    let (mut i, mut lat, mut lon) = (0usize, 0i64, 0i64);
    let mut out = Vec::new();
    let next = |i: &mut usize| -> Option<i64> {
        let (mut result, mut shift) = (0i64, 0u32);
        loop {
            let b = (*bytes.get(*i)? as i64) - 63;
            *i += 1;
            result |= (b & 0x1f) << shift;
            shift += 5;
            if b < 0x20 {
                break;
            }
        }
        Some(if result & 1 != 0 { !(result >> 1) } else { result >> 1 })
    };
    while i < bytes.len() {
        let (Some(dlat), Some(dlon)) = (next(&mut i), next(&mut i)) else { break };
        lat += dlat;
        lon += dlon;
        out.push([lon as f64 / scale, lat as f64 / scale]);
    }
    out
}

/// Keep what the page uses from a Valhalla `/route` answer.
pub fn parse_route(v: &serde_json::Value) -> anyhow::Result<Route> {
    let trip = v.get("trip").ok_or_else(|| anyhow::anyhow!("no trip in the answer"))?;
    let mut shape = Vec::new();
    let mut maneuvers = Vec::new();
    for leg in trip["legs"].as_array().into_iter().flatten() {
        let offset = shape.len();
        shape.extend(decode_polyline6(leg["shape"].as_str().unwrap_or("")));
        for m in leg["maneuvers"].as_array().into_iter().flatten() {
            maneuvers.push(Maneuver {
                instruction: m["instruction"].as_str().unwrap_or("").to_string(),
                kind: m["type"].as_i64().unwrap_or(0),
                km: m["length"].as_f64().unwrap_or(0.0),
                seconds: m["time"].as_f64().unwrap_or(0.0),
                at: offset + m["begin_shape_index"].as_u64().unwrap_or(0) as usize,
                street: m["street_names"].get(0).and_then(|s| s.as_str()).unwrap_or("").to_string(),
                alert: m["verbal_transition_alert_instruction"].as_str().unwrap_or("").to_string(),
                pre: m["verbal_pre_transition_instruction"].as_str().unwrap_or("").to_string(),
                post: m["verbal_post_transition_instruction"].as_str().unwrap_or("").to_string(),
                exit: sign_text(&m["sign"], "exit_number_elements", "/"),
                toward: sign_text(&m["sign"], "exit_toward_elements", " / "),
                exit_name: sign_text(&m["sign"], "exit_name_elements", " / "),
                branch: sign_text(&m["sign"], "exit_branch_elements", " / "),
            });
        }
    }
    Ok(Route {
        shape,
        km: trip["summary"]["length"].as_f64().unwrap_or(0.0),
        seconds: trip["summary"]["time"].as_f64().unwrap_or(0.0),
        maneuvers,
        limits: Vec::new(),
    })
}

/// Speed limits along a route from Valhalla's `trace_attributes` edges
/// (shape indices refer to the same encoded line).
pub fn parse_limits(v: &serde_json::Value) -> Vec<Limit> {
    v["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let kmh = e["speed_limit"].as_u64().filter(|k| *k > 0 && *k < 200)? as u32;
            Some(Limit { from: e["begin_shape_index"].as_u64()? as usize, to: e["end_shape_index"].as_u64()? as usize, kmh })
        })
        .collect()
}

pub async fn route(req: &RouteRequest) -> anyhow::Result<Route> {
    let mut locations = vec![serde_json::json!({ "lon": req.from[0], "lat": req.from[1] })];
    locations.extend(req.via.iter().map(|p| serde_json::json!({ "lon": p[0], "lat": p[1], "type": "through" })));
    locations.push(serde_json::json!({ "lon": req.to[0], "lat": req.to[1] }));
    let body = serde_json::json!({
        "locations": locations,
        "costing": "auto",
        "units": "kilometers",
        "language": "fr-FR",
        "directions_options": { "units": "kilometers", "language": "fr-FR" },
    });
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build()?;
    let resp = client.post(VALHALLA_URL).json(&body).send().await?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await?;
    if !status.is_success() {
        anyhow::bail!("{}", v["error"].as_str().unwrap_or("routing failed"));
    }
    let mut route = parse_route(&v)?;
    // Speed limits: best effort (a route without them is still a route).
    // Through points keep one leg, so leg 0's shape is the whole line.
    if let Some(shape) = v["trip"]["legs"][0]["shape"].as_str().filter(|_| v["trip"]["legs"].as_array().map_or(0, |l| l.len()) == 1) {
        let body = serde_json::json!({
            "encoded_polyline": shape,
            "shape_match": "edge_walk",
            "costing": "auto",
            "filters": { "attributes": ["edge.speed_limit", "edge.begin_shape_index", "edge.end_shape_index"], "action": "include" },
        });
        match client.post(VALHALLA_URL.replace("/route", "/trace_attributes")).json(&body).send().await {
            Ok(r) => match r.json::<serde_json::Value>().await {
                Ok(t) => route.limits = parse_limits(&t),
                Err(e) => tracing::warn!("navigation: speed limits unreadable: {e}"),
            },
            Err(e) => tracing::warn!("navigation: speed limits unavailable: {e}"),
        }
    }
    Ok(route)
}

// ── Live traffic (Google Routes API) ───────────────────────────────
// Optional, off unless you provide your own Google Maps Platform key: Google's
// traffic (Google Maps + Waze data) over our offline map. Check Google's terms
// before enabling it: they restrict showing Routes data on a non-Google map.
// Our route and guidance stay Valhalla's; Google gives the traffic-aware
// duration and the slow/jammed stretches. Key: one line in TRAFFIC_KEY_FILE
// (0600, never in the repo). No key or no network: no traffic.
const TRAFFIC_KEY_FILE: &str = "/etc/rascar/google-routes.key";
const ROUTES_URL: &str = "https://routes.googleapis.com/directions/v2:computeRoutes";

#[derive(Debug, Serialize, PartialEq)]
pub struct Jam {
    /// "SLOW" or "TRAFFIC_JAM".
    pub speed: String,
    pub coords: Vec<[f64; 2]>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Traffic {
    /// Duration with current traffic, and without (Google's "static").
    pub seconds: f64,
    pub free_seconds: f64,
    pub km: f64,
    pub jams: Vec<Jam>,
    /// Google's whole line: to tell whether its route takes other roads, and
    /// to follow it when switching.
    pub line: Vec<[f64; 2]>,
}

fn seconds(v: &serde_json::Value) -> f64 {
    v.as_str().and_then(|s| s.trim_end_matches('s').parse().ok()).unwrap_or(0.0)
}

/// Keep what the page uses from a Routes API answer: durations, distance and
/// the non-normal stretches of its line.
pub fn parse_traffic(v: &serde_json::Value) -> anyhow::Result<Traffic> {
    let r = &v["routes"][0];
    if r.is_null() {
        anyhow::bail!("{}", v["error"]["message"].as_str().unwrap_or("no route"));
    }
    let line = decode_polyline(r["polyline"]["encodedPolyline"].as_str().unwrap_or(""), 1e5);
    let jams = r["travelAdvisory"]["speedReadingIntervals"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| {
            let speed = i["speed"].as_str().unwrap_or("NORMAL");
            if speed == "NORMAL" {
                return None;
            }
            // A missing start index means 0 (proto3 default).
            let from = i["startPolylinePointIndex"].as_u64().unwrap_or(0) as usize;
            let to = i["endPolylinePointIndex"].as_u64()? as usize;
            let coords = line.get(from..=to.min(line.len().saturating_sub(1)))?.to_vec();
            (coords.len() >= 2).then(|| Jam { speed: speed.to_string(), coords })
        })
        .collect();
    Ok(Traffic {
        seconds: seconds(&r["duration"]),
        free_seconds: seconds(&r["staticDuration"]),
        km: r["distanceMeters"].as_f64().unwrap_or(0.0) / 1000.0,
        jams,
        line,
    })
}

pub async fn traffic(req: &RouteRequest) -> anyhow::Result<Traffic> {
    let key = std::fs::read_to_string(TRAFFIC_KEY_FILE)
        .map(|k| k.trim().to_string())
        .ok()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no traffic key ({TRAFFIC_KEY_FILE})"))?;
    let point = |p: [f64; 2]| serde_json::json!({ "location": { "latLng": { "latitude": p[1], "longitude": p[0] } } });
    let intermediates: Vec<serde_json::Value> = req
        .via
        .iter()
        .map(|p| serde_json::json!({ "via": true, "location": { "latLng": { "latitude": p[1], "longitude": p[0] } } }))
        .collect();
    let body = serde_json::json!({
        "origin": point(req.from),
        "destination": point(req.to),
        "intermediates": intermediates,
        "travelMode": "DRIVE",
        "routingPreference": "TRAFFIC_AWARE",
        "extraComputations": ["TRAFFIC_ON_POLYLINE"],
        "languageCode": "fr-FR",
        "units": "METRIC",
    });
    let v: serde_json::Value = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?
        .post(ROUTES_URL)
        .header("X-Goog-Api-Key", key)
        .header(
            "X-Goog-FieldMask",
            "routes.duration,routes.staticDuration,routes.distanceMeters,routes.polyline.encodedPolyline,routes.travelAdvisory.speedReadingIntervals",
        )
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    parse_traffic(&v)
}

// ── Time to home ──────────────────────────────────────────────────
// The nav page shows "home in N min" at all times. Google's traffic time is
// paid per call, so it is fetched at most once per HOME_GOOGLE_EVERY; in
// between, a large move is answered by the local Valhalla (free, ~100 ms)
// scaled by the traffic ratio Google gave last time.
const HOME_REFRESH: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const HOME_GOOGLE_EVERY: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// Recompute (locally) before HOME_REFRESH once the car moved this far…
const HOME_MOVED_M: f64 = 1500.0;
/// …but not more often than this.
const HOME_LOCAL_MIN: std::time::Duration = std::time::Duration::from_secs(30);
/// A Google traffic ratio older than this no longer describes the roads.
const HOME_RATIO_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(20 * 60);
/// Closer than this, the car is home: nothing to show, nothing to compute.
pub const AT_HOME_M: f64 = 300.0;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HomeEta {
    pub at_home: bool,
    /// Straight-line distance to home.
    pub distance_km: f64,
    /// Road distance and duration (with traffic when known), and the
    /// free-flow duration when Google gave one.
    pub km: f64,
    pub seconds: f64,
    pub free_seconds: Option<f64>,
    /// "google", "valhalla+traffic" (local route scaled by Google's last
    /// traffic ratio) or "valhalla" (no traffic).
    pub source: String,
    /// Seconds since this value was computed.
    pub age_s: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeRefresh {
    Cached,
    Local,
    Google,
}

/// What to do for a request: `cache_age`/`moved_m` describe the last answer
/// (None = none for this home), `google_age` the last Google call.
pub fn plan_home_refresh(
    cache_age: Option<std::time::Duration>,
    moved_m: f64,
    google_age: Option<std::time::Duration>,
) -> HomeRefresh {
    let google_ok = google_age.is_none_or(|a| a >= HOME_GOOGLE_EVERY);
    let stale = match cache_age {
        None => true,
        Some(a) => a >= HOME_REFRESH,
    };
    let moved = cache_age.is_some_and(|a| a >= HOME_LOCAL_MIN) && moved_m >= HOME_MOVED_M;
    match (stale || moved, google_ok) {
        (false, _) => HomeRefresh::Cached,
        (true, true) => HomeRefresh::Google,
        (true, false) => HomeRefresh::Local,
    }
}

struct HomeCache {
    home: [f64; 2],
    from: [f64; 2],
    at: std::time::Instant,
    eta: HomeEta,
}

#[derive(Default)]
struct HomeState {
    cache: Option<HomeCache>,
    google_at: Option<std::time::Instant>,
    /// Google duration ÷ Valhalla duration for the same trip, and when.
    ratio: Option<(f64, [f64; 2], std::time::Instant)>,
}

/// One computation at a time: concurrent pages must not double-bill Google.
static HOME_STATE: tokio::sync::Mutex<Option<HomeState>> = tokio::sync::Mutex::const_new(None);

fn metres(a: [f64; 2], b: [f64; 2]) -> f64 {
    haversine_km(a[1], a[0], b[1], b[0]) * 1000.0
}

/// Road distance (km) and duration (s) from the local Valhalla, summary only.
pub async fn valhalla_summary(from: [f64; 2], to: [f64; 2]) -> anyhow::Result<(f64, f64)> {
    let body = serde_json::json!({
        "locations": [{ "lon": from[0], "lat": from[1] }, { "lon": to[0], "lat": to[1] }],
        "costing": "auto",
        "units": "kilometers",
        "directions_type": "none",
    });
    let v: serde_json::Value = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?
        .post(VALHALLA_URL)
        .json(&body)
        .send()
        .await?
        .json()
        .await?;
    let s = &v["trip"]["summary"];
    match (s["length"].as_f64(), s["time"].as_f64()) {
        (Some(km), Some(t)) => Ok((km, t)),
        _ => anyhow::bail!("{}", v["error"].as_str().unwrap_or("no route home")),
    }
}

/// Time from `from` to `home` (both `[lon, lat]`), cached as described above.
pub async fn home_eta(from: [f64; 2], home: [f64; 2]) -> anyhow::Result<HomeEta> {
    use std::time::Instant;
    let distance_km = haversine_km(from[1], from[0], home[1], home[0]);
    if distance_km * 1000.0 < AT_HOME_M {
        return Ok(HomeEta { at_home: true, distance_km, km: 0.0, seconds: 0.0, free_seconds: None, source: String::new(), age_s: 0 });
    }
    let mut guard = HOME_STATE.lock().await;
    let st = guard.get_or_insert_with(HomeState::default);
    let cache = st.cache.as_ref().filter(|c| c.home == home);
    let plan = plan_home_refresh(cache.map(|c| c.at.elapsed()), cache.map_or(0.0, |c| metres(c.from, from)), st.google_at.map(|t| t.elapsed()));
    let cached = |st: &HomeState| {
        st.cache.as_ref().filter(|c| c.home == home).map(|c| HomeEta { distance_km, age_s: c.at.elapsed().as_secs(), ..c.eta.clone() })
    };
    if plan == HomeRefresh::Cached {
        if let Some(e) = cached(st) {
            return Ok(e);
        }
    }
    let local = valhalla_summary(from, home).await;
    let mut eta = None;
    if plan == HomeRefresh::Google {
        st.google_at = Some(Instant::now());
        match traffic(&RouteRequest { from, to: home, via: Vec::new() }).await {
            Ok(t) if t.seconds > 0.0 => {
                if let Ok((_, vs)) = &local {
                    if *vs > 0.0 {
                        st.ratio = Some((t.seconds / vs, home, Instant::now()));
                    }
                }
                eta = Some(HomeEta {
                    at_home: false,
                    distance_km,
                    km: t.km,
                    seconds: t.seconds,
                    free_seconds: Some(t.free_seconds),
                    source: "google".into(),
                    age_s: 0,
                });
            }
            Ok(_) => tracing::warn!("navigation: home ETA: empty Google answer"),
            Err(e) => tracing::info!("navigation: home ETA without traffic: {e:#}"),
        }
    }
    if eta.is_none() {
        match local {
            Ok((km, seconds)) => {
                let ratio = st.ratio.filter(|(_, h, at)| *h == home && at.elapsed() < HOME_RATIO_MAX_AGE).map(|(r, _, _)| r);
                eta = Some(HomeEta {
                    at_home: false,
                    distance_km,
                    km,
                    seconds: seconds * ratio.unwrap_or(1.0),
                    free_seconds: ratio.map(|_| seconds),
                    source: if ratio.is_some() { "valhalla+traffic" } else { "valhalla" }.into(),
                    age_s: 0,
                });
            }
            // Nothing new: an older answer beats none.
            Err(e) => return cached(st).ok_or(e),
        }
    }
    let eta = eta.expect("set above");
    st.cache = Some(HomeCache { home, from, at: Instant::now(), eta: eta.clone() });
    Ok(eta)
}

// ── Voice guidance ─────────────────────────────────────────────────
// Piper (offline neural TTS) into PulseAudio's default output, which is
// what the car receives. One phrase at a time: a new one waits.
const PIPER: &str = "/opt/piper/piper";
const VOICE: &str = "/var/lib/rascar/voice/fr_FR-siwis-medium.onnx";

/// Held while a voice plays, so two phrases never overlap.
pub(crate) static SPEAKING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn voice_rate() -> u32 {
    std::fs::read_to_string(format!("{VOICE}.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["audio"]["sample_rate"].as_u64())
        .unwrap_or(22050) as u32
}

pub async fn say(text: &str) -> anyhow::Result<()> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let text = text.trim();
    if text.is_empty() || text.len() > 500 {
        anyhow::bail!("nothing to say");
    }
    if !std::path::Path::new(PIPER).exists() || !std::path::Path::new(VOICE).exists() {
        anyhow::bail!("voice not installed ({PIPER}, {VOICE})");
    }
    let _turn = SPEAKING.lock().await;
    let mut piper = tokio::process::Command::new(PIPER)
        .args(["--model", VOICE, "--output_raw"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let pcm: Stdio = piper.stdout.take().ok_or_else(|| anyhow::anyhow!("piper has no stdout"))?.try_into()?;
    let mut play = tokio::process::Command::new("paplay")
        .args(["--raw", "--format=s16le", "--channels=1", &format!("--rate={}", voice_rate()), "--client-name=RasCar navigation"])
        .stdin(pcm)
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = piper.stdin.take().ok_or_else(|| anyhow::anyhow!("piper has no stdin"))?;
    stdin.write_all(format!("{}\n", text.replace('\n', " ")).as_bytes()).await?;
    drop(stdin);
    // Bounded: a stuck synthesis must not hold the voice forever.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let _ = piper.wait().await;
        let _ = play.wait().await;
    })
    .await
    .map_err(|_| anyhow::anyhow!("speech took longer than 30 s"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_words_all_match_and_the_last_is_a_prefix() {
        assert_eq!(fts_query("Main Str").as_deref(), Some("\"main\" AND \"str\"*"));
        // FTS operators and quotes typed by the user are just separators.
        assert_eq!(fts_query("a\" OR b*").as_deref(), Some("\"a\" AND \"or\" AND \"b\"*"));
        assert_eq!(fts_query("  ,; "), None);
        // Accented letters are kept (the tokenizer folds them).
        assert_eq!(fts_query("Église").as_deref(), Some("\"église\"*"));
    }

    #[test]
    fn polyline6_decodes_to_lon_lat() {
        // Two points: (38.5, -120.2) and (40.7, -120.95), precision 6.
        let pts = decode_polyline6("_izlhA~rlgdF_{geC~ywl@");
        assert_eq!(pts.len(), 2);
        assert!((pts[0][1] - 38.5).abs() < 1e-6 && (pts[0][0] + 120.2).abs() < 1e-6, "{pts:?}");
        assert!((pts[1][1] - 40.7).abs() < 1e-6 && (pts[1][0] + 120.95).abs() < 1e-6, "{pts:?}");
    }

    #[test]
    fn a_valhalla_answer_keeps_line_totals_and_maneuvers() {
        let v = serde_json::json!({ "trip": {
            "summary": { "length": 12.3, "time": 900.0 },
            "legs": [{ "shape": "_izlhA~rlgdF_{geC~ywl@", "maneuvers": [
                { "instruction": "Conduisez vers le nord.", "type": 1, "length": 1.2, "time": 60.0, "begin_shape_index": 0, "street_names": ["Main Street"] },
                { "instruction": "Tournez à gauche.", "type": 15, "length": 11.1, "time": 840.0, "begin_shape_index": 1,
                  "sign": { "exit_number_elements": [{ "text": "12" }], "exit_toward_elements": [{ "text": "City Centre" }] } }
            ]}]
        }});
        let r = parse_route(&v).unwrap();
        assert_eq!(r.shape.len(), 2);
        assert_eq!((r.km, r.seconds), (12.3, 900.0));
        assert_eq!(r.maneuvers[0].street, "Main Street");
        assert_eq!(r.maneuvers[1].instruction, "Tournez à gauche.");
        assert_eq!((r.maneuvers[1].exit.as_str(), r.maneuvers[1].toward.as_str()), ("12", "City Centre"));
        assert_eq!((r.maneuvers[1].kind, r.maneuvers[1].at, r.maneuvers[1].km), (15, 1, 11.1));
    }

    #[test]
    fn speed_limits_keep_posted_values_only() {
        let t = serde_json::json!({ "edges": [
            { "speed_limit": 50, "begin_shape_index": 0, "end_shape_index": 4 },
            { "speed_limit": 0, "begin_shape_index": 4, "end_shape_index": 6 },
            { "begin_shape_index": 6, "end_shape_index": 9 },
            { "speed_limit": 120, "begin_shape_index": 9, "end_shape_index": 30 }
        ]});
        assert_eq!(parse_limits(&t), vec![Limit { from: 0, to: 4, kmh: 50 }, Limit { from: 9, to: 30, kmh: 120 }]);
    }

    #[test]
    fn google_traffic_keeps_durations_and_slow_stretches() {
        // Polyline (precision 5) with 3 points: (38.5,-120.2) (40.7,-120.95) (43.252,-126.453).
        let v = serde_json::json!({ "routes": [{
            "duration": "1860s", "staticDuration": "1500s", "distanceMeters": 34800,
            "polyline": { "encodedPolyline": "_p~iF~ps|U_ulLnnqC_mqNvxq`@" },
            "travelAdvisory": { "speedReadingIntervals": [
                { "endPolylinePointIndex": 1, "speed": "NORMAL" },
                { "startPolylinePointIndex": 1, "endPolylinePointIndex": 2, "speed": "TRAFFIC_JAM" }
            ]}
        }]});
        let t = parse_traffic(&v).unwrap();
        assert_eq!((t.seconds, t.free_seconds, t.km), (1860.0, 1500.0, 34.8));
        assert_eq!(t.jams.len(), 1);
        assert_eq!(t.jams[0].speed, "TRAFFIC_JAM");
        assert_eq!(t.jams[0].coords.len(), 2);
        assert!((t.jams[0].coords[1][1] - 43.252).abs() < 1e-5, "{:?}", t.jams[0].coords);
    }

    #[test]
    fn a_google_error_is_reported() {
        let v = serde_json::json!({ "error": { "message": "API key not valid." } });
        assert_eq!(parse_traffic(&v).unwrap_err().to_string(), "API key not valid.");
    }

    #[test]
    fn home_eta_calls_google_at_most_every_five_minutes() {
        use std::time::Duration;
        let m = |s| Duration::from_secs(s);
        // First request: Google; again without a Google call ever: Google.
        assert_eq!(plan_home_refresh(None, 0.0, None), HomeRefresh::Google);
        // Fresh answer, car barely moved: cached.
        assert_eq!(plan_home_refresh(Some(m(60)), 200.0, Some(m(60))), HomeRefresh::Cached);
        // Big move inside 5 min: local recompute, Google untouched.
        assert_eq!(plan_home_refresh(Some(m(60)), 2000.0, Some(m(60))), HomeRefresh::Local);
        // Big move but the answer is only seconds old: cached.
        assert_eq!(plan_home_refresh(Some(m(10)), 2000.0, Some(m(10))), HomeRefresh::Cached);
        // 5 min old: Google again.
        assert_eq!(plan_home_refresh(Some(m(300)), 0.0, Some(m(300))), HomeRefresh::Google);
        // Home changed (no cache) but Google was just called: local.
        assert_eq!(plan_home_refresh(None, 0.0, Some(m(20))), HomeRefresh::Local);
    }

    #[test]
    fn distance_is_great_circle_km() {
        // Paris (Notre-Dame) → London (Trafalgar Square) ≈ 343.5 km.
        let d = haversine_km(48.8567, 2.3517, 51.5074, -0.1278);
        assert!((d - 343.5).abs() < 1.5, "{d}");
    }
}
