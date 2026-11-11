//! Agents without the namespace sandbox build at other absolute paths than
//! their base. Each scenario gives the fixture one way of capturing a path,
//! then checks that exactly the units that captured the base's paths
//! rebuild, that the artifacts behave as if built at the agent's paths
//! (run directly, so a second cargo call cannot hide a one-build lag), and
//! that a second build rebuilds nothing.

#![allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "helpers outside #[test] functions fail the test by panicking"
)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Build, Snapshot, TestRepo, path_of, str_of};

fn sandbox_off() -> TestRepo {
    TestRepo::with_env(&[("FERRY_SANDBOX", "off")])
}

fn put(repo: &TestRepo, rel: &str, contents: &str) {
    let path = repo.root().join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    repo.write(&path, contents);
}

fn edit(repo: &TestRepo, rel: &str, change: impl FnOnce(String) -> String) {
    let path = repo.root().join(rel);
    let old = fs::read_to_string(&path).unwrap();
    repo.write(&path, &change(old));
}

/// Makes `app` print one more line, first.
fn app_prints(repo: &TestRepo, statement: &str) {
    edit(repo, "app/src/main.rs", |main| {
        main.replacen("fn main() {", &format!("fn main() {{\n    {statement}"), 1)
    });
}

fn add_dependency(repo: &TestRepo, package: &str, dependency: &str) {
    edit(repo, &format!("{package}/Cargo.toml"), |manifest| {
        if manifest.contains("[dependencies]") {
            manifest.replacen(
                "[dependencies]",
                &format!("[dependencies]\n{dependency}"),
                1,
            )
        } else {
            format!("{manifest}\n[dependencies]\n{dependency}\n")
        }
    });
}

/// Adds the workspace member `name` with `files`; `manifest` is appended
/// to its `[package]` table.
fn add_member(repo: &TestRepo, name: &str, manifest: &str, files: &[(&str, &str)]) {
    put(
        repo,
        &format!("{name}/Cargo.toml"),
        &format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n{manifest}"
        ),
    );
    for (rel, contents) in files {
        put(repo, &format!("{name}/{rel}"), contents);
    }
    edit(repo, "Cargo.toml", |ws| {
        ws.replacen("\"embed\"]", &format!("\"embed\", \"{name}\"]"), 1)
    });
}

/// A member `app` depends on.
fn add_app_dependency(repo: &TestRepo, name: &str, manifest: &str, files: &[(&str, &str)]) {
    add_member(repo, name, manifest, files);
    add_dependency(repo, "app", &format!("{name} = {{ path = \"../{name}\" }}"));
}

/// Commits the fixture with an up-to-date lockfile and builds the base.
fn commit_and_base(repo: &TestRepo, message: &str) -> String {
    lock(repo, repo.root());
    repo.commit_all(message);
    repo.base()
}

fn lock(repo: &TestRepo, dir: &Path) {
    common::run(repo.cargo_in(dir).args(["generate-lockfile", "--offline"])).ok();
}

/// Runs an executable directly and returns its stdout.
fn run_exe(repo: &TestRepo, exe: &Path, args: &[&str]) -> String {
    common::run(repo.command(exe).args(args)).ok().stdout
}

/// Runs the agent's `app` binary straight from its target dir.
fn app_output(repo: &TestRepo, agent: &str) -> String {
    run_exe(repo, &repo.agent_target(agent).join("debug/app"), &[])
}

#[track_caller]
fn field(output: &str, key: &str) -> String {
    let prefix = format!("{key}=");
    output
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no `{key}=` line in:\n{output}"))
        .to_owned()
}

fn cargo_test_no_run(repo: &TestRepo, agent: &str, package: &str) -> Build {
    Build::run(
        repo.agent_cargo(agent)
            .args(["test", "--no-run", "-p", package]),
    )
}

fn rebuilt_targets(build: &Build) -> Vec<(&str, &str)> {
    let mut targets: Vec<_> = build
        .rebuilt
        .iter()
        .map(|u| (u.package.as_str(), u.target.as_str()))
        .collect();
    targets.sort_unstable();
    targets.dedup();
    targets
}

/// The minor version of the cargo the tests run.
fn cargo_minor() -> u32 {
    let out = std::process::Command::new("cargo")
        .arg("-V")
        .output()
        .unwrap();
    let version = String::from_utf8(out.stdout).unwrap();
    version
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.split('.').nth(1))
        .and_then(|m| m.parse().ok())
        .unwrap()
}

const EMBED_RECORDS_DIR: &str = "fn main() {\n    let dir = std::env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n    \
     let out = std::env::var(\"OUT_DIR\").unwrap();\n    \
     fs::write(format!(\"{out}/dir.rs\"), format!(\"pub const DIR: &str = {dir:?};\")).unwrap();";

fn embed_records_its_dir(repo: &TestRepo) {
    edit(repo, "embed/build.rs", |b| {
        b.replacen("fn main() {", EMBED_RECORDS_DIR, 1)
    });
    edit(repo, "embed/src/lib.rs", |lib| {
        format!("{lib}include!(concat!(env!(\"OUT_DIR\"), \"/dir.rs\"));\n")
    });
    app_prints(repo, "println!(\"dir={}\", embed::DIR);");
}

/// `rt` reads a file its build script copied into `OUT_DIR`, at run time.
fn add_runtime_reader(repo: &TestRepo) {
    add_app_dependency(
        repo,
        "rt",
        "",
        &[
            (
                "build.rs",
                "fn main() {\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    \
                 std::fs::copy(\"data.in\", format!(\"{out}/data.txt\")).unwrap();\n    \
                 println!(\"cargo::rerun-if-changed=data.in\");\n}\n",
            ),
            ("data.in", "one\n"),
            (
                "src/lib.rs",
                "pub fn data() -> String {\n    \
                 std::fs::read_to_string(concat!(env!(\"OUT_DIR\"), \"/data.txt\"))\n        \
                 .unwrap()\n        .trim()\n        .to_owned()\n}\n",
            ),
        ],
    );
    app_prints(repo, "println!(\"data={}\", rt::data());");
}

/// `assets` includes generated code that includes a generated file by its
/// absolute path.
fn add_generated_include(repo: &TestRepo) {
    add_app_dependency(
        repo,
        "assets",
        "",
        &[
            (
                "build.rs",
                "fn main() {\n    let out = std::path::PathBuf::from(std::env::var(\"OUT_DIR\").unwrap());\n    \
                 let logo = out.join(\"logo.bin\");\n    std::fs::write(&logo, [7u8; 42]).unwrap();\n    \
                 let code = format!(\"pub static LOGO: &[u8] = include_bytes!({:?});\", logo.to_str().unwrap());\n    \
                 std::fs::write(out.join(\"gen.rs\"), code).unwrap();\n    \
                 println!(\"cargo::rerun-if-changed=build.rs\");\n}\n",
            ),
            (
                "src/lib.rs",
                "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n",
            ),
        ],
    );
    app_prints(repo, "println!(\"logo={}\", assets::LOGO.len());");
}

const MANIFEST_DIR_MACRO: &str = "use proc_macro::TokenStream;\n\n#[proc_macro]\n\
     pub fn manifest_dir(_: TokenStream) -> TokenStream {\n    \
     format!(\"{:?}\", std::env::var(\"CARGO_MANIFEST_DIR\").unwrap())\n        .parse()\n        .unwrap()\n}\n";

/// `pmext` (a proc macro reading `CARGO_MANIFEST_DIR`) and `extlib` (which
/// uses it for its own constant) live outside the repository; `app` uses
/// both.
fn add_external_macro(repo: &TestRepo) -> PathBuf {
    let ext = repo.scratch().join("ext");
    for (rel, contents) in [
        (
            "pmext/Cargo.toml",
            "[package]\nname = \"pmext\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n",
        ),
        ("pmext/src/lib.rs", MANIFEST_DIR_MACRO),
        (
            "extlib/Cargo.toml",
            "[package]\nname = \"extlib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
             [dependencies]\npmext = { path = \"../pmext\" }\n",
        ),
        (
            "extlib/src/lib.rs",
            "pub const DIR: &str = pmext::manifest_dir!();\n",
        ),
    ] {
        let path = ext.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    for name in ["pmext", "extlib"] {
        let dir = ext.join(name).display().to_string();
        add_dependency(repo, "app", &format!("{name} = {{ path = {dir:?} }}"));
    }
    app_prints(
        repo,
        "println!(\"ext_app={}\\next_lib={}\", pmext::manifest_dir!(), extlib::DIR);",
    );
    ext
}

/// I2: a build script passes its manifest dir to the library through
/// `rustc-env`.
#[test]
fn a_build_script_env_value_naming_the_source_is_rerun() {
    let repo = sandbox_off();
    edit(&repo, "embed/build.rs", |b| {
        b.replacen(
            "fn main() {",
            "fn main() {\n    println!(\"cargo::rustc-env=EMBED_DIR={}\", std::env::var(\"CARGO_MANIFEST_DIR\").unwrap());",
            1,
        )
    });
    edit(&repo, "embed/src/lib.rs", |lib| {
        format!("{lib}pub const DIR: &str = env!(\"EMBED_DIR\");\n")
    });
    app_prints(&repo, "println!(\"dir={}\", embed::DIR);");
    commit_and_base(&repo, "embed passes its dir");
    let worktree = repo.spawn("a");

    repo.build("a").assert_rebuilt(&["embed", "app"]);
    let dir = field(&app_output(&repo, "a"), "dir");
    assert_eq!(Path::new(&dir), worktree.join("embed"));
    repo.build("a").assert_rebuilt(&[]);
}

/// I3: a build script watches a file outside its package by absolute path
/// and passes its contents on.
#[test]
fn an_absolute_rerun_if_changed_path_follows_the_agent() {
    let repo = sandbox_off();
    put(&repo, "shared.txt", "v1\n");
    edit(&repo, "embed/build.rs", |b| {
        b.replacen(
            "fn main() {",
            "fn main() {\n    let dir = std::env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n    \
             let shared = Path::new(&dir).parent().unwrap().join(\"shared.txt\");\n    \
             println!(\"cargo::rerun-if-changed={}\", shared.display());\n    \
             println!(\"cargo::rustc-env=SHARED={}\", fs::read_to_string(&shared).unwrap().trim());",
            1,
        )
    });
    edit(&repo, "embed/src/lib.rs", |lib| {
        format!("{lib}pub const SHARED: &str = env!(\"SHARED\");\n")
    });
    app_prints(&repo, "println!(\"shared={}\", embed::SHARED);");
    commit_and_base(&repo, "embed watches shared.txt");
    let worktree = repo.spawn("a");

    repo.build("a").assert_rebuilt(&["embed", "app"]);
    assert_eq!(field(&app_output(&repo, "a"), "shared"), "v1");
    repo.write(&worktree.join("shared.txt"), "v2\n");
    repo.build("a").assert_rebuilt(&["embed", "app"]);
    assert_eq!(field(&app_output(&repo, "a"), "shared"), "v2");
    repo.build("a").assert_rebuilt(&[]);
}

/// I4: a local proc macro reads `CARGO_MANIFEST_DIR` through `std::env`,
/// and `app` reaches it through a facade crate.
#[test]
fn users_of_a_proc_macro_reading_the_manifest_dir_rebuild() {
    let repo = sandbox_off();
    add_member(
        &repo,
        "pm",
        "\n[lib]\nproc-macro = true\n",
        &[("src/lib.rs", MANIFEST_DIR_MACRO)],
    );
    add_app_dependency(
        &repo,
        "pmf",
        "\n[dependencies]\npm = { path = \"../pm\" }\n",
        &[("src/lib.rs", "pub use pm::manifest_dir;\n")],
    );
    app_prints(&repo, "println!(\"pm={}\", pmf::manifest_dir!());");
    commit_and_base(&repo, "a proc macro reading the manifest dir");
    let worktree = repo.spawn("a");

    repo.build("a").assert_rebuilt(&["pmf", "app"]);
    let dir = field(&app_output(&repo, "a"), "pm");
    assert_eq!(Path::new(&dir), worktree.join("app"));
    repo.build("a").assert_rebuilt(&[]);
}

/// I5: a local proc macro embeds a path derived from its working directory;
/// a literal-free local derive must not cost its users a rebuild.
#[test]
fn users_of_a_local_proc_macro_using_its_cwd_rebuild() {
    let repo = sandbox_off();
    add_app_dependency(
        &repo,
        "pmcwd",
        "\n[lib]\nproc-macro = true\n",
        &[(
            "src/lib.rs",
            "use proc_macro::TokenStream;\n\n#[proc_macro]\n\
             pub fn data_dir(_: TokenStream) -> TokenStream {\n    \
             let dir = std::env::current_dir().unwrap().join(\"data\");\n    \
             format!(\"{:?}\", dir.to_str().unwrap()).parse().unwrap()\n}\n",
        )],
    );
    add_member(
        &repo,
        "pmplain",
        "\n[lib]\nproc-macro = true\n",
        &[(
            "src/lib.rs",
            "use proc_macro::TokenStream;\n\n#[proc_macro_derive(Plain)]\n\
             pub fn plain(_: TokenStream) -> TokenStream {\n    TokenStream::new()\n}\n",
        )],
    );
    add_dependency(&repo, "mid", "pmplain = { path = \"../pmplain\" }");
    edit(&repo, "mid/src/lib.rs", |lib| {
        format!("{lib}\n#[derive(pmplain::Plain)]\npub struct Plain;\n")
    });
    app_prints(&repo, "println!(\"cwd={}\", pmcwd::data_dir!());");
    commit_and_base(&repo, "local proc macros");
    let worktree = repo.spawn("a");

    repo.build("a").assert_rebuilt(&["app"]);
    let dir = field(&app_output(&repo, "a"), "cwd");
    assert_eq!(Path::new(&dir), worktree.join("data"));
    repo.build("a").assert_rebuilt(&[]);
}

/// I6: a proc macro outside the repository reads `CARGO_MANIFEST_DIR`.
/// Registry-like crates see the same value in base and agent, so only the
/// local user rebuilds.
#[test]
fn only_local_users_of_an_external_proc_macro_rebuild() {
    let repo = sandbox_off();
    let ext = add_external_macro(&repo);
    commit_and_base(&repo, "an external proc macro");
    let worktree = repo.spawn("a");

    repo.build("a").assert_rebuilt(&["app"]);
    let out = app_output(&repo, "a");
    assert_eq!(Path::new(&field(&out, "ext_app")), worktree.join("app"));
    assert_eq!(Path::new(&field(&out, "ext_lib")), ext.join("extlib"));
    repo.build("a").assert_rebuilt(&[]);
}

const BIN_TEST: &str = "#[test]\nfn runs_the_binary() {\n    \
     let out = std::process::Command::new(env!(\"CARGO_BIN_EXE_app\")).output().unwrap();\n    \
     let first = String::from_utf8(out.stdout).unwrap();\n    \
     println!(\"binary-said={}\", first.lines().next().unwrap_or_default());\n}\n";

/// I7: an integration test runs the package's binary through
/// `env!("CARGO_BIN_EXE_app")`, and binaries are not part of a test's
/// fingerprint.
#[test]
fn an_integration_test_runs_the_agents_binary() {
    let repo = sandbox_off();
    put(&repo, "app/tests/bin.rs", BIN_TEST);
    commit_and_base(&repo, "a test of the binary");
    let worktree = repo.spawn("a");

    repo.write(
        &worktree.join("app/src/main.rs"),
        "fn main() {\n    println!(\"marker=EDITED-BIN\");\n    let _ = (mid::marker(), embed::ASSETS);\n}\n",
    );
    let built = cargo_test_no_run(&repo, "a", "app");
    built.assert_rebuilt(&["app"]);
    assert!(
        rebuilt_targets(&built).contains(&("app", "bin")),
        "{:?}",
        built.rebuilt
    );
    let out = run_exe(&repo, built.executable("bin"), &["--nocapture"]);
    assert_eq!(field(&out, "binary-said"), "marker=EDITED-BIN", "{out}");
    cargo_test_no_run(&repo, "a", "app").assert_rebuilt(&[]);
}

/// I8: a test writes into `env!("CARGO_TARGET_TMPDIR")`, which must be the
/// agent's, never the immutable base's.
#[test]
fn a_test_writes_into_the_agents_tmpdir() {
    let repo = sandbox_off();
    put(
        &repo,
        "side/tests/tmp.rs",
        "#[test]\nfn writes_the_tmpdir() {\n    \
         std::fs::write(concat!(env!(\"CARGO_TARGET_TMPDIR\"), \"/m\"), \"x\").unwrap();\n}\n",
    );
    let base = commit_and_base(&repo, "a test using the tmpdir");
    let before = Snapshot::of(&repo.base_dir(&base));
    repo.spawn("a");

    let built = cargo_test_no_run(&repo, "a", "side");
    assert_eq!(rebuilt_targets(&built), [("side", "tmp")], "{}", built.log);
    run_exe(&repo, built.executable("tmp"), &[]);
    assert!(repo.agent_target("a").join("tmp/m").is_file());
    before.assert_unchanged(&Snapshot::of(&repo.base_dir(&base)));
    cargo_test_no_run(&repo, "a", "side").assert_rebuilt(&[]);
}

/// I9: a library reads its `OUT_DIR` at run time. The base's copy stays
/// identical to the agent's until the build script reruns, and then the
/// library rebuilds.
#[test]
fn reading_the_out_dir_at_run_time_needs_no_rebuild() {
    let repo = sandbox_off();
    add_runtime_reader(&repo);
    commit_and_base(&repo, "a run-time reader");
    let worktree = repo.spawn("a");

    repo.build("a").assert_rebuilt(&[]);
    assert_eq!(field(&app_output(&repo, "a"), "data"), "one");
    repo.write(&worktree.join("rt/data.in"), "two\n");
    repo.build("a").assert_rebuilt(&["rt", "app"]);
    assert_eq!(field(&app_output(&repo, "a"), "data"), "two");
    repo.build("a").assert_rebuilt(&[]);
}

/// I10: a build script writes a target path outside its own `OUT_DIR` into
/// it, which the library reads at run time.
#[test]
fn out_dir_data_naming_the_profile_dir_is_rerun() {
    let repo = sandbox_off();
    add_app_dependency(
        &repo,
        "prof",
        "",
        &[
            (
                "build.rs",
                "fn main() {\n    let out = std::path::PathBuf::from(std::env::var(\"OUT_DIR\").unwrap());\n    \
                 let build = out.ancestors().find(|d| d.file_name().is_some_and(|n| n == \"build\")).unwrap();\n    \
                 std::fs::write(out.join(\"profile.txt\"), build.parent().unwrap().to_str().unwrap()).unwrap();\n    \
                 println!(\"cargo::rerun-if-changed=build.rs\");\n}\n",
            ),
            (
                "src/lib.rs",
                "pub fn profile() -> String {\n    \
                 std::fs::read_to_string(concat!(env!(\"OUT_DIR\"), \"/profile.txt\")).unwrap()\n}\n",
            ),
        ],
    );
    app_prints(&repo, "println!(\"profile={}\", prof::profile());");
    commit_and_base(&repo, "a profile dir in OUT_DIR");
    repo.spawn("a");

    repo.build("a").assert_rebuilt(&["prof", "app"]);
    let profile = field(&app_output(&repo, "a"), "profile");
    // In remap mode the path goes through the worktree's `.ferry-target`.
    assert_eq!(
        ferry::sys::canonicalize(&profile).unwrap(),
        ferry::sys::canonicalize(repo.agent_target("a").join("debug")).unwrap()
    );
    repo.build("a").assert_rebuilt(&[]);
}

/// I11: generated code includes a generated file by its absolute path in the
/// base's `OUT_DIR`, which is alive and identical.
#[test]
fn generated_includes_of_the_out_dir_need_no_rebuild() {
    let repo = sandbox_off();
    add_generated_include(&repo);
    commit_and_base(&repo, "a generated include");
    let worktree = repo.spawn("a");

    repo.build("a").assert_rebuilt(&[]);
    assert_eq!(field(&app_output(&repo, "a"), "logo"), "42");
    repo.write(
        &worktree.join("assets/src/lib.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\npub fn extra() {}\n",
    );
    repo.build("a").assert_rebuilt(&["assets", "app"]);
    assert_eq!(field(&app_output(&repo, "a"), "logo"), "42");
    repo.build("a").assert_rebuilt(&[]);
}

/// I12: a nested workspace keeps its target dir, and only its unit that
/// captured the base's path rebuilds.
#[test]
fn nested_workspaces_keep_their_reuse() {
    let repo = TestRepo::with_env(&[
        ("FERRY_SANDBOX", "off"),
        (
            "FERRY_BASE_COMMAND",
            "cargo build --workspace && cargo build --manifest-path tools/Cargo.toml",
        ),
    ]);
    put(
        &repo,
        "tools/Cargo.toml",
        "[workspace]\nmembers = [\"plain\", \"cap\"]\nresolver = \"2\"\n\n[profile.dev]\ndebug = false\n",
    );
    put(
        &repo,
        "tools/plain/Cargo.toml",
        "[package]\nname = \"plain\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    put(
        &repo,
        "tools/plain/src/lib.rs",
        "pub fn name() -> &'static str {\n    \"plain\"\n}\n",
    );
    put(
        &repo,
        "tools/cap/Cargo.toml",
        "[package]\nname = \"cap\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nplain = { path = \"../plain\" }\n",
    );
    put(
        &repo,
        "tools/cap/build.rs",
        &format!("use std::fs;\n\n{EMBED_RECORDS_DIR}\n}}\n"),
    );
    put(
        &repo,
        "tools/cap/src/main.rs",
        "include!(concat!(env!(\"OUT_DIR\"), \"/dir.rs\"));\n\n\
         fn main() {\n    println!(\"dir={}\\nplain={}\", DIR, plain::name());\n}\n",
    );
    lock(&repo, &repo.root().join("tools"));
    commit_and_base(&repo, "a nested workspace");
    let worktree = repo.spawn("a");
    let nested = repo.agent_target("a").join("ferry-ws/tools");
    assert!(nested.join("debug").is_dir(), "{}", nested.display());

    let tools = |repo: &TestRepo| {
        Build::run(
            repo.agent_cargo("a")
                .args(["build", "--manifest-path", "tools/Cargo.toml"]),
        )
    };
    tools(&repo).assert_rebuilt(&["cap"]);
    let out = run_exe(&repo, &nested.join("debug/cap"), &[]);
    assert_eq!(Path::new(&field(&out, "dir")), worktree.join("tools/cap"));
    assert_eq!(field(&out, "plain"), "plain");
    tools(&repo).assert_rebuilt(&[]);
}

/// I13: a seeded refresh relocates the seed's units, so the new base and its
/// agents keep working after gc collects the seed.
#[test]
fn a_seeded_base_survives_its_seed() {
    let repo = TestRepo::with_env(&[
        ("FERRY_SANDBOX", "off"),
        (
            "FERRY_BASE_COMMAND",
            "cargo build --workspace --message-format=json",
        ),
    ]);
    embed_records_its_dir(&repo);
    add_runtime_reader(&repo);
    add_generated_include(&repo);
    let first = commit_and_base(&repo, "every kind of OUT_DIR use");
    edit(&repo, "side/src/lib.rs", |lib| {
        format!("{lib}\npub fn more() {{}}\n")
    });
    repo.commit_all("touch side");
    let refresh = common::run(&mut repo.ferry_cmd(["--json", "base"])).ok();
    let report = refresh.json();
    assert_eq!(report["base"]["seeded_from"], first.as_str(), "{report}");
    let second = str_of(&report["base"]["id"]).to_owned();
    let warm: Vec<String> = refresh
        .stderr
        .lines()
        .filter(|l| l.starts_with('{'))
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|m| m["reason"] == "compiler-artifact" && m["fresh"] == false)
        .map(|m| common::package_name(str_of(&m["package_id"])).to_owned())
        .collect();
    for package in &warm {
        assert!(
            ["embed", "rt", "assets", "side", "app"].contains(&package.as_str()),
            "the refresh rebuilt {package}: {warm:?}"
        );
    }
    assert!(warm.iter().any(|p| p == "side"), "{warm:?}");

    let gc = repo.ferry_json(["gc"]);
    assert_eq!(gc["removed"], serde_json::json!([first]), "{gc}");
    assert!(!repo.base_dir(&first).exists());
    let worktree = repo.spawn("a");
    assert_eq!(
        repo.ferry_json(["list"])["agents"][0]["base"],
        second.as_str()
    );

    repo.build("a").assert_rebuilt(&["embed", "app"]);
    let out = app_output(&repo, "a");
    assert_eq!(Path::new(&field(&out, "dir")), worktree.join("embed"));
    assert_eq!(field(&out, "data"), "one");
    assert_eq!(field(&out, "logo"), "42");
    repo.build("a").assert_rebuilt(&[]);
    repo.write(
        &worktree.join("assets/src/lib.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\npub fn extra() {}\n",
    );
    repo.build("a").assert_rebuilt(&["assets", "app"]);
    repo.write(&worktree.join("rt/data.in"), "two\n");
    repo.build("a").assert_rebuilt(&["rt", "app"]);
    assert_eq!(field(&app_output(&repo, "a"), "data"), "two");
}

/// I14: a base built in the namespace sandbox recorded the canonical target,
/// an empty mountpoint on the host, so nothing may keep reading it.
#[test]
fn an_agent_without_the_sandbox_stops_reading_the_canonical_target() {
    let repo = TestRepo::with_env(&[("FERRY_SANDBOX", "namespace")]);
    if !repo.require_userns() {
        return;
    }
    let ext = add_external_macro(&repo);
    add_runtime_reader(&repo);
    add_generated_include(&repo);
    commit_and_base(&repo, "OUT_DIR uses and an external proc macro");
    let spawned = common::run(
        repo.ferry_cmd(["--json", "spawn", "a"])
            .env("FERRY_SANDBOX", "off"),
    )
    .ok()
    .json();
    assert_eq!(spawned["agent"]["isolation"], "none", "{spawned}");
    let worktree = path_of(&spawned["agent"]["worktree"]);

    repo.build("a")
        .assert_rebuilt(&["rt", "assets", "extlib", "app"]);
    let out = app_output(&repo, "a");
    assert_eq!(field(&out, "data"), "one");
    assert_eq!(field(&out, "logo"), "42");
    assert_eq!(Path::new(&field(&out, "ext_app")), worktree.join("app"));
    assert_eq!(Path::new(&field(&out, "ext_lib")), ext.join("extlib"));
    repo.build("a").assert_rebuilt(&[]);
    repo.write(
        &worktree.join("assets/src/lib.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\npub fn extra() {}\n",
    );
    repo.build("a").assert_rebuilt(&["assets", "app"]);
}

/// I15: a build script watches the base checkout's own git dir, which is
/// outside the checkout.
#[test]
fn a_build_script_watching_the_git_dir_follows_the_agents_commits() {
    let repo = sandbox_off();
    edit(&repo, "embed/build.rs", |b| {
        b.replacen(
            "fn main() {",
            "fn main() {\n    let git = |args: &[&str]| {\n        \
             let out = std::process::Command::new(\"git\").args(args).output().unwrap();\n        \
             String::from_utf8(out.stdout).unwrap().trim().to_owned()\n    };\n    \
             println!(\"cargo::rerun-if-changed={}/HEAD\", git(&[\"rev-parse\", \"--absolute-git-dir\"]));\n    \
             println!(\"cargo::rustc-env=HEAD_HASH={}\", git(&[\"rev-parse\", \"HEAD\"]));",
            1,
        )
    });
    edit(&repo, "embed/src/lib.rs", |lib| {
        format!("{lib}pub const HASH: &str = env!(\"HEAD_HASH\");\n")
    });
    app_prints(&repo, "println!(\"hash={}\", embed::HASH);");
    commit_and_base(&repo, "embed records the commit");
    let worktree = repo.spawn("a");
    let start = repo.git(&worktree, ["rev-parse", "HEAD"]);

    repo.build("a").assert_rebuilt(&["embed", "app"]);
    assert_eq!(field(&app_output(&repo, "a"), "hash"), start);
    repo.git(&worktree, ["commit", "-q", "--allow-empty", "-m", "next"]);
    repo.git(&worktree, ["checkout", "-q", "--detach"]);
    let next = repo.git(&worktree, ["rev-parse", "HEAD"]);
    assert_ne!(next, start);
    repo.build("a").assert_rebuilt(&["embed", "app"]);
    assert_eq!(field(&app_output(&repo, "a"), "hash"), next);
    repo.build("a").assert_rebuilt(&[]);
}

/// I17: remap mode reaches the target through `<src>/.ferry-target`, and
/// paths through that link are target paths too.
#[test]
fn remap_mode_rebuilds_only_tests_of_the_binary() {
    let repo = TestRepo::with_env(&[("FERRY_SANDBOX", "off"), ("FERRY_REMAP_PATHS", "1")]);
    put(&repo, "app/tests/bin.rs", BIN_TEST);
    commit_and_base(&repo, "a test of the binary");
    let report = repo.ferry_json(["base"]);
    if report["base"]["remap_paths"] != true {
        common::skip(&format!("remap-paths unsupported here: {report}"));
        return;
    }
    repo.spawn("a");

    repo.build("a").assert_rebuilt(&[]);
    let out = app_output(&repo, "a");
    assert_eq!(field(&out, "marker"), common::BASE_MARKER);
    let built = cargo_test_no_run(&repo, "a", "app");
    assert_eq!(rebuilt_targets(&built), [("app", "bin")], "{}", built.log);
    let out = run_exe(&repo, built.executable("bin"), &["--nocapture"]);
    assert_eq!(
        field(&out, "binary-said"),
        format!("marker={}", common::BASE_MARKER)
    );
    cargo_test_no_run(&repo, "a", "app").assert_rebuilt(&[]);
}

/// I18: `include_str!` of a workspace file outside the package. Cargo up to
/// 1.100 records it by absolute path; later versions relative to the
/// workspace root.
#[test]
fn an_include_outside_the_package_follows_the_agent() {
    let repo = sandbox_off();
    put(&repo, "shared.txt", "v1\n");
    edit(&repo, "embed/src/lib.rs", |lib| {
        format!("{lib}pub const SHARED: &str = include_str!(\"../../shared.txt\");\n")
    });
    app_prints(&repo, "println!(\"shared={}\", embed::SHARED.trim());");
    commit_and_base(&repo, "embed includes shared.txt");
    let worktree = repo.spawn("a");

    let first: &[&str] = if cargo_minor() <= 100 {
        &["embed", "app"]
    } else {
        &[]
    };
    repo.build("a").assert_rebuilt(first);
    assert_eq!(field(&app_output(&repo, "a"), "shared"), "v1");
    repo.write(&worktree.join("shared.txt"), "v2\n");
    repo.build("a").assert_rebuilt(&["embed", "app"]);
    assert_eq!(field(&app_output(&repo, "a"), "shared"), "v2");
    repo.build("a").assert_rebuilt(&[]);
}

#[cfg(target_os = "linux")]
const NAT_BUILD: &str = "use std::process::Command;\n\nfn main() {\n    \
     let out = std::env::var(\"OUT_DIR\").unwrap();\n    \
     std::fs::write(format!(\"{out}/nat.c\"), \"int nat_answer(void) { return 42; }\\n\").unwrap();\n    \
     let built = Command::new(\"cc\")\n        .args([\"-shared\", \"-fPIC\", \"-g0\", \"-o\"])\n        \
     .arg(format!(\"{out}/libnat.so\"))\n        .arg(format!(\"{out}/nat.c\"))\n        \
     .status()\n        .unwrap();\n    assert!(built.success());\n    \
     println!(\"cargo::rerun-if-changed=build.rs\");\n    \
     println!(\"cargo::rustc-link-search=native={out}\");\n    \
     println!(\"cargo::rustc-link-lib=dylib=nat\");\n    \
     println!(\"cargo::rustc-link-arg=-Wl,-rpath,{out}\");\n}\n";

/// `natbin` loads a library its build script built in `OUT_DIR`, through an
/// rpath that the linker baked into the binary.
#[cfg(target_os = "linux")]
fn add_rpath_binary(repo: &TestRepo) {
    add_member(
        repo,
        "natbin",
        "",
        &[
            ("build.rs", NAT_BUILD),
            (
                "src/main.rs",
                "extern \"C\" {\n    fn nat_answer() -> i32;\n}\n\n\
                 fn main() {\n    println!(\"answer={}\", unsafe { nat_answer() });\n}\n",
            ),
        ],
    );
}

#[cfg(target_os = "linux")]
fn natbin_answer(repo: &TestRepo, target: &Path) -> String {
    field(&run_exe(repo, &target.join("debug/natbin"), &[]), "answer")
}

/// I19 (I13 with an rpath): the seeded refresh relinks a binary whose rpath
/// leads into the seed's `OUT_DIR`, so it still starts after gc collects the
/// seed.
#[cfg(target_os = "linux")]
#[test]
fn a_seeded_base_relinks_binaries_with_an_rpath_into_the_seed() {
    let repo = sandbox_off();
    add_rpath_binary(&repo);
    let first = commit_and_base(&repo, "a binary with an rpath");
    edit(&repo, "side/src/lib.rs", |lib| {
        format!("{lib}\npub fn more() {{}}\n")
    });
    repo.commit_all("touch side");
    let refresh = repo.ferry_json(["base"]);
    assert_eq!(refresh["base"]["seeded_from"], first.as_str(), "{refresh}");
    let second = str_of(&refresh["base"]["id"]).to_owned();
    let gc = repo.ferry_json(["gc"]);
    assert_eq!(gc["removed"], serde_json::json!([first]), "{gc}");

    assert_eq!(
        natbin_answer(&repo, &repo.base_dir(&second).join("target")),
        "42"
    );
    repo.spawn("a");
    repo.build("a").assert_rebuilt(&[]);
    assert_eq!(natbin_answer(&repo, &repo.agent_target("a")), "42");
}

/// I20 (I14 with an rpath): a binary linked in the namespace sandbox loads
/// from the canonical target, an empty mountpoint on the host, so an agent
/// without the sandbox relinks it.
#[cfg(target_os = "linux")]
#[test]
fn an_agent_without_the_sandbox_relinks_binaries_with_an_rpath_into_the_canonical_target() {
    let repo = TestRepo::with_env(&[("FERRY_SANDBOX", "namespace")]);
    if !repo.require_userns() {
        return;
    }
    add_rpath_binary(&repo);
    commit_and_base(&repo, "a binary with an rpath");
    let spawned = common::run(
        repo.ferry_cmd(["--json", "spawn", "a"])
            .env("FERRY_SANDBOX", "off"),
    )
    .ok()
    .json();
    assert_eq!(spawned["agent"]["isolation"], "none", "{spawned}");

    repo.build("a").assert_rebuilt(&["natbin"]);
    assert_eq!(natbin_answer(&repo, &repo.agent_target("a")), "42");
    repo.build("a").assert_rebuilt(&[]);
}
