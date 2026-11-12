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
}

impl Action {
    pub fn word(self) -> &'static str {
        match self {
            Action::Check => "check",
            Action::Build => "build",
            Action::Run => "run",
            Action::Test => "test",
        }
    }

    fn of(word: &str) -> Option<Action> {
        [Action::Check, Action::Build, Action::Run, Action::Test].into_iter().find(|it| it.word() == word)
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
    /// A file's modification time moved, its bytes as they were.
    Touch(String),
    /// A command whose outcome only has to be success: `build`, `run`, `check`, `test`.
    Do(Action),
    /// From here on the commands run in another checkout, of this name, beside the first:
    /// a second project, served by the same daemon. The revision's files are that project's
    /// whole, and the revisions after it change that project. Only the real driver has two
    /// checkouts.
    Project(String),
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
    /// Functions whose code was made again, among others.
    pub redone_has: Vec<String>,
    /// Functions whose code was not made again.
    pub reused: Vec<String>,
    /// The most units made again.
    pub max_units: Option<u64>,
    /// The most engine memos run, and proved unchanged without running.
    pub max_executed: Option<u64>,
    pub max_verified: Option<u64>,
    /// The longest the action may take, in milliseconds: a bound a hang or a storm of
    /// retries misses, generous enough that a loaded machine does not.
    pub max_ms: Option<u64>,
    /// The build was answered as the last one was, with no walk (the no-op lane).
    pub noop: bool,
    /// How the program was linked: `first`, `spliced`, `unchanged`, `full`, `none`.
    pub link: Option<String>,
    /// The linked bytes equal a clean build's.
    pub bytes: bool,
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
    pub executed: Option<u64>,
    pub verified: Option<u64>,
    pub noop: Option<bool>,
    pub link: Option<String>,
    /// Panics the daemon caught since the revision before.
    pub panics: u64,
    pub first_panic: String,
    /// The bytes compared with a clean build's: the linked program (real), the code
    /// records in key order (simulated).
    pub image: Option<Vec<u8>>,
    /// `blaze why` of the build, for a failure's message.
    pub why: String,
    pub ms: u128,
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
        "stop" => Step::Stop,
        "kill" => Step::Kill,
        _ => match word.split_once(' ') {
            Some(("touch", path)) => Step::Touch(path.to_owned()),
            Some(("project", name)) => {
                assert!(
                    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "{at}: `project NAME` names a directory beside the first checkout, by letters, digits and `_`, not `{name}`"
                );
                Step::Project(name.to_owned())
            }
            _ => Step::Do(Action::of(word).unwrap_or_else(|| {
                panic!(
                    "{at}: no step `{word}`: `edit`, `edit by rename`, `edit by truncate then write`, `stop`, \
                     `kill`, `touch PATH`, `project NAME`, `check`, `build`, `run`, `test`"
                )
            })),
        },
    }
}

/// Every key a revision's table may hold: another is a typo, and is refused.
const KEYS: [&str; 20] = [
    "action", "args", "env", "steps", "remove", "fails", "prints", "says", "redone", "redone_has", "reused",
    "max_units", "max_executed", "max_verified", "max_ms", "noop", "link", "bytes", "known", "about",
];

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
            into.push(Change {
                path: path.strip_prefix(under).unwrap().to_str().unwrap().to_owned(),
                bytes: Some(fs::read(&path).unwrap()),
            });
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
            let word = text(row, "action", &at).unwrap_or_else(|| panic!("{at}: `action` is check, build, run or test"));
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
                expect: Expect {
                    fails: text(row, "fails", &at),
                    prints: text(row, "prints", &at),
                    says: texts(row, "says", &at).unwrap_or_default(),
                    redone: texts(row, "redone", &at),
                    redone_has: texts(row, "redone_has", &at).unwrap_or_default(),
                    reused: texts(row, "reused", &at).unwrap_or_default(),
                    max_units: count(row, "max_units", &at),
                    max_executed: count(row, "max_executed", &at),
                    max_verified: count(row, "max_verified", &at),
                    max_ms: count(row, "max_ms", &at),
                    noop: flag(row, "noop", &at),
                    link: text(row, "link", &at),
                    bytes: flag(row, "bytes", &at),
                },
                known,
            });
        }
        assert!(revisions.len() >= 2, "{at}: a revision test has at least two revisions");
        Test { name: name.to_owned(), about, drivers, seeds: count(&table, "seeds", &at).unwrap_or(8), revisions }
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
