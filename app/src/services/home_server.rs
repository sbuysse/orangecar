//! Optional client for a self-hosted home-automation server ("home server").
//!
//! Disabled unless `~/.config/rascar/home-server.json` exists: without it the
//! Home page is hidden, the arrival popup never shows and nothing is sent over
//! the network. See `app/README.md` ("Home server") for the HTTP contract the
//! server has to implement.
//!
//! The server exposes an authenticated REST API (JWT from `POST /api/auth/login`)
//! on the home LAN (`lanUrl`) and, optionally, from anywhere (`remoteUrl`, a
//! self-signed certificate pinned by SHA-256). The car tries the LAN first and
//! falls back to the remote URL when the LAN is not reachable, so the Home page
//! works at home, on a phone hotspot and on 4G alike.
//!
//! Configuration (`~/.config/rascar/home-server.json`, keep it 0600: it holds
//! the car's password on the server):
//!
//! ```json
//! { "lanUrl": "http://homeserver.local", "remoteUrl": "https://example.org",
//!   "certSha256": "<hex>", "username": "car", "password": "…",
//!   "frontDoor": { "address": "1.2.3", "pulseSecs": 5 },
//!   "home": { "latitude": 0.0, "longitude": 0.0, "radiusMeters": 100 } }
//! ```
//!
//! Besides proxying the Home page, the service runs two background loops: it
//! records the car's position on the server (`POST /api/car`) and it detects
//! arriving home, which the dashboard turns into the "open the front door /
//! the garage?" popup.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::{Mutex, RwLock, broadcast};
use tracing::{info, warn};

use super::gps::GpsService;

const LAN_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const REMOTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// Default request timeout. Garage actions pass their own: the server waits up
/// to 60 s for the door to report its new state.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
pub const GARAGE_TIMEOUT: Duration = Duration::from_secs(75);

/// Push the car position when it moved this far since the last push…
const PUSH_MIN_DISTANCE_M: f64 = 25.0;
/// …or at least this often while it has a fix (so "last seen" stays current).
const PUSH_HEARTBEAT: Duration = Duration::from_secs(30 * 60);
/// A fix older than this is not the car's position any more.
const MAX_FIX_AGE_SECS: f64 = 10.0;
/// After an arrival popup, stay quiet this long (GPS jitter at the gate).
const ARRIVAL_COOLDOWN: Duration = Duration::from_secs(5 * 60);
/// Leaving is only recognised past radius × this, so jitter on the boundary
/// cannot produce a string of arrivals.
const LEAVE_HYSTERESIS: f64 = 1.5;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HomeServerConfig {
    pub lan_url: Option<String>,
    pub remote_url: Option<String>,
    /// SHA-256 (hex) of the remote server's certificate.
    pub cert_sha256: Option<String>,
    pub username: String,
    pub password: String,
    /// Door opened by a KNX pulse from the Home page / arrival popup; no
    /// door tile when absent.
    #[serde(default)]
    pub front_door: Option<FrontDoor>,
    pub home: Option<HomeArea>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontDoor {
    /// KNX group address of the door opener: written 0, then 1 after
    /// `pulseSecs` (the telegrams a typical wall push-button sends).
    pub address: String,
    #[serde(default = "default_pulse_secs")]
    pub pulse_secs: u64,
}

fn default_pulse_secs() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HomeArea {
    pub latitude: f64,
    pub longitude: f64,
    pub radius_meters: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    Lan,
    Remote,
}

/// A request body passed through unchanged (bytes + its content type).
pub struct RawBody {
    pub bytes: bytes::Bytes,
    pub content_type: Option<String>,
}

/// A home server answer, passed through to the dashboard as is.
pub struct Reply {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: bytes::Bytes,
}

#[derive(Default)]
struct Session {
    token: Option<String>,
    /// Endpoint that answered last; tried first next time.
    prefer: Option<Via>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Status {
    pub configured: bool,
    pub via: Option<Via>,
    pub error: Option<String>,
    /// Distance to home in metres, when home and a fix are known.
    pub home_distance_m: Option<f64>,
    pub last_position_push: Option<String>,
    /// A front door is configured (the page shows its tile).
    pub front_door: bool,
}

pub struct HomeServerService {
    cfg: Option<HomeServerConfig>,
    lan: Option<(String, reqwest::Client)>,
    remote: Option<(String, reqwest::Client)>,
    session: Mutex<Session>,
    status: RwLock<Status>,
    gps: Arc<GpsService>,
}

fn config_path() -> std::path::PathBuf {
    dirs::config_dir().unwrap_or_else(|| "/tmp".into()).join("rascar").join("home-server.json")
}

impl HomeServerService {
    pub fn new(gps: Arc<GpsService>) -> Self {
        let path = config_path();
        let cfg = match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<HomeServerConfig>(&s) {
                Ok(c) => Some(c),
                Err(e) => {
                    warn!("home-server: {} is invalid: {e}", path.display());
                    None
                }
            },
            Err(_) => {
                info!("home-server: no {} — Home page disabled", path.display());
                None
            }
        };
        Self::from_config(cfg, gps)
    }

    pub fn from_config(cfg: Option<HomeServerConfig>, gps: Arc<GpsService>) -> Self {
        let lan = cfg.as_ref().and_then(|c| c.lan_url.clone()).and_then(|u| {
            reqwest::Client::builder()
                .connect_timeout(LAN_CONNECT_TIMEOUT)
                .build()
                .map_err(|e| warn!("home-server: LAN client: {e}"))
                .ok()
                .map(|c| (u.trim_end_matches('/').to_string(), c))
        });
        let remote = cfg
            .as_ref()
            .and_then(|c| Some((c.remote_url.clone()?, c.cert_sha256.clone()?)))
            .and_then(|(u, pin)| match pinned_tls(&pin).and_then(pinned_client) {
                Ok(c) => Some((u.trim_end_matches('/').to_string(), c)),
                Err(e) => {
                    warn!("home-server: remote client: {e}");
                    None
                }
            });
        let status = Status {
            configured: cfg.is_some(),
            front_door: cfg.as_ref().is_some_and(|c| c.front_door.is_some()),
            ..Default::default()
        };
        Self { cfg, lan, remote, session: Mutex::default(), status: RwLock::new(status), gps }
    }

    /// Whether home-server.json was found and parsed.
    pub fn configured(&self) -> bool {
        self.cfg.is_some()
    }

    /// The house's position from home-server.json, as (lat, lon).
    pub fn home(&self) -> Option<(f64, f64)> {
        self.cfg.as_ref()?.home.as_ref().map(|h| (h.latitude, h.longitude))
    }

    pub async fn status(&self) -> Status {
        self.status.read().await.clone()
    }

    fn endpoint(&self, via: Via) -> Option<&(String, reqwest::Client)> {
        match via {
            Via::Lan => self.lan.as_ref(),
            Via::Remote => self.remote.as_ref(),
        }
    }

    /// Call the home server: `path` starts with `/api/`. LAN first (or whichever
    /// endpoint answered last), falling back to the other one ONLY when the
    /// connection could not be made — a request that reached the server is never
    /// replayed, since it may have moved a door.
    pub async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
        timeout: Duration,
    ) -> anyhow::Result<Reply> {
        let body = body.map(|b| RawBody {
            bytes: serde_json::to_vec(&b).unwrap_or_default().into(),
            content_type: Some("application/json".into()),
        });
        self.call_raw(method, path, body, timeout).await
    }

    /// `call` with an opaque body (any bytes, any content type).
    pub async fn call_raw(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<RawBody>,
        timeout: Duration,
    ) -> anyhow::Result<Reply> {
        let cfg = self.cfg.as_ref().ok_or_else(|| anyhow::anyhow!("the home server is not configured"))?;
        let order = match self.session.lock().await.prefer {
            Some(Via::Remote) => [Via::Remote, Via::Lan],
            _ => [Via::Lan, Via::Remote],
        };
        let mut last_err = anyhow::anyhow!("no home server endpoint configured");
        for via in order {
            let Some((base, client)) = self.endpoint(via) else { continue };
            match self.call_on(cfg, base, client, &method, path, body.as_ref(), timeout).await {
                Ok(r) => {
                    self.session.lock().await.prefer = Some(via);
                    let mut st = self.status.write().await;
                    st.via = Some(via);
                    st.error = None;
                    return Ok(r);
                }
                Err(e) if e.downcast_ref::<reqwest::Error>().is_some_and(|e| e.is_connect()) => {
                    last_err = e;
                }
                Err(e) => {
                    self.status.write().await.error = Some(e.to_string());
                    return Err(e);
                }
            }
        }
        let mut st = self.status.write().await;
        st.via = None;
        st.error = Some(format!("house unreachable: {last_err}"));
        Err(last_err)
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_on(
        &self,
        cfg: &HomeServerConfig,
        base: &str,
        client: &reqwest::Client,
        method: &reqwest::Method,
        path: &str,
        body: Option<&RawBody>,
        timeout: Duration,
    ) -> anyhow::Result<Reply> {
        // Tokens are signed by the one server behind both URLs, so a token from
        // either endpoint is good on the other.
        let mut token = self.session.lock().await.token.clone();
        for attempt in 0..2 {
            let t = match token.take() {
                Some(t) => t,
                None => self.login(cfg, base, client).await?,
            };
            let mut req = client.request(method.clone(), format!("{base}{path}")).bearer_auth(&t).timeout(timeout);
            if let Some(b) = body {
                if let Some(ct) = &b.content_type {
                    req = req.header(reqwest::header::CONTENT_TYPE, ct);
                }
                req = req.body(b.bytes.clone());
            }
            let resp = req.send().await?;
            // 401 = expired token (no refresh tokens). The request
            // was rejected before doing anything, so logging in again and
            // retrying once is safe.
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                self.session.lock().await.token = None;
                continue;
            }
            let status = resp.status().as_u16();
            let content_type =
                resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
            let body = resp.bytes().await?;
            return Ok(Reply { status, content_type, body });
        }
        anyhow::bail!("the home server refused the car's credentials")
    }

    async fn login(&self, cfg: &HomeServerConfig, base: &str, client: &reqwest::Client) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Login {
            token: String,
        }
        let resp = client
            .post(format!("{base}/api/auth/login"))
            .json(&serde_json::json!({ "username": cfg.username, "password": cfg.password }))
            .timeout(DEFAULT_TIMEOUT)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("home server login failed ({})", resp.status());
        }
        let token = resp.json::<Login>().await?.token;
        self.session.lock().await.token = Some(token.clone());
        Ok(token)
    }

    /// Write one KNX group value (`/api/knx` answers as soon as it is queued).
    pub async fn knx(&self, address: &str, value: &str) -> anyhow::Result<Reply> {
        self.call(
            reqwest::Method::POST,
            "/api/knx",
            Some(serde_json::json!({ "address": address, "value": value })),
            DEFAULT_TIMEOUT,
        )
        .await
    }

    /// Open the front door: 0, then 1 after `pulseSecs`, exactly like the
    /// kiosk. The second half runs detached, so the lock is released even if
    /// the popup that asked for it is gone by then.
    pub async fn open_front_door(self: &Arc<Self>) -> anyhow::Result<Reply> {
        let door = self
            .cfg
            .as_ref()
            .and_then(|c| c.front_door.clone())
            .ok_or_else(|| anyhow::anyhow!("no front door configured"))?;
        let first = self.knx(&door.address, "0").await?;
        if (200..300).contains(&first.status) {
            let me = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(door.pulse_secs)).await;
                // Retried: leaving the door in its "0" state is the one outcome
                // to avoid.
                for attempt in 1..=3 {
                    match me.knx(&door.address, "1").await {
                        Ok(r) if (200..300).contains(&r.status) => return,
                        Ok(r) => warn!("front door: release answered {} (attempt {attempt})", r.status),
                        Err(e) => warn!("front door: release failed: {e} (attempt {attempt})"),
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            });
        }
        Ok(first)
    }

    /// Latest value of every KNX group address, keyed dot-formatted ("1.2.3")
    /// like the topology. The bus only pushes changes, so this is rebuilt from
    /// the server's event log (newest first) and reduced here, so the car downloads a small map, not 1000 events.
    pub async fn values(&self) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
        let r = self.call(reqwest::Method::GET, "/api/events?action=knx_value&limit=1000", None, DEFAULT_TIMEOUT).await?;
        if !(200..300).contains(&r.status) {
            anyhow::bail!("home server events answered {}", r.status);
        }
        Ok(reduce_knx_events(&serde_json::from_slice(&r.body)?))
    }

    /// Background loops: position push + arrival detection.
    pub fn start(self: &Arc<Self>, events: broadcast::Sender<String>) {
        if self.cfg.is_none() {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move { me.position_loop().await });
        let me = self.clone();
        tokio::spawn(async move { me.arrival_loop(events).await });
    }

    /// A real, recent GPS fix (never a mock one), else None.
    async fn real_fix(&self) -> Option<(f64, f64)> {
        let g = self.gps.current.read().await;
        (g.has_fix && !g.is_mock && g.age_secs() <= MAX_FIX_AGE_SECS).then_some((g.latitude, g.longitude))
    }

    async fn position_loop(&self) {
        let mut last: Option<((f64, f64), Instant)> = None;
        let mut failures = 0u32;
        loop {
            if failures == 0 {
                tokio::time::sleep(Duration::from_secs(15)).await;
            } else {
                super::wait_for_refresh(failures, Duration::from_secs(600)).await;
            }
            let Some(pos) = self.real_fix().await else { continue };
            let due = match last {
                None => true,
                Some((p, t)) => distance_m(p, pos) >= PUSH_MIN_DISTANCE_M || t.elapsed() >= PUSH_HEARTBEAT,
            };
            if !due {
                continue;
            }
            let body = serde_json::json!({ "latitude": pos.0, "longitude": pos.1 });
            match self.call(reqwest::Method::POST, "/api/car", Some(body), DEFAULT_TIMEOUT).await {
                // Checked on the body, not the status: a server without the car
                // route may answer 200 with its web UI (an SPA fallback).
                Ok(r) if serde_json::from_slice::<serde_json::Value>(&r.body)
                    .is_ok_and(|v| v["success"] == serde_json::Value::Bool(true)) =>
                {
                    last = Some((pos, Instant::now()));
                    failures = 0;
                    self.status.write().await.last_position_push = Some(chrono::Local::now().to_rfc3339());
                }
                Ok(r) => {
                    if failures == 0 {
                        warn!("home-server: position push not recorded (HTTP {}; is /api/car routed?)", r.status);
                    }
                    failures += 1;
                }
                Err(e) => {
                    if failures == 0 {
                        warn!("home-server: position push failed: {e}");
                    }
                    failures += 1;
                }
            }
        }
    }

    async fn arrival_loop(&self, events: broadcast::Sender<String>) {
        let Some(home) = self.cfg.as_ref().and_then(|c| c.home.clone()) else {
            info!("home-server: no home area configured — no arrival popup");
            return;
        };
        let mut geo = Geofence::new(home.radius_meters);
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let Some(pos) = self.real_fix().await else { continue };
            let d = distance_m((home.latitude, home.longitude), pos);
            self.status.write().await.home_distance_m = Some(d.round());
            if geo.update(d, Instant::now()) {
                info!("home-server: arriving home ({d:.0} m) — offering the doors");
                let _ = events.send(serde_json::json!({ "type": "arrival" }).to_string());
            }
        }
    }
}

/// Outside → inside detector with hysteresis and a cooldown.
struct Geofence {
    radius: f64,
    /// None until the first fix: starting the car at home is not an arrival.
    inside: Option<bool>,
    last_arrival: Option<Instant>,
}

impl Geofence {
    fn new(radius: f64) -> Self {
        Self { radius, inside: None, last_arrival: None }
    }

    /// Feed a distance to home; true when this reading is an arrival.
    fn update(&mut self, distance: f64, now: Instant) -> bool {
        match self.inside {
            None => {
                self.inside = Some(distance <= self.radius);
                false
            }
            Some(true) => {
                if distance > self.radius * LEAVE_HYSTERESIS {
                    self.inside = Some(false);
                }
                false
            }
            Some(false) => {
                if distance > self.radius {
                    return false;
                }
                self.inside = Some(true);
                let cooled = self.last_arrival.is_none_or(|t| now.duration_since(t) >= ARRIVAL_COOLDOWN);
                if cooled {
                    self.last_arrival = Some(now);
                }
                cooled
            }
        }
    }
}

/// Newest `value_float` per destination from newest-first `knx_value` events.
fn reduce_knx_events(rows: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for row in rows.as_array().into_iter().flatten() {
        let p = &row["payload"];
        let (Some(dest), Some(v)) = (p["destination"].as_str(), p["value_float"].as_f64()) else { continue };
        out.entry(dest.replace('/', ".")).or_insert(serde_json::json!(v));
    }
    out
}

/// Great-circle distance in metres.
pub fn distance_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (la1, lo1, la2, lo2) = (a.0.to_radians(), a.1.to_radians(), b.0.to_radians(), b.1.to_radians());
    let h = ((la2 - la1) / 2.0).sin().powi(2) + la1.cos() * la2.cos() * ((lo2 - lo1) / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * h.sqrt().asin()
}

// --- Certificate pinning -------------------------------------------------

/// TLS for the remote endpoint, which presents a self-signed certificate: it
/// is accepted when its SHA-256 matches the pin, and the handshake signature
/// is still verified against it (so the certificate alone, without its key, is
/// not enough to impersonate the house).
fn pinned_tls(pin_hex: &str) -> anyhow::Result<rustls::ClientConfig> {
    let pin = decode_hex(pin_hex).ok_or_else(|| anyhow::anyhow!("certSha256 is not 64 hex digits"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert { pin, provider }))
        .with_no_client_auth();
    Ok(tls)
}

fn pinned_client(tls: rustls::ClientConfig) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .connect_timeout(REMOTE_CONNECT_TIMEOUT)
        .build()?)
}

fn decode_hex(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

#[derive(Debug)]
struct PinnedCert {
    pin: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use sha2::Digest;
        if sha2::Sha256::digest(end_entity.as_ref()).as_slice() == self.pin {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("certificate does not match the pinned home server certificate".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_is_right_on_a_known_pair() {
        // One degree of latitude is ~111.2 km.
        let d = distance_m((10.0, 20.0), (11.0, 20.0));
        assert!((d - 111_195.0).abs() < 50.0, "{d}");
        assert!(distance_m((48.85, 2.35), (48.85, 2.35)) < 1e-6);
    }

    #[test]
    fn starting_at_home_is_not_an_arrival() {
        let mut g = Geofence::new(100.0);
        let t = Instant::now();
        assert!(!g.update(20.0, t));
        assert!(!g.update(10.0, t));
    }

    #[test]
    fn arriving_fires_once_with_hysteresis_and_cooldown() {
        let mut g = Geofence::new(100.0);
        let t = Instant::now();
        assert!(!g.update(2_000.0, t));
        assert!(g.update(90.0, t), "outside -> inside is an arrival");
        assert!(!g.update(80.0, t));
        // Jitter just past the radius does not count as leaving.
        assert!(!g.update(120.0, t));
        assert!(!g.update(95.0, t));
        // Leave and come back within the cooldown: quiet.
        assert!(!g.update(500.0, t + Duration::from_secs(60)));
        assert!(!g.update(50.0, t + Duration::from_secs(120)));
        // Leave and come back after it: popup again.
        assert!(!g.update(500.0, t + Duration::from_secs(250)));
        assert!(!g.update(50.0, t + Duration::from_secs(290)), "still inside the 5 min cooldown");
        assert!(!g.update(500.0, t + Duration::from_secs(300)));
        assert!(g.update(50.0, t + Duration::from_secs(310)));
    }

    #[test]
    fn knx_values_keep_the_newest_reading_per_address() {
        let rows = serde_json::json!([
            { "payload": { "destination": "1/2/3", "value_float": 60.0 } },
            { "payload": { "destination": "1/2/3", "value_float": 0.0 } },
            { "payload": { "destination": "1/2/4", "value_float": 21.5 } },
            { "payload": { "destination": "1/2/5" } },
        ]);
        let m = reduce_knx_events(&rows);
        assert_eq!(m["1.2.3"], 60.0);
        assert_eq!(m["1.2.4"], 21.5);
        assert!(!m.contains_key("1.2.5"));
    }

    #[test]
    fn pin_must_be_64_hex_digits() {
        assert!(decode_hex(&"a5".repeat(32)).is_some());
        assert!(decode_hex("2fda").is_none());
        assert!(decode_hex(&"zz".repeat(32)).is_none());
    }

    /// An https:// home server with a self-signed certificate: only the right
    /// pin gets through.
    #[tokio::test]
    async fn remote_endpoint_uses_the_pinned_certificate() {
        use sha2::Digest;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let cert = rcgen::generate_simple_self_signed(vec!["homeserver.test".into()]).unwrap();
        let der = cert.cert.der().clone();
        let pin: String = sha2::Sha256::digest(der.as_ref()).iter().map(|b| format!("{b:02x}")).collect();
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![der], key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else { return };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                    let mut buf = vec![0u8; 4096];
                    let _ = tls.read(&mut buf).await;
                    let body = br#"{"ok":true}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = tls.write_all(head.as_bytes()).await;
                    let _ = tls.write_all(body).await;
                    let _ = tls.shutdown().await;
                });
            }
        });

        let make = |pin: &str| {
            let cfg: HomeServerConfig = serde_json::from_value(serde_json::json!({
                "remoteUrl": format!("https://127.0.0.1:{port}"), "certSha256": pin,
                "username": "car", "password": "pw"
            }))
            .unwrap();
            HomeServerService::from_config(Some(cfg), Arc::new(GpsService::new()))
        };
        let good = make(&pin);
        good.session.lock().await.token = Some("tok".into());
        let r = good.call(reqwest::Method::GET, "/api/garage", None, DEFAULT_TIMEOUT).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(&r.body[..], br#"{"ok":true}"#);
        assert_eq!(good.status().await.via, Some(Via::Remote));

        let bad = make(&"00".repeat(32));
        bad.session.lock().await.token = Some("tok".into());
        assert!(bad.call(reqwest::Method::GET, "/api/garage", None, DEFAULT_TIMEOUT).await.is_err(), "a wrong pin must fail");
    }

    #[test]
    fn config_parses_with_defaults() {
        let c: HomeServerConfig =
            serde_json::from_str(r#"{"lanUrl":"http://x","username":"u","password":"p"}"#).unwrap();
        assert!(c.front_door.is_none());
        assert!(c.home.is_none());
        let c: HomeServerConfig =
            serde_json::from_str(r#"{"username":"u","password":"p","frontDoor":{"address":"1.2.3"}}"#).unwrap();
        let door = c.front_door.unwrap();
        assert_eq!(door.address, "1.2.3");
        assert_eq!(door.pulse_secs, 5);
    }
}
