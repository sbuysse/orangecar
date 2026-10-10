//! Video capture and H.264 encoding for Android Auto.
//!
//! Captures the Pi's display (X11 or framebuffer) and encodes as H.264
//! Baseline Profile for transmission to the car's head unit.
//!
//! AA video requirements:
//!   - H.264 Baseline Profile
//!   - 800x480 @ 30fps minimum
//!   - Max bitrate: 4Mbps (480p), 6Mbps (720p), 8Mbps (1080p)
//!   - I-frames + P-frames only, no B-frames
//!
//! Uses ffmpeg subprocess for capture + encode pipeline.

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tracing::info;

#[derive(Debug, Clone)]
pub struct VideoSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub keyframe_interval: u32,
    /// X11 display (e.g. ":0") or "fb" for framebuffer.
    pub display: String,
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            width: 800,
            height: 480,
            fps: 30,
            bitrate_kbps: 3000,
            keyframe_interval: 30,
            display: ":0".into(),
        }
    }
}

pub struct VideoEncoder {
    settings: VideoSettings,
    process: Option<Child>,
}

impl VideoEncoder {
    pub fn new(settings: VideoSettings) -> Self {
        Self {
            settings,
            process: None,
        }
    }

    fn build_args(&self) -> Vec<String> {
        let s = &self.settings;
        let use_fb = s.display == "fb";
        let use_test = s.display == "test";

        let mut args: Vec<String> = vec![
            "-loglevel".into(), "warning".into(),
        ];

        if use_test {
            // lavfi test pattern — no display required
            args.extend([
                "-f".into(), "lavfi".into(),
                "-i".into(), format!("testsrc2=size={}x{}:rate={}", s.width, s.height, s.fps),
            ]);
        } else if use_fb {
            args.extend([
                "-f".into(), "rawvideo".into(),
                "-pixel_format".into(), "bgra".into(),
                "-video_size".into(), format!("{}x{}", s.width, s.height),
                "-framerate".into(), s.fps.to_string(),
                "-i".into(), "/dev/fb0".into(),
            ]);
        } else {
            args.extend([
                "-f".into(), "x11grab".into(),
                "-framerate".into(), s.fps.to_string(),
                "-video_size".into(), format!("{}x{}", s.width, s.height),
                "-i".into(), s.display.clone(),
            ]);
        }

        args.extend([
            "-c:v".into(), "libx264".into(),
            "-pix_fmt".into(), "yuv420p".into(),
            "-profile:v".into(), "baseline".into(),
            "-level:v".into(), "3.1".into(),
            "-preset".into(), "ultrafast".into(),
            "-tune".into(), "zerolatency".into(),
            "-b:v".into(), format!("{}k", s.bitrate_kbps),
            "-maxrate".into(), format!("{}k", s.bitrate_kbps),
            "-bufsize".into(), format!("{}k", s.bitrate_kbps / 2),
            "-g".into(), s.keyframe_interval.to_string(),
            "-keyint_min".into(), s.keyframe_interval.to_string(),
            "-sc_threshold".into(), "0".into(),
            "-bf".into(), "0".into(),
            "-x264-params".into(), "repeat-headers=1".into(),
            "-f".into(), "h264".into(),
            "-an".into(),
            "pipe:1".into(),
        ]);

        args
    }

    /// Start the ffmpeg capture/encode pipeline.
    pub async fn start(&mut self) -> Result<()> {
        let args = self.build_args();
        info!("starting video encoder: ffmpeg {} ...", args[..6].join(" "));

        // Pass DISPLAY explicitly so ffmpeg x11grab works even when the bridge
        // runs as root (which may not inherit DISPLAY from the user session).
        // It MUST come from `settings.display` (the --display flag), not from the
        // process environment: input injection uses the same value, and reading
        // $DISPLAY here would capture one screen while touches went to another.
        // "fb" / "test" are not X11 displays, so fall back to the environment.
        let display = match self.settings.display.as_str() {
            "fb" | "test" => std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into()),
            disp => disp.to_string(),
        };
        let child = Command::new("ffmpeg")
            .args(&args)
            .env("DISPLAY", &display)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit()) // surface ffmpeg errors in journal
            .spawn()
            .context("spawning ffmpeg")?;

        info!("video encoder started (pid={})", child.id().unwrap_or(0));
        self.process = Some(child);
        Ok(())
    }

    /// Read a chunk of H.264 data from the encoder.
    pub async fn read_chunk(&mut self, buf: &mut [u8]) -> Result<usize> {
        let child = self.process.as_mut().context("encoder not started")?;
        let stdout = child.stdout.as_mut().context("no stdout")?;
        let n = stdout.read(buf).await?;
        Ok(n)
    }

    /// Stop the encoder.
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
            info!("video encoder stopped");
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.start_kill();
        }
    }
}
