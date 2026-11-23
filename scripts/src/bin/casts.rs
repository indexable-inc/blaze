//! casts: the integer `as` casts only go down, per crate (AGENTS.md correct-by-construction,
//! rule 76; type-then-lint-then-rule, rule 73). `DOC` below is what its usage error prints; it
//! says what a cast is, what the text scan misses, which code is a test's, and the rule.
//!
//!     blaze run -q -p scripts --bin casts -- '{"command": "check", "base": "origin/main"}'
//!     blaze run -q -p scripts --bin casts -- '{"command": "write"}'
//!     blaze run -q -p scripts --bin casts -- selftest
//!
//! Why (2026-11-15): `x as u32` is one of five conversions (widen, narrow and hope, truncate on
//! purpose, reinterpret the sign, or a mix), and the reader cannot tell which; a narrowing that
//! drops bits it should not is silent. crates/blaze-int names each (after Zig's `@intCast`,
//! `@truncate`): `widen_to` only where nothing is lost, `narrow` checked with a typed error,
//! `narrow_or_stop` where failure is a bug, `truncate_to` and `saturate` on purpose, and
//! `core`'s `cast_signed`/`cast_unsigned`. This holds the count of what is left from the text,
//! with no compiler, as `allocs` and `untyped` hold theirs, until a blaze lint with a fix
//! (`blaze::int_cast`, rule 73) replaces it.
//!
//! Test code, the scanner, the walk (`CrateScope::AllDirs`, since `tools/ratchets/casts.txt`
//! tracks `harness/*` too), the list and the base comparison are `blaze_script::ratchet`'s.

/// What the usage error prints.
const DOC: &str = r#"casts: the integer `as` casts only go down, per crate (AGENTS.md correct-by-construction, rule 76).

    blaze run -q -p scripts --bin casts -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin casts -- '{"command": "write"}'
    blaze run -q -p scripts --bin casts -- selftest

    command   check   each crate's count equals its row's in the list: one over its row is
                      refused (its new casts named given `base`), one under it must be lowered
                      with `write`; a crate with no row is held to 0, so a new crate starts at
                      0; with `base`, no row is above the base's (CI)
              write   write the list from the tree as it is
              findings  print every cast as a `blaze_script::finding` document (for `advise`)
    root      the checkout (".")
    base      the commit the change is against, or null: its list (`git show`) holds every
              row down, and the lines added since it (`git diff --unified=0`) name a crate's
              new casts

The list's row is `<crate><TAB><casts>`, of `.rs` files under crates/ (a directory named
`target` is not read).

A cast is the word `as` followed (after spaces or line breaks) by the name of a primitive
integer type, `u8` to `u128`, `usize`, `i8` to `i128` or `isize`, as a whole word; counted at
the line of its `as`, once per `as` (`x as u32 as usize` is two). Comments, strings (raw and
byte strings too) and character literals are skipped. Not counted:
  - `as` in a `use` item, from the word `use` to its `;` (`use a::B as C`: a rename);
  - `as` before any other type: `<T as Trait>`, `as char`, `as f64`, `as *const u8`, `as Self`;
  - test code (below).
The count is read from the text, with no types, so it differs from a lint's:
  missed  `as _` and `as T` where `_` or `T` is an integer (an inferred type, an alias, a
          type parameter); a path to the type (`as core::primitive::u32`); a cast a macro
          makes from tokens (`$e as $t`)
  over    a crate's own type named like a primitive integer (`struct u8;`), which no crate has
`as` in a `macro_rules!` body to a written integer type (`$e as i64`) counts once, at the
template.

A site in test code is not counted. Test code is, and nothing else is:
  - a file under a directory named `tests` or `benches`, or named `tests.rs` or `test.rs`;
  - a file that says `#![cfg(test)]`;
  - the item after `#[cfg(test)]` or `#[cfg(all(test, ...))]`, or after `#[test]` or another
    attribute whose path ends in the word `test` (`#[tokio::test]`, not `#[test_case]`), from
    the attribute's line to the line of the `}` or `;` that ends the item (strings,
    characters and comments read as Rust reads them); an attribute whose item does not end
    before its block does (a match arm, a field) covers its own line only;
  - the file of a module declared out of line in test code (`#[cfg(test)] mod t;`, or any
    `mod m;` in a test file): `m.rs` or `m/mod.rs` beside a crate root (`lib.rs`, `main.rs`,
    `mod.rs`, `build.rs`, a file of a `bin` directory), else in the directory named for the
    declaring file. A `#[path]` is not followed, so such a module counts.
Everything else counts, build scripts and binaries included.

A crate is the nearest directory above the file, under crates/, that holds a Cargo.toml, named
by its path under crates/ (`blaze-cargo`, `harness/harness-native`). crates/blaze-int, the one
home of the conversions, has a row of its own: its few casts are the conversions themselves.
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use blaze_script::{
    TempDir, fail,
    finding::{self, Finding},
    input::{self, Fields, Input, InputError},
    ratchet::{self, BaseError, CrateScope, FileTests, Hit, ListError, Policy, Run, Tests, comment_end, ident_len, quote_end, string_end, word_end},
};

const SCRIPT: &str = "casts";
/// The list, from the root.
const LIST: &str = "tools/ratchets/casts.txt";
/// The primitive integer types: a cast to one of them is a site.
const INTEGERS: [&str; 12] = ["u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize"];
/// What every message about a crate over its row ends with.
const RULE: &str = "AGENTS.md correct-by-construction (rule 76): a conversion says what happens to a value that does \
                    not fit; `as` says nothing, and drops bits silently";
/// What to write instead, for a message.
const INSTEAD: &str = "write the conversion that says what it does, from blaze-int (`use blaze_int::Int`): \
                       `x.widen_to::<u64>()` where no value can be lost (it compiles only then), `x.narrow::<u32>()?` \
                       returning its `Narrowed` where the function can fail, `x.narrow_or_stop::<u32>()` with a comment \
                       stating the invariant that makes it fit, `x.truncate_to::<u8>()` where dropping the high bits is \
                       the point (hashing, codecs), `x.saturate::<u8>()` to clamp; `cast_signed()`/`cast_unsigned()` for \
                       a sign change of equal width; `blaze_int::idx(id)` for an id as an index, `blaze_int::len32(len)` \
                       for a length as a `u32`; `Cast::<S, T>::..` in a `const`";
/// What a cast is as a finding: not mechanical, since which conversion it is depends on what the
/// value can hold.
const KIND: finding::Kind = finding::Kind {
    name: "cast",
    label: "bare casts",
    rule: "76 correct-by-construction",
    instead: "the blaze-int conversion that says what happens: `widen_to`, `narrow`, `narrow_or_stop`, `truncate_to`, `saturate`",
    mechanical: false,
};
/// The list's first lines.
const HEADER: &str = "# The integer `as` casts outside test code, by crate (its directory under crates/): `as`\n\
                      # followed by a primitive integer type (`u8`..`u128`, `usize`, `i8`..`i128`, `isize`), not in a\n\
                      # `use` item, read from the text. Only shrinks; a crate with no row is held to 0, so a new crate\n\
                      # starts at 0 (AGENTS.md correct-by-construction; the conversions are crates/blaze-int's).\n\
                      # Written by scripts/src/bin/casts.rs (`{\"command\": \"write\"}`), checked in CI.\n\
                      # crate\tcasts\n";

struct Args {
    command: String,
    root: String,
    base: Option<String>,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "base"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of("command", &["check", "write", "findings"], None)?,
            root: f.str("root", Some("."))?,
            base: f.str_or_null("base")?,
        })
    }
}

/// The lines (from 1) of `text`'s casts outside its test code, in order, a line once per cast.
fn live_in(found: &FileTests, text: &str) -> Vec<usize> {
    if found.whole {
        Vec::new()
    } else {
        cast_lines(text).into_iter().filter(|line| !found.test.get(line - 1).copied().unwrap_or(false)).collect()
    }
}

/// The line (from 1) of each integer cast's `as` in `text`, in text order, once per cast, test
/// code included ([`live_in`] takes it out): comments, strings and characters skipped as
/// `blaze_script::ratchet::item_end` skips them, and a `use` item, from the word `use` to its `;`,
/// casts nothing.
fn cast_lines(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    // The line of byte `counted`, kept up to date as `at` moves on.
    let mut line = 1;
    let mut counted = 0;
    let mut in_use = false;
    let mut at = 0;
    // Bounded: every turn moves `at` forward, to the end of the text at most.
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        at = match byte {
            b'/' if next == Some(b'/') => text[at..].find('\n').map_or(bytes.len(), |n| at + n),
            b'/' if next == Some(b'*') => comment_end(bytes, at),
            b'"' => string_end(bytes, at + 1),
            b'\'' => quote_end(text, at),
            b';' => {
                in_use = false;
                at + 1
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let end = word_end(text, at);
                match &text[at..end] {
                    "use" => in_use = true,
                    "as" if !in_use && casts_to_integer(text, end) => {
                        line += text[counted..at].matches('\n').count();
                        counted = at;
                        out.push(line);
                    }
                    _ => {}
                }
                end
            }
            // A number's digits and suffix (`1u8`, `0x1F`): never the start of a word.
            b'0'..=b'9' => at + ident_len(&bytes[at..]),
            _ => at + 1,
        };
    }
    out
}

/// Whether the text after an `as` that ends at byte `from` is, after spaces and line breaks, a
/// primitive integer type's name as a whole word, not the start of a path (`u32::MAX`).
fn casts_to_integer(text: &str, from: usize) -> bool {
    let rest = text[from..].trim_start();
    let len = ident_len(rest.as_bytes());
    INTEGERS.contains(&&rest[..len]) && !rest[len..].starts_with("::")
}

/// Each crate's casts outside test code, in file and line order; a crate with none is absent.
/// Walks all of `crates/` (`CrateScope::AllDirs`), because `tools/ratchets/casts.txt` tracks
/// `harness/*` too.
fn count(tests: &Tests, root: &Path) -> BTreeMap<String, Vec<Hit<()>>> {
    ratchet::walk_with(SCRIPT, root, CrateScope::AllDirs, tests, |_| false, |file, emit| {
        for line in cast_lines(file.text) {
            emit(line, ());
        }
    })
}

fn named(it: &Hit<()>) -> String {
    format!("{}:{}", it.path, it.line)
}

/// `counts`' total, for a line of output.
fn summary(counts: &BTreeMap<String, Vec<Hit<()>>>) -> String {
    let total: usize = counts.values().map(Vec::len).sum();
    format!("{total} integer `as` casts outside tests, in {} crates", counts.len())
}

const POLICY: Policy<'static> = Policy {
    list: LIST,
    noun: "integer `as` casts",
    instead: INSTEAD,
    rule: RULE,
    at_zero: &[],
};

fn main_with(tests: &Tests, it: &Args) {
    // `Path(root).resolve()`.
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    if !root.join("crates").is_dir() {
        fail(SCRIPT, &format!("{} has no crates/ directory", root.display()));
    }
    let counts = count(tests, &root);
    if it.command == "findings" {
        let found = counts.iter().flat_map(|(krate, sites)| {
            sites.iter().map(|it| Finding { krate: krate.clone(), file: it.path.clone(), line: it.line, kind: KIND.name, detail: None })
        });
        finding::print(SCRIPT, &[KIND], found.collect());
        return;
    }
    let summary = summary(&counts);
    let run = Run { script: SCRIPT, policy: &POLICY, header: HEADER, root: &root, write: it.command == "write", base: it.base.as_deref(), summary: &summary };
    ratchet::write_or_check(&run, &counts, named);
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

/// The casts of the counted crates, as `crate: path:line, ...`, for a selftest's message.
fn shown(counts: &BTreeMap<String, Vec<Hit<()>>>) -> String {
    let crates: Vec<String> = counts
        .iter()
        .map(|(krate, sites)| {
            let at: Vec<String> = sites.iter().map(|it| format!("{}:{}", it.path, it.line)).collect();
            format!("{krate}: {}", at.join(", "))
        })
        .collect();
    crates.join("; ")
}

/// One file's casts, for a selftest's expectation.
fn sites_of(path: &str, lines: &[usize]) -> Vec<Hit<()>> {
    lines.iter().map(|line| Hit { path: path.to_owned(), line: *line, what: () }).collect()
}

/// The selftest's file of crate `a`. Line by line, what each cast is:
/// - 1, `use crate::Thing as u8;`: a rename in a `use` item, not counted;
/// - 2, `x as u32`: counted;
/// - 3, `(x as u32) as usize`: two;
/// - 4, `as char`, 5, `as f64`: not integers;
/// - 6, `<T as Into<u8>>`: a trait cast, not counted;
/// - 7, a cast in a string and in a comment: not counted;
/// - 8, one in a block comment, not counted, then `p as usize`: counted;
/// - 9, `$e as $t` in a `macro_rules!` body, not counted; `$e as i64`: counted at the template;
/// - 10, `as _`: no type written, not counted;
/// - 11-14, `x as` and `u64` on the next line: counted at 12, the `as`'s line;
/// - 15, `r#as` is an identifier, then `r#as as u16`: one;
/// - 18, in `#[cfg(test)] mod tests`: test code;
/// - 21, after `#[test]`: test code;
/// - 22, `as usize}`: counted;
/// - 23, `as core::primitive::u32`: a path, not counted (a miss DOC names); `u32::MAX as u64`:
///   counted, once.
const FIXTURE: &str = r#"use crate::Thing as u8;
fn a(x: u64) -> u32 { x as u32 }
fn b(x: u64) -> usize { (x as u32) as usize }
fn c(x: u8) -> char { x as char }
fn d(x: u32) -> f64 { x as f64 }
fn e<T: Into<u8>>(t: T) -> u8 { <T as Into<u8>>::into(t) }
const S: &str = "x as u32"; // y as u64
/* z as i8 */ fn f(p: *const u8) -> usize { p as usize }
macro_rules! m { ($e:expr, $t:ty) => { $e as $t }; ($e:expr) => { $e as i64 }; }
fn g(x: i32) -> u32 { x as _ }
fn h(x: u32) -> u64 {
    x as
        u64
}
fn raw() { let r#as = 1u8; let _ = r#as as u16; }
#[cfg(test)]
mod tests {
    fn t(x: u64) -> u8 { x as u8 }
}
#[test]
fn a_test() { let _ = 1u64 as u8; }
fn ab(x: u32) -> usize { x as usize}
fn path(x: u64) -> (u32, u64) { (x as core::primitive::u32, u32::MAX as u64) }
"#;

/// The lines of [`FIXTURE`]'s casts outside its test code.
const FIXTURE_SITES: [usize; 9] = [2, 3, 3, 8, 9, 12, 15, 22, 23];

fn selftest(tests: &Tests) {
    // What a cast is: `as` and an integer type's name as a whole word, not a path's start.
    expect(casts_to_integer(" u32 }", 0) && casts_to_integer("\n\t usize)", 0), "an integer after spaces and line breaks");
    expect(!casts_to_integer(" u32x", 0) && !casts_to_integer(" u32::MAX", 0) && !casts_to_integer(" char", 0), "not a longer word, a path or another type");

    // One file: every case of [`FIXTURE`], test code in, then out.
    let all = cast_lines(FIXTURE);
    let mut want: Vec<usize> = FIXTURE_SITES.to_vec();
    want.extend([18, 21]);
    want.sort();
    expect(all == want, &format!("the fixture's casts, test code in: found {all:?}, wanted {want:?}"));
    let live = |text: &str| live_in(&tests.of(text), text);
    let found = live(FIXTURE);
    expect(found == FIXTURE_SITES, &format!("the fixture's casts outside test code: {found:?}"));
    // Planted: the test module's attribute, then the test's, removed: each is a site.
    let planted = live(&FIXTURE.replace("#[cfg(test)]\n", "\n"));
    expect(planted.contains(&18), &format!("`t` outside test code is a site: {planted:?}"));
    let planted = live(&FIXTURE.replace("#[test]\n", "\n"));
    expect(planted.contains(&21), &format!("`a_test` without `#[test]` is a site: {planted:?}"));
    // Planted: the `use` item's `;` gone, the rename reads on into line 2 and hides its cast.
    let planted = cast_lines(&FIXTURE.replace("use crate::Thing as u8;", "use crate::Thing as u8"));
    expect(!planted.contains(&2), &format!("a `use` item casts nothing up to its `;`: {planted:?}"));
    // Planted: `as _` given its type is counted.
    let planted = cast_lines(&FIXTURE.replace("x as _", "x as u32"));
    expect(planted.contains(&10), &format!("`as u32` in place of `as _` is counted: {planted:?}"));

    // A tree: crates, test files by path and by declaration, a nested crate.
    let tmp = TempDir::new("casts-selftest-").unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
        made.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("{path} is not written: {e}")));
    };
    let one = "pub fn f(x: u64) -> u8 { x as u8 }\n";
    put("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    put("crates/a/src/lib.rs", FIXTURE);
    put("crates/a/src/live.rs", one);
    put("crates/a/src/x.rs", "#[cfg(test)]\nmod t;\n");
    put("crates/a/src/x/t.rs", one);
    put("crates/a/build.rs", one);
    put("crates/a/tests/it.rs", one);
    put("crates/a/target/debug/build/out.rs", one);
    put("crates/b/Cargo.toml", "[workspace]\n");
    put("crates/b/c/Cargo.toml", "[package]\nname = \"c\"\n");
    put("crates/b/c/src/lib.rs", one);
    let counts = count(tests, tmp.path());
    let mut a = sites_of("crates/a/build.rs", &[1]);
    a.extend(sites_of("crates/a/src/lib.rs", &FIXTURE_SITES));
    a.extend(sites_of("crates/a/src/live.rs", &[1]));
    let want = BTreeMap::from([("a".to_owned(), a), ("b/c".to_owned(), sites_of("crates/b/c/src/lib.rs", &[1]))]);
    expect(counts == want, &format!("the tree's casts: {}", shown(&counts)));

    // The ratchet. As recorded, nothing is wrong, and the list reads back as it was written.
    let rows = ratchet::parse_rows(LIST, &ratchet::render(HEADER, &counts, &[])).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    expect(rows == BTreeMap::from([("a".to_owned(), 11), ("b/c".to_owned(), 1)]), &format!("the rows read back: {rows:?}"));
    expect(ratchet::problems(&POLICY, &counts, &rows, Some(&rows), None, named).is_empty(), "the tree as recorded is right");
    // Planted: one cast added. Named by its crate, its place (given the base's diff), what to
    // write instead and the rule.
    let mut fewer = rows.clone();
    fewer.insert("a".to_owned(), 10);
    let added = BTreeMap::from([("crates/a/src/lib.rs".to_owned(), BTreeSet::from([22]))]);
    let said = ratchet::problems(&POLICY, &counts, &fewer, None, Some(&added), named);
    expect(
        said.len() == 1
            && said[0].contains("`a` has 11 integer `as` casts outside tests, over its row's 10")
            && said[0].contains("new: crates/a/src/lib.rs:22)")
            && said[0].contains("narrow_or_stop")
            && said[0].contains("correct-by-construction"),
        &format!("one cast added is caught and named: {said:?}"),
    );
    // Planted: one cast removed. The row must come down; a crate with none left goes.
    let mut more = rows.clone();
    more.insert("b/c".to_owned(), 2);
    let said = ratchet::problems(&POLICY, &counts, &more, None, None, named);
    expect(said.len() == 1 && said[0].contains("`b/c`") && said[0].contains("lower it to 1"), &format!("one cast removed is caught: {said:?}"));
    let mut gone = rows.clone();
    gone.insert("old".to_owned(), 3);
    let said = ratchet::problems(&POLICY, &counts, &gone, None, None, named);
    expect(said.len() == 1 && said[0].contains("`old`") && said[0].contains("remove the row"), &format!("a crate gone: {said:?}"));
    // Planted: a crate with no row is held to 0; a row raised above the base's is caught even
    // where the tree matches it.
    let said = ratchet::problems(&POLICY, &counts, &BTreeMap::from([("a".to_owned(), 11)]), None, None, named);
    expect(said.len() == 1 && said[0].contains("`b/c` has 1") && said[0].contains("held"), &format!("a new crate starts at 0: {said:?}"));
    let said = ratchet::problems(&POLICY, &counts, &rows, Some(&fewer), None, named);
    expect(said.len() == 1 && said[0].contains("the row `a` is 11 and was 10 at the base"), &format!("a row raised: {said:?}"));

    // The base's diff: hunks of added lines, a deleted file, a hunk that only removes.
    // The first hunk adds a line whose text starts `++ `: it is no file's header.
    let diff = "diff --git a/crates/a/src/lib.rs b/crates/a/src/lib.rs\n--- a/crates/a/src/lib.rs\n+++ b/crates/a/src/lib.rs\n\
        @@ -2,0 +3 @@ fn f() {\n+++ b/not/a/file\n@@ -9 +10,2 @@\n-old\n+y\n+z\n@@ -20 +21,0 @@\n-w\n\
        diff --git a/crates/a/src/gone.rs b/crates/a/src/gone.rs\n--- a/crates/a/src/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-v\n";
    let lines = ratchet::added_lines(diff).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    expect(
        lines == BTreeMap::from([("crates/a/src/lib.rs".to_owned(), BTreeSet::from([3, 10, 11]))]),
        &format!("the lines added: {lines:?}"),
    );
    expect(ratchet::added_lines("@@ nonsense\n").is_err_and(|e| matches!(e, BaseError::Hunk { .. })), "a hunk that is none is refused");
    expect(
        ratchet::parse_rows(LIST, "a\t1\nb 2\n").is_err_and(|e| e == ListError::NotARow { list: LIST.to_owned(), line: 2, text: "b 2".to_owned() })
            && ratchet::parse_rows(LIST, "a\t1\na\t2\n").is_err_and(|e| matches!(e, ListError::TwoRows { line: 2, .. }))
            && ratchet::parse_rows(LIST, "a\t1\t0\n").is_err()
            && ratchet::parse_rows(LIST, "a\tx\n").is_err()
            && ratchet::parse_rows(LIST, "\t1\n").is_err(),
        "a bad row is refused, naming its line",
    );
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tests = Tests::new(SCRIPT);
    match args.as_slice() {
        [one] if one == "selftest" => selftest(&tests),
        [one] => main_with(&tests, &input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pattern compiles with the regex features the lock builds, so a missing feature fails
    /// `blaze test`, not the script's first run.
    #[test]
    fn the_patterns_compile() {
        let _ = Tests::new(SCRIPT);
    }

    /// The script's own cases run under `blaze test` and `cargo test` too: one that fails ends
    /// the test process with status 1, which fails the run.
    #[test]
    fn the_selftest_holds() {
        selftest(&Tests::new(SCRIPT));
    }

    /// A rise over the crate's row is refused, naming the cast the base's diff added; the tree
    /// at its row is not.
    #[test]
    fn a_rise_is_refused_with_its_site() {
        let counts = BTreeMap::from([("s".to_owned(), sites_of("crates/s/src/lib.rs", &[3, 9]))]);
        let at_row = BTreeMap::from([("s".to_owned(), 2)]);
        assert!(ratchet::problems(&POLICY, &counts, &at_row, Some(&at_row), None, named).is_empty());
        let under = BTreeMap::from([("s".to_owned(), 1)]);
        let added = BTreeMap::from([("crates/s/src/lib.rs".to_owned(), BTreeSet::from([9]))]);
        let said = ratchet::problems(&POLICY, &counts, &under, None, Some(&added), named);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("over its row's 1") && said[0].contains("new: crates/s/src/lib.rs:9)"), "{said:?}");
    }
}
