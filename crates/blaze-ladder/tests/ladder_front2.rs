//! The ladder's rungs through the NEW front end (front2, `FrontChoice::New`), each in this
//! test's process with fd 1 on a pipe, as tests/ladder.rs runs them through the old one. A rung
//! passes through front2 only when its status and stdout equal its `expected` and front2 raised
//! no refusal. Each rung prints one line: `PASS` or the first refusal by name (the work list
//! for the switch; crates/blaze-cli/tests/ladder/README.md, "The seam"). The rungs in
//! [`PASSING`] must stay passing.

#![feature(allocator_api)]

mod rungs;

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};
use rungs::{RUNNING, SUPPORT, expected, ladder, sources};

/// The rungs that pass through front2 today: the next one is added when it passes.
const PASSING: &[&str] = &["1-empty", "2-exit", "3-hello", "4-modules", "5-generics", "6-aggregates", "7-traits", "7a-drops", "7c-main-io", "support/s1-layout", "support/s3-extern-c", "support/s4-intrinsics", "front2/f1-defaults", "front2/f2-generic-self", "7b-dyn", "front2/f4-assoc", "front2/f5-closures", "front2/f6-literals", "front2/x0-fpat-01", "front2/x0-dropglue-01", "front2/x0-reach-01", "support/s2-statics", "support/s5-thread-local", "front2/x-niche-01", "front2/x0-static-01", "front2/x0-flit-01", "front2/x0-extern-local-01", "front2/x0-static-02-late-body", "front2/x0-dropglue-02-slice", "front2/x0-atomic-01-orderings", "front2/x0-int-11-three-way", "front2/x0-ref-13-slice-get-unchecked", "front2/x0-adt-09-discriminant-high-bit", "front2/x0-adt-10-packed", "front2/x0-simd-01-layout", "front2/x0-simd-02-lanes", "front2/x0-ref-12t-wide-ptr-eq", "front2/x0-const-assoc-01", "front2/x0-const-assoc-02-order", "front2/x0-const-block-01", "front2/x0-op-01-projection-operand", "front2/x0-ref-11t-deref-unsized", "front2/x0-static-04-generic-field", "front2/x0-move-11-field-of-temp", "front2/x0-move-12-match-on-deref", "front2/x0-drop-11-zst-temporary", "front2/x0-drop-12-zst-order", "front2/x0-drop-12-let-else-temp", "front2/x0-drop-13-match-temp-in-expr", "front2/x0-static-03-fn-pointers", "front2/x0-int-15-add-wraps-release", "front2/x0-dyn-01-provided-override", "front2/x0-dyn-02-receivers", "front2/x0-dyn-05-drop-in-place", "front2/x0-drop-14-if-let-temp", "front2/x0-box-01-moves", "front2/x0-box-02-call-and-scope", "front2/x0-box-03-partial-local-branch", "front2/x0-box-04-box-moved-in-branch", "front2/x0-dropglue-03-boxed-slice"];

/// front2's own programs (ladder/front2/*/): each one seam of the run path no rung covers.
/// The old ladder's `&dyn` rung (no_core) runs here too: front2's vtables, placed.
/// Out of [`PASSING`] with a reason: `x0-unwind-01` (the ladder does not build with unwinding nor
/// register unwind entries yet; 15:28 it printed WRONG status 69, which must become a named
/// refusal or a pass: writer acc774362254151fd). Not listed at all: the rungs whose `expected`
/// says `panicked ..` (x0-int-12, x0-int-13, x0-int-14): this harness reads only `status <n>`;
/// the front2_rungs example judges them (`rungs::Want::Panicked`). Nor `x1-poll-panic-01`: its
/// `[profile.dev]` says `panic = ..`, which `rungs::x::overflow_of` refuses to read (18:04).
const OWN: &[&str] = &["7b-dyn", "front2/f1-defaults", "front2/f2-generic-self", "front2/f3-dyn", "front2/f4-assoc", "front2/f5-closures", "front2/f6-literals", "front2/x-niche-01", "front2/x0-fpat-01", "front2/x0-dropglue-01", "front2/x0-reach-01", "front2/x0-static-01", "front2/x0-static-02-late-body", "front2/x0-flit-01", "front2/x0-extern-local-01", "front2/x0-async-01", "front2/x0-dropglue-02-slice", "front2/x0-ref-11-deref-unsized", "front2/x0-atomic-01-orderings", "front2/x0-int-11-three-way", "front2/x0-ref-13-slice-get-unchecked", "front2/x0-ref-12-wide-ptr-eq", "front2/x0-ref-11t-deref-unsized", "front2/x0-ref-12t-wide-ptr-eq","front2/x0-const-assoc-01", "front2/x0-const-assoc-02-order", "front2/x0-const-block-01", "front2/x0-adt-09-discriminant-high-bit", "front2/x0-adt-10-packed", "front2/x0-simd-01-layout", "front2/x0-simd-02-lanes", "front2/x0-unwind-01", "front2/x0-op-01-projection-operand", "front2/x0-static-03-fn-pointers", "front2/x0-static-04-generic-field", "front2/x0-move-11-field-of-temp", "front2/x0-move-12-match-on-deref", "front2/x0-drop-11-zst-temporary", "front2/x0-drop-12-zst-order", "front2/x0-drop-12-let-else-temp", "front2/x0-drop-13-match-temp-in-expr", "front2/x-unsize-01-pin-box-async", "front2/x0-int-15-add-wraps-release", "front2/x1-waker-01", "front2/x1-cancel-01", "front2/x1-runtime-01", "front2/x0-async-trait-01", "front2/x0-async-02-hidden-coroutine", "front2/x0-dyn-01-provided-override", "front2/x0-dyn-02-receivers", "front2/x0-dyn-03-uncalled-method", "front2/x0-dyn-04-impl-for-object", "front2/x0-dyn-05-drop-in-place", "front2/x0-drop-14-if-let-temp", "front2/x0-tls-threads", "front2/x0-box-01-moves", "front2/x0-box-02-call-and-scope", "front2/x0-box-03-partial-local-branch", "front2/x0-box-04-box-moved-in-branch", "front2/x0-dropglue-03-boxed-slice", "front2/x0-static-05-interior-mut"];

fn capture(f: impl FnOnce() -> u8) -> (u8, Vec<u8>) {
    let (read_owned, write_owned) = blaze_sys::posix::io::pipe().expect("a pipe");
    let read_end = read_owned.into_raw();
    let write_end = write_owned.into_raw();
    let saved = blaze_sys::posix::io::dup(blaze_sys::Fd::new(1)).expect("a copy of fd 1").into_raw();
    blaze_sys::posix::io::dup2(blaze_sys::Fd::new(write_end), blaze_sys::Fd::new(1)).expect("fd 1 onto the pipe");
    let status = f();
    blaze_sys::posix::io::dup2(blaze_sys::Fd::new(saved), blaze_sys::Fd::new(1)).expect("fd 1 back");
    unsafe {
        let _ = blaze_sys::posix::io::close(blaze_sys::posix::io::OwnedFd::from_raw(saved));
        let _ = blaze_sys::posix::io::close(blaze_sys::posix::io::OwnedFd::from_raw(write_end));
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = blaze_sys::posix::io::read(blaze_sys::Fd::new(read_end), &mut buf).map_or(-1isize, |n| n as isize);
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        let _ = blaze_sys::posix::io::close(blaze_sys::posix::io::OwnedFd::from_raw(read_end));
        (status, out)
    }
}

/// The edition line of a rung's `Cargo.toml`.
fn edition_of(dir: &std::path::Path) -> String {
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).expect("a rung's Cargo.toml");
    let line = manifest.lines().find_map(|l| l.trim().strip_prefix("edition")).expect("an edition line");
    line.trim().trim_start_matches('=').trim().trim_matches('"').to_string()
}

#[test]
fn every_rung_through_front2() {
    let mut lines = Vec::new();
    let mut regressed = Vec::new();
    for rung in RUNNING.iter().chain(SUPPORT).chain(OWN) {
        let dir = ladder().join(rung);
        let files = sources(&dir);
        let views: Vec<SourceFile<'_>> = files.iter().map(|(path, text)| SourceFile { path, text }).collect();
        let edition = edition_of(&dir);
        // A blaze edition builds against the real libraries: the toolchain's shelf.
        let shelf = edition.starts_with("blaze-").then(|| format!("{}/.blaze/v2/toolchain", std::env::var("HOME").expect("HOME")));
        let choice = FrontChoice::New(NewFront { package: "ladder", edition: &edition, shelf: shelf.as_deref(), overflow: rungs::x::overflow_of(&dir) });
        let mut failed = None;
        let (status, out) = capture(|| match run_with(&views, "src/main.rs", None, choice, blaze_own::testing::heap().shared()) {
            Ok(status) => status,
            Err(error) => {
                failed = Some(error.to_string());
                u8::MAX
            }
        });
        let want = expected(&dir);
        let line = match failed {
            Some(error) => format!("{rung}: REFUSED: {error}"),
            None if (status, &out) == (want.0, &want.1) => format!("{rung}: PASS (status {status}, stdout byte-equal)"),
            None => format!("{rung}: WRONG: status {status}, stdout {:?}", String::from_utf8_lossy(&out)),
        };
        if PASSING.contains(rung) && !line.contains(": PASS") {
            regressed.push(line.clone());
        }
        lines.push(line);
    }
    let shown = format!("FRONT2 RUNGS:\n{}\n", lines.join("\n"));
    println!("{shown}");
    // The checker scripts keep no test output: `FRONT2_RUNGS_LIST` names a file for the lines.
    if let Some(path) = std::env::var_os("FRONT2_RUNGS_LIST") {
        std::fs::write(&path, &shown).expect("writing FRONT2_RUNGS_LIST");
    }
    assert!(regressed.is_empty(), "rungs that passed through front2 no longer do:\n{}", regressed.join("\n"));
}
