//! Bases are immutable once built, and a refreshed base never disturbs the
//! agents still using an older one.

// clippy's allow-unwrap-in-tests covers only #[test] functions, not their
// helpers.
#![allow(clippy::unwrap_used)]

mod common;

use common::{App, BASE_MARKER, Build, Snapshot, TestRepo, str_of};
use serde_json::Value;

fn current_base(list: &Value) -> &str {
    let current: Vec<&Value> = list["bases"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["current"] == true)
        .collect();
    assert_eq!(current.len(), 1, "{list}");
    str_of(&current[0]["id"])
}

#[test]
fn agents_never_modify_the_base() {
    let repo = TestRepo::new();
    let base = repo.base();
    let dir = repo.base_dir(&base);
    let before = Snapshot::of(&dir);

    let worktree = repo.spawn("a");
    repo.build("a").assert_rebuilt(&[]);
    repo.set_marker(&worktree, "AGENT_EDIT");
    repo.set_asset(&worktree, "a.txt", "agent");
    repo.build("a")
        .assert_rebuilt(&["leaf", "mid", "embed", "app"]);
    common::run(repo.agent_cargo("a").args(["test", "--workspace", "-q"])).ok();
    common::run(repo.agent_cargo("a").args(["clean", "-p", "leaf"])).ok();
    repo.build("a").assert_rebuilt(&["leaf", "mid", "app"]);
    before.assert_unchanged(&Snapshot::of(&dir));

    repo.ferry(["rm", "--force", "a"]).ok();
    before.assert_unchanged(&Snapshot::of(&dir));
}

#[test]
fn building_the_same_commit_again_reuses_the_base() {
    let repo = TestRepo::new();
    let first = repo.ferry_json(["base"]);
    assert_eq!(first["reused"], false, "{first}");
    let second = repo.ferry_json(["base"]);
    assert_eq!(second["reused"], true, "{second}");
    assert_eq!(first["base"]["id"], second["base"]["id"]);

    let list = repo.ferry_json(["list"]);
    assert_eq!(list["bases"].as_array().map(Vec::len), Some(1), "{list}");
    assert_eq!(list["bases"][0]["ready"], true, "{list}");
    assert_eq!(current_base(&list), str_of(&first["base"]["id"]));
}

#[test]
fn a_new_base_leaves_live_agents_on_the_old_one_until_they_are_removed() {
    let repo = TestRepo::new();
    let first_commit = repo.git(repo.root(), ["rev-parse", "HEAD"]);
    let base1 = repo.base();
    repo.spawn("old");

    repo.set_marker(repo.root(), "LEAF_V2");
    repo.commit_all("v2");
    let base2 = repo.base();
    assert_ne!(base1, base2);
    let list = repo.ferry_json(["list"]);
    assert_eq!(current_base(&list), base2);
    let old = &list["agents"][0];
    assert_eq!(old["name"], "old", "{list}");
    assert_eq!(old["base"], base1.as_str(), "{list}");
    assert_eq!(old["base_is_current"], false, "{list}");

    repo.build("old").assert_rebuilt(&[]);
    assert_eq!(repo.app("old").marker, BASE_MARKER);

    repo.spawn("new");
    repo.build("new").assert_rebuilt(&[]);
    assert_eq!(repo.app("new").marker, "LEAF_V2");
    // A ready base built from exactly the requested commit wins over the
    // current one.
    let pinned = repo.ferry_json(["spawn", "pinned", "--from", &first_commit]);
    assert_eq!(pinned["agent"]["base"], base1.as_str(), "{pinned}");

    let gc = repo.ferry_json(["gc"]);
    assert_eq!(gc["removed"], serde_json::json!([]), "{gc}");
    assert!(repo.base_dir(&base1).is_dir());

    repo.ferry(["rm", "old"]).ok();
    repo.ferry(["rm", "pinned"]).ok();
    let dry = repo.ferry_json(["gc", "--dry-run"]);
    assert_eq!(dry["removed"], serde_json::json!([base1]), "{dry}");
    assert!(repo.base_dir(&base1).is_dir(), "--dry-run deleted the base");

    let gc = repo.ferry_json(["gc"]);
    assert_eq!(gc["removed"], serde_json::json!([base1]), "{gc}");
    assert!(!repo.base_dir(&base1).exists());
    assert!(repo.base_dir(&base2).is_dir());
    let worktrees = repo.git(repo.root(), ["worktree", "list", "--porcelain"]);
    assert!(!worktrees.contains(&base1), "{worktrees}");

    repo.build("new").assert_rebuilt(&[]);
}

/// The JSON messages a warm command printed; `ferry base` sends build output
/// to stderr. Cargo writes a status line (`    Finished` and the rest) to
/// stderr in two writes while the shim forwards the messages from cargo's
/// stdout, so a message can start in the middle of a line.
fn messages(stderr: &str) -> impl Iterator<Item = &str> {
    stderr
        .lines()
        .filter_map(|l| l.find(r#"{"reason":""#).map(|at| &l[at..]))
}

/// Package names cargo compiled (not fresh) according to [`messages`].
fn compiled_in_base_build(stderr: &str) -> Vec<String> {
    let mut names: Vec<String> = messages(stderr)
        .filter_map(|m| serde_json::from_str::<Value>(m).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["fresh"] == false)
        .map(|m| common::package_name(str_of(&m["package_id"])).to_owned())
        .collect();
    names.sort();
    names.dedup();
    names
}

#[test]
fn a_refreshed_base_is_seeded_from_the_previous_one_and_rebuilds_only_changes() {
    let json_build = [(
        "FERRY_BASE_COMMAND",
        "cargo build --workspace --message-format=json",
    )];
    let repo = TestRepo::with_env(&json_build);
    let base1 = repo.base();
    let before = Snapshot::of(&repo.base_dir(&base1));

    repo.set_marker(repo.root(), "LEAF_V2");
    repo.commit_all("v2");
    let refresh = repo.ferry(["--json", "base"]).ok();
    let report = refresh.json();
    assert_eq!(report["base"]["seeded_from"], base1.as_str(), "{report}");
    assert_eq!(
        compiled_in_base_build(&refresh.stderr),
        ["app", "leaf", "mid"]
    );
    before.assert_unchanged(&Snapshot::of(&repo.base_dir(&base1)));

    repo.spawn("a");
    repo.build("a").assert_rebuilt(&[]);
    assert_eq!(repo.app("a").marker, "LEAF_V2");

    repo.set_marker(repo.root(), "LEAF_V3");
    repo.commit_all("v3");
    let cold = repo.ferry(["--json", "base", "--cold"]).ok();
    assert_eq!(cold.json()["base"]["seeded_from"], Value::Null);
    assert!(compiled_in_base_build(&cold.stderr).len() >= common::ALL_PACKAGES.len());

    // The shim forwards the JSON messages the command asked for (which
    // `compiled_in_base_build` reads above) while recording them, and
    // without printing them a second time.
    let artifacts: Vec<&str> = messages(&refresh.stderr)
        .filter(|m| m.starts_with(r#"{"reason":"compiler-artifact""#))
        .collect();
    let unique: std::collections::BTreeSet<&str> = artifacts.iter().copied().collect();
    assert!(!artifacts.is_empty(), "{}", refresh.stderr);
    assert_eq!(artifacts.len(), unique.len(), "{}", refresh.stderr);
}

/// Files named `name` in the fingerprint dirs of a base's target, in either
/// Cargo build-dir layout: one per unit of that target kind and name.
fn fingerprint_files(repo: &TestRepo, base: &str, name: &str) -> usize {
    let mut count = 0;
    let mut stack = vec![repo.base_dir(base).join("target")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == name) {
                let parent = dir.file_name().unwrap_or_default();
                let grandparent = dir.parent().and_then(|p| p.file_name()).unwrap_or_default();
                if parent == "fingerprint" || grandparent == ".fingerprint" {
                    count += 1;
                }
            }
        }
    }
    count
}

/// A refresh seeded across a manifest change deletes the seed's units it no
/// longer uses. One left behind (here the library with a default feature
/// that B turns off) would look fresh to C, which turns the feature back on,
/// while holding A's code.
#[test]
fn a_feature_switched_off_and_on_is_rebuilt_not_reused() {
    feature_switched_off_and_on(&TestRepo::new());
}

/// Without the namespace sandbox the seed's local units are invalidated when
/// its target is cloned, which leaves their outputs behind.
#[test]
fn a_feature_switched_off_and_on_is_rebuilt_not_reused_without_the_sandbox() {
    feature_switched_off_and_on(&TestRepo::with_env(&[("FERRY_SANDBOX", "off")]));
}

/// Under `--keep-going` Cargo writes the fingerprint dir of a unit that fails
/// to compile but reports nothing for it. After a failed call that is no
/// sign of a cargo call Ferry did not see: the lineage stays observed and
/// seeded across inputs, and the failed unit is swept like an unused one.
#[test]
fn a_unit_that_fails_under_keep_going_does_not_stop_seeding() {
    let repo = TestRepo::with_env(&[(
        "FERRY_BASE_COMMAND",
        "cargo build --workspace && { cargo build --workspace --examples --keep-going || true; }",
    )]);
    let example = repo.root().join("app/examples/broken.rs");
    std::fs::create_dir_all(example.parent().unwrap()).unwrap();
    repo.write(&example, "this is not rust\n");
    repo.commit_all("a broken example");
    feature_switched_off_and_on(&repo);
}

fn feature_switched_off_and_on(repo: &TestRepo) {
    let manifest = repo.root().join("leaf/Cargo.toml");
    let plain = std::fs::read_to_string(&manifest).unwrap();
    let with_feature = format!("{plain}\n[features]\ndefault = [\"x\"]\nx = []\n");

    repo.write(&manifest, &with_feature);
    repo.set_marker(repo.root(), "LEAF_A");
    repo.commit_all("A: feature on");
    let a = repo.ferry_json(["base"]);
    assert_eq!(a["base"]["observed"], true, "{a}");
    let a = str_of(&a["base"]["id"]).to_owned();

    repo.write(&manifest, &plain);
    repo.set_marker(repo.root(), "LEAF_B");
    repo.commit_all("B: feature off");
    // So that B is the newest seed for C.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let b = repo.ferry_json(["base"]);
    assert_eq!(b["base"]["seeded_from"], a.as_str(), "{b}");
    assert!(b["base"]["pruned"]["units"].as_u64() > Some(0), "{b}");
    let b = str_of(&b["base"]["id"]).to_owned();
    assert_eq!(
        fingerprint_files(repo, &b, "lib-leaf"),
        fingerprint_files(repo, &a, "lib-leaf"),
        "B kept A's build of leaf"
    );

    repo.write(&manifest, &with_feature);
    repo.commit_all("C: feature on again");
    let c = repo.ferry_json(["base"]);
    assert_eq!(c["base"]["seeded_from"], b.as_str(), "{c}");

    repo.spawn("agent");
    assert_eq!(repo.app("agent").marker, "LEAF_B");
}

/// Nested workspaces' target dirs are not observed, so a refresh seeded
/// across unit inputs drops them and builds them again. One kept from the
/// seed could hand an agent the nested build of a library whose feature the
/// seed's commit turned off and this one turned back on.
#[test]
fn a_nested_workspace_is_rebuilt_on_a_refresh_across_inputs() {
    let repo = TestRepo::with_env(&[(
        "FERRY_BASE_COMMAND",
        "cargo build --workspace && cargo build --manifest-path fuzz/Cargo.toml",
    )]);
    let fuzz = repo.root().join("fuzz");
    std::fs::create_dir_all(fuzz.join("fz/src")).unwrap();
    repo.write(
        &fuzz.join("Cargo.toml"),
        "[workspace]\nmembers = [\"fz\"]\nresolver = \"2\"\n\n[profile.dev]\ndebug = false\n",
    );
    repo.write(
        &fuzz.join("fz/Cargo.toml"),
        "[package]\nname = \"fz\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n\n\
         [dependencies]\nleaf = { path = \"../../leaf\" }\n",
    );
    repo.write(
        &fuzz.join("fz/src/main.rs"),
        "fn main() {\n    println!(\"marker={}\", leaf::marker());\n}\n",
    );
    common::run(
        repo.cargo_in(&fuzz)
            .args(["generate-lockfile", "--offline"]),
    )
    .ok();
    let manifest = repo.root().join("leaf/Cargo.toml");
    let plain = std::fs::read_to_string(&manifest).unwrap();
    let with_feature = format!("{plain}\n[features]\ndefault = [\"x\"]\nx = []\n");

    repo.write(&manifest, &with_feature);
    repo.set_marker(repo.root(), "LEAF_A");
    repo.commit_all("A: nested workspace, feature on");
    let a = repo.ferry_json(["base"]);
    assert_eq!(a["base"]["observed"], true, "{a}");
    let a = str_of(&a["base"]["id"]).to_owned();

    repo.write(&manifest, &plain);
    repo.set_marker(repo.root(), "LEAF_B");
    repo.commit_all("B: feature off");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let b = repo.ferry_json(["base"]);
    assert_eq!(b["base"]["seeded_from"], a.as_str(), "{b}");
    let b = str_of(&b["base"]["id"]).to_owned();
    let a_built = std::fs::metadata(repo.base_dir(&a).join("base.json"))
        .unwrap()
        .modified()
        .unwrap();
    let mut stack = vec![repo.base_dir(&b).join("target/ferry-ws")];
    let mut files = 0;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else {
                files += 1;
                assert!(
                    meta.modified().unwrap() > a_built,
                    "B kept {}",
                    path.display()
                );
            }
        }
    }
    assert!(files > 0, "B did not build the nested workspace");

    repo.write(&manifest, &with_feature);
    repo.commit_all("C: feature on again");
    let c = repo.ferry_json(["base"]);
    assert_eq!(c["base"]["seeded_from"], b.as_str(), "{c}");

    let spawned = repo.ferry_json(["spawn", "agent"]);
    // Without the namespace sandbox the agent builds at its own path, and
    // Cargo hashes a path package outside the workspace root (leaf, for
    // fuzz) by its absolute path: leaf and fz are new units there.
    let rebuilt: &[&str] = if spawned["agent"]["isolation"] == "namespace" {
        &[]
    } else {
        &["fz", "leaf"]
    };
    let nested = ["--manifest-path", "fuzz/Cargo.toml"];
    Build::run(repo.agent_cargo("agent").arg("build").args(nested)).assert_rebuilt(rebuilt);
    let out = common::run(repo.agent_cargo("agent").args(["run", "-q"]).args(nested)).ok();
    assert_eq!(out.stdout.trim(), "marker=LEAF_B", "{out}");
}

/// The unit hashes of `package` in a base's dev profile, in either layout.
fn unit_hashes(repo: &TestRepo, base: &str, package: &str) -> Vec<String> {
    let profile = repo.base_dir(base).join("target/debug");
    let names = |dir: std::path::PathBuf| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.unwrap().file_name().into_string().ok())
            .collect::<Vec<_>>()
    };
    let prefix = format!("{package}-");
    let mut hashes: Vec<String> = names(profile.join(".fingerprint"))
        .into_iter()
        .chain(names(profile.join("build")))
        .filter_map(|n| n.strip_prefix(&prefix).map(str::to_owned))
        .collect();
    hashes.extend(names(profile.join("build").join(package)));
    assert!(!hashes.is_empty(), "no units of {package} in base {base}");
    hashes
}

fn set_leaf_version(repo: &TestRepo, version: &str) {
    let manifest = repo.root().join("leaf/Cargo.toml");
    let text: String = std::fs::read_to_string(&manifest)
        .unwrap()
        .lines()
        .map(|l| {
            if l.starts_with("version") {
                format!("version = \"{version}\"\n")
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    repo.write(&manifest, &text);
    common::run(
        repo.cargo_in(repo.root())
            .args(["generate-lockfile", "--offline"]),
    )
    .ok();
}

#[test]
fn a_reverted_version_bump_is_rebuilt_not_reused() {
    let repo = TestRepo::new();
    repo.set_marker(repo.root(), "LEAF_A");
    repo.commit_all("A: leaf 0.1.0");
    let a = repo.base();
    let a_leaf = unit_hashes(&repo, &a, "leaf");

    set_leaf_version(&repo, "0.2.0");
    repo.set_marker(repo.root(), "LEAF_B");
    repo.commit_all("B: leaf 0.2.0");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let b = repo.ferry_json(["base"]);
    assert_eq!(b["base"]["seeded_from"], a.as_str(), "{b}");
    let b = str_of(&b["base"]["id"]).to_owned();
    let mut stack = vec![repo.base_dir(&b).join("target")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let text = path.to_string_lossy();
            if let Some(hash) = a_leaf.iter().find(|h| text.contains(h.as_str())) {
                panic!("B kept A's leaf unit {hash}: {}", path.display());
            }
            if path.is_dir() {
                stack.push(path);
            }
        }
    }

    set_leaf_version(&repo, "0.1.0");
    repo.commit_all("C: leaf 0.1.0 again");
    let c = repo.ferry_json(["base"]);
    assert_eq!(c["base"]["seeded_from"], b.as_str(), "{c}");
    repo.spawn("agent");
    assert_eq!(repo.app("agent").marker, "LEAF_B");
}

/// `CARGO_INCREMENTAL` changes the profile of every local unit, and so its
/// hash. A refresh seeded as if it did not would keep the seed's units
/// built with `CARGO_INCREMENTAL=0`, which an agent building with it would
/// find fresh, holding the seed's code.
#[test]
fn a_refresh_across_a_cargo_incremental_change_is_pruned() {
    let repo = TestRepo::new();
    let a = common::run(
        repo.ferry_cmd(["--json", "base"])
            .env("CARGO_INCREMENTAL", "0"),
    )
    .ok()
    .json();
    let a = str_of(&a["base"]["id"]).to_owned();

    repo.set_marker(repo.root(), "LEAF_B");
    repo.commit_all("B: source");
    let b = repo.ferry_json(["base"]);
    assert_eq!(b["base"]["seeded_from"], a.as_str(), "{b}");

    repo.spawn("agent");
    let app = App::run(repo.agent_cargo("agent").env("CARGO_INCREMENTAL", "0"));
    assert_eq!(app.marker, "LEAF_B");
    assert!(b["base"]["pruned"]["units"].as_u64() > Some(0), "{b}");
}

/// Two builds of one binary with different features are both fresh in a
/// refresh and both uplifted to target/debug/app; pruning must tell them
/// apart (on Linux by the hard link Cargo made) and keep both. Not on Windows:
/// with MSVC, Cargo names binaries without a hash, so the variants share
/// `deps/app.exe` and Cargo itself relinks when switching between them.
#[cfg(not(windows))]
#[test]
fn fresh_variants_of_one_binary_survive_pruning() {
    let repo = TestRepo::with_env(&[(
        "FERRY_BASE_COMMAND",
        "cargo build --workspace && cargo build -p app --features extra",
    )]);
    add_line(&repo, "app/Cargo.toml", "\n[features]\nextra = []");
    repo.commit_all("app: feature extra");
    let base1 = repo.base();

    add_line(
        &repo,
        "side/Cargo.toml",
        "# unit inputs change, units do not",
    );
    repo.commit_all("side: comment");
    let base2 = repo.ferry_json(["base"]);
    assert_eq!(base2["base"]["seeded_from"], base1.as_str(), "{base2}");
    assert_eq!(base2["base"]["observed"], true, "{base2}");
    assert_eq!(base2["base"]["pruned"]["units"], 0, "{base2}");

    repo.spawn("a");
    repo.build("a").assert_rebuilt(&[]);
    Build::run(
        repo.agent_cargo("a")
            .args(["build", "-p", "app", "--features", "extra"]),
    )
    .assert_rebuilt(&[]);
}

fn add_line(repo: &TestRepo, file: &str, line: &str) {
    let path = repo.root().join(file);
    let text = std::fs::read_to_string(&path).unwrap();
    repo.write(&path, &format!("{text}{line}\n"));
}

#[test]
fn an_unobservable_lineage_is_not_seeded_across_inputs() {
    let repo = TestRepo::with_env(&[(
        "FERRY_BASE_COMMAND",
        "cargo build --workspace --message-format=short",
    )]);
    let a = repo.ferry_json(["base"]);
    assert_eq!(a["base"]["observed"], false, "{a}");

    add_line(&repo, "leaf/Cargo.toml", "# B");
    repo.commit_all("B: manifest");
    let b = repo.ferry_json(["base"]);
    assert_eq!(b["base"]["seeded_from"], Value::Null, "{b}");

    repo.set_marker(repo.root(), "LEAF_C");
    repo.commit_all("C: source");
    let c = repo.ferry_json(["base"]);
    assert_eq!(c["base"]["seeded_from"], b["base"]["id"], "{c}");
}

#[test]
fn pruning_can_be_turned_off() {
    let repo = TestRepo::with_env(&[("FERRY_BASE_PRUNE", "0")]);
    let a = repo.ferry_json(["base"]);
    assert_eq!(a["base"]["observed"], false, "{a}");
    add_line(&repo, "leaf/Cargo.toml", "# B");
    repo.commit_all("B: manifest");
    let b = repo.ferry_json(["base"]);
    assert_eq!(b["base"]["seeded_from"], Value::Null, "{b}");
}

/// The warm commands are part of a base's fingerprint, but a script they
/// run is not. When it stops being observable, the seeded refresh is redone
/// from scratch instead of publishing a target that may hold unused units.
#[test]
fn observability_lost_mid_lineage_falls_back_to_a_cold_refresh() {
    let repo = TestRepo::with_env(&[("FERRY_BASE_COMMAND", "sh ./warm.sh")]);
    let script = repo.root().join("warm.sh");
    repo.write(&script, "cargo build --workspace\n");
    repo.commit_all("A: warm script");
    let a = repo.ferry_json(["base"]);
    assert_eq!(a["base"]["observed"], true, "{a}");

    repo.write(&script, "cargo build --workspace --message-format=short\n");
    add_line(&repo, "leaf/Cargo.toml", "# B");
    repo.set_marker(repo.root(), "LEAF_B");
    repo.commit_all("B: unobservable script");
    let b = repo.ferry_json(["base"]);
    assert_eq!(b["base"]["seeded_from"], Value::Null, "{b}");
    assert_eq!(b["base"]["observed"], false, "{b}");
    assert!(b["warnings"].to_string().contains("from scratch"), "{b}");

    repo.spawn("agent");
    repo.build("agent").assert_rebuilt(&[]);
    assert_eq!(repo.app("agent").marker, "LEAF_B");
}

#[test]
fn program_output_of_warm_commands_is_forwarded() {
    let repo = TestRepo::with_env(&[(
        "FERRY_BASE_COMMAND",
        "cargo build --workspace && cargo run -q -p app",
    )]);
    let run = repo.ferry(["--json", "base"]).ok();
    assert_eq!(run.json()["base"]["observed"], true, "{run}");
    assert!(
        run.stderr.contains(&format!("marker={BASE_MARKER}")),
        "{run}"
    );
    assert_eq!(messages(&run.stderr).count(), 0, "{run}");
}

#[test]
fn a_failing_warm_cargo_call_fails_the_base() {
    let repo = TestRepo::with_env(&[("FERRY_BASE_COMMAND", "cargo build -p nosuch")]);
    let run = repo.ferry(["base"]).failed();
    assert!(run.stderr.contains("cargo build -p nosuch"), "{run}");
    let list = repo.ferry_json(["list"]);
    assert_eq!(list["bases"], serde_json::json!([]), "{list}");
}
