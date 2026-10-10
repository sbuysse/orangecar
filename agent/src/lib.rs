//! rascar-agent: the car's maintenance agent.
//!
//! * pushes the journal (rascar units + kernel) and durable debug files to
//!   the update server whenever there is connectivity (`logship`);
//! * asks the update server for newer releases, verifies their ed25519 signature
//!   against the car's own key, stages them, switches at the next boot and
//!   rolls back automatically when the new release is unhealthy (`ota`).
//!
//! It is a separate service from rascar-app on purpose: the updater must
//! survive (and judge) a broken rascar-app release, and a slow 4G upload or a
//! bundle download must never compete with the dashboard's own event loop.
//!
//! Everything here is optional: until an update server is configured
//! (`/etc/rascar/update-server.json`, see `docs/ota.md`) the daemon only
//! judges a pending trial release and otherwise sits idle.

pub mod config;
pub mod logship;
pub mod manifest;
pub mod ota;
pub mod server;
pub mod util;

/// Version of the agent itself (reported to the update server). `RASCAR_BUILD_ID` is
/// set by tools/release.sh, as for rascar-carplay.
pub fn agent_version() -> String {
    format!("{}+{}", env!("CARGO_PKG_VERSION"), option_env!("RASCAR_BUILD_ID").unwrap_or("dev"))
}
