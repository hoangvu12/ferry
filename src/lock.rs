//! Advisory file locks in the store: `flock(2)` on Unix, `LockFileEx` on
//! Windows (both through std's `File::lock`).
//!
//! Locks guard Ferry's own metadata (agent registry, base construction,
//! namespace join/create). They are never held while an agent's build runs.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::error::{IoContext, Result};

#[derive(Debug)]
pub struct FileLock {
    _file: File,
}

#[derive(Clone, Copy)]
enum Kind {
    Exclusive,
    Shared,
}

impl FileLock {
    /// Blocks until the exclusive lock is acquired. `what` names the resource
    /// in the message printed while waiting.
    pub fn exclusive(path: &Path, what: &str) -> Result<Self> {
        Self::acquire(path, what, Kind::Exclusive)
    }

    pub fn shared(path: &Path, what: &str) -> Result<Self> {
        Self::acquire(path, what, Kind::Shared)
    }

    /// Returns `None` if another process holds the lock.
    pub fn try_exclusive(path: &Path) -> Result<Option<Self>> {
        let file = open_lock_file(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e).at("lock", path),
        }
    }

    fn acquire(path: &Path, what: &str, kind: Kind) -> Result<Self> {
        let file = open_lock_file(path)?;
        let attempt = match kind {
            Kind::Exclusive => file.try_lock(),
            Kind::Shared => file.try_lock_shared(),
        };
        match attempt {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                crate::fsutil::note(&format!(
                    "waiting for another ferry process to release {what}"
                ));
                match kind {
                    Kind::Exclusive => file.lock(),
                    Kind::Shared => file.lock_shared(),
                }
                .at("lock", path)?;
            }
            Err(TryLockError::Error(e)) => return Err(e).at("lock", path),
        }
        Ok(Self { _file: file })
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .at("open lock file", path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Retries `check` for up to two seconds. Other test threads fork
    /// children, and until such a child execs it shares this process's open
    /// file descriptions, so a dropped lock can stay held for a moment.
    pub(crate) fn eventually(mut check: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !check() {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        true
    }

    #[test]
    fn exclusive_excludes_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let held = FileLock::exclusive(&path, "test").unwrap();
        assert!(FileLock::try_exclusive(&path).unwrap().is_none());
        drop(held);
        assert!(eventually(|| FileLock::try_exclusive(&path)
            .unwrap()
            .is_some()));
    }
}
