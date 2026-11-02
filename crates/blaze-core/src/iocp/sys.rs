//! What every part of the driver shares of Windows: wide strings and paths, owned handles, the
//! head an `OVERLAPPED` starts of each request, the buffers the kernel writes into, and the
//! completion port.

use std::{
    cell::UnsafeCell,
    ffi::OsStr,
    io,
    marker::PhantomPinned,
    os::windows::{
        ffi::OsStrExt as _,
        io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::NonNull,
    time::Duration,
};

use blaze_mem::{MemError, Region, Tag as MemTag};
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT},
    Storage::FileSystem::{GetFullPathNameW, SetFileCompletionNotificationModes},
    System::IO::{
        CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED, OVERLAPPED_0_0,
        OVERLAPPED_ENTRY, PostQueuedCompletionStatus,
    },
};

use crate::FileName;

/// A request's own completion: its `lpOverlapped` is a [`Head`] of the driver's.
pub(super) const KEY_IO: usize = 0;
/// Work done elsewhere sent what it came to through the channel.
pub(super) const KEY_WAKE: usize = 1;
/// A spawn's process ended: its `lpOverlapped` carries the bits of the spawn's handle, not an
/// address.
pub(super) const KEY_EXIT: usize = 2;
/// The warm launcher's process ended: its `lpOverlapped` carries the bits of the launcher's
/// handle, not an address.
pub(super) const KEY_WARM: usize = 3;
/// Keys from here are a job's: the bits of its spawn's handle, which are at least 2^32 (a live
/// handle's generation is odd), so above every key before. Its `lpOverlapped` is a process id
/// and its byte count the message (`JOB_OBJECT_MSG_*`), neither of them an address.
pub(super) const KEY_JOB: usize = 16;

// A handle's 64 bits are a completion key and a packet's `lpOverlapped` as they are.
const _: () = assert!(usize::BITS == 64);

/// What a buffer the kernel writes into holds: a watch's records, a read of a process's
/// output.
pub(super) const BUFFER: usize = 64 << 10;

const BACKSLASH: u16 = b'\\' as u16;
const SLASH: u16 = b'/' as u16;
const VERBATIM: [u16; 4] = [BACKSLASH, BACKSLASH, b'?' as u16, BACKSLASH];
const VERBATIM_UNC: [u16; 8] =
    [BACKSLASH, BACKSLASH, b'?' as u16, BACKSLASH, b'U' as u16, b'N' as u16, b'C' as u16, BACKSLASH];

/// `s` as a NUL-terminated wide string, or an error where it holds a NUL of its own.
pub(super) fn wide(s: &OsStr) -> io::Result<Vec<u16>> {
    // alloc: setup and the threads' calls; the port's paths are made by `Paths`.
    let mut wide: Vec<u16> = s.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            // alloc: an error path.
            format!("{}: a NUL inside a name", s.to_string_lossy()),
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// A NUL-terminated wide string of fewer than `N` units, made on the stack: a name a call takes
/// once per spawn, made without allocating.
pub(super) struct WideName<const N: usize> {
    units: [u16; N],
    len: usize,
}

impl<const N: usize> WideName<N> {
    pub(super) fn new() -> WideName<N> {
        WideName { units: [0; N], len: 0 }
    }

    pub(super) fn text(&mut self, text: &str) -> &mut WideName<N> {
        for unit in text.encode_utf16() {
            self.push(unit);
        }
        self
    }

    pub(super) fn number(&mut self, mut n: u64) -> &mut WideName<N> {
        let mut digits = [0u16; 20];
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' as u16 + (n % 10) as u16;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        for &unit in &digits[at..] {
            self.push(unit);
        }
        self
    }

    fn push(&mut self, unit: u16) {
        // The last unit stays the NUL.
        assert!(self.len + 1 < N, "a name past the {N} units made for it");
        self.units[self.len] = unit;
        self.len += 1;
    }

    /// The name and its NUL.
    pub(super) fn as_ptr(&self) -> *const u16 {
        self.units.as_ptr()
    }
}

/// `path` made absolute, `\`-separated and verbatim (`\\?\`), so no length limit or
/// reinterpretation of its names applies, NUL-terminated.
pub(super) fn path(path: &Path) -> io::Result<Vec<u16>> {
    // alloc: setup and the threads' calls; the port's paths are made by `Paths`.
    let absolute = std::path::absolute(path)?;
    let text = absolute.as_os_str().to_string_lossy();
    if absolute.as_os_str().to_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            // alloc: an error path.
            format!("{text}: a path that is not Unicode"),
        ));
    }
    // alloc: as above.
    let text = text.replace('/', "\\");
    let verbatim = if text.starts_with(r"\\?\") {
        text
    } else if let Some(unc) = text.strip_prefix(r"\\") {
        // alloc: as above.
        format!(r"\\?\UNC\{unc}")
    } else {
        // alloc: as above.
        format!(r"\\?\{text}")
    };
    wide(OsStr::new(&verbatim))
}

/// Wide paths of names under the driver's root, each what [`path`] makes of `root.join(name)`,
/// made in buffers kept for the driver's life.
pub(super) struct Paths {
    root: PathBuf,
    /// What `root.join(name)` puts before a name with no prefix or root of its own: the root
    /// and the separator std adds after it.
    prefix: Vec<u16>,
    /// The root is verbatim (`\\?\`), so `std::path::absolute` leaves a name under it as it is.
    verbatim: bool,
    unicode: bool,
    joined: Vec<u16>,
    full: Vec<u16>,
    out: Vec<u16>,
}

impl Paths {
    pub(super) fn new(root: &Path) -> Paths {
        // alloc: setup, once per driver.
        let probe = root.join("x");
        // alloc: setup.
        let mut prefix: Vec<u16> = probe.as_os_str().encode_wide().collect();
        prefix.pop();
        Paths {
            verbatim: prefix.starts_with(&VERBATIM),
            unicode: root.to_str().is_some(),
            // alloc: setup.
            root: root.to_owned(),
            prefix,
            // alloc: setup, the buffers every path is made in, grown to the longest.
            joined: Vec::with_capacity(512),
            full: Vec::with_capacity(512), // alloc: setup.
            out: Vec::with_capacity(512),  // alloc: setup.
        }
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    /// `name` under the root, NUL-terminated: what `path(&root.join(name))` makes, without
    /// allocating once the buffers are as long as the longest path.
    pub(super) fn of(&mut self, name: &FileName) -> io::Result<&[u16]> {
        let text = name.as_str();
        let bytes = text.as_bytes();
        // A name std reads as having a prefix or root of its own (which `join` puts in place of
        // the root's), or refuses, or a root that is not Unicode: made as std makes it.
        if !self.unicode || text.contains('\0') || bytes[0] == b'\\' || bytes.get(1) == Some(&b':') {
            // alloc: a name Windows reads as rooted, or one that fails.
            let wide = path(&self.root.join(text))?;
            self.out.clear();
            self.out.extend_from_slice(&wide);
            return Ok(&self.out);
        }
        self.joined.clear();
        self.joined.extend_from_slice(&self.prefix);
        self.joined.extend(text.encode_utf16());
        self.out.clear();
        let slashed = |unit: &u16| if *unit == SLASH { BACKSLASH } else { *unit };
        if self.verbatim {
            self.out.extend(self.joined.iter().map(slashed));
            self.out.push(0);
            return Ok(&self.out);
        }
        self.joined.push(0);
        full_path(&self.joined, &mut self.full)?;
        let full = &self.full[..];
        let rest = if full.starts_with(&VERBATIM) {
            full
        } else if full.starts_with(&[BACKSLASH, BACKSLASH]) {
            self.out.extend_from_slice(&VERBATIM_UNC);
            &full[2..]
        } else {
            self.out.extend_from_slice(&VERBATIM);
            full
        };
        self.out.extend(rest.iter().map(slashed));
        self.out.push(0);
        Ok(&self.out)
    }
}

/// What `GetFullPathNameW` makes of the NUL-terminated `wide`, into `into` (no NUL): what
/// `std::path::absolute` asks of it.
fn full_path(wide: &[u16], into: &mut Vec<u16>) -> io::Result<()> {
    loop {
        let room = into.capacity().max(260);
        into.resize(room, 0);
        // SAFETY: a NUL-terminated path, a buffer of the length given, and no part asked for.
        let len = unsafe { GetFullPathNameW(wide.as_ptr(), room as u32, into.as_mut_ptr(), std::ptr::null_mut()) };
        let len = len as usize;
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        if len < room {
            into.truncate(len);
            return Ok(());
        }
        // Too short: `len` is the room it needs, its NUL too.
        // alloc: grown once to the longest path.
        into.reserve(len - into.len() + 1);
    }
}

/// The handle a call returned, or the error it set where it returned none.
pub(super) fn owned(raw: HANDLE) -> io::Result<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a handle the call just made, owned by no one else.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// Ok where a call that returns a `BOOL` returned true.
pub(super) fn check(ok: windows_sys::core::BOOL) -> io::Result<()> {
    match ok {
        0 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}

/// Ok where an overlapped call is in flight: it returned true, or false with
/// `ERROR_IO_PENDING`. Either way its completion comes to the port: for a handle that does not
/// skip the port on success ([`Port::associate`]).
pub(super) fn pending(ok: windows_sys::core::BOOL) -> io::Result<()> {
    match ok != 0 || last() == ERROR_IO_PENDING {
        true => Ok(()),
        false => Err(io::Error::last_os_error()),
    }
}

/// How an overlapped call on a handle that skips the port on success
/// ([`Port::associate_skipping`]) started: `Ok(true)` where it ended at once, so no packet
/// comes and how it ended is in its `OVERLAPPED` already ([`Head::result`]); `Ok(false)` where
/// it is in flight and its packet comes.
pub(super) fn started(ok: windows_sys::core::BOOL) -> io::Result<bool> {
    if ok != 0 {
        return Ok(true);
    }
    match last() {
        ERROR_IO_PENDING => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

/// `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE` (winbase.h).
const SKIP_ON_SUCCESS: u8 = 1 | 2;

/// The error code a call just set.
pub(super) fn last() -> u32 {
    // SAFETY: no arguments.
    unsafe { GetLastError() }
}

/// What a request is to the kernel: an `OVERLAPPED`, which a completion hands back, and the
/// bits of the request's handle after it, read back through it.
#[repr(C)]
struct Slot {
    overlapped: OVERLAPPED,
    id: u64,
}

/// A request's [`Slot`], inline in the request's entry of the driver's slab, which never moves
/// while it is there. The kernel writes the `OVERLAPPED` while a step is in flight
/// (`UnsafeCell`), and no `&mut` of the request may assume it alone reaches those bytes
/// (`PhantomPinned`). The entry is let go only once the packet of its last step is taken
/// (`Iocp::requests`).
pub(super) struct Head {
    slot: UnsafeCell<Slot>,
    _pinned: PhantomPinned,
}

impl Head {
    /// The head of the request whose handle's bits are `id`.
    pub(super) fn new(id: u64) -> Head {
        // SAFETY: all zeros is an `OVERLAPPED` of no offset and no event.
        let overlapped = unsafe { std::mem::zeroed() };
        Head { slot: UnsafeCell::new(Slot { overlapped, id }), _pinned: PhantomPinned }
    }

    /// The `OVERLAPPED`, for a call that starts a step or cancels one.
    pub(super) fn overlapped(&self) -> *mut OVERLAPPED {
        // `Slot` is `repr(C)` and starts with it.
        self.slot.get().cast()
    }

    /// Made ready for the request's next step, at `at` of a file (0 where none applies). Only
    /// while no step of it is in flight: every caller asks for the step at once.
    pub(super) fn reset(&self, at: u64) -> *mut OVERLAPPED {
        let overlapped = self.overlapped();
        // SAFETY: all zeros is an `OVERLAPPED` of no offset and no event: integers, a union of
        // integers and a null handle.
        let mut fresh: OVERLAPPED = unsafe { std::mem::zeroed() };
        fresh.Anonymous.Anonymous = OVERLAPPED_0_0 { Offset: at as u32, OffsetHigh: (at >> 32) as u32 };
        // SAFETY: `overlapped` is the first field of this head's `repr(C)` slot, aligned and
        // live while `self` is; no step of the request is in flight (every caller starts one
        // at once after this), so neither the kernel nor another thread reads or writes it.
        // Writing it while a step was in flight would race the kernel's completion write.
        unsafe { overlapped.write(fresh) };
        overlapped
    }

    /// How the step ended, once its completion is dequeued or its call ended it at once
    /// ([`started`]): the bytes it moved, or the Win32 error its status (an `NTSTATUS`, in
    /// `Internal`) says.
    pub(super) fn result(&self) -> Result<u32, u32> {
        // SAFETY: the step's packet was dequeued, or its call returned having ended it on a
        // handle that skips the port, so the kernel writes the slot no more.
        let overlapped = unsafe { &*self.overlapped() };
        let status = overlapped.Internal as i32;
        match status < 0 {
            // SAFETY: no pointers.
            true => Err(unsafe { windows_sys::Win32::Foundation::RtlNtStatusToDosError(status) }),
            false => Ok(overlapped.InternalHigh as u32),
        }
    }

    /// The bits of the handle of the request whose `OVERLAPPED` a packet hands back.
    ///
    /// # Safety
    ///
    /// `overlapped` is a [`Head::overlapped`], in memory that is still mapped: a request's
    /// entry, which the slab keeps mapped for the driver's life, or a test's own head.
    pub(super) unsafe fn id_of(overlapped: *const OVERLAPPED) -> u64 {
        let id = overlapped.cast::<u8>().wrapping_add(std::mem::offset_of!(Slot, id)).cast::<u64>();
        // SAFETY: as the caller says, `overlapped` starts a live `repr(C)` `Slot`, so `id` is
        // its aligned `u64` field. The id is written as the request is made, never by the
        // kernel, which writes only the `OVERLAPPED`; a foreign pointer would read anything.
        unsafe { id.read() }
    }
}

/// [`BUFFER`]-byte buffers in one reserved region, each where it is for the region's life,
/// committed as more are made and given back to a free list when the request that reads into
/// it ends: a warm watch or spawn takes one and allocates nothing.
pub(super) struct Buffers {
    region: Region,
    made: u32,
    /// Room for every buffer the region holds, so giving one back never grows it.
    free: Vec<u32>,
}

/// One of [`Buffers`]: 64 KiB aligned, so aligned for the records the kernel writes in it.
#[derive(Clone, Copy)]
pub(super) struct Buffer {
    index: u32,
    at: NonNull<u8>,
}

impl Buffer {
    pub(super) fn ptr(self) -> *mut u8 {
        self.at.as_ptr()
    }
}

impl Buffers {
    pub(super) fn new(max: u32) -> Result<Buffers, MemError> {
        Ok(Buffers {
            region: Region::reserve(max as usize * BUFFER, MemTag::Io)?,
            made: 0,
            // alloc: setup, the free list's room for every buffer.
            free: Vec::with_capacity(max as usize),
        })
    }

    pub(super) fn take(&mut self) -> io::Result<Buffer> {
        let index = match self.free.pop() {
            Some(index) => index,
            None => {
                let index = self.made;
                self.region.ensure((index as usize + 1) * BUFFER).map_err(io::Error::other)?;
                self.made += 1;
                index
            }
        };
        Ok(Buffer { index, at: self.region.at(index as usize * BUFFER) })
    }

    /// `buffer` given back, once nothing reads into it.
    pub(super) fn give(&mut self, buffer: Buffer) {
        debug_assert!(self.free.len() < self.free.capacity());
        self.free.push(buffer.index);
    }
}

/// The completion port: every request's completion, the jobs' messages and the wakes of work
/// done elsewhere come to it.
pub(super) struct Port(OwnedHandle);

impl Port {
    pub(super) fn new() -> io::Result<Port> {
        // SAFETY: no handle to associate; a new port, for one thread at a time.
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
        Ok(Port(owned(port)?))
    }

    pub(super) fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
    }

    /// Completions of overlapped requests of `handle` come to this port, with [`KEY_IO`].
    pub(super) fn associate(&self, handle: HANDLE) -> io::Result<()> {
        // SAFETY: an open handle of the caller's and this port.
        let port = unsafe { CreateIoCompletionPort(handle, self.raw(), KEY_IO, 0) };
        match port.is_null() {
            true => Err(io::Error::last_os_error()),
            false => Ok(()),
        }
    }

    /// As [`Port::associate`], and a request on `handle` that its call ends at once (a read
    /// the cache holds, a write it takes) posts no packet: [`started`] says so, and the driver
    /// ends it where it asked. Only for handles whose every request is asked through
    /// [`started`]: the files' (`Op::Read`, `Op::Write`).
    pub(super) fn associate_skipping(&self, handle: HANDLE) -> io::Result<()> {
        self.associate(handle)?;
        // SAFETY: an open handle of the caller's, no request yet on it.
        check(unsafe { SetFileCompletionNotificationModes(handle, SKIP_ON_SUCCESS) })
    }

    /// Posts a packet of `key`, whose `carried` is a number of the key's, never an address.
    pub(super) fn post(&self, key: usize, bytes: u32, carried: usize) -> io::Result<()> {
        post(self.raw(), key, bytes, carried)
    }

    /// Up to `entries.len()` packets, waiting up to `wait` for the first; how many came.
    pub(super) fn take(&self, entries: &mut [OVERLAPPED_ENTRY], wait: Duration) -> io::Result<usize> {
        let mut got = 0;
        // Rounded up, so a wait shorter than a millisecond is not a spin.
        let ms = match wait.is_zero() {
            true => 0,
            false => u32::try_from(wait.as_nanos().div_ceil(1_000_000)).unwrap_or(u32::MAX - 1),
        };
        // SAFETY: room for `entries.len()` entries and a count to write.
        let ok = unsafe {
            GetQueuedCompletionStatusEx(
                self.raw(),
                entries.as_mut_ptr(),
                entries.len() as u32,
                &mut got,
                ms,
                0,
            )
        };
        if ok == 0 {
            return match last() {
                WAIT_TIMEOUT => Ok(0),
                _ => Err(io::Error::last_os_error()),
            };
        }
        Ok(got as usize)
    }
}

/// A packet of `key` posted to the port `port`, which is open; `carried` is a number of the
/// key's. Refuses [`KEY_IO`]: the driver reads a `KEY_IO` packet's `OVERLAPPED` through as a
/// [`Head`], so only the kernel's completions, or [`post_head`], may carry that key.
pub(super) fn post(port: HANDLE, key: usize, bytes: u32, carried: usize) -> io::Result<()> {
    if key == KEY_IO {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a KEY_IO packet carries a request's OVERLAPPED, which `post` cannot vouch for",
        ));
    }
    // SAFETY: an open port; the call only queues `carried`, handed back as a number and never
    // read through for any key but `KEY_IO`, refused above.
    check(unsafe { PostQueuedCompletionStatus(port, bytes, key, carried as *const OVERLAPPED) })
}

/// A [`KEY_IO`] packet handing back `head`, as the kernel's completion of a request would.
///
/// # Safety
///
/// `head` is a [`Head::overlapped`] whose `Head` stays where it is, alive, until the driver has
/// taken the packet: the driver reads the head's id through it.
#[cfg(test)]
pub(super) unsafe fn post_head(port: HANDLE, head: *mut OVERLAPPED) -> io::Result<()> {
    // SAFETY: an open port; the caller keeps `head` alive until the packet is taken.
    check(unsafe { PostQueuedCompletionStatus(port, 0, KEY_IO, head) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name made on the stack is what `format!` and [`wide`] make of the same parts, and
    /// allocates nothing.
    #[test]
    fn a_wide_name_is_the_formatted_one() {
        let mut rng = crate::sim::Rng::new(7);
        for _ in 0..1000 {
            let (pid, n, count) = (rng.next_u64() as u32, rng.next_u64() >> rng.below(64), rng.below(1000));
            let (name, used) = blaze_alloc_count::measure(|| {
                let mut name = WideName::<128>::new();
                name.text(r"\\.\pipe\blaze-").number(u64::from(pid)).text("-").number(n).text("-out-").number(count);
                name
            });
            assert_eq!(used.allocations, 0);
            let wanted = wide(OsStr::new(&format!(r"\\.\pipe\blaze-{pid}-{n}-out-{count}"))).unwrap();
            // SAFETY: the name's units, NUL-terminated within its room.
            let made = unsafe { std::slice::from_raw_parts(name.as_ptr(), wanted.len()) };
            assert_eq!(made, &wanted[..]);
        }
    }

    /// Safe `post` refuses the one key whose packet the driver reads through, and posts the
    /// others as numbers, which come back as they were sent.
    #[test]
    fn a_posted_packet_never_carries_an_overlapped() {
        let port = Port::new().unwrap();
        let refused = port.post(KEY_IO, 0, 0x1000).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        port.post(KEY_WAKE, 3, 7).unwrap();
        // SAFETY: all zeros is an `OVERLAPPED_ENTRY` of no packet: integers and a null pointer.
        let mut entries = [unsafe { std::mem::zeroed::<OVERLAPPED_ENTRY>() }; 2];
        assert_eq!(port.take(&mut entries, Duration::from_millis(100)).unwrap(), 1);
        assert_eq!(entries[0].lpCompletionKey, KEY_WAKE);
        assert_eq!(entries[0].lpOverlapped as usize, 7);
        assert_eq!(entries[0].dwNumberOfBytesTransferred, 3);
        assert_eq!(port.take(&mut entries, Duration::ZERO).unwrap(), 0, "the refused packet was posted");
    }
}
