//! Windows on x86-64. A memory object is a section backed by the paging file, mapped read-write
//! in the daemon. The run process holds no handle to it: the daemon maps each view into the run
//! process itself (`MapViewOfFile3` on its process handle) when the start hook asks, so what a
//! run process can map, and how, is what the daemon allows. Views are placed on the 64 KiB
//! allocation granularity, which is [`page`]. The channel is a named pipe ([`pipe`]).
//!
//! The image is one view of an image section, which can't be partly replaced, so the daemon
//! rebuilds it in the run process when the hook asks (OVERLAY, [`Remote::overlay`]) while the
//! hook's thread waits on the pipe: it reads every page the image has outside the overlays,
//! unmaps the image, reserves its span as one placeholder, splits it at the overlays, maps each
//! overlay's view over its placeholder (`MEM_REPLACE_PLACEHOLDER`), and puts back the pages it
//! read as private memory with their protection. Every byte at every address is as before.

mod pipe;
#[cfg(test)]
mod tests;

use std::ffi::c_void;
use std::ops::Range;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64};

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_NO_MORE_FILES, ERROR_TIMEOUT, GetLastError, HANDLE, HMODULE,
    INVALID_HANDLE_VALUE, SetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Cryptography::ProcessPrng;
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, FlushInstructionCache, ReadProcessMemory, RtlCaptureContext, RtlLookupFunctionEntry, RtlVirtualUnwind,
    UNW_FLAG_NHANDLER, WriteProcessMemory,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_COMMIT, MEM_IMAGE, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER,
    MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_NOACCESS, PAGE_READONLY,
    PAGE_READWRITE, PAGE_WRITECOPY, SEC_COMMIT, UnmapViewOfFile, UnmapViewOfFile2, VirtualAlloc2, VirtualFreeEx,
    VirtualProtectEx, VirtualQueryEx,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentThreadId, INFINITE, OpenProcess, PROCESS_DUP_HANDLE,
    PROCESS_QUERY_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE, SetEvent,
    TerminateProcess, WaitForMultipleObjects, WaitForSingleObject, WaitOnAddress, WakeByAddressAll,
};

pub(crate) use pipe::{Listener, Stream, connect};

use super::Rights;
use crate::channel::Channel;
use crate::frame::{FrameError, Reader, Writer, kinds};
use crate::handshake::{Grant, Overlay};
use crate::{Access, Error};

/// The sequence number of the frame after the GRANT that lists the objects granted: their
/// lengths and rights. Nothing else crosses: the daemon keeps the handles.
const OBJECTS_SEQ: u32 = 1;

/// What failed, with the error Windows gave. Call it right after the failing call.
fn os(what: impl std::fmt::Display) -> Error {
    Error::new(format!("Windows: {what}: {}", std::io::Error::last_os_error()))
}

pub(crate) fn page() -> u64 {
    static PAGE: OnceLock<u64> = OnceLock::new();
    *PAGE.get_or_init(|| {
        let mut info = SYSTEM_INFO::default();
        // SAFETY: GetSystemInfo only writes the struct it is given, which outlives the call.
        unsafe { GetSystemInfo(&mut info) };
        u64::from(info.dwAllocationGranularity)
    })
}

/// x86-64 keeps instruction fetch coherent with stores from any processor: a thread that
/// serializes after a store runs what was stored.
pub(crate) fn check_icache() -> Result<(), Error> {
    Ok(())
}

pub(crate) fn random(out: &mut [u8; 16]) -> Result<(), Error> {
    // SAFETY: ProcessPrng writes exactly `out.len()` bytes into `out`.
    if unsafe { ProcessPrng(out.as_mut_ptr(), out.len()) } == 0 {
        return Err(os("ProcessPrng"));
    }
    Ok(())
}

fn protection(access: Access) -> u32 {
    match access {
        Access::Execute => PAGE_EXECUTE_READ,
        Access::Read => PAGE_READONLY,
        Access::Copy => PAGE_WRITECOPY,
        Access::Shared => PAGE_READWRITE,
    }
}

fn rights_tag(rights: Rights) -> u8 {
    match rights {
        Rights::Code => 1,
        Rights::Words => 2,
    }
}

/// Whether a run process may map an object of `rights` with `access`: code never shared
/// writable, words never executable.
fn allowed(rights: Rights, access: Access) -> bool {
    match rights {
        Rights::Code => !matches!(access, Access::Shared),
        Rights::Words => matches!(access, Access::Shared | Access::Read),
    }
}

/// Views start, end and are placed on whole granules.
fn granular(offset: u64, len: u64, at: u64) -> Result<(), Error> {
    let page = page();
    if len == 0 || !offset.is_multiple_of(page) || !len.is_multiple_of(page) || !at.is_multiple_of(page) {
        return Err(Error::new(format!(
            "a view of {len:#x} bytes from offset {offset:#x} at {at:#x}: Windows maps views of whole \
             {page:#x}-byte granules"
        )));
    }
    Ok(())
}

/// Maps `len` bytes of `section` from `offset` into `process` at `at` (0: anywhere), as `kind`
/// says (0 into free address space, or `MEM_REPLACE_PLACEHOLDER`).
fn map_view(
    section: HANDLE,
    process: HANDLE,
    offset: u64,
    len: u64,
    at: u64,
    kind: u32,
    access: Access,
) -> Result<u64, Error> {
    let size = usize::try_from(len).map_err(|_| Error::new(format!("a view of {len:#x} bytes")))?;
    // SAFETY: `section` and `process` are handles the caller holds. The view goes where the
    // system finds room (`at` 0), or exactly over a placeholder of the caller's own (failing
    // over anything else): it never lands on memory the process uses.
    let view = unsafe {
        MapViewOfFile3(
            section,
            process,
            at as *const c_void,
            offset,
            size,
            kind,
            protection(access),
            std::ptr::null_mut(),
            0,
        )
    };
    if view.Value.is_null() {
        return Err(os(format_args!("MapViewOfFile3 of {len:#x} bytes from offset {offset:#x} at {at:#x} as {access:?}")));
    }
    Ok(view.Value as u64)
}

/// A pagefile-backed section, zero when made. Its pages are committed with it (`SEC_COMMIT`:
/// charged against the commit limit, given memory only once touched), so every view of it is
/// readable everywhere with no commit step in between.
pub(crate) struct Object {
    section: OwnedHandle,
    len: u64,
}

impl Object {
    pub(crate) fn create(name: &str, len: u64) -> Result<Object, Error> {
        // SAFETY: a section backed by the paging file (`INVALID_HANDLE_VALUE`), unnamed, with the
        // default security; every argument is a value or null.
        let section = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_EXECUTE_READWRITE | SEC_COMMIT,
                (len >> 32) as u32,
                len as u32,
                std::ptr::null(),
            )
        };
        if section.is_null() {
            return Err(os(format_args!("CreateFileMappingW of {len:#x} bytes for {name}")));
        }
        // SAFETY: the handle was just made and nothing else owns it.
        Ok(Object { section: unsafe { OwnedHandle::from_raw_handle(section) }, len })
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn duplicate(&self) -> Result<Object, Error> {
        let section =
            self.section.try_clone().map_err(|it| Error::new(format!("Windows: DuplicateHandle of a section: {it}")))?;
        Ok(Object { section, len: self.len })
    }

    pub(crate) fn map_here(&self, access: Access) -> Result<View, Error> {
        let len = usize::try_from(self.len).map_err(|_| Error::new(format!("a view of {:#x} bytes", self.len)))?;
        // SAFETY: GetCurrentProcess returns a pseudo-handle, valid always.
        let at = map_view(self.section.as_raw_handle(), unsafe { GetCurrentProcess() }, 0, self.len, 0, 0, access)?;
        Ok(View { at, len, writable: matches!(access, Access::Shared | Access::Copy) })
    }
}

/// A view in this process, unmapped when dropped.
pub(crate) struct View {
    at: u64,
    len: usize,
    writable: bool,
}

impl View {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn address(&self) -> u64 {
        self.at
    }

    fn check(&self, range: &Range<usize>) -> Result<(), Error> {
        if range.start > range.end || range.end > self.len {
            return Err(Error::new(format!("bytes {:#x}..{:#x} of a view of {:#x}", range.start, range.end, self.len)));
        }
        Ok(())
    }

    pub(crate) fn bytes(&self, range: Range<usize>) -> Result<&[u8], Error> {
        self.check(&range)?;
        // SAFETY: the range is inside the view (checked), which stays mapped and readable while
        // `self` lives. Nothing writes the bytes a caller reads while it reads them: the daemon
        // writes a range of code before any process can reach it (`CodeMemory::write`), and a
        // run process maps code objects only to read, execute or copy (`allowed`).
        Ok(unsafe { std::slice::from_raw_parts((self.at as usize + range.start) as *const u8, range.len()) })
    }

    pub(crate) fn bytes_mut(&mut self, range: Range<usize>) -> Result<&mut [u8], Error> {
        if !self.writable {
            return Err(Error::new("writing a view mapped read-only"));
        }
        self.check(&range)?;
        // SAFETY: as in `bytes`, and the view is writable (checked); `&mut self` keeps every
        // other slice of this view from living meanwhile.
        Ok(unsafe { std::slice::from_raw_parts_mut((self.at as usize + range.start) as *mut u8, range.len()) })
    }

    pub(crate) fn word(&self, at: usize) -> &AtomicU64 {
        assert!(
            at.is_multiple_of(8) && at.checked_add(8).is_some_and(|end| end <= self.len),
            "a word at {at:#x} of a view of {:#x}",
            self.len
        );
        // SAFETY: in bounds and 8-aligned (asserted; the view starts on a granule), in a mapping
        // that lives as long as `self`. Every access to these words, here and in other
        // processes, is atomic.
        unsafe { &*((self.at as usize + at) as *const AtomicU64) }
    }

    pub(crate) fn word32(&self, at: usize) -> &AtomicU32 {
        assert!(
            at.is_multiple_of(4) && at.checked_add(4).is_some_and(|end| end <= self.len),
            "a word at {at:#x} of a view of {:#x}",
            self.len
        );
        // SAFETY: as in `word`, for 4 bytes.
        unsafe { &*((self.at as usize + at) as *const AtomicU32) }
    }

    pub(crate) fn publish(&self, range: Range<usize>) {
        if self.check(&range).is_err() || range.is_empty() {
            return;
        }
        // SAFETY: a range of this view; FlushInstructionCache reads nothing of it.
        unsafe { FlushInstructionCache(GetCurrentProcess(), (self.at as usize + range.start) as *const c_void, range.len()) };
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: `at` is the start of a view this `View` alone stands for; nothing refers into
        // it once it drops (every slice borrows `self`).
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.at as *mut c_void }) };
    }
}

/// Private memory of this process: code, then data right after it, in one allocation.
pub(crate) struct Exec {
    at: *mut u8,
    code_pages: usize,
    data: usize,
}

// SAFETY: the code is never written once `Exec::new` returns, and the data only through
// `&mut self`; nothing of the allocation is tied to a thread.
unsafe impl Send for Exec {}
// SAFETY: as above: `&self` reads only addresses.
unsafe impl Sync for Exec {}

impl Drop for Exec {
    fn drop(&mut self) {
        // SAFETY: `at` is the base of this `Exec`'s own allocation; every slice of it borrowed
        // `self` or ended with `Exec::new`'s `write`.
        unsafe { VirtualFreeEx(GetCurrentProcess(), self.at.cast::<c_void>(), 0, MEM_RELEASE) };
    }
}

impl Exec {
    pub(crate) fn new(code: usize, data: usize, write: impl FnOnce(&mut [u8], &mut [u8])) -> Result<Exec, Error> {
        let page = page() as usize;
        let code_pages = code.max(1).next_multiple_of(page);
        let len = code_pages + data.next_multiple_of(page);
        // SAFETY: reserves and commits fresh private memory wherever the system finds room:
        // nothing of this process is replaced.
        let at = unsafe {
            VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                len,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        };
        if at.is_null() {
            return Err(os(format_args!("VirtualAlloc2 of {len:#x} bytes of code and data")));
        }
        let exec = Exec { at: at.cast::<u8>(), code_pages, data };
        // SAFETY: the fresh allocation of `len` readable and writable bytes, which nothing else
        // refers to; `code <= code_pages` and `code_pages + data <= len`, so the slices are
        // disjoint and in it. `write` can't keep them past its return.
        let (code_bytes, data_bytes) = unsafe {
            (std::slice::from_raw_parts_mut(exec.at, code), std::slice::from_raw_parts_mut(exec.at.add(code_pages), data))
        };
        write(code_bytes, data_bytes);
        let mut old = 0;
        // SAFETY: the code's pages of the allocation above, which nothing writes from here on.
        if unsafe { VirtualProtectEx(GetCurrentProcess(), exec.at as *const c_void, code_pages, PAGE_EXECUTE_READ, &mut old) } == 0
        {
            return Err(os(format_args!("VirtualProtectEx of {code_pages:#x} bytes of code to execute and read")));
        }
        // SAFETY: a range of the allocation; FlushInstructionCache reads nothing of it.
        unsafe { FlushInstructionCache(GetCurrentProcess(), exec.at as *const c_void, code) };
        Ok(exec)
    }

    pub(crate) fn code_address(&self) -> u64 {
        self.at as u64
    }

    pub(crate) fn data_address(&self) -> u64 {
        self.at as u64 + self.code_pages as u64
    }

    pub(crate) fn data(&mut self) -> &mut [u8] {
        // SAFETY: the data's `data` bytes after the code's pages, committed writable, live as
        // long as `self`; `&mut self` makes this the only borrow.
        unsafe { std::slice::from_raw_parts_mut(self.at.add(self.code_pages), self.data) }
    }
}

/// What the daemon keeps to act in a run process: the process, and the objects it was granted
/// with their rights.
pub(crate) struct Remote {
    process: OwnedHandle,
    objects: Vec<(Object, Rights)>,
}

pub(crate) fn send_objects(
    channel: &mut Channel,
    pid: u32,
    grant: &[u8],
    objects: &[(&Object, Rights)],
) -> Result<Remote, Error> {
    let access = PROCESS_VM_OPERATION
        | PROCESS_VM_READ
        | PROCESS_VM_WRITE
        | PROCESS_QUERY_INFORMATION
        | PROCESS_TERMINATE
        | PROCESS_DUP_HANDLE
        | PROCESS_SYNCHRONIZE;
    // SAFETY: value-only arguments; the handle it returns is checked and owned below.
    let process = unsafe { OpenProcess(access, 0, pid) };
    if process.is_null() {
        return Err(os(format_args!("OpenProcess of the run process {pid}")));
    }
    // SAFETY: just opened, owned by nothing else.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut kept = Vec::with_capacity(objects.len());
    for (object, rights) in objects {
        kept.push((object.duplicate()?, *rights));
    }
    channel.send(kinds::GRANT, 0, grant)?;
    let mut body = Vec::with_capacity(4 + 9 * objects.len());
    let mut out = Writer::new(&mut body);
    out.u32(objects.len() as u32);
    for (object, rights) in objects {
        out.u64(object.len()).u8(rights_tag(*rights));
    }
    channel.send(kinds::GRANT, OBJECTS_SEQ, &body)?;
    Ok(Remote { process, objects: kept })
}

/// A page range of the run process's image as it was before the overlays, outside them: its
/// bytes and protection, put back as private memory.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Kept {
    at: u64,
    protect: u32,
    bytes: Vec<u8>,
}

/// One placeholder of the rebuilt span, in address order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Piece {
    /// A view of overlay `index` of the grant.
    View { at: u64, len: u64, index: usize },
    /// Private address space, reserved; the kept pages in it are committed.
    Private { at: u64, len: u64 },
}

/// The pieces `span` splits into at `overlays` (process addresses, by address, none
/// overlapping, all inside the span, all on granules).
fn pieces(span: Range<u64>, overlays: &[Range<u64>]) -> Result<Vec<Piece>, Error> {
    let mut out = Vec::with_capacity(2 * overlays.len() + 1);
    let mut at = span.start;
    for (index, overlay) in overlays.iter().enumerate() {
        if overlay.start < at || overlay.end > span.end || overlay.start >= overlay.end {
            return Err(Error::new(format!(
                "an overlay at {:#x}..{:#x}: out of order or outside the image {:#x}..{:#x}",
                overlay.start, overlay.end, span.start, span.end
            )));
        }
        if overlay.start > at {
            out.push(Piece::Private { at, len: overlay.start - at });
        }
        out.push(Piece::View { at: overlay.start, len: overlay.end - overlay.start, index });
        at = overlay.end;
    }
    if at < span.end {
        out.push(Piece::Private { at, len: span.end - at });
    }
    Ok(out)
}

/// The parts of `range` no overlay covers.
fn outside(range: Range<u64>, overlays: &[Range<u64>]) -> Vec<Range<u64>> {
    let mut out = Vec::new();
    let mut at = range.start;
    for overlay in overlays {
        if overlay.end <= at || overlay.start >= range.end {
            continue;
        }
        if overlay.start > at {
            out.push(at..overlay.start);
        }
        at = at.max(overlay.end);
    }
    if at < range.end {
        out.push(at..range.end);
    }
    out
}

/// The protection a private page takes for an image page's: copy-on-write is the image's own.
fn private_protection(protect: u32) -> Result<u32, Error> {
    match protect {
        PAGE_READONLY | PAGE_READWRITE => Ok(protect),
        PAGE_WRITECOPY => Ok(PAGE_READWRITE),
        other => Err(Error::new(format!(
            "image pages of protection {other:#x} outside the overlays: only data is kept as it was, and code runs only \
             from the code memory"
        ))),
    }
}

/// The asks' events: the doorbell the run process sets, and one per slot the daemon sets once
/// it answered there, each auto-reset and duplicated into the run process; and the process, to
/// see it end.
pub(crate) struct Bells {
    doorbell: OwnedHandle,
    slots: Vec<OwnedHandle>,
    process: OwnedHandle,
}

impl Bells {
    pub(crate) fn new(remote: &Remote, slots: usize) -> Result<(Bells, Vec<u64>), Error> {
        let process = remote.process.try_clone().map_err(|it| Error::new(format!("Windows: DuplicateHandle of a process: {it}")))?;
        let mut names = Vec::with_capacity(slots + 1);
        let doorbell = bell(&remote.process, &mut names)?;
        let mut kept = Vec::with_capacity(slots);
        for _ in 0..slots {
            kept.push(bell(&remote.process, &mut names)?);
        }
        Ok((Bells { doorbell, slots: kept, process }, names))
    }

    pub(crate) fn wake(&self, slot: usize, _: &AtomicU64) {
        if let Some(event) = self.slots.get(slot) {
            // SAFETY: an event this daemon made and holds.
            unsafe { SetEvent(event.as_raw_handle()) };
        }
    }

    pub(crate) fn wait_ring(&self, _: &mut Stream) -> Result<bool, Error> {
        let handles = [self.doorbell.as_raw_handle(), self.process.as_raw_handle()];
        // SAFETY: two handles this daemon holds, live for the call.
        let waited = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        match waited {
            WAIT_OBJECT_0 => Ok(true),
            it if it == WAIT_OBJECT_0 + 1 => Ok(false),
            _ => Err(os("WaitForMultipleObjects on a run process's doorbell")),
        }
    }
}

/// A new auto-reset event, and its name in `process` put in `names`.
fn bell(process: &OwnedHandle, names: &mut Vec<u64>) -> Result<OwnedHandle, Error> {
    // SAFETY: an unnamed auto-reset event, not signaled; value-only arguments.
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    if event.is_null() {
        return Err(os("CreateEventW for an asks bell"));
    }
    // SAFETY: just made, owned by nothing else.
    let event = unsafe { OwnedHandle::from_raw_handle(event) };
    let mut theirs: HANDLE = std::ptr::null_mut();
    // SAFETY: duplicates an event this daemon holds into the run process, opened with
    // PROCESS_DUP_HANDLE; writes only `theirs`, a handle value in that process.
    let ok = unsafe {
        DuplicateHandle(GetCurrentProcess(), event.as_raw_handle(), process.as_raw_handle(), &mut theirs, 0, 0, DUPLICATE_SAME_ACCESS)
    };
    if ok == 0 {
        return Err(os("DuplicateHandle of an asks bell into the run process"));
    }
    names.push(theirs as u64);
    Ok(event)
}

pub(crate) fn ring(_: &Stream, doorbell: u64) -> Result<(), Error> {
    // SAFETY: the doorbell the daemon duplicated into this process; a handle that is not an
    // event fails the call, which is checked.
    if unsafe { SetEvent(doorbell as HANDLE) } == 0 {
        return Err(os("SetEvent on the asks doorbell"));
    }
    Ok(())
}

pub(crate) fn hung_up(stream: &Stream) -> bool {
    stream.closed()
}

fn millis(timeout: std::time::Duration) -> u32 {
    u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1).clamp(1, INFINITE - 1)
}

pub(crate) fn wait_answer(_: &AtomicU64, _: u32, bell: u64, timeout: std::time::Duration) -> bool {
    // SAFETY: the slot's event the daemon duplicated into this process; a handle that is not
    // one fails the wait, and the caller looks at the state word either way.
    unsafe { WaitForSingleObject(bell as HANDLE, millis(timeout)) != WAIT_TIMEOUT }
}

/// Within this process only: on Windows the run process sleeps for a slot of its own.
pub(crate) fn wait(word: &AtomicU64, expected: u32, timeout: std::time::Duration) -> bool {
    // SAFETY: `word` is live and 8-aligned; WaitOnAddress compares its first four bytes, the
    // low half, with `expected`, which is live for the call.
    let ok = unsafe { WaitOnAddress(word.as_ptr().cast(), (&raw const expected).cast(), 4, millis(timeout)) };
    // SAFETY: GetLastError reads this thread's last error.
    ok != 0 || unsafe { GetLastError() } != ERROR_TIMEOUT
}

pub(crate) fn wake(word: &AtomicU64) {
    // SAFETY: wakes the threads waiting on `word`'s address; reads nothing.
    unsafe { WakeByAddressAll(word.as_ptr().cast()) };
}

/// The calling thread's last error, put back when dropped.
pub(crate) struct Errno(u32);

impl Errno {
    pub(crate) fn keep() -> Errno {
        // SAFETY: reads this thread's last error.
        Errno(unsafe { GetLastError() })
    }
}

impl Drop for Errno {
    fn drop(&mut self) {
        // SAFETY: sets this thread's last error.
        unsafe { SetLastError(self.0) };
    }
}

impl Remote {
    pub(crate) fn kill(&self) -> Result<(), Error> {
        // SAFETY: the run process's own handle, opened with PROCESS_TERMINATE.
        if unsafe { TerminateProcess(self.process.as_raw_handle(), 137) } == 0 {
            // SAFETY: the process handle, opened with PROCESS_SYNCHRONIZE; a zero wait returns
            // at once.
            if unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } == WAIT_OBJECT_0 {
                return Ok(());
            }
            return Err(os("TerminateProcess of the run process"));
        }
        Ok(())
    }

    /// Maps the whole of object `object` into the run process anywhere, if its rights allow
    /// `access`.
    pub(crate) fn map(&self, object: u32, access: Access) -> Result<u64, Error> {
        let Some((granted, rights)) = self.objects.get(object as usize) else {
            return Err(Error::new(format!("a map of object {object} of {}", self.objects.len())));
        };
        if !allowed(*rights, access) || access == Access::Execute {
            return Err(Error::new(format!("object {object} ({rights:?}) may not be mapped anywhere as {access:?}")));
        }
        map_view(granted.section.as_raw_handle(), self.process.as_raw_handle(), 0, granted.len, 0, 0, access)
    }

    pub(crate) fn overlay(&self, slide: u64, grant: &Grant) -> Result<(), Error> {
        let page = page();
        let base = grant.span.start.wrapping_add(slide);
        let end = grant
            .span
            .end
            .wrapping_add(slide)
            .checked_next_multiple_of(page)
            .filter(|it| *it > base && base.is_multiple_of(page))
            .ok_or_else(|| Error::new(format!("an image span {:#x}..{:#x} slid by {slide:#x}", grant.span.start, grant.span.end)))?;
        let overlays: Vec<Range<u64>> = grant.overlays.iter().map(|it| it.range()).map(|it| it.start + slide..it.end + slide).collect();
        for (overlay, range) in grant.overlays.iter().zip(&overlays) {
            granular(overlay.offset, overlay.len, range.start)?;
            let (_, rights) = self
                .objects
                .get(overlay.object as usize)
                .ok_or_else(|| Error::new(format!("an overlay of object {} of {}", overlay.object, self.objects.len())))?;
            if !allowed(*rights, overlay.access) {
                return Err(Error::new(format!("object {} ({rights:?}) may not be mapped {:?}", overlay.object, overlay.access)));
            }
        }
        let pieces = pieces(base..end, &overlays)?;
        let kept = self.keep(base, end, &overlays)?;
        let rebuilt = self.rebuild(base, end, &pieces, &grant.overlays, &kept);
        if let Err(error) = rebuilt {
            // SAFETY: the run process's own handle, opened with PROCESS_TERMINATE; a process
            // whose image is half replaced must not run on.
            unsafe { TerminateProcess(self.process.as_raw_handle(), 127) };
            return Err(Error::new(format!("{}; the run process was ended", error.what)));
        }
        Ok(())
    }

    fn query(&self, at: u64) -> Result<MEMORY_BASIC_INFORMATION, Error> {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: writes at most `size_of::<MEMORY_BASIC_INFORMATION>()` bytes into `info`.
        let got = unsafe {
            VirtualQueryEx(self.process.as_raw_handle(), at as *const c_void, &mut info, size_of::<MEMORY_BASIC_INFORMATION>())
        };
        if got == 0 {
            return Err(os(format_args!("VirtualQueryEx at {at:#x} in the run process")));
        }
        Ok(info)
    }

    /// Reads every committed page of the image at `base` outside the overlays. The image must
    /// be one view of an image section from `base`, ending at or before `end`.
    fn keep(&self, base: u64, end: u64, overlays: &[Range<u64>]) -> Result<Vec<Kept>, Error> {
        let mut kept = Vec::new();
        let mut at = base;
        while at < end {
            let info = self.query(at)?;
            let region = at..(info.BaseAddress as u64).saturating_add(info.RegionSize as u64).min(end);
            if info.State != MEM_COMMIT {
                at = region.end;
                continue;
            }
            if info.AllocationBase as u64 != base || info.Type != MEM_IMAGE {
                return Err(Error::new(format!(
                    "{:#x}..{:#x} of the run process's image span is not its image (allocation at {:#x}, type {:#x})",
                    region.start, region.end, info.AllocationBase as u64, info.Type
                )));
            }
            for part in outside(region.clone(), overlays) {
                if info.Protect == PAGE_NOACCESS {
                    continue;
                }
                let protect = private_protection(info.Protect)?;
                let mut bytes = vec![0u8; (part.end - part.start) as usize];
                let mut read = 0usize;
                // SAFETY: reads the run process's committed, readable pages at `part` into
                // `bytes`, of the same length.
                let got = unsafe {
                    ReadProcessMemory(
                        self.process.as_raw_handle(),
                        part.start as *const c_void,
                        bytes.as_mut_ptr().cast(),
                        bytes.len(),
                        &mut read,
                    )
                };
                if got == 0 || read != bytes.len() {
                    return Err(os(format_args!("ReadProcessMemory of {:#x}..{:#x}", part.start, part.end)));
                }
                kept.push(Kept { at: part.start, protect, bytes });
            }
            at = region.end;
        }
        Ok(kept)
    }

    fn rebuild(&self, base: u64, end: u64, pieces: &[Piece], overlays: &[Overlay], kept: &[Kept]) -> Result<(), Error> {
        let process = self.process.as_raw_handle();
        // SAFETY: unmaps the run process's image view, whose every page outside the overlays was
        // read above; its only thread that runs, the start hook's, waits on the pipe for the
        // OVERLAID this answers with, and reaches no byte of the image until then.
        if unsafe { UnmapViewOfFile2(process, MEMORY_MAPPED_VIEW_ADDRESS { Value: base as *mut c_void }, 0) } == 0 {
            return Err(os(format_args!("UnmapViewOfFile2 of the image at {base:#x}")));
        }
        // SAFETY: reserves the span just unmapped, free now, as one placeholder of no access.
        let placeholder = unsafe {
            VirtualAlloc2(
                process,
                base as *const c_void,
                (end - base) as usize,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            )
        };
        if placeholder as u64 != base {
            return Err(os(format_args!("VirtualAlloc2 of a placeholder over the image at {base:#x}..{end:#x}")));
        }
        for piece in pieces {
            let (at, len) = match *piece {
                Piece::View { at, len, .. } | Piece::Private { at, len } => (at, len),
            };
            if at + len < end {
                // SAFETY: splits `at..at + len` off the front of the placeholder this loop
                // still holds from `at`: no memory changes.
                if unsafe { VirtualFreeEx(process, at as *mut c_void, len as usize, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) } == 0 {
                    return Err(os(format_args!("VirtualFreeEx splitting {len:#x} bytes at {at:#x} off the placeholder")));
                }
            }
            match *piece {
                Piece::View { index, .. } => {
                    let overlay = &overlays[index];
                    let (object, _) = &self.objects[overlay.object as usize];
                    let kind = MEM_REPLACE_PLACEHOLDER;
                    map_view(object.section.as_raw_handle(), process, overlay.offset, len, at, kind, overlay.access)?;
                }
                Piece::Private { .. } => {
                    // SAFETY: replaces the placeholder at `at..at + len`, just split off, with
                    // reserved private address space.
                    let got = unsafe {
                        VirtualAlloc2(
                            process,
                            at as *const c_void,
                            len as usize,
                            MEM_RESERVE | MEM_REPLACE_PLACEHOLDER,
                            PAGE_NOACCESS,
                            std::ptr::null_mut(),
                            0,
                        )
                    };
                    if got as u64 != at {
                        return Err(os(format_args!("VirtualAlloc2 of {len:#x} private bytes at {at:#x}")));
                    }
                }
            }
        }
        for part in kept {
            let len = part.bytes.len();
            // SAFETY: commits pages of private address space reserved just above.
            let got = unsafe {
                VirtualAlloc2(process, part.at as *const c_void, len, MEM_COMMIT, PAGE_READWRITE, std::ptr::null_mut(), 0)
            };
            if got as u64 != part.at {
                return Err(os(format_args!("VirtualAlloc2 committing {len:#x} bytes at {:#x}", part.at)));
            }
            let mut written = 0usize;
            // SAFETY: writes the bytes read from these addresses back into the pages just
            // committed writable, of the same length.
            let got = unsafe {
                WriteProcessMemory(process, part.at as *const c_void, part.bytes.as_ptr().cast(), len, &mut written)
            };
            if got == 0 || written != len {
                return Err(os(format_args!("WriteProcessMemory of {len:#x} bytes at {:#x}", part.at)));
            }
            let mut old = 0;
            // SAFETY: the protection of the pages just written, as the image had them.
            if unsafe { VirtualProtectEx(process, part.at as *const c_void, len, part.protect, &mut old) } == 0 {
                return Err(os(format_args!("VirtualProtectEx of {len:#x} bytes at {:#x}", part.at)));
            }
        }
        Ok(())
    }
}

/// An object the start hook was granted, by its index in the grant: the daemon maps it.
pub(crate) struct Granted {
    index: u32,
    len: u64,
    rights: Rights,
}

pub(crate) fn recv_objects(channel: &mut Channel, count: usize) -> Result<Vec<Granted>, Error> {
    let bad = |it: FrameError| Error::new(format!("the daemon's list of objects: {it}"));
    let frame = channel.recv()?;
    if frame.seq != OBJECTS_SEQ {
        return Err(Error::new(format!("the daemon's list of objects came as frame {}", frame.seq)));
    }
    let mut read = frame.reader(kinds::GRANT).map_err(bad)?;
    let listed = read.count(9).map_err(bad)?;
    if listed != count {
        return Err(Error::new(format!("the daemon listed {listed} objects, where the grant names {count}")));
    }
    let mut out = Vec::with_capacity(listed);
    for index in 0..listed {
        let len = read.u64().map_err(bad)?;
        let rights = match read.u8().map_err(bad)? {
            1 => Rights::Code,
            2 => Rights::Words,
            other => return Err(Error::new(format!("object {index} granted with rights {other}"))),
        };
        out.push(Granted { index: index as u32, len, rights });
    }
    read.finish().map_err(bad)?;
    Ok(out)
}

/// The daemon's answer to a request of the start hook: the body of `want`, or why not.
fn answer<'a>(frame: crate::frame::Frame<'a>, want: crate::frame::Kind, what: &str) -> Result<&'a [u8], Error> {
    match frame.kind {
        kind if kind == want => Ok(frame.body),
        kinds::REFUSED => Err(Error::new(format!(
            "the daemon would not {what}: {}",
            Reader::new(frame.body).str().unwrap_or("(not UTF-8)")
        ))),
        other => Err(Error::new(format!("the daemon answered a request to {what} with a frame of kind {:#06x}", other.0))),
    }
}

impl Granted {
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn map_anywhere(&self, channel: &mut Channel, access: Access) -> Result<u64, Error> {
        if !allowed(self.rights, access) || access == Access::Execute {
            return Err(Error::new(format!("object {} ({:?}) mapped anywhere as {access:?}", self.index, self.rights)));
        }
        let bad = |it: FrameError| Error::new(format!("the daemon's answer to a map: {it}"));
        let mut body = Vec::with_capacity(5);
        crate::handshake::encode_map(self.index, access, &mut body);
        let what = format!("map object {}", self.index);
        let frame = channel.request(kinds::MAP, &body)?;
        let mut read = Reader::new(answer(frame, kinds::MAPPED, &what)?);
        let address = read.u64().map_err(bad)?;
        read.finish().map_err(bad)?;
        Ok(address)
    }
}

pub(crate) fn overlay(channel: &mut Channel, _: &[Granted], _: &[Overlay], _: u64) -> Result<(), Error> {
    let frame = channel.request(kinds::OVERLAY, &[])?;
    let body = answer(frame, kinds::OVERLAID, "lay the overlays over this image")?;
    if !body.is_empty() {
        return Err(Error::new(format!("an OVERLAID of {} bytes", body.len())));
    }
    Ok(())
}

#[link(name = "kernel32.dll", kind = "raw-dylib", modifiers = "+verbatim")]
unsafe extern "system" {
    /// psapi's `EnumProcessModules`, which kernel32 exports under this name: the process's
    /// modules in load order, read from its loader's list without taking the loader lock.
    fn K32EnumProcessModules(process: HANDLE, modules: *mut HMODULE, bytes: u32, needed: *mut u32) -> windows_sys::core::BOOL;
}

/// What the process's global scope is on Windows: every module the system loader loaded, the
/// executable first, each export found by `GetProcAddress` (forwarders followed), the first
/// module that exports a name winning.
pub(crate) struct Scope {
    /// Listed at the first lookup.
    modules: Option<Vec<HMODULE>>,
    /// The name being looked up, NUL-terminated; reused.
    name: Vec<u8>,
}

impl Scope {
    pub(crate) fn new() -> Scope {
        Scope { modules: None, name: Vec::new() }
    }

    fn modules() -> Result<Vec<HMODULE>, Error> {
        let mut modules: Vec<HMODULE> = vec![std::ptr::null_mut(); 64];
        loop {
            let bytes = u32::try_from(modules.len() * size_of::<HMODULE>())
                .map_err(|_| Error::new(format!("{} modules in this process", modules.len())))?;
            let mut needed = 0u32;
            // SAFETY: the process's own pseudo-handle; `modules` is `bytes` writable bytes and
            // `needed` a live `u32`, the only memory the call writes.
            if unsafe { K32EnumProcessModules(GetCurrentProcess(), modules.as_mut_ptr(), bytes, &mut needed) } == 0 {
                return Err(os("EnumProcessModules of this process"));
            }
            let count = needed as usize / size_of::<HMODULE>();
            if count <= modules.len() {
                modules.truncate(count);
                return Ok(modules);
            }
            modules.resize(count, std::ptr::null_mut());
        }
    }

    pub(crate) fn find(&mut self, name: &str) -> Result<Option<u64>, Error> {
        if name.is_empty() || name.as_bytes().contains(&0) {
            return Err(Error::new(format!("a lookup of {name:?}: not a C name")));
        }
        let modules = match &mut self.modules {
            Some(modules) => modules,
            none => none.insert(Scope::modules()?),
        };
        self.name.clear();
        self.name.reserve(name.len() + 1);
        self.name.extend_from_slice(name.as_bytes());
        self.name.push(0);
        for &module in modules.iter() {
            // SAFETY: `module` is a module of this process the loader listed, loaded for as
            // long as the start hook runs (it runs under the loader lock, so nothing unloads
            // one); `self.name` is NUL-terminated with no NUL before its end (checked above).
            if let Some(found) = unsafe { GetProcAddress(module, self.name.as_ptr()) } {
                return Ok(Some(found as usize as u64));
            }
        }
        Ok(None)
    }
}

pub(crate) fn own_image() -> Result<(u64, &'static [u8]), Error> {
    // SAFETY: a null name asks for the executable's own module, which stays for the process.
    let base = unsafe { GetModuleHandleW(std::ptr::null()) } as u64;
    if base == 0 || !base.is_multiple_of(page()) {
        return Err(os(format_args!("GetModuleHandleW of the executable ({base:#x})")));
    }
    // SAFETY: the system loader maps the headers' first page readable at the module's base for
    // the life of the process.
    let first = unsafe { std::slice::from_raw_parts(base as *const u8, 0x1000) };
    let len = crate::image::headers_len(crate::Format::Pe, first)
        .ok_or_else(|| Error::new("the executable's PE headers are not ones"))?;
    if len > 1 << 20 {
        return Err(Error::new(format!("the executable's headers are {len:#x} bytes")));
    }
    // SAFETY: the system loader maps `SizeOfHeaders` bytes of headers readable at the base.
    Ok((base, unsafe { std::slice::from_raw_parts(base as *const u8, len as usize) }))
}

/// NTFS has no clones; ReFS block cloning is not asked for: the caller writes the bytes.
pub(crate) fn clone_file(_: &Path, _: &Path) -> Result<bool, Error> {
    Ok(false)
}

/// A walk of every thread of the system: Windows lists threads by process no other way. A
/// thread missing from the list would have its fence slot reaped, so a walk that fails midway
/// is an error, never a shorter list.
pub(crate) fn threads(pid: u32, out: &mut Vec<u64>) -> Result<(), Error> {
    out.clear();
    // SAFETY: value-only arguments; the handle it returns is checked and owned below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(os("CreateToolhelp32Snapshot of the system's threads"));
    }
    // SAFETY: just made, owned by nothing else.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let size = size_of::<THREADENTRY32>() as u32;
    let mut entry = THREADENTRY32 { dwSize: size, ..Default::default() };
    // SAFETY: `entry` is a THREADENTRY32 whose `dwSize` says so; the call writes only it.
    if unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } == 0 {
        return Err(os("Thread32First"));
    }
    loop {
        if entry.th32OwnerProcessID == pid {
            out.push(u64::from(entry.th32ThreadID));
        }
        entry.dwSize = size;
        // SAFETY: as above.
        if unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                return Ok(());
            }
            return Err(Error::new(format!("Windows: Thread32Next: {error}")));
        }
    }
}

pub(crate) fn service() -> Option<String> {
    None
}

pub(crate) fn thread_id() -> u64 {
    // SAFETY: no arguments; reads the calling thread's ID.
    u64::from(unsafe { GetCurrentThreadId() })
}

pub(crate) fn serialize() {
    // SAFETY: `cpuid` touches no memory and writes only eax, ebx, ecx and edx; rbx, which LLVM
    // reserves, is kept in another register around it.
    unsafe {
        std::arch::asm!(
            "mov {saved}, rbx",
            "cpuid",
            "mov rbx, {saved}",
            saved = out(reg) _,
            inout("eax") 0u32 => _,
            inout("ecx") 0u32 => _,
            out("edx") _,
            options(nostack, preserves_flags),
        );
    }
}

/// `RtlCaptureContext` stores the vector registers with aligned stores; windows-sys's `CONTEXT`
/// is aligned to 8 only.
#[repr(C, align(16))]
struct Context(CONTEXT);

/// The return addresses of the calling thread's frames, unwound one by one with the tables the
/// system keeps (the loaded images' and those added with `RtlAddFunctionTable`). A frame
/// below the first with no unwind information can't be stepped over, since only a leaf may
/// lack it, so the walk did not finish: `None`. The outermost frame returns to 0.
#[inline(never)]
pub(crate) fn backtrace(out: &mut [u64]) -> Option<usize> {
    // SAFETY: CONTEXT is plain integers: all zero is a value.
    let mut context = Context(unsafe { std::mem::zeroed() });
    // SAFETY: fills the 16-byte aligned CONTEXT it is given with this thread's registers.
    unsafe { RtlCaptureContext(&mut context.0) };
    let mut len = 0;
    loop {
        let pc = context.0.Rip;
        let mut image = 0u64;
        // SAFETY: looks `pc` up in the function tables the system keeps; writes only `image`.
        let function = unsafe { RtlLookupFunctionEntry(pc, &mut image, std::ptr::null_mut()) };
        if function.is_null() {
            return None;
        }
        let mut data = std::ptr::null_mut();
        let mut frame = 0u64;
        // SAFETY: `function` is the entry the system has for `pc`, in a table that stays while
        // its code is mapped; unwinding reads this thread's own stack, which is mapped, and
        // writes only `context`, `data` and `frame`.
        let _ = unsafe {
            RtlVirtualUnwind(
                UNW_FLAG_NHANDLER,
                image,
                pc,
                function,
                &mut context.0,
                &mut data,
                &mut frame,
                std::ptr::null_mut(),
            )
        };
        if context.0.Rip == 0 {
            return Some(len);
        }
        *out.get_mut(len)? = context.0.Rip;
        len += 1;
    }
}
