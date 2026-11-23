//! One level of a directory as columns: what [`crate::Effect::Entries`] completes with
//! ([`crate::Done::Entries`]). docs/v2/plan/design/std.md, "What std makes easy to get slow".
//!
//! In place of `Vec<(FileName, Stamp)>` (an `Arc<str>` allocated per entry, the directory's name
//! copied into each, 56 bytes of pair), a [`Listing`] holds the entries' own names (one part
//! each, never the directory's) end to end in one buffer with their ends (blaze-own's
//! [`SortedTexts`]), and beside them two columns in the same order: the [`Stamp`]s and the
//! [`Kind`]s. A listing of `n` entries is a constant number of allocations, whatever `n` is.
//!
//! The rules every driver and the simulation keep, through [`ListingBuilder`], the one way to
//! make a listing:
//! - **Order** is by the names' bytes (rule 25): never the directory's own order, which differs
//!   between file systems, machines and runs.
//! - **`.` and `..`** are not entries (they are the directory and its parent, in every one) and
//!   are left out wherever a system's call hands them over (Linux's `getdents64`).
//! - **A name that is not one part** (empty, with a `/` or a NUL) cannot come from a file
//!   system: it fails the listing ([`ListingError::NotAName`]), as a driver's bug.
//! - **A name that is not UTF-8** (Linux allows any bytes) fails the listing, naming the
//!   directory and the name (its bytes escaped): blaze's names are text, and a listing that
//!   quietly dropped one would say the file is not there ([`ListingError::NotUtf8`]).
//! - **Bounded** (rule 18): more than [`MOST_ENTRIES`] entries or [`MOST_NAME_BYTES`] bytes of
//!   names fails the listing, naming the directory, before anything past the cap is held.

use std::{alloc::Global, fmt};

use blaze_own::{Share, SortedTexts, Texts};

use crate::{DirName, FileName, Stamp};

/// Entries one listing holds at most: 2^20. A directory past it is a tree to split, not one to
/// list in one answer; the cap keeps a listing's columns under 64 MiB of stamps.
pub const MOST_ENTRIES: usize = 1 << 20;

/// Bytes of names one listing holds at most: 64 MiB (a name is at most 255 bytes on APFS, ext4
/// and XFS, so 2^20 names average 64 bytes at the cap). Under `u32::MAX`, which the ends allow.
pub const MOST_NAME_BYTES: usize = 64 << 20;

/// What an entry is, from its stamp's type bits: a column of its own, one byte an entry, so
/// a walk that wants the directories (or the files) reads one byte each and no stamp.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Kind {
    File,
    Dir,
    /// A symbolic link, stamped itself and not followed.
    Link,
    /// A FIFO, a socket or a device.
    Other,
    /// Gone between the listing and its stamp ([`Stamp::GONE`]).
    Gone,
    /// The system would say nothing of it ([`Stamp::UNKNOWN`]).
    Unknown,
}

impl Kind {
    /// Of `stamp`, by its type bits.
    pub fn of(stamp: &Stamp) -> Kind {
        if stamp.is_unknown() {
            Kind::Unknown
        } else if stamp.is_gone() {
            Kind::Gone
        } else if stamp.is_dir() {
            Kind::Dir
        } else if stamp.is_file() {
            Kind::File
        } else if stamp.is_link() {
            Kind::Link
        } else {
            Kind::Other
        }
    }
}

/// One entry's own name in a [`Listing`]: one part, never empty, `.` or `..`, with no `/`, and
/// UTF-8. A view into the listing's names: no allocation. [`Listing::file`] makes the
/// [`FileName`] under the root where one is needed (an effect takes it owned).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Leaf<'a>(&'a str);

impl<'a> Leaf<'a> {
    pub fn as_str(self) -> &'a str {
        self.0
    }
}

impl fmt::Display for Leaf<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Why a listing could not be made: each names the directory ([module](self)).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ListingError {
    /// More than [`MOST_ENTRIES`] entries.
    TooManyEntries { dir: DirName, most: usize },
    /// More than [`MOST_NAME_BYTES`] bytes of names.
    TooManyNameBytes { dir: DirName, most: usize },
    /// A name that is not UTF-8; `name` is its bytes, escaped.
    NotUtf8 { dir: DirName, name: String },
    /// A name that is not one part: empty, or with a `/` or a NUL.
    NotAName { dir: DirName, name: String },
    /// One name twice: no file system lists one so.
    Twice { dir: DirName, name: String },
    /// The allocator refused the columns' room.
    OutOfMemory { dir: DirName, bytes: usize },
}

impl fmt::Display for ListingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListingError::TooManyEntries { dir, most } => {
                write!(f, "{dir}: a listing of more than {most} entries is refused (listing cap)")
            }
            ListingError::TooManyNameBytes { dir, most } => {
                write!(f, "{dir}: a listing of more than {most} bytes of names is refused (listing cap)")
            }
            ListingError::NotUtf8 { dir, name } => {
                write!(f, "{dir}/{name}: a name that is not UTF-8; blaze's names are text, rename it")
            }
            ListingError::NotAName { dir, name } => {
                write!(f, "{dir}: the system listed {name:?}, which is not one part of a name")
            }
            ListingError::Twice { dir, name } => write!(f, "{dir}: the system listed {name} twice"),
            ListingError::OutOfMemory { dir, bytes } => {
                write!(f, "{dir}: the allocator refused {bytes} bytes for its listing")
            }
        }
    }
}

impl std::error::Error for ListingError {}

impl From<ListingError> for std::io::Error {
    fn from(error: ListingError) -> std::io::Error {
        let kind = match &error {
            ListingError::NotUtf8 { .. } | ListingError::NotAName { .. } | ListingError::Twice { .. } => {
                std::io::ErrorKind::InvalidData
            }
            ListingError::OutOfMemory { .. } => std::io::ErrorKind::OutOfMemory,
            ListingError::TooManyEntries { .. } | ListingError::TooManyNameBytes { .. } => {
                std::io::ErrorKind::FileTooLarge
            }
        };
        std::io::Error::new(kind, error)
    }
}

/// The entries of one directory as a driver finds them, in any order, made into a [`Listing`]
/// by [`ListingBuilder::finish`]: the one way to make one, so every listing keeps the
/// [module](self)'s rules.
pub struct ListingBuilder {
    dir: DirName,
    names: Texts<Global>,
    stamps: Vec<Stamp, Global>,
}

impl ListingBuilder {
    /// For the directory `dir`, the driver's root or a directory named under it.
    pub fn new(dir: impl Into<DirName>) -> ListingBuilder {
        ListingBuilder { dir: dir.into(), names: Texts::new_in(Global), stamps: Vec::new_in(Global) }
    }

    /// The entry `name`, as the system gave its bytes, with its stamp. `.` and `..` are left
    /// out; anything else that is not a name, and a name past a cap, fails ([module](self)).
    pub fn push(&mut self, name: &[u8], stamp: Stamp) -> Result<(), ListingError> {
        if name == b"." || name == b".." {
            return Ok(());
        }
        let dir = &self.dir;
        if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
            // alloc: an error path.
            let name = name.escape_ascii().to_string();
            return Err(ListingError::NotAName { dir: dir.share(), name });
        }
        let Ok(text) = std::str::from_utf8(name) else {
            // alloc: an error path.
            let name = name.escape_ascii().to_string();
            return Err(ListingError::NotUtf8 { dir: dir.share(), name });
        };
        if self.names.len() >= MOST_ENTRIES {
            return Err(ListingError::TooManyEntries { dir: dir.share(), most: MOST_ENTRIES });
        }
        if self.names.bytes_len() + text.len() > MOST_NAME_BYTES {
            return Err(ListingError::TooManyNameBytes { dir: dir.share(), most: MOST_NAME_BYTES });
        }
        let refused = |bytes| ListingError::OutOfMemory { dir: dir.share(), bytes };
        self.stamps.try_reserve(1).map_err(|_| refused(size_of::<Stamp>()))?;
        self.names.try_push(text).map_err(|_| refused(text.len()))?;
        self.stamps.push(stamp);
        Ok(())
    }

    /// The listing, sorted by the names' bytes: one sort of a `u32` index and one copy of the
    /// names in order, or none when they came sorted (the simulation's, and many file
    /// systems'). Fails on a name listed twice.
    pub fn finish(self) -> Result<Listing, ListingError> {
        let ListingBuilder { dir, names, stamps } = self;
        let (names, stamps) = match SortedTexts::try_from_sorted(names) {
            Ok(sorted) => (sorted, stamps),
            Err(names) => {
                let refused = |bytes| ListingError::OutOfMemory { dir: dir.share(), bytes };
                let n = names.len();
                let mut order: Vec<u32, Global> = Vec::new_in(Global);
                order.try_reserve_exact(n).map_err(|_| refused(n * size_of::<u32>()))?;
                // `n` is at most MOST_ENTRIES, so each index fits.
                order.extend((0..n).map(|i| i as u32));
                order.sort_unstable_by(|&a, &b| names.text(a as usize).cmp(names.text(b as usize)));
                let mut sorted = Texts::try_with_capacity_in(n, names.bytes_len(), Global)
                    .map_err(|_| refused(names.bytes_len()))?;
                let mut in_order: Vec<Stamp, Global> = Vec::new_in(Global);
                in_order.try_reserve_exact(n).map_err(|_| refused(n * size_of::<Stamp>()))?;
                for &at in &order {
                    // Room for all was made above, so neither push allocates.
                    sorted.try_push(names.text(at as usize)).map_err(|_| refused(0))?;
                    in_order.push(stamps[at as usize]);
                }
                match SortedTexts::try_from_sorted(sorted) {
                    Ok(sorted) => (sorted, in_order),
                    Err(sorted) => {
                        let twice = (1..sorted.len())
                            .find(|&i| sorted.text(i - 1) == sorted.text(i))
                            .map(|i| sorted.text(i))
                            .expect("sorted strings not strictly increasing hold one twice");
                        // alloc: an error path.
                        return Err(ListingError::Twice { dir, name: twice.to_owned() });
                    }
                }
            }
        };
        let mut kinds: Vec<Kind, Global> = Vec::new_in(Global);
        kinds
            .try_reserve_exact(stamps.len())
            .map_err(|_| ListingError::OutOfMemory { dir: dir.share(), bytes: stamps.len() })?;
        kinds.extend(stamps.iter().map(Kind::of));
        Ok(Listing { dir, names, stamps, kinds })
    }
}

/// What is directly in one directory: names, stamps and kinds as columns, sorted by the names'
/// bytes ([module](self)). Read by index (`0..len()`), or found by name in `O(log n)`.
pub struct Listing {
    /// The directory listed: the driver's root, or one named under it.
    dir: DirName,
    /// Each entry's own name, one part, sorted by bytes and each once.
    names: SortedTexts<Global>,
    /// Entry `i`'s stamp: a symbolic link's own, not followed.
    stamps: Vec<Stamp, Global>,
    /// Entry `i`'s kind, of its stamp.
    kinds: Vec<Kind, Global>,
}

impl Listing {
    /// The directory listed: the driver's root, or one named under it.
    pub fn dir(&self) -> &DirName {
        &self.dir
    }

    pub fn len(&self) -> usize {
        self.stamps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }

    /// Entry `i`'s own name.
    ///
    /// # Panics
    ///
    /// When `i >= len()`.
    #[track_caller]
    pub fn leaf(&self, i: usize) -> Leaf<'_> {
        Leaf(self.names.text(i))
    }

    #[track_caller]
    pub fn stamp(&self, i: usize) -> Stamp {
        self.stamps[i]
    }

    #[track_caller]
    pub fn kind(&self, i: usize) -> Kind {
        self.kinds[i]
    }

    /// The stamps, in the names' order.
    pub fn stamps(&self) -> &[Stamp] {
        &self.stamps
    }

    /// The kinds, in the names' order.
    pub fn kinds(&self) -> &[Kind] {
        &self.kinds
    }

    /// The index of the entry named `leaf` (one part), by binary search.
    pub fn find(&self, leaf: &str) -> Option<usize> {
        self.names.index_of(leaf)
    }

    /// The index of the entry whose name under the root is `file` (`dir/leaf`, or `leaf` in
    /// the root); `None` for a name not directly in this directory.
    pub fn find_file(&self, file: &str) -> Option<usize> {
        let leaf = match self.dir.name() {
            Some(dir) => file.strip_prefix(dir.as_str())?.strip_prefix('/')?,
            None => file,
        };
        self.find(leaf)
    }

    /// Entry `i`'s name under the root (`dir/leaf`, or `leaf` in the root), made: for an
    /// effect, which takes it owned.
    #[track_caller]
    pub fn file(&self, i: usize) -> FileName {
        let leaf = self.names.text(i);
        let dir = self.dir.name().map_or("", |dir| dir.as_str());
        // alloc: one name made, the effect's.
        let mut joined = String::with_capacity(dir.len() + 1 + leaf.len());
        self.file_into(i, &mut joined);
        FileName::new(&joined).expect("a directory's name and one part of a name are a name")
    }

    /// Entry `i`'s name under the root (`dir/leaf`, or `leaf` in the root), written into
    /// `into` (cleared first): read with no name made.
    #[track_caller]
    pub fn file_into<'s>(&self, i: usize, into: &'s mut String) -> &'s str {
        into.clear();
        if let Some(dir) = self.dir.name() {
            into.push_str(dir.as_str());
            into.push('/');
        }
        into.push_str(self.names.text(i));
        into
    }

    /// Each entry's own name and stamp, in order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (Leaf<'_>, Stamp)> + '_ {
        self.names.iter().map(Leaf).zip(self.stamps.iter().copied())
    }

    /// Keeps the entries `keep` says to, in order: one copy of the kept names, and none when
    /// all are kept.
    pub fn retain(&mut self, mut keep: impl FnMut(Leaf<'_>, &Stamp) -> bool) {
        let kept: Vec<bool, Global> = {
            let mut kept = Vec::with_capacity_in(self.len(), Global);
            kept.extend(self.iter().map(|(leaf, stamp)| keep(leaf, &stamp)));
            kept
        };
        if kept.iter().all(|it| *it) {
            return;
        }
        let bytes = self.names.iter().zip(&kept).filter(|it| *it.1).map(|it| it.0.len()).sum();
        let count = kept.iter().filter(|it| **it).count();
        let mut names = Texts::with_capacity_in(count, bytes, Global);
        for (name, _) in self.names.iter().zip(&kept).filter(|it| *it.1) {
            names.push(name);
        }
        let mut at = 0;
        self.stamps.retain(|_| (kept[at], at += 1).0);
        let mut at = 0;
        self.kinds.retain(|_| (kept[at], at += 1).0);
        self.names = SortedTexts::try_from_sorted(names)
            .unwrap_or_else(|_| unreachable!("a subsequence of sorted names is sorted"));
    }

    /// Each stamp passed through `change` (a test that floors times): the kinds follow.
    pub fn map_stamps(&mut self, mut change: impl FnMut(&mut Stamp)) {
        for (stamp, kind) in self.stamps.iter_mut().zip(self.kinds.iter_mut()) {
            change(stamp);
            *kind = Kind::of(stamp);
        }
    }

    /// The bytes this holds from its allocator.
    pub fn heap_bytes(&self) -> usize {
        self.names.heap_bytes()
            + self.stamps.capacity() * size_of::<Stamp>()
            + self.kinds.capacity() * size_of::<Kind>()
    }
}

impl Clone for Listing {
    // clone: a test keeping what it saw; a completion moves to its owner.
    fn clone(&self) -> Listing {
        let mut names =
            Texts::with_capacity_in(self.names.len(), self.names.as_texts().bytes_len(), Global);
        for name in self.names.iter() {
            names.push(name);
        }
        Listing {
            dir: self.dir.share(),
            names: SortedTexts::try_from_sorted(names)
                .unwrap_or_else(|_| unreachable!("a copy of sorted names is sorted")),
            stamps: self.stamps.clone(),
            kinds: self.kinds.clone(),
        }
    }
}

impl PartialEq for Listing {
    fn eq(&self, other: &Listing) -> bool {
        self.dir == other.dir && self.names.as_texts() == other.names.as_texts() && self.stamps == other.stamps
    }
}

impl Eq for Listing {}

impl fmt::Debug for Listing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listing").field("dir", &self.dir).field("entries", &DebugEntries(self)).finish()
    }
}

struct DebugEntries<'a>(&'a Listing);

impl fmt::Debug for DebugEntries<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.0.iter().map(|(leaf, stamp)| (leaf.as_str(), stamp))).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> DirName {
        FileName::new("src").unwrap().into()
    }

    fn file(size: u64) -> Stamp {
        Stamp { ino: size + 1, size, mtime_ns: 1, ctime_ns: 1, dev: 1, mode: crate::S_IFREG | 0o644 }
    }

    /// Any names in any order come out sorted by their bytes, each stamp beside its own name;
    /// each is found by name, and by its name under the root; `.` and `..` are no entries.
    #[test]
    fn a_listing_is_sorted_by_bytes_with_its_columns_beside() {
        blaze_sim::check(blaze_sim::Config::DEFAULT, |tape| {
            let count = tape.below(40, "entries") as usize;
            let mut names: Vec<String> = Vec::new();
            for _ in 0..count {
                let len = 1 + tape.below(4, "length") as usize;
                // Letters of both cases, punctuation that sorts between them, a two-byte letter.
                let name: String = (0..len)
                    .map(|_| ['a', 'B', '-', '.', '_', '0', 'é'][tape.below(7, "letter") as usize])
                    .collect();
                if !names.contains(&name) && name != "." && name != ".." {
                    names.push(name);
                }
            }
            let mut builder = ListingBuilder::new(dir());
            builder.push(b".", file(0)).unwrap();
            for (at, name) in names.iter().enumerate() {
                builder.push(name.as_bytes(), file(at as u64)).unwrap();
            }
            builder.push(b"..", file(0)).unwrap();
            let listing = builder.finish().unwrap();
            let mut sorted: Vec<(&str, u64)> =
                names.iter().enumerate().map(|(at, it)| (it.as_str(), at as u64)).collect();
            sorted.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            let got: Vec<(&str, u64)> = listing.iter().map(|(leaf, stamp)| (leaf.as_str(), stamp.size)).collect();
            assert_eq!(got, sorted);
            for (at, (name, _)) in sorted.iter().enumerate() {
                assert_eq!(listing.find(name), Some(at));
                assert_eq!(listing.find_file(&format!("src/{name}")), Some(at));
                assert_eq!(listing.file(at).as_str(), format!("src/{name}"));
                assert_eq!(listing.kind(at), Kind::File);
            }
            assert_eq!(listing.find("."), None);
            assert_eq!(listing.find_file("srcx/a"), None);
        });
    }

    /// The stated rule for what is not a name: a `/` or a NUL, or empty, fails naming the
    /// directory; bytes that are not UTF-8 fail naming the directory and the name escaped; a
    /// name twice fails.
    #[test]
    fn what_is_not_a_name_fails_the_listing_naming_it() {
        let push = |name: &[u8]| ListingBuilder::new(dir()).push(name, file(0));
        assert!(matches!(push(b"a/b"), Err(ListingError::NotAName { .. })));
        assert!(matches!(push(b"a\0"), Err(ListingError::NotAName { .. })));
        assert!(matches!(push(b""), Err(ListingError::NotAName { .. })));
        let refused = push(b"caf\xe9").unwrap_err();
        assert_eq!(refused, ListingError::NotUtf8 { dir: dir(), name: "caf\\xe9".into() });
        assert_eq!(refused.to_string(), "src/caf\\xe9: a name that is not UTF-8; blaze's names are text, rename it");
        let mut twice = ListingBuilder::new(dir());
        twice.push(b"b", file(0)).unwrap();
        twice.push(b"a", file(0)).unwrap();
        twice.push(b"b", file(1)).unwrap();
        assert_eq!(twice.finish().unwrap_err(), ListingError::Twice { dir: dir(), name: "b".into() });
    }

    /// A listing stops at its caps with a refusal naming the directory, before it holds more:
    /// the count of entries, and the bytes of names.
    #[test]
    fn a_listing_past_its_caps_is_refused() {
        // A push checks the caps; that no name is there twice is `finish`'s, so one name
        // pushed again fills them.
        let mut many = ListingBuilder::new(dir());
        for _ in 0..MOST_ENTRIES {
            many.push(b"a", file(0)).unwrap();
        }
        assert_eq!(
            many.push(b"one-more", file(0)),
            Err(ListingError::TooManyEntries { dir: dir(), most: MOST_ENTRIES })
        );
        let mut long = ListingBuilder::new(dir());
        for _ in 0..MOST_NAME_BYTES / 256 {
            long.push(&[b'a'; 256], file(0)).unwrap();
        }
        assert_eq!(
            long.push(b"x", file(0)),
            Err(ListingError::TooManyNameBytes { dir: dir(), most: MOST_NAME_BYTES })
        );
    }

    /// `retain` keeps the order and the columns together.
    #[test]
    fn retain_keeps_the_columns_together() {
        let mut builder = ListingBuilder::new(dir());
        for (at, name) in ["d", "a", "c", "b"].iter().enumerate() {
            builder.push(name.as_bytes(), file(at as u64)).unwrap();
        }
        let mut listing = builder.finish().unwrap();
        listing.retain(|leaf, _| leaf.as_str() != "b");
        let got: Vec<_> = listing.iter().map(|(leaf, stamp)| (leaf.as_str(), stamp.size)).collect();
        assert_eq!(got, [("a", 1), ("c", 2), ("d", 0)]);
        assert_eq!(listing.find("c"), Some(1));
        assert_eq!(listing.clone(), listing);
    }
}
