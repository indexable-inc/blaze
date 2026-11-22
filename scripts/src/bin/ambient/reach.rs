//! The reach ratchet (rule 75 "nothing ambient"; decisions D102 and D104,
//! docs/v2/plan/decisions.json): the heap's sibling. It counts each crate's calls that reach the
//! clock, the environment and process facts, randomness, the process's exit or a whole world
//! with no caller handing the capability, outside test code (the ONE definition of test code:
//! `walk`, `scan_file`, as the count of `std` reads it), and holds each crate to its row of
//! tools/ratchets/ambient-reach.txt, which only falls. The capability set it points to is
//! /Volumes/Projects/blaze-int-1112-notes/port27/bstd-capabilities.md PART 2 (`Clock`, `Env`,
//! `Rng`, `Host::exit`, `blaze_core::Os`).
//!
//!     blaze run -q -p scripts --bin ambient -- '{"command": "reach-check", "base": "HEAD"}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "reach-write"}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "reach-report"}'
//!     blaze run -q -p scripts --bin ambient -- '{"command": "reach-sites"}'

use super::*;

/// What ambient.rs's `DOC` says of the reach ratchet, appended to it in the usage error.
pub(super) const REACH_DOC: &str = r#"
The reach ratchet (rule 75; D104): the clock, the environment, randomness, exit and the world
are capabilities (`bstd::Clock`, `Env`, `Rng`, `Host::exit`, `blaze_core::Os`) the program's
entry takes and hands down. Its own list tools/ratchets/ambient-reach.txt, rows `<crate><TAB>`
and six columns, each a count of calls outside test code:
  clock      `Instant::now`, `SystemTime::now`, `Monotonic::new`/`default`, `thread::sleep`,
             and by any path `system_wall_ns`, `process_age`, `clock_gettime`,
             `mach_absolute_time`, `gettimeofday`
  env        `env::{var, var_os, vars, vars_os, args, args_os, current_dir, current_exe,
             temp_dir, home_dir, set_var, remove_var, set_current_dir}`, `home::user`,
             `home::record`, `temp::base`, `cores::get`, `cores::machine`, `huge::chosen`,
             `thread::available_parallelism`, `process::id`, `libc::{getpid, getuid, geteuid,
             uname, sysconf, sysctlbyname, getrlimit, sched_getaffinity}`, and by any path
             `choose_from_env`
  random     `SipKey::random`, `LazyKey::new`, `adaptive::switch_key`, `random::random`,
             `RandomState::new`, and by any path `getentropy`, `getrandom`, `arc4random`,
             `arc4random_buf`
  exit       `process::exit`, `process::abort`, `libc::{_exit, exit, abort}`
  world      `Io::native`, `blaze_core::native`, `rt::run`, `rt::launch`, `Os::process` (the
             OS root: free inside the top-level `fn main` of an entry file, an example, a
             `src/bin/*.rs` or a `src/main.rs`, as `Heap::process` is), and `Io` as a
             parameter's type in any `fn main` signature (D104: `main` names each capability)
  test_root  `testing::os`, `testing::native`: a test's root, named outside a test: a bug
A call is read from the text as written (`Instant::now(`, with a path before it; "by any
path" also bare), comments and strings skipped; a call inside a macro's arguments counts; a
method (`x.now()`) and a definition (`fn system_wall_ns(`) do not. A name brought in by `use`
and called bare is not seen.

    command   reach-check   each crate at or under its row; one over is refused, naming its
                            sites (the new ones given `base`); one under is said, to be
                            lowered by `reach-write`
              reach-write   writes the list from the tree; refuses when a crate is over its
                            row (the list only falls; a new program's entry is the one reason
                            to raise `world`, by hand, saying so in the change)
              reach-report  each crate's calls outside test code and in it, by column, the
                            largest first: the migration's table
              reach-sites   each call outside test code: crate, path:line, column, call
"#;

/// The list, from the root.
pub(super) const REACH_LIST: &str = "tools/ratchets/ambient-reach.txt";

const REACH_HEADER: &str = "# The calls that reach an ambient authority with no caller handing the capability (rule 75,\n\
                            # D104), outside test code, by crate (its directory under crates/): the clock, the\n\
                            # environment and process facts, randomness, the process's exit, a whole world (`Io::native`,\n\
                            # `Os::process` outside an entry's `fn main`, `Io` in a `main` signature) and a test's root.\n\
                            # Only falls; a crate with no row is held to 0 in every column.\n\
                            # Written by scripts/src/bin/ambient.rs (`{\"command\": \"reach-write\"}`), checked by\n\
                            # `{\"command\": \"reach-check\"}`.\n\
                            # crate\tclock\tenv\trandom\texit\tworld\ttest_root\n";

/// The list's columns, in their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Column {
    Clock,
    Env,
    Random,
    Exit,
    World,
    TestRoot,
}

const COLUMNS: [Column; 6] = [Column::Clock, Column::Env, Column::Random, Column::Exit, Column::World, Column::TestRoot];

/// A crate's counts, by [`Column`].
type ReachRow = [usize; 6];

impl Column {
    const fn at(self) -> usize {
        self as usize
    }

    const fn name(self) -> &'static str {
        match self {
            Column::Clock => "clock",
            Column::Env => "env",
            Column::Random => "random",
            Column::Exit => "exit",
            Column::World => "world",
            Column::TestRoot => "test_root",
        }
    }

    const fn what(self) -> &'static str {
        match self {
            Column::Clock => "reads of the clock",
            Column::Env => "reads of the environment or process facts",
            Column::Random => "draws of randomness",
            Column::Exit => "exits of the process",
            Column::World => "makings of a whole world",
            Column::TestRoot => "test roots",
        }
    }

    const fn instead(self) -> &'static str {
        match self {
            Column::Clock => {
                "take the clock from the caller (`clock: &bstd::Clock`, from `io.clock()` at the entry); a deadline narrows through `Cx`"
            }
            Column::Env => {
                "take the environment from the caller (`env: &bstd::Env`, narrowed by `Env::only`; `Host`'s facts), or call the pure \
                 form with the value (`cores::of`, `home::user_in`, `huge::chosen_in`)"
            }
            Column::Random => "take randomness from the caller (`rng: &mut bstd::Rng`, from `io.rng()`, or a `SipKey` drawn from it)",
            Column::Exit => "return the status up to the program's entry, or take `Host` from the caller and call `Host::exit`",
            Column::World => {
                "only the program's entry makes a world, taking `blaze_core::Os::process()` in its `fn main`; take the capabilities \
                 needed (`Out`, `Dir<_>`, `Clock`, ...) from the caller, and name each in `main`'s signature, never a whole `Io` (D104)"
            }
            Column::TestRoot => "a test's root is for test code only: take the capability from the caller",
        }
    }
}

/// One call the ratchet reads: `<prev>::<word>(`, or with `prev` `None` the word called by any
/// path or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Call {
    pub(super) prev: Option<&'static str>,
    pub(super) word: &'static str,
    pub(super) column: Column,
}

const fn call(prev: &'static str, word: &'static str, column: Column) -> Call {
    Call { prev: Some(prev), word, column }
}

const fn any(word: &'static str, column: Column) -> Call {
    Call { prev: None, word, column }
}

/// `Os::process()`, the OS root (bstd-capabilities.md P2); [`reach_uses`] marks it inside a
/// top-level `fn main` as [`ReachUse::MainOs`].
pub(super) static OS_PROCESS: Call = call("Os", "process", Column::World);

/// Every call read, but [`OS_PROCESS`]. Each `(prev, word)` once (the selftest holds it). The
/// sans-IO gate (sansio.rs) reads the same table over the tree's tokens: one list of the calls
/// that reach an ambient authority.
pub(super) static CALLS: [Call; 60] = [
    call("Instant", "now", Column::Clock),
    call("SystemTime", "now", Column::Clock),
    call("Monotonic", "new", Column::Clock),
    call("Monotonic", "default", Column::Clock),
    call("thread", "sleep", Column::Clock),
    any("system_wall_ns", Column::Clock),
    any("process_age", Column::Clock),
    any("clock_gettime", Column::Clock),
    any("mach_absolute_time", Column::Clock),
    any("gettimeofday", Column::Clock),
    call("env", "var", Column::Env),
    call("env", "var_os", Column::Env),
    call("env", "vars", Column::Env),
    call("env", "vars_os", Column::Env),
    call("env", "args", Column::Env),
    call("env", "args_os", Column::Env),
    call("env", "current_dir", Column::Env),
    call("env", "current_exe", Column::Env),
    call("env", "temp_dir", Column::Env),
    call("env", "home_dir", Column::Env),
    call("env", "set_var", Column::Env),
    call("env", "remove_var", Column::Env),
    call("env", "set_current_dir", Column::Env),
    call("home", "user", Column::Env),
    call("home", "record", Column::Env),
    call("temp", "base", Column::Env),
    call("cores", "get", Column::Env),
    call("cores", "machine", Column::Env),
    call("huge", "chosen", Column::Env),
    call("thread", "available_parallelism", Column::Env),
    call("process", "id", Column::Env),
    call("libc", "getpid", Column::Env),
    call("libc", "getuid", Column::Env),
    call("libc", "geteuid", Column::Env),
    call("libc", "uname", Column::Env),
    call("libc", "sysconf", Column::Env),
    call("libc", "sysctlbyname", Column::Env),
    call("libc", "getrlimit", Column::Env),
    call("libc", "sched_getaffinity", Column::Env),
    any("choose_from_env", Column::Env),
    call("SipKey", "random", Column::Random),
    call("LazyKey", "new", Column::Random),
    call("adaptive", "switch_key", Column::Random),
    call("random", "random", Column::Random),
    call("RandomState", "new", Column::Random),
    any("getentropy", Column::Random),
    any("getrandom", Column::Random),
    any("arc4random", Column::Random),
    any("arc4random_buf", Column::Random),
    call("process", "exit", Column::Exit),
    call("process", "abort", Column::Exit),
    call("libc", "_exit", Column::Exit),
    call("libc", "exit", Column::Exit),
    call("libc", "abort", Column::Exit),
    call("Io", "native", Column::World),
    call("blaze_core", "native", Column::World),
    call("rt", "run", Column::World),
    call("rt", "launch", Column::World),
    call("testing", "os", Column::TestRoot),
    call("testing", "native", Column::TestRoot),
];

/// One reach of an ambient authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum ReachUse {
    /// A call of one of [`CALLS`] or [`OS_PROCESS`].
    Call(&'static Call),
    /// `Os::process()` inside a top-level `fn main`: in an entry file (an example, a
    /// `src/bin/*.rs`, a `src/main.rs`) the program's entry, which is free; elsewhere it counts
    /// as [`OS_PROCESS`] ([`count_reach`] decides, by path). Never a site.
    MainOs,
    /// `Io` as a parameter's type in a `fn main` signature (D104: a whole world asked for).
    MainIo,
}

impl ReachUse {
    const fn column(self) -> Column {
        match self {
            ReachUse::Call(it) => it.column,
            ReachUse::MainOs | ReachUse::MainIo => Column::World,
        }
    }

    /// The call as a person reads it: `` `env::var()` ``.
    fn shown(self) -> String {
        match self {
            ReachUse::Call(Call { prev: Some(prev), word, .. }) => format!("`{prev}::{word}()`"),
            ReachUse::Call(Call { prev: None, word, .. }) => format!("`{word}()`"),
            ReachUse::MainOs => "`Os::process()`".to_owned(),
            ReachUse::MainIo => "`Io` in a `fn main` signature".to_owned(),
        }
    }
}

/// The reach the word at bytes `at..end` of `text` is, read from the path before it and the
/// `(` after it.
fn reach_use_of(text: &str, at: usize, end: usize) -> Option<ReachUse> {
    if !text[end..].trim_start().starts_with('(') {
        return None;
    }
    let before = text[..at].trim_end();
    let prev = before.strip_suffix("::").map(trailing_word);
    // A method (`x.now()`) or a definition (`fn system_wall_ns(`, an `extern` block's too).
    if prev.is_none() && (before.ends_with('.') || trailing_word(before) == "fn") {
        return None;
    }
    let word = &text[at..end];
    if prev == Some("Os") && word == "process" {
        return Some(ReachUse::Call(&OS_PROCESS));
    }
    CALLS
        .iter()
        .find(|it| it.word == word && (it.prev.is_none() || it.prev == prev))
        .map(ReachUse::Call)
}

/// The bytes of the parameter list of the `fn main` whose name ends at byte `end`: its `(` and
/// its `)`. `None` when a body or `;` comes first.
fn params_of(text: &str, end: usize) -> Option<(usize, usize)> {
    let open = end + text[end..].find('(')?;
    if text[end..open].contains(['{', ';']) {
        return None;
    }
    let mut depth: usize = 0;
    // Bounded: one turn per byte, to the end of the text at most.
    for (n, byte) in text.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, open + n));
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether the word at bytes `at..end` of `text` stands as a parameter's whole type: after
/// `name:` (and a `&`, a `&mut`, a path), before `,` or `)`.
fn typed_here(text: &str, at: usize, end: usize) -> bool {
    if !text[end..].trim_start().starts_with([',', ')']) {
        return false;
    }
    let mut before = text[..at].trim_end();
    // Bounded: each turn takes one `segment::` off the end.
    while let Some(rest) = before.strip_suffix("::") {
        let rest = rest.trim_end();
        let word = trailing_word(rest);
        if word.is_empty() {
            break;
        }
        before = rest[..rest.len() - word.len()].trim_end();
    }
    if trailing_word(before) == "mut" {
        before = before[..before.len() - 3].trim_end();
    }
    let before = before.strip_suffix('&').map_or(before, str::trim_end);
    before.ends_with(':') && !before.ends_with("::")
}

/// Each reach in `text`, test code included ([`scan_file`] splits it): its line (from 1) and
/// what it is, in order. Comments, strings and characters are skipped as [`item_end`] skips
/// them.
pub(super) fn reach_uses(text: &str) -> Vec<(usize, ReachUse)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    // The line of byte `counted`, kept up to date as `at` moves on.
    let mut line = 1;
    let mut counted = 0;
    let mut at = 0;
    // The bytes of the first top-level `fn main` (its `fn` at a line's start, or after `pub `).
    let mut main: Option<(usize, usize)> = None;
    // The parameter list of the last `fn main` read, at any depth.
    let mut params: Option<(usize, usize)> = None;
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
                let word = &text[at..end];
                if word == "main" && trailing_word(&text[..at]) == "fn" {
                    params = params_of(text, end);
                    if main.is_none() {
                        let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
                        let lead = &text[line_start..at];
                        if lead == "fn " || lead == "pub fn " {
                            main = item_end(text, at).map(|stop| (at, stop));
                        }
                    }
                }
                let found = if word == "Io" && params.is_some_and(|(from, to)| from < at && at < to) {
                    typed_here(text, at, end).then_some(ReachUse::MainIo)
                } else {
                    reach_use_of(text, at, end).map(|it| {
                        let in_main = main.is_some_and(|(from, to)| from < at && at < to);
                        if in_main && it == ReachUse::Call(&OS_PROCESS) { ReachUse::MainOs } else { it }
                    })
                };
                if let Some(found) = found {
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

/// One reach in the tree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ReachSite {
    /// Relative to the root.
    path: String,
    line: usize,
    /// Never [`ReachUse::MainOs`]: [`count_reach`] drops or turns it.
    what: ReachUse,
}

/// Each crate's reaches outside test code and in it, in file and line order.
type ReachCounts = BTreeMap<String, (Vec<ReachSite>, Vec<ReachSite>)>;

fn count_reach(root: &Path) -> Result<ReachCounts, TreeError> {
    let Walked { scanned, skipped, .. } = walk(root)?;
    let mut out: ReachCounts = BTreeMap::new();
    let mut manifests: BTreeMap<Vec<String>, bool> = BTreeMap::new();
    for (parts, found) in scanned {
        if found.reach.is_empty() && found.test_reach.is_empty() {
            continue;
        }
        let path = parts.join("/");
        let whole_test = skipped.contains(&path);
        let krate = crate_of(root, &parts, &mut manifests);
        let (outside, inside) = out.entry(krate).or_default();
        // An entry file's `fn main` takes the OS root freely: that is where it is taken.
        let entry = super::heap::entry_file(&path);
        let kept = |what: ReachUse| match what {
            ReachUse::MainOs if entry => None,
            ReachUse::MainOs => Some(ReachUse::Call(&OS_PROCESS)),
            other => Some(other),
        };
        for (line, what) in &found.reach {
            let Some(what) = kept(*what) else { continue };
            let site = ReachSite { path: path.clone(), line: *line, what };
            if whole_test { inside.push(site) } else { outside.push(site) }
        }
        inside.extend(
            found.test_reach.iter().filter_map(|(line, what)| Some(ReachSite { path: path.clone(), line: *line, what: kept(*what)? })),
        );
    }
    for (outside, inside) in out.values_mut() {
        outside.sort();
        inside.sort();
    }
    Ok(out)
}

fn tally_reach(sites: &[ReachSite]) -> ReachRow {
    let mut row = [0; 6];
    for it in sites {
        row[it.what.column().at()] += 1;
    }
    row
}

fn row_text(row: ReachRow) -> String {
    row.map(|n| n.to_string()).join("\t")
}

/// The list as `reach-write` writes it: the header, then a row for each crate with a reach
/// outside test code, sorted.
fn render_reach(counts: &ReachCounts) -> String {
    let mut out = REACH_HEADER.to_owned();
    for (krate, (outside, _)) in counts {
        let row = tally_reach(outside);
        if row != [0; 6] {
            out.push_str(&format!("{krate}\t{}\n", row_text(row)));
        }
    }
    out
}

/// A row of the list that is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReachListError {
    NotARow { line: usize, text: String },
    TwoRows { line: usize, krate: String },
}

impl fmt::Display for ReachListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReachListError::NotARow { line, text } => write!(
                f,
                "{REACH_LIST}:{line}: `{text}` is not `<crate><TAB><clock><TAB><env><TAB><random><TAB><exit><TAB><world><TAB><test_root>`"
            ),
            ReachListError::TwoRows { line, krate } => write!(f, "{REACH_LIST}:{line}: a second row for `{krate}`"),
        }
    }
}

fn parse_reach_rows(text: &str) -> Result<BTreeMap<String, ReachRow>, ReachListError> {
    let mut rows = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || ReachListError::NotARow { line: at + 1, text: line.to_owned() };
        let cells: Vec<&str> = line.split('\t').collect();
        let [krate, rest @ ..] = cells.as_slice() else { return Err(bad()) };
        if krate.is_empty() || rest.len() != COLUMNS.len() {
            return Err(bad());
        }
        let mut row = [0; 6];
        for (cell, it) in row.iter_mut().zip(rest) {
            *cell = it.parse().map_err(|_| bad())?;
        }
        if rows.insert((*krate).to_owned(), row).is_some() {
            return Err(ReachListError::TwoRows { line: at + 1, krate: (*krate).to_owned() });
        }
    }
    Ok(rows)
}

/// What is over its row (each a refusal, the line `reach-check` prints after `ambient: `) and
/// what is under it (each a note).
fn reach_problems(
    counts: &ReachCounts,
    rows: &BTreeMap<String, ReachRow>,
    added: Option<&BTreeMap<String, BTreeSet<usize>>>,
) -> (Vec<String>, Vec<String>) {
    let (mut over, mut under) = (Vec::new(), Vec::new());
    let crates: BTreeSet<&String> = counts.keys().chain(rows.keys()).collect();
    for krate in crates {
        let sites = counts.get(krate).map_or(&[][..], |(outside, _)| outside.as_slice());
        let now_row = tally_reach(sites);
        let held_row = rows.get(krate).copied().unwrap_or([0; 6]);
        for column in COLUMNS {
            let (now, held) = (now_row[column.at()], held_row[column.at()]);
            let name = column.name();
            if now > held {
                let of_kind = sites.iter().filter(|it| it.what.column() == column);
                let shown = |it: &ReachSite| format!("{}:{} {}", it.path, it.line, it.what.shown());
                let named: Vec<String> = match added {
                    Some(added) => {
                        of_kind.filter(|it| added.get(&it.path).is_some_and(|lines| lines.contains(&it.line))).map(shown).collect()
                    }
                    None => of_kind.take(3).map(shown).collect(),
                };
                let new = match (added, named.is_empty()) {
                    (Some(_), true) => "none is on a line added since the base: a call moved in, or a file git does not track".to_owned(),
                    (Some(_), false) => format!("new: {}", named.join(", ")),
                    (None, _) => format!("first: {}", named.join(", ")),
                };
                over.push(format!(
                    "`{krate}` has {now} {} outside tests, over its row's `{name}` {held} in {REACH_LIST} ({new}): {} (rule 75)",
                    column.what(),
                    column.instead()
                ));
            } else if now < held {
                under.push(format!(
                    "`{krate}` has {now} {} outside tests, under its row's `{name}` {held} in {REACH_LIST}: lower it (command `reach-write`)",
                    column.what()
                ));
            }
        }
    }
    (over, under)
}

fn summary(row: ReachRow) -> String {
    let parts: Vec<String> = COLUMNS.iter().map(|it| format!("{} {}", row[it.at()], it.name())).collect();
    parts.join(", ")
}

pub(super) fn main_with(command: &str, base: Option<&str>, root: &Path) {
    let counts = count_reach(root).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let totals = |pick: fn(&(Vec<ReachSite>, Vec<ReachSite>)) -> &Vec<ReachSite>| {
        let mut row = [0; 6];
        for it in counts.values() {
            for (sum, n) in row.iter_mut().zip(tally_reach(pick(it))) {
                *sum += n;
            }
        }
        row
    };
    if command == "reach-sites" {
        for (krate, (outside, _)) in &counts {
            for it in outside {
                println!("{krate}\t{}:{}\t{}\t{}", it.path, it.line, it.what.column().name(), it.what.shown());
            }
        }
        return;
    }
    if command == "reach-report" {
        let mut by_total: Vec<(&String, ReachRow, ReachRow)> =
            counts.iter().map(|(krate, (outside, inside))| (krate, tally_reach(outside), tally_reach(inside))).collect();
        by_total.sort_by_key(|(krate, outside, inside)| {
            (core::cmp::Reverse(outside.iter().sum::<usize>() + inside.iter().sum::<usize>()), (*krate).clone())
        });
        let heads: Vec<String> =
            COLUMNS.iter().map(|it| format!("non-test {}", it.name())).chain(COLUMNS.iter().map(|it| format!("test {}", it.name()))).collect();
        println!("crate\t{}", heads.join("\t"));
        for (krate, outside, inside) in by_total {
            println!("{krate}\t{}\t{}", row_text(outside), row_text(inside));
        }
        println!("{SCRIPT}: outside tests {}; in tests {}", summary(totals(|it| &it.0)), summary(totals(|it| &it.1)));
        return;
    }
    let listed = read_text(&root.join(REACH_LIST));
    let rows = match &listed {
        Some(text) => parse_reach_rows(text).unwrap_or_else(|e| fail(SCRIPT, &e.to_string())),
        None if command == "reach-write" => BTreeMap::new(),
        None => fail(SCRIPT, &format!("{REACH_LIST} could not be read: every crate is held to its row there (command `reach-write` makes it)")),
    };
    if command == "reach-write" {
        // The first write takes the tree as it stands; after it, rows only fall.
        if listed.is_some() {
            let (over, _) = reach_problems(&counts, &rows, None);
            if !over.is_empty() {
                let lines: Vec<String> = over.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
                eprintln!("{}\n{SCRIPT}: {REACH_LIST} is not written: the list only falls", lines.join("\n"));
                std::process::exit(1);
            }
        }
        if let Err(e) = std::fs::write(root.join(REACH_LIST), render_reach(&counts)) {
            fail(SCRIPT, &format!("{REACH_LIST} is not written: {e}"));
        }
        println!("{SCRIPT}: wrote {REACH_LIST}: outside tests {}", summary(totals(|it| &it.0)));
        return;
    }
    let added = base.map(|base| super::heap::added_since(root, base).unwrap_or_else(|e| fail(SCRIPT, &e.to_string())));
    let (over, under) = reach_problems(&counts, &rows, added.as_ref());
    for note in &under {
        println!("{SCRIPT}: {note}");
    }
    if !over.is_empty() {
        let lines: Vec<String> = over.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
        eprintln!("{}", lines.join("\n"));
        std::process::exit(1);
    }
    println!("{SCRIPT}: outside tests {}, each crate at or under its row in {REACH_LIST}", summary(totals(|it| &it.0)));
}

/// The cases of the reach count, for the script's selftest. The last is the tool caught
/// planting: a tree whose list holds it, a file added that reads `HOME`, and the one line
/// `reach-check` prints for it (after `ambient: `):
///
///     `a` has 1 reads of the environment or process facts outside tests, over its row's `env` 0
///     in tools/ratchets/ambient-reach.txt (new: crates/a/src/plant.rs:2 `env::var_os()`): take
///     the environment from the caller (...) (rule 75)
///
/// and, the plant taken out again, no line.
pub(super) fn selftest() {
    let named = |prev: &str, word: &str| {
        let found = CALLS.iter().find(|it| it.prev == Some(prev) && it.word == word);
        ReachUse::Call(found.unwrap_or_else(|| fail(&format!("{SCRIPT} selftest"), &format!("`{prev}::{word}` is not in the table"))))
    };
    let instant = named("Instant", "now");
    let native = named("Io", "native");
    let var = named("env", "var");

    // Each `(prev, word)` once in the table, `Os::process` not in it.
    let mut keys: Vec<(Option<&str>, &str)> = CALLS.iter().map(|it| (it.prev, it.word)).collect();
    keys.sort();
    keys.dedup();
    expect(keys.len() == CALLS.len() && !keys.contains(&(Some("Os"), "process")), "each call once in the table");

    // The plan's case: one `clock` and one `world` outside test code, one `clock` in it.
    let split = scan_file("fn f() { Instant::now(); bstd::Io::native(w); }\n#[cfg(test)]\nmod tests { fn g() { Instant::now(); } }\n");
    expect(
        split.reach == vec![(1, instant), (1, native)] && split.test_reach == vec![(3, instant)],
        &format!("test code's reach is the test's: {:?} {:?}", split.reach, split.test_reach),
    );

    // Controls: a comment, a string, a method, a definition, a path not called count zero; a
    // macro's argument counts; a bare call counts only for an any-path name.
    let text = "// Instant::now()\nlet s = \"env::var(\";\nlet t = x.now(); let u = c.system_wall_ns();\n\
                pub fn system_wall_ns() -> u64 { 0 }\nlet f = Instant::now;\nassert!(ok(std::env::var(\"X\")));\n\
                let w = system_wall_ns(); let n = now();\n";
    let found = reach_uses(text);
    let wall = CALLS.iter().find(|it| it.prev.is_none() && it.word == "system_wall_ns");
    let wall = ReachUse::Call(wall.unwrap_or_else(|| fail(&format!("{SCRIPT} selftest"), "`system_wall_ns` is not in the table")));
    expect(found == vec![(6, var), (7, wall)], &format!("the reaches read: {found:?}"));

    // The OS root: inside the top-level `fn main` marked (free in an entry file), a helper's
    // counted.
    let entry = "fn helper() -> &'static Os {\n    Os::process()\n}\n\nfn main() {\n    let os = blaze_core::Os::process();\n}\n";
    let found = reach_uses(entry);
    expect(
        found == vec![(2, ReachUse::Call(&OS_PROCESS)), (6, ReachUse::MainOs)],
        &format!("an entry's `main` takes the OS root: {found:?}"),
    );

    // D104: `Io` as a parameter's type in a `fn main` signature is a world asked for; named
    // capabilities, another function's `Io` and a `main` in a string are not.
    let mains = "fn main(cx: &Cx, io: bstd::Io) -> R { r() }\nfn other(io: Io) {}\nmod m {\n    pub fn main(cx: &Cx, out: Out, data: Dir<ReadOnly>) {}\n}\n\
                 const T: &str = \"fn main(cx: &Cx, io: Io)\";\nfn main(_: &mut Io, n: IoKind) {}\n";
    let found = reach_uses(mains);
    expect(found == vec![(1, ReachUse::MainIo), (7, ReachUse::MainIo)], &format!("`Io` in a `main` signature: {found:?}"));

    // The list: over is refused naming the added line; under is a note; it reads back.
    let site = |line| ReachSite { path: "crates/a/src/lib.rs".to_owned(), line, what: instant };
    let counts: ReachCounts = BTreeMap::from([("a".to_owned(), (vec![site(3), site(9)], Vec::new()))]);
    let added = BTreeMap::from([("crates/a/src/lib.rs".to_owned(), BTreeSet::from([9]))]);
    let (over, under) = reach_problems(&counts, &BTreeMap::from([("a".to_owned(), [1, 0, 0, 0, 0, 0])]), Some(&added));
    expect(
        over.len() == 1 && over[0].contains("(new: crates/a/src/lib.rs:9 `Instant::now()`)") && under.is_empty(),
        &format!("one over its row, named: {over:?}"),
    );
    let (over, under) = reach_problems(&counts, &BTreeMap::from([("a".to_owned(), [3, 0, 0, 0, 0, 0])]), None);
    expect(over.is_empty() && under.len() == 1, &format!("under its row is a note: {over:?} {under:?}"));
    expect(parse_reach_rows(&render_reach(&counts)) == Ok(BTreeMap::from([("a".to_owned(), [2, 0, 0, 0, 0, 0])])), "the list reads back");
    expect(
        parse_reach_rows("a\t1\t0\t0\t0\t0\n").is_err()
            && parse_reach_rows("a\t1\t0\t0\t0\t0\t0\na\t1\t0\t0\t0\t0\t0\n").is_err()
            && parse_reach_rows("a\t1\t0\t0\t0\t0\tx\n").is_err(),
        "a bad row is refused",
    );

    // The tool itself, on a tree: a crate whose list holds it passes; a planted read of the
    // environment is caught with the one line below; the plant removed, it passes again.
    let tmp = TempDir::new("ambient-reach-selftest-").unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let put = |path: &str, text: &str| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
        made.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("{path} is not written: {e}")));
    };
    // The root workspace: what the counts read (tree_files).
    put("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    put("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    put(
        "crates/a/src/lib.rs",
        "pub fn now() -> u64 {\n    std::time::Instant::now();\n    0\n}\npub fn os() -> &'static Os {\n    Os::process()\n}\n\
         #[cfg(test)]\nmod tests {\n    fn t() { std::env::var(\"X\"); blaze_core::testing::os(); }\n}\n",
    );
    put("crates/a/examples/x.rs", "fn main() {\n    let os = blaze_core::Os::process();\n}\n");
    let tree = count_reach(tmp.path()).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let rows = tree.get("a").map(|(outside, inside)| (tally_reach(outside), tally_reach(inside)));
    expect(rows == Some(([1, 0, 0, 0, 1, 0], [0, 1, 0, 0, 0, 1])), &format!("the tree's rows (an entry's root free): {rows:?}"));
    let held = parse_reach_rows(&render_reach(&tree)).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let (over, _) = reach_problems(&tree, &held, None);
    expect(over.is_empty(), &format!("the tree at its rows: {over:?}"));
    put("crates/a/src/plant.rs", "pub fn home() -> Option<std::ffi::OsString> {\n    std::env::var_os(\"HOME\")\n}\n");
    let planted = count_reach(tmp.path()).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let added = BTreeMap::from([("crates/a/src/plant.rs".to_owned(), BTreeSet::from([1, 2, 3]))]);
    let (over, _) = reach_problems(&planted, &held, Some(&added));
    let want = format!(
        "`a` has 1 reads of the environment or process facts outside tests, over its row's `env` 0 in \
         tools/ratchets/ambient-reach.txt (new: crates/a/src/plant.rs:2 `env::var_os()`): {} (rule 75)",
        Column::Env.instead()
    );
    expect(over == vec![want], &format!("the plant is caught with its line: {over:?}"));
    if let Err(e) = std::fs::remove_file(tmp.path().join("crates/a/src/plant.rs")) {
        fail(&format!("{SCRIPT} selftest"), &format!("the plant is not removed: {e}"));
    }
    let unplanted = count_reach(tmp.path()).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let (over, _) = reach_problems(&unplanted, &held, Some(&added));
    expect(over.is_empty(), &format!("the plant removed, nothing is over: {over:?}"));
}
