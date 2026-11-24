//! D70 through the command: ids, the views, and `blaze check --fix` on the simulated file
//! system (fix.rs). A child of main.rs's tests: its helpers are theirs.

use blaze_own::Alloc;
use blaze_report::frame::words::{Area, HelpKind};
use blaze_typeck::suggest::{Cand, Hole, Suggests, Taken};
use blaze_typeck::{PTy, PatId, Range};

use super::*;
use crate::fix::{Change, Id, Listed};

/// A package of the blaze edition that names the toolchain's crate again: one finding, with a
/// certain fix (its line deleted).
const BLAZE_MANIFEST: &str = "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\nbstd = { package = \"blaze-std\", path = \"/x/blaze-std\" }\n";
const BSTD_LINE: &str = "bstd = { package = \"blaze-std\", path = \"/x/blaze-std\" }\n";

fn agent(io: &Io, fix: Option<&[&str]>) -> (Checked, std::string::String) {
    let before = stdout(io).len();
    let checked = check_view::<fix::Atomic>(io, View::Agent, fix);
    (checked, stdout(io)[before..].to_owned())
}

/// `out` is exactly `golden`'s lines, each ended by a newline; a difference names its line.
fn golden(out: &str, golden: &[&str]) {
    let lines: std::vec::Vec<&str> = out.split_terminator('\n').collect();
    for (i, (got, want)) in lines.iter().zip(golden.iter()).enumerate() {
        assert_eq!(got, want, "line {i}");
    }
    assert_eq!(lines.len(), golden.len(), "{out}");
    assert!(out.ends_with('\n'), "{out}");
}

fn read(io: &Io, path: &str) -> std::string::String {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let mut bytes = cx.bytes();
    root.read_all(&cx, &Path::new(path).expect("a path"), &mut bytes).expect("read");
    std::string::String::from_utf8(bytes.as_slice().to_vec()).expect("UTF-8")
}

/// The ids this tree gives the two findings (run t82, 2026-11-21; `gv7fz` and `xqq2p` before).
/// D70: "THE ID is derived from what the finding IS ... a digest of the item it sits in, its
/// code and its item-relative range"; the item enters through its chain (fix.rs `item_in`,
/// work.rs `item_chain`), which hashes the outline's item digests (blaze-file-items chain.rs:
/// `item.hash` / `item.signature_hash`). The code, the path and the ranges are unchanged (the
/// columns are), so the item digests are what moved (blaze-file-items digest.rs changed
/// since C3). That cause is not isolated by a run. An id is a function of the tree that drew
/// it, never pinned across a change of what it digests.
const UNRESOLVED_ID: &str = "yvsnk";
const SYNTAX_ID: &str = "8f3db";

/// The agent view of the command, exactly: a syntax error inside a body and an unresolved import.
#[test]
fn the_agent_view_of_a_syntax_error_and_an_unresolved_import() {
    let io = Io::simulated(SimWorld::new(9, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", "use crate::nope;\nfn f() { let x = ; }\n");
    let (checked, out) = agent(&io, None);
    assert_eq!(checked.status, Some(1), "{out}");
    assert_eq!(out, format!("{UNRESOLVED_ID}  error[unresolved]  src/lib.rs:1:12  no `nope` in `crate`\n{SYNTAX_ID}  error[syntax]  src/lib.rs:2:17  expected expression\n2 errors: blaze check\n"), "D70: the id is \"a digest of the item it sits in, its code and its item-relative range\"");
    let (_, again) = agent(&io, None);
    assert_eq!(again, out, "an unchanged repeat says the same");
    let before = stdout(&io).len();
    let _ = check_view::<fix::Atomic>(&io, View::Json, None);
    let unresolved = format!(r#"{{"id":"{UNRESOLVED_ID}","severity":"error","code":"unresolved","path":"src/lib.rs","line":1,"col":12,"end_line":1,"end_col":16,"message":"no `nope` in `crate`","fix":null}}"#);
    let syntax = format!(r#"{{"id":"{SYNTAX_ID}","severity":"error","code":"syntax","path":"src/lib.rs","line":2,"col":17,"end_line":2,"end_col":17,"message":"expected expression","fix":null}}"#);
    golden(
        &stdout(&io)[before..],
        &[
            unresolved.as_str(),
            syntax.as_str(),
            r#"{"errors":2,"warnings":0,"fixes":0,"command":"blaze check","not_checked":[]}"#,
        ],
    );
}

/// An id is what the finding is: an edit above it in the same file leaves it; an edit of its
/// own item changes it (the other finding's stands).
#[test]
fn an_edit_above_keeps_the_id_and_an_edit_of_its_item_changes_it() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", "use crate::nope;\nfn f() { let x = ; }\n");
    let (first, out) = agent(&io, None);
    assert_eq!(first.ids.len(), 2, "{:?}", first.ids);
    write(&io, "src/lib.rs", "fn above() {}\n\nuse crate::nope;\nfn f() { let x = ; }\n");
    let (moved, moved_out) = agent(&io, None);
    assert_eq!(moved.ids, first.ids, "an edit above moves no id:\n{out}\n{moved_out}");
    write(&io, "src/lib.rs", "fn above() {}\n\nuse crate::nope;\nfn f() { let y = ; }\n");
    let (edited, _) = agent(&io, None);
    let syntax = |c: &Checked| c.ids.iter().find(|(code, _)| code == "syntax").cloned();
    let unresolved = |c: &Checked| c.ids.iter().find(|(code, _)| code == "unresolved").cloned();
    assert_ne!(syntax(&edited), syntax(&first), "an edit of its item changes its id");
    assert_eq!(unresolved(&edited), unresolved(&first), "the other item's id stands");
}

/// The type checker's fixture finding (`suggests` exactly as the checker gives it for the
/// owner's case), made a finding by the driver's glue and printed in each view; then its fix
/// applied by id.
#[test]
fn the_checkers_fix_in_every_view_and_applied_by_id() {
    const OWNERS: &str = "fn add(a: u32, b: u32) -> u32 { a + c }\n";
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    write(&io, "src/main.rs", OWNERS);
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let mut suggests = Suggests::new_in(Alloc::global());
    let cand = |start, unused| Cand { taken: Taken::Local(PatId(start)), name: Range { start, end: start + 1 }, ty: PTy(0), unused, depth: 0 };
    suggests.cands.push(cand(7, false));
    suggests.cands.push(cand(15, true));
    suggests.holes.push(Hole { at: Range { start: 36, end: 37 }, expected: Some(PTy(0)), first: 0, len: 2 });
    let mut listed: Vec<Listed<'_>, &Frame> = Vec::new_in(frame);
    let item = (0, OWNERS.len() as u32 - 1, 0);
    fix::checker_findings(&suggests, "src/main.rs", OWNERS.as_bytes(), item, blaze_drive::Name::file(b"src/main.rs"), &|_| "u32", Alloc::global(), &mut listed).expect("the record and the text agree");
    fix::lengths(&mut listed, frame);
    assert_eq!(listed.len(), 1);
    let id = listed[0].id.as_str().to_owned();
    let out = io.need_out().expect("out");
    let mut said = std::vec::Vec::new();
    for view in [View::Agent, View::Json, View::Person] {
        let before = stdout(&io).len();
        let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, view, "/ws", frame);
        print::finding(&cx, &listed[0], OWNERS.as_bytes(), &mut shown, frame, &out).expect("drawn");
        print::closing(&cx, &shown, 1, 0, None, None, None, frame, &out).expect("closed");
        said.push(stdout(&io)[before..].to_owned());
    }
    assert_eq!(said[0], format!("{id}  error[E0425]  src/main.rs:1:37  cannot find value `c` in this scope\n       fix: replace `c` with `b` (1:37-1:38): `b` is in scope, is a `u32`, and is never used\n1 error, 1 fix: blaze check --fix {id}\n"));
    assert_eq!(said[1], format!("{{\"id\":\"{id}\",\"severity\":\"error\",\"code\":\"E0425\",\"path\":\"src/main.rs\",\"line\":1,\"col\":37,\"end_line\":1,\"end_col\":38,\"message\":\"cannot find value `c` in this scope\",\"fix\":{{\"start_line\":1,\"start_col\":37,\"end_line\":1,\"end_col\":38,\"replace\":\"c\",\"with\":\"b\",\"reason\":\"`b` is in scope, is a `u32`, and is never used\"}}}}\n{{\"errors\":1,\"warnings\":0,\"fixes\":1,\"command\":\"blaze check --fix {id}\",\"not_checked\":[]}}\n"));
    assert_eq!(said[2], format!("src/main.rs:1:37: error[E0425]: cannot find value `c` in this scope\n  fix: blaze check --fix {id}\nhelp: `b` is in scope, is a `u32`, and is never used\n1 error · 1 fixable: blaze check --fix {id} · 1 file checked · 0.0 ms\n"));
    assert_eq!(id, "p45mr", "the id is a digest of the item, the code and the range");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let applied = fix::apply(&cx, &root, &fix::Atomic, &listed, &[&id[..2]], frame).expect("no effect failed").expect("a unique prefix applies");
    assert_eq!(applied.fixed.len(), 1);
    assert_eq!(read(&io, "src/main.rs"), "fn add(a: u32, b: u32) -> u32 { a + b }\n");
}

/// The manifest finding with a certain fix, through the command: its agent view, then `--fix
/// <id>` deletes the line and the check that runs again finds nothing of it.
#[test]
fn fix_deletes_the_toolchain_line_by_id_and_checks_again() {
    let io = Io::simulated(SimWorld::new(5, Limits::USUAL));
    write(&io, "Cargo.toml", BLAZE_MANIFEST);
    write(&io, "src/lib.rs", "pub fn f() {}\n");
    let (first, out) = agent(&io, None);
    let id = first.ids.iter().find(|(code, _)| code == "toolchain-crate-named").map(|(_, id)| id.clone()).unwrap_or_else(|| panic!("{out}"));
    assert_eq!(id, "vf9w8");
    golden(
        &out,
        &[
            r#"vf9w8  error[toolchain-crate-named]  Cargo.toml:7:1  under edition blaze-2026.11 blaze gives this package `bstd`: this line names another copy"#,
            r#"       fix: delete `bstd = { package = "blaze-std", path = "/x/blaze-std" }` (7:1-8:1): under edition blaze-2026.11 the package is given `bstd` with no line"#,
            "1 error, 1 fix: blaze check --fix vf9w8",
        ],
    );
    let (fixed, out) = agent(&io, Some(&[&id]));
    assert!(!fixed.refused, "{out}");
    assert_eq!(read(&io, "Cargo.toml"), BLAZE_MANIFEST.replace(BSTD_LINE, ""));
    golden(&out, &["note: applied 1 fix in 1 file: vf9w8", "0 errors: blaze check"]);
}

fn one(path: &'static str, text: &'static str, needle: &str, id: &str) -> Listed<'static> {
    let start = text.find(needle).expect("in the text") as u32;
    let end = start + needle.len() as u32;
    Listed {
        path: Some(path),
        about: blaze_drive::Name::file(path.as_bytes()),
        read: fix::text_id(text.as_bytes()),
        area: Area::Cli,
        rule: units::TOOLCHAIN_NAMED,
        words: b"bstd",
        item: 0,
        start,
        end,
        id: Id::of_text(id),
        fix: Some(Change { start, end, insert: "", reason: HelpKind::ToolchainLine, args: ["bstd", ""], given: 1 }),
    }
}

/// Every refusal names what it refuses, and nothing is written.
#[test]
fn fix_refuses_by_name_and_writes_nothing() {
    const A: &str = "keep\nDROP ONE\nDROP TWO\n";
    let io = Io::simulated(SimWorld::new(2, Limits::USUAL));
    write(&io, "a.txt", A);
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let listed = [one("a.txt", A, "DROP ONE\n", "ab2cd"), one("a.txt", A, "DROP TWO\n", "ab2ce"), one("a.txt", A, "ONE\nDROP", "zzzzz")];
    let said = |selectors: &[&str]| match fix::apply(&cx, &root, &fix::Atomic, &listed, selectors, frame).expect("no effect failed") {
        Ok(_) => std::string::String::from("applied"),
        Err(refusal) => refusal.to_string(),
    };
    assert_eq!(said(&["ab2c"]), "--fix ab2c: the prefix begins 2 ids: ab2cd ab2ce; give more of one");
    assert_eq!(said(&["q9"]), "--fix q9: no finding of this check has this id, code or path (an id changes when its item's text does: check again for today's)");
    assert_eq!(said(&["ab2cd", "zzzzz"]), "--fix: the fixes of ab2cd and zzzzz change the same bytes: apply one, then check again");
    assert_eq!(read(&io, "a.txt"), A, "nothing written");
    write(&io, "a.txt", "keep\nDROP ONE\nDROP TWO\nnew\n");
    assert_eq!(said(&["ab2cd"]), "--fix ab2cd: its file changed while this command ran: nothing written");
    write(&io, "a.txt", A);
    assert_eq!(said(&["ab2cd", "ab2ce"]), "applied");
    assert_eq!(read(&io, "a.txt"), "keep\n");
}

/// The test's power cut: after `left` files are written, the machine loses power.
struct CrashAfter<'io, W> {
    io: &'io Io,
    left: core::cell::Cell<usize>,
    inner: W,
}

impl<W: fix::Writer> fix::Writer for CrashAfter<'_, W> {
    fn replace(&self, cx: &Cx, root: &Dir<ReadWrite>, path: &Path, bytes: &[u8]) -> Result<(), CliError> {
        if self.left.get() == 0 {
            assert!(self.io.crash(), "a simulated world");
            return Err(CliError::Text { what: "the power was cut" });
        }
        self.left.set(self.left.get() - 1);
        self.inner.replace(cx, root, path, bytes)
    }
}

/// Two files fixed, the power cut after 0, 1 or 2 of them: each file is whole, old or new.
fn crash_case<W: fix::Writer>(seed: u64, cut: usize, inner: W) -> Result<(), std::string::String> {
    let big = |tag: &str| -> &'static str {
        let mut text = std::string::String::new();
        for i in 0..60 {
            writeln!(text, "// {tag} line {i:03} of a file larger than a sector").expect("a string takes text");
        }
        text.push_str("DROP\n");
        for i in 0..60 {
            writeln!(text, "// {tag} after {i:03}").expect("a string takes text");
        }
        std::boxed::Box::leak(text.into_boxed_str())
    };
    let (a, b) = (big("a"), big("b"));
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").root();
    for (path, text) in [("a.rs", a), ("b.rs", b)] {
        root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(text.as_bytes()), Durable::Yes).expect("a durable start");
    }
    let listed = [one("a.rs", a, "DROP\n", "aaaaa"), one("b.rs", b, "DROP\n", "bbbbb")];
    let writer = CrashAfter { io: &io, left: core::cell::Cell::new(cut), inner };
    let done = fix::apply(&cx, &root, &writer, &listed, &["all"], frame);
    if cut >= 2 {
        assert!(done.is_ok(), "both written");
        io.crash();
    }
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a context after the crash");
    for (path, old) in [("a.rs", a), ("b.rs", b)] {
        let mut bytes = cx.bytes();
        root.read_all(&cx, &Path::new(path).expect("a path"), &mut bytes).map_err(|e| format!("{path}: {e}"))?;
        let new = old.replace("DROP\n", "");
        if bytes.as_slice() != old.as_bytes() && bytes.as_slice() != new.as_bytes() {
            return Err(format!("seed {seed}, cut after {cut}: {path} is neither old nor new: {} bytes", bytes.len()));
        }
    }
    Ok(())
}

/// The write is atomic per file: a power cut anywhere leaves each file old or new. Control: the
/// planted bug (written in place, unsynced) is caught.
#[test]
fn a_crash_mid_fix_leaves_each_file_old_or_new() {
    for seed in 0..12 {
        for cut in 0..=2 {
            crash_case(seed, cut, fix::Atomic).unwrap_or_else(|e| panic!("{e}"));
        }
    }
    let caught = (0..12).flat_map(|seed| (0..=2).map(move |cut| (seed, cut))).find_map(|(seed, cut)| crash_case(seed, cut, fix::InPlace).err());
    // Caught as: "seed 0, cut after 2: a.rs is neither old nor new: 3605 bytes".
    assert!(caught.is_some(), "the planted in-place write must be caught");
}

/// Between `blaze check` and `blaze check --fix <id>`, an edit is drawn: none, above the line,
/// below it, of the line itself, the line deleted, or another file. After `--fix`, the manifest
/// is exactly the edited one with the line deleted, or the fix was refused by its id and the
/// manifest is exactly the edited one. And over drawn projects, the ids of a run are unique and
/// none begins another.
fn fix_after_an_edit(tape: &blaze_sim::Tape) -> Result<(), blaze_sim::Broke> {
    let io = Io::simulated(SimWorld::new(tape.next_u64("sim seed"), Limits::USUAL));
    let mut lib = std::string::String::from("pub fn f() {}\n");
    // At most one `use`: two unnamed `use` items share an item chain today, so the second's
    // finding is placed at the first (names.rs `placed`; reported, not yet fixed).
    if tape.range(0..2, "a bad import") == 1 {
        lib.push_str("use crate::nope;\n");
    }
    for k in 0..tape.range(0..4, "broken bodies") {
        writeln!(lib, "fn g{k}() {{ let x = ; }}").expect("text");
    }
    write(&io, "Cargo.toml", BLAZE_MANIFEST);
    write(&io, "src/lib.rs", &lib);
    let (first, out) = agent(&io, None);
    for (i, (_, a)) in first.ids.iter().enumerate() {
        if first.ids.iter().enumerate().any(|(j, (_, b))| i != j && b.starts_with(a.as_str())) {
            return Err(blaze_sim::Broke::Oracle(format!("id {a} begins another: {:?}", first.ids)));
        }
    }
    let Some((_, id)) = first.ids.iter().find(|(code, _)| code == "toolchain-crate-named").cloned() else {
        return Err(blaze_sim::Broke::Oracle(format!("no toolchain finding:\n{out}")));
    };
    let edit = tape.range(0..6, "edit");
    let edited = match edit {
        0 => BLAZE_MANIFEST.to_owned(),
        1 => BLAZE_MANIFEST.replace("version", "description = \"above\"\nversion"),
        2 => format!("{BLAZE_MANIFEST}\n[features]\nx = []\n"),
        3 => BLAZE_MANIFEST.replace("/x/blaze-std", "/y/blaze-std"),
        4 => BLAZE_MANIFEST.replace(BSTD_LINE, ""),
        _ => {
            write(&io, "src/lib.rs", "pub fn h() {}\n");
            BLAZE_MANIFEST.to_owned()
        }
    };
    write(&io, "Cargo.toml", &edited);
    let (fixed, out) = agent(&io, Some(&[&id]));
    let now = read(&io, "Cargo.toml");
    let touched = matches!(edit, 3 | 4);
    match (touched, fixed.refused) {
        (false, false) if now == edited.replace(BSTD_LINE, "") => Ok(()),
        (true, true) if now == edited && out.contains(&format!("--fix {id}:")) => Ok(()),
        _ => Err(blaze_sim::Broke::Oracle(format!("edit {edit}: refused {}; the manifest:\n{now}\nsaid:\n{out}", fixed.refused))),
    }
}

#[test]
fn a_fix_after_drawn_edits_is_exact_or_refused_by_its_id() {
    blaze_sim::check(blaze_sim::Config::cases(24), fix_after_an_edit);
}

#[test]
fn fix_minimizes_unused_imports_and_removes_unused_mut() {
    let io = Io::simulated(SimWorld::new(7, Limits::USUAL));
    write(
        &io,
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace.metadata.blaze]\nlints = \"rust\"\nlints-add = [\"unused-import\", \"unused-mut\"]\n",
    );
    write(&io, "src/sub.rs", "pub struct Io;\npub struct Missing;\n");
    write(
        &io,
        "src/lib.rs",
        "mod sub;\n\nuse sub::{Io, Missing};\n\npub fn check(mut x: u32) -> (u32, Missing) {\n    let mut y = x + 1;\n    (y, Missing)\n}\n",
    );
    let (fixed, out) = agent(&io, Some(&["all"]));
    assert!(!fixed.refused, "{out}");
    assert_eq!(
        read(&io, "src/lib.rs"),
        "mod sub;\n\nuse sub::Missing;\n\npub fn check(x: u32) -> (u32, Missing) {\n    let y = x + 1;\n    (y, Missing)\n}\n",
        "output was:\n{out}"
    );
}
