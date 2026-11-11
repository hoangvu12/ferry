//! Making a fresh checkout's unchanged files look as old as the base they
//! fork from, without ever hiding a real change from Cargo.
//!
//! Cargo decides freshness by comparing source mtimes with its fingerprint
//! files. A new worktree gets "now" as the mtime of every file, so without
//! intervention every crate looks dirty. Ferry sets ("stamps") the mtime of a
//! file to [`STAMP_SECS`] — far older than any base artifact — but only when
//! that file is proven byte-identical to the file the base compiled. Any file
//! that differs keeps its real (new) mtime, so Cargo rebuilds what depends on
//! it.
//!
//! INVARIANT: a file is stamped only if all of the following hold:
//! 1. it is a tracked regular file at merge stage 0 in the agent's index,
//! 2. the base commit's tree has the same path with the same mode and blob id,
//! 3. the agent's working file and the base checkout's working file have
//!    identical bytes (this catches smudge filters, `core.autocrlf`,
//!    post-checkout hooks, and anything else that makes the working tree
//!    differ from the blob), and
//! 4. its metadata has not changed between verification and stamping, and
//! 5. Cargo cannot reach it through a symlink that leads elsewhere in the
//!    agent than in the base (see `changed_link_regions`).
//!
//! A directory (any ancestor of a tracked path, up to and including the
//! worktree root) is stamped only if its entry names equal those of the same
//! directory in the base checkout, and condition 5 holds for it. Cargo's
//! `rerun-if-changed=<dir>` takes the recursive max mtime of the directory,
//! its subdirectories and files, and a deleted file is only visible as its
//! parent directory's mtime change.
//!
//! Leaving a file unstamped makes Cargo rebuild only where it compares
//! mtimes. With checksum freshness it compares content, and a file reached
//! through a changed symlink is identical to the base's, so the units that
//! read one also get their dep-info invalidated ([`link_rebuilds`]).
//!
//! [`StampPlan`] can only be constructed by [`plan`], so code elsewhere cannot
//! stamp an unverified path. Stamping happens once, when an agent is
//! spawned: re-stamping a tree an agent has worked in would hide deletions.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs::{self, File, Metadata};
use std::io::{self, ErrorKind, Read};
use std::num::NonZero;
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::FreshnessPref;
use crate::error::{Error, IoContext, Result};
use crate::git::Entry;
use crate::sys;
use crate::toolchain::Toolchain;

mod relocation;

pub use relocation::{
    Capture, Origin, RELOCATION_RULES, RelocationPlan, RelocationReport, Removal, apply_relocation,
    invalidate_dep_info, link_rebuilds, plan_relocation,
};

/// The mtime given to stamped files: 2023-11-14T22:13:20Z. Any fixed time
/// earlier than every base build works; a fixed value keeps stamping
/// deterministic across bases and agents.
pub const STAMP_SECS: u64 = 1_700_000_000;

const STAMP: (i64, i64) = (STAMP_SECS.cast_signed(), 0);

/// Bytes compared per read when checking a file against the base.
const CHUNK: usize = 64 * 1024;

/// Below this many files per thread, spawning threads costs more than it saves.
const MIN_FILES_PER_THREAD: usize = 64;

const SYMLINK_MODE: u32 = 0o120_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FreshnessMode {
    /// Cargo compares content hashes (`-Zchecksum-freshness`). Stamping still
    /// runs, for inputs Cargo keeps checking by mtime.
    Checksum,
    /// Cargo compares mtimes; stamping is what makes reuse possible.
    Mtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// The path does not exist in the base commit.
    NotInBase,
    /// Same path, different git mode (e.g. became executable or a symlink).
    ModeDiffers,
    /// Same path, different blob.
    BlobDiffers,
    /// The index has the path at a non-zero merge stage.
    Unmerged,
    /// Symlinks and submodules are never stamped.
    NotRegularFile,
    /// Blob ids match but the working-tree bytes differ from the base's.
    ContentDiffers,
    /// The file is missing from the agent or the base working tree.
    Missing,
    /// Identical to the base, but Cargo may reach it through a symlink that
    /// leads elsewhere in the agent than in the base.
    ReachedThroughChangedSymlink,
}

/// Files and directories proven identical to the base, ready to stamp.
#[derive(Debug)]
pub struct StampPlan {
    root: PathBuf,
    identical: Vec<Verified>,
    skipped: Vec<(PathBuf, SkipReason)>,
    dirs: Vec<Verified>,
    skipped_dirs: Vec<(PathBuf, SkipReason)>,
    tracked: usize,
    /// Some symlink leads to another place in the checkout in the agent than
    /// in the base, or into or out of it.
    links_changed: bool,
}

/// A verified path plus what was observed when it was verified, so [`apply`]
/// can detect a change in between.
#[derive(Debug)]
struct Verified {
    path: PathBuf,
    seen: Observed,
}

/// Identity and change indicators of an inode. `ctime` is included because a
/// writer can restore `mtime` (as stamping itself does) but not `ctime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Observed {
    id: sys::FileId,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Observed {
    fn from(stat: sys::Stat) -> Self {
        Self {
            id: stat.id,
            len: stat.len,
            mtime: stat.mtime,
            ctime: stat.ctime,
        }
    }

    /// `path` itself, whose `symlink_metadata` is `meta`.
    fn at(path: &Path, meta: &Metadata) -> Result<Self> {
        sys::stat(path, meta).map(Self::from).at("stat", path)
    }

    fn of_file(file: &File, meta: &Metadata, path: &Path) -> Result<Self> {
        sys::stat_file(file, meta).map(Self::from).at("stat", path)
    }
}

type Verdict<T> = std::result::Result<T, SkipReason>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StampReport {
    pub tracked: usize,
    pub stamped: usize,
    /// Tracked files left unstamped, including any that changed between
    /// verification and stamping.
    pub skipped: usize,
    /// Ancestor directories of tracked paths, including the worktree root.
    pub dirs: usize,
    pub dirs_stamped: usize,
}

impl StampPlan {
    pub fn skipped(&self) -> &[(PathBuf, SkipReason)] {
        &self.skipped
    }

    /// Whether some symlink leads to another place in the checkout in the
    /// agent than in the base, or into or out of it. Leaving what it reaches
    /// unstamped is enough only where Cargo compares mtimes; see
    /// [`link_rebuilds`] for checksum freshness.
    pub fn links_changed(&self) -> bool {
        self.links_changed
    }
}

#[cfg(test)]
impl StampPlan {
    /// The agent worktree, canonicalized.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn identical(&self) -> impl Iterator<Item = &Path> {
        self.identical.iter().map(|v| v.path.as_path())
    }

    /// Directories whose entry names match the base; `""` is the root.
    pub fn dirs(&self) -> impl Iterator<Item = &Path> {
        self.dirs.iter().map(|v| v.path.as_path())
    }

    pub fn skipped_dirs(&self) -> &[(PathBuf, SkipReason)] {
        &self.skipped_dirs
    }

    pub fn tracked(&self) -> usize {
        self.tracked
    }
}

/// Decides which of the agent's tracked files, and which of their ancestor
/// directories, may be stamped.
///
/// `base_tree` is `git ls-tree -r` of the base commit, `base_src` the base's
/// checkout, `agent_index` is `git ls-files -s` of the agent worktree at
/// `agent_root`. Comparing file contents may use multiple threads.
pub fn plan(
    base_tree: &[Entry],
    base_src: &Path,
    agent_index: &[Entry],
    agent_root: &Path,
) -> Result<StampPlan> {
    let root = sys::canonicalize(agent_root).at("resolve", agent_root)?;
    let base_root = sys::canonicalize(base_src).at("resolve", base_src)?;
    let base: HashMap<&Path, &Entry> = base_tree.iter().map(|e| (e.path.as_path(), e)).collect();

    // `None` marks a path with an entry at a non-zero merge stage.
    let mut index: BTreeMap<&Path, Option<&Entry>> = BTreeMap::new();
    for entry in agent_index {
        check_relative(&entry.path)?;
        let slot = index.entry(entry.path.as_path()).or_insert(Some(entry));
        if entry.stage != 0 {
            *slot = None;
        }
    }
    let dirs = scan_dirs(&root, index.keys().copied())?;

    let mut skipped = Vec::new();
    let mut candidates = Vec::new();
    for (&path, &entry) in &index {
        let reason = index_skip(entry, base.get(path).copied())
            .or_else(|| (!parent_is_dir(&dirs, path)).then_some(SkipReason::Missing));
        match reason {
            Some(reason) => skipped.push((path.to_path_buf(), reason)),
            None => candidates.push(path),
        }
    }

    let verdicts = verify_files(&root, &base_root, &candidates)?;
    let mut identical = Vec::new();
    for (path, verdict) in candidates.into_iter().zip(verdicts) {
        match verdict {
            Ok(seen) => identical.push(Verified {
                path: path.to_path_buf(),
                seen,
            }),
            Err(reason) => skipped.push((path.to_path_buf(), reason)),
        }
    }

    // Symlinks of the base commit, and any in the agent's working tree at a
    // tracked path or one of its directories.
    let mut links: BTreeSet<&Path> = base_tree
        .iter()
        .filter(|e| e.mode == SYMLINK_MODE)
        .map(|e| e.path.as_path())
        .collect();
    let unverified = skipped
        .iter()
        .map(|(path, _)| path.as_path())
        .filter(|path| parent_is_dir(&dirs, path))
        .chain(dirs.iter().filter_map(|(dir, state)| {
            matches!(state, Err(SkipReason::NotRegularFile)).then_some(*dir)
        }));
    links.extend(unverified.filter(|path| is_symlink(&root.join(path))));
    let regions = changed_link_regions(&root, &base_root, &links);
    let (reached, identical): (Vec<_>, Vec<_>) = identical
        .into_iter()
        .partition(|file| within(&regions, &file.path));
    skipped.extend(
        reached
            .into_iter()
            .map(|file| (file.path, SkipReason::ReachedThroughChangedSymlink)),
    );
    skipped.sort_by(|a, b| a.0.cmp(&b.0));

    let mut verified_dirs = Vec::new();
    let mut skipped_dirs = Vec::new();
    for (dir, state) in dirs {
        let verdict = if state.is_ok() && within(&regions, dir) {
            Err(SkipReason::ReachedThroughChangedSymlink)
        } else {
            verify_dir(&root, &base_root, dir, state)?
        };
        match verdict {
            Ok(seen) => verified_dirs.push(Verified {
                path: dir.to_path_buf(),
                seen,
            }),
            Err(reason) => skipped_dirs.push((dir.to_path_buf(), reason)),
        }
    }

    Ok(StampPlan {
        root,
        identical,
        skipped,
        dirs: verified_dirs,
        skipped_dirs,
        tracked: index.len(),
        links_changed: !regions.is_empty(),
    })
}

/// Stamps every file, then every directory, in `plan` whose metadata still
/// matches what was verified. Symlinks are never followed.
pub fn apply(plan: &StampPlan) -> Result<StampReport> {
    let mut report = StampReport {
        tracked: plan.tracked,
        skipped: plan.skipped.len(),
        dirs: plan.dirs.len() + plan.skipped_dirs.len(),
        ..StampReport::default()
    };
    for file in &plan.identical {
        if stamp_unchanged(&plan.root, file)? {
            report.stamped += 1;
        } else {
            report.skipped += 1;
        }
    }
    let mut dirs: Vec<&Verified> = plan.dirs.iter().collect();
    dirs.sort_by_key(|d| Reverse(d.path.components().count()));
    for dir in dirs {
        if stamp_unchanged(&plan.root, dir)? {
            report.dirs_stamped += 1;
        }
    }
    Ok(report)
}

/// Makes every regular file and directory that `plan` skipped for differing
/// from the base, or for being reached through a changed symlink, at least
/// as new as `at_least`. Symlinks, submodules and missing paths are left
/// alone, and nothing is reached through a symlink.
///
/// Checkout gives such paths the current time, but a post-checkout hook
/// that restores commit times, or clock skew, can leave them older than the
/// base's artifacts, and Cargo would keep stale units. Returns the number
/// of paths changed.
pub fn freshen_skipped(plan: &StampPlan, at_least: SystemTime) -> Result<usize> {
    let Ok(since_epoch) = at_least.duration_since(UNIX_EPOCH) else {
        return Ok(0);
    };
    let time = (
        i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX),
        i64::from(since_epoch.subsec_nanos()),
    );
    let mut freshened = 0;
    for (rel, reason) in plan.skipped.iter().chain(&plan.skipped_dirs) {
        if matches!(reason, SkipReason::NotRegularFile | SkipReason::Missing) {
            continue;
        }
        let path = under(&plan.root, rel);
        if !parent_is_real(&plan.root, &path)? {
            continue;
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() || meta.is_dir() => meta,
            Ok(_) => continue,
            Err(e) if is_gone(&e) => continue,
            Err(e) => return Err(e).at("stat", &path),
        };
        if meta.modified().at("stat", &path)? < at_least {
            set_times(&path, time)?;
            freshened += 1;
        }
    }
    Ok(freshened)
}

/// Stamps every tracked regular file of a freshly created base checkout, and
/// every ancestor directory of a tracked path.
///
/// The base checkout is the reference everything else is compared against,
/// so it needs no comparison. Must only be called before the base's first
/// build. Single-threaded: the caller enters a user namespace next, which
/// requires that no other threads exist. Returns the number of files and
/// directories stamped.
pub(crate) fn stamp_base_checkout(base_src: &Path, index: &[Entry]) -> Result<usize> {
    let root = sys::canonicalize(base_src).at("resolve", base_src)?;
    for entry in index {
        check_relative(&entry.path)?;
    }
    let dirs = scan_dirs(&root, index.iter().map(|e| e.path.as_path()))?;
    let mut stamped = 0;
    for entry in index.iter().filter(|e| e.stage == 0 && e.is_regular_file()) {
        if !parent_is_dir(&dirs, &entry.path) {
            continue;
        }
        let path = root.join(&entry.path);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {
                set_stamp(&path)?;
                stamped += 1;
            }
            // Sparse checkouts leave tracked files out of the working tree.
            Err(e) if !is_gone(&e) => return Err(e).at("stat", &path),
            _ => {}
        }
    }
    let mut real_dirs: Vec<&Path> = dirs
        .iter()
        .filter(|(_, state)| state.is_ok())
        .map(|(dir, _)| *dir)
        .collect();
    real_dirs.sort_by_key(|d| Reverse(d.components().count()));
    for dir in real_dirs {
        set_stamp(&under(&root, dir))?;
        stamped += 1;
    }
    Ok(stamped)
}

/// Picks the freshness mode for a new base.
pub fn resolve_mode(pref: FreshnessPref, toolchain: &Toolchain) -> Result<FreshnessMode> {
    match pref {
        FreshnessPref::Auto | FreshnessPref::Checksum if toolchain.is_nightly() => {
            Ok(FreshnessMode::Checksum)
        }
        FreshnessPref::Auto | FreshnessPref::Mtime => Ok(FreshnessMode::Mtime),
        FreshnessPref::Checksum => Err(Error::Refused(format!(
            "checksum freshness needs a nightly toolchain (cargo#14136), but rustc {} is not \
             nightly; use freshness mode `auto` or `mtime`",
            toolchain.release
        ))),
    }
}

/// Environment variables every build of a base (and its agents) must see for
/// `mode`.
pub fn cargo_env(mode: FreshnessMode) -> &'static [(&'static str, &'static str)] {
    match mode {
        // Nightlies from 2026-09-02 (cargo#17382) silently fall back to mtime
        // with only the first variable; older ones ignore the second.
        FreshnessMode::Checksum => &[
            ("CARGO_UNSTABLE_CHECKSUM_FRESHNESS", "true"),
            ("CARGO_BUILD_FINGERPRINT", "content"),
        ],
        FreshnessMode::Mtime => &[],
    }
}

/// Git never reports such paths; refusing them keeps every stamp inside the
/// worktree even if an index parser goes wrong.
fn check_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(Error::Refused(format!(
            "git reported the index path `{}`, which is not a plain relative path",
            path.display()
        )));
    }
    Ok(())
}

fn index_skip(entry: Option<&Entry>, base: Option<&Entry>) -> Option<SkipReason> {
    let Some(entry) = entry else {
        return Some(SkipReason::Unmerged);
    };
    if !entry.is_regular_file() {
        return Some(SkipReason::NotRegularFile);
    }
    match base {
        None => Some(SkipReason::NotInBase),
        Some(b) if b.mode != entry.mode => Some(SkipReason::ModeDiffers),
        Some(b) if b.oid != entry.oid => Some(SkipReason::BlobDiffers),
        Some(_) => None,
    }
}

/// `lstat`s every ancestor directory of `tracked`. A directory below a
/// symlink or non-directory counts as missing, so nothing is ever reached
/// through a symlink.
fn scan_dirs<'a>(
    root: &Path,
    tracked: impl Iterator<Item = &'a Path>,
) -> Result<BTreeMap<&'a Path, Verdict<Metadata>>> {
    // Paths order by component, so a parent is always visited before its
    // descendants.
    let ancestors: BTreeSet<&Path> = tracked.flat_map(|p| p.ancestors().skip(1)).collect();
    let mut dirs = BTreeMap::new();
    for dir in ancestors {
        let state = if dir.parent().is_some() && !parent_is_dir(&dirs, dir) {
            Err(SkipReason::Missing)
        } else {
            let path = under(root, dir);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => Ok(meta),
                Ok(_) => Err(SkipReason::NotRegularFile),
                Err(e) if is_gone(&e) => Err(SkipReason::Missing),
                Err(e) => return Err(e).at("stat", &path),
            }
        };
        dirs.insert(dir, state);
    }
    Ok(dirs)
}

fn parent_is_dir(dirs: &BTreeMap<&Path, Verdict<Metadata>>, path: &Path) -> bool {
    path.parent()
        .is_some_and(|p| matches!(dirs.get(p), Some(Ok(_))))
}

/// What a path leads to in one checkout.
struct LinkEnd {
    link: bool,
    /// Relative to the checkout root; `None` if the path does not resolve to
    /// anything inside it, so nothing stampable is reached through it there.
    target: Option<PathBuf>,
    dir: bool,
}

impl LinkEnd {
    fn of(root: &Path, rel: &Path) -> Self {
        let path = root.join(rel);
        let target = sys::canonicalize(&path)
            .ok()
            .and_then(|t| t.strip_prefix(root).ok().map(Path::to_path_buf));
        Self {
            link: is_symlink(&path),
            dir: target.as_ref().is_some_and(|t| under(root, t).is_dir()),
            target,
        }
    }
}

/// The paths, each with everything beneath it, that Cargo may reach through
/// one of `links` (symlinks in either checkout) that leads elsewhere in the
/// agent than in the base.
///
/// Both targets count: Cargo records a rustc input (`include_str!`,
/// `#[path]`) by its canonical path in the base when that lies inside the
/// input's package or the target dir, but follows a build script's
/// `rerun-if-changed` path through the link in the agent. `..`
/// after a link to a directory climbs out of it, so that region reaches up
/// to where the two targets' ancestors meet. A symlink inside another link's
/// region leads on to wherever it points, which the other checkout need not
/// mirror, so unless that is a file inside the regions the whole tree is the
/// region.
fn changed_link_regions(root: &Path, base_root: &Path, links: &BTreeSet<&Path>) -> Vec<PathBuf> {
    let ends: Vec<(&Path, [LinkEnd; 2])> = links
        .iter()
        .map(|&link| {
            (
                link,
                [LinkEnd::of(root, link), LinkEnd::of(base_root, link)],
            )
        })
        .collect();
    let changed: Vec<(&Path, Vec<PathBuf>)> = ends
        .iter()
        .filter(|(_, [agent, base])| agent.target != base.target)
        .map(|(link, [agent, base])| (*link, link_regions(agent, base)))
        .collect();
    let regions: Vec<PathBuf> = changed
        .iter()
        .flat_map(|(_, r)| r.iter().cloned())
        .collect();
    let escapes = ends.iter().any(|(link, pair)| {
        changed
            .iter()
            .any(|(other, r)| other != link && within(r, link))
            && pair.iter().any(|end| {
                end.link
                    && end
                        .target
                        .as_ref()
                        .is_some_and(|t| end.dir || !within(&regions, t))
            })
    });
    if escapes {
        vec![PathBuf::new()]
    } else {
        regions
    }
}

/// The regions of one link whose two ends lead to different places.
fn link_regions(agent: &LinkEnd, base: &LinkEnd) -> Vec<PathBuf> {
    [(agent, base), (base, agent)]
        .into_iter()
        .filter_map(|(end, other)| {
            let target = end.target.as_deref()?;
            let region = match other.target.as_deref() {
                Some(other) if end.dir => divergence(target, other),
                _ => target,
            };
            Some(region.to_path_buf())
        })
        .collect()
}

/// The highest ancestor of `path`, or `path` itself, that differs from the
/// ancestor of `other` as many levels up; the root if they never meet.
fn divergence<'a>(mut path: &'a Path, mut other: &Path) -> &'a Path {
    let mut top = path;
    while path != other {
        top = path;
        let (Some(up), Some(other_up)) = (path.parent(), other.parent()) else {
            return Path::new("");
        };
        (path, other) = (up, other_up);
    }
    top
}

fn within(regions: &[PathBuf], path: &Path) -> bool {
    regions.iter().any(|region| path.starts_with(region))
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink())
}

fn verify_dir(
    root: &Path,
    base_root: &Path,
    dir: &Path,
    state: Verdict<Metadata>,
) -> Result<Verdict<Observed>> {
    let meta = match state {
        Ok(meta) => meta,
        Err(reason) => return Ok(Err(reason)),
    };
    let agent = entry_names(&under(root, dir))?;
    let base = entry_names(&under(base_root, dir))?;
    Ok(match (agent, base) {
        (Some(agent), Some(base)) if agent == base => Ok(Observed::at(&under(root, dir), &meta)?),
        (Some(_), Some(_)) => Err(SkipReason::ContentDiffers),
        _ => Err(SkipReason::Missing),
    })
}

fn entry_names(dir: &Path) -> Result<Option<Vec<OsString>>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if is_gone(&e) => return Ok(None),
        Err(e) => return Err(e).at("read directory", dir),
    };
    let mut names = entries
        .map(|e| e.map(|e| e.file_name()))
        .collect::<io::Result<Vec<_>>>()
        .at("read directory", dir)?;
    names.sort_unstable();
    Ok(Some(names))
}

/// Compares each of `paths` between the two trees, in parallel. The result
/// is in the order of `paths`.
fn verify_files(root: &Path, base_root: &Path, paths: &[&Path]) -> Result<Vec<Verdict<Observed>>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let threads = thread::available_parallelism()
        .map_or(1, NonZero::get)
        .min(paths.len().div_ceil(MIN_FILES_PER_THREAD));
    let per_thread = paths.len().div_ceil(threads);
    thread::scope(|scope| {
        let workers: Vec<_> = paths
            .chunks(per_thread)
            .map(|chunk| {
                scope.spawn(move || {
                    let mut bufs = [Vec::with_capacity(CHUNK), Vec::with_capacity(CHUNK)];
                    chunk
                        .iter()
                        .map(|p| verify_file(&root.join(p), &base_root.join(p), &mut bufs))
                        .collect::<Result<Vec<_>>>()
                })
            })
            .collect();
        let mut verdicts = Vec::with_capacity(paths.len());
        for worker in workers {
            let part = worker
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
            verdicts.extend(part);
        }
        Ok(verdicts)
    })
}

fn verify_file(agent: &Path, base: &Path, bufs: &mut [Vec<u8>; 2]) -> Result<Verdict<Observed>> {
    let (mut agent_file, agent_meta) = match open_regular(agent)? {
        Ok(opened) => opened,
        Err(reason) => return Ok(Err(reason)),
    };
    let (mut base_file, base_meta) = match open_regular(base)? {
        Ok(opened) => opened,
        Err(reason) => return Ok(Err(reason)),
    };
    if agent_meta.len() != base_meta.len() {
        return Ok(Err(SkipReason::ContentDiffers));
    }
    let [a, b] = bufs;
    loop {
        a.clear();
        b.clear();
        let n = read_chunk(&mut agent_file, a).at("read", agent)?;
        read_chunk(&mut base_file, b).at("read", base)?;
        if a != b {
            return Ok(Err(SkipReason::ContentDiffers));
        }
        if n < CHUNK {
            return Ok(Ok(Observed::of_file(&agent_file, &agent_meta, agent)?));
        }
    }
}

fn read_chunk(file: &mut File, buf: &mut Vec<u8>) -> io::Result<usize> {
    file.take(CHUNK as u64).read_to_end(buf)
}

/// Opens `path` without following a symlink in its last component. The
/// metadata comes from the open file, so it describes the bytes compared.
/// Non-blocking, so a FIFO cannot hang the open.
fn open_regular(path: &Path) -> Result<Verdict<(File, Metadata)>> {
    let file = match sys::open_nofollow(path) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(Err(SkipReason::NotRegularFile)),
        Err(e) if is_gone(&e) => return Ok(Err(SkipReason::Missing)),
        Err(e) => return Err(e).at("open", path),
    };
    let meta = file.metadata().at("stat", path)?;
    if !meta.is_file() {
        return Ok(Err(SkipReason::NotRegularFile));
    }
    Ok(Ok((file, meta)))
}

/// Stamps a verified path unless it changed since it was verified.
fn stamp_unchanged(root: &Path, verified: &Verified) -> Result<bool> {
    let path = under(root, &verified.path);
    match fs::symlink_metadata(&path) {
        Ok(meta) if Observed::at(&path, &meta)? == verified.seen => {}
        Ok(_) => return Ok(false),
        Err(e) if is_gone(&e) => return Ok(false),
        Err(e) => return Err(e).at("stat", &path),
    }
    set_stamp(&path)?;
    Ok(true)
}

fn set_stamp(path: &Path) -> Result<()> {
    set_times(path, STAMP)
}

fn set_times(path: &Path, time: (i64, i64)) -> Result<()> {
    sys::set_times(path, time, time, true).at("set the mtime of", path)
}

/// Whether every directory from `root` down to the parent of `path`, a path
/// under `root` without `.` or `..`, is a real directory.
fn parent_is_real(root: &Path, path: &Path) -> Result<bool> {
    let parent = match path.parent() {
        Some(parent) if path != root => parent,
        _ => return Ok(true),
    };
    match sys::canonicalize(parent) {
        Ok(canonical) => Ok(canonical == parent),
        Err(e) if is_gone(&e) => Ok(false),
        Err(e) => Err(e).at("resolve", parent),
    }
}

fn under(root: &Path, rel: &Path) -> PathBuf {
    if rel.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    }
}

fn is_gone(e: &io::Error) -> bool {
    matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

/// Real subdirectories of `dir` (symlinks are not followed); none if `dir`
/// does not exist.
pub(crate) fn subdirs(dir: &Path) -> Result<Vec<PathBuf>> {
    entries_of_type(dir, fs::FileType::is_dir)
}

/// Regular files in `dir`; none if `dir` does not exist.
pub(crate) fn files(dir: &Path) -> Result<Vec<PathBuf>> {
    entries_of_type(dir, fs::FileType::is_file)
}

fn entries_of_type(dir: &Path, wanted: fn(&fs::FileType) -> bool) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if is_gone(&e) => return Ok(Vec::new()),
        Err(e) => return Err(e).at("read directory", dir),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.at("read directory", dir)?;
        if wanted(&entry.file_type().at("stat", entry.path())?) {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

pub(crate) fn is_real_dir(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.is_dir()),
        Err(e) if is_gone(&e) => Ok(false),
        Err(e) => Err(e).at("stat", path),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use super::*;
    use crate::git::{self, Repo};

    const STAMPED: (i64, i64) = (STAMP_SECS.cast_signed(), 0);

    /// A repository with one commit, checked out as a base worktree and an
    /// agent worktree, with git isolated from the user's configuration.
    struct Fixture {
        tmp: tempfile::TempDir,
        repo: PathBuf,
        base: PathBuf,
        agent: PathBuf,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Self {
            Self::with(files, |_| {})
        }

        /// `prepare` runs after `files` are staged and before the commit, so
        /// it can stage symlinks or raw index entries.
        fn with(files: &[(&str, &str)], prepare: impl FnOnce(&Self)) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = sys::canonicalize(tmp.path()).unwrap();
            let fx = Self {
                repo: root.join("repo"),
                base: root.join("base"),
                agent: root.join("agent"),
                tmp,
            };
            fs::create_dir(&fx.repo).unwrap();
            fx.git(&fx.repo, &["init", "--quiet"]);
            for (path, contents) in files {
                write(&fx.repo.join(path), contents);
            }
            fx.git(&fx.repo, &["add", "--all"]);
            prepare(&fx);
            fx.git(
                &fx.repo,
                &["commit", "--quiet", "--allow-empty", "-m", "base"],
            );
            let (base, agent) = (fx.base.to_str().unwrap(), fx.agent.to_str().unwrap());
            fx.git(
                &fx.repo,
                &["worktree", "add", "--quiet", "--detach", base, "HEAD"],
            );
            fx.git(
                &fx.repo,
                &["worktree", "add", "--quiet", "-b", "agent", agent, "HEAD"],
            );
            fx
        }

        #[cfg(unix)]
        fn outside(&self) -> PathBuf {
            self.repo.parent().unwrap().to_path_buf()
        }

        fn git(&self, dir: &Path, args: &[&str]) -> String {
            self.git_with_input(dir, args, "")
        }

        fn git_with_input(&self, dir: &Path, args: &[&str], input: &str) -> String {
            let mut child = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args([
                    "-c",
                    "user.name=Ferry Test",
                    "-c",
                    "user.email=ferry@example.invalid",
                ])
                .args([
                    "-c",
                    "init.defaultBranch=main",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(["-c", "core.autocrlf=false"])
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", self.tmp.path())
                .env("XDG_CONFIG_HOME", self.tmp.path())
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "git {args:?} failed: {stderr}");
            String::from_utf8(out.stdout).unwrap()
        }

        fn plan(&self) -> StampPlan {
            let repo = Repo::discover(&self.repo).unwrap();
            let head = repo.resolve_commit("HEAD").unwrap();
            let tree = repo.ls_tree(&head).unwrap();
            let index = git::ls_files(&self.agent).unwrap();
            plan(&tree, &self.base, &index, &self.agent).unwrap()
        }
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn mtime(path: &Path) -> (i64, i64) {
        let meta = fs::symlink_metadata(path).unwrap();
        sys::mtime(&meta)
    }

    fn names<'a>(paths: impl Iterator<Item = &'a Path>) -> Vec<String> {
        paths.map(|p| p.to_string_lossy().into_owned()).collect()
    }

    fn reasons(skipped: &[(PathBuf, SkipReason)]) -> Vec<(String, SkipReason)> {
        skipped
            .iter()
            .map(|(p, r)| (p.to_string_lossy().into_owned(), *r))
            .collect()
    }

    /// Coarse kernel timestamps can give a write the mtime and ctime the file
    /// already had; waiting a tick makes the change observable.
    fn let_clock_tick() {
        thread::sleep(Duration::from_millis(20));
    }

    #[test]
    fn identical_checkout_stamps_every_file_and_directory() {
        let fx = Fixture::new(&[
            ("Cargo.toml", "[package]\n"),
            ("src/lib.rs", "pub fn f() {}\n"),
            ("src/bin/main.rs", "fn main() {}\n"),
            ("assets/sub/a.txt", "a\n"),
        ]);
        let plan = fx.plan();
        assert_eq!(plan.root(), fx.agent);
        assert!(plan.skipped().is_empty(), "{:?}", plan.skipped());
        assert!(plan.skipped_dirs().is_empty(), "{:?}", plan.skipped_dirs());
        assert_eq!(
            names(plan.identical()),
            [
                "Cargo.toml",
                "assets/sub/a.txt",
                "src/bin/main.rs",
                "src/lib.rs"
            ]
        );
        assert_eq!(
            names(plan.dirs()),
            ["", "assets", "assets/sub", "src", "src/bin"]
        );

        let report = apply(&plan).unwrap();
        assert_eq!(
            report,
            StampReport {
                tracked: 4,
                stamped: 4,
                skipped: 0,
                dirs: 5,
                dirs_stamped: 5,
            }
        );
        for rel in [
            "Cargo.toml",
            "src/lib.rs",
            "src/bin/main.rs",
            "assets/sub/a.txt",
            "",
            "assets",
            "assets/sub",
            "src",
            "src/bin",
        ] {
            let meta = fs::symlink_metadata(under(&fx.agent, Path::new(rel))).unwrap();
            assert_eq!(sys::mtime(&meta), STAMPED, "mtime of {rel:?}");
            assert_eq!(sys::atime(&meta), STAMPED, "atime of {rel:?}");
        }
        assert_ne!(mtime(&fx.base.join("src/lib.rs")), STAMPED);
    }

    #[cfg(unix)]
    #[test]
    fn files_that_differ_from_the_base_are_skipped() {
        let big = "x".repeat(3 * CHUNK + 17);
        let fx = Fixture::new(&[
            ("same.rs", "same\n"),
            ("blob.rs", "blob\n"),
            ("mode.sh", "#!/bin/sh\n"),
            ("bytes.rs", "abc\n"),
            ("size.rs", "size\n"),
            ("gone.rs", "gone\n"),
            ("base_gone.rs", "base gone\n"),
            ("big_same.bin", &big),
            ("big_diff.bin", &big),
        ]);
        let agent = &fx.agent;
        write(&agent.join("new.rs"), "new\n");
        write(&agent.join("blob.rs"), "blob changed\n");
        fs::set_permissions(agent.join("mode.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fx.git(agent, &["add", "new.rs", "blob.rs", "mode.sh"]);
        // Same blob in the index, different working-tree bytes: what a smudge
        // filter or autocrlf produces.
        write(&agent.join("bytes.rs"), "acb\n");
        write(&agent.join("size.rs"), "size, longer\n");
        let mut tail_differs = big.clone();
        tail_differs.replace_range(big.len() - 1.., "y");
        write(&agent.join("big_diff.bin"), &tail_differs);
        fs::remove_file(agent.join("gone.rs")).unwrap();
        fs::remove_file(fx.base.join("base_gone.rs")).unwrap();

        let plan = fx.plan();
        assert_eq!(
            reasons(plan.skipped()),
            [
                ("base_gone.rs".into(), SkipReason::Missing),
                ("big_diff.bin".into(), SkipReason::ContentDiffers),
                ("blob.rs".into(), SkipReason::BlobDiffers),
                ("bytes.rs".into(), SkipReason::ContentDiffers),
                ("gone.rs".into(), SkipReason::Missing),
                ("mode.sh".into(), SkipReason::ModeDiffers),
                ("new.rs".into(), SkipReason::NotInBase),
                ("size.rs".into(), SkipReason::ContentDiffers),
            ]
        );
        assert_eq!(names(plan.identical()), ["big_same.bin", "same.rs"]);
        // The root lost gone.rs and gained new.rs.
        assert_eq!(
            reasons(plan.skipped_dirs()),
            [(String::new(), SkipReason::ContentDiffers)]
        );

        let report = apply(&plan).unwrap();
        assert_eq!((report.tracked, report.stamped, report.skipped), (10, 2, 8));
        assert_eq!(mtime(&agent.join("same.rs")), STAMPED);
        assert_eq!(mtime(&agent.join("big_same.bin")), STAMPED);
        for rel in [
            "",
            "new.rs",
            "blob.rs",
            "mode.sh",
            "bytes.rs",
            "size.rs",
            "big_diff.bin",
            "base_gone.rs",
        ] {
            assert_ne!(mtime(&under(agent, Path::new(rel))), STAMPED, "{rel:?}");
        }
    }

    #[test]
    fn unmerged_paths_are_skipped_once() {
        let fx = Fixture::new(&[("a.rs", "a\n"), ("b.rs", "b\n")]);
        let oid = fx.git(&fx.agent, &["rev-parse", "HEAD:a.rs"]);
        let oid = oid.trim();
        let zero = "0".repeat(oid.len());
        let conflict = format!(
            "0 {zero}\ta.rs\n100644 {oid} 1\ta.rs\n100644 {oid} 2\ta.rs\n100644 {oid} 3\ta.rs\n"
        );
        fx.git_with_input(&fx.agent, &["update-index", "--index-info"], &conflict);

        let plan = fx.plan();
        assert_eq!(plan.tracked(), 2);
        assert_eq!(
            reasons(plan.skipped()),
            [("a.rs".into(), SkipReason::Unmerged)]
        );
        assert_eq!(names(plan.identical()), ["b.rs"]);
        apply(&plan).unwrap();
        assert_ne!(mtime(&fx.agent.join("a.rs")), STAMPED);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_gitlinks_are_never_stamped_or_followed() {
        let fx = Fixture::with(&[("src/lib.rs", "x\n"), ("vendor/README", "v\n")], |fx| {
            symlink("../outside.txt", fx.repo.join("link")).unwrap();
            symlink("src", fx.repo.join("dirlink")).unwrap();
            fx.git(&fx.repo, &["add", "link", "dirlink"]);
            let gitlink = format!("160000,{},vendor/sub", "1".repeat(40));
            fx.git(
                &fx.repo,
                &["update-index", "--add", "--cacheinfo", &gitlink],
            );
        });
        let outside = fx.outside().join("outside.txt");
        write(&outside, "outside\n");
        let outside_before = mtime(&outside);
        let agent = &fx.agent;
        let unstampable = ["link", "dirlink", "vendor/sub"];
        let before: Vec<_> = unstampable.iter().map(|r| mtime(&agent.join(r))).collect();

        let plan = fx.plan();
        assert_eq!(
            reasons(plan.skipped()),
            [
                ("dirlink".into(), SkipReason::NotRegularFile),
                ("link".into(), SkipReason::NotRegularFile),
                ("vendor/sub".into(), SkipReason::NotRegularFile),
            ]
        );
        assert_eq!(names(plan.identical()), ["src/lib.rs", "vendor/README"]);
        assert_eq!(names(plan.dirs()), ["", "src", "vendor"]);

        apply(&plan).unwrap();
        let after: Vec<_> = unstampable.iter().map(|r| mtime(&agent.join(r))).collect();
        assert_eq!(before, after);
        assert_eq!(mtime(&outside), outside_before);
        assert_eq!(mtime(&agent.join("vendor")), STAMPED);
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_replaced_by_a_symlink_is_not_followed() {
        let fx = Fixture::new(&[("src/lib.rs", "x\n"), ("top.rs", "t\n")]);
        let elsewhere = fx.outside().join("elsewhere");
        write(&elsewhere.join("lib.rs"), "x\n");
        let before = (mtime(&elsewhere), mtime(&elsewhere.join("lib.rs")));
        fs::remove_dir_all(fx.agent.join("src")).unwrap();
        symlink(&elsewhere, fx.agent.join("src")).unwrap();

        let plan = fx.plan();
        assert_eq!(
            reasons(plan.skipped()),
            [("src/lib.rs".into(), SkipReason::Missing)]
        );
        assert_eq!(
            reasons(plan.skipped_dirs()),
            [("src".into(), SkipReason::NotRegularFile)]
        );
        apply(&plan).unwrap();
        assert_eq!(
            (mtime(&elsewhere), mtime(&elsewhere.join("lib.rs"))),
            before
        );
        assert_ne!(mtime(&fx.agent.join("src")), STAMPED);
        assert_eq!(mtime(&fx.agent.join("top.rs")), STAMPED);
    }

    #[cfg(unix)]
    /// Replaces the file or symlink `at` with a symlink to `target`.
    fn relink(target: &str, at: &Path) {
        fs::remove_file(at).unwrap();
        symlink(target, at).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn files_reached_through_a_changed_symlink_are_not_stamped() {
        let files = [
            ("a.txt", "a\n"),
            ("b.txt", "b\n"),
            ("c.txt", "c\n"),
            ("d.txt", "d\n"),
            ("e.txt", "e\n"),
            ("f.txt", "f\n"),
            ("lone.txt", "l\n"),
            ("plain.txt", "p\n"),
            ("untouched.txt", "u\n"),
        ];
        let fx = Fixture::with(&files, |fx| {
            for (link, target) in [
                ("retargeted", "a.txt"),
                ("was_link", "d.txt"),
                ("same", "e.txt"),
                ("same_edited", "f.txt"),
                ("out", "../outside1.txt"),
            ] {
                symlink(target, fx.repo.join(link)).unwrap();
            }
            fx.git(&fx.repo, &["add", "--all"]);
        });
        for outside in ["outside1.txt", "outside2.txt"] {
            write(&fx.outside().join(outside), "o\n");
        }
        let agent = &fx.agent;
        relink("b.txt", &agent.join("retargeted"));
        relink("c.txt", &agent.join("plain.txt"));
        relink("../outside2.txt", &agent.join("out"));
        symlink("lone.txt", agent.join("fresh")).unwrap();
        fs::remove_file(agent.join("was_link")).unwrap();
        write(&agent.join("was_link"), "now a file\n");
        write(&agent.join("f.txt"), "f edited\n");
        fx.git(agent, &["add", "--all"]);

        let plan = fx.plan();
        let reached = SkipReason::ReachedThroughChangedSymlink;
        assert_eq!(
            reasons(plan.skipped()),
            [
                ("a.txt".into(), reached),
                ("b.txt".into(), reached),
                ("c.txt".into(), reached),
                ("d.txt".into(), reached),
                ("f.txt".into(), SkipReason::BlobDiffers),
                ("fresh".into(), SkipReason::NotRegularFile),
                ("lone.txt".into(), reached),
                ("out".into(), SkipReason::NotRegularFile),
                ("plain.txt".into(), SkipReason::NotRegularFile),
                ("retargeted".into(), SkipReason::NotRegularFile),
                ("same".into(), SkipReason::NotRegularFile),
                ("same_edited".into(), SkipReason::NotRegularFile),
                ("was_link".into(), SkipReason::ModeDiffers),
            ]
        );
        assert_eq!(names(plan.identical()), ["e.txt", "untouched.txt"]);

        apply(&plan).unwrap();
        for rel in ["a.txt", "b.txt", "c.txt", "d.txt", "lone.txt"] {
            assert_ne!(mtime(&agent.join(rel)), STAMPED, "{rel}");
        }
        assert_eq!(mtime(&agent.join("e.txt")), STAMPED);
    }

    #[cfg(unix)]
    #[test]
    fn everything_reached_through_a_retargeted_directory_symlink_is_not_stamped() {
        let files = [
            ("v1/m.rs", "m\n"),
            ("v1/sub/n.rs", "n\n"),
            ("v2/m.rs", "m\n"),
            ("v2/sub/n.rs", "n\n"),
            ("rel/one/x/f.rs", "f\n"),
            ("rel/one/VERSION", "1\n"),
            ("rel/two/x/f.rs", "f\n"),
            ("rel/two/VERSION", "2\n"),
            ("top.rs", "t\n"),
        ];
        let fx = Fixture::with(&files, |fx| {
            symlink("v1", fx.repo.join("impl")).unwrap();
            symlink("rel/one/x", fx.repo.join("deep")).unwrap();
            fx.git(&fx.repo, &["add", "--all"]);
        });
        relink("v2", &fx.agent.join("impl"));
        relink("rel/two/x", &fx.agent.join("deep"));
        fx.git(&fx.agent, &["add", "--all"]);

        let plan = fx.plan();
        let reached = SkipReason::ReachedThroughChangedSymlink;
        let mut expected: Vec<(String, SkipReason)> = files[..8]
            .iter()
            .map(|(path, _)| ((*path).to_owned(), reached))
            .collect();
        expected.extend(["deep", "impl"].map(|l| (l.to_owned(), SkipReason::NotRegularFile)));
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(reasons(plan.skipped()), expected);
        assert_eq!(names(plan.identical()), ["top.rs"]);
        // `deep/../VERSION` reaches rel/one/VERSION in the base and
        // rel/two/VERSION in the agent; rel/ itself is the same for both.
        let dirs = [
            "rel/one",
            "rel/one/x",
            "rel/two",
            "rel/two/x",
            "v1",
            "v1/sub",
            "v2",
            "v2/sub",
        ];
        assert_eq!(
            reasons(plan.skipped_dirs()),
            dirs.map(|d| (d.to_owned(), reached))
        );
        assert_eq!(names(plan.dirs()), ["", "rel"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_leading_out_of_a_retargeted_directory_unstamps_everything() {
        let files = [
            ("v1/m.rs", "m\n"),
            ("v2/m.rs", "m\n"),
            ("shared/s.rs", "s\n"),
            ("top.rs", "t\n"),
        ];
        let fx = Fixture::with(&files, |fx| {
            symlink("v1", fx.repo.join("impl")).unwrap();
            symlink("../shared", fx.repo.join("v1/data")).unwrap();
            symlink("../shared", fx.repo.join("v2/data")).unwrap();
            fx.git(&fx.repo, &["add", "--all"]);
        });
        relink("v2", &fx.agent.join("impl"));
        fx.git(&fx.agent, &["add", "impl"]);

        let plan = fx.plan();
        assert_eq!(plan.identical().count(), 0);
        assert_eq!(plan.dirs().count(), 0);
        let reached = SkipReason::ReachedThroughChangedSymlink;
        for (path, _) in files {
            assert!(
                plan.skipped().contains(&(PathBuf::from(path), reached)),
                "{path}"
            );
        }
        assert_eq!(
            reasons(plan.skipped_dirs()),
            ["", "shared", "v1", "v2"].map(|d| (d.to_owned(), reached))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_replaced_by_a_symlink_into_the_tree_leaves_its_target_unstamped() {
        let fx = Fixture::new(&[
            ("src/lib.rs", "x\n"),
            ("src_old/lib.rs", "x\n"),
            ("top.rs", "t\n"),
        ]);
        fs::remove_dir_all(fx.agent.join("src")).unwrap();
        symlink("src_old", fx.agent.join("src")).unwrap();

        let plan = fx.plan();
        let reached = SkipReason::ReachedThroughChangedSymlink;
        assert_eq!(
            reasons(plan.skipped()),
            [
                ("src/lib.rs".into(), SkipReason::Missing),
                ("src_old/lib.rs".into(), reached),
            ]
        );
        assert_eq!(
            reasons(plan.skipped_dirs()),
            [
                ("src".into(), SkipReason::NotRegularFile),
                ("src_old".into(), reached),
            ]
        );
        assert_eq!(names(plan.identical()), ["top.rs"]);
    }

    #[test]
    fn divergence_climbs_to_where_both_ancestors_meet() {
        for (path, other, expected) in [
            ("p/v2", "p/v1", "p/v2"),
            ("q/a/b", "q/c/d", "q/a"),
            ("x", "y", "x"),
            ("d", "d/sub", ""),
            ("x/y", "z", ""),
        ] {
            assert_eq!(
                divergence(Path::new(path), Path::new(other)),
                Path::new(expected),
                "{path} vs {other}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_skipped_without_blocking() {
        let fx = Fixture::new(&[("pipe", "p\n"), ("ok.rs", "o\n")]);
        let pipe = fx.agent.join("pipe");
        fs::remove_file(&pipe).unwrap();
        let status = Command::new("mkfifo").arg(&pipe).status().unwrap();
        assert!(status.success());

        let plan = fx.plan();
        assert_eq!(
            reasons(plan.skipped()),
            [("pipe".into(), SkipReason::NotRegularFile)]
        );
        assert_eq!(names(plan.identical()), ["ok.rs"]);
    }

    #[test]
    fn paths_changed_after_planning_are_not_stamped() {
        let fx = Fixture::new(&[
            ("rewritten.rs", "a\n"),
            ("swap/replaced.rs", "b\n"),
            ("restored.rs", "c\n"),
            ("untouched.rs", "d\n"),
            ("dir/f.rs", "e\n"),
        ]);
        let agent = &fx.agent;
        let plan = fx.plan();
        assert_eq!(plan.identical().count(), 5);
        let_clock_tick();

        fs::write(agent.join("rewritten.rs"), "a\n").unwrap();
        write(&agent.join("swap/replaced.tmp"), "b\n");
        let replaced = agent.join("swap/replaced.rs");
        fs::rename(agent.join("swap/replaced.tmp"), &replaced).unwrap();
        let restored = agent.join("restored.rs");
        let original = fs::metadata(&restored).unwrap().modified().unwrap();
        fs::write(&restored, "c\n").unwrap();
        File::options()
            .write(true)
            .open(&restored)
            .unwrap()
            .set_modified(original)
            .unwrap();
        write(&agent.join("dir/new.rs"), "new\n");

        let report = apply(&plan).unwrap();
        assert_eq!(
            report,
            StampReport {
                tracked: 5,
                stamped: 2,
                skipped: 3,
                dirs: 3,
                dirs_stamped: 1,
            }
        );
        assert_eq!(mtime(&agent.join("untouched.rs")), STAMPED);
        assert_eq!(mtime(&agent.join("dir/f.rs")), STAMPED);
        assert_eq!(mtime(agent), STAMPED);
        for rel in [
            "rewritten.rs",
            "swap/replaced.rs",
            "restored.rs",
            "dir",
            "swap",
        ] {
            assert_ne!(mtime(&agent.join(rel)), STAMPED, "{rel}");
        }
        let restored_mtime = fs::metadata(&restored).unwrap().modified().unwrap();
        assert_eq!(restored_mtime, original);
    }

    #[cfg(unix)]
    /// Sets the atime and mtime of `path` itself, even of a symlink.
    fn set_secs(path: &Path, secs: i64) {
        let time = Timespec {
            tv_sec: secs,
            tv_nsec: 0,
        };
        set_times(path, time).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn skipped_paths_are_made_at_least_as_new_as_asked() {
        // As a hook restoring commit times leaves them.
        const OLD: i64 = 1_000_000_000;
        let fx = Fixture::with(
            &[
                ("same.rs", "s\n"),
                ("blob.rs", "b\n"),
                ("bytes.rs", "abc\n"),
                ("newer.rs", "n\n"),
                ("gone.rs", "g\n"),
                ("sub/a.rs", "a\n"),
                ("v1.txt", "1\n"),
                ("v2.txt", "2\n"),
                ("vendor/x.rs", "x\n"),
            ],
            |fx| {
                symlink("v1.txt", fx.repo.join("cur")).unwrap();
                symlink("same.rs", fx.repo.join("link")).unwrap();
                fx.git(&fx.repo, &["add", "--all"]);
            },
        );
        let agent = &fx.agent;
        write(&agent.join("blob.rs"), "b changed\n");
        write(&agent.join("newer.rs"), "n changed\n");
        write(&agent.join("new.rs"), "new\n");
        write(&agent.join("vendor/added.rs"), "added\n");
        relink("v2.txt", &agent.join("cur"));
        fx.git(agent, &["add", "--all"]);
        write(&agent.join("bytes.rs"), "acb\n");
        write(&agent.join("sub/untracked.txt"), "u\n");
        fs::remove_file(agent.join("gone.rs")).unwrap();
        // vendor/added.rs differs from the base, but is now reached through
        // a symlink out of the worktree.
        let outside = fx.outside().join("vendor");
        write(&outside.join("added.rs"), "added\n");
        fs::remove_dir_all(agent.join("vendor")).unwrap();
        symlink(&outside, agent.join("vendor")).unwrap();
        for rel in [
            "", "blob.rs", "bytes.rs", "cur", "link", "new.rs", "same.rs", "sub", "sub/a.rs",
            "v1.txt", "v2.txt", "vendor",
        ] {
            set_secs(&under(agent, Path::new(rel)), OLD);
        }
        set_secs(&agent.join("newer.rs"), 1_600_000_000);
        set_secs(&outside.join("added.rs"), OLD);

        let plan = fx.plan();
        let reached = SkipReason::ReachedThroughChangedSymlink;
        assert_eq!(
            reasons(plan.skipped()),
            [
                ("blob.rs".into(), SkipReason::BlobDiffers),
                ("bytes.rs".into(), SkipReason::ContentDiffers),
                ("cur".into(), SkipReason::NotRegularFile),
                ("gone.rs".into(), SkipReason::Missing),
                ("link".into(), SkipReason::NotRegularFile),
                ("new.rs".into(), SkipReason::NotInBase),
                ("newer.rs".into(), SkipReason::BlobDiffers),
                ("v1.txt".into(), reached),
                ("v2.txt".into(), reached),
                ("vendor/added.rs".into(), SkipReason::NotInBase),
                ("vendor/x.rs".into(), SkipReason::Missing),
            ]
        );
        apply(&plan).unwrap();
        let at_least = UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        assert_eq!(freshen_skipped(&plan, at_least).unwrap(), 7);

        for rel in [
            "", "blob.rs", "bytes.rs", "new.rs", "sub", "v1.txt", "v2.txt",
        ] {
            let meta = fs::symlink_metadata(under(agent, Path::new(rel))).unwrap();
            assert_eq!(
                (meta.mtime(), meta.atime()),
                (1_500_000_000, 1_500_000_000),
                "{rel:?}"
            );
        }
        for rel in ["cur", "link", "vendor"] {
            assert_eq!(mtime(&agent.join(rel)), (OLD, 0), "{rel}");
        }
        assert_eq!(mtime(&outside.join("added.rs")), (OLD, 0));
        assert_eq!(mtime(&agent.join("newer.rs")), (1_600_000_000, 0));
        assert_eq!(mtime(&agent.join("same.rs")), STAMPED);
        assert_eq!(mtime(&agent.join("sub/a.rs")), STAMPED);
        assert_eq!(freshen_skipped(&plan, at_least).unwrap(), 0);
    }

    #[test]
    fn directories_are_stamped_only_with_the_base_entry_names() {
        let fx = Fixture::new(&[
            ("extra/a.rs", "a\n"),
            ("fewer/b.rs", "b\n"),
            ("same/c.rs", "c\n"),
        ]);
        write(&fx.agent.join("extra/untracked.txt"), "u\n");
        write(&fx.base.join("fewer/only_in_base.txt"), "o\n");
        write(&fx.agent.join("newdir/x.rs"), "x\n");
        fx.git(&fx.agent, &["add", "newdir/x.rs"]);

        let plan = fx.plan();
        assert_eq!(
            names(plan.identical()),
            ["extra/a.rs", "fewer/b.rs", "same/c.rs"]
        );
        assert_eq!(
            reasons(plan.skipped_dirs()),
            [
                (String::new(), SkipReason::ContentDiffers),
                ("extra".into(), SkipReason::ContentDiffers),
                ("fewer".into(), SkipReason::ContentDiffers),
                ("newdir".into(), SkipReason::Missing),
            ]
        );
        assert_eq!(names(plan.dirs()), ["same"]);

        let report = apply(&plan).unwrap();
        assert_eq!((report.dirs, report.dirs_stamped), (5, 1));
        assert_eq!(mtime(&fx.agent.join("same")), STAMPED);
        for rel in ["", "extra", "fewer", "newdir"] {
            assert_ne!(mtime(&under(&fx.agent, Path::new(rel))), STAMPED, "{rel:?}");
        }
    }

    #[test]
    fn many_files_are_verified_in_parallel_with_a_deterministic_result() {
        let files: Vec<(String, String)> = (0..2000)
            .map(|i| {
                let path = format!("d{:02}/f{i:04}.rs", i % 20);
                (path, format!("// file {i:04}\n"))
            })
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let fx = Fixture::new(&refs);
        let changed = [0, 3, 250, 999, 1000, 1500, 1999];
        for i in changed {
            fs::write(fx.agent.join(&files[i].0), format!("// FILE {i:04}\n")).unwrap();
        }

        let first = fx.plan();
        let second = fx.plan();
        let identical = names(first.identical());
        assert_eq!(identical, names(second.identical()));
        assert_eq!(reasons(first.skipped()), reasons(second.skipped()));
        assert_eq!(identical.len(), 2000 - changed.len());
        assert!(identical.is_sorted());
        let mut expected: Vec<(String, SkipReason)> = changed
            .iter()
            .map(|&i| (files[i].0.clone(), SkipReason::ContentDiffers))
            .collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(reasons(first.skipped()), expected);

        let report = apply(&first).unwrap();
        assert_eq!((report.stamped, report.skipped), (1993, 7));
        assert_eq!((report.dirs, report.dirs_stamped), (21, 21));
    }

    #[cfg(unix)]
    #[test]
    fn base_checkout_stamps_tracked_files_and_their_directories() {
        let files = [
            ("README.md", "r\n"),
            ("src/lib.rs", "x\n"),
            ("sparse/gone.rs", "g\n"),
        ];
        let fx = Fixture::with(&files, |fx| {
            symlink("README.md", fx.repo.join("link")).unwrap();
            fx.git(&fx.repo, &["add", "link"]);
        });
        let base = &fx.base;
        write(&base.join("untracked.txt"), "u\n");
        // As a sparse checkout leaves it.
        fs::remove_file(base.join("sparse/gone.rs")).unwrap();
        let link_before = mtime(&base.join("link"));

        let index = git::ls_files(base).unwrap();
        assert_eq!(stamp_base_checkout(base, &index).unwrap(), 5);
        for rel in ["README.md", "src/lib.rs", "", "src", "sparse"] {
            assert_eq!(mtime(&under(base, Path::new(rel))), STAMPED, "{rel:?}");
        }
        assert_eq!(mtime(&base.join("link")), link_before);
        assert_ne!(mtime(&base.join("untracked.txt")), STAMPED);
    }

    #[test]
    fn index_paths_that_could_escape_the_worktree_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in ["../escape.rs", "/etc/passwd", "a/../../b", ""] {
            let entry = Entry {
                mode: 0o100_644,
                oid: "1".repeat(40),
                stage: 0,
                path: PathBuf::from(bad),
            };
            let planned = plan(&[], tmp.path(), std::slice::from_ref(&entry), tmp.path());
            assert!(matches!(planned, Err(Error::Refused(_))), "{bad:?}");
            let stamped = stamp_base_checkout(tmp.path(), &[entry]);
            assert!(matches!(stamped, Err(Error::Refused(_))), "{bad:?}");
        }
    }

    #[test]
    fn mode_follows_the_toolchain_channel() {
        let stable = Toolchain::parse("release: 1.99.0\n", "cargo 1.99.0");
        let nightly = Toolchain::parse("release: 1.101.0-nightly\n", "cargo 1.101.0-nightly");
        let mode = |pref, toolchain| resolve_mode(pref, toolchain).unwrap();
        assert_eq!(mode(FreshnessPref::Auto, &nightly), FreshnessMode::Checksum);
        assert_eq!(mode(FreshnessPref::Auto, &stable), FreshnessMode::Mtime);
        assert_eq!(mode(FreshnessPref::Mtime, &nightly), FreshnessMode::Mtime);
        assert_eq!(
            mode(FreshnessPref::Checksum, &nightly),
            FreshnessMode::Checksum
        );
        let err = resolve_mode(FreshnessPref::Checksum, &stable).unwrap_err();
        assert!(
            matches!(&err, Error::Refused(msg) if msg.contains("nightly") && msg.contains("14136")),
            "{err}"
        );
    }

    #[test]
    fn checksum_mode_sets_both_cargo_switches() {
        assert_eq!(
            cargo_env(FreshnessMode::Checksum),
            [
                ("CARGO_UNSTABLE_CHECKSUM_FRESHNESS", "true"),
                ("CARGO_BUILD_FINGERPRINT", "content"),
            ]
        );
        let none: &[(&str, &str)] = &[];
        assert_eq!(cargo_env(FreshnessMode::Mtime), none);
    }
}
