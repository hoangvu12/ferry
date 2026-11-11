//! Lifecycle and user-data-safety regressions found in review.

mod common;

use std::fs;
use std::path::PathBuf;

use common::{App, TestRepo, path_of};

/// True if some ref (branch, tag, stash, ...) still reaches `commit`.
fn reachable(repo: &TestRepo, commit: &str) -> bool {
    !repo
        .git(
            repo.root(),
            ["for-each-ref", "--contains", commit, "--format=%(refname)"],
        )
        .is_empty()
}

/// CRITICAL: `ferry rm` (and the Claude `WorktreeRemove` hook and `gc`'s
/// reaper, which share `remove_locked`) removes a clean worktree whose HEAD
/// is detached with new commits, then deletes the untouched branch. The
/// commits are left reachable from no ref (the worktree's HEAD reflog goes
/// with the worktree), so `git gc` eventually deletes them.
#[test]
fn rm_does_not_orphan_commits_made_on_a_detached_head() {
    let repo = TestRepo::new();
    repo.base();
    let worktree = repo.spawn("a");
    repo.git(&worktree, ["checkout", "-q", "--detach"]);
    repo.set_marker(&worktree, "DETACHED_WORK");
    repo.git(&worktree, ["commit", "-q", "-am", "agent work"]);
    let commit = repo.git(&worktree, ["rev-parse", "HEAD"]);

    let removed = repo.ferry(["rm", "a"]);
    assert!(
        !removed.status.success() || reachable(&repo, &commit),
        "rm succeeded and commit {commit} is now reachable from no ref: {removed}"
    );
}

/// CRITICAL (wrong build): a plain git worktree nested inside an agent's
/// worktree (Claude Code's default `<repo>/.claude/worktrees/<name>` for an
/// `isolation: worktree` sub-agent when the session runs in the agent) is
/// resolved by path prefix as that agent. The shim then builds the nested
/// checkout in the agent's sandbox against the agent's target dir, and the
/// agent's next build treats those artifacts as fresh.
#[test]
fn nested_plain_worktree_does_not_contaminate_the_enclosing_agent() {
    let repo = TestRepo::new();
    repo.base();
    let worktree = repo.spawn("a");
    assert_eq!(repo.app("a"), App::base());

    let nested = worktree.join(".claude/worktrees/sub");
    repo.git(
        &worktree,
        [
            "worktree".as_ref(),
            "add".as_ref(),
            "-q".as_ref(),
            nested.as_os_str(),
            "-b".as_ref(),
            "sub".as_ref(),
        ],
    );
    repo.set_marker(&nested, "SUB_AGENT_EDIT");
    common::run(
        repo.shimmed_cargo(&nested)
            .args(["build", "-q", "-p", "app"]),
    )
    .ok();

    // Agent a's sources are untouched; its build must still say LEAF_BASE.
    assert_eq!(
        repo.app("a"),
        App::base(),
        "agent a now runs code from a different worktree"
    );
}

/// HIGH: after the store (cache dir) is deleted, which the docs anticipate
/// ("macOS may purge caches and users clear them casually"), `ferry base`
/// fails forever: the old base checkout is still registered and *locked*,
/// `discard` only calls `git worktree remove` when the directory exists, and
/// `git worktree prune` skips locked entries. `ferry gc` does not recover.
#[test]
fn base_can_be_rebuilt_after_the_store_is_deleted() {
    let repo = TestRepo::new();
    repo.base();
    let store = repo.store();
    fs::remove_dir_all(&store).unwrap();
    repo.ferry(["gc"]);
    let again = repo.ferry(["base"]);
    assert!(again.status.success(), "{again}");
}

/// HIGH: with a bare repository plus worktrees, `main_worktree` falls back to
/// the *current* worktree, whose directory name is part of the store key, so
/// every worktree (each agent included) opens a different store. Inside an
/// agent, `ferry run`/`env` fail and the shim silently runs plain cargo.
#[test]
fn bare_repository_worktrees_share_one_store() {
    let repo = TestRepo::new();
    let bare = repo.scratch().join("proj.git");
    let main = repo.scratch().join("main");
    repo.git(
        repo.scratch(),
        [
            "clone".as_ref(),
            "-q".as_ref(),
            "--bare".as_ref(),
            repo.root().as_os_str(),
            bare.as_os_str(),
        ],
    );
    repo.git(
        &bare,
        [
            "worktree".as_ref(),
            "add".as_ref(),
            "-q".as_ref(),
            main.as_os_str(),
            "main".as_ref(),
        ],
    );
    common::run(repo.ferry_cmd(["base"]).current_dir(&main)).ok();
    let spawned = common::run(repo.ferry_cmd(["--json", "spawn", "a"]).current_dir(&main))
        .ok()
        .json();
    let worktree = path_of(&spawned["agent"]["worktree"]);

    let env = common::run(repo.ferry_cmd(["env"]).current_dir(&worktree));
    assert!(
        env.status.success(),
        "inside the agent, ferry does not find it: {env}"
    );
}

/// HIGH: for reflink and copy agents nothing records running processes, so
/// `ferry rm` (and `gc`'s reaper, and the `WorktreeRemove` hook) delete the
/// worktree and target of an agent while `ferry run` is still using it, and
/// `ferry list` reports it as idle.
#[test]
fn rm_refuses_a_busy_copy_agent() {
    let repo = TestRepo::with_env(&[("FERRY_BACKEND", "copy")]);
    repo.base();
    let worktree = repo.spawn("a");
    let mut busy = repo
        .ferry_cmd(["run", "a", "--", "sleep", "30"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));

    let removed = repo.ferry(["rm", "a"]);
    let _ = busy.kill();
    let _ = busy.wait();
    assert!(
        !removed.status.success() && worktree.is_dir(),
        "rm removed an agent with a running process: {removed}"
    );
}

/// HIGH: `spawn` checks that `--path` is free under the store lock but runs
/// `git worktree add` after releasing it, and `rollback` force-removes
/// whatever worktree is at that path. When two spawns target one path, the
/// loser's rollback deletes the winner's worktree with its uncommitted
/// work. The `reference-transaction` hook only widens the timing window.
#[test]
fn failed_spawn_does_not_remove_another_agents_worktree() {
    let repo = TestRepo::new();
    repo.base();
    let hook = repo.root().join(".git/hooks/reference-transaction");
    fs::write(
        &hook,
        "#!/bin/sh\n[ \"$1\" = committed ] && grep -q 'refs/heads/ferry/a$' && sleep 3\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let shared: PathBuf = repo.scratch().join("shared");

    let mut first = repo
        .ferry_cmd([
            "spawn".as_ref(),
            "a".as_ref(),
            "--path".as_ref(),
            shared.as_os_str(),
        ])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let second = common::run(&mut repo.ferry_cmd([
        "spawn".as_ref(),
        "b".as_ref(),
        "--path".as_ref(),
        shared.as_os_str(),
    ]));
    if second.status.success() {
        repo.write(
            &shared.join("WORK_IN_PROGRESS.txt"),
            "b's uncommitted work\n",
        );
        let _ = first.wait();
        assert!(
            shared.join("WORK_IN_PROGRESS.txt").is_file(),
            "agent b's worktree (and its uncommitted work) was deleted by a's rollback"
        );
    } else {
        // b was refused because a had claimed the path; a must be unaffected.
        assert!(first.wait().unwrap().success(), "{second}");
        assert!(shared.join(".git").is_file(), "{second}");
    }
}

/// MEDIUM: a spawn that fails while `--path` names a pre-existing empty
/// directory (here: a branch name git rejects) leaves an agent in state
/// `creating` that neither `ferry rm` nor `ferry rm --force` can remove,
/// because rollback and rm both insist on `git worktree remove` for a
/// directory that never became a worktree.
#[test]
fn failed_spawn_into_an_empty_dir_leaves_a_removable_agent() {
    let repo = TestRepo::new();
    repo.base();
    let dir = repo.scratch().join("empty");
    fs::create_dir(&dir).unwrap();
    common::run(&mut repo.ferry_cmd([
        "spawn".as_ref(),
        "x".as_ref(),
        "--path".as_ref(),
        dir.as_os_str(),
        "--branch".as_ref(),
        "bad..name".as_ref(),
    ]))
    .failed();
    let listed = repo.ferry_json(["list"]);
    if listed["agents"].as_array().is_some_and(|a| !a.is_empty()) {
        let removed = repo.ferry(["rm", "--force", "x"]);
        assert!(removed.status.success(), "stuck agent: {removed}");
    }
}

/// MEDIUM: the Claude hook maps worktree names to agent names lossily, and
/// an existing agent with the mapped name is reused, so distinct Claude
/// worktrees share one agent worktree and build state.
#[test]
fn distinct_claude_worktree_names_get_distinct_agents() {
    let repo = TestRepo::new();
    repo.base();
    let create = |name: &str| {
        let mut cmd = repo.ferry_cmd(["hook", "claude-worktree-create"]);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let input = serde_json::json!({ "cwd": repo.root(), "name": name }).to_string();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    let first = create("feat/login");
    let second = create("feat-login");
    assert_ne!(first, second, "two Claude worktrees share {first}");
}

/// MEDIUM: every fresh `ferry base` (via `discard`), every `gc` and `rm` of
/// an agent with a missing worktree run a repository-wide `git worktree
/// prune`, which deletes the admin dir (index, HEAD, per-worktree refs) of
/// the user's unrelated worktrees whose directory is temporarily absent.
#[test]
fn base_does_not_prune_unrelated_worktrees() {
    let repo = TestRepo::new();
    let mount = repo.scratch().join("usb");
    let feature = mount.join("feature");
    repo.git(
        repo.root(),
        [
            "worktree".as_ref(),
            "add".as_ref(),
            "-q".as_ref(),
            feature.as_os_str(),
            "-b".as_ref(),
            "feature".as_ref(),
        ],
    );
    let offline = repo.scratch().join("usb.offline");
    fs::rename(&mount, &offline).unwrap();
    repo.base();
    fs::rename(&offline, &mount).unwrap();
    let status = common::run(repo.command("git").arg("-C").arg(&feature).arg("status"));
    assert!(
        status.status.success(),
        "user's worktree was pruned by ferry base: {status}"
    );
}
