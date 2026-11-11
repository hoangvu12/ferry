//! `ferry doctor`: what this machine supports, what Ferry will do, and how to
//! fix what is missing.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{SandboxMode, Settings};
use crate::context::Context;
use crate::fork;
use crate::freshness::{self, FreshnessMode};
use crate::fsutil;
use crate::git;
use crate::platform::{Capabilities, Os, Probe};
use crate::sandbox::Isolation;
use crate::toolchain::Toolchain;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Info,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    /// What to do about a warning or failure.
    pub fix: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
    pub capabilities: Option<Capabilities>,
    /// What `ferry spawn` would do with the current settings.
    pub plan: Option<Plan>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub backend: Option<fork::BackendKind>,
    pub backend_reason: String,
    pub isolation: Isolation,
    pub freshness: Option<FreshnessMode>,
    /// Whether an agent's first edit can reuse the base's incremental state.
    pub incremental_reuse: bool,
}

impl Report {
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|c| c.status == Status::Fail)
    }
}

/// Runs every check. `ctx` is `Err` when the current directory is not a
/// usable repository; the machine-level checks still run.
pub fn run(ctx: Result<&Context, String>) -> Report {
    let mut checks = Vec::new();
    match git::version() {
        Ok(v) => checks.push(ok("git", v)),
        Err(e) => checks.push(fail(
            "git",
            e.to_string(),
            Some("install git 2.20 or newer".into()),
        )),
    }

    let ctx = match ctx {
        Ok(ctx) => ctx,
        Err(e) => {
            checks.push(fail(
                "repository",
                e,
                Some("run `ferry doctor` inside the git repository you want to use".into()),
            ));
            return Report {
                checks,
                capabilities: None,
                plan: None,
            };
        }
    };
    checks.push(ok(
        "repository",
        ctx.repo.main_worktree.display().to_string(),
    ));
    checks.push(match &ctx.settings.config_path {
        Some(p) => ok("config", p.display().to_string()),
        None => info(
            "config",
            "no ferry.toml; using defaults (`ferry init` writes one)".into(),
        ),
    });

    let toolchain = Toolchain::detect(&ctx.repo.main_worktree);
    let freshness = match &toolchain {
        Ok(t) => {
            checks.push(ok(
                "toolchain",
                format!("rustc {} ({}), {}", t.release, t.channel, t.cargo_version),
            ));
            // Only here: without a toolchain the clippy probe fails too,
            // and "not installed" would point at the wrong fix.
            checks.extend(clippy_check(ctx));
            freshness::resolve_mode(ctx.settings.freshness, t).ok()
        }
        Err(e) => {
            checks.push(fail(
                "toolchain",
                e.to_string(),
                Some("install Rust with rustup".into()),
            ));
            None
        }
    };
    if let Some(mode) = freshness {
        checks.push(match mode {
            FreshnessMode::Checksum => {
                ok("freshness", "checksum freshness (content hashes)".into())
            }
            FreshnessMode::Mtime => ok(
                "freshness",
                "mtime stamping of files proven identical to the base".into(),
            ),
        });
    }

    let caps = Capabilities::detect(ctx.store.root());
    checks.push(store_check(ctx.store.root(), &caps));
    checks.push(match &caps.reflink {
        Probe::Ok => ok(
            "reflink",
            format!("copy-on-write clones work on {}", caps.store_fs.name),
        ),
        other => info(
            "reflink",
            format!(
                "unavailable on {}: {}",
                caps.store_fs.name,
                probe_detail(other)
            ),
        ),
    });
    checks.extend(namespace_checks(&caps));
    match crate::base::unapplied_cargo_configs(ctx) {
        Ok(configs) => checks.extend(configs.iter().map(|config| {
            warn(
                "cargo config",
                crate::base::unapplied_message(ctx, config),
                Some(crate::base::CONFIG_ADVICE.into()),
            )
        })),
        Err(e) => checks.push(warn("cargo config", e.to_string(), None)),
    }

    let plan = plan(&ctx.settings, &caps, freshness);
    checks.extend(remap_check(ctx, plan.isolation, toolchain.ok().as_ref()));
    Report {
        checks,
        capabilities: Some(caps),
        plan: Some(plan),
    }
}

fn clippy_check(ctx: &Context) -> Option<Check> {
    if !ctx.settings.warm_commands.iter().any(|c| c.clippy) {
        return None;
    }
    Some(if Toolchain::clippy_available(&ctx.repo.main_worktree) {
        ok("clippy", "installed; bases warm `cargo clippy`".into())
    } else {
        Check {
            fix: Some("rustup component add clippy".into()),
            ..info(
                "clippy",
                "not installed for this toolchain; bases skip warming it".into(),
            )
        }
    })
}

/// Shown when `ferry base` would remap paths: remap-paths is on, there is no
/// namespace sandbox and the toolchain supports it (as in `resolve_remap`).
fn remap_check(
    ctx: &Context,
    isolation: Isolation,
    toolchain: Option<&Toolchain>,
) -> Option<Check> {
    let remaps = ctx.settings.remap_paths
        && isolation == Isolation::None
        && toolchain.is_some_and(|t| crate::remap::supported(t).is_ok());
    remaps.then(|| {
        info(
            "remap-paths",
            "`cargo clippy` runs are not remapped: clippy replaces the wrapper, so workspace \
             crates re-lint without incremental reuse"
                .into(),
        )
    })
}

fn store_check(root: &Path, caps: &Capabilities) -> Check {
    let free = crate::sys::free_space(root).ok();
    let detail = format!(
        "{} ({}{})",
        root.display(),
        caps.store_fs.name,
        free.map(|b| format!(", {} free", fsutil::human_bytes(b)))
            .unwrap_or_default()
    );
    if matches!(
        caps.store_fs.name.as_str(),
        "nfs" | "cifs" | "smb2" | "fuse"
    ) {
        return warn(
            "store",
            detail,
            Some("network and FUSE filesystems make builds slow and mtimes unreliable; set FERRY_HOME to a local disk".into()),
        );
    }
    if caps.store_fs.name == "overlay" {
        return warn(
            "store",
            detail,
            Some("overlayfs cannot use a store on overlayfs as its upper layer (the overlay \
                  backend falls back to reflink or copy); in a container, put FERRY_HOME on a volume"
                .into()),
        );
    }
    ok("store", detail)
}

fn namespace_checks(caps: &Capabilities) -> Vec<Check> {
    if caps.os != Os::Linux {
        let os = if caps.os == Os::Windows {
            "Windows"
        } else {
            "macOS"
        };
        return vec![info(
            "sandbox",
            format!(
                "{os} has no mount namespaces: agents build at their own paths (only crates \
                 that captured the base's paths rebuild; no incremental reuse on the first edit)"
            ),
        )];
    }
    let mut out = Vec::new();
    match &caps.userns {
        Probe::Ok => out.push(ok(
            "namespaces",
            "unprivileged user + mount namespaces work".into(),
        )),
        other => out.push(warn(
            "namespaces",
            probe_detail(other),
            Some(userns_fix(caps)),
        )),
    }
    match &caps.overlay {
        Probe::Ok => out.push(ok("overlayfs", "mounts inside user namespaces".into())),
        other if caps.userns.is_ok() => out.push(warn(
            "overlayfs",
            probe_detail(other),
            Some("overlayfs needs Linux 5.11+ and a store filesystem with user xattrs (ext4, btrfs, xfs; tmpfs on 6.6+)".into()),
        )),
        _ => {}
    }
    out
}

pub(crate) fn userns_fix(caps: &Capabilities) -> String {
    if let Some(marker) = crate::setup::container_marker() {
        return format!(
            "this runs in a container ({}): allow unshare and mount in its seccomp profile, keep \
             FERRY_HOME on a volume, and on Ubuntu 24.04+ hosts change the host; see the \
             troubleshooting guide (Containers), or set FERRY_SANDBOX=off",
            marker.display()
        );
    }
    if caps.sysctl("kernel.apparmor_restrict_unprivileged_userns") == Some("1") {
        let profiled = crate::setup::profiled_binaries(Path::new(crate::setup::PROFILE_DIR));
        if let Ok(exe) = crate::setup::current_exe()
            && !profiled.contains(&exe)
            && let Some(other) = profiled.first()
        {
            return format!(
                "a Ferry AppArmor profile covers {}, but this is {}: run that binary, or re-run \
                 `ferry setup apparmor`",
                other.display(),
                exe.display()
            );
        }
        return "Ubuntu's AppArmor policy denies capabilities in unprivileged user namespaces: \
                run `ferry setup apparmor`, which shows each step and asks before one sudo \
                command (manual steps: the troubleshooting guide)"
            .into();
    }
    if caps.sysctl("kernel.unprivileged_userns_clone") == Some("0") {
        return "unprivileged user namespaces are disabled: `sudo sysctl kernel.unprivileged_userns_clone=1`".into();
    }
    if caps.sysctl("user.max_user_namespaces") == Some("0") {
        return "user namespaces are disabled: `sudo sysctl user.max_user_namespaces=15000`".into();
    }
    "if this runs in a container, allow unshare(CLONE_NEWUSER) (e.g. a seccomp profile permitting it), \
     or set FERRY_SANDBOX=off"
        .into()
}

fn plan(settings: &Settings, caps: &Capabilities, freshness: Option<FreshnessMode>) -> Plan {
    let isolation = if settings.sandbox != SandboxMode::Off && caps.userns.is_ok() {
        Isolation::Namespace
    } else {
        Isolation::None
    };
    let (backend, backend_reason) = match fork::select(settings.backend, caps, isolation) {
        Ok(choice) => (Some(choice.kind), choice.reason),
        Err(e) => (None, e.to_string()),
    };
    Plan {
        backend,
        backend_reason,
        isolation,
        freshness,
        incremental_reuse: isolation == Isolation::Namespace,
    }
}

/// An `AppArmor` profile that lets `binary` create user namespaces on Ubuntu
/// 24.04+ (`kernel.apparmor_restrict_unprivileged_userns=1`).
///
/// It is the stacked form Ubuntu uses for `bwrap-userns-restrict`: ferry
/// itself may create the namespace and mount, while everything it executes
/// runs under `unpriv_ferry`, which denies capabilities. The simpler
/// `flags=(unconfined)` profile would let every process an agent starts
/// create privileged user namespaces, exempting the whole agent from the
/// restriction.
pub fn apparmor_profile(binary: &Path) -> String {
    let path = binary.display();
    let file = apparmor_file_name(binary);
    format!(
        "{PROFILE_MARKER}{path} create user namespaces; what it runs gets no capabilities in them.\n\
         # Managed by `ferry setup apparmor`; remove with `ferry setup apparmor --uninstall`.\n\
         # The path must be root-owned and not writable by users.\n\
         # Manual install: sudo install -m 644 this-file /etc/apparmor.d/{file} && sudo apparmor_parser -r /etc/apparmor.d/{file}\n\
         abi <abi/4.0>,\n\
         include <tunables/global>\n\
         \n\
         profile ferry {path} flags=(attach_disconnected) {{\n\
         \x20 allow capability,\n\
         \x20 allow file rwlkm /{{**,}},\n\
         \x20 allow network,\n\
         \x20 allow unix,\n\
         \x20 allow ptrace,\n\
         \x20 allow signal,\n\
         \x20 allow mqueue,\n\
         \x20 allow io_uring,\n\
         \x20 allow userns,\n\
         \x20 allow mount,\n\
         \x20 allow umount,\n\
         \x20 allow pivot_root,\n\
         \x20 allow dbus,\n\
         \x20 # ferry re-running itself (the namespace probe) must keep its capabilities;\n\
         \x20 # an exact path takes precedence over the pattern below.\n\
         \x20 allow ix {path},\n\
         \x20 allow px /** -> ferry//&unpriv_ferry,\n\
         \n\
         \x20 include if exists <local/ferry>\n\
         }}\n\
         \n\
         profile unpriv_ferry flags=(attach_disconnected) {{\n\
         \x20 allow file rwlkm /{{**,}},\n\
         \x20 allow network,\n\
         \x20 allow unix,\n\
         \x20 allow ptrace,\n\
         \x20 allow signal,\n\
         \x20 allow mqueue,\n\
         \x20 allow io_uring,\n\
         \x20 allow userns,\n\
         \x20 allow mount,\n\
         \x20 allow umount,\n\
         \x20 allow pivot_root,\n\
         \x20 allow dbus,\n\
         \x20 allow pix /** -> &unpriv_ferry,\n\
         \x20 audit deny capability,\n\
         \n\
         \x20 include if exists <local/unpriv_ferry>\n\
         }}\n"
    )
}

/// First line of every profile file Ferry generates, followed by the path.
pub const PROFILE_MARKER: &str = "# Ferry: lets ";

/// The file name Ubuntu's convention gives `binary`'s profile in
/// `/etc/apparmor.d`: its path without the leading `/`, with `.` for `/`.
pub fn apparmor_file_name(binary: &Path) -> String {
    binary
        .to_string_lossy()
        .trim_start_matches('/')
        .replace('/', ".")
}

/// Why `binary` is unsuitable for an `AppArmor` profile, if it is: the
/// profile attaches by path, so anyone who can replace the file inherits the
/// exemption.
pub fn apparmor_path_problem(binary: &Path) -> Option<String> {
    let writable = user_writable_ancestor(binary)?;
    Some(format!(
        "{} is writable by a non-root user, so any process could replace ferry and \
         create user namespaces; `ferry setup apparmor` installs it to /usr/local/bin \
         and loads the profile, or install it to a root-owned path yourself first, e.g. \
         `sudo install -m 755 {} /usr/local/bin/ferry`, then run \
         `/usr/local/bin/ferry doctor --apparmor-profile`",
        writable.display(),
        binary.display()
    ))
}

/// The nearest of `path` and its ancestors that is missing or that a
/// non-root user could write to or replace.
///
/// Only used for the root-owned `AppArmor` install path; on Windows every path
/// counts as user-writable.
#[cfg(windows)]
pub fn user_writable_ancestor(path: &Path) -> Option<PathBuf> {
    Some(path.to_path_buf())
}

#[cfg(unix)]
pub fn user_writable_ancestor(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    // A sticky directory keeps others from renaming the root-owned entry
    // inside it.
    let user_writable = |path: &Path| {
        std::fs::metadata(path).map_or(true, |m| {
            m.uid() != 0 || (m.mode() & 0o022 != 0 && m.mode() & 0o1000 == 0)
        })
    };
    path.ancestors()
        .find(|p| user_writable(p))
        .map(Path::to_path_buf)
}

/// Why `binary` cannot be named in a profile: the profile's attachment is
/// read as a glob, so `*`, `{`, quotes and the like would make it match
/// other binaries or not parse.
pub fn apparmor_path_unusable(binary: &Path) -> Option<String> {
    let path = binary.to_str()?;
    let safe = |c: char| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+');
    (!path.chars().all(safe)).then(|| {
        format!(
            "{path} contains characters an AppArmor profile path cannot hold safely; \
             install ferry to a plain path such as /usr/local/bin/ferry"
        )
    })
}

pub(crate) fn probe_detail(probe: &Probe) -> String {
    match probe {
        Probe::Ok => "ok".into(),
        Probe::Unsupported { reason } => reason.clone(),
        Probe::Failed { stage, error } => format!("{stage} failed: {error}"),
    }
}

fn ok(name: &'static str, detail: String) -> Check {
    Check {
        name,
        status: Status::Ok,
        detail,
        fix: None,
    }
}

fn info(name: &'static str, detail: String) -> Check {
    Check {
        name,
        status: Status::Info,
        detail,
        fix: None,
    }
}

fn warn(name: &'static str, detail: String, fix: Option<String>) -> Check {
    Check {
        name,
        status: Status::Warn,
        detail,
        fix,
    }
}

fn fail(name: &'static str, detail: String, fix: Option<String>) -> Check {
    Check {
        name,
        status: Status::Fail,
        detail,
        fix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apparmor_profile_is_stacked_and_names_the_binary() {
        let p = apparmor_profile(Path::new("/usr/local/bin/ferry"));
        assert!(p.starts_with("# Ferry: lets /usr/local/bin/ferry create user namespaces;"));
        assert!(p.contains("/etc/apparmor.d/usr.local.bin.ferry"));
        assert!(p.contains("profile ferry /usr/local/bin/ferry flags=(attach_disconnected) {"));
        let ferry = &p[p.find("profile ferry ").unwrap()..p.find("profile unpriv_ferry").unwrap()];
        assert!(
            ferry.contains("  allow ix /usr/local/bin/ferry,"),
            "{ferry}"
        );
        assert!(p.contains("  allow userns,"));
        assert!(p.contains("  allow px /** -> ferry//&unpriv_ferry,"));
        assert!(p.contains("profile unpriv_ferry flags=(attach_disconnected) {"));
        assert!(p.contains("  audit deny capability,"));
        assert!(p.contains("  allow file rwlkm /{**,},"));
        assert!(p.contains("abi <abi/4.0>,"));
        assert_eq!(p.matches('{').count(), p.matches('}').count());
    }

    #[test]
    fn profile_files_follow_ubuntu_naming() {
        assert_eq!(
            apparmor_file_name(Path::new("/usr/local/bin/ferry")),
            "usr.local.bin.ferry"
        );
        assert_eq!(
            apparmor_file_name(Path::new("/opt/ferry/bin/ferry")),
            "opt.ferry.bin.ferry"
        );
    }

    #[test]
    fn user_writable_ancestor_finds_the_nearest() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("ferry");
        std::fs::write(&bin, b"").unwrap();
        assert_eq!(user_writable_ancestor(&bin), Some(bin.clone()));
        if rustix::process::geteuid().as_raw() != 0 {
            assert_eq!(user_writable_ancestor(Path::new("/")), None);
        }
    }

    fn caps(store_fs: &str, userns: Probe, restrict: &str) -> Capabilities {
        Capabilities {
            os: Os::Linux,
            kernel: "test".into(),
            store_fs: crate::platform::FsInfo {
                name: store_fs.into(),
            },
            reflink: Probe::Ok,
            userns,
            overlay: Probe::Ok,
            sysctls: vec![crate::platform::Sysctl {
                name: "kernel.apparmor_restrict_unprivileged_userns".into(),
                value: restrict.into(),
            }],
            probe_label: None,
        }
    }

    #[test]
    fn overlay_store_warns() {
        let check = store_check(Path::new("/"), &caps("overlay", Probe::Ok, "0"));
        assert_eq!(check.status, Status::Warn);
        assert!(check.fix.unwrap().contains("FERRY_HOME"));
        let check = store_check(Path::new("/"), &caps("ext4", Probe::Ok, "0"));
        assert_eq!(check.status, Status::Ok);
    }

    #[test]
    fn userns_fix_points_to_setup_apparmor_when_restricted() {
        let failed = Probe::Failed {
            stage: "setgroups".into(),
            error: "Permission denied".into(),
        };
        let fix = userns_fix(&caps("ext4", failed, "1"));
        if crate::setup::container_marker().is_some() {
            assert!(fix.contains("troubleshooting"), "{fix}");
        } else {
            assert!(fix.contains("ferry setup apparmor"), "{fix}");
            assert!(!fix.contains("sudo tee"), "{fix}");
        }
    }

    #[test]
    fn user_writable_binary_is_rejected_for_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("ferry");
        std::fs::write(&bin, b"").unwrap();
        assert!(apparmor_path_problem(&bin).is_some());
    }

    #[test]
    fn glob_characters_cannot_name_a_profile() {
        assert_eq!(
            apparmor_path_unusable(Path::new("/usr/local/bin/ferry")),
            None
        );
        assert_eq!(
            apparmor_path_unusable(Path::new("/opt/ferry-1.0+x/ferry")),
            None
        );
        for bad in [
            "/home/*/ferry",
            "/opt/my ferry/ferry",
            "/opt/{a,b}/ferry",
            "/x\"/ferry",
        ] {
            assert!(apparmor_path_unusable(Path::new(bad)).is_some(), "{bad}");
        }
    }

    #[test]
    fn shipped_profile_matches_generator() {
        let shipped = include_str!("../contrib/apparmor/usr.local.bin.ferry");
        assert_eq!(shipped, apparmor_profile(Path::new("/usr/local/bin/ferry")));
    }
}
