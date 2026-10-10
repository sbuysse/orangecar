//! RasCar Android Auto Bridge — Main Orchestrator.

mod media;
mod protocol;
mod usb;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use media::audio::{AudioCapture, AudioPlayer};
use media::encode_media_frame;
use media::input::{parse_touch_event, InputInjector};
use media::video::{VideoEncoder, VideoSettings};
use protocol::channels::{
    ChannelManager, MSG_AUDIO_FOCUS_RESPONSE, MSG_INPUT_BINDING_REQUEST,
    MSG_INPUT_BINDING_RESPONSE, MSG_INPUT_EVENT_INDICATION, MSG_MEDIA_DATA, MSG_MEDIA_DATA_ACK,
    MSG_MEDIA_SETUP_REQUEST, MSG_MEDIA_SETUP_RESPONSE, MSG_MIC_REQUEST, MSG_START_REQUEST,
    MSG_STOP_REQUEST, MSG_VIDEO_FOCUS,
};
use protocol::control::{self, channel, MsgType};
use protocol::crypto::{AATlsContext, AATlsSession};
use protocol::transport::{decode_frame, fragment_message, DecodeError, MAX_MESSAGE_SIZE};
use usb::accessory::AccessoryConnection;
use usb::{ffs, gadget};

/// Upper bound on writing one video message to the bulk IN endpoint. Exceeding it
/// means the host has stopped draining the endpoint, which is fatal to the session.
const VIDEO_WRITE_BUDGET: Duration = Duration::from_millis(800);
/// Same, for audio. Tighter because audio chunks are small and latency-sensitive.
const AUDIO_WRITE_BUDGET: Duration = Duration::from_millis(200);

/// How many times to retry `open_endpoints()` before falling back to waiting for a
/// physical host reconnect.
const ENDPOINT_OPEN_RETRIES: u32 = 15;
const ENDPOINT_OPEN_RETRY_DELAY: Duration = Duration::from_millis(200);

/// A media subprocess task that has terminated, reported to the protocol loop.
/// Without this the loop keeps running against a dead pipeline: the head unit
/// shows a frozen frame forever and the session never ends, so the reconnect loop
/// never gets a chance to rebuild it.
#[derive(Debug, Clone, Copy)]
enum MediaExit {
    VideoEncoder,
    AudioCapture(u8),
}

#[derive(Parser)]
#[command(name = "rascar-bridge", about = "RasCar Android Auto Bridge")]
struct Cli {
    /// Video width
    #[arg(long, default_value = "800")]
    width: u32,
    /// Video height
    #[arg(long, default_value = "480")]
    height: u32,
    /// Video FPS
    #[arg(long, default_value = "30")]
    fps: u32,
    /// X11 display or "fb" for framebuffer
    #[arg(long, default_value = ":0")]
    display: String,
    /// Enable debug logging
    #[arg(long)]
    debug: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls ring crypto provider");

    let filter = if cli.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    // Shutdown signalling. Without it the reconnect loop below never terminates,
    // the teardown at the end of main is unreachable, and a Ctrl-C / systemctl stop
    // leaves the gadget bound with FunctionFS still mounted — after which the next
    // start stacks a second FunctionFS on the same mount point.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    spawn_signal_handler(shutdown_tx);
    let mut shutdown_rx = shutdown_rx;

    // Phase 1: USB gadget + FunctionFS setup
    info!("=== Phase 1: USB Gadget Setup ===");
    gadget::create().context("gadget setup failed — ensure rk3588-dwc3-peripheral overlay and run as root")?;

    // Phase 2: Write FFS descriptors (must happen before bind)
    info!("=== Phase 2: FunctionFS Init ===");
    let ep0_path = format!("{}/ep0", ffs::FFS_MOUNT);
    let mut ep0 = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&ep0_path)
        .await
        .context("opening FFS ep0 — is FunctionFS mounted?")?;
    ffs::write_descriptors(&mut ep0).await?;
    gadget::bind()?;

    // Phase 3: AOA2 handshake (car sends GET_PROTOCOL / SEND_STRING / START_ACCESSORY)
    info!("=== Phase 3: Waiting for Car (AOA2 handshake) ===");
    ffs::handle_aoa_setup(&mut ep0).await?;

    // Phase 4: Re-enumerate as accessory (PID 0x4EE1 → 0x2D00).
    // The DWC3 UDC stays EBUSY as long as ep0 is open — the FFS function holds
    // a reference that prevents rebinding.  Fix: close ep0 first so FFS releases
    // the UDC, then do a full gadget recreate with PID 0x2D00.
    info!("=== Phase 4: Switch to Accessory Mode ===");
    drop(ep0); // releases FFS function → device disconnects from host
    gadget::remove()?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    gadget::create()?; // recreates configfs + mounts FFS (PID=0x4EE1 initially)
    gadget::set_pid("0x2D00")?; // override before writing descriptors
    let ep0_path = format!("{}/ep0", ffs::FFS_MOUNT);
    let mut ep0 = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&ep0_path)
        .await
        .context("reopening FFS ep0 after PID switch")?;
    ffs::write_descriptors(&mut ep0).await?;
    gadget::bind()?;
    info!("gadget live as PID 0x2D00");

    // Phase 7: Input injection — set up once, reused across reconnects.
    // The display comes from the CLI, the same value the video encoder captures
    // from: reading $DISPLAY here independently made touches land on one screen
    // while video came from another.
    info!("=== Phase 7: Input Setup ===");
    let mut input = InputInjector::new(cli.width, cli.height, &cli.display);
    if let Err(e) = input.setup() {
        warn!("uinput setup failed: {e} (touch won't work)");
    }

    // Phase 5: Wait for the car's first connection after Phase 4 re-enumeration.
    // Subsequent reconnects (host process restart without physical disconnect) do NOT
    // produce a new ENABLE event — the gadget stays ENABLED for the whole USB session.
    // So we only do wait_for_enable once here; inside the loop we just reopen the
    // endpoint files directly, falling back to wait_for_enable only if the host
    // physically disconnects (cable pull → DISABLE on ep0).
    info!("=== Phase 5: Waiting for Car (first connect) ===");
    // Cleared when the wait is cut short by a shutdown signal or an ep0 error, in
    // which case we skip straight to teardown instead of entering the loop.
    let mut car_connected = true;
    tokio::select! {
        _ = shutdown_rx.changed() => car_connected = false,
        result = ffs::wait_for_enable(&mut ep0) => {
            if let Err(e) = result {
                error!("ep0 error on first connect: {e:#}");
                car_connected = false;
            }
        }
    }

    // Reconnect loop — handles multiple headunit sessions without restarting.
    if car_connected {
        loop {
            // Phase 6: Open bulk endpoints.
            let Some((ep_out, ep_in)) = open_endpoints_or_wait(&mut ep0, &mut shutdown_rx).await
            else {
                break;
            };
            let conn = Arc::new(AccessoryConnection::from_ffs(ep_out, ep_in));

            // Phase 8: AA Protocol
            info!("=== Phase 8: AA Protocol ===");
            match run_protocol(conn, &mut input, &cli, shutdown_rx.clone()).await {
                Ok(()) => info!("session ended normally"),
                Err(e) => error!("session ended: {e:#}"),
            }

            if *shutdown_rx.borrow() {
                break;
            }

            info!("=== Session ended — reconnecting ===");
            // Brief pause to let tokio clean up the previous session's tasks and
            // release the endpoint file descriptors before reopening them.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    info!("shutting down — releasing input device and USB gadget");
    input.destroy();
    gadget::remove()?;
    info!("bridge shutdown complete");
    Ok(())
}

/// Watch for Ctrl-C (interactive) and SIGTERM (`systemctl stop`), and flip the
/// shutdown watch channel so the reconnect loop can exit through the normal
/// teardown path.
fn spawn_signal_handler(tx: watch::Sender<bool>) {
    tokio::spawn(async move {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => Some(s),
                Err(e) => {
                    error!("cannot install SIGTERM handler: {e}");
                    None
                }
            };
        let terminate = async {
            match sigterm.as_mut() {
                Some(s) => {
                    s.recv().await;
                }
                // Never resolves — Ctrl-C still works without a SIGTERM handler.
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            result = tokio::signal::ctrl_c() => match result {
                Ok(()) => info!("SIGINT received — shutting down"),
                Err(e) => {
                    error!("cannot install Ctrl-C handler: {e} — signal shutdown disabled");
                    std::future::pending::<()>().await
                }
            },
            _ = terminate => info!("SIGTERM received — shutting down"),
        }

        let _ = tx.send(true);
        // Hold the sender forever: dropping it makes every receiver's `changed()`
        // resolve with an error, which the loops below read as a shutdown request.
        std::future::pending::<()>().await
    });
}

/// Open the bulk endpoints, waiting for the host to (re)connect if they are not
/// available. Returns `None` when the bridge should shut down.
///
/// FunctionFS allows exactly one opener per endpoint, and the previous session's
/// USB reader may still be parked in a blocking read on ep1 (a blocking bulk read
/// cannot be interrupted from userspace). EBUSY immediately after a session ends is
/// therefore expected and transient, so retry briefly before falling back to
/// `wait_for_enable`, which only returns when the host physically reconnects.
async fn open_endpoints_or_wait(
    ep0: &mut tokio::fs::File,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<(tokio::fs::File, tokio::fs::File)> {
    info!("=== Phase 6: Opening Bulk Endpoints ===");
    let mut attempt = 0u32;
    loop {
        match ffs::open_endpoints().await {
            Ok(endpoints) => return Some(endpoints),
            Err(e) if attempt < ENDPOINT_OPEN_RETRIES => {
                attempt += 1;
                tracing::debug!(
                    "endpoints busy ({e:#}) — retry {attempt}/{ENDPOINT_OPEN_RETRIES}"
                );
                tokio::select! {
                    _ = shutdown.changed() => return None,
                    _ = tokio::time::sleep(ENDPOINT_OPEN_RETRY_DELAY) => {}
                }
            }
            Err(e) => {
                info!("endpoints unavailable ({e:#}) — waiting for host to reconnect");
                tokio::select! {
                    _ = shutdown.changed() => return None,
                    result = ffs::wait_for_enable(ep0) => {
                        if let Err(e) = result {
                            error!("ep0 fatal error: {e:#}");
                            return None;
                        }
                    }
                }
                attempt = 0;
            }
        }
    }
}

async fn run_protocol(
    conn: Arc<AccessoryConnection>,
    input: &mut InputInjector,
    cli: &Cli,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let tls_ctx = AATlsContext::new()?;
    let mut tls: Option<AATlsSession> = None;
    let mut tls_active = false;
    let mut channels = ChannelManager::new(cli.width, cli.height, cli.fps);
    let mut reassembly: HashMap<u8, Vec<u8>> = HashMap::new();
    let mut running = true;
    let mut channels_opened: usize = 0;
    let mut video_started = false;
    // Whether we already asked the head unit to set the video channel up. Video
    // streaming is gated on the setup handshake, never on the channel-open count.
    let mut video_setup_sent = false;
    let mut audio_started = false;
    // System audio (phone → car system sounds, 16 kHz mono) is opt-in: it needs a
    // distinct PulseAudio source ($RASCAR_SYSTEM_AUDIO_SOURCE) so it doesn't just
    // duplicate the media stream onto the car speakers. Off unless that env is set.
    let mut system_audio_started = false;
    // Incremented on session reset so old media tasks know their reports are stale.
    let mut video_generation: u64 = 0;

    // Channel: raw incoming bytes from USB reader task → main loop
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<Vec<u8>>(16);
    // Channel: encoded H.264 chunks from encoder task → main loop for sending.
    // Recreated on session reset so the old encoder task gets a send error and stops.
    let (mut video_chunk_tx, mut video_chunk_rx) = mpsc::channel::<Vec<u8>>(4);
    // Channel: (target_channel_id, raw PCM) from audio capture tasks → main loop.
    // The channel id lets one queue carry both media-audio and system-audio streams.
    // Recreated on session reset so the old capture task stops cleanly.
    let (mut audio_chunk_tx, mut audio_chunk_rx) = mpsc::channel::<(u8, Vec<u8>)>(8);
    // Signals main loop when USB reader detects a disconnect.
    let (disc_tx, mut disc_rx) = tokio::sync::oneshot::channel::<()>();
    // Media subprocess supervision: every encoder/capture task reports its own
    // termination here, tagged with the generation it belongs to.
    let (media_exit_tx, mut media_exit_rx) = mpsc::channel::<(u64, MediaExit)>(4);
    // Microphone audio (car → phone) playback queue. Bounded + drop-on-full.
    let (mut mic_tx, mic_rx) = mpsc::channel::<Vec<u8>>(8);

    // Single monotonic origin for media frame timestamps. `Instant::now().elapsed()`
    // is always ~0; AA expects timestamps relative to a fixed session start.
    let session_start = Instant::now();

    // Byte-level carry buffer: a single AA frame can be split across two USB bulk
    // reads (a large first frame exceeds the 16 KiB read buffer, or the FFS layer
    // coalesces/splits transfers). Unconsumed tail bytes are held here and prepended
    // to the next chunk so a partial frame is never dropped (which would desync the
    // stream). Distinct from `reassembly`, which reassembles multi-frame messages.
    let mut carry: Vec<u8> = Vec::new();

    // Handles for every task this session owns, so none of them outlive it.
    let mut media_tasks: Vec<JoinHandle<()>> = Vec::new();
    let mut mic_handle = spawn_mic_playback(mic_rx);

    // Spawn USB reader task — reads bulk endpoint continuously so we never block
    // the main loop waiting for incoming data.
    //
    // The JoinHandle MUST be kept. FunctionFS allows a single opener per endpoint,
    // so a reader left running after the session ends holds ep1's fd and makes
    // every later `open_endpoints()` fail with EBUSY — which used to wedge the
    // reconnect loop permanently and leak a blocking-pool thread per session.
    let reader_handle = {
        let conn_r = conn.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16384];
            loop {
                match conn_r.read(&mut buf).await {
                    Ok(0) => {
                        // Ok(0) from a bulk endpoint read is EOF — the host has
                        // disconnected (a blocking bulk read never returns 0 for an
                        // idle-but-connected endpoint). Signal the main loop and stop;
                        // otherwise this task spins forever holding the endpoint fds
                        // open and leaks a task + fds on every reconnect.
                        info!("USB reader: endpoint EOF — host disconnected");
                        let _ = disc_tx.send(());
                        break;
                    }
                    Ok(n) => {
                        if incoming_tx.send(buf[..n].to_vec()).await.is_err() {
                            break; // main loop exited
                        }
                    }
                    Err(e) => {
                        error!("USB reader: {e}");
                        let _ = disc_tx.send(());
                        break;
                    }
                }
            }
        })
    };

    // The session body runs inside an async block so that EVERY exit path — `?`,
    // an early `return`, a `break` — lands on the teardown below instead of
    // dropping out of the function with tasks still running.
    let result: Result<()> = async {
        while running {
            tokio::select! {
                biased; // prioritise incoming protocol messages over video

                // ── Shutdown requested (Ctrl-C / SIGTERM) ────────────────────────
                _ = shutdown.changed() => {
                    info!("shutdown requested — ending session");
                    break;
                }

                // ── Host disconnected (USB reader exited with error) ──────────────
                _ = &mut disc_rx => {
                    info!("host disconnected — ending session");
                    break;
                }

                // ── A media subprocess task terminated ────────────────────────────
                exit = media_exit_rx.recv() => {
                    match exit {
                        Some((generation, what)) if generation == video_generation => match what {
                            MediaExit::VideoEncoder => {
                                // A dead encoder leaves the head unit showing the last
                                // decoded frame forever. End the session so the
                                // reconnect loop rebuilds the whole pipeline.
                                warn!("video encoder terminated — ending session");
                                break;
                            }
                            MediaExit::AudioCapture(audio_ch) => {
                                warn!("audio capture on ch{audio_ch} terminated — no more audio this session");
                                if audio_ch == channel::MEDIA_AUDIO { audio_started = false; }
                                if audio_ch == channel::SYSTEM_AUDIO { system_audio_started = false; }
                            }
                        },
                        // From a previous generation (session reset) — not actionable.
                        Some((generation, what)) => {
                            tracing::debug!("stale media exit {what:?} from generation {generation}");
                        }
                        // Unreachable while this scope holds `media_exit_tx`.
                        None => {}
                    }
                }

                // ── Incoming USB data ────────────────────────────────────────────
                incoming = incoming_rx.recv() => {
                    let chunk = match incoming {
                        Some(b) => b,
                        None => break, // reader task exited
                    };
                    if carry.is_empty() {
                        carry = chunk;
                    } else {
                        carry.extend_from_slice(&chunk);
                    }

                    let mut offset = 0;
                    let mut session_reset = false;
                    while offset < carry.len() {
                        let (frame, consumed) = match decode_frame(&carry[offset..]) {
                            Ok(v) => v,
                            // Incomplete frame at the tail — keep the unconsumed bytes and
                            // wait for the next read to supply the remainder.
                            Err(DecodeError::NeedMore(_)) => break,
                            // Oversize/corrupt frame: we have lost byte alignment. Drop the
                            // whole buffer and resync on the next FIRST frame rather than
                            // misparsing subsequent bytes.
                            Err(DecodeError::MessageTooLarge(n)) => {
                                warn!("frame declared oversize ({n} bytes) — resyncing");
                                reassembly.clear();
                                offset = carry.len();
                                break;
                            }
                        };
                        offset += consumed;

                        let ch = frame.channel;
                        if frame.is_first() {
                            reassembly.insert(ch, frame.payload.clone());
                        } else if let Some(acc) = reassembly.get_mut(&ch) {
                            acc.extend_from_slice(&frame.payload);
                        } else {
                            reassembly.insert(ch, frame.payload.clone());
                        }

                        // Abort reassembly if a channel accumulates more than a full
                        // message worth of bytes (e.g. FIRST-without-LAST flood). This
                        // bounds memory against malformed / hostile head-unit input.
                        if let Some(acc) = reassembly.get(&ch) {
                            if acc.len() > MAX_MESSAGE_SIZE {
                                warn!(
                                    "channel {ch}: reassembly exceeded {} bytes — dropping",
                                    MAX_MESSAGE_SIZE
                                );
                                reassembly.remove(&ch);
                                continue;
                            }
                        }

                        if !frame.is_last() {
                            continue;
                        }

                        let data = reassembly.remove(&ch).unwrap_or(frame.payload.clone());

                        let data = if frame.is_encrypted() {
                            match tls.as_mut() {
                                Some(t) => t.decrypt(&data)?,
                                // Passing ciphertext through as plaintext hands the
                                // dispatcher two random bytes as a message type. This
                                // happens for frames still in flight when a mid-session
                                // VersionRequest tears the TLS session down.
                                None => {
                                    warn!("channel {ch}: ENCRYPTED frame with no TLS session — dropping");
                                    continue;
                                }
                            }
                        } else {
                            data
                        };

                        match ch {
                            channel::CONTROL => {
                                let gen_before = video_generation;
                                handle_control(
                                    &data,
                                    &conn,
                                    &mut tls,
                                    &mut tls_active,
                                    &tls_ctx,
                                    &mut channels,
                                    &mut video_started,
                                    &mut video_setup_sent,
                                    &mut audio_started,
                                    cli,
                                    &mut running,
                                    &mut channels_opened,
                                    &mut reassembly,
                                    &mut video_generation,
                                )
                                .await?;
                                // If handle_control bumped the generation, tear down and
                                // recreate this session's media plumbing.
                                if video_generation != gen_before {
                                    // Stop the previous generation's subprocess tasks
                                    // outright: recreating the queues alone leaves a task
                                    // parked in read_chunk() with a live ffmpeg/parec.
                                    for task in media_tasks.drain(..) {
                                        task.abort();
                                    }
                                    let (new_tx, new_rx) = mpsc::channel::<Vec<u8>>(4);
                                    video_chunk_tx = new_tx;
                                    video_chunk_rx = new_rx;
                                    let (new_atx, new_arx) = mpsc::channel::<(u8, Vec<u8>)>(8);
                                    audio_chunk_tx = new_atx;
                                    audio_chunk_rx = new_arx;
                                    system_audio_started = false;
                                    // Restart mic playback so a session reset also clears
                                    // a latched playback failure.
                                    mic_handle.abort();
                                    let (new_mtx, new_mrx) = mpsc::channel::<Vec<u8>>(8);
                                    mic_tx = new_mtx;
                                    mic_handle = spawn_mic_playback(new_mrx);
                                    // Drain only bytes that were already queued in incoming_rx
                                    // at the moment of session reset (stale data from the
                                    // previous session still in the FFS kernel buffer).
                                    // Use try_recv() so we never block — the new ClientHello
                                    // hasn't arrived yet (the peer is still processing our
                                    // VersionResponse), so anything in the queue now is old.
                                    let mut drained = 0usize;
                                    while let Ok(stale) = incoming_rx.try_recv() {
                                        drained += stale.len();
                                    }
                                    if drained > 0 {
                                        tracing::debug!("drained {drained} stale bytes after session reset");
                                    }
                                    // Any bytes still buffered belong to the old session;
                                    // drop them so the fresh ClientHello starts clean.
                                    session_reset = true;
                                    info!("ready for new TLS handshake");
                                }
                            }
                            channel::INPUT => {
                                // The message type MUST be checked. Feeding every message
                                // on this channel to the touch parser turns an unrelated
                                // indication into a phantom tap at whatever the varints
                                // happen to decode to — a real click on the dashboard, or
                                // a touch-down that is never released.
                                if data.len() >= 2 {
                                    let msg_type = u16::from_be_bytes([data[0], data[1]]);
                                    match msg_type {
                                        MSG_INPUT_EVENT_INDICATION => {
                                            let events = parse_touch_event(&data[2..]);
                                            for (action, x, y, pid) in &events {
                                                info!("← touch: {action:?} at ({x},{y}) pid={pid}");
                                            }
                                            for (action, x, y, pid) in events {
                                                input.inject_touch(action, x, y, pid);
                                            }
                                        }
                                        MSG_INPUT_BINDING_REQUEST | MSG_INPUT_BINDING_RESPONSE => {
                                            tracing::debug!("← input binding msg 0x{msg_type:04x}");
                                        }
                                        _ => tracing::debug!(
                                            "← input ch{ch} msg 0x{msg_type:04x} ({} bytes) — not a touch event, ignored",
                                            data.len()
                                        ),
                                    }
                                } else {
                                    warn!("← input frame too short: {} bytes", data.len());
                                }
                            }
                            channel::VIDEO => {
                                if data.len() >= 2 {
                                    let msg_type = u16::from_be_bytes([data[0], data[1]]);
                                    match msg_type {
                                        MSG_VIDEO_FOCUS => info!("← video focus notification"),
                                        // Head-unit-driven setup — the usual AA direction
                                        // for video. Answer with our config, then start.
                                        MSG_MEDIA_SETUP_REQUEST => {
                                            info!("← video setup request");
                                            let resp = channels.encode_video_setup();
                                            send_secure(&conn, &mut tls, tls_active, channel::VIDEO, &resp).await?;
                                            start_video(
                                                &conn, &mut tls, tls_active, cli,
                                                &mut video_started, &video_chunk_tx,
                                                &media_exit_tx, video_generation, &mut media_tasks,
                                            ).await?;
                                        }
                                        // Response to the setup request we send once the
                                        // channels are open, for head units that expect the
                                        // phone to drive video setup the way audio is.
                                        MSG_MEDIA_SETUP_RESPONSE => {
                                            info!("← video setup response");
                                            start_video(
                                                &conn, &mut tls, tls_active, cli,
                                                &mut video_started, &video_chunk_tx,
                                                &media_exit_tx, video_generation, &mut media_tasks,
                                            ).await?;
                                        }
                                        MSG_STOP_REQUEST => info!("← video stop request"),
                                        MSG_MEDIA_DATA_ACK => tracing::trace!("← video ack"),
                                        _ => tracing::debug!(
                                            "← video ch{ch} msg 0x{msg_type:04x} ({} bytes)",
                                            data.len()
                                        ),
                                    }
                                }
                            }
                            channel::MEDIA_AUDIO | channel::SYSTEM_AUDIO => {
                                if data.len() >= 2 {
                                    let msg_type = u16::from_be_bytes([data[0], data[1]]);
                                    info!("← audio ch{ch} msg 0x{msg_type:04x} ({} bytes)", data.len());
                                    match msg_type {
                                        MSG_MEDIA_SETUP_RESPONSE => {
                                            // Headunit accepted our setup request
                                            info!("← audio setup OK on ch{ch}");
                                            // Send start indication with session=1, config=0
                                            let start = ChannelManager::encode_av_start(1, 0);
                                            send_secure(&conn, &mut tls, tls_active, ch, &start).await?;
                                            info!("→ audio AVChannelStartIndication on ch{ch}");

                                            // Start capture. `spawn_capture` streams a PCM
                                            // source into the audio queue tagged with its
                                            // target channel id. Media audio (48 kHz stereo)
                                            // always captures the sink monitor; system audio
                                            // (16 kHz mono) captures only when a distinct
                                            // source is configured, so it never duplicates the
                                            // media stream on the car's speakers.
                                            let spawn_capture =
                                                |ch_id: u8,
                                                 rate: u32,
                                                 chans: u32,
                                                 source: String,
                                                 tx: mpsc::Sender<(u8, Vec<u8>)>,
                                                 exit_tx: mpsc::Sender<(u64, MediaExit)>,
                                                 generation: u64| {
                                                    tokio::spawn(async move {
                                                        let mut cap = AudioCapture::new(rate, chans);
                                                        if let Err(e) = cap.start(Some(&source)).await {
                                                            error!("audio capture start failed (ch{ch_id}): {e}");
                                                            let _ = exit_tx.send((generation, MediaExit::AudioCapture(ch_id))).await;
                                                            return;
                                                        }
                                                        info!("audio capture started (ch{ch_id}, {rate}Hz {chans}ch, src={source})");
                                                        let mut buf = vec![0u8; 3840];
                                                        loop {
                                                            match cap.read_chunk(&mut buf).await {
                                                                Ok(n) if n > 0 => {
                                                                    if tx.send((ch_id, buf[..n].to_vec())).await.is_err() { break; }
                                                                }
                                                                // Ok(0) is EOF: the parec subprocess exited.
                                                                // Stop so cap.stop() runs — never busy-spin.
                                                                Ok(_) => { warn!("audio capture stdout closed (EOF) — ch{ch_id} exited"); break; }
                                                                Err(e) => { error!("audio read (ch{ch_id}): {e}"); break; }
                                                            }
                                                        }
                                                        cap.stop().await;
                                                        // Report termination so the protocol loop
                                                        // stops believing audio is streaming.
                                                        let _ = exit_tx.send((generation, MediaExit::AudioCapture(ch_id))).await;
                                                    })
                                                };
                                            if ch == channel::MEDIA_AUDIO && !audio_started {
                                                audio_started = true;
                                                let source = std::env::var("RASCAR_AUDIO_SOURCE")
                                                    .ok()
                                                    .filter(|s| !s.is_empty())
                                                    .unwrap_or_else(|| "alsa_output.platform-sound.stereo-fallback.monitor".to_string());
                                                media_tasks.push(spawn_capture(
                                                    channel::MEDIA_AUDIO, 48000, 2, source,
                                                    audio_chunk_tx.clone(), media_exit_tx.clone(), video_generation,
                                                ));
                                            } else if ch == channel::SYSTEM_AUDIO && !system_audio_started {
                                                match std::env::var("RASCAR_SYSTEM_AUDIO_SOURCE").ok().filter(|s| !s.is_empty()) {
                                                    Some(source) => {
                                                        system_audio_started = true;
                                                        media_tasks.push(spawn_capture(
                                                            channel::SYSTEM_AUDIO, 16000, 1, source,
                                                            audio_chunk_tx.clone(), media_exit_tx.clone(), video_generation,
                                                        ));
                                                    }
                                                    None => info!(
                                                        "system audio channel open but $RASCAR_SYSTEM_AUDIO_SOURCE unset — not streaming (avoids duplicating media audio)"
                                                    ),
                                                }
                                            }
                                        }
                                        MSG_AUDIO_FOCUS_RESPONSE => {
                                            info!("← audio focus response on ch{ch}");
                                        }
                                        MSG_MEDIA_SETUP_REQUEST => {
                                            info!("← audio setup request on ch{ch} (unexpected)");
                                        }
                                        MSG_START_REQUEST => {
                                            info!("← audio start request on ch{ch}");
                                        }
                                        MSG_STOP_REQUEST => {
                                            info!("← audio stop on ch{ch}");
                                        }
                                        MSG_MEDIA_DATA_ACK => {
                                            // Flow control ack — good, headunit is receiving
                                            tracing::trace!("← audio ack ch{ch}");
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            channel::MIC => {
                                // Microphone audio flows car → phone. A media-data frame is
                                // msg_type(2) + timestamp(8) + PCM; anything shorter is a
                                // control message.
                                if data.len() >= 2 {
                                    let msg_type = u16::from_be_bytes([data[0], data[1]]);
                                    if msg_type == MSG_MEDIA_DATA && data.len() > 10 {
                                        // Hand the PCM to the playback task rather than
                                        // writing to pacat's stdin here: a stalled sink fills
                                        // the 64 KiB pipe, `write_all` blocks, and the whole
                                        // protocol loop stops answering pings and draining
                                        // ep1 — which stalls the host's bulk-OUT endpoint.
                                        if mic_tx.try_send(data[10..].to_vec()).is_err() {
                                            tracing::trace!("mic playback queue full — dropping frame");
                                        }
                                    } else if msg_type == MSG_MIC_REQUEST {
                                        info!("← mic start/stop request on ch{ch}");
                                    } else {
                                        tracing::debug!("← mic ch{ch} msg 0x{msg_type:04x} ({} bytes)", data.len());
                                    }
                                }
                            }
                            _ => {
                                tracing::debug!(
                                    "channel {ch} ({}): {} bytes",
                                    channel::name(ch),
                                    data.len()
                                );
                            }
                        }

                        // A session reset (new VersionRequest) invalidates any remaining
                        // buffered bytes — stop processing this chunk and drop the carry.
                        if session_reset {
                            break;
                        }
                    }

                    if session_reset {
                        carry.clear();
                    } else {
                        // Keep only the unconsumed tail (a partial frame) for next time.
                        carry.drain(..offset);
                    }
                }

                // ── Outgoing video chunk ─────────────────────────────────────────
                chunk = video_chunk_rx.recv() => {
                    if let Some(chunk) = chunk {
                        // Discard stale chunks if the session was reset
                        if !video_started { continue; }
                        let msg = encode_media_frame(
                            MSG_MEDIA_DATA,
                            &chunk,
                            session_start.elapsed().as_micros() as u64,
                        );
                        send_media(
                            &conn, &mut tls, tls_active, channel::VIDEO, &msg, VIDEO_WRITE_BUDGET,
                        ).await?;
                    }
                }

                // ── Outgoing audio chunk (media or system, tagged with its channel) ──
                chunk = audio_chunk_rx.recv() => {
                    if let Some((audio_ch, chunk)) = chunk {
                        if !audio_started && !system_audio_started { continue; }
                        let msg = encode_media_frame(
                            MSG_MEDIA_DATA,
                            &chunk,
                            session_start.elapsed().as_micros() as u64,
                        );
                        // Errors propagate exactly like the video arm's. Swallowing them
                        // left the loop running on a link that was already broken.
                        send_media(
                            &conn, &mut tls, tls_active, audio_ch, &msg, AUDIO_WRITE_BUDGET,
                        ).await?;
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    // ── Session teardown ─────────────────────────────────────────────────────
    // Every task spawned above must be gone before the caller reopens the
    // endpoints. The USB reader in particular is parked in a blocking read on ep1
    // and will never notice its channel closing, so it has to be aborted; only
    // then can the last `AccessoryConnection` reference drop and release the fds.
    for task in media_tasks.drain(..) {
        task.abort();
        let _ = task.await;
    }
    mic_handle.abort();
    let _ = mic_handle.await;
    reader_handle.abort();
    let _ = reader_handle.await;
    drop(conn);

    result
}

/// Drain the mic playback queue into `pacat`.
///
/// Playback lives in its own task because `write_chunk` can block on a stalled
/// sink. A start or write failure latches playback off for the rest of the session
/// instead of forking a fresh doomed `pacat` for every MIC frame (~50/s).
fn spawn_mic_playback(mut rx: mpsc::Receiver<Vec<u8>>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut player: Option<AudioPlayer> = None;
        let mut disabled = false;
        while let Some(pcm) = rx.recv().await {
            if disabled {
                continue;
            }
            if player.is_none() {
                // Started lazily on the first data frame, so pacat is never spawned
                // when the car has no microphone.
                let mut p = AudioPlayer::new(16000, 1);
                if let Err(e) = p.start().await {
                    error!("mic playback start failed: {e} — disabled for this session");
                    disabled = true;
                    continue;
                }
                info!("mic playback started (car → speaker)");
                player = Some(p);
            }
            let write = player
                .as_mut()
                .expect("player started above")
                .write_chunk(&pcm)
                .await;
            if let Err(e) = write {
                error!("mic playback write failed: {e} — disabled for this session");
                if let Some(mut p) = player.take() {
                    p.stop().await;
                }
                disabled = true;
            }
        }
        if let Some(mut p) = player {
            p.stop().await;
        }
    })
}

/// Send AVChannelStartIndication on the video channel and spawn the encoder.
///
/// Video streaming must NOT be triggered by the channel-open count: MEDIA_DATA
/// that arrives before the head unit's decoder is configured is discarded — the
/// leading IDR included — so the screen never paints. Start only once the video
/// channel's setup handshake has completed, mirroring the audio sequence.
/// Idempotent: a second setup message will not spawn a second encoder.
#[allow(clippy::too_many_arguments)]
async fn start_video(
    conn: &AccessoryConnection,
    tls: &mut Option<AATlsSession>,
    tls_active: bool,
    cli: &Cli,
    video_started: &mut bool,
    video_chunk_tx: &mpsc::Sender<Vec<u8>>,
    media_exit_tx: &mpsc::Sender<(u64, MediaExit)>,
    generation: u64,
    media_tasks: &mut Vec<JoinHandle<()>>,
) -> Result<()> {
    if *video_started {
        return Ok(());
    }

    let start = ChannelManager::encode_av_start(1, 0);
    send_secure(conn, tls, tls_active, channel::VIDEO, &start).await?;
    info!("→ video AVChannelStartIndication on ch{}", channel::VIDEO);
    *video_started = true;

    let tx = video_chunk_tx.clone();
    let exit_tx = media_exit_tx.clone();
    let settings = VideoSettings {
        width: cli.width,
        height: cli.height,
        fps: cli.fps,
        display: cli.display.clone(),
        ..Default::default()
    };

    media_tasks.push(tokio::spawn(async move {
        let mut enc = VideoEncoder::new(settings);
        if let Err(e) = enc.start().await {
            error!("video encoder failed to start: {e}");
            let _ = exit_tx.send((generation, MediaExit::VideoEncoder)).await;
            return;
        }
        info!("video encoder started (gen={generation})");
        let mut vbuf = vec![0u8; 8192];
        loop {
            match enc.read_chunk(&mut vbuf).await {
                Ok(n) if n > 0 => {
                    // tx.send failing means the main loop dropped its receiver
                    // (session reset bumped the generation, and a new session will
                    // spawn a fresh encoder). Stop cleanly.
                    if tx.send(vbuf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
                Ok(_) => {
                    // Ok(0) is EOF: the ffmpeg encoder subprocess exited (crash, bad
                    // DISPLAY, x264 error). Stop the loop so enc.stop() runs — never
                    // busy-spin on a dead pipe.
                    warn!("video encoder stdout closed (EOF) — encoder exited");
                    break;
                }
                Err(e) => {
                    error!("video encoder read error: {e}");
                    break;
                }
            }
        }
        enc.stop().await;
        let _ = exit_tx.send((generation, MediaExit::VideoEncoder)).await;
    }));

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_control(
    data: &[u8],
    conn: &AccessoryConnection,
    tls: &mut Option<AATlsSession>,
    tls_active: &mut bool,
    tls_ctx: &AATlsContext,
    channels: &mut ChannelManager,
    video_started: &mut bool,
    video_setup_sent: &mut bool,
    audio_started: &mut bool,
    cli: &Cli,
    running: &mut bool,
    channels_opened: &mut usize,
    reassembly: &mut HashMap<u8, Vec<u8>>,
    video_generation: &mut u64,
) -> Result<()> {
    let Some((msg_type_u16, payload)) = control::decode_message(data) else {
        return Ok(());
    };

    let msg_type = MsgType::from_u16(msg_type_u16);

    match msg_type {
        Some(MsgType::VersionRequest) => {
            if let Some((major, minor)) = control::parse_version_request(data) {
                info!("← version request: {major}.{minor}");
            }
            // Always reset protocol state on a new VersionRequest so that stale
            // TLS sessions (e.g. from a previous headunit run that partially completed
            // the handshake) don't corrupt the new session.
            info!("new connection — resetting session state");
            *tls = None;
            *tls_active = false;
            *channels = ChannelManager::new(cli.width, cli.height, cli.fps);
            *channels_opened = 0;
            *video_started = false;
            *video_setup_sent = false;
            *audio_started = false;
            reassembly.clear();
            *video_generation += 1;
            let resp = control::make_version_response();
            send_raw(conn, channel::CONTROL, &resp).await?;
            info!("→ version response sent");
        }

        Some(MsgType::SslHandshake) => {
            info!("← SSL handshake data ({} bytes)", payload.len());
            // Construct the TLS session fallibly — a malformed/early SSL handshake
            // message from the head unit must never panic the bridge.
            if tls.is_none() {
                *tls = Some(
                    AATlsSession::new(tls_ctx.config.clone())
                        .context("creating TLS session")?,
                );
            }
            let session = tls.as_mut().expect("just inserted above");
            let out = session.feed(payload)?;
            if !out.is_empty() {
                let msg = control::encode_message(MsgType::SslHandshake, &out);
                send_raw(conn, channel::CONTROL, &msg).await?;
                info!("→ SSL handshake response sent");
            }
            if session.is_handshake_complete() {
                *tls_active = true;
                info!("TLS handshake complete — encrypted mode");
            }
        }

        Some(MsgType::AuthComplete) => {
            // A head unit that rejects a non-Google cert reports failure here and
            // then goes silent — which looks identical to a service-discovery stall
            // unless the status is actually decoded and acted on.
            let status = parse_auth_status(payload);
            info!(
                "← auth complete: {} bytes, hex={:02x?}, status={status:?}",
                payload.len(),
                payload
            );
            match status {
                // 0 == OK/SUCCESS. A missing field 1 is tolerated (some head units
                // send an empty indication) but flagged.
                Some(0) | None => {
                    if status.is_none() {
                        warn!("auth complete carried no readable status field — assuming success");
                    }
                    let req = control::make_service_discovery_request("RasCar", "RasCar");
                    send_secure(conn, tls, *tls_active, channel::CONTROL, &req).await?;
                    info!("→ service discovery request sent");
                }
                Some(code) => anyhow::bail!(
                    "head unit REJECTED authentication (status={code}) — it did not accept our \
                     TLS certificate. Android Auto requires a Google-signed device certificate; \
                     this is a certificate wall, not a service-discovery stall."
                ),
            }
        }

        Some(MsgType::ServiceDiscoveryResponse) => {
            info!("← service discovery response ({} bytes)", payload.len());
            for &(ch_id, name) in &[
                (channel::INPUT, "input"),
                (channel::SENSOR, "sensor"),
                (channel::VIDEO, "video"),
                (channel::MEDIA_AUDIO, "media_audio"),
                (channel::SYSTEM_AUDIO, "system_audio"),
                (channel::MIC, "microphone"),
            ] {
                let req = control::make_channel_open_request(ch_id);
                send_secure(conn, tls, *tls_active, channel::CONTROL, &req).await?;
                info!("→ channel open: {name} (ch{ch_id})");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        Some(MsgType::ChannelOpenResponse) => {
            *channels_opened += 1;
            info!("← channel open response ({}/6)", *channels_opened);
            // Trigger the start sequence exactly once, on the 6th response. Using
            // `!= 6` (not `< 6`) prevents a duplicate / 7th ChannelOpenResponse from
            // re-entering this block and re-sending driving-status / audio-focus /
            // setup messages, which confuses the head unit mid-stream.
            // NOTE (HW-gated follow-up): this still assumes all 6 requested channels
            // are accepted. A real head unit that rejects an optional channel (MIC /
            // SYSTEM_AUDIO) sends <6 responses and never triggers. Correlating each
            // response to its channel id + status requires parsing the
            // ChannelOpenResponse protobuf and validating against a real head unit —
            // tracked as a follow-up rather than guessed at blind.
            if *channels_opened != 6 {
                return Ok(());
            }

            // All 6 channels open — send driving status, then negotiate the A/V
            // channels. Note that streaming does NOT start here: both audio and video
            // only start once their own setup handshake completes.
            let status = ChannelManager::encode_sensor_driving_status(0);
            send_secure(conn, tls, *tls_active, channel::SENSOR, &status).await?;
            info!("→ driving status: UNRESTRICTED");

            // Audio: request focus on CONTROL channel, then setup on audio channel.
            // AudioFocusRequest (type 18) is a CONTROL channel message.
            // AVChannelSetupRequest (0x8000) goes on the audio data channel.
            if !*audio_started {
                // Step 1: AudioFocusRequest on CONTROL — must come first
                let focus_req = ChannelManager::encode_audio_focus_request(1);
                send_secure(conn, tls, *tls_active, channel::CONTROL, &focus_req).await?;
                info!("→ audio AudioFocusRequest (GAIN) on CONTROL channel");

                // Step 2: AVChannelSetupRequest on MEDIA_AUDIO channel
                let setup_req = ChannelManager::encode_av_setup_request(0);
                send_secure(conn, tls, *tls_active, channel::MEDIA_AUDIO, &setup_req).await?;
                info!("→ audio AVChannelSetupRequest (config_index=0) on ch{}", channel::MEDIA_AUDIO);
            }

            // Video: mirror the audio sequence. Head units normally drive video setup
            // themselves (they send AVChannelSetupRequest), so this is only the
            // fallback for those that expect the phone to ask first. Either way the
            // encoder starts from the setup handshake, never from this counter.
            if !*video_started && !*video_setup_sent {
                *video_setup_sent = true;
                let setup_req = ChannelManager::encode_av_setup_request(0);
                send_secure(conn, tls, *tls_active, channel::VIDEO, &setup_req).await?;
                info!("→ video AVChannelSetupRequest (config_index=0) on ch{}", channel::VIDEO);
            }

            info!("=== Channels open — awaiting A/V setup handshakes ===");
        }

        Some(MsgType::PingRequest) => {
            let resp = control::make_ping_response(data);
            // Housekeeping: allowed in the clear so ping/pong still works before the
            // handshake completes, but the fallback is logged loudly.
            send_encrypted(conn, tls, *tls_active, channel::CONTROL, &resp, false).await?;
        }

        Some(MsgType::ByebyeRequest) => {
            info!("← byebye — shutting down");
            let resp = control::make_byebye_response();
            send_encrypted(conn, tls, *tls_active, channel::CONTROL, &resp, false).await?;
            *running = false;
        }

        Some(MsgType::AudioFocusNotify) => {
            info!("← audio focus granted (control channel)");
        }

        _ => {
            tracing::debug!("← control msg 0x{msg_type_u16:04x} ({} bytes)", payload.len());
        }
    }

    Ok(())
}

/// Decode `AuthCompleteIndication { status = 1 }`.
///
/// The status is a protobuf `int32`, so a negative value (−3 is this project's
/// known certificate-rejection code) is transmitted as a 10-byte sign-extended
/// varint. Reading `payload[1]` as a raw byte reports 253 for −3, which is why a
/// rejection used to be indistinguishable from a stall.
fn parse_auth_status(payload: &[u8]) -> Option<i32> {
    let mut pos = 0usize;
    while pos < payload.len() {
        let tag = payload[pos];
        pos += 1;
        let field_num = tag >> 3;
        match tag & 0x07 {
            0 => {
                let (value, next) = media::input::decode_varint(payload, pos)?;
                pos = next;
                if field_num == 1 {
                    // Truncating to 32 bits recovers the signed int32 from the
                    // sign-extended 64-bit varint.
                    return Some(value as u32 as i32);
                }
            }
            1 => pos = pos.checked_add(8).filter(|end| *end <= payload.len())?,
            5 => pos = pos.checked_add(4).filter(|end| *end <= payload.len())?,
            2 => {
                let (len, next) = media::input::decode_varint(payload, pos)?;
                let len = usize::try_from(len).ok()?;
                pos = next.checked_add(len).filter(|end| *end <= payload.len())?;
            }
            // Groups / reserved wire types cannot be skipped safely.
            _ => return None,
        }
    }
    None
}

/// Send a media-data message, bounded by `budget`.
///
/// A timeout here is ALWAYS fatal, by design. `send_encrypted` seals a TLS record
/// (advancing the sequence number) before it writes any bytes, so a cancelled write
/// leaves a truncated frame on the wire and a TLS state the peer can no longer
/// follow. That is only safe to abandon because the caller ends the session and the
/// reconnect builds a fresh TLS context and a fresh frame stream. Never turn this
/// into a "log and continue".
async fn send_media(
    conn: &AccessoryConnection,
    tls: &mut Option<AATlsSession>,
    tls_active: bool,
    channel: u8,
    data: &[u8],
    budget: Duration,
) -> Result<()> {
    match tokio::time::timeout(budget, send_secure(conn, tls, tls_active, channel, data)).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!(
            "USB write on channel {channel} did not finish within {budget:?} — \
             the host stopped draining the bulk endpoint"
        ),
    }
}

async fn send_raw(conn: &AccessoryConnection, channel: u8, data: &[u8]) -> Result<()> {
    let frames = fragment_message(channel, data, false);
    conn.write_all_frames(&frames).await
}

/// Send `data` on `channel`, encrypted when TLS is active.
///
/// `require_encryption` decides what happens when it is NOT active. Everything
/// carrying session content — the service discovery request (device name/brand),
/// the channel open requests, sensor data and media — must require it: silently
/// degrading to cleartext put that traffic on the wire in the clear while the log
/// still reported a successful send.
async fn send_encrypted(
    conn: &AccessoryConnection,
    tls: &mut Option<AATlsSession>,
    tls_active: bool,
    channel: u8,
    data: &[u8],
    require_encryption: bool,
) -> Result<()> {
    if tls_active {
        if let Some(t) = tls {
            let encrypted = t.encrypt(data)?;
            let frames = fragment_message(channel, &encrypted, true);
            return conn.write_all_frames(&frames).await;
        }
    }

    if require_encryption {
        anyhow::bail!(
            "refusing to send {} bytes on channel {channel} in cleartext — TLS is not active",
            data.len()
        );
    }
    warn!(
        "TLS not active — sending {} bytes on channel {channel} IN THE CLEAR",
        data.len()
    );
    send_raw(conn, channel, data).await
}

/// `send_encrypted` with encryption required — the default for session content.
async fn send_secure(
    conn: &AccessoryConnection,
    tls: &mut Option<AATlsSession>,
    tls_active: bool,
    channel: u8,
    data: &[u8],
) -> Result<()> {
    send_encrypted(conn, tls, tls_active, channel, data, true).await
}
