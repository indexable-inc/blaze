//! Incremental reparsing (`syntax::Parse::reparse`, run on every edit of an open file): a text
//! is parsed, one edit is reparsed in place, and the result is checked against a full parse of
//! the edited text (`syntax::fuzz::CheckReparse`).
//!
//! The input is `CheckReparse::from_data`'s, the format of `crates/syntax/test_data/reparse/
//! fuzz-failures`: the edit's start and length on two lines, the inserted text on the third,
//! then the text, which is put inside a function's body. Input that is not UTF-8, or whose
//! edit is not a range of the text on character boundaries, is skipped.
//!
//! Invariants, each checked by `CheckReparse::run` (a violation panics, which the engine
//! reports as a crash):
//! - the reparse neither panics nor hangs;
//! - the reparsed tree's text is the edited text, and its blocks are balanced;
//! - it has the same nodes, kinds and ranges, in the same order, as the full parse.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_fuzzer::Target;
use syntax::fuzz::CheckReparse;

#[path = "syntax_shared.rs"]
mod shared;

const TARGET: Target = Target {
    name: "syntax_reparse",
    check,
    seeds,
    dictionary: &[
        b"\n", b"0\n", b"1\n", b"{", b"}", b"(", b")", b"[", b"]", b"\"", b"'", b"r#\"", b"\"#",
        b"/*", b"*/", b"//", b"fn f() {}", b"let x = ", b"match x {", b"=>", b"::", b"'a",
        b"macro_rules! m {", b"#[cfg(x)]",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

fn check(data: &[u8]) {
    if let Some(check) = CheckReparse::from_data(data) {
        check.run();
    }
}

/// The reparse failures fuzzing found before, then each of the parser's valid test files with
/// one edit at a different place: a brace, a quote or a comment opened, a token replaced, a
/// body pasted in, a range deleted.
fn seeds() -> Vec<Vec<u8>> {
    let failures = shared::rust_files("crates/syntax/test_data/reparse/fuzz-failures");
    // alloc: seeds, setup
    let mut seeds: Vec<Vec<u8>> = failures.iter().map(|path| shared::read(path)).collect();
    const INSERTS: &[&str] = &["{", "}", "\"", "/*", "x", "fn f() { 1 }", "", "'a"];
    for (n, path) in shared::rust_files("crates/parser/test_data/parser/ok").iter().enumerate() {
        let bytes = shared::read(path);
        let Ok(text) = std::str::from_utf8(&bytes) else { continue };
        let Some(first) = text.lines().next() else { continue };
        let mut start = (n * 37) % (first.len() + 1);
        while !first.is_char_boundary(start) {
            start -= 1;
        }
        let len = (n % 4).min(first.len() - start);
        let len = (0..=len).rev().find(|&it| first.is_char_boundary(start + it)).unwrap_or(0);
        let insert = INSERTS[n % INSERTS.len()];
        seeds.push(format!("{start}\n{len}\n{insert}\n{text}").into_bytes()); // alloc: seeds, setup
    }
    seeds
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
