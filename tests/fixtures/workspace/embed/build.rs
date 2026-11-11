use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    // For a directory, Cargo compares the newest mtime of the directory
    // itself, its subdirectories and its files against the last run.
    println!("cargo::rerun-if-changed=assets");
    let mut files = Vec::new();
    collect(Path::new("assets"), &mut files);
    files.sort();
    let entries: Vec<String> = files
        .iter()
        .map(|path| {
            // `/` on every platform, so the tests expect one spelling.
            let name = path
                .strip_prefix("assets")
                .unwrap()
                .display()
                .to_string()
                .replace('\\', "/");
            let contents = fs::read_to_string(path).unwrap();
            format!("{name}={}", contents.trim())
        })
        .collect();
    println!("cargo::rustc-env=EMBED_ASSETS={}", entries.join(";"));
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push(path);
        }
    }
}
