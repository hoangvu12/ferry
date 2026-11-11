//! Small filesystem helpers shared across modules.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, IoContext, Result};
use crate::sys;

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).at("read", path)?;
    serde_json::from_slice(&bytes).map_err(|source| Error::Metadata {
        path: path.to_path_buf(),
        source,
    })
}

/// Writes `value` so that readers see either the old or the new file, never a
/// torn one. Metadata files double as "ready" markers, so a partial write
/// would make a half-built base look complete.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| Error::Metadata {
        path: path.to_path_buf(),
        source,
    })?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    {
        let mut file = fs::File::create(&tmp).at("create", &tmp)?;
        file.write_all(bytes).at("write", &tmp)?;
        file.sync_all().at("sync", &tmp)?;
    }
    fs::rename(&tmp, path).at("rename into place", path)
}

/// Allocated bytes under `path`, counting each hard-linked inode once.
pub fn disk_usage(path: &Path) -> Result<u64> {
    let mut seen = HashSet::new();
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let meta = match fs::symlink_metadata(&dir) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).at("stat", &dir),
        };
        let id = sys::stat(&dir, &meta).at("stat", &dir)?.id;
        if seen.insert(id) {
            total += sys::allocated(&meta);
        }
        if !meta.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir).at("read directory", &dir)? {
            let entry = entry.at("read directory", &dir)?;
            stack.push(entry.path());
        }
    }
    Ok(total)
}

/// `remove_dir_all` that also handles directories without write or search
/// permission. overlayfs leaves a mode-000 `work/work` directory behind, and
/// a plain recursive delete fails on it with EACCES. On Windows, read-only
/// files (git's objects, for one) cannot be deleted until the attribute is
/// cleared.
pub fn remove_dir_all_force(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }
    make_dirs_writable(path)?;
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).at("remove", path),
        _ => Ok(()),
    }
}

/// Removes everything inside `dir` but not `dir` itself, which may be a
/// mount point.
pub(crate) fn clear_dir(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir).at("read directory", dir)? {
        let path = entry.at("read directory", dir)?.path();
        if fs::symlink_metadata(&path).at("stat", &path)?.is_dir() {
            remove_dir_all_force(&path)?;
        } else {
            fs::remove_file(&path).at("remove", &path)?;
        }
    }
    Ok(())
}

fn make_dirs_writable(path: &Path) -> Result<()> {
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&dir) else {
            continue;
        };
        #[cfg(windows)]
        if meta.permissions().readonly() && !meta.is_symlink() {
            let mut perms = meta.permissions();
            // Clears the read-only attribute: no Unix mode bits are involved.
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            fs::set_permissions(&dir, perms).at("make writable", &dir)?;
        }
        if !meta.is_dir() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode();
            if mode & 0o700 != 0o700 {
                fs::set_permissions(&dir, fs::Permissions::from_mode(mode | 0o700))
                    .at("chmod", &dir)?;
            }
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            stack.push(entry.path());
        }
    }
    Ok(())
}

/// Creates `dir` (and missing parents) readable only by the current user, or
/// checks that an existing one is owned by them and not writable by others.
/// Store and worktree directories hold copies of the source and build
/// output; another user who could write there could also swap what a build
/// or agent runs.
///
/// On Windows the directory inherits its parent's ACL (the user profile's
/// `AppData\Local` by default, private to the user), and only its kind is
/// checked.
pub fn private_dir(dir: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if !meta.is_dir() || !owned_by_me(&meta) {
                return Err(Error::Refused(format!(
                    "{} must be a directory owned by you and not writable by others",
                    dir.display()
                )));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                fs::create_dir_all(parent).at("create directory", parent)?;
            }
            #[cfg_attr(windows, allow(unused_mut))]
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(dir) {
                Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                    return Err(e).at("create directory", dir);
                }
                _ => {}
            }
        }
        Err(e) => return Err(e).at("stat", dir),
    }
    Ok(dir.to_path_buf())
}

#[cfg(unix)]
fn owned_by_me(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.uid() == rustix::process::getuid().as_raw() && meta.mode() & 0o022 == 0
}

#[cfg(windows)]
fn owned_by_me(_: &fs::Metadata) -> bool {
    true
}

/// Prints `ferry: <message>` to stderr. Unlike `eprintln!`, it does not
/// panic when stderr is closed (`ferry base 2>&1 | head -1`).
pub fn note(message: &str) {
    let _ = writeln!(std::io::stderr(), "ferry: {message}");
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.json");
        write_json_atomic(&path, &vec![1, 2, 3]).unwrap();
        write_json_atomic(&path, &vec![4]).unwrap();
        let back: Vec<i32> = read_json(&path).unwrap();
        assert_eq!(back, vec![4]);
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[test]
    fn force_remove_handles_unsearchable_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("agent");
        let locked = root.join("work/work");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("f"), b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        }
        #[cfg(windows)]
        {
            let mut perms = fs::metadata(locked.join("f")).unwrap().permissions();
            perms.set_readonly(true);
            fs::set_permissions(locked.join("f"), perms).unwrap();
        }
        remove_dir_all_force(&root).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(190 * 1024 * 1024), "190.0 MiB");
    }
}
