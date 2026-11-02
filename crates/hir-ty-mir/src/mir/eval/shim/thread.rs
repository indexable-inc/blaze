//! The shims of threads, on the evaluator's thread core (`eval/threads.rs`): `pthread_*`
//! (threads and their attributes, keys, mutexes, condition variables), the futex
//! (`syscall(SYS_futex, ..)` on Linux, `__ulock_*` and `os_sync_*` on Apple's), dispatch
//! semaphores (std's parker on Apple's), `sched_*`, `sysconf`, the clock and sleeping, `errno`,
//! the TLS destructors' registration, and what std's stack-overflow handler calls
//! (`sigaltstack`, `sigaction`, anonymous `mmap`). A call that blocks hands the core a [`Wait`]
//! and completes when the thread wakes, in [`Evaluator::resume_shim`] when it has more to do.
//!
//! The state of a `pthread_*` object is kept in its own bytes, at offsets of the evaluator's
//! choosing where the static initializers leave them valid (glibc's all-zero ones, Apple's
//! `__sig`); only these shims read them, unseen by the machine, which sees the synchronization
//! instead (`release` at an unlock or a wake, `acquire` at a lock).

use cfg::CfgAtom;
use intern::Symbol;

use super::*;
use crate::mir::eval::{
    At, word_bytes,
    machine::{Access, AtomicOrd, EvalThread},
    threads::Wait,
};

/// The threads a run reports `available_parallelism` as: one, so libtest and thread pools run
/// what they spawn one after the other, which the scheduler interleaves anyway.
const CPUS: i128 = 1;
/// `CLOCK_REALTIME` when the run starts: 2026-01-01T00:00:00Z, the same in every run.
const REALTIME_START_NS: u64 = 1_767_225_600 * NANOS;
/// `CLOCK_MONOTONIC` when the run starts: a machine up for a day, so `Instant::now()` minus a
/// few seconds does not underflow.
const MONOTONIC_START_NS: u64 = 86_400 * NANOS;
const NANOS: u64 = 1_000_000_000;
/// POSIX's `PTHREAD_DESTRUCTOR_ITERATIONS` (glibc's and Apple's).
const DESTRUCTOR_ITERATIONS: u8 = 4;
/// What `pthread_attr_init` writes at the start of the attributes, to tell an initialized one.
const ATTR_MAGIC: u32 = 0x6174_7472;
/// What `dispatch_semaphore_create` writes after the count.
const SEMAPHORE_MAGIC: u64 = 0x7365_6d61_7068_6f72;
/// `dispatch_semaphore_wait`'s result when it times out (`KERN_OPERATION_TIMED_OUT`).
const SEMAPHORE_TIMED_OUT: i128 = 49;
const DISPATCH_TIME_FOREVER: u64 = !0;

const FUTEX_WAIT: i32 = 0;
const FUTEX_WAKE: i32 = 1;
const FUTEX_WAIT_BITSET: i32 = 9;
const FUTEX_WAKE_BITSET: i32 = 10;
const FUTEX_PRIVATE_FLAG: i32 = 128;
const FUTEX_CLOCK_REALTIME: i32 = 256;

const ULF_NO_ERRNO: u32 = 0x0100_0000;
const ULF_WAKE_ALL: u32 = 0x100;
const ULF_WAKE_THREAD: u32 = 0x200;
const OS_SYNC_SHARED: u32 = 1;
const OS_CLOCK_MACH_ABSOLUTE_TIME: u32 = 32;

const MAP_FIXED: i32 = 0x10;
const TIMER_ABSTIME: i32 = 1;
const AT_PAGESZ: u128 = 6;
const AT_MINSIGSTKSZ: u128 = 51;

/// Apple's `__sig` of a `pthread_mutex_t` once initialized, and of its static initializers.
const APPLE_MUTEX_SIG: usize = 0x4D55_5458;
const APPLE_MUTEX_SIG_INIT: usize = 0x32AA_ABA7;
const APPLE_ERRORCHECK_MUTEX_SIG_INIT: usize = 0x32AA_ABA1;
const APPLE_RECURSIVE_MUTEX_SIG_INIT: usize = 0x32AA_ABA2;
const APPLE_FIRSTFIT_MUTEX_SIG_INIT: usize = 0x32AA_ABA3;
const APPLE_MUTEXATTR_SIG: usize = 0x4D54_5841;
const APPLE_COND_SIG: usize = 0x434F_4E44;
const APPLE_COND_SIG_INIT: usize = 0x3CB0_B1BB;

/// The `pthread_key_*` state of one thread, and what else is per thread here: its `errno`
/// and its alternate signal stack. Key numbering and destructors are the process's
/// ([`Process`]).
#[derive(Debug, Default)]
pub(in crate::mir::eval) struct TlsData {
    /// The value of each key, by its number; past the end, null.
    values: Vec<Address>,
    /// The thread's `errno`, made when first asked for.
    errno: Option<Address>,
    /// `sigaltstack`'s stack and its size; `None`: disabled.
    altstack: Option<(Address, usize)>,
    destructors: DestructorPass,
}

/// Where the thread is in running its keys' destructors at its exit, POSIX's way: pass after
/// pass over the keys while one gave a destructor a value, at most
/// [`DESTRUCTOR_ITERATIONS`] passes.
#[derive(Debug, Default, Clone, Copy)]
struct DestructorPass {
    next_key: usize,
    pass: u8,
    found: bool,
}

/// What the shims keep for the whole process.
#[derive(Debug, Default)]
pub(in crate::mir::eval) struct Process {
    target: Option<Target>,
    /// Every key `pthread_key_create` made, by number; a deleted key's number is not reused,
    /// as the threads' values of it are not all reachable to clear.
    keys: Vec<Key>,
    /// The `struct sigaction` installed for each signal, by number; none: `SIG_DFL`. The
    /// evaluator raises no signal, so nothing calls the handlers.
    // alloc: once per `sigaction`, setup.
    actions: Vec<Option<Box<[u8]>>>,
}

#[derive(Debug, Clone, Copy)]
struct Key {
    /// The bytes of the destructor's function pointer; 0 for none.
    destructor: usize,
    live: bool,
}

/// How a blocked call goes on when its thread wakes (or its deadline passes), on that thread.
#[derive(Debug, Clone, Copy)]
pub(in crate::mir::eval) enum Resume {
    /// A futex, `__ulock_wait` or `os_sync_wait_on_address` wait: 0 when woken, `ETIMEDOUT`
    /// the way `errors` says when its deadline passed.
    Wait { errors: Errors },
    /// `pthread_mutex_lock`, or the relock of a condition variable's waiter: tries again, and
    /// gives `result` once it holds the mutex.
    MutexLock { mutex: Address, result: i32 },
    /// A waiter of the condition variable `cond`, woken or timed out: relocks `mutex`.
    CondWait { cond: Address, mutex: Address },
    /// `dispatch_semaphore_wait`: tries again.
    Semaphore { semaphore: Address, deadline: Option<u64> },
}

/// How a call reports an error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::mir::eval) enum Errors {
    /// `-1`, with the code in `errno`.
    Errno,
    /// The code negated (`__ulock_*` with `ULF_NO_ERRNO`).
    Negated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Os {
    Linux,
    Apple,
}

/// What the shims depend on of the target, read once from the crate's cfg.
#[derive(Debug, Clone, Copy)]
pub(super) struct Target {
    pub(super) os: Os,
    /// `SYS_futex` and `SYS_gettid` of the architecture.
    sys_futex: i64,
    sys_gettid: i64,
    page: usize,
    /// `PTHREAD_STACK_MIN`.
    stack_min: usize,
    errno: ErrnoCodes,
}

#[derive(Debug, Clone, Copy)]
struct ErrnoCodes {
    perm: i32,
    noent: i32,
    srch: i32,
    again: i32,
    nomem: i32,
    busy: i32,
    inval: i32,
    range: i32,
    deadlk: i32,
    timedout: i32,
}

const LINUX_ERRNO: ErrnoCodes = ErrnoCodes {
    perm: 1,
    noent: 2,
    srch: 3,
    again: 11,
    nomem: 12,
    busy: 16,
    inval: 22,
    range: 34,
    deadlk: 35,
    timedout: 110,
};

const APPLE_ERRNO: ErrnoCodes = ErrnoCodes {
    perm: 1,
    noent: 2,
    srch: 3,
    again: 35,
    nomem: 12,
    busy: 16,
    inval: 22,
    range: 34,
    deadlk: 11,
    timedout: 60,
};

impl Os {
    fn sc_pagesize(self) -> i128 {
        match self {
            Os::Linux => 30,
            Os::Apple => 29,
        }
    }
    fn sc_nprocessors(self) -> [i128; 2] {
        match self {
            Os::Linux => [83, 84],
            Os::Apple => [57, 58],
        }
    }
    fn map_anonymous(self) -> i32 {
        match self {
            Os::Linux => 0x20,
            Os::Apple => 0x1000,
        }
    }
    fn ss_disable(self) -> i32 {
        match self {
            Os::Linux => 2,
            Os::Apple => 4,
        }
    }
    fn minsigstksz(self) -> usize {
        match self {
            Os::Linux => 2048,
            Os::Apple => 32768,
        }
    }
    fn sigstop(self) -> i128 {
        match self {
            Os::Linux => 19,
            Os::Apple => 17,
        }
    }
    /// `PTHREAD_CREATE_JOINABLE` and `PTHREAD_CREATE_DETACHED`.
    fn detach_states(self) -> [u32; 2] {
        match self {
            Os::Linux => [0, 1],
            Os::Apple => [1, 2],
        }
    }
    /// The stack of a thread whose attributes don't say.
    fn default_stack(self) -> usize {
        match self {
            Os::Linux => 8 << 20,
            Os::Apple => 512 << 10,
        }
    }
    fn keys_max(self) -> usize {
        match self {
            Os::Linux => 1024,
            Os::Apple => 512,
        }
    }
    /// The mutex kind a `PTHREAD_MUTEX_*` type is.
    fn mutex_kind(self, ty: i128) -> Option<MutexKind> {
        Some(match (self, ty) {
            (_, 0) => MutexKind::Normal,
            (Os::Linux, 1) | (Os::Apple, 2) => MutexKind::Recursive,
            (Os::Linux, 2) | (Os::Apple, 1) => MutexKind::ErrorCheck,
            _ => return None,
        })
    }
    /// What a clock reads as the virtual clock plus: realtime clocks start at
    /// [`REALTIME_START_NS`], the others (monotonic, boot, uptime, CPU time) at
    /// [`MONOTONIC_START_NS`].
    fn clock_base(self, clock: i128) -> Option<u64> {
        match (self, clock) {
            (Os::Linux, 0 | 5 | 8 | 11) | (Os::Apple, 0) => Some(REALTIME_START_NS),
            (Os::Linux, 1..=4 | 6 | 7 | 9) | (Os::Apple, 4..=9 | 12 | 16) => {
                Some(MONOTONIC_START_NS)
            }
            _ => None,
        }
    }
}

/// A mutex's kind, as glibc numbers `__kind`, which its static initializers set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutexKind {
    Normal = 0,
    Recursive = 1,
    ErrorCheck = 2,
}

impl MutexKind {
    fn of(raw: u32) -> Option<MutexKind> {
        Some(match raw {
            0 => MutexKind::Normal,
            1 => MutexKind::Recursive,
            2 => MutexKind::ErrorCheck,
            _ => return None,
        })
    }
}

/// A mutex's state: who holds it (`pthread_t`, 0: nobody) and how many times.
#[derive(Debug, Clone, Copy)]
struct MutexState {
    kind: MutexKind,
    owner: u32,
    count: u32,
}

/// Where a `pthread_mutex_t`'s state is: on Linux in glibc's own fields (`__count`,
/// `__owner`, `__kind`), after Apple's `__sig` there.
#[derive(Debug, Clone, Copy)]
struct MutexLayout {
    kind: usize,
    owner: usize,
    count: usize,
}

/// Where a `pthread_cond_t`'s state is: its clock, how many wait, and the mutex they wait
/// with (an address, 0 while none waits).
#[derive(Debug, Clone, Copy)]
struct CondLayout {
    clock: usize,
    waiters: usize,
    mutex: usize,
}

fn missing<'db>(name: &str) -> MirEvalError<'db> {
    // alloc: the error that ends the run.
    MirEvalError::InternalError(format!("{name} args are not provided").into())
}

fn null() -> Address {
    Address::from_usize(0)
}

impl<'db, M: Machine<'db>> Evaluator<'db, M> {
    /// Runs the shim `name` if it is one of threads: `Ok(false)` when it is not.
    pub(super) fn exec_thread_shim(
        &mut self,
        name: &str,
        args: &[IntervalAndTy<'db>],
        destination: Interval,
        locals: &Locals<'db>,
        span: MirSpan,
    ) -> Result<'db, bool> {
        let arg = |i: usize| args.get(i).ok_or_else(|| missing(name));
        match name {
            "pthread_create" => self.pthread_create(args, destination, locals, span)?,
            "pthread_join" => {
                let [thread, retval] = args else { return Err(missing(name)) };
                self.pthread_join(thread, retval, destination)?
            }
            "pthread_detach" => {
                let thread = self.pthread_arg(arg(0)?)?;
                let Some(thread) = thread else {
                    let srch = self.target()?.errno.srch;
                    return self.ret_int(destination, srch.into()).map(|()| true);
                };
                self.detach_thread(thread)?;
                self.ret_int(destination, 0)?
            }
            "pthread_self" => {
                let me = self.thread();
                self.ret_int(destination, i128::from(me) + 1)?
            }
            "pthread_equal" => {
                let (a, b) = (self.uint_of(arg(0)?)?, self.uint_of(arg(1)?)?);
                self.ret_int(destination, (a == b).into())?
            }
            "pthread_setname_np" => self.pthread_setname(args, destination)?,
            "pthread_threadid_np" => {
                let thread = match self.uint_of(arg(0)?)? {
                    0 => Some(self.thread()),
                    _ => self.pthread_arg(arg(0)?)?,
                };
                let Some(thread) = thread else {
                    let srch = self.target()?.errno.srch;
                    return self.ret_int(destination, srch.into()).map(|()| true);
                };
                let out = self.ptr_of(arg(1)?)?;
                self.write_memory(out, &(u64::from(thread) + 1).to_le_bytes())?;
                self.ret_int(destination, 0)?
            }
            "gettid" => {
                let me = self.thread();
                self.ret_int(destination, i128::from(me) + 1)?
            }
            "pthread_attr_init" => {
                let attr = self.ptr_of(arg(0)?)?;
                let Target { os, page, .. } = self.target()?;
                self.attr_write(attr, 0, os.default_stack(), page, os.detach_states()[0])?;
                self.ret_int(destination, 0)?
            }
            "pthread_attr_destroy" => {
                let attr = self.attr_of(arg(0)?)?;
                self.poke_u32(attr, 0)?;
                self.ret_int(destination, 0)?
            }
            "pthread_attr_setstacksize" => {
                let attr = self.attr_of(arg(0)?)?;
                let size = self.uint_of(arg(1)?)? as usize;
                let target = self.target()?;
                let unaligned = target.os == Os::Apple && size % target.page != 0;
                if size < target.stack_min || unaligned {
                    return self.ret_int(destination, target.errno.inval.into()).map(|()| true);
                }
                self.poke_word(self.attr_field(attr, 2), size)?;
                self.ret_int(destination, 0)?
            }
            "pthread_attr_setguardsize" => {
                let attr = self.attr_of(arg(0)?)?;
                let size = self.uint_of(arg(1)?)? as usize;
                self.poke_word(self.attr_field(attr, 3), size)?;
                self.ret_int(destination, 0)?
            }
            "pthread_attr_getstacksize" | "pthread_attr_getguardsize" => {
                let attr = self.attr_of(arg(0)?)?;
                let field = if name == "pthread_attr_getstacksize" { 2 } else { 3 };
                let value = self.peek_word(self.attr_field(attr, field))?;
                let out = self.ptr_of(arg(1)?)?;
                self.write_memory(out, &word_bytes(value))?;
                self.ret_int(destination, 0)?
            }
            "pthread_attr_getstack" => {
                let attr = self.attr_of(arg(0)?)?;
                let addr = self.peek_word(self.attr_field(attr, 1))?;
                let size = self.peek_word(self.attr_field(attr, 2))?;
                let (addr_out, size_out) = (self.ptr_of(arg(1)?)?, self.ptr_of(arg(2)?)?);
                self.write_memory(addr_out, &word_bytes(addr))?;
                self.write_memory(size_out, &word_bytes(size))?;
                self.ret_int(destination, 0)?
            }
            "pthread_attr_setdetachstate" => {
                let attr = self.attr_of(arg(0)?)?;
                let state = self.uint_of(arg(1)?)?;
                let target = self.target()?;
                let Some(state) = target.os.detach_states().into_iter().find(|&it| u128::from(it) == state)
                else {
                    return self.ret_int(destination, target.errno.inval.into()).map(|()| true);
                };
                self.poke_u32(self.attr_field(attr, 4), state)?;
                self.ret_int(destination, 0)?
            }
            "pthread_attr_getdetachstate" => {
                let attr = self.attr_of(arg(0)?)?;
                let state = self.peek_u32(self.attr_field(attr, 4))?;
                let out = self.ptr_of(arg(1)?)?;
                self.write_memory(out, &state.to_le_bytes())?;
                self.ret_int(destination, 0)?
            }
            "pthread_getattr_np" => {
                let Some(thread) = self.pthread_arg(arg(0)?)? else {
                    let srch = self.target()?.errno.srch;
                    return self.ret_int(destination, srch.into()).map(|()| true);
                };
                let attr = self.ptr_of(arg(1)?)?;
                let (low, size) = self.thread_stack(thread);
                let target = self.target()?;
                let state = target.os.detach_states()[usize::from(!self.joinable(thread))];
                self.attr_write(attr, low, size, target.page, state)?;
                self.ret_int(destination, 0)?
            }
            "pthread_get_stackaddr_np" | "pthread_get_stacksize_np" => {
                let Some(thread) = self.pthread_arg(arg(0)?)? else {
                    not_supported!("{name} of a thread that was never made")
                };
                let (low, size) = self.thread_stack(thread);
                // Apple's stack address is the top: the stack grows down from it.
                let value = if name == "pthread_get_stackaddr_np" { low + size } else { size };
                destination.write_from_bytes(self, &word_bytes(value))?
            }
            "pthread_key_create" => self.pthread_key_create(args, destination, locals)?,
            "pthread_key_delete" => {
                let key = self.uint_of(arg(0)?)? as usize;
                let inval = self.target()?.errno.inval;
                let code = match self.process.keys.get_mut(key) {
                    Some(it) if it.live => {
                        it.live = false;
                        0
                    }
                    _ => inval,
                };
                self.ret_int(destination, code.into())?
            }
            "pthread_getspecific" => {
                let key = self.live_key(arg(0)?, name)?;
                let value = self.tls().values.get(key).copied().unwrap_or_else(null);
                self.write_pointer(destination.addr, value)?
            }
            "pthread_setspecific" => {
                let key = self.live_key(arg(0)?, name)?;
                let value = self.ptr_of(arg(1)?)?;
                let values = &mut self.tls().values;
                if values.len() <= key {
                    values.resize(key + 1, null());
                }
                values[key] = value;
                self.ret_int(destination, 0)?
            }
            "__cxa_thread_atexit_impl" => {
                let func = self.uint_of(arg(0)?)? as usize;
                let obj = self.ptr_of(arg(1)?)?;
                self.at_thread_exit(func, obj);
                self.ret_int(destination, 0)?
            }
            "_tlv_atexit" => {
                let func = self.uint_of(arg(0)?)? as usize;
                let obj = self.ptr_of(arg(1)?)?;
                self.at_thread_exit(func, obj);
            }
            "pthread_mutexattr_init" => {
                let attr = self.ptr_of(arg(0)?)?;
                let kind_at = match self.target()?.os {
                    Os::Linux => 0,
                    Os::Apple => {
                        self.poke_word(attr, APPLE_MUTEXATTR_SIG)?;
                        self.ptr_size()
                    }
                };
                self.poke_u32(attr.offset(kind_at), MutexKind::Normal as u32)?;
                self.ret_int(destination, 0)?
            }
            "pthread_mutexattr_settype" => {
                let attr = self.ptr_of(arg(0)?)?;
                let ty = self.int_of(arg(1)?)?;
                let target = self.target()?;
                let Some(kind) = target.os.mutex_kind(ty) else {
                    return self.ret_int(destination, target.errno.inval.into()).map(|()| true);
                };
                let kind_at = self.mutexattr_kind_at(attr)?;
                self.poke_u32(attr.offset(kind_at), kind as u32)?;
                self.ret_int(destination, 0)?
            }
            "pthread_mutexattr_destroy" | "pthread_condattr_destroy" => {
                self.ret_int(destination, 0)?
            }
            "pthread_mutex_init" => {
                let mutex = self.ptr_of(arg(0)?)?;
                let attr = self.ptr_of(arg(1)?)?;
                let kind = match attr.to_usize() {
                    0 => MutexKind::Normal,
                    _ => {
                        let kind_at = self.mutexattr_kind_at(attr)?;
                        let raw = self.peek_u32(attr.offset(kind_at))?;
                        let Some(kind) = MutexKind::of(raw) else {
                            return Err(MirEvalError::UndefinedBehavior(
                                "pthread_mutex_init with attributes pthread_mutexattr_init did \
                                 not initialize"
                                    .to_owned(), // alloc: the error that ends the run.
                            ));
                        };
                        kind
                    }
                };
                if self.target()?.os == Os::Apple {
                    self.poke_word(mutex, APPLE_MUTEX_SIG)?;
                }
                self.mutex_set(mutex, MutexState { kind, owner: 0, count: 0 })?;
                self.ret_int(destination, 0)?
            }
            "pthread_mutex_lock" => {
                let mutex = self.ptr_of(arg(0)?)?;
                self.mutex_lock(mutex, 0, destination)?
            }
            "pthread_mutex_trylock" => {
                let mutex = self.ptr_of(arg(0)?)?;
                let mut state = self.mutex_get(mutex)?;
                let me = self.thread() + 1;
                let code = match state.owner {
                    0 => {
                        state.owner = me;
                        state.count = 1;
                        self.mutex_set(mutex, state)?;
                        self.machine.acquire(mutex.to_usize() as u64)?;
                        0
                    }
                    owner if owner == me && state.kind == MutexKind::Recursive => {
                        state.count += 1;
                        self.mutex_set(mutex, state)?;
                        0
                    }
                    _ => self.target()?.errno.busy,
                };
                self.ret_int(destination, code.into())?
            }
            "pthread_mutex_unlock" => {
                let mutex = self.ptr_of(arg(0)?)?;
                let code = self.mutex_unlock(mutex)?;
                self.ret_int(destination, code.into())?
            }
            "pthread_mutex_destroy" => {
                let mutex = self.ptr_of(arg(0)?)?;
                let state = self.mutex_get(mutex)?;
                if state.owner != 0 {
                    let busy = self.target()?.errno.busy;
                    return self.ret_int(destination, busy.into()).map(|()| true);
                }
                if self.target()?.os == Os::Apple {
                    self.poke_word(mutex, 0)?;
                }
                self.ret_int(destination, 0)?
            }
            "pthread_condattr_init" => {
                let attr = self.ptr_of(arg(0)?)?;
                match self.target()?.os {
                    Os::Linux => self.poke_u32(attr, 0)?,
                    Os::Apple => self.poke_word(attr, 0)?,
                }
                self.ret_int(destination, 0)?
            }
            "pthread_condattr_setclock" => {
                let attr = self.ptr_of(arg(0)?)?;
                let clock = self.int_of(arg(1)?)?;
                let target = self.target()?;
                if target.os != Os::Linux || !matches!(clock, 0 | 1) {
                    return self.ret_int(destination, target.errno.inval.into()).map(|()| true);
                }
                self.poke_u32(attr, clock as u32)?;
                self.ret_int(destination, 0)?
            }
            "pthread_cond_init" => {
                let cond = self.ptr_of(arg(0)?)?;
                let attr = self.ptr_of(arg(1)?)?;
                let target = self.target()?;
                let clock = match (target.os, attr.to_usize()) {
                    (Os::Linux, 0) | (Os::Apple, _) => 0,
                    (Os::Linux, _) => self.peek_u32(attr)?,
                };
                let layout = self.cond_layout(target.os);
                if target.os == Os::Apple {
                    self.poke_word(cond, APPLE_COND_SIG)?;
                }
                self.poke_u32(cond.offset(layout.clock), clock)?;
                self.poke_u32(cond.offset(layout.waiters), 0)?;
                self.poke_word(cond.offset(layout.mutex), 0)?;
                self.ret_int(destination, 0)?
            }
            "pthread_cond_wait" => {
                let (cond, mutex) = (self.ptr_of(arg(0)?)?, self.ptr_of(arg(1)?)?);
                self.cond_wait(cond, mutex, None, destination)?
            }
            "pthread_cond_timedwait" | "pthread_cond_timedwait_relative_np" => {
                let (cond, mutex) = (self.ptr_of(arg(0)?)?, self.ptr_of(arg(1)?)?);
                let time = self.ptr_of(arg(2)?)?;
                let target = self.target()?;
                let Some(ns) = self.timespec(time)? else {
                    return self.ret_int(destination, target.errno.inval.into()).map(|()| true);
                };
                let deadline = match name {
                    "pthread_cond_timedwait_relative_np" => self.now().saturating_add(ns),
                    _ => {
                        let layout = self.cond_layout(target.os);
                        self.cond_ready(cond)?;
                        let clock = self.peek_u32(cond.offset(layout.clock))?;
                        let Some(base) = target.os.clock_base(clock.into()) else {
                            return self
                                .ret_int(destination, target.errno.inval.into())
                                .map(|()| true);
                        };
                        ns.saturating_sub(base)
                    }
                };
                self.cond_wait(cond, mutex, Some(deadline), destination)?
            }
            "pthread_cond_signal" | "pthread_cond_broadcast" => {
                let cond = self.ptr_of(arg(0)?)?;
                self.cond_ready(cond)?;
                let count = if name == "pthread_cond_signal" { 1 } else { usize::MAX };
                self.wake(cond.to_usize(), u32::MAX, count)?;
                self.ret_int(destination, 0)?
            }
            "pthread_cond_destroy" => {
                let cond = self.ptr_of(arg(0)?)?;
                self.cond_ready(cond)?;
                let target = self.target()?;
                let layout = self.cond_layout(target.os);
                if self.peek_u32(cond.offset(layout.waiters))? != 0 {
                    return self.ret_int(destination, target.errno.busy.into()).map(|()| true);
                }
                if target.os == Os::Apple {
                    self.poke_word(cond, 0)?;
                }
                self.ret_int(destination, 0)?
            }
            "syscall" => {
                let Some((id, rest)) = args.split_first() else { return Err(missing(name)) };
                let id = self.int_of(id)?;
                let target = self.target()?;
                if id == i128::from(target.sys_futex) {
                    self.exec_futex(rest, destination)?
                } else if id == i128::from(target.sys_gettid) {
                    let me = self.thread();
                    self.ret_int(destination, i128::from(me) + 1)?
                } else {
                    return Ok(false);
                }
            }
            "__ulock_wait" | "__ulock_wait2" => {
                let op = self.uint_of(arg(0)?)? as u32;
                let addr = self.ptr_of(arg(1)?)?;
                let value = self.uint_of(arg(2)?)? as u64;
                let timeout = self.uint_of(arg(3)?)? as u64;
                let timeout = if name == "__ulock_wait" { timeout * 1000 } else { timeout };
                self.ulock_wait(op, addr, value, timeout, destination)?
            }
            "__ulock_wake" => {
                let op = self.uint_of(arg(0)?)? as u32;
                let addr = self.ptr_of(arg(1)?)?;
                let errors = if op & ULF_NO_ERRNO != 0 { Errors::Negated } else { Errors::Errno };
                if op & ULF_WAKE_THREAD != 0 {
                    not_supported!("__ulock_wake of one thread (ULF_WAKE_THREAD)")
                }
                let count = if op & ULF_WAKE_ALL != 0 { usize::MAX } else { 1 };
                match self.wake(addr.to_usize(), u32::MAX, count)? {
                    0 => {
                        let noent = self.target()?.errno.noent;
                        self.fail(destination, errors, noent)?
                    }
                    _ => self.ret_int(destination, 0)?,
                }
            }
            "os_sync_wait_on_address" | "os_sync_wait_on_address_with_timeout" => {
                let addr = self.ptr_of(arg(0)?)?;
                let value = self.uint_of(arg(1)?)? as u64;
                let size = self.uint_of(arg(2)?)? as usize;
                let flags = self.uint_of(arg(3)?)? as u32;
                let inval = self.target()?.errno.inval;
                if !matches!(size, 4 | 8) || flags & !OS_SYNC_SHARED != 0 {
                    return self.fail(destination, Errors::Errno, inval).map(|()| true);
                }
                let deadline = match name {
                    "os_sync_wait_on_address" => None,
                    _ => {
                        let clock = self.uint_of(arg(4)?)? as u32;
                        let timeout = self.uint_of(arg(5)?)? as u64;
                        if clock != OS_CLOCK_MACH_ABSOLUTE_TIME || timeout == 0 {
                            return self.fail(destination, Errors::Errno, inval).map(|()| true);
                        }
                        Some(self.now().saturating_add(timeout))
                    }
                };
                let value = if size == 4 { value & u64::from(u32::MAX) } else { value };
                let wait = FutexWait { addr, size, expected: value, bitset: u32::MAX, deadline };
                self.futex_wait(wait, None, Errors::Errno, destination)?
            }
            "os_sync_wake_by_address_any" | "os_sync_wake_by_address_all" => {
                let addr = self.ptr_of(arg(0)?)?;
                let size = self.uint_of(arg(1)?)? as usize;
                let flags = self.uint_of(arg(2)?)? as u32;
                let target = self.target()?;
                if !matches!(size, 4 | 8) || flags & !OS_SYNC_SHARED != 0 {
                    return self.fail(destination, Errors::Errno, target.errno.inval).map(|()| true);
                }
                let count = if name == "os_sync_wake_by_address_any" { 1 } else { usize::MAX };
                match self.wake(addr.to_usize(), u32::MAX, count)? {
                    0 => self.fail(destination, Errors::Errno, target.errno.noent)?,
                    _ => self.ret_int(destination, 0)?,
                }
            }
            "dispatch_semaphore_create" => {
                let value = self.int_of(arg(0)?)?;
                if value < 0 {
                    return self.write_pointer(destination.addr, null()).map(|()| true);
                }
                let semaphore = self.allocate(16, 8, Origin::Machine, true)?;
                self.poke_memory(semaphore, 8)?.copy_from_slice(&(value as i64).to_le_bytes());
                self.poke_memory(semaphore.offset(8), 8)?
                    .copy_from_slice(&SEMAPHORE_MAGIC.to_le_bytes());
                self.write_pointer(destination.addr, semaphore)?
            }
            "dispatch_semaphore_wait" => {
                let semaphore = self.semaphore_of(arg(0)?)?;
                let deadline = match self.uint_of(arg(1)?)? as u64 {
                    DISPATCH_TIME_FOREVER => None,
                    at => Some(at),
                };
                self.semaphore_wait(semaphore, deadline, destination)?
            }
            "dispatch_semaphore_signal" => {
                let semaphore = self.semaphore_of(arg(0)?)?;
                let count = from_bytes!(i64, self.peek_memory(semaphore, 8)?);
                self.poke_memory(semaphore, 8)?.copy_from_slice(&(count + 1).to_le_bytes());
                let woken = self.wake(semaphore.to_usize(), u32::MAX, 1)?;
                self.ret_int(destination, (woken != 0).into())?
            }
            "dispatch_time" => {
                let when = self.uint_of(arg(0)?)? as u64;
                let delta = self.int_of(arg(1)?)?;
                let at = match when {
                    DISPATCH_TIME_FOREVER => DISPATCH_TIME_FOREVER,
                    _ => {
                        let base = if when == 0 { self.now() } else { when };
                        let at = i128::from(base) + delta;
                        at.clamp(0, i128::from(DISPATCH_TIME_FOREVER - 1)) as u64
                    }
                };
                self.ret_int(destination, at.into())?
            }
            "dispatch_release" => {
                let semaphore = self.semaphore_of(arg(0)?)?;
                self.poke_memory(semaphore.offset(8), 8)?.fill(0);
                self.deallocate(semaphore, Origin::Machine, "dispatch_release")?;
            }
            "sched_yield" => {
                self.yield_thread();
                self.ret_int(destination, 0)?
            }
            "sched_getaffinity" => {
                let size = self.uint_of(arg(1)?)? as usize;
                let set = self.ptr_of(arg(2)?)?;
                if size == 0 {
                    let inval = self.target()?.errno.inval;
                    return self.fail(destination, Errors::Errno, inval).map(|()| true);
                }
                let mask = self.write_memory_using_ref(set, size)?;
                mask.fill(0);
                mask[0] = 1;
                self.ret_int(destination, 0)?
            }
            "sysconf" => {
                let what = self.int_of(arg(0)?)?;
                let target = self.target()?;
                let value = if what == target.os.sc_pagesize() {
                    target.page as i128
                } else if target.os.sc_nprocessors().contains(&what) {
                    CPUS
                } else {
                    not_supported!("sysconf({what})")
                };
                self.ret_int(destination, value)?
            }
            "getauxval" => {
                let what = self.uint_of(arg(0)?)?;
                let target = self.target()?;
                match what {
                    AT_PAGESZ => self.ret_int(destination, target.page as i128)?,
                    // A kernel that does not say: std then takes `SIGSTKSZ`.
                    AT_MINSIGSTKSZ => {
                        self.set_errno(target.errno.noent)?;
                        self.ret_int(destination, 0)?
                    }
                    _ => not_supported!("getauxval({what})"),
                }
            }
            "dlsym" => {
                let symbol = self.ptr_of(arg(1)?)?;
                let len = self.c_str_len(symbol)?;
                let symbol = self.read_memory(symbol, len)?;
                // glibc's private `PTHREAD_STACK_MIN` plus TLS: std takes `PTHREAD_STACK_MIN`
                // where libc has none.
                if symbol != b"__pthread_get_minstack" {
                    // alloc: the error that ends the run.
                    let symbol = String::from_utf8_lossy(symbol);
                    not_supported!("dlsym of `{symbol}`")
                }
                self.write_pointer(destination.addr, null())?
            }
            "__errno_location" | "__error" => {
                let cell = self.errno_cell()?;
                self.write_pointer(destination.addr, cell)?
            }
            "clock_gettime" => {
                let clock = self.int_of(arg(0)?)?;
                let out = self.ptr_of(arg(1)?)?;
                let target = self.target()?;
                let Some(base) = target.os.clock_base(clock) else {
                    return self.fail(destination, Errors::Errno, target.errno.inval).map(|()| true);
                };
                let ns = base + self.now();
                self.write_timespec(out, ns)?;
                self.ret_int(destination, 0)?
            }
            "nanosleep" => {
                let request = self.ptr_of(arg(0)?)?;
                let Some(ns) = self.timespec(request)? else {
                    let inval = self.target()?.errno.inval;
                    return self.fail(destination, Errors::Errno, inval).map(|()| true);
                };
                self.sleep_for(ns);
                self.ret_int(destination, 0)?
            }
            "clock_nanosleep" => {
                let clock = self.int_of(arg(0)?)?;
                let flags = self.int_of(arg(1)?)? as i32;
                let request = self.ptr_of(arg(2)?)?;
                let target = self.target()?;
                let (Some(base), Some(ns)) = (target.os.clock_base(clock), self.timespec(request)?)
                else {
                    return self.ret_int(destination, target.errno.inval.into()).map(|()| true);
                };
                let ns = match flags & TIMER_ABSTIME {
                    0 => ns,
                    _ => ns.saturating_sub(base + self.now()),
                };
                self.sleep_for(ns);
                self.ret_int(destination, 0)?
            }
            "mmap" | "mmap64" => self.mmap(args, destination)?,
            "munmap" => {
                let addr = self.ptr_of(arg(0)?)?;
                let len = self.uint_of(arg(1)?)? as usize;
                let page = self.target()?.page;
                let block = match addr.at() {
                    At::Heap(pos) => self.heap_blocks.get(&pos).copied(),
                    _ => None,
                };
                let at = addr.to_usize();
                match block {
                    Some(block)
                        if block.origin == Origin::Mmap
                            && len.checked_next_multiple_of(page) == Some(block.size) =>
                    {
                        self.deallocate(addr, Origin::Mmap, "munmap")?
                    }
                    _ => not_supported!(
                        "munmap of {len} bytes at {at:#x}: only a whole mapping mmap made"
                    ),
                }
                self.ret_int(destination, 0)?
            }
            "mprotect" => {
                let addr = self.ptr_of(arg(0)?)?;
                let len = self.uint_of(arg(1)?)? as usize;
                let page = self.target()?.page;
                let mapped = self.heap_blocks.range(..=addr.offset).next_back().is_some_and(
                    |(&start, block)| {
                        block.origin == Origin::Mmap
                            && addr.offset + len <= start + block.size
                    },
                );
                let at = addr.to_usize();
                if !matches!(addr.at(), At::Heap(_)) || !mapped || at % page != 0 {
                    not_supported!("mprotect of {len} bytes at {at:#x}: only pages mmap mapped")
                }
                self.ret_int(destination, 0)?
            }
            "sigaltstack" => self.sigaltstack(args, destination)?,
            "sigaction" => self.sigaction(args, destination, locals)?,
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Goes on with a call that blocked, on its thread, once it is woken or `timed_out`;
    /// `destination` is the call's. It may block again ([`Self::block_on`]).
    pub(in crate::mir::eval) fn resume_shim(
        &mut self,
        resume: Resume,
        timed_out: bool,
        destination: Interval,
    ) -> Result<'db, ()> {
        match resume {
            Resume::Wait { errors } => match timed_out {
                true => {
                    let timedout = self.target()?.errno.timedout;
                    self.fail(destination, errors, timedout)
                }
                false => self.ret_int(destination, 0),
            },
            Resume::MutexLock { mutex, result } => self.mutex_lock(mutex, result, destination),
            Resume::CondWait { cond, mutex } => {
                let os = self.target()?.os;
                let layout = self.cond_layout(os);
                let waiters = self.peek_u32(cond.offset(layout.waiters))?.saturating_sub(1);
                self.poke_u32(cond.offset(layout.waiters), waiters)?;
                if waiters == 0 {
                    self.poke_word(cond.offset(layout.mutex), 0)?;
                }
                let result = if timed_out { self.target()?.errno.timedout } else { 0 };
                self.mutex_lock(mutex, result, destination)
            }
            Resume::Semaphore { semaphore, deadline } => match timed_out {
                true => self.ret_int(destination, SEMAPHORE_TIMED_OUT),
                false => self.semaphore_wait(semaphore, deadline, destination),
            },
        }
    }

    /// The next of the running thread's key destructors to call as it exits, and the value
    /// to call it with, which the key no longer holds; `None` once they are all run.
    pub(in crate::mir::eval) fn next_key_destructor(&mut self) -> Option<(usize, Address)> {
        loop {
            let pass = self.tls().destructors;
            if pass.next_key >= self.process.keys.len() {
                if !pass.found || pass.pass + 1 >= DESTRUCTOR_ITERATIONS {
                    return None;
                }
                self.tls().destructors = DestructorPass { next_key: 0, pass: pass.pass + 1, found: false };
                continue;
            }
            let key_id = pass.next_key;
            let key = self.process.keys[key_id];
            let tls = self.tls();
            tls.destructors.next_key = key_id + 1;
            if !key.live || key.destructor == 0 {
                continue;
            }
            let Some(value) = tls.values.get_mut(key_id) else { continue };
            if value.to_usize() == 0 {
                continue;
            }
            let value = std::mem::replace(value, null());
            tls.destructors.found = true;
            return Some((key.destructor, value));
        }
    }

    pub(super) fn target(&mut self) -> Result<'db, Target> {
        if let Some(target) = self.process.target {
            return Ok(target);
        }
        let cfg = self.crate_id.cfg_options(self.db);
        let is = |key: &str, value: &str| {
            cfg.check_atom(&CfgAtom::KeyValue { key: Symbol::intern(key), value: Symbol::intern(value) })
        };
        let os = if is("target_os", "linux") || is("target_os", "android") {
            Os::Linux
        } else if is("target_vendor", "apple") {
            Os::Apple
        } else {
            not_supported!("threads on a target that is neither Linux nor Apple's")
        };
        let arch = ["x86_64", "x86", "arm", "aarch64", "riscv64", "loongarch64"]
            .into_iter()
            .find(|arch| is("target_arch", arch));
        let Some(arch) = arch else { not_supported!("threads on this target_arch") };
        let (sys_futex, sys_gettid) = match arch {
            "x86_64" => (202, 186),
            "x86" | "arm" => (240, 224),
            _ => (98, 178),
        };
        let page = if os == Os::Apple && arch == "aarch64" { 16384 } else { 4096 };
        let stack_min = match (os, arch) {
            (Os::Linux, "aarch64" | "riscv64" | "loongarch64") => 131072,
            (Os::Apple, "x86_64") => 8192,
            _ => 16384,
        };
        let errno = if os == Os::Linux { LINUX_ERRNO } else { APPLE_ERRNO };
        let target = Target { os, sys_futex, sys_gettid, page, stack_min, errno };
        self.process.target = Some(target);
        Ok(target)
    }

    fn pthread_create(
        &mut self,
        args: &[IntervalAndTy<'db>],
        destination: Interval,
        locals: &Locals<'db>,
        span: MirSpan,
    ) -> Result<'db, ()> {
        let [thread, attr, start, arg] = args else { return Err(missing("pthread_create")) };
        let out = self.ptr_of(thread)?;
        let out_size = self.pointee_size(thread, locals, "pthread_create's pthread_t")?;
        let attr = self.ptr_of(attr)?;
        let detached = match attr.to_usize() {
            0 => false,
            _ => {
                self.attr_ready(attr)?;
                let state = self.peek_u32(self.attr_field(attr, 4))?;
                state == self.target()?.os.detach_states()[1]
            }
        };
        let start = from_bytes!(usize, start.get(self)?);
        let arg = self.ptr_of(arg)?;
        let spawned = self.spawn_thread(start, arg, span)?;
        self.write_int(out, out_size, i128::from(spawned) + 1)?;
        if detached {
            self.detach_thread(spawned)?;
        }
        self.ret_int(destination, 0)
    }

    fn pthread_join(
        &mut self,
        thread: &IntervalAndTy<'db>,
        retval: &IntervalAndTy<'db>,
        destination: Interval,
    ) -> Result<'db, ()> {
        let errno = self.target()?.errno;
        let Some(thread) = self.pthread_arg(thread)? else {
            return self.ret_int(destination, errno.srch.into());
        };
        if thread == self.thread() {
            return self.ret_int(destination, errno.deadlk.into());
        }
        if !self.joinable(thread) {
            // alloc: the error that ends the run.
            return Err(MirEvalError::UndefinedBehavior(format!(
                "pthread_join of thread {thread}, which is detached, joined already, or was \
                 never made"
            )));
        }
        let retval = self.ptr_of(retval)?;
        self.block_on(Wait::Join { thread, retval });
        Ok(())
    }

    /// `pthread_setname_np(thread, name)` on Linux, `pthread_setname_np(name)` of the running
    /// thread on Apple's.
    fn pthread_setname(
        &mut self,
        args: &[IntervalAndTy<'db>],
        destination: Interval,
    ) -> Result<'db, ()> {
        let target = self.target()?;
        let (thread, name) = match (target.os, args) {
            (Os::Linux, [thread, name]) => (self.pthread_arg(thread)?, name),
            (Os::Apple, [name]) => (Some(self.thread()), name),
            _ => return Err(missing("pthread_setname_np")),
        };
        let Some(thread) = thread else {
            return self.ret_int(destination, target.errno.srch.into());
        };
        let name = self.ptr_of(name)?;
        let len = self.c_str_len(name)?;
        // Linux's `TASK_COMM_LEN` (16) and Apple's `MAXTHREADNAMESIZE` (64), with the NUL.
        let max = if target.os == Os::Linux { 15 } else { 63 };
        if len > max {
            return self.ret_int(destination, target.errno.range.into());
        }
        let bytes = self.read_memory(name, len)?;
        self.machine.thread_name(thread, bytes);
        self.ret_int(destination, 0)
    }

    fn pthread_key_create(
        &mut self,
        args: &[IntervalAndTy<'db>],
        destination: Interval,
        locals: &Locals<'db>,
    ) -> Result<'db, ()> {
        let [key, destructor] = args else { return Err(missing("pthread_key_create")) };
        let out = self.ptr_of(key)?;
        let size = self.pointee_size(key, locals, "pthread_key_create's key")?;
        let destructor = self.uint_of(destructor)? as usize;
        let target = self.target()?;
        let live = self.process.keys.iter().filter(|it| it.live).count();
        if live >= target.os.keys_max() {
            return self.ret_int(destination, target.errno.again.into());
        }
        let id = self.process.keys.len();
        self.process.keys.push(Key { destructor, live: true });
        self.write_int(out, size, id as i128)?;
        self.ret_int(destination, 0)
    }

    /// The key `arg` names, which must be live.
    fn live_key(&self, arg: &IntervalAndTy<'db>, what: &str) -> Result<'db, usize> {
        let key = self.uint_of(arg)? as usize;
        match self.process.keys.get(key) {
            Some(it) if it.live => Ok(key),
            // alloc: the error that ends the run.
            _ => Err(MirEvalError::UndefinedBehavior(format!(
                "{what} of key {key}, which was deleted or never made"
            ))),
        }
    }

    fn mmap(&mut self, args: &[IntervalAndTy<'db>], destination: Interval) -> Result<'db, ()> {
        let [_addr, len, _prot, flags, fd, _offset] = args else { return Err(missing("mmap")) };
        let len = self.uint_of(len)? as usize;
        let flags = self.int_of(flags)? as i32;
        let fd = self.int_of(fd)?;
        let target = self.target()?;
        if flags & target.os.map_anonymous() == 0 || fd != -1 {
            not_supported!("mmap of a file: only anonymous mappings are")
        }
        if flags & MAP_FIXED != 0 {
            not_supported!("mmap with MAP_FIXED")
        }
        let Some(size) = len.checked_next_multiple_of(target.page).filter(|&it| it != 0) else {
            let code = if len == 0 { target.errno.inval } else { target.errno.nomem };
            return self.fail(destination, Errors::Errno, code);
        };
        let mapping = self.allocate(size, target.page, Origin::Mmap, true)?;
        self.write_pointer(destination.addr, mapping)
    }

    fn sigaltstack(
        &mut self,
        args: &[IntervalAndTy<'db>],
        destination: Interval,
    ) -> Result<'db, ()> {
        let [new, old] = args else { return Err(missing("sigaltstack")) };
        let (new, old) = (self.ptr_of(new)?, self.ptr_of(old)?);
        let target = self.target()?;
        let word = self.ptr_size();
        let (flags_at, size_at) = match target.os {
            Os::Linux => (word, 2 * word),
            Os::Apple => (2 * word, word),
        };
        let disable = target.os.ss_disable();
        let set = match new.to_usize() {
            0 => None,
            _ => {
                let flags = from_bytes!(i32, self.read_memory(new.offset(flags_at), 4)?);
                match flags & disable {
                    0 => {
                        let stack = self.read_pointer(new)?;
                        let size = from_bytes!(usize, self.read_memory(new.offset(size_at), word)?);
                        if size < target.os.minsigstksz() {
                            return self.fail(destination, Errors::Errno, target.errno.nomem);
                        }
                        Some(Some((stack, size)))
                    }
                    _ => Some(None),
                }
            }
        };
        if old.to_usize() != 0 {
            let current = self.tls().altstack;
            let (stack, size, flags) = match current {
                Some((stack, size)) => (stack, size, 0),
                None => (null(), 0, disable),
            };
            self.write_pointer(old, stack)?;
            self.write_memory(old.offset(size_at), &word_bytes(size))?;
            self.write_memory(old.offset(flags_at), &flags.to_le_bytes())?;
        }
        if let Some(set) = set {
            self.tls().altstack = set;
        }
        self.ret_int(destination, 0)
    }

    fn sigaction(
        &mut self,
        args: &[IntervalAndTy<'db>],
        destination: Interval,
        locals: &Locals<'db>,
    ) -> Result<'db, ()> {
        let [signal, act, old] = args else { return Err(missing("sigaction")) };
        let signal = self.int_of(signal)?;
        let target = self.target()?;
        if !(1..=64).contains(&signal) || signal == 9 || signal == target.os.sigstop() {
            return self.fail(destination, Errors::Errno, target.errno.inval);
        }
        let size = self.pointee_size(act, locals, "sigaction's struct sigaction")?;
        let (act, old) = (self.ptr_of(act)?, self.ptr_of(old)?);
        let signal = signal as usize;
        if old.to_usize() != 0 {
            // alloc: `sigaction` is setup.
            let installed = self.process.actions.get(signal).cloned().flatten();
            match installed {
                Some(bytes) if bytes.len() == size => self.write_memory(old, &bytes)?,
                Some(_) => not_supported!("sigaction with two sizes of struct sigaction"),
                None => self.write_memory_using_ref(old, size)?.fill(0),
            }
        }
        if act.to_usize() != 0 {
            // alloc: `sigaction` is setup.
            let bytes: Box<[u8]> = self.read_memory(act, size)?.into();
            let actions = &mut self.process.actions;
            if actions.len() <= signal {
                actions.resize(signal + 1, None);
            }
            actions[signal] = Some(bytes);
        }
        self.ret_int(destination, 0)
    }

    fn exec_futex(&mut self, args: &[IntervalAndTy<'db>], destination: Interval) -> Result<'db, ()> {
        let arg = |i: usize| args.get(i).ok_or_else(|| missing("SYS_futex"));
        let target = self.target()?;
        let addr = self.ptr_of(arg(0)?)?;
        let op = self.int_of(arg(1)?)? as i32;
        let cmd = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
        let inval = target.errno.inval;
        let bitset = match cmd {
            FUTEX_WAIT | FUTEX_WAKE => u32::MAX,
            FUTEX_WAIT_BITSET | FUTEX_WAKE_BITSET => self.uint_of(arg(5)?)? as u32,
            _ => not_supported!("futex operation {cmd}"),
        };
        if bitset == 0 {
            return self.fail(destination, Errors::Errno, inval);
        }
        if let FUTEX_WAKE | FUTEX_WAKE_BITSET = cmd {
            let count = self.int_of(arg(2)?)?.clamp(1, i128::from(i32::MAX)) as usize;
            let woken = self.wake(addr.to_usize(), bitset, count)?;
            return self.ret_int(destination, woken as i128);
        }
        let expected = self.uint_of(arg(2)?)? as u32;
        let timeout = self.ptr_of(arg(3)?)?;
        let deadline = match timeout.to_usize() {
            0 => None,
            _ => {
                let Some(ns) = self.timespec(timeout)? else {
                    return self.fail(destination, Errors::Errno, inval);
                };
                Some(match cmd {
                    FUTEX_WAIT => self.now().saturating_add(ns),
                    _ if op & FUTEX_CLOCK_REALTIME != 0 => ns.saturating_sub(REALTIME_START_NS),
                    _ => ns.saturating_sub(MONOTONIC_START_NS),
                })
            }
        };
        let wait = FutexWait { addr, size: 4, expected: expected.into(), bitset, deadline };
        self.futex_wait(wait, Some(target.errno.again), Errors::Errno, destination)
    }

    fn ulock_wait(
        &mut self,
        op: u32,
        addr: Address,
        value: u64,
        timeout_ns: u64,
        destination: Interval,
    ) -> Result<'db, ()> {
        let errors = if op & ULF_NO_ERRNO != 0 { Errors::Negated } else { Errors::Errno };
        // `UL_COMPARE_AND_WAIT` and `UL_COMPARE_AND_WAIT_SHARED`, then their 64-bit forms;
        // the unfair locks have an owner protocol of their own.
        let (size, expected) = match op & 0xff {
            1 | 3 => (4, value & u64::from(u32::MAX)),
            5 | 6 => (8, value),
            other => not_supported!("__ulock_wait operation {other}"),
        };
        let deadline = (timeout_ns != 0).then(|| self.now().saturating_add(timeout_ns));
        let wait = FutexWait { addr, size, expected, bitset: u32::MAX, deadline };
        self.futex_wait(wait, None, errors, destination)
    }

    /// Blocks on the futex word of `wait` unless it holds another value than `expected`, as
    /// the kernel does: then gives `mismatch` as an error, or 0 when that is `None`.
    fn futex_wait(
        &mut self,
        wait: FutexWait,
        mismatch: Option<i32>,
        errors: Errors,
        destination: Interval,
    ) -> Result<'db, ()> {
        let errno = self.target()?.errno;
        let FutexWait { addr, size, expected, bitset, deadline } = wait;
        if addr.to_usize() % size != 0 {
            return self.fail(destination, errors, errno.inval);
        }
        if self.futex_load(addr, size)? != expected {
            return match mismatch {
                Some(code) => self.fail(destination, errors, code),
                None => self.ret_int(destination, 0),
            };
        }
        if deadline.is_some_and(|it| it <= self.now()) {
            return self.fail(destination, errors, errno.timedout);
        }
        let resume = Resume::Wait { errors };
        self.block_on(Wait::Shim { addr: addr.to_usize(), bitset, deadline, resume });
        Ok(())
    }

    /// The futex word at `addr`, read as the kernel does: a sequentially consistent fence,
    /// then a relaxed atomic load.
    fn futex_load(&mut self, addr: Address, size: usize) -> Result<'db, u64> {
        let mut latest = [0; 8];
        latest[..size].copy_from_slice(self.peek_memory(addr, size)?);
        if M::CHECKS {
            self.machine.fence(AtomicOrd::SeqCst, false)?;
            let ptr = self.ptr(addr);
            let access = Access::Atomic { ord: AtomicOrd::Relaxed, rmw: false, cxchg_failed: false };
            let shadow = self.machine.read(ptr, size as u64, size as u64, access)?;
            self.machine.require_init(&shadow, 0..size)?;
            let older = self.machine.atomic_load(ptr, size as u64, AtomicOrd::Relaxed, &latest[..size])?;
            if let Some(older) = older {
                latest[..size].copy_from_slice(&older);
            }
        }
        Ok(u64::from_le_bytes(latest))
    }

    /// Takes `mutex` for the running thread and gives `result`, or blocks until it can.
    fn mutex_lock(&mut self, mutex: Address, result: i32, destination: Interval) -> Result<'db, ()> {
        let mut state = self.mutex_get(mutex)?;
        let me = self.thread() + 1;
        if state.owner == 0 {
            state.owner = me;
            state.count = 1;
            self.mutex_set(mutex, state)?;
            self.machine.acquire(mutex.to_usize() as u64)?;
            return self.ret_int(destination, result.into());
        }
        if state.owner == me {
            match state.kind {
                MutexKind::Recursive => {
                    let Some(count) = state.count.checked_add(1) else {
                        let again = self.target()?.errno.again;
                        return self.ret_int(destination, again.into());
                    };
                    state.count = count;
                    self.mutex_set(mutex, state)?;
                    return self.ret_int(destination, result.into());
                }
                MutexKind::ErrorCheck => {
                    let deadlk = self.target()?.errno.deadlk;
                    return self.ret_int(destination, deadlk.into());
                }
                // A normal mutex locked again by its owner deadlocks, as in glibc: the thread
                // waits for an unlock nobody makes.
                MutexKind::Normal => {}
            }
        }
        let resume = Resume::MutexLock { mutex, result };
        self.block_on(Wait::Shim { addr: mutex.to_usize(), bitset: u32::MAX, deadline: None, resume });
        Ok(())
    }

    /// Unlocks `mutex` for the running thread: 0, or the error code of a mutex it does not
    /// hold that reports one.
    fn mutex_unlock(&mut self, mutex: Address) -> Result<'db, i32> {
        let mut state = self.mutex_get(mutex)?;
        let me = self.thread() + 1;
        if state.owner != me {
            if state.kind == MutexKind::Normal {
                // alloc: the error that ends the run.
                return Err(MirEvalError::UndefinedBehavior(format!(
                    "pthread_mutex_unlock of a mutex at {:#x} the thread does not hold",
                    mutex.to_usize()
                )));
            }
            return Ok(self.target()?.errno.perm);
        }
        state.count -= 1;
        if state.count == 0 {
            state.owner = 0;
            self.mutex_set(mutex, state)?;
            self.wake(mutex.to_usize(), u32::MAX, 1)?;
        } else {
            self.mutex_set(mutex, state)?;
        }
        Ok(0)
    }

    fn mutex_layout(&self, os: Os) -> MutexLayout {
        match os {
            Os::Linux if self.ptr_size() == 8 => MutexLayout { kind: 16, owner: 8, count: 4 },
            Os::Linux => MutexLayout { kind: 12, owner: 8, count: 4 },
            Os::Apple => {
                let word = self.ptr_size();
                MutexLayout { kind: word, owner: word + 4, count: word + 8 }
            }
        }
    }

    /// The state of `mutex`; an Apple mutex of a static initializer is initialized first.
    fn mutex_get(&mut self, mutex: Address) -> Result<'db, MutexState> {
        let os = self.target()?.os;
        let layout = self.mutex_layout(os);
        if os == Os::Apple {
            let kind = match self.peek_word(mutex)? {
                APPLE_MUTEX_SIG => None,
                APPLE_MUTEX_SIG_INIT | APPLE_FIRSTFIT_MUTEX_SIG_INIT => Some(MutexKind::Normal),
                APPLE_ERRORCHECK_MUTEX_SIG_INIT => Some(MutexKind::ErrorCheck),
                APPLE_RECURSIVE_MUTEX_SIG_INIT => Some(MutexKind::Recursive),
                _ => {
                    // alloc: the error that ends the run.
                    return Err(MirEvalError::UndefinedBehavior(format!(
                        "a pthread_mutex_t at {:#x} that is not initialized",
                        mutex.to_usize()
                    )));
                }
            };
            if let Some(kind) = kind {
                self.poke_word(mutex, APPLE_MUTEX_SIG)?;
                self.mutex_set(mutex, MutexState { kind, owner: 0, count: 0 })?;
            }
        }
        let raw = self.peek_u32(mutex.offset(layout.kind))?;
        let Some(kind) = MutexKind::of(raw) else {
            not_supported!("a pthread_mutex_t of kind {raw}")
        };
        let owner = self.peek_u32(mutex.offset(layout.owner))?;
        let count = self.peek_u32(mutex.offset(layout.count))?;
        Ok(MutexState { kind, owner, count })
    }

    fn mutex_set(&mut self, mutex: Address, state: MutexState) -> Result<'db, ()> {
        let os = self.target()?.os;
        let layout = self.mutex_layout(os);
        self.poke_u32(mutex.offset(layout.kind), state.kind as u32)?;
        self.poke_u32(mutex.offset(layout.owner), state.owner)?;
        self.poke_u32(mutex.offset(layout.count), state.count)
    }

    /// Where the kind is in a `pthread_mutexattr_t`, which must be initialized.
    fn mutexattr_kind_at(&mut self, attr: Address) -> Result<'db, usize> {
        match self.target()?.os {
            Os::Linux => Ok(0),
            Os::Apple if self.peek_word(attr)? == APPLE_MUTEXATTR_SIG => Ok(self.ptr_size()),
            Os::Apple => Err(MirEvalError::UndefinedBehavior(
                // alloc: the error that ends the run.
                "a pthread_mutexattr_t that pthread_mutexattr_init did not initialize".to_owned(),
            )),
        }
    }

    fn cond_layout(&self, os: Os) -> CondLayout {
        let word = self.ptr_size();
        match os {
            Os::Linux => CondLayout { clock: 0, waiters: 4, mutex: 8 },
            Os::Apple => CondLayout { clock: word, waiters: word + 4, mutex: word + 8 },
        }
    }

    /// Fails unless `cond` is initialized; an Apple one of the static initializer is
    /// initialized first.
    fn cond_ready(&mut self, cond: Address) -> Result<'db, ()> {
        if self.target()?.os != Os::Apple {
            return Ok(());
        }
        match self.peek_word(cond)? {
            APPLE_COND_SIG => Ok(()),
            APPLE_COND_SIG_INIT => {
                let layout = self.cond_layout(Os::Apple);
                self.poke_word(cond, APPLE_COND_SIG)?;
                self.poke_u32(cond.offset(layout.clock), 0)?;
                self.poke_u32(cond.offset(layout.waiters), 0)?;
                self.poke_word(cond.offset(layout.mutex), 0)
            }
            // alloc: the error that ends the run.
            _ => Err(MirEvalError::UndefinedBehavior(format!(
                "a pthread_cond_t at {:#x} that is not initialized",
                cond.to_usize()
            ))),
        }
    }

    /// `pthread_cond_wait` and its timed forms: unlocks `mutex`, which the thread must hold
    /// once, and waits on `cond` until it is signalled or `deadline` passes, then relocks it.
    fn cond_wait(
        &mut self,
        cond: Address,
        mutex: Address,
        deadline: Option<u64>,
        destination: Interval,
    ) -> Result<'db, ()> {
        self.cond_ready(cond)?;
        let target = self.target()?;
        let layout = self.cond_layout(target.os);
        let state = self.mutex_get(mutex)?;
        if state.owner != self.thread() + 1 {
            if state.kind == MutexKind::Normal {
                return Err(MirEvalError::UndefinedBehavior(
                    // alloc: the error that ends the run.
                    "pthread_cond_wait with a mutex the thread does not hold".to_owned(),
                ));
            }
            return self.ret_int(destination, target.errno.perm.into());
        }
        if state.count != 1 {
            not_supported!("pthread_cond_wait with a recursive mutex locked more than once")
        }
        let bound = self.peek_word(cond.offset(layout.mutex))?;
        if bound != 0 && bound != mutex.to_usize() {
            return Err(MirEvalError::UndefinedBehavior(
                // alloc: the error that ends the run.
                "pthread_cond_wait with two mutexes for one condition variable at once".to_owned(),
            ));
        }
        if deadline.is_some_and(|it| it <= self.now()) {
            return self.ret_int(destination, target.errno.timedout.into());
        }
        self.poke_word(cond.offset(layout.mutex), mutex.to_usize())?;
        let waiters = self.peek_u32(cond.offset(layout.waiters))?;
        self.poke_u32(cond.offset(layout.waiters), waiters + 1)?;
        self.mutex_unlock(mutex)?;
        let resume = Resume::CondWait { cond, mutex };
        self.block_on(Wait::Shim { addr: cond.to_usize(), bitset: u32::MAX, deadline, resume });
        Ok(())
    }

    /// The semaphore `arg` points to, which `dispatch_semaphore_create` made.
    fn semaphore_of(&self, arg: &IntervalAndTy<'db>) -> Result<'db, Address> {
        let semaphore = self.ptr_of(arg)?;
        let made = match semaphore.at() {
            At::Heap(pos) => self.heap_blocks.get(&pos).is_some_and(|it| {
                it.origin == Origin::Machine && it.size == 16
            }),
            _ => false,
        };
        if !made || from_bytes!(u64, self.peek_memory(semaphore.offset(8), 8)?) != SEMAPHORE_MAGIC {
            not_supported!("a dispatch object that is not a semaphore dispatch_semaphore_create made")
        }
        Ok(semaphore)
    }

    /// Takes one of `semaphore`'s count, or blocks until it can or `deadline` passes.
    fn semaphore_wait(
        &mut self,
        semaphore: Address,
        deadline: Option<u64>,
        destination: Interval,
    ) -> Result<'db, ()> {
        let count = from_bytes!(i64, self.peek_memory(semaphore, 8)?);
        if count > 0 {
            self.poke_memory(semaphore, 8)?.copy_from_slice(&(count - 1).to_le_bytes());
            self.machine.acquire(semaphore.to_usize() as u64)?;
            return self.ret_int(destination, 0);
        }
        if deadline.is_some_and(|it| it <= self.now()) {
            return self.ret_int(destination, SEMAPHORE_TIMED_OUT);
        }
        let resume = Resume::Semaphore { semaphore, deadline };
        let addr = semaphore.to_usize();
        self.block_on(Wait::Shim { addr, bitset: u32::MAX, deadline, resume });
        Ok(())
    }

    /// `nanosleep` and `clock_nanosleep`: the virtual clock moves on by `ns`, and other
    /// threads get to run.
    fn sleep_for(&mut self, ns: u64) {
        self.advance_clock(ns);
        self.yield_thread();
    }

    fn errno_cell(&mut self) -> Result<'db, Address> {
        if let Some(cell) = self.tls().errno {
            return Ok(cell);
        }
        let cell = self.allocate(4, 4, Origin::Machine, true)?;
        self.tls().errno = Some(cell);
        Ok(cell)
    }

    /// Sets the running thread's `errno`, as a shim's failing call does.
    pub fn set_errno(&mut self, code: i32) -> Result<'db, ()> {
        let cell = self.errno_cell()?;
        self.write_memory(cell, &code.to_le_bytes())
    }

    /// An error `code` as the call reports one.
    fn fail(&mut self, destination: Interval, errors: Errors, code: i32) -> Result<'db, ()> {
        match errors {
            Errors::Errno => {
                self.set_errno(code)?;
                self.ret_int(destination, -1)
            }
            Errors::Negated => self.ret_int(destination, (-code).into()),
        }
    }

    /// The thread a `pthread_t` argument names, if it names one.
    fn pthread_arg(&self, arg: &IntervalAndTy<'db>) -> Result<'db, Option<EvalThread>> {
        let raw = self.uint_of(arg)?;
        Ok(raw.checked_sub(1).and_then(|it| EvalThread::try_from(it).ok()))
    }

    /// The attributes `arg` points to, which must be initialized.
    fn attr_of(&self, arg: &IntervalAndTy<'db>) -> Result<'db, Address> {
        let attr = self.ptr_of(arg)?;
        self.attr_ready(attr)?;
        Ok(attr)
    }

    fn attr_ready(&self, attr: Address) -> Result<'db, ()> {
        if self.peek_u32(attr)? != ATTR_MAGIC {
            return Err(MirEvalError::UndefinedBehavior(
                // alloc: the error that ends the run.
                "a pthread_attr_t that pthread_attr_init did not initialize".to_owned(),
            ));
        }
        Ok(())
    }

    /// Field `i` of the attributes: 1 the stack's address, 2 its size, 3 the guard's size, all
    /// words, and 4 the detach state, a `u32`. 0 is [`ATTR_MAGIC`].
    fn attr_field(&self, attr: Address, i: usize) -> Address {
        attr.offset(i * self.ptr_size())
    }

    fn attr_write(
        &mut self,
        attr: Address,
        stack: usize,
        size: usize,
        guard: usize,
        detach: u32,
    ) -> Result<'db, ()> {
        self.poke_u32(attr, ATTR_MAGIC)?;
        self.poke_word(self.attr_field(attr, 1), stack)?;
        self.poke_word(self.attr_field(attr, 2), size)?;
        self.poke_word(self.attr_field(attr, 3), guard)?;
        self.poke_u32(self.attr_field(attr, 4), detach)
    }

    /// The length of the C string at `at`, without its NUL.
    fn c_str_len(&self, at: Address) -> Result<'db, usize> {
        let mut len = 0;
        while self.peek_memory(at.offset(len), 1)?[0] != 0 {
            len += 1;
        }
        Ok(len)
    }

    /// The nanoseconds of the `struct timespec` at `at` (two words, as `time_t` and `long`
    /// are on the targets here); `None` when it is not a valid one.
    fn timespec(&self, at: Address) -> Result<'db, Option<u64>> {
        let word = self.ptr_size();
        let bytes = self.read_memory(at, 2 * word)?;
        let (secs, nanos) = bytes.split_at(word);
        let secs = i128::from_le_bytes(pad16(secs, IsSigned::Yes));
        let nanos = i128::from_le_bytes(pad16(nanos, IsSigned::Yes));
        if secs < 0 || !(0..i128::from(NANOS)).contains(&nanos) {
            return Ok(None);
        }
        let ns = secs.saturating_mul(i128::from(NANOS)).saturating_add(nanos);
        Ok(Some(ns.min(i128::from(u64::MAX)) as u64))
    }

    fn write_timespec(&mut self, at: Address, ns: u64) -> Result<'db, ()> {
        let word = self.ptr_size();
        self.write_memory(at, &word_bytes((ns / NANOS) as usize))?;
        self.write_memory(at.offset(word), &word_bytes((ns % NANOS) as usize))
    }

    fn pointee_size(
        &self,
        arg: &IntervalAndTy<'db>,
        locals: &Locals<'db>,
        what: &'static str,
    ) -> Result<'db, usize> {
        let Some((ty, ..)) = arg.ty.as_reference_or_ptr() else {
            // alloc: the error that ends the run.
            return Err(MirEvalError::InternalError(format!("{what} is not a pointer").into()));
        };
        self.size_of_sized(ty, locals, what)
    }

    fn int_of(&self, arg: &IntervalAndTy<'db>) -> Result<'db, i128> {
        Ok(i128::from_le_bytes(pad16(arg.get(self)?, IsSigned::Yes)))
    }

    fn uint_of(&self, arg: &IntervalAndTy<'db>) -> Result<'db, u128> {
        Ok(u128::from_le_bytes(pad16(arg.get(self)?, IsSigned::No)))
    }

    /// A pointer argument, with the provenance it carries.
    fn ptr_of(&self, arg: &IntervalAndTy<'db>) -> Result<'db, Address> {
        self.read_pointer(arg.interval.addr)
    }

    /// `value` as the call's result, an integer of the destination's size.
    fn ret_int(&mut self, destination: Interval, value: i128) -> Result<'db, ()> {
        destination.write_from_bytes(self, &value.to_le_bytes()[..destination.size])
    }

    fn write_int(&mut self, at: Address, size: usize, value: i128) -> Result<'db, ()> {
        self.write_memory(at, &value.to_le_bytes()[..size])
    }

    fn peek_u32(&self, at: Address) -> Result<'db, u32> {
        Ok(from_bytes!(u32, self.peek_memory(at, 4)?))
    }

    fn poke_u32(&mut self, at: Address, value: u32) -> Result<'db, ()> {
        self.poke_memory(at, 4)?.copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    fn peek_word(&self, at: Address) -> Result<'db, usize> {
        Ok(from_bytes!(usize, self.peek_memory(at, self.ptr_size())?))
    }

    fn poke_word(&mut self, at: Address, value: usize) -> Result<'db, ()> {
        let word = self.ptr_size();
        self.poke_memory(at, word)?.copy_from_slice(&word_bytes(value));
        Ok(())
    }
}

/// A wait on a futex word: `size` bytes at `addr` holding `expected`, woken by a wake whose
/// bitset meets `bitset`, until `deadline` on the virtual clock.
#[derive(Debug, Clone, Copy)]
struct FutexWait {
    addr: Address,
    size: usize,
    expected: u64,
    bitset: u32,
    deadline: Option<u64>,
}
