//! The producer-string ratchet (decision D105, docs/v2/plan/design/producer-id.md "The gate"):
//! the heap's and the reach's sibling. A producer's identity is handed down from the entry
//! (`blaze_ids::producer::Producers`, a `ProducerId<P>` per producer); a version string a person
//! types and bumps ("blaze body checker 2026-11-16.8") is the class it replaces. It counts each
//! crate's typed-in producer strings in key code, outside test code (the ONE definition of test
//! code: `walk`, `scan_file`, as the count of `std` reads it), and holds each crate to its row of
//! tools/ratchets/producer-strings.txt, which only falls.
//!
//!     blaze run -q -p scripts --bin ambient -- '{"command": "producers-check", "base": "HEAD"}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "producers-write"}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "producers-sites"}'
//!
//! How a producer string is told from a domain separator and a FORMAT number, by what the text
//! is and where it goes, never by a list of known strings:
//!
//! - Only key code is read: a string literal that is the first argument of a hash call
//!   (`Hasher::new`, `ContentId::of`, `digest`, `derive_key`, `new_derive_key`, `producer`), and a
//!   `const`/`static` whose initializer is a string literal and which is a domain: its name is the
//!   first argument of such a call in the same file, or has `DOMAIN` as one of its `_` words.
//!   Comments, strings that merely hold such a call's text, and `fn` definitions of those names
//!   are not calls (one lexer pass skips them).
//! - A FORMAT number is an integer constant (`const FORM: u32 = 4`, a magic): never read.
//! - A domain separator names its kind of key and at most one layout number ("blaze-drive task
//!   key 1", "blaze v2 lower body key 1"): not counted.
//! - A producer string is any of:
//!   - a literal passed to `producer(..)` (blaze-units' `Query::PRODUCER`: "the number raised
//!     with every change to what the query computes", by its own doc);
//!   - a date in the text (`dddd-dd-dd`: "blaze body checker 2026-11-16.8");
//!   - two or more numbers, each a whole word of the text split at spaces, with a trailing `,`
//!     `.` `;` `:` or `)` and a leading `(` taken off ("blaze v2 lower body 10, parser 1"): a key's
//!     layout is one number, a second is a producer's version beside it (`v2`, `x86-64`, `blake3`
//!     are words, not numbers);
//!   - a domain constant named for code, in a file that hashes: one of its `_` words is
//!     `CHECKER`, `EXPANDER`, `PRODUCER`, `LOWERING`, `SOLVER` or `BUILDER` (`SOLVER_DOMAIN`). The
//!     same name in a file that never hashes (DWARF's `PRODUCER = b"blaze\0"`) is not key code.
//!
//! What it cannot see, said: a domain with ONE number a person bumps for a code change (blaze-scope
//! `crate_scope.rs` `const VERSION: u32 = 2` with its "// 2: ..." log is the integer form). A
//! number's history (a `/// 3: ...` log above it) does not tell code from layout: the record
//! forms log their layout changes the same way, so a rule on it would count every form. That
//! class is closed by the type (a result key takes a `ProducerId<P>`, decision D105), not here.

use super::heap::{
    ColumnSpec, RatchetCounts, RatchetSite, TableListError, TablePolicy, added_since, check_table_problems, load_table_rows,
    parse_table_rows, render_table, report_problems, write_table,
};
use super::*;

/// What ambient.rs's `DOC` says of the producer-string ratchet, appended to it in the usage error.
pub(super) const PRODUCERS_DOC: &str = r#"
The producer-string ratchet (decision D105, docs/v2/plan/design/producer-id.md): a producer's
identity is a `blaze_ids::producer::ProducerId<P>` from the table the entry makes, never a
version string a person bumps. Its own list tools/ratchets/producer-strings.txt, rows
`<crate><TAB><count>`, each the typed-in producer strings in key code outside test code: a hash
call's literal domain or a domain constant (one passed to `Hasher::new`, `ContentId::of`,
`digest`, `derive_key`, `new_derive_key` or `producer` in its file, or named `*DOMAIN*`) that is
  callee     a literal passed to `producer(..)` (a query's producer version)
  date       a date in its text ("blaze body checker 2026-11-16.8")
  numbers    two or more whole-word numbers ("blaze v2 lower body 10, parser 1")
  named      a constant named for code (`CHECKER`, `EXPANDER`, `PRODUCER`, `LOWERING`, `SOLVER`,
             `BUILDER` as one of its `_` words) in a file that hashes
A domain separator (a kind of key and at most one layout number: "blaze-drive task key 1") and an
integer FORMAT constant are never counted.
  producers-check  each crate at or under its row; with "base", an over names the added lines
  producers-write  rewrite the list from the tree (refused when a crate rose: it only falls)
  producers-sites  every counted string outside tests, `crate<TAB>path:line<TAB>why<TAB>text`
"#;

/// The list, from the root.
pub(super) const PRODUCERS_LIST: &str = "tools/ratchets/producer-strings.txt";

const PRODUCERS_HEADER: &str = "# The typed-in producer strings in key code (decision D105), outside test code, by crate (its\n\
                                # directory under crates/): a hash domain with a date, two or more numbers, a name for\n\
                                # code, or a literal passed to `producer(..)`. Only falls; a crate with no row is held to 0.\n\
                                # The way down: the producer's id from the entry's table (`blaze_ids::producer`).\n\
                                # Written by scripts/src/bin/ambient.rs (`{\"command\": \"producers-write\"}`), checked by\n\
                                # `{\"command\": \"producers-check\"}`.\n\
                                # crate\tcount\n";

/// What a refusal tells the writer to do instead.
const INSTEAD: &str = "take the producer's id from the table the entry hands down (`blaze_ids::producer::ProducerId`, \
                       docs/v2/plan/design/producer-id.md); a domain separator names its key and at most one layout number";

/// The functions whose first argument is a hash domain, called by name (a path's last segment).
const HASH_CALLS: [&str; 4] = ["digest", "derive_key", "new_derive_key", "producer"];
/// The same, called by a two-segment path: `Hasher::new`, `ContentId::of`.
const HASH_PATHS: [(&str, &str); 2] = [("Hasher", "new"), ("ContentId", "of")];
/// The `_` words that name a constant for the code that made a value.
const CODE_WORDS: [&str; 6] = ["CHECKER", "EXPANDER", "PRODUCER", "LOWERING", "SOLVER", "BUILDER"];

/// Why a string is a producer's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Why {
    /// A literal passed to `producer(..)`.
    Callee,
    /// A `dddd-dd-dd` date in the text.
    Dated,
    /// Two or more whole-word numbers in the text.
    Numbers,
    /// A domain constant named for code, in a file that hashes.
    Named,
}

impl Why {
    fn name(self) -> &'static str {
        match self {
            Why::Callee => "callee",
            Why::Dated => "date",
            Why::Numbers => "numbers",
            Why::Named => "named",
        }
    }

    fn what(self) -> &'static str {
        match self {
            Why::Callee => "a query's producer version",
            Why::Dated => "a date",
            Why::Numbers => "two or more numbers",
            Why::Named => "a constant named for code",
        }
    }
}

/// One typed-in producer string: why it is one, and its text as written (escapes kept).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ProducerUse {
    pub(super) why: Why,
    pub(super) text: String,
}

impl ProducerUse {
    fn shown(&self) -> String {
        format!("\"{}\" ({})", self.text, self.why.what())
    }
}

/// One token of Rust text, comments and character literals skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok<'t> {
    /// An identifier, keyword or number.
    Word(&'t str),
    /// A string literal's text between its quotes (any of `"`, `b"`, `r#"`, `br"`, `c"`).
    Str(&'t str),
    /// One ASCII punctuation byte.
    Punct(u8),
}

/// The text of a literal whose text starts at `from` and whose closing quote and `hashes` hashes
/// end just before `end`; empty for one cut off at the end of the file.
fn inner(text: &str, from: usize, end: usize, hashes: usize) -> &str {
    text.get(from..end.saturating_sub(1 + hashes).max(from)).unwrap_or("")
}

/// `text`'s tokens with the line (from 1) each starts on.
fn tokens(text: &str) -> Vec<(usize, Tok<'_>)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    // The line of byte `counted`, kept up to date as `at` moves on.
    let (mut line, mut counted, mut at) = (1, 0, 0);
    // Bounded: every turn moves `at` forward, to the end of the text at most.
    while at < bytes.len() {
        line += bytes[counted..at].iter().filter(|b| **b == b'\n').count();
        counted = at;
        let next = bytes.get(at + 1).copied();
        let (tok, end) = match bytes[at] {
            b'/' if next == Some(b'/') => (None, text[at..].find('\n').map_or(bytes.len(), |n| at + n)),
            b'/' if next == Some(b'*') => (None, comment_end(bytes, at)),
            b'"' => {
                let end = string_end(bytes, at + 1);
                (Some(Tok::Str(inner(text, at + 1, end, 0))), end)
            }
            b'\'' => (None, quote_end(text, at)),
            b'_' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => {
                let stop = at + ident_len(&bytes[at..]);
                let end = word_end(text, at).max(stop);
                if end == stop {
                    (Some(Tok::Word(&text[at..end])), end)
                } else {
                    // A prefixed literal (`b"`, `r#"`, ...), a byte character (`b'x'`) or a raw
                    // identifier (`r#type`).
                    let hashes = bytes[stop..end].iter().take_while(|b| **b == b'#').count();
                    let open = stop + hashes;
                    match bytes.get(open) {
                        Some(b'"') => (Some(Tok::Str(inner(text, open + 1, end, hashes))), end),
                        Some(b'\'') => (None, end),
                        _ => (Some(Tok::Word(&text[open..end])), end),
                    }
                }
            }
            b if b.is_ascii_whitespace() => (None, at + 1),
            b if b.is_ascii() => (Some(Tok::Punct(b)), at + 1),
            _ => (None, at + text[at..].chars().next().map_or(1, char::len_utf8)),
        };
        if let Some(tok) = tok {
            out.push((line, tok));
        }
        at = end.max(at + 1);
    }
    out
}

/// A constant's name: ASCII capitals, digits and `_`, starting with a capital.
fn constant_cased(word: &str) -> bool {
    word.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
        && word.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Whether one of `name`'s `_` words is `word`.
fn has_word(name: &str, words: &[&str]) -> bool {
    name.split('_').any(|it| words.contains(&it))
}

/// Whether `text` holds a `dddd-dd-dd` date.
fn dated(text: &str) -> bool {
    let shape = *b"dddd-dd-dd";
    text.as_bytes().windows(shape.len()).any(|w| w.iter().zip(shape).all(|(b, s)| if s == b'd' { b.is_ascii_digit() } else { *b == s }))
}

/// The whole-word numbers in `text`: words split at whitespace, a trailing `,.;:)` and a leading
/// `(` taken off, all digits.
fn numbers(text: &str) -> usize {
    fn word(it: &str) -> &str {
        it.trim_start_matches('(').trim_end_matches([',', '.', ';', ':', ')'])
    }
    text.split_ascii_whitespace().map(word).filter(|it| !it.is_empty() && it.bytes().all(|b| b.is_ascii_digit())).count()
}

/// Why the text alone makes a string a producer's, if it does.
fn by_text(text: &str) -> Option<Why> {
    if dated(text) {
        Some(Why::Dated)
    } else if numbers(text) >= 2 {
        Some(Why::Numbers)
    } else {
        None
    }
}

/// The hash call whose name is at token `at`: its name and the index of its `(`. Not a `fn`'s
/// definition of that name.
fn call_at(toks: &[(usize, Tok<'_>)], at: usize) -> Option<(&'static str, usize)> {
    let tok = |i: usize| toks.get(i).map(|(_, it)| *it);
    let Some(Tok::Word(word)) = tok(at) else { return None };
    if at > 0 && tok(at - 1) == Some(Tok::Word("fn")) {
        return None;
    }
    if let Some(name) = HASH_CALLS.iter().find(|it| **it == word) {
        return (tok(at + 1) == Some(Tok::Punct(b'('))).then_some((*name, at + 1));
    }
    let &(head, last) = HASH_PATHS.iter().find(|(head, _)| *head == word)?;
    let path = tok(at + 1) == Some(Tok::Punct(b':'))
        && tok(at + 2) == Some(Tok::Punct(b':'))
        && tok(at + 3) == Some(Tok::Word(last))
        && tok(at + 4) == Some(Tok::Punct(b'('));
    path.then_some((head, at + 4))
}

/// The typed-in producer strings of one file's text, each with the line (from 1) its text starts
/// on, in line order. The rule: this module's doc.
pub(super) fn producer_uses(text: &str) -> Vec<(usize, ProducerUse)> {
    let toks = tokens(text);
    let tok = |i: usize| toks.get(i).map(|(_, it)| *it);
    // The file hashes: some hash call is in it.
    let mut hashes = false;
    // The constants named as a hash call's first argument.
    let mut domains: BTreeSet<&str> = BTreeSet::new();
    let mut out = Vec::new();
    // Bounded: one turn per token.
    for at in 0..toks.len() {
        let Some((name, open)) = call_at(&toks, at) else { continue };
        hashes = true;
        match tok(open + 1) {
            Some(Tok::Str(text)) => {
                let why = if name == "producer" { Some(Why::Callee) } else { by_text(text) };
                if let Some(why) = why {
                    out.push((toks[open + 1].0, ProducerUse { why, text: text.to_owned() }));
                }
            }
            Some(Tok::Word(_)) => {
                // A path to a constant (`DOMAIN`, `Self::DOMAIN`, `key::DOMAIN`) ended by `,` or `)`.
                let mut end = open + 1;
                // Bounded: `end` moves forward over the path's tokens, to the end at most.
                while matches!(tok(end), Some(Tok::Word(_) | Tok::Punct(b':'))) {
                    end += 1;
                }
                if let (Some(Tok::Word(last)), Some(Tok::Punct(b',' | b')'))) = (tok(end - 1), tok(end))
                    && constant_cased(last)
                {
                    domains.insert(last);
                }
            }
            _ => {}
        }
    }
    // The string constants: `const|static [mut] NAME: <type> = "<text>"`.
    for at in 0..toks.len() {
        if !matches!(tok(at), Some(Tok::Word("const" | "static"))) {
            continue;
        }
        let named = if tok(at + 1) == Some(Tok::Word("mut")) { at + 2 } else { at + 1 };
        let (Some(Tok::Word(name)), Some(Tok::Punct(b':'))) = (tok(named), tok(named + 1)) else { continue };
        if !constant_cased(name) {
            continue;
        }
        // The type runs to the `=` (a type holds none); a `;` first is a declaration alone.
        let Some(eq) = (named + 2..toks.len()).find(|i| matches!(tok(*i), Some(Tok::Punct(b'=' | b';')))) else { continue };
        let Some(Tok::Str(text)) = (tok(eq) == Some(Tok::Punct(b'='))).then(|| tok(eq + 1)).flatten() else { continue };
        let code = hashes && has_word(name, &CODE_WORDS);
        if !(code || domains.contains(name) || has_word(name, &["DOMAIN"])) {
            continue;
        }
        if let Some(why) = by_text(text).or(code.then_some(Why::Named)) {
            out.push((toks[eq + 1].0, ProducerUse { why, text: text.to_owned() }));
        }
    }
    out.sort();
    out
}

/// One producer string in the tree.
type ProducerSite = RatchetSite<ProducerUse>;

/// Each crate's producer strings outside test code and in it, in file and line order.
type ProducerCounts = RatchetCounts<ProducerUse>;

fn count_producers(root: &Path) -> Result<ProducerCounts, TreeError> {
    let Walked { scanned, skipped, .. } = walk(root)?;
    let mut out: ProducerCounts = BTreeMap::new();
    let mut manifests: BTreeMap<Vec<String>, bool> = BTreeMap::new();
    for (parts, found) in scanned {
        if found.producers.is_empty() && found.test_producers.is_empty() {
            continue;
        }
        let path = parts.join("/");
        let whole_test = skipped.contains(&path);
        let krate = crate_of(root, &parts, &mut manifests);
        let (outside, inside) = out.entry(krate).or_default();
        for (line, what) in found.producers {
            let site = ProducerSite { path: path.clone(), line, what };
            if whole_test { inside.push(site) } else { outside.push(site) }
        }
        inside.extend(found.test_producers.into_iter().map(|(line, what)| ProducerSite { path: path.clone(), line, what }));
    }
    for (outside, inside) in out.values_mut() {
        outside.sort();
        inside.sort();
    }
    Ok(out)
}

/// The list as `producers-write` writes it: the header, then a row for each crate with a
/// producer string outside test code, sorted.
fn render_producers(counts: &ProducerCounts) -> String {
    render_table(PRODUCERS_HEADER, counts, |sites| [sites.len()])
}

static PRODUCERS_POLICY: TablePolicy<1> = TablePolicy {
    list: PRODUCERS_LIST,
    scope: "outside tests",
    moved_noun: "string",
    rule: "decision D105",
    write_cmd: "producers-write",
    columns: [ColumnSpec { name: "count", what: "typed-in producer strings", instead: INSTEAD }],
};

fn parse_producer_rows(text: &str) -> Result<BTreeMap<String, usize>, TableListError> {
    parse_table_rows(&PRODUCERS_POLICY, text).map(|rows| rows.into_iter().map(|(k, [n])| (k, n)).collect())
}

fn producer_problems_rows(
    counts: &ProducerCounts,
    rows: &BTreeMap<String, [usize; 1]>,
    added: Option<&BTreeMap<String, BTreeSet<usize>>>,
) -> (Vec<String>, Vec<String>) {
    check_table_problems(
        &PRODUCERS_POLICY,
        counts,
        rows,
        added,
        |_| 0,
        |it| format!("{}:{} {}", it.path, it.line, it.what.shown()),
    )
}

/// What is over its row (each a refusal, the line `producers-check` prints after `ambient: `) and
/// what is under it (each a note).
fn producer_problems(
    counts: &ProducerCounts,
    rows: &BTreeMap<String, usize>,
    added: Option<&BTreeMap<String, BTreeSet<usize>>>,
) -> (Vec<String>, Vec<String>) {
    let rows: BTreeMap<String, [usize; 1]> = rows.iter().map(|(k, n)| (k.clone(), [*n])).collect();
    producer_problems_rows(counts, &rows, added)
}

pub(super) fn main_with(command: &str, base: Option<&str>, root: &Path) {
    let counts = count_producers(root).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let total: usize = counts.values().map(|(outside, _)| outside.len()).sum();
    let in_tests: usize = counts.values().map(|(_, inside)| inside.len()).sum();
    if command == "producers-sites" {
        for (krate, (outside, _)) in &counts {
            for it in outside {
                println!("{krate}\t{}:{}\t{}\t{}", it.path, it.line, it.what.why.name(), it.what.text);
            }
        }
        return;
    }
    let (listed, rows) = load_table_rows(&PRODUCERS_POLICY, root, command == "producers-write");
    if command == "producers-write" {
        // The first write takes the tree as it stands; after it, rows only fall.
        let over = if listed.is_some() { producer_problems_rows(&counts, &rows, None).0 } else { Vec::new() };
        write_table(PRODUCERS_LIST, root, &over, &render_producers(&counts));
        println!("{SCRIPT}: wrote {PRODUCERS_LIST}: {total} producer strings outside tests ({in_tests} in tests, not held)");
        return;
    }
    let added = base.map(|base| added_since(root, base).unwrap_or_else(|e| fail(SCRIPT, &e.to_string())));
    let (over, under) = producer_problems_rows(&counts, &rows, added.as_ref());
    report_problems(&over, &under);
    println!("{SCRIPT}: {total} producer strings outside tests, each crate at or under its row in {PRODUCERS_LIST}");
}

/// The cases of the producer-string count, for the script's selftest: each of today's forms
/// counted, each domain separator and FORMAT number not; then the tool on a tree, a planted
/// checker string caught with the one line `producers-check` prints for it (after `ambient: `):
///
///     `a` has 1 typed-in producer strings outside tests, over its row 0 in
///     tools/ratchets/producer-strings.txt (new: crates/a/src/plant.rs:2 "blaze body checker
///     2026-11-16.8" (a date)): take the producer's id ... (decision D105)
///
/// and, the plant taken out again, no line.
pub(super) fn selftest() {
    let at =|line: usize, why: Why, text: &str| (line, ProducerUse { why, text: text.to_owned() });

    // The words of the rule.
    expect(dated("blaze body checker 2026-11-16.8") && !dated("blaze v2 1-2-3"), "a date is `dddd-dd-dd`");
    expect(
        numbers("blaze v2 lower body 10, parser 1") == 2
            && numbers("blaze target x86-64 code 1") == 1
            && numbers("blake3 (2) u64") == 1
            && numbers("blaze-drive task key 1") == 1,
        "numbers are whole words",
    );

    // Each of today's forms (2026-11-16) counted; each domain separator, FORMAT number, string
    // holding a call's text, comment and definition not.
    let text = "// const OLD_DOMAIN: &str = \"blaze body checker 2026-11-16.8\";\n\
                const KEY_CONTEXT: &str = \"blaze-cond 2026-11-15 cfg variable key\";\n\
                fn k() -> H { blaze_blake3::Hasher::new_derive_key(KEY_CONTEXT) }\n\
                const SOLVER_DOMAIN: &str = \"blaze v2 goal 3 solver\";\n\
                const TASK_DOMAIN: &str = \"blaze-drive task key 1\";\n\
                fn a(x: &[u8]) -> ContentId { ContentId::of(\"blaze v2 lower body 10, parser 1\", x) }\n\
                fn b(x: &[u8]) -> ContentId { blaze_ids::ContentId::of(\"blaze-cli kind tool 1\", x) }\n\
                impl Query for Plan { const PRODUCER: ContentId = producer(\"blaze v2 cargo plan 10\"); }\n\
                const FORM: u32 = 4;\n\
                pub const VERSION: &str = \"rustc 1.101.0-nightly (16d1ffac1 2026-10-26)\";\n\
                const SAID: &str = \"Hasher::new(\\\"blaze 2026-11-16 x\\\")\";\n\
                const NEXT_DOMAIN: &str =\n    r#\"blaze lower 3 parser 2\"#;\n\
                pub const fn producer(what: &str) -> ContentId { schema(what) }\n\
                fn c(x: &[u8]) -> ContentId { let mut h = Hasher::new(Self::BITS_DOMAIN); h.part(x); h.finish() }\n";
    let found = producer_uses(text);
    let want = vec![
        at(2, Why::Dated, "blaze-cond 2026-11-15 cfg variable key"),
        at(4, Why::Named, "blaze v2 goal 3 solver"),
        at(6, Why::Numbers, "blaze v2 lower body 10, parser 1"),
        at(8, Why::Callee, "blaze v2 cargo plan 10"),
        at(13, Why::Numbers, "blaze lower 3 parser 2"),
    ];
    expect(found == want, &format!("the producer strings read: {found:?}"));
    // A code-word name in a file that never hashes is not key code (DWARF's producer name).
    let dwarf = "pub(crate) const PRODUCER: &[u8] = b\"blaze\\0\";\nconst CODEVIEW_PRODUCER: &str = \"blaze 1 2\";\n";
    let found = producer_uses(dwarf);
    expect(found.is_empty(), &format!("no hashing, no key code: {found:?}"));

    // The list: over is refused naming the added line; under is a note; it reads back.
    let site = |line| ProducerSite { path: "crates/a/src/lib.rs".to_owned(), line, what: at(0, Why::Dated, "x 2026-01-01").1 };
    let counts: ProducerCounts = BTreeMap::from([("a".to_owned(), (vec![site(3), site(9)], Vec::new()))]);
    let added = BTreeMap::from([("crates/a/src/lib.rs".to_owned(), BTreeSet::from([9]))]);
    let (over, under) = producer_problems(&counts, &BTreeMap::from([("a".to_owned(), 1)]), Some(&added));
    expect(
        over.len() == 1 && over[0].contains("(new: crates/a/src/lib.rs:9 \"x 2026-01-01\" (a date))") && under.is_empty(),
        &format!("one over its row, named: {over:?}"),
    );
    let (over, under) = producer_problems(&counts, &BTreeMap::from([("a".to_owned(), 3)]), None);
    expect(over.is_empty() && under.len() == 1, &format!("under its row is a note: {over:?} {under:?}"));
    expect(parse_producer_rows(&render_producers(&counts)) == Ok(BTreeMap::from([("a".to_owned(), 2)])), "the list reads back");
    expect(
        parse_producer_rows("a\n").is_err() && parse_producer_rows("a\t1\na\t1\n").is_err() && parse_producer_rows("a\tx\n").is_err(),
        "a bad row is refused",
    );

    // The tool itself, on a tree: a crate whose list holds it passes (its domain separator and
    // its test's dated string are not held); a planted checker string is caught with the one line
    // below; the plant removed, it passes again.
    let tmp = TempDir::new("ambient-producers-selftest-").unwrap_or_else(|e| failed(&e));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
        made.unwrap_or_else(|e| failed(&format!("{path} is not written: {e}")));
    };
    put("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    put("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    put(
        "crates/a/src/lib.rs",
        "const TASK_DOMAIN: &str = \"a task key 1\";\npub fn key(x: &[u8]) -> ContentId {\n    ContentId::of(TASK_DOMAIN, x)\n}\n\
         #[cfg(test)]\nmod tests {\n    fn t() { ContentId::of(\"a test 2026-11-16 key\", b\"\"); }\n}\n",
    );
    let tree = count_producers(tmp.path()).unwrap_or_else(|e| failed(&e));
    let rows = tree.get("a").map(|(outside, inside)| (outside.len(), inside.len()));
    expect(rows == Some((0, 1)), &format!("the tree's rows (the test's string not held): {rows:?}"));
    let held = parse_producer_rows(&render_producers(&tree)).unwrap_or_else(|e| failed(&e));
    let (over, _) = producer_problems(&tree, &held, None);
    expect(over.is_empty(), &format!("the tree at its rows: {over:?}"));
    put(
        "crates/a/src/plant.rs",
        "pub fn checker_tool(id: &[u8]) -> ContentId {\n    const CHECKER: &str = \"blaze body checker 2026-11-16.8\";\n    ContentId::of(CHECKER, id)\n}\n",
    );
    let planted = count_producers(tmp.path()).unwrap_or_else(|e| failed(&e));
    let added = BTreeMap::from([("crates/a/src/plant.rs".to_owned(), BTreeSet::from([1, 2, 3, 4]))]);
    let (over, _) = producer_problems(&planted, &held, Some(&added));
    let want = format!(
        "`a` has 1 typed-in producer strings outside tests, over its row 0 in tools/ratchets/producer-strings.txt \
         (new: crates/a/src/plant.rs:2 \"blaze body checker 2026-11-16.8\" (a date)): {INSTEAD} (decision D105)"
    );
    expect(over == vec![want], &format!("the plant is caught with its line: {over:?}"));
    if let Err(e) = std::fs::remove_file(tmp.path().join("crates/a/src/plant.rs")) {
        failed(&format!("the plant is not removed: {e}"));
    }
    let unplanted = count_producers(tmp.path()).unwrap_or_else(|e| failed(&e));
    let (over, _) = producer_problems(&unplanted, &held, Some(&added));
    expect(over.is_empty(), &format!("the plant removed, nothing is over: {over:?}"));
}
