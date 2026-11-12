//! compiletest: the suites of rustc's test tree that make sense for blaze, run by blaze
//! (docs/compiletest.md). `DOC` below is what it prints when not given one argument.

const DOC: &str = r#"compiletest: rustc's own tests under blaze (docs/compiletest.md).

    compiletest '{"command": "fetch", "corpus": "/root/wt/x/corpus"}'
    compiletest '{"command": "list", "corpus": DIR, "suites": ["ui"]}'
    compiletest '{"command": "run", "corpus": DIR, "blaze": "/root/inst/b/blaze", "work": DIR, "suites": ["ui", "incremental", "crashes"], "jobs": 16, "out": "report.jsonl"}'

The input is one JSON object (inline, a file's path, or `-`); an unknown field is refused.

    command       fetch  download the pinned corpus (harness/compiletest/corpus.json: rustc's
                         source archive of blaze's nightly, by sha256) and unpack its tests into
                         `corpus`; a no-op when it is there
                  list   plan every test and print the counts; nothing is built
                  run    plan, run every test that has a plan against one daemon, print the
                         table and the causes, and hold the result to the expectations
    root          the checkout (".")
    corpus        the directory `fetch` fills; the tests are under `<corpus>/src/tests`
    blaze         run: the blaze client under test (`blaze-engine` is beside it)
    work          run: a directory of this run's own (packages, the daemon's home); it is made
    suites        which of ui, incremental, crashes (all three)
    only          tests whose path has one of these parts
    stride, offset   every `stride`-th test of each suite from the `offset`-th (1, 0): a sample
                  or a shard
    jobs          tests at once (8)
    cores         the daemon's BLAZE_CORES (16)
    memory_gb     the daemon's ceiling, BLAZE_DAEMON_MEMORY_GB (24)
    timeout       seconds for one blaze command, and for one program's run (120)
    warm_timeout  seconds for the first build of a hello world (1800)
    restart_every the daemon is stopped and started again after this many tests (1000; 0 never)
    expectations  check (the gate: harness/compiletest/<suite>.toml is held both ways),
                  record (the files are written from this run) or off (off)
    min_ran       a run of a part of a suite (`only`, `stride`) must say how many tests have to
                  run at least, over all its suites: fewer is a failed run
    keep          keep the package of a test that passed (false)
    out           run: one JSON line a test, after a first line with the table

Every test is one of: pass, fail (with its cause and first difference), panic (blaze's own),
timeout, unsupported (a directive or flag with no blaze meaning yet), ignored (the test's own
conditions: another target). Exit status 1 when the expectations do not hold, fewer tests ran
than must, or the corpus is not the pinned one; 2 when blaze could not build a hello world.
"#;

#[path = "../compiletest/clean.rs"]
mod clean;
#[path = "../compiletest/directives.rs"]
mod directives;
#[path = "../compiletest/expect.rs"]
mod expect;
#[path = "../compiletest/judge.rs"]
mod judge;
#[path = "../compiletest/plan.rs"]
mod plan;
#[path = "../compiletest/run.rs"]
mod run;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use blaze_script::{
    Value, fail, fan_out,
    input::{self, Fields, Input, InputError},
    json,
};

use judge::Verdict;
use plan::{Host, Plan, Planned, Suite};

const SCRIPT: &str = "compiletest";
const PIN: &str = "harness/compiletest/corpus.json";

struct Given {
    command: String,
    root: String,
    corpus: String,
    blaze: String,
    work: String,
    suites: Vec<String>,
    only: Vec<String>,
    stride: i64,
    offset: i64,
    jobs: i64,
    cores: i64,
    memory_gb: i64,
    timeout: i64,
    warm_timeout: i64,
    restart_every: i64,
    expectations: String,
    min_ran: i64,
    keep: bool,
    out: String,
}

impl Input for Given {
    const FIELDS: &'static [&'static str] = &[
        "command", "root", "corpus", "blaze", "work", "suites", "only", "stride", "offset", "jobs", "cores", "memory_gb", "timeout", "warm_timeout",
        "restart_every", "expectations", "min_ran", "keep", "out",
    ];

    fn parse(f: &Fields<'_>) -> Result<Given, InputError> {
        Ok(Given {
            command: f.one_of("command", &["fetch", "list", "run"], None)?,
            root: f.str("root", Some("."))?,
            corpus: f.str("corpus", None)?,
            blaze: f.str("blaze", Some(""))?,
            work: f.str("work", Some(""))?,
            suites: f.ones_of("suites", &["ui", "incremental", "crashes"], &["ui", "incremental", "crashes"])?,
            only: f.strs("only")?,
            stride: f.int("stride", Some(1))?,
            offset: f.int("offset", Some(0))?,
            jobs: f.int("jobs", Some(8))?,
            cores: f.int("cores", Some(16))?,
            memory_gb: f.int("memory_gb", Some(24))?,
            timeout: f.int("timeout", Some(120))?,
            warm_timeout: f.int("warm_timeout", Some(1800))?,
            restart_every: f.int("restart_every", Some(1000))?,
            expectations: f.one_of("expectations", &["check", "record", "off"], Some("off"))?,
            min_ran: f.int("min_ran", Some(-1))?,
            keep: f.bool("keep", Some(false))?,
            out: f.str("out", Some(""))?,
        })
    }
}

/// The pin of the corpus (`harness/compiletest/corpus.json`).
struct Pin {
    archive: String,
    sha256: String,
    prefix: String,
    paths: Vec<String>,
}

fn pin(root: &Path) -> Pin {
    let path = root.join(PIN);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", path.display())));
    let value: Value = json::from_str(&text).unwrap_or_else(|e| fail(SCRIPT, &format!("{PIN}: {e}")));
    let field = |name: &str| value[name].as_str().unwrap_or_else(|| fail(SCRIPT, &format!("{PIN}: no string `{name}`"))).to_owned();
    let pin = Pin {
        archive: field("archive"),
        sha256: field("sha256"),
        prefix: field("prefix"),
        paths: value["paths"].as_array().unwrap_or_else(|| fail(SCRIPT, &format!("{PIN}: no list `paths`"))).iter().filter_map(|it| it.as_str().map(str::to_owned)).collect(),
    };
    let hex = pin.sha256.len() == 64 && pin.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let plain = |it: &str| !it.is_empty() && !it.starts_with('/') && !it.split('/').any(|part| part == ".." || part.is_empty());
    if !pin.archive.starts_with("https://") || !hex || !plain(&pin.prefix) || pin.paths.is_empty() || !pin.paths.iter().all(|it| plain(it)) {
        fail(SCRIPT, &format!("{PIN}: `archive` is an https address, `sha256` 64 lowercase hex digits, `prefix` and each of `paths` a plain relative path"));
    }
    pin
}

/// A program run to its end with its output shown; the script stops when it fails.
fn must(argv: &[&str], cwd: &Path) {
    match blaze_script::run_shown(argv, cwd) {
        Ok(0) => {}
        Ok(status) => fail(SCRIPT, &format!("`{}` exited {status}", argv.join(" "))),
        Err(why) => fail(SCRIPT, &why),
    }
}

/// The sha256 of `file`, by the machine's `sha256sum` or `shasum`.
fn sha256(file: &Path, cwd: &Path) -> String {
    let name = file.to_string_lossy().into_owned();
    let argv: Vec<&str> = match blaze_script::which("sha256sum") {
        Some(_) => vec!["sha256sum", name.as_str()],
        None => vec!["shasum", "-a", "256", name.as_str()],
    };
    let ran = blaze_script::run(&argv, cwd).unwrap_or_else(|why| fail(SCRIPT, &why));
    if ran.status != 0 {
        fail(SCRIPT, &format!("`{}` exited {}: {}", argv.join(" "), ran.status, ran.err().trim()));
    }
    ran.out().split_whitespace().next().unwrap_or("").to_owned()
}

/// `fetch`: the pinned archive downloaded, held to its sha256 and only then unpacked, the
/// pinned paths alone. The corpus is data: nothing of it runs here.
fn fetch(given: &Given) {
    let pin = pin(Path::new(&given.root));
    let corpus = PathBuf::from(&given.corpus);
    let marker = corpus.join("PINNED");
    if std::fs::read_to_string(&marker).is_ok_and(|it| it.trim() == pin.sha256) {
        println!("{SCRIPT}: {} holds the pinned corpus ({})", corpus.display(), pin.sha256);
        return;
    }
    let src = corpus.join("src");
    let _ = std::fs::remove_dir_all(&src);
    std::fs::create_dir_all(&src).unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", src.display())));
    let archive = corpus.join("archive.tar.xz");
    let archive_name = archive.to_string_lossy().into_owned();
    must(&["curl", "--fail", "--silent", "--show-error", "--location", "--proto", "=https", "--retry", "3", "--output", archive_name.as_str(), pin.archive.as_str()], &corpus);
    let got = sha256(&archive, &corpus);
    if got != pin.sha256 {
        let _ = std::fs::remove_file(&archive);
        fail(SCRIPT, &format!("{} has sha256 {got}, and {PIN} pins {}: refused and removed", pin.archive, pin.sha256));
    }
    let members: Vec<String> = pin.paths.iter().map(|it| format!("{}/{it}", pin.prefix)).collect();
    let src_name = src.to_string_lossy().into_owned();
    let strip = format!("--strip-components={}", pin.prefix.split('/').count());
    let mut argv: Vec<&str> = vec!["tar", "-xJf", archive_name.as_str(), "-C", src_name.as_str(), strip.as_str()];
    argv.extend(members.iter().map(String::as_str));
    must(&argv, &corpus);
    let _ = std::fs::remove_file(&archive);
    std::fs::write(&marker, format!("{}\n", pin.sha256)).unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", marker.display())));
    println!("{SCRIPT}: unpacked {} of {} into {}", pin.paths.join(", "), pin.archive, src.display());
}

/// Every `.rs` file under `dir` that can be a test, as paths from `root`, sorted: not under an
/// `auxiliary` directory, nor under one compiletest is told to pass over.
fn sources(root: &Path, dir: &Path, into: &mut Vec<String>) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = read.filter_map(|it| it.ok().map(|it| it.path())).collect();
    entries.sort();
    if entries.iter().any(|it| it.file_name().is_some_and(|name| name == "compiletest-ignore-dir")) {
        return;
    }
    for path in entries {
        // A link is not followed: the corpus names what it likes.
        let Ok(kind) = std::fs::symlink_metadata(&path).map(|it| it.file_type()) else { continue };
        if kind.is_dir() {
            if path.file_name().is_some_and(|name| name != "auxiliary") {
                sources(root, &path, into);
            }
        } else if kind.is_file()
            && path.extension().is_some_and(|it| it == "rs")
            && let Ok(below) = path.strip_prefix(root)
        {
            into.push(below.to_string_lossy().into_owned());
        }
    }
}

/// One test of a suite as the run sees it: a ui test's revision is a test of its own.
struct Case {
    suite: Suite,
    /// `path.rs`, or `path.rs#revision`.
    id: String,
    bytes: u64,
    /// The plans to carry out in order in one directory; empty when it does not run.
    plans: Vec<(Option<String>, Plan)>,
    /// Why it does not run: (`unsupported` or `ignored`, the directive or flag).
    not_run: Option<(&'static str, String)>,
}

fn cases(suite: Suite, root: &Path, given: &Given, host: &Host) -> Vec<Case> {
    let mut files = Vec::new();
    sources(root, root, &mut files);
    let read = |path: &str| std::fs::read_to_string(root.join(path)).ok();
    let mut out = Vec::new();
    for file in files {
        let bytes = std::fs::metadata(root.join(&file)).map_or(0, |it| it.len());
        let not_run = |id: String, class: &'static str, why: String| Case { suite, id, bytes, plans: Vec::new(), not_run: Some((class, why)) };
        let Some(source) = read(&file) else {
            out.push(not_run(file, "unsupported", "the file is not UTF-8".to_owned()));
            continue;
        };
        let parsed = directives::parse(&source);
        // A file another test uses is not a test.
        if parsed.directives.iter().any(|it| it.name == "ignore-auxiliary") {
            continue;
        }
        let revisions = parsed.revisions();
        let one = |revision: Option<&str>| plan::plan(suite, &file, &source, &parsed, revision, host, &read);
        if suite == Suite::Incremental {
            if revisions.is_empty() {
                // A module of another test, or a file with nothing to build twice.
                continue;
            }
            let mut plans = Vec::new();
            let mut stopped = None;
            for revision in &revisions {
                match one(Some(revision)) {
                    Planned::Run(plan) => plans.push((Some(revision.clone()), *plan)),
                    Planned::Ignored(why) => stopped = stopped.or(Some(("ignored", why))),
                    Planned::Unsupported(why) => stopped = stopped.or(Some(("unsupported", why))),
                }
            }
            out.push(match stopped {
                Some((class, why)) => not_run(file, class, why),
                None => Case { suite, id: file, bytes, plans, not_run: None },
            });
            continue;
        }
        let each: Vec<Option<&str>> = if revisions.is_empty() { vec![None] } else { revisions.iter().map(|it| Some(it.as_str())).collect() };
        for revision in each {
            let id = match revision {
                Some(revision) => format!("{file}#{revision}"),
                None => file.clone(),
            };
            out.push(match one(revision) {
                Planned::Run(plan) => Case { suite, id, bytes, plans: vec![(revision.map(str::to_owned), *plan)], not_run: None },
                Planned::Ignored(why) => not_run(id, "ignored", why),
                Planned::Unsupported(why) => not_run(id, "unsupported", why),
            });
        }
    }
    let stride = usize::try_from(given.stride.max(1)).unwrap_or(1);
    let offset = usize::try_from(given.offset.max(0)).unwrap_or(0) % stride;
    out.into_iter()
        .filter(|it| given.only.is_empty() || given.only.iter().any(|part| it.id.contains(part.as_str())))
        .enumerate()
        .filter(|(index, _)| index % stride == offset)
        .map(|(_, it)| it)
        .collect()
}

/// One test's line of the report.
struct Row {
    suite: Suite,
    id: String,
    class: &'static str,
    cause: String,
    detail: String,
    ms: u128,
    bytes: u64,
    not_held: Vec<String>,
    hints: (usize, usize, Vec<String>),
}

impl Row {
    fn json(&self) -> Value {
        json::json!({
            "suite": self.suite.name(), "test": self.id, "class": self.class, "cause": self.cause, "detail": self.detail, "ms": self.ms as u64,
            "bytes": self.bytes, "not_held": self.not_held, "clean_held": self.hints.0, "clean_not_held": self.hints.1, "reused_though_changed": self.hints.2,
        })
    }
}

const CLASSES: [&str; 6] = ["pass", "fail", "panic", "timeout", "unsupported", "ignored"];

fn counts(rows: &[&Row]) -> BTreeMap<&'static str, usize> {
    let mut out: BTreeMap<&'static str, usize> = CLASSES.iter().map(|it| (*it, 0)).collect();
    for row in rows {
        *out.entry(row.class).or_default() += 1;
    }
    out
}

/// The causes of the rows of `classes`, most tests first, each with its smallest example.
fn causes<'a>(rows: &[&'a Row], classes: &[&str]) -> Vec<(String, usize, &'a Row)> {
    let mut groups: BTreeMap<String, Vec<&'a Row>> = BTreeMap::new();
    for row in rows.iter().filter(|it| classes.contains(&it.class)) {
        let name = if row.class == "fail" || row.class == "unsupported" || row.class == "ignored" { row.cause.clone() } else { format!("{}: {}", row.class, row.cause) };
        groups.entry(name).or_default().push(*row);
    }
    let mut out: Vec<(String, usize, &'a Row)> = groups
        .into_iter()
        .map(|(cause, tests)| {
            let smallest = tests.iter().copied().min_by(|a, b| (a.bytes, &a.id).cmp(&(b.bytes, &b.id))).expect("a group has a test");
            (cause, tests.len(), smallest)
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

fn table(rows: &[Row]) -> Vec<Value> {
    let mut out = Vec::new();
    println!("{:<12} {:>6} {:>6} {:>6} {:>6} {:>6} {:>7} {:>11} {:>7}", "suite", "total", "ran", "pass", "fail", "panic", "timeout", "unsupported", "ignored");
    for suite in Suite::ALL {
        let mine: Vec<&Row> = rows.iter().filter(|it| it.suite == suite).collect();
        if mine.is_empty() {
            continue;
        }
        let n = counts(&mine);
        let ran = n["pass"] + n["fail"] + n["panic"] + n["timeout"];
        println!(
            "{:<12} {:>6} {:>6} {:>6} {:>6} {:>6} {:>7} {:>11} {:>7}",
            suite.name(),
            mine.len(),
            ran,
            n["pass"],
            n["fail"],
            n["panic"],
            n["timeout"],
            n["unsupported"],
            n["ignored"]
        );
        out.push(json::json!({"suite": suite.name(), "total": mine.len(), "ran": ran, "pass": n["pass"], "fail": n["fail"], "panic": n["panic"], "timeout": n["timeout"], "unsupported": n["unsupported"], "ignored": n["ignored"]}));
    }
    out
}

fn print_causes(title: &str, rows: &[&Row], classes: &[&str], most: usize) {
    let all = causes(rows, classes);
    if all.is_empty() {
        return;
    }
    println!("\n{title} ({} causes; the first {}):", all.len(), most.min(all.len()));
    for (cause, count, smallest) in all.iter().take(most) {
        println!("  {count:>5}  {cause}\n         e.g. {}/{} ({} bytes){}", smallest.suite.name(), smallest.id, smallest.bytes, if smallest.detail.is_empty() { String::new() } else { format!(": {}", directives::cut(&smallest.detail, 300)) });
    }
}

fn work_name(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '+' }).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [one] = args.as_slice() else {
        eprintln!("{DOC}");
        blaze_script::exit(1)
    };
    let given: Given = input::parse(one, SCRIPT);
    if given.command == "fetch" {
        return fetch(&given);
    }
    let root = PathBuf::from(&given.root);
    let pinned = pin(&root);
    let corpus = PathBuf::from(&given.corpus);
    if !std::fs::read_to_string(corpus.join("PINNED")).is_ok_and(|it| it.trim() == pinned.sha256) {
        fail(SCRIPT, &format!("{} is not the pinned corpus ({}): run `{{\"command\": \"fetch\", \"corpus\": ..}}`", corpus.display(), pinned.sha256));
    }
    let host = Host { arch: std::env::consts::ARCH.to_owned(), os: std::env::consts::OS.to_owned() };
    let suites: Vec<Suite> = given.suites.iter().filter_map(|it| Suite::of(it)).collect();
    let suite_root = |suite: Suite| corpus.join("src/tests").join(suite.name());
    let mut all: Vec<Case> = Vec::new();
    for suite in &suites {
        let found = cases(*suite, &suite_root(*suite), &given, &host);
        if found.is_empty() {
            fail(SCRIPT, &format!("no test of {} was found under {}", suite.name(), suite_root(*suite).display()));
        }
        all.extend(found);
    }
    let part = !given.only.is_empty() || given.stride > 1;
    if given.command == "run" && part && given.min_ran < 0 {
        fail(SCRIPT, "a run of a part of a suite (`only`, `stride`) says how many tests must run at least (`min_ran`)");
    }
    let mut rows: Vec<Row> = Vec::new();
    let began = Instant::now();
    if given.command == "list" {
        for case in &all {
            let (class, cause) = case.not_run.clone().unwrap_or(("pass", String::new()));
            rows.push(Row { suite: case.suite, id: case.id.clone(), class, cause, detail: String::new(), ms: 0, bytes: case.bytes, not_held: Vec::new(), hints: (0, 0, Vec::new()) });
        }
        println!("{SCRIPT}: the plan of {} tests for {} (`pass` here: has a plan; nothing was built)", rows.len(), host.triple());
        table(&rows);
        let shown: Vec<&Row> = rows.iter().collect();
        print_causes("not supported", &shown, &["unsupported"], 40);
        print_causes("ignored by the test's own conditions", &shown, &["ignored"], 10);
        return;
    }
    if given.blaze.is_empty() || given.work.is_empty() {
        fail(SCRIPT, "`run` needs `blaze` and `work`");
    }
    let blaze = PathBuf::from(&given.blaze);
    let work = PathBuf::from(&given.work);
    let engine = run::Engine {
        engine: blaze.with_file_name("blaze-engine"),
        blaze,
        home: work.join("home"),
        cache: work.join("cache"),
        tmp: work.join("tmp"),
        memory_gb: given.memory_gb,
        cores: given.cores,
        limit: Duration::from_secs(given.timeout.max(1) as u64),
    };
    for dir in [&engine.home, &engine.cache, &engine.tmp] {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", dir.display())));
    }
    let mut profiles = BTreeMap::from([("dev".to_owned(), String::new())]);
    if all.iter().any(|it| it.plans.iter().any(|(_, plan)| plan.panic_abort)) {
        profiles.insert("abort".to_owned(), "\n[profile.dev]\npanic = \"abort\"\n".to_owned());
    }
    if let Err(why) = run::warm(&engine, &work, &profiles, Duration::from_secs(given.warm_timeout.max(1) as u64)) {
        let _ = engine.stop();
        eprintln!("{SCRIPT}: {why}");
        blaze_script::exit(2);
    }
    println!("{SCRIPT}: warm in {} s; {} tests, {} with a plan, {} at once", began.elapsed().as_secs(), all.len(), all.iter().filter(|it| it.not_run.is_none()).count(), given.jobs);
    let chunk = if given.restart_every > 0 { given.restart_every as usize } else { usize::MAX };
    let mut done = 0;
    for batch in all.chunks(chunk) {
        let one = |case: &Case| -> Row {
            let started = Instant::now();
            let mut row = Row { suite: case.suite, id: case.id.clone(), class: "pass", cause: String::new(), detail: String::new(), ms: 0, bytes: case.bytes, not_held: Vec::new(), hints: (0, 0, Vec::new()) };
            if let Some((class, why)) = &case.not_run {
                (row.class, row.cause) = (*class, why.clone());
                return row;
            }
            let dir = work.join("t").join(case.suite.name()).join(work_name(&case.id));
            let mut notes = run::Notes::default();
            let verdict = engine.test(case.suite, &suite_root(case.suite), &dir, &case.plans, &mut notes);
            row.not_held = case.plans.iter().flat_map(|(_, plan)| plan.not_held.iter().cloned()).collect();
            row.not_held.sort();
            row.not_held.dedup();
            row.hints = (notes.hints_held, notes.hints_not_held, notes.reused_though_changed);
            match verdict {
                Verdict::Pass => {
                    if !given.keep {
                        let _ = std::fs::remove_dir_all(&dir);
                    }
                }
                Verdict::Fail { cause, detail } => (row.class, row.cause, row.detail) = ("fail", cause, detail),
                Verdict::Panic { cause, detail } => (row.class, row.cause, row.detail) = ("panic", cause, detail),
                Verdict::Timeout { step } => (row.class, row.cause) = ("timeout", step),
            }
            row.ms = started.elapsed().as_millis();
            row
        };
        fan_out(given.jobs.max(1) as usize, batch, one, |row| {
            done += 1;
            if row.class != "pass" && row.class != "unsupported" && row.class != "ignored" {
                println!("{SCRIPT}: {} {}/{}: {}", row.class, row.suite.name(), row.id, row.cause);
            }
            if done % 200 == 0 {
                println!("{SCRIPT}: {done} of {} in {} s", all.len(), began.elapsed().as_secs());
            }
            rows.push(row);
        });
        match engine.stop() {
            Ok(said) => println!("{SCRIPT}: after {done} tests, {said}"),
            Err(why) => println!("{SCRIPT}: after {done} tests, `blaze stop` failed: {why}"),
        }
    }
    println!("\n{SCRIPT}: {} tests in {} s on {} with {}", rows.len(), began.elapsed().as_secs(), host.triple(), given.blaze);
    let summary = table(&rows);
    let shown: Vec<&Row> = rows.iter().collect();
    print_causes("why tests do not pass", &shown, &["fail", "panic", "timeout"], 30);
    print_causes("not supported", &shown, &["unsupported"], 30);
    let not_held: usize = rows.iter().filter(|it| it.class == "pass" && !it.not_held.is_empty()).count();
    let clean: (usize, usize, usize) = rows.iter().fold((0, 0, 0), |sum, it| (sum.0 + it.hints.0, sum.1 + it.hints.1, sum.2 + it.hints.2.len()));
    println!("\n{not_held} of the passes also expect something the runner does not hold (a rustfix result, NOTE annotations, rustc's error-pattern); `not_held` names it on each line of the report");
    if suites.contains(&Suite::Incremental) {
        println!("rustc_clean: {} held, {} with no blaze meaning or not read, {} functions reused though the test changes them", clean.0, clean.1, clean.2);
    }
    if !given.out.is_empty() {
        let mut text = format!("{}\n", json::json!({"summary": summary, "seconds": began.elapsed().as_secs(), "host": host.triple(), "part": part}));
        for row in &rows {
            text.push_str(&format!("{}\n", row.json()));
        }
        std::fs::write(&given.out, text).unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", given.out)));
    }
    let mut problems: Vec<String> = Vec::new();
    let ran = rows.iter().filter(|it| ["pass", "fail", "panic", "timeout"].contains(&it.class)).count();
    if part && (ran as i64) < given.min_ran {
        problems.push(format!("{ran} tests ran, fewer than `min_ran` ({})", given.min_ran));
    }
    if ran == 0 {
        problems.push("no test ran".to_owned());
    }
    for suite in &suites {
        let path = root.join(format!("harness/compiletest/{}.toml", suite.name()));
        let seen: Vec<expect::Seen<'_>> = rows.iter().filter(|it| it.suite == *suite).map(|it| expect::Seen { id: &it.id, class: it.class, cause: &it.cause }).collect();
        if given.expectations == "off" {
            continue;
        }
        let before = match std::fs::read_to_string(&path) {
            Ok(text) => expect::read(&text).unwrap_or_else(|why| fail(SCRIPT, &format!("{}: {why}", path.display()))),
            Err(_) if given.expectations == "record" => expect::Expected::default(),
            Err(why) => fail(SCRIPT, &format!("{}: {why}", path.display())),
        };
        if given.expectations == "record" {
            let after = expect::recorded(&before, &seen, !part);
            std::fs::write(&path, expect::write(suite.name(), &after)).unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", path.display())));
            println!("{SCRIPT}: wrote {} ({} listed)", path.display(), after.listed.len());
            continue;
        }
        let found = expect::check(&before, &seen, !part);
        if !found.is_empty() {
            println!("\n{}: {} differences from {} (the first 40):", suite.name(), found.len(), path.display());
            for line in found.iter().take(40) {
                println!("  {line}");
            }
            problems.push(format!("{}: {} differences from its expectations", suite.name(), found.len()));
        }
    }
    if !problems.is_empty() {
        for it in &problems {
            eprintln!("{SCRIPT}: {it}");
        }
        blaze_script::exit(1);
    }
}
