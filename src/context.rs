//! The repository, configuration and store a command operates on.

use std::path::{Path, PathBuf};

use crate::config::{Config, Settings};
use crate::error::{Error, IoContext, Result};
use crate::git::Repo;
use crate::store::{self, Store};

#[derive(Debug, Clone)]
pub struct Context {
    pub repo: Repo,
    pub settings: Settings,
    pub store: Store,
}

impl Context {
    /// Discovers the repository containing `dir` and opens (creating if
    /// needed) its store, reading configuration from the process environment.
    pub fn discover(dir: &Path) -> Result<Self> {
        Self::discover_with_env(dir, &|k| std::env::var(k).ok())
    }

    pub fn discover_with_env(dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let (repo, settings, root) = resolve(dir, env)?;
        let store = Store::open(root, &repo.main_worktree)?;
        Ok(Self {
            repo,
            settings,
            store,
        })
    }

    /// Like [`Context::discover`], but `None` when the repository has no
    /// store yet. Used on paths that run for every cargo call (the shim) or
    /// every agent session, which must not leave a store behind in each
    /// repository they pass through.
    pub fn discover_existing(dir: &Path) -> Result<Option<Self>> {
        let env = |k: &str| std::env::var(k).ok();
        let (repo, settings, root) = resolve(dir, &env)?;
        if !root.join("store.json").is_file() {
            return Ok(None);
        }
        let store = Store::open(root, &repo.main_worktree)?;
        Ok(Some(Self {
            repo,
            settings,
            store,
        }))
    }
}

fn resolve(dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<(Repo, Settings, PathBuf)> {
    let repo = Repo::discover(dir)?;
    let (config, config_path) = Config::load(&repo.main_worktree, env)?;
    let settings = config.resolve(&repo.main_worktree, config_path, env)?;
    let parent = settings.store_parent.clone().ok_or(Error::NoStoreDir)?;
    // Canonical so that reaching the repository through a symlink still
    // finds the same store.
    let common_dir = crate::sys::canonicalize(&repo.common_dir).at("resolve", &repo.common_dir)?;
    let root = parent.join(store::repo_key(&common_dir, &display_name(&common_dir)));
    Ok((repo, settings, root))
}

/// `proj` for `proj/.git` and for a bare `proj.git`. Taken from the common
/// dir rather than a worktree, so that every worktree of a bare repository
/// (which has no main one) opens the same store.
fn display_name(common_dir: &Path) -> String {
    let dir = if common_dir.file_name().is_some_and(|n| n == ".git") {
        common_dir.parent().unwrap_or(common_dir)
    } else {
        common_dir
    };
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
    match name {
        Some(name) => name.strip_suffix(".git").unwrap_or(&name).to_owned(),
        None => "repo".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_ignores_the_git_dir() {
        assert_eq!(display_name(Path::new("/src/proj/.git")), "proj");
        assert_eq!(display_name(Path::new("/src/proj.git")), "proj");
        assert_eq!(display_name(Path::new("/src/proj")), "proj");
        assert_eq!(display_name(Path::new("/")), "repo");
    }
}
