//! The platform-specific calls Ferry makes outside the Linux sandbox and the
//! platform probes, behind one interface for Unix and Windows.
//!
//! On Unix every function here is the `std::os::unix` or `rustix` call the
//! code used before; on Windows it is the closest Win32 equivalent, and the
//! difference is documented on the function. Nothing here pretends: where
//! Windows has no cheap answer (a file's identity is not in its `Metadata`),
//! the function takes the path and asks the filesystem.

use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
pub use std::os::unix::ffi::{OsStrExt, OsStringExt};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::{OsStrExt, OsStringExt};

/// A file's inode number: 64 bits on Unix, 128 on Windows (ReFS file ids
/// do not fit in 64).
#[cfg(unix)]
pub type Ino = u64;
#[cfg(windows)]
pub type Ino = u128;

/// A file's identity: device and inode on Unix, volume serial number and
/// file id on Windows. Two paths with the same identity are the same file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FileId {
    pub dev: u64,
    pub ino: Ino,
}

/// Identity and change indicators of a file, as `(seconds, nanoseconds)`
/// since the Unix epoch. `ctime` is the inode change time on Unix and the
/// NTFS/ReFS `ChangeTime` on Windows; both move when the mtime is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub id: FileId,
    pub len: u64,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
    pub nlink: u64,
}

/// The [`Stat`] of `path` itself (a final symlink is not followed), whose
/// `symlink_metadata` is `meta`. Free on Unix; one open on Windows.
pub fn stat(path: &Path, meta: &Metadata) -> io::Result<Stat> {
    #[cfg(unix)]
    {
        let _ = path;
        Ok(unix_stat(meta))
    }
    #[cfg(windows)]
    {
        let _ = meta;
        windows::stat_path(path, false)
    }
}

/// The [`Stat`] of an open file, whose metadata is `meta`.
pub fn stat_file(file: &File, meta: &Metadata) -> io::Result<Stat> {
    #[cfg(unix)]
    {
        let _ = file;
        Ok(unix_stat(meta))
    }
    #[cfg(windows)]
    {
        let _ = meta;
        windows::stat_handle(file)
    }
}

#[cfg(unix)]
fn unix_stat(meta: &Metadata) -> Stat {
    use std::os::unix::fs::MetadataExt;
    Stat {
        id: FileId {
            dev: meta.dev(),
            ino: meta.ino(),
        },
        len: meta.len(),
        mtime: (meta.mtime(), meta.mtime_nsec()),
        ctime: (meta.ctime(), meta.ctime_nsec()),
        nlink: meta.nlink(),
    }
}

/// The identity of the file `path` resolves to (symlinks followed).
pub fn file_id(path: &Path) -> io::Result<FileId> {
    #[cfg(unix)]
    {
        Ok(unix_stat(&std::fs::metadata(path)?).id)
    }
    #[cfg(windows)]
    {
        Ok(windows::stat_path(path, true)?.id)
    }
}

/// `meta`'s modification time as `(seconds, nanoseconds)` since the epoch.
pub fn mtime(meta: &Metadata) -> (i64, i64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.mtime(), meta.mtime_nsec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        windows::from_filetime(meta.last_write_time().cast_signed())
    }
}

/// `meta`'s access time as `(seconds, nanoseconds)` since the epoch.
pub fn atime(meta: &Metadata) -> (i64, i64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.atime(), meta.atime_nsec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        windows::from_filetime(meta.last_access_time().cast_signed())
    }
}

/// Bytes the file occupies on disk: allocated blocks on Unix, the length on
/// Windows (which does not report allocation in `Metadata`).
pub fn allocated(meta: &Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.blocks() * 512
    }
    #[cfg(windows)]
    {
        meta.len()
    }
}

/// Sets the access and modification times of `path`, each as `(seconds,
/// nanoseconds)` since the epoch, without following a final symlink when
/// `nofollow` is set. Windows keeps 100 ns resolution.
pub fn set_times(
    path: &Path,
    atime: (i64, i64),
    mtime: (i64, i64),
    nofollow: bool,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};
        let ts = |(secs, nsec): (i64, i64)| Timespec {
            tv_sec: secs,
            tv_nsec: nsec as _,
        };
        let times = Timestamps {
            last_access: ts(atime),
            last_modification: ts(mtime),
        };
        let flags = if nofollow {
            AtFlags::SYMLINK_NOFOLLOW
        } else {
            AtFlags::empty()
        };
        utimensat(CWD, path, &times, flags).map_err(io::Error::from)
    }
    #[cfg(windows)]
    {
        windows::set_times(path, atime, mtime, nofollow)
    }
}

/// Opens `path` for reading without following a symlink in its last
/// component. `Ok(None)` if the last component is a symlink (or, on
/// Windows, any reparse point). On Unix the open does not block on a FIFO.
pub fn open_nofollow(path: &Path) -> io::Result<Option<File>> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags};
        use rustix::io::Errno;
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        match rustix::fs::open(path, flags, Mode::empty()) {
            Ok(fd) => Ok(Some(File::from(fd))),
            Err(Errno::LOOP) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(windows)]
    {
        windows::open_nofollow(path)
    }
}

/// Runs `command` in place of this process: `exec` on Unix. Windows has no
/// `exec`: the command runs as a child with this process's standard
/// handles, this process ignores Ctrl+C meanwhile (the child gets it from
/// the console) and exits with the child's exit code. Returns only on
/// failure to start.
pub fn exec(command: &mut Command) -> io::Error {
    #[cfg(unix)]
    {
        std::os::unix::process::CommandExt::exec(command)
    }
    #[cfg(windows)]
    {
        windows::exec(command)
    }
}

/// Bytes available to the current user on the filesystem holding `path`.
pub fn free_space(path: &Path) -> io::Result<u64> {
    #[cfg(unix)]
    {
        let s = rustix::fs::statvfs(path)?;
        Ok(s.f_bavail.saturating_mul(s.f_frsize))
    }
    #[cfg(windows)]
    {
        windows::free_space(path)
    }
}

/// `fs::canonicalize`, except that on Windows the result is not a verbatim
/// (`\\?\C:\...`) path where a plain one means the same, so it prefix-matches
/// paths that came from elsewhere (the current directory, git, the user).
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref();
    #[cfg(unix)]
    {
        std::fs::canonicalize(path)
    }
    #[cfg(windows)]
    {
        dunce::canonicalize(path)
    }
}

/// Creates a symlink at `link` with the same target and kind as the symlink
/// `original` (Windows tells file and directory symlinks apart).
pub fn copy_symlink(original: &Path, link: &Path) -> io::Result<()> {
    let target = std::fs::read_link(original)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, link)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if std::fs::symlink_metadata(original)?
            .file_type()
            .is_symlink_dir()
        {
            std::os::windows::fs::symlink_dir(&target, link)
        } else {
            std::os::windows::fs::symlink_file(&target, link)
        }
    }
}

/// The name a program is looked up by on `PATH`: `cargo` on Unix,
/// `cargo.exe` on Windows.
pub fn exe_name(stem: &str) -> String {
    format!("{stem}{}", std::env::consts::EXE_SUFFIX)
}

/// Whether the program file name `name` (`argv[0]`'s last component, or an
/// executable's file name) is `stem`: exactly on Unix; on Windows ignoring
/// case and an `.exe` extension.
pub fn is_program(name: &std::ffi::OsStr, stem: &str) -> bool {
    #[cfg(unix)]
    {
        name == stem
    }
    #[cfg(windows)]
    {
        let name = name.to_string_lossy();
        let name = name
            .len()
            .checked_sub(4)
            .filter(|&i| name.is_char_boundary(i) && name[i..].eq_ignore_ascii_case(".exe"))
            .map_or(&*name, |i| &name[..i]);
        name.eq_ignore_ascii_case(stem)
    }
}

#[cfg(windows)]
pub use self::windows::{agent_job_pids, clone_file, fs_name, join_agent_job, windows_version};
