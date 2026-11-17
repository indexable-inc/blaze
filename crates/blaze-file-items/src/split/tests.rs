//! `resplit` and `join` against `split_top_level`: random files of top-level snippets (braces
//! in strings, comments, raw strings, nested groups), random edits, random chunk starts, and
//! every file of library/; with the planted resplit bug the oracle must catch.

extern crate std;

use std::{format, string::String, vec::Vec as StdVec};

use blaze_sim::{Broke, Config, Tape};

use super::{Part, Plant, join, part_digest, resplit_in, speculate, split_top_level};

const SNIPPETS: [&str; 18] = [
    "fn a() { let s = \"}\"; }\n",
    "struct S { a: u8 }\n",
    "impl S { fn f(&self) -> u8 { /* } */ 1 } }\n",
    "mod m { fn g() {} }\n",
    "const C: u8 = { 1 };\n",
    "// a comment with { a brace\n",
    "#[cfg(x)]\nfn b() {}\n",
    "/// doc\nenum E { A, B }\n",
    "macro_rules! m { () => { } }\n",
    "static T: &str = \"{\";\n",
    "use a::b;\n",
    "fn r() -> &'static str { r#\"}\"# }\n",
    "type U = [u8; { 2 }];\n",
    "m! { x }\n",
    "fn c() { 'a: loop { break 'a; } }\n",
    "fn d() -> char { '{' }\n",
    "fn \u{e9}() { \"\u{e9}\" }\n",
    "struct P(u8);\n",
];

const FRAGMENTS: [&str; 14] = ["}", "{", "\"", "/*", "*/", "fn z() {}\n", ";", "// c\n", "x", "#", "!", "\n", "'", "struct Q;\n"];

fn file(tape: &Tape) -> String {
    let mut text = String::new();
    for snippet in tape.vec(0..=12, "snippets", |tape| *tape.pick(&SNIPPETS, "snippet").expect("snippets")) {
        text.push_str(snippet);
    }
    text
}

/// A char boundary of `text` at or after `at`.
fn boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Where an edit goes: anywhere, at a cut, inside a string, or inside a comment.
fn place(tape: &Tape, text: &str) -> usize {
    let near = |needle: &str| text.match_indices(needle).map(|(at, _)| at + needle.len()).collect::<StdVec<_>>();
    let cuts: StdVec<usize> =
        split_top_level(text, blaze_own::testing::heap().alloc()).iter().map(|range| usize::try_from(range.start).expect("small")).collect();
    let choices = [cuts, near("\""), near("/*"), near("//")];
    let at = match tape.below(5, "where") {
        4 => tape.index(text.len() + 1, "anywhere"),
        kind => {
            let usize_kind = usize::try_from(kind).expect("below 5");
            match tape.pick(&choices[usize_kind], "at") {
                Some(&at) => at,
                None => tape.index(text.len() + 1, "anywhere"),
            }
        }
    };
    boundary(text, at)
}

fn edit(tape: &Tape, text: &str) -> String {
    let at = place(tape, text);
    let fragment = *tape.pick(&FRAGMENTS, "fragment").expect("fragments");
    let cut_to = boundary(text, at + tape.index(8, "length"));
    match tape.below(3, "edit") {
        0 => format!("{}{fragment}{}", &text[..at], &text[at..]),
        1 => format!("{}{}", &text[..at], &text[cut_to..]),
        _ => format!("{}{fragment}{}", &text[..at], &text[cut_to..]),
    }
}

fn parts(text: &str) -> StdVec<Part> {
    split_top_level(text, blaze_own::testing::heap().alloc())
        .iter()
        .map(|range| Part { range: *range, digest: part_digest(&text.as_bytes()[range.to_usize()]) })
        .collect()
}

fn resplit_equals_split(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let before = file(tape);
    let mut after = edit(tape, &before);
    if tape.coin("two edits") {
        after = edit(tape, &after);
    }
    let prev = parts(&before);
    let made = resplit_in(&before, &prev, &after, blaze_own::testing::heap().alloc(), plant);
    let want = parts(&after);
    if made.parts[..] != want[..] {
        return Err(Broke::Oracle(format!(
            "resplit differs from split_top_level:\nbefore {before:?}\nafter {after:?}\nresplit {:?}\nsplit {:?}",
            &made.parts[..],
            want
        )));
    }
    Ok(())
}

/// The invariant: a resplit after any edit is the split of the new text, digests and all.
#[test]
fn resplit_equals_split_after_random_edits() {
    blaze_sim::check(Config::cases(2000), |tape| resplit_equals_split(tape, Plant::Clean));
}

/// A resplit that keeps the old parts after the edit when its scan met no old cut (an unclosed
/// brace moved every later cut) is wrong; the oracle must say so.
#[test]
fn the_oracle_catches_a_suffix_kept_unsynced() {
    blaze_sim::control(
        Config::cases(2000),
        Plant::Clean,
        Plant::KeepSuffixUnsynced,
        "resplit differs",
        resplit_equals_split,
    );
}

/// `join` of speculative scans from any starts (char boundaries, mid-token too) is the split.
fn joined_equals_split(text: &str, starts: &[u32]) -> Result<(), String> {
    let len = u32::try_from(text.len()).expect("small");
    let chunks: StdVec<_> = starts
        .iter()
        .enumerate()
        .map(|(at, &start)| speculate(text, start, starts.get(at + 1).copied().unwrap_or(len), blaze_own::testing::heap().alloc()))
        .collect();
    let (joined, _) = join(text, &chunks, blaze_own::testing::heap().alloc());
    let want = split_top_level(text, blaze_own::testing::heap().alloc());
    match joined[..] == want[..] {
        true => Ok(()),
        false => Err(format!("join differs from split_top_level at starts {starts:?}:\n{text:?}\n{:?}\n{:?}", &joined[..], &want[..])),
    }
}

#[test]
fn join_equals_split_from_any_starts() {
    blaze_sim::check(Config::cases(2000), |tape| {
        let text = file(tape);
        let mut starts = StdVec::from([0u32]);
        for _ in 0..tape.below(6, "chunks") {
            starts.push(u32::try_from(boundary(&text, tape.index(text.len() + 1, "start"))).expect("small"));
        }
        starts.sort_unstable();
        starts.dedup();
        joined_equals_split(&text, &starts).map_err(Broke::Oracle)
    });
}

/// On every file of library/, join from `chunk_starts` at 2, 7 and 18 chunks is the split.
#[test]
fn join_equals_split_on_library() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library");
    let mut stack = StdVec::from([root.clone()]);
    let mut files = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())).flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|it| it == "rs") {
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                for chunks in [2, 7, 18] {
                    let starts = super::chunk_starts(&text, chunks, blaze_own::testing::heap().alloc());
                    joined_equals_split(&text, &starts).unwrap_or_else(|why| panic!("{}: {why}", path.display()));
                }
                files += 1;
            }
        }
    }
    assert!(files > 3000, "library/ under {} holds only {files} .rs files", root.display());
}
