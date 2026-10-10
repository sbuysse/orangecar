mod checks;
mod display;
mod handshake;
mod tls;
// Shared, byte-identical with the bridge's copy (`bridge/src/protocol/transport.rs`),
// so its full API is carried here even where the head unit only exercises part of
// it. Suppress dead-code at the module level rather than annotating — and
// diverging — the shared file.
#[allow(dead_code)]
mod transport;
mod usb;

use anyhow::Result;
use checks::{print_result, print_summary, CheckResult};
use clap::Parser;
use handshake::AaHandshake;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use usb::UsbTransport;

#[derive(Parser)]
#[command(name = "rascar-headunit", about = "RasCar AA Protocol Conformance Prober")]
struct Cli {
    /// Per-step timeout in seconds
    #[arg(long, default_value = "5")]
    timeout: u64,
    /// Enable debug logging (hex frame dumps to stderr)
    #[arg(long)]
    debug: bool,
}

/// Run an `AaHandshake` method with a timeout and convert the outcome to a `CheckResult`.
/// On timeout, `passed = false` with detail "timeout".
/// On error, `passed = false` with the error string.
/// On success, `passed = true` with the provided detail.
///
/// `$hs` is the handshake itself, read *after* the future has been awaited (the
/// temporary holding its `&mut` borrow dies at the end of the `let outcome`
/// statement) so a failure can show the last frames actually on the wire.
macro_rules! hs_check {
    ($results:expr, $name:expr, $timeout_secs:expr, $detail:expr, $hs:expr, $fut:expr) => {{
        let dur = Duration::from_secs($timeout_secs);
        let outcome = timeout(dur, $fut).await;
        let r: CheckResult = match outcome {
            Ok(Ok(())) => CheckResult::plain($name, true, $detail),
            Ok(Err(e)) => CheckResult {
                name: $name,
                passed: false,
                detail: Some(e.to_string()),
                last_sent: $hs.last_sent(),
                last_received: $hs.last_received(),
            },
            Err(_) => CheckResult {
                name: $name,
                passed: false,
                detail: Some("timeout".into()),
                last_sent: $hs.last_sent(),
                last_received: $hs.last_received(),
            },
        };
        print_result(&r);
        let passed = r.passed;
        $results.push(r);
        if !passed {
            print_summary(&$results);
            std::process::exit(1);
        }
    }};
}

/// Variant for methods that return `Result<T>` where we compute the detail from `T`.
macro_rules! hs_check_val {
    ($results:expr, $name:expr, $timeout_secs:expr, $hs:expr, $fut:expr, $detail_fn:expr) => {{
        let dur = Duration::from_secs($timeout_secs);
        let outcome = timeout(dur, $fut).await;
        let r: CheckResult = match outcome {
            Ok(Ok(val)) => CheckResult::plain($name, true, $detail_fn(val)),
            Ok(Err(e)) => CheckResult {
                name: $name,
                passed: false,
                detail: Some(e.to_string()),
                last_sent: $hs.last_sent(),
                last_received: $hs.last_received(),
            },
            Err(_) => CheckResult {
                name: $name,
                passed: false,
                detail: Some("timeout".into()),
                last_sent: $hs.last_sent(),
                last_received: $hs.last_received(),
            },
        };
        print_result(&r);
        let passed = r.passed;
        $results.push(r);
        if !passed {
            print_summary(&$results);
            std::process::exit(1);
        }
    }};
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Scope --debug to this crate. A bare "debug" also turns on nusb's per-file
    // sysfs probe logging, which buries the frame hex dumps this flag exists for.
    let filter = if cli.debug { "info,rascar_headunit=debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let mut results: Vec<CheckResult> = Vec::new();

    // ── Phase -1: environment preflight ───────────────────────────────────
    // Everything probed here is spawned unconditionally later in the run, so
    // checking it up front turns "hangs with no window" into a named failure.
    let mut preflight_ok = true;
    for r in checks::preflight() {
        print_result(&r);
        preflight_ok &= r.passed;
        results.push(r);
    }
    if !preflight_ok {
        print_summary(&results);
        std::process::exit(1);
    }

    // ── Phase 0: USB / AOA2 init ──────────────────────────────────────────

    // USB open needs its own budget: the stale-AA-mode recovery path resets the
    // device, then polls up to 10s for it to re-appear, runs AOA2 init and
    // waits another 3s for re-enumeration. Capping it at `--timeout` (default
    // 5s) made that path unreachable — it always aborted mid-reconnect.
    let usb_timeout = cli.timeout.max(20);
    let transport = {
        match tokio::time::timeout(
            std::time::Duration::from_secs(usb_timeout),
            UsbTransport::open(),
        ).await {
            Ok(Ok((t, performed))) => {
                let detail = if performed { "0x4EE1 → 0x2D00" } else { "already 0x2D00" };
                let r = CheckResult::plain("AOA2 init", true, Some(detail.to_string()));
                print_result(&r);
                results.push(r);
                t
            }
            Ok(Err(e)) => {
                let r = CheckResult::plain("AOA2 init", false, Some(e.to_string()));
                print_result(&r);
                results.push(r);
                print_summary(&results);
                std::process::exit(1);
            }
            Err(_) => {
                let r = CheckResult::plain(
                    "AOA2 init",
                    false,
                    Some(format!("timeout after {usb_timeout}s")),
                );
                print_result(&r);
                results.push(r);
                print_summary(&results);
                std::process::exit(1);
            }
        }
    };

    let ep_in = transport.ep_in();
    let ep_out = transport.ep_out();
    let r = CheckResult::plain(
        "USB connect",
        true,
        Some(format!("ep IN=0x{ep_in:02x} OUT=0x{ep_out:02x}")),
    );
    print_result(&r);
    results.push(r);

    let mut hs = AaHandshake::new(transport);

    // ── Phase 1: AA Protocol ──────────────────────────────────────────────

    hs_check_val!(
        results,
        "Version exchange",
        cli.timeout,
        hs,
        hs.version_exchange(),
        |val: (u16, u16)| {
            let (major, minor) = val;
            Some(format!("host=1.7, device={major}.{minor}"))
        }
    );

    let tls_start = Instant::now();
    hs_check_val!(
        results,
        "TLS handshake",
        cli.timeout,
        hs,
        hs.tls_handshake(),
        |_: ()| { Some(format!("{}ms", tls_start.elapsed().as_millis())) }
    );

    hs_check!(results, "Auth complete", cli.timeout, None, hs, hs.auth_complete());

    hs_check_val!(
        results,
        "Service discovery",
        cli.timeout,
        hs,
        hs.service_discovery(),
        |val: (String, String)| {
            let (name, brand) = val;
            Some(format!("device={name}/{brand}"))
        }
    );

    // Channel opens — report each channel individually
    let opened_channels = {
        let dur = Duration::from_secs(cli.timeout);
        let outcome = timeout(dur, hs.channel_opens()).await;
        match outcome {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                let r = CheckResult {
                    name: "Channel opens",
                    passed: false,
                    detail: Some(e.to_string()),
                    last_sent: hs.last_sent(),
                    last_received: hs.last_received(),
                };
                print_result(&r);
                results.push(r);
                print_summary(&results);
                std::process::exit(1);
            }
            Err(_) => {
                let r = CheckResult {
                    name: "Channel opens",
                    passed: false,
                    detail: Some("timeout".into()),
                    last_sent: hs.last_sent(),
                    last_received: hs.last_received(),
                };
                print_result(&r);
                results.push(r);
                print_summary(&results);
                std::process::exit(1);
            }
        }
    };
    for &(ch_id, ch_name) in &[
        (1u8, "Channel open: input"),
        (2u8, "Channel open: sensor"),
        (3u8, "Channel open: video"),
        (4u8, "Channel open: media_audio"),
        (5u8, "Channel open: system_audio"),
        (7u8, "Channel open: microphone"),
    ] {
        let passed = opened_channels.contains(&ch_id);
        let r = CheckResult::plain(
            ch_name,
            passed,
            if passed { Some(format!("ch{ch_id}")) } else { Some("not received".into()) },
        );
        print_result(&r);
        let p = r.passed;
        results.push(r);
        if !p {
            print_summary(&results);
            std::process::exit(1);
        }
    }

    hs_check!(
        results,
        "Driving status",
        cli.timeout,
        Some("UNRESTRICTED".to_string()),
        hs,
        hs.send_driving_status()
    );

    let video_start = Instant::now();
    let first_frame: Vec<u8> = {
        let dur = Duration::from_secs(cli.timeout);
        let outcome = timeout(dur, hs.wait_for_video_frame()).await;
        match outcome {
            Ok(Ok(bytes)) => {
                let r = CheckResult::plain(
                    "First video frame",
                    true,
                    Some(format!(
                        "H.264 {} bytes, {}ms from connect",
                        bytes.len(),
                        video_start.elapsed().as_millis()
                    )),
                );
                print_result(&r);
                results.push(r);
                bytes
            }
            Ok(Err(e)) => {
                let r = CheckResult {
                    name: "First video frame",
                    passed: false,
                    detail: Some(e.to_string()),
                    last_sent: hs.last_sent(),
                    last_received: hs.last_received(),
                };
                print_result(&r);
                results.push(r);
                print_summary(&results);
                std::process::exit(1);
            }
            Err(_) => {
                let r = CheckResult {
                    name: "First video frame",
                    passed: false,
                    detail: Some("timeout".into()),
                    last_sent: hs.last_sent(),
                    last_received: hs.last_received(),
                };
                print_result(&r);
                results.push(r);
                print_summary(&results);
                std::process::exit(1);
            }
        }
    };

    print_summary(&results);

    let all_passed = results.iter().all(|r| r.passed);
    if !all_passed {
        std::process::exit(1);
    }

    // ── Stream video + audio ────────────────────────────────────────────
    eprintln!("\n=== All checks passed — streaming video + audio (Esc or close window to stop) ===");

    // H.264 channel: bridge → display thread.  Large buffer so we never drop
    // frames — H.264 P-frames depend on prior frames; a single drop corrupts
    // every subsequent frame until the next IDR keyframe.
    let (h264_tx, h264_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(128);
    // Touch channel: display thread → bridge
    let (touch_tx, mut touch_rx) = tokio::sync::mpsc::channel::<(u32, u32, u8)>(32);
    // Audio PCM channel: bridge → audio playback task
    let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

    // Spawn the display thread: owns the minifb window (Wayland-native),
    // decodes H.264 via ffmpeg, delivers touch events back to us.
    // Its `exited` handle is what makes Esc / window-close actually stop the
    // process, and what surfaces a pipeline that never started.
    let mut display = display::spawn(first_frame, h264_rx, touch_tx);

    // Spawn audio playback: pipe raw PCM (48kHz 16-bit stereo) to paplay.
    // When running as root (sudo), we need PULSE_SERVER to reach the user's PA.
    // Resolve the PulseAudio socket for the *invoking* user. Under sudo the
    // process uid is 0 and $XDG_RUNTIME_DIR often still points at root's runtime
    // dir (/run/user/0), which has no PulseAudio socket. So prefer $SUDO_UID (the
    // invoking user) when present, then $XDG_RUNTIME_DIR, then the current uid.
    let pulse_server = std::env::var("SUDO_UID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(|uid| format!("unix:/run/user/{uid}/pulse/native"))
        .or_else(|| {
            std::env::var("XDG_RUNTIME_DIR")
                .ok()
                .map(|dir| format!("unix:{dir}/pulse/native"))
        })
        .unwrap_or_else(|| {
            let uid = unsafe { libc::getuid() };
            format!("unix:/run/user/{uid}/pulse/native")
        });

    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut child = match tokio::process::Command::new("paplay")
            .args(["--format=s16le", "--rate=48000", "--channels=2", "--raw"])
            .env("PULSE_SERVER", &pulse_server)
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[audio] failed to start paplay: {e}");
                while audio_rx.recv().await.is_some() {}
                return;
            }
        };
        eprintln!("[audio] paplay started — playing PCM audio");
        // Drain paplay's stderr. A piped-but-unread stderr fills its 64 KiB
        // pipe after enough PulseAudio warnings; paplay then blocks on its next
        // write, stops reading stdin, and audio dies with the explanation stuck
        // inside the unread pipe.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("[audio] paplay: {line}");
                }
            });
        }
        let mut stdin = child.stdin.take().unwrap();
        while let Some(pcm) = audio_rx.recv().await {
            if stdin.write_all(&pcm).await.is_err() {
                break;
            }
        }
        let _ = child.kill().await;
    });

    // Race the protocol loop against the display. Whichever ends first ends the
    // session: a closed window / Escape stops the stream, and a mid-stream USB
    // failure stops the window.
    let stream_result: Result<()> = {
        let exited = display.exited_mut();
        tokio::select! {
            r = hs.stream_video_chan(&h264_tx, &audio_tx, &mut touch_rx) => r,
            d = exited => match d {
                // Display returned — Esc / window closed (Ok) or pipeline failed (Err).
                Ok(inner) => inner,
                // Sender dropped without a value: treat as a normal end of session.
                Err(_) => Ok(()),
            },
        }
    };

    // Tear the display down and reap ffmpeg. `stop()` is what keeps the join
    // bounded when the protocol side ended first and the window is still up;
    // dropping the H.264 sender unblocks the feed thread.
    display.stop();
    drop(h264_tx);
    display.join();

    // A failure here is a real failure: exiting 0 after "stream ended: USB
    // bulk read failed" misleads every CI wrapper and `&&` chain.
    if let Err(e) = stream_result {
        eprintln!("stream ended: {e:#}");
        return Err(e);
    }

    Ok(())
}
