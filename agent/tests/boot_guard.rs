//! Runs the real boot guard (setup/ota/rascar-ota-boot) against a temp tree:
//! the guard is the rollback path of last resort, so it is tested as shipped.

use std::path::{Path, PathBuf};
use std::process::Command;

use rascar_agent::ota::{Trial, read_trial};

struct Fx {
    _d: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    link: PathBuf,
}

fn fx() -> Fx {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().to_path_buf();
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    for r in ["v1", "v2"] {
        std::fs::create_dir_all(root.join("releases").join(r).join(".ota")).unwrap();
    }
    let link = root.join("orangecar");
    std::os::unix::fs::symlink(root.join("releases/v2"), &link).unwrap();
    Fx { _d: d, root, state, link }
}

fn guard(fx: &Fx, agent: &Path) -> std::process::Output {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../setup/ota/rascar-ota-boot");
    Command::new("bash")
        .arg(script)
        .env("RASCAR_AGENT_STATE", &fx.state)
        .env("RASCAR_INSTALL_LINK", &fx.link)
        .env("RASCAR_MAX_TRIAL_BOOTS", "3")
        .env("RASCAR_AGENT_BIN", agent)
        .env("RASCAR_SYSTEMCTL", "true")
        .output()
        .unwrap()
}

fn write_trial(fx: &Fx, boots: u32) {
    let t = Trial { version: "v2".into(), release: fx.root.join("releases/v2"), previous: fx.root.join("releases/v1"), boots };
    std::fs::write(fx.state.join("trial"), t.render()).unwrap();
}

#[test]
fn a_trial_boot_is_counted_and_nothing_else_happens() {
    let fx = fx();
    write_trial(&fx, 1);
    let marker = fx.root.join("agent-called");
    let agent = fx.root.join("agent.sh");
    std::fs::write(&agent, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    std::fs::set_permissions(&agent, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let out = guard(&fx, &agent);
    assert!(out.status.success());
    assert_eq!(read_trial(&fx.state).unwrap().boots, 2);
    assert_eq!(std::fs::canonicalize(&fx.link).unwrap(), fx.root.join("releases/v2"));
    assert!(!marker.exists(), "no new release is applied on top of a trial");
}

#[test]
fn past_the_boot_budget_the_guard_rolls_back_on_its_own() {
    let fx = fx();
    write_trial(&fx, 3);
    // A damaged installed.list naming a path outside the allowlist: ignored.
    let victim = fx.root.join("victim");
    std::fs::write(&victim, "keep me").unwrap();
    std::fs::write(fx.root.join("releases/v2/.ota/installed.list"), format!("{}\n", victim.display())).unwrap();

    let out = guard(&fx, Path::new("/nonexistent"));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(std::fs::canonicalize(&fx.link).unwrap(), fx.root.join("releases/v1"));
    assert!(read_trial(&fx.state).is_none());
    assert_eq!(std::fs::read_to_string(fx.state.join("bad-versions")).unwrap(), "v2\n");
    assert_eq!(std::fs::read_to_string(fx.state.join("guard-rollback")).unwrap().trim(), "v2");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
    assert!(String::from_utf8_lossy(&out.stdout).contains("skipping disallowed path"));
}

#[test]
fn without_a_trial_the_current_agent_applies_staged_releases() {
    let fx = fx();
    let args = fx.root.join("args");
    let agent = fx.root.join("agent.sh");
    std::fs::write(&agent, format!("#!/bin/sh\necho \"$@\" > {}\nexit 1\n", args.display())).unwrap();
    std::fs::set_permissions(&agent, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let out = guard(&fx, &agent);
    // The agent failing must not fail the unit (the services start anyway).
    assert!(out.status.success());
    assert_eq!(std::fs::read_to_string(&args).unwrap().trim(), "apply-staged");
}

#[test]
fn a_trial_that_is_not_current_any_more_is_forgotten() {
    let fx = fx();
    write_trial(&fx, 1);
    std::fs::remove_file(&fx.link).unwrap();
    std::os::unix::fs::symlink(fx.root.join("releases/v1"), &fx.link).unwrap();
    assert!(guard(&fx, Path::new("/nonexistent")).status.success());
    assert!(read_trial(&fx.state).is_none());
    assert!(!fx.state.join("bad-versions").exists());
}
