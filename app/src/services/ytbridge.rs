//! YouTube Music through the car browser's own session.
//!
//! The Google session never leaves the browser. Copying its cookies into a
//! second client (ytmusicapi with browser-auth headers) signed the car out
//! within the hour: the copy never rotated `__Secure-*PSIDTS` while Chromium
//! did, and Google treats two clients on one session as a stolen cookie.
//!
//! Instead rascar-app keeps ONE background tab of the car browser on
//! https://music.youtube.com/ (idle, muted, never brought to the front; a
//! live page of the site is also what keeps the session rotated), and the
//! Python helper sends its YouTube Music API calls here
//! (`POST /internal/ytm/fetch`). Each one runs as a same-origin `fetch()`
//! inside that tab: the browser attaches its own cookies, and the page
//! computes the `SAPISIDHASH` authorization from its own `SAPISID` cookie.
//! Nothing that identifies the session is returned, stored or logged.
//!
//! The endpoint answers only loopback peers carrying this process's random
//! token (handed to the helper in its environment), and only for
//! `https://music.youtube.com/youtubei/…` URLs.
//!
//! Keeping the Google session alive in a car (findings from 2026-10):
//! - The board has no RTC and every drive ends in a power cut, so each boot
//!   starts on the clock `fake-hwclock` last saved (measured: 16 min, 2.3 h,
//!   5.2 h and 26.7 h behind) until GPS or NTP corrects it. The page signs
//!   every API call with a timestamped `SAPISIDHASH`; a stale timestamp is
//!   what a replayed authorization looks like. So the tab is only opened, and
//!   calls only run, once the clock is trusted (`clock_trusted`), and the tab
//!   is closed if the clock steps under it.
//! - Chromium writes cookie changes to disk every ~30 s. A power cut right
//!   after Google rotated a session cookie brings the older value back at the
//!   next boot. The tab is therefore opened on demand and closed when idle
//!   (`IDLE_CLOSE`): when the car is switched off, Google's cookies have
//!   usually been quiet long enough to be on disk.
//! - A signed-in → signed-out transition is logged loudly with the clock
//!   state, so the next loss can be dated without guesswork.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tracing::{info, warn};

use super::apps::{cdp_call, cdp_call_timeout, list_targets};

/// What the background tab shows. Measured on the Pi (2026-09-29): the idle
/// hidden tab costs ~1.7 % of one core, so the real page (which rotates the
/// session cookies by itself) is used rather than a lighter same-origin path.
pub const TAB_URL: &str = "https://music.youtube.com/";
const ORIGIN: &str = "https://music.youtube.com";
/// The only API the helper may reach through the browser.
const API_PREFIX: &str = "/youtubei/";
/// Header carrying the per-process token (`RASCAR_YTM_BRIDGE_TOKEN`).
pub const TOKEN_HEADER: &str = "x-rascar-bridge";
/// Bound on one bridged call (the helper's own ceiling is 17 s).
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const KEEPER_PERIOD: Duration = Duration::from_secs(30);
/// Close the tab after this long without a bridged call.
const IDLE_CLOSE: Duration = Duration::from_secs(5 * 60);
/// systemd-timesyncd creates this once NTP synchronised the clock and touches
/// it on every successful poll (at most every 2048 s).
const NTP_SYNCED_FLAG: &str = "/run/systemd/timesync/synchronized";
/// An NTP flag older than this (or dated in the future) no longer vouches for
/// the clock: the clock moved since, or we went offline long ago.
const NTP_FLAG_MAX_AGE: Duration = Duration::from_secs(3 * 3600);
/// The car's NMEA feed (written by rascar-carplay, read by services/gps.rs).
const CAR_GPS_PATH: &str = "/var/lib/rascar-debug/car-gps.nmea";
/// The GPS time must agree with the system clock within this.
const GPS_MAX_SKEW: i64 = 30;
/// ...and must have moved recently (a live feed, not the last drive's tail).
const GPS_LIVE_WITHIN: Duration = Duration::from_secs(20);
/// A wall-clock step larger than this between two keeper ticks closes the tab.
const CLOCK_STEP: i64 = 10;

/// DevTools target id of the background tab.
static TAB: Mutex<Option<String>> = Mutex::new(None);

fn tab_id() -> Option<String> {
    TAB.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn set_tab_id(id: Option<String>) {
    *TAB.lock().unwrap_or_else(|p| p.into_inner()) = id;
}

/// Whether `target_id` is the background YouTube Music tab (tab cleaners
/// and the dashboard watchdog leave it alone).
pub fn is_bridge_tab(target_id: &str) -> bool {
    tab_id().as_deref() == Some(target_id)
}

// --- Clock trust -----------------------------------------------------------

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// What vouches for the system clock right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClockSource {
    Ntp,
    Gps,
}

/// An NTP flag touched `now - mtime` seconds ago still vouches for the clock.
/// A flag dated in the future means the clock was set back since.
fn ntp_flag_vouches(now: i64, mtime: i64) -> bool {
    (-5..=NTP_FLAG_MAX_AGE.as_secs() as i64).contains(&(now - mtime))
}

/// The GPS time and the system clock agree.
fn gps_agrees(gps: i64, now: i64) -> bool {
    (gps - now).abs() <= GPS_MAX_SKEW
}

/// UTC seconds since the epoch from a valid `$..RMC` sentence (status `A`).
/// Same rules as `carplay/src/gps_clock.rs`, which steps the clock from it.
fn rmc_epoch(sentence: &str) -> Option<i64> {
    let body = sentence.trim().split('*').next()?;
    let f: Vec<&str> = body.split(',').collect();
    if f.len() < 10 || !f[0].ends_with("RMC") || f[2] != "A" {
        return None;
    }
    let (t, d) = (f[1], f[9]);
    if t.len() < 6 || d.len() != 6 || !t.is_char_boundary(6) {
        return None;
    }
    let num = |s: &str| s.parse::<i64>().ok();
    let (hh, mm, ss) = (num(&t[0..2])?, num(&t[2..4])?, num(&t[4..6])?);
    let (day, month, yy) = (num(&d[0..2])?, num(&d[2..4])?, num(&d[4..6])?);
    if hh > 23 || mm > 59 || ss > 60 || !(1..=31).contains(&day) || !(1..=12).contains(&month) {
        return None;
    }
    // Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { 2000 + yy - 1 } else { 2000 + yy };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// The newest valid RMC time at the end of the car's NMEA file.
fn last_rmc(path: &str) -> Option<i64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(4096))).ok()?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).ok()?;
    String::from_utf8_lossy(&tail).lines().rev().find_map(rmc_epoch)
}

/// Last RMC time read, when it was first seen, and whether it ever changed
/// between two reads (the file keeps the previous drive's tail: a value
/// only counts once it is seen moving).
static GPS_SEEN: Mutex<Option<(i64, std::time::Instant, bool)>> = Mutex::new(None);

/// The car's live GPS time, `None` while the feed is not moving.
fn gps_live_time() -> Option<i64> {
    let path = std::env::var("RASCAR_CAR_GPS_PATH").unwrap_or_else(|_| CAR_GPS_PATH.to_string());
    let t = last_rmc(&path)?;
    let mut seen = GPS_SEEN.lock().unwrap_or_else(|p| p.into_inner());
    match *seen {
        Some((prev, _, _)) if prev != t => {
            *seen = Some((t, std::time::Instant::now(), true));
            Some(t)
        }
        Some((_, at, true)) if at.elapsed() < GPS_LIVE_WITHIN => Some(t),
        Some(_) => None,
        None => {
            *seen = Some((t, std::time::Instant::now(), false));
            None
        }
    }
}

/// Whether the system clock can be shown to Google, and why. NTP first; else
/// a live car GPS feed that agrees with the clock (rascar-carplay steps the
/// clock to it, so agreement means the step happened).
pub fn clock_source() -> Option<ClockSource> {
    let now = unix_now();
    let ntp_mtime = std::fs::metadata(NTP_SYNCED_FLAG)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    if ntp_mtime.is_some_and(|m| ntp_flag_vouches(now, m)) {
        return Some(ClockSource::Ntp);
    }
    gps_live_time().filter(|&g| gps_agrees(g, now)).map(|_| ClockSource::Gps)
}

/// `clock_source`, giving a GPS feed one second to show it is moving (the
/// first read of the file after start can only prime the tracker).
async fn clock_source_settled() -> Option<ClockSource> {
    if let Some(s) = clock_source() {
        return Some(s);
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    clock_source()
}

/// Wall-clock and monotonic readings at the last keeper tick.
static CLOCK_BASE: Mutex<Option<(i64, std::time::Instant)>> = Mutex::new(None);

/// Size of the wall-clock step since the previous call, if there was one.
fn clock_stepped() -> Option<i64> {
    let (wall, mono) = (unix_now(), std::time::Instant::now());
    let mut base = CLOCK_BASE.lock().unwrap_or_else(|p| p.into_inner());
    let step = base.map(|(w, m)| (wall - w) - mono.duration_since(m).as_secs() as i64);
    *base = Some((wall, mono));
    step.filter(|s| s.abs() > CLOCK_STEP)
}

// --- Session tracking -------------------------------------------------------

/// Last bridged call (the tab closes after `IDLE_CLOSE` without one).
static LAST_USE: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Whether the page held a session at the last call, and since when (unix s).
static SESSION: Mutex<Option<(bool, i64)>> = Mutex::new(None);

fn uptime_secs() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split('.').next().and_then(|n| n.parse().ok()))
        .unwrap_or(0)
}

/// Log every change of the browser's YouTube Music session.
fn note_session(signed_in: bool, clock: ClockSource) {
    let now = unix_now();
    let mut state = SESSION.lock().unwrap_or_else(|p| p.into_inner());
    match (*state, signed_in) {
        (Some((true, since)), false) => warn!(
            "ytbridge: YouTube Music session LOST: the car browser no longer holds a Google session \
             (it was there for {} s; clock from {clock:?}; uptime {} s) — sign in again from the YouTube Music page",
            now - since,
            uptime_secs()
        ),
        (Some((true, _)), true) | (Some((false, _)), false) => return,
        (_, true) => info!("ytbridge: the car browser holds a YouTube Music session"),
        (None, false) => info!("ytbridge: the car browser is not signed in to YouTube Music"),
    }
    *state = Some((signed_in, now));
}

/// Random token the helper must present, fixed for the life of the process.
pub fn token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let mut bytes = [0u8; 24];
        if std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes)).is_err() {
            // Never an empty/guessable token: fall back to time + pid entropy.
            let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
                ^ (std::process::id() as u128) << 64;
            bytes[..16].copy_from_slice(&seed.to_le_bytes());
        }
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    })
}

/// A call the helper asks the browser to make.
#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct FetchRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
}

/// The browser's answer. `signed_in` = the page holds a first-party
/// `SAPISID` (the call went out authenticated).
#[derive(Debug, Deserialize, Serialize)]
pub struct FetchReply {
    pub status: u16,
    #[serde(default, rename = "statusText")]
    pub status_text: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: String,
    #[serde(default, rename = "signedIn")]
    pub signed_in: bool,
}

/// Only the kiosk's own machine may drive the browser session.
pub fn peer_allowed(peer: &SocketAddr) -> bool {
    match peer.ip() {
        std::net::IpAddr::V4(ip) => ip.is_loopback(),
        // `::ffff:127.0.0.1` when the listener is dual-stack.
        std::net::IpAddr::V6(ip) => ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

/// GET/POST to the YouTube Music API on its exact origin, nothing else.
pub fn url_allowed(method: &str, url: &str) -> bool {
    if !matches!(method, "GET" | "POST") {
        return false;
    }
    let Ok(u) = url::Url::parse(url) else { return false };
    u.scheme() == "https"
        && u.host_str() == Some("music.youtube.com")
        && u.port().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && u.path().starts_with(API_PREFIX)
        && !u.path().contains("/../")
}

/// Headers the helper may pass on. Everything that carries or claims an
/// identity (Cookie, Authorization, X-Goog-AuthUser, Origin, the visitor id)
/// is set by the page itself; transport headers are the browser's business.
pub fn scrub_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    const KEEP: &[&str] = &["accept", "accept-language", "content-type", "x-goog-request-time", "x-youtube-client-name", "x-youtube-client-version"];
    headers
        .iter()
        .filter(|(k, v)| KEEP.contains(&k.to_ascii_lowercase().as_str()) && !v.contains(['\r', '\n']))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect()
}

/// SHA-1 based `SAPISIDHASH` exactly as YouTube's web client builds it:
/// `SAPISIDHASH <ts>_<sha1("<ts> <SAPISID> <origin>")>`. `ts` in seconds.
pub const SAPISID_HASH_JS: &str = r#"async (ts, sapisid, origin) => {
  const data = new TextEncoder().encode(ts + ' ' + sapisid + ' ' + origin);
  const hex = [...new Uint8Array(await crypto.subtle.digest('SHA-1', data))].map(b => b.toString(16).padStart(2, '0')).join('');
  return 'SAPISIDHASH ' + ts + '_' + hex;
}"#;

/// Runs in the tab with the scrubbed request. Refuses to run anywhere but
/// on the YouTube Music origin (the tab could have been navigated away).
const FETCH_JS: &str = r#"async (req, sapisidHash) => {
  if (location.origin !== 'https://music.youtube.com') return { error: 'origin ' + location.origin };
  const jar = {};
  for (const c of document.cookie.split('; ')) { const i = c.indexOf('='); if (i > 0) jar[c.slice(0, i)] = c.slice(i + 1); }
  const headers = new Headers(req.headers);
  headers.set('X-Origin', location.origin);
  // Signed out, the browser still holds a third-party __Secure-3PAPISID:
  // only a first-party SAPISID means a Google session to authorize with.
  const signedIn = !!jar.SAPISID;
  if (signedIn) {
    headers.set('Authorization', await sapisidHash(Math.floor(Date.now() / 1000), jar.SAPISID, location.origin));
    headers.set('X-Goog-AuthUser', '0');
  }
  const visitor = window.ytcfg && ytcfg.get && ytcfg.get('VISITOR_DATA');
  if (visitor) headers.set('X-Goog-Visitor-Id', visitor);
  const r = await fetch(req.url, { method: req.method, headers, body: req.body == null ? undefined : req.body, credentials: 'include' });
  const out = {};
  for (const k of ['content-type']) { const v = r.headers.get(k); if (v) out[k] = v; }
  return { status: r.status, statusText: r.statusText, headers: out, body: await r.text(), signedIn };
}"#;

/// The expression evaluated in the tab for `req` (already scrubbed).
pub fn fetch_expression(req: &FetchRequest) -> Result<String> {
    Ok(format!("({FETCH_JS})({}, {SAPISID_HASH_JS})", serde_json::to_string(req)?))
}

/// Mutes and pauses whatever media the page holds: this tab must never play.
const HUSH_JS: &str = "document.querySelectorAll('video,audio').forEach(m => { m.muted = true; if (!m.paused) m.pause(); }); 'ok'";

/// The DevTools websocket of the background tab, if it is up.
async fn tab_ws() -> Result<String> {
    let id = tab_id().ok_or_else(|| anyhow!("no YouTube Music tab yet"))?;
    list_targets()
        .await?
        .into_iter()
        .find(|t| t.id == id && !t.ws.is_empty())
        .map(|t| t.ws)
        .ok_or_else(|| anyhow!("the YouTube Music tab is gone"))
}

/// Run a validated request in the tab.
pub async fn fetch(req: FetchRequest) -> Result<FetchReply> {
    let req = FetchRequest { headers: scrub_headers(&req.headers), ..req };
    // Never sign a Google call with a clock that may be hours off.
    let clock = clock_source_settled().await.ok_or_else(|| anyhow!("system clock not confirmed yet (no NTP, no live GPS)"))?;
    *LAST_USE.lock().unwrap_or_else(|p| p.into_inner()) = Some(std::time::Instant::now());
    let ws = match tab_ws().await {
        Ok(ws) => ws,
        Err(_) => {
            // Opened on demand, e.g. the first call after a browser restart.
            ensure_tab().await?;
            tab_ws().await?
        }
    };
    let reply = cdp_call_timeout(&ws, "Runtime.evaluate", serde_json::json!({
        "expression": fetch_expression(&req)?, "awaitPromise": true, "returnByValue": true,
    }), FETCH_TIMEOUT).await?;
    if let Some(ex) = reply.pointer("/result/exceptionDetails") {
        // A network failure inside the page ("Failed to fetch"): no session data in it.
        let text = ex.pointer("/exception/description").or_else(|| ex.get("text")).and_then(|v| v.as_str()).unwrap_or("exception");
        return Err(anyhow!("in-page fetch failed: {}", text.lines().next().unwrap_or_default()));
    }
    let value = reply.pointer("/result/result/value").cloned().context("no value from the tab")?;
    if let Some(e) = value.get("error").and_then(|e| e.as_str()) {
        return Err(anyhow!("tab refused: {e}"));
    }
    let reply: FetchReply = serde_json::from_value(value)?;
    note_session(reply.signed_in, clock);
    Ok(reply)
}

/// Close the background tab (it is reopened by the next call).
async fn close_tab(reason: &str) {
    let Some(id) = tab_id() else { return };
    set_tab_id(None);
    match super::signin::browser_ws().await {
        Ok(browser) => {
            if let Err(e) = cdp_call(&browser, "Target.closeTarget", serde_json::json!({ "targetId": id })).await {
                warn!("ytbridge: could not close the YouTube Music tab: {e:#}");
                return;
            }
            info!("ytbridge: closed the YouTube Music tab ({reason})");
        }
        Err(e) => warn!("ytbridge: could not close the YouTube Music tab: {e:#}"),
    }
}

/// Make sure the background tab exists: adopt a music.youtube.com page left
/// by a previous run of this process (never the sign-in tab), else open one
/// in the background. Also puts it back on YouTube Music if it wandered off.
pub async fn ensure_tab() -> Result<()> {
    if clock_source_settled().await.is_none() {
        return Err(anyhow!("system clock not confirmed yet: YouTube Music waits for NTP or the car's GPS"));
    }
    let pages: Vec<_> = list_targets().await?.into_iter().filter(|t| t.kind == "page").collect();
    let on_music = |url: &str| url.starts_with(ORIGIN) && url[ORIGIN.len()..].starts_with('/');
    if let Some(id) = tab_id() {
        if let Some(t) = pages.iter().find(|t| t.id == id) {
            if !on_music(&t.url) && !t.ws.is_empty() {
                info!("ytbridge: the YouTube Music tab left the site, sending it back");
                cdp_call(&t.ws, "Page.navigate", serde_json::json!({ "url": TAB_URL })).await?;
            }
            return Ok(());
        }
        set_tab_id(None);
    }
    if let Some(t) = pages.iter().find(|t| on_music(&t.url) && !super::signin::is_signin_tab(&t.id)) {
        info!("ytbridge: adopted the existing YouTube Music tab");
        set_tab_id(Some(t.id.clone()));
        return Ok(());
    }
    let browser = super::signin::browser_ws().await?;
    let created = cdp_call(&browser, "Target.createTarget", serde_json::json!({ "url": TAB_URL, "background": true })).await?;
    let id = created.pointer("/result/targetId").and_then(|v| v.as_str()).context("createTarget gave no id")?;
    info!("ytbridge: opened the background YouTube Music tab");
    set_tab_id(Some(id.to_string()));
    // Wait for the page (its cookies and ytcfg) before the first call uses it.
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Ok(ws) = tab_ws().await {
            if super::apps::evaluate(&ws, "document.readyState").await.is_ok_and(|s| s == "complete") {
                break;
            }
        }
    }
    Ok(())
}

/// After a sign-in: reload the tab so the page runs with the new session.
pub async fn reload() -> Result<()> {
    ensure_tab().await?;
    let ws = tab_ws().await?;
    cdp_call(&ws, "Page.navigate", serde_json::json!({ "url": TAB_URL })).await?;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if super::apps::evaluate(&ws, "location.origin + ' ' + document.readyState").await.is_ok_and(|s| s == format!("{ORIGIN} complete")) {
            return Ok(());
        }
    }
    Err(anyhow!("the YouTube Music tab did not finish loading"))
}

/// Watch the tab for as long as the app runs: it is opened on demand by
/// `fetch`, kept silent and on the site while in use, and closed when idle,
/// when the clock steps, or when the clock is no longer trusted.
pub fn spawn_keeper() {
    tokio::spawn(async {
        let mut was_trusted = None;
        loop {
            let stepped = clock_stepped();
            // Read every tick, tab or not: it also lets the GPS tracker see the feed move.
            let clock = clock_source();
            if was_trusted != Some(clock.is_some()) {
                match clock {
                    Some(src) => info!("ytbridge: system clock confirmed by {src:?}: YouTube Music may use the Google session"),
                    None => info!("ytbridge: system clock not confirmed (no NTP, no live GPS): YouTube Music keeps away from Google"),
                }
                was_trusted = Some(clock.is_some());
            }
            if tab_id().is_some() {
                let idle = LAST_USE.lock().unwrap_or_else(|p| p.into_inner()).is_none_or(|t| t.elapsed() > IDLE_CLOSE);
                if let Some(s) = stepped {
                    close_tab(&format!("the system clock stepped by {s:+} s")).await;
                } else if clock.is_none() {
                    close_tab("the system clock is no longer confirmed").await;
                } else if idle {
                    close_tab("idle").await;
                } else {
                    match ensure_tab().await {
                        Ok(()) => {
                            if let Ok(ws) = tab_ws().await {
                                let _ = super::apps::evaluate(&ws, HUSH_JS).await;
                            }
                        }
                        Err(e) => warn!("ytbridge: cannot keep the YouTube Music tab: {e:#}"),
                    }
                }
            }
            tokio::time::sleep(KEEPER_PERIOD).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_youtube_music_api_is_reachable() {
        assert!(url_allowed("POST", "https://music.youtube.com/youtubei/v1/browse?alt=json&key=k"));
        assert!(url_allowed("GET", "https://music.youtube.com/youtubei/v1/next"));
        assert!(!url_allowed("DELETE", "https://music.youtube.com/youtubei/v1/browse"));
        assert!(!url_allowed("POST", "http://music.youtube.com/youtubei/v1/browse"));
        assert!(!url_allowed("POST", "https://www.youtube.com/youtubei/v1/browse"));
        assert!(!url_allowed("POST", "https://music.youtube.com.evil.example/youtubei/v1/browse"));
        assert!(!url_allowed("POST", "https://music.youtube.com:444/youtubei/v1/browse"));
        assert!(!url_allowed("POST", "https://u:p@music.youtube.com/youtubei/v1/browse"));
        assert!(!url_allowed("GET", "https://music.youtube.com/"));
        assert!(!url_allowed("GET", "https://music.youtube.com/playlist?list=x"));
        assert!(!url_allowed("POST", "https://accounts.google.com/youtubei/v1/browse"));
        // `url` normalises dot segments: this one leaves the API prefix.
        assert!(!url_allowed("GET", "https://music.youtube.com/youtubei/../signin"));
        assert!(!url_allowed("POST", "not a url"));
    }

    #[test]
    fn only_loopback_peers() {
        assert!(peer_allowed(&"127.0.0.1:5000".parse().unwrap()));
        assert!(peer_allowed(&"[::1]:5000".parse().unwrap()));
        assert!(peer_allowed(&"[::ffff:127.0.0.1]:5000".parse().unwrap()));
        assert!(!peer_allowed(&"10.0.0.10:5000".parse().unwrap()));
        assert!(!peer_allowed(&"[::ffff:10.0.0.2]:5000".parse().unwrap()));
        assert!(!peer_allowed(&"0.0.0.0:5000".parse().unwrap()));
    }

    #[test]
    fn identity_headers_never_pass() {
        let h: BTreeMap<String, String> = [
            ("Cookie", "SAPISID=bridge"),
            ("authorization", "SAPISIDHASH 1_x"),
            ("X-Goog-AuthUser", "0"),
            ("X-Goog-Visitor-Id", "v"),
            ("origin", "https://music.youtube.com"),
            ("user-agent", "Firefox"),
            ("accept-encoding", "gzip"),
            ("content-encoding", "gzip"),
            ("Content-Type", "application/json"),
            ("accept", "*/*"),
            ("x-youtube-client-version", "1.2\r\nCookie: x"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let kept = scrub_headers(&h);
        assert_eq!(kept.keys().map(String::as_str).collect::<Vec<_>>(), ["accept", "content-type"]);
        assert_eq!(kept["content-type"], "application/json");
    }

    #[test]
    fn the_request_is_embedded_as_json_data_not_code() {
        let req = FetchRequest {
            method: "POST".into(),
            url: "https://music.youtube.com/youtubei/v1/browse".into(),
            headers: BTreeMap::new(),
            body: Some("\"});alert(1);({\"".into()),
        };
        let expr = fetch_expression(&req).unwrap();
        let json = serde_json::to_string(&req).unwrap();
        assert!(expr.contains(&json));
        assert!(expr.starts_with("(async (req, sapisidHash)"));
    }

    #[test]
    fn rmc_time_from_the_cars_feed() {
        // Format of a real line from the car, neutral position.
        let s = "$GPRMC,120001.00,A,4851.402000,N,00221.102000,E,0.000,305.47,051026,,,E*61";
        assert_eq!(rmc_epoch(s), Some(1_791_201_601));
        assert_eq!(rmc_epoch(&s.replace(",A,", ",V,")), None, "no fix");
        assert_eq!(rmc_epoch("$GPGGA,120001.00,4851.4020,N"), None);
        assert_eq!(rmc_epoch("$GPRMC,1,A"), None);
        assert_eq!(rmc_epoch("$GPRMC,12é000.00,A,1,N,1,E,0,0,051026,,,E"), None, "never panics on bytes");
    }

    #[test]
    fn last_rmc_reads_the_tail_of_the_feed() {
        let path = std::env::temp_dir().join(format!("ytbridge-nmea-{}", std::process::id()));
        let mut body = String::new();
        for _ in 0..200 {
            body.push_str("$GPGGA,120000.00,4851.4020,N,00221.1020,E,1,8,1.0,100,M,,,,*00\n");
        }
        body.push_str("$GPRMC,120000.00,A,4851.402000,N,00221.102000,E,0.000,305.47,051026,,,E*60\n");
        body.push_str("$PASCD,43200.000,C,P,1,1,0.00,0.000*61\n");
        std::fs::write(&path, body).unwrap();
        assert_eq!(last_rmc(path.to_str().unwrap()), Some(1_791_201_600));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ntp_flag_only_vouches_while_fresh_and_not_from_the_future() {
        assert!(ntp_flag_vouches(1000, 1000));
        assert!(ntp_flag_vouches(1000 + 3600, 1000));
        assert!(!ntp_flag_vouches(1000 + 4 * 3600, 1000), "stale flag");
        // fake-hwclock set the clock back after NTP touched the flag.
        assert!(!ntp_flag_vouches(1000, 1000 + 600));
        assert!(gps_agrees(1000, 1020));
        assert!(!gps_agrees(1000, 1000 + 18_868), "the 2026-10-05 boot ran 5.2 h behind");
    }

    #[test]
    fn token_is_long_and_stable() {
        assert_eq!(token().len(), 48);
        assert!(token().bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(token(), token());
    }

    /// Runs the in-page hash with node (skipped where node is absent) against
    /// a vector computed with Python's hashlib:
    /// sha1("1700000000 abcDEF/ghi https://music.youtube.com").
    #[test]
    fn sapisidhash_matches_youtube_format() {
        let script = format!("({SAPISID_HASH_JS})(1700000000, 'abcDEF/ghi', 'https://music.youtube.com').then(v => process.stdout.write(v))");
        let Ok(out) = std::process::Command::new("node").arg("-e").arg(&script).output() else {
            eprintln!("node not installed: SAPISIDHASH check skipped");
            return;
        };
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "SAPISIDHASH 1700000000_f05a3b9d7e73d0897df18cf793218762a40e8803"
        );
    }
}
