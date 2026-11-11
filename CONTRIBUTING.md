# Contributing to Ferry

Thanks for helping. Bug reports, docs fixes and pull requests are all welcome.
Before starting on a large change, please open an issue to discuss it. Read
[`docs/src/architecture.md`](docs/src/architecture.md) first: it is the
contract the code is written against, including the invariants every change
must keep.

By contributing you agree that your contributions are dual-licensed under
MIT and Apache-2.0, like the rest of the project, and that you follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

## Development setup

Ferry builds on Linux and macOS (not Windows). You need:

- [rustup](https://rustup.rs). The repository does not pin a toolchain; CI
  uses current stable, the MSRV and nightly:

  ```sh
  rustup toolchain install stable
  rustup toolchain install 1.89 nightly --profile minimal
  ```

- A C compiler and linker (`cc`), and `git`.
- For the full set of checks: [cargo-deny](https://github.com/EmbarkStudios/cargo-deny),
  [typos](https://github.com/crate-ci/typos) and [mdBook](https://github.com/rust-lang/mdBook).

On NixOS (or anywhere with Nix), `nix develop` gives you a shell with a C
toolchain, git, rustup, cargo-deny, typos, mdBook, hyperfine and jq. It does
not provide Rust itself; use rustup inside the shell as above.

## Checks

CI runs these on every pull request. Run them locally before pushing:

| CI job | Local command |
| --- | --- |
| rustfmt | `cargo fmt --all --check` |
| clippy (Linux, macOS) | `cargo clippy --all-targets --locked -- -D warnings` |
| test (Linux, macOS; stable and 1.89) | `cargo test --locked` and `cargo +1.89 test --locked` |
| test (Linux, nightly; not required) | `cargo +nightly test --locked` |
| test (macos-26, stable, remap-paths) | `FERRY_TEST_REMAP=1 cargo test --locked` |
| rustdoc | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked` |
| cargo-deny | `cargo deny --locked check` |
| typos | `typos` |
| Docs | `mdbook build docs` (or `mdbook serve docs` to preview) |
| Release (`dist plan`) | see [Releases](#releases); only for changes to `dist-workspace.toml` or `Cargo.toml` |

Some environments (CI images, some agent tools) export `CARGO_INCREMENTAL=0`.
Unset it before running the tests: they exercise rustc's incremental reuse,
which that variable switches off. CI does the same.

If you edit a workflow under `.github/workflows/`, also run
[actionlint](https://github.com/rhysd/actionlint) and
[zizmor](https://github.com/zizmorcore/zizmor) on it. Pin every action by full
commit SHA with a `# vX.Y.Z` comment, and give jobs only the permissions they
need.

## Tests and platform capabilities

Much of Ferry depends on what the machine supports, and `ferry doctor` tells
you what yours does:

- **User namespaces and overlayfs (Linux).** Needed by the namespace sandbox
  and the overlay backend. Ubuntu 23.10 and later block them by default; to
  run these tests, lift the restriction until the next reboot with
  `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`. Check with
  `unshare --user --map-root-user --mount true`. The unit tests of `ferry
  setup apparmor` also run its root scripts as root of such a namespace.
- **`ferry setup apparmor` against the real system.** `tests/setup.rs` has
  an ignored round trip that installs, re-runs, uninstalls and reinstalls
  through real `sudo` and AppArmor. It changes the machine, so it runs only
  with `FERRY_TEST_SYSTEM_SETUP=1` on Ubuntu 24.04 or later with the
  restriction on and passwordless `sudo`, as CI's AppArmor job does:
  `FERRY_TEST_SYSTEM_SETUP=1 cargo test --test setup -- --ignored`.
- **Reflinks.** Needed by the reflink backend: btrfs, XFS, bcachefs or
  OpenZFS 2.2+ on Linux, APFS on macOS. `/tmp` is often tmpfs, which has no
  reflinks.

Tests that need a capability the machine lacks skip themselves and say so in
the test output instead of failing, so a green run on a machine without them
proves less than it looks. Check the output for skips before concluding a
change is covered. In CI, the Linux jobs lift the AppArmor restriction and
fail if namespaces still do not work; macOS covers `clonefile` and the
non-namespace path.

`FERRY_TEST_REQUIRE` turns those skips into failures: it lists what must be
available (`userns`, `overlay`, `reflink`, and `remap` for a rustc that
supports [remap-paths](docs/src/remap-paths.md), 1.98 or newer). CI sets it
on every leg. `FERRY_TEST_REMAP=1` gives every ferry command the tests run
`FERRY_REMAP_PATHS=1` (a test's own setting still wins); the `remap-paths`
leg runs the whole suite that way on macOS, where every agent builds without
the namespace sandbox. That leg also runs an ignored lldb test as a
measurement that may fail without failing the job.

The `Bench` workflow (`.github/workflows/bench.yml`) is started by hand and
runs a reduced benchmark on a macOS runner; see `bench/README.md`.

## Commits and pull requests

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):
`feat:`, `fix:`, `perf:`, `refactor:`, `docs:`, `test:`, `ci:`, `build:` and
`chore:`, with an optional scope (`fix(freshness): ...`). Mark breaking changes
with `!` (`feat!: ...`) or a `BREAKING CHANGE:` footer. Pull requests are
squash-merged, so the PR title becomes the commit on `main` and the changelog
entry: make it a Conventional Commit too.

Keep pull requests focused, add tests for new behavior, and document
user-visible changes in `docs/`. Do not edit `CHANGELOG.md`; it is generated.

## Releases

Releases are automated with [release-plz](https://release-plz.dev) and
[dist](https://github.com/axodotdev/cargo-dist):

1. On every push to `main`, release-plz opens or updates a release PR that
   bumps the version in `Cargo.toml` and adds the new section to
   `CHANGELOG.md`, both derived from the Conventional Commits since the last
   release. Before 1.0, a breaking change bumps the minor version and anything
   else bumps the patch version.
2. Merging the release PR makes release-plz publish `ferry-rs` to crates.io
   (trusted publishing) and push the `vX.Y.Z` tag.
3. The tag starts `release.yml`, generated by dist, which builds the Linux
   (gnu and musl) and macOS binaries, creates the GitHub Release with the
   archives, checksums and shell installer, and updates the Homebrew formula
   in `patwrall/homebrew-tap`.

`release.yml` is generated from `dist-workspace.toml`: never edit it by hand.
Change the configuration, install exactly the `cargo-dist-version` it names
(from the [dist releases](https://github.com/axodotdev/cargo-dist/releases);
crates.io lags behind), then run `dist generate` and check the result with
`dist plan`. Action pins for that file live in `[dist.github-action-commits]`,
because Dependabot does not update it.

## MSRV

The minimum supported Rust version is **1.89**, declared as `rust-version` in
`Cargo.toml` and tested in CI on Linux and macOS. Raising it is allowed when a
dependency or a language feature needs it; do it in its own commit, update
`rust-version` and the `1.89` in `.github/workflows/ci.yml` together, and note
it in the PR description. `Cargo.lock` is resolved with the MSRV-aware
resolver, so `cargo update` picks dependency versions that support the MSRV;
the 1.89 test jobs fail if a newer Rust slips in.
