# Claude Code

There are two ways to use Ferry with Claude Code:

- **R1: run the whole session in an agent's sandbox.** One command, nothing to
  configure. Best for interactive sessions you start yourself.
- **R2: let Claude create Ferry agents through hooks.** Covers
  `claude --worktree`, `EnterWorktree`, background sessions and subagents with
  `isolation: worktree`, so every worktree Claude creates is a Ferry agent with
  a warm build.

Both need a base: run `ferry base` in the repository first, and again when
your main branch moves on.

Example files are in
[`examples/claude-code/`](https://github.com/patwrall/ferry/tree/main/examples/claude-code).

## R1: `ferry run a1 -- claude`

```sh
ferry spawn a1
ferry run a1 -- claude
```

Claude starts in `a1`'s worktree at its host path, so its transcripts and
memory stay separate per agent. Every `cargo` it runs goes through the shim
and builds in `a1`'s target dir. Because the namespace exists before Claude
starts, this works whatever Claude's own sandbox allows (see
[Claude Code's sandbox](#claude-codes-sandbox)).

Map your own uid, not root: Ferry already does, and Claude refuses
`--dangerously-skip-permissions` as root.

## R2: WorktreeCreate, WorktreeRemove and SessionStart hooks

Add this to `~/.claude/settings.json` (it is
[`examples/claude-code/settings.json`](https://github.com/patwrall/ferry/blob/main/examples/claude-code/settings.json)):

```json
{
  "hooks": {
    "WorktreeCreate": [
      { "hooks": [ { "type": "command", "command": "ferry hook claude-worktree-create", "timeout": 120 } ] }
    ],
    "WorktreeRemove": [
      { "hooks": [ { "type": "command", "command": "ferry hook claude-worktree-remove", "timeout": 120 } ] }
    ],
    "SessionStart": [
      { "hooks": [ { "type": "command", "command": "ferry hook claude-session-start", "timeout": 30 } ] }
    ]
  }
}
```

If you already have hooks, merge these entries into your `hooks` object.
`ferry` must be on the `PATH` Claude Code runs hooks with.

You can put the hooks in a project's `.claude/settings.json` instead, but only
a **committed** one works: Claude Code loads project settings for a
hook-created worktree from that worktree's checkout, and an uncommitted file in
your main checkout is not in it.

Then start Claude in a worktree as usual:

```sh
cd ~/src/myproject
claude --worktree feature-x
```

This recipe was verified with Claude Code 2.1.280: `WorktreeCreate` created a
Ferry agent, the session's working directory was the agent's worktree,
`SessionStart` put the shim on `PATH`, and `cargo build` in the session was
warm (0.04 s).

### What each hook does

**`ferry hook claude-worktree-create`** reads Claude's JSON on stdin (`cwd`
and the worktree `name`), creates a Ferry agent of that name, and prints the
worktree path as the last line of stdout, which is what Claude Code expects.
Progress goes to stderr.

- The agent name is Claude's worktree name with anything other than letters,
  digits, `-`, `_` and `.` replaced by `-` (`feat/login page` becomes
  `feat-login-page`). If a fully created agent of that name exists, the hook
  reuses it.
- The branch is `ferry/<name>` and starts at `HEAD` of the checkout Claude was
  started from. Claude's `worktree.baseRef` setting does not apply, because the
  hook replaces Claude's own worktree creation.
- The hook never builds a base. In a repository without one (not a Rust
  project, or `ferry base` never run there), it creates the worktree Claude
  would have created itself: `.claude/worktrees/<name>` on branch
  `worktree-<name>`, with no fork. So the hooks can be configured globally
  without breaking `claude --worktree` in other projects.
  `claude-worktree-remove` removes such a worktree with `git worktree remove`,
  which refuses when it has uncommitted changes.
- Claude Code does not process `.worktreeinclude` when a `WorktreeCreate` hook
  is configured, and Ferry does not copy those files either. The only file
  Ferry adds to a new worktree is the base's `Cargo.lock`, when the repository
  does not track one.

**`ferry hook claude-worktree-remove`** runs when Claude removes a worktree it
created through the hook (for example when you exit an interactive session
and choose to remove it). It removes the agent's worktree and fork. It refuses,
exiting non-zero and leaving the worktree in place, when the worktree has
uncommitted changes or processes still run in the agent's sandbox. It deletes
the branch only if the branch has no new commits.

**`ferry hook claude-session-start`** appends one line to `$CLAUDE_ENV_FILE`:

```sh
export PATH=/home/you/.cache/ferry/bin:"$PATH"
```

Claude Code runs that file before every Bash command, so every `cargo` goes
through the shim. It writes only `PATH`. A `CARGO_TARGET_DIR` written there
would also reach subagents working in other worktrees (this was measured),
recreating the shared-target bug. Outside a git repository the hook does
nothing. Outside an agent worktree the shim runs the real cargo unchanged, so
the hook is safe for every session.

### Subagents with `isolation: worktree`

A subagent whose frontmatter has `isolation: worktree` gets its own worktree.
With the hooks installed, that is a Ferry agent with a warm build:

```markdown
---
name: rust-implementer
description: Implements a focused change in this Rust workspace in its own git worktree.
tools: Read, Edit, Write, Bash, Grep, Glob
isolation: worktree
---

You implement one focused change ...
```

The full file is
[`examples/claude-code/agents/rust-implementer.md`](https://github.com/patwrall/ferry/blob/main/examples/claude-code/agents/rust-implementer.md).
Put it in `.claude/agents/` (project) or `~/.claude/agents/` (user).

Claude names subagent worktrees `agent-<hex>`, so their Ferry agents are named
that way too. Claude Code keeps a hook-created subagent worktree when the
subagent finishes and does not run `WorktreeRemove` for it; nothing cleans up
after `claude -p` runs either. `ferry gc` removes these agents for you: it
reaps agents created by the hook that are clean, have no new commits, have
nothing running and are not locked by Claude (`git worktree lock`, which Claude
holds while an agent runs). Agents you created with `ferry spawn` are never
reaped. Run `ferry gc --dry-run` to see what it would remove.

## Claude Code's sandbox

Claude Code's optional sandbox restricts what Bash commands can do. Two things
matter for Ferry:

- **Writes.** cargo writes to the agent's fork and to the canonical mount
  paths, which live under Ferry's store, outside the worktree. Allow writes to
  the store's parent directory (the parent of the directory `ferry shim`
  prints; by default `~/.cache/ferry` on Linux).
- **Nested user namespaces.** Claude Code 2.1.305 and later block them inside
  the sandbox by default, so the shim cannot create the agent's namespace.
  Claude Code 2.1.280 still allowed them.
- **Ubuntu 24.04 and later with Ferry's AppArmor profile.** Everything
  `ferry run` starts runs under the `unpriv_ferry` profile, without
  capabilities in user namespaces, so Claude's own `bwrap` sandbox is expected
  to fail inside `ferry run a1 -- claude` there (not yet measured). Recipes 2
  and 3 below start Claude outside `ferry run`.

Pick one of these:

1. **Recipe R1.** `ferry run a1 -- claude` creates the namespace before
   Claude's sandbox starts. Claude's sandbox still needs the store in
   `allowWrite`:

   ```json
   { "sandbox": { "enabled": true,
       "filesystem": { "allowWrite": ["/home/you/.cache/ferry"] } } }
   ```

2. **Allow nested namespaces** in `~/.claude/settings.json` (Claude Code
   honors this only from user settings, `--settings` or managed settings). It
   turns off one of the sandbox's security boundaries for every command:

   ```json
   { "sandbox": { "allowNestedUserNamespaces": true,
       "filesystem": { "allowWrite": ["/home/you/.cache/ferry"] } } }
   ```

3. **Run cargo outside Claude's sandbox:**

   ```json
   { "sandbox": { "excludedCommands": ["cargo *"] } }
   ```

   Claude Code matches this against the command text. A compound command such
   as `cd crates/x && cargo test` stays sandboxed, and then fails.

Use absolute paths in `allowWrite`. These settings follow Claude Code's
documentation and measurements with `@anthropic-ai/sandbox-runtime` 0.0.81;
they have not been tested end to end with Claude Code 2.1.305 or later.

## Things not to do

- Do not set `worktree.symlinkDirectories: ["target"]`. It makes every
  worktree share one target dir.
- Do not put `CARGO_TARGET_DIR` (or `CARGO_BUILD_BUILD_DIR`) in the settings
  `env` block or in `$CLAUDE_ENV_FILE`.
- Do not start Claude at Ferry's canonical path (`<store>/mnt/src`). Every
  agent has the same canonical path, so they would share one transcript
  directory and memory.
