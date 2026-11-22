//! The ban of reading a `Cargo.lock` by text (rule 30: [`crate::lock::Lock::read`] is the one
//! reader). A text split at the package headers made a package named "" of a malformed entry
//! and misread comments and inline tables (duct-tape audit 2026-11-21, row 6). This test fails
//! when any crate's `src/` splits text at that header again.
//!
//! Left, outside `src/` and so not scanned: the interim bootstrap builder's copies
//! (scripts/src/bin/proc_macro_modules.rs and its build_scripts.rs) and the example
//! crates/blaze-proc-macro/examples/tempo_pipeline.rs; they go with the builder (D193 point 1).
//! Exempt by name: blaze-proc-macro's module_lock_tests.rs, which keeps the old reader as the
//! control of its byte-equality test.
//!
//! Prints `no_crate_reads_a_lock_by_text: N files scanned, 0 uses`.
//! Control: the same scan over a planted line finds it.

use std::string::{String, ToString};
use std::vec::Vec;

/// The split, spelled in two pieces so this file does not hold it whole.
fn banned() -> String {
    let mut out = String::from("split(\"[[pack");
    out.push_str("age]]\")");
    out
}

const EXEMPT: [&str; 2] = ["lock_ban_tests.rs", "module_lock_tests.rs"];

fn uses(file: &str, text: &str, banned: &str, out: &mut Vec<(String, usize)>) {
    for (n, line) in text.lines().enumerate() {
        if line.contains(banned) {
            out.push((file.to_string(), n + 1));
        }
    }
}

/// Every `.rs` file under each crate's `src/`, as `(path, text)`.
fn sources(crates: &std::path::Path, out: &mut Vec<(String, String)>) {
    let mut work = Vec::new();
    for entry in std::fs::read_dir(crates).expect("the crates folder is readable") {
        let src = entry.expect("a directory entry").path().join("src");
        if src.is_dir() {
            work.push(src);
        }
    }
    while let Some(d) = work.pop() {
        for entry in std::fs::read_dir(&d).expect("a src folder is readable") {
            let p = entry.expect("a directory entry").path();
            if p.is_dir() {
                work.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") && !p.file_name().is_some_and(|f| EXEMPT.iter().any(|x| f == *x)) {
                // A non-UTF-8 source is no Rust file; it cannot hold the split.
                if let Ok(text) = std::fs::read_to_string(&p) {
                    out.push((p.display().to_string(), text));
                }
            }
        }
    }
    out.sort();
}

#[test]
fn no_crate_reads_a_lock_by_text() {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    sources(&crates, &mut files);
    assert!(files.len() > 1000, "the scan saw the workspace's crates: {} files under {}", files.len(), crates.display());
    assert!(files.iter().any(|(p, _)| p.ends_with("blaze-proc-macro/src/module.rs")), "the scan saw the module key's reader");
    let banned = banned();
    let mut found = Vec::new();
    for (p, t) in files.iter() {
        uses(p, t, &banned, &mut found);
    }
    assert!(found.is_empty(), "a Cargo.lock is read by blaze_manifest::lock::Lock::read only (rule 30): {found:?}");
    // Control: the scan finds a planted split.
    let mut planted = Vec::new();
    let line = std::format!("    for entry in lock.{banned}.skip(1) {{");
    uses("planted.rs", &std::format!("fn f(lock: &str) {{\n{line}\n    }}\n}}\n"), &banned, &mut planted);
    assert_eq!(planted, [("planted.rs".to_string(), 2)], "the scan finds a planted split");
    std::println!("no_crate_reads_a_lock_by_text: {} files scanned, 0 uses", files.len());
}
