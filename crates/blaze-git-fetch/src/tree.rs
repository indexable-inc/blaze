//! A commit's files: its tree walked from a [`Pack`], each file handed out with its path, mode
//! and bytes. A name that could leave the checkout or mean something to git (`..`, `.`, `.git`
//! in any case, a `/` or `\`, not UTF-8) is refused naming its path, as is a submodule (a
//! gitlink), which needs a second fetch this walk does not make.

#![blaze::sans_io]

use core::{alloc::Allocator, fmt};

use blaze_own::{AllocFailure, Vec};

use crate::{
    oid::{Kind, Oid},
    pack::Pack,
    plant::{self, Plant},
};

/// How much a tree may hold: every field is the caller's choice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    pub most_files: u32,
    /// The most bytes of one path.
    pub most_path: u16,
    /// The most directories deep.
    pub most_depth: u16,
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
    pub bytes: &'a [u8],
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
#[derive(Clone, Copy)]
struct Open<'p> {
    tree: &'p [u8],
    pos: usize,
    path_len: usize,
}

/// Every file of `commit`'s tree, handed to `visit` in tree order.
pub fn files<'p, A: Allocator, B: Allocator + Clone>(
    pack: &'p Pack<A>,
    commit: Oid,
    limits: Limits,
    alloc: B,
    mut visit: impl FnMut(File<'_>),
) -> Result<(), TreeError> {
    let at_root = |why| TreeError::new(b"", why);
    let body = object(pack, commit, Kind::Commit, b"")?;
    let hex = body.strip_prefix(b"tree ").and_then(|rest| rest.get(..40)).ok_or(at_root(TreeWhy::Commit))?;
    let root = Oid::parse_hex(hex).ok_or(at_root(TreeWhy::Commit))?;
    let mut path: Vec<u8, B> = Vec::new_in(alloc.clone());
    let mut stack: Vec<Open<'p>, B> = Vec::new_in(alloc);
    stack.push(Open { tree: object(pack, root, Kind::Tree, b"")?, pos: 0, path_len: 0 });
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
        let refuse = |why| TreeError::new(&path, why);
        if !good_name(name) {
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
                let tree = object(pack, oid, Kind::Tree, &path)?;
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
        let bytes = object(pack, oid, Kind::Blob, &path)?;
        let text = core::str::from_utf8(&path).map_err(|_| refuse(TreeWhy::Name))?;
        visit(File { path: text, mode, oid, bytes });
    }
    Ok(())
}

fn object<'p, A: Allocator>(pack: &'p Pack<A>, oid: Oid, want: Kind, path: &[u8]) -> Result<&'p [u8], TreeError> {
    let found = pack.get(&oid).ok_or(TreeError::new(path, TreeWhy::Missing { oid }))?;
    if found.kind != want {
        return Err(TreeError::new(path, TreeWhy::NotKind { oid, want }));
    }
    Ok(found.bytes)
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

fn good_name(name: &[u8]) -> bool {
    if name == b".." && plant::on(Plant::ParentNameTaken) {
        return true;
    }
    !name.is_empty()
        && name != b"."
        && name != b".."
        && !name.eq_ignore_ascii_case(b".git")
        && !name.iter().any(|&b| b == b'/' || b == b'\\' || b == 0)
        && core::str::from_utf8(name).is_ok()
}
