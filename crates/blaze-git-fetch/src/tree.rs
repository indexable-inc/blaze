//! A commit's files: its tree walked from an [`Index`](crate::Index), each file handed out with its path,
//! mode and the blob to read. A name that could leave the checkout or mean something to git (`..`, `.`, `.git`
//! in any case, a `/` or `\`, not UTF-8) is refused naming its path, as is a submodule (a
//! gitlink), which needs a second fetch this walk does not make.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt};

use blaze_own::{AllocFailure, Vec, planted::Planted};

use crate::{
    oid::{Kind, Oid},
    pack::{Found, PackError, PackWhy, PlantedIndex},
    plant::GitFetchPlant,
};

/// How much a tree may hold: every field is the caller's choice, [`Limits::SHIPPED`] the values
/// a real repository is walked under (D165).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most files (not directories) handed out.
    pub most_files: u32,
    /// The most bytes of one path.
    pub most_path: u16,
    /// The most directories deep: a directory of `k` components past it is refused.
    pub most_depth: u16,
}

impl Limits {
    /// For real repositories: 200,000 files, 4 KiB paths, 128 directories deep (blaze-fetch's
    /// `Limits::REAL` names the same values).
    pub const SHIPPED: Limits = Limits { most_files: 200_000, most_path: 4096, most_depth: 128 };
}

/// A file's mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    File,
    Exec,
    /// A symbolic link: its bytes are the target, for the caller to check before making it.
    Symlink,
}

/// One file of the tree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct File<'a> {
    /// `/`-separated, relative, each part checked.
    pub path: &'a str,
    pub mode: Mode,
    pub oid: Oid,
    /// The blob: its size, and what [`Index::read`](crate::Index::read) takes.
    pub object: Found,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TreeWhy {
    /// The object is missing from the pack.
    Missing { oid: Oid },
    /// The object is of another kind than the walk needs.
    NotKind { oid: Oid, want: Kind },
    /// The commit has no `tree` line first.
    Commit,
    /// A tree entry is cut short or has no `mode name\0id` shape.
    Entry,
    /// A name that is empty, `.`, `..`, `.git`, holds `/` or `\`, or is not UTF-8.
    Name,
    Mode { mode: u32 },
    /// A gitlink: a submodule's commit.
    Submodule,
    PathOver { most: u16 },
    FilesOver { most: u32 },
    DepthOver { most: u16 },
    /// A tree or commit whose bytes did not read back from the pack.
    Read(PackWhy),
    Alloc(AllocFailure),
}

/// A refused tree: the path it was found at (cut to 96 bytes) and why.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TreeError {
    path: [u8; 96],
    len: u8,
    pub why: TreeWhy,
}

impl TreeError {
    fn new(path: &[u8], why: TreeWhy) -> TreeError {
        let mut copy = [0u8; 96];
        let len = path.len().min(96);
        copy[..len].copy_from_slice(&path[..len]);
        TreeError { path: copy, len: len as u8, why }
    }

    /// The path's bytes, as far as kept.
    pub fn path(&self) -> &[u8] {
        &self.path[..usize::from(self.len)]
    }
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("git tree at `")?;
        for &b in self.path() {
            match b {
                b' '..=b'~' => write!(f, "{}", b as char)?,
                _ => write!(f, "\\x{b:02x}")?,
            }
        }
        f.write_str("`: ")?;
        match self.why {
            TreeWhy::Missing { oid } => write!(f, "object {oid} is not in the pack"),
            TreeWhy::NotKind { oid, want } => write!(f, "object {oid} is no {}", want.name()),
            TreeWhy::Commit => f.write_str("the commit has no tree"),
            TreeWhy::Entry => f.write_str("a tree entry with no `mode name\\0id` shape"),
            TreeWhy::Name => f.write_str("a name that is empty, `.`, `..` or `.git`, holds a slash, or is not UTF-8"),
            TreeWhy::Mode { mode } => write!(f, "mode {mode:o}, no file, link or directory"),
            TreeWhy::Submodule => f.write_str("a submodule, which this fetch does not follow"),
            TreeWhy::PathOver { most } => write!(f, "a path longer than the most allowed, {most} bytes"),
            TreeWhy::FilesOver { most } => write!(f, "more files than the most allowed, {most}"),
            TreeWhy::DepthOver { most } => write!(f, "directories deeper than the most allowed, {most}"),
            TreeWhy::Read(why) => write!(f, "{}", PackError { entry: None, at: 0, why }),
            TreeWhy::Alloc(e) => write!(f, "{e}"),
        }
    }
}

impl fmt::Debug for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TreeError({self})")
    }
}

impl core::error::Error for TreeError {}

/// One directory being walked: its bytes, the next entry's offset, and its path's length.
struct Open<B: Allocator> {
    tree: Vec<u8, B>,
    pos: usize,
    path_len: usize,
}

/// Every file of `commit`'s tree, handed to `visit` in tree order: its path, mode and the
/// object to read ([`Index::read`](crate::Index::read)). Only the trees on the path being
/// walked are held. The walk's plant is the index's ([`PlantedIndex::plant`]): `NoPlant` for a
/// shipped [`Index`](crate::Index).
pub fn files<A: Allocator + Clone, B: Allocator + Clone, P: Planted<GitFetchPlant>>(
    index: &mut PlantedIndex<'_, A, P>,
    commit: Oid,
    limits: Limits,
    alloc: B,
    mut visit: impl FnMut(File<'_>),
) -> Result<(), TreeError> {
    let at_root = |why| TreeError::new(b"", why);
    let mut body = Vec::new_in(alloc.clone());
    object(index, commit, Kind::Commit, b"", &mut body)?;
    let hex = body.strip_prefix(b"tree ").and_then(|rest| rest.get(..40)).ok_or(at_root(TreeWhy::Commit))?;
    let root = Oid::parse_hex(hex).ok_or(at_root(TreeWhy::Commit))?;
    drop(body);
    let mut path: Vec<u8, B> = Vec::new_in(alloc.clone());
    let mut stack: Vec<Open<B>, B> = Vec::new_in(alloc.clone());
    let mut tree = Vec::new_in(alloc.clone());
    object(index, root, Kind::Tree, b"", &mut tree)?;
    stack.try_push(Open { tree, pos: 0, path_len: 0 }).map_err(|r| at_root(TreeWhy::Alloc(r.why)))?;
    let mut count = 0u32;
    while let Some(top) = stack.last_mut() {
        if top.pos == top.tree.len() {
            stack.pop();
            continue;
        }
        let base = top.path_len;
        path.truncate(base);
        let (mode, name, oid, next) = entry(&top.tree[top.pos..]).ok_or(TreeError::new(&path, TreeWhy::Entry))?;
        top.pos += next;
        if base > 0 {
            path.try_push(b'/').map_err(|r| TreeError::new(&[], TreeWhy::Alloc(r.why)))?;
        }
        path.try_extend_from_slice(name).map_err(|e| TreeError::new(&[], TreeWhy::Alloc(e)))?;
        let good = good_name(name, index.plant());
        let refuse = |why| TreeError::new(&path, why);
        if !good {
            return Err(refuse(TreeWhy::Name));
        }
        if path.len() > usize::from(limits.most_path) {
            return Err(refuse(TreeWhy::PathOver { most: limits.most_path }));
        }
        let mode = match mode {
            0o040000 => {
                if stack.len() > usize::from(limits.most_depth) {
                    return Err(refuse(TreeWhy::DepthOver { most: limits.most_depth }));
                }
                let mut tree = Vec::new_in(alloc.clone());
                object(index, oid, Kind::Tree, &path, &mut tree)?;
                let len = path.len();
                stack.try_push(Open { tree, pos: 0, path_len: len }).map_err(|r| refuse(TreeWhy::Alloc(r.why)))?;
                continue;
            }
            0o100644 | 0o100664 => Mode::File,
            0o100755 => Mode::Exec,
            0o120000 => Mode::Symlink,
            0o160000 => return Err(refuse(TreeWhy::Submodule)),
            mode => return Err(refuse(TreeWhy::Mode { mode })),
        };
        count += 1;
        if count > limits.most_files {
            return Err(refuse(TreeWhy::FilesOver { most: limits.most_files }));
        }
        let found = find(index, oid, Kind::Blob, &path)?;
        let text = core::str::from_utf8(&path).map_err(|_| refuse(TreeWhy::Name))?;
        visit(File { path: text, mode, oid, object: found });
    }
    Ok(())
}

fn find<A: Allocator + Clone, P: Planted<GitFetchPlant>>(
    index: &PlantedIndex<'_, A, P>,
    oid: Oid,
    want: Kind,
    path: &[u8],
) -> Result<Found, TreeError> {
    let found = index.find(&oid).ok_or(TreeError::new(path, TreeWhy::Missing { oid }))?;
    if found.kind != want {
        return Err(TreeError::new(path, TreeWhy::NotKind { oid, want }));
    }
    Ok(found)
}

/// The object `oid`, of kind `want`, read whole into `out`.
fn object<A: Allocator + Clone, B: Allocator, P: Planted<GitFetchPlant>>(
    index: &mut PlantedIndex<'_, A, P>,
    oid: Oid,
    want: Kind,
    path: &[u8],
    out: &mut Vec<u8, B>,
) -> Result<(), TreeError> {
    let found = find(index, oid, want, path)?;
    index.read_vec(found, out).map_err(|e| TreeError::new(path, TreeWhy::Read(e.why)))
}

/// One tree entry: its mode, name, id and length.
fn entry(bytes: &[u8]) -> Option<(u32, &[u8], Oid, usize)> {
    let space = bytes.iter().position(|&b| b == b' ')?;
    let digits = &bytes[..space];
    if digits.is_empty() || digits.len() > 6 || !digits.iter().all(|b| (b'0'..=b'7').contains(b)) {
        return None;
    }
    let mode = digits.iter().fold(0u32, |m, &b| m * 8 + u32::from(b - b'0'));
    let nul = space + 1 + bytes[space + 1..].iter().position(|&b| b == 0)?;
    let name = &bytes[space + 1..nul];
    let id = bytes.get(nul + 1..nul + 21)?;
    let mut oid = [0u8; 20];
    oid.copy_from_slice(id);
    Some((mode, name, Oid(oid), nul + 21))
}

fn good_name(name: &[u8], plant: impl Planted<GitFetchPlant>) -> bool {
    if name == b".." && plant.has(GitFetchPlant::ParentNameTaken) {
        return true;
    }
    !name.is_empty()
        && name != b"."
        && name != b".."
        && !name.eq_ignore_ascii_case(b".git")
        && !name.iter().any(|&b| b == b'/' || b == b'\\' || b == 0)
        && core::str::from_utf8(name).is_ok()
}
