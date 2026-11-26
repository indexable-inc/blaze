//! Async-signal-safe path remapping for compile-time `env!("CARGO_BIN_EXE_<name>")` guest paths
//! (`/target/debug/bin/<package>/<name>`) to the test run's host launcher stubs (`test.md` piece 17).
//!
//! On Linux, `/target/debug/bin` is physically bind-mounted read-only inside the sandbox's private
//! mount namespace (`pivot_root_with_bins`), so every process sees real files at the guest path
//! without needing `BLAZE_LAUNCH_BIN_MAP`.
//!
//! On macOS, root `/` is an immutable APFS Sealed System Volume (SIP) with no unprivileged mount
//! namespaces, so `/target` cannot be created by an unprivileged test runner. Instead, `BinServer`
//! places launcher stubs in a short, shell-token-safe user-private directory
//! `/tmp/bzb-<nonce>-r<suite>-<row>/<package>/<name>` (strictly `[a-zA-Z0-9/_.-]`, never spaces or
//! shell metacharacters regardless of `$HOME`), sets runtime `CARGO_BIN_EXE_<name>` to that path,
//! and this table remaps compile-time `/target/debug/bin/<package>/<name>` literals across
//! `open`/`openat`/`fopen`/`stat*`/`access*`/`realpath`/`readlink*` AND across every
//! process-launching entry point (`execv`/`execve`/`execvp`/`execvpe`/`execvP`/`execl`/`execlp`/
//! `execle`/`posix_spawn`/`posix_spawnp`/`system`/`popen`).
//!
//! Installed once on the launcher's main thread before any guest thread starts (from
//! `BLAZE_LAUNCH_BIN_MAP`, which is removed from the process environment immediately after).
//! Pre-allocates a demand-paged anonymous scratch pool ONCE at `install()` time (before any thread
//! or `fork()` exists) so every wrapper is strictly POSIX async-signal-safe between `fork()` and
//! `exec*` (zero `malloc`, zero `mmap`/`munmap`, zero mutexes), keeps stack frames tiny (safe on
//! 16 KiB `PTHREAD_STACK_MIN` thread stacks), and supports full `ARG_MAX` (256 KiB) argument and
//! environment vectors.

use core::ffi::{CStr, c_char, c_int};
use core::mem::{align_of, size_of};
use core::ptr;
use core::slice;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use blaze_own::{ABox, AVec, Alloc, Box, Vec};

/// Fixed compile-time guest prefix emitted by `blaze_scope::crate_env::bin_exe`.
const GUEST_PREFIX: &[u8] = b"/target/debug/bin/";

/// Fixed guest prefix without trailing slash (for `execvP` `search_path` matching).
const GUEST_ROOT: &[u8] = b"/target/debug/bin";

/// Small stack fast-path pointer count for variadic `execl*` collection (256 bytes on stack;
/// spills into a pre-allocated async-signal-safe scratch slot when > 31 arguments are passed).
const EXECL_STACK_PTRS: usize = 32;

/// Number of pre-allocated lock-free scratch slots reserved at `install()` time.
const SCRATCH_SLOTS: usize = 16;

/// Byte capacity per pre-allocated scratch slot (256 KiB = macOS `ARG_MAX`; demand-paged so
/// untouched pages consume 0 physical RSS).
const SCRATCH_SLOT_BYTES: usize = 256 * 1024;

struct Entry {
    guest: AVec<'static, u8>,
    /// NUL-terminated host stub path bytes.
    host: AVec<'static, u8>,
}

struct ScratchSlot {
    in_use: AtomicBool,
    base: *mut u8,
    len: usize,
}

struct RemapTable {
    entries: AVec<'static, Entry>,
    /// Parent directories derived from `entries` (`/target/debug/bin` and
    /// `/target/debug/bin/<package>`) mapped to NUL-terminated host directories so `chdir`,
    /// `opendir`, `stat`, and `posix_spawn_file_actions_addchdir_np` work seamlessly on parent
    /// paths derived from `env!("CARGO_BIN_EXE_*")`.
    dir_entries: AVec<'static, Entry>,
    /// Common host root prefix corresponding to `/target/debug/bin` (without trailing slash),
    /// derived from `entries` when all stubs share `<host_root>/<package>/<bin>`.
    host_root: AVec<'static, u8>,
    slots: [ScratchSlot; SCRATCH_SLOTS],
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
    /// Pre-mapping the async-signal-safe scratch pool at startup failed.
    ScratchAllocFailed,
}

impl core::fmt::Display for BinRemapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BinRemapError::MalformedLine => f.write_str("BLAZE_LAUNCH_BIN_MAP holds a line without `<guest>\\t<host>`"),
            BinRemapError::BadGuestPrefix => f.write_str("BLAZE_LAUNCH_BIN_MAP guest path does not start with `/target/debug/bin/`"),
            BinRemapError::NulByte => f.write_str("BLAZE_LAUNCH_BIN_MAP holds a path with an embedded NUL byte"),
            BinRemapError::ScratchAllocFailed => f.write_str("BLAZE_LAUNCH_BIN_MAP failed to reserve scratch pool"),
        }
    }
}

/// Derives the common `<host_root>` (without trailing `/`) corresponding to `/target/debug/bin`
/// from `entries`, when every entry has the form `/target/debug/bin/<rel> -> <host_root>/<rel>`.
fn derive_host_root(entries: &[Entry], alloc: Alloc<'static>) -> AVec<'static, u8> {
    let mut out = Vec::new_in(alloc);
    let Some(first) = entries.first() else { return out };
    let Some(rel) = first.guest.strip_prefix(GUEST_PREFIX) else { return out };
    let host_no_nul = first.host.get(..first.host.len().saturating_sub(1)).unwrap_or(&[]);
    let Some(prefix_with_slash) = host_no_nul.strip_suffix(rel) else { return out };
    let Some(root) = prefix_with_slash.strip_suffix(b"/") else { return out };
    for entry in entries.iter().skip(1) {
        let Some(r) = entry.guest.strip_prefix(GUEST_PREFIX) else { return out };
        let h = entry.host.get(..entry.host.len().saturating_sub(1)).unwrap_or(&[]);
        let Some(p) = h.strip_suffix(r).and_then(|s| s.strip_suffix(b"/")) else { return out };
        if p != root {
            return out;
        }
    }
    out.extend_from_slice(root);
    out
}

fn alloc_scratch_slots() -> Result<[ScratchSlot; SCRATCH_SLOTS], BinRemapError> {
    let total = SCRATCH_SLOTS
        .checked_mul(SCRATCH_SLOT_BYTES)
        .ok_or(BinRemapError::ScratchAllocFailed)?;
    // SAFETY: called once single-threaded at launcher startup before any guest thread or `fork()`.
    let base = unsafe {
        libc::mmap(
            ptr::null_mut(),
            total,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED || base.is_null() {
        return Err(BinRemapError::ScratchAllocFailed);
    }
    let base_u8 = base.cast::<u8>();
    Ok(core::array::from_fn(|i| ScratchSlot {
        in_use: AtomicBool::new(false),
        // SAFETY: `i * SCRATCH_SLOT_BYTES < total` within the mapped region.
        base: unsafe { base_u8.add(i * SCRATCH_SLOT_BYTES) },
        len: SCRATCH_SLOT_BYTES,
    }))
}

/// Derives deduplicated parent directory entries (`/target/debug/bin` and
/// `/target/debug/bin/<package>`) from `entries` and `host_root`.
fn derive_dir_entries(entries: &[Entry], host_root: &[u8], alloc: Alloc<'static>) -> AVec<'static, Entry> {
    let mut dirs = Vec::new_in(alloc);
    if !host_root.is_empty() {
        let mut g = Vec::with_capacity_in(GUEST_ROOT.len(), alloc);
        g.extend_from_slice(GUEST_ROOT);
        let mut h = Vec::with_capacity_in(host_root.len() + 1, alloc);
        h.extend_from_slice(host_root);
        h.push(0);
        dirs.push(Entry { guest: g, host: h });
    }
    for entry in entries.iter() {
        let Some(slash_g) = entry.guest.iter().rposition(|&b| b == b'/') else { continue };
        let Some(guest_dir) = entry.guest.get(..slash_g) else { continue };
        if guest_dir.len() <= GUEST_ROOT.len() {
            continue;
        }
        if dirs.iter().any(|d: &Entry| d.guest.as_slice() == guest_dir) {
            continue;
        }
        let host_no_nul = entry.host.get(..entry.host.len().saturating_sub(1)).unwrap_or(&[]);
        let Some(slash_h) = host_no_nul.iter().rposition(|&b| b == b'/') else { continue };
        let Some(host_dir) = host_no_nul.get(..slash_h) else { continue };
        if host_dir.is_empty() {
            continue;
        }
        let mut g = Vec::with_capacity_in(guest_dir.len(), alloc);
        g.extend_from_slice(guest_dir);
        let mut h = Vec::with_capacity_in(host_dir.len() + 1, alloc);
        h.extend_from_slice(host_dir);
        h.push(0);
        dirs.push(Entry { guest: g, host: h });
    }
    dirs
}

/// Parses `raw` (`<guest>\t<host>\n` lines), reserves the demand-paged async-signal-safe scratch
/// pool, and installs the process-wide immutable remap table.
/// Returns `Ok(true)` if at least one mapping was installed, `Ok(false)` if `raw` was empty.
pub fn install(raw: &[u8], alloc: Alloc<'static>) -> Result<bool, BinRemapError> {
    let entries = parse_entries(raw, alloc)?;
    if entries.is_empty() {
        return Ok(false);
    }
    let host_root = derive_host_root(entries.as_slice(), alloc);
    let dir_entries = derive_dir_entries(entries.as_slice(), host_root.as_slice(), alloc);
    let slots = alloc_scratch_slots()?;
    let table: ABox<'static, RemapTable> = Box::new_in(RemapTable { entries, dir_entries, host_root, slots }, alloc);
    let (ptr, alloc) = Box::into_raw_with_allocator(table);
    if MAP
        .compare_exchange(ptr::null_mut(), ptr, Ordering::Release, Ordering::Relaxed)
        .is_err()
    {
        // SAFETY: `ptr` was never published; unmap its scratch pool and reclaim it.
        unsafe {
            let total = SCRATCH_SLOTS * SCRATCH_SLOT_BYTES;
            let _ = libc::munmap((*ptr).slots[0].base.cast(), total);
            drop(Box::from_raw_in(ptr, alloc));
        }
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

/// Lock-free RAII lease of one pre-allocated scratch slot (strictly POSIX async-signal-safe:
/// uses only `AtomicBool::compare_exchange` / `store`, zero syscalls, zero allocator calls).
/// Used exclusively by async-signal-safe `exec*` wrappers (`execv*`, `execl*`).
struct SlotGuard<'a> {
    slot: Option<&'a ScratchSlot>,
}

impl<'a> SlotGuard<'a> {
    const EMPTY: SlotGuard<'a> = SlotGuard { slot: None };

    fn acquire(slots: &'a [ScratchSlot], need: usize) -> Result<SlotGuard<'a>, ()> {
        for slot in slots.iter() {
            if slot.len < need {
                continue;
            }
            if slot
                .in_use
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(SlotGuard { slot: Some(slot) });
            }
        }
        Err(())
    }

    fn base(&self) -> *mut u8 {
        self.slot.map_or(ptr::null_mut(), |s| s.base)
    }

    fn len(&self) -> usize {
        self.slot.map_or(0, |s| s.len)
    }
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            slot.in_use.store(false, Ordering::Release);
        }
    }
}

/// Heap-backed RAII scratch buffer (`libc::malloc`/`libc::free`) used by non-async-signal-safe
/// wrappers (`system`, `popen`, `posix_spawn`, `posix_spawnp`) so arbitrary numbers of concurrent
/// guest threads can block inside `system()`/`popen()` or invoke `posix_spawn*()` without any
/// fixed process-wide concurrency cap and without starving `exec*` scratch slots.
struct MallocBuf {
    ptr: *mut u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RemapErr {
    TooBig,
    NoMem,
}

impl RemapErr {
    fn errno(self) -> c_int {
        match self {
            RemapErr::TooBig => libc::E2BIG,
            RemapErr::NoMem => libc::ENOMEM,
        }
    }
}

impl MallocBuf {
    const EMPTY: MallocBuf = MallocBuf { ptr: ptr::null_mut() };

    fn alloc(bytes: usize) -> Result<MallocBuf, RemapErr> {
        let size = bytes.max(1);
        // SAFETY: `size > 0`; caller frees via `Drop`.
        let ptr = unsafe { libc::malloc(size).cast::<u8>() };
        if ptr.is_null() {
            return Err(RemapErr::NoMem);
        }
        Ok(MallocBuf { ptr })
    }

    fn base(&self) -> *mut u8 {
        self.ptr
    }
}

impl Drop for MallocBuf {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: `self.ptr` came from `libc::malloc` and is freed once here.
            unsafe { libc::free(self.ptr.cast()) };
        }
    }
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
        let prev_ok = hit == 0 || input.get(hit - 1).is_none_or(|&b| !is_path_cont(b));
        let mut matched: Option<&Entry> = None;
        if prev_ok {
            for entry in entries.iter() {
                let g = entry.guest.as_slice();
                if input.get(hit..hit + g.len()) == Some(g) {
                    let next_ok = input.get(hit + g.len()).is_none_or(|&b| !is_path_cont(b));
                    if next_ok && matched.is_none_or(|m| g.len() > m.guest.len()) {
                        matched = Some(entry);
                    }
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
        let prev_ok = hit == 0 || input.get(hit - 1).is_none_or(|&b| !is_path_cont(b));
        let mut matched: Option<&Entry> = None;
        if prev_ok {
            for entry in entries.iter() {
                let g = entry.guest.as_slice();
                if input.get(hit..hit + g.len()) == Some(g) {
                    let next_ok = input.get(hit + g.len()).is_none_or(|&b| !is_path_cont(b));
                    if next_ok && matched.is_none_or(|m| g.len() > m.guest.len()) {
                        matched = Some(entry);
                    }
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

/// Rewrites colon-separated `PATH` / `execvP` `search_path` entries replacing `/target/debug/bin`
/// (or `/target/debug/bin/...`) with `<host_root>` (or `<host_root>/...`) into `scratch`.
fn rewrite_search_path(
    input: &[u8],
    host_root: &[u8],
    scratch: &mut [u8],
) -> Result<Option<usize>, ()> {
    if host_root.is_empty() || find_subslice(input, GUEST_ROOT, 0).is_none() {
        return Ok(None);
    }
    let mut cursor = 0usize;
    let mut changed = false;
    let mut first = true;
    for comp in input.split(|&b| b == b':') {
        if !first {
            *scratch.get_mut(cursor).ok_or(())? = b':';
            cursor += 1;
        }
        first = false;
        let replaced = if comp == GUEST_ROOT {
            changed = true;
            let dst = scratch.get_mut(cursor..cursor.checked_add(host_root.len()).ok_or(())?).ok_or(())?;
            dst.copy_from_slice(host_root);
            cursor += host_root.len();
            true
        } else if let Some(rest) = comp.strip_prefix(GUEST_PREFIX) {
            changed = true;
            let dst = scratch.get_mut(cursor..cursor.checked_add(host_root.len()).ok_or(())?).ok_or(())?;
            dst.copy_from_slice(host_root);
            cursor += host_root.len();
            *scratch.get_mut(cursor).ok_or(())? = b'/';
            cursor += 1;
            let dst2 = scratch.get_mut(cursor..cursor.checked_add(rest.len()).ok_or(())?).ok_or(())?;
            dst2.copy_from_slice(rest);
            cursor += rest.len();
            true
        } else {
            false
        };
        if !replaced {
            let dst = scratch.get_mut(cursor..cursor.checked_add(comp.len()).ok_or(())?).ok_or(())?;
            dst.copy_from_slice(comp);
            cursor += comp.len();
        }
    }
    if !changed {
        return Ok(None);
    }
    *scratch.get_mut(cursor).ok_or(())? = 0;
    Ok(Some(cursor + 1))
}

/// Rewrites a NUL-terminated `char* const*` list (`argv` or `envp`) using a pre-allocated
/// async-signal-safe scratch slot from `slots` ONLY when at least one entry needs remapping.
unsafe fn remap_cstr_list_with<'a>(
    list: *const *const c_char,
    entries: &[Entry],
    slots: &'a [ScratchSlot],
    guard: &mut SlotGuard<'a>,
) -> Result<*const *const c_char, ()> {
    if list.is_null() {
        return Ok(list);
    }
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
    let ptr_bytes = ptr_slots.checked_mul(size_of::<*const c_char>()).ok_or(())?;
    let total = ptr_bytes.checked_add(need_bytes).ok_or(())?;
    *guard = SlotGuard::acquire(slots, total)?;
    // SAFETY: every slot base is page-aligned (>= `align_of::<*const c_char>()`), valid for
    // `total <= slot.len`, and exclusively leased by `guard`.
    let _ = align_of::<*const c_char>();
    let ptrs_slice = unsafe { slice::from_raw_parts_mut(guard.base().cast::<*const c_char>(), ptr_slots) };
    let b_ptr = unsafe { guard.base().add(ptr_bytes) };
    let scratch_slice = unsafe { slice::from_raw_parts_mut(b_ptr, need_bytes) };

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

unsafe fn remap_cstr_list<'a>(
    list: *const *const c_char,
    guard: &mut SlotGuard<'a>,
) -> Result<*const *const c_char, ()> {
    if list.is_null() {
        return Ok(list);
    }
    let map_ptr = MAP.load(Ordering::Acquire);
    if map_ptr.is_null() {
        return Ok(list);
    }
    // SAFETY: `map_ptr` was published once and never freed for the life of the process.
    let table: &'a RemapTable = unsafe { &*map_ptr };
    unsafe { remap_cstr_list_with(list, table.entries.as_slice(), &table.slots, guard) }
}

/// Rewrites a NUL-terminated `char* const*` list (`argv` or `envp`) using `MallocBuf` (for
/// non-async-signal-safe `posix_spawn`/`posix_spawnp`) ONLY when at least one entry needs remapping.
unsafe fn remap_cstr_list_malloc_with(
    list: *const *const c_char,
    entries: &[Entry],
    buf_guard: &mut MallocBuf,
) -> Result<*const *const c_char, RemapErr> {
    if list.is_null() {
        return Ok(list);
    }
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
        match measure_rewrite(bytes, entries).map_err(|()| RemapErr::TooBig)? {
            MeasureResult::Unchanged => {}
            MeasureResult::Exact(_) => any_changed = true,
            MeasureResult::NeedsBytes(n) => {
                any_changed = true;
                need_bytes = need_bytes.checked_add(n).ok_or(RemapErr::TooBig)?;
            }
        }
        count = count.checked_add(1).ok_or(RemapErr::TooBig)?;
    }
    if !any_changed {
        return Ok(list);
    }
    let ptr_slots = count.checked_add(1).ok_or(RemapErr::TooBig)?;
    let ptr_bytes = ptr_slots.checked_mul(size_of::<*const c_char>()).ok_or(RemapErr::TooBig)?;
    let total = ptr_bytes.checked_add(need_bytes).ok_or(RemapErr::TooBig)?;
    *buf_guard = MallocBuf::alloc(total)?;
    // SAFETY: `libc::malloc` returns memory aligned for any fundamental type (including pointers),
    // valid for `total` bytes, and exclusively owned by `buf_guard`.
    let _ = align_of::<*const c_char>();
    let ptrs_slice = unsafe { slice::from_raw_parts_mut(buf_guard.base().cast::<*const c_char>(), ptr_slots) };
    let b_ptr = unsafe { buf_guard.base().add(ptr_bytes) };
    let scratch_slice = unsafe { slice::from_raw_parts_mut(b_ptr, need_bytes) };

    let mut cursor = 0usize;
    for i in 0..count {
        // SAFETY: `i < count`, valid elements of `list`.
        let elem = unsafe { *list.add(i) };
        let bytes = unsafe { CStr::from_ptr(elem) }.to_bytes();
        let out_ptr = match rewrite_bytes(bytes, entries, scratch_slice, &mut cursor).map_err(|()| RemapErr::TooBig)? {
            RewriteResult::Unchanged => elem,
            RewriteResult::Exact(host) => host,
            RewriteResult::Rewritten { start, .. } => {
                // SAFETY: `start < scratch_slice.len()` and string is NUL-terminated.
                unsafe { scratch_slice.as_ptr().add(start).cast::<c_char>() }
            }
        };
        *ptrs_slice.get_mut(i).ok_or(RemapErr::TooBig)? = out_ptr;
    }
    *ptrs_slice.get_mut(count).ok_or(RemapErr::TooBig)? = ptr::null();
    Ok(ptrs_slice.as_ptr())
}

unsafe fn remap_cstr_list_malloc(
    list: *const *const c_char,
    buf_guard: &mut MallocBuf,
) -> Result<*const *const c_char, RemapErr> {
    if list.is_null() {
        return Ok(list);
    }
    let map_ptr = MAP.load(Ordering::Acquire);
    if map_ptr.is_null() {
        return Ok(list);
    }
    // SAFETY: `map_ptr` was published once and never freed for the life of the process.
    let table = unsafe { &*map_ptr };
    unsafe { remap_cstr_list_malloc_with(list, table.entries.as_slice(), buf_guard) }
}

/// Rewrites a single shell command C string (for `system` / `popen`) lazily into a heap-backed
/// `MallocBuf` only when embedded rewriting is needed, imposing no process-wide concurrency cap.
unsafe fn remap_cmd_cstr_malloc_with(
    cmd: *const c_char,
    entries: &[Entry],
    buf_guard: &mut MallocBuf,
) -> Result<*const c_char, RemapErr> {
    if cmd.is_null() {
        return Ok(cmd);
    }
    let bytes = unsafe { CStr::from_ptr(cmd) }.to_bytes();
    match measure_rewrite(bytes, entries).map_err(|()| RemapErr::TooBig)? {
        MeasureResult::Unchanged => Ok(cmd),
        MeasureResult::Exact(host) => Ok(host),
        MeasureResult::NeedsBytes(need) => {
            *buf_guard = MallocBuf::alloc(need)?;
            // SAFETY: `buf_guard` owns `need` writable bytes.
            let buf = unsafe { slice::from_raw_parts_mut(buf_guard.base(), need) };
            let mut cursor = 0usize;
            match rewrite_bytes(bytes, entries, buf, &mut cursor).map_err(|()| RemapErr::TooBig)? {
                RewriteResult::Unchanged => Ok(cmd),
                RewriteResult::Exact(host) => Ok(host),
                RewriteResult::Rewritten { start, .. } => {
                    Ok(unsafe { buf.as_ptr().add(start).cast::<c_char>() })
                }
            }
        }
    }
}

unsafe fn remap_cmd_cstr_malloc(
    cmd: *const c_char,
    buf_guard: &mut MallocBuf,
) -> Result<*const c_char, RemapErr> {
    if cmd.is_null() {
        return Ok(cmd);
    }
    let map_ptr = MAP.load(Ordering::Acquire);
    if map_ptr.is_null() {
        return Ok(cmd);
    }
    // SAFETY: `map_ptr` was published once and never freed for the life of the process.
    let table = unsafe { &*map_ptr };
    unsafe { remap_cmd_cstr_malloc_with(cmd, table.entries.as_slice(), buf_guard) }
}

#[cfg(target_os = "macos")]
unsafe fn remap_search_path_cstr<'a>(
    search_path: *const c_char,
    guard: &mut SlotGuard<'a>,
) -> Result<*const c_char, ()> {
    if search_path.is_null() {
        return Ok(search_path);
    }
    let map_ptr = MAP.load(Ordering::Acquire);
    if map_ptr.is_null() {
        return Ok(search_path);
    }
    // SAFETY: `map_ptr` was published once and never freed for the life of the process.
    let table: &'a RemapTable = unsafe { &*map_ptr };
    if table.host_root.is_empty() {
        return Ok(search_path);
    }
    let bytes = unsafe { CStr::from_ptr(search_path) }.to_bytes();
    if find_subslice(bytes, GUEST_ROOT, 0).is_none() {
        return Ok(search_path);
    }
    *guard = SlotGuard::acquire(&table.slots, SCRATCH_SLOT_BYTES)?;
    // SAFETY: `guard` exclusively leases `guard.len()` writable bytes.
    let buf = unsafe { slice::from_raw_parts_mut(guard.base(), guard.len()) };
    match rewrite_search_path(bytes, table.host_root.as_slice(), buf)? {
        None => Ok(search_path),
        Some(_) => Ok(buf.as_ptr().cast::<c_char>()),
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

/// Maps `path` to its host launcher stub (or stub parent directory `/target/debug/bin[/<pkg>]`)
/// C string when it matches an installed guest path; otherwise returns `path` unchanged.
/// Lock-free, zero-allocation, and async-signal-safe.
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
    let table = unsafe { &*ptr };
    // SAFETY: caller passes a valid NUL-terminated C string pointer.
    let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
    if !bytes.starts_with(GUEST_ROOT) {
        return path;
    }
    if bytes.starts_with(GUEST_PREFIX) {
        for entry in table.entries.as_slice().iter() {
            if entry.guest.as_slice() == bytes {
                return entry.host.as_ptr().cast::<c_char>();
            }
        }
    }
    let mut trimmed = bytes;
    while trimmed.len() > 1 && trimmed.last().copied() == Some(b'/') {
        if let Some(shorter) = trimmed.get(..trimmed.len() - 1) {
            trimmed = shorter;
        } else {
            break;
        }
    }
    for dir in table.dir_entries.as_slice().iter() {
        if dir.guest.as_slice() == trimmed {
            return dir.host.as_ptr().cast::<c_char>();
        }
    }
    path
}

pub(crate) extern "C" fn remap_execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    let mut argv_slot = SlotGuard::EMPTY;
    let mut envp_slot = SlotGuard::EMPTY;
    let Ok(new_argv) = (unsafe { remap_cstr_list(argv, &mut argv_slot) }) else {
        set_errno(libc::E2BIG);
        return -1;
    };
    let Ok(new_envp) = (unsafe { remap_cstr_list(envp, &mut envp_slot) }) else {
        set_errno(libc::E2BIG);
        return -1;
    };
    // SAFETY: `remap_cstr`, `new_argv`, and `new_envp` stay live across `execve`.
    unsafe { libc::execve(remap_cstr(path), new_argv, new_envp) }
}

pub(crate) extern "C" fn remap_execv(path: *const c_char, argv: *const *const c_char) -> c_int {
    let mut argv_slot = SlotGuard::EMPTY;
    let Ok(new_argv) = (unsafe { remap_cstr_list(argv, &mut argv_slot) }) else {
        set_errno(libc::E2BIG);
        return -1;
    };
    // SAFETY: `remap_cstr` and `new_argv` stay live across `execv`.
    unsafe { libc::execv(remap_cstr(path), new_argv) }
}

pub(crate) extern "C" fn remap_execvp(file: *const c_char, argv: *const *const c_char) -> c_int {
    let mut argv_slot = SlotGuard::EMPTY;
    let Ok(new_argv) = (unsafe { remap_cstr_list(argv, &mut argv_slot) }) else {
        set_errno(libc::E2BIG);
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
    let mut argv_slot = SlotGuard::EMPTY;
    let mut envp_slot = SlotGuard::EMPTY;
    let Ok(new_argv) = (unsafe { remap_cstr_list(argv, &mut argv_slot) }) else {
        set_errno(libc::E2BIG);
        return -1;
    };
    let Ok(new_envp) = (unsafe { remap_cstr_list(envp, &mut envp_slot) }) else {
        set_errno(libc::E2BIG);
        return -1;
    };
    // SAFETY: `remap_cstr`, `new_argv`, and `new_envp` stay live across `execvpe`.
    unsafe { libc::execvpe(remap_cstr(file), new_argv, new_envp) }
}

#[cfg(target_os = "macos")]
#[expect(non_snake_case, reason = "matches Darwin libc symbol name `execvP`")]
pub(crate) extern "C" fn remap_execvP(
    file: *const c_char,
    search_path: *const c_char,
    argv: *const *const c_char,
) -> c_int {
    let mut path_slot = SlotGuard::EMPTY;
    let mut argv_slot = SlotGuard::EMPTY;
    let Ok(new_path) = (unsafe { remap_search_path_cstr(search_path, &mut path_slot) }) else {
        set_errno(libc::E2BIG);
        return -1;
    };
    let Ok(new_argv) = (unsafe { remap_cstr_list(argv, &mut argv_slot) }) else {
        set_errno(libc::E2BIG);
        return -1;
    };
    // SAFETY: `remap_cstr`, `new_path`, and `new_argv` stay live across Darwin `execvP`.
    unsafe { libc::execvP(remap_cstr(file), new_path, new_argv.cast()) }
}

/// Collects variadic `execl`/`execlp`/`execle` arguments into a tiny 32-pointer stack buffer
/// (spilling into a pre-allocated async-signal-safe scratch slot when > 31 args), then invokes `f`.
unsafe fn collect_execl_args<F>(
    arg0: *const c_char,
    mut args: core::ffi::VaList<'_>,
    has_envp: bool,
    f: F,
) -> c_int
where
    F: FnOnce(*const *const c_char, *const *const c_char) -> c_int,
{
    let mut stack = [ptr::null::<c_char>(); EXECL_STACK_PTRS];
    let mut spill = SlotGuard::EMPTY;
    let mut spill_cap = 0usize;
    let mut count = 0usize;
    let mut cur = arg0;
    loop {
        if spill.base().is_null() {
            if count < EXECL_STACK_PTRS - 1 {
                if let Some(slot) = stack.get_mut(count) {
                    *slot = cur;
                }
            } else {
                let map_ptr = MAP.load(Ordering::Acquire);
                if map_ptr.is_null() {
                    set_errno(libc::E2BIG);
                    return -1;
                }
                // SAFETY: `map_ptr` lives for the process.
                let table = unsafe { &*map_ptr };
                let Ok(leased) = SlotGuard::acquire(&table.slots, SCRATCH_SLOT_BYTES) else {
                    set_errno(libc::E2BIG);
                    return -1;
                };
                spill_cap = leased.len() / size_of::<*const c_char>();
                if count >= spill_cap {
                    set_errno(libc::E2BIG);
                    return -1;
                }
                // SAFETY: `leased.base()` is page-aligned and holds `spill_cap` pointers.
                unsafe {
                    ptr::copy_nonoverlapping(stack.as_ptr(), leased.base().cast::<*const c_char>(), count);
                    *leased.base().cast::<*const c_char>().add(count) = cur;
                }
                spill = leased;
            }
        } else {
            if count >= spill_cap {
                set_errno(libc::E2BIG);
                return -1;
            }
            // SAFETY: `count < spill_cap`.
            unsafe {
                *spill.base().cast::<*const c_char>().add(count) = cur;
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
    let argv = if spill.base().is_null() {
        stack.as_ptr()
    } else {
        spill.base().cast::<*const c_char>()
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
    let mut argv_buf = MallocBuf::EMPTY;
    let mut envp_buf = MallocBuf::EMPTY;
    let new_argv = match unsafe { remap_cstr_list_malloc(argv.cast(), &mut argv_buf) } {
        Ok(v) => v,
        Err(err) => return err.errno(),
    };
    let new_envp = match unsafe { remap_cstr_list_malloc(envp.cast(), &mut envp_buf) } {
        Ok(v) => v,
        Err(err) => return err.errno(),
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
    let mut argv_buf = MallocBuf::EMPTY;
    let mut envp_buf = MallocBuf::EMPTY;
    let new_argv = match unsafe { remap_cstr_list_malloc(argv.cast(), &mut argv_buf) } {
        Ok(v) => v,
        Err(err) => return err.errno(),
    };
    let new_envp = match unsafe { remap_cstr_list_malloc(envp.cast(), &mut envp_buf) } {
        Ok(v) => v,
        Err(err) => return err.errno(),
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

pub(crate) extern "C" fn remap_posix_spawn_file_actions_addopen(
    file_actions: *mut libc::posix_spawn_file_actions_t,
    fildes: c_int,
    path: *const c_char,
    oflag: c_int,
    mode: libc::mode_t,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe {
        libc::posix_spawn_file_actions_addopen(file_actions, fildes, remap_cstr(path), oflag, mode)
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn posix_spawn_file_actions_addchdir_np(
        file_actions: *mut libc::posix_spawn_file_actions_t,
        path: *const c_char,
    ) -> c_int;
}

#[cfg(target_os = "macos")]
pub(crate) extern "C" fn remap_posix_spawn_file_actions_addchdir_np(
    file_actions: *mut libc::posix_spawn_file_actions_t,
    path: *const c_char,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated directory C string.
    unsafe { posix_spawn_file_actions_addchdir_np(file_actions, remap_cstr(path)) }
}

pub(crate) extern "C" fn remap_chdir(path: *const c_char) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated directory C string.
    unsafe { libc::chdir(remap_cstr(path)) }
}

pub(crate) extern "C" fn remap_opendir(path: *const c_char) -> *mut libc::DIR {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated directory C string.
    unsafe { libc::opendir(remap_cstr(path)) }
}

pub(crate) extern "C" fn remap_system(command: *const c_char) -> c_int {
    let mut buf = MallocBuf::EMPTY;
    let cmd = match unsafe { remap_cmd_cstr_malloc(command, &mut buf) } {
        Ok(c) => c,
        Err(err) => {
            set_errno(err.errno());
            return -1;
        }
    };
    // SAFETY: `cmd` is null or a valid NUL-terminated string live across `libc::system`.
    unsafe { libc::system(cmd) }
}

pub(crate) extern "C" fn remap_popen(
    command: *const c_char,
    mode: *const c_char,
) -> *mut libc::FILE {
    let mut buf = MallocBuf::EMPTY;
    let cmd = match unsafe { remap_cmd_cstr_malloc(command, &mut buf) } {
        Ok(c) => c,
        Err(err) => {
            set_errno(err.errno());
            return ptr::null_mut();
        }
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
        BinRemapError, MallocBuf, RewriteResult, SCRATCH_SLOTS, SCRATCH_SLOT_BYTES, SlotGuard,
        alloc_scratch_slots, derive_dir_entries, derive_host_root, parse_entries,
        remap_cmd_cstr_malloc_with, remap_cstr_list_with, rewrite_bytes, rewrite_search_path,
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
    fn rewrite_bytes_and_search_path_handle_all_cases() {
        let alloc = blaze_own::testing::heap().alloc();
        let entries = parse_entries(
            b"/target/debug/bin/app/cli\t/tmp/bzb-0123-r0-0/app/cli\n/target/debug/bin/app/cli-v2\t/tmp/bzb-0123-r0-0/app/cli-v2\n",
            alloc,
        )
        .expect("entries");
        let host_root = derive_host_root(entries.as_slice(), alloc);
        assert_eq!(host_root.as_slice(), b"/tmp/bzb-0123-r0-0");

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
        assert_eq!(unsafe { CStr::from_ptr(ptr) }.to_bytes(), b"/tmp/bzb-0123-r0-0/app/cli");

        let Ok(RewriteResult::Rewritten { start, end }) = rewrite_bytes(
            b"\"/target/debug/bin/app/cli\" && /target/debug/bin/app/cli-v2 --ok",
            entries.as_slice(),
            &mut scratch,
            &mut cursor,
        ) else {
            panic!("expected rewritten shell command");
        };
        assert_eq!(
            &scratch[start..end - 1],
            b"\"/tmp/bzb-0123-r0-0/app/cli\" && /tmp/bzb-0123-r0-0/app/cli-v2 --ok"
        );

        assert!(matches!(
            rewrite_bytes(b"/target/debug/bin/app/cli2", entries.as_slice(), &mut scratch, &mut cursor),
            Ok(RewriteResult::Unchanged)
        ));
        // Leading boundary must be enforced: path suffixes must NOT be rewritten.
        assert!(matches!(
            rewrite_bytes(b"prefix/target/debug/bin/app/cli", entries.as_slice(), &mut scratch, &mut cursor),
            Ok(RewriteResult::Unchanged)
        ));
        assert!(matches!(
            rewrite_bytes(b"./target/debug/bin/app/cli", entries.as_slice(), &mut scratch, &mut cursor),
            Ok(RewriteResult::Unchanged)
        ));
        // Valid leading delimiters (`=`, space) still rewrite only the delimited occurrence.
        let Ok(RewriteResult::Rewritten { start, end }) = rewrite_bytes(
            b"prefix/target/debug/bin/app/cli BIN=/target/debug/bin/app/cli",
            entries.as_slice(),
            &mut scratch,
            &mut cursor,
        ) else {
            panic!("expected delimited occurrence to rewrite");
        };
        assert_eq!(
            &scratch[start..end - 1],
            b"prefix/target/debug/bin/app/cli BIN=/tmp/bzb-0123-r0-0/app/cli"
        );

        let dirs = derive_dir_entries(entries.as_slice(), host_root.as_slice(), alloc);
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].guest.as_slice(), b"/target/debug/bin");
        assert_eq!(dirs[0].host.as_slice(), b"/tmp/bzb-0123-r0-0\0");
        assert_eq!(dirs[1].guest.as_slice(), b"/target/debug/bin/app");
        assert_eq!(dirs[1].host.as_slice(), b"/tmp/bzb-0123-r0-0/app\0");

        let mut sp_buf = [0u8; 256];
        let sp_len = rewrite_search_path(
            b"/usr/bin:/target/debug/bin/app:/target/debug/bin:/bin",
            host_root.as_slice(),
            &mut sp_buf,
        )
        .expect("rewrite search_path")
        .expect("changed");
        assert_eq!(
            &sp_buf[..sp_len - 1],
            b"/usr/bin:/tmp/bzb-0123-r0-0/app:/tmp/bzb-0123-r0-0:/bin"
        );
    }

    #[test]
    fn remap_cstr_list_uses_preallocated_async_signal_safe_pool_for_large_vectors() {
        let alloc = blaze_own::testing::heap().alloc();
        let entries = parse_entries(
            b"/target/debug/bin/app/cli\t/tmp/bzb-0123-r0-0/app/cli\n",
            alloc,
        )
        .expect("entries");
        let slots = alloc_scratch_slots().expect("pre-allocated scratch pool");

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

        {
            let mut guard = SlotGuard::EMPTY;
            let out = unsafe { remap_cstr_list_with(raw.as_ptr(), entries.as_slice(), &slots, &mut guard) }
                .expect("400-element argv must succeed from pre-allocated pool");
            assert!(!guard.base().is_null());
            assert_eq!(unsafe { CStr::from_ptr(*out.add(0)) }.to_bytes(), b"arg");
            assert_eq!(unsafe { CStr::from_ptr(*out.add(300)) }.to_bytes(), b"/tmp/bzb-0123-r0-0/app/cli");
            assert_eq!(
                unsafe { CStr::from_ptr(*out.add(350)) }.to_bytes(),
                b"RUN=/tmp/bzb-0123-r0-0/app/cli --flag"
            );
            assert_eq!(unsafe { CStr::from_ptr(*out.add(399)) }.to_bytes(), b"arg");
            assert!(unsafe { *out.add(400) }.is_null());
        }
        // Dropping `guard` returns the slot so all slots are free again.
        assert!(!slots[0].in_use.load(core::sync::atomic::Ordering::Acquire));

        let unchanged_raw = [plain, plain, ptr::null()];
        let mut unused_guard = SlotGuard::EMPTY;
        let same = unsafe { remap_cstr_list_with(unchanged_raw.as_ptr(), entries.as_slice(), &slots, &mut unused_guard) }
            .expect("unchanged vector succeeds");
        assert_eq!(same, unchanged_raw.as_ptr());
        assert!(unused_guard.base().is_null());

        // Clean up the test's temporary pool mapping.
        unsafe {
            let _ = libc::munmap(slots[0].base.cast(), SCRATCH_SLOTS * SCRATCH_SLOT_BYTES);
        }
    }

    #[test]
    fn remap_cmd_cstr_malloc_has_no_concurrency_cap() {
        let threads = SCRATCH_SLOTS + 8;
        let barrier = std::sync::Barrier::new(threads);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let alloc = blaze_own::testing::heap().alloc();
                    let entries = parse_entries(
                        b"/target/debug/bin/app/cli\t/tmp/bzb-0123-r0-0/app/cli\n",
                        alloc,
                    )
                    .expect("entries");
                    let mut buf = MallocBuf::EMPTY;
                    let cmd = c"\"/target/debug/bin/app/cli\" --concurrent";
                    let out = unsafe { remap_cmd_cstr_malloc_with(cmd.as_ptr(), entries.as_slice(), &mut buf) }
                        .expect("concurrent remapped system() command must never hit a process-wide slot cap");
                    assert!(!buf.base().is_null());
                    // Hold all `SCRATCH_SLOTS + 8` buffers alive simultaneously across the barrier.
                    barrier.wait();
                    assert_eq!(
                        unsafe { CStr::from_ptr(out) }.to_bytes(),
                        b"\"/tmp/bzb-0123-r0-0/app/cli\" --concurrent"
                    );
                });
            }
        });
    }
}
