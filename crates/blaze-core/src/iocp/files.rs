//! Files on Windows: the handles the port reads and appends through, and the calls of the
//! effects that block (a whole-file write, a rename, a clone, a mapping, a mode, times), which are
//! `crate::fs`'s on Windows, so `Threads` and `Iocp` do the same.

use std::{
    io::{self, Write as _},
    os::windows::io::{AsRawHandle as _, OwnedHandle},
    path::Path,
};

use windows_sys::Win32::{
    Foundation::{FILETIME, GENERIC_READ, GENERIC_WRITE, HANDLE},
    Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_END_OF_FILE_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FileEndOfFileInfo, GetFileAttributesW,
        GetFileSizeEx, GetVolumeInformationByHandleW, INVALID_FILE_ATTRIBUTES,
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_ALWAYS,
        OPEN_EXISTING, ReadFile, SetFileAttributesW, SetFileInformationByHandle, SetFileTime,
    },
    System::{
        IO::DeviceIoControl,
        Ioctl::{
            DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE,
            FSCTL_GET_INTEGRITY_INFORMATION, FSCTL_GET_INTEGRITY_INFORMATION_BUFFER,
            FSCTL_SET_INTEGRITY_INFORMATION, FSCTL_SET_INTEGRITY_INFORMATION_BUFFER,
        },
        SystemServices::FILE_SUPPORTS_BLOCK_REFCOUNTING,
    },
};

use super::sys;

/// Every other opener may read, write, rename and remove it meanwhile, as on Unix.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// What a handle of the port's is opened for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Access {
    /// Reads at offsets; the file must exist.
    Read,
    /// Writes at its end only (`FILE_APPEND_DATA`), made with its directories if missing.
    Append,
    /// Writes at offsets; the file must exist.
    WriteAt,
}

/// The file at `wide` (NUL-terminated, as [`sys::Paths`] makes it) opened for the port's
/// overlapped requests. The error is the system's, unnamed: the caller names the file.
pub(super) fn open(wide: &[u16], access: Access) -> io::Result<OwnedHandle> {
    debug_assert_eq!(wide.last(), Some(&0));
    let (rights, disposition) = match access {
        Access::Read => (GENERIC_READ, OPEN_EXISTING),
        Access::Append => (FILE_APPEND_DATA, OPEN_ALWAYS),
        Access::WriteAt => (GENERIC_WRITE, OPEN_EXISTING),
    };
    // SAFETY: a NUL-terminated path; no security attributes or template.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            rights,
            SHARE_ALL,
            std::ptr::null(),
            disposition,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    sys::owned(handle)
}

/// The whole file at `wide` (NUL-terminated, as [`sys::Paths`] makes it), read on the calling
/// thread: opened for reading alone (not overlapped, so each read returns once done), its
/// length asked, and read into a buffer one byte longer than that, so the read that comes back
/// short is the end: open, length, one read and close for a file that does not grow meanwhile.
/// The error is the system's, unnamed: the caller names the file.
pub(super) fn read_whole(wide: &[u16]) -> io::Result<Vec<u8>> {
    debug_assert_eq!(wide.last(), Some(&0));
    // SAFETY: a NUL-terminated path; no security attributes or template.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_SEQUENTIAL_SCAN,
            std::ptr::null_mut(),
        )
    };
    let handle = sys::owned(handle)?;
    let raw = handle.as_raw_handle();
    let size = usize::try_from(size(raw)?).unwrap_or(usize::MAX);
    // alloc: the bytes read, `Done::Read`'s payload, which the reads write in place.
    let mut bytes = Vec::with_capacity(size.saturating_add(1));
    loop {
        if bytes.len() == bytes.capacity() {
            // alloc: the file grew after its length was asked.
            bytes.reserve(bytes.capacity().max(4096));
        }
        let room = (bytes.capacity() - bytes.len()).min(u32::MAX as usize);
        let mut got = 0u32;
        // SAFETY: an open handle not opened for overlapped I/O, so the call returns once it has
        // written at most `room` bytes into the `Vec`'s spare room, which is that long, and
        // their count into `got`.
        sys::check(unsafe {
            ReadFile(raw, bytes.spare_capacity_mut().as_mut_ptr().cast(), room as u32, &mut got, std::ptr::null_mut())
        })?;
        // SAFETY: the call wrote `got` bytes, at most `room`, from the start of the spare room.
        unsafe { bytes.set_len(bytes.len() + (got as usize).min(room)) };
        if (got as usize) < room {
            return Ok(bytes);
        }
    }
}

/// The length of the file open as `handle`.
pub(super) fn size(handle: HANDLE) -> io::Result<u64> {
    let mut size = 0i64;
    // SAFETY: an open handle (the caller's) and an `i64` to write.
    sys::check(unsafe { GetFileSizeEx(handle, &mut size) })?;
    Ok(size as u64)
}

fn moved(from: &Path, to: &Path) -> io::Result<()> {
    let (wide_from, wide_to) = (sys::path(from)?, sys::path(to)?);
    // SAFETY: two NUL-terminated paths. Write-through: it returns once the new name is on
    // the disk, which a directory's sync is on Unix.
    sys::check(unsafe {
        MoveFileExW(
            wide_from.as_ptr(),
            wide_to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    })
}

/// `bytes` as the whole of `path`: a temporary file next to it, written and flushed, moved
/// over it with the move written through, so a crash leaves the old contents or the new.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    crate::fs::make_parent(path)?;
    let temporary = crate::fs::temporary_next_to(path);
    let written = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(bytes)?;
        // `FlushFileBuffers`.
        file.sync_all()
    })();
    let placed = written.and_then(|()| moved(&temporary, path));
    if placed.is_err() {
        _ = std::fs::remove_file(&temporary);
    }
    placed
}

/// `from` renamed over `to`, the new name on the disk when it returns.
pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
    crate::fs::make_parent(to)?;
    moved(from, to)
}

/// The file's permission bits as `mode` asks, where Windows keeps them. A file keeps one
/// permission of its own there, the read-only attribute, which is the owner's write bit. A
/// file runs by its name (`CreateProcessW` starts whatever it is given, consulting no bit), so
/// the bits that let one run need nothing; group and others reading and running as the owner
/// may (0o755, 0o644, 0o555, 0o444) is what the directory's access list already grants them.
/// Any other mode asks for what Windows cannot keep, and is refused.
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    let owner = (mode >> 6) & 0o7;
    let shared = owner & 0o5;
    let keepable =
        mode & !0o777 == 0 && owner & 0o4 != 0 && (mode >> 3) & 0o7 == shared && mode & 0o7 == shared;
    if !keepable {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            // alloc: an error path.
            format!(
                "{}: mode {mode:o} asks for what Windows does not keep: only the owner's write \
                 bit (the read-only attribute), with group and others reading and running as \
                 the owner may",
                path.display()
            ),
        ));
    }
    let wide = sys::path(path)?;
    // SAFETY: a NUL-terminated path.
    let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        let error = io::Error::last_os_error();
        // alloc: an error path.
        return Err(io::Error::new(error.kind(), format!("{}: {error}", path.display())));
    }
    let wanted = match owner & 0o2 {
        0 => attributes | FILE_ATTRIBUTE_READONLY,
        _ => attributes & !FILE_ATTRIBUTE_READONLY,
    };
    if wanted == attributes {
        return Ok(());
    }
    let wanted = match wanted & !FILE_ATTRIBUTE_DIRECTORY {
        0 if wanted & FILE_ATTRIBUTE_DIRECTORY == 0 => FILE_ATTRIBUTE_NORMAL,
        _ => wanted,
    };
    // SAFETY: a NUL-terminated path.
    sys::check(unsafe { SetFileAttributesW(wide.as_ptr(), wanted) })
        // alloc: an error path.
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

/// 100 ns intervals from 1601, where a `FILETIME` counts from, to the Unix epoch.
const UNIX_EPOCH_INTERVALS: u64 = 116_444_736_000_000_000;

/// `modified_ns` since the Unix epoch as a `FILETIME`: 100 ns intervals since 1601, NTFS's
/// resolution, so what is below 100 ns is what the file system cannot keep. Never 0 (which
/// `SetFileTime` reads as "leave it") nor past `i64::MAX`: `u64::MAX / 100` plus the offset
/// is some 3 * 10^17.
fn file_time(modified_ns: u64) -> FILETIME {
    let intervals = modified_ns / 100 + UNIX_EPOCH_INTERVALS;
    FILETIME { dwLowDateTime: intervals as u32, dwHighDateTime: (intervals >> 32) as u32 }
}

/// The access and modification times of the file or directory at `wide` (NUL-terminated, as
/// [`sys::Paths`] makes it) both `modified_ns` since the Unix epoch: a handle opened for its
/// attributes alone, `SetFileTime`, closed. The error is the system's, unnamed: the caller
/// names the file.
pub(super) fn set_times_of(wide: &[u16], modified_ns: u64) -> io::Result<()> {
    debug_assert_eq!(wide.last(), Some(&0));
    // SAFETY: a NUL-terminated path; no security attributes or template. Backup semantics
    // opens a directory as well as a file.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_WRITE_ATTRIBUTES,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    let handle = sys::owned(handle)?;
    let time = file_time(modified_ns);
    // SAFETY: an open handle with the right to write attributes, and a `FILETIME` alive across
    // the call for both the access and the write time; the null creation time is left as is.
    sys::check(unsafe { SetFileTime(handle.as_raw_handle(), std::ptr::null(), &time, &time) })
}

/// [`set_times_of`] of `path`, for the calls on threads; the error names `path`.
pub(crate) fn set_times(path: &Path, modified_ns: u64) -> io::Result<()> {
    let wide = sys::path(path)?;
    set_times_of(&wide, modified_ns)
        // alloc: an error path.
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

/// A handle of `path` for the blocking calls below, opened as `rights` asks.
fn plain(path: &Path, rights: u32, disposition: u32) -> io::Result<OwnedHandle> {
    let wide = sys::path(path)?;
    // SAFETY: a NUL-terminated path; no security attributes or template.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            rights,
            SHARE_ALL,
            std::ptr::null(),
            disposition,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    // alloc: an error path.
    sys::owned(handle).map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

/// The name of the file system `handle` is on, and whether it shares blocks between files.
fn file_system(handle: &OwnedHandle) -> io::Result<(String, bool)> {
    let mut name = [0u16; 64];
    let mut flags = 0;
    // SAFETY: an open handle, a buffer of the length given and a flags word to write; the
    // rest is not asked for.
    sys::check(unsafe {
        GetVolumeInformationByHandleW(
            handle.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            name.as_mut_ptr(),
            name.len() as u32,
        )
    })?;
    let len = name.iter().position(|&it| it == 0).unwrap_or(name.len());
    // alloc: a clone's check of its volume, once per clone, for its error.
    Ok((String::from_utf16_lossy(&name[..len]), flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0))
}

fn control<T, U>(handle: &OwnedHandle, code: u32, input: Option<&T>, output: Option<&mut U>) -> io::Result<()> {
    let (in_ptr, in_len) = input.map_or((std::ptr::null(), 0), |it| ((it as *const T).cast(), size_of::<T>()));
    let (out_ptr, out_len) =
        output.map_or((std::ptr::null_mut(), 0), |it| ((it as *mut U).cast(), size_of::<U>()));
    let mut returned = 0;
    // SAFETY: an open handle that is not overlapped, so the call is synchronous and the buffers
    // (of `size_of::<T>()` and `size_of::<U>()` bytes, from live references) outlive it; every
    // caller passes as `T` and `U` the `FSCTL_*` structs that `code` takes and returns, plain
    // integers valid for any bytes written; a `U` with invalid bit patterns or pointers would
    // be UB.
    sys::check(unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            code,
            in_ptr,
            in_len as u32,
            out_ptr,
            out_len as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    })
}

/// `to` made sharing `from`'s blocks (ReFS's block cloning), or an error naming the file
/// system that cannot: nothing copies instead.
pub(crate) fn clone(from: &Path, to: &Path) -> io::Result<()> {
    crate::fs::make_parent(to)?;
    let source = plain(from, GENERIC_READ, OPEN_EXISTING)?;
    let (system, shares) = file_system(&source)?;
    if !shares {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            // alloc: an error path.
            format!(
                "the file system of {} ({system}) cannot share blocks with {}; ReFS can, and \
                 blaze does not copy instead",
                to.display(),
                from.display()
            ),
        ));
    }
    let len = size(source.as_raw_handle())?;
    let target = plain(to, GENERIC_READ | GENERIC_WRITE, CREATE_NEW)?;
    let cloned = (|| {
        // Both must keep checksums alike, and the clone is in whole clusters.
        let mut integrity: FSCTL_GET_INTEGRITY_INFORMATION_BUFFER =
            // SAFETY: all zeros is a valid buffer of plain integers.
            unsafe { std::mem::zeroed() };
        control::<(), _>(&source, FSCTL_GET_INTEGRITY_INFORMATION, None, Some(&mut integrity))?;
        let set = FSCTL_SET_INTEGRITY_INFORMATION_BUFFER {
            ChecksumAlgorithm: integrity.ChecksumAlgorithm,
            Reserved: 0,
            Flags: integrity.Flags,
        };
        control::<_, ()>(&target, FSCTL_SET_INTEGRITY_INFORMATION, Some(&set), None)?;
        let end = FILE_END_OF_FILE_INFO { EndOfFile: len as i64 };
        // SAFETY: an open handle and the information of the class named, of its size.
        sys::check(unsafe {
            SetFileInformationByHandle(
                target.as_raw_handle(),
                FileEndOfFileInfo,
                (&raw const end).cast(),
                size_of::<FILE_END_OF_FILE_INFO>() as u32,
            )
        })?;
        let cluster = u64::from(integrity.ClusterSizeInBytes.max(1));
        // Each call's count below 4 GiB, as the call takes, in whole clusters.
        let most = (u64::from(u32::MAX) / cluster) * cluster;
        let mut at = 0;
        while at < len {
            let count = (len - at).div_ceil(cluster) * cluster;
            let count = count.min(most);
            let extents = DUPLICATE_EXTENTS_DATA {
                FileHandle: source.as_raw_handle(),
                SourceFileOffset: at as i64,
                TargetFileOffset: at as i64,
                ByteCount: count as i64,
            };
            control::<_, ()>(&target, FSCTL_DUPLICATE_EXTENTS_TO_FILE, Some(&extents), None)?;
            at += count;
        }
        Ok(())
    })();
    if cloned.is_err() {
        drop(target);
        _ = std::fs::remove_file(to);
    }
    cloned.map_err(|error: io::Error| {
        // alloc: an error path.
        io::Error::new(error.kind(), format!("cloning {} to {}: {error}", from.display(), to.display()))
    })
}

// The whole file mapped read-only and shared: `crate::fs::Mapped`'s, as on Unix. Windows
// refuses to replace or cut a file a view of which is open.
pub(crate) use crate::fs::map;

