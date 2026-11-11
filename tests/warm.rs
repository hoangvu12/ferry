//! The default base warms `cargo check` and `cargo clippy` as well as
//! `cargo build` and `cargo test`, and clippy never costs a base.

#![allow(
    clippy::unwrap_used,
    reason = "helpers outside #[test] functions fail the test by panicking"
)]

mod common;

use std::fs;
use std::path::Path;

use common::{ALL_PACKAGES, Build, TestRepo, str_of};

/// `ferry --json base` with the default warm commands.
fn default_base(repo: &TestRepo) -> serde_json::Value {
    repo.ferry_json(["base"])
}

fn append(repo: &TestRepo, path: &Path, extra: &str) {
    let mut contents = fs::read_to_string(path).unwrap();
    contents.push_str(extra);
    repo.write(path, &contents);
}

/// Adds `files` (relative path, contents) to the main checkout and commits
/// them with an up-to-date lockfile.
fn commit_files(repo: &TestRepo, files: &[(&str, &str)]) {
    for (path, contents) in files {
        let path = repo.root().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        repo.write(&path, contents);
    }
    common::run(repo.cargo_in(repo.root()).arg("generate-lockfile")).ok();
    repo.commit_all("change");
}

fn warnings(report: &serde_json::Value) -> Vec<&str> {
    report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(str_of)
        .collect()
}

#[test]
fn default_base_warms_check_and_clippy() {
    let repo = TestRepo::with_default_config(&[]);
    let clippy = repo.require_clippy();
    let report = default_base(&repo);
    let commands = report["base"]["commands"].as_array().unwrap();
    assert_eq!(commands.len(), if clippy { 4 } else { 3 }, "{report}");
    assert_eq!(
        report["base"]["command_secs"].as_array().map(Vec::len),
        Some(commands.len()),
        "{report}"
    );
    assert_eq!(report["base"]["incomplete"], serde_json::json!([]));
    if !clippy {
        assert!(
            warnings(&report).iter().any(|w| w.contains("clippy")),
            "{report}"
        );
    }
    if !repo.require_userns() {
        return;
    }
    repo.spawn("a");
    let mut runs: Vec<&[&str]> = vec![&["check", "--workspace"], &["check"]];
    if clippy {
        runs.extend([
            &["clippy", "--workspace", "--all-targets"][..],
            &["clippy", "--workspace"],
            &["clippy", "--workspace", "--tests"],
        ]);
    }
    for args in runs {
        Build::run(repo.agent_cargo("a").args(args)).assert_rebuilt(&[]);
    }
}

/// Clippy lints only workspace members, so their dependencies' check units
/// are shared with `cargo check`; lint arguments after `--` re-lint every
/// member, and `build.warnings` re-lints nothing.
#[test]
fn clippy_reuses_checked_dependencies_and_relints_only_members() {
    let repo = TestRepo::with_env(&[("FERRY_BASE_COMMAND", "cargo check --workspace")]);
    if !repo.require_userns() || !repo.require_clippy() {
        return;
    }
    let manifest = repo.root().join("Cargo.toml");
    let workspace = fs::read_to_string(&manifest).unwrap().replace(
        "resolver = \"2\"",
        "resolver = \"2\"\nexclude = [\"vendor\"]",
    );
    repo.write(&manifest, &workspace);
    append(
        &repo,
        &repo.root().join("leaf/Cargo.toml"),
        "\n[dependencies]\ndep = { path = \"../vendor/dep\" }\n",
    );
    // An unused dependency is a warning (`cargo::unused_dependencies`),
    // which the `build.warnings` call below would deny.
    append(
        &repo,
        &repo.root().join("leaf/src/lib.rs"),
        "\npub use dep::one;\n",
    );
    commit_files(
        &repo,
        &[
            (
                "vendor/dep/Cargo.toml",
                "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("vendor/dep/src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n"),
        ],
    );
    repo.base();
    repo.spawn("a");

    let clippy = || {
        let mut cmd = repo.agent_cargo("a");
        cmd.args(["clippy", "--workspace"]);
        cmd
    };
    Build::run(&mut clippy()).assert_rebuilt(ALL_PACKAGES);
    Build::run_clippy(&mut clippy(), &["-D", "warnings"]).assert_rebuilt(ALL_PACKAGES);
    Build::run(&mut clippy()).assert_rebuilt(ALL_PACKAGES);
    // Ignored by Cargo before 1.97, which also rebuilds nothing.
    Build::run(clippy().env("CARGO_BUILD_WARNINGS", "deny")).assert_rebuilt(&[]);
}

#[test]
fn check_reuses_build_scripts_and_proc_macros_from_build() {
    let repo = TestRepo::with_env(&[("FERRY_BASE_COMMAND", "cargo build --workspace")]);
    if !repo.require_userns() {
        return;
    }
    let manifest = repo.root().join("Cargo.toml");
    let workspace = fs::read_to_string(&manifest)
        .unwrap()
        .replace("\"embed\"]", "\"embed\", \"pm\"]");
    repo.write(&manifest, &workspace);
    append(
        &repo,
        &repo.root().join("side/Cargo.toml"),
        "\n[dependencies]\npm = { path = \"../pm\" }\n",
    );
    commit_files(
        &repo,
        &[
            (
                "pm/Cargo.toml",
                "[package]\nname = \"pm\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                 [lib]\nproc-macro = true\n",
            ),
            (
                "pm/src/lib.rs",
                "use proc_macro::TokenStream;\n\n#[proc_macro]\n\
                 pub fn answer(_: TokenStream) -> TokenStream {\n    \"42\".parse().unwrap()\n}\n",
            ),
            (
                "side/src/lib.rs",
                "pub fn answer() -> u32 {\n    pm::answer!()\n}\n",
            ),
        ],
    );
    repo.base();
    repo.spawn("a");

    // The proc macro is only a dependency here, built as `cargo build` does.
    Build::run(repo.agent_cargo("a").args(["check", "-p", "side"])).assert_rebuilt(&["side"]);
    let check = Build::run(repo.agent_cargo("a").args(["check", "--workspace"]));
    assert!(
        check.rebuilt.iter().any(|u| u.package == "leaf"),
        "{:?}",
        check.rebuilt
    );
    assert!(
        check
            .rebuilt
            .iter()
            .all(|u| u.target != "build-script-build"),
        "{:?}",
        check.rebuilt
    );
    let ran_build_script = check.log.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("Running `") && l.contains("/build-script-build`")
    });
    assert!(!ran_build_script, "{}", check.log);
}

#[test]
fn a_failing_clippy_keeps_the_base() {
    let repo = TestRepo::with_default_config(&[]);
    if !repo.require_clippy() {
        return;
    }
    // `clippy::eq_op` is deny-by-default; rustc accepts it.
    commit_files(
        &repo,
        &[(
            "side/src/lib.rs",
            "pub fn same(x: u32) -> bool {\n    x == x\n}\n",
        )],
    );
    let report = default_base(&repo);
    assert!(
        warnings(&report)
            .iter()
            .any(|w| w.contains("cargo clippy") && w.contains("failed")),
        "{report}"
    );
    assert_eq!(
        report["base"]["incomplete"],
        serde_json::json!(["cargo clippy --all-targets --keep-going --workspace"]),
        "{report}"
    );
    if !repo.require_userns() {
        return;
    }
    repo.spawn("a");
    Build::run(repo.agent_cargo("a").args(["check", "--workspace"])).assert_rebuilt(&[]);
    Build::run(repo.agent_cargo("a").args([
        "clippy",
        "--workspace",
        "--all-targets",
        "--exclude",
        "side",
    ]))
    .assert_rebuilt(&[]);
}

#[test]
fn lint_strictness_never_fails_a_base() {
    let repo = TestRepo::with_default_config(&[("CARGO_BUILD_WARNINGS", "deny")]);
    commit_files(
        &repo,
        &[(
            "side/src/lib.rs",
            "pub fn answer() -> u32 {\n    let unused = 1;\n    42\n}\n",
        )],
    );
    let report = default_base(&repo);
    assert_eq!(
        report["base"]["incomplete"],
        serde_json::json!([]),
        "{report}"
    );
}

#[cfg(unix)]
/// A repository with the default configuration whose `cargo clippy` fails
/// the way it does where clippy is not installed. Keep the directory alive
/// as long as the repository.
fn repo_without_clippy() -> (tempfile::TempDir, TestRepo) {
    use std::os::unix::fs::PermissionsExt;
    let bin = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let fake = bin.path().join("cargo-clippy");
    fs::write(&fake, "#!/bin/sh\necho 'no clippy here' >&2\nexit 1\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    // Cargo runs the first `cargo-clippy` on PATH.
    let path = std::env::join_paths(
        std::iter::once(bin.path().to_path_buf())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let repo = TestRepo::with_default_config(&[("PATH", path.to_str().unwrap())]);
    (bin, repo)
}

#[cfg(unix)]
/// A toolchain without clippy still gets a base, warmed without it.
#[test]
fn missing_clippy_is_skipped() {
    let (_bin, repo) = repo_without_clippy();
    let report = default_base(&repo);
    let commands = report["base"]["commands"].as_array().unwrap();
    assert!(
        commands.iter().all(|c| !str_of(c).contains("clippy")),
        "{report}"
    );
    assert!(
        warnings(&report)
            .iter()
            .any(|w| w.contains("clippy is not installed")),
        "{report}"
    );
}

#[cfg(unix)]
/// Skipping clippy must not leave a base that warms nothing, which config
/// validation rejects for the same reason.
#[test]
fn missing_clippy_fails_a_clippy_only_base() {
    let (_bin, repo) = repo_without_clippy();
    commit_files(&repo, &[("ferry.toml", "[base]\ntargets = [\"clippy\"]\n")]);
    let failed = repo.ferry(["--json", "base"]).failed();
    assert!(
        failed.stderr.contains("clippy is not installed")
            && failed.stderr.contains("rustup component add clippy"),
        "{failed}"
    );
    let listed = repo.ferry_json(["list"]);
    assert_eq!(listed["bases"], serde_json::json!([]), "{listed}");
}

/// Names of the checks `ferry --json doctor` reports with `env` added.
fn doctor_checks(repo: &TestRepo, env: &[(&str, &str)]) -> Vec<String> {
    let mut cmd = repo.ferry_cmd(["--json", "doctor"]);
    cmd.envs(env.iter().copied());
    // Doctor exits non-zero when a check fails; the report is still JSON.
    let report = common::run(&mut cmd).json();
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| str_of(&c["name"]).to_owned())
        .collect()
}

/// Doctor's note that clippy runs are not remapped shows only where
/// `ferry base` remaps: without the namespace sandbox, on a rustc that can.
#[test]
fn doctor_notes_unremapped_clippy_only_where_remapping_applies() {
    let repo = TestRepo::with_env(&[("FERRY_REMAP_PATHS", "1")]);
    let toolchain = ferry::toolchain::Toolchain::detect(repo.root()).unwrap();
    let remaps = ferry::remap::supported(&toolchain).is_ok();
    let noted = |sandbox| {
        doctor_checks(&repo, &[("FERRY_SANDBOX", sandbox)])
            .iter()
            .any(|name| name == "remap-paths")
    };
    assert_eq!(noted("off"), remaps);
    if repo.require_userns() {
        assert!(!noted("auto"));
    }
}

#[cfg(unix)]
/// Without a working toolchain, doctor reports that, not a missing clippy.
#[test]
fn doctor_skips_the_clippy_check_without_a_toolchain() {
    let repo = TestRepo::with_default_config(&[]);
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let bin = repo.scratch().join("git-only");
    fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(git, bin.join("git")).unwrap();
    let checks = doctor_checks(&repo, &[("PATH", bin.to_str().unwrap())]);
    assert!(checks.iter().any(|name| name == "toolchain"), "{checks:?}");
    assert!(!checks.iter().any(|name| name == "clippy"), "{checks:?}");
}
