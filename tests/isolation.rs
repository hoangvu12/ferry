//! Where an agent's cargo runs and writes: canonical paths inside the
//! namespace sandbox, host paths without it, and never a directory chosen by
//! user configuration.

mod common;

use std::fs;
use std::path::Path;

use common::{TestRepo, path_of};
use serde_json::Value;

fn cargo_metadata(repo: &TestRepo, agent: &str) -> Value {
    common::run(
        repo.agent_cargo(agent)
            .args(["metadata", "--format-version=1", "--no-deps"]),
    )
    .ok()
    .json()
}

#[test]
fn cargo_runs_at_the_canonical_paths_inside_the_namespace_sandbox() {
    let repo = TestRepo::new();
    if !repo.require_userns() {
        return;
    }
    repo.base();
    let worktree = repo.spawn("a");
    let mnt = repo.store().join("mnt");

    let metadata = cargo_metadata(&repo, "a");
    assert_eq!(path_of(&metadata["workspace_root"]), mnt.join("src"));
    assert_eq!(path_of(&metadata["target_directory"]), mnt.join("target"));

    // Other commands keep the host path, which agent tools key state on.
    let pwd = common::run(&mut repo.ferry_cmd(["run", "a", "--", "pwd"])).ok();
    assert_eq!(pwd.stdout.trim(), worktree.to_str().unwrap());

    // The mounts exist only inside the sandbox.
    for dir in ["src", "target"] {
        assert_eq!(fs::read_dir(mnt.join(dir)).unwrap().count(), 0, "mnt/{dir}");
    }
}

#[test]
fn cargo_runs_at_host_paths_without_a_sandbox() {
    let repo = TestRepo::with_env(&[("FERRY_SANDBOX", "off")]);
    repo.base();
    let worktree = repo.spawn("a");
    let env = repo.ferry_json(["env", "a"]);
    assert_eq!(env["isolation"], "none", "{env}");

    let metadata = cargo_metadata(&repo, "a");
    assert_eq!(path_of(&metadata["workspace_root"]), worktree);
    let mut target = path_of(&metadata["target_directory"]);
    // With remap-paths (FERRY_TEST_REMAP=1), cargo reaches the target
    // through a link in the worktree.
    if target == worktree.join(ferry::remap::TARGET_LINK) {
        target = fs::read_link(&target).unwrap();
    }
    assert_eq!(target, path_of(&env["host_target_dir"]));
}

#[test]
fn configured_target_and_build_dirs_cannot_redirect_builds() {
    let repo = TestRepo::new();
    let redirected = repo.scratch().join("redirected");
    let config = format!(
        "[build]\ntarget-dir = \"{0}/target\"\nbuild-dir = \"{0}/build/{{workspace-path-hash}}\"\n",
        // TOML basic strings treat `\` as an escape.
        redirected.display().to_string().replace('\\', "/")
    );
    fs::create_dir(repo.root().join(".cargo")).unwrap();
    repo.write(&repo.root().join(".cargo/config.toml"), &config);
    repo.commit_all("cargo config with shared dirs");

    repo.base();
    let worktree = repo.spawn("a");
    repo.build("a").assert_rebuilt(&[]);
    repo.set_marker(&worktree, "EDITED");
    repo.build("a").assert_rebuilt(&["leaf", "mid", "app"]);
    assert!(
        !redirected.exists(),
        "a build wrote to {}",
        redirected.display()
    );
}

/// Without the namespace sandbox an agent builds at other paths than its
/// base. A build script that writes its `CARGO_MANIFEST_DIR` into generated
/// code leaves that path in no dep-info; spawn finds it in the script's
/// `OUT_DIR` and makes the script rerun, so `embed` and its dependents (and
/// nothing else) rebuild in the agent's first build.
#[test]
fn without_a_sandbox_generated_code_sees_the_agents_paths() {
    let repo = TestRepo::with_env(&[("FERRY_SANDBOX", "off")]);
    let embed = repo.root().join("embed");
    let build_rs = fs::read_to_string(embed.join("build.rs")).unwrap().replacen(
        "fn main() {",
        "fn main() {\n    let dir = std::env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n    \
         let out = std::env::var(\"OUT_DIR\").unwrap();\n    \
         fs::write(format!(\"{out}/dir.rs\"), format!(\"pub const DIR: &str = {dir:?};\")).unwrap();",
        1,
    );
    repo.write(&embed.join("build.rs"), &build_rs);
    let lib = fs::read_to_string(embed.join("src/lib.rs")).unwrap();
    repo.write(
        &embed.join("src/lib.rs"),
        &format!("{lib}include!(concat!(env!(\"OUT_DIR\"), \"/dir.rs\"));\n"),
    );
    let main = repo.root().join("app/src/main.rs");
    let app = fs::read_to_string(&main).unwrap().replacen(
        "fn main() {",
        "fn main() {\n    println!(\"dir={}\", embed::DIR);",
        1,
    );
    repo.write(&main, &app);
    repo.commit_all("embed records its manifest dir");
    repo.base();

    let worktree = repo.spawn("a");
    repo.build("a").assert_rebuilt(&["embed", "app"]);
    // Run directly: another cargo call could hide a unit that lags a build.
    let out = common::run(&mut repo.command(repo.agent_target("a").join("debug/app"))).ok();
    let dir = out
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("dir="))
        .unwrap();
    assert_eq!(Path::new(dir), worktree.join("embed"), "{out}");
    repo.build("a").assert_rebuilt(&[]);
}

/// Cargo reads `.cargo/config.toml` from every directory above the one it
/// runs in. Ferry's builds run under the store, so a config above the
/// repository (or an untracked one in it) silently stops applying.
#[test]
fn cargo_configs_that_ferry_builds_miss_are_reported() {
    let repo = TestRepo::new();
    let above = repo.scratch().join(".cargo").join("config.toml");
    fs::create_dir_all(above.parent().unwrap()).unwrap();
    repo.write(&above, "[build]\njobs = 2\n");
    let untracked = repo.root().join(".cargo").join("config.toml");
    fs::create_dir_all(untracked.parent().unwrap()).unwrap();
    repo.write(&untracked, "[build]\njobs = 2\n");

    let built = repo.ferry_json(["base"]);
    // The strings themselves: JSON escapes the backslashes of Windows paths.
    let warnings: Vec<&str> = built["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|w| w.as_str())
        .collect();
    for config in [&above, &untracked] {
        let config = config
            .to_str()
            .unwrap()
            .replace('/', std::path::MAIN_SEPARATOR_STR);
        assert!(warnings.iter().any(|w| w.contains(&config)), "{built}");
    }

    repo.commit_all("track the repository's cargo config");
    let doctor = repo.ferry_json(["doctor"]);
    let reported: Vec<&Value> = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["name"] == "cargo config")
        .collect();
    assert_eq!(reported.len(), 1, "{doctor}");
    assert!(
        reported[0]["detail"]
            .as_str()
            .unwrap()
            .contains(above.to_str().unwrap())
    );
}
