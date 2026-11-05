//! Builds the page's files into the crate: every file under `assets/` but the tools that make
//! them (`dev/`, `build/`) and their notes (`*.md`), by its path under `assets/`.
#![forbid(unsafe_code)]

use std::{
    fs,
    path::{Path, PathBuf},
};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    println!("cargo::rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    let mut out = String::from("pub(crate) static ASSETS: &[(&str, &[u8])] = &[\n");
    for file in &files {
        let name = file.strip_prefix(&root).expect("under assets/").to_str().expect("UTF-8");
        let name = name.replace('\\', "/");
        out.push_str(&format!(
            "    ({name:?}, include_bytes!({:?})),\n",
            file.display().to_string()
        ));
    }
    out.push_str("];\n");
    let at = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("assets.rs");
    fs::write(at, out).expect("writing assets.rs");
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("a directory entry").path();
        let name = path.file_name().and_then(|it| it.to_str()).unwrap_or_default();
        if path.is_dir() {
            if name != "dev" && name != "build" && name != "node_modules" {
                println!("cargo::rerun-if-changed={}", path.display());
                walk(&path, files);
            }
        } else if !name.ends_with(".md") && !name.starts_with('.') {
            files.push(path);
        }
    }
}
