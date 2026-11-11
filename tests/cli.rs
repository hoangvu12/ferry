//! The CLI contract: `--json` output parses, and errors print `error:` with
//! a `hint:` where there is an obvious fix.

mod common;

use std::fs;

use common::{TestRepo, path_of};
use serde_json::json;

#[test]
fn json_output_of_list_env_and_doctor() {
    let repo = TestRepo::new();
    let base = repo.base();
    let worktree = repo.spawn("a");

    let list = repo.ferry_json(["list"]);
    assert_eq!(list["bases"][0]["id"], base.as_str(), "{list}");
    assert_eq!(list["bases"][0]["agents"], json!(["a"]), "{list}");
    let agent = &list["agents"][0];
    assert_eq!(agent["name"], "a", "{list}");
    assert_eq!(agent["state"], "ready", "{list}");
    assert_eq!(agent["base_is_current"], true, "{list}");
    assert_eq!(agent["worktree_exists"], true, "{list}");
    assert_eq!(path_of(&agent["worktree"]), worktree);

    let env = repo.ferry_json(["env", "a"]);
    assert_eq!(path_of(&env["worktree"]), worktree);
    assert_eq!(env["branch"], "ferry/a", "{env}");
    assert_eq!(env["run_prefix"], json!(["ferry", "run", "a", "--"]));
    assert!(env["build_target_dir"].is_string(), "{env}");
    assert!(
        path_of(&env["shim_dir"])
            .join(format!("cargo{}", std::env::consts::EXE_SUFFIX))
            .exists(),
        "{env}"
    );

    // A target dir in the exported environment would leak into sub-agents
    // that work in other worktrees.
    let exports = repo.ferry(["env", "a", "--shell", "sh"]).ok();
    assert!(
        exports.stdout.contains("export FERRY_WORKTREE="),
        "{exports}"
    );
    assert!(!exports.stdout.contains("CARGO_TARGET_DIR"), "{exports}");

    let doctor = common::run(&mut repo.ferry_cmd(["--json", "doctor"])).json();
    assert!(
        doctor["checks"].as_array().is_some_and(|c| !c.is_empty()),
        "{doctor}"
    );
    for probe in ["userns", "overlay", "reflink"] {
        assert!(
            doctor["capabilities"][probe]["status"].is_string(),
            "{doctor}"
        );
    }
}

#[test]
fn list_is_empty_in_a_new_repository() {
    let repo = TestRepo::new();
    assert_eq!(
        repo.ferry_json(["list"]),
        json!({ "bases": [], "agents": [] })
    );
}

#[test]
fn spawn_without_a_base_fails_with_a_hint() {
    let repo = TestRepo::new();
    let run = repo.ferry(["spawn", "a"]).failed();
    assert!(run.stderr.contains("error: no warm base"), "{run}");
    assert!(run.stderr.contains("hint: run `ferry base`"), "{run}");
    assert_eq!(repo.ferry_json(["list"])["agents"], json!([]));
}

#[test]
fn unknown_agents_and_invalid_names_are_errors() {
    let repo = TestRepo::new();
    let unknown = repo.ferry(["run", "ghost", "--", "true"]).failed();
    assert!(
        unknown.stderr.contains("error: no agent named `ghost`"),
        "{unknown}"
    );
    assert!(unknown.stderr.contains("hint:"), "{unknown}");

    let invalid = repo.ferry(["spawn", "bad/name"]).failed();
    assert!(
        invalid.stderr.contains("error: invalid agent name"),
        "{invalid}"
    );
}

#[test]
fn commands_outside_a_repository_fail_unless_given_one_with_dash_c() {
    let repo = TestRepo::new();
    let outside = repo.scratch().join("not-a-repo");
    fs::create_dir(&outside).unwrap();

    let run = common::run(repo.ferry_cmd(["list"]).current_dir(&outside)).failed();
    assert!(
        run.stderr.contains("is not inside a git repository"),
        "{run}"
    );
    assert!(run.stderr.contains("hint:"), "{run}");

    let root = repo.root().to_str().unwrap();
    common::run(repo.ferry_cmd(["-C", root, "list"]).current_dir(&outside)).ok();
}
