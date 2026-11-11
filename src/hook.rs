//! Agent-tool hook handlers (`ferry hook <event>`).
//!
//! Claude Code: <https://code.claude.com/docs/en/hooks>. Its `WorktreeCreate`
//! hook replaces git worktree creation for `claude --worktree`, sub-agents
//! with `isolation: worktree` and background sessions; the hook must print
//! the new worktree's absolute path as the last line of stdout, and anything
//! else on stdout would be taken as that path. `WorktreeRemove` receives
//! `worktree_path`; exiting non-zero leaves the directory in place.

use std::io::{Read, Write};
use std::path::PathBuf;

use serde::Deserialize;

use crate::agent::{self, Origin, SpawnOptions};
use crate::context::Context;
use crate::error::{Error, IoContext, Result};
use crate::fsutil;
use crate::store::AgentName;

#[derive(Debug, Deserialize)]
struct WorktreeCreateInput {
    cwd: PathBuf,
    name: String,
}

#[derive(Debug, Deserialize)]
struct WorktreeRemoveInput {
    cwd: Option<PathBuf>,
    worktree_path: PathBuf,
}

/// Handles Claude Code's `WorktreeCreate`: spawns (or resumes) an agent and
/// prints its worktree path. Returns the path.
pub fn claude_worktree_create(stdin: &mut dyn Read, stdout: &mut dyn Write) -> Result<PathBuf> {
    let input: WorktreeCreateInput = read_input(stdin)?;
    let ctx = Context::discover(&input.cwd)?;
    let name = agent_name(&input.name)?;
    let worktree = match agent::load(&ctx, &name) {
        Ok(meta) if meta.origin != Origin::ClaudeHook => {
            return Err(Error::Refused(format!(
                "an agent named `{name}` already exists and was not created by Claude Code; \
                 pick another worktree name"
            )));
        }
        Ok(meta) if meta.state == agent::AgentState::Ready => {
            fsutil::note(&format!("reusing agent {name}"));
            meta.worktree
        }
        Ok(_) => {
            return Err(Error::Refused(format!(
                "agent `{name}` exists but was not fully created; run `ferry rm {name}`"
            )));
        }
        Err(Error::AgentNotFound(_)) => {
            let report = agent::spawn(
                &ctx,
                &name,
                SpawnOptions {
                    origin: Origin::ClaudeHook,
                    ..SpawnOptions::default()
                },
            )?;
            for w in &report.warnings {
                fsutil::note(&format!("warning: {w}"));
            }
            fsutil::note(&format!(
                "spawned {name} ({} fork, {:.2}s)",
                report.agent.backend, report.elapsed_secs
            ));
            report.agent.worktree
        }
        Err(e) => return Err(e),
    };
    writeln!(stdout, "{}", worktree.display()).at("write", "stdout")?;
    Ok(worktree)
}

/// Handles Claude Code's `WorktreeRemove`. Refuses (non-zero exit, worktree
/// left in place) when the worktree has uncommitted changes or processes
/// still run in it; commits stay on the agent's branch.
pub fn claude_worktree_remove(stdin: &mut dyn Read) -> Result<()> {
    let input: WorktreeRemoveInput = read_input(stdin)?;
    let start = input.cwd.unwrap_or_else(|| input.worktree_path.clone());
    let ctx = Context::discover(&start)?;
    if !input.worktree_path.exists() {
        return Ok(());
    }
    let meta = agent::find_by_dir(&ctx, &input.worktree_path)?
        .ok_or_else(|| Error::NotInAgent(input.worktree_path.clone()))?;
    let report = agent::remove(&ctx, &meta.name, false)?;
    if !report.branch_deleted && report.branch_commits > 0 {
        fsutil::note(&format!(
            "kept branch {} ({} new commit(s))",
            report.branch, report.branch_commits
        ));
    }
    Ok(())
}

/// Handles Claude Code's `SessionStart`: puts Ferry's cargo shim first on
/// `PATH` for every Bash command via `$CLAUDE_ENV_FILE`. Only `PATH` is
/// written: the shim resolves the agent per call from the working
/// directory, so it stays correct in sub-agents that inherit this
/// environment.
pub fn claude_session_start(stdin: &mut dyn Read) -> Result<()> {
    #[derive(Deserialize)]
    struct Input {
        cwd: PathBuf,
    }
    let input: Input = read_input(stdin)?;
    let Some(env_file) = std::env::var_os("CLAUDE_ENV_FILE") else {
        return Ok(());
    };
    if crate::git::toplevel(&input.cwd)?.is_none() {
        return Ok(());
    }
    let shim_dir = crate::shim::ensure()?;
    // Claude Code sources the file in its Bash tool, Git Bash on Windows.
    let line = format!(
        "export PATH={}:\"$PATH\"\n",
        crate::config::sh_path(&shim_dir)
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&env_file)
        .at("open", &env_file)?;
    file.write_all(line.as_bytes()).at("write", &env_file)
}

/// Claude names worktrees like `bright-running-fox` or, for sub-agents,
/// `agent-<hex>`. Names Ferry cannot use as-is are mapped to its alphabet
/// plus a hash of the original, so two different Claude names never share
/// one agent (`feat/login` and `feat-login` must not get the same worktree).
fn agent_name(raw: &str) -> Result<AgentName> {
    if let Ok(name) = raw.parse::<AgentName>() {
        return Ok(name);
    }
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let stem: String = cleaned
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .chars()
        .take(48)
        .collect();
    let hash = crate::store::fnv1a64(raw.as_bytes()) & 0xff_ffff;
    let name = if stem.is_empty() {
        format!("claude-{hash:06x}")
    } else {
        format!("{stem}-{hash:06x}")
    };
    name.parse()
}

fn read_input<T: for<'de> Deserialize<'de>>(stdin: &mut dyn Read) -> Result<T> {
    let mut buf = Vec::new();
    stdin.read_to_end(&mut buf).at("read", "stdin")?;
    serde_json::from_slice(&buf).map_err(Error::HookInput)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_names_map_to_distinct_agent_names() {
        assert_eq!(agent_name("t1").unwrap().as_str(), "t1");
        assert_eq!(
            agent_name("agent-abd7add2abe2a458a").unwrap().as_str(),
            "agent-abd7add2abe2a458a"
        );
        let mapped = agent_name("feat/login page").unwrap();
        assert!(mapped.as_str().starts_with("feat-login-page-"), "{mapped}");
        let names = [
            "feat/login",
            "feat-login",
            "feat login",
            "日本-login",
            "ü-login",
            "///",
        ];
        let mapped: std::collections::BTreeSet<String> = names
            .iter()
            .map(|n| agent_name(n).unwrap().to_string())
            .collect();
        assert_eq!(mapped.len(), names.len(), "{mapped:?}");
    }

    #[test]
    fn remove_input_parses_measured_shape() {
        let json = br#"{"session_id":"s","transcript_path":"/t","cwd":"/repo","hook_event_name":"WorktreeRemove","worktree_path":"/w"}"#;
        let input: WorktreeRemoveInput = read_input(&mut &json[..]).unwrap();
        assert_eq!(input.worktree_path, PathBuf::from("/w"));
    }
}
