//! rascar-agent — log push + OTA updates for the car (see the crate docs).
//!
//! ```text
//! rascar-agent [run]        the daemon (rascar-agent.service)
//! rascar-agent apply-staged switch to a staged release; called at boot by rascar-ota-boot
//! rascar-agent apply-now    apply a staged release live: switch, restart, judge (bench use)
//! rascar-agent check-now    ask the update server for an update and stage it now
//! rascar-agent status       print the local state
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rascar_agent::config::AgentConfig;
use rascar_agent::server::{Req, UpdateServer};
use rascar_agent::{agent_version, logship, ota, util};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

#[derive(Parser)]
#[command(version, about = "OrangeCar log push + OTA agent")]
struct Cli {
    #[arg(long, default_value = rascar_agent::config::DEFAULT_CONFIG)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    Run,
    ApplyStaged,
    ApplyNow,
    CheckNow,
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .without_time() // journald stamps every line
        .init();
    let cli = Cli::parse();
    let cfg = AgentConfig::load(&cli.config)?;
    std::fs::create_dir_all(&cfg.state_dir)?;
    match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::Run => run(cfg).await,
        Cmd::ApplyStaged => {
            // Boot path: never fail the unit, the services must start anyway.
            match ota::apply_staged(&cfg).await {
                Ok(Some(a)) => info!(version = %a.version, "boot: release applied"),
                Ok(None) => info!("boot: nothing staged"),
                Err(e) => warn!(error = %e, "boot: apply failed; staying on the current release"),
            }
            Ok(())
        }
        Cmd::ApplyNow => apply_now(&cfg).await,
        Cmd::CheckNow => {
            let g = UpdateServer::load(&cfg.server_json)
                .map_err(|e| anyhow::anyhow!("no usable update server ({e:#}); see docs/ota.md"))?;
            let mut st = ota::load_state(&cfg.state_dir);
            let r = ota::check_and_stage(&cfg, &g, &mut st, logship::uplink_is_metered(&cfg.metered_drivers)).await;
            ota::save_state(&cfg.state_dir, &st)?;
            println!("{:?}", r?);
            Ok(())
        }
        Cmd::Status => {
            println!("{}", serde_json::to_string_pretty(&status_report(&cfg, None).await)?);
            Ok(())
        }
    }
}

async fn apply_now(cfg: &AgentConfig) -> Result<()> {
    let Some(a) = ota::apply_staged(cfg).await? else {
        println!("nothing staged");
        return Ok(());
    };
    let trial = ota::read_trial(&cfg.state_dir).ok_or_else(|| anyhow::anyhow!("trial file missing after apply"))?;
    let m = ota::release_manifest(cfg, &trial.release)?;
    ota::restart_units(&ota::units_to_restart(&m)).await;
    let r = ota::judge(cfg, &trial).await?;
    println!("{}: {} ({})", a.version, r.result, r.detail);
    Ok(())
}

/// The status the car reports to the update server.
async fn status_report(cfg: &AgentConfig, upload: Option<&logship::UploadReport>) -> serde_json::Value {
    let (version, serial) = ota::installed(cfg);
    let st = ota::load_state(&cfg.state_dir);
    let trial = ota::read_trial(&cfg.state_dir);
    let state = if trial.is_some() {
        "trial"
    } else if st.staged.is_some() {
        "staged"
    } else {
        match st.last_update.as_ref().map(|u| u.result.as_str()) {
            Some("rolled_back") => "rolled_back",
            Some("failed") => "failed",
            _ => "ok",
        }
    };
    serde_json::json!({
        "agent_version": agent_version(),
        "version": version,
        "serial": serial,
        "state": state,
        "staged": st.staged,
        "trial": trial.map(|t| serde_json::json!({ "version": t.version, "boots": t.boots })),
        "last_update": st.last_update,
        "bad_versions": ota::bad_versions(&cfg.state_dir),
        "boot_id": util::boot_id_short(),
        "uptime_secs": util::uptime_secs() as u64,
        "metered": logship::uplink_is_metered(&cfg.metered_drivers),
        "log_backlog_bytes": upload.map(|u| u.backlog),
        "detail": st.last_update.as_ref().map(|u| u.detail.clone()),
        // The car's head unit (rascar-carplay's headunit.json), when known.
        "headunit": util::read_headunit(),
    })
}

async fn post_status(cfg: &AgentConfig, g: &UpdateServer, upload: Option<&logship::UploadReport>) {
    let body = serde_json::to_vec(&status_report(cfg, upload).await).unwrap_or_default();
    let req = Req::post("/api/carops/status", body, Duration::from_secs(20)).header("Content-Type", "application/json");
    match g.send(&req).await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => warn!(status = r.status().as_u16(), "status report refused"),
        Err(e) => info!(error = %e, "status report: update server unreachable"),
    }
}

/// Wait until the update server is configured. The file can appear after the
/// agent started (first install), so it is looked for again every 5 minutes;
/// a missing file is the normal "no server" setup and is only logged once.
async fn update_server(cfg: &AgentConfig) -> Arc<UpdateServer> {
    let mut said = false;
    loop {
        if cfg.server_json.exists() {
            match UpdateServer::load(&cfg.server_json) {
                Ok(g) => return Arc::new(g),
                Err(e) => warn!(error = %format!("{e:#}"), "update server settings unusable; retrying in 5 min"),
            }
        } else if !said {
            info!(file = %cfg.server_json.display(), "no update server configured: log push and OTA stay off (see docs/ota.md)");
            said = true;
        }
        tokio::time::sleep(Duration::from_secs(300)).await;
    }
}

async fn run(cfg: AgentConfig) -> Result<()> {
    info!(version = %agent_version(), "rascar-agent starting");
    let cfg = Arc::new(cfg);
    let logs = Arc::new(Mutex::new(logship::load_state(&cfg.state_dir)));
    logship::repair_spool(&cfg, &*logs.lock().await)?;

    // 1. Pending trial: judge it once the services had time to start. Local
    //    only, so it runs whether or not a server is reachable.
    if let Some(t) = ota::read_trial(&cfg.state_dir) {
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let current = std::fs::canonicalize(&cfg.ota.install_link).ok();
            match ota::trial_action(Some(&t), current.as_deref(), cfg.ota.max_trial_boots) {
                ota::TrialAction::Judge => {
                    tokio::time::sleep(Duration::from_secs(cfg.ota.health_delay_secs)).await;
                    match ota::judge(&cfg, &t).await {
                        Ok(r) => info!(version = %r.version, result = %r.result, detail = %r.detail, "trial judged"),
                        Err(e) => warn!(error = %e, "trial judgement failed"),
                    }
                }
                ota::TrialAction::Forget => {
                    warn!(version = %t.version, "trial release is no longer current; forgetting the trial");
                    let _ = std::fs::remove_file(cfg.state_dir.join(ota::TRIAL_FILE));
                }
                ota::TrialAction::Rollback => {
                    if let Err(e) = ota::rollback(&cfg, &t, "too many boots without a successful health check").await {
                        warn!(error = %e, "rollback failed");
                    }
                }
                ota::TrialAction::None => {}
            }
        });
    }
    // The boot guard rolled back on its own: record it.
    let guard_note = cfg.state_dir.join(ota::GUARD_ROLLBACK_FILE);
    if let Ok(v) = std::fs::read_to_string(&guard_note) {
        let mut st = ota::load_state(&cfg.state_dir);
        st.last_update = Some(ota::UpdateResult::new(v.trim(), "rolled_back", "boot guard: no successful health check within the boot budget"));
        ota::save_state(&cfg.state_dir, &st)?;
        let _ = std::fs::remove_file(&guard_note);
    }

    let g = update_server(&cfg).await;

    // 2. Journal → spool: local only, runs even with no network at all (but
    //    only once a server is configured: nobody would read the spool).
    if cfg.journal.enabled {
        let (cfg, logs) = (cfg.clone(), logs.clone());
        tokio::spawn(async move {
            loop {
                {
                    let mut st = logs.lock().await;
                    match logship::collect_journal(&cfg, &mut st).await {
                        Ok(_) => {
                            if let Err(e) = logship::save_state(&cfg.state_dir, &st) {
                                warn!(error = %e, "log state not saved");
                            }
                        }
                        Err(e) => warn!(error = %e, "journal collection failed"),
                    }
                }
                tokio::time::sleep(Duration::from_secs(cfg.journal.interval_secs.max(2))).await;
            }
        });
    }

    // 3. OTA check loop (first check soon after boot, then hourly).
    if cfg.ota.enabled {
        let (cfg, g) = (cfg.clone(), g.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(120)).await;
            loop {
                let metered = logship::uplink_is_metered(&cfg.metered_drivers);
                if metered && !rascar_agent::config::DashboardPolicy::read(cfg.dashboard_settings.as_deref()).ota_over_4g {
                    info!("update check skipped: OTA over 4G is off in the dashboard's 4G panel");
                    tokio::time::sleep(Duration::from_secs(cfg.ota.check_interval_secs.max(60))).await;
                    continue;
                }
                let mut st = ota::load_state(&cfg.state_dir);
                let before = st.last_update.clone();
                match ota::check_and_stage(&cfg, &g, &mut st, metered).await {
                    Ok(c) => info!(result = ?c, "update check"),
                    Err(e) => warn!(error = %format!("{e:#}"), "update check failed"),
                }
                if let Err(e) = ota::save_state(&cfg.state_dir, &st) {
                    warn!(error = %e, "ota state not saved");
                }
                if st.last_update != before {
                    post_status(&cfg, &g, None).await;
                }
                tokio::time::sleep(Duration::from_secs(cfg.ota.check_interval_secs.max(60))).await;
            }
        });
    }

    // 4. Upload loop + periodic status, in the foreground.
    let mut last_status = tokio::time::Instant::now() - Duration::from_secs(cfg.status_interval_secs);
    loop {
        let metered = logship::uplink_is_metered(&cfg.metered_drivers);
        if metered && !rascar_agent::config::DashboardPolicy::read(cfg.dashboard_settings.as_deref()).logs_over_4g {
            // Logs wait for Wi-Fi/LAN; the small status report still goes out.
            debug!("log upload skipped: logs over 4G are off in the dashboard's 4G panel");
            if last_status.elapsed() >= Duration::from_secs(cfg.status_interval_secs) {
                post_status(&cfg, &g, None).await;
                last_status = tokio::time::Instant::now();
            }
            tokio::time::sleep(Duration::from_secs(cfg.upload.metered_interval_secs.max(10))).await;
            continue;
        }
        let mut local = logs.lock().await.clone();
        let rep = logship::upload(&cfg, &mut local, &g, metered).await;
        {
            let mut st = logs.lock().await;
            st.streams = local.streams;
            st.files = local.files;
            st.metered = local.metered;
            for p in logship::prune_spool(&cfg, &st) {
                info!(spool = %p.display(), "journal spool removed");
            }
            if let Err(e) = logship::save_state(&cfg.state_dir, &st) {
                warn!(error = %e, "log state not saved");
            }
        }
        if rep.bytes_sent > 0 || rep.stop != logship::Stop::Done {
            info!(sent = rep.bytes_sent, backlog = rep.backlog, stop = ?rep.stop, metered, "log upload round");
        }
        if last_status.elapsed() >= Duration::from_secs(cfg.status_interval_secs) && rep.stop != logship::Stop::Offline {
            post_status(&cfg, &g, Some(&rep)).await;
            last_status = tokio::time::Instant::now();
        }
        let wait = if metered { cfg.upload.metered_interval_secs } else { cfg.upload.interval_secs };
        tokio::time::sleep(Duration::from_secs(wait.max(10))).await;
    }
}
