# Platform support

Ferry gives every agent a warm, private target dir everywhere it runs. What
varies is whether an agent's **first edit** can reuse rustc's incremental
state, which needs the Linux namespace sandbox (or the experimental
[remap-paths mode](remap-paths.md)).

- **Crate-level reuse**: crates whose sources match the base are not rebuilt,
  the workspace's included. An edit rebuilds the edited crate and its
  dependents from scratch. Without a canonical path, the few crates that
  captured the base's absolute paths (a test baking in
  `env!("CARGO_MANIFEST_DIR")`, users of a proc macro reading it) also
  rebuild once after spawn.
- **Incremental reuse**: on top of that, rustc recompiles only the parts of an
  edited crate that changed. In the prototype on ripgrep (2 vCPU) the first
  edit after spawning took 1.75 s with incremental reuse and 5.3 s with
  crate-level reuse only.

Agents in the namespace sandbox get both crate-level and incremental reuse.

After an agent's first build, its own incremental state exists and later
edits are incremental either way.

## Summary

| Platform | Fork backends | Sandbox | First edit after spawn |
|---|---|---|---|
| Linux with unprivileged user namespaces (Debian, Fedora, Arch, NixOS, RHEL without STIG hardening, ...) | reflink, overlay, copy | namespace sandbox with canonical paths | incremental reuse |
| Ubuntu 24.04 and later, with Ferry's AppArmor profile installed | reflink, overlay, copy | namespace sandbox | incremental reuse |
| Ubuntu 24.04 and later, default policy | reflink, copy | none | crate-level reuse |
| Docker, default seccomp profile | reflink, copy | none | crate-level reuse |
| macOS (APFS) | reflink (`clonefile`), copy | none | crate-level reuse; experimental remap-paths mode |
| Windows 10 and 11 (NTFS) | copy | none | crate-level reuse |

## Linux

The full feature set: agents build in a private user and mount namespace, at
canonical paths. Forks are copy-on-write clones where the store's filesystem
supports them; elsewhere the overlay backend forks in constant time. This
needs:

- unprivileged user namespaces, which are on by default on Debian 12 and 13,
  Fedora, RHEL 9 and 10, Arch and NixOS, but not on Ubuntu 24.04 and later
  (AppArmor) or in Docker's default seccomp profile, and are disabled by the
  DISA STIG for RHEL (`user.max_user_namespaces = 0`);
- for the overlay backend, Linux 5.11 or newer and a store filesystem that
  supports user extended attributes (ext4, btrfs, XFS; tmpfs from 6.6).

When namespaces are unavailable, `sandbox = "auto"` falls back to building at
host paths, and `ferry base` warns about it. See
[Troubleshooting](troubleshooting.md#user-namespaces) for every case.

### Ubuntu 24.04 and later

Ubuntu restricts unprivileged user namespaces with AppArmor
(`kernel.apparmor_restrict_unprivileged_userns = 1`). Creating one succeeds,
but the process gets no capabilities inside it, so mapping its uid and
mounting fail. Ferry detects this and either:

- uses the namespace sandbox once `ferry setup apparmor` has installed ferry
  to a root-owned path with its AppArmor profile (one confirmed `sudo` call;
  CI runs Ferry's sandbox tests on Ubuntu 24.04 this way), or
- falls back to crate-level reuse.

Ubuntu on WSL2 runs Microsoft's kernel, which has no such sysctl, and Linux
Mint 22 ships the restriction switched off; `ferry setup apparmor` reads the
sysctl and says when nothing is needed.

See [AppArmor on Ubuntu](troubleshooting.md#apparmor-on-ubuntu-2404-and-later).
Ubuntu 23.10 ships the restriction switched off.

### Containers

Docker's default seccomp profile allows `unshare` and `mount` only with
`CAP_SYS_ADMIN`, and on AppArmor hosts Docker's default AppArmor profile
denies mounts, so Ferry falls back to crate-level reuse inside Docker.
Rootless Podman's default profiles allow them. On Ubuntu 24.04 and later
hosts, the host's restriction applies to container processes too, and only
the host can lift it. If you run Ferry in a container with namespaces
allowed, put `FERRY_HOME` on a volume, because overlayfs cannot use the
container's overlay root filesystem as its upper layer. See
[Containers](troubleshooting.md#containers) for the seccomp profile and a
devcontainer example. These container facts come from the profiles' sources
and standalone probes; Ferry itself has not been tested in containers.

### Filesystems

Where the store lives decides which fork backends work and what they cost.

| Filesystem | reflink backend | overlay upper layer | `auto` picks | Notes |
|---|---|---|---|---|
| btrfs | yes | yes | reflink | overlay copy-up is a reflink |
| XFS | yes (`reflink=1`, the mkfs default since xfsprogs 5.1) | expected to work, not tested | reflink | overlay copy-up is a reflink |
| OpenZFS | 2.2.0, and 2.3.0 or newer; off by default in 2.2.2 to 2.2.x | not tested | reflink where block cloning is on | |
| ext4 | no | yes | overlay | overlay copy-up copies the whole file |
| tmpfs | no | Linux 6.6 or newer | overlay (6.6+) | |
| NFS, CIFS, FUSE | not recommended | not recommended | | `ferry doctor` warns: slow builds, unreliable mtimes |

Without the namespace sandbox `auto` never picks overlay: it uses reflink
where clones work and copy elsewhere.

`ferry doctor` probes reflinks and overlayfs on the store's actual filesystem
rather than trusting this table.

## macOS

Agents fork the base's target dir with one `clonefile(2)` call on APFS (the
`reflink` backend) and build at their own paths. macOS has no per-process
mount namespaces, so there is no canonical path: crates are reused except the
few that captured the base's absolute paths, which rebuild once after spawn,
and the first edit of a crate loses its incremental state.

The experimental [remap-paths mode](remap-paths.md) is designed for this
case. On Linux it reuses 12 of 13 library codegen units after a fork to a new
path, against 0 of 13 without it. That it works the same on macOS is predicted
from rustc's source but has not been verified on a Mac.

Bases and forks live in `~/Library/Caches/ferry`; Ferry excludes base target
dirs and agent build state from Time Machine backups. Agent worktrees live in
`~/Library/Application Support/ferry/worktrees`, because macOS may purge
caches.

## Windows

Windows works like macOS without clones: no mount namespaces, so agents
build at their own paths with a [relocation plan](architecture.md), and
forks are full copies (a 4 GB target copies in about 8 s on NVMe). What
differs from Unix:

- **The shim is a hard link.** `bin\cargo.exe` (and `bin\ferry-rustc.exe`)
  are hard links to `ferry.exe`, or copies when the store is on another
  volume, because symlinks need Developer Mode or admin rights. A marker
  file in the bin dir keeps Ferry from taking its own `cargo.exe` for the
  real one.
- **No `exec`.** Ferry starts cargo (or `ferry run`'s command) as a child,
  waits for it and exits with its code; Ctrl+C reaches the child through
  the console.
- **Busy agents are tracked with a job object.** `ferry run`, `ferry shell`
  and the shim put their process tree in a per-agent job object, whose
  inheritable handle keeps it findable while any descendant runs, so `rm`,
  `list` and `gc` see processes that outlive the one holding `busy.lock`,
  and name their pids.
- **Builds run at below-normal priority.** Agent and base cargo processes
  start `BELOW_NORMAL_PRIORITY_CLASS`, which rustc and the linker inherit,
  so several agents building at once leave the desktop responsive. Set
  `FERRY_BUILD_PRIORITY=normal` to turn it off.
- **Relocation reads every separator spelling.** Windows records paths with
  `\`, `\\` (JSON, Debug output) or `/`; relocation folds them all to `/`
  before matching, and MSVC binaries, which Cargo names without a hash,
  are matched by name.
- **Warm commands** made only of words and quotes, as the generated ones
  are, run directly; others run with `sh` from Git for Windows when it is
  on `PATH`, else with `cmd /C`.
- **`ferry env`** prints PowerShell (`ferry env a1 | Invoke-Expression`)
  outside Git Bash and `sh` inside it (`--shell` picks one). The Claude Code
  `SessionStart` hook writes the Git Bash form, which Claude Code's Bash
  tool uses on Windows.
- **Long paths.** Ferry passes `core.longpaths=true` to the git commands it
  runs. Turn on `LongPathsEnabled` in Windows, or keep `FERRY_HOME` short.

Not yet on Windows: copy-on-write clones (ReFS block cloning on a Dev Drive)
and the experimental remap-paths mode. The store and worktrees default to
`%LOCALAPPDATA%\ferry` (worktrees in `%LOCALAPPDATA%\ferry\worktrees`), whichever shell runs Ferry.

## What `ferry doctor` reports

`ferry doctor` runs these checks and ends with the plan for new agents.

| Check | Status | What it means |
|---|---|---|
| `git` | ok or fail | `git --version`; fails if git is missing |
| `repository` | ok or fail | the main checkout Ferry will use; fails outside a git repository |
| `config` | ok or info | the `ferry.toml` that was loaded, or that defaults are in use |
| `toolchain` | ok or fail | the `rustc` and `cargo` the repository selects |
| `clippy` | ok or info | whether `cargo clippy` works, when the toolchain works and the warm commands include the generated clippy command; bases skip it when it does not |
| `freshness` | ok | `checksum freshness (content hashes)` or `mtime stamping of files proven identical to the base` |
| `store` | ok or warn | the store path, its filesystem and free space; warns on NFS, CIFS and FUSE, and on overlayfs (a container's root filesystem) |
| `reflink` | ok or info | whether copy-on-write clones work on the store's filesystem |
| `namespaces` | ok or warn (Linux) | whether an unprivileged user and mount namespace with a bind mount works; the fix names the cause it found (AppArmor, with `ferry setup apparmor` as the fix or the path of a different ferry binary a Ferry profile covers; a sysctl; or a container) |
| `overlayfs` | ok or warn (Linux) | whether overlayfs mounts inside such a namespace, with upper and work dirs in the store |
| `sandbox` | info (macOS) | that macOS has no mount namespaces |
| `remap-paths` | info | when bases would remap paths (remap-paths on, no namespace sandbox, rustc 1.98 or newer): `cargo clippy` runs are not remapped |

The plan line reads like:

```text
  → agents fork with reflink (btrfs supports copy-on-write clones) and build in a namespace sandbox at canonical paths (incremental reuse on the first edit)
```

on ext4:

```text
  → agents fork with overlay (overlayfs works in the namespace sandbox on ext4; no copy-on-write clones on ext4 (FICLONE: Operation not supported (os error 95))) and build in a namespace sandbox at canonical paths (incremental reuse on the first edit)
```

or, without namespaces:

```text
  → agents fork with reflink (btrfs supports copy-on-write clones) and build at their own paths (only crates that captured the base's paths rebuild; no incremental reuse on the first edit)
```

`ferry --json doctor` adds the raw probe results and the sysctls that explain
namespace failures: `user.max_user_namespaces`,
`kernel.unprivileged_userns_clone`,
`kernel.apparmor_restrict_unprivileged_userns`,
`kernel.apparmor_restrict_unprivileged_unconfined` and
`kernel.yama.ptrace_scope`, where they exist, and `probe_label`, the namespace
probe's AppArmor label (`ferry (enforce)` when Ferry's profile attached).
