//! Named heads and transactions against them.
//!
//! A transaction names the root it was made against. On the head it applies as it is. On an
//! older root it is rebased file by file onto the head: a path nobody changed since takes the
//! op; an edit of a range that does not meet what changed is moved to where that range is now;
//! anything else is refused with the path, so the agent decides
//! ([docs/roadmap.md](../../../docs/roadmap.md#writes-transactions-against-a-base)).

use std::{fmt, sync::Arc};

use rustc_hash::FxHashMap;

use crate::{Hash, NodeId, Op, OpError, Tree};

/// A tree and its heads.
#[derive(Default)]
pub struct Vfs {
    pub tree: Tree,
    heads: FxHashMap<Box<str>, NodeId>,
}

/// What a transaction made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Commit {
    pub root: NodeId,
    /// The head had moved past the transaction's base and the ops were moved onto it.
    pub rebased: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommitError {
    /// The base root is not one this tree has.
    UnknownBase(Hash),
    /// The head moved and changed `path` in a way the ops can't be moved past.
    Conflict {
        path: String,
    },
    Op(OpError),
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitError::UnknownBase(hash) => write!(f, "no root {hash} is known"),
            CommitError::Conflict { path } => {
                write!(f, "{path} changed since the transaction's base")
            }
            CommitError::Op(error) => error.fmt(f),
        }
    }
}

impl From<OpError> for CommitError {
    fn from(error: OpError) -> CommitError {
        CommitError::Op(error)
    }
}

impl Vfs {
    pub fn new() -> Vfs {
        Vfs::default()
    }

    /// The root `workspace` is at; the empty root for one never written.
    pub fn head(&self, workspace: &str) -> NodeId {
        self.heads.get(workspace).copied().unwrap_or(self.tree.empty())
    }

    pub fn heads(&self) -> impl Iterator<Item = (&str, NodeId)> {
        self.heads.iter().map(|(name, root)| (&**name, *root))
    }

    /// Moves `workspace` to `root`, as a checkout or a fork does.
    pub fn set_head(&mut self, workspace: &str, root: NodeId) {
        self.heads.insert(workspace.into(), root);
    }

    /// Applies `ops`, made against `base`, to `workspace`'s head, and moves the head.
    pub fn commit(
        &mut self,
        workspace: &str,
        base: Hash,
        ops: &[Op],
    ) -> Result<Commit, CommitError> {
        let base = self.tree.node(&base).ok_or(CommitError::UnknownBase(base))?;
        let head = self.head(workspace);
        let (ops, rebased) = if base == head {
            (ops.to_vec(), false)
        } else {
            (rebase(&self.tree, base, head, ops)?, true)
        };
        let root = self.tree.apply(head, &ops)?;
        self.heads.insert(workspace.into(), root);
        Ok(Commit { root, rebased })
    }

    /// Drops every node no head and no root in `keep` reaches; `keep` is renumbered in place.
    /// Returns each old id's new one, `None` for a node dropped.
    pub fn collect(&mut self, keep: &mut Vec<NodeId>) -> Vec<Option<NodeId>> {
        let names: Vec<Box<str>> = self.heads.keys().cloned().collect();
        let held = keep.len();
        keep.extend(names.iter().map(|name| self.heads[name]));
        let renumbered = self.tree.collect(keep);
        for (name, root) in names.into_iter().zip(keep.drain(held..)) {
            self.heads.insert(name, root);
        }
        renumbered
    }
}

/// `ops`, made against `base`, moved onto `head`.
pub fn rebase(tree: &Tree, base: NodeId, head: NodeId, ops: &[Op]) -> Result<Vec<Op>, CommitError> {
    let mut touched: FxHashMap<&str, usize> = FxHashMap::default();
    for op in ops {
        for path in op.paths() {
            *touched.entry(path).or_default() += 1;
        }
    }
    // A path that resolves alike in both has the same directories above it in both; one a head
    // made unreachable fails as the op would on the head.
    let changed = |path: &str| tree.lookup(base, path) != tree.lookup(head, path);
    let mut moved = Vec::with_capacity(ops.len());
    for op in ops {
        let Some(path) = op.paths().find(|it| changed(it)) else {
            moved.push(op.clone());
            continue;
        };
        let conflict = || CommitError::Conflict { path: path.to_owned() };
        // Only a lone edit of a file both sides still have can be moved past a change.
        let Op::Edit { path, at, len, bytes } = op else { return Err(conflict()) };
        if touched[path.as_str()] > 1 {
            return Err(conflict());
        }
        let (Some(was), Some(now)) = (tree.read(base, path), tree.read(head, path)) else {
            return Err(conflict());
        };
        let (from, to) = changed_range(was, now);
        let end = at.checked_add(*len).ok_or_else(conflict)?;
        if clash(*at, end, from as u64, to as u64) {
            return Err(conflict());
        }
        let at = if *at >= to as u64 { *at + now.len() as u64 - was.len() as u64 } else { *at };
        moved.push(Op::Edit { path: path.clone(), at, len: *len, bytes: bytes.clone() });
    }
    Ok(moved)
}

/// The range of `was` that differs from `now`: everything between their common prefix and
/// their common suffix.
fn changed_range(was: &Arc<[u8]>, now: &Arc<[u8]>) -> (usize, usize) {
    let prefix = was.iter().zip(now.iter()).take_while(|(a, b)| a == b).count();
    let longest = was.len().min(now.len()) - prefix;
    let suffix =
        was.iter().rev().zip(now.iter().rev()).take(longest).take_while(|(a, b)| a == b).count();
    (prefix, was.len() - suffix)
}

/// Whether two ranges overlap, or meet where either is empty (two inserts at one place).
fn clash(a0: u64, a1: u64, b0: u64, b1: u64) -> bool {
    if a0 == a1 || b0 == b1 { a0 <= b1 && b0 <= a1 } else { a0 < b1 && b0 < a1 }
}
