//! What each process of a hung user-code run is blocked in, read from `/proc` as the run's
//! bound ends it: its state, the system call it is in (with the file its first argument is
//! open on, for a call on a descriptor) and the kernel's wait channel. On 2026-11-10 a build
//! script's worker threads had exited (io_uring refused by its sandbox's filter) and its run
//! hung with nothing saying where: the bound's error now says, for each process, what it
//! waits in.
//!
//! [`stat`], [`syscall`] and [`said`] are pure over the files' text; [`of_program`] is the
//! walk of `/proc`, bounded ([`SCAN_MOST`] processes looked at, [`MOST`] reported, [`WITHIN`]
//! of wall time) and ending in a typed [`Outcome`]: a walk cut short says so, and where there
//! is no `/proc` (macOS, Windows) it says what reads the same there instead of nothing.

use std::{
    fmt::{self, Write as _},
    path::Path,
    time::Duration,
};

/// Entries of `/proc` looked at, at most.
pub const SCAN_MOST: usize = 8192;
/// Processes of the run reported, at most.
pub const MOST: usize = 32;
/// The walk's wall time, at most: it runs on the path that kills a run after minutes of quiet.
pub const WITHIN: Duration = Duration::from_millis(200);

/// The blocking system calls by number, x86_64 and aarch64: what a process that waits is in.
pub const X86_64: &[(i64, &str)] = &[
    (0, "read"),
    (1, "write"),
    (7, "poll"),
    (23, "select"),
    (34, "pause"),
    (35, "nanosleep"),
    (42, "connect"),
    (43, "accept"),
    (44, "sendto"),
    (45, "recvfrom"),
    (46, "sendmsg"),
    (47, "recvmsg"),
    (61, "wait4"),
    (72, "fcntl"),
    (73, "flock"),
    (128, "rt_sigtimedwait"),
    (130, "rt_sigsuspend"),
    (202, "futex"),
    (230, "clock_nanosleep"),
    (232, "epoll_wait"),
    (247, "waitid"),
    (257, "openat"),
    (270, "pselect6"),
    (271, "ppoll"),
    (281, "epoll_pwait"),
    (288, "accept4"),
    (426, "io_uring_enter"),
    (441, "epoll_pwait2"),
    (449, "futex_waitv"),
];
pub const AARCH64: &[(i64, &str)] = &[
    (22, "epoll_pwait"),
    (25, "fcntl"),
    (32, "flock"),
    (56, "openat"),
    (63, "read"),
    (64, "write"),
    (72, "pselect6"),
    (73, "ppoll"),
    (95, "waitid"),
    (98, "futex"),
    (101, "nanosleep"),
    (115, "clock_nanosleep"),
    (133, "rt_sigsuspend"),
    (137, "rt_sigtimedwait"),
    (202, "accept"),
    (203, "connect"),
    (206, "sendto"),
    (207, "recvfrom"),
    (211, "sendmsg"),
    (212, "recvmsg"),
    (242, "accept4"),
    (260, "wait4"),
    (426, "io_uring_enter"),
    (441, "epoll_pwait2"),
    (449, "futex_waitv"),
];

/// This machine's table; empty on an architecture blaze has none for, where a call is said by
/// its number alone.
pub fn names() -> &'static [(i64, &'static str)] {
    if cfg!(target_arch = "x86_64") {
        X86_64
    } else if cfg!(target_arch = "aarch64") {
        AARCH64
    } else {
        &[]
    }
}

/// The calls whose first argument is a descriptor: the file it is open on says what is
/// waited for.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const ON_A_DESCRIPTOR: &[&str] = &[
    "read",
    "write",
    "connect",
    "accept",
    "accept4",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
    "fcntl",
    "flock",
    "epoll_wait",
    "epoll_pwait",
    "epoll_pwait2",
    "io_uring_enter",
];

/// What `/proc/PID/syscall` says a process is in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Syscall {
    /// `running`: on a CPU, in no call.
    Running,
    /// `-1`: blocked outside any system call (a page fault, stopped).
    Outside,
    /// In the call `number`, named when the table has it, with its first argument.
    In { number: i64, name: Option<&'static str>, first: u64 },
    /// The file was not read, and why: the process ended meanwhile, or the kernel refused
    /// (`ptrace_scope`).
    NotRead(String),
}

/// One process of a run, as `/proc` showed it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Blocked {
    pub pid: u32,
    pub parent: u32,
    /// The kernel's name of it (`comm`, 15 bytes of its program's name).
    pub name: String,
    /// The state letter of `/proc/PID/stat`: `R`, `S`, `D`, `Z`, `T`, ...
    pub state: char,
    pub threads: u32,
    pub syscall: Syscall,
    /// What a call on a descriptor is on (`/proc/PID/fd/N`); `None` for any other call and
    /// when the link was not read.
    pub on: Option<String>,
    /// The kernel function it sleeps in (`/proc/PID/wchan`); `None` when it is not sleeping
    /// or the kernel does not say (`0`).
    pub channel: Option<String>,
}

/// What a walk of `/proc` for a run's processes ended as.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The run's processes, parents before children; `cut` says why the list is not whole
    /// when a bound ended the walk.
    Read { processes: Vec<Blocked>, cut: Option<String> },
    /// Nothing was read, and why, with what reads the same on this platform.
    NotRead(String),
}

/// The process, its name, state, parent and thread count, from the text of `/proc/PID/stat`:
/// `4711 (build-script-bu) S 4700 ...`. The name is between the first `(` and the last `)`,
/// as it may hold either.
pub fn stat(text: &str) -> Option<(u32, String, char, u32, u32)> {
    let (open, close) = (text.find('(')?, text.rfind(')')?);
    let pid = text.get(..open)?.trim().parse().ok()?;
    let name = text.get(open + 1..close)?.to_owned(); // alloc: once a process reported
    let mut rest = text.get(close + 1..)?.split_whitespace();
    let state = rest.next()?.chars().next()?;
    let parent = rest.next()?.parse().ok()?;
    // `num_threads` is the stat's field 20: the 16th after the parent.
    let threads = rest.nth(15)?.parse().ok()?;
    Some((pid, name, state, parent, threads))
}

/// What the text of `/proc/PID/syscall` says, with calls named by `table`: `running`, `-1 SP
/// PC`, or `NR ARG0 .. ARG5 SP PC` with the arguments in hex.
pub fn syscall(text: &str, table: &'static [(i64, &'static str)]) -> Syscall {
    let mut parts = text.split_whitespace();
    match parts.next() {
        Some("running") => Syscall::Running,
        Some("-1") => Syscall::Outside,
        Some(number) => match number.parse::<i64>() {
            Ok(number) => {
                let first = parts
                    .next()
                    .and_then(|it| u64::from_str_radix(it.trim_start_matches("0x"), 16).ok())
                    .unwrap_or(0);
                let name = table.iter().find(|it| it.0 == number).map(|it| it.1);
                Syscall::In { number, name, first }
            }
            // alloc: error path
            Err(_) => Syscall::NotRead(format!("its text is not a call's: {:?}", text.trim())),
        },
        None => Syscall::NotRead("it is empty".to_owned()), // alloc: error path
    }
}

/// A state letter in words.
pub fn state_says(state: char) -> &'static str {
    match state {
        'R' => "running or runnable",
        'S' => "sleeping in a wait it can be woken from",
        'D' => "in an uninterruptible wait (disk, a page fault)",
        'Z' => "a zombie: it exited and its parent has not reaped it",
        'T' => "stopped by a signal",
        't' => "stopped by a tracer",
        'I' => "idle",
        'X' | 'x' => "dead",
        _ => "in a state this blaze has no name for",
    }
}

impl fmt::Display for Blocked {
    /// `process 4711 `build-script-bu` (child of 4700, 1 thread): sleeping in a wait it can be
    /// woken from (S), in `read` (system call 0) on pipe:[88213], kernel wait `pipe_read` `.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "process {} `{}` (child of {}, {} thread{}): {} ({})",
            self.pid,
            self.name,
            self.parent,
            self.threads,
            if self.threads == 1 { "" } else { "s" },
            state_says(self.state),
            self.state
        )?;
        match &self.syscall {
            Syscall::Running => write!(f, ", in no system call")?,
            Syscall::Outside => write!(f, ", blocked outside any system call")?,
            Syscall::In { number, name: Some(name), .. } => {
                write!(f, ", in `{name}` (system call {number})")?
            }
            Syscall::In { number, name: None, .. } => write!(f, ", in system call {number}")?,
            Syscall::NotRead(why) => write!(f, ", its system call was not read ({why})")?,
        }
        if let Some(on) = &self.on {
            write!(f, " on {on}")?;
        }
        if let Some(channel) = &self.channel {
            write!(f, ", kernel wait `{channel}`")?;
        }
        Ok(())
    }
}

/// What a stall report says of `outcome`: a line a process, then why the list is cut if it
/// is; or why nothing was read.
pub fn said(outcome: &Outcome) -> String {
    // alloc: error path, a stuck run's report
    let mut out = String::new();
    // Writing to a `String` does not fail.
    match outcome {
        Outcome::NotRead(why) => _ = write!(out, "\n  what its processes were blocked in was not read: {why}"),
        Outcome::Read { processes, cut } => {
            match processes.is_empty() {
                true => out.push_str(
                    "\n  none of its processes was found in /proc as it was killed: they had exited, and what was left was its launcher waiting for them",
                ),
                false => out.push_str("\n  what each of its processes was blocked in as it was killed:"),
            }
            for it in processes {
                _ = write!(out, "\n    {it}");
            }
            if let Some(why) = cut {
                _ = write!(out, "\n    (the list is not whole: {why})");
            }
        }
    }
    out
}

/// The run's processes among `all` (each process `/proc` listed): those `is_root` says run
/// the run's program, and every descendant of one, parents before children, by process id
/// within a parent; at most [`MOST`], with whether more were left out.
pub fn of_run(mut all: Vec<Blocked>, is_root: impl Fn(&Blocked) -> bool) -> (Vec<Blocked>, bool) {
    all.sort_by_key(|it| it.pid);
    // alloc: error path
    let mut chosen: Vec<u32> = all.iter().filter(|it| is_root(it)).map(|it| it.pid).collect();
    // Each pass adds the children of those chosen; a tree is at most as deep as it has
    // processes, so the passes are bounded by `all`'s length.
    for _ in 0..all.len() {
        let before = chosen.len();
        for it in &all {
            if chosen.contains(&it.parent) && !chosen.contains(&it.pid) {
                chosen.push(it.pid);
            }
        }
        if chosen.len() == before {
            break;
        }
    }
    let more = chosen.len() > MOST;
    chosen.truncate(MOST);
    // In the order chosen: roots first, then each generation.
    let mut out = Vec::with_capacity(chosen.len()); // alloc: error path
    for pid in chosen {
        if let Some(at) = all.iter().position(|it| it.pid == pid) {
            out.push(all.swap_remove(at));
        }
    }
    (out, more)
}

/// What each process running `program`, and each process it started, is blocked in now,
/// read within [`WITHIN`].
#[cfg(target_os = "linux")]
pub fn of_program(program: &Path) -> Outcome {
    of_program_within(program, WITHIN)
}

/// [`of_program`] with its wall-time bound given: the walk of `/proc` is cut once it has
/// taken more than `within`, and says so.
#[cfg(target_os = "linux")]
fn of_program_within(program: &Path, within: Duration) -> Outcome {
    let began = std::time::Instant::now();
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(error) => return Outcome::NotRead(format!("/proc: {error}")), // alloc: error path
    };
    let mut all = Vec::new(); // alloc: error path
    let mut roots = Vec::new(); // alloc: error path
    let mut cut = None;
    for (seen, entry) in entries.flatten().enumerate() {
        if seen >= SCAN_MOST {
            // alloc: error path
            cut = Some(format!("/proc lists more than {SCAN_MOST} entries, the most looked at"));
            break;
        }
        if began.elapsed() > within {
            // alloc: error path
            cut = Some(format!(
                "reading /proc took more than {} ms, its bound, with {seen} of its entries looked at",
                within.as_millis()
            ));
            break;
        }
        let Some(pid) = entry.file_name().to_str().and_then(|it| it.parse::<u32>().ok()) else {
            continue;
        };
        let dir = entry.path();
        // A process that ended meanwhile has no files: it is not one the run is blocked in.
        let Some((_, name, state, parent, threads)) =
            std::fs::read_to_string(dir.join("stat")).ok().as_deref().and_then(stat)
        else {
            continue;
        };
        if std::fs::read_link(dir.join("exe")).is_ok_and(|it| it == program) {
            roots.push(pid);
        }
        all.push(Blocked {
            pid,
            parent,
            name,
            state,
            threads,
            syscall: Syscall::Running,
            on: None,
            channel: None,
        });
    }
    let (mut processes, more) = of_run(all, |it| roots.contains(&it.pid));
    if more && cut.is_none() {
        cut = Some(format!("the run has more than {MOST} processes, the most reported")); // alloc: error path
    }
    // The files only the run's own processes are asked for.
    for it in &mut processes {
        let dir = Path::new("/proc").join(it.pid.to_string());
        it.syscall = match std::fs::read_to_string(dir.join("syscall")) {
            Ok(text) => syscall(&text, names()),
            Err(error) => Syscall::NotRead(error.to_string()), // alloc: error path
        };
        if let Syscall::In { name: Some(name), first, .. } = &it.syscall
            && ON_A_DESCRIPTOR.contains(name)
        {
            it.on = std::fs::read_link(dir.join("fd").join(first.to_string()))
                .ok()
                .map(|it| it.display().to_string());
        }
        it.channel = std::fs::read_to_string(dir.join("wchan"))
            .ok()
            .map(|it| it.trim().to_owned())
            .filter(|it| !it.is_empty() && it != "0");
    }
    Outcome::Read { processes, cut }
}

/// Where there is no `/proc`: nothing is read, and the outcome says what reads the same.
#[cfg(not(target_os = "linux"))]
pub fn of_program(program: &Path) -> Outcome {
    let how = if cfg!(target_os = "macos") {
        "macOS has no /proc; a process's state and wait channel are `ps -o pid,ppid,state,wchan,command`, and the stack it is blocked in is `sample PID` (both need no root for one's own processes); blaze does not run them for a run it is killing"
    } else {
        "this platform has no /proc, and blaze reads a process's wait nowhere else yet"
    };
    // alloc: error path
    Outcome::NotRead(format!("{how} (the run's program is {})", program.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `/proc/PID/stat` of a script whose name holds a space and a parenthesis.
    const STAT: &str = "4711 (build (script) S 4700 4711 4699 0 -1 4194304 189 0 0 0 1 0 0 0 20 0 3 0 8812345 2637824 213 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 2 0 0 0 0 0\n";

    fn blocked(pid: u32, parent: u32) -> Blocked {
        Blocked {
            pid,
            parent,
            name: format!("p{pid}"),
            state: 'S',
            threads: 1,
            syscall: Syscall::Running,
            on: None,
            channel: None,
        }
    }

    #[test]
    fn a_stat_gives_the_name_state_parent_and_threads() {
        assert_eq!(stat(STAT), Some((4711, "build (script".to_owned(), 'S', 4700, 3)));
        assert_eq!(stat(""), None);
        assert_eq!(stat("4711 (x) S"), None, "a stat cut short is none, never zeros");
    }

    #[test]
    fn a_syscall_file_is_running_outside_or_a_named_call() {
        assert_eq!(syscall("running\n", X86_64), Syscall::Running);
        assert_eq!(syscall("-1 0x7ffd0 0x7f1c\n", X86_64), Syscall::Outside);
        let read = "0 0x3 0x7ffd1000 0x2000 0x0 0x0 0x0 0x7ffd0 0x7f1c2a0e\n";
        assert_eq!(syscall(read, X86_64), Syscall::In { number: 0, name: Some("read"), first: 3 });
        // The same number is another call on another architecture, and one the table lacks is
        // said by its number.
        assert_eq!(
            syscall("98 0x55d0 0x80 0x0 0x0 0x0 0x0 0x7f 0x7f\n", AARCH64),
            Syscall::In { number: 98, name: Some("futex"), first: 0x55d0 }
        );
        assert_eq!(
            syscall("999 0x1 0x0\n", X86_64),
            Syscall::In { number: 999, name: None, first: 1 }
        );
        assert!(matches!(syscall("what\n", X86_64), Syscall::NotRead(it) if it.contains("what")));
        assert!(matches!(syscall("", X86_64), Syscall::NotRead(_)));
    }

    #[test]
    fn a_runs_processes_are_its_programs_and_their_descendants_parents_first() {
        // 10 runs the program; 11 and 12 are its children, 13 a grandchild; 20 is another's.
        let all = vec![blocked(13, 12), blocked(20, 1), blocked(12, 10), blocked(10, 5), blocked(11, 10)];
        let (run, more) = of_run(all, |it| it.pid == 10);
        assert_eq!(run.iter().map(|it| it.pid).collect::<Vec<_>>(), [10, 11, 12, 13]);
        assert!(!more);
        // Past the bound the list is cut and says so.
        let many: Vec<Blocked> = (0..MOST as u32 + 5).map(|it| blocked(100 + it, 99)).collect();
        let (run, more) = of_run(many, |it| it.parent == 99);
        assert_eq!((run.len(), more), (MOST, true));
        assert_eq!(of_run(Vec::new(), |_| true), (Vec::new(), false));
    }

    #[test]
    fn a_stall_report_says_what_each_process_waits_in() {
        let script = Blocked {
            name: "build-script-bu".to_owned(),
            threads: 1,
            syscall: Syscall::In { number: 0, name: Some("read"), first: 3 },
            on: Some("pipe:[88213]".to_owned()),
            channel: Some("pipe_read".to_owned()),
            ..blocked(4711, 4700)
        };
        let child = Blocked {
            name: "cc".to_owned(),
            state: 'Z',
            threads: 2,
            syscall: Syscall::NotRead("No such process".to_owned()),
            ..blocked(4712, 4711)
        };
        let outcome = Outcome::Read {
            processes: vec![script, child],
            cut: Some("the run has more than 32 processes, the most reported".to_owned()),
        };
        assert_eq!(
            said(&outcome),
            "\n  what each of its processes was blocked in as it was killed:\
             \n    process 4711 `build-script-bu` (child of 4700, 1 thread): sleeping in a wait it can be woken from (S), in `read` (system call 0) on pipe:[88213], kernel wait `pipe_read`\
             \n    process 4712 `cc` (child of 4711, 2 threads): a zombie: it exited and its parent has not reaped it (Z), its system call was not read (No such process)\
             \n    (the list is not whole: the run has more than 32 processes, the most reported)"
        );
        let none = said(&Outcome::Read { processes: Vec::new(), cut: None });
        assert!(none.contains("none of its processes was found"), "{none}");
        let unread = said(&Outcome::NotRead("macOS has no /proc".to_owned()));
        assert!(unread.contains("was not read: macOS has no /proc"), "{unread}");
    }

    /// Where there is a `/proc`, this process is found by its own program and is not cut.
    /// The walk is given a minute, not [`WITHIN`]: `/proc` lists processes oldest first, so
    /// this one is among the last, and a walk cut at 200 ms on a machine busy with other work
    /// ends before it (on 2026-11-11 a run beside a build found none). What the bound does is
    /// the test below's.
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_is_found_by_its_program() {
        let program = std::env::current_exe().unwrap();
        let outcome = of_program_within(&program, Duration::from_secs(60));
        let Outcome::Read { processes, cut } = &outcome else {
            panic!("/proc was not read: {outcome:?}")
        };
        assert_eq!(*cut, None, "the walk was cut");
        // Its pid as `/proc` lists it, which is not `std::process::id()` where `/proc` is of
        // another pid namespace than the process.
        let own: u32 = std::fs::read_link("/proc/self")
            .unwrap()
            .to_str()
            .and_then(|it| it.parse().ok())
            .expect("/proc/self names a pid");
        assert!(
            processes.iter().any(|it| it.pid == own),
            "process {own} (`std::process::id()` says {}) running {} is not among {processes:#?}",
            std::process::id(),
            program.display()
        );
    }

    /// A walk out of time is cut and says so, with what it found until then: never a list
    /// that reads as whole.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_walk_out_of_time_says_it_was_cut() {
        let program = std::env::current_exe().unwrap();
        // No time at all: the first entry's check finds the bound passed.
        let Outcome::Read { cut, .. } = of_program_within(&program, Duration::ZERO) else {
            panic!("/proc was not read")
        };
        let cut = cut.expect("a walk with no time is cut");
        assert!(cut.contains("its bound"), "{cut}");
    }
}
