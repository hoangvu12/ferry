//! Shared harness for the integration tests: a throwaway git repository made
//! from `tests/fixtures/workspace`, a private `FERRY_HOME`, and helpers that
//! run ferry and cargo and report what Cargo rebuilt.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]
// clippy's allow-*-in-tests settings cover only #[test] functions; these
// helpers are test code too, and failing loudly is their job.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::io::Write as _;

use ferry::sys::{self, OsStrExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use serde_json::Value;

pub const BASE_MARKER: &str = "LEAF_BASE";
const BASE_ASSETS: &str = "a.txt=alpha;sub/b.txt=beta;sub/c.txt=gamma";
pub const ALL_PACKAGES: &[&str] = &["app", "embed", "leaf", "mid", "side"];

/// The binary under test: `FERRY_TEST_BIN` when set (CI points it at a copy
/// installed under an `AppArmor` profile), otherwise the one Cargo built.
fn ferry_bin() -> PathBuf {
    std::env::var_os("FERRY_TEST_BIN")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ferry")), PathBuf::from)
}

/// Removed from every command the harness runs: they would redirect or
/// reconfigure the builds under test (CI toolchain actions export
/// `CARGO_INCREMENTAL=0`, and Cargo 1.99 turns incremental off by default
/// when `CI` or `TF_BUILD` is set, for example) or colour the output tests
/// match on. `GIT_*` and `FERRY_*` variables are removed as well.
const SCRUBBED_VARS: &[&str] = &[
    "CI",
    "TF_BUILD",
    "CARGO_INCREMENTAL",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_BUILD_BUILD_DIR",
    "RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_WARNINGS",
    "CLIPPY_CONF_DIR",
    "CLICOLOR_FORCE",
];

/// A copy of the fixture workspace committed to a fresh git repository, with
/// its own `FERRY_HOME`. Everything lives in one temporary directory that is
/// deleted on drop.
#[derive(Debug)]
pub struct TestRepo {
    tmp: tempfile::TempDir,
    /// Canonical path of `tmp`, so paths compare equal to the ones ferry
    /// reports.
    scratch: PathBuf,
    root: PathBuf,
    home: PathBuf,
    gitconfig: PathBuf,
    env: Vec<(String, String)>,
}

impl TestRepo {
    /// [`TestRepo::with_env`] with no extra variables, so it ignores any
    /// `ferry.toml` too.
    pub fn new() -> Self {
        Self::with_env(&[])
    }

    /// A repository whose ferry and cargo commands all get `env` on top of
    /// the scrubbed environment, e.g. `FERRY_BACKEND`. Its bases warm only
    /// `build` and `test`: the default `check` and `clippy` commands double
    /// the time of every base and are covered by `tests/warm.rs`.
    ///
    /// That is done by setting `FERRY_CONFIG`, which Ferry reads instead of
    /// the repository's `ferry.toml`: a `ferry.toml` a test writes is
    /// silently ignored. Tests that need one use
    /// [`TestRepo::with_default_config`].
    pub fn with_env(env: &[(&str, &str)]) -> Self {
        Self::with_workspace(env, |root| copy_dir(&fixture_dir(), root))
    }

    /// Like [`TestRepo::with_env`], but the first commit holds what
    /// `populate` writes into the (not yet created) repository root instead
    /// of the fixture workspace. Bases warm only `build` and `test`, and a
    /// `ferry.toml` it writes is ignored, as for [`TestRepo::with_env`].
    pub fn with_workspace(env: &[(&str, &str)], populate: impl FnOnce(&Path)) -> Self {
        let mut repo = Self::create(env, populate);
        let config = repo.scratch.join("ferry-test.toml");
        fs::write(&config, "[base]\ntargets = [\"build\", \"test\"]\n").unwrap();
        repo.env.insert(
            0,
            ("FERRY_CONFIG".into(), config.to_string_lossy().into_owned()),
        );
        repo
    }

    /// [`TestRepo::with_env`] with Ferry's built-in configuration.
    pub fn with_default_config(env: &[(&str, &str)]) -> Self {
        Self::create(env, |root| copy_dir(&fixture_dir(), root))
    }

    fn create(env: &[(&str, &str)], populate: impl FnOnce(&Path)) -> Self {
        // Under the target dir rather than /tmp: it is on the same
        // filesystem as the project, which is where reflinks are likely to
        // work, and /tmp is often tmpfs.
        let tmp = tempfile::Builder::new()
            .prefix("ferry-test-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let scratch = sys::canonicalize(tmp.path()).unwrap();
        let root = scratch.join("repo");
        populate(&root);
        let gitconfig = scratch.join("gitconfig");
        fs::write(
            &gitconfig,
            "[user]\n\tname = Ferry Tests\n\temail = tests@ferry.invalid\n\
             [init]\n\tdefaultBranch = main\n",
        )
        .unwrap();
        let repo = Self {
            home: scratch.join("ferry-home"),
            tmp,
            scratch,
            root,
            gitconfig,
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        };
        repo.git(&repo.root, ["init", "-q"]);
        repo.commit_all("fixture");
        repo
    }

    /// The main checkout.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Scratch space next to the repository, on the same filesystem.
    pub fn scratch(&self) -> &Path {
        &self.scratch
    }

    /// `program` with a scrubbed environment, the test's git config and
    /// `FERRY_HOME`, running in the main checkout.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut cmd = Command::new(program);
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("GIT_")
                || name.starts_with("FERRY_")
                || SCRUBBED_VARS.contains(&name.as_ref())
            {
                cmd.env_remove(&key);
            }
        }
        // CI runs the whole suite in remap mode this way; a test's own
        // FERRY_REMAP_PATHS still wins.
        if std::env::var_os("FERRY_TEST_REMAP").is_some_and(|v| v == "1") {
            cmd.env("FERRY_REMAP_PATHS", "1");
        }
        // The ceiling keeps git from finding the repository that contains
        // the target dir when a test runs outside its own repository.
        cmd.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &self.gitconfig)
            .env("GIT_CEILING_DIRECTORIES", &self.scratch)
            .env("FERRY_HOME", &self.home)
            .env("CARGO_NET_OFFLINE", "true")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.root)
            .stdin(Stdio::null());
        cmd
    }

    pub fn ferry_cmd<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.command(ferry_bin());
        cmd.args(args);
        cmd
    }

    pub fn ferry<I, S>(&self, args: I) -> Run
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run(&mut self.ferry_cmd(args))
    }

    /// Runs `ferry --json <args>`, requires success and parses stdout.
    #[track_caller]
    pub fn ferry_json<I, S>(&self, args: I) -> Value
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.ferry_cmd(["--json"]);
        cmd.args(args);
        run(&mut cmd).ok().json()
    }

    /// `ferry base`; returns the base id.
    #[track_caller]
    pub fn base(&self) -> String {
        str_of(&self.ferry_json(["base"])["base"]["id"]).to_owned()
    }

    /// `ferry spawn <name>`; returns the agent's worktree.
    #[track_caller]
    pub fn spawn(&self, name: &str) -> PathBuf {
        path_of(&self.ferry_json(["spawn", name])["agent"]["worktree"])
    }

    /// `ferry run <agent> -- cargo`, ready for cargo's arguments.
    pub fn agent_cargo(&self, agent: &str) -> Command {
        self.ferry_cmd(["run", agent, "--", "cargo"])
    }

    /// Plain cargo in `dir`, without ferry.
    pub fn cargo_in(&self, dir: &Path) -> Command {
        let mut cmd = self.command("cargo");
        cmd.current_dir(dir);
        cmd
    }

    /// Plain cargo in `dir` with the directory printed by `ferry shim` first
    /// on `PATH`.
    #[track_caller]
    pub fn shimmed_cargo(&self, dir: &Path) -> Command {
        let shim = PathBuf::from(self.ferry(["shim"]).ok().stdout.trim());
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(std::iter::once(shim).chain(std::env::split_paths(&path)))
            .unwrap();
        let mut cmd = self.cargo_in(dir);
        cmd.env("PATH", path);
        cmd
    }

    /// `cargo build` in an agent's build context.
    #[track_caller]
    pub fn build(&self, agent: &str) -> Build {
        Build::run(self.agent_cargo(agent).arg("build"))
    }

    /// Output of the fixture's `app` binary, built and run in an agent.
    #[track_caller]
    pub fn app(&self, agent: &str) -> App {
        App::run(&mut self.agent_cargo(agent))
    }

    /// Writes `contents` to `path` with an mtime strictly newer than any
    /// file written before the call (see [`tick`]).
    pub fn write(&self, path: &Path, contents: &str) {
        tick(&self.scratch);
        fs::write(path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }

    pub fn set_marker(&self, worktree: &Path, marker: &str) {
        self.write(&worktree.join("leaf/src/lib.rs"), &leaf_source(marker));
    }

    /// Replaces `embed/assets/<name>`.
    pub fn set_asset(&self, worktree: &Path, name: &str, contents: &str) {
        self.write(
            &worktree.join("embed/assets").join(name),
            &format!("{contents}\n"),
        );
    }

    /// Runs git in `dir`, requires success and returns trimmed stdout.
    #[track_caller]
    pub fn git<I, S>(&self, dir: &Path, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.command("git");
        cmd.arg("-C").arg(dir).args(args);
        run(&mut cmd).ok().stdout.trim().to_owned()
    }

    /// Commits everything in the main checkout; returns the new commit id.
    #[track_caller]
    pub fn commit_all(&self, message: &str) -> String {
        self.git(&self.root, ["add", "-A"]);
        self.git(&self.root, ["commit", "-q", "-m", message]);
        self.git(&self.root, ["rev-parse", "HEAD"])
    }

    /// The repository's store under `FERRY_HOME`.
    #[track_caller]
    pub fn store(&self) -> PathBuf {
        let stores: Vec<PathBuf> = fs::read_dir(&self.home)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.join("store.json").is_file())
            .collect();
        assert_eq!(stores.len(), 1, "stores in {}", self.home.display());
        stores.into_iter().next().unwrap()
    }

    pub fn base_dir(&self, id: &str) -> PathBuf {
        self.store().join("bases").join(id)
    }

    /// The target dir of an agent forked by the reflink or copy backend.
    pub fn agent_target(&self, name: &str) -> PathBuf {
        self.store().join("agents").join(name).join("target")
    }

    /// What `ferry doctor` detected on this machine for this store.
    #[track_caller]
    pub fn capabilities(&self) -> Capabilities {
        // Exit status is ignored: doctor fails when any check fails, but
        // the capabilities are still reported.
        let report = run(&mut self.ferry_cmd(["--json", "doctor"])).json();
        let caps = &report["capabilities"];
        assert!(
            caps.is_object(),
            "doctor reported no capabilities: {report}"
        );
        Capabilities {
            userns: probe(&caps["userns"]),
            overlay: probe(&caps["overlay"]),
            reflink: probe(&caps["reflink"]),
        }
    }

    /// False, after printing why the calling test is skipped, when
    /// unprivileged user + mount namespaces are unavailable.
    pub fn require_userns(&self) -> bool {
        require("user namespaces", "userns", self.capabilities().userns)
    }

    pub fn require_overlay(&self) -> bool {
        require(
            "overlayfs in a user namespace",
            "overlay",
            self.capabilities().overlay,
        )
    }

    pub fn require_reflink(&self) -> bool {
        require(
            "reflinks in the store",
            "reflink",
            self.capabilities().reflink,
        )
    }

    /// False, after printing why, when `cargo clippy` does not work here.
    pub fn require_clippy(&self) -> bool {
        let version = run(self.cargo_in(&self.root).args(["clippy", "--version"]));
        let probe = if version.status.success() {
            Ok(())
        } else {
            Err(version.stderr.trim().to_owned())
        };
        require("clippy", "clippy", probe)
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        // overlayfs leaves a mode-000 work dir that TempDir cannot delete.
        let _ = ferry::fsutil::remove_dir_all_force(self.tmp.path());
    }
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace")
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            if entry.file_name() != "target" {
                copy_dir(&entry.path(), &dest);
            }
        } else {
            fs::copy(entry.path(), &dest).unwrap();
        }
    }
}

/// Source of the fixture's `leaf/src/lib.rs` with `marker` as its marker.
pub fn leaf_source(marker: &str) -> String {
    format!("pub fn marker() -> &'static str {{\n    \"{marker}\"\n}}\n")
}

/// Returns once a file created in `dir` gets a strictly newer mtime than one
/// created before the call. Cargo only treats a source as changed if its
/// mtime is strictly newer than the build that last compiled it, and file
/// timestamps come from a coarse clock (or whole seconds on some
/// filesystems), so an edit right after a build could otherwise go unseen.
pub fn tick(dir: &Path) {
    let created = || {
        tempfile::tempfile_in(dir)
            .and_then(|f| f.metadata())
            .and_then(|m| m.modified())
            .unwrap()
    };
    let start = created();
    while created() <= start {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// True if `path` has ferry's fixed stamp mtime.
pub fn is_stamped(path: &Path) -> bool {
    let meta =
        fs::symlink_metadata(path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
    let (secs, nsec) = sys::mtime(&meta);
    u64::try_from(secs) == Ok(ferry::freshness::STAMP_SECS) && nsec == 0
}

#[track_caller]
pub fn str_of(value: &Value) -> &str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("expected a string, got {value}"))
}

#[track_caller]
pub fn path_of(value: &Value) -> PathBuf {
    PathBuf::from(str_of(value))
}

/// A finished command.
#[derive(Debug)]
pub struct Run {
    pub command: String,
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[track_caller]
pub fn run(cmd: &mut Command) -> Run {
    let command = std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("cannot run `{command}`: {e}"));
    Run {
        command,
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

impl Run {
    #[track_caller]
    pub fn ok(self) -> Self {
        assert!(self.status.success(), "{self}");
        self
    }

    #[track_caller]
    pub fn failed(self) -> Self {
        assert!(!self.status.success(), "expected a failure: {self}");
        self
    }

    #[track_caller]
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {self}"))
    }
}

impl fmt::Display for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` exited with {}\n--- stdout\n{}\n--- stderr\n{}",
            self.command, self.status, self.stdout, self.stderr
        )
    }
}

/// A compile unit from Cargo's JSON messages.
#[derive(Debug)]
pub struct Unit {
    pub package: String,
    pub target: String,
}

/// What one cargo invocation compiled, from `--message-format=json`.
#[derive(Debug)]
pub struct Build {
    /// Units reported with `"fresh": false`.
    pub rebuilt: Vec<Unit>,
    pub fresh: usize,
    /// Cargo's `-v` output, which names the reason for every dirty unit.
    pub log: String,
    /// Target name and path of every executable Cargo reported, fresh or not.
    pub executables: Vec<(String, PathBuf)>,
}

impl Build {
    /// Runs `cargo` (a command whose subcommand is already set, such as
    /// `build` or `test --no-run`) with JSON messages and requires success.
    #[track_caller]
    pub fn run(cargo: &mut Command) -> Self {
        Self::run_clippy(cargo, &[])
    }

    /// [`Build::run`] for `cargo clippy` with `lint_args` after `--`, where
    /// cargo's own flags must not go.
    #[track_caller]
    pub fn run_clippy(cargo: &mut Command, lint_args: &[&str]) -> Self {
        cargo.args(["--message-format=json", "-v"]);
        if !lint_args.is_empty() {
            cargo.arg("--").args(lint_args);
        }
        let output = run(cargo).ok();
        let mut rebuilt = Vec::new();
        let mut fresh = 0;
        let mut executables = Vec::new();
        for line in output.stdout.lines().filter(|l| l.starts_with('{')) {
            let message: Value = serde_json::from_str(line).unwrap();
            if message["reason"] != "compiler-artifact" {
                continue;
            }
            if let Some(exe) = message["executable"].as_str() {
                executables.push((
                    str_of(&message["target"]["name"]).to_owned(),
                    PathBuf::from(exe),
                ));
            }
            if message["fresh"].as_bool() == Some(true) {
                fresh += 1;
            } else {
                rebuilt.push(Unit {
                    package: package_name(str_of(&message["package_id"])).to_owned(),
                    target: str_of(&message["target"]["name"]).to_owned(),
                });
            }
        }
        Self {
            rebuilt,
            fresh,
            log: output.stderr,
            executables,
        }
    }

    /// The executable Cargo reported for target `name`.
    #[track_caller]
    pub fn executable(&self, name: &str) -> &Path {
        match self.executables.iter().find(|(target, _)| target == name) {
            Some((_, path)) => path,
            None => panic!("no executable for `{name}`: {:?}", self.executables),
        }
    }

    fn rebuilt_packages(&self) -> BTreeSet<&str> {
        self.rebuilt.iter().map(|u| u.package.as_str()).collect()
    }

    /// Requires that exactly the units of `packages` were rebuilt.
    #[track_caller]
    pub fn assert_rebuilt(&self, packages: &[&str]) {
        let expected: BTreeSet<&str> = packages.iter().copied().collect();
        let log: Vec<&str> = self
            .log
            .lines()
            .filter(|l| !l.trim_start().starts_with("Running `"))
            .collect();
        assert_eq!(
            self.rebuilt_packages(),
            expected,
            "rebuilt units: {:?} ({} fresh)\ncargo log:\n{}",
            self.rebuilt,
            self.fresh,
            log.join("\n")
        );
    }
}

/// Package name from a package id spec: `path+file:///ws/leaf#0.1.0`, or
/// `path+file:///ws/dir#leaf@0.1.0` when the name differs from the
/// directory.
pub fn package_name(id: &str) -> &str {
    let (url, fragment) = id.rsplit_once('#').unwrap_or((id, ""));
    match fragment.split_once('@') {
        Some((name, _)) => name,
        None => url.rsplit('/').next().unwrap_or(url),
    }
}

/// What the fixture's `app` binary printed.
#[derive(Debug, PartialEq, Eq)]
pub struct App {
    pub marker: String,
    pub assets: String,
}

impl App {
    pub fn new(marker: &str, assets: &str) -> Self {
        Self {
            marker: marker.to_owned(),
            assets: assets.to_owned(),
        }
    }

    pub fn base() -> Self {
        Self::new(BASE_MARKER, BASE_ASSETS)
    }

    /// Runs `cargo run -q -p app` with `cargo` and parses its output.
    #[track_caller]
    pub fn run(cargo: &mut Command) -> Self {
        let output = run(cargo.args(["run", "-q", "-p", "app"])).ok();
        let field = |key: &str| {
            output
                .stdout
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .unwrap_or_else(|| panic!("no `{key}` line: {output}"))
                .to_owned()
        };
        Self {
            marker: field("marker="),
            assets: field("assets="),
        }
    }
}

/// Every path under a directory with its type, permissions, inode, link
/// count, mtime and a content hash.
#[derive(Debug)]
pub struct Snapshot(BTreeMap<PathBuf, Node>);

#[derive(Debug, PartialEq, Eq)]
struct Node {
    kind: &'static str,
    mode: u32,
    id: sys::FileId,
    nlink: u64,
    mtime: (i64, i64),
    len: u64,
    content: u64,
}

/// Permission bits: the mode on Unix, the attributes on Windows.
fn mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        std::os::unix::fs::MetadataExt::mode(meta)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::MetadataExt::file_attributes(meta)
    }
}

impl Snapshot {
    #[track_caller]
    pub fn of(root: &Path) -> Self {
        let mut nodes = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            let meta = fs::symlink_metadata(&path)
                .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
            let (kind, content) = if meta.is_dir() {
                for entry in fs::read_dir(&path).unwrap() {
                    stack.push(entry.unwrap().path());
                }
                ("dir", 0)
            } else if meta.is_symlink() {
                (
                    "symlink",
                    hash(fs::read_link(&path).unwrap().as_os_str().as_bytes()),
                )
            } else {
                ("file", hash(&fs::read(&path).unwrap()))
            };
            let stat = sys::stat(&path, &meta).unwrap();
            let node = Node {
                kind,
                mode: mode(&meta),
                id: stat.id,
                nlink: stat.nlink,
                mtime: stat.mtime,
                len: meta.len(),
                content,
            };
            nodes.insert(path.strip_prefix(root).unwrap().to_path_buf(), node);
        }
        Self(nodes)
    }

    /// Requires `after` to be identical, listing every difference.
    #[track_caller]
    pub fn assert_unchanged(&self, after: &Self) {
        let mut diffs = Vec::new();
        for (path, node) in &self.0 {
            match after.0.get(path) {
                None => diffs.push(format!("removed {}", path.display())),
                Some(new) if new != node => {
                    diffs.push(format!("changed {}: {node:?} -> {new:?}", path.display()));
                }
                Some(_) => {}
            }
        }
        for path in after.0.keys().filter(|p| !self.0.contains_key(*p)) {
            diffs.push(format!("added {}", path.display()));
        }
        assert!(
            diffs.is_empty(),
            "{} change(s):\n{}",
            diffs.len(),
            diffs.join("\n")
        );
    }
}

fn hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(bytes);
    hasher.finish()
}

/// Outcome of one of ferry's capability probes: `Err` holds the reason it is
/// unavailable.
pub type Probe = Result<(), String>;

#[derive(Debug)]
pub struct Capabilities {
    pub userns: Probe,
    pub overlay: Probe,
    pub reflink: Probe,
}

#[track_caller]
fn probe(value: &Value) -> Probe {
    match value["status"].as_str() {
        Some("ok") => Ok(()),
        Some("unsupported") => Err(str_of(&value["reason"]).to_owned()),
        Some("failed") => Err(format!(
            "{} failed: {}",
            str_of(&value["stage"]),
            str_of(&value["error"])
        )),
        _ => panic!("unexpected probe {value}"),
    }
}

/// Whether `FERRY_TEST_REQUIRE` lists `key`. It lists capabilities
/// (`userns`, `overlay`, `reflink`, `clippy`, `remap`) that must be present:
/// CI sets it where they are, so a regression in detection fails the run
/// instead of silently skipping tests.
pub fn required(key: &str) -> bool {
    std::env::var("FERRY_TEST_REQUIRE").is_ok_and(|v| v.split(',').any(|k| k.trim() == key))
}

fn require(what: &str, key: &str, probe: Probe) -> bool {
    match probe {
        Ok(()) => true,
        Err(reason) => {
            assert!(
                !required(key),
                "{what} required by FERRY_TEST_REQUIRE but unavailable: {reason}"
            );
            skip(&format!("{what} unavailable ({reason})"));
            false
        }
    }
}

/// Reports that the calling test is skipped.
pub fn skip(reason: &str) {
    let thread = std::thread::current();
    let test = thread.name().unwrap_or("test");
    // Written to the stream directly: libtest captures `eprintln!` from
    // passing tests, and a skip must stay visible in CI logs.
    let _ = writeln!(std::io::stderr(), "skipping {test}: {reason}");
}

/// Retries `check` for up to five seconds.
pub fn eventually(check: impl FnMut() -> bool) -> bool {
    eventually_for(Duration::from_secs(5), check)
}

/// Retries `check` for up to `limit`.
pub fn eventually_for(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while !check() {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}
