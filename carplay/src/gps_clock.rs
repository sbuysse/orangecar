//! System clock from the car's GPS.
//!
//! The Pi has no RTC and no internet in the car, so every trip boots with the
//! time `fake-hwclock` last saved — hours or days old. The car sends `$GPRMC`
//! once a second over iAP2 (date + UTC time, status `A` when the fix is valid),
//! so this steps the system clock to it. NTP always wins: when
//! systemd-timesyncd reports synchronisation (e.g. on Wi-Fi) nothing is touched.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// Only step the clock when it is further off than this.
const MAX_DRIFT: Duration = Duration::from_secs(2);
/// After a successful set, look again at most this often.
const RECHECK_EVERY: Duration = Duration::from_secs(60);
/// systemd-timesyncd creates this file once NTP has synchronised the clock.
const NTP_SYNCED_FLAG: &str = "/run/systemd/timesync/synchronized";

static LAST_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

/// UTC seconds since the epoch from a valid `$..RMC` sentence, else None.
pub fn rmc_epoch(sentence: &str) -> Option<i64> {
    let body = sentence.split('*').next()?;
    let f: Vec<&str> = body.split(',').collect();
    if f.len() < 10 || !f[0].ends_with("RMC") || f[2] != "A" {
        return None;
    }
    let (t, d) = (f[1], f[9]);
    if t.len() < 6 || d.len() != 6 {
        return None;
    }
    let num = |s: &str| s.parse::<i64>().ok();
    let (hh, mm, ss) = (num(&t[0..2])?, num(&t[2..4])?, num(&t[4..6])?);
    let (day, month, yy) = (num(&d[0..2])?, num(&d[2..4])?, num(&d[4..6])?);
    if hh > 23 || mm > 59 || ss > 60 || !(1..=31).contains(&day) || !(1..=12).contains(&month) {
        return None;
    }
    // Two-digit year: GPS receivers of this era report 20yy.
    let year = 2000 + yy;
    Some(days_from_civil(year, month, day) * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Feed every car NMEA sentence here; steps the clock from `$..RMC` when needed.
pub fn on_car_nmea(sentence: &str) {
    let Some(gps) = rmc_epoch(sentence) else { return };
    {
        let mut last = LAST_CHECK.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_some_and(|t| t.elapsed() < RECHECK_EVERY) {
            return;
        }
        *last = Some(Instant::now());
    }
    if std::path::Path::new(NTP_SYNCED_FLAG).exists() {
        return; // NTP has the clock; the car's GPS is only the fallback.
    }
    // Sanity: a fix years away from the build is a receiver fault, not the time.
    if !(1_735_689_600..4_102_444_800).contains(&gps) {
        warn!("car clock: ignoring implausible GPS time {gps} ({sentence})");
        return;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let drift = gps - now;
    if drift.unsigned_abs() <= MAX_DRIFT.as_secs() {
        return;
    }
    let ts = libc::timespec { tv_sec: gps as libc::time_t, tv_nsec: 0 };
    // SAFETY: plain syscall on a stack value; needs CAP_SYS_TIME (we run as root).
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) } != 0 {
        warn!("car clock: clock_settime failed: {}", std::io::Error::last_os_error());
        return;
    }
    info!("car clock: system time stepped by {drift:+} s to GPS time ({})", &sentence[..sentence.len().min(40)]);
    // Persist it so the next power-up (no clean shutdown in the car) starts close.
    match std::process::Command::new("fake-hwclock").arg("save").status() {
        Ok(s) if s.success() => {}
        Ok(s) => warn!("car clock: fake-hwclock save exited {s}"),
        Err(e) => warn!("car clock: fake-hwclock save failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_cars_rmc() {
        // Sentence captured from the car (position moved to a neutral place).
        let s = "$GPRMC,120000.00,A,4851.402000,N,00221.102000,E,13.510,329.80,270926,,,E*5B";
        assert_eq!(rmc_epoch(s), Some(1_790_510_400));
    }

    #[test]
    fn rejects_invalid_fixes_and_other_sentences() {
        assert_eq!(rmc_epoch("$GPRMC,120000.00,V,,,,,,,270926,,,N*00"), None);
        assert_eq!(rmc_epoch("$GPGGA,120000.00,4851.402000,N,00221.102000,E,6,17,0.6,35.0,M,44.8,M,,*5A"), None);
        assert_eq!(rmc_epoch("$GPRMC,120000.00,A,4851.4,N,00221.1,E,13.5,329.8,321326,,,E*00"), None);
    }

    #[test]
    fn epoch_arithmetic_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
    }
}
