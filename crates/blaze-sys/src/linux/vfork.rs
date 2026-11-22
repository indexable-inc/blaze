//! A child started on a stack the caller lends, sharing the caller's memory until it runs its
//! program (`CLONE_VM | CLONE_VFORK`, what `posix_spawn` does): a spawn that costs the same
//! however large the parent is, where a fork copies its page tables. The pidfd comes back with
//! the pid from the same call (`CLONE_PIDFD`), so no second step races the child.
//!
//! One operation, [`start_on_stack`], in the two forms of the
//! [`StartProcess`](super::slot::StartProcess) slot, chosen once from `KernelFacts`:
//!
//! | form | call | flags | pidfd written to | kernel |
//! |---|---|---|---|---|
//! | `Clone3` | `clone3(&clone_args, 88)` | `CLONE_VM \| CLONE_VFORK \| CLONE_PIDFD`, `exit_signal = SIGCHLD` | `clone_args.pidfd` | 5.3 |
//! | `Clone` | `clone(flags, sp, &pidfd, 0, 0)` | `CLONE_VM \| CLONE_VFORK \| CLONE_PIDFD \| SIGCHLD` | the third argument, `parent_tid` | 5.2 (`CLONE_PIDFD`) |
//!
//! The legacy `clone`'s argument order (kernel/fork.c, `SYSCALL_DEFINE5(clone, ...)`): the
//! default is `(clone_flags, newsp, parent_tidptr, child_tidptr, tls)`, which x86-64 uses
//! (arch/x86/Kconfig selects `CLONE_BACKWARDS` only for 32-bit x86); under
//! `CONFIG_CLONE_BACKWARDS` it is `(clone_flags, newsp, parent_tidptr, tls, child_tidptr)`, the
//! order musl's and glibc's aarch64 `clone` pass (`tls` in `x3`, `ctid` in `x4`). Only
//! `CONFIG_CLONE_BACKWARDS2` (s390: `newsp` first) moves the first three. So on both targets
//! blaze builds for, `parent_tidptr` is the THIRD argument, and with `CLONE_PIDFD` (and no
//! `CLONE_PARENT_SETTID`, which the kernel refuses beside it) the pidfd is written through it.
//! The fourth and fifth, which the two orders swap, are both 0 here (no `CLONE_CHILD_SETTID`,
//! `CLONE_CHILD_CLEARTID` or `CLONE_SETTLS`), so the swap cannot matter. This settles UNSURE K1.
//!
//! Where the child's stack pointer starts: `clone3` sets it to `stack + stack_size`
//! (kernel/fork.c, `clone3_stack_valid`, on a stack that grows down), the legacy `clone` to
//! `newsp`. Both are given the same 16-byte-aligned address, which both ABIs want.
//!
//! A later, better form (not built): `CLONE_INTO_CGROUP` (5.7, `clone_args` to its `cgroup`
//! field, [`CloneFlags::INTO_CGROUP`](super::CloneFlags::INTO_CGROUP)) starts the child in its
//! cgroup, which would replace blaze-core spawn.rs's step 2, "putting it in its cgroup" (a
//! write of "0" to `cgroup.procs` in the child), with no step in the child at all.
//!
//! # The contract the child keeps (spawn.rs's `run` states the same)
//!
//! - Memory is shared (`CLONE_VM`) and so is the calling thread's TLS (no `CLONE_SETTLS`):
//!   until it execs or exits, the child makes raw system calls only (blaze-sys's: none writes
//!   `errno`), plain loads and stores to the data the parent lent through `arg`, and its own
//!   stack. No allocation, no lock (another thread of the parent may hold any), no panic: a
//!   panic would run the process's panic handler in the child on the parent's memory. `entry`
//!   should carry `#[blaze::noalloc] #[blaze::nopanic]`; the trampoline calls it through
//!   the `extern "C"` shim `launch`, so an unwind cannot leave it (it aborts at that boundary).
//! - The parent's calling thread is suspended until the child execs or exits
//!   (`CLONE_VFORK`; the kernel's wait is killable only, and a fatal signal ends the parent
//!   without returning); other threads of the parent run on.
//! - Signals are part of the wrapper, not the caller's obligation (rule 76): every signal is
//!   blocked (`rt_sigprocmask(SIG_SETMASK, all)`) before the call and the mask before it is
//!   restored in the parent after, so no handler of the parent's runs in the child on the
//!   shared memory. The child starts with every signal blocked: before it unblocks any, it
//!   resets each handled signal's disposition to the default; before its exec it sets the mask
//!   its program should start with (`execve` keeps the mask and resets only handlers).
//! - The stack has no guard page: the child's deepest frame must fit in the lent bytes. Fewer
//!   than [`MIN_STACK`] are refused before any call ([`StartFailed::StackTooSmall`]).
//! - What the child wrote through `arg` is read by the parent after the call returns, through
//!   the same `&mut T`: the wrapper hands `arg`'s address to the trampoline (an opaque call) by
//!   storing it on the lent stack, so the compiler must assume that call wrote it. A caller
//!   that kept a raw pointer instead reads through it with `read_volatile`.
//!
//! On x86-64 with user shadow stacks (6.6), a `CLONE_VFORK` child shares the parent's shadow
//! stack (arch/x86/kernel/shstk.c): the child's `call` into `entry` pushes below the parent's
//! shadow pointer and never returns into the trampoline, so the parent's entries stay intact.

use core::mem::MaybeUninit;

use super::fs::OwnedFd;
use super::slot::StartProcess;
use super::wait::SigSet;
use super::{CloneArgs, MaskHow, nr, raw, rt_sigprocmask};
use crate::Errno;

/// `CLONE_VM`: the child shares the caller's memory.
const CLONE_VM: u64 = 0x100;
/// `CLONE_PIDFD`: a pidfd of the child is written for the parent.
const CLONE_PIDFD: u64 = 0x1000;
/// `CLONE_VFORK`: the calling thread waits until the child execs or exits.
const CLONE_VFORK: u64 = 0x4000;
/// `SIGCHLD`: the signal the parent gets at the child's end.
const SIGCHLD: u64 = 17;

/// The fewest stack bytes [`start_on_stack`] lends a child: a few frames of system call
/// wrappers with room to spare. Fewer are [`StartFailed::StackTooSmall`].
pub const MIN_STACK: usize = 16 << 10;

/// What the child runs: `entry(arg)` on the lent stack; its return is the child's exit status
/// (the kernel keeps the low 8 bits). It may also end the child itself (`execveat`,
/// `exit_group`). See the module doc for what it may do.
/// replaces the `int (*fn)(void *)` of libc::clone
pub type ChildEntry<T> = fn(&mut T) -> i32;

/// The child the parent got: its pid and its pidfd, made by the same call.
/// replaces the pid libc::clone returns and the pidfd it writes through `parent_tid`
#[derive(Debug)]
pub struct Launched {
    /// The child's pid.
    pub pid: i32,
    /// The child's pidfd: polls readable at its exit; `waitid(P_PIDFD)` reaps it.
    pub pidfd: OwnedFd,
}

/// Why [`start_on_stack`] has no child to give.
/// replaces the -1 and errno of libc::clone
#[derive(Debug)]
pub enum StartFailed {
    /// The lent stack is under [`MIN_STACK`] bytes; refused before any call.
    StackTooSmall {
        /// Its length.
        len: usize,
    },
    /// Blocking every signal before the call was refused; no child was made.
    Mask(Errno),
    /// The `clone3` or `clone` was refused; no child was made.
    Clone(Errno),
    /// The child ran (and has exec'd or exited) but the kernel wrote no pidfd: not a kernel
    /// the form was chosen for. Its pid, for the caller to reap.
    NoPidfd {
        /// The child's pid.
        pid: i32,
    },
    /// The child ran, but the mask before the call could not be restored: this thread still
    /// has every signal blocked. The child, for the caller to wait for.
    MaskNotRestored {
        /// Why.
        errno: Errno,
        /// The child.
        launched: Launched,
    },
}

impl core::fmt::Display for StartFailed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StartFailed::StackTooSmall { len } => {
                write!(f, "start a child on a lent stack: {len} bytes, under the {MIN_STACK} it needs")
            }
            StartFailed::Mask(errno) => write!(f, "start a child: blocking every signal first: {errno:?}"),
            StartFailed::Clone(errno) => write!(f, "start a child (CLONE_VM | CLONE_VFORK | CLONE_PIDFD): {errno:?}"),
            StartFailed::NoPidfd { pid } => write!(f, "start a child: child {pid} ran but the kernel wrote no pidfd"),
            StartFailed::MaskNotRestored { errno, launched } => write!(
                f,
                "start a child: child {} ran but the signal mask before it was not restored: {errno:?}",
                launched.pid
            ),
        }
    }
}

/// What the child's first frame reads, written on the lent stack 16 bytes above where its
/// stack pointer starts: the entry and its argument.
#[repr(C)]
struct Launch<T> {
    /// The entry.
    entry: ChildEntry<T>,
    /// Its argument, the parent's.
    arg: *mut T,
}

// Layout: two pointers, whatever `T` is (a `fn` pointer and a thin `*mut T`, `T: Sized`).
// Source: the Rust reference, "Type layout", `repr(C)`; asserted for one `T`, the same for all.
const _: () = {
    assert!(size_of::<Launch<u8>>() == 16);
    assert!(align_of::<Launch<u8>>() == 8);
    assert!(core::mem::offset_of!(Launch<u8>, entry) == 0);
    assert!(core::mem::offset_of!(Launch<u8>, arg) == 8);
};

/// The words below the stack's 16-byte-aligned top that the trampoline reads: `[sp]` the
/// address of [`launch`], `[sp + 8]` unused, `[sp + 16 .. sp + 32]` the [`Launch`].
const FRAME: usize = 32;

/// The child's first Rust frame: reads the [`Launch`] at `frame` and runs its entry. Called
/// only by the trampoline's child path, on the lent stack.
///
/// # Safety
///
/// `frame` points at a live `Launch<T>` whose `arg` is a live, unaliased `T`: what
/// [`start_on_stack`] wrote before the call, while the parent waits (`CLONE_VFORK`).
unsafe extern "C" fn launch<T>(frame: *const u8) -> i32 {
    // SAFETY: as the function's contract: `start_on_stack` wrote a `Launch<T>` there, 8-byte
    // aligned (16-aligned `sp` plus 16), and nothing writes it while the child runs.
    let Launch { entry, arg } = unsafe { frame.cast::<Launch<T>>().read() };
    // SAFETY: `arg` came from the `&mut T` the parent lent for the whole call; the parent's
    // thread is suspended, so this is the one live reference to it.
    entry(unsafe { &mut *arg })
}

/// The call, its child path and the child's end, per architecture. Arguments in the C ABI's
/// registers: `a0` to `a4` the call's five, `nr` its number. In the parent it returns the
/// call's raw result (the pid, or an error's number negated). In the child (result 0) the
/// stack pointer is already the lent stack's `sp` (the kernel set it), so nothing of the
/// parent's frames is touched: it calls `[sp]` (the [`launch`] shim) with `sp + 16`, then
/// `exit_group` with what it returned, and never returns.
///
/// x86-64: `rdi rsi rdx rcx r8` in, `r9` the number; the call wants `r10` for the fourth and
/// clobbers `rcx` and `r11`, all caller-saved. aarch64: `x0` to `x4` in, `x5` the number,
/// moved to `x8`; the kernel keeps every register but `x0`, `x30` included, so `ret` returns.
///
/// # Safety
///
/// `nr` is `clone` or `clone3` with `CLONE_VM | CLONE_VFORK` and a child stack whose start
/// holds the [`FRAME`] words [`start_on_stack`] writes; anything else is undefined.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C" fn vfork_call(_a0: usize, _a1: usize, _a2: usize, _a3: usize, _a4: usize, _nr: usize) -> usize {
    core::arch::naked_asm!(
        "mov rax, r9",
        "mov r10, rcx",
        "syscall",
        "test rax, rax",
        "jz 2f",
        "ret",
        // The child, on the lent stack, `rsp` 16-byte aligned.
        "2:",
        "xor ebp, ebp",
        "lea rdi, [rsp + 16]",
        "call qword ptr [rsp]",
        "mov edi, eax",
        "mov eax, {exit_group}",
        "syscall",
        "ud2",
        exit_group = const nr::EXIT_GROUP,
    );
}

/// As x86-64's: see there.
///
/// # Safety
///
/// As x86-64's.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn vfork_call(_a0: usize, _a1: usize, _a2: usize, _a3: usize, _a4: usize, _nr: usize) -> usize {
    core::arch::naked_asm!(
        "mov x8, x5",
        "svc #0",
        "cbz x0, 2f",
        "ret",
        // The child, on the lent stack, `sp` 16-byte aligned.
        "2:",
        "mov x29, xzr",
        "mov x30, xzr",
        "ldr x1, [sp]",
        "add x0, sp, #16",
        "blr x1",
        "mov x8, #{exit_group}",
        "svc #0",
        "brk #1",
        exit_group = const nr::EXIT_GROUP,
    );
}

/// Starts a child that runs `entry(arg)` on `stack`, sharing this process's memory until it
/// execs or exits, in the form `form` (the `StartProcess` slot's choice from `KernelFacts`):
/// its pid and pidfd. The calling thread is suspended until the child has exec'd or exited,
/// so when this returns the child no longer uses `stack` or `arg`, and what it wrote through
/// `arg` is there to read. Every signal is blocked across the call and the mask restored
/// after (the module doc). [`StartFailed::StackTooSmall`] under [`MIN_STACK`] bytes, before
/// any call.
///
/// replaces libc::clone (with CLONE_VM | CLONE_VFORK and a pidfd: blaze_sys::linux::start_on_stack)
/// replaces libc::syscall(libc::SYS_clone3) with CLONE_VM | CLONE_VFORK | CLONE_PIDFD and a stack (blaze_sys::linux::start_on_stack(StartProcess::Clone3, ..))
/// replaces libc::pthread_sigmask around a vfork-style clone (the wrapper blocks and restores every signal itself)
///
/// # Safety
///
/// `entry` keeps the module doc's contract until it execs or exits: raw system calls,
/// loads and stores to `arg` and its own stack, no allocation, lock or panic, and frames that
/// fit in `stack` (no guard page: past it, it writes whatever lies below).
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn start_on_stack<T>(
    form: StartProcess,
    stack: &mut [MaybeUninit<u8>],
    entry: ChildEntry<T>,
    arg: &mut T,
) -> Result<Launched, StartFailed> {
    let len = stack.len();
    if len < MIN_STACK {
        return Err(StartFailed::StackTooSmall { len });
    }
    let base = stack.as_mut_ptr().cast::<u8>();
    // It grows down from its end, which both ABIs want on 16 bytes; `FRAME` bytes below that
    // end are the trampoline's words, and the child's `sp` is their start.
    let top = base.wrapping_add(len).map_addr(|addr| addr & !15);
    let sp = top.wrapping_sub(FRAME);
    // SAFETY: `len >= MIN_STACK` (far over `FRAME + 15`), so `sp .. top` is inside `stack`,
    // which this call borrows mutably; `sp` is 16-aligned, so the word at `sp` and the
    // `Launch<T>` at `sp + 16` (align 8) are aligned. Plain writes of initialized values into
    // uninitialized bytes.
    unsafe {
        sp.cast::<usize>().write((launch::<T> as unsafe extern "C" fn(*const u8) -> i32) as usize);
        sp.add(16).cast::<Launch<T>>().write(Launch { entry, arg: core::ptr::from_mut(arg) });
    }
    let sp_addr = sp.expose_provenance();
    let before = rt_sigprocmask(MaskHow::Set, SigSet(u64::MAX)).map_err(StartFailed::Mask)?;
    let mut pidfd: i32 = -1;
    let result = match form {
        StartProcess::Clone3 => {
            let args = CloneArgs {
                flags: CLONE_VM | CLONE_VFORK | CLONE_PIDFD,
                pidfd: core::ptr::from_mut(&mut pidfd).expose_provenance() as u64,
                exit_signal: SIGCHLD,
                stack: base.expose_provenance() as u64,
                // `sp >= base` (above): the kernel starts the child at `stack + stack_size`,
                // which is `sp`.
                stack_size: sp_addr.wrapping_sub(base.addr()) as u64,
                ..CloneArgs::default()
            };
            // SAFETY: `args` is a live `clone_args` of the size passed (88 bytes, its fields
            // past VER0 zero, which a 5.3 kernel accepts); `pidfd` is a live `int` the kernel
            // writes; the child starts at `sp` with the trampoline's words written above, and
            // with CLONE_VFORK this thread waits until it no longer uses `stack`, `arg` or
            // `args`. The caller keeps `entry`'s contract.
            unsafe {
                vfork_call(
                    core::ptr::from_ref(&args).expose_provenance(),
                    size_of::<CloneArgs>(),
                    0,
                    0,
                    0,
                    nr::CLONE3,
                )
            }
        }
        StartProcess::Clone => {
            // SAFETY: as `Clone3`'s; the pidfd is written through the third argument,
            // `parent_tid`, on x86-64 and aarch64 alike (module doc); the fourth and fifth
            // (child tid, tls) are 0, unused without their flags.
            unsafe {
                vfork_call(
                    (CLONE_VM | CLONE_VFORK | CLONE_PIDFD | SIGCHLD) as usize,
                    sp_addr,
                    core::ptr::from_mut(&mut pidfd).expose_provenance(),
                    0,
                    0,
                    nr::CLONE,
                )
            }
        }
    };
    let restored = rt_sigprocmask(MaskHow::Set, before);
    let pid = match raw::check(result) {
        Ok(pid) => pid as i32,
        // No child: the mask's restore matters alone, and its failure is the error to name.
        Err(errno) => return Err(restored.err().map_or(StartFailed::Clone(errno), StartFailed::Mask)),
    };
    // SAFETY: a live local the kernel wrote through the address the call was given, which the
    // compiler cannot see; volatile, so it is read after the call.
    let pidfd = unsafe { core::ptr::read_volatile(&raw const pidfd) };
    if pidfd < 0 {
        return Err(StartFailed::NoPidfd { pid });
    }
    // SAFETY: the kernel just opened `pidfd` for this call, and nothing else owns it.
    let launched = Launched { pid, pidfd: unsafe { OwnedFd::from_raw(pidfd) } };
    match restored {
        Ok(_) => Ok(launched),
        Err(errno) => Err(StartFailed::MaskNotRestored { errno, launched }),
    }
}

#[cfg(test)]
mod tests {
    use core::mem::MaybeUninit;

    use super::{ChildEntry, MIN_STACK, StartFailed, start_on_stack};
    use crate::Path;
    use crate::linux::probe::KernelFacts;
    use crate::linux::slot::StartProcess;
    use crate::linux::wait::SigSet;
    use crate::linux::{
        Arg, Args, AtFlags, CWD, Children, Ended, MaskHow, WaitOptions, Waited, execveat, exit_group,
        rt_sigprocmask, waitid,
    };

    /// What the child writes, for the parent to read after the call.
    struct Record {
        value: u64,
    }

    /// The child's entry: writes the record, then ends by raw `exit_group(7)`.
    fn write_and_exit(record: &mut Record) -> i32 {
        record.value = 0x0b1a_2e00;
        exit_group(7)
    }

    /// The child's entry: writes the record and returns 9, which the trampoline's
    /// `exit_group` makes the status.
    fn write_and_return(record: &mut Record) -> i32 {
        record.value = 9;
        9
    }

    /// A 64 KiB stack, its bytes unwritten.
    fn stack() -> Vec<MaybeUninit<u8>> {
        vec![MaybeUninit::uninit(); 64 << 10]
    }

    /// Every form this kernel has runs the entry on the lent stack: the parent sees the record
    /// written, its mask as before, and the exit status by the pidfd.
    #[test]
    fn every_form_runs_the_entry_on_its_stack() {
        let facts = KernelFacts::probe().unwrap();
        let mut ran = 0;
        for form in StartProcess::ALL {
            if !form.available(&facts) {
                println!("{form:?}: not on this kernel");
                continue;
            }
            let cases: [(ChildEntry<Record>, i32, u64); 2] =
                [(write_and_exit, 7, 0x0b1a_2e00), (write_and_return, 9, 9)];
            for (entry, code, value) in cases {
                let mut stack = stack();
                let mut record = Record { value: 0 };
                let mask = rt_sigprocmask(MaskHow::Block, SigSet::EMPTY).unwrap();
                // SAFETY: both entries store into the record and make one raw system call.
                let launched = unsafe { start_on_stack(form, &mut stack, entry, &mut record) }.unwrap();
                assert_eq!(rt_sigprocmask(MaskHow::Block, SigSet::EMPTY).unwrap(), mask, "{form:?}: mask restored");
                assert_eq!(record.value, value, "{form:?}: the child's write");
                let waited = waitid(Children::Pidfd(launched.pidfd.fd()), WaitOptions::EXITED);
                assert_eq!(waited, Ok(Some(Waited { pid: launched.pid, ended: Ended::Exited(code) })), "{form:?}");
            }
            println!("{form:?}: ran");
            ran += 1;
        }
        assert!(ran > 0, "no form of starting a process on this kernel");
    }

    /// What the exec child reads and writes.
    struct Exec<'a> {
        argv: Args<'a>,
        envp: Args<'a>,
        /// The exec's error, written only if it was refused.
        errno: u16,
    }

    /// The child's entry: `execveat` of `/bin/sh -c "exit 0"` (`/bin/sh` is on every Linux
    /// this runs on, NixOS included, where `/bin/true` is not); 127 and the errno if refused.
    /// The mask stays all-blocked: `sh` runs `exit 0` whatever it is.
    fn exec_sh(exec: &mut Exec<'_>) -> i32 {
        let errno = execveat(CWD, Path::new(c"/bin/sh"), exec.argv, exec.envp, AtFlags::NONE);
        exec.errno = errno.raw();
        127
    }

    /// Every form's child can exec a program, whose status the parent waits for.
    #[test]
    fn every_form_execs_a_program() {
        let facts = KernelFacts::probe().unwrap();
        let argv = [Arg::new(c"/bin/sh"), Arg::new(c"-c"), Arg::new(c"exit 0"), Arg::END];
        let envp = [Arg::END];
        for form in StartProcess::ALL {
            if !form.available(&facts) {
                println!("{form:?}: not on this kernel");
                continue;
            }
            let mut stack = stack();
            let mut exec = Exec { argv: Args::new(&argv).unwrap(), envp: Args::new(&envp).unwrap(), errno: 0 };
            // SAFETY: the entry makes one raw system call (or two: `execveat`, then the
            // trampoline's `exit_group`) and one store into the record.
            let launched = unsafe { start_on_stack(form, &mut stack, exec_sh, &mut exec) }.unwrap();
            let waited = waitid(Children::Pidfd(launched.pidfd.fd()), WaitOptions::EXITED);
            assert_eq!(exec.errno, 0, "{form:?}: the exec was refused");
            assert_eq!(waited, Ok(Some(Waited { pid: launched.pid, ended: Ended::Exited(0) })), "{form:?}");
            println!("{form:?}: exec'd /bin/sh");
        }
    }

    /// A stack under the minimum is refused before any call.
    #[test]
    fn a_small_stack_is_refused_before_any_call() {
        let mut small = vec![MaybeUninit::uninit(); MIN_STACK - 16];
        let mut record = Record { value: 0 };
        // SAFETY: refused before any call; the entry never runs.
        let refused = unsafe { start_on_stack(StartProcess::Clone, &mut small, write_and_return, &mut record) };
        assert!(matches!(refused, Err(StartFailed::StackTooSmall { len }) if len == MIN_STACK - 16));
        assert_eq!(record.value, 0);
    }
}
