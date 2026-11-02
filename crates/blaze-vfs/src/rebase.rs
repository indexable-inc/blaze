//! Moving a stale transaction onto its head, file by file: a path nobody changed since the base
//! takes the op; an edit of a range that does not meet what changed is moved to where that
//! range is now; anything else is refused with the path, so the agent decides.

use blaze_hash::HashMap;

use crate::{
    CommitError, Op,
    tree::{NodeId, Tree, op_paths},
};

/// `ops`, made against `base`, moved onto `head`: for each, `None` where it applies as it is
/// (so its bytes are never copied) or the op moved to where its range is now.
pub(crate) fn rebase(
    tree: &Tree,
    base: NodeId,
    head: NodeId,
    ops: &[Op],
) -> Result<Vec<Option<Op>>, CommitError> {
    let mut touched: HashMap<&str, usize> = HashMap::default();
    for op in ops {
        for path in op_paths(op) {
            *touched.entry(path).or_default() += 1;
        }
    }
    // A path that resolves alike in both has the same directories above it in both; one a head
    // made unreachable fails as the op would on the head. A change of mode is a change.
    let changed = |path: &str| tree.entry(base, path) != tree.entry(head, path);
    // alloc: only a transaction its head moved under is rebased, a slot per op.
    let mut moved = Vec::with_capacity(ops.len());
    for op in ops {
        let Some(path) = op_paths(op).find(|it| changed(it)) else {
            moved.push(None);
            continue;
        };
        // alloc: the error's path
        let conflict = || CommitError::Conflict { path: path.to_owned() };
        // Only a lone edit of a file both sides still have can be moved past a change.
        let Op::Edit { path, at, len, bytes } = op else { return Err(conflict()) };
        if touched[path.as_str()] > 1 {
            return Err(conflict());
        }
        let (Some(was), Some(now)) = (tree.read(base, path)?, tree.read(head, path)?) else {
            return Err(conflict());
        };
        let (from, to) = changed_range(was, now);
        let end = at.checked_add(*len).ok_or_else(conflict)?;
        if clash(*at, end, from as u64, to as u64) {
            return Err(conflict());
        }
        let at = if *at >= to as u64 { *at + now.len() as u64 - was.len() as u64 } else { *at };
        // clone: only an edit moved past a change, into an op of its own.
        moved.push(Some(Op::Edit { path: path.clone(), at, len: *len, bytes: bytes.clone() }));
    }
    Ok(moved)
}

/// The range of `was` that differs from `now`: everything between their common prefix and
/// their common suffix.
fn changed_range(was: &[u8], now: &[u8]) -> (usize, usize) {
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
