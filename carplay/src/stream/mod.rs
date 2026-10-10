//! Media plumbing shared by the CarPlay session, plus the older AirPlay
//! screen-mirroring pipeline ([`run`]).
//!
//! Used by the CarPlay session (`airplay::carplay_session`):
//!   * [`render`]: puts the dashboard on a display sized to the car's screen
//!     (Xvfb + Chromium, or the cage Wayland kiosk with `RASCAR_COMPOSITOR=wayland`).
//!   * [`video`]: captures it and H.264-encodes it (baseline, IDR every 15 frames).
//!   * [`wlcapture`]: frame capture from the Wayland kiosk.
//!   * [`audio`], [`call`]: media capture, telephony and ringtone audio.
//!   * [`input`]: touch injection (uinput / XTest) and coordinate mapping.
//!   * [`crypto`], [`screen`], [`carkeys`]: stream ciphers, screen ownership,
//!     steering-wheel keys.
//!
//! [`run`] drives the Apple TV style mirroring session (`airplay::run`): it
//! renders, packetizes video in the AirPlay mirroring format, keeps `/feedback`
//! going, and has no negotiated audio or touch channel (audio no-ops, touch is
//! only logged unless `RASCAR_TOUCH_PARSER=experimental`).
//!
//! # Reuse map
//!
//! | piece            | source                                     |
//! |------------------|--------------------------------------------|
//! | ffmpeg capture   | project's earlier Android Auto bridge      |
//! | parec capture    | project's earlier Android Auto bridge      |
//! | uinput / XTest   | project's earlier Android Auto bridge      |
//! | mirror packets   | doubletake `mirror.go` (ported, LGPL-3.0)  |
//! | video cipher     | doubletake `client.go`/`mirror.go` (ported)|

pub mod audio;
pub mod call;
pub mod carkeys;
pub mod crypto;
pub mod input;
pub mod render;
pub mod screen;
pub mod video;
pub mod wlcapture;

use anyhow::Result;
use std::time::Duration;
use tracing::{error, info, warn};

use crate::airplay::{AirPlaySender, SessionHandle};
use crypto::VideoCipher;
use video::{VideoEncoder, VideoSettings};

/// Run the full streaming session until the video pipeline ends (encoder EOF,
/// write failure, or the data connection closing). Owns `sender` so the RTSP
/// control connection (and its `/feedback` keep-alive) stays open.
pub async fn run(mut sender: AirPlaySender, session: SessionHandle) -> Result<()> {
    // --- 1. Render prerequisite ---
    // The AppleTV/doubletake path renders at the fixed CarPlay minimum surface;
    // ensure_dashboard reports the effective capture geometry (its own Xvfb, or
    // a pre-existing kiosk's actual root size).
    let (_render_guard, cap_w, cap_h) =
        render::ensure_dashboard(render::WIDTH, render::HEIGHT).await?;

    // Sizes.
    let content_size = (cap_w, cap_h); // what ffmpeg encodes
    let dash_res = (cap_w, cap_h); // our injectable surface
    let display_size = session.display.unwrap_or(content_size); // receiver display
    let car_touch_res = session.display.unwrap_or(display_size); // car touchscreen

    info!(
        "stream: session ready — data_port={} cipher-inputs(sci={}, enc={}, secret={}B) display={:?}",
        session.data_port,
        session.stream_connection_id,
        session.channel_encrypted,
        session.shared_secret.len(),
        session.display,
    );

    // --- Video cipher (matches what setup_streams negotiated) ---
    let cipher = VideoCipher::from_session(
        &session.stream_key,
        session.channel_encrypted,
        &session.shared_secret,
        session.stream_connection_id,
    );

    // --- Split the data connection: video WRITES, input READS the reverse dir ---
    let (rd, wr) = session.data_conn.into_split();

    // --- 2. Video encoder ---
    let mut encoder = VideoEncoder::new(VideoSettings {
        display: render::x_display(),
        width: render::WIDTH,
        height: render::HEIGHT,
        ..Default::default()
    });
    encoder.start().await?;

    info!("**** STREAMING — dashboard on car screen ****");

    // --- 5. /feedback keep-alive ---
    let feedback = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.tick().await; // consume the immediate first tick
        loop {
            tick.tick().await;
            if let Err(e) = sender.feedback().await {
                warn!("stream: /feedback keep-alive failed: {e:#} — stopping keep-alive");
                break;
            }
        }
    });

    // --- 3. Audio (gated) ---
    let audio = tokio::spawn(audio::run(None));

    // --- 4. Reverse touch ---
    let input = tokio::spawn(input::run(
        rd,
        render::x_display(),
        car_touch_res,
        dash_res,
    ));

    // --- 2. Video push (primary; its end ends the session) ---
    let video = tokio::spawn(video::stream_to(encoder, wr, cipher, content_size, display_size));

    let result = match video.await {
        Ok(Ok(())) => {
            // Only reachable now when ffmpeg exited 0 having actually sent
            // frames; a failed capture surfaces as Err below.
            info!("stream: video pipeline ended cleanly (encoder exited 0 after sending frames)");
            Ok(())
        }
        Ok(Err(e)) => {
            error!("stream: video pipeline error: {e:#}");
            Err(e)
        }
        Err(join) => {
            error!("stream: video task panicked/cancelled: {join}");
            Err(anyhow::anyhow!("video task join error: {join}"))
        }
    };

    // Tear down the side tasks; render guard drops (kills Xvfb/Chromium) on return.
    feedback.abort();
    audio.abort();
    input.abort();
    info!("stream: session torn down");
    result
}
