//! A watch on Windows: `ReadDirectoryChangesW` of the whole tree under a directory, overlapped
//! on the port, asked again after each completion. A completion of no bytes, or of
//! `STATUS_NOTIFY_ENUM_DIR`, is the system's buffer overflowing: changes were lost.
//!
//! The buffer the system writes its records into is one of the driver's [`Buffers`], given
//! back when the watch ends; the records are read in place, their names made in strings kept
//! for the driver's life, so a change costs only what [`Done::Changed`] carries.

use std::{
    io,
    os::windows::io::{AsRawHandle as _, OwnedHandle},
    path::Path,
};

use windows_sys::Win32::{
    Storage::FileSystem::{
        CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_RENAMED_NEW_NAME, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
        FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION,
        FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
        FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileAttributesW, GetLongPathNameW, INVALID_FILE_ATTRIBUTES, OPEN_EXISTING,
        ReadDirectoryChangesW,
    },
    System::IO::OVERLAPPED,
};

use super::sys::{self, BUFFER, Buffer, Paths};
use crate::{Done, FileName};

const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_ATTRIBUTES
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION;

pub(super) struct Watching {
    handle: OwnedHandle,
    /// The watched directory, as the driver names it.
    dir: FileName,
    /// Where the system writes its records, the driver's until the watch ends.
    pub(super) buffer: Buffer,
    pub(super) stopping: bool,
}

/// The strings every watch's names are made in, kept for the driver's life.
#[derive(Default)]
pub(super) struct Scratch {
    name: String,
    /// Every name of a buffer's records, joined to the watched directory, one after another.
    joined: String,
    spans: Vec<Span>,
}

/// A record's name in [`Scratch::joined`], and whether it says the name came in (made, or
/// renamed to), so a directory it names is walked.
struct Span {
    start: u32,
    end: u32,
    came: bool,
}

impl Watching {
    /// A watch of `dir`, its records written into `buffer`.
    pub(super) fn new(paths: &mut Paths, dir: FileName, buffer: Buffer) -> io::Result<Watching> {
        let wide = paths.of(&dir)?;
        // SAFETY: a NUL-terminated path; no security attributes or template.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        let handle = sys::owned(handle).map_err(|error| {
            let path = paths.root().join(dir.as_str());
            // alloc: an error path.
            io::Error::new(error.kind(), format!("watching {}: {error}", path.display()))
        })?;
        Ok(Watching { handle, dir, buffer, stopping: false })
    }

    pub(super) fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.handle.as_raw_handle()
    }

    /// Asks for the next changes, into the buffer, completing on the port through
    /// `overlapped`. Both stay where they are until that completion is dequeued (the driver
    /// keeps the request, and the buffer with it, until then).
    ///
    /// # Safety
    ///
    /// `overlapped` is a request's [`super::sys::Head::overlapped`], made ready for this step,
    /// and the request's entry (its head and this watch with its buffer) stays where it is, alive, until the
    /// step's completion is dequeued: the kernel writes them after this returns, so freeing or
    /// moving either first is a write to freed memory.
    pub(super) unsafe fn read(&mut self, overlapped: *mut OVERLAPPED) -> io::Result<()> {
        // SAFETY: an open directory handle, a buffer of the length given (aligned to 64 KiB,
        // so to the `DWORD`s the call writes), and an `OVERLAPPED`, which outlive the request
        // (above).
        sys::check(unsafe {
            ReadDirectoryChangesW(
                self.raw(),
                self.buffer.ptr().cast(),
                BUFFER as u32,
                1,
                FILTER,
                std::ptr::null_mut(),
                overlapped,
                None,
            )
        })
    }

    /// What `got` bytes of records say changed. A directory made or moved in is walked, and
    /// what is in it named, as nothing told of it. The names are sorted and deduplicated in
    /// the scratch before any [`FileName`] is made, so a change costs its list and one name
    /// for each file it names.
    pub(super) fn take(&self, got: usize, paths: &mut Paths, scratch: &mut Scratch) -> Done {
        if got == 0 {
            // alloc: none, an empty `Vec`.
            return Done::Changed { files: Vec::new(), lost: true };
        }
        // SAFETY: the read's completion was taken, so the system writes the buffer no more; it
        // wrote `got` bytes of it, read here as bytes while the watch holds it.
        let bytes = unsafe { std::slice::from_raw_parts(self.buffer.ptr().cast_const(), got.min(BUFFER)) };
        let Scratch { name, joined, spans } = scratch;
        joined.clear();
        spans.clear();
        let dir = self.dir.as_str();
        let mut lost = parse(bytes, name, |action, name| {
            let start = joined.len() as u32;
            joined.push_str(dir);
            joined.push('/');
            joined.push_str(name);
            let came = matches!(action, FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME);
            // alloc: grown once to the most records a buffer holds, then kept.
            spans.push(Span { start, end: joined.len() as u32, came });
        });
        let text = |span: &Span| &joined[span.start as usize..span.end as usize];
        spans.sort_unstable_by(|a, b| text(a).cmp(text(b)));
        spans.dedup_by(|later, kept| {
            let same = text(later) == text(kept);
            kept.came |= same && later.came;
            same
        });
        // alloc: `Done::Changed`'s payload, once, as long as the names it can hold.
        let mut files = Vec::with_capacity(spans.len());
        for span in spans.iter() {
            let Some(named) = named(paths.root(), text(span)) else {
                lost = true;
                continue;
            };
            if span.came && is_dir(paths, &named) {
                let path = paths.root().join(named.as_str());
                // alloc: a directory come in, walked once, as std lists it.
                lost |= walk(&path, &named, &mut files).is_err();
            }
            files.push(named);
        }
        // What a walk added, or a short name made long, may be out of order.
        if files.windows(2).any(|pair| pair[0] >= pair[1]) {
            files.sort_unstable();
            files.dedup();
        }
        Done::Changed { files, lost }
    }
}

/// A record's name joined to the watched directory (`/`-separated) as the driver names it; a
/// short (8.3) name is made long where the file is still there to ask.
fn named(root: &Path, joined: &str) -> Option<FileName> {
    if !joined.contains('~') {
        return FileName::new(joined);
    }
    // alloc: a short name, made long by asking the system: rare, as 8.3 names are.
    let long = long_name(&root.join(joined)).and_then(|long| {
        // alloc: as above.
        let long = long.strip_prefix(root).ok()?.to_str()?.replace('\\', "/");
        FileName::new(&long)
    });
    long.or_else(|| FileName::new(joined))
}

/// Whether `name` is a directory, followed where it is a link, as `Path::is_dir` says.
fn is_dir(paths: &mut Paths, name: &FileName) -> bool {
    let Ok(wide) = paths.of(name) else { return false };
    // SAFETY: a NUL-terminated path.
    let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        return false;
    }
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        // alloc: a link come in, followed as std follows it.
        return paths.root().join(name.as_str()).is_dir();
    }
    attributes & FILE_ATTRIBUTE_DIRECTORY != 0
}

/// Each record of a buffer: its action and its name, `\` made `/` (into `name`, which is
/// reused), to `each`. Whether changes were lost: where a record runs past the end or its
/// name is not Unicode, the rest is.
fn parse(bytes: &[u8], name: &mut String, mut each: impl FnMut(u32, &str)) -> bool {
    let mut at = 0usize;
    let u32_at = |at: usize| bytes.get(at..at + 4).map(|it| u32::from_ne_bytes(it.try_into().unwrap()));
    loop {
        let (Some(next), Some(action), Some(len)) = (u32_at(at), u32_at(at + 4), u32_at(at + 8))
        else {
            return true;
        };
        let Some(wide) = bytes.get(at + 12..at + 12 + len as usize) else {
            return true;
        };
        name.clear();
        let units = wide.chunks_exact(2).map(|it| u16::from_ne_bytes([it[0], it[1]]));
        for unit in char::decode_utf16(units) {
            match unit {
                Ok('\\') => name.push('/'),
                Ok(unit) => name.push(unit),
                Err(_) => return true,
            }
        }
        each(action, name);
        if next == 0 {
            return false;
        }
        at += next as usize;
    }
}

/// The long form of `path`, which exists.
fn long_name(path: &Path) -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    let wide = sys::wide(path.as_os_str()).ok()?;
    // alloc: a short name made long, rare (above).
    let mut long = vec![0u16; 1024];
    // SAFETY: a NUL-terminated path and a buffer of the length given.
    let len = unsafe { GetLongPathNameW(wide.as_ptr(), long.as_mut_ptr(), long.len() as u32) };
    let len = len as usize;
    if len == 0 || len >= long.len() {
        return None;
    }
    Some(std::ffi::OsString::from_wide(&long[..len]).into())
}

/// Every file and directory under `path` (named `name`) into `files`.
fn walk(path: &Path, name: &FileName, files: &mut Vec<FileName>) -> io::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        // alloc: a directory come in, walked once (`take`).
        let Some(file) = entry.file_name().to_str().and_then(|it| FileName::new(&format!("{name}/{it}")))
        else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a name that is not Unicode"));
        };
        if entry.file_type()?.is_dir() {
            walk(&entry.path(), &file, files)?;
        }
        files.push(file);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_ne_bytes).collect();
        let mut bytes = Vec::new();
        bytes.extend(next.to_ne_bytes());
        bytes.extend(action.to_ne_bytes());
        bytes.extend((wide.len() as u32).to_ne_bytes());
        bytes.extend(wide);
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        bytes
    }

    fn records(bytes: &[u8]) -> (Vec<(u32, String)>, bool) {
        let mut records = Vec::new();
        let lost = parse(bytes, &mut String::new(), |action, name| records.push((action, name.to_owned())));
        (records, lost)
    }

    #[test]
    fn records_are_read_to_their_end() {
        let first = record(0, FILE_ACTION_ADDED, "deep\\lib.rs");
        let mut bytes = record(0, 3, "keep");
        let next = bytes.len() as u32;
        bytes.splice(0..4, next.to_ne_bytes());
        bytes.extend(first);
        let (records, lost) = records(&bytes);
        assert!(!lost);
        assert_eq!(records, [(3, "keep".to_owned()), (FILE_ACTION_ADDED, "deep/lib.rs".to_owned())]);
    }

    #[test]
    fn a_record_past_the_end_is_lost() {
        let bytes = record(0, FILE_ACTION_ADDED, "keep");
        let (records, lost) = records(&bytes[..bytes.len() - 6]);
        assert!(lost && records.is_empty(), "{records:?}");
        let (_, lost) = self::records(&bytes[..6]);
        assert!(lost);
    }

    #[test]
    fn a_name_that_is_not_unicode_loses_the_rest() {
        let mut bytes = record(0, 3, "keep");
        let next = bytes.len() as u32;
        bytes.splice(0..4, next.to_ne_bytes());
        let mut bad = record(0, 3, "x");
        bad[12..14].copy_from_slice(&0xD800u16.to_ne_bytes());
        bytes.extend(bad);
        let (records, lost) = records(&bytes);
        assert!(lost);
        assert_eq!(records, [(3, "keep".to_owned())]);
    }

    /// Reading a buffer's records into strings that have grown to them allocates nothing.
    #[test]
    fn records_are_read_in_place() {
        let bytes = record(0, FILE_ACTION_ADDED, "deep\\lib.rs");
        let mut name = String::with_capacity(64);
        let mut seen = 0;
        blaze_alloc_count::assert_at_most(0, "parsing a watch's records", || {
            for _ in 0..1000 {
                assert!(!parse(&bytes, &mut name, |_, it| seen += it.len()));
            }
        });
        assert_eq!(seen, 1000 * "deep/lib.rs".len());
    }
}
