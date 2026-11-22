//! The ban of a package walk's inline folder rule (rule 30: served.rs `in_package` is the one
//! rule; duct-tape audit 2026-11-21, row 21). Three walkers each wrote "a folder named `target`
//! or starting with `.`, at any depth" and could drift apart, so a module key missed an input or
//! two walkers keyed one crate two ways. This test fails when a crate's `src/` or the scripts'
//! sources write that rule inline again: a line naming `"target"` and testing a name's leading
//! dot.
//!
//! Exempt by name: walks of blaze's OWN tree for a source scan, which are no package walk
//! (blaze-scope tests/no_spelled_dollar_crate.rs, parser lexed_str/differential.rs, scripts
//! toolchain_cut.rs), and this file.
//!
//! Prints `no_package_walk_writes_its_own_rule: N files scanned, 0 uses`.
//! Control: the same scan over a planted line finds it.

use std::string::{String, ToString};
use std::vec::Vec;

const EXEMPT: [&str; 4] = ["served_ban_tests.rs", "no_spelled_dollar_crate.rs", "differential.rs", "toolchain_cut.rs"];

/// A line that writes the rule inline: it names the folder `target` and tests a leading dot.
fn writes_the_rule(line: &str) -> bool {
    let dot = String::from("starts_with('.") + "')";
    line.contains("\"target\"") && line.contains(dot.as_str())
}

fn uses(file: &str, text: &str, out: &mut Vec<(String, usize)>) {
    for (n, line) in text.lines().enumerate() {
        if writes_the_rule(line) {
            out.push((file.to_string(), n + 1));
        }
    }
}

/// Every `.rs` file under `dirs`, as `(path, text)`.
fn sources(mut work: Vec<std::path::PathBuf>, out: &mut Vec<(String, String)>) {
    while let Some(d) = work.pop() {
        for entry in std::fs::read_dir(&d).expect("a source folder is readable") {
            let p = entry.expect("a directory entry").path();
            if p.is_dir() {
                work.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") && !p.file_name().is_some_and(|f| EXEMPT.iter().any(|x| f == *x)) {
                // A source that is not UTF-8 is no Rust file; it cannot write the rule.
                if let Ok(text) = std::fs::read_to_string(&p) {
                    out.push((p.display().to_string(), text));
                }
            }
        }
    }
    out.sort();
}

#[test]
fn no_package_walk_writes_its_own_rule() {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut roots = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("the crates folder is readable") {
        let src = entry.expect("a directory entry").path().join("src");
        if src.is_dir() {
            roots.push(src);
        }
    }
    roots.push(crates.join("../scripts/src"));
    let mut files = Vec::new();
    sources(roots, &mut files);
    assert!(files.len() > 1000, "the scan saw the workspace's sources: {} files", files.len());
    for walker in ["blaze-proc-macro/src/module.rs", "blaze-cli/src/macro_files.rs", "scripts/src/bin/proc_macro_modules.rs"] {
        assert!(files.iter().any(|(p, _)| p.ends_with(walker)), "the scan saw the package walker {walker}");
    }
    let mut found = Vec::new();
    for (p, t) in files.iter() {
        uses(p, t, &mut found);
    }
    assert!(found.is_empty(), "a package walk takes blaze_build_script::served::in_package (rule 30), never its own rule: {found:?}");
    // Control: the scan finds the rule as the walkers wrote it before.
    let mut planted = Vec::new();
    let dot = String::from("starts_with('.") + "')";
    uses("planted.rs", &std::format!("fn f(name: &str) -> bool {{\n    name != \"target\" && !name.{dot}\n}}\n"), &mut planted);
    assert_eq!(planted, [("planted.rs".to_string(), 2)], "the scan finds a planted rule");
    std::println!("no_package_walk_writes_its_own_rule: {} files scanned, 0 uses", files.len());
}
