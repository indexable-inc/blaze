//! [`crate::Effect::Stamp`] and [`crate::Effect::Entries`] on Windows: what a file is now, as
//! much as a sweep compares (docs/v2/plan/lanes/frontend/journal/windows.md).
//!
//! A name is stamped by a handle that asks for its attributes only, with every opener let in
//! and a link or junction opened itself: its file index (the NTFS file reference number), its
//! size, `LastWriteTime` and `ChangeTime`. A directory's entries come from the directory
//! listing itself (`FileIdBothDirectoryInfo`, 64 KiB of entries per call), which has every one
//! of those, so no entry is opened.

use std::{
    io,
    os::windows::io::{AsRawHandle as _, OwnedHandle},
    path::Path,
};

use windows_sys::Win32::{
    Foundation::{
        ERROR_BAD_PATHNAME, ERROR_DIRECTORY, ERROR_FILE_NOT_FOUND, ERROR_INVALID_NAME,
        ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileAttributeTagInfo, FileBasicInfo, FileIdBothDirectoryInfo,
        FileIdBothDirectoryRestartInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        OPEN_EXISTING,
    },
};

use super::sys;
use crate::{FileName, Names, S_IFDIR, S_IFLNK, S_IFREG, Stamp};

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// How many names one thread of a sweep stamps.
const CHUNK: usize = 2048;

/// What one call of a listing fills.
const LISTING: usize = 64 << 10;

/// 100-nanosecond ticks from 1601 to 1970.
const EPOCH: u64 = 116_444_736_000_000_000;

/// `IsReparseTagNameSurrogate`: the reparse point names another file (a symbolic link, a
/// junction), as against one a filter keeps data behind (a cloud file, a deduplicated one),
/// which reads as the file it is. `std::fs` draws the same line.
const NAME_SURROGATE: u32 = 0x2000_0000;

// `FILE_ID_BOTH_DIR_INFO` as winioctl.h's `FILE_ID_BOTH_DIR_INFORMATION` lays it out, read
// field by field from the listing's bytes.
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, NextEntryOffset) == 0);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, LastWriteTime) == 24);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, ChangeTime) == 32);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, EndOfFile) == 40);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileAttributes) == 56);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength) == 60);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, EaSize) == 64);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileId) == 96);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileName) == 104);
};

/// Ticks of a `FILETIME` as nanoseconds since 1970, wrapping for a time before it: distinct
/// times stay distinct, which is all a stamp needs.
fn ns(ticks: i64) -> u64 {
    (ticks as u64).wrapping_sub(EPOCH).wrapping_mul(100)
}

/// `st_mode` as a system with none is made to say it: a name-surrogate reparse point is a link,
/// then a directory, then a file, whose owner may write unless it is read-only.
fn mode(attributes: u32, tag: u32) -> u32 {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && tag & NAME_SURROGATE != 0 {
        S_IFLNK | 0o777
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        S_IFDIR | 0o755
    } else if attributes & FILE_ATTRIBUTE_READONLY != 0 {
        S_IFREG | 0o444
    } else {
        S_IFREG | 0o644
    }
}

/// A verbatim wide path made the prefix each name is put after: no NUL and no `\` at its end.
fn trim(wide: &mut Vec<u16>) {
    wide.pop();
    if wide.last() == Some(&u16::from(b'\\')) {
        wide.pop();
    }
}

fn open(wide: &[u16], rights: u32, flags: u32) -> io::Result<OwnedHandle> {
    debug_assert_eq!(wide.last(), Some(&0));
    // SAFETY: a NUL-terminated path; no security attributes or template.
    sys::owned(unsafe {
        CreateFileW(wide.as_ptr(), rights, SHARE_ALL, std::ptr::null(), OPEN_EXISTING, flags, std::ptr::null_mut())
    })
}

/// Whether a name has a character Windows keeps out of names, so nothing is there by it: a
/// `\` or `:` would name another file or a stream of one.
fn unnameable(name: &str) -> bool {
    name.bytes().any(|it| it < 0x20 || matches!(it, b'\\' | b':' | b'*' | b'?' | b'"' | b'<' | b'>' | b'|'))
}

/// The stamp of what `handle` is open on.
fn stamp_of(handle: &OwnedHandle) -> io::Result<Stamp> {
    let raw = handle.as_raw_handle();
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: an open handle and room for one `BY_HANDLE_FILE_INFORMATION`.
    sys::check(unsafe { GetFileInformationByHandle(raw, &mut info) })?;
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: an open handle and room for the `FILE_BASIC_INFO` the class asks for, of the size
    // given; plain integers, valid for any bytes written.
    sys::check(unsafe {
        GetFileInformationByHandleEx(
            raw,
            FileBasicInfo,
            (&raw mut basic).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    })?;
    let mut tag = FILE_ATTRIBUTE_TAG_INFO::default();
    if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        // SAFETY: as above, for the class's `FILE_ATTRIBUTE_TAG_INFO`.
        sys::check(unsafe {
            GetFileInformationByHandleEx(
                raw,
                FileAttributeTagInfo,
                (&raw mut tag).cast(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        })?;
    }
    Ok(Stamp {
        ino: u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow),
        size: u64::from(info.nFileSizeHigh) << 32 | u64::from(info.nFileSizeLow),
        mtime_ns: ns(basic.LastWriteTime),
        ctime_ns: ns(basic.ChangeTime),
        mode: mode(basic.FileAttributes, tag.ReparseTag),
    })
}

/// The stamp of `name` under the root whose wide path is `prefix`, with `path` the buffer it is
/// put together in.
fn stamp_name(prefix: &[u16], name: &str, path: &mut Vec<u16>) -> Stamp {
    if unnameable(name) {
        return Stamp::GONE;
    }
    path.clear();
    path.extend_from_slice(prefix);
    path.push(u16::from(b'\\'));
    path.extend(name.encode_utf16().map(|it| if it == u16::from(b'/') { u16::from(b'\\') } else { it }));
    path.push(0);
    match open(path, FILE_READ_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT) {
        Ok(handle) => stamp_of(&handle).unwrap_or(Stamp::UNKNOWN),
        Err(error) => match error.raw_os_error().map(|it| it as u32) {
            Some(
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_DIRECTORY | ERROR_INVALID_NAME
                | ERROR_BAD_PATHNAME,
            ) => Stamp::GONE,
            _ => Stamp::UNKNOWN,
        },
    }
}

/// How many threads stamp `names` names: one per [`CHUNK`] of them, at most one per core. A
/// name costs an open, two queries and a close (some 20 µs, most of it the open), so a thread
/// spawned for 2,048 of them (some 40 ms) costs it under 0.1%.
fn sweepers(names: usize, cores: usize) -> usize {
    names.div_ceil(CHUNK).clamp(1, cores.max(1))
}

/// Every name's stamp, in order, on [`sweepers`] threads at once. Fails only when the root
/// cannot be opened.
pub(crate) fn stamps(root: &Path, names: &Names) -> io::Result<Vec<Stamp>> {
    // alloc: an error path.
    let named = |error: io::Error| io::Error::new(error.kind(), format!("{}: {error}", root.display()));
    let mut prefix = sys::path(root).map_err(named)?;
    open(&prefix, 0, FILE_FLAG_BACKUP_SEMANTICS).map_err(named)?;
    trim(&mut prefix);
    // alloc: the stamps the sweep answers with, once per sweep.
    let mut out = vec![Stamp::GONE; names.len()];
    let threads = sweepers(names.len(), std::thread::available_parallelism().map_or(1, |it| it.get()));
    if threads == 1 {
        let mut path = Vec::with_capacity(prefix.len() + 256);
        for (at, stamp) in out.iter_mut().enumerate() {
            *stamp = stamp_name(&prefix, names.get(at), &mut path);
        }
        return Ok(out);
    }
    let each = names.len().div_ceil(threads);
    let prefix = &prefix;
    std::thread::scope(|scope| {
        for (n, stamps) in out.chunks_mut(each).enumerate() {
            scope.spawn(move || {
                let mut path = Vec::with_capacity(prefix.len() + 256);
                for (at, stamp) in stamps.iter_mut().enumerate() {
                    *stamp = stamp_name(prefix, names.get(n * each + at), &mut path);
                }
            });
        }
    });
    Ok(out)
}

fn word<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at + N)?.try_into().ok()
}

/// One entry of a listing: its name's UTF-16 and its stamp; `None` where the bytes end first.
fn entry(bytes: &[u8]) -> Option<(&[u8], Stamp)> {
    let i64_at = |at| word::<8>(bytes, at).map(i64::from_le_bytes);
    let u32_at = |at| word::<4>(bytes, at).map(u32::from_le_bytes);
    let attributes = u32_at(56)?;
    let name_len = u32_at(60)? as usize;
    // A reparse point's tag is where its extended attributes' size would be (MS-FSCC 2.4.17).
    let tag = match attributes & FILE_ATTRIBUTE_REPARSE_POINT {
        0 => 0,
        _ => u32_at(64)?,
    };
    let name = bytes.get(104..104 + name_len)?;
    Some((name, Stamp {
        ino: i64_at(96)? as u64,
        size: i64_at(40)? as u64,
        mtime_ns: ns(i64_at(24)?),
        ctime_ns: ns(i64_at(32)?),
        mode: mode(attributes, tag),
    }))
}

/// What is directly in `dir` (named `name` under the root), each with its stamp, sorted by
/// name; links and junctions are stamped themselves.
pub(crate) fn entries(dir: &Path, name: &FileName) -> io::Result<Vec<(FileName, Stamp)>> {
    thread_local! {
        // alloc: once per thread, the buffer every listing on it fills.
        static BUFFER: std::cell::RefCell<Box<[u64]>> = std::cell::RefCell::new(vec![0u64; LISTING / 8].into_boxed_slice());
    }
    BUFFER.with_borrow_mut(|buffer| list(dir, name, buffer))
}

fn list(dir: &Path, name: &FileName, buffer: &mut [u64]) -> io::Result<Vec<(FileName, Stamp)>> {
    // alloc: an error path.
    let named = |error: io::Error| io::Error::new(error.kind(), format!("{}: {error}", dir.display()));
    let wide = sys::path(dir).map_err(named)?;
    let handle = open(&wide, FILE_LIST_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS).map_err(named)?;
    let mut class = FileIdBothDirectoryRestartInfo;
    // alloc: the entries the listing answers with, once per listing.
    let mut found = Vec::new();
    let mut text = String::with_capacity(name.as_str().len() + 256);
    loop {
        // SAFETY: an open directory handle and a buffer of the length given, 8-aligned as the
        // class's entries are.
        let ok = unsafe {
            GetFileInformationByHandleEx(handle.as_raw_handle(), class, buffer.as_mut_ptr().cast(), LISTING as u32)
        };
        if ok == 0 {
            return match sys::last() {
                ERROR_NO_MORE_FILES => {
                    found.sort_unstable_by(|a: &(FileName, Stamp), b| a.0.cmp(&b.0));
                    Ok(found)
                }
                _ => Err(named(io::Error::last_os_error())),
            };
        }
        class = FileIdBothDirectoryInfo;
        // SAFETY: the call filled the buffer, which is read as bytes while it is not written.
        let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), LISTING) };
        let mut at = 0;
        let malformed = || named(io::Error::new(io::ErrorKind::InvalidData, "an entry of the listing past its end"));
        loop {
            let rest = bytes.get(at..).ok_or_else(malformed)?;
            let (leaf, stamp) = entry(rest).ok_or_else(malformed)?;
            let next = word::<4>(rest, 0).map(u32::from_le_bytes).ok_or_else(malformed)? as usize;
            let units = leaf.chunks_exact(2).map(|it| u16::from_le_bytes([it[0], it[1]]));
            text.clear();
            text.push_str(name.as_str());
            text.push('/');
            let start = text.len();
            for unit in char::decode_utf16(units) {
                let Ok(unit) = unit else {
                    return Err(named(io::Error::new(io::ErrorKind::InvalidData, "a name that is not UTF-8")));
                };
                text.push(unit);
            }
            if !matches!(&text[start..], "." | "..") {
                let file = FileName::new(&text).ok_or_else(|| {
                    // alloc: an error path.
                    named(io::Error::new(io::ErrorKind::InvalidData, format!("{text}: not a name")))
                })?;
                found.push((file, stamp));
            }
            if next == 0 {
                break;
            }
            at += next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing_entry(next: u32, name: &str, attributes: u32, tag: u32, id: i64) -> Vec<u8> {
        let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut bytes = vec![0u8; 104];
        bytes[0..4].copy_from_slice(&next.to_le_bytes());
        bytes[24..32].copy_from_slice(&(EPOCH as i64 + 30).to_le_bytes());
        bytes[32..40].copy_from_slice(&(EPOCH as i64 + 40).to_le_bytes());
        bytes[40..48].copy_from_slice(&7i64.to_le_bytes());
        bytes[56..60].copy_from_slice(&attributes.to_le_bytes());
        bytes[60..64].copy_from_slice(&(wide.len() as u32).to_le_bytes());
        bytes[64..68].copy_from_slice(&tag.to_le_bytes());
        bytes[96..104].copy_from_slice(&id.to_le_bytes());
        bytes.extend(wide);
        bytes
    }

    #[test]
    fn an_entry_is_read_from_its_fields() {
        let bytes = listing_entry(0, "lib.rs", FILE_ATTRIBUTE_READONLY, 0, 42);
        let (name, stamp) = entry(&bytes).unwrap();
        assert_eq!(name, "lib.rs".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>());
        assert_eq!(stamp, Stamp { ino: 42, size: 7, mtime_ns: 3000, ctime_ns: 4000, mode: 0o100444 });
        assert_eq!(entry(&bytes[..bytes.len() - 1]), None);
    }

    #[test]
    fn a_reparse_point_is_a_link_only_where_it_names_another_file() {
        let symlink = 0xA000_000C;
        let cloud = 0x9000_001A;
        let link = listing_entry(0, "l", FILE_ATTRIBUTE_REPARSE_POINT, symlink, 1);
        assert!(entry(&link).unwrap().1.is_link());
        let junction = listing_entry(0, "j", FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY, 0xA000_0003, 1);
        assert!(entry(&junction).unwrap().1.is_link());
        let placeholder = listing_entry(0, "c", FILE_ATTRIBUTE_REPARSE_POINT, cloud, 1);
        assert!(entry(&placeholder).unwrap().1.is_file());
        // The size of extended attributes is not a tag where there is no reparse point.
        let plain = listing_entry(0, "p", 0, symlink, 1);
        assert!(entry(&plain).unwrap().1.is_file());
    }

    /// A sweep takes a thread per 2,048 names, never more than the cores and never none; the
    /// chunks the threads take cover every name once.
    #[test]
    fn a_sweep_takes_a_thread_per_chunk_up_to_the_cores() {
        assert_eq!(sweepers(0, 16), 1);
        assert_eq!(sweepers(CHUNK, 16), 1);
        assert_eq!(sweepers(CHUNK + 1, 16), 2);
        assert_eq!(sweepers(50_000, 16), 16);
        assert_eq!(sweepers(50_000, 64), 50_000usize.div_ceil(CHUNK));
        assert_eq!(sweepers(50_000, 0), 1);
        for names in [1, CHUNK, CHUNK + 1, 50_000, 1 << 20] {
            for cores in [1, 3, 8, 16, 64, 256] {
                let threads = sweepers(names, cores);
                let each = names.div_ceil(threads);
                assert!(threads <= cores.max(1) && (threads - 1) * each < names, "{names} {cores}");
            }
        }
    }

    #[test]
    fn names_windows_keeps_out_of_names_are_gone() {
        assert!(unnameable("a\\b") && unnameable("a:b") && unnameable("x?") && unnameable("t\u{1}"));
        assert!(!unnameable("src/lib.rs") && !unnameable("ünïcode.rs"));
    }
}
