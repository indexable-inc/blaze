//! smoke: the proof that a built blaze does its job, quoted by every merge to main (AGENTS.md
//! rule proof-before-merge). `DOC` below is what its usage error prints.
//!
//!     blaze run -q -p scripts --bin smoke -- '{"blaze": "/root/inst/blaze", "work": "/root/smoke-1113"}' > quote.txt
//!     blaze run -q -p scripts --bin smoke -- '{"verify": {"quote": "<text>", "tree": "<git tree hash>"}}'
//!
//! The class it closes (2026-11-13): a check that the code compiles stood in for a check that
//! the tool works. Three pull requests were merged on compile checks; the merged blaze
//! segfaulted at start, and after that fix could not finish a cold hello-world. So a merge
//! carries the output of the built tool doing its job, tied to the exact tree and binary, and
//! `.github/scripts/land.py` will neither open nor merge a pull request without it. The quote's
//! format is defined here once: `land.py` asks this script (`verify`) and never reads it itself.
//!
//! Each project is built in a copy of its fixture (harness/v2/versus/<name>: the manifest and
//! lock files; the source is written here) under a fresh `BLAZE_HOME`, and each step is one
//! child process run with an environment built from nothing, under a deadline at which its
//! whole process group is killed (`blaze_script::child`): a hang ends as `TIMEOUT`, never as a
//! hang of this script. A step that fails is reported with what makes it debuggable without a
//! rerun (its output's tail, the daemon's `serve.log`, the slowest spans of the last build), and
//! the steps after it run only where they still mean something; nothing is retried.

#![feature(allocator_api)]

/// What the usage error prints.
const DOC: &str = r#"smoke: a built blaze doing its job on hello-world, as the quote a merge to main carries (rule proof-before-merge).

    blaze run -q -p scripts --bin smoke -- '{"blaze": "<release blaze>", "work": "<empty dir>"}' > quote.txt
    blaze run -q -p scripts --bin smoke -- '{"verify": {"quote": "<text>", "tree": "<git tree hash>"}}'

A run (the quote on standard output and in <work>/quote.txt; what went wrong on standard error):

    blaze     the blaze binary under test (the thin `blaze`, its engine beside it), required
    work      a directory the run may create and delete: absent or empty, required
    projects  which fixtures of harness/v2/versus, of "hello" and "six" (["hello"])
    tree      the checkout the binary was built from, whose tree the quote names (".")

Per project, in a fresh BLAZE_HOME, each step one child under a deadline (cold 180 s, the rest
60 s; the whole process group is killed at it), with only HOME, PATH, TERM=dumb, LANG=C.UTF-8
and BLAZE_HOME set:

    cold       `blaze run`: exit 0 and the program's line
    no-op      `blaze run` again, nothing changed: the same line
    body-edit  the printed number changed, `blaze run`: the new line
    comment    a comment line added at the top, `blaze run`: the line of the body edit
    check      `blaze check`: exit 0
    stop       `blaze stop`: exit 0 (it exits 0 saying "no daemon is running" when none is)

The four `run` steps are a chain: after one fails, the rest of the chain is skipped (each is an
edit from the state the one before leaves). `check` and `stop` always run. Exit 0 only for
`verdict ok`.

A verify: `quote` is any text holding exactly one quote (a pull request's body), `tree` the git
tree hash of the commit being merged. Prints `ok` and exits 0 when the quote names that tree,
clean, with every step ok; else prints why and exits 1. `.github/scripts/land.py` runs it.

The quote:

    blaze-smoke 1
    tree <git tree hash of HEAD>[ +dirty <sha256 of `git diff HEAD --binary` and the untracked files>]
    binary <sha256 of the blaze binary>
    host <os>-<arch> <cores this process may use>
    hello cold      ok      31.2s
    ...
    verdict ok                     (or: verdict FAILED hello cold: timed out after 180.0s)
"#;

use std::{
    alloc::Global,
    ffi::{OsStr, OsString},
    fmt::Write as _,
    fs::File,
    io::Read as _,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_script::{
    child::{self, Ended},
    exit, fail,
    input::{self, Fields, Input, InputError},
    read_text_lossy, run_within,
};
use blaze_sha256::Sha256;

const SCRIPT: &str = "smoke";
/// The quote's first line; the number is its format's version, raised on any change to it.
const HEADER: &str = "blaze-smoke 1";
/// The fixtures a run may smoke, each a directory of [`FIXTURES`].
const PROJECTS: [&str; 2] = ["hello", "six"];
/// Where the fixtures' manifests and lock files are, in the checkout (shared with versus.py).
const FIXTURES: &str = "harness/v2/versus";
/// The files of a fixture copied into its project, when the fixture has them.
const FIXTURE_FILES: [&str; 3] = ["Cargo.toml", "Cargo.lock", "blaze.lock"];

/// The cold `blaze run`'s deadline: it builds std and the program from nothing. On the
/// 128-core build VM a cold hello-world took 28 to 43 s on the last working builds
/// (2026-11-13); 180 s is a ceiling for "something is wrong", not a performance bar.
const COLD: Duration = Duration::from_secs(180);
/// Every other step's deadline: a no-op, an edit, a check and a stop each took far under 10 s
/// on the last working builds; 60 s is a ceiling for "something is wrong", not a performance
/// bar.
const WARM: Duration = Duration::from_secs(60);
/// `blaze debug sql`'s deadline when a step failed: a query of one build's spans answers in
/// well under a second; past 20 s the blaze under test is not answering, which is said.
const DIAGNOSE: Duration = Duration::from_secs(20);
/// Each `git` call's deadline: reading HEAD's tree, the diff and the untracked files of a
/// checkout takes well under a second; 60 s is a ceiling for "something is wrong".
const GIT: Duration = Duration::from_secs(60);
/// How many of the last lines of a failed step's output, and of `serve.log`, are printed.
const TAIL: usize = 40;
/// The bytes read at a time when hashing a file: the binary is hundreds of MiB, streamed
/// through one fixed buffer on the stack.
const HASH_CHUNK: usize = 64 * 1024;
/// The slowest spans of the last build, printed when a step failed.
const SPANS_SQL: &str = "SELECT name, count(*) n, round(sum(dur_ns)/1e6) ms, round(max(dur_ns)/1e6) max_ms FROM spans \
     WHERE build_id = (SELECT max(build_id) FROM spans) GROUP BY ALL ORDER BY max_ms DESC LIMIT 12";

/// One step of a project's smoke, in [`STEPS`]' order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Cold,
    NoOp,
    BodyEdit,
    Comment,
    Check,
    Stop,
}

/// Every step, in the order each project runs them and its quote lists them.
const STEPS: [Step; 6] = [Step::Cold, Step::NoOp, Step::BodyEdit, Step::Comment, Step::Check, Step::Stop];

impl Step {
    /// The step's name in the quote.
    fn word(self) -> &'static str {
        match self {
            Step::Cold => "cold",
            Step::NoOp => "no-op",
            Step::BodyEdit => "body-edit",
            Step::Comment => "comment",
            Step::Check => "check",
            Step::Stop => "stop",
        }
    }

    fn of(word: &str) -> Option<Step> {
        STEPS.into_iter().find(|step| step.word() == word)
    }

    fn limit(self) -> Duration {
        if self == Step::Cold { COLD } else { WARM }
    }

    /// Whether the step is one of the chain of `blaze run`s, each an edit from the last.
    fn in_chain(self) -> bool {
        matches!(self, Step::Cold | Step::NoOp | Step::BodyEdit | Step::Comment)
    }

    /// The source's state for this step of the chain: the body's number and whether the
    /// comment line is there.
    fn source_state(self) -> (u32, bool) {
        match self {
            Step::Cold | Step::NoOp | Step::Check | Step::Stop => (0, false),
            Step::BodyEdit => (1, false),
            Step::Comment => (1, true),
        }
    }
}

/// How a step ended, as the quote says it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Word {
    Ok,
    Failed,
    TimedOut,
    Skipped,
}

impl Word {
    fn word(self) -> &'static str {
        match self {
            Word::Ok => "ok",
            Word::Failed => "FAILED",
            Word::TimedOut => "TIMEOUT",
            Word::Skipped => "skipped",
        }
    }

    fn of(word: &str) -> Option<Word> {
        [Word::Ok, Word::Failed, Word::TimedOut, Word::Skipped].into_iter().find(|it| it.word() == word)
    }
}

/// One step's line of the quote.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Line {
    project: String,
    step: Step,
    word: Word,
    /// Its wall time in tenths of a second; none for a step skipped or never started.
    tenths: Option<u64>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Verdict {
    Ok,
    /// The first failure, `<project> <step>: <why>`, on one line.
    Failed(String),
}

/// A smoke quote: what ran, on which tree and binary, and how each step ended.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Quote {
    /// `git rev-parse HEAD^{tree}` of the checkout.
    tree: String,
    /// The sha256 of the checkout's changes from HEAD, when it had any.
    dirty: Option<String>,
    /// The sha256 of the blaze binary.
    binary: String,
    /// `<os>-<arch>`.
    platform: String,
    cores: u32,
    lines: Vec<Line, Global>,
    verdict: Verdict,
}

impl Quote {
    /// The quote as text: deterministic but for the times, one fact per line.
    fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{HEADER}");
        match &self.dirty {
            None => {
                let _ = writeln!(out, "tree {}", self.tree);
            }
            Some(dirty) => {
                let _ = writeln!(out, "tree {} +dirty {dirty}", self.tree);
            }
        }
        let _ = writeln!(out, "binary {}", self.binary);
        let _ = writeln!(out, "host {} {}", self.platform, self.cores);
        for line in &self.lines {
            let _ = write!(out, "{:<5} {:<9} {:<7} ", line.project, line.step.word(), line.word.word());
            let _ = match line.tenths {
                Some(tenths) => writeln!(out, "{}.{}s", tenths / 10, tenths % 10),
                None => writeln!(out, "-"),
            };
        }
        let _ = match &self.verdict {
            Verdict::Ok => writeln!(out, "verdict ok"),
            Verdict::Failed(why) => writeln!(out, "verdict FAILED {why}"),
        };
        out
    }
}

/// Whether `text` is lowercase hex of one of the lengths `lens`.
fn is_hex(text: &str, lens: &[usize]) -> bool {
    lens.contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A git object id: SHA-1 (40 digits) or SHA-256 (64).
const TREE_LENS: [usize; 2] = [40, 64];
/// A SHA-256 in hex.
const SHA256_LENS: [usize; 1] = [64];

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The one quote in `text` (a file, a pull request's body: other lines around it are allowed,
/// and each line is read trimmed, so a code fence or an indent around it does not matter).
fn parse_quote(text: &str) -> Result<Quote, String> {
    let mut lines = Vec::new_in(Global);
    lines.extend(text.lines().map(str::trim));
    let mut starts = lines.iter().enumerate().filter(|(_, line)| **line == HEADER).map(|(at, _)| at);
    let Some(start) = starts.next() else {
        return Err(format!("no `{HEADER}` line: the text holds no smoke quote"));
    };
    if starts.next().is_some() {
        return Err(format!("more than one `{HEADER}` block: keep only the quote of the tree being merged"));
    }
    let body = &lines[start + 1..];
    let tree_field = field(body, 0, "tree")?;
    let (tree, dirty) = match tree_field.split_once(" +dirty ") {
        Some((tree, dirty)) => (tree, Some(dirty)),
        None => (tree_field, None),
    };
    if !is_hex(tree, &TREE_LENS) || dirty.is_some_and(|it| !is_hex(it, &SHA256_LENS)) {
        return Err(format!("the quote's `tree {tree_field}` is not a git tree hash, with `+dirty <sha256>` or not"));
    }
    let binary = field(body, 1, "binary")?;
    if !is_hex(binary, &SHA256_LENS) {
        return Err(format!("the quote's `binary {binary}` is not a sha256"));
    }
    let host = field(body, 2, "host")?;
    let (platform, cores) = host
        .split_once(' ')
        .and_then(|(platform, cores)| Some((platform, cores.parse::<u32>().ok()?)))
        .filter(|(platform, _)| platform.contains('-'))
        .ok_or_else(|| format!("the quote's `host {host}` is not `<os>-<arch> <cores>`"))?;
    let mut steps = Vec::new_in(Global);
    let mut at = 3;
    // Bounded: one line of the body a turn.
    let verdict = loop {
        let Some(line) = body.get(at) else { return Err("the quote ends before its `verdict` line".to_owned()) };
        if let Some(verdict) = line.strip_prefix("verdict ") {
            break match verdict.strip_prefix("FAILED ") {
                _ if verdict == "ok" => Verdict::Ok,
                Some(why) if !why.trim().is_empty() => Verdict::Failed(why.to_owned()),
                _ => return Err(format!("the quote's `verdict {verdict}` is neither `ok` nor `FAILED <why>`")),
            };
        }
        steps.push(parse_line(line)?);
        at += 1;
    };
    if steps.is_empty() || steps.len() % STEPS.len() != 0 {
        return Err(format!("the quote lists {} steps, not the {} of each project", steps.len(), STEPS.len()));
    }
    let mut projects = Vec::new_in(Global);
    for chunk in steps.chunks(STEPS.len()) {
        let project = &chunk[0].project;
        if projects.contains(&project) {
            return Err(format!("the quote lists project {project} twice"));
        }
        projects.push(project);
        if chunk.iter().map(|line| (line.project == *project, line.step)).ne(STEPS.into_iter().map(|step| (true, step))) {
            return Err(format!("the quote's steps of {project} are not {}, in that order", STEPS.map(Step::word).join(", ")));
        }
    }
    let all_ok = steps.iter().all(|line| line.word == Word::Ok);
    match (&verdict, all_ok) {
        (Verdict::Ok, false) => return Err("the quote says `verdict ok` over a step that is not ok".to_owned()),
        (Verdict::Failed(_), true) => return Err("the quote says `verdict FAILED` with every step ok".to_owned()),
        _ => {}
    }
    Ok(Quote {
        tree: tree.to_owned(),
        dirty: dirty.map(str::to_owned),
        binary: binary.to_owned(),
        platform: platform.to_owned(),
        cores,
        lines: steps,
        verdict,
    })
}

/// What follows `<name> ` on the quote's line `at` after its header.
fn field<'a>(body: &[&'a str], at: usize, name: &str) -> Result<&'a str, String> {
    let line: &'a str = body.get(at).copied().ok_or_else(|| format!("the quote ends before its `{name}` line"))?;
    line.strip_prefix(name)
        .and_then(|rest| rest.strip_prefix(' '))
        .ok_or_else(|| format!("the quote's line {} is `{line}`, not its `{name} ...` line", at + 2))
}

/// One step's line: `<project> <step> <word> <time>`, the time `12.3s` or `-`.
fn parse_line(line: &str) -> Result<Line, String> {
    let mut words = line.split_whitespace();
    let (Some(project), Some(step), Some(word), Some(time), None) = (words.next(), words.next(), words.next(), words.next(), words.next()) else {
        return Err(format!("the quote's line `{line}` is not `<project> <step> <outcome> <time>`"));
    };
    let named = !project.is_empty() && project.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let (Some(step), Some(word), true) = (Step::of(step), Word::of(word), named) else {
        return Err(format!("the quote's line `{line}` names no project, step and outcome this script knows"));
    };
    let tenths = match time {
        "-" => None,
        _ => {
            let parsed = time.strip_suffix('s').and_then(|it| it.split_once('.')).and_then(|(whole, tenth)| {
                let tenth = (tenth.len() == 1).then(|| tenth.parse::<u64>().ok()).flatten()?;
                Some(whole.parse::<u64>().ok()? * 10 + tenth)
            });
            Some(parsed.ok_or_else(|| format!("the quote's line `{line}` has a time that is not `<seconds>.<tenth>s` or `-`"))?)
        }
    };
    if (word == Word::Skipped) != tenths.is_none() && word != Word::Failed {
        return Err(format!("the quote's line `{line}`: a skipped step has no time, and an ok or timed-out one has one"));
    }
    Ok(Line { project: project.to_owned(), step, word, tenths })
}

/// `Ok` when `text` holds the quote of a clean `tree` whose every step passed; else why not.
fn verify(text: &str, tree: &str) -> Result<(), String> {
    let quote = parse_quote(text)?;
    if let Some(dirty) = &quote.dirty {
        return Err(format!(
            "the quote was made on a checkout with uncommitted changes (+dirty {dirty}): commit them, build a release blaze of that commit and run the smoke on it"
        ));
    }
    if quote.tree != tree {
        return Err(format!(
            "the quote names tree {}, and the commit being merged has tree {tree}: run the smoke on a release blaze built from that commit",
            quote.tree
        ));
    }
    if let Verdict::Failed(why) = &quote.verdict {
        return Err(format!("the quote's verdict is FAILED {why}: fix that; never run it again hoping it passes"));
    }
    if !quote.lines.iter().any(|line| line.project == "hello") {
        return Err("the quote has no hello steps: the smoke is hello-world at least".to_owned());
    }
    Ok(())
}

/// A run's input.
struct Run {
    blaze: String,
    work: String,
    projects: Vec<String, Global>,
    tree: String,
}

/// A verify's input.
struct Verify {
    quote: String,
    tree: String,
}

/// What was asked: a run, or a verify of a quote.
enum Asked {
    Run(Run),
    Verify(Verify),
}

/// The fields of a run, which a verify is given none of.
const RUN_FIELDS: [&str; 4] = ["blaze", "work", "projects", "tree"];

impl Input for Verify {
    const FIELDS: &'static [&'static str] = &["quote", "tree"];

    fn parse(f: &Fields<'_>) -> Result<Verify, InputError> {
        let quote = f.str("quote", None)?;
        let tree = f.str("tree", None)?;
        if !is_hex(&tree, &TREE_LENS) {
            return Err(InputError(format!("{} is a git tree hash (`git rev-parse <commit>^{{tree}}`), not {tree:?}", f.at("tree"))));
        }
        Ok(Verify { quote, tree })
    }
}

impl Input for Asked {
    const FIELDS: &'static [&'static str] = &["blaze", "work", "projects", "tree", "verify"];

    fn parse(f: &Fields<'_>) -> Result<Asked, InputError> {
        if let Some(verify) = f.object::<Verify>("verify", false)? {
            if let Some(given) = RUN_FIELDS.into_iter().find(|name| f.get(name).is_some()) {
                return Err(InputError(format!("{} is a run's field, and {} reads only its quote", f.at(given), f.at("verify"))));
            }
            return Ok(Asked::Verify(verify));
        }
        let projects = f.ones_of("projects", &PROJECTS, &["hello"])?;
        let distinct = projects.iter().enumerate().all(|(at, name)| !projects[..at].contains(name));
        if projects.is_empty() || !distinct {
            return Err(InputError(format!("{} names each project at most once, and one at least", f.at("projects"))));
        }
        let mut named = Vec::new_in(Global);
        named.extend(projects);
        Ok(Asked::Run(Run { blaze: f.str("blaze", None)?, work: f.str("work", None)?, projects: named, tree: f.str("tree", Some("."))? }))
    }
}

/// How one step ended.
enum Outcome {
    Ok,
    Failed(String),
    TimedOut(Duration),
    Skipped,
}

/// hello-world: the fixture `hello` prints `hello: <55 + body>`. The same program as
/// harness/v2/versus.py's `HELLO`; `@BODY@` is the body edit's slot.
const HELLO: &str = r#"fn value() -> u32 {
    (1..=10).sum::<u32>() + @BODY@
}

fn main() {
    println!("hello: {}", value());
}
"#;

/// The fixture `six`: one call into each of its six dependencies, so each is built and
/// linked; it prints `six: <55 + body> 8`. The same program as versus.py's `SIX`.
const SIX: &str = r#"bitflags::bitflags! {
    struct Flags: u8 {
        const A = 1;
        const B = 2;
    }
}

fn value() -> u32 {
    (1..=10).sum::<u32>() + @BODY@
}

// One call into each dependency, so each is linked: 1 + 1 + 2 + 1 + 3.
fn others() -> usize {
    let digits = regex::Regex::new(r"^[0-9]+$").unwrap();
    let mut map: hashbrown::HashMap<u32, u32> = hashbrown::HashMap::new();
    map.insert(1, 2);
    let mut buffer = itoa::Buffer::new();
    let text = buffer.format(42u32);
    let mut small: smallvec::SmallVec<[u8; 4]> = smallvec::SmallVec::new();
    small.push(1);
    small.push(2);
    let at = memchr::memchr(b'2', text.as_bytes()).unwrap_or(9);
    usize::from(digits.is_match(text)) + map.len() + small.len() + at + usize::from((Flags::A | Flags::B).bits())
}

fn main() {
    println!("six: {} {}", value(), others());
}
"#;

/// The comment edit's line, put at the top so every line after it moves.
const COMMENT_LINE: &str = "// smoke: a comment line, added by the comment edit\n";

/// The project's `src/main.rs` with the body's number `body`, and the comment line or not.
fn source(project: &str, body: u32, comment: bool) -> String {
    let template = if project == "six" { SIX } else { HELLO };
    let mut text = String::new();
    if comment {
        text.push_str(COMMENT_LINE);
    }
    text.push_str(&template.replace("@BODY@", &body.to_string()));
    text
}

/// The line the project's program prints with the body's number `body`.
fn expected(project: &str, body: u32) -> String {
    match project {
        "six" => format!("six: {} 8", 55 + body),
        _ => format!("hello: {}", 55 + body),
    }
}

/// What every step of the run shares.
struct Smoke {
    blaze: PathBuf,
    work: PathBuf,
    root: PathBuf,
    /// This script's own HOME and PATH, the only variables handed on.
    home_dir: OsString,
    path: OsString,
}

impl Smoke {
    /// A child's whole environment: nothing inherited but HOME (the toolchain blaze fetched is
    /// the machine's) and PATH (the C driver).
    fn env<'a>(&'a self, home: &'a Path) -> [(&'static str, &'a OsStr); 5] {
        [
            ("HOME", self.home_dir.as_os_str()),
            ("PATH", self.path.as_os_str()),
            ("TERM", OsStr::new("dumb")),
            ("LANG", OsStr::new("C.UTF-8")),
            ("BLAZE_HOME", home.as_os_str()),
        ]
    }

    fn argv(&self, words: &[&str]) -> Vec<String, Global> {
        let mut argv = Vec::new_in(Global);
        argv.push(self.blaze.display().to_string());
        argv.extend(words.iter().map(|word| (*word).to_owned()));
        argv
    }

    fn log(&self, project: &str, what: &str) -> PathBuf {
        self.work.join("logs").join(format!("{project}-{what}"))
    }

    /// The project's copy of its fixture, with the source of the cold step.
    fn make(&self, project: &str, dir: &Path) -> Result<(), String> {
        let fixture = self.root.join(FIXTURES).join(project);
        std::fs::create_dir_all(dir.join("src")).map_err(|e| format!("{}: {e}", dir.display()))?;
        for name in FIXTURE_FILES {
            let from = fixture.join(name);
            if from.is_file() {
                std::fs::copy(&from, dir.join(name)).map_err(|e| format!("{} could not be copied: {e}", from.display()))?;
            }
        }
        if !dir.join("Cargo.toml").is_file() {
            return Err(format!("{} has no Cargo.toml", fixture.display()));
        }
        let (body, comment) = Step::Cold.source_state();
        write_source(dir, project, body, comment)
    }

    /// One step: its child under its deadline, and how it ended against what it must print.
    fn step(&self, project: &str, step: Step, dir: &Path, home: &Path) -> (Outcome, Option<Duration>) {
        let words: &[&str] = match step {
            Step::Check => &["check"],
            Step::Stop => &["stop"],
            _ => &["run"],
        };
        let log = self.log(project, step.word());
        let ran = child::run_logged(&self.argv(words), dir, &self.env(home), &log, step.limit());
        let measured = match ran {
            Err(why) => return (Outcome::Failed(why), None),
            Ok(Ended::TimedOut { after }) => return (Outcome::TimedOut(after), Some(after)),
            Ok(Ended::Exited(measured)) => measured,
        };
        let wall = Duration::from_nanos(measured.wall_ns);
        let outcome = match measured.status {
            0 => Outcome::Ok,
            -1 => Outcome::Failed("was ended by a signal".to_owned()),
            status => Outcome::Failed(format!("exited {status}")),
        };
        if !matches!(outcome, Outcome::Ok) || !step.in_chain() {
            return (outcome, Some(wall));
        }
        let line = expected(project, step.source_state().0);
        let out = log.with_extension("out");
        let outcome = match read_text_lossy(&out) {
            Err(e) => Outcome::Failed(format!("its output {}: {e}", out.display())),
            Ok(text) if text.lines().any(|it| it.trim_end() == line) => Outcome::Ok,
            Ok(text) => Outcome::Failed(format!(
                "exited 0 and printed no line `{line}` (its last line: `{}`)",
                text.lines().last().unwrap_or("").trim_end()
            )),
        };
        (outcome, Some(wall))
    }

    /// What makes a failed step debuggable without a rerun, on standard error: the tails of
    /// its output and of the daemon's log, and the slowest spans of the last build.
    fn diagnose(&self, project: &str, step: Step, dir: &Path, home: &Path) {
        let log = self.log(project, step.word());
        for ext in ["err", "out"] {
            let path = log.with_extension(ext);
            eprintln!("{SCRIPT}: {project} {}: the last {TAIL} lines of {}:", step.word(), path.display());
            print_tail(&path);
        }
        let serve = home.join("v2").join("serve.log");
        if serve.is_file() {
            eprintln!("{SCRIPT}: {project} {}: the last {TAIL} lines of {}:", step.word(), serve.display());
            print_tail(&serve);
        }
        let spans = self.log(project, &format!("{}-spans", step.word()));
        match child::run_logged(&self.argv(&["debug", "sql", SPANS_SQL]), dir, &self.env(home), &spans, DIAGNOSE) {
            Err(why) => eprintln!("{SCRIPT}: `blaze debug sql` could not run: {why}"),
            Ok(Ended::TimedOut { after }) => {
                eprintln!("{SCRIPT}: `blaze debug sql` did not answer within {} s (its output: {})", after.as_secs(), spans.display());
            }
            Ok(Ended::Exited(ran)) => {
                eprintln!("{SCRIPT}: the slowest spans of the last build (`blaze debug sql`, exit {}):", ran.status);
                print_tail(&spans.with_extension("out"));
                print_tail(&spans.with_extension("err"));
            }
        }
    }

    /// The six steps of `project`, each one's line pushed to `lines` and each failure's
    /// one-line reason to `failures`. A failure of this script's own setup ends it.
    fn project(&self, project: &str, lines: &mut Vec<Line, Global>, failures: &mut Vec<String, Global>) {
        let dir = self.work.join(project);
        let home = self.work.join(format!("{project}-home"));
        if let Err(why) = std::fs::create_dir_all(&home).map_err(|e| format!("{}: {e}", home.display())).and_then(|()| self.make(project, &dir)) {
            fail(SCRIPT, &format!("{project}: the project could not be made: {why}"));
        }
        // The chain step that failed, when one has: the chain's later steps are skipped.
        let mut broken: Option<Step> = None;
        for step in STEPS {
            let (outcome, wall) = match broken {
                Some(by) if step.in_chain() => {
                    eprintln!(
                        "{SCRIPT}: {project} {} skipped: {} did not pass, and this step is an edit from the state it leaves",
                        step.word(),
                        by.word()
                    );
                    (Outcome::Skipped, None)
                }
                _ => {
                    if matches!(step, Step::BodyEdit | Step::Comment) {
                        let (body, comment) = step.source_state();
                        if let Err(why) = write_source(&dir, project, body, comment) {
                            fail(SCRIPT, &format!("{project} {}: the edit: {why}", step.word()));
                        }
                    }
                    self.step(project, step, &dir, &home)
                }
            };
            let why = match &outcome {
                Outcome::Ok | Outcome::Skipped => None,
                Outcome::Failed(why) => Some(why.clone()),
                Outcome::TimedOut(after) => Some(format!("timed out after {}s", tenths_text(*after))),
            };
            if let Some(why) = why {
                let why = why.replace(['\n', '\r'], " ");
                eprintln!("{SCRIPT}: {project} {} FAILED: {why}", step.word());
                self.diagnose(project, step, &dir, &home);
                failures.push(format!("{project} {}: {why}", step.word()));
                if step.in_chain() && broken.is_none() {
                    broken = Some(step);
                }
            }
            let word = match outcome {
                Outcome::Ok => Word::Ok,
                Outcome::Failed(_) => Word::Failed,
                Outcome::TimedOut(_) => Word::TimedOut,
                Outcome::Skipped => Word::Skipped,
            };
            lines.push(Line { project: project.to_owned(), step, word, tenths: wall.map(tenths) });
        }
    }
}

/// `d` in tenths of a second, rounded to the nearest.
fn tenths(d: Duration) -> u64 {
    u64::try_from((d.as_millis() + 50) / 100).unwrap_or(u64::MAX)
}

/// `d` as the quote writes seconds: `31.2`.
fn tenths_text(d: Duration) -> String {
    let t = tenths(d);
    format!("{}.{}", t / 10, t % 10)
}

/// The project's source written in place, as an editor that truncates and writes saves.
fn write_source(dir: &Path, project: &str, body: u32, comment: bool) -> Result<(), String> {
    let path = dir.join("src").join("main.rs");
    std::fs::write(&path, source(project, body, comment)).map_err(|e| format!("{}: {e}", path.display()))
}

/// The last [`TAIL`] lines of the file at `path`, indented, on standard error.
fn print_tail(path: &Path) {
    match read_text_lossy(path) {
        Ok(text) => {
            let count = text.lines().count();
            for line in text.lines().skip(count.saturating_sub(TAIL)) {
                eprintln!("    {line}");
            }
        }
        Err(e) => eprintln!("    ({}: {e})", path.display()),
    }
}

/// `git <args>` in `root` under [`GIT`]'s deadline: its standard output's bytes.
fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut argv = Vec::new_in(Global);
    argv.push("git");
    argv.extend_from_slice(args);
    match run_within(&argv, root, GIT)? {
        None => Err(format!("git {} did not end within {} s in {}", args.join(" "), GIT.as_secs(), root.display())),
        Some(ran) if ran.status == 0 => Ok(ran.stdout),
        Some(ran) => Err(format!("git {} exited {} in {}: {}", args.join(" "), ran.status, root.display(), ran.err().trim())),
    }
}

/// The sha256 of the file at `path`, streamed through a fixed buffer.
fn sha256_file(path: &Path) -> Result<[u8; 32], String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; HASH_CHUNK];
    // Bounded: the file's length, a chunk a turn, to its end.
    loop {
        let read = file.read(&mut buffer).map_err(|e| format!("{}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finish())
}

/// The checkout's tree, and the digest of what differs from it when anything does: `git diff
/// HEAD --binary`, then each untracked file's path and its own sha256.
fn named_tree(root: &Path) -> Result<(String, Option<String>), String> {
    let tree = String::from_utf8_lossy(&git_bytes(root, &["rev-parse", "HEAD^{tree}"])?).trim().to_owned();
    let diff = git_bytes(root, &["diff", "HEAD", "--binary"])?;
    let untracked = git_bytes(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    if diff.is_empty() && untracked.is_empty() {
        return Ok((tree, None));
    }
    let mut hasher = Sha256::new();
    hasher.update(&diff);
    for path in untracked.split(|byte| *byte == 0).filter(|path| !path.is_empty()) {
        hasher.update(path).update(&[0]).update(&sha256_file(&root.join(OsStr::from_bytes(path)))?);
    }
    Ok((tree, Some(hex(&hasher.finish()))))
}

/// The run: every project's steps, then the quote on standard output and in `<work>/quote.txt`.
fn smoke(it: &Run) -> ! {
    let canon = |what: &str, path: &str| std::fs::canonicalize(path).unwrap_or_else(|e| fail(SCRIPT, &format!("the {what} {path}: {e}")));
    let (blaze, root) = (canon("blaze", &it.blaze), canon("tree", &it.tree));
    if !blaze.is_file() {
        fail(SCRIPT, &format!("the blaze {} is not a file", blaze.display()));
    }
    let work = PathBuf::from(&it.work);
    match std::fs::read_dir(&work) {
        Ok(mut listed) => {
            if listed.next().is_some() {
                fail(SCRIPT, &format!("the work directory {} is not empty", work.display()));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => fail(SCRIPT, &format!("the work directory {}: {e}", work.display())),
    }
    if let Err(e) = std::fs::create_dir_all(work.join("logs")) {
        fail(SCRIPT, &format!("the work directory {}: {e}", work.display()));
    }
    let work = canon("work", &it.work);
    #[allow(clippy::disallowed_methods, reason = "a script hands its own HOME and PATH to the blaze it runs, and nothing else")]
    let (home_dir, path) = (std::env::var_os("HOME"), std::env::var_os("PATH"));
    let (Some(home_dir), Some(path)) = (home_dir, path) else { fail(SCRIPT, "HOME and PATH are set: the blaze under test gets both and nothing else") };
    let (tree, dirty) = named_tree(&root).unwrap_or_else(|why| fail(SCRIPT, &why));
    let binary = hex(&sha256_file(&blaze).unwrap_or_else(|why| fail(SCRIPT, &why)));
    let cores = std::thread::available_parallelism().map_or(0, |it| u32::try_from(it.get()).unwrap_or(u32::MAX));
    let run = Smoke { blaze, work: work.clone(), root, home_dir, path };
    let (mut lines, mut failures) = (Vec::new_in(Global), Vec::new_in(Global));
    for project in &it.projects {
        run.project(project, &mut lines, &mut failures);
    }
    let verdict = match failures.as_slice() {
        [] => Verdict::Ok,
        [only] => Verdict::Failed(only.clone()),
        [first, rest @ ..] => Verdict::Failed(format!("{first} (and {} more)", rest.len())),
    };
    let ok = verdict == Verdict::Ok;
    let quote = Quote {
        tree,
        dirty,
        binary,
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        cores,
        lines,
        verdict,
    };
    let text = quote.text();
    print!("{text}");
    let at = work.join("quote.txt");
    if let Err(e) = std::fs::write(&at, &text) {
        fail(SCRIPT, &format!("{} could not be written: {e}", at.display()));
    }
    eprintln!("{SCRIPT}: the quote is in {} (land.py's `smoke`); the logs are in {}", at.display(), work.join("logs").display());
    exit(if ok { 0 } else { 1 })
}

fn main() {
    let mut args = Vec::new_in(Global);
    args.extend(std::env::args().skip(1));
    match args.as_slice() {
        [one] => match input::parse::<Asked>(one, SCRIPT) {
            Asked::Run(run) => smoke(&run),
            Asked::Verify(asked) => match verify(&asked.quote, &asked.tree) {
                Ok(()) => {
                    println!("ok");
                    exit(0)
                }
                Err(why) => {
                    println!("{why}");
                    exit(1)
                }
            },
        },
        _ => {
            eprintln!("usage: {SCRIPT} '<json>'\n\n{DOC}");
            exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    const BINARY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn asked(text: &str) -> Result<Asked, InputError> {
        input::read(text).and_then(|value| input::of::<Asked>(&value, ""))
    }

    fn quote(words: [Word; 6], verdict: Verdict) -> Quote {
        let mut lines = Vec::new_in(Global);
        for (step, (word, tenths)) in STEPS.into_iter().zip(words.into_iter().zip([312, 4, 11, 3, 9, 1])) {
            lines.push(Line { project: "hello".to_owned(), step, word, tenths: (word != Word::Skipped).then_some(tenths) });
        }
        Quote { tree: TREE.to_owned(), dirty: None, binary: BINARY.to_owned(), platform: "linux-aarch64".to_owned(), cores: 128, lines, verdict }
    }

    fn all_ok() -> Quote {
        quote([Word::Ok; 6], Verdict::Ok)
    }

    #[test]
    fn the_input_is_typed() {
        let Ok(Asked::Run(run)) = asked(r#"{"blaze": "/b/blaze", "work": "/w"}"#) else { panic!("a run's input is refused") };
        assert_eq!((run.projects.as_slice(), run.tree.as_str()), (["hello".to_owned()].as_slice(), "."));
        assert!(matches!(asked(&format!(r#"{{"verify": {{"quote": "q", "tree": "{TREE}"}}}}"#)), Ok(Asked::Verify(_))));
        for refused in [
            r#"{"blaze": "/b/blaze", "work": "/w", "timeout": 5}"#,
            r#"{"blaze": "/b/blaze"}"#,
            r#"{"blaze": "/b/blaze", "work": "/w", "projects": ["seven"]}"#,
            r#"{"blaze": "/b/blaze", "work": "/w", "projects": ["hello", "hello"]}"#,
            r#"{"blaze": "/b/blaze", "work": "/w", "projects": []}"#,
            r#"{"verify": {"quote": "q", "tree": "main"}}"#,
            r#"{"verify": {"quote": "q", "tree": "4b825dc642cb6eb9a060e54bf8d69288fbee4904", "admin": true}}"#,
            r#"{"verify": {"quote": "q", "tree": "4b825dc642cb6eb9a060e54bf8d69288fbee4904"}, "blaze": "/b/blaze"}"#,
        ] {
            assert!(asked(refused).is_err(), "accepted: {refused}");
        }
    }

    #[test]
    fn the_quote_is_formatted_one_fact_a_line() {
        let want = "blaze-smoke 1\n\
                    tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
                    binary e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
                    host linux-aarch64 128\n\
                    hello cold      ok      31.2s\n\
                    hello no-op     ok      0.4s\n\
                    hello body-edit ok      1.1s\n\
                    hello comment   ok      0.3s\n\
                    hello check     ok      0.9s\n\
                    hello stop      ok      0.1s\n\
                    verdict ok\n";
        assert_eq!(all_ok().text(), want);
    }

    #[test]
    fn a_quote_parses_back_to_itself() {
        let failed = quote(
            [Word::TimedOut, Word::Skipped, Word::Skipped, Word::Skipped, Word::Ok, Word::Failed],
            Verdict::Failed("hello cold: timed out after 180.0s (and 1 more)".to_owned()),
        );
        let mut dirty = all_ok();
        dirty.dirty = Some(BINARY.to_owned());
        for quote in [all_ok(), failed, dirty] {
            assert_eq!(parse_quote(&quote.text()), Ok(quote.clone()));
            // As a pull request's body holds it: text around it, in a fence.
            let body = format!("Merge x: y\n\n- a commit\n\n```\n{}```\n", quote.text());
            assert_eq!(parse_quote(&body), Ok(quote));
        }
    }

    #[test]
    fn a_malformed_quote_is_refused() {
        let text = all_ok().text();
        let twice = format!("{text}{text}");
        let ok_over_a_failure = text.replace("hello check     ok      0.9s", "hello check     FAILED  0.9s");
        let missing_step = text.replace("hello stop      ok      0.1s\n", "");
        let no_verdict = text.replace("verdict ok\n", "");
        for bad in ["no quote here", twice.as_str(), ok_over_a_failure.as_str(), missing_step.as_str(), no_verdict.as_str()] {
            assert!(parse_quote(bad).is_err(), "parsed: {bad}");
        }
    }

    #[test]
    fn verify_holds_the_quote_to_the_tree() {
        assert_eq!(verify(&all_ok().text(), TREE), Ok(()));
        let other = "1111111111111111111111111111111111111111";
        assert!(verify(&all_ok().text(), other).is_err_and(|why| why.contains(other)));
        let mut dirty = all_ok();
        dirty.dirty = Some(BINARY.to_owned());
        assert!(verify(&dirty.text(), TREE).is_err());
        let failed = quote(
            [Word::Ok, Word::Failed, Word::Skipped, Word::Skipped, Word::Ok, Word::Ok],
            Verdict::Failed("hello no-op: exited 101".to_owned()),
        );
        assert!(verify(&failed.text(), TREE).is_err_and(|why| why.contains("exited 101")));
    }

    #[test]
    fn each_step_expects_its_line() {
        assert_eq!(expected("hello", Step::Cold.source_state().0), "hello: 55");
        assert_eq!(expected("hello", Step::BodyEdit.source_state().0), "hello: 56");
        assert_eq!(expected("six", Step::Comment.source_state().0), "six: 56 8");
        assert!(source("hello", 1, true).starts_with(COMMENT_LINE) && source("hello", 1, true).contains("+ 1\n"));
        assert_eq!(tenths(Duration::from_millis(31_249)), 312);
        assert_eq!(tenths_text(Duration::from_secs(180)), "180.0");
    }
}
