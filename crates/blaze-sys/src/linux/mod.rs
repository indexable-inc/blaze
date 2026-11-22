//! Linux, by raw system call: no libc. aarch64 and x86-64.
//!
//! The raw call ([`raw`]) is private; everything public is typed per operation. Each wrapper
//! maps the kernel's result the same way: a value in `-4095..=-1` is the [`crate::Errno`] its
//! number names ([`crate::Errno::from_raw`]), anything else is the call's value. No wrapper
//! retries, loops or waits twice: an interruption ([`crate::Errno::Intr`]) and a short read
//! or write go back to the caller, whose retry loop is the one bounded one
//! (`blaze_core::wait::until`).
//!
//! replaces libc::syscall: by the typed call per number (grep "replaces libc::SYS_<name>"; the
//! table of every number and its wrapper is linux/nr.rs's module doc)

pub mod nr;
mod raw;

/// The auxiliary vector, typed: the table the kernel hands a process at its start.
pub mod auxv;
/// The vDSO's `clock_gettime`, found from `AT_SYSINFO_EHDR`: kernel code, not the C library.
pub mod vdso;

/// D182: block devices' facts, the flagged positional write (`RWF_DSYNC`, `RWF_ATOMIC`).
mod block;
pub use block::*;

/// D177: a confined child's step, made in the parent, entered between fork and exec.
pub mod confine;
mod counters;
/// Descriptor control (`fcntl` as typed operations, `dup`, `ioctl`) and the memory and file
/// calls `posix::io` needs: `preadv`, `fallocate`, `memfd_create`, `mincore`, `msync`.
pub mod fdctl;
mod fs;
/// The process's identity, group, session, limits and affinity, and the host's name.
pub mod host;
mod landlock;
mod mem;
mod net;
mod notify;
mod process;
/// D185: what this kernel offers, probed once (`KernelFacts`), and the probe and slot tables.
pub mod probe;
/// D185, rule 78: each operation with several forms, its form chosen once from `KernelFacts`.
pub mod slot;
/// A child on a lent stack sharing this process's memory until its exec (`CLONE_VM |
/// CLONE_VFORK`), with its pidfd: either form of the `StartProcess` slot.
pub mod vfork;
pub use vfork::{ChildEntry, Launched, MIN_STACK, StartFailed, start_on_stack};
/// The calls of bstd's sampler (D178): a thread CPU-time timer and its signal's handler.
pub mod sample;
/// Signal actions with blaze-sys's own restorer, the alternate stack, `tgkill` and `raise`.
pub mod signal;
/// The process and file calls `posix::proc` needs: `wait4`, `fork` by `clone`, `execve`,
/// `newfstatat`, `mknodat`, the xattrs, `fadvise64`, `mount`, priorities, `sysinfo`.
pub mod task;
/// The socket calls `posix::net` needs beyond `net`'s: whole message headers, batches,
/// option bytes.
pub mod sock;
mod uring;
mod wait;

pub use fs::{
    AtFlags, CWD, CloneRange, Entries, Entry, EntryKind, EventFlags, OpenFlags, OpenHow, OwnedFd,
    PipeFlags, RandomFlags, RenameFlags, Resolve, Statx, StatxMask, StatxTime, SyncRange, Unlink,
    Whence, close, copy_file_range, eventfd2, fdatasync, ficlone, ficlonerange, fsync, ftruncate, getdents64, getrandom,
    is_terminal, lseek, window_size, mkdirat, open_beneath, openat, openat2, pipe2, pread, pwrite, read, renameat2, statx,
    sync_file_range, unlinkat, write,
};
pub use fs::{FileTime, LockOp, Statfs, fchmod, flock, fstatfs, linkat, readlinkat, symlinkat, utimensat};
pub use fs::{FileHandle, MAX_HANDLE_SZ, name_to_handle_at, open_by_handle_at};
pub use fs::write_from_address;
pub use landlock::{
    AccessFs, AccessNet, NetPort, PathBeneath, RulesetAttr, landlock_abi, landlock_add_rule_path,
    landlock_add_rule_port, landlock_create_ruleset, landlock_restrict_self,
};
pub use net::{SockOpt, setsockopt};
pub use notify::{
    FAN_METADATA_VERSION, FanEvent, FanEventMetadata, FanEvents, FanInit, FanMark, FanMask,
    INOTIFY_EVENT_HEADER, InotifyEvent, InotifyEvents, InotifyFlags, InotifyMask, Watch,
    fanotify_init, fanotify_mark, inotify_add_watch, inotify_init1, inotify_rm_watch,
};
pub use process::{Arg, Args, execveat, kill_group, sched_yield};
pub use process::pidfd_send_signal;
pub use counters::{PerfCounters, Who, getrusage, perf_counters_open, perf_counters_read};
pub use counters::{PerfAttrFlags, PerfEventAttr, PerfOpenFlags, perf_event_open};
pub use mem::{Advice, MapFlags, Prot, madvise, mmap, mprotect, munmap};
pub use mem::{Membarrier, membarrier};
pub use mem::{MemPolicy, PIDFD_SELF_THREAD_GROUP, mbind, process_madvise};
pub use net::{
    Family, IoSlice, IoSliceMut, MsgFlags, Shutdown, SockAddr, SockFlags, SockKind, UnixAddr,
    V4Addr, V6Addr, accept4, bind, connect, listen, recvmsg, sendmsg, shutdown, socket,
};
pub use process::{
    Children, CloneArgs, CloneFlags, Cloned, Disposition, Ended, MaskHow, Prctl, Rusage,
    SeccompFlags, SockFilter, Waited, WaitOptions, clone3_process, exit_group, exit_thread,
    getpid, kill, pidfd_open, prctl, rt_sigaction, rt_sigprocmask, seccomp_filter, thread_name,
    wait4_now, waitid,
};
pub use uring::{
    CqOffsets, ETIME, EnterFlags, EnterWait, Register, SetupFlags, SqOffsets, UringParams,
    io_uring_enter, io_uring_register, io_uring_setup,
};
pub use uring::{BufReg, FileIndexRange, RSRC_REGISTER_SPARSE, RsrcRegister, RsrcUpdate2};
pub use uring::{
    MEM_REGION_REG_WAIT_ARG, MEM_REGION_TYPE_USER, MemRegionReg, REG_WAIT_TS, RegWait, RegionDesc,
    io_uring_enter_reg_wait,
};
pub use wait::{
    ClockId, FUTEX_ANY, PollEvents, PollFd, SigSet, clock_gettime, clock_gettime_vdso,
    clock_nanosleep_until, futex_wait_bitset, futex_wake, ppoll,
};
pub use wait::{futex_wait_bitset_shared, futex_wait_shared, futex_wake_shared};

#[cfg(test)]
mod tests;
