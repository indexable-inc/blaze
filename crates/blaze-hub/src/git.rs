//! Git is the origin and the hub is an index of it (hub.md, "Git"): every file and every
//! directory the hub holds is known by two names, its BLAKE3 content id and its git object
//! id (SHA-1, or SHA-256 in a repository that uses it), in one table ([`Index`]). A commit's
//! tree is imported to a root once per tree id ([`Index::import`]), an object can be asked
//! for by either name, and an export hashes only what has no git id yet ([`Index::oid`]).
//!
//! What of git has an explicit rule here:
//!
//! - **Modes.** `100644`, `100755`, `120000` (a symlink, its blob the target) and `40000` (a
//!   directory) are the tree's own kinds. Any other mode (`100664` of old gits among them)
//!   is refused by name: the tree written back would not be the tree read.
//! - **Order.** Git sorts a directory as if its name ended in `/`; the tree here sorts by
//!   name. [`tree_body`] writes git's order.
//! - **Canonical trees only.** An imported tree whose bytes are not the ones [`tree_body`]
//!   writes for its entries is refused: every tree in the index round-trips byte for byte.
//! - **Empty directories** do not exist in either.
//! - **Submodules** (`160000`) are refused by name until an entry can hold a commit (hub.md).
//! - **LFS pointers and `.gitattributes` filters.** A root holds the repository's form of a
//!   file, the bytes git stores: a pointer file stays a pointer file and no line ending is
//!   converted. An agent reads and writes that form.
//! - **Names** hold no `/`, NUL or backslash and are not `.` or `..`, as [`crate::tree`]
//!   holds them; a tree with another name is refused.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

use blaze_vfs::Mode;
use bstd::id::ContentId;

use crate::{
    tree::{self, DEPTH_MOST, Entry, Kind, Store, TreeError},
    wire::BLOB_MOST,
};

/// Which hash a repository names its objects by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Sha1,
    Sha256,
}

/// A git object id: 20 bytes of SHA-1 or 32 of SHA-256.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Oid {
    len: u8,
    bytes: [u8; 32],
}

impl Oid {
    /// `bytes` as an id; `None` unless they are 20 or 32.
    pub fn from_bytes(bytes: &[u8]) -> Option<Oid> {
        if bytes.len() != 20 && bytes.len() != 32 {
            return None;
        }
        let mut id = [0; 32];
        id[..bytes.len()].copy_from_slice(bytes);
        Some(Oid { len: bytes.len() as u8, bytes: id })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_bytes().iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

fn compress(state: &mut [u32; 5], block: &[u8]) {
    let mut w = [0u32; 80];
    for at in 0..16 {
        w[at] = u32::from_be_bytes([
            block[4 * at],
            block[4 * at + 1],
            block[4 * at + 2],
            block[4 * at + 3],
        ]);
    }
    for at in 16..80 {
        w[at] = (w[at - 3] ^ w[at - 8] ^ w[at - 14] ^ w[at - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *state;
    for (at, word) in w.iter().enumerate() {
        let (f, k): (u32, u32) = match at {
            0..=19 => ((b & c) | (!b & d), 0x5a82_7999),
            20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
            _ => (b ^ c ^ d, 0xca62_c1d6),
        };
        let next = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = next;
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
}

/// The SHA-1 of `bytes` (FIPS 180-4): git's object name, and nothing else's. It is not a
/// hash blaze trusts for anything: every object is verified by its BLAKE3 id.
pub fn sha1(bytes: &[u8]) -> [u8; 20] {
    let mut state = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0];
    let mut blocks = bytes.chunks_exact(64);
    for block in &mut blocks {
        compress(&mut state, block);
    }
    let rest = blocks.remainder();
    let mut tail = [0u8; 128];
    tail[..rest.len()].copy_from_slice(rest);
    tail[rest.len()] = 0x80;
    let total = if rest.len() < 56 { 64 } else { 128 };
    tail[total - 8..total].copy_from_slice(&((bytes.len() as u64) * 8).to_be_bytes());
    for block in tail[..total].chunks_exact(64) {
        compress(&mut state, block);
    }
    let mut out = [0; 20];
    for (at, word) in state.iter().enumerate() {
        out[4 * at..4 * at + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// The id git gives an object of `kind` (`blob`, `tree`) with this body.
pub fn object_id(format: Format, kind: &str, body: &[u8]) -> Oid {
    // alloc: the object as git hashes it, its header and its body, for the one hash.
    let mut object = Vec::with_capacity(kind.len() + 24 + body.len());
    object.extend_from_slice(kind.as_bytes());
    object.push(b' ');
    // alloc: the length in decimal, for the one hash.
    object.extend_from_slice(body.len().to_string().as_bytes());
    object.push(0);
    object.extend_from_slice(body);
    let (len, mut bytes) = (if format == Format::Sha1 { 20 } else { 32 }, [0; 32]);
    match format {
        Format::Sha1 => bytes[..20].copy_from_slice(&sha1(&object)),
        Format::Sha256 => bytes = bstd::digest::sha256(&object),
    }
    Oid { len, bytes }
}

fn mode_text(kind: Kind) -> &'static str {
    match kind {
        Kind::File(Mode::Regular) => "100644",
        Kind::File(Mode::Executable) => "100755",
        Kind::File(Mode::Symlink) => "120000",
        Kind::Dir => "40000",
    }
}

/// A git tree object's body for `entries`, each with its git id: git's order (a directory
/// sorts as its name and `/`), each entry its mode, a space, its name, a NUL and its raw id.
pub fn tree_body(entries: &[(Entry, Oid)]) -> Vec<u8> {
    // alloc: the entries in git's order, for the one call.
    let mut order: Vec<&(Entry, Oid)> = entries.iter().collect();
    let key = |entry: &Entry| {
        // alloc: the name git sorts by, for the one call.
        let mut key = entry.name.as_bytes().to_vec();
        if entry.kind == Kind::Dir {
            key.push(b'/');
        }
        key
    };
    order.sort_by(|a, b| key(&a.0).cmp(&key(&b.0)));
    // alloc: the tree object's body, which the caller hashes or serves.
    let mut out = Vec::new();
    for (entry, oid) in order {
        out.extend_from_slice(mode_text(entry.kind).as_bytes());
        out.push(b' ');
        out.extend_from_slice(entry.name.as_bytes());
        out.push(0);
        out.extend_from_slice(oid.as_bytes());
    }
    out
}

/// Git's objects, as an import reads them: a tree's or a blob's body under its id. The
/// driver gives the hub's own clone; nothing read through it is used before its id checks.
pub trait Objects {
    fn object(&self, oid: &Oid) -> Option<&[u8]>;
}

/// Why a git tree is not imported. Each names the object or the entry and the rule.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GitError {
    /// The clone lacks an object the tree reaches.
    Missing { oid: Oid },
    /// An object's bytes are not the ones its id names.
    Forged { oid: Oid, is: Oid },
    /// A tree object that does not parse, or is not the bytes git writes for its entries.
    NotCanonical { oid: Oid, what: &'static str },
    /// An entry whose mode the tree has no kind for.
    Mode { mode: String, name: String, tree: Oid },
    Submodule { name: String, tree: Oid },
    Name { name: String, tree: Oid },
    TooBig { len: u64, name: String, tree: Oid },
    TooDeep { tree: Oid },
    Tree(TreeError),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::Missing { oid } => {
                write!(f, "the clone lacks the git object {oid}, which the tree reaches")
            }
            GitError::Forged { oid, is } => write!(
                f,
                "the bytes read as the git object {oid} hash to {is}: nothing of them is used"
            ),
            GitError::NotCanonical { oid, what } => write!(
                f,
                "the git tree {oid} is refused: {what} (only a tree that is written back byte \
                 for byte is imported)"
            ),
            GitError::Mode { mode, name, tree } => write!(
                f,
                "{name:?} in the git tree {tree} has the mode {mode}, which is none of 100644, \
                 100755, 120000 and 40000"
            ),
            GitError::Submodule { name, tree } => write!(
                f,
                "{name:?} in the git tree {tree} is a submodule; the hub does not import \
                 submodules yet (hub.md, \"Git\")"
            ),
            GitError::Name { name, tree } => write!(
                f,
                "the name {name:?} in the git tree {tree} is refused: a name is 1 to 255 bytes \
                 of UTF-8 with no '/', NUL or backslash"
            ),
            GitError::TooBig { len, name, tree } => write!(
                f,
                "{name:?} in the git tree {tree} is {len} bytes, past the {BLOB_MOST} one file \
                 may be (wire::BLOB_MOST)"
            ),
            GitError::TooDeep { tree } => write!(
                f,
                "the git tree {tree} is more than {DEPTH_MOST} directories deep \
                 (tree::DEPTH_MOST)"
            ),
            GitError::Tree(error) => write!(f, "{error}"),
        }
    }
}

impl core::error::Error for GitError {}

/// The one table of an object's two names: its content id and its git id.
pub struct Index {
    format: Format,
    // Ordered, so a dump is the same every run.
    oids: BTreeMap<ContentId, Oid>,
    ids: BTreeMap<Oid, ContentId>,
}

impl Index {
    pub fn new(format: Format) -> Index {
        Index { format, oids: BTreeMap::new(), ids: BTreeMap::new() }
    }

    pub fn format(&self) -> Format {
        self.format
    }

    /// How many objects have both names.
    pub fn len(&self) -> usize {
        self.oids.len()
    }

    /// The file or the root a git id names, where the index has it.
    pub fn id_of(&self, oid: &Oid) -> Option<ContentId> {
        self.ids.get(oid).copied()
    }

    /// The git id of a file or a root, where the index has it.
    pub fn known(&self, id: &ContentId) -> Option<Oid> {
        self.oids.get(id).copied()
    }

    fn record(&mut self, id: ContentId, oid: Oid) {
        self.oids.insert(id, oid);
        self.ids.insert(oid, id);
    }

    /// The git id of the file or directory `id`, hashing only what the index has no id for:
    /// after a change, the directories on the changed path and the files written.
    pub fn oid<S: Store>(&mut self, store: &S, kind: Kind, id: ContentId) -> Result<Oid, TreeError> {
        self.oid_at(store, kind, id, 0)
    }

    fn oid_at<S: Store>(
        &mut self,
        store: &S,
        kind: Kind,
        id: ContentId,
        depth: usize,
    ) -> Result<Oid, TreeError> {
        if let Some(oid) = self.oids.get(&id) {
            return Ok(*oid);
        }
        let oid = match kind {
            Kind::File(_) => object_id(self.format, "blob", tree::file(store, id)?),
            Kind::Dir => object_id(self.format, "tree", &self.body_at(store, id, depth)?),
        };
        self.record(id, oid);
        Ok(oid)
    }

    /// The body of the git tree object for the directory `dir`: what a `git fetch` of a root
    /// is served.
    pub fn tree_object<S: Store>(&mut self, store: &S, dir: ContentId) -> Result<Vec<u8>, TreeError> {
        self.body_at(store, dir, 0)
    }

    fn body_at<S: Store>(
        &mut self,
        store: &S,
        dir: ContentId,
        depth: usize,
    ) -> Result<Vec<u8>, TreeError> {
        if depth > DEPTH_MOST {
            // alloc: error path.
            return Err(TreeError::Corrupt {
                id: dir,
                what: "its directories go deeper than tree::DEPTH_MOST".to_string(),
            });
        }
        let entries = tree::list(store, dir)?;
        // alloc: each entry with its git id, for the one call.
        let mut with = Vec::with_capacity(entries.len());
        for entry in entries {
            let oid = self.oid_at(store, entry.kind, entry.id, depth + 1)?;
            with.push((entry, oid));
        }
        Ok(tree_body(&with))
    }

    /// The root of the git tree `tree`, read from `git`: every object checked against its id
    /// before it is used, every tree held to the rules in this module's documentation. A
    /// tree the index has is answered from it, so importing a commit costs what differs from
    /// the commits already imported.
    pub fn import<S: Store, G: Objects>(
        &mut self,
        store: &mut S,
        git: &G,
        tree: Oid,
    ) -> Result<ContentId, GitError> {
        self.import_at(store, git, tree, 0)
    }

    fn checked<'a, G: Objects>(&self, git: &'a G, kind: &str, oid: Oid) -> Result<&'a [u8], GitError> {
        let body = git.object(&oid).ok_or(GitError::Missing { oid })?;
        let is = object_id(self.format, kind, body);
        if is != oid {
            return Err(GitError::Forged { oid, is });
        }
        Ok(body)
    }

    fn import_at<S: Store, G: Objects>(
        &mut self,
        store: &mut S,
        git: &G,
        tree: Oid,
        depth: usize,
    ) -> Result<ContentId, GitError> {
        if let Some(root) = self.ids.get(&tree) {
            return Ok(*root);
        }
        if depth > DEPTH_MOST {
            return Err(GitError::TooDeep { tree });
        }
        let body = self.checked(git, "tree", tree)?;
        let id_len = if self.format == Format::Sha1 { 20 } else { 32 };
        let bad = |what| GitError::NotCanonical { oid: tree, what };
        // alloc: the tree's entries, for the one call.
        let mut with: Vec<(Entry, Oid)> = Vec::new();
        let mut rest = body;
        while !rest.is_empty() {
            let space = rest.iter().position(|byte| *byte == b' ');
            let space = space.ok_or_else(|| bad("an entry has no mode"))?;
            let nul = rest.iter().position(|byte| *byte == 0);
            let nul = nul.filter(|nul| *nul > space).ok_or_else(|| bad("an entry's name does not end"))?;
            if rest.len() < nul + 1 + id_len {
                return Err(bad("it ends inside an entry's id"));
            }
            let mode = &rest[..space];
            let name = &rest[space + 1..nul];
            let oid = Oid::from_bytes(&rest[nul + 1..nul + 1 + id_len]);
            let oid = oid.ok_or_else(|| bad("an entry's id is not an id"))?;
            rest = &rest[nul + 1 + id_len..];
            // alloc: the entry's name, which the node's bytes take; lossy only in an error.
            let name = match core::str::from_utf8(name) {
                Ok(name) if tree::parts(name).is_ok_and(|parts| parts.len() == 1) => name.to_string(),
                _ => {
                    let name = String::from_utf8_lossy(name).into_owned();
                    return Err(GitError::Name { name, tree });
                }
            };
            let kind = match mode {
                b"100644" => Kind::File(Mode::Regular),
                b"100755" => Kind::File(Mode::Executable),
                b"120000" => Kind::File(Mode::Symlink),
                b"40000" => Kind::Dir,
                b"160000" => return Err(GitError::Submodule { name, tree }),
                _ => {
                    let mode = String::from_utf8_lossy(mode).into_owned();
                    return Err(GitError::Mode { mode, name, tree });
                }
            };
            let known = self.ids.get(&oid).copied();
            let id = match (kind, known) {
                (_, Some(id)) => id,
                (Kind::Dir, None) => self.import_at(store, git, oid, depth + 1)?,
                (Kind::File(_), None) => {
                    let bytes = self.checked(git, "blob", oid)?;
                    if bytes.len() as u64 > BLOB_MOST {
                        return Err(GitError::TooBig { len: bytes.len() as u64, name, tree });
                    }
                    // alloc: the file's bytes, which the store keeps.
                    let id = tree::write(store, bytes.to_vec());
                    self.record(id, oid);
                    id
                }
            };
            with.push((Entry { name, kind, id }, oid));
        }
        // The bytes git would write for these entries are the bytes read, or the tree is
        // not one the index can give back: an order or a mode git itself would not write.
        if tree_body(&with) != body {
            return Err(bad("its bytes are not the ones git writes for its entries"));
        }
        // alloc: the entries in the tree's own order, by name.
        let mut entries: Vec<Entry> = with.into_iter().map(|(entry, _)| entry).collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        if entries.windows(2).any(|pair| pair[0].name == pair[1].name) {
            return Err(bad("it names an entry twice"));
        }
        if entries.is_empty() && depth > 0 {
            return Err(bad("it is an empty directory inside a tree"));
        }
        let root = tree::build(store, &entries).map_err(GitError::Tree)?;
        self.record(root, tree);
        Ok(root)
    }
}
