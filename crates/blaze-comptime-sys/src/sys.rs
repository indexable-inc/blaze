//! THE CORE: the system layer's state and its one machine (C5), sans-IO. child.rs's C
//! functions are thin: each turns its C arguments into one call here, and when the call needs
//! the host, drives [`Sys::next_effect`] and [`Sys::step`] until [`Sys::state`] is
//! [`SysState::Done`], then takes [`Sys::result`]. Nothing here touches memory it was not
//! handed, a descriptor of the real process, or a clock.
//!
//! What the child holds (comptime-native.md, "The system layer"):
//!
//! - DESCRIPTORS: a table of at most [`Limits::most_fds`] numbers, each naming an open
//!   description (shared by `dup`, so an offset is shared as on the real system). A file opened
//!   for reading is looked up ONCE with [`Want::Read`] and held whole: `read`, `pread`, `lseek`
//!   and `fstat` are answered from those bytes, and nothing crosses again. A file is at most
//!   [`Limits::most_file`] bytes and all held bytes at most [`Limits::most_held`]: past either,
//!   `open` fails with `EFBIG` (the lookup is on the host's read list all the same, with what it
//!   saw, so a change to the file still reruns the call). A directory opened for reading is a
//!   stub until `fdopendir` lists it. A file opened for writing (the output root only: the host
//!   refuses the rest with `EROFS` and a finding) keeps a shadow of its bytes so `O_RDWR`,
//!   `O_APPEND` and `fstat` are answered here; each write crosses as `WriteFile`.
//! - DIRECTORY STREAMS: a listing, asked once ([`Want::List`]), served `.`, `..`, then the
//!   names in the host's (sorted) order; an entry's type is unknown (`DT_UNKNOWN`), as a listing
//!   carries none, so a caller that needs it stats, which is a recorded lookup.
//! - THE ENVIRONMENT: each `getenv` of a name not yet asked crosses as `Env`, so the host
//!   records the fact (present with its digest, or asked-and-absent: the decision of
//!   2026-11-20, so setting it later reruns the call); `setenv` and `unsetenv` change a local
//!   overlay only. Every answer is its own allocation, so a pointer `getenv` returned stays
//!   valid until that name is set again, as POSIX allows.
//! - THE WORKING DIRECTORY: asked once at the start; `chdir` is a lookup; every path that
//!   crosses is absolute (the cwd joined, `.` and `..` resolved lexically: a root holds no link).
//! - SPAWNED COMMANDS: std's `Command` on posix_spawn, emulated: `pipe` makes a local pipe,
//!   file actions are recorded, `posix_spawnp` crosses ONCE as `Spawn` and its answer fills the
//!   pipes its actions put on 1 and 2 (a stream left inherited is written to the run's own
//!   stream with `Write`); `poll`, `read` and `waitpid` are then local.

#![blaze::sans_io]

use blaze_comptime::{
    Answer, Call, ClockKind, FileKind, Guest, GuestEffect, GuestError, GuestEvent, GuestOpts, Nanos, Stream, SysError, Want,
    WireLimits, Words,
};
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{AVec, Alloc, Vec};

use crate::errno::{Errno, Os};
use crate::plant::SysPlant;

/// The caps of one child's layer (D165: a field each).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Descriptors open at once (`EMFILE` past it).
    pub most_fds: u32 = 256,
    /// The bytes of one file held open (`EFBIG` past it).
    pub most_file: u64 = 64 << 20,
    /// The bytes of every file held open together (`EFBIG` past it).
    pub most_held: u64 = 512 << 20,
    /// The bytes of one path (`ENAMETOOLONG` past it), the wire's name cap.
    pub most_path: u32 = 4096,
    /// Variables asked or set (`ENOMEM` past it, from `setenv`; a fault from `getenv`).
    pub most_env: u32 = 4096,
    /// Commands spawned in one run (`EAGAIN` past it).
    pub most_spawns: u32 = 256,
    /// The page size `sysconf(_SC_PAGESIZE)` answers: the system's (16 KiB on macOS arm64; the
    /// kernel's on Linux, which the launcher reads from the auxiliary vector).
    pub page: u64,
}

/// The system's numbers the layer reads in C arguments and writes in answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Consts {
    pub o_accmode: i32,
    pub o_wronly: i32,
    pub o_rdwr: i32,
    pub o_append: i32,
    pub o_creat: i32,
    pub o_trunc: i32,
    pub o_excl: i32,
    pub o_directory: i32,
    pub at_fdcwd: i32,
    pub at_removedir: i32,
    pub map_shared: i32,
    pub map_private: i32,
    pub map_fixed: i32,
    pub map_anon: i32,
    /// Flags allowed beside `MAP_PRIVATE | MAP_ANON`: `MAP_NORESERVE`, and `MAP_STACK` on Linux.
    pub map_harmless: i32,
    pub f_dupfd_cloexec: i32,
    pub sc_pagesize: i32,
    pub sc_nprocessors_conf: i32,
    pub sc_nprocessors_onln: i32,
    pub clock_realtime: i32,
    /// Every clock id read as the monotonic clock (monotonic, raw, uptime, boot, CPU time).
    pub clocks_monotonic: &'static [i32],
}

/// macOS arm64 (`sys/fcntl.h`, `sys/mman.h`, `unistd.h`, `time.h`).
pub const MAC: Consts = Consts {
    o_accmode: 3,
    o_wronly: 1,
    o_rdwr: 2,
    o_append: 0x8,
    o_creat: 0x200,
    o_trunc: 0x400,
    o_excl: 0x800,
    o_directory: 0x100000,
    at_fdcwd: -2,
    at_removedir: 0x80,
    map_shared: 0x1,
    map_private: 0x2,
    map_fixed: 0x10,
    map_anon: 0x1000,
    map_harmless: 0x40,
    f_dupfd_cloexec: 67,
    sc_pagesize: 29,
    sc_nprocessors_conf: 57,
    sc_nprocessors_onln: 58,
    clock_realtime: 0,
    // CLOCK_MONOTONIC_RAW, _RAW_APPROX, CLOCK_MONOTONIC, CLOCK_UPTIME_RAW, _RAW_APPROX,
    // CLOCK_PROCESS_CPUTIME_ID, CLOCK_THREAD_CPUTIME_ID.
    clocks_monotonic: &[4, 5, 6, 8, 9, 12, 16],
};

/// Linux arm64, glibc (`asm-generic/fcntl.h` as arm64 has it, `asm-generic/mman.h`).
pub const LINUX: Consts = Consts {
    o_accmode: 3,
    o_wronly: 1,
    o_rdwr: 2,
    o_append: 0x400,
    o_creat: 0x40,
    o_trunc: 0x200,
    o_excl: 0x80,
    o_directory: 0x4000,
    at_fdcwd: -100,
    at_removedir: 0x200,
    map_shared: 0x1,
    map_private: 0x2,
    map_fixed: 0x10,
    map_anon: 0x20,
    map_harmless: 0x4000 | 0x20000,
    f_dupfd_cloexec: 1030,
    sc_pagesize: 30,
    sc_nprocessors_conf: 83,
    sc_nprocessors_onln: 84,
    clock_realtime: 0,
    // CLOCK_MONOTONIC, _PROCESS_CPUTIME_ID, _THREAD_CPUTIME_ID, _MONOTONIC_RAW,
    // _MONOTONIC_COARSE, _BOOTTIME (CLOCK_REALTIME_COARSE, 5, is the wall clock: see `clock`).
    clocks_monotonic: &[1, 2, 3, 4, 6, 7],
};

pub const fn consts(os: Os) -> &'static Consts {
    match os {
        Os::Mac => &MAC,
        Os::Linux => &LINUX,
    }
}

pub const PROT_READ: i32 = 1;
pub const PROT_WRITE: i32 = 2;

/// Whether an `mmap` may reach the process's own definition: anonymous private memory only,
/// readable or writable or neither, at an address the kernel picks. Refused: a file mapping
/// (there is no real descriptor), `MAP_SHARED` (nothing is shared with anyone), `MAP_FIXED`
/// (it could replace the launcher's own pages), `MAP_JIT` and `PROT_EXEC` (code comes from
/// records only), each with `EPERM` and a finding (child.rs).
pub fn mmap_allowed(os: Os, prot: i32, flags: i32, fd: i32) -> Result<(), Errno> {
    let c = consts(os);
    let base = c.map_private | c.map_anon;
    if fd != -1 || prot & !(PROT_READ | PROT_WRITE) != 0 || flags & base != base || flags & !(base | c.map_harmless) != 0 {
        return Err(Errno::Perm);
    }
    Ok(())
}

/// Whether an `mprotect` may reach the process's own definition: no `PROT_EXEC`.
pub fn mprotect_allowed(prot: i32) -> Result<(), Errno> {
    if prot & !(PROT_READ | PROT_WRITE) != 0 { Err(Errno::Perm) } else { Ok(()) }
}

/// A descriptor's number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Fd(pub i32);

/// What `fstat` and `stat` answer, before child.rs lays it out as the system's `struct stat`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StatOut {
    pub kind: Kind,
    pub len: u64,
    /// The first eight bytes of the digest of the path's one spelling: equal for one path on
    /// every host, and distinct paths differ (std's `same_file` and `walkdir` compare these).
    pub ino: u64,
}

/// A file's type as `st_mode` holds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    File,
    Dir,
    /// A standard stream or `/dev/null` (`S_IFCHR`).
    Char,
    /// A pipe (`S_IFIFO`).
    Fifo,
}

/// One directory entry (`readdir`): its name, NUL-ended, borrowed from the stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DirEnt<'s> {
    pub name: &'s [u8],
    pub ino: u64,
    /// The entry's place, for `d_seekoff` and `telldir`.
    pub at: u64,
}

/// What a finished call answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ret {
    /// A count, a descriptor, a pid or zero.
    Int(i64),
    Nanos(u64),
    Stat(StatOut),
    /// `getenv`: the index of the answer ([`Sys::env_value`]), or `None` when unset.
    Env(Option<u32>),
    /// The bytes are in [`Sys::scratch`] (random bytes, a resolved path).
    Scratch,
    /// `getcwd`, `environ`: answered from the layer's state, read with its accessor.
    State,
}

/// The layer's state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SysState {
    /// `Hello` and `Cwd` not yet answered.
    Starting,
    /// No call in flight.
    Ready,
    /// A call is with the host.
    Asking,
    /// A call is finished: [`Sys::result`] takes its answer.
    Done,
    /// `exit` or `abort` sent: nothing more crosses, and the launcher ends the process.
    Ended { code: i32 },
    /// The protocol broke ([`Fault`]): the launcher ends the process.
    Broken,
}

/// Why the layer cannot go on: the run ends, and the launcher reports it (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// The guest machine refused a frame or a call.
    Guest(GuestError),
    /// A call while another is in flight, or a step with none.
    Busy { state: SysState },
    /// The host answered `Hello` with another protocol.
    Protocol { host: u32 },
    /// More variables asked than [`Limits::most_env`]: `getenv` has no failure value but
    /// NULL, and NULL would be a lie ("unset"), so the run ends.
    EnvFull { most: u32 },
    /// A wait no thread can end: `poll` with no descriptor ready and no timeout, a `futex`
    /// wait on a word that holds the value waited for, `pthread_join` of a thread.
    Deadlock,
}

impl From<GuestError> for Fault {
    fn from(error: GuestError) -> Fault {
        Fault::Guest(error)
    }
}

/// One open description.
enum Desc<'a> {
    /// 0, 1, 2 as inherited: reading 0 is the end of input; 1 and 2 cross as `Write`.
    Std(Option<Stream>),
    /// `/dev/null`: reads end, writes vanish.
    Null,
    File { path: AVec<'a, u8>, bytes: AVec<'a, u8>, at: u64 },
    /// A directory opened by `open`: `read` is `EISDIR`; `fdopendir` lists it.
    DirStub { path: AVec<'a, u8> },
    /// A directory stream: `.` and `..`, then `names` (NUL-ended words), from `next`
    /// (`byte_pos` tracks the next unread byte in `names`; `dir_ino` caches `ino_of(&path)`).
    Dir { path: AVec<'a, u8>, names: AVec<'a, u8>, next: u64, byte_pos: usize, dir_ino: u64 },
    /// A file in the output root: each write crosses; `shadow` is its bytes as written.
    Out { path: AVec<'a, u8>, shadow: AVec<'a, u8>, at: u64, append: bool, read: bool },
    PipeRead { pipe: u32 },
    PipeWrite { pipe: u32 },
}

struct Open<'a> {
    desc: Desc<'a>,
    refs: u32,
}

/// A local pipe.
struct Pipe<'a> {
    bytes: AVec<'a, u8>,
    at: usize,
    writers: u32,
    readers: u32,
}

/// One answered variable: `NAME=value\0` in its own allocation, or an unset name.
struct Var<'a> {
    text: AVec<'a, u8>,
    name_len: u32,
    set: bool,
}

/// A spawn's recorded file actions (`posix_spawn_file_actions_*`).
struct Actions<'a> {
    /// `(fd, newfd)` in order.
    dup2: AVec<'a, (i32, i32)>,
    chdir: Option<AVec<'a, u8>>,
}

/// A spawned command, after its one crossing.
#[derive(Clone, Copy)]
struct Child {
    pid: i32,
    status: i32,
    reaped: bool,
}

/// The chain of steps one C call takes across the wire.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pending {
    None,
    Hello,
    Cwd,
    /// `open` for reading: the lookup.
    OpenRead,
    /// `open` for writing, step one: the file's bytes (or its absence).
    OpenWriteLook { flags: i32 },
    /// `open` for writing: creating/truncating the file (`truncated`: length is already 0).
    OpenWriteCreate { flags: i32, truncated: bool },
    /// `open` with `O_TRUNC`: the length set to zero; `fd` is already installed.
    OpenWriteTruncate { fd: i32 },
    /// `fdopendir` of a stub, or `open` with `O_DIRECTORY` (`opened`: the descriptor was made
    /// for this call, and is closed again if the lookup fails).
    List { fd: i32, opened: bool },
    Stat,
    Chdir,
    Readlink,
    Realpath,
    Getenv,
    Environ,
    Args,
    Clock,
    /// `mach_wait_until`: the clock, then a sleep to the deadline.
    WaitClock { deadline: u64 },
    Random,
    Done { ret: i64 },
    /// A write into an output file at `at`, `len` bytes.
    WriteOut { open: u32, len: usize, at: u64, moves: bool },
    SetLen { open: u32, len: u64 },
    Thread,
    Socket,
    Denied { errno: Errno },
    Spawn { actions: Option<u32> },
    /// A spawned command's inherited stream written to the run's own.
    SpawnForward { pid: i32, err_left: bool },
}

/// [`Sys::new`]'s arguments.
pub struct SysOpts<'a> {
    pub os: Os,
    pub limits: Limits,
    pub wire: WireLimits = WireLimits::SHIPPED,
    pub alloc: Alloc<'a>,
}

/// The layer.
pub struct Sys<'a, P: Planted<SysPlant> = NoPlant> {
    os: Os,
    limits: Limits,
    alloc: Alloc<'a>,
    plant: P,
    guest: Guest<'a>,
    state: SysState,
    pending: Pending,
    result: Option<Result<Ret, Errno>>,
    fds: AVec<'a, Option<u32>>,
    opens: AVec<'a, Option<Open<'a>>>,
    held: u64,
    pipes: AVec<'a, Pipe<'a>>,
    vars: AVec<'a, Var<'a>>,
    env_all: Option<AVec<'a, u8>>,
    args: Option<AVec<'a, u8>>,
    cwd: AVec<'a, u8>,
    actions: AVec<'a, Option<Actions<'a>>>,
    children: AVec<'a, Child>,
    /// The path of the call in flight, absolute and lexically resolved.
    path: AVec<'a, u8>,
    /// A second path (`rename`'s target), and the spare a join is built in.
    path2: AVec<'a, u8>,
    scratch: AVec<'a, u8>,
    /// The spawn in flight's answer: its stdout then stderr, and where they part.
    spawned: AVec<'a, u8>,
    spawned_split: usize,
}

impl<'a> Sys<'a> {
    pub fn new(opts: SysOpts<'a>) -> Sys<'a> {
        Sys::new_planted(opts, NoPlant)
    }
}

fn ino_of(path: &[u8]) -> u64 {
    let digest = blaze_blake3::derive_key("blaze comptime-sys inode", path);
    // Never 0, which `readdir` callers read as a deleted entry.
    u64::from_le_bytes([digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7]]) | 1
}

/// Split-borrow helper: sends `call` through `guest` so callers can borrow fields of `Sys`
/// (like `self.path`, `self.opens`, or `self.spawned`) without cloning into a temporary `AVec`.
fn ask_guest(
    guest: &mut Guest<'_>,
    state: &mut SysState,
    pending: &mut Pending,
    call: &Call<'_>,
    next: Pending,
) -> Result<(), Fault> {
    if let Err(error) = guest.call(call) {
        *state = SysState::Broken;
        return Err(Fault::Guest(error));
    }
    *pending = next;
    *state = SysState::Asking;
    Ok(())
}

/// Whether `words` (NUL-terminated `NAME=value` entries) contains a variable named `name`.
fn words_has_var(words: &[u8], name: &[u8]) -> bool {
    for word in words.split_inclusive(|b| *b == 0) {
        let entry = word.strip_suffix(&[0]).unwrap_or(word);
        if let Some(eq) = entry.iter().position(|b| *b == b'=')
            && &entry[..eq] == name
        {
            return true;
        }
    }
    false
}

impl<'a, P: Planted<SysPlant>> Sys<'a, P> {
    pub fn new_planted(opts: SysOpts<'a>, plant: P) -> Sys<'a, P> {
        let alloc = opts.alloc;
        let mut sys = Sys {
            os: opts.os,
            limits: opts.limits,
            alloc,
            plant,
            guest: Guest::new(GuestOpts { wire: opts.wire, alloc }),
            state: SysState::Starting,
            pending: Pending::Hello,
            result: None,
            fds: Vec::new_in(alloc),
            opens: Vec::new_in(alloc),
            held: 0,
            pipes: Vec::new_in(alloc),
            vars: Vec::new_in(alloc),
            env_all: None,
            args: None,
            cwd: Vec::new_in(alloc),
            actions: Vec::new_in(alloc),
            children: Vec::new_in(alloc),
            path: Vec::new_in(alloc),
            path2: Vec::new_in(alloc),
            scratch: Vec::new_in(alloc),
            spawned: Vec::new_in(alloc),
            spawned_split: 0,
        };
        for desc in [Desc::Std(None), Desc::Std(Some(Stream::Out)), Desc::Std(Some(Stream::Err))] {
            sys.opens.push(Some(Open { desc, refs: 1 }));
            let at = sys.opens.len() as u32 - 1;
            sys.fds.push(Some(at));
        }
        // The first frame; the guest machine is Ready, so this cannot be refused.
        let _ = sys.guest.call(&Call::Hello { protocol: blaze_comptime::PROTOCOL });
        sys
    }

    pub fn os(&self) -> Os {
        self.os
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn state(&self) -> SysState {
        self.state
    }

    /// The layer waits only on the host, which bounds its answers (host.rs).
    pub fn next_deadline(&self) -> Option<Nanos> {
        None
    }

    /// The frame to send, once per call.
    pub fn next_effect(&mut self) -> Option<GuestEffect<'_>> {
        self.guest.next_effect()
    }

    /// The host's answer to the call in flight.
    pub fn step(&mut self, now: Nanos, event: GuestEvent<'_>) -> Result<(), Fault> {
        if !matches!(self.state, SysState::Starting | SysState::Asking) {
            return Err(self.broke(Fault::Busy { state: self.state }));
        }
        if let Err(error) = self.guest.step(now, event) {
            return Err(self.broke(Fault::Guest(error)));
        }
        let got = match self.take() {
            Ok(got) => got,
            Err(fault) => return Err(self.broke(fault)),
        };
        self.advance(got)
    }

    /// The finished call's answer; the layer is Ready again.
    pub fn result(&mut self) -> Result<Result<Ret, Errno>, Fault> {
        match (self.state, self.result.take()) {
            (SysState::Done, Some(result)) => {
                self.state = SysState::Ready;
                Ok(result)
            }
            (state, _) => Err(Fault::Busy { state }),
        }
    }

    /// Bytes an answer left: random bytes, a resolved path.
    pub fn scratch(&self) -> &[u8] {
        &self.scratch
    }

    /// The working directory, absolute.
    pub fn cwd(&self) -> &[u8] {
        &self.cwd
    }

    /// The answer `getenv` returned: `value\0`, valid until the name is set again.
    pub fn env_value(&self, at: u32) -> Option<&[u8]> {
        let var = self.vars.get(at as usize)?;
        var.set.then(|| &var.text[var.name_len as usize + 1..])
    }

    /// Every variable after [`Sys::environ`]: `NAME=value` words, NUL-ended.
    pub fn environ_words(&self) -> Option<&[u8]> {
        self.env_all.as_deref()
    }

    /// The arguments after [`Sys::args`], `argv[0]` first, NUL-ended words.
    pub fn args_words(&self) -> Option<&[u8]> {
        self.args.as_deref()
    }

    /// The arguments (`_NSGetArgv`, the launcher's `argv`): one `Args` call, the first time.
    pub fn args(&mut self) -> Result<(), Fault> {
        self.begin()?;
        if self.args.is_some() {
            return self.finish(Ok(Ret::State));
        }
        self.ask(&Call::Args, Pending::Args)
    }

    fn broke(&mut self, fault: Fault) -> Fault {
        self.state = SysState::Broken;
        fault
    }

    // ---- asking -------------------------------------------------------------------------

    fn ask(&mut self, call: &Call<'_>, pending: Pending) -> Result<(), Fault> {
        ask_guest(&mut self.guest, &mut self.state, &mut self.pending, call, pending)
    }

    fn begin(&mut self) -> Result<(), Fault> {
        match self.state {
            SysState::Ready => Ok(()),
            state => Err(Fault::Busy { state }),
        }
    }

    fn finish(&mut self, result: Result<Ret, Errno>) -> Result<(), Fault> {
        self.pending = Pending::None;
        self.result = Some(result);
        self.state = SysState::Done;
        Ok(())
    }

    /// The path `raw` (from `at`: a directory descriptor or `AT_FDCWD`) as it crosses:
    /// absolute, resolved, in `self.path`.
    fn resolve_into_path(&mut self, at: i32, raw: &[u8]) -> Result<(), Errno> {
        if raw.is_empty() {
            return Err(Errno::NoEnt);
        }
        if raw.len() > self.limits.most_path as usize {
            return Err(Errno::NameTooLong);
        }
        self.path.clear();
        if raw[0] != b'/' && !self.plant.has(SysPlant::CwdIgnored) {
            if at == consts(self.os).at_fdcwd {
                self.path.extend_from_slice(&self.cwd);
            } else {
                let open = usize::try_from(at).ok().and_then(|n| self.fds.get(n).copied().flatten()).ok_or(Errno::BadF)?;
                match self.opens.get(open as usize).and_then(Option::as_ref).map(|open| &open.desc) {
                    Some(Desc::DirStub { path } | Desc::Dir { path, .. }) => self.path.extend_from_slice(path),
                    Some(_) => return Err(Errno::NotDir),
                    None => return Err(Errno::BadF),
                }
            }
        }
        self.path.push(b'/');
        self.path.extend_from_slice(raw);
        // Resolve lexically: `.` dropped, `..` takes back a segment and stops at `/`.
        self.path2.clear();
        for segment in self.path[..].split(|b| *b == b'/') {
            match segment {
                b"" | b"." => {}
                b".." => {
                    let keep = self.path2.iter().rposition(|b| *b == b'/').unwrap_or(0);
                    self.path2.truncate(keep);
                }
                _ if segment.contains(&0) => return Err(Errno::Inval),
                _ => {
                    self.path2.push(b'/');
                    self.path2.extend_from_slice(segment);
                }
            }
        }
        if self.path2.is_empty() {
            self.path2.push(b'/');
        }
        core::mem::swap(&mut self.path, &mut self.path2);
        if self.path.len() > self.limits.most_path as usize { Err(Errno::NameTooLong) } else { Ok(()) }
    }

    /// The answer, owned: bytes moved into `self.scratch` (one copy, from the guest's frame).
    fn take(&mut self) -> Result<Got, Fault> {
        let answer = self.guest.answer()?;
        let scratch = &mut self.scratch;
        scratch.clear();
        Ok(match answer {
            Answer::Hello { protocol } => Got::Hello(protocol),
            Answer::File { bytes } => {
                scratch.extend_from_slice(bytes);
                Got::Bytes
            }
            Answer::Stat { kind, len } => Got::Stat { kind, len },
            Answer::Listing { names } => {
                scratch.extend_from_slice(names.as_bytes());
                Got::Bytes
            }
            Answer::Value { value } => {
                if let Some(value) = value {
                    scratch.extend_from_slice(value);
                }
                Got::Value { set: value.is_some() }
            }
            Answer::Words { words } => {
                scratch.extend_from_slice(words.as_bytes());
                Got::Bytes
            }
            Answer::Clock { ns } => Got::Clock(ns),
            Answer::Random { bytes } => {
                scratch.extend_from_slice(bytes);
                Got::Bytes
            }
            Answer::Done => Got::Done,
            Answer::Spawned { status, stdout, stderr } => {
                self.spawned.clear();
                self.spawned.extend_from_slice(stdout);
                self.spawned.extend_from_slice(stderr);
                self.spawned_split = stdout.len();
                Got::Spawned { status }
            }
            Answer::Bridge { .. } => Got::Done,
            Answer::Error(error) => Got::Error(error),
        })
    }

    fn scratch_owned(&mut self) -> AVec<'a, u8> {
        let mut out = Vec::new_in(self.alloc);
        core::mem::swap(&mut out, &mut self.scratch);
        out
    }

    fn path_owned(&self) -> AVec<'a, u8> {
        let mut out = Vec::new_in(self.alloc);
        out.extend_from_slice(&self.path);
        out
    }

    /// An output file's descriptor, holding `shadow` (empty when `None`); with `O_TRUNC` and
    /// `!already_truncated`, the file's length is then set to zero, one more crossing.
    fn open_out(&mut self, flags: i32, shadow: Option<AVec<'a, u8>>, already_truncated: bool) -> Result<(), Fault> {
        let c = consts(self.os);
        let shadow = shadow.unwrap_or_else(|| Vec::new_in(self.alloc));
        let len = shadow.len() as u64;
        if len > self.limits.most_file || self.held.checked_add(len).is_none_or(|total| total > self.limits.most_held) {
            return self.finish(Err(Errno::FBig));
        }
        let path = self.path_owned();
        let out = Desc::Out { path, shadow, at: 0, append: flags & c.o_append != 0, read: flags & c.o_accmode == c.o_rdwr };
        let fd = match self.install(out) {
            Ok(fd) => fd,
            Err(errno) => return self.finish(Err(errno)),
        };
        self.held += len;
        if already_truncated || flags & c.o_trunc == 0 {
            return self.finish(Ok(Ret::Int(fd.0 as i64)));
        }
        ask_guest(
            &mut self.guest,
            &mut self.state,
            &mut self.pending,
            &Call::SetLen { path: &self.path, len: 0 },
            Pending::OpenWriteTruncate { fd: fd.0 },
        )
    }

    /// One step of the chain in flight, with the host's answer.
    fn advance(&mut self, got: Got) -> Result<(), Fault> {
        let errno = |error: SysError| Errno::of(error);
        match (self.pending, got) {
            (Pending::Hello, Got::Hello(protocol)) if protocol == blaze_comptime::PROTOCOL => {
                self.ask(&Call::Cwd, Pending::Cwd)?;
                self.state = SysState::Starting;
                Ok(())
            }
            (Pending::Hello, Got::Hello(host)) => Err(self.broke(Fault::Protocol { host })),
            (Pending::Cwd, Got::Value { set: true }) => {
                self.cwd.clear();
                self.cwd.extend_from_slice(&self.scratch);
                self.pending = Pending::None;
                self.state = SysState::Ready;
                Ok(())
            }
            (Pending::OpenRead, Got::Bytes) => {
                let len = self.scratch.len() as u64;
                if len > self.limits.most_file || self.held.checked_add(len).is_none_or(|total| total > self.limits.most_held) {
                    return self.finish(Err(Errno::FBig));
                }
                let bytes = self.scratch_owned();
                let path = self.path_owned();
                match self.install(Desc::File { path, bytes, at: 0 }) {
                    Ok(fd) => {
                        self.held += len;
                        self.finish(Ok(Ret::Int(fd.0 as i64)))
                    }
                    Err(errno) => self.finish(Err(errno)),
                }
            }
            // A directory opened without `O_DIRECTORY` opens, as on the real system; reading it
            // is `EISDIR` and `fdopendir` lists it.
            (Pending::OpenRead, Got::Error(SysError::IsADir)) => {
                let path = self.path_owned();
                let opened = self.install(Desc::DirStub { path });
                self.finish(opened.map(|fd| Ret::Int(fd.0 as i64)))
            }
            (Pending::List { fd, .. }, Got::Bytes) => {
                let alloc = self.alloc;
                let names = self.scratch_owned();
                let Some(open) = self.open_of_mut(Fd(fd)) else { return self.finish(Err(Errno::BadF)) };
                let Desc::DirStub { path } = &mut open.desc else { return self.finish(Err(Errno::BadF)) };
                let path = core::mem::replace(path, Vec::new_in(alloc));
                let dir_ino = ino_of(&path);
                open.desc = Desc::Dir { path, names, next: 0, byte_pos: 0, dir_ino };
                self.finish(Ok(Ret::Int(fd as i64)))
            }
            (Pending::List { fd, opened: true }, Got::Error(error)) => {
                let _ = self.close(Fd(fd));
                self.finish(Err(errno(error)))
            }
            (Pending::OpenWriteLook { flags }, Got::Bytes) => {
                let c = consts(self.os);
                if flags & c.o_excl != 0 && flags & c.o_creat != 0 {
                    return self.finish(Err(Errno::Exist));
                }
                if flags & c.o_trunc != 0 {
                    return self.open_out(flags, None, false);
                }
                let shadow = self.scratch_owned();
                self.open_out(flags, Some(shadow), true)
            }
            (Pending::OpenWriteLook { flags }, Got::Stat { kind, .. }) => {
                let c = consts(self.os);
                if flags & c.o_excl != 0 && flags & c.o_creat != 0 {
                    return self.finish(Err(Errno::Exist));
                }
                match kind {
                    FileKind::Dir => self.finish(Err(Errno::IsDir)),
                    FileKind::File => self.open_out(flags, None, false),
                }
            }
            (Pending::OpenWriteLook { flags }, Got::Error(SysError::NotFound)) if flags & consts(self.os).o_creat != 0 => {
                if flags & consts(self.os).o_trunc != 0 {
                    ask_guest(
                        &mut self.guest,
                        &mut self.state,
                        &mut self.pending,
                        &Call::SetLen { path: &self.path, len: 0 },
                        Pending::OpenWriteCreate { flags, truncated: true },
                    )
                } else {
                    ask_guest(
                        &mut self.guest,
                        &mut self.state,
                        &mut self.pending,
                        &Call::WriteFile { path: &self.path, at: 0, bytes: &[] },
                        Pending::OpenWriteCreate { flags, truncated: true },
                    )
                }
            }
            (Pending::OpenWriteCreate { flags, truncated }, Got::Done) => self.open_out(flags, None, truncated),
            (Pending::OpenWriteTruncate { fd }, Got::Done) => self.finish(Ok(Ret::Int(fd as i64))),
            (Pending::OpenWriteTruncate { fd }, Got::Error(error)) => {
                let _ = self.close(Fd(fd));
                self.finish(Err(errno(error)))
            }
            (Pending::Stat, Got::Stat { kind, len }) => {
                let kind = match kind {
                    FileKind::File => Kind::File,
                    FileKind::Dir => Kind::Dir,
                };
                let ino = ino_of(&self.path);
                self.finish(Ok(Ret::Stat(StatOut { kind, len, ino })))
            }
            (Pending::Chdir, Got::Stat { kind: FileKind::Dir, .. }) => {
                self.cwd.clear();
                self.cwd.extend_from_slice(&self.path);
                self.finish(Ok(Ret::Int(0)))
            }
            (Pending::Chdir, Got::Stat { kind: FileKind::File, .. }) => self.finish(Err(Errno::NotDir)),
            (Pending::Readlink, Got::Stat { .. }) => self.finish(Err(Errno::Inval)),
            (Pending::Realpath, Got::Stat { .. }) => {
                self.scratch.clear();
                self.scratch.extend_from_slice(&self.path);
                self.finish(Ok(Ret::Scratch))
            }
            (Pending::Getenv, Got::Value { set }) => {
                let value = self.scratch_owned();
                let at = self.remember(&value, set)?;
                self.finish(Ok(Ret::Env(set.then_some(at))))
            }
            (Pending::Environ, Got::Bytes) => {
                let words = if self.vars.is_empty() {
                    self.scratch_owned()
                } else {
                    let mut merged = Vec::new_in(self.alloc);
                    for word in self.scratch.split_inclusive(|b| *b == 0) {
                        let entry = word.strip_suffix(&[0]).unwrap_or(word);
                        let key = match entry.iter().position(|b| *b == b'=') {
                            Some(eq) => &entry[..eq],
                            None => entry,
                        };
                        if self.find_var(key).is_none() {
                            merged.extend_from_slice(word);
                        }
                    }
                    for var in self.vars.iter() {
                        if var.set {
                            merged.extend_from_slice(&var.text);
                        }
                    }
                    merged
                };
                self.env_all = Some(words);
                self.finish(Ok(Ret::State))
            }
            (Pending::Args, Got::Bytes) => {
                let words = self.scratch_owned();
                self.args = Some(words);
                self.finish(Ok(Ret::State))
            }
            (Pending::Clock, Got::Clock(ns)) => self.finish(Ok(Ret::Nanos(ns))),
            (Pending::WaitClock { deadline }, Got::Clock(now)) if deadline > now => {
                self.ask(&Call::Sleep { ns: deadline - now }, Pending::Done { ret: 0 })
            }
            (Pending::WaitClock { .. }, Got::Clock(_)) => self.finish(Ok(Ret::Int(0))),
            (Pending::Random, Got::Bytes) => self.finish(Ok(Ret::Scratch)),
            (Pending::Done { ret }, Got::Done) => self.finish(Ok(Ret::Int(ret))),
            (Pending::WriteOut { open, len, at, moves }, Got::Done) => {
                let data_at = at as usize;
                let held = &mut self.held;
                let Some(Some(Open { desc: Desc::Out { shadow, at: offset, .. }, .. })) = self.opens.get_mut(open as usize) else {
                    return self.finish(Err(Errno::BadF));
                };
                let end = data_at + len;
                if shadow.len() < end {
                    *held += (end - shadow.len()) as u64;
                    shadow.resize(end, 0);
                }
                shadow[data_at..end].copy_from_slice(&self.path2[..len]);
                if moves {
                    *offset = end as u64;
                }
                self.finish(Ok(Ret::Int(len as i64)))
            }
            (Pending::SetLen { open, len }, Got::Done) => {
                if let Some(Some(Open { desc: Desc::Out { shadow, .. }, .. })) = self.opens.get_mut(open as usize) {
                    let cur = shadow.len() as u64;
                    if len >= cur {
                        self.held += len - cur;
                    } else {
                        self.held -= cur - len;
                    }
                    shadow.resize(len as usize, 0);
                }
                self.finish(Ok(Ret::Int(0)))
            }
            // The host refuses threads and sockets; the C caller is told what the real system
            // says when it cannot make one.
            (Pending::Thread, _) => self.finish(Err(Errno::Again)),
            (Pending::Socket, _) => self.finish(Err(Errno::Perm)),
            // The table's errno, whatever the host answered (call.rs `Denied`).
            (Pending::Denied { errno }, _) => self.finish(Err(errno)),
            (Pending::Spawn { actions }, Got::Spawned { status }) => self.spawned_answer(actions, status),
            (Pending::SpawnForward { pid, err_left: true }, Got::Done) => {
                let (split, len) = (self.spawned_split, self.spawned.len());
                if split < len {
                    return ask_guest(
                        &mut self.guest,
                        &mut self.state,
                        &mut self.pending,
                        &Call::Write { stream: Stream::Err, bytes: &self.spawned[split..] },
                        Pending::SpawnForward { pid, err_left: false },
                    );
                }
                self.finish(Ok(Ret::Int(pid as i64)))
            }
            (Pending::SpawnForward { pid, .. }, Got::Done) => self.finish(Ok(Ret::Int(pid as i64))),
            (_, Got::Error(error)) => self.finish(Err(errno(error))),
            (pending, _) => Err(self.broke(Fault::Guest(GuestError::Mismatch { on: pending_kind(pending) }))),
        }
    }

    // ---- descriptors --------------------------------------------------------------------

    fn open_of(&self, fd: Fd) -> Option<&Open<'a>> {
        let at = (*self.fds.get(usize::try_from(fd.0).ok()?)?)?;
        self.opens.get(at as usize)?.as_ref()
    }

    fn open_of_mut(&mut self, fd: Fd) -> Option<&mut Open<'a>> {
        let at = (*self.fds.get(usize::try_from(fd.0).ok()?)?)?;
        self.opens.get_mut(at as usize)?.as_mut()
    }

    /// The lowest free number, as POSIX gives (`EMFILE` past the cap).
    fn lowest_free(&self, from: usize) -> Result<usize, Errno> {
        let free = (from..self.fds.len()).find(|at| self.fds[*at].is_none()).unwrap_or(self.fds.len().max(from));
        if free >= self.limits.most_fds as usize { Err(Errno::MFile) } else { Ok(free) }
    }

    fn bind(&mut self, number: usize, open: u32) {
        while self.fds.len() <= number {
            self.fds.push(None);
        }
        self.fds[number] = Some(open);
    }

    fn install(&mut self, desc: Desc<'a>) -> Result<Fd, Errno> {
        let number = self.lowest_free(0)?;
        let open = match self.opens.iter().position(Option::is_none) {
            Some(at) => {
                self.opens[at] = Some(Open { desc, refs: 1 });
                at
            }
            None => {
                self.opens.push(Some(Open { desc, refs: 1 }));
                self.opens.len() - 1
            }
        };
        self.bind(number, open as u32);
        Ok(Fd(number as i32))
    }

    /// `close`.
    pub fn close(&mut self, fd: Fd) -> Result<(), Errno> {
        let number = usize::try_from(fd.0).map_err(|_| Errno::BadF)?;
        let at = self.fds.get_mut(number).and_then(Option::take).ok_or(Errno::BadF)? as usize;
        let Some(open) = self.opens[at].as_mut() else { return Err(Errno::BadF) };
        open.refs -= 1;
        if open.refs == 0 {
            match self.opens[at].take().map(|open| open.desc) {
                Some(Desc::File { bytes, .. } | Desc::Out { shadow: bytes, .. }) => self.held -= bytes.len() as u64,
                Some(Desc::PipeRead { pipe }) => {
                    let p = &mut self.pipes[pipe as usize];
                    p.readers -= 1;
                    if p.readers == 0 {
                        self.held -= (p.bytes.len() - p.at) as u64;
                        p.bytes = Vec::new_in(self.alloc);
                        p.at = 0;
                    }
                }
                Some(Desc::PipeWrite { pipe }) => {
                    let p = &mut self.pipes[pipe as usize];
                    p.writers -= 1;
                    if p.writers == 0 && p.readers == 0 {
                        self.held -= (p.bytes.len() - p.at) as u64;
                        p.bytes = Vec::new_in(self.alloc);
                        p.at = 0;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// `dup` (`min` 0) and `fcntl(F_DUPFD, min)`: the lowest free number at least `min`.
    pub fn dup(&mut self, fd: Fd, min: i32) -> Result<Fd, Errno> {
        let at = self.fds.get(usize::try_from(fd.0).map_err(|_| Errno::BadF)?).copied().flatten().ok_or(Errno::BadF)?;
        let number = self.lowest_free(usize::try_from(min).map_err(|_| Errno::Inval)?)?;
        if self.plant.has(SysPlant::DupCopies) {
            // The plant: a copy of the description, so the offset is no longer shared.
            if let Some(Open { desc: Desc::File { path, bytes, at: offset }, .. }) = self.opens[at as usize].as_ref() {
                let (mut p, mut b) = (Vec::new_in(self.alloc), Vec::new_in(self.alloc));
                p.extend_from_slice(path);
                b.extend_from_slice(bytes);
                let offset = *offset;
                self.opens.push(Some(Open { desc: Desc::File { path: p, bytes: b, at: offset }, refs: 1 }));
                let fresh = self.opens.len() as u32 - 1;
                self.bind(number, fresh);
                return Ok(Fd(number as i32));
            }
        }
        self.bump(at);
        self.bind(number, at);
        Ok(Fd(number as i32))
    }

    fn bump(&mut self, at: u32) {
        if let Some(Some(open)) = self.opens.get_mut(at as usize) {
            open.refs += 1;
            match open.desc {
                Desc::PipeRead { pipe } => self.pipes[pipe as usize].readers += 1,
                Desc::PipeWrite { pipe } => self.pipes[pipe as usize].writers += 1,
                _ => {}
            }
        }
    }

    /// `dup2`.
    pub fn dup2(&mut self, fd: Fd, to: Fd) -> Result<Fd, Errno> {
        let at = self.fds.get(usize::try_from(fd.0).map_err(|_| Errno::BadF)?).copied().flatten().ok_or(Errno::BadF)?;
        let number = usize::try_from(to.0).map_err(|_| Errno::BadF)?;
        if number >= self.limits.most_fds as usize {
            return Err(Errno::BadF);
        }
        if fd == to {
            return Ok(to);
        }
        if self.fds.get(number).copied().flatten().is_some() {
            self.close(to)?;
        }
        self.bump(at);
        self.bind(number, at);
        Ok(to)
    }

    /// `read` (`at` `None`) and `pread`.
    pub fn read(&mut self, fd: Fd, buf: &mut [u8], place: Option<u64>) -> Result<usize, Errno> {
        let past_end = self.plant.has(SysPlant::ReadPastEnd);
        let pipes = &mut self.pipes;
        let held = &mut self.held;
        let number = usize::try_from(fd.0).map_err(|_| Errno::BadF)?;
        let open = self.fds.get(number).copied().flatten().ok_or(Errno::BadF)?;
        let open = self.opens.get_mut(open as usize).and_then(Option::as_mut).ok_or(Errno::BadF)?;
        let (bytes, offset): (&[u8], &mut u64) = match &mut open.desc {
            Desc::Std(None) | Desc::Null => return Ok(0),
            Desc::Std(Some(_)) | Desc::PipeWrite { .. } => return Err(Errno::BadF),
            Desc::DirStub { .. } | Desc::Dir { .. } => return Err(Errno::IsDir),
            Desc::Out { read: false, .. } => return Err(Errno::BadF),
            Desc::File { bytes, at, .. } | Desc::Out { shadow: bytes, at, .. } => (&bytes[..], at),
            Desc::PipeRead { pipe } => {
                if place.is_some() {
                    return Err(Errno::SPipe);
                }
                let pipe = &mut pipes[*pipe as usize];
                let avail = pipe.bytes.len() - pipe.at;
                let n = buf.len().min(avail);
                buf[..n].copy_from_slice(&pipe.bytes[pipe.at..pipe.at + n]);
                pipe.at += n;
                *held -= n as u64;
                if pipe.at == pipe.bytes.len() {
                    pipe.bytes.clear();
                    pipe.at = 0;
                } else if pipe.at >= 4096 && pipe.at * 2 >= pipe.bytes.len() {
                    let rem = pipe.bytes.len() - pipe.at;
                    pipe.bytes.copy_within(pipe.at.., 0);
                    pipe.bytes.truncate(rem);
                    pipe.at = 0;
                }
                // An empty pipe with a writer open would block forever: one thread.
                return if n == 0 && pipe.writers > 0 && !buf.is_empty() { Err(Errno::Again) } else { Ok(n) };
            }
        };
        let from = place.unwrap_or(*offset);
        let start = usize::try_from(from).unwrap_or(usize::MAX).min(bytes.len());
        let end = if past_end {
            start.saturating_add(buf.len()).min(bytes.len() + 1)
        } else {
            start.saturating_add(buf.len()).min(bytes.len())
        };
        let n = end - start;
        if !past_end || end <= bytes.len() {
            buf[..n].copy_from_slice(&bytes[start..end]);
        } else {
            let valid = bytes.len() - start;
            buf[..valid].copy_from_slice(&bytes[start..]);
            buf[valid..n].fill(0);
        }
        if place.is_none() {
            *offset = end as u64;
        }
        Ok(n)
    }

    /// `lseek`: `whence` 0 (`SEEK_SET`), 1 (`SEEK_CUR`), 2 (`SEEK_END`), the same on both.
    pub fn lseek(&mut self, fd: Fd, by: i64, whence: i32) -> Result<u64, Errno> {
        let open = self.open_of_mut(fd).ok_or(Errno::BadF)?;
        let (len, offset) = match &mut open.desc {
            Desc::File { bytes, at, .. } | Desc::Out { shadow: bytes, at, .. } => (bytes.len() as i64, at),
            Desc::Null => return Ok(0),
            Desc::Std(_) | Desc::PipeRead { .. } | Desc::PipeWrite { .. } => return Err(Errno::SPipe),
            Desc::DirStub { .. } | Desc::Dir { .. } => return Err(Errno::IsDir),
        };
        let base = match whence {
            0 => 0,
            1 => *offset as i64,
            2 => len,
            _ => return Err(Errno::Inval),
        };
        let to = base.checked_add(by).filter(|to| *to >= 0).ok_or(Errno::Inval)?;
        *offset = to as u64;
        Ok(to as u64)
    }

    /// `fstat`.
    pub fn fstat(&self, fd: Fd) -> Result<StatOut, Errno> {
        let open = self.open_of(fd).ok_or(Errno::BadF)?;
        Ok(match &open.desc {
            Desc::Std(_) | Desc::Null => StatOut { kind: Kind::Char, len: 0, ino: fd.0 as u64 + 1 },
            Desc::File { path, bytes, .. } | Desc::Out { path, shadow: bytes, .. } => {
                StatOut { kind: Kind::File, len: bytes.len() as u64, ino: ino_of(path) }
            }
            Desc::DirStub { path } => StatOut { kind: Kind::Dir, len: 0, ino: ino_of(path) },
            Desc::Dir { dir_ino, .. } => StatOut { kind: Kind::Dir, len: 0, ino: *dir_ino },
            Desc::PipeRead { pipe } | Desc::PipeWrite { pipe } => StatOut { kind: Kind::Fifo, len: 0, ino: *pipe as u64 + 1 },
        })
    }

    /// The path a descriptor was opened by (`fcntl(F_GETPATH)` on macOS).
    pub fn path_of(&self, fd: Fd) -> Result<&[u8], Errno> {
        match &self.open_of(fd).ok_or(Errno::BadF)?.desc {
            Desc::File { path, .. } | Desc::Out { path, .. } | Desc::DirStub { path } | Desc::Dir { path, .. } => Ok(path),
            _ => Err(Errno::Inval),
        }
    }

    /// Whether `fd` is open (socket calls answer `ENOTSOCK` for it, `EBADF` otherwise).
    pub fn is_open(&self, fd: Fd) -> bool {
        self.open_of(fd).is_some()
    }

    /// `open` and `openat`: a lookup; `Ok(())` when a call is in flight.
    pub fn open(&mut self, at: i32, path: &[u8], flags: i32) -> Result<(), Fault> {
        self.begin()?;
        let c = consts(self.os);
        if let Err(errno) = self.resolve_into_path(at, path) {
            return self.finish(Err(errno));
        }
        if self.path[..] == b"/dev/null"[..] {
            let opened = self.install(Desc::Null);
            return self.finish(opened.map(|fd| Ret::Int(fd.0 as i64)));
        }
        let writes = flags & c.o_accmode != 0 || flags & (c.o_creat | c.o_trunc | c.o_append) != 0;
        if !writes {
            if flags & c.o_directory != 0 {
                // Listed at once: the descriptor is a stream already, as `fdopendir` wants.
                let stub_path = self.path_owned();
                let stub = self.install(Desc::DirStub { path: stub_path });
                return match stub {
                    Ok(fd) => ask_guest(
                        &mut self.guest,
                        &mut self.state,
                        &mut self.pending,
                        &Call::Look { path: &self.path, want: Want::List },
                        Pending::List { fd: fd.0, opened: true },
                    ),
                    Err(errno) => self.finish(Err(errno)),
                };
            }
            return ask_guest(
                &mut self.guest,
                &mut self.state,
                &mut self.pending,
                &Call::Look { path: &self.path, want: Want::Read },
                Pending::OpenRead,
            );
        }
        if flags & c.o_creat != 0 && flags & c.o_trunc != 0 && flags & c.o_excl == 0 {
            // `SetLen(0)` creates if absent and truncates if present in one round trip.
            return ask_guest(
                &mut self.guest,
                &mut self.state,
                &mut self.pending,
                &Call::SetLen { path: &self.path, len: 0 },
                Pending::OpenWriteCreate { flags, truncated: true },
            );
        }
        // `O_TRUNC` (without `O_CREAT`) and `O_CREAT | O_EXCL` never keep existing file bytes:
        // stat instead of reading up to 64 MiB over the wire only to discard it.
        let want = if flags & c.o_trunc != 0 || (flags & c.o_creat != 0 && flags & c.o_excl != 0) {
            Want::Stat
        } else {
            Want::Read
        };
        ask_guest(
            &mut self.guest,
            &mut self.state,
            &mut self.pending,
            &Call::Look { path: &self.path, want },
            Pending::OpenWriteLook { flags },
        )
    }

    /// `opendir`: a listing.
    pub fn opendir(&mut self, path: &[u8]) -> Result<(), Fault> {
        let directory = consts(self.os).o_directory;
        self.open(consts(self.os).at_fdcwd, path, directory)
    }

    /// `fdopendir`: a stub is listed; a stream is itself.
    pub fn fdopendir(&mut self, fd: Fd) -> Result<(), Fault> {
        self.begin()?;
        let Some(open) = usize::try_from(fd.0).ok().and_then(|n| self.fds.get(n).copied().flatten()) else {
            return self.finish(Err(Errno::BadF));
        };
        match self.opens.get(open as usize).and_then(Option::as_ref).map(|open| &open.desc) {
            Some(Desc::Dir { .. }) => self.finish(Ok(Ret::Int(fd.0 as i64))),
            Some(Desc::DirStub { path }) => ask_guest(
                &mut self.guest,
                &mut self.state,
                &mut self.pending,
                &Call::Look { path, want: Want::List },
                Pending::List { fd: fd.0, opened: false },
            ),
            Some(_) => self.finish(Err(Errno::NotDir)),
            None => self.finish(Err(Errno::BadF)),
        }
    }

    /// `readdir`: the next entry, or `None` at the end, in O(1) per entry.
    pub fn readdir(&mut self, fd: Fd) -> Result<Option<DirEnt<'_>>, Errno> {
        let open = self.open_of_mut(fd).ok_or(Errno::BadF)?;
        let Desc::Dir { names, next, byte_pos, dir_ino, .. } = &mut open.desc else { return Err(Errno::BadF) };
        let at = *next;
        let name: &[u8] = match at {
            0 => b".\0",
            1 => b"..\0",
            _ => {
                if *byte_pos >= names.len() {
                    return Ok(None);
                }
                let rest = &names[*byte_pos..];
                let len = rest.iter().position(|b| *b == 0).map_or(rest.len(), |p| p + 1);
                let word = &rest[..len];
                *byte_pos += len;
                word
            }
        };
        *next += 1;
        let ino = (*dir_ino ^ at) | 1;
        Ok(Some(DirEnt { name, ino, at }))
    }

    /// `rewinddir`.
    pub fn rewinddir(&mut self, fd: Fd) -> Result<(), Errno> {
        match &mut self.open_of_mut(fd).ok_or(Errno::BadF)?.desc {
            Desc::Dir { next, byte_pos, .. } => {
                *next = 0;
                *byte_pos = 0;
                Ok(())
            }
            _ => Err(Errno::BadF),
        }
    }

    /// `stat`, `lstat`, `fstatat` (a root has no links: the two are one).
    pub fn stat(&mut self, at: i32, path: &[u8]) -> Result<(), Fault> {
        self.look(at, path, Pending::Stat)
    }

    /// `chdir`.
    pub fn chdir(&mut self, path: &[u8]) -> Result<(), Fault> {
        self.look(consts(self.os).at_fdcwd, path, Pending::Chdir)
    }

    /// `readlink`.
    pub fn readlink(&mut self, at: i32, path: &[u8]) -> Result<(), Fault> {
        self.look(at, path, Pending::Readlink)
    }

    /// `realpath`: the resolved spelling, once the host has seen it exist.
    pub fn realpath(&mut self, path: &[u8]) -> Result<(), Fault> {
        self.look(consts(self.os).at_fdcwd, path, Pending::Realpath)
    }

    fn look(&mut self, at: i32, path: &[u8], pending: Pending) -> Result<(), Fault> {
        self.begin()?;
        if let Err(errno) = self.resolve_into_path(at, path) {
            return self.finish(Err(errno));
        }
        ask_guest(&mut self.guest, &mut self.state, &mut self.pending, &Call::Look { path: &self.path, want: Want::Stat }, pending)
    }

    /// `write`, `pwrite` (`at` given) and `writev` (gathered by the caller).
    pub fn write(&mut self, fd: Fd, bytes: &[u8], at: Option<u64>) -> Result<(), Fault> {
        self.begin()?;
        let Some(number) = usize::try_from(fd.0).ok() else { return self.finish(Err(Errno::BadF)) };
        let Some(open) = self.fds.get(number).copied().flatten() else { return self.finish(Err(Errno::BadF)) };
        let len = bytes.len();
        match self.opens[open as usize].as_mut().map(|open| &mut open.desc) {
            Some(Desc::Std(Some(stream))) => {
                let stream = *stream;
                self.ask(&Call::Write { stream, bytes }, Pending::Done { ret: len as i64 })
            }
            Some(Desc::Null) => self.finish(Ok(Ret::Int(len as i64))),
            Some(Desc::PipeWrite { pipe }) => {
                let pipe = &mut self.pipes[*pipe as usize];
                if pipe.readers == 0 {
                    return self.finish(Err(Errno::Pipe));
                }
                let live = (pipe.bytes.len() - pipe.at) as u64;
                let Some(next_live) = live.checked_add(len as u64) else {
                    return self.finish(Err(Errno::FBig));
                };
                if next_live > self.limits.most_file
                    || self.held.checked_add(len as u64).is_none_or(|total| total > self.limits.most_held)
                {
                    return self.finish(Err(Errno::FBig));
                }
                if pipe.at > 0 {
                    if pipe.at == pipe.bytes.len() {
                        pipe.bytes.clear();
                        pipe.at = 0;
                    } else if pipe.at >= 4096 && pipe.at * 2 >= pipe.bytes.len() {
                        let rem = pipe.bytes.len() - pipe.at;
                        pipe.bytes.copy_within(pipe.at.., 0);
                        pipe.bytes.truncate(rem);
                        pipe.at = 0;
                    }
                }
                pipe.bytes.extend_from_slice(bytes);
                self.held += len as u64;
                self.finish(Ok(Ret::Int(len as i64)))
            }
            Some(Desc::Out { path, shadow, at: offset, append, .. }) => {
                let place = match (at, *append) {
                    (Some(at), _) => at,
                    (None, true) => shadow.len() as u64,
                    (None, false) => *offset,
                };
                let Some(end) = place.checked_add(len as u64) else {
                    return self.finish(Err(Errno::FBig));
                };
                let growth = end.saturating_sub(shadow.len() as u64);
                if end > self.limits.most_file
                    || self.held.checked_add(growth).is_none_or(|total| total > self.limits.most_held)
                {
                    return self.finish(Err(Errno::FBig));
                }
                // The bytes are kept for the shadow until the host says done.
                self.path2.clear();
                self.path2.extend_from_slice(bytes);
                let pending = Pending::WriteOut { open, len, at: place, moves: at.is_none() };
                ask_guest(
                    &mut self.guest,
                    &mut self.state,
                    &mut self.pending,
                    &Call::WriteFile { path, at: place, bytes },
                    pending,
                )
            }
            Some(Desc::DirStub { .. } | Desc::Dir { .. }) => self.finish(Err(Errno::IsDir)),
            Some(_) => self.finish(Err(Errno::BadF)),
            None => self.finish(Err(Errno::BadF)),
        }
    }

    /// `ftruncate`: an output file's length.
    pub fn ftruncate(&mut self, fd: Fd, len: u64) -> Result<(), Fault> {
        self.begin()?;
        let Some(open) = usize::try_from(fd.0).ok().and_then(|n| self.fds.get(n).copied().flatten()) else {
            return self.finish(Err(Errno::BadF));
        };
        match self.opens[open as usize].as_ref().map(|open| &open.desc) {
            Some(Desc::Out { path, shadow, .. }) => {
                let growth = len.saturating_sub(shadow.len() as u64);
                if len > self.limits.most_file
                    || self.held.checked_add(growth).is_none_or(|total| total > self.limits.most_held)
                {
                    return self.finish(Err(Errno::FBig));
                }
                ask_guest(
                    &mut self.guest,
                    &mut self.state,
                    &mut self.pending,
                    &Call::SetLen { path, len },
                    Pending::SetLen { open, len },
                )
            }
            Some(_) => self.finish(Err(Errno::Inval)),
            None => self.finish(Err(Errno::BadF)),
        }
    }

    /// `mkdir`, `unlink`, `rmdir`, `rename`: one call each, the output root's (host.rs).
    pub fn create_dir(&mut self, at: i32, path: &[u8]) -> Result<(), Fault> {
        self.path_call(at, path, |path| Call::CreateDir { path })
    }

    pub fn remove(&mut self, at: i32, path: &[u8]) -> Result<(), Fault> {
        self.path_call(at, path, |path| Call::Remove { path })
    }

    pub fn rename(&mut self, from_at: i32, from: &[u8], to_at: i32, to: &[u8]) -> Result<(), Fault> {
        self.begin()?;
        if let Err(errno) = self.resolve_into_path(to_at, to) {
            return self.finish(Err(errno));
        }
        // Stash resolved `to` in `self.scratch` (unused while sending) so `resolve_into_path`
        // can reuse `self.path` and `self.path2` for `from` without any heap allocation.
        core::mem::swap(&mut self.path, &mut self.scratch);
        if let Err(errno) = self.resolve_into_path(from_at, from) {
            return self.finish(Err(errno));
        }
        ask_guest(
            &mut self.guest,
            &mut self.state,
            &mut self.pending,
            &Call::Rename { from: &self.path, to: &self.scratch },
            Pending::Done { ret: 0 },
        )
    }

    fn path_call(&mut self, at: i32, path: &[u8], call: impl for<'p> Fn(&'p [u8]) -> Call<'p>) -> Result<(), Fault> {
        self.begin()?;
        if let Err(errno) = self.resolve_into_path(at, path) {
            return self.finish(Err(errno));
        }
        ask_guest(&mut self.guest, &mut self.state, &mut self.pending, &call(&self.path), Pending::Done { ret: 0 })
    }

    // ---- the environment ----------------------------------------------------------------

    fn find_var(&self, name: &[u8]) -> Option<usize> {
        self.vars.iter().rposition(|var| &var.text[..var.name_len as usize] == name)
    }

    #[cfg(feature = "child")]
    pub(crate) fn env_needs_host_check(&self, name: &[u8]) -> bool {
        if name.is_empty() || name.contains(&b'=') || name.contains(&0) {
            return false;
        }
        if self.find_var(name).is_some() {
            return false;
        }
        if let Some(words) = &self.env_all
            && words_has_var(words, name)
        {
            return false;
        }
        self.vars.len() < self.limits.most_env as usize
    }

    fn remember(&mut self, value: &[u8], set: bool) -> Result<u32, Fault> {
        if let Some(at) = self.find_var(&self.path2) {
            let var = &mut self.vars[at];
            var.text.clear();
            var.text.extend_from_slice(&self.path2);
            var.text.push(b'=');
            var.text.extend_from_slice(value);
            var.text.push(0);
            var.name_len = self.path2.len() as u32;
            var.set = set;
            return Ok(at as u32);
        }
        if self.vars.len() >= self.limits.most_env as usize {
            return Err(self.broke(Fault::EnvFull { most: self.limits.most_env }));
        }
        // `self.path2` holds the name the call asked (see `getenv`).
        let mut text = Vec::new_in(self.alloc);
        text.extend_from_slice(&self.path2);
        text.push(b'=');
        text.extend_from_slice(value);
        text.push(0);
        let name_len = self.path2.len() as u32;
        self.vars.push(Var { text, name_len, set });
        Ok(self.vars.len() as u32 - 1)
    }

    /// `getenv`: the overlay or an earlier answer, else one `Env` call (the host records the
    /// fact, asked-and-absent included).
    pub fn getenv(&mut self, name: &[u8]) -> Result<(), Fault> {
        self.begin()?;
        if name.is_empty() || name.contains(&b'=') {
            return self.finish(Ok(Ret::Env(None)));
        }
        if let Some(at) = self.find_var(name) {
            if !self.plant.has(SysPlant::EnvAskedTwice) {
                let set = self.vars[at].set;
                return self.finish(Ok(Ret::Env(set.then_some(at as u32))));
            }
        }
        self.path2.clear();
        self.path2.extend_from_slice(name);
        self.ask(&Call::Env { name }, Pending::Getenv)
    }

    /// `setenv` (`value` `Some`) and `unsetenv`: the overlay only; nothing crosses, nothing is
    /// recorded (the run's own value is no fact of the world).
    pub fn setenv(&mut self, name: &[u8], value: Option<&[u8]>, overwrite: bool) -> Result<(), Errno> {
        if name.is_empty() || name.contains(&b'=') || name.contains(&0) {
            return Err(Errno::Inval);
        }
        if let Some(at) = self.find_var(name) {
            if !overwrite && self.vars[at].set {
                return Ok(());
            }
            let var = &mut self.vars[at];
            var.text.clear();
            var.text.extend_from_slice(name);
            var.text.push(b'=');
            var.text.extend_from_slice(value.unwrap_or(&[]));
            var.text.push(0);
            var.name_len = name.len() as u32;
            var.set = value.is_some();
            self.env_all = None;
            return Ok(());
        }
        if !overwrite
            && let Some(words) = &self.env_all
            && words_has_var(words, name)
        {
            return Ok(());
        }
        if self.vars.len() >= self.limits.most_env as usize {
            return Err(Errno::NoMem);
        }
        let mut text = Vec::new_in(self.alloc);
        text.extend_from_slice(name);
        text.push(b'=');
        text.extend_from_slice(value.unwrap_or(&[]));
        text.push(0);
        self.vars.push(Var { text, name_len: name.len() as u32, set: value.is_some() });
        self.env_all = None;
        Ok(())
    }

    /// Every variable (`environ`, `_NSGetEnviron`): one `EnvAll` call, the first time.
    pub fn environ(&mut self) -> Result<(), Fault> {
        self.begin()?;
        if self.env_all.is_some() {
            return self.finish(Ok(Ret::State));
        }
        self.ask(&Call::EnvAll, Pending::Environ)
    }

    // ---- time and chance ----------------------------------------------------------------

    /// `clock_gettime`, `gettimeofday` (`CLOCK_REALTIME`), `mach_absolute_time` (monotonic).
    pub fn clock(&mut self, id: i32) -> Result<(), Fault> {
        self.begin()?;
        let c = consts(self.os);
        let which = if id == c.clock_realtime || (self.os == Os::Linux && id == 5) {
            ClockKind::Wall
        } else if c.clocks_monotonic.contains(&id) {
            ClockKind::Monotonic
        } else {
            return self.finish(Err(Errno::Inval));
        };
        self.ask(&Call::Clock { which }, Pending::Clock)
    }

    /// `nanosleep`, `clock_nanosleep` (relative).
    pub fn sleep(&mut self, ns: u64) -> Result<(), Fault> {
        self.begin()?;
        self.ask(&Call::Sleep { ns }, Pending::Done { ret: 0 })
    }

    /// `mach_wait_until`: the monotonic clock, then a sleep to `deadline`.
    pub fn wait_until(&mut self, deadline: u64) -> Result<(), Fault> {
        self.begin()?;
        self.ask(&Call::Clock { which: ClockKind::Monotonic }, Pending::WaitClock { deadline })
    }

    /// `getentropy` (at most 256), `getrandom`, `arc4random_buf`, `CCRandomGenerateBytes`.
    pub fn random(&mut self, len: u32) -> Result<(), Fault> {
        self.begin()?;
        self.ask(&Call::Random { len }, Pending::Random)
    }

    // ---- refusals and ends --------------------------------------------------------------

    /// A table refusal: `Denied` crosses so the host records the finding; the caller gets
    /// `errno`.
    pub fn denied(&mut self, name: &[u8], errno: Errno) -> Result<(), Fault> {
        self.begin()?;
        self.ask(&Call::Denied { name }, Pending::Denied { errno })
    }

    pub fn thread(&mut self) -> Result<(), Fault> {
        self.begin()?;
        self.ask(&Call::Thread, Pending::Thread)
    }

    pub fn socket(&mut self) -> Result<(), Fault> {
        self.begin()?;
        self.ask(&Call::Socket, Pending::Socket)
    }

    /// `exit`, `_exit` (`code`), `abort` (134, as a shell reports `SIGABRT`).
    pub fn exit(&mut self, code: i32) -> Result<(), Fault> {
        self.begin()?;
        if let Err(error) = self.guest.call(&Call::Exit { code }) {
            return Err(self.broke(Fault::Guest(error)));
        }
        self.state = SysState::Ended { code };
        Ok(())
    }

    /// A proc macro's other calls (the bridge, `Return`) go through the same guest machine.
    pub fn call(&mut self, call: &Call<'_>) -> Result<(), Fault> {
        self.begin()?;
        if let Call::Return { .. } = call {
            if let Err(error) = self.guest.call(call) {
                return Err(self.broke(Fault::Guest(error)));
            }
            self.state = SysState::Ended { code: 0 };
            return Ok(());
        }
        self.ask(call, Pending::Done { ret: 0 })
    }

    /// The bridge's reply to the last [`Sys::call`], read from the guest's frame.
    pub fn bridge_reply(&self) -> Result<&[u8], Fault> {
        match self.guest.answer()? {
            Answer::Bridge { reply } => Ok(reply),
            _ => Err(Fault::Guest(GuestError::Mismatch { on: blaze_comptime::CallKind::Bridge })),
        }
    }

    // ---- pipes and spawned commands ------------------------------------------------------

    /// `pipe`: `(read, write)`.
    pub fn pipe(&mut self) -> Result<(Fd, Fd), Errno> {
        let pipe = match self.pipes.iter().position(|p| p.readers == 0 && p.writers == 0) {
            Some(at) => {
                let p = &mut self.pipes[at];
                p.bytes.clear();
                p.at = 0;
                p.writers = 1;
                p.readers = 1;
                at as u32
            }
            None => {
                self.pipes.push(Pipe { bytes: Vec::new_in(self.alloc), at: 0, writers: 1, readers: 1 });
                self.pipes.len() as u32 - 1
            }
        };
        let read = match self.install(Desc::PipeRead { pipe }) {
            Ok(fd) => fd,
            Err(errno) => {
                let p = &mut self.pipes[pipe as usize];
                p.readers = 0;
                p.writers = 0;
                return Err(errno);
            }
        };
        match self.install(Desc::PipeWrite { pipe }) {
            Ok(write) => Ok((read, write)),
            Err(errno) => {
                let _ = self.close(read);
                self.pipes[pipe as usize].writers = 0;
                Err(errno)
            }
        }
    }

    /// `posix_spawn_file_actions_init`: a handle child.rs writes into the caller's object.
    pub fn actions_new(&mut self) -> u32 {
        let fresh = Actions { dup2: Vec::new_in(self.alloc), chdir: None };
        match self.actions.iter().position(Option::is_none) {
            Some(at) => {
                self.actions[at] = Some(fresh);
                at as u32
            }
            None => {
                self.actions.push(Some(fresh));
                self.actions.len() as u32 - 1
            }
        }
    }

    /// `posix_spawn_file_actions_destroy`: reclaims the recorded action slot and its buffers.
    pub fn actions_destroy(&mut self, actions: u32) -> Result<(), Errno> {
        let slot = self.actions.get_mut(actions as usize).ok_or(Errno::Inval)?;
        if slot.is_none() {
            return Err(Errno::Inval);
        }
        *slot = None;
        Ok(())
    }

    pub fn actions_dup2(&mut self, actions: u32, fd: i32, to: i32) -> Result<(), Errno> {
        self.actions.get_mut(actions as usize).and_then(Option::as_mut).ok_or(Errno::Inval)?.dup2.push((fd, to));
        Ok(())
    }

    pub fn actions_chdir(&mut self, actions: u32, path: &[u8]) -> Result<(), Errno> {
        let mut owned = Vec::new_in(self.alloc);
        owned.extend_from_slice(path);
        self.actions.get_mut(actions as usize).and_then(Option::as_mut).ok_or(Errno::Inval)?.chdir = Some(owned);
        Ok(())
    }

    /// `posix_spawn` and `posix_spawnp`: ONE `Spawn`. `args` are `argv[1..]`, `env` the
    /// `envp` words, both NUL-ended (`Words`).
    pub fn spawn(&mut self, program: &[u8], args: &[u8], env: &[u8], actions: Option<u32>) -> Result<(), Fault> {
        self.begin()?;
        if self.children.len() >= self.limits.most_spawns as usize {
            return self.finish(Err(Errno::Again));
        }
        let (Some(args), Some(env)) = (Words::new(args), Words::new(env)) else { return self.finish(Err(Errno::Inval)) };
        let act = match actions {
            Some(at) => match self.actions.get(at as usize).and_then(Option::as_ref) {
                Some(act) => Some((at, act)),
                None => return self.finish(Err(Errno::Inval)),
            },
            None => None,
        };
        // The command's own stdin: a pipe or a file put on 0 has no wire field, so it is
        // refused by name rather than dropped (rule 3).
        if let Some((_, act)) = act
            && act.dup2.iter().any(|(fd, to)| *to == 0 && !matches!(self.open_of(Fd(*fd)).map(|o| &o.desc), Some(Desc::Null)))
        {
            return self.finish(Err(Errno::NotSup));
        }
        let has_chdir = if let Some((_, act)) = act
            && let Some(path) = &act.chdir
        {
            self.scratch.clear();
            self.scratch.extend_from_slice(path);
            true
        } else {
            false
        };
        if has_chdir {
            let mut raw = Vec::new_in(self.alloc);
            core::mem::swap(&mut raw, &mut self.scratch);
            let res = self.resolve_into_path(consts(self.os).at_fdcwd, &raw);
            core::mem::swap(&mut raw, &mut self.scratch);
            if let Err(errno) = res {
                return self.finish(Err(errno));
            }
            ask_guest(
                &mut self.guest,
                &mut self.state,
                &mut self.pending,
                &Call::Spawn { program, args, env, cwd: &self.path },
                Pending::Spawn { actions },
            )
        } else {
            ask_guest(
                &mut self.guest,
                &mut self.state,
                &mut self.pending,
                &Call::Spawn { program, args, env, cwd: &self.cwd },
                Pending::Spawn { actions },
            )
        }
    }

    /// The pipe a spawn's actions put on `stream`'s number, if any.
    fn pipe_on(&self, actions: Option<u32>, number: i32) -> Option<Option<u32>> {
        let act = self.actions.get(actions? as usize)?.as_ref()?;
        let (fd, _) = act.dup2.iter().rev().find(|(_, to)| *to == number)?;
        Some(match self.open_of(Fd(*fd)).map(|open| &open.desc) {
            Some(Desc::PipeWrite { pipe }) => Some(*pipe),
            _ => None,
        })
    }

    fn spawned_answer(&mut self, actions: Option<u32>, status: i32) -> Result<(), Fault> {
        let pid = 100 + self.children.len() as i32;
        self.children.push(Child { pid, status, reaped: false });
        let split = self.spawned_split;
        let mut inherited = [false; 2];
        for (number, range) in [(1, 0..split), (2, split..self.spawned.len())] {
            match self.pipe_on(actions, number) {
                Some(Some(pipe)) => {
                    let p = &mut self.pipes[pipe as usize];
                    if p.readers > 0 {
                        if p.at > 0 {
                            let rem = p.bytes.len() - p.at;
                            p.bytes.copy_within(p.at.., 0);
                            p.bytes.truncate(rem);
                            p.at = 0;
                        }
                        let slice = &self.spawned[range];
                        p.bytes.extend_from_slice(slice);
                        self.held = self.held.saturating_add(slice.len() as u64);
                    }
                }
                // Put on `/dev/null` or a closed number: gone, as on the real system.
                Some(None) => {}
                None => inherited[number as usize - 1] = true,
            }
        }
        if !inherited[1] {
            self.spawned_split = self.spawned.len();
        }
        if inherited[0] && split > 0 {
            return ask_guest(
                &mut self.guest,
                &mut self.state,
                &mut self.pending,
                &Call::Write { stream: Stream::Out, bytes: &self.spawned[..split] },
                Pending::SpawnForward { pid, err_left: true },
            );
        }
        self.pending = Pending::SpawnForward { pid, err_left: true };
        self.advance(Got::Done)
    }

    /// `waitpid` and `waitid`: the status as `wait` encodes it (`WEXITSTATUS` in bits 8..16; a
    /// negative status from the host is the signal that ended the command).
    pub fn waitpid(&mut self, pid: i32) -> Result<(i32, i32), Errno> {
        let child = self.children.iter_mut().find(|child| (pid == -1 || child.pid == pid) && !child.reaped).ok_or(Errno::Child)?;
        child.reaped = true;
        let encoded = if child.status >= 0 { (child.status & 0xff) << 8 } else { (-child.status) & 0x7f };
        Ok((child.pid, encoded))
    }

    /// `poll`'s answer for one descriptor: `POLLIN` (1) when a read would not wait, `POLLHUP`
    /// (16) at a pipe's end, `POLLOUT` (4) for a write; `POLLNVAL` (32) when closed.
    pub fn poll_one(&self, fd: Fd, events: i16) -> i16 {
        match self.open_of(fd).map(|open| &open.desc) {
            None => 32,
            Some(Desc::PipeRead { pipe }) => {
                let pipe = &self.pipes[*pipe as usize];
                let ready = if pipe.at < pipe.bytes.len() { events & 1 } else { 0 };
                ready | if pipe.writers == 0 { 16 } else { 0 }
            }
            Some(_) => events & (1 | 4),
        }
    }
}

/// An answer, owned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Got {
    Hello(u32),
    /// In `scratch`.
    Bytes,
    Stat { kind: FileKind, len: u64 },
    Value { set: bool },
    Clock(u64),
    Done,
    Spawned { status: i32 },
    Error(SysError),
}

fn pending_kind(pending: Pending) -> blaze_comptime::CallKind {
    use blaze_comptime::CallKind as K;
    match pending {
        Pending::None | Pending::Done { .. } => K::Write,
        Pending::Hello => K::Hello,
        Pending::Cwd => K::Cwd,
        Pending::OpenRead | Pending::OpenWriteLook { .. } | Pending::List { .. } | Pending::Stat => K::Look,
        Pending::Chdir | Pending::Readlink | Pending::Realpath => K::Look,
        Pending::OpenWriteCreate { truncated: true, .. } | Pending::OpenWriteTruncate { .. } | Pending::SetLen { .. } => K::SetLen,
        Pending::OpenWriteCreate { truncated: false, .. } | Pending::WriteOut { .. } => K::WriteFile,
        Pending::Getenv => K::Env,
        Pending::Environ => K::EnvAll,
        Pending::Args => K::Args,
        Pending::Clock | Pending::WaitClock { .. } => K::Clock,
        Pending::Random => K::Random,
        Pending::Thread => K::Thread,
        Pending::Socket => K::Socket,
        Pending::Denied { .. } => K::Denied,
        Pending::Spawn { .. } | Pending::SpawnForward { .. } => K::Spawn,
    }
}
