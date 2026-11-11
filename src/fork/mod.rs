//! Forking a base's target dir into an agent's private build state.
//!
//! INVARIANT: no backend ever writes to the base. Each backend gives the
//! agent state that is physically separate (reflink and copy create new
//! inodes; overlayfs keeps the base as a read-only lower layer).
//!
//! There is deliberately no hardlink backend: Cargo rewrites some files in
//! place (fingerprints are written with `File::create`, which truncates the
//! existing inode), so a hardlinked fork would let an agent's rebuild modify
//! the base.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::BackendPref;
use crate::platform::{Capabilities, Os, Probe};
use crate::sandbox::Isolation;

#[cfg(target_os = "macos")]
pub(crate) mod clonefile;
mod copy;
#[cfg(target_os = "linux")]
mod overlay;
#[cfg(target_os = "linux")]
pub(crate) mod reflink;
#[cfg(windows)]
mod refs;
pub(crate) mod tree;

#[derive(Debug, thiserror::Error)]
pub enum ForkError {
    #[error("the {backend} backend is not available here: {reason}")]
    Unavailable {
        backend: BackendKind,
        reason: String,
    },

    #[error("failed to {op} {}: {source}", path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl ForkError {
    pub fn hint(&self) -> Option<String> {
        match self {
            ForkError::Unavailable { .. } => Some(
                "run `ferry doctor`, or set FERRY_BACKEND=auto to pick a supported backend".into(),
            ),
            ForkError::Io { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Copy-on-write clone: `FICLONE` per file on Linux (btrfs, XFS, ...),
    /// `clonefile(2)` of the whole tree on macOS (APFS).
    Reflink,
    /// overlayfs with the base as lower layer and a per-agent upper layer.
    /// Linux only; mounted inside the agent's namespace sandbox.
    Overlay,
    /// Plain recursive copy preserving mtimes. Always available, costs a full
    /// copy of the target dir per agent.
    Copy,
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BackendKind::Reflink => "reflink",
            BackendKind::Overlay => "overlay",
            BackendKind::Copy => "copy",
        })
    }
}

/// Where an agent's build state lives on the host.
#[derive(Debug, Clone)]
pub struct ForkPaths {
    /// The base's target dir. Read-only for every backend.
    pub base_target: PathBuf,
    /// Agent target dir (reflink and copy backends).
    pub agent_target: PathBuf,
    /// overlayfs upper and work dirs (overlay backend).
    pub upper: PathBuf,
    pub work: PathBuf,
}

/// What the sandbox must present to the build as its target dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSpec {
    /// A plain directory, used directly or bind-mounted.
    Dir(PathBuf),
    /// An overlayfs mount; only possible inside a namespace sandbox.
    Overlay {
        lower: PathBuf,
        upper: PathBuf,
        work: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ForkStats {
    pub files: u64,
    pub bytes: u64,
    #[serde(rename = "elapsed_secs", with = "duration_secs")]
    pub elapsed: Duration,
}

pub trait Backend: std::fmt::Debug {
    /// Creates the agent's build state from `paths.base_target`. Must not
    /// modify anything under `base_target`, and must preserve mtimes, since
    /// Cargo's freshness check depends on them.
    fn fork(&self, paths: &ForkPaths) -> Result<ForkStats, ForkError>;

    /// The target dir the sandbox should present to builds.
    fn target_spec(&self, paths: &ForkPaths) -> TargetSpec;

    /// Bytes this agent's build state occupies beyond the base (best
    /// effort: for reflink clones this counts files rewritten since the
    /// fork, because shared extents are not visible through `stat`).
    fn private_bytes(&self, paths: &ForkPaths) -> Result<u64, ForkError>;
}

/// The backend implementation for `kind` on this platform.
pub fn backend(kind: BackendKind) -> Result<Box<dyn Backend>, ForkError> {
    match kind {
        BackendKind::Copy => Ok(Box::new(copy::Copy)),
        #[cfg(target_os = "linux")]
        BackendKind::Reflink => Ok(Box::new(reflink::Reflink)),
        #[cfg(target_os = "macos")]
        BackendKind::Reflink => Ok(Box::new(clonefile::Clonefile)),
        #[cfg(windows)]
        BackendKind::Reflink => Ok(Box::new(refs::Refs)),
        #[cfg(target_os = "linux")]
        BackendKind::Overlay => Ok(Box::new(overlay::Overlay)),
        #[allow(unreachable_patterns)]
        other => Err(ForkError::Unavailable {
            backend: other,
            reason: "not implemented on this platform".into(),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Choice {
    pub kind: BackendKind,
    /// Why this backend was chosen, for `ferry spawn` and `ferry doctor`.
    pub reason: String,
}

/// Chooses a backend. An explicit preference that cannot be honoured is an
/// error, never a silent fallback.
///
/// `auto` order: reflink when the store's filesystem supports copy-on-write
/// clones (a plain directory: no copy-up, no shared mount to coordinate, no
/// upper-filesystem sync when a sandbox exits; costs time per file at
/// spawn); otherwise overlay when the agent runs in a namespace sandbox and
/// overlayfs works there (O(1) fork; copy-ups are full copies without
/// clones); otherwise copy.
pub fn select(
    pref: BackendPref,
    caps: &Capabilities,
    isolation: Isolation,
) -> Result<Choice, ForkError> {
    let fs = &caps.store_fs.name;
    let reflink = match &caps.reflink {
        Probe::Ok => Ok(format!("{fs} supports copy-on-write clones")),
        other => Err(format!(
            "no copy-on-write clones on {fs} ({})",
            probe_reason(other)
        )),
    };
    let overlay = if caps.os != Os::Linux {
        Err("overlayfs is Linux-only".to_owned())
    } else if isolation != Isolation::Namespace {
        Err("overlayfs needs the namespace sandbox, which is not in use".to_owned())
    } else {
        match &caps.overlay {
            Probe::Ok => Ok(format!("overlayfs works in the namespace sandbox on {fs}")),
            other => Err(format!("overlayfs unavailable ({})", probe_reason(other))),
        }
    };
    let choose = |kind, result: Result<String, String>| match result {
        Ok(reason) => Ok(Choice { kind, reason }),
        Err(reason) => Err(ForkError::Unavailable {
            backend: kind,
            reason,
        }),
    };
    match pref {
        BackendPref::Reflink => choose(BackendKind::Reflink, reflink),
        BackendPref::Overlay => choose(BackendKind::Overlay, overlay),
        BackendPref::Copy => Ok(Choice {
            kind: BackendKind::Copy,
            reason: "requested by configuration".into(),
        }),
        BackendPref::Auto => match (reflink, overlay) {
            (Ok(reason), _) => Ok(Choice {
                kind: BackendKind::Reflink,
                reason,
            }),
            (Err(no_reflink), Ok(reason)) => Ok(Choice {
                kind: BackendKind::Overlay,
                reason: format!("{reason}; {no_reflink}"),
            }),
            (Err(no_reflink), Err(no_overlay)) => Ok(Choice {
                kind: BackendKind::Copy,
                reason: format!("full copy: {no_reflink}; {no_overlay}"),
            }),
        },
    }
}

fn probe_reason(probe: &Probe) -> String {
    match probe {
        Probe::Ok => "ok".into(),
        Probe::Unsupported { reason } => reason.clone(),
        Probe::Failed { stage, error } => format!("{stage}: {error}"),
    }
}

/// Removes everything a fork created under the agent dir.
pub fn remove(paths: &ForkPaths) -> crate::Result<()> {
    for dir in [&paths.agent_target, &paths.upper, &paths.work] {
        crate::fsutil::remove_dir_all_force(dir)?;
    }
    Ok(())
}

pub(crate) fn io_err<'a>(
    op: &'static str,
    path: &'a Path,
) -> impl FnOnce(std::io::Error) -> ForkError + 'a {
    move |source| ForkError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

mod duration_secs {
    use std::time::Duration;

    pub fn serialize<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::FsInfo;

    fn caps(reflink: bool, overlay: bool) -> Capabilities {
        let probe = |ok: bool| {
            if ok {
                Probe::Ok
            } else {
                Probe::Failed {
                    stage: "test".into(),
                    error: "unavailable".into(),
                }
            }
        };
        Capabilities {
            os: Os::current(),
            kernel: "test".into(),
            store_fs: FsInfo {
                name: "testfs".into(),
            },
            reflink: probe(reflink),
            userns: probe(overlay),
            overlay: probe(overlay),
            sysctls: Vec::new(),
            probe_label: None,
        }
    }

    #[test]
    fn auto_prefers_reflink_then_overlay_then_copy() {
        let ns = Isolation::Namespace;
        let auto = |reflink, overlay, isolation| {
            select(BackendPref::Auto, &caps(reflink, overlay), isolation).unwrap()
        };
        assert_eq!(auto(true, true, ns).kind, BackendKind::Reflink);
        assert_eq!(auto(true, true, Isolation::None).kind, BackendKind::Reflink);
        assert_eq!(auto(true, false, ns).kind, BackendKind::Reflink);
        let no_clones = auto(false, true, ns);
        if cfg!(target_os = "linux") {
            assert_eq!(no_clones.kind, BackendKind::Overlay);
            assert!(
                no_clones.reason.contains("no copy-on-write clones"),
                "{}",
                no_clones.reason
            );
        } else {
            assert_eq!(no_clones.kind, BackendKind::Copy);
        }
        let copy = auto(false, false, ns);
        assert_eq!(copy.kind, BackendKind::Copy);
        assert!(copy.reason.contains("testfs"), "{}", copy.reason);
    }

    #[test]
    fn overlay_requires_namespace_isolation() {
        let c = caps(false, true);
        assert_eq!(
            select(BackendPref::Auto, &c, Isolation::None).unwrap().kind,
            BackendKind::Copy
        );
        assert!(matches!(
            select(BackendPref::Overlay, &c, Isolation::None),
            Err(ForkError::Unavailable {
                backend: BackendKind::Overlay,
                ..
            })
        ));
    }

    #[test]
    fn explicit_unavailable_backend_is_an_error() {
        let c = caps(false, false);
        assert!(select(BackendPref::Reflink, &c, Isolation::Namespace).is_err());
        assert!(select(BackendPref::Overlay, &c, Isolation::Namespace).is_err());
        assert_eq!(
            select(BackendPref::Copy, &c, Isolation::Namespace)
                .unwrap()
                .kind,
            BackendKind::Copy
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reflink_clone_when_supported() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"shared extents").unwrap();
        match reflink::clone_file(&a, &b) {
            Ok(()) => assert_eq!(std::fs::read(&b).unwrap(), b"shared extents"),
            Err(e) => eprintln!(
                "skipping reflink_clone_when_supported: {e} on {}",
                dir.path().display()
            ),
        }
    }
}
