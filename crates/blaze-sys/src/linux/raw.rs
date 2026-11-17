//! The raw system call, per architecture. Private to the layer: nothing outside `linux` can
//! make a call by number.
//!
//! | | instruction | number | arguments | result | clobbered |
//! |---|---|---|---|---|---|
//! | x86-64 | `syscall` | `rax` | `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` | `rax` | `rcx` (the return address), `r11` (the flags) |
//! | aarch64 | `svc 0` | `x8` | `x0` to `x5` | `x0` | nothing else |
//!
//! The kernel keeps every other register, uses its own stack (`nostack`: the red zone is
//! untouched), and on x86-64 restores the flags from `r11` (`preserves_flags`). A result in
//! `-4095..=-1` is an error's number negated ([`check`]); every other value is the call's.

use core::arch::asm;

use crate::Errno;

/// The largest result that is a value: the 4095 above it are errors on every architecture
/// (`IS_ERR_VALUE`). A constant, so the subtraction is done as the crate compiles and
/// [`check`] has no overflow check in it.
const LAST_VALUE: usize = usize::MAX - 4095;

/// The call's value, or the error its result names.
#[inline]
#[blaze::noalloc]
#[blaze::nopanic]
pub(super) fn check(result: usize) -> Result<usize, Errno> {
    if result > LAST_VALUE {
        Err(Errno::from_raw(result.wrapping_neg() as u16))
    } else {
        Ok(result)
    }
}

macro_rules! raw_calls {
    ($($name:ident($($arg:ident),*);)*) => {
        $(
            /// One system call.
            ///
            /// # Safety
            ///
            /// The caller upholds the kernel's contract for call `nr` with these arguments:
            /// every pointer among them is valid for what that call reads or writes.
            #[inline]
            #[blaze::noalloc(trusted = "one system call instruction: the kernel runs on its own stack and calls no code of this process, so no allocator runs")]
            #[blaze::nopanic]
            pub(super) unsafe fn $name(nr: usize, $($arg: usize),*) -> usize {
                let result: usize;
                // SAFETY: the instruction and registers of this architecture's system call
                // ABI (the table above); the kernel clobbers only what is declared, and the
                // caller answers for the call's own contract.
                unsafe { raw_asm!(nr, result, $($arg),*) };
                result
            }
        )*
    };
}

#[cfg(target_arch = "x86_64")]
macro_rules! raw_asm {
    ($nr:ident, $result:ident, $($arg:ident),*) => {
        raw_asm!(@regs $nr, $result, [$($arg),*], ["rdi", "rsi", "rdx", "r10", "r8", "r9"], [])
    };
    (@regs $nr:ident, $result:ident, [$arg:ident $(, $rest:ident)*], [$reg:tt $(, $regs:tt)*], [$($done:tt)*]) => {
        raw_asm!(@regs $nr, $result, [$($rest),*], [$($regs),*], [$($done)* in($reg) $arg,])
    };
    (@regs $nr:ident, $result:ident, [], [$($regs:tt),*], [$($done:tt)*]) => {
        asm!(
            "syscall",
            inlateout("rax") $nr => $result,
            $($done)*
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack, preserves_flags),
        )
    };
}

#[cfg(target_arch = "aarch64")]
macro_rules! raw_asm {
    ($nr:ident, $result:ident, $first:ident $(, $arg:ident)*) => {
        raw_asm!(@regs $nr, $result, [$($arg),*], ["x1", "x2", "x3", "x4", "x5"], [inlateout("x0") $first => $result,])
    };
    // A call with no argument: `x0` is only the result.
    ($nr:ident, $result:ident,) => {
        asm!("svc 0", in("x8") $nr, lateout("x0") $result, options(nostack))
    };
    (@regs $nr:ident, $result:ident, [$arg:ident $(, $rest:ident)*], [$reg:tt $(, $regs:tt)*], [$($done:tt)*]) => {
        raw_asm!(@regs $nr, $result, [$($rest),*], [$($regs),*], [$($done)* in($reg) $arg,])
    };
    (@regs $nr:ident, $result:ident, [], [$($regs:tt),*], [$($done:tt)*]) => {
        asm!("svc 0", in("x8") $nr, $($done)* options(nostack))
    };
}

raw_calls! {
    syscall0();
    syscall1(a);
    syscall2(a, b);
    syscall3(a, b, c);
    syscall4(a, b, c, d);
    syscall5(a, b, c, d, e);
    syscall6(a, b, c, d, e, f);
}
