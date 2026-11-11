//! Experimental: path-independent rustc incremental state for builds that run
//! without the namespace sandbox (macOS, or Linux without user namespaces).
//!
//! rustc discards or invalidates incremental state when the absolute working
//! directory or `--out-dir` differ from the previous session. Ferry's
//! `ferry-rustc` `RUSTC_WORKSPACE_WRAPPER` (a symlink to the ferry binary)
//! adds `-Zremap-cwd-prefix=.` and rewrites absolute worktree and target paths
//! in rustc's argv to relative ones, for workspace members only. The target
//! dir is reached through a link inside the source root, so the relative path
//! from source to target is identical in the base and every agent.

use std::ffi::{OsStr, OsString};
#[cfg_attr(windows, allow(unused_imports))]
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[cfg_attr(windows, allow(unused_imports))]
use crate::error::IoContext;
use crate::error::{Error, Result};
use crate::sys::{OsStrExt, OsStringExt};
use crate::toolchain::Toolchain;

/// Name of the wrapper symlink in the shim directory.
pub const WRAPPER_NAME: &str = "ferry-rustc";

/// Name of the link to the target dir inside a source root.
pub const TARGET_LINK: &str = ".ferry-target";

/// Keeps the absolute cwd out of rustc's tracked options. `.` (rather than a
/// fixed absolute path) also makes debuggers find sources when started from
/// the worktree root.
const REMAP_CWD: &str = "-Zremap-cwd-prefix=.";

/// Whether `toolchain` supports remapping; the error says why not.
pub fn supported(toolchain: &Toolchain) -> std::result::Result<(), String> {
    let release = &toolchain.release;
    let version = release.split(['-', '+']).next().unwrap_or_default();
    let mut parts = version.split('.').map(str::parse::<u32>);
    match (parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor))) if (major, minor) >= (1, 98) => Ok(()),
        // Before rust-lang/rust#157348, -Zremap-cwd-prefix stored the absolute
        // cwd in the tracked options anyway.
        (Some(Ok(_)), Some(Ok(_))) => Err(format!(
            "rustc {release} is too old; path remapping needs rustc 1.98 or newer"
        )),
        _ => Err(format!("cannot parse the rustc release `{release}`")),
    }
}

/// Set for the wrapper when the toolchain accepts `#![feature]` by itself.
const NIGHTLY_ENV: &str = "FERRY_REMAP_NIGHTLY";

/// The wrapper as a build uses it.
#[derive(Debug, Clone)]
pub struct Wrapper {
    pub path: PathBuf,
    /// Whether the build's toolchain is a nightly one.
    pub nightly: bool,
}

/// `<store parent>/bin/ferry-rustc`, (re)pointed at the running binary. The
/// path must be stable: Cargo hashes the wrapper path into workspace members'
/// artifact names.
pub fn ensure_wrapper(toolchain: &Toolchain) -> Result<Wrapper> {
    Ok(Wrapper {
        path: crate::shim::ensure_link(WRAPPER_NAME)?.join(crate::sys::exe_name(WRAPPER_NAME)),
        nightly: toolchain.is_nightly(),
    })
}

#[cfg(windows)]
pub fn prepare(_: &Path, _: &Path) -> Result<PathBuf> {
    Err(Error::Refused(
        "remap-paths is not supported on Windows yet; set `[sandbox] remap-paths = false`".into(),
    ))
}

/// Creates `<src_root>/.ferry-target -> target` (hidden from `git status`)
/// and returns the link path, which builds use as their target dir.
#[cfg(unix)]
pub fn prepare(src_root: &Path, target: &Path) -> Result<PathBuf> {
    let link = src_root.join(TARGET_LINK);
    let target = std::path::absolute(target).at("resolve", target)?;
    match fs::symlink_metadata(&link) {
        Ok(meta) if !meta.is_symlink() => {
            return Err(Error::Refused(format!(
                "{} exists and is not a symlink; remove it to build with remap-paths",
                link.display()
            )));
        }
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e).at("stat", &link),
        _ => crate::shim::symlink_atomic(&target, &link)?,
    }
    hide_from_git(src_root)?;
    Ok(link)
}

/// Adds `/.ferry-target` to the repository's shared `info/exclude`, which
/// every worktree reads.
#[cfg(unix)]
fn hide_from_git(src_root: &Path) -> Result<()> {
    let info = crate::git::Repo::discover(src_root)?
        .common_dir
        .join("info");
    let exclude = info.join("exclude");
    let pattern = format!("/{TARGET_LINK}");
    let existing = match fs::read(&exclude) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).at("read", &exclude),
    };
    if existing
        .split(|b| *b == b'\n')
        .any(|line| line.trim_ascii() == pattern.as_bytes())
    {
        return Ok(());
    }
    fs::create_dir_all(&info).at("create directory", &info)?;
    let separator = if existing.is_empty() || existing.ends_with(b"\n") {
        ""
    } else {
        "\n"
    };
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)
        .and_then(|mut f| writeln!(f, "{separator}{pattern}"))
        .at("write", &exclude)
}

/// Extra cargo environment in remap mode.
pub fn cargo_env(wrapper: &Wrapper) -> Vec<(OsString, OsString)> {
    let mut env = vec![(
        "RUSTC_WORKSPACE_WRAPPER".into(),
        wrapper.path.as_os_str().to_owned(),
    )];
    if wrapper.nightly {
        env.push((NIGHTLY_ENV.into(), "1".into()));
    }
    env
}

/// True if the process was started through the wrapper link.
pub fn invoked_as_wrapper() -> bool {
    crate::shim::invoked_as(WRAPPER_NAME)
}

/// Entry point when invoked as `ferry-rustc <rustc> <args...>`.
pub fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(rustc) = args.next() else {
        let _ = writeln!(
            io::stderr(),
            "ferry-rustc: usage: ferry-rustc <rustc> [args...] (a RUSTC_WORKSPACE_WRAPPER)"
        );
        return ExitCode::from(2);
    };
    let args: Vec<OsString> = args.collect();
    let mut command = Command::new(&rustc);
    match (std::env::current_dir(), incremental_crate(&args)) {
        // Everything else, notably Cargo's `-vV` and stdin probes, must pass
        // through unchanged: Cargo hashes their output into fingerprints.
        (Ok(cwd), Some(crate_name)) => {
            command
                .args(rewrite_args(&args, &Roots::new(cwd)))
                .arg(REMAP_CWD);
            // RUSTC_BOOTSTRAP is only meant to unlock the -Z flag; a crate that
            // could not use `#![feature]` without the wrapper must not with it.
            let bootstrap = std::env::var_os("RUSTC_BOOTSTRAP");
            let nightly = std::env::var_os(NIGHTLY_ENV).is_some();
            if !features_allowed(nightly, bootstrap.as_deref(), crate_name) {
                command.arg("-Zallow-features=");
            }
            command.env("RUSTC_BOOTSTRAP", "1");
        }
        _ => {
            command.args(&args);
        }
    }
    let err = crate::sys::exec(&mut command);
    let _ = writeln!(
        io::stderr(),
        "ferry-rustc: failed to run `{}`: {err}",
        rustc.to_string_lossy()
    );
    ExitCode::from(127)
}

/// The crate name, if `args` are an incremental compile of a crate.
fn incremental_crate(args: &[OsString]) -> Option<&[u8]> {
    let mut crate_name = None;
    let mut incremental = false;
    let mut prev: &[u8] = b"";
    for arg in args {
        let arg = arg.as_bytes();
        if prev == b"--crate-name" {
            crate_name = Some(arg);
        } else if let Some(name) = arg.strip_prefix(b"--crate-name=") {
            crate_name = Some(name);
        }
        incremental |= arg.starts_with(b"-Cincremental=")
            || arg.starts_with(b"--codegen=incremental=")
            || (matches!(prev, b"-C" | b"--codegen") && arg.starts_with(b"incremental="));
        prev = arg;
    }
    crate_name.filter(|_| incremental)
}

/// Whether rustc would accept `#![feature]` in `crate_name` without the
/// wrapper, by rustc's own reading of `RUSTC_BOOTSTRAP`.
fn features_allowed(nightly: bool, bootstrap: Option<&OsStr>, crate_name: &[u8]) -> bool {
    match bootstrap.map(OsStr::as_bytes) {
        Some(b"1") => true,
        Some(b"-1") => false,
        Some(crates) if crates.split(|b| *b == b',').any(|c| c == crate_name) => true,
        _ => nightly,
    }
}

fn rewrite_args(args: &[OsString], roots: &Roots) -> Vec<OsString> {
    let mut prev: &[u8] = b"";
    args.iter()
        .map(|arg| {
            let bytes = arg.as_bytes();
            // Its FROM side is matched against the paths rustc sees, so making
            // it relative would change what it remaps.
            let remap_rule =
                prev == b"--remap-path-prefix" || bytes.starts_with(b"--remap-path-prefix=");
            prev = bytes;
            if remap_rule {
                arg.clone()
            } else {
                roots.rewrite(arg)
            }
        })
        .collect()
}

/// The working directory and where its target link points, both canonical.
struct Roots {
    cwd: PathBuf,
    target: Option<PathBuf>,
}

impl Roots {
    /// `cwd` must be canonical, as `std::env::current_dir` returns it.
    fn new(cwd: PathBuf) -> Self {
        let target = crate::sys::canonicalize(cwd.join(TARGET_LINK)).ok();
        Self { cwd, target }
    }

    /// Rewrites an absolute path in `arg`, either the whole argument or the
    /// value after its first `=` (`dependency=/..`, `incremental=/..`,
    /// `name=/..` for `--extern`).
    fn rewrite(&self, arg: &OsStr) -> OsString {
        let bytes = arg.as_bytes();
        let start = if starts_absolute(bytes) {
            0
        } else {
            match bytes.iter().position(|b| *b == b'=') {
                Some(eq) if starts_absolute(&bytes[eq + 1..]) => eq + 1,
                _ => return arg.to_owned(),
            }
        };
        let (prefix, path) = bytes.split_at(start);
        match self.relative(Path::new(OsStr::from_bytes(path))) {
            Some(rel) => OsString::from_vec([prefix, rel.as_os_str().as_bytes()].concat()),
            None => arg.to_owned(),
        }
    }

    /// `path` relative to the working dir, when it lies under it or under the
    /// target link and the relative form names the same file.
    fn relative(&self, path: &Path) -> Option<PathBuf> {
        let resolved = resolve(path)?;
        let candidates = [
            path.strip_prefix(&self.cwd).ok().map(Path::to_path_buf),
            self.target
                .as_ref()
                .and_then(|t| resolved.strip_prefix(t).ok())
                .map(|rest| Path::new(TARGET_LINK).join(rest)),
            resolved.strip_prefix(&self.cwd).ok().map(Path::to_path_buf),
        ];
        let rel = candidates
            .into_iter()
            .flatten()
            .find(|rel| resolve(&self.cwd.join(rel)).as_ref() == Some(&resolved))?;
        Some(if rel.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            rel
        })
    }
}

/// Whether `bytes` start with an absolute path: `/` on Unix; also a drive
/// (`C:\`, `C:/`) or a UNC or verbatim prefix (`\\`) on Windows.
fn starts_absolute(bytes: &[u8]) -> bool {
    if bytes.starts_with(b"/") {
        return true;
    }
    cfg!(windows)
        && (bytes.starts_with(b"\\\\")
            || matches!(bytes, [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic()))
}

/// `path` with symlinks resolved, also for outputs that do not exist yet: the
/// deepest existing ancestor is canonicalized and the rest appended.
fn resolve(path: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(canonical) = crate::sys::canonicalize(existing) {
            return Some(missing.iter().rev().fold(canonical, |p, c| p.join(c)));
        }
        missing.push(existing.file_name()?);
        existing = existing.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toolchain::Toolchain;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn requires_rustc_1_98() {
        let check = |release: &str| {
            let vv = format!("rustc x\nhost: x86_64-unknown-linux-gnu\nrelease: {release}\n");
            supported(&Toolchain::parse(&vv, "cargo x"))
        };
        for ok in [
            "1.98.0",
            "1.99.0",
            "1.101.0-nightly",
            "1.98.0-beta.3",
            "2.0.0",
        ] {
            assert!(check(ok).is_ok(), "{ok}");
        }
        for old in ["1.97.1", "1.88.0", "0.99.0"] {
            assert!(check(old).unwrap_err().contains("too old"), "{old}");
        }
        for bad in ["", "nightly", "1"] {
            assert!(check(bad).unwrap_err().contains("cannot parse"), "{bad:?}");
        }
    }

    #[test]
    fn only_incremental_compiles_are_rewritten() {
        let cargo_compile = os(&[
            "--crate-name",
            "lib",
            "--edition=2024",
            "lib/src/lib.rs",
            "-C",
            "incremental=/t/debug/incremental",
        ]);
        let lib: Option<&[u8]> = Some(b"lib");
        assert_eq!(incremental_crate(&cargo_compile), lib);
        assert_eq!(
            incremental_crate(&os(&["--crate-name=lib", "-Cincremental=/t"])),
            lib
        );
        assert_eq!(
            incremental_crate(&os(&["--crate-name", "lib", "--codegen", "incremental=/t"])),
            lib
        );
        assert_eq!(incremental_crate(&os(&["-vV"])), None);
        let probe = os(&[
            "-",
            "--crate-name",
            "___",
            "--print=file-names",
            "--crate-type",
            "bin",
        ]);
        assert_eq!(incremental_crate(&probe), None);
        assert_eq!(
            incremental_crate(&os(&["--crate-name", "lib", "-C", "opt-level=3"])),
            None
        );
    }

    #[test]
    fn features_follow_rustcs_own_rules() {
        let allowed = |nightly, bootstrap: Option<&str>| {
            features_allowed(nightly, bootstrap.map(OsStr::new), b"lib")
        };
        assert!(!allowed(false, None));
        assert!(allowed(true, None));
        assert!(allowed(false, Some("1")));
        assert!(!allowed(true, Some("-1")));
        assert!(allowed(false, Some("other,lib")));
        assert!(!allowed(false, Some("other")));
        assert!(allowed(true, Some("other")));
    }

    #[cfg(unix)]
    #[test]
    fn rewrites_paths_under_cwd_and_target_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::sys::canonicalize(dir.path()).unwrap();
        let (ws, target) = (root.join("ws"), root.join("target"));
        fs::create_dir_all(ws.join("src")).unwrap();
        fs::create_dir_all(target.join("debug/deps")).unwrap();
        fs::write(target.join("debug/deps/libdep.rlib"), b"").unwrap();
        std::os::unix::fs::symlink(&target, ws.join(TARGET_LINK)).unwrap();
        std::os::unix::fs::symlink(&ws, root.join("alias")).unwrap();
        let roots = Roots::new(ws.clone());
        let at = |p: &Path| p.to_str().unwrap().to_owned();
        let rewrite = |arg: String| roots.rewrite(OsStr::new(&arg)).into_string().unwrap();
        let link = ws.join(TARGET_LINK);

        assert_eq!(
            rewrite(at(&link.join("debug/deps"))),
            ".ferry-target/debug/deps"
        );
        assert_eq!(
            rewrite(format!("dependency={}", at(&link.join("debug/deps")))),
            "dependency=.ferry-target/debug/deps"
        );
        assert_eq!(
            rewrite(format!(
                "dep={}",
                at(&target.join("debug/deps/libdep.rlib"))
            )),
            "dep=.ferry-target/debug/deps/libdep.rlib",
            "canonical target path"
        );
        assert_eq!(
            rewrite(at(&link.join("debug/deps/new-output.o"))),
            ".ferry-target/debug/deps/new-output.o",
            "output that does not exist yet"
        );
        assert_eq!(
            rewrite(at(&root.join("alias/.ferry-target/debug/deps"))),
            ".ferry-target/debug/deps",
            "cwd reached through a symlink"
        );
        assert_eq!(rewrite(at(&ws.join("src"))), "src");
        assert_eq!(rewrite(format!("incremental={}", at(&ws))), "incremental=.");
        for unchanged in [
            at(&root.join("elsewhere/x")),
            "src/lib.rs".to_owned(),
            "opt-level=3".to_owned(),
            "--cfg=feature=\"x\"".to_owned(),
            format!("{}/missing/../x", at(&link)),
        ] {
            assert_eq!(rewrite(unchanged.clone()), unchanged);
        }

        let remap = format!("{}=/cargo/build-dir", at(&link));
        let args = os(&[
            "--remap-path-prefix",
            &remap,
            &format!("--remap-path-prefix={remap}"),
            &at(&ws.join("src")),
        ]);
        assert_eq!(
            rewrite_args(&args, &roots),
            os(&[
                "--remap-path-prefix",
                &remap,
                &format!("--remap-path-prefix={remap}"),
                "src"
            ])
        );
    }

    fn git_init(dir: &Path) {
        let status = Command::new("git")
            .args(["init", "-q"])
            .arg(dir)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[test]
    fn prepare_links_target_and_hides_it_from_git() {
        let dir = tempfile::tempdir().unwrap();
        let (src, target) = (dir.path().join("src"), dir.path().join("target"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&target).unwrap();
        git_init(&src);
        let exclude = src.join(".git/info/exclude");
        fs::write(&exclude, b"# no trailing newline").unwrap();

        let link = prepare(&src, &target).unwrap();
        assert_eq!(link, src.join(TARGET_LINK));
        assert_eq!(fs::read_link(&link).unwrap(), target);
        let other = dir.path().join("other");
        prepare(&src, &other).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), other, "stale link replaced");

        let patterns = fs::read_to_string(&exclude).unwrap();
        assert_eq!(patterns, "# no trailing newline\n/.ferry-target\n");
        let status = Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=all"])
            .current_dir(&src)
            .output()
            .unwrap();
        assert!(status.status.success());
        assert_eq!(String::from_utf8_lossy(&status.stdout), "");
    }

    #[test]
    fn prepare_refuses_to_replace_a_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join(TARGET_LINK)).unwrap();
        git_init(&src);
        let err = prepare(&src, dir.path()).unwrap_err();
        assert!(matches!(err, Error::Refused(_)), "{err}");
        assert!(src.join(TARGET_LINK).is_dir());
    }
}
