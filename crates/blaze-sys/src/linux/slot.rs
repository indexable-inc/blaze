//! The slots (D185, rule 78): each operation the kernel offers in several forms is one seam,
//! and its form a strategy value chosen once from [`KernelFacts`] before any work, never on a
//! failure (rule 3). Every form of a slot gives the same answer; `linux/tests.rs` runs every
//! form this machine has against the others. A slot with no form here is a [`Refused`] naming
//! the missing fact. The table of slots, forms and kernels is in `linux::probe`'s doc.
//!
//! Each slot is an enum of its forms with:
//! - `choose(&KernelFacts) -> Result<Self, Refused>`: the best form this kernel has;
//! - `ALL` and `available(self, &KernelFacts)`: every form, and whether this kernel has it
//!   (the conformance tests' list, with no allocation);
//! - the operation, as a method of the form.
//!
//! Waiting on descriptors has no slot: blaze uses no epoll (`epoll_pwait2`, 5.11; `epoll_pwait`,
//! 2.6.19); the ring waits on descriptors (`IORING_OP_POLL_ADD`).

use core::num::NonZeroUsize;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

use super::fdctl::mincore;
use super::fs::{arg, owned};
use super::probe::{Fact, HashStep, KernelFacts, Presence, RingFutex, ensure_private_hash};
use super::task::fstatat;
use super::{
    AtFlags, CloneArgs, CloneFlags, Cloned, MapFlags, OpenFlags, OpenHow, OwnedFd, Prot, RWF_ATOMIC,
    Resolve, StatxMask, clone3_process, futex_wait_bitset, futex_wake, getdents64, kill, mmap, munmap,
    nr, openat, openat2, pread, pwrite, pwrite_flagged, raw, statx,
};
use crate::{Errno, Fd, Path, Timespec};

/// An operation with forms: what a [`Refused`] names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    /// [`WaitMany`].
    WaitMany,
    /// [`CloseRange`].
    CloseRange,
    /// [`StartProcess`].
    StartProcess,
    /// [`FileFacts`].
    FileFacts,
    /// [`OpenBeneath`].
    OpenBeneath,
    /// [`Signal`].
    Signal,
    /// [`CopyRange`].
    Copy,
    /// [`SecretMemory`].
    SecretMemory,
    /// [`ResidentPages`].
    ResidentPages,
    /// [`Seal`].
    Seal,
    /// [`AtomicWrite`].
    AtomicWrite,
    /// [`Wait`].
    Wait,
}

impl Slot {
    /// The slot's name, as an error prints it.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn name(self) -> &'static str {
        match self {
            Slot::WaitMany => "wait on several words",
            Slot::CloseRange => "close a range",
            Slot::StartProcess => "start a process",
            Slot::FileFacts => "file facts",
            Slot::OpenBeneath => "open beneath",
            Slot::Signal => "signal a process",
            Slot::Copy => "copy",
            Slot::SecretMemory => "secret memory",
            Slot::ResidentPages => "resident pages",
            Slot::Seal => "seal a mapping",
            Slot::AtomicWrite => "atomic write",
            Slot::Wait => "wait",
        }
    }
}

/// A slot with no form on this kernel: the slot and the fact it lacks. A denied fact is an
/// absent one to a slot (`probe::Presence`); [`KernelFacts::presence`] of `fact` says which,
/// and [`WaitStartError::Refused`] carries it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Refused {
    /// The slot.
    pub slot: Slot,
    /// The fact no form could do without.
    pub fact: Fact,
}

impl core::fmt::Display for Refused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}: no form on this kernel: {} (Linux {}) is absent or denied here, by `{}`",
            self.slot.name(),
            self.fact.name(),
            self.fact.since(),
            self.fact.probe()
        )
    }
}

impl core::error::Error for Refused {}

// ---------------------------------------------------------------------------------------------
// Wait on several words.

/// The most words one wait watches: `FUTEX_WAITV_MAX`.
pub const WAIT_MANY_MAX: usize = 128;

/// A word watched by [`WaitMany::wait`] while it holds `expected`.
#[derive(Clone, Copy, Debug)]
pub struct Watched<'a> {
    /// The word.
    pub word: &'a AtomicU32,
    /// What it holds while there is nothing to do.
    pub expected: u32,
}

/// `struct futex_waitv` (linux/futex.h).
#[derive(Clone, Copy)]
#[repr(C)]
struct FutexWaitv {
    val: u64,
    uaddr: u64,
    flags: u32,
    reserved: u32,
}

// linux/futex.h: `struct futex_waitv` is 24 bytes, `uaddr` at 8 and `flags` at 16.
const _: () = {
    assert!(size_of::<FutexWaitv>() == 24);
    assert!(core::mem::offset_of!(FutexWaitv, uaddr) == 8);
    assert!(core::mem::offset_of!(FutexWaitv, flags) == 16);
};

/// `FUTEX2_SIZE_U32 | FUTEX2_PRIVATE`.
const FUTEX2_U32_PRIVATE: u32 = 0x02 | 128;
/// `CLOCK_MONOTONIC`.
const CLOCK_MONOTONIC: usize = 1;

/// Sleep until one of several words changes. Every waker stores its word, then calls
/// [`WaitMany::wake`] with the same form and wake word as the waiters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaitMany {
    /// `futex_waitv` (5.16): the kernel watches each word.
    Waitv,
    /// One futex on a shared wake word, bumped by every waker (2.6.25): the words are checked
    /// before the sleep; a waker of any word wakes every waiter.
    WakeWord,
}

impl WaitMany {
    /// Every form.
    pub const ALL: [WaitMany; 2] = [WaitMany::Waitv, WaitMany::WakeWord];

    /// The best form here.
    /// replaces a single `futex_wait_bitset` where several words are watched (no caller today)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn choose(facts: &KernelFacts) -> Result<WaitMany, Refused> {
        Ok(if facts.futex_waitv() { WaitMany::Waitv } else { WaitMany::WakeWord })
    }

    /// Whether this kernel has the form.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn available(self, facts: &KernelFacts) -> bool {
        match self {
            WaitMany::Waitv => facts.futex_waitv(),
            WaitMany::WakeWord => true,
        }
    }

    /// Sleeps while every word holds its expected value, until `at` (absolute
    /// `CLOCK_MONOTONIC`; no limit for `None`). `Ok` when woken (a word may have changed);
    /// [`Errno::Again`] where a word already differs; [`Errno::TimedOut`]; [`Errno::Intr`].
    /// No words or more than [`WAIT_MANY_MAX`] is [`Errno::Inval`].
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn wait(self, words: &[Watched<'_>], wake: &AtomicU32, at: Option<Timespec>) -> Result<(), Errno> {
        if words.is_empty() || words.len() > WAIT_MANY_MAX {
            return Err(Errno::Inval);
        }
        match self {
            WaitMany::Waitv => {
                let mut waiters = [FutexWaitv { val: 0, uaddr: 0, flags: 0, reserved: 0 }; WAIT_MANY_MAX];
                let mut at_index = 0usize;
                while let (Some(each), Some(to)) = (words.get(at_index), waiters.get_mut(at_index)) {
                    *to = FutexWaitv {
                        val: u64::from(each.expected),
                        uaddr: each.word.as_ptr() as u64,
                        flags: FUTEX2_U32_PRIVATE,
                        reserved: 0,
                    };
                    at_index = at_index.wrapping_add(1);
                }
                let timeout = at.as_ref().map_or(0, |it| core::ptr::from_ref(it) as usize);
                // SAFETY: `waiters` is live, its first `words.len()` entries each name a live
                // `u32` of this process the kernel only reads; the timespec (null for none) is
                // a live local; the kernel keeps no pointer after the call.
                raw::check(unsafe {
                    raw::syscall5(
                        nr::FUTEX_WAITV,
                        waiters.as_ptr() as usize,
                        words.len(),
                        0,
                        timeout,
                        CLOCK_MONOTONIC,
                    )
                })
                .map(|_| ())
            }
            WaitMany::WakeWord => {
                let seen = wake.load(Ordering::Acquire);
                if words.iter().any(|each| each.word.load(Ordering::Acquire) != each.expected) {
                    return Err(Errno::Again);
                }
                // A waker stores its word before bumping `wake`: a change after the check
                // above moved `wake` from `seen`, and this wait returns at once.
                match futex_wait_bitset(wake, seen, at, super::FUTEX_ANY) {
                    // The wake word moved: a word changed (or a spurious bump), which is a wake.
                    Err(Errno::Again) => Ok(()),
                    other => other,
                }
            }
        }
    }

    /// Wakes every waiter on `word`, after the caller stored to it.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn wake(self, word: &AtomicU32, wake: &AtomicU32) -> Result<(), Errno> {
        match self {
            WaitMany::Waitv => futex_wake(word, u32::MAX).map(|_| ()),
            WaitMany::WakeWord => {
                wake.fetch_add(1, Ordering::Release);
                futex_wake(wake, u32::MAX).map(|_| ())
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Close a range.

/// Close every descriptor from a number up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CloseRange {
    /// `close_range(first, ~0, 0)` (5.9).
    Call,
    /// A `getdents64` walk of `/proc/self/fd`, closing each number at or above `first` (needs
    /// procfs mounted and readable: [`Errno::NoEnt`] where it is not).
    ProcWalk,
}

/// `/proc/self/fd`.
const PROC_SELF_FD: &core::ffi::CStr = c"/proc/self/fd";

impl CloseRange {
    /// Every form.
    pub const ALL: [CloseRange; 2] = [CloseRange::Call, CloseRange::ProcWalk];

    /// The best form here.
    /// replaces libc::syscall(libc::SYS_close_range, 4, u32::MAX, 0) in blaze-launch-rt zygote.rs as the one choice
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn choose(facts: &KernelFacts) -> Result<CloseRange, Refused> {
        Ok(if facts.close_range() { CloseRange::Call } else { CloseRange::ProcWalk })
    }

    /// Whether this kernel has the form.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn available(self, facts: &KernelFacts) -> bool {
        match self {
            CloseRange::Call => facts.close_range(),
            CloseRange::ProcWalk => true,
        }
    }

    /// Closes every open descriptor numbered `first` or more. A descriptor whose close
    /// reports an error is closed all the same (as `close_range` does). `buffer` is the walk's
    /// room for directory entries (unused by [`CloseRange::Call`]).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn close_from(self, first: u32, buffer: &mut [u8]) -> Result<(), Errno> {
        match self {
            CloseRange::Call => {
                // SAFETY: the call takes numbers and no memory; the caller owns every
                // descriptor from `first` up and gives them all away.
                raw::check(unsafe { raw::syscall3(nr::CLOSE_RANGE, first as usize, u32::MAX as usize, 0) })
                    .map(|_| ())
            }
            CloseRange::ProcWalk => {
                let flags = OpenFlags::READ.with(OpenFlags::DIRECTORY).with(OpenFlags::CLOSE_ON_EXEC);
                let dir = openat(super::CWD, Path::new(PROC_SELF_FD), flags, 0)?;
                let own = dir.fd().raw();
                loop {
                    let mut entries = getdents64(dir.fd(), buffer)?.peekable();
                    if entries.peek().is_none() {
                        return Ok(());
                    }
                    for entry in entries {
                        let Some(number) = descriptor_number(entry.name) else { continue };
                        if number >= i64::from(first) && number != i64::from(own) {
                            // SAFETY: the call takes a number; the caller gave away every
                            // descriptor from `first` up, and the walk's own is skipped.
                            _ = unsafe { raw::syscall1(nr::CLOSE, number as usize) };
                        }
                    }
                }
            }
        }
    }
}

/// A `/proc/self/fd` entry's number; `None` for `.`, `..` or anything not decimal.
#[blaze::noalloc]
#[blaze::nopanic]
fn descriptor_number(name: &[u8]) -> Option<i64> {
    if name.is_empty() {
        return None;
    }
    let mut number = 0i64;
    for &byte in name {
        let digit = byte.checked_sub(b'0').filter(|digit| *digit <= 9)?;
        number = number.checked_mul(10)?.checked_add(i64::from(digit))?;
    }
    (number <= i64::from(i32::MAX)).then_some(number)
}

// ---------------------------------------------------------------------------------------------
// Start a process.

/// `SIGCHLD`.
const SIGCHLD: u64 = 17;
/// `CLONE_PIDFD`.
const CLONE_PIDFD: usize = 0x1000;

/// Which side of a [`StartProcess::fork`] this is.
#[derive(Debug)]
pub enum Started {
    /// The new process.
    Child,
    /// The caller: the child's pid and its pidfd.
    Parent {
        /// The child's pid.
        pid: i32,
        /// The child's pidfd.
        pidfd: OwnedFd,
    },
}

/// A child with a copy of the caller's memory (a fork) and a pidfd for it, `SIGCHLD` at its
/// end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StartProcess {
    /// `clone3` with `CLONE_PIDFD` (5.3).
    Clone3,
    /// `clone(SIGCHLD | CLONE_PIDFD)`, the pidfd written through the parent-tid argument
    /// (5.2; a kernel before ignores the flag, so the form is chosen on `pidfd_open`, 5.3).
    Clone,
}

impl StartProcess {
    /// Every form.
    pub const ALL: [StartProcess; 2] = [StartProcess::Clone3, StartProcess::Clone];

    /// The best form here; [`Refused`] naming `pidfd_open` on a kernel with no pidfd.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn choose(facts: &KernelFacts) -> Result<StartProcess, Refused> {
        if facts.clone3() {
            Ok(StartProcess::Clone3)
        } else if facts.pidfd_open() {
            Ok(StartProcess::Clone)
        } else {
            Err(Refused { slot: Slot::StartProcess, fact: Fact::PidfdOpen })
        }
    }

    /// Whether this kernel has the form.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn available(self, facts: &KernelFacts) -> bool {
        match self {
            StartProcess::Clone3 => facts.clone3(),
            StartProcess::Clone => facts.pidfd_open(),
        }
    }

    /// Forks: [`Started::Child`] in the child, [`Started::Parent`] with the child's pid and
    /// pidfd in the caller.
    ///
    /// # Safety
    ///
    /// As [`clone3_process`]: the child of a process with other threads holds only the
    /// calling thread, and until it execs or exits may call only what is async-signal-safe.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub unsafe fn fork(self) -> Result<Started, Errno> {
        let mut pidfd: i32 = -1;
        let pid = match self {
            StartProcess::Clone3 => {
                let args = CloneArgs {
                    flags: CloneFlags::PIDFD.bits(),
                    pidfd: core::ptr::from_mut(&mut pidfd) as u64,
                    exit_signal: SIGCHLD,
                    ..CloneArgs::default()
                };
                // SAFETY: the caller keeps `clone3_process`'s contract; `args` asks for no
                // shared memory and no stack, and `pidfd` is a live `int` the kernel writes.
                match unsafe { clone3_process(&args) }? {
                    Cloned::Child => return Ok(Started::Child),
                    Cloned::Parent(pid) => pid,
                }
            }
            StartProcess::Clone => {
                // SAFETY: no `CLONE_VM` and stack 0: the child returns here on a copy of this
                // stack, as a fork; `pidfd` is a live `int` the kernel writes in the parent
                // (the third argument on x86-64 and aarch64 alike: `parent_tid`); the caller
                // keeps the async-signal-safe contract in the child.
                let result = unsafe {
                    raw::syscall5(
                        nr::CLONE,
                        CLONE_PIDFD | SIGCHLD as usize,
                        0,
                        core::ptr::from_mut(&mut pidfd) as usize,
                        0,
                        0,
                    )
                };
                match raw::check(result)? {
                    0 => return Ok(Started::Child),
                    pid => pid as i32,
                }
            }
        };
        if pidfd < 0 {
            // The kernel made a child and no pidfd: not a kernel these facts describe.
            return Err(Errno::BadF);
        }
        // SAFETY: the kernel just opened `pidfd` for this process, and nothing else owns it.
        Ok(Started::Parent { pid, pidfd: unsafe { OwnedFd::from_raw(pidfd) } })
    }

    /// Starts a child on a lent stack, sharing this process's memory until it execs or exits
    /// (`CLONE_VM | CLONE_VFORK`): [`super::vfork::start_on_stack`] in this form.
    ///
    /// # Safety
    ///
    /// As [`super::vfork::start_on_stack`].
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub unsafe fn start_on_stack<T>(
        self,
        stack: &mut [core::mem::MaybeUninit<u8>],
        entry: super::vfork::ChildEntry<T>,
        arg: &mut T,
    ) -> Result<super::vfork::Launched, super::vfork::StartFailed> {
        // SAFETY: the caller keeps start_on_stack's contract.
        unsafe { super::vfork::start_on_stack(self, stack, entry, arg) }
    }
}

// ---------------------------------------------------------------------------------------------
// File facts.

/// The facts of a file every form gives alike.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileStat {
    /// Type and permission bits (`st_mode`).
    pub mode: u16,
    /// The inode number.
    pub ino: u64,
    /// The device holding it: major, minor.
    pub dev: (u32, u32),
    /// The device it is, for a device file: major, minor.
    pub rdev: (u32, u32),
    /// Hard links.
    pub nlink: u64,
    /// Owner.
    pub uid: u32,
    /// Group.
    pub gid: u32,
    /// Bytes.
    pub size: u64,
    /// 512-byte blocks held.
    pub blocks: u64,
    /// Last change of the contents: seconds, nanoseconds.
    pub mtime: (i64, u32),
    /// Last change of the inode: seconds, nanoseconds.
    pub ctime: (i64, u32),
}

/// A device number's major and minor (`new_decode_dev`, linux/kdev_t.h).
#[blaze::noalloc]
#[blaze::nopanic]
const fn decode_dev(dev: u64) -> (u32, u32) {
    let major = ((dev & 0xfff00) >> 8) | ((dev >> 32) & 0xffff_f000);
    let minor = (dev & 0xff) | ((dev >> 12) & 0xffff_ff00);
    (major as u32, minor as u32)
}

/// The status of a file under a directory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileFacts {
    /// `statx(STATX_BASIC_STATS)` (4.11).
    Statx,
    /// `newfstatat` (2.6.16).
    Fstatat,
}

impl FileFacts {
    /// Every form.
    pub const ALL: [FileFacts; 2] = [FileFacts::Statx, FileFacts::Fstatat];

    /// The best form here.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn choose(facts: &KernelFacts) -> Result<FileFacts, Refused> {
        Ok(if facts.statx() { FileFacts::Statx } else { FileFacts::Fstatat })
    }

    /// Whether this kernel has the form.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn available(self, facts: &KernelFacts) -> bool {
        match self {
            FileFacts::Statx => facts.statx(),
            FileFacts::Fstatat => true,
        }
    }

    /// The status of `path` under `dir` (`flags` as `fstatat`'s: [`AtFlags::NO_FOLLOW`],
    /// [`AtFlags::EMPTY_PATH`]).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn stat(self, dir: Fd, path: Path<'_>, flags: AtFlags) -> Result<FileStat, Errno> {
        match self {
            FileFacts::Statx => {
                let it = statx(dir, path, flags, StatxMask::BASIC)?;
                Ok(FileStat {
                    mode: it.mode,
                    ino: it.ino,
                    dev: (it.dev_major, it.dev_minor),
                    rdev: (it.rdev_major, it.rdev_minor),
                    nlink: u64::from(it.nlink),
                    uid: it.uid,
                    gid: it.gid,
                    size: it.size,
                    blocks: it.blocks,
                    mtime: (it.mtime.sec, it.mtime.nsec),
                    ctime: (it.ctime.sec, it.ctime.nsec),
                })
            }
            FileFacts::Fstatat => {
                let it = fstatat(dir, path, flags)?;
                Ok(FileStat {
                    // The mode's bits fit 16 (`S_IFMT` and the permissions).
                    mode: it.st_mode as u16,
                    ino: it.st_ino,
                    dev: decode_dev(it.st_dev),
                    rdev: decode_dev(it.st_rdev),
                    nlink: u64::from(it.st_nlink),
                    uid: it.st_uid,
                    gid: it.st_gid,
                    size: it.st_size as u64,
                    blocks: it.st_blocks as u64,
                    mtime: (it.st_mtime, it.st_mtime_nsec as u32),
                    ctime: (it.st_ctime, it.st_ctime_nsec as u32),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Open beneath.

/// The longest component the walk copies.
const COMPONENT_MAX: usize = 256;

/// Open a relative path beneath a directory with no symlink anywhere on it. The contract,
/// checked before either form runs: an empty path is [`Errno::NoEnt`]; an absolute one or one
/// with a `..` component is [`Errno::XDev`]; a symlink at any component is [`Errno::Loop`]; a
/// trailing `/` asks for a directory. Stricter than `linux::open_beneath`, which follows a
/// link that stays beneath.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpenBeneath {
    /// `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` (5.6): one call.
    Openat2,
    /// `openat(O_PATH | O_DIRECTORY | O_NOFOLLOW)` per directory, then the last component
    /// with `O_NOFOLLOW` (2.6.39).
    Walk,
}

impl OpenBeneath {
    /// Every form.
    pub const ALL: [OpenBeneath; 2] = [OpenBeneath::Openat2, OpenBeneath::Walk];

    /// The best form here.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn choose(facts: &KernelFacts) -> Result<OpenBeneath, Refused> {
        Ok(if facts.openat2() { OpenBeneath::Openat2 } else { OpenBeneath::Walk })
    }

    /// Whether this kernel has the form.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn available(self, facts: &KernelFacts) -> bool {
        match self {
            OpenBeneath::Openat2 => facts.openat2(),
            OpenBeneath::Walk => true,
        }
    }

    /// Opens `path` (its bytes, no NUL) beneath `dir` with `flags` and `mode` (for a
    /// create), close-on-exec. `buffer` holds the path with its NUL (as [`Path::in_buffer`]).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn open(self, dir: Fd, path: &[u8], flags: OpenFlags, mode: u32, buffer: &mut [u8]) -> Result<OwnedFd, Errno> {
        if path.is_empty() {
            return Err(Errno::NoEnt);
        }
        if path.first() == Some(&b'/') || path.split(|&byte| byte == b'/').any(|part| part == b"..") {
            return Err(Errno::XDev);
        }
        let mut flags = flags.with(OpenFlags::CLOSE_ON_EXEC);
        if path.last() == Some(&b'/') {
            flags = flags.with(OpenFlags::DIRECTORY);
        }
        match self {
            OpenBeneath::Openat2 => {
                let whole = Path::in_buffer(path, buffer)?;
                let how = OpenHow::new(flags, mode, Resolve::BENEATH.with(Resolve::NO_SYMLINKS));
                openat2(dir, whole, &how)
            }
            OpenBeneath::Walk => walk_beneath(dir, path, flags, mode),
        }
    }
}

/// [`OpenBeneath::Walk`]: each directory opened `O_PATH | O_NOFOLLOW`, a link told apart from
/// a file by `fstatat` where the open says `ENOTDIR`.
#[blaze::noalloc]
#[blaze::nopanic]
fn walk_beneath(dir: Fd, path: &[u8], flags: OpenFlags, mode: u32) -> Result<OwnedFd, Errno> {
    let step = OpenFlags::PATH.with(OpenFlags::DIRECTORY).with(OpenFlags::NO_FOLLOW).with(OpenFlags::CLOSE_ON_EXEC);
    let mut parts = path.split(|&byte| byte == b'/').filter(|part| !part.is_empty() && *part != b".").peekable();
    let mut held: Option<OwnedFd> = None;
    let mut room = [0u8; COMPONENT_MAX];
    while let Some(part) = parts.next() {
        let at = held.as_ref().map_or(dir, OwnedFd::fd);
        let name = Path::in_buffer(part, &mut room)?;
        if parts.peek().is_none() {
            return openat(at, name, flags.with(OpenFlags::NO_FOLLOW), mode);
        }
        match openat(at, name, step, 0) {
            Ok(next) => held = Some(next),
            Err(Errno::NotDir) => {
                let name = Path::in_buffer(part, &mut room)?;
                let it = fstatat(at, name, AtFlags::NO_FOLLOW)?;
                // `S_IFLNK`.
                return Err(if it.st_mode & 0o170000 == 0o120000 { Errno::Loop } else { Errno::NotDir });
            }
            Err(errno) => return Err(errno),
        }
    }
    // Every component was `.`: the directory itself.
    let at = held.as_ref().map_or(dir, OwnedFd::fd);
    openat(at, Path::new(c"."), flags, mode)
}

// ---------------------------------------------------------------------------------------------
// Signal a process.

/// Send a signal to a child.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Signal {
    /// `pidfd_send_signal` (5.1): exact, whenever the pidfd is held.
    Pidfd,
    /// `kill` by pid: exact only while the caller is the parent and has not reaped the child
    /// (after that the pid may name another process).
    KillByPid,
}

impl Signal {
    /// Every form.
    pub const ALL: [Signal; 2] = [Signal::Pidfd, Signal::KillByPid];

    /// The best form here.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn choose(facts: &KernelFacts) -> Result<Signal, Refused> {
        Ok(if facts.pidfd_send_signal() { Signal::Pidfd } else { Signal::KillByPid })
    }

    /// Whether this kernel has the form.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn available(self, facts: &KernelFacts) -> bool {
        match self {
            Signal::Pidfd => facts.pidfd_send_signal(),
            Signal::KillByPid => true,
        }
    }

    /// Sends `signal` (0 asks only whether the child is there) to the child `pid` whose
    /// pidfd is `pidfd`. [`Errno::Srch`] where it has ended.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn send(self, pid: i32, pidfd: Fd, signal: u32) -> Result<(), Errno> {
        match self {
            Signal::Pidfd => super::process::pidfd_send_signal(pidfd, signal),
            Signal::KillByPid => kill(pid, signal),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Copy.

/// Copy bytes between two files.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CopyRange {
    /// `copy_file_range` (4.5): in the kernel, a reflink or server-side copy where the file
    /// system has one.
    CopyFileRange,
    /// `pread` then `pwrite` through the caller's buffer.
    Buffered,
}

impl CopyRange {
    /// Every form.
    pub const ALL: [CopyRange; 2] = [CopyRange::CopyFileRange, CopyRange::Buffered];

    /// The best form here between files on one file system (`same_file_system`) or two:
    /// across file systems `copy_file_range`'s answer differs by kernel (`EXDEV` before 5.3,
    /// again for some pairs from 5.19), so the buffered form is chosen there.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn choose(facts: &KernelFacts, same_file_system: bool) -> Result<CopyRange, Refused> {
        Ok(if facts.copy_file_range() && same_file_system { CopyRange::CopyFileRange } else { CopyRange::Buffered })
    }

    /// Whether this kernel has the form.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn available(self, facts: &KernelFacts) -> bool {
        match self {
            CopyRange::CopyFileRange => facts.copy_file_range(),
            CopyRange::Buffered => true,
        }
    }

    /// One step: up to `len` bytes from `from` at `from_at` to `to` at `to_at` (neither
    /// descriptor's position moves); how many were copied, 0 at `from`'s end. The caller
    /// loops. `buffer` is the buffered form's room (unused by the other); an empty one is
    /// [`Errno::Inval`] there.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn step(self, from: Fd, from_at: u64, to: Fd, to_at: u64, len: u64, buffer: &mut [u8]) -> Result<usize, Errno> {
        match self {
            CopyRange::CopyFileRange => super::copy_file_range(from, from_at, to, to_at, len),
            CopyRange::Buffered => {
                let most = usize::try_from(len).unwrap_or(usize::MAX).min(buffer.len());
                let Some(room) = buffer.get_mut(..most) else { return Err(Errno::Inval) };
                if room.is_empty() && len > 0 {
                    return Err(Errno::Inval);
                }
                let read = pread(from, room, from_at)?;
                let bytes = room.get(..read).unwrap_or(&[]);
                let mut done = 0usize;
                while let Some(left) = bytes.get(done..).filter(|left| !left.is_empty()) {
                    let wrote = pwrite(to, left, to_at.wrapping_add(done as u64))?;
                    if wrote == 0 {
                        return Err(Errno::NoSpc);
                    }
                    done = done.wrapping_add(wrote);
                }
                Ok(read)
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Secret memory.

/// Memory no other process, and not the kernel's direct map, can read: one form, or a refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SecretMemory {
    /// `memfd_secret` (5.14, and `secretmem.enable`).
    MemfdSecret,
}

impl SecretMemory {
    /// Every form.
    pub const ALL: [SecretMemory; 1] = [SecretMemory::MemfdSecret];

    /// The form, or [`Refused`] naming `memfd_secret`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn choose(facts: &KernelFacts) -> Result<SecretMemory, Refused> {
        if facts.memfd_secret() {
            Ok(SecretMemory::MemfdSecret)
        } else {
            Err(Refused { slot: Slot::SecretMemory, fact: Fact::MemfdSecret })
        }
    }

    /// Whether this kernel has the form.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn available(self, facts: &KernelFacts) -> bool {
        facts.memfd_secret()
    }

    /// A new secret memory file, close-on-exec: size it with `ftruncate`, then map it shared.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn create(self) -> Result<OwnedFd, Errno> {
        /// `O_CLOEXEC`, memfd_secret's one flag.
        const CLOEXEC: usize = 0o2000000;
        // SAFETY: the call takes a flag and no memory, and makes a descriptor owned here.
        owned(unsafe { raw::syscall1(nr::MEMFD_SECRET, CLOEXEC) })
    }
}

// ---------------------------------------------------------------------------------------------
// Resident pages.

/// `struct cachestat_range` (linux/mman.h).
#[repr(C)]
struct CachestatRange {
    off: u64,
    len: u64,
}

/// `struct cachestat` (linux/mman.h).
#[derive(Default)]
#[repr(C)]
struct Cachestat {
    nr_cache: u64,
    nr_dirty: u64,
    nr_writeback: u64,
    nr_evicted: u64,
    nr_recently_evicted: u64,
}

// linux/mman.h: `cachestat_range` is two `__u64`, `cachestat` five.
const _: () = {
    assert!(size_of::<CachestatRange>() == 16);
    assert!(size_of::<Cachestat>() == 40);
    assert!(core::mem::offset_of!(Cachestat, nr_dirty) == 8);
};

/// How many pages of a file's range are in the page cache.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResidentPages {
    /// `cachestat` (6.5): asks the cache directly, no mapping. From 6.10 it needs write
    /// permission on the file or ownership of it.
    Cachestat,
    /// A shared read mapping of the range, `mincore`, unmapped (2.4). From 5.0 it counts only
    /// pages of a file the caller could write or owns.
    Mincore,
}

impl ResidentPages {
    /// Every form.
    pub const ALL: [ResidentPages; 2] = [ResidentPages::Cachestat, ResidentPages::Mincore];

    /// The best form here.
    /// replaces the libc::mincore map-and-ask of blaze-core fs.rs as the one choice
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn choose(facts: &KernelFacts) -> Result<ResidentPages, Refused> {
        Ok(if facts.cachestat() { ResidentPages::Cachestat } else { ResidentPages::Mincore })
    }

    /// Whether this kernel has the form.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn available(self, facts: &KernelFacts) -> bool {
        match self {
            ResidentPages::Cachestat => facts.cachestat(),
            ResidentPages::Mincore => true,
        }
    }

    /// The resident pages of `file` from byte `at` (a multiple of `page`) for `len` bytes
    /// (more than 0). `held` is the mapping form's room, a byte a page (unused by the other);
    /// too small is [`Errno::Inval`]. `page` is the page size the auxiliary vector says.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn count(self, file: Fd, at: u64, len: NonZeroUsize, page: NonZeroUsize, held: &mut [u8]) -> Result<u64, Errno> {
        if at % page.get() as u64 != 0 {
            return Err(Errno::Inval);
        }
        match self {
            ResidentPages::Cachestat => {
                let range = CachestatRange { off: at, len: len.get() as u64 };
                let mut answer = Cachestat::default();
                // SAFETY: `range` is live and read; `answer` is live and written whole; the
                // kernel keeps neither.
                raw::check(unsafe {
                    raw::syscall4(
                        nr::CACHESTAT,
                        arg(file),
                        core::ptr::from_ref(&range) as usize,
                        core::ptr::from_mut(&mut answer) as usize,
                        0,
                    )
                })?;
                Ok(answer.nr_cache)
            }
            ResidentPages::Mincore => {
                let pages = len.get().div_ceil(page.get());
                let held = held.get_mut(..pages).ok_or(Errno::Inval)?;
                // SAFETY: a new shared read-only mapping of `file`; nothing else is replaced
                // (no hint), and it is only read by the kernel's `mincore` before the unmap.
                let mapped = unsafe { mmap(None, len.get(), Prot::READ, MapFlags::SHARED, Some(file), at) }?;
                // SAFETY: `mapped` is the start of the `len`-byte mapping just made, page
                // aligned; `held` has a byte for each of its pages.
                let asked = unsafe { mincore(mapped, len.get(), held) };
                // SAFETY: the mapping made above, unmapped once; nothing points into it.
                let gone = unsafe { munmap(mapped, len.get()) };
                asked?;
                gone?;
                Ok(held.iter().filter(|byte| **byte & 1 != 0).count() as u64)
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Seal a mapping.

/// Seal a mapping against unmap, remap and protection change: one form, or a refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seal {
    /// `mseal` (6.10, 64-bit only).
    Mseal,
}

impl Seal {
    /// Every form.
    pub const ALL: [Seal; 1] = [Seal::Mseal];

    /// The form, or [`Refused`] naming `mseal`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn choose(facts: &KernelFacts) -> Result<Seal, Refused> {
        if facts.mseal() { Ok(Seal::Mseal) } else { Err(Refused { slot: Slot::Seal, fact: Fact::Mseal }) }
    }

    /// Whether this kernel has the form.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn available(self, facts: &KernelFacts) -> bool {
        facts.mseal()
    }

    /// Seals `len` bytes from `at` (page aligned), for the life of the process.
    ///
    /// # Safety
    ///
    /// The range is this process's mappings, and nothing will need to unmap, remap or
    /// change the protection of them again: the process keeps them until it ends.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub unsafe fn seal(self, at: NonNull<u8>, len: usize) -> Result<(), Errno> {
        // SAFETY: the kernel changes only the mappings' flags; the caller answers for them.
        raw::check(unsafe { raw::syscall3(nr::MSEAL, at.as_ptr() as usize, len, 0) }).map(|_| ())
    }
}

// ---------------------------------------------------------------------------------------------
// Atomic write.

/// A write that lands whole or not at all (`RWF_ATOMIC`, 6.11), on a file whose atomic unit
/// bounds the caller probed (`linux::atomic_write_bounds`): a fact of the file, not the
/// kernel, so it is chosen from those bounds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AtomicWrite {
    min: u32,
    max: u32,
}

impl AtomicWrite {
    /// The slot for a file whose bounds are `bounds` (`atomic_write_bounds`'s answer);
    /// [`Refused`] naming the atomic write where it has none.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn choose(bounds: Option<(u32, u32)>) -> Result<AtomicWrite, Refused> {
        match bounds {
            Some((min, max)) if min > 0 && min <= max => Ok(AtomicWrite { min, max }),
            _ => Err(Refused { slot: Slot::AtomicWrite, fact: Fact::AtomicWrite }),
        }
    }

    /// The smallest unit.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn min(&self) -> u32 {
        self.min
    }

    /// The largest unit.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn max(&self) -> u32 {
        self.max
    }

    /// Writes `bytes` at `at` whole or not at all: its length a power of two within the
    /// bounds and `at` a multiple of it, else [`Errno::Inval`] before the call.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn write(&self, fd: Fd, bytes: &[u8], at: u64) -> Result<usize, Errno> {
        let len = bytes.len();
        if !len.is_power_of_two() || len < self.min as usize || len > self.max as usize || at % len as u64 != 0 {
            return Err(Errno::Inval);
        }
        pwrite_flagged(fd, bytes, at, RWF_ATOMIC)
    }
}

// ---------------------------------------------------------------------------------------------
// Wait.

/// Proof that this process's futex waiters cannot sit in the global hash while its wakes go to
/// a private one (`probe`'s "ring futex fact"): the private hash is installed, or this kernel
/// makes none. Made only by [`install_private_hash`]; the wait ring's form holds one (rule 74).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PrivateHash {
    _made: (),
}

/// Installs this process's private futex hash where the kernel makes them
/// (`prctl(PR_FUTEX_HASH, PR_FUTEX_HASH_SET_SLOTS, 16, 0)`, 6.16) and it has none yet; a
/// token at once where the facts say `PR_FUTEX_HASH` is absent or denied (only the global hash
/// there: the class cannot happen). Called before a process's first ring wait, in a child too
/// (a child's memory starts with no private hash): the guard that makes a `lost-wake` kernel
/// safe. Idempotent.
/// replaces nothing: new (the guard of `slot::Wait::Ring`)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn install_private_hash(facts: &KernelFacts) -> Result<PrivateHash, Errno> {
    if !facts.futex_hash() {
        return Ok(PrivateHash { _made: () });
    }
    match ensure_private_hash()? {
        HashStep::NoHashes | HashStep::Had | HashStep::Installed => Ok(PrivateHash { _made: () }),
    }
}

/// A form of the [`Wait`] slot, before it is started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaitForm {
    /// The thread's wait ring (blaze-core `wait/ring.rs`, 6.13).
    Ring,
    /// Direct calls (blaze-core `wait/direct.rs`, 5.3).
    Direct,
}

impl WaitForm {
    /// The first fact this form lacks here, in the order its needs are checked: `None` where
    /// the form can be used. The ring's: the ring, its operations (`WAITID` 50, `FUTEX_WAIT`
    /// 51, `FUTEX_WAKE` 52, `POLL_ADD` 6, `TIMEOUT` 11, `ASYNC_CANCEL` 14), the wait ring's setup
    /// flags, the registered wait region, and a ring futex that is `sound` or `lost-wake` (the
    /// latter safe behind [`install_private_hash`]). The direct form's: `pidfd_open`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn missing(self, facts: &KernelFacts) -> Option<Fact> {
        let ring = facts.ring();
        match self {
            WaitForm::Ring if !ring.present() => Some(Fact::Ring),
            WaitForm::Ring
                if !(ring.waitid() && ring.futex() && ring.poll_add() && ring.timeout() && ring.async_cancel()) =>
            {
                Some(Fact::RingOps)
            }
            WaitForm::Ring if !ring.wait_ring_flags() => Some(Fact::RingWaitFlags),
            WaitForm::Ring if !ring.mem_region() => Some(Fact::RingMemRegion),
            WaitForm::Ring => match facts.ring_futex() {
                RingFutex::Sound | RingFutex::LostWake => None,
                RingFutex::Absent | RingFutex::Denied => Some(Fact::RingFutex),
            },
            WaitForm::Direct if !facts.pidfd_open() => Some(Fact::PidfdOpen),
            WaitForm::Direct => None,
        }
    }

    /// Whether this kernel has the form.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn available(self, facts: &KernelFacts) -> bool {
        self.missing(facts).is_none()
    }
}

/// How a thread blocks: on a word, a child's exit, a descriptor, a time. Chosen once per
/// process from [`KernelFacts`] (the top-level process's probe, or a child's parent's facts
/// from its start message), never from an environment mark (rule 75).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    /// The thread's wait ring, started behind the private hash guard.
    Ring(PrivateHash),
    /// Direct calls.
    Direct,
}

/// Why a [`Wait`] was not started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaitStartError {
    /// The form asked for (or, from [`Wait::choose`], the last one) is not here: the fact it
    /// lacks, and whether that fact was absent or denied.
    Refused {
        /// The slot and the fact.
        refused: Refused,
        /// [`Presence::Absent`] or [`Presence::Denied`].
        presence: Presence,
    },
    /// Installing the private futex hash failed (`prctl(PR_FUTEX_HASH, SET_SLOTS)`): the errno.
    PrivateHash(Errno),
}

impl core::fmt::Display for WaitStartError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WaitStartError::Refused { refused, presence: Presence::Denied } => write!(
                f,
                "wait: no form on this kernel: {} is denied here (a seccomp profile or a sysctl), by `{}`",
                refused.fact.name(),
                refused.fact.probe()
            ),
            WaitStartError::Refused { refused, .. } => write!(f, "{refused}"),
            WaitStartError::PrivateHash(errno) => write!(
                f,
                "wait: the ring form's guard, prctl(PR_FUTEX_HASH, PR_FUTEX_HASH_SET_SLOTS, 16, 0), answered {errno}"
            ),
        }
    }
}

impl core::error::Error for WaitStartError {}

impl Wait {
    /// Every form, best first.
    pub const ALL: [WaitForm; 2] = [WaitForm::Ring, WaitForm::Direct];

    /// The best form here, started: the ring where [`WaitForm::Ring`] is available, else the
    /// direct calls, else [`WaitStartError::Refused`] naming `pidfd_open`. A choice from facts,
    /// not a fallback: no form is tried and then left.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn choose(facts: &KernelFacts) -> Result<Wait, WaitStartError> {
        let form = if WaitForm::Ring.available(facts) { WaitForm::Ring } else { WaitForm::Direct };
        Wait::start(form, facts)
    }

    /// `form`, started (the ring's guard installed): [`WaitStartError::Refused`] where this
    /// kernel lacks it (the conformance tests start each form this way).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn start(form: WaitForm, facts: &KernelFacts) -> Result<Wait, WaitStartError> {
        if let Some(fact) = form.missing(facts) {
            let presence = match facts.presence(fact) {
                Presence::Denied => Presence::Denied,
                Presence::Absent | Presence::Present => Presence::Absent,
            };
            return Err(WaitStartError::Refused { refused: Refused { slot: Slot::Wait, fact }, presence });
        }
        match form {
            WaitForm::Ring => install_private_hash(facts).map(Wait::Ring).map_err(WaitStartError::PrivateHash),
            WaitForm::Direct => Ok(Wait::Direct),
        }
    }

    /// The form started.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn form(&self) -> WaitForm {
        match self {
            Wait::Ring(_) => WaitForm::Ring,
            Wait::Direct => WaitForm::Direct,
        }
    }
}
