//! What a revision test is, whichever driver runs it: a project, a sequence of revisions of
//! its files, and after each revision an action, what must come of it, and what must have
//! been made again and what reused. A driver (the real daemon's, `real.rs` beside this file; the simulation's,
//! hir-ty-codegen's `v2/edits/revisions.rs`) applies the revisions and says what it
//! [`Observed`]; `check.rs` holds that to the [`Expect`].

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// One test: `cases/<name>/` (a `test.toml`, `base/` and `rev2/`, `rev3/`.. overlays) or one
/// rustc-style file whose revisions are `--cfg <revision>` (`rustc.rs`).
#[derive(Clone, Debug)]
pub struct Test {
    pub name: String,
    /// One sentence: the property the test holds.
    pub about: String,
    /// The drivers it runs under: `sim`, `real`.
    pub drivers: Vec<String>,
    /// How many seeds the simulated driver runs it under.
    pub seeds: u64,
    /// `revisions[0]` writes the whole project; each later one what changed.
    pub revisions: Vec<Revision>,
}

#[derive(Clone, Debug)]
pub struct Revision {
    /// `rev1`, `rev2`.., or rustc's name (`rpass2`).
    pub label: String,
    /// The files it writes and removes, by path under the project, sorted.
    pub changes: Vec<Change>,
    /// What happens before its action, in order; [`Step::Edit`] applies `changes`.
    pub steps: Vec<Step>,
    pub action: Action,
    /// More arguments of the action's command.
    pub args: Vec<String>,
    /// Variables of the environment of every command from this revision on.
    pub env: Vec<(String, String)>,
    /// How many times the action runs, all started together (`at_once`; 1 where unstated):
    /// each run is held to `prints`, `status` and `as_built`, and what is counted of the
    /// action (`max_units`, `starts`, ..) is of all of them together.
    pub at_once: u64,
    pub expect: Expect,
    /// Checks known to fail here, each with its cause: a known failure is listed, never
    /// hidden, and one that passes fails the test until its row is removed.
    pub known: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct Change {
    pub path: String,
    /// `None`: the file is removed.
    pub bytes: Option<Vec<u8>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Check,
    Build,
    Run,
    Test,
    /// `run the file`: the program the last `blaze build` made (its `compiler-artifact`'s
    /// `executable`, `target/debug/<name>`) run by hand, with the revision's environment and
    /// the arguments after its `--`, not through blaze: what a person does who runs the built
    /// file. Its `prints`, `status` and `as_built` are held as a `run`'s. Not a step.
    RunTheFile,
}

impl Action {
    pub fn word(self) -> &'static str {
        match self {
            Action::Check => "check",
            Action::Build => "build",
            Action::Run => "run",
            Action::Test => "test",
            Action::RunTheFile => "run the file",
        }
    }

    fn of(word: &str) -> Option<Action> {
        [Action::Check, Action::Build, Action::Run, Action::Test, Action::RunTheFile].into_iter().find(|it| it.word() == word)
    }
}

/// What happens between two revisions' actions.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// The revision's changes, each file's bytes written in place.
    Edit,
    /// The changes, each file written beside itself and renamed over (what editors and
    /// `sed -i` do).
    EditByRename,
    /// The changes, each file truncated and then written in a second write.
    EditTruncateWrite,
    /// `blaze stop`: the daemon makes its state durable and exits; the next command starts one.
    Stop,
    /// `kill -9` of the daemon.
    Kill,
    /// The changes saved twice before one build: each `.rs` file written first with a line
    /// comment after its last line, then as the revision has it.
    EditTwice,
    /// A `build` started and interrupted (SIGINT to the command) this many milliseconds in:
    /// a build cancelled midway, whose outcome is not read.
    Cancel(u64),
    /// The project as the revision before left it, copied to a checkout of this name beside
    /// the first and built there by the same daemon: a second project of the same sources.
    Beside(String),
    /// A file's modification time moved, its bytes as they were.
    Touch(String),
    /// Nothing is asked of the daemon for this many milliseconds: what it does with a save
    /// of its own accord (a build ahead, `serve::ahead`) has the time to end.
    Wait(u64),
    /// A command whose outcome only has to be success: `build`, `run`, `check`, `test`.
    Do(Action),
    /// From here on the commands run in another checkout, of this name, beside the first:
    /// a second project, served by the same daemon. The revision's files are that project's
    /// whole, and the revisions after it change that project. Only the real driver has two
    /// checkouts.
    Project(String),
    /// From here on the commands run under another home and another machine directory, of
    /// this name: the daemon is stopped, and the new home starts with a copy of the store the
    /// first home's daemon left (as a store fetched from another machine is). With
    /// `project NAME` before it, the same sources in another directory under another home.
    /// Only the real driver has homes.
    Home(String),
    /// As `home NAME`, but the new home starts with an empty store into which a pack is
    /// imported: the daemon is stopped, `blaze store export --all` writes every record of
    /// the first home's store that may travel as one pack, and `blaze store import` reads
    /// it under the new home (docs/remote-cache.md, "Packs"; lane packs-1112). Only the real
    /// driver has homes.
    Packed(String),
    /// `run`, started and left running: the program keeps running across the revisions that
    /// follow, until [`Step::End`] or the test's end. Only the real driver has a process.
    Start,
    /// The running program's standard output holds `text` no later than `within_ms` after
    /// the last edit step (after [`Step::Start`], with none since): how long a save takes to
    /// reach a running program.
    Await { within_ms: u64, text: String },
    /// The running program's standard output so far holds `text` exactly `times` times: a
    /// program that prints a line as it starts did not start again.
    Count { times: u64, text: String },
    /// The running program is ended.
    End,
    /// `replace engine`: the engine beside the thin `blaze` (`blaze-engine` in the directory
    /// the driver installed the two in) copied to a name beside it and renamed over itself:
    /// the same bytes under a new inode and new times, as an install of the same release
    /// over itself leaves it, so its stamp moves and nothing else. Only the real driver has
    /// an engine on disk.
    ReplaceEngine,
}

/// What must come of a revision's action. Every list is of function paths as `blaze why`
/// names them (`app::f`).
#[derive(Clone, Default, Debug)]
pub struct Expect {
    /// The action fails, and says this.
    pub fails: Option<String>,
    /// `run`: the program's standard output, whole.
    pub prints: Option<String>,
    /// Texts the action's output holds (`test alpha ... ok`).
    pub says: Vec<String>,
    /// The functions whose code was made again: exactly these.
    pub redone: Option<Vec<String>>,
    /// The functions whose code may be made again: none but these (the bound of an edit
    /// whose take-back makes nothing).
    pub redone_within: Option<Vec<String>>,
    /// Functions whose code was made again, among others.
    pub redone_has: Vec<String>,
    /// Functions whose code was not made again.
    pub reused: Vec<String>,
    /// The most units made again.
    pub max_units: Option<u64>,
    /// The most units whose record was not in the store, so that the unit was worked out
    /// again (`blaze why`'s `unit` steps): a unit can miss its record and still compile
    /// nothing, when every function of it is a store hit, which `max_units` does not see.
    pub max_records: Option<u64>,
    /// The most build scripts' runs that were not found kept, so that the script ran again
    /// (`blaze why`'s `build script run` steps): a run whose result is what it was before
    /// makes no unit again, which `max_units` and `max_records` do not see.
    pub max_runs: Option<u64>,
    /// The most engine memos run, and proved unchanged without running.
    pub max_executed: Option<u64>,
    pub max_verified: Option<u64>,
    /// The most writes of inputs of the query engine (`query_counts.written`): 0 for a
    /// request that changes nothing.
    pub max_written: Option<u64>,
    /// The most units made whose record was not kept (`blaze why`'s `unit record` steps), so
    /// that the next build makes them again whatever changed.
    pub max_unkept: Option<u64>,
    /// The most describes the code generator's walk asked its front end for (`blaze explain
    /// edit_work`'s `codegen walk` stage, `described`, over every unit of the build): what a
    /// describe read from a store record does not count
    /// (docs/v2/plan/design/store.md, "Describe records").
    pub max_described: Option<u64>,
    /// The most runs of the queries whose name holds each text, summed over them
    /// (`query_counts.executed` by query, as `blaze explain counts_by_cause` lists it):
    /// `max_ran = { item_tree = 0 }` holds a build to no item tree made. Sorted by text.
    pub max_ran: Vec<(String, u64)>,
    /// The longest the action may take, in milliseconds: a bound a hang or a storm of
    /// retries misses, generous enough that a loaded machine does not.
    pub max_ms: Option<u64>,
    /// The build was answered as the last one was, with no walk (the no-op lane).
    pub noop: bool,
    /// How the program was linked: `first`, `spliced`, `unchanged`, `full`, `none`.
    pub link: Option<String>,
    /// The linked bytes equal a clean build's.
    pub bytes: bool,
    /// `run`: the program `blaze run` ran is the program `blaze build` makes, by what it does:
    /// the file the build names, run in the checkout with the same arguments and environment,
    /// prints the same standard output and exits with the same status, and what it prints on
    /// standard error is in what `blaze run` printed there (which carries blaze's own lines
    /// beside the program's). The refinement of a launched run, which makes a function at its
    /// first call, by the ahead-built program; the build runs after the action and its
    /// account, so it is never the build those are of.
    pub as_built: bool,
    /// The action's exit status: for a `run`, the program's. A status other than 0 is the
    /// outcome stated, not a failure, unless `fails` says it is one.
    pub status: Option<i32>,
    /// Exactly how many spans of each name the build ran (`blaze explain time_by_span_name`):
    /// `spans = { "codegen walk" = 0 }` holds a build to no walk. Sorted by name.
    pub spans: Vec<(String, u64)>,
    /// `daemon_after`: right after the action (before the driver asks anything of its own), a
    /// daemon holds blaze's state (`serve.lock` in it is held: a shared try-lock fails,
    /// `blaze_cargo::serve::holder`) when `true`, and none does when `false`.
    pub daemon_after: Option<bool>,
    /// `starts`: exactly how many daemons started during the revision, its steps and its
    /// action: the lines `serve.log` gained that a daemon writes as it starts serving
    /// (`blaze_cargo::serve::LISTENING`), counted right after the action.
    pub starts: Option<u64>,
}

/// A program run apart from blaze: [`Expect::as_built`]'s file.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Ran {
    pub stdout: String,
    pub stderr: String,
    /// `None`: ended by a signal.
    pub status: Option<i32>,
}

/// What a driver saw of one revision's action. A `None` is a thing the driver cannot see,
/// which fails a check that asks for it: nothing is taken as met unseen.
#[derive(Clone, Default, Debug)]
pub struct Observed {
    /// The action succeeded (a `run`: the build and the program).
    pub ok: bool,
    /// What the action printed: the program's standard output for a `run`.
    pub stdout: String,
    /// Diagnostics and everything else it said.
    pub said: String,
    pub redone: Option<Vec<String>>,
    pub units: Option<u64>,
    /// The units whose record missed ([`Expect::max_records`]).
    pub records: Option<u64>,
    /// The build scripts' runs that missed ([`Expect::max_runs`]).
    pub runs: Option<u64>,
    pub executed: Option<u64>,
    pub verified: Option<u64>,
    /// The engine's input writes ([`Expect::max_written`]).
    pub written: Option<u64>,
    /// The units made whose record was not kept ([`Expect::max_unkept`]).
    pub unkept: Option<u64>,
    /// The describes the codegen walk asked for ([`Expect::max_described`]).
    pub described: Option<u64>,
    /// Each query that ran, with its runs ([`Expect::max_ran`]).
    pub ran: Option<BTreeMap<String, u64>>,
    pub noop: Option<bool>,
    pub link: Option<String>,
    /// Panics the daemon caught since the revision before.
    pub panics: u64,
    pub first_panic: String,
    /// The bytes compared with a clean build's: the linked program (real), the code
    /// records in key order (simulated).
    pub image: Option<Vec<u8>>,
    /// The build's diagnostics as data (level, code, message, first span), sorted; `None`
    /// where the driver does not read them.
    pub diagnostics: Option<Vec<String>>,
    /// What blaze said that it must never say: a contained unwind of the changed-set walk, a
    /// step recomputed for no recorded reason.
    pub alarms: Vec<String>,
    /// `blaze why` of the build, for a failure's message.
    pub why: String,
    /// The alarms the build's account names (`blaze explain account`: work with no recorded
    /// cause, a sum that does not hold, work out of proportion to the change), by name: the
    /// standing expectation of every revision is none (`check.rs`, the check `account`).
    /// `None` where the driver does not see the account.
    pub account: Option<Vec<String>>,
    /// The account's numbers in words, for a failure's message.
    pub account_said: String,
    pub ms: u128,
    /// The action's exit status; `None` where a signal ended it or the driver does not see it.
    pub status: Option<i32>,
    /// [`Expect::as_built`]: the file `blaze build` made, run; or why there is none.
    pub built: Option<Result<Ran, String>>,
    /// Each span name the build ran, with its count ([`Expect::spans`]).
    pub spans: Option<BTreeMap<String, u64>>,
    /// [`Expect::daemon_after`]: whether a daemon held blaze's state right after the action.
    pub daemon_after: Option<bool>,
    /// [`Expect::starts`]: the daemons that started during the revision.
    pub starts: Option<u64>,
}

impl Step {
    /// The step as `test.toml` words it: [`step`] reads it back.
    pub fn word(&self) -> String {
        match self {
            Step::Edit => "edit".to_owned(),
            Step::EditByRename => "edit by rename".to_owned(),
            Step::EditTruncateWrite => "edit by truncate then write".to_owned(),
            Step::EditTwice => "edit twice".to_owned(),
            Step::Cancel(after) => format!("cancel build after {after} ms"),
            Step::Beside(name) => format!("beside {name}"),
            Step::Stop => "stop".to_owned(),
            Step::Kill => "kill".to_owned(),
            Step::Touch(path) => format!("touch {path}"),
            Step::Wait(ms) => format!("wait {ms} ms"),
            Step::Do(action) => action.word().to_owned(),
            Step::Project(name) => format!("project {name}"),
            Step::Home(name) => format!("home {name}"),
            Step::Packed(name) => format!("packed {name}"),
            Step::Start => "start".to_owned(),
            Step::Await { within_ms, text } => format!("await {within_ms} {text}"),
            Step::Count { times, text } => format!("count {times} {text}"),
            Step::End => "end".to_owned(),
            Step::ReplaceEngine => "replace engine".to_owned(),
        }
    }
}

/// The first of the two saves of [`Step::EditTwice`]: a `.rs` file with a comment after its
/// last line; any other file has one save.
pub fn first_save(path: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    path.ends_with(".rs").then(|| [bytes, &b"// the first of two saves\n"[..]].concat())
}

fn text(table: &toml::Table, key: &str, at: &str) -> Option<String> {
    table.get(key).map(|it| match it {
        toml::Value::String(it) => it.clone(),
        other => panic!("{at}: `{key}` is a string, not {other}"),
    })
}

fn texts(table: &toml::Table, key: &str, at: &str) -> Option<Vec<String>> {
    table.get(key).map(|it| match it {
        toml::Value::Array(all) => all
            .iter()
            .map(|it| it.as_str().unwrap_or_else(|| panic!("{at}: `{key}` holds strings, not {it}")).to_owned())
            .collect(),
        other => panic!("{at}: `{key}` is a list of strings, not {other}"),
    })
}

fn count(table: &toml::Table, key: &str, at: &str) -> Option<u64> {
    table.get(key).map(|it| match it.as_integer() {
        Some(n) if n >= 0 => n as u64,
        _ => panic!("{at}: `{key}` is a count, not {it}"),
    })
}

fn flag(table: &toml::Table, key: &str, at: &str) -> bool {
    table.get(key).is_some_and(|it| it.as_bool().unwrap_or_else(|| panic!("{at}: `{key}` is true or false, not {it}")))
}

fn step(word: &str, at: &str) -> Step {
    match word {
        "edit" => Step::Edit,
        "edit by rename" => Step::EditByRename,
        "edit by truncate then write" => Step::EditTruncateWrite,
        "edit twice" => Step::EditTwice,
        "stop" => Step::Stop,
        "kill" => Step::Kill,
        "start" => Step::Start,
        "end" => Step::End,
        "replace engine" => Step::ReplaceEngine,
        _ => match word.split_once(' ') {
            Some(("touch", path)) => Step::Touch(path.to_owned()),
            Some(("home", name)) => {
                assert!(
                    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "{at}: `home NAME` names a home beside the first, by letters, digits and `_`, not `{name}`"
                );
                Step::Home(name.to_owned())
            }
            Some(("packed", name)) => {
                assert!(
                    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "{at}: `packed NAME` names a home beside the first, by letters, digits and `_`, not `{name}`"
                );
                Step::Packed(name.to_owned())
            }
            Some(("wait", ms)) => Step::Wait(
                ms.strip_suffix(" ms")
                    .and_then(|it| it.parse().ok())
                    .unwrap_or_else(|| panic!("{at}: `wait N ms`, not `{word}`")),
            ),
            Some(("cancel", after)) => Step::Cancel(
                after
                    .strip_prefix("build after ")
                    .and_then(|it| it.strip_suffix(" ms"))
                    .and_then(|it| it.parse().ok())
                    .unwrap_or_else(|| panic!("{at}: `cancel build after N ms`, not `{word}`")),
            ),
            Some(("beside", name)) => {
                assert!(
                    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "{at}: `beside NAME` names a directory beside the first checkout, by letters, digits and `_`, not `{name}`"
                );
                Step::Beside(name.to_owned())
            }
            Some(("project", name)) => {
                assert!(
                    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "{at}: `project NAME` names a directory beside the first checkout, by letters, digits and `_`, not `{name}`"
                );
                Step::Project(name.to_owned())
            }
            Some((which @ ("await" | "count"), rest)) => {
                let (number, text) = rest.split_once(' ').unwrap_or_else(|| {
                    panic!("{at}: `{word}`: `await MS TEXT` or `count TIMES TEXT`")
                });
                let number: u64 =
                    number.parse().unwrap_or_else(|_| panic!("{at}: `{word}`: `{number}` is no number"));
                match which {
                    "await" => Step::Await { within_ms: number, text: text.to_owned() },
                    _ => Step::Count { times: number, text: text.to_owned() },
                }
            }
            // `run the file` is an action, never a step: a step's command is blaze's.
            _ => Step::Do(Action::of(word).filter(|it| *it != Action::RunTheFile).unwrap_or_else(|| {
                panic!(
                    "{at}: no step `{word}`: `edit`, `edit by rename`, `edit by truncate then write`, `edit twice`, \
                     `stop`, `kill`, `touch PATH`, `wait N ms`, `project NAME`, `beside NAME`, `home NAME`, `packed NAME`, \
                     `cancel build after N ms`, `start`, `await MS TEXT`, `count TIMES TEXT`, `end`, `replace engine`, \
                     `check`, `build`, `run`, `test`"
                )
            })),
        },
    }
}

/// Every key a revision's table may hold: another is a typo, and is refused.
const KEYS: [&str; 33] = [
    "action", "args", "env", "steps", "remove", "fails", "prints", "says", "redone", "redone_within", "redone_has", "reused",
    "max_units", "max_records", "max_runs", "max_executed", "max_verified", "max_written", "max_unkept", "max_described", "max_ran", "max_ms", "noop", "link", "bytes", "known", "about",
    "as_built", "status", "spans", "daemon_after", "starts", "at_once",
];

/// The blaze checkout this test is of: what a case's `@BLAZE_TREE@` stands for.
pub fn tree() -> PathBuf {
    let tree = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::canonicalize(&tree).unwrap_or_else(|error| panic!("the tree at {}: {error}", tree.display()))
}

/// The suffix of a case's file that is a file of the tree: `Cargo.lock.from-tree` holds the
/// path `Cargo.lock` and is written as `Cargo.lock` with the bytes of the tree's.
const FROM_TREE: &str = ".from-tree";

/// A case's file as it is written: `@BLAZE_TREE@` in it is the tree's path ([`tree`]), so a
/// project can depend on the tree's crates (bstd) wherever the driver writes it; and a file
/// [`FROM_TREE`] is the tree's file it names.
fn resolved(path: String, bytes: Vec<u8>) -> (String, Vec<u8>) {
    if let Some(named) = path.strip_suffix(FROM_TREE) {
        let of = tree().join(String::from_utf8(bytes).expect("a `.from-tree` file holds a path").trim());
        let bytes = fs::read(&of).unwrap_or_else(|error| panic!("{path}: reading {}: {error}", of.display()));
        return (named.to_owned(), bytes);
    }
    const TREE: &[u8] = b"@BLAZE_TREE@";
    if !bytes.windows(TREE.len()).any(|it| it == TREE) {
        return (path, bytes);
    }
    let text = String::from_utf8(bytes).unwrap_or_else(|_| panic!("{path}: names `@BLAZE_TREE@` and is not UTF-8"));
    let tree = tree();
    (path, text.replace("@BLAZE_TREE@", tree.to_str().expect("the tree's path is UTF-8")).into_bytes())
}

fn files(dir: &Path, under: &Path, into: &mut Vec<Change>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("reading {}: {error}", dir.display()))
        .map(|it| it.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files(&path, under, into);
        } else {
            let (path, bytes) =
                resolved(path.strip_prefix(under).unwrap().to_str().unwrap().to_owned(), fs::read(&path).unwrap());
            into.push(Change { path, bytes: Some(bytes) });
        }
    }
}

impl Test {
    /// The test of `cases/<name>/`.
    pub fn read(cases: &Path, name: &str) -> Test {
        let dir = cases.join(name);
        let at = format!("{name}/test.toml");
        let source = fs::read_to_string(dir.join("test.toml"))
            .unwrap_or_else(|error| panic!("reading {}: {error}", dir.join("test.toml").display()));
        let table: toml::Table = source.parse().unwrap_or_else(|error| panic!("{at}: {error}"));
        for key in table.keys() {
            assert!(
                ["about", "drivers", "seeds", "revision"].contains(&key.as_str()),
                "{at}: no key `{key}`: `about`, `drivers`, `seeds`, `[[revision]]`"
            );
        }
        let about = text(&table, "about", &at).unwrap_or_else(|| panic!("{at}: `about` says what the test holds"));
        let drivers = texts(&table, "drivers", &at).unwrap_or_else(|| panic!("{at}: `drivers` names `sim`, `real` or both"));
        let Some(toml::Value::Array(rows)) = table.get("revision") else {
            panic!("{at}: no `[[revision]]`");
        };
        let mut revisions = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let label = format!("rev{}", index + 1);
            let at = format!("{at}, {label}");
            let row = row.as_table().unwrap();
            for key in row.keys() {
                assert!(KEYS.contains(&key.as_str()), "{at}: no key `{key}`; a revision's keys: {KEYS:?}");
            }
            let mut changes = Vec::new();
            let overlay = dir.join(if index == 0 { "base".to_owned() } else { label.clone() });
            if overlay.is_dir() {
                files(&overlay, &overlay, &mut changes);
            } else {
                assert!(index > 0, "{at}: {} is the project", overlay.display());
            }
            for path in texts(row, "remove", &at).unwrap_or_default() {
                changes.push(Change { path, bytes: None });
            }
            let steps = match texts(row, "steps", &at) {
                Some(words) => words.iter().map(|it| step(it, &at)).collect(),
                None => vec![Step::Edit],
            };
            let word = text(row, "action", &at)
                .unwrap_or_else(|| panic!("{at}: `action` is check, build, run, test or run the file"));
            let action = Action::of(&word).unwrap_or_else(|| panic!("{at}: no action `{word}`"));
            let env = match row.get("env") {
                None => Vec::new(),
                Some(toml::Value::Table(env)) => {
                    env.iter().map(|(name, value)| (name.clone(), value.as_str().unwrap().to_owned())).collect()
                }
                Some(other) => panic!("{at}: `env` is a table of variables, not {other}"),
            };
            let known = match row.get("known") {
                None => BTreeMap::new(),
                Some(toml::Value::Table(known)) => {
                    known.iter().map(|(check, cause)| (check.clone(), cause.as_str().unwrap().to_owned())).collect()
                }
                Some(other) => panic!("{at}: `known` is a table from a check's name to its cause, not {other}"),
            };
            revisions.push(Revision {
                label,
                changes,
                steps,
                action,
                args: texts(row, "args", &at).unwrap_or_default(),
                env,
                at_once: match count(row, "at_once", &at) {
                    None => 1,
                    Some(0) => panic!("{at}: `at_once` is how many runs start together, at least 1, not 0"),
                    Some(n) => n,
                },
                expect: Expect {
                    fails: text(row, "fails", &at),
                    prints: text(row, "prints", &at),
                    says: texts(row, "says", &at).unwrap_or_default(),
                    redone: texts(row, "redone", &at),
                    redone_within: texts(row, "redone_within", &at),
                    redone_has: texts(row, "redone_has", &at).unwrap_or_default(),
                    reused: texts(row, "reused", &at).unwrap_or_default(),
                    max_units: count(row, "max_units", &at),
                    max_records: count(row, "max_records", &at),
                    max_runs: count(row, "max_runs", &at),
                    max_executed: count(row, "max_executed", &at),
                    max_verified: count(row, "max_verified", &at),
                    max_written: count(row, "max_written", &at),
                    max_unkept: count(row, "max_unkept", &at),
                    max_described: count(row, "max_described", &at),
                    max_ran: match row.get("max_ran") {
                        None => Vec::new(),
                        Some(toml::Value::Table(bounds)) => bounds
                            .iter()
                            .map(|(name, bound)| match bound.as_integer() {
                                // clone: the case's text, read once
                                Some(n) if n >= 0 => (name.clone(), n as u64),
                                _ => panic!("{at}: `max_ran.{name}` is a count, not {bound}"),
                            })
                            .collect(),
                        Some(other) => panic!("{at}: `max_ran` is a table from a text of a query's name to the most runs, not {other}"),
                    },
                    max_ms: count(row, "max_ms", &at),
                    noop: flag(row, "noop", &at),
                    link: text(row, "link", &at),
                    bytes: flag(row, "bytes", &at),
                    as_built: flag(row, "as_built", &at),
                    status: row.get("status").map(|it| match it.as_integer() {
                        Some(n @ 0..=255) => n as i32,
                        _ => panic!("{at}: `status` is an exit status, 0 to 255, not {it}"),
                    }),
                    spans: match row.get("spans") {
                        None => Vec::new(),
                        Some(toml::Value::Table(counts)) => counts
                            .iter()
                            .map(|(name, count)| match count.as_integer() {
                                // clone: the case's text, read once
                                Some(n) if n >= 0 => (name.clone(), n as u64),
                                _ => panic!("{at}: `spans.{name}` is a count, not {count}"),
                            })
                            .collect(),
                        Some(other) => panic!("{at}: `spans` is a table from a span's name to how many ran, not {other}"),
                    },
                    daemon_after: row.get("daemon_after").map(|it| {
                        it.as_bool().unwrap_or_else(|| panic!("{at}: `daemon_after` is true or false, not {it}"))
                    }),
                    starts: count(row, "starts", &at),
                },
                known,
            });
        }
        assert!(revisions.len() >= 2, "{at}: a revision test has at least two revisions");
        Test { name: name.to_owned(), about, drivers, seeds: count(&table, "seeds", &at).unwrap_or(8), revisions }
    }

    /// Whether the test is of the thin `blaze`'s own path: some revision states what only that
    /// path shows, whether a daemon is reached (`daemon_after`, `starts`), runs at once
    /// (`at_once`), or the built file run by hand (`run the file`). Each of its `build`,
    /// `run`, `check` and `test` commands is then run as a person types it, with no
    /// `--message-format` and no `BLAZE_*` variable but `BLAZE_HOME`, since either one sends
    /// the command line to the engine before the thin `blaze` checks anything (blaze-thin's
    /// `route`): those keys would otherwise hold the engine, never the client.
    pub fn as_typed(&self) -> bool {
        self.revisions.iter().any(|it| {
            it.expect.daemon_after.is_some() || it.expect.starts.is_some() || it.at_once > 1 || it.action == Action::RunTheFile
        })
    }

    /// The revision that began the project revision `upto` (an index) is of, and the
    /// project's name: the last at or before it with a [`Step::Project`], or the first
    /// revision and no name (the first checkout).
    pub fn project_at(&self, upto: usize) -> (usize, Option<&str>) {
        for (index, revision) in self.revisions[..=upto].iter().enumerate().rev() {
            let named = revision.steps.iter().rev().find_map(|it| match it {
                Step::Project(name) => Some(name.as_str()),
                _ => None,
            });
            if named.is_some() {
                return (index, named);
            }
        }
        (0, None)
    }

    /// The files of revision `upto`'s project as they are after it (an index), by path.
    pub fn files_at(&self, upto: usize) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        let (began, _) = self.project_at(upto);
        for revision in &self.revisions[began..=upto] {
            for change in &revision.changes {
                match &change.bytes {
                    // clone: the test's bytes, into the state asked for
                    Some(bytes) => files.insert(change.path.clone(), bytes.clone()),
                    None => files.remove(&change.path),
                };
            }
        }
        files
    }

    /// The environment of revision `upto`'s action: each earlier revision's variables, the
    /// later value of a name winning.
    pub fn env_at(&self, upto: usize) -> BTreeMap<String, String> {
        self.revisions[..=upto].iter().flat_map(|it| it.env.iter().cloned()).collect()
    }
}

/// `cases/`, beside this file.
pub fn cases() -> PathBuf {
    // From either crate a driver lives in (blaze-engine's real one, hir-ty-codegen's simulated).
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-engine/tests/revisions/cases")
}
