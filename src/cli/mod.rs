//! Command-line interface. Errors are `anyhow` here and typed below.

mod output;
mod setup;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use anstream::{print, println};
use anyhow::Context as _;
use clap::{Parser, Subcommand};

use crate::agent::{self, AgentMeta, SpawnOptions};
use crate::base;
use crate::config;
use crate::context::Context;
use crate::doctor;
use crate::fsutil::human_bytes;
use crate::store::AgentName;

use output::{BOLD, DIM, GREEN, YELLOW};

#[derive(Debug, Parser)]
#[command(
    name = "ferry",
    version,
    about = "Parallel agents, one warm build: forked, isolated Cargo builds per git worktree",
    long_about = "Ferry gives every parallel AI coding agent its own git worktree with an \
                  instantly forked, already-warm Cargo target dir. Agents never wait on a cold \
                  compile and never share (and corrupt) each other's build state.\n\n\
                  Typical flow:\n  ferry base\n  ferry spawn agent-1\n  ferry run agent-1 -- cargo test",
    propagate_version = true
)]
pub struct Cli {
    /// Run as if ferry was started in DIR.
    #[arg(short = 'C', global = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// Print machine-readable JSON on stdout.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Write ferry.toml and report what this machine supports.
    Init {
        /// Overwrite an existing ferry.toml.
        #[arg(long)]
        force: bool,
    },

    /// Build an immutable warm base that new agents fork from.
    Base {
        /// Commit to build (any git revision).
        #[arg(long, default_value = "HEAD")]
        rev: String,
        /// Build from scratch instead of seeding from the newest compatible base.
        #[arg(long, conflicts_with = "watch")]
        cold: bool,
        /// Keep running, and build a base each time REV moves to a new
        /// commit (seen on two polls in a row).
        #[arg(long)]
        watch: bool,
        /// With --watch: seconds between polls of REV.
        #[arg(long, value_name = "SECS", requires = "watch", value_parser = parse_interval)]
        interval: Option<std::time::Duration>,
        /// With --watch: run `ferry gc` after each new base.
        #[arg(long, requires = "watch")]
        gc: bool,
    },

    /// Create an agent: a git worktree plus a forked warm target dir.
    Spawn {
        /// Agent name (letters, digits, '-', '_', '.').
        name: String,
        /// Revision the agent's branch starts at.
        #[arg(long, value_name = "REF", default_value = "HEAD")]
        from: String,
        /// Where to create the worktree (default: the worktree directory, see
        /// `[agent] worktree-dir`).
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,
        /// Branch name (default: the configured branch prefix plus NAME).
        #[arg(long)]
        branch: Option<String>,
    },

    /// Run a command in an agent's build context.
    ///
    /// Without NAME, uses the agent whose worktree contains the current
    /// directory.
    Run {
        /// Agent name (default: the agent whose worktree contains the current
        /// directory).
        name: Option<String>,
        /// The command and its arguments, after `--`.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },

    /// Start an interactive shell in an agent's build context.
    Shell {
        /// Agent name (default: the agent whose worktree contains the current
        /// directory).
        name: Option<String>,
    },

    /// Print an agent's paths and environment for wiring into agent tools.
    Env {
        /// Agent name (default: the agent whose worktree contains the current
        /// directory).
        name: Option<String>,
        /// Shell syntax to print: `sh` (`eval "$(ferry env a1)"`, also Git
        /// Bash) or `powershell` (`ferry env a1 | Invoke-Expression`).
        /// Default: `sh`, except on Windows outside Git Bash.
        #[arg(long, value_enum)]
        shell: Option<EnvShell>,
    },

    /// List bases and agents with their status and disk usage.
    List,

    /// Remove an agent's worktree and build state.
    ///
    /// Refuses if the worktree has uncommitted changes or processes started
    /// for the agent are still running. The branch is kept if it has new
    /// commits.
    Rm {
        /// Agent to remove.
        name: String,
        /// Discard uncommitted changes and ignore running processes.
        #[arg(long)]
        force: bool,
    },

    /// Delete bases that no agent uses and that are not current.
    Gc {
        /// Show what would be deleted.
        #[arg(long)]
        dry_run: bool,
    },

    /// Check platform capabilities and print fixes for what is missing.
    Doctor {
        /// Print an `AppArmor` profile that allows this ferry binary to use
        /// user namespaces (Ubuntu 24.04+).
        #[arg(long)]
        apparmor_profile: bool,
    },

    /// Print the directory holding Ferry's cargo shim.
    ///
    /// Put it first on PATH (`export PATH="$(ferry shim):$PATH"`): every cargo
    /// call made inside an agent worktree then runs in that agent's sandbox
    /// with its own target dir, and cargo elsewhere runs unchanged.
    Shim,

    /// Hook handlers for agent tools (reads the tool's JSON on stdin).
    Hook {
        #[command(subcommand)]
        event: HookEvent,
    },

    /// Change the system so Ferry's namespace sandbox works (asks first).
    Setup {
        #[command(subcommand)]
        what: SetupCommand,
    },

    #[command(name = "__probe-sandbox", hide = true)]
    ProbeSandbox { dir: PathBuf },

    /// Prints this binary's capabilities as JSON, probing under DIR.
    #[command(name = "__detect", hide = true)]
    Detect { dir: PathBuf },
}

#[derive(Debug, Subcommand)]
pub enum SetupCommand {
    /// Let ferry create user namespaces on Ubuntu 24.04+: install it to a
    /// root-owned path and load its `AppArmor` profile, with one sudo call.
    ///
    /// Prints every step, the profile and the root script first, and asks
    /// before running sudo.
    Apparmor {
        /// Run without asking; the plan is still printed.
        #[arg(short = 'y', long, conflicts_with = "dry_run")]
        yes: bool,
        /// Print the plan and exit without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Unload and remove Ferry's profile instead.
        #[arg(long)]
        uninstall: bool,
        /// Where to install ferry when its current path cannot be profiled.
        #[arg(long, value_name = "DIR", default_value = crate::setup::DEFAULT_BIN_DIR)]
        bin_dir: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum HookEvent {
    /// Claude Code `WorktreeCreate`: spawn an agent, print its worktree path.
    ClaudeWorktreeCreate,
    /// Claude Code `WorktreeRemove`: remove the agent unless it has
    /// uncommitted work.
    ClaudeWorktreeRemove,
    /// Claude Code `SessionStart`: put the cargo shim on PATH via
    /// `$CLAUDE_ENV_FILE`.
    ClaudeSessionStart,
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            let hint = err
                .downcast_ref::<crate::Error>()
                .and_then(crate::Error::hint);
            // Most messages already end with their source's text.
            let message = err.to_string();
            let mut shown = message.clone();
            let causes: Vec<String> = err
                .chain()
                .skip(1)
                .map(ToString::to_string)
                .filter(|cause| {
                    let new = !shown.contains(cause.as_str());
                    shown.push_str(cause);
                    new
                })
                .collect();
            output::error(&message, &causes, hint.as_deref());
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let cwd = match &cli.dir {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    let json = cli.json;
    let discover = || Context::discover(&cwd);
    match cli.command {
        Command::Init { force } => init(&discover()?, force, json),
        Command::Base {
            rev,
            watch: true,
            interval,
            gc,
            ..
        } => watch_cmd(
            &discover()?,
            &crate::watch::WatchOptions {
                rev,
                interval: interval.unwrap_or(std::time::Duration::from_secs(2)),
                gc,
            },
            json,
        ),
        Command::Base { rev, cold, .. } => base_cmd(&discover()?, &rev, cold, json),
        Command::Spawn {
            name,
            from,
            path,
            branch,
        } => spawn(
            &discover()?,
            &name,
            SpawnOptions {
                from: Some(from),
                path,
                branch,
                origin: agent::Origin::Cli,
                cwd: Some(cwd.clone()),
            },
            json,
        ),
        Command::Run { name, command } => {
            let ctx = discover()?;
            let meta = resolve_agent(&ctx, name.as_deref(), &cwd)?;
            match agent::exec_session(&ctx, &meta, &command, &cwd)? {}
        }
        Command::Shell { name } => {
            let ctx = discover()?;
            let meta = resolve_agent(&ctx, name.as_deref(), &cwd)?;
            let shell = std::env::var_os("SHELL").unwrap_or_else(|| {
                if cfg!(windows) {
                    "powershell".into()
                } else {
                    "/bin/sh".into()
                }
            });
            match agent::exec_session(&ctx, &meta, &[shell], &cwd)? {}
        }
        Command::Env { name, shell } => {
            let ctx = discover()?;
            let meta = resolve_agent(&ctx, name.as_deref(), &cwd)?;
            env(&ctx, &meta, shell.unwrap_or_else(EnvShell::detect), json)
        }
        Command::List => list(&discover()?, json),
        Command::Rm { name, force } => rm(&discover()?, &name, force, json),
        Command::Gc { dry_run } => gc(&discover()?, dry_run, json),
        Command::Doctor { apparmor_profile } => doctor_cmd(&cwd, apparmor_profile, json),
        Command::Shim => {
            let dir = crate::shim::ensure()?;
            if json {
                output::json(&serde_json::json!({ "shim_dir": dir }))?;
            } else {
                println!("{}", dir.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Hook { event } => hook(&event),
        Command::Setup { what } => setup::run(&what, json),
        Command::ProbeSandbox { dir } => probe(&dir),
        Command::Detect { dir } => {
            output::json(&crate::platform::Capabilities::detect(&dir))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn resolve_agent(
    ctx: &Context,
    name: Option<&str>,
    cwd: &std::path::Path,
) -> anyhow::Result<AgentMeta> {
    Ok(match name {
        Some(name) => agent::load(ctx, &name.parse::<AgentName>()?)?,
        None => agent::find_by_cwd(ctx, cwd)?,
    })
}

fn init(ctx: &Context, force: bool, json: bool) -> anyhow::Result<ExitCode> {
    let path = ctx.repo.main_worktree.join(config::FILE_NAME);
    let existed = path.exists();
    if existed && !force {
        if !json {
            output::warning(&format!(
                "{} already exists (pass --force to overwrite)",
                path.display()
            ));
        }
    } else {
        std::fs::write(&path, config::TEMPLATE)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    let report = doctor::run(Ok(ctx));
    if json {
        output::json(&serde_json::json!({
            "config": path,
            "written": !existed || force,
            "doctor": report,
        }))?;
    } else {
        if !existed || force {
            output::success(&format!("wrote {}", path.display()));
        }
        print_plan(&report);
        println!("\nNext: `ferry base`, then `ferry spawn <name>`.");
    }
    Ok(ExitCode::SUCCESS)
}

fn base_cmd(ctx: &Context, rev: &str, cold: bool, json: bool) -> anyhow::Result<ExitCode> {
    let report = base::build(ctx, rev, cold)?;
    if json {
        output::json(&report)?;
        return Ok(ExitCode::SUCCESS);
    }
    print_build(&report)?;
    Ok(ExitCode::SUCCESS)
}

fn watch_cmd(
    ctx: &Context,
    opts: &crate::watch::WatchOptions,
    json: bool,
) -> anyhow::Result<ExitCode> {
    use crate::watch::Event;
    let short = |c: &crate::git::CommitId| c.as_str().chars().take(12).collect::<String>();
    if !json {
        crate::fsutil::note(&format!(
            "watching {} every {:.1}s; Ctrl-C stops",
            opts.rev,
            opts.interval.as_secs_f64()
        ));
    }
    let mut print = |event: &Event| {
        if json {
            // One event per line.
            if let Ok(line) = serde_json::to_string(event) {
                println!("{line}");
            }
            return;
        }
        match event {
            Event::Building { commit } => crate::fsutil::note(&format!(
                "{} moved to {}: building",
                opts.rev,
                short(commit)
            )),
            Event::Built { report, .. } => {
                let _ = print_build(report);
            }
            Event::Failed { commit, error } => {
                let keeping = base::current(ctx)
                    .ok()
                    .flatten()
                    .map(|b| format!("; keeping base {}", b.id.short()))
                    .unwrap_or_default();
                output::warning(&format!(
                    "build for {} failed: {error}{keeping}",
                    short(commit)
                ));
            }
            Event::Collected { report } if !report.removed.is_empty() => {
                output::success(&format!(
                    "gc removed {} base{}, {} freed",
                    report.removed.len(),
                    if report.removed.len() == 1 { "" } else { "s" },
                    human_bytes(report.freed_bytes)
                ));
            }
            Event::Collected { .. } => {}
            Event::GcFailed { error } => output::warning(&format!("gc failed: {error}")),
        }
    };
    match crate::watch::watch(ctx, opts, &mut print)? {}
}

fn parse_interval(value: &str) -> Result<std::time::Duration, String> {
    value
        .parse::<f64>()
        .ok()
        .and_then(|secs| std::time::Duration::try_from_secs_f64(secs).ok())
        .filter(|interval| !interval.is_zero())
        .ok_or_else(|| format!("`{value}` is not a positive number of seconds, at most 1e19"))
}

/// The human summary of a `ferry base`.
fn print_build(report: &base::BuildReport) -> anyhow::Result<()> {
    for w in &report.warnings {
        output::warning(w);
    }
    let b = &report.base;
    if report.reused {
        output::success(&format!(
            "base {} is ready (already built) and current",
            b.id.short()
        ));
    } else {
        let seeded = b
            .seeded_from
            .as_ref()
            .map(|s| format!(", seeded from {}", s.short()))
            .unwrap_or_default();
        let pruned = b
            .pruned
            .map(|p| {
                let s = if p.units == 1 { "" } else { "s" };
                format!(", pruned {} unit{s} ({})", p.units, human_bytes(p.bytes))
            })
            .unwrap_or_default();
        output::success(&format!(
            "base {} ready in {:.1}s ({}, {} freshness{seeded}{pruned})",
            b.id.short(),
            b.build_secs,
            match b.isolation {
                crate::sandbox::Isolation::Namespace => "namespace sandbox",
                crate::sandbox::Isolation::None => "no sandbox",
            },
            serde_json::to_value(b.freshness)?
                .as_str()
                .unwrap_or_default()
        ));
    }
    Ok(())
}

fn spawn(ctx: &Context, name: &str, opts: SpawnOptions, json: bool) -> anyhow::Result<ExitCode> {
    let name: AgentName = name.parse()?;
    let report = agent::spawn(ctx, &name, opts)?;
    if json {
        output::json(&report)?;
        return Ok(ExitCode::SUCCESS);
    }
    for w in &report.warnings {
        output::warning(w);
    }
    let a = &report.agent;
    output::success(&format!(
        "spawned {} in {:.2}s",
        a.name, report.elapsed_secs
    ));
    output::fields(&[
        (
            "worktree",
            format!("{}  (branch {})", a.worktree.display(), a.branch),
        ),
        ("base", a.base.short()),
        ("fork", format!("{} — {}", a.backend, report.backend_reason)),
        ("sandbox", a.isolation.to_string()),
        (
            "stamped",
            format!("{}/{} tracked files", a.stamp.stamped, a.stamp.tracked),
        ),
        ("run", format!("ferry run {} -- cargo build", a.name)),
    ]);
    Ok(ExitCode::SUCCESS)
}

/// The syntax `ferry env` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum EnvShell {
    Sh,
    Powershell,
}

impl EnvShell {
    /// `sh`, except on Windows outside Git Bash (which sets `MSYSTEM`).
    fn detect() -> Self {
        if cfg!(windows) && std::env::var_os("MSYSTEM").is_none() {
            EnvShell::Powershell
        } else {
            EnvShell::Sh
        }
    }
}

fn env(ctx: &Context, meta: &AgentMeta, shell: EnvShell, json: bool) -> anyhow::Result<ExitCode> {
    let report = agent::env_report(ctx, meta)?;
    if json {
        output::json(&report)?;
        return Ok(ExitCode::SUCCESS);
    }
    // Deliberately no CARGO_TARGET_DIR: agent tools copy their environment
    // into sub-agents working in other worktrees, so an exported target dir
    // would make them share build state. The shim resolves it per call.
    match shell {
        EnvShell::Sh => {
            let q = |p: &std::path::Path| config::shell_quote(&p.to_string_lossy());
            println!("export FERRY_WORKTREE={}", q(&report.worktree));
            println!(
                "export PATH={}:\"$PATH\"",
                config::sh_path(&report.shim_dir)
            );
        }
        EnvShell::Powershell => {
            let q = |p: &std::path::Path| format!("'{}'", p.to_string_lossy().replace('\'', "''"));
            println!("$env:FERRY_WORKTREE = {}", q(&report.worktree));
            println!(
                "$env:PATH = {} + [IO.Path]::PathSeparator + $env:PATH",
                q(&report.shim_dir)
            );
        }
    }
    println!(
        "# cargo in {} now builds in agent {}'s sandbox; other commands: ferry run {} -- <command>",
        report.worktree.display(),
        report.name,
        report.name
    );
    Ok(ExitCode::SUCCESS)
}

fn hook(event: &HookEvent) -> anyhow::Result<ExitCode> {
    let mut stdin = std::io::stdin().lock();
    match event {
        HookEvent::ClaudeWorktreeCreate => {
            crate::hook::claude_worktree_create(&mut stdin, &mut std::io::stdout().lock())?;
        }
        HookEvent::ClaudeWorktreeRemove => crate::hook::claude_worktree_remove(&mut stdin)?,
        HookEvent::ClaudeSessionStart => crate::hook::claude_session_start(&mut stdin)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn list(ctx: &Context, json: bool) -> anyhow::Result<ExitCode> {
    let bases = base::list(ctx)?;
    let agents = agent::list(ctx)?;
    if json {
        output::json(&serde_json::json!({ "bases": bases, "agents": agents }))?;
        return Ok(ExitCode::SUCCESS);
    }
    output::heading("Bases");
    if bases.is_empty() {
        println!("  {DIM}none — run `ferry base`{DIM:#}");
    } else {
        let rows: Vec<Vec<String>> = bases
            .iter()
            .map(|b| {
                vec![
                    if b.current { "*".into() } else { String::new() },
                    b.id.short(),
                    b.meta
                        .as_ref()
                        .map_or_else(|| "incomplete".into(), |m| output::age(m.created_unix)),
                    human_bytes(b.disk_bytes),
                    match b.agents.len() {
                        0 => "-".into(),
                        n => format!("{n} agent{}", if n == 1 { "" } else { "s" }),
                    },
                ]
            })
            .collect();
        output::table(&["", "ID", "BUILT", "DISK", "USED BY"], &rows);
    }
    println!();
    output::heading("Agents");
    if agents.is_empty() {
        println!("  {DIM}none — run `ferry spawn <name>`{DIM:#}");
        return Ok(ExitCode::SUCCESS);
    }
    let rows: Vec<Vec<String>> = agents
        .iter()
        .map(|a| {
            let status = if a.meta.state == agent::AgentState::Creating {
                format!("{YELLOW}incomplete{YELLOW:#}")
            } else if a.busy {
                format!("{GREEN}running{GREEN:#}")
            } else if !a.worktree_exists {
                format!("{YELLOW}worktree missing{YELLOW:#}")
            } else {
                "idle".into()
            };
            let base = if a.base_is_current {
                a.meta.base.short()
            } else {
                format!("{} (old)", a.meta.base.short())
            };
            vec![
                format!("{BOLD}{}{BOLD:#}", a.meta.name),
                base,
                a.meta.backend.to_string(),
                a.meta.isolation.to_string(),
                a.private_bytes.map_or_else(|| "?".into(), human_bytes),
                status,
                a.meta.worktree.display().to_string(),
            ]
        })
        .collect();
    output::table(
        &[
            "NAME", "BASE", "FORK", "SANDBOX", "DISK", "STATUS", "WORKTREE",
        ],
        &rows,
    );
    Ok(ExitCode::SUCCESS)
}

fn rm(ctx: &Context, name: &str, force: bool, json: bool) -> anyhow::Result<ExitCode> {
    let report = agent::remove(ctx, &name.parse()?, force)?;
    if json {
        output::json(&report)?;
        return Ok(ExitCode::SUCCESS);
    }
    output::success(&format!("removed {}", report.name));
    if !report.branch_deleted && report.branch_commits > 0 {
        println!(
            "  kept branch {} ({} commit{} not in the agent's starting point)",
            report.branch,
            report.branch_commits,
            if report.branch_commits == 1 { "" } else { "s" }
        );
    }
    if let Some(rescued) = &report.rescued_ref {
        println!("  kept commits that were on no branch as {rescued}");
    }
    Ok(ExitCode::SUCCESS)
}

fn gc(ctx: &Context, dry_run: bool, json: bool) -> anyhow::Result<ExitCode> {
    let report = base::gc(ctx, dry_run)?;
    if json {
        output::json(&report)?;
        return Ok(ExitCode::SUCCESS);
    }
    let verb = if dry_run { "would remove" } else { "removed" };
    for name in &report.reaped_agents {
        println!("  {verb} abandoned agent {name} (created by an agent tool, clean and idle)");
    }
    for id in &report.removed {
        println!("  {verb} base {}", id.short());
    }
    for id in &report.busy {
        println!("  skipped base {} (being built)", id.short());
    }
    for warning in &report.warnings {
        output::warning(warning);
    }
    output::success(&format!(
        "{verb} {} agent{} and {} base{}, {} freed",
        report.reaped_agents.len(),
        if report.reaped_agents.len() == 1 {
            ""
        } else {
            "s"
        },
        report.removed.len(),
        if report.removed.len() == 1 { "" } else { "s" },
        human_bytes(report.freed_bytes)
    ));
    Ok(ExitCode::SUCCESS)
}

fn doctor_cmd(
    cwd: &std::path::Path,
    apparmor_profile: bool,
    json: bool,
) -> anyhow::Result<ExitCode> {
    if apparmor_profile {
        let exe = std::env::current_exe().context("cannot locate the ferry binary")?;
        let exe = crate::sys::canonicalize(&exe).unwrap_or(exe);
        if let Some(unusable) = doctor::apparmor_path_unusable(&exe) {
            anyhow::bail!(unusable);
        }
        let warning = doctor::apparmor_path_problem(&exe);
        let profile = doctor::apparmor_profile(&exe);
        if json {
            output::json(&serde_json::json!({
                "binary": exe,
                "profile": profile,
                "warning": warning,
            }))?;
        } else {
            if let Some(warning) = &warning {
                output::warning(warning);
            }
            print!("{profile}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let ctx = Context::discover(cwd);
    let report = doctor::run(ctx.as_ref().map_err(ToString::to_string));
    if json {
        output::json(&report)?;
    } else {
        for check in &report.checks {
            let mark = match check.status {
                doctor::Status::Ok => format!("{GREEN}✓{GREEN:#}"),
                doctor::Status::Info => format!("{DIM}·{DIM:#}"),
                doctor::Status::Warn => format!("{YELLOW}!{YELLOW:#}"),
                doctor::Status::Fail => format!("{}✗{:#}", output::RED, output::RED),
            };
            println!("  {mark} {BOLD}{:<11}{BOLD:#} {}", check.name, check.detail);
            if let Some(fix) = &check.fix {
                println!("    {}fix:{:#} {fix}", output::CYAN, output::CYAN);
            }
        }
        print_plan(&report);
    }
    Ok(if report.has_failures() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn print_plan(report: &doctor::Report) {
    let Some(plan) = &report.plan else { return };
    let backend = plan
        .backend
        .map_or_else(|| "none".into(), |b| b.to_string());
    println!(
        "\n  {BOLD}→{BOLD:#} agents fork with {BOLD}{backend}{BOLD:#} ({}) and build {}",
        plan.backend_reason,
        if plan.incremental_reuse {
            "in a namespace sandbox at canonical paths (incremental reuse on the first edit)"
        } else {
            "at their own paths (only crates that captured the base's paths rebuild; no \
             incremental reuse on the first edit)"
        }
    );
}

fn probe(dir: &std::path::Path) -> anyhow::Result<ExitCode> {
    #[cfg(target_os = "linux")]
    {
        let result = crate::sandbox::linux::probe_in_child(dir);
        output::json(&result)?;
        Ok(ExitCode::SUCCESS)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dir;
        anyhow::bail!("namespace probes are Linux-only")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_requires_separator_and_name_is_optional() {
        let cli = Cli::try_parse_from(["ferry", "run", "a1", "--", "cargo", "build"]).unwrap();
        let Command::Run { name, command } = cli.command else {
            panic!()
        };
        assert_eq!(name.as_deref(), Some("a1"));
        assert_eq!(
            command,
            vec![OsString::from("cargo"), OsString::from("build")]
        );

        let cli = Cli::try_parse_from(["ferry", "run", "--", "cargo", "test"]).unwrap();
        let Command::Run { name, .. } = cli.command else {
            panic!()
        };
        assert!(name.is_none());

        assert!(Cli::try_parse_from(["ferry", "run", "a1"]).is_err());
    }

    #[test]
    fn watch_options_need_watch_and_exclude_cold() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["ferry", "base"], args].concat());
        let Command::Base {
            watch,
            interval,
            gc,
            ..
        } = parse(&["--watch", "--interval", "0.2", "--gc"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert!(watch && gc);
        assert_eq!(interval, Some(std::time::Duration::from_millis(200)));
        assert!(parse(&["--watch", "--cold"]).is_err());
        assert!(parse(&["--interval", "1"]).is_err());
        assert!(parse(&["--gc"]).is_err());
        assert!(parse(&["--watch", "--interval", "0"]).is_err());
        // Too long for a `Duration`, and too short to be anything but zero.
        assert!(parse(&["--watch", "--interval", "1e20"]).is_err());
        assert!(parse(&["--watch", "--interval", "1e-300"]).is_err());
        assert!(parse(&["--cold"]).is_ok());
    }

    #[test]
    fn json_flag_is_global() {
        let cli = Cli::try_parse_from(["ferry", "list", "--json"]).unwrap();
        assert!(cli.json);
        let cli = Cli::try_parse_from(["ferry", "--json", "doctor"]).unwrap();
        assert!(cli.json);
    }

    #[test]
    fn setup_apparmor_dry_run_excludes_yes() {
        assert!(Cli::try_parse_from(["ferry", "setup", "apparmor", "--dry-run", "--yes"]).is_err());
        let cli =
            Cli::try_parse_from(["ferry", "--json", "setup", "apparmor", "--dry-run"]).unwrap();
        assert!(cli.json);
        let Command::Setup {
            what: SetupCommand::Apparmor {
                dry_run, bin_dir, ..
            },
        } = cli.command
        else {
            panic!()
        };
        assert!(dry_run);
        assert_eq!(bin_dir, PathBuf::from("/usr/local/bin"));
        let cli = Cli::try_parse_from(["ferry", "setup", "apparmor", "-y", "--uninstall"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Setup {
                what: SetupCommand::Apparmor {
                    yes: true,
                    uninstall: true,
                    ..
                }
            }
        ));
    }
}
