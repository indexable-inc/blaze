//! The tree a root names (hub.md, "The tree"): a BLAKE3 Merkle tree of a checkout.
//!
//! - A **file** is its bytes under the VFS's own id, [`FILE_DOMAIN`]: BLAKE3's tree hash of
//!   them, so equal bytes are one object here, in the VFS and in the store.
//! - A **directory** maps names to a [`Kind`] and an id. It is a search tree whose shape is a
//!   function of its names alone: an entry whose name's hash ([`level`]) is at least `l + 1`
//!   ends a node of layer `l`, so a node holds [`FANOUT`] entries on average, a directory of
//!   100,000 entries is four layers, a change to one entry makes at most one new node a
//!   layer, and two directories of the same entries have the same root however they were
//!   made.
//! - A **root** is the id of the top directory's top node: the name of the exact tree.
//!
//! Every object is read through [`node`], which holds its bytes to the encoding before any
//! of it is used: layer, count, names in order, kinds, and that no entry inside a node is
//! one that would have ended it. Sizes and depths are bounded ([`ENTRIES_MOST`],
//! [`LAYERS_MOST`], [`DEPTH_MOST`], [`NAME_MOST`]).
//!
//! A change ([`apply`]) rebuilds each directory on the changed path from its entries: the
//! new nodes are few, the work is the directory's size. Splicing only the nodes a change
//! touches is hub.md's step 2.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

pub use blaze_vfs::FILE_DOMAIN;
use blaze_vfs::{Mode, Op};
use bstd::id::ContentId;
use serde::{Deserialize, Serialize};

use crate::{Plant, wire::BLOB_MOST};

/// The domain of a directory node's id.
pub const NODE_DOMAIN: &str = "blaze hub tree node 1";
/// The domain a name is hashed under to find where nodes end.
pub const EDGE_DOMAIN: &str = "blaze hub tree edge 1";
/// The entries a node holds on average: one name in this many ends a node.
pub const FANOUT: u32 = 32;
const FANOUT_BITS: u32 = 5;
/// The highest layer a node may be: `FANOUT` to this power is more entries than any
/// directory holds.
pub const LAYERS_MOST: u8 = 8;
/// The most entries one node may hold. A run this long with no name that ends a node does
/// not happen by chance (one in `e^130`); a directory made to have one is refused by name.
pub const ENTRIES_MOST: usize = 4096;
/// The most bytes one name may be.
pub const NAME_MOST: usize = 255;
/// The most directories deep a path may go.
pub const DEPTH_MOST: usize = 64;

/// What an entry of a directory is.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Kind {
    File(Mode),
    Dir,
}

impl Kind {
    fn code(self) -> u8 {
        match self {
            Kind::File(Mode::Regular) => 0,
            Kind::File(Mode::Executable) => 1,
            Kind::File(Mode::Symlink) => 2,
            Kind::Dir => 3,
        }
    }

    fn of_code(code: u8) -> Option<Kind> {
        Some(match code {
            0 => Kind::File(Mode::Regular),
            1 => Kind::File(Mode::Executable),
            2 => Kind::File(Mode::Symlink),
            3 => Kind::Dir,
            _ => return None,
        })
    }
}

/// One entry of a directory: a file's content id, or a directory's root.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub id: ContentId,
}

/// Where objects are: bytes under their content id. The hub's driver gives the daemon's own
/// (the VFS and the store's pool); [`Memory`] is the one of tests and of a hub with no
/// daemon yet. A store only gains objects here; what no root reaches is its collector's.
pub trait Store {
    fn get(&self, id: &ContentId) -> Option<&[u8]>;
    /// `bytes` under `id`, which the caller made from them.
    fn put(&mut self, id: ContentId, bytes: Vec<u8>);
}

/// Objects in memory.
#[derive(Default)]
pub struct Memory {
    // Ordered, so a dump of a test's store is the same every run.
    objects: BTreeMap<ContentId, Vec<u8>>,
}

impl Memory {
    pub fn new() -> Memory {
        Memory { objects: BTreeMap::new() }
    }

    /// How many objects it holds.
    pub fn len(&self) -> usize {
        self.objects.len()
    }
}

impl Store for Memory {
    fn get(&self, id: &ContentId) -> Option<&[u8]> {
        self.objects.get(id).map(Vec::as_slice)
    }

    fn put(&mut self, id: ContentId, bytes: Vec<u8>) {
        self.objects.insert(id, bytes);
    }
}

/// Why a tree cannot be read or changed. Each names the path or the object and the rule.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub enum TreeError {
    /// A path is relative, of at most [`DEPTH_MOST`] `/`-separated names, each 1 to
    /// [`NAME_MOST`] bytes, none `.` or `..`, with no NUL and no backslash.
    BadPath { path: String },
    NotFound { path: String },
    IsADirectory { path: String },
    NotADirectory { path: String },
    Exists { path: String },
    /// An edit of `len` bytes at `at` in a file of `size`.
    OutOfRange { at: u64, len: u64, path: String, size: u64 },
    TooBig { len: u64, path: String },
    /// A rename of a directory to a path inside itself.
    IntoItself { to: String, from: String },
    /// A directory with more than [`ENTRIES_MOST`] names in a row none of which ends a node.
    TooWide { name: String },
    /// The store lacks an object a root reaches.
    Missing { id: ContentId },
    /// An object's bytes are not a node of this encoding.
    Corrupt { id: ContentId, what: String },
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TreeError::BadPath { path } => write!(
                f,
                "the path {path:?} is refused: a path is relative, at most {DEPTH_MOST} names \
                 deep, each name 1 to {NAME_MOST} bytes and not '.' or '..', with no NUL and no \
                 backslash"
            ),
            TreeError::NotFound { path } => write!(f, "{path:?} is not in the tree"),
            TreeError::IsADirectory { path } => {
                write!(f, "{path:?} is a directory, where a file is needed")
            }
            TreeError::NotADirectory { path } => {
                write!(f, "{path:?} is a file, where a directory is needed")
            }
            TreeError::Exists { path } => {
                write!(f, "{path:?} exists already: a rename never replaces")
            }
            TreeError::OutOfRange { at, len, path, size } => write!(
                f,
                "an edit of {len} bytes at {at} is outside {path:?}, which is {size} bytes"
            ),
            TreeError::TooBig { len, path } => write!(
                f,
                "{path:?} would be {len} bytes, past the {BLOB_MOST} one file may be \
                 (wire::BLOB_MOST)"
            ),
            TreeError::IntoItself { to, from } => {
                write!(f, "{from:?} cannot be renamed to {to:?}, a path inside itself")
            }
            TreeError::TooWide { name } => write!(
                f,
                "a directory has more than {ENTRIES_MOST} names in a row from {name:?} none of \
                 which ends a node (tree::ENTRIES_MOST): names made to defeat the tree's shape"
            ),
            TreeError::Missing { id } => {
                write!(f, "the store lacks the object {id:x}, which the tree reaches")
            }
            TreeError::Corrupt { id, what } => {
                write!(f, "the object {id:x} is not a tree node: {what}")
            }
        }
    }
}

impl core::error::Error for TreeError {}

/// The layer up to which `name` ends a node: an entry ends the node of every layer below
/// its level. A function of the name alone, which is what makes a directory's shape one of
/// its names and not of its history.
pub fn level(name: &str) -> u8 {
    let hash = ContentId::of(EDGE_DOMAIN, name.as_bytes());
    let mut low = [0; 8];
    low.copy_from_slice(&hash.0[..8]);
    let level = u64::from_le_bytes(low).trailing_zeros() / FANOUT_BITS;
    level.min(LAYERS_MOST as u32) as u8
}

/// A directory node, read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Node {
    /// Layer 0: entries.
    Leaf(Vec<Entry>),
    /// A higher layer: the nodes of the layer below, each under the last name it holds.
    Inner { layer: u8, children: Vec<(String, ContentId)> },
}

fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MOST
        && name != "."
        && name != ".."
        && !name.bytes().any(|byte| byte == b'/' || byte == 0 || byte == b'\\')
}

/// `path`'s names, or [`TreeError::BadPath`].
pub fn parts(path: &str) -> Result<Vec<&str>, TreeError> {
    // alloc: a path's few names, for the one call.
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > DEPTH_MOST || !parts.iter().all(|part| name_ok(part)) {
        // alloc: error path.
        return Err(TreeError::BadPath { path: path.to_string() });
    }
    Ok(parts)
}

fn encode_leaf(entries: &[Entry]) -> Vec<u8> {
    // alloc: the node's bytes, which the store keeps.
    let mut out = Vec::with_capacity(3 + entries.len() * 48);
    out.push(0);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for entry in entries {
        out.push(entry.kind.code());
        out.push(entry.name.len() as u8);
        out.extend_from_slice(entry.name.as_bytes());
        out.extend_from_slice(&entry.id.0);
    }
    out
}

fn encode_inner(layer: u8, children: &[(String, ContentId)]) -> Vec<u8> {
    // alloc: the node's bytes, which the store keeps.
    let mut out = Vec::with_capacity(3 + children.len() * 48);
    out.push(layer);
    out.extend_from_slice(&(children.len() as u16).to_le_bytes());
    for (key, id) in children {
        out.push(key.len() as u8);
        out.extend_from_slice(key.as_bytes());
        out.extend_from_slice(&id.0);
    }
    out
}

/// A cursor over a node's bytes that never reads past them.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], &'static str> {
        let end = self.at.checked_add(len).filter(|end| *end <= self.bytes.len());
        let end = end.ok_or("it ends inside an entry")?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn name(&mut self) -> Result<&'a str, &'static str> {
        let len = self.take(1)?[0] as usize;
        let name = core::str::from_utf8(self.take(len)?).map_err(|_| "a name is not UTF-8")?;
        if !name_ok(name) {
            return Err("a name is empty, '.', '..' or holds '/', NUL or a backslash");
        }
        Ok(name)
    }

    fn id(&mut self) -> Result<ContentId, &'static str> {
        let mut id = [0; 32];
        id.copy_from_slice(self.take(32)?);
        Ok(ContentId(id))
    }
}

/// Whether `name` may stand where it does in a node of `layer`: after `before`, and, unless
/// it is the node's last, neither one that ends a node of this layer nor (above the entries)
/// one that no name of this layer ended.
fn in_order(layer: u8, name: &str, last: bool, before: Option<&str>) -> Result<(), &'static str> {
    if before.is_some_and(|before| before >= name) {
        return Err("its names are not in order, each once");
    }
    let level = level(name);
    if !last && level > layer {
        return Err("a name inside it is one that ends a node of its layer");
    }
    if !last && level < layer {
        return Err("it holds a node that no name of its layer ended");
    }
    Ok(())
}

/// `bytes` as a node, held to the encoding: nothing of a node that fails is used.
pub fn decode(bytes: &[u8]) -> Result<Node, &'static str> {
    let mut from = Cursor { bytes, at: 0 };
    let layer = from.take(1)?[0];
    if layer > LAYERS_MOST {
        return Err("its layer is past tree::LAYERS_MOST");
    }
    let count = from.take(2)?;
    let count = u16::from_le_bytes([count[0], count[1]]) as usize;
    if count > ENTRIES_MOST {
        return Err("it holds more than tree::ENTRIES_MOST entries");
    }
    let mut before: Option<&str> = None;
    let node = if layer == 0 {
        // alloc: the node read, for the one call.
        let mut entries = Vec::with_capacity(count);
        for at in 0..count {
            let kind = Kind::of_code(from.take(1)?[0]).ok_or("an entry's kind is none")?;
            let name = from.name()?;
            in_order(layer, name, at + 1 == count, before)?;
            before = Some(name);
            // alloc: the node read, for the one call.
            entries.push(Entry { name: name.to_string(), kind, id: from.id()? });
        }
        Node::Leaf(entries)
    } else {
        if count == 0 {
            return Err("a node above the entries holds none");
        }
        // alloc: the node read, for the one call.
        let mut children = Vec::with_capacity(count);
        for at in 0..count {
            let name = from.name()?;
            in_order(layer, name, at + 1 == count, before)?;
            before = Some(name);
            // alloc: the node read, for the one call.
            children.push((name.to_string(), from.id()?));
        }
        Node::Inner { layer, children }
    };
    if from.at != bytes.len() {
        return Err("bytes follow its last entry");
    }
    Ok(node)
}

/// The node `id`, read from `store` and checked.
pub fn node<S: Store>(store: &S, id: ContentId) -> Result<Node, TreeError> {
    let bytes = store.get(&id).ok_or(TreeError::Missing { id })?;
    // alloc: error path.
    decode(bytes).map_err(|what| TreeError::Corrupt { id, what: what.to_string() })
}

fn corrupt(id: ContentId, what: &str) -> TreeError {
    // alloc: error path.
    TreeError::Corrupt { id, what: what.to_string() }
}

/// Every entry of the directory `dir`, in name order.
pub fn list<S: Store>(store: &S, dir: ContentId) -> Result<Vec<Entry>, TreeError> {
    // alloc: the directory's entries, for the one call.
    let mut out = Vec::new();
    gather(store, dir, None, &mut out)?;
    Ok(out)
}

/// The entries under node `id` onto `out`. `under` is what its parent says of it: its layer
/// and the last name it holds. The layers only fall, so the descent is at most
/// [`LAYERS_MOST`] deep.
fn gather<S: Store>(
    store: &S,
    id: ContentId,
    under: Option<(u8, &str)>,
    out: &mut Vec<Entry>,
) -> Result<(), TreeError> {
    match node(store, id)? {
        Node::Leaf(mut entries) => {
            if let Some((layer, last)) = under {
                if layer != 0 {
                    return Err(corrupt(id, "its layer is not the one below its parent's"));
                }
                if entries.last().map(|entry| entry.name.as_str()) != Some(last) {
                    return Err(corrupt(id, "its last name is not the one its parent has for it"));
                }
            }
            out.append(&mut entries);
        }
        Node::Inner { layer, children } => {
            if let Some((expected, last)) = under {
                if layer != expected {
                    return Err(corrupt(id, "its layer is not the one below its parent's"));
                }
                if children.last().map(|(key, _)| key.as_str()) != Some(last) {
                    return Err(corrupt(id, "its last name is not the one its parent has for it"));
                }
            }
            for (key, child) in &children {
                gather(store, *child, Some((layer - 1, key.as_str())), out)?;
            }
        }
    }
    Ok(())
}

/// The entry `name` of the directory `dir`: one node a layer, never the whole directory.
pub fn entry<S: Store>(
    store: &S,
    dir: ContentId,
    name: &str,
) -> Result<Option<(Kind, ContentId)>, TreeError> {
    let mut at = dir;
    // The layers only fall: a descent that takes more steps than there are layers is a node
    // that lied about its layer.
    for _ in 0..=LAYERS_MOST as usize + 1 {
        match node(store, at)? {
            Node::Leaf(entries) => {
                let found = entries.binary_search_by(|entry| entry.name.as_str().cmp(name));
                return Ok(found.ok().map(|at| (entries[at].kind, entries[at].id)));
            }
            Node::Inner { children, .. } => {
                // The first child whose last name is not before `name` is the one to hold it.
                match children.iter().find(|(key, _)| key.as_str() >= name) {
                    Some((_, child)) => at = *child,
                    None => return Ok(None),
                }
            }
        }
    }
    Err(corrupt(dir, "its nodes go deeper than tree::LAYERS_MOST"))
}

/// What `path` is under `root`; `""` is the root itself.
pub fn find<S: Store>(
    store: &S,
    root: ContentId,
    path: &str,
) -> Result<Option<(Kind, ContentId)>, TreeError> {
    if path.is_empty() {
        return Ok(Some((Kind::Dir, root)));
    }
    let mut at = (Kind::Dir, root);
    for part in parts(path)? {
        let (Kind::Dir, dir) = at else { return Ok(None) };
        match entry(store, dir, part)? {
            Some(found) => at = found,
            None => return Ok(None),
        }
    }
    Ok(Some(at))
}

fn put_node<S: Store>(store: &mut S, bytes: Vec<u8>) -> ContentId {
    let id = ContentId::of(NODE_DOMAIN, &bytes);
    store.put(id, bytes);
    id
}

/// The directory of `entries` (in name order, each name once), written to `store`: its root.
pub fn build<S: Store>(store: &mut S, entries: &[Entry]) -> Result<ContentId, TreeError> {
    build_planted(store, entries, Plant::None)
}

pub(crate) fn build_planted<S: Store>(
    store: &mut S,
    entries: &[Entry],
    plant: Plant,
) -> Result<ContentId, TreeError> {
    if entries.is_empty() {
        return Ok(put_node(store, encode_leaf(&[])));
    }
    // alloc: the layer above, a name and an id for each node of this one.
    let mut items: Vec<(String, ContentId)> = Vec::new();
    let mut from = 0;
    for (at, entry) in entries.iter().enumerate() {
        let ends = match plant {
            // The planted bug: a node ends where it is full, a function of where an entry
            // stands and not of what it is.
            Plant::NodesEndByCount => (at + 1) % FANOUT as usize == 0,
            _ => level(&entry.name) >= 1,
        };
        if ends || at + 1 == entries.len() {
            let run = &entries[from..=at];
            if run.len() > ENTRIES_MOST {
                // alloc: error path.
                return Err(TreeError::TooWide { name: run[0].name.to_string() });
            }
            // clone: the name the layer above files this node under.
            items.push((entry.name.clone(), put_node(store, encode_leaf(run))));
            from = at + 1;
        }
    }
    for layer in 1..=LAYERS_MOST {
        if items.len() == 1 {
            break;
        }
        // alloc: the layer above, a name and an id for each node of this one.
        let mut above: Vec<(String, ContentId)> = Vec::new();
        let mut from = 0;
        for at in 0..items.len() {
            let ends = match plant {
                Plant::NodesEndByCount => (at + 1) % FANOUT as usize == 0,
                _ => level(&items[at].0) > layer,
            };
            if ends || at + 1 == items.len() {
                let run = &items[from..=at];
                if run.len() > ENTRIES_MOST {
                    // alloc: error path.
                    return Err(TreeError::TooWide { name: run[0].0.to_string() });
                }
                // clone: the name the layer above files this node under.
                above.push((items[at].0.clone(), put_node(store, encode_inner(layer, run))));
                from = at + 1;
            }
        }
        items = above;
    }
    // No name's level is past `LAYERS_MOST`, so the top layer is one node.
    Ok(items[0].1)
}

/// The root of the tree with no file.
pub fn empty<S: Store>(store: &mut S) -> ContentId {
    put_node(store, encode_leaf(&[]))
}

/// The file `id`'s bytes.
pub fn file<S: Store>(store: &S, id: ContentId) -> Result<&[u8], TreeError> {
    store.get(&id).ok_or(TreeError::Missing { id })
}

/// `bytes` written as a file: its id.
pub fn write<S: Store>(store: &mut S, bytes: Vec<u8>) -> ContentId {
    let id = ContentId::of(FILE_DOMAIN, &bytes);
    store.put(id, bytes);
    id
}

/// The directory `dir` with what is at `names` set to `new` (`None` removes it): its new
/// root, or `None` once it holds nothing, as git keeps no empty directory. `path` is the
/// whole path, for an error to name.
fn set<S: Store>(
    store: &mut S,
    dir: Option<ContentId>,
    names: &[&str],
    new: Option<(Kind, ContentId)>,
    path: &str,
    plant: Plant,
) -> Result<Option<ContentId>, TreeError> {
    let mut entries = match dir {
        Some(dir) => list(store, dir)?,
        // alloc: a new directory's entries, for the one call.
        None => Vec::new(),
    };
    let name = names[0];
    let found = entries.binary_search_by(|entry| entry.name.as_str().cmp(name));
    if names.len() == 1 {
        match (found, new) {
            (Ok(at), Some((kind, id))) => {
                if entries[at].kind == Kind::Dir && kind != Kind::Dir {
                    // alloc: error path.
                    return Err(TreeError::IsADirectory { path: path.to_string() });
                }
                entries[at].kind = kind;
                entries[at].id = id;
            }
            (Ok(at), None) => {
                entries.remove(at);
            }
            (Err(at), Some((kind, id))) => {
                // alloc: the entry's name, which the node's bytes take.
                entries.insert(at, Entry { name: name.to_string(), kind, id });
            }
            // alloc: error path.
            (Err(_), None) => return Err(TreeError::NotFound { path: path.to_string() }),
        }
    } else {
        let child = match found {
            Ok(at) if entries[at].kind == Kind::Dir => Some(entries[at].id),
            // alloc: error path.
            Ok(_) => return Err(TreeError::NotADirectory { path: path.to_string() }),
            // alloc: error path.
            Err(_) if new.is_none() => return Err(TreeError::NotFound { path: path.to_string() }),
            Err(_) => None,
        };
        let made = set(store, child, &names[1..], new, path, plant)?;
        match (made, found) {
            (Some(id), Ok(at)) => entries[at].id = id,
            (Some(id), Err(at)) => {
                // alloc: the entry's name, which the node's bytes take.
                entries.insert(at, Entry { name: name.to_string(), kind: Kind::Dir, id });
            }
            (None, Ok(at)) => match plant {
                // The planted bug: a directory emptied stays, as an entry with no file under
                // it, so a tree's root says how it was made.
                Plant::KeepsEmptyDirs => entries[at].id = empty(store),
                _ => {
                    entries.remove(at);
                }
            },
            (None, Err(_)) => {}
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }
    build_planted(store, &entries, plant).map(Some)
}

/// `root` with `ops` applied in order: the new root. The store only gains objects, so a
/// failure part way leaves `root` what it was: the caller moves nothing until this returns.
pub fn apply<S: Store>(store: &mut S, root: ContentId, ops: &[Op]) -> Result<ContentId, (usize, TreeError)> {
    apply_planted(store, root, ops, Plant::None).map_err(|(at, _, error)| (at, error))
}

/// [`apply`], which on a failure also says the root as it was after the ops before the one
/// that failed (what a planted bug keeps).
pub(crate) fn apply_planted<S: Store>(
    store: &mut S,
    root: ContentId,
    ops: &[Op],
    plant: Plant,
) -> Result<ContentId, (usize, ContentId, TreeError)> {
    let mut at = root;
    for (index, op) in ops.iter().enumerate() {
        at = one(store, at, op, plant).map_err(|error| (index, at, error))?;
    }
    Ok(at)
}

fn replace<S: Store>(
    store: &mut S,
    root: ContentId,
    path: &str,
    new: Option<(Kind, ContentId)>,
    plant: Plant,
) -> Result<ContentId, TreeError> {
    let names = parts(path)?;
    match set(store, Some(root), &names, new, path, plant)? {
        Some(root) => Ok(root),
        None => Ok(empty(store)),
    }
}

fn one<S: Store>(store: &mut S, root: ContentId, op: &Op, plant: Plant) -> Result<ContentId, TreeError> {
    match op {
        Op::Write { mode, path, bytes } => {
            if bytes.len() as u64 > BLOB_MOST {
                // alloc: error path.
                return Err(TreeError::TooBig { len: bytes.len() as u64, path: path.to_string() });
            }
            // clone: the file's bytes, which the store keeps.
            let id = write(store, bytes.clone());
            replace(store, root, path, Some((Kind::File(*mode), id)), plant)
        }
        Op::Edit { at, len, path, bytes } => {
            let (kind, id) = match find(store, root, path)? {
                Some((Kind::Dir, _)) => {
                    // alloc: error path.
                    return Err(TreeError::IsADirectory { path: path.to_string() });
                }
                Some(found) => found,
                // alloc: error path.
                None => return Err(TreeError::NotFound { path: path.to_string() }),
            };
            let old = file(store, id)?;
            let size = old.len() as u64;
            let end = at.checked_add(*len).filter(|end| *end <= size);
            let Some(end) = end else {
                // alloc: error path.
                return Err(TreeError::OutOfRange { at: *at, len: *len, path: path.to_string(), size });
            };
            let new_len = size - len + bytes.len() as u64;
            if new_len > BLOB_MOST {
                // alloc: error path.
                return Err(TreeError::TooBig { len: new_len, path: path.to_string() });
            }
            // alloc: the file's new bytes, which the store keeps.
            let mut new = Vec::with_capacity(new_len as usize);
            new.extend_from_slice(&old[..*at as usize]);
            new.extend_from_slice(bytes);
            new.extend_from_slice(&old[end as usize..]);
            let id = write(store, new);
            replace(store, root, path, Some((kind, id)), plant)
        }
        Op::Delete { path } => replace(store, root, path, None, plant),
        Op::Rename { to, from } => {
            parts(to)?;
            let Some(moved) = find(store, root, from)? else {
                // alloc: error path.
                return Err(TreeError::NotFound { path: from.to_string() });
            };
            if to == from || to.strip_prefix(from.as_str()).is_some_and(|rest| rest.starts_with('/')) {
                // alloc: error path.
                return Err(TreeError::IntoItself { to: to.to_string(), from: from.to_string() });
            }
            if find(store, root, to)?.is_some() {
                // alloc: error path.
                return Err(TreeError::Exists { path: to.to_string() });
            }
            let without = replace(store, root, from, None, plant)?;
            replace(store, without, to, Some(moved), plant)
        }
    }
}
