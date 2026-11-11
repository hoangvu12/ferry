//! A tracked symlink that leads elsewhere in an agent than in its base must
//! not let Cargo keep the base's artifacts. Cargo records a rustc input read
//! through a symlink by its canonical path in the base when that lies inside
//! the input's package, as here, and follows a build script's
//! `rerun-if-changed` path through the link in the agent. Both targets can
//! be identical to the base's, so spawn leaves both unstamped. Each test
//! expects what a cold build of the agent's commit prints.

// clippy's allow-unwrap-in-tests covers only #[test] functions, not their
// helpers.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use common::{TestRepo, is_stamped, path_of, str_of};

const INCLUDE_CURRENT: &str =
    "pub fn marker() -> &'static str {\n    include_str!(\"../data/current.txt\")\n}\n";

const PATH_MODULE: &str = "#[path = \"../impl/marker.rs\"]\nmod imp;\n\n\
     pub fn marker() -> &'static str {\n    imp::MARKER\n}\n";

/// Writes regular files under `leaf/`, replacing any symlink in the way.
fn put(repo: &TestRepo, files: &[(&str, &str)]) {
    for (path, contents) in files {
        let path = repo.root().join("leaf").join(path);
        if path.is_symlink() {
            fs::remove_file(&path).unwrap();
        }
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
}

/// Makes `leaf/<at>` a symlink to `target`, replacing what is there.
fn link(repo: &TestRepo, at: &str, target: &str) {
    let at = repo.root().join("leaf").join(at);
    if at.symlink_metadata().is_ok() {
        fs::remove_file(&at).unwrap();
    }
    symlink(target, at).unwrap();
}

fn set_leaf(repo: &TestRepo, source: &str) {
    repo.write(&repo.root().join("leaf/src/lib.rs"), source);
}

/// Whether `path` is the Cargo dep-info file of leaf's library, in either
/// build-dir layout: `.fingerprint/leaf-<H>/` or `build/leaf/<H>/fingerprint/`.
fn is_leaf_dep_info(path: &Path) -> bool {
    let parts: Vec<&str> = path.iter().rev().map(|p| p.to_str().unwrap()).collect();
    match parts[..] {
        ["dep-lib-leaf", unit, ".fingerprint", ..] => unit.starts_with("leaf-"),
        ["dep-lib-leaf", "fingerprint", _, "leaf", "build", ..] => true,
        _ => false,
    }
}

/// Leaf's dep-info files below `dir`, with their sizes.
fn leaf_dep_info(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else if is_leaf_dep_info(&entry.path()) {
                found.push((entry.path(), entry.metadata().unwrap().len()));
            }
        }
    }
    found
}

/// Requires that `dep_info`, leaf's dep-info files with their sizes, lists
/// at least one and that all are empty, so Cargo rebuilds leaf under
/// checksum freshness too. Stable Cargo compares mtimes and rebuilds leaf
/// anyway, so only this shows there that spawn or a refresh invalidated it.
#[track_caller]
fn assert_emptied(dep_info: &[(PathBuf, u64)], context: &str) {
    assert!(!dep_info.is_empty(), "no dep-info of leaf in {context}");
    for (path, size) in dep_info {
        assert_eq!(*size, 0, "{}", path.display());
    }
}

/// The base has `current.txt -> v1.txt`, the agent `current.txt -> v2.txt`.
fn retarget_current_txt(repo: &TestRepo) {
    put(repo, &[("data/v1.txt", "ONE"), ("data/v2.txt", "TWO")]);
    link(repo, "data/current.txt", "v1.txt");
    set_leaf(repo, INCLUDE_CURRENT);
    repo.commit_all("current.txt -> v1.txt");
    repo.base();
    link(repo, "data/current.txt", "v2.txt");
    repo.commit_all("current.txt -> v2.txt");
    let report = repo.ferry_json(["spawn", "a"]);
    let worktree = path_of(&report["agent"]["worktree"]);
    for target in ["leaf/data/v1.txt", "leaf/data/v2.txt"] {
        assert!(!is_stamped(&worktree.join(target)), "{target}");
    }
    // An overlay fork takes what spawn writes in its upper dir, which holds
    // nothing else before the first build.
    let fork = match str_of(&report["agent"]["backend"]) {
        "overlay" => repo.store().join("agents/a/upper"),
        _ => repo.agent_target("a"),
    };
    assert_emptied(&leaf_dep_info(&fork), &fork.display().to_string());
}

#[test]
fn a_retargeted_symlink_is_not_hidden_from_cargo() {
    let repo = TestRepo::new();
    retarget_current_txt(&repo);
    repo.build("a").assert_rebuilt(&["app", "leaf", "mid"]);
    assert_eq!(repo.app("a").marker, "TWO");
}

#[test]
fn a_retargeted_symlink_is_not_hidden_from_cargo_without_a_sandbox() {
    let repo = TestRepo::with_env(&[("FERRY_SANDBOX", "off")]);
    retarget_current_txt(&repo);
    assert_eq!(repo.app("a").marker, "TWO");
}

#[test]
fn a_file_replaced_by_a_symlink_is_not_hidden_from_cargo() {
    let repo = TestRepo::new();
    put(
        &repo,
        &[("data/current.txt", "ONE"), ("data/v2.txt", "TWO")],
    );
    set_leaf(&repo, INCLUDE_CURRENT);
    repo.commit_all("current.txt is a file");
    repo.base();
    link(&repo, "data/current.txt", "v2.txt");
    repo.commit_all("current.txt -> v2.txt");
    let worktree = repo.spawn("a");

    assert!(!is_stamped(&worktree.join("leaf/data/v2.txt")));
    assert_eq!(repo.app("a").marker, "TWO");
}

#[test]
fn a_symlink_replaced_by_a_file_is_not_hidden_from_cargo() {
    let repo = TestRepo::new();
    put(&repo, &[("data/v1.txt", "ONE")]);
    link(&repo, "data/current.txt", "v1.txt");
    set_leaf(&repo, INCLUDE_CURRENT);
    repo.commit_all("current.txt -> v1.txt");
    repo.base();
    put(&repo, &[("data/current.txt", "TWO")]);
    repo.commit_all("current.txt is a file");
    let worktree = repo.spawn("a");

    // What Cargo checks is the input the base read, v1.txt.
    assert!(!is_stamped(&worktree.join("leaf/data/v1.txt")));
    assert_eq!(repo.app("a").marker, "TWO");
}

#[test]
fn a_retargeted_directory_symlink_is_not_hidden_from_cargo() {
    let repo = TestRepo::new();
    put(
        &repo,
        &[
            ("impl_v1/marker.rs", "pub const MARKER: &str = \"ONE\";\n"),
            ("impl_v2/marker.rs", "pub const MARKER: &str = \"TWO\";\n"),
        ],
    );
    link(&repo, "impl", "impl_v1");
    set_leaf(&repo, PATH_MODULE);
    repo.commit_all("impl -> impl_v1");
    repo.base();
    link(&repo, "impl", "impl_v2");
    repo.commit_all("impl -> impl_v2");
    let worktree = repo.spawn("a");

    for reached in [
        "impl_v1",
        "impl_v1/marker.rs",
        "impl_v2",
        "impl_v2/marker.rs",
    ] {
        assert!(
            !is_stamped(&worktree.join("leaf").join(reached)),
            "{reached}"
        );
    }
    assert!(is_stamped(&worktree.join("leaf/src/lib.rs")));
    assert_eq!(repo.app("a").marker, "TWO");
}

#[test]
fn a_path_climbing_out_of_a_retargeted_directory_symlink_is_not_hidden_from_cargo() {
    let repo = TestRepo::new();
    put(
        &repo,
        &[
            ("v1/impl/README", "impl"),
            ("v2/impl/README", "impl"),
            ("v1/VERSION", "ONE"),
            ("v2/VERSION", "TWO"),
        ],
    );
    link(&repo, "impl", "v1/impl");
    // The base reads v1/VERSION; Cargo records it by that canonical path.
    set_leaf(
        &repo,
        "pub fn marker() -> &'static str {\n    include_str!(\"../impl/../VERSION\")\n}\n",
    );
    repo.commit_all("impl -> v1/impl");
    repo.base();
    link(&repo, "impl", "v2/impl");
    repo.commit_all("impl -> v2/impl");
    let worktree = repo.spawn("a");

    assert!(!is_stamped(&worktree.join("leaf/v1/VERSION")));
    assert_eq!(repo.app("a").marker, "TWO");
}

#[test]
fn an_unchanged_symlink_leaves_everything_else_stamped() {
    let repo = TestRepo::new();
    put(
        &repo,
        &[
            ("impl_v1/marker.rs", "pub const MARKER: &str = \"ONE\";\n"),
            ("data/v1.txt", "data"),
            ("data/notes.txt", "notes"),
        ],
    );
    link(&repo, "impl", "impl_v1");
    link(&repo, "data/current.txt", "v1.txt");
    set_leaf(
        &repo,
        &format!(
            "{PATH_MODULE}\npub fn data() -> &'static str {{\n    \
             include_str!(\"../data/current.txt\")\n}}\n"
        ),
    );
    repo.commit_all("symlinks");
    repo.base();
    put(&repo, &[("data/notes.txt", "edited notes")]);
    repo.commit_all("edit notes");

    let report = repo.ferry_json(["spawn", "a"]);
    let worktree = path_of(&report["agent"]["worktree"]);
    // The two symlinks and notes.txt.
    assert_eq!(report["agent"]["stamp"]["skipped"], 3, "{report}");
    for stamped in [
        "leaf/impl_v1",
        "leaf/impl_v1/marker.rs",
        "leaf/data",
        "leaf/data/v1.txt",
    ] {
        assert!(is_stamped(&worktree.join(stamped)), "{stamped}");
    }
    repo.build("a").assert_rebuilt(&[]);
    assert_eq!(repo.app("a").marker, "ONE");
}

/// A refresh seeded from the base before the retarget builds what a cold
/// build of its commit would; its agents are stamped against it.
#[test]
fn a_refresh_seeded_across_a_retargeted_symlink_is_not_hidden_from_cargo() {
    // The warm build rewrites leaf's dep-info, so the default warm commands
    // run after listing the size of each `dep-lib-leaf` in the target as the
    // refresh prepared it. The trailing slash makes `find` enter the target
    // dir when it is a symlink (remap mode's `.ferry-target`).
    let repo = TestRepo::with_env(&[(
        "FERRY_BASE_COMMAND",
        "find \"$CARGO_TARGET_DIR/\" -name dep-lib-leaf -exec wc -c {} \\; && \
         cargo build && cargo test --no-run",
    )]);
    put(&repo, &[("data/v1.txt", "ONE"), ("data/v2.txt", "TWO")]);
    link(&repo, "data/current.txt", "v1.txt");
    set_leaf(&repo, INCLUDE_CURRENT);
    repo.commit_all("current.txt -> v1.txt");
    let seed = repo.base();
    link(&repo, "data/current.txt", "v2.txt");
    repo.commit_all("current.txt -> v2.txt");
    let refresh = repo.ferry(["--json", "base"]).ok();
    let report = refresh.json();
    assert_eq!(report["base"]["seeded_from"], seed.as_str(), "{report}");
    // `wc -c` prints `<size> <path>`, the path as the sandbox shows it.
    let listed: Vec<(PathBuf, u64)> = refresh
        .stderr
        .lines()
        .filter_map(|line| {
            let (size, path) = line.trim_start().split_once(' ')?;
            is_leaf_dep_info(Path::new(path)).then(|| (path.into(), size.parse().unwrap()))
        })
        .collect();
    assert_emptied(&listed, &refresh.to_string());
    repo.spawn("a");
    repo.build("a").assert_rebuilt(&[]);
    assert_eq!(repo.app("a").marker, "TWO");
}
