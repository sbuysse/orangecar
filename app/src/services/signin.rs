//! One-time sign-in for the embedded apps.
//!
//! The apps run as iframes of the dashboard, and some identity providers
//! (Google in particular) refuse to show their sign-in page inside a frame
//! (`accounts.google.com` answers 401 there). So signing in happens in a real
//! top-level tab of the car browser: its cookies are then used by the frames,
//! which the `rascar-embedded-apps.json` Chromium policy allows.
//!
//! That tab hides the dashboard (dock, keyboard), so it carries its own
//! overlay (`signin_overlay.js`): an on-screen keyboard and a "Done" button.
//! The overlay reports through a DevTools binding and this session types the
//! keys as trusted input, then closes the tab and brings the dashboard back.

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use super::apps::{cdp_call, dashboard_target, evaluate, list_targets, APPS};

const BINDING: &str = "__rascarSignin";
/// A forgotten sign-in tab closes itself after this.
const SIGNIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const OVERLAY_JS: &str = include_str!("signin_overlay.js");

/// Run in the dashboard: its keyboard rules (`.osk*`, `.nav-kbd-btn`,
/// `.hidden`) and the tokens they use, so the overlay looks like the dashboard
/// without redefining a single colour.
const DASHBOARD_KEYBOARD_CSS: &str = r#"(() => {
  const cs = getComputedStyle(document.documentElement);
  const vars = ['--bg-primary','--bg-secondary','--border','--border-strong','--text-primary','--text-secondary',
    '--accent','--accent-soft','--accent-glow','--radius-sm','--radius-md','--radius-lg','--font']
    .map(v => `${v}:${cs.getPropertyValue(v)}`).join(';');
  const rules = [];
  for (const sheet of document.styleSheets) {
    let list; try { list = sheet.cssRules; } catch (e) { continue; }
    for (const r of list) if (r.selectorText && /(^|[\s,])\.(osk|nav-kbd-btn|hidden)\b/.test(r.selectorText)) rules.push(r.cssText);
  }
  return `:host{${vars}}\n` + rules.join('\n');
})()"#;

/// The page to open for each app's sign-in (top-level, never framed).
pub fn signin_url(id: &str) -> Option<&'static str> {
    Some(match id {
        "youtube" => "https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fwww.youtube.com%2F",
        "meet" => "https://accounts.google.com/ServiceLogin?continue=https%3A%2F%2Fmeet.google.com%2F",
        "netflix" => "https://www.netflix.com/login",
        "disney" => "https://www.disneyplus.com/login",
        "freetv" => "https://tv.free.fr/",
        "teams" => "https://teams.microsoft.com/",
        "maps" => "https://accounts.google.com/ServiceLogin?continue=https%3A%2F%2Fwww.google.com%2Fmaps%3Fhl%3Dfr",
        "waze" => "https://www.waze.com/signin?redirect=%2Flive-map",
        // Ends on music.youtube.com so the session covers it (ytbridge.rs).
        "ytmusic" => "https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fmusic.youtube.com%2F",
        _ => return None,
    })
}

/// Sign-ins that are not streaming apps: the navigation page's maps (framed
/// the same way) and YouTube Music (used through the browser's own session,
/// see ytbridge.rs).
const NAV_MAPS: &[(&str, &str)] = &[("maps", "Google Maps"), ("waze", "Waze"), ("ytmusic", "YouTube Music")];

/// Display name of anything that can be signed in to, `None` if unknown.
fn signin_name(id: &str) -> Option<&'static str> {
    APPS.iter()
        .find(|a| a.id == id)
        .map(|a| a.name)
        .or_else(|| NAV_MAPS.iter().find(|(m, _)| *m == id).map(|(_, n)| *n))
}

/// What the overlay may ask for. Anything else is ignored.
#[derive(Debug, PartialEq)]
enum OverlayKey {
    Text(String),
    Backspace,
    Enter,
    Done,
}

fn parse_overlay(payload: &str) -> Option<OverlayKey> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    match v["k"].as_str()? {
        "text" => {
            let t = v["v"].as_str()?;
            (!t.is_empty() && t.chars().count() <= 8).then(|| OverlayKey::Text(t.to_string()))
        }
        "back" => Some(OverlayKey::Backspace),
        "enter" => Some(OverlayKey::Enter),
        "done" => Some(OverlayKey::Done),
        _ => None,
    }
}

pub(super) async fn browser_ws() -> Result<String> {
    let v: serde_json::Value = reqwest::get(format!("{}/json/version", super::apps::DEVTOOLS)).await?.json().await?;
    v["webSocketDebuggerUrl"].as_str().map(str::to_string).ok_or_else(|| anyhow!("no browser endpoint"))
}

/// The sign-in tab currently open, if any (ytbridge.rs must not adopt it:
/// the YouTube Music sign-in ends on music.youtube.com too).
static SIGNIN_TARGET: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub(super) fn is_signin_tab(target_id: &str) -> bool {
    SIGNIN_TARGET.lock().unwrap_or_else(|p| p.into_inner()).as_deref() == Some(target_id)
}

fn set_signin_tab(target_id: Option<String>) {
    *SIGNIN_TARGET.lock().unwrap_or_else(|p| p.into_inner()) = target_id;
}

/// Close every sign-in tab left open (anything that is not the dashboard or
/// the background YouTube Music tab).
async fn close_stray_tabs(browser: &str) {
    if let Ok(targets) = list_targets().await {
        let stray = |t: &&super::apps::Target| {
            t.kind == "page" && !super::apps::is_dashboard(&t.url) && !super::ytbridge::is_bridge_tab(&t.id)
        };
        for t in targets.iter().filter(stray) {
            let _ = cdp_call(browser, "Target.closeTarget", serde_json::json!({ "targetId": t.id })).await;
        }
    }
}

/// Open `id`'s sign-in page in its own tab with the keyboard overlay. Returns
/// once the tab is shown; the session keeps running until "Done" or timeout,
/// then `{"type":"signin-done","id":…}` goes to the dashboard over `events`
/// so it reloads the app. (A tab switch in the kiosk does not fire
/// `visibilitychange` on the dashboard, so it cannot notice by itself.)
/// `on_done` runs after the tab closed and before that event; the fields it
/// returns are merged into the event (YouTube Music reports its session).
pub async fn open<F, Fut>(id: &str, events: tokio::sync::broadcast::Sender<String>, on_done: F) -> Result<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = serde_json::Value> + Send,
{
    let name = signin_name(id).ok_or_else(|| anyhow!("unknown app {id:?}"))?;
    let url = signin_url(id).ok_or_else(|| anyhow!("{name} has no sign-in page"))?;
    // A Google session created with a clock hours off starts out looking
    // replayed (see ytbridge.rs): wait for NTP or the car's GPS.
    if url.starts_with("https://accounts.google.com/") && super::ytbridge::clock_source().is_none() {
        return Err(anyhow!("the system clock is not confirmed yet (no NTP, no live GPS): try again in a minute"));
    }
    let dash = dashboard_target().await?;
    let css = evaluate(&dash.ws, DASHBOARD_KEYBOARD_CSS).await.unwrap_or_default();
    let browser = browser_ws().await?;
    close_stray_tabs(&browser).await;

    let created = cdp_call(&browser, "Target.createTarget", serde_json::json!({ "url": "about:blank" })).await?;
    let target_id = created.pointer("/result/targetId").and_then(|v| v.as_str()).context("createTarget gave no id")?.to_string();
    set_signin_tab(Some(target_id.clone()));
    let mut page_ws = None;
    for _ in 0..20 {
        if let Some(t) = list_targets().await?.into_iter().find(|t| t.id == target_id && !t.ws.is_empty()) {
            page_ws = Some(t.ws);
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let page_ws = page_ws.context("the sign-in tab never appeared")?;
    let (ws, _) = tokio::time::timeout(Duration::from_secs(3), tokio_tungstenite::connect_async(&page_ws))
        .await
        .context("DevTools connect timed out")??;
    info!("signin: {name} sign-in tab opened");
    let source = format!("({OVERLAY_JS})({})", serde_json::to_string(&css)?);
    let app_id = NAV_MAPS.iter().map(|(m, _)| *m).chain(APPS.iter().map(|a| a.id)).find(|x| *x == id).unwrap_or_default();
    tokio::spawn(async move {
        if let Err(e) = run(ws, &source, url).await {
            warn!("signin: {name} session ended: {e:#}");
        }
        let _ = cdp_call(&browser, "Target.closeTarget", serde_json::json!({ "targetId": target_id })).await;
        set_signin_tab(None);
        // Back to the dashboard (it reloads the app to pick up the login).
        if let Ok(d) = dashboard_target().await {
            let _ = cdp_call(&d.ws, "Page.bringToFront", serde_json::json!({})).await;
        }
        info!("signin: {name} sign-in tab closed");
        let mut event = serde_json::json!({ "type": "signin-done", "id": app_id });
        if let (Some(ev), serde_json::Value::Object(extra)) = (event.as_object_mut(), on_done().await) {
            for (k, v) in extra {
                if k != "type" && k != "id" {
                    ev.insert(k, v);
                }
            }
        }
        let _ = events.send(event.to_string());
    });
    Ok(())
}

async fn run(
    mut ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    overlay: &str,
    url: &str,
) -> Result<()> {
    let mut next_id: u64 = 0;
    let mut msg = |method: &str, params: serde_json::Value| {
        next_id += 1;
        Message::Text(serde_json::json!({ "id": next_id, "method": method, "params": params }).to_string().into())
    };
    ws.send(msg("Runtime.enable", serde_json::json!({}))).await?;
    // Car touches arrive as mouse drags: make them touches here too (scroll).
    ws.send(msg("Emulation.setEmitTouchEventsForMouse", serde_json::json!({ "enabled": true, "configuration": "mobile" }))).await?;
    ws.send(msg("Page.enable", serde_json::json!({}))).await?;
    ws.send(msg("Runtime.addBinding", serde_json::json!({ "name": BINDING }))).await?;
    ws.send(msg("Page.addScriptToEvaluateOnNewDocument", serde_json::json!({ "source": overlay }))).await?;
    ws.send(msg("Page.navigate", serde_json::json!({ "url": url }))).await?;
    ws.send(msg("Page.bringToFront", serde_json::json!({}))).await?;
    let deadline = tokio::time::sleep(SIGNIN_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return Err(anyhow!("timed out")),
            m = ws.next() => {
                let text = match m {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_))) | None => return Err(anyhow!("tab closed")),
                    Some(Err(e)) => return Err(e.into()),
                    _ => continue,
                };
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                if v["method"] != "Runtime.bindingCalled" || v["params"]["name"] != BINDING {
                    continue;
                }
                match parse_overlay(v["params"]["payload"].as_str().unwrap_or_default()) {
                    Some(OverlayKey::Text(t)) => ws.send(msg("Input.insertText", serde_json::json!({ "text": t }))).await?,
                    Some(OverlayKey::Backspace) => {
                        for kind in ["keyDown", "keyUp"] {
                            ws.send(msg("Input.dispatchKeyEvent", serde_json::json!({
                                "type": kind, "key": "Backspace", "code": "Backspace", "windowsVirtualKeyCode": 8,
                            }))).await?;
                        }
                    }
                    Some(OverlayKey::Enter) => {
                        for kind in ["keyDown", "keyUp"] {
                            let mut ev = serde_json::json!({ "type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13 });
                            if kind == "keyDown" { ev["text"] = "\r".into(); }
                            ws.send(msg("Input.dispatchKeyEvent", ev)).await?;
                        }
                    }
                    Some(OverlayKey::Done) => return Ok(()),
                    None => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_app_has_a_top_level_signin_page() {
        // Local pages (no hosts, e.g. Android Auto) have nothing to sign in to.
        for a in APPS.iter().filter(|a| !a.hosts.is_empty()) {
            let u = signin_url(a.id).unwrap_or_else(|| panic!("{} has no sign-in page", a.id));
            assert!(u.starts_with("https://"), "{u}");
        }
        for (m, _) in NAV_MAPS {
            assert!(signin_url(m).is_some_and(|u| u.starts_with("https://")), "{m}");
        }
        assert!(signin_url("evil").is_none());
        assert_eq!(signin_name("maps"), Some("Google Maps"));
        assert_eq!(signin_name("evil"), None);
    }

    #[test]
    fn overlay_messages_are_validated() {
        assert_eq!(parse_overlay(r#"{"k":"text","v":"@"}"#), Some(OverlayKey::Text("@".into())));
        assert_eq!(parse_overlay(r#"{"k":"back"}"#), Some(OverlayKey::Backspace));
        assert_eq!(parse_overlay(r#"{"k":"enter"}"#), Some(OverlayKey::Enter));
        assert_eq!(parse_overlay(r#"{"k":"done"}"#), Some(OverlayKey::Done));
        assert_eq!(parse_overlay(r#"{"k":"text","v":""}"#), None);
        assert_eq!(parse_overlay(r#"{"k":"text","v":"far too long a burst"}"#), None);
        assert_eq!(parse_overlay(r#"{"k":"navigate","v":"x"}"#), None);
        assert_eq!(parse_overlay("not json"), None);
    }
}
