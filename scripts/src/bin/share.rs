//! share: the programs of `crates/hir-ty-share/programs/` built by blaze, each program's stdout
//! held to the `<name>.stdout` beside it. The port of `harness/v2/share.py`, to be deleted once
//! both were run on the same tree and agreed (docs/v2/plan/design/scripts.md, "Ports and their
//! parity"). `DOC` below is what its usage error prints.
//!
//!     blaze run -q -p scripts --bin share -- '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/share.json"}'
//!     blaze run -q -p scripts --bin share -- selftest
//!
//! The first harness on `blaze_script::engine` and `report`: N programs at once
//! (`blaze_script::fan_out`), one daemon for all.

use std::path::{Path, PathBuf};

use blaze_script::{
    Value,
    engine::{self, Engine, EngineSpec, Fault, Ran},
    fail, fan_out,
    input::{self, Fields, Input, InputError},
    py_round,
    pypath::pure_path,
    pystr::splitlines,
    read_text,
    report::{Number, Outcome, Report, Verdict},
};

/// What the usage error prints: harness/v2/share.py's docstring.
const DOC: &str = r#"share.py: the programs of `crates/hir-ty-share/programs/` built by blaze, each program's
stdout held to the `<name>.stdout` beside it
(`fastcompile-ident-share 2` in fed1eb428d:docs/v2/requests/fastcompile.md; docs/v2/plan/design/compile-time.md).

    uv run --frozen python harness/v2/share.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/share.json"}'
    uv run --frozen python harness/v2/share.py '{"engine": {...}, "out": "...", "programs": ["type_id", "statics"]}'
    uv run --frozen python harness/v2/share.py selftest

These are where sharing one instance's code between type arguments (idea 2) would show if it
were wrong: `TypeId`, `type_name`, `needs_drop`, statics in generic functions, associated
consts, const generics, drop order, `dyn` vtables and function pointers. The expected outputs
were written from what Rust guarantees.

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec)
    out       the path the result JSON is written to (report.py)
    work      the directory everything is built in; empty: a new one under /tmp
    dir       the programs' directory ("crates/hir-ty-share/programs", from the repository)
    programs  which programs run, by name; empty: all of them
    jobs      how many programs build at once (4); blaze's side is one daemon for all
    keep      keep the work directory after the run

The cases are the directory's: every `<name>.rs` with a `<name>.stdout` beside it. A `.rs`
without one is an error naming it, not a program left out.

blaze builds each program as a one-file package (`src/main.rs`) in a workspace of its own,
a binary in the dev profile at edition 2024, as corpus.py's steps are built, and it is run
from an empty directory. One outcome per program:

    pass   it exits 0 and its stdout is `<name>.stdout`
    fail   its stdout or exit is not that (named with both texts), or blaze does not build
           it, or the engine says the build was not blaze's
    error  a command timed out, or the harness could not do a step
"#;

const SCRIPT: &str = "share.py";
const PROGRAMS: &str = "crates/hir-ty-share/programs";
const EDITION: &str = "2024";
const BUILD_SECS: f64 = 1800.0;
const RUN_SECS: f64 = 120.0;

struct Args {
    engine: EngineSpec,
    out: String,
    work: String,
    dir: String,
    programs: Vec<String>,
    jobs: i64,
    keep: bool,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["engine", "out", "work", "dir", "programs", "jobs", "keep"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            engine: f.object_required("engine")?,
            out: f.str("out", None)?,
            work: f.str("work", Some(""))?,
            dir: f.str("dir", Some(PROGRAMS))?,
            programs: f.strs("programs")?,
            jobs: f.int("jobs", Some(4))?,
            keep: f.bool("keep", Some(false))?,
        })
    }
}

/// A program and the stdout it has to print.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Case {
    name: String,
    source: PathBuf,
    expected: String,
}

/// The cases of `directory` in order of name, and each `.rs` in it that has no `.stdout`.
fn cases(directory: &Path) -> (Vec<Case>, Vec<String>) {
    let mut sources: Vec<PathBuf> = std::fs::read_dir(directory)
        .map(|entries| entries.filter_map(Result::ok).map(|it| it.path()).collect())
        .unwrap_or_default();
    sources.retain(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".rs")));
    sources.sort();
    let mut found = Vec::new();
    let mut lone = Vec::new();
    for source in sources {
        let file = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let stem = file.strip_suffix(".rs").unwrap_or(&file).to_owned();
        let expected = source.with_file_name(format!("{stem}.stdout"));
        if expected.is_file() {
            let text = read_text(&expected)
                .unwrap_or_else(|| fail(SCRIPT, &format!("{} is not read as UTF-8 text", expected.display())));
            found.push(Case { name: stem, source, expected: lines(&text) });
        } else {
            lone.push(file);
        }
    }
    (found, lone)
}

/// `text` with Windows line ends as Unix's, for comparing what a program printed.
fn lines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn indented(text: &str) -> String {
    let joined: Vec<String> = splitlines(text).iter().map(|line| format!("    {line}")).collect();
    if joined.is_empty() { "    (nothing)".to_owned() } else { joined.join("\n") }
}

/// `who`'s stdout of `case` beside the one expected, with the first line that differs.
fn against(case: &Case, who: &str, got: &str) -> String {
    let got = lines(got);
    let (want, have) = (splitlines(&case.expected), splitlines(&got));
    let at = want.iter().zip(&have).position(|(a, b)| a != b).unwrap_or(want.len().min(have.len()));
    format!(
        "{who}'s stdout is not {}.stdout (first difference at line {}):\n  expected:\n{}\n  {who} printed:\n{}",
        case.name,
        at + 1,
        indented(&case.expected),
        indented(&got)
    )
}

/// The verdict of `case` from what blaze's build and run gave (the run is None when the build
/// failed), and who built it when blaze did not.
fn judge(case: &Case, blaze_build: &Ran, blaze_run: Option<&Ran>, who: Option<&str>) -> (Verdict, String) {
    let mut problems: Vec<String> = Vec::new();
    if let Some(who) = who {
        problems.push(format!("blaze's build of {} is not blaze's: {who}", case.name));
    } else if let (0, Some(blaze_run)) = (blaze_build.returncode, blaze_run) {
        if blaze_run.returncode != 0 {
            problems.push(format!("blaze's {} exits {}:\n{}", case.name, blaze_run.returncode, blaze_run.said(1500)));
        }
        if lines(&blaze_run.stdout) != case.expected {
            problems.push(against(case, "blaze", &blaze_run.stdout));
        }
    } else {
        problems.push(format!(
            "blaze does not build {} (exit {}):\n{}",
            case.name,
            blaze_build.returncode,
            blaze_build.said(1500)
        ));
    }
    if problems.is_empty() {
        return (Verdict::Pass, String::new());
    }
    (Verdict::Fail, format!("{}: {}", case.name, problems.join("\n")))
}

/// The manifest of the one-file package of program `name`, a workspace of its own.
fn manifest(name: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"{EDITION}\"\npublish = false\n\n[[bin]]\nname = \"{name}\"\npath = \"src/main.rs\"\n\n[workspace]\n"
    )
}

/// What blaze's build and run of `case` gave, and who built it when blaze did not.
fn build_and_run(eng: &Engine, case: &Case, root: &Path, home: &Path, label: &str) -> Result<(Ran, Option<Ran>, Option<String>), Fault> {
    let (ws, cwd) = (root.join("ws"), root.join("cwd"));
    let _ = std::fs::remove_dir_all(root);
    let src = ws.join("src");
    std::fs::create_dir_all(&src).map_err(engine::os_at(&src))?;
    std::fs::create_dir(&cwd).map_err(engine::os_at(&cwd))?;
    let toml = ws.join("Cargo.toml");
    std::fs::write(&toml, manifest(&case.name)).map_err(engine::os_at(&toml))?;
    std::fs::copy(&case.source, src.join("main.rs")).map_err(engine::os_at(&case.source))?;
    let plain = engine::scrubbed();
    let env = eng.env(home, Some(&root.join("target")), &[])?;
    let blaze_build = eng.build(&format!("{label}: blaze build"), &ws, &env, &[], "build", BUILD_SECS)?;
    let who = eng.not_blazes(&blaze_build);
    let built = eng.build_dir(&ws, &env)?.join("debug").join(format!("{}{}", case.name, engine::EXE));
    let blaze_run = if blaze_build.returncode == 0 && who.is_none() {
        Some(engine::run(&format!("{label}: blaze's program"), &[built.display().to_string()], &cwd, &plain, RUN_SECS, false)?)
    } else {
        None
    };
    Ok((blaze_build, blaze_run, who))
}

/// `case` built and run by blaze under `root`, by the daemon of `home` (every case's, so one
/// store), into a target directory of its own under `root`. Never fails for what the harness
/// could not do: that is an outcome too.
fn run_case(eng: &Engine, case: &Case, root: &Path, home: &Path) -> Outcome {
    let label = format!("share/{}", case.name);
    match build_and_run(eng, case, root, home, &label) {
        Err(Fault::Unmeasurable(said)) => Outcome::new(&label, Verdict::Unmeasured, &said),
        Err(Fault::Failed(said) | Fault::Os(said)) => Outcome::new(&label, Verdict::Error, &said),
        Ok((blaze_build, blaze_run, who)) => {
            let (verdict, detail) = judge(case, &blaze_build, blaze_run.as_ref(), who.as_deref());
            let mut outcome = Outcome::new(&label, verdict, &detail);
            outcome.numbers.push(("blaze_s".to_owned(), Number::Float(py_round(blaze_build.seconds, 2))));
            outcome
        }
    }
}

/// The cases `programs` names, or all; a name with no case is refused.
fn chosen(args: &Args, found: &[Case]) -> Result<Vec<Case>, String> {
    let mut unknown: Vec<&str> =
        args.programs.iter().map(String::as_str).filter(|p| !found.iter().any(|c| c.name == *p)).collect();
    unknown.sort_unstable();
    unknown.dedup();
    if !unknown.is_empty() {
        return Err(format!("programs: {} has no {} (with a .stdout)", args.dir, unknown.join(", ")));
    }
    Ok(found.iter().filter(|c| args.programs.is_empty() || args.programs.contains(&c.name)).cloned().collect())
}

fn main_with(args: &Args) -> ! {
    if args.jobs < 1 {
        fail(SCRIPT, &format!("jobs: at least 1, not {}", args.jobs));
    }
    let dir = pure_path(&args.dir);
    let directory = if Path::new(&dir).is_absolute() { PathBuf::from(&dir) } else { engine::root().join(&dir) };
    if !directory.is_dir() {
        fail(SCRIPT, &format!("dir: no directory {}", directory.display()));
    }
    let (found, lone) = cases(&directory);
    let picked = chosen(args, &found).unwrap_or_else(|said| fail(SCRIPT, &said));
    let eng = Engine::of(&args.engine).unwrap_or_else(|f| fail(SCRIPT, f.said()));
    let work = engine::work_dir(&args.work, "bz-share-").unwrap_or_else(|f| fail(SCRIPT, f.said()));
    if let Err(e) = std::fs::create_dir_all(&work) {
        fail(SCRIPT, Fault::os(&e, &work).said());
    }
    let mut rep = Report::new(SCRIPT, &args.engine, &args.out);
    rep.extra.insert("work".to_owned(), Value::String(work.display().to_string()));
    rep.extra.insert("dir".to_owned(), Value::String(directory.display().to_string()));
    rep.extra.insert("programs".to_owned(), Value::Array(picked.iter().map(|c| Value::String(c.name.clone())).collect()));
    rep.extra.insert("toolchain".to_owned(), Value::String(engine::toolchain()));
    println!("work: {}", work.display());
    for name in &lone {
        let stem = name.strip_suffix(".rs").unwrap_or(name);
        rep.add(
            &format!("share/{name}"),
            Verdict::Error,
            &format!("{} has no {stem}.stdout beside it: every program of the directory is a case", directory.join(name).display()),
            Vec::new(),
        );
    }
    let home = work.join("h");
    // In the order of the programs, not the order they end in: one report for one input.
    let jobs = usize::try_from(args.jobs).unwrap_or(1);
    fan_out(jobs, &picked, |c| run_case(&eng, c, &work.join(&c.name), &home), |outcome| rep.add_outcome(outcome));
    if home.is_dir() {
        let stopped = eng.env(&home, None, &[]).and_then(|env| eng.stop(&work, &env));
        if let Err(f) = stopped {
            rep.add("share: stopping the daemon", Verdict::Error, f.said(), Vec::new());
        }
    }
    if !args.keep {
        let _ = std::fs::remove_dir_all(&work);
    }
    rep.finish()
}

fn ran(returncode: i32, stdout: &str, stderr: &str) -> Ran {
    Ran { argv: vec!["x".to_owned()], seconds: 0.5, returncode, stdout: stdout.to_owned(), stderr: stderr.to_owned() }
}

fn case() -> Case {
    Case { name: "type_id".to_owned(), source: PathBuf::from("type_id.rs"), expected: "false\ntrue\ntrue\nfalse\n".to_owned() }
}

/// Python's `assert holds, what`, as the selftest prints the `AssertionError`.
fn check(holds: bool, what: String) -> Result<(), String> {
    if holds { Ok(()) } else { Err(format!("AssertionError: {what}")) }
}

fn test_cases() -> Result<(), String> {
    let tmp = blaze_script::TempDir::new("share-selftest-").map_err(|e| format!("OSError: {e}"))?;
    let top = tmp.path();
    for (name, body) in
        [("b.rs", "fn main() {}\n"), ("b.stdout", "x\n"), ("a.rs", ""), ("a.stdout", ""), ("lone.rs", ""), ("orphan.stdout", "")]
    {
        std::fs::write(top.join(name), body).map_err(|e| format!("OSError: {e}"))?;
    }
    let (found, lone) = cases(top);
    let got: Vec<(&str, &str)> = found.iter().map(|c| (c.name.as_str(), c.expected.as_str())).collect();
    check(got == [("a", ""), ("b", "x\n")], format!("{got:?}"))?;
    check(lone == ["lone.rs"], format!("{lone:?}"))?;
    let (found, lone) = cases(&engine::root().join(PROGRAMS));
    let names: Vec<&str> = found.iter().map(|c| c.name.as_str()).collect();
    check(
        lone.is_empty() && names.len() >= 9 && ["type_id", "statics", "drop_order"].iter().all(|n| names.contains(n)),
        format!("({names:?}, {lone:?})"),
    )
}

fn test_judge() -> Result<(), String> {
    let c = case();
    let good = ran(0, &c.expected, "");
    check(judge(&c, &ran(0, "", ""), Some(&good), None) == (Verdict::Pass, String::new()), String::new())?;
    let crlf = ran(0, &c.expected.replace('\n', "\r\n"), "");
    check(judge(&c, &ran(0, "", ""), Some(&crlf), None) == (Verdict::Pass, String::new()), String::new())?;
    let (verdict, detail) = judge(&c, &ran(0, "", ""), Some(&ran(0, "false\ntrue\ntrue\n", "")), None);
    check(verdict == Verdict::Fail && detail.contains("blaze's stdout is not type_id.stdout") && detail.contains("line 4"), detail.clone())?;
    check(detail.contains("expected:\n    false") && detail.contains("blaze printed:\n    false"), detail)?;
    let (verdict, detail) = judge(&c, &ran(101, "", "error: x"), None, None);
    check(verdict == Verdict::Fail && detail.contains("blaze does not build type_id (exit 101)") && detail.contains("error: x"), detail)?;
    let (verdict, detail) = judge(&c, &ran(0, "", ""), Some(&ran(101, &c.expected, "panicked")), None);
    check(verdict == Verdict::Fail && detail.contains("blaze's type_id exits 101"), detail)?;
    let (verdict, detail) = judge(&c, &ran(0, "", ""), None, Some("cargo: no daemon"));
    check(verdict == Verdict::Fail && detail.contains("not blaze's: cargo: no daemon"), detail)?;
    // A wrong exit and a wrong stdout: both are said.
    let (verdict, detail) = judge(&c, &ran(0, "", ""), Some(&ran(1, "y\n", "")), None);
    check(verdict == Verdict::Fail && detail.contains("exits 1") && detail.contains("blaze's stdout"), detail)
}

fn test_package() -> Result<(), String> {
    let text = manifest("drop_order");
    check(
        text.contains("[package]\nname = \"drop_order\"") && text.contains("edition = \"2024\"") && text.ends_with("[workspace]\n"),
        String::new(),
    )
}

fn test_input() -> Result<(), String> {
    let read = |text: &str| input::read(text).and_then(|value| input::of::<Args>(&value, ""));
    let args = read(r#"{"engine": {"kind": "v2", "blaze": "b"}, "out": "", "programs": ["type_id"]}"#).map_err(|e| e.0)?;
    check(args.programs == ["type_id"] && args.dir == PROGRAMS && args.jobs == 4, String::new())?;
    let statics = Case { name: "statics".to_owned(), source: PathBuf::from("s.rs"), expected: String::new() };
    let picked: Vec<String> = chosen(&args, &[case(), statics])?.into_iter().map(|c| c.name).collect();
    check(picked == ["type_id"], String::new())?;
    let unknown = Args { programs: vec!["nope".to_owned()], ..args };
    match chosen(&unknown, &[case()]) {
        Err(said) => check(said.contains("nope"), said)?,
        Ok(_) => return Err("AssertionError: an unknown program was taken".to_owned()),
    }
    match read(r#"{"engine": {"kind": "v2", "blaze": "b"}, "out": "", "program": []}"#) {
        Err(said) => check(said.0.contains("no field program"), said.0),
        Ok(_) => Err("AssertionError: an unknown field was taken".to_owned()),
    }
}

type Test = (&'static str, fn() -> Result<(), String>);
const SELFTESTS: [Test; 4] =
    [("test_cases", test_cases), ("test_judge", test_judge), ("test_package", test_package), ("test_input", test_input)];

fn selftest() -> ! {
    let mut failed = 0;
    for (name, test) in SELFTESTS {
        match test() {
            Ok(()) => println!("ok    {name}"),
            Err(said) => {
                failed += 1;
                println!("FAIL  {name}: {said}");
            }
        }
    }
    println!("{SCRIPT} selftest: {} of {} pass", SELFTESTS.len() - failed, SELFTESTS.len());
    blaze_script::exit(if failed > 0 { 1 } else { 0 })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            blaze_script::exit(1)
        }
    }
}
