//! Async-signal-safe path remapping for compile-time `env!("CARGO_BIN_EXE_<name>")` guest paths
//! (`/target/debug/bin/<package>/<name>`) to the test run's host launcher stubs (`test.md` piece 17).
//!
//! On Linux, `/target/debug/bin` is physically bind-mounted read-only inside the sandbox's private
//! mount namespace (`pivot_root_with_bins`), so every process sees real files at the guest path
//! without needing `BLAZE_LAUNCH_BIN_MAP`.
//!
//! On macOS, root `/` is an immutable APFS Sealed System Volume (SIP) with no unprivileged mount
//! namespaces, so `/target` cannot be created by an unprivileged test runner. Instead, runtime
//! `CARGO_BIN_EXE_<name>` carries the real host stub path and this table remaps compile-time
//! `/target/debug/bin/<package>/<name>` literals across `open`/`openat`/`fopen`/`stat*`/`access*`/
//! `realpath`/`readlink*` AND across every process-launching entry point (`execv`/`execve`/
//! `execvp`/`execvpe`/`execl`/`execlp`/`execle`/`posix_spawn`/`posix_spawnp`/`system`/`popen`)
//! so child processes such as `/bin/sh` receive the real host stub path.
//!
//! Installed once on the launcher's main thread before any guest thread starts (from
//! `BLAZE_LAUNCH_BIN_MAP`, which is removed from the process environment immediately after).
//! Every wrapper is lock-free and async-signal-safe after `fork()`: small calls use a stack
//! fast-path with zero syscalls, and arbitrarily large `argv`/`envp` or command buffers fall back
//! to a raw `mmap(MAP_PRIVATE | MAP_ANON)` region freed via `munmap` on drop (no malloc locks, no
//! artificial 255-argument cap).

use core::ffi::{CStr, c_char, c_int, c_void};
use core::mem::{align_of, size_of};
use core::ptr;
use core::slice;
use core::sync::atomic::{AtomicPtr, Ordering};

use blaze_own::{ABox, AVec, Alloc, Box, Vec};

/// Fixed compile-time guest prefix emitted by `blaze_scope::crate_env::bin_exe`.
const GUEST_PREFIX: &[u8] = b"/target/debug/bin/";

/// Fast-path stack pointer count before falling back to async-signal-safe anonymous `mmap`.
const STACK_PTRS: usize = 256;

/// Fast-path stack scratch bytes before falling back to async-signal-safe anonymous `mmap`.
const STACK_SCRATCH: usize = 16 * 1024;

struct Entry {
    guest: AVec<'static, u8>,
    /// NUL-terminated host stub path bytes.
    host: AVec<'static, u8>,
}

struct RemapTable {
    entries: AVec<'static, Entry>,
}

static MAP: AtomicPtr<RemapTable> = AtomicPtr::new(ptr::null_mut());

/// Why `BLAZE_LAUNCH_BIN_MAP` was refused before guest code started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinRemapError {
    /// A line lacked the `<guest>\t<host>` tab separator or had an empty side.
    MalformedLine,
    /// The guest path did not begin with `/target/debug/bin/`.
    BadGuestPrefix,
    /// Either path contained an embedded NUL byte.
    NulByte,
}

impl core::fmt::Display for BinRemapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BinRemapError::MalformedLine => f.write_str("BLAZE_LAUNCH_BIN_MAP holds a line without `<guest>\\t<host>`"),
            BinRemapError::BadGuestPrefix => f.write_str("BLAZE_LAUNCH_BIN_MAP guest path does not start with `/target/debug/bin/`"),
            BinRemapError::NulByte => f.write_str("BLAZE_LAUNCH_BIN_MAP holds a path with an embedded NUL byte"),
        }
    }
}

/// Parses `raw` (`<guest>\t<host>\n` lines) and installs the process-wide immutable remap table.
/// Returns `Ok(true)` if at least one mapping was installed, `Ok(false)` if `raw` was empty.
pub fn install(raw: &[u8], alloc: Alloc<'static>) -> Result<bool, BinRemapError> {
    let entries = parse_entries(raw, alloc)?;
    if entries.is_empty() {
        return Ok(false);
    }
    let table: ABox<'static, RemapTable> = Box::new_in(RemapTable { entries }, alloc);
    let (ptr, alloc) = Box::into_raw_with_allocator(table);
    if MAP
        .compare_exchange(ptr::null_mut(), ptr, Ordering::Release, Ordering::Relaxed)
        .is_err()
    {
        // SAFETY: `ptr` was never published; reclaim it with the same `alloc`.
        drop(unsafe { Box::from_raw_in(ptr, alloc) });
    }
    Ok(true)
}

fn parse_entries(raw: &[u8], alloc: Alloc<'static>) -> Result<AVec<'static, Entry>, BinRemapError> {
    let mut out = Vec::new_in(alloc);
    for line in raw.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Some(tab) = line.iter().position(|&b| b == b'\t') else {
            return Err(BinRemapError::MalformedLine);
        };
        let Some(guest) = line.get(..tab) else {
            return Err(BinRemapError::MalformedLine);
        };
        let Some(host) = line.get(tab + 1..) else {
            return Err(BinRemapError::MalformedLine);
        };
        if guest.is_empty() || host.is_empty() {
            return Err(BinRemapError::MalformedLine);
        }
        if !guest.starts_with(GUEST_PREFIX) {
            return Err(BinRemapError::BadGuestPrefix);
        }
        if guest.contains(&0) || host.contains(&0) {
            return Err(BinRemapError::NulByte);
        }
        let mut guest_buf = Vec::with_capacity_in(guest.len(), alloc);
        guest_buf.extend_from_slice(guest);
        let mut host_buf = Vec::with_capacity_in(host.len() + 1, alloc);
        host_buf.extend_from_slice(host);
        host_buf.push(0);
        out.push(Entry { guest: guest_buf, host: host_buf });
    }
    Ok(out)
}

/// Returns whether byte `b` can continue a package/binary path component after `entry.guest`.
#[inline]
fn is_path_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/')
}

/// Finds the first index of `needle` in `haystack` starting at `from`.
fn find_subslice(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() || haystack.len() - from < needle.len() {
        return None;
    }
    let last = haystack.len() - needle.len();
    let first_byte = *needle.first()?;
    let mut i = from;
    while i <= last {
        if haystack.get(i).copied() == Some(first_byte) && haystack.get(i..i + needle.len()) == Some(needle) {
            return Some(i);
        }
        i += 1;
    }
    None
}

enum MeasureResult {
    Unchanged,
    Exact(*const c_char),
    /// Exact number of bytes required including the trailing NUL byte.
    NeedsBytes(usize),
}

/// Measures whether `input` requires rewriting and, if embedded rewriting is needed, returns the
/// exact output byte count (including trailing NUL) without allocating.
fn measure_rewrite(input: &[u8], entries: &[Entry]) -> Result<MeasureResult, ()> {
    let Some(first_hit) = find_subslice(input, GUEST_PREFIX, 0) else {
        return Ok(MeasureResult::Unchanged);
    };
    if first_hit == 0 {
        for entry in entries.iter() {
            if entry.guest.as_slice() == input {
                return Ok(MeasureResult::Exact(entry.host.as_ptr().cast::<c_char>()));
            }
        }
    }
    let mut pos = 0usize;
    let mut out_len = 0usize;
    let mut changed = false;
    while pos < input.len() {
        let Some(hit) = find_subslice(input, GUEST_PREFIX, pos) else {
            break;
        };
        let mut matched: Option<&Entry> = None;
        for entry in entries.iter() {
            let g = entry.guest.as_slice();
            if input.get(hit..hit + g.len()) == Some(g) {
                let next_ok = input.get(hit + g.len()).is_none_or(|&b| !is_path_cont(b));
                if next_ok && matched.is_none_or(|m| g.len() > m.guest.len()) {
                    matched = Some(entry);
                }
            }
        }
        let Some(entry) = matched else {
            let copy_end = hit + GUEST_PREFIX.len();
            if changed {
                out_len = out_len.checked_add(copy_end - pos).ok_or(())?;
            }
            pos = copy_end;
            continue;
        };
        if !changed {
            changed = true;
            out_len = hit;
        } else {
            out_len = out_len.checked_add(hit - pos).ok_or(())?;
        }
        let host_len = entry.host.len().saturating_sub(1);
        out_len = out_len.checked_add(host_len).ok_or(())?;
        pos = hit + entry.guest.len();
    }
    if !changed {
        return Ok(MeasureResult::Unchanged);
    }
    out_len = out_len.checked_add(input.len() - pos).ok_or(())?;
    out_len = out_len.checked_add(1).ok_or(())?;
    Ok(MeasureResult::NeedsBytes(out_len))
}

enum RewriteResult {
    Unchanged,
    Exact(*const c_char),
    Rewritten { start: usize, end: usize },
}

/// Rewrites every boundary-delimited occurrence of an installed `entry.guest` inside `input` into
/// `scratch[*cursor..]` (NUL-terminated). Fails closed with `Err(())` on scratch overflow.
fn rewrite_bytes(
    input: &[u8],
    entries: &[Entry],
    scratch: &mut [u8],
    cursor: &mut usize,
) -> Result<RewriteResult, ()> {
    let Some(first_hit) = find_subslice(input, GUEST_PREFIX, 0) else {
        return Ok(RewriteResult::Unchanged);
    };
    if first_hit == 0 {
        for entry in entries.iter() {
            if entry.guest.as_slice() == input {
                return Ok(RewriteResult::Exact(entry.host.as_ptr().cast::<c_char>()));
            }
        }
    }
    let start = *cursor;
    let mut pos = 0usize;
    let mut changed = false;
    while pos < input.len() {
        let Some(hit) = find_subslice(input, GUEST_PREFIX, pos) else {
            break;
        };
        let mut matched: Option<&Entry> = None;
        for entry in entries.iter() {
            let g = entry.guest.as_slice();
            if input.get(hit..hit + g.len()) == Some(g) {
                let next_ok = input.get(hit + g.len()).is_none_or(|&b| !is_path_cont(b));
                if next_ok && matched.is_none_or(|m| g.len() > m.guest.len()) {
                    matched = Some(entry);
                }
            }
        }
        let Some(entry) = matched else {
            let copy_end = hit + GUEST_PREFIX.len();
            if changed {
                let seg = input.get(pos..copy_end).ok_or(())?;
                let dst = scratch.get_mut(*cursor..cursor.checked_add(seg.len()).ok_or(())?).ok_or(())?;
                dst.copy_from_slice(seg);
                *cursor += seg.len();
            }
            pos = copy_end;
            continue;
        };
        if !changed {
            changed = true;
            let prefix = input.get(..hit).ok_or(())?;
            let dst = scratch.get_mut(*cursor..cursor.checked_add(prefix.len()).ok_or(())?).ok_or(())?;
            dst.copy_from_slice(prefix);
            *cursor += prefix.len();
        } else {
            let gap = input.get(pos..hit).ok_or(())?;
            let dst = scratch.get_mut(*cursor..cursor.checked_add(gap.len()).ok_or(())?).ok_or(())?;
            dst.copy_from_slice(gap);
            *cursor += gap.len();
        }
        let host_bytes = entry.host.get(..entry.host.len().saturating_sub(1)).ok_or(())?;
        let dst = scratch.get_mut(*cursor..cursor.checked_add(host_bytes.len()).ok_or(())?).ok_or(())?;
        dst.copy_from_slice(host_bytes);
        *cursor += host_bytes.len();
        pos = hit + entry.guest.len();
    }
    if !changed {
        return Ok(RewriteResult::Unchanged);
    }
    let tail = input.get(pos..).ok_or(())?;
    let dst = scratch.get_mut(*cursor..cursor.checked_add(tail.len()).ok_or(())?).ok_or(())?;
    dst.copy_from_slice(tail);
    *cursor += tail.len();
    let nul = scratch.get_mut(*cursor).ok_or(())?;
    *nul = 0;
    *cursor += 1;
    Ok(RewriteResult::Rewritten { start, end: *cursor })
}

/// Async-signal-safe anonymous `mmap` allocation used ONLY when a rewritten `argv`/`envp` or
/// command string exceeds the stack fast-path (`STACK_PTRS` / `STACK_SCRATCH`).
/// Automatically unmapped on drop (or reclaimed by the kernel on successful `exec*`).
struct MmapGuard {
    ptr: *mut c_void,
    len: usize,
}

impl MmapGuard {
    const EMPTY: MmapGuard = MmapGuard { ptr: ptr::null_mut(), len: 0 };

    fn alloc(len: usize) -> Result<MmapGuard, ()> {
        if len == 0 {
            return Ok(MmapGuard::EMPTY);
        }
        // SAFETY: anonymous private mapping touches no allocator locks and is async-signal-safe.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED || ptr.is_null() {
            return Err(());
        }
        Ok(MmapGuard { ptr, len })
    }
}

impl Drop for MmapGuard {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.len > 0 {
            // SAFETY: `self.ptr` and `self.len` came from `libc::mmap` in `MmapGuard::alloc`.
            unsafe {
                let _ = libc::munmap(self.ptr, self.len);
            }
        }
    }
}

/// Rewrites a NUL-terminated `char* const*` list (`argv` or `envp`) whenever any entry matches or
/// embeds an installed `/target/debug/bin/<package>/<name>` path.
///
/// Uses caller stack storage (`stack_ptrs` + `stack_scratch`) on the fast path, and falls back to
/// an async-signal-safe anonymous `mmap` stored in `mmap_guard` for arbitrarily large vectors.
unsafe fn remap_cstr_list_with(
    list: *const *const c_char,
    entries: &[Entry],
    stack_ptrs: &mut [*const c_char; STACK_PTRS],
    stack_scratch: &mut [u8; STACK_SCRATCH],
    mmap_guard: &mut MmapGuard,
) -> Result<*const *const c_char, ()> {
    if list.is_null() {
        return Ok(list);
    }
    // Pass 1: measure total pointer count and rewritten byte budget without allocating.
    let mut count = 0usize;
    let mut any_changed = false;
    let mut need_bytes = 0usize;
    loop {
        // SAFETY: caller passes a valid NUL-terminated `argv`/`envp` pointer array.
        let elem = unsafe { *list.add(count) };
        if elem.is_null() {
            break;
        }
        let bytes = unsafe { CStr::from_ptr(elem) }.to_bytes();
        match measure_rewrite(bytes, entries)? {
            MeasureResult::Unchanged => {}
            MeasureResult::Exact(_) => any_changed = true,
            MeasureResult::NeedsBytes(n) => {
                any_changed = true;
                need_bytes = need_bytes.checked_add(n).ok_or(())?;
            }
        }
        count = count.checked_add(1).ok_or(())?;
    }
    if !any_changed {
        return Ok(list);
    }
    let ptr_slots = count.checked_add(1).ok_or(())?;
    let (ptrs_slice, scratch_slice): (&mut [*const c_char], &mut [u8]) =
        if ptr_slots <= STACK_PTRS && need_bytes <= STACK_SCRATCH {
            (stack_ptrs.as_mut_slice(), stack_scratch.as_mut_slice())
        } else {
            let ptr_bytes = ptr_slots.checked_mul(size_of::<*const c_char>()).ok_or(())?;
            let total = ptr_bytes.checked_add(need_bytes).ok_or(())?;
            *mmap_guard = MmapGuard::alloc(total)?;
            // SAFETY: `mmap` returns page-aligned memory (>= `align_of::<*const c_char>()`),
            // valid for `ptr_bytes + need_bytes` bytes, exclusively owned by `mmap_guard`.
            let _ = align_of::<*const c_char>();
            let p_slice = unsafe { slice::from_raw_parts_mut(mmap_guard.ptr.cast::<*const c_char>(), ptr_slots) };
            let b_ptr = unsafe { mmap_guard.ptr.cast::<u8>().add(ptr_bytes) };
            let b_slice = unsafe { slice::from_raw_parts_mut(b_ptr, need_bytes) };
            (p_slice, b_slice)
        };

    // Pass 2: populate pointer table and rewritten strings.
    let mut cursor = 0usize;
    for i in 0..count {
        // SAFETY: `i < count`, valid elements of `list`.
        let elem = unsafe { *list.add(i) };
        let bytes = unsafe { CStr::from_ptr(elem) }.to_bytes();
        let out_ptr = match rewrite_bytes(bytes, entries, scratch_slice, &mut cursor)? {
            RewriteResult::Unchanged => elem,
            RewriteResult::Exact(host) => host,
            RewriteResult::Rewritten { start, .. } => {
                // SAFETY: `start < scratch_slice.len()` and string is NUL-terminated.
                unsafe { scratch_slice.as_ptr().add(start).cast::<c_char>() }
            }
        };
        *ptrs_slice.get_mut(i).ok_or(())? = out_ptr;
    }
    *ptrs_slice.get_mut(count).ok_or(())? = ptr::null();
    Ok(ptrs_slice.as_ptr())
}

unsafe fn remap_cstr_list(
    list: *const *const c_char,
    stack_ptrs: &mut [*const c_char; STACK_PTRS],
    stack_scratch: &mut [u8; STACK_SCRATCH],
    mmap_guard: &mut MmapGuard,
) -> Result<*const *const c_char, ()> {
    if list.is_null() {
        return Ok(list);
    }
    let map_ptr = MAP.load(Ordering::Acquire);
    if map_ptr.is_null() {
        return Ok(list);
    }
    // SAFETY: `map_ptr` was published once and never freed for the life of the process.
    let entries = unsafe { (*map_ptr).entries.as_slice() };
    unsafe { remap_cstr_list_with(list, entries, stack_ptrs, stack_scratch, mmap_guard) }
}

/// Rewrites a single shell command C string (for `system` / `popen`) into stack storage or
/// `mmap_guard` when larger than `STACK_SCRATCH`.
unsafe fn remap_cmd_cstr(
    cmd: *const c_char,
    stack_scratch: &mut [u8; STACK_SCRATCH],
    mmap_guard: &mut MmapGuard,
) -> Result<*const c_char, ()> {
    if cmd.is_null() {
        return Ok(cmd);
    }
    let map_ptr = MAP.load(Ordering::Acquire);
    if map_ptr.is_null() {
        return Ok(cmd);
    }
    // SAFETY: `map_ptr` was published once and never freed for the life of the process.
    let entries = unsafe { (*map_ptr).entries.as_slice() };
    let bytes = unsafe { CStr::from_ptr(cmd) }.to_bytes();
    match measure_rewrite(bytes, entries)? {
        MeasureResult::Unchanged => Ok(cmd),
        MeasureResult::Exact(host) => Ok(host),
        MeasureResult::NeedsBytes(need) => {
            let buf: &mut [u8] = if need <= STACK_SCRATCH {
                stack_scratch.as_mut_slice()
            } else {
                *mmap_guard = MmapGuard::alloc(need)?;
                // SAFETY: `mmap_guard` owns `need` writable bytes.
                unsafe { slice::from_raw_parts_mut(mmap_guard.ptr.cast::<u8>(), need) }
            };
            let mut cursor = 0usize;
            match rewrite_bytes(bytes, entries, buf, &mut cursor)? {
                RewriteResult::Unchanged => Ok(cmd),
                RewriteResult::Exact(host) => Ok(host),
                RewriteResult::Rewritten { start, .. } => {
                    Ok(unsafe { buf.as_ptr().add(start).cast::<c_char>() })
                }
            }
        }
    }
}

#[inline]
fn set_errno(val: c_int) {
    #[cfg(target_os = "linux")]
    // SAFETY: `__errno_location()` returns the calling thread's valid `errno` pointer.
    unsafe {
        *libc::__errno_location() = val;
    }
    #[cfg(target_os = "macos")]
    // SAFETY: `__error()` returns the calling thread's valid `errno` pointer.
    unsafe {
        *libc::__error() = val;
    }
}

/// Maps `path` to its host launcher stub C string when it matches an installed guest binary path;
/// otherwise returns `path` unchanged. Lock-free, zero-allocation, and async-signal-safe.
#[inline]
pub(crate) unsafe fn remap_cstr(path: *const c_char) -> *const c_char {
    if path.is_null() {
        return path;
    }
    // Fast-path exit for every relative path (single byte load).
    // SAFETY: caller passes a valid NUL-terminated C string pointer when non-null.
    if unsafe { *path.cast::<u8>() } != b'/' {
        return path;
    }
    let ptr = MAP.load(Ordering::Acquire);
    if ptr.is_null() {
        return path;
    }
    // SAFETY: `ptr` was published once and never freed for the life of the process.
    let entries = unsafe { (*ptr).entries.as_slice() };
    // SAFETY: caller passes a valid NUL-terminated C string pointer.
    let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
    if !bytes.starts_with(GUEST_PREFIX) {
        return path;
    }
    for entry in entries.iter() {
        if entry.guest.as_slice() == bytes {
            return entry.host.as_ptr().cast::<c_char>();
        }
    }
    path
}

pub(crate) extern "C" fn remap_execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    let mut argv_ptrs = [ptr::null(); STACK_PTRS];
    let mut envp_ptrs = [ptr::null(); STACK_PTRS];
    let mut argv_scratch = [0u8; STACK_SCRATCH];
    let mut envp_scratch = [0u8; STACK_SCRATCH];
    let mut argv_mmap = MmapGuard::EMPTY;
    let mut envp_mmap = MmapGuard::EMPTY;
    let Ok(new_argv) = (unsafe { remap_cstr_list(argv, &mut argv_ptrs, &mut argv_scratch, &mut argv_mmap) }) else {
        set_errno(libc::ENOMEM);
        return -1;
    };
    let Ok(new_envp) = (unsafe { remap_cstr_list(envp, &mut envp_ptrs, &mut envp_scratch, &mut envp_mmap) }) else {
        set_errno(libc::ENOMEM);
        return -1;
    };
    // SAFETY: `remap_cstr`, `new_argv`, and `new_envp` stay live across `execve`.
    unsafe { libc::execve(remap_cstr(path), new_argv, new_envp) }
}

pub(crate) extern "C" fn remap_execv(path: *const c_char, argv: *const *const c_char) -> c_int {
    let mut argv_ptrs = [ptr::null(); STACK_PTRS];
    let mut argv_scratch = [0u8; STACK_SCRATCH];
    let mut argv_mmap = MmapGuard::EMPTY;
    let Ok(new_argv) = (unsafe { remap_cstr_list(argv, &mut argv_ptrs, &mut argv_scratch, &mut argv_mmap) }) else {
        set_errno(libc::ENOMEM);
        return -1;
    };
    // SAFETY: `remap_cstr` and `new_argv` stay live across `execv`.
    unsafe { libc::execv(remap_cstr(path), new_argv) }
}

pub(crate) extern "C" fn remap_execvp(file: *const c_char, argv: *const *const c_char) -> c_int {
    let mut argv_ptrs = [ptr::null(); STACK_PTRS];
    let mut argv_scratch = [0u8; STACK_SCRATCH];
    let mut argv_mmap = MmapGuard::EMPTY;
    let Ok(new_argv) = (unsafe { remap_cstr_list(argv, &mut argv_ptrs, &mut argv_scratch, &mut argv_mmap) }) else {
        set_errno(libc::ENOMEM);
        return -1;
    };
    // SAFETY: `remap_cstr` and `new_argv` stay live across `execvp`.
    unsafe { libc::execvp(remap_cstr(file), new_argv) }
}

#[cfg(target_os = "linux")]
pub(crate) extern "C" fn remap_execvpe(
    file: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    let mut argv_ptrs = [ptr::null(); STACK_PTRS];
    let mut envp_ptrs = [ptr::null(); STACK_PTRS];
    let mut argv_scratch = [0u8; STACK_SCRATCH];
    let mut envp_scratch = [0u8; STACK_SCRATCH];
    let mut argv_mmap = MmapGuard::EMPTY;
    let mut envp_mmap = MmapGuard::EMPTY;
    let Ok(new_argv) = (unsafe { remap_cstr_list(argv, &mut argv_ptrs, &mut argv_scratch, &mut argv_mmap) }) else {
        set_errno(libc::ENOMEM);
        return -1;
    };
    let Ok(new_envp) = (unsafe { remap_cstr_list(envp, &mut envp_ptrs, &mut envp_scratch, &mut envp_mmap) }) else {
        set_errno(libc::ENOMEM);
        return -1;
    };
    // SAFETY: `remap_cstr`, `new_argv`, and `new_envp` stay live across `execvpe`.
    unsafe { libc::execvpe(remap_cstr(file), new_argv, new_envp) }
}

/// Collects variadic `execl`/`execlp`/`execle` arguments into a stack buffer (falling back to
/// async-signal-safe anonymous `mmap` when > `STACK_PTRS - 1` args), then calls `f(argv, envp)`.
unsafe fn collect_execl_args<F>(
    arg0: *const c_char,
    mut args: core::ffi::VaList<'_>,
    has_envp: bool,
    f: F,
) -> c_int
where
    F: FnOnce(*const *const c_char, *const *const c_char) -> c_int,
{
    let mut stack = [ptr::null::<c_char>(); STACK_PTRS];
    let mut count = 0usize;
    let mut mmap = MmapGuard::EMPTY;
    let mut mmap_cap = 0usize;
    let mut cur = arg0;
    loop {
        if mmap.ptr.is_null() {
            if count < STACK_PTRS - 1 {
                if let Some(slot) = stack.get_mut(count) {
                    *slot = cur;
                }
            } else {
                let new_cap = STACK_PTRS.saturating_mul(2);
                let Some(bytes) = new_cap.checked_mul(size_of::<*const c_char>()) else {
                    set_errno(libc::ENOMEM);
                    return -1;
                };
                let Ok(buf) = MmapGuard::alloc(bytes) else {
                    set_errno(libc::ENOMEM);
                    return -1;
                };
                // SAFETY: `buf` is page-aligned and holds `new_cap` pointers.
                unsafe {
                    ptr::copy_nonoverlapping(stack.as_ptr(), buf.ptr.cast::<*const c_char>(), count);
                    *buf.ptr.cast::<*const c_char>().add(count) = cur;
                }
                mmap = buf;
                mmap_cap = new_cap;
            }
        } else {
            if count >= mmap_cap - 1 {
                let Some(new_cap) = mmap_cap.checked_mul(2) else {
                    set_errno(libc::ENOMEM);
                    return -1;
                };
                let Some(bytes) = new_cap.checked_mul(size_of::<*const c_char>()) else {
                    set_errno(libc::ENOMEM);
                    return -1;
                };
                let Ok(buf) = MmapGuard::alloc(bytes) else {
                    set_errno(libc::ENOMEM);
                    return -1;
                };
                // SAFETY: `buf` holds `new_cap` pointers; old `mmap` is unmapped on replacement.
                unsafe {
                    ptr::copy_nonoverlapping(mmap.ptr.cast::<*const c_char>(), buf.ptr.cast::<*const c_char>(), count);
                }
                mmap = buf;
                mmap_cap = new_cap;
            }
            // SAFETY: `count < mmap_cap - 1`.
            unsafe {
                *mmap.ptr.cast::<*const c_char>().add(count) = cur;
            }
        }
        if cur.is_null() {
            break;
        }
        count += 1;
        // SAFETY: POSIX `execl*` variadic arguments are `*const c_char` terminated by `NULL`.
        cur = unsafe { args.next_arg::<*const c_char>() };
    }
    let envp = if has_envp {
        // SAFETY: POSIX `execle` passes `envp: *const *const c_char` immediately after `NULL`.
        unsafe { args.next_arg::<*const *const c_char>() }
    } else {
        ptr::null()
    };
    let argv = if mmap.ptr.is_null() {
        stack.as_ptr()
    } else {
        mmap.ptr.cast::<*const c_char>()
    };
    f(argv, envp)
}

pub(crate) unsafe extern "C" fn remap_execl(
    path: *const c_char,
    arg0: *const c_char,
    args: ...
) -> c_int {
    unsafe { collect_execl_args(arg0, args, false, |argv, _| remap_execv(path, argv)) }
}

pub(crate) unsafe extern "C" fn remap_execlp(
    file: *const c_char,
    arg0: *const c_char,
    args: ...
) -> c_int {
    unsafe { collect_execl_args(arg0, args, false, |argv, _| remap_execvp(file, argv)) }
}

pub(crate) unsafe extern "C" fn remap_execle(
    path: *const c_char,
    arg0: *const c_char,
    args: ...
) -> c_int {
    unsafe { collect_execl_args(arg0, args, true, |argv, envp| remap_execve(path, argv, envp)) }
}

pub(crate) extern "C" fn remap_posix_spawn(
    pid: *mut libc::pid_t,
    path: *const c_char,
    file_actions: *const libc::posix_spawn_file_actions_t,
    attrp: *const libc::posix_spawnattr_t,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    let mut argv_ptrs = [ptr::null(); STACK_PTRS];
    let mut envp_ptrs = [ptr::null(); STACK_PTRS];
    let mut argv_scratch = [0u8; STACK_SCRATCH];
    let mut envp_scratch = [0u8; STACK_SCRATCH];
    let mut argv_mmap = MmapGuard::EMPTY;
    let mut envp_mmap = MmapGuard::EMPTY;
    let Ok(new_argv) = (unsafe { remap_cstr_list(argv.cast(), &mut argv_ptrs, &mut argv_scratch, &mut argv_mmap) }) else {
        return libc::ENOMEM;
    };
    let Ok(new_envp) = (unsafe { remap_cstr_list(envp.cast(), &mut envp_ptrs, &mut envp_scratch, &mut envp_mmap) }) else {
        return libc::ENOMEM;
    };
    // SAFETY: POSIX `posix_spawn` never mutates `argv`/`envp` strings; buffers outlive the call.
    unsafe {
        libc::posix_spawn(
            pid,
            remap_cstr(path),
            file_actions,
            attrp,
            new_argv.cast::<*mut c_char>(),
            new_envp.cast::<*mut c_char>(),
        )
    }
}

pub(crate) extern "C" fn remap_posix_spawnp(
    pid: *mut libc::pid_t,
    file: *const c_char,
    file_actions: *const libc::posix_spawn_file_actions_t,
    attrp: *const libc::posix_spawnattr_t,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    let mut argv_ptrs = [ptr::null(); STACK_PTRS];
    let mut envp_ptrs = [ptr::null(); STACK_PTRS];
    let mut argv_scratch = [0u8; STACK_SCRATCH];
    let mut envp_scratch = [0u8; STACK_SCRATCH];
    let mut argv_mmap = MmapGuard::EMPTY;
    let mut envp_mmap = MmapGuard::EMPTY;
    let Ok(new_argv) = (unsafe { remap_cstr_list(argv.cast(), &mut argv_ptrs, &mut argv_scratch, &mut argv_mmap) }) else {
        return libc::ENOMEM;
    };
    let Ok(new_envp) = (unsafe { remap_cstr_list(envp.cast(), &mut envp_ptrs, &mut envp_scratch, &mut envp_mmap) }) else {
        return libc::ENOMEM;
    };
    // SAFETY: POSIX `posix_spawnp` never mutates `argv`/`envp` strings; buffers outlive the call.
    unsafe {
        libc::posix_spawnp(
            pid,
            remap_cstr(file),
            file_actions,
            attrp,
            new_argv.cast::<*mut c_char>(),
            new_envp.cast::<*mut c_char>(),
        )
    }
}

pub(crate) extern "C" fn remap_system(command: *const c_char) -> c_int {
    let mut scratch = [0u8; STACK_SCRATCH];
    let mut mmap = MmapGuard::EMPTY;
    let Ok(cmd) = (unsafe { remap_cmd_cstr(command, &mut scratch, &mut mmap) }) else {
        set_errno(libc::ENOMEM);
        return -1;
    };
    // SAFETY: `cmd` is null or a valid NUL-terminated string live across `libc::system`.
    unsafe { libc::system(cmd) }
}

pub(crate) extern "C" fn remap_popen(
    command: *const c_char,
    mode: *const c_char,
) -> *mut libc::FILE {
    let mut scratch = [0u8; STACK_SCRATCH];
    let mut mmap = MmapGuard::EMPTY;
    let Ok(cmd) = (unsafe { remap_cmd_cstr(command, &mut scratch, &mut mmap) }) else {
        set_errno(libc::ENOMEM);
        return ptr::null_mut();
    };
    // SAFETY: `cmd` and `mode` are valid NUL-terminated C strings live across `libc::popen`.
    unsafe { libc::popen(cmd, mode) }
}

pub(crate) extern "C" fn remap_stat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::stat(remap_cstr(path), buf) }
}

pub(crate) extern "C" fn remap_lstat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::lstat(remap_cstr(path), buf) }
}

pub(crate) extern "C" fn remap_fstatat(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut libc::stat,
    flags: c_int,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::fstatat(dirfd, remap_cstr(path), buf, flags) }
}

#[cfg(target_os = "linux")]
pub(crate) extern "C" fn remap_stat64(path: *const c_char, buf: *mut libc::stat64) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::stat64(remap_cstr(path), buf) }
}

#[cfg(target_os = "linux")]
pub(crate) extern "C" fn remap_lstat64(path: *const c_char, buf: *mut libc::stat64) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::lstat64(remap_cstr(path), buf) }
}

#[cfg(target_os = "linux")]
pub(crate) extern "C" fn remap_fstatat64(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut libc::stat64,
    flags: c_int,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::fstatat64(dirfd, remap_cstr(path), buf, flags) }
}

#[cfg(target_os = "linux")]
pub(crate) extern "C" fn remap_statx(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    mask: libc::c_uint,
    buf: *mut libc::statx,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::statx(dirfd, remap_cstr(path), flags, mask, buf) }
}

pub(crate) extern "C" fn remap_access(path: *const c_char, amode: c_int) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::access(remap_cstr(path), amode) }
}

pub(crate) extern "C" fn remap_faccessat(
    dirfd: c_int,
    path: *const c_char,
    amode: c_int,
    flags: c_int,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::faccessat(dirfd, remap_cstr(path), amode, flags) }
}

pub(crate) extern "C" fn remap_realpath(
    path: *const c_char,
    resolved: *mut c_char,
) -> *mut c_char {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::realpath(remap_cstr(path), resolved) }
}

pub(crate) extern "C" fn remap_readlink(
    path: *const c_char,
    buf: *mut c_char,
    bufsiz: libc::size_t,
) -> libc::ssize_t {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::readlink(remap_cstr(path), buf, bufsiz) }
}

pub(crate) extern "C" fn remap_readlinkat(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut c_char,
    bufsiz: libc::size_t,
) -> libc::ssize_t {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::readlinkat(dirfd, remap_cstr(path), buf, bufsiz) }
}

pub(crate) extern "C" fn remap_fopen(
    path: *const c_char,
    mode: *const c_char,
) -> *mut libc::FILE {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::fopen(remap_cstr(path), mode) }
}

pub(crate) unsafe extern "C" fn remap_open(
    path: *const c_char,
    oflag: c_int,
    mut args: ...
) -> c_int {
    let remapped = unsafe { remap_cstr(path) };
    if (oflag & libc::O_CREAT) != 0 {
        let mode: libc::c_uint = unsafe { args.next_arg() };
        unsafe { libc::open(remapped, oflag, mode) }
    } else {
        unsafe { libc::open(remapped, oflag) }
    }
}

pub(crate) unsafe extern "C" fn remap_openat(
    dirfd: c_int,
    path: *const c_char,
    oflag: c_int,
    mut args: ...
) -> c_int {
    let remapped = unsafe { remap_cstr(path) };
    if (oflag & libc::O_CREAT) != 0 {
        let mode: libc::c_uint = unsafe { args.next_arg() };
        unsafe { libc::openat(dirfd, remapped, oflag, mode) }
    } else {
        unsafe { libc::openat(dirfd, remapped, oflag) }
    }
}

#[cfg(test)]
mod tests {
    use core::ffi::{CStr, c_char};
    use core::ptr;

    use blaze_own::Vec;

    use super::{
        BinRemapError, MmapGuard, RewriteResult, STACK_PTRS, STACK_SCRATCH, parse_entries,
        remap_cstr_list_with, rewrite_bytes,
    };

    #[test]
    fn parse_valid_and_rejects_malformed() {
        let alloc = blaze_own::testing::heap().alloc();
        let ok = parse_entries(b"/target/debug/bin/app/cli\t/tmp/stub/cli\n\n/target/debug/bin/app/srv\t/tmp/stub/srv\n", alloc)
            .expect("valid entries");
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0].guest.as_slice(), b"/target/debug/bin/app/cli");
        assert_eq!(ok[0].host.as_slice(), b"/tmp/stub/cli\0");
        assert_eq!(parse_entries(b"no-tab", alloc).err(), Some(BinRemapError::MalformedLine));
        assert_eq!(parse_entries(b"/usr/bin/foo\t/tmp/foo", alloc).err(), Some(BinRemapError::BadGuestPrefix));
        assert_eq!(parse_entries(b"/target/debug/bin/a/b\t/tmp/a\0b", alloc).err(), Some(BinRemapError::NulByte));
    }

    #[test]
    fn rewrite_bytes_handles_exact_embedded_env_boundary_and_overflow() {
        let alloc = blaze_own::testing::heap().alloc();
        let entries = parse_entries(
            b"/target/debug/bin/app/cli\t/host/b/app/cli\n/target/debug/bin/app/cli-v2\t/host/b/app/cli-v2\n",
            alloc,
        )
        .expect("entries");
        let mut scratch = [0u8; 256];
        let mut cursor = 0usize;

        assert!(matches!(
            rewrite_bytes(b"--flag=value", entries.as_slice(), &mut scratch, &mut cursor),
            Ok(RewriteResult::Unchanged)
        ));
        assert_eq!(cursor, 0);

        let Ok(RewriteResult::Exact(ptr)) =
            rewrite_bytes(b"/target/debug/bin/app/cli", entries.as_slice(), &mut scratch, &mut cursor)
        else {
            panic!("expected exact match");
        };
        assert_eq!(unsafe { CStr::from_ptr(ptr) }.to_bytes(), b"/host/b/app/cli");
        assert_eq!(cursor, 0);

        let Ok(RewriteResult::Rewritten { start, end }) = rewrite_bytes(
            b"\"/target/debug/bin/app/cli\" && /target/debug/bin/app/cli-v2 --ok",
            entries.as_slice(),
            &mut scratch,
            &mut cursor,
        ) else {
            panic!("expected rewritten shell command");
        };
        assert_eq!(&scratch[start..end - 1], b"\"/host/b/app/cli\" && /host/b/app/cli-v2 --ok");
        assert_eq!(scratch[end - 1], 0);

        assert!(matches!(
            rewrite_bytes(b"/target/debug/bin/app/cli2", entries.as_slice(), &mut scratch, &mut cursor),
            Ok(RewriteResult::Unchanged)
        ));

        let Ok(RewriteResult::Rewritten { start, end }) = rewrite_bytes(
            b"MY_BIN=/target/debug/bin/app/cli",
            entries.as_slice(),
            &mut scratch,
            &mut cursor,
        ) else {
            panic!("expected rewritten env");
        };
        assert_eq!(&scratch[start..end - 1], b"MY_BIN=/host/b/app/cli");

        let mut tiny = [0u8; 8];
        let mut tiny_cur = 0usize;
        assert!(
            rewrite_bytes(b"X=/target/debug/bin/app/cli", entries.as_slice(), &mut tiny, &mut tiny_cur).is_err()
        );
    }

    #[test]
    fn remap_cstr_list_handles_large_vectors_past_stack_threshold() {
        let alloc = blaze_own::testing::heap().alloc();
        let entries = parse_entries(b"/target/debug/bin/app/cli\t/host/b/app/cli\n", alloc).expect("entries");
        let plain = c"arg".as_ptr();
        let exact = c"/target/debug/bin/app/cli".as_ptr();
        let embedded = c"RUN=/target/debug/bin/app/cli --flag".as_ptr();
        let mut raw: Vec<*const c_char, _> = Vec::with_capacity_in(402, alloc);
        for i in 0..400usize {
            if i == 300 {
                raw.push(exact);
            } else if i == 350 {
                raw.push(embedded);
            } else {
                raw.push(plain);
            }
        }
        raw.push(ptr::null());

        let mut stack_ptrs = [ptr::null(); STACK_PTRS];
        let mut stack_scratch = [0u8; STACK_SCRATCH];
        let mut mmap_guard = MmapGuard::EMPTY;
        let out = unsafe {
            remap_cstr_list_with(
                raw.as_ptr(),
                entries.as_slice(),
                &mut stack_ptrs,
                &mut stack_scratch,
                &mut mmap_guard,
            )
        }
        .expect("400-element argv must succeed via anonymous mmap");
        assert!(!mmap_guard.ptr.is_null(), "expected mmap fallback for > STACK_PTRS entries");
        assert_eq!(unsafe { CStr::from_ptr(*out.add(0)) }.to_bytes(), b"arg");
        assert_eq!(unsafe { CStr::from_ptr(*out.add(300)) }.to_bytes(), b"/host/b/app/cli");
        assert_eq!(
            unsafe { CStr::from_ptr(*out.add(350)) }.to_bytes(),
            b"RUN=/host/b/app/cli --flag"
        );
        assert_eq!(unsafe { CStr::from_ptr(*out.add(399)) }.to_bytes(), b"arg");
        assert!(unsafe { *out.add(400) }.is_null());
    }
}
