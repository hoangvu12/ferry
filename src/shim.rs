//! The `cargo` shim.
//!
//! `<ferry home>/bin/cargo` is a symlink to the ferry binary (on Windows,
//! `cargo.exe` is a hard link or copy of `ferry.exe`). When ferry
//! runs under the name `cargo` it resolves the agent from the working
//! directory on every call and runs the real cargo inside that agent's
//! sandbox, with that agent's target dir. Outside any agent worktree it runs
//! the real cargo untouched. Putting the shim first on `PATH` is therefore
//! safe for every process an agent tool starts, including sub-agents working
//! in other worktrees, which is not true of exporting `CARGO_TARGET_DIR`.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead as _, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use crate::context::Context;
use crate::error::{Error, IoContext, Result};
use crate::sys::{self, OsStrExt, OsStringExt};

pub const NAME: &str = "cargo";

/// Counts shim hops so a misconfigured `PATH` fails instead of looping.
pub(crate) const DEPTH_ENV: &str = "FERRY_SHIM_DEPTH";

/// The hop count to pass to the cargo this shim execs.
pub(crate) fn next_depth() -> String {
    let depth: u32 = std::env::var(DEPTH_ENV)
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(0);
    (depth + 1).to_string()
}

/// True if the process was started through the shim link.
pub fn invoked_as_cargo() -> bool {
    invoked_as(NAME)
}

/// True if this process runs under the program name `name`: `argv[0]` on
/// Unix (the symlink's name); the executable's own file name on Windows,
/// where the shim is a hard link and `argv[0]` is whatever the caller typed.
pub(crate) fn invoked_as(name: &str) -> bool {
    #[cfg(unix)]
    let program = std::env::args_os().next().map(PathBuf::from);
    #[cfg(windows)]
    let program = std::env::current_exe().ok();
    program
        .as_deref()
        .and_then(Path::file_name)
        .is_some_and(|n| sys::is_program(n, name))
}

/// The directory holding the shim, creating it and (re)pointing the `cargo`
/// symlink at the running ferry binary.
pub fn ensure() -> Result<PathBuf> {
    ensure_link(NAME)
}

/// `$FERRY_HOME/bin`, or `bin` in the platform cache dir. Deliberately not
/// taken from a repository's `ferry.toml`: this directory goes first on
/// agents' `PATH`, so repository content must not choose it.
pub fn bin_dir() -> Result<PathBuf> {
    crate::fsutil::private_dir(&home()?.join("bin"))
}

/// `$FERRY_HOME`, or the platform cache dir, created private to the user.
pub fn home() -> Result<PathBuf> {
    let home = std::env::var_os("FERRY_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
        .or_else(crate::store::default_store_parent)
        .ok_or(Error::NoStoreDir)?;
    crate::fsutil::private_dir(&home)
}

/// [`bin_dir`], creating it and (re)pointing the `name` symlink at the
/// running ferry binary.
#[cfg(unix)]
pub(crate) fn ensure_link(name: &str) -> Result<PathBuf> {
    let dir = bin_dir()?;
    let exe = std::env::current_exe().at("locate", "the ferry binary")?;
    let exe = sys::canonicalize(&exe).unwrap_or(exe);
    let link = dir.join(name);
    if std::fs::symlink_metadata(&link).is_ok_and(|m| !m.is_symlink()) {
        return Err(Error::Refused(format!(
            "{} exists and is not Ferry's symlink; remove it to install the shim",
            link.display()
        )));
    }
    symlink_atomic(&exe, &link)?;
    Ok(dir)
}

/// Marks a directory as a Ferry `bin` dir, so [`real_cargo`] never picks a
/// shim from it. On Unix the shim is recognised as a symlink to `ferry`; a
/// Windows hard link or copy is named `cargo.exe` like the real one.
#[cfg(windows)]
const BIN_MARKER: &str = ".ferry-bin";

/// [`bin_dir`], with `name.exe` a hard link to (or, across volumes, a copy
/// of) the running ferry binary. Windows symlinks need Developer Mode or
/// admin rights, and a hard link runs as ferry under its own name.
///
/// A running executable can be renamed but not replaced or deleted, so an
/// outdated link is renamed aside first, then the new one renamed into
/// place; leftovers are deleted once no process runs them.
#[cfg(windows)]
pub(crate) fn ensure_link(name: &str) -> Result<PathBuf> {
    let dir = bin_dir()?;
    let marker = dir.join(BIN_MARKER);
    if !marker.exists() {
        std::fs::write(&marker, b"").at("create", &marker)?;
    }
    let exe = std::env::current_exe().at("locate", "the ferry binary")?;
    let link = dir.join(sys::exe_name(name));
    for entry in std::fs::read_dir(&dir)
        .at("read directory", &dir)?
        .flatten()
    {
        if entry.file_name().to_string_lossy().contains(".old-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    if is_current(&exe, &link) {
        return Ok(dir);
    }
    let tmp = dir.join(format!(".{name}.tmp-{}.exe", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    if std::fs::hard_link(&exe, &tmp).is_err() {
        std::fs::copy(&exe, &tmp).at("copy the ferry binary to", &tmp)?;
    }
    if link.exists() {
        let aside = dir.join(format!(".{name}.old-{}.exe", std::process::id()));
        std::fs::rename(&link, &aside).at("rename aside", &link)?;
    }
    std::fs::rename(&tmp, &link).at("rename into place", &link)?;
    Ok(dir)
}

/// Whether `link` already runs `exe`: the same file, or a copy of the same
/// length and modification time.
#[cfg(windows)]
fn is_current(exe: &Path, link: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (sys::file_id(exe), sys::file_id(link))
        && a == b
    {
        return true;
    }
    match (std::fs::metadata(exe), std::fs::metadata(link)) {
        (Ok(a), Ok(b)) => a.len() == b.len() && sys::mtime(&a) == sys::mtime(&b),
        _ => false,
    }
}

/// Points the symlink `link` at `target`, replacing an existing one
/// atomically so a concurrent reader never finds it missing.
#[cfg(unix)]
pub(crate) fn symlink_atomic(target: &Path, link: &Path) -> Result<()> {
    if std::fs::read_link(link).ok().as_deref() == Some(target) {
        return Ok(());
    }
    let name = link.file_name().unwrap_or_default().to_string_lossy();
    let tmp = link.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).at("create symlink", &tmp)?;
    std::fs::rename(&tmp, link).at("rename into place", link)
}

/// `PATH` with `shim_dir` first and any other occurrence removed.
pub fn path_with(shim_dir: &Path) -> OsString {
    let mut dirs = vec![shim_dir.to_path_buf()];
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path).filter(|d| !same_dir(d, shim_dir)));
    }
    std::env::join_paths(dirs).unwrap_or_else(|_| shim_dir.as_os_str().to_owned())
}

/// The first `cargo` on `PATH` that is not a ferry binary: neither this one
/// nor another ferry's shim (two shims picking each other would loop).
#[cfg(windows)]
pub fn real_cargo() -> Option<PathBuf> {
    let me = std::env::current_exe()
        .ok()
        .and_then(|e| sys::file_id(&e).ok());
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| !d.join(BIN_MARKER).exists())
        .map(|d| d.join(sys::exe_name(NAME)))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|m| m.is_file())
                && sys::file_id(candidate).ok() != me
        })
}

#[cfg(unix)]
pub fn real_cargo() -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let me = std::env::current_exe()
        .ok()
        .and_then(|e| std::fs::metadata(e).ok());
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(NAME))
        .find(|candidate| {
            let is_ferry = sys::canonicalize(candidate)
                .is_ok_and(|real| real.file_name().is_some_and(|n| n == "ferry"));
            match std::fs::metadata(candidate) {
                Ok(meta) => {
                    meta.is_file()
                        && meta.mode() & 0o111 != 0
                        && !is_ferry
                        && !me
                            .as_ref()
                            .is_some_and(|m| m.dev() == meta.dev() && m.ino() == meta.ino())
                }
                Err(_) => false,
            }
        })
}

/// Entry point when invoked as `cargo`.
pub fn main() -> ExitCode {
    let depth: u32 = std::env::var(DEPTH_ENV)
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(0);
    if depth >= 4 {
        warn("cargo resolves back to the shim; check PATH for several Ferry installations");
        return ExitCode::FAILURE;
    }
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(cargo) = real_cargo() else {
        warn("no real `cargo` found on PATH after the shim");
        return ExitCode::from(127);
    };
    let err = match run(&cargo, &args) {
        Ok(never) => match never {},
        Err(e) => e,
    };
    warn(&err.to_string());
    if let Some(hint) = err.hint() {
        let _ = writeln!(std::io::stderr(), "  hint: {hint}");
    }
    ExitCode::FAILURE
}

fn warn(message: &str) {
    let _ = writeln!(std::io::stderr(), "ferry (cargo shim): {message}");
}

fn run(cargo: &Path, args: &[OsString]) -> Result<std::convert::Infallible> {
    let cwd = std::env::current_dir().at("read", "current directory")?;
    let agent = match Context::discover_existing(&cwd) {
        Ok(Some(ctx)) => {
            crate::agent::find_by_root(&ctx, &ctx.repo.toplevel)?.map(|meta| (ctx, meta))
        }
        // No store yet, or not a repository Ferry can manage: plain cargo is
        // exactly right.
        Ok(None) | Err(Error::Git(_)) => None,
        Err(e) => {
            // Plain cargo is still safe here: it builds into the checkout's
            // own target dir, never into another agent's.
            warn(&format!("{e}; running cargo without Ferry"));
            None
        }
    };
    if let Some((ctx, meta)) = agent {
        return crate::agent::exec_cargo(&ctx, &meta, cargo, args, &cwd);
    }
    let mut command = Command::new(cargo);
    command.env(DEPTH_ENV, next_depth());
    if let (Some(src), Some(warm_target)) = (
        std::env::var_os(WARM_SRC_ENV),
        std::env::var_os(WARM_TARGET_ENV),
    ) {
        let src = PathBuf::from(src);
        let warm_target = PathBuf::from(warm_target);
        let target = workspace(cargo, args, &cwd, &[&src]).target(&warm_target);
        set_target(&mut command, target.clone());
        if target.is_some()
            && let Some(cargo_version) = std::env::var_os(WARM_CARGO_ENV)
        {
            keep_incremental(&mut command, &cargo_version.to_string_lossy(), args, &cwd);
        }
        lower_priority(&mut command);
        if let (Some(records), Some(target)) = (std::env::var_os(WARM_UNITS_ENV), target) {
            let root = target
                .strip_prefix(&warm_target)
                .map(Path::to_path_buf)
                .unwrap_or_default();
            return observe(command, cargo, args, Path::new(&records), &target, &root);
        }
    }
    command.args(args);
    Err(exec_error(cargo, sys::exec(&mut command)))
}

/// Opts out of [`lower_priority`] when set to `normal`.
pub const PRIORITY_ENV: &str = "FERRY_BUILD_PRIORITY";

/// On Windows, starts `command` (an agent's or a base's cargo) at below-normal
/// priority, which every process it starts inherits: parallel agents' builds
/// then leave the desktop responsive, at no measurable cost to the builds.
/// `FERRY_BUILD_PRIORITY=normal` turns it off.
pub(crate) fn lower_priority(command: &mut Command) {
    #[cfg(windows)]
    if std::env::var_os(PRIORITY_ENV).is_none_or(|p| p != "normal") {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::BELOW_NORMAL_PRIORITY_CLASS);
    }
    #[cfg(unix)]
    let _ = command;
}

/// Variables whose presence makes Cargo 1.99 and later (cargo#17220) turn
/// incremental compilation off by default.
pub const CI_VARS: [&str; 2] = ["CI", "TF_BUILD"];

/// Sets `CARGO_INCREMENTAL=1` for `command`, cargo called with `args` in
/// `cwd`, when [`needs_incremental`] says so for the toolchain whose
/// `cargo -V` prints `cargo_version`.
pub(crate) fn keep_incremental(
    command: &mut Command,
    cargo_version: &str,
    args: &[OsString],
    cwd: &Path,
) {
    let env = |name: &str| std::env::var_os(name);
    let configs = || cargo_configs(args, cwd, crate::base::cargo_home().as_deref());
    if needs_incremental(cargo_version, env, configs) {
        command.env("CARGO_INCREMENTAL", "1");
    }
}

/// Whether Cargo `cargo_version` turns incremental compilation off only
/// because `env` has `CI` or `TF_BUILD`: it is 1.99 or newer, and neither
/// `CARGO_INCREMENTAL`, `CARGO_BUILD_INCREMENTAL` nor one of `configs` sets
/// `build.incremental`.
///
/// Cargo applies that switch after merging profiles, so it beats
/// `[profile.*] incremental = true`; only the two settings above rank
/// higher. The profile's `incremental` is part of every unit's hash, so an
/// agent with `CI` set would otherwise rebuild every workspace member of a
/// base warmed without it. `CARGO_INCREMENTAL=1` restores the default for
/// dev and test builds, at a cost under `CI`: release and bench builds
/// become incremental too, and `[profile.*] incremental = false` is
/// overridden.
fn needs_incremental(
    cargo_version: &str,
    env: impl Fn(&str) -> Option<OsString>,
    configs: impl FnOnce() -> Vec<toml::Table>,
) -> bool {
    // Cargo tests `std::env::var(name).is_ok()`, false for a non-UTF-8 value.
    CI_VARS
        .iter()
        .any(|name| env(name).is_some_and(|value| value.to_str().is_some()))
        && env("CARGO_INCREMENTAL").is_none()
        && env("CARGO_BUILD_INCREMENTAL").is_none()
        && cargo_release(cargo_version).is_some_and(|release| release >= (1, 99))
        && !configs().iter().any(|config| {
            config
                .get("build")
                .and_then(|build| build.get("incremental"))
                .is_some()
        })
}

/// Major and minor version in `cargo -V` output such as
/// `cargo 1.101.0-nightly (2d3e3e5f8 2026-10-02)`.
fn cargo_release(cargo_version: &str) -> Option<(u32, u32)> {
    let mut parts = cargo_version
        .strip_prefix("cargo ")?
        .split('.')
        .map(str::parse);
    Some((parts.next()?.ok()?, parts.next()?.ok()?))
}

/// The Cargo configs a call with `args` in `cwd` reads, parsed: `--config`
/// values, then `.cargo/config.toml` and `.cargo/config` in `cwd` and each
/// ancestor, then in `cargo_home`. `include`s are not followed.
fn cargo_configs(args: &[OsString], cwd: &Path, cargo_home: Option<&Path>) -> Vec<toml::Table> {
    let mut values = Vec::new();
    let mut options = args.iter().take_while(|a| *a != "--");
    while let Some(arg) = options.next() {
        if arg == "--config" {
            values.extend(options.next().map(OsString::as_os_str));
        } else if let Some(value) = arg.as_bytes().strip_prefix(b"--config=") {
            values.push(OsStr::from_bytes(value));
        }
    }
    // A value is a `KEY=VALUE` pair in TOML or the path of a config file.
    let given = values.into_iter().filter_map(|value| {
        let inline = value.to_str().and_then(|v| v.parse().ok());
        inline.or_else(|| read_config(&cwd.join(value)))
    });
    let files = cwd
        .ancestors()
        .map(|dir| dir.join(".cargo"))
        .chain(cargo_home.map(Path::to_path_buf))
        .flat_map(|dir| [dir.join("config.toml"), dir.join("config")]);
    given
        .chain(files.filter_map(|path| read_config(&path)))
        .collect()
}

fn read_config(path: &Path) -> Option<toml::Table> {
    std::fs::read_to_string(path).ok()?.parse().ok()
}

/// What a warm cargo call needs for Ferry to see which units it uses.
#[derive(Debug, PartialEq, Eq)]
enum Observe {
    /// It builds units: insert `--message-format=json-render-diagnostics`
    /// at this index.
    Inject(usize),
    /// It builds units and already asks for JSON messages.
    AsIs,
    /// It may build units, and Ferry cannot see which.
    Unobservable(&'static str),
    /// It builds no units.
    Nothing,
}

fn classify(args: &[OsString]) -> Observe {
    const BUILDS: &[&str] = &[
        "build", "b", "check", "c", "test", "t", "bench", "run", "r", "rustc", "rustdoc", "doc",
        "d", "fix", "clippy",
    ];
    const BUILDS_NOTHING: &[&str] = &[
        "metadata",
        "locate-project",
        "read-manifest",
        "verify-project",
        "pkgid",
        "tree",
        "fetch",
        "generate-lockfile",
        "update",
        "vendor",
        "search",
        "info",
        "owner",
        "yank",
        "login",
        "logout",
        "new",
        "init",
        "add",
        "remove",
        "fmt",
        "version",
        "help",
        "config",
        "report",
        "clean",
        "uninstall",
    ];
    let (globals, rest) = split_globals(args);
    let Some(subcommand) = rest.first() else {
        return Observe::Nothing;
    };
    let subcommand = subcommand.to_str().unwrap_or_default();
    if BUILDS_NOTHING.contains(&subcommand) {
        return Observe::Nothing;
    }
    if !BUILDS.contains(&subcommand) {
        return Observe::Unobservable("is not a build Ferry can observe");
    }
    let mut formats = Vec::new();
    let mut options = rest[1..].iter().take_while(|a| *a != "--");
    while let Some(arg) = options.next() {
        if arg == "--message-format" {
            formats.extend(options.next().map(|v| v.as_bytes()));
        } else if let Some(value) = arg.as_bytes().strip_prefix(b"--message-format=") {
            formats.push(value);
        }
    }
    if formats.is_empty() {
        return Observe::Inject(globals.len() + 1);
    }
    let json = formats
        .iter()
        .flat_map(|f| f.split(|&b| b == b','))
        .all(|v| v.starts_with(b"json"));
    if json {
        Observe::AsIs
    } else {
        // Cargo refuses to mix them with JSON.
        Observe::Unobservable("asks for human-readable messages")
    }
}

/// Runs a warm cargo call so that Ferry learns which units it used: with
/// Cargo's JSON messages on stdout, recorded under `records` and, unless the
/// call asked for them, kept out of the output. Exits with cargo's status.
fn observe(
    mut command: Command,
    cargo: &Path,
    args: &[OsString],
    records: &Path,
    target: &Path,
    root: &Path,
) -> Result<std::convert::Infallible> {
    let mut args = args.to_vec();
    let forward_messages = match classify(&args) {
        Observe::Nothing => None,
        Observe::Unobservable(why) => {
            let subcommand = split_globals(&args).1.first().cloned().unwrap_or_default();
            crate::prune::Log::unobservable(
                records,
                &format!("`cargo {}` {why}", subcommand.to_string_lossy()),
            )?;
            None
        }
        Observe::AsIs => Some(true),
        Observe::Inject(at) => {
            args.insert(at, "--message-format=json-render-diagnostics".into());
            Some(false)
        }
    };
    command.args(&args);
    let Some(forward_messages) = forward_messages else {
        return Err(exec_error(cargo, sys::exec(&mut command)));
    };
    let mut log = crate::prune::Log::start(records, target, root)?;
    let mut child = command
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| exec_error(cargo, e))?;
    if let Some(stdout) = child.stdout.take() {
        // Program output (`cargo run`, test binaries) shares stdout with the
        // messages and passes through byte for byte.
        let mut reader = std::io::BufReader::new(stdout);
        let mut out = std::io::stdout().lock();
        let mut line = Vec::new();
        while reader
            .read_until(b'\n', &mut line)
            .at("read", "cargo's output")?
            > 0
        {
            if !log.message(&line)? || forward_messages {
                let _ = out.write_all(&line);
            }
            line.clear();
        }
        let _ = out.flush();
    }
    let status = child.wait().map_err(|e| exec_error(cargo, e))?;
    #[cfg(unix)]
    let code = status.code().unwrap_or_else(|| {
        128 + std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0)
    });
    #[cfg(windows)]
    let code = status.code().unwrap_or(1);
    log.end(code)?;
    std::process::exit(code)
}

fn exec_error(cargo: &Path, source: std::io::Error) -> Error {
    Error::Sandbox(crate::sandbox::SandboxError::Exec {
        program: cargo.display().to_string(),
        source,
    })
}

/// Where in a target dir the target dirs of nested workspaces go.
pub(crate) const NESTED_TARGETS: &str = "ferry-ws";

/// Set for a base's warm commands: the checkout and target dir they build,
/// so that their cargo calls get the same per-workspace target dirs as an
/// agent's.
pub(crate) const WARM_SRC_ENV: &str = "FERRY_WARM_SRC";
pub(crate) const WARM_TARGET_ENV: &str = "FERRY_WARM_TARGET";
/// Set for a base's warm commands: `cargo -V` of the toolchain the base
/// records, for [`keep_incremental`].
pub(crate) const WARM_CARGO_ENV: &str = "FERRY_WARM_CARGO";
/// Set when Ferry observes which units the warm commands use: the directory
/// the shim records their cargo calls in (see `prune`).
pub(crate) const WARM_UNITS_ENV: &str = "FERRY_WARM_UNITS";

/// Points `command` at `target`, or for a workspace with no target here,
/// clears any inherited one so that it uses its own as plain cargo would.
pub(crate) fn set_target(command: &mut Command, target: Option<PathBuf>) {
    match target {
        Some(target) => command
            .env("CARGO_TARGET_DIR", &target)
            .env("CARGO_BUILD_BUILD_DIR", &target),
        None => command
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_BUILD_DIR"),
    };
}

/// The workspace a cargo invocation selects, relative to the checkout it
/// runs for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Workspace {
    /// Rooted at the checkout's root: the one the base warms by default.
    Root,
    /// Rooted at this path inside the checkout.
    Nested(PathBuf),
    /// Rooted outside the checkout, for example in another agent's worktree.
    Outside,
}

impl Workspace {
    /// Its target dir, given the checkout's.
    pub(crate) fn target(&self, target: &Path) -> Option<PathBuf> {
        match self {
            Self::Root => Some(target.to_path_buf()),
            // Cargo hashes path packages by their path relative to the
            // workspace root, so two workspaces sharing a target dir would
            // overwrite each other's units with the same names.
            Self::Nested(rel) => Some(target.join(NESTED_TARGETS).join(rel)),
            Self::Outside => None,
        }
    }
}

/// Finds the workspace `args` select when cargo runs in `cwd`. `checkout`
/// lists the paths the checkout is reachable at.
pub(crate) fn workspace(
    cargo: &Path,
    args: &[OsString],
    cwd: &Path,
    checkout: &[&Path],
) -> Workspace {
    let (globals, rest) = split_globals(args);
    let manifest = manifest_path(rest);
    if rest.is_empty() || (manifest.is_none() && checkout.iter().any(|c| same_dir(c, cwd))) {
        return Workspace::Root;
    }
    let mut locate = Command::new(cargo);
    locate
        .args(globals)
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(manifest) = manifest {
        locate.arg("--manifest-path").arg(manifest);
    }
    let located = locate
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| PathBuf::from(OsString::from_vec(out.stdout.trim_ascii_end().to_vec())));
    // Without a workspace the command itself fails, or (`cargo new`) does not
    // build anything.
    let Some(root) = located
        .as_deref()
        .and_then(Path::parent)
        .and_then(|r| sys::canonicalize(r).ok())
    else {
        return Workspace::Root;
    };
    for dir in checkout {
        if let Some(rel) = sys::canonicalize(dir)
            .ok()
            .and_then(|dir| root.strip_prefix(dir).ok().map(Path::to_path_buf))
        {
            return if rel.as_os_str().is_empty() {
                Workspace::Root
            } else {
                Workspace::Nested(rel)
            };
        }
    }
    Workspace::Outside
}

/// Splits off the options before cargo's subcommand (`+toolchain`, `-C dir`,
/// `-Z flag`, `--config k=v`, ...), which `locate-project` needs as well.
fn split_globals(args: &[OsString]) -> (&[OsString], &[OsString]) {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let arg = arg.as_bytes();
        if (i == 0 && arg.starts_with(b"+")) || arg.starts_with(b"-") {
            let takes_value =
                matches!(arg, b"-C" | b"-Z" | b"--config" | b"--color" | b"--explain");
            i += if takes_value { 2 } else { 1 };
        } else {
            break;
        }
    }
    args.split_at(i.min(args.len()))
}

fn manifest_path(args: &[OsString]) -> Option<&OsStr> {
    let mut args = args.iter().take_while(|a| *a != "--");
    while let Some(arg) = args.next() {
        if arg == "--manifest-path" {
            return args.next().map(OsString::as_os_str);
        }
        if let Some(path) = arg.as_bytes().strip_prefix(b"--manifest-path=") {
            return Some(OsStr::from_bytes(path));
        }
    }
    None
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let resolve = |p: &Path| sys::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    resolve(a) == resolve(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn same_dir_resolves_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(same_dir(&real, &link));
        assert!(!same_dir(&real, dir.path()));
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn global_options_are_split_from_the_subcommand() {
        let args = os(&[
            "+nightly",
            "-Zunstable-options",
            "-C",
            "sub",
            "--locked",
            "build",
            "-p",
            "x",
        ]);
        let (globals, rest) = split_globals(&args);
        assert_eq!(globals, &args[..5]);
        assert_eq!(rest, &args[5..]);
        let args = os(&["--config", "build.jobs=1", "test"]);
        assert_eq!(split_globals(&args).1, os(&["test"]));
        assert_eq!(split_globals(&os(&["-V"])).1, &[] as &[OsString]);
        assert_eq!(split_globals(&os(&["-C"])).1, &[] as &[OsString]);
    }

    #[test]
    fn manifest_path_stops_at_the_program_arguments() {
        let path = |args: &[&str]| manifest_path(&os(args)).map(OsStr::to_owned);
        assert_eq!(
            path(&["build", "--manifest-path", "a/Cargo.toml"]),
            Some("a/Cargo.toml".into())
        );
        assert_eq!(
            path(&["build", "--manifest-path=b/Cargo.toml"]),
            Some("b/Cargo.toml".into())
        );
        assert_eq!(path(&["run", "--", "--manifest-path", "c"]), None);
        assert_eq!(path(&["build"]), None);
    }

    #[test]
    fn warm_calls_are_classified_by_what_they_build() {
        let classify = |args: &[&str]| classify(&os(args));
        assert_eq!(classify(&["build"]), Observe::Inject(1));
        assert_eq!(
            classify(&["+nightly", "-Zx", "build", "-p", "a"]),
            Observe::Inject(3)
        );
        assert_eq!(
            classify(&["run", "-p", "app", "--", "--message-format", "x"]),
            Observe::Inject(1)
        );
        assert_eq!(
            classify(&["clippy", "--", "-D", "warnings"]),
            Observe::Inject(1)
        );
        assert_eq!(classify(&["test", "--message-format=json"]), Observe::AsIs);
        assert_eq!(
            classify(&["build", "--message-format", "json-diagnostic-short,json"]),
            Observe::AsIs
        );
        for unobservable in [
            &["check", "--message-format", "short"][..],
            &["build", "--message-format=json,human"],
            &["nextest", "run"],
            &["xtask"],
            &["install", "ripgrep"],
        ] {
            assert!(
                matches!(classify(unobservable), Observe::Unobservable(_)),
                "{unobservable:?}"
            );
        }
        for nothing in [
            &["metadata"][..],
            &["-V"],
            &["--list"],
            &[],
            &["clean", "-p", "a"],
        ] {
            assert_eq!(classify(nothing), Observe::Nothing, "{nothing:?}");
        }
    }

    const CARGO_1_99: &str = "cargo 1.99.0 (5f94df478 2026-08-27)";

    /// [`needs_incremental`] with `vars` as the environment and `configs`
    /// as the Cargo configs.
    fn needs(cargo_version: &str, vars: &[(&str, &str)], configs: &[&str]) -> bool {
        let env = |name: &str| {
            vars.iter()
                .find(|(var, _)| *var == name)
                .map(|(_, value)| OsString::from(value))
        };
        needs_incremental(cargo_version, env, || {
            configs.iter().map(|c| c.parse().unwrap()).collect()
        })
    }

    #[test]
    fn incremental_is_set_where_only_ci_turns_it_off() {
        for ci in [
            ("CI", "true"),
            ("CI", "1"),
            ("CI", ""),
            ("TF_BUILD", "True"),
        ] {
            assert!(needs(CARGO_1_99, &[ci], &[]), "{ci:?}");
        }
        assert!(!needs(CARGO_1_99, &[], &[]));
        assert!(!needs(CARGO_1_99, &[("GITHUB_ACTIONS", "true")], &[]));
        // Cargo does not count a value that is not UTF-8.
        let not_utf8 = |name: &str| (name == "CI").then(|| OsString::from_vec(vec![0xff]));
        assert!(!needs_incremental(CARGO_1_99, not_utf8, Vec::new));

        let ci = [("CI", "true")];
        for explicit in [
            ("CARGO_INCREMENTAL", "0"),
            ("CARGO_INCREMENTAL", "1"),
            ("CARGO_BUILD_INCREMENTAL", "false"),
        ] {
            assert!(!needs(CARGO_1_99, &[ci[0], explicit], &[]), "{explicit:?}");
        }
        for config in ["[build]\nincremental = false", "build.incremental = true"] {
            assert!(!needs(CARGO_1_99, &ci, &["", config]), "{config}");
        }
        // Cargo's switch beats profile settings.
        assert!(needs(
            CARGO_1_99,
            &ci,
            &["[build]\njobs = 2\n[profile.dev]\nincremental = true"]
        ));
    }

    #[test]
    fn incremental_is_set_from_cargo_1_99_on() {
        let ci = [("CI", "true")];
        for newer in [
            "cargo 1.99.0-nightly (0123456789 2026-07-20)",
            "cargo 1.99.1",
            "cargo 1.101.0-nightly (2d3e3e5f8 2026-10-02)",
            "cargo 2.0.0",
        ] {
            assert!(needs(newer, &ci, &[]), "{newer}");
        }
        for older in [
            "cargo 1.98.0 (5d3e1c0b2 2026-07-16)",
            "cargo 1.9.0",
            "cargo 1.88.0 (873a06493 2025-05-10)",
            "rustc 1.99.0",
            "cargo",
            "",
        ] {
            assert!(!needs(older, &ci, &[]), "{older}");
        }
    }

    #[test]
    fn configs_come_from_the_command_line_and_the_directory_tree() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("ws/member");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir(dir.path().join(".cargo")).unwrap();
        std::fs::write(
            dir.path().join(".cargo/config"),
            "build.incremental = false",
        )
        .unwrap();
        std::fs::write(cwd.join("extra.toml"), "build.jobs = 3").unwrap();
        let args = os(&[
            "--config",
            "build.incremental=true",
            "build",
            "--config=extra.toml",
            "--",
            "--config",
            "program.argument=1",
        ]);
        let configs = cargo_configs(&args, &cwd, None);
        let expected: Vec<toml::Table> = [
            "build.incremental = true",
            "build.jobs = 3",
            "build.incremental = false",
        ]
        .iter()
        .map(|c| c.parse().unwrap())
        .collect();
        // The rest, if any, are in the temporary directory's ancestors.
        assert_eq!(configs[..3], expected);
        assert!(!configs.iter().any(|c| c.contains_key("program")));
    }

    #[test]
    fn build_incremental_in_cargo_home_counts() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("ws");
        let home = dir.path().join("cargo-home");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join("config.toml"), "[build]\nincremental = true\n").unwrap();

        let configs = cargo_configs(&[], &cwd, Some(&home));
        let expected: toml::Table = "build.incremental = true".parse().unwrap();
        assert_eq!(configs.last(), Some(&expected));
        let ci = |name: &str| (name == "CI").then(|| OsString::from("true"));
        assert!(!needs_incremental(CARGO_1_99, ci, || configs));
    }

    #[test]
    fn nested_workspaces_get_their_own_target_dir() {
        let target = Path::new("/t");
        assert_eq!(Workspace::Root.target(target), Some(target.to_path_buf()));
        assert_eq!(
            Workspace::Nested("fuzz".into()).target(target),
            Some("/t/ferry-ws/fuzz".into())
        );
        assert_eq!(Workspace::Outside.target(target), None);
    }
}
