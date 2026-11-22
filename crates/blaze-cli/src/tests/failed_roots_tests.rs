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
    let checked = check_view::<fix::Atomic>(io, View::Agent, None);
    (checked, stdout(io)[before..].to_owned())
}

/// [`agent`] with `--each`: every body not checked drawn on a line of its own.
fn agent_each(io: &Io) -> (Checked, std::string::String) {
    let before = stdout(io).len();
    let checked = check_each(io, View::Agent);
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

/// Each root is printed once, and the real error keeps its place (`d/src/lib.rs:1:12`): the
/// file that is not UTF-8, an ERROR of the user's at that file (work.rs `FILE_NOT_TEXT`, never
/// a failure: a file blaze cannot read as text is the user's to fix), its subject the FILE
/// (`a/src/lib.rs`, said once, never its task's name), and `a`'s names, which could not read it,
/// a failure. The bodies of `b` and `c`, whose check scopes hold `a`, are not checked under that
/// one cause: ONE line counts them and names them ("4 bodies not checked because its member's
/// check scope holds `a` ..: `g` (b/src/lib.rs), .., `h` (c/src/lib.rs)"), never a note a body
/// (engine_lines.rs `grouped_lines`, as a refused call's bodies are). AS RECORDS (D164): four
/// noted findings, the plan's refusal, their subjects the four bodies, one group of four; `--each`
/// prints each one's words. No "which failed" line; no other error; the closing line counts one
/// failure beside the two errors, and the summary the 4 items; the status is 1. A no-op repeat
/// prints the same bytes.
///
/// Prints `roots: <the output>` and `roots, each: <the output>`.
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
    let not_text: std::vec::Vec<&&str> = lines.iter().filter(|line| line.contains("error[file-not-text]")).collect();
    assert_eq!(not_text.len(), 1, "the file that is not UTF-8 is an error of its own, once:\n{out}");
    // Its subject is the file, said once; never its task's name.
    assert!(not_text[0].contains("a/src/lib.rs") && not_text[0].matches("a/src/lib.rs").count() == 1, "the file named once as the finding's subject:\n{out}");
    assert!(!not_text[0].contains("FileItems("), "the subject is the file, not its task's name:\n{out}");
    assert!(!out.contains("` failed: "), "every cause is a root printed:\n{out}");
    assert!(roots.iter().any(|&at| lines[at].contains("error[task-failed]") && lines[at].contains("crate `a`")), "`a`'s names failed:\n{out}");
    // No other error: the two roots and the real one, which keeps its place.
    assert!(lines.iter().any(|line| line.contains("error[unresolved]") && line.contains("d/src/lib.rs:1:12")), "the real error with its place:\n{out}");
    assert_eq!(lines.iter().filter(|line| line.contains("error[")).count(), 3, "no error but the two roots and the real one:\n{out}");
    // The bodies under `a`'s failure: one line, counting and naming them, with the crates
    // they are of (`b`, `c`), never a note a body.
    assert!(!out.contains("the body of `"), "no note a body under one cause:\n{out}");
    let under: std::vec::Vec<&&str> = lines.iter().filter(|line| line.contains("not checked because") && line.contains("holds `a`")).collect();
    assert_eq!(under.len(), 1, "one line for the bodies under `a`'s failure:\n{out}");
    assert!(under[0].contains("4 bodies"), "the line counts the bodies:\n{out}");
    for body in ["`g` (b/src/lib.rs)", "`h` (b/src/lib.rs)", "`g` (c/src/lib.rs)", "`h` (c/src/lib.rs)"] {
        assert!(under[0].contains(body), "{body} named under `a`'s failure:\n{out}");
    }
    assert_eq!((first.errors, first.failures), (2, 1), "a waited task is no error, a file not text an error, a failed task a failure:\n{out}");
    // The closing line, then only the summary, which counts the items not checked.
    let closing = lines.iter().position(|line| line.starts_with("1 failure, 2 errors")).unwrap_or_else(|| panic!("the closing line:\n{out}"));
    match &lines[closing + 1..] {
        [summary] => assert!(summary.contains("the check is incomplete") && summary.contains("blaze could not check 4 items"), "the summary counts the 4 bodies:\n{out}"),
        _ => panic!("one summary line after the closing line:\n{out}"),
    }
    assert_eq!(first.status, Some(1), "{out}");
    let (again, repeat) = agent(&io);
    assert_eq!(repeat, out, "a no-op check prints the same bytes as the first:\n{}", again.profile);
    // The records (D164): a noted finding a body, each the plan's refusal under `a`'s failure,
    // a grouped kind, its subject the body (its file and item), never a task; the default
    // view's one line is the query's one group, of the four.
    let noted = super::world_run::noted(&io);
    let under_a: std::vec::Vec<&super::world_run::Noted> = noted.iter().filter(|one| one.of.contains("holds `a`")).collect();
    assert!(
        under_a.iter().all(|one| one.cause == blaze_report::finding::CauseKind::PlanRefused && one.kind == blaze_report::frame::words::Kind::Grouped),
        "each body under `a`'s failure is the plan's refusal, grouped: {under_a:?}"
    );
    let mut bodies: std::vec::Vec<(&str, &str)> = under_a.iter().map(|one| (one.file.as_str(), one.item.as_str())).collect();
    bodies.sort_unstable();
    bodies.dedup();
    assert_eq!(bodies, [("b/src/lib.rs", "g"), ("b/src/lib.rs", "h"), ("c/src/lib.rs", "g"), ("c/src/lib.rs", "h")], "the four bodies are the subjects: {noted:?}");
    let groups: std::vec::Vec<usize> = super::world_run::groups(&noted).into_iter().filter(|(of, _)| of.contains("holds `a`")).map(|(_, count)| count).collect();
    assert_eq!(groups, [4], "one group of four under `a`'s failure: {noted:?}");
    // `--each` is a query too: a line a finding, each its own words (one of them as a
    // snapshot of the renderer).
    let (_, each) = agent_each(&io);
    println!("roots, each: {each}");
    for one in &under_a {
        assert!(each.lines().any(|line| line.contains(one.words.as_str())), "{one:?} under --each:\n{each}");
    }
    assert!(each.lines().any(|line| line.contains("`b/src/lib.rs`: body `g`: not checked because ")), "the --each form:\n{each}");
}

/// A name a refused macro call may have made is no error: the call's own error is printed, and
/// the item that names it is not checked, counted under one line that names the call and why it
/// was refused ("not checked because the call of `make` failed: no rule of the macro's 1
/// matched; ..: `here` (..), `there` (..)"), never an error that the name is unknown. The call's
/// failure is said ONCE on that line (the engine said it twice a body before 2026-11-19: once as
/// the cause and again after the unknown name). AS RECORDS (D164): a noted finding a body, its
/// cause the refused call, the call's failure once in its key, one group of the two.
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
    assert_eq!(cause.matches("no rule of the macro's 1 matched").count(), 1, "the call's failure said once:\n{out}");
    assert!(out.contains("blaze could not check 2 items"), "both bodies counted, once each:\n{out}");
    let (_, repeat) = agent(&io);
    assert_eq!(repeat, out, "the same bytes again");
    // The records (D164): a noted finding a body under the call, its cause the refused call
    // (the call's failure once, in its key), its subject the body; one group of the two.
    let noted = super::world_run::noted(&io);
    let under: std::vec::Vec<&super::world_run::Noted> = noted.iter().filter(|one| one.cause == blaze_report::finding::CauseKind::RefusedCall).collect();
    let mut items: std::vec::Vec<&str> = under.iter().map(|one| one.item.as_str()).collect();
    items.sort_unstable();
    items.dedup();
    assert_eq!(items, ["here", "there"], "both bodies are under the call: {noted:?}");
    assert!(
        under.iter().all(|one| one.of.starts_with("the call of `make` (function-like) in src/lib.rs failed: ") && one.of.matches("no rule of the macro's 1 matched").count() == 1),
        "each cause is the call and its failure, once: {under:?}"
    );
    assert_eq!(super::world_run::groups(&noted).into_iter().map(|(_, count)| count).collect::<std::vec::Vec<usize>>(), [2], "one group, of the two: {noted:?}");
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
    assert_eq!(causes[0].matches("the variable is not set").count(), 1, "the call's reason said once:\n{out}");
    let (_, repeat) = agent(&io);
    assert_eq!(repeat, out, "the same bytes again");
    // `--each`: a line a body, each with the call's reason once.
    let (_, each) = agent_each(&io);
    println!("refused include, each: {each}");
    let bodies: std::vec::Vec<&str> = each.lines().filter(|line| line.contains("not checked because the call of `include` (function-like) in src/lib.rs failed: ")).collect();
    assert_eq!(bodies.len(), 2, "one line a body under --each:\n{each}");
    assert!(bodies.iter().all(|line| line.matches("the variable is not set").count() == 1 && !line.contains("hold no")), "each with the reason once:\n{each}");
}
