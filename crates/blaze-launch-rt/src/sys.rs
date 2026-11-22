//! What the launcher asks of the OS: memory that code runs from and that one thread writes, a
//! thread's stack bounds and its frame pointer.
//!
//! [`Code`] is one reservation: a launcher has one, which holds its slot words, its lazy stubs
//! and its code area (`drive` says why they are one), then, in the same reservation, read and
//! write data that code reaches PC-relatively ([`Code::data_base`]).
//!
//! On both systems the code is **two views of the same pages**: read and execute at the code
//! address, where every thread runs and loads, and read and write at an address only [`Code`]
//! holds, which only [`Code`]'s owner writes through. No page is ever writable and executable
//! at one address, no thread's state decides which it is, and nothing is `mprotect`ed after
//! the views are made. (The JITs of V8, JavaScriptCore and LuaJIT document the same layout.)
//!
//! - **Linux**: a `memfd` mapped twice, `MAP_SHARED`. On AArch64 a write is followed by the
//!   cache maintenance (`dc cvau`, `ic ivau`) and by `membarrier(PRIVATE_EXPEDITED_SYNC_CORE)`,
//!   so no core runs stale instructions once [`Code::write`] returns.
//! - **macOS**: anonymous memory (`mach_vm_allocate`) is the write view, and `mach_vm_remap`
//!   with `copy = FALSE` aliases its pages read and execute at the code address. Each view's
//!   *maximum* protection is lowered (`mach_vm_protect` with `set_maximum`), so no `mprotect`
//!   of the process can make the run view writable or the write view executable. After a
//!   write, `sys_icache_invalidate` on the run view's range. Not `MAP_JIT` with
//!   `pthread_jit_write_protect_np`: that switch is per thread and covers every `MAP_JIT` page
//!   of the process, so a program that itself embeds a JIT and calls it would flip the
//!   launcher's pages for its own thread too. Why `mach_vm_remap` and not a Mach memory entry
//!   (`mach_make_memory_entry_64`, as `blaze-inproc`'s memory object is) mapped twice: see
//!   [`Code::map_with_data`]. The calls are declared here rather than taken from the `libc`
//!   crate so that [`SYMBOLS`] lists exactly what this file calls, and a test resolves each at
//!   run time (rule `macos-private-apis`).
//!
//! **Code signing (macOS)**: the pages carry no code signature, so they run only in a process
//! that does not enforce code signing on its map (`CS_ENFORCEMENT` clear). Measured on macOS
//! 27.0.1, Apple silicon, 2026-11-15, with a C probe of this exact sequence: a binary signed
//! ad hoc by the linker runs it; the same binary with the hardened runtime is killed
//! (`SIGKILL`) at the first instruction fetch, with `com.apple.security.cs.allow-jit` too
//! (that entitlement admits only `MAP_JIT` memory); with
//! `com.apple.security.cs.allow-unsigned-executable-memory` it runs. [`Code::map_with_data`]
//! asks `csops` first and refuses with that reason rather than be killed.
//!
//! **Fork**: what a child forked from the program holds of a [`Code`]:
//!
//! | | the run view | the write view | the data |
//! |---|---|---|---|
//! | macOS | `VM_INHERIT_COPY`: a copy-on-write snapshot of the code as it was at the fork; nothing the parent writes or decommits later reaches it | `VM_INHERIT_NONE`: absent | a private copy |
//! | Linux | `MAP_SHARED`: the same pages as the parent's, so the parent's later writes and decommits reach it | `MADV_DONTFORK`: absent | a private copy |
//!
//! So on both a child keeps a run view at the same address and needs no step after the fork
//! (no thread switch to set, nothing to map again), and no child can write the parent's code.
//! The child has no control thread, so it never writes code; `table` stops a child that
//! reaches a function the parent had not placed. What Linux does not give: a child running a
//! body the parent decommits after the fork reads zeros there. A snapshot on Linux needs the
//! written ranges copied into a new memfd before the fork (a `pthread_atfork` prepare handler,
//! as makepad/relax's `fork_prepare` does); not done here.
//!
//! The invariant every `unsafe` block here rests on, kept by [`Code`]'s owner (the `Driver`,
//! which the control thread owns): **one thread writes a mapping, and it writes only bytes no
//! thread can be running or reading as code** (a range the core handed out and no word names
//! yet), or one aligned word through an atomic store ([`Code::store`]). Program threads only
//! execute and load.

use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU64, Ordering};

use blaze_sys::Errno;
#[cfg(target_os = "macos")]
use blaze_sys::posix::thread;

#[cfg(test)]
mod tests;

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
        // A Mach call (macOS) answers a `kern_return_t`, which `errno` then holds.
        let code = if call.starts_with("mach_") { "kern_return" } else { "errno" };
        write!(f, "{call} failed with {code} {errno} (offset {at}, {len} bytes)")
    }
}

impl core::error::Error for SysError {}

/// The error of `call`, from `errno`.
#[cfg(any(target_os = "macos", all(target_os = "linux", target_arch = "aarch64")))]
fn failed(call: &'static str, at: usize, len: usize) -> SysError {
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    SysError { call, errno, at, len }
}

/// The page size.
pub fn page() -> Result<usize, SysError> {
    #[cfg(target_os = "macos")]
    let size = thread::sysconf(thread::_SC_PAGESIZE);
    // SAFETY: `sysconf` reads a constant of the system.
    #[cfg(target_os = "linux")]
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(size).ok().filter(|it| it.is_power_of_two()).ok_or(SysError {
        call: "sysconf(_SC_PAGESIZE)",
        errno: 0,
        at: 0,
        len: 0,
    })
}

/// One reservation that code runs from, code then data. See the module's documentation for
/// who may write it.
///
/// ```text
/// | code: the run view (read, execute); its pages also at `write` (read, write) | data (read, write) |
/// base                                                                           base + len            base + len + data
/// ```
///
/// **A mapping is never unmapped**: there is no `Drop`. A launcher's code lives as long as
/// its process, and a thread may be in it, or hold an address of it, at any time; so an
/// address in a mapping is good for the life of the process whatever happens to the `Code`
/// that made it, and nothing has to prove that every thread left before a value is dropped.
/// What a `Code` gives back is memory ([`Code::decommit`]), never addresses.
pub struct Code {
    /// Where code runs: the reservation's first byte.
    exec: NonNull<u8>,
    /// Where the same pages are writable: an address only this `Code` holds.
    write: NonNull<u8>,
    /// The memfd, open for the life of the process, as its mappings are.
    #[cfg(target_os = "linux")]
    fd: blaze_sys::linux::OwnedFd,
    /// The code's bytes, a whole number of pages.
    len: usize,
    /// The data's bytes, from `exec + len`.
    data: usize,
}

// SAFETY: a `Code` is the sole owner of its mappings (and its descriptor); nothing in it is
// tied to the thread that made it: which view is writable is the view's, not a thread's.
unsafe impl Send for Code {}

impl Code {
    /// Maps `len` bytes (a whole number of pages) that code can run from, and no data.
    /// Nothing is committed until written.
    pub fn map(len: usize) -> Result<Code, SysError> {
        Code::map_with_data(len, 0)
    }

    /// Where code runs: the address of offset 0.
    pub fn base(&self) -> usize {
        self.exec.as_ptr() as usize
    }

    /// The code's bytes: what [`Code::write`], [`Code::store`] and [`Code::decommit`] take
    /// offsets in.
    pub fn len(&self) -> usize {
        self.len
    }

    /// The address of the data's first byte: right after the code's last page, in the same
    /// reservation, so code reaches it PC-relatively (`adrp`, `[rip + disp32]`) whatever the
    /// system placed the reservation at. Readable and writable by every thread, never
    /// executable, zero until written; its owner writes it as [`Data`]'s owner writes that.
    pub fn data_base(&self) -> usize {
        self.base() + self.len
    }

    /// The data's bytes.
    pub fn data_len(&self) -> usize {
        self.data
    }

    /// `len` and `data` are whole pages, and the reservation's length is a `usize`: the views
    /// must cover exactly the code's pages, or the run view would reach over the data.
    fn layout(len: usize, data: usize) -> Result<usize, SysError> {
        let page = page()?;
        let whole = len.checked_add(data);
        match whole {
            Some(whole) if len != 0 && len % page == 0 && data % page == 0 => Ok(whole),
            _ => Err(SysError { call: "mapping code", errno: Errno::Inval.raw().into(), at: 0, len }),
        }
    }

    /// `at..at + len` must be in the mapping.
    fn within(&self, call: &'static str, at: usize, len: usize) -> Result<(), SysError> {
        match at.checked_add(len) {
            Some(end) if end <= self.len => Ok(()),
            // EFAULT, as the kernel would say of an address outside a mapping.
            _ => Err(SysError { call, errno: Errno::Fault.raw().into(), at, len }),
        }
    }

    /// Stores `word` at `at`, which is a multiple of 8, as one atomic store: the flag of a
    /// gate, which program threads load while this runs.
    pub fn store(&mut self, at: usize, word: u64) -> Result<(), SysError> {
        self.within("store", at, 8)?;
        if at % 8 != 0 {
            return Err(SysError { call: "store", errno: Errno::Inval.raw().into(), at, len: 8 });
        }
        // SAFETY: `write + at` is in the write view (`within`), aligned for a `u64` (the view
        // is page aligned and `at` a multiple of 8), writable, and lives as long as `self`
        // (never unmapped). Other threads only load the word, through the run view, as
        // machine code does: one aligned 8-byte load of the same physical bytes, which an
        // atomic store is sound against.
        let cell = unsafe { AtomicU64::from_ptr(self.write.as_ptr().add(at).cast::<u64>()) };
        cell.store(word, Ordering::SeqCst);
        Ok(())
    }

    /// Copies `bytes` to `at` through the write view, without telling the instruction
    /// caches.
    fn put(&mut self, at: usize, bytes: &[u8]) -> Result<(), SysError> {
        self.within("write", at, bytes.len())?;
        // SAFETY: `write + at .. + bytes.len()` is in the write view (`within`); `bytes` is a
        // live slice that cannot overlap it (the view is not borrowed as a slice anywhere, and
        // only this `Code` knows its address); no thread reads these bytes (the module's
        // invariant).
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), self.write.as_ptr().add(at), bytes.len()) };
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use core::ffi::{c_int, c_void};

    use super::{Errno, SysError, thread};

    /// A Mach port name (`mach_port_t`).
    pub(super) type Port = u32;
    pub(super) const KERN_SUCCESS: c_int = 0;
    pub(super) const MACH_PORT_NULL: Port = 0;
    /// `vm_prot_t` bits (`<mach/vm_prot.h>`).
    pub(super) const VM_PROT_NONE: c_int = 0;
    pub(super) const VM_PROT_READ: c_int = 1;
    pub(super) const VM_PROT_WRITE: c_int = 2;
    pub(super) const VM_PROT_EXECUTE: c_int = 4;
    /// `<mach/vm_statistics.h>`.
    pub(super) const VM_FLAGS_FIXED: c_int = 0;
    pub(super) const VM_FLAGS_ANYWHERE: c_int = 1;
    pub(super) const VM_FLAGS_OVERWRITE: c_int = 0x4000;
    /// `vm_inherit_t` (`<mach/vm_inherit.h>`).
    pub(super) const VM_INHERIT_COPY: u32 = 1;
    pub(super) const VM_INHERIT_NONE: u32 = 2;
    /// `csops` (`<sys/codesign.h>`): the status operation and its enforcement flag.
    pub(super) const CS_OPS_STATUS: u32 = 0;
    pub(super) const CS_ENFORCEMENT: u32 = 0x1000;

    unsafe extern "C" {
        /// `<mach/mach_init.h>`: this task's own port; a send right the task always holds.
        pub(super) fn mach_task_self() -> Port;
        /// `<mach/mach_vm.h>`: zero-filled anonymous memory.
        pub(super) fn mach_vm_allocate(task: Port, address: *mut u64, size: u64, flags: c_int) -> c_int;
        /// `<mach/mach_vm.h>`: unmaps a range.
        pub(super) fn mach_vm_deallocate(task: Port, address: u64, size: u64) -> c_int;
        /// `<mach/mach_vm.h>`: maps the pages of `source_address..` again at `address`;
        /// `copy` = 0 shares them, and the new mapping's current and maximum protection are
        /// the source's, given back through the two pointers.
        pub(super) fn mach_vm_remap(
            target: Port,
            address: *mut u64,
            size: u64,
            mask: u64,
            flags: c_int,
            source: Port,
            source_address: u64,
            copy: c_int,
            current: *mut c_int,
            maximum: *mut c_int,
            inheritance: u32,
        ) -> c_int;
        /// `<mach/mach_vm.h>`: sets a range's current protection, or with `set_maximum` its
        /// maximum (which also lowers the current to it).
        pub(super) fn mach_vm_protect(
            task: Port,
            address: u64,
            size: u64,
            set_maximum: c_int,
            protection: c_int,
        ) -> c_int;
        /// `<mach/mach_vm.h>`: what a child forked from this task gets of a range.
        pub(super) fn mach_vm_inherit(task: Port, address: u64, size: u64, inheritance: u32) -> c_int;
        /// `<mach/mach_vm.h>`: maps a memory object, or with `MACH_PORT_NULL` fresh anonymous
        /// memory, here with `VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE` over what is at `address`.
        pub(super) fn mach_vm_map(
            target: Port,
            address: *mut u64,
            size: u64,
            mask: u64,
            flags: c_int,
            object: Port,
            offset: u64,
            copy: c_int,
            current: c_int,
            maximum: c_int,
            inheritance: u32,
        ) -> c_int;
        /// libsystem_kernel's code-signing status call (`<sys/codesign.h>`, not in the public
        /// SDK's documentation, stable since 10.5; what `codesign` and the kernel's own tests
        /// ask): whether this process enforces code signing on its map.
        pub(super) fn csops(pid: c_int, ops: u32, out: *mut c_void, size: usize) -> c_int;
        /// `<libkern/OSCacheControl.h>`: the instruction cache sees what was written
        /// (`dc cvau`, `dsb ish`, `ic ivau`, `dsb ish`, `isb`; nothing on x86-64).
        pub(super) fn sys_icache_invalidate(start: *mut c_void, len: usize);
        /// `<pthread.h>`: the high end of a thread's stack.
        pub(super) fn pthread_get_stackaddr_np(thread: thread::pthread_t) -> *mut c_void;
        /// `<pthread.h>`: the length of a thread's stack.
        pub(super) fn pthread_get_stacksize_np(thread: thread::pthread_t) -> usize;
    }

    pub(super) fn task() -> Port {
        // SAFETY: no preconditions; names this task's own port.
        unsafe { mach_task_self() }
    }

    /// The `kern_return_t` `code` of `call` about `at..at + len`, as a result.
    pub(super) fn kern(call: &'static str, code: c_int, at: usize, len: usize) -> Result<(), SysError> {
        if code == KERN_SUCCESS { Ok(()) } else { Err(SysError { call, errno: code, at, len }) }
    }

    /// Replaces `len` bytes at `address` with fresh anonymous memory no access reaches
    /// (`VM_PROT_NONE`, not inherited): the pages that were there go back to the system, and
    /// the addresses stay taken, so the system places nothing else there.
    pub(super) fn cover(call: &'static str, address: u64, at: usize, len: usize) -> Result<(), SysError> {
        let mut placed = address;
        // SAFETY: a fixed mapping over `address..address + len`, which the caller owns and in
        // which nothing is live (`Code::decommit`'s contract); `placed` is a live in-out
        // pointer; anonymous memory (`MACH_PORT_NULL`) replaces only those pages.
        let code = unsafe {
            mach_vm_map(
                task(),
                &raw mut placed,
                len as u64,
                0,
                VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE,
                MACH_PORT_NULL,
                0,
                0,
                VM_PROT_NONE,
                VM_PROT_NONE,
                VM_INHERIT_NONE,
            )
        };
        kern(call, code, at, len)?;
        if placed != address {
            return Err(SysError { call, errno: Errno::Fault.raw().into(), at, len });
        }
        Ok(())
    }

    /// This process does not enforce code signing on its map, so pages with no signature run
    /// in it (the module's documentation says what was measured). Refused with the reason,
    /// rather than the kernel killing the process at the first instruction it fetches.
    pub(super) fn unsigned_code_runs(len: usize) -> Result<(), SysError> {
        let mut flags: u32 = 0;
        let pid = thread::getpid();
        // SAFETY: `CS_OPS_STATUS` writes this process's 4-byte status to `flags`, which is a
        // live local of that size.
        let asked = unsafe { csops(pid, CS_OPS_STATUS, (&raw mut flags).cast(), size_of::<u32>()) };
        if asked != 0 {
            return Err(super::failed("csops(CS_OPS_STATUS)", 0, len));
        }
        if flags & CS_ENFORCEMENT != 0 {
            return Err(SysError {
                call: "csops: this process enforces code signing (the hardened runtime), so the kernel \
                       would kill it at the code area's first unsigned page; sign the launcher ad hoc \
                       without the hardened runtime, or with \
                       com.apple.security.cs.allow-unsigned-executable-memory (allow-jit admits only \
                       MAP_JIT memory)",
                errno: Errno::Perm.raw().into(),
                at: 0,
                len,
            });
        }
        Ok(())
    }
}

/// The symbols this file calls on macOS beyond POSIX, each with why. All are public Mach or
/// libSystem calls but `csops`, which the public SDK declares without documenting; each is
/// specific to macOS or to a version of it, so `tests::every_macos_symbol_resolves` looks
/// each up at run time and fails naming the one that is gone.
#[cfg(target_os = "macos")]
pub const SYMBOLS: [(&core::ffi::CStr, &str); 11] = [
    (c"mach_task_self", "the task whose map the code's views are made in"),
    (c"mach_vm_allocate", "the reservation (code, then data) and the code's write view"),
    (c"mach_vm_deallocate", "takes back views made when a later step of making them fails"),
    (
        c"mach_vm_remap",
        "aliases the write view's pages read and execute at the code address: two views of \
         one memory, where MAP_JIT's per-thread switch would also be flipped by a JIT the \
         program itself runs",
    ),
    (c"mach_vm_protect", "lowers each view's maximum protection, so neither can gain the other's"),
    (c"mach_vm_inherit", "a forked child gets no write view"),
    (c"mach_vm_map", "decommit: fresh inaccessible memory over both views of freed pages"),
    (c"csops", "whether this process may run unsigned pages, asked before mapping them"),
    (c"sys_icache_invalidate", "the instruction cache sees new code before a word names it"),
    (c"pthread_get_stackaddr_np", "the bounds a safepoint's walk of its own stack stays in"),
    (c"pthread_get_stacksize_np", "as pthread_get_stackaddr_np"),
];

#[cfg(target_os = "macos")]
impl Code {
    /// Reserves `len` bytes of code then `data` bytes of data (each a whole number of pages),
    /// and makes the code's two views. Nothing is committed until written.
    ///
    /// 1. `mach_vm_allocate` reserves code and data together, anywhere: anonymous, readable
    ///    and writable, inherited by a fork as a copy. The data stays as it is.
    /// 2. `mach_vm_allocate` makes the write view, anywhere.
    /// 3. `mach_vm_remap` (`copy` = 0, `VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE`) maps the write
    ///    view's pages again over the reservation's code part: the run view, inherited by a
    ///    fork as a copy (the child's snapshot, see the module's table).
    /// 4. `mach_vm_protect` sets the run view's maximum, then current, protection to read and
    ///    execute, and the write view's maximum to read and write; `mach_vm_inherit` keeps the
    ///    write view from a child.
    ///
    /// Why `mach_vm_remap` of anonymous memory and not a Mach memory entry
    /// (`mach_make_memory_entry_64` with `MAP_MEM_NAMED_CREATE`) mapped twice with
    /// `mach_vm_map`: both run and rewrite code correctly, but the entry's object keeps every
    /// page it was given until the whole object goes. Measured on macOS 27.0.1, Apple silicon,
    /// 2026-11-15 (a C probe of both, 2 GiB written, 1 GiB then replaced in both views): the
    /// entry gave back nothing (`mach_memory_entry_get_page_counts` still counted every page),
    /// the remapped anonymous memory gave back the 1 GiB. A code area freed a page at a time
    /// ([`Code::decommit`]) needs the second. (`blaze-inproc`'s memory object is an entry
    /// because it is handed to another process as a port; this memory never leaves the
    /// process.) Adapted from makepad/relax (MIT), libs/relax/compiler/src/os/mem.rs, whose
    /// macOS code memory is the same alias; here the fork keeps the run view as a copy where
    /// relax maps it again in a `pthread_atfork` child handler.
    pub fn map_with_data(len: usize, data: usize) -> Result<Code, SysError> {
        use macos::{VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE, kern, task};

        let whole = Code::layout(len, data)?;
        macos::unsigned_code_runs(len)?;
        let mut base = 0u64;
        // SAFETY: anonymous memory anywhere: nothing of this process is replaced; `base` is a
        // live out-pointer, read only on success.
        let code = unsafe { macos::mach_vm_allocate(task(), &raw mut base, whole as u64, macos::VM_FLAGS_ANYWHERE) };
        kern("mach_vm_allocate(code and data)", code, 0, whole)?;
        let mut write = 0u64;
        // SAFETY: as above, for the write view.
        let code = unsafe { macos::mach_vm_allocate(task(), &raw mut write, len as u64, macos::VM_FLAGS_ANYWHERE) };
        if let Err(why) = kern("mach_vm_allocate(the write view)", code, 0, len) {
            // SAFETY: the reservation made above, which nothing has used or named.
            unsafe { macos::mach_vm_deallocate(task(), base, whole as u64) };
            return Err(why);
        }
        let views = || -> Result<(), SysError> {
            let (mut run, mut current, mut maximum) = (base, 0, 0);
            // SAFETY: `base..base + len` is the reservation's code part, made above and named
            // by nothing, so overwriting it replaces nothing live; `write..write + len` is the
            // write view made above; the out-pointers are live locals.
            let code = unsafe {
                macos::mach_vm_remap(
                    task(),
                    &raw mut run,
                    len as u64,
                    0,
                    macos::VM_FLAGS_FIXED | macos::VM_FLAGS_OVERWRITE,
                    task(),
                    write,
                    0,
                    &raw mut current,
                    &raw mut maximum,
                    macos::VM_INHERIT_COPY,
                )
            };
            kern("mach_vm_remap(the run view)", code, 0, len)?;
            if run != base {
                return Err(SysError { call: "mach_vm_remap(the run view)", errno: Errno::Fault.raw().into(), at: 0, len });
            }
            let protect = |address: u64, ceiling: bool, protection: core::ffi::c_int, call: &'static str| {
                // SAFETY: a view made above, of `len` bytes, that nothing runs or names yet;
                // changing its protection touches no other mapping.
                let code = unsafe {
                    macos::mach_vm_protect(task(), address, len as u64, ceiling.into(), protection)
                };
                kern(call, code, 0, len)
            };
            // The maximum first: it is the run view's own, and the source's (read, write and
            // execute) was given to it by the remap.
            protect(base, true, VM_PROT_READ | VM_PROT_EXECUTE, "mach_vm_protect(the run view's maximum)")?;
            protect(base, false, VM_PROT_READ | VM_PROT_EXECUTE, "mach_vm_protect(the run view)")?;
            protect(write, true, VM_PROT_READ | VM_PROT_WRITE, "mach_vm_protect(the write view's maximum)")?;
            // SAFETY: the write view made above; changes only what a fork gives the child.
            let code = unsafe { macos::mach_vm_inherit(task(), write, len as u64, macos::VM_INHERIT_NONE) };
            kern("mach_vm_inherit(the write view)", code, 0, len)
        };
        let made = views().and_then(|()| {
            match (NonNull::new(base as *mut u8), NonNull::new(write as *mut u8)) {
                (Some(exec), Some(write)) => Ok(Code { exec, write, len, data }),
                _ => Err(SysError { call: "mach_vm_allocate", errno: Errno::Fault.raw().into(), at: 0, len }),
            }
        });
        if made.is_err() {
            // SAFETY: the reservation made above, which nothing has run, written or named.
            unsafe { macos::mach_vm_deallocate(task(), base, whole as u64) };
            // SAFETY: as above, the write view.
            unsafe { macos::mach_vm_deallocate(task(), write, len as u64) };
        }
        made
    }

    /// Copies `bytes` to `at` and makes them runnable. The bytes are ones no thread can be
    /// running (the module's invariant).
    pub fn write(&mut self, at: usize, bytes: &[u8]) -> Result<(), SysError> {
        self.put(at, bytes)?;
        // SAFETY: the range just written, in the run view (`put` checked it); the maintenance
        // changes no byte. It is done by the run view's addresses; the data cache line it
        // cleans is the write view's line too, Apple silicon's caches being physically
        // indexed and tagged (what `blaze_inproc_rt::sys::check_icache` rests on as well).
        unsafe { macos::sys_icache_invalidate(self.exec.as_ptr().add(at).cast(), bytes.len()) };
        Ok(())
    }

    /// As [`Code::write`]: on macOS `sys_icache_invalidate` is all a write needs (there is no
    /// `membarrier`; the bytes were never code before, so no core holds stale instructions of
    /// them), so there is nothing to put off. To confirm on a real machine under load: that no
    /// core runs a stale fetch of a range rewritten in place (a gate's entry), which Linux
    /// guards with `membarrier(PRIVATE_EXPEDITED_SYNC_CORE)`.
    pub fn copy(&mut self, at: usize, bytes: &[u8]) -> Result<(), SysError> {
        self.write(at, bytes)
    }

    /// Nothing: [`Code::copy`] is a whole write here.
    pub fn settle(&mut self) -> Result<(), SysError> {
        Ok(())
    }

    /// Gives the memory of whole pages back to the system: both views of `at..at + len` are
    /// replaced with fresh memory no access reaches. Their addresses stay taken and are never
    /// used again (the core hands an offset out once); a fetch there faults.
    ///
    /// Not `madvise`: with the pages mapped twice, `MADV_FREE_REUSABLE`, `MADV_FREE` and
    /// `MADV_DONTNEED` on the write view all left the footprint as it was (measured with the
    /// probe `map_with_data` names: 64 MiB written, 64 MiB still counted after each), as XNU
    /// takes back only pages of an object mapped once.
    pub fn decommit(&mut self, at: usize, len: usize) -> Result<(), SysError> {
        self.within("decommit", at, len)?;
        let page = page()?;
        if at % page != 0 || len % page != 0 {
            return Err(SysError { call: "decommit", errno: Errno::Inval.raw().into(), at, len });
        }
        if len == 0 {
            return Ok(());
        }
        // The run view first: once it is covered, no thread could run the pages even were the
        // core wrong about them.
        macos::cover("mach_vm_map(over the run view)", (self.base() + at) as u64, at, len)?;
        macos::cover("mach_vm_map(over the write view)", self.write.as_ptr() as u64 + at as u64, at, len)
    }
}

/// What AArch64 needs after a write and x86-64 does not (there, code that was never run is
/// in no cache that needs telling, and a word is data).
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
mod linux {
    use blaze_sys::linux::nr::MEMBARRIER;
    use core::ffi::c_long;

    /// `membarrier(2)`: every thread of the process has run a core-serializing instruction
    /// when the call returns, so none runs instructions it fetched before a write.
    pub(super) const PRIVATE_EXPEDITED_SYNC_CORE: c_long = 1 << 5;
    pub(super) const REGISTER_PRIVATE_EXPEDITED_SYNC_CORE: c_long = 1 << 6;

    /// `membarrier(command, 0, 0)`.
    pub(super) fn membarrier(command: c_long) -> bool {
        let zero: c_long = 0;
        // SAFETY: a system call that takes three integers and touches no memory of ours.
        unsafe { libc::syscall(MEMBARRIER as c_long, command, zero, zero) == 0 }
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
    /// Reserves `len` bytes of code then `data` bytes of data (each a whole number of pages),
    /// and makes the code's two views. Nothing is committed until written.
    ///
    /// 1. A memfd of `len` bytes, sparse.
    /// 2. An anonymous private mapping of code and data together, anywhere (`MAP_NORESERVE`):
    ///    the reservation, and the data as it stays.
    /// 3. The memfd read and execute over the reservation's code part (`MAP_FIXED`), the run
    ///    view; the memfd read and write anywhere, the write view, which a fork does not give
    ///    the child (`MADV_DONTFORK`).
    pub fn map_with_data(len: usize, data: usize) -> Result<Code, SysError> {
        use blaze_sys::Path;
        use blaze_sys::linux::{Advice, MapFlags, Prot, ftruncate, madvise, mmap, munmap};
        use blaze_sys::linux::fdctl::{MemfdFlags, memfd_create};
        use blaze_sys::posix::types::off_t;

        let whole = Code::layout(len, data)?;
        let fd = memfd_create(Path::new(c"blaze-launch"), MemfdFlags::CLOSE_ON_EXEC)
            .map_err(|err| SysError { call: "memfd_create", errno: err.raw().into(), at: 0, len })?;
        let Ok(length) = off_t::try_from(len) else {
            return Err(SysError { call: "ftruncate", errno: Errno::FBig.raw().into(), at: 0, len });
        };
        ftruncate(fd.fd(), length as u64)
            .map_err(|err| SysError { call: "ftruncate", errno: err.raw().into(), at: 0, len })?;
        let reservation = MapFlags::PRIVATE | MapFlags::ANONYMOUS | MapFlags::NO_RESERVE;
        let protection = Prot::READ | Prot::WRITE;
        // SAFETY: an anonymous mapping at no fixed address: nothing of this process is
        // replaced.
        let base = unsafe { mmap(None, whole, protection, reservation, None, 0) }
            .map_err(|err| SysError { call: "mmap(code and data)", errno: err.raw().into(), at: 0, len: whole })?;
        let views = || -> Result<(NonNull<u8>, NonNull<u8>), SysError> {
            let call = "mmap(memfd, read and execute)";
            let flags = MapFlags::SHARED | MapFlags::FIXED;
            let protection = Prot::READ | Prot::EXEC;
            // SAFETY: a shared mapping of the memfd over the reservation's code part, made
            // above and named by nothing, so `MAP_FIXED` replaces nothing live. Only the
            // owner writes the memfd through its write view, as the module invariant states.
            let exec = unsafe { mmap(Some(base), len, protection, flags, Some(fd.fd()), 0) }
                .map_err(|err| SysError { call, errno: err.raw().into(), at: 0, len })?;
            let call = "mmap(memfd, read and write)";
            let protection = Prot::READ | Prot::WRITE;
            // SAFETY: a new mapping at no fixed address. Only the owner writes this memfd.
            let write = unsafe { mmap(None, len, protection, MapFlags::SHARED, Some(fd.fd()), 0) }
                .map_err(|err| SysError { call, errno: err.raw().into(), at: 0, len })?;
            // SAFETY: the write view just made; changes only what a fork gives the child.
            if let Err(err) = unsafe { madvise(write, len, Advice::DontFork) } {
                let why = SysError { call: "madvise(MADV_DONTFORK)", errno: err.raw().into(), at: 0, len };
                // SAFETY: the write view just made, which nothing has written or named.
                let _ = unsafe { munmap(write, len) };
                return Err(why);
            }
            Ok((exec, write))
        };
        let (exec, write) = match views() {
            Ok(views) => views,
            Err(why) => {
                // SAFETY: the reservation made above, which nothing has run or named (the run
                // view, if made, is inside it).
                let _ = unsafe { munmap(base, whole) };
                return Err(why);
            }
        };
        let code = Code { exec, write, fd, len, data };
        // A process registers once before it may ask for the barrier; again is harmless.
        #[cfg(target_arch = "aarch64")]
        if !linux::membarrier(linux::REGISTER_PRIVATE_EXPEDITED_SYNC_CORE) {
            return Err(failed("membarrier(REGISTER_PRIVATE_EXPEDITED_SYNC_CORE)", 0, len));
        }
        Ok(code)
    }

    /// Copies `bytes` to `at` and makes them runnable. The bytes are ones no thread can be
    /// running (the module's invariant).
    pub fn write(&mut self, at: usize, bytes: &[u8]) -> Result<(), SysError> {
        self.copy(at, bytes)?;
        self.settle()
    }

    /// [`Code::write`] without its last step: the bytes are in memory and the instruction
    /// caches were told, and no core has yet been made to drop what it fetched before. For
    /// several writes of one edit, each of bytes nothing names: the caller calls
    /// [`Code::settle`] once after the last, before any word names what was written. (One
    /// `membarrier` a function made a launcher's start of 210 functions 210 system calls.)
    pub fn copy(&mut self, at: usize, bytes: &[u8]) -> Result<(), SysError> {
        self.put(at, bytes)?;
        #[cfg(target_arch = "aarch64")]
        linux::icache(self.base() + at, bytes.len());
        Ok(())
    }

    /// Every core runs what [`Code::copy`] wrote from here on: on AArch64,
    /// `membarrier(PRIVATE_EXPEDITED_SYNC_CORE)`; on x86-64 nothing is needed.
    pub fn settle(&mut self) -> Result<(), SysError> {
        #[cfg(target_arch = "aarch64")]
        if !linux::membarrier(linux::PRIVATE_EXPEDITED_SYNC_CORE) {
            return Err(failed("membarrier(PRIVATE_EXPEDITED_SYNC_CORE)", 0, 0));
        }
        Ok(())
    }

    /// Gives the memory of whole pages back to the system: a hole in the memfd, which takes
    /// the pages out of both mappings. (`MADV_DONTNEED` would not: the pages of a shared
    /// mapping belong to the file.) Their addresses stay reserved and are never used again.
    pub fn decommit(&mut self, at: usize, len: usize) -> Result<(), SysError> {
        use blaze_sys::linux::fdctl::{FallocateFlags, fallocate};

        self.within("fallocate(PUNCH_HOLE)", at, len)?;
        // Whole pages only: a hole in part of a page would zero bytes of a body still live.
        let page = page()?;
        if at % page != 0 || len % page != 0 {
            return Err(SysError { call: "fallocate(PUNCH_HOLE)", errno: Errno::Inval.raw().into(), at, len });
        }
        let mode = FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE;
        fallocate(self.fd.fd(), mode, at as u64, len as u64)
            .map_err(|err| SysError { call: "fallocate(PUNCH_HOLE)", errno: err.raw().into(), at, len })
    }
}

/// The calling thread's stack: its low and its high end.
#[cfg(target_os = "macos")]
pub fn stack() -> Result<(usize, usize), SysError> {
    let me = thread::pthread_self();
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
        use blaze_sys::posix::io::{MapFlags, Prot, mmap};

        #[cfg(target_os = "linux")]
        let flags = MapFlags::PRIVATE | MapFlags::ANONYMOUS | MapFlags::NO_RESERVE;
        #[cfg(target_os = "macos")]
        let flags = MapFlags::PRIVATE | MapFlags::ANONYMOUS;
        let protection = Prot::READ | Prot::WRITE;
        // SAFETY: an anonymous private mapping at no fixed address replaces nothing live.
        let base = unsafe { mmap(None, len, protection, flags, None, 0) }
            .map_err(|err| SysError { call: "mmap(the program's data)", errno: err.raw().into(), at: 0, len })?;
        Ok(Data { base, len, end: 0 })
    }

    /// The address of the mapping's first byte.
    pub fn base(&self) -> usize {
        self.base.as_ptr() as usize
    }

    /// How many bytes were handed out, alignment's gaps included.
    pub fn used(&self) -> usize {
        self.end
    }

    /// Hands out `size` bytes aligned to `align` (a power of two), zeroed, and gives their
    /// address. ENOMEM when the reservation has no room: the program is started again.
    pub fn reserve(&mut self, size: usize, align: usize) -> Result<usize, SysError> {
        let full = SysError { call: "reserving the program's data", errno: Errno::NoMem.raw().into(), at: self.end, len: size };
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
            return Err(SysError { call: "filling the program's data", errno: Errno::Fault.raw().into(), at: address, len: bytes.len() });
        }
        // SAFETY: `address..address + bytes.len()` is inside what `reserve` handed out
        // (checked above), in a mapping that is readable, writable and never unmapped;
        // `bytes` is a live slice that cannot overlap it (the mapping is never borrowed as a
        // slice); no thread reads the range yet (the type's invariant).
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len()) };
        Ok(())
    }
}

/// One value handed from a thread to another that waits for it, with nothing of the Rust
/// runtime under it: a pthread mutex and condition variable.
///
/// Why not a channel or `std::sync`: a program thread waits here (a stub's wait for its
/// code) and says things to the control thread at any point of its life, its end included,
/// where the launcher's own thread-locals for that thread may be gone. A channel finds a
/// waiting receiver through the sender's thread id, which lives in a thread-local, and
/// `std::thread::current()` panics once that is destroyed (found on Linux: a function
/// first called while a thread was ending aborted the program). pthread's primitives need
/// no thread-local.
pub struct Handoff<T> {
    #[cfg(target_os = "macos")]
    lock: core::cell::UnsafeCell<thread::pthread_mutex_t>,
    #[cfg(target_os = "linux")]
    lock: core::cell::UnsafeCell<libc::pthread_mutex_t>,
    #[cfg(target_os = "macos")]
    cond: core::cell::UnsafeCell<thread::pthread_cond_t>,
    #[cfg(target_os = "linux")]
    cond: core::cell::UnsafeCell<libc::pthread_cond_t>,
    /// Read and written only with `lock` held.
    value: core::cell::UnsafeCell<Option<T>>,
}

// SAFETY: `value` is touched only under the pthread mutex, which makes a `Handoff<T>` a
// lock around an `Option<T>`: shared between threads when `T` may be sent between them.
unsafe impl<T: Send> Send for Handoff<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Sync for Handoff<T> {}

impl<T> Handoff<T> {
    /// Empty. A `Handoff` must not be moved once a thread uses it (pthread's objects are
    /// tied to their address): every user keeps it behind an `Arc` or in a value that stays.
    pub const fn new() -> Handoff<T> {
        Handoff {
            #[cfg(target_os = "macos")]
            lock: core::cell::UnsafeCell::new(thread::PTHREAD_MUTEX_INITIALIZER),
            #[cfg(target_os = "linux")]
            lock: core::cell::UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER),
            #[cfg(target_os = "macos")]
            cond: core::cell::UnsafeCell::new(thread::PTHREAD_COND_INITIALIZER),
            #[cfg(target_os = "linux")]
            cond: core::cell::UnsafeCell::new(libc::PTHREAD_COND_INITIALIZER),
            value: core::cell::UnsafeCell::new(None),
        }
    }

    /// Runs `with` on the value, with the mutex held.
    fn locked<R>(&self, with: impl FnOnce(&mut Option<T>) -> R) -> R {
        // SAFETY: the mutex was statically initialized and is at a fixed address (`new`'s
        // contract); locking it is sound from any thread.
        #[cfg(target_os = "macos")]
        unsafe { thread::pthread_mutex_lock(self.lock.get()) };
        // SAFETY: the same initialized mutex at a fixed address.
        #[cfg(target_os = "linux")]
        unsafe { libc::pthread_mutex_lock(self.lock.get()) };
        // SAFETY: the mutex is held, so this thread alone touches `value`.
        let result = with(unsafe { &mut *self.value.get() });
        // SAFETY: held by this thread, locked above.
        #[cfg(target_os = "macos")]
        unsafe { thread::pthread_mutex_unlock(self.lock.get()) };
        // SAFETY: held by this thread, locked above.
        #[cfg(target_os = "linux")]
        unsafe { libc::pthread_mutex_unlock(self.lock.get()) };
        result
    }

    /// Hands `value` over, replacing one not yet taken, and wakes a thread that waits.
    pub fn put(&self, value: T) {
        self.locked(|held| *held = Some(value));
        // SAFETY: a statically initialized condition variable at a fixed address.
        #[cfg(target_os = "macos")]
        unsafe { thread::pthread_cond_signal(self.cond.get()) };
        // SAFETY: the same initialized condition variable at a fixed address.
        #[cfg(target_os = "linux")]
        unsafe { libc::pthread_cond_signal(self.cond.get()) };
    }

    /// Waits for a value and takes it. The wait has no clock: it ends when another thread
    /// calls [`Handoff::put`], which every user arranges (the control thread answers every
    /// stub, with code or with why none can come).
    pub fn take(&self) -> T {
        // SAFETY: as `locked`.
        #[cfg(target_os = "macos")]
        unsafe { thread::pthread_mutex_lock(self.lock.get()) };
        // SAFETY: as `locked`.
        #[cfg(target_os = "linux")]
        unsafe { libc::pthread_mutex_lock(self.lock.get()) };
        loop {
            // SAFETY: the mutex is held.
            if let Some(value) = unsafe { (*self.value.get()).take() } {
                // SAFETY: held by this thread.
                #[cfg(target_os = "macos")]
                unsafe { thread::pthread_mutex_unlock(self.lock.get()) };
                // SAFETY: held by this thread.
                #[cfg(target_os = "linux")]
                unsafe { libc::pthread_mutex_unlock(self.lock.get()) };
                return value;
            }
            // SAFETY: the mutex is held by this thread, as `pthread_cond_wait` asks; it
            // comes back with it held. A spurious return loops.
            #[cfg(target_os = "macos")]
            unsafe { thread::pthread_cond_wait(self.cond.get(), self.lock.get()) };
            // SAFETY: the mutex is held by this thread with the same live condition variable.
            #[cfg(target_os = "linux")]
            unsafe { libc::pthread_cond_wait(self.cond.get(), self.lock.get()) };
        }
    }
}

impl<T> core::fmt::Debug for Handoff<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // What it holds is behind its mutex: not read for a debug line.
        f.write_str("Handoff")
    }
}

impl<T> Default for Handoff<T> {
    fn default() -> Handoff<T> {
        Handoff::new()
    }
}
