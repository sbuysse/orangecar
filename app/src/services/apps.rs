//! Embedded apps (YouTube, Netflix, Disney+, Free TV, Google Meet, Teams) and
//! the car browser's DevTools session.
//!
//! The apps run in iframes of the dashboard page itself (the browser's
//! frame-unlock extension lifts their anti-framing headers), so the dock,
//! status bar and picture-in-picture work around them like around Maps. This
//! service only does what a page cannot:
//! - validates meeting links and pre-grants mic/camera (no prompt in the car);
//! - makes the playing video fill the frame while it is shown in PiP;
//! - keeps one persistent DevTools session on the dashboard tab that feeds the
//!   car's GPS position to the frames and types the on-screen keyboard into
//!   whichever field has focus (even inside a cross-origin frame).

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use super::gps::GpsService;

/// DevTools HTTP endpoint (the CarPlay renderer starts Chromium with
/// `--remote-debugging-address=127.0.0.1 --remote-debugging-port=9222`).
pub(super) const DEVTOOLS: &str = "http://127.0.0.1:9222";
/// What the kiosk shows (rascar-carplay's `DASHBOARD_URL`).
const DASHBOARD_URL: &str = "http://localhost:8080/";

#[derive(Debug, Serialize)]
pub struct StreamingApp {
    pub id: &'static str,
    pub name: &'static str,
    pub url: &'static str,
    /// Registrable hosts that belong to the app (subdomains included).
    #[serde(skip)]
    pub hosts: &'static [&'static str],
}

/// The only URLs the dashboard can open: the page picks an id, never a URL.
pub const APPS: &[StreamingApp] = &[
    StreamingApp { id: "youtube", name: "YouTube", url: "https://www.youtube.com/", hosts: &["youtube.com"] },
    StreamingApp { id: "netflix", name: "Netflix", url: "https://www.netflix.com/browse", hosts: &["netflix.com"] },
    StreamingApp { id: "disney", name: "Disney+", url: "https://www.disneyplus.com/", hosts: &["disneyplus.com"] },
    StreamingApp { id: "freetv", name: "Free TV", url: "https://tv.free.fr/", hosts: &["tv.free.fr", "oqee.tv"] },
    // Meetings: opened from a calendar event's link (see `prepare_meeting`), not a tile.
    StreamingApp { id: "meet", name: "Google Meet", url: "https://meet.google.com/", hosts: &["meet.google.com"] },
    StreamingApp { id: "teams", name: "Teams", url: "https://teams.microsoft.com/", hosts: &["teams.microsoft.com", "teams.live.com"] },
    // Android Auto from a phone: a page of this dashboard (served from
    // frontend/), fed by the rascar-aa service. No hosts: it is same-origin,
    // so no DevTools frame target and nothing for the PiP/sign-in helpers.
    StreamingApp { id: "androidauto", name: "Android Auto", url: "/aa.html", hosts: &[] },
];

/// Apps a meeting link may be framed as.
const MEETING_APPS: &[&str] = &["meet", "teams"];

#[derive(Debug, Deserialize)]
pub(super) struct Target {
    #[serde(default)]
    pub(super) id: String,
    #[serde(rename = "type")]
    pub(super) kind: String,
    pub(super) url: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    pub(super) ws: String,
}

/// Every DevTools target of the car browser (pages and out-of-process frames).
pub(super) async fn list_targets() -> Result<Vec<Target>> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(3)).no_proxy().build()?;
    Ok(client.get(format!("{DEVTOOLS}/json/list")).send().await?.json().await?)
}

/// The dashboard tab.
pub(super) async fn dashboard_target() -> Result<Target> {
    list_targets().await?.into_iter()
        .find(|t| t.kind == "page" && is_dashboard(&t.url) && !t.ws.is_empty())
        .ok_or_else(|| anyhow!("dashboard tab not found in the car browser"))
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url).ok()?.host_str().map(str::to_ascii_lowercase)
}

fn host_matches(host: &str, hosts: &[&str]) -> bool {
    hosts.iter().any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// Which app a tab or frame URL belongs to, if any.
fn app_for_url(url: &str) -> Option<&'static StreamingApp> {
    let host = host_of(url)?;
    APPS.iter().find(|a| host_matches(&host, a.hosts))
}

/// The dashboard is the tab served by rascar-app itself (loopback origin).
pub(super) fn is_dashboard(url: &str) -> bool {
    matches!(host_of(url).as_deref(), Some("localhost") | Some("127.0.0.1"))
}

/// Keyboard input from the dashboard's on-screen keyboard, typed into whatever
/// has focus in the car browser (including a field inside the Maps iframe).
#[derive(Debug)]
pub enum KeyInput {
    Text(String),
    Backspace,
    Enter,
}

/// A GPS fix older than this is not sent to the browser: a stale position
/// shown as "you are here" is worse than none.
const MAX_FIX_AGE_SECS: f64 = 10.0;

/// Below this speed the GPS course is noise, so no heading is claimed
/// (W3C: heading is unknown while the device is stationary).
const MIN_HEADING_SPEED_KMH: f64 = 3.0;

/// `Emulation.setGeolocationOverride` params for a fix. Speed (m/s) and
/// heading reach `coords.speed` / `coords.heading` in the frames (checked on
/// the car browser, Chromium 146): without them the map never turned with the car.
fn geolocation_override(fix: &super::gps::GpsData) -> serde_json::Value {
    let mut p = serde_json::json!({
        "latitude": fix.latitude,
        "longitude": fix.longitude,
        "accuracy": fix.accuracy.max(5.0),
        "speed": fix.speed_kmh.max(0.0) / 3.6,
    });
    if fix.speed_kmh >= MIN_HEADING_SPEED_KMH {
        p["heading"] = fix.bearing.rem_euclid(360.0).into();
    }
    p
}

pub struct AppsService {
    http: reqwest::Client,
    gps: Arc<GpsService>,
    input: OnceLock<mpsc::Sender<KeyInput>>,
}

impl AppsService {
    pub fn new(gps: Arc<GpsService>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .no_proxy()
            .build()
            .unwrap_or_default();
        Self { http, gps, input: OnceLock::new() }
    }

    /// Queue keyboard input for the dashboard session.
    pub async fn type_input(&self, key: KeyInput) -> Result<()> {
        let tx = self.input.get().ok_or_else(|| anyhow!("input channel not started"))?;
        tx.send(key).await.map_err(|_| anyhow!("dashboard session is down"))
    }

    async fn targets(&self) -> Result<Vec<Target>> {
        let list: Vec<Target> = self
            .http
            .get(format!("{DEVTOOLS}/json/list"))
            .send()
            .await
            .context("DevTools unreachable (is the car browser running?)")?
            .json()
            .await
            .context("DevTools answered an unexpected target list")?;
        Ok(list.into_iter().filter(|t| t.kind == "page").collect())
    }

    /// Every target, including the out-of-process iframes of embedded apps.
    async fn all_targets(&self) -> Result<Vec<Target>> {
        Ok(self.http.get(format!("{DEVTOOLS}/json/list")).send().await?.json().await?)
    }

    /// Validate a calendar event's meeting link for framing: https only, and a
    /// Meet or Teams host (the page never frames arbitrary URLs). Mic and
    /// camera are pre-granted to the DASHBOARD origin — frames inherit the
    /// top-level origin's permissions, and a prompt can't be answered in the car.
    pub async fn prepare_meeting(&self, link: &str) -> Result<(&'static StreamingApp, String)> {
        let url = url::Url::parse(link).context("not a URL")?;
        if url.scheme() != "https" {
            return Err(anyhow!("meeting links must be https"));
        }
        let app = app_for_url(url.as_str())
            .filter(|a| MEETING_APPS.contains(&a.id))
            .ok_or_else(|| anyhow!("not a Google Meet or Teams link"))?;
        if let Some(dash) = self.targets().await?.into_iter().find(|t| is_dashboard(&t.url)) {
            let origin = url::Url::parse(&dash.url)?.origin().ascii_serialization();
            if let Err(e) = self.grant(&origin, &["audioCapture", "videoCapture"]).await {
                warn!("apps: could not pre-grant mic/camera: {e:#}");
            }
        }
        info!("apps: joining {} meeting", app.name);
        Ok((app, url.to_string()))
    }

    /// While an app is shown in the small PiP window, make its playing video
    /// fill the frame (the site's own layout is unreadable at that size);
    /// `on = false` restores the site. The frame is a separate DevTools target.
    pub async fn set_pip(&self, id: &str, on: bool) -> Result<()> {
        let app = APPS.iter().find(|a| a.id == id).ok_or_else(|| anyhow!("unknown app {id:?}"))?;
        if app.hosts.is_empty() {
            // Local page (Android Auto): it already scales its video to the PiP window.
            return Ok(());
        }
        let frames: Vec<Target> = self.all_targets().await?.into_iter()
            .filter(|t| t.kind == "iframe" && !t.ws.is_empty() && app_for_url(&t.url).is_some_and(|a| a.id == app.id))
            .collect();
        if frames.is_empty() {
            return Err(anyhow!("{} frame not found", app.name));
        }
        let expr = format!("({PIP_JS})({on})");
        for f in frames {
            evaluate(&f.ws, &expr).await?;
        }
        Ok(())
    }

    /// Browser-wide permission grant for an origin (no prompt in the car).
    async fn grant(&self, origin: &str, permissions: &[&str]) -> Result<()> {
        let version: serde_json::Value = self.http.get(format!("{DEVTOOLS}/json/version")).send().await?.json().await?;
        let browser_ws = version["webSocketDebuggerUrl"].as_str().ok_or_else(|| anyhow!("no browser endpoint"))?;
        cdp_call(browser_ws, "Browser.grantPermissions", serde_json::json!({
            "origin": origin, "permissions": permissions,
        })).await?;
        Ok(())
    }

    /// Starts the dashboard session (GPS position + keyboard input).
    pub fn start(self: Arc<Self>) {
        let (tx, rx) = mpsc::channel(64);
        let _ = self.input.set(tx);
        tokio::spawn(self.clone().dashboard_session(rx));
        tokio::spawn(self.dashboard_watchdog());
    }

    /// Puts the dashboard back if a framed site replaced it with itself
    /// (Disney+ did at login): the car has no other way back. A separate loop
    /// because a navigation keeps the same DevTools target, so the dashboard
    /// session itself never notices it now drives another site.
    async fn dashboard_watchdog(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let Ok(targets) = self.targets().await else { continue };
            if targets.is_empty() || targets.iter().any(|t| is_dashboard(&t.url)) {
                continue;
            }
            // The YouTube Music tab is kept in the background on purpose (ytbridge.rs).
            if let Some(t) = targets.iter().find(|t| !t.ws.is_empty() && !super::ytbridge::is_bridge_tab(&t.id)) {
                warn!("apps: the dashboard was replaced by {} — restoring it", t.url);
                if let Err(e) = cdp_call(&t.ws, "Page.navigate", serde_json::json!({ "url": DASHBOARD_URL })).await {
                    warn!("apps: could not restore the dashboard: {e:#}");
                }
            }
        }
    }

    /// Persistent DevTools session on the dashboard tab. It must stay open:
    /// `Emulation.setGeolocationOverride` only lasts as long as the session.
    /// The car browser has no location provider of its own, so this is what
    /// gives the embedded Google Maps / Waze the car's GPS position.
    async fn dashboard_session(self: Arc<Self>, mut rx: mpsc::Receiver<KeyInput>) {
        loop {
            match self.targets().await {
                Ok(targets) => {
                    if let Some(dash) = targets.into_iter().find(|t| is_dashboard(&t.url) && !t.ws.is_empty()) {
                        if let Err(e) = self.grant_geolocation(&dash.url).await {
                            warn!("apps: geolocation grant failed: {e:#}");
                        }
                        if let Err(e) = self.run_dashboard_session(&dash.ws, &mut rx).await {
                            warn!("apps: dashboard session ended: {e:#}");
                        }
                    }
                }
                Err(_) => {}
            }
            // Drop keys typed while no session existed rather than replaying them later.
            while rx.try_recv().is_ok() {}
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Frames inherit permissions from the TOP-LEVEL origin (the dashboard),
    /// so granting it lets the Maps iframe read the position without a prompt
    /// nobody could answer in the car.
    async fn grant_geolocation(&self, dashboard_url: &str) -> Result<()> {
        let origin = url::Url::parse(dashboard_url)?.origin().ascii_serialization();
        self.grant(&origin, &["geolocation"]).await
    }

    async fn run_dashboard_session(&self, ws_url: &str, rx: &mut mpsc::Receiver<KeyInput>) -> Result<()> {
        let (mut ws, _) = tokio::time::timeout(Duration::from_secs(2), tokio_tungstenite::connect_async(ws_url))
            .await
            .context("DevTools connect timed out")??;
        info!("apps: dashboard session up");
        let mut next_id: u64 = 1;
        let mut send = |method: &str, params: serde_json::Value| {
            next_id += 1;
            Message::Text(serde_json::json!({"id": next_id, "method": method, "params": params}).to_string().into())
        };
        // The car's touches reach Chromium as XTest MOUSE drags, which never
        // scroll: Netflix/Disney+ picked up the poster link instead and the
        // dashboard's own lists could only scroll by their thin scrollbar. With
        // this, Chromium turns them into touches (drag = scroll, tap = click),
        // inside cross-origin frames too. Lasts as long as this session, so it
        // is re-applied on every reconnect (like the geolocation override).
        ws.send(send("Emulation.setEmitTouchEventsForMouse", serde_json::json!({ "enabled": true, "configuration": "mobile" }))).await?;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut had_fix = false;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let fix = self.gps.current.read().await.clone();
                    let usable = fix.has_fix && !fix.is_mock && fix.age_secs() < MAX_FIX_AGE_SECS;
                    if usable {
                        ws.send(send("Emulation.setGeolocationOverride", geolocation_override(&fix))).await?;
                    } else if had_fix {
                        // Lost the fix: stop claiming the last position.
                        ws.send(send("Emulation.clearGeolocationOverride", serde_json::json!({}))).await?;
                    }
                    had_fix = usable;
                }
                key = rx.recv() => {
                    let Some(key) = key else { return Ok(()) };
                    match key {
                        KeyInput::Text(text) => ws.send(send("Input.insertText", serde_json::json!({"text": text}))).await?,
                        KeyInput::Backspace => {
                            for kind in ["keyDown", "keyUp"] {
                                ws.send(send("Input.dispatchKeyEvent", serde_json::json!({
                                    "type": kind, "key": "Backspace", "code": "Backspace", "windowsVirtualKeyCode": 8,
                                }))).await?;
                            }
                        }
                        KeyInput::Enter => {
                            for kind in ["keyDown", "keyUp"] {
                                let mut ev = serde_json::json!({
                                    "type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13,
                                });
                                // A keyDown needs its text for the page to see a submit.
                                if kind == "keyDown" { ev["text"] = "\r".into(); }
                                ws.send(send("Input.dispatchKeyEvent", ev)).await?;
                            }
                        }
                    }
                }
                msg = ws.next() => {
                    match msg {
                        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return Err(anyhow!("dashboard tab closed or reloaded")),
                        _ => {} // replies and events are not needed
                    }
                }
            }
        }
    }

}

/// Runtime.evaluate over a short-lived DevTools session; returns the string
/// the expression evaluates to. A persistent session isn't needed because the
/// overlay script is idempotent and re-run on every poll.
pub(super) async fn evaluate(ws_url: &str, expression: &str) -> Result<String> {
    let reply = cdp_call(ws_url, "Runtime.evaluate", serde_json::json!({
        "expression": expression, "returnByValue": true,
    })).await?;
    Ok(reply
        .pointer("/result/result/value")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string())
}

/// One DevTools command over a short-lived session; returns the full reply.
pub(super) async fn cdp_call(ws_url: &str, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    cdp_call_timeout(ws_url, method, params, Duration::from_secs(2)).await
}

/// `cdp_call` for commands that may take longer to answer (an awaited fetch).
pub(super) async fn cdp_call_timeout(
    ws_url: &str,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value> {
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(2), tokio_tungstenite::connect_async(ws_url))
        .await
        .context("DevTools connect timed out")??;
    let req = serde_json::json!({ "id": 1, "method": method, "params": params });
    ws.send(Message::Text(req.to_string().into())).await?;
    let reply = tokio::time::timeout(timeout, async {
        while let Some(msg) = ws.next().await {
            if let Message::Text(txt) = msg? {
                let v: serde_json::Value = serde_json::from_str(&txt)?;
                if v.get("id").and_then(|i| i.as_i64()) == Some(1) {
                    return Ok::<_, anyhow::Error>(v);
                }
            }
        }
        Err(anyhow!("DevTools closed the session"))
    })
    .await
    .context("DevTools call timed out")??;
    let _ = ws.close(None).await;
    if let Some(err) = reply.get("error") {
        return Err(anyhow!("{method}: {err}"));
    }
    Ok(reply)
}

/// Takes `on`; pins the playing (else largest) video over the whole frame.
/// A <style> element is fine here: the frame-unlock extension removes these
/// frames' CSP, and textContent on <style> is not a Trusted Types sink.
const PIP_JS: &str = r#"(on) => {
  const old = document.getElementById('__rascar_pip');
  document.querySelectorAll('[data-rascar-pip]').forEach(v => v.removeAttribute('data-rascar-pip'));
  if (!on) { old?.remove(); return 'off'; }
  const vids = [...document.querySelectorAll('video')];
  const v = vids.find(x => !x.paused) || vids.sort((a, b) => b.clientWidth * b.clientHeight - a.clientWidth * a.clientHeight)[0];
  if (!v) return 'no video';
  v.setAttribute('data-rascar-pip', '');
  const s = old || document.createElement('style');
  s.id = '__rascar_pip';
  // Hide everything but the video (site headers are often layered above it);
  // `visibility` lets the video show through its hidden ancestors.
  s.textContent = 'html{overflow:hidden!important}body *{visibility:hidden!important}'
    + 'video[data-rascar-pip]{visibility:visible!important;position:fixed!important;inset:0!important;width:100vw!important;height:100vh!important;'
    + 'max-width:none!important;max-height:none!important;object-fit:contain!important;z-index:2147483646!important;background:#000!important}';
  if (!old) (document.head || document.documentElement).appendChild(s);
  return 'on';
}"#;

#[cfg(test)]
mod tests {

    #[test]
    fn geolocation_override_carries_speed_and_heading_only_when_moving() {
        let mut fix = crate::services::gps::GpsData::default();
        fix.latitude = 48.85;
        fix.longitude = 2.35;
        fix.accuracy = 2.0;
        fix.speed_kmh = 72.0;
        fix.bearing = -90.0;
        let p = geolocation_override(&fix);
        assert_eq!(p["speed"], 20.0);
        assert_eq!(p["heading"], 270.0);
        assert_eq!(p["accuracy"], 5.0);
        fix.speed_kmh = 1.0;
        let p = geolocation_override(&fix);
        assert!(p.get("heading").is_none(), "no heading when stationary");
    }
    use super::*;

    #[test]
    fn tabs_map_to_their_app_only() {
        assert_eq!(app_for_url("https://www.netflix.com/watch/81").map(|a| a.id), Some("netflix"));
        assert_eq!(app_for_url("https://m.youtube.com/").map(|a| a.id), Some("youtube"));
        assert_eq!(app_for_url("https://tv.free.fr/live").map(|a| a.id), Some("freetv"));
        // Lookalike hosts must not match.
        assert!(app_for_url("https://netflix.com.evil.example/").is_none());
        assert!(app_for_url("https://notnetflix.com/").is_none());
        assert!(app_for_url("http://localhost:8080/").is_none());
        assert_eq!(app_for_url("https://meet.google.com/abc-defg-hij").map(|a| a.id), Some("meet"));
        assert_eq!(app_for_url("https://teams.microsoft.com/l/meetup-join/19%3a").map(|a| a.id), Some("teams"));
        // www.google.com is not Meet.
        assert!(app_for_url("https://www.google.com/maps").is_none());
    }

    #[test]
    fn dashboard_is_the_loopback_tab() {
        assert!(is_dashboard("http://localhost:8080/"));
        assert!(is_dashboard("http://127.0.0.1:8080/#x"));
        assert!(!is_dashboard("https://www.youtube.com/"));
    }
}
