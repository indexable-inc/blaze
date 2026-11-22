//! What a command's findings cost its memory (print.rs `Scratch`, main.rs `draw`): a file's text
//! and outline are read once a file into a frame freed a file, a finding is drawn in a frame
//! freed a finding, and the command's own frame keeps a `Listed`, the words and a path a finding.
//! A child of main.rs's tests: its helpers are theirs.

use super::*;

/// Folders, files a folder and bodies a file: no file has more than 20 top-level items.
const DIRS: usize = 20;
const FILES: usize = 20;
const BODIES: usize = 10;

/// [`DIRS`] modules of [`FILES`] modules of [`BODIES`] functions each, every body one syntax
/// error (`let x = ;`: the Rust Reference, "let statements": `=` is followed by an expression).
fn broken() -> Io {
    let io = Io::simulated(SimWorld::new(14, Limits::USUAL));
    let mut lib = std::string::String::new();
    for a in 0..DIRS {
        writeln!(lib, "mod a{a};").expect("a string takes text");
        let mut dir = std::string::String::new();
        for m in 0..FILES {
            writeln!(dir, "mod m{m};").expect("a string takes text");
            let mut text = std::string::String::new();
            for k in 0..BODIES {
                writeln!(text, "fn g{k:03}() {{ let x = ; }}").expect("a string takes text");
            }
            write(&io, &format!("src/a{a}/m{m}.rs"), &text);
        }
        write(&io, &format!("src/a{a}.rs"), &dir);
    }
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", &lib);
    io
}

/// 4,000 findings in 400 files are listed and drawn by the engine's check, each with its id,
/// and the closing line follows them (ported from the old checker's command, 2026-11-19: the
/// tight frames it was run in were that command's `work::Sizes`; the engine's request frame is
/// engine_check.rs `REQUEST_FRAME`, and print.rs `Scratch` is the same drawing).
///
/// What that work takes in all is many times the scratch: the 400 outlines alone are about
/// 8 MiB (20 KiB each), and the 4,000 drawings more than 256 KiB. Before `Scratch` each FINDING
/// kept its file's outline and four copies of its text in the command's frame (blaze's own
/// 3,487 findings filled a 64 MiB frame and the command panicked while printing: drive.md,
/// runner 13). So this passes only while a file's work is freed a file and a drawing's a
/// finding. Run once with each reset removed (2026-11-16); the test then fails with:
/// `one.reset()` out of print.rs `file_findings`: `the request answers: Draw(Stopped { doing:
/// "drawing", path: "src/a7/m16.rs", cut: false, file: 0, one: 262132, limit: 262144 })`;
/// `scratch.file.reset()` out of print.rs `list`: `the request answers: Draw(Stopped { doing:
/// "placing", path: "src/a13/m2.rs", cut: false, file: 259840, one: 0, limit: 262144 })`.
#[test]
fn thousands_of_findings_are_drawn_in_frames_a_fraction_of_their_work() {
    let io = broken();
    let checked = check(&io);
    let out = stdout(&io);
    assert_eq!(checked.failures, 0, "{}", checked.profile);
    let all = DIRS * FILES * BODIES;
    assert_eq!(checked.errors as usize, all, "{}", checked.profile);
    assert_eq!(out.matches("error[syntax]").count(), all, "every finding is drawn");
    assert_eq!(checked.ids.len(), all);
    assert_eq!(checked.status, Some(1), "the closing line is printed and the command fails");
    assert!(out.lines().last().is_some_and(|last| last.contains("4000 errors")), "the closing line: {:?}", out.lines().last());
    // The first and the last finding, each at its own line of its own file.
    assert!(out.contains("src/a0/m0.rs:1:") && out.contains(&format!("src/a{}/m{}.rs:{BODIES}:", DIRS - 1, FILES - 1)), "{}", &out[..out.len().min(600)]);
}

/// A file written between the listing of its findings and their drawing is refused by name: a
/// `Listed` holds its file's digest (fix.rs `text_id`), which the drawing's own read is checked
/// against. Here: the digest of other text.
#[test]
fn a_finding_is_not_drawn_in_text_it_was_not_listed_in() {
    let io = Io::simulated(SimWorld::new(15, Limits::USUAL));
    write(&io, "src/lib.rs", "fn f() {}\n");
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
    let listed = |read| fix::Listed {
        path: Some("src/lib.rs"),
        about: blaze_drive::Name::file(b"src/lib.rs"),
        read,
        area: blaze_report::frame::words::Area::Cli,
        rule: crate::units::TOOLCHAIN_NAMED,
        words: b"bstd",
        item: 0,
        start: 3,
        end: 4,
        id: fix::Id::of_text("abcde"),
        fix: None,
    };
    let mut scratch = print::Scratch::new(SCRATCH).expect("frames");
    shown.listed.push(listed(fix::text_id(b"fn f() {}\n")));
    print::file_findings(&cx, &root, Some("src/lib.rs"), 0..1, &mut shown, &mut scratch, &out).expect("drawn in the text it was listed in");
    shown.listed.push(listed(fix::text_id(b"fn g() {}\n")));
    let refused = print::file_findings(&cx, &root, Some("src/lib.rs"), 1..2, &mut shown, &mut scratch, &out).expect_err("other text");
    assert_eq!(refused.to_string(), "`src/lib.rs` was written while blaze drew its findings: their places are the old text's; run the command again");
}
