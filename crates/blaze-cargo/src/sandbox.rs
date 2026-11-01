//! Running a program in a sandbox: `blaze sandbox SPEC -- PROGRAM ARGS...`. The program sees
//! the machine's file system read-only, the directories [`Spec::overlays`] names as they are
//! but with its writes kept to itself, those [`Spec::writable`] names written for real, empty
//! temporary directories of its own, the environment the spec gives and no other, and no
//! network but its own loopback. What it reached outside is written to [`Spec::record`] as it
//! ends: the writes and the network the sandbox refused it, and, when [`Spec::record_reads`],
//! every path it opened, looked at, listed or ran, for what it read to be part of what its
//! result is a function of. `blaze test` runs every test executable in one
//! ([docs/cargo.md]); build scripts and proc macros are to run in the same ([#147]).
//!
//! This file is what every platform shares: the spec, the command, the probe, the record's
//! format and the marker a test harness gives the name of the test a thread runs by. Each
//! platform's sandbox is a module of its own with one entry, `run(spec, program)`: on Linux
//! namespaces, overlayfs and seccomp's user notification ([`linux`]); on macOS Seatbelt and a
//! library every process of the sandbox loads, which keeps the overlays' writes and records
//! (`sandbox/macos.rs`).
//!
//! [docs/cargo.md]: https://github.com/indexable-inc/blaze/blob/main/docs/cargo.md
//! [#147]: https://github.com/indexable-inc/blaze/issues/147

use std::{
    ffi::OsString,
    fmt::Write as _,
    hash::BuildHasher as _,
    io::Write as _,
    path::{Path, PathBuf},
};

use blaze_core::utf8::push_lossy;
use blaze_hash::{FixedState, HashMap, HashSet};
use blaze_usercode::trace::{self, Seen};
use serde::{Deserialize, Serialize};

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[allow(unsafe_code, reason = "a `link_section` constructor that runs a test's sandbox before `main`")]
mod alone;

#[cfg(target_os = "linux")]
#[allow(unsafe_code, reason = "the Linux driver: forks and closes by number; its calls are in `linux/raw.rs`")]
pub mod linux;

#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "the macOS driver: forks; its calls are in `macos/raw.rs`")]
pub mod macos;

#[cfg(unix)]
#[allow(unsafe_code, reason = "`sigaction` and the handler's `kill`")]
mod signal;
#[cfg(unix)]
pub use signal::pass_signals_to;

#[cfg(windows)]
#[allow(unsafe_code, reason = "the Win32 driver: AppContainers, job objects, DACLs, change watches")]
pub mod windows;

pub mod v2;

/// The subcommand of `blaze` that runs a program in a sandbox.
pub const COMMAND: &str = "sandbox";

/// What a test harness looks up, on the thread of a test, just before the test with the
/// test's name after it and just after it with nothing after it (`access(MARK + name, F_OK)`):
/// the sandbox takes the calls of that thread in between to be the test's. Never a path of
/// the record. Outside a sandbox it is one failed `access` a test.
pub const MARK: &str = "/.blaze-test/";

/// What the interposer a traced run's processes load (`sandbox/linux_interpose.c` on Linux)
/// looks up to tell the supervisor what a seccomp filter can't see, each one failed
/// `access(path, F_OK)`, never a path of the record: a variable read by name after it
/// ([`ENV_MARK`]), a realtime clock read ([`CLOCK_MARK`]), and, as it loads, that it is in the
/// process ([`LOADED_MARK`]). Randomness the filter sees itself (`getrandom`, an open of
/// `/dev/random` or `/dev/urandom`).
pub const ENV_MARK: &str = "/.blaze-env/";
pub const CLOCK_MARK: &str = "/.blaze-clock";
pub const LOADED_MARK: &str = "/.blaze-loaded";

/// The status a sandbox that could not be made ends with, as `env` ends when it can't run
/// what it is given.
pub const COULD_NOT: i32 = 125;

/// The variable that turns sandboxes off (`off` or `0`), for tests, build scripts and
/// proc-macro servers alike.
pub const VAR: &str = "BLAZE_SANDBOX";

/// Whether sandboxes are on, with `var` looking a variable up: unless [`VAR`] is `off` or
/// `0`. The one rule tests, build scripts and proc-macro servers decide by.
pub fn on_with(var: impl Fn(&str) -> Option<String>) -> bool {
    let value = var(VAR);
    !matches!(value.as_deref(), Some("off" | "0"))
}

/// Whether sandboxes are on for this process, as [`on_with`] says of its environment.
pub fn on() -> bool {
    on_with(|name| std::env::var(name).ok())
}

/// What a program is run with.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Spec {
    /// The directory it starts in.
    pub cwd: PathBuf,
    /// All of its environment. A sandbox may add variables of its own, which no identity is
    /// made of.
    pub env: Vec<(String, String)>,
    /// Directories it sees as they are and may change, its changes seen by it alone and gone
    /// when it ends.
    pub overlays: Vec<PathBuf>,
    /// Directories it writes for real.
    #[serde(default)]
    pub writable: Vec<PathBuf>,
    /// Directories it sees as they are and may not write, though they are below one of the
    /// sandbox's empty directories (a workspace or target directory under `/tmp`): the rest of
    /// the machine is seen read-only anyway.
    #[serde(default)]
    pub readable: Vec<PathBuf>,
    /// The file written as it ends with what it reached outside ([`Line`]), or nothing.
    pub record: Option<PathBuf>,
    /// Whether the record has the paths it read, which costs: every call that names a path
    /// goes through the sandbox. What was refused it is recorded either way.
    #[serde(default)]
    pub record_reads: bool,
    /// Paths that are not inputs of what runs: the program itself, whose code is known.
    pub not_inputs: Vec<PathBuf>,
    /// The file system it sees in place of the machine's, when it is given one: then it sees
    /// nothing of the machine's but what the view binds, and the overlays, the writable and
    /// the readable directories are empty (what it sees is the view's binds alone).
    #[serde(default)]
    pub view: Option<View>,
}

/// What `blaze sandbox --v2` asks of a sandbox beyond a [`Spec`] ([`v2`]); nothing more for
/// the form `blaze sandbox SPEC -- PROGRAM`.
#[derive(Default, Clone, Copy)]
pub struct Also<'a> {
    /// Whether it may reach the network: what it connects to is a `T network` line.
    pub network: bool,
    /// Where each line goes as it is first seen, before the call it notes goes on; with one,
    /// and reads recorded, the processes load the interposer, which sees what a sandbox's
    /// filter can't.
    pub live: Option<&'a Live>,
    /// The program's environment as it is, in place of [`Spec::env`], which then has what of
    /// it is text, for the variables the sandbox reads itself (`TMPDIR`).
    pub env: Option<&'a [(OsString, OsString)]>,
    /// Mach global names the program may look up beyond [`macos`]'s own few; read on macOS
    /// only.
    pub services: &'a [String],
}

/// The live record of a run of `blaze sandbox --v2`.
pub struct Live {
    pub path: PathBuf,
    /// Where this run's lines start in it.
    pub start: u64,
    /// Why a line could not be added to it, once one could not: the run's record is not whole.
    pub failed: std::cell::RefCell<Option<String>>,
}

impl Live {
    /// A line could not be added, for `why`; the first reason is kept.
    pub fn failed(&self, why: impl Into<String>) {
        self.failed.borrow_mut().get_or_insert_with(|| why.into());
    }
}

/// A file system of a program's own, as Nix gives a build: what it binds, each where the
/// program sees it, on a root of the sandbox's own that has nothing else but
///
/// - `/proc`, the PID namespace's own, which shows the sandbox's processes alone;
/// - `/dev` with `null`, `zero`, `full`, `random`, `urandom` and `tty`, the machine's,
///   `fd`, `stdin`, `stdout` and `stderr` linked into `/proc/self/fd`, and an empty `shm`;
/// - an empty `/tmp`;
/// - `/etc` with `passwd` and `group`, one line for the user it runs as and one for
///   `nobody`, whom files of other users show as, and `hosts`, `localhost` alone: what
///   Nix's sandbox gives, enough for `getpwuid` and a loopback name and nothing of the
///   machine's (no `resolv.conf`, there being no network to resolve on; no `nsswitch.conf`,
///   glibc reading the files without one).
///
/// The root, `/dev` and `/etc` are read-only. Everything else is not there: `/usr`, `$HOME`,
/// the store paths that are not bound (a `/nix/store` has those bound alone), the machine's
/// sockets. A path the program or anything it runs names outside the binds fails with
/// `ENOENT`, as the paths outside a Nix build's closure do. On Linux the view is a mount
/// namespace whose root replaces the machine's (`pivot_root`), the machine's tree detached
/// from it. On macOS a view is not made (decided): the program sees the machine as it would
/// without one.
///
/// A view for a build script, with a pinned compiler unpacked away from the store:
///
/// ```ignore
/// let view = View {
///     binds: vec![
///         Bind::read("/nix/store/abc-gcc-14", "/home/me/.blaze/nix/abc-gcc-14"),
///         Bind::read("/src/pkg", "/src/pkg"),
///         Bind::write("/out", "/target/debug/build/pkg-1/out"),
///     ],
/// };
/// ```
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// What it sees, in any order: one inside another is bound after it. Two never bind the
    /// same path, and none is `/` or below `/proc` or `/dev`, which are the view's own.
    pub binds: Vec<Bind>,
}

/// A directory or a file of the machine's, seen in a [`View`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    /// Where the program sees it: a whole path, with no `..`. Inside another bind it is a
    /// path that is there already, of the same kind (nothing is made in what is bound).
    pub inside: PathBuf,
    /// What it is on the machine, with the links on its way followed. One that is not there
    /// is an error, before anything runs.
    pub outside: PathBuf,
    /// Whether the program writes it for real; otherwise it is read-only, with what is below
    /// it.
    #[serde(default)]
    pub writable: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Bind {
    /// `outside`, seen read-only at `inside`.
    pub fn read(inside: impl Into<PathBuf>, outside: impl Into<PathBuf>) -> Bind {
        Bind { inside: inside.into(), outside: outside.into(), writable: false }
    }

    /// `outside`, written for real at `inside`.
    pub fn write(inside: impl Into<PathBuf>, outside: impl Into<PathBuf>) -> Bind {
        Bind { inside: inside.into(), outside: outside.into(), writable: true }
    }
}

/// What a line of a record says was reached.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Reached {
    /// `R`: a path opened, listed or run.
    Read,
    /// `L`: a path only looked at (`stat`, `access`, `chdir`, `readlink`): what it is, whether
    /// it can be run, a link's text and a file's size, but not what is in it.
    Looked,
    /// `W`: a write the sandbox refused: outside the overlays, the writable directories and
    /// the program's own empty ones.
    Write,
    /// `N`: an address the sandbox refused a connection, a bind or a message to.
    Net,
    /// `X`: the record is not whole, and why: what was read is not all known.
    Incomplete,
}

impl Reached {
    fn letter(self) -> char {
        match self {
            Reached::Read => 'R',
            Reached::Looked => 'L',
            Reached::Write => 'W',
            Reached::Net => 'N',
            Reached::Incomplete => 'X',
        }
    }

    fn of(letter: &str) -> Option<Reached> {
        Some(match letter {
            "R" => Reached::Read,
            "L" => Reached::Looked,
            "W" => Reached::Write,
            "N" => Reached::Net,
            "X" => Reached::Incomplete,
            _ => return None,
        })
    }

    /// What the record of the form `blaze sandbox SPEC -- PROGRAM` has for what a sandbox saw,
    /// which is a [`trace::Line`]: a listing is a read there, and variables and what can't be
    /// traced are not in it (that form loads no interposer and keys by none of them).
    fn of_seen(seen: Seen) -> Option<Reached> {
        Some(match seen {
            Seen::Read | Seen::Listed => Reached::Read,
            Seen::Looked => Reached::Looked,
            Seen::RefusedWrite => Reached::Write,
            Seen::RefusedNet => Reached::Net,
            Seen::Incomplete => Reached::Incomplete,
            Seen::Env | Seen::Untraceable => return None,
        })
    }
}

/// A line of the record `blaze sandbox SPEC -- PROGRAM` writes ([`Spec::record`]), made from
/// the same [`trace::Line`]s the live record of `--v2` has: `R`, `L`, `W`, `N` or `X`, the test, and the path, the address or why, by
/// tabs. The test is libtest's name of the one whose thread made the call, as [`MARK`] said,
/// or empty when that is not known (another thread, a child process, the program before or
/// after its tests): what has an empty test is taken to be every test's of the run. A record
/// has each line once, in order.
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Line {
    pub reached: Reached,
    pub test: String,
    pub what: String,
}

/// The lines of the record `file`, `None` when there is none or it is not a record.
pub fn read_record(file: &Path) -> Option<Vec<Line>> {
    let text = std::fs::read_to_string(file).ok()?;
    text.lines()
        .map(|line| {
            let mut parts = line.splitn(3, '\t');
            let reached = Reached::of(parts.next()?)?;
            // alloc: the record is read back once, after the run, into the lines the caller keeps.
            let (test, what) = (parts.next()?.to_owned(), parts.next()?.to_owned());
            Some(Line { reached, test, what })
        })
        // alloc: one `Vec` of the lines, once per record read back.
        .collect()
}

/// The text of the record, each name once: the paths, variables and scopes its lines name,
/// one after another in one buffer, each by a `u32`. A name is hashed once and
/// compared only with the names of its hash, and one seen before allocates nothing.
#[derive(Default)]
struct Names {
    text: String,
    /// Where each name ends in `text`, by its id.
    ends: Vec<u32>,
    /// The last name of each hash; [`Names::next`] has the others of that hash before it.
    first: HashMap<u64, u32>,
    next: Vec<u32>,
}

/// No name: the end of a chain of [`Names::next`].
const NO_NAME: u32 = u32::MAX;

/// The name with id 0, made first: no scope.
const NO_SCOPE: u32 = 0;

impl Names {
    fn get(&self, id: u32) -> &str {
        name_at(&self.text, &self.ends, id)
    }

    fn intern(&mut self, name: &str) -> u32 {
        let head = self.first.entry(FixedState.hash_one(name)).or_insert(NO_NAME);
        let mut at = *head;
        while at != NO_NAME {
            if name_at(&self.text, &self.ends, at) == name {
                return at;
            }
            at = self.next[at as usize];
        }
        let id = self.ends.len() as u32;
        self.text.push_str(name);
        self.ends.push(self.text.len() as u32);
        self.next.push(*head);
        *head = id;
        id
    }
}

fn name_at<'a>(text: &'a str, ends: &[u32], id: u32) -> &'a str {
    let start = match id {
        0 => 0,
        _ => ends[id as usize - 1] as usize,
    };
    &text[start..ends[id as usize] as usize]
}

/// A [`trace::Line`] by the ids of its names.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Noted {
    what: u32,
    scope: u32,
    seen: Seen,
}

const _: () = assert!(std::mem::size_of::<Noted>() == 12);

/// A scope of a [`Recorder`], by the id of its name: what a thread runs in, handed from one
/// thread to another without its text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Scope(u32);

impl Scope {
    /// No scope: every scope's.
    pub const NONE: Scope = Scope(NO_SCOPE);
}

/// Whether a scope or what a line names can't be written as it is: a tab or a line break.
fn unwritable(text: &str) -> bool {
    text.as_bytes().contains(&b'\t') || text.as_bytes().contains(&b'\n')
}

/// The lines a [`Recorder`] has room for before it grows: a test's run reads a few hundred
/// paths.
const LINES: usize = 1024;

/// The room of a line or a name being written: a path is at most 4096 bytes, most far less.
const LINE: usize = 512;

/// What a sandbox notes as its program runs: each [`trace::Line`] once, appended to the live
/// record as it is first seen when there is one ([`Recorder::live`]), and written as the record
/// of `blaze sandbox SPEC -- PROGRAM` when the run ends ([`Recorder::write`]). A line seen
/// before costs hashing its path and one lookup, and allocates nothing.
pub struct Recorder {
    names: Names,
    /// Every line seen, kept or not: one seen before is not looked at again.
    noted: HashSet<Noted>,
    /// The lines kept, in the order they were first seen.
    lines: Vec<Noted>,
    /// The live record, each new line appended before the call it notes goes on.
    live: Option<std::fs::File>,
    /// Why a line could not be appended to the live record: it is not whole after that.
    live_failed: Option<String>,
    /// The line being appended to the live record.
    appending: Vec<u8>,
    /// A name read as text (a marker's, or why a program was not traced), kept from one to
    /// the next.
    text: String,
    /// The scope each thread runs in, by the thread's id, as [`MARK`] said or as the thread
    /// that forked its process had it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    threads: HashMap<u64, u32>,
    /// Every thread seen, scoped or not: one not seen yet is new.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    known: HashSet<u64>,
    /// Whether the processes load the interposer: one that ran a program and never said it
    /// loaded is an `X` line ([`Recorder::ended`]).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    interposed: bool,
    /// The processes that ran a program and have not said the interposer loaded since, by
    /// process id: the ids of the program's name and of the scope it ran in. Walked in process
    /// id order once, as the run ends ([`Recorder::ended`]).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pending: HashMap<u64, (u32, u32)>,
    reads: bool,
    /// What is no input of the run's own process: the program it is, read again by a test
    /// that runs it, is an input of that test ([`Recorder::input`]).
    not_inputs: HashSet<String>,
    /// What is the sandbox's and no input of any line: the interposer.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ours: Vec<String>,
    /// Where writes are the program's: the overlays, the writable directories and the
    /// sandbox's own.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    written: Vec<String>,
    /// The readable directories: below one of the sandbox's own, but not written there.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    readable: Vec<String>,
    overlays: Vec<String>,
    /// The sandbox's own directories, whose paths are no inputs: the kernel's views and the
    /// empty directories.
    own: Vec<String>,
}

impl Recorder {
    /// For `spec`, in a sandbox whose own directories are `own`.
    pub fn new(spec: &Spec, own: &[&str]) -> Recorder {
        // alloc: the directories the lines are told by, made whole once per sandbox.
        let tidied = |it: &Vec<PathBuf>| it.iter().map(|it| tidy(it)).collect::<Vec<_>>();
        let overlays = tidied(&spec.overlays);
        // clone: once per sandbox, the overlays being where writes are the program's too.
        let mut written = overlays.clone();
        written.extend(tidied(&spec.writable));
        // alloc: once per sandbox, its own directories by name.
        written.extend(own.iter().map(|it| (*it).to_owned()));
        let mut readable = tidied(&spec.readable);
        // A view's binds are where it writes and what it reads, as the writable and readable
        // directories are, whichever of the sandbox's own directories they are below.
        for bind in spec.view.iter().flat_map(|it| &it.binds) {
            match bind.writable {
                true => written.push(tidy(&bind.inside)),
                false => readable.push(tidy(&bind.inside)),
            }
        }
        // What is read there is an input, as what is read in an overlay is.
        let mut overlays = overlays;
        overlays.extend(readable.iter().cloned());
        let mut names = Names::default();
        names.intern("");
        Recorder {
            names,
            noted: HashSet::default(),
            lines: Vec::with_capacity(LINES),
            live: None,
            live_failed: None,
            appending: Vec::with_capacity(LINE),
            text: String::with_capacity(LINE),
            threads: HashMap::default(),
            known: HashSet::default(),
            interposed: false,
            pending: HashMap::default(),
            reads: spec.record_reads,
            // alloc: once per sandbox, the paths that are no input by name.
            not_inputs: spec.not_inputs.iter().map(|it| tidy(it)).collect(),
            ours: Vec::with_capacity(1),
            written,
            readable,
            overlays,
            // alloc: once per sandbox, as `written`'s.
            own: own.iter().map(|it| (*it).to_owned()).collect(),
        }
    }

    /// Appends each line to `file` as it is first seen, from now on.
    pub fn live(&mut self, file: &Path) -> Result<(), String> {
        let opened = std::fs::OpenOptions::new().append(true).open(file);
        // alloc: the error, once, and the sandbox is not made.
        self.live = Some(opened.map_err(|e| format!("opening the live record {}: {e}", file.display()))?);
        Ok(())
    }

    /// Takes the programs the run's processes run to load the interposer, which says so as it
    /// loads ([`LOADED_MARK`]): one that does not is noted at the end of the run.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn interposed(&mut self) {
        self.interposed = true;
    }

    /// Takes `path` as the sandbox's own file, which no line names.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn ours(&mut self, path: &Path) {
        self.ours.push(tidy(path));
    }

    /// `scope` (empty when it is not known) saw `what`, as `seen` says: for a sandbox that
    /// knows the scope of a call itself, not by its thread. A read, a listing, a variable and
    /// what can't be traced are kept only when reads are recorded, a path only when it is an
    /// input, as [`Recorder::read`] keeps it.
    pub fn reached(&mut self, seen: Seen, scope: &str, what: &str) {
        let scope = self.names.intern(scope);
        self.note(seen, scope, what);
    }

    /// [`Recorder::reached`], in the scope of id `scope`.
    fn note(&mut self, seen: Seen, scope: u32, what: &str) {
        let recorded = match seen {
            Seen::Read | Seen::Looked | Seen::Listed | Seen::Env | Seen::Untraceable => self.reads,
            Seen::RefusedWrite | Seen::RefusedNet | Seen::Incomplete => true,
        };
        if !recorded {
            return;
        }
        let noted = Noted { what: self.names.intern(what), scope, seen };
        if !self.noted.insert(noted) {
            return;
        }
        if matches!(seen, Seen::Read | Seen::Looked | Seen::Listed) && !self.input(scope, what) {
            return;
        }
        let written = match (unwritable(self.names.get(scope)), unwritable(what)) {
            (false, false) => noted,
            (true, _) => Noted {
                what: self.names.intern("a scope with a tab or a line break in it"),
                scope: NO_SCOPE,
                seen: Seen::Incomplete,
            },
            (false, true) => Noted {
                what: self.names.intern("a path or a name with a tab or a line break in it"),
                scope,
                seen: Seen::Incomplete,
            },
        };
        if written != noted && !self.noted.insert(written) {
            return;
        }
        if let Some(file) = &mut self.live
            && self.live_failed.is_none()
        {
            let line = &mut self.appending;
            line.clear();
            line.push(written.seen.letter() as u8);
            line.push(b'\t');
            line.extend_from_slice(self.names.get(written.scope).as_bytes());
            line.push(b'\t');
            line.extend_from_slice(self.names.get(written.what).as_bytes());
            line.push(b'\n');
            if let Err(e) = file.write_all(line) {
                // alloc: the first failure alone, after which no line is appended.
                self.live_failed = Some(e.to_string());
            }
        }
        self.lines.push(written);
    }

    /// Every line seen, each once, in the order first seen.
    #[cfg(test)]
    pub fn lines(&self) -> impl Iterator<Item = trace::Line> + '_ {
        self.lines.iter().map(|it| trace::Line {
            seen: it.seen,
            scope: self.names.get(it.scope).to_owned(),
            what: self.names.get(it.what).to_owned(),
        })
    }

    /// Why the live record is not whole, when a line could not be appended to it.
    pub fn live_failed(&self) -> Option<&str> {
        self.live_failed.as_deref()
    }

    /// The run ended: each process that ran a program which never said the interposer loaded
    /// (a static or setuid one, or one that cleared the variables that load it) read what was
    /// not traced. A program that failed to start is one too: it read nothing, but is not told
    /// from one that did.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn ended(&mut self) {
        // alloc: once, as the run ends: the processes never traced, sorted by process id.
        let mut pending: Vec<(u64, (u32, u32))> = std::mem::take(&mut self.pending).into_iter().collect();
        blaze_ids::radix::sort_by_key(&mut pending, |it| it.0);
        let mut why = std::mem::take(&mut self.text);
        for (_, (program, scope)) in pending {
            why.clear();
            let program = self.names.get(program);
            _ = write!(why, "{program} ran without the interposer, so what it read is not known");
            self.note(Seen::Incomplete, scope, &why);
        }
        self.text = why;
    }

    /// Whether `path`, made whole, is an input of the line in `scope`: not the sandbox's own
    /// file, not one of `not_inputs` in the run's own process's (unscoped) lines, and not in
    /// the sandbox's own directories but where an overlay is.
    fn input(&self, scope: u32, path: &str) -> bool {
        !self.ours.iter().any(|it| it == path)
            && (scope != NO_SCOPE || !self.not_inputs.contains(path))
            && (self.overlays.iter().any(|it| under(path, it))
                || !self.own.iter().any(|it| under(path, it)))
    }

    /// Writes the record of `blaze sandbox SPEC -- PROGRAM` to `file`, whole or not at all.
    pub fn write(&self, file: &Path) {
        let mut lines: Vec<(Reached, &str, &str)> = self
            .lines
            .iter()
            .filter_map(|it| {
                let reached = Reached::of_seen(it.seen)?;
                Some((reached, self.names.get(it.scope), self.names.get(it.what)))
            })
            // alloc: once, as the run ends: the record's lines by reference, to sort.
            .collect();
        lines.sort_unstable();
        lines.dedup();
        let room = lines.iter().map(|it| it.1.len() + it.2.len() + 4).sum();
        let mut text = String::with_capacity(room);
        for (reached, test, what) in lines {
            text.push(reached.letter());
            text.push('\t');
            text.push_str(test);
            text.push('\t');
            text.push_str(what);
            text.push('\n');
        }
        let writing = file.with_extension("writing");
        if std::fs::write(&writing, text).and_then(|()| std::fs::rename(&writing, file)).is_err() {
            _ = std::fs::remove_file(file);
        }
    }
}

/// What a sandbox that knows the test of a call by its thread notes: the Linux one, whose
/// supervisor sees each call's thread and the harness's marker.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Recorder {
    /// Whether the reads are recorded.
    pub fn reads(&self) -> bool {
        self.reads
    }

    /// Takes `path`, as `thread` of `process` named it, as one of the markers when it is:
    /// [`MARK`] (the thread runs in the scope after it, or none), and, when the processes load
    /// the interposer, [`ENV_MARK`], [`CLOCK_MARK`] and [`LOADED_MARK`]. Whether it was one.
    pub fn marked(&mut self, thread: u64, process: u64, path: &[u8]) -> bool {
        self.known.insert(thread);
        if let Some(name) = path.strip_prefix(MARK.as_bytes()) {
            match name.is_empty() {
                true => _ = self.threads.remove(&thread),
                false => {
                    lossy_into(name, &mut self.text);
                    let scope = self.names.intern(&self.text);
                    self.threads.insert(thread, scope);
                }
            }
            return true;
        }
        if !self.interposed {
            return false;
        }
        if let Some(name) = path.strip_prefix(ENV_MARK.as_bytes()) {
            // A name that is not UTF-8 is read as one that is: the variable is an input
            // either way, and `Traced::env` names variables as text.
            let mut text = std::mem::take(&mut self.text);
            lossy_into(name, &mut text);
            self.note(Seen::Env, self.scope_id(thread), &text);
            self.text = text;
            return true;
        }
        if path == CLOCK_MARK.as_bytes() {
            self.untraceable(thread, blaze_core::Untraceable::Clock);
            return true;
        }
        if path == LOADED_MARK.as_bytes() {
            self.pending.remove(&process);
            return true;
        }
        false
    }

    /// The name of the scope `thread` runs in, empty when none.
    pub fn scope(&self, thread: u64) -> &str {
        self.names.get(self.scope_id(thread))
    }

    /// The scope `thread` runs in, [`Scope::NONE`] when none.
    pub fn scope_of(&self, thread: u64) -> Scope {
        Scope(self.scope_id(thread))
    }

    /// The scope named `name`, kept from now on.
    #[cfg(test)]
    fn scope_named(&mut self, name: &str) -> Scope {
        Scope(self.names.intern(name))
    }

    /// The id of the scope `thread` runs in.
    fn scope_id(&self, thread: u64) -> u32 {
        self.threads.get(&thread).copied().unwrap_or(NO_SCOPE)
    }

    /// Whether `thread` was seen before.
    pub fn knows(&self, thread: u64) -> bool {
        self.known.contains(&thread)
    }

    /// `thread`, not seen before, starts in `scope` ([`Scope::NONE`] for none): a new process
    /// starts in the scope the thread that forked it had then.
    pub fn starts(&mut self, thread: u64, scope: Scope) {
        if self.known.insert(thread) && scope != Scope::NONE {
            self.threads.insert(thread, scope.0);
        }
    }

    /// `thread` read `path`, made whole: recorded when reads are and it is an input.
    pub fn read(&mut self, thread: u64, path: &str) {
        self.note(Seen::Read, self.scope_id(thread), path);
    }

    /// `thread` looked at `path`, made whole, without opening it: recorded as [`Self::read`]
    /// records a read.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn looked(&mut self, thread: u64, path: &str) {
        self.note(Seen::Looked, self.scope_id(thread), path);
    }

    /// `thread` listed the directory `path`, made whole: recorded as [`Self::read`] records a
    /// read.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn listed(&mut self, thread: u64, path: &str) {
        self.note(Seen::Listed, self.scope_id(thread), path);
    }

    /// `thread` got what can't be traced.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn untraceable(&mut self, thread: u64, it: blaze_core::Untraceable) {
        self.note(Seen::Untraceable, self.scope_id(thread), trace::untraceable_name(it));
    }

    /// Randomness was read. What a process read of it stays in it (`std` keeps the keys of its
    /// hash maps), whatever scope it is in later: it is every scope's.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn random(&mut self) {
        let name = trace::untraceable_name(blaze_core::Untraceable::Random);
        self.note(Seen::Untraceable, NO_SCOPE, name);
    }

    /// `process` runs the program `path`, in `scope`: when the processes load the interposer,
    /// it is to say so before the run ends.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn runs(&mut self, process: u64, path: &str, scope: Scope) {
        if self.interposed && self.reads {
            let program = self.names.intern(path);
            self.pending.insert(process, (program, scope.0));
        }
    }

    /// Whether a write to `path` is the program's to make.
    pub fn may_write(&self, path: &str) -> bool {
        self.written.iter().any(|it| under(path, it) && !self.only_read(path, it))
    }

    /// Whether `path` is in a readable directory that is below `dir`, one of the sandbox's
    /// own: it is seen there read-only.
    fn only_read(&self, path: &str, dir: &str) -> bool {
        self.readable.iter().any(|it| under(it, dir) && it != dir && under(path, it))
            && !self.written.iter().any(|it| under(it, dir) && it != dir && under(path, it))
    }

    /// Whether a connection to the Unix socket at `path`, made whole with its links followed,
    /// is the program's to make: one in the overlays, the writable directories or the
    /// sandbox's empty ones, which only the program's sockets can be in; not one of the
    /// kernel's views or the machine's `/dev`, which the sandbox sees as they are; or one the
    /// read set names itself (a test's fork server's socket).
    pub fn may_connect(&self, path: &str) -> bool {
        let seen_as_is = ["/proc", "/sys", "/dev"].iter().any(|it| under(path, it));
        (self.may_write(path) && (!seen_as_is || under(path, "/dev/shm"))) || self.readable.iter().any(|it| it == path)
    }

    /// The sandbox refused `thread` a write to `path`.
    pub fn refused_write(&mut self, thread: u64, path: &str) {
        self.note(Seen::RefusedWrite, self.scope_id(thread), path);
    }

    /// The sandbox refused `thread` the network at `address`.
    pub fn refused_net(&mut self, thread: u64, address: &str) {
        self.note(Seen::RefusedNet, self.scope_id(thread), address);
    }

    /// The record is not whole for what `thread` did, and why.
    pub fn incomplete(&mut self, thread: u64, why: &str) {
        self.note(Seen::Incomplete, self.scope_id(thread), why);
    }
}

/// Whether `path` is `dir` or below it.
fn under(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `blaze sandbox SPEC -- PROGRAM ARGS...`, `blaze sandbox --v2 SPEC_FILE` ([`v2::command`]),
/// `blaze sandbox --v2-serve` ([`v2::serve`]), or `blaze sandbox --probe [--overlays]`: runs the
/// program and ends as it ended. Runs before anything else in the process, which is one thread:
/// it forks.
pub fn command(mut args: impl Iterator<Item = OsString>) -> ! {
    let first = args.next();
    if first.as_deref() == Some("--v2".as_ref()) {
        v2::command(args)
    }
    if first.as_deref() == Some("--v2-serve".as_ref()) {
        v2::serve(args)
    }
    if first.as_deref() == Some("--probe".as_ref()) {
        let kind = match args.next() {
            None => Probe::Writable,
            Some(it) if it == "--overlays" => Probe::Overlays,
            Some(it) if it == "--v2" => Probe::V2,
            Some(it) => {
                eprintln!("blaze sandbox: --probe takes --overlays or --v2, not {}", it.display());
                std::process::exit(COULD_NOT);
            }
        };
        std::process::exit(match probe(kind) {
            Ok(()) => 0,
            Err(why) => {
                eprintln!("{why}");
                1
            }
        });
    }
    let spec = first.ok_or("a spec").and_then(|it| {
        let text = std::fs::read_to_string(&it).map_err(|_| "a spec it can read")?;
        serde_json::from_str::<Spec>(&text).map_err(|_| "a spec it can parse")
    });
    // alloc: the program's command line, once, at the edge.
    let program: Vec<OsString> = args.skip_while(|it| it != "--").skip(1).collect();
    let spec = match (spec, program.is_empty()) {
        (Ok(spec), false) => spec,
        (Err(what), _) => {
            eprintln!("blaze sandbox: needs {what}: blaze sandbox SPEC -- PROGRAM ARGS...");
            std::process::exit(COULD_NOT);
        }
        (_, true) => {
            eprintln!("blaze sandbox: needs a program: blaze sandbox SPEC -- PROGRAM ARGS...");
            std::process::exit(COULD_NOT);
        }
    };
    std::process::exit(match backend(&spec, Some(&program), Also::default()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("blaze: the sandbox could not be made: {error}");
            COULD_NOT
        }
    })
}

/// Runs `program` in the sandbox `spec` and `also` say, or with `None` makes the sandbox and
/// runs nothing: what it ends with, its status or 128 and the signal that ended it.
fn backend(spec: &Spec, program: Option<&[OsString]>, also: Also<'_>) -> Result<i32, String> {
    #[cfg(target_os = "linux")]
    return linux::run(spec, program, also);
    #[cfg(target_os = "macos")]
    return macos::run(spec, program, also);
    #[cfg(windows)]
    return windows_backend(spec, program, also);
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        _ = (spec, program, also);
        // alloc: the error, once: there is no sandbox to make.
        Err(format!("there is no sandbox on {} yet", std::env::consts::OS))
    }
}

/// [`backend`] on Windows, by [`windows::run`]: an AppContainer has no overlays and no views,
/// and traces nothing, so a record is one `X` line.
#[cfg(windows)]
fn windows_backend(spec: &Spec, program: Option<&[OsString]>, also: Also<'_>) -> Result<i32, String> {
    if !spec.overlays.is_empty() {
        // alloc: the error, once: no sandbox is made.
        return Err("there are no overlays on Windows: a test's sandbox can't be made".to_owned());
    }
    if spec.view.is_some() {
        // alloc: the error, once: no sandbox is made.
        return Err("there are no views of the machine on Windows".to_owned());
    }
    let Some((name, args)) = program.and_then(|it| it.split_first()) else {
        return windows::probe().map(|()| 0);
    };
    // The launcher's spec, made once per run at the edge: what it owns is what it hands the
    // AppContainer.
    let confined = blaze_usercode::confine::Spec {
        program: PathBuf::from(name),
        args: args.to_vec(), // alloc: once per run, the launcher's spec.
        env: spec.env.iter().map(|(k, v)| (k.into(), v.into())).collect(), // alloc: as `args`.
        dir: spec.cwd.clone(),        // clone: as `args`.
        read: spec.readable.clone(),  // clone: as `args`.
        write: spec.writable.clone(), // clone: as `args`.
        network: also.network,
        trace: false,
        services: Vec::new(), // alloc: none, an empty list; Windows has no Mach names.
        live: PathBuf::new(),
        report: PathBuf::new(),
        not_inputs: Vec::with_capacity(0),
    };
    let code = windows::run(&confined, windows::Given::own())?;
    if let Some(file) = &spec.record {
        let mut recorder = Recorder::new(spec, &[]);
        recorder.reached(Seen::Incomplete, "", v2::NOT_TRACED_ON_WINDOWS);
        recorder.write(file);
    }
    Ok(code)
}

/// Whether programs can be run in a sandbox here, with `overlays` (as tests are) or without
/// (as build scripts are), and why not when they can't: the probe makes one around nothing, in
/// a process of its own, once per process that asks.
pub fn available(overlays: bool) -> Result<(), String> {
    probed(match overlays {
        true => Probe::Overlays,
        false => Probe::Writable,
    })
}

/// What a probe makes a sandbox for.
#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Serialize, Deserialize))]
enum Probe {
    /// A writable directory, as a build script's sandbox has.
    Writable,
    /// An overlay, as a test's sandbox has.
    Overlays,
    /// What `blaze sandbox --v2` makes: a writable directory and a live record, and on Linux
    /// the interposer the run's processes load.
    V2,
}

/// [`available`] for `kind`, the probe run once per process.
fn probed(kind: Probe) -> Result<(), String> {
    static AVAILABLE: [std::sync::OnceLock<Result<(), String>>; 3] =
        [std::sync::OnceLock::new(), std::sync::OnceLock::new(), std::sync::OnceLock::new()];
    AVAILABLE[kind as usize]
        .get_or_init(|| {
            if !cfg!(any(target_os = "linux", target_os = "macos", windows)) {
                // alloc: the error, once per process.
                return Err(format!("there is no sandbox on {} yet", std::env::consts::OS));
            }
            // alloc: the error, once per process.
            let exe = std::env::current_exe().map_err(|it| format!("blaze is not found ({it})"))?;
            let flag = match kind {
                Probe::Writable => None,
                Probe::Overlays => Some("--overlays"),
                Probe::V2 => Some("--v2"),
            };
            let out = std::process::Command::new(exe)
                .args([COMMAND, "--probe"])
                .args(flag)
                .stdin(std::process::Stdio::null())
                .output()
                // alloc: the error, once per process.
                .map_err(|it| format!("the probe did not run ({it})"))?;
            match out.status.success() {
                true => Ok(()),
                // alloc: the error, once per process.
                false => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
            }
        })
        // `Ok(())` copies nothing.
        // clone: the error alone: why no sandbox can be made, for the caller to report.
        .clone()
}

/// Makes a sandbox around nothing, as `kind` says: `Ok` when it could be made, why not
/// otherwise, with what would let it be where that is known.
fn probe(kind: Probe) -> Result<(), String> {
    // alloc: the probe's directory, once per process.
    let dir = std::env::temp_dir().join(format!("blaze-sandbox-probe-{}", std::process::id()));
    // alloc: the error, once.
    std::fs::create_dir_all(&dir).map_err(|it| format!("{}: {it}", dir.display()))?;
    // The probe's spec, once per process: the directory is each of its parts.
    let spec = match kind {
        // clone: the probe's spec, once per process.
        Probe::Overlays => Spec { cwd: dir.clone(), overlays: vec![dir.clone()], ..Spec::default() },
        // clone: the probe's spec, once per process.
        Probe::Writable => Spec { cwd: dir.clone(), writable: vec![dir.clone()], ..Spec::default() },
        Probe::V2 => Spec {
            cwd: dir.clone(),           // clone: the probe's spec, once per process.
            writable: vec![dir.clone()], // clone: the probe's spec, once per process.
            record_reads: true,
            ..Spec::default()
        },
    };
    let live = Live { path: dir.join("live"), start: 0, failed: Default::default() };
    let also = match kind {
        Probe::V2 => {
            // alloc: the error, once.
            std::fs::write(&live.path, "").map_err(|it| format!("{}: {it}", live.path.display()))?;
            Also { network: false, live: Some(&live), env: None, services: &[] }
        }
        Probe::Writable | Probe::Overlays => Also::default(),
    };
    let ended = backend(&spec, None, also);
    _ = std::fs::remove_dir_all(&dir);
    let restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns");
    let hint = match restricted.is_ok_and(|it| it.trim() == "1") {
        true => {
            " (AppArmor keeps users from making namespaces; \
             `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0` lets them, \
             or an AppArmor profile that allows `userns` for blaze)"
        }
        false => "",
    };
    match ended {
        Ok(0) => Ok(()),
        // alloc: the error, once per process.
        Ok(_) => Err(format!("the sandbox could not be made (it said why above){hint}")),
        // alloc: the error, once per process.
        Err(error) => Err(format!("{error}{hint}")),
    }
}

/// What a container this process is in is known by, below `root` (`/` but in tests), with
/// `container` the variable of that name: `/.dockerenv` (Docker), `/run/.containerenv` (Podman),
/// the variable (systemd-nspawn, Podman, LXC), or `/proc/1/cgroup` naming a runtime (cgroup v1
/// Docker, Kubernetes, containerd). `None` outside one, as far as it can be told.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn container(root: &Path, container: Option<&str>) -> Option<String> {
    for file in [".dockerenv", "run/.containerenv"] {
        if root.join(file).exists() {
            // alloc: part of an error, once: the sandbox could not be made.
            return Some(format!("/{file}"));
        }
    }
    if let Some(name) = container.filter(|it| !it.is_empty()) {
        // alloc: part of an error, once.
        return Some(format!("`container={name}`"));
    }
    let cgroup = std::fs::read_to_string(root.join("proc/1/cgroup")).unwrap_or_default();
    ["docker", "kubepods", "containerd", "libpod", "lxc"]
        .into_iter()
        .find(|it| cgroup.contains(it))
        // alloc: part of an error, once.
        .map(|it| format!("/proc/1/cgroup naming {it}"))
}

/// What is said after a user namespace refused with `EPERM` in a container known by `known`
/// ([`container`]): the container's seccomp profile (Docker's default) is what refuses it, and
/// the ways on: a profile that allows it, or no sandbox at all.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn container_hint(known: &str) -> String {
    // alloc: part of an error, once: the sandbox could not be made.
    format!(
        "; this runs in a container ({known}), and its seccomp profile (Docker's default) refuses \
         `unshare(CLONE_NEWUSER)`: run the container with `--security-opt seccomp=unconfined` \
         (or a seccomp profile that allows user namespaces; on Kubernetes \
         `securityContext.seccompProfile.type: Unconfined`), or set `{VAR}=off` to run \
         build scripts, proc macros and tests outside a sandbox, as cargo does"
    )
}

/// `path` with `.` and empty parts left out.
fn tidy(path: &Path) -> String {
    use std::path::Component;
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.to_string_lossy().into_owned()
}

/// [`tidy`] of the Unix path `path` into `out`, which keeps its room from one path to the next.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn tidy_into(path: &[u8], out: &mut String) {
    out.clear();
    if path.first() == Some(&b'/') {
        out.push('/');
    }
    for part in path.split(|&it| it == b'/').filter(|it| !it.is_empty() && *it != b".") {
        if !out.is_empty() && !out.ends_with('/') {
            out.push('/');
        }
        push_lossy(out, part);
    }
}

/// `bytes` as text into `out`, in the room `out` keeps from one to the next: as
/// `String::from_utf8_lossy` makes it, each piece that is not UTF-8 one U+FFFD.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn lossy_into(bytes: &[u8], out: &mut String) {
    out.clear();
    push_lossy(out, bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts each test thread's allocations, for the allocation-budget tests
    /// (docs/v2/plan/goals/principles.md): this crate's test binary has its one global allocator
    /// here.
    #[global_allocator]
    static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

    /// The recorder's hot path, a line it has seen (a path read again, a write refused again, a
    /// thread marking the test it is in again, a variable read again), allocates nothing; with
    /// the live record too, which a line seen before is not appended to again.
    #[test]
    fn a_line_seen_before_allocates_nothing() {
        let dir = std::env::temp_dir().join(format!("blaze-record-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("live");
        std::fs::write(&live, "").unwrap();
        let spec = Spec { overlays: vec!["/ws".into()], record_reads: true, ..Spec::default() };
        let mut recorder = Recorder::new(&spec, &["/tmp"]);
        recorder.live(&live).unwrap();
        recorder.interposed();
        let seen = |recorder: &mut Recorder| {
            assert!(recorder.marked(7, 7, b"/.blaze-test/tests::a"));
            recorder.read(7, "/ws/a");
            recorder.read(8, "/ws/a");
            recorder.looked(7, "/ws/b");
            recorder.listed(7, "/ws");
            recorder.refused_write(7, "/etc/x");
            recorder.refused_net(7, "1.2.3.4:443");
            recorder.untraceable(7, blaze_core::Untraceable::Clock);
            recorder.random();
            assert!(recorder.marked(7, 7, b"/.blaze-env/HOME"));
            assert!(recorder.marked(7, 7, b"/.blaze-test/\xfftests"));
            recorder.read(7, "/ws/a");
            assert!(recorder.marked(7, 7, b"/.blaze-test/"));
            _ = recorder.scope_of(7);
        };
        seen(&mut recorder);
        let written = std::fs::read_to_string(&live).unwrap();
        blaze_alloc_count::assert_at_most(0, "a line the recorder has seen", || {
            for _ in 0..100 {
                seen(&mut recorder);
            }
        });
        assert_eq!(std::fs::read_to_string(&live).unwrap(), written, "no line twice");
        assert_eq!(recorder.live_failed(), None);
        _ = std::fs::remove_dir_all(&dir);
    }

    /// A name that is not UTF-8 is read as `String::from_utf8_lossy` reads it, into the room
    /// kept.
    #[test]
    fn a_name_is_read_as_lossy_text_into_its_room() {
        let names: [&[u8]; 6] = [b"", b"tests::a", b"\xff", b"a\xffb\xfe", b"\xe2\x82", "é".as_bytes()];
        let mut out = String::with_capacity(64);
        for name in names {
            blaze_alloc_count::assert_at_most(0, "a name read as text", || lossy_into(name, &mut out));
            assert_eq!(out, String::from_utf8_lossy(name), "{name:?}");
        }
    }

    #[test]
    fn blaze_sandbox_off_or_0_turns_sandboxes_off() {
        let with = |vars: &'static [(&str, &str)]| {
            on_with(|name| vars.iter().find(|it| it.0 == name).map(|it| it.1.to_owned()))
        };
        assert!(with(&[]) && with(&[("BLAZE_SANDBOX", "on")]));
        assert!(!with(&[("BLAZE_SANDBOX", "off")]) && !with(&[("BLAZE_SANDBOX", "0")]));
    }

    // The paths a sandbox records are a Unix machine's: there is one on Linux and macOS alone.
    #[cfg(unix)]
    #[test]
    fn a_container_is_known_by_its_marks() {
        let root =
            std::env::temp_dir().join(format!("blaze-container-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proc/1")).unwrap();
        std::fs::create_dir_all(root.join("run")).unwrap();
        assert_eq!(container(&root, None), None);
        assert_eq!(container(&root, Some("")), None);
        assert_eq!(container(&root, Some("podman")).as_deref(), Some("`container=podman`"));
        std::fs::write(root.join("proc/1/cgroup"), "0::/\n").unwrap();
        assert_eq!(container(&root, None), None);
        std::fs::write(root.join("proc/1/cgroup"), "12:pids:/kubepods/burstable/pod1/abc\n")
            .unwrap();
        assert_eq!(container(&root, None).as_deref(), Some("/proc/1/cgroup naming kubepods"));
        std::fs::write(root.join("run/.containerenv"), "").unwrap();
        assert_eq!(container(&root, None).as_deref(), Some("/run/.containerenv"));
        std::fs::write(root.join(".dockerenv"), "").unwrap();
        let known = container(&root, Some("podman")).unwrap();
        assert_eq!(known, "/.dockerenv");
        let hint = container_hint(&known);
        assert!(hint.contains("container (/.dockerenv)"), "{hint}");
        assert!(hint.contains("--security-opt seccomp=unconfined"), "{hint}");
        assert!(hint.contains("`BLAZE_SANDBOX=off`"), "{hint}");
        _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_the_read_set_names_may_be_connected_to_and_nothing_below_it() {
        let spec = Spec { readable: vec!["/req/1/s".into()], ..Spec::default() };
        let recorder = Recorder::new(&spec, &["/tmp"]);
        assert!(recorder.may_connect("/req/1/s"));
        assert!(!recorder.may_connect("/req/1/s/x") && !recorder.may_connect("/req/1/t"));
    }

    // The paths a sandbox records are a Unix machine's: there is one on Linux and macOS alone.
    #[cfg(unix)]
    #[test]
    fn a_record_is_its_lines_in_order_once() {
        let spec = Spec {
            overlays: vec!["/ws".into()],
            not_inputs: vec!["/ws/target/t".into(), "/bin/t".into()],
            record_reads: true,
            ..Spec::default()
        };
        let mut recorder = Recorder::new(&spec, &["/proc", "/tmp"]);
        recorder.read(1, "/etc/hosts");
        recorder.read(1, "/bin/t");
        recorder.read(1, "/ws/target/t");
        assert!(recorder.marked(7, 1, b"/.blaze-test/tests::a"));
        recorder.read(7, "/ws/data.txt");
        recorder.read(7, "/ws/data.txt");
        recorder.read(7, "/proc/self/maps");
        recorder.read(7, "/bin/t");
        recorder.refused_write(7, "/home/u/.config/x");
        recorder.refused_net(1, "1.2.3.4:443");
        assert!(recorder.marked(7, 1, b"/.blaze-test/"));
        recorder.read(7, "/etc/passwd");
        recorder.read(7, "/ws/a\tb");
        assert!(!recorder.marked(7, 1, b"/etc/x"));
        assert!(!recorder.marked(7, 1, b"/.blaze-env/HOME"), "no interposer, no variables");
        recorder.listed(7, "/ws/src");
        recorder.untraceable(7, blaze_core::Untraceable::Random);
        assert!(recorder.may_write("/ws/target/x") && recorder.may_write("/tmp"));
        assert!(!recorder.may_write("/wsx") && !recorder.may_write("/etc/x"));
        assert!(recorder.may_connect("/tmp/s.sock") && recorder.may_connect("/ws/target/s"));
        assert!(!recorder.may_connect("/proc/1/root/run/docker.sock"));
        assert!(!recorder.may_connect("/run/docker.sock") && !recorder.may_connect("/etc/s"));
        let dir = std::env::temp_dir().join(format!("blaze-record-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("record");
        recorder.write(&file);
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            text,
            "R\t\t/etc/hosts\nR\t\t/etc/passwd\nR\t\t/ws/src\nR\ttests::a\t/bin/t\n\
             R\ttests::a\t/ws/data.txt\nW\ttests::a\t/home/u/.config/x\nN\t\t1.2.3.4:443\n\
             X\t\ta path or a name with a tab or a line break in it\n",
            "the program is no input of the run's own process, but is of a test that reads it"
        );
        let lines = read_record(&file).unwrap();
        assert_eq!(lines.len(), 8);
        assert_eq!(
            lines[4],
            Line { reached: Reached::Read, test: "tests::a".into(), what: "/ws/data.txt".into() }
        );
        _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_live_record_has_each_line_once_as_it_is_seen() {
        let dir = std::env::temp_dir().join(format!("blaze-live-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("live");
        std::fs::write(&live, "E\t\t*\n").unwrap();
        let spec = Spec { overlays: vec!["/ws".into()], record_reads: true, ..Spec::default() };
        let mut recorder = Recorder::new(&spec, &["/tmp"]);
        recorder.live(&live).unwrap();
        recorder.interposed();
        assert!(recorder.marked(7, 7, b"/.blaze-test/tests::a"));
        recorder.read(7, "/ws/a");
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "E\t\t*\nR\ttests::a\t/ws/a\n");
        recorder.read(7, "/ws/a");
        recorder.read(7, "/tmp/scratch");
        assert!(recorder.marked(7, 7, b"/.blaze-env/HOME"));
        assert!(recorder.marked(8, 7, b"/.blaze-clock"));
        recorder.listed(7, "/ws");
        recorder.untraceable(8, blaze_core::Untraceable::Network);
        let (lines, used) = trace::parse(&std::fs::read_to_string(&live).unwrap()).unwrap();
        assert_eq!(used, std::fs::read_to_string(&live).unwrap().len());
        let texts: Vec<String> = lines.iter().map(trace::Line::text).collect();
        assert_eq!(
            texts,
            [
                "E\t\t*\n",
                "R\ttests::a\t/ws/a\n",
                "E\ttests::a\tHOME\n",
                "T\t\tclock\n",
                "D\ttests::a\t/ws\n",
                "T\t\tnetwork\n",
            ]
        );
        assert_eq!(recorder.live_failed(), None);
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_program_that_never_says_it_loaded_the_interposer_is_not_traced() {
        let spec = Spec { record_reads: true, ..Spec::default() };
        let mut recorder = Recorder::new(&spec, &[]);
        recorder.interposed();
        assert!(recorder.marked(10, 10, b"/.blaze-test/tests::b"));
        recorder.runs(10, "/bin/loads", recorder.scope_of(10));
        recorder.runs(11, "/bin/static", recorder.scope_of(11));
        assert!(recorder.marked(10, 10, b"/.blaze-loaded"));
        recorder.ended();
        let lines: Vec<_> = recorder.lines().collect();
        assert_eq!(
            lines,
            [trace::Line::new(
                Seen::Incomplete,
                "",
                "/bin/static ran without the interposer, so what it read is not known",
            )]
        );
    }

    #[test]
    fn programs_never_traced_are_told_in_process_id_order() {
        let spec = Spec { record_reads: true, ..Spec::default() };
        let mut recorder = Recorder::new(&spec, &[]);
        recorder.interposed();
        let processes = [900u64, 3, 70, 1 << 40, 12, 5000];
        for process in processes {
            recorder.runs(process, &format!("/bin/p{process}"), Scope::NONE);
        }
        recorder.ended();
        let mut sorted = processes;
        sorted.sort();
        let want: Vec<_> = sorted
            .iter()
            .map(|it| {
                let why = format!("/bin/p{it} ran without the interposer, so what it read is not known");
                trace::Line::new(Seen::Incomplete, "", &why)
            })
            .collect();
        assert_eq!(recorder.lines().collect::<Vec<_>>(), want);
    }

    #[test]
    fn a_new_process_starts_in_the_scope_it_is_given_and_a_thread_once() {
        let mut recorder = Recorder::new(&Spec { record_reads: true, ..Spec::default() }, &[]);
        assert!(!recorder.knows(20));
        let (c, d) = (recorder.scope_named("tests::c"), recorder.scope_named("tests::d"));
        recorder.starts(20, c);
        assert!(recorder.knows(20));
        assert_eq!(recorder.scope(20), "tests::c");
        recorder.starts(20, d);
        assert_eq!(recorder.scope(20), "tests::c", "a thread seen before keeps its scope");
        assert!(recorder.marked(20, 20, b"/.blaze-test/"));
        assert_eq!(recorder.scope(20), "");
    }

    #[cfg(unix)]
    #[test]
    fn a_path_is_tidied_into_a_buffer_as_it_is_tidied() {
        use std::os::unix::ffi::OsStrExt as _;
        let paths: [&[u8]; 12] = [
            b"",
            b"/",
            b"//",
            b"/a/./b//c/",
            b"./a",
            b"a/../b",
            b".",
            b"./",
            b"/./.",
            b"a//b/.",
            b"/x/\xff\xfe/y",
            b"\xe2/z",
        ];
        let mut out = String::from("left over");
        for path in paths {
            tidy_into(path, &mut out);
            assert_eq!(out, tidy(Path::new(std::ffi::OsStr::from_bytes(path))), "{path:?}");
        }
    }

    #[test]
    fn a_name_is_kept_once() {
        let mut names = Names::default();
        let ids = ["", "/ws/a", "tests::a", "/ws/a", ""].map(|it| names.intern(it));
        assert_eq!(ids, [0, 1, 2, 1, 0]);
        assert_eq!([names.get(0), names.get(1), names.get(2)], ["", "/ws/a", "tests::a"]);
        assert_eq!(names.text, "/ws/atests::a");
    }

    #[test]
    fn a_line_is_kept_once_and_one_that_cant_be_written_is_an_x_line() {
        let mut recorder = Recorder::new(&Spec { record_reads: true, ..Spec::default() }, &[]);
        assert!(recorder.marked(30, 30, b"/.blaze-test/tests::e"));
        for _ in 0..3 {
            recorder.read(30, "/ws/a");
            recorder.read(31, "/ws/a");
        }
        recorder.read(30, "/ws/b\tc");
        recorder.read(30, "/ws/d\ne");
        assert!(recorder.marked(32, 32, b"/.blaze-test/tests\tf"));
        recorder.read(32, "/ws/a");
        let lines: Vec<_> = recorder.lines().collect();
        assert_eq!(
            lines,
            [
                trace::Line::new(Seen::Read, "tests::e", "/ws/a"),
                trace::Line::new(Seen::Read, "", "/ws/a"),
                trace::Line::new(Seen::Read, "tests::e", "/ws/b\tc"),
                trace::Line::new(Seen::Read, "tests\tf", "/ws/a"),
            ]
        );
    }
}
