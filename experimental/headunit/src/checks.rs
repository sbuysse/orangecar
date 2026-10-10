#[derive(Debug)]
pub struct CheckResult {
    pub name: &'static str,
    pub passed: bool,
    pub detail: Option<String>,
    pub last_sent: Option<Vec<u8>>,
    pub last_received: Option<Vec<u8>>,
}

impl CheckResult {
    /// A check with no associated wire traffic (environment probes, summaries).
    pub fn plain(name: &'static str, passed: bool, detail: Option<String>) -> Self {
        Self { name, passed, detail, last_sent: None, last_received: None }
    }
}

/// Resolve an executable name against `$PATH`, like `which(1)`.
/// Returns the first match, or `None` if nothing executable was found.
pub fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Environment probes run before any USB or protocol work.
///
/// Everything checked here is spawned unconditionally later in the run
/// (`ffmpeg` and a window in `display`, `paplay` in `main`), so a missing piece
/// used to surface as a hung process with no window rather than a clear error.
pub fn preflight() -> Vec<CheckResult> {
    let mut out = Vec::new();

    out.push(match which("ffmpeg") {
        Some(p) => CheckResult::plain("ffmpeg present", true, Some(p.display().to_string())),
        None => CheckResult::plain(
            "ffmpeg present",
            false,
            Some("not found in $PATH — required to decode the H.264 video stream".into()),
        ),
    });

    out.push(match which("paplay") {
        Some(p) => CheckResult::plain("paplay present", true, Some(p.display().to_string())),
        None => CheckResult::plain(
            "paplay present",
            false,
            Some("not found in $PATH — install pulseaudio-utils (or pipewire-pulse)".into()),
        ),
    });

    let wayland = std::env::var("WAYLAND_DISPLAY").ok().filter(|v| !v.is_empty());
    let x11 = std::env::var("DISPLAY").ok().filter(|v| !v.is_empty());
    out.push(match (wayland, x11) {
        (Some(w), _) => CheckResult::plain("display server", true, Some(format!("wayland ({w})"))),
        (None, Some(d)) => CheckResult::plain("display server", true, Some(format!("x11 ({d})"))),
        (None, None) => CheckResult::plain(
            "display server",
            false,
            Some(
                "neither $WAYLAND_DISPLAY nor $DISPLAY is set — the video window cannot open \
                 (running under sudo? pass the variables through)"
                    .into(),
            ),
        ),
    });

    out
}

pub fn print_result(r: &CheckResult) {
    if r.passed {
        match &r.detail {
            Some(d) => println!("[PASS] {:<28} ({})", r.name, d),
            None => println!("[PASS] {}", r.name),
        }
    } else {
        let detail = r.detail.as_deref().unwrap_or("unknown error");
        eprintln!("[FAIL] {} — {}", r.name, detail);
        if let Some(sent) = &r.last_sent {
            eprintln!("  sent:     {}", hex_dump(sent));
        }
        if let Some(recv) = &r.last_received {
            eprintln!("  received: {}", hex_dump(recv));
        }
    }
}

pub fn print_summary(results: &[CheckResult]) {
    let passed = results.iter().filter(|r| r.passed).count();
    println!("{}", "─".repeat(42));
    println!("RESULT: {}/{} passed", passed, results.len());
}

pub fn hex_dump(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_dump_formats_correctly() {
        let data = vec![0xde, 0xad, 0xbe, 0xef];
        assert_eq!(hex_dump(&data), "de ad be ef");
    }

    #[test]
    fn which_resolves_an_executable_on_path() {
        // `sh` is required to exist by POSIX on every platform this runs on.
        let found = which("sh").expect("sh must be resolvable via $PATH");
        assert!(found.is_absolute(), "which returned a relative path: {found:?}");
        assert!(found.ends_with("sh"));
    }

    #[test]
    fn which_returns_none_for_missing_program() {
        assert!(which("rascar-definitely-not-a-real-program").is_none());
    }

    #[test]
    fn preflight_names_every_probed_dependency() {
        // The set of probes is the contract: each one guards something main
        // spawns unconditionally. Losing one silently reintroduces a hang.
        let names: Vec<&str> = preflight().iter().map(|r| r.name).collect();
        assert!(names.contains(&"ffmpeg present"), "got {names:?}");
        assert!(names.contains(&"paplay present"), "got {names:?}");
        assert!(names.contains(&"display server"), "got {names:?}");
    }

    #[test]
    fn failed_check_reports_detail_not_unknown_error() {
        let r = CheckResult::plain("probe", false, Some("boom".into()));
        assert!(!r.passed);
        assert_eq!(r.detail.as_deref(), Some("boom"));
        assert!(r.last_sent.is_none());
    }
}
