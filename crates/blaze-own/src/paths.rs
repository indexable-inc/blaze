//! Paths as a table: directories as (parent, name) rows and files as (directory, name) rows,
//! each name stored once for its directory, in place of `Vec<String>` or `Vec<PathBuf>` of
//! whole paths, which store every directory's name again in every path below it. The owner
//! asked for it on 2026-11-15 ("represent paths more efficiently, like fff").
//!
//! # Intended use: resolve the directory once, then names within it
//!
//! A lookup from a whole path's text ([`Paths::find_path`], [`Paths::intern_path`],
//! [`PathsView::find_path`]) pays one probe (or, sealed, one binary search) per component, so
//! it costs the path's depth times one probe. Measured on reth's tree (2026-11-15,
//! `alloc_cost paths`): one probe is 16.0 ns at 1,371 paths and 46.7 ns at 1e6, and paths have
//! 5.4 and 6.4 components, so `find_path` takes 89 and 529 ns, against 17 and 136 for a
//! `HashSet<&str>` of whole paths. The callers this is for hold a directory already (a sweep
//! walks directories; a module's files sit beside it): they keep the [`DirId`] and ask
//! [`Paths::find`] or [`Paths::intern`] for names in it, one probe each, 46.7 ns at 1e6. No
//! whole-path index is kept, by decision: it would cost 5 to 10 bytes a path for callers that
//! should not be resolving from the root.
//!
//! A file is a [`PathId`], a directory a [`DirId`] (the root is [`DirId::ROOT`]): 4 bytes each,
//! compared and hashed as integers. The text of a path is made only when printed
//! ([`Paths::write`] into a sink of `&str` pieces).
//!
//! Two forms:
//!
//! - [`Paths`], to build: `intern` finds or adds a row through an open-addressing table on
//!   (kind, parent, name), so interning a path that is there allocates nothing.
//! - [`SealedPaths`], made once by [`Paths::seal_in`]: directories numbered breadth first with
//!   each directory's subdirectories, and then its files, in one run sorted by name. Children
//!   in name order are a range, `find` is a binary search in the run, and there is no table. Its
//!   columns ([`PathColumns`]: plain `u32` and byte arrays) can be stored as they are and read
//!   in place through a [`PathsView`], checked once when made ([`PathsView::new`]).

use core::{alloc::Allocator, cmp::Ordering, fmt, ops::Range, str};

use crate::{
    error::{AllocFailure, fail},
    lists::Lists,
    texts::Texts,
    vec::Vec,
};

/// A directory of a [`Paths`] or [`SealedPaths`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DirId(pub u32);

impl DirId {
    /// The root, the directory every relative path starts from; its name is empty.
    pub const ROOT: DirId = DirId(0);
}

/// A file of a [`Paths`] or [`SealedPaths`]: two equal paths have equal ids, in one table.
/// Ids are in the order interned (or, sealed, breadth first), not the paths' text order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PathId(pub u32);

/// The table entry's bit saying it names a file.
const FILE: u32 = 1 << 31;
/// An empty table slot.
const EMPTY: u32 = u32::MAX;
/// The most ids of a kind: 31 bits, less the empty slot's value.
const MAX_IDS: usize = (FILE - 1) as usize;
/// What a refusal names.
const WHAT: &str = "Paths rows (ids are 31 bits)";

/// Checks a name: not empty, no `/`.
#[track_caller]
fn check_name(name: &str) {
    assert!(!name.is_empty() && !name.contains('/'), "Paths: a name is not empty and holds no `/`: {name:?}");
}

/// The table's hash of a row's key: its kind, its parent and its name, a word at a time.
fn key_hash(file: bool, parent: u32, name: &[u8]) -> u64 {
    const K: u64 = 0xf135_7aea_2e62_a9c5;
    let mut h = ((u64::from(parent) << 1) | u64::from(file)).wrapping_mul(K);
    let mut words = name.chunks_exact(8);
    for word in &mut words {
        let mut w = [0u8; 8];
        w.copy_from_slice(word);
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(K).rotate_left(26);
    }
    let rest = words.remainder();
    let mut w = [0u8; 8];
    w[..rest.len()].copy_from_slice(rest);
    h = (h ^ u64::from_le_bytes(w) ^ ((name.len() as u64) << 56)).wrapping_mul(K);
    h ^ (h >> 29)
}

/// Writes directory `d` (its ancestors first, joined by `/`), reading rows through `parent`
/// and `name`; nothing for the root.
fn write_dir<'n>(d: u32, parent: &impl Fn(u32) -> u32, name: &impl Fn(u32) -> &'n str, put: &mut impl FnMut(&str)) {
    if d == 0 {
        return;
    }
    const CHUNK: usize = 128;
    let mut buf = [0u32; CHUNK];
    let mut cur = d;
    let mut len = 0usize;
    for slot in &mut buf {
        if cur == 0 {
            break;
        }
        *slot = cur;
        len += 1;
        cur = parent(cur);
    }
    if cur == 0 {
        for (i, &id) in buf.iter().take(len).rev().enumerate() {
            if i > 0 {
                put("/");
            }
            put(name(id));
        }
        return;
    }
    let mut depth = len;
    while cur != 0 {
        depth += 1;
        cur = parent(cur);
    }
    let mut emitted = 0usize;
    while emitted < depth {
        let take = (depth - emitted).min(CHUNK);
        let skip = depth - emitted - take;
        let mut cur = d;
        for _ in 0..skip {
            cur = parent(cur);
        }
        for slot in buf.iter_mut().take(take) {
            *slot = cur;
            cur = parent(cur);
        }
        for (i, &id) in buf.iter().take(take).rev().enumerate() {
            if emitted > 0 || i > 0 {
                put("/");
            }
            put(name(id));
        }
        emitted += take;
    }
}

// ---------------------------------------------------------------------------------------------
// Paths: the form to build.

/// Directories and files as rows, interned through a table ([module](self)): each name stored once for its directory, a lookup one probe per component.
pub struct Paths<A: Allocator + Clone> {
    /// Directory `d`'s parent; the root's is itself.
    dir_parent: Vec<u32, A>,
    /// Directory `d`'s name; the root's is empty.
    dir_names: Texts<A>,
    /// File `f`'s directory.
    file_dir: Vec<u32, A>,
    file_names: Texts<A>,
    /// Open addressing, linear probing, at most three quarters full: [`EMPTY`], or a
    /// directory's id, or a file's with [`FILE`] set. The root is not in it.
    table: Vec<u32, A>,
    alloc: A,
}

impl<A: Allocator + Clone> Paths<A> {
    /// The root alone.
    #[track_caller]
    pub fn new_in(alloc: A) -> Paths<A> {
        let mut dir_parent = Vec::new_in(alloc.clone());
        dir_parent.push(0);
        let mut dir_names = Texts::new_in(alloc.clone());
        dir_names.push("");
        Paths {
            dir_parent,
            dir_names,
            file_dir: Vec::new_in(alloc.clone()),
            file_names: Texts::new_in(alloc.clone()),
            table: Vec::new_in(alloc.clone()),
            alloc,
        }
    }

    /// The number of directories, the root among them.
    pub fn dirs(&self) -> usize {
        self.dir_parent.len()
    }

    /// The number of files.
    pub fn len(&self) -> usize {
        self.file_dir.len()
    }

    pub fn is_empty(&self) -> bool {
        self.file_dir.is_empty()
    }

    /// Directory `d`'s parent; `None` for the root.
    #[track_caller]
    pub fn dir_parent(&self, d: DirId) -> Option<DirId> {
        (d != DirId::ROOT).then(|| DirId(self.dir_parent[d.0 as usize]))
    }

    #[track_caller]
    pub fn dir_name(&self, d: DirId) -> &str {
        self.dir_names.text(d.0 as usize)
    }

    /// File `p`'s directory.
    #[track_caller]
    pub fn parent(&self, p: PathId) -> DirId {
        DirId(self.file_dir[p.0 as usize])
    }

    /// File `p`'s own name, its last component.
    #[track_caller]
    pub fn name(&self, p: PathId) -> &str {
        self.file_names.text(p.0 as usize)
    }

    fn matches(&self, entry: u32, file: bool, parent: u32, name: &[u8]) -> bool {
        if (entry & FILE != 0) != file {
            return false;
        }
        let id = (entry & !FILE) as usize;
        if file {
            self.file_dir[id] == parent && self.file_names.text(id).as_bytes() == name
        } else {
            self.dir_parent[id] == parent && self.dir_names.text(id).as_bytes() == name
        }
    }

    /// The id under the key, or the empty slot it would take. The table is not empty.
    fn probe(&self, file: bool, parent: u32, name: &[u8]) -> Result<u32, usize> {
        let mask = self.table.len() - 1;
        let mut at = key_hash(file, parent, name) as usize & mask;
        loop {
            let entry = self.table[at];
            if entry == EMPTY {
                return Err(at);
            }
            if self.matches(entry, file, parent, name) {
                return Ok(entry & !FILE);
            }
            at = (at + 1) & mask;
        }
    }

    fn find_entry(&self, file: bool, parent: u32, name: &str) -> Option<u32> {
        if self.table.is_empty() {
            return None;
        }
        self.probe(file, parent, name.as_bytes()).ok()
    }

    /// Room in the table for one more key: doubles it past three quarters, re-placing every
    /// row by its stored key.
    #[track_caller]
    fn make_room(&mut self) {
        let keys = self.dir_parent.len() - 1 + self.file_dir.len();
        if (keys + 1) * 4 <= self.table.len() * 3 {
            return;
        }
        let len = (self.table.len() * 2).max(16);
        let mut table = Vec::from_elem_in(EMPTY, len, self.alloc.clone());
        let mask = len - 1;
        let mut place = |file: bool, parent: u32, name: &str, entry: u32| {
            let mut at = key_hash(file, parent, name.as_bytes()) as usize & mask;
            while table[at] != EMPTY {
                at = (at + 1) & mask;
            }
            table[at] = entry;
        };
        for d in 1..self.dir_parent.len() {
            place(false, self.dir_parent[d], self.dir_names.text(d), d as u32);
        }
        for f in 0..self.file_dir.len() {
            place(true, self.file_dir[f], self.file_names.text(f), f as u32 | FILE);
        }
        self.table = table;
    }

    /// The subdirectory `name` of `parent`, added if new.
    ///
    /// # Panics
    ///
    /// When `name` is empty or holds a `/`, `parent` is not a directory here, or there are
    /// `2^31 - 1` directories already.
    #[track_caller]
    pub fn intern_dir(&mut self, parent: DirId, name: &str) -> DirId {
        check_name(name);
        assert!((parent.0 as usize) < self.dirs(), "Paths::intern_dir: no directory {parent:?}");
        self.make_room();
        match self.probe(false, parent.0, name.as_bytes()) {
            Ok(id) => DirId(id),
            Err(slot) => {
                let id = self.dir_parent.len();
                if id >= MAX_IDS {
                    fail(AllocFailure::CapacityOverflow { what: WHAT, len: id, additional: 1 });
                }
                self.dir_parent.push(parent.0);
                self.dir_names.push(name);
                self.table[slot] = id as u32;
                DirId(id as u32)
            }
        }
    }

    /// The file `name` in `dir`, added if new.
    ///
    /// # Panics
    ///
    /// As [`Paths::intern_dir`].
    #[track_caller]
    pub fn intern(&mut self, dir: DirId, name: &str) -> PathId {
        check_name(name);
        assert!((dir.0 as usize) < self.dirs(), "Paths::intern: no directory {dir:?}");
        self.make_room();
        match self.probe(true, dir.0, name.as_bytes()) {
            Ok(id) => PathId(id),
            Err(slot) => {
                let id = self.file_dir.len();
                if id >= MAX_IDS {
                    fail(AllocFailure::CapacityOverflow { what: WHAT, len: id, additional: 1 });
                }
                self.file_dir.push(dir.0);
                self.file_names.push(name);
                self.table[slot] = id as u32 | FILE;
                PathId(id as u32)
            }
        }
    }

    /// The file at `path`, relative to the root and `/`-separated, with its directories,
    /// added if new.
    ///
    /// # Panics
    ///
    /// When a component is empty (`""`, `"a//b"`, `"/a"`, `"a/"`), or as [`Paths::intern`].
    #[track_caller]
    pub fn intern_path(&mut self, path: &str) -> PathId {
        let (dirs, file) = match path.rsplit_once('/') {
            Some((dirs, file)) => (Some(dirs), file),
            None => (None, path),
        };
        let mut dir = DirId::ROOT;
        if let Some(dirs) = dirs {
            for name in dirs.split('/') {
                dir = self.intern_dir(dir, name);
            }
        }
        self.intern(dir, file)
    }

    pub fn find_dir(&self, parent: DirId, name: &str) -> Option<DirId> {
        self.find_entry(false, parent.0, name).map(DirId)
    }

    pub fn find(&self, dir: DirId, name: &str) -> Option<PathId> {
        self.find_entry(true, dir.0, name).map(PathId)
    }

    /// The file at `path` (as [`Paths::intern_path`] takes it), if it is here.
    pub fn find_path(&self, path: &str) -> Option<PathId> {
        let (dirs, file) = match path.rsplit_once('/') {
            Some((dirs, file)) => (Some(dirs), file),
            None => (None, path),
        };
        let mut dir = DirId::ROOT;
        if let Some(dirs) = dirs {
            for name in dirs.split('/') {
                dir = self.find_dir(dir, name)?;
            }
        }
        self.find(dir, file)
    }

    /// Writes directory `d`'s path into `put`, piece by piece; nothing for the root.
    pub fn write_dir(&self, d: DirId, put: &mut impl FnMut(&str)) {
        write_dir(d.0, &|d| self.dir_parent[d as usize], &|d| self.dir_names.text(d as usize), put);
    }

    /// Writes file `p`'s path into `put`, piece by piece: the text exists only in the sink.
    pub fn write(&self, p: PathId, put: &mut impl FnMut(&str)) {
        let dir = self.file_dir[p.0 as usize];
        self.write_dir(DirId(dir), put);
        if dir != 0 {
            put("/");
        }
        put(self.name(p));
    }

    /// The bytes this holds from its allocator.
    pub fn heap_bytes(&self) -> usize {
        (self.dir_parent.capacity() + self.file_dir.capacity() + self.table.capacity()) * 4
            + self.dir_names.heap_bytes()
            + self.file_names.heap_bytes()
    }

    /// The sealed form, and where each id went.
    #[track_caller]
    pub fn seal_in<B: Allocator + Clone>(&self, alloc: B) -> (SealedPaths<B>, Remap<B>) {
        let dirs = self.dirs();
        let files = self.len();
        // Each directory's subdirectories and files, each sorted by name.
        let mut pairs: Vec<(usize, u32), B> = Vec::with_capacity_in(dirs - 1, alloc.clone());
        for d in 1..dirs {
            pairs.push((self.dir_parent[d] as usize, d as u32));
        }
        let mut kids: Lists<u32, B> = Lists::from_pairs_in(dirs, pairs, alloc.clone());
        let mut pairs: Vec<(usize, u32), B> = Vec::with_capacity_in(files, alloc.clone());
        for f in 0..files {
            pairs.push((self.file_dir[f] as usize, f as u32));
        }
        let mut held: Lists<u32, B> = Lists::from_pairs_in(dirs, pairs, alloc.clone());
        for d in 0..dirs {
            kids.row_mut(d).sort_unstable_by(|&a, &b| self.dir_names.text(a as usize).cmp(self.dir_names.text(b as usize)));
            held.row_mut(d).sort_unstable_by(|&a, &b| self.file_names.text(a as usize).cmp(self.file_names.text(b as usize)));
        }
        // Breadth first: a directory's subdirectories follow those of the one before it.
        let mut order: Vec<u32, B> = Vec::with_capacity_in(dirs, alloc.clone());
        let mut new_dir: Vec<u32, B> = Vec::from_elem_in(0, dirs, alloc.clone());
        let mut dirs_end: Vec<u32, B> = Vec::with_capacity_in(dirs, alloc.clone());
        order.push(0);
        let mut i = 0;
        while i < order.len() {
            let d = order[i] as usize;
            for &c in kids.row(d) {
                new_dir[c as usize] = order.len() as u32;
                order.push(c);
            }
            dirs_end.push(order.len() as u32);
            i += 1;
        }
        assert_eq!(order.len(), dirs, "Paths::seal_in: every directory is below the root");
        let mut dir_parent: Vec<u32, B> = Vec::with_capacity_in(dirs, alloc.clone());
        let mut dir_names: Texts<B> = Texts::with_capacity_in(dirs, self.dir_names.bytes_len(), alloc.clone());
        for &old in order.iter() {
            dir_parent.push(new_dir[self.dir_parent[old as usize] as usize]);
            dir_names.push(self.dir_names.text(old as usize));
        }
        let mut new_file: Vec<u32, B> = Vec::from_elem_in(0, files, alloc.clone());
        let mut file_dir: Vec<u32, B> = Vec::with_capacity_in(files, alloc.clone());
        let mut file_names: Texts<B> = Texts::with_capacity_in(files, self.file_names.bytes_len(), alloc.clone());
        let mut files_end: Vec<u32, B> = Vec::with_capacity_in(dirs, alloc.clone());
        for (d, &old) in order.iter().enumerate() {
            for &f in held.row(old as usize) {
                new_file[f as usize] = file_dir.len() as u32;
                file_dir.push(d as u32);
                file_names.push(self.file_names.text(f as usize));
            }
            files_end.push(file_dir.len() as u32);
        }
        (
            SealedPaths { dir_parent, dirs_end, files_end, dir_names, file_dir, file_names },
            Remap { dirs: new_dir, files: new_file },
        )
    }
}

impl<A: Allocator + Clone> fmt::Debug for Paths<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Paths {{ {} dirs, {} files }}", self.dirs(), self.len())
    }
}

/// Where [`Paths::seal_in`] put each directory and file.
pub struct Remap<A: Allocator> {
    dirs: Vec<u32, A>,
    files: Vec<u32, A>,
}

impl<A: Allocator> Remap<A> {
    #[track_caller]
    pub fn dir(&self, d: DirId) -> DirId {
        DirId(self.dirs[d.0 as usize])
    }

    #[track_caller]
    pub fn path(&self, p: PathId) -> PathId {
        PathId(self.files[p.0 as usize])
    }
}

// ---------------------------------------------------------------------------------------------
// The sealed form.

/// Paths sorted into runs and frozen ([module](self)); read through [`SealedPaths::view`].
pub struct SealedPaths<A: Allocator> {
    dir_parent: Vec<u32, A>,
    dirs_end: Vec<u32, A>,
    files_end: Vec<u32, A>,
    dir_names: Texts<A>,
    file_dir: Vec<u32, A>,
    file_names: Texts<A>,
}

impl<A: Allocator> SealedPaths<A> {
    /// The reading form, with no check: these columns were made sorted.
    pub fn view(&self) -> PathsView<'_> {
        PathsView { cols: self.columns() }
    }

    /// The columns, to store as they are.
    pub fn columns(&self) -> PathColumns<'_> {
        PathColumns {
            dir_parent: &self.dir_parent,
            dirs_end: &self.dirs_end,
            files_end: &self.files_end,
            dir_name_ends: self.dir_names.ends(),
            dir_name_bytes: self.dir_names.bytes(),
            file_dir: &self.file_dir,
            file_name_ends: self.file_names.ends(),
            file_name_bytes: self.file_names.bytes(),
        }
    }

    pub fn heap_bytes(&self) -> usize {
        (self.dir_parent.capacity() + self.dirs_end.capacity() + self.files_end.capacity() + self.file_dir.capacity())
            * 4
            + self.dir_names.heap_bytes()
            + self.file_names.heap_bytes()
    }
}

/// A sealed table's columns: what is stored, and what a [`PathsView`] reads in place.
#[derive(Clone, Copy, Debug)]
pub struct PathColumns<'a> {
    /// Directory `d`'s parent (the root's is 0).
    pub dir_parent: &'a [u32],
    /// Where directory `d`'s run of subdirectories ends; it starts where `d - 1`'s ends (the
    /// root's at 1).
    pub dirs_end: &'a [u32],
    /// Where directory `d`'s run of files ends; it starts where `d - 1`'s ends (the root's
    /// at 0).
    pub files_end: &'a [u32],
    pub dir_name_ends: &'a [u32],
    pub dir_name_bytes: &'a [u8],
    pub file_dir: &'a [u32],
    pub file_name_ends: &'a [u32],
    pub file_name_bytes: &'a [u8],
}

/// Why columns are not a sealed table: the rule broken and the row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BadColumns {
    pub rule: &'static str,
    pub at: usize,
}

impl fmt::Display for BadColumns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "path columns break \"{}\" at row {}", self.rule, self.at)
    }
}

/// A sealed table read in place, its columns checked once ([`PathsView::new`]): every read
/// after is unchecked by that proof.
#[derive(Clone, Copy, Debug)]
pub struct PathsView<'a> {
    cols: PathColumns<'a>,
}

/// The run `i` of `ends`: from the end before it (or `first`) to its own.
fn run(ends: &[u32], i: usize, first: u32) -> Range<usize> {
    let start = if i == 0 { first } else { ends[i - 1] };
    start as usize..ends[i] as usize
}

/// Checks a name column: ends that rise within the bytes, each run UTF-8.
fn check_names(ends: &[u32], bytes: &[u8], rule: &'static str) -> Result<(), BadColumns> {
    let mut start = 0usize;
    for (i, &end) in ends.iter().enumerate() {
        let end = end as usize;
        if end < start || end > bytes.len() || str::from_utf8(&bytes[start..end]).is_err() {
            return Err(BadColumns { rule, at: i });
        }
        start = end;
    }
    Ok(())
}

impl<'a> PathsView<'a> {
    /// Checks `cols` is a sealed table: the lengths agree, names are UTF-8 runs, every run is
    /// its directory's and sorted by name with no repeat, and each subdirectory comes after its
    /// parent (so the tree has no cycle). `O(rows + bytes)`, once.
    pub fn new(cols: PathColumns<'a>) -> Result<PathsView<'a>, BadColumns> {
        let dirs = cols.dir_parent.len();
        let files = cols.file_dir.len();
        let bad = |rule, at| Err(BadColumns { rule, at });
        if dirs == 0 || cols.dirs_end.len() != dirs || cols.files_end.len() != dirs || cols.dir_name_ends.len() != dirs {
            return bad("every directory column has one row a directory, the root at least", 0);
        }
        if cols.file_name_ends.len() != files {
            return bad("every file column has one row a file", 0);
        }
        check_names(cols.dir_name_ends, cols.dir_name_bytes, "directory names are UTF-8 runs")?;
        check_names(cols.file_name_ends, cols.file_name_bytes, "file names are UTF-8 runs")?;
        if cols.dir_parent[0] != 0 || cols.dir_name_ends[0] != 0 {
            return bad("the root is its own parent, with an empty name", 0);
        }
        let view = PathsView { cols };
        let (mut dirs_at, mut files_at) = (1u32, 0u32);
        for d in 0..dirs {
            let (dirs_to, files_to) = (cols.dirs_end[d], cols.files_end[d]);
            if dirs_to < dirs_at || dirs_to as usize > dirs || files_to < files_at || files_to as usize > files {
                return bad("the runs rise within the rows", d);
            }
            for c in dirs_at..dirs_to {
                if cols.dir_parent[c as usize] as usize != d || c as usize <= d {
                    return bad("a subdirectory's run is its parent's, after it", c as usize);
                }
                let name = view.dir_name(DirId(c));
                if name.is_empty() || name.contains('/') || (c > dirs_at && view.dir_name(DirId(c - 1)) >= name) {
                    return bad("a run's names are sorted, unrepeated, not empty, with no `/`", c as usize);
                }
            }
            for f in files_at..files_to {
                if cols.file_dir[f as usize] as usize != d {
                    return bad("a file's run is its directory's", f as usize);
                }
                let name = view.name(PathId(f));
                if name.is_empty() || name.contains('/') || (f > files_at && view.name(PathId(f - 1)) >= name) {
                    return bad("a run's names are sorted, unrepeated, not empty, with no `/`", f as usize);
                }
            }
            (dirs_at, files_at) = (dirs_to, files_to);
        }
        if dirs_at as usize != dirs || files_at as usize != files {
            return bad("the runs cover every row", dirs);
        }
        Ok(view)
    }

    pub fn columns(&self) -> PathColumns<'a> {
        self.cols
    }

    pub fn dirs(&self) -> usize {
        self.cols.dir_parent.len()
    }

    pub fn len(&self) -> usize {
        self.cols.file_dir.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cols.file_dir.is_empty()
    }

    pub fn dir_parent(&self, d: DirId) -> Option<DirId> {
        (d != DirId::ROOT).then(|| DirId(self.cols.dir_parent[d.0 as usize]))
    }

    pub fn dir_name(&self, d: DirId) -> &'a str {
        let bytes = &self.cols.dir_name_bytes[run(self.cols.dir_name_ends, d.0 as usize, 0)];
        // SAFETY: `new` checked every name run is UTF-8 (or `SealedPaths` made it from `&str`s).
        unsafe { str::from_utf8_unchecked(bytes) }
    }

    pub fn parent(&self, p: PathId) -> DirId {
        DirId(self.cols.file_dir[p.0 as usize])
    }

    pub fn name(&self, p: PathId) -> &'a str {
        let bytes = &self.cols.file_name_bytes[run(self.cols.file_name_ends, p.0 as usize, 0)];
        // SAFETY: as `dir_name`.
        unsafe { str::from_utf8_unchecked(bytes) }
    }

    /// Directory `d`'s subdirectories, in name order.
    pub fn child_dirs(&self, d: DirId) -> impl ExactSizeIterator<Item = DirId> + use<> {
        run(self.cols.dirs_end, d.0 as usize, 1).map(|c| DirId(c as u32))
    }

    /// Directory `d`'s files, in name order.
    pub fn files_in(&self, d: DirId) -> impl ExactSizeIterator<Item = PathId> + use<> {
        run(self.cols.files_end, d.0 as usize, 0).map(|f| PathId(f as u32))
    }

    /// The index in `range` whose name (by `name`) is `want`: a binary search in a sorted run.
    fn search(range: Range<usize>, want: &str, name: impl Fn(usize) -> &'a str) -> Option<usize> {
        let (mut low, mut high) = (range.start, range.end);
        while low < high {
            let mid = low + (high - low) / 2;
            match name(mid).cmp(want) {
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
                Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    pub fn find_dir(&self, parent: DirId, name: &str) -> Option<DirId> {
        let range = run(self.cols.dirs_end, parent.0 as usize, 1);
        Self::search(range, name, |c| self.dir_name(DirId(c as u32))).map(|c| DirId(c as u32))
    }

    pub fn find(&self, dir: DirId, name: &str) -> Option<PathId> {
        let range = run(self.cols.files_end, dir.0 as usize, 0);
        Self::search(range, name, |f| self.name(PathId(f as u32))).map(|f| PathId(f as u32))
    }

    pub fn find_path(&self, path: &str) -> Option<PathId> {
        let (dirs, file) = match path.rsplit_once('/') {
            Some((dirs, file)) => (Some(dirs), file),
            None => (None, path),
        };
        let mut dir = DirId::ROOT;
        if let Some(dirs) = dirs {
            for name in dirs.split('/') {
                dir = self.find_dir(dir, name)?;
            }
        }
        self.find(dir, file)
    }

    pub fn write_dir(&self, d: DirId, put: &mut impl FnMut(&str)) {
        write_dir(d.0, &|d| self.cols.dir_parent[d as usize], &|d| self.dir_name(DirId(d)), put);
    }

    pub fn write(&self, p: PathId, put: &mut impl FnMut(&str)) {
        let dir = self.cols.file_dir[p.0 as usize];
        self.write_dir(DirId(dir), put);
        if dir != 0 {
            put("/");
        }
        put(self.name(p));
    }
}
