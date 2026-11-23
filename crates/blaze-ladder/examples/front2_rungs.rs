//! The EXECUTION RUNGS' runner: every `front2/x-*` and `x0-*` rung (tests/rungs/x.rs says what
//! a rung and its line are) built and run through the new front end and code builder, the
//! table printed a line at a time. A release build, run by the integrator:
//!
//! `mixcheck.sh blaze-ladder run front2_rungs <ladder folder> <out file> [<prefix>] [<seconds>]`
//!
//! - `<ladder folder>`: `crates/blaze-cli/tests/ladder` (its `front2` holds the rungs);
//! - `<out file>`: started over with the first line, then each line appended as its rung ends
//!   (a run cut short keeps every finished line);
//! - `<prefix>`: only the rungs whose name starts with it (`-` or nothing: all), so one rung
//!   can be timed alone;
//! - `<seconds>`: each rung's deadline (default 60).
//!
//! THE PARENT resolves ONCE what the rungs share: the toolchain's packs read and verified and
//! every toolchain crate's names (`blaze_ladder::crates::library`: 26 to 32 s in a release
//! build, which every rung paid again, in a debug build, when each resolved it itself), once
//! per edition the selected `x-*` rungs are on (one today). It prints the time that took. An
//! `x0-*` twin is `no_core` and names no toolchain crate: nothing is shared for it.
//!
//! EACH RUNG then runs in a FORKED CHILD, one at a time, in name order: the child inherits the
//! library's names in its memory, resolves the rung's own crate over them, checks, builds,
//! places and calls its `main`, writes its verdict as one line to a pipe and `_exit`s. The
//! parent reads the pipe with the deadline: a child still alive then is killed (`HUNG after
//! <n> s`); one ended by a signal is `CRASHED (signal <n>)`; one that exited with no verdict
//! `CRASHED (exit status <n>, no verdict)`. So a hang or a crash costs one line, not the run,
//! and the lines are in rung order by construction.
//!
//! THE PARENT NEVER RUNS A RUNG ITSELF: running one changes the process (the shared set of
//! prepared macro definitions the rung's names add to, texts kept for the process, placed code
//! and its thread-local key, fd 1 while the program runs). In a child those writes are private
//! to it (the fork's memory is copy-on-write) and gone at its `_exit`, so every rung starts
//! from the same parent state and no rung can change another's answer.
//!
//! The last line counts each word; the exit status is 1 when any rung is WRONG, HUNG or CRASHED
//! (REFUSED is the work list, PANICKED a reported bug: neither fails the run), 2 when the run
//! could not start.
//!
//! `fork`, `pipe`, `poll`, `waitpid` and `kill` are libc's, here only: bstd's process
//! capability starts a program (spawn and exec), and a fork that keeps the parent's memory is
//! what this tool is for. A tool, not blaze's runtime.

#![cfg_attr(test, allow(dead_code, reason = "under the test harness `main` and what only it calls are not called"))]

#[path = "../tests/rungs/mod.rs"]
mod rungs;

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use blaze_ladder::crates::library::Library;
use blaze_ladder::crates::{CratesError, DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, RunError, build_over, run_built, run_with};
use blaze_scope::apart::ApartError;
use rungs::x;

/// Why the run did not start.
enum Stop {
    Usage,
    Seconds { written: String },
    Out { path: PathBuf, error: std::io::Error },
    Toolchain(CratesError),
    StandIn { edition: String, error: RunError },
    Library { edition: String, error: ApartError<'static> },
    Frame(blaze_mem::MemError),
    Threaded,
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stop::Usage => write!(f, "usage: front2_rungs <ladder folder> <out file> [<rung name prefix> | -] [<seconds for each rung>]"),
            Stop::Seconds { written } => write!(f, "the deadline `{written}` is not a whole number of seconds above 0"),
            Stop::Out { path, error } => write!(f, "the out file {}: {error}", path.display()),
            Stop::Toolchain(error) => write!(f, "the toolchain: {error}"),
            Stop::StandIn { edition, error } => write!(f, "the stand-in program of edition `{edition}`: {error}"),
            Stop::Library { edition, error } => write!(f, "the toolchain library's names for edition `{edition}`: {error}"),
            Stop::Frame(error) => write!(f, "the library's frame: {error}"),
            Stop::Threaded => write!(
                f,
                "the process has started a thread before the first fork (pthread_is_threaded_np): a forked child could inherit a lock its holder never releases; the resolve must stay on one thread (`Serial`)"
            ),
        }
    }
}

/// `value` kept until the process ends. The parent's library and each child's own texts are
/// borrowed for `'static` by the names resolved over them; the parent holds its library to its
/// end, and a child ends (`_exit`) at its verdict.
fn kept<T: 'static>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

/// How one child ended.
enum Ended {
    /// It exited having written a whole line: the verdict's word and rest.
    Said(String),
    /// It was alive at the deadline and was killed.
    Hung,
    /// A signal ended it.
    Signal(i32),
    /// It exited (with this status) without a whole line.
    Silent(i32),
}

/// Writes all of `bytes` to `fd`; false when a write failed. Only `write`: async-signal-safe.
fn write_all(fd: i32, bytes: &[u8]) -> bool {
    let mut at = 0;
    while at < bytes.len() {
        // SAFETY: `fd` is the open write end of this child's pipe; the pointer and length are
        // of the live slice `bytes[at..]`.
        let n = blaze_sys::posix::io::write(blaze_sys::Fd::new(fd), &bytes[at..]).map_or(-1, |n| n as isize);
        if n <= 0 {
            return false;
        }
        at += n as usize;
    }
    true
}

/// Reads what `fd` (non-blocking) holds now into `text`; true at end of file.
fn drain(fd: i32, text: &mut Vec<u8>) -> bool {
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: `fd` is the open read end of the pipe; `buf` is a live array of that length.
        let n = blaze_sys::posix::io::read(blaze_sys::Fd::new(fd), &mut buf).map_or(-1, |n| n as isize);
        if n > 0 {
            text.extend_from_slice(&buf[..n as usize]);
        } else {
            // 0: every write end is closed. Below 0: nothing to read now (`EAGAIN`), or an
            // interrupted call: the caller's loop comes back.
            return n == 0;
        }
    }
}

/// `rung(name)` in a forked child, its verdict read from a pipe by `deadline`.
///
/// Rests on: the caller's process has ONE thread at the fork (`main` refuses to start
/// otherwise), so the child inherits no lock held by a thread that does not exist in it, and
/// what `rung` does in the child (allocate, lock, resolve, run code) is as safe as in the
/// parent. Between the fork and `rung` the child makes one `close`; after `rung` only `write`
/// and `_exit`: all async-signal-safe, and no allocation, so the loop itself is sound even
/// forked from a threaded process (the unit test's harness) when `rung` is.
fn in_child(name: &str, deadline: Duration, rung: &mut dyn FnMut(&str) -> (&'static str, String)) -> Ended {
    let (read_owned, write_owned) = blaze_sys::posix::io::pipe().expect("a pipe for the rung's verdict");
    let (read_end, write_end) = (read_owned.into_raw(), write_owned.into_raw());
    // What the parent buffered for standard output is written before the fork (the child never
    // writes it again: it leaves by `_exit`, which flushes nothing).
    std::io::stdout().flush().expect("flushing standard output");
    // SAFETY: the single-thread assumption in this function's doc; both sides are handled
    // below and the child never returns from this function.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
    if pid == 0 {
        // THE CHILD. It must never return into the caller's loop: every path ends in `_exit`,
        // a panic of `rung` included (caught here; where panics abort, the abort's signal is
        // the parent's CRASHED line).
        // SAFETY: `read_end` is this process's copy of an open fd, not used again here.
        let _ = unsafe { blaze_sys::posix::io::close(blaze_sys::posix::io::OwnedFd::from_raw(read_end)) };
        let said = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rung(name)));
        let code = match said {
            Ok((word, rest)) => {
                let sep: &[u8] = if rest.is_empty() { b"" } else { b" " };
                let whole = write_all(write_end, word.as_bytes()) && write_all(write_end, sep) && write_all(write_end, rest.as_bytes()) && write_all(write_end, b"\n");
                if whole { 0 } else { 102 }
            }
            Err(_) => 101,
        };
        // SAFETY: `_exit` ends this process at once: no destructor, no `atexit`, no flush of
        // buffers copied from the parent.
        blaze_sys::posix::signal::exit_now(code)
    }
    // THE PARENT.
    // SAFETY: `write_end` is the parent's copy (the child keeps its own); `read_end` is open:
    // `fcntl` only sets its non-blocking flag, so no read below can block past the deadline.
    let _ = unsafe { blaze_sys::posix::io::close(blaze_sys::posix::io::OwnedFd::from_raw(write_end)) };
    blaze_sys::posix::io::set_nonblocking(blaze_sys::Fd::new(read_end), true).expect("the pipe's non-blocking flag");
    let started = Instant::now();
    let mut text = Vec::new();
    let mut eof = false;
    let ended = loop {
        let mut status = 0i32;
        // SAFETY: `pid` is this process's child, not yet reaped; `status` is a live i32.
        let reaped = blaze_sys::posix::proc::wait_pid(pid, blaze_sys::posix::proc::WaitFlags::NO_HANG).expect("waitpid for the rung's child");
        if let Some((_, waited)) = reaped {
            status = waited.0;
            // Everything it wrote is in the pipe: its write end closed when it ended.
            drain(read_end, &mut text);
            break if blaze_sys::posix::proc::WIFSIGNALED(status) {
                Ended::Signal(blaze_sys::posix::proc::WTERMSIG(status))
            } else {
                match text.iter().position(|&b| b == b'\n') {
                    Some(end) if blaze_sys::posix::proc::WEXITSTATUS(status) == 0 => Ended::Said(String::from_utf8_lossy(&text[..end]).into_owned()),
                    _ => Ended::Silent(blaze_sys::posix::proc::WEXITSTATUS(status)),
                }
            };
        }
        let left = deadline.saturating_sub(started.elapsed());
        if left.is_zero() {
            // SAFETY: `pid` is this process's own unreaped child, so the id cannot have been
            // reused; after SIGKILL the blocking `waitpid` returns once the kernel has ended it.
            blaze_sys::posix::signal::kill(pid, blaze_sys::posix::signal::SIGKILL).expect("kill the rung's child");
            let _ = blaze_sys::posix::proc::wait_pid(pid, blaze_sys::posix::proc::WaitFlags::NONE).expect("waitpid after kill");
            break Ended::Hung;
        }
        // Wait for the pipe (or, once it is at its end, only for time), 50 ms at most, so the
        // child's end and the deadline are both seen promptly.
        let wait = left.min(Duration::from_millis(50)).as_millis().max(1) as i32;
        let timeout = blaze_sys::Timespec::from_duration(Duration::from_millis(wait as u64));
        if eof {
            let _ = blaze_sys::posix::io::poll(&mut [], Some(timeout));
        } else {
            let mut poll = [blaze_sys::posix::io::PollFd::new(
                blaze_sys::Fd::new(read_end),
                blaze_sys::posix::io::PollEvents::IN,
            )];
            let _ = blaze_sys::posix::io::poll(&mut poll, Some(timeout));
        }
        if !eof {
            eof = drain(read_end, &mut text);
        }
    };
    // SAFETY: the parent's read end, not used again.
    let _ = unsafe { blaze_sys::posix::io::close(blaze_sys::posix::io::OwnedFd::from_raw(read_end)) };
    ended
}

/// THE PARENT'S LOOP: each of `names`, in the order given, through `rung` in a forked child
/// with `deadline`; each rung's line (tests/rungs/x.rs `table_line`) given to `line` the moment
/// the rung ends, then the last line (the counts). One child at a time, so the lines are in
/// the order of `names` whatever a child does.
fn table(names: &[String], deadline: Duration, rung: &mut dyn FnMut(&str) -> (&'static str, String), line: &mut dyn FnMut(&str)) -> x::Counts {
    let mut counts = x::Counts::default();
    for name in names {
        let (word, rest) = match in_child(name, deadline, rung) {
            Ended::Said(said) => {
                let (word, rest) = said.split_once(' ').unwrap_or((&said, ""));
                match x::WORDS.iter().find(|w| **w == word) {
                    Some(word) => (*word, rest.to_string()),
                    None => ("CRASHED", format!("(a verdict line with no verdict word: {said:?})")),
                }
            }
            Ended::Hung => ("HUNG", format!("after {} s", deadline.as_secs())),
            Ended::Signal(signal) => ("CRASHED", format!("(signal {signal})")),
            Ended::Silent(status) => ("CRASHED", format!("(exit status {status}, no verdict)")),
        };
        counts.add(word);
        line(&x::table_line(name, word, &rest));
    }
    line(&counts.last_line());
    counts
}

/// The run's exit status: 1 when any rung is WRONG, HUNG or CRASHED.
fn exit_status(counts: &x::Counts) -> i32 {
    i32::from(["WRONG", "HUNG", "CRASHED"].iter().any(|w| counts.of(w) > 0))
}

/// The toolchain library's names for programs of `edition`, kept for the process: resolved
/// under a stand-in program crate of the rungs' package name and that edition (`fn main() {}`;
/// its text is not read by the library's resolve).
fn library_of(tc: &'static Toolchain, edition: &str, _alloc: blaze_own::Alloc<'static>) -> Result<&'static Library<'static>, Stop> {
    let stand_in = [SourceFile { path: x::ROOT, text: "fn main() {}\n" }];
    // The stand-in is never built (only the library's names are read of it): its overflow
    // setting is the dev profile's, as a rung's is unless its manifest says otherwise.
    let new = NewFront { package: x::PACKAGE, edition, shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = kept(ProgramTexts::new(&stand_in, x::ROOT, new, tc, blaze_build::Panic::Unwind).map_err(|error| Stop::StandIn { edition: edition.to_string(), error })?);
    let user: &'static UserCrate<'static> = kept(texts.user());
    let inputs: &'static Inputs<'static> = kept(Inputs::new(user, tc).map_err(Stop::Toolchain)?);
    let crates = kept(inputs.crates());
    let frame: &'static blaze_mem::Frame = kept(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).map_err(Stop::Frame)?);
    let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let library = Library::solve(inputs, crates, bump).map_err(|error| Stop::Library { edition: edition.to_string(), error })?;
    Ok(kept(library))
}

/// One program built and run over `library` (IN A CHILD: see the module doc): `run_with`'s
/// stages, the names resolved for the program's crate alone.
fn run_over(library: &'static Library<'static>, views: &[SourceFile<'_>], new: NewFront<'_>, alloc: blaze_own::SharedAlloc<'static>) -> Result<u8, RunError> {
    let texts = kept(ProgramTexts::new(views, x::ROOT, new, library.toolchain(), blaze_build::Panic::Unwind)?);
    let user: &'static UserCrate<'static> = kept(texts.user());
    let inputs: &'static Inputs<'static> = kept(library.inputs_for(user));
    let crates = inputs.crates();
    let names = library.names(inputs, &crates).map_err(|e| RunError::Names { shown: format!("{e}").into_boxed_str() })?;
    let built = build_over(&names, views, texts, alloc.local())?;
    run_built(built, None, alloc)
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// <pthread.h>, macOS: non-zero once the process has created a thread.
    fn pthread_is_threaded_np() -> core::ffi::c_int;
}

/// Whether this process has ever had a second thread.
#[cfg(target_os = "macos")]
fn threaded() -> bool {
    // SAFETY: takes nothing, reads one flag of the threading library.
    unsafe { pthread_is_threaded_np() != 0 }
}

/// Whether this process has a second thread now. The ladder's one target is macOS
/// (`crates::TRIPLE`); elsewhere the count is the kernel's: `Threads:` of /proc/self/status.
#[cfg(not(target_os = "macos"))]
fn threaded() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    let threads = status.lines().find_map(|l| l.strip_prefix("Threads:")).expect("a Threads line");
    threads.trim() != "1"
}

fn run(heap: &'static blaze_own::Heap) -> Result<i32, Stop> {
    let mut args = std::env::args().skip(1);
    let (Some(ladder), Some(out)) = (args.next(), args.next()) else { return Err(Stop::Usage) };
    let prefix = args.next().filter(|p| p != "-").unwrap_or_default();
    let secs: u64 = match args.next() {
        Some(written) => written.parse::<u64>().ok().filter(|s| *s > 0).ok_or(Stop::Seconds { written })?,
        None => 60,
    };
    let out = PathBuf::from(out);
    let front2 = Path::new(&ladder).join("front2");
    // A prefix starting with `s` asks for the std rungs (`s<digit>-*`), the std census's.
    let listed = if prefix.starts_with('s') { x::std_rungs(&front2) } else { x::x_rungs(&front2) };
    let names: Vec<String> = listed.into_iter().filter(|n| n.starts_with(&prefix)).collect();
    let out_error = |error| Stop::Out { path: out.clone(), error };
    let first = format!("FRONT2 X RUNGS: started, {} rungs, {secs} s each at most", names.len());
    println!("{first}");
    std::fs::write(&out, format!("{first}\n")).map_err(out_error)?;
    let mut file = std::fs::OpenOptions::new().append(true).open(&out).map_err(out_error)?;
    let mut say = |line: &str| {
        println!("{line}");
        // The table is the run's whole product: a line that cannot be kept stops it.
        file.write_all(format!("{line}\n").as_bytes()).unwrap_or_else(|e| panic!("appending to {}: {e}", out.display()));
    };

    // What the rungs share, once: the toolchain, and its library's names for each edition a
    // selected rung on the shelf has.
    let shelf = x::shelf();
    let mut libraries: Vec<(String, &'static Library<'static>)> = Vec::new();
    // The std census (`s` prefix): a std rung is a Rust-edition program over the real `std`, so
    // every selected rung builds against the shelf, whatever its edition.
    let std_census = prefix.starts_with('s');
    let mut editions: Vec<String> = names.iter().map(|n| x::edition_of(&front2.join(n))).filter(|e| std_census || x::on_shelf(e)).collect();
    editions.sort();
    editions.dedup();
    if !editions.is_empty() {
        let started = Instant::now();
        let tc: &'static Toolchain = kept(Toolchain::open(&mut DirShelf::new(shelf.clone())).map_err(Stop::Toolchain)?);
        say(&format!("FRONT2 X RUNGS: the toolchain's packs read in {:.1} s", started.elapsed().as_secs_f64()));
        for edition in editions {
            let started = Instant::now();
            let library = library_of(tc, &edition, heap.alloc())?;
            say(&format!("FRONT2 X RUNGS: the toolchain library's names for edition {edition} resolved once in {:.1} s", started.elapsed().as_secs_f64()));
            libraries.push((edition, library));
        }
    }
    // Every fork below rests on this (`in_child`): the resolve above ran on this thread
    // (`Serial`), and nothing else here starts one.
    if threaded() {
        return Err(Stop::Threaded);
    }

    let mut rung = |name: &str| -> (&'static str, String) {
        let dir = front2.join(name);
        let verdict = x::run_one(&dir, Some(shelf.as_str()), |views, new| {
            let new = if std_census { NewFront { shelf: Some(shelf.as_str()), ..new } } else { new };
            match libraries.iter().find(|(e, _)| e == new.edition) {
            Some((_, library)) if new.shelf.is_some() => run_over(*library, views, new, heap.shared()),
            // A `no_core` twin: no toolchain crate, nothing shared.
            _ => run_with(views, x::ROOT, None, FrontChoice::New(new), heap.shared()),
            }
        });
        x::verdict_line(&dir, verdict)
    };
    let counts = table(&names, Duration::from_secs(secs), &mut rung, &mut say);
    Ok(exit_status(&counts))
}

fn main() {
    match run(blaze_own::Heap::process()) {
        Ok(status) => std::process::exit(status),
        Err(stop) => {
            eprintln!("front2_rungs: {stop}");
            // Standard output too: the checker script shows it whole.
            println!("FRONT2 X RUNGS: did not start: {stop}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parent's loop over three fake rungs: one says PASS, one sleeps past a 1 s deadline,
    /// one aborts. A hang and a crash each cost their own line, the lines are in rung order,
    /// and the run fails. The lines:
    ///
    /// ```text
    /// [core] x-1-passes: PASS
    /// [core] x-2-sleeps: HUNG after 1 s
    /// [core] x-3-aborts: CRASHED (signal 6)
    /// FRONT2 X RUNGS: 3 rungs: 1 PASS, 0 REFUSED, 0 PANICKED, 0 WRONG, 1 HUNG, 1 CRASHED
    /// ```
    ///
    /// Run: `lanecheck.sh blaze-ladder test "--example front2_rungs"`; it prints
    /// `test tests::a_hang_and_a_crash_cost_one_line_each ... ok`. The harness has threads at
    /// these forks: the fake rungs allocate nothing and take no lock in the child (`in_child`'s
    /// doc), which is why they return a static word and an empty rest.
    #[test]
    fn a_hang_and_a_crash_cost_one_line_each() {
        let names: Vec<String> = ["x-1-passes", "x-2-sleeps", "x-3-aborts"].iter().map(|n| n.to_string()).collect();
        let mut rung = |name: &str| -> (&'static str, String) {
            match name {
                "x-1-passes" => ("PASS", String::new()),
                "x-2-sleeps" => {
                    std::thread::sleep(Duration::from_secs(30));
                    ("PASS", String::new())
                }
                _ => std::process::abort(),
            }
        };
        let mut lines: Vec<String> = Vec::new();
        let started = Instant::now();
        let counts = table(&names, Duration::from_secs(1), &mut rung, &mut |line| lines.push(line.to_string()));
        assert_eq!(
            lines,
            [
                "[core] x-1-passes: PASS",
                "[core] x-2-sleeps: HUNG after 1 s",
                "[core] x-3-aborts: CRASHED (signal 6)",
                "FRONT2 X RUNGS: 3 rungs: 1 PASS, 0 REFUSED, 0 PANICKED, 0 WRONG, 1 HUNG, 1 CRASHED",
            ]
        );
        assert_ne!(exit_status(&counts), 0, "a HUNG or CRASHED rung fails the run");
        assert!(started.elapsed() < Duration::from_secs(10), "the sleeping child was killed at its deadline, not waited for");
    }
}
