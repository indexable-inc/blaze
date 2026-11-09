//! The NTFS change journal behind [`crate::Effect::Journal`] on Windows
//! (docs/v2/plan/design/restore.md, docs/v2/plan/design/restore.md).
//!
//! The watched directory's volume is opened for traversal only, which any user may, and read
//! with `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` (Windows 10 1709 on), overlapped on the port: from
//! the saved USN with nothing to wait for until the read reaches the `NextUsn` the watch began
//! at (its history), then waiting for one byte (live), so every change, then and after, is the
//! journal's and every batch's cursor is the USN after its last record. `FSCTL_READ_USN_JOURNAL`
//! would give names too, but it is refused to anyone outside Administrators.
//!
//! The unprivileged read gives a record without its name: the file's and its parent's
//! reference numbers, its attributes and why it changed. [`Names`] makes names of them, keeping
//! where each directory it has met is: a file made, written or come in under a new name is
//! named where it is now (`OpenFileById`, `GetFinalPathNameByHandleW`); one removed, or the old
//! name of a rename, is told as its directory's names changed ([`Lost::Unnamed`]). A record
//! that has its name (an elevated reader's) is named from its parent with no call at all.
//! Records whose parent is outside the watched directory cost one lookup in a map.
//!
//! A flush of the watch ([`crate::Effect::Flush`]) is marked with the journal's `NextUsn`,
//! queried after it was asked ([`mark`]), and told once every record before that is told
//! ([`crate::journal::barrier`]): nothing is written for it. While a flush waits, reads are
//! asked without waiting, so one returns at the journal's end even when no record comes.

use std::{
    fmt, io,
    os::windows::io::{AsRawHandle as _, OwnedHandle},
    path::Path,
    sync::Arc,
};

use blaze_hash::{HashMap, HashSet};
use std::alloc::Global;
use windows_sys::Win32::{
    Foundation::{
        ERROR_ACCESS_DENIED, ERROR_CANT_ACCESS_FILE, ERROR_DELETE_PENDING, ERROR_FILE_NOT_FOUND,
        ERROR_HANDLE_EOF, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER,
        ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE,
        ERROR_PATH_NOT_FOUND, HANDLE,
    },
    Storage::FileSystem::{
        CreateFileW, ExtendedFileIdType, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_ID_128, FILE_ID_DESCRIPTOR,
        FILE_ID_DESCRIPTOR_0, FILE_ID_INFO, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FileIdInfo,
        GetFileInformationByHandleEx, GetFinalPathNameByHandleW, GetVolumeInformationByHandleW,
        OPEN_EXISTING, OpenFileById, VOLUME_NAME_DOS, VOLUME_NAME_GUID, VOLUME_NAME_NT,
    },
    System::{
        IO::{DeviceIoControl, OVERLAPPED},
        Ioctl::{
            FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_UNPRIVILEGED_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1,
            USN_JOURNAL_DATA_V1, USN_REASON_BASIC_INFO_CHANGE, USN_REASON_CLOSE,
            USN_REASON_COMPRESSION_CHANGE, USN_REASON_DATA_EXTEND, USN_REASON_DATA_OVERWRITE,
            USN_REASON_DATA_TRUNCATION, USN_REASON_EA_CHANGE, USN_REASON_ENCRYPTION_CHANGE,
            USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_HARD_LINK_CHANGE,
            USN_REASON_INDEXABLE_CHANGE, USN_REASON_INTEGRITY_CHANGE, USN_REASON_NAMED_DATA_EXTEND,
            USN_REASON_NAMED_DATA_OVERWRITE, USN_REASON_NAMED_DATA_TRUNCATION,
            USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME,
            USN_REASON_REPARSE_POINT_CHANGE, USN_REASON_SECURITY_CHANGE, USN_REASON_STREAM_CHANGE,
            USN_RECORD_V2, USN_RECORD_V3,
        },
    },
};

use super::sys;
use crate::{Cursor, FileName, Journaled, Lost, journal::barrier::At};

/// What one read of the journal fills at most.
const BUFFER: usize = 256 << 10;
/// Names a batch of the history gathers before it is told.
const GATHER: usize = 4096;
/// How far the journal may move past the last batch told before a live read of nothing under
/// the watched directory is told anyway, for its cursor: what a restart reads again, and what
/// the journal must still keep for it.
const ADVANCE: i64 = 1 << 20;

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const BACKSLASH: u16 = b'\\' as u16;

/// Every reason that changes a file's data, names, attributes, security or links. Not the
/// object ID, the storage class or a close alone; a close record carries the reasons it closes,
/// so the last write of a file held open is told by it.
const REASONS: u32 = USN_REASON_DATA_OVERWRITE
    | USN_REASON_DATA_EXTEND
    | USN_REASON_DATA_TRUNCATION
    | USN_REASON_NAMED_DATA_OVERWRITE
    | USN_REASON_NAMED_DATA_EXTEND
    | USN_REASON_NAMED_DATA_TRUNCATION
    | USN_REASON_FILE_CREATE
    | USN_REASON_FILE_DELETE
    | USN_REASON_EA_CHANGE
    | USN_REASON_SECURITY_CHANGE
    | USN_REASON_RENAME_OLD_NAME
    | USN_REASON_RENAME_NEW_NAME
    | USN_REASON_INDEXABLE_CHANGE
    | USN_REASON_BASIC_INFO_CHANGE
    | USN_REASON_HARD_LINK_CHANGE
    | USN_REASON_COMPRESSION_CHANGE
    | USN_REASON_ENCRYPTION_CHANGE
    | USN_REASON_REPARSE_POINT_CHANGE
    | USN_REASON_STREAM_CHANGE
    | USN_REASON_INTEGRITY_CHANGE;

// The records and requests as winioctl.h lays them out; records are read field by field from
// the read's bytes at these offsets.
const _: () = {
    use std::mem::offset_of;
    assert!(size_of::<READ_USN_JOURNAL_DATA_V1>() == 48);
    assert!(size_of::<USN_JOURNAL_DATA_V1>() == 64);
    assert!(size_of::<FILE_ID_INFO>() == 24);
    assert!(offset_of!(USN_RECORD_V2, MajorVersion) == 4);
    assert!(offset_of!(USN_RECORD_V2, FileReferenceNumber) == 8);
    assert!(offset_of!(USN_RECORD_V2, ParentFileReferenceNumber) == 16);
    assert!(offset_of!(USN_RECORD_V2, Reason) == 40);
    assert!(offset_of!(USN_RECORD_V2, FileAttributes) == 52);
    assert!(offset_of!(USN_RECORD_V2, FileNameLength) == 56);
    assert!(offset_of!(USN_RECORD_V2, FileNameOffset) == 58);
    assert!(offset_of!(USN_RECORD_V3, FileReferenceNumber) == 8);
    assert!(offset_of!(USN_RECORD_V3, ParentFileReferenceNumber) == 24);
    assert!(offset_of!(USN_RECORD_V3, Reason) == 56);
    assert!(offset_of!(USN_RECORD_V3, FileAttributes) == 68);
    assert!(offset_of!(USN_RECORD_V3, FileNameLength) == 72);
    assert!(offset_of!(USN_RECORD_V3, FileNameOffset) == 74);
};

/// One record of a read, as far as names are made of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Record {
    /// The file's reference number: a version 2 record's 64 bits, or a version 3 record's 128
    /// (ReFS), as `FILE_ID_128`'s bytes read little-endian, which on NTFS are the same number.
    pub(super) id: u128,
    pub(super) parent: u128,
    pub(super) reason: u32,
    pub(super) attributes: u32,
    /// Where its name's UTF-16 is in the read's bytes, and how many bytes: none where the
    /// read gives no names.
    pub(super) name: (u32, u32),
}

fn word<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

/// The USN the next read starts at, and each record of a read's bytes into `records`; why not,
/// where a record runs past the end or is of a version not asked for.
pub(super) fn parse(bytes: &[u8], records: &mut Vec<Record>) -> Result<i64, String> {
    let next = word::<8>(bytes, 0).map(i64::from_le_bytes).ok_or("a read of under 8 bytes")?;
    let mut at = 8;
    while at < bytes.len() {
        let rest = &bytes[at..];
        // alloc: an error path.
        let bad = |what: &str| format!("the record at byte {at} of the read {what}");
        let len =
            word::<4>(rest, 0).map(u32::from_le_bytes).ok_or_else(|| bad("is cut short"))? as usize;
        let major =
            word::<2>(rest, 4).map(u16::from_le_bytes).ok_or_else(|| bad("is cut short"))?;
        let record = rest.get(..len).ok_or_else(|| bad("runs past its end"))?;
        let u64_at = |at| word::<8>(record, at).map(u64::from_le_bytes);
        let u128_at = |at| word::<16>(record, at).map(u128::from_le_bytes);
        let (id, parent, fields) = match major {
            2 => (u64_at(8).map(u128::from), u64_at(16).map(u128::from), 40),
            3 => (u128_at(8), u128_at(24), 56),
            // alloc: an error path.
            _ => {
                return Err(bad(&format!(
                    "is of version {major}; versions 2 and 3 were asked for"
                )));
            }
        };
        let u32_at = |at| word::<4>(record, at).map(u32::from_le_bytes);
        let u16_at = |at| word::<2>(record, at).map(u16::from_le_bytes);
        let (Some(id), Some(parent), Some(reason), Some(attributes), Some(name_len), Some(name_at)) = (
            id,
            parent,
            u32_at(fields),
            u32_at(fields + 12),
            u16_at(fields + 16),
            u16_at(fields + 18),
        ) else {
            return Err(bad("is shorter than its fields"));
        };
        let (name_at, name_len) = (usize::from(name_at), usize::from(name_len));
        if name_len % 2 != 0 || (name_len > 0 && name_at + name_len > len) {
            return Err(bad("has a name past its end"));
        }
        records.push(Record {
            id,
            parent,
            reason,
            attributes,
            name: ((at + name_at) as u32, name_len as u32),
        });
        at += len;
    }
    Ok(next)
}

/// What a file or directory is found to be, by its reference number, now.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Found {
    /// The watched directory or under it: its name under the driver's root.
    Under(FileName),
    /// Under the watched directory below a name that is not UTF-8: the directory that name is in.
    Unnameable(FileName),
    /// Not under the watched directory, or not one this user may open, which is not one blaze
    /// builds from.
    Outside,
    /// Not there any more.
    Gone,
    /// The system would not say (an error other than its not being there or being refused).
    Unknown,
}

/// What [`Names`] asks of the volume.
pub(super) trait System {
    fn find(&mut self, id: u128) -> Found;
    /// Whether the watched directory is no longer where the watch found it.
    fn root_moved(&mut self) -> bool;
}

/// Where a directory met is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    /// The watched directory or under it: its name, in [`Names::paths`].
    Under(u32),
    /// Under a name that is not UTF-8: the directory that name is in, in [`Names::paths`].
    Unnameable(u32),
    Outside,
    Gone,
}

/// What a record's reasons say of its names.
#[derive(Clone, Copy)]
struct How {
    /// Removed, or the old name of a rename: where it was named is not it any more.
    gone: bool,
    /// The new name of a rename.
    arrived: bool,
    /// The names in its directory changed: a removal, a rename's old name, a link made or
    /// removed.
    names: bool,
}

impl How {
    /// A close record carries every reason of the handle it closes, each of which was told by a
    /// record of its own as it happened; only a removal, made at the close, is its own.
    fn of(reason: u32) -> How {
        let closing = reason & USN_REASON_CLOSE != 0;
        let deleted = reason & USN_REASON_FILE_DELETE != 0;
        let gone = deleted || (!closing && reason & USN_REASON_RENAME_OLD_NAME != 0);
        let arrived = !closing && !deleted && reason & USN_REASON_RENAME_NEW_NAME != 0;
        let names = gone || (!closing && reason & USN_REASON_HARD_LINK_CHANGE != 0);
        How { gone, arrived, names }
    }
}

/// Makes names of records: the directories met (by reference number) and where they are, and
/// the names and losses of the batch being gathered.
pub(super) struct Names {
    dir: FileName,
    root: u128,
    dirs: HashMap<u128, Place, Global>,
    /// The names [`Place`]s point at; the watched directory's is the first.
    paths: Vec<FileName>,
    /// Files of this read already named where they are now.
    seen: HashSet<u128, Global>,
    files: Vec<FileName>,
    lost: Vec<(FileName, Lost)>,
    /// The watched directory itself moved or went: the watch ends after this read.
    pub(super) root_changed: bool,
    text: String,
}

impl Names {
    pub(super) fn new(dir: FileName, root: u128) -> Names {
        let mut dirs = HashMap::new_in(Global);
        dirs.insert(root, Place::Under(0));
        // alloc: setup, once per watch; the buffers are reused for its life.
        Names {
            // clone: setup.
            paths: vec![dir.clone()],
            dir,
            root,
            dirs,
            seen: HashSet::new_in(Global),
            // alloc: setup.
            files: Vec::new(),
            lost: Vec::new(), // alloc: setup.
            root_changed: false,
            // alloc: setup.
            text: String::new(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.files.is_empty() && self.lost.is_empty()
    }

    pub(super) fn gathered(&self) -> usize {
        self.files.len() + self.lost.len()
    }

    /// The watched directory, as a whole, `lost`.
    pub(super) fn lose_all(&mut self, lost: Lost) {
        // clone: a name the batch owns, once per loss of the whole watch.
        self.lost.push((self.dir.clone(), lost));
    }

    /// What was gathered, each once, sorted, and the gathering begun again.
    pub(super) fn drain(&mut self) -> (Vec<FileName>, Vec<(FileName, Lost)>) {
        let (mut files, mut lost) =
            (std::mem::take(&mut self.files), std::mem::take(&mut self.lost));
        files.sort_unstable();
        files.dedup();
        lost.sort_unstable_by(|a, b| a.0.cmp(&b.0).then((a.1 as u8).cmp(&(b.1 as u8))));
        lost.dedup();
        (files, lost)
    }

    /// The end of a read: what was named of it forgotten, and the names of directories no
    /// longer met let go once they are most of them.
    pub(super) fn read_done(&mut self) {
        self.seen.clear();
        if self.paths.len() > 64 + 4 * self.dirs.len() {
            // clone: amortized, once the names outgrow the directories four times over.
            let mut paths = vec![self.dir.clone()];
            for place in self.dirs.values_mut() {
                if let Place::Under(at) | Place::Unnameable(at) = place {
                    if *at != 0 {
                        // clone: as above.
                        paths.push(self.paths[*at as usize].clone());
                        *at = (paths.len() - 1) as u32;
                    }
                }
            }
            self.paths = paths;
        }
    }

    fn intern(&mut self, name: FileName) -> u32 {
        self.paths.push(name);
        (self.paths.len() - 1) as u32
    }

    fn place(&mut self, found: Found) -> Place {
        match found {
            Found::Under(name) => Place::Under(self.intern(name)),
            Found::Unnameable(name) => Place::Unnameable(self.intern(name)),
            Found::Outside => Place::Outside,
            Found::Gone => Place::Gone,
            // Nothing under the watched directory can be ruled out.
            Found::Unknown => Place::Unnameable(0),
        }
    }

    fn lose(&mut self, at: u32, lost: Lost) {
        // clone: a name the batch owns, as `Journaled::lost` holds them.
        self.lost.push((self.paths[at as usize].clone(), lost));
    }

    fn root_changed(&mut self) {
        if !self.root_changed {
            self.root_changed = true;
            self.lose_all(Lost::RootChanged);
        }
    }

    /// Where the directory `id` is: as met before, or asked once and kept.
    fn dir_place(&mut self, id: u128, system: &mut impl System) -> Place {
        if let Some(&place) = self.dirs.get(&id) {
            return place;
        }
        let found = system.find(id);
        let keep = !matches!(found, Found::Gone | Found::Unknown);
        let place = self.place(found);
        if keep {
            self.dirs.insert(id, place);
        }
        place
    }

    /// The directory at `at`, a `/`, and `name` (UTF-16, little-endian), if that is a name.
    fn join(&mut self, at: u32, name: &[u8]) -> Option<FileName> {
        self.text.clear();
        self.text.push_str(self.paths[at as usize].as_str());
        self.text.push('/');
        let units = name.chunks_exact(2).map(|it| u16::from_le_bytes([it[0], it[1]]));
        for unit in char::decode_utf16(units) {
            match unit {
                Ok('/' | '\\') | Err(_) => return None,
                Ok(unit) => self.text.push(unit),
            }
        }
        FileName::new(&self.text)
    }

    /// Every directory known at `path` or under it forgotten.
    fn forget_below(&mut self, path: &FileName) {
        let paths = &self.paths;
        self.dirs.retain(|_, place| match place {
            Place::Under(at) | Place::Unnameable(at) => {
                *at == 0 || !paths[*at as usize].is_under(path)
            }
            Place::Outside | Place::Gone => true,
        });
    }

    /// Every directory known under the watched one forgotten, but it.
    fn forget_all(&mut self) {
        self.dirs.retain(|_, place| matches!(place, Place::Outside | Place::Gone));
        self.dirs.insert(self.root, Place::Under(0));
        self.paths.truncate(1);
    }

    /// One record, with its name's bytes where the read gave it one.
    pub(super) fn record(
        &mut self,
        record: &Record,
        name: Option<&[u8]>,
        system: &mut impl System,
    ) {
        let is_dir = record.attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        let how = How::of(record.reason);
        if record.id == self.root {
            match how.gone || how.arrived {
                true => self.root_changed(),
                // clone: a name the batch owns, as `Journaled::files` holds them.
                false => self.files.push(self.dir.clone()),
            }
            return;
        }
        let parent = self.dir_place(record.parent, system);
        let named = match (parent, name) {
            (Place::Under(at), Some(name)) => match self.join(at, name) {
                Some(named) => Some(named),
                None => return self.lose(at, Lost::Rescan),
            },
            _ => None,
        };
        if is_dir && (how.gone || how.arrived) {
            // A directory known is named where it was, wherever its parent is now.
            if let Some(was) = self.dir_moved(record, how, parent, named.as_ref(), system) {
                self.files.push(was);
            }
        }
        match parent {
            Place::Gone => {}
            Place::Outside => {
                if is_dir && how.arrived {
                    self.dirs.insert(record.id, Place::Outside);
                }
            }
            Place::Unnameable(at) => self.lose(at, Lost::Rescan),
            Place::Under(at) => match named {
                Some(named) => {
                    if is_dir
                        && !how.gone
                        && (how.arrived || record.reason & USN_REASON_FILE_CREATE != 0)
                    {
                        // clone: one for the map of directories, one for the batch.
                        let at = self.intern(named.clone());
                        self.dirs.insert(record.id, Place::Under(at));
                    }
                    self.files.push(named);
                }
                None => self.unnamed(record, at, is_dir, how, system),
            },
        }
    }

    /// A directory renamed or removed: what is known of where directories are made true again.
    /// Where it was, where that was known.
    fn dir_moved(
        &mut self,
        record: &Record,
        how: How,
        parent: Place,
        named: Option<&FileName>,
        system: &mut impl System,
    ) -> Option<FileName> {
        self.seen.clear();
        let known = match self.dirs.remove(&record.id) {
            // clone: a name the batch owns, once per directory renamed or removed.
            Some(Place::Under(at)) => Some(self.paths[at as usize].clone()),
            _ => None,
        };
        if how.arrived {
            // Come in from elsewhere: what was under it outside may be under the watched one now.
            if parent != Place::Outside {
                self.dirs.retain(|_, it| *it != Place::Outside);
            }
        } else if let Some(path) = known.as_ref().or(named) {
            self.forget_below(path);
        } else if parent != Place::Outside && record.reason & USN_REASON_FILE_DELETE == 0 {
            // A directory not met moved away from under the watched one: any directory known
            // may have been under it. (One removed was empty: what was under it went first.)
            self.forget_all();
        }
        // One of the watched directory's own may have moved, taking it along.
        if known.is_none() && system.root_moved() {
            self.root_changed();
        }
        known
    }

    /// A record with no name, in the directory at `at`.
    fn unnamed(
        &mut self,
        record: &Record,
        at: u32,
        is_dir: bool,
        how: How,
        system: &mut impl System,
    ) {
        if how.names {
            self.lose(at, Lost::Unnamed);
        }
        if how.gone {
            return;
        }
        // Made, written, or come in under a new name: named where it is now.
        let found = match is_dir {
            true => match self.dirs.get(&record.id) {
                // clone: a name the batch owns.
                Some(&Place::Under(it)) if !how.arrived => {
                    Found::Under(self.paths[it as usize].clone())
                }
                _ => system.find(record.id),
            },
            false if !self.seen.insert(record.id) => return,
            false => system.find(record.id),
        };
        match found {
            Found::Under(name) => {
                if is_dir {
                    // clone: one for the map of directories, one for the batch.
                    let it = self.intern(name.clone());
                    self.dirs.insert(record.id, Place::Under(it));
                }
                self.files.push(name);
            }
            Found::Unnameable(dir) => self.lost.push((dir, Lost::Rescan)),
            // Moved out since (its rename tells it), or a link whose name found is elsewhere,
            // or not to be asked: the directory's entries say what it is under that name.
            Found::Outside | Found::Unknown => self.lose(at, Lost::Unnamed),
            // Removed since: its removal tells it.
            Found::Gone => {}
        }
    }
}

/// What the watch asks of the volume: names of reference numbers, through a handle of the
/// watched directory, which every `OpenFileById` is relative to and which follows the
/// directory wherever it moves.
struct Volume {
    hint: OwnedHandle,
    /// The watched directory's NT path (`\Device\HarddiskVolume3\...`, normalized, no NUL),
    /// which every path found is compared with. No mount point is looked up to make it.
    root: Vec<u16>,
    dir: FileName,
    scratch: Vec<u16>,
    text: String,
}

impl Volume {
    /// What the path in `scratch` is, against the watched directory's.
    fn relative(&mut self) -> Found {
        let Some(rest) = self.scratch.strip_prefix(&self.root[..]) else { return Found::Outside };
        let rest = match rest {
            // clone: a name the batch owns.
            [] => return Found::Under(self.dir.clone()),
            [first, rest @ ..] if *first == BACKSLASH => rest,
            _ => return Found::Outside,
        };
        let text = &mut self.text;
        text.clear();
        text.push_str(self.dir.as_str());
        for part in rest.split(|&it| it == BACKSLASH) {
            let len = text.len();
            text.push('/');
            for unit in char::decode_utf16(part.iter().copied()) {
                match unit {
                    Ok(unit) => text.push(unit),
                    Err(_) => {
                        text.truncate(len);
                        return FileName::new(text).map_or(Found::Unknown, Found::Unnameable);
                    }
                }
            }
        }
        FileName::new(text).map_or(Found::Unknown, Found::Under)
    }
}

impl System for Volume {
    fn find(&mut self, id: u128) -> Found {
        let descriptor = FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: ExtendedFileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 {
                ExtendedFileId: FILE_ID_128 { Identifier: id.to_le_bytes() },
            },
        };
        // SAFETY: an open handle on the volume, a descriptor of the size it says, and no
        // security attributes. No access is asked: a name needs none.
        let opened = sys::owned(unsafe {
            OpenFileById(
                self.hint.as_raw_handle(),
                &descriptor,
                0,
                SHARE_ALL,
                std::ptr::null(),
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            )
        });
        let handle = match opened {
            Ok(handle) => handle,
            Err(error) => {
                return match error.raw_os_error().map(|it| it as u32) {
                    Some(
                        ERROR_INVALID_PARAMETER
                        | ERROR_FILE_NOT_FOUND
                        | ERROR_PATH_NOT_FOUND
                        | ERROR_DELETE_PENDING,
                    ) => Found::Gone,
                    Some(ERROR_ACCESS_DENIED | ERROR_CANT_ACCESS_FILE) => Found::Outside,
                    _ => Found::Unknown,
                };
            }
        };
        match final_path(&handle, FILE_NAME_NORMALIZED | VOLUME_NAME_NT, &mut self.scratch) {
            Ok(()) => self.relative(),
            Err(_) => Found::Unknown,
        }
    }

    fn root_moved(&mut self) -> bool {
        match final_path(&self.hint, FILE_NAME_NORMALIZED | VOLUME_NAME_NT, &mut self.scratch) {
            Ok(()) => self.scratch != self.root,
            Err(_) => true,
        }
    }
}

/// The path of what `handle` is open on, as `flags` asks, into `into` (no NUL).
fn final_path(handle: &OwnedHandle, flags: u32, into: &mut Vec<u16>) -> io::Result<()> {
    loop {
        let room = into.capacity().max(260);
        into.resize(room, 0);
        // SAFETY: an open handle and a buffer of the length given.
        let len = unsafe {
            GetFinalPathNameByHandleW(handle.as_raw_handle(), into.as_mut_ptr(), room as u32, flags)
        };
        let len = len as usize;
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        if len < room {
            into.truncate(len);
            return Ok(());
        }
        // Too short: `len` is the room it needs, its NUL too.
        into.reserve(len - into.len() + 1);
    }
}

/// The volume's serial number and the 128-bit reference number of what `handle` is open on.
fn file_id(handle: &OwnedHandle) -> io::Result<(u64, u128)> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: an open handle and room for the `FILE_ID_INFO` the class asks for; plain integers,
    // valid for any bytes written.
    sys::check(unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    })?;
    Ok((info.VolumeSerialNumber, u128::from_le_bytes(info.FileId.Identifier)))
}

/// The file system's name and flags of the volume `handle` is on.
fn volume_info(handle: &OwnedHandle) -> io::Result<(String, u32)> {
    let mut name = [0u16; 64];
    let mut flags = 0;
    // SAFETY: an open handle, a buffer of the length given and a flags word to write; the rest
    // is not asked for.
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
    // alloc: setup, once per watch.
    Ok((String::from_utf16_lossy(&name[..len]), flags))
}

fn open(wide: &[u16], rights: u32, flags: u32) -> io::Result<OwnedHandle> {
    debug_assert_eq!(wide.last(), Some(&0));
    // SAFETY: a NUL-terminated path; no security attributes or template.
    sys::owned(unsafe {
        CreateFileW(
            wide.as_ptr(),
            rights,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    })
}

/// The journal's state, through a synchronous handle of the volume.
fn query(handle: &OwnedHandle) -> io::Result<USN_JOURNAL_DATA_V1> {
    let mut data = USN_JOURNAL_DATA_V1::default();
    let mut returned = 0;
    // SAFETY: an open handle that is not overlapped, so the call is synchronous, and room for
    // the `USN_JOURNAL_DATA_V1` it writes; plain integers, valid for any bytes written.
    sys::check(unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            (&raw mut data).cast(),
            size_of::<USN_JOURNAL_DATA_V1>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    })?;
    Ok(data)
}

/// Where the volume's journal ends now, through a synchronous handle of it: a flush's mark.
/// Any user may ask (the handle is opened for traversal only, as [`Journal::open`] opens it).
pub(super) fn mark(querier: &OwnedHandle) -> io::Result<At> {
    query(querier).map(|data| At { journal: data.UsnJournalID, usn: data.NextUsn })
}

/// What one read and its records came to: a batch to tell, the watch's end with why, and,
/// unless it ended, how far it has told.
pub(super) struct Taken {
    pub(super) batch: Option<Box<Journaled>>,
    pub(super) end: Option<String>,
    pub(super) read: Option<Read>,
}

/// How far a watch has told, after a read and its batch: what a flush's mark is compared with.
#[derive(Clone, Copy)]
pub(super) struct Read {
    /// Every record before it is told: the next USN to read once nothing gathered waits to
    /// be told, else the cursor of the last batch.
    pub(super) told: At,
    /// The read was asked without waiting and moved no further.
    pub(super) stalled: bool,
}

/// A watch of the journal: the volume's handles, where the read is, and the names it makes.
pub(super) struct Journal {
    names: Names,
    volume: Volume,
    /// The volume, overlapped on the port: the reads.
    reader: OwnedHandle,
    /// The volume, synchronous: the queries, and a flush's mark, which the driver asks on its
    /// threads while a read holds the journal.
    querier: Arc<OwnedHandle>,
    /// The watched directory as the system names it, for errors.
    shown: String,
    serial: u64,
    journal: u64,
    /// The USN the next read starts at.
    next: i64,
    /// The `NextUsn` the watch began at: its history ends there.
    target: i64,
    caught_up: bool,
    /// A flush waits: the next read is asked without waiting.
    hurry: bool,
    /// The cursor's USN of the last batch told.
    told: i64,
    /// The request the kernel reads while a read is in flight (`METHOD_NEITHER`: in place), in
    /// the boxed journal, which stays where it is while the request holds it.
    input: READ_USN_JOURNAL_DATA_V1,
    /// Where the kernel writes records: `u64`s, as they are 8-aligned. The watch's own, made
    /// once as it starts (256 KiB, four of the driver's buffers, which a watch that lives as
    /// long as the daemon would only hold the same way).
    buffer: Box<[u64]>,
    records: Vec<Record>,
}

impl Journal {
    /// A watch of `dir`'s volume's journal from `since`, and the batch to tell at once where it
    /// starts now (none where it reads history first).
    pub(super) fn open(
        root: &Path,
        dir: &FileName,
        since: Cursor,
    ) -> io::Result<(Box<Journal>, Option<Box<Journaled>>)> {
        // Setup, once per watch: what is allocated here is the watch's for its life, and every
        // error names the directory.
        let path = root.join(dir.as_str());
        let context = |what: &str, error: io::Error| {
            // alloc: an error path.
            io::Error::new(
                error.kind(),
                format!("the USN journal of {}: {what}: {error}", path.display()),
            )
        };
        let refuse = |why: String| io::Error::new(io::ErrorKind::Unsupported, why);
        let wide = sys::path(&path)?;
        let hint = open(&wide, FILE_READ_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS)
            .map_err(|it| context("opening it", it))?;
        let (serial, root_id) = file_id(&hint).map_err(|it| context("its file ID", it))?;
        // alloc: setup, the watch's scratch for every path it asks for.
        let mut scratch = Vec::with_capacity(512);
        final_path(&hint, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS, &mut scratch)
            .map_err(|it| context("its path", it))?;
        let shown = String::from_utf16_lossy(&scratch); // alloc: setup.
        // alloc: setup.
        let shown = shown.strip_prefix(r"\\?\").unwrap_or(&shown).to_owned();
        let mut root_nt = Vec::with_capacity(scratch.len() + 16);
        final_path(&hint, FILE_NAME_NORMALIZED | VOLUME_NAME_NT, &mut root_nt)
            .map_err(|it| context("its path", it))?;
        let share = r"\Device\Mup\";
        if share.encode_utf16().eq(root_nt.iter().copied().take(share.len())) {
            // alloc: an error path.
            return Err(refuse(format!(
                "{shown} is on a network share, whose changes are in no journal blaze can read"
            )));
        }
        // The volume's flags do not say whether it keeps a journal: Windows 11 (26200) reports
        // none of `FILE_SUPPORTS_USN_JOURNAL` on NTFS volumes that answer the query, so the query
        // itself is the test.
        let (system, _) = volume_info(&hint).map_err(|it| context("its volume", it))?;
        // `\\?\Volume{GUID}\...`: the volume is its first component, opened as a device.
        final_path(&hint, FILE_NAME_NORMALIZED | VOLUME_NAME_GUID, &mut scratch)
            .map_err(|it| context("its volume", it))?;
        let end = scratch
            .iter()
            .skip(4)
            .position(|&it| it == BACKSLASH)
            .map_or(scratch.len(), |it| it + 4);
        // alloc: setup.
        let mut device = scratch[..end].to_vec();
        device.push(0);
        let querier =
            open(&device, FILE_TRAVERSE, 0).map_err(|it| context("opening its volume", it))?;
        // alloc: setup, shared with the flushes' marks for the watch's life.
        let querier = Arc::new(querier);
        let reader = open(&device, FILE_TRAVERSE, FILE_FLAG_OVERLAPPED)
            .map_err(|it| context("opening its volume", it))?;
        let data = query(&querier).map_err(|error| match error.raw_os_error().map(|it| it as u32) {
            // alloc: an error path.
            Some(ERROR_JOURNAL_NOT_ACTIVE) => refuse(format!(
                "the volume of {shown} has no USN journal; `fsutil usn createjournal m=0x2000000 a=0x400000 <drive>` \
                 run as an administrator makes one (blaze reads it and does not make it)"
            )),
            // alloc: an error path.
            Some(ERROR_INVALID_FUNCTION) => refuse(format!("{shown} is on a {system} volume, which keeps no USN journal")),
            // alloc: an error path.
            Some(ERROR_JOURNAL_DELETE_IN_PROGRESS) => refuse(format!("the USN journal of the volume of {shown} is being deleted")),
            _ => context("querying it", error),
        })?;
        let now = data.NextUsn;
        let (next, lost) = match since {
            Cursor::Usn { volume, journal, next }
                if volume == serial && journal == data.UsnJournalID =>
            {
                match next {
                    next if next < data.FirstUsn => (now, Some(Lost::Truncated)),
                    next if next > now => (now, Some(Lost::Reset)),
                    next => (next, None),
                }
            }
            Cursor::None => (now, None),
            _ => (now, Some(Lost::Reset)),
        };
        // clone: setup.
        let mut names = Names::new(dir.clone(), root_id);
        if let Some(lost) = lost {
            names.lose_all(lost);
        }
        // alloc: setup, the watch itself, kept where it is while the kernel reads into it.
        let mut journal = Box::new(Journal {
            names,
            // alloc: setup.
            volume: Volume { hint, root: root_nt, dir: dir.clone(), scratch, text: String::new() },
            reader,
            querier,
            shown,
            serial,
            journal: data.UsnJournalID,
            next,
            target: now,
            caught_up: next >= now,
            hurry: false,
            told: next,
            input: READ_USN_JOURNAL_DATA_V1::default(),
            // alloc: setup, the one buffer every read of the watch fills.
            buffer: vec![0u64; BUFFER / 8].into_boxed_slice(),
            records: Vec::with_capacity(BUFFER / 64),
        });
        let first = journal.caught_up.then(|| journal.batch());
        Ok((journal, first))
    }

    /// The volume handle the reads are on: what the port is told of and a cancel names.
    pub(super) fn raw(&self) -> HANDLE {
        self.reader.as_raw_handle()
    }

    /// The synchronous handle of the volume a flush's [`mark`] is asked through.
    pub(super) fn querier(&self) -> Arc<OwnedHandle> {
        // clone: an `Arc`, once per watch as it opens.
        Arc::clone(&self.querier)
    }

    /// The watched directory as the system names it, for errors.
    pub(super) fn shown(&self) -> &str {
        &self.shown
    }

    /// Whether the next read is asked without waiting, as it is while a flush waits.
    pub(super) fn set_hurry(&mut self, hurry: bool) {
        self.hurry = hurry;
    }

    /// Whether the read last asked waits for a record: one a flush must cancel to be told.
    pub(super) fn waits(&self) -> bool {
        self.input.BytesToWaitFor != 0
    }

    /// How far the watch has told, `stalled` as the read was.
    pub(super) fn reached(&self, stalled: bool) -> Read {
        let usn = match self.names.is_empty() {
            true => self.next,
            false => self.told,
        };
        Read { told: At { journal: self.journal, usn }, stalled }
    }

    /// Asks for the next records, completing on the port through `overlapped`. The request
    /// and the buffer stay where they are until that completion is dequeued (the driver keeps
    /// the journal in the request until then).
    ///
    /// # Safety
    ///
    /// `overlapped` is a request's [`super::sys::Head::overlapped`], made ready for this step,
    /// and the request's entry (its head and this journal with its buffer and input) stays where it is, alive, until the
    /// step's completion is dequeued: the kernel writes them after this returns, so freeing or
    /// moving either first is a write to freed memory.
    pub(super) unsafe fn read(&mut self, overlapped: *mut OVERLAPPED) -> io::Result<()> {
        self.input = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: self.next,
            ReasonMask: REASONS,
            ReturnOnlyOnClose: 0,
            // Ignored on an overlapped handle: a live read waits until a record comes or it is
            // cancelled.
            Timeout: 0,
            // Live, a read waits for a record, unless a flush waits: then it returns at once, at
            // the journal's end, which is past the flush's mark.
            BytesToWaitFor: u64::from(self.caught_up && !self.hurry),
            UsnJournalID: self.journal,
            MinMajorVersion: 2,
            MaxMajorVersion: 3,
        };
        // SAFETY: an open volume handle on the port, the request and a buffer of the lengths
        // given and an `OVERLAPPED`, all of which outlive the request (above); the kernel reads
        // and writes them in place (`METHOD_NEITHER`).
        sys::pending(unsafe {
            DeviceIoControl(
                self.reader.as_raw_handle(),
                FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
                (&raw const self.input).cast(),
                size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                self.buffer.as_mut_ptr().cast(),
                (self.buffer.len() * 8) as u32,
                std::ptr::null_mut(),
                overlapped,
            )
        })
    }

    /// What the read that ended with `result` came to. Blocks on the names it asks of the
    /// volume: for the driver's threads.
    pub(super) fn take(&mut self, result: io::Result<u32>) -> Taken {
        let start = self.next;
        let got = match result {
            Ok(got) => got as usize,
            Err(error) => return self.failed(error),
        };
        // SAFETY: the read's completion was taken, so the kernel holds the buffer no more; it
        // wrote `got` bytes of it, read here as bytes while nothing writes it.
        let bytes = unsafe {
            std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), got.min(BUFFER))
        };
        let mut records = std::mem::take(&mut self.records);
        records.clear();
        let parsed = parse(bytes, &mut records);
        let next = match parsed {
            Ok(next) => next,
            Err(why) => return self.end(format_args!("reading it: {why}")),
        };
        for record in &records {
            let (at, len) = (record.name.0 as usize, record.name.1 as usize);
            let name = (len > 0).then(|| &bytes[at..at + len]);
            self.names.record(record, name, &mut self.volume);
        }
        self.records = records;
        self.names.read_done();
        self.next = next;
        self.after(start)
    }

    /// The batch to tell after a read from `start`, if one is due, and the end if the watched
    /// directory moved.
    fn after(&mut self, start: i64) -> Taken {
        let stalled = !self.waits() && self.next == start;
        let mut tell = false;
        if !self.caught_up && (self.next >= self.target || self.next == start) {
            self.caught_up = true;
            tell = true;
        }
        tell |= match self.caught_up {
            true => !self.names.is_empty() || self.next - self.told >= ADVANCE,
            false => self.names.gathered() >= GATHER,
        };
        if self.names.root_changed {
            return self.end(format_args!(
                "the watched directory was moved, replaced or removed: it is to be watched again"
            ));
        }
        let batch = tell.then(|| self.batch());
        Taken { batch, end: None, read: Some(self.reached(stalled)) }
    }

    fn batch(&mut self) -> Box<Journaled> {
        let (files, lost) = self.names.drain();
        self.told = self.next;
        // alloc: the batch as `Done::Journaled` boxes it, once per batch.
        Box::new(Journaled {
            files,
            lost,
            cursor: Cursor::Usn { volume: self.serial, journal: self.journal, next: self.next },
            caught_up: self.caught_up,
        })
    }

    /// The watch's end, `why` said of the journal, with what was gathered before it told.
    fn end(&mut self, why: fmt::Arguments<'_>) -> Taken {
        let batch = (!self.names.is_empty()).then(|| self.batch());
        // alloc: the watch's end, once.
        Taken { batch, end: Some(format!("the USN journal of {}: {why}", self.shown)), read: None }
    }

    /// The journal read again from its `NextUsn` now, after what is between being `lost`.
    fn restart(&mut self, data: &USN_JOURNAL_DATA_V1, lost: Lost) -> Taken {
        self.journal = data.UsnJournalID;
        self.next = data.NextUsn;
        self.target = data.NextUsn;
        self.caught_up = true;
        self.names.lose_all(lost);
        let batch = Some(self.batch());
        Taken { batch, end: None, read: Some(self.reached(false)) }
    }

    fn failed(&mut self, error: io::Error) -> Taken {
        let code = error.raw_os_error().map(|it| it as u32);
        match code {
            // Nothing past the start: the end of the history, or of what is there yet.
            Some(ERROR_HANDLE_EOF) => self.after(self.next),
            // The journal no longer reaches the next USN: it wrapped past it.
            Some(ERROR_JOURNAL_ENTRY_DELETED) => match query(&self.querier) {
                Ok(data) => self.restart(&data, Lost::Truncated),
                Err(again) => self.end(format_args!("querying it: {again}")),
            },
            // Deleted, or made again with another ID, since the last read: made again is a
            // reset, read from its start now; deleted and not made again ends the watch, its
            // last batch saying the whole directory is lost.
            Some(
                ERROR_JOURNAL_DELETE_IN_PROGRESS
                | ERROR_JOURNAL_NOT_ACTIVE
                | ERROR_INVALID_PARAMETER,
            ) => match query(&self.querier) {
                Ok(data) if data.UsnJournalID != self.journal => self.restart(&data, Lost::Reset),
                Ok(_) => self.end(format_args!("reading it: {error}")),
                Err(again) => match again.raw_os_error().map(|it| it as u32) {
                    Some(ERROR_JOURNAL_NOT_ACTIVE | ERROR_JOURNAL_DELETE_IN_PROGRESS) => {
                        self.names.lose_all(Lost::Reset);
                        self.end(format_args!("it was deleted"))
                    }
                    _ => self.end(format_args!("querying it: {again}")),
                },
            },
            Some(ERROR_INVALID_FUNCTION) => {
                self.end(format_args!("reading it unprivileged needs Windows 10 1709 or later"))
            }
            _ => self.end(format_args!("reading it: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    /// A volume as a map of where each reference number is now.
    #[derive(Default)]
    struct Fake {
        now: HashMap<u128, Found, Global>,
        moved: bool,
        asked: Vec<u128>,
    }

    impl Fake {
        fn at(mut self, id: u128, found: &str) -> Fake {
            self.now.insert(id, Found::Under(name(found)));
            self
        }
    }

    impl System for Fake {
        fn find(&mut self, id: u128) -> Found {
            self.asked.push(id);
            self.now.get(&id).cloned().unwrap_or(Found::Gone)
        }

        fn root_moved(&mut self) -> bool {
            self.moved
        }
    }

    const ROOT: u128 = 1;
    const DIR: u32 = FILE_ATTRIBUTE_DIRECTORY;

    fn record(id: u128, parent: u128, reason: u32, attributes: u32) -> Record {
        Record { id, parent, reason, attributes, name: (0, 0) }
    }

    fn run(
        names: &mut Names,
        system: &mut Fake,
        records: &[(Record, Option<&str>)],
    ) -> (Vec<FileName>, Vec<(FileName, Lost)>) {
        for (record, leaf) in records {
            let bytes: Option<Vec<u8>> =
                leaf.map(|it| it.encode_utf16().flat_map(u16::to_le_bytes).collect());
            names.record(record, bytes.as_deref(), system);
        }
        names.read_done();
        names.drain()
    }

    fn files(names: &[&str]) -> Vec<FileName> {
        names.iter().map(|it| name(it)).collect()
    }

    #[test]
    fn a_file_written_is_named_where_it_is_once_a_read() {
        let mut names = Names::new(name("w"), ROOT);
        let mut system = Fake::default().at(2, "w/src").at(3, "w/src/a.rs");
        let write = record(3, 2, USN_REASON_DATA_EXTEND, 0);
        let close = record(3, 2, USN_REASON_DATA_EXTEND | USN_REASON_CLOSE, 0);
        let (files, lost) = run(&mut names, &mut system, &[(write, None), (close, None)]);
        assert_eq!((files, lost), (self::files(&["w/src/a.rs"]), vec![]));
        assert_eq!(system.asked, [2, 3]);
        // The directory is kept; the file is asked again in the next read.
        run(&mut names, &mut system, &[(write, None)]);
        assert_eq!(system.asked, [2, 3, 3]);
    }

    #[test]
    fn a_removal_or_an_old_name_is_its_directorys_names_changed() {
        let mut names = Names::new(name("w"), ROOT);
        let mut system = Fake::default().at(2, "w/src").at(4, "w/src/new.rs");
        let (files, lost) = run(
            &mut names,
            &mut system,
            &[
                (record(3, ROOT, USN_REASON_FILE_DELETE | USN_REASON_CLOSE, 0), None),
                (record(4, 2, USN_REASON_RENAME_OLD_NAME, 0), None),
                (record(4, 2, USN_REASON_RENAME_NEW_NAME, 0), None),
                (
                    record(
                        4,
                        2,
                        USN_REASON_RENAME_OLD_NAME | USN_REASON_RENAME_NEW_NAME | USN_REASON_CLOSE,
                        0,
                    ),
                    None,
                ),
            ],
        );
        assert_eq!(files, self::files(&["w/src/new.rs"]));
        assert_eq!(lost, [(name("w"), Lost::Unnamed), (name("w/src"), Lost::Unnamed)]);
    }

    #[test]
    fn what_is_outside_is_asked_of_once_and_never_named() {
        let mut names = Names::new(name("w"), ROOT);
        let mut system = Fake::default();
        system.now.insert(9, Found::Outside);
        let write = record(10, 9, USN_REASON_DATA_OVERWRITE, 0);
        for _ in 0..3 {
            let (files, lost) = run(
                &mut names,
                &mut system,
                &[(write, None), (record(11, 9, USN_REASON_FILE_DELETE, 0), None)],
            );
            assert!(files.is_empty() && lost.is_empty());
        }
        assert_eq!(system.asked, [9]);
    }

    #[test]
    fn a_directory_made_and_renamed_unnamed_names_what_is_in_it_where_it_is_now() {
        let mut names = Names::new(name("w"), ROOT);
        let mut system = Fake::default().at(5, "w/moved").at(6, "w/moved/f.rs");
        let (files, lost) = run(
            &mut names,
            &mut system,
            &[
                (record(5, ROOT, USN_REASON_FILE_CREATE, DIR), None),
                (record(6, 5, USN_REASON_FILE_CREATE, 0), None),
                (record(6, 5, USN_REASON_DATA_EXTEND, 0), None),
                (record(5, ROOT, USN_REASON_RENAME_OLD_NAME, DIR), None),
                (record(5, ROOT, USN_REASON_RENAME_NEW_NAME, DIR), None),
            ],
        );
        assert_eq!(files, self::files(&["w/moved", "w/moved/f.rs"]));
        assert_eq!(lost, [(name("w"), Lost::Unnamed)]);
    }

    #[test]
    fn named_records_are_named_from_their_parent_without_asking() {
        let mut names = Names::new(name("w"), ROOT);
        let mut system = Fake::default();
        let (files, lost) = run(
            &mut names,
            &mut system,
            &[
                (record(5, ROOT, USN_REASON_FILE_CREATE, DIR), Some("new")),
                (record(6, 5, USN_REASON_FILE_CREATE, 0), Some("f.rs")),
                (record(5, ROOT, USN_REASON_RENAME_OLD_NAME, DIR), Some("new")),
                (record(5, ROOT, USN_REASON_RENAME_NEW_NAME, DIR), Some("moved")),
                (record(7, 5, USN_REASON_FILE_CREATE, 0), Some("g.rs")),
                (record(8, ROOT, USN_REASON_DATA_EXTEND, 0), Some("n\u{e9}.rs")),
            ],
        );
        assert_eq!(
            files,
            self::files(&["w/moved", "w/moved/g.rs", "w/new", "w/new/f.rs", "w/n\u{e9}.rs"])
        );
        assert!(lost.is_empty());
        assert!(system.asked.is_empty(), "{:?}", system.asked);
    }

    #[test]
    fn a_name_that_is_not_utf8_is_its_directory_looked_at_again() {
        let mut names = Names::new(name("w"), ROOT);
        let mut system = Fake::default().at(2, "w/src");
        let mut record_ = record(3, 2, USN_REASON_FILE_CREATE, 0);
        record_.name = (0, 2);
        names.record(&record_, Some(&0xD800u16.to_le_bytes()), &mut system);
        assert_eq!(names.drain(), (vec![], vec![(name("w/src"), Lost::Rescan)]));
        system.now.insert(4, Found::Unnameable(name("w/src")));
        names.record(&record(4, 2, USN_REASON_DATA_EXTEND, 0), None, &mut system);
        assert_eq!(names.drain(), (vec![], vec![(name("w/src"), Lost::Rescan)]));
    }

    #[test]
    fn the_watched_directory_moving_ends_the_watch() {
        let mut names = Names::new(name("w"), ROOT);
        let mut system = Fake::default();
        let (files, lost) = run(
            &mut names,
            &mut system,
            &[(record(ROOT, 99, USN_REASON_BASIC_INFO_CHANGE, DIR), None)],
        );
        assert_eq!((files, lost), (self::files(&["w"]), vec![]));
        assert!(!names.root_changed);
        run(&mut names, &mut system, &[(record(ROOT, 99, USN_REASON_RENAME_OLD_NAME, DIR), None)]);
        assert!(names.root_changed);
        // One above it, met as a directory not known moving.
        let mut names = Names::new(name("w"), ROOT);
        let mut system = Fake { moved: true, ..Fake::default() };
        system.now.insert(50, Found::Outside);
        let (_, lost) = run(
            &mut names,
            &mut system,
            &[(record(51, 50, USN_REASON_RENAME_OLD_NAME, DIR), None)],
        );
        assert!(names.root_changed);
        assert_eq!(lost, [(name("w"), Lost::RootChanged)]);
    }

    #[test]
    fn a_directory_not_known_moving_away_forgets_where_directories_are() {
        let mut names = Names::new(name("w"), ROOT);
        let mut system = Fake::default().at(2, "w/a/b").at(3, "w/a/b/f.rs");
        system.now.insert(4, Found::Outside);
        let write = record(3, 2, USN_REASON_DATA_EXTEND, 0);
        run(
            &mut names,
            &mut system,
            &[(write, None), (record(30, 4, USN_REASON_DATA_EXTEND, 0), None)],
        );
        assert_eq!(system.asked, [2, 3, 4]);
        // `w/a`, never met, moves to `w/c`: `w/a/b` is not where it was.
        system.now.insert(2, Found::Under(name("w/c/b")));
        system.now.insert(3, Found::Under(name("w/c/b/f.rs")));
        let (files, lost) = run(
            &mut names,
            &mut system,
            &[(record(20, ROOT, USN_REASON_RENAME_OLD_NAME, DIR), None), (write, None)],
        );
        assert_eq!(files, self::files(&["w/c/b/f.rs"]));
        assert_eq!(lost, [(name("w"), Lost::Unnamed)]);
        assert_eq!(&system.asked[3..], [2, 3]);
        // A directory known moving away forgets only what was under it; outside stays outside.
        let (files, _) = run(
            &mut names,
            &mut system,
            &[
                (record(2, 20, USN_REASON_RENAME_OLD_NAME, DIR), None),
                (record(30, 4, USN_REASON_DATA_EXTEND, 0), None),
            ],
        );
        assert_eq!(files, self::files(&["w/c/b"]));
        assert_eq!(&system.asked[5..], [20]);
    }

    #[test]
    fn what_is_gathered_is_told_each_once_sorted() {
        let mut names = Names::new(name("w"), ROOT);
        names.files.extend([name("w/b"), name("w/a"), name("w/b")]);
        names.lost.extend([
            (name("w/x"), Lost::Unnamed),
            (name("w"), Lost::Reset),
            (name("w/x"), Lost::Unnamed),
        ]);
        let (files, lost) = names.drain();
        assert_eq!(files, self::files(&["w/a", "w/b"]));
        assert_eq!(lost, [(name("w"), Lost::Reset), (name("w/x"), Lost::Unnamed)]);
        assert!(names.is_empty());
    }

    fn v2(id: u64, parent: u64, reason: u32, attributes: u32, leaf: &str) -> Vec<u8> {
        let wide: Vec<u8> = leaf.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let len = (60 + wide.len()).next_multiple_of(8);
        let mut bytes = vec![0u8; len];
        bytes[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
        bytes[8..16].copy_from_slice(&id.to_le_bytes());
        bytes[16..24].copy_from_slice(&parent.to_le_bytes());
        bytes[40..44].copy_from_slice(&reason.to_le_bytes());
        bytes[52..56].copy_from_slice(&attributes.to_le_bytes());
        bytes[56..58].copy_from_slice(&(wide.len() as u16).to_le_bytes());
        bytes[58..60].copy_from_slice(&60u16.to_le_bytes());
        bytes[60..60 + wide.len()].copy_from_slice(&wide);
        bytes
    }

    fn v3(id: u128, parent: u128, reason: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 80];
        bytes[0..4].copy_from_slice(&80u32.to_le_bytes());
        bytes[4..6].copy_from_slice(&3u16.to_le_bytes());
        bytes[8..24].copy_from_slice(&id.to_le_bytes());
        bytes[24..40].copy_from_slice(&parent.to_le_bytes());
        bytes[56..60].copy_from_slice(&reason.to_le_bytes());
        bytes[74..76].copy_from_slice(&76u16.to_le_bytes());
        bytes
    }

    #[test]
    fn records_of_both_versions_are_read_to_the_end() {
        let mut bytes = 777i64.to_le_bytes().to_vec();
        bytes.extend(v2(5, 1, USN_REASON_FILE_CREATE, DIR, "src"));
        let second = bytes.len();
        bytes.extend(v3(1 << 70, 5, USN_REASON_DATA_EXTEND));
        let mut records = Vec::new();
        assert_eq!(parse(&bytes, &mut records), Ok(777));
        assert_eq!(
            records,
            [
                Record {
                    id: 5,
                    parent: 1,
                    reason: USN_REASON_FILE_CREATE,
                    attributes: DIR,
                    name: (8 + 60, 6)
                },
                Record {
                    id: 1 << 70,
                    parent: 5,
                    reason: USN_REASON_DATA_EXTEND,
                    attributes: 0,
                    name: (second as u32 + 76, 0)
                },
            ]
        );
        assert_eq!(
            &bytes[68..74],
            "src".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_record_past_the_end_or_of_another_version_is_refused() {
        let mut bytes = 1i64.to_le_bytes().to_vec();
        bytes.extend(v2(5, 1, 0, 0, "x"));
        let mut records = Vec::new();
        assert!(parse(&bytes[..bytes.len() - 1], &mut records).is_err());
        let mut four = bytes.clone();
        four[12..14].copy_from_slice(&4u16.to_le_bytes());
        assert!(parse(&four, &mut records).is_err());
        let mut long_name = bytes.clone();
        long_name[8 + 56..8 + 58].copy_from_slice(&200u16.to_le_bytes());
        assert!(parse(&long_name, &mut records).is_err());
        assert!(parse(&[0; 4], &mut records).is_err());
        records.clear();
        assert_eq!(parse(&9i64.to_le_bytes(), &mut records), Ok(9));
        assert!(records.is_empty());
    }

    #[test]
    fn a_close_tells_only_a_removal_of_names() {
        let close =
            How::of(USN_REASON_RENAME_OLD_NAME | USN_REASON_RENAME_NEW_NAME | USN_REASON_CLOSE);
        assert!(!close.gone && !close.arrived && !close.names);
        let removed =
            How::of(USN_REASON_FILE_DELETE | USN_REASON_RENAME_NEW_NAME | USN_REASON_CLOSE);
        assert!(removed.gone && !removed.arrived && removed.names);
        let link = How::of(USN_REASON_HARD_LINK_CHANGE);
        assert!(!link.gone && link.names);
    }
}
