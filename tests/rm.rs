//! `ferry rm` never destroys work: it refuses dirty worktrees and keeps
//! branches that carry commits.

mod common;

use common::TestRepo;

fn branch_exists(repo: &TestRepo, branch: &str) -> bool {
    let reference = format!("refs/heads/{branch}");
    common::run(
        repo.command("git")
            .args(["show-ref", "--verify", "--quiet", &reference]),
    )
    .status
    .success()
}

#[test]
fn rm_refuses_a_dirty_worktree_without_force() {
    let repo = TestRepo::new();
    repo.base();
    let worktree = repo.spawn("a");

    let untracked = worktree.join("notes.txt");
    repo.write(&untracked, "agent notes\n");
    let refused = repo.ferry(["rm", "a"]).failed();
    assert!(refused.stderr.contains("error:"), "{refused}");
    assert!(refused.stderr.contains("uncommitted"), "{refused}");
    assert!(worktree.is_dir());
    std::fs::remove_file(&untracked).unwrap();

    repo.set_marker(&worktree, "UNSAVED");
    repo.ferry(["rm", "a"]).failed();
    assert!(worktree.is_dir());
    assert_eq!(repo.ferry_json(["list"])["agents"][0]["name"], "a");

    let removed = repo.ferry_json(["rm", "--force", "a"]);
    assert_eq!(removed["branch_deleted"], true, "{removed}");
    assert!(!worktree.exists());
    assert!(!branch_exists(&repo, "ferry/a"));
    assert_eq!(repo.ferry_json(["list"])["agents"], serde_json::json!([]));
}

#[test]
fn rm_keeps_a_branch_with_new_commits() {
    let repo = TestRepo::new();
    repo.base();
    let worktree = repo.spawn("a");
    repo.set_marker(&worktree, "COMMITTED");
    repo.git(&worktree, ["commit", "-q", "-am", "agent work"]);
    let tip = repo.git(&worktree, ["rev-parse", "HEAD"]);

    let removed = repo.ferry_json(["rm", "a"]);
    assert_eq!(removed["branch_deleted"], false, "{removed}");
    assert_eq!(removed["branch_commits"], 1, "{removed}");
    assert!(!worktree.exists());
    assert_eq!(repo.git(repo.root(), ["rev-parse", "ferry/a"]), tip);
}

#[test]
fn a_fresh_agent_is_removable_when_the_repo_does_not_track_its_lockfile() {
    let repo = TestRepo::new();
    repo.git(repo.root(), ["rm", "-q", "--cached", "Cargo.lock"]);
    repo.git(repo.root(), ["commit", "-q", "-m", "untrack Cargo.lock"]);
    std::fs::remove_file(repo.root().join("Cargo.lock")).unwrap();
    repo.base();
    let worktree = repo.spawn("a");
    // Spawn gives the agent the base's lockfile so it resolves the same
    // versions; that copy is not the agent's work.
    assert!(worktree.join("Cargo.lock").is_file());
    repo.build("a").assert_rebuilt(&[]);

    repo.ferry(["rm", "a"]).ok();
    assert!(!worktree.exists());

    // A lockfile the agent changed is its work, though.
    let worktree = repo.spawn("b");
    repo.write(&worktree.join("Cargo.lock"), "# edited by the agent\n");
    let refused = repo.ferry(["rm", "b"]).failed();
    assert!(refused.stderr.contains("Cargo.lock"), "{refused}");
}

#[test]
fn rm_refuses_while_a_process_runs_as_the_agent_on_any_backend() {
    let repo = TestRepo::with_env(&[("FERRY_SANDBOX", "off"), ("FERRY_BACKEND", "copy")]);
    repo.base();
    let worktree = repo.spawn("a");
    let mut sleeper = repo
        .ferry_cmd(["run", "a", "--", "sleep", "30"])
        .spawn()
        .unwrap();
    // `ferry run` takes the agent's busy lock before it execs.
    let busy = || repo.ferry_json(["list"])["agents"][0]["busy"] == true;
    assert!(
        common::eventually(busy),
        "the running agent is not reported busy"
    );

    let refused = repo.ferry(["rm", "a"]).failed();
    assert!(refused.stderr.contains("still in use"), "{refused}");
    assert!(worktree.is_dir());

    // On Windows `ferry run` waits for its child rather than exec'ing it, and
    // killing ferry alone would leave `sleep` running, as the job object
    // rightly reports: end the whole tree.
    #[cfg(windows)]
    std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &sleeper.id().to_string()])
        .output()
        .unwrap();
    #[cfg(unix)]
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
    assert!(
        common::eventually(|| !busy()),
        "the agent stays busy after its process exited"
    );
    repo.ferry(["rm", "a"]).ok();
}
