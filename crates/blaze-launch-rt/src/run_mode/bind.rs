//! THE BINDER SEAM (D175, comptime-native.md "The table bound"; rules 30, 76, 78): how the run
//! gives a C name a record calls or reads its address. One composable seam, `Bind`, with
//! strategy as a type parameter and typed refusals ([`BindError`]):
//!
//! 1. A C name the PROGRAM defines (a C unit's global atom, blaze-cunit; a Rust `#[no_mangle]`
//!    fn or static) is a symbol the parent declared (`Down::Declare`, a function slot) or told
//!    (`Down::Data`, a static): its symbol is the name's own (`Symbol::of_name`), so a
//!    record's `Space::Outside` target of it resolves to that slot or static BEFORE any
//!    strategy is asked (`run_mode.rs` `Bound`).
//! 2. Next, [`Bind::intercept`] lets a strategy claim runtime-owned symbols (even when a
//!    `native_lib` dylib is loaded!):
//!    - [`LauncherOwn<B>`] intercepts `__dso_handle`, `pthread_create` (so every user thread
//!      announces its birth to `threads.rs` and the sampler), `_Unwind_RaiseException` (AArch64:
//!      restores `span_exit` shadow frames before libunwind walks the stack), and Linux
//!      `dl_iterate_phdr` (backtraces).
//!    - [`ObservedBind<B>`] layers zero-touch telemetry wrappers (`malloc`, `calloc`, `realloc`,
//!      `free`, `read`, `write`, `close`, `connect`, `getenv`, `pthread_mutex_lock`) on top of
//!      `B`, emitting lock-free 64-byte events into `crate::telemetry` with zero overhead when
//!      the plain `LauncherOwn<Dlsym>` type is chosen instead.
//!    - [`Table`] (compile-time runs) overrides `intercept` with `None` and walls every symbol
//!      through `blaze-comptime-sys`'s table — comptime isolation holds by type.
//! 3. Otherwise the strategy's [`Bind::address`] resolves the symbol (`Dlsym`, `Table`, or
//!    `native_lib` when present). Refusal is a typed [`BindError`].

use core::ffi::{CStr, c_char, c_int, c_void};
use core::fmt;

use blaze_comptime_sys::child::{Binding, binding};
use blaze_comptime_sys::{How, NAMES, Os, Unbound, find};

use crate::telemetry::{AllocOp, Event, IoOp, current_thread_id, emit, now_ns};

/// Why [`Bind::address`] refused to bind a C name (rule 19 & 76: typed error, never bare `&str`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BindError {
    /// No loaded image defines the symbol (`dlsym`).
    NotFound,
    /// A compile-time program requested a C name outside `blaze-comptime-sys`'s table.
    NotInTable,
    /// `blaze-comptime-sys`'s table answers this name only on the other OS.
    OtherSystem,
    /// A data row of `blaze-comptime-sys`'s table was not bound before the run started.
    DataUnbound,
}

impl BindError {
    /// Human-readable reason text carried by [`super::RunModeError::Outside`].
    pub const fn reason(self) -> &'static str {
        match self {
            BindError::NotFound => "no image of this process defines it (`dlsym`)",
            BindError::NotInTable => {
                "a compile-time program binds only the names blaze-comptime-sys's table answers (table.rs `NAMES`), and it has no row for this one"
            }
            BindError::OtherSystem => {
                "blaze-comptime-sys's table answers this name only on the other system"
            }
            BindError::DataUnbound => {
                "a data row of blaze-comptime-sys's table not bound before the run (run_mode/bind.rs `Table::bind`)"
            }
        }
    }
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl core::error::Error for BindError {}

/// How a run binds a C name no record of the program defines.
pub trait Bind {
    /// Optional runtime-owned interception checked BEFORE `native_lib` and [`Bind::address`].
    fn intercept(&mut self, _name: &str) -> Option<u64> {
        None
    }

    /// The address C name `name` binds to (`cname`: the same bytes, NUL-ended), or why it does
    /// not ([`BindError`]).
    fn address(&mut self, name: &str, cname: &CStr) -> Result<u64, BindError>;
}

/// An ordinary run's fallback binding: the images this process loaded, in the loader's order
/// (`dlsym(RTLD_DEFAULT, name)`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Dlsym;

impl Bind for Dlsym {
    fn address(&mut self, _: &str, cname: &CStr) -> Result<u64, BindError> {
        #[cfg(target_os = "macos")]
        // SAFETY: RTLD_DEFAULT searches all loaded images and cname is NUL-terminated.
        let address = unsafe {
            blaze_sys::posix::thread::dlsym(blaze_sys::posix::thread::RTLD_DEFAULT, cname.as_ptr())
        };
        #[cfg(not(target_os = "macos"))]
        // SAFETY: a lookup, by a live NUL-terminated name, among the images this process
        // loaded; it reads the name and returns an address or null.
        let address = unsafe { libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr()) };
        match address.is_null() {
            true => Err(BindError::NotFound),
            false => Ok(address as u64),
        }
    }
}

/// Composable launcher-owned interception layer for ordinary `run_mode` runs.
///
/// Always intercepts:
/// - `__dso_handle` (the placed program's synthetic DSO handle)
/// - `pthread_create` → [`crate::threads::create`] (registers every user thread with the
///   mutator epoch table, CPU sampler, and telemetry ring)
/// - `_Unwind_RaiseException` (AArch64) → [`crate::observe::unwind_raise_exception`] (restores
///   `span_exit` frames on the stack before libunwind walks it)
/// - `dl_iterate_phdr` (Linux) → [`crate::symbols::iterate`] (in-process backtrace symbols)
///
/// Intentionally does NOT intercept `sigaction`/`signal` in `run_mode`: `run_mode` installs
/// `fault_report::install` (chained directly to the previous action), not `signals::install`.
#[derive(Clone, Copy, Debug, Default)]
pub struct LauncherOwn<B> {
    inner: B,
}

impl<B> LauncherOwn<B> {
    pub const fn new(inner: B) -> LauncherOwn<B> {
        LauncherOwn { inner }
    }
}

impl<B: Bind> Bind for LauncherOwn<B> {
    fn intercept(&mut self, name: &str) -> Option<u64> {
        static DSO_HANDLE: u8 = 0;
        match name {
            "__dso_handle" => Some(&raw const DSO_HANDLE as u64),
            "pthread_create" => Some(crate::threads::create as *const () as u64),
            #[cfg(target_arch = "aarch64")]
            "_Unwind_RaiseException" => Some(crate::observe::unwind_raise_exception as *const () as u64),
            #[cfg(target_os = "linux")]
            "dl_iterate_phdr" => Some(crate::symbols::iterate as *const () as u64),
            _ => self.inner.intercept(name),
        }
    }

    fn address(&mut self, name: &str, cname: &CStr) -> Result<u64, BindError> {
        self.inner.address(name, cname)
    }
}

/// Zero-touch OS/allocator/sync telemetry wrapper layered over any `B: Bind` (typically
/// `ObservedBind<LauncherOwn<Dlsym>>`). Active only when chosen by the launcher (`BLAZE_LAUNCH_TELEMETRY`);
/// plain `LauncherOwn<Dlsym>` pays zero overhead.
#[derive(Clone, Copy, Debug, Default)]
pub struct ObservedBind<B> {
    inner: B,
}

impl<B> ObservedBind<B> {
    pub const fn new(inner: B) -> ObservedBind<B> {
        ObservedBind { inner }
    }
}

impl<B: Bind> Bind for ObservedBind<B> {
    fn intercept(&mut self, name: &str) -> Option<u64> {
        if let Some(addr) = self.inner.intercept(name) {
            return Some(addr);
        }
        match name {
            "malloc" => Some(traced_malloc as *const () as u64),
            "calloc" => Some(traced_calloc as *const () as u64),
            "realloc" => Some(traced_realloc as *const () as u64),
            "posix_memalign" => Some(traced_posix_memalign as *const () as u64),
            "free" => Some(traced_free as *const () as u64),
            "read" => Some(traced_read as *const () as u64),
            "write" => Some(traced_write as *const () as u64),
            "close" => Some(traced_close as *const () as u64),
            "connect" => Some(traced_connect as *const () as u64),
            "getenv" => Some(traced_getenv as *const () as u64),
            "pthread_mutex_lock" => Some(traced_mutex_lock as *const () as u64),
            "pthread_mutex_unlock" => Some(traced_mutex_unlock as *const () as u64),
            _ => None,
        }
    }

    fn address(&mut self, name: &str, cname: &CStr) -> Result<u64, BindError> {
        self.inner.address(name, cname)
    }
}

/// Sets the allocation telemetry byte-sampling interval (`0` records every allocation;
/// `N > 0` samples roughly every `N` allocated bytes like Go's `MemProfileRate`).
pub fn set_alloc_sample_bytes(bytes: u64) {
    crate::telemetry::set_alloc_sample_bytes(bytes);
}

#[inline(always)]
fn caller_frames() -> [u64; 3] {
    crate::telemetry::capture_caller_frames(1)
}

/// Reads the immediate caller's return address from the current frame record.
#[inline(always)]
fn caller_return_addr() -> u64 {
    caller_frames()[0]
}

extern "C" fn traced_malloc(size: usize) -> *mut c_void {
    let guard = crate::telemetry::try_enter_alloc_hook();
    // SAFETY: standard libc `malloc`.
    let ptr = unsafe { libc::malloc(size) };
    if guard.is_some() && crate::telemetry::should_sample_alloc(size) {
        let frames = caller_frames();
        let _ = emit(Event::alloc(
            current_thread_id(),
            AllocOp::Malloc,
            now_ns(),
            ptr as usize as u64,
            size as u64,
            frames,
        ));
    }
    ptr
}

extern "C" fn traced_calloc(count: usize, size: usize) -> *mut c_void {
    let guard = crate::telemetry::try_enter_alloc_hook();
    // SAFETY: standard libc `calloc`.
    let ptr = unsafe { libc::calloc(count, size) };
    let total = count.saturating_mul(size);
    if guard.is_some() && crate::telemetry::should_sample_alloc(total) {
        let frames = caller_frames();
        let _ = emit(Event::alloc(
            current_thread_id(),
            AllocOp::Calloc,
            now_ns(),
            ptr as usize as u64,
            total as u64,
            frames,
        ));
    }
    ptr
}

unsafe extern "C" fn traced_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    let guard = crate::telemetry::try_enter_alloc_hook();
    // SAFETY: forwarded from caller with libc `realloc` contract.
    let out = unsafe { libc::realloc(ptr, size) };
    if guard.is_some() && crate::telemetry::should_sample_alloc(size) {
        let frames = caller_frames();
        let _ = emit(Event::alloc(
            current_thread_id(),
            AllocOp::Realloc,
            now_ns(),
            out as usize as u64,
            size as u64,
            frames,
        ));
    }
    out
}

unsafe extern "C" fn traced_posix_memalign(
    memptr: *mut *mut c_void,
    alignment: usize,
    size: usize,
) -> c_int {
    let guard = crate::telemetry::try_enter_alloc_hook();
    // SAFETY: forwarded from caller with POSIX `posix_memalign` contract.
    let ret = unsafe { libc::posix_memalign(memptr, alignment, size) };
    if guard.is_some() && ret == 0 && !memptr.is_null() && crate::telemetry::should_sample_alloc(size) {
        // SAFETY: `ret == 0` guarantees `*memptr` was written by `posix_memalign`.
        let ptr = unsafe { *memptr };
        let frames = caller_frames();
        let _ = emit(Event::alloc(
            current_thread_id(),
            AllocOp::PosixMemalign,
            now_ns(),
            ptr as usize as u64,
            size as u64,
            frames,
        ));
    }
    ret
}

unsafe extern "C" fn traced_free(ptr: *mut c_void) {
    let guard = crate::telemetry::try_enter_alloc_hook();
    if guard.is_some() && crate::telemetry::alloc_sample_bytes() == 0 && !ptr.is_null() {
        let frames = caller_frames();
        let _ = emit(Event::alloc(
            current_thread_id(),
            AllocOp::Free,
            now_ns(),
            ptr as usize as u64,
            0,
            frames,
        ));
    }
    // SAFETY: forwarded from caller with libc `free` contract.
    unsafe { libc::free(ptr) };
}

unsafe extern "C" fn traced_read(fd: c_int, buf: *mut c_void, count: usize) -> isize {
    let lr = caller_return_addr();
    let start = now_ns();
    // SAFETY: forwarded from caller with POSIX `read` contract.
    let ret = unsafe { libc::read(fd, buf, count) };
    let end = now_ns();
    let _ = emit(Event::io(
        current_thread_id(),
        IoOp::Read,
        end,
        end.saturating_sub(start),
        fd as i64 as u64,
        ret as i64 as u64,
        lr,
    ));
    ret
}

unsafe extern "C" fn traced_write(fd: c_int, buf: *const c_void, count: usize) -> isize {
    let lr = caller_return_addr();
    let start = now_ns();
    // SAFETY: forwarded from caller with POSIX `write` contract.
    let ret = unsafe { libc::write(fd, buf, count) };
    let end = now_ns();
    let _ = emit(Event::io(
        current_thread_id(),
        IoOp::Write,
        end,
        end.saturating_sub(start),
        fd as i64 as u64,
        ret as i64 as u64,
        lr,
    ));
    ret
}

extern "C" fn traced_close(fd: c_int) -> c_int {
    let lr = caller_return_addr();
    let start = now_ns();
    // SAFETY: standard POSIX `close`.
    let ret = unsafe { libc::close(fd) };
    let end = now_ns();
    let _ = emit(Event::io(
        current_thread_id(),
        IoOp::Close,
        end,
        end.saturating_sub(start),
        fd as i64 as u64,
        ret as i64 as u64,
        lr,
    ));
    ret
}

unsafe extern "C" fn traced_connect(
    fd: c_int,
    addr: *const libc::sockaddr,
    len: libc::socklen_t,
) -> c_int {
    let lr = caller_return_addr();
    let start = now_ns();
    // SAFETY: forwarded from caller with POSIX `connect` contract.
    let ret = unsafe { libc::connect(fd, addr, len) };
    let end = now_ns();
    let _ = emit(Event::io(
        current_thread_id(),
        IoOp::Connect,
        end,
        end.saturating_sub(start),
        fd as i64 as u64,
        ret as i64 as u64,
        lr,
    ));
    ret
}

unsafe extern "C" fn traced_getenv(name: *const c_char) -> *mut c_char {
    let lr = caller_return_addr();
    let start = now_ns();
    // SAFETY: forwarded from caller with POSIX `getenv` contract.
    let ret = unsafe { libc::getenv(name) };
    let end = now_ns();
    let _ = emit(Event::io(
        current_thread_id(),
        IoOp::Getenv,
        end,
        end.saturating_sub(start),
        name as usize as u64,
        ret as usize as u64,
        lr,
    ));
    ret
}

/// Bounded lock-free mutex-holder cache (`mutex_ptr -> (holder_tid, holder_pc)`) so blocked
/// threads report WHO held the lock and WHERE they acquired it (YourKit/Tracy lock attribution).
struct LockSlot {
    mutex: core::sync::atomic::AtomicU64,
    holder_tid: core::sync::atomic::AtomicU32,
    holder_pc: core::sync::atomic::AtomicU64,
}

impl LockSlot {
    const fn empty() -> Self {
        Self {
            mutex: core::sync::atomic::AtomicU64::new(0),
            holder_tid: core::sync::atomic::AtomicU32::new(0),
            holder_pc: core::sync::atomic::AtomicU64::new(0),
        }
    }
}

const LOCK_SLOTS: usize = 1024;
static LOCK_HOLDERS: [LockSlot; LOCK_SLOTS] = [const { LockSlot::empty() }; LOCK_SLOTS];

#[inline]
fn lock_slot_idx(mutex_addr: u64) -> usize {
    ((mutex_addr.wrapping_mul(0x9e37_79b9_7f4a_7c15)) >> 54) as usize & (LOCK_SLOTS - 1)
}

#[inline]
fn read_lock_holder(mutex_addr: u64) -> (u32, u64) {
    use core::sync::atomic::Ordering;
    let slot = &LOCK_HOLDERS[lock_slot_idx(mutex_addr)];
    if slot.mutex.load(Ordering::Acquire) == mutex_addr {
        (slot.holder_tid.load(Ordering::Acquire), slot.holder_pc.load(Ordering::Relaxed))
    } else {
        (0, 0)
    }
}

#[inline]
fn set_lock_holder(mutex_addr: u64, tid: u32, pc: u64) {
    use core::sync::atomic::Ordering;
    let slot = &LOCK_HOLDERS[lock_slot_idx(mutex_addr)];
    slot.holder_pc.store(pc, Ordering::Relaxed);
    slot.holder_tid.store(tid, Ordering::Release);
    slot.mutex.store(mutex_addr, Ordering::Release);
}

#[inline]
fn clear_lock_holder(mutex_addr: u64, tid: u32) {
    use core::sync::atomic::Ordering;
    let slot = &LOCK_HOLDERS[lock_slot_idx(mutex_addr)];
    if slot.mutex.load(Ordering::Acquire) == mutex_addr && slot.holder_tid.load(Ordering::Acquire) == tid {
        slot.holder_tid.store(0, Ordering::Release);
        slot.mutex.store(0, Ordering::Release);
    }
}

unsafe extern "C" fn traced_mutex_lock(mutex: *mut libc::pthread_mutex_t) -> c_int {
    let lr = caller_return_addr();
    let tid = current_thread_id();
    let addr = mutex as usize as u64;
    let (prev_holder_tid, prev_holder_pc) = read_lock_holder(addr);
    let start = now_ns();
    // SAFETY: forwarded from caller with POSIX `pthread_mutex_lock` contract.
    let ret = unsafe { libc::pthread_mutex_lock(mutex) };
    let end = now_ns();
    if ret == 0 {
        set_lock_holder(addr, tid, lr);
    }
    let _ = emit(Event::sync_wait(
        tid,
        end,
        end.saturating_sub(start),
        addr,
        lr,
        if prev_holder_tid != tid { prev_holder_tid } else { 0 },
        if prev_holder_tid != tid { prev_holder_pc } else { 0 },
    ));
    ret
}

unsafe extern "C" fn traced_mutex_unlock(mutex: *mut libc::pthread_mutex_t) -> c_int {
    let addr = mutex as usize as u64;
    clear_lock_holder(addr, current_thread_id());
    // SAFETY: forwarded from caller with POSIX `pthread_mutex_unlock` contract.
    unsafe { libc::pthread_mutex_unlock(mutex) }
}

/// The most data rows (`How::Data`) one system's table may hold: [`Table::bind`] keeps each
/// row's address in a fixed array, and refuses a table with more.
pub const MOST_DATA_ROWS: usize = 4;

/// A compile-time run's binding (D175): every name through blaze-comptime-sys's table, for the
/// system the code was built for (rule 75: `os` is handed in, never read from the host).
///
/// THREADS: the layer's one cell (blaze-comptime-sys child.rs `LAYER`) is touched only by the
/// thread that installed it and, once `run` starts, by the program's thread. The binder runs on
/// `run`'s CONTROL thread, so it must never touch the cell. Only a data row's binding does
/// (`environ`'s pointer array is built in the layer), so [`Table::bind`] binds every data row
/// of `os` on the installing thread BEFORE `run` starts its threads (the spawn orders those
/// writes before every read), and [`Bind::address`] answers a data row from that list. Every
/// other row is answered from constants (`find`, `function`) or the process's own `dlsym`.
/// This is by construction: a `Table` exists only through `bind`.
#[derive(Clone, Copy, Debug)]
pub struct Table {
    os: Os,
    /// Each data row of `os` and the address the layer gave it, `count` of them.
    data: [(&'static str, usize); MOST_DATA_ROWS],
    count: usize,
}

/// Why a [`Table`] could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TableError {
    /// The table of `os` holds more data rows than [`MOST_DATA_ROWS`].
    TooManyData { os: Os, most: usize },
    /// The layer gave no address for data row `name` (it has no word for it: a bug of
    /// blaze-comptime-sys, whose test checks every row has one).
    DataUnbound { name: &'static str, why: Unbound },
}

impl core::fmt::Display for TableError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TableError::TooManyData { os, most } => write!(
                f,
                "blaze-comptime-sys's table for {os:?} holds more than {most} data rows (run_mode/bind.rs `MOST_DATA_ROWS`)"
            ),
            TableError::DataUnbound { name, why } => {
                write!(f, "blaze-comptime-sys's layer gives no word for its data row `{name}` ({why:?}; child.rs `data`)")
            }
        }
    }
}

impl Table {
    /// The table of `os`, its data rows bound now, on the calling thread: call it on the thread
    /// that installed the layer, after `install` and before `run` (the struct's doc).
    pub fn bind(os: Os) -> Result<Table, TableError> {
        let mut table = Table { os, data: [("", 0); MOST_DATA_ROWS], count: 0 };
        for row in NAMES.iter().filter(|row| row.how == How::Data && row.on.has(os)) {
            if table.count == MOST_DATA_ROWS {
                return Err(TableError::TooManyData { os, most: MOST_DATA_ROWS });
            }
            let at = match binding(row.c, os) {
                Ok(Binding::Here(at)) => at,
                Ok(Binding::Own) => return Err(TableError::DataUnbound { name: row.c, why: Unbound::NotInTable }),
                Err(refused) => return Err(TableError::DataUnbound { name: row.c, why: refused.why }),
            };
            table.data[table.count] = (row.c, at);
            table.count += 1;
        }
        Ok(table)
    }

    /// The table of `os` with NO data row bound (a test with no layer installed): a data row
    /// asked of it is refused by [`Bind::address`].
    #[cfg(test)]
    pub(crate) fn no_data(os: Os) -> Table {
        Table { os, data: [("", 0); MOST_DATA_ROWS], count: 0 }
    }

    /// The system this table answers for.
    pub fn os(&self) -> Os {
        self.os
    }
}

impl Bind for Table {
    fn address(&mut self, name: &str, cname: &CStr) -> Result<u64, BindError> {
        // A data row: the address `bind` took on the layer's thread; the cell is not touched.
        if let Some(&(_, at)) = self.data[..self.count].iter().find(|(row, _)| *row == name) {
            return Ok(at as u64);
        }
        if matches!(find(name, self.os), Ok(row) if row.how == How::Data) {
            return Err(BindError::DataUnbound);
        }
        match binding(name, self.os) {
            Ok(Binding::Here(at)) => Ok(at as u64),
            // A row the table admits as the process's own (memory, strings, unwinding).
            Ok(Binding::Own) => Dlsym.address(name, cname),
            Err(refused) => Err(match refused.why {
                Unbound::NotInTable => BindError::NotInTable,
                Unbound::OtherSystem { .. } => BindError::OtherSystem,
            }),
        }
    }
}
