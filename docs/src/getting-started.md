# Getting started

This page installs Ferry, builds a warm base, and runs two agents' test suites
in parallel. Most of the time goes into your first warm build.

The sample output on this page comes from a one-crate demo repository on
Linux (btrfs), with paths shortened.

## Requirements

- Linux, macOS, or Windows 10/11 (see [Platform support](platforms.md#windows)).
- `git` 2.20 or newer.
- Rust installed with [rustup](https://rustup.rs). Ferry runs whatever `cargo`
  and `rustc` your repository selects, including `rust-toolchain.toml`.
- For incremental reuse on an agent's first edit: Linux with unprivileged user
  namespaces. `ferry doctor` tells you whether you have them. Without them Ferry
  still works, reusing unchanged crates but recompiling an edited one from
  scratch (see [Platform support](platforms.md)).

## Install

```sh
# From crates.io (the crate is ferry-rs; the binary is ferry)
cargo install ferry-rs --locked

# Prebuilt binary, Linux and macOS
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/patwrall/ferry/releases/latest/download/ferry-rs-installer.sh | sh

# Homebrew
brew install patwrall/tap/ferry
```

Or build from a clone of the repository:

```sh
git clone https://github.com/patwrall/ferry
cargo install --path ferry --locked
```

On Ubuntu 24.04 and later, also run `ferry setup apparmor` (it shows each step
and asks before one `sudo` call), or read
[AppArmor on Ubuntu](troubleshooting.md#apparmor-on-ubuntu-2404-and-later)
before relying on the namespace sandbox.

## 1. Check the machine

Run `ferry doctor` inside the repository you want to use:

```console
$ ferry doctor
  ✓ git         git version 2.55.0
  ✓ repository  /home/you/src/myproject
  · config      no ferry.toml; using defaults (`ferry init` writes one)
  ✓ toolchain   rustc 1.99.0 (stable), cargo 1.99.0 (5f94df478 2026-08-27)
  ✓ freshness   mtime stamping of files proven identical to the base
  ✓ store       /home/you/.cache/ferry/myproject-d5c4fd3a48754db4 (btrfs, 30.3 GiB free)
  ✓ reflink     copy-on-write clones work on btrfs
  ✓ namespaces  unprivileged user + mount namespaces work
  ✓ overlayfs   mounts inside user namespaces

  → agents fork with reflink (btrfs supports copy-on-write clones) and build in a namespace sandbox at canonical paths (incremental reuse on the first edit)
```

The last line is the plan: how agents will fork their target dir and whether
their first edit can reuse rustc's incremental state. A check marked `!` comes
with a `fix:` line. `ferry doctor` exits non-zero only when a check fails
outright (no git, no toolchain, not in a repository).

## 2. Write a config (optional)

```sh
ferry init
```

This writes `ferry.toml` to the root of your main checkout and prints the same
plan. Ferry works without it; the defaults warm the base with
`cargo build --workspace` and `cargo test --no-run --workspace`. Edit it to
warm other commands or profiles: see [Configuration](configuration.md).

Ferry reads `ferry.toml` from the main checkout's working tree, so you do not
have to commit it for agents to use it.

## 3. Build the base

```console
$ ferry base
ferry: running `cargo build --workspace`
   ...
ferry: running `cargo test --no-run --workspace`
   ...
✓ base 9ae1f5d4b0aa-20509c3c ready in 0.3s (namespace sandbox, mtime freshness)
```

A base is an immutable warm build of one commit (`HEAD` unless you pass
`--rev`). It is built once and never modified. When your main branch moves
on, run `ferry base` again: the new base is seeded from the previous one, so
only what changed is rebuilt. Pass `--cold` to build from scratch. To have
this happen on its own, keep `ferry base --watch` running in a spare
terminal: it builds a new base whenever `HEAD` moves.

## 4. Spawn agents

```console
$ ferry spawn a1
✓ spawned a1 in 0.09s
  worktree  /home/you/.local/share/ferry/worktrees/myproject-d5c4fd3a48754db4/a1  (branch ferry/a1)
  base      9ae1f5d4b0aa-20509c3c
  fork      reflink — btrfs supports copy-on-write clones
  sandbox   namespace
  stamped   3/3 tracked files
  run       ferry run a1 -- cargo build
$ ferry spawn a2
```

Each agent gets a git worktree on its own branch (`ferry/<name>`, starting at
`HEAD` unless you pass `--from`) and a private fork of the base's target dir.
Spawning never builds anything.

## 5. Build in parallel

Point each agent (or each terminal) at its worktree and run cargo through
Ferry. Either use `ferry run`:

```sh
# terminal 1
ferry run a1 -- cargo test

# terminal 2
ferry run a2 -- cargo test
```

or put Ferry's cargo shim first on `PATH` and use plain cargo inside each
worktree:

```sh
export PATH="$(ferry shim):$PATH"    # run this inside the repository
cd ~/.local/share/ferry/worktrees/myproject-d5c4fd3a48754db4/a1
cargo test
```

The shim resolves the agent from the directory cargo runs in, on every call.
Inside an agent worktree it builds in that agent's sandbox with that agent's
target dir. Anywhere else it runs the real cargo unchanged, so it is safe to
leave on `PATH`.

Both builds start warm. Each agent only rebuilds the crates its own edits
touch, and neither agent's build can change the other's.

## 6. See what exists

```console
$ ferry list
Bases
     ID                     BUILT    DISK     USED BY
  *  9ae1f5d4b0aa-20509c3c  30s ago  7.5 MiB  2 agents

Agents
  NAME  BASE                   FORK     SANDBOX    DISK       STATUS  WORKTREE
  a1    9ae1f5d4b0aa-20509c3c  reflink  namespace  136.0 KiB  idle    /home/you/.local/share/ferry/worktrees/myproject-d5c4fd3a48754db4/a1
  a2    9ae1f5d4b0aa-20509c3c  reflink  namespace  0 B        idle    /home/you/.local/share/ferry/worktrees/myproject-d5c4fd3a48754db4/a2
```

`*` marks the current base, which new agents fork from. `DISK` for an agent is
the build state it holds beyond the base.

## 7. Clean up

```sh
ferry rm a1
ferry gc
```

`ferry rm` refuses while the worktree has uncommitted changes (untracked files
count) or while processes still run in the agent's
sandbox; `--force` overrides both. It deletes the
agent's branch only if the branch has no new commits, and otherwise tells you
how many it kept. `ferry gc` deletes bases that are neither current nor used
by an agent.

## Running what you built

Inside the namespace sandbox, the paths cargo prints exist only there, and
with the overlay backend so does the whole target dir. Run binaries and tests
through Ferry:

```sh
ferry run a1 -- cargo run --bin myapp
ferry shell a1        # an interactive shell in a1's build context
```

Inside `ferry run` and `ferry shell`, the target dir is at the path that
`ferry --json env a1` prints as `build_target_dir`. Reflink and copy targets
are also on the host, at `host_target_dir`.

## Next steps

- [Agent integrations](agents.md): Claude Code hooks, Codex, and any other
  agent.
- [Configuration](configuration.md): warm `clippy`, release builds or custom
  commands.
- [Troubleshooting](troubleshooting.md) if `ferry doctor` reports a problem.
