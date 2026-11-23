//! Structured semantic diff & RFC 6902 JSON Patch over [`blaze_data::Value`].
//!
//! - Uses RFC 6901 JSON Pointer paths (`/a/b/0`, escaping `~` as `~0` and `/` as `~1`).
//! - 100% iterative (explicit worklist stack — zero recursion, zero stack overflow risk on deeply
//!   nested JSON/TOML/YAML/CBOR documents).
//! - Deterministic order: map keys are visited in sorted byte order (`Map::sorted`).

use core::alloc::Allocator;
use core::fmt::{self, Write as _};

use blaze_data::Value;
use blaze_own::{AllocFailure, String, Vec};

/// One RFC 6902 JSON Patch operation over [`blaze_data::Value`].
#[derive(Debug, PartialEq, Eq)]
pub enum ValuePatchOp<A: Allocator> {
    Add { path: String<A>, value: Value<A> },
    Remove { path: String<A> },
    Replace { path: String<A>, value: Value<A> },
}

/// Why applying a [`ValuePatchOp`] failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValuePatchError {
    /// A JSON Pointer did not start with `/` (and was not empty `""` for root).
    InvalidPointer { op_index: usize },
    /// A parent container along the pointer did not exist or was not indexable.
    MissingParent { op_index: usize },
    /// A list index segment was not a valid index or `-`.
    InvalidIndex { op_index: usize },
    /// Target key or index did not exist for `Remove` or `Replace`.
    TargetNotFound { op_index: usize },
    /// Allocator refused an allocation.
    Alloc(AllocFailure),
}

impl fmt::Display for ValuePatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ValuePatchError::InvalidPointer { op_index } => {
                write!(f, "bstd::diff: patch op #{op_index} has an invalid RFC 6901 JSON pointer")
            }
            ValuePatchError::MissingParent { op_index } => {
                write!(f, "bstd::diff: patch op #{op_index} references a missing parent container")
            }
            ValuePatchError::InvalidIndex { op_index } => {
                write!(f, "bstd::diff: patch op #{op_index} has an invalid list index")
            }
            ValuePatchError::TargetNotFound { op_index } => {
                write!(f, "bstd::diff: patch op #{op_index} target does not exist")
            }
            ValuePatchError::Alloc(err) => write!(f, "bstd::diff: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for ValuePatchError {}

/// Escape a single RFC 6901 JSON Pointer segment (`~` -> `~0`, `/` -> `~1`) and append `/segment`.
fn append_pointer_segment<A: Allocator>(
    base: &str,
    segment: &str,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(base.len() + 1 + segment.len(), alloc)?;
    out.try_push_str(base)?;
    out.try_push('/')?;
    for ch in segment.chars() {
        match ch {
            '~' => out.try_push_str("~0")?,
            '/' => out.try_push_str("~1")?,
            other => out.try_push(other)?,
        }
    }
    Ok(out)
}

fn append_pointer_index<A: Allocator>(
    base: &str,
    index: usize,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut out = String::try_with_capacity_in(base.len() + 21, alloc)?;
    out.try_push_str(base)?;
    let _ = write!(&mut out, "/{index}");
    Ok(out)
}

fn unescape_pointer_segment<A: Allocator>(
    seg: &str,
    alloc: A,
) -> Result<Option<String<A>>, AllocFailure> {
    let mut out = String::try_with_capacity_in(seg.len(), alloc)?;
    let mut chars = seg.chars();
    while let Some(ch) = chars.next() {
        if ch == '~' {
            match chars.next() {
                Some('0') => out.try_push('~')?,
                Some('1') => out.try_push('/')?,
                _ => return Ok(None),
            }
        } else {
            out.try_push(ch)?;
        }
    }
    Ok(Some(out))
}

struct DiffTask<'a, A: Allocator> {
    path: String<A>,
    old: &'a Value<A>,
    new: &'a Value<A>,
}

/// Compute a deterministic RFC 6902 patch transforming `old` into `new` (iterative, zero recursion).
pub fn diff_values<A: Allocator + Copy>(
    old: &Value<A>,
    new: &Value<A>,
    alloc: A,
) -> Result<Vec<ValuePatchOp<A>, A>, AllocFailure> {
    let mut ops = Vec::new_in(alloc);
    let mut stack: Vec<DiffTask<'_, A>, A> = Vec::new_in(alloc);
    stack
        .try_push(DiffTask { path: String::new_in(alloc), old, new })
        .map_err(|r| r.why)?;

    while let Some(task) = stack.pop() {
        if task.old == task.new {
            continue;
        }
        match (task.old, task.new) {
            (Value::Map(om), Value::Map(nm)) => {
                // First emit removals in sorted key order.
                for (k, _) in om.sorted() {
                    if !nm.contains_key(k) {
                        let child_path = append_pointer_segment(task.path.as_str(), k, alloc)?;
                        ops.try_push(ValuePatchOp::Remove { path: child_path })
                            .map_err(|r| r.why)?;
                    }
                }
                // Collect shared & added keys in sorted order; push shared children in reverse onto
                // the LIFO stack so they are emitted in ascending key order.
                let mut child_tasks: Vec<DiffTask<'_, A>, A> = Vec::new_in(alloc);
                for (k, nv) in nm.sorted() {
                    let child_path = append_pointer_segment(task.path.as_str(), k, alloc)?;
                    match om.get(k) {
                        None => {
                            ops.try_push(ValuePatchOp::Add {
                                path: child_path,
                                value: nv.copy_in(alloc),
                            })
                            .map_err(|r| r.why)?;
                        }
                        Some(ov) if ov != nv => {
                            child_tasks
                                .try_push(DiffTask { path: child_path, old: ov, new: nv })
                                .map_err(|r| r.why)?;
                        }
                        Some(_) => {}
                    }
                }
                while let Some(ct) = child_tasks.pop() {
                    stack.try_push(ct).map_err(|r| r.why)?;
                }
            }
            (Value::List(ol), Value::List(nl)) => {
                let common = ol.len().min(nl.len());
                // Remove excess trailing elements from the end backwards so indices stay valid.
                if ol.len() > nl.len() {
                    let mut idx = ol.len();
                    while idx > nl.len() {
                        idx -= 1;
                        let child_path = append_pointer_index(task.path.as_str(), idx, alloc)?;
                        ops.try_push(ValuePatchOp::Remove { path: child_path })
                            .map_err(|r| r.why)?;
                    }
                }
                // Append new elements beyond `ol.len()`.
                if nl.len() > ol.len() {
                    for (offset, nv) in nl.as_slice().get(ol.len()..).unwrap_or(&[]).iter().enumerate()
                    {
                        let child_path =
                            append_pointer_index(task.path.as_str(), ol.len() + offset, alloc)?;
                        ops.try_push(ValuePatchOp::Add {
                            path: child_path,
                            value: nv.copy_in(alloc),
                        })
                        .map_err(|r| r.why)?;
                    }
                }
                // Diff shared index positions (push in reverse onto LIFO stack for ascending order).
                let mut i = common;
                while i > 0 {
                    i -= 1;
                    if let (Some(ov), Some(nv)) = (ol.get(i), nl.get(i))
                        && ov != nv
                    {
                        let child_path = append_pointer_index(task.path.as_str(), i, alloc)?;
                        stack
                            .try_push(DiffTask { path: child_path, old: ov, new: nv })
                            .map_err(|r| r.why)?;
                    }
                }
            }
            _ => {
                ops.try_push(ValuePatchOp::Replace {
                    path: task.path,
                    value: task.new.copy_in(alloc),
                })
                .map_err(|r| r.why)?;
            }
        }
    }

    Ok(ops)
}

/// Apply a sequence of RFC 6902 [`ValuePatchOp`]s to a copy of `old`, returning the new [`Value`].
pub fn apply_value_patch<A: Allocator + Copy>(
    old: &Value<A>,
    ops: &[ValuePatchOp<A>],
    alloc: A,
) -> Result<Value<A>, ValuePatchError> {
    let mut root = old.copy_in(alloc);

    for (op_index, op) in ops.iter().enumerate() {
        let path = match op {
            ValuePatchOp::Add { path, .. }
            | ValuePatchOp::Remove { path }
            | ValuePatchOp::Replace { path, .. } => path.as_str(),
        };

        if path.is_empty() {
            match op {
                ValuePatchOp::Add { value, .. } | ValuePatchOp::Replace { value, .. } => {
                    root = value.copy_in(alloc);
                }
                ValuePatchOp::Remove { .. } => {
                    return Err(ValuePatchError::TargetNotFound { op_index });
                }
            }
            continue;
        }

        let Some(rest) = path.strip_prefix('/') else {
            return Err(ValuePatchError::InvalidPointer { op_index });
        };

        // Split pointer into parent segments and final target segment.
        let mut segments: Vec<String<A>, A> = Vec::new_in(alloc);
        for raw_seg in rest.split('/') {
            let Some(unesc) =
                unescape_pointer_segment(raw_seg, alloc).map_err(ValuePatchError::Alloc)?
            else {
                return Err(ValuePatchError::InvalidPointer { op_index });
            };
            segments.try_push(unesc).map_err(|r| ValuePatchError::Alloc(r.why))?;
        }
        let Some(leaf) = segments.pop() else {
            return Err(ValuePatchError::InvalidPointer { op_index });
        };

        // Navigate down to parent container.
        let mut cursor = &mut root;
        for seg in segments.as_slice() {
            cursor = match cursor {
                Value::Map(map) => map
                    .get_mut(seg.as_str())
                    .ok_or(ValuePatchError::MissingParent { op_index })?,
                Value::List(list) => {
                    let idx = seg
                        .as_str()
                        .parse::<usize>()
                        .map_err(|_| ValuePatchError::InvalidIndex { op_index })?;
                    list.get_mut(idx).ok_or(ValuePatchError::MissingParent { op_index })?
                }
                _ => return Err(ValuePatchError::MissingParent { op_index }),
            };
        }

        // Apply leaf operation.
        match cursor {
            Value::Map(map) => match op {
                ValuePatchOp::Add { value, .. } => {
                    let key = String::try_from_str_in(leaf.as_str(), alloc)
                        .map_err(ValuePatchError::Alloc)?;
                    map.insert(key, value.copy_in(alloc));
                }
                ValuePatchOp::Replace { value, .. } => {
                    let slot = map
                        .get_mut(leaf.as_str())
                        .ok_or(ValuePatchError::TargetNotFound { op_index })?;
                    *slot = value.copy_in(alloc);
                }
                ValuePatchOp::Remove { .. } => {
                    map.remove(leaf.as_str())
                        .ok_or(ValuePatchError::TargetNotFound { op_index })?;
                }
            },
            Value::List(list) => match op {
                ValuePatchOp::Add { value, .. } => {
                    if leaf.as_str() == "-" {
                        list.try_push(value.copy_in(alloc))
                            .map_err(|r| ValuePatchError::Alloc(r.why))?;
                    } else {
                        let idx = leaf
                            .as_str()
                            .parse::<usize>()
                            .map_err(|_| ValuePatchError::InvalidIndex { op_index })?;
                        if idx > list.len() {
                            return Err(ValuePatchError::InvalidIndex { op_index });
                        }
                        list.insert_shift(idx, value.copy_in(alloc));
                    }
                }
                ValuePatchOp::Replace { value, .. } => {
                    let idx = leaf
                        .as_str()
                        .parse::<usize>()
                        .map_err(|_| ValuePatchError::InvalidIndex { op_index })?;
                    let slot =
                        list.get_mut(idx).ok_or(ValuePatchError::TargetNotFound { op_index })?;
                    *slot = value.copy_in(alloc);
                }
                ValuePatchOp::Remove { .. } => {
                    let idx = leaf
                        .as_str()
                        .parse::<usize>()
                        .map_err(|_| ValuePatchError::InvalidIndex { op_index })?;
                    if idx >= list.len() {
                        return Err(ValuePatchError::TargetNotFound { op_index });
                    }
                    let _ = list.remove_shift(idx);
                }
            },
            _ => return Err(ValuePatchError::MissingParent { op_index }),
        }
    }

    Ok(root)
}
