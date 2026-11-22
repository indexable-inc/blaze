//! token-mark-2 (drive.md, "A token keeps its mark"): `$crate` travels as a MARK, never as a
//! spelled identifier in expansion text. The gate: no source file under crates/ holds the old
//! spelled prefix. CONTROL: with the old `render_with`/`to_path` (blaze-surface's `DOLLAR_CRATE`
//! const and the spelled-identifier arm), this test finds blaze-surface/src/macro_def.rs and
//! fails. Prints nothing.
//!
//! A test reading the checkout it is built from: the tree is the input, read once, in a sorted
//! work list (no recursion), so the first offender named is the same on any machine.

use std::path::{Path, PathBuf};
use std::vec::Vec;

/// The spelled prefix, assembled so this file does not hold it.
fn spelled() -> std::string::String {
    ["__dollar", "_crate_"].concat()
}

#[test]
fn no_source_under_crates_spells_dollar_crate() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("crates/ holds blaze-scope").to_path_buf();
    let needle = spelled();
    let mut work: Vec<PathBuf> = std::vec![crates];
    let mut found: Vec<PathBuf> = Vec::new();
    while let Some(dir) = work.pop() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.unwrap_or_else(|e| panic!("{}: {e}", dir.display())).path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if name != "target" && !name.starts_with('.') {
                    work.push(path);
                }
                continue;
            }
            if !matches!(path.extension().and_then(|e| e.to_str()), Some("rs" | "md" | "toml")) {
                continue;
            }
            // A file that is not UTF-8 holds no Rust source.
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if text.contains(needle.as_str()) {
                found.push(path);
            }
        }
    }
    found.sort();
    assert!(found.is_empty(), "the spelled `$crate` is back (token-mark-2: a `$crate` is a mark): {found:?}");
}
