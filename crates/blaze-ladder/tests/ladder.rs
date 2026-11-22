//! The ladder's rungs (crates/blaze-cli/tests/ladder/*/), each run in this test's process
//! through `blaze_ladder::run::run`, its status and standard output compared with its
//! `expected` file. The verdict on a rung is `ladderrun.sh` running the real `blaze run`
//! (README.md); this test keeps a rung that ran from going backwards. It is this binary's
//! only test: it points fd 1 at a pipe, which a test beside it would write into.

#![feature(allocator_api)]

mod rungs;

use blaze_ladder::refusal::SourceFile;
use rungs::{RUNNING, SUPPORT, expected, ladder, sources};

/// Runs `f` with this process's standard output (fd 1) going to a pipe; gives what was written.
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

#[test]
fn every_running_rung_ends_as_expected() {
    let mut failures = Vec::new();
    for rung in RUNNING.iter().chain(SUPPORT) {
        let dir = ladder().join(rung);
        let files = sources(&dir);
        let views: Vec<SourceFile<'_>> = files.iter().map(|(path, text)| SourceFile { path, text }).collect();
        // The error is shown after fd 1 is back: a panic while it is the pipe loses its words.
        let mut failed = None;
        let overflow = rungs::x::overflow_of(&dir);
        let (status, out) = capture(|| match blaze_ladder::run::run(&views, "src/main.rs", None, overflow, blaze_own::testing::heap().shared()) {
            Ok(status) => status,
            Err(error) => {
                failed = Some(error.to_string());
                u8::MAX
            }
        });
        // Every program is run and each that fails is named, so one run shows them all.
        match failed {
            Some(error) => failures.push(format!("{rung}: {error}")),
            None if (status, &out) != (expected(&dir).0, &expected(&dir).1) => {
                failures.push(format!("{rung}: status {status}, stdout {:?}", String::from_utf8_lossy(&out)));
            }
            None => {}
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
