//! NTP 32.32 fixed-point timestamps, as carried by AirPlay time-sync packets
//! and screen-frame presentation times.
//!
//! The upper 32 bits count whole seconds since 1900-01-01, the lower 32 bits
//! are the fraction of a second in units of 2^-32 s (RFC 5905 §6). Seconds wrap
//! modulo 2^32 like any NTP era.

use std::time::Duration;

/// Distance from the NTP epoch (1900-01-01) to the Unix epoch (1970-01-01).
pub const NTP_TO_UNIX: Duration = Duration::from_secs(2_208_988_800);

/// Encode `since_ntp_epoch` as a 32.32 timestamp. The fraction is truncated
/// (rounded toward zero), never rounded up into the next second.
pub fn encode(since_ntp_epoch: Duration) -> u64 {
    let whole = since_ntp_epoch.as_secs() << 32;
    // subsec_nanos < 10^9, so nanos * 2^32 < 2^62: no overflow in u64.
    let fraction = (u64::from(since_ntp_epoch.subsec_nanos()) << 32) / 1_000_000_000;
    whole | fraction
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_and_fractional_seconds() {
        assert_eq!(encode(Duration::ZERO), 0);
        assert_eq!(encode(Duration::from_millis(1500)), 0x0000_0001_8000_0000);
        assert_eq!(encode(Duration::from_nanos(123_456_789)), 0x0000_0000_1f9a_dd37);
        assert_eq!(encode(Duration::from_nanos(999_999_999)) >> 32, 0, "never rounds up");
    }

    #[test]
    fn unix_epoch_offset() {
        assert_eq!(encode(NTP_TO_UNIX + Duration::from_millis(250)), 0x83aa_7e80_4000_0000);
    }

    #[test]
    fn seconds_wrap_at_the_era_boundary() {
        assert_eq!(encode(Duration::from_secs((1 << 32) + 3)), 3 << 32);
    }
}
