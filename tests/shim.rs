//! The cargo shim decides per call, from the working directory alone,
//! whether cargo builds in an agent's context or runs untouched.

mod common;

use common::{ALL_PACKAGES, App, Build, TestRepo};

#[test]
fn shim_builds_in_the_agent_whose_worktree_holds_the_cwd() {
    let repo = TestRepo::new();
    repo.base();
    let worktree = repo.spawn("a");

    Build::run(repo.shimmed_cargo(&worktree).arg("build")).assert_rebuilt(&[]);
    Build::run(repo.shimmed_cargo(&worktree.join("leaf/src")).arg("build")).assert_rebuilt(&[]);
    assert!(!worktree.join("target").exists());

    // An inherited target dir (from a parent agent's environment, say) must
    // not redirect the agent's build.
    let inherited = repo.scratch().join("inherited-target");
    Build::run(
        repo.shimmed_cargo(&worktree)
            .env("CARGO_TARGET_DIR", &inherited)
            .arg("build"),
    )
    .assert_rebuilt(&[]);
    assert!(!inherited.exists());
}

#[test]
fn shim_runs_plain_cargo_outside_agent_worktrees() {
    let repo = TestRepo::new();
    repo.base();
    repo.spawn("a");

    Build::run(repo.shimmed_cargo(repo.root()).arg("build")).assert_rebuilt(ALL_PACKAGES);
    assert!(
        repo.root()
            .join(format!("target/debug/app{}", std::env::consts::EXE_SUFFIX))
            .is_file()
    );

    let outside = repo.scratch().join("not-a-repo");
    std::fs::create_dir(&outside).unwrap();
    common::run(repo.shimmed_cargo(&outside).arg("--version")).ok();
}

/// Cargo hashes path packages by their path relative to the workspace root,
/// so another workspace's packages would overwrite the agent's units with
/// the same names if they shared its target dir.
#[test]
fn other_workspaces_never_build_into_the_agents_target() {
    let repo = TestRepo::new();
    repo.base();
    let a = repo.spawn("a");
    let b = repo.spawn("b");

    repo.set_marker(&b, "AGENT_B");
    common::run(
        repo.shimmed_cargo(&a)
            .arg("build")
            .arg("--manifest-path")
            .arg(b.join("Cargo.toml")),
    )
    .ok();
    assert_eq!(repo.app("a"), App::base(), "agent a runs agent b's code");

    let nested = a.join("nested");
    std::fs::create_dir_all(nested.join("leaf/src")).unwrap();
    repo.write(
        &nested.join("Cargo.toml"),
        "[workspace]\nmembers = [\"leaf\"]\nresolver = \"2\"\n\n[profile.dev]\ndebug = false\n",
    );
    let leaf_manifest = std::fs::read_to_string(a.join("leaf/Cargo.toml")).unwrap();
    repo.write(&nested.join("leaf/Cargo.toml"), &leaf_manifest);
    repo.set_marker(&nested, "NESTED");
    common::run(repo.shimmed_cargo(&nested).arg("build")).ok();
    assert_eq!(
        repo.app("a"),
        App::base(),
        "agent a runs the nested workspace's code"
    );

    // A member's directory still selects the root workspace and its warm
    // target dir.
    Build::run(repo.shimmed_cargo(&a.join("leaf")).arg("build")).assert_rebuilt(&[]);
}
