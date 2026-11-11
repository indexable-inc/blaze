//! Linux, by raw system call: no libc. aarch64 and x86-64.
//!
//! The raw call ([`raw`]) is private; everything public is typed per operation. Each wrapper
//! maps the kernel's result the same way: a value in `-4095..=-1` is the [`crate::Errno`] its
//! number names ([`crate::Errno::from_raw`]), anything else is the call's value. No wrapper
//! retries, loops or waits twice: an interruption ([`crate::Errno::Intr`]) and a short read
//! or write go back to the caller, whose retry loop is the one bounded one
//! (`blaze_core::wait::until`).

pub mod nr;
mod raw;

mod fs;
mod mem;
mod net;
mod process;
mod uring;
mod wait;

pub use fs::{
    AtFlags, CWD, CloneRange, Entries, Entry, EntryKind, EventFlags, OpenFlags, OpenHow, OwnedFd,
    PipeFlags, RandomFlags, RenameFlags, Resolve, Statx, StatxMask, StatxTime, SyncRange, Unlink,
    close, eventfd2, fdatasync, ficlone, ficlonerange, fsync, ftruncate, getdents64, getrandom,
    mkdirat, open_beneath, openat, openat2, pipe2, pread, pwrite, read, renameat2, statx,
    sync_file_range, unlinkat, write,
};
pub use mem::{Advice, MapFlags, Prot, madvise, mmap, mprotect, munmap};
pub use net::{
    Family, IoSlice, IoSliceMut, MsgFlags, Shutdown, SockAddr, SockFlags, SockKind, UnixAddr,
    V4Addr, V6Addr, accept4, bind, connect, listen, recvmsg, sendmsg, shutdown, socket,
};
pub use process::{
    Children, CloneArgs, CloneFlags, Cloned, Disposition, Ended, MaskHow, Prctl, Rusage,
    SeccompFlags, SockFilter, Waited, WaitOptions, clone3_process, exit_group, exit_thread,
    getpid, kill, pidfd_open, prctl, rt_sigaction, rt_sigprocmask, seccomp_filter, wait4_now,
    waitid,
};
pub use uring::{
    CqOffsets, ETIME, EnterFlags, EnterWait, Register, SetupFlags, SqOffsets, UringParams,
    io_uring_enter, io_uring_register, io_uring_setup,
};
pub use wait::{
    ClockId, FUTEX_ANY, PollEvents, PollFd, SigSet, clock_gettime, clock_nanosleep_until,
    futex_wait_bitset, futex_wake, ppoll,
};

#[cfg(test)]
mod tests;
