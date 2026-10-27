//! blaze's source of truth ([docs/roadmap.md](../../../docs/roadmap.md#1-the-vfs-a-content-addressed-merkle-tree)):
//! a persistent, content-addressed Merkle tree.
//!
//! - A file node is the BLAKE3 hash of its bytes; a directory node is the hash of its entries
//!   sorted by name. Nodes are immutable and interned by hash in a [`Tree`], so one hash is one
//!   [`NodeId`] and equal subtrees are one node.
//! - A root is a directory node. [`Tree::apply`] makes a new root from a base and a list of
//!   [`Op`]s, all or nothing, copying only the directories on the paths it changes.
//! - [`Tree::diff`] gives the files that differ between two roots, descending only where the
//!   hashes differ.
//! - [`Vfs`] keeps named heads (a workspace, an agent, a branch) and takes transactions against
//!   a base root, rebasing them onto a head that moved ([`heads`]); [`journal`] makes it durable
//!   through effects, so the same code runs under the simulation and on disk.

pub mod heads;
pub mod journal;

use std::{fmt, sync::Arc};

use rustc_hash::FxHashMap;

pub use heads::{Commit, CommitError, Vfs};

/// The BLAKE3 hash of a node.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash(pub [u8; 32]);

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Hash {
    /// The first 12 hex digits, as git shows a short id.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0[..6] {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::LowerHex for Hash {
    /// All 64 hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// A node of a [`Tree`]. Only meaningful for the tree that made it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct NodeId(u32);

/// An interned entry name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct NameId(u32);

#[derive(Clone)]
enum Node {
    File {
        hash: Hash,
        bytes: Arc<[u8]>,
    },
    /// Sorted by the names' bytes, so the hash does not depend on interning order.
    Dir {
        hash: Hash,
        entries: Box<[(NameId, NodeId)]>,
    },
}

/// A change to a list of files, applied all together or not at all.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Op {
    /// The whole file, created with its directories if missing.
    Write { path: String, bytes: Arc<[u8]> },
    /// `len` bytes at `at` replaced by `bytes`, as an editor's change or a `str_replace` is.
    Edit { path: String, at: u64, len: u64, bytes: Vec<u8> },
    /// A file, or a directory with everything under it.
    Delete { path: String },
    /// A file or a directory moved; `to` must not exist.
    Rename { from: String, to: String },
}

impl Op {
    /// The paths whose contents it reads or changes.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        let (one, two) = match self {
            Op::Write { path, .. } | Op::Edit { path, .. } | Op::Delete { path } => (path, None),
            Op::Rename { from, to } => (from, Some(to)),
        };
        std::iter::once(one.as_str()).chain(two.map(String::as_str))
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OpError {
    /// Empty, absolute, or with an empty, `.` or `..` part.
    BadPath(String),
    NotFound(String),
    /// A file was asked for and a directory is there.
    IsADirectory(String),
    /// A directory was needed and a file is there.
    NotADirectory(String),
    Exists(String),
    OutOfRange {
        path: String,
        at: u64,
        len: u64,
        size: u64,
    },
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpError::BadPath(path) => write!(f, "{path:?} is not a path in the tree"),
            OpError::NotFound(path) => write!(f, "{path} is not there"),
            OpError::IsADirectory(path) => write!(f, "{path} is a directory"),
            OpError::NotADirectory(path) => write!(f, "a part of {path} is a file"),
            OpError::Exists(path) => write!(f, "{path} is already there"),
            OpError::OutOfRange { path, at, len, size } => {
                write!(f, "{path} has {size} bytes, so {len} at {at} are not in it")
            }
        }
    }
}

/// A file that differs between two roots.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}

/// The parts of a path, checked.
pub fn parts(path: &str) -> Result<Vec<&str>, OpError> {
    let parts: Vec<&str> = path.split('/').collect();
    let bad = path.is_empty() || parts.iter().any(|it| it.is_empty() || *it == "." || *it == "..");
    if bad { Err(OpError::BadPath(path.to_owned())) } else { Ok(parts) }
}

/// Every node and name made so far, interned.
#[derive(Clone)]
pub struct Tree {
    nodes: Vec<Node>,
    by_hash: FxHashMap<Hash, NodeId>,
    names: Vec<Box<str>>,
    name_ids: FxHashMap<Box<str>, NameId>,
    empty: NodeId,
}

impl Default for Tree {
    fn default() -> Tree {
        Tree::new()
    }
}

/// What a transaction leaves at each path it touched: new bytes, or nothing.
type Pending = std::collections::BTreeMap<String, Option<Arc<[u8]>>>;

impl Tree {
    pub fn new() -> Tree {
        let mut tree = Tree {
            nodes: Vec::new(),
            by_hash: FxHashMap::default(),
            names: Vec::new(),
            name_ids: FxHashMap::default(),
            empty: NodeId(0),
        };
        tree.empty = tree.dir(Vec::new());
        tree
    }

    /// The root with no files.
    pub fn empty(&self) -> NodeId {
        self.empty
    }

    /// How many nodes it holds.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn hash(&self, node: NodeId) -> Hash {
        match &self.nodes[node.0 as usize] {
            Node::File { hash, .. } | Node::Dir { hash, .. } => *hash,
        }
    }

    pub fn node(&self, hash: &Hash) -> Option<NodeId> {
        self.by_hash.get(hash).copied()
    }

    pub fn is_dir(&self, node: NodeId) -> bool {
        matches!(self.nodes[node.0 as usize], Node::Dir { .. })
    }

    /// A directory's entries, sorted by name; `None` for a file.
    pub fn entries(&self, node: NodeId) -> Option<&[(NameId, NodeId)]> {
        match &self.nodes[node.0 as usize] {
            Node::Dir { entries, .. } => Some(entries),
            Node::File { .. } => None,
        }
    }

    /// A file's bytes; `None` for a directory.
    pub fn bytes(&self, node: NodeId) -> Option<&Arc<[u8]>> {
        match &self.nodes[node.0 as usize] {
            Node::File { bytes, .. } => Some(bytes),
            Node::Dir { .. } => None,
        }
    }

    pub fn name(&mut self, name: &str) -> NameId {
        if let Some(&id) = self.name_ids.get(name) {
            return id;
        }
        let id = NameId(self.names.len() as u32);
        self.names.push(name.into());
        self.name_ids.insert(name.into(), id);
        id
    }

    pub fn name_str(&self, name: NameId) -> &str {
        &self.names[name.0 as usize]
    }

    fn intern(&mut self, hash: Hash, node: impl FnOnce() -> Node) -> NodeId {
        if let Some(&id) = self.by_hash.get(&hash) {
            return id;
        }
        let id = NodeId(u32::try_from(self.nodes.len()).expect("fewer than 2^32 nodes"));
        self.nodes.push(node());
        self.by_hash.insert(hash, id);
        id
    }

    /// The node of a file with these bytes.
    pub fn file(&mut self, bytes: Arc<[u8]>) -> NodeId {
        let hash = Hash(*blake3::hash(&bytes).as_bytes());
        self.intern(hash, || Node::File { hash, bytes })
    }

    /// The node of a directory with these entries, in any order; names must be distinct.
    pub fn dir(&mut self, mut entries: Vec<(NameId, NodeId)>) -> NodeId {
        entries
            .sort_unstable_by(|a, b| self.names[a.0.0 as usize].cmp(&self.names[b.0.0 as usize]));
        // A key of its own, so no file's bytes can hash to a directory's.
        let mut hasher = blake3::Hasher::new_derive_key("blaze vfs directory v1");
        for &(name, child) in &entries {
            let name = &self.names[name.0 as usize];
            hasher.update(&[u8::from(self.is_dir(child))]);
            hasher.update(&(name.len() as u32).to_le_bytes());
            hasher.update(name.as_bytes());
            hasher.update(&self.hash(child).0);
        }
        let hash = Hash(*hasher.finalize().as_bytes());
        self.intern(hash, || Node::Dir { hash, entries: entries.into_boxed_slice() })
    }

    fn child(&self, dir: NodeId, name: &str) -> Option<NodeId> {
        let entries = self.entries(dir)?;
        let at = entries.binary_search_by(|(it, _)| (*self.names[it.0 as usize]).cmp(name)).ok()?;
        Some(entries[at].1)
    }

    /// The node at `path` under `root`.
    pub fn lookup(&self, root: NodeId, path: &str) -> Option<NodeId> {
        path.split('/').try_fold(root, |node, name| self.child(node, name))
    }

    /// The bytes of the file at `path` under `root`.
    pub fn read(&self, root: NodeId, path: &str) -> Option<&Arc<[u8]>> {
        self.bytes(self.lookup(root, path)?)
    }

    /// Every file under `node`, with its path below `prefix`, in path order.
    pub fn files(&self, node: NodeId, prefix: &str) -> Vec<(String, NodeId)> {
        let mut out = Vec::new();
        self.walk(node, &mut prefix.to_owned(), &mut out);
        out
    }

    fn walk(&self, node: NodeId, path: &mut String, out: &mut Vec<(String, NodeId)>) {
        let Some(entries) = self.entries(node) else {
            out.push((path.clone(), node));
            return;
        };
        for &(name, child) in entries {
            let len = path.len();
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(&self.names[name.0 as usize]);
            self.walk(child, path, out);
            path.truncate(len);
        }
    }

    /// The root `ops` make of `base`, or why they can't. Each op sees what the ones before it
    /// did; `base` itself is never changed.
    pub fn apply(&mut self, base: NodeId, ops: &[Op]) -> Result<NodeId, OpError> {
        let mut pending = Pending::new();
        for op in ops {
            self.stage(base, &mut pending, op)?;
        }
        let mut changes: Vec<(Vec<&str>, Option<Arc<[u8]>>)> = Vec::with_capacity(pending.len());
        for (path, bytes) in &pending {
            changes.push((path.split('/').collect(), bytes.clone()));
        }
        Ok(self.rebuild(Some(base), &changes).unwrap_or(self.empty))
    }

    /// The bytes at `path` as the transaction sees it so far: `Err` for a directory.
    fn current(
        &self,
        base: NodeId,
        pending: &Pending,
        path: &str,
    ) -> Result<Option<Arc<[u8]>>, ()> {
        if let Some(bytes) = self.file_now(base, pending, path) {
            return Ok(Some(bytes));
        }
        if self.has_files_under(base, pending, path) { Err(()) } else { Ok(None) }
    }

    /// The file at `path` as the transaction sees it so far, if a file is there.
    fn file_now(&self, base: NodeId, pending: &Pending, path: &str) -> Option<Arc<[u8]>> {
        match pending.get(path) {
            Some(bytes) => bytes.clone(),
            None if shadowed(pending, path) => None,
            None => self.read(base, path).cloned(),
        }
    }

    /// Whether the transaction sees any file under the directory `path`; stops at the first.
    fn has_files_under(&self, base: NodeId, pending: &Pending, path: &str) -> bool {
        let below = format!("{path}/");
        let staged = pending
            .range(below.clone()..)
            .take_while(|it| it.0.starts_with(&below))
            .any(|it| it.1.is_some());
        if staged {
            return true;
        }
        if pending.contains_key(path) || shadowed(pending, path) {
            return false;
        }
        let Some(dir) = self.lookup(base, path).filter(|it| self.is_dir(*it)) else { return false };
        let mut stack = vec![(dir, path.to_owned())];
        while let Some((node, at)) = stack.pop() {
            for &(name, child) in self.entries(node).expect("a directory") {
                let file = format!("{at}/{}", self.name_str(name));
                if pending.contains_key(&file) {
                    continue;
                }
                if self.is_dir(child) {
                    stack.push((child, file));
                } else {
                    return true;
                }
            }
        }
        false
    }

    /// The files under the directory `path`, as the transaction sees them so far.
    fn files_under(&self, base: NodeId, pending: &Pending, path: &str) -> Vec<(String, Arc<[u8]>)> {
        let mut found: std::collections::BTreeMap<String, Arc<[u8]>> =
            std::collections::BTreeMap::new();
        let gone = pending.contains_key(path) || shadowed(pending, path);
        if !gone && let Some(dir) = self.lookup(base, path).filter(|it| self.is_dir(*it)) {
            for (file, node) in self.files(dir, path) {
                if !pending.contains_key(&file) && !shadowed(pending, &file) {
                    found.insert(file, self.bytes(node).expect("a file").clone());
                }
            }
        }
        let below = format!("{path}/");
        for (file, bytes) in pending.range(below.clone()..) {
            if !file.starts_with(&below) {
                break;
            }
            if let Some(bytes) = bytes {
                found.insert(file.clone(), bytes.clone());
            }
        }
        found.into_iter().collect()
    }

    fn stage(&self, base: NodeId, pending: &mut Pending, op: &Op) -> Result<(), OpError> {
        for path in op.paths() {
            parts(path)?;
        }
        let file_at = |pending: &Pending, path: &str| -> Result<Option<Arc<[u8]>>, OpError> {
            self.current(base, pending, path).map_err(|()| OpError::IsADirectory(path.to_owned()))
        };
        let no_file_above = |pending: &Pending, path: &str| -> Result<(), OpError> {
            for up in ancestors(path) {
                if self.file_now(base, pending, up).is_some() {
                    return Err(OpError::NotADirectory(path.to_owned()));
                }
            }
            Ok(())
        };
        match op {
            Op::Write { path, bytes } => {
                no_file_above(pending, path)?;
                file_at(pending, path)?;
                pending.insert(path.clone(), Some(bytes.clone()));
            }
            Op::Edit { path, at, len, bytes } => {
                let old = file_at(pending, path)?.ok_or_else(|| OpError::NotFound(path.clone()))?;
                let size = old.len() as u64;
                if at.checked_add(*len).is_none_or(|end| end > size) {
                    return Err(OpError::OutOfRange {
                        path: path.clone(),
                        at: *at,
                        len: *len,
                        size,
                    });
                }
                let (at, end) = (*at as usize, (*at + *len) as usize);
                let new: Arc<[u8]> = [&old[..at], bytes, &old[end..]].concat().into();
                pending.insert(path.clone(), Some(new));
            }
            Op::Delete { path } => match file_at(pending, path) {
                Ok(Some(_)) => {
                    pending.insert(path.clone(), None);
                }
                Ok(None) => return Err(OpError::NotFound(path.clone())),
                Err(_) => {
                    for (file, _) in self.files_under(base, pending, path) {
                        pending.insert(file, None);
                    }
                    pending.insert(path.clone(), None);
                }
            },
            Op::Rename { from, to } => {
                if to.starts_with(&format!("{from}/")) {
                    return Err(OpError::BadPath(to.clone()));
                }
                if from == to {
                    return Err(OpError::Exists(to.clone()));
                }
                let moved: Vec<(String, Arc<[u8]>)> = match file_at(pending, from) {
                    Ok(Some(bytes)) => vec![(String::new(), bytes)],
                    Ok(None) => return Err(OpError::NotFound(from.clone())),
                    Err(_) => {
                        let under = self.files_under(base, pending, from);
                        under
                            .into_iter()
                            .map(|(file, bytes)| (file[from.len()..].to_owned(), bytes))
                            .collect()
                    }
                };
                match file_at(pending, to) {
                    Ok(None) => {}
                    _ => return Err(OpError::Exists(to.clone())),
                }
                no_file_above(pending, to)?;
                self.stage(base, pending, &Op::Delete { path: from.clone() })?;
                for (rest, bytes) in moved {
                    pending.insert(format!("{to}{rest}"), Some(bytes));
                }
            }
        }
        Ok(())
    }

    /// The directory `dir` with `changes` (paths below it, sorted) made to it: `None` when it is
    /// left with nothing in it.
    fn rebuild(
        &mut self,
        dir: Option<NodeId>,
        changes: &[(Vec<&str>, Option<Arc<[u8]>>)],
    ) -> Option<NodeId> {
        let mut entries: Vec<(NameId, NodeId)> =
            dir.and_then(|it| self.entries(it)).map(<[_]>::to_vec).unwrap_or_default();
        let mut start = 0;
        while start < changes.len() {
            let name = changes[start].0[0];
            let end = start + changes[start..].iter().take_while(|it| it.0[0] == name).count();
            let group = &changes[start..end];
            start = end;
            let name_id = self.name(name);
            let existing = entries.iter().position(|it| it.0 == name_id);
            let here = group.iter().find(|it| it.0.len() == 1).map(|it| &it.1);
            let deeper: Vec<(Vec<&str>, Option<Arc<[u8]>>)> = group
                .iter()
                .filter(|it| it.0.len() > 1)
                .map(|(path, bytes)| (path[1..].to_vec(), bytes.clone()))
                .collect();
            let child = match here {
                Some(here) if !deeper.iter().any(|it| it.1.is_some()) => {
                    here.clone().map(|bytes| self.file(bytes))
                }
                // A directory, made of what was there if that was one and nothing replaced it.
                _ => {
                    let was = existing.map(|at| entries[at].1).filter(|it| self.is_dir(*it));
                    self.rebuild(was.filter(|_| here.is_none()), &deeper)
                }
            };
            match (existing, child) {
                (Some(at), Some(child)) => entries[at].1 = child,
                (Some(at), None) => {
                    entries.remove(at);
                }
                (None, Some(child)) => entries.push((name_id, child)),
                (None, None) => {}
            }
        }
        if entries.is_empty() && dir != Some(self.empty) {
            return None;
        }
        Some(self.dir(entries))
    }

    /// The files that differ between `from` and `to`, in path order.
    pub fn diff(&self, from: NodeId, to: NodeId) -> Vec<Change> {
        let mut out = Vec::new();
        self.diff_into(Some(from), Some(to), &mut String::new(), &mut out);
        out
    }

    fn diff_into(
        &self,
        from: Option<NodeId>,
        to: Option<NodeId>,
        path: &mut String,
        out: &mut Vec<Change>,
    ) {
        if from == to {
            return;
        }
        let all = |node: NodeId, kind: ChangeKind, path: &str, out: &mut Vec<Change>| {
            out.extend(self.files(node, path).into_iter().map(|(path, _)| Change { path, kind }));
        };
        match (from, to) {
            (Some(a), Some(b)) => match (self.entries(a), self.entries(b)) {
                (None, None) => out.push(Change { path: path.clone(), kind: ChangeKind::Modified }),
                (Some(x), Some(y)) => {
                    let (mut i, mut j) = (0, 0);
                    while i < x.len() || j < y.len() {
                        let order = match (x.get(i), y.get(j)) {
                            (Some(a), Some(b)) => self.name_str(a.0).cmp(self.name_str(b.0)),
                            (Some(_), None) => std::cmp::Ordering::Less,
                            _ => std::cmp::Ordering::Greater,
                        };
                        let (name, left, right) = match order {
                            std::cmp::Ordering::Less => (x[i].0, Some(x[i].1), None),
                            std::cmp::Ordering::Greater => (y[j].0, None, Some(y[j].1)),
                            std::cmp::Ordering::Equal => (x[i].0, Some(x[i].1), Some(y[j].1)),
                        };
                        i += usize::from(order.is_le());
                        j += usize::from(order.is_ge());
                        let len = path.len();
                        if !path.is_empty() {
                            path.push('/');
                        }
                        path.push_str(self.name_str(name));
                        self.diff_into(left, right, path, out);
                        path.truncate(len);
                    }
                }
                _ => {
                    all(a, ChangeKind::Removed, path, out);
                    all(b, ChangeKind::Added, path, out);
                    out.sort();
                }
            },
            (Some(a), None) => all(a, ChangeKind::Removed, path, out),
            (None, Some(b)) => all(b, ChangeKind::Added, path, out),
            (None, None) => {}
        }
    }

    /// Drops every node no root in `live` reaches, and renumbers the rest; `live` is updated in
    /// place. Names are kept. Returns each old id's new one, `None` for a node dropped.
    pub fn collect(&mut self, live: &mut [NodeId]) -> Vec<Option<NodeId>> {
        let mut reached = vec![false; self.nodes.len()];
        let mut stack: Vec<NodeId> = live.iter().copied().chain([self.empty]).collect();
        while let Some(node) = stack.pop() {
            if std::mem::replace(&mut reached[node.0 as usize], true) {
                continue;
            }
            if let Some(entries) = self.entries(node) {
                stack.extend(entries.iter().map(|it| it.1));
            }
        }
        let mut renumbered = vec![NodeId(u32::MAX); self.nodes.len()];
        let mut kept = Vec::with_capacity(reached.iter().filter(|it| **it).count());
        // Children have smaller ids than their parents, so they are renumbered first.
        for (old, node) in std::mem::take(&mut self.nodes).into_iter().enumerate() {
            if !reached[old] {
                continue;
            }
            renumbered[old] = NodeId(kept.len() as u32);
            kept.push(match node {
                Node::Dir { hash, entries } => Node::Dir {
                    hash,
                    entries: entries
                        .iter()
                        .map(|&(name, child)| (name, renumbered[child.0 as usize]))
                        .collect(),
                },
                file => file,
            });
        }
        self.nodes = kept;
        self.by_hash = self
            .nodes
            .iter()
            .enumerate()
            .map(|(at, node)| {
                let (Node::File { hash, .. } | Node::Dir { hash, .. }) = node;
                (*hash, NodeId(at as u32))
            })
            .collect();
        self.empty = renumbered[self.empty.0 as usize];
        for root in live {
            *root = renumbered[root.0 as usize];
        }
        renumbered.into_iter().map(|it| (it.0 != u32::MAX).then_some(it)).collect()
    }
}

/// Whether the transaction deleted or replaced with a file a directory above `path`, so the
/// base's contents there are gone.
fn shadowed(pending: &Pending, path: &str) -> bool {
    ancestors(path).any(|up| pending.contains_key(up))
}

/// `a/b/c` -> `a/b`, `a`.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    let mut rest = path;
    std::iter::from_fn(move || {
        let at = rest.rfind('/')?;
        rest = &rest[..at];
        Some(rest)
    })
}

#[cfg(test)]
mod tests;
