//! `/api/home/*` — the dashboard's Home page, backed by the optional home server
//! (services/home_server.rs). Every route but `/status` answers 404 while no
//! home server is configured.
//!
//! These routes open the front door and move the garage, and the dashboard has
//! no authentication of its own, so the whole subtree answers only to loopback
//! peers: the kiosk Chromium on the Pi itself. Anyone else sharing a network
//! with the car (the house Wi-Fi, a hotspot) gets 403.

use std::net::SocketAddr;

use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use tracing::warn;

use crate::AppState;
use crate::services::home_server::{DEFAULT_TIMEOUT, GARAGE_TIMEOUT, Reply};

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/values", get(values))
        .route("/topology", get(|s| read(s, "/api/topology")))
        .route("/garage", get(|s| read(s, "/api/garage")))
        .route("/car", get(|s| read(s, "/api/car")))
        // The server's key/value store: weather, ventilation, … for the status strip.
        .route("/persistent", get(|s| read(s, "/api/persistent")))
        .route("/garage/{id}/state", get(garage_state))
        .route("/garage/{id}/light", post(garage_light))
        .route("/garage/{id}/{action}", post(garage_action))
        .route("/door/open", post(door_open))
        .route("/knx", post(knx))
        // Shows the arrival popup without driving home — for testing the UI.
        .route("/arrival", post(arrival))
        .layer(middleware::from_fn_with_state(state, loopback_only))
}

async fn loopback_only(
    State(s): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if !peer.ip().is_loopback() {
        warn!("home: refused {} {} from {peer}", req.method(), req.uri());
        return (StatusCode::FORBIDDEN, "the Home controls only answer the car's own screen").into_response();
    }
    // Feature off: only `/status` (which says so) answers, nothing is relayed.
    if !s.home_server.configured() && !req.uri().path().ends_with("/status") {
        return (StatusCode::NOT_FOUND, "no home server configured").into_response();
    }
    next.run(req).await
}

/// Pass the server's answer through: its status (so a 409 interlock reason or a
/// 403 reaches the page intact) and its body.
fn relay(result: anyhow::Result<Reply>) -> Response {
    match result {
        Ok(r) => {
            let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_GATEWAY);
            let ct = r.content_type.unwrap_or_else(|| "text/plain; charset=utf-8".into());
            (status, [(header::CONTENT_TYPE, ct)], r.body).into_response()
        }
        Err(e) => {
            warn!("home: {e:#}");
            (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
        }
    }
}

async fn status(State(s): State<AppState>) -> impl IntoResponse {
    Json(s.home_server.status().await)
}

async fn values(State(s): State<AppState>) -> Response {
    match s.home_server.values().await {
        Ok(m) => Json(m).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

async fn read(State(s): State<AppState>, path: &'static str) -> Response {
    relay(s.home_server.call(reqwest::Method::GET, path, None, DEFAULT_TIMEOUT).await)
}

/// Door ids are expected to be short slugs; anything that is not a plain slug is refused
/// before it is spliced into a path.
fn safe_id(id: &str) -> Result<&str, Response> {
    if !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        Ok(id)
    } else {
        Err((StatusCode::BAD_REQUEST, "invalid id").into_response())
    }
}

async fn garage_state(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    let id = match safe_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    // `fresh=true` asks the door itself; a close needs such a reading.
    let path = format!("/api/garage/{id}/state?fresh=true");
    relay(s.home_server.call(reqwest::Method::GET, &path, None, GARAGE_TIMEOUT).await)
}

async fn garage_action(State(s): State<AppState>, Path((id, action)): Path<(String, String)>) -> Response {
    let id = match safe_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    if !matches!(action.as_str(), "open" | "close" | "stop" | "ventilate") {
        return (StatusCode::BAD_REQUEST, "unknown garage action").into_response();
    }
    let path = format!("/api/garage/{id}/{action}");
    relay(s.home_server.call(reqwest::Method::POST, &path, None, GARAGE_TIMEOUT).await)
}

#[derive(Deserialize)]
struct LightBody {
    on: bool,
}

async fn garage_light(State(s): State<AppState>, Path(id): Path<String>, Json(b): Json<LightBody>) -> Response {
    let id = match safe_id(&id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let path = format!("/api/garage/{id}/light");
    relay(s.home_server.call(reqwest::Method::POST, &path, Some(serde_json::json!({ "on": b.on })), GARAGE_TIMEOUT).await)
}

async fn door_open(State(s): State<AppState>) -> Response {
    relay(s.home_server.open_front_door().await)
}

#[derive(Deserialize)]
struct KnxBody {
    address: String,
    value: String,
}

fn valid_group_address(a: &str) -> bool {
    let parts: Vec<&str> = a.split(['.', '/']).collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.len() <= 4 && p.bytes().all(|b| b.is_ascii_digit()))
}

async fn knx(State(s): State<AppState>, Json(b): Json<KnxBody>) -> Response {
    if !valid_group_address(&b.address) || b.value.len() > 16 {
        return (StatusCode::BAD_REQUEST, "invalid KNX write").into_response();
    }
    relay(s.home_server.knx(&b.address, &b.value).await)
}

async fn arrival(State(s): State<AppState>) -> impl IntoResponse {
    let _ = s.broadcast_tx.send(serde_json::json!({ "type": "arrival" }).to_string());
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_addresses_are_validated() {
        assert!(valid_group_address("1.2.3"));
        assert!(valid_group_address("1/3/20"));
        assert!(!valid_group_address("1.2"));
        assert!(!valid_group_address("1.2.x"));
        assert!(!valid_group_address("1.2.3.4"));
    }

    #[test]
    fn ids_cannot_escape_the_path() {
        assert!(safe_id("12345678901234567").is_ok());
        assert!(safe_id("door-1").is_ok());
        assert!(safe_id("../users").is_err());
        assert!(safe_id("a?b").is_err());
        assert!(safe_id("").is_err());
    }
}
