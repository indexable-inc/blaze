//! The EXECUTION RUNGS' one home (`crates/blaze-cli/tests/ladder/front2/x-*` and their `x0-*`
//! twins): which folders are rungs, how one is run to its verdict, and how its line and the
//! table's last line are written. Shared by examples/front2_rungs.rs (the runner: the toolchain
//! library resolved once, a forked child per rung) and tests/front2_rungs.rs (a child process
//! per rung, each resolving the library itself; asked for by a variable).
//!
//! A rung's `main` exits with the status its `expected` file states when every check it makes
//! passed (0 for an `x-*` rung on the blaze edition over the real `core`, 42 for an `x0-*`
//! `no_core` twin), and otherwise the number of its first failed check. One line per rung:
//! `[core] x-adt-01-fields: PASS`, the family first (`[twin]` for `x0-*`), then
//! - `PASS`: status and stdout equal `expected`;
//! - `REFUSED <class>: <first line>`: the front end or the builder said by name what it does
//!   not build yet (the work list; not a failure);
//! - `PANICKED <first line>`: the run path panicked (a bug, reported, not a wrong answer);
//! - `WRONG got status <n> stdout <..> want status <n> stdout <..>`: a wrong answer;
//! - `HUNG after <s> s`: the rung's child did not finish by its deadline and was killed;
//! - `CRASHED <how>`: the rung's child ended with no verdict (a signal, an abort).

use std::path::Path;

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, RunError};

use super::{expected, sources};

/// The package name every rung is built as.
pub const PACKAGE: &str = "ladder";

/// The root file of every rung.
pub const ROOT: &str = "src/main.rs";

/// What one rung did.
pub enum Verdict {
    Pass,
    Refused { class: String, first: String },
    Panicked { first: String },
    Wrong { status: u8, out: Vec<u8> },
}

/// Runs `f` with fd 1 on a pipe; its status and what it wrote there. As ladder_front2.rs's.
pub fn capture(f: impl FnOnce() -> u8) -> (u8, Vec<u8>) {
    let mut fds = [0; 2];
    // SAFETY: as tests/ladder.rs: `pipe` fills the array; fd 1 is restored before returning
    // (`f` cannot unwind past here: the caller catches its panics inside `f`).
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
pub fn edition_of(dir: &Path) -> String {
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).expect("a rung's Cargo.toml");
    let line = manifest.lines().find_map(|l| l.trim().strip_prefix("edition")).expect("an edition line");
    line.trim().trim_start_matches('=').trim().trim_matches('"').to_string()
}

/// Whether a package of `edition` builds against the real libraries (the toolchain's shelf):
/// a blaze edition does; a `no_core` twin on a Rust edition names nothing outside itself.
pub fn on_shelf(edition: &str) -> bool {
    edition.starts_with("blaze-")
}

/// The toolchain's shelf: `<home>/.blaze/v2/toolchain`.
pub fn shelf() -> String {
    format!("{}/.blaze/v2/toolchain", std::env::var("HOME").expect("HOME"))
}

/// Every WHOLE `x-*` and `x0-*` rung's directory name under `front2` (the ladder's `front2`
/// folder), sorted. A folder still being written (no `Cargo.toml`, `expected` or `src/main.rs`
/// yet) is not a rung yet: its name goes to standard error and it is left out.
pub fn x_rungs(front2: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(front2).expect("the ladder's front2 directory") {
        let entry = entry.expect("a directory entry");
        let name = entry.file_name().into_string().expect("a UTF-8 rung name");
        let dir = entry.path();
        if !(name.starts_with("x-") || name.starts_with("x0-")) || !dir.is_dir() {
            continue;
        }
        if ["Cargo.toml", "expected", "src/main.rs"].iter().all(|f| dir.join(f).is_file()) {
            names.push(name);
        } else {
            eprintln!("front2_rungs: {name} is not whole yet (Cargo.toml, expected, src/main.rs): left out");
        }
    }
    names.sort();
    names
}

/// `x0-*` are the runnable twins (no_core, edition 2024, success is status 42); `x-*` the same
/// checks on the blaze edition over the real `core` (success is 0).
pub fn family(rung: &str) -> &'static str {
    if rung.starts_with("x0-") { "twin" } else { "core" }
}

/// The variant's name at the head of a `Debug` rendering: `Lower { .. }` gives `Lower`.
pub fn class_of(debug: &str) -> String {
    debug.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect()
}

pub fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").to_string()
}

/// The rung in `dir` run to its verdict in this process: `run` is given its files and its
/// package (name, edition; `shelf` when it builds against the real libraries) and builds and
/// runs it (`run_with`, or the runner's own path over a library resolved once); its status and
/// what it wrote to fd 1 are judged against `expected`.
pub fn run_one(dir: &Path, shelf: Option<&str>, run: impl FnOnce(&[SourceFile<'_>], NewFront<'_>) -> Result<u8, RunError>) -> Verdict {
    let files = sources(dir);
    let views: Vec<SourceFile<'_>> = files.iter().map(|(path, text)| SourceFile { path, text }).collect();
    let edition = edition_of(dir);
    let new = NewFront { package: PACKAGE, edition: &edition, shelf: if on_shelf(&edition) { shelf } else { None } };
    let mut refused = None;
    let mut panicked = None;
    let (status, out) = capture(|| {
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&views, new)));
        match ran {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                refused = Some((class_of(&format!("{error:?}")), first_line(&error.to_string())));
                u8::MAX
            }
            Err(payload) => {
                let said = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "a panic with no message".to_string());
                panicked = Some(first_line(&said));
                u8::MAX
            }
        }
    });
    if let Some((class, first)) = refused {
        return Verdict::Refused { class, first };
    }
    if let Some(first) = panicked {
        return Verdict::Panicked { first };
    }
    let want = expected(dir);
    if (status, &out) == (want.0, &want.1) { Verdict::Pass } else { Verdict::Wrong { status, out } }
}

/// The verdict's word and its rest (no newline in either): what follows the rung's name.
pub fn verdict_line(dir: &Path, verdict: Verdict) -> (&'static str, String) {
    match verdict {
        Verdict::Pass => ("PASS", String::new()),
        Verdict::Refused { class, first } => ("REFUSED", format!("{class}: {first}")),
        Verdict::Panicked { first } => ("PANICKED", first),
        Verdict::Wrong { status, out } => {
            let want = expected(dir);
            let rest = format!(
                "got status {status} stdout {:?} want status {} stdout {:?}",
                String::from_utf8_lossy(&out),
                want.0,
                String::from_utf8_lossy(&want.1)
            );
            ("WRONG", rest)
        }
    }
}

/// One rung's line of the table: `[core] x-adt-01-fields: PASS`.
pub fn table_line(rung: &str, word: &str, rest: &str) -> String {
    format!("[{}] {rung}: {word} {rest}", family(rung)).trim_end().to_string()
}

/// The words a line's verdict can be, in the order the last line counts them.
pub const WORDS: [&str; 6] = ["PASS", "REFUSED", "PANICKED", "WRONG", "HUNG", "CRASHED"];

/// How many rungs ended with each word of [`WORDS`].
#[derive(Default)]
pub struct Counts(pub [u32; 6]);

impl Counts {
    /// Counts one rung that ended with `word`; a word outside [`WORDS`] is a bug of the caller.
    pub fn add(&mut self, word: &str) {
        let at = WORDS.iter().position(|w| *w == word).unwrap_or_else(|| panic!("`{word}` is no verdict word"));
        self.0[at] += 1;
    }

    pub fn of(&self, word: &str) -> u32 {
        WORDS.iter().position(|w| *w == word).map_or(0, |at| self.0[at])
    }

    /// The table's last line: `FRONT2 X RUNGS: 3 rungs: 1 PASS, 0 REFUSED, 0 PANICKED, 0 WRONG, 1 HUNG, 1 CRASHED`.
    pub fn last_line(&self) -> String {
        let total: u32 = self.0.iter().sum();
        let each: Vec<String> = WORDS.iter().zip(self.0).map(|(w, n)| format!("{n} {w}")).collect();
        format!("FRONT2 X RUNGS: {total} rungs: {}", each.join(", "))
    }
}
