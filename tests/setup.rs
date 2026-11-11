//! `ferry setup apparmor`: it never runs sudo without consent, and its JSON
//! report has the documented shape. The round trip through real sudo and
//! `AppArmor` runs only where `FERRY_TEST_SYSTEM_SETUP=1` (CI's `AppArmor`
//! job).

// The helper below is test code too; failing loudly is its job.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use common::TestRepo;
use serde_json::Value;

const OUTCOMES: &[&str] = &[
    "not-applicable",
    "not-needed",
    "already-done",
    "refused",
    "planned",
    "done",
    "failed",
];

/// `ferry setup apparmor` with a fake `sudo` first on `PATH` that records
/// that it ran.
fn with_fake_sudo(repo: &TestRepo) -> (PathBuf, impl Fn(&[&str]) -> common::Run) {
    let bin = repo.scratch().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let sudo = bin.join("sudo");
    fs::write(&sudo, "#!/bin/sh\ntouch \"$0.ran\"\nexit 1\n").unwrap();
    fs::set_permissions(&sudo, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let ran = bin.join("sudo.ran");
    let run = move |args: &[&str]| {
        let mut cmd = repo.ferry_cmd(args);
        cmd.env("PATH", &path);
        common::run(&mut cmd)
    };
    (ran, run)
}

fn restriction() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .ok()
        .map(|v| v.trim().to_owned())
}

#[test]
fn setup_never_runs_sudo_without_consent() {
    let repo = TestRepo::new();
    let (ran, setup) = with_fake_sudo(&repo);
    let restricted = cfg!(target_os = "linux") && restriction().as_deref() == Some("1");

    // stdin is /dev/null, as for any agent or script.
    let plain = setup(&["setup", "apparmor"]);
    if restricted {
        if !plain.status.success() {
            assert!(
                plain.stderr.contains("stdin is not a terminal") || plain.stderr.contains("error:"),
                "{plain}"
            );
        }
    } else {
        assert!(plain.status.success(), "{plain}");
    }
    let dry = setup(&["setup", "apparmor", "--dry-run"]);
    assert!(
        dry.status.success() || dry.stderr.contains("error:"),
        "{dry}"
    );
    let uninstall = setup(&["setup", "apparmor", "--uninstall"]);
    if !restricted {
        assert!(uninstall.status.success(), "{uninstall}");
    }
    let json = setup(&["--json", "setup", "apparmor"]);
    let report: Value =
        serde_json::from_str(&json.stdout).unwrap_or_else(|e| panic!("{e}: {json}"));
    assert!(
        OUTCOMES.contains(&report["outcome"].as_str().unwrap_or_default()),
        "{report}"
    );
    if report["outcome"] == "refused" {
        assert!(!json.status.success(), "{json}");
    }

    assert!(!ran.exists(), "sudo ran without consent");
}

#[test]
fn setup_dry_run_json_shape() {
    let repo = TestRepo::new();
    let (ran, setup) = with_fake_sudo(&repo);
    for args in [
        &["--json", "setup", "apparmor", "--dry-run"][..],
        &["--json", "setup", "apparmor", "--uninstall", "--dry-run"][..],
    ] {
        let out = setup(args);
        let report: Value =
            serde_json::from_str(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out}"));
        let action = if args.contains(&"--uninstall") {
            "uninstall"
        } else {
            "install"
        };
        assert_eq!(report["action"], action, "{report}");
        let outcome = report["outcome"].as_str().unwrap_or_default();
        assert!(OUTCOMES.contains(&outcome), "{report}");
        assert_ne!(outcome, "done", "{report}");
        assert!(report["reason"].is_string(), "{report}");
        assert!(report["steps"].is_array(), "{report}");
        assert!(report["command"].is_array(), "{report}");
        assert!(report["warnings"].is_array(), "{report}");
        if outcome == "planned" {
            assert_eq!(report["command"][0], "sudo", "{report}");
            assert!(report["profile_path"].is_string(), "{report}");
        }
        if cfg!(target_os = "macos") {
            assert_eq!(outcome, "not-applicable", "{report}");
        }
        if cfg!(target_os = "linux") && restriction().is_none() {
            assert_eq!(outcome, "not-needed", "{report}");
            assert!(out.status.success(), "{out}");
        }
    }
    assert!(!ran.exists(), "a dry run ran sudo");
}

#[cfg(target_os = "linux")]
#[test]
fn detect_reports_the_probe_label() {
    let repo = TestRepo::new();
    let dir = repo.scratch().join("detect");
    fs::create_dir(&dir).unwrap();
    let caps = repo
        .ferry([std::ffi::OsStr::new("__detect"), dir.as_os_str()])
        .ok()
        .json();
    assert!(caps["userns"]["status"].is_string(), "{caps}");
    if Path::new("/sys/kernel/security/apparmor").exists() {
        assert!(caps["probe_label"].is_string(), "{caps}");
    }
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "probe left files");
}

/// Uninstalls whatever is set up, then installs through real sudo,
/// verifies, re-runs, uninstalls and installs again, leaving the machine set
/// up. Needs Ubuntu 24.04+ with the restriction on and passwordless sudo.
#[test]
#[ignore = "changes the system; CI's AppArmor job runs it with FERRY_TEST_SYSTEM_SETUP=1"]
fn install_verify_rerun_uninstall_roundtrip() {
    if std::env::var_os("FERRY_TEST_SYSTEM_SETUP").is_none_or(|v| v != "1") {
        common::skip("FERRY_TEST_SYSTEM_SETUP=1 is not set");
        return;
    }
    assert_eq!(restriction().as_deref(), Some("1"));
    let repo = TestRepo::new();
    // A user-writable path, so the install step runs.
    let built = PathBuf::from(env!("CARGO_BIN_EXE_ferry"));
    let setup = |args: &[&str]| {
        let mut cmd = repo.command(&built);
        cmd.args(["--json", "setup", "apparmor"]).args(args);
        common::run(&mut cmd)
    };
    let installed = Path::new("/usr/local/bin/ferry");

    let clean = setup(&["--uninstall", "--yes"]).ok().json();
    assert!(
        clean["outcome"] == "done" || clean["outcome"] == "not-needed",
        "{clean}"
    );
    // Uninstall leaves the binary; remove it so the install step runs.
    let rm = Command::new("sudo")
        .args(["-n", "rm", "-f"])
        .arg(installed)
        .status();
    assert!(rm.unwrap().success());

    let out = setup(&["--yes"]);
    let report = out.ok().json();
    assert_eq!(report["outcome"], "done", "{report}");
    assert!(
        report["steps"][0]
            .as_str()
            .is_some_and(|s| s.starts_with("install this ferry")),
        "{report}"
    );
    assert_eq!(report["verified"]["userns"]["status"], "ok", "{report}");
    assert!(
        report["verified"]["label"]
            .as_str()
            .is_some_and(|l| l.starts_with("ferry (")),
        "{report}"
    );

    assert_eq!(fs::read(installed).unwrap(), fs::read(&built).unwrap());
    let profile = Path::new("/etc/apparmor.d/usr.local.bin.ferry");
    let meta = fs::metadata(profile).unwrap();
    assert_eq!(
        (meta.uid(), meta.gid(), meta.mode() & 0o7777),
        (0, 0, 0o644)
    );

    // The probe child keeps the `ferry` label (the self-exec rule).
    let installed_ferry = |args: &[&str]| {
        let mut cmd = repo.command(installed);
        cmd.args(args);
        common::run(&mut cmd)
    };
    let doctor = installed_ferry(&["--json", "doctor"]).json();
    assert_eq!(doctor["capabilities"]["userns"]["status"], "ok", "{doctor}");
    assert_eq!(
        doctor["capabilities"]["probe_label"], "ferry (enforce)",
        "{doctor}"
    );

    let again = setup(&["--yes"]).ok().json();
    assert_eq!(again["outcome"], "already-done", "{again}");
    assert_eq!(again["steps"], serde_json::json!([]), "{again}");

    // What an agent runs gets no capabilities in a namespace.
    installed_ferry(&["base"]).ok();
    installed_ferry(&["spawn", "a"]).ok();
    let inside = installed_ferry(&[
        "run",
        "a",
        "--",
        "sh",
        "-c",
        "cat /proc/self/attr/current; echo; unshare -Ur true && echo unshare-worked",
    ]);
    assert!(inside.stdout.contains("unpriv_ferry"), "{inside}");
    assert!(!inside.stdout.contains("unshare-worked"), "{inside}");

    let removed = setup(&["--uninstall", "--yes"]).ok().json();
    assert_eq!(removed["outcome"], "done", "{removed}");
    assert!(!profile.exists());
    let doctor = installed_ferry(&["--json", "doctor"]).json();
    assert_ne!(doctor["capabilities"]["userns"]["status"], "ok", "{doctor}");

    let report = setup(&["--yes"]).ok().json();
    assert_eq!(report["outcome"], "done", "{report}");
    assert!(installed_ferry(&["--version"]).status.success());
}
