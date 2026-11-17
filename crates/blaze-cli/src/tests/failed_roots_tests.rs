//! What `blaze check` prints when tasks fail (not_checked/waited.rs): each task that failed on
//! its own is printed ONCE, last, with a line counting what hangs from it; a task that failed
//! only because one it needs failed (work.rs `WAITED`) is no line and no error of its own. On
//! tempo (2026-11-17) the old form printed 29,283 "needs X, which failed" errors under 8 roots.
//!
//! CONTROL (rule 23): file a waited task's failure as `TASK_FAILED` again (work.rs
//! `filed_answer` returning `filed_record` for `Why::Waited`) and the first test sees a
//! "which failed" line for each crate's scope and `errors` past 1, and fails.
//!
//! A child of main.rs's tests: its helpers are theirs.

use super::*;

/// The agent view of `check` (plain lines, the same bytes for the same tree), and its output.
fn agent(io: &Io) -> (Checked, std::string::String) {
    let before = stdout(io).len();
    let checked = ask_view::<fix::Atomic>(io, (Command::Check, "check"), work::Sizes::USUAL, View::Agent, None).expect("the request answers");
    (checked, stdout(io)[before..].to_owned())
}

/// A virtual workspace: `a`'s root file is not UTF-8, so its items' task fails on its own
/// (a refusal, filed: a function of the text), `a`'s names wait on it, `b` and `c` depend on
/// `a`, and `d`, apart, has an unresolved import, a real error.
fn workspace(seed: u64) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\", \"c\", \"d\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
        root.publish(&cx, &Path::new("a/src/lib.rs").expect("a path"), Content::bytes(b"#![no_core]\npub fn \xff() {}\n"), Durable::No).expect("write");
    }
    for dep in ["b", "c"] {
        write(&io, &format!("{dep}/Cargo.toml"), &format!("[package]\nname = \"{dep}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = {{ path = \"../a\" }}\n"));
        write(&io, &format!("{dep}/src/lib.rs"), "#![no_core]\npub fn g() {}\npub fn h() {}\n");
    }
    write(&io, "d/Cargo.toml", "[package]\nname = \"d\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "d/src/lib.rs", "use crate::nope;\npub fn k() {}\n");
    io
}

/// Each root is printed once, after the real error, which keeps its place (`d/src/lib.rs:1:12`):
/// the file that is not UTF-8, an ERROR of the user's at that file (work.rs `FILE_NOT_TEXT`,
/// never a failure: a file blaze cannot read as text is the user's to fix), and `a`'s names,
/// which could not read it, a failure; under `a`'s: "2 crates .. not checked because of it ..:
/// `b`, `c`", the crates that waited on it (through its fork's base: blaze-drive
/// `Request::base_of`); no "which failed" line; the closing line counts one failure beside the
/// two errors; the status is 1. A no-op repeat prints the same bytes.
///
/// Prints `roots: <the output>`.
#[test]
fn a_failed_task_is_one_failure_with_its_crates_counted() {
    let io = workspace(71);
    let (first, out) = agent(&io);
    println!("roots: {out}");
    assert!(!out.contains("which failed"), "no line a waiting task:\n{out}");
    let lines: std::vec::Vec<&str> = out.lines().collect();
    // Two roots, each once: the file's items, and `a`'s names, which read that file.
    let roots: std::vec::Vec<usize> = (0..lines.len()).filter(|&at| lines[at].contains("error[task-failed]") || lines[at].contains("error[file-not-text]")).collect();
    assert_eq!(roots.len(), 2, "each root once:\n{out}");
    let not_text = lines.iter().filter(|line| line.contains("error[file-not-text]")).count();
    assert_eq!(not_text, 1, "the file that is not UTF-8 is an error of its own, once:\n{out}");
    assert!(!out.contains("` failed: "), "every cause is a root printed:\n{out}");
    let scope = roots.iter().copied().find(|&at| lines[at].contains("crate `a`")).unwrap_or_else(|| panic!("`a`'s names failed:\n{out}"));
    let under = lines.get(scope + 1).copied().unwrap_or("");
    assert!(under.contains("2 crates") && under.contains("not checked because of it") && under.ends_with(": `b`, `c`"), "the crates under `a`'s failure:\n{out}");
    let real = lines.iter().position(|line| line.contains("error[unresolved]") && line.contains("d/src/lib.rs:1:12")).unwrap_or_else(|| panic!("the real error with its place:\n{out}"));
    assert!(roots.iter().all(|&at| real < at), "the failures last:\n{out}");
    assert_eq!((first.errors, first.failures), (2, 1), "a waited task is no error, a file not text an error, a failed task a failure:\n{out}");
    // The closing line, then only the summary, which counts the items under the failure's line
    // (not_checked/items.rs `summary`): it never reads better than the lines above it.
    let closing = lines.iter().position(|line| line.starts_with("1 failure, 2 errors")).unwrap_or_else(|| panic!("the closing line:\n{out}"));
    let items_of = |line: &str| {
        let words: std::vec::Vec<&str> = line.split([' ', ',']).collect();
        words.windows(2).find(|pair| pair[1] == "items" || pair[1] == "item").map_or(0, |pair| pair[0].parse::<u32>().expect("a count"))
    };
    let counted: std::vec::Vec<u32> = lines.iter().filter(|line| line.contains("not checked because of it")).map(|line| items_of(line)).filter(|n| *n > 0).collect();
    let (n, failures) = (counted.iter().sum::<u32>(), counted.len());
    match (n, &lines[closing + 1..]) {
        (0, after) => assert!(after.is_empty(), "no item under a failure, no summary:\n{out}"),
        (n, [summary]) => {
            let plural = if failures == 1 { "failure" } else { "failures" };
            assert!(summary.contains("the check is incomplete") && summary.contains(&format!(": {n} behind {failures} {plural}")), "the summary counts the {n} items under the failures:\n{out}");
        }
        (_, _) => panic!("one summary line after the closing line:\n{out}"),
    }
    assert_eq!(first.status, Some(1), "{out}");
    let (again, repeat) = agent(&io);
    assert_eq!(repeat, out, "a no-op check prints the same bytes as the first:\n{}", again.profile);
}

/// A name a refused macro call may have made is no error: the call's own error is printed, and
/// the item that names it is not checked, counted under one line that names the call and why it
/// was refused ("not checked because the call of `make` failed: no rule of the macro's 1
/// matched; ..: `here` (..), `there` (..)"), never an error that the name is unknown.
///
/// Prints `refused: <the output>`.
#[test]
fn a_name_after_a_refused_macro_call_is_counted_under_the_call() {
    let io = Io::simulated(SimWorld::new(73, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(&io, "a/src/lib.rs", "#![no_core]\nmacro_rules! make {\n    (fn $n:ident) => { pub fn $n() {} };\n}\nmake!(struct foo);\npub fn here() { foo(); }\npub fn there() { foo(); }\n");
    let (_, out) = agent(&io);
    println!("refused: {out}");
    assert!(!out.lines().any(|line| line.contains("error[") && line.contains(blaze_ladder::front2::MACRO_REFUSED)), "no name after a refused call is an error:\n{out}");
    assert_eq!(out.lines().filter(|line| line.contains("error[macro-call-failed]")).count(), 1, "the call's own error, once:\n{out}");
    // One cause line naming the refused call AND why (the matcher's reason), the items under it.
    let cause = out.lines().find(|line| line.contains("not checked because the call of `make` (function-like) in src/lib.rs failed: ")).unwrap_or_else(|| panic!("the cause line names the call:\n{out}"));
    assert!(cause.contains("matched") && cause.contains("`here`") && cause.contains("`there`"), "the reason and both items under the call:\n{out}");
    assert_eq!(out.lines().filter(|line| line.contains("not checked because")).count(), 1, "one cause line:\n{out}");
    let (_, repeat) = agent(&io);
    assert_eq!(repeat, out, "the same bytes again");
}

/// MR (2026-11-17): a call refused for a reason other than a failed match (here an `include!`
/// whose nested `env!` asks a variable the build does not set) is named with its reason too,
/// from the names record's row of it (blaze-scope names_record.rs `refused_row`), once: the
/// items behind it under one line "not checked because the call of `include` (function-like)
/// in src/lib.rs failed: `env!(\"NOPE\")`: the variable is not set", never "the records hold no note of its reason" (tempo's
/// check g081341 printed that 62 times, over 261 items).
///
/// CONTROL: with `failed_section` writing rows of `ExpandNote::Failed` notes only, the cause
/// reads "macro call <key> failed, and the records hold no row of it" and the test fails.
///
/// Prints `refused include: <the output>`.
#[test]
fn a_refused_include_is_named_once_with_its_reason() {
    let io = Io::simulated(SimWorld::new(79, Limits::USUAL));
    write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\nresolver = \"2\"\n");
    write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(
        &io,
        "a/src/lib.rs",
        "#![no_core]\n#[rustc_builtin_macro]\nmacro_rules! include { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! env { () => {} }\npub mod ffi {\n    include!(concat!(env!(\"NOPE\"), \"/b.rs\"));\n}\npub fn here() { ffi::x(); }\npub fn there() { ffi::x(); }\n",
    );
    let (_, out) = agent(&io);
    println!("refused include: {out}");
    assert!(!out.contains("hold no"), "every refused call has its row:\n{out}");
    let causes: std::vec::Vec<&str> = out.lines().filter(|line| line.contains("not checked because the call of `include` (function-like) in src/lib.rs failed: ")).collect();
    assert_eq!(causes.len(), 1, "the call's reason once:\n{out}");
    assert!(causes[0].contains("env!(\"NOPE\")") && causes[0].contains("`here`") && causes[0].contains("`there`"), "the variable named, both items under the call:\n{out}");
    let (_, repeat) = agent(&io);
    assert_eq!(repeat, out, "the same bytes again");
}
