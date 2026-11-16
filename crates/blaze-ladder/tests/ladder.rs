//! The ladder's rungs (crates/blaze-cli/tests/ladder/*/), each run in this test's process
//! through `blaze_ladder::run::run`, its status and standard output compared with its
//! `expected` file. The verdict on a rung is `ladderrun.sh` running the real `blaze run`
//! (README.md); this test keeps a rung that ran from going backwards.

#![feature(allocator_api)]

use std::path::{Path, PathBuf};

use blaze_ladder::lower::SourceFile;

/// The rungs' directory.
fn ladder() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-cli/tests/ladder")
}

/// `expected`'s status and standard output.
fn expected(dir: &Path) -> (u8, Vec<u8>) {
    let text = std::fs::read(dir.join("expected")).expect("a rung's expected file");
    let mut lines = text.splitn(3, |&b| b == b'\n');
    let status = lines.next().and_then(|l| std::str::from_utf8(l).ok()).and_then(|l| l.strip_prefix("status "));
    let status: u8 = status.and_then(|s| s.trim().parse().ok()).expect("line 1: `status <n>`");
    assert_eq!(lines.next(), Some(&b"stdout"[..]), "line 2 of {}: `stdout`", dir.display());
    (status, lines.next().unwrap_or(&[]).to_vec())
}

/// Every `.rs` file under `dir/src`, by path relative to `dir`.
fn sources(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.join("src")];
    while let Some(at) = todo.pop() {
        for entry in std::fs::read_dir(&at).expect("a rung's src directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                todo.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path.strip_prefix(dir).expect("under the rung").to_str().expect("UTF-8").to_owned();
                out.push((rel, std::fs::read_to_string(&path).expect("a UTF-8 source file")));
            }
        }
    }
    out.sort();
    out
}

/// Runs `f` with this process's standard output (fd 1) going to a pipe; gives what was written.
fn capture(f: impl FnOnce() -> u8) -> (u8, Vec<u8>) {
    let mut fds = [0; 2];
    // SAFETY: `pipe` writes two descriptors into the array it is given; `dup`/`dup2` take and
    // give descriptors this function owns; fd 1 is restored before it returns.
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

/// The rungs that run today: the next one is added when it runs.
const RUNNING: &[&str] = &["1-empty"];

#[test]
fn every_running_rung_ends_as_expected() {
    for rung in RUNNING {
        let dir = ladder().join(rung);
        let files = sources(&dir);
        let views: Vec<SourceFile<'_>> = files.iter().map(|(path, text)| SourceFile { path, text }).collect();
        let (status, out) = capture(|| match blaze_ladder::run::run(&views, "src/main.rs") {
            Ok(status) => status,
            Err(error) => panic!("rung {rung}: {error}"),
        });
        assert_eq!((status, out), expected(&dir), "rung {rung}");
    }
}
