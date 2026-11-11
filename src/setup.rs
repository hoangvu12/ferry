//! `ferry setup apparmor`: on Ubuntu 24.04 and later, installs ferry to a
//! root-owned path and loads the `AppArmor` profile that lets it create user
//! namespaces, through one `sudo` call the user confirms.
//!
//! [`Facts::gather`] reads the machine; [`decide`] and [`decide_uninstall`]
//! are pure, so every branch is tested against made-up machines. The root
//! script takes every path as an argument and the profile on stdin, and the
//! binary is copied from `/proc/<pid>/exe`, so nothing a user's process can
//! write sits between the user's consent and root acting on it.

use std::ffi::OsStr;
use std::io::BufRead;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

use crate::doctor;
use crate::error::{IoContext, Result};
use crate::platform::{Capabilities, Os, Probe};

pub const PROFILE_DIR: &str = "/etc/apparmor.d";
pub const DEFAULT_BIN_DIR: &str = "/usr/local/bin";
const PARSERS: [&str; 2] = ["/usr/sbin/apparmor_parser", "/sbin/apparmor_parser"];
const RESTRICT: &str = "kernel.apparmor_restrict_unprivileged_userns";
/// Podman documents the first; the second is a Docker convention.
const CONTAINER_MARKERS: [&str; 2] = ["/run/.containerenv", "/.dockerenv"];
/// First line of the profiles `ferry doctor --apparmor-profile` printed
/// before this command existed.
const LEGACY_MARKER: &str = "# AppArmor profile: lets ";
/// The running binary as the kernel sees it: replacing the file on disk does
/// not change it.
pub const SELF_EXE: &str = "/proc/self/exe";

/// Run as root by `sh -euc`. `$1` is the pid of the ferry process whose
/// binary to install, `$2` where to install it (empty for no install), `$3`
/// `apparmor_parser`, `$4` the profile file to write from stdin; the rest
/// are older Ferry profile files to remove. Temporary names start with `.`
/// because `AppArmor` skips dot files when it loads the directory at boot.
pub const ROOT_SCRIPT: &str = r#"pid=$1 bin=$2 parser=$3 profile=$4; shift 4
umask 022
tmpbin= tmp=${profile%/*}/.${profile##*/}.new.$$
trap 'rm -f "$tmp" ${tmpbin:+"$tmpbin"}' EXIT
if [ -n "$bin" ]; then
  echo "ferry: installing $bin" >&2
  dir=${bin%/*}; tmpbin=$dir/.ferry.new.$$
  [ -d "$dir" ] || install -d -o root -g root -m 0755 "$dir"
  install -o root -g root -m 0755 "/proc/$pid/exe" "$tmpbin"
  mv -f "$tmpbin" "$bin"
fi
echo "ferry: writing $profile" >&2
cat >"$tmp"; chown root:root "$tmp"; chmod 0644 "$tmp"
"$parser" -Q -K "$tmp"
for old in "$@"; do rm -f "$old"; done
mv -f "$tmp" "$profile"
echo "ferry: loading $profile" >&2
"$parser" -r "$profile"
"#;

/// Run as root by `sh -euc`: `$1` is `apparmor_parser`, the rest are the
/// profile files to unload and remove. `-R` needs the file's definitions.
pub const UNINSTALL_SCRIPT: &str = r#"parser=$1; shift
for profile in "$@"; do
  echo "ferry: unloading and removing $profile" >&2
  "$parser" -R "$profile" || echo "ferry: $profile was not loaded" >&2
  rm -f "$profile"
done
"#;

/// What [`Facts::gather`] found out about this machine.
#[derive(Debug, Clone)]
pub struct Facts {
    pub os: Os,
    pub euid: u32,
    /// Whether this process is in the initial user namespace, the only one
    /// where `sudo` can become root.
    pub initial_userns: bool,
    pub container: Option<PathBuf>,
    /// The running binary's probes.
    pub caps: Capabilities,
    /// The running binary, canonicalized.
    pub exe: PathBuf,
    /// Why `exe` cannot be named in a profile where it is.
    pub exe_problem: Option<String>,
    /// `--bin-dir`, canonicalized as far as it exists.
    pub bin_dir: PathBuf,
    pub bin_dir_problem: Option<String>,
    /// Whether `bin_dir/ferry` is a root-owned regular file with the running
    /// binary's bytes ([`holds_binary`]).
    pub installed: bool,
    /// The namespace probe of [`Facts::target`], when that is `exe`
    /// profiled where it is or an installed copy.
    pub target_userns: Option<Probe>,
    pub parser: Option<PathBuf>,
    /// Whether the `AppArmor` 4.0 ABI the profile declares is installed.
    pub abi: bool,
    pub profile_dir: PathBuf,
    /// Profile files Ferry wrote, recognized by their first line.
    pub ferry_files: Vec<ProfileFile>,
    /// Files that are not Ferry's but define `ferry` or `unpriv_ferry`, or
    /// hold the name Ferry's profile file needs.
    pub foreign_files: Vec<PathBuf>,
    /// `$CARGO_HOME/bin/ferry` when it is not the running binary and has
    /// other bytes: often a `cargo install` upgrade the profiled copy lacks.
    pub cargo_copy: Option<PathBuf>,
}

/// A profile file Ferry wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileFile {
    pub path: PathBuf,
    pub text: String,
}

impl ProfileFile {
    /// The binary its `ferry` profile attaches to.
    pub fn binary(&self) -> Option<PathBuf> {
        self.text
            .lines()
            .find_map(|l| l.strip_prefix("profile ferry ")?.split_whitespace().next())
            .map(PathBuf::from)
    }
}

impl Facts {
    /// Reads the machine. `bin_dir` is where ferry is installed when its
    /// current path cannot be profiled. Never reads a repository.
    pub fn gather(bin_dir: &Path) -> Result<Self> {
        let home = crate::shim::home()?;
        let caps = Capabilities::detect(&home);
        let exe = current_exe()?;
        let exe_problem =
            doctor::apparmor_path_unusable(&exe).or_else(|| doctor::apparmor_path_problem(&exe));
        let (bin_dir, bin_dir_problem) = check_bin_dir(bin_dir);
        let profile_dir = PathBuf::from(PROFILE_DIR);
        let (ferry_files, foreign_files) = scan(&profile_dir);
        let cargo_copy = crate::base::cargo_home()
            .and_then(|home| std::fs::canonicalize(home.join("bin/ferry")).ok())
            .filter(|copy| *copy != exe && !same_bytes(copy, Path::new(SELF_EXE)));
        let mut facts = Facts {
            os: caps.os,
            #[cfg(unix)]
            euid: rustix::process::geteuid().as_raw(),
            // Never root: AppArmor setup does not apply on Windows anyway.
            #[cfg(windows)]
            euid: u32::MAX,
            initial_userns: std::fs::read_to_string("/proc/self/uid_map")
                .is_ok_and(|map| map.split_whitespace().eq(["0", "0", "4294967295"])),
            container: container_marker(),
            caps,
            exe,
            exe_problem,
            bin_dir,
            bin_dir_problem,
            installed: false,
            target_userns: None,
            parser: PARSERS.iter().map(PathBuf::from).find(|p| p.is_file()),
            abi: profile_dir.join("abi/4.0").exists(),
            profile_dir,
            ferry_files,
            foreign_files,
            cargo_copy,
        };
        let target = facts.target();
        let file = facts.profile_path(&target);
        if file.exists() && !facts.ferry_files.iter().any(|f| f.path == file) {
            facts.foreign_files.push(file);
        }
        // `target` can be `exe` with `exe_problem` set, e.g. a user-owned
        // file in a root-owned directory; then it still has to be replaced.
        if facts.exe_problem.is_none() {
            facts.target_userns = Some(facts.caps.userns.clone());
        } else if holds_binary(&target, Path::new(SELF_EXE)) {
            facts.installed = true;
            facts.target_userns = Some(verify(&target, &home).userns);
        }
        Ok(facts)
    }

    /// The binary the profile names: `exe` where it is if it can be
    /// profiled there, otherwise a copy in `bin_dir`.
    pub fn target(&self) -> PathBuf {
        if self.exe_problem.is_none() {
            self.exe.clone()
        } else {
            self.bin_dir.join("ferry")
        }
    }

    fn profile_path(&self, target: &Path) -> PathBuf {
        self.profile_dir.join(doctor::apparmor_file_name(target))
    }
}

/// What to do, or why nothing runs as root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision<P> {
    Stop { outcome: Outcome, reason: String },
    Run(P),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    NotApplicable,
    NotNeeded,
    AlreadyDone,
    Refused,
    Planned,
    Done,
    Failed,
}

fn stop<P>(outcome: Outcome, reason: impl Into<String>) -> Decision<P> {
    Decision::Stop {
        outcome,
        reason: reason.into(),
    }
}

const ROOT_REFUSAL: &str = "run this as your user, not as root: Ferry asks sudo itself, and \
                            root is never restricted, so a root run could verify nothing";

/// The root work [`decide`] settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// The running binary.
    pub exe: PathBuf,
    /// The binary the profile names.
    pub target: PathBuf,
    /// Whether root copies the running binary to `target` first.
    pub install: bool,
    pub profile_path: PathBuf,
    pub profile: String,
    /// Older Ferry profile files to remove.
    pub remove: Vec<PathBuf>,
    pub parser: PathBuf,
    pub warnings: Vec<String>,
}

impl InstallPlan {
    /// One line per thing root does.
    pub fn steps(&self) -> Vec<String> {
        let mut steps = Vec::new();
        if self.install {
            steps.push(format!(
                "install this ferry ({}) as {}, owned by root, mode 0755: the profile attaches \
                 by path, so it must name a file only root can replace",
                self.exe.display(),
                self.target.display()
            ));
        }
        steps.push(format!(
            "write {} (shown below) and check that it parses",
            self.profile_path.display()
        ));
        for old in &self.remove {
            steps.push(format!("remove the older Ferry profile {}", old.display()));
        }
        steps.push(format!(
            "load it with {} -r; AppArmor loads it again at every boot",
            self.parser.display()
        ));
        steps
    }

    /// The `sudo` command; the profile goes on its stdin. `-n` makes a
    /// needed password fail at once instead of waiting for a prompt nobody
    /// answers.
    pub fn sudo_argv(&self, pid: u32, non_interactive: bool) -> Vec<String> {
        let mut argv = sudo_prefix(non_interactive, ROOT_SCRIPT);
        argv.push(pid.to_string());
        argv.push(if self.install {
            lossy(&self.target)
        } else {
            String::new()
        });
        argv.push(lossy(&self.parser));
        argv.push(lossy(&self.profile_path));
        argv.extend(self.remove.iter().map(|p| lossy(p)));
        argv
    }
}

/// The root work [`decide_uninstall`] settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallPlan {
    pub files: Vec<PathBuf>,
    pub parser: PathBuf,
    /// Binaries the removed profiles name; they stay in place.
    pub binaries: Vec<PathBuf>,
}

impl UninstallPlan {
    pub fn steps(&self) -> Vec<String> {
        self.files
            .iter()
            .map(|f| format!("unload and remove {}", f.display()))
            .collect()
    }

    pub fn sudo_argv(&self, non_interactive: bool) -> Vec<String> {
        let mut argv = sudo_prefix(non_interactive, UNINSTALL_SCRIPT);
        argv.push(lossy(&self.parser));
        argv.extend(self.files.iter().map(|p| lossy(p)));
        argv
    }
}

fn sudo_prefix(non_interactive: bool, script: &str) -> Vec<String> {
    let mut argv = vec!["sudo".to_owned()];
    if non_interactive {
        argv.push("-n".into());
    }
    argv.extend(["--", "/bin/sh", "-euc", script, "ferry-setup"].map(String::from));
    argv
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// What `ferry setup apparmor` should do on the machine `f` describes.
pub fn decide(f: &Facts) -> Decision<InstallPlan> {
    if f.os != Os::Linux {
        return stop(
            Outcome::NotApplicable,
            "only Linux has AppArmor and user namespaces",
        );
    }
    if f.euid == 0 {
        return stop(Outcome::Refused, ROOT_REFUSAL);
    }
    let restrict = f.caps.sysctl(RESTRICT);
    let covered = f
        .ferry_files
        .iter()
        .any(|p| p.binary().as_deref() == Some(f.exe.as_path()));
    if f.caps.userns.is_ok() && !(restrict == Some("1") && covered) {
        return stop(
            Outcome::NotNeeded,
            "this ferry binary can already create user namespaces",
        );
    }
    match restrict {
        None => {
            let other = match &f.caps.userns {
                Probe::Ok => String::new(),
                failed => format!(
                    "; namespaces fail for another reason ({}): `ferry doctor` shows the fix",
                    doctor::probe_detail(failed)
                ),
            };
            return stop(
                Outcome::NotNeeded,
                format!(
                    "this kernel has no AppArmor user-namespace restriction (an Ubuntu kernel \
                     patch){other}"
                ),
            );
        }
        Some("1") => {}
        Some(value) => {
            return stop(
                Outcome::NotNeeded,
                format!(
                    "the restriction is off ({RESTRICT} = {value}); run this again if it is \
                     turned on"
                ),
            );
        }
    }
    if !f.initial_userns {
        return stop(
            Outcome::Refused,
            "run this from a shell outside Ferry's sandbox and outside containers: sudo \
             cannot become root in a nested user namespace",
        );
    }
    if let Some(marker) = &f.container {
        return stop(
            Outcome::Refused,
            format!(
                "this runs in a container ({}), and only the host can load AppArmor policy: \
                 see the troubleshooting guide (Containers) for what to change on the host",
                marker.display()
            ),
        );
    }
    let (Some(parser), true) = (&f.parser, f.abi) else {
        return stop(
            Outcome::Refused,
            format!(
                "install the apparmor package (AppArmor 4.0 or newer userspace): Ferry needs \
                 apparmor_parser in /usr/sbin or /sbin and {}",
                f.profile_dir.join("abi/4.0").display()
            ),
        );
    };
    if let Some(file) = f.foreign_files.first() {
        return stop(
            Outcome::Refused,
            format!(
                "{} is not Ferry's, but it defines an AppArmor profile named ferry or \
                 unpriv_ferry, or has the name Ferry's profile needs; loading Ferry's would \
                 replace it, so rename or remove it first",
                file.display()
            ),
        );
    }
    if f.exe_problem.is_some()
        && let Some(problem) = &f.bin_dir_problem
    {
        return stop(Outcome::Refused, problem.clone());
    }
    install_plan(f, parser)
}

/// The steps still missing, each listed only when needed, so a re-run on a
/// machine that is set up does nothing.
fn install_plan(f: &Facts, parser: &Path) -> Decision<InstallPlan> {
    let target = f.target();
    let install = f.exe_problem.is_some() && !f.installed;
    let profile_path = f.profile_path(&target);
    let profile = doctor::apparmor_profile(&target);
    let remove: Vec<PathBuf> = f
        .ferry_files
        .iter()
        .filter(|p| p.path != profile_path)
        .map(|p| p.path.clone())
        .collect();
    let current = f
        .ferry_files
        .iter()
        .any(|p| p.path == profile_path && p.text == profile);
    // Afterwards the target holds this binary's bytes, so a different cargo
    // copy is a build the profile does not cover.
    let upgrade = f.cargo_copy.as_ref().map(|copy| {
        format!(
            "{0} is a different build from this ferry ({1}); if it is an upgrade, run \
             `{0} setup apparmor` to set it up",
            copy.display(),
            f.exe.display()
        )
    });
    if !install
        && current
        && remove.is_empty()
        && f.target_userns.as_ref().is_some_and(Probe::is_ok)
    {
        let done = if target == f.exe {
            format!(
                "{} has its profile and can create user namespaces",
                target.display()
            )
        } else {
            format!(
                "{} is installed with its profile and can create user namespaces; the \
                 profile does not cover {}, which you ran: run {0} instead",
                target.display(),
                f.exe.display()
            )
        };
        return stop(
            Outcome::AlreadyDone,
            match upgrade {
                Some(upgrade) => format!("{done}; {upgrade}"),
                None => done,
            },
        );
    }
    let mut warnings: Vec<String> = upgrade.into_iter().collect();
    if target.starts_with("/nix/store") {
        warnings.push(format!(
            "the profile names {}, a Nix store path that changes with every upgrade: run \
             `ferry setup apparmor` again after each one",
            target.display()
        ));
    }
    Decision::Run(InstallPlan {
        exe: f.exe.clone(),
        target,
        install,
        profile_path,
        profile,
        remove,
        parser: parser.to_path_buf(),
        warnings,
    })
}

/// What `ferry setup apparmor --uninstall` should do.
pub fn decide_uninstall(f: &Facts) -> Decision<UninstallPlan> {
    if f.os != Os::Linux {
        return stop(
            Outcome::NotApplicable,
            "only Linux has AppArmor and user namespaces",
        );
    }
    if f.euid == 0 {
        return stop(Outcome::Refused, ROOT_REFUSAL);
    }
    if f.ferry_files.is_empty() {
        return stop(
            Outcome::NotNeeded,
            format!(
                "nothing to remove (no Ferry profile in {})",
                f.profile_dir.display()
            ),
        );
    }
    if !f.initial_userns {
        return stop(
            Outcome::Refused,
            "run this from a shell outside Ferry's sandbox and outside containers: sudo \
             cannot become root in a nested user namespace",
        );
    }
    let Some(parser) = &f.parser else {
        return stop(
            Outcome::Refused,
            "apparmor_parser is not in /usr/sbin or /sbin, so the profile cannot be unloaded",
        );
    };
    let mut binaries: Vec<PathBuf> = f
        .ferry_files
        .iter()
        .filter_map(ProfileFile::binary)
        .collect();
    binaries.dedup();
    Decision::Run(UninstallPlan {
        files: f.ferry_files.iter().map(|p| p.path.clone()).collect(),
        parser: parser.clone(),
        binaries,
    })
}

/// True for `y` or `yes` (any case); anything else, including EOF or an
/// unreadable line, declines.
pub fn confirm(mut input: impl BufRead) -> bool {
    let mut line = String::new();
    input.read_line(&mut line).is_ok()
        && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// What `binary __detect` reported, from the profiled binary itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verification {
    pub userns: Probe,
    pub overlay: Probe,
    pub label: Option<String>,
}

/// Runs `binary __detect dir`, so the probe takes the same exec path from
/// `binary` that `ferry base` and `ferry spawn` take.
pub fn verify(binary: &Path, dir: &Path) -> Verification {
    let detected = Command::new(binary)
        .arg("__detect")
        .arg(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("{}: {e}", binary.display()))
        .and_then(|out| {
            serde_json::from_slice::<Capabilities>(&out.stdout).map_err(|_| {
                let stderr = String::from_utf8_lossy(&out.stderr);
                format!(
                    "`{} __detect` {} without a result{}",
                    binary.display(),
                    out.status,
                    stderr
                        .lines()
                        .next()
                        .map(|l| format!(": {l}"))
                        .unwrap_or_default()
                )
            })
        });
    match detected {
        Ok(caps) => Verification {
            userns: caps.userns,
            overlay: caps.overlay,
            label: caps.probe_label,
        },
        Err(error) => {
            let failed = Probe::Failed {
                stage: "probe process".into(),
                error,
            };
            Verification {
                userns: failed.clone(),
                overlay: failed,
                label: None,
            }
        }
    }
}

/// What a probe label after a failed verify says about the cause.
pub fn label_hint(label: Option<&str>) -> &'static str {
    match label {
        Some(l) if l.contains("unpriv_ferry") => {
            "the probe ran under unpriv_ferry, so the profile's rule that keeps ferry \
             re-running itself as `ferry` did not take effect; please report this"
        }
        _ => {
            "the profile did not attach: check that the binary's path is the one the \
             profile names and that AppArmor is enabled at boot"
        }
    }
}

/// The marker file that shows this process runs in a container, if any.
pub fn container_marker() -> Option<PathBuf> {
    CONTAINER_MARKERS
        .iter()
        .map(Path::new)
        .find(|p| p.exists())
        .map(Path::to_path_buf)
}

/// The binaries Ferry's profiles in `dir` name.
pub fn profiled_binaries(dir: &Path) -> Vec<PathBuf> {
    scan(dir).0.iter().filter_map(ProfileFile::binary).collect()
}

/// Ferry's profile files in `dir`, and the other files there that define a
/// profile named `ferry` or `unpriv_ferry`.
pub fn scan(dir: &Path) -> (Vec<ProfileFile>, Vec<PathBuf>) {
    let (mut ferry, mut foreign) = (Vec::new(), Vec::new());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (ferry, foreign);
    };
    for entry in entries.flatten() {
        // AppArmor skips dot files when it loads the directory.
        let visible = entry
            .file_name()
            .to_str()
            .is_some_and(|n| !n.starts_with('.'));
        if !visible || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let first = text.lines().next().unwrap_or_default();
        if first.starts_with(doctor::PROFILE_MARKER)
            || (first.starts_with(LEGACY_MARKER) && text.contains("profile unpriv_ferry "))
        {
            ferry.push(ProfileFile { path, text });
        } else if defines_ferry_profile(&text) {
            foreign.push(path);
        }
    }
    ferry.sort_by(|a, b| a.path.cmp(&b.path));
    foreign.sort();
    (ferry, foreign)
}

fn defines_ferry_profile(text: &str) -> bool {
    text.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next() == Some("profile")
            && words
                .next()
                .is_some_and(|n| matches!(n.trim_end_matches('{'), "ferry" | "unpriv_ferry"))
    })
}

/// `dir` resolved as far as it exists, and why it cannot hold the profiled
/// binary, if it cannot.
fn check_bin_dir(dir: &Path) -> (PathBuf, Option<String>) {
    if !dir.is_absolute() || dir.components().any(|c| c == Component::ParentDir) {
        return (
            dir.to_path_buf(),
            Some(format!(
                "--bin-dir {} must be an absolute path without `..`",
                dir.display()
            )),
        );
    }
    let existing = dir
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(Path::new("/"));
    let rest = dir.strip_prefix(existing).unwrap_or(Path::new(""));
    let canonical = std::fs::canonicalize(existing).unwrap_or_else(|_| existing.to_path_buf());
    let resolved = canonical.join(rest);
    let problem = doctor::user_writable_ancestor(&canonical)
        .map(|w| {
            format!(
                "ferry has to be installed to a root-owned path, and {} is writable by a \
                 non-root user, so the binary the profile names could be replaced; pick a \
                 root-owned directory, e.g. `--bin-dir /opt/ferry/bin`",
                w.display()
            )
        })
        .or_else(|| doctor::apparmor_path_unusable(&resolved.join("ferry")));
    (resolved, problem)
}

/// The running binary's canonical path.
pub fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().at("locate", "the ferry binary")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// Whether `target` is an installed copy of `binary` that a profile can
/// name: a regular file, not a symlink (`AppArmor` matches the resolved
/// path), that only root can replace, with `binary`'s bytes.
pub fn holds_binary(target: &Path, binary: &Path) -> bool {
    std::fs::symlink_metadata(target).is_ok_and(|m| m.file_type().is_file())
        && doctor::apparmor_path_problem(target).is_none()
        && same_bytes(target, binary)
}

/// Whether both files can be read and have the same bytes.
pub fn same_bytes(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The first `name` on `PATH`, canonicalized.
pub fn first_on_path(name: impl AsRef<OsStr>) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name.as_ref()))
        .find(|p| p.is_file())
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::platform::{FsInfo, Sysctl};

    const USER_EXE: &str = "/home/you/.cargo/bin/ferry";
    /// The start of what `ferry doctor --apparmor-profile` printed before
    /// `ferry setup apparmor` existed.
    const LEGACY_PROFILE: &str = "# AppArmor profile: lets /usr/local/bin/ferry create \
        unprivileged user namespaces.\n\
        # Install: sudo cp this-file /etc/apparmor.d/ferry && sudo apparmor_parser -r /etc/apparmor.d/ferry\n\
        abi <abi/4.0>,\n\
        profile ferry /usr/local/bin/ferry flags=(attach_disconnected) {\n}\n\
        profile unpriv_ferry flags=(attach_disconnected) {\n}\n";

    fn failed(stage: &str) -> Probe {
        Probe::Failed {
            stage: stage.into(),
            error: "Permission denied".into(),
        }
    }

    /// Ubuntu 24.04 with the restriction on, no profile, and ferry in
    /// `~/.cargo/bin`.
    fn ubuntu() -> Facts {
        Facts {
            os: Os::Linux,
            euid: 1000,
            initial_userns: true,
            container: None,
            caps: Capabilities {
                os: Os::Linux,
                kernel: "6.8.0".into(),
                store_fs: FsInfo {
                    name: "ext4".into(),
                },
                reflink: failed("FICLONE"),
                userns: failed("setgroups"),
                overlay: failed("setgroups"),
                sysctls: vec![Sysctl {
                    name: RESTRICT.into(),
                    value: "1".into(),
                }],
                probe_label: Some("unprivileged_userns".into()),
            },
            exe: USER_EXE.into(),
            exe_problem: Some("/home/you is writable".into()),
            bin_dir: DEFAULT_BIN_DIR.into(),
            bin_dir_problem: None,
            installed: false,
            target_userns: None,
            parser: Some("/usr/sbin/apparmor_parser".into()),
            abi: true,
            profile_dir: PROFILE_DIR.into(),
            ferry_files: Vec::new(),
            foreign_files: Vec::new(),
            cargo_copy: None,
        }
    }

    fn ferry_file(name: &str, binary: &str) -> ProfileFile {
        ProfileFile {
            path: Path::new(PROFILE_DIR).join(name),
            text: doctor::apparmor_profile(Path::new(binary)),
        }
    }

    fn legacy_file() -> ProfileFile {
        ProfileFile {
            path: Path::new(PROFILE_DIR).join("ferry"),
            text: LEGACY_PROFILE.into(),
        }
    }

    #[track_caller]
    fn stopped(decision: Decision<InstallPlan>) -> (Outcome, String) {
        match decision {
            Decision::Stop { outcome, reason } => (outcome, reason),
            Decision::Run(plan) => panic!("expected no root work, got {plan:?}"),
        }
    }

    #[track_caller]
    fn plan(decision: Decision<InstallPlan>) -> InstallPlan {
        match decision {
            Decision::Run(plan) => plan,
            Decision::Stop { outcome, reason } => panic!("{outcome:?}: {reason}"),
        }
    }

    #[test]
    fn decide_is_not_applicable_on_macos() {
        let mac = Facts {
            os: Os::Macos,
            ..ubuntu()
        };
        assert_eq!(stopped(decide(&mac)).0, Outcome::NotApplicable);
        assert!(matches!(
            decide_uninstall(&mac),
            Decision::Stop {
                outcome: Outcome::NotApplicable,
                ..
            }
        ));
    }

    #[test]
    fn decide_refuses_root() {
        let root = Facts {
            euid: 0,
            ..ubuntu()
        };
        let (outcome, reason) = stopped(decide(&root));
        assert_eq!(outcome, Outcome::Refused);
        assert!(reason.contains("not as root"), "{reason}");
    }

    #[test]
    fn decide_not_needed_without_sysctl() {
        let mut f = ubuntu();
        f.caps.sysctls.clear();
        let (outcome, reason) = stopped(decide(&f));
        assert_eq!(outcome, Outcome::NotNeeded);
        assert!(
            reason.contains("no AppArmor user-namespace restriction"),
            "{reason}"
        );
        assert!(reason.contains("setgroups failed"), "{reason}");
        assert!(reason.contains("ferry doctor"), "{reason}");

        f.caps.userns = Probe::Ok;
        let (outcome, reason) = stopped(decide(&f));
        assert_eq!(outcome, Outcome::NotNeeded);
        assert!(!reason.contains("doctor"), "{reason}");
    }

    #[test]
    fn decide_not_needed_when_sysctl_is_zero() {
        let mut f = ubuntu();
        f.caps.sysctls[0].value = "0".into();
        let (outcome, reason) = stopped(decide(&f));
        assert_eq!(outcome, Outcome::NotNeeded);
        assert!(reason.contains("= 0"), "{reason}");
    }

    #[test]
    fn decide_not_needed_when_probe_passes_without_a_ferry_profile() {
        let mut f = ubuntu();
        f.caps.userns = Probe::Ok;
        let (outcome, reason) = stopped(decide(&f));
        assert_eq!(outcome, Outcome::NotNeeded);
        assert!(reason.contains("already create"), "{reason}");
    }

    #[test]
    fn decide_already_done_when_probe_passes() {
        let mut f = ubuntu();
        f.exe = "/usr/local/bin/ferry".into();
        f.exe_problem = None;
        f.caps.userns = Probe::Ok;
        f.target_userns = Some(Probe::Ok);
        f.ferry_files = vec![ferry_file("usr.local.bin.ferry", "/usr/local/bin/ferry")];
        let (outcome, reason) = stopped(decide(&f));
        assert_eq!(outcome, Outcome::AlreadyDone, "{reason}");

        // A profile from before `setup` works but is migrated.
        f.ferry_files = vec![legacy_file()];
        let p = plan(decide(&f));
        assert!(!p.install);
        assert_eq!(
            p.profile_path,
            Path::new("/etc/apparmor.d/usr.local.bin.ferry")
        );
        assert_eq!(p.remove, vec![PathBuf::from("/etc/apparmor.d/ferry")]);
    }

    #[test]
    fn decide_refuses_nested_userns() {
        let f = Facts {
            initial_userns: false,
            ..ubuntu()
        };
        let (outcome, reason) = stopped(decide(&f));
        assert_eq!(outcome, Outcome::Refused);
        assert!(reason.contains("outside Ferry's sandbox"), "{reason}");
    }

    #[test]
    fn decide_refuses_in_container() {
        let f = Facts {
            container: Some("/.dockerenv".into()),
            ..ubuntu()
        };
        let (outcome, reason) = stopped(decide(&f));
        assert_eq!(outcome, Outcome::Refused);
        assert!(reason.contains("/.dockerenv"), "{reason}");
        assert!(reason.contains("Containers"), "{reason}");
    }

    #[test]
    fn decide_refuses_without_parser_or_abi() {
        for f in [
            Facts {
                parser: None,
                ..ubuntu()
            },
            Facts {
                abi: false,
                ..ubuntu()
            },
        ] {
            let (outcome, reason) = stopped(decide(&f));
            assert_eq!(outcome, Outcome::Refused);
            assert!(reason.contains("apparmor package"), "{reason}");
        }
    }

    #[test]
    fn decide_refuses_foreign_ferry_profile() {
        let f = Facts {
            foreign_files: vec!["/etc/apparmor.d/ferry-ide".into()],
            ..ubuntu()
        };
        let (outcome, reason) = stopped(decide(&f));
        assert_eq!(outcome, Outcome::Refused);
        assert!(reason.contains("/etc/apparmor.d/ferry-ide"), "{reason}");
    }

    #[test]
    fn plan_installs_binary_from_user_writable_path() {
        let p = plan(decide(&ubuntu()));
        assert!(p.install);
        assert_eq!(p.target, Path::new("/usr/local/bin/ferry"));
        assert_eq!(
            p.profile_path,
            Path::new("/etc/apparmor.d/usr.local.bin.ferry")
        );
        assert_eq!(p.profile, doctor::apparmor_profile(&p.target));
        let steps = p.steps().join("\n");
        assert!(steps.contains(USER_EXE), "{steps}");
        assert!(steps.contains("-r"), "{steps}");
        assert_eq!(p.warnings, Vec::<String>::new());
    }

    #[test]
    fn plan_replaces_user_owned_binary_in_root_owned_dir() {
        // After `sudo cp -p ~/.cargo/bin/ferry /usr/local/bin/`, the
        // directory is root's, the file is the user's, and it is the binary
        // that runs.
        let mut f = ubuntu();
        f.exe = "/usr/local/bin/ferry".into();
        f.exe_problem = Some("/usr/local/bin/ferry is writable".into());
        let p = plan(decide(&f));
        assert!(p.install, "{p:?}");
        assert_eq!(p.target, f.exe);
        assert_eq!(p.sudo_argv(42, false)[7], "/usr/local/bin/ferry");

        // A profile an earlier run loaded for it works, but the file is
        // still the user's.
        f.caps.userns = Probe::Ok;
        f.ferry_files = vec![ferry_file("usr.local.bin.ferry", "/usr/local/bin/ferry")];
        let p = plan(decide(&f));
        assert!(p.install, "{p:?}");
        assert_eq!(p.remove, Vec::<PathBuf>::new());
    }

    // SELF_EXE is a /proc path.
    #[cfg(target_os = "linux")]
    #[test]
    fn user_owned_copy_is_not_installed() {
        if rustix::process::geteuid().as_raw() == 0 {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let copy = tmp.path().join("ferry");
        std::fs::copy(SELF_EXE, &copy).unwrap();
        assert!(same_bytes(&copy, Path::new(SELF_EXE)));
        assert!(!holds_binary(&copy, Path::new(SELF_EXE)));
    }

    #[cfg(unix)]
    /// `AppArmor` attaches by the resolved path, so a profile for a symlink
    /// never attaches: root has to replace the link with the file.
    #[test]
    fn symlink_is_not_installed() {
        // A root-owned symlink to a root-owned binary, such as /bin/sh on
        // NixOS, Debian or Fedora.
        let Some((link, file)) = ["/bin/sh", "/usr/bin/sh", "/usr/bin/env", "/bin/bash"]
            .iter()
            .map(Path::new)
            .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_symlink()))
            .filter_map(|p| Some((p, std::fs::canonicalize(p).ok()?)))
            .find(|(link, file)| {
                doctor::apparmor_path_problem(link).is_none()
                    && doctor::apparmor_path_problem(file).is_none()
            })
        else {
            eprintln!("skipping: no root-owned symlink to a root-owned binary here");
            return;
        };
        assert!(holds_binary(&file, &file), "{}", file.display());
        assert!(!holds_binary(link, &file), "{}", link.display());
    }

    #[test]
    fn plan_profiles_root_owned_exe_in_place() {
        let mut f = ubuntu();
        f.exe = "/nix/store/abc-ferry-0.1.0/bin/ferry".into();
        f.exe_problem = None;
        f.target_userns = Some(failed("setgroups"));
        let p = plan(decide(&f));
        assert!(!p.install);
        assert_eq!(p.target, f.exe);
        assert_eq!(
            p.profile_path,
            Path::new("/etc/apparmor.d/nix.store.abc-ferry-0.1.0.bin.ferry")
        );
        assert!(p.warnings[0].contains("every upgrade"), "{:?}", p.warnings);
    }

    #[cfg(unix)]
    #[test]
    fn plan_refuses_user_writable_bin_dir_and_suggests_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, problem) = check_bin_dir(&tmp.path().join("ferry-bin"));
        let problem = problem.unwrap();
        assert!(problem.contains("--bin-dir /opt/ferry/bin"), "{problem}");
        assert!(dir.ends_with("ferry-bin"));
        let f = Facts {
            bin_dir_problem: Some(problem.clone()),
            ..ubuntu()
        };
        assert_eq!(stopped(decide(&f)), (Outcome::Refused, problem));

        assert!(check_bin_dir(Path::new("bin")).1.is_some());
        assert!(check_bin_dir(Path::new("/usr/../tmp")).1.is_some());
        if rustix::process::geteuid().as_raw() != 0 && Path::new("/usr").is_dir() {
            let (dir, problem) = check_bin_dir(Path::new("/usr/ferry-test-missing/bin"));
            assert_eq!(problem, None);
            assert!(dir.ends_with("ferry-test-missing/bin"), "{}", dir.display());
        }
    }

    #[test]
    fn plan_removes_legacy_and_marked_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, text: &str| std::fs::write(dir.path().join(name), text).unwrap();
        write(
            "usr.local.bin.ferry",
            &doctor::apparmor_profile(Path::new("/usr/local/bin/ferry")),
        );
        write("ferry", LEGACY_PROFILE);
        write("ferry-ide", "profile ferry /opt/ferry-ide/ferry {\n}\n");
        write("bwrap", "profile bwrap /usr/bin/bwrap {\n}\n");
        write(
            ".usr.local.bin.ferry.new.42",
            &doctor::apparmor_profile(Path::new("/usr/local/bin/ferry")),
        );
        std::fs::create_dir(dir.path().join("local")).unwrap();
        write("local/ferry", "# Ferry: lets nothing\n");

        let (ferry, foreign) = scan(dir.path());
        let names: Vec<_> = ferry.iter().map(|p| p.path.file_name().unwrap()).collect();
        assert_eq!(names, ["ferry", "usr.local.bin.ferry"]);
        assert_eq!(foreign, vec![dir.path().join("ferry-ide")]);
        assert_eq!(
            profiled_binaries(dir.path()),
            vec![
                PathBuf::from("/usr/local/bin/ferry"),
                PathBuf::from("/usr/local/bin/ferry")
            ]
        );

        let f = Facts {
            profile_dir: dir.path().into(),
            ferry_files: ferry,
            ..ubuntu()
        };
        let p = plan(decide(&f));
        assert_eq!(p.remove, vec![dir.path().join("ferry")]);
    }

    #[test]
    fn plan_is_empty_on_rerun_with_identical_bytes_and_profile() {
        let mut f = ubuntu();
        f.installed = true;
        f.target_userns = Some(Probe::Ok);
        f.ferry_files = vec![ferry_file("usr.local.bin.ferry", "/usr/local/bin/ferry")];
        let (outcome, reason) = stopped(decide(&f));
        assert_eq!(outcome, Outcome::AlreadyDone);
        assert!(
            reason.contains("run /usr/local/bin/ferry instead"),
            "{reason}"
        );

        // An upgraded binary is installed again; the profile stays.
        f.installed = false;
        f.target_userns = None;
        let p = plan(decide(&f));
        assert!(p.install);
        assert_eq!(p.remove, Vec::<PathBuf>::new());

        // Installed and current, but not loaded: load it again.
        f.installed = true;
        f.target_userns = Some(failed("setgroups"));
        let p = plan(decide(&f));
        assert!(!p.install);
    }

    #[test]
    fn rerun_from_installed_copy_points_at_upgraded_cargo_copy() {
        // `cargo install` upgraded ~/.cargo/bin/ferry, but /usr/local/bin
        // comes first on PATH, so a bare `ferry` runs the old copy.
        let mut f = ubuntu();
        f.exe = "/usr/local/bin/ferry".into();
        f.exe_problem = None;
        f.caps.userns = Probe::Ok;
        f.target_userns = Some(Probe::Ok);
        f.ferry_files = vec![ferry_file("usr.local.bin.ferry", "/usr/local/bin/ferry")];
        f.cargo_copy = Some(USER_EXE.into());
        let hint = format!("run `{USER_EXE} setup apparmor`");
        let (outcome, reason) = stopped(decide(&f));
        assert_eq!(outcome, Outcome::AlreadyDone);
        assert!(reason.contains(&hint), "{reason}");

        f.target_userns = Some(failed("setgroups"));
        let p = plan(decide(&f));
        assert!(p.warnings.iter().any(|w| w.contains(&hint)), "{p:?}");

        f.cargo_copy = None;
        assert_eq!(plan(decide(&f)).warnings, Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn scripts_are_valid_sh() {
        for script in [ROOT_SCRIPT, UNINSTALL_SCRIPT] {
            let status = Command::new("/bin/sh")
                .args(["-n", "-c", script])
                .status()
                .unwrap();
            assert!(status.success(), "{script}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn sudo_argv_carries_paths_as_arguments_and_n_only_when_non_interactive() {
        for script in [ROOT_SCRIPT, UNINSTALL_SCRIPT] {
            assert!(!script.contains("/usr/local"), "{script}");
            assert!(!script.contains("/etc/apparmor.d"), "{script}");
        }
        let mut f = ubuntu();
        f.ferry_files = vec![legacy_file()];
        let p = plan(decide(&f));
        let argv = p.sudo_argv(42, false);
        assert_eq!(
            argv,
            [
                "sudo",
                "--",
                "/bin/sh",
                "-euc",
                ROOT_SCRIPT,
                "ferry-setup",
                "42",
                "/usr/local/bin/ferry",
                "/usr/sbin/apparmor_parser",
                "/etc/apparmor.d/usr.local.bin.ferry",
                "/etc/apparmor.d/ferry",
            ]
        );
        assert_eq!(p.sudo_argv(42, true)[..3], ["sudo", "-n", "--"]);

        f.installed = true;
        assert_eq!(plan(decide(&f)).sudo_argv(42, false)[7], "");

        let Decision::Run(u) = decide_uninstall(&f) else {
            panic!()
        };
        assert_eq!(
            u.sudo_argv(true),
            [
                "sudo",
                "-n",
                "--",
                "/bin/sh",
                "-euc",
                UNINSTALL_SCRIPT,
                "ferry-setup",
                "/usr/sbin/apparmor_parser",
                "/etc/apparmor.d/ferry",
            ]
        );
        assert_eq!(u.binaries, vec![PathBuf::from("/usr/local/bin/ferry")]);
    }

    #[test]
    fn decide_uninstall_without_profiles_does_nothing() {
        let (outcome, reason) = match decide_uninstall(&ubuntu()) {
            Decision::Stop { outcome, reason } => (outcome, reason),
            Decision::Run(p) => panic!("{p:?}"),
        };
        assert_eq!(outcome, Outcome::NotNeeded);
        assert!(reason.contains("nothing to remove"), "{reason}");
        let root = Facts {
            euid: 0,
            ferry_files: vec![legacy_file()],
            ..ubuntu()
        };
        assert!(matches!(
            decide_uninstall(&root),
            Decision::Stop {
                outcome: Outcome::Refused,
                ..
            }
        ));
    }

    #[test]
    fn confirm_accepts_only_y_or_yes() {
        for (input, expected) in [
            ("", false),
            ("\n", false),
            ("n\n", false),
            ("yess\n", false),
            ("y\n", true),
            ("yes\n", true),
            ("Y\n", true),
            (" yes \n", true),
        ] {
            assert_eq!(confirm(input.as_bytes()), expected, "{input:?}");
        }
    }

    #[test]
    fn label_hints_name_the_cause() {
        assert!(label_hint(Some("ferry//&unpriv_ferry (enforce)")).contains("report"));
        assert!(label_hint(Some("unprivileged_userns")).contains("did not attach"));
        assert!(label_hint(None).contains("did not attach"));
    }

    /// What a sudo command runs as root: everything after `--`.
    #[cfg(target_os = "linux")]
    fn root_command(argv: &[String]) -> &[String] {
        &argv[argv.iter().position(|a| a == "--").unwrap() + 1..]
    }

    /// Runs `args` as root of a new user namespace, so the root scripts'
    /// chown and install steps work on files we own.
    #[cfg(target_os = "linux")]
    fn run_as_fake_root(args: &[String], stdin: &[u8]) -> Option<std::process::Output> {
        use std::io::Write as _;
        let probe = Command::new("unshare").args(["-r", "true"]).status();
        if !probe.is_ok_and(|s| s.success()) {
            let required = std::env::var("FERRY_TEST_REQUIRE")
                .is_ok_and(|v| v.split(',').any(|k| k.trim() == "userns"));
            assert!(!required, "userns required but `unshare -r` fails");
            eprintln!("skipping: `unshare -r` does not work here");
            return None;
        }
        let mut child = Command::new("unshare")
            .arg("-r")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        Some(child.wait_with_output().unwrap())
    }

    /// A stand-in for `apparmor_parser` that logs its arguments.
    #[cfg(target_os = "linux")]
    fn fake_parser(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let parser = dir.join("apparmor_parser");
        std::fs::write(
            &parser,
            format!(
                "#!/bin/sh\necho \"$*\" >> {}\nfor f; do :; done\ntest -f \"$f\"\n",
                dir.join("parser.log").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&parser, std::fs::Permissions::from_mode(0o755)).unwrap();
        parser
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn root_script_installs_writes_checks_and_loads() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let profiles = dir.join("apparmor.d");
        std::fs::create_dir(&profiles).unwrap();
        std::fs::write(profiles.join("ferry"), LEGACY_PROFILE).unwrap();
        let (ferry_files, _) = scan(&profiles);
        let f = Facts {
            bin_dir: dir.join("opt/bin"),
            parser: Some(fake_parser(dir)),
            profile_dir: profiles.clone(),
            ferry_files,
            ..ubuntu()
        };
        let p = plan(decide(&f));
        // Root of a child namespace may not read our /proc/<pid>/exe, so
        // the script installs the shell that runs it: `$$` survives exec.
        let root = root_command(&p.sudo_argv(0, true)).to_vec();
        assert_eq!(root[..2], ["/bin/sh", "-euc"]);
        let mut args: Vec<String> = [
            "/bin/sh",
            "-c",
            r#"script=$1; shift 2; exec /bin/sh -euc "$script" ferry-setup "$$" "$@""#,
            "wrapper",
        ]
        .map(String::from)
        .into();
        args.push(root[2].clone());
        args.extend(root[4..].iter().cloned());
        let Some(out) = run_as_fake_root(&args, p.profile.as_bytes()) else {
            return;
        };
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        assert!(stderr.contains("ferry: loading"), "{stderr}");

        let installed = dir.join("opt/bin/ferry");
        assert!(same_bytes(
            &installed,
            &std::fs::canonicalize("/bin/sh").unwrap()
        ));
        assert_eq!(
            std::fs::metadata(&installed).unwrap().mode() & 0o7777,
            0o755
        );
        let profile = &p.profile_path;
        let name = doctor::apparmor_file_name(&installed);
        assert_eq!(profile, &profiles.join(&name));
        assert_eq!(std::fs::read_to_string(profile).unwrap(), p.profile);
        assert_eq!(std::fs::metadata(profile).unwrap().mode() & 0o7777, 0o644);
        let left: Vec<_> = std::fs::read_dir(&profiles)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [name.as_str()], "old and temporary files are gone");
        let log = std::fs::read_to_string(dir.join("parser.log")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "{log}");
        assert!(lines[0].starts_with("-Q -K "), "{log}");
        assert!(lines[0].contains(&format!("/.{name}.new.")), "{log}");
        assert_eq!(lines[1], format!("-r {}", profile.display()));

        // A symlink at the target is replaced by the file, not followed.
        let elsewhere = dir.join("elsewhere");
        std::fs::write(&elsewhere, b"old").unwrap();
        std::fs::remove_file(&installed).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &installed).unwrap();
        let out = run_as_fake_root(&args, p.profile.as_bytes()).unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(std::fs::symlink_metadata(&installed).unwrap().is_file());
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"old");

        // A failing parse check changes no profile.
        std::fs::write(dir.join("apparmor_parser"), "#!/bin/sh\nexit 1\n").unwrap();
        let out = run_as_fake_root(&args, b"broken").unwrap();
        assert!(!out.status.success());
        assert_eq!(std::fs::read_to_string(profile).unwrap(), p.profile);
        assert_eq!(std::fs::read_dir(&profiles).unwrap().count(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn uninstall_script_unloads_and_removes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let profiles = dir.join("apparmor.d");
        std::fs::create_dir(&profiles).unwrap();
        for (name, binary) in [
            ("ferry", "/usr/local/bin/ferry"),
            ("opt.bin.ferry", "/opt/bin/ferry"),
        ] {
            std::fs::write(
                profiles.join(name),
                doctor::apparmor_profile(Path::new(binary)),
            )
            .unwrap();
        }
        std::fs::write(
            profiles.join("bwrap"),
            "profile bwrap /usr/bin/bwrap {\n}\n",
        )
        .unwrap();
        let (ferry_files, _) = scan(&profiles);
        let f = Facts {
            parser: Some(fake_parser(dir)),
            profile_dir: profiles.clone(),
            ferry_files,
            ..ubuntu()
        };
        let Decision::Run(u) = decide_uninstall(&f) else {
            panic!()
        };
        assert_eq!(
            u.binaries,
            [
                PathBuf::from("/usr/local/bin/ferry"),
                PathBuf::from("/opt/bin/ferry")
            ]
        );
        let Some(out) = run_as_fake_root(root_command(&u.sudo_argv(true)), b"") else {
            return;
        };
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let left: Vec<_> = std::fs::read_dir(&profiles)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["bwrap"]);
        let log = std::fs::read_to_string(dir.join("parser.log")).unwrap();
        assert_eq!(
            log,
            format!(
                "-R {}\n-R {}\n",
                profiles.join("ferry").display(),
                profiles.join("opt.bin.ferry").display()
            )
        );
    }
}
