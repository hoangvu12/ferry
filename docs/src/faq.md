# FAQ

## Is Ferry a sandbox for untrusted agents?

No. Ferry isolates build state, not processes. Agents run as your user, with
your files and your network. The Linux namespace Ferry creates only changes
two mount points so that every agent builds at the same paths; host paths stay
reachable inside it. Use a VM, a container or your agent tool's sandbox to
contain code you do not trust. See
[`SECURITY.md`](https://github.com/patwrall/ferry/blob/main/SECURITY.md).

## Why not point every worktree at one `CARGO_TARGET_DIR`?

Because it gives wrong builds. Cargo compares source mtimes with the time it
last built each crate. If agent B edits a file, then agent A edits it
differently and builds, B's next build finds its source older than A's
artifact and rebuilds nothing: B runs A's code. Ferry's test suite reproduces
this with plain cargo in `tests/contamination.rs`. Cargo's checksum freshness
(nightly) narrows the problem but does not close it: build-script inputs are
still compared by mtime, so B can still get A's build-script output. Separately,
Cargo holds an exclusive lock on the build directory for the whole build, so
agents sharing one target dir wait for each other.

## How does Ferry compare with sccache, kache or plain worktrees?

| Approach | New worktree | First edit in it | Parallel agents |
|---|---|---|---|
| Plain worktrees, one target dir each | cold build | normal | correct |
| Copied target dir (`cp --reflink`, Worktrunk's `copy-ignored`) | dependencies reused; workspace crates rebuild, because a fresh checkout's sources are newer than the copied artifacts | not incremental: the path changed | correct |
| Shared `CARGO_TARGET_DIR` | warm | depends on the other agents | builds wait on Cargo's lock, and one agent can silently get another's code |
| sccache | registry crates hit; workspace crates miss, because the key includes the working directory and absolute `CARGO_*` paths | not incremental: sccache does not cache incremental units | correct, with a target dir per worktree |
| kache 1.2.2 | warm: a content-addressed `RUSTC_WRAPPER` with path-normalized keys restores artifacts | not incremental: restored artifacts carry no incremental state | correct, with a target dir per worktree |
| Ferry | warm, nothing rebuilt | incremental on Linux with user namespaces; elsewhere unchanged crates are reused and the edited crates recompile from scratch | correct, each agent has its own fork |

kache and Ferry solve different parts of the problem and can be combined.
kache shares artifacts across machines, commits and CI through remote stores,
which Ferry does not do at all. Ferry gives each local agent a private,
already-incremental build. kache's main branch (unreleased as of this
writing) adds incremental seeding between worktrees: a crate is seeded once
another worktree has already built an edit of it. One caveat when combining
them: kache strips Cargo's incremental argument for every crate it handles,
so a Ferry base built with `RUSTC_WRAPPER=kache` has no incremental state.
Ferry does not ship an integration for this yet.

Cargo itself may eventually share build results between workspaces: a
cross-workspace cache is a 2026 Rust project goal, starting with simple crates
on nightly. It is not usable yet.

## Can I use sccache or kache with Ferry?

As `RUSTC_WRAPPER`, both leave Cargo's fingerprints alone, so crate-level
reuse keeps working. But sccache refuses incremental units, and kache strips
the incremental argument, so you lose Ferry's incremental reuse for whatever
they handle. For now, use them for builds outside Ferry.

## Do I have to commit `ferry.toml`?

No. Ferry reads it from the main checkout's working tree. Commit it if you
want everyone on the project to warm the same commands. Note that a
`ferry.toml` can run arbitrary commands when someone runs `ferry base`.

## Where are my agent's binaries?

In its own target dir, at the path `ferry --json env <name>` prints as
`build_target_dir`; in the namespace sandbox that path exists only inside it.
Use `ferry run <name> -- cargo run`, or work inside `ferry shell <name>`. With
the reflink and copy backends, `host_target_dir` is a real directory you can
also read from the host; with the overlay backend the target dir exists only
inside the sandbox.

## Which cargo commands go through Ferry?

All of them, including subcommands such as `cargo clippy` or `cargo nextest`,
as long as `cargo` resolves to the shim: inside `ferry run` and `ferry shell`,
or with the shim first on your `PATH`. Tools that run `rustc` directly, or
call cargo by an absolute path, bypass it. Warm what agents run: the default
base warms `build`, `test --no-run`, `check` and `clippy --all-targets` for the
`dev` profile. `check` shares only build scripts and proc macros with
`build`, `clippy` shares everything but workspace members with `check`, and
profiles share nothing (see
[Configuration](configuration.md#base-what-warms-a-base)).

## What happens when my main branch moves on?

Run `ferry base` again, or keep `ferry base --watch` running to have that
done each time `HEAD` (or the `--rev` you give it) moves. The new base is
seeded from the previous one, so only what changed is rebuilt, also after
`Cargo.toml` or `Cargo.lock` changes, and new agents fork from it. Existing
agents keep the base they were forked from. If you rebase an agent's branch,
the files that changed get new mtimes and Cargo rebuilds what they affect;
Ferry never re-stamps a live worktree.

## What does Ferry add to my repository?

Git worktrees and branches. Each agent is a worktree on its own branch
(`ferry/<name>` by default), and each base is a detached worktree in the
store, locked with `git worktree lock` so git never prunes it. They show up in
`git worktree list`. Ferry writes nothing into your main checkout, except
`ferry.toml` when you run `ferry init` and, in remap-paths mode, one line in
`.git/info/exclude`.

## Does the shim leave anything behind in repositories I never use with Ferry?

No. The shim and the Claude Code `SessionStart` hook look up every git
repository they run in, but only read: a repository without a store gets plain
cargo, and no store directory is created for it.

## Does Ferry handle rust-analyzer?

Not yet. Ferry does not set up rust-analyzer per worktree. An editor whose
rust-analyzer runs cargo in an agent's worktree uses whatever target dir its
own configuration gives it.

## How do I remove everything?

Remove agents with `ferry rm <name>` and unused bases with `ferry gc`. There
is no command that removes the current base. To remove all of Ferry's state
for a repository, delete its store directory (under `~/.cache/ferry` by
default) and its worktree directory, then in the repository run
`git worktree list`, `git worktree unlock <path>` for each leftover base
checkout, and `git worktree prune`.

## Does Ferry need root?

No. It uses unprivileged user namespaces where they are available.
`ferry doctor` may suggest `sudo` commands (a sysctl, for example) for you to
review and run; it never runs them itself. The one exception is `ferry setup
apparmor` on Ubuntu 24.04 and later, which you run on purpose: it prints the
profile, the root script and the exact `sudo` command, asks, and then runs
that one command.

## Windows?

Yes, as on macOS: agents build at their own paths and forks are full
copies. See [Platform support](platforms.md#windows).

## What are Ferry's limitations?

- **The incremental win needs Linux user namespaces.** Where they are blocked
  (Ubuntu 24.04+ without the AppArmor profile, Docker's default seccomp
  profile, hardened systems), agents reuse unchanged crates but recompile an
  edited crate from scratch, and the few crates that captured the base's
  absolute paths rebuild once after spawn.
- **Overlay copy-up costs disk where clones are unavailable.** On ext4 and
  tmpfs agents fork with overlay, and an agent's first write to a file copies
  all of it into the agent's upper layer; relinked binaries and incremental
  session files make up most of it. `ferry list` shows each agent's share.
- **Paths that cargo prints in the sandbox do not exist on the host.** Run
  built binaries through `ferry run` or `ferry shell`. Reflink and copy
  targets are also reachable on the host at `ferry env`'s
  `host_target_dir`; an overlay target exists only inside the sandbox.
- **Build scripts that write outside `OUT_DIR`** write into the base's
  checkout, and agents do not get those files. `ferry base` warns when it
  happens.
- **mtime freshness has blind spots.** A file whose new content arrives with
  an old mtime (for example copied with preserved timestamps) can look
  unchanged to Cargo. On nightly, checksum freshness covers rustc's inputs;
  build-script inputs stay mtime-based.
- **macOS has no incremental reuse by default.** The remap-paths mode that
  addresses it is experimental and unverified on a Mac.
- **rust-analyzer per worktree is not addressed.**
- **Claude Code 2.1.305 and later** block nested user namespaces in their
  sandbox unless configured; run the session with `ferry run` or see
  [Claude Code's sandbox](agents/claude-code.md#claude-codes-sandbox).
- **Local only.** Ferry does not share builds across machines.

