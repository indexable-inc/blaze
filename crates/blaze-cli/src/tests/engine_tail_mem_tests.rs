//! THE TAIL'S MEMORY (lane SW2): what `check` holds while it prints the answer's lines
//! (engine_lines.rs `print_all`, print.rs `Scratch`). The lines are STREAMED: a file's text and
//! outline in a frame emptied per file, a line's drawing in a frame emptied per line, nothing
//! drawn kept. The bound, stated (engine_lines.rs `Tail`): what both frames hold at once is at
//! most one file's work and one line at their largest, `peak <= floor`, whatever the number of
//! files or findings. And the waiters of a failed task, grouped under it, a line a cause.
//!
//! Expected (`cargo test -p blaze-cli --bin blaze engine_tail_mem`): "test ... ok" for each, and
//! the lines `tail: streamed peak P floor F; kept peak K` and `waiters: <a cause line>` (two).
//! A child of main.rs's tests: its helpers are theirs. Every text here is made in a frame.

use super::*;

/// Files, and findings a file: 4,000 findings, each at its own function.
const FILES: usize = 40;
const FINDINGS: usize = 100;
/// The frame the fixture's texts are made in.
const FIXTURE: usize = 1 << 20;

/// The fixture's frame.
fn fixture() -> Frame {
    Frame::with_reserve(FIXTURE, bstd::mem::Tag::Other).expect("the fixture's frame")
}

/// File `f`'s path.
fn path_of(f: usize, frame: &Frame) -> bstd::mem::String<&Frame> {
    bstd::mem::format_in!(frame, "src/f{f:02}.rs")
}

/// One line of a file: a function, 15 bytes.
fn function(k: usize, frame: &Frame) -> bstd::mem::String<&Frame> {
    bstd::mem::format_in!(frame, "fn g{k:03}() {{}}\n")
}

/// The [`FILES`] files of [`FINDINGS`] functions each, written in a fresh world.
fn files() -> Io {
    let io = Io::simulated(SimWorld::new(21, Limits::USUAL));
    let mut made = fixture();
    write(&io, "Cargo.toml", MANIFEST);
    for f in 0..FILES {
        made.reset();
        let mut text = bstd::mem::String::new_in(&made);
        for k in 0..FINDINGS {
            text.push_str(function(k, &made).as_str());
        }
        write(&io, path_of(f, &made).as_str(), text.as_str());
    }
    io
}

/// `print_all` over [`FILES`] blocks of [`FINDINGS`] syntax findings each (an outline's form:
/// a path, at the file's start, `start..end` in the finding's function), its frames emptied
/// or `kept`: what it printed and what it counted.
fn printed(kept: bool) -> (std::string::String, engine_lines::Tally) {
    let io = files();
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let request = crate::engine_phase_mem::RequestMem::new().expect("the request's arena");
    let made = fixture();
    let mut lines = engine_lines::Lines::new_in(request.shared());
    for f in 0..FILES {
        let path = path_of(f, &made);
        let name = blaze_drive::Name::file(path.as_bytes());
        let mut at = 0u32;
        for k in 0..FINDINGS {
            let len = u32::try_from(function(k, &made).len()).expect("a short line");
            lines.finding(name, work::SYNTAX, at, at + 2, Some(path.as_bytes()), b"expected an expression");
            at += len;
        }
    }
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
    let tally = if kept {
        print::with_frames_kept(|| engine_lines::print_all(&cx, &root, lines.blocks(), &mut shown, request.shared(), frame, &out))
    } else {
        engine_lines::print_all(&cx, &root, lines.blocks(), &mut shown, request.shared(), frame, &out)
    };
    (stdout(&io), tally.expect("printed"))
}

/// STREAMED: 4,000 findings in 40 files are printed holding at most one file's work and one
/// line (`peak <= floor`). CONTROL (rule 23): with the frames never emptied (print.rs
/// `with_frames_kept`) the same bytes are printed, and the frames hold every file's work: the
/// peak is many floors (40 files' work against one), above four.
#[test]
fn the_tail_holds_one_file_and_one_line() {
    let (streamed, tally) = printed(false);
    let held = tally.tail.held;
    assert_eq!(tally.listed as usize, FILES * FINDINGS, "every finding listed once");
    assert_eq!(streamed.matches("error[").count(), FILES * FINDINGS, "every finding drawn:\n{}", &streamed[..streamed.len().min(600)]);
    assert!(held.text > 0 && held.line > 0 && held.file >= held.text, "the floor is measured: {held:?}");
    assert!(held.peak <= held.floor(), "streamed, the frames hold at most one file's work and one line: {held:?}");
    let (kept, kept_tally) = printed(true);
    let kept_held = kept_tally.tail.held;
    assert_eq!(kept, streamed, "CONTROL: the frames kept, the same bytes are printed");
    assert!(kept_held.peak > 4 * kept_held.floor(), "CONTROL: the frames kept, they hold every file's work, many floors: {kept_held:?}");
    println!("tail: streamed peak {} floor {}; kept peak {}", held.peak, held.floor(), kept_held.peak);
}

/// THE WAITERS, under their cause (not_checked/waited.rs `groups`): a script fails; two crate
/// scopes wait on it and a body waits on the first scope (a chain); a fourth task waits on a
/// script whose failure is in no block. Printed: the failure's own note, then one line for the
/// three under it, naming its reason once and the crates, and one line naming the cause no
/// block holds. CONTROL: the four waiters count as failures, and no one line counts them all
/// (the old "4 task(s) not run: each waits on a task that failed").
#[test]
fn waiters_are_grouped_under_their_cause() {
    let io = Io::simulated(SimWorld::new(22, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let request = crate::engine_phase_mem::RequestMem::new().expect("the request's arena");
    let mut lines = engine_lines::Lines::new_in(request.shared());
    let id = |n: u8| blaze_ids::ContentId([n; 32]);
    let script = blaze_drive::Name::new(blaze_drive::Kind::Script, id(7));
    let lost = blaze_drive::Name::new(blaze_drive::Kind::Script, id(8));
    let scope = |n: u8| blaze_drive::Name::new(blaze_drive::Kind::CrateScope, id(n));
    let body = blaze_drive::Name::new(blaze_drive::Kind::BodyCheck, id(3));
    lines.note(script, engine_lines::Class::Failure, b"the build script of a failed: no module");
    lines.note(scope(1), engine_lines::Class::Waited, not_checked::waited::words(script, Some(("a", false)), frame).as_bytes());
    lines.note(scope(2), engine_lines::Class::Waited, not_checked::waited::words(script, Some(("b", false)), frame).as_bytes());
    lines.note(body, engine_lines::Class::Waited, not_checked::waited::words(scope(1), None, frame).as_bytes());
    lines.note(scope(4), engine_lines::Class::Waited, not_checked::waited::words(lost, Some(("c", false)), frame).as_bytes());
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
    let tally = engine_lines::print_all(&cx, &root, lines.blocks(), &mut shown, request.shared(), frame, &out).expect("printed");
    let said = stdout(&io);
    let grouped = "3 task(s) not run: each waits on a task that failed: the build script of a failed: no module; 2 crates not checked because of it (3 tasks in all): `a`, `b`";
    let unfiled = "which failed (its failure is not among these lines); 1 crate not checked because of it (1 task in all): `c`";
    assert!(said.contains(grouped), "the three under the script's failure, its reason once:\n{said}");
    assert!(said.lines().any(|line| line.contains("1 task(s) not run: each waits on `") && line.contains(unfiled)), "the cause no block holds, named:\n{said}");
    assert_eq!((tally.waited, tally.failures), (4, 5), "CONTROL: the waiters count as failures");
    assert!(!said.contains("4 task(s) not run"), "CONTROL: no one line for all the waiters:\n{said}");
    for line in said.lines().filter(|line| line.contains("task(s) not run")) {
        println!("waiters: {line}");
    }
}
