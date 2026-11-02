//! The lexer against `rustc_lexer` alone: the same tokens, checkpoints, long tokens and errors
//! from [`LexedStr::lex`] with and without the byte classes, on the parser's files in every
//! edition, on every `.rs` file of the tree's `library/`, `crates/` and `harness/`, and on texts
//! a mutation fuzzer makes from them.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use blaze_ids::rng::SplitMix64;

use super::LexedStr;
use crate::Edition;

/// Texts weighted to the bytes where the classes and `rustc_lexer` could part: prefixes, quotes,
/// `#`, escapes, `.` after digits, bases, exponents, suffixes, comments and their nesting,
/// Unicode whitespace and identifiers, and the end of the text.
const PIECES: &[&str] = &[
    "r", "b", "c", "k", "br", "cr", "rb", "r#", "#", "##", "###", "\"", "\"#", "\"##", "'", "'a", "'r#a",
    "'1", "'_", "_", "__", "0", "1", "9", "0x", "0b", "0o", "0x_", "0xfF", "1e", "1E", "1e+", "1e-", "e",
    "E", "1.", "1.5", "..", ".", "1_000", "u8", "f32", "i128", "_u", "/", "//", "///", "////", "//!",
    "/*", "*/", "/**", "/*!", "/**/", "/***", "*", "\n", " ", "\t", "\r", "\r\n", "\x0B", "\x0C",
    "\u{85}", "\u{200E}", "\u{2028}", "é", "名", "🦀", "\u{200D}", "fn", "Self", "self", "let",
    "async", "gen", "dyn", "try", "union", "{", "}", "(", ")", "[", "]", ";", ":", "::", "<", ">", "=",
    "!", "?", "@", "$", "~", "^", "%", "&", "|", "+", "-", "\\", "x", "Z", "\0", "\x01", "\x7F", "`",
    "\\n", "\\\"", "\\\\", "\\'", "\\0", "\\u{41}", "\\u{D800}", "\\u{}", "\\u{110000}", "\\x7f",
    "\\x80", "\\x4", "\\\n", "\\\n\n", "\\q", "\"\\", "\"\"", "''", "'''", "'\\''", "b'", "b\"",
    "r\"", "br\"", "c\"", "cr#\"", "r##\"", "\"##", "k#fn", "r#fn", "r#_", "r#self",
];

pub(super) fn random_text(rng: &mut SplitMix64) -> String {
    let mut text = String::new();
    for _ in 0..rng.index(200) {
        text.push_str(PIECES[rng.index(PIECES.len())]);
    }
    text
}

/// Every `.rs` file under `dirs`, depth first, without `target` or hidden directories.
fn rust_files(dirs: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = dirs.into_iter().collect();
    let mut out = Vec::new();
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries {
            let path = entry.unwrap().path();
            let name = path.file_name().and_then(|it| it.to_str()).unwrap_or("");
            if path.is_dir() {
                if name != "target" && !name.starts_with('.') {
                    dirs.push(path);
                }
            } else if path.extension().is_some_and(|it| it == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn read_all(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|path| fs::read_to_string(path).unwrap()).collect()
}

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The parser's sources and test files.
pub(super) fn parser_files() -> Vec<String> {
    let out = read_all(&rust_files([manifest_dir().join("src"), manifest_dir().join("test_data")]));
    assert!(out.len() > 500, "the parser's sources and test files are found");
    out
}

/// The parser's sources alone: real Rust with no intended lexer error.
pub(super) fn parser_sources() -> Vec<String> {
    let out = read_all(&rust_files([manifest_dir().join("src")]));
    assert!(out.iter().any(|it| it == include_str!("differential.rs")), "the parser's sources are found");
    out
}

/// Panics, naming the first token that differs, unless `text` lexes alike with the byte
/// classes and with `rustc_lexer` alone in `edition`.
fn assert_alike(edition: Edition, text: &str, what: &str) {
    let fast = LexedStr::lex::<true>(edition, text);
    let slow = LexedStr::lex::<false>(edition, text);
    let errors = |it: &LexedStr<'_>| it.errors().map(|(i, e)| (i, e.to_owned())).collect::<Vec<_>>();
    if *fast.tokens == *slow.tokens
        && *fast.checkpoints == *slow.checkpoints
        && fast.long == slow.long
        && errors(&fast) == errors(&slow)
    {
        assert_hashed(edition, text, &fast, what);
        return;
    }
    let near = |at: usize| text.get(at..).map(|it| it.chars().take(40).collect::<String>());
    let shared = fast.tokens.len().min(slow.tokens.len());
    match (0..shared).find(|&i| fast.raw_kind(i) != slow.raw_kind(i) || fast.raw_len(i) != slow.raw_len(i)) {
        Some(i) => {
            let at = slow.text_start(i);
            panic!(
                "{what} ({edition:?}): token {i} at byte {at} is {:?} of {} bytes with the classes and \
                 {:?} of {} by rustc_lexer, at {:?}",
                fast.raw_kind(i),
                fast.raw_len(i),
                slow.raw_kind(i),
                slow.raw_len(i),
                near(at),
            )
        }
        None => panic!(
            "{what} ({edition:?}): {} and {} tokens; checkpoints {}, long tokens {:?} and {:?}, \
             errors {:?} and {:?}",
            fast.tokens.len(),
            slow.tokens.len(),
            if *fast.checkpoints == *slow.checkpoints { "alike" } else { "differ" },
            fast.long,
            slow.long,
            errors(&fast),
            errors(&slow),
        ),
    }
}

/// Panics unless [`LexedStr::new_hashed`] lexes `text` as `fast` did (the tokens, checkpoints,
/// long tokens and errors) and hashes each name as the interner does: one
/// [`blaze_phf::name_hash`] for each token [`LexedStr::has_name_hash`] is true of, in order,
/// of its text without a raw identifier's `r#`, read back from the text.
fn assert_hashed(edition: Edition, text: &str, fast: &LexedStr<'_>, what: &str) {
    let hashed = LexedStr::new_hashed(edition, text);
    assert!(
        *hashed.tokens == *fast.tokens && *hashed.checkpoints == *fast.checkpoints && hashed.long == fast.long,
        "{what} ({edition:?}): hashing changed the tokens"
    );
    assert!(hashed.errors().eq(fast.errors()), "{what} ({edition:?}): hashing changed the errors");
    assert!(fast.name_hashes().is_empty(), "{what}: `new` hashes nothing");
    let mut names = hashed.name_hashes().iter();
    let mut at = 0;
    for i in 0..hashed.len() {
        let len = hashed.raw_len(i);
        if LexedStr::has_name_hash(hashed.raw_kind(i)) {
            let token = &text[at..at + len];
            let name = token.strip_prefix("r#").unwrap_or(token);
            let hash = names.next().unwrap_or_else(|| panic!("{what} ({edition:?}): no hash for token {i}, {token:?}"));
            assert_eq!(*hash, blaze_phf::name_hash(name.as_bytes()), "{what} ({edition:?}): token {i}, {token:?}");
        }
        at += len;
    }
    assert_eq!(names.len(), 0, "{what} ({edition:?}): hashes past the last name");
}

/// The hashed lex of a warm worker allocates nothing: its hashes are the worker's scratch too.
#[test]
fn a_warm_hashed_lex_allocates_nothing() {
    let text = "fn main() { let r#type = Self::new(iter_mut, a_much_longer_name); }\n".repeat(64);
    drop(LexedStr::new_hashed(Edition::CURRENT, &text));
    blaze_alloc_count::assert_at_most(0, "a warm hashed lex", || {
        let lexed = LexedStr::new_hashed(Edition::CURRENT, &text);
        assert_eq!(lexed.name_hashes().len(), 64 * 8);
    });
}

/// A text lexed with the classes is the text lexed by `rustc_lexer` alone, in every edition:
/// the parser's files, random texts, and the edges (a shebang and frontmatter, guarded strings,
/// a token past the packed length, every length around a block's end).
#[test]
fn fast_and_rustc_lexer_lex_alike() {
    let mut rng = SplitMix64::new(13);
    let mut texts = parser_files();
    texts.extend((0..2000).map(|_| random_text(&mut rng)));
    texts.push("#!/usr/bin/env cargo\n---\n[package]\n---\nfn main() {}\n".to_owned());
    texts.push("#!/bin/sh\nfn f() { r#\"x\"# ; #\"y\"# ## z }".to_owned());
    texts.push(format!("fn f() {{ \"{}\" }}", "s".repeat((1 << 22) + 3)));
    texts.push(format!("fn f() {{ {} }}", " ".repeat((1 << 22) + 3)));
    texts.push(format!("fn {}() {{}}", "x".repeat((1 << 22) + 3)));
    texts.push(format!("/* {} */", "/* */ ".repeat(1 << 16)));
    texts.push(format!("/*{}{}", "/*".repeat(1 << 12), "*/".repeat(1 << 12)));
    for len in 0..200 {
        for tail in ["x", " ", ";", "1", "'a", "\"s\"", "//", "/* */", "r#\"\"#", "é"] {
            texts.push(format!("{}{tail}", "a".repeat(len)));
            texts.push(format!("{}{tail}", " ".repeat(len)));
            texts.push(format!("{}{tail}", ";".repeat(len)));
        }
    }
    for (i, text) in texts.iter().enumerate() {
        for edition in Edition::iter() {
            assert_alike(edition, text, &format!("text {i}"));
        }
    }
}

/// Special tokens lexed in a block's window (`scan::simple`) lex as `rustc_lexer` lexes them at
/// every offset of a block: one alone, several in a row, one ending right at a block's end or
/// past it, one after a block full of one-byte tokens, and raw identifiers, whose names are
/// hashed there.
#[test]
fn special_tokens_lex_alike_at_every_offset_of_a_window() {
    let specials = [
        "1", "0x1f", "1.5e3", "2u8", "\"s\"", "\"a\\\"b\"", "'a", "'c'", "b'x'", "// c\n", "/* c */",
        "/** d */", "r#x", "r#\"q\"#", "#", "`", "1\"s\"'c'#2", "//x\n'a 1", "\"crosses\"", "r#type",
    ];
    for k in 0..130 {
        for special in specials {
            for (filler, after) in [(";", "x"), (" ", " y"), ("a ", ";"), (";", "")] {
                let text = format!("{}{special}{after}{special}", filler.repeat(k));
                for edition in Edition::iter() {
                    assert_alike(edition, &text, &format!("{k} of {filler:?} then {special:?}"));
                }
            }
        }
    }
}

/// Every `.rs` file of the tree's `library/` (std at the pinned nightly), `crates/` and
/// `harness/` (the harness projects) lexes alike, in the 2015 edition and the current one,
/// on as many threads as the machine has.
#[test]
fn every_file_of_the_corpus_lexes_alike() {
    let root = manifest_dir().join("../..");
    let files = rust_files(["library", "crates", "harness"].map(|dir| root.join(dir)));
    assert!(files.len() > 7000, "{} files in library/, crates/ and harness/", files.len());
    let (next, read) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let workers = std::thread::available_parallelism().map_or(4, |it| it.get());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(i) else { break };
                    // A file that is not UTF-8 is not Rust; the corpus has a few on purpose.
                    let Ok(text) = fs::read_to_string(path) else { continue };
                    read.fetch_add(1, Ordering::Relaxed);
                    for edition in [Edition::Edition2015, Edition::CURRENT] {
                        assert_alike(edition, &text, &path.display().to_string());
                    }
                }
            });
        }
    });
    let read = read.into_inner();
    assert!(read * 100 >= files.len() * 99, "{read} of {} files read as UTF-8", files.len());
}

/// The fuzzer: a window of up to 2 KiB of a parser file, changed one to eight times (a piece
/// inserted, a range deleted, a range copied elsewhere, a character replaced by a piece, all
/// at character boundaries), lexes alike in a random edition.
#[test]
fn mutated_texts_lex_alike() {
    let seeds = parser_files();
    let mut rng = SplitMix64::new(17);
    let editions: Vec<Edition> = Edition::iter().collect();
    let boundary = |text: &str, rng: &mut SplitMix64| {
        let mut at = rng.index(text.len() + 1);
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    for round in 0..20_000 {
        let seed = &seeds[rng.index(seeds.len())];
        let from = boundary(seed, &mut rng);
        let mut to = (from + 1 + rng.index(2048)).min(seed.len());
        while !seed.is_char_boundary(to) {
            to -= 1;
        }
        let mut text = seed[from..to].to_owned();
        for _ in 0..1 + rng.index(8) {
            let at = boundary(&text, &mut rng);
            let mut end = (at + rng.index(16)).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            match rng.index(4) {
                0 => text.insert_str(at, PIECES[rng.index(PIECES.len())]),
                1 => text.replace_range(at..end, ""),
                2 => {
                    let copy = text[at..end].to_owned();
                    let to = boundary(&text, &mut rng);
                    text.insert_str(to, &copy);
                }
                _ => {
                    let len = text[at..].chars().next().map_or(0, char::len_utf8);
                    text.replace_range(at..at + len, PIECES[rng.index(PIECES.len())]);
                }
            }
        }
        let edition = editions[rng.index(editions.len())];
        assert_alike(edition, &text, &format!("mutant {round}"));
    }
}
