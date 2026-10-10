//! Phone calls through the SIM7600E-H: the call audio goes through the car's
//! CarPlay telephony stream.
//!
//! * Owns the modem's VOICE side only: AT port = USB interface 3 (cell.rs
//!   polls interface 2, so the two never share a port), PCM port = interface 4.
//!   QMI (`cdc-wdm0`) stays rascar-4g-qmi's.
//! * Feature flag `RASCAR_PHONE` (default OFF until the bench gate): when off
//!   no port is opened and `available` is false — except that an emergency
//!   number (112/911/999/100/101) is ALWAYS dialable: dialing one starts the
//!   service on demand.
//! * API: `GET /api/phone`, `POST /api/phone/{dial,answer,hangup,dtmf,mute}`;
//!   the same status goes out as WS key `"phone"`.
//! * Car wheel keys (`key hangup|hook|drop` from rascar-carplay) act on the
//!   phone call when one exists; otherwise main.rs forwards them to the page
//!   as before (meetings).

pub mod at;
pub mod audio;
pub mod calls;
pub mod car;
pub mod engine;
pub mod port;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{info, warn};

pub use calls::CallRecord;
pub use car::note_meeting;

/// The `"phone"` WS payload / `GET /api/phone`.
#[derive(Debug, Clone, Serialize)]
pub struct PhoneStatus {
    pub available: bool,
    /// "absent" | "ready" | "busy" | "error"
    pub modem: &'static str,
    /// "volte" | "cs" | "unknown"
    pub voice_domain: &'static str,
    pub call: Option<CallView>,
}

impl Default for PhoneStatus {
    fn default() -> Self {
        PhoneStatus { available: false, modem: "absent", voice_domain: "unknown", call: None }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallView {
    pub id: u32,
    /// "dialing" | "alerting" | "incoming" | "active" | "held" | "ended"
    pub state: &'static str,
    /// "in" | "out"
    pub direction: &'static str,
    pub number: String,
    pub name: Option<String>,
    /// When the call appeared (dial or first ring), RFC 3339 UTC.
    pub started_at: String,
    /// Seconds since it was answered (0 before).
    pub duration_s: u64,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub error: String,
}

impl ApiError {
    pub fn new(status: u16, error: &str) -> Self {
        ApiError { status, error: error.to_string() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Dial(String),
    Answer,
    Hangup,
    Dtmf(String),
    Mute(bool),
}

pub struct Envelope {
    pub req: Request,
    pub reply: oneshot::Sender<Result<(), ApiError>>,
}

/// State shared between the engine and the rest of the app.
#[derive(Default)]
pub struct Shared {
    pub status: RwLock<PhoneStatus>,
    /// The phone holds the car's telephony stream (see car.rs).
    pub owns_call: AtomicBool,
}

/// `RASCAR_PHONE=1|on|true|yes` enables the service.
pub fn flag_on(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Answer,
    Hangup,
}

/// What a car key does to the phone call in `state` (None = no live call →
/// not ours, the page gets the key). `hangup` is the old single key and
/// answers a ringing call like `hook`; `drop` always ends/declines.
pub fn key_action(key: &str, state: Option<&str>) -> Option<KeyAction> {
    let state = state.filter(|s| *s != "ended")?;
    match (key, state) {
        ("hook" | "hangup", "incoming") => Some(KeyAction::Answer),
        ("hook" | "hangup" | "drop", _) => Some(KeyAction::Hangup),
        _ => None,
    }
}

pub struct PhoneService {
    enabled: bool,
    started: AtomicBool,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Envelope>,
    rx: Mutex<Option<mpsc::Receiver<Envelope>>>,
    history_tx: mpsc::UnboundedSender<CallRecord>,
    history_rx: Mutex<Option<mpsc::UnboundedReceiver<CallRecord>>>,
    contacts_path: PathBuf,
}

impl PhoneService {
    pub fn new(contacts_path: PathBuf) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(8);
        let (history_tx, history_rx) = mpsc::unbounded_channel();
        let enabled = flag_on(std::env::var("RASCAR_PHONE").ok().as_deref());
        Arc::new(PhoneService {
            enabled,
            started: AtomicBool::new(false),
            shared: Arc::new(Shared::default()),
            tx,
            rx: Mutex::new(Some(rx)),
            history_tx,
            history_rx: Mutex::new(Some(history_rx)),
            contacts_path,
        })
    }

    pub fn start(&self) {
        if self.enabled {
            self.ensure_started();
        } else {
            info!("phone: disabled (RASCAR_PHONE not set); emergency numbers still start it on demand");
        }
    }

    fn ensure_started(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(rx) = self.rx.lock().ok().and_then(|mut g| g.take()) else { return };
        tokio::spawn(supervise(self.shared.clone(), rx, self.history_tx.clone(), self.contacts_path.clone()));
    }

    /// Ended calls, for the call history (taken once by main.rs).
    pub fn take_history(&self) -> Option<mpsc::UnboundedReceiver<CallRecord>> {
        self.history_rx.lock().ok().and_then(|mut g| g.take())
    }

    pub async fn status(&self) -> PhoneStatus {
        self.shared.status.read().await.clone()
    }

    /// The phone currently holds the car's telephony stream (a meeting's
    /// `call 0` must not be relayed then).
    pub fn owns_call(&self) -> bool {
        self.shared.owns_call.load(Ordering::Relaxed)
    }


    pub async fn request(&self, req: Request) -> Result<(), ApiError> {
        if let Request::Dial(n) = &req {
            if at::validate_number(n).is_ok_and(|n| at::is_emergency(&n)) {
                self.ensure_started();
            }
        }
        if !self.started.load(Ordering::SeqCst) {
            return Err(ApiError::new(503, "phone disabled (RASCAR_PHONE)"));
        }
        let (reply, rx) = oneshot::channel();
        match tokio::time::timeout(Duration::from_secs(2), self.tx.send(Envelope { req, reply })).await {
            Ok(Ok(())) => {}
            _ => return Err(ApiError::new(503, "phone service busy")),
        }
        // Longest path: an emergency hang-up (10 s) + ATD (30 s) + a stale drain.
        match tokio::time::timeout(Duration::from_secs(50), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(ApiError::new(503, "phone service restarted")),
            Err(_) => Err(ApiError::new(504, "phone service did not answer")),
        }
    }

    /// A car key: true when it acted on a phone call (the page must not also
    /// get it).
    pub async fn handle_key(self: &Arc<Self>, key: &str) -> bool {
        let state = self.shared.status.read().await.call.as_ref().map(|c| c.state);
        let Some(action) = key_action(key, state) else { return false };
        let req = match action {
            KeyAction::Answer => Request::Answer,
            KeyAction::Hangup => Request::Hangup,
        };
        info!("phone: car key '{key}' → {req:?}");
        // Do not hold the key listener while the modem answers.
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.request(req).await {
                warn!("phone: car key action failed: {}", e.error);
            }
        });
        true
    }
}

/// Find, open and run the modem; on any failure publish why and retry. While
/// no session runs, requests are answered 503 at once.
async fn supervise(shared: Arc<Shared>, mut rx: mpsc::Receiver<Envelope>, history: mpsc::UnboundedSender<CallRecord>, contacts: PathBuf) {
    use tokio_serial::SerialPortBuilderExt;
    let mut last_logged = "";
    loop {
        let modem: &'static str = match port::sim7600_ports() {
            None => "absent",
            // rascar-4g-radio / ModemManager mid-conversation: let them finish.
            Some(_) if crate::services::cell::at_port_busy() => "busy",
            Some((at_port, pcm_port)) => match tokio_serial::new(at_port.to_string_lossy(), 115_200).open_native_async() {
                Ok(stream) => {
                    info!("phone: voice AT port {} open (PCM {})", at_port.display(), pcm_port.display());
                    last_logged = "";
                    let engine = engine::Engine::new(
                        stream,
                        audio::PcmAudio::new(pcm_port),
                        car::UdpCar::default(),
                        shared.clone(),
                        history.clone(),
                        Some(contacts.clone()),
                        car::meeting_active,
                    );
                    let (exit, back) = engine.run(rx).await;
                    rx = back;
                    warn!("phone: modem session ended: {exit:?}");
                    match exit {
                        engine::Exit::Closed => return,
                        engine::Exit::Gone(_) => "absent",
                        engine::Exit::Error(_) => "error",
                    }
                }
                Err(e) => {
                    let busy = e.to_string().to_ascii_lowercase().contains("busy") || e.to_string().contains("temporarily unavailable");
                    warn!("phone: cannot open {}: {e}", at_port.display());
                    if busy { "busy" } else { "error" }
                }
            },
        };
        if modem != last_logged {
            info!("phone: modem {modem}");
            last_logged = modem;
        }
        {
            let status = PhoneStatus { modem, ..PhoneStatus::default() };
            *shared.status.write().await = status;
        }
        if !idle(&mut rx, Duration::from_secs(3)).await {
            return;
        }
    }
}

/// Wait, answering requests 503. False when the service was dropped.
async fn idle(rx: &mut mpsc::Receiver<Envelope>, d: Duration) -> bool {
    let sleep = tokio::time::sleep(d);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return true,
            env = rx.recv() => match env {
                None => return false,
                Some(env) => { let _ = env.reply.send(Err(ApiError::new(503, "modem not available"))); }
            },
        }
    }
}

// ── HTTP ─────────────────────────────────────────────────────────

pub fn router<S: Clone + Send + Sync + 'static>(phone: Arc<PhoneService>) -> Router<S> {
    Router::new()
        .route("/api/phone", get(api_status))
        .route("/api/phone/dial", post(api_dial))
        .route("/api/phone/answer", post(api_answer))
        .route("/api/phone/hangup", post(api_hangup))
        .route("/api/phone/dtmf", post(api_dtmf))
        .route("/api/phone/mute", post(api_mute))
        .with_state(phone)
}

fn respond(r: Result<(), ApiError>) -> Response {
    match r {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => {
            let code = StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_REQUEST);
            (code, Json(serde_json::json!({ "ok": false, "error": e.error }))).into_response()
        }
    }
}

fn bad_body(e: JsonRejection) -> Response {
    respond(Err(ApiError::new(400, &e.body_text())))
}

async fn api_status(State(p): State<Arc<PhoneService>>) -> Json<PhoneStatus> {
    Json(p.status().await)
}

#[derive(Deserialize)]
struct DialBody {
    number: String,
}

async fn api_dial(State(p): State<Arc<PhoneService>>, body: Result<Json<DialBody>, JsonRejection>) -> Response {
    match body {
        Ok(Json(b)) => respond(p.request(Request::Dial(b.number)).await),
        Err(e) => bad_body(e),
    }
}

async fn api_answer(State(p): State<Arc<PhoneService>>) -> Response {
    respond(p.request(Request::Answer).await)
}

async fn api_hangup(State(p): State<Arc<PhoneService>>) -> Response {
    respond(p.request(Request::Hangup).await)
}

#[derive(Deserialize)]
struct DtmfBody {
    digits: String,
}

async fn api_dtmf(State(p): State<Arc<PhoneService>>, body: Result<Json<DtmfBody>, JsonRejection>) -> Response {
    match body {
        Ok(Json(b)) => respond(p.request(Request::Dtmf(b.digits)).await),
        Err(e) => bad_body(e),
    }
}

#[derive(Deserialize)]
struct MuteBody {
    on: bool,
}

async fn api_mute(State(p): State<Arc<PhoneService>>, body: Result<Json<MuteBody>, JsonRejection>) -> Response {
    match body {
        Ok(Json(b)) => respond(p.request(Request::Mute(b.on)).await),
        Err(e) => bad_body(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_flag_values() {
        for v in ["1", "on", "ON", "true", "yes", " 1 "] {
            assert!(flag_on(Some(v)), "{v}");
        }
        for v in ["", "0", "off", "no", "false"] {
            assert!(!flag_on(Some(v)), "{v}");
        }
        assert!(!flag_on(None));
    }

    #[test]
    fn car_keys_by_call_state() {
        assert_eq!(key_action("hook", Some("incoming")), Some(KeyAction::Answer));
        assert_eq!(key_action("hangup", Some("incoming")), Some(KeyAction::Answer));
        assert_eq!(key_action("drop", Some("incoming")), Some(KeyAction::Hangup));
        for s in ["dialing", "alerting", "active", "held"] {
            for k in ["hook", "hangup", "drop"] {
                assert_eq!(key_action(k, Some(s)), Some(KeyAction::Hangup), "{k} in {s}");
            }
        }
        // No phone call (or only the "ended" banner): the meeting/page keeps the key.
        assert_eq!(key_action("hook", None), None);
        assert_eq!(key_action("drop", Some("ended")), None);
        assert_eq!(key_action("next", Some("active")), None);
    }

    #[tokio::test]
    async fn disabled_service_refuses_but_starts_for_an_emergency() {
        let p = PhoneService::new(PathBuf::from("/nonexistent/contacts.json"));
        if p.enabled {
            return; // the developer's environment enables it; nothing to check
        }
        p.start();
        assert_eq!(p.request(Request::Answer).await.unwrap_err().status, 503);
        assert!(!p.started.load(Ordering::SeqCst));
        assert_eq!(p.request(Request::Dial("07700900101".into())).await.unwrap_err().error, "phone disabled (RASCAR_PHONE)");
        // 112 starts the service (no modem here → 503 "modem not available",
        // i.e. it was attempted, not refused by the flag).
        let e = p.request(Request::Dial("112".into())).await.unwrap_err();
        assert!(p.started.load(Ordering::SeqCst));
        assert_ne!(e.error, "phone disabled (RASCAR_PHONE)");
    }
}
