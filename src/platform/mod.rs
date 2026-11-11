//! Detecting what the running system can do: filesystem type, reflinks,
//! unprivileged user namespaces, overlayfs inside them, and the knobs that
//! restrict them (Ubuntu's `AppArmor` userns restriction, Debian's sysctl).
//!
//! Namespace probes run in a child process (`ferry __probe-sandbox`), because
//! entering a user namespace is irreversible for the calling process.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Linux,
    Macos,
    Windows,
}

impl Os {
    #[cfg(test)]
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Os::Macos
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsInfo {
    /// Lowercase filesystem name, e.g. `btrfs`, `xfs`, `ext4`, `tmpfs`,
    /// `apfs`, `zfs`, `overlay`, `nfs`, or `unknown(0x...)`.
    pub name: String,
}

/// Outcome of one capability probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Probe {
    Ok,
    /// Not applicable on this platform (e.g. namespaces on macOS).
    Unsupported {
        reason: String,
    },
    /// Supported in principle but failed here; `stage` names the step
    /// (e.g. `unshare`, `uid_map`, `mount overlay`).
    Failed {
        stage: String,
        error: String,
    },
}

impl Probe {
    pub fn is_ok(&self) -> bool {
        matches!(self, Probe::Ok)
    }

    /// True when the probe itself could not run or answer (e.g. it timed
    /// out), which says nothing about whether the capability exists.
    pub fn is_inconclusive(&self) -> bool {
        matches!(self, Probe::Failed { stage, .. } if stage == "probe process")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub os: Os,
    /// `uname -r` (Linux) or the Darwin release (macOS).
    pub kernel: String,
    /// Filesystem holding the store (bases and agent build state).
    pub store_fs: FsInfo,
    /// Copy-on-write clones between two files in the store.
    pub reflink: Probe,
    /// Unprivileged user + mount namespace with a self-mapped uid, and a
    /// bind mount inside it.
    pub userns: Probe,
    /// overlayfs mounted inside such a namespace, with upper/work in the store.
    pub overlay: Probe,
    /// Linux sysctls that commonly explain a failed `userns` probe.
    pub sysctls: Vec<Sysctl>,
    /// The namespace probe process's `AppArmor` label, e.g. `ferry (enforce)`.
    #[serde(default)]
    pub probe_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sysctl {
    pub name: String,
    pub value: String,
}

impl Capabilities {
    /// Probes everything. `store_root` must exist; probes create and remove
    /// scratch files under it.
    pub fn detect(store_root: &Path) -> Self {
        #[cfg(target_os = "linux")]
        {
            linux::detect(store_root)
        }
        #[cfg(target_os = "macos")]
        {
            macos::detect(store_root)
        }
        #[cfg(windows)]
        {
            windows::detect(store_root)
        }
    }

    pub fn sysctl(&self, name: &str) -> Option<&str> {
        self.sysctls
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.value.as_str())
    }
}

/// Excludes `path` from Time Machine backups, as Cargo does for its target
/// dirs. A no-op on Linux.
pub fn exclude_from_backups(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::exclude_from_backups(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Ok(())
    }
}

/// Tries a copy-on-write clone between two scratch files in `dir`, using the
/// same call the reflink backend uses.
pub fn probe_reflink(dir: &Path) -> Probe {
    let scratch = dir.join(format!(".ferry-reflink-probe-{}", std::process::id()));
    let result = (|| {
        std::fs::create_dir_all(&scratch).map_err(|e| ("create scratch dir", e))?;
        let (src, dst) = (scratch.join("src"), scratch.join("dst"));
        std::fs::write(&src, b"ferry reflink probe").map_err(|e| ("write", e))?;
        #[cfg(target_os = "linux")]
        crate::fork::reflink::clone_file(&src, &dst).map_err(|e| ("FICLONE", e))?;
        #[cfg(target_os = "macos")]
        crate::fork::clonefile::clone_file(&src, &dst).map_err(|e| ("clonefile", e))?;
        #[cfg(windows)]
        crate::sys::clone_file(&src, &dst).map_err(|e| ("FSCTL_DUPLICATE_EXTENTS_TO_FILE", e))?;
        Ok::<(), (&str, std::io::Error)>(())
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    match result {
        Ok(()) => Probe::Ok,
        Err((stage, e)) => Probe::Failed {
            stage: stage.into(),
            error: e.to_string(),
        },
    }
}
