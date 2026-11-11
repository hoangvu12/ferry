# Ferry

Ferry gives every parallel AI coding agent its own git worktree with an
instantly forked, already-warm Cargo build. Agents stop waiting on cold
compiles, and they stop corrupting each other's builds.

## The problem

Running several coding agents on one Rust repository needs one git worktree
per agent, so their edits do not collide. That leaves two bad options for the
build:

- **A target dir per worktree.** Every new worktree starts cold. On a real
  workspace the first `cargo build` or `cargo test` takes minutes, and every
  agent pays it.
- **One shared `CARGO_TARGET_DIR`.** Builds queue behind each other: Cargo
  holds an exclusive lock on the build directory for the whole build. Worse,
  the result can be silently wrong. Cargo decides whether a crate is fresh by
  comparing source modification times with its own records. If agent B edits
  a file, then agent A edits the same file differently and builds, B's next
  build sees sources older than A's artifacts. Cargo rebuilds nothing, and B
  runs A's code.

The second case is not hypothetical. Ferry's test suite reproduces it with
plain cargo ([`tests/contamination.rs`](https://github.com/patwrall/ferry/blob/main/tests/contamination.rs)):
B's source says `ONLY_IN_B`, B's build rebuilds 0 crates, and B's binary
prints `ONLY_IN_A`. The same file checks that two Ferry agents given the same
edits each build their own code.

## What Ferry does

1. **`ferry base`** builds one immutable, warm copy of your workspace for a
   commit.
2. **`ferry spawn <name>`** creates a git worktree and branch for an agent and
   forks the base's target dir for it: a copy-on-write clone, or where the
   filesystem has none an overlayfs layer, or a plain copy. A copy-on-write
   clone costs time per file, not per byte; an overlay fork takes the same
   time whatever the size of the target dir.
3. **`ferry run <name> -- cargo test`**, or plain `cargo` with Ferry's shim on
   `PATH`, builds in that agent's own target dir. Cargo sees the fork as fresh
   for every file that is identical to the base, so only what the agent
   changed is rebuilt.
4. On Linux, each build runs in a private mount namespace where every agent's
   worktree and target dir appear at the same two paths. rustc's incremental
   state, which records absolute paths, then stays valid, and the agent's
   first edit compiles incrementally.

No agent ever writes to the base or to another agent's build state.

## What Ferry is not

Ferry isolates build state, not processes. Agents run with your user's full
privileges and can read and write anything you can. Use a VM, a container or
your agent tool's own sandbox to contain code you do not trust. See
[`SECURITY.md`](https://github.com/patwrall/ferry/blob/main/SECURITY.md) for
the threat model.

## Status

Ferry is pre-1.0. It runs on Linux, macOS and Windows. The
full benefit (incremental reuse on an agent's first edit) needs unprivileged
user namespaces on Linux. Everywhere else Ferry still gives every agent a warm,
private target dir with its dependencies already built. See
[Platform support](platforms.md).

## Where to go next

- [Getting started](getting-started.md): install Ferry and run two agents.
- [Agent integrations](agents.md): wire Ferry into Claude Code, Codex or any
  other agent.
- [How it works](how-it-works.md): bases, forks, stamping and the sandbox.
- [Troubleshooting](troubleshooting.md): namespaces, AppArmor, filesystems and
  disk usage.
