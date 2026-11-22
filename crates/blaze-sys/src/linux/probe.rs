//! What this kernel offers, probed once at start (D185): [`KernelFacts`].
//!
//! No minimum kernel version, and on each kernel the best interface it has. Each fact is the
//! answer of one cheap call made for the purpose, never a parse of `uname` (distributions
//! backport): the ring's own probe of its operations, a system call made with arguments that
//! cannot have an effect (the kernel answers `ENOSYS` for a number it lacks, and a known
//! refusal for one it has), the Landlock version query. Every probe has three answers
//! ([`Presence`]): present, absent (`ENOSYS`, or the refusal a kernel without the interface
//! gives), and denied (`EPERM` or `EACCES`: the call is there or not, but a seccomp profile or
//! a sysctl forbids it here). A slot treats denied as absent, and says so. A probe answering
//! anything else is a [`ProbeError`] naming it, never a guess. The value is made once, handed
//! down like the clock (rule 75), passed to a child by [`KernelFacts::to_bytes`] and
//! [`KernelFacts::from_parent`], and printed in the profile by its `Display`. Each slot
//! (`linux::slot`) chooses its form from it once, before any work: a choice from a stated
//! fact, not a fallback (rule 3).
//!
//! [`KernelFacts::probe`] is called by the top-level process BEFORE it makes its second
//! thread: the ring futex probe (below) observes a kernel's behaviour only in a process that
//! has no private futex hash yet, and the kernel gives a process one when it makes its second
//! thread. A child does not probe: it takes its parent's facts from its start message.
//!
//! # The probes
//!
//! | fact | invocation | present | denied | why it is harmless |
//! |---|---|---|---|---|
//! | ring | `io_uring_setup(1, {0})`, closed at once | a descriptor | `EPERM`/`EACCES` (`kernel.io_uring_disabled`, or a filter) | a ring of one entry, no kernel thread, closed before the probe returns |
//! | ring ops | `io_uring_register(ring, IORING_REGISTER_PROBE, zeroed probe, 256)` | the op table (`EINVAL` before 5.6: no probe) | (the ring's) | it writes only the caller's buffer |
//! | ring wait flags | `io_uring_setup(1, SUBMIT_ALL \| R_DISABLED \| SINGLE_ISSUER \| DEFER_TASKRUN)` | a descriptor (`EINVAL`: a flag unknown) | `EPERM`/`EACCES` | as the first ring; disabled, so not even enterable |
//! | ring mem region | on that disabled ring, `io_uring_register(ring, IORING_REGISTER_MEM_REGION, {one 64 KiB user region, WAIT_ARG}, 1)`, then the ring closed and the region unmapped | 0 (`EINVAL`/`EOPNOTSUPP`: no such register opcode, before 6.13) | `EPERM`/`EACCES` | the region is the probe's own anonymous mapping; the kernel pins it until the ring closes; nothing is ever waited on |
//! | futex hash | `prctl(PR_FUTEX_HASH, PR_FUTEX_HASH_GET_SLOTS, 0, 0, 0)` | a slot count (0: none yet) (`EINVAL` before 6.16) | `EPERM`/`EACCES` | a read |
//! | ring futex | a ring with the wait ring's flags, one `IORING_OP_FUTEX_WAIT` on a word of the probe's own, then (where the process has no private futex hash) `prctl(PR_FUTEX_HASH, SET_SLOTS, 16, 0)`, then `futex(FUTEX_WAKE \| PRIVATE, 1)` | the wake's count: 1 `sound`, 0 `lost-wake` (`absent` without ring ops 51/52 or the wait flags) | `EPERM`/`EACCES` from the ring or the `prctl` | the request is reaped or cancelled under a 100 ms bound (an expiry is a [`ProbeError`], never a hang); no thread is made; it leaves the process's private futex hash installed, which is the wait slot's guard itself |
//! | futex_waitv | `futex_waitv(NULL, 0, 0, NULL, 0)` | `EINVAL` (`flags`, then `nr_futexes == 0`) | `EPERM`/`EACCES` | both checks precede any read |
//! | close_range | `close_range(~0, ~0 - 1, 0)` | `EINVAL` (`fd > max_fd`) | `EPERM`/`EACCES` | the range is empty: refused before any descriptor is looked at |
//! | clone3 | `clone3(NULL, 0)` | `EINVAL` (`size < CLONE_ARGS_SIZE_VER0`) | `EPERM`/`EACCES` | the size is checked before the arguments are read; no child can result |
//! | openat2 | `openat2(-1, NULL, NULL, 0)` | `EINVAL` (`usize < OPEN_HOW_SIZE_VER0`) | `EPERM`/`EACCES` | checked before the path or `how` is read |
//! | statx | `statx(-1, "", 0, STATX__RESERVED, NULL)` | `EINVAL` (the reserved mask bit), or `ENOENT`/`EBADF`/`EFAULT` | `EPERM`/`EACCES` | the empty name without `AT_EMPTY_PATH` names nothing; no buffer is written without a found file |
//! | pidfd_open | `pidfd_open(0, ~0)` | `EINVAL` (unknown flags, then `pid <= 0`) | `EPERM`/`EACCES` | no descriptor can be made |
//! | pidfd_send_signal | `pidfd_send_signal(-1, 0, NULL, 0)` | `EBADF` | `EPERM`/`EACCES` | signal 0 through no descriptor |
//! | copy_file_range | `copy_file_range(-1, NULL, -1, NULL, 0, 0)` | `EBADF` | `EPERM`/`EACCES` | no file, no length |
//! | memfd_secret | `memfd_secret(~0)` | `EINVAL` (unknown flags) | `EPERM`/`EACCES` | no descriptor can be made; `ENOSYS` also where the kernel is built or booted without it (`secretmem.enable`), which is the same fact: it cannot be used |
//! | cachestat | `cachestat(~0, NULL, NULL, 0)` | `EBADF` | `EPERM`/`EACCES` | no file |
//! | mseal | `mseal(0, 0, ~0)` | `EINVAL` (flags first), or 0 | `EPERM`/`EACCES` | nothing sealed: flags are refused, and the range is empty |
//! | get_auxv | `prctl(PR_GET_AUXV, NULL, 0, 0, 0)` | a size above 0 (a kernel without it: `EINVAL`, an unknown option) | `EPERM`/`EACCES` | a length of 0 copies nothing; the call answers the saved vector's size |
//! | landlock | `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` ([`super::landlock_abi`]) | the ABI (`EOPNOTSUPP`: built, off at boot) | `EPERM`/`EACCES` | the version query makes no ruleset |
//!
//! The atomic write (D182) is a fact of a file, not of the kernel: the caller probes its own
//! store's file with `linux::atomic_write_bounds` (`statx(STATX_WRITE_ATOMIC)`) and hands the
//! answer to `slot::AtomicWrite::choose`. It is not a field here.
//!
//! ## The ring futex fact
//!
//! From 6.16 (80367ad01d93) a process gets a private futex hash of its own when it makes its
//! second thread (or by `prctl(PR_FUTEX_HASH, PR_FUTEX_HASH_SET_SLOTS, n)`). A ring
//! `FUTEX_WAIT` queued while the process had none sits in the global hash; once a private one
//! is installed, a `FUTEX_WAKE` hashes into it and misses that waiter: a lost wake. Fixed on
//! 2026-10-15 ("io_uring/futex: allocate private hash before waiting"); present from 6.16 up to
//! the fix, and on any stable kernel without its backport. Before 6.16 there is only the global
//! hash: no bug. Because `FUTEX_WAKE` returns how many it woke, the probe sees it directly:
//! `sound` (1 woken), `lost-wake` (0). Where the process already had a private hash the
//! transition cannot be seen and the probe answers `sound`, which is true of that process.
//!
//! The guard (rule 54) makes the class impossible whatever the fact: the wait slot's `Ring`
//! form is started only through `slot::install_private_hash`, which installs the process's
//! private hash before its first ring wait wherever `PR_FUTEX_HASH` exists, so no ring waiter
//! is ever queued in the global hash, on a fixed kernel or not, in a child too. So a
//! `lost-wake` kernel takes the ring as well; the fact is still probed and printed so a person
//! sees which class of kernel they are on.
//!
//! A speed note, not a fact: before the two 2026-07-30 commits merged for 7.3 ("don't mark
//! futex wake requests as inflight", "only mark private futex waits as inflight", Cc: stable)
//! a scalar ring wait/wake pair is ~10% slower (~180 ns -> ~197 ns measured). Every wait and
//! wake answers correctly there; no probe and no floor follows from it.
//!
//! Sources, unverified: the dates, titles and hashes above were summarised by a fetch tool from
//! two pages and never read verbatim: the history of io_uring/futex.c at
//! <https://git.zx2c4.com/linux-rng/log/io_uring/futex.c?showmsg=1>, and the timings from
//! the regression thread at <https://lkml.iu.edu/2607.3/13492.html>. Check them against the
//! kernel tree before relying on a date, a hash or a version here. The probe and the guard
//! do not depend on them: the probe observes the behaviour, and the guard removes the cause.
//!
//! # The table
//!
//! Each slot, its forms, the kernel each form came in and the probe that detects it. A slot's
//! floor is its oldest form's kernel; the newer forms are what a newer kernel would give.
//!
//! | slot | form | kernel | probe |
//! |---|---|---|---|
//! | wait (a thread's blocking waits: a word, a child's exit, a descriptor, a sleep) | the thread's wait ring (blaze-core `wait/ring.rs`): `FUTEX_WAIT`/`FUTEX_WAKE` and `WAITID` (ops 51, 52, 50: 6.7), `POLL_ADD` (6), `TIMEOUT` (11), `ASYNC_CANCEL` (14) on a `SUBMIT_ALL \| SINGLE_ISSUER \| DEFER_TASKRUN` ring (6.1) with a registered wait region (`REGISTER_MEM_REGION`, `ENTER_EXT_ARG_REG`: 6.13; `ENTER_ABS_TIMER`: 6.12), started behind the private futex hash guard; taken on `sound` and on `lost-wake` (the guard makes it safe) | 6.13 | ring, ring ops, ring wait flags, ring mem region, ring futex |
//! | | direct calls (blaze-core `wait/direct.rs`): `futex(FUTEX_WAIT_BITSET \| PRIVATE)`, `FUTEX_WAKE`, `waitid`, `pidfd_open` then `ppoll`, `clock_nanosleep` | 5.3 | pidfd_open |
//! | wait on several words | `futex_waitv` | 5.16 | futex_waitv |
//! | | one `futex(FUTEX_WAIT_BITSET)` on a wake word | 2.6.25 | (always) |
//! | close a range | `close_range` | 5.9 | close_range |
//! | | a `getdents64` walk of `/proc/self/fd`, closing each | 2.6 (needs procfs mounted and readable) | (always) |
//! | start a process with its pidfd | `clone3(CLONE_PIDFD)` | 5.3 | clone3 |
//! | | `clone(CLONE_PIDFD)` | 5.2 (chosen on the pidfd_open fact, 5.3) | pidfd_open |
//! | file facts | `statx` | 4.11 | statx |
//! | | `newfstatat` | 2.6.16 | (always) |
//! | open beneath a root | `openat2(RESOLVE_BENEATH \| RESOLVE_NO_SYMLINKS)` | 5.6 | openat2 |
//! | | `openat(O_PATH \| O_NOFOLLOW)` per component, each checked a directory | 2.6.39 (`O_PATH`) | (always) |
//! | signal a process | `pidfd_send_signal` | 5.1 | pidfd_send_signal |
//! | | `kill` by pid (only before the child is reaped) | 1.0 | (always) |
//! | copy | `copy_file_range` | 4.5 (across file systems 5.3) | copy_file_range |
//! | | `pread` and `pwrite` through the caller's buffer | 2.1 | (always) |
//! | wait on descriptors | `epoll_pwait2` / `epoll_pwait` | 5.11 / 2.6.19 | not used: the ring (blaze has no epoll) |
//! | secret memory | `memfd_secret` | 5.14 | memfd_secret |
//! | | a refusal naming the fact | | |
//! | cache residency | `cachestat` | 6.5 | cachestat |
//! | | `mmap` and `mincore` | 2.4 | (always) |
//! | seal a mapping | `mseal` | 6.10 | mseal |
//! | | a refusal naming the fact | | |
//! | atomic write | `pwritev2(RWF_ATOMIC)` | 6.11 (`STATX_WRITE_ATOMIC`: 6.11 block devices, 6.13 ext4 and xfs files) | per file: `atomic_write_bounds` |
//! | | a refusal naming the fact | | |
//! | the auxiliary vector | `prctl(PR_GET_AUXV)` (`facts::Aux::for_top_level`) | 6.4 | get_auxv |
//! | | `/proc/self/auxv` | 2.6 | (always) |
//! | confinement | Landlock ABI 1 to 7 (5.13, 5.19, 6.2, 6.7, 6.10, 6.12, 6.15) | 5.13 | landlock |
//! | | a refusal (rule 9: where no sandbox can be made, blaze stops) | | |
//! | ring operations (facts, not slots) | linked timeouts (op 15) 5.5; `FILES_UPDATE` (op 20) 5.6; `URING_CMD` (op 46) 6.0; `SEND_ZC` (op 47) 6.0; `WAITID`, `FUTEX_WAIT`, `FUTEX_WAKE` (ops 50 to 52) 6.7; `FIXED_FD_INSTALL` (op 54) 6.8 | | ring ops |
//!
//! A denied fact is an absent one to every slot; a slot left with no form is a refusal that
//! names the fact (and the wait slot's, whether it was denied).
//!
//! The slots' floor, the largest oldest form: 5.3, set by starting a process and by the wait
//! slot's direct form (both need `pidfd_open`).
//!
//! What blaze still requires unconditionally (no slot yet), and its kernel:
//!
//! | use | where | kernel |
//! |---|---|---|
//! | `pipe2`, `eventfd2`, `dup3`, `inotify_init1` | linux/fs.rs, fdctl.rs, notify.rs | 2.6.27 |
//! | `FUTEX_WAIT_BITSET` | linux/wait.rs | 2.6.25 |
//! | `accept4`, `preadv`, `recvmmsg`, `prlimit64`, `sendmmsg` | net.rs, fdctl.rs, sock.rs, host.rs | 2.6.28 to 3.0 |
//! | `fanotify_init` | notify.rs | 2.6.37; blaze-core's watch needs `FAN_REPORT_DFID_NAME`, 5.9 |
//! | `renameat2`, `getrandom`, `memfd_create`, `seccomp`, `execveat` | fs.rs, fdctl.rs, process.rs | 3.15 to 3.19 |
//! | `pwritev2`, `RWF_DSYNC` | block.rs | 4.6, 4.7 |
//! | `statx` (`Effect::Stat`, the stamps) | fs.rs, blaze-core uring.rs | 4.11 |
//! | `pidfd_send_signal` | blaze-core uring.rs, blaze_inproc_rt | 5.1 |
//! | `io_uring_setup` | uring.rs | 5.1 |
//! | `pidfd_open`, `clone3_process` | process.rs, blaze-core wait/direct.rs | 5.3 |
//! | `openat2` (`open_beneath`) | fs.rs, blaze-core uring.rs | 5.6 |
//! | `close_range` | blaze-launch-rt zygote.rs | 5.9 |
//! | Landlock | landlock.rs, the sandbox | 5.13 (more by the rights a grant needs: network 6.7) |
//! | the driver's ring: `SQPOLL` unprivileged and `FEAT_EXT_ARG` (5.11), `SUBMIT_ALL` (5.18), `REGISTER_PBUF_RING` (5.19), `SEND_ZC` and `REGISTER_FILE_ALLOC_RANGE` (6.0) | blaze-core uring.rs | 6.0 |
//!
//! **The honest floor is Linux 6.0**, set by blaze-core's driver ring (`uring.rs`: it
//! registers `IORING_REGISTER_FILE_ALLOC_RANGE`, opcode 25, at setup and sends with `SEND_ZC`,
//! both 6.0); without it 5.9 (fanotify's names), then 5.3 (the slots). The thread's wait ring
//! no longer sets it: it is the wait slot's better form (6.13), chosen from the facts, with the
//! direct calls (5.3) below it. That holds once blaze-core's `wait/choice.rs` takes
//! `slot::Wait::choose` in place of the `__BLAZE_SANDBOXED` mark; until then a process outside
//! the sandbox takes the ring unconditionally and its floor there is 6.13.
//!
//! The next conversions, each a slot with its older form already in the tree:
//! 1. the driver's ring (6.0) against the `threads` driver (blaze-core `threads/`).
//! 2. fanotify with names (5.9) against inotify (2.6.13).

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};
use core::time::Duration;

use super::{
    EnterFlags, EnterWait, MapFlags, OwnedFd, Prot, SetupFlags, UringParams, close, fs::arg, futex_wake,
    io_uring_enter, io_uring_setup, landlock_abi, mmap, munmap, nr, raw,
};
use crate::{Errno, Fd, Timespec};

/// A fact [`KernelFacts`] holds, each made by one probe: what a [`ProbeError`] and a slot's
/// refusal (`slot::Refused`) name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fact {
    /// io_uring itself.
    Ring,
    /// The ring's table of operations (`IORING_REGISTER_PROBE`).
    RingOps,
    /// The setup flags of blaze's thread wait ring.
    RingWaitFlags,
    /// A registered wait region (`IORING_REGISTER_MEM_REGION` with `WAIT_ARG`).
    RingMemRegion,
    /// Whether a ring futex wait is woken by a plain `FUTEX_WAKE` across the process's first
    /// private futex hash ([`RingFutex`]).
    RingFutex,
    /// `futex_waitv`.
    FutexWaitv,
    /// `close_range`.
    CloseRange,
    /// `clone3`.
    Clone3,
    /// `openat2`.
    Openat2,
    /// `statx`.
    Statx,
    /// `pidfd_open`.
    PidfdOpen,
    /// `pidfd_send_signal`.
    PidfdSendSignal,
    /// `copy_file_range`.
    CopyFileRange,
    /// `memfd_secret`.
    MemfdSecret,
    /// `cachestat`.
    Cachestat,
    /// `mseal`.
    Mseal,
    /// `prctl(PR_GET_AUXV)`.
    GetAuxv,
    /// `prctl(PR_FUTEX_HASH)`: a process's private futex hash.
    FutexHash,
    /// Landlock's ABI.
    Landlock,
    /// A file's atomic write unit (`statx(STATX_WRITE_ATOMIC)`, probed per file by the caller).
    AtomicWrite,
}

impl Fact {
    /// The fact's name, as the profile line prints it.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn name(self) -> &'static str {
        match self {
            Fact::Ring => "ring",
            Fact::RingOps => "ring ops",
            Fact::RingWaitFlags => "ring wait flags",
            Fact::RingMemRegion => "ring mem region",
            Fact::RingFutex => "ring futex",
            Fact::FutexWaitv => "futex_waitv",
            Fact::CloseRange => "close_range",
            Fact::Clone3 => "clone3",
            Fact::Openat2 => "openat2",
            Fact::Statx => "statx",
            Fact::PidfdOpen => "pidfd_open",
            Fact::PidfdSendSignal => "pidfd_send_signal",
            Fact::CopyFileRange => "copy_file_range",
            Fact::MemfdSecret => "memfd_secret",
            Fact::Cachestat => "cachestat",
            Fact::Mseal => "mseal",
            Fact::GetAuxv => "get_auxv",
            Fact::FutexHash => "futex hash",
            Fact::Landlock => "landlock",
            Fact::AtomicWrite => "atomic write",
        }
    }

    /// The call its probe makes.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn probe(self) -> &'static str {
        match self {
            Fact::Ring => "io_uring_setup(1, {0})",
            Fact::RingOps => "io_uring_register(ring, IORING_REGISTER_PROBE, probe, 256)",
            Fact::RingWaitFlags => "io_uring_setup(1, SUBMIT_ALL | R_DISABLED | SINGLE_ISSUER | DEFER_TASKRUN)",
            Fact::RingMemRegion => {
                "io_uring_register(disabled ring, IORING_REGISTER_MEM_REGION, {64 KiB user region, WAIT_ARG}, 1)"
            }
            Fact::RingFutex => {
                "a ring FUTEX_WAIT, prctl(PR_FUTEX_HASH, PR_FUTEX_HASH_SET_SLOTS, 16, 0), futex(FUTEX_WAKE | PRIVATE, 1)"
            }
            Fact::FutexWaitv => "futex_waitv(NULL, 0, 0, NULL, 0)",
            Fact::CloseRange => "close_range(~0, ~0 - 1, 0)",
            Fact::Clone3 => "clone3(NULL, 0)",
            Fact::Openat2 => "openat2(-1, NULL, NULL, 0)",
            Fact::Statx => "statx(-1, \"\", 0, STATX__RESERVED, NULL)",
            Fact::PidfdOpen => "pidfd_open(0, ~0)",
            Fact::PidfdSendSignal => "pidfd_send_signal(-1, 0, NULL, 0)",
            Fact::CopyFileRange => "copy_file_range(-1, NULL, -1, NULL, 0, 0)",
            Fact::MemfdSecret => "memfd_secret(~0)",
            Fact::Cachestat => "cachestat(~0, NULL, NULL, 0)",
            Fact::Mseal => "mseal(0, 0, ~0)",
            Fact::GetAuxv => "prctl(PR_GET_AUXV, NULL, 0, 0, 0)",
            Fact::FutexHash => "prctl(PR_FUTEX_HASH, PR_FUTEX_HASH_GET_SLOTS, 0, 0, 0)",
            Fact::Landlock => "landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)",
            Fact::AtomicWrite => "statx(file, \"\", AT_EMPTY_PATH, STATX_WRITE_ATOMIC)",
        }
    }

    /// The kernel the fact's interface came in.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn since(self) -> &'static str {
        match self {
            Fact::Ring => "5.1",
            Fact::RingOps => "5.6",
            Fact::RingWaitFlags => "6.1",
            Fact::RingMemRegion => "6.13",
            Fact::RingFutex => "6.7",
            Fact::FutexWaitv => "5.16",
            Fact::CloseRange => "5.9",
            Fact::Clone3 => "5.3",
            Fact::Openat2 => "5.6",
            Fact::Statx => "4.11",
            Fact::PidfdOpen => "5.3",
            Fact::PidfdSendSignal => "5.1",
            Fact::CopyFileRange => "4.5",
            Fact::MemfdSecret => "5.14",
            Fact::Cachestat => "6.5",
            Fact::Mseal => "6.10",
            Fact::GetAuxv => "6.4",
            Fact::FutexHash => "6.16",
            Fact::Landlock => "5.13",
            Fact::AtomicWrite => "6.11",
        }
    }
}

/// What one probe found: the interface is there, is not, or is denied here (`EPERM` or
/// `EACCES`: a seccomp profile, a sysctl). A slot treats [`Presence::Denied`] as absent: it
/// cannot use what it may not call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Presence {
    /// `ENOSYS`, or the refusal a kernel without the interface gives.
    Absent,
    /// There, and allowed.
    Present,
    /// Denied here: whether the kernel has it is not known, and does not matter.
    Denied,
}

impl Presence {
    /// `yes`, `no` or `denied`, as the profile line prints it.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn name(self) -> &'static str {
        match self {
            Presence::Absent => "no",
            Presence::Present => "yes",
            Presence::Denied => "denied",
        }
    }

    /// Whether it can be used.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn present(self) -> bool {
        matches!(self, Presence::Present)
    }
}

/// What a probe answered that is neither "present", "absent" nor "denied".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    /// An error number.
    Errno(Errno),
    /// A value where a refusal was expected.
    Value(usize),
}

/// A probe answered something it should not: the fact and the answer. Never guessed past.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProbeError {
    /// The fact whose probe answered.
    pub fact: Fact,
    /// What it answered.
    pub answer: Answer,
}

impl core::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the kernel's facts: the probe of {} ({}) answered ", self.fact.name(), self.fact.probe())?;
        match self.answer {
            Answer::Errno(errno) => write!(f, "{errno}")?,
            Answer::Value(value) => write!(f, "the value {value}")?,
        }
        f.write_str(", neither its present answer, ENOSYS, nor a denial (EPERM, EACCES)")
    }
}

impl core::error::Error for ProbeError {}

/// Why a parent's [`KernelFacts::to_bytes`] could not be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FromParentError {
    /// The message is not [`ENCODED`] bytes long.
    Length {
        /// The length given.
        found: usize,
    },
    /// The message has another version than this build's.
    Version {
        /// The version byte given.
        found: u8,
    },
    /// A field holds a value no probe makes: its name.
    Invalid {
        /// The field.
        field: &'static str,
    },
}

impl core::fmt::Display for FromParentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FromParentError::Length { found } => {
                write!(f, "the kernel's facts: the parent's message is {found} bytes, not {ENCODED}")
            }
            FromParentError::Version { found } => {
                write!(f, "the kernel's facts: the parent's message is version {found}, not {VERSION}")
            }
            FromParentError::Invalid { field } => write!(f, "the kernel's facts: {field} holds no probe's answer"),
        }
    }
}

impl core::error::Error for FromParentError {}

/// Whether io_uring can be used here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RingState {
    /// `ENOSYS`: no io_uring, or a filter answering so (blaze's sandbox).
    Absent,
    /// `EPERM` or `EACCES`: denied by `kernel.io_uring_disabled`, or a filter answering so.
    Denied(Errno),
    /// A ring was made.
    Present,
}

/// Whether a ring futex wait queued before the process's first private futex hash is woken
/// by a plain `FUTEX_WAKE` after it (the module doc's "ring futex fact").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RingFutex {
    /// No ring, no `FUTEX_WAIT`/`FUTEX_WAKE` ops (51, 52), or not the wait ring's flags.
    Absent,
    /// The ring, or the `prctl` the probe needs, is denied here.
    Denied,
    /// Woken: before 6.16 (one global hash), a fixed kernel, or a process that had a private
    /// hash already.
    Sound,
    /// Not woken: 6.16 up to the 2026-10-15 fix (unverified: see the module doc's sources).
    /// Safe behind `slot::install_private_hash`.
    LostWake,
}

impl RingFutex {
    /// `sound`, `lost-wake`, `absent` or `denied`, as the profile line prints it.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn name(self) -> &'static str {
        match self {
            RingFutex::Absent => "absent",
            RingFutex::Denied => "denied",
            RingFutex::Sound => "sound",
            RingFutex::LostWake => "lost-wake",
        }
    }
}

/// The ring's facts: whether one can be made, the features its setup reported, and the
/// operations its probe lists.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RingFacts {
    state: RingState,
    /// `io_uring_params.features` (`IORING_FEAT_*`).
    features: u32,
    /// Whether `IORING_REGISTER_PROBE` answered (5.6).
    probed: bool,
    /// How many operations this kernel numbers (`ops_len`).
    known: u16,
    /// Bit `n` set: operation `n` is supported.
    ops: [u8; 32],
    /// Whether blaze's thread wait ring's setup flags are all known.
    wait_ring_flags: Presence,
    /// Whether a registered wait region can be made.
    mem_region: Presence,
}

/// `IORING_OP_POLL_ADD`.
const OP_POLL_ADD: u8 = 6;
/// `IORING_OP_TIMEOUT`.
const OP_TIMEOUT: u8 = 11;
/// `IORING_OP_ASYNC_CANCEL`.
const OP_ASYNC_CANCEL: u8 = 14;
/// `IORING_OP_LINK_TIMEOUT`.
const OP_LINK_TIMEOUT: u8 = 15;
/// `IORING_OP_FILES_UPDATE`.
const OP_FILES_UPDATE: u8 = 20;
/// `IORING_OP_URING_CMD`.
const OP_URING_CMD: u8 = 46;
/// `IORING_OP_SEND_ZC`.
const OP_SEND_ZC: u8 = 47;
/// `IORING_OP_WAITID`.
const OP_WAITID: u8 = 50;
/// `IORING_OP_FUTEX_WAIT`.
const OP_FUTEX_WAIT: u8 = 51;
/// `IORING_OP_FUTEX_WAKE`.
const OP_FUTEX_WAKE: u8 = 52;
/// `IORING_OP_FIXED_FD_INSTALL`.
const OP_FIXED_FD_INSTALL: u8 = 54;

impl RingFacts {
    const fn without(state: RingState) -> RingFacts {
        RingFacts {
            state,
            features: 0,
            probed: false,
            known: 0,
            ops: [0; 32],
            wait_ring_flags: Presence::Absent,
            mem_region: Presence::Absent,
        }
    }

    /// Whether a ring can be made, and if not, why.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn state(&self) -> RingState {
        self.state
    }

    /// Whether a ring can be made.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn present(&self) -> bool {
        matches!(self.state, RingState::Present)
    }

    /// The features the setup reported (`IORING_FEAT_*`); 0 without a ring.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn features(&self) -> u32 {
        self.features
    }

    /// Whether the kernel answered the probe of its operations (5.6). Without it every
    /// operation reads as not supported.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn probed(&self) -> bool {
        self.probed
    }

    /// How many operations this kernel numbers.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn known_ops(&self) -> u16 {
        self.known
    }

    /// How many of them it supports (some are compiled out, the network ones without
    /// `CONFIG_NET`).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn supported_ops(&self) -> u16 {
        self.ops.iter().fold(0u16, |count, byte| count.wrapping_add(byte.count_ones() as u16))
    }

    /// Whether operation `op` (`IORING_OP_*`) is supported.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn op(&self, op: u8) -> bool {
        self.ops.get(usize::from(op >> 3)).is_some_and(|byte| byte & 1u8.rotate_left(u32::from(op & 7)) != 0)
    }

    /// A poll of one descriptor as a request (`IORING_OP_POLL_ADD`, 5.1).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn poll_add(&self) -> bool {
        self.op(OP_POLL_ADD)
    }

    /// A timeout as a request (`IORING_OP_TIMEOUT`, 5.4).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn timeout(&self) -> bool {
        self.op(OP_TIMEOUT)
    }

    /// A cancel of a request by its `user_data` (`IORING_OP_ASYNC_CANCEL`, 5.5).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn async_cancel(&self) -> bool {
        self.op(OP_ASYNC_CANCEL)
    }

    /// Linked timeouts (`IORING_OP_LINK_TIMEOUT`, 5.5).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn link_timeout(&self) -> bool {
        self.op(OP_LINK_TIMEOUT)
    }

    /// Updates of the registered file table by request (`IORING_OP_FILES_UPDATE`, 5.6). Opens
    /// straight into the table (5.15) have no operation of their own: the probe cannot say.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn files_update(&self) -> bool {
        self.op(OP_FILES_UPDATE)
    }

    /// A direct descriptor installed as a normal one (`IORING_OP_FIXED_FD_INSTALL`, 6.8).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn fixed_fd_install(&self) -> bool {
        self.op(OP_FIXED_FD_INSTALL)
    }

    /// The driver passthrough command, NVMe's among them (`IORING_OP_URING_CMD`, 6.0).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn uring_cmd(&self) -> bool {
        self.op(OP_URING_CMD)
    }

    /// Zero-copy send (`IORING_OP_SEND_ZC`, 6.0).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn send_zc(&self) -> bool {
        self.op(OP_SEND_ZC)
    }

    /// A child's exit as a request (`IORING_OP_WAITID`, 6.7).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn waitid(&self) -> bool {
        self.op(OP_WAITID)
    }

    /// Futex waits and wakes as requests (`IORING_OP_FUTEX_WAIT` and `_WAKE`, 6.7).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn futex(&self) -> bool {
        self.op(OP_FUTEX_WAIT) && self.op(OP_FUTEX_WAKE)
    }

    /// Whether `SUBMIT_ALL | R_DISABLED | SINGLE_ISSUER | DEFER_TASKRUN` (blaze's thread wait
    /// ring, 6.1) are all known to the kernel and allowed.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn wait_ring_flags(&self) -> bool {
        self.wait_ring_flags.present()
    }

    /// Whether a registered wait region (`IORING_REGISTER_MEM_REGION` with
    /// `IORING_MEM_REGION_REG_WAIT_ARG`, 6.13; with it `IORING_ENTER_EXT_ARG_REG`, 6.13, and
    /// `IORING_ENTER_ABS_TIMER`, 6.12) can be made and is allowed.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn mem_region(&self) -> bool {
        self.mem_region.present()
    }
}

/// Landlock on this kernel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Landlock {
    /// `ENOSYS`: not built (or a filter answering so).
    Absent,
    /// `EOPNOTSUPP`: built, not enabled at boot.
    Disabled,
    /// `EPERM` or `EACCES`: the query is denied here.
    Denied,
    /// Enabled, at this ABI (1 or more).
    Abi(u32),
}

/// The system calls' facts, one bit each.
const FUTEX_WAITV: u16 = 1 << 0;
const CLOSE_RANGE: u16 = 1 << 1;
const CLONE3: u16 = 1 << 2;
const OPENAT2: u16 = 1 << 3;
const STATX: u16 = 1 << 4;
const PIDFD_OPEN: u16 = 1 << 5;
const PIDFD_SEND_SIGNAL: u16 = 1 << 6;
const COPY_FILE_RANGE: u16 = 1 << 7;
const MEMFD_SECRET: u16 = 1 << 8;
const CACHESTAT: u16 = 1 << 9;
const MSEAL: u16 = 1 << 10;
const GET_AUXV: u16 = 1 << 11;
const FUTEX_HASH: u16 = 1 << 12;
/// Every bit a probe sets.
const CALLS_ALL: u16 = (1 << 13) - 1;

/// The call facts in bit order: bit `n` of the calls is `CALL_FACTS[n]`.
pub(super) const CALL_FACTS: [Fact; 13] = [
    Fact::FutexWaitv,
    Fact::CloseRange,
    Fact::Clone3,
    Fact::Openat2,
    Fact::Statx,
    Fact::PidfdOpen,
    Fact::PidfdSendSignal,
    Fact::CopyFileRange,
    Fact::MemfdSecret,
    Fact::Cachestat,
    Fact::Mseal,
    Fact::GetAuxv,
    Fact::FutexHash,
];

/// What this kernel offers, probed once at start ([`KernelFacts::probe`]) or told by the
/// parent ([`KernelFacts::from_parent`]). Read through getters only.
/// replaces the `uname` parse of blaze-core's `cgroup::kernel()` in the setup errors, as the source of what the kernel has
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KernelFacts {
    ring: RingFacts,
    /// Bit set: the call is present.
    calls: u16,
    /// Bit set: the call is denied here. Never set with the same bit of `calls`.
    denied: u16,
    ring_futex: RingFutex,
    landlock: Landlock,
}

/// What sets blaze's kernel floor today, as the module doc's table says: the last words of the
/// profile's `kernel:` line ([`KernelFacts`]'s `Display`), so a person on an older kernel sees
/// which piece refuses it. Changed with the table when a conversion lands.
pub const FLOOR: &str = "floor 6.0, set by blaze-core's driver ring (uring.rs: REGISTER_FILE_ALLOC_RANGE, SEND_ZC); then 5.9 (fanotify names), 5.3 (the slots)";

/// The encoding's version: the first byte of [`KernelFacts::to_bytes`]'s output. 2: denied
/// facts, the wait region, the ring futex and the futex hash.
const VERSION: u8 = 2;

/// The bytes [`KernelFacts::to_bytes`] writes, little-endian: the version (1); the ring's
/// state (1: 0 absent, 1 present, 2 denied) and its denial's errno (2); its features (4); its
/// flags (1: bit 0 probed, bit 1 the wait ring's flags present, bit 2 they are denied, bit 3
/// the wait region present, bit 4 it is denied); the operations it numbers (2); its operation
/// bitmap (32); the calls present (2); the calls denied (2); the ring futex (1: 0 absent, 1
/// denied, 2 sound, 3 lost-wake); Landlock's kind (1: 0 absent, 1 disabled, 2 an ABI, 3
/// denied) and ABI (4).
pub const ENCODED: usize = 53;

/// `IORING_REGISTER_PROBE`.
const REGISTER_PROBE: usize = 8;
/// `IO_URING_OP_SUPPORTED`.
const OP_SUPPORTED: u16 = 1;
/// The probe's room: every operation a `u8` can number.
const PROBE_OPS: usize = 256;
/// `IORING_SETUP_R_DISABLED` (5.10), absent from [`SetupFlags`].
const SETUP_R_DISABLED: u32 = 1 << 6;
/// `IORING_REGISTER_MEM_REGION` (6.13).
const REGISTER_MEM_REGION: usize = 34;
// The region's layouts and flags have one home, linux/uring.rs (rule 30).
use super::uring::{MEM_REGION_REG_WAIT_ARG, MEM_REGION_TYPE_USER, MemRegionReg, RegionDesc};
/// The probe's region: a multiple of every page size Linux uses (4, 16 and 64 KiB), so the
/// kernel's page checks pass on each; `mmap` gives it page-aligned.
const REGION_BYTES: usize = 64 * 1024;
/// `STATX__RESERVED`: a mask bit no kernel accepts.
const STATX_RESERVED: usize = 0x8000_0000;
/// `PR_GET_AUXV` (linux/prctl.h).
const PR_GET_AUXV: usize = 0x4155_5856;
/// `PR_FUTEX_HASH` (linux/prctl.h, 6.16).
pub(super) const PR_FUTEX_HASH: usize = 78;
/// `PR_FUTEX_HASH_SET_SLOTS`: `prctl(PR_FUTEX_HASH, SET_SLOTS, slots, flags)`.
pub(super) const PR_FUTEX_HASH_SET_SLOTS: usize = 1;
/// `PR_FUTEX_HASH_GET_SLOTS`: the private hash's slots, 0 for none.
pub(super) const PR_FUTEX_HASH_GET_SLOTS: usize = 2;
/// The slots of the private hash blaze installs: the kernel's own default for a process of
/// one thread (`roundup_pow_of_two(4 * threads)`, at least 16).
pub(super) const PRIVATE_HASH_SLOTS: usize = 16;
/// `~0` as an `unsigned int` argument.
const UINT_ALL: usize = 0xFFFF_FFFF;

/// `struct io_uring_probe_op`.
#[derive(Clone, Copy)]
#[repr(C)]
struct ProbeOp {
    op: u8,
    resv: u8,
    flags: u16,
    resv2: u32,
}

/// `struct io_uring_probe` with room for every operation.
#[repr(C)]
struct UringProbe {
    last_op: u8,
    ops_len: u8,
    resv: u16,
    resv2: [u32; 3],
    ops: [ProbeOp; PROBE_OPS],
}

// linux/io_uring.h: `io_uring_probe_op` is 8 bytes; `io_uring_probe` a 16-byte head before
// its flexible `ops`, on every architecture.
const _: () = {
    assert!(size_of::<ProbeOp>() == 8);
    assert!(align_of::<ProbeOp>() == 4);
    assert!(core::mem::offset_of!(ProbeOp, flags) == 2);
    assert!(size_of::<UringProbe>() == 16 + 8 * PROBE_OPS);
    assert!(core::mem::offset_of!(UringProbe, ops_len) == 1);
    assert!(core::mem::offset_of!(UringProbe, ops) == 16);
};

impl UringProbe {
    /// All zero: the kernel refuses a probe buffer that is not.
    const ZERO: UringProbe = UringProbe {
        last_op: 0,
        ops_len: 0,
        resv: 0,
        resv2: [0; 3],
        ops: [ProbeOp { op: 0, resv: 0, flags: 0, resv2: 0 }; PROBE_OPS],
    };
}

/// `struct io_uring_sqe`, as the probe ring and the tests fill it. The driver's and the wait
/// ring's rings keep their own (uring.rs's doc: the rings stay the driver's).
#[allow(dead_code, reason = "a layout of linux/io_uring.h, read by the kernel")]
#[expect(blaze::unwired, reason = "wired: a kernel ABI layout whose fields the kernel reads, not Rust")]
#[derive(Clone, Copy)]
#[repr(C)]
pub(super) struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    op_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    file_index: u32,
    addr3: u64,
    pad: u64,
}

/// `struct io_uring_cqe` (without the 32-byte form's tail, which the probe ring does not ask
/// for).
#[allow(dead_code, reason = "a layout of linux/io_uring.h, written by the kernel")]
#[expect(blaze::unwired, reason = "wired: a kernel ABI layout whose fields the kernel writes, not Rust")]
#[derive(Clone, Copy)]
#[repr(C)]
struct Cqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

// linux/io_uring.h, every architecture: the SQE is 64 bytes (`off` at 8, `addr` at 16, `len`
// at 24, the op flags at 28, `user_data` at 32, `addr3` at 48); the CQE 16.
const _: () = {
    assert!(size_of::<Sqe>() == 64);
    assert!(align_of::<Sqe>() == 8);
    assert!(core::mem::offset_of!(Sqe, fd) == 4);
    assert!(core::mem::offset_of!(Sqe, off) == 8);
    assert!(core::mem::offset_of!(Sqe, addr) == 16);
    assert!(core::mem::offset_of!(Sqe, len) == 24);
    assert!(core::mem::offset_of!(Sqe, op_flags) == 28);
    assert!(core::mem::offset_of!(Sqe, user_data) == 32);
    assert!(core::mem::offset_of!(Sqe, file_index) == 44);
    assert!(core::mem::offset_of!(Sqe, addr3) == 48);
    assert!(size_of::<Cqe>() == 16);
    assert!(core::mem::offset_of!(Cqe, res) == 8);
};

/// `FUTEX2_SIZE_U32 | FUTEX2_PRIVATE`.
const FUTEX2_U32_PRIVATE: i32 = 0x02 | 0x80;
/// `FUTEX_BITSET_MATCH_ANY`.
const FUTEX_BITSET_ANY: u64 = 0xffff_ffff;

impl Sqe {
    /// All zero.
    const ZERO: Sqe = Sqe {
        opcode: 0,
        flags: 0,
        ioprio: 0,
        fd: 0,
        off: 0,
        addr: 0,
        len: 0,
        op_flags: 0,
        user_data: 0,
        buf_index: 0,
        personality: 0,
        file_index: 0,
        addr3: 0,
        pad: 0,
    };

    /// `IORING_OP_FUTEX_WAIT` on `word` (this process's, 32 bits) while it holds `expected`,
    /// woken by a wake of any bit.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(super) fn futex_wait(word: &AtomicU32, expected: u32, user_data: u64) -> Sqe {
        Sqe {
            opcode: OP_FUTEX_WAIT,
            fd: FUTEX2_U32_PRIVATE,
            addr: word.as_ptr() as u64,
            off: u64::from(expected),
            addr3: FUTEX_BITSET_ANY,
            user_data,
            ..Sqe::ZERO
        }
    }

    /// `IORING_OP_ASYNC_CANCEL` of the request whose `user_data` is `target`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(super) fn cancel(target: u64, user_data: u64) -> Sqe {
        Sqe { opcode: OP_ASYNC_CANCEL, addr: target, user_data, ..Sqe::ZERO }
    }
}

/// One completion taken from a [`ProbeRing`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Completion {
    /// The request's `user_data`.
    pub(super) user_data: u64,
    /// Its result: a value, or a negated errno.
    pub(super) res: i32,
}

impl Completion {
    /// No completion: the room's filler.
    pub(super) const NONE: Completion = Completion { user_data: 0, res: 0 };

    /// The errno a negative result names.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(super) fn errno(self) -> Errno {
        Errno::from_raw(u16::try_from(self.res.unsigned_abs()).unwrap_or(u16::MAX))
    }
}

/// A mapping, unmapped when dropped.
struct Mapping {
    at: NonNull<u8>,
    len: usize,
}

impl Mapping {
    /// `len` bytes read-write: of `file` at `offset` (shared, populated), or anonymous and
    /// private for `None`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn new(len: usize, file: Option<Fd>, offset: u64) -> Result<Mapping, Errno> {
        let flags = match file {
            Some(_) => MapFlags::SHARED.with(MapFlags::POPULATE),
            None => MapFlags::PRIVATE.with(MapFlags::ANONYMOUS).with(MapFlags::POPULATE),
        };
        // SAFETY: a new mapping the kernel places (no hint, no `MAP_FIXED`), replacing
        // nothing; a ring's shared words are read and written only through atomics below.
        let at = unsafe { mmap(None, len, Prot::READ.with(Prot::WRITE), flags, file, offset) }?;
        Ok(Mapping { at, len })
    }

    /// The `u32` at `offset` as an atomic: `None` past the end or misaligned.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn word(&self, offset: u32) -> Option<&AtomicU32> {
        let offset = offset as usize;
        if offset % align_of::<AtomicU32>() != 0 || offset.checked_add(size_of::<u32>())? > self.len {
            return None;
        }
        // SAFETY: in the mapping (checked above), aligned (the mapping is page-aligned and the
        // offset a multiple of 4), and live while `self` is; the kernel's side of a ring word
        // is atomic, as `AtomicU32` is.
        Some(unsafe { AtomicU32::from_ptr(self.at.as_ptr().wrapping_add(offset).cast::<u32>()) })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping is this value's, made in `new`, and nothing points into it past
        // its drop.
        _ = unsafe { munmap(self.at, self.len) };
    }
}

/// A small ring of the probe's (and the tests'): set up with the wait ring's flags, enabled,
/// its queues mapped. One thread, its issuer (`SINGLE_ISSUER`), uses it.
pub(super) struct ProbeRing {
    // Closed first (fields drop in order): the kernel ends what is in flight, then the
    // mappings go.
    fd: OwnedFd,
    rings: Mapping,
    sqes: Mapping,
    sq_entries: u32,
    sq_mask: u32,
    cq_mask: u32,
    sq_head: u32,
    sq_tail: u32,
    cq_head: u32,
    cq_tail: u32,
    cqes: u32,
    /// The submission tail this side has written.
    tail: u32,
}

/// `IORING_OFF_SQ_RING`.
const OFF_SQ_RING: u64 = 0;
/// `IORING_OFF_SQES`.
const OFF_SQES: u64 = 0x1000_0000;

/// The wait ring's setup flags (blaze-core `wait/ring.rs`), enabled at once.
#[blaze::noalloc]
#[blaze::nopanic]
fn wait_ring_setup() -> SetupFlags {
    SetupFlags::SUBMIT_ALL.with(SetupFlags::SINGLE_ISSUER).with(SetupFlags::DEFER_TASKRUN)
}

impl ProbeRing {
    /// A ring of `entries` with the wait ring's flags, enabled, its queues mapped (one mapping
    /// for both rings: `IORING_FEAT_SINGLE_MMAP`, 5.4, which every kernel with these flags has).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(super) fn new(entries: u32) -> Result<ProbeRing, Errno> {
        let mut params = UringParams { flags: wait_ring_setup().bits(), ..UringParams::default() };
        let fd = io_uring_setup(entries, &mut params)?;
        let (sq, cq) = (params.sq_off, params.cq_off);
        let sq_len = (sq.array as usize).saturating_add((params.sq_entries as usize).saturating_mul(size_of::<u32>()));
        let cq_len = (cq.cqes as usize).saturating_add((params.cq_entries as usize).saturating_mul(size_of::<Cqe>()));
        let rings = Mapping::new(sq_len.max(cq_len), Some(fd.fd()), OFF_SQ_RING)?;
        let sqes = Mapping::new((params.sq_entries as usize).saturating_mul(size_of::<Sqe>()), Some(fd.fd()), OFF_SQES)?;
        let mut index = 0u32;
        while index < params.sq_entries {
            // The kernel's array of submission indices: slot `n` names entry `n`, once.
            let at = sq.array.wrapping_add(index.wrapping_mul(size_of::<u32>() as u32));
            rings.word(at).ok_or(Errno::Fault)?.store(index, Ordering::Relaxed);
            index = index.wrapping_add(1);
        }
        let sq_mask = rings.word(sq.ring_mask).ok_or(Errno::Fault)?.load(Ordering::Relaxed);
        let cq_mask = rings.word(cq.ring_mask).ok_or(Errno::Fault)?.load(Ordering::Relaxed);
        Ok(ProbeRing {
            fd,
            rings,
            sqes,
            sq_entries: params.sq_entries,
            sq_mask,
            cq_mask,
            sq_head: sq.head,
            sq_tail: sq.tail,
            cq_head: cq.head,
            cq_tail: cq.tail,
            cqes: cq.cqes,
            tail: 0,
        })
    }

    /// Writes `sqe` for the next enter to submit: false where the ring is full.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(super) fn push(&mut self, sqe: Sqe) -> bool {
        let (Some(head), Some(tail)) = (self.rings.word(self.sq_head), self.rings.word(self.sq_tail)) else {
            return false;
        };
        if self.tail.wrapping_sub(head.load(Ordering::Acquire)) >= self.sq_entries {
            return false;
        }
        let at = ((self.tail & self.sq_mask) as usize).saturating_mul(size_of::<Sqe>());
        if at.saturating_add(size_of::<Sqe>()) > self.sqes.len {
            return false;
        }
        // SAFETY: in the entries' mapping (checked above), 64-byte aligned (page-aligned
        // mapping, a multiple of 64 in), and past the kernel's head: not the kernel's until the
        // tail below publishes it.
        unsafe { self.sqes.at.as_ptr().wrapping_add(at).cast::<Sqe>().write(sqe) };
        self.tail = self.tail.wrapping_add(1);
        tail.store(self.tail, Ordering::Release);
        true
    }

    /// One `io_uring_enter`: hands over `to_submit` and, with `min` above 0, waits for `min`
    /// completions for up to `most`. As [`io_uring_enter`]: [`super::ETIME`] where the time ran
    /// out, [`Errno::Intr`].
    ///
    /// # Safety
    ///
    /// Every request pushed names memory that lives until its completion is taken or the ring
    /// is closed.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(super) unsafe fn enter(&self, to_submit: u32, min: u32, most: Duration) -> Result<u32, Errno> {
        let wait = EnterWait { left: Some(Timespec::from_duration(most)), mask: None };
        let (flags, wait) = if min > 0 { (EnterFlags::GET_EVENTS, Some(&wait)) } else { (EnterFlags::NONE, None) };
        // SAFETY: the caller answers for the requests' memory; the wait argument is a live
        // local.
        unsafe { io_uring_enter(self.fd.fd(), to_submit, min, flags, wait) }
    }

    /// Takes every completion posted into `room` (one beyond its room is taken and dropped):
    /// how many were written.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(super) fn completions(&self, room: &mut [Completion]) -> usize {
        let (Some(head_word), Some(tail_word)) = (self.rings.word(self.cq_head), self.rings.word(self.cq_tail)) else {
            return 0;
        };
        let mut head = head_word.load(Ordering::Relaxed);
        let tail = tail_word.load(Ordering::Acquire);
        let mut got = 0usize;
        while head != tail {
            let at = (self.cqes as usize).saturating_add(((head & self.cq_mask) as usize).saturating_mul(size_of::<Cqe>()));
            if at.saturating_add(size_of::<Cqe>()) <= self.rings.len {
                // SAFETY: in the rings' mapping (checked above), 16-byte aligned, and posted
                // (the acquire above); not reused until the head moves past it.
                let cqe = unsafe { self.rings.at.as_ptr().wrapping_add(at).cast::<Cqe>().read() };
                if let Some(slot) = room.get_mut(got) {
                    *slot = Completion { user_data: cqe.user_data, res: cqe.res };
                    got = got.wrapping_add(1);
                }
            }
            head = head.wrapping_add(1);
        }
        head_word.store(head, Ordering::Release);
        got
    }

    /// Closes the ring, then unmaps its queues.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(super) fn close(self) -> Result<(), Errno> {
        let ProbeRing { fd, rings, sqes, .. } = self;
        let closed = close(fd);
        drop(rings);
        drop(sqes);
        closed
    }
}

/// What [`ensure_private_hash`] found or did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum HashStep {
    /// This kernel makes no private futex hash (`PR_FUTEX_HASH` unknown, before 6.16, or
    /// `SET_SLOTS` refused with `EINVAL`: built without private hashes): only the global hash.
    NoHashes,
    /// The process had one already.
    Had,
    /// It had none; one of [`PRIVATE_HASH_SLOTS`] was installed.
    Installed,
}

/// Gives this process a private futex hash where the kernel makes them and it has none: the
/// one home of the guard's call, used by the ring futex probe and `slot::install_private_hash`.
#[blaze::noalloc]
#[blaze::nopanic]
pub(super) fn ensure_private_hash() -> Result<HashStep, Errno> {
    // SAFETY: `PR_FUTEX_HASH_GET_SLOTS` reads no memory of the caller's.
    let slots = unsafe { raw::syscall5(nr::PRCTL, PR_FUTEX_HASH, PR_FUTEX_HASH_GET_SLOTS, 0, 0, 0) };
    match raw::check(slots) {
        Err(Errno::Inval | Errno::NoSys) => return Ok(HashStep::NoHashes),
        Err(errno) => return Err(errno),
        Ok(slots) if slots > 0 => return Ok(HashStep::Had),
        Ok(_) => {}
    }
    // SAFETY: `PR_FUTEX_HASH_SET_SLOTS` takes numbers and reads no memory; it changes only the
    // process's futex hash, which every futex call of the process then uses alike.
    let set = unsafe { raw::syscall5(nr::PRCTL, PR_FUTEX_HASH, PR_FUTEX_HASH_SET_SLOTS, PRIVATE_HASH_SLOTS, 0, 0) };
    match raw::check(set) {
        Ok(_) => Ok(HashStep::Installed),
        Err(Errno::Inval) => Ok(HashStep::NoHashes),
        Err(errno) => Err(errno),
    }
}

/// Whether `errno` is in `list`.
#[blaze::noalloc]
#[blaze::nopanic]
fn listed(list: &[Errno], errno: Errno) -> bool {
    let mut at = 0usize;
    while let Some(&each) = list.get(at) {
        if each == errno {
            return true;
        }
        at = at.wrapping_add(1);
    }
    false
}

/// A system call's probe read: `ENOSYS` absent, `EPERM`/`EACCES` denied, an error of
/// `present` present, anything else the [`ProbeError`] naming `fact`.
#[blaze::noalloc]
#[blaze::nopanic]
fn answered(fact: Fact, result: usize, present: &[Errno]) -> Result<Presence, ProbeError> {
    match raw::check(result) {
        Err(Errno::NoSys) => Ok(Presence::Absent),
        Err(errno) if listed(present, errno) => Ok(Presence::Present),
        Err(Errno::Perm | Errno::Acces) => Ok(Presence::Denied),
        Err(errno) => Err(ProbeError { fact, answer: Answer::Errno(errno) }),
        Ok(value) => Err(ProbeError { fact, answer: Answer::Value(value) }),
    }
}

/// The ring's facts: one throwaway ring of one entry, probed for its operations and closed,
/// then one with the wait ring's flags, disabled, on which the wait region is registered,
/// closed.
#[blaze::noalloc]
#[blaze::nopanic]
fn probe_ring() -> Result<RingFacts, ProbeError> {
    let mut params = UringParams::default();
    let ring = match io_uring_setup(1, &mut params) {
        Ok(ring) => ring,
        Err(Errno::NoSys) => return Ok(RingFacts::without(RingState::Absent)),
        Err(errno @ (Errno::Perm | Errno::Acces)) => return Ok(RingFacts::without(RingState::Denied(errno))),
        Err(errno) => return Err(ProbeError { fact: Fact::Ring, answer: Answer::Errno(errno) }),
    };
    let mut probe = UringProbe::ZERO;
    // SAFETY: `probe` is a live, zeroed `io_uring_probe` with room for `PROBE_OPS` entries,
    // the count passed; the kernel writes at most that many and keeps no pointer.
    let asked = unsafe {
        raw::syscall4(
            nr::IO_URING_REGISTER,
            arg(ring.fd()),
            REGISTER_PROBE,
            core::ptr::from_mut(&mut probe) as usize,
            PROBE_OPS,
        )
    };
    let mut ops = [0u8; 32];
    let probed = match raw::check(asked) {
        Ok(_) => true,
        // A ring without the probe (5.1 to 5.5): an unknown register opcode.
        Err(Errno::Inval) => false,
        Err(errno) => return Err(ProbeError { fact: Fact::RingOps, answer: Answer::Errno(errno) }),
    };
    let known = if probed { u16::from(probe.ops_len) } else { 0 };
    let mut at = 0usize;
    while at < usize::from(known) {
        if let Some(entry) = probe.ops.get(at)
            && entry.flags & OP_SUPPORTED != 0
            && let Some(byte) = ops.get_mut(usize::from(entry.op >> 3))
        {
            *byte |= 1u8.rotate_left(u32::from(entry.op & 7));
        }
        at = at.wrapping_add(1);
    }
    close(ring).map_err(|errno| ProbeError { fact: Fact::Ring, answer: Answer::Errno(errno) })?;
    let (wait_ring_flags, mem_region) = probe_wait_ring()?;
    Ok(RingFacts { state: RingState::Present, features: params.features, probed, known, ops, wait_ring_flags, mem_region })
}

/// The wait ring's flags and its wait region: a throwaway ring with the flags and
/// `R_DISABLED` (`EINVAL`: a flag unknown), on which a region of the probe's own memory is
/// registered as the wait region (6.13; a disabled ring is the one the kernel takes it on),
/// then the ring closed and the region unmapped. Without the flags, the region is absent too:
/// it came later (6.13 against 6.1).
#[blaze::noalloc]
#[blaze::nopanic]
fn probe_wait_ring() -> Result<(Presence, Presence), ProbeError> {
    let region_failed = |errno| ProbeError { fact: Fact::RingMemRegion, answer: Answer::Errno(errno) };
    // Made before the ring, so it is unmapped after the ring closes (locals drop in reverse).
    let region = Mapping::new(REGION_BYTES, None, 0).map_err(region_failed)?;
    let flags = wait_ring_setup().with(SetupFlags::from_bits(SETUP_R_DISABLED));
    let mut params = UringParams { flags: flags.bits(), ..UringParams::default() };
    let ring = match io_uring_setup(1, &mut params) {
        Ok(ring) => ring,
        Err(Errno::Inval) => return Ok((Presence::Absent, Presence::Absent)),
        Err(Errno::Perm | Errno::Acces) => return Ok((Presence::Denied, Presence::Denied)),
        Err(errno) => return Err(ProbeError { fact: Fact::RingWaitFlags, answer: Answer::Errno(errno) }),
    };
    let mut desc = RegionDesc {
        user_addr: region.at.as_ptr() as u64,
        size: REGION_BYTES as u64,
        flags: MEM_REGION_TYPE_USER,
        id: 0,
        mmap_offset: 0,
        resv: [0; 4],
    };
    let reg = MemRegionReg {
        region_uptr: core::ptr::from_mut(&mut desc) as u64,
        flags: MEM_REGION_REG_WAIT_ARG,
        resv: [0; 2],
    };
    // SAFETY: `reg` and the `desc` it names are live locals the kernel reads (and `desc` it
    // writes back); the region is this function's anonymous mapping, page-aligned and a
    // multiple of the page size, which the kernel pins until the ring closes below and which
    // is unmapped only after that. The ring is disabled: nothing can wait on it.
    let asked = unsafe {
        raw::syscall4(nr::IO_URING_REGISTER, arg(ring.fd()), REGISTER_MEM_REGION, core::ptr::from_ref(&reg) as usize, 1)
    };
    let mem_region = match raw::check(asked) {
        Ok(_) => Ok(Presence::Present),
        // An unknown register opcode (before 6.13), or a kernel that refuses the region's kind.
        Err(Errno::Inval | Errno::OpNotSupp) => Ok(Presence::Absent),
        Err(Errno::Perm | Errno::Acces) => Ok(Presence::Denied),
        Err(errno) => Err(region_failed(errno)),
    };
    close(ring).map_err(|errno| ProbeError { fact: Fact::RingWaitFlags, answer: Answer::Errno(errno) })?;
    drop(region);
    Ok((Presence::Present, mem_region?))
}

/// The ring futex fact (the module doc's section): one ring wait on a word of this function's,
/// the process's first private futex hash installed under it, and a plain wake that counts.
/// Bounded: every end of the request is taken within [`RING_FUTEX_BOUND`] or is a
/// [`ProbeError`].
#[blaze::noalloc]
#[blaze::nopanic]
fn probe_ring_futex(ring: &RingFacts) -> Result<RingFutex, ProbeError> {
    match ring.state {
        RingState::Denied(_) => return Ok(RingFutex::Denied),
        RingState::Absent => return Ok(RingFutex::Absent),
        RingState::Present => {}
    }
    if !ring.futex() {
        return Ok(RingFutex::Absent);
    }
    match ring.wait_ring_flags {
        Presence::Denied => return Ok(RingFutex::Denied),
        Presence::Absent => return Ok(RingFutex::Absent),
        Presence::Present => {}
    }
    let failed = |answer| ProbeError { fact: Fact::RingFutex, answer };
    // Declared before the ring: it outlives it on every path (locals drop in reverse).
    let word = AtomicU32::new(0);
    let mut probe = match ProbeRing::new(4) {
        Ok(probe) => probe,
        Err(Errno::Perm | Errno::Acces) => return Ok(RingFutex::Denied),
        Err(errno) => return Err(failed(Answer::Errno(errno))),
    };
    if !probe.push(Sqe::futex_wait(&word, 0, WAITED)) {
        return Err(failed(Answer::Value(0)));
    }
    // SAFETY: the one request names `word`, which outlives the ring.
    match unsafe { probe.enter(1, 0, RING_FUTEX_BOUND) } {
        Ok(1) => {}
        Err(Errno::Perm | Errno::Acces) => return Ok(RingFutex::Denied),
        Ok(taken) => return Err(failed(Answer::Value(taken as usize))),
        Err(errno) => return Err(failed(Answer::Errno(errno))),
    }
    let mut room = [Completion::NONE; 4];
    // The wait must be queued: a completion already posted is a request that ended at once.
    if probe.completions(&mut room) > 0 {
        let [first, ..] = room;
        return Err(failed(Answer::Errno(first.errno())));
    }
    let hashed = match ensure_private_hash() {
        Ok(step) => Some(step),
        Err(Errno::Perm | Errno::Acces) => None,
        Err(errno) => return Err(failed(Answer::Errno(errno))),
    };
    let woken = futex_wake(&word, 1).map_err(|errno| failed(Answer::Errno(errno)))?;
    let fact = match (hashed, woken) {
        (None, _) => RingFutex::Denied,
        (Some(_), 1) => RingFutex::Sound,
        (Some(HashStep::Installed), 0) => RingFutex::LostWake,
        // No hash changed, and still not woken: not a kernel this probe describes.
        (Some(_), other) => return Err(failed(Answer::Value(other as usize))),
    };
    if woken == 1 {
        // SAFETY: as above.
        end_within(unsafe { probe.enter(0, 1, RING_FUTEX_BOUND) }).map_err(failed)?;
        let got = probe.completions(&mut room);
        match room.get(..got).and_then(|taken| taken.iter().find(|it| it.user_data == WAITED)) {
            Some(ended) if ended.res == 0 => {}
            Some(ended) => return Err(failed(Answer::Errno(ended.errno()))),
            None => return Err(failed(Answer::Errno(super::ETIME))),
        }
    } else {
        if !probe.push(Sqe::cancel(WAITED, CANCEL)) {
            return Err(failed(Answer::Value(0)));
        }
        // SAFETY: as above; the cancel names no memory.
        end_within(unsafe { probe.enter(1, 2, RING_FUTEX_BOUND) }).map_err(failed)?;
        let got = probe.completions(&mut room);
        let taken = room.get(..got).unwrap_or(&[]);
        let ended = |user_data: u64| taken.iter().any(|it| it.user_data == user_data);
        if !(ended(WAITED) && ended(CANCEL)) {
            return Err(failed(Answer::Errno(super::ETIME)));
        }
    }
    probe.close().map_err(|errno| failed(Answer::Errno(errno)))?;
    Ok(fact)
}

/// How long the ring futex probe waits for its request's end: past it, a [`ProbeError`].
const RING_FUTEX_BOUND: Duration = Duration::from_millis(100);
/// The `user_data` of the probe's wait.
const WAITED: u64 = 1;
/// The `user_data` of its cancel.
const CANCEL: u64 = 2;

/// An enter that waited for the request's end: its time running out, or a signal, is the
/// probe's failure (no retry: the probe runs once, before any thread).
#[blaze::noalloc]
#[blaze::nopanic]
fn end_within(entered: Result<u32, Errno>) -> Result<(), Answer> {
    match entered {
        Ok(_) | Err(Errno::Busy) => Ok(()),
        Err(errno) => Err(Answer::Errno(errno)),
    }
}

/// Every system call probe: the calls present and the calls denied.
#[blaze::noalloc]
#[blaze::nopanic]
fn probe_calls() -> Result<(u16, u16), ProbeError> {
    let none = arg(Fd::new(-1));
    let mut calls = 0u16;
    let mut denied = 0u16;
    let mut set = |bit: u16, presence: Presence| match presence {
        Presence::Present => calls |= bit,
        Presence::Denied => denied |= bit,
        Presence::Absent => {}
    };
    // SAFETY: `flags` is 0 and `nr_futexes` 0: refused with EINVAL before the (null) array or
    // timeout is read.
    let result = unsafe { raw::syscall5(nr::FUTEX_WAITV, 0, 0, 0, 0, 0) };
    set(FUTEX_WAITV, answered(Fact::FutexWaitv, result, &[Errno::Inval])?);
    // SAFETY: `first > last`: refused with EINVAL before any descriptor is looked at.
    let result = unsafe { raw::syscall3(nr::CLOSE_RANGE, UINT_ALL, UINT_ALL - 1, 0) };
    set(CLOSE_RANGE, answered(Fact::CloseRange, result, &[Errno::Inval])?);
    // SAFETY: a size of 0 is below `CLONE_ARGS_SIZE_VER0`: refused with EINVAL before the
    // (null) arguments are read, so no child is made.
    let result = unsafe { raw::syscall2(nr::CLONE3, 0, 0) };
    set(CLONE3, answered(Fact::Clone3, result, &[Errno::Inval])?);
    // SAFETY: a size of 0 is below `OPEN_HOW_SIZE_VER0`: refused with EINVAL before the (null)
    // path or `open_how` is read.
    let result = unsafe { raw::syscall4(nr::OPENAT2, none, 0, 0, 0) };
    set(OPENAT2, answered(Fact::Openat2, result, &[Errno::Inval])?);
    // SAFETY: the name is a live empty string the kernel reads; without `AT_EMPTY_PATH` it
    // names nothing, and the reserved mask bit is refused, so the (null) buffer is never
    // written. Which refusal comes first differs by kernel; each listed one is presence.
    let result =
        unsafe { raw::syscall5(nr::STATX, none, c"".as_ptr() as usize, 0, STATX_RESERVED, 0) };
    set(STATX, answered(Fact::Statx, result, &[Errno::Inval, Errno::NoEnt, Errno::BadF, Errno::Fault])?);
    // SAFETY: unknown flags, then pid 0: refused with EINVAL; no descriptor is made.
    let result = unsafe { raw::syscall2(nr::PIDFD_OPEN, 0, UINT_ALL) };
    set(PIDFD_OPEN, answered(Fact::PidfdOpen, result, &[Errno::Inval])?);
    // SAFETY: descriptor -1: EBADF; signal 0 and a null `siginfo`, nothing is sent.
    let result = unsafe { raw::syscall4(nr::PIDFD_SEND_SIGNAL, none, 0, 0, 0) };
    set(PIDFD_SEND_SIGNAL, answered(Fact::PidfdSendSignal, result, &[Errno::BadF])?);
    // SAFETY: descriptor -1: EBADF before the (null) offsets are read; length 0.
    let result = unsafe { raw::syscall6(nr::COPY_FILE_RANGE, none, 0, none, 0, 0, 0) };
    set(COPY_FILE_RANGE, answered(Fact::CopyFileRange, result, &[Errno::BadF])?);
    // SAFETY: unknown flags: refused with EINVAL; no descriptor is made.
    let result = unsafe { raw::syscall1(nr::MEMFD_SECRET, UINT_ALL) };
    set(MEMFD_SECRET, answered(Fact::MemfdSecret, result, &[Errno::Inval])?);
    // SAFETY: descriptor ~0: EBADF before the (null) range or answer is read or written.
    let result = unsafe { raw::syscall4(nr::CACHESTAT, UINT_ALL, 0, 0, 0) };
    set(CACHESTAT, answered(Fact::Cachestat, result, &[Errno::Inval, Errno::BadF])?);
    // SAFETY: nonzero flags are refused first (EINVAL); were the range checked first, a length
    // of 0 seals nothing and answers 0.
    let result = unsafe { raw::syscall3(nr::MSEAL, 0, 0, usize::MAX) };
    set(
        MSEAL,
        match raw::check(result) {
            Ok(0) => Presence::Present,
            _ => answered(Fact::Mseal, result, &[Errno::Inval])?,
        },
    );
    // SAFETY: a null buffer of length 0: a kernel with `PR_GET_AUXV` copies nothing and
    // answers the saved vector's size; one without refuses the unknown option with EINVAL.
    let result = unsafe { raw::syscall5(nr::PRCTL, PR_GET_AUXV, 0, 0, 0, 0) };
    set(
        GET_AUXV,
        match raw::check(result) {
            Ok(size) if size > 0 => Presence::Present,
            Err(Errno::Inval | Errno::NoSys) => Presence::Absent,
            Err(Errno::Perm | Errno::Acces) => Presence::Denied,
            Ok(value) => return Err(ProbeError { fact: Fact::GetAuxv, answer: Answer::Value(value) }),
            Err(errno) => return Err(ProbeError { fact: Fact::GetAuxv, answer: Answer::Errno(errno) }),
        },
    );
    // SAFETY: `PR_FUTEX_HASH_GET_SLOTS` reads no memory and changes nothing; a kernel before
    // 6.16 refuses the unknown option with EINVAL.
    let result = unsafe { raw::syscall5(nr::PRCTL, PR_FUTEX_HASH, PR_FUTEX_HASH_GET_SLOTS, 0, 0, 0) };
    set(
        FUTEX_HASH,
        match raw::check(result) {
            Ok(_) => Presence::Present,
            Err(Errno::Inval | Errno::NoSys) => Presence::Absent,
            Err(Errno::Perm | Errno::Acces) => Presence::Denied,
            Err(errno) => return Err(ProbeError { fact: Fact::FutexHash, answer: Answer::Errno(errno) }),
        },
    );
    Ok((calls, denied))
}

/// Landlock's fact, from [`landlock_abi`].
#[blaze::noalloc]
#[blaze::nopanic]
fn probe_landlock() -> Result<Landlock, ProbeError> {
    match landlock_abi() {
        Ok(abi) => Ok(Landlock::Abi(abi)),
        Err(Errno::NoSys) => Ok(Landlock::Absent),
        Err(Errno::OpNotSupp) => Ok(Landlock::Disabled),
        Err(Errno::Perm | Errno::Acces) => Ok(Landlock::Denied),
        Err(errno) => Err(ProbeError { fact: Fact::Landlock, answer: Answer::Errno(errno) }),
    }
}

/// `bytes` written into `out` from `at`; what does not fit is not written (the callers'
/// offsets are constants within [`ENCODED`]).
#[blaze::noalloc]
#[blaze::nopanic]
fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
    let mut each = 0usize;
    while let (Some(&byte), Some(slot)) = (bytes.get(each), out.get_mut(at.wrapping_add(each))) {
        *slot = byte;
        each = each.wrapping_add(1);
    }
}

/// `N` bytes of `bytes` from `at`.
#[blaze::noalloc]
#[blaze::nopanic]
fn take<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.first_chunk::<N>().copied()
}

/// The ring's flags byte: a presence as its two bits (`present` at `at`, `denied` at `at + 1`).
#[blaze::noalloc]
#[blaze::nopanic]
const fn presence_bits(presence: Presence, at: u32) -> u8 {
    match presence {
        Presence::Absent => 0,
        Presence::Present => 1 << at,
        Presence::Denied => 1 << (at + 1),
    }
}

/// The presence two bits of the ring's flags byte hold; `None` for both set.
#[blaze::noalloc]
#[blaze::nopanic]
const fn presence_of(flags: u8, at: u32) -> Option<Presence> {
    match (flags >> at) & 0b11 {
        0 => Some(Presence::Absent),
        1 => Some(Presence::Present),
        2 => Some(Presence::Denied),
        _ => None,
    }
}

impl KernelFacts {
    /// Every probe, once: the ring's (two throwaway rings, closed, and a region registered
    /// and unmapped), each system call's, Landlock's, and last the ring futex's (a third ring,
    /// closed). No allocation, and no thread is made.
    ///
    /// Called by the top-level process BEFORE its second thread (see the module doc): the
    /// ring futex probe sees the kernel's behaviour only where the process has no private
    /// futex hash yet, and it leaves one installed (`PR_FUTEX_HASH_SET_SLOTS`, 16 slots, the
    /// kernel's own default for one thread) wherever the kernel makes them: the wait slot's
    /// guard, which the `Ring` form's start would install anyway. Nothing else is left behind.
    /// A child never probes: it takes [`KernelFacts::from_parent`].
    ///
    /// [`ProbeError`] where a probe answers something it should not.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn probe() -> Result<KernelFacts, ProbeError> {
        let ring = probe_ring()?;
        let (calls, denied) = probe_calls()?;
        let landlock = probe_landlock()?;
        let ring_futex = probe_ring_futex(&ring)?;
        Ok(KernelFacts { ring, calls, denied, ring_futex, landlock })
    }

    /// The facts a parent told this child, in [`KernelFacts::to_bytes`]'s encoding.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn from_parent(bytes: &[u8]) -> Result<KernelFacts, FromParentError> {
        if bytes.len() != ENCODED {
            return Err(FromParentError::Length { found: bytes.len() });
        }
        let short = FromParentError::Length { found: bytes.len() };
        let [version] = take::<1>(bytes, 0).ok_or(short)?;
        if version != VERSION {
            return Err(FromParentError::Version { found: version });
        }
        let [state] = take::<1>(bytes, 1).ok_or(short)?;
        let denial = u16::from_le_bytes(take::<2>(bytes, 2).ok_or(short)?);
        let features = u32::from_le_bytes(take::<4>(bytes, 4).ok_or(short)?);
        let [flags] = take::<1>(bytes, 8).ok_or(short)?;
        let known = u16::from_le_bytes(take::<2>(bytes, 9).ok_or(short)?);
        let ops = take::<32>(bytes, 11).ok_or(short)?;
        let calls = u16::from_le_bytes(take::<2>(bytes, 43).ok_or(short)?);
        let denied = u16::from_le_bytes(take::<2>(bytes, 45).ok_or(short)?);
        let [futex] = take::<1>(bytes, 47).ok_or(short)?;
        let [kind] = take::<1>(bytes, 48).ok_or(short)?;
        let abi = u32::from_le_bytes(take::<4>(bytes, 49).ok_or(short)?);
        let state = match state {
            0 => RingState::Absent,
            1 => RingState::Present,
            2 => RingState::Denied(Errno::from_raw(denial)),
            _ => return Err(FromParentError::Invalid { field: "the ring's state" }),
        };
        let bad_flags = FromParentError::Invalid { field: "the ring's flags" };
        if flags & !0b1_1111 != 0 || (!matches!(state, RingState::Present) && flags != 0) {
            return Err(bad_flags);
        }
        let wait_ring_flags = presence_of(flags, 1).ok_or(bad_flags)?;
        let mem_region = presence_of(flags, 3).ok_or(bad_flags)?;
        if usize::from(known) > PROBE_OPS {
            return Err(FromParentError::Invalid { field: "the ring's operations" });
        }
        if calls & !CALLS_ALL != 0 || denied & !CALLS_ALL != 0 || calls & denied != 0 {
            return Err(FromParentError::Invalid { field: "the calls" });
        }
        let ring_futex = match (futex, state) {
            (0, _) => RingFutex::Absent,
            (1, _) => RingFutex::Denied,
            (2, RingState::Present) => RingFutex::Sound,
            (3, RingState::Present) => RingFutex::LostWake,
            _ => return Err(FromParentError::Invalid { field: "the ring futex" }),
        };
        let landlock = match (kind, abi) {
            (0, 0) => Landlock::Absent,
            (1, 0) => Landlock::Disabled,
            (2, abi @ 1..) => Landlock::Abi(abi),
            (3, 0) => Landlock::Denied,
            _ => return Err(FromParentError::Invalid { field: "landlock" }),
        };
        let ring = RingFacts {
            state,
            features,
            probed: flags & 1 != 0,
            known,
            ops,
            wait_ring_flags,
            mem_region,
        };
        Ok(KernelFacts { ring, calls, denied, ring_futex, landlock })
    }

    /// What a child is told ([`KernelFacts::from_parent`] reads it).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn to_bytes(&self, out: &mut [u8; ENCODED]) {
        let ring = &self.ring;
        let (state, denial) = match ring.state {
            RingState::Absent => (0u8, 0u16),
            RingState::Present => (1, 0),
            RingState::Denied(errno) => (2, errno.raw()),
        };
        let flags = u8::from(ring.probed) | presence_bits(ring.wait_ring_flags, 1) | presence_bits(ring.mem_region, 3);
        let futex = match self.ring_futex {
            RingFutex::Absent => 0u8,
            RingFutex::Denied => 1,
            RingFutex::Sound => 2,
            RingFutex::LostWake => 3,
        };
        let (kind, abi) = match self.landlock {
            Landlock::Absent => (0u8, 0u32),
            Landlock::Disabled => (1, 0),
            Landlock::Abi(abi) => (2, abi),
            Landlock::Denied => (3, 0),
        };
        put(out, 0, &[VERSION, state]);
        put(out, 2, &denial.to_le_bytes());
        put(out, 4, &ring.features.to_le_bytes());
        put(out, 8, &[flags]);
        put(out, 9, &ring.known.to_le_bytes());
        put(out, 11, &ring.ops);
        put(out, 43, &self.calls.to_le_bytes());
        put(out, 45, &self.denied.to_le_bytes());
        put(out, 47, &[futex, kind]);
        put(out, 49, &abi.to_le_bytes());
    }

    /// The ring's facts.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn ring(&self) -> &RingFacts {
        &self.ring
    }

    /// What the probe of `fact` found. A fact of the ring where the ring is not present is
    /// the ring's own answer; the atomic write, a fact of a file, is absent here.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn presence(&self, fact: Fact) -> Presence {
        let ring = match self.ring.state {
            RingState::Absent => Presence::Absent,
            RingState::Denied(_) => Presence::Denied,
            RingState::Present => Presence::Present,
        };
        let call = Self::call;
        match fact {
            Fact::Ring => ring,
            Fact::RingOps if !ring.present() => ring,
            Fact::RingOps => {
                if self.ring.probed {
                    Presence::Present
                } else {
                    Presence::Absent
                }
            }
            Fact::RingWaitFlags if !ring.present() => ring,
            Fact::RingWaitFlags => self.ring.wait_ring_flags,
            Fact::RingMemRegion if !ring.present() => ring,
            Fact::RingMemRegion => self.ring.mem_region,
            Fact::RingFutex => match self.ring_futex {
                RingFutex::Absent => Presence::Absent,
                RingFutex::Denied => Presence::Denied,
                RingFutex::Sound | RingFutex::LostWake => Presence::Present,
            },
            Fact::FutexWaitv => call(FUTEX_WAITV, self),
            Fact::CloseRange => call(CLOSE_RANGE, self),
            Fact::Clone3 => call(CLONE3, self),
            Fact::Openat2 => call(OPENAT2, self),
            Fact::Statx => call(STATX, self),
            Fact::PidfdOpen => call(PIDFD_OPEN, self),
            Fact::PidfdSendSignal => call(PIDFD_SEND_SIGNAL, self),
            Fact::CopyFileRange => call(COPY_FILE_RANGE, self),
            Fact::MemfdSecret => call(MEMFD_SECRET, self),
            Fact::Cachestat => call(CACHESTAT, self),
            Fact::Mseal => call(MSEAL, self),
            Fact::GetAuxv => call(GET_AUXV, self),
            Fact::FutexHash => call(FUTEX_HASH, self),
            Fact::Landlock => match self.landlock {
                Landlock::Abi(_) => Presence::Present,
                Landlock::Absent | Landlock::Disabled => Presence::Absent,
                Landlock::Denied => Presence::Denied,
            },
            Fact::AtomicWrite => Presence::Absent,
        }
    }

    /// The presence of the call whose bit is `bit`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    const fn call(bit: u16, facts: &KernelFacts) -> Presence {
        if facts.calls & bit != 0 {
            Presence::Present
        } else if facts.denied & bit != 0 {
            Presence::Denied
        } else {
            Presence::Absent
        }
    }

    /// `futex_waitv` (5.16).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn futex_waitv(&self) -> bool {
        self.calls & FUTEX_WAITV != 0
    }

    /// `close_range` (5.9).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn close_range(&self) -> bool {
        self.calls & CLOSE_RANGE != 0
    }

    /// `clone3` (5.3).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn clone3(&self) -> bool {
        self.calls & CLONE3 != 0
    }

    /// `openat2` (5.6).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn openat2(&self) -> bool {
        self.calls & OPENAT2 != 0
    }

    /// `statx` (4.11).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn statx(&self) -> bool {
        self.calls & STATX != 0
    }

    /// `pidfd_open` (5.3).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn pidfd_open(&self) -> bool {
        self.calls & PIDFD_OPEN != 0
    }

    /// `pidfd_send_signal` (5.1).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn pidfd_send_signal(&self) -> bool {
        self.calls & PIDFD_SEND_SIGNAL != 0
    }

    /// Both pidfd calls.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn pidfd(&self) -> bool {
        self.pidfd_open() && self.pidfd_send_signal()
    }

    /// `copy_file_range` (4.5).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn copy_file_range(&self) -> bool {
        self.calls & COPY_FILE_RANGE != 0
    }

    /// `memfd_secret` (5.14, and enabled).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn memfd_secret(&self) -> bool {
        self.calls & MEMFD_SECRET != 0
    }

    /// `cachestat` (6.5).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn cachestat(&self) -> bool {
        self.calls & CACHESTAT != 0
    }

    /// `mseal` (6.10).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn mseal(&self) -> bool {
        self.calls & MSEAL != 0
    }

    /// `prctl(PR_GET_AUXV)` (6.4): what `facts::Aux::for_top_level` takes.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn get_auxv(&self) -> bool {
        self.calls & GET_AUXV != 0
    }

    /// `prctl(PR_FUTEX_HASH)` (6.16): the kernel gives processes private futex hashes, and
    /// this one may ask for its own (`slot::install_private_hash`).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn futex_hash(&self) -> bool {
        self.calls & FUTEX_HASH != 0
    }

    /// The ring futex fact (the module doc's section).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn ring_futex(&self) -> RingFutex {
        self.ring_futex
    }

    /// These facts as a child confined by `filter` ([`super::confine::build`]) finds them: what
    /// the parent sends in its start message in place of its own facts, so the child chooses
    /// from facts (the wait slot's direct form, no ring), never from a mark. The filter answers
    /// `io_uring_setup` with `EPERM` ([`super::confine::deny::EPERM`]), so the ring and the
    /// ring futex read denied; without `processes` it answers `clone3` with `ENOSYS`, so
    /// `clone3` reads absent. Every other fact is the kernel's, which the filter allows.
    /// A test holds this to `build`: each call it refuses is a fact here, or none probes it.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn under(self, filter: super::confine::Filter) -> KernelFacts {
        let mut facts = self;
        facts.ring = RingFacts::without(RingState::Denied(Errno::Perm));
        facts.ring_futex = RingFutex::Denied;
        if !filter.processes {
            facts.calls &= !CLONE3;
            facts.denied &= !CLONE3;
        }
        facts
    }

    /// Landlock.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn landlock(&self) -> Landlock {
        self.landlock
    }
}

/// The profile line: `kernel: ring ops N of M, ...`. A denied fact prints `denied`, so a
/// person sees why an older form was chosen.
impl core::fmt::Display for KernelFacts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let ring = &self.ring;
        match ring.state {
            RingState::Absent => f.write_str("kernel: ring no")?,
            RingState::Denied(errno) => write!(f, "kernel: ring denied ({})", errno.name())?,
            RingState::Present if !ring.probed => f.write_str("kernel: ring ops ? (no probe)")?,
            RingState::Present => write!(f, "kernel: ring ops {} of {}", ring.supported_ops(), ring.known)?,
        }
        let shown = |fact: Fact| self.presence(fact).name();
        let pidfd = match (self.presence(Fact::PidfdOpen), self.presence(Fact::PidfdSendSignal)) {
            (Presence::Present, Presence::Present) => "yes",
            (Presence::Denied, _) | (_, Presence::Denied) => "denied",
            _ => "no",
        };
        write!(
            f,
            ", ring wait flags {}, ring mem region {}, ring futex {}, futex hash {}, waitv {}, close_range {}, \
             clone3 {}, openat2 {}, statx {}, pidfd {}, copy_file_range {}, memfd_secret {}, cachestat {}, mseal {}, ",
            shown(Fact::RingWaitFlags),
            shown(Fact::RingMemRegion),
            self.ring_futex.name(),
            shown(Fact::FutexHash),
            shown(Fact::FutexWaitv),
            shown(Fact::CloseRange),
            shown(Fact::Clone3),
            shown(Fact::Openat2),
            shown(Fact::Statx),
            pidfd,
            shown(Fact::CopyFileRange),
            shown(Fact::MemfdSecret),
            shown(Fact::Cachestat),
            shown(Fact::Mseal),
        )?;
        match self.landlock {
            Landlock::Absent => f.write_str("landlock no")?,
            Landlock::Disabled => f.write_str("landlock off")?,
            Landlock::Denied => f.write_str("landlock denied")?,
            Landlock::Abi(abi) => write!(f, "landlock {abi}")?,
        }
        write!(f, ", get_auxv {}; {FLOOR}", shown(Fact::GetAuxv))
    }
}

/// The probes on the real kernel: each answers, a parent's encoding round-trips, and the
/// profile line has every fact.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probes_answer_and_round_trip() {
        let facts = KernelFacts::probe().unwrap();
        println!("{facts}");
        // Every kernel blaze's tests run on has these (ix VM, 7.x).
        assert!(facts.statx() && facts.pidfd() && facts.copy_file_range());
        let mut bytes = [0u8; ENCODED];
        facts.to_bytes(&mut bytes);
        assert_eq!(KernelFacts::from_parent(&bytes), Ok(facts));
        assert_eq!(
            KernelFacts::from_parent(&bytes[..ENCODED - 1]),
            Err(FromParentError::Length { found: ENCODED - 1 })
        );
        let mut wrong = bytes;
        wrong[0] = VERSION + 1;
        assert_eq!(KernelFacts::from_parent(&wrong), Err(FromParentError::Version { found: VERSION + 1 }));
        let mut wrong = bytes;
        wrong[1] = 9;
        assert_eq!(KernelFacts::from_parent(&wrong), Err(FromParentError::Invalid { field: "the ring's state" }));
        let line = facts.to_string();
        for name in ["kernel: ring", "waitv", "close_range", "clone3", "openat2", "statx", "pidfd", "cachestat", "mseal", "landlock", "get_auxv", "ring mem region", "ring futex", "futex hash", "floor 6.0"] {
            assert!(line.contains(name), "{line} lacks {name}");
        }
    }

    /// The probe's presence answers are the real call's: `get_auxv` against libc's `prctl`,
    /// and the ring's operations hold what blaze-core's driver uses where the ring is there.
    #[test]
    fn the_probes_agree_with_the_calls() {
        let facts = KernelFacts::probe().unwrap();
        let mut room = [0u8; 4096];
        let got = super::super::auxv::prctl_get_auxv(&mut room);
        assert_eq!(facts.get_auxv(), got.is_ok(), "{got:?}");
        if facts.ring().present() && facts.ring().probed() {
            // Operation 0 (`NOP`) is in every ring.
            assert!(facts.ring().op(0));
            assert!(facts.ring().supported_ops() <= facts.ring().known_ops());
        }
    }

    /// A kernel with every fact present.
    fn everything() -> KernelFacts {
        let mut ops = [0xffu8; 32];
        ops[31] = 0x7f;
        KernelFacts {
            ring: RingFacts {
                state: RingState::Present,
                features: 0x3fff,
                probed: true,
                known: 60,
                ops,
                wait_ring_flags: Presence::Present,
                mem_region: Presence::Present,
            },
            calls: CALLS_ALL,
            denied: 0,
            ring_futex: RingFutex::Sound,
            landlock: Landlock::Abi(7),
        }
    }

    /// `facts` through the encoding, and its profile line naming a denial where it has one.
    fn round_trips(facts: KernelFacts) {
        let mut bytes = [0u8; ENCODED];
        facts.to_bytes(&mut bytes);
        assert_eq!(KernelFacts::from_parent(&bytes), Ok(facts), "{facts:?}");
        let mut again = [0u8; ENCODED];
        KernelFacts::from_parent(&bytes).unwrap().to_bytes(&mut again);
        assert_eq!(bytes, again, "the bytes are the same written twice");
    }

    /// A confined child's facts: the ring denied (as the filter answers `io_uring_setup`), so
    /// the wait slot picks the direct form from facts; `clone3` absent without `processes`; the
    /// rest the kernel's; the result round-trips through the start message's encoding.
    #[test]
    fn facts_under_the_confinement_filter() {
        use super::super::confine::{Filter, deny};
        use super::super::slot::{Wait, WaitForm};
        assert!(deny::EPERM.contains(&(super::super::nr::IO_URING_SETUP as u32)), "the filter denies io_uring_setup");
        for processes in [false, true] {
            let facts = everything().under(Filter { processes });
            assert_eq!(facts.presence(Fact::Ring), Presence::Denied);
            assert_eq!(facts.presence(Fact::RingFutex), Presence::Denied);
            let clone3 = if processes { Presence::Present } else { Presence::Absent };
            assert_eq!(facts.presence(Fact::Clone3), clone3);
            assert_eq!(facts.presence(Fact::Statx), Presence::Present);
            assert!(!WaitForm::Ring.available(&facts));
            assert!(matches!(Wait::choose(&facts), Ok(Wait::Direct)));
            round_trips(facts);
        }
    }

    /// Every denied fact, alone and all at once, round-trips, reads `Denied` from
    /// `presence`, and prints `denied`; a byte naming present and denied at once is refused.
    #[test]
    fn every_denial_round_trips() {
        round_trips(everything());
        let mut all = everything();
        for (bit, fact) in CALL_FACTS.into_iter().enumerate() {
            let bit = 1u16 << bit;
            let mut one = everything();
            one.calls &= !bit;
            one.denied |= bit;
            all.calls &= !bit;
            all.denied |= bit;
            assert_eq!(one.presence(fact), Presence::Denied, "{fact:?}");
            assert!(one.to_string().contains("denied"), "{one}");
            round_trips(one);
        }
        let mut wait_flags = everything();
        wait_flags.ring.wait_ring_flags = Presence::Denied;
        wait_flags.ring.mem_region = Presence::Denied;
        wait_flags.ring_futex = RingFutex::Denied;
        assert_eq!(wait_flags.presence(Fact::RingWaitFlags), Presence::Denied);
        round_trips(wait_flags);
        let mut region = everything();
        region.ring.mem_region = Presence::Denied;
        assert_eq!(region.presence(Fact::RingMemRegion), Presence::Denied);
        round_trips(region);
        let mut futex = everything();
        futex.ring_futex = RingFutex::Denied;
        assert_eq!(futex.presence(Fact::RingFutex), Presence::Denied);
        round_trips(futex);
        let mut lost = everything();
        lost.ring_futex = RingFutex::LostWake;
        assert!(lost.to_string().contains("ring futex lost-wake"), "{lost}");
        round_trips(lost);
        let mut landlock = everything();
        landlock.landlock = Landlock::Denied;
        assert_eq!(landlock.presence(Fact::Landlock), Presence::Denied);
        round_trips(landlock);
        all.ring = RingFacts::without(RingState::Denied(Errno::Perm));
        all.ring_futex = RingFutex::Denied;
        all.landlock = Landlock::Denied;
        for fact in [Fact::Ring, Fact::RingOps, Fact::RingWaitFlags, Fact::RingMemRegion, Fact::RingFutex] {
            assert_eq!(all.presence(fact), Presence::Denied, "{fact:?}");
        }
        assert!(all.to_string().contains("ring denied (EPERM)"), "{all}");
        round_trips(all);
        let mut acces = all;
        acces.ring = RingFacts::without(RingState::Denied(Errno::Acces));
        round_trips(acces);

        let mut bytes = [0u8; ENCODED];
        everything().to_bytes(&mut bytes);
        let mut both = bytes;
        both[45] = 1; // FUTEX_WAITV denied while present
        assert_eq!(KernelFacts::from_parent(&both), Err(FromParentError::Invalid { field: "the calls" }));
        let mut both = bytes;
        both[8] |= 0b110; // the wait flags present and denied
        assert_eq!(KernelFacts::from_parent(&both), Err(FromParentError::Invalid { field: "the ring's flags" }));
        let mut futex = bytes;
        futex[47] = 4;
        assert_eq!(KernelFacts::from_parent(&futex), Err(FromParentError::Invalid { field: "the ring futex" }));
        let mut landlock = bytes;
        landlock[48] = 3;
        assert_eq!(KernelFacts::from_parent(&landlock), Err(FromParentError::Invalid { field: "landlock" }), "denied with an ABI");
        let mut sound_without_ring = [0u8; ENCODED];
        all.to_bytes(&mut sound_without_ring);
        sound_without_ring[47] = 2;
        assert_eq!(
            KernelFacts::from_parent(&sound_without_ring),
            Err(FromParentError::Invalid { field: "the ring futex" })
        );
    }
}
