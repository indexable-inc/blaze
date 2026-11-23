//! The heap's ratchet (decision D102, docs/v2/plan/decisions.json: the allocator is a
//! capability handed down from the program's entry, rule 75 "nothing ambient"). It counts each
//! crate's calls that reach the process's heap with no caller handing it, outside test code
//! (the ONE definition of test code: `walk`, `scan_file`, as the count of `std` reads it), and
//! holds each crate to its row of tools/ratchets/alloc-global.txt, which only falls.
//!
//!     blaze run -q -p scripts --bin ambient -- '{"command": "heap-check", "base": "HEAD"}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "heap-write"}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "heap-report"}'

use super::*;

/// What ambient.rs's `DOC` says of the heap's ratchet, appended to it in the usage error.
pub(super) const HEAP_DOC: &str = r#"
The heap's ratchet (decision D102): the global allocator is a capability (`blaze_own::Heap`)
the program's entry takes and hands down. Its own list tools/ratchets/alloc-global.txt, rows
`<crate><TAB>` and five columns, each a count of calls outside test code (the last in it too):
  global         `Alloc::global()`: the heap from nowhere (it goes, D102 step 3)
  shared_global  `SharedAlloc::global()`: the same, the handle that crosses threads
  test_heap      `testing::heap()`: a test's root, named outside a test: a bug
  process        `Heap::process()`: the program's entry's; anywhere else, a bug
  leak           `Root::leak(`: a root heap (blaze_mem::Root) made `'static` for good, for
                 a caller that still demands `Alloc<'static>`; counted in test code too, so it
                 falls to 0 as those callers take a lifetime
A root made at a test's or an entry's top (`Root::new(`) is the design and is not counted.
A call is read from the text as written (`Alloc::global(`, with or without a path before it),
comments and strings skipped; a call inside a macro's arguments counts.

    command   heap-check    each crate at or under its row; one over is refused, naming its
                            sites (the new ones given `base`); one under is said, to be
                            lowered by `heap-write`
              heap-write    writes the list from the tree; a row never rises (each column the
                            lower of row and count), so a crate over its row is named and
                            stays over; a raise is by hand, with a `#` comment above the row
                            saying why, which every write keeps
              heap-report   each crate's calls outside test code and in it, by column, the
                            largest first: the migration's table
              heap-sites    each call outside test code: crate, path:line, column
"#;

/// The list, from the root.
pub(super) const HEAP_LIST: &str = "tools/ratchets/alloc-global.txt";

const HEAP_HEADER: &str = "# The calls that reach the process's heap with no caller handing it (decision D102, rule 75),\n\
                           # outside test code, by crate (its directory under crates/): `Alloc::global()`,\n\
                           # `SharedAlloc::global()`, `testing::heap()` (a test's root) and `Heap::process()` (the\n\
                           # program's entry's); and `Root::leak(` (a root heap made `'static`), in test code too.\n\
                           # Only falls; a crate with no row is held to 0 in every column.\n\
                           # Written by scripts/src/bin/ambient.rs (`{\"command\": \"heap-write\"}`), checked by\n\
                           # `{\"command\": \"heap-check\"}`.\n\
                           # crate\tglobal\tshared_global\ttest_heap\tprocess\tleak\n";

/// One call that reaches the heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum HeapUse {
    Global,
    SharedGlobal,
    TestHeap,
    Process,
    /// `Root::leak(..)`: a root heap made the process's for good. Counted in test code as
    /// outside it ([`count_heap`]): the one named form for a caller that still demands
    /// `'static` handles, driven to 0.
    Leak,
    /// `Heap::process()` inside a top-level `fn main`: in an entry file (an example, a
    /// `src/bin/*.rs`, a `src/main.rs`) the program's entry, which is free; elsewhere it counts
    /// as [`HeapUse::Process`] ([`count_heap`] decides, by path). Never a column.
    MainProcess,
}

const USES: [HeapUse; 4] = [HeapUse::Global, HeapUse::SharedGlobal, HeapUse::TestHeap, HeapUse::Process];

/// Every column, in order: [`USES`], then the leaked root's.
const COLUMNS: [HeapUse; 5] = [USES[0], USES[1], USES[2], USES[3], HeapUse::Leak];

/// A crate's counts, by [`HeapUse`]'s column.
type HeapRow = [usize; 5];

impl HeapUse {
    const fn column(self) -> usize {
        self as usize
    }

    const fn name(self) -> &'static str {
        match self {
            HeapUse::Global => "global",
            HeapUse::SharedGlobal => "shared_global",
            HeapUse::TestHeap => "test_heap",
            HeapUse::Process | HeapUse::MainProcess => "process",
            HeapUse::Leak => "leak",
        }
    }

    const fn what(self) -> &'static str {
        match self {
            HeapUse::Global => "calls of `Alloc::global()`",
            HeapUse::SharedGlobal => "calls of `SharedAlloc::global()`",
            HeapUse::TestHeap => "calls of `testing::heap()`",
            HeapUse::Process | HeapUse::MainProcess => "calls of `Heap::process()` outside an entry's `fn main`",
            HeapUse::Leak => "calls of `Root::leak(..)` (test code included)",
        }
    }

    const fn instead(self) -> &'static str {
        match self {
            HeapUse::Global | HeapUse::SharedGlobal => {
                "take `alloc: Alloc<'_>` (or `SharedAlloc<'_>`, or `&Heap`) from the caller, up to the program's entry, \
                 which takes `blaze_own::Heap::process()`; in a test, `blaze_own::testing::heap().alloc()`"
            }
            HeapUse::TestHeap => "a test's root is for test code only: take the allocator from the caller",
            HeapUse::Process | HeapUse::MainProcess => {
                "only the program's entry takes the heap, in its `fn main`: take `heap: &'static Heap` (or an `Alloc<'_>`) from the caller"
            }
            HeapUse::Leak => {
                "make the root at the top (`let root = Root::new(..)?;`) and hand down `root.alloc()`; give the callee that \
                 demands `Alloc<'static>` a lifetime instead"
            }
        }
    }
}

/// Whether the file `path` is a program's entry file: an example, a `src/bin` file, a
/// `src/main.rs`.
pub(super) fn entry_file(path: &str) -> bool {
    path.contains("/examples/") || path.contains("/src/bin/") || path.ends_with("/src/main.rs")
}

/// The heap use the word at bytes `at..end` of `text` is, read from the path before it and the
/// `(` after it.
fn heap_use_of(text: &str, at: usize, end: usize) -> Option<HeapUse> {
    if !text[end..].trim_start().starts_with('(') {
        return None;
    }
    let prev = text[..at].trim_end().strip_suffix("::").map(trailing_word)?;
    Some(match (prev, &text[at..end]) {
        ("Alloc", "global") => HeapUse::Global,
        ("SharedAlloc", "global") => HeapUse::SharedGlobal,
        ("testing", "heap") => HeapUse::TestHeap,
        ("Heap", "process") => HeapUse::Process,
        ("Root", "leak") => HeapUse::Leak,
        _ => return None,
    })
}

/// Each heap use in `text`, test code included ([`scan_file`] splits it): its line (from 1)
/// and what it is, in order. Comments, strings and characters are skipped as [`item_end`]
/// skips them.
pub(super) fn heap_uses(text: &str) -> Vec<(usize, HeapUse)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    // The line of byte `counted`, kept up to date as `at` moves on.
    let mut line = 1;
    let mut counted = 0;
    let mut at = 0;
    // The bytes of a top-level `fn main` (its `fn` at a line's start, or after `pub `).
    let mut main: Option<(usize, usize)> = None;
    // Bounded: every turn moves `at` forward, to the end of the text at most.
    while at < bytes.len() {
        let next = bytes.get(at + 1).copied();
        at = match bytes[at] {
            b'/' if next == Some(b'/') => text[at..].find('\n').map_or(bytes.len(), |n| at + n),
            b'/' if next == Some(b'*') => comment_end(bytes, at),
            b'"' => string_end(bytes, at + 1),
            b'\'' => quote_end(text, at),
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let end = word_end(text, at);
                if main.is_none() && &text[at..end] == "main" && trailing_word(&text[..at]) == "fn" {
                    let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
                    let lead = &text[line_start..at];
                    if lead == "fn " || lead == "pub fn " {
                        main = item_end(text, at).map(|stop| (at, stop));
                    }
                }
                if let Some(mut found) = heap_use_of(text, at, end) {
                    if found == HeapUse::Process && main.is_some_and(|(from, to)| from < at && at < to) {
                        found = HeapUse::MainProcess;
                    }
                    line += text[counted..at].matches('\n').count();
                    counted = at;
                    out.push((line, found));
                }
                end
            }
            b'0'..=b'9' => at + ident_len(&bytes[at..]),
            _ => at + 1,
        };
    }
    out
}

/// One counted site in the tree (shared by the heap, reach and producer-string ratchets).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct RatchetSite<T> {
    /// Relative to the root.
    pub(super) path: String,
    pub(super) line: usize,
    pub(super) what: T,
}

/// Each crate's sites outside test code and in it, in file and line order.
pub(super) type RatchetCounts<T> = BTreeMap<String, (Vec<RatchetSite<T>>, Vec<RatchetSite<T>>)>;

/// Counts `sites` into an `N`-column row by `column_of`.
pub(super) fn tally_table<T, const N: usize>(sites: &[RatchetSite<T>], column_of: impl Fn(&T) -> usize) -> [usize; N] {
    let mut row = [0; N];
    for it in sites {
        row[column_of(&it.what)] += 1;
    }
    row
}

/// Formats an `N`-column row as tab-separated integers.
pub(super) fn row_text<const N: usize>(row: [usize; N]) -> String {
    row.map(|n| n.to_string()).join("\t")
}

/// Renders a ratchet list: `header`, then `<crate><TAB><cols>` for each crate with a non-zero row.
pub(super) fn render_table<T, const N: usize>(
    header: &str,
    counts: &RatchetCounts<T>,
    tally: impl Fn(&[RatchetSite<T>]) -> [usize; N],
) -> String {
    let mut out = header.to_owned();
    for (krate, (outside, _)) in counts {
        let row = tally(outside);
        if row != [0; N] {
            out.push_str(&format!("{krate}\t{}\n", row_text(row)));
        }
    }
    out
}

/// A row of a tab-separated ratchet list that is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TableListError {
    NotARow { list: &'static str, columns: &'static [ColumnSpec], line: usize, text: String },
    TwoRows { list: &'static str, line: usize, krate: String },
}

impl fmt::Display for TableListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableListError::NotARow { list, columns, line, text } => {
                write!(f, "{list}:{line}: `{text}` is not `<crate>")?;
                for col in *columns {
                    write!(f, "<TAB><{}>", col.name)?;
                }
                write!(f, "`")
            }
            TableListError::TwoRows { list, line, krate } => write!(f, "{list}:{line}: a second row for `{krate}`"),
        }
    }
}

/// Parses a tab-separated `<crate><TAB><col_0>...<TAB><col_{N-1}>` ratchet list.
pub(super) fn parse_table_rows<const N: usize>(
    policy: &'static TablePolicy<N>,
    text: &str,
) -> Result<BTreeMap<String, [usize; N]>, TableListError> {
    let mut rows = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || TableListError::NotARow { list: policy.list, columns: &policy.columns, line: at + 1, text: line.to_owned() };
        let cells: Vec<&str> = line.split('\t').collect();
        let [krate, rest @ ..] = cells.as_slice() else { return Err(bad()) };
        if krate.is_empty() || rest.len() != N {
            return Err(bad());
        }
        let mut row = [0; N];
        for (cell, it) in row.iter_mut().zip(rest) {
            *cell = it.parse().map_err(|_| bad())?;
        }
        if rows.insert((*krate).to_owned(), row).is_some() {
            return Err(TableListError::TwoRows { list: policy.list, line: at + 1, krate: (*krate).to_owned() });
        }
    }
    Ok(rows)
}

/// One column's wording in a ratchet table's refusal/note messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ColumnSpec {
    pub(super) name: &'static str,
    pub(super) what: &'static str,
    pub(super) instead: &'static str,
}

/// A ratchet table's message policy for [`parse_table_rows`] and [`check_table_problems`].
pub(super) struct TablePolicy<const N: usize> {
    pub(super) list: &'static str,
    pub(super) scope: &'static str,
    pub(super) moved_noun: &'static str,
    pub(super) rule: &'static str,
    pub(super) write_cmd: &'static str,
    pub(super) columns: [ColumnSpec; N],
}

/// What is over its row (each a refusal) and what is under it (each a note).
pub(super) fn check_table_problems<T, const N: usize>(
    policy: &TablePolicy<N>,
    counts: &RatchetCounts<T>,
    rows: &BTreeMap<String, [usize; N]>,
    added: Option<&BTreeMap<String, BTreeSet<usize>>>,
    column_of: impl Fn(&T) -> usize,
    shown: impl Fn(&RatchetSite<T>) -> String,
) -> (Vec<String>, Vec<String>) {
    let (mut over, mut under) = (Vec::new(), Vec::new());
    let crates: BTreeSet<&String> = counts.keys().chain(rows.keys()).collect();
    for krate in crates {
        let sites = counts.get(krate).map_or(&[][..], |(outside, _)| outside.as_slice());
        let now_row: [usize; N] = tally_table(sites, &column_of);
        let held_row = rows.get(krate).copied().unwrap_or([0; N]);
        for (col_idx, col) in policy.columns.iter().enumerate() {
            let (now, held) = (now_row[col_idx], held_row[col_idx]);
            let row_ref = if N == 1 { "its row".to_owned() } else { format!("its row's `{}`", col.name) };
            if now > held {
                let of_kind = sites.iter().filter(|it| column_of(&it.what) == col_idx);
                let named: Vec<String> = match added {
                    Some(added) => {
                        of_kind.filter(|it| added.get(&it.path).is_some_and(|lines| lines.contains(&it.line))).map(&shown).collect()
                    }
                    None => of_kind.take(3).map(&shown).collect(),
                };
                let new = match (added, named.is_empty()) {
                    (Some(_), true) => {
                        format!("none is on a line added since the base: a {} moved in, or a file git does not track", policy.moved_noun)
                    }
                    (Some(_), false) => format!("new: {}", named.join(", ")),
                    (None, _) => format!("first: {}", named.join(", ")),
                };
                over.push(format!(
                    "`{krate}` has {now} {} {}, over {row_ref} {held} in {} ({new}): {} ({})",
                    col.what, policy.scope, policy.list, col.instead, policy.rule
                ));
            } else if now < held {
                under.push(format!(
                    "`{krate}` has {now} {} {}, under {row_ref} {held} in {}: lower it (command `{}`)",
                    col.what, policy.scope, policy.list, policy.write_cmd
                ));
            }
        }
    }
    (over, under)
}

/// Reads and parses a ratchet list, allowing a missing file only when writing.
pub(super) fn load_table_rows<const N: usize>(
    policy: &'static TablePolicy<N>,
    root: &Path,
    is_write: bool,
) -> (Option<String>, BTreeMap<String, [usize; N]>) {
    let listed = read_text(&root.join(policy.list));
    let rows = match &listed {
        Some(text) => parse_table_rows(policy, text).unwrap_or_else(|e| fail(SCRIPT, &e.to_string())),
        None if is_write => BTreeMap::new(),
        None => fail(
            SCRIPT,
            &format!(
                "{} could not be read: every crate is held to its row there (command `{}` makes it)",
                policy.list, policy.write_cmd
            ),
        ),
    };
    (listed, rows)
}

/// Writes `text` to `list`, refusing first if `over` is non-empty ("the list only falls").
pub(super) fn write_table(list: &str, root: &Path, over: &[String], text: &str) {
    if !over.is_empty() {
        let lines: Vec<String> = over.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
        eprintln!("{}\n{SCRIPT}: {list} is not written: the list only falls", lines.join("\n"));
        std::process::exit(1);
    }
    if let Err(e) = std::fs::write(root.join(list), text) {
        fail(SCRIPT, &format!("{list} is not written: {e}"));
    }
}

/// Prints any under-row notes and, if any crate is over its row, prints refusals and exits 1.
pub(super) fn report_problems(over: &[String], under: &[String]) {
    for note in under {
        println!("{SCRIPT}: {note}");
    }
    if !over.is_empty() {
        let lines: Vec<String> = over.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
        eprintln!("{}", lines.join("\n"));
        std::process::exit(1);
    }
}

/// One heap use in the tree.
type HeapSite = RatchetSite<HeapUse>;

/// Each crate's heap uses outside test code and in it, in file and line order.
type HeapCounts = RatchetCounts<HeapUse>;

fn count_heap(root: &Path) -> Result<HeapCounts, TreeError> {
    // Test code here is `testing`: test code, and `cfg(any(test, feature = "testing"))` modules
    // (a test's root, which takes `testing::heap()`).
    let Walked { scanned, testing, .. } = walk(root)?;
    let mut out: HeapCounts = BTreeMap::new();
    let mut manifests: BTreeMap<Vec<String>, bool> = BTreeMap::new();
    for (parts, found) in scanned {
        if found.heap.is_empty() && found.test_heap.is_empty() {
            continue;
        }
        let path = parts.join("/");
        let whole_test = testing.contains(&path);
        let krate = crate_of(root, &parts, &mut manifests);
        let (outside, inside) = out.entry(krate).or_default();
        // An entry file's `fn main` takes the heap freely: that is where it is taken.
        let entry = entry_file(&path);
        let kept = |what: HeapUse| match what {
            HeapUse::MainProcess if entry => None,
            HeapUse::MainProcess => Some(HeapUse::Process),
            other => Some(other),
        };
        // A leaked root counts wherever it is, test code too.
        for (line, what) in &found.heap {
            let Some(what) = kept(*what) else { continue };
            let site = HeapSite { path: path.clone(), line: *line, what };
            if whole_test && what != HeapUse::Leak { inside.push(site) } else { outside.push(site) }
        }
        for (line, what) in &found.test_heap {
            let Some(what) = kept(*what) else { continue };
            let site = HeapSite { path: path.clone(), line: *line, what };
            if what == HeapUse::Leak { outside.push(site) } else { inside.push(site) }
        }
    }
    for (outside, inside) in out.values_mut() {
        outside.sort();
        inside.sort();
    }
    Ok(out)
}

fn tally_heap(sites: &[HeapSite]) -> HeapRow {
    tally_table(sites, |it| it.column())
}

/// The list as `heap-write` writes it: the header, then a row for each crate with a use
/// outside test code, sorted.
fn render_heap(counts: &HeapCounts) -> String {
    render_table(HEAP_HEADER, counts, tally_heap)
}

/// The rows `heap-write` keeps over an existing list: each column the lower of the row and the
/// count; a crate with no row is held at 0.
fn held_rows(counts: &HeapCounts, rows: &BTreeMap<String, HeapRow>) -> BTreeMap<String, HeapRow> {
    let mut out = BTreeMap::new();
    for (krate, held) in rows {
        let now = counts.get(krate).map_or([0; 5], |(outside, _)| tally_heap(outside));
        let row: HeapRow = core::array::from_fn(|at| now[at].min(held[at]));
        if row != [0; 5] {
            out.insert(krate.clone(), row);
        }
    }
    out
}

/// The list with `rows`, each under the comment lines `notes` keeps for it.
fn render_heap_rows(rows: &BTreeMap<String, HeapRow>, notes: &BTreeMap<String, std::vec::Vec<String>>) -> String {
    let mut out = HEAP_HEADER.to_owned();
    for (krate, row) in rows {
        for note in notes.get(krate).into_iter().flatten() {
            out.push_str(note);
            out.push('\n');
        }
        out.push_str(&format!("{krate}\t{}\n", row_text(*row)));
    }
    out
}

const fn heap_column(u: HeapUse) -> ColumnSpec {
    ColumnSpec { name: u.name(), what: u.what(), instead: u.instead() }
}

static HEAP_POLICY: TablePolicy<5> = TablePolicy {
    list: HEAP_LIST,
    scope: "counted",
    moved_noun: "call",
    rule: "D102",
    write_cmd: "heap-write",
    columns: [
        heap_column(COLUMNS[0]),
        heap_column(COLUMNS[1]),
        heap_column(COLUMNS[2]),
        heap_column(COLUMNS[3]),
        heap_column(COLUMNS[4]),
    ],
};

fn parse_heap_rows(text: &str) -> Result<BTreeMap<String, HeapRow>, TableListError> {
    parse_table_rows(&HEAP_POLICY, text)
}

/// The lines added since `base` under crates/ (tracked files only).
pub(super) fn added_since(root: &Path, base: &str) -> Result<BTreeMap<String, BTreeSet<usize>>, BaseError> {
    let root_arg = root.to_string_lossy();
    let dir = root_arg.as_ref();
    let diff = git(root, &["-C", dir, "diff", "--no-color", "--no-ext-diff", "--no-renames", "--unified=0", base, "--", "crates"])
        .map_err(|why| BaseError::Git { base: base.to_owned(), what: "diff the tree", why })?;
    added_lines(&diff)
}

/// What is over its row (each a refusal) and what is under it (each a note).
fn heap_problems(
    counts: &HeapCounts,
    rows: &BTreeMap<String, HeapRow>,
    added: Option<&BTreeMap<String, BTreeSet<usize>>>,
) -> (Vec<String>, Vec<String>) {
    check_table_problems(&HEAP_POLICY, counts, rows, added, |it| it.column(), |it| format!("{}:{}", it.path, it.line))
}

pub(super) fn main_with(command: &str, base: Option<&str>, root: &Path) {
    let counts = count_heap(root).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let totals = |pick: fn(&(Vec<HeapSite>, Vec<HeapSite>)) -> &Vec<HeapSite>| {
        let mut row = [0; 5];
        for it in counts.values() {
            for (sum, n) in row.iter_mut().zip(tally_heap(pick(it))) {
                *sum += n;
            }
        }
        row
    };
    let summary = |row: HeapRow| {
        format!("{} `Alloc::global()`, {} `SharedAlloc::global()`, {} `testing::heap()`, {} `Heap::process()`", row[0], row[1], row[2], row[3])
            + &format!(", {} `Root::leak(..)`", row[4])
    };
    if command == "heap-sites" {
        for (krate, (outside, _)) in &counts {
            for it in outside {
                println!("{krate}\t{}:{}\t{}", it.path, it.line, it.what.name());
            }
        }
        return;
    }
    if command == "heap-report" {
        let mut by_total: Vec<(&String, HeapRow, HeapRow)> =
            counts.iter().map(|(krate, (outside, inside))| (krate, tally_heap(outside), tally_heap(inside))).collect();
        by_total.sort_by_key(|(krate, outside, inside)| {
            (core::cmp::Reverse(outside[0] + outside[1] + inside[0] + inside[1]), (*krate).clone())
        });
        println!(
            "crate\tnon-test global\tnon-test shared_global\ttest global\ttest shared_global\tnon-test test_heap\tnon-test process\tleak"
        );
        for (krate, outside, inside) in by_total {
            println!("{krate}\t{}\t{}\t{}\t{}\t{}\t{}\t{}", outside[0], outside[1], inside[0], inside[1], outside[2], outside[3], outside[4]);
        }
        println!("{SCRIPT}: outside tests {}; in tests {}", summary(totals(|it| &it.0)), summary(totals(|it| &it.1)));
        return;
    }
    let (listed, rows) = load_table_rows(&HEAP_POLICY, root, command == "heap-write");
    if command == "heap-write" {
        // The first write takes the tree as it stands; after it, a row never rises: each column
        // is the lower of the row and the count (a crate with no row is held at 0), so a crate
        // over its row stays over until its uses are moved or the row is raised by hand, with a
        // comment above it saying why (kept by every write).
        let text = match &listed {
            Some(text) => {
                let (over, _) = heap_problems(&counts, &rows, None);
                for w in &over {
                    println!("{SCRIPT}: {w}");
                }
                render_heap_rows(&held_rows(&counts, &rows), &super::row_notes(text, HEAP_HEADER))
            }
            None => render_heap(&counts),
        };
        write_table(HEAP_LIST, root, &[], &text);
        println!("{SCRIPT}: wrote {HEAP_LIST}: outside tests {}", summary(totals(|it| &it.0)));
        return;
    }
    let added = base.map(|base| added_since(root, base).unwrap_or_else(|e| fail(SCRIPT, &e.to_string())));
    let (over, under) = heap_problems(&counts, &rows, added.as_ref());
    report_problems(&over, &under);
    println!("{SCRIPT}: outside tests {}, each crate at or under its row in {HEAP_LIST}", summary(totals(|it| &it.0)));
}

/// The cases of the heap's count, for the script's selftest.
pub(super) fn selftest() {
    // Line by line: 1 a call; 2 a comment and a string naming it; 3 the shared one through a
    // path; 4 a test's root and the entry's; 5 a method named `global` (not the type's); 6 in a
    // macro's arguments; 7 no call (a path to the function, not called).
    let text = "let a = Alloc::global();\n// Alloc::global() \"SharedAlloc::global()\"\nlet b = blaze_own::SharedAlloc::global ();\n\
                let t = blaze_own::testing::heap(); let p = Heap::process();\nlet g = x.global();\nassert!(v(Alloc::global()));\n\
                let f = Alloc::global;\n";
    let found = heap_uses(text);
    let want = vec![
        (1, HeapUse::Global),
        (3, HeapUse::SharedGlobal),
        (4, HeapUse::TestHeap),
        (4, HeapUse::Process),
        (6, HeapUse::Global),
    ];
    expect(found == want, &format!("the heap uses read: {found:?}"));
    // An entry's `fn main` takes the heap: marked, so an entry file does not count it; a
    // helper's call is counted.
    let entry = "fn helper() -> &'static Heap {\n    Heap::process()\n}\n\nfn main() {\n    let heap = blaze_own::Heap::process();\n}\n";
    let found = heap_uses(entry);
    expect(found == vec![(2, HeapUse::Process), (6, HeapUse::MainProcess)], &format!("an entry's `main` is marked: {found:?}"));
    expect(entry_file("crates/a/examples/x.rs") && entry_file("crates/a/src/bin/y.rs") && !entry_file("crates/a/src/lib.rs"), "entry files");
    // Test code: a call under `#[cfg(test)]` is the test's, not the crate's.
    let split = scan_file("fn f() { Alloc::global(); }\n#[cfg(test)]\nmod tests {\n    fn g() { Alloc::global(); }\n}\n");
    expect(
        split.heap == vec![(1, HeapUse::Global)] && split.test_heap == vec![(4, HeapUse::Global)],
        &format!("test code's call is the test's: {:?} {:?}", split.heap, split.test_heap),
    );
    // A `cfg(any(test, feature = "testing"))` item is a test's root for the heap columns (D102):
    // its call is the test's; the item after it is not; and a module it declares out of line
    // is marked so its file is too (walk).
    let testing = "#[cfg(any(test, feature = \"testing\"))]\npub fn fixture() { Alloc::global(); }\nfn g() { Alloc::global(); }\n\
                   #[cfg(any(feature = \"testing\", test))]\npub mod testing;\nmod plain;\n";
    let split = scan_file(testing);
    expect(
        split.heap == vec![(3, HeapUse::Global)] && split.test_heap == vec![(2, HeapUse::Global)],
        &format!("a `testing` item's call is the test's: {:?} {:?}", split.heap, split.test_heap),
    );
    let marks: Vec<(&str, bool, bool)> = split.mods.iter().map(|it| (it.name.as_str(), it.in_test, it.in_testing)).collect();
    expect(marks == vec![("testing", false, true), ("plain", false, false)], &format!("a `testing` module is marked: {marks:?}"));
    let whole = scan_file("#![cfg(any(test, feature = \"testing\"))]\nfn f() { Alloc::global(); }\n");
    expect(whole.heap.is_empty() && whole.test_heap.len() == 1, "a `testing` file is a test's root");
    // Only that cfg: a feature of another name is the crate's code.
    let other = scan_file("#[cfg(any(test, feature = \"std\"))]\nfn f() { Alloc::global(); }\n");
    expect(other.heap == vec![(2, HeapUse::Global)], &format!("another feature is code: {:?}", other.heap));
    // The list: a crate over its row is refused, naming the added line; under is a note.
    let site = |line| HeapSite { path: "crates/a/src/lib.rs".to_owned(), line, what: HeapUse::Global };
    let counts: HeapCounts = BTreeMap::from([("a".to_owned(), (vec![site(3), site(9)], Vec::new()))]);
    let held = BTreeMap::from([("a".to_owned(), [1, 0, 0, 0, 0])]);
    let added = BTreeMap::from([("crates/a/src/lib.rs".to_owned(), BTreeSet::from([9]))]);
    let (over, under) = heap_problems(&counts, &held, Some(&added));
    expect(
        over.len() == 1 && over[0].contains("(new: crates/a/src/lib.rs:9)") && under.is_empty(),
        &format!("one over its row, named: {over:?}"),
    );
    let (over, under) = heap_problems(&counts, &BTreeMap::from([("a".to_owned(), [3, 0, 0, 0, 0])]), None);
    expect(over.is_empty() && under.len() == 1, &format!("under its row is a note: {over:?} {under:?}"));
    let rendered = render_heap(&counts);
    expect(parse_heap_rows(&rendered) == Ok(BTreeMap::from([("a".to_owned(), [2, 0, 0, 0, 0])])), "the list reads back");
    // A write never raises a row: `a` at 2 over its 1 keeps 1; a crate with no row is held at 0.
    let kept = held_rows(&counts, &held);
    expect(kept == BTreeMap::from([("a".to_owned(), [1, 0, 0, 0, 0])]), &format!("a write keeps the lower: {kept:?}"));
    expect(held_rows(&counts, &BTreeMap::new()).is_empty(), "a crate with no row stays without one");
    expect(
        parse_heap_rows("a\t1\t0\t0\t0\n").is_err() && parse_heap_rows("a\t1\t0\t0\t0\t0\na\t1\t0\t0\t0\t0\n").is_err(),
        "a bad row is refused (a four-column row too)",
    );
    // A root heap: `Root::leak(` is read, in test code as outside it; `Root::new(` is not counted.
    let rooted = "let r = Root::new(RootOpts::TEST);\nlet s = blaze_mem::Root::leak(r);\n#[cfg(test)]\nmod tests {\n    fn t() { Root::leak(x); }\n}\n";
    let split = scan_file(rooted);
    expect(
        split.heap == vec![(2, HeapUse::Leak)] && split.test_heap == vec![(5, HeapUse::Leak)],
        &format!("a leaked root is read, a made one is not: {:?} {:?}", split.heap, split.test_heap),
    );
    let leak = |line| HeapSite { path: "crates/a/src/lib.rs".to_owned(), line, what: HeapUse::Leak };
    let leaks: HeapCounts = BTreeMap::from([("a".to_owned(), (vec![leak(2), leak(5)], Vec::new()))]);
    let (over, _) = heap_problems(&leaks, &BTreeMap::from([("a".to_owned(), [0, 0, 0, 0, 1])]), None);
    expect(over.len() == 1 && over[0].contains("`leak` 1"), &format!("a leak over its row is refused: {over:?}"));
}
