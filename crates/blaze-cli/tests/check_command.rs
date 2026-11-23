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
const NO_COMMAND: &str = "the command is check, run, test, peer, clean, gc, warm-toolchain or explain";

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

/// `blaze <args>` in `ws` with the home `home`; its streams kept as `<ws>.<what>.out|err`.
fn blaze(ws: &Path, home: &Path, args: &[&str], what: &str) -> Said {
    let out_path = ws.with_extension(format!("{what}.out"));
    let err_path = ws.with_extension(format!("{what}.err"));
    let out = File::create(&out_path).unwrap_or_else(|error| panic!("making {}: {error}", out_path.display()));
    let err = File::create(&err_path).unwrap_or_else(|error| panic!("making {}: {error}", err_path.display()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .args(args)
        .current_dir(ws)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("PWD", ws)
        .env_remove("SDKROOT")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("blaze starts");
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
    let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
    Said { status: status.code(), out: read(&out_path), err: read(&err_path) }
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

    let _ = std::fs::remove_dir_all(&base);
    println!(
        "CHECK COMMAND: clean {:?}, type error {:?} at {finding}, borrows-only {:?}, check-old refused {first:?}, --stats {:?}",
        clean.status, typed.status, counted.status, stats.status
    );
}
