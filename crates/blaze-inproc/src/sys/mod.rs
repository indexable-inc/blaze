//! The platform: memory objects and their mappings, the channel's transport, handing objects
//! to a run process and laying them over its executable, its threads, the instruction cache
//! and stack walks. Every `unsafe` of the crate is under this module (but the start hooks'
//! entry points, their environment and their stores of a dependency image's words through
//! [`store_private`], `crate::runtime`'s), behind the safe API each
//! platform module exports with the same names and these signatures (`Error` is
//! `crate::Error`, `Channel` is `crate::channel::Channel`, `Access` is `crate::Access`,
//! `Overlay` and `Grant` are `crate::handshake`'s):
//!
//! - `fn page() -> u64`: the mapping granule (Linux's page size, 16 KiB on macOS AArch64,
//!   the 64 KiB allocation granularity on Windows).
//! - `fn check_icache() -> Result<(), Error>`: whether `View::publish` from the daemon's
//!   view makes code right in another process's view of the same pages here (AArch64: an
//!   instruction cache that does not alias by virtual address); an error names what the CPU
//!   reports.
//! - `fn random(out: &mut [u8; 16]) -> Result<(), Error>`: from the OS's generator.
//! - `struct Object` (`Send`): a memory object this process holds, zero when made.
//!   `fn create(name: &str, len: u64) -> Result<Object, Error>`, `fn len(&self) -> u64`,
//!   `fn duplicate(&self) -> Result<Object, Error>` (another reference to the same object),
//!   `fn map_here(&self, access: Access) -> Result<View, Error>` (the whole object, in this
//!   process; the daemon maps `Access::Shared`).
//! - `struct View` (`Send + Sync`): a mapping in this process, unmapped when dropped.
//!   `fn len(&self) -> usize`, `fn address(&self) -> u64`,
//!   `fn bytes(&self, range: Range<usize>) -> Result<&[u8], Error>`,
//!   `fn bytes_mut(&mut self, range: Range<usize>) -> Result<&mut [u8], Error>` (an error on a
//!   view not writable), `fn word(&self, at: usize) -> &AtomicU64` and
//!   `fn word32(&self, at: usize) -> &AtomicU32` (`at` aligned and in bounds, else a panic:
//!   callers check), and `fn publish(&self, range: Range<usize>)`, the instruction-cache
//!   maintenance for code written there (`dc cvau`, `dsb ish`, `ic ivau`, `dsb ish`, `isb`
//!   on AArch64 as `CTR_EL0` asks; nothing on x86-64).
//! - `struct Exec` (`Send + Sync`): private memory of this process, code then data, zero when
//!   made, freed when dropped (`crate::exec`). `fn new(code: usize, data: usize, write: impl
//!   FnOnce(&mut [u8], &mut [u8])) -> Result<Exec, Error>`: `write` gets the code's `code`
//!   bytes and the data's `data`, writable, at the addresses they run at, on the calling
//!   thread; then the code is made read and execute (Linux `mprotect`, macOS `MAP_JIT` with
//!   `pthread_jit_write_protect_np`, Windows `VirtualProtectEx`) with the instruction cache
//!   maintenance `View::publish` does. The data is within 1 GiB of every code byte.
//!   `fn code_address(&self) -> u64`, `fn data_address(&self) -> u64`,
//!   `fn data(&mut self) -> &mut [u8]`.
//! - `struct Stream` (`Send`): a connected byte stream of the channel.
//!   `fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error>` (0 when the peer hung up),
//!   `fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error>`,
//!   `fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), Error>`.
//! - `struct Listener` (daemon, `Send`): `fn bind(dir: &Path) -> Result<(Listener, String),
//!   Error>`, the `String` being what `connect` takes; `fn accept(&self) -> Result<(Stream,
//!   u32), Error>`, the peer's process ID as the OS says it, refusing peers of another user.
//! - `fn connect(address: &str) -> Result<Stream, Error>` (start hook).
//! - `fn send_objects(channel: &mut Channel, pid: u32, grant: &[u8], objects: &[(&Object,
//!   Rights)]) -> Result<Remote, Error>` (daemon): sends the GRANT frame of body `grant`
//!   (kind `kinds::GRANT`, sequence 0) and, with it or after it, `objects` so the start hook
//!   of process `pid` can map each with the rights `Rights` says and no more.
//!   `fn recv_objects(channel: &mut Channel, count: usize) -> Result<Vec<Granted>, Error>`
//!   (start hook), called once the GRANT frame was read, gives them in the same order.
//! - `struct Remote` (daemon, `Send`): what the daemon keeps to act in the run process.
//!   `fn map(&self, object: u32, access: Access) -> Result<u64, Error>` maps a whole granted
//!   object anywhere in the run process, for a hook's MAP frame where the platform maps from
//!   the daemon's side (Windows); an error elsewhere. `fn overlay(&self, slide: u64, grant:
//!   &Grant) -> Result<(), Error>` lays the grant's overlays over the run process's image
//!   (Windows, for a hook's OVERLAY frame), each at its linked address plus `slide`, every
//!   other byte of the image's span as it was; an error elsewhere. On a failure past the
//!   first change it ends the process, which can't run half laid over.
//! - `struct Granted` (start hook): an object the hook was given. `fn len(&self) -> u64`,
//!   `fn map_anywhere(&self, channel: &mut Channel, access: Access) -> Result<u64, Error>`
//!   (the whole object, for the rest of the process).
//! - `fn overlay(channel: &mut Channel, objects: &[Granted], overlays: &[Overlay], slide: u64)
//!   -> Result<(), Error>` (start hook): replaces each overlay's pages of this process's
//!   executable, at its linked address plus `slide`, with the mapping of its object, at once
//!   (one `mmap` or `mach_vm_map` each; Windows asks the daemon, OVERLAY). The caller proved
//!   every overlay is a code segment of the image, with the bytes it replaces, or inside a
//!   reservation ([`crate::handshake::Grant::check`]).
//! - `struct Scope` (start hook): the run process's global scope, as the system loader filled
//!   it from the executable's libraries. `fn new() -> Scope`, `fn find(&mut self, name: &str)
//!   -> Result<Option<u64>, Error>`: the address of the C name `name`, none when nothing loaded
//!   defines it (`dlsym(RTLD_DEFAULT, ..)` on Linux and macOS, `GetProcAddress` over the
//!   process's modules in load order on Windows; an error elsewhere).
//! - `fn own_image() -> Result<(u64, &'static [u8]), Error>` (start hook): where this
//!   process's executable has its header, and its headers' bytes (the program headers, load
//!   commands or section table), as the system loader mapped them.
//! - `fn clone_file(from: &Path, to: &Path) -> Result<bool, Error>` (daemon): makes `to`, new,
//!   a clone of `from` sharing its blocks (Linux `FICLONE`, macOS `clonefile`); `false` when
//!   the filesystem has no clones (the caller writes the bytes), an error otherwise.
//! - `fn threads(pid: u32, out: &mut Vec<u64>) -> Result<(), Error>`: replaces `out` with the
//!   IDs of the live threads of process `pid` as `thread_id` names them; none for a process
//!   that is gone.
//! - `fn thread_id() -> u64`: the calling thread's, as the OS names it system-wide.
//! - `fn service() -> Option<String>` (daemon): the Mach global name a sandboxed run looks up
//!   to reach this daemon (macOS's rendezvous, `dev.blaze.inproc.<pid>`); none elsewhere.
//! - `fn serialize()`: a context synchronization of the calling thread (`isb`; `cpuid` on
//!   x86-64).
//! - `fn backtrace(out: &mut [u64]) -> Option<usize>`: the return addresses of the calling
//!   thread's frames, all of them up to its outermost; `None` when they don't fit or the walk
//!   could not finish.
//! - `Remote::kill(&self) -> Result<(), Error>` (daemon): ends the run process (a pidfd on
//!   Linux, the process handle on Windows, its process ID on macOS); one already gone is fine.
//!
//! The asks ([`crate::ask`]), each async-signal-safe on the run process's side:
//!
//! - `fn wait(word: &AtomicU64, expected: u32, timeout: Duration) -> bool`: sleeps while the
//!   word's low 32 bits (its first four bytes) are `expected`, until woken or `timeout`; `false`
//!   when the timeout passed. Across processes for a word of a shared mapping on Unix (a futex
//!   that is not private, `__ulock_wait` shared); within the process on Windows.
//!   `fn wake(word: &AtomicU64)` wakes every thread sleeping on it.
//! - `struct Bells` (daemon, `Send + Sync`): what wakes a run process's askers and what they
//!   ring. `fn new(remote: &Remote, slots: usize) -> Result<(Bells, Vec<u64>), Error>`, with
//!   what the run process names the doorbell and each slot's bell by (Windows: the events,
//!   duplicated into it; 0 elsewhere); `fn wake(&self, slot: usize, word: &AtomicU64)` after an
//!   answer is stored in `word`; `fn wait_ring(&self, stream: &mut Stream) -> Result<bool,
//!   Error>`, until the doorbell rang (`true`) or the process ended (`false`).
//! - `fn ring(stream: &Stream, doorbell: u64) -> Result<(), Error>` (run): one byte on the
//!   channel, or the doorbell's event. `fn wait_answer(word: &AtomicU64, expected: u32, bell:
//!   u64, timeout: Duration) -> bool`, as `wait` (Windows: on the slot's event).
//!   `fn hung_up(stream: &Stream) -> bool`: the daemon's end wrote or closed, which after the
//!   start means it is gone.
//! - `struct Errno` (run): `fn keep() -> Errno`, the thread's `errno` (last error on Windows),
//!   put back when dropped.

#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
))]
mod unix;

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod linux;
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
pub(crate) use linux::*;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) use macos::*;

#[cfg(all(windows, target_arch = "x86_64"))]
mod windows;
#[cfg(all(windows, target_arch = "x86_64"))]
pub(crate) use windows::*;

#[cfg(not(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64"),
)))]
mod none;
#[cfg(not(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64"),
)))]
pub(crate) use none::*;

/// What a run process may do with an object it is given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rights {
    /// Map it to read, execute or copy, never to write it shared: the image and arena, a
    /// fixed map's.
    Code,
    /// Map it shared and writable, never executable: the fence's control words, the asks, the
    /// cells.
    Words,
}

/// The words of an object the start hook mapped at `at`, `len` bytes and at least `least`, for
/// the rest of the process: the control words, the asks.
pub(crate) fn adopt_words(at: u64, len: u64, least: u64) -> Result<&'static [std::sync::atomic::AtomicU64], crate::Error> {
    if at == 0 || !at.is_multiple_of(8) || len < least || len > isize::MAX as u64 {
        return Err(crate::Error::new(format!("words of {len} bytes at {at:#x}, where {least} are needed")));
    }
    // SAFETY: the start hook mapped a words object shared and writable at `at`, `len` bytes,
    // with `Granted::map_anywhere`, which never unmaps it. It is non-null, 8-aligned and of
    // `len / 8` whole words (checked above). Every access to it, here and in the daemon's
    // process, is atomic, so a `&[AtomicU64]` to it is sound for the rest of the process.
    Ok(unsafe { std::slice::from_raw_parts(at as *const std::sync::atomic::AtomicU64, (len / 8) as usize) })
}

/// Stores `value` in the 8 bytes at `at` of this process, unaligned: a word of a fixed map's
/// private copy the start hook binds ([`crate::bind`]).
///
/// # Safety
///
/// `at..at + 8` is mapped readable and writable in this process and private to it (an
/// `Access::Copy` overlay), and no other thread reads or writes it meanwhile.
pub(crate) unsafe fn store_private(at: u64, value: u64) {
    // SAFETY: the caller's contract: 8 writable bytes of this process's own, nobody else's.
    unsafe { std::ptr::write_unaligned(at as *mut u64, value) };
}

impl crate::fence::Words for View {
    fn words(&self) -> usize {
        self.len() / 8
    }

    fn word(&self, index: usize) -> &std::sync::atomic::AtomicU64 {
        View::word(self, index * 8)
    }
}
