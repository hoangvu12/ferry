use crate::sys::OsStringExt;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::*;

/// The base the synthetic targets were built by.
const ID: &str = "0123456789abcdef0123456789abcdef01234567-0000000a";
const TRIPLE: &str = "x86_64-unknown-linux-gnu";
const RUN: &str = "run-build-script-build-script-build";
const DECLS: &[u8] = b"\0__rustc_proc_macro_decls_0123456789abcdef__\0";

fn put(path: &Path, contents: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// How Cargo writes a unit's hash file: the u64's little-endian bytes.
fn hex(hash: u64) -> String {
    hash.to_le_bytes().iter().fold(String::new(), |mut s, b| {
        write!(s, "{b:02x}").unwrap();
        s
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// Up to Cargo 1.99: `.fingerprint/<pkg>-<H>/`, `build/<pkg>-<H>/`.
    Old,
    /// From Cargo 1.100: `build/<pkg>/<H>/{fingerprint,out,run}/`.
    New,
}

/// A unit of a synthetic target.
#[derive(Debug, Clone, Copy)]
struct U<'a> {
    layout: Layout,
    profile: &'a str,
    pkg: &'a str,
    meta: &'a str,
    hash: u64,
    deps: &'a [u64],
}

impl<'a> U<'a> {
    fn new(pkg: &'a str, meta: &'a str, hash: u64) -> Self {
        Self {
            layout: Layout::Old,
            profile: "debug",
            pkg,
            meta,
            hash,
            deps: &[],
        }
    }

    fn deps(self, deps: &'a [u64]) -> Self {
        Self { deps, ..self }
    }

    fn layout(self, layout: Layout) -> Self {
        Self { layout, ..self }
    }

    fn profile(self, profile: &'a str) -> Self {
        Self { profile, ..self }
    }

    fn fingerprint_dir(&self) -> String {
        let Self {
            profile, pkg, meta, ..
        } = self;
        match self.layout {
            Layout::Old => format!("{profile}/.fingerprint/{pkg}-{meta}"),
            Layout::New => format!("{profile}/build/{pkg}/{meta}/fingerprint"),
        }
    }

    /// Where rustc writes the unit's outputs and dep-info.
    fn out_dir(&self) -> String {
        let Self {
            profile, pkg, meta, ..
        } = self;
        match self.layout {
            Layout::Old => format!("{profile}/deps"),
            Layout::New => format!("{profile}/build/{pkg}/{meta}/out"),
        }
    }

    /// A run's dir and its stdout file's name.
    fn run_dir(&self) -> (String, &'static str) {
        let Self {
            profile, pkg, meta, ..
        } = self;
        match self.layout {
            Layout::Old => (format!("{profile}/build/{pkg}-{meta}"), "output"),
            Layout::New => (format!("{profile}/build/{pkg}/{meta}/run"), "stdout"),
        }
    }

    fn run_out_dir(&self) -> String {
        let Self {
            profile, pkg, meta, ..
        } = self;
        match self.layout {
            Layout::Old => format!("{profile}/build/{pkg}-{meta}/out"),
            Layout::New => format!("{profile}/build/{pkg}/{meta}/out"),
        }
    }
}

/// A target dir at `<store>/bases/<ID>/target`, built from the checkout
/// `<store>/bases/<ID>/src`, whose git dir is outside the store.
struct Synth {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    store: PathBuf,
    src: PathBuf,
    target: PathBuf,
    git_dir: PathBuf,
}

impl Synth {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::sys::canonicalize(tmp.path()).unwrap();
        Self::at(tmp, root.clone(), root.join("store/repo-0123456789abcdef"))
    }

    fn at(tmp: tempfile::TempDir, root: PathBuf, store: PathBuf) -> Self {
        let base = store.join("bases").join(ID);
        let synth = Self {
            src: base.join("src"),
            target: base.join("target"),
            git_dir: root.join("repo/.git/worktrees/src"),
            store,
            root,
            _tmp: tmp,
        };
        fs::create_dir_all(&synth.src).unwrap();
        fs::create_dir_all(&synth.target).unwrap();
        synth
    }

    fn origin(&self, live: bool) -> Origin {
        Origin {
            store: self.store.clone(),
            checkout: self.src.clone(),
            host_checkout: self.src.clone(),
            git_dir: self.git_dir.clone(),
            live: live.then(|| ID.to_owned()),
        }
    }

    fn plan(&self, live: bool) -> RelocationPlan {
        plan_relocation(&self.target, &self.origin(live)).unwrap()
    }

    /// Every path the plan removes, sorted.
    fn removed(&self, live: bool) -> Vec<String> {
        let mut paths: Vec<String> = self
            .plan(live)
            .remove
            .iter()
            .flat_map(|r| r.paths.iter().map(|p| slashed(p)))
            .collect();
        paths.sort();
        paths
    }

    /// Why the unit with fingerprint dir `dir` is removed, if it is.
    fn why(&self, live: bool, dir: &str) -> Option<Capture> {
        self.plan(live)
            .remove
            .iter()
            .find(|r| r.paths.iter().any(|p| p == Path::new(dir)))
            .map(|r| r.why)
    }

    fn src(&self) -> String {
        self.src.display().to_string()
    }

    fn tgt(&self) -> String {
        self.target.display().to_string()
    }

    fn put(&self, rel: &str, contents: impl AsRef<[u8]>) {
        put(&self.target.join(rel), contents);
    }

    /// Writes a unit's hash file and fingerprint JSON; returns its
    /// fingerprint dir.
    fn fingerprint(&self, unit: U, kind: &str) -> String {
        let dir = unit.fingerprint_dir();
        let deps: Vec<String> = unit
            .deps
            .iter()
            .map(|d| format!("[1,\"dep\",false,{d}]"))
            .collect();
        self.put(&format!("{dir}/{kind}"), hex(unit.hash));
        self.put(
            &format!("{dir}/{kind}.json"),
            format!("{{\"deps\":[{}],\"local\":[]}}", deps.join(",")),
        );
        dir
    }

    /// A compile unit whose rustc dep-info is written as rustc does: one
    /// rule per output, a phony rule per input, then env-deps.
    fn compile(
        &self,
        unit: U,
        kind: &str,
        inputs: &[&str],
        env: &[(&str, &str)],
        outputs: &[&str],
    ) -> String {
        let dir = self.fingerprint(unit, kind);
        self.put(
            &format!("{dir}/dep-{kind}"),
            b"\x01\0\0\0\0\x0a\0\0\0src/lib.rs",
        );
        let out = unit.out_dir();
        let stem = format!("{}-{}", unit.pkg.replace('-', "_"), unit.meta);
        let all: Vec<String> = inputs.iter().map(|i| i.replace(' ', "\\ ")).collect();
        let all = all.join(" ");
        let t = self.tgt();
        let mut d = format!("{t}/{out}/{stem}.d: {all}\n\n");
        for output in outputs {
            writeln!(d, "{t}/{out}/{output}: {all}\n").unwrap();
        }
        for input in inputs {
            writeln!(d, "{}:", input.replace(' ', "\\ ")).unwrap();
        }
        for (key, value) in env {
            // Escaped as rustc does: Windows paths hold backslashes.
            let value = value
                .replace('\\', "\\\\")
                .replace('\n', "\\n")
                .replace('\r', "\\r");
            write!(d, "\n# env-dep:{key}={value}").unwrap();
        }
        d.push('\n');
        self.put(&format!("{out}/{stem}.d"), d);
        dir
    }

    /// A library of a workspace member, with a relative input.
    fn member(&self, unit: U, env: &[(&str, &str)]) -> String {
        let input = format!("{}/src/lib.rs", unit.pkg);
        self.compile(
            unit,
            &format!("lib-{}", unit.pkg),
            &[&input],
            env,
            &[&format!("lib{}-{}.rlib", unit.pkg, unit.meta)],
        )
    }

    /// A library from outside the checkout, as a registry crate is.
    fn registry(&self, unit: U, extra_inputs: &[&str], env: &[(&str, &str)]) -> String {
        let source = self
            .root
            .join(format!("registry/{}-1.0.0/src/lib.rs", unit.pkg));
        put(&source, "pub fn f() {}\n");
        let source = source.display().to_string();
        let mut inputs = vec![source.as_str()];
        inputs.extend_from_slice(extra_inputs);
        self.compile(
            unit,
            &format!("lib-{}", unit.pkg),
            &inputs,
            env,
            &[&format!("lib{}-{}.rlib", unit.pkg, unit.meta)],
        )
    }

    /// A build-script run whose `root-output` is its `OUT_DIR` in the base
    /// target; returns that `OUT_DIR` relative to the target.
    fn run(&self, unit: U, stdout: &str) -> String {
        self.fingerprint(unit, RUN);
        let (run, stdout_name) = unit.run_dir();
        let out = unit.run_out_dir();
        self.put(&format!("{run}/{stdout_name}"), stdout);
        self.put(
            &format!("{run}/root-output"),
            format!("{}/{out}", self.tgt()),
        );
        fs::create_dir_all(self.target.join(&out)).unwrap();
        out
    }
}

/// The paths a run's removal covers.
fn run_removal(unit: U) -> Vec<String> {
    match unit.layout {
        Layout::Old => vec![unit.fingerprint_dir(), unit.run_dir().0],
        Layout::New => vec![format!("{}/build/{}/{}", unit.profile, unit.pkg, unit.meta)],
    }
}

/// `path` with `/` separators, as the expectations spell them.
fn slashed(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn sorted(mut paths: Vec<String>) -> Vec<String> {
    paths.sort();
    paths
}

#[test]
fn an_untouched_target_needs_no_removals() {
    let s = Synth::new();
    for layout in [Layout::Old, Layout::New] {
        let run = U::new("embed", "1111111111111111", 1).layout(layout);
        let out = s.run(
            run,
            "cargo::rerun-if-changed=assets\ncargo::rustc-env=E=a=1\n",
        );
        s.put(&format!("{out}/gen.rs"), "pub const X: u32 = 1;");
        let lib = U::new("embed", "2222222222222222", 2)
            .layout(layout)
            .deps(&[1]);
        let generated = format!("{}/{out}/gen.rs", s.tgt());
        let out_dir = format!("{}/{out}", s.tgt());
        s.compile(
            lib,
            "lib-embed",
            &["embed/src/lib.rs", &generated],
            &[("OUT_DIR", &out_dir), ("E", "a=1")],
            &["libembed-2222222222222222.rlib"],
        );
    }
    // Cargo's uplifted copy of the dep-info names absolute paths; it is not
    // rustc's and is never read.
    s.put(
        "debug/libembed.d",
        format!(
            "{}/debug/libembed.rlib: {}/embed/src/lib.rs\n",
            s.tgt(),
            s.src()
        ),
    );
    assert_eq!(s.removed(true), Vec::<String>::new());
    // A dead OUT_DIR may be gone by the time a local crate rebuilds.
    assert_eq!(
        s.removed(false),
        [
            "debug/.fingerprint/embed-2222222222222222",
            "debug/build/embed/2222222222222222/fingerprint"
        ]
    );
}

#[test]
fn roots_are_found_in_every_spelling() {
    let tmp = tempfile::tempdir().unwrap();
    let root = crate::sys::canonicalize(tmp.path()).unwrap();
    // Windows file names cannot hold `"`.
    let name = if cfg!(windows) {
        "a 'é' b"
    } else {
        "a \"é\" b"
    };
    let store = root.join(name).join("repo-0123456789abcdef");
    let s = Synth::at(tmp, root, store);
    let file = format!("{}/x/lib.rs", s.src());
    let json = serde_json::to_string(&file).unwrap();
    let percent = file
        .replace(' ', "%20")
        .replace('"', "%22")
        .replace('é', "%C3%A9");
    for (i, spelling) in [
        file.clone(),
        format!("{file:?}"),
        json,
        file.replace(' ', "\\ "),
        percent,
    ]
    .iter()
    .enumerate()
    {
        let meta = format!("{i:016x}");
        let unit = U::new("reg", &meta, 100 + i as u64);
        let dir = s.registry(unit, &[], &[("SOME_PATH", spelling)]);
        assert_eq!(s.why(true, &dir), Some(Capture::SourcePath), "{spelling}");
    }
}

#[test]
fn every_path_of_the_store_is_classified() {
    let s = Synth::new();
    let store = s.store.display().to_string();
    let out = s.run(U::new("gen", "1111111111111111", 1), "");
    let cases = [
        // (env-dep value, flagged in live mode)
        (format!("{store}/bases/{ID}/src/.ferry-target/{out}"), false),
        (
            format!("{store}/bases/{ID}/src/.ferry-target/debug/app"),
            true,
        ),
        (format!("{store}/bases/{ID}/target/{out}"), false),
        (format!("{store}/bases/{ID}/src"), true),
        (format!("{store}/bases/{ID}/srcX/{out}"), true),
        (format!("{store}/bases/{ID}"), true),
        (format!("{store}/bases/other/target/{out}"), true),
        (format!("{store}/bases/other/src/x"), true),
        (format!("{store}/mnt/target/{out}"), true),
        (format!("{store}/mnt/src/x"), true),
        (format!("{store}/agents/a/target/{out}"), true),
        (format!("{store}/bases/{ID}/target/{out}/../../x"), true),
        (format!("{}/HEAD", s.git_dir.display()), true),
        ("placeholder:app".to_owned(), false),
    ];
    let mut expected = Vec::new();
    for (i, (value, flagged)) in cases.iter().enumerate() {
        let meta = format!("{:016x}", i + 2);
        let unit = U::new("reg", &meta, 10 + i as u64).deps(&[1]);
        let dir = s.registry(unit, &[], &[("V", value)]);
        if *flagged {
            expected.push(dir);
        }
    }
    assert_eq!(s.removed(true), sorted(expected));
}

#[test]
fn a_root_without_a_long_plain_suffix_clears_the_target() {
    let tmp = tempfile::tempdir().unwrap();
    let root = crate::sys::canonicalize(tmp.path()).unwrap();
    let s = Synth::at(tmp, root.clone(), root.join("é/repo"));
    s.member(U::new("leaf", "1111111111111111", 1), &[]);
    let plan = s.plan(true);
    assert!(plan.clear, "{plan:?}");
    let fork = s.root.join("fork");
    put(&fork.join("debug/x"), "x");
    let report = apply_relocation(&fork, &plan).unwrap();
    assert!(report.cleared);
    assert_eq!(fs::read_dir(&fork).unwrap().count(), 0);
    assert!(report.warning().unwrap().contains("full build"));
}

#[test]
fn climbing_out_of_an_exempt_out_dir_is_not_exempt() {
    let s = Synth::new();
    let unit = U::new("gen", "1111111111111111", 1);
    let out = format!("{}/{}", s.tgt(), unit.run_out_dir());
    s.run(
        unit,
        &format!("cargo::rustc-env=ASSETS={out}/../../../../../src/assets\n"),
    );
    let fine = U::new("gen", "2222222222222222", 2);
    let fine_out = format!("{}/{}", s.tgt(), fine.run_out_dir());
    s.run(
        fine,
        &format!("cargo::rustc-env=ASSETS={fine_out}/sub/./x\n"),
    );
    assert_eq!(s.removed(true), sorted(run_removal(unit)));
}

#[test]
fn dep_info_inputs_are_normalized() {
    let s = Synth::new();
    let run = U::new("reg", "1111111111111111", 1);
    let out = s.run(run, "");
    s.put(&format!("{out}/gen.rs"), "pub const X: u32 = 1;");
    let t = s.tgt();
    let input = format!("{t}/{out}/sub/../gen.rs");
    let out_dir = format!("{t}/{out}");
    let lib = U::new("reg", "2222222222222222", 2).deps(&[1]);
    s.registry(lib, &[&input], &[("OUT_DIR", &out_dir)]);
    // A member's input climbing from the target into the checkout.
    let climbing = format!("{t}/{out}/../../../../../src/x.rs");
    let member = U::new("app", "3333333333333333", 3);
    let dir = s.compile(member, "lib-app", &["app/src/lib.rs", &climbing], &[], &[]);
    // Unnormalized, the registry crate's input would climb out of its
    // OUT_DIR and be read as a generated file that does not exist.
    assert_eq!(s.removed(true), [dir.as_str()]);
    assert_eq!(s.removed(false), [dir]);
}

#[test]
fn profiles_are_found_at_any_depth() {
    let s = Synth::new();
    let manifest = format!("{}/x", s.src());
    let mut expected = Vec::new();
    for (i, profile) in [
        "debug".to_owned(),
        format!("{TRIPLE}/debug"),
        format!("miri/{TRIPLE}/debug"),
        "llvm-cov-target/debug".to_owned(),
        format!("ferry-ws/a/b/{TRIPLE}/debug"),
        "ferry-ws/build/debug".to_owned(),
    ]
    .iter()
    .enumerate()
    {
        let meta = format!("{:016x}", i + 1);
        let unit = U::new("x", &meta, i as u64 + 1).profile(profile);
        expected.push(s.member(unit, &[("CARGO_MANIFEST_DIR", &manifest)]));
    }
    // A cargo target someone built inside an OUT_DIR is the run's content.
    let run = U::new("pmt", "aaaaaaaaaaaaaaaa", 50);
    let out = s.run(run, "");
    s.put(
        &format!("{out}/nested/debug/.fingerprint/n-1111111111111111/lib-n"),
        hex(51),
    );
    s.put(
        &format!("{out}/nested/debug/.fingerprint/n-1111111111111111/lib-n.json"),
        "{\"deps\":[]}",
    );
    assert_eq!(s.removed(true), sorted(expected));
}

#[cfg(unix)]
#[test]
fn unreadable_dirs_below_the_target_are_removed() {
    let s = Synth::new();
    s.member(U::new("leaf", "1111111111111111", 1), &[]);
    let locked = s.target.join("tmp/locked");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let removed = s.removed(true);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(removed, ["tmp/locked"]);
}

#[cfg(unix)]
#[test]
fn unreadable_package_dirs_of_the_new_layout_are_removed() {
    let s = Synth::new();
    for (pkg, meta, hash) in [
        ("clean", "1111111111111111", 1),
        ("plain", "2222222222222222", 2),
    ] {
        s.member(U::new(pkg, meta, hash).layout(Layout::New), &[]);
    }
    let locked = s.target.join("debug/build/plain");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let removed = s.removed(true);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(removed, ["debug/build/plain"]);
}

#[test]
fn units_are_their_hash_files_and_unparsable_ones_are_removed() {
    let s = Synth::new();
    for layout in [Layout::Old, Layout::New] {
        let clean = U::new("clean", "1111111111111111", 1).layout(layout);
        s.member(clean, &[]);
        // rustdoc's dep-info sits next to the hash file and is not one.
        let doc = U::new("doc", "2222222222222222", 2).layout(layout);
        let doc_dir = s.fingerprint(doc, "doc-lib-doc");
        s.put(&format!("{doc_dir}/doc-lib-doc.d"), "x: y\n");
    }
    let mut expected = Vec::new();
    for layout in [Layout::Old, Layout::New] {
        let empty = U::new("empty", "3333333333333333", 3).layout(layout);
        let dir = s.member(empty, &[]);
        s.put(&format!("{dir}/lib-empty"), "");
        expected.push(dir);
        // Without its JSON a file is no hash file, and nothing tells what
        // kind of unit the dir holds.
        let no_json = U::new("nojson", "4444444444444444", 4).layout(layout);
        let dir = s.member(no_json, &[]);
        fs::remove_file(s.target.join(format!("{dir}/lib-nojson.json"))).unwrap();
        expected.push(match layout {
            Layout::Old => dir,
            Layout::New => "debug/build/nojson/4444444444444444".into(),
        });
    }
    // No hash file at all: the whole unit goes, run dir included.
    s.put(
        "debug/.fingerprint/bare-5555555555555555/invoked.timestamp",
        "",
    );
    s.put("debug/build/bare-5555555555555555/output", "");
    expected.push("debug/.fingerprint/bare-5555555555555555".into());
    expected.push("debug/build/bare-5555555555555555".into());
    s.put(
        "debug/build/bare/6666666666666666/fingerprint/invoked.timestamp",
        "",
    );
    expected.push("debug/build/bare/6666666666666666".into());
    // An empty hash file of a run removes the run whole.
    let run = U::new("emptyrun", "7777777777777777", 7);
    s.run(run, "");
    s.put(&format!("{}/{RUN}", run.fingerprint_dir()), "");
    expected.extend(run_removal(run));
    assert_eq!(s.removed(true), sorted(expected));
}

#[test]
fn dependencies_resolve_by_little_endian_hash_across_profiles() {
    let s = Synth::new();
    // A host proc macro in debug/ used by a cross-compiled unit.
    let host = U::new("pm", "1111111111111111", 0x0123_4567_89ab_cdef);
    s.registry(host, &[], &[]);
    let profile = format!("{TRIPLE}/debug");
    let cross = U::new("user", "2222222222222222", 2)
        .profile(&profile)
        .deps(&[0x0123_4567_89ab_cdef]);
    s.member(cross, &[]);
    let dangling = U::new("dangling", "3333333333333333", 3).deps(&[999]);
    let dir = s.member(dangling, &[]);
    assert_eq!(s.removed(true), [dir.as_str()]);
    assert_eq!(s.why(true, &dir), Some(Capture::CannotCheck));
}

#[test]
fn dependencies_resolve_in_the_dependents_own_target() {
    let s = Synth::new();
    // The same run, by hash, in the root target and a nested one.
    let root_run = U::new("gen", "1111111111111111", 1);
    let root_out = s.run(root_run, "");
    s.run(root_run.profile("ferry-ws/tools/debug"), "");
    // A unit of the nested workspace names the root target's OUT_DIR.
    let stray = U::new("cap", "2222222222222222", 2)
        .profile("ferry-ws/tools/debug")
        .deps(&[1]);
    let dir = s.member(stray, &[("X", &format!("{}/{root_out}/x", s.tgt()))]);
    assert_eq!(s.removed(true), [dir.as_str()]);
}

#[test]
fn cargo_records_naming_old_paths_flag_their_unit() {
    let s = Synth::new();
    let git = U::new("vergen", "1111111111111111", 1);
    s.run(git, "cargo::rerun-if-changed=x\n");
    s.put(
        &format!("{}/{RUN}.json", git.fingerprint_dir()),
        format!(
            r#"{{"deps":[],"local":[{{"RerunIfChanged":{{"output":"o","paths":[{}]}}}}]}}"#,
            serde_json::to_string(&format!("{}/HEAD", s.git_dir.display())).unwrap()
        ),
    );
    let ws = U::new("wsrel", "2222222222222222", 2);
    let dir = s.member(ws, &[]);
    s.put(
        &format!("{dir}/dep-lib-wsrel"),
        [
            b"\x02\0\0\0\0\x0a\0\0\0src/lib.rs\0\x01".as_slice(),
            format!("{}/x.txt", s.src()).as_bytes(),
        ]
        .concat(),
    );
    assert_eq!(
        s.why(true, &git.fingerprint_dir()),
        Some(Capture::CargoRecord)
    );
    assert_eq!(s.why(true, &dir), Some(Capture::CargoRecord));
    assert_eq!(
        s.removed(true),
        sorted([run_removal(git), vec![dir]].concat())
    );
}

#[test]
fn source_paths_in_dep_info_flag_their_unit() {
    let s = Synth::new();
    let absolute = format!("{}/app/src/gen.rs", s.src());
    let member = U::new("app", "1111111111111111", 1);
    let flagged = s.compile(member, "lib-app", &["app/src/lib.rs", &absolute], &[], &[]);
    // A vendored crate: Cargo hashes the absolute path of a package that is
    // not under the workspace root, so it rebuilds every unit at the
    // agent's checkout anyway, and its run must rerun from scratch.
    let script = U::new("v", "2222222222222222", 2);
    let script_dir = s.compile(
        script,
        "build-script-build-script-build",
        &[&format!("{}/vendor/v/build.rs", s.src())],
        &[],
        &["build_script_build-2222222222222222"],
    );
    let run = U::new("v", "3333333333333333", 3).deps(&[2]);
    s.run(run, "");
    let lib = U::new("v", "4444444444444444", 4).deps(&[3]);
    let lib_dir = s.compile(
        lib,
        "lib-v",
        &[&format!("{}/vendor/v/src/lib.rs", s.src())],
        &[],
        &[],
    );
    let manifest = format!("{}/vendor/w", s.src());
    let env_dep = s.compile(
        U::new("w", "5555555555555555", 5),
        "lib-w",
        &[&format!("{manifest}/src/lib.rs")],
        &[("CARGO_MANIFEST_DIR", &manifest)],
        &[],
    );
    assert_eq!(
        s.removed(true),
        sorted(
            [
                vec![flagged, script_dir, lib_dir, env_dep],
                run_removal(run)
            ]
            .concat()
        )
    );
    assert_eq!(s.why(true, &run.fingerprint_dir()), Some(Capture::Rerun));
}

#[test]
fn target_paths_in_env_values_need_an_exemption() {
    let s = Synth::new();
    let t = s.tgt();
    let own = U::new("own", "1111111111111111", 1);
    let own_out = s.run(own, "");
    let other = U::new("other", "2222222222222222", 2);
    let other_out = s.run(other, "");

    // Reads its own run's OUT_DIR at run time: fine while live.
    let reader = U::new("reader", "3333333333333333", 3).deps(&[1]);
    let reader_dir = s.member(reader, &[("OUT_DIR", &format!("{t}/{own_out}"))]);
    // Reads the OUT_DIR of a run it does not depend on.
    let stray = U::new("stray", "4444444444444444", 4).deps(&[1]);
    let stray_dir = s.member(stray, &[("X", &format!("{t}/{other_out}/x"))]);
    let bin_exe = s.compile(
        U::new("app", "5555555555555555", 5),
        "test-integration-test-cli",
        &["app/tests/cli.rs"],
        &[
            ("CARGO_BIN_EXE_app", &format!("{t}/debug/app")),
            ("CARGO_TARGET_TMPDIR", &format!("{t}/tmp")),
        ],
        &[],
    );
    s.compile(
        U::new("app", "6666666666666666", 6),
        "test-integration-test-cli",
        &["app/tests/cli.rs"],
        &[("CARGO_BIN_EXE_app", "placeholder:app")],
        &[],
    );
    // Registry crates including from their own OUT_DIR, as serde does.
    let include = format!("{t}/{own_out}/private.rs");
    s.put(&format!("{own_out}/private.rs"), "pub use x;");
    let serde = U::new("serde", "7777777777777777", 7).deps(&[1]);
    s.registry(
        serde,
        &[&include],
        &[("OUT_DIR", &format!("{t}/{own_out}"))],
    );
    let runtime = U::new("rtreg", "8888888888888888", 8).deps(&[1]);
    let runtime_dir = s.registry(runtime, &[], &[("OUT_DIR", &format!("{t}/{own_out}"))]);
    let local_include = U::new("localinc", "9999999999999999", 9).deps(&[1]);
    let local_dir = s.compile(
        local_include,
        "lib-localinc",
        &["localinc/src/lib.rs", &include],
        &[("OUT_DIR", &format!("{t}/{own_out}"))],
        &[],
    );

    assert_eq!(
        s.removed(true),
        sorted(vec![stray_dir.clone(), bin_exe.clone()])
    );
    // Without a live base only a registry crate's own include is exempt.
    assert_eq!(
        s.removed(false),
        sorted(vec![reader_dir, stray_dir, bin_exe, runtime_dir, local_dir])
    );
}

#[test]
fn build_script_directives_may_only_name_out_dirs() {
    let s = Synth::new();
    let t = s.tgt();
    for layout in [Layout::Old, Layout::New] {
        let z = U::new("zsys", "1111111111111111", 1).layout(layout);
        let z_out = s.run(z, "");
        let z_out = format!("{t}/{z_out}");
        // Its own OUT_DIR (E1) and a linked dependency's (E2, live only).
        let git = U::new("gitsys", "2222222222222222", 2)
            .layout(layout)
            .deps(&[1]);
        let git_out = format!("{t}/{}", git.run_out_dir());
        s.run(
            git,
            &format!(
                "cargo:rustc-link-search=native={git_out}/lib\n\
                 cargo::metadata=INCLUDE={z_out}/include\n\
                 cargo:warning={}/ignored\n\
                 cargo::error={}/ignored\n\
                 just text {}/ignored\n",
                s.src(),
                s.src(),
                s.src()
            ),
        );
    }
    for layout in [Layout::Old, Layout::New] {
        let src = U::new("srcenv", "3333333333333333", 3).layout(layout);
        s.run(src, &format!("cargo:rustc-env=DIR={}/x\n", s.src()));
    }
    let dead_live: Vec<String> = [Layout::Old, Layout::New]
        .into_iter()
        .flat_map(|l| run_removal(U::new("srcenv", "3333333333333333", 3).layout(l)))
        .collect();
    assert_eq!(s.removed(true), sorted(dead_live.clone()));
    let mut dead = dead_live;
    for layout in [Layout::Old, Layout::New] {
        dead.extend(run_removal(
            U::new("gitsys", "2222222222222222", 2).layout(layout),
        ));
    }
    assert_eq!(s.removed(false), sorted(dead));

    // Without root-output, Cargo rewrites nothing.
    let s = Synth::new();
    let unit = U::new("noroot", "4444444444444444", 4);
    let out = format!("{}/{}", s.tgt(), unit.run_out_dir());
    s.run(unit, &format!("cargo:rustc-link-search={out}\n"));
    fs::remove_file(s.target.join(format!("{}/root-output", unit.run_dir().0))).unwrap();
    assert_eq!(s.removed(true), sorted(run_removal(unit)));
    // A missing stdout cannot be checked.
    let s = Synth::new();
    let unit = U::new("nostdout", "5555555555555555", 5);
    s.run(unit, "");
    fs::remove_file(s.target.join(format!("{}/output", unit.run_dir().0))).unwrap();
    assert_eq!(s.removed(true), sorted(run_removal(unit)));
}

/// Cargo rewrites a replayed run's old `OUT_DIR`, but the linker already
/// baked link arguments (an rpath) into the package's binaries.
#[test]
fn link_arguments_may_only_name_live_out_dirs() {
    let s = Synth::new();
    let t = s.tgt();
    let mut linked = Vec::new();
    for (i, directive) in [
        "rustc-link-arg=-Wl,-rpath,",
        "rustc-link-arg-bins=-Wl,-rpath,",
        "rustc-link-arg-bin=app=-Wl,-rpath,",
        "rustc-link-arg-tests=-Wl,-rpath,",
        "rustc-link-arg-examples=-Wl,-rpath,",
        "rustc-link-arg-benches=-Wl,-rpath,",
        "rustc-cdylib-link-arg=-Wl,-rpath,",
        // Only rustc reads the search path, after the rewrite.
        "rustc-link-search=native=",
    ]
    .iter()
    .enumerate()
    {
        let layout = if i % 2 == 0 { Layout::Old } else { Layout::New };
        let meta = format!("{:016x}", i + 1);
        let unit = U::new("nat", &meta, i as u64 + 1).layout(layout);
        let out = format!("{t}/{}", unit.run_out_dir());
        s.run(unit, &format!("cargo::{directive}{out}\n"));
        if directive.contains("link-arg") {
            linked.extend(run_removal(unit));
        }
    }
    // A live OUT_DIR outlives the binaries that load from it.
    assert_eq!(s.removed(true), Vec::<String>::new());
    assert_eq!(s.removed(false), sorted(linked));
}

#[cfg(unix)]
/// Cargo replaces the exact `root-output` string, not another spelling of
/// the same directory.
#[test]
fn only_the_recorded_spelling_of_an_out_dir_is_rewritten() {
    let tmp = tempfile::tempdir().unwrap();
    let root = crate::sys::canonicalize(tmp.path()).unwrap();
    // Through a link to a name with a space, the canonical spelling's needle
    // is a suffix of the given one's, which is dropped.
    fs::create_dir_all(root.join("My Ferry/repo-0123456789abcdef")).unwrap();
    symlink(root.join("My Ferry"), root.join("link")).unwrap();
    let s = Synth::at(tmp, root.clone(), root.join("link/repo-0123456789abcdef"));
    let canonical = U::new("nat", "1111111111111111", 1);
    let out = root
        .join("My Ferry/repo-0123456789abcdef/bases")
        .join(ID)
        .join("target")
        .join(canonical.run_out_dir());
    s.run(
        canonical,
        &format!("cargo::rustc-link-search=native={}\n", out.display()),
    );
    let given = U::new("given", "2222222222222222", 2);
    let out = format!("{}/{}", s.tgt(), given.run_out_dir());
    s.run(given, &format!("cargo::rustc-link-search=native={out}\n"));
    // A live OUT_DIR is still there under either spelling.
    assert_eq!(s.removed(true), Vec::<String>::new());
    assert_eq!(s.removed(false), sorted(run_removal(canonical)));
}

#[cfg(unix)]
#[test]
fn out_dir_contents_may_only_name_live_out_dirs() {
    let s = Synth::new();
    let t = s.tgt();
    let own = U::new("own", "1111111111111111", 1);
    let out = s.run(own, "");
    s.put(
        &format!("{out}/gen.rs"),
        format!("include_bytes!(\"{t}/{out}/logo.bin\")"),
    );
    s.put(
        &format!("{out}/obj.o"),
        format!("\0comp_dir\0{t}/{out}/build\0"),
    );
    symlink("gen.rs", s.target.join(format!("{out}/link.rs"))).unwrap();
    symlink("../../../release", s.target.join(format!("{out}/up"))).unwrap();

    let src = U::new("src", "2222222222222222", 2);
    let out = s.run(src, "");
    s.put(
        &format!("{out}/dir.rs"),
        format!("pub const DIR: &str = {:?};", s.src()),
    );
    let plugin = U::new("plugin", "3333333333333333", 3);
    let out = s.run(plugin, "");
    s.put(
        &format!("{out}/plugins.txt"),
        format!("{t}/debug/libplugin.so"),
    );
    let abs_link = U::new("abslink", "4444444444444444", 4);
    let out = s.run(abs_link, "");
    symlink(
        s.src.join("include"),
        s.target.join(format!("{out}/include")),
    )
    .unwrap();
    let escape = U::new("escape", "5555555555555555", 5);
    let out = s.run(escape, "");
    symlink("../../../../../../x", s.target.join(format!("{out}/up"))).unwrap();
    let fifo = U::new("fifo", "6666666666666666", 6);
    let out = s.run(fifo, "");
    let made = Command::new("mkfifo")
        .arg(s.target.join(format!("{out}/pipe")))
        .status()
        .unwrap();
    assert!(made.success());
    let unreadable = U::new("unreadable", "7777777777777777", 7);
    let out = s.run(unreadable, "");
    let secret = s.target.join(format!("{out}/secret"));
    put(&secret, "x");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();

    let flagged: Vec<String> = [src, plugin, abs_link, escape, fifo, unreadable]
        .into_iter()
        .flat_map(run_removal)
        .collect();
    assert_eq!(s.removed(true), sorted(flagged.clone()));
    // A dead OUT_DIR may not even name itself.
    assert_eq!(
        s.removed(false),
        sorted([flagged, run_removal(own)].concat())
    );
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
}

/// Files are scanned `CHUNK` bytes at a time, but a path is seen whole.
#[test]
fn paths_across_scan_pieces_are_seen_whole() {
    let s = Synth::new();
    let t = s.tgt();
    // `bytes` with `path` across the first piece boundary, between zeros.
    let across = |path: &str| {
        let mut bytes = vec![0; CHUNK - path.len() / 2];
        bytes.extend_from_slice(path.as_bytes());
        bytes.resize(bytes.len() + 100, 0);
        bytes
    };
    let src = U::new("src", "1111111111111111", 1);
    let out = s.run(src, "");
    s.put(
        &format!("{out}/data.bin"),
        across(&format!("{}/x", s.src())),
    );
    // Climbs out of its own OUT_DIR only after the boundary.
    let climb = U::new("climb", "2222222222222222", 2);
    let out = s.run(climb, "");
    let path = format!("{t}/{out}/{}{}x", "sub/".repeat(40), "../".repeat(45));
    s.put(&format!("{out}/paths.bin"), across(&path));
    // A run of path bytes longer than a piece cannot be checked.
    let long = U::new("long", "3333333333333333", 3);
    let out = s.run(long, "");
    let mut bytes = format!("{t}/{out}/").into_bytes();
    bytes.resize(3 * CHUNK, b'a');
    s.put(&format!("{out}/long.txt"), bytes);
    let fine = U::new("fine", "4444444444444444", 4);
    let out = s.run(fine, "");
    s.put(
        &format!("{out}/data.bin"),
        across(&format!("{t}/{out}/data.txt")),
    );
    assert_eq!(
        s.removed(true),
        sorted(
            [src, climb, long]
                .into_iter()
                .flat_map(run_removal)
                .collect()
        )
    );
}

/// Peak resident memory of this test process, in bytes.
#[cfg(target_os = "linux")]
fn peak_rss() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let kib = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .unwrap();
    kib.trim()
        .trim_end_matches("kB")
        .trim()
        .parse::<u64>()
        .unwrap()
        * 1024
}

/// Some crates download prebuilt libraries of gigabytes into `OUT_DIR`.
#[cfg(target_os = "linux")]
#[test]
fn large_files_are_scanned_in_bounded_memory() {
    const SIZE: u64 = 128 << 20;
    // The peak is per process, and other tests run in this one: measure in
    // a child that runs only this test.
    const CHILD: &str = "FERRY_TEST_SCAN_MEMORY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "freshness::relocation::tests::large_files_are_scanned_in_bounded_memory",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let s = Synth::new();
    let sparse = |rel: &str| {
        let path = s.target.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::File::create(&path).unwrap().set_len(SIZE).unwrap();
    };
    let run = U::new("torch", "1111111111111111", 1);
    let out = s.run(run, "");
    sparse(&format!("{out}/libtorch.so"));
    // A generated input outside every OUT_DIR, and a shared library output.
    let lib = U::new("torch", "2222222222222222", 2).deps(&[1]);
    let generated = format!("{}/gen/big.rs", s.tgt());
    s.compile(
        lib,
        "lib-torch",
        &["torch/src/lib.rs", &generated],
        &[],
        &["libtorch-2222222222222222.so"],
    );
    sparse("gen/big.rs");
    sparse("debug/deps/libtorch-2222222222222222.so");
    assert_eq!(s.removed(true), Vec::<String>::new());
    let peak = peak_rss();
    assert!(peak < SIZE / 2, "peak RSS {} MiB", peak >> 20);
}

#[test]
fn generated_inputs_outside_out_dirs_flag_their_consumer_and_its_runs() {
    let s = Synth::new();
    let t = s.tgt();
    let schema = U::new("schema", "1111111111111111", 1);
    s.run(schema, "");
    s.put(
        "schema/api.rs",
        format!("pub const DEFS: &str = \"{}/defs\";", s.src()),
    );
    s.put("clean/api.rs", "pub const X: u32 = 1;");
    let api = U::new("api", "2222222222222222", 2).deps(&[1]);
    let api_dir = s.compile(
        api,
        "lib-api",
        &["api/src/lib.rs", &format!("{t}/schema/api.rs")],
        &[],
        &[],
    );
    let clean = U::new("clean", "3333333333333333", 3);
    s.compile(
        clean,
        "lib-clean",
        &["clean/src/lib.rs", &format!("{t}/clean/api.rs")],
        &[],
        &[],
    );
    let missing = U::new("missing", "4444444444444444", 4);
    let missing_dir = s.compile(
        missing,
        "lib-missing",
        &["missing/src/lib.rs", &format!("{t}/gone/api.rs")],
        &[],
        &[],
    );
    let why = s.why(true, &api_dir);
    assert_eq!(why, Some(Capture::GeneratedInput));
    assert_eq!(
        s.removed(true),
        sorted([vec![api_dir, missing_dir], run_removal(schema)].concat())
    );
}

/// A proc macro whose library holds `literals` and whose sources are `src`.
fn proc_macro(s: &Synth, unit: U, src: &str, literals: &[&str], local: bool) -> String {
    let lib = format!("lib{}-{}.so", unit.pkg, unit.meta);
    let input = if local {
        let rel = format!("{}/src/lib.rs", unit.pkg);
        put(&s.src.join(&rel), src);
        rel
    } else {
        let path = s.root.join(format!(
            "cargo/registry/src/index.crates.io-0000000000000000/{}-1.0.0/src/lib.rs",
            unit.pkg
        ));
        put(&path, src);
        path.display().to_string()
    };
    let dir = s.compile(unit, &format!("lib-{}", unit.pkg), &[&input], &[], &[&lib]);
    let mut bytes = DECLS.to_vec();
    for literal in literals {
        bytes.extend_from_slice(literal.as_bytes());
        bytes.push(0);
    }
    s.put(&format!("{}/{lib}", unit.out_dir()), bytes);
    dir
}

#[test]
fn users_of_proc_macros_that_may_read_paths_are_flagged() {
    let s = Synth::new();
    // Class M: the same value for registry units in base and agent.
    proc_macro(
        &s,
        U::new("embedimpl", "1111111111111111", 1),
        "std::env::var(\"CARGO_MANIFEST_DIR\")",
        &["CARGO_MANIFEST_DIR"],
        false,
    );
    // rust-embed re-exports rust-embed-impl.
    let embed_dir = s.registry(U::new("embed", "2222222222222222", 2).deps(&[1]), &[], &[]);
    let local_user = U::new("app", "3333333333333333", 3).deps(&[2]);
    let local_dir = s.member(local_user, &[]);
    let registry_user = U::new("webui", "4444444444444444", 4).deps(&[2]);
    let registry_dir = s.registry(registry_user, &[], &[]);
    // Class X: differs for every unit.
    proc_macro(
        &s,
        U::new("outimpl", "5555555555555555", 5),
        "std::env::var(\"OUT_DIR\")",
        &["OUT_DIR"],
        false,
    );
    let x_user = U::new("xuser", "6666666666666666", 6).deps(&[5]);
    let x_dir = s.registry(x_user, &[], &[]);
    // A literal-free proc macro, and a cdylib without the decls marker.
    proc_macro(&s, U::new("plain", "7777777777777777", 7), "", &[], false);
    s.member(U::new("plainuser", "8888888888888888", 8).deps(&[7]), &[]);
    let cdylib = U::new("cdy", "9999999999999999", 9);
    s.compile(cdylib, "lib-cdy", &["cdy/src/lib.rs"], &[], &["libcdy.so"]);
    s.put("debug/deps/libcdy.so", "OUT_DIR CARGO_MANIFEST_DIR");
    s.member(U::new("cdyuser", "aaaaaaaaaaaaaaaa", 10).deps(&[9]), &[]);

    assert_eq!(s.why(true, &local_dir), Some(Capture::ProcMacro));
    assert_eq!(
        s.removed(true),
        sorted(vec![local_dir.clone(), x_dir.clone()])
    );
    // Without a live base, registry users are exposed too.
    assert_eq!(
        s.removed(false),
        sorted(vec![local_dir, embed_dir, registry_dir, x_dir])
    );
}

#[test]
fn allowlisted_and_unattributed_literal_hits() {
    let s = Synth::new();
    // The literal is only in an allowlisted crate's sources.
    let df = U::new("document-features", "1111111111111111", 1);
    let lib = "libdocument_features-1111111111111111.so";
    let source = s.root.join(
        "cargo/registry/src/index.crates.io-0000000000000000/document-features-0.2.12/lib.rs",
    );
    put(&source, "std::env::var(\"CARGO_MANIFEST_DIR\")");
    s.compile(
        df,
        "lib-document_features",
        &[&source.display().to_string()],
        &[],
        &[lib],
    );
    s.put(
        &format!("debug/deps/{lib}"),
        [DECLS, b"CARGO_MANIFEST_DIR"].concat(),
    );
    s.member(U::new("tui", "2222222222222222", 2).deps(&[1]), &[]);
    // The literal is in the library but in no source.
    proc_macro(
        &s,
        U::new("hidden", "3333333333333333", 3),
        "fn f() {}",
        &["CARGO_MANIFEST_DIR"],
        false,
    );
    let hidden_user = s.member(U::new("huser", "4444444444444444", 4).deps(&[3]), &[]);
    // A source of the macro is unreadable.
    let gone = U::new("gone", "5555555555555555", 5);
    proc_macro(
        &s,
        gone,
        "env::var(\"CARGO_MANIFEST_DIR\")",
        &["CARGO_MANIFEST_DIR"],
        false,
    );
    fs::remove_file(
        s.root
            .join("cargo/registry/src/index.crates.io-0000000000000000/gone-1.0.0/src/lib.rs"),
    )
    .unwrap();
    let gone_user = s.member(U::new("guser", "6666666666666666", 6).deps(&[5]), &[]);
    // The literal comes from a dylib helper the macro links.
    let helper = U::new("helper", "7777777777777777", 7);
    s.compile(
        helper,
        "lib-helper",
        &["helper/src/lib.rs"],
        &[],
        &["libhelper-7777777777777777.so"],
    );
    s.put("debug/deps/libhelper-7777777777777777.so", "PWD");
    put(&s.src.join("helper/src/lib.rs"), "std::env::var(\"PWD\")");
    proc_macro(
        &s,
        U::new("usesdylib", "8888888888888888", 8).deps(&[7]),
        "",
        &[],
        false,
    );
    let dylib_user = s.registry(U::new("duser", "9999999999999999", 9).deps(&[8]), &[], &[]);
    // An unreadable library may be a proc macro of any kind.
    let lost = U::new("lost", "aaaaaaaaaaaaaaaa", 10);
    s.compile(lost, "lib-lost", &["lost/src/lib.rs"], &[], &["liblost.so"]);
    let lost_user = s.registry(
        U::new("luser", "bbbbbbbbbbbbbbbb", 11).deps(&[10]),
        &[],
        &[],
    );

    assert_eq!(
        s.removed(true),
        sorted(vec![hidden_user, gone_user, dylib_user, lost_user])
    );
}

#[test]
fn local_proc_macros_using_cwd_or_process_apis_are_suspects() {
    let s = Synth::new();
    for (i, token) in [
        "current_dir",
        "canonicalize",
        "absolute",
        "Command",
        "var(",
        "var_os",
        "vars(",
        "local_file",
        "PWD",
    ]
    .iter()
    .enumerate()
    {
        let pkg = format!("pm{i}");
        let meta = format!("{:016x}", 2 * i + 1);
        let hash = 2 * i as u64 + 1;
        proc_macro(
            &s,
            U::new(&pkg, &meta, hash),
            &format!("x.{token}"),
            &[],
            true,
        );
        let user_meta = format!("{:016x}", 2 * i + 2);
        // A registry user: class X flags every user.
        let user = U::new("user", &user_meta, hash + 1);
        let deps = [hash];
        s.registry(user.deps(&deps), &[], &[]);
    }
    let plain = U::new("pmplain", "aaaaaaaaaaaaaaaa", 100);
    proc_macro(&s, plain, "fn derive() {}", &[], true);
    s.member(
        U::new("plainuser", "bbbbbbbbbbbbbbbb", 101).deps(&[100]),
        &[],
    );
    let missing = U::new("pmmissing", "cccccccccccccccc", 102);
    proc_macro(&s, missing, "", &[], true);
    fs::remove_file(s.src.join("pmmissing/src/lib.rs")).unwrap();
    let missing_user = s.member(U::new("muser", "dddddddddddddddd", 103).deps(&[102]), &[]);

    let removed = s.removed(true);
    assert_eq!(removed.len(), 10, "{removed:?}");
    assert!(removed.contains(&missing_user), "{removed:?}");
    assert!(
        !removed.iter().any(|r| r.contains("plainuser")),
        "{removed:?}"
    );
}

#[test]
fn runs_above_a_flagged_unit_rerun_from_scratch() {
    let s = Synth::new();
    for layout in [Layout::Old, Layout::New] {
        let manifest = format!("{}/dep", s.src());
        // A build-dependency that captured a path.
        let dep = U::new("dep", "1111111111111111", 1).layout(layout);
        s.member(dep, &[("CARGO_MANIFEST_DIR", &manifest)]);
        // `tool`'s build script, its compile and its run.
        let script = U::new("tool", "2222222222222222", 2)
            .layout(layout)
            .deps(&[1]);
        s.compile(
            script,
            "build-script-build-script-build",
            &["tool/build.rs"],
            &[],
            &["build_script_build-2222222222222222"],
        );
        let run = U::new("tool", "3333333333333333", 3)
            .layout(layout)
            .deps(&[2]);
        s.run(run, "");
        // A `links` dependent's run follows tool's run.
        let downstream = U::new("down", "4444444444444444", 4)
            .layout(layout)
            .deps(&[3]);
        s.run(downstream, "");
        // tool's library depends on its run: Cargo rebuilds it unasked.
        s.member(
            U::new("tool", "5555555555555555", 5)
                .layout(layout)
                .deps(&[3]),
            &[],
        );
    }
    let mut expected = Vec::new();
    for layout in [Layout::Old, Layout::New] {
        expected.push(
            U::new("dep", "1111111111111111", 1)
                .layout(layout)
                .fingerprint_dir(),
        );
        expected.extend(run_removal(
            U::new("tool", "3333333333333333", 3).layout(layout),
        ));
        expected.extend(run_removal(
            U::new("down", "4444444444444444", 4).layout(layout),
        ));
    }
    assert_eq!(s.removed(true), sorted(expected));
    let run = U::new("tool", "3333333333333333", 3).fingerprint_dir();
    assert_eq!(s.why(true, &run), Some(Capture::Rerun));
}

#[test]
fn applying_removes_whole_runs_and_compile_fingerprints_only() {
    let s = Synth::new();
    let manifest = format!("{}/x", s.src());
    let mut runs = Vec::new();
    for layout in [Layout::Old, Layout::New] {
        let script = U::new("x", "1111111111111111", 1).layout(layout);
        s.compile(
            script,
            "build-script-build-script-build",
            &["x/build.rs"],
            &[],
            &["build_script_build-1111111111111111"],
        );
        let run = U::new("x", "2222222222222222", 2).layout(layout).deps(&[1]);
        s.run(run, &format!("cargo:rustc-env=D={manifest}\n"));
        if layout == Layout::Old {
            // The build script's binary has a dir of its own.
            s.put(
                "debug/build/x-1111111111111111/build_script_build-1111111111111111",
                "bin",
            );
        }
        runs.push(run);
        s.member(
            U::new("x", "3333333333333333", 3).layout(layout).deps(&[2]),
            &[("D", &manifest)],
        );
    }
    let plan = s.plan(true);
    let fork = s.root.join("fork");
    let copied = Command::new("cp")
        .arg("-a")
        .arg(&s.target)
        .arg(&fork)
        .status()
        .unwrap();
    assert!(copied.success());
    // Removing a path that is already gone is fine.
    fs::remove_dir_all(fork.join(runs[0].run_dir().0)).unwrap();
    let report = apply_relocation(&fork, &plan).unwrap();
    assert_eq!((report.units, report.reruns), (2, 2), "{report:?}");
    assert!(report.warning().unwrap().contains("4 build units"));
    for run in &runs {
        for gone in run_removal(*run) {
            assert!(!fork.join(&gone).exists(), "{gone}");
        }
    }
    for kept in [
        "debug/.fingerprint/x-1111111111111111",
        "debug/build/x-1111111111111111",
        "debug/build/x/1111111111111111/fingerprint",
        "debug/build/x/3333333333333333/out",
        "debug/deps/x-3333333333333333.d",
    ] {
        assert!(fork.join(kept).exists(), "{kept}");
    }
    for gone in [
        "debug/.fingerprint/x-3333333333333333",
        "debug/build/x/3333333333333333/fingerprint",
    ] {
        assert!(!fork.join(gone).exists(), "{gone}");
    }
}

#[cfg(unix)]
#[test]
fn applying_never_follows_a_symlinked_ancestor() {
    let s = Synth::new();
    let manifest = format!("{}/x", s.src());
    s.member(
        U::new("x", "1111111111111111", 1),
        &[("CARGO_MANIFEST_DIR", &manifest)],
    );
    let plan = s.plan(true);
    let outside = s.root.join("outside");
    put(&outside.join(".fingerprint/x-1111111111111111/lib-x"), "x");
    let fork = s.root.join("fork");
    fs::create_dir_all(&fork).unwrap();
    symlink(&outside, fork.join("debug")).unwrap();
    assert!(apply_relocation(&fork, &plan).is_err());
    assert!(
        outside
            .join(".fingerprint/x-1111111111111111/lib-x")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn symlinks_in_the_target_are_never_followed() {
    let s = Synth::new();
    let outside = s.root.join("outside");
    let manifest = format!("{}/x", s.src());
    put(&outside.join("lib-x"), hex(1));
    put(&outside.join("lib-x.json"), "{\"deps\":[]}");
    fs::create_dir_all(s.target.join("debug/.fingerprint")).unwrap();
    symlink(
        &outside,
        s.target.join("debug/.fingerprint/x-1111111111111111"),
    )
    .unwrap();
    s.put(
        "debug/deps/x-1111111111111111.d",
        format!("/t/x.d: src/main.rs\n\n# env-dep:CARGO_MANIFEST_DIR={manifest}\n"),
    );
    // The link itself goes; nothing behind it is read or touched.
    assert_eq!(s.removed(true), ["debug/.fingerprint/x-1111111111111111"]);
    let fork = s.root.join("fork");
    fs::create_dir_all(fork.join("debug/.fingerprint")).unwrap();
    symlink(&outside, fork.join("debug/.fingerprint/x-1111111111111111")).unwrap();
    apply_relocation(&fork, &s.plan(true)).unwrap();
    assert!(outside.join("lib-x").exists());
    assert!(
        fork.join("debug/.fingerprint")
            .read_dir()
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn unhashed_cdylib_dep_info_is_found_by_lib_name() {
    let s = Synth::new();
    let manifest = s.src();
    for (name, env) in [("cdy", Some(manifest.as_str())), ("cdz", None)] {
        let dir = format!("debug/.fingerprint/{name}-1111111111111111");
        s.put(
            &format!("{dir}/lib-{name}"),
            hex(if env.is_some() { 1 } else { 2 }),
        );
        s.put(&format!("{dir}/lib-{name}.json"), "{\"deps\":[]}");
        s.put(&format!("{dir}/dep-lib-{name}"), "src/lib.rs");
        let env = env.map_or(String::new(), |m| {
            format!("\n# env-dep:CARGO_MANIFEST_DIR={m}\n")
        });
        s.put(
            &format!("debug/deps/{name}.d"),
            format!(
                "{0}/debug/deps/lib{name}.so: src/lib.rs\n\nsrc/lib.rs:\n{env}",
                s.tgt()
            ),
        );
    }
    assert_eq!(s.removed(true), ["debug/.fingerprint/cdy-1111111111111111"]);
}

#[cfg(unix)]
#[test]
fn canonical_and_given_spellings_of_the_store_are_both_found() {
    let tmp = tempfile::tempdir().unwrap();
    let root = crate::sys::canonicalize(tmp.path()).unwrap();
    fs::create_dir_all(root.join("real/repo-0123456789abcdef")).unwrap();
    symlink(root.join("real"), root.join("alias")).unwrap();
    let s = Synth::at(tmp, root.clone(), root.join("alias/repo-0123456789abcdef"));
    let real = root
        .join("real/repo-0123456789abcdef/bases")
        .join(ID)
        .join("src/x");
    let dir = s.member(
        U::new("x", "1111111111111111", 1),
        &[("CARGO_MANIFEST_DIR", &real.display().to_string())],
    );
    assert_eq!(s.removed(true), [dir]);
}

#[test]
fn plans_are_cached_only_when_they_fit() {
    let s = Synth::new();
    let plan = s.plan(true);
    assert!(plan.fits(ID));
    assert!(!plan.fits("another-base"));
    assert!(!s.plan(false).fits(ID));
    assert!(
        !RelocationPlan {
            rules: RELOCATION_RULES + 1,
            ..plan.clone()
        }
        .fits(ID)
    );
    // Bases outlive a Ferry upgrade, which may change a rule without
    // bumping RELOCATION_RULES.
    assert!(
        !RelocationPlan {
            ferry: "0.0.0-older".into(),
            ..plan.clone()
        }
        .fits(ID)
    );
    assert!(plan.cacheable());
    let odd = PathBuf::from(OsString::from_vec(b"debug/.fingerprint/\xff".to_vec()));
    let uncacheable = RelocationPlan {
        remove: vec![Removal {
            paths: vec![odd],
            unit: "x".into(),
            run: false,
            why: Capture::CannotCheck,
        }],
        ..plan
    };
    assert!(!uncacheable.cacheable());
}

#[cfg(unix)]
/// An agent checkout at `<root>/agent`. It and the base's both hold
/// `files`, and each of `links` (path, base target, agent target) is a
/// symlink in both.
fn agent_checkout(s: &Synth, files: &[&str], links: &[(&str, &str, &str)]) -> PathBuf {
    let agent = s.root.join("agent");
    for root in [&s.src, &agent] {
        for file in files {
            put(&root.join(file), *file);
        }
    }
    for (at, in_base, in_agent) in links {
        for (root, to) in [(&s.src, in_base), (&agent, in_agent)] {
            let at = root.join(at);
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            symlink(to, at).unwrap();
        }
    }
    agent
}

#[cfg(unix)]
fn link_rebuilt(s: &Synth, origin: &Origin, agent: &Path) -> Vec<String> {
    link_rebuilds(&s.target, origin, agent)
        .unwrap()
        .iter()
        .map(|p| p.display().to_string())
        .collect()
}

#[cfg(unix)]
#[test]
fn units_reading_through_a_retargeted_symlink_rebuild() {
    let s = Synth::new();
    let agent_target = s.store.join("agents/a/target");
    put(&agent_target.join("debug/gen.rs"), "gen");
    s.put("debug/gen.rs", "gen");
    let agent = agent_checkout(
        &s,
        &["leaf/src/lib.rs", "leaf/data/v1.txt", "leaf/data/v2.txt"],
        &[
            ("leaf/data/current.txt", "v1.txt", "v2.txt"),
            ("leaf/data/same.txt", "v1.txt", "v1.txt"),
            // Remap mode's link to each one's own target dir.
            (
                ".ferry-target",
                &s.tgt(),
                &agent_target.display().to_string(),
            ),
        ],
    );
    let mut expected = Vec::new();
    for layout in [Layout::Old, Layout::New] {
        let leaf = U::new("leaf", "1111111111111111", 1).layout(layout);
        let inputs = ["leaf/src/lib.rs", "leaf/src/../data/current.txt"];
        let dir = s.compile(leaf, "lib-leaf", &inputs, &[], &[]);
        expected.push(format!("{dir}/dep-lib-leaf"));
        let same = U::new("same", "2222222222222222", 2).layout(layout);
        let inputs = ["leaf/src/lib.rs", "leaf/src/../data/same.txt"];
        s.compile(same, "lib-same", &inputs, &[], &[]);
        let generated = U::new("gen", "3333333333333333", 3).layout(layout);
        let inputs = [".ferry-target/debug/gen.rs"];
        s.compile(generated, "lib-gen", &inputs, &[], &[]);
        let registry = U::new("reg", "4444444444444444", 4).layout(layout);
        s.registry(registry, &[], &[]);
        // Build-script runs compare mtimes, which stamping covers.
        let run = U::new("leaf", "5555555555555555", 5).layout(layout);
        s.run(run, "cargo::rerun-if-changed=data/current.txt\n");
        // Without rustc's dep-info, nothing shows what a compile read.
        let blind = U::new("blind", "6666666666666666", 6).layout(layout);
        let dir = s.fingerprint(blind, "lib-blind");
        expected.push(format!("{dir}/dep-lib-blind"));
        let doc = U::new("doc", "7777777777777777", 7).layout(layout);
        s.fingerprint(doc, "doc-lib-doc");
    }
    assert_eq!(link_rebuilt(&s, &s.origin(false), &agent), sorted(expected));
}

#[cfg(unix)]
#[test]
fn invalidated_dep_info_is_emptied_in_the_writable_dir() {
    let s = Synth::new();
    let agent = agent_checkout(
        &s,
        &["leaf/src/lib.rs", "leaf/data/v1.txt", "leaf/data/v2.txt"],
        &[("leaf/data/current.txt", "v1.txt", "v2.txt")],
    );
    for layout in [Layout::Old, Layout::New] {
        let leaf = U::new("leaf", "1111111111111111", 1).layout(layout);
        let inputs = ["leaf/src/../data/current.txt"];
        s.compile(leaf, "lib-leaf", &inputs, &[], &[]);
    }
    let stale = link_rebuilds(&s.target, &s.origin(false), &agent).unwrap();
    assert_eq!(stale.len(), 2, "{stale:?}");
    // An overlay's upper dir holds nothing of the unit yet.
    let upper = s.root.join("upper");
    fs::create_dir_all(&upper).unwrap();
    for root in [&s.target, &upper] {
        invalidate_dep_info(root, &stale).unwrap();
        for rel in &stale {
            assert_eq!(fs::read(root.join(rel)).unwrap(), b"", "{}", rel.display());
        }
    }
}

#[cfg(unix)]
#[test]
fn inputs_of_a_nested_workspace_resolve_from_its_root() {
    let s = Synth::new();
    let agent = agent_checkout(
        &s,
        &[
            "tools/Cargo.toml",
            "tools/cfg/a.toml",
            "tools/cfg/b.toml",
            "cfg/a.toml",
        ],
        &[
            ("tools/cfg/now.toml", "a.toml", "b.toml"),
            ("cfg/now.toml", "a.toml", "a.toml"),
        ],
    );
    let mut expected = Vec::new();
    for layout in [Layout::Old, Layout::New] {
        let tool = U::new("tool", "1111111111111111", 1)
            .layout(layout)
            .profile("ferry-ws/tools/debug");
        let dir = s.compile(tool, "bin-tool", &["cfg/now.toml"], &[], &[]);
        expected.push(format!("{dir}/dep-bin-tool"));
        // The same input of the root workspace is another file.
        let root = U::new("root", "2222222222222222", 2).layout(layout);
        s.compile(root, "lib-root", &["cfg/now.toml"], &[], &[]);
    }
    assert_eq!(link_rebuilt(&s, &s.origin(false), &agent), sorted(expected));
}

#[cfg(unix)]
#[test]
fn absolute_inputs_resolve_below_the_checkout_rustc_saw() {
    let s = Synth::new();
    let agent = agent_checkout(
        &s,
        &["vendor/v/src/v1.rs", "vendor/v/src/v2.rs"],
        &[("vendor/v/src/imp.rs", "v1.rs", "v2.rs")],
    );
    // A base built in the namespace sandbox saw its checkout at the mount.
    let mount = s.store.join("mnt/src");
    let sandboxed = Origin {
        checkout: mount.clone(),
        ..s.origin(false)
    };
    let mut at_mount = Vec::new();
    let mut at_host = Vec::new();
    for layout in [Layout::Old, Layout::New] {
        for (pkg, meta, hash, checkout, expected) in [
            ("v", "1111111111111111", 1, &mount, &mut at_mount),
            ("w", "2222222222222222", 2, &s.src, &mut at_host),
        ] {
            let unit = U::new(pkg, meta, hash).layout(layout);
            let input = format!("{}/vendor/v/src/imp.rs", checkout.display());
            let dir = s.compile(unit, &format!("lib-{pkg}"), &[&input], &[], &[]);
            expected.push(format!("{dir}/dep-lib-{pkg}"));
        }
    }
    assert_eq!(link_rebuilt(&s, &sandboxed, &agent), sorted(at_mount));
    assert_eq!(link_rebuilt(&s, &s.origin(false), &agent), sorted(at_host));
}

// Real cargo: a workspace built at `<store>/bases/a/{src,target}`, copied to
// `<tmp>/agent/{ws,target}` as an agent without the sandbox has it.

/// Appends to `$OUT_DIR/runs` on every run.
const COUNTING: &str = "    let runs = std::path::Path::new(&std::env::var(\"OUT_DIR\").unwrap()).join(\"runs\");\n    \
     let mut count = std::fs::read(&runs).unwrap_or_default();\n    count.push(b'x');\n    \
     std::fs::write(&runs, count).unwrap();\n    println!(\"cargo::rerun-if-changed=build.rs\");\n";

struct Moved {
    _tmp: tempfile::TempDir,
    store: PathBuf,
    base_src: PathBuf,
    ws: PathBuf,
    target: PathBuf,
}

impl Moved {
    /// `None` (after saying why) if cargo or a linker is missing.
    fn build(files: &[(&str, &str)]) -> Option<Self> {
        let have = |program: &str, arg: &str| {
            Command::new(program)
                .arg(arg)
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if !have(&cargo().to_string_lossy(), "-V") || !have("cc", "--version") {
            eprintln!("skipping: this test needs cargo and a C linker (`cc`) on PATH");
            return None;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::sys::canonicalize(tmp.path()).unwrap();
        let store = root.join("store/repo-0123456789abcdef");
        let base = store.join("bases/a");
        let base_src = base.join("src");
        for (rel, contents) in files {
            put(&base_src.join(rel), contents);
        }
        let (built, _) = cargo_build(&base_src, &base.join("target"));
        assert!(built.iter().all(|(_, _, fresh)| !fresh), "{built:?}");
        let agent = root.join("agent");
        fs::create_dir_all(&agent).unwrap();
        for (from, to) in [("src", "ws"), ("target", "target")] {
            let copied = Command::new("cp")
                .arg("-a")
                .arg(base.join(from))
                .arg(agent.join(to))
                .status()
                .unwrap();
            assert!(copied.success());
        }
        Some(Self {
            _tmp: tmp,
            ws: agent.join("ws"),
            target: agent.join("target"),
            store,
            base_src,
        })
    }

    fn plan(&self) -> RelocationPlan {
        let origin = Origin {
            store: self.store.clone(),
            checkout: self.base_src.clone(),
            host_checkout: self.base_src.clone(),
            git_dir: self.store.join("no-git-dir/.git/worktrees/src"),
            live: Some("a".into()),
        };
        plan_relocation(&self.store.join("bases/a/target"), &origin).unwrap()
    }

    fn relocate(&self) -> RelocationPlan {
        let plan = self.plan();
        apply_relocation(&self.target, &plan).unwrap();
        plan
    }

    fn build_once(&self) -> (Vec<(String, String, bool)>, Vec<PathBuf>) {
        cargo_build(&self.ws, &self.target)
    }

    fn run(&self, bin: &str) -> String {
        let out = Command::new(self.target.join("debug").join(bin))
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
}

fn cargo() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

fn manifest(name: &str, rest: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{rest}")
}

/// What one `cargo build` of `ws` into `target` did: each compiled unit as
/// (package directory, target kind, fresh), and the build scripts'
/// `OUT_DIR`s.
fn cargo_build(ws: &Path, target: &Path) -> (Vec<(String, String, bool)>, Vec<PathBuf>) {
    let mut command = Command::new(cargo());
    for var in [
        "CARGO_BUILD_BUILD_DIR",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_TARGET",
        "CARGO_INCREMENTAL",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTC_WRAPPER",
    ] {
        command.env_remove(var);
    }
    let out = command
        .args(["build", "--offline", "--message-format=json"])
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_BUILD_BUILD_DIR", target)
        .current_dir(ws)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "cargo build failed: {stderr}");
    let (mut units, mut out_dirs) = (Vec::new(), Vec::new());
    for line in String::from_utf8(out.stdout).unwrap().lines() {
        let message: serde_json::Value = serde_json::from_str(line).unwrap();
        match message["reason"].as_str() {
            Some("compiler-artifact") => {
                let manifest = Path::new(message["manifest_path"].as_str().unwrap());
                let package = manifest.parent().unwrap().file_name().unwrap();
                units.push((
                    package.to_string_lossy().into_owned(),
                    message["target"]["kind"][0].as_str().unwrap().to_owned(),
                    message["fresh"].as_bool().unwrap(),
                ));
            }
            Some("build-script-executed") => {
                out_dirs.push(PathBuf::from(message["out_dir"].as_str().unwrap()));
            }
            _ => {}
        }
    }
    units.sort();
    (units, out_dirs)
}

fn unit(package: &str, kind: &str, fresh: bool) -> (String, String, bool) {
    (package.to_owned(), kind.to_owned(), fresh)
}

const WORKSPACE: &str =
    "[profile.dev]\ndebug = false\n\n[workspace]\nresolver = \"2\"\nmembers = [";

/// C1: generated code naming the manifest dir is fixed in one build.
#[test]
fn a_rerun_build_script_fixes_its_dependents_in_the_same_build() {
    let Some(m) = Moved::build(&[
        (
            "Cargo.toml",
            &format!("{WORKSPACE}\"gen\", \"app\", \"other\"]\n"),
        ),
        ("gen/Cargo.toml", &manifest("gen", "")),
        (
            "gen/build.rs",
            &format!(
                "fn main() {{\n    let dir = std::env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n    \
                 let out = std::env::var(\"OUT_DIR\").unwrap();\n    \
                 std::fs::write(format!(\"{{out}}/dir.rs\"), format!(\"pub const DIR: &str = {{dir:?}};\")).unwrap();\n\
                 {COUNTING}}}\n"
            ),
        ),
        (
            "gen/src/lib.rs",
            "include!(concat!(env!(\"OUT_DIR\"), \"/dir.rs\"));\n",
        ),
        (
            "app/Cargo.toml",
            &manifest("app", "\n[dependencies]\ngen = { path = \"../gen\" }\n"),
        ),
        (
            "app/src/main.rs",
            "fn main() {\n    println!(\"{}\", gen::DIR);\n}\n",
        ),
        ("other/Cargo.toml", &manifest("other", "")),
        ("other/src/lib.rs", "pub fn g() {}\n"),
    ]) else {
        return;
    };
    let plan = m.relocate();
    let runs: Vec<_> = plan.remove.iter().filter(|r| r.run).collect();
    assert_eq!(runs.len(), 1, "{plan:?}");
    let (built, out_dirs) = m.build_once();
    assert_eq!(
        built,
        [
            unit("app", "bin", false),
            unit("gen", "custom-build", true),
            unit("gen", "lib", false),
            unit("other", "lib", true),
        ]
    );
    assert_eq!(Path::new(&m.run("app")), m.ws.join("gen"));
    assert_eq!(fs::read(out_dirs[0].join("runs")).unwrap(), b"x");
    let (again, _) = m.build_once();
    assert!(again.iter().all(|(_, _, fresh)| *fresh), "{again:?}");
}

/// C2: a script rerun because its build-dependency rebuilds starts from an
/// empty `OUT_DIR`, not from the base's state.
#[test]
fn a_build_script_above_a_relocated_build_dependency_reruns_from_scratch() {
    let Some(m) = Moved::build(&[
        ("Cargo.toml", &format!("{WORKSPACE}\"dep\", \"tool\"]\n")),
        ("dep/Cargo.toml", &manifest("dep", "")),
        (
            "dep/src/lib.rs",
            "pub const DIR: &str = env!(\"CARGO_MANIFEST_DIR\");\n",
        ),
        (
            "tool/Cargo.toml",
            &manifest(
                "tool",
                "\n[build-dependencies]\ndep = { path = \"../dep\" }\n",
            ),
        ),
        (
            "tool/build.rs",
            &format!(
                "fn main() {{\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    \
                 let state = std::path::Path::new(&out).join(\"state\");\n    \
                 if let Ok(old) = std::fs::read_to_string(&state) {{\n        \
                 assert_eq!(old, out, \"rerun into another build's OUT_DIR\");\n    }}\n    \
                 std::fs::write(&state, &out).unwrap();\n    let _ = dep::DIR;\n{COUNTING}}}\n"
            ),
        ),
        ("tool/src/lib.rs", "pub fn f() {}\n"),
    ]) else {
        return;
    };
    m.relocate();
    let (built, out_dirs) = m.build_once();
    assert!(built.contains(&unit("dep", "lib", false)), "{built:?}");
    assert_eq!(out_dirs.len(), 1);
    assert!(out_dirs[0].starts_with(&m.target));
    assert_eq!(fs::read(out_dirs[0].join("runs")).unwrap(), b"x");
}

/// C5: Cargo rebuilds a vendored crate at the agent's checkout, because it
/// hashes the absolute path of a package outside the workspace root; its
/// script must not rerun over the base's `OUT_DIR`.
#[test]
fn a_vendored_build_script_reruns_from_scratch() {
    let script = format!(
        "fn main() {{\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    \
         let state = std::path::Path::new(&out).join(\"state\");\n    \
         if let Ok(old) = std::fs::read_to_string(&state) {{\n        \
         assert_eq!(old, out, \"rerun into another build's OUT_DIR\");\n    }}\n    \
         std::fs::write(&state, &out).unwrap();\n{COUNTING}}}\n"
    );
    let Some(m) = Moved::build(&[
        ("Cargo.toml", &format!("{WORKSPACE}\"app\"]\n")),
        (
            ".cargo/config.toml",
            "[source.crates-io]\nreplace-with = \"vendored\"\n\n\
             [source.vendored]\ndirectory = \"vendor\"\n",
        ),
        (
            "app/Cargo.toml",
            &manifest("app", "\n[dependencies]\nvsys = \"0.1\"\n"),
        ),
        (
            "app/src/main.rs",
            "fn main() {\n    println!(\"{}\", vsys::V);\n}\n",
        ),
        ("vendor/vsys/Cargo.toml", &manifest("vsys", "")),
        (
            "vendor/vsys/.cargo-checksum.json",
            "{\"files\":{},\"package\":null}",
        ),
        ("vendor/vsys/build.rs", &script),
        ("vendor/vsys/src/lib.rs", "pub const V: u32 = 7;\n"),
    ]) else {
        return;
    };
    m.relocate();
    let (built, out_dirs) = m.build_once();
    assert!(built.contains(&unit("vsys", "lib", false)), "{built:?}");
    assert_eq!(out_dirs.len(), 1);
    assert!(out_dirs[0].starts_with(&m.target), "{out_dirs:?}");
    assert_eq!(fs::read(out_dirs[0].join("runs")).unwrap(), b"x");
    assert_eq!(m.run("app"), "7");
    let (again, _) = m.build_once();
    assert!(again.iter().all(|(_, _, fresh)| *fresh), "{again:?}");
}

/// C3: reading the live base's `OUT_DIR` at run time needs no rebuild until
/// the script reruns.
#[test]
fn a_live_out_dir_is_reused_until_its_script_reruns() {
    let Some(m) = Moved::build(&[
        ("Cargo.toml", &format!("{WORKSPACE}\"rt\", \"app\"]\n")),
        ("rt/Cargo.toml", &manifest("rt", "")),
        (
            "rt/build.rs",
            "fn main() {\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    \
             std::fs::copy(\"data.in\", format!(\"{out}/data.txt\")).unwrap();\n    \
             println!(\"cargo::rerun-if-changed=data.in\");\n}\n",
        ),
        ("rt/data.in", "one"),
        (
            "rt/src/lib.rs",
            "pub fn data() -> String {\n    \
             std::fs::read_to_string(concat!(env!(\"OUT_DIR\"), \"/data.txt\")).unwrap()\n}\n",
        ),
        (
            "app/Cargo.toml",
            &manifest("app", "\n[dependencies]\nrt = { path = \"../rt\" }\n"),
        ),
        (
            "app/src/main.rs",
            "fn main() {\n    println!(\"{}\", rt::data());\n}\n",
        ),
    ]) else {
        return;
    };
    assert_eq!(m.relocate().remove, []);
    let (built, _) = m.build_once();
    assert!(built.iter().all(|(_, _, fresh)| *fresh), "{built:?}");
    assert_eq!(m.run("app"), "one");
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(m.ws.join("rt/data.in"), "two").unwrap();
    let (built, out_dirs) = m.build_once();
    assert!(built.contains(&unit("rt", "lib", false)), "{built:?}");
    assert!(out_dirs[0].starts_with(&m.target), "{out_dirs:?}");
    assert_eq!(m.run("app"), "two");
}

/// C4: real proc-macro libraries.
#[test]
fn real_proc_macros_are_told_apart_by_their_literals() {
    let pm = |name: &str| manifest(name, "\n[lib]\nproc-macro = true\n");
    let Some(m) = Moved::build(&[
        (
            "Cargo.toml",
            &format!("{WORKSPACE}\"pmlit\", \"pmplain\", \"uselit\", \"useplain\"]\n"),
        ),
        ("pmlit/Cargo.toml", &pm("pmlit")),
        (
            "pmlit/src/lib.rs",
            "use proc_macro::TokenStream;\n#[proc_macro]\n\
             pub fn here(_: TokenStream) -> TokenStream {\n    \
             format!(\"{:?}\", std::env::var(\"CARGO_MANIFEST_DIR\").unwrap()).parse().unwrap()\n}\n",
        ),
        ("pmplain/Cargo.toml", &pm("pmplain")),
        (
            "pmplain/src/lib.rs",
            "use proc_macro::TokenStream;\n#[proc_macro_derive(Plain)]\n\
             pub fn plain(_: TokenStream) -> TokenStream {\n    TokenStream::new()\n}\n",
        ),
        (
            "uselit/Cargo.toml",
            &manifest(
                "uselit",
                "\n[dependencies]\npmlit = { path = \"../pmlit\" }\n",
            ),
        ),
        (
            "uselit/src/lib.rs",
            "pub const HERE: &str = pmlit::here!();\n",
        ),
        (
            "useplain/Cargo.toml",
            &manifest(
                "useplain",
                "\n[dependencies]\npmplain = { path = \"../pmplain\" }\n",
            ),
        ),
        (
            "useplain/src/lib.rs",
            "#[derive(pmplain::Plain)]\npub struct S;\n",
        ),
    ]) else {
        return;
    };
    let plan = m.relocate();
    let units: Vec<&str> = plan.remove.iter().map(|r| r.unit.as_str()).collect();
    assert_eq!(units, ["uselit (lib-uselit)"], "{plan:?}");
    assert_eq!(plan.remove[0].why, Capture::ProcMacro);
    let (built, _) = m.build_once();
    assert_eq!(
        built,
        [
            unit("pmlit", "proc-macro", true),
            unit("pmplain", "proc-macro", true),
            unit("uselit", "lib", false),
            unit("useplain", "lib", true),
        ]
    );
}
