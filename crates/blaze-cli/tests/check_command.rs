//! `blaze check` IS the engine (check_command.rs): one checker (rule 30). Before the switch,
//! `check` ran the old checker (main.rs `request`, body_types.rs) and the engine was a second
//! command, `engine-check`; now `check` runs the engine, `engine-check` is its alias, and the old
//! checker is gone (2026-11-19: `check-old`, its word, with it).
//!
//! The fixture: a workspace of two `#![no_std]` packages, `app` calling `dep` (the shape the
//! engine type-checks whole, tests/engine_block_extern_crate.rs). Five runs, each in its own
//! checkout under one fresh home:
//!
//! 1. CLEAN: the closing line is `0 errors: blaze check`; the status is 3 while borrows are
//!    notes (`BORROWS_ARE_ERRORS` false: the borrows stage is "NOT checked", named by its
//!    reason) and 0 once they are errors (the switch's value is read from body_types.rs, as
//!    blaze-ladder's flow census reads it). No "NOT checked" line names types.
//! 2. A TYPE ERROR (`let _x: u32 = T;`, `T` a unit struct: the Reference, "Let statements",
//!    the initializer's type must be the pattern's; E0308): status 1, the finding at
//!    `app/src/lib.rs:4:<col>`, the closing line `1 error: blaze check`.
//! 3. ONE BODY WHOSE BORROWS ARE NOT CHECKED (`later`, holding an `async` closure, which the
//!    borrow lowering refuses by name, `AsyncClosure`; its types are checked): the
//!    closing lines say "NOT checked: borrows" and never "types" (the owner's rule: never a
//!    pass for a stage that did not run, and never a fail for one that did); status 3, not 4
//!    (no item was left unchecked: its types were checked). Under `--each` the body's own note
//!    says "types checked; borrows not checked"; without it, one line counts the body.
//!    CONTROL: the clean run (1) prints neither the note nor the count.
//! 4. `check-old` on the clean checkout is no command: refused (a nonzero status) with the
//!    usage line naming the commands there are, nothing on standard output. CONTROL: `check`
//!    on the same checkout answers (1).
//! 5. `check --stats` on the clean checkout, checked before (its certificate stands): the
//!    same status and closing line as a plain check, and on stderr one line "blaze: 0 of N
//!    task(s) entered, .., certificate stood; .. ms in main; .blaze/v2/profile.txt"
//!    (engine_check.rs `stats_line`). CONTROL: the plain check prints no such line.
//!
//! Expected line: `CHECK COMMAND: clean <s>, type error <s> at <line>, borrows-only <s>,
//! check-old refused <s>, --stats <s>` (`--stats Some(3)` while borrows are notes).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (linked into a fresh home).

mod support;
#[path = "support/switch.rs"]
mod switch;

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// How long one check of the two small packages may take; a hang is a bug (rule 18).
const DEADLINE: Duration = Duration::from_secs(200);
const REAP: Duration = Duration::from_secs(10);

const DEP: &str = "#![no_std]\npub fn one() -> u32 {\n    1\n}\n";

/// `app`'s lib.rs; `{BODY}` is `two`'s first statement, at line 4.
const APP: &str = "#![no_std]\npub struct T;\npub fn two() -> u32 {\n    {BODY}\n    dep::one() + 1\n}\n";

/// The line of `{BODY}` in [`APP`].
const BODY_LINE: u32 = 4;

/// A body whose types check and whose move and borrow check the engine refuses today: an
/// `async` closure (the Reference, "Closure expressions", "Async closures", stable since 1.85),
/// which the borrow lowering refuses by name, `Form::AsyncClosure` (blaze-flow-lower lib.rs, the
/// body walk; blaze-ladder tests/front2_borrow_async_closure.rs CONTROL 3: "still refused
/// `AsyncClosure` by name, never checked clean"). Until 2026-11-21 it was `folder_of` (the
/// census's NestedOutlives shape, flow-census-1118-0049.txt: a closure returning a reference
/// into the fn's argument); its closure's output is now an owner position whose requirement the
/// owner proves (blaze-flow-lower closure.rs `External::SigOwner`, `Need::Sig`), so its borrows
/// are checked whole, as rustc accepts it, and it plants no refusal.
const BORROWS_ONLY: &str = "pub fn later() -> u8 {\n    let c = async || 1u8;\n    let _ = c;\n    1\n}\n";

/// The body's note under `--each` (engine_bodies.rs `Said::BorrowsUnchecked`).
const BORROWS_NOTE: &str = "types checked; borrows not checked";
/// The count line without `--each` (print.rs `stage_notes`).
const BORROWS_COUNT: &str = "type-checked whole had the move and borrow check refused";
/// The first line the old checker printed (main.rs `CHECK_OLD`, gone 2026-11-19): no command
/// prints it now.
const OLD_FIRST: &str = "note: check-old: the OLD checker";
/// The usage line of a word that is no command (main.rs `command`'s last arm).
const NO_COMMAND: &str = "the command is check, ci, inspect, run, test, export, peer, clean, gc, warm-toolchain or explain";

fn write(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap_or_else(|error| panic!("making {}: {error}", dir.display()));
    }
    std::fs::write(path, text).unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
}

/// The workspace at `ws`, `body` in `two` and `extra` after it in app/src/lib.rs.
fn workspace(ws: &Path, body: &str, extra: &str) {
    write(&ws.join("Cargo.toml"), "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"2\"\n");
    write(
        &ws.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"dep\"]\n\n[[package]]\nname = \"dep\"\nversion = \"0.1.0\"\n",
    );
    write(&ws.join("app/Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n");
    write(&ws.join("app/src/lib.rs"), &format!("{}{extra}", APP.replace("{BODY}", body)));
    write(&ws.join("dep/Cargo.toml"), "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(&ws.join("dep/src/lib.rs"), DEP);
}

/// What one run said: its status, its standard output and its standard error.
struct Said {
    status: Option<i32>,
    out: String,
    err: String,
}

impl Said {
    /// Both streams, for a message.
    fn all(&self) -> String {
        format!("status {:?}\n--- stdout\n{}--- stderr\n{}", self.status, self.out, self.err)
    }

    /// The closing line's counts (`0 errors`, `1 error`), without the command after them
    /// (`: blaze check`, and ` --fix <id>` when a fix is offered: blaze-report plain.rs
    /// `agent_closing`).
    fn closing(&self) -> String {
        let line = self.out.lines().rev().find(|line| line.contains(": blaze check")).unwrap_or_else(|| panic!("no closing line:\n{}", self.all()));
        format!("{}: blaze check", line.split(": blaze check").next().unwrap_or(""))
    }

    /// Every line that says a stage was not checked.
    fn not_checked(&self) -> Vec<&str> {
        self.out.lines().filter(|line| line.contains("NOT checked")).collect()
    }
}

fn read_text(path: &Path) -> impl core::ops::Deref<Target = str> + core::fmt::Display + use<> {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

/// `blaze <args>` in `ws` with the home `home`; its streams kept as `<ws>.<what>.out|err`.
fn blaze(ws: &Path, home: &Path, args: &[&str], what: &str) -> Said {
    blaze_env(ws, home, args, &[], what)
}

/// [`blaze`] with additional environment variables (clearing `GITHUB_ACTIONS` and
/// `GITHUB_STEP_SUMMARY` unless supplied in `extra_env`).
fn blaze_env(ws: &Path, home: &Path, args: &[&str], extra_env: &[(&str, &str)], what: &str) -> Said {
    let out_path = ws.with_extension(format!("{what}.out"));
    let err_path = ws.with_extension(format!("{what}.err"));
    let out = File::create(&out_path).unwrap_or_else(|error| panic!("making {}: {error}", out_path.display()));
    let err = File::create(&err_path).unwrap_or_else(|error| panic!("making {}: {error}", err_path.display()));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_blaze"));
    cmd.args(args)
        .current_dir(ws)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("PWD", ws)
        .env_remove("SDKROOT")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_STEP_SUMMARY")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    for &(k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("blaze starts");
    let started = Instant::now();
    // Waiting for: `blaze <args>` of two small packages, at most DEADLINE.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > DEADLINE => {
                let _ = child.kill();
                let reaped = Instant::now();
                // Waiting for: the killed child's exit, at most REAP.
                while child.try_wait().ok().flatten().is_none() && reaped.elapsed() < REAP {
                    std::thread::sleep(Duration::from_millis(5));
                }
                panic!("`blaze {}` in {} did not end within {DEADLINE:?} (rule 18)", args.join(" "), ws.display());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("waiting for `blaze {}` in {}: {error}", args.join(" "), ws.display());
            }
        }
    };
    Said { status: status.code(), out: read_text(&out_path).to_owned(), err: read_text(&err_path).to_owned() }
}

#[test]
fn check_is_the_engine_and_names_exactly_the_stage_not_checked() {
    let base = fresh_home("check_command", true);
    let home = base.join("home");
    let errors = switch::borrows_are_errors();
    let clean_status = if errors { 0 } else { 3 };

    // 1. Clean.
    let clean_ws = base.join("clean");
    workspace(&clean_ws, "let _x: u32 = 1;", "");
    let clean = blaze(&clean_ws, &home, &["check"], "check");
    assert_eq!(clean.status, Some(clean_status), "clean: the status is {clean_status} with BORROWS_ARE_ERRORS {errors}:\n{}", clean.all());
    assert_eq!(clean.closing(), "0 errors: blaze check", "clean: the closing line:\n{}", clean.all());
    assert!(!clean.out.lines().next().is_some_and(|line| line.starts_with(OLD_FIRST)), "clean: `check` printed the old checker's first line:\n{}", clean.all());
    assert!(clean.not_checked().iter().all(|line| !line.contains("types")), "clean: a NOT checked line names types:\n{}", clean.all());
    let borrows_named = clean.not_checked().iter().any(|line| line.contains("borrows"));
    assert_eq!(borrows_named, !errors, "clean: \"NOT checked: borrows\" is printed exactly while borrows are notes:\n{}", clean.all());
    assert!(!clean.out.contains(BORROWS_NOTE) && !clean.out.contains(BORROWS_COUNT), "clean (the control): no body's borrows were refused:\n{}", clean.all());

    // 2. A type error, at its file, line and column.
    let typed_ws = base.join("typed");
    workspace(&typed_ws, "let _x: u32 = T;", "");
    let typed = blaze(&typed_ws, &home, &["check"], "check");
    assert_eq!(typed.status, Some(1), "type error: status 1:\n{}", typed.all());
    assert_eq!(typed.closing(), "1 error: blaze check", "type error: the closing line:\n{}", typed.all());
    let at = format!("app/src/lib.rs:{BODY_LINE}:");
    let finding = typed.out.lines().find(|line| line.contains("error[") && line.contains(&at)).unwrap_or_else(|| panic!("type error: no `error[` line at {at}<col>:\n{}", typed.all()));
    let col = finding.split(&at).nth(1).map(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>()).unwrap_or_default();
    assert!(col.parse::<u32>().is_ok_and(|col| col > 0), "type error: the finding's column after {at}: {finding}");

    // 3. One body whose borrows are not checked: borrows named, types not; with and without --each.
    let borrows_ws = base.join("borrows");
    workspace(&borrows_ws, "let _x: u32 = 1;", BORROWS_ONLY);
    let each = blaze(&borrows_ws, &home, &["check", "--each"], "each");
    let counted = blaze(&borrows_ws, &home, &["check"], "check");
    for (what, said) in [("--each", &each), ("counted", &counted)] {
        assert_eq!(said.status, Some(3), "borrows-only ({what}): status 3 (incomplete), never 4 (no item was left unchecked) nor 1:\n{}", said.all());
        assert_eq!(said.closing(), "0 errors: blaze check", "borrows-only ({what}): the closing line:\n{}", said.all());
        assert!(said.not_checked().iter().any(|line| line.contains("NOT checked: borrows")), "borrows-only ({what}): \"NOT checked: borrows\":\n{}", said.all());
        assert!(said.not_checked().iter().all(|line| !line.contains("types")), "borrows-only ({what}): the types WERE checked, and a NOT checked line names them:\n{}", said.all());
        assert!(!said.out.contains("the check is incomplete: blaze could not check"), "borrows-only ({what}): counted as an item not checked:\n{}", said.all());
    }
    assert!(each.out.lines().any(|line| line.contains(BORROWS_NOTE) && line.contains("later")), "borrows-only (--each): the body's own note naming `later`:\n{}", each.all());
    if !errors {
        assert!(counted.out.contains(&format!("1 body {BORROWS_COUNT}")), "borrows-only: the one line counting the body:\n{}", counted.all());
    }

    // 4. The old checker's word is no command now: refused by the usage line, nothing checked.
    let old = blaze(&clean_ws, &home, &["check-old"], "old");
    assert!(old.status.is_some_and(|status| status != 0), "check-old: refused with a nonzero status:\n{}", old.all());
    assert!(old.out.is_empty() && !old.out.contains(OLD_FIRST), "check-old: nothing on standard output:\n{}", old.all());
    assert!(old.err.contains(NO_COMMAND), "check-old: the usage line names the commands:\n{}", old.all());
    let first = old.status;

    // 5. `--stats`: the same answer, and one stats line on stderr (engine_check.rs
    // `stats_line`). The clean checkout was checked above, so this one stands on its
    // certificate: nothing entered. CONTROL: the plain run of the same checkout before it
    // prints no stats line.
    let plain = blaze(&clean_ws, &home, &["check"], "plain");
    let stats = blaze(&clean_ws, &home, &["check", "--stats"], "stats");
    assert_eq!(stats.status, plain.status, "--stats: the status is the plain check's:\n{}", stats.all());
    assert_eq!(stats.closing(), plain.closing(), "--stats: the closing line is the plain check's:\n{}", stats.all());
    let line = stats.err.lines().find(|line| line.starts_with("blaze: ")).unwrap_or_else(|| panic!("--stats: no stats line on stderr:\n{}", stats.all()));
    assert!(line.contains("task(s) entered") && line.contains("certificate stood") && line.ends_with(".blaze/v2/profile.txt"), "--stats: the line counts the check, whose certificate stood: {line}");
    assert!(line.starts_with("blaze: 0 of "), "--stats: nothing was entered over a standing certificate: {line}");
    assert!(!plain.err.lines().any(|line| line.starts_with("blaze: ")), "--stats CONTROL: the plain check printed a stats line:\n{}", plain.all());

    cleanup(&base);
    println!(
        "CHECK COMMAND: clean {:?}, type error {:?} at {finding}, borrows-only {:?}, check-old refused {first:?}, --stats {:?}",
        clean.status, typed.status, counted.status, stats.status
    );
}

const INSPECT_SAMPLE: &str = "#![blaze::records]
#![blaze::error_unions]
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u32 {}
impl Copy for i32 {}
impl Copy for bool {}

enum Infallible {}

enum ControlFlow<B, C> {
    #[lang = \"Continue\"]
    Continue(C),
    #[lang = \"Break\"]
    Break(B),
}

#[lang = \"from\"]
trait From<T> {
    fn from(value: T) -> Self;
}

impl<T> From<T> for T {
    fn from(value: T) -> Self {
        value
    }
}

trait FromResidual<R = <Self as Try>::Residual> {
    #[lang = \"from_residual\"]
    fn from_residual(residual: R) -> Self;
}

trait Try: FromResidual {
    type Output;
    type Residual;
    #[lang = \"branch\"]
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output>;
}

enum Result<T, E> {
    Ok(T),
    Err(E),
}

impl<T, E> Try for Result<T, E> {
    type Output = T;
    type Residual = Result<Infallible, E>;
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output> {
        match self {
            Result::Ok(v) => ControlFlow::Continue(v),
            Result::Err(e) => ControlFlow::Break(Result::Err(e)),
        }
    }
}

impl<T, E, F: From<E>> FromResidual<Result<Infallible, E>> for Result<T, F> {
    fn from_residual(residual: Result<Infallible, E>) -> Self {
        match residual {
            Result::Err(e) => Result::Err(From::from(e)),
            Result::Ok(x) => match x {},
        }
    }
}

struct ErrA(u32);
struct ErrB(u32);
struct ErrC(u32);

fn read(mode: u32) -> Result<u32, _> {
    if mode == 1 {
        Result::Err(ErrA(10))
    } else {
        Result::Ok(mode)
    }
}

fn parse(x: u32) -> Result<u32, _> {
    if x == 2 {
        Result::Err(ErrB(20))
    } else {
        Result::Ok(x + 1)
    }
}

pub fn load(mode: u32) -> Result<u32, _> {
    if mode == 3 {
        return Result::Err(ErrC(30));
    }
    let a = read(mode)?;
    let b = parse(a)?;
    Result::Ok(b)
}

fn sum_def(r: { a: u32, b: u32 = 32 }) -> u32 {
    r.a + r.b
}

fn read_host(r: { host: u32 }) -> u32 {
    r.host
}

pub fn demo(mode: u32) -> u32 {
    let x = sum_def({ a: 10u32 });
    let wide = { host: 20u32, port: 8080u32, tls: true };
    let y = read_host(wide);
    let z = match load(mode) {
        Result::Ok(v) => v,
        Result::Err(e) => match e {
            a: ErrA => a.0 + 1,
            rest => match rest {
                b: ErrB => b.0 + 2,
                c => c.0 + 3,
            },
        },
    };
    x + y + z
}
";

#[test]
fn inspect_command_emits_annotated_hints_and_json() {
    let base = fresh_home("inspect_command", false);
    let home = base.join("home");
    let ws = base.join("ws");
    write(&ws.join("sample.rs"), INSPECT_SAMPLE);

    // 1. Default annotated output on the full file.
    let ann = blaze(&ws, &home, &["inspect", "sample.rs"], "inspect_ann");
    assert_eq!(ann.status, Some(0), "inspect annotated failed:\n{}", ann.all());
    assert!(ann.out.contains("// pub fn load(mode: u32) -> Result<u32, "), "missing load sig:\n{}", ann.out);
    assert!(ann.out.contains("/* = "), "missing inferred return hint:\n{}", ann.out);
    assert!(ann.out.contains("/* ? ErrA */"), "missing ? ErrA hint:\n{}", ann.out);
    assert!(ann.out.contains("/*: u32*/"), "missing binding hint:\n{}", ann.out);
    assert!(ann.out.contains("/* ..defaults: b: u32 */"), "missing record default hint:\n{}", ann.out);
    assert!(ann.out.contains("/* project -> { host: u32 } */"), "missing record projection coercion hint:\n{}", ann.out);
    assert!(ann.out.contains("rest/*: ") && ann.out.contains("c/*: ErrC*/"), "missing union residual narrowing hints:\n{}", ann.out);

    // 2. Filtered JSON output (`sample.rs::load --json`).
    let json = blaze(&ws, &home, &["inspect", "sample.rs::load", "--json"], "inspect_json");
    assert_eq!(json.status, Some(0), "inspect --json failed:\n{}", json.all());
    assert!(json.out.contains("\"items\": ["), "missing items array:\n{}", json.out);
    assert!(json.out.contains("\"name\": \"load\""), "missing load item:\n{}", json.out);
    assert!(!json.out.contains("\"name\": \"demo\""), "filter should exclude demo:\n{}", json.out);
    assert!(json.out.contains("\"inferred_output\": \"Result<u32, "), "missing inferred_output:\n{}", json.out);
    assert!(json.out.contains("\"inferred_error_set\": \""), "missing inferred_error_set:\n{}", json.out);
    assert!(json.out.contains("\"kind\": \"inferred_return\""), "missing inferred_return hint:\n{}", json.out);
    assert!(json.out.contains("\"kind\": \"try_propagate\""), "missing try_propagate hint:\n{}", json.out);
    assert!(json.out.contains("\"kind\": \"binding\""), "missing binding hint:\n{}", json.out);

    // 3. Opt-out flags (`--no-coercions --no-defaults`).
    let stripped = blaze(&ws, &home, &["inspect", "sample.rs::demo", "--no-coercions", "--no-defaults"], "inspect_stripped");
    assert_eq!(stripped.status, Some(0), "inspect stripped failed:\n{}", stripped.all());
    assert!(!stripped.out.contains("project ->"), "--no-coercions must omit coercions:\n{}", stripped.out);
    assert!(!stripped.out.contains("..defaults:"), "--no-defaults must omit defaults:\n{}", stripped.out);
    assert!(stripped.out.contains("/*: u32*/"), "bindings should remain enabled:\n{}", stripped.out);

    println!("=== ANNOTATED SAMPLE (demo + load) ===\n{}", ann.out);
    println!("=== JSON SAMPLE (load) ===\n{}", json.out);
}

/// Multi-crate fixture exercising unused-dependency soundness rules:
/// `used_use` (`use`), `used_path` (qualified path), `used_mac` (macro call), `used_ext`
/// (`extern crate ... as _`), `opt_dep` (optional/feature), `native-sys` (`links`), and
/// optionally `ghost` (genuinely unused).
fn build_udeps_workspace(ws: &Path, include_ghost: bool) {
    let ghost_member = if include_ghost { ", \"ghost\"" } else { "" };
    write(
        &ws.join("Cargo.toml"),
        &format!(
            "[workspace]\nmembers = [\"app\", \"used_use\", \"used_path\", \"used_mac\", \"used_ext\", \"opt_dep\", \"native-sys\"{ghost_member}]\nresolver = \"2\"\n"
        ),
    );
    let ghost_line = if include_ghost { "ghost = { path = \"../ghost\" }\n" } else { "" };
    write(
        &ws.join("app/Cargo.toml"),
        &format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nwith_opt = [\"dep:opt_dep\"]\n\n[dependencies]\nused_use = {{ path = \"../used_use\" }}\nused_path = {{ path = \"../used_path\" }}\nused_mac = {{ path = \"../used_mac\" }}\nused_ext = {{ path = \"../used_ext\" }}\nopt_dep = {{ path = \"../opt_dep\", optional = true }}\nnative-sys = {{ path = \"../native-sys\" }}\n{ghost_line}"
        ),
    );
    write(
        &ws.join("app/src/lib.rs"),
        "#![no_std]\nuse used_use::val;\nextern crate used_ext as _;\n\n// ghost::comment_mention_must_not_count();\npub const S: &str = \"ghost::string_must_not_count\";\n\npub fn compute() -> u32 {\n    val() + used_path::one() + used_mac::emit!()\n}\n",
    );
    write(&ws.join("used_use/Cargo.toml"), "[package]\nname = \"used_use\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(&ws.join("used_use/src/lib.rs"), "#![no_std]\npub fn val() -> u32 { 1 }\n");
    write(&ws.join("used_path/Cargo.toml"), "[package]\nname = \"used_path\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(&ws.join("used_path/src/lib.rs"), "#![no_std]\npub fn one() -> u32 { 2 }\n");
    write(&ws.join("used_mac/Cargo.toml"), "[package]\nname = \"used_mac\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(&ws.join("used_mac/src/lib.rs"), "#![no_std]\n#[macro_export]\nmacro_rules! emit { () => { 3u32 }; }\n");
    write(&ws.join("used_ext/Cargo.toml"), "[package]\nname = \"used_ext\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(&ws.join("used_ext/src/lib.rs"), "#![no_std]\npub fn init() {}\n");
    write(&ws.join("opt_dep/Cargo.toml"), "[package]\nname = \"opt_dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(&ws.join("opt_dep/src/lib.rs"), "#![no_std]\npub fn opt() -> u32 { 4 }\n");
    write(
        &ws.join("native-sys/Cargo.toml"),
        "[package]\nname = \"native-sys\"\nversion = \"0.1.0\"\nedition = \"2021\"\nlinks = \"native\"\n",
    );
    write(&ws.join("native-sys/src/lib.rs"), "#![no_std]\npub fn link_only() {}\n");
    if include_ghost {
        write(&ws.join("ghost/Cargo.toml"), "[package]\nname = \"ghost\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
        write(&ws.join("ghost/src/lib.rs"), "#![no_std]\npub fn unused() -> u32 { 99 }\n");
    }
}

#[test]
fn check_detects_unused_dependency_with_zero_false_positives_and_fixes_it() {
    let base = fresh_home("unused_deps_e2e", true);
    let home = base.join("home");
    let ws = base.join("ws");
    build_udeps_workspace(&ws, true);

    let check = blaze_env(&ws, &home, &["check"], &[("BLAZE_NO_DAEMON", "1")], "check");
    assert!(
        check.out.contains("warning[unused-dependency]") && check.out.contains("app/Cargo.toml:") && check.out.contains("`ghost`"),
        "expected unused-dependency warning for `ghost` at app/Cargo.toml:\n{}",
        check.all()
    );
    for allowed in ["used_use", "used_path", "used_mac", "used_ext", "opt_dep", "native-sys"] {
        assert!(
            !check.out.lines().any(|l| l.contains("unused-dependency") && l.contains(allowed)),
            "false positive unused-dependency for `{allowed}`:\n{}",
            check.all()
        );
    }

    let fixed = blaze_env(&ws, &home, &["check", "--fix", "all"], &[("BLAZE_NO_DAEMON", "1")], "fix");
    assert!(
        !fixed.out.contains("unused-dependency"),
        "after `--fix all`, no unused-dependency warning should remain:\n{}",
        fixed.all()
    );
    let manifest_after = read_text(&ws.join("app/Cargo.toml"));
    assert!(!manifest_after.contains("ghost ="), "ghost line should be deleted:\n{manifest_after}");

    cleanup(&base);
}

#[test]
fn blaze_ci_passes_on_clean_fails_on_errors_and_unused_deps_and_emits_github_and_btab() {
    let base = fresh_home("ci_command_e2e", true);
    let home = base.join("home");
    let alloc = blaze_own::testing::heap().alloc();

    // 1. Clean workspace -> exit 0, btab todo with `status: passed` and `todo: []`.
    let clean_ws = base.join("clean");
    build_udeps_workspace(&clean_ws, false);
    let clean = blaze_env(&clean_ws, &home, &["ci", "--no-test"], &[("BLAZE_NO_DAEMON", "1")], "ci_clean");
    assert_eq!(clean.status, Some(0), "clean ci should exit 0:\n{}", clean.all());
    assert!(clean.out.contains("blaze ci: PASSED"), "expected PASSED verdict:\n{}", clean.all());
    let todo_btab = read_text(&clean_ws.join(".blaze/ci/todo.btab"));
    assert!(
        todo_btab.contains("schema: 1") && todo_btab.contains("status: passed") && todo_btab.contains("todo: []"),
        "unexpected todo.btab:\n{todo_btab}"
    );
    let parsed_clean = bstd::tab::read(todo_btab.as_bytes(), bstd::tab::Limits::SHIPPED, alloc)
        .expect("clean todo.btab must parse via bstd::tab::read");
    assert_eq!(parsed_clean.as_map().and_then(|m| m.get("status")).and_then(|v| v.as_str()), Some("passed"));

    // 2. Unused dependency -> exit 1, btab + JSON todo with suggestion, `--fix` repairs and passes.
    let udep_ws = base.join("udep");
    build_udeps_workspace(&udep_ws, true);
    let udep = blaze_env(&udep_ws, &home, &["ci", "--no-test"], &[("BLAZE_NO_DAEMON", "1")], "ci_udep");
    assert_eq!(udep.status, Some(1), "ci with unused dep must fail (exit 1):\n{}", udep.all());
    assert!(udep.out.contains("blaze ci: FAILED") && udep.out.contains("1 unused dep(s)"), "expected FAILED naming unused dep:\n{}", udep.all());
    let udep_btab = read_text(&udep_ws.join(".blaze/ci/todo.btab"));
    assert!(
        udep_btab.contains("status: failed")
            && udep_btab.contains("unused_deps: 1")
            && udep_btab.contains("todo: [[")
            && udep_btab.contains("];")
            && udep_btab.contains("app/Cargo.toml")
            && !udep_btab.contains("\"app/Cargo.toml\"")
            && udep_btab.contains("unused_dependency")
            && udep_btab.contains("replacement: \"\""),
        "unexpected udep todo.btab:\n{udep_btab}"
    );
    let parsed_udep = bstd::tab::read(udep_btab.as_bytes(), bstd::tab::Limits::SHIPPED, alloc)
        .expect("udep todo.btab must parse via bstd::tab::read");
    assert_eq!(parsed_udep.as_map().and_then(|m| m.get("status")).and_then(|v| v.as_str()), Some("failed"));
    assert_eq!(
        parsed_udep.as_map().and_then(|m| m.get("todo")).and_then(|v| v.as_list()).map(|l| l.len()),
        Some(1)
    );

    let udep_json_run = blaze_env(&udep_ws, &home, &["ci", "--no-test", "--todo-format=json"], &[("BLAZE_NO_DAEMON", "1")], "ci_udep_json");
    assert_eq!(udep_json_run.status, Some(1), "ci json run fails:\n{}", udep_json_run.all());
    let udep_json = read_text(&udep_ws.join(".blaze/ci/todo.json"));
    assert!(
        udep_json.contains("\"schema\": 1")
            && udep_json.contains("\"status\": \"failed\"")
            && udep_json.contains("\"unused_deps\": 1")
            && udep_json.contains("\"kind\":\"unused_dependency\""),
        "unexpected udep todo.json:\n{udep_json}"
    );

    let udep_fixed = blaze_env(&udep_ws, &home, &["ci", "--no-test", "--fix"], &[("BLAZE_NO_DAEMON", "1")], "ci_udep_fix");
    assert_eq!(udep_fixed.status, Some(0), "ci --fix should fix unused dep and pass:\n{}", udep_fixed.all());
    assert!(udep_fixed.out.contains("blaze ci: PASSED"), "expected PASSED after --fix:\n{}", udep_fixed.all());

    // 3. GitHub Actions mode (`GITHUB_ACTIONS=true`, `GITHUB_STEP_SUMMARY`) with type error + unused dep.
    let gh_ws = base.join("gh");
    build_udeps_workspace(&gh_ws, true);
    write(
        &gh_ws.join("app/src/lib.rs"),
        "#![no_std]\nuse used_use::val;\nextern crate used_ext as _;\npub struct T;\npub fn compute() -> u32 {\n    let _bad: u32 = T;\n    val() + used_path::one() + used_mac::emit!()\n}\n",
    );
    let summary_file = base.join("step_summary.md");
    let summary_str = summary_file.to_string_lossy().into_owned();
    let gh = blaze_env(
        &gh_ws,
        &home,
        &["ci", "--no-test"],
        &[("BLAZE_NO_DAEMON", "1"), ("GITHUB_ACTIONS", "true"), ("GITHUB_STEP_SUMMARY", &summary_str)],
        "ci_gh",
    );
    assert_eq!(gh.status, Some(1), "github ci with type error + unused dep must fail:\n{}", gh.all());
    assert!(gh.out.contains("::group::blaze check"), "missing ::group::blaze check:\n{}", gh.all());
    assert!(gh.out.contains("::endgroup::"), "missing ::endgroup:::\n{}", gh.all());
    assert!(gh.out.contains("::error file=app/src/lib.rs,line=6,col="), "missing ::error annotation:\n{}", gh.all());
    assert!(
        gh.out.contains("::warning file=app/Cargo.toml,line=") && gh.out.contains("title=unused-dependency::"),
        "missing ::warning annotation for unused-dependency:\n{}",
        gh.all()
    );
    let gh_btab = read_text(&gh_ws.join(".blaze/ci/todo.btab"));
    assert!(
        gh_btab.contains("todo: [[")
            && gh_btab.contains("app/src/lib.rs")
            && gh_btab.contains("app/Cargo.toml")
            && !gh_btab.contains("\"app/src/lib.rs\"")
            && gh_btab.contains(" _ "),
        "expected sparse btab table with bare paths and `_` sentinel:\n{gh_btab}"
    );
    let parsed_gh = bstd::tab::read(gh_btab.as_bytes(), bstd::tab::Limits::SHIPPED, alloc)
        .expect("gh todo.btab must parse via bstd::tab::read");
    assert_eq!(
        parsed_gh.as_map().and_then(|m| m.get("todo")).and_then(|v| v.as_list()).map(|l| l.len()),
        Some(2)
    );
    let review_json = read_text(&gh_ws.join(".blaze/ci/github-review.json"));
    assert!(
        review_json.contains("\"path\":\"app/Cargo.toml\"") && review_json.contains("```suggestion"),
        "expected PR suggestion comment in github-review.json:\n{review_json}"
    );
    let step_md = read_text(&summary_file);
    assert!(
        step_md.contains("## blaze ci: FAILED")
            && step_md.contains("| `check` | fail |")
            && step_md.contains("| `unused-deps` | fail | 1 unused dependency(ies) |"),
        "unexpected GITHUB_STEP_SUMMARY:\n{step_md}"
    );

    // 4. Full `blaze ci` with passing `#[test]` when `blaze-launcher` is present beside `blaze`.
    let launcher_beside = Path::new(env!("CARGO_BIN_EXE_blaze")).with_file_name("blaze-launcher");
    if cfg!(target_os = "macos") && launcher_beside.exists() {
        let test_ws = base.join("with_test");
        write(&test_ws.join("Cargo.toml"), "[package]\nname = \"clean_test\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
        write(&test_ws.join("src/lib.rs"), "pub fn add(a: u32, b: u32) -> u32 { a + b }\n#[test]\nfn adds() { assert_eq!(add(2, 3), 5); }\n");
        let ran = blaze_env(&test_ws, &home, &["ci"], &[("BLAZE_NO_DAEMON", "1")], "ci_with_test");
        assert_eq!(ran.status, Some(0), "full blaze ci with passing test must exit 0:\n{}", ran.all());
        assert!(
            ran.out.contains("blaze ci: PASSED") && ran.out.contains("1 test(s) passed"),
            "expected PASSED with 1 test passed:\n{}",
            ran.all()
        );
    }

    cleanup(&base);
}
