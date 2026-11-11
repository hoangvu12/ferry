//! Git operations, by shelling out to the `git` binary.
//!
//! Ferry needs `worktree add/lock/remove` with the user's real configuration:
//! hooks, filters, LFS, `core.autocrlf`, sparse checkout. libgit2 and gix do
//! not implement worktree creation with those semantics, and getting any of
//! them subtly wrong would change file contents relative to what `git`
//! itself checks out, which is exactly the class of bug Ferry exists to
//! prevent. The git CLI is everywhere Ferry runs.

use crate::sys::OsStringExt;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git is not installed or not on PATH")]
    NotInstalled,

    #[error("{} is not inside a git repository", .0.display())]
    NotARepo(PathBuf),

    #[error("`git {args}` failed: {stderr}")]
    Command { args: String, stderr: String },

    #[error("could not parse output of `git {args}`: {detail}")]
    Parse { args: String, detail: String },

    #[error("`{rev}` does not name a commit")]
    BadRevision { rev: String },
}

impl GitError {
    pub fn hint(&self) -> Option<String> {
        match self {
            GitError::NotInstalled => Some("install git 2.20 or newer".into()),
            GitError::NotARepo(_) => {
                Some("run ferry from inside a git repository, or pass -C <dir>".into())
            }
            _ => None,
        }
    }
}

type Result<T> = std::result::Result<T, GitError>;

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct CommitId(String);

impl CommitId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One entry of `git ls-tree -r` or `git ls-files -s`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Git file mode, e.g. `0o100644`, `0o100755`, `0o120000` (symlink),
    /// `0o160000` (submodule).
    pub mode: u32,
    pub oid: String,
    /// Merge stage; always 0 for tree entries.
    pub stage: u8,
    /// Path relative to the worktree root, as raw bytes from git.
    pub path: PathBuf,
}

impl Entry {
    pub fn is_regular_file(&self) -> bool {
        self.mode == 0o100_644 || self.mode == 0o100_755
    }
}

/// A repository as seen from some directory inside one of its worktrees.
#[derive(Debug, Clone)]
pub struct Repo {
    /// Root of the worktree containing the directory Ferry was started in.
    pub toplevel: PathBuf,
    /// The shared `.git` directory of all worktrees.
    pub common_dir: PathBuf,
    /// The main worktree (first entry of `git worktree list`), or `toplevel`
    /// for bare repositories.
    pub main_worktree: PathBuf,
}

impl Repo {
    pub fn discover(dir: &Path) -> Result<Self> {
        let out = run(
            dir,
            [
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-common-dir",
            ],
        )
        .map_err(|e| match e {
            GitError::Command { .. } => GitError::NotARepo(dir.to_path_buf()),
            other => other,
        })?;
        let mut lines = out.split(|b| *b == b'\n').filter(|l| !l.is_empty());
        let (Some(toplevel), Some(common)) = (lines.next(), lines.next()) else {
            return Err(GitError::NotARepo(dir.to_path_buf()));
        };
        let toplevel = path_from_bytes(toplevel);
        let common_dir = path_from_bytes(common);
        let main_worktree = main_worktree(&toplevel)?.unwrap_or_else(|| toplevel.clone());
        Ok(Self {
            toplevel,
            common_dir,
            main_worktree,
        })
    }

    /// Resolves `rev` to a full commit id.
    pub fn resolve_commit(&self, rev: &str) -> Result<CommitId> {
        let spec = format!("{rev}^{{commit}}");
        let out = run(
            &self.toplevel,
            [
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &spec,
            ],
        )
        .map_err(|_| GitError::BadRevision {
            rev: rev.to_owned(),
        })?;
        let id = String::from_utf8_lossy(&out).trim().to_owned();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(GitError::BadRevision {
                rev: rev.to_owned(),
            });
        }
        Ok(CommitId(id))
    }

    /// The git dir of the worktree at `toplevel`, where a rebase, merge or
    /// bisect in progress keeps its state.
    pub fn git_dir(&self) -> Result<PathBuf> {
        let out = run(&self.toplevel, ["rev-parse", "--absolute-git-dir"])?;
        Ok(path_from_bytes(out.trim_ascii_end()))
    }

    /// Detached worktree at `commit` (used for bases).
    pub fn add_detached_worktree(&self, path: &Path, commit: &CommitId) -> Result<()> {
        run(
            &self.toplevel,
            [
                os("worktree"),
                os("add"),
                os("--quiet"),
                os("--detach"),
                path.as_os_str().to_owned(),
                os(commit.as_str()),
            ],
        )
        .map(drop)
    }

    /// Worktree on a new branch `branch` starting at `commit` (used for agents).
    pub fn add_branch_worktree(&self, path: &Path, branch: &str, commit: &CommitId) -> Result<()> {
        run(
            &self.toplevel,
            [
                os("worktree"),
                os("add"),
                os("--quiet"),
                os("-b"),
                os(branch),
                path.as_os_str().to_owned(),
                os(commit.as_str()),
            ],
        )
        .map(drop)
    }

    /// Prevents `git worktree prune` from deleting a base's source tree while
    /// it lives in a cache directory.
    pub fn lock_worktree(&self, path: &Path, reason: &str) -> Result<()> {
        run(
            &self.toplevel,
            [
                os("worktree"),
                os("lock"),
                os("--reason"),
                os(reason),
                path.as_os_str().to_owned(),
            ],
        )
        .map(drop)
    }

    /// Removes a worktree. Without `force`, git refuses if it has uncommitted
    /// changes or untracked files, which protects agent work.
    pub fn remove_worktree(&self, path: &Path, force: bool) -> Result<()> {
        let mut args = vec![os("worktree"), os("remove")];
        if force {
            // Twice: once for dirty trees, once for locked ones.
            args.extend([os("--force"), os("--force")]);
        }
        args.push(path.as_os_str().to_owned());
        run(&self.toplevel, args).map(drop)
    }

    /// Drops git's registration of the worktree at `path`, whether or not the
    /// directory still exists (a deleted cache dir leaves a "missing but
    /// locked" entry behind). Unlike `git worktree prune`, this never touches
    /// other worktrees, such as ones on a drive that is not mounted right now.
    pub fn forget_worktree(&self, path: &Path) -> Result<()> {
        if !self.worktrees()?.iter().any(|w| same_path(&w.path, path)) {
            return Ok(());
        }
        self.remove_worktree(path, true)
    }

    /// Every worktree git knows about (`git worktree list --porcelain`).
    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        let out = run(&self.toplevel, ["worktree", "list", "--porcelain", "-z"])?;
        let mut all = Vec::new();
        let mut current: Option<Worktree> = None;
        for field in out.split(|b| *b == 0) {
            if field.is_empty() {
                all.extend(current.take());
            } else if let Some(path) = field.strip_prefix(b"worktree ") {
                all.extend(current.take());
                current = Some(Worktree {
                    path: path_from_bytes(path),
                    branch: None,
                    locked: false,
                });
            } else if let Some(w) = current.as_mut() {
                if let Some(branch) = field.strip_prefix(b"branch refs/heads/") {
                    w.branch = Some(String::from_utf8_lossy(branch).into_owned());
                } else if field == b"locked" || field.starts_with(b"locked ") {
                    w.locked = true;
                }
            }
        }
        all.extend(current);
        Ok(all)
    }

    /// Tip of `refs/heads/<branch>`, or `None` if the branch does not exist.
    pub fn branch_tip(&self, branch: &str) -> Result<Option<CommitId>> {
        let refname = format!("refs/heads/{branch}");
        match run(
            &self.toplevel,
            ["rev-parse", "--verify", "--quiet", &refname],
        ) {
            Ok(out) => Ok(Some(CommitId(
                String::from_utf8_lossy(&out).trim().to_owned(),
            ))),
            Err(GitError::Command { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        run(&self.toplevel, ["branch", "--quiet", "-D", branch]).map(drop)
    }

    /// Every blob in `commit`'s tree, recursively.
    pub fn ls_tree(&self, commit: &CommitId) -> Result<Vec<Entry>> {
        let args = ["ls-tree", "-r", "-z", "--full-tree", commit.as_str()];
        let out = run(&self.toplevel, args)?;
        parse_entries(&out, false).map_err(|detail| GitError::Parse {
            args: args.join(" "),
            detail,
        })
    }

    /// Contents of `path` at `commit`, or `None` if it does not exist there.
    pub fn show_file(&self, commit: &CommitId, path: &str) -> Result<Option<Vec<u8>>> {
        let spec = format!("{commit}:{path}");
        match run(&self.toplevel, ["cat-file", "blob", &spec]) {
            Ok(out) => Ok(Some(out)),
            Err(GitError::Command { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Index entries of the worktree at `worktree` (`git ls-files -s`).
pub fn ls_files(worktree: &Path) -> Result<Vec<Entry>> {
    let args = ["ls-files", "-s", "-z"];
    let out = run(worktree, args)?;
    parse_entries(&out, true).map_err(|detail| GitError::Parse {
        args: args.join(" "),
        detail,
    })
}

/// Rewrites the index stat cache after stamping. Otherwise every stamped
/// file looks stat-dirty and the agent's first `git status` rehashes the
/// whole tree.
pub fn refresh_index(worktree: &Path) -> Result<()> {
    // Exit status 1 just means some entries need updating; that is not an
    // error here.
    let output = command(worktree, ["update-index", "-q", "--refresh"])
        .output()
        .map_err(|e| map_spawn_error(&e))?;
    match output.status.code() {
        Some(0 | 1) => Ok(()),
        _ => Err(command_error(&["update-index", "-q", "--refresh"], &output)),
    }
}

/// Paths reported by `git status --porcelain` (changed, untracked).
pub fn status_paths(worktree: &Path) -> Result<Vec<String>> {
    Ok(status_entries(worktree)?
        .into_iter()
        .map(|(_, path)| path)
        .collect())
}

/// `(XY status code, path)` for every entry of `git status --porcelain`;
/// untracked files have the code `??`.
pub fn status_entries(worktree: &Path) -> Result<Vec<(String, String)>> {
    let out = run(
        worktree,
        ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let mut entries = Vec::new();
    let mut records = out.split(|b| *b == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        if record.len() <= 3 {
            continue;
        }
        let code = String::from_utf8_lossy(&record[..2]).into_owned();
        // Renames and copies are followed by a second record with the
        // original path, which belongs to the same entry.
        if code.starts_with(['R', 'C']) {
            records.next();
        }
        entries.push((code, String::from_utf8_lossy(&record[3..]).into_owned()));
    }
    Ok(entries)
}

/// Number of commits reachable from `tip` but not from `base`.
pub fn commits_ahead(repo: &Repo, base: &CommitId, tip: &CommitId) -> Result<u64> {
    let range = format!("{base}..{tip}");
    let out = run(&repo.toplevel, ["rev-list", "--count", &range])?;
    String::from_utf8_lossy(&out)
        .trim()
        .parse()
        .map_err(|_| GitError::Parse {
            args: format!("rev-list --count {range}"),
            detail: "expected a number".into(),
        })
}

/// One entry of `git worktree list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// The checked-out branch (without `refs/heads/`); `None` when detached.
    pub branch: Option<String>,
    /// `git worktree lock`ed; agent tools lock a worktree while they use it.
    pub locked: bool,
}

/// Root of the worktree containing `dir` (physical path), or `None` outside
/// any repository.
pub fn toplevel(dir: &Path) -> Result<Option<PathBuf>> {
    match run(
        dir,
        ["rev-parse", "--path-format=absolute", "--show-toplevel"],
    ) {
        Ok(out) => Ok(Some(path_from_bytes(out.trim_ascii_end()))),
        Err(GitError::Command { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The commit checked out in `worktree`.
pub fn head(worktree: &Path) -> Result<CommitId> {
    let out = run(worktree, ["rev-parse", "--verify", "HEAD"])?;
    Ok(CommitId(String::from_utf8_lossy(&out).trim().to_owned()))
}

/// Number of commits reachable from `worktree`'s HEAD but from no branch, tag
/// or remote-tracking ref: work that removing the worktree (and with it its
/// HEAD and reflog) would orphan, e.g. commits made on a detached HEAD.
pub fn unreferenced_commits(worktree: &Path) -> Result<u64> {
    let out = run(
        worktree,
        [
            "rev-list",
            "--count",
            "HEAD",
            "--not",
            "--branches",
            "--tags",
            "--remotes",
        ],
    )?;
    String::from_utf8_lossy(&out)
        .trim()
        .parse()
        .map_err(|_| GitError::Parse {
            args: "rev-list --count HEAD --not --branches --tags --remotes".into(),
            detail: "expected a number".into(),
        })
}

/// Points `refname` at `commit`, creating it if needed.
pub fn update_ref(repo: &Repo, refname: &str, commit: &CommitId) -> Result<()> {
    run(&repo.toplevel, ["update-ref", refname, commit.as_str()]).map(drop)
}

/// Whether `name` is a valid branch name (`git check-ref-format --branch`).
pub fn is_valid_branch_name(name: &str) -> Result<bool> {
    match run(Path::new("."), ["check-ref-format", "--branch", name]) {
        Ok(_) => Ok(true),
        Err(GitError::Command { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Paths git ignores in `worktree` (`git status --ignored`).
pub fn ignored_paths(worktree: &Path) -> Result<Vec<String>> {
    let out = run(
        worktree,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--ignored=matching",
            "--untracked-files=normal",
        ],
    )?;
    Ok(out
        .split(|b| *b == 0)
        .filter_map(|r| r.strip_prefix(b"!! "))
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

fn same_path(a: &Path, b: &Path) -> bool {
    let resolve = |p: &Path| crate::sys::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    a == b || resolve(a) == resolve(b)
}
pub fn version() -> Result<String> {
    let out = run(Path::new("."), ["--version"])?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

fn main_worktree(dir: &Path) -> Result<Option<PathBuf>> {
    let out = run(dir, ["worktree", "list", "--porcelain", "-z"])?;
    let first = out.split(|b| *b == 0).next().unwrap_or_default();
    let is_bare = out
        .split(|b| *b == 0)
        .take_while(|r| !r.is_empty())
        .any(|r| r == b"bare");
    Ok(first
        .strip_prefix(b"worktree ")
        .filter(|_| !is_bare)
        .map(path_from_bytes))
}

/// Parses NUL-terminated `<mode> SP <type|oid> ...` records from `ls-tree -z`
/// (`<mode> <type> <oid>\t<path>`) or `ls-files -s -z`
/// (`<mode> <oid> <stage>\t<path>`).
fn parse_entries(out: &[u8], index: bool) -> std::result::Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| "missing tab separator".to_string())?;
        let (meta, path) = (&record[..tab], &record[tab + 1..]);
        let meta = std::str::from_utf8(meta).map_err(|e| e.to_string())?;
        let fields: Vec<&str> = meta.split(' ').collect();
        let [mode, second, third] = fields[..] else {
            return Err(format!("unexpected record {meta:?}"));
        };
        let mode = u32::from_str_radix(mode, 8).map_err(|e| format!("bad mode {mode:?}: {e}"))?;
        let (oid, stage) = if index {
            (
                second,
                third
                    .parse()
                    .map_err(|e| format!("bad stage {third:?}: {e}"))?,
            )
        } else {
            (third, 0)
        };
        entries.push(Entry {
            mode,
            oid: oid.to_owned(),
            stage,
            path: path_from_bytes(path),
        });
    }
    Ok(entries)
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

fn os(s: &str) -> OsString {
    OsString::from(s)
}

fn command<I, S>(dir: &Path, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        // Ferry parses git's output; a user's pager or locale must not leak in.
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    cmd
}

fn run<I, S>(dir: &Path, args: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let output = command(dir, &args)
        .output()
        .map_err(|e| map_spawn_error(&e))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let display: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        Err(command_error(&display, &output))
    }
}

fn command_error<S: AsRef<str>>(args: &[S], output: &Output) -> GitError {
    // git's own `hint:` lines suggest git commands, which mislead next to
    // Ferry's hints.
    let stderr = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|l| !l.starts_with("hint:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    GitError::Command {
        args: args.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(" "),
        stderr: if stderr.is_empty() {
            format!("exited with {}", output.status)
        } else {
            stderr
        },
    }
}

fn map_spawn_error(e: &std::io::Error) -> GitError {
    if e.kind() == std::io::ErrorKind::NotFound {
        GitError::NotInstalled
    } else {
        GitError::Command {
            args: String::new(),
            stderr: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ls_tree() {
        let out = b"100644 blob 1111111111111111111111111111111111111111\tsrc/lib.rs\0\
                    120000 blob 2222222222222222222222222222222222222222\tlink\0\
                    160000 commit 3333333333333333333333333333333333333333\tvendor/sub\0";
        let entries = parse_entries(out, false).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].mode, 0o100_644);
        assert!(entries[0].is_regular_file());
        assert_eq!(entries[0].oid, "1111111111111111111111111111111111111111");
        assert_eq!(entries[0].path, PathBuf::from("src/lib.rs"));
        assert!(!entries[1].is_regular_file());
        assert_eq!(entries[2].mode, 0o160_000);
    }

    #[test]
    fn parses_ls_files_with_stages_and_odd_paths() {
        let out = b"100755 4444444444444444444444444444444444444444 0\tbin/run me\0\
                    100644 5555555555555555555555555555555555555555 2\tconflict\twith tab\0";
        let entries = parse_entries(out, true).unwrap();
        assert_eq!(entries[0].mode, 0o100_755);
        assert_eq!(entries[0].stage, 0);
        assert_eq!(entries[0].path, PathBuf::from("bin/run me"));
        assert_eq!(entries[1].stage, 2);
        assert_eq!(entries[1].path, PathBuf::from("conflict\twith tab"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_entries(b"nonsense\0", false).is_err());
        assert!(parse_entries(b"10x644 blob abc\tp\0", false).is_err());
    }
}
