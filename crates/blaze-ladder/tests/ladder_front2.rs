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
const PASSING: &[&str] = &["1-empty", "2-exit", "3-hello", "4-modules", "5-generics", "6-aggregates", "7-traits", "7a-drops", "7c-main-io", "support/s1-layout", "support/s3-extern-c", "support/s4-intrinsics", "front2/f1-defaults", "front2/f2-generic-self", "7b-dyn", "front2/f4-assoc", "front2/f5-closures", "front2/f6-literals"];

/// front2's own programs (ladder/front2/*/): each one seam of the run path no rung covers.
/// The old ladder's `&dyn` rung (no_core) runs here too: front2's vtables, placed.
const OWN: &[&str] = &["7b-dyn", "front2/f1-defaults", "front2/f2-generic-self", "front2/f3-dyn", "front2/f4-assoc", "front2/f5-closures", "front2/f6-literals", "front2/x-niche-01"];

fn capture(f: impl FnOnce() -> u8) -> (u8, Vec<u8>) {
    let mut fds = [0; 2];
    // SAFETY: as tests/ladder.rs: `pipe` fills the array; fd 1 is restored before returning.
    unsafe {
        assert_eq!(libc::pipe(fds.as_mut_ptr()), 0, "a pipe");
        let saved = libc::dup(1);
        assert!(saved >= 0, "a copy of fd 1");
        assert!(libc::dup2(fds[1], 1) >= 0, "fd 1 onto the pipe");
        let status = f();
        assert!(libc::dup2(saved, 1) >= 0, "fd 1 back");
        libc::close(saved);
        libc::close(fds[1]);
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = libc::read(fds[0], buf.as_mut_ptr().cast(), buf.len());
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        libc::close(fds[0]);
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
        let choice = FrontChoice::New(NewFront { package: "ladder", edition: &edition, shelf: shelf.as_deref() });
        let mut failed = None;
        let (status, out) = capture(|| match run_with(&views, "src/main.rs", None, choice) {
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
