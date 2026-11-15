//! lint_fix: CI's lints on blaze's tree, fixed where a fix is machine-applicable and refused
//! where none is. The `lint-baselines` job of .github/workflows/blaze.yaml runs it.
//!
//!     blaze run -q -p scripts --bin lint_fix -- '{"event": "pull_request", "head_ref": "my-branch", "out": "/tmp/lf"}'
//!     blaze run -q -p scripts --bin lint_fix -- selftest
//!
//! The owner's rule (2026-11-12): a lint in CI fixes what it can in a commit of its own rather
//! than refusing the change, and refuses only what no fix makes.
//!
//! On a pull request from a branch of this repository (`event` `pull_request`, `fork` false):
//!
//! 1. `blaze check <select> --message-format json` gives the findings before.
//! 2. `blaze check <select> --fix` makes every machine-applicable suggestion
//!    (crates/blaze-cargo/src/cli/fix.rs); `git diff --name-only` is what it changed.
//! 3. `blaze check <select> --message-format json` again gives the findings after.
//! 4. When files changed, one commit, authored by `github-actions[bot]`: `blaze check --fix: <N>
//!    fixes of <lints>`, N from `--fix`'s `Fixed` line and the lints those whose findings fell.
//!    Its hash is written to `<out>/commit`, and the workflow's next step pushes it to the pull
//!    request's branch (`head_ref`) with the bot's token. This script pushes nothing, and holds
//!    no token. A fix is never a failure: it is said as a notice.
//! 5. Every error-level finding left (each `deny` lint, each compile error) fails the run, said
//!    as `path:line:col: lint: message`, grouped by lint with each group's count and the lint's
//!    help.
//!
//! The loop guard: when the head commit is already the bot's fix commit and `--fix` changes files
//! again, nothing is committed and the run fails naming the lints whose fixes do not converge.
//!
//! A fork's pull request gets no commit and no push (its run has no write token, and gets none:
//! AGENTS.md's review rules). When `--fix` changes files, the diff is written to
//! `<out>/lint-fix.diff` for the workflow to upload, and the run fails with the one command that
//! makes the same fixes locally.
//!
//! On any other event (a push to main, a merge group, a run by hand) nothing is fixed: step 5
//! alone. main is never pushed to: a `head_ref` of `main` is refused before anything runs.
//!
//! Input: `{"root": ".", "blaze": "blaze", "select": ["--workspace"], "event": "<event>",
//! "fork": false, "head_ref": "<branch>", "out": "<dir>"}`; `head_ref` and `out` are needed on
//! a pull request. Exit 0 when nothing is left to refuse, 1 otherwise.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use blaze_script::{
    Ran, fail, git,
    input::{self, Fields, Input, InputError},
    run,
};
use serde_json::Value;

const SCRIPT: &str = "lint_fix";
/// The bot every fix commit is authored by, as fmt.yaml commits as it.
const BOT_NAME: &str = "github-actions[bot]";
const BOT_EMAIL: &str = "41898282+github-actions[bot]@users.noreply.github.com";
/// What every fix commit's subject starts with.
const SUBJECT: &str = "blaze check --fix: ";
/// The diff a fork's run leaves, under `out`.
const DIFF: &str = "lint-fix.diff";
/// The fix commit's hash, under `out`: the workflow pushes it when it is there.
const COMMIT: &str = "commit";
/// The events a run is for; only `pull_request` fixes.
const EVENTS: [&str; 5] = ["pull_request", "push", "merge_group", "workflow_dispatch", "schedule"];

struct Args {
    root: String,
    blaze: String,
    select: Vec<String>,
    event: String,
    fork: bool,
    head_ref: Option<String>,
    out: Option<String>,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["root", "blaze", "select", "event", "fork", "head_ref", "out"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        let select = f.strs("select")?;
        Ok(Args {
            root: f.str("root", Some("."))?,
            blaze: f.str("blaze", Some("blaze"))?,
            select: if f.get("select").is_some() { select } else { vec!["--workspace".to_owned()] },
            event: f.one_of("event", &EVENTS, None)?,
            fork: f.bool("fork", Some(false))?,
            head_ref: f.str_or_null("head_ref")?,
            out: f.str_or_null("out")?,
        })
    }
}

/// What the run reaches outside itself: git, blaze and the files it leaves. The real one runs
/// them; the selftest's answers as a scripted checkout would.
trait World {
    /// `git <args>`: its standard output, `Err` when it failed.
    fn git(&mut self, args: &[&str]) -> Result<String, String>;
    /// `blaze <args>`: what it left, whatever its status.
    fn blaze(&mut self, args: &[String]) -> Result<Ran, String>;
    /// `text` written to `name` under the run's `out` directory.
    fn leave(&mut self, name: &str, text: &str) -> Result<(), String>;
}

/// One diagnostic of `blaze check --message-format json`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Finding {
    /// The lint's name, or `error` for a diagnostic with no code.
    lint: String,
    path: String,
    line: u64,
    column: u64,
    message: String,
    /// The first `help` child's text.
    help: Option<String>,
    /// Whether it is error level: a `deny` lint or a compile error.
    error: bool,
    /// Whether one of its suggestions is machine-applicable.
    fixable: bool,
}

/// Whether `diagnostic` or a child of it has a machine-applicable suggestion.
fn has_fix(diagnostic: &Value) -> bool {
    let spans = diagnostic["spans"].as_array().into_iter().flatten();
    spans.into_iter().any(|span| span["suggestion_applicability"] == "MachineApplicable")
        || diagnostic["children"].as_array().into_iter().flatten().any(has_fix)
}

/// The findings of a check's JSON lines; a line that is not JSON is refused, naming it. Lines of
/// another `reason` (artifacts, `build-finished`) are not findings.
fn findings(stdout: &str) -> Result<Vec<Finding>, String> {
    let mut out = Vec::new();
    for (at, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|e| format!("line {} of blaze check's JSON output is not JSON ({e}): {line}", at + 1))?;
        if value["reason"] != "compiler-message" {
            continue;
        }
        let message = &value["message"];
        let level = message["level"].as_str().unwrap_or("");
        if !matches!(level, "error" | "warning") {
            continue;
        }
        let spans = message["spans"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let primary = spans.iter().find(|span| span["is_primary"] == true).or(spans.first());
        let (path, line, column) = match primary {
            Some(span) => (
                span["file_name"].as_str().unwrap_or("").to_owned(),
                span["line_start"].as_u64().unwrap_or(0),
                span["column_start"].as_u64().unwrap_or(0),
            ),
            None => (String::new(), 0, 0),
        };
        let children = message["children"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        out.push(Finding {
            lint: message["code"]["code"].as_str().unwrap_or("error").to_owned(),
            path,
            line,
            column,
            message: message["message"].as_str().unwrap_or("").to_owned(),
            help: children.iter().find(|it| it["level"] == "help").and_then(|it| it["message"].as_str()).map(str::to_owned),
            error: level == "error",
            fixable: has_fix(message),
        });
    }
    Ok(out)
}

/// The error-level findings, by lint, each group sorted by place: what fails the run.
fn denied(found: &[Finding]) -> BTreeMap<&str, Vec<&Finding>> {
    let mut out: BTreeMap<&str, Vec<&Finding>> = BTreeMap::new();
    for it in found.iter().filter(|it| it.error) {
        out.entry(it.lint.as_str()).or_default().push(it);
    }
    for group in out.values_mut() {
        group.sort_by(|a, b| (&a.path, a.line, a.column, &a.message).cmp(&(&b.path, b.line, b.column, &b.message)));
    }
    out
}

/// The refusal of what is left: per lint its count, each finding as `path:line:col: lint:
/// message`, and the lint's help once.
fn said_denied(groups: &BTreeMap<&str, Vec<&Finding>>) -> Vec<String> {
    let total: usize = groups.values().map(Vec::len).sum();
    let mut out = vec![format!(
        "::error::{total} error-level finding(s) of {} lint(s) have no machine-applicable fix: each is fixed by hand (or, where it is right, `#[expect(<lint>, reason = \"...\")]` at the place)",
        groups.len()
    )];
    for (lint, group) in groups {
        out.push(format!("{lint}: {} finding(s)", group.len()));
        for it in group {
            out.push(format!("{}:{}:{}: {lint}: {}", it.path, it.line, it.column, it.message));
        }
        if let Some(help) = group.iter().find_map(|it| it.help.as_deref()) {
            out.push(format!("  help: {help}"));
        }
    }
    out
}

/// The lints a fix took findings from: those with fewer findings after it, else (a fix that
/// moved findings without lowering a count) those that had a machine-applicable suggestion
/// before it.
fn fixed_lints(before: &[Finding], after: &[Finding]) -> Vec<String> {
    let count = |found: &[Finding]| {
        let mut out: BTreeMap<String, usize> = BTreeMap::new();
        for it in found {
            *out.entry(it.lint.clone()).or_default() += 1;
        }
        out
    };
    let (was, now) = (count(before), count(after));
    let fell: Vec<String> = was.iter().filter(|(lint, n)| now.get(*lint).copied().unwrap_or(0) < **n).map(|(lint, _)| lint.clone()).collect();
    if !fell.is_empty() {
        return fell;
    }
    let mut fixable: Vec<String> = before.iter().filter(|it| it.fixable).map(|it| it.lint.clone()).collect();
    fixable.sort_unstable();
    fixable.dedup();
    fixable
}

/// The suggestions `--fix` made, from its `Fixed <N> suggestion(s) in ...` line; `None` when it
/// said none, which is a `--fix` that stopped.
fn suggestions_made(stderr: &str) -> Option<usize> {
    stderr.lines().find_map(|line| {
        let rest = &line[line.find("Fixed ")? + "Fixed ".len()..];
        rest.split(' ').next()?.parse().ok()
    })
}

/// The check's findings: its JSON lines read, or the run refused when it gave no finding and
/// still failed (it could not run).
fn check(world: &mut impl World, select: &[String]) -> Result<Vec<Finding>, String> {
    let mut argv = vec!["check".to_owned()];
    argv.extend(select.iter().cloned());
    argv.extend(["--message-format".to_owned(), "json".to_owned()]);
    let ran = world.blaze(&argv)?;
    let found = findings(&ran.out())?;
    if ran.status != 0 && !found.iter().any(|it| it.error) {
        return Err(format!("`blaze {}` exited {} with no error-level finding: it did not run. It said:\n{}", argv.join(" "), ran.status, ran.err().trim()));
    }
    Ok(found)
}

/// What a run said and how it ends.
#[derive(Debug, Default)]
struct Outcome {
    said: Vec<String>,
    failed: bool,
}

/// The run, on `world`.
fn decide(world: &mut impl World, it: &Args) -> Result<Outcome, String> {
    let mut done = Outcome::default();
    let fixing = it.event == "pull_request";
    if fixing && it.head_ref.as_deref().is_none_or(|head| head.is_empty() || head == "main") {
        return Err(format!(
            "a pull request's run needs `head_ref`, its branch, and it is not `main`: fixes are committed to the pull request's branch, never to main (got {:?})",
            it.head_ref
        ));
    }
    if fixing && it.out.is_none() {
        return Err("a pull request's run needs `out`, the directory the commit's hash or a fork's diff is left in".to_owned());
    }
    let after = if fixing {
        let dirty = world.git(&["status", "--porcelain", "--untracked-files=no"])?;
        if !dirty.trim().is_empty() {
            return Err(format!("the checkout has changes before `--fix` ran, which would be committed as its fixes:\n{}", dirty.trim_end()));
        }
        let head = world.git(&["log", "-1", "--format=%ae%x1f%s"])?;
        let (email, subject) = head.trim_end().split_once('\u{1f}').unwrap_or(("", ""));
        let head_is_fix = email == BOT_EMAIL && subject.starts_with(SUBJECT);
        let before = check(world, &it.select)?;
        let mut argv = vec!["check".to_owned()];
        argv.extend(it.select.iter().cloned());
        argv.push("--fix".to_owned());
        let ran = world.blaze(&argv)?;
        let Some(made) = suggestions_made(&ran.err()) else {
            return Err(format!("`blaze {}` stopped before it fixed anything (exit {}). It said:\n{}", argv.join(" "), ran.status, ran.err().trim()));
        };
        let changed: Vec<String> = world.git(&["diff", "--name-only"])?.lines().map(str::to_owned).collect();
        let after = check(world, &it.select)?;
        if !changed.is_empty() {
            let lints = fixed_lints(&before, &after).join(", ");
            let files = changed.join(", ");
            if it.fork {
                let diff = world.git(&["diff", "--binary"])?;
                world.leave(DIFF, &diff)?;
                done.said.push(format!(
                    "::error::blaze check --fix makes {made} fix(es) of {lints} in {}: {files}. A fork's pull request is not pushed to (its run has no write token); the diff is the `{DIFF}` artifact. To make them: `blaze check --workspace --fix`, then commit.",
                    changed.len()
                ));
                done.failed = true;
            } else if head_is_fix {
                done.said.push(format!(
                    "::error::the head is already `blaze check --fix`'s commit, and --fix changed {files} again: the fixes of {lints} do not converge (each run makes the next's findings). Nothing is committed; that lint's fix is the bug."
                ));
                done.failed = true;
            } else {
                let message = format!("{SUBJECT}{made} fixes of {lints}");
                world.git(&["add", "-u"])?;
                world.git(&[
                    "-c",
                    &format!("user.name={BOT_NAME}"),
                    "-c",
                    &format!("user.email={BOT_EMAIL}"),
                    "commit",
                    "-q",
                    "-m",
                    &message,
                ])?;
                let hash = world.git(&["rev-parse", "HEAD"])?;
                world.leave(COMMIT, hash.trim())?;
                done.said.push(format!(
                    "::notice::{message}, in {}: {files}. Committed as {BOT_NAME} for the next step to push to {}.",
                    changed.len(),
                    it.head_ref.as_deref().unwrap_or("")
                ));
            }
        } else {
            done.said.push("blaze check --fix: nothing machine-applicable to fix".to_owned());
        }
        after
    } else {
        done.said.push(format!("{}: nothing is fixed on a `{}` run, only checked", SCRIPT, it.event));
        check(world, &it.select)?
    };
    let groups = denied(&after);
    if groups.is_empty() {
        done.said.push("no error-level finding is left".to_owned());
    } else {
        done.said.extend(said_denied(&groups));
        done.failed = true;
    }
    Ok(done)
}

/// The world CI runs in.
struct Real {
    root: PathBuf,
    blaze: String,
    out: Option<PathBuf>,
}

impl World for Real {
    fn git(&mut self, args: &[&str]) -> Result<String, String> {
        git(&self.root, args)
    }

    fn blaze(&mut self, args: &[String]) -> Result<Ran, String> {
        let mut argv = vec![self.blaze.as_str()];
        argv.extend(args.iter().map(String::as_str));
        run(&argv, &self.root)
    }

    fn leave(&mut self, name: &str, text: &str) -> Result<(), String> {
        let dir = self.out.as_ref().ok_or_else(|| format!("{name} is to be left, and no `out` was given"))?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{} is not made: {e}", dir.display()))?;
        std::fs::write(dir.join(name), text).map_err(|e| format!("{} is not written: {e}", dir.join(name).display()))
    }
}

fn main_with(it: &Args) {
    let root = std::fs::canonicalize(&it.root).unwrap_or_else(|e| fail(SCRIPT, &format!("the root {} is no directory: {e}", it.root)));
    // A path (`target/release/blaze`) is the root's; a bare name is found on PATH.
    let blaze = if Path::new(&it.blaze).components().count() > 1 { root.join(&it.blaze).to_string_lossy().into_owned() } else { it.blaze.clone() };
    let mut world = Real { root, blaze, out: it.out.as_ref().map(PathBuf::from) };
    let done = decide(&mut world, it).unwrap_or_else(|e| fail(SCRIPT, &e));
    println!("{}", done.said.join("\n"));
    std::process::exit(i32::from(done.failed));
}

/// A scripted checkout: `before` and `after` are the check's JSON before and after `--fix`, which
/// changes `changes`; every call is logged.
struct Fake {
    head: String,
    before: String,
    after: String,
    fixed: String,
    changes: Vec<String>,
    ran_fix: bool,
    log: Vec<String>,
    left: BTreeMap<String, String>,
}

impl Fake {
    fn new(head: &str, before: &str, after: &str, made: usize, changes: &[&str]) -> Fake {
        Fake {
            head: head.to_owned(),
            before: before.to_owned(),
            after: after.to_owned(),
            fixed: format!("       Fixed {made} suggestion(s) in {} file(s)\n", changes.len()),
            changes: changes.iter().map(|it| (*it).to_owned()).collect(),
            ran_fix: false,
            log: Vec::new(),
            left: BTreeMap::new(),
        }
    }

    fn committed(&self) -> bool {
        self.log.iter().any(|it| it.starts_with("git -c") && it.contains(" commit "))
    }
}

impl World for Fake {
    fn git(&mut self, args: &[&str]) -> Result<String, String> {
        self.log.push(format!("git {}", args.join(" ")));
        Ok(match args {
            ["log", ..] => self.head.clone(),
            ["diff", "--name-only"] if self.ran_fix => self.changes.iter().map(|it| format!("{it}\n")).collect(),
            ["diff", "--binary"] => "diff --git a/x b/x\n".to_owned(),
            ["rev-parse", "HEAD"] => "f1x\n".to_owned(),
            ["push", ..] => return Err("the selftest never pushes".to_owned()),
            _ => String::new(),
        })
    }

    fn blaze(&mut self, args: &[String]) -> Result<Ran, String> {
        self.log.push(format!("blaze {}", args.join(" ")));
        if args.iter().any(|it| it == "--fix") {
            self.ran_fix = true;
            return Ok(Ran { status: 0, stdout: Vec::new(), stderr: self.fixed.clone().into_bytes() });
        }
        let out = if self.ran_fix { &self.after } else { &self.before };
        let status = i32::from(findings(out).is_ok_and(|it| it.iter().any(|f| f.error)));
        Ok(Ran { status, stdout: out.clone().into_bytes(), stderr: Vec::new() })
    }

    fn leave(&mut self, name: &str, text: &str) -> Result<(), String> {
        self.left.insert(name.to_owned(), text.to_owned());
        Ok(())
    }
}

/// A `compiler-message` line of `lint` at `path:line:col`, error or warning level, with a
/// machine-applicable suggestion or not.
fn message(lint: &str, error: bool, path: &str, line: u64, fixable: bool) -> String {
    let applicability = if fixable { "\"MachineApplicable\"" } else { "null" };
    let level = if error { "error" } else { "warning" };
    format!(
        r#"{{"reason":"compiler-message","package_id":"p","manifest_path":"m","target":{{}},"message":{{"rendered":"r","$message_type":"diagnostic","children":[{{"children":[],"code":null,"level":"help","message":"the help of {lint}","rendered":null,"spans":[{{"file_name":"{path}","is_primary":true,"line_start":{line},"column_start":5,"suggestion_applicability":{applicability}}}]}}],"level":"{level}","message":"a finding of {lint}","spans":[{{"file_name":"{path}","is_primary":true,"line_start":{line},"column_start":5,"suggestion_applicability":null}}],"code":{{"code":"{lint}","explanation":null}}}}}}"#
    )
}

fn args(event: &str, fork: bool) -> Args {
    Args {
        root: ".".to_owned(),
        blaze: "blaze".to_owned(),
        select: vec!["--workspace".to_owned()],
        event: event.to_owned(),
        fork,
        head_ref: Some("a-branch".to_owned()),
        out: Some("out".to_owned()),
    }
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

fn selftest() {
    let author = "someone@example.com\u{1f}a change\n";
    let bot = format!("{BOT_EMAIL}\u{1f}{SUBJECT}2 fixes of blaze::std_use\n");
    let fixable = [message("blaze::std_use", true, "crates/a/src/lib.rs", 3, true), message("blaze::std_use", true, "crates/a/src/lib.rs", 9, true)].join("\n");
    let unfixable = message("blaze::env_read", true, "crates/a/src/main.rs", 7, false);
    let warned = message("clippy::needless_return", false, "crates/a/src/lib.rs", 1, true);
    let run = |fake: &mut Fake, it: &Args| decide(fake, it).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));

    // A fix changed files and nothing is left: one commit as the bot, its hash left for the push,
    // a notice, and the run passes.
    let mut fake = Fake::new(author, &format!("{fixable}\n{warned}"), &warned, 2, &["crates/a/src/lib.rs"]);
    let done = run(&mut fake, &args("pull_request", false));
    expect(!done.failed && fake.committed(), &format!("fixed, committed, passing: {done:?} {:?}", fake.log));
    let commit = fake.log.iter().find(|it| it.contains(" commit ")).cloned().unwrap_or_default();
    expect(
        commit.contains(&format!("user.email={BOT_EMAIL}")) && commit.contains("-m blaze check --fix: 2 fixes of blaze::std_use"),
        &format!("the commit is the bot's and names the fixes: {commit}"),
    );
    expect(fake.left.get(COMMIT).map(String::as_str) == Some("f1x"), &format!("the hash is left for the push: {:?}", fake.left));
    expect(done.said.iter().any(|it| it.starts_with("::notice::blaze check --fix: 2 fixes")), &format!("a notice: {:?}", done.said));
    expect(!fake.log.iter().any(|it| it.starts_with("git push")), "the script never pushes");

    // Nothing fixable, a deny left: no commit, and the run fails listing it grouped with its help.
    let mut fake = Fake::new(author, &format!("{unfixable}\n{warned}"), &format!("{unfixable}\n{warned}"), 0, &[]);
    let done = run(&mut fake, &args("pull_request", false));
    expect(done.failed && !fake.committed(), &format!("a deny left fails, nothing committed: {done:?}"));
    expect(
        done.said.iter().any(|it| it == "blaze::env_read: 1 finding(s)")
            && done.said.iter().any(|it| it == "crates/a/src/main.rs:7:5: blaze::env_read: a finding of blaze::env_read")
            && done.said.iter().any(|it| it == "  help: the help of blaze::env_read")
            && !done.said.iter().any(|it| it.contains("needless_return")),
        &format!("the deny, grouped, with its help, and no warning: {:?}", done.said),
    );

    // Fixed some, a deny left: committed and still failing.
    let mut fake = Fake::new(author, &format!("{fixable}\n{unfixable}"), &unfixable, 2, &["crates/a/src/lib.rs"]);
    let done = run(&mut fake, &args("pull_request", false));
    expect(done.failed && fake.committed(), &format!("fixes committed, the deny left fails: {done:?}"));

    // A fork: no commit and no push; the diff is left and the command to apply it said.
    let mut fake = Fake::new(author, &fixable, "", 2, &["crates/a/src/lib.rs"]);
    let done = run(&mut fake, &args("pull_request", true));
    expect(done.failed && !fake.committed() && !fake.left.contains_key(COMMIT), &format!("a fork commits nothing: {done:?}"));
    expect(fake.left.contains_key(DIFF), &format!("a fork's diff is left: {:?}", fake.left));
    expect(done.said.iter().any(|it| it.contains("`blaze check --workspace --fix`")), &format!("the command: {:?}", done.said));

    // The loop: the head is the bot's fix and --fix changes files again: no commit, the lint named.
    let mut fake = Fake::new(&bot, &fixable, "", 2, &["crates/a/src/lib.rs"]);
    let done = run(&mut fake, &args("pull_request", false));
    expect(done.failed && !fake.committed(), &format!("a fix that does not converge commits nothing: {done:?}"));
    expect(done.said.iter().any(|it| it.contains("do not converge") && it.contains("blaze::std_use")), &format!("the lint named: {:?}", done.said));
    // The bot's head with nothing more to fix passes.
    let mut fake = Fake::new(&bot, &warned, &warned, 0, &[]);
    let done = run(&mut fake, &args("pull_request", false));
    expect(!done.failed && !fake.committed(), &format!("a converged fix passes: {done:?}"));

    // main: no --fix, only the check.
    let mut fake = Fake::new(author, &unfixable, &unfixable, 0, &[]);
    let done = run(&mut fake, &args("push", false));
    expect(done.failed && !fake.log.iter().any(|it| it.contains("--fix")) && !fake.committed(), &format!("a push checks only: {:?}", fake.log));
    let mut on_main = args("pull_request", false);
    on_main.head_ref = Some("main".to_owned());
    expect(decide(&mut Fake::new(author, "", "", 0, &[]), &on_main).is_err(), "a head_ref of main is refused");

    // A --fix that stopped (no `Fixed` line), a check that did not run, a line that is not JSON.
    let mut stopped = Fake::new(author, &fixable, "", 0, &[]);
    stopped.fixed = "error: the suggestions made would give 1 error(s)\n".to_owned();
    expect(decide(&mut stopped, &args("pull_request", false)).is_err(), "a --fix that stopped fails the run");
    expect(findings("not json\n").is_err(), "a line that is not JSON is refused");
    expect(suggestions_made("       Fixed 12 suggestion(s) in 3 file(s): a") == Some(12), "the count of suggestions made");
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn selftest() {
        super::selftest();
    }
}
