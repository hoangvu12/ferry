//! Pruning a seeded base's target dir to the units its own warm build used.
//!
//! INVARIANT: a ready base's target dir holds only units its own warm build
//! compiled or found fresh. A cold base holds only what it compiled, and a
//! base seeded from one with the same unit inputs selects the same units, so
//! both satisfy it by construction. A refresh seeded across manifest,
//! lockfile or config changes inherits units the new build no longer
//! selects. They were checked only against older checkouts, so an agent that
//! selects one again (by reverting the change) would find it fresh while it
//! holds older code.
//!
//! The cargo shim records the JSON messages of every warm cargo call
//! (`Log`). `Watch::used` works out from them, and from the fingerprints
//! Cargo wrote after a threshold, which units the build used; `sweep`
//! deletes the rest. Whatever cannot be accounted for makes the observation
//! incomplete, and the caller then rebuilds the base from scratch.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::freshness::{files, is_real_dir, subdirs};

/// What `sweep` deleted from a seeded target dir.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pruned {
    pub units: u64,
    pub bytes: u64,
}

/// One line of a cargo call's record.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Record {
    /// A call began; `root` is its target dir relative to the base's.
    Start {
        root: PathBuf,
    },
    /// A call that may build units Ferry cannot see.
    Unobservable {
        why: String,
    },
    Unit(UnitRecord),
    End {
        status: i32,
    },
}

/// A `compiler-artifact` message, or a `build-script-executed` one (no
/// `fresh`).
#[derive(Debug, Serialize, Deserialize)]
struct UnitRecord {
    package: String,
    kinds: Vec<String>,
    name: String,
    test: bool,
    fresh: Option<bool>,
    files: Vec<FileRecord>,
}

/// A path the message named, relative to the call's target dir, as it was
/// when the message arrived: a later call may relink an uplifted path.
#[derive(Debug, Serialize, Deserialize)]
struct FileRecord {
    path: PathBuf,
    stat: Option<Stat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Stat {
    id: crate::sys::FileId,
    len: u64,
    mtime: (i64, i64),
}

impl Stat {
    /// `path` itself, whose `symlink_metadata` is `meta`; `None` if it is
    /// gone by now.
    fn of(path: &Path, meta: &fs::Metadata) -> Option<Self> {
        let stat = crate::sys::stat(path, meta).ok()?;
        Some(Self {
            id: stat.id,
            len: stat.len,
            mtime: stat.mtime,
        })
    }
}

/// The shim's record of one warm cargo call: a file of its own in the
/// records dir, so concurrent calls never interleave.
#[derive(Debug)]
pub(crate) struct Log {
    out: BufWriter<File>,
    path: PathBuf,
    target: PathBuf,
}

impl Log {
    /// Records a call that may build units Ferry cannot see.
    pub(crate) fn unobservable(dir: &Path, why: &str) -> Result<()> {
        let mut log = Self::create(dir, PathBuf::new())?;
        log.write(&Record::Unobservable {
            why: why.to_owned(),
        })?;
        log.flush()
    }

    /// Starts the record of a call that builds into `target`, which is
    /// `root` inside the base's target dir.
    pub(crate) fn start(dir: &Path, target: &Path, root: &Path) -> Result<Self> {
        let mut log = Self::create(dir, target.to_path_buf())?;
        log.write(&Record::Start {
            root: root.to_path_buf(),
        })?;
        // So that a call that dies leaves a start without an end.
        log.flush()?;
        Ok(log)
    }

    /// Records `line` if it is one of Cargo's JSON messages, and says
    /// whether it was.
    pub(crate) fn message(&mut self, line: &[u8]) -> Result<bool> {
        let Some(message) = cargo_message(line) else {
            return Ok(false);
        };
        if let Some(unit) = self.unit(message) {
            self.write(&Record::Unit(unit))?;
        }
        Ok(true)
    }

    pub(crate) fn end(mut self, status: i32) -> Result<()> {
        self.write(&Record::End { status })?;
        self.flush()
    }

    fn create(dir: &Path, target: PathBuf) -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = dir.join(format!("{}-{nanos}.jsonl", std::process::id()));
        let file = File::create_new(&path).at("create", &path)?;
        Ok(Self {
            out: BufWriter::new(file),
            path,
            target,
        })
    }

    fn write(&mut self, record: &Record) -> Result<()> {
        serde_json::to_writer(&mut self.out, record).map_err(|source| Error::Metadata {
            path: self.path.clone(),
            source,
        })?;
        self.out.write_all(b"\n").at("write", &self.path)
    }

    fn flush(&mut self) -> Result<()> {
        self.out.flush().at("write", &self.path)
    }

    fn unit(&self, message: Message) -> Option<UnitRecord> {
        let fresh = match message.reason.as_str() {
            // A missing `fresh` is treated as rebuilt, which must then be
            // visible in the fingerprints.
            "compiler-artifact" => Some(message.fresh.unwrap_or(false)),
            "build-script-executed" => None,
            _ => return None,
        };
        let (kinds, name) = message.target.map(|t| (t.kind, t.name)).unwrap_or_default();
        let files = message
            .filenames
            .into_iter()
            .chain(message.executable)
            .chain(message.out_dir)
            .filter_map(|path| {
                let stat = fs::symlink_metadata(&path)
                    .ok()
                    .and_then(|m| Stat::of(&path, &m));
                let path = path.strip_prefix(&self.target).ok()?.to_path_buf();
                Some(FileRecord { path, stat })
            })
            .collect();
        Some(UnitRecord {
            package: package_name(&message.package_id).to_owned(),
            kinds,
            name,
            test: message.profile.is_some_and(|p| p.test),
            fresh,
            files,
        })
    }
}

/// The fields of Cargo's JSON messages that Ferry reads.
#[derive(Debug, Deserialize)]
struct Message {
    reason: String,
    #[serde(default)]
    package_id: String,
    target: Option<MessageTarget>,
    profile: Option<MessageProfile>,
    fresh: Option<bool>,
    #[serde(default)]
    filenames: Vec<PathBuf>,
    executable: Option<PathBuf>,
    out_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct MessageTarget {
    kind: Vec<String>,
    name: String,
}

#[derive(Debug, Deserialize)]
struct MessageProfile {
    test: bool,
}

/// `line` parsed, if it is one of Cargo's messages rather than output of a
/// program cargo ran.
fn cargo_message(line: &[u8]) -> Option<Message> {
    const REASONS: &[&str] = &[
        "compiler-artifact",
        "compiler-message",
        "build-script-executed",
        "build-finished",
        "timing-info",
    ];
    if !line.starts_with(br#"{"reason":""#) {
        return None;
    }
    let message: Message = serde_json::from_slice(line).ok()?;
    REASONS
        .contains(&message.reason.as_str())
        .then_some(message)
}

/// The package name in a package id: `path+file:///ws/leaf#0.1.0`,
/// `path+file:///ws/dir#leaf@0.1.0`, `registry+https://...#serde@1.0.0`.
fn package_name(id: &str) -> &str {
    let (url, fragment) = id.rsplit_once('#').unwrap_or((id, ""));
    if let Some((name, _)) = fragment.split_once('@') {
        return name;
    }
    let path = url.split('?').next().unwrap_or(url);
    path.rsplit('/').next().unwrap_or(path)
}

/// Where a warm build's records go, and the threshold after which a file in
/// the target dir counts as written by it.
#[derive(Debug)]
pub(crate) struct Watch {
    dir: PathBuf,
    since: std::result::Result<SystemTime, String>,
}

/// Which units of a target dir a warm build used, or why Ferry cannot tell.
#[derive(Debug)]
pub(crate) enum Observation {
    Used(Used),
    Incomplete(String),
}

/// The units a warm build did not use.
#[derive(Debug, Default)]
pub(crate) struct Used {
    unused: Vec<PathBuf>,
    units: u64,
}

impl Watch {
    /// Creates an empty records dir at `dir`. Call it once the target dir is
    /// in place: every file there now counts as inherited, every file
    /// written later as built by the warm commands. `seed_meta` is the seed's
    /// `base.json`, which a sane clock puts before the threshold.
    pub(crate) fn start(dir: &Path, seed_meta: Option<&Path>) -> Result<Self> {
        crate::fsutil::remove_dir_all_force(dir)?;
        fs::create_dir_all(dir).at("create directory", dir)?;
        let seeded = match seed_meta {
            Some(meta) => Some(mtime(meta)?),
            None => None,
        };
        let since = match threshold(dir)? {
            None => Err("file timestamps in the store did not advance".to_owned()),
            Some(t) if seeded.is_some_and(|s| t <= s) => {
                Err("the clock is behind the seed's build".to_owned())
            }
            Some(t) => Ok(t),
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            since,
        })
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Forgets the calls recorded so far, for a rebuild into an emptied
    /// target dir.
    pub(crate) fn clear(&self) -> Result<()> {
        for path in record_files(&self.dir)? {
            fs::remove_file(&path).at("remove", &path)?;
        }
        Ok(())
    }

    pub(crate) fn remove(self) -> Result<()> {
        crate::fsutil::remove_dir_all_force(&self.dir)
    }

    /// Which units of `target` the recorded calls used.
    pub(crate) fn used(&self, target: &Path) -> Result<Observation> {
        let check = || {
            let since = match &self.since {
                Ok(since) => *since,
                Err(why) => return Err(Gap::Incomplete(why.clone())),
            };
            let records = read_records(&self.dir)?;
            // The warm commands may leave directories Ferry cannot read (a
            // test's mode-000 scratch dir, say), and units it cannot see
            // cannot be accounted for.
            used(target, &records.units, records.failed, since).map_err(|gap| match gap {
                Gap::Failed(e) => Gap::Incomplete(format!("cannot read the target dir: {e}")),
                incomplete @ Gap::Incomplete(_) => incomplete,
            })
        };
        match check() {
            Ok(used) => Ok(Observation::Used(used)),
            Err(Gap::Incomplete(why)) => Ok(Observation::Incomplete(why)),
            Err(Gap::Failed(e)) => Err(e),
        }
    }
}

/// Deletes the units a warm build did not use. Uplifted files, `doc/` and
/// `incremental/` stay: the next build that links an uplifted file relinks
/// it, and rustc never reads another unit's incremental state.
pub(crate) fn sweep(used: &Used) -> Result<Pruned> {
    let mut bytes = 0;
    for path in &used.unused {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).at("stat", path),
        };
        bytes += crate::fsutil::disk_usage(path)?;
        if meta.is_dir() {
            crate::fsutil::remove_dir_all_force(path)?;
        } else {
            fs::remove_file(path).at("remove", path)?;
        }
    }
    Ok(Pruned {
        units: used.units,
        bytes,
    })
}

/// A file's mtime strictly newer than that of a file created just before,
/// so that every file written afterwards is at least as new; `None` if the
/// clock does not advance within a few seconds.
fn threshold(dir: &Path) -> Result<Option<SystemTime>> {
    let probe = dir.join("started");
    let touch = || {
        match fs::remove_file(&probe) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).at("remove", &probe);
            }
            _ => {}
        }
        File::create_new(&probe).at("create", &probe)?;
        mtime(&probe)
    };
    let first = touch()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let now = touch()?;
        if now > first {
            return Ok(Some(now));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(None)
}

fn mtime(path: &Path) -> Result<SystemTime> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .at("stat", path)
}

fn record_files(dir: &Path) -> Result<Vec<PathBuf>> {
    Ok(files(dir)?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect())
}

/// Why [`Watch::used`] has no answer.
enum Gap {
    Incomplete(String),
    Failed(Error),
}

impl From<Error> for Gap {
    fn from(e: Error) -> Self {
        Self::Failed(e)
    }
}

type Check<T> = std::result::Result<T, Gap>;

fn incomplete<T>(why: String) -> Check<T> {
    Err(Gap::Incomplete(why))
}

/// What the calls that built into the base's own target dir recorded.
/// Nested workspaces' target dirs are emptied before a build that sweeps,
/// so whatever is in them was built by it.
#[derive(Debug)]
struct Records {
    units: Vec<UnitRecord>,
    /// One of them exited non-zero.
    failed: bool,
}

fn read_records(dir: &Path) -> Check<Records> {
    let mut records = Records {
        units: Vec::new(),
        failed: false,
    };
    for path in record_files(dir)? {
        let text = fs::read(&path).at("read", &path)?;
        let (mut root, mut ended) = (None, false);
        for line in text.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let Ok(record) = serde_json::from_slice::<Record>(line) else {
                return incomplete(format!("{} is unreadable", path.display()));
            };
            let own_target = root.as_deref() == Some(Path::new(""));
            match record {
                Record::Unobservable { why } => return incomplete(why),
                Record::Start { root: r } => root = Some(r),
                Record::Unit(unit) if own_target => records.units.push(unit),
                Record::Unit(_) => {}
                Record::End { status } => {
                    ended = true;
                    records.failed |= own_target && status != 0;
                }
            }
        }
        if !ended {
            return incomplete("a cargo call did not finish".into());
        }
    }
    Ok(records)
}

/// A unit in a profile dir: everything named by its hash.
#[derive(Debug, Default)]
struct Unit {
    package: Option<String>,
    /// Absent for leftovers whose fingerprint was deleted.
    fingerprint: Option<PathBuf>,
    /// Names of the files in the fingerprint dir (`lib-leaf`, `bin-app`).
    fingerprint_files: Vec<String>,
    /// Outputs an uplifted file may be a hard link to or a copy of.
    outputs: Vec<PathBuf>,
    /// What [`sweep`] deletes.
    paths: Vec<PathBuf>,
    /// Cargo wrote its fingerprint after the threshold.
    written: bool,
    /// A message of the build named it.
    claimed: bool,
}

/// The units of each profile dir (relative to the target dir), by hash.
type Profiles = BTreeMap<PathBuf, BTreeMap<String, Unit>>;

/// Which units of `target` the build used. A unit Cargo wrote but never
/// reported was either built by a call Ferry did not see or, after a call
/// that failed, one that failed to build: Cargo writes a failing unit's
/// fingerprint dir (and empties its hash, so nothing finds it fresh) but
/// sends no message for it.
fn used(
    target: &Path,
    records: &[UnitRecord],
    a_call_failed: bool,
    since: SystemTime,
) -> Check<Used> {
    let mut profiles = Profiles::new();
    for dir in profile_dirs(target)? {
        let rel = dir.strip_prefix(target).unwrap_or(&dir).to_path_buf();
        profiles.insert(rel, scan(&dir, since)?);
    }
    for record in records {
        claim(&mut profiles, record)?;
    }
    let mut used = Used::default();
    for units in profiles.into_values() {
        for unit in units.into_values() {
            match (unit.written, unit.claimed) {
                (true, false) if !a_call_failed => {
                    return incomplete(format!(
                        "{} was built by a cargo call Ferry did not see",
                        unit.paths
                            .first()
                            .map_or_else(String::new, |p| p.display().to_string())
                    ));
                }
                (_, false) => {
                    used.units += 1;
                    used.unused.extend(unit.paths);
                }
                (_, true) => {}
            }
        }
    }
    Ok(used)
}

/// Directories holding a `.cargo-lock`, which Cargo creates in every
/// profile dir (`debug`, `<triple>/release`, ...) in both layouts. Nested
/// workspaces' target dirs are left out.
fn profile_dirs(target: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut stack = vec![target.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for sub in subdirs(&dir)? {
            if dir == target && sub.ends_with(crate::shim::NESTED_TARGETS) {
                continue;
            }
            if sub.join(".cargo-lock").is_file() {
                found.push(sub);
            } else {
                stack.push(sub);
            }
        }
    }
    Ok(found)
}

/// The units of one profile dir, in either Cargo layout.
fn scan(profile: &Path, since: SystemTime) -> Check<BTreeMap<String, Unit>> {
    for dir in subdirs(profile)? {
        let name = file_name(&dir);
        let known = matches!(
            name,
            ".fingerprint" | "build" | "deps" | "examples" | "incremental"
        ) || name.ends_with(".dSYM");
        if !known {
            return incomplete(format!("unknown build-dir layout: {}", dir.display()));
        }
    }
    let mut units: BTreeMap<String, Unit> = BTreeMap::new();
    let add_fingerprint = |unit: &mut Unit, package: &str, dir: PathBuf| -> Result<()> {
        unit.package = Some(package.to_owned());
        for file in files(&dir)? {
            unit.written |= fs::symlink_metadata(&file)
                .and_then(|m| m.modified())
                .at("stat", &file)?
                >= since;
            unit.fingerprint_files.push(file_name(&file).to_owned());
        }
        unit.fingerprint = Some(dir);
        Ok(())
    };

    // Up to Cargo 1.99: .fingerprint/<package>-<hash>/, build scripts in
    // build/<package>-<hash>/, outputs named <name>-<hash>[.ext] in deps/
    // and examples/.
    for dir in subdirs(&profile.join(".fingerprint"))? {
        let Some((package, hash)) = split_hash(file_name(&dir)) else {
            return incomplete(format!("unknown fingerprint dir {}", dir.display()));
        };
        let (package, hash) = (package.to_owned(), hash.to_owned());
        let unit = units.entry(hash).or_default();
        unit.paths.push(dir.clone());
        add_fingerprint(unit, &package, dir)?;
    }
    for dir in subdirs(&profile.join("build"))? {
        if let Some((_, hash)) = split_hash(file_name(&dir)) {
            units.entry(hash.to_owned()).or_default().paths.push(dir);
            continue;
        }
        // From Cargo 1.100: build/<package>/<hash>/{fingerprint,out,run}/.
        let package = file_name(&dir).to_owned();
        for unit_dir in subdirs(&dir)? {
            let hash = file_name(&unit_dir).to_owned();
            if !is_hash(&hash) {
                return incomplete(format!("unknown unit dir {}", unit_dir.display()));
            }
            let unit = units.entry(hash).or_default();
            let fingerprint = unit_dir.join("fingerprint");
            if is_real_dir(&fingerprint)? {
                add_fingerprint(unit, &package, fingerprint)?;
            }
            unit.outputs.extend(files(&unit_dir.join("out"))?);
            unit.paths.push(unit_dir);
        }
    }
    for dir in [profile.join("deps"), profile.join("examples")] {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => Err(e).at("read directory", &dir)?,
        };
        for entry in entries {
            let path = entry.at("read directory", &dir)?.path();
            // Unhashed names are uplifted examples and the single-slot
            // outputs of path dylibs, which the next build overwrites.
            if let Some(hash) = output_hash(file_name(&path)) {
                let unit = units.entry(hash.to_owned()).or_default();
                unit.outputs.push(path.clone());
                unit.paths.push(path);
            }
        }
    }
    Ok(units)
}

/// Marks the units a message names as used.
fn claim(profiles: &mut Profiles, record: &UnitRecord) -> Check<()> {
    let what = || {
        format!(
            "the {} `{}` of {}",
            record.kinds.join(","),
            record.name,
            record.package
        )
    };
    // A path inside the unit's own dir or named by its hash identifies it.
    let mut named = Vec::new();
    for file in &record.files {
        if let Some((profile, rel)) = locate(profiles, &file.path)
            && let Some(hash) = path_hash(rel)
        {
            named.push((profile.to_path_buf(), hash.to_owned()));
        }
    }
    if !named.is_empty() {
        for (profile, hash) in named {
            let unit = profiles
                .get_mut(&profile)
                .and_then(|units| units.get_mut(&hash))
                .filter(|u| u.fingerprint.is_some());
            let Some(unit) = unit else {
                return incomplete(format!("Cargo reported {} with no fingerprint", what()));
            };
            if record.fresh == Some(false) && !unit.written {
                return incomplete(format!(
                    "Cargo rebuilt {} but its fingerprint predates the build",
                    what()
                ));
            }
            unit.claimed = true;
        }
        return Ok(());
    }

    // Only uplifted paths (binaries, examples, root proc macros and dylibs):
    // the candidates are the units of that package and target in the
    // profile dir.
    let mut candidates = candidates(profiles, record);
    if candidates.is_empty() {
        return incomplete(format!("no unit in the target dir matches {}", what()));
    }
    if record.fresh != Some(true) {
        let mut any = false;
        for unit in candidates.into_iter().filter(|u| u.written) {
            unit.claimed = true;
            any = true;
        }
        if !any {
            return incomplete(format!(
                "Cargo rebuilt {} but no fingerprint shows it",
                what()
            ));
        }
        return Ok(());
    }
    let stats: Vec<Stat> = record.files.iter().filter_map(|f| f.stat).collect();
    if candidates.len() > 1 {
        let outputs = |unit: &Unit| -> Vec<Stat> {
            unit.outputs
                .iter()
                .filter_map(|p| Stat::of(p, &fs::symlink_metadata(p).ok()?))
                .collect()
        };
        // Cargo hard-links uplifted files on Linux and copies them on macOS.
        let same_inode = |s: &Stat, o: &Stat| s.id == o.id;
        let same_copy = |s: &Stat, o: &Stat| (s.len, s.mtime) == (o.len, o.mtime);
        for same in [same_inode, same_copy] {
            let matching: Vec<bool> = candidates
                .iter()
                .map(|u| {
                    let outputs = outputs(u);
                    stats.iter().any(|s| outputs.iter().any(|o| same(s, o)))
                })
                .collect();
            if matching.iter().filter(|m| **m).count() == 1 {
                candidates = candidates
                    .into_iter()
                    .zip(matching)
                    .filter_map(|(u, m)| m.then_some(u))
                    .collect();
                break;
            }
        }
    }
    match <[&mut Unit; 1]>::try_from(candidates) {
        Ok([unit]) => {
            unit.claimed = true;
            Ok(())
        }
        Err(candidates) => incomplete(format!(
            "cannot tell which of {} units is {}",
            candidates.len(),
            what()
        )),
    }
}

/// The units that can have produced `record`'s uplifted files: those of its
/// package whose fingerprint dir has the file Cargo names after the target.
fn candidates<'a>(profiles: &'a mut Profiles, record: &UnitRecord) -> Vec<&'a mut Unit> {
    let mut example = false;
    let mut doc = false;
    let mut in_profiles = Vec::new();
    for file in &record.files {
        if let Some((profile, rel)) = locate(profiles, &file.path) {
            example |= rel.starts_with("examples");
            if !in_profiles.iter().any(|p| p == profile) {
                in_profiles.push(profile.to_path_buf());
            }
        } else {
            // `doc/` or `<triple>/doc/`; `debug/doc` is a binary.
            doc |= file
                .path
                .components()
                .take(2)
                .any(|c| c.as_os_str() == "doc");
        }
    }
    let kind = if example {
        "example"
    } else {
        record
            .kinds
            .iter()
            .find_map(|k| match k.as_str() {
                "bin" => Some("bin"),
                "example" => Some("example"),
                "test" => Some("integration-test"),
                "bench" => Some("bench"),
                "custom-build" => Some("build-script"),
                _ => None,
            })
            .unwrap_or("lib")
    };
    let flavor = if record.test {
        "test-"
    } else if doc {
        "doc-"
    } else {
        ""
    };
    let file = format!("{flavor}{kind}-{}", record.name);
    profiles
        .iter_mut()
        // Docs go to <target>/doc whatever the profile.
        .filter(|(rel, _)| doc || in_profiles.iter().any(|p| p == *rel))
        .flat_map(|(_, units)| units.values_mut())
        .filter(|u| {
            u.fingerprint.is_some()
                && u.package.as_deref() == Some(record.package.as_str())
                && u.fingerprint_files.contains(&file)
        })
        .collect()
}

/// The profile dir containing `path`, and `path` relative to it.
fn locate<'a>(profiles: &'a Profiles, path: &'a Path) -> Option<(&'a Path, &'a Path)> {
    profiles
        .keys()
        .filter_map(|p| Some((p.as_path(), path.strip_prefix(p).ok()?)))
        .max_by_key(|(p, _)| p.components().count())
}

/// The unit hash in a path relative to its profile dir:
/// `build/<package>-<hash>/...` and `deps/<name>-<hash>.<ext>` up to Cargo
/// 1.99, `build/<package>/<hash>/...` from 1.100.
fn path_hash(rel: &Path) -> Option<&str> {
    let mut parts = rel.components().map(|c| c.as_os_str().to_str());
    match (parts.next()??, parts.next()??) {
        ("build", dir) => split_hash(dir)
            .map(|(_, hash)| hash)
            .or_else(|| parts.next()?.filter(|h| is_hash(h))),
        ("deps" | "examples", file) => output_hash(file),
        _ => None,
    }
}

/// `<package>-<hash>` split at its last `-`.
fn split_hash(name: &str) -> Option<(&str, &str)> {
    name.rsplit_once('-').filter(|(_, hash)| is_hash(hash))
}

/// The hash in an output's name: the first `-<hash>` followed by `.` or the
/// end (`libleaf-<hash>.rmeta`, `app-<hash>`, `app-<hash>.app.<n>.rcgu.o`).
fn output_hash(name: &str) -> Option<&str> {
    name.match_indices('-').find_map(|(at, _)| {
        let hash = name.get(at + 1..at + 17)?;
        let rest = &name[at + 17..];
        (is_hash(hash) && (rest.is_empty() || rest.starts_with('.'))).then_some(hash)
    })
}

/// Cargo's unit hashes are 16 lowercase hex digits.
fn is_hash(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const H1: &str = "1111111111111111";
    const H2: &str = "2222222222222222";
    const H3: &str = "3333333333333333";
    const H4: &str = "4444444444444444";

    /// A synthetic target dir. Files are written before the threshold
    /// (inherited from the seed) unless written with `new_file`.
    struct Tree {
        _tmp: tempfile::TempDir,
        target: PathBuf,
        since: SystemTime,
    }

    impl Tree {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let tree = Self {
                target: tmp.path().join("target"),
                _tmp: tmp,
                since: SystemTime::now(),
            };
            tree.old("debug/.cargo-lock", "");
            tree
        }

        fn put(&self, rel: &str, contents: &str, mtime: SystemTime) -> PathBuf {
            let path = self.target.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            path
        }

        fn old(&self, rel: &str, contents: &str) -> PathBuf {
            self.put(rel, contents, self.since - Duration::from_secs(3600))
        }

        fn new_file(&self, rel: &str) -> PathBuf {
            self.put(rel, "", self.since + Duration::from_secs(1))
        }

        /// A legacy fingerprint dir holding `files`.
        fn fingerprint(&self, profile: &str, unit: &str, files: &[&str]) {
            for file in files {
                self.old(&format!("{profile}/.fingerprint/{unit}/{file}"), "");
            }
        }

        fn link(&self, from: &str, to: &str) {
            let to = self.target.join(to);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::hard_link(self.target.join(from), to).unwrap();
        }

        /// A message as the shim records it: paths relative to the target
        /// dir, statted now.
        fn unit(
            &self,
            package: &str,
            kind: &str,
            name: &str,
            fresh: Option<bool>,
            paths: &[&str],
        ) -> UnitRecord {
            UnitRecord {
                package: package.into(),
                kinds: vec![kind.into()],
                name: name.into(),
                test: false,
                fresh,
                files: paths
                    .iter()
                    .map(|p| FileRecord {
                        path: p.into(),
                        stat: {
                            let path = self.target.join(p);
                            fs::symlink_metadata(&path)
                                .ok()
                                .and_then(|m| Stat::of(&path, &m))
                        },
                    })
                    .collect(),
            }
        }

        /// The unused paths, relative and sorted, or why they are unknown.
        fn unused(&self, records: &[UnitRecord]) -> std::result::Result<Vec<String>, String> {
            self.unused_after(records, false)
        }

        /// [`Self::unused`] when `a_call_failed` or not.
        fn unused_after(
            &self,
            records: &[UnitRecord],
            a_call_failed: bool,
        ) -> std::result::Result<Vec<String>, String> {
            match used(&self.target, records, a_call_failed, self.since) {
                Ok(used) => {
                    let mut paths: Vec<String> = used
                        .unused
                        .iter()
                        .map(|p| {
                            p.strip_prefix(&self.target)
                                .unwrap()
                                .display()
                                .to_string()
                                .replace(std::path::MAIN_SEPARATOR, "/")
                        })
                        .collect();
                    paths.sort();
                    Ok(paths)
                }
                Err(Gap::Incomplete(why)) => Err(why),
                Err(Gap::Failed(e)) => panic!("{e}"),
            }
        }
    }

    #[test]
    fn units_the_build_did_not_name_are_unused_with_their_outputs() {
        let t = Tree::new();
        t.fingerprint(
            "debug",
            &format!("leaf-{H1}"),
            &["lib-leaf", "lib-leaf.json"],
        );
        t.old(&format!("debug/deps/libleaf-{H1}.rlib"), "");
        t.old(&format!("debug/deps/libleaf-{H1}.rmeta"), "");
        t.fingerprint("debug", &format!("leaf-{H2}"), &["lib-leaf"]);
        t.old(&format!("debug/deps/libleaf-{H2}.rlib"), "");
        t.old(&format!("debug/deps/leaf-{H2}.d"), "");
        // An orphan: its fingerprint is gone.
        t.old(&format!("debug/deps/libgone-{H3}.rlib"), "");
        t.old("debug/libleaf.rlib", "");
        t.old("debug/incremental/leaf-abc/s-1/x", "");
        t.old("doc/leaf/index.html", "");

        let rmeta = format!("debug/deps/libleaf-{H1}.rmeta");
        let leaf = t.unit(
            "leaf",
            "lib",
            "leaf",
            Some(true),
            &["debug/libleaf.rlib", &rmeta],
        );
        assert_eq!(
            t.unused(&[leaf]).unwrap(),
            [
                format!("debug/.fingerprint/leaf-{H2}"),
                format!("debug/deps/leaf-{H2}.d"),
                format!("debug/deps/libgone-{H3}.rlib"),
                format!("debug/deps/libleaf-{H2}.rlib"),
            ]
        );

        let Ok(nothing_used) = used(&t.target, &[], false, t.since) else {
            panic!("incomplete")
        };
        assert_eq!(sweep(&nothing_used).unwrap().units, 3);
        assert!(
            !t.target
                .join(format!("debug/.fingerprint/leaf-{H2}"))
                .exists()
        );
        for kept in [
            "debug/libleaf.rlib",
            "debug/incremental/leaf-abc/s-1/x",
            "doc/leaf/index.html",
        ] {
            assert!(t.target.join(kept).exists(), "{kept}");
        }
    }

    #[test]
    fn an_uplifted_binary_is_matched_by_inode_then_as_a_copy() {
        let t = Tree::new();
        for (hash, contents) in [(H1, "one"), (H2, "two!")] {
            t.fingerprint("debug", &format!("app-{hash}"), &["bin-app"]);
            t.old(&format!("debug/deps/app-{hash}"), contents);
        }
        let app = |t: &Tree| t.unit("app", "bin", "app", Some(true), &["debug/app"]);

        t.link(&format!("debug/deps/app-{H1}"), "debug/app");
        assert_eq!(
            t.unused(&[app(&t)]).unwrap(),
            [
                format!("debug/.fingerprint/app-{H2}"),
                format!("debug/deps/app-{H2}")
            ]
        );

        // A copy, as on macOS: the length and mtime of the unit's output.
        fs::remove_file(t.target.join("debug/app")).unwrap();
        let mtime = fs::metadata(t.target.join(format!("debug/deps/app-{H2}")))
            .unwrap()
            .modified()
            .unwrap();
        t.put("debug/app", "two!", mtime);
        assert_eq!(
            t.unused(&[app(&t)]).unwrap(),
            [
                format!("debug/.fingerprint/app-{H1}"),
                format!("debug/deps/app-{H1}")
            ]
        );

        t.put("debug/app", "neither", SystemTime::now());
        let why = t.unused(&[app(&t)]).unwrap_err();
        assert!(why.contains("cannot tell which of 2 units"), "{why}");

        // With one candidate there is nothing to tell apart.
        fs::remove_dir_all(t.target.join(format!("debug/.fingerprint/app-{H2}"))).unwrap();
        assert_eq!(
            t.unused(&[app(&t)]).unwrap(),
            [format!("debug/deps/app-{H2}")]
        );
    }

    /// `debug/doc` is a binary named `doc`, not documentation, which lies
    /// outside every profile dir.
    #[test]
    fn binaries_and_examples_named_doc_are_not_documentation() {
        let t = Tree::new();
        t.fingerprint("debug", &format!("app-{H1}"), &["bin-doc"]);
        t.old(&format!("debug/deps/doc-{H1}"), "");
        t.link(&format!("debug/deps/doc-{H1}"), "debug/doc");
        t.fingerprint("debug", &format!("app-{H2}"), &["example-doc"]);
        t.old(&format!("debug/examples/doc-{H2}"), "");
        t.link(&format!("debug/examples/doc-{H2}"), "debug/examples/doc");
        t.fingerprint("debug", &format!("doc-{H3}"), &["doc-lib-doc"]);
        t.old("doc/doc/index.html", "");
        let records = [
            t.unit("app", "bin", "doc", Some(true), &["debug/doc"]),
            t.unit("app", "example", "doc", Some(true), &["debug/examples/doc"]),
            t.unit("doc", "lib", "doc", Some(true), &["doc/doc/index.html"]),
        ];
        assert_eq!(t.unused(&records).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn build_scripts_tests_and_single_slot_libraries_are_found() {
        let t = Tree::new();
        let compile = format!("embed-{H1}");
        t.fingerprint("debug", &compile, &["build-script-build-script-build"]);
        t.old(&format!("debug/build/{compile}/build-script-build"), "");
        let run = format!("embed-{H2}");
        t.fingerprint("debug", &run, &["run-build-script-build-script-build"]);
        t.old(&format!("debug/build/{run}/out/gen.rs"), "");
        t.fingerprint("debug", &format!("app-{H3}"), &["test-bin-app"]);
        t.old(&format!("debug/deps/app-{H3}"), "");
        // A path cdylib: unhashed outputs, fingerprint named by the package.
        t.fingerprint("debug", &format!("cdy-{H4}"), &["lib-cdy"]);
        t.old("debug/deps/libcdy.so", "x");
        t.old("debug/libcdy.so", "x");

        let mut test = t.unit(
            "app",
            "bin",
            "app",
            Some(true),
            &[&format!("debug/deps/app-{H3}")],
        );
        test.test = true;
        let records = [
            t.unit(
                "embed",
                "custom-build",
                "build-script-build",
                Some(true),
                &[&format!("debug/build/{compile}/build-script-build")],
            ),
            t.unit("embed", "", "", None, &[&format!("debug/build/{run}/out")]),
            test,
            t.unit("cdy", "cdylib", "cdy", Some(true), &["debug/libcdy.so"]),
        ];
        assert_eq!(t.unused(&records).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn rebuilt_units_must_show_in_the_fingerprints_and_be_reported() {
        let t = Tree::new();
        t.new_file(&format!("debug/.fingerprint/leaf-{H1}/lib-leaf"));
        let leaf_rmeta = format!("debug/deps/libleaf-{H1}.rmeta");
        t.old(&leaf_rmeta, "");
        let rebuilt = t.unit("leaf", "lib", "leaf", Some(false), &[&leaf_rmeta]);
        assert_eq!(t.unused(&[rebuilt]).unwrap(), Vec::<String>::new());

        let why = t.unused(&[]).unwrap_err();
        assert!(why.contains("did not see"), "{why}");

        t.fingerprint("debug", &format!("mid-{H2}"), &["lib-mid"]);
        let mid_rmeta = format!("debug/deps/libmid-{H2}.rmeta");
        t.old(&mid_rmeta, "");
        let records = [
            t.unit("leaf", "lib", "leaf", Some(true), &[&leaf_rmeta]),
            t.unit("mid", "lib", "mid", Some(false), &[&mid_rmeta]),
        ];
        let why = t.unused(&records).unwrap_err();
        assert!(why.contains("predates the build"), "{why}");

        let side_rmeta = format!("debug/deps/libside-{H3}.rmeta");
        let missing = [
            t.unit("leaf", "lib", "leaf", Some(true), &[&leaf_rmeta]),
            t.unit("side", "lib", "side", Some(true), &[&side_rmeta]),
        ];
        let why = t.unused(&missing).unwrap_err();
        assert!(why.contains("no fingerprint"), "{why}");
    }

    /// Under `--keep-going`, Cargo writes the fingerprint dir of a unit that
    /// fails to compile (`invoked.timestamp`, `output-*`) and reports nothing
    /// for it. After a failed call that is expected, not a sign of a call
    /// Ferry did not see, and the unit is as good as unused.
    #[test]
    fn units_a_failed_call_wrote_but_never_reported_are_unused() {
        let t = Tree::new();
        let failed = format!("debug/.fingerprint/app-{H1}");
        t.new_file(&format!("{failed}/invoked.timestamp"));
        t.new_file(&format!("{failed}/output-example-broken"));
        t.fingerprint("debug", &format!("leaf-{H2}"), &["lib-leaf"]);
        let rmeta = format!("debug/deps/libleaf-{H2}.rmeta");
        t.old(&rmeta, "");
        let leaf = |t: &Tree| t.unit("leaf", "lib", "leaf", Some(true), &[&rmeta]);

        let why = t.unused(&[leaf(&t)]).unwrap_err();
        assert!(why.contains("did not see"), "{why}");
        assert_eq!(t.unused_after(&[leaf(&t)], true).unwrap(), [failed]);
    }

    #[cfg(unix)]
    /// A directory the warm commands left unreadable (a test's mode-000
    /// scratch dir under `target/tmp`, say) may hide a profile dir, so Ferry
    /// cannot tell what is in the target; that is no reason to fail the base.
    #[test]
    fn an_unreadable_directory_makes_the_observation_incomplete() {
        use std::os::unix::fs::PermissionsExt as _;
        let t = Tree::new();
        let locked = t.target.join("tmp/locked");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let readable_anyway = fs::read_dir(&locked).is_ok();
        let watch = Watch::start(&t.target.parent().unwrap().join("records"), None).unwrap();
        let observed = watch.used(&t.target);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        if readable_anyway {
            // Running as root.
            return;
        }
        match observed {
            Ok(Observation::Incomplete(why)) => assert!(why.contains("tmp/locked"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn new_layout_units_are_whole_directories() {
        let t = Tree::new();
        for (package, hash, file) in [
            ("leaf", H1, "lib-leaf"),
            ("leaf", H2, "lib-leaf"),
            ("app", H3, "bin-app"),
            ("app", H4, "bin-app"),
        ] {
            t.old(
                &format!("debug/build/{package}/{hash}/fingerprint/{file}"),
                "",
            );
        }
        let rmeta = format!("debug/build/leaf/{H1}/out/libleaf-{H1}.rmeta");
        t.old(&rmeta, "");
        t.old(&format!("debug/build/app/{H3}/out/app"), "three");
        t.old(&format!("debug/build/app/{H4}/out/app"), "four");
        t.link(&format!("debug/build/app/{H4}/out/app"), "debug/app");
        let records = [
            t.unit("leaf", "lib", "leaf", Some(true), &[&rmeta]),
            t.unit("app", "bin", "app", Some(true), &["debug/app"]),
        ];
        assert_eq!(
            t.unused(&records).unwrap(),
            [
                format!("debug/build/app/{H3}"),
                format!("debug/build/leaf/{H2}")
            ]
        );
    }

    #[test]
    fn every_profile_dir_is_scanned_and_unknown_layouts_are_refused() {
        let t = Tree::new();
        let triple = "x86_64-unknown-linux-gnu/debug";
        t.old(&format!("{triple}/.cargo-lock"), "");
        t.fingerprint(triple, &format!("leaf-{H1}"), &["lib-leaf"]);
        // Nested workspaces' target dirs are emptied before such a build.
        t.old("ferry-ws/fuzz/debug/.cargo-lock", "");
        t.fingerprint("ferry-ws/fuzz/debug", &format!("leaf-{H2}"), &["lib-leaf"]);
        assert_eq!(
            t.unused(&[]).unwrap(),
            [format!("{triple}/.fingerprint/leaf-{H1}")]
        );

        t.old("debug/surprise/x", "");
        let why = t.unused(&[]).unwrap_err();
        assert!(why.contains("unknown build-dir layout"), "{why}");
    }

    #[test]
    fn the_log_keeps_cargo_messages_and_unfinished_or_unobservable_calls_count() {
        let t = Tree::new();
        let records = t.target.parent().unwrap().join("records");
        fs::create_dir(&records).unwrap();
        let rmeta = t.target.join(format!("debug/deps/libleaf-{H1}.rmeta"));
        // Cargo puts `reason` first.
        let fields = serde_json::json!({
            "package_id": "path+file:///ws/leaf#0.1.0",
            "target": {"kind": ["lib"], "name": "leaf", "src_path": "/ws/leaf/src/lib.rs"},
            "profile": {"test": false, "opt_level": "0"},
            "fresh": true,
            "filenames": [rmeta, "/elsewhere/libleaf.rlib"],
            "executable": null,
        })
        .to_string();
        let message = format!(r#"{{"reason":"compiler-artifact",{}"#, &fields[1..]);
        let mut log = Log::start(&records, &t.target, Path::new("")).unwrap();
        assert!(log.message(format!("{message}\n").as_bytes()).unwrap());
        assert!(!log.message(b"marker=LEAF_BASE\n").unwrap());
        assert!(!log.message(br#"{"reason":"not-cargo"}"#).unwrap());
        assert!(
            log.message(br#"{"reason":"build-finished","success":true}"#)
                .unwrap()
        );
        log.end(0).unwrap();
        let Ok(Records { units, failed }) = read_records(&records) else {
            panic!("incomplete")
        };
        assert!(!failed);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].package, "leaf");
        let paths: Vec<&Path> = units[0].files.iter().map(|f| f.path.as_path()).collect();
        assert_eq!(
            paths,
            [PathBuf::from(format!("debug/deps/libleaf-{H1}.rmeta"))]
        );

        // A nested workspace's call does not build into the observed target.
        let nested = Log::start(&records, &t.target, Path::new("ferry-ws/fuzz")).unwrap();
        nested.end(101).unwrap();
        assert!(matches!(
            read_records(&records),
            Ok(Records { failed: false, .. })
        ));
        Log::start(&records, &t.target, Path::new(""))
            .unwrap()
            .end(101)
            .unwrap();
        assert!(matches!(
            read_records(&records),
            Ok(Records { failed: true, .. })
        ));

        drop(Log::start(&records, &t.target, Path::new("")).unwrap());
        let unfinished = read_records(&records);
        assert!(matches!(unfinished, Err(Gap::Incomplete(w)) if w.contains("did not finish")));

        crate::fsutil::clear_dir(&records).unwrap();
        Log::unobservable(&records, "`cargo nextest` is not a build").unwrap();
        let unobservable = read_records(&records);
        assert!(matches!(unobservable, Err(Gap::Incomplete(w)) if w.contains("nextest")));
    }

    #[test]
    fn a_clock_behind_the_seed_makes_the_observation_incomplete() {
        let t = Tree::new();
        let later = SystemTime::now() + Duration::from_secs(3600);
        let seed_meta = t.put("seed.json", "{}", later);
        let records = t.target.join("records");
        let watch = Watch::start(&records, Some(&seed_meta)).unwrap();
        let observed = watch.used(&t.target).unwrap();
        assert!(matches!(observed, Observation::Incomplete(w) if w.contains("clock")));

        let watch = Watch::start(&records, None).unwrap();
        assert!(matches!(
            watch.used(&t.target).unwrap(),
            Observation::Used(_)
        ));
    }

    #[test]
    fn names_are_parsed_as_cargo_writes_them() {
        assert_eq!(package_name("path+file:///ws/leaf#0.1.0"), "leaf");
        assert_eq!(package_name("path+file:///ws/dir#leaf@0.1.0"), "leaf");
        assert_eq!(
            package_name("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"),
            "serde"
        );
        assert_eq!(
            package_name("git+https://github.com/a/repo?branch=main#0.1.0"),
            "repo"
        );
        assert_eq!(output_hash(&format!("libleaf-{H1}.rmeta")), Some(H1));
        assert_eq!(output_hash(&format!("my-app-{H1}")), Some(H1));
        assert_eq!(
            output_hash(&format!("app-{H1}.app.{H2}-cgu.0.rcgu.o")),
            Some(H1)
        );
        assert_eq!(output_hash("libcdy.so"), None);
        assert_eq!(output_hash(&format!("app-{H1}x")), None);
        assert_eq!(split_hash(&format!("my-pkg-{H1}")), Some(("my-pkg", H1)));
        let hash = |p: String| path_hash(Path::new(&p)).map(str::to_owned);
        assert_eq!(hash(format!("build/embed/{H1}/out")), Some(H1.into()));
        assert_eq!(hash(format!("build/embed-{H1}/out")), Some(H1.into()));
        assert_eq!(hash("app".into()), None);
    }
}
