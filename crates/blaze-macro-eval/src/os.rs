//! The machine as a proc macro sees it on blaze's evaluator: the calls that leave the macro's
//! code for the system (files, the environment, the clock, randomness, raw system calls),
//! answered here and noted as they are made, so that the expansion is keyed by exactly what it
//! read ([`Os::seen`]), as a native sandbox's trace keys a build script's run.
//!
//! The rules are the proc-macro sandbox's, on the real paths: the macro reads below the calling
//! crate's `CARGO_MANIFEST_DIR` and `OUT_DIR` and nothing else, writes nothing, and sets no
//! variable.
//! A path outside them fails with `EACCES` and is noted as refused, which is not an input: the
//! refusal is the same every run. A write, a variable set or a system call no shim knows stops
//! the expansion ([`Stop`]).
//!
//! A `stat` says a file's kind and size and an inode of its real path, and nothing else of the
//! machine (no times, owner or permissions), so that a look at a file says no more than what
//! the trace keys it by: its bytes.
//!
//! The calls the evaluator's own shims answer (writes to stdout and stderr, the threads'
//! `futex` through `syscall`, the monotonic clocks its timed waits run on) are given back to it
//! ([`Answer::Evaluator`]).

mod clock;
mod env;
mod files;
mod random;
pub(crate) mod target;

#[cfg(test)]
mod tests;

use base_db::{CrateOrigin, Db, Env, LangCrateOrigin};
use blaze_usercode::trace::Seen;
use hir_def::{
    FunctionId, HasModule, ItemContainerId, Lookup, attrs::AttrFlags,
    signatures::FunctionSignature,
};

use crate::guest::{Guest, Stop, put_word, word};

use self::target::{Errno, Sys, Target};

/// What a claimed function is: which shim, decided once per function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Shim {
    call: Call,
    /// The macro's target, `None` for one the shims don't model.
    target: Option<Target>,
}

/// Whether a shim answered a call, or gave it back to the evaluator to run as if it were not
/// claimed: a call of a function the evaluator's own shims answer part of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Answer {
    Answered,
    Evaluator,
}

/// What a shim returns: an answer, or why the expansion stops.
type Done = Result<Answer, Stop>;

/// Each function a shim answers, by what it is under every name it has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Call {
    Open,
    Openat,
    Read,
    Pread,
    Readv,
    Close,
    Lseek,
    Fstat,
    Stat,
    Lstat,
    Fstatat,
    Getcwd,
    Realpath,
    Readlink,
    Access,
    Faccessat,
    Opendir,
    Fdopendir,
    Readdir,
    Closedir,
    Dirfd,
    Free,
    Getenv,
    Setenv,
    Unsetenv,
    Putenv,
    /// macOS's `_NSGetEnviron`, and std's `sys::env::unix::environ` on Linux, where it reads
    /// the extern static `environ`: either is what `env::vars` walks.
    Environ,
    ClockGettime,
    Gettimeofday,
    Time,
    MachAbsoluteTime,
    MachTimebaseInfo,
    /// std's `sys::random::hashmap_random_keys`, which `RandomState::new` asks.
    HashmapRandomKeys,
    Getrandom,
    Getentropy,
    Arc4randomBuf,
    CcRandomGenerateBytes,
    SecRandomCopyBytes,
    Syscall,
}

impl Call {
    /// The call an extern function of the symbol `name` is: its `#[link_name]`, else its name.
    /// Each name is one std calls at the pinned nightly on Linux (glibc) or macOS, or `libc`
    /// declares for them.
    fn of_extern(name: &str) -> Option<Call> {
        Some(match name {
            "open" | "open64" => Call::Open,
            "openat" | "openat64" => Call::Openat,
            "read" => Call::Read,
            "pread" | "pread64" => Call::Pread,
            "readv" => Call::Readv,
            "close" | "close$NOCANCEL" => Call::Close,
            "lseek" | "lseek64" => Call::Lseek,
            "fstat" | "fstat64" | "fstat$INODE64" => Call::Fstat,
            "stat" | "stat64" | "stat$INODE64" => Call::Stat,
            "lstat" | "lstat64" | "lstat$INODE64" => Call::Lstat,
            "fstatat" | "fstatat64" | "fstatat$INODE64" => Call::Fstatat,
            "getcwd" => Call::Getcwd,
            "realpath" | "realpath$DARWIN_EXTSN" => Call::Realpath,
            "readlink" => Call::Readlink,
            "access" => Call::Access,
            "faccessat" => Call::Faccessat,
            "opendir" | "opendir$INODE64" => Call::Opendir,
            "fdopendir" | "fdopendir$INODE64" => Call::Fdopendir,
            "readdir" | "readdir64" | "readdir$INODE64" => Call::Readdir,
            "closedir" => Call::Closedir,
            "dirfd" => Call::Dirfd,
            "free" => Call::Free,
            "getenv" => Call::Getenv,
            "setenv" => Call::Setenv,
            "unsetenv" => Call::Unsetenv,
            "putenv" => Call::Putenv,
            "_NSGetEnviron" => Call::Environ,
            "clock_gettime" => Call::ClockGettime,
            "gettimeofday" => Call::Gettimeofday,
            "time" => Call::Time,
            "mach_absolute_time" => Call::MachAbsoluteTime,
            "mach_timebase_info" => Call::MachTimebaseInfo,
            "getrandom" => Call::Getrandom,
            "getentropy" => Call::Getentropy,
            "arc4random_buf" => Call::Arc4randomBuf,
            "CCRandomGenerateBytes" => Call::CcRandomGenerateBytes,
            "SecRandomCopyBytes" => Call::SecRandomCopyBytes,
            "syscall" => Call::Syscall,
            _ => return None,
        })
    }

    /// The call a function of std named `name` in the module `module` is.
    fn of_std(name: &str, module: &str) -> Option<Call> {
        Some(match (name, module) {
            // `sys/random/mod.rs` on macOS, `sys/random/linux.rs` on Linux.
            ("hashmap_random_keys", "random" | "linux") => Call::HashmapRandomKeys,
            ("environ", "unix") => Call::Environ,
            _ => return None,
        })
    }
}

/// One line of what the macro did, its text in [`Trace::text`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Line {
    noted: Noted,
    start: u32,
    end: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Noted {
    Seen(Seen),
    /// A path outside the roots, refused: not an input, as a native sandbox's refused write is
    /// not, and with no letter of its own in [`Seen`].
    RefusedRead,
}

/// What the macro read, in the order it read it, each line's text in one buffer.
#[derive(Default)]
struct Trace {
    lines: Vec<Line>,
    text: String,
    /// Whether the clock, randomness and the whole environment have been noted: each is one
    /// line however often it is read.
    clock: bool,
    random: bool,
    all_env: bool,
}

impl Trace {
    /// Notes `what`, unless it is the line noted last.
    fn note(&mut self, noted: Noted, what: &str) {
        if let Some(last) = self.lines.last()
            && last.noted == noted
            && &self.text[last.start as usize..last.end as usize] == what
        {
            return;
        }
        let start = self.text.len() as u32;
        self.text.push_str(what);
        self.lines.push(Line { noted, start, end: self.text.len() as u32 });
    }

    fn text(&self, line: &Line) -> &str {
        &self.text[line.start as usize..line.end as usize]
    }
}

/// One expansion's machine as its macro sees it: the calling crate's environment, the
/// directories it may read, its open descriptors and directory streams, and what it read.
pub(crate) struct Os<'a> {
    env: &'a base_db::Env,
    /// The directories it may read, found on its first path.
    roots: Option<files::Roots>,
    fds: Vec<files::Slot>,
    streams: Vec<files::Stream>,
    trace: Trace,
    unkeyable: Option<String>,
    /// The value `getenv` gave of each variable it was asked of that is set, by its name.
    answers: Vec<(&'a str, u64)>,
    /// What `environ` gives, once made.
    environ: Option<u64>,
    /// What `realpath` and `getcwd` allocated, which the program frees.
    allocated: Vec<u64>,
    /// The path a call names, made absolute, and the real path it leads to.
    given: String,
    real: String,
    /// What of a path is still to walk.
    rest: String,
    /// A path built to hash into an inode.
    scratch: Vec<u8>,
    /// The bytes of a C string built to give the program.
    bytes: Vec<u8>,
}

impl<'a> Os<'a> {
    /// `env` is the calling crate's (`ProcMacroCall::env`): what the macro's `getenv` answers.
    /// It may read under `CARGO_MANIFEST_DIR` and `OUT_DIR` of `env`, and nothing else.
    pub(crate) fn new(env: &'a Env) -> Os<'a> {
        Os {
            env,
            roots: None,
            fds: Vec::new(), // alloc: none until the first open, then each slot reused
            streams: Vec::new(), // alloc: none until the first `opendir`, then reused
            trace: Trace::default(),
            unkeyable: None,
            answers: Vec::new(), // alloc: one entry per variable read, once
            environ: None,
            allocated: Vec::new(), // alloc: none until a `realpath` or `getcwd` allocates
            given: String::new(), // alloc: none until the first path, then reused
            real: String::new(), // alloc: as `given`
            rest: String::new(), // alloc: as `given`
            scratch: Vec::new(), // alloc: none until the first listing, then reused
            bytes: Vec::new(), // alloc: none until the first string given, then reused
        }
    }

    /// Whether `func` is one of the shims: an extern function of a known name (its
    /// `#[link_name]` if it has one, so macOS's `stat$INODE64` and the like), or one of the few
    /// std functions named in [`Call::of_std`].
    pub(crate) fn claims(db: &Db, func: FunctionId) -> Option<Shim> {
        let call = match func.lookup(db).container {
            ItemContainerId::ExternBlockId(_) => {
                let link_name = AttrFlags::link_name(db, func.into());
                let name = match &link_name {
                    Some(name) => name.as_str(),
                    None => FunctionSignature::of(db, func).name.as_str(),
                };
                Call::of_extern(name)?
            }
            ItemContainerId::ModuleId(module) => {
                let krate = func.krate(db);
                if !matches!(krate.data(db).origin, CrateOrigin::Lang(LangCrateOrigin::Std)) {
                    return None;
                }
                let module = module.name(db)?;
                Call::of_std(FunctionSignature::of(db, func).name.as_str(), module.as_str())?
            }
            ItemContainerId::ImplId(_) | ItemContainerId::TraitId(_) => return None,
        };
        Some(Shim { call, target: Os::target_of(db, func.krate(db)) })
    }

    fn target_of(db: &Db, krate: base_db::Crate) -> Option<Target> {
        match &krate.workspace_data(db).target {
            Ok(data) => Target::of_tuple(data.tuple.as_str()),
            Err(_) => None,
        }
    }

    /// Runs `shim` with the argument bytes `args`, writing its result to `ret` (zeroed, the
    /// size of the return type). `Err` stops the expansion; [`Answer::Evaluator`] gives the
    /// call back to the evaluator.
    pub(crate) fn call(
        &mut self,
        shim: Shim,
        guest: &mut impl Guest,
        args: &[&[u8]],
        ret: &mut [u8],
    ) -> Done {
        let Some(t) = shim.target else {
            // alloc: error path
            return Err(Stop(format!(
                "calls `{:?}` on a target whose system blaze's evaluator does not model \
                 (it models 64-bit Linux with glibc and 64-bit macOS)",
                shim.call
            )));
        };
        let g = guest;
        match shim.call {
            Call::Open => {
                let flags = int(args, 1)?;
                self.open(t, g, ret, (t.at_flags().fdcwd, word(args, 0)?, flags))
            }
            Call::Openat => {
                let flags = int(args, 2)?;
                self.open(t, g, ret, (int(args, 0)?, word(args, 1)?, flags))
            }
            Call::Read => self.read(t, g, ret, (int(args, 0)?, word(args, 1)?, word(args, 2)?, None)),
            Call::Pread => {
                let offset = int(args, 3)?;
                self.read(t, g, ret, (int(args, 0)?, word(args, 1)?, word(args, 2)?, Some(offset)))
            }
            Call::Readv => self.readv(t, g, ret, (int(args, 0)?, word(args, 1)?, int(args, 2)?)),
            Call::Close => self.close(t, g, ret, int(args, 0)?),
            Call::Lseek => self.lseek(t, g, ret, (int(args, 0)?, int(args, 1)?, int(args, 2)?)),
            Call::Fstat => self.fstat(t, g, ret, (int(args, 0)?, word(args, 1)?)),
            Call::Stat | Call::Lstat => {
                let at = (t.at_flags().fdcwd, word(args, 0)?);
                let flags = if shim.call == Call::Lstat { t.at_flags().symlink_nofollow } else { 0 };
                self.fstatat(t, g, ret, at, word(args, 1)?, flags, files::Form::Stat)
            }
            Call::Fstatat => {
                let at = (int(args, 0)?, word(args, 1)?);
                self.fstatat(t, g, ret, at, word(args, 2)?, int(args, 3)?, files::Form::Stat)
            }
            Call::Getcwd => self.getcwd(t, g, ret, (word(args, 0)?, word(args, 1)?)),
            Call::Realpath => self.realpath(t, g, ret, (word(args, 0)?, word(args, 1)?)),
            Call::Readlink => self.readlink(t, g, ret, (word(args, 0)?, word(args, 1)?, word(args, 2)?)),
            Call::Access => {
                let at = (t.at_flags().fdcwd, word(args, 0)?);
                self.faccessat(t, g, ret, at, int(args, 1)?, 0)
            }
            Call::Faccessat => {
                let at = (int(args, 0)?, word(args, 1)?);
                self.faccessat(t, g, ret, at, int(args, 2)?, int(args, 3)?)
            }
            Call::Opendir => self.opendir(t, g, ret, word(args, 0)?),
            Call::Fdopendir => self.fdopendir(t, g, ret, int(args, 0)?),
            Call::Readdir => self.readdir(t, g, ret, word(args, 0)?),
            Call::Closedir => self.closedir(t, g, ret, word(args, 0)?),
            Call::Dirfd => self.dirfd(t, g, ret, word(args, 0)?),
            Call::Free => self.free(word(args, 0)?),
            Call::Getenv => self.getenv(g, ret, word(args, 0)?),
            Call::Setenv => Err(env::sets(g, "sets", word(args, 0)?, false)),
            Call::Unsetenv => Err(env::sets(g, "unsets", word(args, 0)?, false)),
            Call::Putenv => Err(env::sets(g, "sets", word(args, 0)?, true)),
            Call::Environ => self.environ(g, ret),
            Call::ClockGettime => self.clock_gettime(t, g, ret, (int(args, 0)?, word(args, 1)?)),
            Call::Gettimeofday => self.gettimeofday(g, ret, (word(args, 0)?, word(args, 1)?)),
            Call::Time => self.time(g, ret, word(args, 0)?),
            Call::MachAbsoluteTime => self.mach_absolute_time(ret),
            Call::MachTimebaseInfo => clock::mach_timebase_info(g, ret, word(args, 0)?),
            Call::HashmapRandomKeys => Ok(random::hashmap_random_keys(ret)),
            Call::Getrandom => self.getrandom(g, ret, (word(args, 0)?, word(args, 1)?)),
            Call::Getentropy => self.getentropy(t, g, ret, (word(args, 0)?, word(args, 1)?)),
            Call::Arc4randomBuf => self.fill_random(g, ret, (word(args, 0)?, word(args, 1)?), None),
            Call::CcRandomGenerateBytes => {
                self.fill_random(g, ret, (word(args, 0)?, word(args, 1)?), Some(0))
            }
            Call::SecRandomCopyBytes => {
                self.fill_random(g, ret, (word(args, 2)?, word(args, 1)?), Some(0))
            }
            Call::Syscall => self.syscall(t, g, args, ret),
        }
    }

    /// What it read, in the order it read it, as `assemble_seen` takes it.
    pub(crate) fn seen(&self) -> impl Iterator<Item = (Seen, &str)> {
        self.trace.lines.iter().filter_map(|line| match line.noted {
            Noted::Seen(seen) => Some((seen, self.trace.text(line))),
            Noted::RefusedRead => None,
        })
    }

    /// The paths it was refused, in the order it asked for them.
    pub(crate) fn refused(&self) -> impl Iterator<Item = &str> {
        self.trace.lines.iter().filter(|line| line.noted == Noted::RefusedRead).map(|line| self.trace.text(line))
    }

    /// Why what it read can't be keyed, when it can't: a read that failed in a way the trace
    /// doesn't say (an I/O error other than not-found).
    pub(crate) fn unkeyable(&self) -> Option<&str> {
        self.unkeyable.as_deref()
    }

    /// `syscall(number, ...)`: the numbers std makes through it go to the shims of the same
    /// calls, those of the evaluator's threads back to it, and any other stops the expansion.
    fn syscall(&mut self, t: Target, g: &mut impl Guest, args: &[&[u8]], ret: &mut [u8]) -> Done {
        let number = int(args, 0)?;
        let rest = &args[1..];
        match t.sys(number) {
            Sys::Statx => {
                let at = (int(rest, 0)?, word(rest, 1)?);
                self.fstatat(t, g, ret, at, word(rest, 4)?, int(rest, 2)?, files::Form::Statx)
            }
            Sys::Getrandom => self.getrandom(g, ret, (word(rest, 0)?, word(rest, 1)?)),
            Sys::Evaluator => Ok(Answer::Evaluator),
            Sys::Other(name) => {
                let name = name.unwrap_or("one blaze has no name for");
                // alloc: error path
                Err(Stop(format!("makes the system call `{number}` (`{name}`)")))
            }
        }
    }

    /// Notes that a read failed in a way the trace can't say, keeping the first such.
    fn cant(&mut self, path: &str, error: &std::io::Error) {
        // alloc: the message of an expansion that can't be keyed (error path)
        self.unkeyable.get_or_insert_with(|| format!("{path}: {error}"));
    }
}

/// The `n`th argument as a signed integer of its width: a `c_int`, `off_t` or `c_long`.
fn int(args: &[&[u8]], n: usize) -> Result<i64, Stop> {
    let value = word(args, n)?;
    let bits = 8 * args[n].len().clamp(1, 8) as u32;
    let shift = 64 - bits;
    Ok(((value << shift) as i64) >> shift)
}

/// The call failed with `errno`: `-1` returned.
fn fail(t: Target, g: &mut impl Guest, ret: &mut [u8], errno: Errno) -> Done {
    g.set_errno(t.errno(errno))?;
    put_word(ret, u64::MAX);
    Ok(Answer::Answered)
}

/// The call failed with `errno`: a null pointer returned.
fn fail_null(t: Target, g: &mut impl Guest, ret: &mut [u8], errno: Errno) -> Done {
    g.set_errno(t.errno(errno))?;
    put_word(ret, 0);
    Ok(Answer::Answered)
}

fn ok(ret: &mut [u8], value: u64) -> Done {
    put_word(ret, value);
    Ok(Answer::Answered)
}
