//! Instances (crates/blaze-cli/tests/ladder/README.md, rung 5): a generic fn is built once per
//! list of generic arguments it is called with. The arguments live in the program's one
//! *ground table*, a [`PortableTys`] that holds only types with no generic parameter, no hole
//! and no error; two instances are the same exactly when their items and their interned
//! argument lists are equal, so an [`Instance`] is its own identity within a run.
//!
//! [`ground`] carries a type of another table (a body's [`blaze_typeck::Checked`] types, a
//! signature's, a struct's fields) into the ground table, each `Param(i)` replaced by the
//! instance's `args[i]`. It walks the reachable nodes in index order: a portable table's
//! children have smaller indices than their parents, so no recursion and no stack.

use blaze_own::{Alloc, Vec};
use blaze_typeck::{ItemKey, List, PNode, PTy, PTys, PortableTys};

use crate::BuildError;

/// A fn item and its generic arguments, interned in the ground table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Instance {
    pub item: ItemKey,
    pub args: List,
}

/// `ty` of table `from`, in `ground` with `Param(i)` replaced by `args[i]` (`args` are types of
/// `ground`). Refused, naming the node: a parameter past `args`, a hole, an error, an alias,
/// an opaque type, a closure, a fn pointer: none has a ground form the ladder builds yet.
/// `scratch` is the allocator of the walk's two columns.
pub fn ground(
    from: PTys<'_>,
    ty: PTy,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
) -> Result<PTy, BuildError> {
    let malformed = |_| BuildError::Malformed { what: "a type index past its table" };
    // Which nodes of `from` `ty` reaches: marked, then mapped in index order.
    let top = ty.0 as usize;
    let mut reached: Vec<bool, Alloc<'_>> = Vec::with_capacity_in(top + 1, scratch);
    reached.resize(top + 1, false);
    reached[top] = true;
    let mut mapped: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(top + 1, scratch);
    mapped.resize(top + 1, PTy(u32::MAX));
    let mut children: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    // Marking runs from the top down: a node's children have smaller indices.
    for at in (0..=top).rev() {
        if !reached[at] {
            continue;
        }
        children.clear();
        each_child(from, from.node(PTy(at as u32)).map_err(malformed)?, &mut children)?;
        for child in children.iter() {
            *reached.get_mut(child.0 as usize).ok_or(BuildError::Malformed { what: "a child after its parent" })? = true;
        }
    }
    let mut list: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    for at in 0..=top {
        if !reached[at] {
            continue;
        }
        let mut map_list = |l: List, list: &mut Vec<PTy, Alloc<'_>>| -> Result<List, BuildError> {
            list.clear();
            for t in from.list(l).map_err(malformed)? {
                list.push(mapped[t.0 as usize]);
            }
            ground.intern_list(list).map_err(|_| BuildError::Malformed { what: "the ground table is full" })
        };
        let node = match from.node(PTy(at as u32)).map_err(malformed)? {
            PNode::Param(i) => {
                mapped[at] = *args.get(i as usize).ok_or(BuildError::UnsupportedSig { what: "a generic parameter with no argument" })?;
                continue;
            }
            n @ (PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str | PNode::Never) => n,
            PNode::Tuple(l) => PNode::Tuple(map_list(l, &mut list)?),
            PNode::Ref(m, t) => PNode::Ref(m, mapped[t.0 as usize]),
            PNode::Ptr(m, t) => PNode::Ptr(m, mapped[t.0 as usize]),
            PNode::Array(t, n) => PNode::Array(mapped[t.0 as usize], n),
            PNode::Slice(t) => PNode::Slice(mapped[t.0 as usize]),
            PNode::Adt(key, l) => PNode::Adt(key, map_list(l, &mut list)?),
            PNode::FnDef(key, l) => PNode::FnDef(key, map_list(l, &mut list)?),
            PNode::FnPtr(_) => return Err(BuildError::UnsupportedSig { what: "a fn pointer type in an instance" }),
            PNode::Closure(..) => return Err(BuildError::UnsupportedSig { what: "a closure type in an instance" }),
            PNode::Alias { .. } => return Err(BuildError::UnsupportedSig { what: "an associated type in an instance" }),
            PNode::Opaque(..) => return Err(BuildError::UnsupportedSig { what: "an `impl Trait` type in an instance" }),
            PNode::Infer | PNode::Error => return Err(BuildError::Malformed { what: "a hole or error type after a clean check" }),
        };
        mapped[at] = ground.intern(node).map_err(|_| BuildError::Malformed { what: "the ground table is full" })?;
    }
    Ok(mapped[top])
}

/// The children of `node` (of table `from`), pushed to `out`.
fn each_child(from: PTys<'_>, node: PNode, out: &mut Vec<PTy, Alloc<'_>>) -> Result<(), BuildError> {
    let malformed = |_| BuildError::Malformed { what: "a list past its table" };
    match node {
        PNode::Tuple(l) | PNode::Adt(_, l) | PNode::FnDef(_, l) | PNode::FnPtr(l) | PNode::Opaque(_, l) => {
            out.extend_from_slice(from.list(l).map_err(malformed)?);
        }
        PNode::Alias { args, .. } => out.extend_from_slice(from.list(args).map_err(malformed)?),
        PNode::Ref(_, t) | PNode::Ptr(_, t) | PNode::Array(t, _) | PNode::Slice(t) | PNode::Closure(_, t) => out.push(t),
        PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str | PNode::Never | PNode::Param(_) | PNode::Infer | PNode::Error => {}
    }
    Ok(())
}
