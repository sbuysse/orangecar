pub mod apps;
pub mod fuel;
pub mod gps;
pub mod lyrics;
pub mod home_server;
pub mod headunit;
pub mod obd;
pub mod park_sync;
pub mod phone;
pub mod player;
pub mod resume;
pub mod calendar;
pub mod cell;
pub mod connections;
pub mod datausage;
pub mod weather;
pub mod media;
pub mod music;
pub mod navigation;
pub mod settings;
pub mod signin;
pub mod signal;
pub mod trip;
pub mod visualizer;
pub mod volume;
pub mod ytbridge;

/// Longest wait between two attempts while a service keeps failing.
const MAX_FAILURE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

/// Delay before the next refresh of a network-backed service: the normal
/// period after a success, else 10 s doubling up to 30 s (or the period if
/// shorter). At boot the network is often not up yet (and the car may have no
/// connection at all); waiting a full period after the first failure left the
/// calendar/weather empty for minutes.
pub fn refresh_delay(consecutive_failures: u32, period: std::time::Duration) -> std::time::Duration {
    if consecutive_failures == 0 {
        return period;
    }
    let backoff = std::time::Duration::from_secs(10u64.saturating_mul(1u64 << (consecutive_failures - 1).min(10)));
    backoff.min(MAX_FAILURE_BACKOFF).min(period)
}

/// Sleep until the next refresh is due — or, while failing, until the network
/// changes (a default route appears or moves), whichever comes first. On
/// On one boot the 4G came up at t and weather/calendar, deep in their
/// backoff, would only have retried at t+67 s: the dashboard said "offline"
/// for over a minute with a working link.
pub async fn wait_for_refresh(consecutive_failures: u32, period: std::time::Duration) {
    let delay = refresh_delay(consecutive_failures, period);
    if consecutive_failures == 0 {
        tokio::time::sleep(delay).await;
        return;
    }
    let mut net = net_changes();
    tokio::select! {
        _ = tokio::time::sleep(delay) => {}
        _ = net.changed() => {}
    }
}

/// Bumped every time the set of default routes changes. Started on first use.
pub fn net_changes() -> tokio::sync::watch::Receiver<u64> {
    static TX: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> = std::sync::OnceLock::new();
    TX.get_or_init(|| {
        let (tx, _) = tokio::sync::watch::channel(0u64);
        let watcher = tx.clone();
        tokio::spawn(async move {
            let mut last = String::new();
            loop {
                let now = std::fs::read_to_string("/proc/net/route").map(|t| default_routes(&t)).unwrap_or_default();
                if now != last {
                    tracing::info!("network: default routes now [{now}]");
                    last = now;
                    watcher.send_modify(|g| *g += 1);
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
        tx
    })
    .subscribe()
}

/// "iface gateway" of every UP IPv4 default route, sorted, from /proc/net/route.
fn default_routes(table: &str) -> String {
    let mut routes: Vec<String> = table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // Iface Destination Gateway Flags RefCnt Use Metric Mask …
            let up = f.len() >= 8 && u32::from_str_radix(f[3], 16).is_ok_and(|flags| flags & 0x1 != 0);
            (up && f[1] == "00000000" && f[7] == "00000000").then(|| format!("{} {}", f[0], f[2]))
        })
        .collect();
    routes.sort();
    routes.join(", ")
}

#[cfg(test)]
mod refresh_delay_tests {
    use super::refresh_delay;
    use std::time::Duration;

    #[test]
    fn backs_off_from_ten_seconds_up_to_thirty() {
        let p = Duration::from_secs(300);
        assert_eq!(refresh_delay(0, p), p);
        assert_eq!(refresh_delay(1, p), Duration::from_secs(10));
        assert_eq!(refresh_delay(2, p), Duration::from_secs(20));
        assert_eq!(refresh_delay(3, p), Duration::from_secs(30));
        assert_eq!(refresh_delay(99, p), Duration::from_secs(30));
        assert_eq!(refresh_delay(5, Duration::from_secs(15)), Duration::from_secs(15));
    }

    #[test]
    fn default_routes_lists_up_default_routes_only() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            wwu1u3i5\t00000000\t0200400A\t0003\t0\t0\t2048\t00000000\t0\t0\t0\n\
            enP3p49s0\t00000000\t0100000A\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
            enP3p49s0\t0000000A\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(super::default_routes(table), "enP3p49s0 0100000A, wwu1u3i5 0200400A");
        assert_eq!(super::default_routes("Iface\n"), "");
    }
}
