//! A warm workspace and target dir copied to another path, as a fork without
//! the namespace sandbox sees them: the units that captured the old absolute
//! paths must rebuild, and nothing else.

#![allow(
    clippy::unwrap_used,
    reason = "helpers outside #[test] functions fail the test by panicking"
)]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;
use std::process::Command;

use ferry::freshness::{Origin, apply_relocation, plan_relocation};

fn cargo() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

fn runs(program: impl AsRef<OsStr>, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .output()
        .is_ok_and(|out| out.status.success())
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn manifest(name: &str, dependencies: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\n{dependencies}"
    )
}

/// Builds the workspace at `ws` into `target`, as Ferry would, and returns
/// whether Cargo considered each target fresh.
fn build(ws: &Path, target: &Path) -> BTreeMap<String, bool> {
    let out = Command::new(cargo())
        .args(["build", "--offline", "--message-format=json"])
        .current_dir(ws)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_BUILD_BUILD_DIR", target)
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "cargo build failed:\n{stderr}");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|msg| msg["reason"] == "compiler-artifact")
        .map(|msg| {
            let name = msg["target"]["name"].as_str().unwrap().to_owned();
            (name, msg["fresh"].as_bool().unwrap())
        })
        .collect()
}

fn run_app(target: &Path) -> Vec<String> {
    let out = Command::new(target.join("debug/app")).output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn relocated_units_rebuild_and_see_the_new_paths() {
    if !runs(cargo(), "-V") || !runs("cc", "--version") {
        eprintln!("skipping: this test needs cargo and a C linker (`cc`) on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = ferry::sys::canonicalize(tmp.path()).unwrap();
    // Laid out as Ferry lays out a base and an agent.
    let store = root.join("store/repo-0123456789abcdef");
    let base = store.join("bases/a");
    let old_ws = base.join("src");
    write(
        &old_ws.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"shared\", \"plain\"]\nresolver = \"2\"\n",
    );
    write(&old_ws.join("shared.txt"), "v1\n");
    write(
        &old_ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "shared = { path = \"../shared\" }\nplain = { path = \"../plain\" }\n",
        ),
    );
    write(
        &old_ws.join("app/src/main.rs"),
        "fn main() {\n    println!(\"{}\", env!(\"CARGO_MANIFEST_DIR\"));\n    \
         println!(\"{}\", shared::TEXT.trim());\n    println!(\"{}\", plain::NAME);\n}\n",
    );
    // A workspace file outside the package: Cargo <= 1.100 records it by
    // absolute path.
    write(&old_ws.join("shared/Cargo.toml"), &manifest("shared", ""));
    write(
        &old_ws.join("shared/src/lib.rs"),
        "pub const TEXT: &str = include_str!(\"../../shared.txt\");\n",
    );
    write(&old_ws.join("plain/Cargo.toml"), &manifest("plain", ""));
    write(
        &old_ws.join("plain/src/lib.rs"),
        "pub const NAME: &str = \"plain\";\n",
    );
    let old_target = base.join("target");
    build(&old_ws, &old_target);

    let agent = root.join("agent");
    let (new_ws, new_target) = (agent.join("ws"), agent.join("target"));
    fs::create_dir_all(&agent).unwrap();
    for (from, to) in [(&old_ws, &new_ws), (&old_target, &new_target)] {
        let copied = Command::new("cp")
            .arg("-a")
            .arg(from)
            .arg(to)
            .status()
            .unwrap();
        assert!(copied.success());
    }
    let origin = Origin {
        store: store.clone(),
        checkout: old_ws.clone(),
        host_checkout: old_ws.clone(),
        git_dir: root.join("repo/.git/worktrees/src"),
        live: Some("a".into()),
    };
    let plan = plan_relocation(&old_target, &origin).unwrap();
    let units: Vec<&str> = plan.remove.iter().map(|r| r.unit.as_str()).collect();
    assert!(units.iter().any(|u| u.starts_with("app ")), "{units:?}");
    assert!(!units.iter().any(|u| u.starts_with("plain ")), "{units:?}");
    apply_relocation(&new_target, &plan).unwrap();

    let rebuilt = build(&new_ws, &new_target);
    assert_eq!(rebuilt.get("app"), Some(&false), "{rebuilt:?}");
    assert_eq!(rebuilt.get("plain"), Some(&true), "{rebuilt:?}");
    let manifest_dir = new_ws.join("app").display().to_string();
    assert_eq!(run_app(&new_target), [manifest_dir.as_str(), "v1", "plain"]);

    // The old workspace still exists, so a unit still watching its files
    // would stay fresh and keep printing v1.
    write(&new_ws.join("shared.txt"), "v2\n");
    build(&new_ws, &new_target);
    assert_eq!(run_app(&new_target)[1], "v2");
}
