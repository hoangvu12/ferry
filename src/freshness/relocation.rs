//! Making a forked target dir safe to build at other absolute paths than the
//! ones it was built at.
//!
//! An agent without the namespace sandbox builds at its own worktree and
//! target dir, but the fork of its base's target holds units built at the
//! base's paths. Cargo's unit hashes survive the move; what can break is a
//! unit that captured an old path where Cargo never looks again. A plan keeps
//! every unit unless something recorded in the target shows such a path:
//!
//! - R1: Cargo's own records (`dep-*`, the fingerprint JSON) name one.
//! - R2: rustc's dep-info names a checkout path, as an `env!` value or as an
//!   absolute input.
//! - R3: an `env!` value names a target path that is not exempt (below).
//! - R4: a build script's directives name one, other than its own `OUT_DIR`
//!   as Cargo recorded it, which Cargo rewrites whenever it replays them. A
//!   dead (below) `OUT_DIR` may not stay in a link argument even so: the
//!   linker baked it into the package's binaries already.
//! - R5: a file or symlink in a build script's `OUT_DIR` names one.
//! - R6: a generated file outside every `OUT_DIR` that a compile included
//!   names one.
//! - R7: a proc macro may read a path from the environment: its library
//!   holds the name of a path variable, or a local macro's sources use the
//!   working directory, a subprocess or dynamic variable names. Every unit
//!   that can invoke it rebuilds.
//!
//! Every build-script run that would rerun because a removed unit rebuilds
//! is removed whole, so it reruns into an empty `OUT_DIR` and its dependents
//! rebuild in the same build.
//!
//! Target paths of a *live* base, one that is immutable and that gc keeps
//! while the forks exist, may stay where they lead into the `OUT_DIR` of a
//! run the referring unit depends on: the base's bytes there equal the
//! fork's until that run reruns, and then the unit rebuilds too. Every other
//! target path in the store is *dead*: a namespace base's (an empty
//! mountpoint on the host) and a seed's (which gc may collect).
//!
//! Paths are found by one needle per root: the longest suffix of the store
//! path, and of the base checkout's git dir, made only of bytes that no
//! escaping (Debug, JSON, Make, percent-encoding, Unicode normalization)
//! rewrites. So every textual spelling of an old path contains its needle,
//! and what follows the needle tells which part of the store it names.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use memchr::memmem;
use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::sys::{OsStrExt, OsStringExt};

/// Bumped whenever a rule changes, so cached plans are recomputed.
pub const RELOCATION_RULES: u32 = 1;

/// Where a target dir was built, and which base outlives the forks a plan
/// for it is applied to.
#[derive(Debug, Clone)]
pub struct Origin {
    /// The repository's store root.
    pub store: PathBuf,
    /// The checkout path rustc saw: `bases/<id>/src`, or `<store>/mnt/src`
    /// for a base built in the namespace sandbox.
    pub checkout: PathBuf,
    /// Where that checkout is readable now: `bases/<id>/src`.
    pub host_checkout: PathBuf,
    /// The checkout's own git dir, `<common dir>/worktrees/<name>`.
    pub git_dir: PathBuf,
    /// The base whose target dir is immutable and kept by gc for as long as
    /// any fork the plan is applied to exists.
    pub live: Option<String>,
}

/// Why a unit is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capture {
    /// Cargo's dep-info or fingerprint names an old path (R1).
    CargoRecord,
    /// rustc's dep-info names a checkout path (R2).
    SourcePath,
    /// An `env!` value names a target path that may not stay (R3).
    TargetPath,
    /// A build script's directives or `OUT_DIR` name an old path (R4, R5).
    BuildScript,
    /// A generated file a compile included names an old path (R6).
    GeneratedInput,
    /// The unit can invoke a proc macro that may read a path (R7).
    ProcMacro,
    /// A build-script run that reruns because something it depends on does.
    Rerun,
    /// Something the rules need to read is missing or unreadable.
    CannotCheck,
}

/// One unit to remove (or a directory that could not be checked).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removal {
    /// Relative to the target dir.
    pub paths: Vec<PathBuf>,
    /// `<package> (<hash file>)`, or the directory for what is not a unit.
    pub unit: String,
    /// A build-script run, removed whole so that it reruns from scratch.
    pub run: bool,
    pub why: Capture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelocationPlan {
    /// [`RELOCATION_RULES`] when the plan was made.
    pub rules: u32,
    /// The Ferry version that made the plan. Bases survive an upgrade, which
    /// may change a rule without bumping [`RELOCATION_RULES`].
    pub ferry: String,
    /// The [`Origin::live`] the plan was made for.
    pub live: Option<String>,
    /// Empty the target instead: an old root has nothing to search for.
    pub clear: bool,
    pub remove: Vec<Removal>,
}

impl RelocationPlan {
    /// Whether the plan can be stored as JSON: every path is UTF-8.
    pub fn cacheable(&self) -> bool {
        self.remove
            .iter()
            .flat_map(|r| &r.paths)
            .all(|p| p.to_str().is_some())
    }

    /// Whether a stored plan is the one these rules make for forks of the
    /// live base `live`.
    pub fn fits(&self, live: &str) -> bool {
        self.rules == RELOCATION_RULES
            && self.ferry == env!("CARGO_PKG_VERSION")
            && self.live.as_deref() == Some(live)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RelocationReport {
    pub cleared: bool,
    /// Compile units removed, plus directories that could not be checked.
    pub units: usize,
    /// Build-script runs removed.
    pub reruns: usize,
    pub examples: Vec<String>,
}

impl RelocationReport {
    pub fn warning(&self) -> Option<String> {
        if self.cleared {
            return Some(
                "the store or git dir path has no part Ferry can match in build outputs, so \
                 this agent's first build is a full build"
                    .into(),
            );
        }
        let removed = self.units + self.reruns;
        (removed > 0).then(|| {
            format!(
                "{removed} build units captured the base's absolute paths (or could not be \
                 checked) and rebuild on this agent's first build (e.g. {})",
                self.examples.join(", ")
            )
        })
    }
}

/// Plans what to remove from forks of `target` (or `target` itself), which
/// was built as `origin` describes, so that builds at other paths see no
/// unit that captured an old one. Reads `target` only.
pub fn plan_relocation(target: &Path, origin: &Origin) -> Result<RelocationPlan> {
    let mut plan = RelocationPlan {
        rules: RELOCATION_RULES,
        ferry: env!("CARGO_PKG_VERSION").to_owned(),
        live: origin.live.clone(),
        clear: false,
        remove: Vec::new(),
    };
    let Some(needles) = Needles::new(origin) else {
        plan.clear = true;
        return Ok(plan);
    };
    let inventory = Inventory::of(target)?;
    plan.remove = Planner::new(target, origin, &needles, inventory.units).plan(inventory.unchecked);
    Ok(plan)
}

/// Applies `plan` to `target`, a fork of the target it was made for.
pub fn apply_relocation(target: &Path, plan: &RelocationPlan) -> Result<RelocationReport> {
    let mut report = RelocationReport::default();
    if plan.clear {
        crate::fsutil::remove_dir_all_force(target)?;
        fs::create_dir_all(target).at("create directory", target)?;
        report.cleared = true;
        return Ok(report);
    }
    let mut present = Vec::new();
    for removal in &plan.remove {
        for rel in &removal.paths {
            if reachable(target, rel)? {
                present.push(target.join(rel));
            }
        }
    }
    for path in present {
        remove_entry(&path)?;
    }
    for removal in &plan.remove {
        if removal.run {
            report.reruns += 1;
        } else {
            report.units += 1;
        }
        if report.examples.len() < 3 {
            report.examples.push(removal.unit.clone());
        }
    }
    Ok(report)
}

/// The Cargo dep-info files (relative to `target`) of the compile units in
/// `target` that read a source which leads elsewhere in `checkout` than in
/// `origin`'s checkout, through a symlink that differs between the two.
///
/// When the canonical path of such an input lies inside its package or the
/// target dir, Cargo records it by that path in the base, otherwise by the
/// path through the link. Leaving the canonical file unstamped makes Cargo
/// rebuild only where it compares mtimes; with checksum freshness it
/// compares that file's content, which the agent shares with the base.
/// Inputs outside the checkout, and ones that resolve into the store in
/// both checkouts, do not count. A compile without readable rustc dep-info
/// counts too, except rustdoc's, which compares mtimes, as build-script runs
/// do. Reads `target` and both checkouts only.
pub fn link_rebuilds(target: &Path, origin: &Origin, checkout: &Path) -> Result<Vec<PathBuf>> {
    let base =
        crate::sys::canonicalize(&origin.host_checkout).at("resolve", &origin.host_checkout)?;
    let agent = crate::sys::canonicalize(checkout).at("resolve", checkout)?;
    let store = crate::sys::canonicalize(&origin.store).unwrap_or_else(|_| origin.store.clone());
    let mut stale = Vec::new();
    for unit in Inventory::of(target)?.units {
        let moved = match &unit.detail {
            Detail::Run(_) => false,
            Detail::Compile(None) => !unit.kind.starts_with("doc-"),
            Detail::Compile(Some(info)) => {
                let ws = workspace_dir(&base, &unit.profile);
                info.inputs.iter().any(|input| {
                    let Some(rel) = checkout_input(origin, &ws, input) else {
                        return false;
                    };
                    let (was, now) = (resolve(&base, &rel), resolve(&agent, &rel));
                    // Remap mode's link to the target dir: Cargo tracks what
                    // is below it relative to the target, not the checkout.
                    let output =
                        |p: &Option<PathBuf>| p.as_ref().is_some_and(|p| p.starts_with(&store));
                    was != now && !(output(&was) && output(&now))
                })
            }
        };
        if moved {
            stale.push(unit.fingerprint.join(format!("dep-{}", unit.kind)));
        }
    }
    stale.sort();
    Ok(stale)
}

/// Makes each of `dep_info` (relative to `root`, the fork's writable dir:
/// the target, or an overlay's upper dir) an empty file, creating missing
/// parents. Cargo then fails to parse the unit's dep-info, rebuilds it, and
/// rebuilds its dependents in the same build. Emptying its hash file
/// instead would leave the dependents fresh.
pub fn invalidate_dep_info(root: &Path, dep_info: &[PathBuf]) -> Result<()> {
    for rel in dep_info {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).at("create directory", parent)?;
        }
        fs::write(&path, b"").at("write", &path)?;
    }
    Ok(())
}

/// `input`, a rustc dep-info input of a compile in workspace `ws` (relative
/// to the checkout), relative to the checkout; `None` outside it.
fn checkout_input(origin: &Origin, ws: &Path, input: &[u8]) -> Option<PathBuf> {
    let path = os(input);
    if path.is_relative() {
        return Some(ws.join(path));
    }
    [
        Some(origin.checkout.clone()),
        crate::sys::canonicalize(&origin.checkout).ok(),
    ]
    .into_iter()
    .flatten()
    .find_map(|checkout| path.strip_prefix(checkout).ok().map(Path::to_path_buf))
}

/// Where `rel` leads from `root` (canonical), following symlinks as the
/// kernel does: relative to `root` inside it, else absolute; `None` if it
/// leads nowhere.
fn resolve(root: &Path, rel: &Path) -> Option<PathBuf> {
    let path = crate::sys::canonicalize(root.join(rel)).ok()?;
    Some(
        path.strip_prefix(root)
            .map_or_else(|_| path.clone(), Path::to_path_buf),
    )
}

/// The workspace, relative to `checkout`, whose target dir holds `profile`:
/// the checkout's own, or the nested workspace whose `ferry-ws/` target dir
/// holds it.
fn workspace_dir(checkout: &Path, profile: &Path) -> PathBuf {
    let mut root = PathBuf::new();
    if let Ok(rest) = profile.strip_prefix(crate::shim::NESTED_TARGETS) {
        let mut rel = PathBuf::new();
        for part in rest.components() {
            rel.push(part);
            if checkout.join(&rel).join("Cargo.toml").is_file() {
                root.clone_from(&rel);
            }
        }
    }
    root
}

/// Whether `rel`, a removal path of a plan, can be removed below `target`:
/// false if an ancestor is gone. A symlinked ancestor (which a plan never
/// leads through) is refused.
fn reachable(target: &Path, rel: &Path) -> Result<bool> {
    if rel.as_os_str().is_empty()
        || !rel
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err(Error::Refused(format!(
            "a relocation plan names `{}`, which is not a plain relative path",
            rel.display()
        )));
    }
    let mut dir = target.to_path_buf();
    for component in rel.parent().into_iter().flat_map(Path::components) {
        dir.push(component);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(Error::Refused(format!(
                    "{} is not a directory, so Ferry will not remove anything below it",
                    dir.display()
                )));
            }
            Err(e) if super::is_gone(&e) => return Ok(false),
            Err(e) => return Err(e).at("stat", &dir),
        }
    }
    Ok(true)
}

/// Removes a file, symlink or directory tree, never following a symlink.
fn remove_entry(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => crate::fsutil::remove_dir_all_force(path),
        Ok(_) => match fs::remove_file(path) {
            Err(e) if !super::is_gone(&e) => Err(e).at("remove", path),
            _ => Ok(()),
        },
        Err(e) if super::is_gone(&e) => Ok(()),
        Err(e) => Err(e).at("stat", path),
    }
}

/// Bytes that no escaping or normalization of a path rewrites. On Windows
/// `\` counts too: [`fold`] turns every spelling of a separator into `/`
/// before a needle is searched for, and a file is scanned in pieces that end
/// after a byte that is not plain, so a piece never splits a separator run.
fn plain(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(b, b'.' | b'_' | b'-' | b'/')
        || (cfg!(windows) && b == b'\\')
}

/// Windows records a path's separators as `\`, as `\\` in JSON and Debug
/// output, and sometimes as `/` (git, some build scripts). Every byte
/// string relocation searches or compares is folded so that each run of
/// backslashes reads as one `/`, which gives the needles and the
/// classification after them a single spelling. Folding only ever adds
/// matches, so it can only make a plan remove more. Unchanged on Unix.
fn fold(bytes: &[u8]) -> Cow<'_, [u8]> {
    if !cfg!(windows) || !bytes.contains(&b'\\') {
        return Cow::Borrowed(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut in_run = false;
    for &b in bytes {
        if b == b'\\' {
            if !in_run {
                out.push(b'/');
            }
            in_run = true;
        } else {
            out.push(b);
            in_run = false;
        }
    }
    Cow::Owned(out)
}

/// [`fold`] for a value that is moved into place.
fn folded(bytes: Vec<u8>) -> Vec<u8> {
    match fold(&bytes) {
        Cow::Borrowed(_) => bytes,
        Cow::Owned(out) => out,
    }
}

/// Whether folded `path` is absolute: it starts with `/`, or on Windows with
/// a drive (`C:/`); UNC and verbatim prefixes fold to a leading `/`.
fn is_abs(path: &[u8]) -> bool {
    path.starts_with(b"/")
        || (cfg!(windows) && matches!(path, [drive, b':', b'/', ..] if drive.is_ascii_alphabetic()))
}

/// Whether folded `value` is one absolute path rather than a list of them
/// (`:`-separated on Unix, `;`-separated on Windows, where a drive's `:` is
/// part of the path).
fn is_single_path(value: &[u8]) -> bool {
    if cfg!(windows) {
        is_abs(value)
            && !value.contains(&b';')
            && !value.get(2..).unwrap_or_default().contains(&b':')
    } else {
        is_abs(value) && !value.contains(&b':')
    }
}

/// Whether a path component ends where `rest` starts.
fn at_end(rest: &[u8]) -> bool {
    rest.first().is_none_or(|&b| b == b'/' || !plain(b))
}

fn plain_prefix(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|&b| !plain(b)).unwrap_or(bytes.len())]
}

fn climbs(path: &[u8]) -> bool {
    path.split(|&b| b == b'/').any(|c| c == b"..")
}

/// Whether `path`, a plain path below a target root, lies in `dir` without
/// climbing out of it again.
fn inside(path: &[u8], dir: &[u8]) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| (rest.is_empty() || rest[0] == b'/') && !climbs(rest))
}

/// `path` with `.` and `..` resolved lexically. `None` if it climbs above
/// its start (the root, for an absolute path).
fn normalize(path: &[u8]) -> Option<Vec<u8>> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in path.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    let mut out = if path.starts_with(b"/") {
        b"/".to_vec()
    } else {
        Vec::new()
    };
    out.extend_from_slice(&parts.join(&b'/'));
    Some(out)
}

fn os(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

/// The shortest needle that can stand for a root without matching far too
/// much.
const MIN_NEEDLE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Root {
    Store,
    GitDir,
}

/// What an occurrence of an old root names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class<'h> {
    /// A checkout: the base's, another base's, the canonical mount, or the
    /// base's git dir.
    Src,
    /// A path inside a base's target dir (or the canonical one, never
    /// live); `rel` is the plain path below the target root.
    Target { live: bool, rel: &'h [u8] },
    /// Anywhere else in the store.
    Dead,
}

struct Needles {
    finders: Vec<(memmem::Finder<'static>, Root)>,
    live: Option<Vec<u8>>,
}

impl Needles {
    /// `None` if a root has no plain suffix long enough to search for.
    fn new(origin: &Origin) -> Option<Self> {
        let mut needles: Vec<(Vec<u8>, Root)> = Vec::new();
        for (root, kind) in [
            (&origin.store, Root::Store),
            (&origin.git_dir, Root::GitDir),
        ] {
            let given = normalize(&fold(root.as_os_str().as_bytes()));
            let canonical = crate::sys::canonicalize(root)
                .ok()
                .map(|p| folded(p.into_os_string().into_vec()));
            for spelling in [given, canonical].into_iter().flatten() {
                let needle = plain_suffix(&spelling);
                if needle.len() < MIN_NEEDLE {
                    return None;
                }
                needles.push((needle.to_vec(), kind));
            }
        }
        // A needle that contains another finds nothing the other misses.
        let mut kept: Vec<(Vec<u8>, Root)> = Vec::new();
        for (needle, kind) in &needles {
            let redundant = kept.iter().any(|(k, _)| k == needle)
                || needles
                    .iter()
                    .any(|(other, _)| other != needle && memmem::find(needle, other).is_some());
            if !redundant {
                kept.push((needle.clone(), *kind));
            }
        }
        Some(Self {
            finders: kept
                .into_iter()
                .map(|(needle, kind)| (memmem::Finder::new(&needle).into_owned(), kind))
                .collect(),
            live: origin.live.as_ref().map(|id| id.as_bytes().to_vec()),
        })
    }

    /// Every occurrence of an old root in `hay`, with what it names.
    fn hits<'h>(&'h self, hay: &'h [u8]) -> impl Iterator<Item = (usize, Class<'h>)> + 'h {
        self.finders.iter().flat_map(move |(finder, root)| {
            finder.find_iter(hay).map(move |at| {
                let rest = &hay[at + finder.needle().len()..];
                let class = match root {
                    Root::GitDir => Class::Src,
                    Root::Store => self.classify(rest),
                };
                (at, class)
            })
        })
    }

    /// Classifies the bytes after a store root.
    fn classify<'h>(&self, rest: &'h [u8]) -> Class<'h> {
        let target = |live: bool, rest: &'h [u8]| Class::Target {
            live,
            rel: rest.strip_prefix(b"/").map_or(&[][..], plain_prefix),
        };
        let checkout = |rest: &'h [u8], live: bool| match component(rest, b"/.ferry-target") {
            Some(rest) => target(live, rest),
            None => Class::Src,
        };
        if let Some(rest) = rest.strip_prefix(b"/bases/") {
            let len = rest
                .iter()
                .position(|&b| b == b'/' || !plain(b))
                .unwrap_or(rest.len());
            let (id, rest) = rest.split_at(len);
            let live = self.live.as_deref() == Some(id);
            if let Some(rest) = component(rest, b"/src") {
                return checkout(rest, live);
            }
            if let Some(rest) = component(rest, b"/target") {
                return target(live, rest);
            }
        } else if let Some(rest) = component(rest, b"/mnt/src") {
            return checkout(rest, false);
        } else if let Some(rest) = component(rest, b"/mnt/target") {
            return target(false, rest);
        }
        Class::Dead
    }

    /// Adds the live target paths in `hay` to `live`; whether `hay` names
    /// any other old path.
    fn other_than_live(&self, hay: &[u8], live: &mut Vec<Vec<u8>>) -> bool {
        let mut other = false;
        for (_, class) in self.hits(hay) {
            match class {
                Class::Target { live: true, rel } => live.push(rel.to_vec()),
                _ => other = true,
            }
        }
        other
    }
}

/// `rest` after `part`, if `part` is a whole leading path component of it.
fn component<'a>(rest: &'a [u8], part: &[u8]) -> Option<&'a [u8]> {
    rest.strip_prefix(part).filter(|r| at_end(r))
}

/// The longest suffix of `path` that starts with `/` and holds only plain
/// bytes.
fn plain_suffix(path: &[u8]) -> &[u8] {
    let start = path.iter().rposition(|&b| !plain(b)).map_or(0, |i| i + 1);
    let suffix = &path[start..];
    match suffix.iter().position(|&b| b == b'/') {
        Some(slash) => &suffix[slash..],
        None => &[],
    }
}

/// E1: whether the occurrence at `at` in `line` lies in a copy of
/// `root_output` that names that directory or a path below it. Cargo
/// replaces exactly that string when it replays the run
/// (`BuildOutput::parse`), so another spelling of the same directory keeps
/// its old path.
fn in_root_output(line: &[u8], at: usize, root_output: &[u8]) -> bool {
    memmem::find_iter(line, root_output).any(|start| {
        let end = start + root_output.len();
        let rest = &line[end..];
        (start..end).contains(&at) && at_end(rest) && !climbs(plain_prefix(rest))
    })
}

/// Build-script directives whose values the linker bakes into the package's
/// binaries (an rpath), where Cargo's rewrite of a replayed run's `OUT_DIR`
/// never reaches. Search paths (`rustc-link-search`, `rustc-flags`) only
/// reach rustc, after the rewrite.
const LINK_ARGS: &[&str] = &[
    "rustc-link-arg",
    "rustc-link-arg-bins",
    "rustc-link-arg-bin",
    "rustc-link-arg-tests",
    "rustc-link-arg-examples",
    "rustc-link-arg-benches",
    "rustc-link-arg-cdylib",
    "rustc-cdylib-link-arg",
];

/// What rustc's dep-info file records for a compile.
#[derive(Debug, Default)]
struct DepInfo {
    /// Inputs of its first rule; absolute ones normalized.
    inputs: Vec<Vec<u8>>,
    /// `# env-dep:` variables and values; values that are one absolute path
    /// normalized.
    env: Vec<(Vec<u8>, Vec<u8>)>,
    /// Rule targets other than the dep-info file, in the scanned target.
    outputs: Vec<PathBuf>,
}

impl DepInfo {
    fn parse(&mut self, contents: &[u8], file: &Path) {
        let dir = file.parent().unwrap_or(file);
        let own = file.file_name();
        let mut first = true;
        for line in contents.split(|&b| b == b'\n') {
            if let Some(env) = line.strip_prefix(b"# env-dep:") {
                if let Some(eq) = env.iter().position(|&b| b == b'=') {
                    let value = folded(unescape_env(&env[eq + 1..]));
                    let value = is_single_path(&value)
                        .then(|| normalize(&value))
                        .flatten()
                        .unwrap_or(value);
                    self.env.push((env[..eq].to_vec(), value));
                }
                continue;
            }
            if line.starts_with(b"#") {
                continue;
            }
            let Some(colon) = line.windows(2).position(|w| w == b": ") else {
                continue;
            };
            let target = folded(unescape_spaces(&line[..colon]));
            if let Some(name) = os(&target).file_name()
                && Some(name) != own
            {
                self.outputs.push(dir.join(name));
            }
            if first {
                first = false;
                self.inputs
                    .extend(split_inputs(&line[colon + 2..]).into_iter().map(|input| {
                        let input = folded(input);
                        if is_abs(&input) {
                            normalize(&input).unwrap_or(input)
                        } else {
                            input
                        }
                    }));
            }
        }
    }
}

/// Paths of a dep-info rule, which rustc separates by spaces and escapes
/// spaces in as `\ `.
fn split_inputs(rule: &[u8]) -> Vec<Vec<u8>> {
    let mut inputs = Vec::new();
    let mut current = Vec::new();
    let mut i = 0;
    while i < rule.len() {
        match rule[i] {
            b'\\' if rule.get(i + 1) == Some(&b' ') => {
                current.push(b' ');
                i += 1;
            }
            b' ' => {
                if !current.is_empty() {
                    inputs.push(std::mem::take(&mut current));
                }
            }
            b => current.push(b),
        }
        i += 1;
    }
    if !current.is_empty() {
        inputs.push(current);
    }
    inputs
}

/// rustc escapes spaces in dep-info paths as `\ `.
fn unescape_spaces(escaped: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(escaped.len());
    let mut i = 0;
    while i < escaped.len() {
        if escaped[i] == b'\\' && escaped.get(i + 1) == Some(&b' ') {
            i += 1;
        }
        out.push(escaped[i]);
        i += 1;
    }
    out
}

/// rustc escapes `\`, newlines and carriage returns in env-dep values.
fn unescape_env(escaped: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(escaped.len());
    let mut i = 0;
    while i < escaped.len() {
        let b = escaped[i];
        if b == b'\\' {
            match escaped.get(i + 1) {
                Some(b'\\') => out.push(b'\\'),
                Some(b'n') => out.push(b'\n'),
                Some(b'r') => out.push(b'\r'),
                _ => {
                    out.push(b);
                    i += 1;
                    continue;
                }
            }
            i += 2;
            continue;
        }
        out.push(b);
        i += 1;
    }
    out
}

/// A unit of a target: one fingerprint hash file and what Cargo keeps next
/// to it.
#[derive(Debug)]
struct Unit {
    /// `<package> (<hash file>)`.
    label: String,
    /// The hash file's name: `lib-embed`, `run-build-script-build-script-build`.
    kind: String,
    hash: u64,
    deps: Vec<u64>,
    /// The profile dir, relative to the target.
    profile: PathBuf,
    /// The fingerprint dir holding its hash file, relative to the target:
    /// `.fingerprint/<pkg>-<H>` or `build/<pkg>/<H>/fingerprint` below the
    /// profile.
    fingerprint: PathBuf,
    /// What removing the unit deletes, relative to the target.
    removal: Vec<PathBuf>,
    /// The fingerprint dir's `dep-*` and `*.json` files, `\0`-separated;
    /// `None` if one is unreadable.
    records: Option<Vec<u8>>,
    detail: Detail,
}

#[derive(Debug)]
enum Detail {
    /// `None`: no readable rustc dep-info.
    Compile(Option<DepInfo>),
    Run(RunFiles),
}

#[derive(Debug)]
struct RunFiles {
    stdout: Option<Vec<u8>>,
    /// The `OUT_DIR` the script last ran with, which Cargo replaces in its
    /// directives when it replays them.
    root_output: Option<Vec<u8>>,
    /// Relative to the target.
    out_dir: PathBuf,
}

impl Unit {
    fn is_run(&self) -> bool {
        matches!(self.detail, Detail::Run(_))
    }

    fn is_lib(&self) -> bool {
        self.kind.starts_with("lib-")
    }

    fn dep_info(&self) -> Option<&DepInfo> {
        match &self.detail {
            Detail::Compile(info) => info.as_ref(),
            Detail::Run(_) => None,
        }
    }

    fn run_files(&self) -> Option<&RunFiles> {
        match &self.detail {
            Detail::Run(run) => Some(run),
            Detail::Compile(_) => None,
        }
    }
}

#[derive(Deserialize)]
struct FingerprintJson {
    deps: Vec<(u64, String, bool, u64)>,
}

/// Cargo writes a unit's hash as the hex of its little-endian bytes.
fn parse_hash(bytes: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.len() != 16 {
        return None;
    }
    u64::from_str_radix(text, 16).ok().map(u64::swap_bytes)
}

/// Directories a profile dir holds units in, which the walk does not enter.
const PROFILE_PARTS: [&str; 5] = ["deps", "examples", "incremental", "build", ".fingerprint"];

#[derive(Debug, Default)]
struct Inventory {
    units: Vec<Unit>,
    /// What could not be checked and is removed whole.
    unchecked: Vec<Removal>,
}

impl Inventory {
    fn of(target: &Path) -> Result<Self> {
        let mut inventory = Self::default();
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            let dir = target.join(&rel);
            let subdirs = match entries(&dir) {
                Ok(entries) => entries,
                Err(e) if rel.as_os_str().is_empty() => return Err(e).at("read directory", &dir),
                Err(_) => {
                    inventory.unchecked(vec![rel]);
                    continue;
                }
            };
            let profile = is_profile(&dir);
            if profile {
                inventory.profile(target, &rel);
            }
            stack.extend(
                subdirs
                    .into_iter()
                    .filter(|(name, kind)| {
                        kind.is_dir()
                            && !(profile
                                && PROFILE_PARTS
                                    .iter()
                                    .any(|p| name.as_bytes() == p.as_bytes()))
                    })
                    .map(|(name, _)| rel.join(name)),
            );
        }
        Ok(inventory)
    }

    fn unchecked(&mut self, paths: Vec<PathBuf>) {
        if let Some(first) = paths.first() {
            self.unchecked.push(Removal {
                unit: first.display().to_string(),
                paths,
                run: false,
                why: Capture::CannotCheck,
            });
        }
    }

    /// The units of one profile dir, in both Cargo layouts.
    fn profile(&mut self, target: &Path, profile: &Path) {
        let build: Vec<OsString> = names(&target.join(profile).join("build"), true);
        self.old_layout(target, profile, &build);
        self.new_layout(target, profile, &build);
    }

    /// Up to Cargo 1.99: fingerprints in .fingerprint/<pkg>-<H>/, rustc's
    /// <crate>-<H>.d next to the outputs in deps/, examples/ or (build
    /// scripts) build/<pkg>-<H>/. Path-source cdylibs and dylibs get no
    /// hash, so their deps/<crate>.d is matched by lib name instead.
    fn old_layout(&mut self, target: &Path, profile: &Path, build: &[OsString]) {
        let dir = target.join(profile);
        let Ok(fingerprints) = entries(&dir.join(".fingerprint")) else {
            self.unchecked(vec![profile.join(".fingerprint"), profile.join("build")]);
            return;
        };
        if fingerprints.is_empty() {
            return;
        }
        let mut by_hash: HashMap<Vec<u8>, Vec<PathBuf>> = HashMap::new();
        let mut unhashed: HashMap<Vec<u8>, Vec<PathBuf>> = HashMap::new();
        let deps = dir.join("deps");
        let dirs = [deps.clone(), dir.join("examples")]
            .into_iter()
            .chain(build.iter().map(|b| dir.join("build").join(b)));
        for d in dirs {
            for file in names(&d, false) {
                let Some(stem) = file.as_bytes().strip_suffix(b".d") else {
                    continue;
                };
                match stem.iter().rposition(|&b| b == b'-') {
                    Some(dash) => by_hash.entry(stem[dash + 1..].to_vec()),
                    None if d == deps => unhashed.entry(stem.to_vec()),
                    None => continue,
                }
                .or_default()
                .push(d.join(&file));
            }
        }
        for (name, kind) in fingerprints {
            let fingerprint = profile.join(".fingerprint").join(&name);
            if !kind.is_dir() {
                self.unchecked(vec![fingerprint]);
                continue;
            }
            let bytes = name.as_bytes();
            let (pkg, hash) = match bytes.iter().rposition(|&b| b == b'-') {
                Some(dash) => (&bytes[..dash], &bytes[dash + 1..]),
                None => (bytes, &b""[..]),
            };
            let run_dir = profile.join("build").join(&name);
            let unknown = || {
                let mut paths = vec![fingerprint.clone()];
                if target.join(&run_dir).is_dir() {
                    paths.push(run_dir.clone());
                }
                paths
            };
            let Some(files) = fingerprint_files(&target.join(&fingerprint)) else {
                self.unchecked(unknown());
                continue;
            };
            let hash_files = hash_files(&files);
            if hash_files.is_empty() {
                self.unchecked(unknown());
            }
            for kind in hash_files {
                let detail = if kind.starts_with("run-") {
                    Detail::Run(RunFiles::read(
                        target,
                        &run_dir.join("output"),
                        &run_dir.join("root-output"),
                        run_dir.join("out"),
                    ))
                } else {
                    let mut dep_info = by_hash.get(hash).cloned().unwrap_or_default();
                    for file in &files {
                        if let Some(lib) = file.as_bytes().strip_prefix(b"dep-lib-")
                            && let Some(found) = unhashed.get(lib)
                        {
                            dep_info.extend(found.iter().cloned());
                        }
                    }
                    Detail::Compile(read_dep_info(&dep_info))
                };
                let removal = if detail_is_run(&detail) {
                    vec![fingerprint.clone(), run_dir.clone()]
                } else {
                    vec![fingerprint.clone()]
                };
                let found = Found {
                    pkg,
                    profile,
                    kind,
                    removal,
                    detail,
                };
                self.unit(target, &fingerprint, &files, found);
            }
        }
    }

    /// From Cargo 1.100: build/<pkg>/<H>/{fingerprint,out,run}/.
    fn new_layout(&mut self, target: &Path, profile: &Path, build: &[OsString]) {
        let dir = target.join(profile);
        for pkg in build {
            let Ok(hashes) = entries(&dir.join("build").join(pkg)) else {
                self.unchecked(vec![profile.join("build").join(pkg)]);
                continue;
            };
            for (hash, kind) in hashes {
                if !kind.is_dir() || !is_unit_hash(hash.as_bytes()) {
                    continue;
                }
                let unit_dir = profile.join("build").join(pkg).join(&hash);
                let fingerprint = unit_dir.join("fingerprint");
                match fs::symlink_metadata(target.join(&fingerprint)) {
                    Ok(meta) if meta.is_dir() => {}
                    Err(e) if super::is_gone(&e) => continue,
                    _ => {
                        self.unchecked(vec![unit_dir]);
                        continue;
                    }
                }
                let Some(files) = fingerprint_files(&target.join(&fingerprint)) else {
                    self.unchecked(vec![unit_dir]);
                    continue;
                };
                let hash_files = hash_files(&files);
                if hash_files.is_empty() {
                    self.unchecked(vec![unit_dir.clone()]);
                }
                for kind in hash_files {
                    let (detail, removal) = if kind.starts_with("run-") {
                        let run = unit_dir.join("run");
                        let files = RunFiles::read(
                            target,
                            &run.join("stdout"),
                            &run.join("root-output"),
                            unit_dir.join("out"),
                        );
                        (Detail::Run(files), vec![unit_dir.clone()])
                    } else {
                        let out = target.join(&unit_dir).join("out");
                        let dep_info: Vec<PathBuf> = names(&out, false)
                            .into_iter()
                            .filter(|f| f.as_bytes().ends_with(b".d"))
                            .map(|f| out.join(f))
                            .collect();
                        (
                            Detail::Compile(read_dep_info(&dep_info)),
                            vec![fingerprint.clone()],
                        )
                    };
                    let found = Found {
                        pkg: pkg.as_bytes(),
                        profile,
                        kind,
                        removal,
                        detail,
                    };
                    self.unit(target, &fingerprint, &files, found);
                }
            }
        }
    }

    /// Reads the hash, dependencies and records of `found`, whose hash file
    /// is in `fingerprint` (holding `files`).
    fn unit(&mut self, target: &Path, fingerprint: &Path, files: &[OsString], found: Found) {
        let Found {
            pkg,
            profile,
            kind,
            removal,
            detail,
        } = found;
        let label = format!("{} ({kind})", String::from_utf8_lossy(pkg));
        let dir = target.join(fingerprint);
        let hash = fs::read(dir.join(&kind)).ok().and_then(|b| parse_hash(&b));
        let json = fs::read(dir.join(format!("{kind}.json")))
            .ok()
            .and_then(|b| serde_json::from_slice::<FingerprintJson>(&b).ok());
        let (Some(hash), Some(json)) = (hash, json) else {
            self.unchecked.push(Removal {
                paths: removal,
                unit: label,
                run: detail_is_run(&detail),
                why: Capture::CannotCheck,
            });
            return;
        };
        let mut records = Some(Vec::new());
        for file in files {
            let name = file.as_bytes();
            if name.starts_with(b"dep-") || name.ends_with(b".json") {
                match (&mut records, fs::read(dir.join(file))) {
                    (Some(all), Ok(bytes)) => {
                        all.extend_from_slice(&fold(&bytes));
                        all.push(0);
                    }
                    _ => records = None,
                }
            }
        }
        self.units.push(Unit {
            label,
            kind,
            hash,
            deps: json.deps.into_iter().map(|d| d.3).collect(),
            profile: profile.to_path_buf(),
            fingerprint: fingerprint.to_path_buf(),
            removal,
            records,
            detail,
        });
    }
}

/// A hash file found in a profile, before its contents are read.
struct Found<'a> {
    pkg: &'a [u8],
    profile: &'a Path,
    kind: String,
    removal: Vec<PathBuf>,
    detail: Detail,
}

fn detail_is_run(detail: &Detail) -> bool {
    matches!(detail, Detail::Run(_))
}

impl RunFiles {
    fn read(target: &Path, stdout: &Path, root_output: &Path, out_dir: PathBuf) -> Self {
        Self {
            stdout: fs::read(target.join(stdout)).ok().map(folded),
            root_output: fs::read(target.join(root_output)).ok().map(folded),
            // Compared with folded paths below the target root.
            out_dir: PathBuf::from(OsString::from_vec(folded(
                out_dir.into_os_string().into_vec(),
            ))),
        }
    }
}

/// rustc's dep-info for a compile, merged from its `.d` files; `None` if
/// there are none or one is unreadable.
fn read_dep_info(files: &[PathBuf]) -> Option<DepInfo> {
    if files.is_empty() {
        return None;
    }
    let mut info = DepInfo::default();
    for file in files {
        info.parse(&fs::read(file).ok()?, file);
    }
    Some(info)
}

/// A directory is a profile if Cargo keeps fingerprints in it, in either
/// layout.
fn is_profile(dir: &Path) -> bool {
    if fs::symlink_metadata(dir.join(".fingerprint")).is_ok_and(|m| m.is_dir()) {
        return true;
    }
    let build = dir.join("build");
    names(&build, true).iter().any(|pkg| {
        names(&build.join(pkg), true).iter().any(|hash| {
            is_unit_hash(hash.as_bytes())
                && fs::symlink_metadata(build.join(pkg).join(hash).join("fingerprint"))
                    .is_ok_and(|m| m.is_dir())
        })
    })
}

/// Cargo's per-unit directory names in the new layout: 16 hex digits.
fn is_unit_hash(name: &[u8]) -> bool {
    name.len() == 16 && name.iter().all(u8::is_ascii_hexdigit)
}

/// Every file `F` with a sibling `F.json` is a unit's hash file.
fn hash_files(files: &[OsString]) -> Vec<String> {
    let mut found: Vec<String> = files
        .iter()
        .filter_map(|f| f.to_str())
        .filter(|f| {
            let name = f.as_bytes();
            !name.ends_with(b".json")
                && !name.ends_with(b".d")
                && files
                    .iter()
                    .any(|j| j.as_bytes() == format!("{f}.json").as_bytes())
        })
        .map(str::to_owned)
        .collect();
    found.sort();
    found
}

/// The regular files of a fingerprint dir; `None` if it is unreadable.
fn fingerprint_files(dir: &Path) -> Option<Vec<OsString>> {
    let entries = entries(dir).ok()?;
    Some(
        entries
            .into_iter()
            .filter(|(_, kind)| kind.is_file())
            .map(|(name, _)| name)
            .collect(),
    )
}

/// The entries of `dir`, sorted, without following symlinks; none if it
/// does not exist.
fn entries(dir: &Path) -> io::Result<Vec<(OsString, fs::FileType)>> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if super::is_gone(&e) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut found = Vec::new();
    for entry in read {
        let entry = entry?;
        found.push((entry.file_name(), entry.file_type()?));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Names of the real subdirectories (`dirs`) or regular files of `dir`; none
/// if it cannot be read.
fn names(dir: &Path, dirs: bool) -> Vec<OsString> {
    entries(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, kind)| if dirs { kind.is_dir() } else { kind.is_file() })
        .map(|(name, _)| name)
        .collect()
}

/// How much of a file a scan reads at a time.
const CHUNK: usize = 1 << 20;

/// Calls `scan` with consecutive pieces of the file at `path`, so that a
/// library of gigabytes in an `OUT_DIR` costs no more memory than a small
/// one. A piece ends after a byte that is not plain, or at the end of the
/// file, so no needle or literal, nor the plain path after a needle, is ever
/// split. Fails on a run of plain bytes longer than `CHUNK`, which no path
/// is.
fn scan_file(path: &Path, mut scan: impl FnMut(&[u8])) -> io::Result<()> {
    let mut file = fs::File::open(path)?;
    let mut buf = Vec::new();
    loop {
        if (&mut file).take(CHUNK as u64).read_to_end(&mut buf)? == 0 {
            scan(&fold(&buf));
            return Ok(());
        }
        let end = buf.iter().rposition(|&b| !plain(b)).map_or(0, |i| i + 1);
        if buf.len() - end > CHUNK {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a run of path bytes longer than any path",
            ));
        }
        scan(&fold(&buf[..end]));
        buf.drain(..end);
    }
}

/// Literal names of path variables in a proc macro's library. Class M
/// variables hold the same value for a registry unit in the base and an
/// agent; class X variables differ for every unit (Ferry sets the first
/// four of them).
const CLASS_M: &[&[u8]] = &[b"CARGO_MANIFEST_", b"CARGO_RUSTC_CURRENT_DIR"];
const CLASS_X: &[&[u8]] = &[
    b"OUT_DIR",
    b"PWD",
    b"CARGO_TARGET_DIR",
    b"CARGO_BUILD_TARGET_DIR",
    b"CARGO_BUILD_BUILD_DIR",
    b"CARGO_TARGET_TMPDIR",
    b"CARGO_BIN_EXE_",
    b"CARGO_WORKSPACE_DIR",
];

/// In a local proc macro's sources: ways to a path without naming a
/// variable.
const CWD_TOKENS: &[&[u8]] = &[
    b"current_dir",
    b"canonicalize",
    b"absolute",
    b"Command",
    b"var(",
    b"var_os",
    b"vars(",
    b"local_file",
    b"PWD",
];

/// Registry crates (`<name>-<version>`) whose literal hits a human checked:
/// they read files or rewrite their own sources, and embed no path.
/// document-features reads `Cargo.toml` (`lib.rs:252`); xflags-macros reads
/// the variable only under `UPDATE_XFLAGS` (`update.rs:5`).
const ALLOWLIST: &[&str] = &["document-features-0.2.12", "xflags-macros-0.3.2"];

const PROC_MACRO_DECLS: &[u8] = b"__rustc_proc_macro_decls_";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MacroClass {
    /// May read a variable whose value only differs for local units.
    Manifest,
    /// May read a path that differs for every unit.
    Any,
}

/// A shared library a unit built.
#[derive(Debug, Default)]
struct SharedLib {
    proc_macro: bool,
    unreadable: bool,
    literals: Vec<&'static [u8]>,
}

fn literals_in(bytes: &[u8]) -> Vec<&'static [u8]> {
    let mut found = Vec::new();
    for at in memmem::find_iter(bytes, b"CARGO_") {
        for literal in CLASS_M.iter().chain(CLASS_X) {
            if bytes[at..].starts_with(literal) && !found.contains(literal) {
                found.push(*literal);
            }
        }
    }
    for literal in [b"OUT_DIR".as_slice(), b"PWD"] {
        if memmem::find(bytes, literal).is_some() {
            found.push(literal);
        }
    }
    found
}

struct Planner<'a> {
    target: &'a Path,
    origin: &'a Origin,
    needles: &'a Needles,
    units: Vec<Unit>,
    deps: Vec<Vec<usize>>,
    rdeps: Vec<Vec<usize>>,
    flags: Vec<Option<Capture>>,
    local: Vec<bool>,
    /// For each unit, the runs it depends on (itself included, for a run),
    /// once needed.
    reached: Vec<Option<Vec<usize>>>,
}

impl<'a> Planner<'a> {
    fn new(target: &'a Path, origin: &'a Origin, needles: &'a Needles, units: Vec<Unit>) -> Self {
        let n = units.len();
        let mut by_hash: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, unit) in units.iter().enumerate() {
            by_hash.entry(unit.hash).or_default().push(i);
        }
        let mut flags = vec![None; n];
        let mut deps = vec![Vec::new(); n];
        let mut rdeps = vec![Vec::new(); n];
        for (i, unit) in units.iter().enumerate() {
            for hash in &unit.deps {
                match by_hash.get(hash) {
                    Some(found) => {
                        // A nested workspace's target can hold the same unit;
                        // Cargo resolves dependencies in the unit's own.
                        let shared = |d: &usize| {
                            let other = units[*d].profile.components();
                            unit.profile
                                .components()
                                .zip(other)
                                .take_while(|(a, b)| a == b)
                                .count()
                        };
                        let best = found.iter().map(shared).max().unwrap_or(0);
                        for &d in found.iter().filter(|d| shared(d) == best) {
                            deps[i].push(d);
                            rdeps[d].push(i);
                        }
                    }
                    // Cargo considers the unit stale already: its dependency
                    // was rebuilt since. Removing it costs nothing.
                    None => flags[i] = Some(Capture::CannotCheck),
                }
            }
        }
        // A workspace member (Cargo passes relative inputs only for packages
        // under the workspace root), or a package in the checkout.
        let local = units
            .iter()
            .map(|u| {
                u.dep_info().is_some_and(|info| {
                    info.inputs
                        .iter()
                        .any(|i| !is_abs(i) || needles.hits(i).any(|(_, c)| c == Class::Src))
                })
            })
            .collect();
        Self {
            target,
            origin,
            needles,
            units,
            deps,
            rdeps,
            flags,
            local,
            reached: vec![None; n],
        }
    }

    fn flag(&mut self, i: usize, why: Capture) {
        self.flags[i].get_or_insert(why);
    }

    fn plan(mut self, unchecked: Vec<Removal>) -> Vec<Removal> {
        for i in 0..self.units.len() {
            self.records(i);
            if self.units[i].is_run() {
                self.directives(i);
                self.out_dir(i);
            } else {
                self.dep_info(i);
                self.generated_inputs(i);
            }
        }
        self.proc_macros();
        self.removals(unchecked)
    }

    /// R1.
    fn records(&mut self, i: usize) {
        match &self.units[i].records {
            Some(records) if self.needles.hits(records).next().is_none() => {}
            Some(_) => self.flag(i, Capture::CargoRecord),
            None => self.flag(i, Capture::CannotCheck),
        }
    }

    /// R2 and R3.
    fn dep_info(&mut self, i: usize) {
        let unit = &self.units[i];
        let Some(info) = unit.dep_info() else {
            if !unit.kind.starts_with("doc-") {
                self.flag(i, Capture::CannotCheck);
            }
            return;
        };
        let mut why = None;
        let mut live = Vec::new();
        for (_, value) in &info.env {
            for (_, class) in self.needles.hits(value) {
                match class {
                    Class::Src => why = why.or(Some(Capture::SourcePath)),
                    Class::Target { live: true, rel } => live.push(rel.to_vec()),
                    // A registry crate's `include!(concat!(env!("OUT_DIR"),
                    // ..))`: compile-time only, and Cargo tracks the input
                    // relative to the target root.
                    Class::Target { live: false, .. } | Class::Dead
                        if !self.local[i] && is_input_prefix(info, value) => {}
                    Class::Target { live: false, .. } | Class::Dead => {
                        why = why.or(Some(Capture::TargetPath));
                    }
                }
            }
        }
        // A member's inputs are relative. Cargo hashes the absolute source
        // path of every other package (`path_args`), so a vendored crate or
        // a path package outside the workspace root rebuilds at the agent's
        // checkout anyway; flagging it also removes the runs above it.
        if info
            .inputs
            .iter()
            .any(|input| self.needles.hits(input).any(|(_, c)| c == Class::Src))
        {
            why = why.or(Some(Capture::SourcePath));
        }
        if let Some(why) = why {
            self.flag(i, why);
        }
        if live.iter().any(|rel| !self.in_reached_out_dir(i, rel)) {
            self.flag(i, Capture::TargetPath);
        }
    }

    /// R6.
    fn generated_inputs(&mut self, i: usize) {
        let Some(info) = self.units[i].dep_info() else {
            return;
        };
        // Inputs in an OUT_DIR are that run's content (R5).
        let mut files = Vec::new();
        let mut flagged = false;
        for input in &info.inputs {
            for (_, class) in self.needles.hits(input) {
                match class {
                    Class::Target { rel, .. } if !self.in_any_out_dir(rel) => {
                        files.push(rel.to_vec());
                    }
                    Class::Target { .. } | Class::Src => {}
                    Class::Dead => flagged = true,
                }
            }
        }
        let needles = self.needles;
        for rel in files {
            let path = self.target.join(os(&rel));
            let mut live = Vec::new();
            let scanned = scan_file(&path, |piece| {
                flagged |= needles.other_than_live(piece, &mut live);
            });
            if scanned.is_err() {
                flagged = true;
                break;
            }
            if live.iter().any(|rel| !self.in_reached_out_dir(i, rel)) {
                flagged = true;
            }
        }
        if flagged {
            self.flag(i, Capture::GeneratedInput);
            for run in self.reached_runs(i) {
                self.flag(run, Capture::GeneratedInput);
            }
        }
    }

    /// R4.
    fn directives(&mut self, i: usize) {
        let Some(run) = self.units[i].run_files() else {
            return;
        };
        let Some(stdout) = &run.stdout else {
            self.flag(i, Capture::CannotCheck);
            return;
        };
        let root_output = run.root_output.as_deref();
        let mut flagged = false;
        let mut live = Vec::new();
        for line in stdout.split(|&b| b == b'\n') {
            let Ok(text) = std::str::from_utf8(line) else {
                continue;
            };
            let text = text.trim();
            let Some(directive) = text
                .strip_prefix("cargo::")
                .or_else(|| text.strip_prefix("cargo:"))
            else {
                continue;
            };
            let key = directive.split_once('=').map_or(directive, |(k, _)| k);
            if matches!(key, "warning" | "error") {
                continue;
            }
            let linked = LINK_ARGS.contains(&key);
            let line = text.as_bytes();
            for (at, class) in self.needles.hits(line) {
                match class {
                    Class::Src => flagged = true,
                    // The rewrite never reaches what the linker baked into
                    // the package's binaries.
                    Class::Target { live: false, .. } | Class::Dead if linked => flagged = true,
                    _ if root_output.is_some_and(|r| in_root_output(line, at, r)) => {}
                    Class::Target { live: true, rel } => live.push(rel.to_vec()),
                    Class::Target { .. } | Class::Dead => flagged = true,
                }
            }
        }
        if flagged || live.iter().any(|rel| !self.in_reached_out_dir(i, rel)) {
            self.flag(i, Capture::BuildScript);
        }
    }

    /// R5.
    fn out_dir(&mut self, i: usize) {
        let Some(run) = self.units[i].run_files() else {
            return;
        };
        let needles = self.needles;
        let mut live = Vec::new();
        let mut flagged = false;
        let mut stack = vec![run.out_dir.clone()];
        while let Some(rel) = stack.pop() {
            let dir = self.target.join(&rel);
            let Ok(found) = entries(&dir) else {
                flagged = true;
                break;
            };
            for (name, kind) in found {
                let path = dir.join(&name);
                if kind.is_dir() {
                    stack.push(rel.join(&name));
                    continue;
                }
                let checked = if kind.is_file() {
                    scan_file(&path, |piece| {
                        flagged |= needles.other_than_live(piece, &mut live);
                    })
                    .is_ok()
                } else if kind.is_symlink() {
                    fs::read_link(&path).is_ok_and(|link| {
                        let link = folded(link.into_os_string().into_vec());
                        if is_abs(&link) {
                            flagged |= needles.other_than_live(&link, &mut live);
                            return true;
                        }
                        let mut resolved = fold(rel.as_os_str().as_bytes()).into_owned();
                        resolved.push(b'/');
                        resolved.extend_from_slice(&link);
                        // A relative link may lead anywhere in the target,
                        // but no further.
                        normalize(&resolved).is_some()
                    })
                } else {
                    false
                };
                flagged |= !checked;
                if flagged {
                    break;
                }
            }
            if flagged {
                break;
            }
        }
        if flagged || live.iter().any(|rel| !self.in_reached_out_dir(i, rel)) {
            self.flag(i, Capture::BuildScript);
        }
    }

    /// R7.
    fn proc_macros(&mut self) {
        let mut libs: Vec<Option<SharedLib>> = Vec::with_capacity(self.units.len());
        for unit in &self.units {
            let shared: Vec<&PathBuf> = unit
                .dep_info()
                .map(|info| {
                    info.outputs
                        .iter()
                        .filter(|o| {
                            o.extension()
                                .is_some_and(|e| e == "so" || e == "dylib" || e == "dll")
                        })
                        .collect()
                })
                .unwrap_or_default();
            if shared.is_empty() {
                libs.push(None);
                continue;
            }
            let mut lib = SharedLib::default();
            for path in shared {
                let scanned = scan_file(path, |piece| {
                    lib.proc_macro |= memmem::find(piece, PROC_MACRO_DECLS).is_some();
                    for literal in literals_in(piece) {
                        if !lib.literals.contains(&literal) {
                            lib.literals.push(literal);
                        }
                    }
                });
                lib.unreadable |= scanned.is_err();
            }
            libs.push(Some(lib));
        }
        let is_macro = |i: usize| {
            libs[i]
                .as_ref()
                .is_some_and(|l| l.proc_macro || l.unreadable)
        };
        for p in (0..self.units.len()).filter(|&p| is_macro(p)) {
            // The non-proc-macro libraries the macro links.
            let mut closure = Vec::new();
            let mut stack = self.deps[p].clone();
            while let Some(d) = stack.pop() {
                if !closure.contains(&d) && self.units[d].is_lib() && !is_macro(d) {
                    closure.push(d);
                    stack.extend(&self.deps[d]);
                }
            }
            let mut class = libs[p]
                .as_ref()
                .is_some_and(|l| l.unreadable)
                .then_some(MacroClass::Any);
            let mut literals: BTreeSet<&'static [u8]> = BTreeSet::new();
            for &c in std::iter::once(&p).chain(&closure) {
                if let Some(lib) = &libs[c] {
                    literals.extend(&lib.literals);
                }
            }
            let crates: Vec<usize> = std::iter::once(p).chain(closure.iter().copied()).collect();
            for literal in literals {
                if self.allowlisted(&crates, literal) {
                    continue;
                }
                let found = if CLASS_M.contains(&literal) {
                    MacroClass::Manifest
                } else {
                    MacroClass::Any
                };
                class = class.max(Some(found));
            }
            if self.local[p] && class != Some(MacroClass::Any) {
                let local: Vec<usize> = crates.iter().copied().filter(|&c| self.local[c]).collect();
                if self.uses_cwd(&local) {
                    class = Some(MacroClass::Any);
                }
            }
            if let Some(class) = class {
                let dead = self.origin.live.is_none();
                for u in self.exposed(p, &is_macro) {
                    if class == MacroClass::Any || dead || self.local[u] {
                        self.flag(u, Capture::ProcMacro);
                    }
                }
            }
        }
    }

    /// E5: whether every crate whose sources hold `literal` is allowlisted,
    /// and at least one does. Any unreadable source counts against it.
    fn allowlisted(&self, crates: &[usize], literal: &[u8]) -> bool {
        let mut holders = 0;
        for &c in crates {
            let Some(sources) = self.sources(c) else {
                return false;
            };
            if sources
                .iter()
                .any(|source| memmem::find(source, literal).is_some())
            {
                if !self.is_allowlisted(c) {
                    return false;
                }
                holders += 1;
            }
        }
        holders > 0
    }

    fn is_allowlisted(&self, c: usize) -> bool {
        let Some(input) = self.units[c].dep_info().and_then(|i| i.inputs.first()) else {
            return false;
        };
        let parts: Vec<&[u8]> = input.split(|&b| b == b'/').collect();
        parts.windows(4).any(|w| {
            w[0] == b"registry" && w[1] == b"src" && ALLOWLIST.iter().any(|a| a.as_bytes() == w[3])
        })
    }

    /// Tier 2: whether a local macro's sources may compute a path.
    fn uses_cwd(&self, crates: &[usize]) -> bool {
        crates.iter().any(|&c| {
            self.sources(c).is_none_or(|sources| {
                sources
                    .iter()
                    .any(|s| CWD_TOKENS.iter().any(|t| memmem::find(s, t).is_some()))
            })
        })
    }

    /// The contents of every input of a compile; `None` if one is
    /// unreadable.
    fn sources(&self, c: usize) -> Option<Vec<Vec<u8>>> {
        let unit = &self.units[c];
        let info = unit.dep_info()?;
        let root = self.workspace_root(&unit.profile);
        info.inputs
            .iter()
            .map(|input| fs::read(self.source_path(&root, input)).ok())
            .collect()
    }

    /// Where an input of a compile can be read now.
    fn source_path(&self, root: &Path, input: &[u8]) -> PathBuf {
        let path = os(input);
        if !path.is_absolute() {
            return root.join(path);
        }
        for checkout in [
            Some(self.origin.checkout.clone()),
            crate::sys::canonicalize(&self.origin.checkout).ok(),
        ]
        .into_iter()
        .flatten()
        {
            if let Ok(rest) = path.strip_prefix(&checkout) {
                return self.origin.host_checkout.join(rest);
            }
        }
        path.to_path_buf()
    }

    /// The workspace root relative inputs of units in `profile` are relative
    /// to: the checkout, or the nested workspace whose target dir holds the
    /// profile.
    fn workspace_root(&self, profile: &Path) -> PathBuf {
        let host = &self.origin.host_checkout;
        host.join(workspace_dir(host, profile))
    }

    /// The units that can invoke proc macro `p`: its dependents, and through
    /// re-exports the dependents of every library among them.
    fn exposed(&self, p: usize, is_macro: &dyn Fn(usize) -> bool) -> Vec<usize> {
        let mut exposed = Vec::new();
        let mut stack = self.rdeps[p].clone();
        while let Some(u) = stack.pop() {
            if exposed.contains(&u) {
                continue;
            }
            exposed.push(u);
            if self.units[u].is_lib() && !is_macro(u) {
                stack.extend(&self.rdeps[u]);
            }
        }
        exposed
    }

    /// The runs unit `i` depends on, and `i` itself if it is a run.
    fn reached_runs(&mut self, i: usize) -> Vec<usize> {
        if let Some(runs) = &self.reached[i] {
            return runs.clone();
        }
        let mut seen = vec![false; self.units.len()];
        let mut stack = vec![i];
        let mut runs = Vec::new();
        while let Some(u) = stack.pop() {
            if std::mem::replace(&mut seen[u], true) {
                continue;
            }
            if self.units[u].is_run() {
                runs.push(u);
            }
            stack.extend(&self.deps[u]);
        }
        self.reached[i] = Some(runs.clone());
        runs
    }

    /// E2: whether `rel` (below the live target) lies in the `OUT_DIR` of a
    /// run unit `i` depends on. That `OUT_DIR` is where the run's files are
    /// in the live base whatever its `root-output` says: a run replayed
    /// fresh from a seed keeps the seed's there.
    fn in_reached_out_dir(&mut self, i: usize, rel: &[u8]) -> bool {
        self.reached_runs(i)
            .into_iter()
            .any(|r| self.run_out_dir(r).is_some_and(|out| inside(rel, out)))
    }

    fn in_any_out_dir(&self, rel: &[u8]) -> bool {
        (0..self.units.len()).any(|r| self.run_out_dir(r).is_some_and(|out| inside(rel, out)))
    }

    /// A run's `OUT_DIR` below the target root, if Cargo recorded the run's
    /// output dir (without it, Cargo rewrites nothing when it replays the
    /// run, so nothing may refer to it).
    fn run_out_dir(&self, r: usize) -> Option<&[u8]> {
        let run = self.units[r].run_files()?;
        run.root_output.as_ref()?;
        Some(run.out_dir.as_os_str().as_bytes())
    }

    /// The removal list: flagged units, the runs that would rerun because
    /// of them, and what could not be checked. A path below another is left
    /// out.
    fn removals(self, unchecked: Vec<Removal>) -> Vec<Removal> {
        let n = self.units.len();
        let mut above = vec![false; n];
        let mut stack: Vec<usize> = (0..n).filter(|&i| self.flags[i].is_some()).collect();
        while let Some(u) = stack.pop() {
            for &d in &self.rdeps[u] {
                if !std::mem::replace(&mut above[d], true) {
                    stack.push(d);
                }
            }
        }
        let mut removals = unchecked;
        for (i, unit) in self.units.into_iter().enumerate() {
            let run = matches!(unit.detail, Detail::Run(_));
            let why = match self.flags[i] {
                Some(why) => why,
                None if run && above[i] => Capture::Rerun,
                None => continue,
            };
            removals.push(Removal {
                paths: unit.removal,
                unit: unit.label,
                run,
                why,
            });
        }
        let all: BTreeSet<PathBuf> = removals.iter().flat_map(|r| r.paths.clone()).collect();
        let mut claimed = BTreeSet::new();
        for removal in &mut removals {
            removal.paths.retain(|path| {
                !path.ancestors().skip(1).any(|a| all.contains(a)) && claimed.insert(path.clone())
            });
        }
        removals.retain(|r| !r.paths.is_empty());
        removals.sort_by(|a, b| a.paths.cmp(&b.paths));
        removals
    }
}

/// Whether `value`, a single path, is a directory holding one of the
/// compile's own inputs.
fn is_input_prefix(info: &DepInfo, value: &[u8]) -> bool {
    is_abs(value)
        && info
            .inputs
            .iter()
            .any(|input| os(input).starts_with(os(value)))
}

#[cfg(test)]
mod tests;
