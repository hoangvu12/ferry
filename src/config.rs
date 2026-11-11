//! `ferry.toml` schema and resolution.
//!
//! Precedence, lowest to highest: built-in defaults, `ferry.toml`, its
//! `[linux]`/`[macos]` table for the running platform, `FERRY_*` environment
//! variables.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "ferry.toml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid {}: {message}", path.display())]
    Parse { path: PathBuf, message: String },

    #[error("invalid value {value:?} for {var}: expected one of {expected}")]
    Env {
        var: &'static str,
        value: String,
        expected: &'static str,
    },

    #[error("invalid configuration: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    pub base: BaseConfig,
    pub agent: AgentConfig,
    pub store: StoreConfig,
    pub fork: ForkConfig,
    pub sandbox: SandboxConfig,
    pub freshness: FreshnessConfig,
    pub linux: PlatformOverrides,
    pub macos: PlatformOverrides,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct BaseConfig {
    /// Cargo subcommands that warm the base.
    pub targets: Vec<WarmTarget>,
    /// Cargo profiles to warm.
    pub profiles: Vec<String>,
    /// Extra arguments appended to every generated cargo command.
    pub cargo_args: Vec<String>,
    /// Replaces the generated commands. Each runs with `sh -c`.
    pub commands: Option<Vec<String>>,
    /// Seed refreshes across unit-input changes, deleting the units their
    /// warm commands did not use (see `prune`).
    pub prune: bool,
}

impl Default for BaseConfig {
    fn default() -> Self {
        Self {
            targets: vec![
                WarmTarget::Build,
                WarmTarget::Test,
                WarmTarget::Check,
                WarmTarget::Clippy,
            ],
            profiles: vec!["dev".into()],
            cargo_args: vec!["--workspace".into()],
            commands: None,
            prune: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WarmTarget {
    Check,
    Build,
    Test,
    Clippy,
}

/// A shell command that warms a base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WarmCommand {
    pub line: String,
    /// Generated from the `clippy` target. Clippy is an optional toolchain
    /// component, and lints can fail where a build does not, so the command
    /// is skipped when clippy is not installed and its failure only warns
    /// instead of discarding the base.
    pub clippy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct AgentConfig {
    pub branch_prefix: String,
    /// Parent directory for agent worktrees.
    pub worktree_dir: Option<PathBuf>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            branch_prefix: "ferry/".into(),
            worktree_dir: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct StoreConfig {
    /// Parent directory for per-repository stores.
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ForkConfig {
    pub backend: BackendPref,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    /// Experimental: path-independent incremental state for builds that run
    /// without the namespace sandbox (see `remap`).
    pub remap_paths: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct FreshnessConfig {
    pub mode: FreshnessPref,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct PlatformOverrides {
    pub backend: Option<BackendPref>,
    pub sandbox: Option<SandboxMode>,
    pub freshness: Option<FreshnessPref>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendPref {
    #[default]
    Auto,
    Reflink,
    Overlay,
    Copy,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    #[default]
    Auto,
    Namespace,
    Off,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FreshnessPref {
    #[default]
    Auto,
    Checksum,
    Mtime,
}

/// Fully resolved settings for the running platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Settings {
    /// Parent directory of per-repository stores.
    pub store_parent: Option<PathBuf>,
    pub backend: BackendPref,
    pub sandbox: SandboxMode,
    pub freshness: FreshnessPref,
    pub warm_commands: Vec<WarmCommand>,
    pub branch_prefix: String,
    /// Parent directory of agent worktrees (one subdirectory per repository).
    pub worktree_parent: Option<PathBuf>,
    pub remap_paths: bool,
    pub prune: bool,
    /// The config file that was loaded, if any.
    pub config_path: Option<PathBuf>,
}

impl Config {
    /// Loads `$FERRY_CONFIG` if set, else `<repo_root>/ferry.toml` if it exists,
    /// else defaults. Returns the path that was read.
    pub fn load(
        repo_root: &Path,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(Self, Option<PathBuf>), ConfigError> {
        let path = env("FERRY_CONFIG").map_or_else(|| repo_root.join(FILE_NAME), PathBuf::from);
        if env("FERRY_CONFIG").is_none() && !path.exists() {
            return Ok((Self::default(), None));
        }
        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let config = Self::parse(&text).map_err(|message| ConfigError::Parse {
            path: path.clone(),
            message,
        })?;
        Ok((config, Some(path)))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.base.commands.as_ref().is_some_and(Vec::is_empty) {
            return Err("base.commands must not be empty when set".into());
        }
        if self.base.commands.is_none()
            && (self.base.targets.is_empty() || self.base.profiles.is_empty())
        {
            return Err("base.targets and base.profiles must not be empty".into());
        }
        if self
            .base
            .profiles
            .iter()
            .any(|p| p.is_empty() || p.contains(char::is_whitespace))
        {
            return Err("base.profiles entries must be non-empty profile names".into());
        }
        Ok(())
    }

    /// Applies platform overrides and `FERRY_*` variables. `repo_root` anchors a
    /// relative `store.dir`.
    pub fn resolve(
        &self,
        repo_root: &Path,
        config_path: Option<PathBuf>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Settings, ConfigError> {
        let platform = if cfg!(target_os = "macos") {
            &self.macos
        } else {
            &self.linux
        };
        let mut backend = platform.backend.unwrap_or(self.fork.backend);
        let mut sandbox = platform.sandbox.unwrap_or(self.sandbox.mode);
        let mut freshness = platform.freshness.unwrap_or(self.freshness.mode);
        if let Some(v) = env("FERRY_BACKEND") {
            backend = parse_env("FERRY_BACKEND", &v, "auto, reflink, overlay, copy")?;
        }
        if let Some(v) = env("FERRY_SANDBOX") {
            sandbox = parse_env("FERRY_SANDBOX", &v, "auto, namespace, off")?;
        }
        if let Some(v) = env("FERRY_FRESHNESS") {
            freshness = parse_env("FERRY_FRESHNESS", &v, "auto, checksum, mtime")?;
        }
        let warm_commands = match env("FERRY_BASE_COMMAND") {
            Some(cmd) if !cmd.trim().is_empty() => vec![WarmCommand {
                line: cmd,
                clippy: false,
            }],
            _ => self.warm_commands(),
        };
        let explicit_store = match env("FERRY_HOME").filter(|v| !v.is_empty()) {
            Some(home) => Some(PathBuf::from(home)),
            None => self
                .store
                .dir
                .as_ref()
                .map(|dir| expand_dir(dir, repo_root)),
        };
        // Worktrees hold agents' uncommitted work, so by default they live in
        // the platform data dir rather than next to build state in the cache
        // dir, which macOS may purge and users clear casually.
        let worktree_parent = match env("FERRY_WORKTREES").filter(|v| !v.is_empty()) {
            Some(dir) => Some(PathBuf::from(dir)),
            None => match (&self.agent.worktree_dir, &explicit_store) {
                (Some(dir), _) => Some(expand_dir(dir, repo_root)),
                (None, Some(store)) => Some(store.join("worktrees")),
                (None, None) => crate::store::default_worktree_parent(),
            },
        };
        let store_parent = explicit_store.or_else(crate::store::default_store_parent);
        // Only these variables can make either path relative.
        for (var, dir) in [
            ("FERRY_HOME", &store_parent),
            ("FERRY_WORKTREES", &worktree_parent),
        ] {
            if let Some(p) = dir
                && !p.is_absolute()
            {
                return Err(ConfigError::Invalid(format!(
                    "{var} must be an absolute path, not `{}`",
                    p.display()
                )));
            }
        }
        let remap_paths = parse_bool_env(env, "FERRY_REMAP_PATHS", self.sandbox.remap_paths)?;
        let prune = parse_bool_env(env, "FERRY_BASE_PRUNE", self.base.prune)?;
        Ok(Settings {
            store_parent,
            backend,
            sandbox,
            freshness,
            warm_commands,
            branch_prefix: self.agent.branch_prefix.clone(),
            worktree_parent,
            remap_paths,
            prune,
            config_path,
        })
    }

    /// Shell commands that warm a base, generated from `[base]` unless
    /// `base.commands` overrides them.
    pub fn warm_commands(&self) -> Vec<WarmCommand> {
        if let Some(commands) = &self.base.commands {
            return commands
                .iter()
                .map(|line| WarmCommand {
                    line: line.clone(),
                    clippy: false,
                })
                .collect();
        }
        // Only these reach cargo; the rest go to rustc or clippy-driver.
        let cargo_args: Vec<&str> = self
            .base
            .cargo_args
            .iter()
            .map(String::as_str)
            .take_while(|a| *a != "--")
            .collect();
        let mut out = Vec::new();
        for profile in &self.base.profiles {
            for target in &self.base.targets {
                let mut argv: Vec<&str> = vec!["cargo"];
                argv.extend(target.subcommand());
                // Passing `--profile dev` explicitly would also force test
                // targets onto `dev` instead of `test`, so later `cargo test`
                // runs would not reuse them.
                match profile.as_str() {
                    "dev" => {}
                    "release" => argv.push("--release"),
                    other => argv.extend(["--profile", other]),
                }
                let clippy = *target == WarmTarget::Clippy;
                if clippy {
                    // `--all-targets` also checks tests and their
                    // dev-dependencies; `--keep-going` still warms the rest
                    // when some target fails (e.g. benches that need
                    // nightly). Cargo rejects either flag given twice.
                    if !cargo_args.iter().any(|a| selects_targets(a)) {
                        argv.push("--all-targets");
                    }
                    if !cargo_args.contains(&"--keep-going") {
                        argv.push("--keep-going");
                    }
                }
                argv.extend(self.base.cargo_args.iter().map(String::as_str));
                out.push(WarmCommand {
                    line: argv
                        .iter()
                        .map(|a| shell_quote(a))
                        .collect::<Vec<_>>()
                        .join(" "),
                    clippy,
                });
            }
        }
        out
    }
}

impl WarmTarget {
    fn subcommand(self) -> &'static [&'static str] {
        match self {
            WarmTarget::Check => &["check"],
            WarmTarget::Build => &["build"],
            WarmTarget::Test => &["test", "--no-run"],
            WarmTarget::Clippy => &["clippy"],
        }
    }
}

/// True for cargo's target-selection flags, with or without `=value`.
fn selects_targets(arg: &str) -> bool {
    let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
    matches!(
        flag,
        "--lib"
            | "--bin"
            | "--bins"
            | "--example"
            | "--examples"
            | "--test"
            | "--tests"
            | "--bench"
            | "--benches"
            | "--all-targets"
    )
}

macro_rules! display_as_serde {
    ($($ty:ty),*) => {$(
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let value = serde_json::to_value(self).map_err(|_| fmt::Error)?;
                f.write_str(value.as_str().unwrap_or_default())
            }
        }
    )*};
}
display_as_serde!(BackendPref, SandboxMode, FreshnessPref, WarmTarget);

fn parse_env<T: for<'de> Deserialize<'de>>(
    var: &'static str,
    value: &str,
    expected: &'static str,
) -> Result<T, ConfigError> {
    let de: serde::de::value::StrDeserializer<'_, serde::de::value::Error> =
        value.trim().into_deserializer();
    T::deserialize(de).map_err(|_| ConfigError::Env {
        var,
        value: value.to_owned(),
        expected,
    })
}

fn parse_bool_env(
    env: &dyn Fn(&str) -> Option<String>,
    var: &'static str,
    default: bool,
) -> Result<bool, ConfigError> {
    match env(var).as_deref().map(str::trim) {
        None => Ok(default),
        Some("1" | "true") => Ok(true),
        Some("0" | "false" | "") => Ok(false),
        Some(other) => Err(ConfigError::Env {
            var,
            value: other.to_owned(),
            expected: "1, 0, true, false",
        }),
    }
}

fn expand_dir(dir: &Path, repo_root: &Path) -> PathBuf {
    if let Ok(rest) = dir.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        repo_root.join(dir)
    }
}

/// Quotes `arg` for `sh -c` only when it needs it, so generated commands stay
/// readable in output.
pub fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// `dir` quoted for a `PATH` entry in `sh`. On Windows that `sh` is Git
/// Bash, whose `PATH` holds MSYS paths (`/c/Users/...`): a drive path is
/// rewritten to one, since its `:` would split the entry.
pub fn sh_path(dir: &Path) -> String {
    let text = dir.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    if cfg!(windows)
        && let [drive, b':', rest @ ..] = text.as_bytes()
        && drive.is_ascii_alphabetic()
    {
        let rest = String::from_utf8_lossy(rest).replace('\\', "/");
        return shell_quote(&format!("/{}{rest}", drive.to_ascii_lowercase() as char));
    }
    shell_quote(text)
}

/// Splits `line` into words as `sh` would, if it is only words: whitespace
/// separates them, single quotes and double quotes group, and a backslash
/// escapes the next character outside single quotes. `None` for anything
/// else a shell would interpret (operators, expansions, globs, comments, a
/// leading variable assignment), so the caller runs such a line with a
/// shell. Reverses [`shell_quote`].
pub fn split_words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => words.extend(word.take()),
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => w.push(c),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\' | '$' | '`') => w.push(c),
                            c => {
                                w.push('\\');
                                w.push(c);
                            }
                        },
                        '$' | '`' => return None,
                        c => w.push(c),
                    }
                }
            }
            '\\' => word.get_or_insert_with(String::new).push(chars.next()?),
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '$' | '`' | '*' | '?' | '[' | ']' | '{'
            | '}' | '~' | '#' | '\n' | '\r' => return None,
            '=' if words.is_empty() => return None,
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    Some(words)
}

/// Written by `ferry init`.
pub const TEMPLATE: &str = r#"# Ferry configuration: https://patwrall.github.io/ferry/configuration.html

[base]
# Cargo subcommands that warm the base: "check", "build", "test" (--no-run),
# "clippy" (--all-targets --keep-going; skipped if not installed, failures only warn).
targets = ["build", "test", "check", "clippy"]
# Cargo profiles to warm. "dev" uses Cargo's default profile selection.
profiles = ["dev"]
# Extra arguments for every generated command.
cargo-args = ["--workspace"]
# Replace the generated commands entirely; each entry runs with `sh -c`.
# commands = ["cargo build --workspace --all-targets --locked"]
# Seed refreshes across manifest, lockfile and Cargo config changes, deleting the
# units the warm commands no longer use; false builds such refreshes from scratch.
prune = true

[agent]
# Branch created for each agent: <prefix><name>.
branch-prefix = "ferry/"
# Where agent worktrees (and their uncommitted work) live, one subdirectory per repository.
# Default: $XDG_DATA_HOME/ferry/worktrees or ~/.local/share/ferry/worktrees (Linux),
# ~/Library/Application Support/ferry/worktrees (macOS).
# worktree-dir = "~/agents"

[store]
# Parent directory for Ferry's per-repository build state (bases, forks).
# Default: $XDG_CACHE_HOME/ferry or ~/.cache/ferry (Linux), ~/Library/Caches/ferry (macOS).
# dir = "/fast/disk/ferry"

[fork]
# How an agent's target dir is forked from the base: "auto", "reflink", "overlay", "copy".
backend = "auto"

[sandbox]
# "auto" uses a private mount namespace when available (Linux), "namespace" requires it, "off" never uses it.
mode = "auto"
# Experimental: keep rustc's incremental state reusable when building without the
# namespace sandbox (macOS; Linux without user namespaces). Needs rustc 1.98+.
remap-paths = false

[freshness]
# "auto" uses checksum freshness when the toolchain supports it, otherwise mtime stamping.
mode = "auto"

# Per-platform overrides of backend, sandbox and freshness.
# [linux]
# sandbox = "namespace"
# [macos]
# backend = "reflink"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn template_parses_to_defaults() {
        assert_eq!(Config::parse(TEMPLATE).unwrap(), Config::default());
    }

    fn lines(commands: &[WarmCommand]) -> Vec<&str> {
        commands.iter().map(|c| c.line.as_str()).collect()
    }

    #[test]
    fn default_warm_command() {
        let commands = Config::default().warm_commands();
        assert_eq!(
            lines(&commands),
            vec![
                "cargo build --workspace",
                "cargo test --no-run --workspace",
                "cargo check --workspace",
                "cargo clippy --all-targets --keep-going --workspace",
            ]
        );
        let clippy: Vec<bool> = commands.iter().map(|c| c.clippy).collect();
        assert_eq!(clippy, [false, false, false, true]);
    }

    #[test]
    fn clippy_target_selection_is_not_duplicated() {
        let clippy_with = |args: &str| {
            let toml = format!("[base]\ntargets = [\"clippy\"]\ncargo-args = {args}\n");
            Config::parse(&toml).unwrap().warm_commands()[0]
                .line
                .clone()
        };
        assert_eq!(
            clippy_with(r#"["--workspace", "--all-targets", "--locked"]"#),
            "cargo clippy --keep-going --workspace --all-targets --locked"
        );
        assert_eq!(
            clippy_with(r#"["--lib"]"#),
            "cargo clippy --keep-going --lib"
        );
        assert_eq!(
            clippy_with(r#"["--bin=app"]"#),
            "cargo clippy --keep-going --bin=app"
        );
        assert_eq!(
            clippy_with(r#"["--keep-going"]"#),
            "cargo clippy --all-targets --keep-going"
        );
        // Arguments after `--` go to clippy-driver, not to cargo.
        assert_eq!(
            clippy_with(r#"["--", "--test"]"#),
            "cargo clippy --all-targets --keep-going -- --test"
        );
    }

    #[test]
    fn explicit_commands_are_never_optional() {
        let config = Config::parse("[base]\ncommands = [\"cargo clippy\"]\n").unwrap();
        assert!(config.warm_commands().iter().all(|c| !c.clippy));
        let env = |k: &str| (k == "FERRY_BASE_COMMAND").then(|| "cargo clippy".to_string());
        let s = Config::default()
            .resolve(Path::new("/r"), None, &env)
            .unwrap();
        assert!(s.warm_commands.iter().all(|c| !c.clippy));
    }

    #[test]
    fn generated_commands_cover_profiles_and_targets() {
        let config = Config::parse(
            r#"
            [base]
            targets = ["check", "test", "clippy"]
            profiles = ["dev", "release", "ci fast"]
            cargo-args = ["--features", "a b"]
            "#,
        );
        assert!(config.is_err(), "profile names with spaces are rejected");

        let config = Config::parse(
            r#"
            [base]
            targets = ["test", "clippy"]
            profiles = ["release", "ci"]
            cargo-args = ["--features", "a b"]
            "#,
        )
        .unwrap();
        assert_eq!(
            lines(&config.warm_commands()),
            vec![
                "cargo test --no-run --release --features 'a b'",
                "cargo clippy --release --all-targets --keep-going --features 'a b'",
                "cargo test --no-run --profile ci --features 'a b'",
                "cargo clippy --profile ci --all-targets --keep-going --features 'a b'",
            ]
        );
    }

    #[test]
    fn unknown_keys_are_errors() {
        let err = Config::parse("[fork]\nbackend = \"auto\"\nbakend = 1\n").unwrap_err();
        assert!(err.contains("bakend"), "{err}");
        assert!(Config::parse("[fork]\nbackend = \"hardlink\"\n").is_err());
    }

    #[test]
    fn precedence_platform_then_env() {
        let config = Config::parse(
            r#"
            [fork]
            backend = "copy"
            [linux]
            backend = "overlay"
            [macos]
            backend = "reflink"
            "#,
        )
        .unwrap();
        let root = Path::new("/repo");
        let s = config.resolve(root, None, &no_env).unwrap();
        let expected = if cfg!(target_os = "macos") {
            BackendPref::Reflink
        } else {
            BackendPref::Overlay
        };
        assert_eq!(s.backend, expected);

        let env = |k: &str| (k == "FERRY_BACKEND").then(|| "copy".to_string());
        assert_eq!(
            config.resolve(root, None, &env).unwrap().backend,
            BackendPref::Copy
        );

        let bad = |k: &str| (k == "FERRY_SANDBOX").then(|| "maybe".to_string());
        let err = config.resolve(root, None, &bad).unwrap_err().to_string();
        assert!(
            err.contains("FERRY_SANDBOX") && err.contains("namespace"),
            "{err}"
        );
    }

    #[test]
    fn store_dir_resolution() {
        let root = Path::new("/repo");
        let env = |k: &str| (k == "FERRY_HOME").then(|| "/custom".to_string());
        let s = Config::default().resolve(root, None, &env).unwrap();
        assert_eq!(s.store_parent, Some(PathBuf::from("/custom")));

        let config = Config::parse("[store]\ndir = \".ferry-store\"\n").unwrap();
        let s = config.resolve(root, None, &no_env).unwrap();
        assert_eq!(s.store_parent, Some(PathBuf::from("/repo/.ferry-store")));

        let rel = |k: &str| (k == "FERRY_HOME").then(|| "relative".to_string());
        assert!(Config::default().resolve(root, None, &rel).is_err());
    }

    #[test]
    fn base_command_env_overrides_everything() {
        let env = |k: &str| (k == "FERRY_BASE_COMMAND").then(|| "cargo check".to_string());
        let s = Config::default()
            .resolve(Path::new("/r"), None, &env)
            .unwrap();
        assert_eq!(lines(&s.warm_commands), vec!["cargo check"]);
    }

    #[test]
    fn prune_env_overrides_the_file() {
        let config = Config::parse("[base]\nprune = false\n").unwrap();
        let root = Path::new("/r");
        assert!(!config.resolve(root, None, &no_env).unwrap().prune);
        let on = |k: &str| (k == "FERRY_BASE_PRUNE").then(|| "1".to_string());
        assert!(config.resolve(root, None, &on).unwrap().prune);
        let bad = |k: &str| (k == "FERRY_BASE_PRUNE").then(|| "yes".to_string());
        assert!(config.resolve(root, None, &bad).is_err());
        assert!(
            Config::default()
                .resolve(root, None, &no_env)
                .unwrap()
                .prune
        );
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("--workspace"), "--workspace");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
