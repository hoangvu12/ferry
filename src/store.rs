//! On-disk layout of a repository's Ferry store.
//!
//! ```text
//! <store>/                      one per repository, keyed by its git common dir
//!   store.json                  layout version + repository path
//!   lock                        store-wide lock for registry changes
//!   watch.lock                  held by `ferry base --watch`
//!   current                     id of the base new agents fork from
//!   mnt/src, mnt/target         canonical mountpoints (Linux namespace mode)
//!   bases/<id>/                 immutable once base.json exists
//!     base.json  src/  target/
//!     relocation.json           what a fork without the sandbox removes
//!                               from target/ (written before base.json)
//!     warm-units/               cargo calls the shim recorded; only while building
//!   bases/<id>.lock             held while the base is being built
//!   agents/<name>/
//!     agent.json  lock  target/ | upper/ work/ mount.lock ns.json
//! ```

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::fsutil;
use crate::lock::FileLock;

const LAYOUT_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreInfo {
    layout: u32,
    repository: PathBuf,
}

/// Paths belonging to one base.
#[derive(Debug, Clone)]
pub struct BasePaths {
    pub dir: PathBuf,
    pub meta: PathBuf,
    pub src: PathBuf,
    pub target: PathBuf,
    /// The relocation plan for forks of `target` that build without the
    /// namespace sandbox; only bases built without it have one.
    pub relocation: PathBuf,
    pub lock: PathBuf,
}

/// Paths belonging to one agent. Which of `target` or `upper`/`work` exist
/// depends on the fork backend.
#[derive(Debug, Clone)]
pub struct AgentPaths {
    pub dir: PathBuf,
    pub meta: PathBuf,
    pub lock: PathBuf,
    pub target: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
    pub ns_record: PathBuf,
    /// Held (shared) through an inherited descriptor by every process that
    /// runs as this agent, whatever the backend.
    pub busy_lock: PathBuf,
}

impl Store {
    /// Opens (creating if needed) the store rooted at `root` for the
    /// repository whose main worktree is `repository`.
    pub fn open(root: PathBuf, repository: &Path) -> Result<Self> {
        let store = Self { root };
        fsutil::private_dir(&store.root)?;
        for dir in [
            store.root.join("bases"),
            store.root.join("agents"),
            store.canonical_src(),
            store.canonical_target(),
        ] {
            fs::create_dir_all(&dir).at("create directory", &dir)?;
        }
        let info_path = store.root.join("store.json");
        if info_path.exists() {
            let info: StoreInfo = fsutil::read_json(&info_path)?;
            if info.layout > LAYOUT_VERSION {
                return Err(Error::Refused(format!(
                    "store {} was written by a newer ferry (layout {}); upgrade ferry",
                    store.root.display(),
                    info.layout
                )));
            }
        } else {
            fsutil::write_json_atomic(
                &info_path,
                &StoreInfo {
                    layout: LAYOUT_VERSION,
                    repository: repository.to_path_buf(),
                },
            )?;
        }
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The repository key this store is named after.
    pub fn key(&self) -> &str {
        self.root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo")
    }

    /// Where the agent worktree is bind-mounted inside a namespace sandbox.
    /// Every base and agent of this repository builds at this same path, which
    /// is what keeps rustc's incremental state (absolute paths) reusable.
    pub fn canonical_src(&self) -> PathBuf {
        self.root.join("mnt/src")
    }

    pub fn canonical_target(&self) -> PathBuf {
        self.root.join("mnt/target")
    }

    pub fn lock(&self) -> Result<FileLock> {
        FileLock::exclusive(&self.root.join("lock"), "the ferry store")
    }

    /// Held by the one `ferry base --watch` of this repository.
    pub fn watch_lock(&self) -> PathBuf {
        self.root.join("watch.lock")
    }

    pub fn base(&self, id: &BaseId) -> BasePaths {
        let dir = self.root.join("bases").join(id.as_str());
        BasePaths {
            meta: dir.join("base.json"),
            src: dir.join("src"),
            target: dir.join("target"),
            relocation: dir.join("relocation.json"),
            lock: self.root.join("bases").join(format!("{id}.lock")),
            dir,
        }
    }

    pub fn agent(&self, name: &AgentName) -> AgentPaths {
        let dir = self.root.join("agents").join(name.as_str());
        AgentPaths {
            meta: dir.join("agent.json"),
            lock: dir.join("lock"),
            target: dir.join("target"),
            upper: dir.join("upper"),
            work: dir.join("work"),
            ns_record: dir.join("ns.json"),
            busy_lock: dir.join("busy.lock"),
            dir,
        }
    }

    pub fn current_base(&self) -> Result<Option<BaseId>> {
        let path = self.root.join("current");
        match fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s.trim().parse()?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).at("read", &path),
        }
    }

    pub fn set_current_base(&self, id: &BaseId) -> Result<()> {
        fsutil::write_atomic(&self.root.join("current"), format!("{id}\n").as_bytes())
    }

    /// Ids of every directory under `bases/`, complete or not.
    pub fn base_ids(&self) -> Result<Vec<BaseId>> {
        let mut ids: Vec<BaseId> = Self::list_dirs(&self.root.join("bases"))?
            .into_iter()
            .filter_map(|n| n.parse().ok())
            .collect();
        ids.sort();
        Ok(ids)
    }

    pub fn agent_names(&self) -> Result<Vec<AgentName>> {
        let mut names: Vec<AgentName> = Self::list_dirs(&self.root.join("agents"))?
            .into_iter()
            .filter_map(|n| n.parse().ok())
            .collect();
        names.sort();
        Ok(names)
    }

    fn list_dirs(dir: &Path) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(dir).at("read directory", dir)? {
            let entry = entry.at("read directory", dir)?;
            if entry.file_type().at("stat", entry.path())?.is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                out.push(name.to_owned());
            }
        }
        Ok(out)
    }
}

/// Directory name for a repository's store: readable prefix plus a stable
/// hash of the git common dir, so every worktree of one repository (agents
/// included) resolves to the same store.
pub fn repo_key(common_dir: &Path, display_name: &str) -> String {
    let name: String = display_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    format!(
        "{name}-{:016x}",
        fnv1a64(common_dir.as_os_str().as_encoded_bytes())
    )
}

/// FNV-1a. Store keys must stay identical across ferry versions and Rust
/// releases, which rules out `DefaultHasher`.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Platform cache directory for Ferry: `$XDG_CACHE_HOME/ferry` or
/// `~/.cache/ferry` on Linux, `~/Library/Caches/ferry` on macOS.
///
/// On Windows `%LOCALAPPDATA%\ferry`, whatever the shell: Git Bash sets
/// `HOME` and PowerShell does not, and both must find the same store.
pub fn default_store_parent() -> Option<PathBuf> {
    if cfg!(windows) {
        return local_app_data().map(|d| d.join("ferry"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library/Caches/ferry"));
    }
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".cache")))
        .map(|c| c.join("ferry"))
}

/// Platform data directory for agent worktrees:
/// `$XDG_DATA_HOME/ferry/worktrees` or `~/.local/share/ferry/worktrees` on
/// Linux, `~/Library/Application Support/ferry/worktrees` on macOS.
///
/// On Windows `%LOCALAPPDATA%\ferry\worktrees`, which Windows never purges.
pub fn default_worktree_parent() -> Option<PathBuf> {
    if cfg!(windows) {
        return local_app_data().map(|d| d.join("ferry").join("worktrees"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library/Application Support/ferry/worktrees"));
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".local/share")))
        .map(|d| d.join("ferry/worktrees"))
}

/// `%LOCALAPPDATA%`, if it is set to an absolute path.
fn local_app_data() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// A validated agent name. It becomes a directory name and part of a git
/// branch name, so the alphabet is deliberately small.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentName(String);

impl AgentName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AgentName {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let invalid = |reason| Error::InvalidName {
            name: s.to_owned(),
            reason,
        };
        if s.is_empty() || s.len() > 64 {
            return Err(invalid("must be 1 to 64 characters"));
        }
        if !s.starts_with(|c: char| c.is_ascii_alphanumeric()) {
            return Err(invalid("must start with a letter or digit"));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(invalid(
                "may only contain letters, digits, '-', '_' and '.'",
            ));
        }
        // Git refuses branch components ending in ".lock" (case-sensitively).
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        let lock_suffix = s.ends_with(".lock");
        if s.ends_with('.') || s.contains("..") || lock_suffix {
            return Err(invalid("is not a valid git branch component"));
        }
        Ok(Self(s.to_owned()))
    }
}

impl TryFrom<String> for AgentName {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<AgentName> for String {
    fn from(n: AgentName) -> String {
        n.0
    }
}

impl fmt::Display for AgentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `<commit>-<fingerprint>`: the commit the base was built from plus a hash of
/// everything else that determines its artifacts (toolchain, warm commands,
/// freshness and isolation mode). The id only decides which base to reuse;
/// build correctness never depends on it because Cargo's own fingerprints
/// still decide what is fresh.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BaseId(String);

impl BaseId {
    pub fn new(commit: &str, fingerprint: u64) -> Self {
        Self(format!("{commit}-{:08x}", fingerprint & 0xffff_ffff))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The fingerprint part of the id: equal for bases built with the same
    /// toolchain, warm commands and modes.
    pub fn fingerprint(&self) -> &str {
        self.0.rsplit_once('-').map_or("", |(_, fp)| fp)
    }

    /// Short form for human output: 12 hex digits of the commit plus the
    /// fingerprint.
    pub fn short(&self) -> String {
        match self.0.rsplit_once('-') {
            Some((commit, fp)) => format!("{}-{fp}", &commit[..commit.len().min(12)]),
            None => self.0.clone(),
        }
    }
}

impl FromStr for BaseId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let valid = s.rsplit_once('-').is_some_and(|(commit, fp)| {
            commit.len() >= 7
                && fp.len() == 8
                && commit.bytes().all(|b| b.is_ascii_hexdigit())
                && fp.bytes().all(|b| b.is_ascii_hexdigit())
        });
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(Error::BaseMissing { id: s.to_owned() })
        }
    }
}

impl TryFrom<String> for BaseId {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<BaseId> for String {
    fn from(id: BaseId) -> String {
        id.0
    }
}

impl fmt::Display for BaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_are_validated() {
        for ok in ["a", "agent-1", "fix_bug.2", "A9"] {
            assert!(ok.parse::<AgentName>().is_ok(), "{ok}");
        }
        for bad in [
            "",
            "-a",
            ".a",
            "a/b",
            "a..b",
            "a.",
            "x.lock",
            "a b",
            &"a".repeat(65),
        ] {
            assert!(bad.parse::<AgentName>().is_err(), "{bad}");
        }
    }

    #[test]
    fn base_id_round_trips() {
        let id = BaseId::new("0123456789abcdef0123456789abcdef01234567", 0x1_dead_beef);
        assert_eq!(
            id.as_str(),
            "0123456789abcdef0123456789abcdef01234567-deadbeef"
        );
        assert_eq!(id.short(), "0123456789ab-deadbeef");
        assert_eq!(id.as_str().parse::<BaseId>().unwrap(), id);
        assert!("not-an-id".parse::<BaseId>().is_err());
    }

    #[test]
    fn repo_key_is_stable_and_sanitized() {
        let key = repo_key(Path::new("/home/u/src/my repo/.git"), "my repo");
        assert_eq!(
            key,
            repo_key(Path::new("/home/u/src/my repo/.git"), "my repo")
        );
        assert!(key.starts_with("my_repo-"));
        assert_ne!(key, repo_key(Path::new("/other/.git"), "my repo"));
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn open_creates_layout_and_current_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("s"), Path::new("/repo")).unwrap();
        assert!(store.canonical_src().is_dir());
        assert!(store.current_base().unwrap().is_none());
        let id = BaseId::new("abcdef1234567", 7);
        store.set_current_base(&id).unwrap();
        assert_eq!(store.current_base().unwrap(), Some(id));
    }
}
