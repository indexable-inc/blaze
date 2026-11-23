//! Answers of the seam written as text two programs can compare.
//!
//! The whole program and the lazy one lay their type tables out in different orders: one
//! `PTy` names two different types in them. So a type is written as its tree (each node's
//! kind, its items by key, its children in turn), never as an index. Rows differ the same way,
//! so a row is written as its fn's key. Everything else an answer holds (item keys, content
//! ids, ABIs, ranges, bytes) means the same in both programs and is written as it is.
//!
//! Every writer writes into an `AString` in the test's frame; the frame is the allocator.

use core::alloc::Allocator;
use core::fmt::{Result, Write};

use blaze_build::data::DataRecord;
use blaze_ladder::seam::FnFacts;
use blaze_own::AString;
use blaze_typeck::{Adt, FnSig, List, PNode, PTy, PTys};

/// The most nodes one answer writes (rule 18: every walk has a cap): a type past it ends in
/// `..`, which a second program writes the same way at the same place.
const MOST_NODES: u32 = 4096;

/// The deepest level one answer writes: a subtree below it is `..` (each level is three small
/// frames, `node`, `list`, `each`).
const MOST_DEPTH: u32 = 64;

/// Type `t` of `tys`, as its tree.
pub fn ty(o: &mut AString<'_>, tys: PTys<'_>, t: PTy) -> Result {
    node(o, tys, t, &mut Budget::new())
}

/// Types `ts` of `tys`, comma separated.
pub fn types(o: &mut AString<'_>, tys: PTys<'_>, ts: &[PTy]) -> Result {
    each(o, tys, ts, &mut Budget::new())
}

/// The list `l` of `tys` (a ground argument list), comma separated.
pub fn args(o: &mut AString<'_>, tys: PTys<'_>, l: List) -> Result {
    list(o, tys, l, &mut Budget::new())
}

fn each(o: &mut AString<'_>, tys: PTys<'_>, ts: &[PTy], left: &mut Budget) -> Result {
    for (n, &t) in ts.iter().enumerate() {
        if n > 0 {
            o.write_str(", ")?;
        }
        node(o, tys, t, left)?;
    }
    Ok(())
}

fn list(o: &mut AString<'_>, tys: PTys<'_>, l: List, left: &mut Budget) -> Result {
    match tys.list(l) {
        Ok(ts) => each(o, tys, ts, left),
        Err(e) => write!(o, "unread list ({e:?})"),
    }
}

/// What a type's writing may still spend: nodes in all, and levels below this one.
struct Budget {
    nodes: u32,
    depth: u32,
}

impl Budget {
    fn new() -> Budget {
        Budget { nodes: MOST_NODES, depth: 0 }
    }
}

/// A node and its children, one level deeper. The depth is capped as well as the count: a
/// node's children have smaller indices (portable.rs), so a tree is never cyclic, but with only
/// the count its depth could reach MOST_NODES frames of `node`, `list` and `each`.
fn node(o: &mut AString<'_>, tys: PTys<'_>, t: PTy, left: &mut Budget) -> Result {
    if left.nodes == 0 || left.depth == MOST_DEPTH {
        return o.write_str("..");
    }
    left.nodes -= 1;
    left.depth += 1;
    let written = node_at(o, tys, t, left);
    left.depth -= 1;
    written
}

fn node_at(o: &mut AString<'_>, tys: PTys<'_>, t: PTy, left: &mut Budget) -> Result {
    let n = match tys.node(t) {
        Ok(n) => n,
        Err(e) => return write!(o, "unread ({e:?})"),
    };
    match n {
        PNode::Tuple(l) => {
            o.write_str("(")?;
            list(o, tys, l, left)?;
            o.write_str(")")
        }
        PNode::Record(_, l) => {
            o.write_str("record(")?;
            list(o, tys, l, left)?;
            o.write_str(")")
        }
        PNode::Union(l) => {
            o.write_str("union(")?;
            list(o, tys, l, left)?;
            o.write_str(")")
        }
        PNode::Ref(m, inner) => {
            write!(o, "&{m:?} ")?;
            node(o, tys, inner, left)
        }
        PNode::Ptr(m, inner) => {
            write!(o, "*{m:?} ")?;
            node(o, tys, inner, left)
        }
        PNode::Array(elem, len) => {
            o.write_str("[")?;
            node(o, tys, elem, left)?;
            o.write_str("; ")?;
            node(o, tys, len, left)?;
            o.write_str("]")
        }
        PNode::Slice(elem) => {
            o.write_str("[")?;
            node(o, tys, elem, left)?;
            o.write_str("]")
        }
        PNode::Pat(base, ranges) => {
            o.write_str("pat(")?;
            node(o, tys, base, left)?;
            o.write_str(" is ")?;
            list(o, tys, ranges, left)?;
            o.write_str(")")
        }
        PNode::Adt(k, l) => keyed(o, tys, "Adt", k, l, left),
        PNode::FnDef(k, l) => keyed(o, tys, "FnDef", k, l, left),
        PNode::Opaque(k, l) => keyed(o, tys, "Opaque", k, l, left),
        PNode::ExTrait(k, l) => keyed(o, tys, "ExTrait", k, l, left),
        PNode::FnPtr(l, header) => {
            write!(o, "fn {header:?} (")?;
            list(o, tys, l, left)?;
            o.write_str(")")
        }
        PNode::Closure { owner, ordinal, args } => {
            write!(o, "Closure({owner:?} #{ordinal})<")?;
            list(o, tys, args, left)?;
            o.write_str(">")
        }
        PNode::Coroutine { owner, ordinal, args } => {
            write!(o, "Coroutine({owner:?} #{ordinal})<")?;
            list(o, tys, args, left)?;
            o.write_str(">")
        }
        PNode::Future(e, output, interior) => {
            write!(o, "Future({e:?}) -> ")?;
            node(o, tys, output, left)?;
            o.write_str(" in ")?;
            node(o, tys, interior, left)
        }
        PNode::Alias { trait_, name, args } => {
            write!(o, "<{trait_:?}>::{name:?}<")?;
            list(o, tys, args, left)?;
            o.write_str(">")
        }
        PNode::ExProj { trait_, name, args, term } => {
            write!(o, "<{trait_:?}>::{name:?}<")?;
            list(o, tys, args, left)?;
            o.write_str("> = ")?;
            node(o, tys, term, left)
        }
        PNode::Dynamic(l) => {
            o.write_str("dyn(")?;
            list(o, tys, l, left)?;
            o.write_str(")")
        }
        PNode::ConstValue { ty, bits } => {
            node(o, tys, ty, left)?;
            write!(o, " = {bits}")
        }
        // Every other node holds no type of the table: its own `Debug` is its tree.
        leaf => write!(o, "{leaf:?}"),
    }
}

fn keyed(o: &mut AString<'_>, tys: PTys<'_>, what: &str, k: blaze_typeck::ItemKey, l: List, left: &mut Budget) -> Result {
    write!(o, "{what}({k:?})<")?;
    list(o, tys, l, left)?;
    o.write_str(">")
}

/// A fn's facts as a row lends them, less its index and its body (a lazy row's body comes and
/// goes with `check_one` and `done`; the body's answers are asked by their own questions).
pub fn facts(o: &mut AString<'_>, f: &FnFacts<'_>) -> Result {
    write!(
        o,
        "path {} at {:?} kind {:?} abi {:?} symbol {:?} export {:?} const {} condition {} text {} bytes",
        f.path,
        f.at,
        f.kind,
        f.abi,
        f.symbol,
        f.export,
        f.const_fn,
        f.condition,
        f.text.len()
    )
}

/// A signature: its counts and header, then its types as trees.
pub fn sig(o: &mut AString<'_>, s: &FnSig<'_>) -> Result {
    write!(
        o,
        "parents {} own {} declared {} self {} header {:?} c_variadic {} legacy {:?} (",
        s.parent_count, s.own_count, s.declared_count, s.self_param, s.header, s.c_variadic, s.legacy_const_generics
    )?;
    types(o, s.types, s.inputs)?;
    o.write_str(") -> ")?;
    ty(o, s.types, s.output)
}

/// An ADT's row: its kind and generics, each variant, each field's type as a tree.
pub fn adt(o: &mut AString<'_>, a: Option<Adt<'_>>) -> Result {
    let Some(a) = a else {
        return o.write_str("none");
    };
    write!(o, "{:?} generics {}", a.kind, a.generics)?;
    for v in a.variants {
        write!(o, "; variant {:?} {:?} {:?} fields {:?}", v.key, v.name, v.ctor, v.fields)?;
    }
    for f in a.fields {
        write!(o, "; field {:?}: ", f.name)?;
        ty(o, a.types, f.ty)?;
    }
    Ok(())
}

/// A constant's or a static's value: each allocation's bytes and what its words point to.
pub fn data<A: Allocator>(o: &mut AString<'_>, r: &DataRecord<A>) -> Result {
    for (n, alloc) in r.allocs.iter().enumerate() {
        write!(o, "alloc {n} align 2^{} mutable {} bytes {:?} relocs {:?}; ", alloc.align, alloc.mutable, alloc.bytes.as_slice(), alloc.relocs.as_slice())?;
    }
    Ok(())
}
