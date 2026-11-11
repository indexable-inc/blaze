//! Who made a call a sandbox can't trace, and which programs a run started: what a refusal
//! names beside the effect ("read what can't be traced (Random)" says what, never who), and
//! what `blaze explain script UNIT` lists of a run.
//!
//! A sandbox's supervisor sees each call it is notified of with the process that made it. For
//! the first call of each kind a process makes that can't be traced (randomness, a realtime
//! clock, the network) and for each program the run starts, it describes the caller as it is at
//! that moment ([`Call`]): its program and `argv[0]`, its process id as the supervisor and as
//! the sandbox number it, the system call, where its instruction pointer is
//! ([`module_at`] over the process's `/proc/PID/maps`) and how far the process had come
//! ([`Phase`], a heuristic that says it is one). At most [`MOST`] calls and [`EXECS_MOST`]
//! programs a run are described; [`Call::More`] counts the rest, so a list cut short says so.
//!
//! The launcher leaves them in [`FILE`] beside its report, a line each ([`encode`],
//! [`decode`]). They are no part of a [`blaze_core::Traced`]: no key is made of a process id
//! or an address. The driver keeps the last run's by program ([`left`], [`of`]) for the error
//! that refuses the run and for the events the daemon writes of it.
//!
//! Linux fills all of it. macOS's interposer says what was read, not by which process: its
//! runs leave no calls, and a refusal there says so ([`none_said`]).

use std::{
    fmt,
    path::{Path, PathBuf},
};

/// The file a launcher leaves its run's [`Call`]s in, beside its report.
pub const FILE: &str = "calls";

/// The calls a run can't be traced for that are described, at most.
pub const MOST: usize = 32;
/// The programs a run started that are described, at most.
pub const EXECS_MOST: usize = 32;

/// What a [`Made`] of a program started says in [`Made::what`].
pub const EXEC: &str = "exec";

/// The messages of the events the daemon says of a sandboxed build script's run (`blaze`
/// target, rows of the profile's `events` with their fields in `event_fields`), which `blaze
/// explain script UNIT` reads by: the run (what it read, counted and named), each call its
/// sandbox described (a [`Made`], field by field), and how many it did not describe.
pub const EVENT_RUN: &str = "script run traced";
pub const EVENT_CALL: &str = "script call";
pub const EVENT_CUT: &str = "script calls not described";

/// How far a process had come when it made a call. A heuristic: the supervisor sees system
/// calls, not `main`. What it knows is where the call came from (the dynamic loader runs
/// before anything else) and whether the sandbox's library, which the loader loads with the
/// program's other libraries, had said it loaded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// The call came from the dynamic loader: before `main`.
    Loader,
    /// The sandbox's library had not said it loaded: the loader was still loading the
    /// program's libraries, so before `main`. Known only of a run whose processes load it.
    Loading,
    /// The program's libraries were loaded: a library's constructor or `main`, which the
    /// supervisor can't tell apart.
    Loaded,
    /// The run's processes load no library of the sandbox's and the call came from outside the
    /// loader: nothing says how far the process had come.
    NotKnown,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Loader => "loader",
            Phase::Loading => "loading",
            Phase::Loaded => "loaded",
            Phase::NotKnown => "not known",
        }
    }

    pub fn of(name: &str) -> Option<Phase> {
        Some(match name {
            "loader" => Phase::Loader,
            "loading" => Phase::Loading,
            "loaded" => Phase::Loaded,
            "not known" => Phase::NotKnown,
            _ => return None,
        })
    }

    /// Whether it was before the program's `main`, where the heuristic says: `None` where it
    /// can't.
    pub fn before_main(self) -> Option<bool> {
        match self {
            Phase::Loader | Phase::Loading => Some(true),
            Phase::Loaded | Phase::NotKnown => None,
        }
    }

    /// The phase in words, naming the heuristic.
    pub fn says(self) -> &'static str {
        match self {
            Phase::Loader => {
                "before the program's `main` (a heuristic: the call came from the dynamic loader)"
            }
            Phase::Loading => {
                "before the program's `main` (a heuristic: the loader had not finished loading its libraries, the sandbox's own had not said it loaded)"
            }
            Phase::Loaded => {
                "after its libraries were loaded: in a library's constructor or in `main`, which the sandbox can't tell apart"
            }
            Phase::NotKnown => {
                "before or after the program's `main`: not known (this run's processes load no library of the sandbox's, which is what says)"
            }
        }
    }
}

/// What the code at a call's instruction pointer is part of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Module {
    /// The dynamic loader (`ld-linux-x86-64.so.2`, `ld-musl-*.so.1`).
    Loader,
    /// The C library.
    Libc,
    /// The program's own image.
    Program,
    /// The sandbox's library, which reads a clock or a variable for the program's code.
    Interposer,
    /// The kernel's vDSO.
    Vdso,
    /// Another mapped file: a shared library.
    Library,
    /// Memory no file is mapped at (code made at run time).
    Anonymous,
    /// The process's maps were not read, or hold no such address.
    NotKnown,
}

impl Module {
    pub fn name(self) -> &'static str {
        match self {
            Module::Loader => "the dynamic loader",
            Module::Libc => "libc",
            Module::Program => "the program",
            Module::Interposer => "the sandbox's library",
            Module::Vdso => "the vDSO",
            Module::Library => "a library",
            Module::Anonymous => "memory of no file",
            Module::NotKnown => "not known",
        }
    }

    pub fn of(name: &str) -> Option<Module> {
        [
            Module::Loader,
            Module::Libc,
            Module::Program,
            Module::Interposer,
            Module::Vdso,
            Module::Library,
            Module::Anonymous,
            Module::NotKnown,
        ]
        .into_iter()
        .find(|it| it.name() == name)
    }

    /// What the mapping named `path` (as `/proc/PID/maps` names it; empty for memory of no
    /// file) is, in a process running `program`.
    pub fn of_mapping(path: &str, program: &str) -> Module {
        let file = path.rsplit('/').next().unwrap_or(path);
        if path.is_empty() {
            Module::Anonymous
        } else if path == "[vdso]" || path == "[vsyscall]" {
            Module::Vdso
        } else if path.starts_with('[') {
            // `[heap]`, `[stack]`, `[anon:...]`: memory, not a file.
            Module::Anonymous
        } else if path == program {
            Module::Program
        } else if file.starts_with("ld-") || file.starts_with("ld.so") || file.starts_with("ld64.so")
        {
            Module::Loader
        } else if file.starts_with("libc.so") || file.starts_with("libc-") {
            Module::Libc
        } else if file.contains("blaze_sandbox_interpose") {
            Module::Interposer
        } else {
            Module::Library
        }
    }
}

/// A process as it was when it made a call the sandbox describes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Made {
    /// What the call got: `random`, `clock`, `network` ([`crate::trace::untraceable_name`]), or
    /// [`EXEC`] for a program started.
    pub what: String,
    /// The process, as the supervisor numbers it: what `/proc` on the machine names.
    pub pid: u32,
    /// The process as the sandbox's own PID namespace numbers it (what the program's
    /// `getpid` gives); `None` when `/proc` did not say.
    pub inside_pid: Option<u32>,
    /// The program the process was running (`/proc/PID/exe`); for [`EXEC`], the program it
    /// starts. Empty when it could not be read.
    pub program: String,
    /// Its `argv[0]` (the first of `/proc/PID/cmdline`); for [`EXEC`], of the process that
    /// starts the program. Empty when it could not be read.
    pub argv0: String,
    /// The system call by name (`getrandom`), or `system call N` for one this blaze has no
    /// name for.
    pub syscall: String,
    pub number: i64,
    pub phase: Phase,
    /// Where the process was: the address after the system call's instruction.
    pub ip: u64,
    /// The mapping `ip` is in, as `/proc/PID/maps` names it; empty when not known or of no
    /// file.
    pub module: String,
    pub kind: Module,
}

/// One line of a run's [`FILE`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Call {
    Made(Made),
    /// Calls or programs past [`MOST`] or [`EXECS_MOST`] that were not described, counted.
    More(u64),
}

/// `text` with what a line's fields can't hold (a tab, a line break) as spaces.
fn field(text: &str) -> std::borrow::Cow<'_, str> {
    match text.contains(['\t', '\n', '\r']) {
        false => std::borrow::Cow::Borrowed(text),
        // alloc: a name with a tab or a line break in it, once a call described
        true => std::borrow::Cow::Owned(text.replace(['\t', '\n', '\r'], " ")),
    }
}

/// The calls as [`FILE`]'s text: a line a call, its fields by tabs, in the order given.
pub fn encode(calls: &[Call]) -> String {
    use fmt::Write as _;
    // alloc: once a run, as it ends: the file's text
    let mut out = String::with_capacity(calls.len() * 160);
    for call in calls {
        // Writing to a `String` does not fail.
        _ = match call {
            Call::More(n) => writeln!(out, "more\t{n}"),
            Call::Made(it) => writeln!(
                out,
                "made\t{}\t{}\t{}\t{}\t{}\t{}\t{:x}\t{}\t{}\t{}\t{}",
                field(&it.what),
                it.pid,
                it.inside_pid.map_or(-1, i64::from),
                it.number,
                field(&it.syscall),
                it.phase.name(),
                it.ip,
                it.kind.name(),
                field(&it.module),
                field(&it.program),
                field(&it.argv0),
            ),
        };
    }
    out
}

/// The calls `text` holds, or why it is not a run's [`FILE`], naming the line.
pub fn decode(text: &str) -> Result<Vec<Call>, String> {
    // alloc: once a run, as the driver reads what its launcher left
    let mut out = Vec::with_capacity(text.len() / 96);
    for (at, line) in text.lines().enumerate() {
        // alloc: error path
        let bad = |what: &str| format!("line {} is not a call's ({what}): {line:?}", at + 1);
        let mut parts = line.split('\t');
        match parts.next() {
            Some("more") => {
                let n = parts.next().and_then(|it| it.parse().ok()).ok_or_else(|| bad("its count"))?;
                out.push(Call::More(n));
            }
            Some("made") => {
                let mut next = |what: &str| parts.next().ok_or_else(|| bad(what));
                // alloc: a call's fields, once a call described
                let what = next("what it got")?.to_owned();
                let pid = next("its process")?.parse().map_err(|_| bad("its process"))?;
                let inside: i64 =
                    next("its process inside")?.parse().map_err(|_| bad("its process inside"))?;
                let number = next("its number")?.parse().map_err(|_| bad("its number"))?;
                let syscall = next("its name")?.to_owned(); // alloc: as above
                let phase = Phase::of(next("its phase")?).ok_or_else(|| bad("its phase"))?;
                let ip = u64::from_str_radix(next("its address")?, 16)
                    .map_err(|_| bad("its address"))?;
                let kind = Module::of(next("its module's kind")?)
                    .ok_or_else(|| bad("its module's kind"))?;
                let module = next("its module")?.to_owned(); // alloc: as above
                let program = next("its program")?.to_owned(); // alloc: as above
                let argv0 = next("its argv[0]")?.to_owned(); // alloc: as above
                out.push(Call::Made(Made {
                    what,
                    pid,
                    inside_pid: u32::try_from(inside).ok(),
                    program,
                    argv0,
                    syscall,
                    number,
                    phase,
                    ip,
                    module,
                    kind,
                }));
            }
            _ => return Err(bad("neither `made` nor `more`")),
        }
    }
    Ok(out)
}

/// The calls the launcher left in `scratch`, none when it left no [`FILE`] (a run that does
/// not trace, a sandbox that describes none); an error names the file and its line.
pub fn read(scratch: &Path) -> Result<Vec<Call>, String> {
    let file = scratch.join(FILE);
    match std::fs::read_to_string(&file) {
        Ok(text) => decode(&text).map_err(|why| format!("{}: {why}", file.display())), // alloc: error path
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("{}: {error}", file.display())), // alloc: error path
    }
}

/// The mapping of `maps` (the text of `/proc/PID/maps`) that holds `ip`: its path, empty for
/// memory of no file; `None` when no mapping holds it.
pub fn module_at(maps: &str, ip: u64) -> Option<&str> {
    maps.lines().find_map(|line| {
        // `7f1c2a000000-7f1c2a1c0000 r-xp 00028000 08:01 1234   /usr/lib/libc.so.6`
        let mut parts = line.splitn(6, ' ');
        let (from, to) = parts.next()?.split_once('-')?;
        let from = u64::from_str_radix(from, 16).ok()?;
        let to = u64::from_str_radix(to, 16).ok()?;
        if !(from..to).contains(&ip) {
            return None;
        }
        // Permissions, offset, device and inode, then the path after its padding.
        Some(parts.nth(4).map_or("", str::trim))
    })
}

/// The process's id in the innermost PID namespace it is in, from the text of
/// `/proc/PID/status`: the last number of its `NSpid:` line.
pub fn inside_pid(status: &str) -> Option<u32> {
    let line = status.lines().find_map(|it| it.strip_prefix("NSpid:"))?;
    line.split_whitespace().last()?.parse().ok()
}

/// A process's `argv[0]`, from the bytes of `/proc/PID/cmdline` (its arguments, each ended by
/// a nul), as text.
pub fn argv0(cmdline: &[u8]) -> String {
    let first = cmdline.split(|it| *it == 0).next().unwrap_or_default();
    String::from_utf8_lossy(first).into_owned() // alloc: once a call described
}

impl fmt::Display for Made {
    /// `` process 4711 (7 in the sandbox), running `/t/build/x/build-script-build` (argv[0]
    /// `build-script-build`), made `getrandom` (system call 318) from libc
    /// (`/usr/lib/libc.so.6`, at 0x7f1c2a0e4b2d), before the program's `main` (a heuristic:
    /// ...) ``.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "process {}", self.pid)?;
        if let Some(inside) = self.inside_pid {
            write!(f, " ({inside} in the sandbox)")?;
        }
        // What `/proc` did not give is said as `?`, never left out.
        fn shown(it: &str) -> &str {
            if it.is_empty() { "?" } else { it }
        }
        let (program, argv0) = (shown(&self.program), shown(&self.argv0));
        let (syscall, number) = (&self.syscall, self.number);
        match self.what == EXEC {
            true => write!(
                f,
                ", `{argv0}`, started `{program}` by `{syscall}` (system call {number})"
            )?,
            false => write!(
                f,
                ", running `{program}` (argv[0] `{argv0}`), made `{syscall}` (system call {number})"
            )?,
        }
        match (self.kind, self.module.is_empty()) {
            (Module::NotKnown, _) => {
                write!(f, " from code the sandbox could not place (its maps were not read)")?
            }
            (kind, true) => write!(f, " from {} (at {:#x})", kind.name(), self.ip)?,
            (kind, false) => {
                write!(f, " from {} (`{}`, at {:#x})", kind.name(), self.module, self.ip)?
            }
        }
        write!(f, ", {}", self.phase.says())
    }
}

/// What a refusal says of who made the calls of kind `what` (`random`, `clock`, ...) among
/// `calls`: each caller on a line of its own, then how many more were not described; or
/// [`none_said`] when the run's sandbox described none.
pub fn said(calls: &[Call], what: &str) -> String {
    use fmt::Write as _;
    // alloc: error path
    let mut out = String::new();
    let mut more = 0;
    for call in calls {
        match call {
            Call::Made(it) if it.what == what => _ = write!(out, "\n  by {it}"),
            Call::Made(_) => {}
            Call::More(n) => more += n,
        }
    }
    if out.is_empty() {
        return none_said().to_owned(); // alloc: error path
    }
    if more > 0 {
        _ = write!(
            out,
            "\n  ({more} more calls or programs were not described: a run's first {MOST} are)"
        );
    }
    out
}

/// What is said where a run's sandbox described no caller.
pub fn none_said() -> &'static str {
    if cfg!(target_os = "linux") {
        "\n  (which process read it is not known: its sandbox described no caller of this for the run; `blaze explain script UNIT` lists the callers it did describe)"
    } else {
        "\n  (which process read it is not known: only the Linux sandbox's supervisor sees each call's process; here the sandbox's library in the process says what was read, not who)"
    }
}

/// The programs remembered at once, at most: past it the oldest goes.
const KEPT: usize = 256;

/// The last run's calls of each program run in a sandbox, by the program's path, oldest
/// first: a few dozen a build. In memory only: they explain a run of this daemon's.
static LAST: blaze_core::lock::Lock<Vec<(PathBuf, Vec<Call>)>> =
    blaze_core::lock::Lock::new("usercode.calls.last", Vec::new());

/// `program`'s run ended and left `calls`: they are its last run's from now on. A run that
/// left none forgets the run before's.
pub fn left(program: &Path, calls: Vec<Call>) {
    let mut last = LAST.lock();
    last.retain(|it| it.0 != program);
    if calls.is_empty() {
        return;
    }
    if last.len() >= KEPT {
        last.remove(0);
    }
    last.push((program.to_owned(), calls)); // alloc: once a run that left calls
}

/// The calls `program`'s last run left, none when it left none or none ran.
pub fn of(program: &Path) -> Vec<Call> {
    let last = LAST.lock();
    // clone: once a run explained (an error's text, the run's events)
    last.iter().find(|it| it.0 == program).map(|it| it.1.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPS: &str = "\
55d0c8a00000-55d0c8a2c000 r--p 00000000 08:01 1311     /t/debug/build/x-1f/build-script-build
55d0c8a2c000-55d0c8b10000 r-xp 0002c000 08:01 1311     /t/debug/build/x-1f/build-script-build
55d0c9d31000-55d0c9d52000 rw-p 00000000 00:00 0        [heap]
7f1c2a000000-7f1c2a028000 r--p 00000000 08:01 9177     /usr/lib/x86_64-linux-gnu/libc.so.6
7f1c2a028000-7f1c2a1bd000 r-xp 00028000 08:01 9177     /usr/lib/x86_64-linux-gnu/libc.so.6
7f1c2a3f0000-7f1c2a3f4000 r-xp 00000000 08:01 77       /c/blaze/sandbox/h/libblaze_sandbox_interpose.so
7f1c2a400000-7f1c2a402000 rw-p 00000000 00:00 0
7f1c2a41a000-7f1c2a440000 r-xp 00001000 08:01 9170     /usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2
7ffd1b5f2000-7ffd1b5f4000 r-xp 00000000 00:00 0        [vdso]
";
    const PROGRAM: &str = "/t/debug/build/x-1f/build-script-build";

    fn made() -> Made {
        Made {
            what: "random".to_owned(),
            pid: 4711,
            inside_pid: Some(7),
            program: PROGRAM.to_owned(),
            argv0: "build-script-build".to_owned(),
            syscall: "getrandom".to_owned(),
            number: 318,
            phase: Phase::Loaded,
            ip: 0x7f1c_2a0e_4b2d,
            module: "/usr/lib/x86_64-linux-gnu/libc.so.6".to_owned(),
            kind: Module::Libc,
        }
    }

    #[test]
    fn an_address_is_placed_in_the_mapping_that_holds_it() {
        let placed = |ip| module_at(MAPS, ip).map(|it| (it, Module::of_mapping(it, PROGRAM)));
        assert_eq!(placed(0x55d0_c8a2_c010), Some((PROGRAM, Module::Program)));
        assert_eq!(
            placed(0x7f1c_2a0e_4b2d),
            Some(("/usr/lib/x86_64-linux-gnu/libc.so.6", Module::Libc))
        );
        assert_eq!(
            placed(0x7f1c_2a41_a000),
            Some(("/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2", Module::Loader))
        );
        assert_eq!(
            placed(0x7f1c_2a3f_0001).map(|it| it.1),
            Some(Module::Interposer),
            "the sandbox's own library"
        );
        assert_eq!(placed(0x7f1c_2a40_0000), Some(("", Module::Anonymous)));
        assert_eq!(placed(0x55d0_c9d3_1000), Some(("[heap]", Module::Anonymous)));
        assert_eq!(placed(0x7ffd_1b5f_2000), Some(("[vdso]", Module::Vdso)));
        // An end is past its mapping, and an address in none is in none.
        assert_eq!(placed(0x7f1c_2a1b_d000), None);
        assert_eq!(placed(0x10), None);
        assert_eq!(module_at("not maps\n", 0x10), None);
        assert_eq!(
            Module::of_mapping("/lib/ld-musl-x86_64.so.1", PROGRAM),
            Module::Loader,
            "musl's loader is its libc: the loader is what it is called from first"
        );
    }

    #[test]
    fn a_process_is_numbered_as_its_innermost_namespace_numbers_it() {
        let status = "Name:\tbuild-script-bu\nTgid:\t4711\nNSpid:\t4711\t7\nPPid:\t4700\n";
        assert_eq!(inside_pid(status), Some(7));
        assert_eq!(inside_pid("Tgid:\t4711\nNSpid:\t4711\n"), Some(4711), "in no namespace");
        assert_eq!(inside_pid("Tgid:\t4711\n"), None, "a kernel before 4.1 says none");
        assert_eq!(argv0(b"build-script-build\0--flag\0"), "build-script-build");
        assert_eq!(argv0(b""), "", "a process that is gone, or a kernel thread");
    }

    #[test]
    fn calls_round_trip_and_a_bad_line_is_named() {
        let odd = Made {
            what: EXEC.to_owned(),
            inside_pid: None,
            program: "/usr/bin/a\tb".to_owned(),
            argv0: "sh\n".to_owned(),
            syscall: "execve".to_owned(),
            number: 59,
            phase: Phase::NotKnown,
            module: String::new(),
            kind: Module::NotKnown,
            ..made()
        };
        let calls = vec![Call::Made(made()), Call::Made(odd.clone()), Call::More(3)];
        let text = encode(&calls);
        assert_eq!(text.lines().count(), 3, "{text}");
        let read = decode(&text).unwrap();
        // A tab or a line break in a name is a space in its line: a line stays one.
        let odd = Made { program: "/usr/bin/a b".to_owned(), argv0: "sh ".to_owned(), ..odd };
        assert_eq!(read, vec![Call::Made(made()), Call::Made(odd), Call::More(3)]);
        assert_eq!(decode(""), Ok(Vec::new()));
        let why = decode("made\trandom\tx\n").unwrap_err();
        assert!(why.contains("line 1") && why.contains("its process"), "{why}");
        let why = decode(&format!("{text}what\n")).unwrap_err();
        assert!(why.contains("line 4") && why.contains("neither `made` nor `more`"), "{why}");
    }

    #[test]
    fn a_refusal_names_the_process_the_call_where_it_came_from_and_the_heuristic() {
        let exec = Made { what: EXEC.to_owned(), ..made() };
        let calls = [Call::Made(made()), Call::Made(exec), Call::More(2)];
        let text = said(&calls, "random");
        assert_eq!(
            text,
            "\n  by process 4711 (7 in the sandbox), running \
             `/t/debug/build/x-1f/build-script-build` (argv[0] `build-script-build`), made \
             `getrandom` (system call 318) from libc \
             (`/usr/lib/x86_64-linux-gnu/libc.so.6`, at 0x7f1c2a0e4b2d), after its libraries were \
             loaded: in a library's constructor or in `main`, which the sandbox can't tell apart\
             \n  (2 more calls or programs were not described: a run's first 32 are)"
        );
        // A kind no call is of says that none was described, never another kind's caller.
        assert_eq!(said(&calls, "clock"), none_said());
        let early = Made { phase: Phase::Loader, kind: Module::Loader, ..made() };
        assert!(early.to_string().contains("before the program's `main` (a heuristic"), "{early}");
        assert_eq!(Phase::Loader.before_main(), Some(true));
        assert_eq!(Phase::Loaded.before_main(), None, "said as not known, never guessed");
    }

    #[test]
    fn a_programs_last_run_is_what_is_kept_of_it() {
        let program = Path::new("/t/calls-test/build-script-build");
        assert_eq!(of(program), Vec::new());
        left(program, vec![Call::Made(made()), Call::More(1)]);
        left(program, vec![Call::Made(made())]);
        assert_eq!(of(program), vec![Call::Made(made())]);
        // A run that left none is the last run: the one before is not said of it.
        left(program, Vec::new());
        assert_eq!(of(program), Vec::new());
    }
}
