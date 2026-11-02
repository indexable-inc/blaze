//! What a restart needs to know which files changed while blaze was down
//! (docs/v2/plan/design/restore.md, docs/v2/plan/lanes/frontend/journal.md): a [`Cursor`] into
//! the system's journal of changes (FSEvents' event IDs, NTFS's USN journal), the
//! [`Journaled`] batches an [`crate::Effect::Journal`] completes with, and the [`Stamp`]s of a
//! sweep ([`crate::Effect::Stamp`]) that compares what a file is now with what it was.

use std::fmt;

use crate::FileName;

/// Many names under the driver's root in one buffer, for an effect over thousands of them:
/// each is a [`FileName`]'s text and a NUL, so a driver hands it to the system as it is, with
/// no allocation per name.
#[derive(Default, PartialEq, Eq, Debug)]
#[cfg_attr(test, derive(Clone))] // clone: a test stamps the same names twice; effects share them by `Arc`
pub struct Names {
    bytes: Vec<u8>,
    /// Where each name starts in `bytes`; it ends at the NUL before the next start.
    starts: Vec<u32>,
}

impl Names {
    pub fn with_capacity(names: usize, bytes: usize) -> Names {
        Names { bytes: Vec::with_capacity(bytes), starts: Vec::with_capacity(names) }
    }

    pub fn push(&mut self, name: &FileName) {
        self.push_unchecked(name.as_str());
    }

    /// Adds `name` if it is a [`FileName`]'s text; `false`, adding nothing, if not.
    pub fn push_str(&mut self, name: &str) -> bool {
        let ok = FileName::is_valid(name) && !name.as_bytes().contains(&0);
        if ok {
            self.push_unchecked(name);
        }
        ok
    }

    /// Adds `name`, which the caller holds as a [`FileName`]'s text already (a name of a column
    /// that was checked as it was opened): not checked again.
    pub fn push_checked(&mut self, name: &str) {
        debug_assert!(FileName::is_valid(name) && !name.as_bytes().contains(&0), "{name:?}");
        self.push_unchecked(name);
    }

    /// Of `bytes` laid out as a [`Names`] keeps them (each name's text and a NUL, one after
    /// another) and where each name starts in them, for names checked as [`FileName`]s already
    /// (a column of an image, checked as it was opened): one copy, nothing checked again.
    pub fn from_checked(bytes: &[u8], starts: Vec<u32>) -> Names {
        debug_assert!(
            starts.is_empty() == bytes.is_empty()
                && starts.first().is_none_or(|it| *it == 0)
                && starts.iter().enumerate().all(|(at, &start)| {
                    let end = starts.get(at + 1).map_or(bytes.len(), |&it| it as usize);
                    let name = bytes.get(start as usize..end).unwrap_or_default();
                    name.split_last().is_some_and(|(nul, name)| {
                        *nul == 0
                            && std::str::from_utf8(name)
                                .is_ok_and(|it| FileName::is_valid(it) && !it.contains('\0'))
                    })
                }),
            "not names laid out as Names keeps them"
        );
        // alloc: one copy per sweep of a whole column.
        Names { bytes: bytes.to_vec(), starts }
    }

    /// `dir`, a `/`, then `rest`, if that is a name.
    pub fn push_joined(&mut self, dir: &str, rest: &str) -> bool {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(dir.as_bytes());
        if !dir.is_empty() {
            self.bytes.push(b'/');
        }
        self.bytes.extend_from_slice(rest.as_bytes());
        let ok = std::str::from_utf8(&self.bytes[start..])
            .is_ok_and(|it| FileName::is_valid(it) && !it.as_bytes().contains(&0));
        if ok {
            self.bytes.push(0);
            self.starts.push(u32::try_from(start).expect("names of under 4 GiB"));
        } else {
            self.bytes.truncate(start);
        }
        ok
    }

    fn push_unchecked(&mut self, name: &str) {
        let start = u32::try_from(self.bytes.len()).expect("names of under 4 GiB");
        self.bytes.extend_from_slice(name.as_bytes());
        self.bytes.push(0);
        self.starts.push(start);
    }

    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
        self.starts.clear();
    }

    /// The name at `at` with its NUL.
    pub fn with_nul(&self, at: usize) -> &[u8] {
        let start = self.starts[at] as usize;
        let end = self.starts.get(at + 1).map_or(self.bytes.len(), |&it| it as usize);
        &self.bytes[start..end]
    }

    /// The name at `at`.
    pub fn get(&self, at: usize) -> &str {
        let bytes = self.with_nul(at);
        std::str::from_utf8(&bytes[..bytes.len() - 1]).expect("pushed as a str")
    }

    /// The name at `at` as a C string, for the system's calls.
    pub fn c_str(&self, at: usize) -> &std::ffi::CStr {
        std::ffi::CStr::from_bytes_with_nul(self.with_nul(at)).expect("one NUL, at the end")
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        (0..self.len()).map(|at| self.get(at))
    }
}

/// What a file is on disk, as much as a sweep compares: a file whose stamp is unchanged is
/// taken as unchanged and never read or hashed. `mode` is POSIX's `st_mode` (its type bits and
/// permissions; a driver for a system with none makes them: `0o040755` for a directory,
/// `0o100644` for a file, `0o120777` for a link); times are nanoseconds since the epoch, or
/// whatever the system counts in, as long as it moves when the file does.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
#[repr(C)]
pub struct Stamp {
    /// The inode (NTFS: the file reference number): a file replaced by a rename has another.
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: u64,
    /// The inode's change time (NTFS: `ChangeTime`): moves on a chmod, a rename or a link too.
    pub ctime_ns: u64,
    pub mode: u32,
}

const _: () = assert!(std::mem::size_of::<Stamp>() == 40);

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;

impl Stamp {
    /// Of a name with nothing there.
    pub const GONE: Stamp = Stamp { ino: 0, size: 0, mtime_ns: 0, ctime_ns: 0, mode: 0 };

    /// Of a name the system would not say anything of (no permission, an I/O error): moved
    /// from any stamp a file had, so the asker looks at it and meets the error itself.
    pub const UNKNOWN: Stamp =
        Stamp { ino: u64::MAX, size: u64::MAX, mtime_ns: u64::MAX, ctime_ns: u64::MAX, mode: u32::MAX };

    pub fn is_unknown(&self) -> bool {
        *self == Stamp::UNKNOWN
    }

    /// Bytes of one in [`Stamp::encode`].
    pub const BYTES: usize = 36;

    pub fn is_gone(&self) -> bool {
        self.mode == 0
    }

    pub fn is_dir(&self) -> bool {
        !self.is_unknown() && self.mode & S_IFMT == S_IFDIR
    }

    pub fn is_link(&self) -> bool {
        !self.is_unknown() && self.mode & S_IFMT == S_IFLNK
    }

    pub fn is_file(&self) -> bool {
        !self.is_unknown() && self.mode & S_IFMT == S_IFREG
    }

    pub fn executable(&self) -> bool {
        self.mode & 0o111 != 0
    }

    /// Whether the file changed between `self` and `now`: anything a write, a replace, a
    /// chmod, a rename over it or a removal moves.
    pub fn moved(&self, now: &Stamp) -> bool {
        self != now
    }

    /// Little-endian, `BYTES` long, as an image keeps it.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.ino.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.mtime_ns.to_le_bytes());
        out.extend_from_slice(&self.ctime_ns.to_le_bytes());
        out.extend_from_slice(&self.mode.to_le_bytes());
    }

    /// Of [`Stamp::encode`]'s bytes, read in place; `None` when fewer than `BYTES`.
    pub fn decode(bytes: &[u8]) -> Option<Stamp> {
        let bytes: &[u8; Stamp::BYTES] = bytes.get(..Stamp::BYTES)?.try_into().ok()?;
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        Some(Stamp {
            ino: word(0),
            size: word(8),
            mtime_ns: word(16),
            ctime_ns: word(24),
            mode: u32::from_le_bytes(bytes[32..36].try_into().expect("4 bytes")),
        })
    }
}

/// Where a system's journal of changes was: a later [`crate::Effect::Journal`] from it is told
/// every change made after it, or that they are [`Lost`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub enum Cursor {
    /// No place: the watch starts now, and the asker looks at the whole directory itself.
    #[default]
    None,
    /// FSEvents: the last event ID taken, and the UUID of the volume's event store
    /// (`FSEventsCopyUUIDForDevice`), which changes when the store is reset and its IDs
    /// begin again.
    FsEvents { event: u64, volume: [u8; 16] },
    /// NTFS: the volume's serial number, its USN journal's ID (a new journal has a new one),
    /// and the next USN to read.
    Usn { volume: u64, journal: u64, next: i64 },
    /// The simulation's journal ([`crate::sim::Journal`]): its ID, and the next change.
    Sim { journal: u64, next: u64 },
}

impl Cursor {
    /// Bytes of one in [`Cursor::encode`].
    pub const BYTES: usize = 25;

    /// A kind byte and three words, little-endian, as an image keeps it.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let (kind, words): (u8, [u64; 3]) = match *self {
            Cursor::None => (0, [0; 3]),
            Cursor::FsEvents { event, volume } => (
                1,
                [
                    event,
                    u64::from_le_bytes(volume[..8].try_into().expect("8 bytes")),
                    u64::from_le_bytes(volume[8..].try_into().expect("8 bytes")),
                ],
            ),
            Cursor::Usn { volume, journal, next } => (2, [volume, journal, next as u64]),
            Cursor::Sim { journal, next } => (3, [journal, next, 0]),
        };
        out.push(kind);
        for word in words {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }

    /// Of [`Cursor::encode`]'s bytes; `None` for fewer than `BYTES` or an unknown kind.
    pub fn decode(bytes: &[u8]) -> Option<Cursor> {
        let bytes = bytes.get(..Cursor::BYTES)?;
        let word = |n: usize| {
            u64::from_le_bytes(bytes[1 + n * 8..9 + n * 8].try_into().expect("8 bytes"))
        };
        Some(match bytes[0] {
            0 => Cursor::None,
            1 => {
                let mut volume = [0; 16];
                volume[..8].copy_from_slice(&word(1).to_le_bytes());
                volume[8..].copy_from_slice(&word(2).to_le_bytes());
                Cursor::FsEvents { event: word(0), volume }
            }
            2 => Cursor::Usn { volume: word(0), journal: word(1), next: word(2) as i64 },
            3 => Cursor::Sim { journal: word(0), next: word(1) },
            _ => return None,
        })
    }
}

/// Why changes under a directory are not all named: whoever watches looks at that subtree
/// again (its stamps, and the entries of each directory whose stamp moved), and nothing else;
/// for [`Lost::Unnamed`], only the directory's own entries.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Lost {
    /// The system keeps no journal across a restart (Linux): what changed since the cursor
    /// is not known.
    NoJournal,
    /// The cursor is of another journal than the volume's now: FSEvents' store has another
    /// UUID, its IDs wrapped, the USN journal was deleted and made again, or the cursor is of
    /// another system's kind.
    Reset,
    /// The journal no longer reaches back to the cursor: its oldest record is past it.
    Truncated,
    /// The system dropped changes (its queue overflowed).
    Dropped,
    /// The system says to look at this directory again (FSEvents' `MustScanSubDirs`).
    Rescan,
    /// Names directly in this directory were made, removed or renamed and the journal does not
    /// say which (NTFS's unprivileged USN read gives no names): whoever watches lists its
    /// entries again ([`crate::Effect::Entries`]), takes what is new in it whole, and drops what
    /// is gone with what was under it; nothing else under it moved for this.
    Unnamed,
    /// The watched directory was moved, replaced, or its volume mounted or unmounted.
    RootChanged,
}

impl fmt::Display for Lost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Lost::NoJournal => "the system keeps no journal of changes across a restart",
            Lost::Reset => "the journal of changes was reset since the saved place in it",
            Lost::Truncated => "the journal of changes no longer reaches the saved place in it",
            Lost::Dropped => "the system dropped changes",
            Lost::Rescan => "the system said to look at the directory again",
            Lost::Unnamed => "the journal said names in the directory changed but not which",
            Lost::RootChanged => "the watched directory was moved, replaced or unmounted",
        })
    }
}

/// A batch of what an [`crate::Effect::Journal`] heard.
#[derive(Default, PartialEq, Eq, Debug)]
#[cfg_attr(test, derive(Clone))] // clone: inside `Done`, which only tests copy
pub struct Journaled {
    /// Files and directories under the watched one the journal names as made, written,
    /// removed or renamed (from or to), each once, sorted.
    pub files: Vec<FileName>,
    /// Subtrees whose changes the journal could not name, each with why; the watched
    /// directory itself for all of it.
    pub lost: Vec<(FileName, Lost)>,
    /// Where the journal is once these are taken: a watch from it is told only what is after.
    pub cursor: Cursor,
    /// Every change made before the watch started is in this batch or an earlier one; every
    /// batch after it is live.
    pub caught_up: bool,
}

/// The first batch of an [`crate::Effect::Journal`] where the system keeps no journal across
/// a restart (Linux): whatever changed since a saved cursor is lost, and the watch is live
/// from its start, with no cursor to save.
#[cfg(target_os = "linux")]
pub(crate) fn first_on_linux(dir: &FileName, since: &Cursor) -> crate::Done {
    let lost = match since {
        // alloc: none: the first batch, with nothing in it.
        Cursor::None => Vec::new(),
        // clone: once per watch, when a cursor was saved.
        _ => vec![(dir.clone(), Lost::NoJournal)],
    };
    // alloc: once per batch the system told, handed to the asker.
    crate::Done::Journaled(Box::new(Journaled {
        // alloc: none: no files in the first batch.
        files: Vec::new(),
        lost,
        cursor: Cursor::None,
        caught_up: true,
    }))
}

/// A watch's batch as a journal's on a system with no journal (Linux): changes it dropped
/// are the whole watched directory lost. Anything but [`crate::Done::Changed`] is as it was.
#[cfg(target_os = "linux")]
pub(crate) fn live(dir: &FileName, done: crate::Done) -> crate::Done {
    match done {
        // alloc: once per batch the system told, handed to the asker.
        crate::Done::Changed { files, lost } => crate::Done::Journaled(Box::new(Journaled {
            files,
            // clone: only when the system dropped changes.
            lost: if lost { vec![(dir.clone(), Lost::Dropped)] } else { Vec::new() },
            cursor: Cursor::None,
            caught_up: true,
        })),
        done => done,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_keep_each_name_and_its_nul() {
        let mut names = Names::default();
        assert!(names.push_str("src/lib.rs"));
        assert!(!names.push_str("../out"));
        assert!(!names.push_str("a\0b"));
        assert!(names.push_joined("src", "main.rs"));
        assert!(!names.push_joined("src", ""));
        assert!(names.push_joined("", "Cargo.toml"));
        assert_eq!(names.iter().collect::<Vec<_>>(), ["src/lib.rs", "src/main.rs", "Cargo.toml"]);
        assert_eq!(names.c_str(1).to_bytes(), b"src/main.rs");
        assert_eq!(names.with_nul(2), b"Cargo.toml\0");
        let copied = Names::from_checked(&names.bytes, names.starts.clone());
        assert_eq!(copied, names);
        assert_eq!(Names::from_checked(&[], Vec::new()), Names::default());
    }

    #[test]
    fn stamps_and_cursors_round_trip() {
        let stamp = Stamp { ino: 7, size: 1 << 40, mtime_ns: 3, ctime_ns: u64::MAX, mode: 0o100755 };
        let mut bytes = Vec::new();
        stamp.encode(&mut bytes);
        assert_eq!(bytes.len(), Stamp::BYTES);
        assert_eq!(Stamp::decode(&bytes), Some(stamp));
        assert!(stamp.is_file() && stamp.executable() && !stamp.is_dir());
        assert!(Stamp::GONE.is_gone());
        let cursors = [
            Cursor::None,
            Cursor::FsEvents { event: u64::MAX - 1, volume: *b"0123456789abcdef" },
            Cursor::Usn { volume: 0xdead_beef, journal: 9, next: -4 },
            Cursor::Sim { journal: 2, next: 1 << 33 },
        ];
        for cursor in cursors {
            let mut bytes = Vec::new();
            cursor.encode(&mut bytes);
            assert_eq!(bytes.len(), Cursor::BYTES);
            assert_eq!(Cursor::decode(&bytes), Some(cursor));
        }
        assert_eq!(Cursor::decode(&[9; Cursor::BYTES]), None);
        assert_eq!(Cursor::decode(&[0; 3]), None);
    }
}
