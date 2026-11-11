//! Experimental remap mode: a warm target forked to another path keeps rustc's
//! incremental state for workspace libraries when builds go through
//! `ferry-rustc`.
//!
//! Reuse is counted by inode: rustc hard-links
//! unchanged work products into its new session directory, so a reused
//! codegen unit's object file keeps the inode it had in the copied target.
//! Those objects stay linked outside the target until they are counted, so
//! no new object can be given one of their inode numbers.

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "helpers outside #[test] functions fail the test by panicking"
)]

mod common;

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Snapshot, TestRepo, str_of};
use ferry::remap::{TARGET_LINK, WRAPPER_NAME};
use ferry::toolchain::Toolchain;

const LIB: &str = "remaplib";
const BIN: &str = "remapbin";
const MODULES: usize = 12;

#[derive(Debug, Clone, Copy)]
struct Reuse {
    reused: usize,
    total: usize,
}

#[derive(Debug)]
struct Fork {
    lib: Reuse,
    bin: Reuse,
}

#[test]
fn forked_target_keeps_incremental_state_through_the_wrapper() {
    if !remap_supported() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // Canonical, so that macOS's /var -> /private/var does not matter here.
    let root = fs::canonicalize(tmp.path()).unwrap();
    let wrapper = wrapper_in(&root);

    let with = fork_and_edit(&root.join("with"), Some(&wrapper));
    let without = fork_and_edit(&root.join("without"), None);
    report("fork + edit", &with, &without);
    assert_reuse(&with, &without);
}

/// The same through Ferry: a base and an agent without the namespace
/// sandbox, so the agent builds at another path than its base did. The
/// timed case of the benchmark's `edit_on_spawn`: spawn, edit, build.
#[test]
fn ferry_agents_reuse_incremental_state_with_remap_paths() {
    if !remap_supported() {
        return;
    }
    let with = agent_first_edit(true);
    let without = agent_first_edit(false);
    report("ferry spawn + edit", &with, &without);
    assert_reuse(&with, &without);
}

/// Skips the calling test (or fails it, when `FERRY_TEST_REQUIRE` lists
/// `remap`) unless the host rustc can remap.
fn remap_supported() -> bool {
    let rustc = Command::new("rustc").arg("-vV").output().unwrap();
    let toolchain = Toolchain::parse(&String::from_utf8_lossy(&rustc.stdout), "");
    match ferry::remap::supported(&toolchain) {
        Ok(()) => true,
        Err(why) => {
            assert!(
                !common::required("remap"),
                "remap required by FERRY_TEST_REQUIRE but unsupported: {why}"
            );
            common::skip(&why);
            false
        }
    }
}

/// Printed uncaptured, so CI logs carry the measured counts.
fn report(what: &str, with: &Fork, without: &Fork) {
    for (krate, with, without) in [(LIB, with.lib, without.lib), (BIN, with.bin, without.bin)] {
        let _ = writeln!(
            std::io::stderr(),
            "{krate}: {}/{} codegen units reused after {what} with {WRAPPER_NAME}, {}/{} without",
            with.reused,
            with.total,
            without.reused,
            without.total
        );
    }
}

fn assert_reuse(with: &Fork, without: &Fork) {
    // On targets that embed gdb scripts (Linux), every codegen unit of a
    // binary reads the absolute paths of its dependencies, so the binary
    // rebuilds fully either way. Apple targets embed none.
    if cfg!(target_vendor = "apple") {
        assert!(with.bin.total > MODULES, "{:?}", with.bin);
        assert!(with.bin.reused + 2 >= with.bin.total, "{:?}", with.bin);
    }
    let (with, without) = (with.lib, without.lib);
    assert!(with.total > MODULES, "{with:?}");
    assert!(with.reused + 2 >= with.total, "{with:?}");
    assert!(
        without.reused * 10 <= without.total,
        "the control reused codegen units, so the test measures nothing: {without:?}"
    );
}

#[test]
fn wrapper_passes_probes_through_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let wrapper = wrapper_in(tmp.path());
    let direct = Command::new("rustc").arg("-vV").output().unwrap();
    let wrapped = Command::new(&wrapper)
        .args(["rustc", "-vV"])
        .output()
        .unwrap();
    assert!(wrapped.status.success(), "{wrapped:?}");
    assert_eq!(wrapped.stdout, direct.stdout);
    let missing = Command::new(&wrapper)
        .arg(tmp.path().join("no-such-rustc"))
        .arg("-vV")
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(127), "{missing:?}");
}

fn wrapper_in(dir: &Path) -> PathBuf {
    let wrapper = dir.join("bin").join(WRAPPER_NAME);
    fs::create_dir_all(dir.join("bin")).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_ferry"), &wrapper).unwrap();
    wrapper
}

/// Builds a workspace at `<dir>/a`, forks it and its target to new paths,
/// deletes the originals, edits both crates and rebuilds in the fork.
fn fork_and_edit(dir: &Path, wrapper: Option<&Path>) -> Fork {
    let (a, b) = (dir.join("a"), dir.join("b"));
    let (target_a, target_b) = (dir.join("target-a"), dir.join("target-b"));
    write_workspace(&a);
    fs::create_dir_all(&target_a).unwrap();
    run(Command::new("git").args(["init", "-q"]).arg(&a));
    ferry::remap::prepare(&a, &target_a).unwrap();
    assert_eq!(cargo_build(&a, wrapper), [BIN, LIB]);

    run(Command::new("cp").arg("-a").arg(&a).arg(&b));
    run(Command::new("cp").arg("-a").arg(&target_a).arg(&target_b));
    fs::remove_dir_all(&a).unwrap();
    fs::remove_dir_all(&target_a).unwrap();
    ferry::remap::prepare(&b, &target_b).unwrap();
    assert_eq!(fs::read_link(b.join(TARGET_LINK)).unwrap(), target_b);

    edit_workspace(&b);
    let incremental = target_b.join("debug/incremental");
    let before = Before::hold(&incremental, dir);
    assert_eq!(cargo_build(&b, wrapper), [BIN, LIB]);

    let output = Command::new(target_b.join("debug").join(BIN))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), edited_output());
    let fork = reuse_since(&incremental, before);
    assert_eq!(
        cargo_build(&b, wrapper),
        Vec::<String>::new(),
        "no-op rebuild"
    );
    fs::remove_dir_all(dir).unwrap();
    fork
}

/// `ferry base`, then `ferry spawn`, an edit of both crates and the agent's
/// first build; the base must come out of it unchanged.
fn agent_first_edit(remap: bool) -> Fork {
    let repo = TestRepo::with_workspace(
        &[
            ("FERRY_SANDBOX", "off"),
            ("FERRY_REMAP_PATHS", if remap { "1" } else { "0" }),
        ],
        |root| {
            write_workspace(root);
            // Committed, so the agent's checkout has nothing untracked.
            run(Command::new(env!("CARGO"))
                .args(["generate-lockfile", "--offline"])
                .current_dir(root));
        },
    );
    let report = repo.ferry_json(["base"]);
    assert_eq!(report["base"]["isolation"], "none", "{report}");
    assert_eq!(report["base"]["remap_paths"], remap, "{report}");
    let base_dir = repo.base_dir(str_of(&report["base"]["id"]));
    let base_before = Snapshot::of(&base_dir);

    let worktree = repo.spawn("a");
    let target = repo.store().join("agents/a/target");
    if remap {
        assert_eq!(fs::read_link(worktree.join(TARGET_LINK)).unwrap(), target);
    }
    let status = ["status", "--porcelain", "--untracked-files=all"];
    assert_eq!(repo.git(&worktree, status), "");

    edit_workspace(&worktree);
    let incremental = target.join("debug/incremental");
    let before = Before::hold(&incremental, repo.scratch());
    repo.build("a").assert_rebuilt(&[BIN, LIB]);
    let fork = reuse_since(&incremental, before);
    let output = common::run(repo.agent_cargo("a").args(["run", "-q", "-p", BIN])).ok();
    assert_eq!(output.stdout, edited_output());
    repo.build("a").assert_rebuilt(&[]);

    repo.git(&worktree, ["checkout", "--", "."]);
    // Without --force: .ferry-target must count as build output.
    repo.ferry(["rm", "a"]).ok();
    base_before.assert_unchanged(&Snapshot::of(&base_dir));
    fork
}

/// The object files under an incremental dir before a build, each also
/// hard-linked into a directory outside the target so that its inode stays
/// allocated until [`reuse_since`] has counted. rustc may delete the
/// previous session's objects before it writes the new ones, and ext4 gives
/// a freed inode number to the next file it creates, which would make a new
/// object look reused.
struct Before {
    inodes: HashSet<u64>,
    _links: tempfile::TempDir,
}

impl Before {
    /// `outside` must be on the target's filesystem.
    fn hold(incremental: &Path, outside: &Path) -> Self {
        let links = tempfile::tempdir_in(outside).unwrap();
        let mut inodes = HashSet::new();
        for (i, object) in object_files(incremental).iter().enumerate() {
            let link = links.path().join(i.to_string());
            fs::hard_link(object, &link).unwrap();
            inodes.insert(fs::metadata(&link).unwrap().ino());
        }
        Self {
            inodes,
            _links: links,
        }
    }
}

/// Codegen units of the newest sessions under `incremental` whose object
/// files are among `before`, which is released afterwards.
fn reuse_since(incremental: &Path, before: Before) -> Fork {
    let reuse = |krate| {
        let objects: HashSet<u64> = object_files(&newest_session(incremental, krate))
            .iter()
            .map(|object| fs::metadata(object).unwrap().ino())
            .collect();
        Reuse {
            reused: objects.intersection(&before.inodes).count(),
            total: objects.len(),
        }
    };
    let fork = Fork {
        lib: reuse(LIB),
        bin: reuse(BIN),
    };
    drop(before);
    fork
}

/// A library and a binary using it, each with one module (and so one
/// codegen unit) per `MODULES`.
fn write_workspace(root: &Path) {
    let manifest = |name: &str, deps: &str| {
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{deps}"
        )
    };
    for name in [LIB, BIN] {
        fs::create_dir_all(root.join(name).join("src")).unwrap();
    }
    fs::write(
        root.join("Cargo.toml"),
        format!("[workspace]\nmembers = [\"{LIB}\", \"{BIN}\"]\nresolver = \"3\"\n"),
    )
    .unwrap();
    fs::write(root.join(LIB).join("Cargo.toml"), manifest(LIB, "")).unwrap();
    fs::write(
        root.join(BIN).join("Cargo.toml"),
        manifest(BIN, &format!("{LIB} = {{ path = \"../{LIB}\" }}\n")),
    )
    .unwrap();
    fs::write(root.join(LIB).join("src/lib.rs"), modules()).unwrap();
    fs::write(
        root.join(BIN).join("src/main.rs"),
        main_rs(&format!(
            "println!(\"{{}} {{}}\", {LIB}::total(2), total(1));"
        )),
    )
    .unwrap();
}

/// Edits both crates; appended, so every existing item keeps its position.
fn edit_workspace(root: &Path) {
    fs::write(
        root.join(LIB).join("src/lib.rs"),
        modules() + "\npub fn added() -> usize {\n    42\n}\n",
    )
    .unwrap();
    fs::write(
        root.join(BIN).join("src/main.rs"),
        main_rs(&format!(
            "println!(\"{{}} {{}} {{}}\", {LIB}::total(2), total(1), {LIB}::added());"
        )),
    )
    .unwrap();
}

/// What the binary prints after [`edit_workspace`].
fn edited_output() -> String {
    let total = |x: usize| (0..MODULES).map(|i| x * (i + 1) + i).sum::<usize>();
    format!("{} {} 42\n", total(2), total(1))
}

fn modules() -> String {
    let mut src = String::new();
    for i in 0..MODULES {
        let factor = i + 1;
        writeln!(
            src,
            "pub mod m{i} {{\n    pub fn value(x: usize) -> usize {{\n        x * {factor} + {i}\n    }}\n}}\n"
        )
        .unwrap();
    }
    let sum: Vec<String> = (0..MODULES).map(|i| format!("m{i}::value(x)")).collect();
    writeln!(
        src,
        "pub fn total(x: usize) -> usize {{\n    {}\n}}",
        sum.join(" + ")
    )
    .unwrap();
    src
}

fn main_rs(body: &str) -> String {
    format!("{}\nfn main() {{\n    {body}\n}}\n", modules())
}

/// Names of the units cargo rebuilt, sorted.
fn cargo_build(dir: &Path, wrapper: Option<&Path>) -> Vec<String> {
    let target = dir.join(TARGET_LINK);
    // Empty values also override wrappers and flags from the user's config.
    let output = Command::new(env!("CARGO"))
        .args(["build", "--message-format=json"])
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_BUILD_BUILD_DIR", &target)
        .env("CARGO_INCREMENTAL", "1")
        .env("RUSTC_WRAPPER", "")
        .env(
            "RUSTC_WORKSPACE_WRAPPER",
            wrapper.map_or(OsStr::new(""), Path::as_os_str),
        )
        .env("RUSTFLAGS", "")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTC_BOOTSTRAP")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cargo build in {} failed:\n{}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut rebuilt: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["fresh"] == false)
        .filter_map(|m| m["target"]["name"].as_str().map(str::to_owned))
        .collect();
    rebuilt.sort();
    rebuilt
}

/// The most recent incremental session directory of `krate`.
fn newest_session(incremental: &Path, krate: &str) -> PathBuf {
    let prefix = format!("{krate}-");
    fs::read_dir(incremental)
        .unwrap()
        .map(Result::unwrap)
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .flat_map(|e| fs::read_dir(e.path()).unwrap().map(Result::unwrap))
        .filter(|s| s.file_type().unwrap().is_dir())
        .max_by_key(|s| s.metadata().unwrap().modified().unwrap())
        .unwrap_or_else(|| panic!("no incremental session for {krate}"))
        .path()
}

/// Every object file under `dir`.
fn object_files(dir: &Path) -> Vec<PathBuf> {
    let mut objects = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().map(Result::unwrap) {
            if entry.metadata().unwrap().is_dir() {
                stack.push(entry.path());
            } else if entry.path().extension() == Some(OsStr::new("o")) {
                objects.push(entry.path());
            }
        }
    }
    objects
}

fn run(command: &mut Command) {
    let status = command.status().unwrap();
    assert!(status.success(), "{command:?} failed with {status}");
}

/// The wrapper needs `RUSTC_BOOTSTRAP` for its `-Z` flag; that must not let
/// a crate use `#![feature]` on a toolchain that would refuse it.
#[test]
fn wrapper_keeps_feature_gates_on_stable() {
    if !remap_supported() {
        return;
    }
    let rustc = Command::new("rustc").arg("-vV").output().unwrap();
    if Toolchain::parse(&String::from_utf8_lossy(&rustc.stdout), "").is_nightly() {
        common::skip("it needs a stable or beta rustc");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let wrapper = wrapper_in(&root);
    let krate = root.join("gated");
    fs::create_dir_all(krate.join("src")).unwrap();
    fs::write(
        krate.join("Cargo.toml"),
        "[package]\nname = \"gated\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        krate.join("src/lib.rs"),
        "#![feature(never_type)]\npub fn none() -> Option<!> { None }\n",
    )
    .unwrap();
    let build = |nightly: bool| {
        let mut cargo = Command::new("cargo");
        cargo
            .args(["build", "--offline"])
            .current_dir(&krate)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env("RUSTC_WORKSPACE_WRAPPER", &wrapper)
            // The wrapper acts only on incremental compiles, which Cargo
            // turns off by default when `CI` is set.
            .env_remove("CI")
            .env_remove("CARGO_INCREMENTAL")
            .env_remove("RUSTC_BOOTSTRAP")
            .env_remove("FERRY_REMAP_NIGHTLY");
        if nightly {
            cargo.env("FERRY_REMAP_NIGHTLY", "1");
        }
        cargo.output().unwrap()
    };

    let refused = build(false);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{stderr}");
    assert!(
        stderr.contains("not in the list of allowed features"),
        "{stderr}"
    );
    let nightly = build(true);
    assert!(
        nightly.status.success(),
        "{}",
        String::from_utf8_lossy(&nightly.stderr)
    );
}

/// Debug info on Apple targets stays in the object files
/// (`split-debuginfo=unpacked`), and the binary only records where they
/// are. Remapping must not lose that record: backtraces need it from any
/// working directory.
#[cfg(target_os = "macos")]
#[test]
fn debug_info_survives_remapping() {
    if !remap_supported() || !xcrun_has(&["dsymutil", "dwarfdump"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let wrapper = wrapper_in(&root);
    for (name, wrapper) in [("with", Some(wrapper.as_path())), ("without", None)] {
        let (dir, bin) = build_panicking(&root.join(name), wrapper);

        // rust-lang/rust#132143: ld can drop an object's entry when its
        // DWARF has relative paths.
        let map = xcrun(&["dsymutil", "--dump-debug-map"], &bin, &root);
        let objects = debug_map_objects(&map);
        assert!(
            objects.iter().any(|o| o.contains(LIB)),
            "{name}: no {LIB} object in the debug map:\n{map}"
        );
        for object in &objects {
            // An archive member is listed as `lib….rlib(member.o)`.
            let file = Path::new(
                object
                    .split_once('(')
                    .map_or(*object, |(archive, _)| archive),
            );
            assert!(
                file.is_absolute() && file.exists(),
                "{name}: debug map entry {object} is not an existing absolute path:\n{map}"
            );
        }

        let package = dir.join(BIN);
        for cwd in [Path::new("/"), package.as_path()] {
            let output = Command::new(&bin)
                .current_dir(cwd)
                .env("RUST_BACKTRACE", "1")
                .env("REMAP_PANIC", "1")
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{stderr}");
            // The panic message names the file even without debug info.
            let frames = stderr
                .split_once("stack backtrace:")
                .map_or_else(|| panic!("no backtrace:\n{stderr}"), |(_, f)| f);
            assert!(
                frames.contains(&format!("{LIB}/src/lib.rs:")),
                "{name}, cwd {}: no file:line for {LIB}:\n{stderr}",
                cwd.display()
            );
        }

        // `debug/deps/remapbin-<hash>.….o`, or `remapbin.….o` (no hash) in
        // `debug/build/remapbin/<hash>/out/` in Cargo's new build-dir layout.
        // Objects under `debug/incremental` are named by codegen unit only.
        let profile = dir.join(TARGET_LINK).join("debug");
        let units: Vec<PathBuf> = object_files(&profile)
            .into_iter()
            .filter(|p| {
                p.file_name().is_some_and(|n| {
                    n.to_string_lossy()
                        .strip_prefix(BIN)
                        .is_some_and(|rest| rest.starts_with(['-', '.']))
                })
            })
            .collect();
        assert!(
            !units.is_empty(),
            "no {BIN} objects in {}",
            profile.display()
        );
        let expected = if wrapper.is_some() {
            ".".to_owned()
        } else {
            dir.display().to_string()
        };
        for unit in units {
            let dump = xcrun(&["dwarfdump", "--debug-info"], &unit, &root);
            let comp_dirs: Vec<&str> = dump
                .lines()
                .filter(|l| l.contains("DW_AT_comp_dir"))
                .filter_map(|l| l.split_once("(\"").and_then(|(_, v)| v.split_once("\")")))
                .map(|(v, _)| v)
                .collect();
            assert!(
                !comp_dirs.is_empty() && comp_dirs.iter().all(|d| *d == expected),
                "{name}: DW_AT_comp_dir of {} is {comp_dirs:?}, expected {expected:?}",
                unit.display()
            );
        }
    }
}

/// The object files listed in `dsymutil --dump-debug-map` output, a YAML
/// document with one `- filename: '<path>'` item per object.
fn debug_map_objects(map: &str) -> Vec<&str> {
    map.lines()
        .filter_map(|l| l.trim_start().strip_prefix("- filename:"))
        .map(|v| v.trim().trim_matches('\''))
        .collect()
}

#[test]
fn debug_map_objects_reads_dsymutil_output() {
    // What LLVM 21's `dsymutil --dump-debug-map` printed for an arm64 dylib
    // linked from a binary crate's object and a library's rlib, with a
    // shorter directory.
    let map = "\
---
triple:          'arm64-apple-darwin'
binary-path:     remapbin.dylib
objects:
  - filename:        '/private/var/t/with.target/debug/deps/remapbin-abc.remapbin.o'
    timestamp:       0
    type:            102
    symbols:
      - { sym: __RNvCsdtSMxSA38aw_7___rustc17rust_begin_unwind, objAddr: 0x0, binAddr: 0x508, size: 0x10 }
      - { sym: _entry, objAddr: 0x10, binAddr: 0x518, size: 0x1C }
  - filename:        '/private/var/t/with.target/debug/deps/libremaplib-abc.rlib(libremaplib-abc.remaplib.375bbbe60a926f9f-cgu.0.rcgu.o)'
    timestamp:       0
    type:            102
    symbols:
      - { sym: _remap_value, objAddr: 0x0, binAddr: 0x534, size: 0x6C }
...
";
    assert_eq!(
        debug_map_objects(map),
        [
            "/private/var/t/with.target/debug/deps/remapbin-abc.remapbin.o",
            "/private/var/t/with.target/debug/deps/libremaplib-abc.rlib(libremaplib-abc.remaplib.375bbbe60a926f9f-cgu.0.rcgu.o)",
        ]
    );
}

/// Measured, not asserted in CI (it runs with continue-on-error there):
/// whether lldb started from the worktree root finds sources recorded
/// relative to comp dir ".". Runner VMs may not allow lldb to launch
/// processes at all.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "needs lldb permission to launch processes; CI runs it as a measurement"]
fn lldb_finds_sources_from_the_worktree_root() {
    if !remap_supported() || !xcrun_has(&["lldb"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let wrapper = wrapper_in(&root);
    let (dir, bin) = build_panicking(&root.join("with"), Some(&wrapper));
    let source = fs::read_to_string(dir.join(LIB).join("src/lib.rs")).unwrap();
    let line = 1 + source.lines().position(|l| l.contains(BREAKPOINT)).unwrap();
    let mut found = Vec::new();
    for cwd in [dir.as_path(), Path::new("/")] {
        let output = Command::new("xcrun")
            .arg("lldb")
            .arg("--batch")
            .args(["-o", &format!("breakpoint set -f lib.rs -l {line}")])
            .args(["-o", "process launch"])
            .args(["-o", &format!("source list -f lib.rs -l {line} -c 1")])
            .arg("--")
            .arg(&bin)
            .current_dir(cwd)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = writeln!(
            std::io::stderr(),
            "lldb from {}: source {}found\n{text}",
            cwd.display(),
            if text.contains(BREAKPOINT) {
                ""
            } else {
                "not "
            }
        );
        found.push(text.contains(BREAKPOINT));
    }
    assert!(
        found[0],
        "lldb did not show the source from the worktree root"
    );
}

#[cfg(target_os = "macos")]
const BREAKPOINT: &str = "// remap breakpoint";

/// The remap workspace, built at `dir` (target through `.ferry-target`),
/// with a library function that panics when `REMAP_PANIC` is set. Returns
/// the workspace root and the binary.
#[cfg(target_os = "macos")]
fn build_panicking(dir: &Path, wrapper: Option<&Path>) -> (PathBuf, PathBuf) {
    let target = dir.with_extension("target");
    write_workspace(dir);
    fs::write(
        dir.join(LIB).join("src/lib.rs"),
        modules()
            + "\n#[inline(never)]\npub fn check(x: usize) -> usize {\n    \
               if std::env::var_os(\"REMAP_PANIC\").is_some() {\n        \
               panic!(\"remap panic\");\n    }\n    \
               x * 3 "
            + BREAKPOINT
            + "\n}\n",
    )
    .unwrap();
    fs::write(
        dir.join(BIN).join("src/main.rs"),
        main_rs(&format!("println!(\"{{}}\", {LIB}::check(total(1)));")),
    )
    .unwrap();
    fs::create_dir_all(&target).unwrap();
    run(Command::new("git").args(["init", "-q"]).arg(dir));
    ferry::remap::prepare(dir, &target).unwrap();
    assert_eq!(cargo_build(dir, wrapper), [BIN, LIB]);
    (dir.to_path_buf(), target.join("debug").join(BIN))
}

/// Whether every tool is available through `xcrun`; skips (or fails, when
/// remap is required) otherwise.
#[cfg(target_os = "macos")]
fn xcrun_has(tools: &[&str]) -> bool {
    for tool in tools {
        let found = Command::new("xcrun")
            .args(["--find", tool])
            .output()
            .is_ok_and(|o| o.status.success());
        if !found {
            assert!(
                !common::required("remap"),
                "remap required by FERRY_TEST_REQUIRE but xcrun cannot find {tool}"
            );
            common::skip(&format!("xcrun cannot find {tool}"));
            return false;
        }
    }
    true
}

#[cfg(target_os = "macos")]
fn xcrun(args: &[&str], file: &Path, cwd: &Path) -> String {
    let output = Command::new("xcrun")
        .args(args)
        .arg(file)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(output.status.success(), "xcrun {args:?} failed: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}
