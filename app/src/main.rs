//! RasCar Dashboard — Axum web server.
//!
//! Serves the dashboard web UI and provides WebSocket + REST APIs for
//! real-time GPS, OBD-II, calendar, weather, and media data.

mod home_api;
mod services;

use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Query, State},
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use chrono::NaiveDate;
use serde::Deserialize;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::broadcast;
use tower_http::services::ServeDir;
use tracing::{info, warn};

use services::apps::AppsService;
use services::calendar::CalendarService;
use services::cell::CellService;
use services::gps::GpsService;
use services::home_server::HomeServerService;
use services::media::MediaService;
use services::music::MusicService;
use services::obd::ObdService;
use services::player::{PlayerService, Repeat, Track};
use services::lyrics::LyricsService;
use services::settings::SettingsService;
use services::signal::SignalService;
use services::trip::TripService;
use services::volume::VolumeService;
use services::weather::WeatherService;

#[derive(Clone)]
struct AppState {
    apps: Arc<AppsService>,
    gps: Arc<GpsService>,
    home_server: Arc<HomeServerService>,
    obd: Arc<ObdService>,
    calendar: Arc<CalendarService>,
    cell: Arc<CellService>,
    weather: Arc<WeatherService>,
    media: Arc<MediaService>,
    music: Arc<MusicService>,
    player: Arc<PlayerService>,
    resume: Arc<services::resume::ResumeService>,
    lyrics: Arc<LyricsService>,
    settings: Arc<SettingsService>,
    signal: Arc<SignalService>,
    trip: Arc<TripService>,
    fuel: Arc<services::fuel::FuelService>,
    volume: Arc<VolumeService>,
    /// Modem phone calls (services/phone, behind RASCAR_PHONE).
    phone: Arc<services::phone::PhoneService>,
    broadcast_tx: broadcast::Sender<String>,
}

/// Candidate locations for the dashboard's static files, in priority order.
///
/// `env!("CARGO_MANIFEST_DIR")` is resolved at COMPILE time, so a binary built
/// on one machine and run on another looks for the build machine's source tree
/// and answers 404 for every asset — which is what the car was mirroring. The
/// compile-time path stays last so `cargo run` from the workspace still works.
fn frontend_candidates(
    env_dir: Option<String>,
    cwd: Option<PathBuf>,
    exe: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = env_dir {
        candidates.push(PathBuf::from(dir));
    }
    if let Some(cwd) = cwd {
        candidates.push(cwd.join("frontend"));
    }
    if let Some(dir) = exe.as_deref().and_then(Path::parent) {
        // Deployed layout: <root>/target/<profile>/rascar-app next to <root>/app/frontend.
        candidates.push(dir.join("../../app/frontend"));
        candidates.push(dir.join("frontend"));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("frontend"));
    candidates
}

/// First candidate that actually holds an `index.html`.
fn pick_frontend_dir<F>(candidates: &[PathBuf], has_index: F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    candidates.iter().find(|dir| has_index(dir)).cloned()
}

fn resolve_frontend_dir() -> PathBuf {
    let candidates = frontend_candidates(
        std::env::var("RASCAR_FRONTEND_DIR").ok(),
        std::env::current_dir().ok(),
        std::env::current_exe().ok(),
    );
    match pick_frontend_dir(&candidates, |dir| dir.join("index.html").is_file()) {
        Some(dir) => {
            info!("serving the dashboard from {}", dir.display());
            dir
        }
        None => {
            let fallback = candidates
                .last()
                .cloned()
                .unwrap_or_else(|| PathBuf::from("frontend"));
            warn!(
                "no frontend/index.html found (tried: {}) — every static path will 404 and the car \
                 will mirror an error page; set RASCAR_FRONTEND_DIR",
                candidates
                    .iter()
                    .map(|c| c.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            fallback
        }
    }
}

#[derive(Parser)]
#[command(name = "rascar-app", about = "RasCar Dashboard Server")]
struct Cli {
    /// Listen port
    #[arg(long, default_value = "8080")]
    port: u16,

    /// Listen address.
    ///
    /// EXPOSURE: the default `0.0.0.0` publishes every route on all
    /// interfaces with NO authentication — including POST /api/settings and
    /// POST /api/obd/dtc/clear. That is intentional: the kiosk browser and the
    /// occupants' phones reach the dashboard over the car's LAN/hotspot. On an
    /// untrusted network, pass `--bind 127.0.0.1` (kiosk only) or a specific
    /// interface address.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Delete recorded trips older than this many days at startup (0 = keep all).
    #[arg(long, default_value = "365")]
    trip_retention_days: u32,

    /// OBD serial port
    #[arg(long, default_value = "/dev/rfcomm0")]
    obd_port: String,

    /// ICS calendar URIs (comma-separated)
    #[arg(long, default_value = "")]
    ics_uris: String,

    /// Offline map data served at /maps: the PMTiles archive, fonts/ and
    /// sprites/ of the dashboard's own navigation map (js/navmap.js).
    #[arg(long, default_value = "/var/lib/rascar/maps")]
    maps_dir: String,

    /// Weather fallback latitude, used until the GPS has a fix (none by
    /// default: the forecast then waits for the first fix)
    #[arg(long, requires = "lon")]
    lat: Option<f64>,

    /// Weather fallback longitude (see --lat)
    #[arg(long, requires = "lat")]
    lon: Option<f64>,
}

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_target(false)
        .init();

    let cli = Cli::parse();

    // Parse ICS URIs
    let mut ics_uris: Vec<String> = cli
        .ics_uris
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().to_string())
        .collect();

    // Create services
    let gps = Arc::new(GpsService::new());
    let obd = Arc::new(ObdService::new(&cli.obd_port, 38400));
    // Calendar sources = --ics-uris + settings.json `calendar_ics_uris` (ported from
    // feature/dashboard-feature-parity c619bf5, never merged: the Pi, configured
    // through settings.json, started the calendar with 0 sources).
    let settings = Arc::new(SettingsService::new());
    {
        let s = settings.settings.read().await;
        for uri in &s.calendar_ics_uris {
            if !uri.is_empty() && !ics_uris.contains(uri) {
                ics_uris.push(uri.clone());
            }
        }
    }
    let calendar = Arc::new(CalendarService::new(ics_uris));
    let weather = Arc::new(WeatherService::new(cli.lat.zip(cli.lon), gps.clone()));
    let media = Arc::new(MediaService::new());
    // MusicService reads the auth path and enable flag from settings on every
    // call, so `POST /api/settings` takes effect without a restart.
    let music = Arc::new(MusicService::new(settings.clone()));
    music.start_warmup();
    let player = PlayerService::new(music.clone());
    // Resume the last radio / TV / YouTube Music session once online.
    let resume = services::resume::ResumeService::new(player.clone(), music.clone());
    resume.start();
    let lyrics = Arc::new(LyricsService::new(music.clone()));
    // YouTube Music runs through a background tab of the car browser.
    services::ytbridge::spawn_keeper();
    let apps = Arc::new(AppsService::new(gps.clone()));
    apps.clone().start();
    let signal = Arc::new(SignalService::new());
    let cell = Arc::new(CellService::new());
    let trip = Arc::new(TripService::new(
        gps.clone(),
        obd.clone(),
        cli.trip_retention_days,
    )?);
    let volume = Arc::new(VolumeService::new());
    let home_server = Arc::new(HomeServerService::new(gps.clone()));

    // Start services
    gps.start().await;
    obd.start().await;
    calendar.start().await;
    weather.start().await;
    media.start().await;
    signal.start().await;
    cell.start().await;
    // 4G data usage + the 4G panel's controls (own router, merged below).
    let datausage = services::datausage::DataUsage::new(settings.clone());
    datausage.start();
    let fuel = Arc::new(services::fuel::FuelService::new(trip.db_path(), obd.clone(), settings.clone())?);
    fuel.start();
    trip.start(fuel.clone(), settings.clone()).await;
    // The car may cut our power seconds after it is parked: flush on every shift into P.
    services::park_sync::spawn(gps.clone(), obd.clone(), trip.db_path());
    volume.start().await;
    // Phone calls through the SIM7600 (off unless RASCAR_PHONE; 112 always works).
    let phone = services::phone::PhoneService::new(config_path("contacts.json"));
    phone.start();
    if let Some(mut ended) = phone.take_history() {
        // The backend owns the call log of modem calls (the page no longer posts them).
        tokio::spawn(async move {
            while let Some(rec) = ended.recv().await {
                let entry = serde_json::json!({
                    "number": rec.number, "name": rec.name, "at": rec.at,
                    "direction": rec.direction, "missed": rec.missed, "duration_s": rec.duration_s,
                });
                if let Err(e) = append_call_history(entry).await {
                    warn!("call history not saved: {e}");
                }
            }
        });
    }

    // Broadcast channel for WebSocket updates
    let (broadcast_tx, _) = broadcast::channel::<String>(64);
    spawn_touch_echo(broadcast_tx.clone());
    services::visualizer::spawn(broadcast_tx.clone());
    home_server.start(broadcast_tx.clone());

    let state = AppState {
        apps: apps.clone(),
        gps: gps.clone(),
        home_server: home_server.clone(),
        obd: obd.clone(),
        calendar: calendar.clone(),
        cell: cell.clone(),
        weather: weather.clone(),
        media: media.clone(),
        music: music.clone(),
        player: player.clone(),
        resume: resume.clone(),
        lyrics: lyrics.clone(),
        settings: settings.clone(),
        signal: signal.clone(),
        trip: trip.clone(),
        fuel: fuel.clone(),
        volume: volume.clone(),
        phone: phone.clone(),
        broadcast_tx: broadcast_tx.clone(),
    };

    spawn_car_keys(state.clone());

    // Start broadcast loop
    let state_for_broadcast = state.clone();
    tokio::spawn(async move {
        broadcast_loop(state_for_broadcast).await;
    });

    // Frontend path — resolved at runtime, see resolve_frontend_dir().
    let frontend_dir = resolve_frontend_dir();
    services::navigation::init(&cli.maps_dir);

    // Router
    let app = Router::new()
        .route("/api/gps", get(api_gps))
        .route("/api/gps/history", get(api_gps_history))
        .route("/api/headunit", get(api_headunit))
        .route("/api/obd", get(api_obd))
        .route("/api/calendar", get(api_calendar))
        .route("/api/calendar/next", get(api_calendar_next))
        .route("/api/calendar/sources", get(api_calendar_sources))
        .route("/api/calendar/range", get(api_calendar_range))
        .route("/api/weather", get(api_weather))
        .route("/api/media", get(api_media))
        .route("/api/media/art", get(api_media_art))
        .route("/api/crypto", get(api_crypto))
        .route("/api/debug/log", post(api_kiosk_log))
        .route("/api/crypto/history", get(api_crypto_history))
        .route("/api/settings", get(api_settings_get))
        .route("/api/settings", post(api_settings_post))
        .route("/api/obd/history/{metric}", get(api_obd_history))
        .route("/api/obd/dtc", get(api_obd_dtc))
        .route("/api/obd/dtc/clear", post(api_obd_dtc_clear))
        .route("/api/trips", get(api_trips))
        .route("/api/trips/calendar", get(api_trips_calendar))
        .route("/api/trips/{id}", get(api_trip_detail))
        .route("/api/fuel", get(api_fuel))
        .route("/api/fuel/refill", post(api_fuel_refill))
        .route("/api/fuel/refill/{id}/dismiss", post(api_fuel_dismiss))
        .route("/api/volume", get(api_volume_get))
        .route("/api/volume", post(api_volume_set))
        .route("/api/tv/channels", get(api_tv_channels))
        .route("/api/tv/hls", get(api_tv_hls))
        .route("/api/radio/stations", get(api_radio_stations))
        .route("/api/radio/stream", get(api_radio_stream))
        .route("/api/contacts", get(api_contacts))
        .route("/api/calls", get(api_calls).post(api_calls_post))
        .route("/api/apps", get(api_apps))
        .route("/api/apps/meeting", post(api_apps_meeting))
        .route("/api/apps/{id}/pip", post(api_apps_pip))
        .route("/api/apps/{id}/signin", post(api_apps_signin))
        .route("/api/input", post(api_input))
        .route("/api/call", post(api_call))
        .route("/api/carplay/native", get(api_carplay_native_get).post(api_carplay_native))
        .route("/api/cell", get(api_cell))
        .route("/api/wifi", get(api_wifi))
        .route("/api/wifi/scan", post(api_wifi_scan))
        .route("/api/wifi/connect", post(api_wifi_connect))
        .route("/api/wifi/retry", post(api_wifi_retry))
        .route("/api/wifi/forget", post(api_wifi_forget))
        .route("/api/bluetooth", get(api_bluetooth))
        .route("/api/bluetooth/scan", post(api_bluetooth_scan))
        .route("/api/bluetooth/obd", post(api_bluetooth_obd))
        .route("/api/media/like", post(api_media_like))
        .route("/api/media/dislike", post(api_media_dislike))
        .route("/api/media/lyrics", get(api_media_lyrics))
        // Registered after the static /api/media/* routes above; the router
        // prefers a literal segment over this catch-all.
        .route("/api/media/{action}", post(api_media_action))
        .route("/api/music/status", get(api_music_status))
        .route("/api/music/signout", post(api_music_signout))
        .route("/api/music/home", get(api_music_home))
        .route("/api/music/history", get(api_music_history))
        .route("/api/music/play", post(api_music_play))
        .route("/api/music/queue", get(api_music_queue_get).post(api_music_queue_set))
        .route("/api/music/radio", post(api_music_radio))
        .route("/api/music/enqueue", post(api_music_enqueue))
        .route("/api/music/shuffle", post(api_music_shuffle))
        .route("/api/music/repeat", post(api_music_repeat))
        .route("/api/music/seek", post(api_music_seek))
        .route("/api/music/jump", post(api_music_jump))
        .merge(resume.routes())
        .route("/api/music/liked", get(api_music_liked))
        .route("/api/music/playlists", get(api_music_playlists))
        .route("/api/music/playlist/{id}", get(api_music_playlist))
        .route("/api/music/artist/{id}", get(api_music_artist))
        .route("/api/music/search", get(api_music_search))
        .nest("/api/home", home_api::router(state.clone()))
        .route("/internal/ytm/fetch", post(internal_ytm_fetch))
        .route("/ws", get(ws_handler))
        // ServeDir answers Range requests: the map reads the multi-GB PMTiles
        // archive a few tiles at a time.
        .nest_service("/maps", ServeDir::new(&cli.maps_dir))
        .route("/api/geo/search", get(api_geo_search))
        .route("/api/route", post(api_route))
        .route("/api/nav/say", post(api_nav_say))
        .route("/api/traffic", post(api_traffic))
        .route("/api/nav/home", get(api_nav_home_get).post(api_nav_home_set))
        .route("/api/nav/home-eta", get(api_nav_home_eta))
        .merge(services::datausage::router(datausage.clone()))
        .merge(services::phone::router(phone.clone()))
        .fallback_service(ServeDir::new(&frontend_dir).append_index_html_on_directories(true))
        // Always revalidate: without it browsers kept a stale app.js/index.html after
        // each deploy (a fixed feature "didn't work" until a hard reload). ServeDir
        // answers conditional requests with 304, so this costs one round trip.
        .layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache"),
        ))
        .with_state(state);

    let ip: std::net::IpAddr = cli
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --bind address {:?}: {e}", cli.bind))?;
    let addr = SocketAddr::new(ip, cli.port);
    // The helper reaches the browser bridge over loopback, which a listener
    // bound to one LAN address does not serve.
    music.set_bridge_url(if ip.is_unspecified() || ip.is_loopback() {
        Some(format!("http://127.0.0.1:{}/internal/ytm/fetch", cli.port))
    } else {
        warn!("--bind {ip}: YouTube Music sign-in is unavailable (the browser bridge needs loopback)");
        None
    });
    if ip.is_unspecified() {
        info!("dashboard is reachable from the whole network and has no authentication");
    }
    info!("dashboard listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    // Peer addresses let /api/home refuse anything but the kiosk (loopback).
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;

    Ok(())
}

// --- REST Handlers ---

async fn api_gps(State(s): State<AppState>) -> Json<serde_json::Value> {
    let g = s.gps.current.read().await;
    Json(serde_json::json!({
        "lat": g.latitude, "lon": g.longitude,
        "speed": g.speed_kmh, "alt": g.altitude,
        "bearing": g.bearing, "fix": g.has_fix,
        // Fabricated positions must be labelled as such — the UI has to be able
        // to tell a mock fix from a real one.
        "mock": g.is_mock,
        "sats_view": g.sats_in_view, "sats_used": g.sats_used, "sats": g.satellites,
    }))
}

/// The car's head unit as rascar-carplay recorded it (`headunit.json`), or
/// `{"known": false}` before the first CarPlay session.
async fn api_headunit() -> Json<serde_json::Value> {
    Json(match services::headunit::read().await {
        Some(mut v) => {
            v["known"] = serde_json::Value::Bool(true);
            v
        }
        None => serde_json::json!({ "known": false }),
    })
}

async fn api_gps_history(State(s): State<AppState>) -> Json<serde_json::Value> {
    let speed = s.gps.speed_history.read().await;
    let alt = s.gps.altitude_history.read().await;
    Json(serde_json::json!({
        "speed": *speed,
        "altitude": *alt,
    }))
}

async fn api_obd(State(s): State<AppState>) -> Json<serde_json::Value> {
    let o = s.obd.data.read().await;
    Json(serde_json::to_value(&*o).unwrap_or_default())
}

#[derive(Deserialize)]
struct CalendarQuery {
    date_str: Option<String>,
}

async fn api_calendar(
    State(s): State<AppState>,
    Query(q): Query<CalendarQuery>,
) -> Json<serde_json::Value> {
    let date = q
        .date_str
        .and_then(|d| chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok())
        .unwrap_or_else(|| chrono::Utc::now().date_naive());
    let events = s.calendar.get_events_for_date(date).await;
    Json(serde_json::to_value(&events).unwrap_or_default())
}

async fn api_calendar_sources(State(s): State<AppState>) -> Json<Vec<services::calendar::CalendarSource>> {
    Json(s.calendar.sources().await)
}

#[derive(Deserialize)]
struct RangeQuery {
    from: Option<String>,
    to: Option<String>,
}

/// Longest span `/api/calendar/range` serves (a month view is ≤ 42 days).
const CALENDAR_RANGE_MAX_DAYS: i64 = 62;

/// `GET /api/calendar/range?from=YYYY-MM-DD&to=YYYY-MM-DD` (inclusive, local
/// days) → `{"YYYY-MM-DD": [event, …]}`; multi-day events appear on each day.
/// Feeds the Week/Month views and the Calendar date picker's counts.
async fn api_calendar_range(State(s): State<AppState>, Query(q): Query<RangeQuery>) -> impl IntoResponse {
    use axum::http::StatusCode;
    let parse = |d: &Option<String>| d.as_deref().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    let (Some(from), Some(to)) = (parse(&q.from), parse(&q.to)) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "from/to must be YYYY-MM-DD"}))).into_response();
    };
    if to < from || (to - from).num_days() >= CALENDAR_RANGE_MAX_DAYS {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "range must be 1-62 days"}))).into_response();
    }
    let days = s.calendar.get_events_by_day(from, to).await;
    let map: serde_json::Map<String, serde_json::Value> = days
        .into_iter()
        .map(|(d, v)| (d.format("%Y-%m-%d").to_string(), serde_json::to_value(v).unwrap_or_default()))
        .collect();
    Json(serde_json::Value::Object(map)).into_response()
}

async fn api_calendar_next(State(s): State<AppState>) -> Json<serde_json::Value> {
    let event = s.calendar.get_next_event().await;
    Json(serde_json::to_value(&event).unwrap_or(serde_json::Value::Null))
}

async fn api_weather(State(s): State<AppState>) -> Json<serde_json::Value> {
    let current = s.weather.current.read().await;
    let hourly = s.weather.hourly.read().await;
    let daily = s.weather.daily.read().await;
    Json(serde_json::json!({
        "current": *current,
        "hourly": *hourly,
        "daily": *daily,
    }))
}

async fn api_media(State(s): State<AppState>) -> Json<serde_json::Value> {
    let np = s.media.now_playing.read().await;
    Json(serde_json::to_value(&*np).unwrap_or_default())
}

async fn api_media_action(
    State(s): State<AppState>,
    axum::extract::Path(action): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    // `ok` reflects whether playerctl actually carried the action out. Before,
    // this returned true unconditionally — including when playerctl was absent
    // and every transport button was a silent no-op.
    let ok = match action.as_str() {
        "shuffle" if is_our_player(&s).await => s.player.toggle_shuffle().await.is_ok(),
        "shuffle" => s.media.toggle_shuffle().await,
        other => match mpris_action(&s.media, other).await {
            Some(ok) => ok,
            None => return Json(serde_json::json!({"ok": false, "error": "unknown action"})),
        },
    };
    if ok {
        Json(serde_json::json!({"ok": true}))
    } else {
        Json(serde_json::json!({
            "ok": false,
            "error": "player did not accept the command (is playerctl installed and a player running?)"
        }))
    }
}

/// Serve the currently-playing track's artwork.
///
/// MPRIS reports `mpris:artUrl` as a `file://` URI, which a browser cannot load
/// from an http origin — so the file is read and served here. Only a regular,
/// readable image file is served: never a directory, symlink target of another
/// type, device node, or arbitrary text file.
async fn api_media_art(State(s): State<AppState>) -> impl IntoResponse {
    use axum::body::Body;
    use axum::http::StatusCode;

    /// Refuse to buffer anything larger than this; cover art is never close.
    const MAX_ART_BYTES: u64 = 16 * 1024 * 1024;

    let not_found = || {
        (
            StatusCode::NOT_FOUND,
            [("content-type", "text/plain")],
            Body::from("no artwork"),
        )
            .into_response()
    };

    let art_url = s.media.now_playing.read().await.art_url.clone();
    if art_url.is_empty() {
        return not_found();
    }

    // Only local files: a remote artUrl is already loadable by the browser and
    // proxying it here would be an unvetted outbound fetch.
    let Ok(parsed) = url::Url::parse(&art_url) else {
        return not_found();
    };
    if parsed.scheme() != "file" {
        return not_found();
    }
    let Ok(path) = parsed.to_file_path() else {
        return not_found();
    };

    // Resolve symlinks and confirm it is a regular file before opening it.
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return not_found();
    };
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_ART_BYTES {
        return not_found();
    }

    let Ok(bytes) = tokio::fs::read(&path).await else {
        return not_found();
    };
    let Some(content_type) = sniff_image_type(&bytes) else {
        // Not an image — don't hand an arbitrary local file to the browser.
        tracing::warn!("media art {:?} is not a recognised image", path);
        return not_found();
    };

    (
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            // Art changes with the track; the URL doesn't.
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from(bytes),
    )
        .into_response()
}

/// Identify an image by its magic bytes. Returns None for anything else, which
/// is how non-image files are refused.
fn sniff_image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

#[derive(Deserialize)]
struct TripsQuery {
    date: Option<String>,
}

async fn api_trips(
    State(s): State<AppState>,
    Query(q): Query<TripsQuery>,
) -> Json<serde_json::Value> {
    let date = q
        .date
        .and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok())
        .unwrap_or_else(|| chrono::Utc::now().date_naive());
    let trips = s.trip.get_trips_for_date(date).await;
    Json(serde_json::to_value(&trips).unwrap_or_default())
}

#[derive(Deserialize)]
struct MonthQuery {
    /// `YYYY-MM`; the current local month when absent or malformed.
    month: Option<String>,
}

/// Parse `YYYY-MM` into (year, month 1-12).
fn parse_month(m: Option<&str>) -> (i32, u32) {
    use chrono::Datelike;
    m.and_then(|m| NaiveDate::parse_from_str(&format!("{m}-01"), "%Y-%m-%d").ok())
        .map(|d| (d.year(), d.month()))
        .unwrap_or_else(|| {
            let t = chrono::Local::now().date_naive();
            (t.year(), t.month())
        })
}

/// `GET /api/trips/calendar?month=YYYY-MM` → `{"month":"YYYY-MM","days":{"YYYY-MM-DD":n}}`
/// (trip counts per local day, for the Trips date picker).
async fn api_trips_calendar(State(s): State<AppState>, Query(q): Query<MonthQuery>) -> Json<serde_json::Value> {
    let (year, month) = parse_month(q.month.as_deref());
    let counts = s.trip.get_trip_counts_for_month(year, month).await;
    let days: serde_json::Map<String, serde_json::Value> = counts
        .into_iter()
        .map(|(d, n)| (d.format("%Y-%m-%d").to_string(), serde_json::json!(n)))
        .collect();
    Json(serde_json::json!({ "month": format!("{year:04}-{month:02}"), "days": days }))
}

async fn api_trip_detail(
    State(s): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Json<serde_json::Value> {
    match s.trip.get_trip_with_points(id).await {
        Some(trip) => Json(serde_json::to_value(&trip).unwrap_or_default()),
        None => Json(serde_json::json!({"error": "trip not found"})),
    }
}

async fn api_volume_get(State(s): State<AppState>) -> Json<serde_json::Value> {
    let pct = *s.volume.volume_pct.read().await;
    Json(serde_json::json!({ "pct": pct }))
}

#[derive(Deserialize)]
struct VolumeBody {
    pct: u8,
}

async fn api_volume_set(
    State(s): State<AppState>,
    Json(body): Json<VolumeBody>,
) -> Json<serde_json::Value> {
    // Report the real mixer outcome: on a box whose control isn't `Master` the
    // amixer call fails, and answering `{"ok": true}` would leave the slider
    // showing a level the hardware never took.
    match s.volume.set_volume(body.pct).await {
        Ok(pct) => Json(serde_json::json!({"ok": true, "pct": pct})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e})),
    }
}

/// Load a `~/.config/rascar/<name>` JSON file.
///
/// Returns `None` when the file is absent or unparseable — callers that use a
/// config as a security allowlist MUST distinguish that from an empty list and
/// fail closed. `read_config_json` is the lenient wrapper for display-only
/// endpoints.
fn config_path(name: &str) -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/root"))
        .join(".config/rascar")
        .join(name)
}

async fn try_read_config_json(name: &str) -> Option<serde_json::Value> {
    let path = config_path(name);
    let data = match tokio::fs::read_to_string(&path).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("config {name} unavailable: {e}");
            return None;
        }
    };
    match serde_json::from_str(&data) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("failed to parse config {name}: {e}");
            None
        }
    }
}

async fn read_config_json(name: &str) -> serde_json::Value {
    try_read_config_json(name)
        .await
        .unwrap_or_else(|| serde_json::json!([]))
}

async fn api_tv_channels() -> Json<serde_json::Value> {
    Json(read_config_json("tv-channels.json").await)
}

/// Offline destination search (services::navigation).
async fn api_geo_search(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let q = params.get("q").cloned().unwrap_or_default();
    let near = match (params.get("lat").and_then(|v| v.parse().ok()), params.get("lon").and_then(|v| v.parse().ok())) {
        (Some(lat), Some(lon)) => Some((lat, lon)),
        _ => None,
    };
    match tokio::task::spawn_blocking(move || services::navigation::search(&q, near, 8)).await {
        Ok(Ok(places)) => Json(serde_json::json!({ "places": places })).into_response(),
        Ok(Err(e)) => (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": format!("{e:#}") }))).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// Offline car route (services::navigation → local Valhalla).
async fn api_route(Json(req): Json<services::navigation::RouteRequest>) -> impl IntoResponse {
    match services::navigation::route(&req).await {
        Ok(r) => Json(serde_json::to_value(r).unwrap_or_default()).into_response(),
        Err(e) => (axum::http::StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

/// Live traffic for the offline route (services::navigation::traffic, Google).
async fn api_traffic(Json(req): Json<services::navigation::RouteRequest>) -> impl IntoResponse {
    match services::navigation::traffic(&req).await {
        Ok(t) => Json(serde_json::to_value(t).unwrap_or_default()).into_response(),
        Err(e) => (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

/// Home for the nav page: settings.json `home`, else the home server's home area (if configured).
async fn nav_home(s: &AppState) -> Option<(services::settings::HomePlace, &'static str)> {
    if let Some(h) = s.settings.settings.read().await.home.clone() {
        return Some((h, "settings"));
    }
    s.home_server.home().map(|(lat, lon)| (services::settings::HomePlace { lat, lon, label: "Home".into() }, "home_server"))
}

async fn api_nav_home_get(State(s): State<AppState>) -> Json<serde_json::Value> {
    match nav_home(&s).await {
        Some((h, source)) => Json(serde_json::json!({ "home": h, "source": source })),
        None => Json(serde_json::json!({ "home": null })),
    }
}

/// Set the nav home (`{lat, lon, label}`), or `{}` / `{"clear":true}` to go
/// back to the home server's (or none).
async fn api_nav_home_set(State(s): State<AppState>, Json(b): Json<serde_json::Value>) -> impl IntoResponse {
    let home = match (b["lat"].as_f64(), b["lon"].as_f64()) {
        (Some(lat), Some(lon)) if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) => {
            let label: String = b["label"].as_str().unwrap_or("").trim().chars().take(80).collect();
            Some(services::settings::HomePlace { lat, lon, label: if label.is_empty() { "Home".into() } else { label } })
        }
        _ if b["clear"].as_bool() == Some(true) || b.as_object().is_some_and(|o| o.is_empty()) => None,
        _ => return (axum::http::StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "lat/lon expected" }))).into_response(),
    };
    let mut cfg = s.settings.settings.read().await.clone();
    cfg.home = home;
    match s.settings.save(cfg).await {
        Ok(()) => api_nav_home_get(State(s)).await.into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

/// Time to home from `?lat&lon` (services::navigation::home_eta: Google
/// traffic at most every 5 min, local Valhalla in between).
async fn api_nav_home_eta(
    State(s): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let (Some(lat), Some(lon)) = (q.get("lat").and_then(|v| v.parse::<f64>().ok()), q.get("lon").and_then(|v| v.parse::<f64>().ok())) else {
        return (axum::http::StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "lat/lon expected" }))).into_response();
    };
    let Some((home, _)) = nav_home(&s).await else {
        return (axum::http::StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "no home set" }))).into_response();
    };
    match services::navigation::home_eta([lon, lat], [home.lon, home.lat]).await {
        Ok(e) => {
            let mut v = serde_json::to_value(e).unwrap_or_default();
            v["home"] = serde_json::json!(home);
            Json(v).into_response()
        }
        Err(e) => (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

/// Voice guidance: speak one phrase (services::navigation::say). Answers at
/// once; the phrase plays in the background, after any phrase still playing.
async fn api_nav_say(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let text = body["text"].as_str().unwrap_or("").to_string();
    tokio::spawn(async move {
        if let Err(e) = services::navigation::say(&text).await {
            tracing::warn!("navigation: voice: {e:#}");
        }
    });
    axum::http::StatusCode::ACCEPTED
}

async fn api_radio_stations() -> Json<serde_json::Value> {
    Json(read_config_json("radio-stations.json").await)
}

/// True for IPv4 carrier-grade NAT / shared address space, 100.64.0.0/10
/// (second octet 64–127). `Ipv4Addr::is_shared` covers this but is unstable,
/// so match the range explicitly.
fn is_cgnat_v4(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// Shared SSRF guard for the stream proxies: http/https only, and the host must
/// resolve exclusively to public addresses (no loopback / private / link-local /
/// CGNAT / ULA). Returns the host name for the caller's allowlist check.
async fn ensure_public_http_url(parsed: &url::Url) -> Result<String, &'static str> {
    use std::net::IpAddr;

    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err("unsupported scheme"),
    }
    let host = parsed.host_str().ok_or("missing host")?.to_string();

    // Resolve every address the host maps to and reject internal targets.
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| "host resolution failed")?;
    // NOTE: this is a resolve-then-check guard and is therefore subject to a
    // DNS-rebinding TOCTOU — the name could resolve to a public address here
    // and to an internal one when reqwest re-resolves for the actual request.
    // Fully closing that requires pinning the vetted IP into the connection;
    // acknowledged and out of scope for this validator.
    let mut any = false;
    for sa in addrs {
        any = true;
        let blocked = match sa.ip() {
            IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    // Carrier-grade NAT / shared address space (100.64.0.0/10).
                    || is_cgnat_v4(v4)
            }
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    // Unique-local addresses (fc00::/7, incl. fd00::/8).
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    // Link-local unicast (fe80::/10).
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
                    // map IPv4-mapped addresses through the v4 checks
                    || v6
                        .to_ipv4()
                        .map(|m| {
                            m.is_private() || m.is_link_local() || m.is_loopback() || is_cgnat_v4(m)
                        })
                        .unwrap_or(false)
            }
        };
        if blocked {
            return Err("host resolves to a non-routable address");
        }
    }
    if !any {
        return Err("host resolution failed");
    }
    Ok(host)
}

/// Validate a user-supplied radio stream URL to prevent SSRF.
///
/// Requires http/https, rejects hosts that resolve to loopback / private /
/// link-local / unspecified addresses, and (when the configured station list
/// is non-empty) requires the host to appear among the configured stations.
async fn validate_stream_url(raw: &str) -> Result<reqwest::Url, &'static str> {
    let parsed = url::Url::parse(raw).map_err(|_| "invalid url")?;
    let host = ensure_public_http_url(&parsed).await?;

    // The host MUST appear in the configured station list. This is an
    // allowlist, so it fails closed: a missing or unparseable
    // radio-stations.json disables the proxy entirely rather than turning it
    // into an open HTTP relay running from the car's IP address.
    let Some(stations) = try_read_config_json("radio-stations.json").await else {
        return Err("station list unavailable");
    };
    let Some(arr) = stations.as_array() else {
        return Err("station list is malformed");
    };
    let allowed = arr.iter().any(|st| {
        st.get("url")
            .and_then(|u| u.as_str())
            .and_then(|u| url::Url::parse(u).ok())
            .and_then(|u| u.host_str().map(|h| h.eq_ignore_ascii_case(&host)))
            .unwrap_or(false)
    });
    if !allowed {
        return Err("host not in configured stations");
    }

    Ok(parsed)
}

async fn api_radio_stream(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    use axum::body::Body;
    let Some(url) = params.get("url") else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            [("content-type", "text/plain")],
            Body::from("missing url param"),
        ).into_response();
    };
    let valid_url = match validate_stream_url(url).await {
        Ok(u) => u,
        Err(reason) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                [("content-type", "text/plain")],
                Body::from(format!("rejected url: {reason}")),
            ).into_response();
        }
    };
    // CRITICAL: no redirects. `validate_stream_url` vetted exactly one URL;
    // reqwest's default `Policy::limited(10)` would follow a 302 from that
    // vetted host to any unvetted target (loopback, LAN, cloud metadata),
    // defeating the whole SSRF guard.
    // A live stream never ends, so no TOTAL timeout: `.timeout(30 s)` covers
    // the body too and cut every station after exactly 30 s (measured
    // 2026-09-30: curl rc=18 at 30.2 s) — the radio "played nothing" in the car.
    // Bound the connection and each silence between chunks instead.
    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_GATEWAY,
                [("content-type", "text/plain")],
                Body::from(format!("client error: {e}")),
            ).into_response();
        }
    };
    // Redirects are followed by hand, each hop re-vetted by the same public-address
    // guard. Many stations redirect to their CDN: passing the empty 302 on as
    // "200 audio/mpeg" left the player with 0 bytes and the station showed
    // "offline". A redirect target is
    // trusted because the vetted station host pointed at it.
    let mut current = valid_url;
    for hop in 0..4 {
        let resp = match client.get(current.clone()).send().await {
            Ok(r) => r,
            Err(e) => {
                return (
                    axum::http::StatusCode::BAD_GATEWAY,
                    [("content-type", "text/plain")],
                    Body::from(format!("upstream error: {e}")),
                ).into_response();
            }
        };
        if resp.status().is_redirection() {
            let next = resp.headers().get("location").and_then(|v| v.to_str().ok())
                .and_then(|loc| current.join(loc).ok());
            let Some(next) = next.filter(|_| hop < 3) else {
                return (
                    axum::http::StatusCode::BAD_GATEWAY,
                    [("content-type", "text/plain")],
                    Body::from("too many or invalid redirects"),
                ).into_response();
            };
            if let Err(reason) = ensure_public_http_url(&next).await {
                return (
                    axum::http::StatusCode::BAD_GATEWAY,
                    [("content-type", "text/plain")],
                    Body::from(format!("rejected redirect: {reason}")),
                ).into_response();
            }
            current = next;
            continue;
        }
        if !resp.status().is_success() {
            return (
                axum::http::StatusCode::BAD_GATEWAY,
                [("content-type", "text/plain")],
                Body::from(format!("upstream HTTP {}", resp.status())),
            ).into_response();
        }
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("audio/mpeg")
            .to_string();
        return (
            [(axum::http::header::CONTENT_TYPE, ct)],
            Body::from_stream(resp.bytes_stream()),
        ).into_response();
    }
    (
        axum::http::StatusCode::BAD_GATEWAY,
        [("content-type", "text/plain")],
        Body::from("too many redirects"),
    ).into_response()
}

// ── TV: HLS proxy ────────────────────────────────────────────────────────────
//
// The TV CDNs send no CORS headers, so hls.js cannot fetch them from the page
// (every channel failed with "blocked by CORS policy", 2026-09-25). The proxy
// fetches server-side, rewrites every URI in a playlist back through itself and
// streams everything else. Allowlist, failing closed: a host is allowed when it
// appears in tv-channels.json, or when a playlist or redirect served by an
// already-allowed host pointed at it (CDNs hand out edge hosts that way).

static TV_EXTRA_HOSTS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));
/// Cap on hosts learned from playlists, so a hostile playlist cannot grow it forever.
const TV_EXTRA_HOSTS_MAX: usize = 256;

fn tv_learn_host(u: &url::Url) {
    if let Some(h) = u.host_str() {
        if let Ok(mut set) = TV_EXTRA_HOSTS.lock() {
            if set.len() < TV_EXTRA_HOSTS_MAX {
                set.insert(h.to_ascii_lowercase());
            }
        }
    }
}

async fn tv_host_allowed(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if TV_EXTRA_HOSTS.lock().map(|s| s.contains(&host)).unwrap_or(false) {
        return true;
    }
    let Some(channels) = try_read_config_json("tv-channels.json").await else { return false };
    channels.as_array().map(|arr| {
        arr.iter().any(|c| {
            c.get("url")
                .and_then(|u| u.as_str())
                .and_then(|u| url::Url::parse(u).ok())
                .and_then(|u| u.host_str().map(|h| h.eq_ignore_ascii_case(&host)))
                .unwrap_or(false)
        })
    }).unwrap_or(false)
}

/// Proxy path for an absolute upstream URL.
fn tv_proxy_path(u: &url::Url) -> String {
    format!("/api/tv/hls?url={}", urlencoding::encode(u.as_str()))
}

/// Rewrite every URI in an HLS playlist (plain URI lines and `URI="…"`
/// attributes) to an absolute URL through the proxy, learning each host.
fn rewrite_hls_playlist(body: &str, base: &url::Url) -> String {
    let rewrite = |raw: &str| -> String {
        match base.join(raw.trim()) {
            Ok(abs) => {
                tv_learn_host(&abs);
                tv_proxy_path(&abs)
            }
            Err(_) => raw.to_string(),
        }
    };
    let mut out = String::with_capacity(body.len() + 256);
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() {
            out.push_str(line);
        } else if t.starts_with('#') {
            // Tags that carry a URI attribute: EXT-X-KEY, -MEDIA, -MAP, -I-FRAME-STREAM-INF, ...
            if let Some(pos) = line.find("URI=\"") {
                let start = pos + 5;
                if let Some(len) = line[start..].find('"') {
                    out.push_str(&line[..start]);
                    out.push_str(&rewrite(&line[start..start + len]));
                    out.push_str(&line[start + len..]);
                } else {
                    out.push_str(line);
                }
            } else {
                out.push_str(line);
            }
        } else {
            out.push_str(&rewrite(t));
        }
        out.push('\n');
    }
    out
}

async fn api_tv_hls(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    use axum::body::Body;
    use axum::http::StatusCode;
    let reject = |code: StatusCode, msg: String| {
        (code, [("content-type", "text/plain")], Body::from(msg)).into_response()
    };
    let Some(raw) = params.get("url") else {
        return reject(StatusCode::BAD_REQUEST, "missing url param".into());
    };
    let Ok(mut current) = url::Url::parse(raw) else {
        return reject(StatusCode::BAD_REQUEST, "invalid url".into());
    };
    // No automatic redirects (same reason as the radio proxy): each hop is vetted.
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("Mozilla/5.0 (X11; Linux aarch64) RasCar")
        .build()
    {
        Ok(c) => c,
        Err(e) => return reject(StatusCode::BAD_GATEWAY, format!("client error: {e}")),
    };
    for hop in 0..4 {
        let host = match ensure_public_http_url(&current).await {
            Ok(h) => h,
            Err(reason) => return reject(StatusCode::BAD_REQUEST, format!("rejected url: {reason}")),
        };
        if !tv_host_allowed(&host).await {
            return reject(StatusCode::FORBIDDEN, "host not in configured tv channels".into());
        }
        let resp = match client.get(current.clone()).send().await {
            Ok(r) => r,
            Err(e) => return reject(StatusCode::BAD_GATEWAY, format!("upstream error: {e}")),
        };
        if resp.status().is_redirection() {
            let next = resp.headers().get("location").and_then(|v| v.to_str().ok())
                .and_then(|loc| current.join(loc).ok());
            match next {
                Some(n) if hop < 3 => {
                    tv_learn_host(&n); // pointed at by an allowed host
                    current = n;
                    continue;
                }
                _ => return reject(StatusCode::BAD_GATEWAY, "too many or invalid redirects".into()),
            }
        }
        let status = resp.status();
        let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream").to_string();
        let is_playlist = ct.to_ascii_lowercase().contains("mpegurl")
            || current.path().to_ascii_lowercase().ends_with(".m3u8");
        if is_playlist {
            let text = match resp.text().await {
                Ok(t) => t,
                Err(e) => return reject(StatusCode::BAD_GATEWAY, format!("upstream read error: {e}")),
            };
            let body = rewrite_hls_playlist(&text, &current);
            return (
                status,
                [(axum::http::header::CONTENT_TYPE, "application/vnd.apple.mpegurl".to_string())],
                Body::from(body),
            ).into_response();
        }
        return (
            status,
            [(axum::http::header::CONTENT_TYPE, ct)],
            Body::from_stream(resp.bytes_stream()),
        ).into_response();
    }
    reject(StatusCode::BAD_GATEWAY, "too many redirects".into())
}

async fn api_contacts() -> Json<serde_json::Value> {
    Json(read_config_json("contacts.json").await)
}

/// Embedded apps the dashboard may frame (the page picks an id, never a URL).
async fn api_apps() -> Json<serde_json::Value> {
    let apps: Vec<serde_json::Value> = services::apps::APPS
        .iter()
        .map(|a| serde_json::json!({"id": a.id, "name": a.name, "url": a.url}))
        .collect();
    Json(serde_json::json!({"apps": apps}))
}

fn apps_result(r: anyhow::Result<()>) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    match r {
        Ok(()) => (axum::http::StatusCode::OK, Json(serde_json::json!({"ok": true}))),
        Err(e) => {
            warn!("apps: {e:#}");
            (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"ok": false, "error": e.to_string()})))
        }
    }
}

#[derive(Deserialize)]
struct PipBody {
    on: bool,
}

/// Open the app's sign-in page in its own car-browser tab (see services/signin.rs).
async fn api_apps_signin(State(s): State<AppState>, axum::extract::Path(id): axum::extract::Path<String>) -> impl IntoResponse {
    let music = s.music.clone();
    let youtube_music = id == "ytmusic";
    apps_result(
        services::signin::open(&id, s.broadcast_tx.clone(), move || async move {
            if !youtube_music {
                return serde_json::Value::Null;
            }
            // The helper uses the browser's session in place (services/ytbridge.rs).
            serde_json::json!({ "session": music.after_signin().await })
        })
        .await,
    )
}

// --- Meeting state → rascar-carplay (car microphone) ---

#[derive(Deserialize)]
struct CallBody {
    active: bool,
}

/// The page reports a Meet/Teams frame open (every few seconds) or closed.
/// Relayed to rascar-carplay, which switches the car to a hands-free stream
/// and exposes its microphone as the "Car microphone" source
/// (carplay/src/stream/call.rs; a call without heartbeats ends by itself).
async fn api_call(State(s): State<AppState>, Json(b): Json<CallBody>) -> impl IntoResponse {
    services::phone::note_meeting(b.active);
    // The phone wins: while a modem call holds the car's telephony stream, a
    // meeting's `call 0` would hang it up in the car. Not relayed; the phone
    // sends `call 0` itself at the end unless the meeting still beats.
    if !b.active && s.phone.owns_call() {
        return Json(serde_json::json!({ "ok": true, "phone": true }));
    }
    const CARPLAY_CALL_CTRL: &str = "127.0.0.1:7703";
    let msg: &[u8] = if b.active { b"call 1" } else { b"call 0" };
    let sent = match tokio::net::UdpSocket::bind("127.0.0.1:0").await {
        Ok(sock) => sock.send_to(msg, CARPLAY_CALL_CTRL).await.is_ok(),
        Err(_) => false,
    };
    Json(serde_json::json!({ "ok": sent }))
}

// --- "Car" button: give the screen back to the Jaguar's own UI ---

/// rascar-carplay's screen control socket. It exists only while a CarPlay
/// session streams (opened at modesChanged); it understands `screen car` (the
/// car shows its native screen until the user taps its CarPlay icon) and
/// `screen phone`.
const CARPLAY_SCREEN_CTRL: &str = "127.0.0.1:7704";

/// Whether 127.0.0.1:7704/udp is bound, i.e. a CarPlay session is live.
fn carplay_session_live() -> bool {
    std::fs::read_to_string("/proc/net/udp").map(|t| udp_port_bound(&t, 7704)).unwrap_or(false)
}

/// `/proc/net/udp` lists `local_address` as hex `ADDR:PORT` (127.0.0.1 =
/// 0100007F on little-endian); a wildcard bind (00000000) counts too.
fn udp_port_bound(table: &str, port: u16) -> bool {
    let want = format!(":{port:04X}");
    table.lines().skip(1).filter_map(|l| l.split_whitespace().nth(1)).any(|local| {
        local.ends_with(&want) && (local.starts_with("0100007F") || local.starts_with("00000000"))
    })
}

async fn api_carplay_native_get() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "session": carplay_session_live() }))
}

/// Hand the car its native screen (the dashboard's "Car" button).
async fn api_carplay_native() -> Json<serde_json::Value> {
    let session = carplay_session_live();
    let sent = match tokio::net::UdpSocket::bind("127.0.0.1:0").await {
        Ok(sock) => sock.send_to(b"screen car", CARPLAY_SCREEN_CTRL).await.is_ok(),
        Err(_) => false,
    };
    Json(serde_json::json!({ "ok": sent, "session": session }))
}

// --- 4G stick status (services/cell.rs) ---

async fn api_cell(State(s): State<AppState>) -> Json<services::cell::CellStatus> {
    Json(s.cell.status.read().await.clone())
}

// --- Settings ▸ Connections (Wi-Fi, Bluetooth OBD adapter) ---

fn conn_err(e: anyhow::Error) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    warn!("connections: {e:#}");
    (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"ok": false, "error": e.to_string()})))
}

async fn api_wifi() -> impl IntoResponse {
    match services::connections::wifi_status().await {
        Ok(st) => (axum::http::StatusCode::OK, Json(serde_json::json!({"ok": true, "status": st}))),
        Err(e) => conn_err(e),
    }
}

async fn api_wifi_scan() -> impl IntoResponse {
    match services::connections::wifi_scan().await {
        Ok(n) => (axum::http::StatusCode::OK, Json(serde_json::json!({"ok": true, "networks": n}))),
        Err(e) => conn_err(e),
    }
}

#[derive(Deserialize)]
struct WifiConnectBody {
    ssid: String,
    #[serde(default)]
    password: Option<String>,
}

async fn api_wifi_connect(Json(b): Json<WifiConnectBody>) -> impl IntoResponse {
    let pw = b.password.filter(|p| !p.is_empty());
    match services::connections::wifi_connect(&b.ssid, pw.as_deref()).await {
        Ok(()) => (axum::http::StatusCode::OK, Json(serde_json::json!({"ok": true}))),
        Err(e) => conn_err(e),
    }
}

async fn api_wifi_retry() -> impl IntoResponse {
    match services::connections::wifi_retry().await {
        Ok(()) => (axum::http::StatusCode::OK, Json(serde_json::json!({"ok": true}))),
        Err(e) => conn_err(e),
    }
}

#[derive(Deserialize)]
struct WifiForgetBody {
    id: u32,
}

async fn api_wifi_forget(Json(b): Json<WifiForgetBody>) -> impl IntoResponse {
    match services::connections::wifi_forget(b.id).await {
        Ok(()) => (axum::http::StatusCode::OK, Json(serde_json::json!({"ok": true}))),
        Err(e) => conn_err(e),
    }
}

/// Devices BlueZ knows, the chosen OBD adapter, and what `rascar-obd-link`
/// reports about it (`/run/rascar/obd-link.status`).
async fn api_bluetooth(State(s): State<AppState>) -> impl IntoResponse {
    let cfg = s.settings.settings.read().await.clone();
    let link = tokio::fs::read_to_string("/run/rascar/obd-link.status").await.unwrap_or_default();
    match services::connections::bt_devices().await {
        Ok(d) => (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "devices": d,
                "obd": { "mac": cfg.obd_device, "name": cfg.obd_device_name, "custom_pin": cfg.obd_pin.is_some(),
                         "link": link.trim(), "bound": std::path::Path::new("/dev/rfcomm0").exists() },
            })),
        ),
        Err(e) => conn_err(e),
    }
}

async fn api_bluetooth_scan(State(s): State<AppState>) -> impl IntoResponse {
    if let Err(e) = services::connections::bt_scan(10).await {
        return conn_err(e).into_response();
    }
    api_bluetooth(State(s)).await.into_response()
}

#[derive(Deserialize)]
struct ObdChoiceBody {
    /// None / "" = forget the choice (back to automatic discovery).
    #[serde(default)]
    mac: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    pin: Option<String>,
}

async fn api_bluetooth_obd(State(s): State<AppState>, Json(b): Json<ObdChoiceBody>) -> impl IntoResponse {
    let mac = b.mac.map(|m| m.trim().to_uppercase()).filter(|m| !m.is_empty());
    if let Some(m) = &mac {
        if !services::connections::valid_mac(m) {
            return conn_err(anyhow::anyhow!("invalid Bluetooth address")).into_response();
        }
    }
    let pin = b.pin.map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
    if pin.as_ref().is_some_and(|p| p.len() > 16 || !p.bytes().all(|c| c.is_ascii_digit())) {
        return conn_err(anyhow::anyhow!("the PIN must be digits only")).into_response();
    }
    let mut cfg = s.settings.settings.read().await.clone();
    cfg.obd_device_name = mac.as_ref().and(b.name.map(|n| n.chars().take(64).collect()));
    cfg.obd_device = mac;
    cfg.obd_pin = pin;
    match s.settings.save(cfg).await {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => conn_err(e).into_response(),
    }
}

async fn api_apps_pip(
    State(s): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<PipBody>,
) -> impl IntoResponse {
    apps_result(s.apps.set_pip(&id, body.on).await)
}

#[derive(Deserialize)]
struct MeetingBody {
    url: String,
}

/// Validate a calendar event's Google Meet / Teams link; the page then frames
/// the returned URL as the returned app.
async fn api_apps_meeting(State(s): State<AppState>, Json(body): Json<MeetingBody>) -> impl IntoResponse {
    match s.apps.prepare_meeting(&body.url).await {
        Ok((app, url)) => (axum::http::StatusCode::OK, Json(serde_json::json!({"ok": true, "id": app.id, "name": app.name, "url": url}))),
        Err(e) => {
            warn!("apps: {e:#}");
            (axum::http::StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "error": e.to_string()})))
        }
    }
}

#[derive(Deserialize)]
struct InputBody {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    key: Option<String>,
}

/// On-screen keyboard → the focused field in the car browser (e.g. the Maps
/// search box inside its iframe, which the dashboard page cannot reach).
async fn api_input(State(s): State<AppState>, Json(body): Json<InputBody>) -> impl IntoResponse {
    use services::apps::KeyInput;
    let key = match (body.text, body.key.as_deref()) {
        (Some(t), None) if !t.is_empty() && t.chars().count() <= 200 => KeyInput::Text(t),
        (None, Some("Backspace")) => KeyInput::Backspace,
        (None, Some("Enter")) => KeyInput::Enter,
        _ => return apps_result(Err(anyhow::anyhow!("send one of text (1-200 chars) or key Backspace/Enter"))),
    };
    apps_result(s.apps.type_input(key).await)
}

/// The dashboard's call log, newest first: dialer entries posted by the page,
/// plus every modem call (in/out, missed) appended by services/phone.
const CALL_HISTORY_FILE: &str = "call-history.json";
const CALL_HISTORY_MAX: usize = 50;
/// Serialises read-modify-write of the history file across concurrent posts.
static CALL_HISTORY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Deserialize)]
struct CallLogEntry {
    number: String,
    #[serde(default)]
    name: String,
}

/// Digits plus the dialable symbols a keypad can produce; at least one digit.
fn is_dialable(number: &str) -> bool {
    !number.is_empty()
        && number.len() <= 32
        && number.chars().all(|c| c.is_ascii_digit() || "+*# ".contains(c))
        && number.chars().any(|c| c.is_ascii_digit())
}

/// Prepend one entry (newest first, capped) to the call history. Shared by
/// the dialer's POST and the phone service's ended calls, which add
/// `direction`, `missed` and `duration_s` to the same `{number, name, at}` shape.
async fn append_call_history(entry: serde_json::Value) -> std::io::Result<Vec<serde_json::Value>> {
    let _guard = CALL_HISTORY_LOCK.lock().await;
    let mut list = match read_config_json(CALL_HISTORY_FILE).await {
        serde_json::Value::Array(a) => a,
        _ => Vec::new(),
    };
    list.insert(0, entry);
    list.truncate(CALL_HISTORY_MAX);
    // Write-then-rename so a power cut mid-write never leaves a torn file.
    let path = config_path(CALL_HISTORY_FILE);
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(&list).unwrap_or_default();
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::write(&tmp, data).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(list)
}

async fn api_calls() -> Json<serde_json::Value> {
    Json(read_config_json(CALL_HISTORY_FILE).await)
}

async fn api_calls_post(Json(body): Json<CallLogEntry>) -> impl IntoResponse {
    let number = body.number.trim().to_string();
    if !is_dialable(&number) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"ok": false, "error": "invalid number"})),
        );
    }
    let name: String = body.name.trim().chars().take(80).collect();

    let entry = serde_json::json!({
        "number": number,
        "name": name,
        "at": chrono::Utc::now().to_rfc3339(),
    });
    let list = match append_call_history(entry).await {
        Ok(list) => list,
        Err(e) => {
            warn!("call history not saved: {e}");
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "error": "history not saved"})),
            );
        }
    };
    (axum::http::StatusCode::OK, Json(serde_json::Value::Array(list)))
}

/// Max time any single music-helper invocation may take before we give up, so a
/// slow/hung sidecar can never wedge a request handler.
const MUSIC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Rate the currently-playing track (from MPRIS `now_playing`) on YouTube Music.
/// True when the MPRIS player on screen is the dashboard's own mpv.
async fn is_our_player(s: &AppState) -> bool {
    s.media.now_playing.read().await.player_name.starts_with("mpv") && s.player.current_video_id().await.is_some()
}

async fn rate_current(s: &AppState, rating: &str) -> Json<serde_json::Value> {
    // Our own player knows the exact videoId: rate that one, not a search hit.
    if is_our_player(s).await {
        if let Some(id) = s.player.current_video_id().await {
            return match tokio::time::timeout(MUSIC_TIMEOUT, s.music.rate(&id, rating)).await {
                Ok(Ok(_)) => Json(serde_json::json!({"ok": true, "rated": true, "videoId": id})),
                Ok(Err(e)) => Json(serde_json::json!({"ok": false, "rated": false, "error": e.to_string()})),
                Err(_) => Json(serde_json::json!({"ok": false, "rated": false, "error": "music helper timed out"})),
            };
        }
    }
    let (title, artist) = {
        let np = s.media.now_playing.read().await;
        (np.title.clone(), np.artist.clone())
    };
    if title.trim().is_empty() {
        return Json(serde_json::json!({
            "ok": false, "rated": false, "error": "no track playing"
        }));
    }

    let result = match rating {
        "DISLIKE" => {
            tokio::time::timeout(MUSIC_TIMEOUT, s.music.dislike_current(&title, &artist)).await
        }
        _ => tokio::time::timeout(MUSIC_TIMEOUT, s.music.like_current(&title, &artist)).await,
    };

    match result {
        Ok(Ok(())) => Json(serde_json::json!({"ok": true, "rated": true})),
        Ok(Err(e)) => Json(serde_json::json!({
            "ok": false, "rated": false, "error": e.to_string()
        })),
        Err(_) => Json(serde_json::json!({
            "ok": false, "rated": false, "error": "music helper timed out"
        })),
    }
}

async fn api_media_like(State(s): State<AppState>) -> Json<serde_json::Value> {
    rate_current(&s, "LIKE").await
}

/// Thumbs down also skips: nobody wants to hear the rest of a disliked song.
async fn api_media_dislike(State(s): State<AppState>) -> Json<serde_json::Value> {
    let Json(mut v) = rate_current(&s, "DISLIKE").await;
    if v["rated"] == true {
        v["skipped"] = s.media.next_track().await.into();
    }
    Json(v)
}

/// Map a timed music-helper call into a JSON response: the raw value on success,
/// `{"error": ...}` on failure or timeout.
fn music_json(
    r: Result<anyhow::Result<serde_json::Value>, tokio::time::error::Elapsed>,
) -> Json<serde_json::Value> {
    match r {
        Ok(Ok(v)) => Json(v),
        Ok(Err(e)) => Json(serde_json::json!({"error": e.to_string()})),
        Err(_) => Json(serde_json::json!({"error": "music helper timed out"})),
    }
}

/// `connected` only after an authenticated call through the car browser
/// succeeded. `session`: "ok", "none" (the browser is not signed in to
/// YouTube, or signed out on the dashboard), "unavailable" (helper, browser
/// or network).
async fn api_music_status(State(s): State<AppState>) -> Json<serde_json::Value> {
    let (connected, session) = match tokio::time::timeout(MUSIC_TIMEOUT, s.music.status()).await {
        Ok(Ok(_)) => (true, "ok"),
        Ok(Err(e)) if e.to_string() == "auth" => (false, "none"),
        _ => (false, "unavailable"),
    };
    Json(serde_json::json!({
        "connected": connected, "session": session,
        "provider": if connected { serde_json::json!("ytmusic") } else { serde_json::Value::Null },
    }))
}

// --- YouTube Music session (the car browser's own, services/ytbridge.rs) ---

/// Stop using the browser session for YouTube Music until the user signs in
/// again from the YouTube Music page (the Google session itself stays).
async fn api_music_signout(State(s): State<AppState>) -> Json<serde_json::Value> {
    match s.music.sign_out().await {
        Ok(()) => Json(serde_json::json!({"ok": true})),
        Err(e) => {
            warn!("YouTube Music sign-out failed: {e:#}");
            Json(serde_json::json!({"ok": false, "error": "io"}))
        }
    }
}

/// Browser fetch bridge for the YouTube Music helper (services/ytbridge.rs):
/// loopback peers with this process's token only, YouTube Music API URLs only.
async fn internal_ytm_fetch(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<services::ytbridge::FetchRequest>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    let token_ok = headers
        .get(services::ytbridge::TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t == services::ytbridge::token());
    // A page in the car browser is loopback too: it would send an Origin.
    if !services::ytbridge::peer_allowed(&peer) || !token_ok || headers.contains_key(axum::http::header::ORIGIN) {
        warn!("ytbridge: refused a call from {peer}");
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "forbidden"}))).into_response();
    }
    if !services::ytbridge::url_allowed(&req.method, &req.url) {
        // The path only: a query can carry ids.
        let shown = url::Url::parse(&req.url).map(|u| format!("{}{}", u.host_str().unwrap_or(""), u.path())).unwrap_or_default();
        warn!("ytbridge: refused {} {shown}", req.method);
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "url not allowed"}))).into_response();
    }
    match services::ytbridge::fetch(req).await {
        Ok(reply) => Json(reply).into_response(),
        Err(e) => {
            warn!("ytbridge: {e:#}");
            (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": "browser unavailable"}))).into_response()
        }
    }
}

/// Home feed shelves (works signed out too: public recommendations).
async fn api_music_home(State(s): State<AppState>) -> Json<serde_json::Value> {
    music_json(tokio::time::timeout(MUSIC_TIMEOUT, s.music.home(6)).await)
}

async fn api_music_history(State(s): State<AppState>, Query(q): Query<LikedQuery>) -> Json<serde_json::Value> {
    music_json(tokio::time::timeout(MUSIC_TIMEOUT, s.music.history(q.limit.unwrap_or(30))).await)
}

#[derive(Deserialize)]
struct LikedQuery {
    limit: Option<u32>,
}

async fn api_music_liked(
    State(s): State<AppState>,
    Query(q): Query<LikedQuery>,
) -> Json<serde_json::Value> {
    let limit = q.limit.unwrap_or(100);
    music_json(tokio::time::timeout(MUSIC_TIMEOUT, s.music.liked(limit)).await)
}

async fn api_music_playlists(State(s): State<AppState>) -> Json<serde_json::Value> {
    music_json(tokio::time::timeout(MUSIC_TIMEOUT, s.music.playlists()).await)
}

async fn api_music_playlist(
    State(s): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(q): Query<LikedQuery>,
) -> Json<serde_json::Value> {
    let limit = q.limit.unwrap_or(200);
    music_json(tokio::time::timeout(MUSIC_TIMEOUT, s.music.playlist(&id, limit)).await)
}

async fn api_music_artist(
    State(s): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    music_json(tokio::time::timeout(MUSIC_TIMEOUT, s.music.artist(&id)).await)
}

#[derive(Deserialize)]
struct MusicSearchQuery {
    q: Option<String>,
}

async fn api_music_search(
    State(s): State<AppState>,
    Query(query): Query<MusicSearchQuery>,
) -> Json<serde_json::Value> {
    let q = query.q.unwrap_or_default();
    if q.trim().is_empty() {
        return Json(serde_json::json!({"tracks": [], "artists": []}));
    }
    music_json(tokio::time::timeout(MUSIC_TIMEOUT, s.music.search(&q)).await)
}

fn player_json(r: anyhow::Result<()>) -> Json<serde_json::Value> {
    match r {
        Ok(()) => Json(serde_json::json!({"ok": true})),
        Err(e) => {
            warn!("player: {e:#}");
            Json(serde_json::json!({"ok": false, "error": e.to_string()}))
        }
    }
}

/// Play one track, then its radio (like the YouTube Music app).
async fn api_music_play(State(s): State<AppState>, Json(t): Json<Track>) -> Json<serde_json::Value> {
    let r = s.player.play_song(t).await;
    if r.is_ok() { s.resume.user_started_ytm().await; }
    player_json(r)
}

#[derive(Deserialize)]
struct QueueBody {
    tracks: Vec<Track>,
    #[serde(default)]
    start: usize,
    #[serde(default)]
    shuffle: bool,
}

/// Replace the queue: Play all, or a tap on an entry of a list.
async fn api_music_queue_set(State(s): State<AppState>, Json(b): Json<QueueBody>) -> Json<serde_json::Value> {
    let r = s.player.play_list(b.tracks, b.start, b.shuffle).await;
    if r.is_ok() { s.resume.user_started_ytm().await; }
    player_json(r)
}

async fn api_music_queue_get(State(s): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(s.player.state().await).unwrap_or_default())
}

#[derive(Deserialize)]
struct RadioBody {
    #[serde(rename = "videoId")]
    video_id: String,
}

async fn api_music_radio(State(s): State<AppState>, Json(b): Json<RadioBody>) -> Json<serde_json::Value> {
    let r = s.player.play_radio(&b.video_id).await;
    if r.is_ok() { s.resume.user_started_ytm().await; }
    player_json(r)
}

#[derive(Deserialize)]
struct EnqueueBody {
    track: Track,
    /// Play next (right after the current track) rather than at the end.
    #[serde(default)]
    next: bool,
}

async fn api_music_enqueue(State(s): State<AppState>, Json(b): Json<EnqueueBody>) -> Json<serde_json::Value> {
    player_json(s.player.enqueue(b.track, b.next).await)
}

async fn api_music_shuffle(State(s): State<AppState>) -> Json<serde_json::Value> {
    match s.player.toggle_shuffle().await {
        Ok(on) => Json(serde_json::json!({"ok": true, "shuffle": on})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

#[derive(Deserialize)]
struct RepeatBody {
    /// Omitted: cycle off → all → one.
    mode: Option<Repeat>,
}

async fn api_music_repeat(State(s): State<AppState>, body: Option<Json<RepeatBody>>) -> Json<serde_json::Value> {
    let r = match body.and_then(|b| b.0.mode) {
        Some(m) => s.player.set_repeat(m).await.map(|_| m),
        None => s.player.cycle_repeat().await,
    };
    match r {
        Ok(m) => Json(serde_json::json!({"ok": true, "repeat": m})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

#[derive(Deserialize)]
struct SeekBody {
    position: f64,
}

async fn api_music_seek(State(s): State<AppState>, Json(b): Json<SeekBody>) -> Json<serde_json::Value> {
    player_json(s.player.seek(b.position).await)
}

#[derive(Deserialize)]
struct JumpBody {
    index: usize,
}

async fn api_music_jump(State(s): State<AppState>, Json(b): Json<JumpBody>) -> Json<serde_json::Value> {
    player_json(s.player.jump(b.index).await)
}

#[derive(Deserialize)]
struct LyricsQuery {
    title: Option<String>,
    artist: Option<String>,
}

/// Lyrics for the karaoke view (services/lyrics.rs). When our own player plays
/// the track, its exact videoId and duration make the lookup precise.
async fn api_media_lyrics(State(s): State<AppState>, Query(q): Query<LyricsQuery>) -> Json<serde_json::Value> {
    let title = q.title.unwrap_or_default();
    let artist = q.artist.unwrap_or_default();
    if title.trim().is_empty() {
        return Json(serde_json::json!({"lines": [], "synced": false}));
    }
    let (video_id, duration) = match s.player.playback().await {
        Some(pb) if pb.track.title == title => (Some(pb.track.video_id), pb.duration),
        _ => (None, s.media.now_playing.read().await.length_sec),
    };
    let l = s.lyrics.get(&title, &artist, video_id.as_deref(), duration).await;
    Json(serde_json::to_value(l).unwrap_or_default())
}


async fn api_obd_history(
    State(s): State<AppState>,
    axum::extract::Path(metric): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    // The metric isn't one we decode — say so explicitly rather than returning
    // an empty series that looks like "connected but no data".
    let Some(meta) = services::obd::pids::output(&metric) else {
        return Json(serde_json::json!({ "metric": metric, "data": [], "unknown": true }));
    };
    let h = s.obd.histories.read().await;
    let data: Vec<f64> = h.get(metric.as_str()).map(|b| b.iter().cloned().collect()).unwrap_or_default();
    Json(serde_json::json!({
        "metric": metric,
        "label": meta.label,
        "unit": meta.unit,
        "decimals": meta.decimals,
        "data": data,
    }))
}

async fn api_obd_dtc(State(s): State<AppState>) -> Json<serde_json::Value> {
    let dtcs = s.obd.get_dtc_list().await;
    Json(serde_json::to_value(&dtcs).unwrap_or_default())
}

/// Clear stored fault codes.
///
/// Reports what the ECU actually did. Answering `{"ok": true}` before the
/// mode-04 command was even transmitted told the driver the codes were gone
/// while the MIL stayed lit.
async fn api_obd_dtc_clear(State(s): State<AppState>) -> Json<serde_json::Value> {
    let outcome = s.obd.clear_dtcs().await;
    Json(serde_json::to_value(&outcome).unwrap_or_default())
}

async fn api_settings_get(State(s): State<AppState>) -> Json<serde_json::Value> {
    let settings = s.settings.settings.read().await;
    let mut v = serde_json::to_value(&*settings).unwrap_or_default();
    // Private ICS feed URLs embed an access token: report only how many there are.
    if let Some(obj) = v.as_object_mut() {
        obj.remove("calendar_ics_uris");
        obj.remove("obd_pin");
        // Hand-edited extras (contacts, phone numbers) are not the page's business.
        for k in settings.extra.keys() { obj.remove(k); }
        obj.insert("calendar_sources".into(), serde_json::json!(settings.calendar_ics_uris.len()));
    }
    Json(v)
}

/// Incoming settings body.
///
/// The numeric fields are `u32` in `AppSettings`, so deserializing straight
/// into it makes a negative number a 422 rejection with no usable message —
/// axum answers before the handler runs. Taking them as `i64` lets a malformed
/// value produce a clear, displayable error (or be clamped) instead.
#[derive(Deserialize)]
struct SettingsBody {
    speed_alarm_kmh: Option<i64>,
    units: Option<String>,
    screensaver_timeout: Option<i64>,
    #[serde(default)]
    ytmusic_auth_path: Option<String>,
    #[serde(default)]
    ytmusic_enabled: Option<bool>,
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    radio_favorites: Option<Vec<String>>,
    #[serde(default)]
    tv_favorites: Option<Vec<String>>,
    #[serde(default)]
    calendar_hidden: Option<Vec<String>>,
    #[serde(default)]
    fuel_tank_l: Option<f64>,
    #[serde(default)]
    fuel_price_eur_l: Option<f64>,
    #[serde(default)]
    fuel_l100: Option<f64>,
}

/// A fuel setting from the page, clamped into its plausible range. A
/// non-finite value keeps the current one.
fn clamp_f64(warnings: &mut Vec<String>, name: &str, v: Option<f64>, current: f64, lo: f64, hi: f64) -> f64 {
    match v {
        None => current,
        Some(n) if !n.is_finite() => {
            warnings.push(format!("{name} is not a number — kept {current}"));
            current
        }
        Some(n) if n < lo || n > hi => {
            warnings.push(format!("{name} must be between {lo} and {hi} — clamped"));
            n.clamp(lo, hi)
        }
        Some(n) => n,
    }
}

/// A favourites list from the page: names only, trimmed, no duplicates, bounded.
fn clean_favorites(v: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in v.into_iter().map(|s| s.trim().chars().take(120).collect::<String>()) {
        if !s.is_empty() && !out.contains(&s) && out.len() < 200 {
            out.push(s);
        }
    }
    out
}

async fn api_settings_post(
    State(s): State<AppState>,
    Json(body): Json<SettingsBody>,
) -> Json<serde_json::Value> {
    let current = s.settings.settings.read().await.clone();

    // Clamp rather than reject: a negative alarm/timeout is meaningless, and
    // 0 already means "off" for both.
    let mut warnings: Vec<String> = Vec::new();
    let mut clamp = |name: &str, v: Option<i64>, fallback: u32| -> u32 {
        match v {
            None => fallback,
            Some(n) if n < 0 => {
                warnings.push(format!("{name} cannot be negative — treated as 0"));
                0
            }
            Some(n) if n > u32::MAX as i64 => {
                warnings.push(format!("{name} is too large — capped"));
                u32::MAX
            }
            Some(n) => n as u32,
        }
    };

    let new_settings = services::settings::AppSettings {
        speed_alarm_kmh: clamp("speed_alarm_kmh", body.speed_alarm_kmh, current.speed_alarm_kmh),
        screensaver_timeout: clamp(
            "screensaver_timeout",
            body.screensaver_timeout,
            current.screensaver_timeout,
        ),
        units: body.units.unwrap_or(current.units),
        ytmusic_auth_path: body.ytmusic_auth_path.or(current.ytmusic_auth_path),
        ytmusic_enabled: body.ytmusic_enabled.or(current.ytmusic_enabled),
        theme: match body.theme {
            Some(t) if services::settings::THEMES.contains(&t.as_str()) => t,
            Some(t) => {
                warnings.push(format!("unknown theme {t:?} — kept {:?}", current.theme));
                current.theme
            }
            None => current.theme,
        },
        radio_favorites: body.radio_favorites.map(clean_favorites).unwrap_or(current.radio_favorites),
        tv_favorites: body.tv_favorites.map(clean_favorites).unwrap_or(current.tv_favorites),
        calendar_hidden: body.calendar_hidden.map(clean_favorites).unwrap_or(current.calendar_hidden),
        // Set through /api/bluetooth/obd only.
        obd_device: current.obd_device,
        obd_device_name: current.obd_device_name,
        obd_pin: current.obd_pin,
        // Not editable from the page (and never sent to it): keep what the file has.
        calendar_ics_uris: current.calendar_ics_uris,
        // Set through /api/nav/home only.
        home: current.home,
        // Hand-edited in settings.json only.
        nav_favorites: current.nav_favorites,
        fuel_tank_l: clamp_f64(&mut warnings, "fuel_tank_l", body.fuel_tank_l, current.fuel_tank_l, 10.0, 150.0),
        fuel_price_eur_l: clamp_f64(&mut warnings, "fuel_price_eur_l", body.fuel_price_eur_l, current.fuel_price_eur_l, 0.5, 5.0),
        fuel_l100: clamp_f64(&mut warnings, "fuel_l100", body.fuel_l100, current.fuel_l100, 2.0, 30.0),
        extra: current.extra,
    };

    match s.settings.save(new_settings).await {
        Ok(_) => Json(serde_json::json!({"ok": true, "warnings": warnings})),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})),
    }
}

// --- Fuel wallet ---

/// `GET /api/fuel` → status (level, balance, price, pending refill) + the last refills.
async fn api_fuel(State(s): State<AppState>) -> Json<serde_json::Value> {
    let status = s.fuel.status.read().await.clone();
    let refills = s.fuel.recent_refills(10).await;
    Json(serde_json::json!({"status": status, "refills": refills}))
}

#[derive(Deserialize)]
struct RefillBody {
    /// The pending refill being answered; absent = a manual entry.
    id: Option<i64>,
    amount_eur: f64,
    litres: Option<f64>,
}

/// `POST /api/fuel/refill {id?, amount_eur, litres?}` → the confirmed refill.
async fn api_fuel_refill(State(s): State<AppState>, Json(b): Json<RefillBody>) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    match s.fuel.confirm(b.id, b.amount_eur, b.litres).await {
        Ok(r) => (axum::http::StatusCode::OK, Json(serde_json::json!({"ok": true, "refill": r}))),
        Err(e) => (axum::http::StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "error": e}))),
    }
}

/// `POST /api/fuel/refill/{id}/dismiss` — "that was not a refill".
async fn api_fuel_dismiss(State(s): State<AppState>, axum::extract::Path(id): axum::extract::Path<i64>) -> axum::http::StatusCode {
    if s.fuel.dismiss(id).await {
        axum::http::StatusCode::NO_CONTENT
    } else {
        axum::http::StatusCode::NOT_FOUND
    }
}

// --- Kiosk log ---
//
// The car kiosk's console is not captured anywhere, so a page fault in the car
// (the offline map that never moved on 2026-10-01) left no trace. The page posts
// short diagnostic lines here; they land in this service's journal, which the
// debug capture keeps on disk. Bounded: 500 chars a line, 60 lines a minute.

static KIOSK_LOG_BUDGET: std::sync::Mutex<(Option<std::time::Instant>, u32)> = std::sync::Mutex::new((None, 0));

async fn api_kiosk_log(body: String) -> axum::http::StatusCode {
    {
        let mut b = KIOSK_LOG_BUDGET.lock().unwrap_or_else(|e| e.into_inner());
        if b.0.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(60)) {
            *b = (Some(std::time::Instant::now()), 0);
        }
        if b.1 >= 60 {
            return axum::http::StatusCode::TOO_MANY_REQUESTS;
        }
        b.1 += 1;
    }
    let line: String = body.chars().filter(|c| !c.is_control()).take(500).collect();
    info!("kiosk: {line}");
    axum::http::StatusCode::NO_CONTENT
}

// --- Crypto proxy ---
//
// The dashboard's crypto screen used to call CoinGecko and alternative.me
// directly from the page. Proxying here keeps those requests on the same
// origin (no CORS, no key material in the page) and lets one cached answer
// serve every client — the car's uplink is metered and often absent.

/// How long a good crypto answer is reused before refetching.
const CRYPTO_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// Outbound deadline, matching the other proxied calls.
const CRYPTO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

static CRYPTO_CACHE: tokio::sync::Mutex<Option<(std::time::Instant, serde_json::Value)>> =
    tokio::sync::Mutex::const_new(None);

async fn api_crypto(State(s): State<AppState>) -> Json<serde_json::Value> {
    let key = coingecko_key(&s).await;
    let mut cache = CRYPTO_CACHE.lock().await;
    if let Some((at, value)) = cache.as_ref() {
        if at.elapsed() < CRYPTO_TTL {
            return Json(value.clone());
        }
    }

    match fetch_crypto(key.as_deref()).await {
        Ok(value) => {
            *cache = Some((std::time::Instant::now(), value.clone()));
            Json(value)
        }
        Err(e) => {
            tracing::warn!("crypto fetch failed: {e}");
            // Serve the last good value if we have one — a tunnel shouldn't
            // blank the screen — but say the refresh failed.
            match cache.as_ref() {
                Some((_, value)) => {
                    let mut stale = value.clone();
                    if let Some(obj) = stale.as_object_mut() {
                        obj.insert("error".into(), serde_json::json!("refresh failed"));
                        obj.insert("stale".into(), serde_json::json!(true));
                    }
                    Json(stale)
                }
                None => Json(serde_json::json!({"error": "unavailable"})),
            }
        }
    }
}

/// Optional CoinGecko demo key: `$RASCAR_COINGECKO_KEY`, else
/// `coingecko_api_key` hand-added to settings.json (an extra field, never sent
/// to the page). Keyless calls get 403/429 from shared or mobile IPs.
async fn coingecko_key(s: &AppState) -> Option<String> {
    if let Some(k) = std::env::var("RASCAR_COINGECKO_KEY").ok().filter(|k| !k.is_empty()) {
        return Some(k);
    }
    s.settings.settings.read().await.extra.get("coingecko_api_key")
        .and_then(|v| v.as_str()).filter(|k| !k.is_empty()).map(str::to_string)
}

/// After a 403/429 CoinGecko is left alone this long (Kraken serves meanwhile):
/// retrying every minute only prolongs the block.
const COINGECKO_BACKOFF: std::time::Duration = std::time::Duration::from_secs(300);
static COINGECKO_BLOCKED_UNTIL: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

fn coingecko_blocked() -> bool {
    COINGECKO_BLOCKED_UNTIL.lock().map(|u| u.is_some_and(|t| std::time::Instant::now() < t)).unwrap_or(false)
}

/// First `max` chars of a body, whitespace collapsed, for a log line.
fn body_snippet(body: &str, max: usize) -> String {
    body.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect()
}

fn crypto_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(CRYPTO_TIMEOUT)
        // Same rule as every other outbound client: no redirect chasing.
        .redirect(reqwest::redirect::Policy::none())
        // CoinGecko answers 4xx to clients that send no User-Agent.
        .user_agent("rascar-dashboard/0.1")
        .build()?)
}

/// GET a CoinGecko JSON document. Status first: its 403/429 answers are HTML
/// or a JSON error, and decoding them only ever said "error decoding response
/// body". A 403/429 also starts the back-off.
async fn coingecko_get(client: &reqwest::Client, url: &str, key: Option<&str>) -> anyhow::Result<serde_json::Value> {
    let mut req = client.get(url);
    if let Some(k) = key {
        req = req.header("x-cg-demo-api-key", k);
    }
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 403 || status.as_u16() == 429 {
            if let Ok(mut u) = COINGECKO_BLOCKED_UNTIL.lock() {
                *u = Some(std::time::Instant::now() + COINGECKO_BACKOFF);
            }
        }
        anyhow::bail!("coingecko HTTP {}: {}", status.as_u16(), body_snippet(&body, 120));
    }
    Ok(resp.json().await?)
}

/// GET a Kraken public endpoint; `error` must be empty and `result` present.
async fn kraken_get(client: &reqwest::Client, url: &str) -> anyhow::Result<serde_json::Value> {
    let resp = client.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("kraken HTTP {}: {}", status.as_u16(), body_snippet(&body, 120));
    }
    let v: serde_json::Value = resp.json().await?;
    if v["error"].as_array().is_some_and(|e| !e.is_empty()) {
        anyhow::bail!("kraken error: {}", v["error"]);
    }
    v.get("result").cloned().ok_or_else(|| anyhow::anyhow!("kraken answer without result"))
}

async fn fetch_crypto(key: Option<&str>) -> anyhow::Result<serde_json::Value> {
    let client = crypto_client()?;

    // CoinGecko first (it has rank, cap, ATH, supply); Kraken when it is
    // blocked or failing, for price, 24h range/volume, changes and sparkline.
    let mut coins: Option<Vec<serde_json::Value>> = None;
    let mut source = "coingecko";
    if !coingecko_blocked() {
        // One markets call carries everything the list and the detail view need.
        let url = format!(
            "https://api.coingecko.com/api/v3/coins/markets?vs_currency=eur&ids={}\
             &price_change_percentage=1h,24h,7d,30d&sparkline=true",
            CRYPTO_COINS.iter().map(|(id, _)| *id).collect::<Vec<_>>().join(",")
        );
        match coingecko_get(&client, &url, key).await {
            Ok(markets) => match markets.as_array().filter(|l| !l.is_empty()) {
                Some(list) => coins = Some(coins_from_coingecko(list)),
                None => tracing::warn!("crypto: coingecko returned no usable markets"),
            },
            Err(e) => tracing::warn!("crypto: {e}"),
        }
    }
    if coins.as_ref().is_none_or(|c| c.is_empty()) {
        source = "kraken";
        coins = Some(fetch_kraken_coins(&client).await?);
    }
    let coins = coins.unwrap_or_default();
    if coins.is_empty() {
        anyhow::bail!("no crypto source answered");
    }

    // The fear & greed index is a nice-to-have: a failure there must not lose
    // the prices we did get.
    let fng = match client.get("https://api.alternative.me/fng/?limit=1").send().await {
        Ok(r) if r.status().is_success() => r.json::<serde_json::Value>().await.ok(),
        Ok(r) => {
            tracing::warn!("fear & greed: HTTP {}", r.status().as_u16());
            None
        }
        Err(e) => {
            tracing::warn!("fear & greed fetch failed: {e}");
            None
        }
    };
    let fng_obj = fng.as_ref().and_then(|f| f["data"].get(0)).map(|d| {
        serde_json::json!({
            // alternative.me returns the value as a *string*.
            "value": d["value"].as_str().and_then(|v| v.parse::<f64>().ok())
                .or_else(|| d["value"].as_f64()),
            "classification": d["value_classification"].as_str().unwrap_or(""),
        })
    });

    // Legacy shape kept for any older page still asking for btc/eth.
    let coin = |k: &str| {
        let c = coins.iter().find(|c| c["key"] == k);
        serde_json::json!({
            "eur": c.and_then(|c| c["price"].as_f64()),
            "change_24h": c.and_then(|c| c["change_24h"].as_f64()),
        })
    };
    Ok(serde_json::json!({
        "btc": coin("btc"),
        "eth": coin("eth"),
        "coins": coins,
        "fng": fng_obj,
        "source": source,
    }))
}

fn coins_from_coingecko(list: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let find = |id: &str| list.iter().find(|m| m["id"].as_str() == Some(id));
    CRYPTO_COINS
        .iter()
        .filter_map(|(id, key)| {
            let m = find(id)?;
            let spark = m["sparkline_in_7d"]["price"].as_array().cloned().unwrap_or_default();
            Some(serde_json::json!({
                "key": key,
                "id": id,
                "symbol": m["symbol"].as_str().unwrap_or(key).to_uppercase(),
                "name": m["name"],
                "price": m["current_price"],
                "change_1h": m["price_change_percentage_1h_in_currency"],
                "change_24h": m["price_change_percentage_24h_in_currency"],
                "change_7d": m["price_change_percentage_7d_in_currency"],
                "change_30d": m["price_change_percentage_30d_in_currency"],
                "rank": m["market_cap_rank"],
                "market_cap": m["market_cap"],
                "volume_24h": m["total_volume"],
                "high_24h": m["high_24h"],
                "low_24h": m["low_24h"],
                "ath": m["ath"],
                "ath_change": m["ath_change_percentage"],
                "ath_date": m["ath_date"],
                "circulating_supply": m["circulating_supply"],
                "max_supply": m["max_supply"],
                "sparkline_7d": downsample(&spark, 84),
            }))
        })
        .collect()
}

/// Kraken markets for the coins: (CoinGecko id, Kraken pair, Ticker result key, name, symbol).
const KRAKEN_PAIRS: &[(&str, &str, &str, &str, &str)] = &[
    ("bitcoin", "XBTEUR", "XXBTZEUR", "Bitcoin", "BTC"),
    ("ethereum", "ETHEUR", "XETHZEUR", "Ethereum", "ETH"),
    ("zcash", "ZECEUR", "XZECZEUR", "Zcash", "ZEC"),
    ("zelcash", "FLUXEUR", "FLUXEUR", "Flux", "FLUX"),
];

/// Kraken sends numbers as strings.
fn kstr(v: &serde_json::Value) -> Option<f64> {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_f64())
}

/// `[(unix s, close)]` from a Kraken OHLC result (`{"<pair>": [[t,o,h,l,c,…]], "last": n}`).
fn kraken_closes(result: &serde_json::Value) -> Vec<(i64, f64)> {
    let Some(obj) = result.as_object() else { return Vec::new() };
    obj.iter()
        .find(|(k, _)| k.as_str() != "last")
        .and_then(|(_, v)| v.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|r| Some((r.get(0)?.as_i64()?, kstr(r.get(4)?)?)))
                .collect()
        })
        .unwrap_or_default()
}

/// % change from the close at or before `secs` before the last candle to
/// `price`. Candles are missing for hours without trades (thin pairs), so this
/// goes by timestamp, not by candle count.
fn change_since(closes: &[(i64, f64)], price: f64, secs: i64) -> Option<f64> {
    let (last_t, _) = *closes.last()?;
    let target = last_t - secs;
    let (_, then) = closes.iter().rev().find(|(t, _)| *t <= target)?;
    (*then > 0.0).then(|| (price - then) / then * 100.0)
}

/// One coin object (same shape as the CoinGecko one) from Kraken's ticker
/// entry and hourly closes; fields Kraken cannot give are null.
fn kraken_coin(id: &str, key: &str, name: &str, symbol: &str, t: &serde_json::Value, closes: &[(i64, f64)]) -> Option<serde_json::Value> {
    let price = kstr(t.get("c")?.get(0)?)?;
    let vol = kstr(&t["v"][1]).zip(kstr(&t["p"][1])).map(|(v, vwap)| v * vwap);
    let week: Vec<serde_json::Value> = closes
        .iter()
        .filter(|(ts, _)| closes.last().is_some_and(|(l, _)| l - ts <= 7 * 86_400))
        .map(|(_, c)| serde_json::json!(c))
        .collect();
    Some(serde_json::json!({
        "key": key, "id": id, "symbol": symbol, "name": name,
        "price": price,
        "change_1h": change_since(closes, price, 3_600),
        "change_24h": change_since(closes, price, 86_400),
        "change_7d": change_since(closes, price, 7 * 86_400),
        "change_30d": change_since(closes, price, 29 * 86_400),
        "rank": null, "market_cap": null,
        "volume_24h": vol,
        "high_24h": kstr(&t["h"][1]),
        "low_24h": kstr(&t["l"][1]),
        "ath": null, "ath_change": null, "ath_date": null,
        "circulating_supply": null, "max_supply": null,
        "sparkline_7d": downsample(&week, 84),
    }))
}

async fn fetch_kraken_coins(client: &reqwest::Client) -> anyhow::Result<Vec<serde_json::Value>> {
    let pairs = KRAKEN_PAIRS.iter().map(|p| p.1).collect::<Vec<_>>().join(",");
    let ticker = kraken_get(client, &format!("https://api.kraken.com/0/public/Ticker?pair={pairs}")).await?;
    let mut coins = Vec::new();
    for (id, pair, result_key, name, symbol) in KRAKEN_PAIRS {
        let Some(key) = CRYPTO_COINS.iter().find(|(c, _)| c == id).map(|(_, k)| *k) else { continue };
        let Some(t) = ticker.get(*result_key) else { continue };
        // Hourly candles: the last 720 hours = 30 days, enough for every change.
        let closes = match kraken_get(client, &format!("https://api.kraken.com/0/public/OHLC?pair={pair}&interval=60")).await {
            Ok(r) => kraken_closes(&r),
            Err(e) => {
                tracing::warn!("crypto: kraken OHLC {pair}: {e}");
                Vec::new()
            }
        };
        if let Some(c) = kraken_coin(id, key, name, symbol, t, &closes) {
            coins.push(c);
        }
    }
    Ok(coins)
}

/// Price history from Kraken for `/api/crypto/history`: the candle size that
/// covers `days` within Kraken's 720-candle window.
async fn kraken_history(client: &reqwest::Client, id: &str, days: u32) -> anyhow::Result<serde_json::Value> {
    let pair = KRAKEN_PAIRS.iter().find(|p| p.0 == id).map(|p| p.1)
        .ok_or_else(|| anyhow::anyhow!("no kraken pair for {id}"))?;
    let interval = match days { 1 => 5, 7 => 60, 30 => 240, _ => 1440 };
    let closes = kraken_closes(&kraken_get(client, &format!("https://api.kraken.com/0/public/OHLC?pair={pair}&interval={interval}")).await?);
    let last = closes.last().map(|c| c.0).unwrap_or(0);
    let pts: Vec<serde_json::Value> = closes
        .iter()
        .filter(|(t, _)| last - t <= days as i64 * 86_400)
        .map(|(t, c)| serde_json::json!([t * 1000, c]))
        .collect();
    if pts.is_empty() {
        anyhow::bail!("kraken returned no history");
    }
    Ok(serde_json::json!({ "prices": downsample(&pts, 200), "source": "kraken" }))
}

/// Coins on the crypto screen, in display order: (CoinGecko id, short key).
/// Flux is still listed under its former name, ZelCash.
const CRYPTO_COINS: &[(&str, &str)] =
    &[("bitcoin", "btc"), ("ethereum", "eth"), ("zcash", "zec"), ("zelcash", "flux")];
/// Allowed history ranges, in days.
const CRYPTO_HISTORY_DAYS: &[u32] = &[1, 7, 30, 365];
/// History changes slowly and the uplink is metered: reuse it for 5 minutes.
const CRYPTO_HISTORY_TTL: std::time::Duration = std::time::Duration::from_secs(300);

static CRYPTO_HISTORY_CACHE: std::sync::LazyLock<
    tokio::sync::Mutex<std::collections::HashMap<(String, u32), (std::time::Instant, serde_json::Value)>>,
> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(std::collections::HashMap::new()));

/// Keep at most `max` evenly spaced points (always including the last one).
fn downsample(points: &[serde_json::Value], max: usize) -> Vec<serde_json::Value> {
    if points.len() <= max || max < 2 {
        return points.to_vec();
    }
    let step = (points.len() - 1) as f64 / (max - 1) as f64;
    (0..max).map(|i| points[((i as f64) * step).round() as usize].clone()).collect()
}

/// `GET /api/crypto/history?id=<coingecko id>&days=<1|7|30|365>` →
/// `{"prices": [[unix_ms, eur], ...]}` (≤ 200 points).
async fn api_crypto_history(
    State(s): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let cg_key = coingecko_key(&s).await;
    use axum::http::StatusCode;
    let id = params.get("id").map(String::as_str).unwrap_or("");
    let days: u32 = params.get("days").and_then(|d| d.parse().ok()).unwrap_or(7);
    if !CRYPTO_COINS.iter().any(|(c, _)| *c == id) || !CRYPTO_HISTORY_DAYS.contains(&days) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "unknown coin or range"}))).into_response();
    }
    let key = (id.to_string(), days);
    let mut cache = CRYPTO_HISTORY_CACHE.lock().await;
    if let Some((at, v)) = cache.get(&key) {
        if at.elapsed() < CRYPTO_HISTORY_TTL {
            return Json(v.clone()).into_response();
        }
    }
    let fetched: anyhow::Result<serde_json::Value> = async {
        let client = crypto_client()?;
        if !coingecko_blocked() {
            let url = format!(
                "https://api.coingecko.com/api/v3/coins/{id}/market_chart?vs_currency=eur&days={days}"
            );
            match coingecko_get(&client, &url, cg_key.as_deref()).await {
                Ok(body) => match body["prices"].as_array().filter(|p| !p.is_empty()) {
                    Some(prices) => return Ok(serde_json::json!({ "prices": downsample(prices, 200) })),
                    None => tracing::warn!("crypto history: coingecko returned no prices"),
                },
                Err(e) => tracing::warn!("crypto history: {e}"),
            }
        }
        kraken_history(&client, id, days).await
    }
    .await;
    match fetched {
        Ok(v) => {
            cache.insert(key, (std::time::Instant::now(), v.clone()));
            Json(v).into_response()
        }
        Err(e) => {
            tracing::warn!("crypto history fetch failed: {e}");
            match cache.get(&key) {
                Some((_, v)) => Json(v.clone()).into_response(),
                None => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": "unavailable"}))).into_response(),
            }
        }
    }
}

// --- WebSocket ---

/// Touch echoes from rascar-carplay's injector ("p|d|r x y" UDP datagrams on
/// loopback), forwarded to the dashboard as {"type":"touch"} so it can draw a
/// touch glow — also over embedded sites, whose touches never reach the page.
fn spawn_touch_echo(tx: broadcast::Sender<String>) {
    tokio::spawn(async move {
        let sock = match tokio::net::UdpSocket::bind("127.0.0.1:7702").await {
            Ok(s) => s,
            Err(e) => { warn!("touch echo: cannot listen on 127.0.0.1:7702: {e}"); return; }
        };
        let mut buf = [0u8; 64];
        loop {
            let Ok(n) = sock.recv(&mut buf).await else { continue };
            if let Some(msg) = parse_touch_echo(&buf[..n]) {
                let _ = tx.send(msg);
            }
        }
    });
}

fn parse_touch_echo(datagram: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(datagram).ok()?;
    let mut it = text.split_ascii_whitespace();
    let phase = match it.next()? { "p" => "press", "d" => "drag", "r" => "release", _ => return None };
    let x: u32 = it.next()?.parse().ok()?;
    let y: u32 = it.next()?.parse().ok()?;
    if x > 10_000 || y > 10_000 { return None; }
    Some(serde_json::json!({"type": "touch", "phase": phase, "x": x, "y": y}).to_string())
}

/// One transport action on the MPRIS player (YouTube Music's mpv, or any
/// other). `None` for an action it has no verb for.
async fn mpris_action(media: &MediaService, action: &str) -> Option<bool> {
    Some(match action {
        "play-pause" => media.play_pause().await,
        "play" => media.play().await,
        // A live player has nothing to stop that pause does not do better.
        "pause" | "stop" => media.pause().await,
        "next" => media.next_track().await,
        "previous" => media.previous_track().await,
        _ => return None,
    })
}

/// Where rascar-carplay sends the car's media keys (steering wheel / head-unit
/// buttons, decoded from CarPlay HID reports): "key <name>" datagrams, see
/// carplay/src/stream/carkeys.rs. Next to the screen control (7704).
const CAR_KEY_ADDR: &str = "127.0.0.1:7705";
/// The only key names accepted (carplay `CarKey::wire_name`).
/// `hook` (HID 0x20, toggle) and `drop` (0x26) are the telephony keys;
/// `hangup` is their older single name, kept for compatibility.
const CAR_KEYS: [&str; 9] = ["play", "pause", "play-pause", "next", "previous", "stop", "hangup", "hook", "drop"];

fn parse_car_key(datagram: &[u8]) -> Option<&'static str> {
    let text = std::str::from_utf8(datagram).ok()?;
    let mut it = text.split_ascii_whitespace();
    if it.next()? != "key" {
        return None;
    }
    let name = it.next()?;
    if it.next().is_some() {
        return None;
    }
    CAR_KEYS.iter().copied().find(|k| *k == name)
}

/// Car media keys → the page, which knows which source is playing (radio and
/// TV live in it; YouTube Music is the MPRIS player). With no page connected,
/// the key still reaches the MPRIS player directly.
fn spawn_car_keys(state: AppState) {
    tokio::spawn(async move {
        let sock = match tokio::net::UdpSocket::bind(CAR_KEY_ADDR).await {
            Ok(s) => s,
            Err(e) => { warn!("car keys: cannot listen on {CAR_KEY_ADDR}: {e}"); return; }
        };
        let mut buf = [0u8; 64];
        loop {
            let Ok(n) = sock.recv(&mut buf).await else { continue };
            let Some(key) = parse_car_key(&buf[..n]) else {
                warn!("car keys: ignored datagram {:?}", String::from_utf8_lossy(&buf[..n]));
                continue;
            };
            // A modem call takes the telephony keys first; without one they
            // reach the page as before (it ends a meeting on hook/drop/hangup).
            if state.phone.handle_key(key).await {
                continue;
            }
            if state.broadcast_tx.receiver_count() > 0 {
                let _ = state.broadcast_tx.send(serde_json::json!({"type": "carkey", "key": key}).to_string());
                info!("car key '{key}' → dashboard page");
            } else {
                let ok = mpris_action(&state.media, key).await.unwrap_or(false);
                info!("car key '{key}' → no page connected, MPRIS player (ok={ok})");
            }
        }
    });
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: AppState) {
    let mut rx = state.broadcast_tx.subscribe();
    info!("WebSocket client connected");

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    // A slow client fell behind the ring buffer. Don't drop the
                    // connection — just skip the missed frames and resync on
                    // the next (newest) broadcast, which is a full snapshot.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        info!("WebSocket client lagged, skipped {n} frames");
                        continue;
                    }
                    // Sender gone — nothing more will arrive.
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // Handle client commands (media control)
                        if let Ok(cmd) = serde_json::from_str::<serde_json::Value>(&text) {
                            if cmd["type"] == "media" {
                                // Fire-and-forget over the socket; the next
                                // broadcast reflects whether it took effect.
                                let _ok = match cmd["action"].as_str() {
                                    Some("shuffle") => state.media.toggle_shuffle().await,
                                    Some(a) => mpris_action(&state.media, a).await.unwrap_or(true),
                                    None => true,
                                };
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    info!("WebSocket client disconnected");
}

// --- Broadcast Loop ---

async fn broadcast_loop(state: AppState) {
    loop {
        // Snapshot each source into an owned value and release its lock before
        // touching the next one, so we never hold a read guard across an
        // `.await` (which would serialize/stall unrelated writers).
        let gps = state.gps.current.read().await.clone();
        let gear = state.gps.current_gear().await;
        let obd = state.obd.data.read().await.clone();
        let weather = state.weather.current.read().await.clone();
        let mut media = state.media.now_playing.read().await.clone();
        // Our own mpv: shuffle/repeat live in the queue, not in mpv-mpris.
        let q = state.player.state().await;
        let ours = media.player_name.starts_with("mpv") && q.current.is_some();
        if ours {
            media.shuffle = q.shuffle;
            // mpv's own truth and the queue's clean metadata over mpv-mpris.
            if let Some(pb) = state.player.playback().await {
                media.title = pb.track.title;
                media.artist = pb.track.artist;
                media.album = pb.track.album;
                if !pb.track.art.is_empty() {
                    media.art_url = pb.track.art;
                }
                media.position_sec = pb.position;
                media.length_sec = pb.duration;
                media.is_playing = pb.playing;
            }
        }
        let music_state = serde_json::json!({
            "ours": ours, "shuffle": q.shuffle, "repeat": q.repeat,
            "pos": q.pos, "length": q.length,
            "videoId": q.current.as_ref().map(|t| t.video_id.clone()),
        });
        let signal_dbm = *state.signal.signal_dbm.read().await;
        let cell = state.cell.status.read().await.clone();
        let volume_pct = *state.volume.volume_pct.read().await;
        let trip_recording = *state.trip.is_recording.read().await;
        let live_trip = state.trip.live.read().await.clone();
        let fuel = state.fuel.status.read().await.clone();
        let phone = state.phone.status().await;

        let now = chrono::Local::now();
        let payload = serde_json::json!({
            "gps": {
                "lat": gps.latitude,
                "lon": gps.longitude,
                "speed": (gps.speed_kmh * 10.0).round() / 10.0,
                "alt": (gps.altitude * 10.0).round() / 10.0,
                "bearing": (gps.bearing * 10.0).round() / 10.0,
                "fix": gps.has_fix,
                "accuracy": (gps.accuracy * 10.0).round() / 10.0,
                // Fabricated positions must be labelled as such.
                "mock": gps.is_mock,
                // Car GNSS view ($GPGSV / $GPGGA); null/empty until the car sends it.
                "sats_view": gps.sats_in_view,
                "sats_used": gps.sats_used,
                "sats": gps.satellites,
            },
            "obd": {
                "rpm": obd.rpm.round(),
                "speed": obd.speed.round(),
                "coolant": obd.coolant_temp.round(),
                "battery": (obd.battery_voltage * 10.0).round() / 10.0,
                "load": obd.engine_load.round(),
                "throttle": obd.throttle.round(),
                "connected": obd.is_connected,
                // True when the values above are the last known ones rather
                // than live readings (adapter answering, bus dead).
                "stale": obd.stale,
                "fuel": obd.fuel_level.round(),
                "intake": obd.intake_temp.round(),
                "boost": (obd.boost_pressure * 10.0).round() / 10.0,
                "oil": obd.oil_temp.round(),
                "dtc": obd.dtc_count,
                // Every other decoded value (supported PIDs), for the metric
                // list: {k: key, l: label, u: unit, d: decimals, v: value}.
                "metrics": services::obd::metrics_view(&obd),
                // Readiness monitors, misfires per cylinder, monitor tests out
                // of limits, in-use performance (the full mode 06 list is in
                // /api/obd).
                "health": obd.health.broadcast_view(),
            },
            "weather": weather,
            "media": media,
            "music": music_state,
            "signal": signal_dbm,
            "cell": cell,
            // A CarPlay session is streaming (the "Car" button can act).
            "carplay": carplay_session_live(),
            "volume": volume_pct,
            // The car's gear lever ("P"/"R"/"N"/"D"), null when unknown or stale.
            "gear": gear.map(String::from),
            "trip_recording": trip_recording,
            // The open trip's distance, fuel and cost (null between trips).
            "trip": live_trip,
            "fuel": fuel,
            "phone": phone,
            "time": now.format("%H:%M").to_string(),
        });

        let _ = state.broadcast_tx.send(payload.to_string());

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod frontend_path_tests {
    use super::*;

    #[test]
    fn env_var_wins_over_every_other_location() {
        let c = frontend_candidates(
            Some("/opt/custom/frontend".into()),
            Some(PathBuf::from("/opt/orangecar/app")),
            Some(PathBuf::from("/opt/orangecar/target/release/rascar-app")),
        );
        assert_eq!(c[0], PathBuf::from("/opt/custom/frontend"));
    }

    #[test]
    fn deployed_layout_is_covered_before_the_compile_time_path() {
        let c = frontend_candidates(
            None,
            Some(PathBuf::from("/opt/orangecar/app")),
            Some(PathBuf::from("/opt/orangecar/target/release/rascar-app")),
        );
        let compile_time = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("frontend");
        let cwd_pos = c.iter().position(|p| p == &PathBuf::from("/opt/orangecar/app/frontend"));
        let compile_pos = c.iter().position(|p| p == &compile_time);
        assert!(cwd_pos.is_some(), "cwd/frontend must be a candidate: {c:?}");
        assert!(compile_pos.is_some(), "compile-time path must remain a candidate");
        assert!(cwd_pos < compile_pos, "the runtime location must be tried first: {c:?}");
    }

    #[test]
    fn picks_the_first_candidate_that_holds_an_index_html() {
        let c = vec![
            PathBuf::from("/nope"),
            PathBuf::from("/yes"),
            PathBuf::from("/also-yes"),
        ];
        let picked = pick_frontend_dir(&c, |d| d != Path::new("/nope"));
        assert_eq!(picked, Some(PathBuf::from("/yes")));
    }

    #[test]
    fn none_when_no_candidate_holds_the_dashboard() {
        let c = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        assert_eq!(pick_frontend_dir(&c, |_| false), None);
    }
}

#[cfg(test)]
mod tv_hls_tests {
    use super::*;

    #[test]
    fn rewrites_relative_and_absolute_uris_through_the_proxy() {
        let base = url::Url::parse("https://cdn.example.com/live/master.m3u8").unwrap();
        let pl = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nlow/index.m3u8\n#EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example.net/k1\"\nseg1.ts\n";
        let out = rewrite_hls_playlist(pl, &base);
        assert!(out.contains(&format!("/api/tv/hls?url={}", urlencoding::encode("https://cdn.example.com/live/low/index.m3u8"))));
        assert!(out.contains(&format!("URI=\"/api/tv/hls?url={}\"", urlencoding::encode("https://keys.example.net/k1"))));
        assert!(out.contains(&format!("/api/tv/hls?url={}", urlencoding::encode("https://cdn.example.com/live/seg1.ts"))));
        assert!(out.starts_with("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n"));
        // Hosts referenced by the playlist are learned.
        assert!(TV_EXTRA_HOSTS.lock().unwrap().contains("keys.example.net"));
    }
}

#[cfg(test)]
mod touch_echo_tests {
    use super::parse_touch_echo;

    #[test]
    fn parses_echoes_and_rejects_junk() {
        assert_eq!(parse_touch_echo(b"p 120 40").unwrap(), r#"{"phase":"press","type":"touch","x":120,"y":40}"#);
        assert!(parse_touch_echo(b"r 1 2").unwrap().contains("release"));
        assert!(parse_touch_echo(b"x 1 2").is_none());
        assert!(parse_touch_echo(b"p 1").is_none());
        assert!(parse_touch_echo(b"p 99999 2").is_none());
        assert!(parse_touch_echo(&[0xff, 0xfe]).is_none());
    }
}

#[cfg(test)]
mod car_key_tests {
    use super::parse_car_key;

    #[test]
    fn accepts_the_carplay_datagrams_and_nothing_else() {
        // Exactly what carplay/src/stream/carkeys.rs sends (its test pins the same list).
        for k in ["play", "pause", "play-pause", "next", "previous", "stop", "hangup", "hook", "drop"] {
            assert_eq!(parse_car_key(format!("key {k}").as_bytes()), Some(k));
        }
        assert_eq!(parse_car_key(b"key next\n"), Some("next"));
        assert_eq!(parse_car_key(b"key volume-up"), None);
        assert_eq!(parse_car_key(b"key next extra"), None);
        assert_eq!(parse_car_key(b"next"), None);
        assert_eq!(parse_car_key(b"p 1 2"), None);
        assert_eq!(parse_car_key(&[0xff, 0xfe]), None);
    }
}

#[cfg(test)]
mod carplay_screen_tests {
    use super::udp_port_bound;

    #[test]
    fn finds_the_loopback_or_wildcard_bind_of_the_port() {
        let head = "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n";
        let lo = format!("{head}  12: 0100007F:1E18 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 1 2 0 0\n");
        assert!(udp_port_bound(&lo, 7704));
        let any = format!("{head}  12: 00000000:1E18 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 1 2 0 0\n");
        assert!(udp_port_bound(&any, 7704));
        let other = format!("{head}  12: 0100007F:1E17 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 1 2 0 0\n");
        assert!(!udp_port_bound(&other, 7704));
        // A remote peer on port 7704 is not our socket.
        let remote = format!("{head}  12: 0100007F:D431 0100007F:1E18 01 00000000:00000000 00:00000000 00000000  1000        0 1 2 0 0\n");
        assert!(!udp_port_bound(&remote, 7704));
        assert!(!udp_port_bound(head, 7704));
    }
}

#[cfg(test)]
mod call_tests {
    use super::is_dialable;

    #[test]
    fn dialable_numbers() {
        assert!(is_dialable("+44 7700 900123"));
        assert!(is_dialable("*100#"));
        assert!(!is_dialable(""));
        assert!(!is_dialable("+#*"));
        assert!(!is_dialable("tel:123"));
        assert!(!is_dialable(&"1".repeat(33)));
    }
}

#[cfg(test)]
mod crypto_tests {
    use super::*;

    #[test]
    fn downsample_keeps_ends_and_caps_the_count() {
        let pts: Vec<serde_json::Value> = (0..721).map(|i| serde_json::json!(i)).collect();
        let d = downsample(&pts, 200);
        assert_eq!(d.len(), 200);
        assert_eq!(d.first(), Some(&serde_json::json!(0)));
        assert_eq!(d.last(), Some(&serde_json::json!(720)));
        assert_eq!(downsample(&pts[..10], 200).len(), 10);
    }

    #[test]
    fn kraken_ohlc_closes_and_changes_go_by_timestamp() {
        let r = serde_json::json!({"FLUXEUR": [
            [1000, "1", "1", "1", "0.050", "1", "1", 1],
            [1000 + 3600, "1", "1", "1", "0.055", "1", "1", 1],
            // a gap: no trades for 22 hours
            [1000 + 23 * 3600, "1", "1", "1", "0.060", "1", "1", 1],
            [1000 + 24 * 3600, "1", "1", "1", "0.066", "1", "1", 1]
        ], "last": 123});
        let c = kraken_closes(&r);
        assert_eq!(c.len(), 4);
        assert_eq!(c[0], (1000, 0.050));
        // 24h before the last candle is exactly the first one.
        let ch = change_since(&c, 0.066, 86_400).unwrap();
        assert!((ch - 32.0).abs() < 1e-9, "{ch}");
        // 1h back is the candle an hour earlier.
        assert!((change_since(&c, 0.066, 3_600).unwrap() - 10.0).abs() < 1e-9);
        // Older than the data: unknown, not 0.
        assert_eq!(change_since(&c, 0.066, 30 * 86_400), None);
    }

    #[test]
    fn kraken_coin_keeps_the_coingecko_shape_with_nulls() {
        let t = serde_json::json!({"c": ["74213.9", "0.001"], "v": ["10", "20"], "p": ["74000", "74100"],
            "h": ["75000", "75500"], "l": ["73000", "72500"]});
        let c = kraken_coin("bitcoin", "btc", "Bitcoin", "BTC", &t, &[]).unwrap();
        assert_eq!(c["price"], 74213.9);
        assert_eq!(c["high_24h"], 75500.0);
        assert_eq!(c["low_24h"], 72500.0);
        assert_eq!(c["volume_24h"], 20.0 * 74100.0);
        for k in ["rank", "market_cap", "ath", "circulating_supply", "change_24h"] {
            assert!(c[k].is_null(), "{k} must be null");
        }
        assert!(kraken_coin("bitcoin", "btc", "Bitcoin", "BTC", &serde_json::json!({}), &[]).is_none());
    }

    #[test]
    fn body_snippet_collapses_and_truncates() {
        let html = "<!DOCTYPE html>\n<html>\n  <head><title>403 Forbidden</title>";
        assert_eq!(body_snippet(html, 22), "<!DOCTYPE html> <html>");
        assert_eq!(body_snippet(&"x".repeat(500), 120).len(), 120);
    }

    #[test]
    fn every_coin_has_a_kraken_pair() {
        for (id, _) in CRYPTO_COINS {
            assert!(KRAKEN_PAIRS.iter().any(|p| p.0 == *id), "{id}");
        }
    }

    #[test]
    fn flux_is_requested_under_its_coingecko_id() {
        assert!(CRYPTO_COINS.contains(&("zelcash", "flux")));
        assert!(CRYPTO_COINS.contains(&("zcash", "zec")));
    }
}
