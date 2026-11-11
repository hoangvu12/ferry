//! Recursive tree cloning shared by the copy and Linux reflink backends:
//! walks the base target, recreates directories and symlinks, clones or
//! copies regular files in parallel, preserves hard links within the tree,
//! and restores permissions and mtimes.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{ForkError, ForkStats, io_err};
use crate::sys;

/// How one regular file is materialized. Must create `dst` (which does not
/// exist yet) with `src`'s bytes.
pub(crate) type FileOp = fn(&Path, &Path) -> io::Result<()>;

struct Job {
    src: PathBuf,
    dst: PathBuf,
    meta: fs::Metadata,
}

/// Clones the tree at `src` into `dst`, which must not exist.
pub(crate) fn clone_tree(src: &Path, dst: &Path, op: FileOp) -> Result<ForkStats, ForkError> {
    let started = Instant::now();
    if dst.exists() {
        return Err(io_err("create", dst)(io::Error::from(
            io::ErrorKind::AlreadyExists,
        )));
    }
    let mut files = Vec::new();
    let mut links: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut dirs: Vec<(PathBuf, fs::Metadata)> = Vec::new();
    let mut first_link: HashMap<sys::FileId, PathBuf> = HashMap::new();

    let root_meta = fs::symlink_metadata(src).map_err(io_err("stat", src))?;
    fs::create_dir(dst).map_err(io_err("create directory", dst))?;
    dirs.push((dst.to_path_buf(), root_meta));
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((from_dir, to_dir)) = stack.pop() {
        for entry in fs::read_dir(&from_dir).map_err(io_err("read directory", &from_dir))? {
            let entry = entry.map_err(io_err("read directory", &from_dir))?;
            let from = entry.path();
            let to = to_dir.join(entry.file_name());
            let meta = fs::symlink_metadata(&from).map_err(io_err("stat", &from))?;
            let kind = meta.file_type();
            if kind.is_dir() {
                fs::create_dir(&to).map_err(io_err("create directory", &to))?;
                dirs.push((to.clone(), meta));
                stack.push((from, to));
            } else if kind.is_symlink() {
                sys::copy_symlink(&from, &to).map_err(io_err("create symlink", &to))?;
                set_times(&to, &meta, true).map_err(io_err("set times on", &to))?;
            } else if kind.is_file() {
                // Cargo hard-links artifacts from deps/ into the profile dir;
                // keeping the link avoids a second clone and matches what Cargo
                // would have produced.
                let stat = sys::stat(&from, &meta).map_err(io_err("stat", &from))?;
                if stat.nlink > 1 {
                    let key = stat.id;
                    if let Some(first) = first_link.get(&key) {
                        links.push((first.clone(), to));
                        continue;
                    }
                    first_link.insert(key, to.clone());
                }
                files.push(Job {
                    src: from,
                    dst: to,
                    meta,
                });
            } else {
                return Err(io_err("clone", &from)(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "not a regular file, directory or symlink",
                )));
            }
        }
    }

    let bytes = files.iter().map(|j| j.meta.len()).sum();
    run_parallel(&files, op)?;
    for (existing, new) in &links {
        fs::hard_link(existing, new).map_err(io_err("hard-link", new))?;
    }
    // Deepest first: creating entries bumps a directory's mtime.
    dirs.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
    for (dir, meta) in &dirs {
        set_permissions(dir, meta).map_err(io_err("chmod", dir))?;
        set_times(dir, meta, false).map_err(io_err("set times on", dir))?;
    }
    Ok(ForkStats {
        files: (files.len() + links.len()) as u64,
        bytes,
        elapsed: started.elapsed(),
    })
}

fn run_parallel(files: &[Job], op: FileOp) -> Result<(), ForkError> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(16));
    let chunk = files.len().div_ceil(threads).max(64);
    std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|jobs| scope.spawn(move || jobs.iter().try_for_each(|job| materialize(job, op))))
            .collect();
        for handle in handles {
            match handle.join() {
                Ok(result) => result?,
                Err(_) => {
                    return Err(io_err("clone", Path::new("target"))(io::Error::other(
                        "a clone worker thread panicked",
                    )));
                }
            }
        }
        Ok(())
    })
}

fn materialize(job: &Job, op: FileOp) -> Result<(), ForkError> {
    op(&job.src, &job.dst).map_err(io_err("clone", &job.src))?;
    set_permissions(&job.dst, &job.meta).map_err(io_err("chmod", &job.dst))?;
    set_times(&job.dst, &job.meta, false).map_err(io_err("set times on", &job.dst))
}

/// Cargo's freshness check compares mtimes, so a fork that does not preserve
/// them is a fork that rebuilds everything.
pub(super) fn set_times(path: &Path, meta: &fs::Metadata, symlink: bool) -> io::Result<()> {
    sys::set_times(path, sys::atime(meta), sys::mtime(meta), symlink)
}

/// Gives `path` the permissions in `meta`: the mode bits on Unix, the
/// read-only attribute on Windows.
fn set_permissions(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        fs::set_permissions(path, fs::Permissions::from_mode(meta.mode() & 0o7777))
    }
    #[cfg(windows)]
    {
        if meta.permissions().readonly() {
            fs::set_permissions(path, meta.permissions())
        } else {
            Ok(())
        }
    }
}

/// Copies `src` to the new file `dst`. On filesystems that support it the
/// kernel may share extents (`copy_file_range`), which is fine: the result is
/// still a separate inode.
pub(crate) fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    // `CopyFileExW`, the fastest copy Windows has; `dst` does not exist, so
    // nothing is overwritten.
    #[cfg(windows)]
    {
        fs::copy(src, dst).map(drop)
    }
    #[cfg(unix)]
    {
        copy_contents(src, dst)
    }
}

#[cfg(unix)]
fn copy_contents(src: &Path, dst: &Path) -> io::Result<()> {
    let mut from = fs::File::open(src)?;
    let mut to = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    io::copy(&mut from, &mut to)?;
    Ok(())
}

/// Bytes of files under `agent` that no longer share data with the file at
/// the same path under `base` (rewritten, new, or changed size or mtime).
/// Clones that were never written keep the base's size and mtime.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn private_bytes_vs_base(agent: &Path, base: &Path) -> Result<u64, ForkError> {
    let mut total = 0;
    let mut stack = vec![agent.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err("read directory", &dir)(e)),
        };
        for entry in entries {
            let entry = entry.map_err(io_err("read directory", &dir))?;
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).map_err(io_err("stat", &path))?;
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let shared = path
                .strip_prefix(agent)
                .ok()
                .and_then(|rel| fs::symlink_metadata(base.join(rel)).ok())
                .is_some_and(|b| {
                    b.len() == meta.len() && b.modified().ok() == meta.modified().ok()
                });
            if !shared {
                total += sys::allocated(&meta);
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn fixture(root: &Path) {
        fs::create_dir_all(root.join("debug/deps")).unwrap();
        fs::create_dir_all(root.join("debug/.fingerprint/a-1")).unwrap();
        fs::write(root.join("debug/deps/liba-1.rlib"), b"rlib").unwrap();
        fs::hard_link(
            root.join("debug/deps/liba-1.rlib"),
            root.join("debug/liba.rlib"),
        )
        .unwrap();
        fs::write(root.join("debug/.fingerprint/a-1/lib-a"), b"fp").unwrap();
        fs::write(root.join("debug/app"), b"#!bin").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.join("debug/app"), fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink("deps/liba-1.rlib", root.join("debug/link")).unwrap();
        }
    }

    #[test]
    fn copy_preserves_structure_links_modes_and_mtimes() {
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        fixture(&src);
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        fs::File::options()
            .write(true)
            .open(src.join("debug/app"))
            .unwrap()
            .set_modified(old)
            .unwrap();

        let stats = clone_tree(&src, &dst, copy_file).unwrap();
        assert_eq!(stats.files, 4);

        assert_eq!(
            fs::read(dst.join("debug/.fingerprint/a-1/lib-a")).unwrap(),
            b"fp"
        );
        let app = fs::metadata(dst.join("debug/app")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(app.permissions().mode() & 0o777, 0o755);
        }
        assert_eq!(app.modified().unwrap(), old);
        let a = sys::file_id(&dst.join("debug/deps/liba-1.rlib")).unwrap();
        let b = sys::file_id(&dst.join("debug/liba.rlib")).unwrap();
        assert_eq!(a, b, "hard link preserved inside the fork");
        let base = sys::file_id(&src.join("debug/deps/liba-1.rlib")).unwrap();
        assert_ne!(a, base, "fork never shares inodes with the base");
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(dst.join("debug/link")).unwrap(),
            PathBuf::from("deps/liba-1.rlib")
        );
        assert_eq!(
            fs::metadata(dst.join("debug/deps"))
                .unwrap()
                .modified()
                .unwrap(),
            fs::metadata(src.join("debug/deps"))
                .unwrap()
                .modified()
                .unwrap()
        );
    }

    #[test]
    fn writing_the_fork_leaves_the_base_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        fixture(&src);
        clone_tree(&src, &dst, copy_file).unwrap();
        // What Cargo does to a fingerprint: truncate and rewrite in place.
        fs::write(dst.join("debug/.fingerprint/a-1/lib-a"), b"changed").unwrap();
        fs::write(dst.join("debug/liba.rlib"), b"changed").unwrap();
        assert_eq!(
            fs::read(src.join("debug/.fingerprint/a-1/lib-a")).unwrap(),
            b"fp"
        );
        assert_eq!(
            fs::read(src.join("debug/deps/liba-1.rlib")).unwrap(),
            b"rlib"
        );
        assert!(private_bytes_vs_base(&dst, &src).unwrap() > 0);
    }

    #[test]
    fn refuses_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fixture(&src);
        assert!(clone_tree(&src, dir.path(), copy_file).is_err());
    }
}
