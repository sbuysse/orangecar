//! Wi-Fi and Bluetooth for the Settings ▸ Connections dialog.
//!
//! Wi-Fi goes through wpa_supplicant's control socket (`wpa_cli`). The socket
//! is root-only unless the config says `ctrl_interface=DIR=... GROUP=netdev`,
//! so when a direct call is refused it is retried through `sudo -n` (the
//! dashboard user has passwordless sudo on the Pi). Bluetooth goes through `bluetoothctl`
//! (BlueZ over D-Bus). Choosing the OBD adapter only records the choice in
//! settings.json: the root service `rascar-obd-link` does the pairing (legacy
//! PIN) and binds `/dev/rfcomm0`, and follows that choice.

use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

const WPA_CLI: &str = "/usr/sbin/wpa_cli";

async fn run(program: &str, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
    Ok(run_status(program, args, timeout).await?.1)
}

async fn run_status(program: &str, args: &[&str], timeout: Duration) -> anyhow::Result<(bool, String)> {
    let out = tokio::time::timeout(timeout, Command::new(program).args(args).kill_on_drop(true).output())
        .await
        .map_err(|_| anyhow::anyhow!("{program} timed out"))??;
    Ok((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
}

// --- Wi-Fi -------------------------------------------------------------

/// The wireless interface (the first one with a `wireless` dir in sysfs).
pub fn wifi_iface() -> Option<String> {
    std::fs::read_dir("/sys/class/net").ok()?.flatten().find_map(|e| {
        e.path().join("wireless").exists().then(|| e.file_name().to_string_lossy().into_owned())
    })
}

async fn wpa(args: &[&str]) -> anyhow::Result<String> {
    let iface = wifi_iface().ok_or_else(|| anyhow::anyhow!("no Wi-Fi interface"))?;
    let mut full = vec![WPA_CLI, "-i", iface.as_str()];
    full.extend_from_slice(args);
    let t = Duration::from_secs(8);
    // wpa_cli exits 255 when it cannot open the control socket.
    let mut r = run_status(WPA_CLI, &full[1..], t).await?;
    if !r.0 {
        let mut sudo = vec!["-n"];
        sudo.extend_from_slice(&full);
        r = run_status("sudo", &sudo, t).await?;
    }
    match r {
        (true, out) if !out.trim().starts_with("FAIL") => Ok(out),
        (_, out) => anyhow::bail!("wpa_supplicant refused {:?}: {}", args.first().unwrap_or(&""), out.trim()),
    }
}

#[derive(Debug, Serialize, Default)]
pub struct WifiStatus {
    pub state: String,
    pub ssid: Option<String>,
    pub ip: Option<String>,
    pub signal_dbm: Option<i32>,
}

pub async fn wifi_status() -> anyhow::Result<WifiStatus> {
    let s = wpa(&["status"]).await?;
    let kv = |k: &str| s.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(str::to_string);
    let mut st = WifiStatus { state: kv("wpa_state").unwrap_or_default(), ssid: kv("ssid"), ip: kv("ip_address"), signal_dbm: None };
    if st.state == "COMPLETED" {
        if let Ok(p) = wpa(&["signal_poll"]).await {
            st.signal_dbm = p.lines().find_map(|l| l.strip_prefix("RSSI=")).and_then(|v| v.trim().parse().ok());
        }
    }
    Ok(st)
}

#[derive(Debug, Serialize, Clone)]
pub struct WifiNetwork {
    pub ssid: String,
    pub signal_dbm: i32,
    pub secure: bool,
    /// Id in wpa_supplicant when this network is saved.
    pub saved_id: Option<u32>,
    pub current: bool,
}

#[derive(Debug, Clone)]
struct Saved {
    id: u32,
    ssid: String,
    current: bool,
}

async fn saved_networks() -> anyhow::Result<Vec<Saved>> {
    let out = wpa(&["list_networks"]).await?;
    Ok(out
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            Some(Saved { id: f.first()?.trim().parse().ok()?, ssid: f.get(1)?.to_string(), current: l.contains("[CURRENT]") })
        })
        .collect())
}

/// Visible networks (strongest per name) merged with the saved ones.
pub async fn wifi_scan() -> anyhow::Result<Vec<WifiNetwork>> {
    // FAIL-BUSY: a scan is already running; its results will do.
    let _ = wpa(&["scan"]).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let results = wpa(&["scan_results"]).await?;
    let saved = saved_networks().await.unwrap_or_default();
    let mut nets: Vec<WifiNetwork> = Vec::new();
    for l in results.lines().skip(1) {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() < 5 || f[4].is_empty() {
            continue; // hidden networks have no name to show
        }
        let (ssid, signal, flags) = (f[4].to_string(), f[2].trim().parse().unwrap_or(-100), f[3]);
        let secure = flags.contains("WPA") || flags.contains("WEP") || flags.contains("SAE");
        match nets.iter_mut().find(|n| n.ssid == ssid) {
            Some(n) => n.signal_dbm = n.signal_dbm.max(signal),
            None => {
                let s = saved.iter().find(|s| s.ssid == ssid);
                nets.push(WifiNetwork { ssid, signal_dbm: signal, secure, saved_id: s.map(|s| s.id), current: s.is_some_and(|s| s.current) })
            }
        }
    }
    // Saved but out of range: still listed (to forget them).
    for s in &saved {
        if !nets.iter().any(|n| n.ssid == s.ssid) {
            nets.push(WifiNetwork { ssid: s.ssid.clone(), signal_dbm: -127, secure: true, saved_id: Some(s.id), current: s.current });
        }
    }
    nets.sort_by(|a, b| b.current.cmp(&a.current).then(b.signal_dbm.cmp(&a.signal_dbm)));
    Ok(nets)
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", s)
}

/// Connect now to `ssid` (saved, or added with `password`; open if None).
pub async fn wifi_connect(ssid: &str, password: Option<&str>) -> anyhow::Result<()> {
    if ssid.is_empty() || ssid.len() > 32 || ssid.contains(['"', '\n', '\t']) {
        anyhow::bail!("invalid network name");
    }
    if let Some(p) = password {
        if !(8..=63).contains(&p.len()) || p.contains(['"', '\n']) {
            anyhow::bail!("the password must be 8 to 63 characters");
        }
    }
    let saved = saved_networks().await?;
    let id = match saved.iter().find(|s| s.ssid == ssid) {
        Some(s) if password.is_none() => s.id,
        existing => {
            let id: u32 = match existing {
                Some(s) => s.id,
                None => wpa(&["add_network"]).await?.trim().lines().last().unwrap_or("").parse().map_err(|_| anyhow::anyhow!("add_network failed"))?,
            };
            let ids = id.to_string();
            wpa(&["set_network", &ids, "ssid", &quoted(ssid)]).await?;
            match password {
                Some(p) => wpa(&["set_network", &ids, "psk", &quoted(p)]).await?,
                None => wpa(&["set_network", &ids, "key_mgmt", "NONE"]).await?,
            };
            id
        }
    };
    let ids = id.to_string();
    wpa(&["enable_network", &ids]).await?;
    // select_network connects now but disables every other network; re-enable
    // them afterwards so the car still roams to the other saved networks later.
    wpa(&["select_network", &ids]).await?;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let _ = wpa(&["enable_network", "all"]).await;
        let _ = wpa(&["save_config"]).await;
    });
    let _ = wpa(&["save_config"]).await;
    Ok(())
}

/// Drop and redo the association (the "slow to connect" case).
pub async fn wifi_retry() -> anyhow::Result<()> {
    wpa(&["enable_network", "all"]).await?;
    wpa(&["reassociate"]).await?;
    Ok(())
}

pub async fn wifi_forget(id: u32) -> anyhow::Result<()> {
    let ids = id.to_string();
    wpa(&["remove_network", &ids]).await?;
    wpa(&["save_config"]).await?;
    Ok(())
}

// --- Bluetooth ---------------------------------------------------------

#[derive(Debug, Serialize, Clone)]
pub struct BtDevice {
    pub mac: String,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
    pub rssi: Option<i32>,
    pub icon: String,
}

pub fn valid_mac(m: &str) -> bool {
    let p: Vec<&str> = m.split(':').collect();
    p.len() == 6 && p.iter().all(|x| x.len() == 2 && x.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub async fn bt_scan(seconds: u64) -> anyhow::Result<()> {
    let _ = run("bluetoothctl", &["power", "on"], Duration::from_secs(5)).await;
    run("bluetoothctl", &["--timeout", &seconds.to_string(), "scan", "on"], Duration::from_secs(seconds + 5)).await?;
    Ok(())
}

pub async fn bt_devices() -> anyhow::Result<Vec<BtDevice>> {
    let list = run("bluetoothctl", &["devices"], Duration::from_secs(5)).await?;
    let mut out = Vec::new();
    for l in list.lines().take(40) {
        let mut f = l.splitn(3, ' ');
        let (Some("Device"), Some(mac)) = (f.next(), f.next()) else { continue };
        if !valid_mac(mac) {
            continue;
        }
        let info = run("bluetoothctl", &["info", mac], Duration::from_secs(4)).await.unwrap_or_default();
        let field = |k: &str| info.lines().find_map(|l| l.trim().strip_prefix(&format!("{k}: "))).map(str::to_string);
        let name = field("Name").or_else(|| f.next().map(str::to_string)).unwrap_or_default();
        // "RSSI: 0xffffffc4 (-60)" or "RSSI: -60"
        let rssi = field("RSSI").and_then(|v| {
            v.rsplit_once('(').map(|(_, r)| r.trim_end_matches(')').to_string()).unwrap_or(v).trim().parse().ok()
        });
        out.push(BtDevice {
            mac: mac.to_string(),
            name,
            paired: field("Paired").as_deref() == Some("yes"),
            connected: field("Connected").as_deref() == Some("yes"),
            rssi,
            icon: field("Icon").unwrap_or_default(),
        });
    }
    // Named, nearby, paired first; nameless beacons last.
    out.sort_by(|a, b| {
        b.paired.cmp(&a.paired)
            .then(a.name.is_empty().cmp(&b.name.is_empty()))
            .then(b.rssi.unwrap_or(-127).cmp(&a.rssi.unwrap_or(-127)))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_are_validated() {
        assert!(valid_mac("00:1D:A5:00:00:01"));
        assert!(!valid_mac("00:1D:A5:00:00"));
        assert!(!valid_mac("00:1D:A5:00:00:ZZ"));
        assert!(!valid_mac("$(reboot)"));
    }
}
