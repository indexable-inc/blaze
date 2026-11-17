//! The ladder's rungs on disk (crates/blaze-cli/tests/ladder/*/), shared by the test binaries:
//! which run today, their files and their `expected`.

#![allow(dead_code, reason = "each test binary uses its own part of this module")]

use std::path::{Path, PathBuf};

/// The execution rungs (`front2/x-*`): tests/front2_rungs.rs and examples/front2_rungs.rs.
pub mod x;

/// The rungs that run today: the next one is added when it runs.
pub const RUNNING: &[&str] =
    &["1-empty", "2-exit", "3-hello", "4-modules", "5-generics", "6-aggregates", "7-traits", "7a-drops", "7c-main-io"];

/// Build support's programs (support/*/) that run: each one seam of the build (layout,
/// statics, extern "C", intrinsics, thread-locals), kept from going backwards like a rung.
pub const SUPPORT: &[&str] =
    &["support/s1-layout", "support/s2-statics", "support/s3-extern-c", "support/s4-intrinsics", "support/s5-thread-local"];

/// The rungs' directory.
pub fn ladder() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-cli/tests/ladder")
}

/// `expected`'s status and standard output.
pub fn expected(dir: &Path) -> (u8, Vec<u8>) {
    let text = std::fs::read(dir.join("expected")).expect("a rung's expected file");
    let mut lines = text.splitn(3, |&b| b == b'\n');
    let status = lines.next().and_then(|l| std::str::from_utf8(l).ok()).and_then(|l| l.strip_prefix("status "));
    let status: u8 = status.and_then(|s| s.trim().parse().ok()).expect("line 1: `status <n>`");
    assert_eq!(lines.next(), Some(&b"stdout"[..]), "line 2 of {}: `stdout`", dir.display());
    (status, lines.next().unwrap_or(&[]).to_vec())
}

/// How a rung must end: line 1 of its `expected`.
#[derive(Debug, PartialEq, Eq)]
pub enum Want {
    /// `status <n>`: the program returns `n`.
    Status(u8),
    /// `panicked <path>`: the program panics, and the run stops at the raise (D91) with the fn
    /// at `path` (as the stop names it) the one that raised it. A `#![no_core]` twin's own
    /// `#[lang = "panic_const_*"]` fns raise (`_Unwind_RaiseException`), so the line names
    /// which check failed: `panicked panic_const_rem_overflow`.
    Panicked(String),
}

impl std::fmt::Display for Want {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Want::Status(n) => write!(f, "status {n}"),
            Want::Panicked(path) => write!(f, "panicked {path}"),
        }
    }
}

/// `expected`'s ending ([`Want`]) and standard output (what the program wrote before it ended).
pub fn wanted(dir: &Path) -> (Want, Vec<u8>) {
    let text = std::fs::read(dir.join("expected")).expect("a rung's expected file");
    let mut lines = text.splitn(3, |&b| b == b'\n');
    let first = lines.next().and_then(|l| std::str::from_utf8(l).ok()).map(str::trim).unwrap_or("");
    let want = match (first.strip_prefix("status "), first.strip_prefix("panicked ")) {
        (Some(n), _) => Want::Status(n.trim().parse().expect("line 1: `status <n>` with n of 0..=255")),
        (_, Some(path)) if !path.trim().is_empty() => Want::Panicked(path.trim().to_string()),
        _ => panic!("line 1 of {}: `status <n>` or `panicked <fn path>`", dir.display()),
    };
    assert_eq!(lines.next(), Some(&b"stdout"[..]), "line 2 of {}: `stdout`", dir.display());
    (want, lines.next().unwrap_or(&[]).to_vec())
}

/// Every `.rs` file under `dir/src`, by path relative to `dir`.
pub fn sources(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.join("src")];
    while let Some(at) = todo.pop() {
        for entry in std::fs::read_dir(&at).expect("a rung's src directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                todo.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path.strip_prefix(dir).expect("under the rung").to_str().expect("UTF-8").to_owned();
                out.push((rel, std::fs::read_to_string(&path).expect("a UTF-8 source file")));
            }
        }
    }
    out.sort();
    out
}
