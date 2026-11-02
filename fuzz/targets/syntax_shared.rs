//! What the syntax targets share: the files they are seeded with.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `dir` of this repository (a path from the root), sorted; panics on a
/// directory it can't read.
pub fn rust_files(dir: &str) -> Vec<PathBuf> {
    let mut dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(dir)]; // alloc: seeds, setup
    let mut files = Vec::new(); // alloc: seeds, setup
    while let Some(dir) = dirs.pop() {
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        for entry in entries {
            let path = entry.unwrap_or_else(|e| panic!("{}: {e}", dir.display())).path();
            match path.is_dir() {
                true => dirs.push(path),
                false if path.extension().is_some_and(|it| it == "rs") => files.push(path),
                false => {}
            }
        }
    }
    files.sort();
    files
}

/// The bytes of `path`; panics when it can't be read.
pub fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}
