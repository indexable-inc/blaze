//! Signals by raw system call, with no C library: the kernel's `struct sigaction` and
//! `rt_sigaction`, the restorer every handler blaze-sys installs returns through, the kernel's
//! `stack_t` and `sigaltstack`, and `tgkill` (and `raise` made from it). These are what
//! `crate::posix::signal` is on Linux.
//!
//! # The restorer
//!
//! A handler returns to the address the kernel wrote as its return address, which must make
//! `rt_sigreturn` so the kernel puts back the interrupted registers and mask. x86-64 has no
//! default: an action without `SA_RESTORER` and a restorer makes the delivery fail
//! (`setup_rt_frame` returns `-EFAULT` and the thread takes `SIGSEGV`). aarch64 does not need
//! one: without `SA_RESTORER` the kernel returns through the vDSO's `__kernel_rt_sigreturn`
//! (arch/arm64/kernel/signal.c, `setup_return`), and it honours `SA_RESTORER` when it is set.
//! [`set_action`] sets it on both, with this file's own [`restore_rt`], so there is one path
//! and no dependence on a vDSO being mapped.
//!
//! The restorer is written instruction for instruction as the C libraries' are (glibc's
//! `__restore_rt`, the vDSO's): `mov rax, 15; syscall` (bytes `48 c7 c0 0f 00 00 00 0f 05`) on
//! x86-64 and `mov x8, #139; svc #0` (`d2801168 d4000001`) on aarch64. The unwinder Rust's
//! std uses on Linux (libgcc's `x86_64_fallback_frame_state` / `aarch64_fallback_frame_state`,
//! and LLVM libunwind's sigreturn check) recognises a signal frame by exactly those bytes at
//! the return address, so a backtrace taken in a handler walks on into the interrupted code.
//! (`mov eax, 15`, which the kernel accepts as well, is a different encoding and would not be
//! recognised.) The test `the_restorer_is_the_c_librarys_bytes` checks them.
//!
//! # Two libraries in one process
//!
//! A blaze process on Linux may also have glibc loaded (Rust std's, a placed program's). Actions
//! are per process, held by the kernel, so the two see one table:
//!
//! - An action glibc installed, read here by `rt_sigaction`, carries glibc's `SA_RESTORER` and
//!   `__restore_rt`. Its handler is called directly by a chaining handler (it is a plain
//!   function), and put back by [`set_action`] it gets this file's restorer instead, which does
//!   the same. Nothing of glibc's is lost: glibc keeps no copy of an action (its `sigaction`
//!   is a translation of the kernel's), and it returns the kernel's flags as they are,
//!   `SA_RESTORER` included, so a flag word read either way is the same.
//! - An action installed here, read by glibc's `sigaction`, shows this file's restorer; given
//!   back to glibc's `sigaction` it gets glibc's again. Both restorers are valid for the
//!   process's whole life (they are code of loaded objects that are never unloaded).
//! - glibc's own signals, 32 (`SIGCANCEL`) and 33 (`SIGSETXID`), are refused by its
//!   `sigaction`, left out of its `sigfillset` and taken out of a mask by its `sigprocmask`.
//!   This file does not know them (a raw call takes any signal: a child between `clone3` and
//!   `exec` resets them all); `crate::posix::signal` keeps glibc's three rules, so code that
//!   goes through it cannot break glibc's thread cancellation or `setuid` broadcast.
//! - The sets differ: the kernel's [`SigSet`] is 8 bytes and glibc's `sigset_t` 128. A
//!   [`SigSet`] is never handed to a C function.
//!
//! Sources: include/uapi/asm-generic/signal.h and arch/x86/include/uapi/asm/signal.h (`struct
//! sigaction`, `SA_RESTORER`), include/uapi/asm-generic/signal-defs.h, include/uapi/linux/
//! signal.h (`SS_DISABLE`), include/uapi/asm-generic/signal.h (`stack_t`), kernel/signal.c.

use core::ffi::{c_int, c_uint, c_void};

use super::{nr, raw, wait::SigSet};
use crate::Errno;

/// `SA_RESTORER`: the action's `sa_restorer` is where its handler returns to. 0x0400_0000 on
/// x86-64 and aarch64 (include/uapi/asm-generic/signal-defs.h, arch/x86/include/uapi/asm/
/// signal.h). [`set_action`] sets it; a caller does not.
/// replaces libc::SA_RESTORER
pub const SA_RESTORER: c_int = 0x0400_0000;

/// `SS_DISABLE`: in a [`Stack`]'s `ss_flags`, the thread has no alternate stack. 2
/// (include/uapi/linux/signal.h).
/// replaces libc::SS_DISABLE
pub const SS_DISABLE: c_int = 2;

/// The kernel's `struct sigaction` (include/uapi/asm-generic/signal.h; the same order on
/// x86-64): the handler word, the flags, the restorer, the mask. The fields carry the `libc`
/// crate's names so `crate::posix::signal::sigaction` is this type and a port from
/// `libc::sigaction` changes only the path and the mask's type.
///
/// The kernel's `unsigned long sa_flags` is declared as its two halves: `sa_flags`, the `int`
/// the `SA_*` constants are, and `sa_flags_high`, which no flag uses and is always 0. So an
/// `int` flag word is stored as it is; a widening (`as c_ulong`) would sign-extend
/// `SA_RESETHAND` (bit 31) into the high half. Both targets are little-endian, so the `int`
/// half is the word's low four bytes.
/// replaces libc::sigaction (the struct, Linux: the kernel's form, not glibc's)
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct SigAction {
    /// `SIG_DFL` (0), `SIG_IGN` (1), or a handler's address (`sa_handler`, or with
    /// `SA_SIGINFO` a three-argument `sa_sigaction`).
    pub sa_sigaction: usize,
    /// `SA_*`: the low half of the kernel's flag word.
    pub sa_flags: c_int,
    /// The high half of the kernel's flag word: 0.
    pub sa_flags_high: c_uint,
    /// Where the handler returns to: set by [`set_action`] (this file's [`restore_rt`]); as
    /// read from the kernel, whatever the installer set (glibc's `__restore_rt` for an action
    /// glibc installed).
    pub sa_restorer: Option<unsafe extern "C" fn()>,
    /// The signals blocked while the handler runs (its own too, without `SA_NODEFER`).
    pub sa_mask: SigSet,
}

impl SigAction {
    /// `SIG_DFL`, no flags, no restorer, an empty mask: what an all-zero `sigaction` is, so a
    /// value to fill or to be written into needs no `mem::zeroed`.
    /// replaces core::mem::zeroed::<libc::sigaction>()
    pub const DEFAULT: SigAction =
        SigAction { sa_sigaction: 0, sa_flags: 0, sa_flags_high: 0, sa_restorer: None, sa_mask: SigSet::EMPTY };
}

// The kernel's `struct sigaction` (include/uapi/asm-generic/signal.h; arch/x86/include/uapi/
// asm/signal.h), Linux aarch64 and x86-64: 32 bytes, the mask the kernel's 8 at 24.
const _: () = {
    assert!(size_of::<SigAction>() == 32);
    assert!(align_of::<SigAction>() == 8);
    assert!(core::mem::offset_of!(SigAction, sa_sigaction) == 0);
    assert!(core::mem::offset_of!(SigAction, sa_flags) == 8);
    assert!(core::mem::offset_of!(SigAction, sa_flags_high) == 12);
    assert!(core::mem::offset_of!(SigAction, sa_restorer) == 16);
    assert!(core::mem::offset_of!(SigAction, sa_mask) == 24);
    assert!(size_of::<SigSet>() == 8);
};

/// The kernel's `stack_t` (include/uapi/asm-generic/signal.h; glibc's is the same): an
/// alternate signal stack's base, flags (`SS_DISABLE`, `SS_ONSTACK`) and size.
/// replaces libc::stack_t (Linux)
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Stack {
    /// The stack's lowest address.
    pub ss_sp: *mut c_void,
    /// `SS_*`.
    pub ss_flags: c_int,
    /// Its size in bytes.
    pub ss_size: usize,
}

impl Stack {
    /// No stack (`SS_DISABLE`, null, 0): what [`alt_stack`] starts from, and what turns a
    /// thread's alternate stack off when given to [`set_alt_stack`].
    /// replaces core::mem::zeroed::<libc::stack_t>()
    pub const DISABLED: Stack = Stack { ss_sp: core::ptr::null_mut(), ss_flags: SS_DISABLE, ss_size: 0 };
}

// The kernel's `stack_t` (include/uapi/asm-generic/signal.h), Linux aarch64 and x86-64.
const _: () = {
    assert!(size_of::<Stack>() == 24);
    assert!(align_of::<Stack>() == 8);
    assert!(core::mem::offset_of!(Stack, ss_sp) == 0);
    assert!(core::mem::offset_of!(Stack, ss_flags) == 8);
    assert!(core::mem::offset_of!(Stack, ss_size) == 16);
};

/// The restorer: `rt_sigreturn` (x86-64 15), never returning. Its bytes are glibc's
/// `__restore_rt` (module doc). Only the kernel jumps here, as a handler's return address.
///
/// # Safety
///
/// Never called: it is valid only as the return address of a handler the kernel entered, with
/// the signal frame on the stack.
/// replaces glibc's __restore_rt (the restorer glibc's sigaction supplies)
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn restore_rt() {
    core::arch::naked_asm!("mov rax, {number}", "syscall", number = const nr::RT_SIGRETURN);
}

/// The restorer: `rt_sigreturn` (aarch64 139), never returning. Its words are the vDSO's
/// `__kernel_rt_sigreturn` (module doc). Only the kernel jumps here.
///
/// # Safety
///
/// As x86-64's: valid only as a handler's return address.
/// replaces the vDSO's __kernel_rt_sigreturn (aarch64's default restorer)
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn restore_rt() {
    core::arch::naked_asm!("mov x8, #{number}", "svc #0", number = const nr::RT_SIGRETURN);
}

/// `rt_sigaction` with a null new action: `signal`'s action as the kernel holds it.
/// [`Errno::Inval`] for a number that is no signal (0, above 64). Async-signal-safe.
/// replaces libc::sigaction (a read: null `act`)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn get_action(signal: u32) -> Result<SigAction, Errno> {
    let mut old = SigAction::DEFAULT;
    // SAFETY: no new action (null); `old` is a live kernel `sigaction` the kernel writes and
    // keeps no pointer to; the set's size is the kernel's own.
    raw::check(unsafe {
        raw::syscall4(nr::RT_SIGACTION, signal as usize, 0, core::ptr::from_mut(&mut old) as usize, size_of::<SigSet>())
    })?;
    Ok(old)
}

/// `rt_sigaction`: `signal`'s action set to `action`, with `SA_RESTORER` and [`restore_rt`]
/// put in (whatever `action` held there); the action before. [`Errno::Inval`] for a number
/// that is no signal, or `SIGKILL`/`SIGSTOP`. Async-signal-safe. Takes every signal 1 to 64,
/// glibc's own 32 and 33 too (`crate::posix::signal::set_action` refuses those).
///
/// # Safety
///
/// A handler `action` names is a function of the signature its flags say (`SA_SIGINFO`: three
/// arguments), lives as long as it may run, and is async-signal-safe; an `SA_ONSTACK` handler
/// runs on the alternate stack of each thread that has one.
/// replaces libc::sigaction (a write)
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn set_action(signal: u32, action: &SigAction) -> Result<SigAction, Errno> {
    let mut new = *action;
    new.sa_flags |= SA_RESTORER;
    new.sa_restorer = Some(restore_rt);
    let mut old = SigAction::DEFAULT;
    // SAFETY: `new` and `old` are live kernel `sigaction`s, read and written and not kept; the
    // restorer is this file's, valid for the process's life; the handler's contract is the
    // caller's.
    raw::check(unsafe {
        raw::syscall4(
            nr::RT_SIGACTION,
            signal as usize,
            core::ptr::from_ref(&new) as usize,
            core::ptr::from_mut(&mut old) as usize,
            size_of::<SigSet>(),
        )
    })?;
    Ok(old)
}

/// `sigaltstack` with a null new stack: the calling thread's alternate stack ([`SS_DISABLE`]
/// in its flags when it has none). Async-signal-safe.
/// replaces libc::sigaltstack (a read: null `ss`)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn alt_stack() -> Result<Stack, Errno> {
    let mut current = Stack::DISABLED;
    // SAFETY: no new stack (null); `current` is a live `stack_t` the kernel writes.
    raw::check(unsafe { raw::syscall2(nr::SIGALTSTACK, 0, core::ptr::from_mut(&mut current) as usize) })?;
    Ok(current)
}

/// `sigaltstack`: the calling thread's alternate stack set to `stack` ([`Stack::DISABLED`]
/// turns it off); the stack before. [`Errno::NoMem`] for a size under `MINSIGSTKSZ`,
/// [`Errno::Perm`] while the thread runs on it, [`Errno::Inval`] for other flags.
///
/// # Safety
///
/// `stack.ss_size` bytes from `stack.ss_sp` are writable memory nothing else uses, for as long
/// as the thread has it as its alternate stack.
/// replaces libc::sigaltstack (a write)
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn set_alt_stack(stack: &Stack) -> Result<Stack, Errno> {
    let mut before = Stack::DISABLED;
    // SAFETY: `stack` and `before` are live `stack_t`s, read and written and not kept; the
    // memory `stack` names is the caller's.
    raw::check(unsafe {
        raw::syscall2(nr::SIGALTSTACK, core::ptr::from_ref(stack) as usize, core::ptr::from_mut(&mut before) as usize)
    })?;
    Ok(before)
}

/// `tgkill`: `signal` sent to the thread `tid` of the thread group (process) `tgid`.
/// [`Errno::Srch`] where no such thread is in that group (a tid reused by another process
/// is not signalled), [`Errno::Inval`] for a number that is no signal. Async-signal-safe.
/// replaces libc::tgkill
#[blaze::noalloc]
#[blaze::nopanic]
pub fn tgkill(tgid: i32, tid: i32, signal: u32) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall3(nr::TGKILL, tgid as isize as usize, tid as isize as usize, signal as usize) })
        .map(|_| ())
}

/// `raise`: `signal` sent to the calling thread; its handler, if it has one and the signal is
/// not blocked, has run when this returns. As glibc's `raise`: every signal is blocked while
/// the process's and thread's ids are read and the signal sent, so a handler that forks in
/// between cannot make the child signal its parent's thread; the mask put back delivers it.
/// [`Errno::Inval`] for a number that is no signal. Async-signal-safe.
/// replaces libc::raise
#[blaze::noalloc]
#[blaze::nopanic]
pub fn raise(signal: u32) -> Result<(), Errno> {
    // Every bit: the kernel leaves `SIGKILL` and `SIGSTOP` out of any mask itself.
    let before = super::rt_sigprocmask(super::MaskHow::Block, SigSet(u64::MAX))?;
    let sent = tgkill(super::getpid(), super::sample::gettid(), signal);
    super::rt_sigprocmask(super::MaskHow::Set, before)?;
    sent
}
