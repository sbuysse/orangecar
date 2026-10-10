//! rascar-aa — Android Auto head unit service.
//!
//! Waits for an Android phone on a USB host port, switches it to accessory
//! mode, runs the AA head-unit protocol (`rascar_headunit::aa::session`), and
//! serves the phone's screen to the dashboard page `aa.html` over a
//! WebSocket (H.264 passthrough, decoded in the page with WebCodecs). Touches
//! on the page come back over the same socket. Audio is played with `pacat`
//! on the default PulseAudio sink, which the CarPlay session already sends to
//! the car.
//!
//! `--replay file.h264` serves a recorded Annex-B stream instead of a phone,
//! to test the page end to end without one; touches are then echoed back to
//! the page as the TouchEvent the phone would have received.

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use rascar_headunit::aa::browser::{encode_video, nal_types, GopCache, PageMsg, Status, VideoMsg};
use rascar_headunit::aa::config::{HuConfig, Resolution};
use rascar_headunit::aa::session::{audio_format, Event, Phase, Session};
use rascar_headunit::aa::tls::{HuIdentity, HuTls, DEFAULT_CERT_DIRS};
use rascar_headunit::aa::touch::{TouchKind, TouchTracker};
use rascar_headunit::aa::usb_host::{self, AccessoryLink, Found, Prober, DEFAULT_SKIP_VIDS};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, info, warn};

#[derive(Parser, Debug)]
#[command(name = "rascar-aa", about = "Android Auto head unit → dashboard page")]
struct Cli {
    /// WebSocket listen address for aa.html.
    #[arg(long, default_value = "127.0.0.1:8091")]
    ws: SocketAddr,
    /// Directory holding headunit.crt + headunit.key (repeatable; defaults:
    /// /etc/rascar/aa, /etc/openauto, /usr/share/aasdk/cert).
    #[arg(long)]
    cert_dir: Vec<PathBuf>,
    /// Video resolution requested from the phone: 800x480, 1280x720, 1920x1080.
    #[arg(long, default_value = "1280x720")]
    resolution: String,
    /// Size (CSS px) of the page area showing the video; sets the margins.
    #[arg(long, default_value = "1108x460")]
    panel: String,
    /// UI density for the phone (lower = more content).
    #[arg(long, default_value_t = 160)]
    density: u32,
    /// Ask for 60 fps instead of 30.
    #[arg(long)]
    fps60: bool,
    /// Do not send VideoFocus(PROJECTED) right after the video setup.
    #[arg(long)]
    no_focus_after_setup: bool,
    /// Extra USB vendor ids (hex) never to probe for AOA.
    #[arg(long, value_parser = parse_hex_u16)]
    usb_skip: Vec<u16>,
    /// Do not play audio.
    #[arg(long)]
    no_audio: bool,
    /// Serve this Annex-B H.264 file in a loop instead of a phone.
    #[arg(long)]
    replay: Option<PathBuf>,
    #[arg(long, default_value_t = 30)]
    replay_fps: u32,
    #[arg(long)]
    debug: bool,
}

fn parse_hex_u16(s: &str) -> Result<u16, String> {
    u16::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s.split_once('x').ok_or_else(|| anyhow!("expected WxH, got {s:?}"))?;
    Ok((w.parse()?, h.parse()?))
}

fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

enum Cmd {
    Touch { kind: TouchKind, id: u32, x: f64, y: f64 },
    TouchCancel,
    Keyframe,
}

/// State shared between the phone side and the page connections.
struct Hub {
    video_tx: broadcast::Sender<Arc<VideoMsg>>,
    /// Debug text for the pages (replay-mode touch echo).
    text_tx: broadcast::Sender<String>,
    gop: Mutex<GopCache>,
    seq: AtomicU64,
    status: watch::Sender<Status>,
    cmd_tx: mpsc::Sender<Cmd>,
}

impl Hub {
    fn publish_video(&self, config: bool, ts: u64, data: &[u8]) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let m = Arc::new(encode_video(seq, config, ts, data));
        // Cache first, then broadcast: a page that snapshots in between sees
        // this message in the snapshot and skips it live (seq filter).
        self.gop.lock().expect("gop lock").push(m.clone());
        let _ = self.video_tx.send(m);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(if cli.debug { "debug" } else { "info" })
        .with_target(false)
        .init();

    let cfg = HuConfig {
        resolution: Resolution::parse(&cli.resolution).ok_or_else(|| anyhow!("unsupported resolution {}", cli.resolution))?,
        panel: parse_size(&cli.panel)?,
        density: cli.density,
        fps60: cli.fps60,
        video_focus_after_setup: !cli.no_focus_after_setup,
        ..HuConfig::default()
    };
    info!(
        "rascar-aa: {:?}, panel {:?} → margins {:?}, content {:?}",
        cfg.resolution,
        cfg.panel,
        cfg.margins(),
        cfg.content_rect()
    );

    let (video_tx, _) = broadcast::channel(512);
    let (text_tx, _) = broadcast::channel(64);
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let (status, _) = watch::channel(Status::new(&cfg));
    let hub = Arc::new(Hub {
        video_tx,
        text_tx,
        // ~5 s of catch-up at 30 fps at most; longer GOPs → keyframe request.
        gop: Mutex::new(GopCache::new(16 << 20, 150)),
        seq: AtomicU64::new(0),
        status,
        cmd_tx,
    });

    let listener = TcpListener::bind(cli.ws).await.with_context(|| format!("binding {}", cli.ws))?;
    info!("rascar-aa: page WebSocket on ws://{}", cli.ws);
    tokio::spawn(serve_pages(listener, hub.clone()));

    if let Some(file) = cli.replay.clone() {
        replay(file, cli.replay_fps, cfg, hub, cmd_rx).await
    } else {
        let dirs: Vec<PathBuf> = if cli.cert_dir.is_empty() {
            DEFAULT_CERT_DIRS.iter().map(PathBuf::from).collect()
        } else {
            cli.cert_dir.clone()
        };
        let identity = HuIdentity::discover(&dirs)?;
        info!("rascar-aa: head-unit certificate from {}", identity.source.display());
        let mut skip = DEFAULT_SKIP_VIDS.to_vec();
        skip.extend(&cli.usb_skip);
        phone_loop(cfg, identity, skip, !cli.no_audio, hub, cmd_rx).await
    }
}

// ── Pages ────────────────────────────────────────────────────────────────

async fn serve_pages(listener: TcpListener, hub: Arc<Hub>) {
    loop {
        match listener.accept().await {
            Ok((tcp, peer)) => {
                let hub = hub.clone();
                tokio::spawn(async move {
                    if let Err(e) = page_conn(tcp, hub).await {
                        debug!("rascar-aa: page {peer} ended: {e:#}");
                    }
                });
            }
            Err(e) => {
                warn!("rascar-aa: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn page_conn(tcp: TcpStream, hub: Arc<Hub>) -> Result<()> {
    tcp.set_nodelay(true)?;
    let ws = tokio_tungstenite::accept_async(tcp).await?;
    let (mut tx, mut rx) = ws.split();
    // Subscribe BEFORE the snapshot so nothing falls between the two.
    let mut video = hub.video_tx.subscribe();
    let mut text = hub.text_tx.subscribe();
    let mut status = hub.status.subscribe();
    info!("rascar-aa: page connected");

    let first = serde_json::to_string(&*status.borrow_and_update())?;
    tx.send(WsMessage::Text(first.into())).await?;
    let mut last_seq;
    let mut need_key = false;
    {
        let (snap, seq) = hub.gop.lock().expect("gop lock").snapshot();
        last_seq = seq;
        match snap {
            Some(msgs) => {
                for m in msgs {
                    tx.send(WsMessage::Binary(m.bytes.clone())).await?;
                }
            }
            None => need_key = true,
        }
    }
    let streaming = status.borrow().streaming;
    if need_key && streaming {
        let _ = hub.cmd_tx.try_send(Cmd::Keyframe);
    }

    let result: Result<()> = async {
        loop {
            tokio::select! {
                r = video.recv() => match r {
                    Ok(m) => {
                        if m.seq <= last_seq { continue; }
                        last_seq = m.seq;
                        if need_key && !m.config {
                            if !m.key { continue; }
                            need_key = false;
                        }
                        tx.send(WsMessage::Binary(m.bytes.clone())).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // The page fell behind: restart it from the GOP cache
                        // (or the next keyframe) — never skip P-frames.
                        warn!("rascar-aa: page lagged {n} frames, resyncing");
                        let (snap, seq) = hub.gop.lock().expect("gop lock").snapshot();
                        last_seq = seq;
                        match snap {
                            Some(msgs) => for m in msgs { tx.send(WsMessage::Binary(m.bytes.clone())).await?; },
                            None => { need_key = true; let _ = hub.cmd_tx.try_send(Cmd::Keyframe); }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                Ok(t) = text.recv() => tx.send(WsMessage::Text(t.into())).await?,
                Ok(()) = status.changed() => {
                    let s = serde_json::to_string(&*status.borrow_and_update())?;
                    tx.send(WsMessage::Text(s.into())).await?;
                }
                m = rx.next() => match m {
                    None => return Ok(()),
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(WsMessage::Text(t))) => match serde_json::from_str::<PageMsg>(&t) {
                        Ok(PageMsg::Touch { action, id, x, y }) => {
                            let kind = match action.as_str() {
                                "down" => Some(TouchKind::Down),
                                "move" => Some(TouchKind::Move),
                                "up" | "cancel" => Some(TouchKind::Up),
                                _ => None,
                            };
                            if let Some(kind) = kind {
                                // Touch must not be lost: wait for room rather than drop.
                                let _ = hub.cmd_tx.send(Cmd::Touch { kind, id, x, y }).await;
                            }
                        }
                        Ok(PageMsg::Keyframe) => {
                            need_key = true;
                            let _ = hub.cmd_tx.try_send(Cmd::Keyframe);
                        }
                        Err(e) => debug!("rascar-aa: bad page message {t:?}: {e}"),
                    },
                    Some(Ok(WsMessage::Close(_))) => return Ok(()),
                    Some(Ok(_)) => {}
                },
            }
        }
    }
    .await;
    // A page that vanishes mid-gesture must not leave a finger down on the phone.
    let _ = hub.cmd_tx.try_send(Cmd::TouchCancel);
    info!("rascar-aa: page disconnected");
    result
}

// ── Phone ────────────────────────────────────────────────────────────────

async fn phone_loop(
    cfg: HuConfig,
    identity: HuIdentity,
    skip: Vec<u16>,
    audio: bool,
    hub: Arc<Hub>,
    mut cmd_rx: mpsc::Receiver<Cmd>,
) -> Result<()> {
    let identity = Arc::new(identity);
    let prober = Arc::new(Mutex::new(Prober::new(skip)));
    // A phone still in accessory mode from a previous rascar-aa run keeps its
    // old session and ignores our VersionRequest (2026-10-05: every restart
    // with the phone plugged in stayed blank). Reset it once at start so it
    // re-enumerates and gets a fresh AOA switch.
    let mut first_scan = true;
    loop {
        hub.status.send_modify(|s| {
            s.state = "waiting".into();
            s.streaming = false;
            s.phone = None;
        });
        let p = prober.clone();
        let found = tokio::task::spawn_blocking(move || p.lock().expect("prober lock").scan()).await?;
        match found {
            Ok(Found::Accessory(dev)) if first_scan => {
                first_scan = false;
                info!("rascar-aa: phone already in accessory mode at start — resetting it for a fresh session");
                let reset = tokio::task::spawn_blocking(move || dev.open().and_then(|d| d.reset())).await?;
                if let Err(e) = reset {
                    warn!("rascar-aa: USB reset failed ({e}); trying the stale session anyway");
                } else {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
            Ok(Found::Accessory(dev)) => {
                first_scan = false;
                if let Err(e) = run_phone(&cfg, &identity, &dev, audio, &hub, &mut cmd_rx).await {
                    warn!("rascar-aa: phone session ended: {e:#}");
                    hub.status.send_modify(|s| s.error = Some(format!("{e:#}")));
                }
                hub.gop.lock().expect("gop lock").reset();
                // Let the phone drop off the bus before rescanning.
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Ok(Found::Switching) => {
                first_scan = false;
                tokio::time::sleep(Duration::from_millis(1500)).await
            }
            Ok(Found::Nothing) => {
                first_scan = false;
                // Drain page commands while idle so the queue never fills.
                while cmd_rx.try_recv().is_ok() {}
                tokio::time::sleep(Duration::from_secs(1)).await
            }
            Err(e) => {
                warn!("rascar-aa: USB scan failed: {e:#}");
                tokio::time::sleep(Duration::from_secs(3)).await
            }
        }
    }
}

async fn run_phone(
    cfg: &HuConfig,
    identity: &HuIdentity,
    dev: &nusb::DeviceInfo,
    audio_on: bool,
    hub: &Arc<Hub>,
    cmd_rx: &mut mpsc::Receiver<Cmd>,
) -> Result<()> {
    let link = Arc::new(AccessoryLink::open(dev)?);
    hub.status.send_modify(|s| {
        s.phone = Some(link.name.clone());
        s.error = None;
    });
    let mut session = Session::new(cfg.clone(), Box::new(HuTls::new(identity)?));
    let mut audio = Audio::new(audio_on);

    // Writes go through one task so they stay in order and never block reads.
    let (wtx, mut wrx) = mpsc::channel::<Vec<u8>>(256);
    let wlink = link.clone();
    let writer = tokio::spawn(async move {
        while let Some(b) = wrx.recv().await {
            wlink.write(b).await?;
        }
        Ok::<(), anyhow::Error>(())
    });

    let mut reader = link.reader();
    let mut last_keyframe_req = Instant::now() - Duration::from_secs(10);
    let events = session.start()?;
    let mut result = apply(events, hub, &wtx, &mut audio).await;
    while result.is_ok() {
        let step: Result<Vec<Event>> = tokio::select! {
            r = usb_host::read(&mut reader) => match r {
                Ok(data) => session.on_bytes(&data),
                Err(e) => Err(e),
            },
            Some(cmd) = cmd_rx.recv() => match cmd {
                Cmd::Touch { kind, id, x, y } => session.touch(kind, id, x, y, now_us()),
                Cmd::TouchCancel => session.touch_cancel(now_us()),
                Cmd::Keyframe => {
                    // NATIVE then PROJECTED restarts the phone's encoder (fresh
                    // SPS/PPS + IDR). Rate-limited: a page asks once per join.
                    if last_keyframe_req.elapsed() >= Duration::from_secs(2) {
                        last_keyframe_req = Instant::now();
                        info!("rascar-aa: asking the phone for a keyframe (focus toggle)");
                        let mut e = session.set_video_focus(false)?;
                        e.extend(session.set_video_focus(true)?);
                        Ok(e)
                    } else {
                        Ok(Vec::new())
                    }
                }
            },
        };
        result = match step {
            Ok(events) => apply(events, hub, &wtx, &mut audio).await,
            Err(e) => Err(e),
        };
        if session.phase() == Phase::Closed {
            break;
        }
    }
    drop(wtx);
    writer.abort();
    result.map(|_| ())
}

/// Carry out the session's events. Returns Err when the session closed.
async fn apply(events: Vec<Event>, hub: &Hub, wtx: &mpsc::Sender<Vec<u8>>, audio: &mut Audio) -> Result<()> {
    for ev in events {
        match ev {
            Event::Write(b) => wtx.send(b).await.map_err(|_| anyhow!("USB writer stopped"))?,
            Event::Phase(p) => hub.status.send_modify(|s| *s = s.clone().with_phase(p)),
            Event::Phone { device_name, label } => {
                info!("rascar-aa: phone {device_name:?} ({label:?})");
                let name = if device_name.is_empty() { label } else { device_name };
                hub.status.send_modify(|s| s.phone = Some(name));
            }
            Event::Video { config, timestamp_us, data } => hub.publish_video(config, timestamp_us, &data),
            Event::VideoStart => hub.status.send_modify(|s| s.streaming = true),
            Event::VideoStop => hub.status.send_modify(|s| s.streaming = false),
            Event::AudioStart { channel, rate, channels } => audio.start(channel, rate, channels),
            Event::Audio { channel, pcm } => audio.play(channel, pcm),
            Event::AudioStop { channel } => audio.stop(channel),
            Event::PhoneWantsNative => info!("rascar-aa: phone asked to leave the projection (kept projected)"),
            Event::Closed { reason } => return Err(anyhow!("session closed: {reason}")),
        }
    }
    Ok(())
}

// ── Audio (pacat → default PulseAudio sink → CarPlay → car) ───────────────

struct Audio {
    enabled: bool,
    sinks: HashMap<u8, mpsc::Sender<Vec<u8>>>,
    dropped: u64,
}

impl Audio {
    fn new(enabled: bool) -> Self {
        Self { enabled, sinks: HashMap::new(), dropped: 0 }
    }

    fn start(&mut self, channel: u8, rate: u32, channels: u32) {
        if !self.enabled || self.sinks.contains_key(&channel) {
            return;
        }
        let name = match channel {
            4 => "Android Auto media",
            5 => "Android Auto guidance",
            _ => "Android Auto system",
        };
        let child = tokio::process::Command::new("pacat")
            .args([
                "--playback",
                "--raw",
                "--format=s16le",
                &format!("--rate={rate}"),
                &format!("--channels={channels}"),
                "--client-name=rascar-aa",
                &format!("--stream-name={name}"),
                "--latency-msec=80",
            ])
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                warn!("rascar-aa: cannot start pacat for {name}: {e}");
                return;
            }
        };
        let Some(mut stdin) = child.stdin.take() else { return };
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        tokio::spawn(async move {
            let _child = child; // killed when this task ends
            while let Some(pcm) = rx.recv().await {
                if stdin.write_all(&pcm).await.is_err() {
                    break;
                }
            }
        });
        info!("rascar-aa: audio {name}: {rate} Hz × {channels}");
        self.sinks.insert(channel, tx);
    }

    fn play(&mut self, channel: u8, pcm: Vec<u8>) {
        if !self.sinks.contains_key(&channel) {
            if let Some((rate, ch, _)) = audio_format(channel) {
                self.start(channel, rate, ch);
            }
        }
        if let Some(tx) = self.sinks.get(&channel) {
            // Audio tolerates a gap; the protocol must never stall on it.
            if tx.try_send(pcm).is_err() {
                self.dropped += 1;
                if self.dropped == 1 || self.dropped % 100 == 0 {
                    warn!("rascar-aa: audio dropped {} chunk(s)", self.dropped);
                }
            }
        }
    }

    fn stop(&mut self, _channel: u8) {
        // Keep pacat running: restarting it costs ~100 ms and AA stops/starts
        // the guidance stream around every prompt.
    }
}

// ── Replay (no phone) ────────────────────────────────────────────────────

/// Split an Annex-B stream into access units (H.264 7.4.1.2.3, simplified):
/// a new picture starts at a slice whose first_mb_in_slice is 0 (first bit of
/// the slice header set, ue(v) = 0) or at an SPS/PPS/SEI/AUD that follows a
/// slice. Encoders often cut a picture into several slices (x264's
/// zerolatency: one per thread), so "one slice = one picture" is wrong.
fn access_units(stream: &[u8]) -> Vec<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            let s = if i > 0 && stream[i - 1] == 0 { i - 1 } else { i };
            starts.push((s, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut aus = Vec::new();
    let mut cur = Vec::new();
    let mut cur_has_vcl = false;
    for (k, &(s, h)) in starts.iter().enumerate() {
        let e = starts.get(k + 1).map(|x| x.0).unwrap_or(stream.len());
        let Some(&header) = stream.get(h) else { continue };
        let t = header & 0x1F;
        let vcl = t == 1 || t == 5;
        let starts_picture = if vcl {
            stream.get(h + 1).is_some_and(|b| b & 0x80 != 0)
        } else {
            matches!(t, 6..=9)
        };
        if cur_has_vcl && starts_picture {
            aus.push(std::mem::take(&mut cur));
            cur_has_vcl = false;
        }
        cur.extend_from_slice(&stream[s..e]);
        cur_has_vcl |= vcl;
    }
    if !cur.is_empty() {
        aus.push(cur);
    }
    aus
}

async fn replay(file: PathBuf, fps: u32, cfg: HuConfig, hub: Arc<Hub>, mut cmd_rx: mpsc::Receiver<Cmd>) -> Result<()> {
    let stream = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
    let aus = access_units(&stream);
    if aus.is_empty() {
        return Err(anyhow!("{} holds no H.264 access unit", file.display()));
    }
    info!("rascar-aa: replaying {} ({} access units at {fps} fps)", file.display(), aus.len());
    hub.status.send_modify(|s| {
        s.state = "running".into();
        s.phone = Some(format!("replay: {}", file.file_name().and_then(|n| n.to_str()).unwrap_or("?")));
        s.streaming = true;
    });
    let mut tracker = TouchTracker::new();
    let mut tick = tokio::time::interval(Duration::from_micros(1_000_000 / fps.max(1) as u64));
    let mut idx = 0usize;
    let t0 = Instant::now();
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let au = &aus[idx];
                // Like the phone: parameter sets go out as a codec-config
                // message, the picture as data.
                let types = nal_types(au);
                if types.contains(&5) {
                    let split = au.windows(4).position(|w| w == [0, 0, 1, 0x65] || w == [0, 0, 1, 0x25] || w == [0, 0, 1, 0x45])
                        .map(|p| if p > 0 && au[p - 1] == 0 { p - 1 } else { p })
                        .unwrap_or(0);
                    if split > 0 {
                        hub.publish_video(true, 0, &au[..split]);
                    }
                    hub.publish_video(false, t0.elapsed().as_micros() as u64, &au[split..]);
                } else {
                    hub.publish_video(false, t0.elapsed().as_micros() as u64, au);
                }
                idx = (idx + 1) % aus.len();
            }
            Some(cmd) = cmd_rx.recv() => {
                let echo = match cmd {
                    Cmd::Touch { kind, id, x, y } => {
                        let (px, py) = cfg.touch_point(x, y);
                        tracker.event(kind, id, px, py).map(|ev| vec![ev])
                    }
                    Cmd::TouchCancel => Some(tracker.cancel_all()),
                    Cmd::Keyframe => { info!("rascar-aa: page asked for a keyframe"); None }
                };
                for ev in echo.unwrap_or_default() {
                    let pointers: Vec<_> = ev.pointer_data.iter().map(|p| serde_json::json!([p.pointer_id, p.x, p.y])).collect();
                    let j = serde_json::json!({"type": "touch-echo", "action": ev.action, "index": ev.action_index, "pointers": pointers});
                    info!("rascar-aa: touch → {j}");
                    let _ = hub.text_tx.send(j.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_units_group_parameter_sets_with_the_next_slice() {
        let s = [
            0, 0, 0, 1, 0x67, 1, // SPS
            0, 0, 0, 1, 0x68, 2, // PPS
            0, 0, 1, 0x65, 0x88, 3, // IDR slice, first_mb_in_slice = 0
            0, 0, 1, 0x65, 0x40, 3, // IDR slice 2 of the same picture
            0, 0, 0, 1, 0x41, 0x9A, // P, first_mb 0
            0, 0, 0, 1, 0x09, 0x10, // AUD
            0, 0, 0, 1, 0x41, 0x9A, // P
            0, 0, 0, 1, 0x41, 0x20, // second slice of that P
        ];
        let aus = access_units(&s);
        assert_eq!(aus.len(), 3);
        assert_eq!(nal_types(&aus[0]), vec![7, 8, 5, 5]);
        assert_eq!(nal_types(&aus[1]), vec![1]);
        assert_eq!(nal_types(&aus[2]), vec![9, 1, 1]);
    }
}
