//! The record of one fetched unit: its files in one byte string, written once and read in
//! place ([`PackView`]). Little-endian:
//!
//! ```text
//! 0..8     b"BLZFPK03"
//! 8        kind: 1 a registry crate, 2 a git commit, 3 a tree of blaze's toolchain
//! 9..12    zero
//! 12..16   files n
//! 16..24   bytes of paths
//! 24..32   bytes of data
//! 32..64   the source's digest: the `.crate`'s SHA-256, or the commit id and 12 zeros
//! 64..96   the BLAKE3 of the files' `blaze tree 2` listing (crate::tree): the tree's pin
//! 96..     the data, each file's bytes in the archive's order
//! then     the paths
//! then     n rows of 56: path offset u32, path length u16, kind u8, zero u8, data offset u64,
//!          data length u64, the file's BLAKE3; sorted by path bytes, each path once
//! last 32  the BLAKE3 of every byte before it
//! ```
//!
//! The data comes first so a file's bytes are written once, straight into the record as the
//! archive gives them ([`Builder::append`], [`Builder::slot`]): nothing holds a second copy.
//! The paths and rows, small, follow when every file is known.
//!
//! A path is relative to the unit's folder (a crate's `name-version/` stripped), and holds to
//! the listing's rules: no empty, `.`, `..` or `.git` part, and no two paths (or the folders
//! they are in) one name on a case- and normalization-insensitive disk. A link's data is its
//! target, which stays inside the folder. Two builders of one key write equal bytes: the rows
//! are sorted, and the data is in the archive's order, which the key's digest fixes.

#![blaze::sans_io]

use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_text::{Put, PutDisplay as _, Show, Writes as _};

use crate::digest::Blake3;
pub use crate::tree::Kind as Mode;
use crate::tree::{self, TreeRefused};

pub const MAGIC: &[u8; 8] = b"BLZFPK03";
pub const HEADER: usize = 96;
pub const ROW: usize = 56;
pub const FOOTER: usize = 32;

/// What kind of unit a record holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Crate,
    Git,
    /// A tree of blaze's toolchain (docs/v2/plan/design/toolchain.md): its source digest is
    /// its own listing's.
    Tree,
}

impl Kind {
    fn byte(self) -> u8 {
        match self {
            Kind::Crate => 1,
            Kind::Git => 2,
            Kind::Tree => 3,
        }
    }
}

/// What a record may hold (rule 18): the caller's choice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    pub most_files: u32,
    /// Bytes of all files' data.
    pub most_bytes: u64,
    /// Bytes of one path.
    pub most_path: u16,
}

/// Why a record was not made, or bytes are not a record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PackRefused {
    FilesOver { most: u32 },
    BytesOver { most: u64 },
    PathOver { most: u16 },
    /// The listing's rules ([`crate::tree`]).
    Tree(TreeRefused),
    /// A link whose target leaves the folder.
    LinkOut { path: alloc::string::String },
    Alloc(AllocFailure),
    /// Bytes that are not a record: the part that is wrong.
    Form { why: &'static str },
    /// The record's BLAKE3 is not its bytes'.
    Digest,
    /// A record of other sources than the ones asked for (its kind, SHA-256 or commit).
    NotTheSource,
    /// A record past the room reserved for it ([`Fixed`]): the bound it was reserved at was
    /// wrong, which is blaze's bug, refused rather than written past.
    RoomOver { room: u64 },
}

impl Show for PackRefused {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            PackRefused::FilesOver { most } => out.str("more than ").int(*most).str(" files"),
            PackRefused::BytesOver { most } => out.str("more than ").byte_count(*most).str(" of files"),
            PackRefused::PathOver { most } => out.str("a path longer than ").int(*most).str(" bytes"),
            PackRefused::Tree(why) => out.put_display(why),
            PackRefused::LinkOut { path } => out.str("the link ").str(path).str(" points outside its folder"),
            PackRefused::Alloc(failure) => out.put_display(failure),
            PackRefused::Form { why } => out.str("not a record: ").str(why),
            PackRefused::Digest => out.str("a record whose BLAKE3 is not its bytes'"),
            PackRefused::NotTheSource => out.str("a record of other sources than the lock's"),
            PackRefused::RoomOver { room } => {
                out.str("a record past the ").byte_count(*room).str(" reserved for it (blaze's bound is wrong)")
            }
        };
    }
}

#[derive(Clone, Copy)]
struct Row {
    path_at: u32,
    path_len: u16,
    mode: Mode,
    data_at: u64,
    data_len: u64,
    /// The file's BLAKE3, written at [`Builder::finish`].
    blake3: [u8; 32],
}

/// Where a record's bytes go as it is made: the heap (a `Vec`, stored by `Put`) or a file's
/// writable mapping ([`Fixed`], a reservation written in place). The strategy is this type
/// parameter alone: a record's bytes are equal whichever made them (rule 25; the test
/// `a_record_is_the_same_bytes_in_the_heap_or_in_place`).
pub trait Sink {
    fn len(&self) -> usize;
    fn bytes(&self) -> &[u8];
    fn bytes_mut(&mut self) -> &mut [u8];
    /// Room for `more` bytes past the end, the first ask sized exactly: an allocation for the
    /// heap, a check against the reservation in place.
    fn room(&mut self, more: usize, exact: bool) -> Result<(), PackRefused>;
    /// `bytes` past the end, within the room asked.
    fn extend(&mut self, bytes: &[u8]);
    /// `n` zero bytes past the end, within the room asked.
    fn zeros(&mut self, n: usize);
}

impl Sink for Vec<u8, Alloc<'_>> {
    fn len(&self) -> usize {
        Vec::len(self)
    }

    fn bytes(&self) -> &[u8] {
        self
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        self
    }

    fn room(&mut self, more: usize, exact: bool) -> Result<(), PackRefused> {
        match exact {
            true => self.try_reserve_exact(more),
            false => self.try_reserve(more),
        }
        .map_err(PackRefused::Alloc)
    }

    fn extend(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }

    fn zeros(&mut self, n: usize) {
        self.resize(Vec::len(self) + n, 0);
    }
}

/// A record made in place, in the bytes of a reservation (the store's writable mapping of the
/// file that will hold it): never past them, so nothing is allocated for its bytes.
pub struct Fixed<'m> {
    buf: &'m mut [u8],
    len: usize,
}

impl<'m> Fixed<'m> {
    pub fn new(buf: &'m mut [u8]) -> Fixed<'m> {
        Fixed { buf, len: 0 }
    }

    /// The record's length: what the reservation is cut to when it is finished.
    pub fn written(&self) -> u64 {
        self.len as u64
    }
}

impl Sink for Fixed<'_> {
    fn len(&self) -> usize {
        self.len
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.buf[..self.len]
    }

    fn room(&mut self, more: usize, _exact: bool) -> Result<(), PackRefused> {
        match self.len.checked_add(more) {
            Some(end) if end <= self.buf.len() => Ok(()),
            _ => Err(PackRefused::RoomOver { room: self.buf.len() as u64 }),
        }
    }

    fn extend(&mut self, bytes: &[u8]) {
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    fn zeros(&mut self, n: usize) {
        // A reservation starts as zeros, but these bytes are written whatever they held.
        self.buf[self.len..self.len + n].fill(0);
        self.len += n;
    }
}

/// A record being made: files added in the archive's order, their bytes written straight into
/// the record, then [`Builder::finish`].
pub struct Builder<'a, S: Sink = Vec<u8, Alloc<'a>>> {
    kind: Kind,
    source: [u8; 32],
    limits: Limits,
    rows: Vec<Row, Alloc<'a>>,
    paths: Vec<u8, Alloc<'a>>,
    /// The record: its header's room, then the data so far.
    out: S,
}

impl<'a> Builder<'a> {
    /// A builder whose record has room for `data` bytes of files reserved at once (the caller's
    /// best knowledge, held to the limit: a git tree's exact sum, a gzip's stated size). More
    /// is reserved as it comes, never more than the limit.
    pub fn new(kind: Kind, source: [u8; 32], limits: Limits, data: u64, alloc: Alloc<'a>) -> Result<Builder<'a>, PackRefused> {
        Builder::with_sink(kind, source, limits, data, Vec::new_in(alloc), alloc)
    }
}

impl<'a, S: Sink> Builder<'a, S> {
    /// A builder writing into `out` (empty), with room for `data` bytes of files asked at once
    /// as [`Builder::new`] does; the rows and paths are kept in `alloc` until
    /// [`Builder::finish`] writes them after the data.
    pub fn with_sink(kind: Kind, source: [u8; 32], limits: Limits, data: u64, mut out: S, alloc: Alloc<'a>) -> Result<Builder<'a, S>, PackRefused> {
        let room = if crate::planted(crate::Plant::SizeFromWire) { data } else { data.min(limits.most_bytes) };
        let room = usize::try_from(room).map_err(|_| PackRefused::BytesOver { most: limits.most_bytes })?;
        out.room(HEADER + room, true)?;
        out.zeros(HEADER);
        Ok(Builder { kind, source, limits, rows: Vec::new_in(alloc), paths: Vec::new_in(alloc), out })
    }

    fn data_len(&self) -> u64 {
        (self.out.len() - HEADER) as u64
    }

    /// Starts a file at `path` of `size` bytes, as its archive declares it: the size is held to
    /// what is left of the limit before anything is reserved for it.
    pub fn begin(&mut self, path: &str, mode: Mode, size: u64) -> Result<(), PackRefused> {
        if self.rows.len() as u64 >= u64::from(self.limits.most_files) {
            return Err(PackRefused::FilesOver { most: self.limits.most_files });
        }
        let Ok(path_len) = u16::try_from(path.len()) else {
            return Err(PackRefused::PathOver { most: self.limits.most_path });
        };
        if path_len > self.limits.most_path {
            return Err(PackRefused::PathOver { most: self.limits.most_path });
        }
        let left = self.limits.most_bytes.saturating_sub(self.data_len());
        if size > left && !crate::planted(crate::Plant::SizeFromWire) {
            return Err(PackRefused::BytesOver { most: self.limits.most_bytes });
        }
        let size = usize::try_from(size).map_err(|_| PackRefused::BytesOver { most: self.limits.most_bytes })?;
        self.out.room(size, false)?;
        self.paths.try_reserve(path.len()).map_err(PackRefused::Alloc)?;
        self.rows.try_reserve(1).map_err(PackRefused::Alloc)?;
        let row = Row { path_at: self.paths.len() as u32, path_len, mode, data_at: self.data_len(), data_len: 0, blake3: [0; 32] };
        self.paths.extend_from_slice(path.as_bytes());
        self.rows.push(row);
        Ok(())
    }

    /// The next bytes of the file begun last.
    pub fn append(&mut self, bytes: &[u8]) -> Result<(), PackRefused> {
        let left = self.limits.most_bytes.saturating_sub(self.data_len());
        if bytes.len() as u64 > left {
            return Err(PackRefused::BytesOver { most: self.limits.most_bytes });
        }
        let Some(row) = self.rows.last_mut() else {
            return Err(PackRefused::Form { why: "file bytes before any file" });
        };
        self.out.room(bytes.len(), false)?;
        self.out.extend(bytes);
        row.data_len += bytes.len() as u64;
        Ok(())
    }

    /// A whole file.
    pub fn file(&mut self, path: &str, mode: Mode, bytes: &[u8]) -> Result<(), PackRefused> {
        self.begin(path, mode, bytes.len() as u64)?;
        self.append(bytes)
    }

    /// A file of `size` bytes, zeroed, for the caller to fill in place: its bytes are written
    /// once, into the record.
    pub fn slot(&mut self, path: &str, mode: Mode, size: u64) -> Result<&mut [u8], PackRefused> {
        self.begin(path, mode, size)?;
        let at = self.out.len();
        let size = size as usize;
        self.out.zeros(size);
        self.rows.last_mut().expect("begun above").data_len = size as u64;
        Ok(&mut self.out.bytes_mut()[at..])
    }

    /// Files added so far, and their bytes.
    pub fn counts(&self) -> (u32, u64) {
        (self.rows.len() as u32, self.data_len())
    }

    /// The record: every path checked by the listing's rules and every link's target held
    /// inside, the rows sorted, the listing's and the record's digests written.
    pub fn finish(mut self) -> Result<S, PackRefused> {
        let data = &self.out.bytes()[HEADER..];
        // The listing's rules and its digest, over the files as the tree module knows them.
        let mut entries: alloc::vec::Vec<tree::Entry> = alloc::vec::Vec::with_capacity(self.rows.len()); // alloc: the listing's own form, once a record
        for row in self.rows.iter_mut() {
            let path = path_of(&self.paths, row);
            let bytes = data_of(data, row);
            if row.mode == Mode::Link && !link_inside(path, bytes) {
                return Err(PackRefused::LinkOut { path: path.into() });
            }
            row.blake3 = Blake3::of(bytes).0;
            entries.push(tree::Entry { path: path.into(), kind: row.mode, blake3: Blake3(row.blake3), len: row.data_len });
        }
        let listing = tree::listing(&mut entries).map_err(PackRefused::Tree)?;
        let tree_digest = Blake3::of(&listing).0;
        drop(entries);
        drop(listing);
        let paths = &self.paths;
        self.rows.sort_unstable_by(|a, b| path_of(paths, a).as_bytes().cmp(path_of(paths, b).as_bytes()));
        let data_len = self.data_len();
        self.out.room(self.paths.len() + self.rows.len() * ROW + FOOTER, true)?;
        self.out.extend(&self.paths);
        for row in self.rows.iter() {
            self.out.extend(&row.path_at.to_le_bytes());
            self.out.extend(&row.path_len.to_le_bytes());
            self.out.extend(&[mode_byte(row.mode), 0]);
            self.out.extend(&row.data_at.to_le_bytes());
            self.out.extend(&row.data_len.to_le_bytes());
            self.out.extend(&row.blake3);
        }
        let header = &mut self.out.bytes_mut()[..HEADER];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&[self.kind.byte(), 0, 0, 0]);
        header[12..16].copy_from_slice(&(self.rows.len() as u32).to_le_bytes());
        header[16..24].copy_from_slice(&(self.paths.len() as u64).to_le_bytes());
        header[24..32].copy_from_slice(&data_len.to_le_bytes());
        header[32..64].copy_from_slice(&self.source);
        header[64..96].copy_from_slice(&tree_digest);
        let digest = Blake3::of(self.out.bytes()).0;
        self.out.extend(&digest);
        Ok(self.out)
    }
}

fn path_of<'p>(paths: &'p [u8], row: &Row) -> &'p str {
    let at = row.path_at as usize;
    // Built from `&str`s, and cut only at their ends.
    core::str::from_utf8(&paths[at..at + usize::from(row.path_len)]).expect("paths are joined `str`s")
}

fn data_of<'d>(data: &'d [u8], row: &Row) -> &'d [u8] {
    &data[row.data_at as usize..(row.data_at + row.data_len) as usize]
}

fn mode_byte(mode: Mode) -> u8 {
    match mode {
        Mode::File => 0,
        Mode::Exec => 1,
        Mode::Link => 2,
    }
}

/// Whether a link at `path` to `target` (relative, `/`-separated) stays inside the folder:
/// resolved part by part from the link's own folder, never above the top. An absolute target
/// or one with a `\` or a NUL is outside.
pub(crate) fn link_inside(path: &str, target: &[u8]) -> bool {
    let Ok(target) = core::str::from_utf8(target) else { return false };
    if target.is_empty() || target.starts_with('/') || target.contains('\\') || target.contains('\0') {
        return false;
    }
    // Depth below the top of the link's folder.
    let mut depth = path.matches('/').count() as i64;
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => depth += 1,
        }
    }
    true
}

/// A record read in place, its form and digest checked once ([`PackView::open`]).
#[derive(Clone, Copy)]
pub struct PackView<'b> {
    bytes: &'b [u8],
    n: usize,
    paths_at: usize,
    rows_at: usize,
    data_len: u64,
}

/// One file of a record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct File<'b> {
    pub path: &'b str,
    pub mode: Mode,
    /// A borrowed slice of the record: its length is the file's size.
    pub bytes: &'b [u8],
    /// The file's BLAKE3, as the record states it (covered by the record's digest).
    pub blake3: Blake3,
}

impl<'b> PackView<'b> {
    /// `bytes` as a record: its length, its digest, every row's ranges and its order checked.
    pub fn open(bytes: &'b [u8]) -> Result<PackView<'b>, PackRefused> {
        let view = PackView::form(bytes)?;
        let body = bytes.len() - FOOTER;
        if Blake3::of(&bytes[..body]).0 != bytes[body..] {
            return Err(PackRefused::Digest);
        }
        view.rows_checked()?;
        Ok(view)
    }

    /// The reader of a stored source record: `bytes` (the mapped record found at `id.key()`)
    /// opened as [`PackView::open`] does, once, and refused unless it holds the sources `id`
    /// names (its kind and the `.crate`'s SHA-256 or the commit id). After it, every
    /// [`PackView::files`] row (sorted, with its size and BLAKE3) and every file's bytes are
    /// read in place, borrowed from `bytes`, with no check again.
    pub fn open_source(id: &crate::SourceId<'_>, bytes: &'b [u8]) -> Result<PackView<'b>, PackRefused> {
        PackView::open(bytes)?.of_source(id)
    }

    /// [`PackView::open_source`] of a record whose digest was checked when this machine
    /// fetched and put it (a record from elsewhere, the shared cache, takes `open_source`):
    /// its form, rows and source checked, its bytes not hashed again (the memory rule: bytes
    /// verified once per fetch, never on each read).
    pub fn open_source_put(id: &crate::SourceId<'_>, bytes: &'b [u8]) -> Result<PackView<'b>, PackRefused> {
        PackView::open_verified(bytes)?.of_source(id)
    }

    fn of_source(self, id: &crate::SourceId<'_>) -> Result<PackView<'b>, PackRefused> {
        let view = self;
        let (kind, source) = match id {
            crate::SourceId::Crate { sha256, .. } => (Kind::Crate, *sha256),
            crate::SourceId::Git { commit } => {
                let mut source = [0; 32];
                source[..20].copy_from_slice(&commit.0);
                (Kind::Git, source)
            }
        };
        if view.kind() != kind || *view.source() != source {
            return Err(PackRefused::NotTheSource);
        }
        Ok(view)
    }

    /// `bytes` as a record whose digest was checked when it was fetched or put (the store's
    /// key binds it): its lengths, rows and order checked, its digest not computed again (the
    /// memory-and-data rule: bytes verified once per fetch, never on each read).
    pub fn open_verified(bytes: &'b [u8]) -> Result<PackView<'b>, PackRefused> {
        let view = PackView::form(bytes)?;
        view.rows_checked()?;
        Ok(view)
    }

    fn form(bytes: &'b [u8]) -> Result<PackView<'b>, PackRefused> {
        let form = |why| PackRefused::Form { why };
        if bytes.len() < HEADER + FOOTER || &bytes[..8] != MAGIC {
            return Err(form("no header"));
        }
        if !matches!(bytes[8], 1..=3) || bytes[9..12] != [0, 0, 0] {
            return Err(form("an unknown kind"));
        }
        let n = u32::from_le_bytes(bytes[12..16].try_into().expect("4 bytes")) as u64;
        let paths = u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes"));
        let data = u64::from_le_bytes(bytes[24..32].try_into().expect("8 bytes"));
        let total = (HEADER as u64 + FOOTER as u64)
            .checked_add(n.checked_mul(ROW as u64).ok_or(form("lengths overflow"))?)
            .and_then(|it| it.checked_add(paths))
            .and_then(|it| it.checked_add(data))
            .ok_or(form("lengths overflow"))?;
        if total != bytes.len() as u64 {
            return Err(form("lengths that do not add up to its size"));
        }
        let paths_at = HEADER + data as usize;
        let rows_at = paths_at + paths as usize;
        Ok(PackView { bytes, n: n as usize, paths_at, rows_at, data_len: data })
    }

    fn rows_checked(&self) -> Result<(), PackRefused> {
        let form = |why| PackRefused::Form { why };
        let paths = (self.rows_at - self.paths_at) as u64;
        let mut last: Option<&str> = None;
        for at in 0..self.n {
            let row = self.row(at);
            if row.path_at as u64 + u64::from(row.path_len) > paths
                || row.data_at.checked_add(row.data_len).is_none_or(|end| end > self.data_len)
            {
                return Err(form("a row outside its record"));
            }
            let start = self.paths_at + row.path_at as usize;
            let path = core::str::from_utf8(&self.bytes[start..start + usize::from(row.path_len)])
                .map_err(|_| form("a path that is not UTF-8"))?;
            if last.is_some_and(|last| last.as_bytes() >= path.as_bytes()) {
                return Err(form("rows out of order or twice"));
            }
            let r = self.rows_at + at * ROW;
            if self.bytes[r + 6] > 2 || self.bytes[r + 7] != 0 {
                return Err(form("an unknown mode"));
            }
            last = Some(path);
        }
        Ok(())
    }

    fn row(&self, at: usize) -> Row {
        let r = &self.bytes[self.rows_at + at * ROW..self.rows_at + (at + 1) * ROW];
        Row {
            path_at: u32::from_le_bytes(r[0..4].try_into().expect("4 bytes")),
            path_len: u16::from_le_bytes(r[4..6].try_into().expect("2 bytes")),
            mode: match r[6] {
                0 => Mode::File,
                1 => Mode::Exec,
                _ => Mode::Link,
            },
            data_at: u64::from_le_bytes(r[8..16].try_into().expect("8 bytes")),
            data_len: u64::from_le_bytes(r[16..24].try_into().expect("8 bytes")),
            blake3: r[24..56].try_into().expect("32 bytes"),
        }
    }

    pub fn kind(&self) -> Kind {
        match self.bytes[8] {
            1 => Kind::Crate,
            2 => Kind::Git,
            _ => Kind::Tree,
        }
    }

    /// The `.crate`'s SHA-256, or the commit id followed by 12 zeros.
    pub fn source(&self) -> &'b [u8; 32] {
        self.bytes[32..64].try_into().expect("32 bytes")
    }

    /// The BLAKE3 of the files' listing (crate::tree): what a source pin is over.
    pub fn tree(&self) -> Blake3 {
        Blake3(self.bytes[64..96].try_into().expect("32 bytes"))
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    pub fn file(&self, at: usize) -> File<'b> {
        let row = self.row(at);
        let start = self.paths_at + row.path_at as usize;
        let path = core::str::from_utf8(&self.bytes[start..start + usize::from(row.path_len)]).expect("checked at open");
        let data = HEADER + row.data_at as usize;
        File { path, mode: row.mode, bytes: &self.bytes[data..data + row.data_len as usize], blake3: Blake3(row.blake3) }
    }

    pub fn files(&self) -> impl Iterator<Item = File<'b>> + '_ {
        (0..self.n).map(|at| self.file(at))
    }

    /// The file at `path`, by halving.
    pub fn find(&self, path: &str) -> Option<File<'b>> {
        let (mut low, mut high) = (0, self.n);
        while low < high {
            let mid = (low + high) / 2;
            let file = self.file(mid);
            match file.path.as_bytes().cmp(path.as_bytes()) {
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
                core::cmp::Ordering::Equal => return Some(file),
            }
        }
        None
    }

    /// Bytes of all files.
    pub fn data_len(&self) -> u64 {
        self.data_len
    }
}
