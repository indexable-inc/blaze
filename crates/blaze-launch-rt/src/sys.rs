//! What the launcher asks of the OS: memory that code runs from and that one thread writes, a
//! thread's stack bounds and its frame pointer.
//!
//! [`Code`] is one mapping: a launcher has one, which holds its slot words, its lazy stubs
//! and its code area (`drive` says why they are one).
//!
//! - **macOS** (AArch64): `mmap(MAP_JIT)`, readable, writable and executable in name; which of
//!   write and execute a thread has is the thread's own switch, `pthread_jit_write_protect_np`.
//!   [`Code::write`] turns the calling thread's to write for the length of one copy and back,
//!   so every other thread keeps running the area and never sees it writable. Then
//!   `sys_icache_invalidate`. All three are public (`<sys/mman.h>`, `<pthread.h>`,
//!   `<libkern/OSCacheControl.h>`); they are declared here rather than taken from the `libc`
//!   crate so that [`SYMBOLS`] lists exactly what this file calls, and a test resolves each at
//!   run time (rule `macos-private-apis`).
//! - **Linux**: a `memfd` mapped twice, read and execute where code runs and read and write at
//!   an address only [`Code`] knows. No page is ever writable and executable at one address,
//!   and nothing is `mprotect`ed. On AArch64 a write is followed by the cache maintenance
//!   (`dc cvau`, `ic ivau`) and by `membarrier(PRIVATE_EXPEDITED_SYNC_CORE)`, so no core runs
//!   stale instructions once [`Code::write`] returns.
//!
//! The invariant every `unsafe` block here rests on, kept by [`Code`]'s owner (the `Driver`,
//! which the control thread owns): **one thread writes a mapping, and it writes only bytes no
//! thread can be running or reading as code** (a range the core handed out and no word names
//! yet), or one aligned word through an atomic store ([`Code::store`]). Program threads only
//! execute and load.

use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU64, Ordering};

/// A call of the OS that failed: which, its `errno`, and the bytes of the mapping it was about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SysError {
    pub call: &'static str,
    pub errno: i32,
    pub at: usize,
    pub len: usize,
}

impl core::fmt::Display for SysError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let SysError { call, errno, at, len } = *self;
        write!(f, "{call} failed with errno {errno} (offset {at}, {len} bytes)")
    }
}

impl core::error::Error for SysError {}

/// The error of `call`, from `errno`.
fn failed(call: &'static str, at: usize, len: usize) -> SysError {
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    SysError { call, errno, at, len }
}

/// The page size.
pub fn page() -> Result<usize, SysError> {
    // SAFETY: `sysconf` reads a constant of the system.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(size).ok().filter(|it| it.is_power_of_two()).ok_or(SysError {
        call: "sysconf(_SC_PAGESIZE)",
        errno: 0,
        at: 0,
        len: 0,
    })
}

/// One mapping that code runs from. See the module's documentation for who may write it.
///
/// **A mapping is never unmapped**: there is no `Drop`. A launcher's code lives as long as
/// its process, and a thread may be in it, or hold an address of it, at any time; so an
/// address in a mapping is good for the life of the process whatever happens to the `Code`
/// that made it, and nothing has to prove that every thread left before a value is dropped.
/// What a `Code` gives back is memory ([`Code::decommit`]), never addresses.
pub struct Code {
    /// Where code runs.
    exec: NonNull<u8>,
    /// Where the same pages are writable.
    #[cfg(target_os = "linux")]
    write: NonNull<u8>,
    /// The memfd, open for the life of the process, as its mappings are.
    #[cfg(target_os = "linux")]
    fd: std::os::fd::OwnedFd,
    len: usize,
}

// SAFETY: a `Code` is the sole owner of its mappings (and its descriptor); nothing in it is
// tied to the thread that made it. On macOS the write switch is taken and given back inside
// each `write` and `store`, on whichever thread calls them.
unsafe impl Send for Code {}

impl Code {
    /// Where code runs: the address of offset 0.
    pub fn base(&self) -> usize {
        self.exec.as_ptr() as usize
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// `at..at + len` must be in the mapping.
    fn within(&self, call: &'static str, at: usize, len: usize) -> Result<(), SysError> {
        match at.checked_add(len) {
            Some(end) if end <= self.len => Ok(()),
            // EFAULT, as the kernel would say of an address outside a mapping.
            _ => Err(SysError { call, errno: libc::EFAULT, at, len }),
        }
    }

    /// Stores `word` at `at`, which is a multiple of 8, as one atomic store: the flag of a
    /// gate, which program threads load while this runs.
    pub fn store(&mut self, at: usize, word: u64) -> Result<(), SysError> {
        self.within("store", at, 8)?;
        if at % 8 != 0 {
            return Err(SysError { call: "store", errno: libc::EINVAL, at, len: 8 });
        }
        self.writable(|to| {
            // SAFETY: `to + at` is in the mapping (`within`), aligned for a `u64` (the mapping
            // is page aligned and `at` a multiple of 8), writable on this thread (`writable`),
            // and lives as long as `self`. Other threads only load it, as machine code does:
            // one aligned 8-byte load, which an atomic store is sound against.
            let cell = unsafe { AtomicU64::from_ptr(to.add(at).cast::<u64>()) };
            cell.store(word, Ordering::SeqCst);
        });
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use core::ffi::{c_int, c_void};

    /// `MAP_JIT` (`<sys/mman.h>`): the mapping's write and execute are switched a thread.
    pub(super) const MAP_JIT: c_int = 0x0800;
    /// `MADV_FREE_REUSABLE` (`<sys/mman.h>`): the pages' memory goes back to the system now
    /// and stops counting in the process's footprint, where `MADV_FREE` only marks it.
    pub(super) const MADV_FREE_REUSABLE: c_int = 7;

    unsafe extern "C" {
        /// `<pthread.h>`, macOS 11: whether this thread's `MAP_JIT` pages are executable (1)
        /// or writable (0).
        pub(super) fn pthread_jit_write_protect_np(enabled: c_int);
        /// `<pthread.h>`, macOS 11: whether the switch exists on this machine.
        pub(super) fn pthread_jit_write_protect_supported_np() -> c_int;
        /// `<libkern/OSCacheControl.h>`: the instruction cache sees what was written.
        pub(super) fn sys_icache_invalidate(start: *mut c_void, len: usize);
        /// `<pthread.h>`: the high end of a thread's stack.
        pub(super) fn pthread_get_stackaddr_np(thread: libc::pthread_t) -> *mut c_void;
        /// `<pthread.h>`: the length of a thread's stack.
        pub(super) fn pthread_get_stacksize_np(thread: libc::pthread_t) -> usize;
    }
}

/// The symbols this file calls on macOS beyond POSIX, each with why. None is private; each is
/// specific to macOS or to a version of it, so `tests::every_macos_symbol_resolves` looks
/// each up at run time and fails naming the one that is gone.
#[cfg(target_os = "macos")]
pub const SYMBOLS: [(&core::ffi::CStr, &str); 5] = [
    (
        c"pthread_jit_write_protect_np",
        "the control thread's switch between writing and running MAP_JIT pages; other threads \
         keep running them, where mprotect would stop every thread",
    ),
    (c"pthread_jit_write_protect_supported_np", "whether that switch exists on this machine"),
    (c"sys_icache_invalidate", "the instruction cache sees new code before a word names it"),
    (c"pthread_get_stackaddr_np", "the bounds a safepoint's walk of its own stack stays in"),
    (c"pthread_get_stacksize_np", "as pthread_get_stackaddr_np"),
];

#[cfg(target_os = "macos")]
impl Code {
    /// Maps `len` bytes (a whole number of pages) that code can run from. Nothing is committed
    /// until written.
    pub fn map(len: usize) -> Result<Code, SysError> {
        // SAFETY: asks whether the per-thread switch exists; no arguments.
        if unsafe { macos::pthread_jit_write_protect_supported_np() } == 0 {
            return Err(SysError {
                call: "pthread_jit_write_protect_supported_np",
                errno: libc::ENOTSUP,
                at: 0,
                len,
            });
        }
        let protection = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON | macos::MAP_JIT;
        // SAFETY: an anonymous mapping at no fixed address; the result is checked.
        let at = unsafe { libc::mmap(ptr::null_mut(), len, protection, flags, -1, 0) };
        if at == libc::MAP_FAILED {
            return Err(failed("mmap(MAP_JIT)", 0, len));
        }
        match NonNull::new(at.cast::<u8>()) {
            Some(exec) => Ok(Code { exec, len }),
            None => Err(SysError { call: "mmap(MAP_JIT)", errno: 0, at: 0, len }),
        }
    }

    /// Runs `with` on the mapping's base while this thread may write `MAP_JIT` pages. `with`
    /// must not panic and must not run code of any `MAP_JIT` mapping: for its length this
    /// thread cannot execute them.
    fn writable(&mut self, with: impl FnOnce(*mut u8)) {
        // SAFETY: this thread's `MAP_JIT` pages become writable and stop being executable for
        // it; it runs none until the switch back below (`with` is a copy or a store).
        unsafe {
            macos::pthread_jit_write_protect_np(0)
        };
        with(self.exec.as_ptr());
        // SAFETY: back to executable, the state every thread starts in.
        unsafe {
            macos::pthread_jit_write_protect_np(1)
        };
    }

    /// Copies `bytes` to `at` and makes them runnable. The bytes are ones no thread can be
    /// running (the module's invariant).
    pub fn write(&mut self, at: usize, bytes: &[u8]) -> Result<(), SysError> {
        self.within("write", at, bytes.len())?;
        self.writable(|to| {
            // SAFETY: `to + at .. + bytes.len()` is in the mapping (`within`) and writable on
            // this thread (`writable`); `bytes` is a live slice that cannot overlap it (the
            // mapping is not borrowed as a slice anywhere); no thread reads these bytes (the
            // module's invariant).
            unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), to.add(at), bytes.len()) };
        });
        // SAFETY: the range just written, in the mapping.
        unsafe {
            macos::sys_icache_invalidate(self.exec.as_ptr().add(at).cast(), bytes.len())
        };
        Ok(())
    }

    /// Gives the memory of whole pages back to the system. Their addresses stay reserved and
    /// are never used again (the core hands an offset out once).
    pub fn decommit(&mut self, at: usize, len: usize) -> Result<(), SysError> {
        self.within("madvise(MADV_FREE_REUSABLE)", at, len)?;
        // The kernel answers EAGAIN while it cannot take the pages' lock; a few tries, then
        // the refusal (rule bounded-progress).
        for _ in 0..16 {
            // SAFETY: whole pages of the mapping (`within`; the caller gives page multiples)
            // in which nothing is live: no thread is in them, and no one will read them.
            let done = unsafe {
                libc::madvise(self.exec.as_ptr().add(at).cast(), len, macos::MADV_FREE_REUSABLE)
            };
            if done == 0 {
                return Ok(());
            }
            let why = failed("madvise(MADV_FREE_REUSABLE)", at, len);
            if why.errno != libc::EAGAIN {
                return Err(why);
            }
        }
        Err(SysError { call: "madvise(MADV_FREE_REUSABLE)", errno: libc::EAGAIN, at, len })
    }
}

/// What AArch64 needs after a write and x86-64 does not (there, code that was never run is
/// in no cache that needs telling, and a word is data).
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
mod linux {
    /// `membarrier(2)`: every thread of the process has run a core-serializing instruction
    /// when the call returns, so none runs instructions it fetched before a write.
    pub(super) const PRIVATE_EXPEDITED_SYNC_CORE: libc::c_long = 1 << 5;
    pub(super) const REGISTER_PRIVATE_EXPEDITED_SYNC_CORE: libc::c_long = 1 << 6;

    /// `membarrier(command, 0, 0)`.
    pub(super) fn membarrier(command: libc::c_long) -> bool {
        let zero: libc::c_long = 0;
        // SAFETY: a system call that takes three integers and touches no memory of ours.
        unsafe { libc::syscall(libc::SYS_membarrier, command, zero, zero) == 0 }
    }

    /// The instruction cache of every core sees `start..start + len` as it is in memory.
    pub(super) fn icache(start: usize, len: usize) {
        use core::arch::asm;

        let ctr: u64;
        // SAFETY: Linux lets EL0 read CTR_EL0.
        unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
        let (data, instruction) = (4usize << ((ctr >> 16) & 0xf), 4usize << (ctr & 0xf));
        let end = start + len;
        let mut line = start & !(data - 1);
        while line < end {
            // SAFETY: a cache clean of a mapped, readable address.
            unsafe { asm!("dc cvau, {}", in(reg) line, options(nostack, preserves_flags)) };
            line += data;
        }
        // SAFETY: a barrier.
        unsafe { asm!("dsb ish", options(nostack, preserves_flags)) };
        let mut line = start & !(instruction - 1);
        while line < end {
            // SAFETY: an instruction cache invalidate of a mapped address.
            unsafe { asm!("ic ivau, {}", in(reg) line, options(nostack, preserves_flags)) };
            line += instruction;
        }
        // SAFETY: barriers.
        unsafe { asm!("dsb ish", "isb", options(nostack, preserves_flags)) };
    }
}

#[cfg(target_os = "linux")]
impl Code {
    /// Maps `len` bytes (a whole number of pages) that code can run from. Nothing is committed
    /// until written.
    pub fn map(len: usize) -> Result<Code, SysError> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        // SAFETY: a name that is a C string; the result is checked.
        let fd = unsafe { libc::memfd_create(c"blaze-launch".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(failed("memfd_create", 0, len));
        }
        // SAFETY: a descriptor just made, owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let Ok(length) = libc::off_t::try_from(len) else {
            return Err(SysError { call: "ftruncate", errno: libc::EFBIG, at: 0, len });
        };
        // SAFETY: the memfd above; a sparse length, nothing is committed.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), length) } != 0 {
            return Err(failed("ftruncate", 0, len));
        }
        let map = |protection: libc::c_int, call: &'static str| {
            // SAFETY: a shared mapping of the memfd at no fixed address; the result is checked.
            let at = unsafe {
                libc::mmap(ptr::null_mut(), len, protection, libc::MAP_SHARED, fd.as_raw_fd(), 0)
            };
            if at == libc::MAP_FAILED {
                return Err(failed(call, 0, len));
            }
            NonNull::new(at.cast::<u8>()).ok_or(SysError { call, errno: 0, at: 0, len })
        };
        let exec = map(libc::PROT_READ | libc::PROT_EXEC, "mmap(memfd, read and execute)")?;
        let write = match map(libc::PROT_READ | libc::PROT_WRITE, "mmap(memfd, read and write)") {
            Ok(write) => write,
            Err(why) => {
                // SAFETY: the mapping made two lines up, which nothing has run or named.
                unsafe { libc::munmap(exec.as_ptr().cast(), len) };
                return Err(why);
            }
        };
        let code = Code { exec, write, fd, len };
        // A process registers once before it may ask for the barrier; again is harmless.
        #[cfg(target_arch = "aarch64")]
        if !linux::membarrier(linux::REGISTER_PRIVATE_EXPEDITED_SYNC_CORE) {
            return Err(failed("membarrier(REGISTER_PRIVATE_EXPEDITED_SYNC_CORE)", 0, len));
        }
        Ok(code)
    }

    /// Runs `with` on the base of the writable mapping.
    fn writable(&mut self, with: impl FnOnce(*mut u8)) {
        with(self.write.as_ptr());
    }

    /// Copies `bytes` to `at` and makes them runnable. The bytes are ones no thread can be
    /// running (the module's invariant).
    pub fn write(&mut self, at: usize, bytes: &[u8]) -> Result<(), SysError> {
        self.within("write", at, bytes.len())?;
        self.writable(|to| {
            // SAFETY: `to + at .. + bytes.len()` is in the writable mapping (`within`);
            // `bytes` is a live slice that cannot overlap it (the mapping is not borrowed as
            // a slice anywhere); no thread reads these bytes (the module's invariant).
            unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), to.add(at), bytes.len()) };
        });
        #[cfg(target_arch = "aarch64")]
        {
            linux::icache(self.base() + at, bytes.len());
            if !linux::membarrier(linux::PRIVATE_EXPEDITED_SYNC_CORE) {
                return Err(failed("membarrier(PRIVATE_EXPEDITED_SYNC_CORE)", at, bytes.len()));
            }
        }
        Ok(())
    }

    /// Gives the memory of whole pages back to the system: a hole in the memfd, which takes
    /// the pages out of both mappings. (`MADV_DONTNEED` would not: the pages of a shared
    /// mapping belong to the file.) Their addresses stay reserved and are never used again.
    pub fn decommit(&mut self, at: usize, len: usize) -> Result<(), SysError> {
        use std::os::fd::AsRawFd;

        self.within("fallocate(PUNCH_HOLE)", at, len)?;
        let (Ok(from), Ok(length)) = (libc::off_t::try_from(at), libc::off_t::try_from(len)) else {
            return Err(SysError { call: "fallocate(PUNCH_HOLE)", errno: libc::EFBIG, at, len });
        };
        let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
        // SAFETY: whole pages of the memfd in which nothing is live: no thread is in them, and
        // no one will read them.
        if unsafe { libc::fallocate(self.fd.as_raw_fd(), mode, from, length) } != 0 {
            return Err(failed("fallocate(PUNCH_HOLE)", at, len));
        }
        Ok(())
    }
}

/// The calling thread's stack: its low and its high end.
#[cfg(target_os = "macos")]
pub fn stack() -> Result<(usize, usize), SysError> {
    // SAFETY: the calling thread's own handle.
    let me = unsafe { libc::pthread_self() };
    // SAFETY: a live thread's handle; reads its stack's high end.
    let high = unsafe { macos::pthread_get_stackaddr_np(me) } as usize;
    // SAFETY: a live thread's handle; reads its stack's length.
    let len = unsafe { macos::pthread_get_stacksize_np(me) };
    match high.checked_sub(len) {
        Some(low) if len != 0 => Ok((low, high)),
        _ => Err(SysError { call: "pthread_get_stackaddr_np", errno: 0, at: high, len }),
    }
}

/// The calling thread's stack: its low and its high end.
#[cfg(target_os = "linux")]
pub fn stack() -> Result<(usize, usize), SysError> {
    let mut attr = core::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
    // SAFETY: the calling thread's own handle.
    let me = unsafe { libc::pthread_self() };
    // SAFETY: fills `attr` for a live thread; the result is checked before `attr` is read.
    let refused = unsafe { libc::pthread_getattr_np(me, attr.as_mut_ptr()) };
    if refused != 0 {
        return Err(SysError { call: "pthread_getattr_np", errno: refused, at: 0, len: 0 });
    }
    let mut low = ptr::null_mut();
    let mut len = 0usize;
    // SAFETY: `attr` was filled above; the two outputs are live locals.
    let refused = unsafe { libc::pthread_attr_getstack(attr.as_ptr(), &mut low, &mut len) };
    // SAFETY: `attr` was filled above and is not used again.
    unsafe { libc::pthread_attr_destroy(attr.as_mut_ptr()) };
    let low = low as usize;
    match low.checked_add(len) {
        Some(high) if refused == 0 && len != 0 => Ok((low, high)),
        _ => Err(SysError { call: "pthread_attr_getstack", errno: refused, at: low, len }),
    }
}

/// The frame pointer of the function this is inlined into: the address of its frame record
/// (the caller's frame pointer, then the return address), when the code keeps one.
#[inline(always)]
pub fn frame_pointer() -> usize {
    let at: usize;
    // SAFETY: copies one register; touches no memory, no stack, no flags.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("mov {}, x29", out(reg) at, options(nomem, nostack, preserves_flags))
    };
    // SAFETY: copies one register; touches no memory, no stack, no flags.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("mov {}, rbp", out(reg) at, options(nomem, nostack, preserves_flags))
    };
    at
}

/// Memory the program's data lives in: the constants its code refers to and its statics.
/// Address space reserved once and handed out from its end, as the code area is; readable
/// and writable by every thread, never executable.
///
/// The invariant every `unsafe` block of it rests on, kept by its owner (the records, which
/// the control thread owns): **a range is written only between [`Data::reserve`] handing it
/// out and the first store of a word or a pool cell that names it**, so no program thread
/// can be reading it; after that it is the program's own memory, as a linked image's data
/// is. A range is never handed out twice and the mapping is never unmapped (no `Drop`): the
/// program may keep an address into it for as long as it runs.
pub struct Data {
    base: NonNull<u8>,
    len: usize,
    /// The offset the next range begins at or after.
    end: usize,
}

// SAFETY: a `Data` is the sole owner of its mapping, and nothing in it is tied to a thread.
unsafe impl Send for Data {}

impl Data {
    /// Reserves `len` bytes. Nothing is committed until written.
    pub fn map(len: usize) -> Result<Data, SysError> {
        #[cfg(target_os = "linux")]
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE;
        #[cfg(target_os = "macos")]
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON;
        let protection = libc::PROT_READ | libc::PROT_WRITE;
        // SAFETY: an anonymous mapping at no fixed address; the result is checked.
        let at = unsafe { libc::mmap(ptr::null_mut(), len, protection, flags, -1, 0) };
        if at == libc::MAP_FAILED {
            return Err(failed("mmap(the program's data)", 0, len));
        }
        match NonNull::new(at.cast::<u8>()) {
            Some(base) => Ok(Data { base, len, end: 0 }),
            None => Err(SysError { call: "mmap(the program's data)", errno: 0, at: 0, len }),
        }
    }

    /// Hands out `size` bytes aligned to `align` (a power of two), zeroed, and gives their
    /// address. ENOMEM when the reservation has no room: the program is started again.
    pub fn reserve(&mut self, size: usize, align: usize) -> Result<usize, SysError> {
        let full = SysError { call: "reserving the program's data", errno: libc::ENOMEM, at: self.end, len: size };
        // The base is page aligned, so an aligned offset is an aligned address up to a page;
        // a larger alignment is taken of the address itself.
        let base = self.base.as_ptr() as usize;
        let at = (base + self.end).checked_next_multiple_of(align.max(1)).ok_or(full)? - base;
        let end = at.checked_add(size.max(1)).filter(|end| *end <= self.len).ok_or(full)?;
        self.end = end;
        Ok(base + at)
    }

    /// Copies `bytes` to `address`, a range [`Data::reserve`] handed out that nothing the
    /// program can reach names yet (the type's invariant).
    pub fn fill(&mut self, address: usize, bytes: &[u8]) -> Result<(), SysError> {
        let base = self.base.as_ptr() as usize;
        let inside = address
            .checked_sub(base)
            .and_then(|at| at.checked_add(bytes.len()))
            .is_some_and(|end| end <= self.end);
        if !inside {
            return Err(SysError { call: "filling the program's data", errno: libc::EFAULT, at: address, len: bytes.len() });
        }
        // SAFETY: `address..address + bytes.len()` is inside what `reserve` handed out
        // (checked above), in a mapping that is readable, writable and never unmapped;
        // `bytes` is a live slice that cannot overlap it (the mapping is never borrowed as a
        // slice); no thread reads the range yet (the type's invariant).
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len()) };
        Ok(())
    }
}
