//! The seed's own calls, module `blaze_seed` ([`MODULE`]): blaze's ABI, not WASI's, for what the
//! bootstrap seed (blaze compiled to wasm64) needs past the WASI p1 calls it already makes
//! (`wasi_snapshot_preview1_64`). Andrew's decision (2026-11-02): the seed's host need not be
//! WASI, so these are shaped for blaze's std, not copied from p1.
//!
//! - **Shape.** Every pointer, length, size, offset and time is an `i64`; a descriptor or a flag
//!   an `i32`. A call answers an `i32`: 0, or a WASI `errno` ([`Errno`]), the same numbers the
//!   p1 calls give. An output goes through an `i64` pointer to 8 bytes. A path is `(ptr, len)`,
//!   absolute (from `/`); a relative one is `EINVAL`. No rights, fdflags or preopens.
//! - **Descriptors** are the p1 calls' (a file `path_open` opened), one table.
//! - **Version.** [`VERSION`], which `abi_version()` answers. A change to a call's shape or
//!   meaning bumps it; a new call does not.
//! - **Memory.** `decommit(ptr, len)` gives pages back; the range reads zeros after
//!   (blaze-mem's `os.rs` on wasm64).
//! - **Fibers.** `fiber_new(entry, arg, stack_pointer) -> fiber` makes a fiber that will run
//!   the guest function at table slot `entry`, a `(i64) -> ()`, as `entry(arg)`, with
//!   `__stack_pointer` at `stack_pointer` (its shadow stack's top); `fiber_resume(fiber)` runs
//!   it until it calls `fiber_suspend()` (answering 0) or `entry` returns (answering 1);
//!   `fiber_drop(fiber)` ends it, unwinding a suspended one with no guest code run. Each fiber
//!   is one of Wasmtime's own fibers (a native stack, `wasmtime::GuestFiber`), so a switch costs
//!   the same at any depth and resumes nest. The host saves and restores `__stack_pointer`
//!   around each run, so the module exports it and its function table
//!   (`__indirect_function_table`), as blaze-link writes every wasm64 module. Misuse (an
//!   unknown fiber, resuming one that runs or returned, a suspend off any fiber) traps, naming
//!   the fiber and the call. This is the seed's fiber switch, blaze-graph's `sched::stack` on
//!   wasm (docs/future/bootstrap.md, "Fibers in the seed").
//!
//! Only an embedder whose store implements [`EmbedSeed`] links these: the seed runner, which
//! enables this crate's `seed` feature (Wasmtime's `async`, which no wasm build of Wasmtime
//! has). The macro host does not, and its import check refuses the module.

#[cfg(feature = "seed")]
use std::marker::PhantomData;

#[cfg(feature = "seed")]
use wasmtime::{
    Caller, Extern, Global, GuestFiber, GuestFiberResumed, Linker, Ref, Store, Table, Val,
};

#[cfg(feature = "seed")]
use crate::{
    Embed, Errno, Stop, W64,
    calls::{Mem, answered},
};

/// The module the seed imports these calls from.
pub const MODULE: &str = "blaze_seed";

/// The ABI's version: what `abi_version()` answers.
pub const VERSION: u32 = 1;

/// Every function [`link`] defines, under [`MODULE`].
pub const FUNCTIONS: &[&str] = &[
    "abi_version",
    "create_dir",
    "decommit",
    "fd_set_times",
    "fiber_drop",
    "fiber_new",
    "fiber_resume",
    "fiber_suspend",
    "hard_link",
    "pread",
    "pwrite",
    "remove_dir",
    "rename",
    "set_len",
    "set_times",
    "symlink",
    "sync",
    "unlink",
];

/// A time argument that leaves the time as it is.
pub const KEEP: u64 = u64::MAX;

/// A time argument that sets the time to the host's clock.
pub const NOW: u64 = u64::MAX - 1;

/// What a time argument asks for.
#[cfg(feature = "seed")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Time {
    Keep,
    Now,
    /// Nanoseconds since the epoch.
    At(u64),
}

#[cfg(feature = "seed")]
impl Time {
    fn of(ns: u64) -> Time {
        match ns {
            KEEP => Time::Keep,
            NOW => Time::Now,
            ns => Time::At(ns),
        }
    }
}

/// What the seed's calls do: the host's file system changed, its memory given back. Paths are
/// absolute, as the guest named them.
#[cfg(feature = "seed")]
pub trait Seed {
    fn create_dir(&mut self, path: &[u8]) -> Result<(), Errno>;
    fn remove_dir(&mut self, path: &[u8]) -> Result<(), Errno>;
    /// Removes the file (or link) `path`; a directory is `EISDIR` or `EPERM`, as the host says.
    fn unlink(&mut self, path: &[u8]) -> Result<(), Errno>;
    fn rename(&mut self, from: &[u8], to: &[u8]) -> Result<(), Errno>;
    /// A hard link at `to` of `from` itself (a link is not followed).
    fn hard_link(&mut self, from: &[u8], to: &[u8]) -> Result<(), Errno>;
    /// A symbolic link at `path` holding `target`, uninterpreted.
    fn symlink(&mut self, target: &[u8], path: &[u8]) -> Result<(), Errno>;
    fn set_times(
        &mut self,
        path: &[u8],
        follow: bool,
        atime: Time,
        mtime: Time,
    ) -> Result<(), Errno>;
    fn fd_set_times(&mut self, fd: u32, atime: Time, mtime: Time) -> Result<(), Errno>;
    fn set_len(&mut self, fd: u32, len: u64) -> Result<(), Errno>;
    /// Flushes `fd` to its device: its data, and unless `data_only` its metadata.
    fn sync(&mut self, fd: u32, data_only: bool) -> Result<(), Errno>;
    /// Reads at `offset` into `into`, leaving the position: how many were read.
    fn pread(&mut self, fd: u32, into: &mut [u8], offset: u64) -> Result<usize, Errno>;
    /// Writes `bytes` at `offset`, leaving the position: how many were written.
    fn pwrite(&mut self, fd: u32, bytes: &[u8], offset: u64) -> Result<usize, Errno>;
    /// Gives the pages of `range`, bytes of the guest's memory, back to the host; it reads
    /// zeros after.
    fn decommit(&mut self, range: &mut [u8]) -> Result<(), Stop>;
}

/// The store data of an embedder that links the seed's calls.
#[cfg(feature = "seed")]
pub trait EmbedSeed: Embed {
    /// As [`Embed::with`], with the embedder's [`Seed`].
    fn with_seed<R>(
        caller: &mut Caller<'_, Self>,
        f: impl FnOnce(&mut [u8], &mut dyn Seed) -> Result<R, Stop>, // dyn: each embedder's, behind one set of calls
    ) -> wasmtime::Result<R>;

    /// The seed's fibers, kept in the store's data.
    fn fibers(&mut self) -> &mut Fibers;
}

/// The seed's fibers, by the number `fiber_new` answered (a slot here), and the module's
/// exports the switch needs, found at the first `fiber_new`.
#[cfg(feature = "seed")]
#[derive(Default)]
pub struct Fibers {
    slots: Vec<Slot>, // alloc: one per fiber alive at once, kept for the run
    free: Vec<u32>,   // alloc: as above
    exports: Option<(Global, Table)>,
}

#[cfg(feature = "seed")]
enum Slot {
    Free,
    /// Not running: made and not yet resumed, or suspended.
    Idle(Fiber),
    /// Taken out by its `fiber_resume`, which puts it back.
    Running,
    /// Its function returned or trapped; its number stays taken until `fiber_drop`.
    Done,
}

#[cfg(feature = "seed")]
struct Fiber {
    fiber: GuestFiber,
    /// Its `__stack_pointer` while it is switched out.
    stack_pointer: u64,
}

#[cfg(feature = "seed")]
impl Fibers {
    fn slot(&mut self, call: &str, fiber: u64) -> wasmtime::Result<&mut Slot> {
        let slot = u32::try_from(fiber).ok().and_then(|at| self.slots.get_mut(at as usize));
        match slot {
            Some(slot) if !matches!(slot, Slot::Free) => Ok(slot),
            // alloc: error path
            _ => Err(wasmtime::Error::msg(format!(
                "`{MODULE}.{call}` of fiber {fiber}, which the seed never made or already dropped"
            ))),
        }
    }

    /// Takes `fiber` out to run it.
    fn take(&mut self, fiber: u64) -> wasmtime::Result<Fiber> {
        let slot = self.slot("fiber_resume", fiber)?;
        match std::mem::replace(slot, Slot::Running) {
            Slot::Idle(it) => Ok(it),
            was => {
                let why = if matches!(was, Slot::Running) {
                    "it is running"
                } else {
                    "its function returned or trapped"
                };
                *slot = was;
                // alloc: error path
                Err(wasmtime::Error::msg(format!(
                    "`{MODULE}.fiber_resume` of fiber {fiber}: {why}"
                )))
            }
        }
    }

    fn insert(&mut self, fiber: Fiber) -> u64 {
        match self.free.pop() {
            Some(at) => {
                self.slots[at as usize] = Slot::Idle(fiber);
                at.into()
            }
            None => {
                self.slots.push(Slot::Idle(fiber));
                (self.slots.len() - 1) as u64
            }
        }
    }

    /// Ends every fiber left, unwinding each suspended one with no guest code run: after the
    /// seed's entry returned, before its store is dropped. A fiber still suspended then is one
    /// the seed leaked (as blaze-graph leaks a fiber suspended mid-body).
    pub fn dispose_all<T: EmbedSeed>(store: &mut Store<T>) {
        let slots = std::mem::take(&mut store.data_mut().fibers().slots);
        for slot in slots {
            if let Slot::Idle(it) = slot {
                store.guest_fiber_dispose(it.fiber);
            }
        }
        store.data_mut().fibers().free.clear();
    }
}

/// The module's `__stack_pointer` global and function table, found once.
#[cfg(feature = "seed")]
fn exports<T: EmbedSeed>(c: &mut Caller<'_, T>) -> wasmtime::Result<(Global, Table)> {
    if let Some(found) = c.data_mut().fibers().exports {
        return Ok(found);
    }
    let global = match c.get_export("__stack_pointer") {
        Some(Extern::Global(it)) => it,
        // alloc: error path
        _ => {
            return Err(wasmtime::Error::msg(format!(
                "`{MODULE}.fiber_new`: the seed exports no global `__stack_pointer`, which blaze-link exports from every wasm64 module"
            )));
        }
    };
    let table = match c.get_export("__indirect_function_table") {
        Some(Extern::Table(it)) => it,
        // alloc: error path
        _ => {
            return Err(wasmtime::Error::msg(format!(
                "`{MODULE}.fiber_new`: the seed exports no table `__indirect_function_table`, which blaze-link exports from every wasm64 module"
            )));
        }
    };
    c.data_mut().fibers().exports = Some((global, table));
    Ok((global, table))
}

/// The seed's `__stack_pointer` now.
#[cfg(feature = "seed")]
fn stack_pointer<T>(c: &mut Caller<'_, T>, global: Global) -> wasmtime::Result<u64> {
    match global.get(&mut *c) {
        Val::I64(it) => Ok(it as u64),
        // alloc: error path
        other => Err(wasmtime::Error::msg(format!(
            "`{MODULE}.fiber_resume`: the seed's `__stack_pointer` is {:?}, not a wasm64 module's `i64`",
            other.ty(&*c)
        ))),
    }
}

/// `fiber_new`: a fiber of the function at table slot `entry`, to run as `entry(arg)`.
#[cfg(feature = "seed")]
fn fiber_new<T: EmbedSeed>(
    c: &mut Caller<'_, T>,
    entry: u64,
    arg: u64,
    stack_pointer: u64,
) -> wasmtime::Result<u64> {
    let (_, table) = exports(c)?;
    let func = match table.get(&mut *c, entry) {
        Some(Ref::Func(Some(func))) => func,
        // alloc: error path
        _ => {
            return Err(wasmtime::Error::msg(format!(
                "`{MODULE}.fiber_new`: table slot {entry} holds no function"
            )));
        }
    };
    let func = func.typed::<u64, ()>(&*c).map_err(|why| {
        // alloc: error path
        wasmtime::Error::msg(format!("`{MODULE}.fiber_new`: the function at table slot {entry} is not a `(i64) -> ()`: {why:#}"))
    })?;
    let fiber = c.guest_fiber_new(func, arg).map_err(|why| {
        // alloc: error path
        wasmtime::Error::msg(format!(
            "`{MODULE}.fiber_new` of the function at table slot {entry}: {why:#}"
        ))
    })?;
    Ok(c.data_mut().fibers().insert(Fiber { fiber, stack_pointer }))
}

/// `fiber_resume`: runs `fiber` on its stack and its shadow stack until it suspends (0) or its
/// function returns (1). A trap on it is this call's, and ends it.
#[cfg(feature = "seed")]
fn fiber_resume<T: EmbedSeed>(c: &mut Caller<'_, T>, fiber: u64) -> wasmtime::Result<u32> {
    let (global, _) = exports(c)?;
    let mut it = c.data_mut().fibers().take(fiber)?;
    let mine = stack_pointer(c, global)?;
    global.set(&mut *c, Val::I64(it.stack_pointer as i64))?;
    let ran = c.guest_fiber_resume(&mut it.fiber);
    it.stack_pointer = stack_pointer(c, global)?;
    global.set(&mut *c, Val::I64(mine as i64))?;
    let fibers = c.data_mut().fibers();
    let slot = &mut fibers.slots[fiber as usize];
    match ran {
        Ok(GuestFiberResumed::Suspended) => {
            *slot = Slot::Idle(it);
            Ok(0)
        }
        Ok(GuestFiberResumed::Returned) => {
            *slot = Slot::Done;
            Ok(1)
        }
        Err(trap) => {
            *slot = Slot::Done;
            Err(trap)
        }
    }
}

/// `fiber_drop`: ends `fiber`, which must not be running; a suspended one is unwound.
#[cfg(feature = "seed")]
fn fiber_drop<T: EmbedSeed>(c: &mut Caller<'_, T>, fiber: u64) -> wasmtime::Result<()> {
    let slot = c.data_mut().fibers().slot("fiber_drop", fiber)?;
    match std::mem::replace(slot, Slot::Free) {
        Slot::Running => {
            *slot = Slot::Running;
            // alloc: error path
            return Err(wasmtime::Error::msg(format!(
                "`{MODULE}.fiber_drop` of fiber {fiber}: it is running"
            )));
        }
        Slot::Idle(it) => c.guest_fiber_dispose(it.fiber),
        Slot::Done | Slot::Free => {}
    }
    c.data_mut().fibers().free.push(fiber as u32);
    Ok(())
}

/// Runs `f`, the call `name`, on the guest's memory and the embedder's [`Seed`].
#[cfg(feature = "seed")]
fn call<T: EmbedSeed, R>(
    caller: &mut Caller<'_, T>,
    name: &'static str,
    f: impl FnOnce(&mut Mem<'_, W64>, &mut dyn Seed) -> Result<R, Stop>, // dyn: as `EmbedSeed::with_seed`
) -> wasmtime::Result<R> {
    T::with_seed(caller, |bytes, seed| f(&mut Mem { bytes, call: name, width: PhantomData }, seed))
}

/// The absolute path at `(ptr, len)`, or `EINVAL` for a relative one.
#[cfg(feature = "seed")]
fn path<'m>(mem: &'m Mem<'_, W64>, ptr: u64, len: u64) -> Result<Result<&'m [u8], Errno>, Stop> {
    let path = mem.get(ptr, len)?;
    Ok(if path.first() == Some(&b'/') { Ok(path) } else { Err(Errno::INVAL) })
}

/// The `errno` of a call that answers nothing.
#[cfg(feature = "seed")]
fn done(answer: Result<(), Errno>) -> Result<i32, Stop> {
    answered(answer, |()| Ok(()))
}

/// Defines every call of [`FUNCTIONS`] in `l`, under [`MODULE`].
#[cfg(feature = "seed")]
pub fn link<T: EmbedSeed>(l: &mut Linker<T>) -> wasmtime::Result<()> {
    let m = MODULE;
    l.func_wrap(m, "abi_version", |_: Caller<'_, T>| VERSION as i32)?;
    l.func_wrap(m, "decommit", |mut c: Caller<'_, T>, ptr: u64, len: u64| {
        call(&mut c, "decommit", |mem, seed| seed.decommit(mem.get_mut(ptr, len)?))
    })?;
    l.func_wrap(m, "create_dir", |mut c: Caller<'_, T>, ptr: u64, len: u64| {
        call(&mut c, "create_dir", |mem, seed| {
            done(path(mem, ptr, len)?.and_then(|p| seed.create_dir(p)))
        })
    })?;
    l.func_wrap(m, "remove_dir", |mut c: Caller<'_, T>, ptr: u64, len: u64| {
        call(&mut c, "remove_dir", |mem, seed| {
            done(path(mem, ptr, len)?.and_then(|p| seed.remove_dir(p)))
        })
    })?;
    l.func_wrap(m, "unlink", |mut c: Caller<'_, T>, ptr: u64, len: u64| {
        call(&mut c, "unlink", |mem, seed| done(path(mem, ptr, len)?.and_then(|p| seed.unlink(p))))
    })?;
    l.func_wrap(
        m,
        "rename",
        |mut c: Caller<'_, T>, from: u64, from_len: u64, to: u64, to_len: u64| {
            call(&mut c, "rename", |mem, seed| {
                let (from, to) = (path(mem, from, from_len)?, path(mem, to, to_len)?);
                done(from.and_then(|from| seed.rename(from, to?)))
            })
        },
    )?;
    l.func_wrap(
        m,
        "hard_link",
        |mut c: Caller<'_, T>, from: u64, from_len: u64, to: u64, to_len: u64| {
            call(&mut c, "hard_link", |mem, seed| {
                let (from, to) = (path(mem, from, from_len)?, path(mem, to, to_len)?);
                done(from.and_then(|from| seed.hard_link(from, to?)))
            })
        },
    )?;
    l.func_wrap(
        m,
        "symlink",
        |mut c: Caller<'_, T>, target: u64, target_len: u64, at: u64, len: u64| {
            call(&mut c, "symlink", |mem, seed| {
                let target = mem.get(target, target_len)?;
                done(path(mem, at, len)?.and_then(|p| seed.symlink(target, p)))
            })
        },
    )?;
    l.func_wrap(
        m,
        "set_times",
        |mut c: Caller<'_, T>, ptr: u64, len: u64, follow: u32, atime: u64, mtime: u64| {
            call(&mut c, "set_times", |mem, seed| {
                done(
                    path(mem, ptr, len)?.and_then(|p| {
                        seed.set_times(p, follow != 0, Time::of(atime), Time::of(mtime))
                    }),
                )
            })
        },
    )?;
    l.func_wrap(m, "fd_set_times", |mut c: Caller<'_, T>, fd: u32, atime: u64, mtime: u64| {
        call(&mut c, "fd_set_times", |_, seed| {
            done(seed.fd_set_times(fd, Time::of(atime), Time::of(mtime)))
        })
    })?;
    l.func_wrap(m, "set_len", |mut c: Caller<'_, T>, fd: u32, len: u64| {
        call(&mut c, "set_len", |_, seed| done(seed.set_len(fd, len)))
    })?;
    l.func_wrap(m, "sync", |mut c: Caller<'_, T>, fd: u32, data_only: u32| {
        call(&mut c, "sync", |_, seed| done(seed.sync(fd, data_only != 0)))
    })?;
    l.func_wrap(
        m,
        "pread",
        |mut c: Caller<'_, T>, fd: u32, buf: u64, len: u64, offset: u64, out: u64| {
            call(&mut c, "pread", |mem, seed| {
                let got = seed.pread(fd, mem.get_mut(buf, len)?, offset);
                answered(got, |n| mem.put_u64(out, n as u64))
            })
        },
    )?;
    l.func_wrap(
        m,
        "pwrite",
        |mut c: Caller<'_, T>, fd: u32, buf: u64, len: u64, offset: u64, out: u64| {
            call(&mut c, "pwrite", |mem, seed| {
                let wrote = seed.pwrite(fd, mem.get(buf, len)?, offset);
                answered(wrote, |n| mem.put_u64(out, n as u64))
            })
        },
    )?;
    l.func_wrap(
        m,
        "fiber_new",
        |mut c: Caller<'_, T>, entry: u64, arg: u64, stack_pointer: u64| {
            fiber_new(&mut c, entry, arg, stack_pointer)
        },
    )?;
    l.func_wrap(m, "fiber_resume", |mut c: Caller<'_, T>, fiber: u64| fiber_resume(&mut c, fiber))?;
    l.func_wrap(m, "fiber_suspend", |mut c: Caller<'_, T>| {
        c.guest_fiber_suspend()
            .map_err(|why| wasmtime::Error::msg(format!("`{MODULE}.fiber_suspend`: {why:#}"))) // alloc: error path
    })?;
    l.func_wrap(m, "fiber_drop", |mut c: Caller<'_, T>, fiber: u64| fiber_drop(&mut c, fiber))?;
    Ok(())
}
