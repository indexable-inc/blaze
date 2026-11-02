//! The Merkle tree: immutable nodes interned by content ID, so one ID is one [`NodeId`] and
//! equal subtrees are one node. [`Tree::apply`] makes a new root from a base and ops, all or
//! nothing, copying only the directories on the paths it changes; [`Tree::diff`] descends only
//! where IDs differ.
//!
//! A file node is its bytes alone; its [`Mode`] is in the entry that names it, as in git, so a
//! change of mode copies the directories above it and nothing else.
//!
//! The nodes live in the [`Table`] readers share ([`crate::store`]): a node is readable from
//! any thread the moment it is interned, and a collection publishes a new generation.

use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::BTreeMap,
    ops::Bound,
    sync::{Arc, OnceLock},
};

use blaze_format::cursor::Put as _;
use blaze_ids::{
    ContentId,
    bits::{BitIndex, BitSet},
};
use blaze_image::Corrupt;
use hashbrown::{HashTable, hash_table};
use smallvec::{SmallVec, smallvec};

use crate::{
    Change, ChangeKind, DIR_DOMAIN, FILE_DOMAIN, Mode, Op, OpError,
    image::Bytes,
    reader::Reader,
    store::{Building, DIR, Entries, Shape, Table, Writer, adopt, mode_byte, mode_of, name_hash},
};

/// Runs `job(0..n)`, each once, on the caller's workers, returning once all have run: what the
/// daemon's pool gives ([`crate::Vfs::hash_on`]), as `blaze_cargo`'s `on_pool` is.
// dyn: one call per run of files hashed, each run at least `JOB_BYTES`.
pub type Jobs = fn(n: usize, job: &(dyn Fn(usize) + Sync));

/// A commit's new files are hashed on its [`Jobs`] from this many bytes; below it the
/// hand-off costs more than the hashing (1 MiB is about 170 µs of one core's BLAKE3).
const PARALLEL_BYTES: usize = 1 << 20;
/// About how many bytes one job hashes: a file longer is a job alone.
const JOB_BYTES: usize = 256 << 10;

/// Each of `inputs`' content ID as a file, in order, into `out` (cleared first): every input
/// at once, a SIMD lane each ([`ContentId::of_many_into`]), and in runs of about
/// [`JOB_BYTES`] on `jobs` once they hold [`PARALLEL_BYTES`]. The same IDs either way.
pub(crate) fn hash_files(inputs: &[&[u8]], out: &mut Vec<ContentId>, jobs: Option<Jobs>) {
    let total: usize = inputs.iter().map(|it| it.len()).sum();
    let Some(jobs) = jobs.filter(|_| total >= PARALLEL_BYTES && inputs.len() > 1) else {
        ContentId::of_many_into(FILE_DOMAIN, inputs, out);
        return;
    };
    // alloc: once per commit hashed in parallel, where each run starts.
    let mut starts = vec![0];
    let mut bytes = 0;
    for (at, input) in inputs.iter().enumerate() {
        if bytes >= JOB_BYTES {
            starts.push(at);
            bytes = 0;
        }
        bytes += input.len();
    }
    starts.push(inputs.len());
    // alloc: once per commit hashed in parallel, a slot per file.
    let slots: Vec<OnceLock<ContentId>> = (0..inputs.len()).map(|_| OnceLock::new()).collect();
    jobs(starts.len() - 1, &|job| {
        let (from, to) = (starts[job], starts[job + 1]);
        // alloc: one per run of at least `JOB_BYTES` hashed, its IDs.
        let mut ids = Vec::with_capacity(to - from);
        ContentId::of_many_into(FILE_DOMAIN, &inputs[from..to], &mut ids);
        for (slot, id) in slots[from..to].iter().zip(ids) {
            slot.set(id).expect("each file is hashed by one run");
        }
    });
    out.clear();
    out.extend(slots.into_iter().map(|it| it.into_inner().expect("every run ran")));
}

/// A node of a [`Tree`]. Only meaningful for the tree that made it, and only until it collects.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct NodeId(pub(crate) u32);

impl BitIndex for NodeId {
    #[inline]
    fn to_bit(self) -> usize {
        self.0 as usize
    }

    #[inline]
    fn from_bit(bit: usize) -> NodeId {
        NodeId(u32::from_bit(bit))
    }
}

/// An interned entry name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct NameId(pub(crate) u32);

/// A directory's entry: its name, its node and, for a file, its mode ([`Mode::Regular`] for a
/// directory).
pub(crate) type Entry = (NameId, NodeId, Mode);

/// git's mode of a directory entry.
pub(crate) const DIR_MODE: u32 = 0o040000;

/// How many symlinks one path may pass through, as Linux allows.
const MAX_LINKS: usize = 40;

/// A file a transaction puts at a path, with its mode: one the tree has (the base's, or one
/// moved), kept as its node so it is never read or hashed again; or new bytes, which become
/// the file's own (an owned op's, or an edit's, without a copy).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Blob {
    Node(NodeId, Mode),
    New(Bytes, Mode),
}

impl Blob {
    fn mode(&self) -> Mode {
        match self {
            Blob::Node(_, mode) | Blob::New(_, mode) => *mode,
        }
    }
}

/// What a transaction leaves at each path it touched: a new file, or nothing.
// BTreeMap: range queries for the paths under a directory.
pub(crate) type Pending = BTreeMap<String, Option<Blob>>;

/// A path [`Tree::apply`] changes, as the run `start..end` of its components in one column,
/// what it leaves there, and for new bytes where their ID is among the commit's hashed files
/// ([`NOT_HASHED`] for none).
type Staged<'a> = (usize, usize, &'a Option<Blob>, u32);

const NOT_HASHED: u32 = u32::MAX;

/// The paths an op reads or changes.
pub(crate) fn op_paths(op: &Op) -> impl Iterator<Item = &str> {
    let (one, two) = match op {
        Op::Write { path, .. } | Op::Edit { path, .. } | Op::Delete { path } => (path, None),
        Op::Rename { from, to } => (from, Some(to)),
    };
    std::iter::once(one.as_str()).chain(two.map(String::as_str))
}

/// Checks that `path` has one or more parts, none empty, `.` or `..`.
pub(crate) fn check(path: &str) -> Result<(), OpError> {
    let bad =
        path.is_empty() || path.split('/').any(|it| it.is_empty() || it == "." || it == "..");
    // alloc: the error's path
    if bad { Err(OpError::BadPath(path.to_owned())) } else { Ok(()) }
}

/// A directory's entries as the tree reads them.
#[derive(Clone, Copy)]
pub(crate) struct Dir<'a>(Entries<'a>);

impl<'a> Dir<'a> {
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn get(&self, at: usize) -> Entry {
        (NameId(self.0.names[at]), NodeId(self.0.nodes[at]), mode_of(self.0.modes[at]))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = Entry> + 'a {
        let dir = *self;
        (0..dir.len()).map(move |at| dir.get(at))
    }

    pub(crate) fn is_dir(&self, at: usize) -> bool {
        self.0.is_dir(at)
    }
}

/// Every node and name made so far, interned. The one writer of the store readers share.
pub(crate) struct Tree {
    store: Writer,
    /// Each name after the base's by its [`name_hash`] (computed once and kept, so growing
    /// never hashes again); the base's are found by the image's own index.
    name_index: HashTable<(u64, u32)>,
    empty: NodeId,
    /// Tables replaced by collections and not freed yet.
    retired: usize,
    /// A directory's hashed bytes, and its entries as the store keeps them, reused.
    scratch: Vec<u8>,
    columns: Vec<(u32, u32, u8)>,
    /// The entries of the directories a commit is rebuilding, outermost first, reused.
    rebuilt: Vec<Entry>,
    /// A commit's new files' IDs, hashed together before it merges, reused.
    hashed: Vec<ContentId>,
    /// Where a commit's new files are hashed when they are many bytes; one thread without.
    pub(crate) jobs: Option<Jobs>,
    /// The nodes the last collection kept, or the tree had when it was made.
    kept: usize,
}

impl Tree {
    pub(crate) fn new() -> Tree {
        Tree::over(Writer::new())
    }

    /// The tree whose nodes `store` holds, a restored base or nothing: the empty root is
    /// found by its ID, or made.
    pub(crate) fn over(store: Writer) -> Tree {
        let mut tree = Tree {
            store,
            name_index: HashTable::new(),
            empty: NodeId(0),
            retired: 0,
            // alloc: empty, allocates nothing; grown once and reused by every directory.
            scratch: Vec::new(),
            // alloc: empty, allocates nothing; grown once and reused by every directory.
            columns: Vec::new(),
            // alloc: empty, allocates nothing; grown once and reused by every commit.
            rebuilt: Vec::new(),
            // alloc: empty, allocates nothing; grown once and reused by every commit.
            hashed: Vec::new(),
            jobs: None,
            kept: 0,
        };
        tree.empty = tree.dir_sorted(&mut []);
        tree.kept = tree.len();
        tree
    }

    /// Whether a collection is paid for: the tree made at least as many nodes since the last
    /// one as it kept, so its walk and copy of what is kept cost no more than making them did.
    pub(crate) fn collection_due(&self) -> bool {
        self.len() - self.kept >= self.kept
    }

    /// A handle that reads every node from any thread.
    pub(crate) fn reader(&self) -> Reader {
        Reader::new(self.store.shared())
    }

    pub(crate) fn table(&self) -> &Table {
        self.store.table()
    }

    /// Frees the tables collections replaced that no reader is in any more.
    pub(crate) fn reclaim(&mut self) {
        if self.retired > 0 {
            self.retired = self.store.reclaim();
        }
    }

    /// The root with no files.
    pub(crate) fn empty(&self) -> NodeId {
        self.empty
    }

    /// How many nodes it holds.
    pub(crate) fn len(&self) -> usize {
        self.table().len() as usize
    }

    fn shape(&self, node: NodeId) -> Shape {
        self.table().shape(node.0)
    }

    pub(crate) fn id(&self, node: NodeId) -> ContentId {
        *self.table().id(node.0)
    }

    pub(crate) fn node(&self, id: &ContentId) -> Option<NodeId> {
        self.table().find(id).map(NodeId)
    }

    pub(crate) fn is_dir(&self, node: NodeId) -> bool {
        self.shape(node).is_dir()
    }

    /// A directory's entries, sorted by name; `None` for a file.
    pub(crate) fn entries(&self, node: NodeId) -> Option<Dir<'_>> {
        let shape = self.shape(node);
        shape.is_dir().then(|| Dir(self.table().entries(shape)))
    }

    /// A file's bytes, read in place; `None` for a directory. A file of the image whose page
    /// group fails its CRC32C is `Err`, never read.
    pub(crate) fn bytes(&self, node: NodeId) -> Result<Option<&[u8]>, Corrupt> {
        let shape = self.shape(node);
        if shape.is_dir() {
            return Ok(None);
        }
        self.table().blob(shape).map(Some)
    }

    /// A file's bytes, held for as long as wanted: one count of the file's or the image's.
    pub(crate) fn owned(&self, node: NodeId) -> Result<Option<Bytes>, Corrupt> {
        let shape = self.shape(node);
        if shape.is_dir() {
            return Ok(None);
        }
        self.table().bytes(shape).map(Some)
    }

    pub(crate) fn name(&mut self, name: &str) -> NameId {
        let hash = name_hash(name.as_bytes());
        if let Some(id) = self.store.names().find_base(hash, name.as_bytes()) {
            return NameId(id);
        }
        let Tree { store, name_index, .. } = self;
        let same = |&(at, id): &(u64, u32)| at == hash && store.names().get(id) == name.as_bytes();
        match name_index.entry(hash, same, |&(at, _)| at) {
            hash_table::Entry::Occupied(it) => NameId(it.get().1),
            hash_table::Entry::Vacant(it) => {
                let id = store.intern(name);
                it.insert((hash, id));
                NameId(id)
            }
        }
    }

    fn name_bytes(&self, name: NameId) -> &[u8] {
        self.store.names().get(name.0)
    }

    pub(crate) fn name_str(&self, name: NameId) -> &str {
        self.store.names().str(name.0)
    }

    /// The node of a file with these bytes. A node of the image with this ID whose page group
    /// failed its check is given these bytes ([`Tree::repair`]): writing a file again, as
    /// the watcher does when it finds one unreadable, mends it.
    pub(crate) fn file(&mut self, bytes: Bytes) -> NodeId {
        let id = ContentId::of(FILE_DOMAIN, &bytes);
        self.file_with(id, bytes)
    }

    /// [`Tree::file`] of bytes already hashed: `id` is their [`FILE_DOMAIN`] content ID.
    pub(crate) fn file_with(&mut self, id: ContentId, bytes: Bytes) -> NodeId {
        debug_assert_eq!(id, ContentId::of(FILE_DOMAIN, &bytes));
        if let Some(node) = self.table().find(&id).map(NodeId) {
            if self.bytes(node).is_err() {
                self.repair(node, bytes);
            }
            return node;
        }
        NodeId(self.store.file(id, bytes))
    }

    /// Publishes the table with every node where it is and `node`'s bytes `bytes` (which hash
    /// to its ID), in place of its range of the image: as a collection publishes, so readers
    /// move to it at once. Once per corrupt file, so the copy of the columns is rare.
    fn repair(&mut self, node: NodeId, bytes: Bytes) {
        let old = self.store.table();
        let len = old.len();
        let mut next = Building::after(old, len as usize);
        let mut bytes = Some(bytes);
        // alloc: one directory's entries at a time, once per repair.
        let mut columns = Vec::new();
        for at in 0..len {
            let (id, shape) = (*old.id(at), old.shape(at));
            let made = if shape.is_dir() {
                let from = old.entries(shape);
                columns.clear();
                let entry = |k: usize| (from.names[k], from.nodes[k], from.modes[k]);
                columns.extend((0..from.len()).map(entry));
                next.dir(id, &columns)
            } else if at == node.0 {
                next.file(id, bytes.take().expect("one node is repaired"))
            } else {
                next.file_of(id, old, shape)
            };
            assert_eq!(made, at, "a repair keeps every node's number");
        }
        self.retired = self.store.replace(next);
    }

    /// The node of a directory with these entries, in any order; names must be distinct.
    pub(crate) fn dir(&mut self, mut entries: Vec<Entry>) -> NodeId {
        blaze_ids::names::sort_by_name_in(&mut entries, |it| self.name_bytes(it.0));
        self.dir_sorted(&mut entries)
    }

    /// The node of a directory with these entries, sorted by name and distinct. A directory's
    /// entry is given [`Mode::Regular`] whatever it says, so one tree is one ID. Its hashed
    /// bytes are each entry's git mode code, length-prefixed name and child ID, as
    /// [`DIR_DOMAIN`] says, built in one buffer and hashed in one call.
    fn dir_sorted(&mut self, entries: &mut [Entry]) -> NodeId {
        let mut bytes = std::mem::take(&mut self.scratch);
        bytes.clear();
        for entry in entries.iter_mut() {
            let (name, child, mode) = *entry;
            let code = if self.is_dir(child) {
                entry.2 = Mode::Regular;
                DIR_MODE
            } else {
                mode.git()
            };
            let name = self.name_bytes(name);
            bytes.put_u32(code);
            bytes.put_bytes_u64(name);
            bytes.put_bytes(&self.id(child).0);
        }
        let id = ContentId::of(DIR_DOMAIN, &bytes);
        self.scratch = bytes;
        if let Some(node) = self.table().find(&id) {
            return NodeId(node);
        }
        let mut columns = std::mem::take(&mut self.columns);
        columns.clear();
        columns.extend(entries.iter().map(|&(name, child, mode)| {
            (name.0, child.0, if self.is_dir(child) { DIR } else { mode_byte(mode) })
        }));
        let node = self.store.dir(id, &columns);
        self.columns = columns;
        NodeId(node)
    }

    /// git's code for a directory entry: 040000 for a directory, else its file's mode.
    pub(crate) fn code(&self, child: NodeId, mode: Mode) -> u32 {
        if self.is_dir(child) { DIR_MODE } else { mode.git() }
    }

    fn child(&self, dir: NodeId, name: &str) -> Option<(NodeId, Mode)> {
        let Dir(entries) = self.entries(dir)?;
        let at = entries.find(self.store.names(), name.as_bytes())?;
        Some((NodeId(entries.nodes[at]), mode_of(entries.modes[at])))
    }

    /// The entry named `name` in the directory `dir`.
    fn child_entry(&self, dir: NodeId, name: &str) -> Option<Entry> {
        let entries = self.entries(dir)?;
        let at = entries.0.find(self.store.names(), name.as_bytes())?;
        Some(entries.get(at))
    }

    /// The node at `path` under `root` and its mode, as it is (a symlink is not followed);
    /// `""` is the root itself.
    pub(crate) fn entry(&self, root: NodeId, path: &str) -> Option<(NodeId, Mode)> {
        if path.is_empty() {
            return Some((root, Mode::Regular));
        }
        path.split('/').try_fold((root, Mode::Regular), |(node, _), name| self.child(node, name))
    }

    /// The node at `path` under `root`; `""` is the root itself.
    pub(crate) fn lookup(&self, root: NodeId, path: &str) -> Option<NodeId> {
        self.entry(root, path).map(|it| it.0)
    }

    /// The bytes of the file at `path` under `root`.
    pub(crate) fn read(&self, root: NodeId, path: &str) -> Result<Option<&[u8]>, Corrupt> {
        match self.lookup(root, path) {
            Some(node) => self.bytes(node),
            None => Ok(None),
        }
    }

    /// The file at `path` under `root`, as its node and mode: nothing of it is read.
    pub(crate) fn blob(&self, root: NodeId, path: &str) -> Option<Blob> {
        let (node, mode) = self.entry(root, path)?;
        (!self.is_dir(node)).then_some(Blob::Node(node, mode))
    }

    /// A staged file's bytes.
    fn staged_bytes<'a>(&'a self, blob: &'a Blob) -> Result<&'a [u8], Corrupt> {
        match blob {
            Blob::Node(node, _) => Ok(self.bytes(*node)?.expect("a staged node is a file")),
            Blob::New(bytes, _) => Ok(&bytes[..]),
        }
    }

    /// The node at `path` under `root` with every symlink on the way followed, as the system
    /// opens a path: a relative target is taken from the link's directory. `Ok(None)` when
    /// nothing is there or a file is passed through as a directory.
    pub(crate) fn resolve(
        &self,
        root: NodeId,
        path: &str,
    ) -> Result<Option<(NodeId, Mode)>, OpError> {
        // What is left to walk, as cursors into texts: the path, then each link's target in
        // front of what was left when it was met. A target is borrowed from the tree unless it
        // is not UTF-8.
        let mut rest: SmallVec<[(Cow<'_, str>, usize); 4]> = SmallVec::new();
        if !path.is_empty() {
            check(path)?;
            rest.push((Cow::Borrowed(path), 0));
        }
        // The directories from the root to where the walk is, and the names of all but the root.
        // alloc: inline, on the heap only past 16 directories deep.
        let mut dirs: SmallVec<[NodeId; 16]> = smallvec![root];
        let mut names: SmallVec<[NameId; 16]> = SmallVec::new();
        let mut links = 0;
        // The last link followed: the names on its path, and its target's bytes.
        let mut link: SmallVec<[NameId; 16]> = SmallVec::new();
        let mut target: Option<&[u8]> = None;
        let bad = |link: &[NameId], target: Option<&[u8]>| {
            let target = target.expect("only a link's target leaves the tree");
            // alloc: the error's path and target
            let mut path = String::new();
            for (at, name) in link.iter().enumerate() {
                if at > 0 {
                    path.push('/');
                }
                path.push_str(self.name_str(*name));
            }
            let target = String::from_utf8_lossy(target).into_owned(); // alloc: the error's target
            OpError::BadLink { path, target }
        };
        while let Some((text, at)) = rest.last_mut() {
            if *at > text.len() {
                rest.pop();
                continue;
            }
            let start = *at;
            let end = text[start..].find('/').map_or(text.len(), |it| start + it);
            *at = end + 1;
            let part = &rest[rest.len() - 1].0[start..end];
            match part {
                "" | "." => continue,
                ".." => {
                    if dirs.len() == 1 {
                        return Err(bad(&link, target));
                    }
                    dirs.pop();
                    names.pop();
                    continue;
                }
                name => {
                    let dir = *dirs.last().expect("the root");
                    let Some((name, child, mode)) = self.child_entry(dir, name) else {
                        return Ok(None);
                    };
                    if mode == Mode::Symlink {
                        let bytes = self.bytes(child)?.expect("a symlink is a file");
                        links += 1;
                        link.clear();
                        link.extend_from_slice(&names);
                        link.push(name);
                        target = Some(bytes);
                        if links > MAX_LINKS || bytes.is_empty() || bytes[0] == b'/' {
                            return Err(bad(&link, target));
                        }
                        while rest.last().is_some_and(|(text, at)| *at > text.len()) {
                            rest.pop();
                        }
                        // alloc: only a target that is not UTF-8 is copied.
                        rest.push((String::from_utf8_lossy(bytes), 0));
                        continue;
                    }
                    if rest.iter().all(|(text, at)| *at > text.len()) {
                        return Ok(Some((child, mode)));
                    }
                    if !self.is_dir(child) {
                        return Ok(None);
                    }
                    dirs.push(child);
                    names.push(name);
                }
            }
        }
        Ok(Some((*dirs.last().expect("the root"), Mode::Regular)))
    }

    /// Every file under `node`, with its path below `prefix` and its mode, in path order. `node`
    /// itself, when a file, is taken as `mode`.
    pub(crate) fn files(
        &self,
        node: NodeId,
        mode: Mode,
        prefix: &str,
    ) -> Vec<(String, NodeId, Mode)> {
        // alloc: the listing is the output, one path per file; `each_file` lends paths instead.
        let mut out = Vec::new();
        // alloc: the one buffer every path is built in.
        self.each_file(node, mode, &mut prefix.to_owned(), &mut |path, node, mode| {
            out.push((path.to_owned(), node, mode)); // alloc: the listing's path
        });
        out
    }

    /// Calls `f` with every file under `node`, its path below `path` and its mode, in path
    /// order, building each path in the one buffer.
    fn each_file(
        &self,
        node: NodeId,
        mode: Mode,
        path: &mut String,
        f: &mut impl FnMut(&str, NodeId, Mode),
    ) {
        let Some(entries) = self.entries(node) else {
            f(path, node, mode);
            return;
        };
        for (name, child, mode) in entries.iter() {
            let len = path.len();
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(self.name_str(name));
            self.each_file(child, mode, path, f);
            path.truncate(len);
        }
    }

    /// The root `ops` make of `base`, or why they can't. Each op sees what the ones before it
    /// did; `base` itself is never changed.
    pub(crate) fn apply(&mut self, base: NodeId, ops: &[Op]) -> Result<NodeId, OpError> {
        self.apply_all(base, ops.iter().map(Cow::Borrowed))
    }

    /// [`Tree::apply`] of ops given each borrowed or owned: an owned write's bytes become the
    /// file's with no copy.
    pub(crate) fn apply_all<'o>(
        &mut self,
        base: NodeId,
        ops: impl IntoIterator<Item = Cow<'o, Op>>,
    ) -> Result<NodeId, OpError> {
        let mut pending = Pending::new();
        for op in ops {
            self.stage(base, &mut pending, op)?;
        }
        // By component, so each directory's changes are one run, in its entries' order: every
        // path's components in one column, and each change its run of them.
        let mut parts: Vec<&str> = Vec::with_capacity(pending.len() * 4);
        let mut changes: Vec<Staged<'_>> = Vec::with_capacity(pending.len());
        for (path, blob) in &pending {
            let start = parts.len();
            parts.extend(path.split('/'));
            changes.push((start, parts.len(), blob, NOT_HASHED));
        }
        changes.sort_unstable_by(|a, b| parts[a.0..a.1].cmp(&parts[b.0..b.1]));
        // Every new file's ID at once, before the merge interns them in order.
        let mut hashed = std::mem::take(&mut self.hashed);
        {
            // alloc: once per commit, a slice per new file.
            let mut new: Vec<&[u8]> = Vec::new();
            for change in &mut changes {
                let blob: &Option<Blob> = change.2;
                if let Some(Blob::New(bytes, _)) = blob {
                    change.3 = u32::try_from(new.len()).expect("fewer than 2^32 files");
                    new.push(&bytes[..]);
                }
            }
            hash_files(&new, &mut hashed, self.jobs);
        }
        let mut stack = std::mem::take(&mut self.rebuilt);
        let root = self.rebuild(Some(base), &parts, &changes, &hashed, 0, &mut stack);
        self.rebuilt = stack;
        self.hashed = hashed;
        Ok(root.unwrap_or(self.empty))
    }

    /// The file at `path` as the transaction sees it so far: `Err` for a directory.
    fn current(&self, base: NodeId, pending: &Pending, path: &str) -> Result<Option<Blob>, ()> {
        if let Some(blob) = self.file_now(base, pending, path) {
            return Ok(Some(blob));
        }
        if self.has_files_under(base, pending, path) { Err(()) } else { Ok(None) }
    }

    /// The file at `path` as the transaction sees it so far, if a file is there.
    fn file_now(&self, base: NodeId, pending: &Pending, path: &str) -> Option<Blob> {
        match pending.get(path) {
            // clone: a node, or one more count of new bytes; once per path an op checks.
            Some(blob) => blob.clone(),
            None if shadowed(pending, path) => None,
            None => self.blob(base, path),
        }
    }

    /// Whether the transaction sees any file under the directory `path`; stops at the first.
    fn has_files_under(&self, base: NodeId, pending: &Pending, path: &str) -> bool {
        // alloc: once per op checked; the walk below builds every path in this one buffer.
        let mut below = format!("{path}/");
        let staged = pending
            .range::<str, _>((Bound::Included(below.as_str()), Bound::Unbounded))
            .take_while(|it| it.0.starts_with(below.as_str()))
            .any(|it| it.1.is_some());
        if staged {
            return true;
        }
        if pending.contains_key(path) || shadowed(pending, path) {
            return false;
        }
        let Some(dir) = self.lookup(base, path).filter(|it| self.is_dir(*it)) else { return false };
        below.pop();
        self.any_file_left(dir, pending, &mut below)
    }

    /// Whether the directory `dir`, at `path`, has a file under it on a path `pending` has no
    /// say on; stops at the first.
    fn any_file_left(&self, dir: NodeId, pending: &Pending, path: &mut String) -> bool {
        for (name, child, _) in self.entries(dir).expect("a directory").iter() {
            let len = path.len();
            path.push('/');
            path.push_str(self.name_str(name));
            let found = !pending.contains_key(path.as_str())
                && (!self.is_dir(child) || self.any_file_left(child, pending, path));
            path.truncate(len);
            if found {
                return true;
            }
        }
        false
    }

    /// The files under the directory `path`, as the transaction sees them so far.
    fn files_under(&self, base: NodeId, pending: &Pending, path: &str) -> Vec<(String, Blob)> {
        // The base's files are those `pending` has no say on, so no path is found twice.
        // alloc: a directory deleted or moved; each path found becomes a key of `Pending`.
        let mut found: Vec<(String, Blob)> = Vec::new();
        let gone = pending.contains_key(path) || shadowed(pending, path);
        if !gone && let Some(dir) = self.lookup(base, path).filter(|it| self.is_dir(*it)) {
            for (file, node, mode) in self.files(dir, Mode::Regular, path) {
                if !pending.contains_key(&file) && !shadowed(pending, &file) {
                    found.push((file, Blob::Node(node, mode)));
                }
            }
        }
        // alloc: once per directory deleted or moved, the range's start.
        let below = format!("{path}/");
        let range = (Bound::Included(below.as_str()), Bound::Unbounded);
        for (file, blob) in pending.range::<str, _>(range) {
            if !file.starts_with(below.as_str()) {
                break;
            }
            if let Some(blob) = blob {
                // clone: each path found becomes a key of `Pending`; a blob is a node or an `Arc`.
                found.push((file.clone(), blob.clone()));
            }
        }
        // Component order is not string order (`a-b` < `a/b`): sorted by path once.
        found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        found
    }

    /// Checks `op` against `base` with the ops staged before it in `pending`, and stages it. A
    /// write's bytes are moved out of an owned op, copied once from a borrowed one.
    pub(crate) fn stage(
        &self,
        base: NodeId,
        pending: &mut Pending,
        op: Cow<'_, Op>,
    ) -> Result<(), OpError> {
        for path in op_paths(&op) {
            check(path)?;
        }
        let file_at = |pending: &Pending, path: &str| -> Result<Option<Blob>, OpError> {
            // alloc: the error's path
            self.current(base, pending, path).map_err(|()| OpError::IsADirectory(path.to_owned()))
        };
        let no_file_above = |pending: &Pending, path: &str| -> Result<(), OpError> {
            for up in ancestors(path) {
                if self.file_now(base, pending, up).is_some() {
                    // alloc: the error's path
                    return Err(OpError::NotADirectory(path.to_owned()));
                }
            }
            Ok(())
        };
        if let Op::Write { path, mode, .. } = &*op {
            no_file_above(pending, path)?;
            file_at(pending, path)?;
            let mode = *mode;
            let (path, bytes) = match op {
                Cow::Owned(Op::Write { path, bytes, .. }) => (Cow::Owned(path), adopt(bytes)),
                Cow::Borrowed(Op::Write { path, bytes, .. }) => {
                    // alloc: a borrowed op's bytes, copied once; `Vfs::commit_owned` moves them.
                    let bytes = Bytes::from(Arc::<[u8]>::from(bytes.as_slice()));
                    (Cow::Borrowed(path.as_str()), bytes)
                }
                _ => unreachable!("a write, matched above"),
            };
            put_owned(pending, path, Some(Blob::New(bytes, mode)));
            return Ok(());
        }
        match &*op {
            Op::Write { .. } => unreachable!("staged above"),
            Op::Edit { path, at, len, bytes } => {
                // clone: the error's path
                let blob = file_at(pending, path)?.ok_or_else(|| OpError::NotFound(path.clone()))?;
                let (old, mode) = (self.staged_bytes(&blob)?, blob.mode());
                let size = old.len() as u64;
                if at.checked_add(*len).is_none_or(|end| end > size) {
                    return Err(OpError::OutOfRange {
                        // clone: the error's path
                        path: path.clone(),
                        at: *at,
                        len: *len,
                        size,
                    });
                }
                let (at, end) = (*at as usize, (*at + *len) as usize);
                // alloc: the edited file's bytes, made once and kept as they are.
                let new = adopt([&old[..at], bytes, &old[end..]].concat());
                put(pending, path, Some(Blob::New(new, mode)));
            }
            Op::Delete { path } => self.delete(base, pending, path)?,
            Op::Rename { from, to } => {
                if to.strip_prefix(from.as_str()).is_some_and(|rest| rest.starts_with('/')) {
                    // clone: the error's path
                    return Err(OpError::BadPath(to.clone()));
                }
                if from == to {
                    // clone: the error's path
                    return Err(OpError::Exists(to.clone()));
                }
                // Each file moved at its new path.
                let moved: Vec<(String, Blob)> = match file_at(pending, from) {
                    // alloc: the key `Pending` keeps for the one file moved
                    Ok(Some(blob)) => vec![(to.clone(), blob)],
                    // clone: the error's path
                    Ok(None) => return Err(OpError::NotFound(from.clone())),
                    // alloc: `files_under`'s buffer reused in place; a path's own grows only for
                    // a `to` longer than `from`.
                    Err(_) => self
                        .files_under(base, pending, from)
                        .into_iter()
                        .map(|(mut file, blob)| {
                            file.replace_range(..from.len(), to);
                            (file, blob)
                        })
                        // alloc: none, the same element type collects into the same buffer.
                        .collect(),
                };
                // `from` goes first: a directory it leaves empty is gone, so its last file may
                // take the directory's path (`d/b` to `d`).
                self.delete(base, pending, from)?;
                match file_at(pending, to) {
                    Ok(None) => {}
                    // clone: the error's path
                    _ => return Err(OpError::Exists(to.clone())),
                }
                no_file_above(pending, to)?;
                for (path, blob) in moved {
                    pending.insert(path, Some(blob));
                }
            }
        }
        Ok(())
    }

    /// Stages the deletion of the file or the directory at `path`.
    fn delete(&self, base: NodeId, pending: &mut Pending, path: &str) -> Result<(), OpError> {
        match self.current(base, pending, path) {
            Ok(Some(_)) => put(pending, path, None),
            // alloc: the error's path
            Ok(None) => return Err(OpError::NotFound(path.to_owned())),
            Err(()) => {
                for (file, _) in self.files_under(base, pending, path) {
                    pending.insert(file, None);
                }
                put(pending, path, None);
            }
        }
        Ok(())
    }

    /// The directory `dir` with `changes` made to it (paths whose first `depth` components lead
    /// to it, sorted by component): `None` when it is left with nothing in it. Its entries and
    /// the changes are merged in one pass, as both are in name order.
    fn rebuild(
        &mut self,
        dir: Option<NodeId>,
        parts: &[&str],
        changes: &[Staged<'_>],
        hashed: &[ContentId],
        depth: usize,
        stack: &mut Vec<Entry>,
    ) -> Option<NodeId> {
        // This directory's old entries, then its new ones, above its parents' on `stack`.
        let base = stack.len();
        if let Some(entries) = dir.and_then(|it| self.entries(it)) {
            stack.extend(entries.iter());
        }
        let old = stack.len();
        let mut kept = base;
        let mut start = 0;
        let name_at = |change: &Staged<'_>| parts[change.0 + depth];
        while start < changes.len() {
            let name = name_at(&changes[start]);
            let end =
                start + changes[start..].iter().take_while(|it| name_at(*it) == name).count();
            let group = &changes[start..end];
            start = end;
            while kept < old && self.name_bytes(stack[kept].0) < name.as_bytes() {
                let entry = stack[kept];
                stack.push(entry);
                kept += 1;
            }
            let existing = (kept < old && self.name_bytes(stack[kept].0) == name.as_bytes())
                .then(|| {
                    kept += 1;
                    stack[kept - 1]
                });
            // The change at this name itself sorts first in its group: its path is the shortest.
            let (here, deeper) = match group[0].1 - group[0].0 == depth + 1 {
                true => (Some(&group[0]), &group[1..]),
                false => (None, group),
            };
            let child = match here {
                Some(&(_, _, blob, at)) if !deeper.iter().any(|it| it.2.is_some()) => {
                    blob.as_ref().map(|blob| match blob {
                        Blob::Node(node, mode) => (*node, *mode),
                        Blob::New(bytes, mode) => {
                            // clone: the store keeps the new file's bytes, as one more count.
                            (self.file_with(hashed[at as usize], bytes.clone()), *mode)
                        }
                    })
                }
                // A directory, made of what was there if that was one and nothing replaced it.
                _ => {
                    let was =
                        existing.map(|it| it.1).filter(|it| here.is_none() && self.is_dir(*it));
                    self.rebuild(was, parts, deeper, hashed, depth + 1, stack)
                        .map(|dir| (dir, Mode::Regular))
                }
            };
            if let Some((child, mode)) = child {
                let name = match existing {
                    Some(it) => it.0,
                    None => self.name(name),
                };
                stack.push((name, child, mode));
            }
        }
        stack.extend_from_within(kept..old);
        let made = match stack.len() == old && dir != Some(self.empty) {
            true => None,
            false => Some(self.dir_sorted(&mut stack[old..])),
        };
        stack.truncate(base);
        made
    }

    /// The files that differ between `from` and `to`, in path order. A file whose bytes or mode
    /// changed is [`ChangeKind::Modified`].
    pub(crate) fn diff(&self, from: NodeId, to: NodeId) -> Vec<Change> {
        // alloc: the output, one change per file that differs.
        let mut out = Vec::new();
        let root = |node| Some((node, Mode::Regular));
        // alloc: the one buffer every path is built in.
        self.diff_into(root(from), root(to), &mut String::new(), &mut out);
        out
    }

    fn diff_into(
        &self,
        from: Option<(NodeId, Mode)>,
        to: Option<(NodeId, Mode)>,
        path: &mut String,
        out: &mut Vec<Change>,
    ) {
        if from == to {
            return;
        }
        let all = |(node, mode): (NodeId, Mode),
                   kind: ChangeKind,
                   path: &mut String,
                   out: &mut Vec<Change>| {
            self.each_file(node, mode, path, &mut |path, _, _| {
                out.push(Change { path: path.to_owned(), kind }); // alloc: the change's path
            });
        };
        match (from, to) {
            (Some(a), Some(b)) => match (self.entries(a.0), self.entries(b.0)) {
                // clone: the change's path, the output
                (None, None) => out.push(Change { path: path.clone(), kind: ChangeKind::Modified }),
                // The same names in the same order, as when only contents changed: the node and
                // mode columns are compared in place, 16 entries at a time, and only an entry
                // that differs is looked at. Equal node numbers are one node, of one kind, so
                // equal mode bytes are equal modes.
                (Some(x), Some(y)) if x.0.names == y.0.names => {
                    const RUN: usize = 16;
                    for start in (0..x.len()).step_by(RUN) {
                        let end = (start + RUN).min(x.len());
                        if x.0.nodes[start..end] == y.0.nodes[start..end]
                            && x.0.modes[start..end] == y.0.modes[start..end]
                        {
                            continue;
                        }
                        for at in start..end {
                            if x.0.nodes[at] == y.0.nodes[at] && x.0.modes[at] == y.0.modes[at] {
                                continue;
                            }
                            let (name, left, from_mode) = x.get(at);
                            let (_, right, to_mode) = y.get(at);
                            let len = path.len();
                            if !path.is_empty() {
                                path.push('/');
                            }
                            path.push_str(self.name_str(name));
                            let (left, right) = (Some((left, from_mode)), Some((right, to_mode)));
                            self.diff_into(left, right, path, out);
                            path.truncate(len);
                        }
                    }
                }
                (Some(x), Some(y)) => {
                    let (mut i, mut j) = (0, 0);
                    while i < x.len() || j < y.len() {
                        let order = match (i < x.len(), j < y.len()) {
                            (true, true) if x.0.names[i] == y.0.names[j] => Ordering::Equal,
                            (true, true) => self
                                .name_bytes(NameId(x.0.names[i]))
                                .cmp(self.name_bytes(NameId(y.0.names[j]))),
                            (true, false) => Ordering::Less,
                            _ => Ordering::Greater,
                        };
                        let side = |dir: &Dir<'_>, at: usize| {
                            let (_, node, mode) = dir.get(at);
                            Some((node, mode))
                        };
                        let (name, left, right) = match order {
                            Ordering::Less => (x.0.names[i], side(&x, i), None),
                            Ordering::Greater => (y.0.names[j], None, side(&y, j)),
                            Ordering::Equal => (x.0.names[i], side(&x, i), side(&y, j)),
                        };
                        i += usize::from(order.is_le());
                        j += usize::from(order.is_ge());
                        // A shared subtree: skipped without building its path.
                        if left == right {
                            continue;
                        }
                        let len = path.len();
                        if !path.is_empty() {
                            path.push('/');
                        }
                        path.push_str(self.name_str(NameId(name)));
                        self.diff_into(left, right, path, out);
                        path.truncate(len);
                    }
                }
                // A file replaced by a directory or the other way: the file's path comes before
                // every path under it.
                (None, Some(_)) => {
                    // clone: the change's path, the output
                    out.push(Change { path: path.clone(), kind: ChangeKind::Removed });
                    all(b, ChangeKind::Added, path, out);
                }
                (Some(_), None) => {
                    // clone: the change's path, the output
                    out.push(Change { path: path.clone(), kind: ChangeKind::Added });
                    all(a, ChangeKind::Removed, path, out);
                }
            },
            (Some(a), None) => all(a, ChangeKind::Removed, path, out),
            (None, Some(b)) => all(b, ChangeKind::Added, path, out),
            (None, None) => {}
        }
    }

    /// Drops every node no root in `live` reaches, and renumbers the rest into a new table that
    /// readers move to at once; `live` is updated in place. Names are kept. Returns each old
    /// node's new one, `None` for a node dropped.
    pub(crate) fn collect(&mut self, live: &mut [NodeId]) -> Vec<Option<NodeId>> {
        let len = self.len();
        let mut reached = BitSet::<NodeId>::new(len);
        // alloc: once per collection, the walk's stack.
        let mut stack: Vec<NodeId> = live.iter().copied().chain([self.empty]).collect();
        while let Some(node) = stack.pop() {
            if !reached.insert(node) {
                continue;
            }
            if let Some(entries) = self.entries(node) {
                stack.extend(entries.0.nodes.iter().map(|&it| NodeId(it)));
            }
        }
        let count = reached.count();
        let old = self.store.table();
        let mut next = Building::after(old, count);
        // alloc: once per collection, the renumbering it returns.
        let mut renumbered: Vec<Option<NodeId>> = vec![None; len];
        // alloc: once per collection, one directory's entries at a time.
        let mut columns = Vec::new();
        // Children have smaller numbers than their parents, so they are renumbered first.
        for NodeId(at) in reached.iter() {
            let (id, shape) = (*old.id(at), old.shape(at));
            let node = if shape.is_dir() {
                let from = old.entries(shape);
                columns.clear();
                columns.extend((0..from.len()).map(|k| {
                    let child = renumbered[from.nodes[k] as usize].expect("a child is reached");
                    (from.names[k], child.0, from.modes[k])
                }));
                next.dir(id, &columns)
            } else {
                next.file_of(id, old, shape)
            };
            renumbered[at as usize] = Some(NodeId(node));
        }
        self.retired = self.store.replace(next);
        self.kept = count;
        self.empty = renumbered[self.empty.0 as usize].expect("the empty root is kept");
        for root in live {
            *root = renumbered[root.0 as usize].expect("a live root is kept");
        }
        renumbered
    }
}

/// Stages `blob` at `path`, making a key only for a path not staged before.
fn put(pending: &mut Pending, path: &str, blob: Option<Blob>) {
    put_owned(pending, Cow::Borrowed(path), blob);
}

/// [`put`] of a path the op may own: an owned one becomes the key as it is.
fn put_owned(pending: &mut Pending, path: Cow<'_, str>, blob: Option<Blob>) {
    match pending.get_mut(&*path) {
        Some(slot) => *slot = blob,
        None => {
            // alloc: a borrowed path staged for the first time becomes a key of `Pending`.
            pending.insert(path.into_owned(), blob);
        }
    }
}

/// Whether the transaction deleted or replaced with a file a directory above `path`, so the
/// base's contents there are gone.
fn shadowed(pending: &Pending, path: &str) -> bool {
    ancestors(path).any(|up| pending.contains_key(up))
}

/// `a/b/c` -> `a/b`, `a`.
pub(crate) fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    let mut rest = path;
    std::iter::from_fn(move || {
        let at = rest.rfind('/')?;
        rest = &rest[..at];
        Some(rest)
    })
}
