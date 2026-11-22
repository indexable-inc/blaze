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

use blaze_build::OverflowChecks;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, RunError};

use super::{Want, sources, wanted};

/// The package name every rung is built as.
pub const PACKAGE: &str = "ladder";

/// The root file of every rung.
pub const ROOT: &str = "src/main.rs";

/// What one rung did.
pub enum Verdict {
    Pass,
    Refused { class: String, first: String },
    Panicked { first: String },
    /// `raised`: the program panicked, stopped at the raise by this fn (D91).
    Wrong { status: u8, raised: Option<String>, out: Vec<u8> },
}

/// Runs `f` with fd 1 on a pipe; its status and what it wrote there. As ladder_front2.rs's.
pub fn capture(f: impl FnOnce() -> u8) -> (u8, Vec<u8>) {
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
pub fn edition_of(dir: &Path) -> String {
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).expect("a rung's Cargo.toml");
    let line = manifest.lines().find_map(|l| l.trim().strip_prefix("edition")).expect("an edition line");
    line.trim().trim_start_matches('=').trim().trim_matches('"').to_string()
}

/// The rung's overflow setting as `blaze run` takes it (D101): the `dev` profile's, on unless
/// its `Cargo.toml` has `overflow-checks = false` (or `true`) under `[profile.dev]`. Any other
/// line under `[profile.dev]` stops the rung by name: the harness reads only this key.
pub fn overflow_of(dir: &Path) -> OverflowChecks {
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).expect("a rung's Cargo.toml");
    let mut in_dev = false;
    let mut setting = OverflowChecks::On;
    for line in manifest.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if line.starts_with('[') {
            in_dev = line == "[profile.dev]";
            continue;
        }
        if !in_dev {
            continue;
        }
        let (key, value) = line.split_once('=').unwrap_or_else(|| panic!("{}: `{line}` under [profile.dev] is no key", dir.display()));
        assert_eq!(key.trim(), "overflow-checks", "{}: the rung harness reads only `overflow-checks` under [profile.dev]", dir.display());
        setting = match value.trim() {
            "true" => OverflowChecks::On,
            "false" => OverflowChecks::Off,
            other => panic!("{}: `overflow-checks = {other}` is no boolean", dir.display()),
        };
    }
    setting
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

/// Every WHOLE std rung's directory name (`s<digit>-*`: std programs, run by the example
/// runner's census when its prefix starts with `s`), sorted; a folder not whole yet is named on
/// standard error and left out, as in [`x_rungs`].
#[allow(dead_code)]
pub fn std_rungs(front2: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(front2).expect("the ladder's front2 directory") {
        let entry = entry.expect("a directory entry");
        let name = entry.file_name().into_string().expect("a UTF-8 rung name");
        let dir = entry.path();
        let std_rung = name.len() > 2 && name.as_bytes()[0] == b's' && name.as_bytes()[1].is_ascii_digit() && name.as_bytes()[2] == b'-';
        if !std_rung || !dir.is_dir() {
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
    let new = NewFront { package: PACKAGE, edition: &edition, shelf: if on_shelf(&edition) { shelf } else { None }, overflow: overflow_of(dir) };
    let mut refused = None;
    let mut panicked = None;
    // The program panicked and was stopped at the raise (D91): which fn raised, and the
    // refusal it is when `expected` wants a status.
    let mut raised: Option<(String, (String, String))> = None;
    let (status, out) = capture(|| {
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&views, new)));
        match ran {
            Ok(Ok(status)) => status,
            Ok(Err(error @ RunError::UnwindRaised { .. })) => {
                let RunError::UnwindRaised { function } = &error else { unreachable!("matched above") };
                raised = Some((function.path.to_string(), (class_of(&format!("{error:?}")), first_line(&error.to_string()))));
                u8::MAX
            }
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
    let (want, want_out) = wanted(dir);
    let ended = match (&want, &raised) {
        (Want::Status(s), None) => *s == status,
        (Want::Panicked(f), Some((by, _))) => f == by,
        _ => false,
    };
    if ended && out == want_out {
        return Verdict::Pass;
    }
    match raised {
        // A panic where a status was wanted stays the stop it is.
        Some((_, (class, first))) if matches!(want, Want::Status(_)) => Verdict::Refused { class, first },
        raised => Verdict::Wrong { status, raised: raised.map(|(by, _)| by), out },
    }
}

/// The verdict's word and its rest (no newline in either): what follows the rung's name.
pub fn verdict_line(dir: &Path, verdict: Verdict) -> (&'static str, String) {
    match verdict {
        Verdict::Pass => ("PASS", String::new()),
        Verdict::Refused { class, first } => ("REFUSED", format!("{class}: {first}")),
        Verdict::Panicked { first } => ("PANICKED", first),
        Verdict::Wrong { status, raised, out } => {
            let (want, want_out) = wanted(dir);
            let got = match raised {
                Some(by) => format!("panicked {by}"),
                None => format!("status {status}"),
            };
            let rest = format!(
                "got {got} stdout {:?} want {want} stdout {:?}",
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&want_out)
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
