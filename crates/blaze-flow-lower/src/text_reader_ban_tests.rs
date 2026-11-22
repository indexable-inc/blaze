//! The ban of the text-side lifetime reader (piece L6, D148): a fn's, an impl's or a field's
//! regions come from the head records (`RegionFacts`), never from an item's text. The reader
//! (`read_signature` and its family, sig/{alias,impl_life,outlives,value}.rs) is deleted; this
//! test fails when any source file of the crate defines or calls one of its names again.
//!
//! Prints `no_source_file_names_the_text_reader: N files scanned, 0 uses`.
//! Control: the same scan over a planted line (`let x = read_signature(t, ..);`) finds it.

use std::string::{String, ToString};
use std::vec::Vec;

/// The reader's names, as a definition or a call would write them. The test's own file is
/// skipped (it spells them here).
const BANNED: [&str; 10] = [
    "fn read_signature",
    "read_signature(",
    "read_signature_with(",
    "read_async_signature",
    "read_impl_signature",
    "read_impl_trait(",
    "read_callable_",
    "SigBuf",
    "TextRegion",
    "impl_header_end",
];

const OWN: &str = "text_reader_ban_tests.rs";

/// Each `(file, line number, banned name)` in `text`.
fn uses<'t>(file: &'t str, text: &'t str, out: &mut Vec<(&'t str, usize, &'static str)>) {
    for (n, line) in text.lines().enumerate() {
        for b in BANNED {
            if line.contains(b) {
                out.push((file, n + 1, b));
            }
        }
    }
}

/// Every `.rs` file under `dir`, as `(path, text)`, sorted by path.
fn sources(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
    let mut work = std::vec![dir.to_path_buf()];
    while let Some(d) = work.pop() {
        for entry in std::fs::read_dir(&d).expect("the crate's src directory is readable") {
            let p = entry.expect("a directory entry").path();
            if p.is_dir() {
                work.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") && p.file_name().is_some_and(|f| f != OWN) {
                let text = std::fs::read_to_string(&p).expect("a source file is UTF-8");
                out.push((p.display().to_string(), text));
            }
        }
    }
    out.sort();
}

#[test]
fn no_source_file_names_the_text_reader() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    assert!(files.len() > 10, "the scan saw the crate's sources: {} files under {}", files.len(), src.display());
    assert!(files.iter().any(|(p, _)| p.ends_with("lib.rs")), "the scan saw lib.rs");
    let mut found = Vec::new();
    for (p, t) in files.iter() {
        uses(p, t, &mut found);
    }
    assert!(found.is_empty(), "the text reader is banned (L6, D148: regions come from the head records): {found:?}");
    // Control: the scan finds a planted call.
    let mut planted = Vec::new();
    uses("planted.rs", "fn f() {\n    let x = read_signature(t, tys, &inputs, output, &adt, &mut buf, alloc);\n}\n", &mut planted);
    assert_eq!(planted, [("planted.rs", 2, "read_signature(")], "the scan finds a planted call");
    std::println!("no_source_file_names_the_text_reader: {} files scanned, 0 uses", files.len());
}
