//! Building, listing and collecting immutable warm bases.
//!
//! INVARIANT: once `base.json` exists, nothing writes to that base again.
//! Agents fork from it read-only, overlay agents use its target dir as their
//! lower layer, and a refresh builds a new base next to it instead of
//! updating it. `gc` deletes a base only when no agent references it and it
//! is not current. A base built without the namespace sandbox also has its
//! relocation plan, `relocation.json`, written before `base.json`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(windows)]
use std::os::windows::io::AsHandle;
use std::process::{Command, Stdio};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::config::{SandboxMode, WarmCommand};
use crate::context::Context;
use crate::error::{Error, IoContext, Result};
use crate::fork::{BackendKind, ForkPaths, TargetSpec};
use crate::freshness::{self, FreshnessMode};
use crate::fsutil;
use crate::git::{self, CommitId};
use crate::lock::FileLock;
use crate::platform::Capabilities;
use crate::prune::{self, Observation, Pruned};
use crate::sandbox::{self, Isolation, Plan};
use crate::store::{BaseId, BasePaths, fnv1a64};
use crate::toolchain::Toolchain;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaseMeta {
    pub id: BaseId,
    pub commit: CommitId,
    pub created_unix: u64,
    pub build_secs: f64,
    pub freshness: FreshnessMode,
    pub isolation: Isolation,
    pub toolchain: Toolchain,
    pub commands: Vec<String>,
    pub ferry_version: String,
    /// Paths the warm commands created or modified inside the base checkout
    /// (build scripts writing outside `OUT_DIR`). Agents do not get these
    /// files, so they are reported as a warning.
    pub dirtied: Vec<String>,
    /// Built through the experimental `ferry-rustc` path-remapping wrapper;
    /// agents must use it too or rustc discards the incremental state.
    #[serde(default)]
    pub remap_paths: bool,
    /// See `unit_inputs`. A successor with the same value can be seeded from
    /// this base without pruning.
    #[serde(default)]
    pub unit_inputs: String,
    /// The earlier base whose target this one was seeded from.
    #[serde(default)]
    pub seeded_from: Option<BaseId>,
    /// Wall time of each entry of `commands`.
    #[serde(default)]
    pub command_secs: Vec<f64>,
    /// Optional (clippy) commands that failed, so whatever they could not
    /// check is not warm.
    #[serde(default)]
    pub incomplete: Vec<String>,
    /// Ferry saw every cargo call of the warm build (see `prune`). The warm
    /// commands are part of the fingerprint, so successors are likely to be
    /// observable too: only then is one with other unit inputs seeded from
    /// this base.
    #[serde(default)]
    pub observed: bool,
    /// What a refresh seeded across unit inputs deleted from the seed's units.
    #[serde(default)]
    pub pruned: Option<Pruned>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BuildReport {
    pub base: BaseMeta,
    /// True if an identical base already existed and was reused.
    pub reused: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BaseStatus {
    pub id: BaseId,
    pub ready: bool,
    pub current: bool,
    pub meta: Option<BaseMeta>,
    /// Agents forked from this base.
    pub agents: Vec<String>,
    pub disk_bytes: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct GcReport {
    pub removed: Vec<BaseId>,
    /// Bases skipped because another process is building them.
    pub busy: Vec<BaseId>,
    pub kept: Vec<BaseId>,
    /// Abandoned hook-created agents that were removed first.
    pub reaped_agents: Vec<String>,
    /// Hook-created agents gc could not check or remove; the rest of gc
    /// still ran.
    pub warnings: Vec<String>,
    pub freed_bytes: u64,
    pub dry_run: bool,
}

/// Builds (or reuses) the base for `rev` and makes it current.
///
/// The calling process enters the base's namespace sandbox to run the warm
/// commands; that is irreversible, so call this at most once per process.
pub fn build(ctx: &Context, rev: &str, cold: bool) -> Result<BuildReport> {
    let mut warnings = Vec::new();
    let commit = ctx.repo.resolve_commit(rev)?;
    let host_toolchain = Toolchain::detect(&ctx.repo.main_worktree)?;
    let freshness = freshness::resolve_mode(ctx.settings.freshness, &host_toolchain)?;
    let isolation = resolve_isolation(ctx, &mut warnings)?;
    for config in unapplied_cargo_configs(ctx)? {
        warnings.push(format!(
            "{}; {CONFIG_ADVICE}",
            unapplied_message(ctx, &config)
        ));
    }
    let remap = resolve_remap(ctx, isolation, &host_toolchain, &mut warnings);
    // What actually runs is part of the fingerprint, so installing clippy
    // later builds a new base instead of reusing one that never ran it.
    let commands = runnable(&ctx.settings.warm_commands, &mut warnings, || {
        Toolchain::clippy_available(&ctx.repo.main_worktree)
    })?;
    let hash = fingerprint(
        ctx,
        &commit,
        &host_toolchain,
        &commands,
        freshness,
        isolation,
        remap,
    )?;
    let id = BaseId::new(commit.as_str(), hash);
    let paths = ctx.store.base(&id);

    let lock = FileLock::exclusive(&paths.lock, &format!("base {}", id.short()))?;
    if let Some(base) = load_ready(&paths)? {
        set_current(ctx, &id)?;
        return Ok(BuildReport {
            base,
            reused: true,
            warnings,
        });
    }
    discard(ctx, &paths)?;

    let started = Instant::now();
    // Seeding from an earlier base with the same fingerprint turns a refresh
    // into an incremental build: its target is cloned and only files proven
    // identical to its checkout are stamped, exactly as for an agent.
    let inputs = unit_inputs(ctx, &commit)?;
    let seed = if cold {
        None
    } else {
        seed_for(ctx, &id, &inputs, ctx.settings.prune)?
    };
    let _seed_lock = match &seed {
        Some(seed) => Some(FileLock::shared(
            &ctx.store.base(&seed.id).lock,
            &format!("base {}", seed.id.short()),
        )?),
        None => None,
    };
    let toolchain = prepare(ctx, &paths, &commit, seed.as_ref(), isolation, remap)?;

    let remap_wrapper = if remap {
        Some(crate::remap::ensure_wrapper(&toolchain)?)
    } else {
        None
    };
    let warmed = warm(
        ctx,
        &paths,
        &build_plan(ctx, &paths, isolation, remap_wrapper),
        freshness,
        &toolchain,
        seed.as_ref(),
        &inputs,
        &commands,
    )
    .inspect_err(|_| {
        // The process may now be inside the sandbox, but host paths are
        // still reachable, so cleanup works the same way.
        let _ = discard(ctx, &paths);
    })?;
    warnings.extend(warmed.outcome.warnings);
    warnings.extend(warmed.warning);

    let dirtied = dirtied_files(&paths, &mut warnings)?;
    let base = BaseMeta {
        id: id.clone(),
        commit,
        created_unix: fsutil::now_unix(),
        build_secs: started.elapsed().as_secs_f64(),
        freshness,
        isolation,
        toolchain,
        commands: commands.into_iter().map(|c| c.line).collect(),
        ferry_version: env!("CARGO_PKG_VERSION").to_owned(),
        dirtied,
        remap_paths: remap,
        unit_inputs: inputs,
        seeded_from: seed.filter(|_| warmed.seeded).map(|s| s.id),
        observed: warmed.observed,
        pruned: warmed.pruned,
        command_secs: warmed.outcome.secs,
        incomplete: warmed.outcome.incomplete,
    };
    write_relocation_plan(ctx, &id, isolation, &paths)?;
    // Writing base.json is what makes the base ready (and immutable).
    fsutil::write_json_atomic(&paths.meta, &base)?;
    // Made current before the base lock is released, so gc (which skips
    // locked bases) cannot collect it in between. Lock order base -> store
    // is safe because gc only ever try-locks bases.
    set_current(ctx, &id)?;
    drop(lock);
    Ok(BuildReport {
        base,
        reused: false,
        warnings,
    })
}

/// Files the warm commands wrote into the checkout. Cargo writing (or
/// updating) the lockfile is expected; agents get the base's copy when the
/// repository does not track one.
fn dirtied_files(paths: &BasePaths, warnings: &mut Vec<String>) -> Result<Vec<String>> {
    let dirtied: Vec<String> = git::status_paths(&paths.src)?
        .into_iter()
        .filter(|p| p != "Cargo.lock")
        .collect();
    if !dirtied.is_empty() {
        warnings.push(format!(
            "the warm commands wrote {} file(s) into the source tree (e.g. {}); agents will not have them",
            dirtied.len(),
            dirtied[0]
        ));
    }
    Ok(dirtied)
}

/// Metadata of a complete base, or `None` if it does not exist or never
/// finished building.
pub fn load_ready(paths: &BasePaths) -> Result<Option<BaseMeta>> {
    if !paths.meta.exists() {
        return Ok(None);
    }
    fsutil::read_json(&paths.meta).map(Some)
}

pub fn load(ctx: &Context, id: &BaseId) -> Result<BaseMeta> {
    load_ready(&ctx.store.base(id))?.ok_or_else(|| Error::BaseMissing { id: id.to_string() })
}

pub fn current(ctx: &Context) -> Result<Option<BaseMeta>> {
    match ctx.store.current_base()? {
        Some(id) => load_ready(&ctx.store.base(&id)),
        None => Ok(None),
    }
}

/// Every ready base, newest first.
pub fn ready_bases(ctx: &Context) -> Result<Vec<BaseMeta>> {
    let mut bases = Vec::new();
    for id in ctx.store.base_ids()? {
        if let Some(meta) = load_ready(&ctx.store.base(&id))? {
            bases.push(meta);
        }
    }
    bases.sort_by(|a, b| (b.created_unix, &b.id).cmp(&(a.created_unix, &a.id)));
    Ok(bases)
}

pub fn list(ctx: &Context) -> Result<Vec<BaseStatus>> {
    let current = ctx.store.current_base()?;
    let agents = crate::agent::all_meta(ctx)?;
    let mut out = Vec::new();
    for id in ctx.store.base_ids()? {
        let paths = ctx.store.base(&id);
        let meta = load_ready(&paths)?;
        out.push(BaseStatus {
            ready: meta.is_some(),
            current: current.as_ref() == Some(&id),
            agents: agents
                .iter()
                .filter(|a| a.base == id)
                .map(|a| a.name.to_string())
                .collect(),
            disk_bytes: fsutil::disk_usage(&paths.dir)?,
            meta,
            id,
        });
    }
    out.sort_by(|a, b| {
        let created = |s: &BaseStatus| s.meta.as_ref().map_or(0, |m| m.created_unix);
        created(b).cmp(&created(a))
    });
    Ok(out)
}

/// Removes abandoned hook-created agents (see
/// [`crate::agent::reap_abandoned`]), then deletes bases that are neither
/// current nor referenced by any agent, including incomplete ones left by
/// interrupted builds.
pub fn gc(ctx: &Context, dry_run: bool) -> Result<GcReport> {
    let _store = ctx.store.lock()?;
    let (reaped, warnings) = crate::agent::reap_abandoned(ctx, dry_run)?;
    let current = ctx.store.current_base()?;
    let referenced: BTreeSet<BaseId> = crate::agent::all_meta(ctx)?
        .into_iter()
        .filter(|a| !reaped.contains(&a.name))
        .map(|a| a.base)
        .collect();
    let mut report = GcReport {
        dry_run,
        reaped_agents: reaped.iter().map(ToString::to_string).collect(),
        warnings,
        ..GcReport::default()
    };
    for id in ctx.store.base_ids()? {
        if current.as_ref() == Some(&id) || referenced.contains(&id) {
            report.kept.push(id);
            continue;
        }
        let paths = ctx.store.base(&id);
        let Some(_building) = FileLock::try_exclusive(&paths.lock)? else {
            report.busy.push(id);
            continue;
        };
        report.freed_bytes += fsutil::disk_usage(&paths.dir)?;
        if !dry_run {
            discard(ctx, &paths)?;
        }
        report.removed.push(id);
    }
    Ok(report)
}

/// Creates the base's checkout and target dir: worktree, stamps, and either
/// a clone of the seed's target or an empty dir. Returns the toolchain the
/// checkout selects.
fn prepare(
    ctx: &Context,
    paths: &BasePaths,
    commit: &CommitId,
    seed: Option<&BaseMeta>,
    isolation: Isolation,
    remap: bool,
) -> Result<Toolchain> {
    fs::create_dir_all(&paths.dir).at("create directory", &paths.dir)?;
    ctx.repo.add_detached_worktree(&paths.src, commit)?;
    let short = paths
        .dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    ctx.repo
        .lock_worktree(&paths.src, &format!("ferry base {short} (immutable)"))?;
    if remap {
        // Before stamping, so the link is part of the root directory agents
        // compare against.
        crate::remap::prepare(&paths.src, &paths.target)?;
    }
    let links_changed = if let Some(seed) = seed {
        let seed_paths = ctx.store.base(&seed.id);
        crate::agent::copy_untracked_lockfile(&seed_paths.src, &paths.src)?;
        let index = git::ls_files(&paths.src)?;
        let tree = ctx.repo.ls_tree(&seed.commit)?;
        let plan = freshness::plan(&tree, &seed_paths.src, &index, &paths.src)?;
        freshness::apply(&plan)?;
        plan.links_changed()
    } else {
        freshness::stamp_base_checkout(&paths.src, &git::ls_files(&paths.src)?)?;
        false
    };
    let toolchain = Toolchain::detect(&paths.src)?;
    match seed {
        Some(seed) => {
            clone_seed_target(ctx, seed, paths, isolation)?;
            if links_changed {
                let origin = relocation_origin(ctx, &seed.id, isolation, false);
                let stale = freshness::link_rebuilds(&paths.target, &origin, &paths.src)?;
                freshness::invalidate_dep_info(&paths.target, &stale)?;
            }
        }
        None => fs::create_dir_all(&paths.target).at("create directory", &paths.target)?,
    }
    // Best effort: failing to exclude build output from backups is no
    // reason to fail the build.
    let _ = crate::platform::exclude_from_backups(&paths.target);
    Ok(toolchain)
}

/// The newest ready base, other than `id`, built with the same fingerprint
/// (toolchain, warm commands, modes) and with its checkout and target
/// intact. It must have the same unit inputs, unless `cross_inputs` allows
/// any base whose warm build was observed: the refresh then prunes the
/// target to the units it uses.
fn seed_for(
    ctx: &Context,
    id: &BaseId,
    inputs: &str,
    cross_inputs: bool,
) -> Result<Option<BaseMeta>> {
    Ok(ready_bases(ctx)?.into_iter().find(|b| {
        let paths = ctx.store.base(&b.id);
        &b.id != id
            && b.id.fingerprint() == id.fingerprint()
            && (b.unit_inputs == inputs || (cross_inputs && b.observed))
            && paths.src.is_dir()
            && paths.target.is_dir()
    }))
}

/// Clones the seed's target into the new base, by reflink when the store
/// supports it.
fn clone_seed_target(
    ctx: &Context,
    seed: &BaseMeta,
    paths: &BasePaths,
    isolation: Isolation,
) -> Result<()> {
    let seed_paths = ctx.store.base(&seed.id);
    let kind = if crate::platform::probe_reflink(ctx.store.root()).is_ok() {
        BackendKind::Reflink
    } else {
        BackendKind::Copy
    };
    crate::fork::backend(kind)?.fork(&ForkPaths {
        base_target: seed_paths.target.clone(),
        agent_target: paths.target.clone(),
        upper: paths.dir.join("unused-upper"),
        work: paths.dir.join("unused-work"),
    })?;
    // The seed's units may have captured the seed's paths, and the seed may
    // be collected.
    if isolation == Isolation::None {
        let origin = relocation_origin(ctx, &seed.id, isolation, false);
        let plan = freshness::plan_relocation(&paths.target, &origin)?;
        let report = freshness::apply_relocation(&paths.target, &plan)?;
        if report.units + report.reruns > 0 {
            fsutil::note(&format!(
                "{} build units of seed {} captured its paths and rebuild (e.g. {})",
                report.units + report.reruns,
                seed.id.short(),
                report.examples.join(", ")
            ));
        }
    }
    Ok(())
}

/// Where base `id`'s target was built. `live` if forks are planned for while
/// the base is kept, as gc keeps every base an agent forked.
pub(crate) fn relocation_origin(
    ctx: &Context,
    id: &BaseId,
    isolation: Isolation,
    live: bool,
) -> freshness::Origin {
    let paths = ctx.store.base(id);
    let checkout = match isolation {
        Isolation::Namespace => ctx.store.canonical_src(),
        Isolation::None => paths.src.clone(),
    };
    // A worktree's `.git` file names its own git dir; without it, every
    // worktree's counts.
    let dot_git = paths.src.join(".git");
    let git_dir = fs::read_to_string(&dot_git)
        .ok()
        .and_then(|s| {
            s.lines()
                .next()?
                .strip_prefix("gitdir:")
                .map(|d| paths.src.join(d.trim()))
        })
        .unwrap_or_else(|| ctx.repo.common_dir.join("worktrees"));
    freshness::Origin {
        store: ctx.store.root().to_path_buf(),
        checkout,
        host_checkout: paths.src,
        git_dir,
        live: live.then(|| id.to_string()),
    }
}

/// Stores the plan for forks of a base built without the namespace
/// sandbox, so that spawning only applies it.
fn write_relocation_plan(
    ctx: &Context,
    id: &BaseId,
    isolation: Isolation,
    paths: &BasePaths,
) -> Result<()> {
    if isolation != Isolation::None {
        return Ok(());
    }
    let origin = relocation_origin(ctx, id, isolation, true);
    let plan = freshness::plan_relocation(&paths.target, &origin)?;
    if plan.cacheable() {
        fsutil::write_json_atomic(&paths.relocation, &plan)?;
    }
    Ok(())
}

/// The relocation plan for a fork of `base` that builds without the
/// namespace sandbox: the one stored with the base if it fits these rules,
/// else computed on the base's target, which holds the fork's bytes.
pub(crate) fn relocation_plan(ctx: &Context, base: &BaseMeta) -> Result<freshness::RelocationPlan> {
    let paths = ctx.store.base(&base.id);
    let live = base.isolation == Isolation::None;
    if live
        && let Ok(bytes) = fs::read(&paths.relocation)
        && let Ok(plan) = serde_json::from_slice::<freshness::RelocationPlan>(&bytes)
        && plan.fits(base.id.as_str())
    {
        return Ok(plan);
    }
    let origin = relocation_origin(ctx, &base.id, base.isolation, live);
    freshness::plan_relocation(&paths.target, &origin)
}

fn set_current(ctx: &Context, id: &BaseId) -> Result<()> {
    let _store = ctx.store.lock()?;
    ctx.store.set_current_base(id)
}

/// Removes a base's worktree registration and files. Callers must hold the
/// base's lock and know that nothing references it.
fn discard(ctx: &Context, paths: &BasePaths) -> Result<()> {
    // Un-ready it first: an interrupted delete must never leave a base that
    // looks complete over a half-deleted checkout or target.
    match fs::remove_file(&paths.meta) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).at("remove", &paths.meta);
        }
        _ => {}
    }
    ctx.repo.forget_worktree(&paths.src)?;
    fsutil::remove_dir_all_force(&paths.dir)
}

fn resolve_isolation(ctx: &Context, warnings: &mut Vec<String>) -> Result<Isolation> {
    if ctx.settings.sandbox == SandboxMode::Off {
        return Ok(Isolation::None);
    }
    let caps = Capabilities::detect(ctx.store.root());
    if caps.userns.is_ok() {
        return Ok(Isolation::Namespace);
    }
    let detail = match &caps.userns {
        crate::platform::Probe::Ok => String::new(),
        crate::platform::Probe::Unsupported { reason } => reason.clone(),
        crate::platform::Probe::Failed { stage, error } => format!("{stage}: {error}"),
    };
    if caps.userns.is_inconclusive() {
        return Err(Error::Refused(format!(
            "could not tell whether the namespace sandbox works ({detail}), and building \
             without it would silently cost every agent its incremental reuse; retry, or \
             set FERRY_SANDBOX=off to build without the sandbox on purpose"
        )));
    }
    if ctx.settings.sandbox == SandboxMode::Namespace {
        return Err(Error::Sandbox(sandbox::SandboxError::Unsupported(detail)));
    }
    if caps.os == crate::platform::Os::Linux {
        warnings.push(format!(
            "namespace sandbox unavailable ({detail}); building without canonical paths, so an agent's \
             first edit rebuilds without incremental reuse. Run `ferry doctor` for fixes."
        ));
    }
    Ok(Isolation::None)
}

fn resolve_remap(
    ctx: &Context,
    isolation: Isolation,
    toolchain: &Toolchain,
    warnings: &mut Vec<String>,
) -> bool {
    isolation == Isolation::None && ctx.settings.remap_paths && {
        match crate::remap::supported(toolchain) {
            Ok(()) => true,
            Err(why) => {
                warnings.push(format!("remap-paths disabled: {why}"));
                false
            }
        }
    }
}

/// Hash of everything besides the commit that determines a base's artifacts.
fn fingerprint(
    ctx: &Context,
    commit: &CommitId,
    toolchain: &Toolchain,
    commands: &[WarmCommand],
    freshness: FreshnessMode,
    isolation: Isolation,
    remap: bool,
) -> Result<u64> {
    let mut input = Vec::new();
    input.extend_from_slice(toolchain.rustc_verbose.as_bytes());
    input.push(0);
    input.extend_from_slice(toolchain.cargo_version.as_bytes());
    input.push(0);
    // The host toolchain only approximates the one the commit selects, so the
    // commit's own toolchain file is hashed too.
    for file in ["rust-toolchain.toml", "rust-toolchain"] {
        if let Some(bytes) = ctx.repo.show_file(commit, file)? {
            input.extend_from_slice(&bytes);
        }
        input.push(0);
    }
    for cmd in commands {
        input.extend_from_slice(cmd.line.as_bytes());
        input.push(0);
    }
    input.extend_from_slice(format!("{freshness:?}/{isolation:?}/remap={remap}").as_bytes());
    Ok(fnv1a64(&input))
}

/// Hash of what decides which units (feature sets, dependency versions,
/// profiles, flags) a build of `commit` selects: the commit's manifests,
/// lockfiles, Cargo configs and toolchain files, the Cargo configs above the
/// store, Cargo's flag variables, `CARGO_INCREMENTAL`, and whether `CI` or
/// `TF_BUILD` is set. A seed's target still holds units left
/// from the seeds before it, built from older sources than the ones stamping
/// compares against; a build with the same inputs selects none of them.
fn unit_inputs(ctx: &Context, commit: &CommitId) -> Result<String> {
    use crate::sys::OsStrExt;
    // Bumped whenever what counts as an input changes, so that no base
    // recorded under the old definition matches. 2 added CARGO_INCREMENTAL
    // and CI: without it, a base warmed with them by an older Ferry would
    // match a refresh warmed without them and keep its units unpruned.
    let mut input = b"unit-inputs-2\0".to_vec();
    for entry in ctx.repo.ls_tree(commit)? {
        let name = entry.path.file_name().and_then(|n| n.to_str());
        let in_dot_cargo = entry
            .path
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|p| p == ".cargo");
        let selects_units = match name {
            Some("Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml") => true,
            Some("config" | "config.toml") => in_dot_cargo,
            _ => false,
        };
        if !selects_units {
            continue;
        }
        if !entry.is_regular_file() {
            // A link's target is not part of the hash, so no seed can be
            // proven to match.
            return Ok(format!("unseedable-{commit}"));
        }
        input.extend_from_slice(entry.path.as_os_str().as_bytes());
        input.push(0);
        input.extend_from_slice(entry.oid.as_bytes());
        input.push(0);
    }

    let config_dirs = ctx
        .store
        .root()
        .ancestors()
        .map(|dir| dir.join(".cargo"))
        .chain(cargo_home());
    for dir in config_dirs {
        for file in ["config", "config.toml"] {
            let path = dir.join(file);
            match fs::read(&path) {
                Ok(bytes) => input.extend_from_slice(&bytes),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).at("read", &path),
            }
            input.push(0);
        }
    }
    let mut vars: Vec<_> = std::env::vars_os()
        .filter(|(name, _)| name.to_str().is_some_and(selects_units))
        .collect();
    vars.sort();
    for (name, value) in vars {
        input.extend_from_slice(name.as_bytes());
        input.push(b'=');
        input.extend_from_slice(value.as_bytes());
        input.push(0);
    }
    // Either one with a UTF-8 value (Cargo tests `std::env::var(name).is_ok()`)
    // makes Cargo 1.99 and later turn incremental compilation off by default,
    // which changes every local unit's hash. Hashed on every Cargo version:
    // the toolchain is known only after this runs.
    if crate::shim::CI_VARS
        .iter()
        .any(|name| std::env::var(name).is_ok())
    {
        input.extend_from_slice(b"CI\0");
    }
    Ok(format!("{:016x}", fnv1a64(&input)))
}

/// Cargo configs that apply to builds in the main worktree but not to
/// Ferry's: those in directories above it, and an untracked one inside it.
/// Ferry's builds run under the store, so Cargo never finds them there.
pub fn unapplied_cargo_configs(ctx: &Context) -> Result<Vec<std::path::PathBuf>> {
    let cargo_home = cargo_home().and_then(|home| crate::sys::canonicalize(home).ok());
    let head = ctx.repo.resolve_commit("HEAD").ok();
    let mut unapplied = Vec::new();
    let root = &ctx.repo.main_worktree;
    for dir in root.ancestors() {
        let dot_cargo = dir.join(".cargo");
        if cargo_home.is_some() && crate::sys::canonicalize(&dot_cargo).ok() == cargo_home {
            continue;
        }
        for file in ["config", "config.toml"] {
            let path = dot_cargo.join(file);
            if !path.is_file() {
                continue;
            }
            let tracked = match &head {
                Some(head) if dir == root => ctx
                    .repo
                    .show_file(head, &format!(".cargo/{file}"))?
                    .is_some(),
                _ => false,
            };
            if !tracked {
                unapplied.push(path);
            }
        }
    }
    Ok(unapplied)
}

pub(crate) fn cargo_home() -> Option<std::path::PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".cargo")))
}

pub const CONFIG_ADVICE: &str = "move its settings into the repository's tracked .cargo/config.toml or into $CARGO_HOME/config.toml";

pub fn unapplied_message(ctx: &Context, config: &std::path::Path) -> String {
    format!(
        "{} applies to cargo in {} but not to Ferry's builds, which run under {}",
        config.display(),
        ctx.repo.main_worktree.display(),
        ctx.store.root().display()
    )
}

/// Environment variables that change unit hashes or compiler flags, minus
/// the target and build dir, which Ferry sets itself, and `build.warnings`,
/// which Cargo applies after a unit is built and fingerprints nowhere.
fn selects_units(name: &str) -> bool {
    let cargo_config = [
        "CARGO_BUILD_",
        "CARGO_PROFILE_",
        "CARGO_TARGET_",
        "CARGO_UNSTABLE_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix));
    matches!(
        name,
        "RUSTFLAGS"
            | "RUSTDOCFLAGS"
            | "CARGO_ENCODED_RUSTFLAGS"
            | "CARGO_ENCODED_RUSTDOCFLAGS"
            | "CARGO_INCREMENTAL"
    ) || (cargo_config
        && !matches!(
            name,
            "CARGO_TARGET_DIR"
                | "CARGO_BUILD_TARGET_DIR"
                | "CARGO_BUILD_BUILD_DIR"
                | "CARGO_BUILD_WARNINGS"
        ))
}

/// The warm commands that can run: clippy commands are dropped, with a
/// warning, when `clippy_installed` (asked only if there is one) is false.
fn runnable(
    commands: &[WarmCommand],
    warnings: &mut Vec<String>,
    clippy_installed: impl FnOnce() -> bool,
) -> Result<Vec<WarmCommand>> {
    if !commands.iter().any(|c| c.clippy) || clippy_installed() {
        return Ok(commands.to_vec());
    }
    let kept: Vec<WarmCommand> = commands.iter().filter(|c| !c.clippy).cloned().collect();
    if kept.is_empty() {
        return Err(Error::ClippyMissing);
    }
    warnings.push(
        "clippy is not installed for this toolchain; the base does not warm `cargo clippy` \
         (rustup component add clippy)"
            .into(),
    );
    Ok(kept)
}

fn build_plan(
    ctx: &Context,
    paths: &BasePaths,
    isolation: Isolation,
    remap_wrapper: Option<crate::remap::Wrapper>,
) -> Plan {
    Plan {
        isolation,
        src: paths.src.clone(),
        target: TargetSpec::Dir(paths.target.clone()),
        canonical_src: ctx.store.canonical_src(),
        canonical_target: ctx.store.canonical_target(),
        ns_record: None,
        remap_wrapper,
    }
}

/// What [`warm`] found out.
struct Warmed {
    observed: bool,
    pruned: Option<Pruned>,
    /// False if the seed's target was replaced by a build from scratch.
    seeded: bool,
    warning: Option<String>,
    /// The run whose target is published.
    outcome: WarmOutcome,
}

/// Enters the sandbox and runs `commands` into the prepared target. A
/// target seeded across unit inputs is then pruned to the units the
/// commands used, or, when that cannot be told, emptied and built again.
#[allow(clippy::too_many_arguments)]
fn warm(
    ctx: &Context,
    paths: &BasePaths,
    plan: &Plan,
    freshness: FreshnessMode,
    toolchain: &Toolchain,
    seed: Option<&BaseMeta>,
    inputs: &str,
    commands: &[WarmCommand],
) -> Result<Warmed> {
    let cross_inputs = seed.is_some_and(|s| s.unit_inputs != inputs);
    if cross_inputs {
        // Nested workspaces' target dirs are not observed, so only what this
        // build puts there may stay.
        fsutil::remove_dir_all_force(&paths.target.join(crate::shim::NESTED_TARGETS))?;
    }
    let watch = if ctx.settings.prune {
        let seed_meta = seed.map(|s| ctx.store.base(&s.id).meta);
        Some(prune::Watch::start(
            &paths.dir.join(WARM_UNITS),
            seed_meta.as_deref(),
        )?)
    } else {
        None
    };
    let shim_dir = crate::shim::ensure()?;
    sandbox::enter(plan)?;
    let env = warm_env(
        plan,
        freshness,
        toolchain,
        &shim_dir,
        watch.as_ref().map(prune::Watch::dir),
    );
    let outcome = run_warm_commands(plan, &env, commands)?;
    let mut warmed = Warmed {
        observed: false,
        pruned: None,
        seeded: seed.is_some(),
        warning: None,
        outcome,
    };
    let Some(watch) = watch else {
        return Ok(warmed);
    };
    match watch.used(&paths.target)? {
        Observation::Used(used) => {
            warmed.observed = true;
            if cross_inputs {
                warmed.pruned = Some(prune::sweep(&used)?);
            }
        }
        Observation::Incomplete(why) if cross_inputs => {
            // Publishing the target as it is could hand agents units that
            // this build never checked against its checkout.
            warmed.warning = Some(format!(
                "rebuilt the base from scratch: could not tell which of the seed's units the \
                 warm commands used ({why})"
            ));
            fsutil::clear_dir(&paths.target)?;
            watch.clear()?;
            warmed.outcome = run_warm_commands(plan, &env, commands)?;
            warmed.observed = matches!(watch.used(&paths.target)?, Observation::Used(_));
            warmed.seeded = false;
        }
        Observation::Incomplete(_) => {}
    }
    watch.remove()?;
    Ok(warmed)
}

/// Where the shim records a warm build's cargo calls while it runs.
const WARM_UNITS: &str = "warm-units";

/// The warm commands' environment. The target dir variables stay set for
/// cargo calls that bypass the shim; through it, each workspace gets the
/// target dir it gets in an agent, and the same `CARGO_INCREMENTAL` (see
/// `shim::keep_incremental`).
fn warm_env(
    plan: &Plan,
    freshness: FreshnessMode,
    toolchain: &Toolchain,
    shim_dir: &std::path::Path,
    records: Option<&std::path::Path>,
) -> Vec<(OsString, OsString)> {
    let mut env = crate::agent::cargo_env(plan, freshness);
    env.extend([
        // A user's `build.warnings = "deny"` must not fail the base; it is
        // in no fingerprint, so the units still match the agents' builds.
        ("CARGO_BUILD_WARNINGS".into(), "warn".into()),
        ("PATH".into(), crate::shim::path_with(shim_dir)),
        (
            crate::shim::WARM_SRC_ENV.into(),
            plan.build_src().as_os_str().to_owned(),
        ),
        (
            crate::shim::WARM_TARGET_ENV.into(),
            plan.build_target().into_os_string(),
        ),
        (
            crate::shim::WARM_CARGO_ENV.into(),
            toolchain.cargo_version.clone().into(),
        ),
    ]);
    if let Some(records) = records {
        env.push((
            crate::shim::WARM_UNITS_ENV.into(),
            records.as_os_str().to_owned(),
        ));
    }
    env
}

struct WarmOutcome {
    secs: Vec<f64>,
    incomplete: Vec<String>,
    warnings: Vec<String>,
}

/// The command that runs the warm command `line`, and its program's name.
/// `sh -c` on Unix. Windows has no `sh` of its own: a line made only of
/// words and quotes, as the generated ones are, runs directly; any other
/// line runs with Git for Windows' `sh` when it is on `PATH`, else with
/// `cmd /C`.
fn shell_command(line: &str) -> (Command, String) {
    #[cfg(windows)]
    if let Some(argv) = crate::config::split_words(line)
        && let Some((program, args)) = argv.split_first()
    {
        let mut command = Command::new(program);
        command.args(args);
        return (command, program.clone());
    }
    let sh = if cfg!(windows) {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path).any(|d| d.join("sh.exe").is_file())
    } else {
        true
    };
    if sh {
        let mut command = Command::new("sh");
        command.arg("-c").arg(line);
        (command, "sh".into())
    } else {
        let mut command = Command::new("cmd");
        command.arg("/C");
        // cmd.exe does its own parsing of the rest of its command line.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::raw_arg(&mut command, line);
        #[cfg(unix)]
        command.arg(line);
        (command, "cmd".into())
    }
}

fn run_warm_commands(
    plan: &Plan,
    env: &[(OsString, OsString)],
    commands: &[WarmCommand],
) -> Result<WarmOutcome> {
    let mut outcome = WarmOutcome {
        secs: Vec::new(),
        incomplete: Vec::new(),
        warnings: Vec::new(),
    };
    for WarmCommand { line, clippy } in commands {
        fsutil::note(&format!("running `{line}`"));
        // Build output goes to stderr so `--json` output on stdout stays
        // parseable.
        #[cfg(unix)]
        let stderr = std::io::stderr().as_fd().try_clone_to_owned();
        #[cfg(windows)]
        let stderr = std::io::stderr().as_handle().try_clone_to_owned();
        let stderr = stderr.at("duplicate", "stderr")?;
        let started = Instant::now();
        let (mut command, program) = shell_command(line);
        let status = command
            .current_dir(plan.build_src())
            .env("PWD", plan.build_src())
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::from(stderr))
            .status()
            .map_err(|source| sandbox::SandboxError::Exec { program, source })?;
        outcome.secs.push(started.elapsed().as_secs_f64());
        if status.success() {
            continue;
        }
        if !clippy {
            return Err(Error::BaseBuildFailed {
                command: line.clone(),
                status: status.to_string(),
            });
        }
        outcome.warnings.push(format!(
            "`{line}` failed ({status}); the base is kept, but crates clippy could not check \
             are not warm for `cargo clippy`"
        ));
        outcome.incomplete.push(line.clone());
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_variables_select_units_but_ferrys_own_do_not() {
        for name in [
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_PROFILE_DEV_DEBUG",
            "CARGO_BUILD_TARGET",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
            "CARGO_UNSTABLE_CHECKSUM_FRESHNESS",
            "CARGO_INCREMENTAL",
        ] {
            assert!(selects_units(name), "{name}");
        }
        for name in [
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_BUILD_DIR",
            "CARGO_BUILD_WARNINGS",
            "CARGO_HOME",
            "CARGO_TERM_COLOR",
            "RUSTUP_HOME",
        ] {
            assert!(!selects_units(name), "{name}");
        }
    }

    #[test]
    fn runnable_drops_clippy_only_when_missing() {
        let cmd = |line: &str, clippy| WarmCommand {
            line: line.into(),
            clippy,
        };
        let generated = [cmd("cargo build", false), cmd("cargo clippy", true)];

        let mut warnings = Vec::new();
        assert_eq!(
            runnable(&generated, &mut warnings, || true).unwrap(),
            generated
        );
        assert_eq!(warnings, Vec::<String>::new());

        let kept = runnable(&generated, &mut warnings, || false).unwrap();
        assert_eq!(kept, [cmd("cargo build", false)]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("rustup component add clippy"));

        let explicit = [cmd("cargo clippy", false)];
        let kept = runnable(&explicit, &mut warnings, || panic!("probed without clippy")).unwrap();
        assert_eq!(kept, explicit);
        assert_eq!(warnings.len(), 1);
    }

    /// Config validation rejects an empty command list; dropping clippy must
    /// not produce one either.
    #[test]
    fn runnable_refuses_to_drop_every_command() {
        let only_clippy = [WarmCommand {
            line: "cargo clippy".into(),
            clippy: true,
        }];
        let err = runnable(&only_clippy, &mut Vec::new(), || false).unwrap_err();
        assert!(matches!(err, Error::ClippyMissing), "{err}");
        assert!(
            err.hint()
                .is_some_and(|h| h.contains("rustup component add clippy"))
        );
    }
}
