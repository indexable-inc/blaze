//! Signals: the actions, the alternate stack, the masks and sets, `raise`, `kill`, the exits,
//! macOS's `errno`, and the `#[repr(C)]` types a handler is given (`siginfo_t`, `ucontext_t`
//! and its machine context per system and architecture), with the constants. Each item says
//! what it replaces (`/// replaces libc::sigaction`).
//!
//! # Linux: the kernel's calls and types, no C library
//!
//! On Linux every function here is a raw system call (`crate::linux::signal`:
//! `rt_sigaction`, `sigaltstack`, `tgkill`; `crate::linux::rt_sigprocmask`) and the types are
//! the kernel's, because a raw call takes the kernel's layout and a second, C-shaped copy would
//! be converted on every call and could disagree with it:
//!
//! - [`sigset_t`] is the kernel's 8-byte set, `crate::linux::SigSet` (glibc's is 128 bytes for
//!   1024 signals no kernel has; the kernel refuses that size).
//! - [`sigaction`] is the kernel's `struct sigaction` (handler, flags, restorer, mask: 32
//!   bytes; glibc's is 152 in another order), `crate::linux::signal::SigAction`. Its restorer
//!   is blaze-sys's own, put in by [`set_action`] (x86-64 requires one).
//! - [`stack_t`] is the kernel's (the same as glibc's), `crate::linux::signal::Stack`.
//! - [`siginfo_t`] and [`sigevent`] are the same bytes in the kernel and glibc (128 and 64);
//!   they stay declared here, from the kernel's headers.
//! - [`ucontext_t`] is the kernel's: on aarch64 the 8-byte mask and the kernel's 120-byte pad
//!   (the same offsets as glibc's); on x86-64 the kernel's 304 bytes ending at the mask (glibc
//!   adds an FP area of its own after it that the kernel's frame does not have there).
//!
//! What the C library did that is kept: glibc's own two signals (`SIGCANCEL` 32, `SIGSETXID`
//! 33; [`SIGRTMIN`](sigrtmin) is 34) are refused by [`set_action`] and [`add_to_set`], left
//! out of [`full_set`] and taken out of a mask by [`set_mask`], as glibc's `sigaction`,
//! `sigaddset`, `sigfillset` and `sigprocmask` do, because a blaze process may have glibc
//! loaded (Rust std, a placed program) and must not break its threads. What changes for a
//! handler that chains to an action glibc installed, and why it still works, is in
//! `crate::linux::signal`'s module doc ("Two libraries in one process").
//!
//! There is no `errno` on Linux: a raw call returns its error. `errno`/`set_errno` and the
//! C-shaped declarations (`sigaction`, `sigaltstack`, `sigemptyset`, `sigfillset`,
//! `sigaddset`, `sigprocmask`, `raise`, `signal`) are macOS-only; a Linux caller uses the
//! typed wrappers.
//!
//! # macOS: libSystem
//!
//! On macOS, `kill` and `exit` are entries of `crate::macos::cold` (defined in this crate under
//! their C names, found on first call); this file calls them there and declares neither. A cold
//! entry's first call runs `dlsym`, which is not async-signal-safe: [`kill`] and [`exit`] say so.
//! Every other function here is a plain libSystem import, linked as every macOS program links
//! it.
//!
//! Sources (named again on each type): Linux's include/uapi/asm-generic/signal.h,
//! signal-defs.h and siginfo.h, arch/x86/include/uapi/asm/{signal,sigcontext,ucontext}.h,
//! arch/arm64/include/uapi/asm/{sigcontext,ucontext}.h; macOS's `sys/signal.h`,
//! `sys/_types/_sigaltstack.h`, `sys/_types/_ucontext.h`, `arm/_mcontext.h` and
//! `mach/arm/_structs.h`. The `tests` below compare every layout and constant with the `libc`
//! crate's (0.2.191), field by field where Linux's kernel type and glibc's differ in size.

#![allow(non_camel_case_types, reason = "the C library's own names, so a port from `libc::` changes only the path")]

#[cfg(target_os = "macos")]
use core::ffi::c_long;
use core::ffi::{c_int, c_ulong, c_void};

use super::types::{pid_t, sighandler_t, size_t, uid_t};
use crate::Errno;

// ---------------------------------------------------------------------------------------------
// Signal numbers (Linux: asm-generic/signal.h; macOS: sys/signal.h)
// ---------------------------------------------------------------------------------------------

/// `SIGHUP`: the terminal hung up. 1 on both.
/// replaces libc::SIGHUP
pub const SIGHUP: c_int = 1;
/// `SIGINT`: an interrupt from the terminal. 2 on both.
/// replaces libc::SIGINT
pub const SIGINT: c_int = 2;
/// `SIGQUIT`: a quit from the terminal. 3 on both.
/// replaces libc::SIGQUIT
pub const SIGQUIT: c_int = 3;
/// `SIGILL`: an illegal instruction. 4 on both.
/// replaces libc::SIGILL
pub const SIGILL: c_int = 4;
/// `SIGTRAP`: a trap (`brk`, `int3`). 5 on both.
/// replaces libc::SIGTRAP
pub const SIGTRAP: c_int = 5;
/// `SIGABRT`: `abort`. 6 on both.
/// replaces libc::SIGABRT
pub const SIGABRT: c_int = 6;
/// `SIGFPE`: an arithmetic fault. 8 on both.
/// replaces libc::SIGFPE
pub const SIGFPE: c_int = 8;
/// `SIGKILL`: cannot be caught or ignored. 9 on both.
/// replaces libc::SIGKILL
pub const SIGKILL: c_int = 9;
/// `SIGSEGV`: a bad memory access. 11 on both.
/// replaces libc::SIGSEGV
pub const SIGSEGV: c_int = 11;
/// `SIGPIPE`: a write to a pipe or socket with no reader. 13 on both.
/// replaces libc::SIGPIPE
pub const SIGPIPE: c_int = 13;
/// `SIGALRM`: a real-time timer expired. 14 on both.
/// replaces libc::SIGALRM
pub const SIGALRM: c_int = 14;
/// `SIGTERM`: a request to end. 15 on both.
/// replaces libc::SIGTERM
pub const SIGTERM: c_int = 15;
/// `SIGPROF`: a profiling timer expired. 27 on both.
/// replaces libc::SIGPROF
pub const SIGPROF: c_int = 27;
/// `SIGWINCH`: the terminal's size changed. 28 on both.
/// replaces libc::SIGWINCH
pub const SIGWINCH: c_int = 28;

/// `SIGBUS`: a bus error (a mapped file's page gone, an unaligned access). Linux 7.
/// replaces libc::SIGBUS
#[cfg(target_os = "linux")]
pub const SIGBUS: c_int = 7;
/// `SIGBUS`: macOS 10.
/// replaces libc::SIGBUS
#[cfg(target_os = "macos")]
pub const SIGBUS: c_int = 10;

/// `SIGUSR1`: the first user signal. Linux 10.
/// replaces libc::SIGUSR1
#[cfg(target_os = "linux")]
pub const SIGUSR1: c_int = 10;
/// `SIGUSR1`: macOS 30.
/// replaces libc::SIGUSR1
#[cfg(target_os = "macos")]
pub const SIGUSR1: c_int = 30;

/// `SIGUSR2`: the second user signal. Linux 12.
/// replaces libc::SIGUSR2
#[cfg(target_os = "linux")]
pub const SIGUSR2: c_int = 12;
/// `SIGUSR2`: macOS 31.
/// replaces libc::SIGUSR2
#[cfg(target_os = "macos")]
pub const SIGUSR2: c_int = 31;

/// `SIGCHLD`: a child stopped or ended. Linux 17.
/// replaces libc::SIGCHLD
#[cfg(target_os = "linux")]
pub const SIGCHLD: c_int = 17;
/// `SIGCHLD`: macOS 20.
/// replaces libc::SIGCHLD
#[cfg(target_os = "macos")]
pub const SIGCHLD: c_int = 20;

/// `SIGCONT`: continue a stopped process. Linux 18.
/// replaces libc::SIGCONT
#[cfg(target_os = "linux")]
pub const SIGCONT: c_int = 18;
/// `SIGCONT`: macOS 19.
/// replaces libc::SIGCONT
#[cfg(target_os = "macos")]
pub const SIGCONT: c_int = 19;

/// `SIGSTOP`: stop; cannot be caught or ignored. Linux 19.
/// replaces libc::SIGSTOP
#[cfg(target_os = "linux")]
pub const SIGSTOP: c_int = 19;
/// `SIGSTOP`: macOS 17.
/// replaces libc::SIGSTOP
#[cfg(target_os = "macos")]
pub const SIGSTOP: c_int = 17;

/// `SIGSYS`: a bad system call (a seccomp filter's trap). Linux 31.
/// replaces libc::SIGSYS
#[cfg(target_os = "linux")]
pub const SIGSYS: c_int = 31;
/// `SIGSYS`: macOS 12.
/// replaces libc::SIGSYS
#[cfg(target_os = "macos")]
pub const SIGSYS: c_int = 12;

/// `SIGINFO`: a status request from the terminal (ctrl-T). macOS only: 29.
/// replaces libc::SIGINFO
#[cfg(target_os = "macos")]
pub const SIGINFO: c_int = 29;

// ---------------------------------------------------------------------------------------------
// Dispositions, mask operations, action flags, the alternate stack
// ---------------------------------------------------------------------------------------------

/// `SIG_DFL`: the signal's default action, as a handler word. 0 on both.
/// replaces libc::SIG_DFL
pub const SIG_DFL: sighandler_t = 0;
/// `SIG_IGN`: the signal is ignored, as a handler word. 1 on both.
/// replaces libc::SIG_IGN
pub const SIG_IGN: sighandler_t = 1;
/// `SIG_ERR`: what `signal` returns when it fails. All ones (-1) on both.
/// replaces libc::SIG_ERR
pub const SIG_ERR: sighandler_t = !0;

/// `SIG_BLOCK`: `sigprocmask` adds the set's signals to the mask. Linux 0.
/// replaces libc::SIG_BLOCK
#[cfg(target_os = "linux")]
pub const SIG_BLOCK: c_int = 0;
/// `SIG_BLOCK`: macOS 1.
/// replaces libc::SIG_BLOCK
#[cfg(target_os = "macos")]
pub const SIG_BLOCK: c_int = 1;

/// `SIG_UNBLOCK`: `sigprocmask` removes the set's signals from the mask. Linux 1.
/// replaces libc::SIG_UNBLOCK
#[cfg(target_os = "linux")]
pub const SIG_UNBLOCK: c_int = 1;
/// `SIG_UNBLOCK`: macOS 2.
/// replaces libc::SIG_UNBLOCK
#[cfg(target_os = "macos")]
pub const SIG_UNBLOCK: c_int = 2;

/// `SIG_SETMASK`: `sigprocmask` makes the set the mask. Linux 2.
/// replaces libc::SIG_SETMASK
#[cfg(target_os = "linux")]
pub const SIG_SETMASK: c_int = 2;
/// `SIG_SETMASK`: macOS 3.
/// replaces libc::SIG_SETMASK
#[cfg(target_os = "macos")]
pub const SIG_SETMASK: c_int = 3;

/// `SA_NOCLDSTOP`: no `SIGCHLD` when a child stops. Linux 1 (bits/sigaction.h).
/// replaces libc::SA_NOCLDSTOP
#[cfg(target_os = "linux")]
pub const SA_NOCLDSTOP: c_int = 0x1;
/// `SA_SIGINFO`: the handler takes `(signal, siginfo_t *, ucontext_t *)`. Linux 4.
/// replaces libc::SA_SIGINFO
#[cfg(target_os = "linux")]
pub const SA_SIGINFO: c_int = 0x4;
/// `SA_ONSTACK`: the handler runs on the thread's alternate stack. Linux 0x0800_0000.
/// replaces libc::SA_ONSTACK
#[cfg(target_os = "linux")]
pub const SA_ONSTACK: c_int = 0x0800_0000;
/// `SA_RESTART`: a call the signal interrupted is restarted. Linux 0x1000_0000.
/// replaces libc::SA_RESTART
#[cfg(target_os = "linux")]
pub const SA_RESTART: c_int = 0x1000_0000;
/// `SA_NODEFER`: the signal is not blocked while its handler runs. Linux 0x4000_0000.
/// replaces libc::SA_NODEFER
#[cfg(target_os = "linux")]
pub const SA_NODEFER: c_int = 0x4000_0000;
/// `SA_RESETHAND`: the action goes back to `SIG_DFL` once the handler is entered. Linux
/// 0x8000_0000, the sign bit of the `int`.
/// replaces libc::SA_RESETHAND
#[cfg(target_os = "linux")]
pub const SA_RESETHAND: c_int = 0x8000_0000_u32.cast_signed();

/// `SA_ONSTACK`: macOS 1 (sys/signal.h).
/// replaces libc::SA_ONSTACK
#[cfg(target_os = "macos")]
pub const SA_ONSTACK: c_int = 0x1;
/// `SA_RESTART`: macOS 2.
/// replaces libc::SA_RESTART
#[cfg(target_os = "macos")]
pub const SA_RESTART: c_int = 0x2;
/// `SA_RESETHAND`: macOS 4.
/// replaces libc::SA_RESETHAND
#[cfg(target_os = "macos")]
pub const SA_RESETHAND: c_int = 0x4;
/// `SA_NOCLDSTOP`: macOS 8.
/// replaces libc::SA_NOCLDSTOP
#[cfg(target_os = "macos")]
pub const SA_NOCLDSTOP: c_int = 0x8;
/// `SA_NODEFER`: macOS 0x10.
/// replaces libc::SA_NODEFER
#[cfg(target_os = "macos")]
pub const SA_NODEFER: c_int = 0x10;
/// `SA_SIGINFO`: macOS 0x40.
/// replaces libc::SA_SIGINFO
#[cfg(target_os = "macos")]
pub const SA_SIGINFO: c_int = 0x40;

/// `SS_DISABLE`: in a `stack_t`'s `ss_flags`, the thread has no alternate stack. Linux 2, the
/// kernel's `crate::linux::signal::SS_DISABLE`.
/// replaces libc::SS_DISABLE
#[cfg(target_os = "linux")]
pub const SS_DISABLE: c_int = crate::linux::signal::SS_DISABLE;
/// `SS_DISABLE`: macOS 4.
/// replaces libc::SS_DISABLE
#[cfg(target_os = "macos")]
pub const SS_DISABLE: c_int = 4;

/// `SIGSTKSZ`: the size an alternate stack is advised to have. Linux aarch64: 16 KiB (the
/// kernel's arch/arm64/include/uapi/asm/signal.h, as glibc's `bits/sigstack.h` constant; not
/// glibc 2.34's `sysconf(_SC_SIGSTKSZ)` form).
/// replaces libc::SIGSTKSZ
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub const SIGSTKSZ: size_t = 16384;
/// `SIGSTKSZ`: Linux x86-64, 8 KiB (arch/x86/include/uapi/asm/signal.h).
/// replaces libc::SIGSTKSZ
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const SIGSTKSZ: size_t = 8192;
/// `SIGSTKSZ`: macOS, 128 KiB.
/// replaces libc::SIGSTKSZ
#[cfg(target_os = "macos")]
pub const SIGSTKSZ: size_t = 131_072;

/// `SIGEV_THREAD_ID`: a `sigevent` whose signal goes to the thread `sigev_notify_thread_id`.
/// Linux only: 4.
/// replaces libc::SIGEV_THREAD_ID
#[cfg(target_os = "linux")]
pub const SIGEV_THREAD_ID: c_int = 4;

// ---------------------------------------------------------------------------------------------
// The sets
// ---------------------------------------------------------------------------------------------

/// The kernel's signal set on Linux: 64 bits, bit `n - 1` for signal `n`
/// (`crate::linux::SigSet`; include/uapi/asm-generic/signal.h), not glibc's 1024-bit one. The
/// module doc says why. All zero is the empty set.
/// replaces libc::sigset_t
#[cfg(target_os = "linux")]
pub type sigset_t = crate::linux::SigSet;

/// The C library's signal set: one 32-bit word on macOS (`__darwin_sigset_t`), bit `n - 1`
/// for signal `n`.
/// replaces libc::sigset_t
#[cfg(target_os = "macos")]
pub type sigset_t = u32;

/// The empty set, as `sigemptyset` leaves one (all bits clear on both systems): what a mask
/// starts as without a call.
/// replaces libc::sigemptyset (for a set that starts empty)
#[cfg(target_os = "linux")]
pub const SIGSET_EMPTY: sigset_t = crate::linux::SigSet::EMPTY;
/// The empty set (macOS).
/// replaces libc::sigemptyset (for a set that starts empty)
#[cfg(target_os = "macos")]
pub const SIGSET_EMPTY: sigset_t = 0;

// The kernel's `sigset_t` (include/uapi/asm-generic/signal.h, `_NSIG` 64), Linux aarch64 and
// x86-64.
#[cfg(target_os = "linux")]
const _: () = {
    assert!(size_of::<sigset_t>() == 8);
    assert!(align_of::<sigset_t>() == 8);
};

/// glibc's own signals, `SIGCANCEL` (32) and `SIGSETXID` (33), as set bits: refused, left out
/// and taken out by this file's functions as glibc's are (the module doc).
#[cfg(target_os = "linux")]
const C_LIBRARY_SIGNALS: u64 = (1 << 31) | (1 << 32);

// ---------------------------------------------------------------------------------------------
// `struct sigaction`
// ---------------------------------------------------------------------------------------------

/// The kernel's `struct sigaction` on Linux (`crate::linux::signal::SigAction`, whose doc gives
/// its layout): `sa_sigaction`, `sa_flags` (with `sa_flags_high`, 0), `sa_restorer` (left
/// `None`: [`set_action`] puts blaze-sys's own in) and `sa_mask`. Not glibc's: the module doc
/// says why. `sigaction::DEFAULT` is `SigAction::DEFAULT`.
/// replaces libc::sigaction (the struct)
#[cfg(target_os = "linux")]
pub type sigaction = crate::linux::signal::SigAction;

/// The C library's `struct sigaction` on macOS (sys/signal.h): `__sigaction_u` (the handler
/// word), the mask and the flags. libSystem's `sigaction` adds the trampoline itself.
/// replaces libc::sigaction (the struct)
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct sigaction {
    /// `SIG_DFL`, `SIG_IGN`, or a handler's address (`__sigaction_u`).
    pub sa_sigaction: sighandler_t,
    /// The signals blocked while the handler runs.
    pub sa_mask: sigset_t,
    /// `SA_*`.
    pub sa_flags: c_int,
}

#[cfg(target_os = "macos")]
impl sigaction {
    /// `SIG_DFL`, an empty mask, no flags: what an all-zero `sigaction` is, so a value to
    /// fill or to be written into needs no `mem::zeroed`. (Linux's is `SigAction::DEFAULT`.)
    /// replaces core::mem::zeroed::<libc::sigaction>()
    pub const DEFAULT: sigaction = sigaction { sa_sigaction: SIG_DFL, sa_mask: SIGSET_EMPTY, sa_flags: 0 };
}

// The kernel's `struct sigaction` (include/uapi/asm-generic/signal.h), Linux aarch64 and
// x86-64: asserted again here at the offsets a port from glibc's reads.
#[cfg(target_os = "linux")]
const _: () = {
    assert!(size_of::<sigaction>() == 32);
    assert!(align_of::<sigaction>() == 8);
    assert!(core::mem::offset_of!(sigaction, sa_sigaction) == 0);
    assert!(core::mem::offset_of!(sigaction, sa_flags) == 8);
    assert!(core::mem::offset_of!(sigaction, sa_restorer) == 16);
    assert!(core::mem::offset_of!(sigaction, sa_mask) == 24);
};

// macOS's `struct sigaction` (sys/signal.h), arm64.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<sigaction>() == 16);
    assert!(align_of::<sigaction>() == 8);
    assert!(core::mem::offset_of!(sigaction, sa_sigaction) == 0);
    assert!(core::mem::offset_of!(sigaction, sa_mask) == 8);
    assert!(core::mem::offset_of!(sigaction, sa_flags) == 12);
};

// ---------------------------------------------------------------------------------------------
// `stack_t`
// ---------------------------------------------------------------------------------------------

/// An alternate signal stack on Linux: the kernel's `stack_t` (`crate::linux::signal::Stack`;
/// glibc's is the same), its base, flags (`SS_DISABLE`, `SS_ONSTACK`) and size, in that order.
/// `stack_t::DISABLED` is `Stack::DISABLED`.
/// replaces libc::stack_t
#[cfg(target_os = "linux")]
pub type stack_t = crate::linux::signal::Stack;

/// An alternate signal stack on macOS (sys/_types/_sigaltstack.h): base, size, then flags.
/// replaces libc::stack_t
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct stack_t {
    /// The stack's lowest address.
    pub ss_sp: *mut c_void,
    /// Its size in bytes.
    pub ss_size: size_t,
    /// `SS_*`.
    pub ss_flags: c_int,
}

#[cfg(target_os = "macos")]
impl stack_t {
    /// No stack (`SS_DISABLE`, null, 0): what [`alt_stack`] starts from, and what turns a
    /// thread's alternate stack off when given to [`set_alt_stack`]. (Linux's is
    /// `Stack::DISABLED`.)
    /// replaces core::mem::zeroed::<libc::stack_t>()
    pub const DISABLED: stack_t = stack_t { ss_sp: core::ptr::null_mut(), ss_flags: SS_DISABLE, ss_size: 0 };
}

// The kernel's `stack_t` (include/uapi/asm-generic/signal.h), Linux aarch64 and x86-64.
#[cfg(target_os = "linux")]
const _: () = {
    assert!(size_of::<stack_t>() == 24);
    assert!(align_of::<stack_t>() == 8);
    assert!(core::mem::offset_of!(stack_t, ss_sp) == 0);
    assert!(core::mem::offset_of!(stack_t, ss_flags) == 8);
    assert!(core::mem::offset_of!(stack_t, ss_size) == 16);
};

// macOS's `stack_t` (sys/_types/_sigaltstack.h), arm64.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<stack_t>() == 24);
    assert!(align_of::<stack_t>() == 8);
    assert!(core::mem::offset_of!(stack_t, ss_sp) == 0);
    assert!(core::mem::offset_of!(stack_t, ss_size) == 8);
    assert!(core::mem::offset_of!(stack_t, ss_flags) == 16);
};

// ---------------------------------------------------------------------------------------------
// `union sigval`, `siginfo_t`, `struct sigevent`
// ---------------------------------------------------------------------------------------------

/// `union sigval` (an `int` or a pointer), declared as its pointer member, as the `libc` crate
/// declares it: the `int` member is the low four bytes of the same word on these
/// little-endian targets. The value a timer's or a queued signal's `siginfo_t` carries.
/// replaces libc::sigval
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct sigval {
    /// The value, as a pointer; `.addr()` for a number carried in it.
    pub sival_ptr: *mut c_void,
}

// `union sigval` (glibc's bits/types/__sigval_t.h; macOS's sys/signal.h): one word.
const _: () = {
    assert!(size_of::<sigval>() == 8);
    assert!(align_of::<sigval>() == 8);
};

/// The union of a Linux `siginfo_t` (`_sifields`): 112 bytes on a 64-bit target, read as
/// `int`s (`_kill`/`_sigchld`: pid, uid, status) or as words (`_sigfault`: the address;
/// `_rt`/`_timer`: the `sigval` in the second word).
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
#[repr(C)]
union SigFields {
    ints: [c_int; 28],
    words: [*mut c_void; 14],
}

/// What a `SA_SIGINFO` handler and `waitid` are given: the kernel's `siginfo_t`
/// (include/uapi/asm-generic/siginfo.h, 128 bytes; glibc's bits/types/siginfo_t.h is the same
/// bytes): the signal, an errno, a code, then a union read by the accessors, which are
/// `unsafe` because only `si_code` says which member is live.
/// replaces libc::siginfo_t
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct siginfo_t {
    /// The signal.
    pub si_signo: c_int,
    /// An errno, mostly 0.
    pub si_errno: c_int,
    /// Why it was sent (`SI_USER`, `SI_TIMER`, `SEGV_MAPERR`, `CLD_EXITED`, ...).
    pub si_code: c_int,
    __pad0: c_int,
    _sifields: SigFields,
}

#[cfg(target_os = "linux")]
impl siginfo_t {
    /// All zero: what a `waitid` writes into, with no `mem::zeroed`.
    /// replaces core::mem::zeroed::<libc::siginfo_t>()
    pub const ZERO: siginfo_t =
        siginfo_t { si_signo: 0, si_errno: 0, si_code: 0, __pad0: 0, _sifields: SigFields { ints: [0; 28] } };

    /// The sender's or the child's pid (offset 16).
    ///
    /// # Safety
    ///
    /// `si_code` says the union holds a pid: a signal a process sent, or a `SIGCHLD`.
    /// replaces libc::siginfo_t::si_pid
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const unsafe fn si_pid(&self) -> pid_t {
        // SAFETY: all 112 bytes are initialized (the value is a copy of one the kernel or `ZERO`
        // wrote), so either view reads initialized bytes; their meaning is the caller's.
        let [pid, ..] = unsafe { self._sifields.ints };
        pid
    }

    /// The sender's or the child's real uid (offset 20).
    ///
    /// # Safety
    ///
    /// As [`siginfo_t::si_pid`].
    /// replaces libc::siginfo_t::si_uid
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const unsafe fn si_uid(&self) -> uid_t {
        // SAFETY: as `si_pid`.
        let [_, uid, ..] = unsafe { self._sifields.ints };
        uid.cast_unsigned()
    }

    /// A `SIGCHLD`'s exit status or signal (offset 24).
    ///
    /// # Safety
    ///
    /// The value is a `SIGCHLD`'s (`si_code` is a `CLD_*`).
    /// replaces libc::siginfo_t::si_status
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const unsafe fn si_status(&self) -> c_int {
        // SAFETY: as `si_pid`.
        let [_, _, status, ..] = unsafe { self._sifields.ints };
        status
    }

    /// A fault's address (offset 16).
    ///
    /// # Safety
    ///
    /// The value is a fault's (`SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE`, `SIGTRAP` from the
    /// kernel) for the answer to mean an address. Any other value's word is read too (the
    /// union is plain integers, every bit pattern valid): for a signal a process sent it is
    /// the sender's pid and uid, which a report that prints the word for either may show.
    /// replaces libc::siginfo_t::si_addr
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const unsafe fn si_addr(&self) -> *mut c_void {
        // SAFETY: as `si_pid`.
        let [addr, ..] = unsafe { self._sifields.words };
        addr
    }

    /// A timer's or a queued signal's value (offset 24).
    ///
    /// # Safety
    ///
    /// `si_code` is `SI_TIMER` or `SI_QUEUE` (or `SI_MESGQ`).
    /// replaces libc::siginfo_t::si_value
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const unsafe fn si_value(&self) -> sigval {
        // SAFETY: as `si_pid`.
        let [_, value, ..] = unsafe { self._sifields.words };
        sigval { sival_ptr: value }
    }
}

/// What a `SA_SIGINFO` handler and `waitid` are given on macOS (sys/signal.h's `__siginfo`):
/// every member is a field. The accessors are kept beside them, `unsafe` as Linux's are, so one
/// call compiles on both.
/// replaces libc::siginfo_t
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct siginfo_t {
    /// The signal.
    pub si_signo: c_int,
    /// An errno, mostly 0.
    pub si_errno: c_int,
    /// Why it was sent.
    pub si_code: c_int,
    /// The sender's or the child's pid.
    pub si_pid: pid_t,
    /// The sender's or the child's real uid.
    pub si_uid: uid_t,
    /// A `SIGCHLD`'s exit status or signal.
    pub si_status: c_int,
    /// A fault's address.
    pub si_addr: *mut c_void,
    /// A queued signal's value.
    pub si_value: sigval,
    /// A `SIGPOLL`'s band.
    pub si_band: c_long,
    /// `__pad[7]`.
    pub __pad: [c_ulong; 7],
}

#[cfg(target_os = "macos")]
impl siginfo_t {
    /// All zero: what a `waitid` writes into, with no `mem::zeroed`.
    /// replaces core::mem::zeroed::<libc::siginfo_t>()
    pub const ZERO: siginfo_t = siginfo_t {
        si_signo: 0,
        si_errno: 0,
        si_code: 0,
        si_pid: 0,
        si_uid: 0,
        si_status: 0,
        si_addr: core::ptr::null_mut(),
        si_value: sigval { sival_ptr: core::ptr::null_mut() },
        si_band: 0,
        __pad: [0; 7],
    };

    /// `si_pid`, as Linux's accessor reads it.
    ///
    /// # Safety
    ///
    /// None on macOS (a field read); `unsafe` so a call is the same on both systems.
    /// replaces libc::siginfo_t::si_pid
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const unsafe fn si_pid(&self) -> pid_t {
        self.si_pid
    }

    /// `si_uid`, as Linux's accessor reads it.
    ///
    /// # Safety
    ///
    /// As [`siginfo_t::si_pid`].
    /// replaces libc::siginfo_t::si_uid
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const unsafe fn si_uid(&self) -> uid_t {
        self.si_uid
    }

    /// `si_status`, as Linux's accessor reads it.
    ///
    /// # Safety
    ///
    /// As [`siginfo_t::si_pid`].
    /// replaces libc::siginfo_t::si_status
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const unsafe fn si_status(&self) -> c_int {
        self.si_status
    }

    /// `si_addr`, as Linux's accessor reads it.
    ///
    /// # Safety
    ///
    /// As [`siginfo_t::si_pid`].
    /// replaces libc::siginfo_t::si_addr
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const unsafe fn si_addr(&self) -> *mut c_void {
        self.si_addr
    }

    /// `si_value`, as Linux's accessor reads it.
    ///
    /// # Safety
    ///
    /// As [`siginfo_t::si_pid`].
    /// replaces libc::siginfo_t::si_value
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const unsafe fn si_value(&self) -> sigval {
        self.si_value
    }
}

// The kernel's `siginfo_t` (include/uapi/asm-generic/siginfo.h; glibc's the same), Linux
// aarch64 and x86-64: the union at 16,
// `si_pid` and `si_addr` at 16, `si_uid` at 20, `si_status` and `si_value` at 24.
#[cfg(target_os = "linux")]
const _: () = {
    assert!(size_of::<siginfo_t>() == 128);
    assert!(align_of::<siginfo_t>() == 8);
    assert!(core::mem::offset_of!(siginfo_t, si_signo) == 0);
    assert!(core::mem::offset_of!(siginfo_t, si_errno) == 4);
    assert!(core::mem::offset_of!(siginfo_t, si_code) == 8);
    assert!(core::mem::offset_of!(siginfo_t, _sifields) == 16);
    assert!(size_of::<SigFields>() == 112);
    assert!(align_of::<SigFields>() == 8);
};

// macOS's `siginfo_t` (sys/signal.h), arm64.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<siginfo_t>() == 104);
    assert!(align_of::<siginfo_t>() == 8);
    assert!(core::mem::offset_of!(siginfo_t, si_signo) == 0);
    assert!(core::mem::offset_of!(siginfo_t, si_errno) == 4);
    assert!(core::mem::offset_of!(siginfo_t, si_code) == 8);
    assert!(core::mem::offset_of!(siginfo_t, si_pid) == 12);
    assert!(core::mem::offset_of!(siginfo_t, si_uid) == 16);
    assert!(core::mem::offset_of!(siginfo_t, si_status) == 20);
    assert!(core::mem::offset_of!(siginfo_t, si_addr) == 24);
    assert!(core::mem::offset_of!(siginfo_t, si_value) == 32);
    assert!(core::mem::offset_of!(siginfo_t, si_band) == 40);
};

/// `struct sigevent` (the kernel's include/uapi/asm-generic/siginfo.h; glibc's
/// bits/types/struct_sigevent.h is the same 64 bytes), with its union declared as the
/// `SIGEV_THREAD_ID` member, `sigev_notify_thread_id`, as the `libc` crate declares it: what
/// `timer_create` takes. Linux only (blaze's one use is the Linux sampler; the same request
/// made by raw system call is `blaze_sys::linux::sample::timer_create_thread_cpu`).
/// replaces libc::sigevent
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct sigevent {
    /// The value the signal's `siginfo_t` carries.
    pub sigev_value: sigval,
    /// The signal.
    pub sigev_signo: c_int,
    /// `SIGEV_*`.
    pub sigev_notify: c_int,
    /// The thread (a `gettid`) of `SIGEV_THREAD_ID`.
    pub sigev_notify_thread_id: c_int,
    /// The rest of the union: zero.
    pub __unused1: [c_int; 11],
}

#[cfg(target_os = "linux")]
impl sigevent {
    /// All zero: what a request is built from, with no `mem::zeroed`.
    /// replaces core::mem::zeroed::<libc::sigevent>()
    pub const ZERO: sigevent = sigevent {
        sigev_value: sigval { sival_ptr: core::ptr::null_mut() },
        sigev_signo: 0,
        sigev_notify: 0,
        sigev_notify_thread_id: 0,
        __unused1: [0; 11],
    };
}

// The kernel's `struct sigevent` (include/uapi/asm-generic/siginfo.h), Linux aarch64 and x86-64.
#[cfg(target_os = "linux")]
const _: () = {
    assert!(size_of::<sigevent>() == 64);
    assert!(align_of::<sigevent>() == 8);
    assert!(core::mem::offset_of!(sigevent, sigev_value) == 0);
    assert!(core::mem::offset_of!(sigevent, sigev_signo) == 8);
    assert!(core::mem::offset_of!(sigevent, sigev_notify) == 12);
    assert!(core::mem::offset_of!(sigevent, sigev_notify_thread_id) == 16);
};

// ---------------------------------------------------------------------------------------------
// `ucontext_t` and the machine context: Linux aarch64
// ---------------------------------------------------------------------------------------------

/// Linux aarch64's machine context (glibc's sys/ucontext.h, the kernel's `struct
/// sigcontext`): the fault address, x0 to x30, sp, pc and pstate, inline in the `ucontext_t`.
/// replaces libc::mcontext_t
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct mcontext_t {
    /// The faulting address of a data abort.
    pub fault_address: u64,
    /// x0 to x30 (x29 the frame pointer, x30 the link register).
    pub regs: [u64; 31],
    /// The stack pointer.
    pub sp: u64,
    /// The program counter.
    pub pc: u64,
    /// PSTATE.
    pub pstate: u64,
    /// The pad to `__reserved`'s 16-byte alignment.
    __pad: u64,
    /// The FP/SIMD and other records (4096 bytes at 288, aligned 16), which blaze does not read.
    __reserved: [u8; 4096],
}

/// Linux aarch64's `ucontext_t`: the kernel's `struct ucontext`
/// (arch/arm64/include/uapi/asm/ucontext.h): the kernel's 8-byte mask, then its pad to glibc's
/// 1024 bits (`__unused`), then the machine context inline. The same offsets as glibc's.
/// replaces libc::ucontext_t
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ucontext_t {
    /// `uc_flags`.
    pub uc_flags: c_ulong,
    /// The context to resume when this one returns (unused by signals).
    pub uc_link: *mut ucontext_t,
    /// The alternate stack in force.
    pub uc_stack: stack_t,
    /// The mask the interrupted code had, put back on return.
    pub uc_sigmask: sigset_t,
    /// The kernel's pad: `1024 / 8 - sizeof(sigset_t)` bytes.
    __unused: [u8; 120],
    /// The interrupted registers, put back on return.
    pub uc_mcontext: mcontext_t,
}

// The kernel's `struct sigcontext` and `struct ucontext` (arch/arm64/include/uapi/asm/
// {sigcontext,ucontext}.h; glibc's sys/ucontext.h the same), Linux aarch64: `uc_mcontext` at 176,
// so x29 at 416, sp at 432, pc at 440 of the context (as `blaze_sys::linux::sample` reads).
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const _: () = {
    assert!(size_of::<mcontext_t>() == 4384);
    assert!(align_of::<mcontext_t>() == 16);
    assert!(core::mem::offset_of!(mcontext_t, fault_address) == 0);
    assert!(core::mem::offset_of!(mcontext_t, regs) == 8);
    assert!(core::mem::offset_of!(mcontext_t, sp) == 256);
    assert!(core::mem::offset_of!(mcontext_t, pc) == 264);
    assert!(core::mem::offset_of!(mcontext_t, pstate) == 272);
    assert!(core::mem::offset_of!(mcontext_t, __reserved) == 288);
    assert!(size_of::<ucontext_t>() == 4560);
    assert!(align_of::<ucontext_t>() == 16);
    assert!(core::mem::offset_of!(ucontext_t, uc_flags) == 0);
    assert!(core::mem::offset_of!(ucontext_t, uc_link) == 8);
    assert!(core::mem::offset_of!(ucontext_t, uc_stack) == 16);
    assert!(core::mem::offset_of!(ucontext_t, uc_sigmask) == 40);
    assert!(core::mem::offset_of!(ucontext_t, __unused) == 48);
    assert!(core::mem::offset_of!(ucontext_t, uc_mcontext) == 176);
};

// ---------------------------------------------------------------------------------------------
// `ucontext_t` and the machine context: Linux x86-64
// ---------------------------------------------------------------------------------------------

/// One general register of x86-64's `gregs` (glibc's sys/ucontext.h `greg_t`).
/// replaces libc::greg_t
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub type greg_t = i64;

/// `REG_RBP`: `rbp`'s index in `gregs`, 10 (sys/ucontext.h).
/// replaces libc::REG_RBP
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const REG_RBP: c_int = 10;
/// `REG_RAX`: `rax`'s index in `gregs`, 13; a system call's result on return from a
/// `SIGSYS` handler.
/// replaces libc::REG_RAX
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const REG_RAX: c_int = 13;
/// `REG_RIP`: `rip`'s index in `gregs`, 16.
/// replaces libc::REG_RIP
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const REG_RIP: c_int = 16;

/// Linux x86-64's machine context: the kernel's `struct sigcontext`
/// (arch/x86/include/uapi/asm/sigcontext.h) as glibc's sys/ucontext.h declares it: 23 general
/// registers indexed by `REG_*` (r8 to r15, rdi, rsi, rbp, rbx, rdx, rax, rcx, rsp, rip,
/// eflags, cs/gs/fs/ss, err, trapno, oldmask, cr2: the kernel's order), a pointer to the FP
/// state (declared as `c_void`: blaze does not read it), and reserved words.
/// replaces libc::mcontext_t
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct mcontext_t {
    /// The general registers, by `REG_*`.
    pub gregs: [greg_t; 23],
    /// glibc's `fpregset_t`, a `struct _libc_fpstate *`.
    pub fpregs: *mut c_void,
    __private: [u64; 8],
}

/// Linux x86-64's `ucontext_t`: the kernel's `struct ucontext`
/// (arch/x86/include/uapi/asm/ucontext.h), 304 bytes: the machine context inline, then the
/// kernel's 8-byte mask, last. glibc's declares a 128-byte mask and an FP area and shadow-stack
/// words after it (968 bytes); the kernel's signal frame has no such area there (the FP state
/// is wherever `fpregs` points), so those fields would read the frame's next bytes.
/// replaces libc::ucontext_t
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ucontext_t {
    /// `uc_flags`.
    pub uc_flags: c_ulong,
    /// The context to resume when this one returns (unused by signals).
    pub uc_link: *mut ucontext_t,
    /// The alternate stack in force.
    pub uc_stack: stack_t,
    /// The interrupted registers, put back on return.
    pub uc_mcontext: mcontext_t,
    /// The mask the interrupted code had, put back on return.
    pub uc_sigmask: sigset_t,
}

// The kernel's `struct sigcontext` and `struct ucontext` (arch/x86/include/uapi/asm/
// {sigcontext,ucontext}.h), Linux x86-64: `gregs` at 40, so rbp at 120 and rip at 168 of the
// context (as `blaze_sys::linux::sample` reads); the mask at 296, as in glibc's.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const _: () = {
    assert!(size_of::<mcontext_t>() == 256);
    assert!(align_of::<mcontext_t>() == 8);
    assert!(core::mem::offset_of!(mcontext_t, gregs) == 0);
    assert!(core::mem::offset_of!(mcontext_t, fpregs) == 184);
    assert!(size_of::<ucontext_t>() == 304);
    assert!(align_of::<ucontext_t>() == 8);
    assert!(core::mem::offset_of!(ucontext_t, uc_flags) == 0);
    assert!(core::mem::offset_of!(ucontext_t, uc_link) == 8);
    assert!(core::mem::offset_of!(ucontext_t, uc_stack) == 16);
    assert!(core::mem::offset_of!(ucontext_t, uc_mcontext) == 40);
    assert!(core::mem::offset_of!(ucontext_t, uc_sigmask) == 296);
};

// ---------------------------------------------------------------------------------------------
// `ucontext_t` and the machine context: macOS arm64
// ---------------------------------------------------------------------------------------------

/// arm64's exception state (mach/arm/_structs.h): the fault address and the exception
/// syndrome (`ESR_EL1`; bits 26 to 31 the class, bit 6 a data abort's write).
/// replaces libc::__darwin_arm_exception_state64
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct __darwin_arm_exception_state64 {
    /// `FAR_EL1`.
    pub __far: u64,
    /// `ESR_EL1`.
    pub __esr: u32,
    /// The exception number.
    pub __exception: u32,
}

/// arm64's thread state (mach/arm/_structs.h): x0 to x28, fp, lr, sp, pc, cpsr.
/// replaces libc::__darwin_arm_thread_state64
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct __darwin_arm_thread_state64 {
    /// x0 to x28.
    pub __x: [u64; 29],
    /// x29.
    pub __fp: u64,
    /// x30.
    pub __lr: u64,
    /// The stack pointer.
    pub __sp: u64,
    /// The program counter.
    pub __pc: u64,
    /// CPSR.
    pub __cpsr: u32,
    /// Padding (pointer authentication flags on arm64e).
    pub __pad: u32,
}

/// arm64's NEON state (mach/arm/_structs.h): v0 to v31, FPSR, FPCR; aligned 16.
/// replaces libc::__darwin_arm_neon_state64
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[derive(Clone, Copy, Debug)]
#[repr(C, align(16))]
pub struct __darwin_arm_neon_state64 {
    /// v0 to v31.
    pub __v: [u128; 32],
    /// FPSR.
    pub __fpsr: u32,
    /// FPCR.
    pub __fpcr: u32,
}

/// arm64's machine context (arm/_mcontext.h): exception, thread and NEON state. A
/// `ucontext_t` points at it.
/// replaces libc::__darwin_mcontext64
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct __darwin_mcontext64 {
    /// The exception state.
    pub __es: __darwin_arm_exception_state64,
    /// The general registers, put back on return.
    pub __ss: __darwin_arm_thread_state64,
    /// The NEON registers.
    pub __ns: __darwin_arm_neon_state64,
}

/// macOS's `mcontext_t`: a pointer to the machine context (arm/_mcontext.h).
/// replaces libc::mcontext_t
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub type mcontext_t = *mut __darwin_mcontext64;

/// macOS's `ucontext_t` (sys/_types/_ucontext.h, `__darwin_ucontext`): the machine context is
/// a pointer (`uc_mcontext`), to the state the kernel saved on the signal's stack.
/// replaces libc::ucontext_t
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ucontext_t {
    /// Whether the handler runs on the alternate stack.
    pub uc_onstack: c_int,
    /// The mask the interrupted code had.
    pub uc_sigmask: sigset_t,
    /// The alternate stack in force.
    pub uc_stack: stack_t,
    /// The context to resume when this one returns (unused by signals).
    pub uc_link: *mut ucontext_t,
    /// The size of `*uc_mcontext`.
    pub uc_mcsize: usize,
    /// The interrupted state, put back on return.
    pub uc_mcontext: mcontext_t,
}

// macOS arm64 (mach/arm/_structs.h, arm/_mcontext.h, sys/_types/_ucontext.h): the thread
// state at 16 of the machine context, so pc at 272 and fp at 248 of it; the NEON state at 288.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const _: () = {
    assert!(size_of::<__darwin_arm_exception_state64>() == 16);
    assert!(align_of::<__darwin_arm_exception_state64>() == 8);
    assert!(core::mem::offset_of!(__darwin_arm_exception_state64, __far) == 0);
    assert!(core::mem::offset_of!(__darwin_arm_exception_state64, __esr) == 8);
    assert!(core::mem::offset_of!(__darwin_arm_exception_state64, __exception) == 12);
    assert!(size_of::<__darwin_arm_thread_state64>() == 272);
    assert!(align_of::<__darwin_arm_thread_state64>() == 8);
    assert!(core::mem::offset_of!(__darwin_arm_thread_state64, __x) == 0);
    assert!(core::mem::offset_of!(__darwin_arm_thread_state64, __fp) == 232);
    assert!(core::mem::offset_of!(__darwin_arm_thread_state64, __lr) == 240);
    assert!(core::mem::offset_of!(__darwin_arm_thread_state64, __sp) == 248);
    assert!(core::mem::offset_of!(__darwin_arm_thread_state64, __pc) == 256);
    assert!(core::mem::offset_of!(__darwin_arm_thread_state64, __cpsr) == 264);
    assert!(size_of::<__darwin_arm_neon_state64>() == 528);
    assert!(align_of::<__darwin_arm_neon_state64>() == 16);
    assert!(core::mem::offset_of!(__darwin_arm_neon_state64, __fpsr) == 512);
    assert!(core::mem::offset_of!(__darwin_arm_neon_state64, __fpcr) == 516);
    assert!(size_of::<__darwin_mcontext64>() == 816);
    assert!(align_of::<__darwin_mcontext64>() == 16);
    assert!(core::mem::offset_of!(__darwin_mcontext64, __es) == 0);
    assert!(core::mem::offset_of!(__darwin_mcontext64, __ss) == 16);
    assert!(core::mem::offset_of!(__darwin_mcontext64, __ns) == 288);
    assert!(size_of::<ucontext_t>() == 56);
    assert!(align_of::<ucontext_t>() == 8);
    assert!(core::mem::offset_of!(ucontext_t, uc_onstack) == 0);
    assert!(core::mem::offset_of!(ucontext_t, uc_sigmask) == 4);
    assert!(core::mem::offset_of!(ucontext_t, uc_stack) == 8);
    assert!(core::mem::offset_of!(ucontext_t, uc_link) == 32);
    assert!(core::mem::offset_of!(ucontext_t, uc_mcsize) == 40);
    assert!(core::mem::offset_of!(ucontext_t, uc_mcontext) == 48);
};

// ---------------------------------------------------------------------------------------------
// The C library's functions: macOS only (Linux has the raw calls of `crate::linux::signal`)
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// signal.h: `int sigaction(int sig, const struct sigaction *act, struct sigaction *oact)`.
    /// The action of `sig` read into `oact` (unless null) and set to `act` (unless null); 0, or
    /// -1 with `errno`. Async-signal-safe. macOS only: Linux's is [`get_action`] and
    /// [`set_action`] by `rt_sigaction`.
    /// replaces libc::sigaction
    pub fn sigaction(sig: c_int, act: *const sigaction, oact: *mut sigaction) -> c_int;

    /// signal.h: `int sigaltstack(const stack_t *ss, stack_t *oss)`. The calling thread's
    /// alternate stack read into `oss` and set to `ss` (each unless null). Async-signal-safe.
    /// replaces libc::sigaltstack
    pub fn sigaltstack(ss: *const stack_t, oss: *mut stack_t) -> c_int;

    /// signal.h: `int sigemptyset(sigset_t *set)`. Async-signal-safe.
    /// replaces libc::sigemptyset
    pub fn sigemptyset(set: *mut sigset_t) -> c_int;

    /// signal.h: `int sigfillset(sigset_t *set)`: every signal. Async-signal-safe.
    /// replaces libc::sigfillset
    pub fn sigfillset(set: *mut sigset_t) -> c_int;

    /// signal.h: `int sigaddset(sigset_t *set, int signo)`: -1 with `EINVAL` for a number
    /// that is no signal. Async-signal-safe.
    /// replaces libc::sigaddset
    pub fn sigaddset(set: *mut sigset_t, signo: c_int) -> c_int;

    /// signal.h: `int sigprocmask(int how, const sigset_t *set, sigset_t *oset)`: the calling
    /// thread's mask (libSystem acts on the thread, as `pthread_sigmask`). Async-signal-safe.
    /// replaces libc::sigprocmask
    pub fn sigprocmask(how: c_int, set: *const sigset_t, oset: *mut sigset_t) -> c_int;

    /// signal.h: `int raise(int sig)`: `sig` sent to the calling thread, its handler run before
    /// the return. Async-signal-safe.
    /// replaces libc::raise
    pub fn raise(sig: c_int) -> c_int;

    /// signal.h: `void (*signal(int sig, void (*func)(int)))(int)`, with both handlers as
    /// words: BSD semantics (`SA_RESTART`, the handler kept). [`SIG_ERR`] with `errno` on
    /// failure. macOS only; [`set_handler`] is the typed form on both systems.
    /// replaces libc::signal
    pub fn signal(sig: c_int, func: sighandler_t) -> sighandler_t;
}

/// The C library's functions only the wrappers below call: macOS only.
#[cfg(target_os = "macos")]
mod ffi {
    unsafe extern "C" {
        /// unistd.h: the process ends at once, no `atexit` handler run. Async-signal-safe.
        pub(super) fn _exit(status: core::ffi::c_int) -> !;
    }
}

// ---------------------------------------------------------------------------------------------
// Typed wrappers
// ---------------------------------------------------------------------------------------------

/// The calling thread's `errno`: what a C library call that returned -1 left. Async-signal-safe
/// (one read of a thread's word). macOS only, by `__error`, the declaration in
/// `crate::macos::ffi` (not declared again here: rule 30). Linux has no `errno` here: a raw
/// call returns its error (`crate::linux` wrappers give `Result<_, Errno>`).
/// replaces libc::__error
/// replaces libc::__errno_location (on macOS; on Linux, by the raw call's own result)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn errno() -> Errno {
    let cell = errno_cell();
    // SAFETY: `cell` is the calling thread's `errno`, live and aligned for the thread's life;
    // only this thread writes it.
    let code = unsafe { cell.read() };
    // An errno is a small non-negative number; `from_raw(n).raw() == n` for every `u16`, so a
    // value saved here and put back by `set_errno` is the same bits.
    Errno::from_raw(code as u16)
}

/// The calling thread's `errno` set to `error`: what a C-shaped function returning -1 leaves for
/// its caller, and how a signal handler puts back the `errno` it found. Async-signal-safe.
/// macOS only, as [`errno`].
/// replaces libc::__error
/// replaces libc::__errno_location (on macOS)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_errno(error: Errno) {
    let cell = errno_cell();
    // SAFETY: as `errno`: the calling thread's own live, aligned word.
    unsafe { cell.write(c_int::from(error.raw())) };
}

/// The calling thread's `errno` cell (macOS).
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
fn errno_cell() -> *mut c_int {
    // SAFETY: `__error` takes nothing and returns the calling thread's cell.
    unsafe { crate::macos::ffi::__error() }
}

/// The current action of `signal` (Linux: by `rt_sigaction`, `crate::linux::signal::get_action`).
/// Used by a handler's install (to chain to what was there, or to refuse a signal already
/// taken). [`Errno::Inval`] for a number that is no signal. Async-signal-safe.
/// replaces libc::sigaction (a read: null `act`)
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn get_action(signal: c_int) -> Result<sigaction, Errno> {
    // A negative number is no signal; as a `u32` it is one the kernel refuses (`EINVAL`).
    crate::linux::signal::get_action(signal.cast_unsigned())
}

/// `signal`'s action set to `action`; the action before (Linux: by `rt_sigaction`, with
/// blaze-sys's restorer put in: `crate::linux::signal::set_action`). [`Errno::Inval`] for a
/// number that is no signal, `SIGKILL`, `SIGSTOP`, or glibc's own 32 and 33 (as glibc's
/// `sigaction` refuses them: the module doc). Async-signal-safe.
///
/// # Safety
///
/// A handler `action` names is a function of the signature its flags say (`SA_SIGINFO`: three
/// arguments), lives as long as it may run, and is async-signal-safe; an `SA_ONSTACK` handler
/// runs on the alternate stack of each thread that has one.
/// replaces libc::sigaction (a write)
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn set_action(signal: c_int, action: &sigaction) -> Result<sigaction, Errno> {
    if is_c_library_signal(signal) {
        return Err(Errno::Inval);
    }
    // SAFETY: the handler's contract is the caller's, passed on unchanged.
    unsafe { crate::linux::signal::set_action(signal.cast_unsigned(), action) }
}

/// Whether `signal` is one of glibc's own two (32, 33).
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
const fn is_c_library_signal(signal: c_int) -> bool {
    matches!(signal, 32 | 33)
}

/// The current action of `signal`. Used by a handler's install (to chain to what was there,
/// or to refuse a signal already taken). Async-signal-safe.
/// replaces libc::sigaction (a read: null `act`)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn get_action(signal: c_int) -> Result<sigaction, Errno> {
    let mut old = sigaction::DEFAULT;
    // SAFETY: no new action (null); `old` is a live `sigaction` the call writes and keeps no
    // pointer to.
    match unsafe { sigaction(signal, core::ptr::null(), &raw mut old) } {
        0 => Ok(old),
        _ => Err(errno()),
    }
}

/// `signal`'s action set to `action`; the action before. Async-signal-safe.
///
/// # Safety
///
/// A handler `action` names is a function of the signature its flags say (`SA_SIGINFO`: three
/// arguments), lives as long as it may run, and is async-signal-safe; an `SA_ONSTACK` handler
/// runs on the alternate stack of each thread that has one.
/// replaces libc::sigaction (a write)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn set_action(signal: c_int, action: &sigaction) -> Result<sigaction, Errno> {
    let mut old = sigaction::DEFAULT;
    // SAFETY: `action` and `old` are live `sigaction`s, read and written and not kept; the
    // handler's contract is the caller's.
    match unsafe { sigaction(signal, action, &raw mut old) } {
        0 => Ok(old),
        _ => Err(errno()),
    }
}

/// The calling thread's alternate signal stack ([`stack_t::DISABLED`]'s flag when it has
/// none). Async-signal-safe. Linux by `sigaltstack` (`crate::linux::signal::alt_stack`).
/// replaces libc::sigaltstack (a read: null `ss`)
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn alt_stack() -> Result<stack_t, Errno> {
    crate::linux::signal::alt_stack()
}

/// The calling thread's alternate signal stack set to `stack` ([`stack_t::DISABLED`] turns it
/// off). [`Errno::NoMem`] for a size under the system's minimum, [`Errno::Perm`] while the
/// thread runs on it. Linux by `sigaltstack` (`crate::linux::signal::set_alt_stack`, which
/// also gives the stack before).
///
/// # Safety
///
/// `stack.ss_size` bytes from `stack.ss_sp` are writable memory nothing else uses, for as long
/// as the thread has it as its alternate stack.
/// replaces libc::sigaltstack (a write)
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn set_alt_stack(stack: &stack_t) -> Result<(), Errno> {
    // SAFETY: the memory's contract is the caller's, passed on unchanged.
    unsafe { crate::linux::signal::set_alt_stack(stack) }.map(|_| ())
}

/// The set of every signal a program may block: all 64 but glibc's own two (32, 33), as
/// glibc's `sigfillset` leaves them out (`SIGKILL` and `SIGSTOP` are in it; the kernel ignores
/// them in a mask).
/// replaces libc::sigfillset
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn full_set() -> sigset_t {
    crate::linux::SigSet(!C_LIBRARY_SIGNALS)
}

/// `signal` added to `set`. [`Errno::Inval`] for a number that is no signal (outside 1 to 64)
/// or one of glibc's own two, as glibc's `sigaddset`.
/// replaces libc::sigaddset
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn add_to_set(set: &mut sigset_t, signal: c_int) -> Result<(), Errno> {
    match signal {
        1..=64 if !is_c_library_signal(signal) => {
            *set = set.with(signal.cast_unsigned());
            Ok(())
        }
        _ => Err(Errno::Inval),
    }
}

/// The calling thread's mask changed by `set` as `how` says ([`SIG_BLOCK`], [`SIG_UNBLOCK`],
/// [`SIG_SETMASK`]); the mask before. [`Errno::Inval`] for another `how`. As glibc's
/// `sigprocmask`, glibc's own two signals are taken out of a set that blocks or is the mask.
/// Linux by `rt_sigprocmask` (`crate::linux::rt_sigprocmask`).
/// replaces libc::sigprocmask
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_mask(how: c_int, set: &sigset_t) -> Result<sigset_t, Errno> {
    use crate::linux::{MaskHow, SigSet};
    let (how, set) = match how {
        SIG_BLOCK => (MaskHow::Block, SigSet(set.0 & !C_LIBRARY_SIGNALS)),
        SIG_UNBLOCK => (MaskHow::Unblock, *set),
        SIG_SETMASK => (MaskHow::Set, SigSet(set.0 & !C_LIBRARY_SIGNALS)),
        _ => return Err(Errno::Inval),
    };
    crate::linux::rt_sigprocmask(how, set)
}

/// `signal` sent to the calling thread; its handler, if it has one, has run when this returns.
/// [`Errno::Inval`] for a number that is no signal. Async-signal-safe. Linux by `tgkill` to
/// the calling thread with every signal blocked around it, as glibc's `raise`
/// (`crate::linux::signal::raise`).
/// replaces libc::raise
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn raise_signal(signal: c_int) -> Result<(), Errno> {
    crate::linux::signal::raise(signal.cast_unsigned())
}

/// `signal`'s handler set to `handler` (a handler word: [`SIG_DFL`], [`SIG_IGN`] or a
/// one-argument `extern "C" fn(c_int)`'s address) with BSD `signal` semantics: `SA_RESTART`,
/// no other flag, an empty mask; the handler word before. [`Errno::Inval`] as [`set_action`].
/// Both systems, by [`set_action`].
///
/// # Safety
///
/// As [`set_action`]: a handler is an async-signal-safe `extern "C" fn(c_int)` that lives as
/// long as it may run.
/// replaces libc::signal
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn set_handler(signal: c_int, handler: sighandler_t) -> Result<sighandler_t, Errno> {
    let mut action = sigaction::DEFAULT;
    action.sa_sigaction = handler;
    action.sa_flags = SA_RESTART;
    // SAFETY: the handler's contract is the caller's, passed on unchanged.
    unsafe { set_action(signal, &action) }.map(|before| before.sa_sigaction)
}

/// The calling thread's alternate signal stack ([`stack_t::DISABLED`]'s flag when it has
/// none). Async-signal-safe.
/// replaces libc::sigaltstack (a read: null `ss`)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn alt_stack() -> Result<stack_t, Errno> {
    let mut current = stack_t::DISABLED;
    // SAFETY: no new stack (null); `current` is a live `stack_t` the call writes.
    match unsafe { sigaltstack(core::ptr::null(), &raw mut current) } {
        0 => Ok(current),
        _ => Err(errno()),
    }
}

/// The calling thread's alternate signal stack set to `stack` ([`stack_t::DISABLED`] turns it
/// off). [`Errno::NoMem`] for a size under the system's minimum, [`Errno::Perm`] while the
/// thread runs on it.
///
/// # Safety
///
/// `stack.ss_size` bytes from `stack.ss_sp` are writable memory nothing else uses, for as long
/// as the thread has it as its alternate stack.
/// replaces libc::sigaltstack (a write)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn set_alt_stack(stack: &stack_t) -> Result<(), Errno> {
    // SAFETY: `stack` is a live `stack_t` the call reads; the memory it names is the caller's.
    match unsafe { sigaltstack(stack, core::ptr::null_mut()) } {
        0 => Ok(()),
        _ => Err(errno()),
    }
}

/// The set of every signal.
/// replaces libc::sigfillset
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn full_set() -> sigset_t {
    let mut set = SIGSET_EMPTY;
    // SAFETY: `set` is a live set the call writes; `sigfillset` cannot fail on a valid pointer.
    unsafe { sigfillset(&raw mut set) };
    set
}

/// `signal` added to `set`. [`Errno::Inval`] for a number that is no signal.
/// replaces libc::sigaddset
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn add_to_set(set: &mut sigset_t, signal: c_int) -> Result<(), Errno> {
    // SAFETY: `set` is a live set the call reads and writes.
    match unsafe { sigaddset(set, signal) } {
        0 => Ok(()),
        _ => Err(errno()),
    }
}

/// The calling thread's mask changed by `set` as `how` says ([`SIG_BLOCK`], [`SIG_UNBLOCK`],
/// [`SIG_SETMASK`]); the mask before. [`Errno::Inval`] for another `how`.
/// replaces libc::sigprocmask
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_mask(how: c_int, set: &sigset_t) -> Result<sigset_t, Errno> {
    let mut before = SIGSET_EMPTY;
    // SAFETY: `set` and `before` are live sets, read and written and not kept.
    match unsafe { sigprocmask(how, set, &raw mut before) } {
        0 => Ok(before),
        _ => Err(errno()),
    }
}

/// `signal` sent to the calling thread; its handler, if it has one, has run when this returns.
/// [`Errno::Inval`] for a number that is no signal. Async-signal-safe.
/// replaces libc::raise
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn raise_signal(signal: c_int) -> Result<(), Errno> {
    // SAFETY: the call takes a number; the handler it runs carries its own installer's contract.
    match unsafe { raise(signal) } {
        0 => Ok(()),
        _ => Err(errno()),
    }
}

/// `signal` sent to the process `pid`: a negative `pid` is the process group `-pid`, 0 the
/// caller's group, and `signal` 0 only checks the process can be signalled. Linux by system call
/// (`blaze_sys::linux::kill`, which a Linux-only caller may name directly); async-signal-safe.
/// replaces libc::kill
#[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn kill(pid: pid_t, signal: c_int) -> Result<(), Errno> {
    // A negative number is no signal; as a `u32` it is one the kernel refuses (`EINVAL`).
    crate::linux::kill(pid, signal.cast_unsigned())
}

/// `signal` sent to the process `pid` (as on Linux). macOS by `crate::macos::cold::kill`, whose
/// first call in the process runs `dlsym`: not async-signal-safe until a first call has been
/// made, so a program that may first kill from a handler or between `fork` and `exec` makes a
/// `kill(getpid(), 0)` at its start.
/// replaces libc::kill
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn kill(pid: pid_t, signal: c_int) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory.
    match unsafe { crate::macos::cold::kill(pid, signal) } {
        0 => Ok(()),
        _ => Err(errno()),
    }
}

/// The process ends with `status` after its `atexit` handlers run and stdio is flushed: the end
/// of a `main` that returns through C. Not async-signal-safe ([`exit_now`] is). macOS only, by
/// `crate::macos::cold::exit`: the `atexit` list and stdio are the C library's own state, so
/// Linux has no kernel form; a Linux caller ends by [`exit_now`] (`exit_group`) after flushing
/// what it owns, or, where a user program's C library must run its handlers, calls that
/// program's own `exit` (a different question from blaze-sys's).
/// replaces libc::exit
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn exit(status: c_int) -> ! {
    // SAFETY: `exit` takes a number and does not return; the handlers it runs were registered
    // with their own contracts. Through the cold table's `exit`.
    unsafe {
        crate::macos::cold::exit(status);
    }
}

/// The process ends with `status` at once: no `atexit` handler, no flush. Async-signal-safe,
/// and safe in a forked child before its exec. Linux by `exit_group` (`blaze_sys::linux::exit_group`,
/// which is what glibc's `_exit` makes); macOS by libSystem's `_exit`.
/// replaces libc::_exit
#[blaze::noalloc]
#[blaze::nopanic]
pub fn exit_now(status: c_int) -> ! {
    #[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        crate::linux::exit_group(status);
    }
    #[cfg(target_os = "macos")]
    // SAFETY: `_exit` takes a number and does not return.
    unsafe {
        ffi::_exit(status);
    }
}

/// `SIGRTMIN`: the first real-time signal left to programs, 34: the kernel's first (32) after
/// glibc's own two, which this file keeps out of reach (the module doc). The value glibc's
/// `__libc_current_sigrtmin` returns since 2.34 (and before, with libpthread loaded); a
/// constant here, read with no call into the C library. A function, as the C macro is. Linux
/// only.
/// replaces libc::SIGRTMIN
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn sigrtmin() -> c_int {
    34
}

/// Every type, constant and declaration of this file compared with the `libc` crate's, once,
/// and a few calls made both ways. Deleted with the `libc` dev-dependency (D180).
#[cfg(test)]
mod tests {
    use core::ffi::{c_int, c_void};
    use core::mem::{align_of, offset_of, size_of};
    use core::sync::atomic::{AtomicI32, Ordering};

    use super::*;

    /// `set` as a `libc::sigset_t`: the same value on macOS; on Linux the kernel's word put in
    /// glibc's first word, the rest clear (signals 65 to 1024, which no kernel has).
    fn as_libcs(set: &sigset_t) -> libc::sigset_t {
        #[cfg(target_os = "macos")]
        let theirs = *set;
        #[cfg(target_os = "linux")]
        let theirs = {
            // SAFETY: an all-zero `sigset_t` is the empty set.
            let mut theirs: libc::sigset_t = unsafe { core::mem::zeroed() };
            // SAFETY: glibc's set is 16 `unsigned long`s from offset 0; the first holds signals
            // 1 to 64, bit `n - 1` for signal `n`, as the kernel's word does.
            unsafe { core::ptr::from_mut(&mut theirs).cast::<u64>().write(set.0) };
            theirs
        };
        theirs
    }

    /// The signals a set can hold on this system: 1 to 64 on Linux, 1 to 31 on macOS.
    #[cfg(target_os = "linux")]
    const SIGNALS: core::ops::RangeInclusive<c_int> = 1..=64;
    /// The signals a set can hold on this system: 1 to 64 on Linux, 1 to 31 on macOS.
    #[cfg(target_os = "macos")]
    const SIGNALS: core::ops::RangeInclusive<c_int> = 1..=31;

    /// Signals no other test of the crate touches, one per test that installs a handler (actions
    /// are the process's, and tests run at once): real-time signals on Linux; on macOS, which
    /// has none, the two the rest of the crate leaves alone.
    #[cfg(target_os = "linux")]
    fn private_signal(which: c_int) -> c_int {
        sigrtmin() + 10 + which
    }
    /// As Linux's: on macOS `SIGWINCH` and `SIGINFO`.
    #[cfg(target_os = "macos")]
    fn private_signal(which: c_int) -> c_int {
        if which == 0 { SIGWINCH } else { SIGINFO }
    }

    macro_rules! same_layout {
        ($ours:ty, $theirs:ty $(, $field:ident)*) => {{
            assert_eq!(size_of::<$ours>(), size_of::<$theirs>(), "size of {}", stringify!($ours));
            assert_eq!(align_of::<$ours>(), align_of::<$theirs>(), "align of {}", stringify!($ours));
            $(assert_eq!(
                offset_of!($ours, $field),
                offset_of!($theirs, $field),
                "offset of {}.{}",
                stringify!($ours),
                stringify!($field)
            );)*
        }};
    }

    /// Field offsets only, where Linux's kernel type and glibc's differ in size (the size and
    /// the rest are the const assertions beside each type).
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    macro_rules! same_offsets {
        ($ours:ty, $theirs:ty $(, $field:ident)*) => {{
            $(assert_eq!(
                offset_of!($ours, $field),
                offset_of!($theirs, $field),
                "offset of {}.{}",
                stringify!($ours),
                stringify!($field)
            );)*
        }};
    }

    #[test]
    fn every_layout_is_libcs() {
        // Linux: the set and the action are the kernel's, not glibc's (the module doc); the
        // set is glibc's first word, which `sets_are_libcs` checks bit by bit.
        #[cfg(target_os = "macos")]
        {
            same_layout!(sigset_t, libc::sigset_t);
            same_layout!(sigaction, libc::sigaction, sa_sigaction, sa_mask, sa_flags);
        }
        #[cfg(target_os = "linux")]
        {
            assert_eq!(size_of::<sigset_t>(), size_of::<libc::c_ulong>());
            assert_eq!(align_of::<sigset_t>(), align_of::<libc::sigset_t>());
        }
        same_layout!(stack_t, libc::stack_t, ss_sp, ss_flags, ss_size);
        same_layout!(sigval, libc::sigval, sival_ptr);
        same_layout!(siginfo_t, libc::siginfo_t, si_signo, si_errno, si_code);
        #[cfg(target_os = "linux")]
        same_layout!(sigevent, libc::sigevent, sigev_value, sigev_signo, sigev_notify, sigev_notify_thread_id);
        #[cfg(target_os = "macos")]
        same_layout!(siginfo_t, libc::siginfo_t, si_pid, si_uid, si_status, si_addr);
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        {
            same_layout!(mcontext_t, libc::mcontext_t, fault_address, regs, sp, pc, pstate);
            // The kernel's mask is 8 bytes and its pad the rest of glibc's 128: the same size.
            same_layout!(ucontext_t, libc::ucontext_t, uc_flags, uc_link, uc_stack, uc_sigmask, uc_mcontext);
        }
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            same_layout!(mcontext_t, libc::mcontext_t, gregs, fpregs);
            // The kernel's 304 bytes end at the mask; glibc's goes on (968).
            same_offsets!(ucontext_t, libc::ucontext_t, uc_flags, uc_link, uc_stack, uc_mcontext, uc_sigmask);
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            same_layout!(__darwin_arm_exception_state64, libc::__darwin_arm_exception_state64, __far, __esr, __exception);
            same_layout!(__darwin_arm_thread_state64, libc::__darwin_arm_thread_state64, __x, __fp, __lr, __sp, __pc, __cpsr);
            same_layout!(__darwin_arm_neon_state64, libc::__darwin_arm_neon_state64, __v, __fpsr, __fpcr);
            same_layout!(__darwin_mcontext64, libc::__darwin_mcontext64, __es, __ss, __ns);
            same_layout!(ucontext_t, libc::ucontext_t, uc_onstack, uc_sigmask, uc_stack, uc_link, uc_mcsize, uc_mcontext);
        }
    }

    #[test]
    fn every_constant_is_libcs() {
        let ours: &[(&str, c_int, c_int)] = &[
            ("SIGHUP", SIGHUP, libc::SIGHUP),
            ("SIGINT", SIGINT, libc::SIGINT),
            ("SIGQUIT", SIGQUIT, libc::SIGQUIT),
            ("SIGILL", SIGILL, libc::SIGILL),
            ("SIGTRAP", SIGTRAP, libc::SIGTRAP),
            ("SIGABRT", SIGABRT, libc::SIGABRT),
            ("SIGBUS", SIGBUS, libc::SIGBUS),
            ("SIGFPE", SIGFPE, libc::SIGFPE),
            ("SIGKILL", SIGKILL, libc::SIGKILL),
            ("SIGUSR1", SIGUSR1, libc::SIGUSR1),
            ("SIGSEGV", SIGSEGV, libc::SIGSEGV),
            ("SIGUSR2", SIGUSR2, libc::SIGUSR2),
            ("SIGPIPE", SIGPIPE, libc::SIGPIPE),
            ("SIGALRM", SIGALRM, libc::SIGALRM),
            ("SIGTERM", SIGTERM, libc::SIGTERM),
            ("SIGCHLD", SIGCHLD, libc::SIGCHLD),
            ("SIGCONT", SIGCONT, libc::SIGCONT),
            ("SIGSTOP", SIGSTOP, libc::SIGSTOP),
            ("SIGPROF", SIGPROF, libc::SIGPROF),
            ("SIGWINCH", SIGWINCH, libc::SIGWINCH),
            ("SIGSYS", SIGSYS, libc::SIGSYS),
            ("SIG_BLOCK", SIG_BLOCK, libc::SIG_BLOCK),
            ("SIG_UNBLOCK", SIG_UNBLOCK, libc::SIG_UNBLOCK),
            ("SIG_SETMASK", SIG_SETMASK, libc::SIG_SETMASK),
            ("SA_NOCLDSTOP", SA_NOCLDSTOP, libc::SA_NOCLDSTOP),
            ("SA_SIGINFO", SA_SIGINFO, libc::SA_SIGINFO),
            ("SA_ONSTACK", SA_ONSTACK, libc::SA_ONSTACK),
            ("SA_RESTART", SA_RESTART, libc::SA_RESTART),
            ("SA_NODEFER", SA_NODEFER, libc::SA_NODEFER),
            ("SA_RESETHAND", SA_RESETHAND, libc::SA_RESETHAND),
            ("SS_DISABLE", SS_DISABLE, libc::SS_DISABLE),
            #[cfg(target_os = "macos")]
            ("SIGINFO", SIGINFO, libc::SIGINFO),
            #[cfg(target_os = "linux")]
            ("SIGEV_THREAD_ID", SIGEV_THREAD_ID, libc::SIGEV_THREAD_ID),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            ("REG_RBP", REG_RBP, libc::REG_RBP),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            ("REG_RAX", REG_RAX, libc::REG_RAX),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            ("REG_RIP", REG_RIP, libc::REG_RIP),
        ];
        for (name, ours, theirs) in ours {
            assert_eq!(ours, theirs, "{name}");
        }
        assert_eq!(SIG_DFL, libc::SIG_DFL);
        assert_eq!(SIG_IGN, libc::SIG_IGN);
        assert_eq!(SIG_ERR, libc::SIG_ERR);
        assert_eq!(SIGSTKSZ, libc::SIGSTKSZ);
        #[cfg(target_os = "linux")]
        assert_eq!(sigrtmin(), libc::SIGRTMIN());
    }

    /// Each declaration has the C prototype `libc` gives it, where both take only scalars
    /// (macOS: Linux declares none).
    #[cfg(target_os = "macos")]
    #[test]
    fn every_scalar_signature_is_libcs() {
        let _: unsafe extern "C" fn(c_int) -> c_int = raise;
        let _: unsafe extern "C" fn(c_int) -> c_int = libc::raise;
        let _: unsafe extern "C" fn(c_int, sighandler_t) -> sighandler_t = signal;
        let _: unsafe extern "C" fn(c_int, libc::sighandler_t) -> libc::sighandler_t = libc::signal;
        let _: unsafe extern "C" fn(c_int) -> ! = ffi::_exit;
        let _: unsafe extern "C" fn(c_int) -> ! = libc::_exit;
    }

    /// `errno` and `set_errno` are the C library's cell, read and written both ways (macOS:
    /// Linux has no `errno` here).
    #[cfg(target_os = "macos")]
    #[test]
    fn errno_is_the_c_librarys() {
        // SAFETY: the calling thread's cell.
        let theirs = unsafe { libc::__error() };
        set_errno(Errno::Inval);
        // SAFETY: the calling thread's live cell.
        assert_eq!(unsafe { *theirs }, libc::EINVAL);
        // SAFETY: as above.
        unsafe { *theirs = libc::ENOENT };
        assert_eq!(errno(), Errno::NoEnt);
        set_errno(Errno::from_raw(0));
        assert_eq!(errno().raw(), 0);
    }

    /// An action read here is the one `libc` reads, and one set here is the one it reads back.
    #[test]
    fn an_action_round_trips_through_libc() {
        extern "C" fn nothing(_: c_int, _: *mut siginfo_t, _: *mut c_void) {}
        let before = get_action(SIGUSR2).expect("a read of SIGUSR2's action");
        let mut ours = sigaction::DEFAULT;
        ours.sa_sigaction = nothing as extern "C" fn(c_int, *mut siginfo_t, *mut c_void) as sighandler_t;
        ours.sa_flags = SA_SIGINFO;
        // SAFETY: `nothing` is async-signal-safe and lives as long as the process.
        let was = unsafe { set_action(SIGUSR2, &ours) }.expect("SIGUSR2's action set");
        assert_eq!(was.sa_sigaction, before.sa_sigaction);
        // SAFETY: an all-zero `sigaction` is a valid value of the C struct.
        let mut theirs: libc::sigaction = unsafe { core::mem::zeroed() };
        // SAFETY: a read into a live local.
        assert_eq!(unsafe { libc::sigaction(SIGUSR2, core::ptr::null(), &mut theirs) }, 0);
        assert_eq!(theirs.sa_sigaction, ours.sa_sigaction);
        assert_eq!(theirs.sa_flags & SA_SIGINFO, SA_SIGINFO);
        // Linux: glibc reads the kernel's action as it is: this file's restorer and its flag.
        #[cfg(target_os = "linux")]
        {
            let restorer = crate::linux::signal::restore_rt as unsafe extern "C" fn() as usize;
            assert_eq!(theirs.sa_flags & crate::linux::signal::SA_RESTORER, crate::linux::signal::SA_RESTORER);
            assert_eq!(theirs.sa_restorer.map(|it| it as usize), Some(restorer));
        }
        // SAFETY: the action that was there, put back.
        unsafe { set_action(SIGUSR2, &before) }.expect("SIGUSR2's action put back");
    }

    /// A handler installed here runs on a raise and returns (on Linux, through this crate's
    /// restorer: a wrong one ends the test process with `SIGSEGV`).
    #[test]
    fn a_handler_set_here_runs_and_returns() {
        static RAN: AtomicI32 = AtomicI32::new(0);
        extern "C" fn mark(signal: c_int, _: *mut siginfo_t, _: *mut c_void) {
            RAN.store(signal, Ordering::SeqCst);
        }
        let signal = private_signal(0);
        let mut ours = sigaction::DEFAULT;
        ours.sa_sigaction = mark as extern "C" fn(c_int, *mut siginfo_t, *mut c_void) as sighandler_t;
        ours.sa_flags = SA_SIGINFO;
        // SAFETY: `mark` is async-signal-safe (one atomic store) and lives as long as the process.
        let before = unsafe { set_action(signal, &ours) }.expect("the handler set");
        raise_signal(signal).expect("the signal raised");
        assert_eq!(RAN.load(Ordering::SeqCst), signal);
        // SAFETY: the action that was there, put back.
        unsafe { set_action(signal, &before) }.expect("the action put back");
    }

    /// The two-libraries case: an action glibc installed (with glibc's restorer), read here,
    /// shows glibc's restorer and flag; put back here it gets this crate's restorer, and its
    /// handler still runs and returns.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_action_glibc_installed_chains_here() {
        static RAN: AtomicI32 = AtomicI32::new(0);
        extern "C" fn glibcs(signal: c_int) {
            RAN.store(signal, Ordering::SeqCst);
        }
        let signal = private_signal(1);
        let ours = crate::linux::signal::restore_rt as unsafe extern "C" fn() as usize;
        // SAFETY: an all-zero `sigaction` is a valid value of the C struct.
        let mut theirs: libc::sigaction = unsafe { core::mem::zeroed() };
        theirs.sa_sigaction = glibcs as extern "C" fn(c_int) as libc::sighandler_t;
        // SAFETY: as above.
        let mut before: libc::sigaction = unsafe { core::mem::zeroed() };
        // SAFETY: live locals; `glibcs` is async-signal-safe and lives as long as the process.
        assert_eq!(unsafe { libc::sigaction(signal, &theirs, &mut before) }, 0);
        let read = get_action(signal).expect("glibc's action read");
        assert_eq!(read.sa_sigaction, theirs.sa_sigaction);
        // On x86_64 glibc always installs its own `__restore_rt` trampoline (`SA_RESTORER`);
        // on aarch64 glibc relies on the kernel vDSO `__kernel_rt_sigreturn` and leaves
        // `SA_RESTORER` clear.
        #[cfg(target_arch = "x86_64")]
        {
            assert_eq!(read.sa_flags & crate::linux::signal::SA_RESTORER, crate::linux::signal::SA_RESTORER);
            assert!(read.sa_restorer.is_some_and(|it| it as usize != ours), "glibc's own restorer");
        }
        #[cfg(target_arch = "aarch64")]
        {
            assert_ne!(read.sa_restorer.map(|it| it as usize), Some(ours));
        }
        // SAFETY: glibc's action, its handler as above.
        unsafe { set_action(signal, &read) }.expect("glibc's action put back here");
        assert_eq!(get_action(signal).expect("read again").sa_restorer.map(|it| it as usize), Some(ours));
        raise_signal(signal).expect("the signal raised");
        assert_eq!(RAN.load(Ordering::SeqCst), signal);
        // SAFETY: the action before the test, put back by glibc.
        assert_eq!(unsafe { libc::sigaction(signal, &before, core::ptr::null_mut()) }, 0);
    }

    /// The restorer is the C libraries' own instructions, byte for byte, which the unwinder
    /// recognises as a signal frame (`crate::linux::signal`'s module doc).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_restorer_is_the_c_librarys_bytes() {
        #[cfg(target_arch = "x86_64")]
        let expected: &[u8] = &[0x48, 0xc7, 0xc0, 0x0f, 0x00, 0x00, 0x00, 0x0f, 0x05];
        #[cfg(target_arch = "aarch64")]
        let expected: &[u8] = &[0x68, 0x11, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4];
        let at = (crate::linux::signal::restore_rt as unsafe extern "C" fn() as usize) as *const u8;
        // SAFETY: the function's first bytes, readable code of this binary.
        let ours = unsafe { core::slice::from_raw_parts(at, expected.len()) };
        assert_eq!(ours, expected);
    }

    /// glibc's own two signals are refused, as glibc refuses them.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_c_librarys_signals_are_refused() {
        for signal in [32, 33] {
            // SAFETY: refused before any call.
            assert_eq!(unsafe { set_action(signal, &sigaction::DEFAULT) }.err(), Some(Errno::Inval), "{signal}");
            // SAFETY: an all-zero `sigaction` is a valid value of the C struct.
            let theirs: libc::sigaction = unsafe { core::mem::zeroed() };
            // SAFETY: a live local, read and not kept; glibc refuses before any change.
            let glibcs = unsafe { libc::sigaction(signal, &theirs, core::ptr::null_mut()) };
            assert_eq!(glibcs, -1, "glibc refuses {signal} too");
        }
    }

    /// A set built here holds what `libc`'s `sigismember` says, signal by signal: `add_to_set`
    /// accepts and refuses what `sigaddset` does, and `full_set` is `sigfillset`'s.
    #[test]
    fn sets_are_libcs() {
        let mut set = SIGSET_EMPTY;
        add_to_set(&mut set, SIGUSR1).expect("SIGUSR1 added");
        let theirs = as_libcs(&set);
        // SAFETY: a live set.
        assert_eq!(unsafe { libc::sigismember(&theirs, libc::SIGUSR1) }, 1);
        // SAFETY: as above.
        assert_eq!(unsafe { libc::sigismember(&theirs, libc::SIGUSR2) }, 0);
        for signal in (SIGNALS.start() - 1)..=(SIGNALS.end() + 1) {
            let mut ours = SIGSET_EMPTY;
            // SAFETY: an all-zero set is the empty set.
            let mut theirs: libc::sigset_t = unsafe { core::mem::zeroed() };
            // SAFETY: a live set.
            let glibcs = unsafe { libc::sigaddset(&mut theirs, signal) };
            assert_eq!(add_to_set(&mut ours, signal).is_ok(), glibcs == 0, "sigaddset({signal})");
            let ours = as_libcs(&ours);
            for member in SIGNALS {
                // SAFETY: live sets.
                let (in_ours, in_theirs) = unsafe { (libc::sigismember(&ours, member), libc::sigismember(&theirs, member)) };
                assert_eq!(in_ours, in_theirs, "signal {member} in the set made by adding {signal}");
            }
        }
        let full = as_libcs(&full_set());
        // SAFETY: an all-zero set, then filled.
        let mut filled: libc::sigset_t = unsafe { core::mem::zeroed() };
        // SAFETY: a live set.
        assert_eq!(unsafe { libc::sigfillset(&mut filled) }, 0);
        for signal in SIGNALS {
            // SAFETY: live sets.
            let (ours, theirs) = unsafe { (libc::sigismember(&full, signal), libc::sigismember(&filled, signal)) };
            assert_eq!(ours, theirs, "signal {signal} in the full set");
        }
    }

    /// A mask set here is the one `libc` reads back; on Linux glibc's own two are taken out.
    #[test]
    fn a_mask_set_here_is_libcs() {
        let signal = private_signal(0);
        let mut set = SIGSET_EMPTY;
        add_to_set(&mut set, signal).expect("the signal added");
        #[cfg(target_os = "linux")]
        let set = crate::linux::SigSet(set.0 | C_LIBRARY_SIGNALS);
        let before = set_mask(SIG_BLOCK, &set).expect("the signal blocked");
        // SAFETY: an all-zero set, then written.
        let mut now: libc::sigset_t = unsafe { core::mem::zeroed() };
        // SAFETY: a read of the calling thread's mask into a live local.
        assert_eq!(unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, core::ptr::null(), &mut now) }, 0);
        // SAFETY: a live set.
        assert_eq!(unsafe { libc::sigismember(&now, signal) }, 1);
        #[cfg(target_os = "linux")]
        // SAFETY: as above.
        assert_eq!(unsafe { libc::sigismember(&now, 33) }, 0, "SIGSETXID taken out");
        assert_eq!(set_mask(SIG_SETMASK, &before).map(|_| ()), Ok(()));
        assert_eq!(set_mask(99, &before).err(), Some(Errno::Inval));
    }

    /// An alternate stack set here is the one `libc` reads back, and the thread's own is put
    /// back after.
    #[test]
    fn an_alt_stack_set_here_is_libcs() {
        let before = alt_stack().expect("the thread's alternate stack");
        let mut memory = std::vec![0u8; SIGSTKSZ * 2];
        let stack = stack_t { ss_sp: memory.as_mut_ptr().cast(), ss_flags: 0, ss_size: memory.len() };
        // SAFETY: `memory` outlives its use: the thread's own stack is put back before it drops,
        // and no signal runs on it in between (none is sent).
        unsafe { set_alt_stack(&stack) }.expect("the stack set");
        // SAFETY: an all-zero `stack_t` is valid.
        let mut theirs: libc::stack_t = unsafe { core::mem::zeroed() };
        // SAFETY: a read into a live local.
        assert_eq!(unsafe { libc::sigaltstack(core::ptr::null(), &mut theirs) }, 0);
        assert_eq!((theirs.ss_sp.cast::<c_void>(), theirs.ss_size), (stack.ss_sp, stack.ss_size));
        let read = alt_stack().expect("read again");
        assert_eq!((read.ss_sp, read.ss_size), (stack.ss_sp, stack.ss_size));
        // SAFETY: the stack the thread had, as it had it.
        unsafe { set_alt_stack(&before) }.expect("the thread's own stack put back");
        drop(memory);
    }
}
