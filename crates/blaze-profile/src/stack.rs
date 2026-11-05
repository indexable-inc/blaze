//! Walking a thread's frame pointers: the one walker the CPU samplers (macOS and Linux), the
//! panic hook ([`crate::panics`]) and the fatal-signal handler ([`crate::crash`]) share. blaze
//! keeps frame pointers in every frame (arm64 macOS always does), so each frame record is two
//! words, `[fp] = caller's fp, [fp + 8] = return address`, and a walk follows them up the stack.
//!
//! A walk reads each record through its caller's `read`, which gives the record's two words or
//! says it is not readable. The macOS sampler knows the stopped thread's stack bounds and loads
//! a record inside them ([`load`]); on Linux the kernel walks a sampled thread's frame pointers
//! itself (`perf_event_open`'s user call chains); the panic hook and the signal handler know no
//! bounds (a job runs on a fiber's stack, a signal's handler on the thread's alternate stack),
//! so they ask [`probe`] first ([`probed`]): the kernel copies the 16 bytes of a frame record
//! into a pipe, and refuses with `EFAULT` where they are not readable (unmapped, or a guard
//! page), so a wild frame pointer ends the walk instead of faulting. Every step is a system call or a plain
//! load: async-signal-safe, no lock, no allocation.

use std::sync::atomic::{AtomicI32, Ordering};

pub(crate) use crate::sampler::DEPTH;

/// User addresses: below 2^47 on arm64 macOS and x86_64, 2^48 on arm64 Linux; above are tag
/// bits (pointer authentication).
#[cfg(target_vendor = "apple")]
pub(crate) const ADDRESS: u64 = (1 << 47) - 1;
#[cfg(not(target_vendor = "apple"))]
pub(crate) const ADDRESS: u64 = (1 << 48) - 1;

/// What a frame pointer is aligned to: 16 on arm64 (the ABI's stack alignment), 8 elsewhere.
#[cfg(target_arch = "aarch64")]
const ALIGN: u64 = 16;
#[cfg(not(target_arch = "aarch64"))]
const ALIGN: u64 = 8;

/// The most frame records a walk follows, a bound past [`DEPTH`] for a `push` that skips some.
const MOST: usize = 256;

/// Follows the frame records from `fp` up the stack, giving each return address to `push`
/// (masked to [`ADDRESS`]) until `push` says stop, `read` gives no record at an address (its
/// two words: the caller's frame pointer, the return address), a record is misaligned, the
/// next record is not above this one, or [`MOST`] records were read.
pub(crate) fn walk(
    fp: u64,
    read: &mut dyn FnMut(u64) -> Option<[u64; 2]>,
    push: &mut dyn FnMut(u64) -> bool,
) {
    let mut fp = fp & ADDRESS;
    for _ in 0..MOST {
        if fp == 0 || fp % ALIGN != 0 {
            return;
        }
        let Some([next, ret]) = read(fp) else { return };
        let (next, ret) = (next & ADDRESS, ret & ADDRESS);
        if ret == 0 || !push(ret) || next <= fp {
            return;
        }
        fp = next;
    }
}

/// The frame record at `fp`, loaded in place.
///
/// # Safety
///
/// The 16 bytes at `fp` are mapped readable for the call, and `fp` is 8-byte aligned (as
/// [`walk`] checks before it reads).
pub(crate) unsafe fn load(fp: u64) -> [u64; 2] {
    let record = fp as *const u64;
    // SAFETY: the caller's: the record's two words are mapped readable and aligned.
    unsafe { [record.read_volatile(), record.wrapping_add(1).read_volatile()] }
}

/// The frame record at `fp` when [`probe`] vouches for it: for a walk that knows no stack bounds.
pub(crate) fn probed(fp: u64) -> Option<[u64; 2]> {
    // SAFETY: [`probe`] had the kernel copy the record's 16 bytes just now, and a stack is not
    // unmapped under the thread walking itself (the panic hook and the signal handler walk their
    // own thread).
    probe(fp).then(|| unsafe { load(fp) })
}

/// The calling function's frame pointer: where a walk of the calling thread begins. Inlined, so
/// it is the frame of the function that calls it.
#[inline(always)]
pub(crate) fn frame_pointer() -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        let fp: u64;
        // SAFETY: reads a register; no memory, no stack, no flags.
        unsafe {
            std::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack, preserves_flags))
        };
        fp
    }
    #[cfg(target_arch = "x86_64")]
    {
        let fp: u64;
        // SAFETY: as above.
        unsafe {
            std::arch::asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack, preserves_flags))
        };
        fp
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        0
    }
}

/// The pipe [`probe`] writes to and drains, `-1` before [`ready`] made it: atomics, as a signal
/// handler reads them.
static PIPE: [AtomicI32; 2] = [AtomicI32::new(-1), AtomicI32::new(-1)];

/// Makes [`probe`]'s pipe, once a process: non-blocking both ends (a full pipe refuses a
/// probe, never blocks it) and closed on exec. Whether it is there; a probe without it says
/// nothing is readable, so a walk records only the registers it began from.
#[cfg(unix)]
pub(crate) fn ready() -> bool {
    static MADE: std::sync::Once = std::sync::Once::new();
    MADE.call_once(|| {
        let mut fds = [0; 2];
        // SAFETY: `fds` is live for the call to fill with two descriptors.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return;
        }
        for fd in fds {
            // SAFETY: integer calls on a descriptor just made, owned by this module.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            // SAFETY: as above.
            let set = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } != -1
                // SAFETY: as above.
                && unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != -1;
            if !set {
                return;
            }
        }
        PIPE[0].store(fds[0], Ordering::Release);
        PIPE[1].store(fds[1], Ordering::Release);
    });
    PIPE[1].load(Ordering::Acquire) >= 0
}

#[cfg(not(unix))]
pub(crate) fn ready() -> bool {
    false
}

/// Whether the 16 bytes at `at` are readable now: the kernel copies them into [`ready`]'s pipe,
/// or refuses (`EFAULT` for memory not mapped readable, `EAGAIN` for a full pipe), and what it
/// copied is drained. Two system calls, async-signal-safe; two threads probing at once may
/// drain each other's bytes, which nothing reads.
#[cfg(unix)]
pub(crate) fn probe(at: u64) -> bool {
    let (read, write) = (PIPE[0].load(Ordering::Acquire), PIPE[1].load(Ordering::Acquire));
    if read < 0 || write < 0 {
        return false;
    }
    // SAFETY: `write` copies from `at` in the kernel, which checks the range and fails with
    // `EFAULT` where it is not readable: this process's memory is never touched by user code.
    let wrote = unsafe { libc::write(write, at as usize as *const libc::c_void, 16) };
    let mut drained = [0u8; 16];
    if wrote > 0 {
        // SAFETY: `drained` is live for the call and 16 bytes long.
        unsafe { libc::read(read, drained.as_mut_ptr().cast(), 16) };
    }
    wrote == 16
}

#[cfg(not(unix))]
pub(crate) fn probe(_at: u64) -> bool {
    false
}

/// The return addresses of the calling thread from its caller up, innermost first, into `pcs`;
/// how many. For the panic hook: [`probe`] vouches for each record.
#[inline(always)]
pub(crate) fn here(pcs: &mut [u64; DEPTH]) -> usize {
    let mut depth = 0;
    if !ready() {
        return 0;
    }
    walk(frame_pointer(), &mut probed, &mut |pc| {
        pcs[depth] = pc;
        depth += 1;
        depth < DEPTH
    });
    depth
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    fn walked_from_here() -> (usize, [u64; DEPTH]) {
        let mut pcs = [0; DEPTH];
        let depth = here(&mut pcs);
        std::hint::black_box((depth, pcs))
    }

    #[inline(never)]
    fn a_caller_to_find() -> (usize, [u64; DEPTH]) {
        std::hint::black_box(walked_from_here())
    }

    /// The walk of the calling thread names its callers.
    #[cfg(unix)]
    #[test]
    fn a_walk_of_this_thread_finds_its_callers() {
        let (depth, pcs) = a_caller_to_find();
        assert!(depth >= 2, "{depth} frames");
        let frames = crate::symbols::frames(std::iter::once(&pcs[..depth]));
        let found = pcs[..depth].iter().enumerate().any(|(i, pc)| {
            frames[&crate::symbols::lookup(i, *pc)].iter().any(|it| it.contains("a_caller_to_find"))
        });
        assert!(found, "{frames:?}");
    }

    /// A frame pointer to memory that is not mapped ends the walk, with no fault.
    #[cfg(unix)]
    #[test]
    fn a_wild_frame_pointer_ends_the_walk() {
        assert!(ready());
        let mut pushed = 0;
        walk(0x10, &mut probed, &mut |_| {
            pushed += 1;
            true
        });
        assert_eq!(pushed, 0);
        assert!(!probe(0x1000), "the zero page is not readable");
        let word = [0u64; 2];
        assert!(probe(std::ptr::from_ref(&word).addr() as u64));
    }
}
