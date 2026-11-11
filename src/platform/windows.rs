//! Windows capability detection: the store volume's filesystem name.
//!
//! Windows has no user or mount namespaces, so agents always build at their
//! own paths, as on macOS. Copy-on-write clones (`ReFS` block cloning, on a
//! Dev Drive) are not implemented yet, so forks are full copies.

use std::path::Path;

use super::{Capabilities, FsInfo, Os, Probe};

/// Windows capability report: build number, store filesystem, and the
/// clone probe. Namespaces and overlayfs are reported unsupported.
pub fn detect(store_root: &Path) -> Capabilities {
    let no_namespaces = || Probe::Unsupported {
        reason: "Windows has no mount namespaces".into(),
    };
    let store_fs = fs_info(store_root);
    let reflink = if store_fs.name == "refs" {
        super::probe_reflink(store_root)
    } else {
        Probe::Unsupported {
            reason: format!("{} has no copy-on-write clones", store_fs.name),
        }
    };
    Capabilities {
        os: Os::Windows,
        kernel: crate::sys::windows_version(),
        store_fs,
        reflink,
        userns: no_namespaces(),
        overlay: no_namespaces(),
        sysctls: Vec::new(),
        probe_label: None,
    }
}

/// Lowercase filesystem name of the volume holding `path` (`ntfs`, `refs`,
/// `fat32`, ...), or `unknown`.
pub fn fs_info(path: &Path) -> FsInfo {
    FsInfo {
        name: crate::sys::fs_name(path).map_or_else(|_| "unknown".into(), |n| n.to_lowercase()),
    }
}
