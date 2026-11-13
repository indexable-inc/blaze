//! A shipped tree's texts: one file a tree a machine, `<tree's parent>/<tree's name>.texts1`
//! beside the tree that [`super::place`] put at `<machine>/<name>/<digest>/<name>`, mapped by
//! every daemon that mounts the tree, which then lends each file's bytes from the mapping
//! (`blaze_vfs::lent`): file pages the kernel shares and gives back, not the heap, and no file
//! of the tree read or looked at once the texts are there. Nothing here names a tree: the
//! standard library, bstd and any tree [`super::place`] ships are entries in one list
//! (`cli::store::shipped`), and their texts are made and read by the one rule.
//!
//! A sans-IO core: [`encode`], [`decode`] and [`Place`], the steps that put the file in place.
//! The drivers that read the tree, run the steps and map the file are
//! `super::write_texts` and `super::open_texts`.
//!
//! The layout, little-endian:
//!
//! ```text
//! MAGIC (8) | u32 len + the tree's digest | [32] listing hash | u64 listing len | listing | texts
//! listing: u32 count, then per file in name order: u32 len + path | u8 mode (0 regular,
//!          1 executable) | [32] content id | u64 at | u64 len
//! texts:   each at its `at` from the file's start, in the listing's order, nothing between,
//!          the last ending at the file's end
//! ```
//!
//! The listing hash is [`DOMAIN`]'s hash of the listing's bytes; a content id is the VFS's
//! (`ContentId::of(blaze_vfs::FILE_DOMAIN, text)`), which the tree gives the file it mounts.
//!
//! What a start trusts and why. [`decode`] reads the header and the listing only, and checks
//! the digest, the listing's hash, the order and that the texts tile the file exactly; it never
//! hashes a text. The texts are trusted because of how the file comes to be: written once, from
//! the tree read whole and checked against its digest at that moment (`super::write_texts`
//! refuses a tree that is not its digest's), to `<name>.part`, synced, renamed over the name,
//! the directory synced ([`Place`]); a crash at any step leaves no file at the name or the whole
//! of one (the simulation test below). A file that fails [`decode`] is refused by name, never
//! read around (rule no-fallback).
//!
//! The layout is in the file's name (`.texts1`) as well as its magic: a blaze with another
//! layout writes and reads another name, so a machine that has run both keeps both, and a file
//! of another layout met under this name is a file someone changed, refused.
//!
//! How a registry crate would use it (not built): a `.crate` at its checksum is a tree by the
//! same rule, its checksum its digest, unpacked once a machine under a directory of its own
//! (the daemon's on-disk copy, `serve::fetch::keep_on_disk`, is that place today); its texts
//! file is made by the same driver at the unpack, and a fetch answered from it lends the
//! files' bytes from the mapping in place of reading the archive.


use std::{fmt, path::PathBuf};

use blaze_ids::ContentId;
use blaze_vfs::Mode;

/// The layout's number: in the file's name and its magic.
pub(crate) const LAYOUT: u8 = 1;

/// The first bytes of a texts file of [`LAYOUT`].
pub(crate) const MAGIC: [u8; 8] = *b"bztxts01";

/// The domain of the listing's hash.
const DOMAIN: &str = "blaze v2 tree texts 1";

/// A file of the tree as the listing has it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Entry {
    /// `/`-separated, from the tree's directory, with no empty, `.` or `..` part.
    pub path: String,
    pub mode: Mode,
    /// Its text's content id, as the VFS gives it: what the mount takes in place of hashing
    /// each text again (`Vfs::apply_known_files`).
    pub id: ContentId,
    /// Where its text is, from the file's start.
    pub at: u64,
    pub len: u64,
}

/// The listing of a texts file, decoded: its entries in name order, and its hash.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Listing {
    pub entries: Vec<Entry>,
    pub hash: [u8; 32],
}

/// Why [`decode`] refused a file. Its caller names the file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Refused {
    /// It does not start with [`MAGIC`]: another layout's, or not a texts file.
    Layout { found: [u8; 8] },
    /// It is the texts of a tree of another digest.
    Digest { wanted: String, found: String },
    /// Its listing's bytes are not what its header's hash says, or do not read as a listing.
    Listing { why: &'static str },
    /// It is not as long as its header and listing say: cut short, or with bytes after.
    Length { wanted: u64, found: u64 },
    /// Its entries are not in name order (or one is twice): the first out of place.
    Order { path: String },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Layout { found } => write!(
                f,
                "it starts with {:?}, not {:?}: not a texts file of layout {LAYOUT}",
                found.escape_ascii().to_string(),
                MAGIC.escape_ascii().to_string()
            ),
            Refused::Digest { wanted, found } => {
                write!(f, "it holds the texts of the tree of digest {found}, not {wanted}")
            }
            Refused::Listing { why } => write!(f, "its listing {why}"),
            Refused::Length { wanted, found } => {
                write!(f, "it is {found} bytes, and its header and listing say {wanted}")
            }
            Refused::Order { path } => {
                let said = blaze_report::say!("its listing has ", blaze_report::say::Code(path), " out of name order, or twice");
                f.write_str(&said.flat())
            }
        }
    }
}

/// `<tree's parent>/<tree's name>.texts<LAYOUT>`: beside the tree, in the directory
/// [`super::tree_dir`] gives it alone. `None` for a path with no parent or name.
pub(crate) fn name(tree: &std::path::Path) -> Option<PathBuf> {
    let leaf = tree.file_name()?.to_str()?;
    Some(tree.parent()?.join(format!("{leaf}.texts{LAYOUT}")))
}

fn mode_byte(mode: Mode, path: &str) -> u8 {
    match mode {
        Mode::Regular => 0,
        Mode::Executable => 1,
        Mode::Symlink => panic!("{path}: a link in a tree's texts; the tree's walk follows links"),
    }
}

/// The texts file of the tree of `digest` whose files are `files` (path, mode, text), in name
/// order (`blaze_ids::names::sort_by_name` of their paths), each path once: else it panics
/// naming the pair, as the walk that lists them sorts them.
pub(crate) fn encode(digest: &str, files: &[(String, Mode, &[u8])]) -> Vec<u8> {
    for pair in files.windows(2) {
        assert!(
            pair[0].0.as_bytes() < pair[1].0.as_bytes(),
            "{}",
            blaze_report::say!(
                "a tree's texts are given in name order, each path once: ",
                blaze_report::say::Code(&pair[0].0),
                " then ",
                blaze_report::say::Code(&pair[1].0)
            )
            .flat()
        );
    }
    let header = 8 + 4 + digest.len() + 32 + 8;
    let listing_len =
        4 + files.iter().map(|(path, _, _)| 4 + path.len() + 1 + 32 + 8 + 8).sum::<usize>();
    let texts_len = files.iter().map(|(_, _, text)| text.len()).sum::<usize>();
    // alloc: the listing, once a tree a machine
    let mut listing = Vec::with_capacity(listing_len);
    listing.extend_from_slice(&count(files.len()).to_le_bytes());
    let mut at = (header + listing_len) as u64;
    for (path, mode, text) in files {
        listing.extend_from_slice(&count(path.len()).to_le_bytes());
        listing.extend_from_slice(path.as_bytes());
        listing.push(mode_byte(*mode, path));
        listing.extend_from_slice(&ContentId::of(blaze_vfs::FILE_DOMAIN, text).0);
        listing.extend_from_slice(&at.to_le_bytes());
        listing.extend_from_slice(&(text.len() as u64).to_le_bytes());
        at += text.len() as u64;
    }
    debug_assert_eq!(listing.len(), listing_len);
    let hash = ContentId::of(DOMAIN, &listing).0;
    // alloc: the whole file, once a tree a machine, handed to `Place`
    let mut out = Vec::with_capacity(header + listing_len + texts_len);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&count(digest.len()).to_le_bytes());
    out.extend_from_slice(digest.as_bytes());
    out.extend_from_slice(&hash);
    out.extend_from_slice(&(listing.len() as u64).to_le_bytes());
    out.extend_from_slice(&listing);
    for (_, _, text) in files {
        out.extend_from_slice(text);
    }
    out
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or_else(|_| panic!("{n} is past the u32 a tree's texts count in"))
}

/// Reads at a cursor; `None` past the end.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let got = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(got)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

/// The listing of the texts file `bytes` of the tree of `digest`: its header and listing read
/// and checked (the digest, the listing's hash, name order, paths a tree can have, the texts
/// tiling the rest of the file exactly), no text read.
pub(crate) fn decode(digest: &str, bytes: &[u8]) -> Result<Listing, Refused> {
    let found = bytes.len() as u64;
    let short = |wanted: usize| Refused::Length { wanted: wanted as u64, found };
    let mut head = Cursor { bytes, at: 0 };
    let magic: [u8; 8] = head.take(8).ok_or(short(8))?.try_into().expect("8 bytes");
    if magic != MAGIC {
        return Err(Refused::Layout { found: magic });
    }
    let digest_len = head.u32().ok_or(short(12))? as usize;
    let stated = head.take(digest_len).ok_or(short(12 + digest_len))?;
    if stated != digest.as_bytes() {
        return Err(Refused::Digest {
            wanted: digest.to_owned(),
            found: String::from_utf8_lossy(stated).into_owned(),
        });
    }
    let fixed = 12 + digest_len + 32 + 8;
    let hash: [u8; 32] = head.take(32).ok_or(short(fixed))?.try_into().expect("32 bytes");
    let listing_len = head.u64().ok_or(short(fixed))?;
    let wanted = (fixed as u64).saturating_add(listing_len);
    let listing = usize::try_from(listing_len)
        .ok()
        .and_then(|len| head.take(len))
        .ok_or(Refused::Length { wanted, found })?;
    if ContentId::of(DOMAIN, listing).0 != hash {
        return Err(Refused::Listing { why: "is not the bytes its header's hash is of" });
    }
    let bad = |why| Refused::Listing { why };
    let mut it = Cursor { bytes: listing, at: 0 };
    let n = it.u32().ok_or(bad("ends before its count"))? as usize;
    // alloc: the listing's entries, once a tree a daemon mounts; at most what the listing's
    // bytes can hold, whatever its count says
    let mut entries: Vec<Entry> = Vec::with_capacity(n.min(listing.len() / (4 + 1 + 32 + 16)));
    let mut next = wanted;
    for _ in 0..n {
        let len = it.u32().ok_or(bad("ends inside an entry"))? as usize;
        let path = it.take(len).ok_or(bad("ends inside an entry"))?;
        let path = std::str::from_utf8(path).map_err(|_| bad("has a path that is not UTF-8"))?;
        if path.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
            return Err(bad("has a path with an empty, `.` or `..` part"));
        }
        if entries.last().is_some_and(|last| last.path.as_bytes() >= path.as_bytes()) {
            return Err(Refused::Order { path: path.to_owned() });
        }
        let mode = match it.take(1).ok_or(bad("ends inside an entry"))?[0] {
            0 => Mode::Regular,
            1 => Mode::Executable,
            _ => return Err(bad("has a mode that is neither 0 nor 1")),
        };
        let id: [u8; 32] =
            it.take(32).ok_or(bad("ends inside an entry"))?.try_into().expect("32 bytes");
        let at = it.u64().ok_or(bad("ends inside an entry"))?;
        let len = it.u64().ok_or(bad("ends inside an entry"))?;
        if at != next {
            return Err(bad("has a text that does not start where the one before it ends"));
        }
        next = at.checked_add(len).ok_or(bad("has a text past any file's end"))?;
        // alloc: the entry's path, once a tree a daemon mounts
        entries.push(Entry { path: path.to_owned(), mode, id: ContentId(id), at, len });
    }
    if it.at != listing.len() {
        return Err(bad("has bytes after its last entry"));
    }
    if next != found {
        return Err(Refused::Length { wanted: next, found });
    }
    Ok(Listing { entries, hash })
}

/// One step of putting a texts file in place, for its driver to do and complete before it asks
/// [`Place::step`] for the next.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Step {
    /// `bytes` written as the whole of `file`, made or cut first.
    Write { file: PathBuf, bytes: Vec<u8> },
    /// `file`'s bytes made durable.
    Sync { file: PathBuf },
    /// `from` renamed over `to`.
    Rename { from: PathBuf, to: PathBuf },
    /// The names in `dir` made durable.
    SyncDir { dir: PathBuf },
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Write,
    Sync,
    Rename,
    SyncDir,
    Done,
}

/// A planted bug for the crash test's control.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The part is renamed over the name before its bytes are synced.
    RenamesBeforeTheSync,
}

/// The steps that put `bytes` at `at`: `<at>.part` written, synced, renamed over `at`, `at`'s
/// directory synced. A crash at any of them leaves at `at` what was there or the whole of
/// `bytes`; a `.part` left by one is written over by the next place. Two processes never place
/// one tree's texts at once: the driver holds the tree's lock.
pub(crate) struct Place {
    at: PathBuf,
    part: PathBuf,
    bytes: Option<Vec<u8>>,
    stage: Stage,
    #[cfg(test)]
    plant: Plant,
}

impl Place {
    pub(crate) fn new(at: PathBuf, bytes: Vec<u8>) -> Place {
        let mut part = at.clone().into_os_string(); // clone: the part's name, once a place
        part.push(".part");
        Place {
            at,
            part: PathBuf::from(part),
            bytes: Some(bytes),
            stage: Stage::Write,
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    #[cfg(test)]
    pub(crate) fn planted(mut self, plant: Plant) -> Place {
        self.plant = plant;
        self
    }

    /// The next step; each is asked once its one before has completed. [`Step::Done`] once
    /// the last has.
    pub(crate) fn step(&mut self) -> Step {
        // clone: each step's names, a handful a place
        let (part, at) = (self.part.clone(), self.at.clone());
        #[cfg(test)]
        if self.plant == Plant::RenamesBeforeTheSync {
            return match self.stage {
                Stage::Write => {
                    self.stage = Stage::Rename;
                    Step::Write { file: part, bytes: self.bytes.take().expect("written once") }
                }
                Stage::Rename => {
                    self.stage = Stage::Sync;
                    Step::Rename { from: part, to: at }
                }
                Stage::Sync => {
                    self.stage = Stage::SyncDir;
                    Step::Sync { file: at }
                }
                Stage::SyncDir => {
                    self.stage = Stage::Done;
                    Step::SyncDir { dir: dir_of(&at) }
                }
                Stage::Done => Step::Done,
            };
        }
        match self.stage {
            Stage::Write => {
                self.stage = Stage::Sync;
                Step::Write { file: part, bytes: self.bytes.take().expect("written once") }
            }
            Stage::Sync => {
                self.stage = Stage::Rename;
                Step::Sync { file: part }
            }
            Stage::Rename => {
                self.stage = Stage::SyncDir;
                Step::Rename { from: part, to: at }
            }
            Stage::SyncDir => {
                self.stage = Stage::Done;
                Step::SyncDir { dir: dir_of(&at) }
            }
            Stage::Done => Step::Done,
        }
    }
}

fn dir_of(at: &std::path::Path) -> PathBuf {
    at.parent().map_or_else(PathBuf::new, std::path::Path::to_path_buf)
}

#[cfg(test)]
mod tests;
