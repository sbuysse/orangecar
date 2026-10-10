//! Small helpers shared by the log shipper and the updater.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Output of a bounded external command.
#[derive(Debug)]
pub struct CmdOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Run a command with a hard deadline. The child is killed when the deadline
/// passes (kill_on_drop), so a wedged helper (systemctl against a stuck PID 1,
/// journalctl on a damaged file) can never hang the agent or the boot.
pub async fn run(program: &str, args: &[&str], timeout: Duration) -> Result<CmdOutput> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {program}"))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("{program} {} timed out after {timeout:?}", args.join(" ")))??;
    Ok(CmdOutput {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Write `data` to `path` so that a power cut leaves either the old or the
/// new content: temp file in the same directory, fsync, rename, fsync dir.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    fsync_dir(dir);
    Ok(())
}

/// Best effort: persist a directory entry change (rename, create, unlink).
pub fn fsync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        bail!("odd-length hex");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("not hex"))
        .collect()
}

/// Streaming SHA-256 of a reader.
pub fn sha256_reader(mut r: impl Read) -> Result<String> {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(hex(ctx.finish().as_ref()))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    sha256_reader(std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?)
}

pub fn sha256_bytes(data: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

/// First 8 hex digits of the kernel boot id.
pub fn boot_id_short() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().replace('-', "").chars().take(8).collect())
        .unwrap_or_else(|_| "unknown".into())
}

/// Full boot id without dashes (as journald writes it in `_BOOT_ID`).
pub fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().replace('-', ""))
        .unwrap_or_default()
}

pub fn uptime_secs() -> f64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
}

/// The car's head-unit record written by rascar-carplay (`headunit.json`:
/// name, firmware, serial, Bluetooth, USB power...), for the status report.
/// `None` when no CarPlay session has written it yet, or when it is oversized
/// or not a JSON object. The real file is ~600 bytes; the 8 KiB cap keeps a
/// bad file from pushing the whole report past the update server's 64 KiB status limit.
/// `RASCAR_HEADUNIT_PATH` overrides the path.
pub fn read_headunit() -> Option<serde_json::Value> {
    let path = std::env::var("RASCAR_HEADUNIT_PATH").unwrap_or_else(|_| "/var/lib/rascar/headunit.json".into());
    read_json_object(Path::new(&path), 8 * 1024)
}

fn read_json_object(path: &Path, max: u64) -> Option<serde_json::Value> {
    let mut buf = Vec::new();
    std::fs::File::open(path).ok()?.take(max + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > max {
        return None;
    }
    serde_json::from_slice::<serde_json::Value>(&buf).ok().filter(|v| v.is_object())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headunit_json_is_read_bounded_and_must_be_an_object() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("headunit.json");
        assert_eq!(read_json_object(&p, 64), None, "missing file");
        std::fs::write(&p, br#"{"name":"Jaguar F-PACE"}"#).unwrap();
        assert_eq!(read_json_object(&p, 64).unwrap()["name"], "Jaguar F-PACE");
        assert_eq!(read_json_object(&p, 8), None, "over the cap");
        std::fs::write(&p, b"[]").unwrap();
        assert_eq!(read_json_object(&p, 64), None);
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(hex(&[0, 1, 0xab, 0xff]), "0001abff");
        assert_eq!(unhex("0001ABff").unwrap(), vec![0, 1, 0xab, 0xff]);
        assert!(unhex("abc").is_err());
        assert!(unhex("zz").is_err());
    }

    #[test]
    fn sha256_of_abc_matches_the_fips_vector() {
        // FIPS 180-2, appendix B.1.
        assert_eq!(sha256_bytes(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn atomic_write_replaces_content() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1, "no temp file left behind");
    }

    #[tokio::test]
    async fn run_kills_a_command_that_overruns() {
        let t = std::time::Instant::now();
        let r = run("sleep", &["5"], Duration::from_millis(200)).await;
        assert!(r.is_err());
        assert!(t.elapsed() < Duration::from_secs(2));
        let ok = run("sh", &["-c", "echo hi; exit 3"], Duration::from_secs(5)).await.unwrap();
        assert_eq!((ok.status, ok.stdout.trim()), (3, "hi"));
    }
}
